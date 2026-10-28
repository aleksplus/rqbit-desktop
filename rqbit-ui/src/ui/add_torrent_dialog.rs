use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_kit::component::{
    ActiveTheme as _,
    checkbox::Checkbox,
    table::{DataTable, TableState},
    v_flex,
};

use crate::client::UiAddSource;
use crate::ui::file_table::{FileRow, FileTableDelegate};

/// Events emitted by [`AddTorrentDialog`].
#[derive(Clone, Debug)]
pub enum AddTorrentDialogEvent {
    /// A single torrent should be added with the given selected file indices.
    /// `None` means "all files" (no selection restriction). The bool is whether
    /// to overwrite existing files on disk. The trailing string is the local
    /// source file the torrent came from, if any (used for move-after-add on
    /// the desktop).
    AddOne(UiAddSource, Option<Vec<u64>>, bool, Option<String>),
}

/// A single torrent queued for addition, with its (already resolved) file list.
pub struct AddTorrentEntry {
    pub source: UiAddSource,
    pub name: String,
    /// Info hash resolved via list-only add (empty string if unavailable).
    pub info_hash: String,
    /// Destination folder resolved via list-only add.
    pub output_folder: std::path::PathBuf,
    /// The local file this entry was built from, if any. After a successful
    /// add it is moved to the configured "move added torrents to" folder
    /// (desktop only).
    pub source_path: Option<String>,
    /// Resolved file rows. Empty means "add all files" (no selection UI).
    pub files: Vec<FileRow>,
    /// Names of hooks that pre-adjusted the file selection, shown as a hint.
    pub hook_note: Option<String>,
}

/// The body of a modal dialog that lets the user pick which files of one or
/// more torrents to download before actually adding them. The surrounding
/// modal (backdrop, title, footer buttons, close handling) is provided by the
/// gpui-kit `Dialog` component that hosts it (see
/// `MainPanel::create_add_dialog`).
pub struct AddTorrentDialog {
    entries: Vec<AddTorrentEntry>,
    current: usize,
    table_state: Entity<TableState<FileTableDelegate>>,
    /// Whether to overwrite existing files (required to resume/seed into them).
    overwrite: bool,
}

impl EventEmitter<AddTorrentDialogEvent> for AddTorrentDialog {}

impl AddTorrentDialog {
    pub fn new(entries: Vec<AddTorrentEntry>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let table_state = cx.new(|cx| {
            let mut delegate = FileTableDelegate::new();
            delegate.set_table(cx.entity().downgrade());
            TableState::new(delegate, window, cx).row_selectable(false)
        });

        let mut this = Self {
            entries,
            current: 0,
            table_state,
            overwrite: true,
        };
        this.load_current(cx);
        this
    }

    /// Whether any of the current torrent's files already exist on disk.
    fn current_has_existing(&self) -> bool {
        self.entries
            .get(self.current)
            .map(|e| e.files.iter().any(|f| f.exists))
            .unwrap_or(false)
    }

    /// Load the file rows for the current torrent into the table.
    fn load_current(&mut self, cx: &mut Context<Self>) {
        let files: Vec<FileRow> = self
            .entries
            .get(self.current)
            .map(|e| e.files.clone())
            .unwrap_or_default();
        self.table_state.update(cx, |state, cx| {
            state.delegate_mut().rows = files;
            state.delegate_mut().apply_sort();
            cx.notify();
        });
        cx.notify();
    }

    fn current_name(&self) -> String {
        self.entries
            .get(self.current)
            .map(|e| e.name.clone())
            .unwrap_or_default()
    }

    fn has_more(&self) -> bool {
        self.current + 1 < self.entries.len()
    }

    /// Dialog title text ("Add Torrent" or "Add Torrent (i/n)").
    pub fn title(&self) -> String {
        if self.entries.len() > 1 {
            format!("Add Torrent ({}/{})", self.current + 1, self.entries.len())
        } else {
            "Add Torrent".to_string()
        }
    }

    /// Confirm-button label ("Add & Next" while more entries are queued).
    pub fn ok_label(&self) -> &'static str {
        if self.has_more() {
            "Add & Next"
        } else {
            "Add Torrent"
        }
    }

    /// Add the current torrent with the selected files and advance.
    ///
    /// Emits [`AddTorrentDialogEvent::AddOne`] for the current entry. Returns
    /// `true` when the queue is drained and the hosting dialog should close,
    /// `false` when the next queued torrent has been loaded into the body.
    pub fn add_current(&mut self, cx: &mut Context<Self>) -> bool {
        if self.current >= self.entries.len() {
            return true;
        }

        // Take ownership of the current entry (UiAddSource is Clone, but we
        // remove it so the next entry shifts into `self.current`).
        let entry = self.entries.remove(self.current);
        let selection: Option<Vec<u64>> = if entry.files.is_empty() {
            None
        } else {
            Some(
                self.table_state
                    .read(cx)
                    .delegate()
                    .selected_indices()
                    .into_iter()
                    .map(|ix| ix as u64)
                    .collect(),
            )
        };
        cx.emit(AddTorrentDialogEvent::AddOne(
            entry.source,
            selection,
            self.overwrite,
            entry.source_path,
        ));

        if self.entries.is_empty() {
            true
        } else {
            // `remove` shifted the next entry into `self.current`.
            self.load_current(cx);
            false
        }
    }

    /// Toggle whether existing files should be overwritten when adding.
    fn on_toggle_overwrite(&mut self, cx: &mut Context<Self>) {
        self.overwrite = !self.overwrite;
        cx.notify();
    }
}

impl Render for AddTorrentDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let name = self.current_name();
        let has_files = self
            .entries
            .get(self.current)
            .map(|e| !e.files.is_empty())
            .unwrap_or(false);
        let has_existing = self.current_has_existing();
        let hook_note = self
            .entries
            .get(self.current)
            .and_then(|e| e.hook_note.clone());

        v_flex()
            .gap_3()
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.muted_foreground)
                    .child(name),
            )
            .when_some(hook_note, |this, note| {
                this.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "File selection adjusted by hook: {note}. You can still change it."
                        )),
                )
            })
            .when(has_existing, |this| {
                this.child(
                    div()
                        .p_2()
                        .rounded_md()
                        .border_1()
                        .border_color(gpui::rgb(0xb45309))
                        .bg(gpui::rgb(0x422006))
                        .text_color(gpui::rgb(0xfbbf24))
                        .text_size(px(12.))
                        .child(
                            "Some files already exist at the destination. They will be kept \
                             and only missing pieces downloaded. Enable \"Overwrite existing \
                             files\" below to replace them instead (also required to resume/seed).",
                        ),
                )
            })
            .child(if has_files {
                div()
                    .flex_1()
                    .min_h(px(200.))
                    .border_1()
                    .border_color(theme.border)
                    .rounded_md()
                    .overflow_hidden()
                    .child(DataTable::new(&self.table_state))
                    .into_any_element()
            } else {
                div()
                    .text_color(gpui::rgb(0x888888))
                    .child("No file selection available – all files will be downloaded.")
                    .into_any_element()
            })
            .child(
                Checkbox::new("overwrite-existing")
                    .checked(self.overwrite)
                    .label("Overwrite existing files")
                    .on_click(cx.listener(|this, _checked, _, cx| this.on_toggle_overwrite(cx))),
            )
    }
}
