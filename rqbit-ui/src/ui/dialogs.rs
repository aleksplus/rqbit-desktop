use gpui::*;
use gpui_kit::component::{
    ActiveTheme as _, StyledExt as _,
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputState},
    v_flex,
};

/// A modal dialog body for confirming torrent deletion.
///
/// This is only the content of the dialog: the surrounding modal (backdrop,
/// title, footer buttons, close handling) is provided by the gpui-kit
/// `Dialog` component that hosts it (see `MainPanel::create_delete_dialog`).
pub struct DeleteDialog {
    ids: Vec<usize>,
    names: Vec<String>,
    delete_files: bool,
}

impl DeleteDialog {
    pub fn new(
        ids: Vec<usize>,
        names: Vec<String>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            ids,
            names,
            delete_files: true,
        }
    }

    /// Whether this dialog confirms deletion of multiple torrents. Derived
    /// from `ids` so it can never drift out of sync with the actual set.
    pub fn is_bulk(&self) -> bool {
        self.ids.len() > 1
    }

    /// What a confirmation means: the torrent ids and whether downloaded
    /// files should be deleted along with the torrent entries.
    pub fn confirmation(&self) -> (Vec<usize>, bool) {
        (self.ids.clone(), self.delete_files)
    }

    /// Dialog title text.
    pub fn title(&self) -> String {
        if self.is_bulk() {
            format!("Delete {} torrents", self.ids.len())
        } else {
            "Delete torrent".to_string()
        }
    }

    /// Confirm-button label.
    pub fn ok_label(&self) -> String {
        if self.is_bulk() {
            format!("Delete {} Torrents", self.ids.len())
        } else {
            "Delete Torrent".to_string()
        }
    }
}

impl Render for DeleteDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        v_flex()
            .gap_3()
            .child(div().child(if self.is_bulk() {
                "Are you sure you want to delete the following torrents?"
            } else {
                "Are you sure you want to delete this torrent?"
            }))
            .child(
                div()
                    .rounded_md()
                    .bg(theme.muted)
                    .p_3()
                    .max_h(px(200.))
                    .overflow_y_hidden()
                    .children(
                        self.names
                            .iter()
                            .map(|name| div().text_color(theme.foreground).child(name.clone())),
                    ),
            )
            .child(
                Checkbox::new("delete-files")
                    .checked(self.delete_files)
                    .label("Also delete downloaded files")
                    .on_click(
                        cx.listener(|this: &mut DeleteDialog, checked: &bool, _, _| {
                            this.delete_files = *checked;
                        }),
                    ),
            )
    }
}

/// A small dialog body for entering a magnet link URL.
///
/// Content-only; the modal chrome comes from the hosting `Dialog` component
/// (see `MainPanel::on_add_magnet`).
pub struct MagnetDialog {
    input_state: Entity<InputState>,
}

impl MagnetDialog {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            input_state: cx
                .new(|cx| InputState::new(window, cx).placeholder("magnet:?xt=urn:btih:...")),
        }
    }

    /// The trimmed magnet URL entered so far, or `None` when the field is
    /// empty (the hosting dialog treats that as "don't close").
    pub fn magnet(&self, cx: &App) -> Option<String> {
        let value = self.input_state.read(cx).value().trim().to_string();
        (!value.is_empty()).then_some(value)
    }
}

impl Render for MagnetDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        v_flex().gap_3().child(Input::new(&self.input_state)).child(
            div()
                .text_size(px(12.))
                .text_color(theme.muted_foreground)
                .child("Tip: paste a magnet link or a 40-char info hash."),
        )
    }
}

/// Everything the About dialog shows, supplied by the host (the desktop build
/// resolves the values from its own crate metadata and the engine crate; the
/// web build never opens this dialog).
pub struct AboutInfo {
    /// App name, e.g. "Rqbit Desktop".
    pub app_name: SharedString,
    /// The desktop app's own version.
    pub version: SharedString,
    /// One-paragraph project description.
    pub description: SharedString,
    /// The BitTorrent engine's name ("rqbit") and its version.
    pub engine_name: SharedString,
    pub engine_version: SharedString,
    pub engine_author: SharedString,
    /// The desktop app's crate name ("rqbit-desktop") and its author. The
    /// version is shared with [`AboutInfo::version`].
    pub desktop_name: SharedString,
    pub desktop_author: SharedString,
}

/// A modal About dialog body: app version + short description, then version
/// and author rows for the rqbit engine and the desktop app itself. The app
/// name shown in the title comes from [`AboutInfo::app_name`] via the hosting
/// `Dialog` component (see `MainPanel::create_about_dialog`).
pub struct AboutDialog {
    info: AboutInfo,
}

impl AboutDialog {
    pub fn new(info: AboutInfo, _window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { info }
    }

    pub fn info(&self) -> &AboutInfo {
        &self.info
    }
}

impl Render for AboutDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let info = &self.info;

        v_flex()
            .gap_3()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(format!("Version {}", info.version)),
            )
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(info.description.clone()),
            )
            .child(div().h(px(1.)).w_full().bg(theme.border))
            .child(
                // Version/author rows: name + version on the left, author on
                // the right.
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .justify_between()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(div().font_medium().child(info.engine_name.clone()))
                                    .child(
                                        div()
                                            .text_color(theme.muted_foreground)
                                            .child(info.engine_version.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child(info.engine_author.clone()),
                            ),
                    )
                    .child(
                        h_flex()
                            .justify_between()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(div().font_medium().child(info.desktop_name.clone()))
                                    .child(
                                        div()
                                            .text_color(theme.muted_foreground)
                                            .child(info.version.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child(info.desktop_author.clone()),
                            ),
                    ),
            )
    }
}
