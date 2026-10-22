//! Wire types for the Rust-API surface the shared UI consumes.
//!
//! Scope rules for this crate:
//! - One DTO set per `librqbit::Api` method the desktop UI actually calls
//!   (see `messages::Request`); nothing else from librqbit is mapped.
//! - Only fields the desktop-gpui views render are carried. Native-only
//!   concerns (settings, hooks, folder watching, the remaining Api methods)
//!   keep importing librqbit directly and never enter this crate.
//! - Payload types must round-trip through postcard, so plain externally
//!   tagged enums and explicit units (`*_secs`, `*_ms`, `mbps`) are used
//!   instead of librqbit's JSON-flavoured serde shapes.
//! - Ids are `u64` so the wire is identical between 64-bit desktop and
//!   32-bit `usize` wasm targets.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A torrent address on the wire. (librqbit's `TorrentIdOrHash` cannot be
/// used directly: its `Deserialize` calls `deserialize_any`, which
/// non-self-describing formats like postcard don't support.)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TorrentId {
    Id(u64),
    /// 40-char lowercase hex info hash.
    Hash(String),
}

impl TorrentId {
    /// The numeric id, when this is a [`TorrentId::Id`].
    pub fn as_id(&self) -> Option<u64> {
        match self {
            TorrentId::Id(id) => Some(*id),
            TorrentId::Hash(_) => None,
        }
    }
}

impl From<u64> for TorrentId {
    fn from(v: u64) -> Self {
        TorrentId::Id(v)
    }
}

impl From<usize> for TorrentId {
    fn from(v: usize) -> Self {
        TorrentId::Id(v as u64)
    }
}

/// Same lowercase strings as librqbit's `TorrentStatsState` `Display`
/// (`initializing`, `live`, `paused`, `error`) — the UI's status filter
/// matches on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TorrentState {
    Initializing { paused: bool },
    Live,
    Paused,
    Error,
}

impl std::fmt::Display for TorrentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TorrentState::Initializing { .. } => f.write_str("initializing"),
            TorrentState::Live => f.write_str("live"),
            TorrentState::Paused => f.write_str("paused"),
            TorrentState::Error => f.write_str("error"),
        }
    }
}

/// Download/upload speed, matching librqbit's `Speed::mbps` unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Speed {
    pub mbps: f64,
}

impl Speed {
    pub const ZERO: Speed = Speed { mbps: 0.0 };

    /// Bytes per second, mirroring librqbit's `Speed::as_bytes`.
    pub fn as_bytes(&self) -> u64 {
        (self.mbps * 1024f64 * 1024f64) as u64
    }
}

/// One file inside a torrent. `components` lets the native host reconstruct
/// the on-disk path (for the "file exists" hint); the browser client has no
/// filesystem and treats every file as not existing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TorrentFile {
    pub name: String,
    /// Path components relative to the output folder.
    pub components: Vec<String>,
    pub length: u64,
    /// Whether the file is part of the active `only_files` selection.
    pub included: bool,
    /// BEP-47 padding file (the only attribute the UI acts on).
    pub padding: bool,
    /// Hook-provided custom column values for this file, keyed by column key
    /// (`CustomColumnSpec::key`). `None`/empty = no hook data.
    #[serde(default)]
    pub custom: Option<BTreeMap<String, String>>,
}

/// Live (seeding/downloading) stats for a torrent. Carries exactly the
/// values the row/overview/ETA rendering reads out of librqbit's
/// `LiveStats`/`StatsSnapshot`/`AggregatePeerStats` chain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct LiveStats {
    /// Connected peers (aggregate `live` counter).
    pub peers_live: u32,
    /// Peers seen (aggregate `seen` counter).
    pub peers_seen: u32,
    pub download_speed: Speed,
    pub upload_speed: Speed,
    /// Estimated time to completion, in seconds.
    pub time_remaining_secs: Option<u64>,
}

/// Per-torrent stats — payload of `Api::api_stats_v1`.
/// `file_progress[i]` is the downloaded byte count of file `i`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TorrentStats {
    pub state: TorrentState,
    pub file_progress: Vec<u64>,
    pub error: Option<String>,
    pub progress_bytes: u64,
    pub uploaded_bytes: u64,
    pub total_bytes: u64,
    pub finished: bool,
    pub live: Option<LiveStats>,
}

/// A torrent entry — payload of `Api::api_torrent_details` and the rows of
/// `Api::api_torrent_list_ext`. List rows carry `files: None` and no
/// stats-selected extras; `trackers` is filled in for details.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TorrentDetails {
    pub id: Option<u64>,
    pub info_hash: String,
    pub name: Option<String>,
    pub output_folder: String,
    pub total_pieces: u32,
    pub files: Option<Vec<TorrentFile>>,
    pub stats: Option<Box<TorrentStats>>,
    pub comment: Option<String>,
    /// Announce URLs (from the managed torrent handle — the details payload
    /// librqbit returns has no trackers; the UI's Trackers tab reads them).
    pub trackers: Vec<String>,
    /// Hook-provided custom column values for this torrent, keyed by column
    /// key (`CustomColumnSpec::key`). `None`/empty = no hook data.
    #[serde(default)]
    pub custom: Option<BTreeMap<String, String>>,
}

/// How a hook-provided custom-column cell renders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CustomColumnKind {
    /// Render the value as plain text.
    #[default]
    Text,
    /// Render the value as a colored status badge (recognized values map to
    /// success / warning / danger tones, everything else stays plain text).
    Status,
}

/// One hook-provided column definition, served verbatim from the hook's
/// `torrent_columns` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomColumnSpec {
    /// Stable identifier; matches the keys of the per-torrent/per-file value
    /// maps on `TorrentDetails::custom` / `TorrentFile::custom`.
    pub key: String,
    /// Header label.
    pub name: String,
    /// The hook provides per-file values too (file table only).
    pub files: bool,
    pub kind: CustomColumnKind,
    /// Preferred column width in logical pixels; `None` = UI default.
    pub width: Option<f32>,
}

/// Custom column set + connectivity, pushed to (or polled by) the UIs. The
/// columns are only rendered while `connected` is true.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CustomColumnsState {
    /// Column definitions, in the order the hook declared them.
    pub columns: Vec<CustomColumnSpec>,
    /// Whether the providing hook answered the last poll.
    pub connected: bool,
    /// Name of the hook that provided the columns (status bar label).
    pub source: Option<String>,
}

/// Response of `Api::api_torrent_list_ext`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TorrentList {
    pub torrents: Vec<TorrentDetails>,
}

/// Connection kind of a live peer (librqbit's `ConnectionKind`; its `Display`
/// produces the same lowercase strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnKind {
    Tcp,
    Utp,
    Socks,
}

impl std::fmt::Display for ConnKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnKind::Tcp => f.write_str("tcp"),
            ConnKind::Utp => f.write_str("utp"),
            ConnKind::Socks => f.write_str("socks"),
        }
    }
}

/// One peer's stats — the fields the Peers tab renders out of
/// `PeerStats`/`PeerCounters` (address is the key in [`PeerStatsSnapshot`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerStats {
    /// Peer state name as produced by librqbit (e.g. "live", "queued").
    pub state: String,
    pub conn_kind: Option<ConnKind>,
    pub client_name: Option<String>,
    pub fetched_bytes: u64,
    pub uploaded_bytes: u64,
}

/// Peer table snapshot — payload of `Api::api_peer_stats`. A sorted `Vec`
/// rather than a map so serialized bytes are deterministic (the daemon's
/// change detection compares encoded bytes).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerStatsSnapshot {
    /// Sorted by `addr` ascending.
    pub peers: Vec<PeerEntry>,
}

/// A peer keyed by its `ip:port` address.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerEntry {
    pub addr: String,
    pub stats: PeerStats,
}

/// Session-wide footer stats — the fields the footer renders out of
/// `SessionStatsSnapshot` (speeds, totals, uptime).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionStats {
    pub download_speed: Speed,
    pub upload_speed: Speed,
    pub fetched_bytes: u64,
    pub uploaded_bytes: u64,
    pub uptime_seconds: u64,
}

/// DHT health — the routing-table sizes the footer indicator reads out of
/// `DhtStats`. "DHT disabled" is a `ProtoErrorKind::DhtDisabled` response,
/// not a stats value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DhtStats {
    pub routing_table_size: u64,
    pub routing_table_size_v6: u64,
}

/// Session-level configuration the UI needs. The torrent list uses
/// `default_download_folder` to keep torrents that merely sit in the
/// session's default download directory out of destination-folder grouping.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub default_download_folder: String,
}

/// Response of `Api::api_add_torrent`. `id: None` means list-only
/// (the add dialog's file-list preview).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ApiAddTorrent {
    pub id: Option<u64>,
    pub details: TorrentDetails,
    pub output_folder: String,
}
