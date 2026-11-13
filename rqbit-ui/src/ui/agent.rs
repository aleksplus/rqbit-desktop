//! Neutral UI automation models shared with the desktop agent adapter.
//!
//! These types deliberately do not depend on `gpui-agent`. Production views
//! carry a disabled registry; `agent-testing` enables painted-bounds tracking
//! and exposes snapshots to the desktop bridge without affecting wasm.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use gpui_kit::base::ElementExt as _;
use gpui_kit::{AnyElement, Bounds, IntoElement, ParentElement, Pixels, div};

pub type SharedAutomationRegistry = Arc<Mutex<AutomationRegistry>>;

#[derive(Debug, Default)]
pub struct AutomationRegistry {
    enabled: bool,
    first_paint: bool,
    bounds: HashMap<String, PaintedBounds>,
}

impl AutomationRegistry {
    pub fn disabled() -> SharedAutomationRegistry {
        Arc::new(Mutex::new(Self::default()))
    }

    pub fn enabled() -> SharedAutomationRegistry {
        Arc::new(Mutex::new(Self {
            enabled: true,
            ..Self::default()
        }))
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn first_paint(&self) -> bool {
        self.first_paint
    }

    pub fn bounds(&self, id: &str) -> Option<PaintedBounds> {
        self.bounds.get(id).copied()
    }

    /// Observe a semantic element without assigning a GPUI element ID to an
    /// extra wrapper, so the real widget retains its layout and hit-test chain.
    pub fn track<E>(registry: &SharedAutomationRegistry, id: impl Into<String>, child: E) -> E
    where
        E: ParentElement + IntoElement,
    {
        if !registry.lock().unwrap().enabled {
            return child;
        }
        let id = id.into();
        let registry = registry.clone();
        child.on_prepaint(move |bounds, _window, _cx| {
            registry.lock().unwrap().record(id.clone(), bounds);
        })
    }

    /// Track components that cannot accept children themselves. The ID-less
    /// wrapper does not replace the component's own interactive element ID.
    pub fn track_wrapped(
        registry: &SharedAutomationRegistry,
        id: impl Into<String>,
        child: impl IntoElement,
    ) -> AnyElement {
        if !registry.lock().unwrap().enabled {
            return child.into_any_element();
        }
        let id = id.into();
        let registry = registry.clone();
        div()
            .child(child)
            .on_prepaint(move |bounds, _window, _cx| {
                registry.lock().unwrap().record(id.clone(), bounds);
            })
            .into_any_element()
    }

    /// Track the window root and first paint. Child prepaint callbacks run
    /// before this root callback, so bounds must not be cleared here. Snapshots
    /// apply bounds only to nodes that still exist; hidden nodes are rejected
    /// by visibility checks before virtual input.
    pub fn track_window<E>(registry: &SharedAutomationRegistry, child: E) -> E
    where
        E: ParentElement + IntoElement,
    {
        if !registry.lock().unwrap().enabled {
            return child;
        }
        let root = registry.clone();
        child.on_prepaint(move |bounds, _window, _cx| {
            let mut registry = root.lock().unwrap();
            registry.first_paint = true;
            registry.record("rqbit-window".into(), bounds);
        })
    }

    pub fn record(&mut self, id: String, bounds: Bounds<Pixels>) {
        if self.enabled {
            self.bounds.insert(id, PaintedBounds::from(bounds));
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PaintedBounds {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl From<Bounds<Pixels>> for PaintedBounds {
    fn from(bounds: Bounds<Pixels>) -> Self {
        Self {
            x: bounds.origin.x.into(),
            y: bounds.origin.y.into(),
            w: bounds.size.width.into(),
            h: bounds.size.height.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationRole {
    Window,
    Page,
    Navigation,
    Button,
    TextBox,
    List,
    ListItem,
    Scroll,
    Checkbox,
    Note,
    Status,
    Dialog,
}

#[derive(Debug, Clone)]
pub struct AutomationNode {
    pub id: String,
    pub role: AutomationRole,
    pub name: String,
    pub value: Option<String>,
    pub checked: Option<bool>,
    pub focused: bool,
    pub enabled: bool,
    pub visible: bool,
    pub bounds: Option<PaintedBounds>,
    pub children: Vec<AutomationNode>,
}

impl AutomationNode {
    pub fn new(role: AutomationRole, id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            role,
            name: name.into(),
            value: None,
            checked: None,
            focused: false,
            enabled: true,
            visible: true,
            bounds: None,
            children: Vec::new(),
        }
    }

    pub fn value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }

    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = Some(checked);
        self
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn visible(mut self, visible: bool) -> Self {
        self.visible = visible;
        self
    }

    /// Apply visibility to this node and, when hiding, every descendant.
    /// Existing hidden descendants stay hidden when `visible` is true.
    pub fn visible_deep(mut self, visible: bool) -> Self {
        self.visible = self.visible && visible;
        if !visible {
            self.children = self
                .children
                .into_iter()
                .map(|child| child.visible_deep(false))
                .collect();
        }
        self
    }

    pub fn children(mut self, children: Vec<AutomationNode>) -> Self {
        self.children = children;
        self
    }

    pub fn child(mut self, child: AutomationNode) -> Self {
        self.children.push(child);
        self
    }

    pub fn with_bounds_from(mut self, registry: &SharedAutomationRegistry) -> Self {
        let registry = registry.lock().unwrap();
        self.bounds = registry.bounds.get(&self.id).copied();
        for child in &mut self.children {
            child.apply_bounds(&registry);
        }
        self
    }

    fn apply_bounds(&mut self, registry: &AutomationRegistry) {
        self.bounds = registry.bounds.get(&self.id).copied();
        for child in &mut self.children {
            child.apply_bounds(registry);
        }
    }
}

#[derive(Debug, Clone)]
pub struct AutomationSnapshot {
    pub app: String,
    pub fixture: bool,
    pub ready: bool,
    pub nodes: Vec<AutomationNode>,
}

/// Parse the semantic target `torrent-{id}` (a torrent-list row) into the
/// torrent id. Detail-panel, file, and status targets deliberately do not
/// match — they have their own parsers.
#[cfg(feature = "agent-testing")]
pub fn parse_torrent_row_target(target: &str) -> Option<u64> {
    let rest = target.strip_prefix("torrent-")?;
    if rest.starts_with("detail-") || rest.contains("-file-") || rest.ends_with("-status") {
        return None;
    }
    rest.parse().ok()
}

/// Parse the semantic target `torrent-detail-{id}` (an open detail panel).
#[cfg(feature = "agent-testing")]
pub fn parse_torrent_detail_target(target: &str) -> Option<u64> {
    target.strip_prefix("torrent-detail-")?.parse().ok()
}

/// Parse the semantic target `torrent-{id}-file-{index}` (a file row in the
/// detail panel) into `(torrent_id, file_index)`.
#[cfg(feature = "agent-testing")]
pub fn parse_torrent_file_target(target: &str) -> Option<(u64, usize)> {
    let rest = target.strip_prefix("torrent-")?;
    let (id, index) = rest.split_once("-file-")?;
    Some((id.parse().ok()?, index.parse().ok()?))
}

/// Parse the semantic target `torrent-{id}-status` (a row's status child).
#[cfg(feature = "agent-testing")]
pub fn parse_torrent_status_target(target: &str) -> Option<u64> {
    let rest = target.strip_prefix("torrent-")?;
    let (id, suffix) = rest.split_once("-status")?;
    if !suffix.is_empty() {
        return None;
    }
    id.parse().ok()
}

/// Parse the semantic target `detail-tab-{name}` into the tab name.
#[cfg(feature = "agent-testing")]
pub fn parse_detail_tab_target(target: &str) -> Option<&str> {
    target.strip_prefix("detail-tab-")
}

#[cfg(all(test, feature = "agent-testing"))]
mod tests {
    use super::{
        parse_detail_tab_target, parse_torrent_detail_target, parse_torrent_file_target,
        parse_torrent_row_target, parse_torrent_status_target,
    };

    #[test]
    fn row_target_matches_only_plain_torrent_ids() {
        assert_eq!(parse_torrent_row_target("torrent-7"), Some(7));
        assert_eq!(parse_torrent_row_target("torrent-detail-7"), None);
        assert_eq!(parse_torrent_row_target("torrent-7-file-3"), None);
        assert_eq!(parse_torrent_row_target("torrent-7-status"), None);
        assert_eq!(parse_torrent_row_target("torrent-"), None);
        assert_eq!(parse_torrent_row_target("torrent-x"), None);
        assert_eq!(parse_torrent_row_target("status-filter"), None);
    }

    #[test]
    fn detail_and_file_targets_parse_their_ids() {
        assert_eq!(parse_torrent_detail_target("torrent-detail-7"), Some(7));
        assert_eq!(parse_torrent_detail_target("torrent-7"), None);
        assert_eq!(parse_torrent_file_target("torrent-7-file-3"), Some((7, 3)));
        assert_eq!(parse_torrent_file_target("torrent-detail-7"), None);
        assert_eq!(parse_torrent_file_target("torrent-7"), None);
    }

    #[test]
    fn status_and_tab_targets_parse() {
        assert_eq!(parse_torrent_status_target("torrent-7-status"), Some(7));
        assert_eq!(parse_torrent_status_target("torrent-7"), None);
        assert_eq!(parse_detail_tab_target("detail-tab-files"), Some("files"));
        assert_eq!(parse_detail_tab_target("torrent-7"), None);
    }
}
