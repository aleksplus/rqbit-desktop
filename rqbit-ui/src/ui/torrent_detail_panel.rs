use gpui::*;
use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable, WindowExt as _,
    button::{Button, ButtonVariants},
    h_flex,
    notification::Notification,
    scroll::ScrollableElement,
    tab::{Tab, TabBar},
    table::{Column, ColumnSort, DataTable, TableDelegate, TableState},
    v_flex,
};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use rqbit_protocol::{PeerEntry, PeerStatsSnapshot, TorrentDetails, TorrentId, TorrentStats};

use crate::client::TorrentClient;
use crate::host::UiHost;
use crate::ui::file_table::{FileRow, FileTableDelegate};
use crate::ui::utils::{format_bytes, format_speed};

/// Events emitted by [`TorrentDetailPanel`] to communicate with the parent view.
#[derive(Clone, Debug)]
pub enum TorrentDetailPanelEvent {
    /// User clicked **Back** – close the detail panel.
    Back,
}

/// Which tab is active in the detail panel.
#[derive(Clone, Copy, PartialEq)]
enum DetailTab {
    Overview,
    Trackers,
    Peers,
    Files,
}

/// Panel that displays detailed information about a single torrent.
///
/// Uses a tabbed layout: Overview, Trackers, Peers, Files.
pub struct TorrentDetailPanel {
    client: Arc<dyn TorrentClient>,
    host: Arc<dyn UiHost>,
    torrent_id: usize,
    active_tab: DetailTab,
    details: Option<TorrentDetails>,
    stats: Option<TorrentStats>,
    comment: Option<String>,
    peer_stats: Option<PeerStatsSnapshot>,
    file_table_state: Entity<TableState<FileTableDelegate>>,
    peer_table_state: Entity<TableState<PeerTableDelegate>>,
    focus_handle: FocusHandle,
    /// Keeps the periodic peer-stats polling task alive for the lifetime of the panel.
    _peer_task: Option<Task<()>>,
}

/// Marker type so repeated "playlist copied" notifications replace each other
/// instead of stacking up.
struct PlaylistCopiedNotice;

impl EventEmitter<TorrentDetailPanelEvent> for TorrentDetailPanel {}

impl TorrentDetailPanel {
    pub fn new(
        torrent_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
        client: Arc<dyn TorrentClient>,
        host: Arc<dyn UiHost>,
    ) -> Self {
        let panel_weak = cx.entity().downgrade();
        let file_table_state = cx.new(|cx| {
            let mut delegate = FileTableDelegate::new();
            delegate.show_progress();
            delegate.set_table(cx.entity().downgrade());
            delegate.on_toggle = Some(Arc::new({
                let panel = panel_weak.clone();
                move |file_index, included, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |panel, cx| {
                            panel.set_file_included(file_index, included, cx);
                        });
                    }
                }
            }));
            delegate.on_toggle_all = Some(Arc::new({
                let panel = panel_weak.clone();
                move |included, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |panel, cx| {
                            panel.set_all_files_included(included, cx);
                        });
                    }
                }
            }));
            TableState::new(delegate, window, cx).row_selectable(false)
        });
        let peer_table_state = cx
            .new(|cx| TableState::new(PeerTableDelegate::new(), window, cx).row_selectable(false));

        let mut this = Self {
            client,
            host,
            torrent_id,
            active_tab: DetailTab::Overview,
            details: None,
            stats: None,
            comment: None,
            peer_stats: None,
            file_table_state,
            peer_table_state,
            focus_handle: cx.focus_handle(),
            _peer_task: None,
        };

        this.fetch_details(cx);
        this.start_peer_polling(cx);
        this
    }

    /// Switch the panel to display a different torrent, refreshing data.
    pub fn switch_torrent(&mut self, torrent_id: usize, cx: &mut Context<Self>) {
        if self.torrent_id == torrent_id {
            return;
        }
        self.torrent_id = torrent_id;
        self.details = None;
        self.stats = None;
        self.comment = None;
        self.peer_stats = None;
        self.fetch_details(cx);
    }

    fn fetch_details(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let torrent_id = self.torrent_id;
        let file_table_state = self.file_table_state.clone();
        let peer_table_state = self.peer_table_state.clone();

        cx.spawn(async move |this, cx| {
            let torrent = TorrentId::Id(torrent_id as u64);
            let details = client.details(&torrent).await;
            let stats = client.stats(&torrent).await;
            let peer_stats = client.peer_stats(&torrent).await;
            let custom_columns = client.custom_columns().await;

            let _ = this.update(cx, |this, cx| {
                if let Ok(d) = &details {
                    let file_progress = stats
                        .as_ref()
                        .ok()
                        .map(|s| s.file_progress.clone())
                        .unwrap_or_default();
                    let files: Vec<FileRow> = d
                        .files
                        .as_ref()
                        .map(|f| {
                            f.iter()
                                .enumerate()
                                .map(|(idx, file)| FileRow {
                                    file_index: idx,
                                    name: file.name.clone(),
                                    length: file.length,
                                    included: file.included,
                                    exists: false,
                                    progress: file_progress.get(idx).copied().unwrap_or(0),
                                    custom: file.custom.clone().unwrap_or_default(),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    file_table_state.update(cx, |state, cx| {
                        let delegate = state.delegate_mut();
                        delegate.rows = files;
                        delegate.apply_sort();
                        // Hook-provided file columns: swap the set when it
                        // changed (dropped while disconnected), then refresh
                        // so the framework re-reads the columns.
                        if delegate.set_custom_columns(custom_columns.as_ref()) {
                            state.refresh(cx);
                        }
                        cx.notify();
                    });
                }

                if let Ok(ref ps) = peer_stats {
                    let peer_rows: Vec<PeerRow> =
                        ps.peers.iter().map(PeerRow::from_entry).collect();
                    peer_table_state.update(cx, |state, cx| {
                        let delegate = state.delegate_mut();
                        delegate.rows = peer_rows;
                        delegate.apply_sort();
                        cx.notify();
                    });
                }

                this.details = details.ok();
                this.stats = stats.ok();
                this.comment = this.details.as_ref().and_then(|d| d.comment.clone());
                this.peer_stats = peer_stats.ok();
                cx.notify();
            });
        })
        .detach();
    }

    /// Periodically refresh the peer table (and torrent stats) while the detail
    /// panel is open. Mirrors `MainPanel::start_stats_polling`: the refresh is
    /// not tied to window focus, since on macOS background-app windows stay
    /// visible and the stats calls are cheap.
    fn start_peer_polling(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let task = cx.spawn(async move |this, cx| {
            loop {
                // Read the current torrent id each iteration so that switching
                // torrents (via `switch_torrent`) is reflected in the polling.
                let torrent_id = this.update(cx, |this, _cx| this.torrent_id).unwrap_or(0);
                let torrent = TorrentId::Id(torrent_id as u64);

                let peer_stats = client.peer_stats(&torrent).await;
                let stats = client.stats(&torrent).await;
                // Hook-provided custom values ride on the details payload.
                let details = client.details(&torrent).await;

                let _ = this.update(cx, |this, cx| {
                    if let Ok(ref ps) = peer_stats {
                        let peer_rows: Vec<PeerRow> =
                            ps.peers.iter().map(PeerRow::from_entry).collect();
                        this.peer_table_state.update(cx, |state, cx| {
                            let delegate = state.delegate_mut();
                            delegate.rows = peer_rows;
                            delegate.apply_sort();
                            cx.notify();
                        });
                    }
                    if let Ok(s) = stats {
                        // Update per-file progress bars from the latest stats.
                        let file_progress = s.file_progress.clone();
                        this.file_table_state.update(cx, |state, cx| {
                            let delegate = state.delegate_mut();
                            for row in delegate.rows.iter_mut() {
                                row.progress =
                                    file_progress.get(row.file_index).copied().unwrap_or(0);
                            }
                            delegate.apply_sort();
                            cx.notify();
                        });
                        this.stats = Some(s);
                    }
                    if let Ok(d) = details
                        && let Some(files) = &d.files
                    {
                        this.file_table_state.update(cx, |state, cx| {
                            let delegate = state.delegate_mut();
                            for row in delegate.rows.iter_mut() {
                                if let Some(file) = files.get(row.file_index) {
                                    row.custom = file.custom.clone().unwrap_or_default();
                                }
                            }
                            delegate.apply_sort();
                            cx.notify();
                        });
                    }
                    this.peer_stats = peer_stats.ok();
                    cx.notify();
                });

                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
        });
        self._peer_task = Some(task);
    }

    fn on_back(&mut self, cx: &mut Context<Self>) {
        cx.emit(TorrentDetailPanelEvent::Back);
    }

    /// Toggle inclusion of a file and persist it via the client.
    fn set_file_included(&mut self, file_index: usize, included: bool, cx: &mut Context<Self>) {
        // Update the local row immediately for responsive UI.
        self.file_table_state.update(cx, |state, cx| {
            if let Some(row) = state.delegate_mut().rows.get_mut(file_index) {
                row.included = included;
            }
            cx.notify();
        });

        let client = self.client.clone();
        let torrent_id = self.torrent_id;
        let file_table_state = self.file_table_state.clone();

        cx.spawn(async move |_this, cx| {
            // Compute the new set of included file indices from the current table state.
            let only_files: HashSet<usize> = file_table_state.read_with(cx, |state, _| {
                state
                    .delegate()
                    .rows
                    .iter()
                    .filter(|r| r.included)
                    .map(|r| r.file_index)
                    .collect::<HashSet<usize>>()
            });

            let _ = client
                .action(
                    &TorrentId::Id(torrent_id as u64),
                    rqbit_protocol::TorrentAction::UpdateOnlyFiles(
                        only_files.into_iter().map(|ix| ix as u64).collect(),
                    ),
                )
                .await;
        })
        .detach();
    }

    /// Set inclusion state for all files and persist it via the client.
    fn set_all_files_included(&mut self, included: bool, cx: &mut Context<Self>) {
        // Update the local rows immediately for responsive UI.
        self.file_table_state.update(cx, |state, cx| {
            for row in state.delegate_mut().rows.iter_mut() {
                row.included = included;
            }
            cx.notify();
        });

        let client = self.client.clone();
        let torrent_id = self.torrent_id;
        let file_table_state = self.file_table_state.clone();

        cx.spawn(async move |_this, cx| {
            let only_files: HashSet<usize> = file_table_state.read_with(cx, |state, _| {
                state
                    .delegate()
                    .rows
                    .iter()
                    .filter(|r| r.included)
                    .map(|r| r.file_index)
                    .collect::<HashSet<usize>>()
            });

            let _ = client
                .action(
                    &TorrentId::Id(torrent_id as u64),
                    rqbit_protocol::TorrentAction::UpdateOnlyFiles(
                        only_files.into_iter().map(|ix| ix as u64).collect(),
                    ),
                )
                .await;
        })
        .detach();
    }

    /// The playlist URL (M3U8) for this torrent, provided by the host
    /// (desktop: derived from the embedded HTTP API; remote: unavailable).
    fn playlist_url(&self) -> Option<String> {
        self.host.playlist_url(self.torrent_id as u64)
    }

    /// Copy the playlist URL to the clipboard and show a green notification
    /// (mirroring the webui's "Copied" alert).
    fn copy_playlist_url_to_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(url) = self.playlist_url() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(url));
        window.push_notification(
            Notification::success("Playlist URL copied to clipboard. Paste into e.g. VLC to play.")
                .title("Copied")
                .id::<PlaylistCopiedNotice>(),
            cx,
        );
    }

    /// The "Playlist" row in the General section, mirroring the webui's
    /// `PlaylistLink`: a link that copies the playlist URL to the clipboard
    /// (paste into e.g. VLC to play), or "Unavailable" when there is none.
    fn render_playlist_row(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();

        let value = match self.playlist_url() {
            None => div()
                .text_color(theme.muted_foreground)
                .child("Unavailable")
                .into_any_element(),
            Some(_) => Button::new("playlist-copy-link")
                .link()
                .label("Copy playlist URL")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.copy_playlist_url_to_clipboard(window, cx);
                }))
                .into_any_element(),
        };

        h_flex()
            .gap_2()
            .child(
                div()
                    .w(px(140.))
                    .text_color(theme.muted_foreground)
                    .child("Playlist"),
            )
            .child(value)
    }

    fn render_overview(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let name = self
            .details
            .as_ref()
            .and_then(|d| d.name.clone())
            .unwrap_or_else(|| "Loading…".to_string());

        let info_hash = self
            .details
            .as_ref()
            .map(|d| d.info_hash.clone())
            .unwrap_or_default();

        let output_folder = self
            .details
            .as_ref()
            .map(|d| d.output_folder.clone())
            .unwrap_or_default();

        let total_pieces = self.details.as_ref().map(|d| d.total_pieces).unwrap_or(0);

        let state_str = self
            .stats
            .as_ref()
            .map(|s| s.state.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let progress_str = self.stats.as_ref().map_or("N/A".to_string(), |s| {
            if s.total_bytes > 0 {
                format!(
                    "{:.1}%",
                    (s.progress_bytes as f64 / s.total_bytes as f64) * 100.0
                )
            } else {
                "N/A".to_string()
            }
        });

        let downloaded_str = self
            .stats
            .as_ref()
            .map_or("N/A".to_string(), |s| format_bytes(s.progress_bytes));

        let uploaded_str = self
            .stats
            .as_ref()
            .map_or("N/A".to_string(), |s| format_bytes(s.uploaded_bytes));

        let total_str = self
            .stats
            .as_ref()
            .map_or("N/A".to_string(), |s| format_bytes(s.total_bytes));

        let (down_str, up_str, peers_str, eta_str) = self
            .stats
            .as_ref()
            .and_then(|s| s.live.as_ref())
            .map(|live| {
                let peers = format!("{}/{}", live.peers_live, live.peers_seen);
                let down = format_speed(live.download_speed.mbps);
                let up = format_speed(live.upload_speed.mbps);
                let eta = live
                    .time_remaining_secs
                    .map(crate::ui::utils::format_eta)
                    .unwrap_or_else(|| "—".to_string());
                (down, up, peers, eta)
            })
            .unwrap_or((
                "N/A".to_string(),
                "N/A".to_string(),
                "N/A".to_string(),
                "—".to_string(),
            ));

        let error_str = self
            .stats
            .as_ref()
            .and_then(|s| s.error.clone())
            .unwrap_or_default();

        v_flex()
            .gap_4()
            .p_4()
            .overflow_y_scrollbar()
            .child(
                div()
                    .text_size(px(20.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(name),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("General"),
                    )
                    .child(info_row("Info Hash", info_hash))
                    .child(info_row("Output Folder", output_folder))
                    .child(self.render_playlist_row(cx))
                    .child(info_row("Total Pieces", total_pieces.to_string()))
                    .child(info_row("State", state_str))
                    .children(self.comment.as_ref().map(|comment| {
                        h_flex()
                            .gap_2()
                            .items_center()
                            .py_1()
                            .child(
                                div()
                                    .w(px(140.))
                                    .flex_shrink_0()
                                    .text_color(theme.muted_foreground)
                                    .child("Comment"),
                            )
                            .child(
                                Button::new("btn-link")
                                    .link()
                                    .label(comment.clone())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(comment) = this.comment.clone() {
                                            cx.open_url(&comment);
                                        }
                                    })),
                            )
                    })),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Statistics"),
                    )
                    .child(info_row("Progress", progress_str))
                    .child(info_row("Downloaded", downloaded_str))
                    .child(info_row("Uploaded", uploaded_str))
                    .child(info_row("Total Size", total_str))
                    .child(info_row("Download Speed", down_str))
                    .child(info_row("Upload Speed", up_str))
                    .child(info_row("Peers", peers_str))
                    .child(info_row("ETA", eta_str)),
            )
            .children(if error_str.is_empty() {
                None
            } else {
                Some(
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(14.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.danger)
                                .child("Error"),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(theme.danger)
                                .child(error_str),
                        ),
                )
            })
    }

    fn render_trackers(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let trackers = self
            .details
            .as_ref()
            .map(|d| d.trackers.as_slice())
            .unwrap_or_default();
        if trackers.is_empty() {
            return v_flex().p_4().child(
                div()
                    .text_color(gpui::rgb(0x888888))
                    .child("No trackers found."),
            );
        }

        v_flex()
            .p_4()
            .gap_2()
            .child(
                div()
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("Trackers ({})", trackers.len())),
            )
            .children(trackers.iter().map(|t| {
                h_flex()
                    .gap_2()
                    .py_1()
                    .child(div().flex_1().child(t.clone()))
            }))
    }

    fn render_peers(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .p_4()
            .size_full()
            .child(DataTable::new(&self.peer_table_state))
    }

    fn render_files(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .p_4()
            .size_full()
            .child(DataTable::new(&self.file_table_state))
    }
}

impl Render for TorrentDetailPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_tab = self.active_tab;

        v_flex()
            .size_full()
            .gap_0()
            // Tab bar
            .pl(px(10.))
            .pr(px(10.))
            .child(
                TabBar::new("detail-tabs")
                    .underline()
                    .suffix(
                        h_flex().gap_1().child(
                            Button::new("close")
                                .icon(IconName::Close)
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(|this, _, _, cx| this.on_back(cx))),
                        ),
                    )
                    .selected_index(match active_tab {
                        DetailTab::Overview => 0,
                        DetailTab::Trackers => 1,
                        DetailTab::Peers => 2,
                        DetailTab::Files => 3,
                    })
                    .on_click(cx.listener(|this, index, _, cx| {
                        this.active_tab = match index {
                            0 => DetailTab::Overview,
                            1 => DetailTab::Trackers,
                            2 => DetailTab::Peers,
                            3 => DetailTab::Files,
                            _ => DetailTab::Overview,
                        };
                        cx.notify();
                    }))
                    .child(Tab::new().label("Overview"))
                    .child(Tab::new().label("Trackers"))
                    .child(Tab::new().label("Peers"))
                    .child(Tab::new().label("Files")),
            )
            // Tab content
            .child(div().flex_1().min_h_0().child(match active_tab {
                DetailTab::Overview => self.render_overview(cx).into_any_element(),
                DetailTab::Trackers => self.render_trackers(cx).into_any_element(),
                DetailTab::Peers => self.render_peers(cx).into_any_element(),
                DetailTab::Files => self.render_files(cx).into_any_element(),
            }))
    }
}

impl Focusable for TorrentDetailPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// --- Peer table ---

/// A single row in the peer table.
#[derive(Clone)]
struct PeerRow {
    address: String,
    state: String,
    client: String,
    conn_kind: String,
    downloaded: u64,
    uploaded: u64,
}

impl PeerRow {
    fn from_entry(entry: &PeerEntry) -> Self {
        PeerRow {
            address: entry.addr.clone(),
            state: entry.stats.state.clone(),
            client: entry.stats.client_name.clone().unwrap_or_default(),
            conn_kind: entry
                .stats
                .conn_kind
                .map(|k| k.to_string())
                .unwrap_or_default(),
            downloaded: entry.stats.fetched_bytes,
            uploaded: entry.stats.uploaded_bytes,
        }
    }
}

/// Table delegate for the peer list.
struct PeerTableDelegate {
    rows: Vec<PeerRow>,
    columns: Vec<Column>,
    /// Currently active sort column index (None = no active sort).
    sort_col_ix: Option<usize>,
    /// Currently active sort direction.
    sort_dir: ColumnSort,
}

impl PeerTableDelegate {
    fn new() -> Self {
        Self {
            rows: Vec::new(),
            columns: vec![
                Column::new("address", "Address").width(180.).sortable(),
                Column::new("state", "State").width(80.).sortable(),
                Column::new("client", "Client").width(150.).sortable(),
                Column::new("conn", "Connection").width(120.).sortable(),
                Column::new("downloaded", "Downloaded")
                    .width(120.)
                    .sortable(),
                Column::new("uploaded", "Uploaded").width(120.).sortable(),
            ],
            // Default sort by address (ascending) so peers are stable/grouped.
            sort_col_ix: Some(0),
            sort_dir: ColumnSort::Ascending,
        }
    }

    /// Apply the currently persisted sort (`sort_col_ix` / `sort_dir`) to
    /// `self.rows`. No-op when no sort is active. Called after each data
    /// refresh so the sort survives live updates.
    fn apply_sort(&mut self) {
        let Some(col_ix) = self.sort_col_ix else {
            return;
        };
        let Some(col) = self.columns.get(col_ix) else {
            return;
        };
        let key = col.key.as_ref().to_string();

        let cmp = |a: &PeerRow, b: &PeerRow| -> std::cmp::Ordering {
            match key.as_str() {
                "address" => a.address.cmp(&b.address),
                "state" => a.state.cmp(&b.state),
                "client" => a.client.cmp(&b.client),
                "conn" => a.conn_kind.cmp(&b.conn_kind),
                "downloaded" => a.downloaded.cmp(&b.downloaded),
                "uploaded" => a.uploaded.cmp(&b.uploaded),
                _ => std::cmp::Ordering::Equal,
            }
        };

        match self.sort_dir {
            ColumnSort::Default => self.rows.sort_by_key(|r| r.address.clone()),
            ColumnSort::Ascending => self.rows.sort_by(cmp),
            ColumnSort::Descending => {
                self.rows.sort_by(cmp);
                self.rows.reverse();
            }
        }
    }
}

impl TableDelegate for PeerTableDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let row = &self.rows[row_ix];
        let col = &self.columns[col_ix];

        match col.key.as_ref() {
            "address" => div().child(row.address.clone()),
            "state" => div().child(row.state.clone()),
            "client" => div().child(row.client.clone()),
            "conn" => div().child(row.conn_kind.clone()),
            "downloaded" => div().child(format_bytes(row.downloaded)),
            "uploaded" => div().child(format_bytes(row.uploaded)),
            _ => div(),
        }
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        // Persist the active sort so it survives subsequent data refreshes
        // (which replace `self.rows` wholesale).
        self.sort_col_ix = Some(col_ix);
        self.sort_dir = sort;
        self.apply_sort();
        cx.notify();
    }
}

// --- Helpers ---

/// Build a label/value info row.
fn info_row(label: impl Into<String>, value: impl Into<String>) -> Div {
    let label = label.into();
    let value = value.into();
    h_flex()
        .gap_2()
        .child(
            div()
                .w(px(140.))
                .text_color(gpui::rgb(0x888888))
                .child(label),
        )
        .child(div().flex_1().child(value))
}
