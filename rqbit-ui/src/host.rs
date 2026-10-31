//! Host abstraction: the parts of the app that only a native desktop (or a
//! browser) can provide — file dialogs, OS integrations, settings page,
//! theme persistence. The shared views call into an `Arc<dyn UiHost>` and
//! stay portable.

use std::future::Future;

use std::sync::Arc;

use gpui::{AnyView, Context, Window};

use crate::client::{TorrentClient, UiAddSource};
use crate::ui::add_torrent_dialog::AddTorrentEntry;
use crate::ui::main_panel::MainPanel;

/// Which host-provided affordances the shared UI should render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiCapabilities {
    /// Show the "Add Torrent…" (file picker) button.
    pub add_torrent_files: bool,
    /// Offer "Open Folder" in the torrent context menu.
    pub open_folder: bool,
    /// Show the settings button / page.
    pub settings_page: bool,
    /// Show the logs button / page.
    pub logs_page: bool,
}

impl UiCapabilities {
    /// A minimal remote client: magnet/URL add, no OS integrations.
    pub fn remote() -> Self {
        Self {
            add_torrent_files: false,
            open_folder: false,
            settings_page: false,
            logs_page: false,
        }
    }

    /// The full desktop experience.
    pub fn desktop() -> Self {
        Self {
            add_torrent_files: true,
            open_folder: true,
            settings_page: true,
            logs_page: true,
        }
    }
}

/// A .torrent file picked through a host file dialog.
pub struct PickedTorrentFile {
    /// Local path, when the host has one (desktop). Used by the desktop to
    /// move the source file after a successful add.
    pub path: Option<String>,
    pub name: String,
    pub bytes: Vec<u8>,
}

/// Host services for the shared views. All methods have neutral defaults so
/// a minimal web host only implements what it supports.
pub trait UiHost: 'static {
    fn capabilities(&self) -> UiCapabilities;

    /// Version string shown in the title bar.
    fn version(&self) -> String;

    /// Reveal a folder in the OS file manager.
    fn open_folder(&self, _path: &str) {}

    /// Open a URL with the host's default handler (desktop: the OS opener;
    /// browser: a new tab).
    fn open_url(&self, _url: &str) {}

    /// Play a media stream URL. Hosts may wrap the URL to control playback
    /// UX: browsers start bare media documents (a directly-opened
    /// `video/mp4` URL) muted under their autoplay policy, so the web host
    /// opens a minimal HTML player instead. Copy actions should keep using
    /// the raw [`UiHost::stream_url`].
    fn open_media(&self, _url: &str) {}

    /// Play one torrent file (a Play action in the torrent list or the
    /// files list). Receives the file's [`UiHost::stream_url`] as the
    /// fallback. The default delegates to [`UiHost::open_media`]; hosts
    /// with filesystem access may open the local file with the OS default
    /// player instead, using the stream only when the file isn't fully on
    /// disk yet.
    fn play_media_file(&self, _torrent_id: u64, _file_idx: usize, stream_url: &str) {
        self.open_media(stream_url);
    }

    /// Persist the theme choice ("dark"/"light").
    fn persist_theme(&self, _mode: &str) {}

    /// The playlist (M3U8) URL served for a torrent, when the host knows a
    /// usable HTTP media base. `None` renders the row as "Unavailable".
    fn playlist_url(&self, _torrent_id: u64) -> Option<String> {
        None
    }

    /// The HTTP streaming URL of one torrent file, when the host knows a
    /// usable media base. Works for downloading files too (the server
    /// streams from piece availability). Play actions are only rendered
    /// when this returns `Some`.
    fn stream_url(&self, _torrent_id: u64, _file_idx: usize, _filename: &str) -> Option<String> {
        None
    }

    /// Full URL for a daemon-relative artwork path (e.g. the
    /// `TorrentArtwork::path` of a decorated payload), when the host knows
    /// an HTTP base that serves it. Absolute `http(s)://` paths (hook
    /// Image-column values) pass through unchanged. `None` = artwork is not
    /// fetchable here and the UI renders none.
    fn artwork_url(&self, _path: &str) -> Option<String> {
        None
    }

    /// Fetch bytes over HTTP (artwork loading). Desktop: reqwest; browser:
    /// a same-origin fetch. The neutral default always fails, so hosts
    /// without a transport simply never show artwork.
    fn fetch_bytes<'a>(
        &'a self,
        _url: String,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + 'a>> {
        Box::pin(async { Err("fetching bytes is not supported here".to_string()) })
    }

    /// Open the host's settings page (rendered full-page inside MainPanel).
    /// The client is the same transport the panel talks to. The host
    /// subscribes its own "Back" event (via `cx.subscribe`) and must call
    /// `MainPanel::close_page` when it fires. `None` = the capability is
    /// advertised but the page could not open. The panel is leased for the
    /// duration of this call (it is invoked from inside the panel's update),
    /// so the entity itself must not be read here.
    fn open_settings(
        &self,
        _client: Arc<dyn TorrentClient>,
        _window: &mut Window,
        _cx: &mut Context<MainPanel>,
    ) -> Option<AnyView> {
        None
    }

    /// Pick .torrent files via a native/browser dialog, given app context
    /// (the desktop uses gpui's `App::prompt_for_paths`). Resolves
    /// `Ok(vec![])` when the user cancels, `Err` when unsupported.
    fn pick_torrent_files(
        &self,
        cx: &mut gpui::App,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<PickedTorrentFile>, String>> + 'static>>
    {
        let _ = cx;
        Box::pin(async { Err("picking files is not supported here".to_string()) })
    }

    /// Apply host-side file-selection adjustments to a previewed entry
    /// (desktop: `file_selection` hooks). Returns hook names for the UI hint.
    fn adjust_file_selection<'a>(
        &'a self,
        _entry: &'a mut AddTorrentEntry,
    ) -> std::pin::Pin<Box<dyn Future<Output = Vec<String>> + 'a>> {
        Box::pin(async { Vec::new() })
    }
}

/// Convenience: build an entry's source from a picked file.
pub fn source_from_picked(file: &PickedTorrentFile) -> UiAddSource {
    UiAddSource::Bytes(file.bytes.clone())
}
