//! Browser implementation of the shared UI's [`UiHost`]: no OS integration;
//! .torrent files are picked through the browser file picker and uploaded to
//! the daemon as bytes, settings are edited through the daemon's session
//! settings (folder paths as plain text inputs), the theme is session-local
//! and the playlist row is unavailable (the daemon's HTTP media endpoints
//! are not part of this protocol).

use std::future::Future;
use std::sync::Arc;

use gpui::{AnyView, App, AppContext as _, Context, Window};
use rqbit_ui::client::TorrentClient;
use rqbit_ui::host::{PickedTorrentFile, UiCapabilities, UiHost};
use rqbit_ui::ui::add_torrent_dialog::AddTorrentEntry;
use rqbit_ui::ui::main_panel::MainPanel;
use rqbit_ui::ui::settings_page::{SessionSettingsPage, SessionSettingsPageEvent};

use crate::file_picker;

pub struct WebHost;

impl UiHost for WebHost {
    fn capabilities(&self) -> UiCapabilities {
        // Magnet/URL add, browser file uploads, daemon-backed session
        // settings. No "Open Folder" (no filesystem), no log page.
        UiCapabilities {
            add_torrent_files: true,
            open_folder: false,
            settings_page: true,
            logs_page: false,
        }
    }

    fn version(&self) -> String {
        "web".to_string()
    }

    fn pick_torrent_files(
        &self,
        _cx: &mut App,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<PickedTorrentFile>, String>> + 'static>>
    {
        let receiver = file_picker::pick_files(".torrent", true);
        Box::pin(async move {
            match receiver.await {
                Ok(Ok(files)) => Ok(files
                    .into_iter()
                    .map(|file| PickedTorrentFile {
                        // No local path in the browser: nothing to move
                        // after a successful add.
                        path: None,
                        name: file.name,
                        bytes: file.bytes,
                    })
                    .collect()),
                Ok(Err(e)) => Err(e),
                Err(_) => Err("file picker was dropped".to_string()),
            }
        })
    }

    fn open_settings(
        &self,
        client: Arc<dyn TorrentClient>,
        window: &mut Window,
        cx: &mut Context<MainPanel>,
    ) -> Option<AnyView> {
        let page = cx.new(|cx| SessionSettingsPage::new(window, cx, client));
        cx.subscribe(
            &page,
            |this, _entity, event: &SessionSettingsPageEvent, cx| {
                if matches!(event, SessionSettingsPageEvent::Back) {
                    this.close_page(cx);
                }
            },
        )
        .detach();
        Some(page.into())
    }

    fn adjust_file_selection<'a>(
        &'a self,
        _entry: &'a mut AddTorrentEntry,
    ) -> std::pin::Pin<Box<dyn Future<Output = Vec<String>> + 'a>> {
        Box::pin(async { Vec::new() })
    }
}
