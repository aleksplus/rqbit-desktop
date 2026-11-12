//! Watches the configured "autoload torrents" folder and feeds new
//! `.torrent` / `.magnet` files through the same add path as the add-torrent
//! dialog ([`HookDispatcher::add_and_dispatch_added`]). When file-selection
//! hooks apply, a fresh torrent is first added paused with an explicitly empty
//! selection. Hooks run after metadata initialization, and the torrent starts
//! only after their selection is persisted (unless `auto_add_paused` keeps it
//! paused). Hook failures leave the pending paused-empty state intact.
//! The shared callback also moves the source file to the configured "move
//! added torrents to" folder after the add, for both watcher- and
//! dialog-originated files. Each source is processed independently so slow
//! magnet metadata resolution cannot block other watched files.
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
                spawn_process_file(&api, &hooks, opts.auto_add_paused, path);
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

async fn handle_event(
    api: &Arc<Api>,
    hooks: &Arc<HookDispatcher>,
    opts: &WatchOpts,
    ev: notify::Event,
) {
    if !matches!(
        ev.kind,
        notify::EventKind::Create(_) | notify::EventKind::Modify(_)
    ) {
        return;
    }
    for path in ev.paths {
        if is_torrent_or_magnet(&path) && !opts.skips(&path) {
            spawn_process_file(api, hooks, opts.auto_add_paused, path);
        }
    }
}

/// Process each source independently. In particular, librqbit resolves magnet
/// metadata inside `api_add_torrent`, so awaiting that inline would block every
/// other watched file behind one slow magnet.
fn spawn_process_file(
    api: &Arc<Api>,
    hooks: &Arc<HookDispatcher>,
    auto_add_paused: bool,
    path: PathBuf,
) {
    let api = api.clone();
    let hooks = hooks.clone();
    let cancel = api.session().cancellation_token().clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = cancel.cancelled() => {}
            _ = process_file(&api, &hooks, auto_add_paused, path) => {}
        }
    });
}

/// Add one source file through the shared add path (which also moves it to
/// the configured folder on success). `auto_add_paused` comes from the
/// config the watcher was started with. Selection hooks force an initial
/// paused-empty state even when automatic start is enabled. The source is
/// kept when the add fails, so the user can inspect it; it is retried on the
/// next filesystem event.
async fn process_file(
    api: &Arc<Api>,
    hooks: &Arc<HookDispatcher>,
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
    let selection_hooks = hooks.autoload_hooks();
    let waits_for_selection = !selection_hooks.is_empty();
    // An explicit empty selection is the fail-closed pending state. `None`
    // would mark every file for download before the hooks responded.
    let only_files = waits_for_selection.then(Vec::new);
    let paused = waits_for_selection || auto_add_paused;
    // overwrite: true, matching librqbit's own folder watcher.
    let from = path.display().to_string();
    match hooks
        .add_autoload(api, add, only_files, true, paused, Some(path))
        .await
    {
        Ok(outcome) => {
            if waits_for_selection && let Some(id) = outcome.fresh_id {
                hooks.spawn_autoload_selection(
                    api.clone(),
                    selection_hooks,
                    id,
                    auto_add_paused,
                    from,
                );
            }
        }
        Err(e) => {
            warn!(path = %from, error = %e, "error adding torrent from watch folder");
        }
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

    /// A minimal two-file torrent (`test/aaa.mkv` = 5 bytes,
    /// `test/bbb.nfo` = 1 byte), for exercising file selection.
    pub(crate) fn multi_file_torrent_bytes() -> Vec<u8> {
        "d4:infod5:filesld6:lengthi5e4:pathl7:aaa.mkveed6:lengthi1e4:pathl7:bbb.nfoeee4:name4:test12:piece lengthi16384e6:pieces20:####################ee"
            .as_bytes()
            .to_vec()
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

    use crate::config::{HookConfig, HttpMethod, RqbitDesktopConfigHooks};
    use crate::hooks::HookEvent;
    use crate::hooks::testutil::{serve_once, serve_once_gated, serve_once_with_status};
    use librqbit::api::TorrentIdOrHash;

    async fn selection_test_api() -> Arc<Api> {
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
        Arc::new(Api::new(session, None, None))
    }

    fn selection_hook(
        addr: std::net::SocketAddr,
        events: Vec<HookEvent>,
        autoload_file_selection: bool,
    ) -> HookConfig {
        HookConfig {
            name: "selector".into(),
            enabled: true,
            events,
            autoload_file_selection,
            method: HttpMethod::Post,
            url: format!("http://{addr}/select"),
            timeout_secs: 5,
            headers: Default::default(),
            poll_secs: 10,
        }
    }

    fn start_selection_watcher(
        api: &Arc<Api>,
        watch_dir: &TempDir,
        move_dir: &TempDir,
        hook_configs: Vec<HookConfig>,
        auto_add_paused: bool,
    ) {
        let config = crate::config::RqbitDesktopConfig {
            load_torrents_folder: watch_dir.path().to_path_buf(),
            move_torrents_folder: move_dir.path().to_path_buf(),
            auto_add_paused,
            hooks: RqbitDesktopConfigHooks {
                hooks: hook_configs,
            },
            ..Default::default()
        };
        let shared = Arc::new(RwLock::new(SharedState::for_test(config, api.clone())));
        let hooks = Arc::new(HookDispatcher::new(
            shared.clone(),
            tokio::runtime::Handle::current(),
        ));
        start_folder_watcher(shared, hooks);
    }

    async fn only_torrent_id(api: &Api) -> usize {
        let mut torrents = Vec::new();
        for _ in 0..100 {
            torrents = api
                .api_torrent_list_ext(ApiTorrentListOpts { with_stats: false })
                .torrents;
            if torrents.len() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(torrents.len(), 1, "the torrent should have been added");
        torrents[0].id.unwrap()
    }

    fn included_files(api: &Api, id: usize) -> Vec<bool> {
        api.api_torrent_details(TorrentIdOrHash::Id(id))
            .unwrap()
            .files
            .unwrap()
            .iter()
            .map(|file| file.included)
            .collect()
    }

    async fn wait_for_selection(api: &Api, id: usize, expected: &[bool]) {
        let mut actual = Vec::new();
        for _ in 0..100 {
            actual = included_files(api, id);
            if actual == expected {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(actual, expected);
    }

    async fn wait_for_state(api: &Api, id: usize, expected: &str) {
        let mut state = String::new();
        for _ in 0..100 {
            state = api
                .api_stats_v1(TorrentIdOrHash::Id(id))
                .unwrap()
                .state
                .to_string();
            if state == expected {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(state, expected);
    }

    fn assert_paused_empty(api: &Api, id: usize) {
        assert!(
            api.mgr_handle(TorrentIdOrHash::Id(id)).unwrap().is_paused(),
            "torrent must remain paused"
        );
        assert_eq!(included_files(api, id), [false, false]);
    }

    /// Explicit file-selection hooks apply their response after the pending
    /// add, while the configured paused setting remains in effect.
    #[tokio::test]
    async fn watched_folder_applies_file_selection_hooks() {
        let watch_dir = TempDir::new("watch-sel");
        let move_dir = TempDir::new("move-sel");
        let src = watch_dir.path().join("multi.torrent");
        std::fs::write(&src, multi_file_torrent_bytes()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let api = selection_test_api().await;
        let server = tokio::spawn(serve_once(listener, r#"{"exclude": [1]}"#));
        start_selection_watcher(
            &api,
            &watch_dir,
            &move_dir,
            vec![selection_hook(addr, vec![HookEvent::FileSelection], false)],
            true,
        );

        server.await.unwrap();
        let id = only_torrent_id(&api).await;
        wait_for_selection(&api, id, &[true, false]).await;
        assert!(api.mgr_handle(TorrentIdOrHash::Id(id)).unwrap().is_paused());
        assert!(move_dir.path().join("multi.torrent").exists());
    }

    /// A columns-only moviesync-style hook opts into strict selection. The
    /// torrent is visibly paused-empty while the response is held, then starts
    /// only after the desired files have been persisted.
    #[tokio::test]
    async fn watched_columns_hook_delays_selection_and_auto_start() {
        let watch_dir = TempDir::new("watch-delayed");
        let move_dir = TempDir::new("move-delayed");
        let src = watch_dir.path().join("multi.torrent");
        std::fs::write(&src, multi_file_torrent_bytes()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let api = selection_test_api().await;
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(serve_once_gated(
            listener,
            r#"{"exclude": [1]}"#,
            seen_tx,
            release_rx,
        ));
        start_selection_watcher(
            &api,
            &watch_dir,
            &move_dir,
            vec![selection_hook(addr, vec![HookEvent::TorrentColumns], true)],
            false,
        );

        tokio::time::timeout(std::time::Duration::from_secs(5), seen_rx)
            .await
            .unwrap()
            .unwrap();
        let id = only_torrent_id(&api).await;
        assert_paused_empty(&api, id);
        release_tx.send(()).unwrap();
        let raw_request = server.await.unwrap();
        assert!(raw_request.contains(r#""event":"file_selection""#));
        assert!(
            raw_request
                .to_lowercase()
                .contains("x-rqbit-event: file_selection")
        );
        wait_for_selection(&api, id, &[true, false]).await;
        wait_for_state(&api, id, "live").await;
        assert!(!api.mgr_handle(TorrentIdOrHash::Id(id)).unwrap().is_paused());
    }

    /// Every invalid/failing response leaves the original pending selection
    /// intact. The successful add still archives its source file.
    #[tokio::test]
    async fn watched_folder_selection_failures_stay_paused_empty() {
        for (tag, status, body) in [
            ("empty", "200 OK", ""),
            ("malformed", "200 OK", "not json"),
            ("missing-directives", "200 OK", r#"{"unknown": true}"#),
            ("invalid-index", "200 OK", r#"{"exclude": [2]}"#),
            (
                "http-error",
                "500 Internal Server Error",
                r#"{"exclude": [1]}"#,
            ),
        ] {
            let watch_dir = TempDir::new(&format!("watch-failure-{tag}"));
            let move_dir = TempDir::new(&format!("move-failure-{tag}"));
            let src = watch_dir.path().join("multi.torrent");
            std::fs::write(&src, multi_file_torrent_bytes()).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let api = selection_test_api().await;
            let server = tokio::spawn(serve_once_with_status(listener, status, body));
            start_selection_watcher(
                &api,
                &watch_dir,
                &move_dir,
                vec![selection_hook(addr, vec![HookEvent::TorrentColumns], true)],
                false,
            );

            server.await.unwrap();
            let id = only_torrent_id(&api).await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            assert_paused_empty(&api, id);
            assert!(move_dir.path().join("multi.torrent").exists(), "{tag}");
        }
    }

    #[tokio::test]
    async fn watched_folder_hook_timeout_stays_paused_empty() {
        let watch_dir = TempDir::new("watch-timeout");
        let move_dir = TempDir::new("move-timeout");
        let src = watch_dir.path().join("multi.torrent");
        std::fs::write(&src, multi_file_torrent_bytes()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let api = selection_test_api().await;
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(serve_once_gated(
            listener,
            r#"{"exclude": [1]}"#,
            seen_tx,
            release_rx,
        ));
        let mut hook = selection_hook(addr, vec![HookEvent::FileSelection], false);
        hook.timeout_secs = 1;
        start_selection_watcher(&api, &watch_dir, &move_dir, vec![hook], false);

        tokio::time::timeout(std::time::Duration::from_secs(5), seen_rx)
            .await
            .unwrap()
            .unwrap();
        let id = only_torrent_id(&api).await;
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert_paused_empty(&api, id);
        release_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn watched_folder_valid_empty_selection_does_not_auto_start() {
        let watch_dir = TempDir::new("watch-empty-selection");
        let move_dir = TempDir::new("move-empty-selection");
        let src = watch_dir.path().join("multi.torrent");
        std::fs::write(&src, multi_file_torrent_bytes()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let api = selection_test_api().await;
        let server = tokio::spawn(serve_once(listener, r#"{"include": []}"#));
        start_selection_watcher(
            &api,
            &watch_dir,
            &move_dir,
            vec![selection_hook(addr, vec![HookEvent::FileSelection], false)],
            false,
        );

        server.await.unwrap();
        let id = only_torrent_id(&api).await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_paused_empty(&api, id);
    }

    /// Columns providers that have not opted in retain the old immediate add
    /// behavior and are never contacted for file selection.
    #[tokio::test]
    async fn watched_columns_hook_without_opt_in_uses_immediate_auto_start() {
        let watch_dir = TempDir::new("watch-no-opt-in");
        let move_dir = TempDir::new("move-no-opt-in");
        let src = watch_dir.path().join("multi.torrent");
        std::fs::write(&src, multi_file_torrent_bytes()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let api = selection_test_api().await;
        start_selection_watcher(
            &api,
            &watch_dir,
            &move_dir,
            vec![selection_hook(addr, vec![HookEvent::TorrentColumns], false)],
            false,
        );

        let id = only_torrent_id(&api).await;
        wait_for_selection(&api, id, &[true, true]).await;
        wait_for_state(&api, id, "live").await;
    }

    /// A later hook failure cannot leave an earlier hook's partial selection
    /// persisted or trigger auto-start.
    #[tokio::test]
    async fn watched_folder_multiple_hooks_are_atomic() {
        let watch_dir = TempDir::new("watch-atomic");
        let move_dir = TempDir::new("move-atomic");
        let src = watch_dir.path().join("multi.torrent");
        std::fs::write(&src, multi_file_torrent_bytes()).unwrap();
        let first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first_addr = first.local_addr().unwrap();
        let second = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second_addr = second.local_addr().unwrap();
        let api = selection_test_api().await;
        let first_server = tokio::spawn(serve_once(first, r#"{"exclude": [1]}"#));
        let second_server = tokio::spawn(serve_once(second, "not json"));
        start_selection_watcher(
            &api,
            &watch_dir,
            &move_dir,
            vec![
                selection_hook(first_addr, vec![HookEvent::FileSelection], false),
                selection_hook(second_addr, vec![HookEvent::TorrentColumns], true),
            ],
            false,
        );

        first_server.await.unwrap();
        second_server.await.unwrap();
        let id = only_torrent_id(&api).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_paused_empty(&api, id);
    }

    /// librqbit resolves magnet metadata before registering the torrent. If
    /// resolution is impossible, auto-load must not retry an unrestricted add
    /// or archive the source as though selection had succeeded.
    #[tokio::test]
    async fn watched_magnet_metadata_failure_does_not_add_unrestricted() {
        let watch_dir = TempDir::new("watch-magnet-pending");
        let move_dir = TempDir::new("move-magnet-pending");
        let src = watch_dir.path().join("pending.magnet");
        // Hybrid magnet: librqbit currently requires the v1 hash for magnet
        // adds, while the v2 hash preserves BEP 52 identity.
        let magnet = format!(
            "magnet:?xt=urn:btih:{}&xt=urn:btmh:1220{}&dn=pending",
            "0".repeat(40),
            "0".repeat(64)
        );
        std::fs::write(&src, magnet).unwrap();
        assert!(read_and_validate(&src).is_ok());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let api = selection_test_api().await;
        start_selection_watcher(
            &api,
            &watch_dir,
            &move_dir,
            vec![selection_hook(addr, vec![HookEvent::TorrentColumns], true)],
            false,
        );

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            api.api_torrent_list_ext(ApiTorrentListOpts { with_stats: false })
                .torrents
                .is_empty()
        );
        assert!(src.exists());
        assert!(!move_dir.path().join("pending.magnet").exists());
    }
}
