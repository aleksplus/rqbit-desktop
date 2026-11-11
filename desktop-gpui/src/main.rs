#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use gpui_kit::base::input as input_actions;
use gpui_kit::*;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

mod config;
mod desktop_host;
mod events;
mod folder_watcher;
mod hooks;
mod local_client;
mod system_tray;
mod ui;
mod web_settings;

mod state;
mod task_profiler;
use crate::{
    desktop_host::DesktopHost, events::hold_to_quit_view::HoldToQuitView,
    local_client::LocalClient, state::State, system_tray::SystemTray,
};
use rqbit_ui::ui::dialogs::AboutInfo;
use rqbit_ui::ui::main_panel::MainPanel;
impl Global for State {}

// App-menu and tray actions, dispatched by the native menu bar or system tray
// and handled by the global listeners registered below.
gpui_kit::actions!(rqbit_desktop, [About, Settings, Open, Quit]);

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

#[derive(Clone)]
struct MainWindow {
    handle: AnyWindowHandle,
    main_panel: WeakEntity<MainPanel>,
}

type SharedMainWindow = Arc<Mutex<Option<MainWindow>>>;

/// Activates the existing main window, or recreates it after the user closed it
/// to the tray. The librqbit session lives in `State`, not in the window, so
/// recreating the UI does not interrupt downloads or the bundled web view.
fn open_main_window(
    shared_state: &State,
    state_arc: &Arc<State>,
    main_window: &SharedMainWindow,
    cx: &mut App,
) -> WeakEntity<MainPanel> {
    let existing = main_window.lock().unwrap().clone();
    if let Some(existing) = existing
        && existing
            .handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        cx.activate(true);
        return existing.main_panel;
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

    cx.activate(true);
    let (handle, root) = gpui_kit::open_window(window_options, cx, |window, cx| {
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

    let main_panel = root.read(cx).main_panel.downgrade();
    *main_window.lock().unwrap() = Some(MainWindow {
        handle,
        main_panel: main_panel.clone(),
    });
    main_panel
}

use librqbit::tracing_subscriber_config_utils::{InitLoggingOptions, init_logging};

#[tokio::main]
async fn main() {
    task_profiler::maybe_start();

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

    // The tray keeps the session alive without a window. Exiting is explicit
    // through the tray menu, native application menu, or hold-to-quit shortcut.
    let platform = platform::application()
        .with_assets(assets::Assets)
        .with_quit_mode(QuitMode::Explicit);
    platform.run(move |cx: &mut App| {
        // Store the state globally so all windows/views can access it
        cx.set_global(shared_state.clone());

        gpui_kit::init(cx);

        // Apply the persisted theme (light/dark) from the config file.
        let config = shared_state.config();
        let theme_mode = config.theme.as_str();
        if theme_mode == "dark" {
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        } else {
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Light, None, cx);
        }

        let main_window = SharedMainWindow::default();
        open_main_window(&shared_state, &state_arc, &main_window, cx);

        match system_tray::build(cx) {
            Ok(tray) => cx.set_global(SystemTray(tray)),
            Err(error) => warn!(%error, "failed to create system tray; window close will quit"),
        }

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

        cx.on_action({
            let shared_state = shared_state.clone();
            let state_arc = state_arc.clone();
            let main_window = main_window.clone();
            move |_: &Open, cx| {
                open_main_window(&shared_state, &state_arc, &main_window, cx);
            }
        });
        cx.on_action({
            let shared_state = shared_state.clone();
            let state_arc = state_arc.clone();
            let main_window = main_window.clone();
            move |_: &About, cx| {
                open_main_window(&shared_state, &state_arc, &main_window, cx)
                    .update(cx, |panel, cx| panel.open_about(about_info(), cx))
                    .ok();
            }
        });
        cx.on_action({
            let shared_state = shared_state.clone();
            let state_arc = state_arc.clone();
            let main_window = main_window.clone();
            move |_: &Settings, cx| {
                open_main_window(&shared_state, &state_arc, &main_window, cx)
                    .update(cx, |panel, cx| panel.open_settings_page(cx))
                    .ok();
            }
        });
        cx.on_action(|_: &Quit, cx| {
            if let Some(tray) = cx.try_global::<SystemTray>() {
                let tray = tray.0.clone();
                if let Err(error) = tray.close(cx) {
                    warn!(%error, "failed to close system tray");
                }
            }
            cx.quit();
        });

        // Closing the main window leaves the torrent session running in the
        // tray. If the native tray was unavailable, preserve the old behavior
        // instead of leaving a windowless app with no obvious way to exit.
        cx.on_window_closed({
            let main_window = main_window.clone();
            move |cx: &mut App, window_id| {
                let main_window_closed = {
                    let mut slot = main_window.lock().unwrap();
                    if slot
                        .as_ref()
                        .is_some_and(|main| main.handle.window_id() == window_id)
                    {
                        *slot = None;
                        true
                    } else {
                        false
                    }
                };
                if main_window_closed && cx.try_global::<SystemTray>().is_none() {
                    cx.quit();
                }
            }
        })
        .detach();
    });
}
