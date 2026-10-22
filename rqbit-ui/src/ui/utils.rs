//! Shared formatting helpers and small glue for the torrent UI.

use std::sync::Arc;

use gpui::{
    App, Context, Div, Entity, InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels,
    Render, Styled, div, px,
};
use gpui_kit::component::{ActiveTheme, Theme, menu::PopupMenuItem, v_flex};

use rqbit_protocol::{CustomColumnKind, TorrentAction, TorrentId};

use crate::client::{AddTorrentRequest, TorrentClient, UiAddSource};
use crate::ui::{add_torrent_dialog::AddTorrentEntry, file_table::FileRow, main_panel::MainPanel};

/// Binary unit multipliers (powers of 1024), computed once and reused.
const KIB: f64 = 1024.0;
const MIB: f64 = KIB * 1024.0;
const GIB: f64 = MIB * 1024.0;
const TIB: f64 = GIB * 1024.0;

/// Format a byte count into a human-readable binary unit string.
///
/// `suffix` is appended after the unit (e.g. `""` for sizes, `"/s"` for
/// speeds), `decimals` controls the number of fractional digits, and `labels`
/// supplies the four unit prefixes for the TiB/GiB/MiB/KiB tiers (top to
/// bottom). Values below `KIB` are rendered as whole bytes.
fn format_binary(bytes: f64, suffix: &str, decimals: usize, labels: [&str; 4]) -> String {
    let (div, unit) = if bytes >= TIB {
        (TIB, labels[0])
    } else if bytes >= GIB {
        (GIB, labels[1])
    } else if bytes >= MIB {
        (MIB, labels[2])
    } else if bytes >= KIB {
        (KIB, labels[3])
    } else {
        return format!("{:.0} B{}", bytes, suffix);
    };
    format!("{0:.1$} {2}B{3}", bytes / div, decimals, unit, suffix)
}

/// Format a byte count in human-readable form (binary units).
pub fn format_bytes(bytes: u64) -> String {
    format_binary(bytes as f64, "", 2, ["Ti", "Gi", "Mi", "Ki"])
}

/// Format a speed (given in Mbps) in human-readable form.
pub fn format_speed(mbps: f64) -> String {
    format_binary(mbps * MIB, "/s", 1, ["G", "G", "M", "K"])
}

/// Format a duration in seconds as a compact human-readable uptime string.
pub fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86400;
    let hours = (seconds % 86400) / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m {secs}s")
    } else if minutes > 0 {
        format!("{minutes}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

/// Format an ETA in seconds the same way librqbit's
/// `DurationWithHumanReadable` does (`2h 3m`, `3m 4s`, `5s`).
pub fn format_eta(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

/// Visual role of a recognized status value in a `kind: "status"`
/// hook-provided custom column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTone {
    Success,
    Warning,
    Danger,
}

/// Map a custom-column status value to a visual tone (case-insensitive).
/// `None` = unrecognized; such values render as plain text.
pub fn status_tone(value: &str) -> Option<StatusTone> {
    match value.trim().to_lowercase().as_str() {
        "ok" | "synced" | "sync" | "done" | "complete" | "completed" | "up" | "yes" | "true"
        | "healthy" => Some(StatusTone::Success),
        "pending" | "syncing" | "partial" | "in-progress" | "progress" | "queued"
        | "connecting" | "unknown" => Some(StatusTone::Warning),
        "error" | "failed" | "fail" | "missing" | "conflict" | "stale" | "out-of-date"
        | "outdated" | "down" | "no" | "false" => Some(StatusTone::Danger),
        _ => None,
    }
}

/// Render one hook-provided custom-column cell: plain text, or
/// colorized text for recognized status values when the column kind is
/// [`CustomColumnKind::Status`]. Only the text is tinted — the background
/// is left untouched.
pub fn render_custom_cell(value: &str, kind: CustomColumnKind, theme: &Theme) -> Div {
    if let CustomColumnKind::Status = kind
        && let Some(tone) = status_tone(value)
    {
        let color = match tone {
            StatusTone::Success => theme.success,
            StatusTone::Warning => theme.warning,
            StatusTone::Danger => theme.danger,
        };
        return div().text_color(color).child(value.to_string());
    }
    div().child(value.to_string())
}

/// Build a context-menu item that runs a pause/start action on a torrent.
pub fn torrent_action_menu_item(
    label: String,
    torrent_id: usize,
    action: TorrentAction,
    client: Arc<dyn TorrentClient>,
) -> PopupMenuItem {
    PopupMenuItem::new(label).on_click(move |_, _, cx: &mut App| {
        let client = client.clone();
        let action = action.clone();
        cx.spawn(async move |_| {
            let _ = client
                .action(&TorrentId::Id(torrent_id as u64), action)
                .await;
        })
        .detach();
    })
}

/// Build a context-menu item that runs a pause/start action on several
/// torrents in turn (used when the menu targets a multi-row selection).
pub fn torrent_bulk_action_menu_item(
    label: String,
    torrent_ids: Vec<usize>,
    action: TorrentAction,
    client: Arc<dyn TorrentClient>,
) -> PopupMenuItem {
    PopupMenuItem::new(label).on_click(move |_, _, cx: &mut App| {
        let client = client.clone();
        let action = action.clone();
        let torrent_ids = torrent_ids.clone();
        cx.spawn(async move |_| {
            for id in torrent_ids {
                let _ = client
                    .action(&TorrentId::Id(id as u64), action.clone())
                    .await;
            }
        })
        .detach();
    })
}

/// Render a centered modal overlay (dimmed backdrop + panel) for a dialog
/// entity. Clicking the backdrop invokes `close`. `max_height`, when set, caps
/// the panel height (used for the file-selection dialog).
pub fn render_modal_overlay<E: Render + 'static>(
    dialog: &Entity<E>,
    close: fn(&mut MainPanel, &mut Context<MainPanel>),
    max_height: Option<Pixels>,
    cx: &mut Context<MainPanel>,
) -> impl IntoElement {
    let theme = cx.theme();
    let mut panel = v_flex()
        .absolute()
        .top(px(80.))
        .left(px(80.))
        .right(px(80.))
        .bg(theme.background)
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .p_4()
        .overflow_hidden()
        .child(dialog.clone())
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
    if let Some(h) = max_height {
        panel = panel.max_h(h);
    }
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .bg(theme.muted)
        .opacity(0.8)
        .child(panel)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| close(this, cx)),
        )
}

/// Build an [`AddTorrentEntry`] by resolving the torrent's file list via a
/// list-only add. If the file list can't be resolved (e.g. a magnet that
/// can't be fetched right now), the entry is created with no files so it will
/// be added with all files selected.
pub async fn build_entry(
    client: &dyn TorrentClient,
    source: UiAddSource,
) -> Result<AddTorrentEntry, rqbit_protocol::ProtoError> {
    let response = client
        .add_torrent(&AddTorrentRequest {
            source: source.clone(),
            only_files: None,
            overwrite: false,
            paused: false,
            list_only: true,
            source_hint: None,
        })
        .await?;
    let name = response
        .details
        .name
        .clone()
        .unwrap_or_else(|| response.details.info_hash.clone());
    let files: Vec<FileRow> = response
        .details
        .files
        .clone()
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(idx, f)| {
            // Reconstruct the on-disk path from the output folder and the
            // torrent's relative file components, then check if it exists.
            // (Hosts without a filesystem resolve `exists` to false.)
            let mut full_path = std::path::PathBuf::from(&response.output_folder);
            for component in &f.components {
                full_path.push(component);
            }
            let exists = !f.padding && full_path.exists();
            FileRow {
                file_index: idx,
                name: f.name.clone(),
                length: f.length,
                included: f.included,
                exists,
                progress: 0,
                // Freshly resolved torrents have no hook data yet.
                custom: Default::default(),
            }
        })
        .collect();
    Ok(AddTorrentEntry {
        source,
        name,
        info_hash: response.details.info_hash.clone(),
        output_folder: std::path::PathBuf::from(&response.output_folder),
        source_path: None,
        files,
        hook_note: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_tone_maps_recognized_values_case_insensitively() {
        assert_eq!(status_tone("Synced"), Some(StatusTone::Success));
        assert_eq!(status_tone("OK"), Some(StatusTone::Success));
        assert_eq!(status_tone(" pending "), Some(StatusTone::Warning));
        assert_eq!(status_tone("FAILED"), Some(StatusTone::Danger));
    }

    #[test]
    fn status_tone_leaves_unrecognized_values_alone() {
        assert_eq!(status_tone(""), None);
        assert_eq!(status_tone("some bespoke state"), None);
        assert_eq!(status_tone("42"), None);
    }
}
