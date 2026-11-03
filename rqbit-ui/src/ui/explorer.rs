//! Explorer: a dockable panel showing every file of every torrent as a
//! tree (torrent → folders → files), with live per-file download state.
//!
//! Data comes in two streams, both already flowing through the
//! [`TorrentClient`](crate::client::TorrentClient): the torrent **list**
//! (refreshed ~1s by `MainPanel`) carries `stats.file_progress` — per-file
//! downloaded bytes — while the (static) file structure is fetched once per
//! torrent via `details()`. The tree is reinstalled on every list refresh;
//! `TreeItem` expansion state lives behind an `Rc`, so the user's folding
//! survives while labels pick up new percentages.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Window, div, px,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _,
    dock::{BasePanel, Panel, PanelEvent},
    h_flex,
    list::ListItem,
    menu::{PopupMenu, PopupMenuItem},
    notification::Notification,
    tree::{Tree, TreeEntry, TreeItem, TreeState},
};
use rqbit_protocol::{TorrentDetails, TorrentFile, TorrentId, TorrentStats};

use crate::client::TorrentClient;
use crate::host::UiHost;

/// Emitted to the host when the user selects an entry in the tree, so the
/// torrent table / detail panel can follow along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplorerEvent {
    /// The torrent behind the newly selected tree entry.
    TorrentSelected(u64),
}

/// Download state of one file, derived from the `only_files` selection and
/// the per-file progress byte count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// Fully on disk.
    Done,
    /// Included and partially downloaded.
    Downloading,
    /// Included but with no bytes yet.
    Queued,
    /// Not part of the torrent's `only_files` selection.
    Excluded,
    /// A BEP-47 padding file: present in the info dict, no data ever.
    Padding,
    /// No stats received yet.
    Unknown,
}

/// Classify a file for display. A zero-length included file counts as done
/// (there is nothing to download); unknown progress stays unknown.
fn file_status(length: u64, progress: Option<u64>, included: bool, padding: bool) -> FileStatus {
    if padding {
        return FileStatus::Padding;
    }
    if !included {
        return FileStatus::Excluded;
    }
    match progress {
        Some(p) if length == 0 || p >= length => FileStatus::Done,
        Some(p) if p > 0 => FileStatus::Downloading,
        Some(_) => FileStatus::Queued,
        None => FileStatus::Unknown,
    }
}

/// Whole-torrent percent for the root label, mirroring the torrent table's
/// `progress_bytes / total_bytes` computation.
fn torrent_progress_pct(stats: Option<&TorrentStats>) -> Option<u8> {
    let stats = stats?;
    if stats.total_bytes == 0 {
        return None;
    }
    Some(((stats.progress_bytes * 100 / stats.total_bytes).min(100)) as u8)
}

/// Root label: torrent name plus the aggregate percent once size is known.
fn torrent_root_label(name: &str, stats: Option<&TorrentStats>) -> String {
    match torrent_progress_pct(stats) {
        Some(pct) => format!("{name} · {pct}%"),
        None => name.to_string(),
    }
}

fn file_progress_pct(progress: Option<u64>, length: u64) -> Option<u8> {
    progress.map(|p| {
        if length == 0 {
            100
        } else {
            // Only u64 overflow (files over ~180 PB) can miss the chain.
            p.checked_mul(100)
                .and_then(|scaled| scaled.checked_div(length))
                .map_or(100, |pct| pct.min(100) as u8)
        }
    })
}

/// What a tree node represents. Looked up by tree-item id for rendering and
/// context-menu actions.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NodeKind {
    /// A torrent root. Its disk path is the torrent's output folder itself
    /// (`details.output_folder`, which rqbit reports already including any
    /// extraction subfolder); everything below it is relative to the
    /// torrent root.
    Torrent,
    /// A directory inside the torrent, addressed by its components below
    /// the torrent root.
    Dir { path: Vec<String> },
    /// A file leaf: index into `TorrentDetails.files` plus its path below
    /// the torrent root, size and live status.
    File {
        file_index: usize,
        path: Vec<String>,
        length: u64,
        status: FileStatus,
        progress_pct: Option<u8>,
    },
}

impl NodeKind {
    /// The entry's own path components below the torrent root (empty for a
    /// torrent root).
    fn own_path(&self) -> Vec<String> {
        match self {
            NodeKind::Torrent => Vec::new(),
            NodeKind::Dir { path } => path.clone(),
            NodeKind::File { path, .. } => path.clone(),
        }
    }
}

/// Render/action metadata for one node, keyed by tree-item id.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NodeInfo {
    torrent_id: u64,
    kind: NodeKind,
}

// --- node id scheme ---------------------------------------------------------
//
// Stable across refreshes: `t{id}` for roots, `t{id}/d/<joined components>`
// for directories, `t{id}/f/<file index>` for files. The `d`/`f` segments
// keep a file from colliding with a directory chain that spells the same
// string.

fn root_item_id(torrent_id: u64) -> SharedString {
    SharedString::from(format!("t{torrent_id}"))
}

fn dir_item_id(torrent_id: u64, path: &[String]) -> SharedString {
    SharedString::from(format!("t{torrent_id}/d/{}", path.join("/")))
}

fn file_item_id(torrent_id: u64, file_index: usize) -> SharedString {
    SharedString::from(format!("t{torrent_id}/f/{file_index}"))
}

/// Extract the torrent id from an item id (`t<id>…`). Roots, dirs and files
/// share the prefix, so one parser serves cache pruning for all of them.
fn torrent_id_of_item_id(item_id: &str) -> Option<u64> {
    item_id.strip_prefix('t')?.split('/').next()?.parse().ok()
}

/// A directory tier while grouping files by their path components. Only
/// exists during tree construction.
#[derive(Default)]
struct DirGroup {
    dirs: BTreeMap<String, DirGroup>,
    /// Indices into the torrent's `files` vec, in torrent order.
    files: Vec<usize>,
}

impl DirGroup {
    /// File a torrent file into the hierarchy. The last component is the
    /// file's name; everything before it is the directory chain it lives in.
    fn insert(&mut self, components: &[String], file_index: usize) {
        let Some(dirs) = components.split_last().map(|(_, dirs)| dirs) else {
            // Degenerate file with no path components at all.
            self.files.push(file_index);
            return;
        };
        let mut group = self;
        for dir in dirs {
            group = group.dirs.entry(dir.clone()).or_default();
        }
        group.files.push(file_index);
    }
}

/// Sort-key class for [`natural_cmp`]: digits (0-9) first, then latin
/// (a-z), then cyrillic (а-я); everything else sorts last.
fn char_class(c: char) -> u8 {
    if c.is_ascii_digit() {
        0
    } else if c.is_ascii_alphabetic() {
        1
    } else if ('\u{0400}'..='\u{04FF}').contains(&c) {
        2
    } else {
        3
    }
}

/// Case-insensitive name comparison ordering names 0-9, a-z, а-я: the first
/// differing characters decide by class, then by codepoint.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().flat_map(char::to_lowercase);
    let mut bi = b.chars().flat_map(char::to_lowercase);
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) => {
                let (class_a, class_b) = (char_class(ca), char_class(cb));
                if class_a != class_b {
                    return class_a.cmp(&class_b);
                }
                if ca != cb {
                    return ca.cmp(&cb);
                }
            }
        }
    }
}

/// A file's display name: its last path component (the `name` field carries
/// the whole relative path for nested files).
fn last_component(file: &TorrentFile) -> &str {
    file.components
        .last()
        .map(String::as_str)
        .unwrap_or(&file.name)
}

/// Build one torrent's subtree: `TreeItem`s for the tree plus the flat
/// [`NodeInfo`] metadata for every generated id. File components are
/// relative to the torrent root, and the root row stands in for that root
/// folder on disk (`details.output_folder`, which rqbit reports already
/// including any extraction subfolder) — so the torrent name is never a
/// rendered or joined path level. Directories come before files at each
/// tier, all sorted 0-9, a-z, а-я.
fn build_torrent_subtree(
    torrent_id: u64,
    files: &[TorrentFile],
    progress: Option<&[u64]>,
) -> (Vec<TreeItem>, Vec<(SharedString, NodeInfo)>) {
    let mut root = DirGroup::default();
    for (idx, file) in files.iter().enumerate() {
        root.insert(&file.components, idx);
    }

    let mut metas = Vec::new();
    let items = emit_dir_group(torrent_id, &root, files, progress, &[], &mut metas);
    (items, metas)
}

fn emit_dir_group(
    torrent_id: u64,
    group: &DirGroup,
    files: &[TorrentFile],
    progress: Option<&[u64]>,
    dir_path: &[String],
    metas: &mut Vec<(SharedString, NodeInfo)>,
) -> Vec<TreeItem> {
    let mut items = Vec::new();

    let mut dirs: Vec<_> = group.dirs.iter().collect();
    dirs.sort_by(|(a, _), (b, _)| natural_cmp(a, b));
    for (name, sub) in dirs {
        let mut child_path = dir_path.to_vec();
        child_path.push(name.clone());
        let id = dir_item_id(torrent_id, &child_path);
        let grandchildren = emit_dir_group(torrent_id, sub, files, progress, &child_path, metas);
        metas.push((
            id.clone(),
            NodeInfo {
                torrent_id,
                kind: NodeKind::Dir { path: child_path },
            },
        ));
        items.push(TreeItem::new(id, name.clone()).children(grandchildren));
    }

    let mut file_indices = group.files.clone();
    file_indices
        .sort_by(|&x, &y| natural_cmp(last_component(&files[x]), last_component(&files[y])));
    for file_index in file_indices {
        let file = &files[file_index];
        let progress_bytes = progress.and_then(|p| p.get(file_index).copied());
        let status = file_status(file.length, progress_bytes, file.included, file.padding);
        let id = file_item_id(torrent_id, file_index);
        // The disk path: the tier's path (which includes any virtual name
        // folder) plus the file's own last component.
        let mut path = dir_path.to_vec();
        path.push(last_component(file).to_owned());
        metas.push((
            id.clone(),
            NodeInfo {
                torrent_id,
                kind: NodeKind::File {
                    file_index,
                    path,
                    length: file.length,
                    status,
                    progress_pct: file_progress_pct(progress_bytes, file.length),
                },
            },
        ));
        items.push(TreeItem::new(id, last_component(file).to_owned()));
    }

    items
}

/// Marker type for the "path copied" notification, so repeated copies
/// replace instead of stacking.
struct ExplorerPathCopiedNotice;

/// Cached static metadata for one torrent: the file list answered by
/// `details()`.
struct TorrentFiles {
    files: Vec<TorrentFile>,
}

/// The Explorer dock panel: one tree of every torrent's files.
pub struct ExplorerPanel {
    client: Arc<dyn TorrentClient>,
    host: Arc<dyn UiHost>,
    tree_state: Entity<TreeState>,
    /// Torrent roots in first-seen order (stable across refreshes). The
    /// `TreeItem`s are persistent instances, so their `Rc` expansion state
    /// survives the re-`set_items` on every refresh.
    roots: Vec<TreeItem>,
    /// Item id → node metadata, recomputed from the caches whenever the
    /// tree changes. Rendering and menus read it by entry id.
    meta: HashMap<SharedString, NodeInfo>,
    /// torrent id → output folder (from list rows; may be empty on remote
    /// hosts that don't expose paths).
    folders: HashMap<u64, String>,
    /// torrent id → static file metadata, filled once `details()` answers.
    files: HashMap<u64, TorrentFiles>,
    /// torrent id → per-file downloaded bytes (latest list refresh).
    progress: HashMap<u64, Vec<u64>>,
    /// Last user-selected item id, restored after each `set_items` wipe.
    selected_node: Option<SharedString>,
    /// Selection already reported via [`ExplorerEvent::TorrentSelected`].
    /// Deliberately not cleared when the tree wipes selection, so the
    /// per-second restore cycle never re-emits.
    reported_selection: Option<SharedString>,
    focus_handle: FocusHandle,
}

impl ExplorerPanel {
    pub fn new(
        client: Arc<dyn TorrentClient>,
        host: Arc<dyn UiHost>,
        cx: &mut Context<Self>,
    ) -> Self {
        let tree_state = cx.new(|cx| TreeState::new(cx));
        cx.observe(&tree_state, |this, _, cx| this.on_tree_state_changed(cx))
            .detach();

        Self {
            client,
            host,
            tree_state,
            roots: Vec::new(),
            meta: HashMap::new(),
            folders: HashMap::new(),
            files: HashMap::new(),
            progress: HashMap::new(),
            selected_node: None,
            reported_selection: None,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Feed the latest torrent list into the tree. Called by `MainPanel` on
    /// every list refresh (poll tick or push).
    pub fn set_torrents(&mut self, torrents: &[TorrentDetails], cx: &mut Context<Self>) {
        let live_ids: HashSet<u64> = torrents.iter().filter_map(|t| t.id).collect();

        // Refresh the caches that ride along on list rows.
        for t in torrents {
            let Some(id) = t.id else { continue };
            if let Some(stats) = &t.stats {
                self.progress.insert(id, stats.file_progress.clone());
            }
            if !t.output_folder.is_empty() {
                self.folders.insert(id, t.output_folder.clone());
            }
        }

        // Drop roots (and their caches) for torrents that are gone.
        let before = self.roots.len();
        self.roots.retain(|root| {
            torrent_id_of_item_id(&root.id).is_some_and(|id| live_ids.contains(&id))
        });
        if self.roots.len() != before {
            let gone: Vec<u64> = self
                .folders
                .keys()
                .copied()
                .filter(|id| !live_ids.contains(id))
                .collect();
            for id in gone {
                self.folders.remove(&id);
                self.files.remove(&id);
                self.progress.remove(&id);
            }
        }

        // Add new roots; refresh existing labels in place.
        for t in torrents {
            let Some(id) = t.id else { continue };
            let label = SharedString::from(torrent_root_label(
                t.name.as_deref().unwrap_or(&t.info_hash),
                t.stats.as_deref(),
            ));
            if let Some(root) = self
                .roots
                .iter_mut()
                .find(|r| torrent_id_of_item_id(&r.id) == Some(id))
            {
                root.label = label;
                continue;
            }

            let mut root = TreeItem::new(root_item_id(id), label).expanded(true);
            if let Some(cached) = self.files.get(&id) {
                let (children, child_metas) = build_torrent_subtree(
                    id,
                    &cached.files,
                    self.progress.get(&id).map(Vec::as_slice),
                );
                root.children = children;
                self.meta.extend(child_metas);
            } else {
                self.queue_details_fetch(id, cx);
            }
            self.roots.push(root);
        }

        // Sorted tree, 0-9 → a-z → а-я. Roots compare by label; the percent
        // suffix only breaks ties between torrents with identical names.
        self.roots.sort_by(|a, b| natural_cmp(&a.label, &b.label));

        self.rebuild_meta();
        self.rebuild_tree(cx);
    }

    /// Install file metadata answered by a `details()` fetch. The torrent's
    /// children are built once here; later refreshes only touch labels.
    fn install_files(&mut self, torrent_id: u64, files: Vec<TorrentFile>, cx: &mut Context<Self>) {
        self.files.insert(torrent_id, TorrentFiles { files });
        let Some(cached) = self.files.get(&torrent_id) else {
            return;
        };
        let (children, metas) = build_torrent_subtree(
            torrent_id,
            &cached.files,
            self.progress.get(&torrent_id).map(Vec::as_slice),
        );

        let root_ix = self
            .roots
            .iter()
            .position(|r| torrent_id_of_item_id(&r.id) == Some(torrent_id));
        if let Some(ix) = root_ix {
            self.roots[ix].children = children;
            self.meta.extend(metas);
            self.rebuild_tree(cx);
        }
    }

    /// Fetch (once per torrent) the static file list the list rows don't
    /// carry. Failures are silent — the root stays a bare node until the
    /// torrent is removed and re-added.
    fn queue_details_fetch(&mut self, torrent_id: u64, cx: &mut Context<Self>) {
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let Ok(details) = client.details(&TorrentId::Id(torrent_id)).await else {
                return;
            };
            let Some(files) = details.files else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                // List rows always carry the id, so the fetch is queued once;
                // still guard against a late duplicate answer.
                if this.files.contains_key(&torrent_id) {
                    return;
                }
                this.install_files(torrent_id, files, cx);
            });
        })
        .detach();
    }

    /// Regenerate the flat id → metadata map from the cached file lists.
    /// The tree builder is deterministic, so the metadata can never drift
    /// from the rendered items.
    fn rebuild_meta(&mut self) {
        self.meta.clear();
        for root in &self.roots {
            let Some(torrent_id) = torrent_id_of_item_id(&root.id) else {
                continue;
            };
            // The torrent root's disk path is the torrent's output folder
            // itself (already including any extraction subfolder).
            self.meta.insert(
                root.id.clone(),
                NodeInfo {
                    torrent_id,
                    kind: NodeKind::Torrent,
                },
            );
            if let Some(cached) = self.files.get(&torrent_id) {
                let (_, metas) = build_torrent_subtree(
                    torrent_id,
                    &cached.files,
                    self.progress.get(&torrent_id).map(Vec::as_slice),
                );
                self.meta.extend(metas);
            }
        }
    }

    /// Reinstall the (label-refreshed) roots into the tree state and restore
    /// the selection the wipe just cleared. Expansion state survives on its
    /// own via the shared `TreeItem` state.
    fn rebuild_tree(&mut self, cx: &mut Context<Self>) {
        let items = self.roots.clone();
        self.tree_state.update(cx, |state, cx| {
            state.set_items(items, cx);
        });
        if let Some(selected) = &self.selected_node {
            let ix = self.tree_state.read(cx).index_of(selected);
            if let Some(ix) = ix {
                self.tree_state.update(cx, |state, cx| {
                    state.set_selected_index(Some(ix), cx);
                });
            }
        }
    }

    /// Track tree selection: remember it for restore, and report genuinely
    /// new selections to the host. The reported id is deliberately kept when
    /// the tree clears its selection (every refresh wipes then restores it),
    /// so the restore cycle never re-emits.
    fn on_tree_state_changed(&mut self, cx: &mut Context<Self>) {
        let selected = self
            .tree_state
            .read(cx)
            .selected_entry()
            .map(|e| e.item().id.clone());
        if selected.is_some() {
            self.selected_node = selected.clone();
        }
        if let Some(id) = selected
            && self.reported_selection.as_ref() != Some(&id)
        {
            self.reported_selection = Some(id.clone());
            if let Some(info) = self.meta.get(&id) {
                cx.emit(ExplorerEvent::TorrentSelected(info.torrent_id));
            }
        }
    }

    /// Build one visible row. Called from the `Tree`'s render closure, which
    /// hands us a read on the panel via its weak handle.
    fn render_entry(&self, entry: &TreeEntry, selected: bool, cx: &App) -> ListItem {
        let id = entry.item().id.clone();
        let theme = cx.theme();
        let indent = px(6. + entry.depth() as f32 * 14.);
        let mut row = h_flex().items_center().gap_1p5().pl(indent);

        let Some(info) = self.meta.get(&id) else {
            row = row.child(div().text_sm().child(entry.item().label.clone()));
            return ListItem::new(id).child(row).selected(selected);
        };

        let label_color;
        let mut suffix: Option<String> = None;

        match &info.kind {
            NodeKind::Torrent => {
                label_color = theme.foreground;
                row = row.child(
                    Icon::new(IconName::HardDrive)
                        .xsmall()
                        .text_color(theme.muted_foreground),
                );
            }
            NodeKind::Dir { .. } => {
                label_color = theme.foreground;
                row = row.child(
                    Icon::new(if entry.is_expanded() {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .xsmall()
                    .text_color(theme.muted_foreground),
                );
                row = row.child(
                    Icon::new(if entry.is_expanded() {
                        IconName::FolderOpen
                    } else {
                        IconName::Folder
                    })
                    .xsmall()
                    .text_color(theme.muted_foreground),
                );
            }
            NodeKind::File {
                status,
                progress_pct,
                ..
            } => {
                let (icon, icon_color) = match status {
                    FileStatus::Done => (IconName::CircleCheck, theme.success),
                    FileStatus::Downloading => (IconName::LoaderCircle, theme.foreground),
                    FileStatus::Queued => (IconName::File, theme.warning),
                    FileStatus::Excluded => (IconName::EyeOff, theme.muted_foreground),
                    FileStatus::Padding => (IconName::File, theme.muted_foreground),
                    FileStatus::Unknown => (IconName::File, theme.foreground),
                };
                label_color = if matches!(status, FileStatus::Excluded | FileStatus::Padding) {
                    theme.muted_foreground
                } else {
                    theme.foreground
                };
                row = row.child(Icon::new(icon).xsmall().text_color(icon_color));
                suffix = match status {
                    FileStatus::Downloading => progress_pct.map(|p| format!("{p}%")),
                    FileStatus::Queued => Some("queued".to_string()),
                    FileStatus::Excluded => Some("excluded".to_string()),
                    FileStatus::Padding => Some("padding".to_string()),
                    FileStatus::Done | FileStatus::Unknown => None,
                };
            }
        }

        row = row.child(
            div()
                .text_sm()
                .text_color(label_color)
                .truncate()
                .child(entry.item().label.clone()),
        );
        if let Some(suffix) = suffix {
            row = row.child(
                div()
                    .text_xs()
                    .flex_shrink_0()
                    .text_color(theme.muted_foreground)
                    .child(suffix),
            );
        }

        ListItem::new(id).child(row).selected(selected)
    }

    /// Build the right-click menu for one entry: reveal on disk (desktop
    /// hosts only) and copy the resolved path.
    fn context_menu(&self, entry: &TreeEntry, mut menu: PopupMenu) -> PopupMenu {
        let id = entry.item().id.clone();
        let Some(info) = self.meta.get(&id) else {
            return menu;
        };
        let host = self.host.clone();
        let caps = host.capabilities();
        let output_folder = self
            .folders
            .get(&info.torrent_id)
            .cloned()
            .unwrap_or_default();

        let entry_path = info.kind.own_path();
        let copy_path = join_disk_path(&output_folder, &entry_path);
        let open_dir = match &info.kind {
            // Folders (and roots) open themselves; files open their parent.
            NodeKind::File { path, .. } => {
                join_disk_path(&output_folder, &path[..path.len().saturating_sub(1)])
            }
            _ => copy_path.clone(),
        };

        if caps.open_folder
            && let Some(path) = open_dir
        {
            let host = host.clone();
            menu = menu.item(
                PopupMenuItem::new("Open Folder")
                    .on_click(move |_, _, _cx: &mut App| host.open_folder(&path)),
            );
        }

        if let Some(path) = copy_path {
            menu = menu.item(PopupMenuItem::new("Copy Path").on_click(
                move |_, window, cx: &mut App| {
                    cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                    window.push_notification(
                        Notification::success(format!("Copied {path}"))
                            .title("Copied")
                            .id::<ExplorerPathCopiedNotice>(),
                        cx,
                    );
                },
            ));
        }

        menu
    }
}

/// Join the output folder and relative components into an on-disk path.
/// `None` when the host didn't report an output folder.
fn join_disk_path(output_folder: &str, components: &[String]) -> Option<String> {
    if output_folder.is_empty() {
        return None;
    }
    let mut path = PathBuf::from(output_folder);
    for component in components {
        path.push(component);
    }
    Some(path.to_string_lossy().into_owned())
}

impl BasePanel for ExplorerPanel {
    fn panel_name(&self) -> &'static str {
        "explorer"
    }

    /// The toolbar toggle owns visibility; never let the panel itself close.
    fn closable(&self, _cx: &App) -> bool {
        false
    }
}

impl Panel for ExplorerPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Explorer"
    }

    /// The tree draws edge to edge; no tab-group padding.
    fn inner_padding(&self, _cx: &App) -> bool {
        false
    }
}

impl EventEmitter<PanelEvent> for ExplorerPanel {}
impl EventEmitter<ExplorerEvent> for ExplorerPanel {}

impl Focusable for ExplorerPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ExplorerPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.weak_entity();
        let menu_weak = weak.clone();
        let tree = Tree::new(
            &self.tree_state,
            move |_ix, entry, selected, _window, cx| {
                let Some(panel) = weak.upgrade() else {
                    return ListItem::new(entry.item().id.clone())
                        .child(entry.item().label.clone());
                };
                panel.read(cx).render_entry(entry, selected, cx)
            },
        )
        .context_menu(move |_ix, entry, menu, _window, cx| {
            let Some(panel) = menu_weak.upgrade() else {
                return menu;
            };
            panel.read(cx).context_menu(entry, menu)
        });

        div().size_full().overflow_hidden().child(tree)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, components: &[&str], length: u64, included: bool) -> TorrentFile {
        TorrentFile {
            name: name.to_string(),
            components: components.iter().map(|c| c.to_string()).collect(),
            length,
            included,
            padding: false,
            custom: None,
        }
    }

    fn ids(items: &[TreeItem]) -> Vec<String> {
        items.iter().map(|i| i.id.to_string()).collect()
    }

    #[test]
    fn file_status_covers_every_state() {
        assert_eq!(file_status(100, Some(100), true, false), FileStatus::Done);
        assert_eq!(file_status(100, Some(150), true, false), FileStatus::Done);
        assert_eq!(file_status(0, Some(0), true, false), FileStatus::Done);
        assert_eq!(
            file_status(100, Some(50), true, false),
            FileStatus::Downloading
        );
        assert_eq!(file_status(100, Some(0), true, false), FileStatus::Queued);
        assert_eq!(file_status(100, None, true, false), FileStatus::Unknown);
        assert_eq!(
            file_status(100, Some(100), false, false),
            FileStatus::Excluded
        );
        assert_eq!(file_status(100, Some(100), true, true), FileStatus::Padding);
    }

    #[test]
    fn subtree_groups_nested_dirs_dirs_before_files() {
        let files = vec![
            file("zeta.txt", &["zeta.txt"], 10, true),
            file("b.txt", &["alpha", "sub", "b.txt"], 20, true),
            file("a.txt", &["alpha", "a.txt"], 30, true),
        ];
        let (items, metas) = build_torrent_subtree(7, &files, None);

        // Top level under the torrent root: the `alpha` dir first, then the
        // loose zeta.txt.
        assert_eq!(ids(&items), vec!["t7/d/alpha", "t7/f/0"]);

        let alpha = &items[0];
        // Within the dir: nested `sub` dir before the loose file.
        assert_eq!(ids(&alpha.children), vec!["t7/d/alpha/sub", "t7/f/2"]);
        assert_eq!(ids(&alpha.children[0].children), vec!["t7/f/1"]);

        // Every emitted item has metadata, and ids are unique.
        let meta_ids: HashSet<&SharedString> = metas.iter().map(|(id, _)| id).collect();
        assert_eq!(meta_ids.len(), metas.len());
        assert!(metas.iter().any(|(id, _)| id.as_ref() == "t7/d/alpha/sub"));
    }

    #[test]
    fn subtree_mirrors_disk_layout() {
        // File components are relative to the torrent root; on disk they sit
        // under `details.output_folder`, which rqbit reports already
        // including the extraction subfolder. So the torrent name never
        // appears as an id or path level (the torrent root displays it).
        let files = vec![
            file("a.txt", &["a.txt"], 10, true),
            file("b.txt", &["sub", "b.txt"], 20, true),
        ];
        let (items, metas) = build_torrent_subtree(9, &files, None);

        assert_eq!(ids(&items), vec!["t9/d/sub", "t9/f/0"]);
        assert_eq!(ids(&items[0].children), vec!["t9/f/1"]);

        let by_id: HashMap<&str, &NodeInfo> =
            metas.iter().map(|(id, info)| (id.as_ref(), info)).collect();
        assert_eq!(by_id["t9/f/0"].kind.own_path(), vec!["a.txt".to_string()]);
        assert_eq!(
            by_id["t9/f/1"].kind.own_path(),
            vec!["sub".to_string(), "b.txt".to_string()]
        );
        assert!(!metas.iter().any(|(id, _)| id.as_ref() == "t9/d/Pack"));

        // A torrent whose single file has no folder components: the file
        // sits directly under the torrent root, bare filename label.
        let single = vec![file("solo.txt", &["solo.txt"], 10, true)];
        let (items, metas) = build_torrent_subtree(9, &single, None);
        assert_eq!(ids(&items), vec!["t9/f/0"]);
        assert_eq!(items[0].label, "solo.txt");
        assert_eq!(metas[0].1.kind.own_path(), vec!["solo.txt".to_string()]);

        // A single entry that carries its own folder: that folder exists on
        // disk and is rendered like any directory level.
        let own_folder = vec![file("solo.txt", &["Data", "solo.txt"], 10, true)];
        let (items, _) = build_torrent_subtree(9, &own_folder, None);
        assert_eq!(ids(&items), vec!["t9/d/Data"]);
    }

    #[test]
    fn names_sort_digits_latin_cyrillic() {
        let ordered = [
            "10",
            "9",
            "Alpha",
            "apple",
            "Beta",
            "яблоко",
            "Яблоко2",
            "_cache",
        ];
        for pair in ordered.windows(2) {
            assert_eq!(
                natural_cmp(pair[0], pair[1]),
                Ordering::Less,
                "{:?} should sort before {:?}",
                pair[0],
                pair[1]
            );
        }
        assert_eq!(natural_cmp("alpha", "Alpha"), Ordering::Equal);
    }

    #[test]
    fn subtree_sorts_files_naturally() {
        let files = vec![
            file("Зима.txt", &["Зима.txt"], 1, true),
            file("apple.txt", &["apple.txt"], 2, true),
            file("2001.txt", &["2001.txt"], 3, true),
            file("_cache.bin", &["_cache.bin"], 4, true),
            file("Beta.txt", &["Beta.txt"], 5, true),
        ];
        let (items, _) = build_torrent_subtree(5, &files, None);

        // Directly under the torrent root: digits, then latin
        // (case-insensitive), then cyrillic, then symbols.
        assert_eq!(
            ids(&items),
            vec!["t5/f/2", "t5/f/1", "t5/f/4", "t5/f/0", "t5/f/3"]
        );
    }

    #[test]
    fn subtree_sorts_dirs_naturally() {
        let files = vec![
            file("a", &["Зима", "a"], 1, true),
            file("a", &["apple", "a"], 2, true),
            file("a", &["2001", "a"], 3, true),
        ];
        let (items, _) = build_torrent_subtree(6, &files, None);

        assert_eq!(ids(&items), vec!["t6/d/2001", "t6/d/apple", "t6/d/Зима"]);
    }

    #[test]
    fn subtree_metas_carry_status_and_percent() {
        let files = vec![
            file("done.bin", &["done.bin"], 100, true),
            file("part.bin", &["part.bin"], 200, true),
            file("skipped.bin", &["skipped.bin"], 300, false),
        ];
        let (_, metas) = build_torrent_subtree(3, &files, Some(&[100, 50, 0]));

        let by_id: HashMap<&str, &NodeInfo> =
            metas.iter().map(|(id, info)| (id.as_ref(), info)).collect();
        assert!(matches!(
            by_id["t3/f/0"].kind,
            NodeKind::File {
                status: FileStatus::Done,
                progress_pct: Some(100),
                ..
            }
        ));
        assert!(matches!(
            by_id["t3/f/1"].kind,
            NodeKind::File {
                status: FileStatus::Downloading,
                progress_pct: Some(25),
                ..
            }
        ));
        assert!(matches!(
            by_id["t3/f/2"].kind,
            NodeKind::File {
                status: FileStatus::Excluded,
                progress_pct: Some(0),
                ..
            }
        ));
    }

    #[test]
    fn torrent_ids_parse_from_every_id_kind() {
        assert_eq!(torrent_id_of_item_id("t42"), Some(42));
        assert_eq!(torrent_id_of_item_id("t42/d/alpha"), Some(42));
        assert_eq!(torrent_id_of_item_id("t42/f/3"), Some(42));
        assert_eq!(torrent_id_of_item_id("nope"), None);
    }

    #[test]
    fn root_label_appends_percent_once_size_is_known() {
        let stats = |progress: u64, total: u64| TorrentStats {
            state: rqbit_protocol::TorrentState::Live,
            file_progress: vec![],
            error: None,
            progress_bytes: progress,
            uploaded_bytes: 0,
            total_bytes: total,
            finished: false,
            live: None,
        };
        assert_eq!(torrent_root_label("Ubuntu", None), "Ubuntu");
        assert_eq!(torrent_root_label("Ubuntu", Some(&stats(0, 0))), "Ubuntu");
        assert_eq!(
            torrent_root_label("Ubuntu", Some(&stats(25, 100))),
            "Ubuntu · 25%"
        );
    }

    #[test]
    fn disk_paths_join_and_require_a_folder() {
        assert_eq!(join_disk_path("", &["a".to_string()]), None);
        // Expected through PathBuf: the separators are platform-native
        // (`\` on Windows), which is what the open-folder and copy-path
        // consumers want.
        assert_eq!(
            join_disk_path("/downloads", &["alpha", "b.txt"].map(String::from)),
            Some(
                PathBuf::from("/downloads")
                    .join("alpha")
                    .join("b.txt")
                    .to_string_lossy()
                    .into_owned()
            )
        );
        // A torrent root (no components) joins to the folder itself.
        assert_eq!(
            join_disk_path("/downloads", &[]),
            Some("/downloads".to_string())
        );
    }
}
