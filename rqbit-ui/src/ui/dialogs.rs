use gpui::{FocusHandle, *};
use gpui_kit::component::{
    ActiveTheme as _,
    StyledExt as _,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputState},
    v_flex,
};

/// Events emitted by [`DeleteDialog`].
#[derive(Clone, Debug)]
pub enum DeleteDialogEvent {
    /// User cancelled.
    Cancelled,
    /// User confirmed deletion with (ids, delete_files).
    Confirmed(Vec<usize>, bool),
}

/// A modal dialog for confirming torrent deletion.
pub struct DeleteDialog {
    ids: Vec<usize>,
    names: Vec<String>,
    delete_files: bool,
    focus_handle: FocusHandle,
}

impl EventEmitter<DeleteDialogEvent> for DeleteDialog {}

impl DeleteDialog {
    pub fn new(
        ids: Vec<usize>,
        names: Vec<String>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            ids,
            names,
            delete_files: true,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Whether this dialog confirms deletion of multiple torrents. Derived
    /// from `ids` so it can never drift out of sync with the actual set.
    fn is_bulk(&self) -> bool {
        self.ids.len() > 1
    }

    fn on_cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(DeleteDialogEvent::Cancelled);
    }

    fn on_confirm(&mut self, cx: &mut Context<Self>) {
        cx.emit(DeleteDialogEvent::Confirmed(
            self.ids.clone(),
            self.delete_files,
        ));
    }
}

impl Render for DeleteDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        v_flex()
            .gap_3()
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if self.is_bulk() {
                        format!("Delete {} torrents", self.ids.len())
                    } else {
                        "Delete torrent".to_string()
                    }),
            )
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
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("del-cancel")
                            .outline()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.on_cancel(cx))),
                    )
                    .child(
                        Button::new("del-ok")
                            .danger()
                            .label(if self.is_bulk() {
                                format!("Delete {} Torrents", self.ids.len())
                            } else {
                                "Delete Torrent".to_string()
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.on_confirm(cx))),
                    ),
            )
    }
}

impl Focusable for DeleteDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Events emitted by [`MagnetDialog`].
#[derive(Clone, Debug)]
pub enum MagnetDialogEvent {
    /// User clicked **Cancel**.
    Cancelled,
    /// User clicked **Add** and provided a magnet URL.
    Submitted(String),
}

/// A small dialog for entering a magnet link URL.
pub struct MagnetDialog {
    input_state: Entity<InputState>,
    focus_handle: FocusHandle,
}

impl EventEmitter<MagnetDialogEvent> for MagnetDialog {}

impl MagnetDialog {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            input_state: cx
                .new(|cx| InputState::new(window, cx).placeholder("magnet:?xt=urn:btih:...")),
            focus_handle: cx.focus_handle(),
        }
    }

    fn on_cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(MagnetDialogEvent::Cancelled);
    }

    fn on_submit(&mut self, cx: &mut Context<Self>) {
        let value = self.input_state.read(cx).value().to_string();
        if !value.trim().is_empty() {
            cx.emit(MagnetDialogEvent::Submitted(value.trim().to_string()));
        }
    }
}

impl Render for MagnetDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        v_flex()
            .gap_3()
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Add Magnet Link"),
            )
            .child(Input::new(&self.input_state))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("magnet-cancel")
                            .outline()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.on_cancel(cx))),
                    )
                    .child(
                        Button::new("magnet-add")
                            .primary()
                            .label("Add")
                            .on_click(cx.listener(|this, _, _, cx| this.on_submit(cx))),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child("Tip: paste a magnet link or a 40-char info hash."),
            )
    }
}

impl Focusable for MagnetDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Events emitted by [`AboutDialog`].
#[derive(Clone, Debug)]
pub enum AboutDialogEvent {
    /// User clicked **Close**.
    Closed,
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

/// A modal About dialog: app identity + short description, then version and
/// author rows for the rqbit engine and the desktop app itself.
pub struct AboutDialog {
    info: AboutInfo,
    focus_handle: FocusHandle,
}

impl EventEmitter<AboutDialogEvent> for AboutDialog {}

impl AboutDialog {
    pub fn new(info: AboutInfo, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            info,
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Render for AboutDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let info = &self.info;

        v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(info.app_name.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child(format!("Version {}", info.version)),
                    ),
            )
            .child(div().text_color(theme.muted_foreground).child(info.description.clone()))
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
            .child(
                h_flex().gap_2().justify_end().child(
                    Button::new("about-close")
                        .outline()
                        .label("Close")
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(AboutDialogEvent::Closed))),
                ),
            )
    }
}

impl Focusable for AboutDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
