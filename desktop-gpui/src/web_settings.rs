//! Session settings for the bundled web view: maps the wire
//! [`SessionSettings`] DTO onto the session subset of the desktop config and
//! reconfigures the session exactly like the native settings page's Apply.
//!
//! The desktop-only pages (HTTP API, Web View, UPnP, hooks, autoload folders)
//! are deliberately not exposed here — they configure desktop-app services a
//! browser client has no business editing.

use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use app_daemon::server::SessionSettingsBackend;
use futures::future::BoxFuture;
use rqbit_protocol::SessionSettings;

use crate::config::RqbitDesktopConfig;
use crate::state::State;

/// The wire settings for a desktop config's session subset.
pub fn settings_from_config(config: &RqbitDesktopConfig) -> SessionSettings {
    SessionSettings {
        default_download_folder: config
            .default_download_location
            .to_string_lossy()
            .into_owned(),
        dht_enabled: !config.dht.disable,
        dht_persistence_enabled: !config.dht.disable_persistence,
        dht_persistence_filename: config
            .dht
            .persistence_filename
            .to_string_lossy()
            .into_owned(),
        persistence_enabled: !config.persistence.disable,
        persistence_folder: config.persistence.folder.to_string_lossy().into_owned(),
        fastresume: config.persistence.fastresume,
        download_rate_limit_bps: config.ratelimits.download_bps.map(|v| v.get()).unwrap_or(0),
        upload_rate_limit_bps: config.ratelimits.upload_bps.map(|v| v.get()).unwrap_or(0),
        enable_tcp_listen: config.connections.enable_tcp_listen,
        enable_utp: config.connections.enable_utp,
        enable_upnp_port_forward: config.connections.enable_upnp_port_forward,
        listen_port: config.connections.listen_port,
        enable_tcp_outgoing: config.connections.enable_tcp_outgoing,
        socks_proxy: config.connections.socks_proxy.clone(),
        peer_connect_timeout_secs: config.connections.peer_connect_timeout.as_secs() as u32,
        peer_read_write_timeout_secs: config.connections.peer_read_write_timeout.as_secs() as u32,
    }
}

/// Apply the wire settings onto a config draft (session subset only; every
/// other config section is left untouched).
pub fn apply_settings(config: &mut RqbitDesktopConfig, settings: &SessionSettings) {
    config.default_download_location = settings.default_download_folder.clone().into();
    config.dht.disable = !settings.dht_enabled;
    config.dht.disable_persistence = !settings.dht_persistence_enabled;
    config.dht.persistence_filename = settings.dht_persistence_filename.clone().into();
    config.persistence.disable = !settings.persistence_enabled;
    config.persistence.folder = settings.persistence_folder.clone().into();
    config.persistence.fastresume = settings.fastresume;
    config.ratelimits.download_bps = std::num::NonZeroU32::new(settings.download_rate_limit_bps);
    config.ratelimits.upload_bps = std::num::NonZeroU32::new(settings.upload_rate_limit_bps);
    config.connections.enable_tcp_listen = settings.enable_tcp_listen;
    config.connections.enable_utp = settings.enable_utp;
    config.connections.enable_upnp_port_forward = settings.enable_upnp_port_forward;
    config.connections.listen_port = settings.listen_port;
    config.connections.enable_tcp_outgoing = settings.enable_tcp_outgoing;
    config.connections.socks_proxy = settings.socks_proxy.clone();
    config.connections.peer_connect_timeout =
        Duration::from_secs(settings.peer_connect_timeout_secs as u64);
    config.connections.peer_read_write_timeout =
        Duration::from_secs(settings.peer_read_write_timeout_secs as u64);
}

/// Backend handed to the bundled web view's protocol server: Get reads the
/// live config, Set maps the settings onto a full config draft and runs the
/// same reconfigure the native settings page does (persisting the config and
/// rebuilding the session).
///
/// The reconfigure is spawned onto the plain tokio runtime on purpose: it
/// runs from inside a `session.spawn`-ed task (the web view server), and
/// stopping the session cancels that task's context — inline, the rebuild
/// would cancel itself after stopping the old session. Detached, the rebuild
/// completes and re-spawns the web view on the new session; the applying
/// client's connection drops and its auto-reconnect lands on the new
/// session, so the Set response may never arrive.
#[derive(Clone)]
pub struct WebViewSettingsBackend {
    /// Filled once by [`Self::attach`] after the `Arc<State>` exists
    /// (`State::new` builds the backend before that point).
    state: Arc<OnceLock<Weak<State>>>,
}

impl WebViewSettingsBackend {
    pub fn new() -> Self {
        Self {
            state: Arc::new(OnceLock::new()),
        }
    }

    /// Point the backend at the live State (called once from main, after the
    /// `Arc<State>` is created).
    pub fn attach(&self, state: &Arc<State>) {
        let _ = self.state.set(Arc::downgrade(state));
    }

    fn upgrade(&self) -> Option<Arc<State>> {
        self.state.get().and_then(|weak| weak.upgrade())
    }
}

impl Default for WebViewSettingsBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionSettingsBackend for WebViewSettingsBackend {
    fn get(&self) -> SessionSettings {
        match self.upgrade() {
            Some(state) => settings_from_config(&state.config()),
            None => SessionSettings::default(),
        }
    }

    fn set(&self, settings: SessionSettings) -> BoxFuture<'static, Result<(), String>> {
        let backend = self.clone();
        Box::pin(async move {
            let Some(state) = backend.upgrade() else {
                return Err("the app is shutting down".to_string());
            };
            if settings.default_download_folder.trim().is_empty() {
                return Err("Default download folder must not be empty".to_string());
            }
            let mut config = state.config();
            apply_settings(&mut config, &settings);

            // Spawned (not awaited inline): see the type doc. The result is
            // reported to the applying client only when its connection
            // survives the rebuild (the standalone daemon case); the desktop
            // web view task dies with the old session either way.
            tokio::spawn(async move {
                if let Err(e) = state.configure(config).await {
                    tracing::error!(error = %e, "web settings apply failed");
                }
            });
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip_through_config() {
        let mut config = RqbitDesktopConfig::default();
        let original = settings_from_config(&config);

        let mut changed = original.clone();
        changed.listen_port = 9999;
        changed.enable_utp = true;
        changed.download_rate_limit_bps = 123;
        changed.socks_proxy = "socks5://127.0.0.1:1080".into();

        apply_settings(&mut config, &changed);
        let mapped = settings_from_config(&config);
        assert_eq!(mapped, changed);

        // Unrelated config sections are untouched by apply_settings.
        apply_settings(&mut config, &original);
        assert_eq!(
            config.web_view.listen_addr,
            RqbitDesktopConfig::default().web_view.listen_addr
        );
        assert!(!config.upnp.enable_server);
    }
}
