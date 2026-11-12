//! Hook dispatcher: routes torrent lifecycle events to configured hooks and
//! watches the session for torrent completion.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use librqbit::{
    AddTorrent, AddTorrentOptions, Api, ApiError,
    api::{ApiAddTorrentResponse, TorrentIdOrHash},
};
use parking_lot::{Mutex, RwLock};
use tracing::{debug, warn};

use crate::config::HookConfig;
use crate::state::SharedState;
use rqbit_ui::ui::add_torrent_dialog::AddTorrentEntry;
use rqbit_ui::ui::file_table::FileRow;

use super::HookEvent;
use super::payload::{
    FileSelectionRequest, FileSelectionResponse, TorrentAddedPayload, TorrentCompletedPayload,
    TorrentInfo, envelope,
};

/// How often the completion watcher polls the session.
const WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Bookkeeping for the completion watcher: the last-seen `finished` flag per
/// torrent ID, plus IDs primed by a recent UI add (see [`HookDispatcher::prime_added`]).
#[derive(Default)]
struct WatcherInner {
    /// `Arc::as_ptr` of the API the maps below belong to; a change means the
    /// session was restarted and everything must be re-seeded.
    api_ptr: usize,
    /// Torrent ID → last observed `finished` flag. Also acts as the set of
    /// "known" torrents for [`HookDispatcher::is_known`].
    finished: HashMap<usize, bool>,
    /// IDs added through the UI since their last observation by the watcher.
    primed: HashSet<usize>,
}

/// One torrent as observed by a watcher poll.
#[derive(Clone, Debug)]
pub(crate) struct ObservedTorrent {
    pub info: TorrentInfo,
    pub finished: bool,
}

pub(crate) struct AutoloadAddOutcome {
    pub response: ApiAddTorrentResponse,
    pub fresh_id: Option<usize>,
}

#[derive(Debug)]
enum AutoloadSelectionError {
    Api {
        action: &'static str,
        source: ApiError,
    },
    Initialize(anyhow::Error),
    MissingFiles,
    Request {
        hook: String,
        source: anyhow::Error,
    },
    EmptyResponse {
        hook: String,
    },
    InvalidResponse {
        hook: String,
        reason: String,
    },
}

impl fmt::Display for AutoloadSelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api { action, source } => write!(f, "{action}: {source}"),
            Self::Initialize(source) => write!(f, "torrent initialization failed: {source}"),
            Self::MissingFiles => write!(f, "initialized torrent has no file list"),
            Self::Request { hook, source } => {
                write!(f, "file selection hook {hook:?} failed: {source}")
            }
            Self::EmptyResponse { hook } => {
                write!(f, "file selection hook {hook:?} returned an empty response")
            }
            Self::InvalidResponse { hook, reason } => {
                write!(
                    f,
                    "file selection hook {hook:?} returned an invalid response: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for AutoloadSelectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Api { source, .. } => Some(source),
            Self::Initialize(source) => Some(source.as_ref()),
            Self::Request { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl WatcherInner {
    /// Reset all state (used when the underlying session was replaced).
    fn reset(&mut self) {
        self.finished.clear();
        self.primed.clear();
    }

    /// Fold one poll's observations into the state and return the torrents
    /// that transitioned to finished since the last poll.
    ///
    /// First observation of an ID seeds silently (no event) unless the ID was
    /// primed by a UI add — those torrents were known to be unfinished when
    /// added, so observing them finished means they completed in between.
    fn observe(&mut self, torrents: Vec<ObservedTorrent>) -> Vec<TorrentInfo> {
        let mut completed = Vec::new();
        let mut seen: HashSet<usize> = HashSet::new();
        for t in &torrents {
            let Some(id) = t.info.id else { continue };
            seen.insert(id);
            match self.finished.get(&id).copied() {
                Some(prev) => {
                    if !prev && t.finished {
                        completed.push(t.info.clone());
                    }
                    self.finished.insert(id, t.finished);
                }
                None => {
                    if self.primed.remove(&id) && t.finished {
                        completed.push(t.info.clone());
                    }
                    self.finished.insert(id, t.finished);
                }
            }
        }
        // Drop torrents that disappeared (deleted / session replaced).
        self.finished.retain(|id, _| seen.contains(id));
        self.primed.retain(|id| seen.contains(id));
        completed
    }
}

/// Enabled hooks subscribed to `event`, in config order.
fn matching_hooks(shared: &Arc<RwLock<SharedState>>, event: HookEvent) -> Vec<HookConfig> {
    shared
        .read()
        .config()
        .hooks
        .hooks
        .iter()
        .filter(|h| h.enabled && h.events.contains(&event))
        .cloned()
        .collect()
}

/// Hooks that must select files for a watched-folder add. Explicit
/// `file_selection` subscribers retain their existing auto-load role;
/// columns providers participate only through the explicit opt-in.
fn autoload_selection_hooks(shared: &Arc<RwLock<SharedState>>) -> Vec<HookConfig> {
    shared
        .read()
        .config()
        .hooks
        .hooks
        .iter()
        .filter(|hook| {
            hook.enabled
                && (hook.events.contains(&HookEvent::FileSelection)
                    || (hook.autoload_file_selection
                        && hook.events.contains(&HookEvent::TorrentColumns)))
        })
        .cloned()
        .collect()
}

/// Fire-and-forget delivery of an event to all matching hooks: one tokio task
/// per hook, so a slow endpoint cannot delay the others.
fn spawn_deliveries(
    shared: &Arc<RwLock<SharedState>>,
    client: &reqwest::Client,
    runtime: &tokio::runtime::Handle,
    event: HookEvent,
    payload: &serde_json::Value,
) {
    for hook in matching_hooks(shared, event) {
        let client = client.clone();
        let payload = payload.clone();
        runtime.spawn(async move {
            debug!(hook = %hook.name, event = event.as_str(), "delivering hook");
            if let Err(e) = super::http::send(&client, &hook, event, &payload).await {
                warn!(hook = %hook.name, event = event.as_str(), error = %e, "hook delivery failed");
            }
        });
    }
}

/// Routes torrent lifecycle events to the hooks configured in
/// [`crate::state::SharedState`]. Hooks are read from the config at dispatch
/// time, so settings changes apply without rebuilding the dispatcher.
pub struct HookDispatcher {
    shared: Arc<RwLock<SharedState>>,
    runtime: tokio::runtime::Handle,
    client: reqwest::Client,
    inner: Arc<Mutex<WatcherInner>>,
}

impl HookDispatcher {
    pub fn new(shared: Arc<RwLock<SharedState>>, runtime: tokio::runtime::Handle) -> Self {
        Self {
            shared,
            runtime,
            client: reqwest::Client::new(),
            inner: Arc::new(Mutex::new(WatcherInner::default())),
        }
    }

    /// Fire-and-forget delivery of an event to all matching hooks.
    pub fn dispatch(&self, event: HookEvent, payload: serde_json::Value) {
        spawn_deliveries(&self.shared, &self.client, &self.runtime, event, &payload);
    }

    /// Add a torrent through the session API, fire the `torrent_added` hook
    /// for fresh adds, and move the local source file (when the add came from
    /// one and a move folder is configured). This is the shared add path: the
    /// add-torrent dialog and the folder watcher both call it, so
    /// watcher-added torrents behave identically to UI-added ones. `paused`
    /// is the requested initial state; the watcher forces it while strict file
    /// selection is pending, and dialog adds pass `false`. The hook
    /// is skipped when the torrent was already managed (the API response
    /// does not distinguish that case, so the watcher's known-ID set
    /// decides); the source file is moved regardless, as cleanup.
    async fn add_and_dispatch_added_outcome(
        &self,
        api: &Api,
        add: AddTorrent<'static>,
        only_files: Option<Vec<usize>>,
        overwrite: bool,
        paused: bool,
        source_path: Option<std::path::PathBuf>,
    ) -> Result<AutoloadAddOutcome, ApiError> {
        let opts = Some(AddTorrentOptions {
            only_files: only_files.clone(),
            overwrite,
            paused,
            ..Default::default()
        });
        let response = api.api_add_torrent(add, opts).await?;
        let fresh_id = response.id.filter(|id| !self.is_known(*id));
        if let Some(id) = fresh_id {
            self.prime_added(id);
            let files = response.details.files.as_deref().unwrap_or(&[]);
            self.dispatch(
                HookEvent::TorrentAdded,
                envelope(
                    HookEvent::TorrentAdded,
                    TorrentAddedPayload {
                        torrent: TorrentInfo {
                            id: Some(id),
                            name: response.details.name.clone(),
                            info_hash: response.details.info_hash.clone(),
                            output_folder: response.output_folder.clone(),
                        },
                        files_total: files.len(),
                        files_selected: files.iter().filter(|f| f.included).count(),
                        selected_indices: only_files,
                    },
                ),
            );
        }
        if let Some(path) = source_path {
            let move_folder = self.shared.read().config().move_torrents_folder;
            // Skip quietly when the file is already gone (e.g. the folder
            // watcher handled the same file first).
            if !move_folder.as_os_str().is_empty() && path.exists() {
                match crate::folder_watcher::move_file(&path, &move_folder) {
                    Ok(()) => {
                        tracing::info!(
                            from = %path.display(),
                            "moved added torrent source file"
                        );
                    }
                    Err(e) => {
                        warn!(
                            from = %path.display(),
                            error = %e,
                            "error moving torrent source file"
                        );
                    }
                }
            }
        }
        Ok(AutoloadAddOutcome { response, fresh_id })
    }

    /// Add through the shared manual/UI path, discarding the watcher's
    /// fresh-add metadata.
    pub async fn add_and_dispatch_added(
        &self,
        api: &Api,
        add: AddTorrent<'static>,
        only_files: Option<Vec<usize>>,
        overwrite: bool,
        paused: bool,
        source_path: Option<std::path::PathBuf>,
    ) -> Result<ApiAddTorrentResponse, ApiError> {
        self.add_and_dispatch_added_outcome(api, add, only_files, overwrite, paused, source_path)
            .await
            .map(|outcome| outcome.response)
    }

    /// Watcher variant that also reports whether this was a fresh session add.
    pub(crate) async fn add_autoload(
        &self,
        api: &Api,
        add: AddTorrent<'static>,
        only_files: Option<Vec<usize>>,
        overwrite: bool,
        paused: bool,
        source_path: Option<std::path::PathBuf>,
    ) -> Result<AutoloadAddOutcome, ApiError> {
        self.add_and_dispatch_added_outcome(api, add, only_files, overwrite, paused, source_path)
            .await
    }

    /// Whether the watcher already knows this torrent ID (it existed in the
    /// session before). Used to detect re-adds of already-managed torrents.
    pub fn is_known(&self, id: usize) -> bool {
        self.inner.lock().finished.contains_key(&id)
    }

    /// Record a torrent added through the UI: marks it known-and-unfinished so
    /// [`HookDispatcher::is_known`] is accurate immediately (no poll-interval
    /// race), and so the completion watcher treats it as unfinished on its
    /// first poll.
    pub fn prime_added(&self, id: usize) {
        let mut inner = self.inner.lock();
        inner.primed.insert(id);
        inner.finished.insert(id, false);
    }

    /// Forget a torrent ID (called when it is deleted through the UI).
    pub fn forget(&self, id: usize) {
        let mut inner = self.inner.lock();
        inner.primed.remove(&id);
        inner.finished.remove(&id);
    }

    /// Run the `file_selection` hooks against an entry's file list, mutating
    /// `included` flags in place. Returns the names of hooks that changed the
    /// selection. Hooks run sequentially in config order, each bounded by its
    /// timeout; failures leave the selection unchanged.
    pub async fn apply_file_selection(&self, entry: &mut AddTorrentEntry) -> Vec<String> {
        let hooks = matching_hooks(&self.shared, HookEvent::FileSelection);
        let mut applied = Vec::new();
        for hook in hooks {
            let torrent = TorrentInfo {
                id: None,
                name: Some(entry.name.clone()),
                info_hash: entry.info_hash.clone(),
                output_folder: entry.output_folder.to_string_lossy().into_owned(),
            };
            let request = FileSelectionRequest::new(torrent, &entry.files);
            let payload = envelope(HookEvent::FileSelection, request);
            match super::http::send(&self.client, &hook, HookEvent::FileSelection, &payload).await {
                Ok(Some(body)) => match serde_json::from_value::<FileSelectionResponse>(body) {
                    Ok(resp) => {
                        if resp.selects_nothing(&entry.files) {
                            warn!(
                                hook = %hook.name,
                                "file selection hook deselected everything; ignoring response"
                            );
                            continue;
                        }
                        if resp.apply(&mut entry.files) {
                            applied.push(hook.name.clone());
                        }
                    }
                    Err(e) => {
                        warn!(hook = %hook.name, error = %e, "invalid file selection response");
                    }
                },
                Ok(None) => {}
                Err(e) => {
                    warn!(hook = %hook.name, error = %e, "file selection hook failed");
                }
            }
        }
        applied
    }

    pub(crate) fn autoload_hooks(&self) -> Vec<HookConfig> {
        autoload_selection_hooks(&self.shared)
    }

    /// Finalize a fresh watched-folder add without blocking the watcher on
    /// HTTP requests or magnet metadata resolution.
    pub(crate) fn spawn_autoload_selection(
        self: &Arc<Self>,
        api: Arc<Api>,
        selection_hooks: Vec<HookConfig>,
        id: usize,
        auto_add_paused: bool,
        source: String,
    ) {
        let this = Arc::clone(self);
        self.runtime.spawn(async move {
            let cancel = api.session().cancellation_token().clone();
            let result = tokio::select! {
                _ = cancel.cancelled() => {
                    debug!(torrent_id = id, path = %source, "autoload selection cancelled with its session");
                    return;
                }
                result = this.finish_autoload_selection(&api, id, &selection_hooks, auto_add_paused) => result,
            };
            if let Err(error) = result {
                warn!(
                    torrent_id = id,
                    path = %source,
                    error = %error,
                    "autoload finalization failed; torrent was not started"
                );
            }
        });
    }

    async fn finish_autoload_selection(
        &self,
        api: &Api,
        id: usize,
        selection_hooks: &[HookConfig],
        auto_add_paused: bool,
    ) -> Result<(), AutoloadSelectionError> {
        let selected = self
            .resolve_autoload_selection(api, id, selection_hooks)
            .await?;
        let is_empty = selected.is_empty();
        let selected: HashSet<usize> = selected.into_iter().collect();
        api.api_torrent_action_update_only_files(TorrentIdOrHash::Id(id), &selected)
            .await
            .map_err(|source| AutoloadSelectionError::Api {
                action: "updating watched torrent file selection",
                source,
            })?;
        if is_empty || auto_add_paused {
            return Ok(());
        }
        api.api_torrent_action_start(TorrentIdOrHash::Id(id))
            .await
            .map_err(|source| AutoloadSelectionError::Api {
                action: "starting watched torrent after file selection",
                source,
            })?;
        Ok(())
    }

    async fn resolve_autoload_selection(
        &self,
        api: &Api,
        id: usize,
        selection_hooks: &[HookConfig],
    ) -> Result<Vec<usize>, AutoloadSelectionError> {
        let handle = api.mgr_handle(TorrentIdOrHash::Id(id)).map_err(|source| {
            AutoloadSelectionError::Api {
                action: "resolving watched torrent handle",
                source,
            }
        })?;
        handle
            .wait_until_initialized()
            .await
            .map_err(AutoloadSelectionError::Initialize)?;
        let details = api
            .api_torrent_details(TorrentIdOrHash::Id(id))
            .map_err(|source| AutoloadSelectionError::Api {
                action: "reading initialized watched torrent details",
                source,
            })?;
        let mut files: Vec<FileRow> = details
            .files
            .as_deref()
            .ok_or(AutoloadSelectionError::MissingFiles)?
            .iter()
            .enumerate()
            .map(|(index, file)| FileRow {
                file_index: index,
                name: file.name.clone(),
                length: file.length,
                // The persisted pending state is empty, but hook responses
                // compose from an unrestricted in-memory selection.
                included: true,
                exists: false,
                progress: 0,
                custom: Default::default(),
            })
            .collect();
        let torrent = TorrentInfo {
            id: Some(id),
            name: details.name.clone(),
            info_hash: details.info_hash.clone(),
            output_folder: details.output_folder.clone(),
        };

        for hook in selection_hooks {
            let request = FileSelectionRequest::new(torrent.clone(), &files);
            let payload = envelope(HookEvent::FileSelection, request);
            let body = super::http::send(&self.client, hook, HookEvent::FileSelection, &payload)
                .await
                .map_err(|source| AutoloadSelectionError::Request {
                    hook: hook.name.clone(),
                    source,
                })?
                .ok_or_else(|| AutoloadSelectionError::EmptyResponse {
                    hook: hook.name.clone(),
                })?;
            let response: FileSelectionResponse =
                serde_json::from_value(body).map_err(|error| {
                    AutoloadSelectionError::InvalidResponse {
                        hook: hook.name.clone(),
                        reason: error.to_string(),
                    }
                })?;
            response.validate_strict(files.len()).map_err(|reason| {
                AutoloadSelectionError::InvalidResponse {
                    hook: hook.name.clone(),
                    reason,
                }
            })?;
            response.apply(&mut files);
        }

        Ok(files
            .iter()
            .filter(|file| file.included)
            .map(|file| file.file_index)
            .collect())
    }

    /// Spawn the background completion watcher on the tokio runtime. It polls
    /// the session every [`WATCH_INTERVAL`] and diffs the per-torrent
    /// `finished` flag; not-finished → finished transitions fire
    /// `torrent_completed`.
    pub fn start(&self) {
        let shared = self.shared.clone();
        let client = self.client.clone();
        let runtime = self.runtime.clone();
        let watcher_runtime = runtime.clone();
        let inner = self.inner.clone();
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(WATCH_INTERVAL);
            loop {
                interval.tick().await;
                for info in watch_once(&shared, &inner) {
                    let payload = envelope(
                        HookEvent::TorrentCompleted,
                        TorrentCompletedPayload {
                            torrent: info.clone(),
                        },
                    );
                    spawn_deliveries(
                        &shared,
                        &client,
                        &watcher_runtime,
                        HookEvent::TorrentCompleted,
                        &payload,
                    );
                }
            }
        });
    }
}

/// One watcher tick: list torrents with stats, reset state if the session was
/// replaced, and return the torrents that just completed.
fn watch_once(
    shared: &Arc<RwLock<SharedState>>,
    inner: &Arc<Mutex<WatcherInner>>,
) -> Vec<TorrentInfo> {
    let api = shared.read().api();
    let api_ptr = Arc::as_ptr(&api) as usize;
    let torrents: Vec<ObservedTorrent> = api
        .api_torrent_list_ext(librqbit::api::ApiTorrentListOpts { with_stats: true })
        .torrents
        .into_iter()
        .map(|r| ObservedTorrent {
            finished: r.stats.as_ref().is_some_and(|s| s.finished),
            info: TorrentInfo {
                id: r.id,
                name: r.name,
                info_hash: r.info_hash,
                output_folder: r.output_folder,
            },
        })
        .collect();
    let mut inner = inner.lock();
    if inner.api_ptr != api_ptr {
        debug!("hooks watcher: session replaced, re-seeding state");
        inner.reset();
        inner.api_ptr = api_ptr;
    }
    inner.observe(torrents)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: usize) -> TorrentInfo {
        TorrentInfo {
            id: Some(id),
            name: Some(format!("t{id}")),
            info_hash: format!("hash{id}"),
            output_folder: "/tmp".into(),
        }
    }

    fn observed(id: usize, finished: bool) -> ObservedTorrent {
        ObservedTorrent {
            info: info(id),
            finished,
        }
    }

    #[test]
    fn seeds_silently_on_first_observation() {
        let mut inner = WatcherInner::default();
        // Already-finished torrent seen for the first time → no event.
        assert!(inner.observe(vec![observed(1, true)]).is_empty());
        assert!(inner.finished.contains_key(&1));
    }

    #[test]
    fn fires_on_not_finished_to_finished_transition() {
        let mut inner = WatcherInner::default();
        assert!(inner.observe(vec![observed(1, false)]).is_empty());
        let completed = inner.observe(vec![observed(1, true)]);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].id, Some(1));
        // Stays finished: no repeat event.
        assert!(inner.observe(vec![observed(1, true)]).is_empty());
    }

    #[test]
    fn unfinishes_when_selection_grows() {
        // update_only_files can flip finished back to false (more files
        // selected); a later re-completion fires again.
        let mut inner = WatcherInner::default();
        inner.observe(vec![observed(1, false)]);
        inner.observe(vec![observed(1, true)]);
        assert!(inner.observe(vec![observed(1, false)]).is_empty());
        assert_eq!(inner.observe(vec![observed(1, true)]).len(), 1);
    }

    #[test]
    fn primed_torrent_completing_before_first_poll_fires() {
        let mut inner = WatcherInner::default();
        inner.primed.insert(2);
        inner.finished.insert(2, false);
        let completed = inner.observe(vec![observed(2, true)]);
        assert_eq!(completed.len(), 1);
        // Prime consumed: observing finished again does not re-fire.
        assert!(inner.observe(vec![observed(2, true)]).is_empty());
    }

    #[test]
    fn primed_torrent_unfinished_on_first_poll_is_quiet() {
        let mut inner = WatcherInner::default();
        inner.primed.insert(2);
        inner.finished.insert(2, false);
        assert!(inner.observe(vec![observed(2, false)]).is_empty());
        assert_eq!(inner.observe(vec![observed(2, true)]).len(), 1);
    }

    #[test]
    fn deleted_torrents_are_pruned() {
        let mut inner = WatcherInner::default();
        inner.observe(vec![observed(1, false), observed(2, false)]);
        inner.observe(vec![observed(1, false)]); // 2 deleted
        assert!(!inner.finished.contains_key(&2));
        // Re-appearing later (fresh add) seeds silently.
        assert!(
            inner
                .observe(vec![observed(1, false), observed(2, false)])
                .is_empty()
        );
    }

    #[test]
    fn reset_clears_everything() {
        let mut inner = WatcherInner::default();
        inner.observe(vec![observed(1, false)]);
        inner.primed.insert(3);
        inner.reset();
        assert!(inner.finished.is_empty());
        assert!(inner.primed.is_empty());
        // After reset, the same torrent seeds silently again (no event).
        assert!(inner.observe(vec![observed(1, true)]).is_empty());
    }

    // ── End-to-end over real HTTP ──────────────────────────────────────────

    use crate::config::{HookConfig, HttpMethod, RqbitDesktopConfigHooks};
    use crate::hooks::http::testutil::serve_once;
    use crate::state::SharedState;
    use rqbit_ui::client::UiAddSource;
    use rqbit_ui::ui::add_torrent_dialog::AddTorrentEntry;
    use rqbit_ui::ui::file_table::FileRow;

    /// A minimal real session (no DHT, no listener, no persistence) wrapped
    /// in a SharedState with the given hooks config. Also returns the session
    /// API, for dispatchers that need it (e.g. `file_selection_only_files`).
    async fn test_dispatcher_with_api(
        hooks: Vec<HookConfig>,
    ) -> (HookDispatcher, Arc<librqbit::Api>) {
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
        let api = Arc::new(librqbit::Api::new(session, None, None));
        let config = crate::config::RqbitDesktopConfig {
            hooks: RqbitDesktopConfigHooks { hooks },
            ..Default::default()
        };
        let shared = Arc::new(RwLock::new(SharedState::for_test(config, api.clone())));
        (
            HookDispatcher::new(shared, tokio::runtime::Handle::current()),
            api,
        )
    }

    async fn test_dispatcher(hooks: Vec<HookConfig>) -> HookDispatcher {
        test_dispatcher_with_api(hooks).await.0
    }

    fn test_entry() -> AddTorrentEntry {
        AddTorrentEntry {
            source: UiAddSource::Bytes(vec![]),
            name: "Test Torrent".into(),
            info_hash: "0123456789abcdef".into(),
            output_folder: "/tmp".into(),
            source_path: None,
            files: vec![
                FileRow {
                    file_index: 0,
                    name: "movie.mkv".into(),
                    length: 100,
                    included: true,
                    exists: false,
                    progress: 0,
                    custom: Default::default(),
                },
                FileRow {
                    file_index: 1,
                    name: "info.nfo".into(),
                    length: 1,
                    included: true,
                    exists: false,
                    progress: 0,
                    custom: Default::default(),
                },
            ],
            hook_note: None,
        }
    }

    #[tokio::test]
    async fn file_selection_end_to_end_over_http() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve_once(listener, r#"{"include_patterns": ["*.mkv"]}"#);

        let dispatcher = test_dispatcher(vec![HookConfig {
            name: "selector".into(),
            enabled: true,
            events: vec![HookEvent::FileSelection],
            autoload_file_selection: false,
            method: HttpMethod::Post,
            url: format!("http://{addr}/select"),
            timeout_secs: 5,
            headers: Default::default(),
            poll_secs: 10,
        }])
        .await;

        let mut entry = test_entry();
        let (applied, raw_request) =
            tokio::join!(dispatcher.apply_file_selection(&mut entry), server);

        assert_eq!(applied, vec!["selector".to_string()]);
        assert!(entry.files[0].included, "movie.mkv should stay selected");
        assert!(!entry.files[1].included, "info.nfo should be deselected");

        // The request carried the envelope with the file list.
        assert!(raw_request.contains("POST /select"));
        assert!(raw_request.contains("file_selection"));
        assert!(raw_request.contains("movie.mkv"));
        assert!(raw_request.contains("Test Torrent"));
    }

    #[tokio::test]
    async fn disabled_hook_is_not_contacted() {
        let dispatcher = test_dispatcher(vec![HookConfig {
            enabled: false,
            events: vec![HookEvent::FileSelection],
            ..Default::default()
        }])
        .await;

        let mut entry = test_entry();
        let applied = dispatcher.apply_file_selection(&mut entry).await;
        assert!(applied.is_empty());
        assert!(entry.files.iter().all(|f| f.included));
    }

    // ── Autoload selection hook resolution ─────────────────────────────────

    #[tokio::test]
    async fn autoload_hooks_use_opt_in_and_deduplicate_events() {
        let dispatcher = test_dispatcher(vec![
            HookConfig {
                name: "both".into(),
                events: vec![HookEvent::FileSelection, HookEvent::TorrentColumns],
                autoload_file_selection: true,
                ..Default::default()
            },
            HookConfig {
                name: "columns-without-opt-in".into(),
                events: vec![HookEvent::TorrentColumns],
                ..Default::default()
            },
            HookConfig {
                name: "columns-opted-in".into(),
                events: vec![HookEvent::TorrentColumns],
                autoload_file_selection: true,
                ..Default::default()
            },
            HookConfig {
                name: "selection".into(),
                events: vec![HookEvent::FileSelection],
                ..Default::default()
            },
            HookConfig {
                name: "disabled".into(),
                enabled: false,
                events: vec![HookEvent::FileSelection],
                ..Default::default()
            },
        ])
        .await;

        let names: Vec<String> = dispatcher
            .autoload_hooks()
            .into_iter()
            .map(|hook| hook.name)
            .collect();
        assert_eq!(names, ["both", "columns-opted-in", "selection"]);
    }

    #[tokio::test]
    async fn autoload_hooks_empty_without_selection_providers() {
        let dispatcher = test_dispatcher(vec![HookConfig {
            events: vec![HookEvent::TorrentColumns],
            ..Default::default()
        }])
        .await;
        assert!(dispatcher.autoload_hooks().is_empty());
    }

    #[tokio::test]
    async fn dispatch_delivers_torrent_added_over_http() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve_once(listener, "");

        let dispatcher = test_dispatcher(vec![HookConfig {
            name: "notify".into(),
            enabled: true,
            events: vec![HookEvent::TorrentAdded],
            autoload_file_selection: false,
            method: HttpMethod::Post,
            url: format!("http://{addr}/added"),
            timeout_secs: 5,
            headers: Default::default(),
            poll_secs: 10,
        }])
        .await;

        dispatcher.dispatch(
            HookEvent::TorrentAdded,
            serde_json::json!({"event": "torrent_added", "torrent": {"id": 1}}),
        );

        let raw = server.await;
        assert!(raw.contains("POST /added"));
        assert!(raw.contains("torrent_added"));
        assert!(raw.contains("x-rqbit-event: torrent_added"));
    }

    // ── Move-after-add ─────────────────────────────────────────────────────

    use crate::folder_watcher::tests::{TempDir, torrent_bytes};

    /// Adding through the shared callback (the dialog's path) moves the local
    /// source file to the configured folder once a move folder is set.
    #[tokio::test]
    async fn manual_add_moves_source_file() {
        let src_dir = TempDir::new("manual-src");
        let move_dir = TempDir::new("manual-move");
        let src = src_dir.path().join("picked.torrent");
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
        let api = Arc::new(librqbit::Api::new(session, None, None));
        let config = crate::config::RqbitDesktopConfig {
            move_torrents_folder: move_dir.path().to_path_buf(),
            ..Default::default()
        };
        let shared = Arc::new(RwLock::new(SharedState::for_test(config, api.clone())));

        let dispatcher = HookDispatcher::new(shared, tokio::runtime::Handle::current());
        dispatcher
            .add_and_dispatch_added(
                &api,
                librqbit::AddTorrent::from_bytes(torrent_bytes()),
                None,
                true,
                false,
                Some(src.clone()),
            )
            .await
            .unwrap();

        assert!(
            !src.exists(),
            "the picked source file should have been moved"
        );
        let dest = move_dir.path().join("picked.torrent");
        assert_eq!(std::fs::read(&dest).unwrap(), torrent_bytes());
        assert_eq!(
            api.api_torrent_list_ext(librqbit::api::ApiTorrentListOpts { with_stats: false })
                .torrents
                .len(),
            1,
            "the torrent should be managed by the session"
        );
    }

    /// Without a configured move folder the source file stays where it is.
    #[tokio::test]
    async fn manual_add_keeps_source_without_move_folder() {
        let src_dir = TempDir::new("nomove-src");
        let src = src_dir.path().join("picked.torrent");
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
        let api = Arc::new(librqbit::Api::new(session, None, None));
        let shared = Arc::new(RwLock::new(SharedState::for_test(
            crate::config::RqbitDesktopConfig::default(),
            api.clone(),
        )));

        let dispatcher = HookDispatcher::new(shared, tokio::runtime::Handle::current());
        dispatcher
            .add_and_dispatch_added(
                &api,
                librqbit::AddTorrent::from_bytes(torrent_bytes()),
                None,
                true,
                false,
                Some(src.clone()),
            )
            .await
            .unwrap();

        assert!(src.exists(), "no move folder configured, file stays");
    }
}
