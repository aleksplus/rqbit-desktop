use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, Size, Theme, ThemeMode,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    scroll::ScrollableElement,
    setting::{RenderOptions, SettingField, SettingGroup, SettingItem, SettingPage, Settings},
    v_flex,
};
use std::{collections::BTreeMap, num::NonZeroU32, sync::Arc};

use crate::{
    config::{HookConfig, HttpMethod, RqbitDesktopConfig, write_config},
    hooks::HookEvent,
    state::State,
};

type ConfigArc = Arc<parking_lot::RwLock<RqbitDesktopConfig>>;

/// Events emitted by [`SettingsPage`] to communicate with the parent view.
#[derive(Clone, Debug)]
pub enum SettingsPageEvent {
    /// User clicked **Back** – close the settings page.
    Back,
}

/// A full-page settings view built with the gpui-component `Settings` component.
///
/// Replaces the old `ConfigModal`. The config is stored in an `Arc<RwLock<...>>`
/// so that the `SettingField` closures (which receive `&App` / `&mut App`) can
/// read and mutate it.  An **Apply** button persists the config and reconfigures
/// the live session; the page stays open (with the error shown) if that fails,
/// so a failed reconfigure never silently rolls the user's changes back.
pub struct SettingsPage {
    state: Arc<State>,
    config: ConfigArc,
    focus_handle: FocusHandle,
    /// True while the session reconfigure spawned by **Apply** is running.
    applying: bool,
    /// Error text from the last failed Apply, rendered under the toolbar.
    apply_error: Option<SharedString>,
}

impl EventEmitter<SettingsPageEvent> for SettingsPage {}

impl SettingsPage {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>, state: Arc<State>) -> Self {
        let config = state.shared().read().config();
        Self {
            state,
            config: Arc::new(parking_lot::RwLock::new(config)),
            focus_handle: cx.focus_handle(),
            applying: false,
            apply_error: None,
        }
    }

    fn on_back(&mut self, cx: &mut Context<Self>) {
        cx.emit(SettingsPageEvent::Back);
    }

    fn on_apply(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.applying {
            return;
        }
        self.apply_error = None;

        let config = self.config.read().clone();

        // Apply the theme from config immediately so the UI updates without
        // waiting for the session reconfigure.
        let theme_mode = if config.theme == "dark" {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        };
        Theme::change(theme_mode, Some(_window), cx);

        if let Err(e) = write_config(&self.state.config_filename, &config) {
            self.apply_error = Some(format!("Error writing config: {e:#}").into());
            cx.notify();
            return;
        }

        // Reconfigure in the background: the old session is stopped and a new
        // one built, which takes a moment. Keep the page open until it settles
        // so failures are visible instead of silently discarding the changes.
        self.applying = true;
        cx.notify();

        let state = self.state.clone();
        let page = cx.entity();
        cx.spawn(async move |_, cx| {
            let result = state.configure(config).await;
            cx.update_entity(&page, |page: &mut SettingsPage, cx| {
                page.applying = false;
                match result {
                    Ok(()) => cx.emit(SettingsPageEvent::Back),
                    Err(e) => {
                        page.apply_error =
                            Some(format!("Error reconfiguring session: {e:#}").into());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn on_reset(&mut self, cx: &mut Context<Self>) {
        *self.config.write() = RqbitDesktopConfig::default();
        cx.notify();
    }
}

// ── Field helpers ──────────────────────────────────────────────────────────
// Each helper clones the ConfigArc so the `value` and `set_value` closures
// each own their own Arc (avoiding "borrow of moved value" errors).

fn switch_field(
    config: ConfigArc,
    get: fn(&RqbitDesktopConfig) -> bool,
    set: fn(&mut RqbitDesktopConfig, bool),
) -> SettingField<bool> {
    let cfg_get = config.clone();
    let cfg_set = config;
    SettingField::switch(
        move |_| get(&cfg_get.read()),
        move |val: bool, _| set(&mut cfg_set.write(), val),
    )
}

fn input_field(
    config: ConfigArc,
    get: fn(&RqbitDesktopConfig) -> String,
    set: fn(&mut RqbitDesktopConfig, SharedString),
) -> SettingField<SharedString> {
    let cfg_get = config.clone();
    let cfg_set = config;
    SettingField::input(
        move |_| get(&cfg_get.read()).into(),
        move |val: SharedString, _| set(&mut cfg_set.write(), val),
    )
}

/// State backing the download-location field: the text input plus the
/// subscription that writes typed changes back into the config.
struct DlInputState {
    input: Entity<InputState>,
    _subscription: gpui::Subscription,
}

/// A download-location field that pairs a text input with a **Browse…** button.
///
/// The button opens the native folder picker (macOS Finder / open panel).
/// On macOS this also triggers the OS "grant access to this folder" prompt the
/// first time, and the granted access persists for the app — so the user is
/// only asked once.
fn download_location_field(
    config: ConfigArc,
) -> impl Fn(&RenderOptions, &mut Window, &mut App) -> AnyElement {
    move |options, window, cx| {
        let value = get_dl_location(&config.read());
        let set = set_dl_location;
        let cfg = config.clone();

        let state_entity = window.use_keyed_state("download-location-input", cx, {
            let value = value.clone();
            move |window, cx| {
                let input = cx.new(|cx| InputState::new(window, cx).default_value(value.clone()));
                let _subscription = cx.subscribe(&input, {
                    let cfg = cfg.clone();
                    move |_, input, event: &InputEvent, cx| {
                        if matches!(event, InputEvent::Change) {
                            let v = input.read(cx).value();
                            set(&mut cfg.write(), v);
                        }
                    }
                });
                DlInputState {
                    input,
                    _subscription,
                }
            }
        });

        // Keep the displayed text in sync when the config changes externally
        // (e.g. after picking a folder via the Browse button).
        state_entity.update(cx, |state, cx| {
            if state.input.read(cx).value() != value {
                state.input.update(cx, |input, cx| {
                    input.set_value(value.clone(), window, cx);
                });
            }
        });

        let state = state_entity.read(cx);

        h_flex()
            .gap_2()
            .child(
                Input::new(&state.input)
                    .id("download-location")
                    .flex_1()
                    .disabled(options.is_disabled())
                    .with_size(options.size()),
            )
            .child(
                Button::new("browse-download-folder")
                    .label("Browse…")
                    .disabled(options.is_disabled())
                    .on_click({
                        let cfg = config.clone();
                        move |_, _window, cx| {
                            let rx = cx.prompt_for_paths(PathPromptOptions {
                                files: false,
                                directories: true,
                                multiple: false,
                                prompt: Some("Select download folder".into()),
                            });
                            let cfg = cfg.clone();
                            cx.spawn(async move |cx| {
                                if let Ok(Ok(Some(paths))) = rx.await
                                    && let Some(path) = paths.into_iter().next()
                                {
                                    set(
                                        &mut cfg.write(),
                                        path.to_string_lossy().into_owned().into(),
                                    );
                                    cx.update(|cx| cx.refresh_windows());
                                }
                            })
                            .detach();
                        }
                    }),
            )
            .into_any_element()
    }
}

/// State backing the load-torrents-location input.
struct LoadTlsInputState {
    input: Entity<InputState>,
    _subscription: gpui::Subscription,
}

/// A load-torrents-location field that pairs a text input with a Browse button.
fn load_torrents_location_field(
    config: ConfigArc,
) -> impl Fn(&RenderOptions, &mut Window, &mut App) -> AnyElement {
    move |options, window, cx| {
        let value = get_load_torrents_location(&config.read());
        let set = set_load_torrents_location;
        let cfg = config.clone();

        let state_entity = window.use_keyed_state("load-torrents-location-input", cx, {
            let value = value.clone();
            move |window, cx| {
                let input = cx.new(|cx| InputState::new(window, cx).default_value(value.clone()));
                let _subscription = cx.subscribe(&input, {
                    let cfg = cfg.clone();
                    move |_, input, event: &InputEvent, cx| {
                        if matches!(event, InputEvent::Change) {
                            let v = input.read(cx).value();
                            set(&mut cfg.write(), v);
                        }
                    }
                });
                LoadTlsInputState {
                    input,
                    _subscription,
                }
            }
        });

        // Keep the displayed text in sync when config changes externally.
        state_entity.update(cx, |state, cx| {
            if state.input.read(cx).value() != value {
                state.input.update(cx, |input, cx| {
                    input.set_value(value.clone(), window, cx);
                });
            }
        });

        let state = state_entity.read(cx);

        h_flex()
            .gap_2()
            .child(
                Input::new(&state.input)
                    .id("load-torrents-location")
                    .flex_1()
                    .disabled(options.is_disabled())
                    .with_size(options.size()),
            )
            .child(
                Button::new("browse-load-torrents-folder")
                    .label("Browse…")
                    .disabled(options.is_disabled())
                    .on_click({
                        let cfg = config.clone();
                        move |_, _window, cx| {
                            let rx = cx.prompt_for_paths(PathPromptOptions {
                                files: false,
                                directories: true,
                                multiple: false,
                                prompt: Some("Select load torrent folder".into()),
                            });
                            let cfg = cfg.clone();
                            cx.spawn(async move |cx| {
                                if let Ok(Ok(Some(paths))) = rx.await
                                    && let Some(path) = paths.into_iter().next()
                                {
                                    set(
                                        &mut cfg.write(),
                                        path.to_string_lossy().into_owned().into(),
                                    );
                                    cx.update(|cx| cx.refresh_windows());
                                }
                            })
                            .detach();
                        }
                    }),
            )
            .into_any_element()
    }
}

/// State backing the move-torrents-location input.
struct MoveTlsInputState {
    input: Entity<InputState>,
    _subscription: gpui::Subscription,
}

/// A move-torrents-location field that pairs a text input with a Browse button.
fn move_torrents_location_field(
    config: ConfigArc,
) -> impl Fn(&RenderOptions, &mut Window, &mut App) -> AnyElement {
    move |options, window, cx| {
        let value = get_move_torrents_location(&config.read());
        let set = set_move_torrents_location;
        let cfg = config.clone();

        let state_entity = window.use_keyed_state("move-torrents-location-input", cx, {
            let value = value.clone();
            move |window, cx| {
                let input = cx.new(|cx| InputState::new(window, cx).default_value(value.clone()));
                let _subscription = cx.subscribe(&input, {
                    let cfg = cfg.clone();
                    move |_, input, event: &InputEvent, cx| {
                        if matches!(event, InputEvent::Change) {
                            let v = input.read(cx).value();
                            set(&mut cfg.write(), v);
                        }
                    }
                });
                MoveTlsInputState {
                    input,
                    _subscription,
                }
            }
        });

        // Keep the displayed text in sync when config changes externally.
        state_entity.update(cx, |state, cx| {
            if state.input.read(cx).value() != value {
                state.input.update(cx, |input, cx| {
                    input.set_value(value.clone(), window, cx);
                });
            }
        });

        let state = state_entity.read(cx);

        h_flex()
            .gap_2()
            .child(
                Input::new(&state.input)
                    .id("move-torrents-location")
                    .flex_1()
                    .disabled(options.is_disabled())
                    .with_size(options.size()),
            )
            .child(
                Button::new("browse-move-torrents-folder")
                    .label("Browse…")
                    .disabled(options.is_disabled())
                    .on_click({
                        let cfg = config.clone();
                        move |_, _window, cx| {
                            let rx = cx.prompt_for_paths(PathPromptOptions {
                                files: false,
                                directories: true,
                                multiple: false,
                                prompt: Some("Select move torrent folder".into()),
                            });
                            let cfg = cfg.clone();
                            cx.spawn(async move |cx| {
                                if let Ok(Ok(Some(paths))) = rx.await
                                    && let Some(path) = paths.into_iter().next()
                                {
                                    set(
                                        &mut cfg.write(),
                                        path.to_string_lossy().into_owned().into(),
                                    );
                                    cx.update(|cx| cx.refresh_windows());
                                }
                            })
                            .detach();
                        }
                    }),
            )
            .into_any_element()
    }
}

// ── Config accessors ────────────────────────────────────────────────────────

fn get_dl_location(c: &RqbitDesktopConfig) -> String {
    c.default_download_location.to_string_lossy().to_string()
}
fn get_load_torrents_location(c: &RqbitDesktopConfig) -> String {
    c.load_torrents_folder.to_string_lossy().to_string()
}
fn set_load_torrents_location(c: &mut RqbitDesktopConfig, v: SharedString) {
    c.load_torrents_folder = v.to_string().into();
}

fn get_auto_add_paused(c: &RqbitDesktopConfig) -> bool {
    c.auto_add_paused
}
fn set_auto_add_paused(c: &mut RqbitDesktopConfig, v: bool) {
    c.auto_add_paused = v;
}
fn set_dl_location(c: &mut RqbitDesktopConfig, v: SharedString) {
    c.default_download_location = v.to_string().into();
}

fn get_dht_disable(c: &RqbitDesktopConfig) -> bool {
    !c.dht.disable
}
fn set_dht_disable(c: &mut RqbitDesktopConfig, v: bool) {
    c.dht.disable = !v;
}

fn get_dht_persist(c: &RqbitDesktopConfig) -> bool {
    !c.dht.disable_persistence
}
fn set_dht_persist(c: &mut RqbitDesktopConfig, v: bool) {
    c.dht.disable_persistence = !v;
}

fn get_dht_filename(c: &RqbitDesktopConfig) -> String {
    c.dht.persistence_filename.to_string_lossy().to_string()
}
fn set_dht_filename(c: &mut RqbitDesktopConfig, v: SharedString) {
    c.dht.persistence_filename = v.to_string().into();
}

fn get_persist_disable(c: &RqbitDesktopConfig) -> bool {
    !c.persistence.disable
}
fn set_persist_disable(c: &mut RqbitDesktopConfig, v: bool) {
    c.persistence.disable = !v;
}

fn get_persist_folder(c: &RqbitDesktopConfig) -> String {
    c.persistence.folder.to_string_lossy().to_string()
}
fn set_persist_folder(c: &mut RqbitDesktopConfig, v: SharedString) {
    c.persistence.folder = v.to_string().into();
}

fn get_fastresume(c: &RqbitDesktopConfig) -> bool {
    c.persistence.fastresume
}
fn set_fastresume(c: &mut RqbitDesktopConfig, v: bool) {
    c.persistence.fastresume = v;
}

fn get_dl_rate(c: &RqbitDesktopConfig) -> String {
    c.ratelimits
        .download_bps
        .map(|v| v.to_string())
        .unwrap_or_default()
}
fn set_dl_rate(c: &mut RqbitDesktopConfig, v: SharedString) {
    c.ratelimits.download_bps = v
        .parse::<u32>()
        .ok()
        .filter(|&v| v > 0)
        .and_then(NonZeroU32::new);
}

fn get_ul_rate(c: &RqbitDesktopConfig) -> String {
    c.ratelimits
        .upload_bps
        .map(|v| v.to_string())
        .unwrap_or_default()
}
fn set_ul_rate(c: &mut RqbitDesktopConfig, v: SharedString) {
    c.ratelimits.upload_bps = v
        .parse::<u32>()
        .ok()
        .filter(|&v| v > 0)
        .and_then(NonZeroU32::new);
}

fn get_tcp_listen(c: &RqbitDesktopConfig) -> bool {
    c.connections.enable_tcp_listen
}
fn set_tcp_listen(c: &mut RqbitDesktopConfig, v: bool) {
    c.connections.enable_tcp_listen = v;
}

fn get_utp(c: &RqbitDesktopConfig) -> bool {
    c.connections.enable_utp
}
fn set_utp(c: &mut RqbitDesktopConfig, v: bool) {
    c.connections.enable_utp = v;
}

fn get_upnp_fwd(c: &RqbitDesktopConfig) -> bool {
    c.connections.enable_upnp_port_forward
}
fn set_upnp_fwd(c: &mut RqbitDesktopConfig, v: bool) {
    c.connections.enable_upnp_port_forward = v;
}

fn get_listen_port(c: &RqbitDesktopConfig) -> String {
    c.connections.listen_port.to_string()
}
fn set_listen_port(c: &mut RqbitDesktopConfig, v: SharedString) {
    if let Ok(port) = v.parse::<u16>() {
        c.connections.listen_port = port;
    }
}

fn get_tcp_outgoing(c: &RqbitDesktopConfig) -> bool {
    c.connections.enable_tcp_outgoing
}
fn set_tcp_outgoing(c: &mut RqbitDesktopConfig, v: bool) {
    c.connections.enable_tcp_outgoing = v;
}

fn get_socks(c: &RqbitDesktopConfig) -> String {
    c.connections.socks_proxy.clone()
}
fn set_socks(c: &mut RqbitDesktopConfig, v: SharedString) {
    c.connections.socks_proxy = v.to_string();
}

fn get_peer_ct(c: &RqbitDesktopConfig) -> String {
    c.connections.peer_connect_timeout.as_secs().to_string()
}
fn set_peer_ct(c: &mut RqbitDesktopConfig, v: SharedString) {
    if let Ok(secs) = v.parse::<u64>() {
        c.connections.peer_connect_timeout = std::time::Duration::from_secs(secs);
    }
}

fn get_peer_rw(c: &RqbitDesktopConfig) -> String {
    c.connections.peer_read_write_timeout.as_secs().to_string()
}
fn set_peer_rw(c: &mut RqbitDesktopConfig, v: SharedString) {
    if let Ok(secs) = v.parse::<u64>() {
        c.connections.peer_read_write_timeout = std::time::Duration::from_secs(secs);
    }
}

fn get_http_disable(c: &RqbitDesktopConfig) -> bool {
    !c.http_api.disable
}
fn set_http_disable(c: &mut RqbitDesktopConfig, v: bool) {
    c.http_api.disable = !v;
}

fn get_http_ro(c: &RqbitDesktopConfig) -> bool {
    c.http_api.read_only
}
fn set_http_ro(c: &mut RqbitDesktopConfig, v: bool) {
    c.http_api.read_only = v;
}

fn get_http_addr(c: &RqbitDesktopConfig) -> String {
    c.http_api.listen_addr.to_string()
}
fn set_http_addr(c: &mut RqbitDesktopConfig, v: SharedString) {
    if let Ok(addr) = v.parse() {
        c.http_api.listen_addr = addr;
    }
}

fn get_upnp_server(c: &RqbitDesktopConfig) -> bool {
    c.upnp.enable_server
}
fn set_upnp_server(c: &mut RqbitDesktopConfig, v: bool) {
    c.upnp.enable_server = v;
}

fn get_upnp_name(c: &RqbitDesktopConfig) -> String {
    c.upnp.server_friendly_name.clone().unwrap_or_default()
}
fn set_upnp_name(c: &mut RqbitDesktopConfig, v: SharedString) {
    let s = v.to_string();
    c.upnp.server_friendly_name = if s.is_empty() { None } else { Some(s) };
}

fn get_web_view_enable(c: &RqbitDesktopConfig) -> bool {
    !c.web_view.disable
}
fn set_web_view_enable(c: &mut RqbitDesktopConfig, v: bool) {
    c.web_view.disable = !v;
}

fn get_web_view_addr(c: &RqbitDesktopConfig) -> String {
    c.web_view.listen_addr.to_string()
}
fn set_web_view_addr(c: &mut RqbitDesktopConfig, v: SharedString) {
    if let Ok(addr) = v.parse() {
        c.web_view.listen_addr = addr;
    }
}

fn get_web_view_port(c: &RqbitDesktopConfig) -> String {
    c.web_view.listen_addr.port().to_string()
}
fn set_web_view_port(c: &mut RqbitDesktopConfig, v: SharedString) {
    if let Ok(port) = v.parse::<u16>() {
        c.web_view.listen_addr.set_port(port);
    }
}

/// Get the current move-torrents-folder configuration value.
fn get_move_torrents_location(c: &RqbitDesktopConfig) -> String {
    c.move_torrents_folder.to_string_lossy().to_string()
}

/// Set the move-torrents-folder configuration value.
fn set_move_torrents_location(c: &mut RqbitDesktopConfig, v: SharedString) {
    c.move_torrents_folder = v.to_string().into();
}

fn get_theme_dark(c: &RqbitDesktopConfig) -> bool {
    c.theme == "dark"
}
fn set_theme_dark(c: &mut RqbitDesktopConfig, v: bool) {
    c.theme = if v {
        "dark".to_string()
    } else {
        "light".to_string()
    };
}

// ── Hooks ──────────────────────────────────────────────────────────────────

/// Read a field of hook `ix`, falling back when the hook was removed while
/// its fields were still rendered.
fn with_hook<T>(
    config: &ConfigArc,
    ix: usize,
    get: impl FnOnce(&HookConfig) -> T,
    default: T,
) -> T {
    config
        .read()
        .hooks
        .hooks
        .get(ix)
        .map(get)
        .unwrap_or(default)
}

/// Mutate hook `ix` if it still exists.
fn mutate_hook(config: &ConfigArc, ix: usize, f: impl FnOnce(&mut HookConfig)) {
    if let Some(h) = config.write().hooks.hooks.get_mut(ix) {
        f(h);
    }
}

/// Remove hook `ix`; returns whether it existed.
fn remove_hook(config: &ConfigArc, ix: usize) -> bool {
    let mut g = config.write();
    if ix < g.hooks.hooks.len() {
        g.hooks.hooks.remove(ix);
        true
    } else {
        false
    }
}

/// Append a new hook with a default configuration.
fn add_hook(config: &ConfigArc) {
    let mut g = config.write();
    let n = g.hooks.hooks.len() + 1;
    g.hooks.hooks.push(HookConfig {
        name: format!("Hook {n}"),
        ..Default::default()
    });
}

fn hook_switch_field(
    config: ConfigArc,
    ix: usize,
    get: fn(&HookConfig) -> bool,
    set: fn(&mut HookConfig, bool),
) -> SettingField<bool> {
    let cfg_get = config.clone();
    let cfg_set = config;
    SettingField::switch(
        move |_| with_hook(&cfg_get, ix, get, false),
        move |val, _| mutate_hook(&cfg_set, ix, |h| set(h, val)),
    )
}

fn hook_input_field(
    config: ConfigArc,
    ix: usize,
    get: fn(&HookConfig) -> String,
    set: fn(&mut HookConfig, SharedString),
) -> SettingField<SharedString> {
    let cfg_get = config.clone();
    let cfg_set = config;
    SettingField::input(
        move |_| with_hook(&cfg_get, ix, get, String::new()).into(),
        move |val, _| mutate_hook(&cfg_set, ix, |h| set(h, val)),
    )
}

/// POST/GET dropdown for a hook's HTTP method.
fn hook_method_field(config: ConfigArc, ix: usize) -> SettingField<SharedString> {
    let cfg_get = config.clone();
    let cfg_set = config;
    SettingField::dropdown(
        vec![("post".into(), "POST".into()), ("get".into(), "GET".into())],
        move |_| {
            with_hook(
                &cfg_get,
                ix,
                |h| match h.method {
                    HttpMethod::Post => "post",
                    HttpMethod::Get => "get",
                },
                "post",
            )
            .into()
        },
        move |val, _| {
            mutate_hook(&cfg_set, ix, |h| {
                h.method = if val == "get" {
                    HttpMethod::Get
                } else {
                    HttpMethod::Post
                };
            })
        },
    )
}

/// One row of four checkboxes selecting which events a hook subscribes to.
fn hook_events_field(
    config: ConfigArc,
    ix: usize,
) -> impl Fn(&RenderOptions, &mut Window, &mut App) -> AnyElement {
    move |_options, _window, _cx| {
        let mut row = h_flex().gap_4().flex_wrap();
        for event in HookEvent::ALL {
            let checked = with_hook(&config, ix, |h| h.events.contains(&event), false);
            let cfg = config.clone();
            row = row.child(
                Checkbox::new(SharedString::from(format!(
                    "hook-{ix}-event-{}",
                    event.as_str()
                )))
                .checked(checked)
                .label(event.label())
                .on_click(move |_, _, cx: &mut App| {
                    let cfg = cfg.clone();
                    mutate_hook(&cfg, ix, |h| {
                        if h.events.contains(&event) {
                            h.events.retain(|e| *e != event);
                        } else {
                            h.events.push(event);
                        }
                    });
                    cx.refresh_windows();
                }),
            );
        }
        row.into_any_element()
    }
}

/// Multi-line editor for the hook's extra headers, one `Key: Value` per line.
fn hook_headers_field(
    config: ConfigArc,
    ix: usize,
) -> impl Fn(&RenderOptions, &mut Window, &mut App) -> AnyElement {
    move |_options, window, cx| {
        let value = with_hook(&config, ix, headers_to_text, String::new());
        let cfg = config.clone();

        let state_entity =
            window.use_keyed_state(SharedString::from(format!("hook-{ix}-headers")), cx, {
                let value = value.clone();
                move |window, cx| {
                    let input = cx.new(|cx| {
                        TextareaState::new(window, cx)
                            .default_value(value.clone())
                            .auto_grow(1, 6)
                    });
                    let _subscription = cx.subscribe(&input, {
                        let cfg = cfg.clone();
                        move |_, input, event: &InputEvent, cx| {
                            if matches!(event, InputEvent::Change) {
                                let v = input.read(cx).value();
                                mutate_hook(&cfg, ix, |h| set_headers_text(h, &v));
                            }
                        }
                    });
                    HookInputState {
                        input,
                        _subscription,
                    }
                }
            });

        // Keep the displayed text in sync when the config changes externally
        // (e.g. another hook shifting indices after add/remove).
        state_entity.update(cx, |state, cx| {
            if state.input.read(cx).value() != value {
                state.input.update(cx, |input, cx| {
                    input.set_value(value.clone(), window, cx);
                });
            }
        });

        let state = state_entity.read(cx);
        div()
            .border_1()
            .border_color(cx.theme().border)
            .rounded_md()
            .p_2()
            .child(Textarea::new(&state.input))
            .into_any_element()
    }
}

/// State backing the headers textarea (input + its change subscription).
struct HookInputState {
    input: Entity<TextareaState>,
    _subscription: gpui::Subscription,
}

fn headers_to_text(h: &HookConfig) -> String {
    h.headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn set_headers_text(h: &mut HookConfig, text: &str) {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            if !k.is_empty() {
                map.insert(k.to_string(), v.to_string());
            }
        }
    }
    h.headers = map;
}

/// "Remove Hook" button shown at the bottom of each hook's settings.
fn hook_remove_field(
    config: ConfigArc,
    ix: usize,
) -> impl Fn(&RenderOptions, &mut Window, &mut App) -> AnyElement {
    move |_options, _window, _cx| {
        let cfg = config.clone();
        Button::new(SharedString::from(format!("hook-{ix}-remove")))
            .outline()
            .label("Remove Hook")
            .on_click(move |_, _, cx: &mut App| {
                if remove_hook(&cfg, ix) {
                    cx.refresh_windows();
                }
            })
            .into_any_element()
    }
}

/// "Add Hook" button shown in the last group of the Hooks page.
fn add_hook_field(
    config: ConfigArc,
) -> impl Fn(&RenderOptions, &mut Window, &mut App) -> AnyElement {
    move |_options, _window, _cx| {
        let cfg = config.clone();
        Button::new("hooks-add")
            .label("Add Hook")
            .on_click(move |_, _, cx: &mut App| {
                add_hook(&cfg);
                cx.refresh_windows();
            })
            .into_any_element()
    }
}

fn get_hook_enabled(h: &HookConfig) -> bool {
    h.enabled
}
fn set_hook_enabled(h: &mut HookConfig, v: bool) {
    h.enabled = v;
}
fn get_hook_name(h: &HookConfig) -> String {
    h.name.clone()
}
fn set_hook_name(h: &mut HookConfig, v: SharedString) {
    h.name = v.to_string();
}
fn get_hook_url(h: &HookConfig) -> String {
    h.url.clone()
}
fn set_hook_url(h: &mut HookConfig, v: SharedString) {
    h.url = v.to_string();
}
fn get_hook_timeout(h: &HookConfig) -> String {
    h.timeout_secs.to_string()
}
fn set_hook_timeout(h: &mut HookConfig, v: SharedString) {
    if let Ok(secs) = v.parse::<u64>() {
        h.timeout_secs = secs.max(1);
    }
}
fn get_hook_poll(h: &HookConfig) -> String {
    h.poll_secs.to_string()
}
fn set_hook_poll(h: &mut HookConfig, v: SharedString) {
    if let Ok(secs) = v.parse::<u64>() {
        h.poll_secs = secs.max(2);
    }
}

/// Build the groups of the Hooks settings page: one per configured hook,
/// plus a trailing group with the add button.
fn hook_groups(config: ConfigArc) -> Vec<SettingGroup> {
    let count = config.read().hooks.hooks.len();
    let mut groups: Vec<SettingGroup> = (0..count)
        .map(|ix| {
            let title = with_hook(&config, ix, |h| h.name.clone(), String::new());
            let title = if title.trim().is_empty() {
                format!("Hook {}", ix + 1)
            } else {
                title
            };
            SettingGroup::new().title(title).items(vec![
                SettingItem::new(
                    "Enabled",
                    hook_switch_field(config.clone(), ix, get_hook_enabled, set_hook_enabled),
                )
                .description("Uncheck to pause this hook without deleting it."),
                SettingItem::new(
                    "Name",
                    hook_input_field(config.clone(), ix, get_hook_name, set_hook_name),
                )
                .layout(Axis::Vertical),
                SettingItem::new(
                    "URL",
                    hook_input_field(config.clone(), ix, get_hook_url, set_hook_url),
                )
                .layout(Axis::Vertical)
                .description("e.g. http://127.0.0.1:8080/hook"),
                SettingItem::new("Method", hook_method_field(config.clone(), ix)),
                SettingItem::new(
                    "Timeout (seconds)",
                    hook_input_field(config.clone(), ix, get_hook_timeout, set_hook_timeout),
                ),
                SettingItem::new(
                    "Poll (seconds)",
                    hook_input_field(config.clone(), ix, get_hook_poll, set_hook_poll),
                )
                .description("Polling interval for the \"Torrent columns\" event."),
                SettingItem::new(
                    "Events",
                    SettingField::render(hook_events_field(config.clone(), ix)),
                )
                .layout(Axis::Vertical)
                .description("Which lifecycle events trigger this hook."),
                SettingItem::new(
                    "Headers (one per line, \"Key: Value\")",
                    SettingField::render(hook_headers_field(config.clone(), ix)),
                )
                .layout(Axis::Vertical),
                SettingItem::new(
                    "",
                    SettingField::render(hook_remove_field(config.clone(), ix)),
                )
                .layout(Axis::Vertical),
            ])
        })
        .collect();
    groups.push(
        SettingGroup::new()
            .title("Manage")
            .items(vec![SettingItem::new(
                "",
                SettingField::render(add_hook_field(config)),
            )]),
    );
    groups
}

// ── Render ─────────────────────────────────────────────────────────────────

impl Render for SettingsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let config = self.config.clone();
        let applying = self.applying;
        let apply_error = self.apply_error.clone();

        v_flex()
            .size_full()
            .gap_0()
            // Toolbar
            .child(
                h_flex()
                    .gap_2()
                    .pl(px(10.))
                    .pt(px(5.))
                    .pr(px(10.))
                    .pb(px(5.))
                    .bg(theme.background)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("settings-back")
                            .label("← Back")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.on_back(cx))),
                    )
                    .child(
                        Button::new("settings-reset")
                            .label("Reset to Default")
                            .disabled(applying)
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.on_reset(cx))),
                    )
                    .child(
                        h_flex().flex_1().justify_end().child(
                            Button::new("settings-apply")
                                .primary()
                                .label(if applying { "Applying…" } else { "Apply" })
                                .disabled(applying)
                                .small()
                                .on_click(cx.listener(|this, _, window, cx| this.on_apply(window, cx))),
                        ),
                    ),
            )
            // Apply failure banner (kept visible until the next Apply)
            .when_some(apply_error, |this, error| {
                this.child(
                    h_flex()
                        .id("settings-apply-error")
                        .px_4()
                        .py_2()
                        .bg(theme.danger_foreground.opacity(0.1))
                        .border_b_1()
                        .border_color(theme.danger)
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.danger_foreground)
                                .child(error),
                        ),
                )
            })
            // Settings content
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(
                        Settings::new("rqbit-settings")
                            .with_size(Size::Medium)
                            .pages(vec![
                                // ── General ──
                                SettingPage::new("General")
                                    .default_open(true)
                                    .group(SettingGroup::new().title("Download").items(vec![
                                        SettingItem::new(
                                            "Default download folder",
                                            SettingField::render(download_location_field(config.clone())),
                                        )
                                        .layout(Axis::Vertical),
                                        SettingItem::new(
                                            "Autoload torrents from folder",
                                            SettingField::render(load_torrents_location_field(config.clone())),
                                        )
                                        .layout(Axis::Vertical)
                                        .description(
                                            "Watched (recursively) for new .torrent and .magnet files to add automatically. Empty disables.",
                                        ),
                                        SettingItem::new(
                                            "Add autoloaded torrents paused",
                                            switch_field(config.clone(), get_auto_add_paused, set_auto_add_paused),
                                        )
                                        .description(
                                            "Torrents picked up from the watch folder start paused; start them from the torrent list when ready.",
                                        ),
                                        SettingItem::new(
                                            "Move added torrents to folder",
                                            SettingField::render(move_torrents_location_field(config.clone())),
                                        )
                                        .layout(Axis::Vertical)
                                        .description(
                                            "After a torrent is added (from the watch folder or the file picker), its source file is moved here. Empty disables.",
                                        ),
                                    ]))
                                    .group(
                                        SettingGroup::new().title("Appearance").items(vec![
                                            SettingItem::new(
                                                "Dark theme",
                                                switch_field(config.clone(), get_theme_dark, set_theme_dark),
                                            )
                                            .description("Use a dark color scheme for the application UI."),
                                        ]),
                                    ),
                                // ── DHT ──
                                SettingPage::new("DHT").group(
                                    SettingGroup::new().title("DHT Settings").items(vec![
                                        SettingItem::new(
                                            "Enable DHT",
                                            switch_field(config.clone(), get_dht_disable, set_dht_disable),
                                        )
                                        .description("Enable the Distributed Hash Table for peer discovery."),
                                        SettingItem::new(
                                            "Enable DHT persistence",
                                            switch_field(config.clone(), get_dht_persist, set_dht_persist),
                                        )
                                        .description("Persist DHT routing table across restarts."),
                                        SettingItem::new(
                                            "DHT persistence filename",
                                            input_field(config.clone(), get_dht_filename, set_dht_filename),
                                        )
                                        .layout(Axis::Vertical),
                                    ]),
                                ),
                                // ── Session ──
                                SettingPage::new("Session")
                                    .group(
                                        SettingGroup::new().title("Persistence").items(vec![
                                            SettingItem::new(
                                                "Enable session persistence",
                                                switch_field(config.clone(), get_persist_disable, set_persist_disable),
                                            )
                                            .description("Save torrent state to disk for resume on restart."),
                                            SettingItem::new(
                                                "Persistence folder",
                                                input_field(config.clone(), get_persist_folder, set_persist_folder),
                                            )
                                            .layout(Axis::Vertical),
                                            SettingItem::new(
                                                "Enable fast resume (experimental)",
                                                switch_field(config.clone(), get_fastresume, set_fastresume),
                                            ),
                                        ]),
                                    )
                                    .group(
                                        SettingGroup::new().title("Rate Limits").items(vec![
                                            SettingItem::new(
                                                "Download rate limit (bytes/sec, 0 = unlimited)",
                                                input_field(config.clone(), get_dl_rate, set_dl_rate),
                                            )
                                            .layout(Axis::Vertical),
                                            SettingItem::new(
                                                "Upload rate limit (bytes/sec, 0 = unlimited)",
                                                input_field(config.clone(), get_ul_rate, set_ul_rate),
                                            )
                                            .layout(Axis::Vertical),
                                        ]),
                                    ),
                                // ── Connection ──
                                SettingPage::new("Connection")
                                    .group(
                                        SettingGroup::new().title("Listening").items(vec![
                                            SettingItem::new(
                                                "Listen on TCP",
                                                switch_field(config.clone(), get_tcp_listen, set_tcp_listen),
                                            ),
                                            SettingItem::new(
                                                "Listen on uTP (over UDP)",
                                                switch_field(config.clone(), get_utp, set_utp),
                                            ),
                                            SettingItem::new(
                                                "Advertise port over UPnP",
                                                switch_field(config.clone(), get_upnp_fwd, set_upnp_fwd),
                                            ),
                                            SettingItem::new(
                                                "Listen port",
                                                input_field(config.clone(), get_listen_port, set_listen_port),
                                            )
                                            .layout(Axis::Vertical),
                                        ]),
                                    )
                                    .group(
                                        SettingGroup::new().title("Outgoing").items(vec![
                                            SettingItem::new(
                                                "Enable outgoing TCP",
                                                switch_field(config.clone(), get_tcp_outgoing, set_tcp_outgoing),
                                            ),
                                            SettingItem::new(
                                                "SOCKS proxy",
                                                input_field(config.clone(), get_socks, set_socks),
                                            )
                                            .layout(Axis::Vertical),
                                            SettingItem::new(
                                                "Peer connect timeout (seconds)",
                                                input_field(config.clone(), get_peer_ct, set_peer_ct),
                                            )
                                            .layout(Axis::Vertical),
                                            SettingItem::new(
                                                "Peer read/write timeout (seconds)",
                                                input_field(config.clone(), get_peer_rw, set_peer_rw),
                                            )
                                            .layout(Axis::Vertical),
                                        ]),
                                    ),
                                // ── HTTP API ──
                                SettingPage::new("HTTP API").group(
                                    SettingGroup::new().title("HTTP API Settings").items(vec![
                                        SettingItem::new(
                                            "Enable HTTP API",
                                            switch_field(config.clone(), get_http_disable, set_http_disable),
                                        ),
                                        SettingItem::new(
                                            "Read only",
                                            switch_field(config.clone(), get_http_ro, set_http_ro),
                                        )
                                        .description(
                                            "When enabled, the HTTP API will not accept state-changing requests.",
                                        ),
                                        SettingItem::new(
                                            "HTTP API listen address",
                                            input_field(config.clone(), get_http_addr, set_http_addr),
                                        )
                                        .layout(Axis::Vertical),
                                    ]),
                                ),
                                // ── Web View ──
                                SettingPage::new("Web View").group(
                                    SettingGroup::new().title("Browser UI").items(vec![
                                        SettingItem::new(
                                            "Enable web view",
                                            switch_field(config.clone(), get_web_view_enable, set_web_view_enable),
                                        )
                                        .description(
                                            "Serve the browser UI (the same web client app-daemon serves) \
                                             for this session, so the same torrents are visible in a browser. \
                                             Open http://<listen address>/ after applying.",
                                        ),
                                        SettingItem::new(
                                            "Web view port",
                                            input_field(config.clone(), get_web_view_port, set_web_view_port),
                                        )
                                        .layout(Axis::Vertical)
                                        .description(
                                            "TCP port of the browser UI. Editing it only changes \
                                             the port part of the listen address below.",
                                        ),
                                        SettingItem::new(
                                            "Web view listen address",
                                            input_field(config.clone(), get_web_view_addr, set_web_view_addr),
                                        )
                                        .layout(Axis::Vertical)
                                        .description(
                                            "Full ip:port to bind. Defaults to 127.0.0.1:3081 — \
                                             a different port than app-daemon's 3080, so both \
                                             can run at the same time.",
                                        ),
                                    ]),
                                ),
                                // ── UPnP Server ──
                                SettingPage::new("UPnP Server").group(
                                    SettingGroup::new().title("UPnP Media Server").items(vec![
                                        SettingItem::new(
                                            "Enable UPnP media server",
                                            switch_field(config.clone(), get_upnp_server, set_upnp_server),
                                        )
                                        .description(
                                            "Serve downloaded media over UPnP on the local network.",
                                        ),
                                        SettingItem::new(
                                            "UPnP friendly name",
                                            input_field(config.clone(), get_upnp_name, set_upnp_name),
                                        )
                                        .layout(Axis::Vertical),
                                    ]),
                                ),
                                // ── Hooks ──
                                SettingPage::new("Hooks")
                                    .description(
                                        "Webhook-style notifications for torrent lifecycle events, \
                                         delivered as JSON over HTTP. See the hooks module docs \
                                         for the payload contract.",
                                    )
                                    .groups(hook_groups(config.clone())),
                            ]),
                    ),
            )
    }
}

impl Focusable for SettingsPage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    // NOTE: no `use super::*` / `use gpui::*` here — a glob-imported `test`
    // macro shadows the built-in #[test] that #[gpui_kit::test] expands to.
    use super::SettingsPage;
    use crate::folder_watcher::tests::TempDir;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext as _, TestAppContext, px, size};

    /// The port field must only rewrite the port of the web view listen
    /// address, and stay in sync with the full-address field.
    #[test]
    fn web_view_port_updates_only_port() {
        use super::{get_web_view_addr, get_web_view_port, set_web_view_addr, set_web_view_port};
        use crate::config::RqbitDesktopConfig;

        let mut cfg = RqbitDesktopConfig::default();
        assert_eq!(get_web_view_port(&cfg), "3081");

        set_web_view_port(&mut cfg, "4000".into());
        assert_eq!(get_web_view_addr(&cfg), "127.0.0.1:4000");

        // Invalid input is ignored, not written.
        set_web_view_port(&mut cfg, "not-a-port".into());
        assert_eq!(get_web_view_addr(&cfg), "127.0.0.1:4000");

        // Changing the full address is reflected by the port getter.
        set_web_view_addr(&mut cfg, "0.0.0.0:5000".into());
        assert_eq!(get_web_view_port(&cfg), "5000");
    }

    /// End-to-end reproduction of the reported bug: open settings, click the
    /// real Browse button for the autoload folder, answer the (queued, native)
    /// path prompt, and check that the picked directory lands both in the
    /// displayed input and in the page config that Apply persists.
    #[gpui_kit::test]
    fn browse_retains_load_and_move_folders(cx: &mut TestAppContext) {
        // librqbit sessions require a tokio runtime; the UI interactions below
        // never await tokio I/O on this thread.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let watch_dir = TempDir::new("ui-watch");
        let move_dir = TempDir::new("ui-move");
        let scratch = TempDir::new("ui-scratch");
        let config_filename = scratch.path().join("config.json");
        let config_filename = config_filename.to_str().unwrap().to_owned();

        let state = std::sync::Arc::new(rt.block_on(crate::state::tests::test_state(
            crate::state::tests::quiet_config(),
            config_filename,
        )));

        let (window_handle, page) = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(
                gpui::WindowOptions {
                    window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds {
                        origin: gpui::Point::default(),
                        size: size(px(1100.), px(800.)),
                    })),
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| SettingsPage::new(window, cx, state.clone())),
            )
            .expect("open test window")
        });

        let watch_path = watch_dir.path().to_path_buf();
        let move_path = move_dir.path().to_path_buf();

        cx.update_window(window_handle, |_, window, cx| {
            window.render_frame(cx);
            // Fields start empty.
            assert_eq!(window.find("load-torrents-location").value(), Some(""));
            assert_eq!(window.find("move-torrents-location").value(), Some(""));

            // Pick the autoload folder through the real button + native prompt.
            window.click("browse-load-torrents-folder", cx);
        })
        .unwrap();
        cx.simulate_path_prompt_response({
            let watch_path = watch_path.clone();
            move |_| Some(vec![watch_path.clone()])
        });
        cx.run_until_parked();

        cx.update_window(window_handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.find("load-torrents-location").value(),
                Some(watch_path.to_str().unwrap()),
                "autoload field should display the picked directory"
            );
        })
        .unwrap();

        // Now the move folder.
        cx.update_window(window_handle, |_, window, cx| {
            window.click("browse-move-torrents-folder", cx);
        })
        .unwrap();
        cx.simulate_path_prompt_response({
            let move_path = move_path.clone();
            move |_| Some(vec![move_path.clone()])
        });
        cx.run_until_parked();

        cx.update_window(window_handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.find("move-torrents-location").value(),
                Some(move_path.to_str().unwrap()),
                "move field should display the picked directory"
            );
        })
        .unwrap();

        // The page-local config (what Apply persists) must hold both folders.
        cx.update(|cx| {
            let config = page.read(cx).config.read().clone();
            assert_eq!(config.load_torrents_folder, watch_path);
            assert_eq!(config.move_torrents_folder, move_path);
        });
    }
}
