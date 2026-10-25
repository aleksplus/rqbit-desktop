//! Wire protocol shared by the rqbit UI clients (native desktop, browser
//! wasm) and the headless daemon.
//!
//! This crate maps **the Rust API layer only** — the `librqbit::Api` methods
//! the desktop UI actually calls (see [`messages::Request`]) — not the REST
//! bindings. `librqbit` is third-party-maintained (used from crates.io plus a
//! small local patch — see the root `Cargo.toml`), so the payloads here are
//! *wire DTOs*: self-contained serde
//! types carrying exactly the fields the shared UI renders, shaped for a
//! binary non-self-describing codec (postcard) rather than librqbit's
//! JSON representation. Types that only native code needs (settings, hooks,
//! folder watching, the remaining Api surface) are imported from librqbit
//! directly and deliberately have no counterpart here.
//!
//! Layout:
//! - [`types`]: the wire DTOs.
//! - [`messages`]: `ClientMessage`/`ServerMessage` request/response/push frames.
//! - [`error`]: typed [`ProtoError`] carried inside responses.
//! - [`codec`]: postcard encode/decode over WebSocket binary frames.
//! - [`conv`] (feature `librqbit`): `From<librqbit::…>` conversions. This is
//!   the single mapping boundary; native hosts (desktop `LocalClient`,
//!   `app-daemon`) use it, the browser client builds without the feature and
//!   never links librqbit.
//!
//! Wire stability: postcard enums are tagged by variant index, so only
//! append new variants at the end of an enum; never reorder or remove.

pub mod codec;
#[cfg(feature = "librqbit")]
pub mod conv;
pub mod error;
pub mod messages;
pub mod types;

pub use error::{ProtoError, ProtoErrorKind};
pub use messages::{
    AddSource, ClientMessage, PushUpdate, Request, RequestId, ResponsePayload, ServerMessage,
    TorrentAction, TorrentId,
};
pub use types::{
    ApiAddTorrent, ConnKind, CustomColumnKind, CustomColumnSpec, CustomColumnsState, DhtStats,
    LiveStats, PeerEntry, PeerStats, PeerStatsSnapshot, SessionInfo, SessionStats, Speed,
    TorrentDetails, TorrentFile, TorrentList, TorrentState, TorrentStats,
};
