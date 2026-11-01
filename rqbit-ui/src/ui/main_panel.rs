use gpui::prelude::FluentBuilder as _;
use gpui::*;
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
    /// Keeps the connection-state watcher alive (remote clients); the
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
}

impl MainPanel {
    pub fn new(
        client: Arc<dyn TorrentClient>,
        host: Arc<dyn UiHost>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let weak = cx.entity().downgrade();

        // The dockable main layout: Explorer tree docked left, the torrent
        // table + detail split as the center panel.
        let (dock_area, dock_skin) = DockSkin::dock_area("main-dock", None, window, cx);
        dock_skin.set_panel_style(PanelStyle::TabBar, cx);
        let explorer = cx.new(|cx| ExplorerPanel::new(client.clone(), host.clone(), cx));

        let table_state = cx.new(|cx| {
            TableState::new(
                TorrentTableDelegate::new(weak, client.clone(), host.clone()),
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
        let dialog = cx.new(|cx| MagnetDialog::new(window, cx));
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
        let dialog = cx.new(|cx| AddTorrentDialog::new(entries, window, cx));
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
            })
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
                    .child(
                        Button::new("add-magnet")
                            .label("Add Magnet…")
                            .small()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.on_add_magnet(window, cx)),
                            ),
                    )
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
                    .child(
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
                    ),
            )
            .pr(px(10.))
    }

    /// Render the toolbar: selection actions on the left, search box and
    /// status filter on the right. Extra left padding for macOS traffic lights.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
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
                    .child(
                        Button::new("start")
                            .label("Start")
                            .icon(IconName::Play)
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.on_start(cx))),
                    )
                    .child(
                        Button::new("pause")
                            .label("Pause")
                            .icon(IconName::Pause)
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.on_pause(cx))),
                    )
                    .child(
                        Button::new("delete")
                            .label("Delete")
                            .icon(IconName::CircleX)
                            .small()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.on_delete(window, cx)),
                            ),
                    )
                    .child(
                        div().pl_4().child(
                            Button::new("refresh")
                                .tooltip("Refresh")
                                .icon(IconName::LoaderCircle)
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| this.on_refresh(cx))),
                        ),
                    ),
            )
            .child(h_flex().flex_grow_1().size_full())
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Select::new(&self.status_filter)
                            .cleanable(true)
                            .with_size(Size::Small)
                            .placeholder("All statuses")
                            .w(px(160.)),
                    )
                    .child(
                        Input::new(&self.search_input)
                            .with_size(Size::Small)
                            .cleanable(true)
                            .min_w(px(200.))
                            .w(px(200.)),
                    ),
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
            .left(
                Button::new("explorer-toggle")
                    .tooltip("Toggle Explorer")
                    .icon(IconName::PanelLeft)
                    .small()
                    .ghost()
                    .selected(self.explorer_visible(cx))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_explorer(window, cx);
                    })),
            )
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
        let dialog = cx.new(|cx| DeleteDialog::new(ids, names, window, cx));
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
        let dialog = cx.new(|cx| AboutDialog::new(info, window, cx));
        window.open_dialog(cx, {
            let dialog = dialog.clone();
            move |d, _, cx| {
                let app_name = dialog.read(cx).info().app_name.clone();
                d.title(app_name)
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
