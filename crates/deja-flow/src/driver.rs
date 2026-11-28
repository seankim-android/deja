//! Platform-neutral [`PlatformDriver`] trait, UI hierarchy model, compound act/wait contracts,
//! and typed [`DriverError`] definitions.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, Instant};

use deja_core::{
    CrashSignal, CrashSignalSource, LaunchArgumentValue, ProviderKeyRedactor, Redactor,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::backoff::{HashableNode, hash_hierarchy_nodes};
use crate::capabilities::{
    DriverCapabilities, ObservationSource, UnsupportedCapability, VisibleContext,
};
use crate::selector::{self, Selector};

/// Dyn-compatible boxed future returned by [`PlatformDriver`] async methods.
pub type DriverFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, DriverError>> + Send + 'a>>;

/// Default viewport width used when neither the driver nor hierarchy nodes report screen bounds.
pub const DEFAULT_VIEWPORT_WIDTH: f64 = 390.0;

/// Default viewport height used when neither the driver nor hierarchy nodes report screen bounds.
pub const DEFAULT_VIEWPORT_HEIGHT: f64 = 844.0;

/// 2D point in logical screen coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiPoint {
    pub x: f64,
    pub y: f64,
}

impl UiPoint {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// 2D screen or viewport dimensions in logical coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiSize {
    pub width: f64,
    pub height: f64,
}

impl UiSize {
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }

    pub fn is_positive(self) -> bool {
        self.width.is_finite() && self.height.is_finite() && self.width > 0.0 && self.height > 0.0
    }
}

/// Axis-aligned bounding rectangle of a UI element in logical coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl UiRect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Center point `(x + width/2, y + height/2)`.
    pub fn center(self) -> UiPoint {
        UiPoint {
            x: self.x + self.width / 2.0,
            y: self.y + self.height / 2.0,
        }
    }

    pub fn right(self) -> f64 {
        self.x + self.width
    }

    pub fn bottom(self) -> f64 {
        self.y + self.height
    }

    pub fn area(self) -> f64 {
        if !self.width.is_finite() || !self.height.is_finite() {
            return 0.0;
        }
        self.width.max(0.0) * self.height.max(0.0)
    }

    pub fn is_empty(self) -> bool {
        self.area() <= 0.0
    }

    /// Computes intersection area with another rectangle.
    pub fn intersection_area(self, other: UiRect) -> f64 {
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if !left.is_finite() || !top.is_finite() || !right.is_finite() || !bottom.is_finite() {
            return 0.0;
        }
        (right - left).max(0.0) * (bottom - top).max(0.0)
    }

    /// Returns the fraction `[0.0, 1.0]` of this rectangle visible within `[0..viewport]`.
    pub fn visible_fraction_in(self, viewport: UiSize) -> f64 {
        let total = self.area();
        if total <= 0.0 || !viewport.is_positive() {
            return 0.0;
        }
        let vp_rect = UiRect::new(0.0, 0.0, viewport.width, viewport.height);
        (self.intersection_area(vp_rect) / total).clamp(0.0, 1.0)
    }
}

fn default_true() -> bool {
    true
}

fn round_coord_to_i32(v: f64) -> i32 {
    if !v.is_finite() {
        return 0;
    }
    let rounded = v.round();
    if rounded >= i32::MAX as f64 {
        i32::MAX
    } else if rounded <= i32::MIN as f64 {
        i32::MIN
    } else {
        rounded as i32
    }
}

/// Platform-neutral accessibility/DOM/OCR UI element node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "UiNodeWire")]
pub struct UiNode {
    pub index: usize,
    pub parent_index: Option<usize>,
    pub depth: usize,
    pub source: String,
    pub observation_source: ObservationSource,
    pub element_type: String,
    pub identifier: String,
    pub label: String,
    pub title: String,
    pub value: Option<String>,
    pub placeholder_value: Option<String>,
    pub enabled: bool,
    pub checked: bool,
    pub selected: bool,
    pub has_focus: bool,
    pub hittable: bool,
    pub is_on_screen: bool,
    pub frame: UiRect,
    /// Optional clipped visible hit rectangle when the node is partially clipped by the viewport
    /// or an overflow scroll container. When present and non-empty, [`UiNode::tap_point`] targets
    /// this rect's center while [`Self::frame`] retains the full unclipped bounds for visibility
    /// fraction checks (such as `scrollUntilVisible`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_frame: Option<UiRect>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UiNodeWire {
    index: usize,
    #[serde(default)]
    parent_index: Option<usize>,
    #[serde(default)]
    depth: usize,
    #[serde(default)]
    source: String,
    #[serde(default)]
    observation_source: ObservationSource,
    #[serde(default)]
    element_type: String,
    #[serde(default)]
    identifier: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    placeholder_value: Option<String>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    checked: bool,
    #[serde(default)]
    selected: bool,
    #[serde(default)]
    has_focus: bool,
    #[serde(default)]
    hittable: bool,
    #[serde(default)]
    is_on_screen: Option<bool>,
    frame: UiRect,
    #[serde(default)]
    visible_frame: Option<UiRect>,
}

impl From<UiNodeWire> for UiNode {
    fn from(wire: UiNodeWire) -> Self {
        Self {
            index: wire.index,
            parent_index: wire.parent_index,
            depth: wire.depth,
            source: wire.source,
            observation_source: wire.observation_source,
            element_type: wire.element_type,
            identifier: wire.identifier,
            label: wire.label,
            title: wire.title,
            value: wire.value,
            placeholder_value: wire.placeholder_value,
            enabled: wire.enabled,
            checked: wire.checked,
            selected: wire.selected,
            has_focus: wire.has_focus,
            hittable: wire.hittable,
            // Preserve explicit `isOnScreen` from modern drivers (so disabled on-screen nodes
            // remain visible while scroll-clipped nodes stay off-screen). Legacy JSON payloads
            // that omit `isOnScreen` fall back to `hittable` instead of blindly defaulting `true`.
            is_on_screen: wire.is_on_screen.unwrap_or(wire.hittable),
            frame: wire.frame,
            visible_frame: wire.visible_frame,
        }
    }
}

impl Default for UiNode {
    fn default() -> Self {
        Self {
            index: 0,
            parent_index: None,
            depth: 0,
            source: "app".to_string(),
            observation_source: ObservationSource::Tree,
            element_type: String::new(),
            identifier: String::new(),
            label: String::new(),
            title: String::new(),
            value: None,
            placeholder_value: None,
            enabled: true,
            checked: false,
            selected: false,
            has_focus: false,
            hittable: false,
            is_on_screen: true,
            frame: UiRect::default(),
            visible_frame: None,
        }
    }
}

impl UiNode {
    /// Returns the preferred tap point for this node: the center of its clipped visible hit
    /// region (`visible_frame`) when present and non-empty, otherwise `frame.center()`.
    #[must_use]
    pub fn tap_point(&self) -> UiPoint {
        self.visible_frame
            .filter(|r| !r.is_empty())
            .unwrap_or(self.frame)
            .center()
    }

    /// Returns the fraction `[0.0, 1.0]` of this node's full [`Self::frame`] visible inside
    /// `viewport`, using [`Self::visible_frame`] when clipped by an overflow scroll container
    /// or viewport boundary and falling back to [`UiRect::visible_fraction_in`] otherwise.
    #[must_use]
    pub fn visible_fraction(&self, viewport: UiSize) -> f64 {
        let total = self.frame.area();
        if total <= 0.0 || !viewport.is_positive() {
            return 0.0;
        }
        if let Some(clipped) = self.visible_frame {
            let vp_rect = UiRect::new(0.0, 0.0, viewport.width, viewport.height);
            return (clipped.intersection_area(vp_rect) / total).clamp(0.0, 1.0);
        }
        self.frame.visible_fraction_in(viewport)
    }

    /// Projects this node into a [`HashableNode`] matching Swift↔Rust FNV-1a parity.
    pub fn as_hashable(&self) -> HashableNode<'_> {
        HashableNode {
            index: self.index,
            parent_index: self.parent_index,
            depth: self.depth,
            source: &self.source,
            element_type: &self.element_type,
            identifier: &self.identifier,
            label: &self.label,
            title: &self.title,
            value: self.value.as_deref(),
            placeholder_value: self.placeholder_value.as_deref(),
            enabled: self.enabled,
            selected: self.selected,
            has_focus: self.has_focus,
            hittable: self.hittable,
            x: round_coord_to_i32(self.frame.x),
            y: round_coord_to_i32(self.frame.y),
            width: round_coord_to_i32(self.frame.width),
            height: round_coord_to_i32(self.frame.height),
        }
    }
}

/// Captured UI hierarchy snapshot returned by a [`PlatformDriver`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiHierarchy {
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub viewport: Option<UiSize>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub hierarchy_hash: String,
    #[serde(default)]
    pub unchanged: bool,
    #[serde(default)]
    pub nodes: Vec<UiNode>,
}

/// Returns `true` when `hash` is a valid 16-character hexadecimal FNV-1a digest.
pub(crate) fn is_valid_hex_hash(hash: &str) -> bool {
    hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

impl UiHierarchy {
    /// Projects all nodes into [`HashableNode`] records for deterministic FNV-1a hashing.
    pub fn as_hashable(&self) -> Vec<HashableNode<'_>> {
        self.nodes.iter().map(UiNode::as_hashable).collect()
    }

    /// Computes the deterministic chrome-stripped 64-bit FNV-1a hierarchy hash.
    ///
    /// Delegates to [`hash_hierarchy_nodes`] whenever nodes are present (or `unchanged == false`),
    /// preserving bit-for-bit Swift↔Rust parity. When `unchanged == true` and `nodes` is empty,
    /// parses the driver-supplied 16-hex `hierarchy_hash` if valid.
    pub fn hierarchy_hash_u64(&self) -> u64 {
        if self.unchanged
            && self.nodes.is_empty()
            && is_valid_hex_hash(&self.hierarchy_hash)
            && let Ok(parsed) = u64::from_str_radix(&self.hierarchy_hash, 16)
        {
            return parsed;
        }
        hash_hierarchy_nodes(self.as_hashable())
    }

    /// Returns a 16-character lowercase hexadecimal representation of [`Self::hierarchy_hash_u64`].
    pub fn hierarchy_hash_hex(&self) -> String {
        format!("{:016x}", self.hierarchy_hash_u64())
    }

    /// Reconstructs a full hierarchy snapshot from an `unchanged: true` response by reusing ONLY
    /// `cached.nodes` while preserving the fresh response's `truncated`, `state`, `viewport`,
    /// `app_id`, and `hierarchy_hash` metadata (and remaining truncated if `cached` was truncated).
    pub fn merge_unchanged_with_cached_nodes(self, cached: &UiHierarchy) -> UiHierarchy {
        let effective_hash = if !self.hierarchy_hash.is_empty() {
            self.hierarchy_hash
        } else {
            cached.hierarchy_hash.clone()
        };
        let effective_viewport = self.viewport.or(cached.viewport);
        let effective_app_id = if !self.app_id.is_empty() {
            self.app_id
        } else {
            cached.app_id.clone()
        };
        let effective_state = if !self.state.is_empty() {
            self.state
        } else {
            cached.state.clone()
        };
        let nodes = if self.nodes.is_empty() {
            cached.nodes.clone()
        } else {
            self.nodes
        };
        UiHierarchy {
            app_id: effective_app_id,
            state: effective_state,
            viewport: effective_viewport,
            truncated: self.truncated || cached.truncated,
            hierarchy_hash: effective_hash,
            unchanged: false,
            nodes,
        }
    }

    /// Resolves the screen viewport from `self.viewport`, falling back to root/max node bounds
    /// or [`DEFAULT_VIEWPORT_WIDTH`] x [`DEFAULT_VIEWPORT_HEIGHT`].
    pub fn effective_viewport(&self) -> UiSize {
        if let Some(vp) = self.viewport
            && vp.is_positive()
        {
            return vp;
        }
        if let Some(root) = self
            .nodes
            .iter()
            .find(|n| n.depth == 0 && n.frame.width > 0.0 && n.frame.height > 0.0)
        {
            let size = UiSize::new(root.frame.width, root.frame.height);
            if size.is_positive() {
                return size;
            }
        }
        let mut max_x = 0.0_f64;
        let mut max_y = 0.0_f64;
        for n in &self.nodes {
            if n.frame.right().is_finite() {
                max_x = max_x.max(n.frame.right());
            }
            if n.frame.bottom().is_finite() {
                max_y = max_y.max(n.frame.bottom());
            }
        }
        let inferred = UiSize::new(max_x, max_y);
        if inferred.is_positive() {
            inferred
        } else {
            UiSize::new(DEFAULT_VIEWPORT_WIDTH, DEFAULT_VIEWPORT_HEIGHT)
        }
    }
}

/// Options supplied to [`PlatformDriver::launch_app`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchOptions {
    pub clear_state: bool,
    pub clear_keychain: bool,
    pub stop_app: bool,
    /// `None` when omitted in flow YAML (preserving platform default permissions); `Some(empty)`
    /// when explicitly set to `{}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub arguments: BTreeMap<String, LaunchArgumentValue>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            clear_state: false,
            clear_keychain: false,
            stop_app: true,
            permissions: None,
            arguments: BTreeMap::new(),
            env: BTreeMap::new(),
        }
    }
}

/// Options forwarded to [`PlatformDriver::open_link`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenLinkOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_verify: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<bool>,
}

/// Coordinates and timing for [`PlatformDriver::swipe`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwipeParams {
    pub app_id: String,
    pub start: UiPoint,
    pub end: UiPoint,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_duration_ms: Option<u64>,
}

/// Key press request for [`PlatformDriver::press_key`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyPress {
    pub key: String,
    #[serde(default)]
    pub modifiers: Vec<String>,
    pub count: u32,
}

impl KeyPress {
    pub fn single(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            modifiers: Vec::new(),
            count: 1,
        }
    }
}

/// Crash evidence surfaced directly by a driver or OS diagnostic report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverCrashSignal {
    pub source: CrashSignalSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl DriverCrashSignal {
    pub fn to_core(&self) -> CrashSignal {
        CrashSignal {
            source: self.source,
            report_file: self.report_file.clone(),
            process: self.process.clone(),
        }
    }

    pub fn source_label(&self) -> &'static str {
        match self.source {
            CrashSignalSource::MaestroOutput => "maestro-output",
            CrashSignalSource::CrashReport => "crash-report",
        }
    }
}

impl From<DriverCrashSignal> for CrashSignal {
    fn from(value: DriverCrashSignal) -> Self {
        value.to_core()
    }
}

/// Action kind executed by [`PlatformDriver::act_and_observe`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum ActAction {
    Tap {
        x: f64,
        y: f64,
    },
    LongPress {
        x: f64,
        y: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    DoubleTap {
        x: f64,
        y: f64,
    },
    Swipe {
        start: UiPoint,
        end: UiPoint,
        duration_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hold_duration_ms: Option<u64>,
    },
    InputText {
        text: String,
    },
    EraseText {
        count: u32,
    },
    PressKey {
        key: String,
        #[serde(default)]
        modifiers: Vec<String>,
        count: u32,
    },
    HideKeyboard,
    Back,
}

/// Compound action + optional post-action hierarchy capture request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActPayload {
    pub app_id: String,
    pub action: ActAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_action_delay_ms: Option<u64>,
    pub return_hierarchy: bool,
    pub include_system_ui: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_hash: Option<String>,
}

/// Result of [`PlatformDriver::act_and_observe`].
///
/// When the action succeeds but the follow-up hierarchy fetch fails, `action_ok` stays `true`
/// and `hierarchy_error` captures the error message so callers never replay the action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActOutcome {
    pub action_ok: bool,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hierarchy: Option<UiHierarchy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hierarchy_error: Option<String>,
}

/// Request parameters for [`PlatformDriver::wait_for_selector`] and [`selector::wait_local`].
#[derive(Debug, Clone)]
pub struct WaitSelectorRequest {
    pub app_id: String,
    pub selector: Selector,
    pub not_visible: bool,
    /// When `true` (used for Web selector-based taps), a matched element is only accepted once
    /// `node.hittable == true` so occluded elements behind a modal/overlay keep polling until
    /// unoccluded or timeout instead of dispatching a click onto the obscuring overlay.
    pub require_hittable: bool,
    pub wait_stable: bool,
    pub stable_polls: usize,
    /// When `Some(n)`, fails fast on a missing selector once the screen produces `n`
    /// consecutive identical non-truncated hierarchy hashes (used for action lookups like
    /// `tapOn`, left `None` for explicit waits like `extendedWaitUntil`/`assertVisible`).
    pub fail_fast_on_settle_polls: Option<usize>,
    pub timeout: Duration,
    pub include_system_ui: bool,
    pub return_hierarchy: bool,
    pub initial_hierarchy: Option<UiHierarchy>,
    pub redactor: Option<Redactor>,
    pub provider_redactor: Option<ProviderKeyRedactor>,
}

impl WaitSelectorRequest {
    pub fn visible(app_id: impl Into<String>, selector: Selector, timeout: Duration) -> Self {
        Self {
            app_id: app_id.into(),
            selector,
            not_visible: false,
            require_hittable: false,
            wait_stable: false,
            stable_polls: 2,
            fail_fast_on_settle_polls: None,
            timeout,
            include_system_ui: false,
            return_hierarchy: true,
            initial_hierarchy: None,
            redactor: None,
            provider_redactor: None,
        }
    }

    pub fn not_visible(app_id: impl Into<String>, selector: Selector, timeout: Duration) -> Self {
        Self {
            not_visible: true,
            ..Self::visible(app_id, selector, timeout)
        }
    }
}

/// Outcome of a selector wait poll loop.
#[derive(Debug, Clone, PartialEq)]
pub struct WaitSelectorOutcome {
    pub matched: bool,
    pub elapsed_ms: u64,
    pub polls: u32,
    pub settled_early: bool,
    /// `true` only when at least one poll inside this wait obtained a fresh observation
    /// (as opposed to returning the pre-wait `initial_hierarchy` after an immediate timeout).
    pub obtained_fresh_observation: bool,
    pub hierarchy_hash: String,
    pub matched_node: Option<UiNode>,
    pub visible_context: VisibleContext,
    pub hierarchy: Option<UiHierarchy>,
}

/// Typed errors emitted by [`PlatformDriver`] implementations.
#[derive(Debug, Error)]
pub enum DriverError {
    #[error(transparent)]
    Unsupported(#[from] UnsupportedCapability),

    #[error("driver operation cancelled")]
    Cancelled,

    #[error("timed out after {elapsed_ms}ms during {operation}: {detail}")]
    Timeout {
        operation: String,
        elapsed_ms: u64,
        detail: String,
    },

    #[error("failed to launch app `{app_id}`: {detail}")]
    LaunchFailed { app_id: String, detail: String },

    #[error("navigation failed for `{target}`: {detail}")]
    NavigationFailed { target: String, detail: String },

    #[error("app crashed ({})", .0.source_label())]
    AppCrashed(DriverCrashSignal),

    #[error("invalid driver argument: {0}")]
    InvalidArgument(String),

    #[error("driver transport error during {operation}: {detail}")]
    Transport {
        operation: String,
        detail: String,
        transient: bool,
    },

    #[error("driver session closed: {0}")]
    SessionClosed(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Dyn-compatible platform abstraction implemented by iOS, Android, Web, and mock drivers.
pub trait PlatformDriver: Send + Sync {
    /// Canonical lowercase platform identifier (`"ios"`, `"android"`, `"web"`, `"mock"`, ...).
    fn platform(&self) -> &'static str;

    /// Capabilities genuinely supported by this driver implementation.
    fn capabilities(&self) -> &DriverCapabilities;

    fn ensure_session<'a>(&'a self, cancel: &'a CancellationToken) -> DriverFuture<'a, ()>;

    fn launch_app<'a>(
        &'a self,
        app_id: &'a str,
        opts: &'a LaunchOptions,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()>;

    fn stop_app<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()>;

    fn kill_app<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()>;

    fn clear_state<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()>;

    fn clear_keychain<'a>(&'a self, cancel: &'a CancellationToken) -> DriverFuture<'a, ()>;

    fn open_link<'a>(
        &'a self,
        app_id: Option<&'a str>,
        url: &'a str,
        opts: &'a OpenLinkOptions,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()>;

    fn add_media<'a>(
        &'a self,
        paths: &'a [PathBuf],
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()>;

    fn hierarchy<'a>(
        &'a self,
        app_id: &'a str,
        include_system_ui: bool,
        known_hash: Option<&'a str>,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, UiHierarchy>;

    fn tap_point<'a>(
        &'a self,
        app_id: &'a str,
        point: UiPoint,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()>;

    fn long_press_point<'a>(
        &'a self,
        app_id: &'a str,
        point: UiPoint,
        duration_ms: Option<u64>,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()>;

    fn double_tap_point<'a>(
        &'a self,
        app_id: &'a str,
        point: UiPoint,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()>;

    fn swipe<'a>(
        &'a self,
        params: &'a SwipeParams,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()>;

    fn input_text<'a>(
        &'a self,
        app_id: &'a str,
        text: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()>;

    fn erase_text<'a>(
        &'a self,
        app_id: &'a str,
        count: u32,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()>;

    fn press_key<'a>(
        &'a self,
        app_id: &'a str,
        key: &'a KeyPress,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()>;

    fn hide_keyboard<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()>;

    fn back<'a>(&'a self, app_id: &'a str, cancel: &'a CancellationToken) -> DriverFuture<'a, ()>;

    fn take_screenshot<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Vec<u8>>;

    fn check_app_crash<'a>(
        &'a self,
        app_id: &'a str,
        since_epoch_ms: u64,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Option<DriverCrashSignal>>;

    fn shutdown<'a>(&'a self) -> DriverFuture<'a, ()>;

    /// Executes an action and optionally captures a post-action hierarchy snapshot.
    ///
    /// Drivers advertising [`crate::capabilities::DriverCapability::CompoundAct`] may override
    /// this to collapse action + observation into a single IPC round-trip. The default fallback
    /// executes the primitive action, honors `post_action_delay_ms`, and if `return_hierarchy`
    /// fails after the action succeeded, preserves `action_ok: true` with `hierarchy_error`.
    fn act_and_observe<'a>(
        &'a self,
        req: &'a ActPayload,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ActOutcome> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let started = Instant::now();
            match &req.action {
                ActAction::Tap { x, y } => {
                    self.tap_point(&req.app_id, UiPoint::new(*x, *y), cancel)
                        .await?;
                }
                ActAction::LongPress { x, y, duration_ms } => {
                    self.long_press_point(&req.app_id, UiPoint::new(*x, *y), *duration_ms, cancel)
                        .await?;
                }
                ActAction::DoubleTap { x, y } => {
                    self.double_tap_point(&req.app_id, UiPoint::new(*x, *y), cancel)
                        .await?;
                }
                ActAction::Swipe {
                    start,
                    end,
                    duration_ms,
                    hold_duration_ms,
                } => {
                    let params = SwipeParams {
                        app_id: req.app_id.clone(),
                        start: *start,
                        end: *end,
                        duration_ms: *duration_ms,
                        hold_duration_ms: *hold_duration_ms,
                    };
                    self.swipe(&params, cancel).await?;
                }
                ActAction::InputText { text } => {
                    self.input_text(&req.app_id, text, cancel).await?;
                }
                ActAction::EraseText { count } => {
                    self.erase_text(&req.app_id, *count, cancel).await?;
                }
                ActAction::PressKey {
                    key,
                    modifiers,
                    count,
                } => {
                    let kp = KeyPress {
                        key: key.clone(),
                        modifiers: modifiers.clone(),
                        count: *count,
                    };
                    self.press_key(&req.app_id, &kp, cancel).await?;
                }
                ActAction::HideKeyboard => {
                    self.hide_keyboard(&req.app_id, cancel).await?;
                }
                ActAction::Back => {
                    self.back(&req.app_id, cancel).await?;
                }
            }

            if let Some(delay_ms) = req.post_action_delay_ms
                && delay_ms > 0
            {
                tokio::select! {
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                }
            }

            let (hierarchy, hierarchy_error) = if req.return_hierarchy {
                match self
                    .hierarchy(
                        &req.app_id,
                        req.include_system_ui,
                        req.known_hash.as_deref(),
                        cancel,
                    )
                    .await
                {
                    Ok(h) => (Some(h), None),
                    Err(DriverError::Cancelled) => return Err(DriverError::Cancelled),
                    Err(err) => (None, Some(err.to_string())),
                }
            } else {
                (None, None)
            };

            Ok(ActOutcome {
                action_ok: true,
                elapsed_ms: started.elapsed().as_millis() as u64,
                hierarchy,
                hierarchy_error,
            })
        })
    }

    /// Waits for a selector condition to hold, defaulting to local adaptive polling via
    /// [`selector::wait_local`] with `PollSchedule`, `SettleTracker`, and `knownHash` reuse.
    fn wait_for_selector<'a>(
        &'a self,
        req: &'a WaitSelectorRequest,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, WaitSelectorOutcome> {
        Box::pin(async move { selector::wait_local(self, req, cancel).await })
    }
}
