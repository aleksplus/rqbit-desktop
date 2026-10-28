use gpui::*;
use rqbit_ui::ui::main_panel::MainPanel;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::info;

/// Root view that wraps the main panel and implements "hold-to-quit":
/// keep cmd-q / ctrl-c held for 1 second to exit. Releasing either the key or
/// the modifier early drops the countdown task, cancelling the quit. The key
/// listeners are attached to this element, so they are registered during
/// paint (safe) and cleared each frame.
pub struct HoldToQuitView {
    pub main_panel: Entity<MainPanel>,
    pub quit_countdown: Arc<Mutex<Option<Task<()>>>>,
}

impl Render for HoldToQuitView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let quit_countdown = self.quit_countdown.clone();
        div()
            .size_full()
            .on_key_down({
                let quit_countdown = quit_countdown.clone();
                move |event: &KeyDownEvent, _window, cx| {
                    // The gesture is "hold Cmd-Q / Ctrl-C": require the
                    // secondary modifier (Cmd on macOS, Ctrl elsewhere) plus q or c.
                    if !event.keystroke.modifiers.secondary(cx)
                        || (event.keystroke.key != "q" && event.keystroke.key != "c")
                    {
                        return;
                    }
                    let mut lock = quit_countdown.lock().unwrap();
                    if lock.is_some() {
                        return; // already counting down (OS key-repeat guard)
                    }
                    info!("Hotkey held down. Keep holding for 1 second to exit…");
                    let executor = cx.background_executor().clone();
                    let task = cx.spawn(move |cx: &mut AsyncApp| {
                        let async_cx = cx.clone();
                        let executor = executor.clone();
                        async move {
                            executor.timer(Duration::from_secs(1)).await;
                            async_cx.update(|cx| {
                                info!("1 second elapsed! Exiting…");
                                cx.quit();
                            });
                        }
                    });
                    *lock = Some(task);
                }
            })
            .on_key_up({
                let quit_countdown = quit_countdown.clone();
                move |event: &KeyUpEvent, _window, _cx| {
                    // Releasing the letter key (q/c) cancels the pending quit.
                    if event.keystroke.key != "q" && event.keystroke.key != "c" {
                        return;
                    }
                    let mut lock = quit_countdown.lock().unwrap();
                    if lock.is_some() {
                        info!("Key released early! Cancellation successful.");
                        *lock = None; // dropping the task aborts the timer
                    }
                }
            })
            .on_modifiers_changed({
                let quit_countdown = quit_countdown.clone();
                move |event: &ModifiersChangedEvent, _window, cx| {
                    // Releasing the modifier (Cmd/Ctrl) first also breaks the
                    // gesture and must cancel the pending quit.
                    if event.modifiers.secondary(cx) {
                        return;
                    }
                    let mut lock = quit_countdown.lock().unwrap();
                    if lock.is_some() {
                        info!("Modifier released early! Cancellation successful.");
                        *lock = None; // dropping the task aborts the timer
                    }
                }
            })
            .child(self.main_panel.clone())
    }
}
