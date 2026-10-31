use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, Size, Theme, ThemeMode,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    label::Label,
    scroll::ScrollableElement,
    setting::{
        NumberFieldOptions, RenderOptions, SettingField, SettingGroup, SettingItem, SettingPage,
        Settings,
    },
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

        let mut config = self.config.read().clone();

        // Theme is deliberately not owned by the draft: the switches apply and
        // persist it immediately (see `theme_switch_field`), so take the live
        // value here — Apply can neither clobber a concurrent toggle nor be
        // reverted by one.
        config.theme = self.state.config().theme;

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

/// A bounded `f64` number input, the one constructor for every numeric
/// setting value. gpui-component stretches number inputs to the full item
/// width when the item is laid out vertically; pin the compact width
/// horizontal rows use instead.
fn number_input_field(
    min: f64,
    max: f64,
    value: impl Fn(&App) -> f64 + 'static,
    set_value: impl Fn(f64, &mut App) + 'static,
) -> SettingField<f64> {
    SettingField::number_input(
        NumberFieldOptions {
            min,
            max,
            ..Default::default()
        },
        value,
        set_value,
    )
    .w_32()
}

fn number_field(
    config: ConfigArc,
    min: f64,
    max: f64,
    get: fn(&RqbitDesktopConfig) -> f64,
    set: fn(&mut RqbitDesktopConfig, f64),
) -> SettingField<f64> {
    let cfg_get = config.clone();
    let cfg_set = config;
    number_input_field(
        min,
        max,
        move |_| get(&cfg_get.read()),
        move |val: f64, _| set(&mut cfg_set.write(), val),
    )
}

/// A live theme switch: applies and persists the choice immediately, exactly
/// like the titlebar toggle — the theme is not part of the page's draft
/// config, so Apply can neither clobber a concurrent toggle nor be reverted
/// by one (see [`State::set_theme`]).
fn theme_switch_field(state: Arc<State>) -> SettingField<bool> {
    let state_get = state.clone();
    let state_set = state;
    SettingField::switch(
        move |_| state_get.config().theme == "dark",
        move |dark: bool, cx: &mut App| {
            let mode = if dark {
                ThemeMode::Dark
            } else {
                ThemeMode::Light
            };
            Theme::change(mode, None, cx);
            state_set.set_theme(mode.name());
        },
    )
}

/// State backing a folder-location field: the text input plus the
/// subscription that writes typed changes back into the config.
struct FolderInputState {
    input: Entity<InputState>,
    _subscription: gpui::Subscription,
}

/// A folder-location field that pairs a text input with a **Browse…** button.
///
/// The button opens the native folder picker (macOS Finder / open panel).
/// On macOS this also triggers the OS "grant access to this folder" prompt the
/// first time, and the granted access persists for the app — so the user is
/// only asked once.
fn folder_field(
    config: ConfigArc,
    key: &'static str,
    input_id: &'static str,
    button_id: &'static str,
    get: fn(&RqbitDesktopConfig) -> String,
    set: fn(&mut RqbitDesktopConfig, SharedString),
    prompt: &'static str,
) -> impl Fn(&RenderOptions, &mut Window, &mut App) -> AnyElement {
    move |options, window, cx| {
        let value = get(&config.read());
        let cfg = config.clone();

        let state_entity = window.use_keyed_state(key, cx, {
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
                FolderInputState {
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
                    .id(input_id)
                    .flex_1()
                    .disabled(options.is_disabled())
                    .with_size(options.size()),
            )
            .child(
                Button::new(button_id)
                    .label("Browse…")
                    .disabled(options.is_disabled())
                    .on_click({
                        let cfg = config.clone();
                        move |_, _window, cx| {
                            let rx = cx.prompt_for_paths(PathPromptOptions {
                                files: false,
                                directories: true,
                                multiple: false,
                                prompt: Some(prompt.into()),
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

fn get_dl_rate(c: &RqbitDesktopConfig) -> f64 {
    c.ratelimits
        .download_bps
        .map(|v| v.get() as f64)
        .unwrap_or(0.)
}
fn set_dl_rate(c: &mut RqbitDesktopConfig, v: f64) {
    // 0 = unlimited; the input widget clamps to u32::MAX before we get here.
    c.ratelimits.download_bps = NonZeroU32::new(v as u32);
}

fn get_ul_rate(c: &RqbitDesktopConfig) -> f64 {
    c.ratelimits
        .upload_bps
        .map(|v| v.get() as f64)
        .unwrap_or(0.)
}
fn set_ul_rate(c: &mut RqbitDesktopConfig, v: f64) {
    c.ratelimits.upload_bps = NonZeroU32::new(v as u32);
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

fn get_listen_port(c: &RqbitDesktopConfig) -> f64 {
    c.connections.listen_port as f64
}
fn set_listen_port(c: &mut RqbitDesktopConfig, v: f64) {
    c.connections.listen_port = v as u16;
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

fn get_peer_ct(c: &RqbitDesktopConfig) -> f64 {
    c.connections.peer_connect_timeout.as_secs() as f64
}
fn set_peer_ct(c: &mut RqbitDesktopConfig, v: f64) {
    c.connections.peer_connect_timeout = std::time::Duration::from_secs(v as u64);
}

fn get_peer_rw(c: &RqbitDesktopConfig) -> f64 {
    c.connections.peer_read_write_timeout.as_secs() as f64
}
fn set_peer_rw(c: &mut RqbitDesktopConfig, v: f64) {
    c.connections.peer_read_write_timeout = std::time::Duration::from_secs(v as u64);
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

// Artwork switches: the getters invert the `disable_*` config flags so the
// switches read as "on = shown", like every other enable-style switch here.
fn get_posters(c: &RqbitDesktopConfig) -> bool {
    !c.disable_posters
}
fn set_posters(c: &mut RqbitDesktopConfig, v: bool) {
    c.disable_posters = !v;
}

fn get_myshows(c: &RqbitDesktopConfig) -> bool {
    !c.disable_myshows
}
fn set_myshows(c: &mut RqbitDesktopConfig, v: bool) {
    c.disable_myshows = !v;
}

fn get_previews(c: &RqbitDesktopConfig) -> bool {
    !c.disable_previews
}
fn set_previews(c: &mut RqbitDesktopConfig, v: bool) {
    c.disable_previews = !v;
}

fn get_poster_tint(c: &RqbitDesktopConfig) -> bool {
    c.poster_tint
}
fn set_poster_tint(c: &mut RqbitDesktopConfig, v: bool) {
    c.poster_tint = v;
}

fn get_web_view_ip(c: &RqbitDesktopConfig) -> String {
    c.web_view.listen_addr.ip().to_string()
}
fn set_web_view_ip(c: &mut RqbitDesktopConfig, v: SharedString) {
    if let Ok(ip) = v.parse() {
        c.web_view.listen_addr.set_ip(ip);
    }
}

fn get_web_view_port(c: &RqbitDesktopConfig) -> f64 {
    c.web_view.listen_addr.port() as f64
}
fn set_web_view_port(c: &mut RqbitDesktopConfig, v: f64) {
    c.web_view.listen_addr.set_port(v as u16);
}

/// Get the current move-torrents-folder configuration value.
fn get_move_torrents_location(c: &RqbitDesktopConfig) -> String {
    c.move_torrents_folder.to_string_lossy().to_string()
}

/// Set the move-torrents-folder configuration value.
fn set_move_torrents_location(c: &mut RqbitDesktopConfig, v: SharedString) {
    c.move_torrents_folder = v.to_string().into();
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

fn hook_number_field(
    config: ConfigArc,
    ix: usize,
    min: f64,
    max: f64,
    get: fn(&HookConfig) -> f64,
    set: fn(&mut HookConfig, f64),
) -> SettingField<f64> {
    let cfg_get = config.clone();
    let cfg_set = config;
    number_input_field(
        min,
        max,
        move |_| with_hook(&cfg_get, ix, get, min),
        move |val: f64, _| mutate_hook(&cfg_set, ix, |h| set(h, val)),
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

/// The **Add Hook** button pinned in the Hooks page header. It appends a new
/// hook with a default configuration. Living in the header — outside the
/// scrolling list of hook groups — it stays visible no matter how many hooks
/// are configured, and no hook group needs to carry a duplicated manage row.
fn add_hook_button(config: ConfigArc) -> impl Fn(&mut Window, &mut App) -> Button {
    move |_window, _cx| {
        let cfg = config.clone();
        Button::new("hooks-add")
            .label("Add Hook")
            .small()
            .on_click(move |_, _, cx| {
                add_hook(&cfg);
                cx.refresh_windows();
            })
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
fn get_hook_timeout(h: &HookConfig) -> f64 {
    h.timeout_secs as f64
}
fn set_hook_timeout(h: &mut HookConfig, v: f64) {
    h.timeout_secs = v as u64;
}
fn get_hook_poll(h: &HookConfig) -> f64 {
    h.poll_secs as f64
}
fn set_hook_poll(h: &mut HookConfig, v: f64) {
    h.poll_secs = v as u64;
}

/// Build the groups of the Hooks settings page: exactly one group per
/// configured hook. The **Add Hook** action is not a group — it is pinned in
/// the page header (see [`add_hook_button`]), so nothing here duplicates it.
fn hook_groups(config: ConfigArc) -> Vec<SettingGroup> {
    let count = config.read().hooks.hooks.len();
    if count == 0 {
        return vec![SettingGroup::new().items(vec![SettingItem::render(
            move |_options, _window, cx| {
                div()
                    .py_2()
                    .child(
                        Label::new("No hooks configured. Click Add Hook to create one.")
                            .text_color(cx.theme().muted_foreground),
                    )
                    .into_any_element()
            },
        )])];
    }
    (0..count)
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
                    hook_number_field(
                        config.clone(),
                        ix,
                        1.,
                        86400.,
                        get_hook_timeout,
                        set_hook_timeout,
                    ),
                ),
                SettingItem::new(
                    "Poll (seconds)",
                    hook_number_field(config.clone(), ix, 2., 86400., get_hook_poll, set_hook_poll),
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
        .collect()
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
                                            SettingField::render(folder_field(
                                                config.clone(),
                                                "download-location-input",
                                                "download-location",
                                                "browse-download-folder",
                                                get_dl_location,
                                                set_dl_location,
                                                "Select download folder",
                                            )),
                                        )
                                        .layout(Axis::Vertical),
                                        SettingItem::new(
                                            "Autoload torrents from folder",
                                            SettingField::render(folder_field(
                                                config.clone(),
                                                "load-torrents-location-input",
                                                "load-torrents-location",
                                                "browse-load-torrents-folder",
                                                get_load_torrents_location,
                                                set_load_torrents_location,
                                                "Select load torrent folder",
                                            )),
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
                                            SettingField::render(folder_field(
                                                config.clone(),
                                                "move-torrents-location-input",
                                                "move-torrents-location",
                                                "browse-move-torrents-folder",
                                                get_move_torrents_location,
                                                set_move_torrents_location,
                                                "Select move torrent folder",
                                            )),
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
                                                theme_switch_field(self.state.clone()),
                                            )
                                            .description(
                                                "Use a dark color scheme for the application UI. \
                                                 Applies and persists immediately.",
                                            ),
                                        ]),
                                    )
                                    .group(
                                        SettingGroup::new().title("Artwork").items(vec![
                                            SettingItem::new(
                                                "Torrent posters",
                                                switch_field(config.clone(), get_posters, set_posters),
                                            )
                                            .description(
                                                "Show poster artwork in the torrent list and the \
                                                 detail panel: hook-provided posters (e.g. a \
                                                 moviesync-daemon resolve table) and the MyShows \
                                                 lookup below. Applies on session restart.",
                                            ),
                                            SettingItem::new(
                                                "MyShows poster lookup",
                                                switch_field(config.clone(), get_myshows, set_myshows),
                                            )
                                            .description(
                                                "Look up posters on myshows.me (exact-title match) \
                                                 for releases without a hook-provided poster.",
                                            ),
                                            SettingItem::new(
                                                "Media previews",
                                                switch_field(config.clone(), get_previews, set_previews),
                                            )
                                            .description(
                                                "Use a frame from the largest downloaded video \
                                                 file (via ffmpeg) when no poster was found.",
                                            ),
                                            SettingItem::new(
                                                "Tint artwork with the theme color",
                                                switch_field(config.clone(), get_poster_tint, set_poster_tint),
                                            )
                                            .description(
                                                "Recolor posters to match the active theme (a \
                                                 duotone from the background to the primary \
                                                 color) instead of their own colors.",
                                            ),
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
                                                number_field(
                                                    config.clone(),
                                                    0.,
                                                    u32::MAX as f64,
                                                    get_dl_rate,
                                                    set_dl_rate,
                                                ),
                                            ),
                                            SettingItem::new(
                                                "Upload rate limit (bytes/sec, 0 = unlimited)",
                                                number_field(
                                                    config.clone(),
                                                    0.,
                                                    u32::MAX as f64,
                                                    get_ul_rate,
                                                    set_ul_rate,
                                                ),
                                            ),
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
                                                number_field(
                                                    config.clone(),
                                                    0.,
                                                    u16::MAX as f64,
                                                    get_listen_port,
                                                    set_listen_port,
                                                ),
                                            ),
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
                                                number_field(
                                                    config.clone(),
                                                    0.,
                                                    u32::MAX as f64,
                                                    get_peer_ct,
                                                    set_peer_ct,
                                                ),
                                            ),
                                            SettingItem::new(
                                                "Peer read/write timeout (seconds)",
                                                number_field(
                                                    config.clone(),
                                                    0.,
                                                    u32::MAX as f64,
                                                    get_peer_rw,
                                                    set_peer_rw,
                                                ),
                                            ),
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
                                            number_field(
                                                config.clone(),
                                                0.,
                                                u16::MAX as f64,
                                                get_web_view_port,
                                                set_web_view_port,
                                            ),
                                        )
                                        .description(
                                            "TCP port of the browser UI. The IP address to bind \
                                             is configured below.",
                                        ),
                                        SettingItem::new(
                                            "Web view IP address",
                                            input_field(config.clone(), get_web_view_ip, set_web_view_ip),
                                        )
                                        .layout(Axis::Vertical)
                                        .description(
                                            "IP address to bind; the port is configured above. \
                                             Defaults to 127.0.0.1 with port 3081 — a different \
                                             port than app-daemon's 3080, so both can run at \
                                             the same time.",
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
                                    // The add button lives in the page header so it
                                    // stays fixed on top while the hook groups scroll.
                                    .title_suffix(add_hook_button(config.clone()))
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
    /// address, and the IP field must only rewrite the IP — neither
    /// duplicates the other.
    #[test]
    fn web_view_ip_and_port_edit_only_their_own_part() {
        use super::{get_web_view_ip, get_web_view_port, set_web_view_ip, set_web_view_port};
        use crate::config::RqbitDesktopConfig;

        let mut cfg = RqbitDesktopConfig::default();
        assert_eq!(get_web_view_port(&cfg), 3081.);
        assert_eq!(get_web_view_ip(&cfg), "127.0.0.1");

        set_web_view_port(&mut cfg, 4000.);
        assert_eq!(get_web_view_ip(&cfg), "127.0.0.1");
        assert_eq!(cfg.web_view.listen_addr.to_string(), "127.0.0.1:4000");

        // Out-of-range values are clamped by the input widget; the setter
        // saturates the cast to u16.
        set_web_view_port(&mut cfg, 70_000.);
        assert_eq!(cfg.web_view.listen_addr.to_string(), "127.0.0.1:65535");

        // Changing the IP keeps the port.
        set_web_view_ip(&mut cfg, "0.0.0.0".into());
        assert_eq!(get_web_view_port(&cfg), 65535.);
        assert_eq!(cfg.web_view.listen_addr.to_string(), "0.0.0.0:65535");

        // Unparseable input is ignored.
        set_web_view_ip(&mut cfg, "not an ip".into());
        assert_eq!(get_web_view_ip(&cfg), "0.0.0.0");
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
