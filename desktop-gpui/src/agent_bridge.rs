//! gpui-agent server and GPUI UI-thread adapter.
//!
//! Compiled only with `agent-testing`. The TCP thread authenticates requests
//! and posts plain data to [`AgentMailbox`]; this module drains that mailbox
//! from an application-lifetime GPUI task, including while the main window is
//! closed to the tray.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gpui_agent::{
    AgentHost, AgentMailbox, AssertSpec, DeliveryMode, DispatchResult, HelloAuth, HelloInfo,
    KeybindingInfo, KeybindingScope, MailboxRequest, Op, PlatformKind, Response, ScreenshotMode,
    UiNode, UiTree,
};
use gpui_kit::*;
use rqbit_ui::ui::agent::{AutomationNode, AutomationRole, AutomationSnapshot, PaintedBounds};
use rqbit_ui::ui::main_panel::MainPanel;

use crate::{About, MainWindow, Open, Quit, SharedMainWindow};

const MAX_PENDING_WAITS: usize = 32;
const MAX_DISPATCH_PER_TICK: usize = 8;
const MAX_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const MAILBOX_POLL_INTERVAL: Duration = Duration::from_millis(16);

fn reply_error(posted: MailboxRequest, error: impl Into<String>) {
    let id = posted.request.id.clone();
    posted.reply(Response::err(id, error));
}

fn reply_ok(posted: MailboxRequest) {
    let id = posted.request.id.clone();
    posted.reply(Response::ok(id));
}

#[derive(Debug)]
pub enum AgentStartError {
    Security(String),
    Io(io::Error),
}

impl std::fmt::Display for AgentStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Security(error) => {
                write!(f, "gpui-agent security configuration refused: {error}")
            }
            Self::Io(error) => write!(f, "gpui-agent server failed: {error}"),
        }
    }
}

impl std::error::Error for AgentStartError {}

impl From<io::Error> for AgentStartError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Shared server ownership. Clones refer to the same listener and join slot.
#[derive(Clone)]
pub struct AgentControl {
    mailbox: AgentMailbox,
    shutdown: Arc<AtomicBool>,
    accept_thread: Arc<Mutex<Option<JoinHandle<()>>>>,
    fixture: bool,
}

impl AgentControl {
    /// Read upstream's opt-in/release/address gates, then additionally require
    /// token authentication and loopback even if upstream escape flags are set.
    pub fn from_env(fixture: bool) -> Result<Option<Arc<Self>>, AgentStartError> {
        let Some(config) =
            gpui_agent::from_env().map_err(|error| AgentStartError::Security(error.to_string()))?
        else {
            return Ok(None);
        };
        if config.token.as_deref().unwrap_or("").is_empty() {
            return Err(AgentStartError::Security(
                "GPUI_AGENT_TOKEN must be non-empty for rqbit agent testing".into(),
            ));
        }
        gpui_agent::ensure_loopback(config.addr)
            .map_err(|error| AgentStartError::Security(error.to_string()))?;

        let app = app_name(fixture);
        let server = gpui_agent::AgentServer::bind(config.addr, config.token)?.with_app(app);
        let bound = server.local_addr()?;
        let shutdown = server.shutdown_handle();
        let mailbox = AgentMailbox::new();
        let server_mailbox = mailbox.clone();
        let accept_thread = std::thread::Builder::new()
            .name("gpui-agent-accept".into())
            .spawn(move || server.serve_mailbox(server_mailbox, Duration::from_secs(30)))?;
        tracing::info!(%bound, app, fixture, "gpui-agent listening with token authentication");
        Ok(Some(Arc::new(Self {
            mailbox,
            shutdown,
            accept_thread: Arc::new(Mutex::new(Some(accept_thread))),
            fixture,
        })))
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Idempotently stop accepting work and fail requests that have not yet
    /// entered a deferred UI operation. The accept thread is joined off the UI
    /// thread by [`AgentControl::join`].
    pub fn begin_shutdown(&self) {
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }
        for posted in self.mailbox.take() {
            reply_error(posted, "application is shutting down");
        }
    }

    pub async fn join(&self) {
        let handle = self.accept_thread.lock().unwrap().take();
        if let Some(handle) = handle {
            let _ = tokio::task::spawn_blocking(move || handle.join()).await;
        }
    }
}

/// Start the application-lifetime pump. A 16 ms timer is required because
/// `AgentMailbox::push` has no GPUI wake callback of its own.
pub fn start_driver(control: Arc<AgentControl>, main_window: SharedMainWindow, cx: &mut App) {
    cx.spawn(async move |cx| {
        let mut backlog: VecDeque<MailboxRequest> = VecDeque::new();
        let mut pending_waits: Vec<PendingWait> = Vec::new();
        loop {
            if control.is_shutdown() {
                for posted in backlog {
                    reply_error(posted, "application is shutting down");
                }
                for wait in pending_waits {
                    reply_error(wait.posted, "application is shutting down");
                }
                break;
            }

            backlog.extend(control.mailbox.take());
            let mut dispatch_budget = MAX_DISPATCH_PER_TICK;
            while dispatch_budget > 0
                && let Some(posted) = backlog.pop_front()
            {
                dispatch_budget -= 1;
                if matches!(
                    posted.request.op,
                    Op::Wait {
                        timeout_ms: Some(_)
                    } | Op::WaitUntil { .. }
                ) {
                    if pending_waits.len() >= MAX_PENDING_WAITS {
                        reply_error(posted, "too many pending agent waits");
                        continue;
                    }
                    if let Some(wait) = PendingWait::new(posted) {
                        pending_waits.push(wait);
                    }
                    continue;
                }

                let shutdown = matches!(posted.request.op, Op::Shutdown);
                let response =
                    cx.update(|cx| dispatch_request(&posted, &main_window, &control, cx));
                posted.reply(response);
                if shutdown {
                    cx.update(crate::request_quit);
                }
            }

            let mut still_pending = Vec::new();
            for mut wait in pending_waits.drain(..) {
                if Instant::now() >= wait.deadline {
                    let message = wait.timeout_message();
                    reply_error(wait.posted, message);
                    continue;
                }
                match cx.update(|cx| wait.evaluate(&main_window, control.fixture, cx)) {
                    WaitEvaluation::Complete => reply_ok(wait.posted),
                    WaitEvaluation::Pending(error) => {
                        wait.last_error = error;
                        still_pending.push(wait);
                    }
                }
            }
            pending_waits = still_pending;

            cx.background_executor().timer(MAILBOX_POLL_INTERVAL).await;
        }
    })
    .detach();
}

struct PendingWait {
    posted: MailboxRequest,
    kind: PendingWaitKind,
    deadline: Instant,
    last_error: String,
}

enum PendingWaitKind {
    Ready,
    Assert(AssertSpec),
}

enum WaitEvaluation {
    Complete,
    Pending(String),
}

impl PendingWait {
    fn new(posted: MailboxRequest) -> Option<Self> {
        let (kind, timeout_ms) = match &posted.request.op {
            Op::Wait { timeout_ms: None } => return None,
            Op::Wait {
                timeout_ms: Some(timeout_ms),
            } => (PendingWaitKind::Ready, *timeout_ms),
            Op::WaitUntil { timeout_ms, spec } => {
                (PendingWaitKind::Assert(spec.clone()), *timeout_ms)
            }
            _ => return None,
        };
        let timeout = Duration::from_millis(timeout_ms).min(MAX_WAIT_TIMEOUT);
        Some(Self {
            posted,
            kind,
            deadline: Instant::now() + timeout,
            last_error: "condition has not matched yet".into(),
        })
    }

    fn evaluate(
        &mut self,
        main_window: &SharedMainWindow,
        fixture: bool,
        cx: &mut App,
    ) -> WaitEvaluation {
        let tree = current_tree(main_window, fixture, cx);
        match &self.kind {
            PendingWaitKind::Ready if tree.ready => WaitEvaluation::Complete,
            PendingWaitKind::Ready => WaitEvaluation::Pending("application is not ready".into()),
            PendingWaitKind::Assert(spec) => match gpui_agent::assert_tree(&tree, spec) {
                Ok(()) => WaitEvaluation::Complete,
                Err(error) => WaitEvaluation::Pending(error),
            },
        }
    }

    fn timeout_message(&self) -> String {
        match &self.kind {
            PendingWaitKind::Ready => {
                "timed out waiting for the application to become ready".into()
            }
            PendingWaitKind::Assert(_) => {
                format!("wait-until timed out: {}", self.last_error)
            }
        }
    }
}

fn dispatch_request(
    posted: &MailboxRequest,
    main_window: &SharedMainWindow,
    control: &AgentControl,
    cx: &mut App,
) -> Response {
    let request = &posted.request;
    if matches!(request.op, Op::Shutdown) {
        return Response::ok(&request.id);
    }
    if matches!(request.op, Op::Keybinding { .. }) {
        return dispatch_keybinding(request, main_window, cx);
    }
    if let Op::Invoke { name, .. } = &request.op
        && matches!(name.as_str(), "fixture.close_window" | "app.close_window")
    {
        if name == "fixture.close_window" && !control.fixture {
            return Response::err(
                &request.id,
                "fixture.close_window is available only in the isolated fixture",
            );
        }
        return with_current_window(main_window, cx, |window, _cx, _panel| {
            window.remove_window();
            Response::ok(&request.id)
        })
        .unwrap_or_else(|| windowless_response(request, control.fixture));
    }
    if request.op.is_virtual_input() {
        return with_current_window(main_window, cx, |window, cx, panel| {
            dispatch_virtual_request(panel, window, cx, request, control.fixture)
        })
        .unwrap_or_else(|| windowless_response(request, control.fixture));
    }

    with_current_panel(main_window, cx, |panel, window, cx| {
        dispatch_in_window(panel, window, cx, request, control.fixture)
    })
    .unwrap_or_else(|| windowless_response(request, control.fixture))
}

fn dispatch_in_window(
    panel: &mut MainPanel,
    window: &mut Window,
    cx: &mut Context<MainPanel>,
    request: &gpui_agent::Request,
    fixture: bool,
) -> Response {
    let tree = convert_snapshot(panel.agent_snapshot(fixture, cx));
    match &request.op {
        Op::Hello
        | Op::Wait { timeout_ms: None }
        | Op::Snapshot { .. }
        | Op::Revision
        | Op::Assert { .. }
        | Op::Keybindings
        | Op::Events { .. } => readonly_response(tree, fixture, request),
        Op::Click {
            target,
            delivery,
            expect_fingerprint,
        } => {
            debug_assert!(delivery.is_semantic());
            let result = validate_target(&tree, target, *expect_fingerprint)
                .and_then(|()| panel.agent_click(target, window, cx))
                .map(|()| DispatchResult::empty());
            dispatch_response(&request.id, result)
        }
        Op::Type {
            target,
            text,
            delivery,
            expect_fingerprint,
        } => {
            debug_assert!(delivery.is_semantic());
            let result = validate_target(&tree, target, *expect_fingerprint).and_then(|()| {
                panel
                    .agent_type_text(target, text, window, cx)
                    .map(|()| DispatchResult::empty())
            });
            dispatch_response(&request.id, result)
        }
        Op::SetValue {
            target,
            value,
            expect_fingerprint,
        } => {
            let result = validate_target(&tree, target, *expect_fingerprint)
                .and_then(|()| panel.agent_set_value(target, value, window, cx))
                .map(|()| DispatchResult::empty());
            dispatch_response(&request.id, result)
        }
        Op::Key {
            target,
            key,
            delivery,
            expect_fingerprint,
        } => {
            debug_assert!(delivery.is_semantic());
            let result = validate_target(&tree, target, *expect_fingerprint)
                .and_then(|()| panel.agent_key(target, key, window, cx))
                .map(|()| DispatchResult::empty());
            dispatch_response(&request.id, result)
        }
        Op::Screenshot { path, mode, .. } => {
            dispatch_response(&request.id, screenshot(window, path.as_deref(), mode))
        }
        Op::Invoke { name, .. } => Response::err(
            &request.id,
            format!("invoke `{name}` is not allow-listed by rqbit"),
        ),
        Op::Wait { .. } | Op::WaitUntil { .. } => Response::err(
            &request.id,
            "internal error: wait must be handled by the application driver",
        ),
        Op::Keybinding { .. } | Op::Shutdown => Response::err(
            &request.id,
            "internal error: application-level op reached the window adapter",
        ),
    }
}

fn windowless_response(request: &gpui_agent::Request, fixture: bool) -> Response {
    let tree = unavailable_tree(fixture);
    match &request.op {
        Op::Hello
        | Op::Wait { timeout_ms: None }
        | Op::Snapshot { .. }
        | Op::Revision
        | Op::Assert { .. }
        | Op::Keybindings
        | Op::Events { .. } => readonly_response(tree, fixture, request),
        Op::Screenshot { .. } => Response::err(
            &request.id,
            gpui_agent::screenshot_unavailable("main window is closed to the tray"),
        ),
        op if op.is_virtual_input() => Response::err(
            &request.id,
            gpui_agent::virtual_unavailable("main window is closed to the tray"),
        ),
        _ => Response::err(
            &request.id,
            "window_unavailable: main window is closed to the tray",
        ),
    }
}

fn with_current_panel<R>(
    main_window: &SharedMainWindow,
    cx: &mut App,
    f: impl FnOnce(&mut MainPanel, &mut Window, &mut Context<MainPanel>) -> R,
) -> Option<R> {
    let current = main_window.lock().unwrap().clone();
    current.and_then(|current: MainWindow| {
        current
            .handle
            .update(cx, |_, window, cx| {
                current
                    .main_panel
                    .update(cx, |panel, cx| f(panel, window, cx))
                    .ok()
            })
            .ok()
            .flatten()
    })
}

fn with_current_window<R>(
    main_window: &SharedMainWindow,
    cx: &mut App,
    f: impl FnOnce(&mut Window, &mut App, Entity<MainPanel>) -> R,
) -> Option<R> {
    let current = main_window.lock().unwrap().clone();
    current.and_then(|current: MainWindow| {
        current
            .handle
            .update(cx, |_, window, cx| {
                current
                    .main_panel
                    .upgrade()
                    .map(|panel| f(window, cx, panel))
            })
            .ok()
            .flatten()
    })
}

fn current_tree(main_window: &SharedMainWindow, fixture: bool, cx: &mut App) -> UiTree {
    with_current_panel(main_window, cx, |panel, _window, cx| {
        convert_snapshot(panel.agent_snapshot(fixture, cx))
    })
    .unwrap_or_else(|| unavailable_tree(fixture))
}

struct SnapshotHost {
    hello: HelloInfo,
    tree: UiTree,
}

impl AgentHost for SnapshotHost {
    fn hello(&self) -> HelloInfo {
        self.hello.clone()
    }

    fn snapshot(&self) -> UiTree {
        self.tree.clone()
    }

    fn dispatch(&mut self, op: &Op) -> Result<DispatchResult, String> {
        Err(format!(
            "operation {op:?} is not a read-only snapshot operation"
        ))
    }

    fn keybindings(&self) -> Vec<KeybindingInfo> {
        keybinding_catalog()
    }

    fn is_app_focused(&self) -> bool {
        false
    }
}

fn readonly_response(tree: UiTree, fixture: bool, request: &gpui_agent::Request) -> Response {
    let mut host = SnapshotHost {
        hello: hello_info(fixture, tree.ready),
        tree,
    };
    let mut response = gpui_agent::handle_request(&mut host, request.clone(), None, None);
    if let Some(hello) = response.hello.as_mut() {
        hello.auth = HelloAuth::Required;
    }
    response
}

fn hello_info(fixture: bool, ready: bool) -> HelloInfo {
    let identity = gpui_agent::discovery::detect_identity();
    HelloInfo {
        protocol: gpui_agent::PROTOCOL_VERSION,
        app: app_name(fixture).into(),
        platform: PlatformKind::Desktop,
        os: gpui_agent::protocol::host_os(),
        ready,
        deliveries: vec![DeliveryMode::Semantic, DeliveryMode::Virtual],
        auth: HelloAuth::Required,
        bundle_id: identity.bundle_id.or(identity.connect_id),
        executable: Some(identity.executable),
        features: Vec::new(),
    }
}

fn unavailable_tree(fixture: bool) -> UiTree {
    UiTree {
        app: app_name(fixture).into(),
        platform: PlatformKind::Desktop,
        ready: false,
        nodes: vec![
            UiNode::window("rqbit-window", "rqbit")
                .with_enabled(false)
                .with_visible_deep(false),
        ],
        rev: None,
    }
}

fn app_name(fixture: bool) -> &'static str {
    if fixture {
        "rqbit-desktop-fixture"
    } else {
        "rqbit-desktop"
    }
}

fn convert_snapshot(snapshot: AutomationSnapshot) -> UiTree {
    UiTree {
        app: snapshot.app,
        platform: PlatformKind::Desktop,
        ready: snapshot.ready,
        nodes: snapshot.nodes.into_iter().map(convert_node).collect(),
        rev: None,
    }
}

fn convert_node(node: AutomationNode) -> UiNode {
    let role = match node.role {
        AutomationRole::Window => "window",
        AutomationRole::Page => "page",
        AutomationRole::Navigation => "navigation",
        AutomationRole::Button => "button",
        AutomationRole::TextBox => "textbox",
        AutomationRole::List => "list",
        AutomationRole::ListItem => "listitem",
        AutomationRole::Scroll => "scroll",
        AutomationRole::Checkbox => "checkbox",
        AutomationRole::Note => "note",
        AutomationRole::Status => "status",
        AutomationRole::Dialog => "dialog",
    };
    let mut converted = UiNode::new(node.id, role, node.name)
        .with_enabled(node.enabled)
        .with_focused(node.focused)
        .with_visible(node.visible)
        .with_children(node.children.into_iter().map(convert_node).collect());
    converted.value = node.value;
    if let Some(checked) = node.checked {
        converted = converted.with_checked(checked);
    }
    if let Some(bounds) = node.bounds {
        converted = converted.with_bounds(convert_bounds(bounds));
    }
    converted
}

fn convert_bounds(bounds: PaintedBounds) -> gpui_agent::Bounds {
    gpui_agent::Bounds {
        x: bounds.x,
        y: bounds.y,
        w: bounds.w,
        h: bounds.h,
    }
}

fn validate_target(
    tree: &UiTree,
    target: &str,
    expected_fingerprint: Option<u64>,
) -> Result<(), String> {
    if expected_fingerprint.is_some() && tree.fingerprint_of(target).is_none() {
        return Err(format!("target_gone: `{target}` is no longer in the tree"));
    }
    let node = tree.require_id(target)?;
    if !node.visible {
        return Err(format!("target `{target}` is not visible"));
    }
    if !node.enabled {
        return Err(format!("target `{target}` is disabled"));
    }
    if let Some(expected) = expected_fingerprint {
        let Some(actual) = tree.fingerprint_of(target) else {
            return Err(format!("target_gone: `{target}` is no longer in the tree"));
        };
        if actual != expected {
            return Err(format!(
                "stale_target: `{target}` changed between snapshot and dispatch"
            ));
        }
    }
    Ok(())
}

fn dispatch_response(id: &str, result: Result<DispatchResult, String>) -> Response {
    match result {
        Ok(result) => {
            let mut response = Response::ok(id);
            response.result = result.value;
            response
        }
        Err(error) => Response::err(id, error),
    }
}

fn dispatch_virtual_request(
    panel: Entity<MainPanel>,
    window: &mut Window,
    cx: &mut App,
    request: &gpui_agent::Request,
    fixture: bool,
) -> Response {
    let tree = panel.update(cx, |panel, cx| {
        convert_snapshot(panel.agent_snapshot(fixture, cx))
    });
    let result = match &request.op {
        Op::Click {
            target,
            expect_fingerprint,
            ..
        } => validate_target(&tree, target, *expect_fingerprint)
            .and_then(|()| virtual_click(&tree, target, window, cx)),
        Op::Type {
            target,
            text,
            expect_fingerprint,
            ..
        } => validate_target(&tree, target, *expect_fingerprint)
            .and_then(|()| virtual_type(&panel, &tree, target, text, window, cx)),
        Op::Key {
            target,
            key,
            expect_fingerprint,
            ..
        } => validate_target(&tree, target, *expect_fingerprint)
            .and_then(|()| virtual_key(&panel, &tree, target, key, window, cx)),
        _ => Err(gpui_agent::virtual_unavailable(
            "only click, type, and key support virtual delivery",
        )),
    };
    dispatch_response(&request.id, result)
}

fn virtual_click(
    tree: &UiTree,
    target: &str,
    window: &mut Window,
    cx: &mut App,
) -> Result<DispatchResult, String> {
    let plan = gpui_agent::plan_click(tree, target)?;
    let root = tree.require_id("rqbit-window")?.bounds;
    if plan.x < root.x || plan.y < root.y || plan.x > root.x + root.w || plan.y > root.y + root.h {
        return Err(gpui_agent::virtual_unavailable(format!(
            "target `{target}` is outside the application window"
        )));
    }
    dispatch_pointer_click(plan.x, plan.y, window, cx);
    Ok(DispatchResult::json(serde_json::json!({
        "delivery": "virtual",
        "target": target,
        "x": plan.x,
        "y": plan.y,
        "path": "gpui.dispatch_event",
    })))
}

fn virtual_type(
    panel: &Entity<MainPanel>,
    tree: &UiTree,
    target: &str,
    text: &str,
    window: &mut Window,
    cx: &mut App,
) -> Result<DispatchResult, String> {
    // Focus before injecting events. Mouse delivery can synchronously update
    // MainPanel through InputEvent subscriptions, so no panel borrow may be
    // held while Window::dispatch_event or dispatch_keystroke runs.
    panel.update(cx, |panel, cx| panel.agent_focus(target, window, cx))?;
    virtual_click(tree, target, window, cx)?;
    for token in gpui_agent::text_keystrokes(text)? {
        let keystroke = Keystroke::parse_with_os(&token, cx.platform_info().os)
            .map_err(|error| error.to_string())?;
        window.dispatch_keystroke(keystroke, cx);
    }
    Ok(DispatchResult::json(serde_json::json!({
        "delivery": "virtual",
        "target": target,
        "text": text,
        "path": "gpui.dispatch_keystroke",
    })))
}

fn virtual_key(
    panel: &Entity<MainPanel>,
    tree: &UiTree,
    target: &str,
    key: &str,
    window: &mut Window,
    cx: &mut App,
) -> Result<DispatchResult, String> {
    panel.update(cx, |panel, cx| panel.agent_focus(target, window, cx))?;
    virtual_click(tree, target, window, cx)?;
    let token = gpui_agent::keystroke_token(key)?;
    let keystroke = Keystroke::parse_with_os(&token, cx.platform_info().os)
        .map_err(|error| error.to_string())?;
    window.dispatch_keystroke(keystroke, cx);
    Ok(DispatchResult::json(serde_json::json!({
        "delivery": "virtual",
        "target": target,
        "key": key,
        "path": "gpui.dispatch_keystroke",
    })))
}

fn dispatch_pointer_click(x: f32, y: f32, window: &mut Window, cx: &mut App) {
    let position = point(px(x), px(y));
    let modifiers = Modifiers::default();
    window.dispatch_event(
        MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers,
        }
        .to_platform_input(),
        cx,
    );
    window.dispatch_event(
        MouseDownEvent {
            button: MouseButton::Left,
            position,
            modifiers,
            click_count: 1,
            first_mouse: false,
        }
        .to_platform_input(),
        cx,
    );
    window.dispatch_event(
        MouseUpEvent {
            button: MouseButton::Left,
            position,
            modifiers,
            click_count: 1,
        }
        .to_platform_input(),
        cx,
    );
}

fn keybinding_catalog() -> Vec<KeybindingInfo> {
    vec![
        KeybindingInfo::new("app.open", "", KeybindingScope::Global, false),
        KeybindingInfo::new("app.about", "", KeybindingScope::Global, false),
        KeybindingInfo::new("app.quit", "", KeybindingScope::Global, true),
    ]
}

fn dispatch_keybinding(
    request: &gpui_agent::Request,
    main_window: &SharedMainWindow,
    cx: &mut App,
) -> Response {
    let focused = main_window
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|current| {
            current
                .handle
                .update(cx, |_, window, _| window.is_window_active())
                .ok()
        })
        .unwrap_or(false);
    if let Err(error) =
        gpui_agent::authorize_keybinding_op(&request.op, &keybinding_catalog(), focused)
    {
        return Response::err(&request.id, error);
    }
    let Op::Keybinding { binding, .. } = &request.op else {
        unreachable!()
    };
    match binding.as_str() {
        "app.open" => cx.dispatch_action(&Open),
        "app.about" => cx.dispatch_action(&About),
        "app.quit" => cx.dispatch_action(&Quit),
        other => {
            return Response::err(&request.id, format!("unknown binding `{other}`"));
        }
    }
    let mut response = Response::ok(&request.id);
    response.result = Some(serde_json::json!({
        "binding": binding,
        "path": "gpui.dispatch_action",
    }));
    response
}

#[cfg(target_os = "macos")]
fn screenshot(
    window: &Window,
    path: Option<&str>,
    mode: &ScreenshotMode,
) -> Result<DispatchResult, String> {
    if matches!(mode, ScreenshotMode::Scrolled) {
        return Err(gpui_agent::scroll_unavailable(
            "scrolled capture is not implemented by rqbit",
        ));
    }
    let window_id = crate::agent_macos_window::cgwindow_id(window)?;
    gpui_agent::capture_window_via_screencapture(window_id, path)
}

#[cfg(not(target_os = "macos"))]
fn screenshot(
    _window: &Window,
    _path: Option<&str>,
    mode: &ScreenshotMode,
) -> Result<DispatchResult, String> {
    if matches!(mode, ScreenshotMode::Scrolled) {
        return Err(gpui_agent::scroll_unavailable(
            "scrolled capture is not implemented by rqbit",
        ));
    }
    Err(gpui_agent::screenshot_unavailable(
        "viewport capture is currently supported only on macOS",
    ))
}

#[cfg(test)]
mod tests {
    use super::{MAX_WAIT_TIMEOUT, PendingWait, hello_info, unavailable_tree, validate_target};
    use gpui_agent::{AgentMailbox, DeliveryMode, HelloAuth, Op};
    use std::time::Instant;

    #[test]
    fn windowless_snapshots_are_not_ready_and_have_no_revision() {
        let tree = unavailable_tree(false);
        assert!(!tree.ready);
        assert_eq!(tree.rev, None);
        assert!(tree.ids_are_unique());
        assert!(!tree.require_id("rqbit-window").unwrap().visible);
    }

    #[test]
    fn stale_target_is_rejected_before_dispatch() {
        let mut tree = unavailable_tree(false);
        tree.nodes[0].visible = true;
        tree.nodes[0].enabled = true;
        let fingerprint = tree.fingerprint_of("rqbit-window").unwrap();
        assert!(validate_target(&tree, "rqbit-window", Some(fingerprint)).is_ok());
        assert!(
            validate_target(&tree, "rqbit-window", Some(fingerprint + 1))
                .unwrap_err()
                .starts_with("stale_target")
        );
    }

    #[test]
    fn excessive_wait_deadline_is_capped_without_a_panic() {
        let mailbox = AgentMailbox::new();
        let _response = mailbox.push(gpui_agent::Request::new(
            "overflow",
            Op::Wait {
                timeout_ms: Some(u64::MAX),
            },
        ));
        let posted = mailbox.take().remove(0);
        let wait = PendingWait::new(posted).expect("large waits are clamped");
        assert!(wait.deadline <= Instant::now() + MAX_WAIT_TIMEOUT);
    }

    #[test]
    fn hello_reports_protocol_two_authentication_and_both_deliveries() {
        let hello = hello_info(false, true);
        assert_eq!(hello.protocol, 2);
        assert_eq!(hello.auth, HelloAuth::Required);
        assert!(hello.deliveries.contains(&DeliveryMode::Semantic));
        assert!(hello.deliveries.contains(&DeliveryMode::Virtual));
    }
}
