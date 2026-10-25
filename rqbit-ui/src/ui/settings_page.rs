//! Shared session-settings page for remote clients.
//!
//! The desktop app keeps its own full settings page (it owns the whole
//! config); this page covers the session subset a browser client can edit
//! through the daemon ([`SessionSettings`], read/written via the
//! client's `get/set_session_settings`). There is no filesystem in the
//! browser, so every folder path renders as a plain text input.
//!
//! Applying settings makes the host rebuild its session. On the standalone
//! daemon the Set response arrives normally; on the desktop's bundled web
//! view the server dies with the old session and the response is lost — in
//! that case the page waits for the auto-reconnect, verifies with a Get and
//! only then closes.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, Size,
    button::{Button, ButtonVariants},
    h_flex,
    scroll::ScrollableElement,
    setting::{NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage, Settings},
    v_flex,
};
use rqbit_protocol::SessionSettings;

use crate::client::TorrentClient;

/// How long to wait between reconnect-verification attempts, and how many.
/// The remote client reconnects on a 2s backoff, so a handful of attempts
/// covers a normal daemon restart.
const VERIFY_ATTEMPTS: usize = 30;
const VERIFY_DELAY: Duration = Duration::from_secs(1);

/// Events emitted by [`SessionSettingsPage`] to communicate with the parent
/// view.
#[derive(Clone, Debug)]
pub enum SessionSettingsPageEvent {
    /// User clicked **← Back**, or settings were applied successfully –
    /// close the page.
    Back,
}

/// A full-page editor for the daemon's session settings. All folder paths
/// are plain text inputs (remote clients have no filesystem to pick from).
pub struct SessionSettingsPage {
    client: Arc<dyn TorrentClient>,
    /// The edited draft; fields read and write it through closures.
    draft: Arc<RwLock<SessionSettings>>,
    /// Whether the initial load completed (fields render from the draft
    /// either way, so an empty draft shows while loading).
    loaded: bool,
    /// True while an Apply is in flight (including reconnect verification).
    applying: bool,
    /// Error text rendered under the toolbar.
    error: Option<SharedString>,
    focus_handle: FocusHandle,
}

impl EventEmitter<SessionSettingsPageEvent> for SessionSettingsPage {}

impl SessionSettingsPage {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        client: Arc<dyn TorrentClient>,
    ) -> Self {
        let draft = Arc::new(RwLock::new(SessionSettings::default()));

        // Load the current settings; fields re-render from the draft (the
        // setting component's inputs sync to external value changes).
        let weak = cx.entity().downgrade();
        let load_client = client.clone();
        let load_draft = draft.clone();
        window
            .spawn(cx, async move |cx| {
                let result = load_client.get_session_settings().await;
                let _ = cx.update(|_, cx| {
                    weak.update(cx, |page: &mut SessionSettingsPage, cx| {
                        match result {
                            Ok(settings) => {
                                *load_draft.write().unwrap() = settings;
                                page.loaded = true;
                            }
                            Err(e) => {
                                page.error =
                                    Some(format!("Failed to load session settings: {e}").into());
                            }
                        }
                        cx.notify();
                    })
                });
            })
            .detach();

        Self {
            client,
            draft,
            loaded: false,
            applying: false,
            error: None,
            focus_handle: cx.focus_handle(),
        }
    }

    fn on_back(&mut self, cx: &mut Context<Self>) {
        cx.emit(SessionSettingsPageEvent::Back);
    }

    /// Re-fetch the current settings from the daemon, discarding the draft.
    fn on_revert(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let draft = self.draft.clone();
        cx.spawn(async move |this, cx| {
            let result = client.get_session_settings().await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(settings) => {
                    *draft.write().unwrap() = settings;
                    this.error = None;
                    cx.notify();
                }
                Err(e) => {
                    this.error = Some(format!("Failed to reload session settings: {e}").into());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn on_apply(&mut self, cx: &mut Context<Self>) {
        if self.applying {
            return;
        }
        self.error = None;
        if self
            .draft
            .read()
            .unwrap()
            .default_download_folder
            .trim()
            .is_empty()
        {
            self.error = Some("Default download folder must not be empty".into());
            cx.notify();
            return;
        }
        self.applying = true;
        cx.notify();

        let client = self.client.clone();
        let draft = self.draft.read().unwrap().clone();
        let executor = cx.background_executor().clone();
        cx.spawn(
            async move |this, cx| match client.set_session_settings(&draft).await {
                Ok(()) => {
                    let _ = this.update(cx, |_, cx| cx.emit(SessionSettingsPageEvent::Back));
                }
                // A connection error most likely means the apply succeeded and
                // the host is rebuilding its session (the desktop's bundled web
                // view server dies with the old session, so the response is
                // lost). Verify after the auto-reconnect instead of failing.
                Err(e) if is_connection_error(&e) => {
                    let verified = verify_after_reconnect(client.as_ref(), &executor).await;
                    let _ = this.update(cx, |this, cx| {
                        this.applying = false;
                        match verified {
                            Ok(()) => cx.emit(SessionSettingsPageEvent::Back),
                            Err(message) => {
                                this.error = Some(message.into());
                                cx.notify();
                            }
                        }
                    });
                }
                Err(e) => {
                    let _ = this.update(cx, |this, cx| {
                        this.applying = false;
                        this.error = Some(format!("Failed to apply settings: {e}").into());
                        cx.notify();
                    });
                }
            },
        )
        .detach();
    }
}

/// The two connection-loss messages the remote client resolves pending
/// requests with when the socket drops.
fn is_connection_error(e: &rqbit_protocol::ProtoError) -> bool {
    e.message.contains("connection lost") || e.message.contains("connection closed")
}

/// Poll `Get` until the reconnect lands (or attempts run out) — the success
/// signal that the new session is up.
async fn verify_after_reconnect(
    client: &dyn TorrentClient,
    executor: &gpui::BackgroundExecutor,
) -> Result<(), String> {
    for _ in 0..VERIFY_ATTEMPTS {
        executor.timer(VERIFY_DELAY).await;
        if client.get_session_settings().await.is_ok() {
            return Ok(());
        }
    }
    Err(
        "Settings were applied, but the connection did not come back to \
         verify them. Check the daemon."
            .to_string(),
    )
}

// ── Field helpers ───────────────────────────────────────────────────────────
// Each helper clones the draft Arc so the get/set closures share it (the
// closures are `Rc`-held and only run on the UI thread).

type DraftArc = Arc<RwLock<SessionSettings>>;

fn switch_field(
    draft: DraftArc,
    get: fn(&SessionSettings) -> bool,
    set: fn(&mut SessionSettings, bool),
) -> SettingField<bool> {
    let dget = draft.clone();
    let dset = draft;
    SettingField::switch(
        move |_| get(&dget.read().unwrap()),
        move |val: bool, _| set(&mut dset.write().unwrap(), val),
    )
}

fn input_field(
    draft: DraftArc,
    get: fn(&SessionSettings) -> String,
    set: fn(&mut SessionSettings, String),
) -> SettingField<SharedString> {
    let dget = draft.clone();
    let dset = draft;
    SettingField::input(
        move |_| get(&dget.read().unwrap()).into(),
        move |val: SharedString, _| set(&mut dset.write().unwrap(), val.to_string()),
    )
}

fn number_field(
    draft: DraftArc,
    min: f64,
    max: f64,
    get: fn(&SessionSettings) -> f64,
    set: fn(&mut SessionSettings, f64),
) -> SettingField<f64> {
    let dget = draft.clone();
    let dset = draft;
    SettingField::number_input(
        NumberFieldOptions {
            min,
            max,
            ..Default::default()
        },
        move |_| get(&dget.read().unwrap()),
        move |val: f64, _| set(&mut dset.write().unwrap(), val),
    )
}

// ── Accessors (field closures are fn pointers) ──────────────────────────────

fn get_download_folder(s: &SessionSettings) -> String {
    s.default_download_folder.clone()
}
fn set_download_folder(s: &mut SessionSettings, v: String) {
    s.default_download_folder = v;
}

fn get_dht_enabled(s: &SessionSettings) -> bool {
    s.dht_enabled
}
fn set_dht_enabled(s: &mut SessionSettings, v: bool) {
    s.dht_enabled = v;
}
fn get_dht_persist(s: &SessionSettings) -> bool {
    s.dht_persistence_enabled
}
fn set_dht_persist(s: &mut SessionSettings, v: bool) {
    s.dht_persistence_enabled = v;
}
fn get_dht_filename(s: &SessionSettings) -> String {
    s.dht_persistence_filename.clone()
}
fn set_dht_filename(s: &mut SessionSettings, v: String) {
    s.dht_persistence_filename = v;
}

fn get_persist_enabled(s: &SessionSettings) -> bool {
    s.persistence_enabled
}
fn set_persist_enabled(s: &mut SessionSettings, v: bool) {
    s.persistence_enabled = v;
}
fn get_persist_folder(s: &SessionSettings) -> String {
    s.persistence_folder.clone()
}
fn set_persist_folder(s: &mut SessionSettings, v: String) {
    s.persistence_folder = v;
}
fn get_fastresume(s: &SessionSettings) -> bool {
    s.fastresume
}
fn set_fastresume(s: &mut SessionSettings, v: bool) {
    s.fastresume = v;
}

fn get_dl_rate(s: &SessionSettings) -> f64 {
    s.download_rate_limit_bps as f64
}
fn set_dl_rate(s: &mut SessionSettings, v: f64) {
    s.download_rate_limit_bps = v as u32;
}
fn get_ul_rate(s: &SessionSettings) -> f64 {
    s.upload_rate_limit_bps as f64
}
fn set_ul_rate(s: &mut SessionSettings, v: f64) {
    s.upload_rate_limit_bps = v as u32;
}

fn get_tcp_listen(s: &SessionSettings) -> bool {
    s.enable_tcp_listen
}
fn set_tcp_listen(s: &mut SessionSettings, v: bool) {
    s.enable_tcp_listen = v;
}
fn get_utp(s: &SessionSettings) -> bool {
    s.enable_utp
}
fn set_utp(s: &mut SessionSettings, v: bool) {
    s.enable_utp = v;
}
fn get_upnp_fwd(s: &SessionSettings) -> bool {
    s.enable_upnp_port_forward
}
fn set_upnp_fwd(s: &mut SessionSettings, v: bool) {
    s.enable_upnp_port_forward = v;
}
fn get_listen_port(s: &SessionSettings) -> f64 {
    s.listen_port as f64
}
fn set_listen_port(s: &mut SessionSettings, v: f64) {
    s.listen_port = v as u16;
}
fn get_tcp_outgoing(s: &SessionSettings) -> bool {
    s.enable_tcp_outgoing
}
fn set_tcp_outgoing(s: &mut SessionSettings, v: bool) {
    s.enable_tcp_outgoing = v;
}
fn get_socks(s: &SessionSettings) -> String {
    s.socks_proxy.clone()
}
fn set_socks(s: &mut SessionSettings, v: String) {
    s.socks_proxy = v;
}
fn get_peer_ct(s: &SessionSettings) -> f64 {
    s.peer_connect_timeout_secs as f64
}
fn set_peer_ct(s: &mut SessionSettings, v: f64) {
    s.peer_connect_timeout_secs = v as u32;
}
fn get_peer_rw(s: &SessionSettings) -> f64 {
    s.peer_read_write_timeout_secs as f64
}
fn set_peer_rw(s: &mut SessionSettings, v: f64) {
    s.peer_read_write_timeout_secs = v as u32;
}

// ── Render ──────────────────────────────────────────────────────────────────

impl Render for SessionSettingsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let draft = self.draft.clone();
        let applying = self.applying;
        let error = self.error.clone();

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
                        Button::new("web-settings-back")
                            .label("← Back")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.on_back(cx))),
                    )
                    .child(
                        Button::new("web-settings-revert")
                            .label("Revert")
                            .disabled(applying)
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.on_revert(cx))),
                    )
                    .child(
                        h_flex().flex_1().justify_end().child(
                            Button::new("web-settings-apply")
                                .primary()
                                .label(if applying { "Applying…" } else { "Apply" })
                                .disabled(applying || !self.loaded)
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| this.on_apply(cx))),
                        ),
                    ),
            )
            // Error banner (kept visible until the next Apply/Revert)
            .when_some(error, |this, error| {
                this.child(
                    h_flex()
                        .id("web-settings-error")
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
                div().flex_1().min_h_0().overflow_y_scrollbar().child(
                    Settings::new("rqbit-web-settings")
                        .with_size(Size::Medium)
                        .pages(vec![
                            // ── General ──
                            SettingPage::new("General")
                                .default_open(true)
                                .description(
                                    "Changes are applied by restarting the daemon's \
                                         session; the connection briefly drops and \
                                         reconnects.",
                                )
                                .group(SettingGroup::new().title("Download").items(vec![
                                            SettingItem::new(
                                                "Default download folder",
                                                input_field(
                                                    draft.clone(),
                                                    get_download_folder,
                                                    set_download_folder,
                                                ),
                                            )
                                            .layout(Axis::Vertical)
                                            .description(
                                                "Where downloaded data is stored (a path on \
                                                 the machine running the daemon).",
                                            ),
                                        ])),
                            // ── DHT ──
                            SettingPage::new("DHT").group(
                                SettingGroup::new().title("DHT Settings").items(vec![
                                    SettingItem::new(
                                        "Enable DHT",
                                        switch_field(
                                            draft.clone(),
                                            get_dht_enabled,
                                            set_dht_enabled,
                                        ),
                                    )
                                    .description(
                                        "Enable the Distributed Hash Table for peer \
                                             discovery.",
                                    ),
                                    SettingItem::new(
                                        "Enable DHT persistence",
                                        switch_field(
                                            draft.clone(),
                                            get_dht_persist,
                                            set_dht_persist,
                                        ),
                                    )
                                    .description("Persist DHT routing table across restarts."),
                                    SettingItem::new(
                                        "DHT persistence filename",
                                        input_field(
                                            draft.clone(),
                                            get_dht_filename,
                                            set_dht_filename,
                                        ),
                                    )
                                    .layout(Axis::Vertical),
                                ]),
                            ),
                            // ── Session ──
                            SettingPage::new("Session")
                                .group(SettingGroup::new().title("Persistence").items(vec![
                                            SettingItem::new(
                                                "Enable session persistence",
                                                switch_field(
                                                    draft.clone(),
                                                    get_persist_enabled,
                                                    set_persist_enabled,
                                                ),
                                            )
                                            .description(
                                                "Save torrent state to disk for resume on \
                                                 restart.",
                                            ),
                                            SettingItem::new(
                                                "Persistence folder",
                                                input_field(
                                                    draft.clone(),
                                                    get_persist_folder,
                                                    set_persist_folder,
                                                ),
                                            )
                                            .layout(Axis::Vertical),
                                            SettingItem::new(
                                                "Enable fast resume (experimental)",
                                                switch_field(
                                                    draft.clone(),
                                                    get_fastresume,
                                                    set_fastresume,
                                                ),
                                            ),
                                        ]))
                                .group(SettingGroup::new().title("Rate Limits").items(vec![
                                            SettingItem::new(
                                                "Download rate limit (bytes/sec, \
                                                 0 = unlimited)",
                                                number_field(
                                                    draft.clone(),
                                                    0.,
                                                    u32::MAX as f64,
                                                    get_dl_rate,
                                                    set_dl_rate,
                                                ),
                                            )
                                            .layout(Axis::Vertical),
                                            SettingItem::new(
                                                "Upload rate limit (bytes/sec, \
                                                 0 = unlimited)",
                                                number_field(
                                                    draft.clone(),
                                                    0.,
                                                    u32::MAX as f64,
                                                    get_ul_rate,
                                                    set_ul_rate,
                                                ),
                                            )
                                            .layout(Axis::Vertical),
                                        ])),
                            // ── Connection ──
                            SettingPage::new("Connection")
                                .group(SettingGroup::new().title("Listening").items(vec![
                                            SettingItem::new(
                                                "Listen on TCP",
                                                switch_field(
                                                    draft.clone(),
                                                    get_tcp_listen,
                                                    set_tcp_listen,
                                                ),
                                            ),
                                            SettingItem::new(
                                                "Listen on uTP (over UDP)",
                                                switch_field(draft.clone(), get_utp, set_utp),
                                            ),
                                            SettingItem::new(
                                                "Advertise port over UPnP",
                                                switch_field(
                                                    draft.clone(),
                                                    get_upnp_fwd,
                                                    set_upnp_fwd,
                                                ),
                                            ),
                                            SettingItem::new(
                                                "Listen port",
                                                number_field(
                                                    draft.clone(),
                                                    0.,
                                                    u16::MAX as f64,
                                                    get_listen_port,
                                                    set_listen_port,
                                                ),
                                            )
                                            .layout(Axis::Vertical),
                                        ]))
                                .group(SettingGroup::new().title("Outgoing").items(vec![
                                            SettingItem::new(
                                                "Enable outgoing TCP",
                                                switch_field(
                                                    draft.clone(),
                                                    get_tcp_outgoing,
                                                    set_tcp_outgoing,
                                                ),
                                            ),
                                            SettingItem::new(
                                                "SOCKS proxy",
                                                input_field(draft.clone(), get_socks, set_socks),
                                            )
                                            .layout(Axis::Vertical),
                                            SettingItem::new(
                                                "Peer connect timeout (seconds)",
                                                number_field(
                                                    draft.clone(),
                                                    0.,
                                                    u32::MAX as f64,
                                                    get_peer_ct,
                                                    set_peer_ct,
                                                ),
                                            )
                                            .layout(Axis::Vertical),
                                            SettingItem::new(
                                                "Peer read/write timeout (seconds)",
                                                number_field(
                                                    draft.clone(),
                                                    0.,
                                                    u32::MAX as f64,
                                                    get_peer_rw,
                                                    set_peer_rw,
                                                ),
                                            )
                                            .layout(Axis::Vertical),
                                        ])),
                        ]),
                ),
            )
    }
}

impl Focusable for SessionSettingsPage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
