//! Browser implementation of the shared UI's [`UiHost`]: no OS integration;
//! .torrent files are picked through the browser file picker and uploaded to
//! the daemon as bytes, settings are edited through the daemon's session
//! settings (folder paths as plain text inputs), and the theme is
//! session-local. Media playback works through the daemon's HTTP media
//! endpoints (streaming + playlist), which are served on the same port as
//! the WS protocol, so their URLs derive from the daemon URL: playing opens
//! the stream in a new tab inside a minimal HTML player (a bare media
//! document would start muted under browser autoplay policy), which streams
//! while the torrent downloads.

use std::future::Future;
use std::sync::Arc;

use gpui::{AnyView, App, AppContext as _, Context, Window};
use rqbit_ui::client::TorrentClient;
use rqbit_ui::host::{PickedTorrentFile, UiCapabilities, UiHost};
use rqbit_ui::ui::add_torrent_dialog::AddTorrentEntry;
use rqbit_ui::ui::main_panel::MainPanel;
use rqbit_ui::ui::settings_page::{SessionSettingsPage, SessionSettingsPageEvent};

use crate::file_picker;

/// The player page opened for stream URLs. Browsers start bare media
/// documents (a directly-opened `video/mp4` URL) **muted** under their
/// autoplay policy, so Play opens this minimal same-origin page instead:
/// the click on the UI counts as domain interaction, which allows unmuted
/// autoplay. The page distinguishes the possible silence causes instead of
/// leaving a black screen: a policy mute gets an unmute chip (any click
/// outside the video unmutes too), and playback without a decodable audio
/// track — browser codec limits, e.g. DTS/AC3 — says so explicitly.
///
/// `{url}` is substituted with the stream URL (already percent-encoded, so
/// it cannot break out of the attribute).
const PLAYER_HTML: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>rqbit player</title>
<style>
  html, body { margin: 0; height: 100%; background: #000; }
  body { display: grid; place-items: center; }
  video { max-width: 100vw; max-height: 100vh; outline: none; }
  #unmute { position: fixed; bottom: 64px; left: 50%; transform: translateX(-50%);
            font: 14px system-ui; color: #fff; background: #333;
            border: 1px solid #555; border-radius: 999px;
            padding: 8px 16px; cursor: pointer; }
  #status { position: fixed; bottom: 16px; left: 50%; transform: translateX(-50%);
            max-width: 80vw; font: 13px system-ui; color: #ffb84d;
            background: rgba(0,0,0,.7); border-radius: 8px; padding: 8px 12px;
            text-align: center; }
</style>
</head>
<body>
<video id="v" src="{url}" controls autoplay muted></video>
<div id="status" style="display:none"></div>
<script>
const v = document.getElementById('v');
const status = document.getElementById('status');
function show(text) { status.textContent = text; status.style.display = 'block'; }

function offerUnmute() {
  let b = document.getElementById('unmute');
  if (!b) {
    b = document.createElement('button');
    b.id = 'unmute';
    b.textContent = '🔇 Click to unmute';
    b.onclick = (e) => { e.stopPropagation(); v.muted = false; v.play(); };
    document.body.appendChild(b);
  }
}
function removeUnmute() { const b = document.getElementById('unmute'); if (b) b.remove(); }

v.onvolumechange = () => {
  if (v.muted && !v.paused) offerUnmute(); else if (!v.muted) removeUnmute();
};

// Autoplay: prefer sound; if the browser policy refuses, start muted and
// offer the unmute.
v.play().catch(() => {
  v.muted = true;
  v.play().then(offerUnmute).catch(offerUnmute);
});

// Any click outside the video also unblocks sound (large hit target).
document.body.addEventListener('click', () => {
  if (v.muted) { v.muted = false; v.play(); }
});

// Once playback settles, check that audio is actually being decoded.
// webkitAudioDecodedByteCount is Chrome/WebKit only; in other browsers it
// is undefined and the check is skipped.
v.addEventListener('playing', () => setTimeout(() => {
  if (v.muted) { offerUnmute(); return; }
  if (v.webkitAudioDecodedByteCount === 0) {
    show("No decodable audio: the file's audio codec is likely not supported " +
         "by this browser (e.g. DTS/AC3 in an MKV). Use \"Copy URL\" in rqbit " +
         "and open it in VLC/IINA for sound.");
  }
}, 1500));

v.addEventListener('error', () => {
  if (!v.error) return;
  const raw = v.error.message || ('code ' + v.error.code);
  // Browsers demux only a handful of containers (MP4/WebM/Ogg/MP3/WAV...):
  // AVI, MPEG-TS, WMV etc. fail right at open, and AC-3/DTS audio fails at
  // decode. Translate those to guidance instead of the cryptic pipeline
  // error.
  const unsupported = v.error.code === 4 ||
      /DEMUXER_ERROR|SRC_NOT_SUPPORTED|COULD_NOT_OPEN/i.test(raw);
  if (unsupported) {
    show("This file's format can't be decoded by this browser (AVI containers " +
         "and AC-3/DTS audio are the usual culprits). Use \"Copy URL\" in " +
         "rqbit and open it in VLC/IINA/mpv instead. [" + raw + "]");
  } else if (v.error.code === 2) {
    show("Network error while streaming — the torrent may still be " +
         "downloading. Try again once more pieces have arrived. [" + raw + "]");
  } else {
    show('Playback error: ' + raw);
  }
});
</script>
</body>
</html>
"#;

pub struct WebHost {
    /// The daemon's HTTP origin (`http(s)://host:port`), derived from the
    /// WS daemon URL; `None` when the URL doesn't look like one.
    media_base: Option<String>,
}

impl WebHost {
    pub fn new(daemon_url: &str) -> Self {
        Self {
            media_base: media_base_from_daemon_url(daemon_url),
        }
    }
}

/// Derive the daemon's HTTP origin from its WS endpoint URL
/// (`ws(s)://host:port/ws`). Plain http(s) URLs pass through (a reverse
/// proxy may serve both); anything else yields `None`.
fn media_base_from_daemon_url(daemon_url: &str) -> Option<String> {
    let base = daemon_url
        .strip_suffix("/ws")
        .unwrap_or(daemon_url)
        .trim_end_matches('/');
    if let Some(rest) = base.strip_prefix("wss://") {
        Some(format!("https://{rest}"))
    } else if let Some(rest) = base.strip_prefix("ws://") {
        Some(format!("http://{rest}"))
    } else if base.starts_with("http://") || base.starts_with("https://") {
        Some(base.to_string())
    } else {
        None
    }
}

/// If the blob player page cannot be created/opened, fall back to the bare
/// URL (browser media document — starts muted under autoplay policy).
fn open_url_fallback(url: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.open_with_url_and_target(url, "_blank");
    }
}

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

    fn open_url(&self, url: &str) {
        if let Some(window) = web_sys::window() {
            // A new tab: for stream URLs the browser's native player takes
            // over (Range-request streaming of downloading media works);
            // playlist URLs download the M3U8 (open with e.g. VLC).
            let _ = window.open_with_url_and_target(url, "_blank");
        }
    }

    fn open_media(&self, url: &str) {
        let Some(window) = web_sys::window() else {
            return;
        };
        // Wrap the stream in the player page (see PLAYER_HTML) and open it
        // as a blob URL: same origin as this UI, so the click that opened it
        // counts as the interaction browsers require for unmuted autoplay.
        let html = PLAYER_HTML.replace("{url}", url);
        let parts = js_sys::Array::new();
        parts.push(&wasm_bindgen::JsValue::from_str(&html));
        let bag = {
            let bag = web_sys::BlobPropertyBag::new();
            bag.set_type("text/html");
            bag
        };
        let blob = match web_sys::Blob::new_with_str_sequence_and_options(&parts, &bag) {
            Ok(blob) => blob,
            Err(_) => return open_url_fallback(url),
        };
        let blob_url = match web_sys::Url::create_object_url_with_blob(&blob) {
            Ok(u) => u,
            Err(_) => return open_url_fallback(url),
        };
        if window
            .open_with_url_and_target(&blob_url, "_blank")
            .is_err()
        {
            open_url_fallback(url);
        }
    }

    fn playlist_url(&self, torrent_id: u64) -> Option<String> {
        Some(format!(
            "{}/torrents/{torrent_id}/playlist",
            self.media_base.as_deref()?
        ))
    }

    fn stream_url(&self, torrent_id: u64, file_idx: usize, filename: &str) -> Option<String> {
        let filename = urlencoding::encode(filename);
        Some(format!(
            "{}/torrents/{torrent_id}/stream/{file_idx}/{filename}",
            self.media_base.as_deref()?
        ))
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
