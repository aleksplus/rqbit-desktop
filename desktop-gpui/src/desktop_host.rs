//! Desktop implementation of the shared UI's [`UiHost`]: everything that
//! needs a real OS — the file dialog for .torrent picking, "Open Folder",
//! opening URLs with the default handler, config-backed theme persistence,
//! the settings page, and the playlist/stream URLs derived from the
//! embedded HTTP API.

use std::future::Future;
use std::sync::Arc;

use gpui::{AnyView, App, AppContext as _, Context, Window};
use rqbit_ui::client::TorrentClient;
use rqbit_ui::host::{PickedTorrentFile, UiCapabilities, UiHost};
use rqbit_ui::ui::add_torrent_dialog::AddTorrentEntry;
use rqbit_ui::ui::main_panel::MainPanel;
use tracing::warn;

use crate::state::State;
use crate::ui::settings_page::{SettingsPage, SettingsPageEvent};

pub struct DesktopHost {
    state: Arc<State>,
}

impl DesktopHost {
    pub fn new(state: Arc<State>) -> Self {
        Self { state }
    }
}

impl UiHost for DesktopHost {
    fn capabilities(&self) -> UiCapabilities {
        UiCapabilities::desktop()
    }

    fn version(&self) -> String {
        // The desktop app's own version, not the librqbit engine's.
        env!("CARGO_PKG_VERSION").to_string()
    }

    fn open_folder(&self, path: &str) {
        if !path.is_empty()
            && let Err(e) = open::that(path)
        {
            tracing::warn!(path = %path, error = %e, "error opening folder");
        }
    }

    fn open_url(&self, url: &str) {
        // Detached: opening a browser/player must not block the UI thread.
        if let Err(e) = open::that_detached(url) {
            tracing::warn!(url = %url, error = %e, "error opening url");
        }
    }

    fn open_media(&self, url: &str) {
        // No in-app player: hand the stream URL to the OS default handler
        // (browser tab, VLC, …) which manages its own audio.
        self.open_url(url)
    }

    fn persist_theme(&self, mode: &str) {
        self.state.set_theme(mode);
    }

    fn playlist_url(&self, torrent_id: u64) -> Option<String> {
        let http_api = &self.state.config().http_api;
        if http_api.disable {
            return None;
        }
        Some(playlist_http_url(http_api.listen_addr, torrent_id))
    }

    fn stream_url(&self, torrent_id: u64, file_idx: usize, filename: &str) -> Option<String> {
        let http_api = &self.state.config().http_api;
        if http_api.disable {
            return None;
        }
        Some(stream_http_url(
            http_api.listen_addr,
            torrent_id,
            file_idx,
            filename,
        ))
    }

    fn open_settings(
        &self,
        _client: Arc<dyn TorrentClient>,
        window: &mut Window,
        cx: &mut Context<MainPanel>,
    ) -> Option<AnyView> {
        let page = cx.new(|cx| SettingsPage::new(window, cx, self.state.clone()));
        cx.subscribe(&page, |this, _entity, event: &SettingsPageEvent, cx| {
            if matches!(event, SettingsPageEvent::Back) {
                this.close_page(cx);
            }
        })
        .detach();
        Some(page.into())
    }

    fn pick_torrent_files(
        &self,
        cx: &mut App,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<PickedTorrentFile>, String>> + 'static>>
    {
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Select .torrent files".into()),
        });
        Box::pin(async move {
            match receiver.await {
                Ok(Ok(Some(paths))) => {
                    let mut files = Vec::new();
                    for path in paths {
                        match std::fs::read(&path) {
                            Ok(bytes) => {
                                let name = path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "torrent".to_string());
                                files.push(PickedTorrentFile {
                                    path: Some(path.to_string_lossy().into_owned()),
                                    name,
                                    bytes,
                                });
                            }
                            Err(e) => {
                                warn!(path = %path.display(), error = %e, "error reading picked torrent file")
                            }
                        }
                    }
                    Ok(files)
                }
                Ok(Ok(None)) => Ok(Vec::new()), // user cancelled
                Ok(Err(e)) => Err(format!("File dialog error: {e:?}")),
                Err(e) => Err(format!("File dialog receiver error: {e:?}")),
            }
        })
    }

    fn adjust_file_selection<'a>(
        &'a self,
        entry: &'a mut AddTorrentEntry,
    ) -> std::pin::Pin<Box<dyn Future<Output = Vec<String>> + 'a>> {
        Box::pin(async move { self.state.hooks().apply_file_selection(entry).await })
    }
}

/// Build the embedded HTTP API's base URL: an unspecified listen address
/// (0.0.0.0 / ::) is replaced with the loopback address so the URL works
/// from the local machine.
fn http_api_base_url(listen_addr: std::net::SocketAddr) -> String {
    let host = match listen_addr.ip() {
        std::net::IpAddr::V4(v4) if v4.is_unspecified() => "127.0.0.1".to_string(),
        std::net::IpAddr::V6(v6) => {
            let v6 = if v6.is_unspecified() {
                std::net::Ipv6Addr::LOCALHOST
            } else {
                v6
            };
            format!("[{v6}]")
        }
        ip => ip.to_string(),
    };
    format!("http://{host}:{}", listen_addr.port())
}

/// The playlist (M3U8) URL served by the embedded HTTP API for a torrent,
/// mirroring the webui's `getPlaylistUrl`.
fn playlist_http_url(listen_addr: std::net::SocketAddr, torrent_id: u64) -> String {
    format!(
        "{}/torrents/{torrent_id}/playlist",
        http_api_base_url(listen_addr)
    )
}

/// The streaming URL of one torrent file (Range-capable; plays while the
/// torrent is still downloading). The filename segment is decorative.
fn stream_http_url(
    listen_addr: std::net::SocketAddr,
    torrent_id: u64,
    file_idx: usize,
    filename: &str,
) -> String {
    let filename = urlencoding::encode(filename);
    format!(
        "{}/torrents/{torrent_id}/stream/{file_idx}/{filename}",
        http_api_base_url(listen_addr)
    )
}

#[cfg(test)]
mod tests {
    use super::{http_api_base_url, playlist_http_url, stream_http_url};

    fn addr(s: &str) -> std::net::SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn playlist_url_loopback_v4() {
        assert_eq!(
            playlist_http_url(addr("127.0.0.1:3030"), 3),
            "http://127.0.0.1:3030/torrents/3/playlist"
        );
    }

    #[test]
    fn playlist_url_keeps_non_loopback_address() {
        assert_eq!(
            playlist_http_url(addr("192.168.1.5:3030"), 42),
            "http://192.168.1.5:3030/torrents/42/playlist"
        );
    }

    #[test]
    fn playlist_url_replaces_unspecified_v4_with_loopback() {
        assert_eq!(
            playlist_http_url(addr("0.0.0.0:1234"), 0),
            "http://127.0.0.1:1234/torrents/0/playlist"
        );
    }

    #[test]
    fn playlist_url_replaces_unspecified_v6_with_loopback() {
        assert_eq!(
            playlist_http_url(addr("[::]:1234"), 7),
            "http://[::1]:1234/torrents/7/playlist"
        );
    }

    #[test]
    fn base_url_and_stream_url_shape() {
        assert_eq!(
            http_api_base_url(addr("0.0.0.0:3030")),
            "http://127.0.0.1:3030"
        );
        assert_eq!(
            stream_http_url(addr("0.0.0.0:3030"), 7, 2, "some dir/movie (2024).mkv"),
            "http://127.0.0.1:3030/torrents/7/stream/2/some%20dir%2Fmovie%20%282024%29.mkv"
        );
    }
}
