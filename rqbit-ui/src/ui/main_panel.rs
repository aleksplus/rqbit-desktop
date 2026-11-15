use gpui_kit::component::dock::{
    BasePanel, DockArea, DockLayout, DockPlacement, DockSkin, DockToggleEvent, NodeId, PaneRef,
    Panel as DockPanel, PanelControl, PanelEvent, PanelId, PanelStyle, panel_handle,
};
use gpui_kit::component::separator::Separator;
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::{ActiveTheme, Selectable as _, Sizable, WindowExt as _};
use gpui_kit::component::{
    Icon, IconName, Size, StyledExt, TitleBar,
    button::{Button, ButtonVariant, ButtonVariants},
    dialog::{Cancel, Confirm, DialogButtonProps, DialogFooter},
    h_flex,
    input::{Input, InputEvent, InputState},
    resizable::{ResizableState, resizable_panel, v_resizable},
    searchable_list::SearchableVec,
    select::{Select, SelectEvent, SelectState},
    table::{DataTable, TableEvent, TableState},
    v_flex,
};
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use rqbit_protocol::{
    CustomColumnsState, DhtStats, ProtoErrorKind, PushUpdate, SessionStats, TorrentAction,
};

use futures::StreamExt as _;

use crate::client::{AddTorrentRequest, ClientResult, ConnectionState, TorrentClient, UiAddSource};
use crate::host::UiHost;
use crate::ui::add_torrent_dialog::{AddTorrentDialog, AddTorrentDialogEvent, AddTorrentEntry};
#[cfg(feature = "agent-testing")]
use crate::ui::agent::{
    AutomationNode, AutomationRole, AutomationSnapshot, parse_detail_tab_target,
    parse_torrent_detail_target, parse_torrent_file_target, parse_torrent_row_target,
    parse_torrent_status_target,
};
use crate::ui::agent::{AutomationRegistry, SharedAutomationRegistry};

use crate::ui::dialogs::{AboutDialog, AboutInfo, DeleteDialog, MagnetDialog};
use crate::ui::explorer::{ExplorerEvent, ExplorerPanel};
use crate::ui::log_page::{LogPage, LogPageEvent};
use crate::ui::torrent_detail_panel::{TorrentDetailPanel, TorrentDetailPanelEvent};
use crate::ui::torrent_table::{TorrentRow, TorrentTableDelegate};
use crate::ui::utils::{build_entry, format_bytes, format_speed, format_uptime};

/// Future returned by a per-torrent action dispatched by
/// [`MainPanel::run_action_for_ids`].
type TorrentActionFuture = Pin<Box<dyn Future<Output = ClientResult<()>> + 'static>>;

/// DHT health for the footer indicator, derived from `TorrentClient::dht_stats`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DhtFooterStatus {
    /// DHT is running and the routing table has nodes.
    Connected { nodes: usize },
    /// DHT is enabled but the routing table is empty (still bootstrapping,
    /// or the bootstrap nodes are unreachable).
    Connecting,
    /// The stats request failed with something other than "disabled".
    Error,
    /// DHT is turned off on the daemon/session side.
    Disabled,
}

/// Map the result of `TorrentClient::dht_stats` to a footer status.
fn dht_footer_status(res: &ClientResult<DhtStats>) -> DhtFooterStatus {
    match res {
        Ok(stats) => {
            let nodes = (stats.routing_table_size + stats.routing_table_size_v6) as usize;
            if nodes > 0 {
                DhtFooterStatus::Connected { nodes }
            } else {
                DhtFooterStatus::Connecting
            }
        }
        Err(e) if e.kind == ProtoErrorKind::DhtDisabled => DhtFooterStatus::Disabled,
        Err(_) => DhtFooterStatus::Error,
    }
}

/// Raw Lucide `plug` SVG (ISC license), embedded so both the desktop and web
/// builds can tint it without registering an asset source.
const PLUG_SVG: &[u8] = include_bytes!("../../assets/plug.svg");

/// The title bar's action-button cluster (add / settings / logs / theme).
///
/// `Button` lets mouse events bubble up, and `TitleBar` zooms the window on a
/// double-click anywhere inside it, so a double-click on one of these buttons
/// would maximize the window. Swallowing the left mouse-down here stops the
/// title bar from ever registering a click from the cluster. It must be the
/// mouse-down rather than the click: GPUI only fires an element's click
/// handlers when it also saw its mouse-down, and the mouse-up still needs to
/// reach the title bar to reset its window-drag tracking.
fn title_bar_actions() -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// Run configured host file-selection adjustments against an entry's resolved
/// file list and record which ones changed the selection (shown as a hint in
/// the add dialog). A no-op when the host has none.
async fn apply_file_selection_host(host: &Arc<dyn UiHost>, entry: &mut AddTorrentEntry) {
    let applied = host.adjust_file_selection(entry).await;
    if !applied.is_empty() {
        entry.hook_note = Some(applied.join(", "));
    }
}

/// A dialog footer button that dispatches the hosting dialog's `Confirm`
/// action — the same action Enter activates — so the `button_props` `on_ok`
/// handler stays the single decision point for what confirming does and
/// whether the dialog closes.
fn dialog_confirm_button(
    id: &'static str,
    label: impl Into<SharedString>,
    variant: ButtonVariant,
) -> Button {
    Button::new(id)
        .label(label)
        .with_variant(variant)
        .on_click(|_, window, cx| {
            window.dispatch_action(Box::new(Confirm { secondary: false }), cx);
        })
}

/// A dialog footer button that dispatches the hosting dialog's `Cancel`
/// action (the same action Escape activates).
fn dialog_cancel_button(id: &'static str) -> Button {
    Button::new(id)
        .label("Cancel")
        .on_click(|_, window, cx| window.dispatch_action(Box::new(Cancel), cx))
}

/// The dock's center panel: the torrent table + detail split, exactly the
/// pre-dock main layout. Lives inside a [`DockArea`] so the Explorer can be
/// a left dock next to it.
struct TorrentsPanel {
    table_state: Entity<TableState<TorrentTableDelegate>>,
    resizable_state: Entity<ResizableState>,
    /// Mirrored from `MainPanel::detail_panel` (created/closed there).
    detail_panel: Option<Entity<TorrentDetailPanel>>,
    focus_handle: FocusHandle,
}

impl TorrentsPanel {
    fn new(
        table_state: Entity<TableState<TorrentTableDelegate>>,
        resizable_state: Entity<ResizableState>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            table_state,
            resizable_state,
            detail_panel: None,
            focus_handle: cx.focus_handle(),
        }
    }

    fn set_detail_panel(&mut self, detail_panel: Option<Entity<TorrentDetailPanel>>) {
        self.detail_panel = detail_panel;
    }
}

impl BasePanel for TorrentsPanel {
    fn panel_name(&self) -> &'static str {
        "torrents"
    }

    /// The center content can't be dismissed.
    fn closable(&self, _cx: &App) -> bool {
        false
    }
}

impl DockPanel for TorrentsPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Torrents"
    }

    /// The table runs edge to edge, as it did before the dock.
    fn inner_padding(&self, _cx: &App) -> bool {
        false
    }

    /// The panel starts zoomed, so the zoom-out button stays visible on the
    /// zoomed tab bar — not buried in the ⋯ menu.
    fn zoom_control(&self, _cx: &App) -> Option<PanelControl> {
        Some(PanelControl::Both)
    }
}

impl EventEmitter<PanelEvent> for TorrentsPanel {}

impl Focusable for TorrentsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TorrentsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_resizable("main-split")
            .with_state(&self.resizable_state)
            .child(
                resizable_panel().child(
                    div()
                        .size_full()
                        .overflow_hidden()
                        .border_1()
                        .border_color(cx.theme().border)
                        .child(DataTable::new(&self.table_state).bordered(false)),
                ),
            )
            .child(
                resizable_panel()
                    .visible(self.detail_panel.is_some())
                    .size(px(400.))
                    .size_range(px(250.)..px(800.))
                    .child(
                        self.detail_panel
                            .clone()
                            .map(|p| p.into_any_element())
                            .unwrap_or_else(|| div().into_any_element()),
                    ),
            )
    }
}

/// The center tab group containing `panel_id`, for programmatic zoom.
fn center_group_node(area: &DockArea, panel_id: PanelId) -> Option<NodeId> {
    let tree = area.layout(DockPlacement::Center)?;
    let mut found = None;
    tree.root().walk(&mut |node| {
        if matches!(
            node.kind(),
            PaneRef::Tabs { panels, .. } if panels.contains(&panel_id)
        ) {
            found = Some(node.id());
        }
    });
    found
}

/// Which full page currently replaces the main content, for agent snapshots
/// (`page-settings` / `page-logs`; `None` is `page-main`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivePageKind {
    Settings,
    Logs,
}

/// Modal dialog identity, for agent dispatch validation and error messages.
#[cfg(feature = "agent-testing")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModalKind {
    Magnet,
    Delete,
    Add,
    About,
}

#[cfg(feature = "agent-testing")]
impl ModalKind {
    fn name(self) -> &'static str {
        match self {
            ModalKind::Magnet => "magnet",
            ModalKind::Delete => "delete",
            ModalKind::Add => "add",
            ModalKind::About => "about",
        }
    }
}

/// The open modal dialog, with the body entity the agent snapshot reads
/// state from and dispatch acts on. Cleared through the hosting dialog's
/// `on_close`, so any close path (Confirm, Cancel, Escape, close button)
/// resets it. The entities are only read with `agent-testing` enabled.
#[cfg_attr(not(feature = "agent-testing"), allow(dead_code))]
enum ActiveModal {
    Magnet(Entity<MagnetDialog>),
    Delete(Entity<DeleteDialog>),
    Add(Entity<AddTorrentDialog>),
    About(Entity<AboutDialog>),
}

#[cfg(feature = "agent-testing")]
impl ActiveModal {
    fn kind(&self) -> ModalKind {
        match self {
            ActiveModal::Magnet(_) => ModalKind::Magnet,
            ActiveModal::Delete(_) => ModalKind::Delete,
            ActiveModal::Add(_) => ModalKind::Add,
            ActiveModal::About(_) => ModalKind::About,
        }
    }
}

/// Main panel that displays the list of torrents.
pub struct MainPanel {
    client: Arc<dyn TorrentClient>,
    host: Arc<dyn UiHost>,
    table_state: Entity<TableState<TorrentTableDelegate>>,
    /// The dockable main layout: Explorer tree as the left dock, the
    /// torrent table + detail split as the center panel.
    ///
    /// The torrents panel starts zoomed, filling the whole area; the
    /// status-bar toggle reveals/hides the Explorer dock on top of that
    /// (see [`MainPanel::toggle_explorer`]).
    dock_area: Entity<DockArea>,
    explorer: Entity<ExplorerPanel>,
    torrents_panel: Entity<TorrentsPanel>,
    /// Host-provided full page (settings) or the shared log page, when open.
    active_page: Option<AnyView>,
    /// Which kind of full page `active_page` holds (for agent snapshots).
    active_page_kind: Option<ActivePageKind>,
    /// The currently open modal dialog (for agent snapshots and dispatch).
    active_modal: Option<ActiveModal>,
    detail_panel: Option<Entity<TorrentDetailPanel>>,
    /// Torrent ID waiting for detail panel creation (deferred until window is available in render).
    pending_detail_id: Option<usize>,
    /// Torrent entries waiting for add-dialog creation (deferred until window is
    /// available in render, mirroring `pending_detail_id`).
    pending_add_entries: Option<Vec<AddTorrentEntry>>,
    /// Torrent IDs + resolved names waiting for delete-dialog creation
    /// (deferred until window is available in render).
    pending_delete_dialog: Option<(Vec<usize>, Vec<String>)>,
    /// About info waiting for About-dialog creation (deferred until window is
    /// available in render). Queued by the native menu's About action, which
    /// arrives without a `&mut Window`.
    pending_about: Option<AboutInfo>,
    /// Whether the settings page should open on the next render (queued the
    /// same way as `pending_about`, via the native menu's Settings action).
    pending_settings: bool,
    /// Latest session-wide stats for the footer (download/upload speed, uptime).
    footer_stats: Option<SessionStats>,
    /// Latest DHT health for the footer (None until the first poll completes).
    dht_status: Option<DhtFooterStatus>,
    /// Hook-provided custom columns + connectivity. `None` = no column hook
    /// is configured; `connected: false` = configured but unreachable (the
    /// tables then hide the dynamic columns and the footer shows a gray
    /// plug). Fed by the poll loop or `PushUpdate::CustomColumns`.
    custom_columns: Option<CustomColumnsState>,
    /// Search box for filtering torrents by name.
    search_input: Entity<InputState>,
    /// Status filter dropdown (empty string = "All").
    status_filter: Entity<SelectState<SearchableVec<String>>>,
    /// Keeps the periodic stats polling task alive for the lifetime of the
    /// panel (only started for clients that don't push updates).
    _stats_task: Option<Task<()>>,
    /// Keeps the push-update consumer alive for pushing clients.
    _updates_task: Option<Task<()>>,
    /// Keeps the periodic connection-state watcher alive (remote clients); the
    /// in-process default is an empty stream, so the task ends immediately.
    _connection_task: Option<Task<()>>,
    /// Transport liveness, from `TorrentClient::connection_state`. Local
    /// in-process clients never report otherwise. When not `Connected`, the
    /// panel renders the disconnect placeholder on top of the (stale) UI.
    connection: ConnectionState,
    /// Whether the window is currently active (focused). Only used to refresh
    /// immediately on re-activation; the periodic polling no longer depends
    /// on it.
    window_active: bool,
    focus_handle: FocusHandle,
    automation: SharedAutomationRegistry,
}

impl MainPanel {
    pub fn new(
        client: Arc<dyn TorrentClient>,
        host: Arc<dyn UiHost>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_inner(client, host, window, cx, AutomationRegistry::disabled())
    }

    #[cfg(feature = "agent-testing")]
    pub fn new_with_agent(
        client: Arc<dyn TorrentClient>,
        host: Arc<dyn UiHost>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_inner(client, host, window, cx, AutomationRegistry::enabled())
    }

    fn new_inner(
        client: Arc<dyn TorrentClient>,
        host: Arc<dyn UiHost>,
        window: &mut Window,
        cx: &mut Context<Self>,
        automation: SharedAutomationRegistry,
    ) -> Self {
        let weak = cx.entity().downgrade();

        // The dockable main layout: Explorer tree docked left, the torrent
        // table + detail split as the center panel.
        let (dock_area, dock_skin) = DockSkin::dock_area("main-dock", None, window, cx);
        dock_skin.set_panel_style(PanelStyle::TabBar, cx);
        let explorer = cx.new(|cx| ExplorerPanel::new(client.clone(), host.clone(), cx));

        let table_state = cx.new(|cx| {
            TableState::new(
                TorrentTableDelegate::new(weak, client.clone(), host.clone(), automation.clone()),
                window,
                cx,
            )
            .row_selectable(true)
        });
        let resizable_state = cx.new(|_cx| ResizableState::default());
        let torrents_panel =
            cx.new(|cx| TorrentsPanel::new(table_state.clone(), resizable_state.clone(), cx));
        let center = DockLayout::tabs().panel_view(panel_handle(torrents_panel.clone()), cx);
        let left = DockLayout::tabs().panel_view(panel_handle(explorer.clone()), cx);
        let torrents_panel_id = PanelId::from(torrents_panel.entity_id());
        dock_area.update(cx, |area, cx| {
            area.set_center(center, window, cx);
            area.set_dock(DockPlacement::Left, left, window, cx);
            area.set_dock_size(DockPlacement::Left, px(260.), window, cx);
            // The torrents panel starts zoomed: it fills the whole dock
            // area, keeping the Explorer dock (created open, underneath)
            // hidden until the zoom is cleared.
            if let Some(node) = center_group_node(area, torrents_panel_id) {
                area.set_zoomed_in(node, window, cx);
            }
        });

        let mut this = Self {
            client: client.clone(),
            host,
            table_state,
            dock_area,
            explorer,
            torrents_panel,
            active_page: None,
            active_page_kind: None,
            active_modal: None,
            detail_panel: None,
            pending_detail_id: None,
            pending_add_entries: None,
            pending_delete_dialog: None,
            pending_about: None,
            pending_settings: false,
            footer_stats: None,
            dht_status: None,
            custom_columns: None,
            search_input: cx
                .new(|_cx| InputState::new(window, _cx).placeholder("Search torrents…")),
            status_filter: cx.new(|_cx| {
                SelectState::new(
                    SearchableVec::new(vec![
                        "initializing".to_string(),
                        "live".to_string(),
                        "paused".to_string(),
                        "error".to_string(),
                    ]),
                    None,
                    window,
                    _cx,
                )
            }),
            _stats_task: None,
            _updates_task: None,
            _connection_task: None,
            connection: ConnectionState::Connected,
            window_active: true,
            focus_handle: cx.focus_handle(),
            automation,
        };

        // Open/update detail panel when a row is selected in the table, and
        // persist column widths so user resizes survive the delegate's
        // column rebuilds (a `refresh()` re-reads the delegate's columns).
        let table_state = this.table_state.clone();
        cx.subscribe(
            &table_state,
            |this, table, event: &TableEvent, cx| match event {
                TableEvent::SelectRow(row_ix) => {
                    let torrent_id = table.read(cx).delegate().rows.get(*row_ix).map(|r| r.id);
                    if let Some(id) = torrent_id {
                        this.open_details(id as usize, cx);
                    }
                }
                TableEvent::ColumnWidthsChanged(widths) => {
                    table.update(cx, |state, _cx| {
                        state.delegate_mut().set_column_widths(widths);
                    });
                }
                _ => {}
            },
        )
        .detach();

        // Refresh immediately when the window is re-activated, so the table
        // and detail panel don't wait for the next polling cycle after the
        // app was in the background.
        cx.observe_window_activation(window, |this, window, cx| {
            let was_active = this.window_active;
            this.window_active = window.is_window_active();

            // When window becomes active after being inactive, refresh immediately
            // instead of waiting for the next polling cycle.
            if this.window_active && !was_active {
                this.fetch_torrents(cx);
                if let Some(panel) = &this.detail_panel {
                    let selected_ids = this.selected_torrent_ids(cx);
                    if let Some(torrent_id) = selected_ids.first().copied() {
                        panel.update(cx, |panel, cx| {
                            panel.switch_torrent(torrent_id, cx);
                        });
                    }
                }
            }

            cx.notify();
        })
        .detach();

        // Re-apply the search/filter whenever the search box text changes.
        let search_input = this.search_input.clone();
        cx.subscribe(&search_input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.apply_search_and_filter(cx);
            }
        })
        .detach();

        // Re-apply the search/filter whenever the status dropdown changes.
        let status_filter = this.status_filter.clone();
        cx.subscribe(
            &status_filter,
            |this, _, _event: &SelectEvent<SearchableVec<String>>, cx| {
                this.apply_search_and_filter(cx);
            },
        )
        .detach();

        // Re-render right away when the user toggles the dock via its own
        // chrome (collapse button), so the status-bar Explorer toggle's
        // state follows. Zoom changes surface through the regular refresh
        // cadence.
        let dock_area = this.dock_area.clone();
        cx.subscribe(&dock_area, |_, _, _event: &DockToggleEvent, cx| {
            cx.notify();
        })
        .detach();

        // Selecting an entry in the Explorer opens (or retargets) the
        // torrent detail panel.
        let explorer = this.explorer.clone();
        cx.subscribe(&explorer, |this, _, event: &ExplorerEvent, cx| {
            let ExplorerEvent::TorrentSelected(id) = event;
            this.open_details(*id as usize, cx);
        })
        .detach();

        this.fetch_torrents(cx);
        this.start_state_updates(cx);
        this
    }

    /// Keep session-wide state (footer stats, torrent list) flowing. Clients
    /// that push updates get a push consumer; the rest (local in-process)
    /// get the classic 1-second polling loop.
    fn start_state_updates(&mut self, cx: &mut Context<Self>) {
        self.start_connection_watch(cx);
        if self.client.is_push() {
            self.start_push_updates(cx);
        } else {
            self.start_stats_polling(cx);
        }
    }

    /// Track transport liveness for the disconnect placeholder. The default
    /// (in-process clients) is an empty stream, so the watcher ends right
    /// away and the panel stays `Connected` forever.
    fn start_connection_watch(&mut self, cx: &mut Context<Self>) {
        let mut states = self.client.connection_state();
        let task = cx.spawn(async move |this, cx| {
            while let Some(state) = states.next().await {
                let _ = this.update(cx, |this, cx| {
                    if this.connection != state {
                        this.connection = state;
                        cx.notify();
                    }
                });
            }
        });
        self._connection_task = Some(task);
    }

    /// Periodically refresh session-wide stats (footer) and the torrent table.
    ///
    /// The table refresh is deliberately NOT tied to window focus: on macOS,
    /// windows of background apps stay visible, and keying the refresh off
    /// window activation left rows stale for up to 30s (e.g. a torrent
    /// finishing initialization while the user was in another app). The
    /// in-process list+stats call is cheap, so we always poll once per second.
    fn start_stats_polling(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let task = cx.spawn(async move |this, cx| {
            loop {
                let stats = client.session_stats().await;
                let dht_status = dht_footer_status(&client.dht_stats().await);
                let custom_columns = client.custom_columns().await;
                let _ = this.update(cx, |this, cx| {
                    this.footer_stats = stats.ok();
                    this.dht_status = Some(dht_status);
                    this.apply_custom_columns(custom_columns, cx);
                    cx.notify();
                });

                // Refresh the torrent table every second regardless of window
                // focus — see the comment on this method for why.
                let _ = this.update(cx, |this, cx| this.fetch_torrents(cx));
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
        });
        self._stats_task = Some(task);
    }

    /// Push new hook-column state into the torrent table: the dynamic
    /// columns are dropped while disconnected (or unconfigured) and only
    /// trigger a table `refresh()` when the visible set actually changed.
    fn apply_custom_columns(&mut self, state: Option<CustomColumnsState>, cx: &mut Context<Self>) {
        let connected = state.as_ref().is_some_and(|state| state.connected);
        self.explorer.update(cx, |explorer, cx| {
            explorer.set_hook_connected(connected, cx);
        });
        if self.custom_columns == state {
            return;
        }
        self.custom_columns = state.clone();
        self.table_state.update(cx, |table, cx| {
            if table.delegate_mut().set_custom_columns(state.as_ref()) {
                table.refresh(cx);
            }
            cx.notify();
        });
        cx.notify();
    }

    /// Consume pushed updates (torrent list + footer stats) from a pushing
    /// client, and fetch the DHT status once for the footer.
    fn start_push_updates(&mut self, cx: &mut Context<Self>) {
        let mut updates = self.client.updates();
        let client = self.client.clone();
        let task = cx.spawn(async move |this, cx| {
            // One-shot DHT status: pushes don't carry it (it rarely changes
            // mid-session); a "disabled" DHT is reported as an error here.
            let dht_status = dht_footer_status(&client.dht_stats().await);
            let _ = this.update(cx, |this, cx| {
                this.dht_status = Some(dht_status);
                cx.notify();
            });

            while let Some(push) = updates.next().await {
                match push {
                    PushUpdate::TorrentList(list) => {
                        let rows: Vec<TorrentRow> =
                            list.torrents.iter().map(TorrentRow::from_details).collect();
                        let _ = this.update(cx, |this, cx| {
                            this.install_rows(rows, cx);
                            this.update_explorer(&list.torrents, cx);
                        });
                    }
                    PushUpdate::SessionStats(stats) => {
                        let _ = this.update(cx, |this, cx| {
                            this.footer_stats = Some(*stats);
                            cx.notify();
                        });
                    }
                    PushUpdate::CustomColumns(state) => {
                        let _ = this.update(cx, |this, cx| {
                            this.apply_custom_columns(Some(*state), cx);
                        });
                    }
                    // Torrent-level pushes are not consumed here: the detail
                    // panel refreshes itself over requests. The daemon only
                    // sends them to per-torrent subscribers, which this
                    // session-level subscription is not.
                    _ => {}
                }
            }
        });
        self._updates_task = Some(task);
    }

    /// Push the current search text and status filter into the table delegate,
    /// which re-derives the visible rows. Safe to call on every keystroke /
    /// dropdown change; the full `all_rows` set is preserved so live status
    /// updates are not lost.
    fn apply_search_and_filter(&mut self, cx: &mut Context<Self>) {
        let search = self.search_input.read(cx).value().to_string();
        let status = self
            .status_filter
            .read(cx)
            .selected_value()
            .map(|s| s.to_string())
            .unwrap_or_default();
        self.table_state.update(cx, |state, cx| {
            state.delegate_mut().set_filter(search, status);
            cx.notify();
        });
        cx.notify();
    }

    fn fetch_torrents(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let list = client.list(true).await;
            // Session default download folder — keeps default-folder torrents
            // out of destination-folder grouping. Best-effort: on error keep
            // whatever the delegate already knows.
            let default_folder = client.session_default_folder().await.ok().flatten();

            let _ = this.update(cx, |this, cx| {
                this.table_state.update(cx, |state, _cx| {
                    state
                        .delegate_mut()
                        .set_default_folder(default_folder.as_deref());
                });
                // A failed fetch (e.g. the connection dropped) keeps the last
                // good rows: the disconnect placeholder explains the staleness
                // instead of the table silently emptying out.
                if let Ok(list) = &list {
                    let rows: Vec<TorrentRow> =
                        list.torrents.iter().map(TorrentRow::from_details).collect();
                    this.install_rows(rows, cx);
                    this.update_explorer(&list.torrents, cx);
                }
            });
        })
        .detach();
    }

    /// Install a freshly fetched torrent list into the table, preserving the
    /// selection by torrent ID and re-applying filter + sort.
    fn install_rows(&mut self, rows: Vec<TorrentRow>, cx: &mut Context<Self>) {
        // Close the detail panel when the torrent it displays no longer
        // exists in the refreshed list — it was deleted or removed
        // externally. Checked against the full list, not the filtered view,
        // so a torrent merely hidden by the filter keeps its panel open.
        if let Some(panel) = &self.detail_panel {
            let shown = panel.read(cx).torrent_id();
            if rows.iter().all(|r| r.id as usize != shown) {
                self.close_details(cx);
            }
        }
        self.table_state.update(cx, |state, cx| {
            // Preserve selected torrent IDs across refresh by matching after update.
            let prev_selected_ids: HashSet<u64> = state
                .delegate()
                .selected_rows
                .iter()
                .filter_map(|&ix| state.delegate().rows.get(ix).map(|r| r.id))
                .collect();
            state.delegate_mut().all_rows = rows;
            // Derive the visible rows from the full set using the filter
            // stored in the delegate, then re-apply the active sort.
            state.delegate_mut().apply_filter();
            // Re-select rows whose torrent IDs match the previous selection.
            state.delegate_mut().selected_rows = state
                .delegate()
                .rows
                .iter()
                .enumerate()
                .filter_map(|(ix, r)| {
                    if prev_selected_ids.contains(&r.id) {
                        Some(ix)
                    } else {
                        None
                    }
                })
                .collect();
            cx.notify();
        });
        cx.notify();
    }

    /// Get all selected torrent IDs (supports multi-row selection via shift-click).
    fn selected_torrent_ids(&self, cx: &mut Context<Self>) -> Vec<usize> {
        let table = self.table_state.read(cx);
        let delegate = table.delegate();
        let selected = &delegate.selected_rows;
        if selected.is_empty() {
            // Fall back to the table's single selected_row
            return table
                .selected_row()
                .and_then(|r| delegate.rows.get(r).map(|row| row.id as usize))
                .into_iter()
                .collect();
        }
        selected
            .iter()
            .filter_map(|&ix| delegate.rows.get(ix).map(|row| row.id as usize))
            .collect()
    }

    fn on_pause(&mut self, cx: &mut Context<Self>) {
        self.run_action_on_selected(cx, TorrentAction::Pause);
    }

    fn on_start(&mut self, cx: &mut Context<Self>) {
        self.run_action_on_selected(cx, TorrentAction::Start);
    }

    /// Apply a pause/start action to all currently selected torrents, then
    /// clear the selection and refresh the table.
    fn run_action_on_selected(&mut self, cx: &mut Context<Self>, action: TorrentAction) {
        let ids = self.selected_torrent_ids(cx);
        if ids.is_empty() {
            return;
        }
        let client = self.client.clone();
        self.run_action_for_ids(
            ids,
            move |id| {
                let client = client.clone();
                let action = action.clone();
                Box::pin(async move { client.action(&id.into(), action).await })
            },
            cx,
        );
    }

    /// Run `action` for each torrent ID sequentially, then clear the table
    /// selection and refresh. Shared by the pause/start and delete/forget
    /// flows. `action` clones the client handle per call and returns a boxed
    /// future so each variant can dispatch its own API method. Futures are
    /// `?Send`: gpui runs them on the per-window (foreground) executor.
    fn run_action_for_ids<F>(&mut self, ids: Vec<usize>, action: F, cx: &mut Context<Self>)
    where
        F: Fn(usize) -> TorrentActionFuture + 'static,
    {
        let table_state = self.table_state.clone();
        cx.spawn(async move |_, cx| {
            for id in ids {
                if let Err(e) = action(id).await {
                    eprintln!("Torrent action failed: {e}");
                }
            }
            table_state.update(cx, |state, cx| {
                state.delegate_mut().selected_rows.clear();
                cx.notify();
            });
        })
        .detach();
        self.fetch_torrents(cx);
    }

    fn on_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self.selected_torrent_ids(cx);
        if ids.is_empty() {
            return;
        }
        self.open_delete_dialog(ids, window, cx);
    }

    /// Open the delete confirmation dialog for the given torrent IDs.
    ///
    /// Torrent names are resolved asynchronously (this may be a network
    /// round-trip on remote clients); the dialog itself is created from
    /// `pending_delete_dialog` in `render`, where a `Window` is available.
    /// Public so the torrent table's context menu can open it.
    pub fn open_delete_dialog(
        &mut self,
        ids: Vec<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ids.is_empty() {
            return;
        }
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let mut names = Vec::with_capacity(ids.len());
            for id in &ids {
                let name = client
                    .details(&(*id as u64).into())
                    .await
                    .ok()
                    .and_then(|d| d.name)
                    .unwrap_or_else(|| format!("Torrent #{id}"));
                names.push(name);
            }
            let _ = this.update(cx, |this, cx| {
                this.pending_delete_dialog = Some((ids, names));
                cx.notify();
            });
        })
        .detach();
    }

    /// Queue the About dialog (triggered by the native menu). The dialog is
    /// created on the next render, where a `Window` is available.
    pub fn open_about(&mut self, info: AboutInfo, cx: &mut Context<Self>) {
        self.pending_about = Some(info);
        cx.notify();
    }

    /// Queue the settings page (triggered by the native menu). The page is
    /// opened on the next render, where a `Window` is available.
    pub fn open_settings_page(&mut self, cx: &mut Context<Self>) {
        self.pending_settings = true;
        cx.notify();
    }

    /// Close the currently open full page (settings/logs), if any. Called by
    /// the host when its own page emits "Back".
    pub fn close_page(&mut self, cx: &mut Context<Self>) {
        self.active_page = None;
        self.active_page_kind = None;
        cx.notify();
    }

    /// Actually perform the delete after the dialog is confirmed.
    ///
    /// When `delete_files` is true the downloaded data is removed from disk
    /// (delete action); otherwise only the torrent entry is removed and the
    /// files are kept (forget action). Clients may fire host-side side
    /// effects (e.g. the desktop's `torrent_deleted` webhook) internally.
    fn confirm_delete(&mut self, ids: Vec<usize>, delete_files: bool, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let action = if delete_files {
            TorrentAction::Delete
        } else {
            TorrentAction::Forget
        };
        self.run_action_for_ids(
            ids,
            move |id| {
                let client = client.clone();
                let action = action.clone();
                Box::pin(async move { client.action(&id.into(), action).await })
            },
            cx,
        );
    }

    fn on_refresh(&mut self, cx: &mut Context<Self>) {
        self.fetch_torrents(cx);
    }

    /// "Add Torrent…": pick .torrent files through the host, resolve their
    /// file lists, and queue the add dialog. Only shown when the host
    /// supports file picking.
    fn on_add_torrent_files(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let host = self.host.clone();
        let client = self.client.clone();
        let picked = host.pick_torrent_files(cx);
        cx.spawn(async move |this, cx| match picked.await {
            Ok(files) => {
                let mut entries: Vec<AddTorrentEntry> = Vec::new();
                for file in files {
                    match build_entry(client.as_ref(), UiAddSource::Bytes(file.bytes.clone())).await
                    {
                        Ok(mut entry) => {
                            apply_file_selection_host(&host, &mut entry).await;
                            // Remember where this torrent came from, so its
                            // source file can be moved to the configured
                            // folder after adding.
                            entry.source_path = file.path;
                            entries.push(entry);
                        }
                        Err(e) => eprintln!("Error reading torrent {:?}: {e}", file.name),
                    }
                }
                if !entries.is_empty() {
                    let _ = this.update(cx, |this, cx| {
                        this.open_add_dialog(entries, cx);
                    });
                }
            }
            Err(e) => eprintln!("File picker error: {e}"),
        })
        .detach();
    }

    fn on_add_magnet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dialog =
            cx.new(|cx| MagnetDialog::new(window, cx).with_automation(self.automation.clone()));
        self.active_modal = Some(ActiveModal::Magnet(dialog.clone()));
        let panel = cx.entity();
        window.open_dialog(cx, {
            let dialog = dialog.clone();
            move |d, _, _| {
                d.title("Add Magnet Link")
                    .button_props(DialogButtonProps::default().on_ok({
                        let dialog = dialog.clone();
                        let panel = panel.clone();
                        move |_, _, cx| match dialog.read(cx).magnet(cx) {
                            Some(magnet) => {
                                panel.update(cx, |panel, cx| panel.add_magnet(magnet, cx));
                                true
                            }
                            // Empty input: keep the dialog open.
                            None => false,
                        }
                    }))
                    .on_close({
                        let panel = panel.clone();
                        move |_, _, cx| {
                            panel.update(cx, |this, _| this.active_modal = None);
                        }
                    })
                    .footer(
                        DialogFooter::new()
                            .child(dialog_cancel_button("magnet-cancel"))
                            .child(dialog_confirm_button(
                                "magnet-add",
                                "Add",
                                ButtonVariant::Primary,
                            )),
                    )
                    .child(dialog.clone())
            }
        });
    }

    fn add_magnet(&mut self, magnet: String, cx: &mut Context<Self>) {
        let host = self.host.clone();
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            match build_entry(client.as_ref(), UiAddSource::Magnet(magnet)).await {
                Ok(mut entry) => {
                    apply_file_selection_host(&host, &mut entry).await;
                    let _ = this.update(cx, |this, cx| {
                        this.open_add_dialog(vec![entry], cx);
                    });
                }
                Err(e) => eprintln!("Error preparing magnet: {e}"),
            }
        })
        .detach();
    }

    /// Defer creation of the file-selection dialog until `render`, where a
    /// `Window` is available (mirrors `pending_detail_id`).
    fn open_add_dialog(&mut self, entries: Vec<AddTorrentEntry>, cx: &mut Context<Self>) {
        self.pending_add_entries = Some(entries);
        cx.notify();
    }

    /// Actually create the add dialog from pending entries (called from render).
    fn create_add_dialog(
        &mut self,
        entries: Vec<AddTorrentEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog = cx.new(|cx| {
            AddTorrentDialog::new(entries, window, cx).with_automation(self.automation.clone())
        });
        self.active_modal = Some(ActiveModal::Add(dialog.clone()));
        let panel = cx.entity();
        cx.subscribe(
            &dialog,
            |this, _entity, event: &AddTorrentDialogEvent, cx| match event {
                AddTorrentDialogEvent::AddOne(source, selection, overwrite, source_path) => {
                    this.confirm_add(
                        source.clone(),
                        source_path.clone(),
                        selection.clone(),
                        *overwrite,
                        cx,
                    );
                }
            },
        )
        .detach();
        window.open_dialog(cx, {
            let dialog = dialog.clone();
            move |d, _, cx| {
                let title = dialog.read(cx).title();
                let ok_label = dialog.read(cx).ok_label();
                d.title(title)
                    .w(px(600.))
                    .button_props(
                        DialogButtonProps::default()
                            // Returns whether the queue is drained: the
                            // dialog stays open while more torrents await.
                            // `refresh` re-runs the dialog builder so the
                            // chrome title and button label follow the
                            // advance (only the body entity is notified by
                            // the update itself).
                            .on_ok({
                                let dialog = dialog.clone();
                                move |_, window, cx| {
                                    let drained = dialog.update(cx, |d, cx| d.add_current(cx));
                                    window.refresh();
                                    drained
                                }
                            }),
                    )
                    .on_close({
                        let panel = panel.clone();
                        move |_, _, cx| {
                            panel.update(cx, |this, _| this.active_modal = None);
                        }
                    })
                    .footer(
                        DialogFooter::new()
                            .child(dialog_cancel_button("add-cancel"))
                            .child(dialog_confirm_button(
                                "add-ok",
                                ok_label,
                                ButtonVariant::Primary,
                            )),
                    )
                    .child(dialog.clone())
            }
        });
    }

    /// Actually add a torrent with the selected file indices (if any), then
    /// refresh the table. Clients may fire host-side side effects (the
    /// desktop's `torrent_added` webhook and source-file move) internally.
    fn confirm_add(
        &mut self,
        source: UiAddSource,
        source_path: Option<String>,
        selection: Option<Vec<u64>>,
        overwrite: bool,
        cx: &mut Context<Self>,
    ) {
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let req = AddTorrentRequest {
                source,
                only_files: selection,
                overwrite,
                paused: false,
                list_only: false,
                source_hint: source_path,
            };
            if let Err(e) = client.add_torrent(&req).await {
                eprintln!("Error adding torrent: {e}");
            }
            let _ = this.update(cx, |this, cx| this.fetch_torrents(cx));
        })
        .detach();
    }

    /// Open the detail panel for a torrent, or retarget the existing one.
    /// Shared by the table's row selection and the Explorer's tree selection.
    fn open_details(&mut self, torrent_id: usize, cx: &mut Context<Self>) {
        if let Some(panel) = &self.detail_panel {
            // Update existing panel to show the newly selected torrent.
            panel.update(cx, |panel, cx| {
                panel.switch_torrent(torrent_id, cx);
            });
        } else {
            // Mark that we need to create a panel; defer to render where
            // window is available.
            self.pending_detail_id = Some(torrent_id);
            cx.notify();
        }
    }

    /// Whether the Explorer dock is on screen. The torrents panel starts
    /// zoomed — filling the whole area — which keeps the dock hidden until
    /// the zoom is cleared, so visibility is zoom + dock-open, not either
    /// alone.
    fn explorer_visible(&self, cx: &App) -> bool {
        let area = self.dock_area.read(cx);
        !area.is_zoomed() && area.is_dock_open(DockPlacement::Left)
    }

    /// The status-bar Explorer toggle (Zed-style, far left). Showing the
    /// explorer clears the torrents panel's zoom and opens the dock; hiding
    /// it closes the dock and restores the default maximized torrents view.
    fn toggle_explorer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let show = !self.explorer_visible(cx);
        let torrents_panel_id = PanelId::from(self.torrents_panel.entity_id());
        self.dock_area.update(cx, |area, cx| {
            if show {
                if area.is_zoomed() {
                    area.set_zoomed_out(window, cx);
                }
                if !area.is_dock_open(DockPlacement::Left) {
                    area.toggle_dock(DockPlacement::Left, window, cx);
                }
            } else {
                if area.is_dock_open(DockPlacement::Left) {
                    area.toggle_dock(DockPlacement::Left, window, cx);
                }
                if let Some(node) = center_group_node(area, torrents_panel_id) {
                    area.set_zoomed_in(node, window, cx);
                }
            }
        });
        cx.notify();
    }

    /// Feed the latest torrent list to the Explorer tree (per-file progress
    /// rides on the list rows' stats).
    fn update_explorer(
        &mut self,
        torrents: &[rqbit_protocol::TorrentDetails],
        cx: &mut Context<Self>,
    ) {
        self.explorer
            .update(cx, |explorer, cx| explorer.set_torrents(torrents, cx));
    }

    fn create_detail_panel(
        &mut self,
        torrent_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = cx.new(|cx| {
            TorrentDetailPanel::new(
                torrent_id,
                window,
                cx,
                self.client.clone(),
                self.host.clone(),
            )
            .with_automation(self.automation.clone())
        });
        cx.subscribe(
            &panel,
            |this, _entity, event: &TorrentDetailPanelEvent, cx| match event {
                TorrentDetailPanelEvent::Back => this.close_details(cx),
            },
        )
        .detach();
        self.detail_panel = Some(panel);
        self.sync_torrents_panel_detail(cx);
        cx.notify();
    }

    fn close_details(&mut self, cx: &mut Context<Self>) {
        // Deselect the table row so the detail panel stays hidden.
        self.table_state.update(cx, |state, cx| {
            state.delegate_mut().selected_rows.clear();
            state.clear_selection(cx);
            cx.notify();
        });
        self.detail_panel = None;
        self.sync_torrents_panel_detail(cx);
        cx.notify();
    }

    /// Mirror the detail panel into the dock's center panel, which renders
    /// the table + details split.
    fn sync_torrents_panel_detail(&mut self, cx: &mut Context<Self>) {
        let detail = self.detail_panel.clone();
        self.torrents_panel.update(cx, |panel, cx| {
            panel.set_detail_panel(detail);
            cx.notify();
        });
    }

    fn on_settings(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = self.host.open_settings(self.client.clone(), _window, cx) {
            self.active_page = Some(page);
            self.active_page_kind = Some(ActivePageKind::Settings);
            cx.notify();
        }
    }

    fn on_logs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let page = cx.new(|cx| LogPage::new(window, cx, self.client.clone()));
        cx.subscribe(
            &page,
            |this, _entity, event: &LogPageEvent, cx| match event {
                LogPageEvent::Back => this.close_page(cx),
            },
        )
        .detach();
        self.active_page = Some(page.into());
        self.active_page_kind = Some(ActivePageKind::Logs);
        cx.notify();
    }

    /// Toggle between light and dark theme, persist the choice through the
    /// host, and apply it to the active window.
    fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let is_dark = cx.theme().is_dark();
        let new_mode = if is_dark {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };

        // Apply the new theme to the window immediately.
        Theme::change(new_mode, Some(window), cx);

        // Persist the choice through the host (desktop: config file).
        self.host.persist_theme(new_mode.name());

        cx.notify();
    }
}

impl Render for MainPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Create pending detail panel now that window is available.
        let pending_id = self.pending_detail_id.take();
        if let Some(id) = pending_id {
            self.create_detail_panel(id, _window, cx);
        }

        // Create pending add dialog now that window is available.
        let pending_add = self.pending_add_entries.take();
        if let Some(entries) = pending_add {
            self.create_add_dialog(entries, _window, cx);
        }

        // Create pending delete dialog now that window is available.
        let pending_delete = self.pending_delete_dialog.take();
        if let Some((ids, names)) = pending_delete {
            self.create_delete_dialog(ids, names, _window, cx);
        }

        // Create pending About dialog now that window is available.
        if let Some(info) = self.pending_about.take() {
            self.create_about_dialog(info, _window, cx);
        }

        // Open the pending settings page now that window is available.
        if self.pending_settings {
            self.pending_settings = false;
            self.on_settings(_window, cx);
        }

        let automation = self.automation.clone();
        AutomationRegistry::track_window(
            &automation,
            v_flex()
                .size_full()
                .gap_0()
                .child(self.render_title_bar(cx))
                .when(self.active_page.is_none(), |this| {
                    this.child(self.render_toolbar(cx))
                })
                .child(self.render_content(cx))
                .child(self.render_footer(cx))
                // The disconnect placeholder paints over the whole panel: the
                // stale UI stays visible underneath, but nothing can be driven
                // while the transport is down.
                .when(self.connection != ConnectionState::Connected, |this| {
                    this.child(self.render_connection_placeholder(cx))
                }),
        )
    }
}

impl MainPanel {
    /// Render the application title bar: app name/version plus the global
    /// action buttons (add torrent/magnet, settings, theme toggle). Draggable;
    /// handles macOS traffic-light padding. Buttons for host-provided
    /// affordances only render when the host advertises them.
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let caps = self.host.capabilities();
        let automation = self.automation.clone();
        TitleBar::new()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_base().font_semibold().child("Rqbit Desktop"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(self.host.version()),
                    ),
            )
            .child(
                title_bar_actions()
                    .when(caps.add_torrent_files, |this| {
                        this.child(
                            Button::new("add-torrent")
                                .label("Add Torrent…")
                                .small()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.on_add_torrent_files(window, cx)
                                })),
                        )
                    })
                    .child(AutomationRegistry::track(
                        &automation,
                        "add-magnet",
                        Button::new("add-magnet")
                            .label("Add Magnet…")
                            .small()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.on_add_magnet(window, cx)),
                            ),
                    ))
                    .when(caps.settings_page, |this| {
                        this.child(
                            Button::new("settings")
                                .icon(IconName::Settings)
                                .small()
                                .ghost()
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.on_settings(window, cx)),
                                ),
                        )
                    })
                    .when(caps.logs_page, |this| {
                        this.child(
                            Button::new("logs")
                                .icon(IconName::SquareTerminal)
                                .small()
                                .ghost()
                                .tooltip("Log viewer")
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.on_logs(window, cx)),
                                ),
                        )
                    })
                    .child(AutomationRegistry::track(
                        &automation,
                        "theme",
                        Button::new("theme")
                            .icon(if cx.theme().is_dark() {
                                IconName::Sun
                            } else {
                                IconName::Moon
                            })
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_theme(window, cx);
                            })),
                    )),
            )
            .pr(px(10.))
    }

    /// Render the toolbar: selection actions on the left, search box and
    /// status filter on the right. Extra left padding for macOS traffic lights.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let automation = self.automation.clone();
        h_flex()
            .gap_2()
            .bg(theme.background)
            .border_b_1()
            .border_color(theme.border)
            .pl(px(10.))
            .pt(px(5.))
            .pr(px(10.))
            .pb(px(5.))
            .child(
                h_flex()
                    .gap_2()
                    .child(AutomationRegistry::track(
                        &automation,
                        "start",
                        Button::new("start")
                            .label("Start")
                            .icon(IconName::Play)
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.on_start(cx))),
                    ))
                    .child(AutomationRegistry::track(
                        &automation,
                        "pause",
                        Button::new("pause")
                            .label("Pause")
                            .icon(IconName::Pause)
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.on_pause(cx))),
                    ))
                    .child(AutomationRegistry::track(
                        &automation,
                        "delete",
                        Button::new("delete")
                            .label("Delete")
                            .icon(IconName::CircleX)
                            .small()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.on_delete(window, cx)),
                            ),
                    ))
                    .child(
                        div().pl_4().child(AutomationRegistry::track(
                            &automation,
                            "refresh",
                            Button::new("refresh")
                                .tooltip("Refresh")
                                .icon(IconName::LoaderCircle)
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| this.on_refresh(cx))),
                        )),
                    ),
            )
            .child(h_flex().flex_grow_1().size_full())
            .child(
                h_flex()
                    .gap_2()
                    .child(AutomationRegistry::track_wrapped(
                        &automation,
                        "status-filter",
                        Select::new(&self.status_filter)
                            .cleanable(true)
                            .with_size(Size::Small)
                            .placeholder("All statuses")
                            .w(px(160.)),
                    ))
                    .child(AutomationRegistry::track_wrapped(
                        &automation,
                        "torrent-search",
                        Input::new(&self.search_input)
                            .with_size(Size::Small)
                            .cleanable(true)
                            .min_w(px(200.))
                            .w(px(200.)),
                    )),
            )
    }

    /// Render the main content area: the host settings page or log viewer
    /// page when open, otherwise the dockable layout (Explorer dock left,
    /// torrent table + detail split centered).
    fn render_content(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        div().flex_1().min_h_0().child(match &self.active_page {
            // Pages replace the main content.
            Some(page) => div().size_full().child(page.clone()).into_any_element(),
            None => div()
                .size_full()
                .child(self.dock_area.clone())
                .into_any_element(),
        })
    }

    /// Full-window placeholder shown while the transport is down (remote
    /// clients): the last synced state stays visible underneath a translucent
    /// backdrop, so the UI reads as "waiting for the app" instead of silently
    /// frozen. Clicks are swallowed — nothing underneath can work anyway.
    fn render_connection_placeholder(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (title, detail) = match self.connection {
            ConnectionState::Connecting => (
                "Connecting to the app…",
                "Waiting for the rqbit app to accept the connection.",
            ),
            ConnectionState::Disconnected => (
                "App is disconnected",
                "The rqbit app seems to be closed. This page reconnects \
                 automatically once it's running again.",
            ),
            // The overlay is only rendered when the transport is not up.
            ConnectionState::Connected => return div().into_any_element(),
        };
        div()
            .absolute()
            .inset_0()
            .bg(theme.background.opacity(0.85))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
            .child(
                v_flex()
                    .max_w(px(420.))
                    .items_center()
                    .gap_3()
                    .child(
                        Icon::default()
                            .data(PLUG_SVG)
                            .size(px(32.))
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .max_w(px(380.))
                            .text_size(px(13.))
                            .text_color(theme.muted_foreground)
                            .child(detail),
                    ),
            )
            .into_any_element()
    }

    /// Render the footer showing session-wide download/upload speed, DHT
    /// health, and uptime.
    ///
    /// Uses the gpui-kit [`StatusBar`] with three left-pinned groups
    /// (download, upload, DHT) and a right-pinned uptime, separated by
    /// vertical dividers.
    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let automation = self.automation.clone();
        let (down_speed, up_speed, fetched, uploaded, uptime) = match &self.footer_stats {
            Some(stats) => (
                stats.download_speed.mbps,
                stats.upload_speed.mbps,
                stats.fetched_bytes,
                stats.uploaded_bytes,
                stats.uptime_seconds,
            ),
            None => (0.0, 0.0, 0, 0, 0),
        };
        let (down_speed, up_speed, fetched, uploaded, uptime) = (
            format_speed(down_speed),
            format_speed(up_speed),
            format_bytes(fetched),
            format_bytes(uploaded),
            format_uptime(uptime),
        );

        let (dht_color, dht_text) = match self.dht_status {
            Some(DhtFooterStatus::Connected { nodes }) => {
                (cx.theme().success, format!("{nodes} nodes"))
            }
            Some(DhtFooterStatus::Connecting) => (cx.theme().warning, "connecting…".to_string()),
            Some(DhtFooterStatus::Error) => (cx.theme().danger, "error".to_string()),
            Some(DhtFooterStatus::Disabled) => (cx.theme().muted_foreground, "off".to_string()),
            None => (cx.theme().muted_foreground, "—".to_string()),
        };

        // Hook service connectivity: a green plug + service name while the
        // column-providing hook responds, a gray one + "offline" when it is
        // configured but unreachable. Rendered not at all when no hook
        // provides columns.
        let plug_group = self.custom_columns.as_ref().map(|columns| {
            let (color, status_text) = if columns.connected {
                (cx.theme().success, "connected".to_string())
            } else {
                (cx.theme().muted_foreground, "offline".to_string())
            };
            let label = columns
                .source
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("Service")
                .to_string();
            h_flex()
                .items_center()
                .gap_1()
                .child(Icon::default().data(PLUG_SVG).text_color(color))
                .child(div().font_medium().child(label))
                .child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child(status_text),
                )
        });

        StatusBar::new()
            // ← Explorer dock toggle, far left like Zed's status bar. Lit
            // while the tree is on screen (the zoomed torrents panel covers
            // the dock, so visibility is zoom + dock-open).
            .left(AutomationRegistry::track(
                &automation,
                "explorer-toggle",
                Button::new("explorer-toggle")
                    .tooltip("Toggle Explorer")
                    .icon(IconName::PanelLeft)
                    .small()
                    .ghost()
                    .selected(self.explorer_visible(cx))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_explorer(window, cx);
                    })),
            ))
            .left(Separator::vertical())
            // ↓ Download speed (and total fetched)
            .left(
                h_flex()
                    .items_center()
                    .gap_1()
                    .child(div().font_medium().child("↓"))
                    .child(div().child(down_speed))
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("({fetched})")),
                    ),
            )
            .left(Separator::vertical())
            // ↑ Upload speed (and total uploaded)
            .left(
                h_flex()
                    .items_center()
                    .gap_1()
                    .child(div().font_medium().child("↑"))
                    .child(div().child(up_speed))
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("({uploaded})")),
                    ),
            )
            .left(Separator::vertical())
            // DHT health: green = nodes in the routing table, yellow =
            // enabled but no nodes yet, red = error, gray = disabled.
            .left(
                h_flex()
                    .items_center()
                    .gap_1()
                    .child(Icon::new(IconName::Network).text_color(dht_color))
                    .child(div().font_medium().child("DHT"))
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child(dht_text),
                    ),
            )
            .left(Separator::vertical())
            // Hook service plug (hidden when no column hook is configured).
            .when_some(plug_group, |this, group| this.left(group))
            // Uptime
            .right(
                h_flex()
                    .items_center()
                    .gap_1()
                    .child(div().font_medium().child("up"))
                    .child(div().child(uptime)),
            )
    }
}

/// Create the delete dialog for pending (ids, names) — called from render,
/// where a `Window` is available.
impl MainPanel {
    fn create_delete_dialog(
        &mut self,
        ids: Vec<usize>,
        names: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog = cx.new(|cx| {
            DeleteDialog::new(ids, names, window, cx).with_automation(self.automation.clone())
        });
        self.active_modal = Some(ActiveModal::Delete(dialog.clone()));
        let panel = cx.entity();
        window.open_alert_dialog(cx, {
            let dialog = dialog.clone();
            move |d, _, cx| {
                let title = dialog.read(cx).title();
                let ok_label = dialog.read(cx).ok_label();
                let question = dialog.read(cx).question();
                d.title(title)
                    .description(question)
                    .confirm()
                    .ok_text(ok_label)
                    .ok_variant(ButtonVariant::Danger)
                    .width(px(450.))
                    .button_props(DialogButtonProps::default().on_ok({
                        let dialog = dialog.clone();
                        let panel = panel.clone();
                        move |_, _, cx| {
                            let (ids, delete_files) = dialog.read(cx).confirmation();
                            panel.update(cx, |panel, cx| {
                                panel.confirm_delete(ids, delete_files, cx)
                            });
                            true
                        }
                    }))
                    .on_close({
                        let panel = panel.clone();
                        move |_, _, cx| {
                            panel.update(cx, |this, _| this.active_modal = None);
                        }
                    })
                    .child(dialog.clone())
            }
        });
    }

    /// Create the About dialog for pending info — called from render, where a
    /// `Window` is available.
    fn create_about_dialog(
        &mut self,
        info: AboutInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog = cx
            .new(|cx| AboutDialog::new(info, window, cx).with_automation(self.automation.clone()));
        self.active_modal = Some(ActiveModal::About(dialog.clone()));
        let panel = cx.entity();
        window.open_dialog(cx, {
            let dialog = dialog.clone();
            move |d, _, cx| {
                let app_name = dialog.read(cx).info().app_name.clone();
                d.title(app_name)
                    .on_close({
                        let panel = panel.clone();
                        move |_, _, cx| {
                            panel.update(cx, |this, _| this.active_modal = None);
                        }
                    })
                    .footer(DialogFooter::new().child(dialog_confirm_button(
                        "about-close",
                        "Close",
                        ButtonVariant::Secondary,
                    )))
                    .child(dialog.clone())
            }
        });
    }
}

impl Focusable for MainPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Agent-testing surface (feature `agent-testing`): a neutral semantic
/// snapshot of the window plus dispatch methods that drive it through the
/// same code paths as human input. Unknown, unavailable, or ambiguous
/// targets return typed error strings — they never silently succeed.
#[cfg(feature = "agent-testing")]
impl MainPanel {
    /// Semantic snapshot of the window. `fixture` is reported back verbatim
    /// (the bridge knows which backend the window runs against). `ready`
    /// flips true once the first frame has been painted (the registry's
    /// window-root prepaint marks it), so callers can wait for a settled UI.
    pub fn agent_snapshot(&self, fixture: bool, cx: &App) -> AutomationSnapshot {
        // While a modal is open the page behind it is a background: present
        // but not drivable, so it is marked invisible.
        let modal_open = self.active_modal.is_some();
        let page = match self.active_page_kind {
            None => self.agent_main_page_node(!modal_open, cx),
            Some(ActivePageKind::Settings) => {
                AutomationNode::new(AutomationRole::Page, "page-settings", "Settings")
                    .visible(!modal_open)
                    .child(AutomationNode::new(AutomationRole::Button, "back", "Back"))
            }
            Some(ActivePageKind::Logs) => {
                AutomationNode::new(AutomationRole::Page, "page-logs", "Logs")
                    .visible(!modal_open)
                    .child(AutomationNode::new(AutomationRole::Button, "back", "Back"))
            }
        };
        let page = page.visible_deep(!modal_open);
        let mut window =
            AutomationNode::new(AutomationRole::Window, "rqbit-window", "rqbit").child(page);
        if let Some(modal) = &self.active_modal {
            window = window.child(self.agent_modal_node(modal, cx));
        }
        let ready = self.automation.lock().unwrap().first_paint();
        AutomationSnapshot {
            app: if fixture {
                "rqbit-desktop-fixture".to_string()
            } else {
                "rqbit-desktop".to_string()
            },
            fixture,
            ready,
            nodes: vec![window.with_bounds_from(&self.automation)],
        }
    }

    /// The `page-main` subtree: title-bar and toolbar controls, search and
    /// status filter, the explorer (included while hidden), the filtered
    /// torrent list, and the open detail panel.
    fn agent_main_page_node(&self, visible: bool, cx: &App) -> AutomationNode {
        let caps = self.host.capabilities();
        let mut children = vec![AutomationNode::new(
            AutomationRole::Button,
            "add-magnet",
            "Add Magnet…",
        )];
        if caps.settings_page {
            children.push(AutomationNode::new(
                AutomationRole::Button,
                "settings",
                "Settings",
            ));
        }
        if caps.logs_page {
            children.push(AutomationNode::new(AutomationRole::Button, "logs", "Logs"));
        }
        children.push(
            AutomationNode::new(AutomationRole::Button, "theme", "Theme").value(
                if cx.theme().is_dark() {
                    "dark"
                } else {
                    "light"
                },
            ),
        );
        for (id, name) in [
            ("start", "Start"),
            ("pause", "Pause"),
            ("delete", "Delete"),
            ("refresh", "Refresh"),
        ] {
            children.push(AutomationNode::new(AutomationRole::Button, id, name));
        }
        let explorer_visible = self.explorer_visible(cx);
        children.push(
            AutomationNode::new(AutomationRole::Button, "explorer-toggle", "Toggle Explorer")
                .checked(explorer_visible),
        );
        // The explorer container is listed even while hidden, so agents can
        // tell "toggled off" apart from "missing".
        children.push(
            AutomationNode::new(AutomationRole::Navigation, "explorer", "Explorer")
                .visible(explorer_visible),
        );
        children.push(
            AutomationNode::new(AutomationRole::TextBox, "torrent-search", "Search torrents")
                .value(self.search_input.read(cx).value().to_string()),
        );
        children.push(
            AutomationNode::new(AutomationRole::Button, "status-filter", "Status filter").value(
                self.status_filter
                    .read(cx)
                    .selected_value()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
            ),
        );
        children.push(self.agent_torrent_list_node(cx));
        if let Some(panel) = &self.detail_panel {
            let panel = panel.read(cx);
            let id = panel.torrent_id();
            children.push(
                AutomationNode::new(
                    AutomationRole::Page,
                    format!("torrent-detail-{id}"),
                    format!("Torrent {id}"),
                )
                .children(panel.agent_nodes(cx)),
            );
        }
        AutomationNode::new(AutomationRole::Page, "page-main", "Torrents")
            .visible(visible)
            .children(children)
    }

    /// The torrent list: one `torrent-{id}` row per currently visible
    /// (filtered) row, each with a `torrent-{id}-status` child.
    fn agent_torrent_list_node(&self, cx: &App) -> AutomationNode {
        let delegate = self.table_state.read(cx).delegate();
        AutomationNode::new(AutomationRole::List, "torrent-list", "Torrent list").children(
            delegate
                .rows
                .iter()
                .map(|row| {
                    AutomationNode::new(
                        AutomationRole::ListItem,
                        format!("torrent-{}", row.id),
                        row.name.clone(),
                    )
                    .child(
                        AutomationNode::new(
                            AutomationRole::Status,
                            format!("torrent-{}-status", row.id),
                            row.state.clone(),
                        )
                        .value(row.state.clone()),
                    )
                })
                .collect(),
        )
    }

    /// The open modal's subtree, built from the tracked body entity.
    fn agent_modal_node(&self, modal: &ActiveModal, cx: &App) -> AutomationNode {
        match modal {
            ActiveModal::Magnet(dialog) => {
                let value = dialog.read(cx).magnet_value(cx);
                AutomationNode::new(AutomationRole::Dialog, "dialog-magnet", "Add Magnet Link")
                    .child(
                        AutomationNode::new(AutomationRole::TextBox, "magnet-input", "Magnet link")
                            .value(value),
                    )
                    .child(AutomationNode::new(
                        AutomationRole::Button,
                        "magnet-add",
                        "Add",
                    ))
                    .child(AutomationNode::new(
                        AutomationRole::Button,
                        "magnet-cancel",
                        "Cancel",
                    ))
            }
            ActiveModal::Delete(dialog) => {
                let dialog = dialog.read(cx);
                AutomationNode::new(AutomationRole::Dialog, "dialog-delete", dialog.title())
                    .child(
                        AutomationNode::new(
                            AutomationRole::Checkbox,
                            "delete-files",
                            "Also delete downloaded files",
                        )
                        .checked(dialog.delete_files()),
                    )
                    .child(AutomationNode::new(
                        AutomationRole::Button,
                        "delete-confirm",
                        dialog.ok_label(),
                    ))
                    .child(AutomationNode::new(
                        AutomationRole::Button,
                        "delete-cancel",
                        "Cancel",
                    ))
            }
            ActiveModal::Add(dialog) => {
                let dialog = dialog.read(cx);
                AutomationNode::new(AutomationRole::Dialog, "dialog-add", dialog.title())
                    .child(AutomationNode::new(
                        AutomationRole::Button,
                        "add-ok",
                        dialog.ok_label(),
                    ))
                    .child(AutomationNode::new(
                        AutomationRole::Button,
                        "add-cancel",
                        "Cancel",
                    ))
            }
            ActiveModal::About(dialog) => {
                let info = dialog.read(cx).info();
                AutomationNode::new(
                    AutomationRole::Dialog,
                    "dialog-about",
                    info.app_name.to_string(),
                )
                .value(format!("{} {}", info.app_name, info.version))
                .child(AutomationNode::new(
                    AutomationRole::Button,
                    "about-close",
                    "Close",
                ))
            }
        }
    }

    /// Click a semantic target. Buttons reuse the exact handlers the human
    /// path uses; dialog Confirm/Cancel dispatch the same gpui actions as
    /// the footer buttons (and Enter/Escape).
    pub fn agent_click(
        &mut self,
        target: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        match target {
            "add-magnet" => {
                self.on_add_magnet(window, cx);
                Ok(())
            }
            "settings" => {
                self.on_settings(window, cx);
                if self.active_page_kind == Some(ActivePageKind::Settings) {
                    Ok(())
                } else {
                    Err("the settings page did not open".to_string())
                }
            }
            "logs" => {
                self.on_logs(window, cx);
                Ok(())
            }
            "theme" => {
                self.toggle_theme(window, cx);
                Ok(())
            }
            "start" | "pause" => {
                if self.selected_torrent_ids(cx).is_empty() {
                    return Err(format!("{target} requires at least one selected torrent"));
                }
                if target == "start" {
                    self.on_start(cx);
                } else {
                    self.on_pause(cx);
                }
                Ok(())
            }
            "delete" => {
                if self.selected_torrent_ids(cx).is_empty() {
                    return Err("delete requires at least one selected torrent".to_string());
                }
                self.on_delete(window, cx);
                Ok(())
            }
            "refresh" => {
                self.on_refresh(cx);
                Ok(())
            }
            "explorer-toggle" => {
                self.toggle_explorer(window, cx);
                Ok(())
            }
            "back" => {
                if self.active_page.is_some() {
                    self.close_page(cx);
                    Ok(())
                } else {
                    Err("no settings or logs page is open".to_string())
                }
            }
            "magnet-add" => self.agent_dispatch_dialog(ModalKind::Magnet, true, window, cx),
            "magnet-cancel" => self.agent_dispatch_dialog(ModalKind::Magnet, false, window, cx),
            "delete-confirm" => self.agent_dispatch_dialog(ModalKind::Delete, true, window, cx),
            "delete-cancel" => self.agent_dispatch_dialog(ModalKind::Delete, false, window, cx),
            "add-ok" => self.agent_dispatch_dialog(ModalKind::Add, true, window, cx),
            "add-cancel" => self.agent_dispatch_dialog(ModalKind::Add, false, window, cx),
            "about-close" => self.agent_dispatch_dialog(ModalKind::About, true, window, cx),
            "delete-files" => {
                let Some(ActiveModal::Delete(dialog)) = &self.active_modal else {
                    return Err(self.modal_mismatch(ModalKind::Delete));
                };
                let dialog = dialog.clone();
                let next = !dialog.read(cx).delete_files();
                dialog.update(cx, |d, cx| d.set_delete_files(next, cx));
                Ok(())
            }
            _ => self.agent_click_dynamic(target, cx),
        }
    }

    /// Click dispatch for id-parameterized targets: torrent rows, the detail
    /// panel's tabs, and file rows.
    fn agent_click_dynamic(&mut self, target: &str, cx: &mut Context<Self>) -> Result<(), String> {
        if parse_torrent_detail_target(target).is_some() {
            return Err(format!(
                "{target:?} is a container, not a clickable control"
            ));
        }
        if let Some(tab) = parse_detail_tab_target(target) {
            let tab = tab.to_string();
            let Some(panel) = self.detail_panel.clone() else {
                return Err(format!("{target:?} requires an open torrent detail panel"));
            };
            return panel.update(cx, |panel, cx| panel.agent_select_tab(&tab, cx));
        }
        if let Some((torrent_id, file_index)) = parse_torrent_file_target(target) {
            let Some(panel) = self.detail_panel.clone() else {
                return Err(format!("{target:?} requires an open torrent detail panel"));
            };
            return panel.update(cx, |panel, cx| {
                if panel.torrent_id() != torrent_id as usize {
                    return Err(format!(
                        "the detail panel shows torrent {}, not {torrent_id}",
                        panel.torrent_id()
                    ));
                }
                panel.agent_toggle_file(file_index, cx)
            });
        }
        if let Some(id) =
            parse_torrent_status_target(target).or_else(|| parse_torrent_row_target(target))
        {
            return self.agent_select_torrent_row(id, cx);
        }
        Err(format!("unknown automation target {target:?}"))
    }

    /// Select a torrent row by id, mirroring a plain mouse click: replace the
    /// delegate selection, record the shift-click anchor, and go through
    /// [`TableState::set_selected_row`] so the `SelectRow` subscription opens
    /// the detail panel.
    fn agent_select_torrent_row(&mut self, id: u64, cx: &mut Context<Self>) -> Result<(), String> {
        let row_ix = self
            .table_state
            .read(cx)
            .delegate()
            .rows
            .iter()
            .position(|r| r.id == id)
            .ok_or_else(|| format!("torrent-{id} is not in the filtered torrent list"))?;
        self.table_state.update(cx, |state, cx| {
            {
                let delegate = state.delegate_mut();
                delegate.selected_rows.clear();
                delegate.selected_rows.insert(row_ix);
                delegate.set_anchor(id);
            }
            state.set_selected_row(row_ix, cx);
        });
        Ok(())
    }

    /// Dispatch the active dialog's Confirm/Cancel action after validating
    /// that `kind` is the open dialog.
    fn agent_dispatch_dialog(
        &mut self,
        kind: ModalKind,
        confirm: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        match &self.active_modal {
            Some(modal) if modal.kind() == kind => {
                if confirm {
                    window.dispatch_action(Box::new(Confirm { secondary: false }), cx);
                } else {
                    window.dispatch_action(Box::new(Cancel), cx);
                }
                Ok(())
            }
            _ => Err(self.modal_mismatch(kind)),
        }
    }

    /// Why a dialog-scoped target is unavailable right now.
    fn modal_mismatch(&self, expected: ModalKind) -> String {
        match &self.active_modal {
            Some(modal) => format!(
                "the {} dialog is not open (the {} dialog is)",
                expected.name(),
                modal.kind().name()
            ),
            None => format!("the {} dialog is not open", expected.name()),
        }
    }

    /// Replace the text of a semantic textbox. Only `torrent-search` and
    /// `magnet-input` are writable.
    ///
    /// `InputState::set_value` deliberately does not emit
    /// [`InputEvent::Change`], so the search path applies the filter directly
    /// instead of relying on the change subscription.
    pub fn agent_set_value(
        &mut self,
        target: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        match target {
            "torrent-search" => {
                let input = self.search_input.clone();
                input.update(cx, |state, cx| {
                    state.set_value(value.to_string(), window, cx)
                });
                self.apply_search_and_filter(cx);
                Ok(())
            }
            "magnet-input" => {
                let Some(ActiveModal::Magnet(dialog)) = &self.active_modal else {
                    return Err(self.modal_mismatch(ModalKind::Magnet));
                };
                let dialog = dialog.clone();
                dialog.update(cx, |d, cx| d.set_magnet(value, window, cx));
                Ok(())
            }
            _ => Err(format!("set-value is not supported for target {target:?}")),
        }
    }

    /// Append text to a semantic textbox, through the same path as
    /// [`Self::agent_set_value`].
    pub fn agent_type_text(
        &mut self,
        target: &str,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let current = match target {
            "torrent-search" => self.search_input.read(cx).value().to_string(),
            "magnet-input" => match &self.active_modal {
                Some(ActiveModal::Magnet(dialog)) => dialog.read(cx).magnet_value(cx),
                _ => return Err(self.modal_mismatch(ModalKind::Magnet)),
            },
            _ => return Err(format!("type is not supported for target {target:?}")),
        };
        self.agent_set_value(target, &format!("{current}{text}"), window, cx)
    }

    /// Send a named key to a dialog target: "enter"/"return" confirms,
    /// "escape"/"esc" cancels — the same gpui actions the keyboard triggers.
    pub fn agent_key(
        &mut self,
        target: &str,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let kind = match target {
            "dialog-magnet" | "magnet-input" | "magnet-add" | "magnet-cancel" => ModalKind::Magnet,
            "dialog-delete" | "delete-files" | "delete-confirm" | "delete-cancel" => {
                ModalKind::Delete
            }
            "dialog-add" | "add-ok" | "add-cancel" => ModalKind::Add,
            "dialog-about" | "about-close" => ModalKind::About,
            _ => {
                return Err(format!(
                    "key dispatch is only supported on dialog targets, not {target:?}"
                ));
            }
        };
        match key.to_ascii_lowercase().as_str() {
            "enter" | "return" => self.agent_dispatch_dialog(kind, true, window, cx),
            "escape" | "esc" => self.agent_dispatch_dialog(kind, false, window, cx),
            other => Err(format!(
                "unsupported key {other:?} (expected \"enter\" or \"escape\")"
            )),
        }
    }

    /// Focus a semantic textbox (`torrent-search` or `magnet-input`).
    pub fn agent_focus(
        &mut self,
        target: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let handle = match target {
            "torrent-search" => self.search_input.read(cx).focus_handle(cx),
            "magnet-input" => match &self.active_modal {
                Some(ActiveModal::Magnet(dialog)) => dialog.read(cx).input_focus_handle(cx),
                _ => return Err(self.modal_mismatch(ModalKind::Magnet)),
            },
            _ => return Err(format!("focus is not supported for target {target:?}")),
        };
        handle.focus(window, cx);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // NOTE: no `use super::*` here — it would pull gpui's own `#[test]`
    // attribute macro (re-exported via `use gpui::*` above) into scope and
    // shadow the builtin `#[test]` that `#[gpui_kit::test]` expands to.
    use super::title_bar_actions;
    use gpui_kit::component::{TitleBar, button::Button};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Context, InteractiveElement as _, IntoElement,
        ParentElement as _, Point, Render, StatefulInteractiveElement as _, Styled as _,
        TestAppContext, Window, WindowBounds, WindowOptions, div, px, size,
    };
    use std::{cell::Cell, rc::Rc};

    /// A real `TitleBar` holding one action button, wrapped in a click-counting
    /// ancestor. Clicks on the button that bubble past the title bar are
    /// counted by the ancestor; the title bar's own double-click handler is not
    /// observable headlessly, but it sits on the same bubbling path.
    struct Harness {
        guarded: bool,
        button_clicks: Rc<Cell<usize>>,
        ancestor_clicks: Rc<Cell<usize>>,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let button_clicks = self.button_clicks.clone();
            let ancestor_clicks = self.ancestor_clicks.clone();
            let cluster = if self.guarded {
                title_bar_actions()
            } else {
                div().flex().items_center().gap_2()
            };
            div()
                .id("ancestor")
                .size_full()
                .on_click(move |_, _, _| ancestor_clicks.set(ancestor_clicks.get() + 1))
                .child(
                    TitleBar::new().child(
                        cluster.child(
                            Button::new("theme")
                                .label("theme")
                                .on_click(move |_, _, _| {
                                    button_clicks.set(button_clicks.get() + 1)
                                }),
                        ),
                    ),
                )
        }
    }

    fn open_harness(
        cx: &mut TestAppContext,
        guarded: bool,
    ) -> (AnyWindowHandle, Rc<Cell<usize>>, Rc<Cell<usize>>) {
        let button_clicks = Rc::new(Cell::new(0));
        let ancestor_clicks = Rc::new(Cell::new(0));
        cx.update(gpui_kit::init);
        let (window, _) = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(600.), px(200.)),
                    })),
                    ..Default::default()
                },
                cx,
                {
                    let button_clicks = button_clicks.clone();
                    let ancestor_clicks = ancestor_clicks.clone();
                    move |_, cx| {
                        cx.new(|_| Harness {
                            guarded,
                            button_clicks,
                            ancestor_clicks,
                        })
                    }
                },
            )
            .expect("open test window")
        });
        (window, button_clicks, ancestor_clicks)
    }

    #[gpui_kit::test]
    fn double_click_on_action_button_stays_out_of_the_title_bar(cx: &mut TestAppContext) {
        let (window, button_clicks, ancestor_clicks) = open_harness(cx, true);

        cx.update_window(window, |_, window, cx| window.double_click("theme", cx))
            .expect("test window open");

        assert_eq!(
            button_clicks.get(),
            2,
            "the button still receives both clicks"
        );
        assert_eq!(
            ancestor_clicks.get(),
            0,
            "nothing above the action cluster sees the clicks"
        );
    }

    /// Control for the test above: without the guard the same double-click
    /// bubbles through the title bar to its ancestors, which is how the title
    /// bar's double-click-to-zoom handler gets triggered. Skipped on Linux,
    /// where that handler calls `zoom_window`, which the headless test
    /// platform does not implement.
    #[cfg(not(target_os = "linux"))]
    #[gpui_kit::test]
    fn double_click_on_unguarded_button_bubbles_through_the_title_bar(cx: &mut TestAppContext) {
        let (window, button_clicks, ancestor_clicks) = open_harness(cx, false);

        cx.update_window(window, |_, window, cx| window.double_click("theme", cx))
            .expect("test window open");

        assert_eq!(button_clicks.get(), 2);
        assert_eq!(ancestor_clicks.get(), 2);
    }

    /// A plain `Dialog` renders no buttons from `button_props` alone — the
    /// footer must be built explicitly. This pins the full footer wiring used
    /// by the app's dialogs: both footer buttons are painted, clicking them
    /// dispatches the dialog's Cancel/Confirm actions (so the `button_props`
    /// handlers stay the single decision point), and a `true` return closes
    /// the dialog.
    #[gpui_kit::test]
    fn dialog_footer_buttons_dispatch_confirm_and_cancel(cx: &mut TestAppContext) {
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::component::button::ButtonVariant;
        use gpui_kit::component::dialog::{DialogButtonProps, DialogFooter};
        use gpui_kit::{AppContext as _, IntoElement, ParentElement as _, Render, Window, div};

        use super::{dialog_cancel_button, dialog_confirm_button};

        struct Blank;
        impl Render for Blank {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }

        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let window = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(800.), px(600.)),
                    })),
                    ..Default::default()
                },
                cx,
                |_, cx| cx.new(|_| Blank),
            )
            .expect("open test window")
            .0
        });

        let cancelled = Rc::new(Cell::new(0));
        let confirmed = Rc::new(Cell::new(0));

        let draw_frame = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                window.draw(cx).clear(cx);
            })
            .expect("test window open");
        };

        // A frame mounts the dialog layer, the next paints it at rest.
        let open_dialog = |cx: &mut TestAppContext| {
            let cancelled = cancelled.clone();
            let confirmed = confirmed.clone();
            cx.update_window(window, |_, window, cx| {
                window.open_dialog(cx, {
                    let cancelled = cancelled.clone();
                    let confirmed = confirmed.clone();
                    move |d, _, _| {
                        d.title("Test")
                            .button_props(DialogButtonProps::default().on_ok({
                                let confirmed = confirmed.clone();
                                move |_, _, _| {
                                    confirmed.set(confirmed.get() + 1);
                                    true
                                }
                            }))
                            .on_cancel({
                                let cancelled = cancelled.clone();
                                move |_, _, _| {
                                    cancelled.set(cancelled.get() + 1);
                                    true
                                }
                            })
                            .footer(
                                DialogFooter::new()
                                    .child(dialog_cancel_button("test-cancel"))
                                    .child(dialog_confirm_button(
                                        "test-ok",
                                        "OK",
                                        ButtonVariant::Primary,
                                    )),
                            )
                    }
                });
            })
            .expect("test window open");
        };

        open_dialog(cx);
        draw_frame(cx);
        draw_frame(cx);

        cx.update_window(window, |_, window, cx| window.click("test-cancel", cx))
            .expect("test window open");
        assert_eq!(cancelled.get(), 1, "Cancel runs the cancel handler");
        assert_eq!(confirmed.get(), 0);

        open_dialog(cx);
        draw_frame(cx);
        draw_frame(cx);

        cx.update_window(window, |_, window, cx| window.click("test-ok", cx))
            .expect("test window open");
        assert_eq!(confirmed.get(), 1, "OK runs the confirm handler");
        assert_eq!(cancelled.get(), 1);
    }

    /// The delete dialog is an `AlertDialog` with the library's default
    /// footer (`.confirm()`'s Cancel plus OK), customized through
    /// `button_props` alone. This pins that wiring the way
    /// `dialog_footer_buttons_dispatch_confirm_and_cancel` pins the custom
    /// footer: both default buttons are painted, clicking them dispatches the
    /// dialog's Cancel/Confirm actions into the `button_props` handlers (the
    /// single decision point), and a `true` return closes the dialog.
    #[gpui_kit::test]
    fn delete_alert_dialog_footer_dispatches_confirm_and_cancel(cx: &mut TestAppContext) {
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::component::button::ButtonVariant;
        use gpui_kit::component::dialog::DialogButtonProps;
        use gpui_kit::{AppContext as _, IntoElement, ParentElement as _, Render, Window, div};

        use super::DeleteDialog;

        struct Blank;
        impl Render for Blank {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }

        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let window = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(800.), px(600.)),
                    })),
                    ..Default::default()
                },
                cx,
                |_, cx| cx.new(|_| Blank),
            )
            .expect("open test window")
            .0
        });

        let cancelled = Rc::new(Cell::new(0));
        let confirmed = Rc::new(Cell::new(0));

        let draw_frame = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                window.draw(cx).clear(cx);
            })
            .expect("test window open");
        };

        // A frame mounts the dialog layer, the next paints it at rest.
        let open_alert = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                let dialog =
                    cx.new(|cx| DeleteDialog::new(vec![1], vec!["t".to_string()], window, cx));
                window.open_alert_dialog(cx, {
                    let cancelled = cancelled.clone();
                    let confirmed = confirmed.clone();
                    move |d, _, cx| {
                        let title = dialog.read(cx).title();
                        let ok_label = dialog.read(cx).ok_label();
                        let question = dialog.read(cx).question();
                        d.title(title)
                            .description(question)
                            .confirm()
                            .ok_text(ok_label)
                            .ok_variant(ButtonVariant::Danger)
                            .width(px(450.))
                            .button_props(
                                DialogButtonProps::default()
                                    .on_ok({
                                        let confirmed = confirmed.clone();
                                        move |_, _, _| {
                                            confirmed.set(confirmed.get() + 1);
                                            true
                                        }
                                    })
                                    .on_cancel({
                                        let cancelled = cancelled.clone();
                                        move |_, _, _| {
                                            cancelled.set(cancelled.get() + 1);
                                            true
                                        }
                                    }),
                            )
                            .child(dialog.clone())
                    }
                });
            })
            .expect("test window open");
        };

        let dialog_open = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| window.has_active_dialog(cx))
                .expect("test window open")
        };

        open_alert(cx);
        draw_frame(cx);
        draw_frame(cx);
        assert!(dialog_open(cx), "the alert dialog is up before the click");

        cx.update_window(window, |_, window, cx| window.click("cancel", cx))
            .expect("test window open");
        assert_eq!(cancelled.get(), 1, "Cancel runs the cancel handler");
        assert_eq!(confirmed.get(), 0);
        assert!(!dialog_open(cx), "a `true` cancel closes the dialog");

        open_alert(cx);
        draw_frame(cx);
        draw_frame(cx);
        assert!(dialog_open(cx));

        cx.update_window(window, |_, window, cx| window.click("ok", cx))
            .expect("test window open");
        assert_eq!(confirmed.get(), 1, "OK runs the confirm handler");
        assert_eq!(cancelled.get(), 1);
        assert!(!dialog_open(cx), "a `true` confirm closes the dialog");
    }
}

/// Agent-surface tests: a deterministic in-memory client/host drive a real
/// `MainPanel` headlessly, exercising stable semantic IDs, filtering, and
/// dispatch. Gated on the feature because `MainPanel::new_with_agent` and the
/// dispatch methods only exist with `agent-testing`.
#[cfg(all(test, feature = "agent-testing"))]
mod agent_tests {
    // NOTE: no `use super::*` — it would pull gpui's own `#[test]` attribute
    // macro into scope and shadow the builtin one (see the module above).
    use super::MainPanel;
    use crate::client::{AddTorrentRequest, ClientResult, TorrentClient, UiAddSource};
    use crate::host::{UiCapabilities, UiHost};
    use crate::ui::agent::{AutomationNode, AutomationSnapshot};
    use async_trait::async_trait;
    use futures::stream::LocalBoxStream;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement as _,
        Point, Render, TestAppContext, Window, WindowBounds, WindowOptions, div, px, size,
    };
    use rqbit_protocol::{
        ApiAddTorrent, DhtStats, PeerStatsSnapshot, ProtoError, ProtoErrorKind, SessionStats,
        TorrentAction, TorrentDetails, TorrentFile, TorrentId, TorrentList, TorrentState,
        TorrentStats,
    };
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    /// Deterministic, in-memory `TorrentClient`: two seeded torrents, actions
    /// recorded, magnet adds accepted.
    struct MockClient {
        torrents: RefCell<BTreeMap<u64, TorrentDetails>>,
        actions: RefCell<Vec<(u64, TorrentAction)>>,
    }

    impl MockClient {
        fn new() -> Self {
            let torrents: BTreeMap<u64, TorrentDetails> = [
                mock_torrent(1, "Alpha torrent", TorrentState::Live),
                mock_torrent(2, "Beta torrent", TorrentState::Paused),
            ]
            .into_iter()
            .map(|t| (t.id.expect("mock id"), t))
            .collect();
            Self {
                torrents: RefCell::new(torrents),
                actions: RefCell::new(Vec::new()),
            }
        }

        fn recorded_actions(&self) -> Vec<(u64, TorrentAction)> {
            self.actions.borrow().clone()
        }

        fn lookup(&self, id: &TorrentId) -> ClientResult<TorrentDetails> {
            let id = id
                .as_id()
                .ok_or_else(|| ProtoError::bad_request("mock ids are numeric"))?;
            self.torrents.borrow().get(&id).cloned().ok_or_else(|| {
                ProtoError::new(
                    ProtoErrorKind::TorrentNotFound,
                    format!("mock torrent {id} not found"),
                )
            })
        }
    }

    #[async_trait(?Send)]
    impl TorrentClient for MockClient {
        async fn list(&self, _with_stats: bool) -> ClientResult<TorrentList> {
            let torrents = self
                .torrents
                .borrow()
                .values()
                .cloned()
                .map(|mut t| {
                    t.files = None;
                    t
                })
                .collect();
            Ok(TorrentList { torrents })
        }

        async fn details(&self, torrent: &TorrentId) -> ClientResult<TorrentDetails> {
            self.lookup(torrent)
        }

        async fn stats(&self, torrent: &TorrentId) -> ClientResult<TorrentStats> {
            self.lookup(torrent)?
                .stats
                .as_deref()
                .cloned()
                .ok_or_else(|| ProtoError::internal("mock torrent is missing stats"))
        }

        async fn peer_stats(&self, torrent: &TorrentId) -> ClientResult<PeerStatsSnapshot> {
            self.lookup(torrent)?;
            Ok(PeerStatsSnapshot::default())
        }

        async fn session_stats(&self) -> ClientResult<SessionStats> {
            Ok(SessionStats {
                uptime_seconds: 42,
                ..Default::default()
            })
        }

        async fn session_default_folder(&self) -> ClientResult<Option<String>> {
            Ok(Some("/mock".into()))
        }

        async fn dht_stats(&self) -> ClientResult<DhtStats> {
            Err(ProtoError::new(
                ProtoErrorKind::DhtDisabled,
                "DHT is disabled in the mock",
            ))
        }

        async fn action(&self, torrent: &TorrentId, action: TorrentAction) -> ClientResult<()> {
            let id = torrent
                .as_id()
                .ok_or_else(|| ProtoError::bad_request("mock ids are numeric"))?;
            if matches!(action, TorrentAction::Forget | TorrentAction::Delete) {
                if self.torrents.borrow_mut().remove(&id).is_none() {
                    return Err(ProtoError::new(
                        ProtoErrorKind::TorrentNotFound,
                        format!("mock torrent {id} not found"),
                    ));
                }
                self.actions.borrow_mut().push((id, action));
                return Ok(());
            }
            if !self.torrents.borrow().contains_key(&id) {
                return Err(ProtoError::new(
                    ProtoErrorKind::TorrentNotFound,
                    format!("mock torrent {id} not found"),
                ));
            }
            self.actions.borrow_mut().push((id, action));
            Ok(())
        }

        async fn add_torrent(&self, req: &AddTorrentRequest) -> ClientResult<ApiAddTorrent> {
            let UiAddSource::Magnet(_) = &req.source else {
                return Err(ProtoError::bad_request("the mock accepts magnets only"));
            };
            let mut details = mock_torrent(0, "Mock magnet", TorrentState::Paused);
            let id = if req.list_only {
                None
            } else {
                let id = self
                    .torrents
                    .borrow()
                    .keys()
                    .next_back()
                    .copied()
                    .unwrap_or(0)
                    + 1;
                details.id = Some(id);
                self.torrents.borrow_mut().insert(id, details.clone());
                Some(id)
            };
            Ok(ApiAddTorrent {
                id,
                details,
                output_folder: "/mock".into(),
            })
        }

        async fn set_rust_log(&self, _value: String) -> ClientResult<()> {
            Ok(())
        }

        fn log_lines(&self) -> ClientResult<LocalBoxStream<'static, Vec<u8>>> {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    fn mock_torrent(id: u64, name: &str, state: TorrentState) -> TorrentDetails {
        let files = vec![
            TorrentFile {
                name: format!("{name} - first.txt"),
                components: vec![format!("{name} - first.txt")],
                length: 512,
                included: true,
                padding: false,
                custom: None,
            },
            TorrentFile {
                name: format!("{name} - second.txt"),
                components: vec![format!("{name} - second.txt")],
                length: 512,
                included: true,
                padding: false,
                custom: None,
            },
        ];
        TorrentDetails {
            id: Some(id),
            info_hash: format!("{id:040x}"),
            name: Some(name.into()),
            files: Some(files),
            stats: Some(Box::new(TorrentStats {
                state,
                file_progress: vec![256, 256],
                error: None,
                progress_bytes: 512,
                uploaded_bytes: 0,
                total_bytes: 1024,
                finished: false,
                live: (state == TorrentState::Live).then_some(Default::default()),
            })),
            ..Default::default()
        }
    }

    /// A placeholder full page the mock host opens for "settings".
    struct DummyPage;
    impl Render for DummyPage {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().child("dummy settings page")
        }
    }

    struct MockHost;
    impl UiHost for MockHost {
        fn capabilities(&self) -> UiCapabilities {
            UiCapabilities {
                add_torrent_files: false,
                open_folder: false,
                open_file: false,
                settings_page: true,
                logs_page: true,
            }
        }

        fn version(&self) -> String {
            "agent-test".into()
        }

        fn open_settings(
            &self,
            _client: Arc<dyn TorrentClient>,
            _window: &mut Window,
            cx: &mut Context<MainPanel>,
        ) -> Option<gpui_kit::AnyView> {
            Some(cx.new(|_| DummyPage).into())
        }
    }

    fn open_panel(
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<MainPanel>, Arc<MockClient>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        // TorrentClient is deliberately ?Send and UI-thread-local.
        #[allow(clippy::arc_with_non_send_sync)]
        let client = Arc::new(MockClient::new());
        let (window, panel) = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(1200.), px(800.)),
                    })),
                    ..Default::default()
                },
                cx,
                {
                    let client = client.clone();
                    move |window, cx| {
                        cx.new(|cx| {
                            MainPanel::new_with_agent(
                                client.clone(),
                                Arc::new(MockHost),
                                window,
                                cx,
                            )
                        })
                    }
                },
            )
            .expect("open test window")
        });
        (window, panel, client)
    }

    fn draw_frame(window: AnyWindowHandle, cx: &mut TestAppContext) {
        cx.update_window(window, |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("test window open");
    }

    /// MainPanel's one-second polling task is intentionally perpetual, so an
    /// unbounded run_until_parked would never finish. Pump only immediate work.
    fn pump(cx: &TestAppContext) {
        for _ in 0..64 {
            if !cx.executor().tick() {
                break;
            }
        }
    }

    fn snapshot(panel: &Entity<MainPanel>, cx: &TestAppContext) -> AutomationSnapshot {
        cx.read(|cx| panel.read(cx).agent_snapshot(true, cx))
    }

    fn click(
        panel: &Entity<MainPanel>,
        window: AnyWindowHandle,
        target: &str,
        cx: &mut TestAppContext,
    ) -> Result<(), String> {
        cx.update_window(window, |_, window, cx| {
            panel.update(cx, |panel, cx| panel.agent_click(target, window, cx))
        })
        .expect("test window open")
    }

    fn set_value(
        panel: &Entity<MainPanel>,
        window: AnyWindowHandle,
        target: &str,
        value: &str,
        cx: &mut TestAppContext,
    ) -> Result<(), String> {
        cx.update_window(window, |_, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.agent_set_value(target, value, window, cx)
            })
        })
        .expect("test window open")
    }

    fn find_node<'a>(nodes: &'a [AutomationNode], id: &str) -> Option<&'a AutomationNode> {
        nodes.iter().find_map(|n| {
            if n.id == id {
                Some(n)
            } else {
                find_node(&n.children, id)
            }
        })
    }

    fn torrent_row_ids(snap: &AutomationSnapshot) -> Vec<&str> {
        find_node(&snap.nodes, "torrent-list")
            .expect("torrent-list node")
            .children
            .iter()
            .map(|n| n.id.as_str())
            .collect()
    }

    #[gpui_kit::test]
    fn snapshot_has_stable_ids_and_reflects_filtering(cx: &mut TestAppContext) {
        let (window, panel, _client) = open_panel(cx);
        pump(cx);
        draw_frame(window, cx);
        draw_frame(window, cx);

        let snap = snapshot(&panel, cx);
        assert!(snap.ready, "the first paint marks the snapshot ready");
        assert!(snap.fixture);
        assert_eq!(snap.app, "rqbit-desktop-fixture");
        assert_eq!(snap.nodes.len(), 1);
        assert_eq!(snap.nodes[0].id, "rqbit-window");
        assert!(
            snap.nodes[0].bounds.is_some(),
            "the window root records painted bounds"
        );

        for id in [
            "page-main",
            "add-magnet",
            "settings",
            "logs",
            "theme",
            "start",
            "pause",
            "delete",
            "refresh",
            "explorer-toggle",
            "explorer",
            "torrent-search",
            "status-filter",
            "torrent-list",
        ] {
            assert!(find_node(&snap.nodes, id).is_some(), "missing node {id}");
        }

        // The explorer starts hidden but is still listed.
        let explorer = find_node(&snap.nodes, "explorer").unwrap();
        assert!(!explorer.visible, "the explorer starts hidden");
        let toggle = find_node(&snap.nodes, "explorer-toggle").unwrap();
        assert_eq!(toggle.checked, Some(false));

        // Rows use torrent-id identity and carry a status child.
        assert_eq!(torrent_row_ids(&snap), vec!["torrent-1", "torrent-2"]);
        let row = find_node(&snap.nodes, "torrent-1").unwrap();
        assert_eq!(row.name, "Alpha torrent");
        assert!(row.bounds.is_some(), "torrent rows retain painted bounds");
        assert!(
            find_node(&snap.nodes, "torrent-search")
                .unwrap()
                .bounds
                .is_some(),
            "child trackers run before the window root and must not be cleared"
        );
        let status = find_node(&snap.nodes, "torrent-1-status").unwrap();
        assert_eq!(status.name, "live");
        assert_eq!(status.value.as_deref(), Some("live"));

        // Search filtering re-derives the visible rows; the snapshot follows.
        set_value(&panel, window, "torrent-search", "alpha", cx).expect("set search");
        let snap = snapshot(&panel, cx);
        assert_eq!(torrent_row_ids(&snap), vec!["torrent-1"]);
        let search = find_node(&snap.nodes, "torrent-search").unwrap();
        assert_eq!(search.value.as_deref(), Some("alpha"));

        // Unknown targets never silently succeed.
        let err = click(&panel, window, "no-such-target", cx).unwrap_err();
        assert!(
            err.contains("unknown automation target"),
            "unexpected error: {err}"
        );
        // A real torrent id hidden by the filter is not clickable either.
        let err = click(&panel, window, "torrent-2", cx).unwrap_err();
        assert!(
            err.contains("not in the filtered torrent list"),
            "unexpected error: {err}"
        );
    }

    #[gpui_kit::test]
    fn row_click_opens_details_and_drives_tabs_and_files(cx: &mut TestAppContext) {
        let (window, panel, client) = open_panel(cx);
        pump(cx);
        draw_frame(window, cx);

        click(&panel, window, "torrent-2", cx).expect("row click");
        // The SelectRow subscription queues the panel; a render creates it,
        // then its details fetch completes.
        pump(cx);
        draw_frame(window, cx);
        pump(cx);

        let snap = snapshot(&panel, cx);
        let detail = find_node(&snap.nodes, "torrent-detail-2").expect("detail node");
        let overview = find_node(&detail.children, "detail-tab-overview").unwrap();
        assert_eq!(overview.checked, Some(true), "the overview tab is active");
        assert!(find_node(&detail.children, "torrent-2-file-0").is_some());
        assert!(find_node(&detail.children, "torrent-2-file-1").is_some());

        // A row of a different torrent retargets the detail panel.
        click(&panel, window, "torrent-1", cx).expect("row click");
        pump(cx);
        let snap = snapshot(&panel, cx);
        assert!(find_node(&snap.nodes, "torrent-detail-1").is_some());
        assert!(find_node(&snap.nodes, "torrent-detail-2").is_none());

        // Switch to the Files tab and toggle a file by its torrent file index.
        click(&panel, window, "detail-tab-files", cx).expect("files tab");
        let snap = snapshot(&panel, cx);
        let detail = find_node(&snap.nodes, "torrent-detail-1").unwrap();
        assert_eq!(
            find_node(&detail.children, "detail-tab-files")
                .unwrap()
                .checked,
            Some(true)
        );

        click(&panel, window, "torrent-1-file-1", cx).expect("toggle file");
        pump(cx);
        assert_eq!(
            client.recorded_actions(),
            vec![(1, TorrentAction::UpdateOnlyFiles(vec![0]))],
            "toggling file 1 off persists the remaining selection"
        );
        let snap = snapshot(&panel, cx);
        let file = find_node(&snap.nodes, "torrent-1-file-1").unwrap();
        assert_eq!(file.checked, Some(false));

        // File targets resolve by torrent file index, not row position.
        let err = click(&panel, window, "torrent-1-file-9", cx).unwrap_err();
        assert!(err.contains("not in torrent 1"), "unexpected error: {err}");
        // A file click for the wrong torrent id is rejected.
        let err = click(&panel, window, "torrent-2-file-0", cx).unwrap_err();
        assert!(
            err.contains("detail panel shows torrent 1"),
            "unexpected error: {err}"
        );
    }

    #[gpui_kit::test]
    fn magnet_dialog_set_value_confirm_and_cancel(cx: &mut TestAppContext) {
        let (window, panel, _client) = open_panel(cx);
        pump(cx);
        draw_frame(window, cx);

        // Dialog-scoped targets fail while no dialog is open.
        let err = click(&panel, window, "magnet-add", cx).unwrap_err();
        assert!(
            err.contains("magnet dialog is not open"),
            "unexpected error: {err}"
        );

        click(&panel, window, "add-magnet", cx).expect("open magnet dialog");
        draw_frame(window, cx);
        draw_frame(window, cx);

        let snap = snapshot(&panel, cx);
        assert!(find_node(&snap.nodes, "dialog-magnet").is_some());
        assert!(find_node(&snap.nodes, "magnet-input").is_some());
        assert!(find_node(&snap.nodes, "magnet-add").is_some());
        assert!(find_node(&snap.nodes, "magnet-cancel").is_some());
        let page = find_node(&snap.nodes, "page-main").unwrap();
        assert!(!page.visible, "a modal marks the page behind it invisible");
        for id in [
            "add-magnet",
            "start",
            "delete",
            "torrent-search",
            "torrent-1",
        ] {
            assert!(
                !find_node(&snap.nodes, id).unwrap().visible,
                "modal background target {id} must not be drivable"
            );
        }

        // Confirming with an empty input keeps the dialog open (the human
        // path's decision, unchanged).
        click(&panel, window, "magnet-add", cx).expect("dispatch confirm");
        pump(cx);
        let snap = snapshot(&panel, cx);
        assert!(
            find_node(&snap.nodes, "dialog-magnet").is_some(),
            "an empty magnet input does not close the dialog"
        );

        // Fill the input and confirm: the add-torrent dialog takes over.
        set_value(
            &panel,
            window,
            "magnet-input",
            "magnet:?xt=urn:btih:0000000000000000000000000000000000000003",
            cx,
        )
        .expect("set magnet");
        let snap = snapshot(&panel, cx);
        let input = find_node(&snap.nodes, "magnet-input").unwrap();
        assert_eq!(
            input.value.as_deref(),
            Some("magnet:?xt=urn:btih:0000000000000000000000000000000000000003")
        );

        click(&panel, window, "magnet-add", cx).expect("confirm magnet");
        pump(cx);
        draw_frame(window, cx);
        pump(cx);
        let snap = snapshot(&panel, cx);
        assert!(find_node(&snap.nodes, "dialog-magnet").is_none());
        assert!(
            find_node(&snap.nodes, "dialog-add").is_some(),
            "the file-selection dialog follows the magnet dialog"
        );

        // Confirm the add: the mock gains torrent 3 and the modal clears.
        click(&panel, window, "add-ok", cx).expect("confirm add");
        pump(cx);
        draw_frame(window, cx);
        pump(cx);
        let snap = snapshot(&panel, cx);
        assert!(find_node(&snap.nodes, "dialog-add").is_none());
        assert!(
            find_node(&snap.nodes, "page-main").unwrap().visible,
            "the page is visible again once the modal closed"
        );
        assert_eq!(
            torrent_row_ids(&snap),
            vec!["torrent-1", "torrent-2", "torrent-3"]
        );
    }

    #[gpui_kit::test]
    fn magnet_cancel_and_delete_flow(cx: &mut TestAppContext) {
        let (window, panel, client) = open_panel(cx);
        pump(cx);
        draw_frame(window, cx);

        // Open and cancel the magnet dialog.
        click(&panel, window, "add-magnet", cx).expect("open magnet dialog");
        draw_frame(window, cx);
        click(&panel, window, "magnet-cancel", cx).expect("cancel");
        pump(cx);
        let snap = snapshot(&panel, cx);
        assert!(find_node(&snap.nodes, "dialog-magnet").is_none());

        // Select a torrent and delete it without deleting files (Forget).
        click(&panel, window, "torrent-2", cx).expect("select torrent");
        pump(cx);
        draw_frame(window, cx);
        click(&panel, window, "delete", cx).expect("open delete dialog");
        pump(cx);
        draw_frame(window, cx);
        draw_frame(window, cx);

        let snap = snapshot(&panel, cx);
        let dialog = find_node(&snap.nodes, "dialog-delete").expect("delete dialog");
        assert_eq!(
            find_node(&dialog.children, "delete-files").unwrap().checked,
            Some(true),
            "delete-files defaults to checked"
        );

        // Uncheck "also delete downloaded files" via its semantic id.
        click(&panel, window, "delete-files", cx).expect("toggle delete-files");
        let snap = snapshot(&panel, cx);
        assert_eq!(
            find_node(&snap.nodes, "delete-files").unwrap().checked,
            Some(false)
        );

        click(&panel, window, "delete-confirm", cx).expect("confirm delete");
        pump(cx);
        draw_frame(window, cx);
        pump(cx);
        let snap = snapshot(&panel, cx);
        assert!(find_node(&snap.nodes, "dialog-delete").is_none());
        assert_eq!(torrent_row_ids(&snap), vec!["torrent-1"]);
        assert_eq!(
            client.recorded_actions(),
            vec![(2, TorrentAction::Forget)],
            "an unchecked delete-files box forgets instead of deleting"
        );
    }

    #[gpui_kit::test]
    fn settings_and_logs_pages_report_their_kind(cx: &mut TestAppContext) {
        let (window, panel, _client) = open_panel(cx);
        pump(cx);
        draw_frame(window, cx);

        click(&panel, window, "settings", cx).expect("open settings");
        pump(cx);
        let snap = snapshot(&panel, cx);
        assert!(find_node(&snap.nodes, "page-settings").is_some());
        assert!(find_node(&snap.nodes, "page-main").is_none());
        assert!(find_node(&snap.nodes, "back").is_some());

        click(&panel, window, "back", cx).expect("close settings");
        pump(cx);
        let snap = snapshot(&panel, cx);
        assert!(find_node(&snap.nodes, "page-main").is_some());

        click(&panel, window, "logs", cx).expect("open logs");
        pump(cx);
        let snap = snapshot(&panel, cx);
        assert!(find_node(&snap.nodes, "page-logs").is_some());

        click(&panel, window, "back", cx).expect("close logs");
        pump(cx);
        let snap = snapshot(&panel, cx);
        assert!(find_node(&snap.nodes, "page-main").is_some());

        // Back with no page open is an error, not a no-op.
        let err = click(&panel, window, "back", cx).unwrap_err();
        assert!(
            err.contains("no settings or logs page is open"),
            "unexpected error: {err}"
        );
    }
}
