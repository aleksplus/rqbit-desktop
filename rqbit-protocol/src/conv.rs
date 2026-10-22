//! `From<librqbit::…>` conversions into the wire types.
//!
//! This is the single mapping boundary between librqbit's in-process API
//! types and the protocol. Native hosts (desktop `LocalClient`,
//! `app-daemon`) use it; the browser client builds without this module and
//! never links librqbit.
//!
//! Note on nameability: librqbit's `torrent_state`/`stream_connect` modules
//! are private, so a few field types (`StatsSnapshot`, `Speed`,
//! `ConnectionKind`, the gen_stats snapshots) cannot be named from outside —
//! only accessed through public fields. Conversions below use `&_` closures
//! for those, and map `ConnectionKind` via its `Display` impl.

use librqbit::api::{
    ApiAddTorrentResponse, LiveStats as LibrqbitLiveStats,
    PeerStatsSnapshot as LibrqbitPeerStatsSnapshot, TorrentDetailsResponse, TorrentIdOrHash,
    TorrentListResponse,
};
use librqbit::dht::DhtStats as LibrqbitDhtStats;
use librqbit::session_stats::snapshot::SessionStatsSnapshot;
use librqbit::{TorrentStats as LibrqbitTorrentStats, TorrentStatsState};

use crate::error::{ProtoError, ProtoErrorKind};
use crate::messages::{AddSource, TorrentId};
use crate::types::{
    ApiAddTorrent, ConnKind, DhtStats, LiveStats, PeerEntry, PeerStats, PeerStatsSnapshot,
    SessionStats, Speed, TorrentDetails, TorrentFile, TorrentList, TorrentState, TorrentStats,
};

// --- ids ---

impl From<&TorrentIdOrHash> for TorrentId {
    fn from(v: &TorrentIdOrHash) -> Self {
        match v {
            TorrentIdOrHash::Id(id) => TorrentId::Id(*id as u64),
            TorrentIdOrHash::Hash(h) => TorrentId::Hash(h.as_string()),
        }
    }
}

impl From<TorrentIdOrHash> for TorrentId {
    fn from(v: TorrentIdOrHash) -> Self {
        Self::from(&v)
    }
}

impl TorrentId {
    /// Convert to librqbit's id-or-hash. Hash strings are validated by
    /// parsing; a bad hash is a [`ProtoErrorKind::BadRequest`].
    pub fn to_api(&self) -> Result<TorrentIdOrHash, ProtoError> {
        match self {
            TorrentId::Id(id) => Ok(TorrentIdOrHash::Id(*id as usize)),
            TorrentId::Hash(h) => TorrentIdOrHash::parse(h)
                .map_err(|e| ProtoError::bad_request(format!("invalid torrent id {h:?}: {e:#}"))),
        }
    }
}

// --- errors ---

/// Map an [`librqbit::ApiError`] to a wire error by HTTP status, the only
/// stable discriminator (the error kind enum is private upstream).
/// `not_found_is` lets the DHT call site map its 404 ("dht disabled") to
/// [`ProtoErrorKind::DhtDisabled`] instead of "torrent not found".
pub fn proto_error_from_api_error(
    e: &librqbit::ApiError,
    not_found_is: ProtoErrorKind,
) -> ProtoError {
    let kind = match e.status().as_u16() {
        404 => not_found_is,
        400 => ProtoErrorKind::BadRequest,
        412 => ProtoErrorKind::TorrentNotLive,
        _ => ProtoErrorKind::Internal,
    };
    ProtoError::new(kind, e.to_string())
}

// --- torrents ---

/// Convert a details response, attaching tracker URLs resolved from the
/// managed torrent handle (librqbit's details response has no trackers; the
/// UI's Trackers tab reads them via `mgr_handle().shared().trackers`).
pub fn torrent_details(resp: &TorrentDetailsResponse, trackers: Vec<String>) -> TorrentDetails {
    TorrentDetails {
        id: resp.id.map(|id| id as u64),
        info_hash: resp.info_hash.clone(),
        name: resp.name.clone(),
        output_folder: resp.output_folder.clone(),
        total_pieces: resp.total_pieces,
        files: resp.files.as_ref().map(|files| {
            files
                .iter()
                .map(|f| TorrentFile {
                    name: f.name.clone(),
                    components: f.components.clone(),
                    length: f.length,
                    included: f.included,
                    padding: f.attributes.padding,
                    custom: None,
                })
                .collect()
        }),
        stats: resp.stats.as_ref().map(|s| Box::new(TorrentStats::from(s))),
        comment: resp.comment.clone(),
        trackers,
        custom: None,
    }
}

impl From<&TorrentListResponse> for TorrentList {
    fn from(resp: &TorrentListResponse) -> Self {
        TorrentList {
            torrents: resp
                .torrents
                .iter()
                .map(|t| torrent_details(t, Vec::new()))
                .collect(),
        }
    }
}

impl From<&ApiAddTorrentResponse> for ApiAddTorrent {
    fn from(resp: &ApiAddTorrentResponse) -> Self {
        ApiAddTorrent {
            id: resp.id.map(|id| id as u64),
            details: torrent_details(&resp.details, Vec::new()),
            output_folder: resp.output_folder.clone(),
        }
    }
}

// --- stats ---

impl From<&TorrentStatsState> for TorrentState {
    fn from(state: &TorrentStatsState) -> Self {
        match state {
            TorrentStatsState::Initializing { paused } => {
                TorrentState::Initializing { paused: *paused }
            }
            TorrentStatsState::Live => TorrentState::Live,
            TorrentStatsState::Paused => TorrentState::Paused,
            TorrentStatsState::Error => TorrentState::Error,
        }
    }
}

impl From<&LibrqbitTorrentStats> for TorrentStats {
    fn from(s: &LibrqbitTorrentStats) -> Self {
        TorrentStats {
            state: TorrentState::from(&s.state),
            file_progress: s.file_progress.clone(),
            error: s.error.clone(),
            progress_bytes: s.progress_bytes,
            uploaded_bytes: s.uploaded_bytes,
            total_bytes: s.total_bytes,
            finished: s.finished,
            live: s.live.as_ref().map(LiveStats::from),
        }
    }
}

impl From<&LibrqbitLiveStats> for LiveStats {
    fn from(l: &LibrqbitLiveStats) -> Self {
        // The aggregate counters the row/overview rendering reads are
        // `snapshot.peer_stats.live` and `.seen`; the rest of librqbit's
        // snapshot is never surfaced in the UI, so it is not carried.
        LiveStats {
            peers_live: l.snapshot.peer_stats.live,
            peers_seen: l.snapshot.peer_stats.seen,
            download_speed: Speed {
                mbps: l.download_speed.mbps,
            },
            upload_speed: Speed {
                mbps: l.upload_speed.mbps,
            },
            time_remaining_secs: l.time_remaining.as_ref().map(|t| t.as_secs()),
        }
    }
}

// --- session stats ---

impl From<&SessionStatsSnapshot> for SessionStats {
    fn from(s: &SessionStatsSnapshot) -> Self {
        SessionStats {
            download_speed: Speed {
                mbps: s.download_speed.mbps,
            },
            upload_speed: Speed {
                mbps: s.upload_speed.mbps,
            },
            fetched_bytes: s.counters.fetched_bytes,
            uploaded_bytes: s.counters.uploaded_bytes,
            uptime_seconds: s.uptime_seconds,
        }
    }
}

// --- peers ---

impl From<&LibrqbitPeerStatsSnapshot> for PeerStatsSnapshot {
    fn from(s: &LibrqbitPeerStatsSnapshot) -> Self {
        let mut peers: Vec<PeerEntry> = s
            .peers
            .iter()
            .map(|(addr, p)| PeerEntry {
                addr: addr.clone(),
                stats: PeerStats {
                    state: p.state.to_string(),
                    // ConnectionKind is not nameable upstream; its Display
                    // impl produces exactly "tcp"/"utp"/"socks".
                    conn_kind: p.conn_kind.map(|k| match k.to_string().as_str() {
                        "tcp" => ConnKind::Tcp,
                        "utp" => ConnKind::Utp,
                        _ => ConnKind::Socks,
                    }),
                    client_name: p.client_name.clone(),
                    fetched_bytes: p.counters.fetched_bytes,
                    uploaded_bytes: p.counters.uploaded_bytes,
                },
            })
            .collect();
        // Deterministic order for byte-diff change detection and stable UI.
        peers.sort_by(|a, b| a.addr.cmp(&b.addr));
        PeerStatsSnapshot { peers }
    }
}

// --- dht ---

impl From<&LibrqbitDhtStats> for DhtStats {
    fn from(s: &LibrqbitDhtStats) -> Self {
        DhtStats {
            routing_table_size: s.routing_table_size as u64,
            routing_table_size_v6: s.routing_table_size_v6 as u64,
        }
    }
}

// --- add torrent ---

impl From<&AddSource> for librqbit::AddTorrent<'static> {
    fn from(source: &AddSource) -> Self {
        match source {
            // `from_url` covers both http(s) and magnet URIs; librqbit
            // resolves them identically (same path the desktop dialog uses).
            AddSource::Magnet(s) => librqbit::AddTorrent::from_url(s.clone()),
            AddSource::Url(s) => librqbit::AddTorrent::from_url(s.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode_server, encode_server};
    use crate::messages::{ResponsePayload, ServerMessage};
    use std::collections::HashMap;

    /// A converted stats payload must survive a full encode/decode cycle —
    /// this is the property the whole protocol rests on for the daemon →
    /// browser hop. `TorrentStats` is constructible here because every field
    /// type is nameable (`live: None` avoids the unnameable inner snapshot).
    #[test]
    fn converted_torrent_stats_survive_wire_roundtrip() {
        let api_stats = LibrqbitTorrentStats {
            state: TorrentStatsState::Initializing { paused: true },
            file_progress: vec![1, 2, 3],
            error: Some("boom".into()),
            progress_bytes: 100,
            uploaded_bytes: 5,
            total_bytes: 200,
            finished: false,
            live: None,
        };
        let dto = TorrentStats::from(&api_stats);
        assert_eq!(dto.state, TorrentState::Initializing { paused: true });
        assert_eq!(dto.total_bytes, 200);

        let msg = ServerMessage::Response {
            id: 1,
            result: Ok(ResponsePayload::Stats(Box::new(dto))),
        };
        let bytes = encode_server(&msg).unwrap();
        match decode_server(&bytes).unwrap() {
            ServerMessage::Response { id, result } => {
                assert_eq!(id, 1);
                match result.unwrap() {
                    ResponsePayload::Stats(stats) => {
                        assert_eq!(stats.state, TorrentState::Initializing { paused: true });
                        assert_eq!(stats.file_progress, vec![1, 2, 3]);
                        assert_eq!(stats.error.as_deref(), Some("boom"));
                    }
                    other => panic!("unexpected payload: {other:?}"),
                }
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn torrent_id_hash_roundtrip_through_api_type() {
        let hex = "0123456789abcdef0123456789abcdef01234567";
        let wire = TorrentId::Hash(hex.to_string());
        let api = wire.to_api().unwrap();
        let back: TorrentId = api.into();
        assert_eq!(back, wire);
        assert!(matches!(
            TorrentId::Hash("nope".into()).to_api(),
            Err(ProtoError { .. })
        ));
    }

    #[test]
    fn peer_snapshot_conversion_sorts_addresses() {
        let mk = |state: &'static str| librqbit::api::PeerStats {
            counters: librqbit::api::PeerCounters {
                fetched_bytes: 10,
                uploaded_bytes: 20,
                incoming_connections: 0,
                total_time_connecting_ms: 0,
                connection_attempts: 0,
                connections: 0,
                errors: 0,
                fetched_chunks: 0,
                downloaded_and_checked_pieces: 0,
                total_piece_download_ms: 0,
                times_stolen_from_me: 0,
                times_i_stole: 0,
            },
            state,
            conn_kind: None,
            client_name: None,
        };
        let mut peers = HashMap::new();
        peers.insert("9.9.9.9:1".to_string(), mk("queued"));
        peers.insert("1.1.1.1:2".to_string(), mk("live"));
        let snapshot = LibrqbitPeerStatsSnapshot { peers };

        let dto = PeerStatsSnapshot::from(&snapshot);
        let addrs: Vec<&str> = dto.peers.iter().map(|p| p.addr.as_str()).collect();
        assert_eq!(addrs, vec!["1.1.1.1:2", "9.9.9.9:1"]);
        assert_eq!(dto.peers[0].stats.state, "live");
        assert_eq!(dto.peers[0].stats.fetched_bytes, 10);
        assert_eq!(dto.peers[0].stats.uploaded_bytes, 20);
    }

    #[test]
    fn error_mapping_by_status() {
        let e = librqbit::ApiError::dht_disabled();
        assert_eq!(
            proto_error_from_api_error(&e, ProtoErrorKind::DhtDisabled).kind,
            ProtoErrorKind::DhtDisabled
        );
        assert_eq!(
            proto_error_from_api_error(&e, ProtoErrorKind::TorrentNotFound).kind,
            ProtoErrorKind::TorrentNotFound
        );
        let msg = ServerMessage::Response {
            id: 5,
            result: Err(ProtoError::new(
                ProtoErrorKind::TorrentNotFound,
                String::from("gone"),
            )),
        };
        let bytes = encode_server(&msg).unwrap();
        assert!(matches!(
            decode_server(&bytes).unwrap(),
            ServerMessage::Response {
                id: 5,
                result: Err(ProtoError {
                    kind: ProtoErrorKind::TorrentNotFound,
                    ..
                })
            }
        ));
    }
}
