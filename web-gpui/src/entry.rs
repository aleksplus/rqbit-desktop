//! wasm entrypoint: initialize the web platform, connect to the daemon,
//! open the window with the shared MainPanel.

use std::borrow::Cow;
use std::sync::Arc;

use gpui::{App, AppContext as _, TitlebarOptions, WindowOptions};

use rqbit_ui::ui::main_panel::MainPanel;
use wasm_bindgen::prelude::*;

use crate::icon_assets::IconAssets;
use crate::remote_client::RemoteClient;
use crate::web_host::WebHost;

// The browser text system starts with an empty font database (there are no
// system fonts behind cosmic-text in wasm), and GPUI panics on the first
// `.SystemUIFont` lookup unless the families it maps to are registered.
// On this platform `.SystemUIFont`/`.ZedSans` resolve to "IBM Plex Sans".
// Lilex is the mono companion (`.ZedMono`); see assets/fonts/ for licenses.
const EMBEDDED_FONTS: &[&[u8]] = &[
    include_bytes!("../assets/fonts/IBMPlexSans-Regular.ttf"),
    include_bytes!("../assets/fonts/IBMPlexSans-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Lilex-Regular.ttf"),
    include_bytes!("../assets/fonts/Lilex-Bold.ttf"),
];

/// Boot the UI. `daemon_url` is the `ws(s)://host:port/ws` endpoint of an
/// `app-daemon` process. The JS shell calls this after the wasm module
/// initializes; the GPUI event loop keeps running from the returned closure.
#[wasm_bindgen]
pub fn run(daemon_url: String) -> Result<(), JsValue> {
    // Panic hooks + console logging for the web platform.
    gpui_platform::web_init();

    let client = Arc::new(RemoteClient::new(daemon_url.clone()));
    let host = Arc::new(WebHost::new(&daemon_url));

    let app = gpui_platform::single_threaded_web().with_assets(IconAssets);
    app.run(move |cx: &mut App| {
        // Must precede gpui_kit::init: the theme's installed-font probe runs
        // (and caches) during init, and any text layout panics without fonts.
        cx.text_system()
            .add_fonts(EMBEDDED_FONTS.iter().map(|f| Cow::Borrowed(*f)).collect())
            .expect("failed to register embedded fonts");

        gpui_kit::init(cx);

        // The theme's mono default ("DejaVu Sans Mono") has no browser
        // equivalent; point it at the embedded Lilex.
        gpui_kit::component::Theme::update(cx, |theme| {
            theme.mono_font_family = "Lilex".into();
        });

        // Dark theme by default; toggling persists for the session only.
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);

        let window_options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("rqbit".into()),
                ..Default::default()
            }),
            window_min_size: Some(gpui::size(gpui::px(600.), gpui::px(400.))),
            ..Default::default()
        };

        gpui_kit::open_window(window_options, cx, |window, cx| {
            cx.new(|cx| MainPanel::new(client.clone(), host.clone(), window, cx))
        })
        .expect("failed to open window");
    });

    // `Application::run` returns after handing control to the browser event
    // loop; nothing to keep alive on the Rust side.
    Ok(())
}
