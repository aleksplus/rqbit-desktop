use gpui_kit::{App, Global, MenuItem};
use gpui_tray::{Icon, Tray};

use crate::{Open, Quit};

/// Keeps the native tray item alive for the lifetime of the GPUI application.
pub struct SystemTray(pub Tray);

impl Global for SystemTray {}

pub fn build(cx: &mut App) -> gpui_tray::Result<Tray> {
    Tray::builder()
        .icon(tray_icon()?)
        .tooltip("Rqbit Desktop")
        .menu(|_| tray_menu())
        .build(cx)
}

fn tray_menu() -> Vec<MenuItem> {
    vec![
        MenuItem::action("Open Rqbit Desktop", Open),
        MenuItem::separator(),
        MenuItem::action("Exit", Quit),
    ]
}

fn tray_icon() -> gpui_tray::Result<Icon> {
    let image = image::load_from_memory_with_format(
        include_bytes!("../icons/tray@128x128.png"),
        image::ImageFormat::Png,
    )
    .map_err(|error| {
        gpui_tray::Error::InvalidIcon(format!("failed to decode bundled tray icon: {error}"))
    })?
    .into_rgba8();
    let (width, height) = image.dimensions();
    let builder = Icon::from_rgba(image.into_raw(), width, height);
    #[cfg(target_os = "macos")]
    let builder = builder.scale_factor(8.0);
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_contains_open_and_exit_actions() {
        let labels = tray_menu()
            .into_iter()
            .filter_map(|item| match item {
                MenuItem::Action { name, .. } => Some(name.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(labels, ["Open Rqbit Desktop", "Exit"]);
    }

    #[test]
    fn bundled_icon_decodes_with_valid_dimensions() {
        tray_icon().expect("bundled tray icon should be valid");
    }
}
