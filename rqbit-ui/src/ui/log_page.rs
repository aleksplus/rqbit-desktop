use std::{collections::VecDeque, sync::Arc, time::Duration};

use futures::{FutureExt as _, StreamExt as _};
use gpui_kit::base::input::RopeExt as _;
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, Size,
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputState, Textarea, TextareaState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::client::TorrentClient;

/// Maximum number of log lines kept in the viewer (and re-synced into the
/// textarea). Older lines are dropped from the front.
const MAX_LINES: usize = 10_000;

/// Formatted lines collected per wake before applying what was collected. A
/// lagging broadcast receiver replays the whole channel ring as
/// immediately-ready items; without a cap a single wake would monopolize the
/// main thread with one huge textarea insert. At the producer's
/// one-line-per-chunk cadence this is 64 chunks per batch.
const MAX_DRAIN_CHUNKS: usize = 64;

/// Pause between capped drains while the stream stays saturated, so a lag
/// storm cannot turn this task into a back-to-back main-thread loop and
/// starve the rest of the foreground executor.
const DRAIN_COOLDOWN: Duration = Duration::from_millis(50);

/// Events emitted by [`LogPage`] to communicate with the parent view.
#[derive(Clone, Debug)]
pub enum LogPageEvent {
    /// User clicked **← Back** – close the log viewer page.
    Back,
}

/// A full-page live log viewer.
///
/// Subscribes to the client's log line stream (`Api::api_log_lines_stream`
/// locally; the `SubscribeLogs` push channel on remote clients) and appends
/// formatted lines to a readonly [`Textarea`]. While the cursor sits at the
/// end of the text the view follows the tail; if the user scrolls up or makes
/// a selection, incoming lines are only counted (shown as a "N new lines"
/// button) until the user resumes, so reading and copying are not disturbed.
///
/// Dropping the page drops the stream task and with it the subscription, so
/// log production short-circuits while the viewer is closed.
pub struct LogPage {
    client: Arc<dyn TorrentClient>,
    textarea: Entity<TextareaState>,
    /// RUST_LOG value editor for `Api::api_set_rust_log`.
    rust_log_input: Entity<InputState>,
    /// The full desired log content, capped at [`MAX_LINES`] lines. Lines
    /// always end with a newline, so the last rope line is empty.
    lines: VecDeque<String>,
    /// Lines streamed since the user paused tail-following; applied when the
    /// user clicks the "N new lines" button.
    unapplied: usize,
    /// Error text rendered under the toolbar.
    error: Option<SharedString>,
    focus_handle: FocusHandle,
    /// Keeps the log stream consumer alive for the lifetime of the page.
    _stream_task: Task<()>,
}

impl EventEmitter<LogPageEvent> for LogPage {}

impl LogPage {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        client: Arc<dyn TorrentClient>,
    ) -> Self {
        let textarea = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx)
                .placeholder("Waiting for live logs…")
                .soft_wrap(true);
            state.set_readonly(true, cx);
            state
        });
        let rust_log_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        });

        let weak = cx.entity().downgrade();
        let stream_client = client.clone();
        let stream_task = window.spawn(cx, async move |cx| {
            let mut stream = match stream_client.log_lines() {
                Ok(s) => s,
                Err(e) => {
                    let _ = cx.update(|_, cx| {
                        weak.update(cx, |page: &mut LogPage, cx| {
                            page.error = Some(format!("Log streaming is unavailable: {e}").into());
                            cx.notify();
                        })
                    });
                    return;
                }
            };
            let mut ended = false;
            while !ended {
                // Block until the first chunk arrives, then drain up to
                // MAX_DRAIN_CHUNKS of the chunks that are already ready so
                // each wake applies a single batch.
                let mut batch: Vec<String> = Vec::new();
                match stream.next().await {
                    Some(bytes) => format_chunk(&bytes, &mut batch),
                    None => break,
                }
                let mut saturated = false;
                loop {
                    match stream.next().now_or_never() {
                        None => break, // nothing more ready right now
                        Some(Some(bytes)) => {
                            format_chunk(&bytes, &mut batch);
                            if batch.len() >= MAX_DRAIN_CHUNKS {
                                // More may be pending; apply this batch now
                                // and pace the rest (see DRAIN_COOLDOWN).
                                saturated = true;
                                break;
                            }
                        }
                        Some(None) => {
                            ended = true;
                            break;
                        }
                    }
                }
                let res = cx.update(|window, cx| {
                    weak.update(cx, |page: &mut LogPage, cx| {
                        page.apply_lines(batch, window, cx)
                    })
                });
                if res.is_err() {
                    // Window or page is gone; stop consuming.
                    break;
                }
                if saturated && !ended {
                    cx.background_executor().timer(DRAIN_COOLDOWN).await;
                }
            }
        });

        Self {
            client,
            textarea,
            rust_log_input,
            lines: VecDeque::new(),
            unapplied: 0,
            error: None,
            focus_handle: cx.focus_handle(),
            _stream_task: stream_task,
        }
    }

    fn on_back(&mut self, cx: &mut Context<Self>) {
        cx.emit(LogPageEvent::Back);
    }

    fn on_clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.lines.clear();
        self.unapplied = 0;
        self.textarea
            .update(cx, |state, cx| state.set_value("", window, cx));
        cx.notify();
    }

    /// Resume tail-following after the user paused by scrolling up.
    fn on_resume(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_textarea_to_lines(window, cx);
        cx.notify();
    }

    fn on_apply_rust_log(&mut self, cx: &mut Context<Self>) {
        let value = self.rust_log_input.read(cx).value().to_string();
        if value.trim().is_empty() {
            self.error = Some("RUST_LOG cannot be empty".into());
            cx.notify();
            return;
        }
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let result = client.set_rust_log(value).await;
            let _ = this.update(cx, |this, cx| {
                this.error = result
                    .err()
                    .map(|e| format!("Failed to set RUST_LOG: {e}").into());
                cx.notify();
            });
        })
        .detach();
    }

    /// Whether the textarea cursor sits at the very end of the text, i.e. the
    /// view is tail-following.
    fn cursor_at_end(&self, cx: &App) -> bool {
        let textarea = self.textarea.read(cx);
        textarea.cursor() == textarea.text().len()
    }

    /// Apply a batch of freshly streamed lines.
    ///
    /// While following, the batch is inserted at the cursor (which is at the
    /// end); the textarea moves the cursor past the inserted text and scrolls
    /// it into view, sticking the view to the tail. Otherwise the lines are
    /// only buffered and counted until the user resumes.
    fn apply_lines(&mut self, batch: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        if batch.is_empty() {
            return;
        }
        let following = self.unapplied == 0 && self.cursor_at_end(cx);
        for line in &batch {
            self.lines.push_back(line.clone());
        }
        let mut trimmed = 0;
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
            trimmed += 1;
        }

        if following {
            if trimmed > 0 {
                // The head of the content changed under the textarea;
                // rewrite it wholesale and re-pin to the tail.
                self.sync_textarea_to_lines(window, cx);
            } else {
                let mut text = String::new();
                for line in &batch {
                    text.push_str(line);
                    text.push('\n');
                }
                self.textarea
                    .update(cx, |state, cx| state.insert(text, window, cx));
            }
        } else {
            self.unapplied += batch.len();
        }
        cx.notify();
    }

    /// Replace the textarea content with `lines` and move the cursor to the
    /// end, which scrolls the tail into view and resumes tail-following.
    fn sync_textarea_to_lines(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut text = String::new();
        for line in &self.lines {
            text.push_str(line);
            text.push('\n');
        }
        self.textarea.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            // Move the cursor to the end of the text so the element scrolls
            // the tail into view and following resumes.
            let end = state.text().offset_to_position(state.text().len());
            state.set_cursor_position(end, window, cx);
        });
        self.unapplied = 0;
    }
}

// ── Line formatting ─────────────────────────────────────────────────────────

/// Split a streamed chunk into lines and format each one.
///
/// The stream producer flushes one JSON line per event, but be defensive
/// about chunks containing multiple (or partial trailing) lines.
fn format_chunk(chunk: &[u8], out: &mut Vec<String>) {
    for line in chunk.split(|&b| b == b'\n') {
        let line: &[u8] = if line.last() == Some(&b'\r') {
            &line[..line.len() - 1]
        } else {
            line
        };
        if !line.is_empty() {
            out.push(format_log_line(line));
        }
    }
}

/// Format one JSON log line (tracing-subscriber JSON format) into a readable
/// single line:
/// `HH:MM:SS.mmm LEVEL [target] message extra=value`.
/// Non-JSON input is passed through as-is.
fn format_log_line(line: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
        return String::from_utf8_lossy(line).into_owned();
    };
    let level = value.get("level").and_then(|v| v.as_str()).unwrap_or("?");
    let target = value.get("target").and_then(|v| v.as_str()).unwrap_or("");
    let timestamp = value
        .get("timestamp")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let fields = value.get("fields");
    let message = fields
        .and_then(|f| f.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("");

    let mut out = format!(
        "{} {:>5} [{}] {}",
        compact_timestamp(timestamp),
        level,
        target,
        message
    );
    if let Some(fields) = fields.and_then(|f| f.as_object()) {
        for (k, v) in fields {
            if k == "message" {
                continue;
            }
            match v {
                serde_json::Value::String(s) => out.push_str(&format!(" {k}={s}")),
                other => out.push_str(&format!(" {k}={other}")),
            }
        }
    }
    out
}

/// Reduce an RFC3339 timestamp (`2023-12-08T21:48:13.649165Z`) to its time
/// part (`21:48:13.649`).
fn compact_timestamp(timestamp: &str) -> &str {
    let Some((_, time)) = timestamp.split_once('T') else {
        return timestamp;
    };
    match time.as_bytes().get(..12) {
        Some(bytes) => std::str::from_utf8(bytes).unwrap_or(time),
        None => time,
    }
}

// ── Render ─────────────────────────────────────────────────────────────────

impl Render for LogPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let unapplied = self.unapplied;
        let error = self.error.clone();

        v_flex()
            .size_full()
            .gap_0()
            // Toolbar
            .child(
                h_flex()
                    .gap_2()
                    .pl(px(10.))
                    .pt(px(5.))
                    .pr(px(10.))
                    .pb(px(5.))
                    .bg(theme.background)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("log-back")
                            .small()
                            .label("← Back")
                            .on_click(cx.listener(|this, _, _, cx| this.on_back(cx))),
                    )
                    .child(
                        Button::new("log-clear")
                            .small()
                            .label("Clear")
                            .on_click(cx.listener(|this, _, window, cx| this.on_clear(window, cx))),
                    )
                    .when(unapplied > 0, |this| {
                        this.child(
                            Button::new("log-resume")
                                .small()
                                .primary()
                                .label(format!("↓ {unapplied} new lines"))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.on_resume(window, cx)),
                                ),
                        )
                    })
                    .child(
                        h_flex()
                            .flex_1()
                            .justify_end()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("RUST_LOG (stderr):"),
                            )
                            .child(
                                Input::new(&self.rust_log_input)
                                    .with_size(Size::Small)
                                    .min_w(px(200.))
                                    .w(px(200.)),
                            )
                            .child(
                                Button::new("log-apply")
                                    .small()
                                    .primary()
                                    .label("Apply")
                                    .tooltip(
                                        "Reloads the stderr log filter. The streamed view keeps \
                                         its own fixed filter (info,librqbit=debug).",
                                    )
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.on_apply_rust_log(cx)),
                                    ),
                            ),
                    ),
            )
            // Error banner
            .when_some(error, |this, error| {
                this.child(
                    h_flex()
                        .id("log-error")
                        .px_4()
                        .py_2()
                        .bg(theme.danger_foreground.opacity(0.1))
                        .border_b_1()
                        .border_color(theme.danger)
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.danger_foreground)
                                .child(error),
                        ),
                )
            })
            // Log content
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .p_2()
                    .child(Textarea::new(&self.textarea).readonly(true).size_full()),
            )
    }
}

impl Focusable for LogPage {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    // NOTE: no `use super::*` here — it would pull gpui's own `#[test]`
    // attribute macro (re-exported via `use gpui::*` above) into scope and
    // shadow the builtin `#[test]`.
    use super::{compact_timestamp, format_chunk, format_log_line};

    #[test]
    fn formats_json_log_line() {
        let line = br#"{"timestamp":"2023-12-08T21:48:13.649165Z","level":"DEBUG","fields":{"message":"announce","tracker":"http://tracker/announce"},"target":"librqbit_tracker_comms"}"#;
        assert_eq!(
            format_log_line(line),
            "21:48:13.649 DEBUG [librqbit_tracker_comms] announce tracker=http://tracker/announce"
        );
    }

    #[test]
    fn formats_non_string_fields() {
        let line = br#"{"timestamp":"2023-12-08T21:48:13.649165Z","level":"INFO","fields":{"message":"done","count":3},"target":"t"}"#;
        assert_eq!(format_log_line(line), "21:48:13.649  INFO [t] done count=3");
    }

    #[test]
    fn passes_through_non_json() {
        assert_eq!(format_log_line(b"plain text"), "plain text");
    }

    #[test]
    fn handles_missing_fields() {
        // No timestamp/target/message: the line still renders.
        let line = br#"{"level":"WARN"}"#;
        assert_eq!(format_log_line(line), "  WARN [] ");
        // No level at all.
        assert_eq!(format_log_line(b"{}"), "     ? [] ");
    }

    #[test]
    fn splits_chunks_into_lines() {
        let mut out = Vec::new();
        format_chunk(
            b"{\"level\":\"INFO\"}\r\n{\"level\":\"DEBUG\"}\n\ntrailing",
            &mut out,
        );
        assert_eq!(out.len(), 3);
        assert_eq!(out[2], "trailing");
    }

    #[test]
    fn compacts_timestamps() {
        assert_eq!(
            compact_timestamp("2023-12-08T21:48:13.649165Z"),
            "21:48:13.649"
        );
        assert_eq!(compact_timestamp("21:48:13Z"), "21:48:13Z");
        assert_eq!(compact_timestamp("not-a-timestamp"), "not-a-timestamp");
    }
}
