#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use gpui::*;
use gpui_kit::base::input as input_actions;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

mod config;
mod desktop_host;
mod events;
mod folder_watcher;
mod hooks;
mod local_client;
mod ui;
mod web_settings;

mod state;
use crate::{
    desktop_host::DesktopHost, events::hold_to_quit_view::HoldToQuitView,
    local_client::LocalClient, state::State,
};
use rqbit_ui::ui::dialogs::AboutInfo;
use rqbit_ui::ui::main_panel::MainPanel;
impl gpui::Global for State {}

// App-menu actions, dispatched by the native macOS menu bar (see `set_menus`
// below) and handled by the global listeners registered after the window
// opens.
gpui_kit::actions!(rqbit_desktop, [About, Settings, Quit]);

/// Author of the rqbit engine (librqbit crate metadata). The crate exposes no
/// runtime accessor for it, so it is pinned here.
const RQBIT_AUTHOR: &str = "Igor Katson";

/// The About-dialog payload, resolved from the desktop crate's own metadata
/// and the librqbit engine crate.
fn about_info() -> AboutInfo {
    AboutInfo {
        app_name: "Rqbit Desktop".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        description: "A native desktop client for the rqbit BitTorrent engine, \
                      built with GPUI. Bundles a local daemon so the same \
                      session also serves a web UI in the browser."
            .into(),
        engine_name: "rqbit".into(),
        engine_version: librqbit::version().into(),
        engine_author: RQBIT_AUTHOR.into(),
        desktop_name: "rqbit-desktop".into(),
        desktop_author: env!("CARGO_PKG_AUTHORS").into(),
    }
}

use librqbit::tracing_subscriber_config_utils::{InitLoggingOptions, init_logging};

#[tokio::main]
async fn main() {
    // Logging
    let init_logging_result = init_logging(InitLoggingOptions {
        default_rust_log_value: Some("info"),
        log_file: None,
        log_file_rust_log: None,
        log_file_json: false,
    })
    .unwrap();

    match librqbit::try_increase_nofile_limit() {
        Ok(limit) => info!(limit = limit, "increased open file limit"),
        Err(e) => warn!("failed increasing open file limit: {:#}", e),
    };

    // Shared state
    let shared_state = State::new(init_logging_result)
        .await
        .expect("failed to create state");
    // One Arc<State> for the whole app: the web settings backend resolves the
    // session through it, so it must outlive the app (DesktopHost holds it).
    let state_arc = Arc::new(shared_state.clone());
    shared_state.web_settings.attach(&state_arc);

    info!("GPUI application started – state ready");

    // Run GPUI
    let platform = gpui_platform::application().with_assets(gpui_kit::assets::Assets);
    platform.run(move |cx: &mut App| {
        // Store the state globally so all windows/views can access it
        cx.set_global(shared_state.clone());

        // Bring the app to the foreground on launch (macOS: activateIgnoringOtherApps).
        cx.activate(true);

        gpui_kit::init(cx);

        // Apply the persisted theme (light/dark) from the config file.
        let config = shared_state.config();
        let theme_mode = config.theme.as_str();
        if theme_mode == "dark" {
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        } else {
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Light, None, cx);
        }

        let window_options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("rqbit".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(13.), px(13.))),
            }),
            window_bounds: Some(WindowBounds::centered(size(px(1000.), px(800.)), cx)),
            window_decorations: Some(WindowDecorations::Server),
            window_min_size: Some(size(px(600.), px(400.))),
            // The custom `TitleBar` drags and zooms the window itself. Without
            // this, AppKit also treats the title-bar strip as its own drag
            // region and zooms the window on any double-click there, including
            // double-clicks on the title-bar buttons.
            app_owns_titlebar_drag: true,
            ..Default::default()
        };

        // Hold-to-quit: keep cmd-q / ctrl-c held for 1 second to exit. Releasing
        // either the key or the modifier early drops the countdown task,
        // cancelling the quit. The key listeners are attached to the root
        // element, so they are registered during paint (safe) and cleared each
        // frame.
        let quit_countdown: Arc<Mutex<Option<Task<()>>>> = Arc::new(Mutex::new(None));

        let (_, root) = gpui_kit::open_window(window_options, cx, |window, cx| {
            let client = Arc::new(LocalClient::new(
                shared_state.shared(),
                shared_state.hooks(),
            ));
            let host = Arc::new(DesktopHost::new(state_arc.clone()));
            let main_panel = cx.new(|cx| MainPanel::new(client, host, window, cx));
            cx.new(|_cx| HoldToQuitView {
                main_panel,
                quit_countdown: quit_countdown.clone(),
            })
        })
        .expect("Failed to open window");

        // Native macOS menu bar (the "top panel"). Menu items dispatch GPUI
        // actions to the active window; anything the views don't handle falls
        // through to the global handlers registered below. The Edit menu
        // reuses the gpui-base input actions that gpui-component's text
        // inputs already handle, so the items work on the focused input.
        cx.set_menus(vec![
            Menu::new("Rqbit Desktop").items(vec![
                MenuItem::action("About Rqbit Desktop", About),
                MenuItem::separator(),
                MenuItem::action("Settings…", Settings),
                MenuItem::separator(),
                MenuItem::action("Quit Rqbit Desktop", Quit),
            ]),
            Menu::new("Edit").items(vec![
                MenuItem::action("Cut", input_actions::Cut),
                MenuItem::action("Copy", input_actions::Copy),
                MenuItem::action("Paste", input_actions::Paste),
                MenuItem::separator(),
                MenuItem::action("Select All", input_actions::SelectAll),
            ]),
        ]);

        let main_panel = root.read(cx).main_panel.downgrade();

        cx.on_action({
            let main_panel = main_panel.clone();
            move |_: &About, cx| {
                main_panel
                    .update(cx, |panel, cx| panel.open_about(about_info(), cx))
                    .ok();
            }
        });
        cx.on_action({
            let main_panel = main_panel.clone();
            move |_: &Settings, cx| {
                main_panel
                    .update(cx, |panel, cx| panel.open_settings_page(cx))
                    .ok();
            }
        });
        cx.on_action(|_: &Quit, cx| cx.quit());

        // Quit the app when the window is closed (native close button).
        cx.on_window_closed(move |cx: &mut App, _window_id| {
            cx.quit();
        })
        .detach();
    });
}
