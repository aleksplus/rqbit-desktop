//! Per-torrent artwork: the poster shown behind the detail panel's Overview
//! tab and in the torrent list rows' corner.
//!
//! Resolution per torrent, in priority order:
//! 1. Hook poster — an Image-kind custom column keyed `poster` whose value
//!    is an image URL (see `columns.rs`).
//! 2. MyShows poster — the torrent's release name is cleaned into a show
//!    title and searched on api.myshows.me (JSON-RPC); the first result
//!    carrying a poster wins. Cached per cleaned title.
//! 3. Media preview — the largest completed video file's frame, extracted
//!    with the system `ffmpeg` (an absent ffmpeg just disables this source).
//!
//! Resolution runs in one background worker ([`ArtworkProvider::run`]);
//! decoration (`decorate_list`/`decorate_details`) only kicks the worker and
//! attaches what is already ready, so the frequent UI polls never block.
//! Bytes are served by the artwork route in `media.rs`, keyed by info-hash.
//! Preview frames are extracted lazily — advertised as soon as the daemon
//! knows a completed video file exists, but only decoded when the route is
//! first asked for the torrent.
//!
//! The UI cache key ([`ArtData::key`]) changes whenever the image behind a
//! torrent changes (new source, re-extraction, eviction), so clients refetch
//! on the next decorated payload.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use librqbit::api::TorrentIdOrHash;
use parking_lot::Mutex;
use rqbit_protocol as proto;
use rqbit_protocol::conv;
use rqbit_protocol::types::{TorrentArtwork, TorrentDetails, TorrentList};
use serde::Deserialize;

use crate::columns::ColumnsProvider;
use crate::server::ApiSlot;

/// URL path prefix of the artwork route (`media.rs`); the full path is
/// `{prefix}/{info_hash}/artwork`.
pub const ARTWORK_PATH_PREFIX: &str = "/torrents/by-hash";

/// MyShows JSON-RPC endpoint.
const MYSHOWS_RPC_URL: &str = "https://api.myshows.me/v2/rpc/";

/// Cap on artwork bytes kept in memory (LRU-evicted beyond this).
const MAX_CACHED_BYTES: usize = 64 * 1024 * 1024;
/// Cap on torrents whose artwork bytes are kept in memory.
const MAX_CACHED_ENTRIES: usize = 128;

/// No completed media file (yet) — recheck soon, a download may finish.
const COOLDOWN_RETRYABLE: Duration = Duration::from_secs(60);
/// MyShows found nothing for this title / HTTP or ffmpeg failed.
const COOLDOWN_ERROR: Duration = Duration::from_secs(600);
/// The ffmpeg binary is absent — effectively permanent.
const COOLDOWN_NO_FFMPEG: Duration = Duration::from_secs(3600);

/// How many torrents resolve at once (the artwork route waits up to
/// [`EXTRACT_WAIT`] for its torrent, so a couple of slots must stay free).
const MAX_CONCURRENT_RESOLVES: usize = 4;

/// Minimum spacing between MyShows API searches (politeness).
const MYSHOWS_SPACING: Duration = Duration::from_secs(1);
/// HTTP timeouts for poster downloads and MyShows requests.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// ffmpeg extraction budget.
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(10);
/// Where the preview frame is taken from: at least the first minute is
/// skipped so intros (shared by every episode of a show) don't end up as
/// identical artwork. Shorter files fall back to their first frame.
const PREVIEW_SEEK_SECS: u64 = 60;
/// How long the artwork route waits for a lazy preview extraction.
const EXTRACT_WAIT: Duration = Duration::from_secs(12);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Hook,
    MyShows,
    Preview,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Hook => "hook",
            Source::MyShows => "myshows",
            Source::Preview => "preview",
        }
    }
}

/// Why the last resolution attempt produced nothing.
#[derive(Debug, Clone, Copy)]
enum Failure {
    NoCompletedMedia,
    Error,
    NoFfmpeg,
}

impl Failure {
    fn cooldown(self) -> Duration {
        match self {
            Failure::NoCompletedMedia => COOLDOWN_RETRYABLE,
            Failure::Error => COOLDOWN_ERROR,
            Failure::NoFfmpeg => COOLDOWN_NO_FFMPEG,
        }
    }
}

/// Ready (or advertised) artwork for one torrent.
#[derive(Clone)]
struct ArtData {
    source: Source,
    /// Stable identity of the image within the source (URL hash, MyShows
    /// show id, preview file index) — part of the UI cache key.
    ident: String,
    /// Bumped when the image behind the same (source, ident) changes, so
    /// clients refetch.
    rev: u32,
    content_type: &'static str,
    /// `None` while a preview frame is advertised but not extracted yet, or
    /// after an eviction (the next resolve re-derives the bytes).
    bytes: Option<Arc<[u8]>>,
    last_used: Instant,
}

impl ArtData {
    fn key(&self) -> String {
        format!("{}:{}#{}", self.source.as_str(), self.ident, self.rev)
    }
}

#[derive(Default)]
struct Entry {
    art: Option<ArtData>,
    /// Last failure and when the worker may retry. Ignored while `art` is
    /// `Some` (an existing image is never dropped by later failures).
    failed: Option<(Failure, Instant)>,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    /// Torrents waiting for the worker, background (decorated payloads) and
    /// interactive (artwork route hits) lanes; the interactive lane first.
    background: VecDeque<String>,
    interactive: VecDeque<String>,
    queued: HashSet<String>,
    /// Cleaned title → MyShows result (None = searched, nothing found).
    myshows_cache: HashMap<String, Option<(String, String)>>,
    last_myshows: Option<Instant>,
    bytes_total: usize,
}

impl Inner {
    fn next_job(&mut self) -> Option<(String, bool)> {
        if let Some(hash) = self.interactive.pop_front() {
            self.queued.remove(&hash);
            return Some((hash, true));
        }
        let hash = self.background.pop_front()?;
        self.queued.remove(&hash);
        Some((hash, false))
    }

    fn has_jobs(&self) -> bool {
        !self.interactive.is_empty() || !self.background.is_empty()
    }
}

/// Resolves and caches per-torrent artwork for the WS server, the artwork
/// HTTP route and the desktop's local client. Created (and started) wherever
/// the session is built — the desktop's session setup and the standalone
/// daemon's main — and dies with that session.
pub struct ArtworkProvider {
    api: ApiSlot,
    columns: Option<Arc<ColumnsProvider>>,
    myshows_enabled: bool,
    client: reqwest::Client,
    inner: Mutex<Inner>,
    wake: tokio::sync::Notify,
}

impl ArtworkProvider {
    pub fn new(
        api: ApiSlot,
        columns: Option<Arc<ColumnsProvider>>,
        myshows_enabled: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            api,
            columns,
            myshows_enabled,
            client: reqwest::Client::new(),
            inner: Mutex::new(Inner::default()),
            wake: tokio::sync::Notify::new(),
        })
    }

    /// Spawn the worker. A no-op when there is nothing to do? Never — the
    /// provider always serves previews, so callers start it unconditionally.
    pub fn start(self: &Arc<Self>) {
        tokio::spawn(self.clone().run());
    }

    /// The worker loop, for callers that manage their own task lifetime (the
    /// desktop spawns it on the session so it dies with a reconfigure).
    /// Resolutions run concurrently (bounded by
    /// [`MAX_CONCURRENT_RESOLVES`]) so a large library's artwork catches up
    /// promptly instead of trailing one torrent at a time.
    pub async fn run(self: Arc<Self>) {
        let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_RESOLVES));
        loop {
            let permit = permits
                .clone()
                .acquire_owned()
                .await
                .unwrap_or_else(|_| unreachable!("semaphore is never closed"));
            let job = self.inner.lock().next_job();
            match job {
                Some((hash, interactive)) => {
                    let this = self.clone();
                    tokio::spawn(async move {
                        this.resolve(&hash, interactive).await;
                        drop(permit);
                    });
                }
                None => {
                    drop(permit);
                    // Re-check after registering the future: a kick between
                    // the empty check and the await stores a permit.
                    let notified = self.wake.notified();
                    if self.inner.lock().has_jobs() {
                        continue;
                    }
                    notified.await;
                }
            }
        }
    }

    // --- decoration (called from UI-facing payload paths) ---

    pub fn decorate_list(&self, list: &mut TorrentList) {
        for torrent in &mut list.torrents {
            self.decorate(torrent);
        }
    }

    pub fn decorate_details(&self, details: &mut TorrentDetails) {
        self.decorate(details);
    }

    fn decorate(&self, details: &mut TorrentDetails) {
        let hash = details.info_hash.clone();
        if hash.is_empty() {
            return;
        }
        self.kick(&hash);
        let artwork = self.ready_artwork(&hash);
        details.artwork = artwork;
    }

    /// Queue the torrent for resolution unless something is already served
    /// or the last failure is still cooling down.
    fn kick(&self, hash: &str) {
        let mut queue = false;
        {
            let mut inner = self.inner.lock();
            let entry = inner.entries.entry(hash.to_owned()).or_default();
            let ready = entry.art.is_some();
            let cooling = entry
                .failed
                .is_some_and(|(_, until)| Instant::now() < until);
            if !ready && !cooling && inner.queued.insert(hash.to_owned()) {
                inner.background.push_back(hash.to_owned());
                queue = true;
            }
        }
        if queue {
            self.wake.notify_one();
        }
    }

    fn ready_artwork(&self, hash: &str) -> Option<TorrentArtwork> {
        let inner = self.inner.lock();
        let art = inner.entries.get(hash)?.art.as_ref()?;
        Some(TorrentArtwork {
            key: art.key(),
            path: format!("{ARTWORK_PATH_PREFIX}/{hash}/artwork"),
            source: art.source.as_str().to_owned(),
        })
    }

    // --- serving (called from the artwork HTTP route) ---

    /// The current artwork bytes, resolving first when needed (an advertised
    /// preview not yet extracted, or a cooldown that has expired). Waits at
    /// most [`EXTRACT_WAIT`] for the worker.
    pub async fn request_bytes(self: &Arc<Self>, hash: &str) -> Option<(Arc<[u8]>, &'static str)> {
        if let Some(ready) = self.ready_bytes(hash) {
            return Some(ready);
        }
        {
            let mut inner = self.inner.lock();
            if inner.queued.insert(hash.to_owned()) {
                inner.interactive.push_back(hash.to_owned());
            }
        }
        self.wake.notify_one();
        let deadline = Instant::now() + EXTRACT_WAIT;
        loop {
            if let Some(ready) = self.ready_bytes(hash) {
                return Some(ready);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(deadline - now) => {
                    return self.ready_bytes(hash);
                }
            }
        }
    }

    fn ready_bytes(&self, hash: &str) -> Option<(Arc<[u8]>, &'static str)> {
        let mut inner = self.inner.lock();
        let art = inner.entries.get_mut(hash)?.art.as_mut()?;
        art.last_used = Instant::now();
        let bytes = art.bytes.clone()?;
        Some((bytes, art.content_type))
    }

    // --- resolution (worker only) ---

    async fn resolve(&self, hash: &str, _interactive: bool) {
        let Some(torrent) = self.load_torrent(hash) else {
            self.fail(hash, Failure::Error);
            return;
        };
        let name = torrent.name.clone().unwrap_or_default();

        if let Some(url) = self.hook_poster(hash) {
            match fetch_image_with(&self.client, &url).await {
                Ok((bytes, content_type)) => {
                    self.ready(hash, Source::Hook, short_hash(&url), bytes, content_type);
                    return;
                }
                Err(e) => {
                    tracing::info!(hash, url = %url, error = %e, "hook poster fetch failed");
                }
            }
        }

        if self.myshows_enabled && !name.is_empty() {
            match self.myshows_poster(&name).await {
                Ok(Some((show_id, url))) => match fetch_image_with(&self.client, &url).await {
                    Ok((bytes, content_type)) => {
                        self.ready(hash, Source::MyShows, show_id, bytes, content_type);
                        return;
                    }
                    Err(e) => {
                        tracing::info!(hash, url = %url, error = %e, "myshows poster fetch failed");
                    }
                },
                Ok(None) => {}
                Err(e) => {
                    // Visible on the log pages: this is the main reason a
                    // downloading torrent has no poster yet.
                    tracing::info!(hash, error = %e, "myshows search failed");
                }
            }
        }

        match self.preview_candidate(&torrent) {
            Ok(Some((file_idx, path))) => {
                self.advertise_preview(hash, file_idx);
                match extract_frame(&path).await {
                    Ok((bytes, content_type)) => {
                        self.fill_preview_bytes(hash, file_idx, bytes, content_type)
                    }
                    Err(e) => {
                        tracing::debug!(hash, error = %e, "preview extraction failed");
                        if e == "ffmpeg not found" {
                            self.fail(hash, Failure::NoFfmpeg);
                        } else {
                            self.fail(hash, Failure::Error);
                        }
                    }
                }
            }
            Ok(None) => self.fail(hash, Failure::NoCompletedMedia),
            Err(e) => {
                tracing::debug!(hash, error = %e, "no preview candidate");
                self.fail(hash, Failure::Error);
            }
        }
    }

    /// Read the torrent (details + stats) from the live session, by hash.
    fn load_torrent(&self, hash: &str) -> Option<TorrentDetails> {
        let api = self.api.get();
        let Ok(id) = hash.parse::<librqbit::dht::Id20>() else {
            return None;
        };
        let id = TorrentIdOrHash::Hash(id);
        let mut details = conv::torrent_details(&api.api_torrent_details(id).ok()?, Vec::new());
        if details.stats.is_none() {
            details.stats = Some(Box::new(proto::TorrentStats::from(
                &api.api_stats_v1(id).ok()?,
            )));
        }
        Some(details)
    }

    /// The hook's poster URL for this torrent: an Image-kind column keyed
    /// `poster` with an `http(s)://` value (already validated in
    /// `columns.rs`'s sanitization).
    fn hook_poster(&self, hash: &str) -> Option<String> {
        let snapshot = self.columns.as_ref()?.snapshot()?;
        let declared = snapshot
            .state
            .columns
            .iter()
            .any(|c| c.key == "poster" && c.kind == proto::CustomColumnKind::Image);
        if !declared {
            return None;
        }
        let url = snapshot.torrent_values.get(hash)?.get("poster")?;
        (url.starts_with("http://") || url.starts_with("https://")).then(|| url.clone())
    }

    /// Search MyShows for the cleaned release title. Returns
    /// `(show id, poster url)`; `Ok(None)` = searched, nothing usable.
    async fn myshows_poster(&self, name: &str) -> Result<Option<(String, String)>, String> {
        let title = clean_title(name);
        if title.is_empty() {
            return Ok(None);
        }
        if let Some(cached) = self.inner.lock().myshows_cache.get(&title).cloned() {
            return Ok(cached);
        }
        // Politeness spacing between searches: reserve the next free slot
        // up front (resolutions run concurrently, so the last-search time
        // alone would let several searches fire together).
        let slot = {
            let mut inner = self.inner.lock();
            let now = Instant::now();
            let earliest = inner
                .last_myshows
                .map_or(now, |last| (last + MYSHOWS_SPACING).max(now));
            inner.last_myshows = Some(earliest);
            earliest
        };
        let now = Instant::now();
        if slot > now {
            tokio::time::sleep(slot - now).await;
        }

        let result = self.myshows_search(&title).await;
        let found = match result {
            Ok(found) => found,
            Err(e) => {
                // Do not cache transport errors; the next kick retries.
                return Err(e);
            }
        };
        self.inner.lock().myshows_cache.insert(title, found.clone());
        Ok(found)
    }

    async fn myshows_search(&self, title: &str) -> Result<Option<(String, String)>, String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "shows.Search",
            "params": { "query": title },
        });
        let resp = self
            .client
            .post(MYSHOWS_RPC_URL)
            .json(&body)
            .timeout(FETCH_TIMEOUT)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        let rpc: MyShowsRpc = resp.json().await.map_err(|e| e.to_string())?;
        let show = rpc
            .result
            .unwrap_or_default()
            .into_iter()
            .find(|s| s.image.as_deref().is_some_and(|u| u.starts_with("http")));
        Ok(show.map(|s| (format!("s{}", s.id), s.image.expect("filtered above"))))
    }

    /// The largest fully-downloaded video file and its on-disk path.
    fn preview_candidate(
        &self,
        torrent: &TorrentDetails,
    ) -> Result<Option<(usize, PathBuf)>, String> {
        let files = torrent.files.as_ref().ok_or("no files")?;
        let progress = torrent.stats.as_ref().map(|s| &s.file_progress);
        let mut best: Option<(usize, &proto::TorrentFile)> = None;
        for (idx, file) in files.iter().enumerate() {
            if file.padding || file.length == 0 || !is_video_file(&file.name) {
                continue;
            }
            let downloaded = progress.and_then(|p| p.get(idx)).copied().unwrap_or(0);
            if downloaded < file.length {
                continue;
            }
            if best
                .as_ref()
                .is_none_or(|(_, prev)| file.length > prev.length)
            {
                best = Some((idx, file));
            }
        }
        let Some((idx, file)) = best else {
            return Ok(None);
        };
        let path = disk_path(&torrent.output_folder, &file.components).ok_or("no output folder")?;
        if !path.exists() {
            return Err("file missing on disk".to_owned());
        }
        Ok(Some((idx, path)))
    }

    // --- state transitions ---

    fn ready(
        &self,
        hash: &str,
        source: Source,
        ident: String,
        bytes: Vec<u8>,
        content_type: &'static str,
    ) {
        let mut inner = self.inner.lock();
        let entry = inner.entries.entry(hash.to_owned()).or_default();
        let rev = match entry.art.as_ref() {
            Some(prev) if prev.source == source && prev.ident == ident => prev.rev + 1,
            _ => 0,
        };
        entry.failed = None;
        entry.art = Some(ArtData {
            source,
            ident,
            rev,
            content_type,
            bytes: Some(bytes.clone().into()),
            last_used: Instant::now(),
        });
        inner.bytes_total += bytes.len();
        drop(inner);
        tracing::info!(hash, source = source.as_str(), "torrent artwork ready");
        self.evict();
    }

    /// Advertise the preview source (bytes not yet extracted) so the UIs
    /// learn artwork is coming; the route request triggers the extraction.
    /// Never downgrades artwork whose bytes are still cached.
    fn advertise_preview(&self, hash: &str, file_idx: usize) {
        let mut inner = self.inner.lock();
        let entry = inner.entries.entry(hash.to_owned()).or_default();
        if entry.art.as_ref().is_some_and(|a| a.bytes.is_some()) {
            return;
        }
        let ident = format!("f{file_idx}");
        let rev = match entry.art.as_ref() {
            Some(prev) if prev.source == Source::Preview && prev.ident == ident => prev.rev,
            _ => 0,
        };
        entry.failed = None;
        entry.art = Some(ArtData {
            source: Source::Preview,
            ident,
            rev,
            content_type: "image/png",
            bytes: None,
            last_used: Instant::now(),
        });
    }

    fn fill_preview_bytes(
        &self,
        hash: &str,
        file_idx: usize,
        bytes: Vec<u8>,
        content_type: &'static str,
    ) {
        let mut inner = self.inner.lock();
        let Some(entry) = inner.entries.get_mut(hash) else {
            return;
        };
        // Only replace artwork without live bytes (the preview we advertised,
        // or an evicted poster whose source just failed again).
        if entry.art.as_ref().is_some_and(|a| a.bytes.is_some()) {
            return;
        }
        let ident = format!("f{file_idx}");
        let rev = match entry.art.as_ref() {
            Some(prev) if prev.source == Source::Preview && prev.ident == ident => prev.rev + 1,
            _ => 0,
        };
        entry.failed = None;
        entry.art = Some(ArtData {
            source: Source::Preview,
            ident,
            rev,
            content_type,
            bytes: Some(bytes.clone().into()),
            last_used: Instant::now(),
        });
        inner.bytes_total += bytes.len();
        drop(inner);
        tracing::info!(hash, "media preview extracted");
        self.evict();
    }
    fn fail(&self, hash: &str, failure: Failure) {
        let mut inner = self.inner.lock();
        let Some(entry) = inner.entries.get_mut(hash) else {
            return;
        };
        // An existing image is never dropped by a later failed refresh.
        if entry.art.is_none() {
            entry.failed = Some((failure, Instant::now() + failure.cooldown()));
            // "No completed media file" is the normal state for downloads in
            // flight (retried every 60 s) — keep it quiet; the rest is worth
            // surfacing on the log pages.
            match failure {
                Failure::NoCompletedMedia => {
                    tracing::debug!(hash, ?failure, "no torrent artwork");
                }
                Failure::Error | Failure::NoFfmpeg => {
                    tracing::info!(hash, ?failure, "no torrent artwork");
                }
            }
        }
    }

    /// Drop the oldest cached bytes when over budget. Evicted entries keep
    /// their metadata (with a bumped rev) so the next decorated payload
    /// triggers a refetch, which re-downloads or re-extracts.
    fn evict(&self) {
        let mut inner = self.inner.lock();
        loop {
            let with_bytes = inner
                .entries
                .values()
                .filter(|e| e.art.as_ref().is_some_and(|a| a.bytes.is_some()))
                .count();
            if inner.bytes_total <= MAX_CACHED_BYTES && with_bytes <= MAX_CACHED_ENTRIES {
                break;
            }
            let victim = inner
                .entries
                .iter()
                .filter(|(_, e)| e.art.as_ref().is_some_and(|a| a.bytes.is_some()))
                .min_by_key(|(_, e)| {
                    e.art
                        .as_ref()
                        .map(|a| a.last_used)
                        .unwrap_or_else(Instant::now)
                })
                .map(|(hash, _)| hash.clone());
            let Some(victim) = victim else {
                break;
            };
            let Some(entry) = inner.entries.get_mut(&victim) else {
                break;
            };
            let Some(art) = entry.art.as_mut() else {
                break;
            };
            let dropped = art.bytes.take().map(|b| b.len()).unwrap_or(0);
            art.rev += 1;
            inner.bytes_total = inner.bytes_total.saturating_sub(dropped);
        }
    }
}

#[derive(Deserialize)]
struct MyShowsRpc {
    #[serde(default)]
    result: Option<Vec<MyShowsShow>>,
}

#[derive(Deserialize)]
struct MyShowsShow {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    image: Option<String>,
}

/// Download an image and detect its content type.
async fn fetch_image_with(
    client: &reqwest::Client,
    url: &str,
) -> Result<(Vec<u8>, &'static str), String> {
    let resp = client
        .get(url)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?.to_vec();
    detect_image(&bytes)
        .map(|content_type| (bytes, content_type))
        .ok_or_else(|| "unsupported image format".to_owned())
}

/// Sniff PNG / JPEG / WebP magic bytes (the formats the UI's decoder covers).
fn detect_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Extract one frame with the system ffmpeg: seek [`PREVIEW_SEEK_SECS`] in
/// (past the intro — episodes of one show share it, and a fixed early
/// timestamp made all their artwork identical), first frame, scaled to
/// 512px wide, PNG on stdout. Very short files (nothing at the seek point)
/// retry from the start.
async fn extract_frame(path: &Path) -> Result<(Vec<u8>, &'static str), String> {
    match extract_frame_once(path, true).await {
        Ok(out) => Ok(out),
        Err(e) if e == "empty output" => extract_frame_once(path, false).await,
        Err(e) => Err(e),
    }
}

async fn extract_frame_once(path: &Path, seek: bool) -> Result<(Vec<u8>, &'static str), String> {
    use std::process::Stdio;

    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    if seek {
        cmd.args(["-ss", &PREVIEW_SEEK_SECS.to_string()]);
    }
    cmd.arg("-i")
        .arg(path)
        .args(["-frames:v", "1", "-vf", "scale=512:-2"])
        .args(["-f", "image2pipe", "-c:v", "png", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "ffmpeg not found".to_owned()
        } else {
            e.to_string()
        }
    })?;
    let output = tokio::time::timeout(EXTRACT_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "ffmpeg timed out".to_owned())?
        .map_err(|e| e.to_string())?;
    if output.stdout.is_empty() {
        return Err("empty output".to_owned());
    }
    if !output.status.success() {
        return Err(format!("ffmpeg exit {:?}", output.status.code()));
    }
    detect_image(&output.stdout)
        .map(|content_type| (output.stdout, content_type))
        .ok_or_else(|| "ffmpeg produced no image".to_owned())
}

fn is_video_file(name: &str) -> bool {
    mime_guess::from_path(name)
        .first()
        .map(|mime| mime.type_() == mime_guess::mime::VIDEO)
        .unwrap_or(false)
}

/// The absolute on-disk path of a torrent file: the details payload's
/// `output_folder` already includes the extraction subfolder (same rule as
/// the desktop's `torrent_file_disk_path`).
fn disk_path(output_folder: &str, components: &[String]) -> Option<PathBuf> {
    if output_folder.is_empty() {
        return None;
    }
    let mut path = PathBuf::from(output_folder);
    for component in components {
        path.push(component);
    }
    Some(path)
}

/// Turn a release name ("The.Last.of.Us.S02E03.1080p.WEB.h264-KOGI") into a
/// search title ("The Last of Us"). Heuristic: cut the token list at the
/// first season/episode marker, quality tag, release-group separator or
/// year; the rest becomes the title.
fn clean_title(name: &str) -> String {
    name.split(['.', '_', ' '])
        .filter(|t| !t.is_empty())
        .take_while(|token| !is_release_marker(token))
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_release_marker(token: &str) -> bool {
    let token = token.to_lowercase();
    // Check the token and the part before a release-group '-' separator
    // ("h264-KOGI" is a marker too).
    for candidate in [token.as_str(), token.split('-').next().unwrap_or(&token)] {
        if is_release_marker_token(candidate) {
            return true;
        }
    }
    false
}

fn is_release_marker_token(token: &str) -> bool {
    let bytes = token.as_bytes();
    // S01E02 style season/episode markers, and bare season packs (S01,
    // "Season.1") — a plain season marker must cut too, or the search
    // carries the season tag and matches nothing.
    if bytes.len() >= 3 && bytes[0] == b's' {
        let mut i = 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i > 1 {
            if i < bytes.len() && bytes[i] == b'e' {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > i + 1 {
                    return true;
                }
            }
            if i == bytes.len() {
                return true; // bare "s01"
            }
        }
    }
    // Quality tags: 720p / 1080p / 2160p.
    if bytes.len() >= 4
        && bytes[bytes.len() - 1] == b'p'
        && bytes[..bytes.len() - 1].iter().all(|b| b.is_ascii_digit())
    {
        return true;
    }
    // Release years.
    if bytes.len() == 4 && bytes.iter().all(|b| b.is_ascii_digit()) {
        let year: u32 = token.parse().unwrap_or(0);
        if (1900..=2100).contains(&year) {
            return true;
        }
    }
    matches!(
        token,
        "web"
            | "webdl"
            | "webrip"
            | "bluray"
            | "bdrip"
            | "brrip"
            | "hdtv"
            | "dvdrip"
            | "hdrip"
            | "x264"
            | "x265"
            | "h264"
            | "h265"
            | "hevc"
            | "xvid"
            | "aac"
            | "ac3"
            | "eac3"
            | "ddp"
            | "dd5"
            | "truehd"
            | "atmos"
            | "proper"
            | "repack"
            | "extended"
            | "remastered"
            | "season"
            | "complete"
            | "hdr"
            | "10bit"
            | "remux"
            | "amzn"
            | "nf"
            | "dsnp"
            | "hmax"
            | "1080i"
    )
}

/// Tiny FNV-1a, for stable-within-session idents derived from URLs.
fn short_hash(s: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_title_cuts_at_episode_marker() {
        assert_eq!(
            clean_title("The.Last.of.Us.S02E03.1080p.WEB.h264-KOGI"),
            "The Last of Us"
        );
    }

    #[test]
    fn clean_title_cuts_at_season_and_quality() {
        assert_eq!(
            clean_title("Arcane.Season.2.Complete.2160p.NF.WEB-DL.DDP5.1.Atmos.HDR.H.265"),
            "Arcane"
        );
        assert_eq!(clean_title("Dune.2021.1080p.BluRay.x265"), "Dune");
        // Season packs: a bare "S01" must cut too, or the search carries it.
        assert_eq!(
            clean_title("Severance.S01.1080p.WEB.h264-GROUP"),
            "Severance"
        );
    }

    #[test]
    fn clean_title_handles_plain_names() {
        assert_eq!(clean_title("Some Show Name"), "Some Show Name");
        assert_eq!(clean_title(""), "");
    }

    #[test]
    fn detect_image_formats() {
        assert_eq!(detect_image(&[0x89, b'P', b'N', b'G']), Some("image/png"));
        assert_eq!(detect_image(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(detect_image(b"RIFF1234WEBPVP8 "), Some("image/webp"));
        assert_eq!(detect_image(b"<html>"), None);
    }
}
