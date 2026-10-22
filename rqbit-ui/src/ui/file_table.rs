use gpui::*;
use gpui_kit::component::{
    ActiveTheme as _,
    checkbox::Checkbox,
    h_flex,
    progress::Progress,
    table::{Column, ColumnSort, TableDelegate, TableState},
};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use rqbit_protocol::{CustomColumnKind, CustomColumnSpec, CustomColumnsState};

use crate::ui::utils::render_custom_cell;

/// A single row in a file-selection table.
#[derive(Clone)]
pub struct FileRow {
    pub file_index: usize,
    pub name: String,
    pub length: u64,
    pub included: bool,
    /// Whether the file already exists on disk at the destination folder.
    pub exists: bool,
    /// Bytes downloaded for this file so far (0 when unknown/not started).
    pub progress: u64,
    /// Hook-provided custom column values, keyed by column key. Empty when
    /// no hook provides data for this file.
    pub custom: BTreeMap<String, String>,
}

/// Callback invoked when a single file's inclusion is toggled.
pub type OnFileToggle = Arc<dyn Fn(usize, bool, &mut App) + Send + Sync>;
/// Callback invoked when the "select all" checkbox is toggled.
pub type OnToggleAll = Arc<dyn Fn(bool, &mut App) + Send + Sync>;

/// Reusable table delegate for selecting which files of a torrent to download.
///
/// It is self-contained: toggling a checkbox mutates its own rows. An optional
/// `on_toggle` / `on_toggle_all` callback can be supplied for side-effects
/// (e.g. persisting the selection to the API). The owning [`TableState`] must be
/// registered via [`FileTableDelegate::set_table`] so the checkboxes can mutate
/// the rows.
pub struct FileTableDelegate {
    pub rows: Vec<FileRow>,
    columns: Vec<Column>,
    table: WeakEntity<TableState<FileTableDelegate>>,
    pub on_toggle: Option<OnFileToggle>,
    pub on_toggle_all: Option<OnToggleAll>,
    /// Whether to show the per-file Progress column. Only enabled in the
    /// torrent detail panel (where live stats are available); the add-torrent
    /// dialog leaves this off.
    show_progress: bool,
    /// Hook-provided dynamic columns (only those with `files: true`),
    /// appended after the static ones. Empty when no hook provides columns
    /// or the providing hook is unreachable.
    custom_specs: Vec<CustomColumnSpec>,
    /// Render kind per dynamic column key (mirrors `custom_specs`).
    custom_kinds: HashMap<String, CustomColumnKind>,
    /// Currently active sort column index (None = no active sort).
    sort_col_ix: Option<usize>,
    /// Currently active sort direction.
    sort_dir: ColumnSort,
}

impl Default for FileTableDelegate {
    fn default() -> Self {
        Self::new()
    }
}

impl FileTableDelegate {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            table: WeakEntity::new_invalid(),
            on_toggle: None,
            on_toggle_all: None,
            show_progress: false,
            custom_specs: Vec::new(),
            custom_kinds: HashMap::new(),
            columns: Self::base_columns(false),
            // Default sort by file name (ascending).
            sort_col_ix: Some(1),
            sort_dir: ColumnSort::Ascending,
        }
    }

    /// The static columns (plus Progress when enabled), before any
    /// hook-provided dynamic ones.
    fn base_columns(show_progress: bool) -> Vec<Column> {
        let mut columns = vec![
            Column::new("included", "Included")
                .width(35.)
                .text_center()
                .selectable(false),
            Column::new("name", "File Name").width(300.).sortable(),
            Column::new("size", "Size").width(100.).sortable(),
        ];
        if show_progress {
            columns.push(Column::new("progress", "Progress").width(140.).sortable());
        }
        columns
    }

    /// Rebuild `columns` from the base plus dynamic specs, re-mirroring the
    /// active sort so the header arrow survives a `refresh()`.
    fn rebuild_columns(&mut self) {
        let mut columns = Self::base_columns(self.show_progress);
        for spec in &self.custom_specs {
            let mut col = Column::new(spec.key.clone(), spec.name.clone()).sortable();
            if let Some(width) = spec.width {
                col = col.width(width);
            }
            columns.push(col);
        }
        for (ix, col) in columns.iter_mut().enumerate() {
            col.sort = if Some(ix) == self.sort_col_ix {
                Some(self.sort_dir)
            } else {
                col.sort.map(|_| ColumnSort::Default)
            };
        }
        self.columns = columns;
    }

    /// Enable the per-file Progress column. Call before the table is first
    /// rendered so the column list is stable.
    pub fn show_progress(&mut self) {
        if self.show_progress {
            return;
        }
        self.show_progress = true;
        self.rebuild_columns();
    }

    /// Replace the hook-provided dynamic columns (only those carrying
    /// per-file values). `None` or a disconnected state drops them all.
    /// Returns whether the visible column set changed; the caller must then
    /// `refresh()` the table state.
    pub fn set_custom_columns(&mut self, state: Option<&CustomColumnsState>) -> bool {
        let specs: Vec<CustomColumnSpec> = match state {
            Some(s) if s.connected => s.columns.iter().filter(|c| c.files).cloned().collect(),
            _ => Vec::new(),
        };
        if specs == self.custom_specs {
            return false;
        }
        self.custom_specs = specs;
        self.custom_kinds = self
            .custom_specs
            .iter()
            .map(|s| (s.key.clone(), s.kind))
            .collect();
        if self.sort_col_ix.is_some_and(|ix| ix >= self.columns.len()) {
            self.sort_col_ix = None;
            self.sort_dir = ColumnSort::Default;
        }
        self.rebuild_columns();
        true
    }

    /// Persist user-resized widths back into the column defs so they survive
    /// a `refresh()`.
    pub fn set_column_widths(&mut self, widths: &[Pixels]) {
        for (col, width) in self.columns.iter_mut().zip(widths) {
            col.width = *width;
        }
    }

    /// Apply the currently persisted sort (`sort_col_ix` / `sort_dir`) to
    /// `self.rows`. No-op when no sort is active. Called after each data
    /// refresh so the sort survives live updates.
    pub(crate) fn apply_sort(&mut self) {
        let Some(col_ix) = self.sort_col_ix else {
            return;
        };
        let Some(col) = self.columns.get(col_ix) else {
            return;
        };
        let key = col.key.as_ref().to_string();

        let cmp = |a: &FileRow, b: &FileRow| -> std::cmp::Ordering {
            match key.as_str() {
                "name" => a.name.cmp(&b.name),
                "size" => a.length.cmp(&b.length),
                "progress" => a.progress.cmp(&b.progress),
                // Hook-provided dynamic keys (unknown keys compare Equal —
                // both lookups miss).
                k => a.custom.get(k).cmp(&b.custom.get(k)),
            }
        };

        match self.sort_dir {
            ColumnSort::Default => self.rows.sort_by_key(|r| r.name.clone()),
            ColumnSort::Ascending => self.rows.sort_by(cmp),
            ColumnSort::Descending => {
                self.rows.sort_by(cmp);
                self.rows.reverse();
            }
        }
    }

    /// Register the owning [`TableState`] so checkboxes can mutate the rows.
    pub fn set_table(&mut self, table: WeakEntity<TableState<FileTableDelegate>>) {
        self.table = table;
    }

    /// Currently selected file indices (those with `included == true`).
    pub fn selected_indices(&self) -> Vec<usize> {
        self.rows
            .iter()
            .filter(|r| r.included)
            .map(|r| r.file_index)
            .collect()
    }
}

impl TableDelegate for FileTableDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    /// Render a "select all" checkbox in the header of the Included column.
    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let col = &self.columns[col_ix];
        if col.key.as_ref() != "included" {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(col.name.clone())
                .into_any_element();
        }

        let all_included = !self.rows.is_empty() && self.rows.iter().all(|r| r.included);
        let new_state = !all_included;
        let table = self.table.clone();
        let on_toggle_all = self.on_toggle_all.clone();

        Checkbox::new("file-included-all")
            .checked(all_included)
            .on_click(move |_checked, _window, cx| {
                if let Some(state) = table.upgrade() {
                    state.update(cx, |s, cx| {
                        for r in s.delegate_mut().rows.iter_mut() {
                            r.included = new_state;
                        }
                        cx.notify();
                    });
                }
                if let Some(cb) = &on_toggle_all {
                    cb(new_state, cx);
                }
            })
            .into_any_element()
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
            "name" => div().child(row.name.clone()).into_any_element(),
            "size" => div()
                .child(crate::ui::utils::format_bytes(row.length))
                .into_any_element(),
            "progress" => {
                let pct = if row.length > 0 {
                    (row.progress as f64 / row.length as f64).clamp(0.0, 1.0) * 100.0
                } else {
                    0.0
                };
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .child(Progress::new(("file-progress-bar", row_ix)).value(pct as f32))
                            .min_w(px(70.)),
                    )
                    .child(div().child(format!("{:.1}%", pct)))
                    .into_any_element()
            }
            "included" => {
                let new_included = !row.included;
                let table = self.table.clone();
                let on_toggle = self.on_toggle.clone();
                Checkbox::new(("file-included", row_ix))
                    .checked(row.included)
                    .on_click(move |_checked, _window, cx| {
                        if let Some(state) = table.upgrade() {
                            state.update(cx, |s, cx| {
                                if let Some(r) = s.delegate_mut().rows.get_mut(row_ix) {
                                    r.included = new_included;
                                }
                                cx.notify();
                            });
                        }
                        if let Some(cb) = &on_toggle {
                            cb(row_ix, new_included, cx);
                        }
                    })
                    .into_any_element()
            }
            // Hook-provided dynamic columns: text or a tinted status badge.
            _ => match self.custom_kinds.get(col.key.as_ref()).copied() {
                Some(kind) => {
                    let value = row
                        .custom
                        .get(col.key.as_ref())
                        .map(String::as_str)
                        .unwrap_or("");
                    render_custom_cell(value, kind, cx.theme()).into_any_element()
                }
                None => div().into_any_element(),
            },
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
        // (which replace `self.rows` wholesale), and mirror it onto the
        // column definitions so the header arrow survives a `refresh()`.
        self.sort_col_ix = Some(col_ix);
        self.sort_dir = sort;
        for (ix, col) in self.columns.iter_mut().enumerate() {
            col.sort = if ix == col_ix {
                Some(sort)
            } else {
                col.sort.map(|_| ColumnSort::Default)
            };
        }
        self.apply_sort();
        cx.notify();
    }
}
