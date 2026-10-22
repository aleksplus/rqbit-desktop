use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context;
use librqbit::{
    ConnectionOptions, ListenerMode, ListenerOptions, PeerConnectionOptions, dht::PersistentDht,
    limits::LimitsConfig,
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::hooks::HookEvent;

/// HTTP method used to contact a hook endpoint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HttpMethod {
    #[default]
    Post,
    Get,
}

/// A single webhook-style hook. See [`crate::hooks`] for the wire contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HookConfig {
    pub name: String,
    pub enabled: bool,
    /// Events this hook subscribes to (see [`HookEvent`]).
    pub events: Vec<HookEvent>,
    pub method: HttpMethod,
    pub url: String,
    /// Request timeout in seconds.
    pub timeout_secs: u64,
    /// Extra HTTP headers sent with every request.
    pub headers: std::collections::BTreeMap<String, String>,
    /// Seconds between polls of this hook — only used by the
    /// `torrent_columns` event (all other events fire on demand).
    pub poll_secs: u64,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            name: "New hook".to_string(),
            enabled: true,
            events: vec![HookEvent::TorrentAdded],
            method: HttpMethod::Post,
            url: String::new(),
            timeout_secs: 10,
            headers: Default::default(),
            poll_secs: 10,
        }
    }
}

/// Hooks section of the desktop config.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RqbitDesktopConfigHooks {
    pub hooks: Vec<HookConfig>,
}

// Config read/write functions
fn default_true() -> bool {
    true
}

pub fn read_config(path: &str) -> anyhow::Result<RqbitDesktopConfig> {
    use std::fs::File;
    use std::io::BufReader;
    let rdr = BufReader::new(File::open(path)?);
    let mut config: RqbitDesktopConfig = serde_json::from_reader(rdr)?;
    config.persistence.fix_backwards_compat();
    Ok(config)
}

pub fn write_config(path: &str, config: &RqbitDesktopConfig) -> anyhow::Result<()> {
    use std::fs;
    use std::io::BufWriter;
    fs::create_dir_all(Path::new(path).parent().context("no parent")?)
        .context("error creating dirs")?;
    let tmp = format!("{}.tmp", path);
    let mut tmp_file = BufWriter::new(
        fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(&tmp)?,
    );
    serde_json::to_writer(&mut tmp_file, config)?;
    fs::rename(tmp, path)?;
    Ok(())
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RqbitDesktopConfigDht {
    pub disable: bool,
    pub disable_persistence: bool,
    pub persistence_filename: PathBuf,
}

impl Default for RqbitDesktopConfigDht {
    fn default() -> Self {
        Self {
            disable: false,
            disable_persistence: false,
            persistence_filename: PersistentDht::default_persistence_filename().unwrap(),
        }
    }
}

#[serde_as]
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RqbitDesktopConfigConnections {
    pub enable_tcp_listen: bool,
    pub enable_tcp_outgoing: bool,
    pub enable_utp: bool,
    pub enable_upnp_port_forward: bool,
    pub socks_proxy: String,
    pub listen_port: u16,

    #[serde_as(as = "serde_with::DurationSeconds")]
    pub peer_connect_timeout: Duration,
    #[serde_as(as = "serde_with::DurationSeconds")]
    pub peer_read_write_timeout: Duration,
}

impl RqbitDesktopConfigConnections {
    pub fn as_listener_and_connect_opts(&self) -> (Option<ListenerOptions>, ConnectionOptions) {
        let mode = match (self.enable_tcp_listen, self.enable_utp) {
            (true, true) => Some(ListenerMode::TcpAndUtp),
            (true, false) => Some(ListenerMode::TcpOnly),
            (false, true) => Some(ListenerMode::UtpOnly),
            (false, false) => None,
        };
        let listener_opts = mode.map(|mode| ListenerOptions {
            mode,
            listen_addr: (Ipv4Addr::UNSPECIFIED, self.listen_port).into(),
            enable_upnp_port_forwarding: self.enable_upnp_port_forward,
            ..Default::default()
        });
        let connect_opts = ConnectionOptions {
            proxy_url: if self.socks_proxy.is_empty() {
                None
            } else {
                Some(self.socks_proxy.clone())
            },
            enable_tcp: self.enable_tcp_outgoing,
            peer_opts: Some(PeerConnectionOptions {
                connect_timeout: Some(self.peer_connect_timeout),
                read_write_timeout: Some(self.peer_read_write_timeout),
                ..Default::default()
            }),
        };
        (listener_opts, connect_opts)
    }
}

impl Default for RqbitDesktopConfigConnections {
    fn default() -> Self {
        Self {
            enable_tcp_listen: true,
            enable_tcp_outgoing: true,
            enable_utp: false,
            enable_upnp_port_forward: true,
            listen_port: 4240,
            socks_proxy: String::new(),
            peer_connect_timeout: Duration::from_secs(2),
            peer_read_write_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RqbitDesktopConfigPersistence {
    pub disable: bool,

    #[serde(default)]
    pub folder: PathBuf,

    #[serde(default)]
    pub fastresume: bool,

    /// Deprecated, but keeping for backwards compat for serialized / deserialized config.
    #[serde(default)]
    pub filename: PathBuf,
}

impl RqbitDesktopConfigPersistence {
    pub(crate) fn fix_backwards_compat(&mut self) {
        if self.folder != Path::new("") {
            return;
        }
        if self.filename != Path::new("")
            && let Some(parent) = self.filename.parent()
        {
            self.folder = parent.to_owned();
        }
    }
}

impl Default for RqbitDesktopConfigPersistence {
    fn default() -> Self {
        let folder = librqbit::SessionPersistenceConfig::default_json_persistence_folder().unwrap();
        Self {
            disable: false,
            folder,
            fastresume: false,
            filename: PathBuf::new(),
        }
    }
}

#[serde_as]
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RqbitDesktopConfigHttpApi {
    pub disable: bool,
    pub listen_addr: SocketAddr,
    pub read_only: bool,
}

impl Default for RqbitDesktopConfigHttpApi {
    fn default() -> Self {
        Self {
            disable: Default::default(),
            listen_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 3030)),
            read_only: false,
        }
    }
}

/// The bundled web view: the same WebSocket protocol (`/ws`) and embedded
/// browser client (`/`) the standalone `app-daemon` serves, hosted by the
/// desktop app for its own session. One librqbit instance then serves both
/// the native UI and the browser UI.
///
/// The default address deliberately differs from `app-daemon`'s (3080) so
/// both processes can run side by side; each still owns its own session.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RqbitDesktopConfigWebView {
    pub disable: bool,
    pub listen_addr: SocketAddr,
}

impl Default for RqbitDesktopConfigWebView {
    fn default() -> Self {
        Self {
            disable: false,
            listen_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 3081)),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(default)]
pub struct RqbitDesktopConfigUpnp {
    #[serde(default)]
    pub enable_server: bool,

    #[serde(default)]
    pub server_friendly_name: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RqbitDesktopConfig {
    pub default_download_location: PathBuf,

    /// Folder watched for new .torrent files to add automatically.
    /// Empty means the feature is disabled.
    #[serde(default)]
    pub load_torrents_folder: PathBuf,

    /// Whether torrents added automatically from the watch folder start
    /// paused. They can then be started from the torrent list.
    #[serde(default = "default_true")]
    pub auto_add_paused: bool,

    /// Folder to move .torrent files to after they were added.
    /// Empty means the feature is disabled.
    #[serde(default)]
    pub move_torrents_folder: PathBuf,

    #[cfg(feature = "disable-upload")]
    #[serde(default)]
    pub disable_upload: bool,

    pub dht: RqbitDesktopConfigDht,
    pub connections: RqbitDesktopConfigConnections,
    pub upnp: RqbitDesktopConfigUpnp,
    pub persistence: RqbitDesktopConfigPersistence,
    pub http_api: RqbitDesktopConfigHttpApi,
    pub web_view: RqbitDesktopConfigWebView,

    #[serde(default)]
    pub ratelimits: LimitsConfig,

    pub hooks: RqbitDesktopConfigHooks,

    /// UI theme: "light" or "dark". Defaults to "light".
    #[serde(default)]
    pub theme: String,
}

impl Default for RqbitDesktopConfig {
    fn default() -> Self {
        let userdirs = directories::UserDirs::new().expect("directories::UserDirs::new()");
        let download_folder = userdirs
            .download_dir()
            .map(|d| d.to_owned())
            .unwrap_or_else(|| userdirs.home_dir().join("Downloads"));

        Self {
            default_download_location: download_folder,
            load_torrents_folder: PathBuf::new(),
            auto_add_paused: true,
            move_torrents_folder: PathBuf::new(),
            dht: Default::default(),
            connections: Default::default(),
            upnp: Default::default(),
            persistence: Default::default(),
            http_api: Default::default(),
            web_view: Default::default(),
            ratelimits: Default::default(),
            hooks: Default::default(),
            theme: "light".to_string(),
            #[cfg(feature = "disable-upload")]
            disable_upload: false,
        }
    }
}

impl RqbitDesktopConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.upnp.enable_server {
            if self.http_api.disable {
                anyhow::bail!("if UPnP server is enabled, you need to enable the HTTP API also.")
            }
            if self.http_api.listen_addr.ip().is_loopback() {
                anyhow::bail!(
                    "if UPnP server is enabled, you need to set HTTP API IP to 0.0.0.0 or at least non-localhost address."
                )
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_config_without_hooks_key_deserializes() {
        let json = r#"{"default_download_location": "/tmp/dl"}"#;
        let cfg: RqbitDesktopConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.hooks.hooks.is_empty());
        // Sections added later default sensibly: the web view is on, on its
        // own port (not app-daemon's 3080), so both can run side by side.
        assert!(!cfg.web_view.disable);
        assert_eq!(cfg.web_view.listen_addr, "127.0.0.1:3081".parse().unwrap());
    }

    #[test]
    fn web_view_roundtrip() {
        let cfg = RqbitDesktopConfig {
            web_view: crate::config::RqbitDesktopConfigWebView {
                disable: true,
                listen_addr: "0.0.0.0:4000".parse().unwrap(),
            },
            ..Default::default()
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: RqbitDesktopConfig = serde_json::from_str(&s).unwrap();
        assert!(cfg == back);
    }

    #[test]
    fn old_config_without_auto_add_paused_defaults_to_true() {
        let json = r#"{"default_download_location": "/tmp/dl"}"#;
        let cfg: RqbitDesktopConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.auto_add_paused);
    }

    #[test]
    fn hooks_roundtrip() {
        let cfg = RqbitDesktopConfig {
            hooks: RqbitDesktopConfigHooks {
                hooks: vec![HookConfig {
                    name: "svc".into(),
                    enabled: false,
                    events: vec![
                        HookEvent::TorrentCompleted,
                        HookEvent::FileSelection,
                        HookEvent::TorrentColumns,
                    ],
                    method: HttpMethod::Get,
                    url: "http://127.0.0.1:9999/hook".into(),
                    timeout_secs: 3,
                    headers: [("Authorization".to_string(), "Bearer x".to_string())].into(),
                    poll_secs: 5,
                }],
            },
            ..Default::default()
        };
        let s = serde_json::to_string(&cfg).unwrap();
        assert!(s.contains(r#""torrent_completed""#));
        assert!(s.contains(r#""file_selection""#));
        assert!(s.contains(r#""torrent_columns""#));
        assert!(s.contains(r#""poll_secs":5"#));
        let back: RqbitDesktopConfig = serde_json::from_str(&s).unwrap();
        assert!(cfg == back);
    }

    #[test]
    fn hook_defaults() {
        let h: HookConfig = serde_json::from_str(r#"{"url": "http://x"}"#).unwrap();
        assert!(h.enabled);
        assert_eq!(h.timeout_secs, 10);
        assert_eq!(h.method, HttpMethod::Post);
        assert_eq!(h.events, vec![HookEvent::TorrentAdded]);
        assert_eq!(h.poll_secs, 10);
    }
}
