//! Headless rqbit daemon core: exposes the `librqbit::Api` (the Rust layer,
//! not the REST bindings) to rqbit UI clients over a WebSocket speaking the
//! `rqbit-protocol` binary frames (postcard).
//!
//! Split into:
//! - [`server`]: the per-connection actor (request dispatch, change-diffed
//!   push sampling, log-line forwarding) and the request handler, which maps
//!   1:1 onto the `Api` methods the shared UI calls.
//! - `main.rs`: CLI, logging, session bootstrap, axum serving.

pub mod artwork;
pub mod columns;
pub mod embed;
pub mod media;
pub mod server;

use anyhow::Context as _;
use futures::future::BoxFuture;
use librqbit::dht::DhtPersistenceConfig;
use librqbit::limits::LimitsConfig;
use librqbit::{
    Api, ConnectionOptions, DhtSessionConfig, ListenerMode, ListenerOptions, PeerConnectionOptions,
    Session, SessionOptions, SessionPersistenceConfig,
    tracing_subscriber_config_utils::InitLoggingResult,
};
use parking_lot::RwLock;
use rqbit_protocol::SessionSettings;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use server::{ApiSlot, SessionSettingsBackend};

/// Session + API + log plumbing produced by [`start_session`].
pub struct Daemon {
    /// Swappable handle: the settings backend swaps it when the session is
    /// rebuilt, and live connections pick the new Api up per request.
    pub api: ApiSlot,
    /// The session settings the current session was built from; `get` for
    /// the settings backend.
    pub settings: Arc<RwLock<SessionSettings>>,
    /// Session-wide log-line broadcast; forwarded to WS clients that
    /// subscribe (`ClientMessage::SubscribeLogs`).
    pub line_broadcast: Option<librqbit::tracing_subscriber_config_utils::LineBroadcast>,
    #[allow(dead_code)] // kept alive so the broadcast channel keeps working
    pub init_logging: Arc<InitLoggingResult>,
}

/// Boot a librqbit session the same way the desktop app does.
///
/// `persistence`: `None` disables persistence; a `postgres://` string is
/// rejected here (the daemon v1 ships JSON persistence — pass a folder).
pub async fn start_session(
    download_folder: PathBuf,
    persistence_folder: Option<PathBuf>,
    disable_dht: bool,
    init_logging: Arc<InitLoggingResult>,
) -> anyhow::Result<Daemon> {
    let settings = SessionSettings {
        default_download_folder: download_folder.to_string_lossy().into_owned(),
        dht_enabled: !disable_dht,
        dht_persistence_enabled: true,
        dht_persistence_filename: String::new(),
        persistence_enabled: persistence_folder.is_some(),
        persistence_folder: persistence_folder
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default(),
        fastresume: true,
        // The daemon v1 does not open a BitTorrent listener (incoming peer
        // connections require port forwarding anyway); the settings page can
        // turn one on.
        enable_tcp_listen: false,
        enable_utp: false,
        enable_upnp_port_forward: false,
        listen_port: 4240,
        enable_tcp_outgoing: true,
        ..Default::default()
    };

    let api = build_session(&settings, &init_logging)
        .await
        .context("couldn't set up librqbit session")?;

    Ok(Daemon {
        api: ApiSlot::new(api),
        settings: Arc::new(RwLock::new(settings)),
        line_broadcast: Some(init_logging.line_broadcast.clone()),
        init_logging,
    })
}

/// Build a session + Api from wire settings. Shared by [`start_session`] and
/// the settings backend's rebuild path, so both map the settings identically.
async fn build_session(
    settings: &SessionSettings,
    init_logging: &InitLoggingResult,
) -> anyhow::Result<Arc<Api>> {
    let persistence = if !settings.persistence_enabled {
        None
    } else {
        Some(SessionPersistenceConfig::Json {
            folder: if settings.persistence_folder.is_empty() {
                None
            } else {
                Some(PathBuf::from(&settings.persistence_folder))
            },
        })
    };

    let dht = if !settings.dht_enabled {
        None
    } else {
        let persistence = if !settings.dht_persistence_enabled {
            None
        } else if settings.dht_persistence_filename.is_empty() {
            Some(DhtPersistenceConfig::default())
        } else {
            Some(DhtPersistenceConfig {
                config_filename: Some(PathBuf::from(&settings.dht_persistence_filename)),
                ..Default::default()
            })
        };
        Some(DhtSessionConfig {
            persistence,
            ..Default::default()
        })
    };

    let mode = match (settings.enable_tcp_listen, settings.enable_utp) {
        (true, true) => Some(ListenerMode::TcpAndUtp),
        (true, false) => Some(ListenerMode::TcpOnly),
        (false, true) => Some(ListenerMode::UtpOnly),
        (false, false) => None,
    };
    let listen = mode.map(|mode| ListenerOptions {
        mode,
        listen_addr: (Ipv4Addr::UNSPECIFIED, settings.listen_port).into(),
        enable_upnp_port_forwarding: settings.enable_upnp_port_forward,
        ..Default::default()
    });

    let connect = ConnectionOptions {
        proxy_url: if settings.socks_proxy.is_empty() {
            None
        } else {
            Some(settings.socks_proxy.clone())
        },
        enable_tcp: settings.enable_tcp_outgoing,
        peer_opts: Some(PeerConnectionOptions {
            connect_timeout: Some(Duration::from_secs(
                settings.peer_connect_timeout_secs as u64,
            )),
            read_write_timeout: Some(Duration::from_secs(
                settings.peer_read_write_timeout_secs as u64,
            )),
            ..Default::default()
        }),
    };

    let ratelimits = LimitsConfig {
        upload_bps: std::num::NonZeroU32::new(settings.upload_rate_limit_bps),
        download_bps: std::num::NonZeroU32::new(settings.download_rate_limit_bps),
    };

    let session = Session::new_with_opts(
        PathBuf::from(&settings.default_download_folder),
        SessionOptions {
            dht,
            persistence,
            listen,
            connect: Some(connect),
            ratelimits,
            fastresume: settings.fastresume,
            ..Default::default()
        },
    )
    .await
    .context("couldn't set up librqbit session")?;

    Ok(Arc::new(Api::new(
        session,
        Some(init_logging.rust_log_reload_tx.clone()),
        Some(init_logging.line_broadcast.clone()),
    )))
}

/// Session settings backend for the standalone daemon: rebuilds the session
/// in place (swapping the shared [`ApiSlot`]) when settings change, so live
/// connections pick the new session up without reconnecting. The log
/// broadcast is process-global, so log forwarding keeps working across
/// rebuilds.
#[derive(Clone)]
pub struct DaemonSettingsBackend {
    slot: ApiSlot,
    current: Arc<RwLock<SessionSettings>>,
    init_logging: Arc<InitLoggingResult>,
}

impl DaemonSettingsBackend {
    pub fn new(
        slot: ApiSlot,
        current: Arc<RwLock<SessionSettings>>,
        init_logging: Arc<InitLoggingResult>,
    ) -> Self {
        Self {
            slot,
            current,
            init_logging,
        }
    }
}

impl SessionSettingsBackend for DaemonSettingsBackend {
    fn get(&self) -> SessionSettings {
        self.current.read().clone()
    }

    fn set(&self, settings: SessionSettings) -> BoxFuture<'static, Result<(), String>> {
        let this = self.clone();
        Box::pin(async move {
            if settings.default_download_folder.trim().is_empty() {
                return Err("Default download folder must not be empty".to_string());
            }
            // Stop the old session first: a same-port listener would fail to
            // bind while the old session still holds it (the desktop's
            // reconfigure path does the same).
            this.slot.get().session().stop().await;
            match build_session(&settings, &this.init_logging).await {
                Ok(api) => {
                    this.slot.set(api);
                    *this.current.write() = settings;
                    Ok(())
                }
                Err(e) => Err(format!("error rebuilding session: {e:#}")),
            }
        })
    }
}
