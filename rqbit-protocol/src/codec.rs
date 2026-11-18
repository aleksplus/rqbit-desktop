//! postcard codec for the WebSocket binary frames.
//!
//! One `ClientMessage`/`ServerMessage` per binary WS frame; no framing of
//! our own is needed on top.

use postcard::Error as PostcardError;

use crate::messages::{ClientMessage, ServerMessage};
use crate::types::TorrentId;

/// Codec failure. The message carries the postcard error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecError(pub String);

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "protocol codec error: {}", self.0)
    }
}

impl std::error::Error for CodecError {}

impl From<PostcardError> for CodecError {
    fn from(e: PostcardError) -> Self {
        CodecError(e.to_string())
    }
}

/// Serialize a client → daemon frame.
pub fn encode_client(msg: &ClientMessage) -> Result<Vec<u8>, CodecError> {
    postcard::to_allocvec(msg).map_err(Into::into)
}

/// Deserialize a client → daemon frame (daemon side).
pub fn decode_client(bytes: &[u8]) -> Result<ClientMessage, CodecError> {
    postcard::from_bytes(bytes).map_err(Into::into)
}

/// Serialize a daemon → client frame.
pub fn encode_server(msg: &ServerMessage) -> Result<Vec<u8>, CodecError> {
    postcard::to_allocvec(msg).map_err(Into::into)
}

/// Deserialize a daemon → client frame (client side).
pub fn decode_server(bytes: &[u8]) -> Result<ServerMessage, CodecError> {
    postcard::from_bytes(bytes).map_err(Into::into)
}

/// Convenience: `TorrentId` for a numeric id.
pub fn torrent_id(id: u64) -> TorrentId {
    TorrentId::Id(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ProtoError, ProtoErrorKind};
    use crate::messages::*;
    use crate::types::*;

    #[test]
    fn client_message_roundtrip() {
        let msgs = vec![
            ClientMessage::Request {
                id: 42,
                req: Request::List { with_stats: true },
            },
            ClientMessage::Request {
                id: 1,
                req: Request::PeerStats {
                    torrent: TorrentId::Hash("0123456789abcdef0123456789abcdef01234567".into()),
                },
            },
            ClientMessage::Request {
                id: 2,
                req: Request::AddTorrent {
                    source: AddSource::Magnet("magnet:?xt=urn:btih:abc".into()),
                    only_files: Some(vec![0, 2, 5]),
                    overwrite: true,
                    paused: false,
                    list_only: false,
                },
            },
            ClientMessage::Request {
                id: 3,
                req: Request::Action {
                    torrent: TorrentId::Id(7),
                    action: TorrentAction::UpdateOnlyFiles(vec![1, 3]),
                },
            },
            ClientMessage::Request {
                id: 5,
                req: Request::Action {
                    torrent: TorrentId::Id(7),
                    action: TorrentAction::CheckFiles,
                },
            },
            ClientMessage::Subscribe { torrent: None },
            ClientMessage::Subscribe {
                torrent: Some(TorrentId::Id(3)),
            },
            ClientMessage::Unsubscribe {
                torrent: Some(TorrentId::Id(3)),
            },
            ClientMessage::SubscribeLogs(true),
            ClientMessage::Request {
                id: 4,
                req: Request::SetRustLog("info,librqbit=debug".into()),
            },
            ClientMessage::Ping,
        ];
        for msg in msgs {
            let bytes = encode_client(&msg).unwrap();
            assert_eq!(decode_client(&bytes).unwrap(), msg);
        }
    }

    #[test]
    fn server_message_roundtrip() {
        let stats = TorrentStats {
            state: TorrentState::Initializing { paused: true },
            file_progress: vec![1, 2, 3],
            error: Some("boom".into()),
            progress_bytes: 100,
            uploaded_bytes: 5,
            total_bytes: 200,
            finished: false,
            live: None,
        };
        let msgs = vec![
            ServerMessage::Response {
                id: 42,
                result: Ok(ResponsePayload::List(Box::default())),
            },
            ServerMessage::Response {
                id: 43,
                result: Ok(ResponsePayload::Stats(Box::new(stats.clone()))),
            },
            ServerMessage::Response {
                id: 44,
                result: Err(ProtoError::new(
                    ProtoErrorKind::DhtDisabled,
                    "dht is disabled",
                )),
            },
            ServerMessage::Push(PushUpdate::TorrentStats {
                torrent: TorrentId::Id(9),
                stats: Box::new(stats),
            }),
            ServerMessage::Pong,
        ];
        for msg in msgs {
            let bytes = encode_server(&msg).unwrap();
            assert_eq!(decode_server(&bytes).unwrap(), msg);
        }
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(decode_client(&[0xff, 0xff, 0xff]).is_err());
        assert!(decode_server(b"").is_err());
    }
}
