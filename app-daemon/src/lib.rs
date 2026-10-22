//! Headless rqbit daemon core: exposes the `librqbit::Api` (the Rust layer,
//! not the REST bindings) to rqbit UI clients over a WebSocket speaking the
//! `rqbit-protocol` binary frames (postcard).
//!
//! Split into:
//! - [`server`]: the per-connection actor (request dispatch, change-diffed
//!   push sampling, log-line forwarding) and the request handler, which maps
//!   1:1 onto the `Api` methods the shared UI calls.
//! - `main.rs`: CLI, logging, session bootstrap, axum serving.

pub mod columns;
pub mod embed;
pub mod server;

use anyhow::Context as _;
use librqbit::dht::DhtPersistenceConfig;
use librqbit::{
    Api, DhtSessionConfig, Session, SessionOptions, SessionPersistenceConfig,
    tracing_subscriber_config_utils::InitLoggingResult,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Session + API + log plumbing produced by [`start_session`].
pub struct Daemon {
    pub api: Arc<Api>,
    /// Session-wide log-line broadcast; forwarded to WS clients that
    /// subscribe (`ClientMessage::SubscribeLogs`).
    pub line_broadcast: Option<librqbit::tracing_subscriber_config_utils::LineBroadcast>,
    #[allow(dead_code)] // kept alive so the broadcast channel keeps working
    pub init_logging: InitLoggingResult,
}

/// Boot a librqbit session the same way the desktop app does.
///
/// `persistence`: `None` disables persistence; a `postgres://` string is
/// rejected here (the daemon v1 ships JSON persistence — pass a folder).
pub async fn start_session(
    download_folder: PathBuf,
    persistence_folder: Option<PathBuf>,
    disable_dht: bool,
    init_logging: InitLoggingResult,
) -> anyhow::Result<Daemon> {
    let persistence = persistence_folder.map(|folder| SessionPersistenceConfig::Json {
        folder: if folder == Path::new("") {
            None
        } else {
            Some(folder)
        },
    });

    let dht = if disable_dht {
        None
    } else {
        Some(DhtSessionConfig {
            persistence: Some(DhtPersistenceConfig::default()),
            ..Default::default()
        })
    };

    let session = Session::new_with_opts(
        download_folder,
        SessionOptions {
            dht,
            persistence,
            // Outgoing connections only: the daemon does not open a
            // BitTorrent listener in v1 (incoming peer connections require
            // port forwarding anyway).
            listen: None,
            fastresume: true,
            ..Default::default()
        },
    )
    .await
    .context("couldn't set up librqbit session")?;

    let api = Arc::new(Api::new(
        session,
        Some(init_logging.rust_log_reload_tx.clone()),
        Some(init_logging.line_broadcast.clone()),
    ));

    Ok(Daemon {
        api,
        line_broadcast: Some(init_logging.line_broadcast.clone()),
        init_logging,
    })
}
