#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use gpui_kit::base::input as input_actions;
use gpui_kit::*;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

#[cfg(feature = "agent-testing")]
mod agent_bridge;
#[cfg(feature = "agent-testing")]
mod agent_fixture;
#[cfg(all(feature = "agent-testing", target_os = "macos"))]
mod agent_macos_window;
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
use rqbit_ui::client::TorrentClient;
use rqbit_ui::host::UiHost;
use rqbit_ui::ui::dialogs::AboutInfo;
use rqbit_ui::ui::main_panel::MainPanel;
impl Global for State {}

/// The backend a window's services come from: the real librqbit session, or
/// the deterministic agent fixture (feature `agent-testing` plus
/// `RQBIT_AGENT_FIXTURE=1`). Fixture mode exists so agent-driven UI tests can
/// run the full app without a network, listeners, or user config.
#[derive(Clone)]
enum AppBackend {
    Real {
        state: State,
        state_arc: Arc<State>,
    },
    #[cfg(feature = "agent-testing")]
    Fixture {
        client: Arc<agent_fixture::FixtureClient>,
        host: Arc<agent_fixture::FixtureHost>,
    },
}

impl AppBackend {
    /// Per-window UI services. Real mode constructs a fresh
    /// `LocalClient`/`DesktopHost` pair per window, exactly as startup always
    /// did; fixture mode clones the SAME fixture instances for every
    /// recreated `MainPanel`, so all windows share one deterministic dataset.
    fn window_services(&self) -> (Arc<dyn TorrentClient>, Arc<dyn UiHost>) {
        match self {
            AppBackend::Real { state, state_arc } => (
                Arc::new(LocalClient::new(state.shared(), state.hooks())),
                Arc::new(DesktopHost::new(state_arc.clone())),
            ),
            #[cfg(feature = "agent-testing")]
            AppBackend::Fixture { client, host } => (
                client.clone() as Arc<dyn TorrentClient>,
                host.clone() as Arc<dyn UiHost>,
            ),
        }
    }

    fn window_title(&self) -> &'static str {
        match self {
            AppBackend::Real { .. } => "rqbit",
            #[cfg(feature = "agent-testing")]
            AppBackend::Fixture { .. } => "rqbit agent fixture",
        }
    }

    /// Theme applied at startup: the persisted config value in real mode, a
    /// fixed default in fixture mode (deterministic rendering, no config file).
    fn startup_theme(&self) -> String {
        match self {
            AppBackend::Real { state, .. } => state.config().theme,
            #[cfg(feature = "agent-testing")]
            AppBackend::Fixture { .. } => "light".into(),
        }
    }

    /// The real session state; `None` for the fixture backend.
    fn real_state(&self) -> Option<&State> {
        match self {
            AppBackend::Real { state, .. } => Some(state),
            #[cfg(feature = "agent-testing")]
            AppBackend::Fixture { .. } => None,
        }
    }

    #[cfg(feature = "agent-testing")]
    fn is_fixture(&self) -> bool {
        match self {
            AppBackend::Real { .. } => false,
            #[cfg(feature = "agent-testing")]
            AppBackend::Fixture { .. } => true,
        }
    }
}

/// Truthy env-flag parsing ("1", "true", "yes", "on", case-insensitive).
fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

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

fn request_quit(cx: &mut App) {
    if let Some(tray) = cx.try_global::<SystemTray>() {
        let tray = tray.0.clone();
        if let Err(error) = tray.close(cx) {
            warn!(%error, "failed to close system tray");
        }
    }
    cx.quit();
}

#[derive(Clone)]
struct MainWindow {
    handle: AnyWindowHandle,
    main_panel: WeakEntity<MainPanel>,
}

type SharedMainWindow = Arc<Mutex<Option<MainWindow>>>;

/// Activates the existing main window, or recreates it after the user closed it
/// to the tray. In real mode the librqbit session lives in `State`, not in the
/// window, so recreating the UI does not interrupt downloads or the bundled web
/// view; in fixture mode the same shared fixture instances are reused.
fn open_main_window(
    backend: &AppBackend,
    main_window: &SharedMainWindow,
    cx: &mut App,
    _agent_enabled: bool,
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
            title: Some(backend.window_title().into()),
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
    let window_backend = backend.clone();
    let (handle, root) = gpui_kit::open_window(window_options, cx, move |window, cx| {
        let (client, host) = window_backend.window_services();
        #[cfg(feature = "agent-testing")]
        let main_panel = if _agent_enabled {
            cx.new(|cx| MainPanel::new_with_agent(client, host, window, cx))
        } else {
            cx.new(|cx| MainPanel::new(client, host, window, cx))
        };
        #[cfg(not(feature = "agent-testing"))]
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

use librqbit::tracing_subscriber_config_utils::{
    InitLoggingOptions, InitLoggingResult, init_logging,
};

/// Real startup path, unchanged: raise fd limits, build the librqbit
/// session-backed `State`, and attach the web-view settings backend.
async fn real_backend(init_logging_result: InitLoggingResult) -> AppBackend {
    match librqbit::try_increase_nofile_limit() {
        Ok(limit) => info!(limit = limit, "increased open file limit"),
        Err(e) => warn!("failed increasing open file limit: {:#}", e),
    };

    // Shared state
    let state = State::new(init_logging_result)
        .await
        .expect("failed to create state");
    // One Arc<State> for the whole app: the web settings backend resolves the
    // session through it, so it must outlive the app (DesktopHost holds it).
    let state_arc = Arc::new(state.clone());
    state.web_settings.attach(&state_arc);

    info!("GPUI application started – state ready");
    AppBackend::Real { state, state_arc }
}

#[tokio::main]
async fn main() {
    task_profiler::maybe_start();

    // The agent fixture requires the `agent-testing` feature; without it the
    // flag is a configuration error, not a silent real-mode startup.
    #[cfg(not(feature = "agent-testing"))]
    if env_flag_enabled("RQBIT_AGENT_FIXTURE") {
        eprintln!(
            "RQBIT_AGENT_FIXTURE is set, but this build was compiled without the \
             `agent-testing` feature. Rebuild with \
             `cargo build -p desktop-gpui --features agent-testing` to run the \
             agent fixture."
        );
        std::process::exit(2);
    }

    // Logging
    let init_logging_result = init_logging(InitLoggingOptions {
        default_rust_log_value: Some("info"),
        log_file: None,
        log_file_rust_log: None,
        log_file_json: false,
    })
    .unwrap();

    // Fixture mode (feature `agent-testing` + RQBIT_AGENT_FIXTURE=1) bypasses
    // State::new and all real session/service setup: no librqbit session, no
    // listeners, no config file. The logging guards have no other owner in
    // that mode, so they are kept alive here until `platform.run` returns.
    // Without the fixture flag this is the real startup, whether or not
    // GPUI_AGENT is set (the agent server is wired up separately, later).
    #[cfg(feature = "agent-testing")]
    let (backend, _fixture_logging_guard) = if env_flag_enabled("RQBIT_AGENT_FIXTURE") {
        info!("RQBIT_AGENT_FIXTURE is set: using the deterministic agent fixture backend");
        // The fixture is deliberately UI-thread-local (`?Send`, RefCell
        // inside); the `Arc` matches the `Arc<dyn TorrentClient>` /
        // `Arc<dyn UiHost>` service types `MainPanel` takes.
        #[allow(clippy::arc_with_non_send_sync)]
        let fixture = AppBackend::Fixture {
            client: Arc::new(agent_fixture::FixtureClient::new()),
            host: Arc::new(agent_fixture::FixtureHost::new()),
        };
        (fixture, Some(init_logging_result))
    } else {
        (real_backend(init_logging_result).await, None)
    };
    #[cfg(not(feature = "agent-testing"))]
    let backend = real_backend(init_logging_result).await;

    #[cfg(feature = "agent-testing")]
    let agent_control =
        agent_bridge::AgentControl::from_env(backend.is_fixture()).unwrap_or_else(|error| {
            eprintln!("{error}");
            std::process::exit(2);
        });
    let agent_enabled = {
        #[cfg(feature = "agent-testing")]
        {
            agent_control.is_some()
        }
        #[cfg(not(feature = "agent-testing"))]
        {
            false
        }
    };

    // The tray keeps the session alive without a window. Exiting is explicit
    // through the tray menu, native application menu, or hold-to-quit shortcut.
    let main_window = SharedMainWindow::default();
    let platform = platform::application()
        .with_assets(assets::Assets)
        .with_quit_mode(QuitMode::Explicit);
    platform.on_reopen({
        let backend = backend.clone();
        let main_window = main_window.clone();
        move |cx| {
            open_main_window(&backend, &main_window, cx, agent_enabled);
        }
    });
    platform.run(move |cx: &mut App| {
        // Real mode only: store the state globally so all windows/views can
        // access it. The fixture backend has no State and installs no global.
        if let Some(state) = backend.real_state() {
            cx.set_global(state.clone());
        }

        gpui_kit::init(cx);

        // Theme: the persisted config value in real mode, a fixed default for
        // the fixture.
        if backend.startup_theme() == "dark" {
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        } else {
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Light, None, cx);
        }

        open_main_window(&backend, &main_window, cx, agent_enabled);

        #[cfg(feature = "agent-testing")]
        if let Some(control) = agent_control.clone() {
            agent_bridge::start_driver(control.clone(), main_window.clone(), cx);
            cx.on_app_quit(move |_cx| {
                let control = control.clone();
                async move {
                    control.begin_shutdown();
                    control.join().await;
                }
            })
            .detach();
        }

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
            let backend = backend.clone();
            let main_window = main_window.clone();
            move |_: &Open, cx| {
                open_main_window(&backend, &main_window, cx, agent_enabled);
            }
        });
        cx.on_action({
            let backend = backend.clone();
            let main_window = main_window.clone();
            move |_: &About, cx| {
                open_main_window(&backend, &main_window, cx, agent_enabled)
                    .update(cx, |panel, cx| panel.open_about(about_info(), cx))
                    .ok();
            }
        });
        cx.on_action({
            let backend = backend.clone();
            let main_window = main_window.clone();
            move |_: &Settings, cx| {
                open_main_window(&backend, &main_window, cx, agent_enabled)
                    .update(cx, |panel, cx| panel.open_settings_page(cx))
                    .ok();
            }
        });
        cx.on_action(|_: &Quit, cx| {
            request_quit(cx);
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
                    request_quit(cx);
                }
            }
        })
        .detach();
    });
}
