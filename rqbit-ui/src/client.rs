//! Transport-agnostic torrent client used by the shared views.
//!
//! The views in this crate never touch librqbit or the network directly:
//! they talk to an `Arc<dyn TorrentClient>`. Two hosts provide
//! implementations:
//! - desktop: `LocalClient` (in desktop-gpui) wrapping the in-process
//!   `Arc<librqbit::Api>`, converting through `rqbit_protocol::conv`;
//! - browser: `RemoteClient` (in web-gpui) speaking
//!   `rqbit_protocol` frames over a WebSocket to the daemon.
//!
//! Futures are `?Send`: gpui's UI executor is single-threaded per window and
//! the web client's futures hold `JsValue`s.

use async_trait::async_trait;
use futures::stream::LocalBoxStream;

use rqbit_protocol::{
    AddSource, ApiAddTorrent, ClientMessage, CustomColumnsState, DhtStats, PeerStatsSnapshot,
    ProtoError, PushUpdate, SessionSettings, SessionStats, TorrentAction, TorrentDetails,
    TorrentId, TorrentList, TorrentStats,
};

/// Result type all client methods return.
pub type ClientResult<T> = Result<T, ProtoError>;

/// Liveness of the transport between the UI and the backend, as reported by
/// [`TorrentClient::connection_state`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// No connection has been established yet: the initial connect (or a
    /// reconnect after the backend disappeared) is still in flight.
    Connecting,
    /// The transport is up; requests and pushed updates flow.
    Connected,
    /// The transport dropped — e.g. the backend process was closed — and the
    /// client is retrying in the background.
    Disconnected,
}

/// Where a torrent to add comes from, in UI terms. All three travel the
/// wire (picked-file bytes included, so the browser client can add .torrent
/// files through its file picker).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAddSource {
    /// A `magnet:?…` URI (or a bare 40-char info hash, same as the desktop
    /// magnet dialog accepted).
    Magnet(String),
    /// An `http(s)://…` URL pointing at a .torrent file.
    Url(String),
    /// Raw .torrent file bytes.
    Bytes(Vec<u8>),
}

impl UiAddSource {
    /// The wire form.
    pub fn to_wire(&self) -> Result<AddSource, ProtoError> {
        match self {
            UiAddSource::Magnet(s) => Ok(AddSource::Magnet(s.clone())),
            UiAddSource::Url(s) => Ok(AddSource::Url(s.clone())),
            UiAddSource::Bytes(bytes) => Ok(AddSource::Bytes(bytes.clone())),
        }
    }
}

/// Parameters for adding a torrent. Mirrors the inputs of the desktop's
/// `HookDispatcher::add_and_dispatch_added`; `source_hint` never travels the
/// wire (it is the local picked-file path the desktop moves after adding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddTorrentRequest {
    pub source: UiAddSource,
    /// Selected file indices; `None` = all files.
    pub only_files: Option<Vec<u64>>,
    pub overwrite: bool,
    pub paused: bool,
    /// Resolve metadata/file list without adding.
    pub list_only: bool,
    /// Opaque host context (desktop: the local source file path).
    pub source_hint: Option<String>,
}

/// The API surface the shared UI consumes — exactly the `librqbit::Api`
/// methods the desktop views call, nothing more.
#[async_trait(?Send)]
pub trait TorrentClient: 'static {
    /// `Api::api_torrent_list_ext`.
    async fn list(&self, with_stats: bool) -> ClientResult<TorrentList>;
    /// `Api::api_torrent_details` (includes trackers).
    async fn details(&self, torrent: &TorrentId) -> ClientResult<TorrentDetails>;
    /// `Api::api_stats_v1`.
    async fn stats(&self, torrent: &TorrentId) -> ClientResult<TorrentStats>;
    /// `Api::api_peer_stats` (all peers).
    async fn peer_stats(&self, torrent: &TorrentId) -> ClientResult<PeerStatsSnapshot>;
    /// `Api::api_session_stats`.
    async fn session_stats(&self) -> ClientResult<SessionStats>;
    /// Session-level info — the default download folder, used to keep
    /// torrents that merely sit in the session's default download directory
    /// out of destination-folder grouping in the torrent table. `None` (the
    /// default) means "unknown / not supported by this client".
    async fn session_default_folder(&self) -> ClientResult<Option<String>> {
        Ok(None)
    }
    /// `Api::api_dht_stats` — `Err` with
    /// [`rqbit_protocol::ProtoErrorKind::DhtDisabled`] when DHT is off.
    async fn dht_stats(&self) -> ClientResult<DhtStats>;
    /// Hook-provided custom columns + connectivity (`None` = no column
    /// hook is configured). Only consulted by poll-mode clients; push-mode
    /// clients receive [`PushUpdate::CustomColumns`] instead.
    async fn custom_columns(&self) -> Option<CustomColumnsState> {
        None
    }
    /// The `Api::api_torrent_action_*` methods.
    async fn action(&self, torrent: &TorrentId, action: TorrentAction) -> ClientResult<()>;
    /// `Api::api_add_torrent`. With `list_only: true` this resolves the file
    /// list only and returns `id: None`.
    async fn add_torrent(&self, req: &AddTorrentRequest) -> ClientResult<ApiAddTorrent>;
    /// `Api::api_set_rust_log`.
    async fn set_rust_log(&self, value: String) -> ClientResult<()>;
    /// Read the session settings the host manages
    /// ([`SessionSettings`]). Errors when the transport or the host has no
    /// settings backend (e.g. the desktop's in-process client — the desktop
    /// keeps its own full settings page).
    async fn get_session_settings(&self) -> ClientResult<SessionSettings> {
        Err(ProtoError::bad_request(
            "session settings are not supported by this client",
        ))
    }
    /// Replace the session settings. The host rebuilds its session; the
    /// connection may drop and reconnect as part of this.
    async fn set_session_settings(&self, _settings: &SessionSettings) -> ClientResult<()> {
        Err(ProtoError::bad_request(
            "session settings are not supported by this client",
        ))
    }
    /// `Api::api_log_lines_stream` — raw log line bytes. Errors when the
    /// stream is unavailable (e.g. not wired up on the daemon).
    fn log_lines(&self) -> ClientResult<LocalBoxStream<'static, Vec<u8>>>;

    /// Whether this client pushes state updates on its own. `false` (the
    /// default, local in-process clients) makes the UI poll on a timer;
    /// `true` (remote clients) makes it consume [`TorrentClient::updates`].
    fn is_push(&self) -> bool {
        false
    }

    /// Stream of pushed updates ([`PushUpdate::TorrentList`] and
    /// [`PushUpdate::SessionStats`]). Only consumed when [`Self::is_push`].
    fn updates(&self) -> LocalBoxStream<'static, PushUpdate> {
        Box::pin(futures::stream::empty())
    }

    /// Stream of transport liveness ([`ConnectionState`]). Emits the current
    /// state right away, then once per change. The default (in-process
    /// clients) is an empty stream: the UI then assumes
    /// [`ConnectionState::Connected`] forever.
    fn connection_state(&self) -> LocalBoxStream<'static, ConnectionState> {
        Box::pin(futures::stream::empty())
    }
}

/// Encode a subscribe-to-session-updates message (used by remote clients).
pub fn subscribe_session() -> ClientMessage {
    ClientMessage::Subscribe { torrent: None }
}
