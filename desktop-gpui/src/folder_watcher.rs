//! Watches the configured "autoload torrents" folder and feeds new
//! `.torrent` / `.magnet` files through the same add path as the add-torrent
//! dialog ([`HookDispatcher::add_and_dispatch_added`]). The shared callback
//! also moves the source file to the configured "move added torrents to"
//! folder after the add, for both watcher- and dialog-originated files.
//! Watched torrents are added paused when `auto_add_paused` is set in the
//! config (the default), so they can be reviewed before downloading.
//! When the move folder is configured inside the watch folder, it is
//! excluded from watching so moved files are not picked up again.
//!
//! librqbit ships its own folder watcher (`Session::watch_folder`), but it
//! gives no way to observe the adds it performs — which is exactly what
//! move-after-add and the `torrent_added` hook need. So the desktop runs its
//! own watcher on the same primitives the lib's watcher uses (notify,
//! walkdir, librqbit-core validation), driving the shared add callback
//! directly. Because the callback returns when the torrent has been added
//! (and whether it was fresh), no polling, infohash correlation, or baseline
//! bookkeeping is needed: the file being processed is known, and it is moved
//! as soon as its add resolves.
//!
//! The background task is tied to the session it was started with: it exits
//! when that session's cancellation token fires (see `State::configure`),
//! and a fresh watcher is started for the replacement session.

use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use librqbit::{AddTorrent, api::Api};
use notify::Watcher;
use parking_lot::RwLock;
use tokio::sync::mpsc;
use tracing::{debug, warn};
use walkdir::WalkDir;

use crate::{hooks::HookDispatcher, state::SharedState};

/// What the watcher captured from the config it was started with.
struct WatchOpts {
    /// Add watched torrents in a paused state (config `auto_add_paused`).
    auto_add_paused: bool,
    /// The move folder, when it is a strict subfolder of the watch folder.
    /// Files inside it are skipped: they were already added and moved there
    /// (or archived by the user), and the recursive watch would otherwise
    /// pick them up again on every event.
    move_subtree: Option<PathBuf>,
}

impl WatchOpts {
    fn skips(&self, path: &Path) -> bool {
        self.move_subtree
            .as_ref()
            .is_some_and(|folder| path_is_in(path, folder))
    }
}

/// Whether `path` lies inside `folder`. Both are canonicalized when possible
/// so non-normalized or symlinked config paths still match; a path that does
/// not exist (yet) falls back to the raw comparison. `Path::starts_with`
/// compares whole components, so a similarly-named sibling
/// (`/a/watch-moved` vs `/a/watch`) never matches.
fn path_is_in(path: &Path, folder: &Path) -> bool {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    canon(path).starts_with(canon(folder)) || path.starts_with(folder)
}

/// `folder` as an excluded subtree when it is a strict subfolder of `parent`
/// (and not `parent` itself — that config keeps working, the move just
/// becomes a no-op: the source is already in the destination folder).
fn strict_subfolder(folder: &Path, parent: &Path) -> Option<PathBuf> {
    if folder.as_os_str().is_empty() {
        return None;
    }
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let (f, p) = (canon(folder), canon(parent));
    (f != p && f.starts_with(&p)).then(|| folder.to_path_buf())
}

/// Start watching `load_torrents_folder` from the shared config (no-op when
/// it is not configured). Called once per session from `State::new` and after
/// every reconfigure.
pub fn start_folder_watcher(shared: Arc<RwLock<SharedState>>, hooks: Arc<HookDispatcher>) {
    let (watch_folder, move_folder, auto_add_paused, api) = {
        let g = shared.read();
        let config = g.config();
        (
            config.load_torrents_folder,
            config.move_torrents_folder,
            config.auto_add_paused,
            g.api(),
        )
    };
    if watch_folder.as_os_str().is_empty() {
        return;
    }

    let opts = WatchOpts {
        auto_add_paused,
        move_subtree: strict_subfolder(&move_folder, &watch_folder),
    };

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut watcher = match notify::recommended_watcher(move |ev| {
        let _ = tx.send(ev);
    }) {
        Ok(w) => w,
        Err(e) => {
            warn!(folder = %watch_folder.display(), error = %e, "error creating folder watcher");
            return;
        }
    };
    if let Err(e) = watcher.watch(&watch_folder, notify::RecursiveMode::Recursive) {
        warn!(folder = %watch_folder.display(), error = %e, "error watching folder");
        return;
    }

    let cancel = api.session().cancellation_token().clone();
    let task = async move {
        // Keep the watcher alive for as long as this task runs.
        let _watcher = watcher;

        // Files already in the folder are added first; live events cover the
        // rest (a duplicate add of the same file is harmless — the session
        // reports it as already managed, and the source is moved out of the
        // watch folder all the same, renamed when its name is taken there).
        for entry in WalkDir::new(&watch_folder)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.into_path();
            if is_torrent_or_magnet(&path) && !opts.skips(&path) {
                process_file(&api, &hooks, opts.auto_add_paused, path).await;
            }
        }

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    debug!("folder watcher stopping with its session");
                    break;
                }
                ev = rx.recv() => match ev {
                    None => break,
                    Some(Ok(ev)) => handle_event(&api, &hooks, &opts, ev).await,
                    Some(Err(e)) => debug!(error = %e, "folder watch error event"),
                }
            }
        }
    };
    tokio::spawn(task);
}

fn is_torrent_or_magnet(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("torrent") | Some("magnet")
    )
}

async fn handle_event(api: &Arc<Api>, hooks: &HookDispatcher, opts: &WatchOpts, ev: notify::Event) {
    if !matches!(
        ev.kind,
        notify::EventKind::Create(_) | notify::EventKind::Modify(_)
    ) {
        return;
    }
    for path in ev.paths {
        if is_torrent_or_magnet(&path) && !opts.skips(&path) {
            process_file(api, hooks, opts.auto_add_paused, path).await;
        }
    }
}

/// Add one source file through the shared add path (which also moves it to
/// the configured folder on success). `auto_add_paused` comes from the
/// config the watcher was started with and starts the torrent paused. The
/// file is kept when the add fails, so the user can inspect it; it is
/// retried on the next filesystem event.
async fn process_file(
    api: &Arc<Api>,
    hooks: &HookDispatcher,
    auto_add_paused: bool,
    path: PathBuf,
) {
    if !path.exists() {
        // Already handled (e.g. a duplicate event after the move).
        return;
    }
    let add = match read_and_validate(&path) {
        Ok(add) => add,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "invalid file in watch folder");
            return;
        }
    };
    // overwrite: true, matching librqbit's own folder watcher.
    let from = path.display().to_string();
    if let Err(e) = hooks
        .add_and_dispatch_added(api, add, None, true, auto_add_paused, Some(path))
        .await
    {
        warn!(path = %from, error = %e, "error adding torrent from watch folder");
    }
}

/// Read and validate a source file, producing an [`AddTorrent`]. This mirrors
/// the validation librqbit's watch module performs, so invalid files are
/// refused (and logged) before reaching the session.
fn read_and_validate(path: &Path) -> anyhow::Result<AddTorrent<'static>> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("torrent") => {
            let buf = std::fs::read(path).context("error reading file")?;
            librqbit_core::torrent_metainfo::torrent_from_bytes(&buf)
                .context("invalid .torrent file")?;
            Ok(AddTorrent::from_bytes(buf))
        }
        Some("magnet") => {
            let mut url = String::new();
            std::fs::File::open(path)
                .context("error opening file")?
                .read_to_string(&mut url)
                .context("error reading file")?;
            librqbit::Magnet::parse(&url).context("invalid .magnet file")?;
            Ok(AddTorrent::from_url(url))
        }
        other => anyhow::bail!("unsupported file type: {other:?}"),
    }
}

/// Move `src` into `dest_folder`, keeping its file name — or, when that name
/// is already taken there, the first free `name (1).ext`, `name (2).ext`, …
/// ([`first_free_sibling`]). An existing file is never overwritten, the
/// folder is created if needed, and a rename across filesystems falls back to
/// copy + remove. Used by
/// [`crate::hooks::HookDispatcher::add_and_dispatch_added`] for
/// move-after-add on both watcher- and dialog-originated files.
pub(crate) fn move_file(src: &Path, dest_folder: &Path) -> anyhow::Result<()> {
    let file_name = src
        .file_name()
        .with_context(|| format!("{} has no file name", src.display()))?;
    let dest = dest_folder.join(file_name);
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    if canon(src) == canon(&dest) {
        // The source already lives in the destination folder under its own
        // name (e.g. the move folder is the watch folder). Renaming would
        // produce a new watched file and an endless rename chain.
        debug!(path = %src.display(), "source already in move folder, keeping it");
        return Ok(());
    }
    std::fs::create_dir_all(dest_folder)
        .with_context(|| format!("error creating {}", dest_folder.display()))?;
    let dest = if dest.exists() {
        first_free_sibling(&dest)
    } else {
        dest
    };
    if let Err(e) = std::fs::rename(src, &dest) {
        if e.kind() != std::io::ErrorKind::CrossesDevices {
            return Err(e).with_context(|| format!("error moving to {}", dest.display()));
        }
        std::fs::copy(src, &dest)
            .with_context(|| format!("error copying to {}", dest.display()))?;
        std::fs::remove_file(src).with_context(|| format!("error removing {}", src.display()))?;
    }
    Ok(())
}

/// `base.torrent` exists -> the first free `base (1).torrent`,
/// `base (2).torrent`, … next to it. The extension is kept in place so moved
/// files stay recognizable as `.torrent` / `.magnet` (and re-watchable).
fn first_free_sibling(base: &Path) -> PathBuf {
    let name = base.file_name().unwrap_or_default();
    let stem = Path::new(name)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let ext = Path::new(name).extension().map(|e| e.to_string_lossy());
    for n in 1u64.. {
        let candidate = match &ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = base.with_file_name(candidate);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("an unused name is eventually found")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Minimal valid single-file torrent (`d5:hello` content, one zero
    /// piece); anything torrent_from_bytes accepts is enough for validation.
    pub(crate) fn torrent_bytes() -> Vec<u8> {
        torrent_bytes_with_name("hello")
    }

    /// [`torrent_bytes`] with a different name, so two files produce two
    /// distinct torrents (different info hashes).
    fn torrent_bytes_with_name(name: &str) -> Vec<u8> {
        format!(
            "d4:infod6:lengthi5e4:name{}:{}12:piece lengthi16384e6:pieces20:####################ee",
            name.len(),
            name
        )
        .into_bytes()
    }

    pub(crate) struct TempDir(std::path::PathBuf);

    impl TempDir {
        pub(crate) fn new(tag: &str) -> Self {
            // The counter guards against two parallel tests sampling the same
            // nanosecond (observed on macOS) and silently sharing a directory.
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rqbit-watcher-{tag}-{}-{n}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn move_file_moves_and_creates_dest_folder() {
        let src_dir = TempDir::new("src");
        let dest_dir = TempDir::new("dest").0.join("nested");
        let src = src_dir.path().join("t.torrent");
        std::fs::write(&src, b"data").unwrap();

        move_file(&src, &dest_dir).unwrap();

        assert!(!src.exists());
        assert_eq!(std::fs::read(dest_dir.join("t.torrent")).unwrap(), b"data");
    }

    #[test]
    fn move_file_renames_to_free_name_on_collision() {
        let src_dir = TempDir::new("src");
        let dest_dir = TempDir::new("dest");
        let src = src_dir.path().join("t.torrent");
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(dest_dir.path().join("t.torrent"), b"old1").unwrap();
        std::fs::write(dest_dir.path().join("t (1).torrent"), b"old2").unwrap();
        // An unrelated name with the same number must not confuse the search.
        std::fs::write(dest_dir.path().join("other (1).torrent"), b"x").unwrap();

        move_file(&src, dest_dir.path()).unwrap();

        assert!(!src.exists());
        assert_eq!(
            std::fs::read(dest_dir.path().join("t.torrent")).unwrap(),
            b"old1"
        );
        assert_eq!(
            std::fs::read(dest_dir.path().join("t (1).torrent")).unwrap(),
            b"old2"
        );
        assert_eq!(
            std::fs::read(dest_dir.path().join("t (2).torrent")).unwrap(),
            b"new",
            "moved file gets the first free numbered name, extension intact"
        );
    }

    #[test]
    fn move_file_into_its_own_folder_is_a_no_op() {
        let dir = TempDir::new("same");
        let src = dir.path().join("t.torrent");
        std::fs::write(&src, b"data").unwrap();

        move_file(&src, dir.path()).unwrap();

        assert_eq!(std::fs::read(&src).unwrap(), b"data");
        assert!(
            !dir.path().join("t (1).torrent").exists(),
            "no rename may happen, or the watcher would rename forever"
        );
    }

    #[test]
    fn read_and_validate_accepts_torrent_and_magnet() {
        let dir = TempDir::new("validate");
        let torrent = dir.path().join("a.torrent");
        std::fs::write(&torrent, torrent_bytes()).unwrap();
        let magnet = dir.path().join("m.magnet");
        std::fs::write(
            &magnet,
            "magnet:?xt=urn:btih:0000000000000000000000000000000000000000&dn=x\n",
        )
        .unwrap();
        let junk = dir.path().join("j.torrent");
        std::fs::write(&junk, b"not bencoded").unwrap();

        assert!(read_and_validate(&torrent).is_ok());
        assert!(read_and_validate(&magnet).is_ok());
        assert!(read_and_validate(&junk).is_err());
        assert!(read_and_validate(&dir.path().join("x.txt")).is_err());
    }

    #[test]
    fn move_subfolder_detection() {
        let parent = TempDir::new("parent");
        let child = parent.path().join("child");
        std::fs::create_dir_all(&child).unwrap();

        assert_eq!(
            strict_subfolder(&child, parent.path()),
            Some(child.clone()),
            "a subfolder of the watch folder is detected"
        );
        assert_eq!(
            strict_subfolder(parent.path(), parent.path()),
            None,
            "the watch folder itself is not excluded (move is a no-op skip)"
        );
        // A sibling whose name merely starts with the parent's is not a
        // subfolder (component-wise comparison, not a string prefix).
        let sibling = parent.path().with_file_name(format!(
            "{}-sibling",
            parent.path().file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&sibling).unwrap();
        assert_eq!(strict_subfolder(&sibling, parent.path()), None);
        assert_eq!(
            strict_subfolder(Path::new(""), parent.path()),
            None,
            "unset move folder is never excluded"
        );

        assert!(path_is_in(&child.join("x.torrent"), &child));
        assert!(!path_is_in(&parent.path().join("x.torrent"), &child));
        // Non-normalized input still matches via canonicalization.
        let dot_child = parent.path().join("child").join(".");
        assert!(path_is_in(&dot_child, &child));
    }

    // ── End-to-end over a real session ─────────────────────────────────────

    use librqbit::api::ApiTorrentListOpts;

    /// A file present in the watch folder when the watcher starts is added to
    /// the session and moved to the move folder.
    #[tokio::test]
    async fn watched_folder_end_to_end() {
        let watch_dir = TempDir::new("watch");
        let move_dir = TempDir::new("move");
        let src = watch_dir.path().join("dropped.torrent");
        std::fs::write(&src, torrent_bytes()).unwrap();
        let session = librqbit::Session::new_with_opts(
            std::env::temp_dir(),
            librqbit::SessionOptions {
                dht: None,
                persistence: None,
                listen: None,
                disable_local_service_discovery: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let api = Arc::new(Api::new(session, None, None));
        let config = crate::config::RqbitDesktopConfig {
            load_torrents_folder: watch_dir.path().to_path_buf(),
            move_torrents_folder: move_dir.path().to_path_buf(),
            ..Default::default()
        };
        let shared = Arc::new(RwLock::new(SharedState::for_test(config, api.clone())));
        let hooks = Arc::new(HookDispatcher::new(
            shared.clone(),
            tokio::runtime::Handle::current(),
        ));

        start_folder_watcher(shared.clone(), hooks);

        let dest = move_dir.path().join("dropped.torrent");
        for _ in 0..100 {
            if dest.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        assert!(dest.exists(), "source file should have been moved");
        assert_eq!(std::fs::read(&dest).unwrap(), torrent_bytes());
        assert!(
            !src.exists(),
            "source file should be gone from the watch folder"
        );
        assert_eq!(
            api.api_torrent_list_ext(ApiTorrentListOpts { with_stats: false })
                .torrents
                .len(),
            1,
            "the torrent should be managed by the session"
        );

        // auto_add_paused defaults to true, so the torrent must settle in
        // "paused" (after the initial file check) rather than going live.
        let mut state = String::new();
        for _ in 0..100 {
            state = api
                .api_torrent_list_ext(ApiTorrentListOpts { with_stats: true })
                .torrents
                .first()
                .and_then(|t| t.stats.as_ref().map(|s| s.state.to_string()))
                .unwrap_or_default();
            if state == "paused" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(state, "paused", "watched torrent should be added paused");
    }

    /// When the move folder is configured as a subfolder of the watch folder,
    /// files in it are never autoloaded — neither files moved there after an
    /// add, nor files that were already sitting in it at startup, nor files
    /// dropped into it later.
    #[tokio::test]
    async fn watched_folder_skips_move_subfolder() {
        let watch_dir = TempDir::new("watch");
        let move_dir = watch_dir.path().join("moved");
        std::fs::create_dir_all(&move_dir).unwrap();

        // Already archived in the subfolder before the watcher starts: the
        // initial recursive scan must skip it.
        let archived = move_dir.join("archived.torrent");
        std::fs::write(&archived, torrent_bytes_with_name("archived")).unwrap();

        // In the watch root: must be added and moved into the subfolder.
        let src = watch_dir.path().join("dropped.torrent");
        std::fs::write(&src, torrent_bytes()).unwrap();

        let session = librqbit::Session::new_with_opts(
            std::env::temp_dir(),
            librqbit::SessionOptions {
                dht: None,
                persistence: None,
                listen: None,
                disable_local_service_discovery: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let api = Arc::new(Api::new(session, None, None));
        let config = crate::config::RqbitDesktopConfig {
            load_torrents_folder: watch_dir.path().to_path_buf(),
            move_torrents_folder: move_dir.clone(),
            ..Default::default()
        };
        let shared = Arc::new(RwLock::new(SharedState::for_test(config, api.clone())));
        let hooks = Arc::new(HookDispatcher::new(
            shared.clone(),
            tokio::runtime::Handle::current(),
        ));

        start_folder_watcher(shared.clone(), hooks);

        let dest = move_dir.join("dropped.torrent");
        for _ in 0..100 {
            if dest.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(dest.exists(), "source file should have been moved");
        assert!(!src.exists(), "source file should be gone");

        // Drop a new file straight into the subfolder: the recursive watch
        // sees the event, but it must be ignored.
        let stray = move_dir.join("stray.torrent");
        std::fs::write(&stray, torrent_bytes_with_name("stray")).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert!(archived.exists(), "archived file must not be consumed");
        assert!(
            stray.exists(),
            "file dropped into the subfolder must be ignored"
        );
        let torrents = api
            .api_torrent_list_ext(ApiTorrentListOpts { with_stats: false })
            .torrents;
        assert_eq!(
            torrents.len(),
            1,
            "only the watch-root torrent should have been added"
        );
        assert_eq!(torrents[0].name.as_deref(), Some("hello"));
    }
}
