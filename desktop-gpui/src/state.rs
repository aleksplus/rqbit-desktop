use crate::config::{HttpMethod, RqbitDesktopConfig, read_config, write_config};
use crate::folder_watcher::start_folder_watcher;
use crate::hooks::{HookDispatcher, HookEvent};
use app_daemon::columns::{ColumnsHook, ColumnsMethod, ColumnsProvider};
use librqbit::{api::Api, tracing_subscriber_config_utils::InitLoggingResult};
use librqbit_dualstack_sockets::TcpListener;
use std::{path::Path, sync::Arc};

use parking_lot::RwLock;

use anyhow::Context;
use librqbit::{
    ApiError, DhtSessionConfig, Session, SessionOptions, SessionPersistenceConfig,
    dht::DhtPersistenceConfig,
};
use tracing::{debug_span, error, warn};

#[derive(Clone)]
pub struct SharedState {
    config: RqbitDesktopConfig,
    api: Arc<Api>,
    /// Hook-provided custom columns (the `torrent_columns` event), `None`
    /// when no hook provides columns. Swapped together with the session on
    /// reconfigure; `LocalClient` re-reads it per call.
    columns: Option<Arc<ColumnsProvider>>,
}

impl SharedState {
    pub fn api(&self) -> Arc<Api> {
        self.api.clone()
    }

    /// Direct constructor for tests (the normal path builds the API from a
    /// config, which starts listeners).
    #[cfg(test)]
    pub(crate) fn for_test(config: RqbitDesktopConfig, api: Arc<Api>) -> Self {
        Self {
            config,
            api,
            columns: None,
        }
    }

    pub fn config(&self) -> RqbitDesktopConfig {
        self.config.clone()
    }

    /// The custom-column provider, when a hook provides columns.
    pub fn columns(&self) -> Option<Arc<ColumnsProvider>> {
        self.columns.clone()
    }

    /// Replace the in-memory config snapshot (does not persist to disk).
    pub fn set_config(&mut self, config: RqbitDesktopConfig) {
        self.config = config;
    }
}

pub struct State {
    pub config_filename: String,
    shared: Arc<RwLock<SharedState>>,
    init_logging: Arc<InitLoggingResult>,
    hooks: Arc<HookDispatcher>,
}

impl Clone for State {
    fn clone(&self) -> Self {
        Self {
            config_filename: self.config_filename.clone(),
            shared: self.shared.clone(),
            init_logging: self.init_logging.clone(),
            hooks: self.hooks.clone(),
        }
    }
}

/// A session's Api plus the hook-columns provider built alongside it.
struct SessionApi {
    api: Api,
    columns: Option<Arc<ColumnsProvider>>,
}

/// Hooks subscribed to `torrent_columns`, converted for the daemon-side
/// provider. Disabled hooks and hooks without a URL are skipped.
fn column_hooks(config: &RqbitDesktopConfig) -> Vec<ColumnsHook> {
    config
        .hooks
        .hooks
        .iter()
        .filter(|h| {
            h.enabled && h.events.contains(&HookEvent::TorrentColumns) && !h.url.trim().is_empty()
        })
        .map(|h| ColumnsHook {
            name: h.name.clone(),
            url: h.url.clone(),
            method: match h.method {
                HttpMethod::Post => ColumnsMethod::Post,
                HttpMethod::Get => ColumnsMethod::Get,
            },
            timeout_secs: h.timeout_secs,
            headers: h.headers.clone(),
            poll_secs: h.poll_secs,
        })
        .collect()
}

async fn api_from_config(
    init_logging: &InitLoggingResult,
    config: &RqbitDesktopConfig,
) -> anyhow::Result<SessionApi> {
    config
        .validate()
        .context("error validating configuration")?;
    let persistence = if config.persistence.disable {
        None
    } else {
        Some(SessionPersistenceConfig::Json {
            folder: if config.persistence.folder == Path::new("") {
                None
            } else {
                Some(config.persistence.folder.clone())
            },
        })
    };

    let (listen, connect) = config.connections.as_listener_and_connect_opts();

    let mut http_api_opts = librqbit::http_api::HttpApiOptions {
        read_only: config.http_api.read_only,
        basic_auth: None,
        ..Default::default()
    };

    // We need to start prometheus recorder earlier than session.
    if !config.http_api.disable {
        match metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder() {
            Ok(handle) => {
                http_api_opts.prometheus_handle = Some(handle);
            }
            Err(e) => {
                warn!("error installting prometheus recorder: {e:#}");
            }
        }
    }

    let dht = if config.dht.disable {
        None
    } else {
        let persistence = if config.dht.disable_persistence {
            None
        } else {
            Some(DhtPersistenceConfig {
                config_filename: Some(config.dht.persistence_filename.clone()),
                ..Default::default()
            })
        };
        Some(DhtSessionConfig {
            persistence,
            ..Default::default()
        })
    };

    let session = Session::new_with_opts(
        config.default_download_location.clone(),
        SessionOptions {
            dht,
            persistence,
            connect: Some(connect),
            listen,
            fastresume: config.persistence.fastresume,
            ratelimits: config.ratelimits,
            #[cfg(feature = "disable-upload")]
            disable_upload: config.disable_upload,
            ..Default::default()
        },
    )
    .await
    .context("couldn't set up librqbit session")?;

    let api = Api::new(
        session.clone(),
        Some(init_logging.rust_log_reload_tx.clone()),
        Some(init_logging.line_broadcast.clone()),
    );

    if !config.http_api.disable {
        let listen_addr = config.http_api.listen_addr;
        let api = api.clone();
        let upnp_router = if config.upnp.enable_server {
            let friendly_name = config
                .upnp
                .server_friendly_name
                .as_ref()
                .map(|f| f.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_owned())
                .unwrap_or_else(|| {
                    format!(
                        "rqbit-desktop@{}",
                        gethostname::gethostname().to_string_lossy()
                    )
                });

            let mut upnp_adapter = session
                .make_upnp_adapter(friendly_name, config.http_api.listen_addr.port())
                .await
                .context("error starting UPnP server")?;
            let router = upnp_adapter.take_router()?;
            session.spawn(debug_span!("ssdp"), "ssdp", async move {
                upnp_adapter.run_ssdp_forever().await
            });
            Some(router)
        } else {
            None
        };
        let http_api_task = async move {
            let listener = TcpListener::bind_tcp(listen_addr, Default::default())
                .with_context(|| format!("error listening on {}", listen_addr))?;
            librqbit::http_api::HttpApi::new(api.clone(), Some(http_api_opts))
                .make_http_api_and_run(listener, upnp_router)
                .await
        };

        session.spawn(debug_span!("http_api"), "http_api", http_api_task);
    }

    // Hook-provided custom columns: polled for as long as this session
    // lives, served to the web view over the WS protocol and read by
    // LocalClient for the native UI.
    let column_hooks = column_hooks(config);
    let columns = if column_hooks.is_empty() {
        None
    } else {
        let provider = ColumnsProvider::new(Arc::new(api.clone()), column_hooks);
        let poller = provider.clone();
        session.spawn(debug_span!("hook_columns"), "hook_columns", async move {
            poller.run().await;
            Ok(())
        });
        Some(provider)
    };

    // The bundled web view: the same WS protocol (`/ws`) and embedded browser
    // client (`/`) `app-daemon` serves, pointed at this session. One librqbit
    // instance then serves both the native UI and the browser UI. Spawned on
    // the session so a settings reconfigure stops and rebinds it like the
    // HTTP API above.
    if !config.web_view.disable {
        if !app_daemon::embed::APP_BUILT {
            warn!(
                "web view enabled but the browser client payload is not built; \
                 run `make web-client-build` and restart. Serving a placeholder page."
            );
        }
        let listen_addr = config.web_view.listen_addr;
        let api = api.clone();
        let line_broadcast = init_logging.line_broadcast.clone();
        let columns_for_router = columns.clone();
        let web_view_task = async move {
            let router =
                app_daemon::server::router(Arc::new(api), Some(line_broadcast), columns_for_router);
            let listener = tokio::net::TcpListener::bind(listen_addr)
                .await
                .with_context(|| format!("error listening on {listen_addr}"))?;
            tracing::info!(addr = %listen_addr, "web view listening");
            axum::serve(listener, router)
                .await
                .context("web view server error")
        };
        session.spawn(debug_span!("web_view"), "web_view", web_view_task);
    }
    Ok(SessionApi { api, columns })
}

impl State {
    pub async fn new(init_logging: InitLoggingResult) -> anyhow::Result<Self> {
        let config_filename = directories::ProjectDirs::from("com", "rqbit", "desktop")
            .expect("directories::ProjectDirs::from")
            .config_dir()
            .join("config.json")
            .to_str()
            .expect("to_str()")
            .to_owned();

        let config = read_config(&config_filename).unwrap_or_default();

        let session_api = api_from_config(&init_logging, &config)
            .await
            .map_err(|e| {
                warn!(error=?e, "error reading configuration");
                e
            })
            .context("error creating state")?;

        let shared = Arc::new(RwLock::new(SharedState {
            config,
            api: Arc::new(session_api.api),
            columns: session_api.columns,
        }));

        // The hook dispatcher reads hooks from the shared config at dispatch
        // time and runs its completion watcher on the tokio runtime (we are
        // inside `#[tokio::main]` here).
        let hooks = Arc::new(HookDispatcher::new(
            shared.clone(),
            tokio::runtime::Handle::current(),
        ));
        hooks.start();

        // Watch the autoload folder (no-op when unconfigured). The watcher is
        // tied to this session's lifetime.
        start_folder_watcher(shared.clone(), hooks.clone());

        Ok(Self {
            config_filename,
            shared,
            init_logging: Arc::new(init_logging),
            hooks,
        })
    }

    pub fn shared(&self) -> Arc<RwLock<SharedState>> {
        self.shared.clone()
    }

    /// Convenience accessor; the shared views go through `LocalClient`
    /// instead, so this is only exercised by tests today.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn api(&self) -> Arc<Api> {
        self.shared.read().api()
    }

    pub fn hooks(&self) -> Arc<HookDispatcher> {
        self.hooks.clone()
    }

    /// Return a snapshot of the current config.
    pub fn config(&self) -> RqbitDesktopConfig {
        self.shared.read().config()
    }

    pub async fn configure(&self, config: RqbitDesktopConfig) -> Result<(), ApiError> {
        // Quick check under a brief read lock: if nothing changed, bail out.
        {
            let g = self.shared.read();
            if g.config == config {
                // The config didn't change, and the API is running, nothing to do.
                return Ok(());
            }
        }

        // Grab the old API under a brief read lock, then release it so the UI
        // thread (which reads `self.shared` every second for stats) is not
        // blocked while we stop the old session and spin up a new one.
        let old_api = self.shared.read().api();

        // Stop the old session without holding any lock across the await.
        old_api.session().stop().await;

        // Build the replacement API without holding any lock across the await.
        let session_api = match api_from_config(&self.init_logging, &config).await {
            Ok(api) => api,
            Err(e) => {
                // The config file was already written by the caller; adopt the
                // new config here too so the UI does not silently roll the
                // user's changes back (the old session is stopped either way,
                // and no folder watcher is started for it — restart the app
                // or fix the config and re-apply). The old session's column
                // poller died with it, so drop the stale provider.
                let mut shared = self.shared.write();
                shared.config = config;
                shared.columns = None;
                return Err(e.into());
            }
        };
        if let Err(e) = write_config(&self.config_filename, &config) {
            error!("error writing config: {:#}", e);
        }

        // Swap in the new config + API under a brief write lock.
        {
            let mut existing = self.shared.write();
            existing.config = config;
            existing.api = Arc::new(session_api.api);
            existing.columns = session_api.columns;
        }
        // The old session's folder watcher exits with its cancellation
        // token; start one for the new session (no-op when unconfigured).
        start_folder_watcher(self.shared.clone(), self.hooks.clone());
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a State around a minimal session (no DHT / listeners /
    /// persistence / HTTP API) so the test doesn't touch real user state.
    pub(crate) async fn test_state(config: RqbitDesktopConfig, config_filename: String) -> State {
        static INIT_LOGGING: std::sync::OnceLock<Arc<InitLoggingResult>> =
            std::sync::OnceLock::new();
        let init_logging = INIT_LOGGING
            .get_or_init(|| {
                Arc::new(
                    librqbit::tracing_subscriber_config_utils::init_logging(
                        librqbit::tracing_subscriber_config_utils::InitLoggingOptions {
                            default_rust_log_value: Some("info"),
                            log_file: None,
                            log_file_rust_log: None,
                            log_file_json: false,
                        },
                    )
                    .unwrap(),
                )
            })
            .clone();
        let session_api = api_from_config(&init_logging, &config).await.unwrap();
        let shared = Arc::new(RwLock::new(SharedState {
            config,
            api: Arc::new(session_api.api),
            columns: session_api.columns,
        }));
        let hooks = Arc::new(HookDispatcher::new(
            shared.clone(),
            tokio::runtime::Handle::current(),
        ));
        State {
            config_filename,
            shared,
            init_logging,
            hooks,
        }
    }

    pub(crate) fn quiet_config() -> RqbitDesktopConfig {
        RqbitDesktopConfig {
            default_download_location: std::env::temp_dir(),
            dht: crate::config::RqbitDesktopConfigDht {
                disable: true,
                ..Default::default()
            },
            connections: crate::config::RqbitDesktopConfigConnections {
                enable_tcp_listen: false,
                enable_utp: false,
                enable_upnp_port_forward: false,
                ..Default::default()
            },
            persistence: crate::config::RqbitDesktopConfigPersistence {
                disable: true,
                ..Default::default()
            },
            http_api: crate::config::RqbitDesktopConfigHttpApi {
                disable: true,
                ..Default::default()
            },
            web_view: crate::config::RqbitDesktopConfigWebView {
                disable: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Mirror of the settings page's Apply flow: change the load/move folders,
    /// call `configure`, and check that the shared config is swapped, the
    /// config file is written, and the new folder watcher actually picks up a
    /// dropped .torrent file.
    #[tokio::test]
    async fn configure_applies_folder_settings_and_starts_watcher() {
        use crate::folder_watcher::tests::{TempDir, torrent_bytes};

        let watch_dir = TempDir::new("cfg-watch");
        let move_dir = TempDir::new("cfg-move");
        let config_file = TempDir::new("cfg-file");
        let config_filename = config_file.path().join("config.json");
        let config_filename = config_filename.to_str().unwrap().to_owned();

        let state = test_state(quiet_config(), config_filename.clone()).await;

        let mut new_config = state.config();
        new_config.load_torrents_folder = watch_dir.path().to_path_buf();
        new_config.move_torrents_folder = move_dir.path().to_path_buf();

        // Drop a torrent into the watch folder before applying; the watcher
        // scans pre-existing files on startup.
        std::fs::write(watch_dir.path().join("dropped.torrent"), torrent_bytes()).unwrap();

        state.configure(new_config.clone()).await.unwrap();

        // Shared config was swapped.
        assert!(state.config() == new_config);
        // Config file was persisted with the folders.
        let persisted = crate::config::read_config(&config_filename).unwrap();
        assert_eq!(persisted.load_torrents_folder, watch_dir.path());
        assert_eq!(persisted.move_torrents_folder, move_dir.path());
        // And the watcher moved the file.
        let dest = move_dir.path().join("dropped.torrent");
        for _ in 0..100 {
            if dest.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(dest.exists(), "configure() should start the folder watcher");
    }

    /// Applying an unchanged config is a no-op and must not stop the session.
    #[tokio::test]
    async fn configure_noop_when_unchanged() {
        let state = test_state(quiet_config(), "/tmp/rqbit-cfg-noop/config.json".to_owned()).await;

        let before = state.api();
        state.configure(state.config()).await.unwrap();
        let after = state.api();

        assert!(
            Arc::ptr_eq(&before, &after),
            "unchanged config must not rebuild the session"
        );
    }

    /// Same as above but with the "full-fat" config a real user has: TCP
    /// listener, HTTP API, DHT and persistence all enabled (pointed at temp
    /// dirs / scratch ports). This exercises the session replacement path —
    /// in particular whether the new session can re-bind its listeners after
    /// the old one is stopped.
    #[tokio::test]
    async fn configure_replaces_full_session_without_port_conflict() {
        use crate::folder_watcher::tests::TempDir;

        let watch_dir = TempDir::new("full-watch");
        let move_dir = TempDir::new("full-move");
        let scratch = TempDir::new("full-scratch");
        let config_filename = scratch.path().join("config.json");
        let config_filename = config_filename.to_str().unwrap().to_owned();

        let base = || crate::config::RqbitDesktopConfig {
            default_download_location: std::env::temp_dir(),
            dht: crate::config::RqbitDesktopConfigDht {
                disable: false,
                disable_persistence: false,
                persistence_filename: scratch.path().join("dht.json"),
            },
            connections: crate::config::RqbitDesktopConfigConnections {
                enable_tcp_listen: true,
                enable_utp: false,
                enable_upnp_port_forward: false,
                listen_port: 39421,
                ..Default::default()
            },
            persistence: crate::config::RqbitDesktopConfigPersistence {
                disable: false,
                folder: scratch.path().join("persistence"),
                ..Default::default()
            },
            http_api: crate::config::RqbitDesktopConfigHttpApi {
                disable: false,
                listen_addr: "127.0.0.1:39031".parse().unwrap(),
                read_only: false,
            },
            web_view: crate::config::RqbitDesktopConfigWebView {
                disable: false,
                listen_addr: "127.0.0.1:39032".parse().unwrap(),
            },
            ..Default::default()
        };

        let state = test_state(base(), config_filename.clone()).await;

        let mut new_config = base();
        new_config.load_torrents_folder = watch_dir.path().to_path_buf();
        new_config.move_torrents_folder = move_dir.path().to_path_buf();

        // If this fails, the settings Apply silently rolls back in the UI:
        // the shared config is never swapped and the folder watcher never
        // starts, although the config file was already written.
        state.configure(new_config.clone()).await.unwrap();

        assert!(state.config() == new_config);
        assert_eq!(
            state.config().load_torrents_folder,
            watch_dir.path().to_path_buf()
        );
    }

    /// When the replacement session cannot be built, `configure` must still
    /// adopt the new config (the file was already written by the settings
    /// page) so the UI does not silently roll the user's changes back.
    #[tokio::test]
    async fn configure_failure_still_adopts_config() {
        let scratch = crate::folder_watcher::tests::TempDir::new("fail-scratch");
        let config_filename = scratch.path().join("config.json");
        let config_filename = config_filename.to_str().unwrap().to_owned();
        let state = test_state(quiet_config(), config_filename.clone()).await;

        // A config with the UPnP server enabled but the HTTP API disabled
        // fails validation inside api_from_config.
        let mut bad = quiet_config();
        bad.upnp.enable_server = true;
        bad.http_api.disable = true;

        // The settings page persists the config before calling configure.
        crate::config::write_config(&config_filename, &bad).unwrap();

        let result = state.configure(bad.clone()).await;
        assert!(result.is_err(), "invalid config must fail reconfigure");

        // The shared config still reflects what was applied...
        assert!(state.config() == bad);
        // ...and so does the file the settings page wrote before calling
        // configure (verify by reading it back).
        let persisted = crate::config::read_config(&config_filename).unwrap();
        assert!(persisted == bad);
    }

    /// The bundled web view: a session with `web_view` enabled serves the
    /// embedded browser client (the same routes `app-daemon` serves) for the
    /// desktop's own session — this is what makes the web-based view work
    /// while the desktop app runs, with a single librqbit instance.
    #[tokio::test]
    async fn web_view_is_served_by_desktop_session() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let scratch = crate::folder_watcher::tests::TempDir::new("webview-scratch");
        let config_filename = scratch.path().join("config.json");
        let config_filename = config_filename.to_str().unwrap().to_owned();

        let mut config = quiet_config();
        config.web_view = crate::config::RqbitDesktopConfigWebView {
            disable: false,
            listen_addr: "127.0.0.1:38991".parse().unwrap(),
        };
        let _state = test_state(config, config_filename).await;

        // The server binds in a spawned task; poll until it answers.
        let mut html = String::new();
        for _ in 0..100 {
            let attempt = async {
                let mut stream = tokio::net::TcpStream::connect("127.0.0.1:38991").await?;
                stream
                    .write_all(b"GET / HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
                    .await?;
                let mut buf = String::new();
                stream.read_to_string(&mut buf).await?;
                Ok::<String, std::io::Error>(buf)
            };
            if let Ok(buf) = attempt.await {
                html = buf;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        let status = html
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        assert_eq!(status, 200, "GET / should serve the web client");
        assert!(
            html.to_lowercase().contains("content-type: text/html"),
            "index must be HTML, got: {}",
            &html[..html.len().min(300)]
        );
        assert!(html.contains("rqbit"), "page should name rqbit");

        // The wasm payload under its relative ./pkg/ path (real or
        // placeholder — both prove the route is wired to this session).
        let mut stream = tokio::net::TcpStream::connect("127.0.0.1:38991")
            .await
            .unwrap();
        stream
            .write_all(b"GET /pkg/web_gpui.js HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut js = String::new();
        stream.read_to_string(&mut js).await.unwrap();
        let status = js
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        assert_eq!(status, 200, "GET /pkg/web_gpui.js");
        assert!(
            js.to_lowercase().contains("content-type: text/javascript"),
            "js payload must have the JS content type"
        );
    }
}
