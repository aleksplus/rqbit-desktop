//! Hook-provided custom columns (the `torrent_columns` event).
//!
//! A [`ColumnsProvider`] polls every configured hook, POSTing (or GETing)
//! the current torrent list; the first hook that responds successfully owns
//! the columns: its response declares the column set plus per-torrent and
//! per-file values, cached in a [`ColumnsSnapshot`] that the WS server and
//! the desktop client read to decorate list/details payloads.
//!
//! Request body (also sent as `?payload=` for GET hooks):
//!
//! ```json
//! {
//!   "event": "torrent_columns",
//!   "timestamp_unix_secs": 1760000000,
//!   "torrents": [
//!     { "id": 3, "name": "Ubuntu", "info_hash": "…", "output_folder": "/Downloads",
//!       "files": [{ "index": 0, "name": "a.iso", "length": 123 }] }
//!   ]
//! }
//! ```
//!
//! `files` arrays are included only once the hook has declared a file-level
//! column (`"files": true`), and are resolved per torrent from
//! `Api::api_torrent_details`. Response body (all fields optional, unknown
//! fields ignored):
//!
//! ```json
//! {
//!   "columns": [ { "key": "sync_status", "name": "Sync", "files": false,
//!                  "kind": "status", "width": 100 } ],
//!   "torrents": { "<info_hash>": { "sync_status": "synced" } },
//!   "files": { "<info_hash>": [ { "index": 0,
//!                                 "values": { "sync_status": "synced" } } ] }
//! }
//! ```
//!
//! Values must be JSON scalars (string / number / bool); they are stored as
//! strings. Column entries with empty or duplicate keys are dropped, the
//! column count is capped, and value strings are truncated — a misbehaving
//! hook cannot break the layout. An empty response body means "no columns
//! right now" (the hook counts as connected). Errors flip the snapshot's
//! `connected` flag to false (the UI then hides the columns and shows the
//! offline plug) and are logged at `warn!` level, which reaches both UIs'
//! log pages through the shared log broadcast.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use librqbit::Api;
use librqbit::api::{ApiTorrentListOpts, TorrentIdOrHash};
use parking_lot::Mutex;
use rqbit_protocol as proto;
use rqbit_protocol::{CustomColumnKind, CustomColumnSpec, CustomColumnsState};
use serde::{Deserialize, Serialize};

/// Hard cap on columns a single hook may declare (layout protection).
const MAX_COLUMNS: usize = 8;
/// Hard cap on a single stored value's length.
const MAX_VALUE_LEN: usize = 64;
/// Clamping range for hook-declared column widths (logical pixels).
const MIN_WIDTH: f32 = 40.0;
const MAX_WIDTH: f32 = 400.0;
/// Minimum seconds between polls of one hook.
const MIN_POLL_SECS: u64 = 2;
/// While a hook keeps failing after its first error, re-log at most this
/// often so the log page stays readable.
const FAIL_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Wire name of the event; also the payload's `event` field and the
/// `X-Rqbit-Event` header.
pub const EVENT_NAME: &str = "torrent_columns";

/// One hook configured to provide custom columns. Converted from the
/// desktop's `HookConfig` (hooks subscribed to `torrent_columns`) or built
/// from the standalone daemon's CLI flags — deliberately independent of the
/// desktop config types.
#[derive(Debug, Clone)]
pub struct ColumnsHook {
    pub name: String,
    pub url: String,
    pub method: ColumnsMethod,
    pub timeout_secs: u64,
    pub headers: BTreeMap<String, String>,
    /// Seconds between polls of this hook (clamped to [`MIN_POLL_SECS`]).
    pub poll_secs: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColumnsMethod {
    #[default]
    Post,
    Get,
}

impl ColumnsHook {
    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_secs.max(MIN_POLL_SECS))
    }
}

/// The latest successful poll result plus connectivity, shared with the WS
/// server and the desktop client. `state.connected == false` means the last
/// poll failed; the value maps keep the last good data so a reconnecting
/// hook repopulates the UI instantly.
#[derive(Debug, Clone, Default)]
pub struct ColumnsSnapshot {
    pub state: CustomColumnsState,
    /// Per-torrent values, keyed by info hash then column key.
    pub torrent_values: HashMap<String, BTreeMap<String, String>>,
    /// Per-file values, keyed by info hash then file index then column key.
    pub file_values: HashMap<String, HashMap<usize, BTreeMap<String, String>>>,
}

impl ColumnsSnapshot {
    /// Attach torrent-level values to a torrent-list payload.
    pub fn decorate_list(&self, list: &mut proto::TorrentList) {
        for torrent in &mut list.torrents {
            if let Some(values) = self.torrent_values.get(&torrent.info_hash) {
                torrent.custom = Some(values.clone());
            }
        }
    }

    /// Attach torrent-level and per-file values to a details payload.
    pub fn decorate_details(&self, details: &mut proto::TorrentDetails) {
        if let Some(values) = self.torrent_values.get(&details.info_hash) {
            details.custom = Some(values.clone());
        }
        let Some(per_file) = self.file_values.get(&details.info_hash) else {
            return;
        };
        if let Some(files) = &mut details.files {
            for (ix, file) in files.iter_mut().enumerate() {
                if let Some(values) = per_file.get(&ix) {
                    file.custom = Some(values.clone());
                }
            }
        }
    }
}

// --- response parsing ---

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ColumnsResponse {
    columns: Vec<ColumnSpecInput>,
    torrents: HashMap<String, BTreeMap<String, serde_json::Value>>,
    files: HashMap<String, Vec<FileValuesInput>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ColumnSpecInput {
    key: String,
    name: Option<String>,
    files: bool,
    kind: Option<CustomColumnKind>,
    width: Option<f32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileValuesInput {
    index: usize,
    values: BTreeMap<String, serde_json::Value>,
}

/// Parse and sanitize a hook response body into a snapshot. `None` body
/// (empty response) means "connected, no columns right now". Returns `Err`
/// when the body is not a valid response object.
fn parse_snapshot(
    source: &str,
    body: Option<&serde_json::Value>,
) -> anyhow::Result<ColumnsSnapshot> {
    let Some(body) = body else {
        return Ok(ColumnsSnapshot {
            state: CustomColumnsState {
                connected: true,
                source: Some(source.to_string()),
                ..Default::default()
            },
            ..Default::default()
        });
    };
    let resp: ColumnsResponse = serde_json::from_value(body.clone())?;

    // Column definitions: drop empty/duplicate keys, cap the count, clamp
    // widths, default the label to the key.
    let mut specs: Vec<CustomColumnSpec> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for col in resp.columns {
        if specs.len() >= MAX_COLUMNS {
            break;
        }
        let key = col.key.trim().to_string();
        if key.is_empty() || !seen.insert(key.clone()) {
            continue;
        }
        let name = match col.name {
            Some(n) if !n.trim().is_empty() => n.trim().to_string(),
            _ => key.clone(),
        };
        specs.push(CustomColumnSpec {
            key,
            name,
            files: col.files,
            kind: col.kind.unwrap_or_default(),
            width: col.width.map(|w| w.clamp(MIN_WIDTH, MAX_WIDTH)),
        });
    }
    let declared: HashSet<&str> = specs.iter().map(|c| c.key.as_str()).collect();

    // Values: keep only keys that match a declared column; scalars only.
    let mut torrent_values: HashMap<String, BTreeMap<String, String>> = HashMap::new();
    for (info_hash, values) in resp.torrents {
        let sanitized = sanitize_values(values, &declared);
        if !sanitized.is_empty() {
            torrent_values.insert(info_hash, sanitized);
        }
    }

    let mut file_values: HashMap<String, HashMap<usize, BTreeMap<String, String>>> = HashMap::new();
    for (info_hash, entries) in resp.files {
        let mut per_file: HashMap<usize, BTreeMap<String, String>> = HashMap::new();
        for entry in entries {
            let sanitized = sanitize_values(entry.values, &declared);
            if !sanitized.is_empty() {
                per_file.insert(entry.index, sanitized);
            }
        }
        if !per_file.is_empty() {
            file_values.insert(info_hash, per_file);
        }
    }

    Ok(ColumnsSnapshot {
        state: CustomColumnsState {
            columns: specs,
            connected: true,
            source: Some(source.to_string()),
        },
        torrent_values,
        file_values,
    })
}

/// JSON scalars keyed by column key → stored strings. Unknown keys and
/// non-scalar values (objects/arrays/null) are not renderable and are
/// dropped.
fn sanitize_values(
    values: BTreeMap<String, serde_json::Value>,
    declared: &HashSet<&str>,
) -> BTreeMap<String, String> {
    values
        .into_iter()
        .filter(|(k, _)| declared.contains(k.as_str()))
        .filter_map(|(k, v)| scalar_to_string(&v).map(|v| (k, v)))
        .collect()
}

/// JSON scalar → stored string.
fn scalar_to_string(v: &serde_json::Value) -> Option<String> {
    let s = match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => return None,
    };
    Some(truncate(&s, MAX_VALUE_LEN))
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((ix, _)) => s[..ix].to_string(),
        None => s.to_string(),
    }
}

// --- transport ---

/// Send one `torrent_columns` request. Mirrors the desktop `hooks::http`
/// contract: POST with a JSON body, or GET with `?payload=<json>`;
/// `X-Rqbit-Event` always sent; custom headers appended; per-request timeout
/// covering the body read.
async fn send_hook_request(
    client: &reqwest::Client,
    hook: &ColumnsHook,
    payload: &serde_json::Value,
) -> anyhow::Result<Option<serde_json::Value>> {
    let mut req = match hook.method {
        ColumnsMethod::Post => client.post(&hook.url).json(payload),
        ColumnsMethod::Get => {
            let encoded = serde_json::to_string(payload)?;
            client.get(&hook.url).query(&[("payload", encoded)])
        }
    };
    req = req.header("X-Rqbit-Event", EVENT_NAME);
    for (k, v) in &hook.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req
        .timeout(Duration::from_secs(hook.timeout_secs.max(1)))
        .send()
        .await?
        .error_for_status()?;
    let body = resp.bytes().await?;
    if body.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&body)?))
}

// --- provider ---

/// What the last failure means for logging (see [`Store::apply_failure`]).
#[derive(Debug, PartialEq, Eq)]
enum FailureLog {
    /// Log it: first failure, or the rate-limit window has elapsed.
    Log,
    /// Repeated failure within the rate-limit window: stay quiet.
    Silent,
}

#[derive(Default)]
struct Store {
    snapshot: Option<ColumnsSnapshot>,
    /// Include per-torrent `files` arrays in the request payload once the
    /// hook declared a file-level column.
    needs_files: bool,
    last_poll: Vec<Option<Instant>>,
    last_fail_log: Option<Instant>,
}

impl Store {
    fn new(hook_count: usize) -> Self {
        Self {
            last_poll: vec![None; hook_count],
            ..Default::default()
        }
    }

    fn snapshot(&self) -> Option<ColumnsSnapshot> {
        self.snapshot.clone()
    }

    fn state(&self) -> Option<CustomColumnsState> {
        self.snapshot.as_ref().map(|s| s.state.clone())
    }

    fn due(&self, ix: usize, hook: &ColumnsHook, now: Instant) -> bool {
        match self.last_poll.get(ix).copied().flatten() {
            Some(last) => now.duration_since(last) >= hook.poll_interval(),
            None => true,
        }
    }

    fn mark_polled(&mut self, ix: usize, now: Instant) {
        if ix < self.last_poll.len() {
            self.last_poll[ix] = Some(now);
        }
    }

    /// Record a successful poll. Returns true when this is a
    /// disconnected → connected transition worth logging.
    fn apply_success(
        &mut self,
        hook: &ColumnsHook,
        body: Option<&serde_json::Value>,
    ) -> anyhow::Result<bool> {
        let snapshot = parse_snapshot(&hook.name, body)?;
        let was_connected = self.snapshot.as_ref().is_some_and(|s| s.state.connected);
        self.needs_files = snapshot.state.columns.iter().any(|c| c.files);
        self.snapshot = Some(snapshot);
        self.last_fail_log = None;
        Ok(!was_connected)
    }

    /// Record a failed poll: keep the last good data, flip `connected` off,
    /// and decide whether the error should be logged right now.
    fn apply_failure(&mut self, hook: &ColumnsHook) -> FailureLog {
        let was_connected = self
            .snapshot
            .as_ref()
            .is_some_and(|s| s.state.connected && s.state.source.as_deref() == Some(&hook.name));
        if let Some(snapshot) = &mut self.snapshot {
            snapshot.state.connected = false;
        }
        let now = Instant::now();
        if was_connected || self.last_fail_log.is_none() {
            self.last_fail_log = Some(now);
            return FailureLog::Log;
        }
        if now.duration_since(self.last_fail_log.unwrap_or(now)) >= FAIL_LOG_INTERVAL {
            self.last_fail_log = Some(now);
            FailureLog::Log
        } else {
            FailureLog::Silent
        }
    }
}

/// Polls the configured hooks and caches the winning response. One provider
/// serves one session: it is constructed (and started) wherever the session
/// is built — the desktop's bundled web view and the standalone daemon's
/// main — and dies with that session.
pub struct ColumnsProvider {
    api: Arc<Api>,
    hooks: Vec<ColumnsHook>,
    client: reqwest::Client,
    store: Mutex<Store>,
}

impl ColumnsProvider {
    /// Build a provider for the given hooks. With an empty list the provider
    /// is inert (and `start` spawns nothing); callers usually pass `None`
    /// around instead.
    pub fn new(api: Arc<Api>, hooks: Vec<ColumnsHook>) -> Arc<Self> {
        let store = Store::new(hooks.len());
        Arc::new(Self {
            api,
            hooks,
            client: reqwest::Client::new(),
            store: Mutex::new(store),
        })
    }

    /// Number of configured hooks (0 = no column provider configured).
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Spawn the poll loop. Polls run sequentially in config order, each
    /// bounded by its own timeout; a slow hook can delay the others' due
    /// checks but never runs past its timeout.
    pub fn start(self: &Arc<Self>) {
        if self.hooks.is_empty() {
            return;
        }
        tokio::spawn(self.clone().run());
    }

    /// The poll loop, for callers that manage their own task lifetime (the
    /// desktop spawns it on the session so it dies with a settings
    /// reconfigure).
    pub async fn run(self: Arc<Self>) {
        if self.hooks.is_empty() {
            return;
        }
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            self.tick().await;
        }
    }

    /// Latest snapshot (specs + values + connectivity), if any hooks are
    /// configured.
    pub fn snapshot(&self) -> Option<ColumnsSnapshot> {
        self.store.lock().snapshot()
    }

    /// Just the UI-facing state (columns + connected + source).
    pub fn custom_columns_state(&self) -> Option<CustomColumnsState> {
        self.store.lock().state()
    }

    async fn tick(&self) {
        let now = Instant::now();
        for (ix, hook) in self.hooks.iter().enumerate() {
            let (due, needs_files) = {
                let store = self.store.lock();
                (store.due(ix, hook, now), store.needs_files)
            };
            if !due {
                continue;
            }
            self.store.lock().mark_polled(ix, now);
            let payload = self.request_payload(needs_files);
            match send_hook_request(&self.client, hook, &payload).await {
                Ok(body) => match self.store.lock().apply_success(hook, body.as_ref()) {
                    Ok(became_connected) => {
                        if became_connected {
                            tracing::info!(hook = %hook.name, "column hook connected");
                        }
                    }
                    Err(e) => self.log_failure(hook, &e.to_string()),
                },
                Err(e) => self.log_failure(hook, &e.to_string()),
            }
        }
    }

    fn log_failure(&self, hook: &ColumnsHook, error: &str) {
        match self.store.lock().apply_failure(hook) {
            FailureLog::Log => {
                tracing::warn!(hook = %hook.name, error, "column hook failed");
            }
            FailureLog::Silent => {
                tracing::debug!(hook = %hook.name, error, "column hook failed (repeat)");
            }
        }
    }

    /// Build the `torrent_columns` request payload from the current session
    /// state. Per-torrent file lists are resolved only when `needs_files`.
    fn request_payload(&self, needs_files: bool) -> serde_json::Value {
        let list = self
            .api
            .api_torrent_list_ext(ApiTorrentListOpts { with_stats: false });
        let torrents: Vec<serde_json::Value> = list
            .torrents
            .iter()
            .map(|t| {
                let files = if needs_files {
                    t.id.and_then(|id| {
                        let details = self.api.api_torrent_details(TorrentIdOrHash::Id(id)).ok()?;
                        let files = details.files?;
                        Some(
                            files
                                .iter()
                                .enumerate()
                                .map(|(index, f)| {
                                    serde_json::json!({
                                        "index": index,
                                        "name": f.name,
                                        "length": f.length,
                                    })
                                })
                                .collect::<Vec<_>>(),
                        )
                    })
                } else {
                    None
                };
                serde_json::json!({
                    "id": t.id,
                    "name": t.name,
                    "info_hash": t.info_hash,
                    "output_folder": t.output_folder,
                    "files": files,
                })
            })
            .collect();
        serde_json::json!({
            "event": EVENT_NAME,
            "timestamp_unix_secs": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            "torrents": torrents,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::routing::post;
    use serde_json::json;

    type RequestLog = Arc<Mutex<Vec<(Option<String>, serde_json::Value)>>>;

    fn hook(name: &str, url: &str) -> ColumnsHook {
        ColumnsHook {
            name: name.to_string(),
            url: url.to_string(),
            method: ColumnsMethod::Post,
            timeout_secs: 5,
            headers: BTreeMap::new(),
            poll_secs: 10,
        }
    }

    fn sample_response() -> serde_json::Value {
        json!({
            "columns": [
                { "key": "sync_status", "name": "Sync", "kind": "status", "width": 120 },
                { "key": "peer_link", "files": true }
            ],
            "torrents": {
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa": { "sync_status": "synced" }
            },
            "files": {
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa": [
                    { "index": 0, "values": { "peer_link": "up" } }
                ]
            }
        })
    }

    #[test]
    fn parses_full_response() {
        let snapshot = parse_snapshot("sync", Some(&sample_response())).unwrap();
        assert_eq!(snapshot.state.columns.len(), 2);
        assert_eq!(snapshot.state.columns[0].key, "sync_status");
        assert_eq!(snapshot.state.columns[0].kind, CustomColumnKind::Status);
        assert_eq!(snapshot.state.columns[0].width, Some(120.0));
        // Defaults: missing name falls back to the key, kind to Text.
        assert_eq!(snapshot.state.columns[1].name, "peer_link");
        assert_eq!(snapshot.state.columns[1].kind, CustomColumnKind::Text);
        assert!(snapshot.state.columns[1].files);
        assert!(snapshot.state.connected);
        assert_eq!(snapshot.state.source.as_deref(), Some("sync"));
        assert_eq!(
            snapshot.torrent_values["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
                .get("sync_status")
                .map(String::as_str),
            Some("synced")
        );
        assert_eq!(
            snapshot.file_values["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"][&0]
                .get("peer_link")
                .map(String::as_str),
            Some("up")
        );
    }

    #[test]
    fn empty_body_means_connected_without_columns() {
        let snapshot = parse_snapshot("sync", None).unwrap();
        assert!(snapshot.state.connected);
        assert!(snapshot.state.columns.is_empty());
        assert!(snapshot.torrent_values.is_empty());
    }

    #[test]
    fn drops_unknown_keys_and_nonscalars() {
        let body = json!({
            "columns": [{ "key": "k" }],
            "torrents": { "h1": { "k": 42, "unknown": "x", "obj": {} } }
        });
        let snapshot = parse_snapshot("s", Some(&body)).unwrap();
        let values = &snapshot.torrent_values["h1"];
        assert_eq!(values.get("k").map(String::as_str), Some("42"));
        assert!(!values.contains_key("unknown"));
        assert!(!values.contains_key("obj"));
    }

    #[test]
    fn drops_empty_and_duplicate_keys_and_caps_count() {
        let body = json!({
            "columns": [
                { "key": "  " }, { "key": "a" }, { "key": "a", "name": "dup" },
                { "key": "b" }, { "key": "c" }, { "key": "d" }, { "key": "e" },
                { "key": "f" }, { "key": "g" }, { "key": "h" }, { "key": "i" }
            ]
        });
        let snapshot = parse_snapshot("s", Some(&body)).unwrap();
        assert_eq!(snapshot.state.columns.len(), MAX_COLUMNS);
        // The first "a" wins; the duplicate is dropped.
        assert_eq!(snapshot.state.columns[0].key, "a");
        assert!(!snapshot.state.columns.iter().any(|c| c.name == "dup"));
    }

    #[test]
    fn clamps_width_and_truncates_values() {
        let body = json!({
            "columns": [{ "key": "k", "width": 9999.0 }],
            "torrents": { "h": { "k": "x".repeat(500) } }
        });
        let snapshot = parse_snapshot("s", Some(&body)).unwrap();
        assert_eq!(snapshot.state.columns[0].width, Some(MAX_WIDTH));
        assert_eq!(snapshot.torrent_values["h"]["k"].len(), MAX_VALUE_LEN);
    }

    #[test]
    fn decoration_attaches_torrent_and_file_values() {
        let snapshot = parse_snapshot("sync", Some(&sample_response())).unwrap();
        let mut list = proto::TorrentList {
            torrents: vec![proto::TorrentDetails {
                info_hash: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                ..Default::default()
            }],
        };
        snapshot.decorate_list(&mut list);
        assert_eq!(
            list.torrents[0].custom.as_ref().unwrap()["sync_status"],
            "synced"
        );

        let mut details = proto::TorrentDetails {
            info_hash: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            files: Some(vec![
                proto::TorrentFile {
                    name: "a".into(),
                    components: vec![],
                    length: 0,
                    included: true,
                    padding: false,
                    custom: None,
                },
                proto::TorrentFile {
                    name: "b".into(),
                    components: vec![],
                    length: 0,
                    included: true,
                    padding: false,
                    custom: None,
                },
            ]),
            ..Default::default()
        };
        snapshot.decorate_details(&mut details);
        assert!(details.custom.is_some());
        assert!(details.files.as_ref().unwrap()[0].custom.is_some());
        assert!(details.files.as_ref().unwrap()[1].custom.is_none());
    }

    #[test]
    fn success_then_failure_flips_connected_and_keeps_data() {
        let mut store = Store::new(1);
        let h = hook("sync", "http://127.0.0.1:1/hook");
        store.apply_success(&h, Some(&sample_response())).unwrap();
        assert!(store.snapshot().unwrap().state.connected);

        assert_eq!(store.apply_failure(&h), FailureLog::Log);
        let snapshot = store.snapshot().unwrap();
        assert!(!snapshot.state.connected);
        // Last good values retained for instant reconnect.
        assert!(!snapshot.torrent_values.is_empty());

        // Repeated failure within the window is rate-limited.
        assert_eq!(store.apply_failure(&h), FailureLog::Silent);
    }

    #[test]
    fn success_after_failure_logs_reconnect_once() {
        let mut store = Store::new(1);
        let h = hook("sync", "http://127.0.0.1:1/hook");
        store.apply_success(&h, Some(&sample_response())).unwrap();
        store.apply_failure(&h);
        // Reconnecting after a failure is a transition worth logging…
        assert!(store.apply_success(&h, Some(&sample_response())).unwrap());
        // …staying connected is not.
        assert!(!store.apply_success(&h, Some(&sample_response())).unwrap());
    }

    #[test]
    fn non_object_body_is_a_parse_failure() {
        let body = serde_json::json!("OK");
        assert!(parse_snapshot("s", Some(&body)).is_err());
        let mut store = Store::new(1);
        let h = hook("sync", "http://127.0.0.1:1/hook");
        store.apply_success(&h, Some(&sample_response())).unwrap();
        // The provider routes a parse error to apply_failure (connected
        // flips off, error logged) — not silently swallowed.
        assert_eq!(store.apply_failure(&h), FailureLog::Log);
    }

    #[test]
    fn needs_files_follows_file_columns() {
        let mut store = Store::new(1);
        let h = hook("sync", "http://127.0.0.1:1/hook");
        store.apply_success(&h, Some(&sample_response())).unwrap();
        assert!(store.needs_files);

        let body = json!({ "columns": [{ "key": "only_torrent" }] });
        store.apply_success(&h, Some(&body)).unwrap();
        assert!(!store.needs_files);
    }

    #[tokio::test]
    async fn post_roundtrip_with_event_header() {
        let (addr, requests) = spawn_test_server(StatusCode::OK).await;
        let h = hook("sync", &format!("http://{addr}/hook"));
        let payload = json!({ "event": EVENT_NAME, "torrents": [] });
        let body = send_hook_request(&reqwest::Client::new(), &h, &payload)
            .await
            .expect("delivery");
        assert_eq!(body, Some(sample_response()));
        let (event, seen_payload) = requests.lock().pop().expect("request seen");
        assert_eq!(event.as_deref(), Some(EVENT_NAME));
        assert_eq!(seen_payload["event"], EVENT_NAME);
    }

    #[tokio::test]
    async fn http_error_is_an_error() {
        let (addr, _requests) = spawn_test_server(StatusCode::INTERNAL_SERVER_ERROR).await;
        let h = hook("sync", &format!("http://{addr}/hook"));
        let err = send_hook_request(&reqwest::Client::new(), &h, &json!({}))
            .await
            .expect_err("500 must be an error");
        assert!(err.to_string().contains("500"));
    }

    /// Spawn a local axum server that records `X-Rqbit-Event` headers and
    /// payloads and answers every POST with a fixed column response.
    async fn spawn_test_server(status: StatusCode) -> (std::net::SocketAddr, RequestLog) {
        let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
        let requests_for_state = requests.clone();
        let app = axum::Router::new().route(
            "/hook",
            post(move |headers: axum::http::HeaderMap, body: String| {
                let requests = requests_for_state.clone();
                async move {
                    let event = headers
                        .get("x-rqbit-event")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    let payload = serde_json::from_str(&body).unwrap_or_default();
                    requests.lock().push((event, payload));
                    (status, serde_json::to_string(&sample_response()).unwrap())
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr, requests)
    }
}
