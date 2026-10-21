//! Torrent list table: row model, selection/sort state, and cell rendering.

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, checkbox::Checkbox, h_flex,
    menu::PopupMenuItem, progress::Progress,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rqbit_protocol::{TorrentDetails, TorrentState};

use crate::client::TorrentClient;
use crate::host::UiHost;
use crate::ui::main_panel::MainPanel;
use crate::ui::utils::{
    format_bytes, format_eta, format_speed, torrent_action_menu_item, torrent_bulk_action_menu_item,
};

/// Visual tone for a torrent row, derived from the torrent state. Drives
/// row-level styling: paused rows render with dimmed text, and the progress
/// bar fills green while downloading and red on error.
#[derive(Clone, Copy, PartialEq)]
pub enum RowTone {
    /// Default appearance.
    Normal,
    /// Paused (or initializing in a paused holding state): dimmed row text.
    Paused,
    /// Live and not yet finished downloading: green progress bar.
    Downloading,
    /// Live and finished (seeding): default progress bar.
    Seeding,
    /// Errored: red progress bar.
    Error,
}

/// Simplified torrent row data that implements Clone.
///
/// Each display field has a corresponding raw numeric field (suffixed `_raw`)
/// that holds the original value from the API. Sorting uses the raw values
/// directly, avoiding lossy re-parsing of formatted strings.
#[derive(Clone)]
pub struct TorrentRow {
    pub id: u64,
    pub name: String,
    pub state: String,
    /// Display string, e.g. "42.1%".
    pub progress: String,
    /// 0.0–100.0
    pub progress_raw: f64,
    pub peers: String,
    /// Derived visual tone used for row styling (dim text, bar colors).
    pub tone: RowTone,
    /// Live peer count (connected), used for sorting.
    pub peers_raw: u32,
    pub down_speed: String,
    /// Download speed in bytes/s.
    pub down_speed_raw: f64,
    pub up_speed: String,
    /// Upload speed in bytes/s.
    pub up_speed_raw: f64,
    pub downloaded: String,
    /// Downloaded bytes.
    pub downloaded_raw: u64,
    pub uploaded: String,
    /// Uploaded bytes.
    pub uploaded_raw: u64,
    pub eta: String,
    /// ETA in seconds; `None` means unknown / not applicable (sorts last).
    pub eta_raw: Option<u64>,
    /// Raw destination folder from the torrent list (may be empty).
    pub output_folder: String,
    /// Normalized grouping key (`normalize_folder`); empty = never grouped.
    pub folder_key: String,
    /// 0 = top-level (standalone or group parent), 1 = indented child row.
    /// Stamped by [`derive_display_rows`] on the visible copies; `all_rows`
    /// entries always keep 0.
    pub depth: u8,
    /// Members sharing this row's `folder_key`, including itself. 1 =
    /// standalone row. Stamped by [`derive_display_rows`] on the visible
    /// copies; `all_rows` entries always keep 1.
    pub group_size: usize,
}

impl TorrentRow {
    /// Build a display row from a torrent-list entry.
    ///
    /// Data is tiered: `id`/`name` are always present, size and progress fields
    /// need `stats`, and peer/speed/ETA fields additionally need live stats.
    /// Fields whose tier is missing get placeholder display strings ("N/A",
    /// "—", "Unknown") with zeroed/`None` raw values so sorting still behaves
    /// sensibly.
    pub fn from_details(t: &TorrentDetails) -> Self {
        let stats = t.stats.as_deref();
        let live = stats.and_then(|s| s.live.as_ref());

        // Progress percentage; a torrent with unknown total size counts as 0%.
        let progress_raw = stats.map_or(0.0, |s| {
            if s.total_bytes > 0 {
                s.progress_bytes as f64 / s.total_bytes as f64 * 100.0
            } else {
                0.0
            }
        });

        // Visual tone for row styling. Initializing torrents that are paused
        // count as paused (e.g. watched-folder torrents are added that way).
        let tone = match stats {
            None => RowTone::Normal,
            Some(s) => match s.state {
                TorrentState::Error => RowTone::Error,
                TorrentState::Paused => RowTone::Paused,
                TorrentState::Initializing { paused: true } => RowTone::Paused,
                TorrentState::Initializing { paused: false } => RowTone::Normal,
                TorrentState::Live if !s.finished => RowTone::Downloading,
                TorrentState::Live => RowTone::Seeding,
            },
        };

        Self {
            id: t.id.unwrap_or(0),
            name: t.name.clone().unwrap_or_else(|| t.info_hash.clone()),
            state: stats.map_or_else(|| "Unknown".into(), |s| s.state.to_string()),
            progress: match stats {
                Some(s) if s.total_bytes > 0 => format!("{progress_raw:.1}%"),
                Some(_) => "0%".to_string(),
                None => "N/A".to_string(),
            },
            progress_raw,
            peers: live.map_or_else(
                || "N/A".into(),
                |l| format!("{}/{}", l.peers_live, l.peers_seen),
            ),
            peers_raw: live.map_or(0, |l| l.peers_live),
            down_speed: live.map_or_else(|| "N/A".into(), |l| format_speed(l.download_speed.mbps)),
            down_speed_raw: live.map_or(0.0, |l| l.download_speed.as_bytes() as f64),
            tone,
            up_speed: live.map_or_else(|| "N/A".into(), |l| format_speed(l.upload_speed.mbps)),
            up_speed_raw: live.map_or(0.0, |l| l.upload_speed.as_bytes() as f64),
            downloaded: stats.map_or_else(|| "N/A".into(), |s| format_bytes(s.progress_bytes)),
            downloaded_raw: stats.map_or(0, |s| s.progress_bytes),
            uploaded: stats.map_or_else(|| "N/A".into(), |s| format_bytes(s.uploaded_bytes)),
            uploaded_raw: stats.map_or(0, |s| s.uploaded_bytes),
            eta: live.map_or_else(
                || "—".into(),
                |l| {
                    l.time_remaining_secs
                        .map(format_eta)
                        .unwrap_or_else(|| "—".to_string())
                },
            ),
            eta_raw: live.and_then(|l| l.time_remaining_secs),
            output_folder: t.output_folder.clone(),
            folder_key: normalize_folder(&t.output_folder),
            depth: 0,
            group_size: 1,
        }
    }

    /// True when this row heads a multi-torrent destination-folder group
    /// (renders the chevron and child-count badge).
    pub fn is_parent(&self) -> bool {
        self.depth == 0 && self.group_size > 1
    }
}

/// Normalized destination-folder grouping key: trailing separators trimmed,
/// nothing else — the strings otherwise come from the same source (the
/// session's output folder), so no case/Unicode normalization is wanted.
/// The empty string means "ungrouped".
fn normalize_folder(folder: &str) -> String {
    folder.trim_end_matches(['/', '\\']).to_string()
}

/// Active search/status filter stored in the delegate; applied to `all_rows`
/// to derive the visible `rows`.
#[derive(Default)]
struct TorrentFilter {
    /// Name substring filter, case-insensitive; empty = no search filter.
    search: String,
    /// Exact match against the torrent state; empty = all statuses.
    status: String,
}

/// Whether a row passes the given (already lowercased/trimmed) search needle
/// and exact status match. Empty needle/status means "no constraint".
fn row_matches(r: &TorrentRow, needle: &str, status: &str) -> bool {
    (needle.is_empty() || r.name.to_lowercase().contains(needle))
        && (status.is_empty() || r.state == status)
}

/// Ordering comparator for a sortable column key; larger == "greater".
/// Uses the raw numeric fields directly — no string re-parsing. Unknown (or
/// absent) keys compare Equal; callers tie-break by id.
fn column_key_cmp(key: Option<&str>) -> impl Fn(&TorrentRow, &TorrentRow) -> std::cmp::Ordering {
    let key = key.unwrap_or("").to_string();
    move |a, b| match key.as_str() {
        "name" => a.name.cmp(&b.name),
        "state" => a.state.cmp(&b.state),
        "progress" => a
            .progress_raw
            .partial_cmp(&b.progress_raw)
            .unwrap_or(std::cmp::Ordering::Equal),
        "peers" => a.peers_raw.cmp(&b.peers_raw),
        "down" => a.down_speed_raw.total_cmp(&b.down_speed_raw),
        "downloaded" => a.downloaded_raw.cmp(&b.downloaded_raw),
        "up" => a.up_speed_raw.total_cmp(&b.up_speed_raw),
        "uploaded" => a.uploaded_raw.cmp(&b.uploaded_raw),
        "eta" => {
            // `None` (unknown ETA) sorts last in ascending order.
            match (a.eta_raw, b.eta_raw) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
        }
        _ => std::cmp::Ordering::Equal,
    }
}

/// Derive the flat list of visible display rows from `all_rows`:
/// filter → group by destination folder → expand → sort → flatten.
///
/// Grouping is fully derived from the data — there are no stored parent
/// links. Within a folder the lowest-id member (the torrent added first,
/// since ids increase monotonically) is the parent and always renders first;
/// the others become indented child rows. Deleting the parent therefore
/// promotes the next-oldest member on the next refresh, and a folder that
/// shrinks to a single torrent renders as a standalone row again.
///
/// Torrents whose destination is the session's DEFAULT download folder
/// (`default_folder`, normalized the same way as `folder_key`) never group:
/// every add lands there unless pointed elsewhere, so grouping it would
/// collapse the whole list into one group.
///
/// While a filter is active, a group survives when ANY member matches, and
/// it is force-expanded so matching and sibling rows stay visible together.
fn derive_display_rows(
    all_rows: &[TorrentRow],
    filter: &TorrentFilter,
    sort_key: Option<&str>,
    sort_dir: ColumnSort,
    collapsed: &HashSet<String>,
    default_folder: Option<&str>,
) -> Vec<TorrentRow> {
    let needle = filter.search.trim().to_lowercase();
    let status = filter.status.as_str();
    let filter_active = !needle.is_empty() || !status.is_empty();
    let matches = |r: &TorrentRow| row_matches(r, &needle, status);
    let default_key = default_folder.map(normalize_folder);

    // 1. Partition into folder groups (non-empty key) and standalones.
    let mut groups: HashMap<&str, Vec<&TorrentRow>> = HashMap::new();
    let mut standalones: Vec<&TorrentRow> = Vec::new();
    for row in all_rows {
        if row.folder_key.is_empty() || Some(row.folder_key.as_str()) == default_key.as_deref() {
            standalones.push(row);
        } else {
            groups.entry(row.folder_key.as_str()).or_default().push(row);
        }
    }

    // 2. Build display units. `unit[0]` is the parent; a unit renders either
    // all of its members (expanded) or just the parent (collapsed).
    let mut units: Vec<Vec<TorrentRow>> = Vec::new();
    for (key, mut members) in groups {
        members.sort_by_key(|r| r.id);
        if filter_active && !members.iter().any(|r| matches(r)) {
            continue;
        }
        let expanded = filter_active || !collapsed.contains(key);
        let size = members.len();
        let unit: Vec<TorrentRow> = if expanded {
            members.iter().map(|r| (*r).clone()).collect()
        } else {
            vec![members[0].clone()]
        }
        .into_iter()
        .map(|mut r| {
            r.group_size = size;
            r
        })
        .collect();
        units.push(unit);
    }
    for row in standalones {
        if filter_active && !matches(row) {
            continue;
        }
        units.push(vec![row.clone()]);
    }

    // 3. Sort: the active column orders the top-level units by their
    // parent's key and the children within each unit; the parent always
    // stays positionally first. Every arm tie-breaks by ascending id — the
    // list arrives in HashMap order, so input position must never matter.
    let key_cmp = column_key_cmp(sort_key);
    let dir_cmp = |a: &TorrentRow, b: &TorrentRow| match sort_dir {
        ColumnSort::Default => a.id.cmp(&b.id),
        ColumnSort::Ascending => key_cmp(a, b).then(a.id.cmp(&b.id)),
        ColumnSort::Descending => key_cmp(a, b).reverse().then(a.id.cmp(&b.id)),
    };
    for unit in &mut units {
        unit[1..].sort_by(|a, b| dir_cmp(a, b));
    }
    units.sort_by(|a, b| dir_cmp(&a[0], &b[0]));

    // 4. Flatten, tagging child rows.
    units
        .into_iter()
        .flat_map(|unit| {
            unit.into_iter()
                .enumerate()
                .map(|(i, mut r)| {
                    r.depth = (i > 0) as u8;
                    r
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Resolve a shift-click anchor torrent id to its row index among `rows`;
/// `None` when that torrent is filtered out or deleted.
fn resolve_anchor_row(anchor_id: Option<u64>, rows: &[TorrentRow]) -> Option<usize> {
    let id = anchor_id?;
    rows.iter().position(|r| r.id == id)
}

/// The row a shift-click at `row_ix` extends from: the anchor row when its
/// torrent is still visible, otherwise the selected row nearest the click
/// (ties broken toward the top), otherwise the clicked row itself. The
/// fallback keeps shift-click extending from the existing selection even
/// when no anchor was recorded (e.g. rows selected via checkboxes).
fn shift_range_start(
    anchor: Option<usize>,
    selected_rows: &HashSet<usize>,
    row_ix: usize,
) -> usize {
    anchor
        .or_else(|| {
            selected_rows
                .iter()
                .copied()
                .min_by_key(|&ix| (ix.abs_diff(row_ix), ix))
        })
        .unwrap_or(row_ix)
}

/// Torrent ids a context menu opened at `row_ix` should target: the whole
/// current selection when the right-clicked row is part of a multi-row
/// selection, otherwise just the clicked row. Sorted and deduplicated.
fn context_menu_target_ids(
    selected_rows: &HashSet<usize>,
    row_ix: usize,
    rows: &[TorrentRow],
) -> Vec<usize> {
    let keep_selection = selected_rows.contains(&row_ix) && selected_rows.len() > 1;
    let mut ids: Vec<usize> = if keep_selection {
        selected_rows
            .iter()
            .filter_map(|&ix| rows.get(ix).map(|r| r.id as usize))
            .collect()
    } else {
        rows.get(row_ix)
            .map(|r| r.id as usize)
            .into_iter()
            .collect()
    };
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Table delegate that holds torrent row data.
pub struct TorrentTableDelegate {
    /// Full, unfiltered set of rows as last fetched from the client. The
    /// visible `rows` are derived from this by
    /// [`TorrentTableDelegate::apply_filter`].
    pub all_rows: Vec<TorrentRow>,
    pub rows: Vec<TorrentRow>,
    columns: Vec<Column>,
    /// Currently active search/status filter, pushed in by the panel via
    /// [`TorrentTableDelegate::set_filter`].
    filter: TorrentFilter,
    /// Multi-selection state: set of selected row indices.
    pub selected_rows: HashSet<usize>,
    /// The torrent id anchoring shift-click range selection. Tracked by id,
    /// not row index, so the anchor survives data refreshes and re-sorting
    /// (which shift row indices); resolved to a row lazily by
    /// [`TorrentTableDelegate::anchor_row_ix`].
    anchor_id: Option<u64>,
    /// Currently active sort column index (None = no active sort).
    sort_col_ix: Option<usize>,
    /// Currently active sort direction.
    sort_dir: ColumnSort,
    /// Folder keys whose groups the user collapsed. Groups are expanded by
    /// default, so this starts (and mostly stays) empty. Keyed by folder, so
    /// collapse state survives refreshes and parent promotion alike.
    collapsed_folders: HashSet<String>,
    /// Normalized key of the session's default download folder, refreshed by
    /// the panel alongside the torrent list. Torrents sitting there are never
    /// grouped (see [`derive_display_rows`]); `None` = unknown.
    default_folder_key: Option<String>,
    /// Weak handle to the parent [`MainPanel`] so the context menu can open dialogs.
    main_panel: WeakEntity<MainPanel>,
    /// Data source and host services for the context-menu actions.
    client: Arc<dyn TorrentClient>,
    host: Arc<dyn UiHost>,
}

impl TorrentTableDelegate {
    pub fn new(
        main_panel: WeakEntity<MainPanel>,
        client: Arc<dyn TorrentClient>,
        host: Arc<dyn UiHost>,
    ) -> Self {
        Self {
            all_rows: Vec::new(),
            rows: Vec::new(),
            columns: vec![
                Column::new("select", "").width(40.),
                Column::new("name", "Name").width(300.).sortable(),
                Column::new("state", "Status").width(100.).sortable(),
                Column::new("progress", "Progress").width(140.).sortable(),
                Column::new("down", "↓ Speed").width(120.).sortable(),
                Column::new("downloaded", "Downloaded")
                    .width(110.)
                    .sortable(),
                Column::new("up", "↑ Speed").width(120.).sortable(),
                Column::new("uploaded", "Uploaded").width(100.).sortable(),
                Column::new("eta", "ETA").width(100.).sortable(),
                Column::new("peers", "Peers").width(90.).sortable(),
            ],
            filter: TorrentFilter::default(),
            selected_rows: HashSet::new(),
            anchor_id: None,
            sort_col_ix: None,
            sort_dir: ColumnSort::Default,
            collapsed_folders: HashSet::new(),
            default_folder_key: None,
            main_panel,
            client,
            host,
        }
    }

    /// Store the given search text and status filter, then re-derive the
    /// visible rows. Called whenever the search box or status dropdown
    /// changes.
    pub fn set_filter(&mut self, search: String, status: String) {
        self.filter = TorrentFilter { search, status };
        self.apply_filter();
    }

    /// Record the session's default download folder (normalized), keeping
    /// its torrents out of destination-folder grouping. Refreshed by the
    /// panel alongside the torrent list.
    pub fn set_default_folder(&mut self, folder: Option<&str>) {
        let key = folder.map(normalize_folder);
        if key != self.default_folder_key {
            self.default_folder_key = key;
            self.apply_filter();
        }
    }

    /// Whether a search text or status filter is currently constraining the
    /// visible rows. While true, matching groups are force-expanded.
    fn filter_active(&self) -> bool {
        !self.filter.search.trim().is_empty() || !self.filter.status.is_empty()
    }

    /// Row index of the shift-click anchor, when its torrent is still among
    /// the visible rows (a filtered-out or deleted anchor resolves to
    /// `None`, and shift-click falls back to the nearest selected row).
    fn anchor_row_ix(&self) -> Option<usize> {
        resolve_anchor_row(self.anchor_id, &self.rows)
    }

    /// Recompute the visible `rows` from `all_rows`: apply the stored filter
    /// (search text matches the name as a case-insensitive substring, status
    /// must match the torrent's `state` exactly — a folder group survives
    /// when ANY member matches), group same-folder torrents into
    /// parent/child rows, and order everything per the active sort. Called
    /// after every data refresh and by
    /// [`TorrentTableDelegate::set_filter`], so the view stays correct even
    /// as torrent statuses update live.
    pub fn apply_filter(&mut self) {
        let sort_key = self
            .sort_col_ix
            .and_then(|ix| self.columns.get(ix))
            .map(|c| c.key.to_string());
        self.rows = derive_display_rows(
            &self.all_rows,
            &self.filter,
            sort_key.as_deref(),
            self.sort_dir,
            &self.collapsed_folders,
            self.default_folder_key.as_deref(),
        );
        // Row indices shifted, so any index-based selection is now stale.
        // The id-based shift-click anchor intentionally survives: it resolves
        // lazily against the new rows and falls back when its torrent is gone.
        self.selected_rows.clear();
        // Drop collapse state for folders that no longer have any members.
        let live: HashSet<&str> = self
            .all_rows
            .iter()
            .filter(|r| !r.folder_key.is_empty())
            .map(|r| r.folder_key.as_str())
            .collect();
        self.collapsed_folders.retain(|k| live.contains(k.as_str()));
    }

    /// Toggle a folder group between collapsed and expanded, preserving the
    /// current selection by torrent id across the row-index shift (same
    /// pattern as `MainPanel::install_rows`).
    pub fn toggle_folder(&mut self, folder_key: &str) {
        if folder_key.is_empty() {
            return;
        }
        if !self.collapsed_folders.remove(folder_key) {
            self.collapsed_folders.insert(folder_key.to_string());
        }
        let prev: HashSet<u64> = self
            .selected_rows
            .iter()
            .filter_map(|&ix| self.rows.get(ix).map(|r| r.id))
            .collect();
        self.apply_filter();
        self.selected_rows = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| prev.contains(&r.id))
            .map(|(ix, _)| ix)
            .collect();
    }
}

impl TableDelegate for TorrentTableDelegate {
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
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let row = &self.rows[row_ix];
        let col = &self.columns[col_ix];

        match col.key.as_ref() {
            "select" => {
                let checked = self.selected_rows.contains(&row_ix);
                let table_state = cx.entity().clone();
                // Fill the cell so the checkbox centers in both axes, matching
                // the header checkbox (a block child would otherwise only
                // stretch horizontally and stay top-aligned).
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    // Prevent the row's mouse-down handler from hijacking the toggle.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Checkbox::new(("select-cb", row_ix))
                            .checked(checked)
                            .on_click(move |new_checked, _, cx| {
                                table_state.update(cx, |state, cx| {
                                    let id = state.delegate().rows.get(row_ix).map(|r| r.id);
                                    let delegate = state.delegate_mut();
                                    if *new_checked {
                                        delegate.selected_rows.insert(row_ix);
                                    } else {
                                        delegate.selected_rows.remove(&row_ix);
                                    }
                                    // The toggled row becomes the shift-click
                                    // anchor, matching a plain row click.
                                    if let Some(id) = id {
                                        delegate.anchor_id = Some(id);
                                    }
                                    cx.notify();
                                });
                            }),
                    )
            }
            "name" => {
                let theme = cx.theme();
                let is_parent = row.is_parent();
                let mut cell = h_flex().items_center().gap_1();
                // Fixed-width chevron column: parents get the expand/collapse
                // toggle, children the indent, standalones a zero-width
                // spacer — all names stay vertically aligned.
                if is_parent {
                    let key = row.folder_key.clone();
                    let expanded = self.filter_active() || !self.collapsed_folders.contains(&key);
                    let table_state = cx.entity().clone();
                    cell = cell.child(
                        div()
                            .size(px(20.))
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .child(
                                Icon::new(if expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .small(),
                            )
                            // Toggle on press. Stopping propagation keeps both
                            // the row's multi-select handler and the
                            // framework's row click (selection -> detail
                            // panel) from firing — same mechanism as the
                            // select checkbox.
                            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                                cx.stop_propagation();
                                table_state.update(cx, |state, cx| {
                                    state.delegate_mut().toggle_folder(&key);
                                    cx.notify();
                                });
                            }),
                    );
                } else {
                    let indent = if row.depth == 1 { px(20.) } else { px(0.) };
                    cell = cell.child(div().flex_none().w(indent));
                }
                cell = cell.child(div().child(row.name.clone()));
                // Muted child-count badge next to the parent's name. The
                // parent keeps showing its own stats (no aggregation).
                if is_parent {
                    cell = cell.child(
                        div()
                            .ml_1()
                            .px_1()
                            .rounded_sm()
                            .bg(theme.muted)
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("+{}", row.group_size - 1)),
                    );
                }
                cell
            }
            "state" => {
                // Status icon tinted by tone: green arrows while live (down
                // while downloading, up when seeding after completion), a
                // dimmed pause icon when paused (matching the row's dimmed
                // text), and red on error.
                let theme = cx.theme();
                let icon = match row.tone {
                    RowTone::Downloading => {
                        Some(Icon::new(IconName::ArrowDown).text_color(theme.blue_light))
                    }
                    RowTone::Seeding => {
                        Some(Icon::new(IconName::ArrowUp).text_color(theme.success))
                    }
                    RowTone::Paused => {
                        Some(Icon::new(IconName::Pause).text_color(theme.muted_foreground))
                    }
                    RowTone::Error => Some(Icon::new(IconName::CircleX).text_color(theme.danger)),
                    RowTone::Normal => None,
                };
                h_flex()
                    .gap_1()
                    .when_some(icon, |this, icon| this.child(icon.small()))
                    .child(div().child(row.state.clone()))
            }
            "progress" => {
                // Green while downloading, red on error; other tones keep the
                // theme's default fill.
                let bar = match row.tone {
                    RowTone::Error => Progress::new(("progress-bar", row_ix))
                        .value(row.progress_raw as f32)
                        .color(cx.theme().danger),
                    RowTone::Downloading => Progress::new(("progress-bar", row_ix))
                        .value(row.progress_raw as f32)
                        .color(cx.theme().blue_light),
                    _ => Progress::new(("progress-bar", row_ix)).value(row.progress_raw as f32),
                };
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(div().child(bar).min_w(px(70.)))
                    .child(div().child(row.progress.clone()))
            }
            "peers" => div().child(row.peers.clone()),
            "down" => div().child(row.down_speed.clone()),
            "downloaded" => div().child(row.downloaded.clone()),
            "up" => div().child(row.up_speed.clone()),
            "uploaded" => div().child(row.uploaded.clone()),
            "eta" => div().child(row.eta.clone()),
            _ => div(),
        }
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let col = &self.columns[col_ix];
        if col.key.as_ref() == "select" {
            let all_selected = !self.rows.is_empty() && self.selected_rows.len() == self.rows.len();
            let total = self.rows.len();
            let table_state = cx.entity().clone();
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Checkbox::new("select-all-cb")
                        .checked(all_selected)
                        .on_click(move |new_checked, _, cx| {
                            table_state.update(cx, |state, cx| {
                                if *new_checked {
                                    state.delegate_mut().selected_rows = (0..total).collect();
                                } else {
                                    state.delegate_mut().selected_rows.clear();
                                }
                                cx.notify();
                            });
                        }),
                )
                .into_any_element();
        }
        div()
            .size_full()
            .child(self.column(col_ix, cx).name.clone())
            .into_any_element()
    }

    /// Sort the torrent rows by the clicked column.
    ///
    /// Triggered by the table's built-in sort icon (rendered for any column that
    /// is `.sortable()`). `sort` is the new direction the table is cycling to:
    /// `Ascending` -> `Default` (unsorted, restored to API id order) ->
    /// `Descending`.
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
        // Re-derives `rows` (filtering, grouping, and ordering); it also
        // clears the index-based selection, which just went stale.
        self.apply_filter();
        cx.notify();
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let is_selected = self.selected_rows.contains(&row_ix);
        // Paused torrents render with dimmed text; the color inherits into
        // every cell of the row. Hoisted out of the style chain so the
        // closure doesn't need to borrow `cx`.
        let is_paused = self
            .rows
            .get(row_ix)
            .is_some_and(|r| r.tone == RowTone::Paused);
        let dimmed_text = cx.theme().muted_foreground;
        // Subtle left guide on grouped child rows; hoisted like the text
        // color so the style closure doesn't need to borrow `cx`.
        let child_guide = cx.theme().border;
        let is_child = self.rows.get(row_ix).is_some_and(|r| r.depth == 1);
        div()
            .id(("torrent-row", row_ix))
            .when(is_paused, |this| this.text_color(dimmed_text))
            .when(is_child, |this| this.border_l_1().border_color(child_guide))
            .when(is_selected, |this| this.bg(gpui::rgb(0x2a2a3e)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |state, e: &MouseDownEvent, _, cx| {
                    let delegate = state.delegate_mut();
                    let clicked_id = delegate.rows.get(row_ix).map(|r| r.id);
                    if e.modifiers.shift {
                        // Extend from the anchor row — or, when its torrent is
                        // no longer visible, from the selected row nearest the
                        // click — replacing the selection with the range.
                        let start = shift_range_start(
                            delegate.anchor_row_ix(),
                            &delegate.selected_rows,
                            row_ix,
                        );
                        let (lo, hi) = if start <= row_ix {
                            (start, row_ix)
                        } else {
                            (row_ix, start)
                        };
                        delegate.selected_rows.clear();
                        for i in lo..=hi {
                            delegate.selected_rows.insert(i);
                        }
                    } else if e.modifiers.control {
                        if delegate.selected_rows.contains(&row_ix) {
                            delegate.selected_rows.remove(&row_ix);
                        } else {
                            delegate.selected_rows.insert(row_ix);
                        }
                        if let Some(id) = clicked_id {
                            delegate.anchor_id = Some(id);
                        }
                    } else {
                        delegate.selected_rows.clear();
                        delegate.selected_rows.insert(row_ix);
                        if let Some(id) = clicked_id {
                            delegate.anchor_id = Some(id);
                        }
                    }
                    cx.notify();
                }),
            )
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: gpui_kit::component::menu::PopupMenu,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> gpui_kit::component::menu::PopupMenu {
        let Some(row) = self.rows.get(row_ix) else {
            return menu;
        };
        let torrent_id = row.id;
        let torrent_name = row.name.clone();

        // Right-clicking a row inside an existing multi-row selection keeps
        // that selection so the actions below target all of it; right-clicking
        // any other row re-targets the selection to just the clicked row.
        let target_ids = context_menu_target_ids(&self.selected_rows, row_ix, &self.rows);
        if target_ids.len() <= 1 {
            self.selected_rows.clear();
            self.selected_rows.insert(row_ix);
        }
        self.anchor_id = Some(torrent_id);

        let client = self.client.clone();
        let host = self.host.clone();
        let count = target_ids.len();

        let mut m = if count > 1 {
            menu.item(torrent_bulk_action_menu_item(
                format!("Pause ({count} torrents)"),
                target_ids.clone(),
                rqbit_protocol::TorrentAction::Pause,
                client.clone(),
            ))
            .item(torrent_bulk_action_menu_item(
                format!("Start ({count} torrents)"),
                target_ids.clone(),
                rqbit_protocol::TorrentAction::Start,
                client.clone(),
            ))
            .separator()
        } else {
            menu.item(torrent_action_menu_item(
                format!("Pause: {torrent_name}"),
                torrent_id as usize,
                rqbit_protocol::TorrentAction::Pause,
                client.clone(),
            ))
            .item(torrent_action_menu_item(
                format!("Start: {torrent_name}"),
                torrent_id as usize,
                rqbit_protocol::TorrentAction::Start,
                client.clone(),
            ))
            .separator()
        };

        if self.host.capabilities().open_folder {
            // Resolve the output folder lazily (async round-trip; may be a
            // network call on remote clients), then hand it to the host.
            // Single-target by nature: always the right-clicked torrent, even
            // when the rest of the menu targets a multi-row selection.
            m = m
                .item(
                    PopupMenuItem::new("Open Folder").on_click(move |_, _, cx: &mut App| {
                        let client = client.clone();
                        let host = host.clone();
                        cx.spawn(async move |cx| {
                            let Ok(details) = client
                                .details(&rqbit_protocol::TorrentId::Id(torrent_id))
                                .await
                            else {
                                return;
                            };
                            let path = details.output_folder;
                            if path.is_empty() {
                                return;
                            }
                            cx.update(|_cx| host.open_folder(&path));
                        })
                        .detach();
                    }),
                )
                .separator();
        }

        // Parent rows offer deleting the whole folder group at once. Ids are
        // collected from `all_rows` so collapsed (hidden) members are
        // included; the existing bulk delete dialog handles the rest. Only
        // offered without a multi-row selection — then the selection-wide
        // delete below is the operative action.
        if row.is_parent() && count <= 1 {
            let ids: Vec<usize> = self
                .all_rows
                .iter()
                .filter(|r| r.folder_key == row.folder_key)
                .map(|r| r.id as usize)
                .collect();
            let count = ids.len();
            m = m
                .item(
                    PopupMenuItem::new(format!("Delete Group ({count})…")).on_click({
                        let main_panel = self.main_panel.clone();
                        move |_, window, cx: &mut App| {
                            if let Some(main) = main_panel.upgrade() {
                                main.update(cx, |this, cx| {
                                    this.open_delete_dialog(ids.clone(), window, cx);
                                });
                            }
                        }
                    }),
                )
                .separator();
        }

        if count > 1 {
            m.item(
                PopupMenuItem::new(format!("Delete ({count} torrents)…")).on_click({
                    let main_panel = self.main_panel.clone();
                    move |_, window, cx: &mut App| {
                        if let Some(main) = main_panel.upgrade() {
                            main.update(cx, |this, cx| {
                                this.open_delete_dialog(target_ids.clone(), window, cx);
                            });
                        }
                    }
                }),
            )
        } else {
            m.item(
                PopupMenuItem::new(format!("Delete: {torrent_name}")).on_click({
                    let main_panel = self.main_panel.clone();
                    move |_, window, cx: &mut App| {
                        if let Some(main) = main_panel.upgrade() {
                            main.update(cx, |this, cx| {
                                this.open_delete_dialog(vec![torrent_id as usize], window, cx);
                            });
                        }
                    }
                }),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    // NOTE: no `use super::*` here — it would pull gpui's own `#[test]`
    // attribute macro (re-exported via `use gpui::*` above) into scope and
    // shadow the builtin `#[test]`.
    use super::{
        RowTone, TorrentFilter, TorrentRow, context_menu_target_ids, derive_display_rows,
        normalize_folder, resolve_anchor_row, shift_range_start,
    };
    use gpui_kit::component::table::ColumnSort;
    use std::collections::HashSet;

    fn row(id: u64, name: &str, folder: &str) -> TorrentRow {
        row_with_state(id, name, folder, "live")
    }

    fn row_with_state(id: u64, name: &str, folder: &str, state: &str) -> TorrentRow {
        TorrentRow {
            id,
            name: name.to_string(),
            state: state.to_string(),
            progress: "0%".to_string(),
            progress_raw: 0.0,
            peers: "0/0".to_string(),
            tone: RowTone::Normal,
            peers_raw: 0,
            down_speed: "0".to_string(),
            down_speed_raw: 0.0,
            up_speed: "0".to_string(),
            up_speed_raw: 0.0,
            downloaded: "0 B".to_string(),
            downloaded_raw: 0,
            uploaded: "0 B".to_string(),
            uploaded_raw: 0,
            eta: "—".to_string(),
            eta_raw: None,
            output_folder: folder.to_string(),
            folder_key: normalize_folder(folder),
            depth: 0,
            group_size: 1,
        }
    }

    fn no_filter() -> TorrentFilter {
        TorrentFilter {
            search: String::new(),
            status: String::new(),
        }
    }

    /// (id, depth, group_size) summary — enough to assert row structure
    /// without needing `PartialEq` on `TorrentRow`.
    fn derive(
        all: &[TorrentRow],
        filter: &TorrentFilter,
        sort_key: Option<&str>,
        dir: ColumnSort,
        collapsed: &[&str],
    ) -> Vec<(u64, u8, usize)> {
        derive_with_default(all, filter, sort_key, dir, collapsed, None)
    }

    fn derive_with_default(
        all: &[TorrentRow],
        filter: &TorrentFilter,
        sort_key: Option<&str>,
        dir: ColumnSort,
        collapsed: &[&str],
        default_folder: Option<&str>,
    ) -> Vec<(u64, u8, usize)> {
        let collapsed: HashSet<String> = collapsed.iter().map(|s| s.to_string()).collect();
        derive_display_rows(all, filter, sort_key, dir, &collapsed, default_folder)
            .iter()
            .map(|r| (r.id, r.depth, r.group_size))
            .collect()
    }

    #[test]
    fn groups_by_folder_lowest_id_is_parent() {
        // ids 3, 1 share "/x"; id 2 is standalone. The lowest id (added
        // first) parents the group, and the group's children always follow
        // their parent directly.
        let all = [row(3, "c", "/x"), row(1, "a", "/x"), row(2, "b", "")];
        assert_eq!(
            derive(&all, &no_filter(), None, ColumnSort::Default, &[]),
            vec![(1, 0, 2), (3, 1, 2), (2, 0, 1)]
        );
    }

    #[test]
    fn collapsed_group_shows_only_parent() {
        let all = [row(3, "c", "/x"), row(1, "a", "/x"), row(2, "b", "")];
        assert_eq!(
            derive(&all, &no_filter(), None, ColumnSort::Default, &["/x"]),
            vec![(1, 0, 2), (2, 0, 1)]
        );
    }

    #[test]
    fn filter_matching_child_shows_whole_group() {
        // Search hits only the child's name: the whole group stays visible
        // (force-expanded, even though the group was collapsed) while the
        // non-matching standalone is hidden.
        let all = [
            row(3, "child-name", "/x"),
            row(1, "parent", "/x"),
            row(2, "b", ""),
        ];
        let filter = TorrentFilter {
            search: "child".into(),
            status: String::new(),
        };
        assert_eq!(
            derive(&all, &filter, None, ColumnSort::Default, &["/x"]),
            vec![(1, 0, 2), (3, 1, 2)]
        );
    }

    #[test]
    fn filter_status_uses_any_member() {
        // Only one member is "paused": the whole group survives the status
        // filter; the live standalone does not.
        let all = [
            row_with_state(3, "c", "/x", "paused"),
            row(1, "a", "/x"),
            row(2, "b", ""),
        ];
        let filter = TorrentFilter {
            search: String::new(),
            status: "paused".into(),
        };
        assert_eq!(
            derive(&all, &filter, None, ColumnSort::Default, &[]),
            vec![(1, 0, 2), (3, 1, 2)]
        );
    }

    #[test]
    fn parent_promotion_on_delete() {
        // After the parent (id 1) is deleted, the next-oldest member (id 3)
        // becomes the parent of the remaining child (id 5).
        let all = [row(5, "e", "/x"), row(3, "c", "/x")];
        assert_eq!(
            derive(&all, &no_filter(), None, ColumnSort::Default, &[]),
            vec![(3, 0, 2), (5, 1, 2)]
        );
    }

    #[test]
    fn group_shrunk_to_one_renders_standalone() {
        let all = [row(3, "c", "/x")];
        assert_eq!(
            derive(&all, &no_filter(), None, ColumnSort::Default, &[]),
            vec![(3, 0, 1)]
        );
    }

    #[test]
    fn empty_folder_never_groups() {
        let all = [row(2, "b", ""), row(1, "a", "")];
        assert_eq!(
            derive(&all, &no_filter(), None, ColumnSort::Default, &[]),
            vec![(1, 0, 1), (2, 0, 1)]
        );
    }

    #[test]
    fn trailing_separator_normalized() {
        let all = [row(2, "b", "/a/"), row(1, "a", "/a")];
        assert_eq!(
            derive(&all, &no_filter(), None, ColumnSort::Default, &[]),
            vec![(1, 0, 2), (2, 1, 2)]
        );
    }

    #[test]
    fn default_sort_is_id_order_regardless_of_input_order() {
        let all = [row(3, "c", "/y"), row(1, "a", ""), row(2, "b", "")];
        assert_eq!(
            derive(&all, &no_filter(), None, ColumnSort::Default, &[]),
            vec![(1, 0, 1), (2, 0, 1), (3, 0, 1)]
        );
    }

    #[test]
    fn name_sort_orders_units_by_parent_and_children_by_name() {
        // Group "/x": parent id 1 "zeta", children id 3 "gamma", id 4
        // "alpha"; standalone id 2 "mid". Ascending name: units by parent
        // name ("mid" < "zeta"), children by their names.
        let all = [
            row(4, "alpha", "/x"),
            row(2, "mid", ""),
            row(3, "gamma", "/x"),
            row(1, "zeta", "/x"),
        ];
        assert_eq!(
            derive(&all, &no_filter(), Some("name"), ColumnSort::Ascending, &[]),
            vec![(2, 0, 1), (1, 0, 3), (4, 1, 3), (3, 1, 3)]
        );
    }

    #[test]
    fn descending_reverses_units_and_children() {
        let all = [
            row(4, "alpha", "/x"),
            row(2, "mid", ""),
            row(3, "gamma", "/x"),
            row(1, "zeta", "/x"),
        ];
        assert_eq!(
            derive(
                &all,
                &no_filter(),
                Some("name"),
                ColumnSort::Descending,
                &[]
            ),
            vec![(1, 0, 3), (3, 1, 3), (4, 1, 3), (2, 0, 1)]
        );
    }

    #[test]
    fn equal_keys_tiebreak_by_id() {
        // Identical names: the ascending-id tiebreak keeps a stable order in
        // BOTH directions (the parent stays first regardless).
        let all = [
            row(3, "same", "/x"),
            row(1, "same", "/x"),
            row(2, "same", ""),
        ];
        for dir in [ColumnSort::Ascending, ColumnSort::Descending] {
            assert_eq!(
                derive(&all, &no_filter(), Some("name"), dir, &[]),
                vec![(1, 0, 2), (3, 1, 2), (2, 0, 1)]
            );
        }
    }

    #[test]
    fn normalizes_trailing_separators() {
        assert_eq!(normalize_folder("/a/"), "/a");
        assert_eq!(normalize_folder("/a\\"), "/a");
        assert_eq!(normalize_folder("/a//"), "/a");
        assert_eq!(normalize_folder(""), "");
    }

    #[test]
    fn default_folder_torrents_never_group() {
        // Every add lands in the session's default download folder unless
        // pointed elsewhere: those torrents must stay regular rows, while a
        // genuinely shared non-default folder still groups.
        let all = [
            row(1, "a", "/tmp"),
            row(2, "b", "/tmp"),
            row(3, "c", "/data"),
            row(4, "d", "/data"),
        ];
        assert_eq!(
            derive_with_default(
                &all,
                &no_filter(),
                None,
                ColumnSort::Default,
                &[],
                Some("/tmp")
            ),
            vec![(1, 0, 1), (2, 0, 1), (3, 0, 2), (4, 1, 2)]
        );
    }

    #[test]
    fn default_folder_is_normalized_before_comparing() {
        let all = [row(2, "b", "/tmp/"), row(1, "a", "/tmp")];
        assert_eq!(
            derive_with_default(
                &all,
                &no_filter(),
                None,
                ColumnSort::Default,
                &[],
                Some("/tmp/")
            ),
            vec![(1, 0, 1), (2, 0, 1)]
        );
    }

    #[test]
    fn context_menu_targets_whole_selection_from_inside() {
        // Rows 0 and 2 are selected; right-clicking row 2 targets both
        // selected torrents, sorted by id.
        let all = [row(1, "a", ""), row(2, "b", ""), row(3, "c", "")];
        let selected: HashSet<usize> = [0, 2].into_iter().collect();
        assert_eq!(context_menu_target_ids(&selected, 2, &all), vec![1, 3]);
    }

    #[test]
    fn context_menu_retargets_when_click_outside_selection() {
        let all = [row(1, "a", ""), row(2, "b", ""), row(3, "c", "")];
        let selected: HashSet<usize> = [0].into_iter().collect();
        assert_eq!(context_menu_target_ids(&selected, 2, &all), vec![3]);
    }

    #[test]
    fn context_menu_single_selected_row_targets_itself() {
        let all = [row(1, "a", ""), row(2, "b", "")];
        let selected: HashSet<usize> = [1].into_iter().collect();
        assert_eq!(context_menu_target_ids(&selected, 1, &all), vec![2]);
    }

    #[test]
    fn anchor_resolves_by_torrent_id() {
        let all = [row(1, "a", ""), row(2, "b", "")];
        assert_eq!(resolve_anchor_row(Some(2), &all), Some(1));
        // Deleted or never-set anchors resolve to None.
        assert_eq!(resolve_anchor_row(Some(42), &all), None);
        assert_eq!(resolve_anchor_row(None, &all), None);
    }

    #[test]
    fn shift_start_falls_back_to_nearest_selected_row() {
        let selected: HashSet<usize> = [5, 9].into_iter().collect();
        // Without an anchor the nearest selected row wins; equidistant ties
        // break toward the smaller index.
        assert_eq!(shift_range_start(None, &selected, 7), 5);
        assert_eq!(shift_range_start(None, &selected, 8), 9);
        // An anchor beats the fallback; nothing selected and no anchor
        // collapses to the clicked row.
        assert_eq!(shift_range_start(Some(2), &selected, 8), 2);
        assert_eq!(shift_range_start(None, &HashSet::new(), 4), 4);
    }
}
