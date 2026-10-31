//! Per-torrent artwork: the poster shown behind the detail panel's Overview
//! tab and in the torrent list rows' corner.
//!
//! Sources per torrent, in priority order (the chain stops at the first
//! source that produces an image):
//! 1. Hook poster — an Image-kind custom column keyed `poster` whose value
//!    is an image URL (see `columns.rs`).
//! 2. MyShows poster — the torrent's release name is cleaned into a show
//!    title and searched on api.myshows.me (JSON-RPC); the first result
//!    carrying a poster wins. Cached per cleaned title.
//! 3. Media preview — the largest completed video file's frame, extracted
//!    with the system `ffmpeg` (an absent ffmpeg just disables this source).
//!
//! Every image a torrent ever resolved is kept as a *candidate* (at most
//! one per source, ordered by priority): when a higher-priority source
//! appears later (the columns snapshot — and its poster — can arrive after
//! the chain settled on a fallback), it is added alongside the old one, so
//! the UI can fall back to it when the primary's bytes fail to load.
//!
//! Resolution runs in one background worker ([`ArtworkProvider::run`]);
//! decoration (`decorate_list`/`decorate_details`) only kicks the worker and
//! attaches what is already ready, so the frequent UI polls never block.
//! Bytes are served by the artwork route in `media.rs`, keyed by info-hash.
//! Preview frames are extracted lazily — advertised as soon as the daemon
//! knows a completed video file exists, but only decoded when the route is
//! first asked for the torrent.
//!
//! Bytes are additionally persisted in a **disk cache**
//! ([`ArtworkOptions::disk_cache`], one `{source}#{ident}.bin` file per
//! candidate under the torrent's info-hash directory): memory evictions and
//! restarts re-load them from disk instead of re-downloading or
//! re-extracting. The disk cache is capped and LRU-evicted by file mtime.
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

/// Cap on the persisted disk cache (LRU-evicted by file mtime beyond this).
const MAX_DISK_CACHE_BYTES: u64 = 256 * 1024 * 1024;

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

/// Which artwork sources run. The hook poster is authoritative by
/// construction (it is the user's own hook service answering); the MyShows
/// search and the ffmpeg preview are best-effort built-ins that can each be
/// turned off.
#[derive(Clone, Debug, Default)]
pub struct ArtworkOptions {
    /// Hook-provided posters (an Image column keyed `poster`).
    pub posters: bool,
    /// The built-in MyShows poster search (a subset of `posters`; off when
    /// `posters` is off).
    pub myshows: bool,
    /// Media-file preview frames (ffmpeg).
    pub previews: bool,
    /// Directory of the persistent artwork cache. `None` keeps artwork in
    /// memory only (re-downloaded / re-extracted after a restart).
    pub disk_cache: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
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

/// Why the last resolution attempt for a source produced nothing.
#[derive(Debug, Clone, Copy)]
enum Failure {
    NoCompletedMedia,
    /// A hook-poster URL was advertised but its download failed — retried
    /// often (the hook may recover).
    HookUrl,
    /// The MyShows search answered but carried nothing usable.
    NotFound,
    Error,
    NoFfmpeg,
}

impl Failure {
    fn cooldown(self) -> Duration {
        match self {
            Failure::NoCompletedMedia | Failure::HookUrl => COOLDOWN_RETRYABLE,
            Failure::NotFound | Failure::Error => COOLDOWN_ERROR,
            Failure::NoFfmpeg => COOLDOWN_NO_FFMPEG,
        }
    }
}

/// Ready (or advertised) artwork for one torrent from one source.
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
    /// after an eviction (the next resolve re-derives the bytes — from the
    /// disk cache when one is configured).
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
    /// Every resolved candidate, best source first. At most one per source.
    candidates: Vec<ArtData>,
    /// Per-source last failure and when the worker may retry. A source with
    /// a live candidate is never retried regardless of cooldowns.
    failed: HashMap<Source, (Failure, Instant)>,
}

impl Entry {
    fn candidate(&self, source: Source) -> Option<&ArtData> {
        self.candidates.iter().find(|c| c.source == source)
    }

    /// The source still owes bytes: no candidate yet, or a candidate whose
    /// bytes were never filled (an advertised preview) or were evicted.
    fn pending(&self, source: Source) -> bool {
        match self.candidate(source) {
            None => true,
            Some(c) => c.bytes.is_none(),
        }
    }

    fn cooling(&self, source: Source) -> bool {
        self.failed
            .get(&source)
            .is_some_and(|(_, until)| Instant::now() < *until)
    }
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
    opts: ArtworkOptions,
    client: reqwest::Client,
    inner: Mutex<Inner>,
    wake: tokio::sync::Notify,
}

impl ArtworkProvider {
    pub fn new(
        api: ApiSlot,
        columns: Option<Arc<ColumnsProvider>>,
        opts: ArtworkOptions,
    ) -> Arc<Self> {
        Arc::new(Self {
            api,
            columns,
            opts,
            client: reqwest::Client::new(),
            inner: Mutex::new(Inner::default()),
            wake: tokio::sync::Notify::new(),
        })
    }

    /// Spawn the worker. With every source disabled the worker just idles.
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
        let candidates = self.ready_artwork_all(&hash);
        details.artwork = candidates.first().cloned();
        details.artwork_candidates = candidates;
    }

    /// Queue the torrent for resolution while the chain still has work: a
    /// source with no bytes yet (nothing resolved, an advertised preview, or
    /// evicted bytes) that isn't cooling down — or, when a lower-priority
    /// candidate already exists, a hook poster that just appeared (the
    /// upgrade; the old artwork stays as a fallback until the new one is
    /// ready).
    fn kick(&self, hash: &str) {
        let mut queued = false;
        {
            let mut inner = self.inner.lock();
            let entry = inner.entries.entry(hash.to_owned()).or_default();
            let pending = |source| entry.pending(source);
            let cooling = |source| entry.cooling(source);
            let hook_available = self.opts.posters && self.hook_poster(hash).is_some();
            let chain_work = (self.opts.posters && pending(Source::Hook) && hook_available)
                || (self.myshows_active() && pending(Source::MyShows))
                || (self.opts.previews && pending(Source::Preview));

            // Upgrade: a hook poster exists but the best candidate came from
            // a lower-priority source — the columns snapshot (and its
            // poster) can arrive after the background chain already settled
            // on a fallback, and without this the fallback would stick
            // forever. Runs on the interactive lane; a cooling hook URL
            // retries at its failure's cadence instead.
            let upgrade = match entry.candidates.first() {
                Some(best) => {
                    best.source != Source::Hook
                        && hook_available
                        && pending(Source::Hook)
                        && !cooling(Source::Hook)
                }
                None => false,
            };

            // Otherwise: only an untouched chain (no candidates yet) is
            // worth a background pass. A chain that already produced a
            // candidate must not re-run for the lower-priority sources it
            // skipped — the chain stops at the first success, so the pass
            // would just repeat the same failures. Candidates whose bytes
            // were evicted (or never filled) are refilled by the
            // interactive resolve the artwork route triggers.
            let background = entry.candidates.is_empty() && chain_work;
            let should = upgrade || background;
            if should && inner.queued.insert(hash.to_owned()) {
                if upgrade {
                    inner.interactive.push_back(hash.to_owned());
                } else {
                    inner.background.push_back(hash.to_owned());
                }
                queued = true;
            }
        }
        if queued {
            self.wake.notify_one();
        }
    }

    fn ready_artwork_all(&self, hash: &str) -> Vec<TorrentArtwork> {
        let inner = self.inner.lock();
        inner
            .entries
            .get(hash)
            .map(|e| {
                e.candidates
                    .iter()
                    .map(|art| TorrentArtwork {
                        key: art.key(),
                        path: format!("{ARTWORK_PATH_PREFIX}/{hash}/artwork"),
                        source: art.source.as_str().to_owned(),
                    })
                    .collect()
            })
            .unwrap_or_default()
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

    /// Bytes of the first candidate that has any — the route serves one
    /// image per torrent; while the primary's bytes are being re-derived
    /// (advertised preview, eviction), a lower candidate's bytes are better
    /// than a 404.
    fn ready_bytes(&self, hash: &str) -> Option<(Arc<[u8]>, &'static str)> {
        let mut inner = self.inner.lock();
        let entry = inner.entries.get_mut(hash)?;
        let candidate = entry.candidates.iter_mut().find(|c| c.bytes.is_some())?;
        candidate.last_used = Instant::now();
        let bytes = candidate.bytes.clone()?;
        Some((bytes, candidate.content_type))
    }

    // --- resolution (worker only) ---

    /// The chain: hook poster, then MyShows, then the media preview. Stops
    /// at the first source that produces an image; sources that already have
    /// one (or are cooling down) are skipped, so a later pass only ever adds
    /// a candidate (the hook-poster upgrade) or refills evicted bytes.
    async fn resolve(&self, hash: &str, _interactive: bool) {
        let Some(torrent) = self.load_torrent(hash) else {
            tracing::debug!(hash, "torrent gone before artwork resolution");
            return;
        };
        let name = torrent.name.clone().unwrap_or_default();

        // Hook-provided poster (the columns API's Image column keyed
        // `poster` — e.g. moviesync-daemon publishes its resolve table's
        // poster there): authoritative by construction, so a hit ends the
        // chain here.
        if self.opts.posters
            && self.entry_pending(hash, Source::Hook)
            && let Some(url) = self.hook_poster(hash)
        {
            let ident = short_hash(&url);
            if let Some((bytes, content_type)) = self.disk_load(hash, Source::Hook, &ident).await {
                self.ready(hash, Source::Hook, ident, bytes, content_type, false);
                return;
            }
            match fetch_image_with(&self.client, &url).await {
                Ok((bytes, content_type)) => {
                    self.ready(hash, Source::Hook, ident, bytes.into(), content_type, true);
                    return;
                }
                Err(e) => {
                    tracing::info!(hash, url = %url, error = %e, "hook poster fetch failed");
                    self.fail(hash, Source::Hook, Failure::HookUrl);
                }
            }
        }

        // The built-in MyShows search is the fallback when no hook supplied
        // a poster. It only accepts an exact title match (original or
        // localized, compared case/punctuation-insensitively): MyShows'
        // fuzzy ranking returns unrelated shows for hard names
        // ("…First Marriage" → "Marriage Toxin"), and a wrong poster is
        // worse than none — the hook's resolve table is the recall path.
        if self.myshows_active() && self.entry_pending(hash, Source::MyShows) && !name.is_empty() {
            match self.myshows_poster(&name).await {
                Ok(Some((show_id, url))) => {
                    if let Some((bytes, content_type)) =
                        self.disk_load(hash, Source::MyShows, &show_id).await
                    {
                        self.ready(hash, Source::MyShows, show_id, bytes, content_type, false);
                        return;
                    }
                    match fetch_image_with(&self.client, &url).await {
                        Ok((bytes, content_type)) => {
                            self.ready(
                                hash,
                                Source::MyShows,
                                show_id,
                                bytes.into(),
                                content_type,
                                true,
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::info!(hash, url = %url, error = %e, "myshows poster fetch failed");
                            self.fail(hash, Source::MyShows, Failure::Error);
                        }
                    }
                }
                Ok(None) => {
                    self.fail(hash, Source::MyShows, Failure::NotFound);
                }
                Err(e) => {
                    // Visible on the log pages: this is the main reason a
                    // downloading torrent has no poster yet.
                    tracing::info!(hash, error = %e, "myshows search failed");
                    self.fail(hash, Source::MyShows, Failure::Error);
                }
            }
        }

        // The preview stage at the end of the chain.
        if self.opts.previews && self.entry_pending(hash, Source::Preview) {
            match self.preview_candidate(&torrent) {
                Ok(Some((file_idx, path))) => {
                    let ident = format!("f{file_idx}");
                    // A frame extracted by a previous run (or an earlier
                    // eviction) is served from the disk cache — no ffmpeg.
                    if let Some((bytes, content_type)) =
                        self.disk_load(hash, Source::Preview, &ident).await
                    {
                        self.ready(hash, Source::Preview, ident, bytes, content_type, false);
                        return;
                    }
                    self.advertise_preview(hash, file_idx);
                    match extract_frame(&path).await {
                        Ok((bytes, content_type)) => {
                            self.fill_preview_bytes(hash, file_idx, bytes, content_type)
                        }
                        Err(e) => {
                            tracing::debug!(hash, error = %e, "preview extraction failed");
                            // The advertised-but-empty candidate must not
                            // stick around advertising bytes that never come.
                            self.drop_candidate(hash, Source::Preview);
                            if e == "ffmpeg not found" {
                                self.fail(hash, Source::Preview, Failure::NoFfmpeg);
                            } else {
                                self.fail(hash, Source::Preview, Failure::Error);
                            }
                        }
                    }
                }
                Ok(None) => self.fail(hash, Source::Preview, Failure::NoCompletedMedia),
                Err(e) => {
                    tracing::debug!(hash, error = %e, "no preview candidate");
                    self.fail(hash, Source::Preview, Failure::Error);
                }
            }
        }
    }

    /// Whether the given source still owes bytes and isn't cooling down.
    fn entry_pending(&self, hash: &str, source: Source) -> bool {
        let inner = self.inner.lock();
        inner
            .entries
            .get(hash)
            .is_none_or(|e| e.pending(source) && !e.cooling(source))
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

    /// The built-in MyShows search is a poster source: it runs only while
    /// posters as a whole are enabled.
    fn myshows_active(&self) -> bool {
        self.opts.posters && self.opts.myshows
    }

    /// The hook's poster URL for this torrent: an Image-kind column keyed
    /// `poster` with an `http(s)://` value (already validated in
    /// `columns.rs`'s sanitization).
    fn hook_poster(&self, hash: &str) -> Option<String> {
        if !self.opts.posters {
            return None;
        }
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
    /// `(show id, poster url)`; `Ok(None)` = searched, nothing usable. Only
    /// a result whose title — original or localized — equals the cleaned
    /// name is accepted: MyShows' fuzzy ranking returns unrelated shows for
    /// hard names ("…First Marriage" → "Marriage Toxin"), and a wrong poster
    /// is worse than none. The hook's resolve table is the recall path for
    /// names no exact search can match.
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
        let results = rpc.result.unwrap_or_default();
        let norm = normalize_alnum(title);
        let exact = |s: &MyShowsShow| {
            normalize_alnum(&s.title) == norm
                || s.title_original
                    .as_deref()
                    .is_some_and(|t| normalize_alnum(t) == norm)
        };
        let show = results
            .iter()
            .find(|s| exact(s) && s.image.as_deref().is_some_and(|u| u.starts_with("http")));
        Ok(show.map(|s| {
            (
                format!("s{}", s.id),
                s.image.clone().expect("filtered above"),
            )
        }))
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

    /// Record a resolved candidate: replaces any other candidate from the
    /// same source (a changed hook URL or a different MyShows match), keeps
    /// the rest as fallbacks, ordered hook → MyShows → preview.
    fn ready(
        &self,
        hash: &str,
        source: Source,
        ident: String,
        bytes: Arc<[u8]>,
        content_type: &'static str,
        persist: bool,
    ) {
        let mut inner = self.inner.lock();
        let entry = inner.entries.entry(hash.to_owned()).or_default();
        let rev = match entry.candidate(source) {
            Some(prev) if prev.ident == ident => prev.rev + 1,
            _ => 0,
        };
        entry.candidates.retain(|c| c.source != source);
        let art = ArtData {
            source,
            ident: ident.clone(),
            rev,
            content_type,
            bytes: Some(bytes.clone()),
            last_used: Instant::now(),
        };
        let ix = entry
            .candidates
            .partition_point(|c| priority(c.source) <= priority(source));
        entry.candidates.insert(ix, art);
        entry.failed.remove(&source);
        inner.bytes_total += bytes.len();
        let disk = self.opts.disk_cache.clone();
        let hash = hash.to_owned();
        drop(inner);
        tracing::info!(hash, source = source.as_str(), "torrent artwork ready");
        if persist && let Some(dir) = disk {
            tokio::task::spawn_blocking(move || {
                disk_store(&dir, &hash, source, &ident, bytes);
            });
        }
        self.evict();
    }

    /// Advertise the preview source (bytes not yet extracted) so the UIs
    /// learn artwork is coming; the route request triggers the extraction.
    /// Never downgrades a preview candidate whose bytes are already cached.
    fn advertise_preview(&self, hash: &str, file_idx: usize) {
        let mut inner = self.inner.lock();
        let entry = inner.entries.entry(hash.to_owned()).or_default();
        if let Some(prev) = entry.candidate(Source::Preview) {
            if prev.bytes.is_some() {
                return;
            }
            // Already advertised for the same file — nothing to add.
            if prev.ident == format!("f{file_idx}") {
                return;
            }
        }
        let ident = format!("f{file_idx}");
        let rev = match entry.candidate(Source::Preview) {
            Some(prev) if prev.ident == ident => prev.rev,
            _ => 0,
        };
        entry.candidates.retain(|c| c.source != Source::Preview);
        let art = ArtData {
            source: Source::Preview,
            ident,
            rev,
            content_type: "image/png",
            bytes: None,
            last_used: Instant::now(),
        };
        let ix = entry
            .candidates
            .partition_point(|c| priority(c.source) <= priority(Source::Preview));
        entry.candidates.insert(ix, art);
    }

    fn fill_preview_bytes(
        &self,
        hash: &str,
        file_idx: usize,
        bytes: Vec<u8>,
        content_type: &'static str,
    ) {
        let ident = format!("f{file_idx}");
        let bytes: Arc<[u8]> = bytes.into();
        let mut inner = self.inner.lock();
        let Some(entry) = inner.entries.get_mut(hash) else {
            return;
        };
        // Only fill the advertised candidate (or an evicted one) — never
        // replace live bytes.
        if let Some(prev) = entry.candidate(Source::Preview)
            && prev.bytes.is_some()
        {
            return;
        }
        let rev = match entry.candidate(Source::Preview) {
            Some(prev) if prev.ident == ident => prev.rev + 1,
            _ => 0,
        };
        entry.candidates.retain(|c| c.source != Source::Preview);
        let art = ArtData {
            source: Source::Preview,
            ident: ident.clone(),
            rev,
            content_type,
            bytes: Some(bytes.clone()),
            last_used: Instant::now(),
        };
        let ix = entry
            .candidates
            .partition_point(|c| priority(c.source) <= priority(Source::Preview));
        entry.candidates.insert(ix, art);
        entry.failed.remove(&Source::Preview);
        inner.bytes_total += bytes.len();
        let disk = self.opts.disk_cache.clone();
        let hash = hash.to_owned();
        drop(inner);
        tracing::info!(hash, "media preview extracted");
        if let Some(dir) = disk {
            tokio::task::spawn_blocking(move || {
                disk_store(&dir, &hash, Source::Preview, &ident, bytes);
            });
        }
        self.evict();
    }

    /// Remove a candidate (used for the advertised-but-empty preview when
    /// its extraction failed).
    fn drop_candidate(&self, hash: &str, source: Source) {
        let mut inner = self.inner.lock();
        if let Some(entry) = inner.entries.get_mut(hash) {
            entry.candidates.retain(|c| c.source != source);
        }
    }

    fn fail(&self, hash: &str, source: Source, failure: Failure) {
        let mut inner = self.inner.lock();
        let Some(entry) = inner.entries.get_mut(hash) else {
            return;
        };
        entry
            .failed
            .insert(source, (failure, Instant::now() + failure.cooldown()));
        // "No completed media file" is the normal state for downloads in
        // flight (retried every 60 s) — keep it quiet; the rest is worth
        // surfacing on the log pages.
        match failure {
            Failure::NoCompletedMedia | Failure::HookUrl | Failure::NotFound => {
                tracing::debug!(
                    hash,
                    source = source.as_str(),
                    ?failure,
                    "no torrent artwork"
                );
            }
            Failure::Error | Failure::NoFfmpeg => {
                tracing::info!(
                    hash,
                    source = source.as_str(),
                    ?failure,
                    "no torrent artwork"
                );
            }
        }
    }

    /// Drop the oldest cached bytes when over budget. Evicted candidates
    /// keep their metadata (with a bumped rev) so the next decorated payload
    /// triggers a refetch, which re-derives the bytes — from the disk cache
    /// when one is configured.
    fn evict(&self) {
        let mut inner = self.inner.lock();
        loop {
            let with_bytes = inner
                .entries
                .values()
                .filter(|e| e.candidates.iter().any(|c| c.bytes.is_some()))
                .count();
            if inner.bytes_total <= MAX_CACHED_BYTES && with_bytes <= MAX_CACHED_ENTRIES {
                break;
            }
            // The globally oldest candidate-with-bytes, identified by its
            // owning torrent — two torrents can share a source+ident (the
            // same hook poster URL), so the hash must be part of the key.
            let victim = inner
                .entries
                .iter()
                .filter_map(|(hash, e)| {
                    e.candidates
                        .iter()
                        .filter(|c| c.bytes.is_some())
                        .min_by_key(|c| c.last_used)
                        .map(|c| (hash.clone(), c.source, c.ident.clone(), c.last_used))
                })
                .min_by_key(|(_, _, _, last_used)| *last_used);
            let Some((hash, source, ident, _)) = victim else {
                break;
            };
            let Some(entry) = inner.entries.get_mut(&hash) else {
                break;
            };
            let Some(art) = entry
                .candidates
                .iter_mut()
                .find(|c| c.source == source && c.ident == ident && c.bytes.is_some())
            else {
                break;
            };
            let dropped = art.bytes.take().map(|b| b.len()).unwrap_or(0);
            art.rev += 1;
            inner.bytes_total = inner.bytes_total.saturating_sub(dropped);
        }
    }

    // --- disk cache ---

    /// Load a candidate's bytes from the persistent disk cache. `None` when
    /// no cache is configured, the file is absent, or the bytes don't sniff
    /// as a known image format.
    async fn disk_load(
        &self,
        hash: &str,
        source: Source,
        ident: &str,
    ) -> Option<(Arc<[u8]>, &'static str)> {
        let dir = self.opts.disk_cache.as_ref()?;
        let path = cache_file(dir, hash, source, ident);
        tokio::task::spawn_blocking(move || {
            let bytes = std::fs::read(path).ok()?;
            detect_image(&bytes).map(|ct| (Arc::from(bytes.as_slice()) as Arc<[u8]>, ct))
        })
        .await
        .ok()
        .flatten()
    }
}

/// Candidate ordering: hook posters first, then MyShows, then previews.
fn priority(source: Source) -> u8 {
    match source {
        Source::Hook => 0,
        Source::MyShows => 1,
        Source::Preview => 2,
    }
}

/// Filename of one cached artwork candidate under the torrent's info-hash
/// directory. Idents are filesystem-safe by construction (a hex hash, an
/// `s{id}` show id, an `f{idx}` file index) and the info hash is validated
/// to keep path traversal out even if a peer name ever leaked in.
fn cache_file(dir: &Path, hash: &str, source: Source, ident: &str) -> PathBuf {
    dir.join(hash)
        .join(format!("{}#{ident}.bin", source.as_str()))
}

fn valid_cache_hash(hash: &str) -> bool {
    !hash.is_empty() && hash.len() <= 64 && hash.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Persist one candidate's bytes, then shrink the cache back under its cap
/// by deleting the oldest files. Best-effort: failures are logged and the
/// in-memory copy is unaffected.
fn disk_store(dir: &Path, hash: &str, source: Source, ident: &str, bytes: Arc<[u8]>) {
    if !valid_cache_hash(hash) {
        return;
    }
    let path = cache_file(dir, hash, source, ident);
    let Some(parent) = path.parent() else {
        return;
    };
    if let Err(e) = std::fs::create_dir_all(parent) {
        tracing::warn!(error = %e, path = %parent.display(), "artwork disk cache mkdir failed");
        return;
    }
    let tmp = path.with_extension("tmp");
    let write = std::fs::write(&tmp, bytes.as_ref()).and_then(|_| std::fs::rename(&tmp, &path));
    match write {
        Ok(()) => disk_evict(dir),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "artwork disk cache write failed");
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Delete the oldest cached files until the cache is back under
/// [`MAX_DISK_CACHE_BYTES`].
fn disk_evict(dir: &Path) {
    let Ok(hash_dirs) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(PathBuf, u64, Option<std::time::SystemTime>)> = Vec::new();
    for entry in hash_dirs.flatten() {
        let Ok(candidates) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for file in candidates.flatten() {
            let path = file.path();
            if path.extension().is_some_and(|e| e == "bin")
                && let Ok(meta) = file.metadata()
            {
                files.push((path, meta.len(), meta.modified().ok()));
            }
        }
    }
    let total: u64 = files.iter().map(|(_, size, _)| size).sum();
    if total <= MAX_DISK_CACHE_BYTES {
        return;
    }
    // Oldest first; files without an mtime (unexpected) go first too.
    files.sort_by_key(|(_, _, mtime)| *mtime);
    let mut total = total;
    for (path, size, _) in files {
        if total <= MAX_DISK_CACHE_BYTES {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
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
    title: String,
    /// The original (usually English) title — the localized `title` often
    /// differs, and the exact-match check compares both. The API spells the
    /// field camelCase; without the rename it always deserialized empty and
    /// the exact match degraded to comparing localized titles only.
    #[serde(rename = "titleOriginal", default)]
    title_original: Option<String>,
    #[serde(default)]
    image: Option<String>,
}

/// Case- and punctuation-insensitive title comparison, matching
/// moviesync-daemon's `normalize_title` (lowercase, alphanumeric only) so
/// both sides agree on what "the same title" means.
fn normalize_alnum(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
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

    /// The exact-match filter depends on `titleOriginal`: MyShows spells it
    /// camelCase, and a serde field-name mismatch silently deserialized it
    /// empty — leaving localized titles as the only comparison, which
    /// matched almost no release name ("only Newlyweds shows a poster").
    #[test]
    fn myshows_show_parses_camel_case_original_title() {
        let show: MyShowsShow = serde_json::from_str(
            r#"{"id":67366,"title":"Тед Лассо","titleOriginal":"Ted Lasso",
                "image":"https://media.myshows.me/shows/normal/x.jpg"}"#,
        )
        .unwrap();
        assert_eq!(show.title, "Тед Лассо");
        assert_eq!(show.title_original.as_deref(), Some("Ted Lasso"));
        assert_eq!(
            normalize_alnum(show.title_original.as_deref().unwrap()),
            "tedlasso"
        );
    }

    /// The exact-match filter accepts the original or the localized title,
    /// and rejects unrelated shows.
    #[test]
    fn exact_title_check() {
        let ted = MyShowsShow {
            id: 67366,
            title: "Тед Лассо".into(),
            title_original: Some("Ted Lasso".into()),
            image: Some("https://x.jpg".into()),
        };
        assert!(
            normalize_alnum(&ted.title) == normalize_alnum("Ted Lasso") || {
                ted.title_original
                    .as_deref()
                    .is_some_and(|t| normalize_alnum(t) == normalize_alnum("Ted Lasso"))
            }
        );
        let toxin = MyShowsShow {
            id: 98262,
            title: "Брачный токсин".into(),
            title_original: Some("Marriage Toxin".into()),
            image: Some("https://x.jpg".into()),
        };
        let norm = normalize_alnum("Georgie and Mandys First Marriage");
        assert!(normalize_alnum(&toxin.title) != norm);
        assert!(
            !toxin
                .title_original
                .as_deref()
                .is_some_and(|t| normalize_alnum(t) == norm)
        );
    }

    #[test]
    fn cache_file_layout_is_two_levels_and_safe() {
        let dir = Path::new("/cache/artwork");
        let path = cache_file(dir, "ab12", Source::Hook, "0123abcd");
        assert_eq!(path, Path::new("/cache/artwork/ab12/hook#0123abcd.bin"));
        assert!(valid_cache_hash("ab12"));
        assert!(valid_cache_hash(&"x".repeat(64)));
        assert!(!valid_cache_hash(""));
        assert!(!valid_cache_hash("../../etc"));
        assert!(!valid_cache_hash(&"x".repeat(65)));
    }

    #[test]
    fn disk_store_roundtrip_and_eviction() {
        let scratch = std::env::temp_dir().join(format!(
            "rqbit-artwork-disk-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let png_bytes: Arc<[u8]> = vec![0x89u8, b'P', b'N', b'G', 0, 0].into();

        disk_store(&scratch, "ab12", Source::Hook, "1234", png_bytes.clone());
        let path = cache_file(&scratch, "ab12", Source::Hook, "1234");
        assert!(path.exists());
        let read = std::fs::read(&path).unwrap();
        assert_eq!(read, png_bytes.as_ref());
        // The temp file is gone after the rename.
        assert!(!path.with_extension("tmp").exists());

        // Invalid hashes are refused (no traversal out of the cache dir).
        disk_store(&scratch, "../../evil", Source::Hook, "1", png_bytes.clone());
        assert!(!scratch.parent().unwrap().join("evil").exists());

        std::fs::remove_dir_all(&scratch).ok();
    }

    /// Candidate ordering: the same source replaces its own candidate,
    /// everything else lands in priority order (hook → myshows → preview).
    #[test]
    fn candidates_keep_priority_order() {
        let make = |source, ident: &str| ArtData {
            source,
            ident: ident.to_owned(),
            rev: 0,
            content_type: "image/png",
            bytes: Some(vec![0x89, b'P', b'N', b'G', 0].into()),
            last_used: Instant::now(),
        };
        let mut candidates: Vec<ArtData> = Vec::new();
        for art in [
            make(Source::Preview, "f0"),
            make(Source::MyShows, "s1"),
            make(Source::Hook, "aaaa"),
        ] {
            let ix = candidates.partition_point(|c| priority(c.source) <= priority(art.source));
            candidates.insert(ix, art);
        }
        assert_eq!(
            candidates.iter().map(|c| c.source).collect::<Vec<_>>(),
            vec![Source::Hook, Source::MyShows, Source::Preview]
        );
        // A new hook ident replaces the old hook candidate, order intact.
        let replacement = make(Source::Hook, "bbbb");
        candidates.retain(|c| c.source != Source::Hook);
        let ix = candidates.partition_point(|c| priority(c.source) <= priority(replacement.source));
        candidates.insert(ix, replacement);
        assert_eq!(
            candidates.iter().map(|c| c.source).collect::<Vec<_>>(),
            vec![Source::Hook, Source::MyShows, Source::Preview]
        );
        assert_eq!(candidates[0].ident, "bbbb");
    }
}
