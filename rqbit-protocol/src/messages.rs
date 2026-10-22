//! Request/response/push frames exchanged over the WebSocket.
//!
//! Every [`Request`] variant maps 1:1 onto a `librqbit::Api` method the
//! desktop UI calls — nothing more. Flow: the client sends
//! [`ClientMessage`] frames; every `ClientMessage::Request` is answered by
//! exactly one `ServerMessage::Response` carrying the same [`RequestId`].
//! Pushes (`ServerMessage::Push`) arrive only for channels the client
//! subscribed to via `Subscribe`/`Unsubscribe` and are never correlated to a
//! request id.

use serde::{Deserialize, Serialize};

use crate::error::ProtoError;
pub use crate::types::TorrentId;
use crate::types::{
    ApiAddTorrent, CustomColumnsState, DhtStats, PeerStatsSnapshot, SessionInfo, SessionStats,
    TorrentDetails, TorrentList, TorrentStats,
};

pub type RequestId = u64;

/// Client → daemon frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Issue a request; answered by `ServerMessage::Response` with this id.
    Request { id: RequestId, req: Request },
    /// Start receiving pushes. `None` subscribes to session-level pushes
    /// (torrent list + session stats); `Some(id)` additionally pushes that
    /// torrent's stats and peer updates. Subscriptions are per connection
    /// and reset on reconnect.
    Subscribe { torrent: Option<TorrentId> },
    /// Undo a previous `Subscribe`.
    Unsubscribe { torrent: Option<TorrentId> },
    /// Start/stop receiving `PushUpdate::LogLines` (the
    /// `Api::api_log_lines_stream` channel, forwarded as raw line bytes).
    SubscribeLogs(bool),
    /// Keepalive; answered with `ServerMessage::Pong`.
    Ping,
}

/// A single request. Each variant wraps exactly one `librqbit::Api` call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    /// `Api::api_torrent_list_ext` — torrent list, optionally with stats.
    List { with_stats: bool },
    /// `Api::api_torrent_details` — details incl. files and trackers.
    Details { torrent: TorrentId },
    /// `Api::api_stats_v1` — per-torrent stats incl. file progress.
    Stats { torrent: TorrentId },
    /// `Api::api_peer_stats` (all peers — the only mode the UI uses).
    PeerStats { torrent: TorrentId },
    /// `Api::api_session_stats`.
    SessionStats,
    /// Session-level info (e.g. the default download folder) via
    /// `Session::default_output_folder`.
    SessionInfo,
    /// `Api::api_dht_stats` — errors with
    /// [`crate::error::ProtoErrorKind::DhtDisabled`] when DHT is off.
    DhtStats,
    /// The `Api::api_torrent_action_*` methods.
    Action {
        torrent: TorrentId,
        action: TorrentAction,
    },
    /// `Api::api_add_torrent`. With `list_only: true` this only resolves the
    /// file list (the add dialog's preview) and answers
    /// `ResponsePayload::Added` with `id: None`.
    AddTorrent {
        source: AddSource,
        only_files: Option<Vec<u64>>,
        overwrite: bool,
        /// Start the torrent paused (used by autoload flows).
        paused: bool,
        /// Resolve metadata/files without adding.
        list_only: bool,
    },
    /// `Api::api_set_rust_log`.
    SetRustLog(String),
}

/// The torrent actions the UI dispatches (mirrors the
/// `Api::api_torrent_action_*` methods it calls).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TorrentAction {
    /// `api_torrent_action_pause`
    Pause,
    /// `api_torrent_action_start`
    Start,
    /// `api_torrent_action_forget` — remove the entry, keep the files.
    Forget,
    /// `api_torrent_action_delete` — remove the entry and the files.
    Delete,
    /// `api_torrent_action_update_only_files` — replace the file selection.
    UpdateOnlyFiles(Vec<u64>),
}

/// Where a torrent comes from. File bytes are intentionally absent — the
/// browser client cannot yet hand over picked files; magnet/URL covers the
/// agreed first scope and maps to `AddTorrent::from_url` (librqbit resolves
/// magnet and http(s) URLs identically).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AddSource {
    /// A `magnet:?…` URI.
    Magnet(String),
    /// An `http(s)://…` URL pointing at a .torrent file.
    Url(String),
}

/// Daemon → client frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Answer to a `ClientMessage::Request`.
    Response {
        id: RequestId,
        result: Result<ResponsePayload, ProtoError>,
    },
    /// Unsolicited update for a subscribed channel.
    Push(PushUpdate),
    /// Answer to `ClientMessage::Ping`.
    Pong,
}

/// Payload of a successful response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ResponsePayload {
    List(Box<TorrentList>),
    Details(Box<TorrentDetails>),
    Stats(Box<TorrentStats>),
    PeerStats(PeerStatsSnapshot),
    SessionStats(Box<SessionStats>),
    SessionInfo(Box<SessionInfo>),
    DhtStats(DhtStats),
    Added(Box<ApiAddTorrent>),
    /// Action succeeded, nothing to return.
    Empty,
}

/// Pushed updates. The daemon only sends these for active subscriptions and
/// only when the payload actually changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PushUpdate {
    /// Full torrent-list snapshot (rows carry stats, no files).
    TorrentList(Box<TorrentList>),
    /// Session-wide stats (footer).
    SessionStats(Box<SessionStats>),
    /// Subscribed torrent's stats (incl. per-file progress).
    TorrentStats {
        torrent: TorrentId,
        stats: Box<TorrentStats>,
    },
    /// Subscribed torrent's peer table snapshot.
    PeerStats {
        torrent: TorrentId,
        peers: PeerStatsSnapshot,
    },
    /// Log lines from `Api::api_log_lines_stream` (the log page's stream,
    /// pushed as raw tracing line bytes).
    LogLines(Vec<u8>),
    /// Hook-provided custom column definitions + connectivity. Sent when the
    /// encoded state changes; the rows of `TorrentList`/details responses
    /// carry the matching per-row values in their `custom` maps.
    CustomColumns(Box<CustomColumnsState>),
}
