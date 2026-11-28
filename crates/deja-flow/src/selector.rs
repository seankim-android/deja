//! Maestro selector matching engine, spatial ranking, anchored ReDoS-bounded regex compilation,
//! scrubbed [`VisibleContext`] synthesis, and adaptive local wait loop (`wait_local`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use deja_core::{ProviderKeyRedactor, Redactor, provider_key_redactor};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::backoff::{PollSchedule, SettleTracker, is_volatile_system_chrome};
use crate::capabilities::VisibleContext;
use crate::driver::{
    DriverError, PlatformDriver, UiHierarchy, UiNode, UiPoint, UiSize, WaitSelectorOutcome,
    WaitSelectorRequest, is_valid_hex_hash,
};

/// Upper bound on compiled regex program size to prevent ReDoS / memory exhaustion.
pub const REGEX_SIZE_LIMIT: usize = 64 * 1024;

/// Upper bound on compiled regex DFA cache size.
pub const REGEX_DFA_SIZE_LIMIT: usize = 64 * 1024;

/// Maximum relational selector nesting depth (guards against pathological YAML recursion).
pub const MAX_SELECTOR_DEPTH: usize = 16;

/// Single coordinate axis specification (`px` or `%` of viewport).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "unit", content = "value", rename_all = "lowercase")]
pub enum PointCoord {
    Pixels(f64),
    Percent(f64),
}

impl PointCoord {
    pub fn resolve(self, axis_extent: f64) -> f64 {
        match self {
            Self::Pixels(px) => px,
            Self::Percent(pct) => (pct / 100.0) * axis_extent,
        }
    }
}

/// Tap/swipe coordinate pair parsed from `"x,y"` or `"x%,y%"`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PointSpec {
    pub x: PointCoord,
    pub y: PointCoord,
}

impl PointSpec {
    pub const fn pixels(x: f64, y: f64) -> Self {
        Self {
            x: PointCoord::Pixels(x),
            y: PointCoord::Pixels(y),
        }
    }

    pub const fn percent(x_pct: f64, y_pct: f64) -> Self {
        Self {
            x: PointCoord::Percent(x_pct),
            y: PointCoord::Percent(y_pct),
        }
    }

    /// Parses `"x,y"` or `"x%,y%"` (optional spaces around comma).
    pub fn parse(raw: &str) -> Result<Self, SelectorError> {
        let mut parts = raw.split(',');
        let raw_x = parts
            .next()
            .map(str::trim)
            .ok_or_else(|| SelectorError::InvalidPoint(raw.to_string()))?;
        let raw_y = parts
            .next()
            .map(str::trim)
            .ok_or_else(|| SelectorError::InvalidPoint(raw.to_string()))?;
        if parts.next().is_some() || raw_x.is_empty() || raw_y.is_empty() {
            return Err(SelectorError::InvalidPoint(raw.to_string()));
        }
        let x = parse_coord(raw_x, raw)?;
        let y = parse_coord(raw_y, raw)?;
        Ok(Self { x, y })
    }

    /// Returns `true` when either axis is specified as a viewport percentage.
    pub const fn uses_percent(self) -> bool {
        matches!(self.x, PointCoord::Percent(_)) || matches!(self.y, PointCoord::Percent(_))
    }

    /// Resolves pixel or percentage coordinates against a concrete viewport size.
    pub fn resolve(self, viewport: UiSize) -> UiPoint {
        UiPoint {
            x: self.x.resolve(viewport.width),
            y: self.y.resolve(viewport.height),
        }
    }
}

fn parse_coord(part: &str, full: &str) -> Result<PointCoord, SelectorError> {
    if let Some(pct_str) = part.strip_suffix('%') {
        let val: f64 = pct_str
            .trim()
            .parse()
            .map_err(|_| SelectorError::InvalidPoint(full.to_string()))?;
        if !val.is_finite() || !(0.0..=100.0).contains(&val) {
            return Err(SelectorError::InvalidPoint(full.to_string()));
        }
        Ok(PointCoord::Percent(val))
    } else {
        let val: f64 = part
            .parse()
            .map_err(|_| SelectorError::InvalidPoint(full.to_string()))?;
        if !val.is_finite() || val < 0.0 {
            return Err(SelectorError::InvalidPoint(full.to_string()));
        }
        Ok(PointCoord::Pixels(val))
    }
}

/// Errors raised while parsing or compiling a [`Selector`].
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SelectorError {
    #[error("invalid regex in selector `{field}` (`{pattern}`): {message}")]
    InvalidRegex {
        field: &'static str,
        pattern: String,
        message: String,
    },

    #[error("invalid point specification `{0}` (expected \"x,y\" or \"x%,y%\")")]
    InvalidPoint(String),

    #[error("selector has no match criteria")]
    EmptySelector,

    #[error("relational selector nesting exceeds maximum depth of {0}")]
    NestingTooDeep(usize),
}

/// Platform-neutral Maestro element selector.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Selector {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point: Option<PointSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub below: Option<Box<Selector>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub above: Option<Box<Selector>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_of: Option<Box<Selector>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right_of: Option<Box<Selector>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_of: Option<Box<Selector>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains_child: Option<Box<Selector>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contains_descendants: Vec<Selector>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_to_settle_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_tap_if_no_change: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
}

fn contains_regex_meta(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(
            c,
            '.' | '*' | '+' | '?' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
        )
    })
}

impl Selector {
    pub fn by_text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::default()
        }
    }

    pub fn by_id(id: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
            ..Self::default()
        }
    }

    pub fn by_point(point: PointSpec) -> Self {
        Self {
            point: Some(point),
            ..Self::default()
        }
    }

    /// Returns `true` if the selector has at least one matching criterion (not counting `point`).
    pub fn has_element_criteria(&self) -> bool {
        self.id.is_some()
            || self.text.is_some()
            || self.enabled.is_some()
            || self.checked.is_some()
            || self.focused.is_some()
            || self.selected.is_some()
            || self.below.is_some()
            || self.above.is_some()
            || self.left_of.is_some()
            || self.right_of.is_some()
            || self.child_of.is_some()
            || self.contains_child.is_some()
            || !self.contains_descendants.is_empty()
    }

    /// Returns `true` when this selector (or any nested relational/descendant selector)
    /// inspects attributes or nodes excluded from [`crate::backoff::hash_hierarchy_nodes`]
    /// (such as `checked` or volatile system-chrome identifiers), so `knownHash` caching
    /// must be bypassed.
    pub fn uses_unhashed_fields(&self) -> bool {
        if self.checked.is_some() {
            return true;
        }
        if let Some(ref id) = self.id {
            let lower = id.to_ascii_lowercase();
            if lower.contains("com.android.systemui:id/")
                || lower.contains("statusbarbackground")
                || lower.contains("navigationbarbackground")
                || lower.contains("statusbar")
            {
                return true;
            }
        }
        self.below
            .as_deref()
            .is_some_and(Self::uses_unhashed_fields)
            || self
                .above
                .as_deref()
                .is_some_and(Self::uses_unhashed_fields)
            || self
                .left_of
                .as_deref()
                .is_some_and(Self::uses_unhashed_fields)
            || self
                .right_of
                .as_deref()
                .is_some_and(Self::uses_unhashed_fields)
            || self
                .child_of
                .as_deref()
                .is_some_and(Self::uses_unhashed_fields)
            || self
                .contains_child
                .as_deref()
                .is_some_and(Self::uses_unhashed_fields)
            || self
                .contains_descendants
                .iter()
                .any(Self::uses_unhashed_fields)
    }

    /// Returns `true` when this selector AND every nested relational/descendant selector
    /// explicitly requires `enabled == Some(true)`.
    ///
    /// Because [`crate::backoff::hash_hierarchy_nodes`] hashes `hittable` (`enabled && is_on_screen`)
    /// rather than `is_on_screen` independently, a disabled node flipping `is_on_screen` leaves
    /// `hierarchyHash` unchanged. Only selectors requiring `enabled: true` at every level can safely
    /// short-circuit via `knownHash` without risking stale off-screen/on-screen cached nodes.
    pub fn requires_enabled_everywhere(&self) -> bool {
        if self.enabled != Some(true) {
            return false;
        }
        self.below
            .as_deref()
            .is_none_or(Self::requires_enabled_everywhere)
            && self
                .above
                .as_deref()
                .is_none_or(Self::requires_enabled_everywhere)
            && self
                .left_of
                .as_deref()
                .is_none_or(Self::requires_enabled_everywhere)
            && self
                .right_of
                .as_deref()
                .is_none_or(Self::requires_enabled_everywhere)
            && self
                .child_of
                .as_deref()
                .is_none_or(Self::requires_enabled_everywhere)
            && self
                .contains_child
                .as_deref()
                .is_none_or(Self::requires_enabled_everywhere)
            && self
                .contains_descendants
                .iter()
                .all(Self::requires_enabled_everywhere)
    }

    /// Returns `true` ONLY when `id`/`text` contain no regex metacharacters and no `index`,
    /// relational, point, or state filters exist.
    ///
    /// Any regex, relational, indexed, or state-filtered selector returns `false` so it ALWAYS
    /// executes locally in [`wait_local`] with full Rust selector semantics.
    pub fn is_simple_literal_for_server_wait(&self) -> bool {
        if self.id.is_none() && self.text.is_none() {
            return false;
        }
        if let Some(ref id) = self.id
            && (id.is_empty() || contains_regex_meta(id))
        {
            return false;
        }
        if let Some(ref text) = self.text
            && (text.is_empty() || contains_regex_meta(text))
        {
            return false;
        }
        self.index.is_none()
            && self.enabled.is_none()
            && self.checked.is_none()
            && self.focused.is_none()
            && self.selected.is_none()
            && self.point.is_none()
            && self.below.is_none()
            && self.above.is_none()
            && self.left_of.is_none()
            && self.right_of.is_none()
            && self.child_of.is_none()
            && self.contains_child.is_none()
            && self.contains_descendants.is_empty()
    }

    /// Formats a concise human-readable description for step logs and failure labels.
    pub fn describe(&self) -> String {
        if let Some(ref text) = self.text {
            if let Some(ref id) = self.id {
                return format!("\"{text}\" (id: {id})");
            }
            return format!("\"{text}\"");
        }
        if let Some(ref id) = self.id {
            return format!("id: {id}");
        }
        if let Some(point) = self.point {
            let fmt_coord = |c: PointCoord| match c {
                PointCoord::Pixels(px) => format!("{px}"),
                PointCoord::Percent(pct) => format!("{pct}%"),
            };
            return format!("point {},{}", fmt_coord(point.x), fmt_coord(point.y));
        }
        if let Some(ref below) = self.below {
            return format!("element below {}", below.describe());
        }
        if let Some(ref above) = self.above {
            return format!("element above {}", above.describe());
        }
        if let Some(ref left_of) = self.left_of {
            return format!("element leftOf {}", left_of.describe());
        }
        if let Some(ref right_of) = self.right_of {
            return format!("element rightOf {}", right_of.describe());
        }
        if let Some(ref child_of) = self.child_of {
            return format!("element childOf {}", child_of.describe());
        }
        "selector".to_string()
    }
}

/// Maximum raw byte length of a single selector `id`/`text` pattern.
pub const MAX_SELECTOR_PATTERN_BYTES: usize = 4096;

/// Compiled exact-or-anchored-regex matcher for a selector `id` or `text` field.
///
/// Exact string equality is always honored alongside anchored regex matching so literal UI
/// copy containing regex metacharacters (such as `$5.00 · 6 stickers`, `Total: $5.00`, or
/// unescaped brackets/parentheses) still matches verbatim while oversized/complex regex
/// patterns fail closed on [`REGEX_SIZE_LIMIT`].
#[derive(Debug, Clone)]
struct CompiledPattern {
    raw: String,
    regex: Option<Regex>,
}

impl CompiledPattern {
    fn compile(field: &'static str, pattern: &str) -> Result<Self, SelectorError> {
        if pattern.len() > MAX_SELECTOR_PATTERN_BYTES {
            return Err(SelectorError::InvalidRegex {
                field,
                pattern: pattern.chars().take(64).collect(),
                message: format!(
                    "selector pattern exceeds maximum length of {MAX_SELECTOR_PATTERN_BYTES} bytes"
                ),
            });
        }
        let anchored = format!("^(?:{pattern})$");
        match RegexBuilder::new(&anchored)
            .size_limit(REGEX_SIZE_LIMIT)
            .dfa_size_limit(REGEX_DFA_SIZE_LIMIT)
            .build()
        {
            Ok(re) => Ok(Self {
                raw: pattern.to_string(),
                regex: Some(re),
            }),
            Err(regex::Error::Syntax(_)) => {
                // Literal UI text containing unbalanced regex syntax (e.g. "Save [20%" or
                // "Item (1") falls back to exact string equality.
                Ok(Self {
                    raw: pattern.to_string(),
                    regex: None,
                })
            }
            Err(err) => Err(SelectorError::InvalidRegex {
                field,
                pattern: pattern.to_string(),
                message: err.to_string(),
            }),
        }
    }

    fn is_match(&self, candidate: &str) -> bool {
        if candidate.is_empty() && !self.raw.is_empty() {
            return false;
        }
        if candidate == self.raw {
            return true;
        }
        self.regex.as_ref().is_some_and(|re| re.is_match(candidate))
    }
}

/// Pre-compiled [`Selector`] with bounded anchored regexes ready for repeated polling evaluations.
#[derive(Debug, Clone)]
pub struct CompiledSelector {
    id_pattern: Option<CompiledPattern>,
    text_pattern: Option<CompiledPattern>,
    index: Option<usize>,
    enabled: Option<bool>,
    checked: Option<bool>,
    focused: Option<bool>,
    selected: Option<bool>,
    below: Option<Box<CompiledSelector>>,
    above: Option<Box<CompiledSelector>>,
    left_of: Option<Box<CompiledSelector>>,
    right_of: Option<Box<CompiledSelector>>,
    child_of: Option<Box<CompiledSelector>>,
    contains_child: Option<Box<CompiledSelector>>,
    contains_descendants: Vec<CompiledSelector>,
}

impl CompiledSelector {
    /// Compiles a [`Selector`], enforcing anchored regex size limits and nesting depth caps.
    pub fn compile(selector: &Selector) -> Result<Self, SelectorError> {
        Self::compile_depth(selector, 0)
    }

    fn compile_depth(selector: &Selector, depth: usize) -> Result<Self, SelectorError> {
        if depth > MAX_SELECTOR_DEPTH {
            return Err(SelectorError::NestingTooDeep(MAX_SELECTOR_DEPTH));
        }
        if !selector.has_element_criteria() {
            return Err(SelectorError::EmptySelector);
        }
        let id_pattern = match &selector.id {
            Some(pat) => Some(CompiledPattern::compile("id", pat)?),
            None => None,
        };
        let text_pattern = match &selector.text {
            Some(pat) => Some(CompiledPattern::compile("text", pat)?),
            None => None,
        };
        let below = selector
            .below
            .as_deref()
            .map(|s| Self::compile_depth(s, depth + 1).map(Box::new))
            .transpose()?;
        let above = selector
            .above
            .as_deref()
            .map(|s| Self::compile_depth(s, depth + 1).map(Box::new))
            .transpose()?;
        let left_of = selector
            .left_of
            .as_deref()
            .map(|s| Self::compile_depth(s, depth + 1).map(Box::new))
            .transpose()?;
        let right_of = selector
            .right_of
            .as_deref()
            .map(|s| Self::compile_depth(s, depth + 1).map(Box::new))
            .transpose()?;
        let child_of = selector
            .child_of
            .as_deref()
            .map(|s| Self::compile_depth(s, depth + 1).map(Box::new))
            .transpose()?;
        let contains_child = selector
            .contains_child
            .as_deref()
            .map(|s| Self::compile_depth(s, depth + 1).map(Box::new))
            .transpose()?;
        let mut contains_descendants = Vec::with_capacity(selector.contains_descendants.len());
        for desc in &selector.contains_descendants {
            contains_descendants.push(Self::compile_depth(desc, depth + 1)?);
        }

        Ok(Self {
            id_pattern,
            text_pattern,
            index: selector.index,
            enabled: selector.enabled,
            checked: selector.checked,
            focused: selector.focused,
            selected: selector.selected,
            below,
            above,
            left_of,
            right_of,
            child_of,
            contains_child,
            contains_descendants,
        })
    }

    fn matches_local_attributes(&self, node: &UiNode) -> bool {
        if let Some(ref pat) = self.id_pattern
            && !pat.is_match(&node.identifier)
        {
            return false;
        }
        if let Some(ref pat) = self.text_pattern {
            let candidates = [
                Some(node.label.as_str()),
                Some(node.title.as_str()),
                node.value.as_deref(),
                node.placeholder_value.as_deref(),
            ];
            let matched = candidates.into_iter().flatten().any(|s| pat.is_match(s));
            if !matched {
                return false;
            }
        }
        if let Some(expected) = self.enabled
            && node.enabled != expected
        {
            return false;
        }
        if let Some(expected) = self.checked
            && node.checked != expected
        {
            return false;
        }
        if let Some(expected) = self.focused
            && node.has_focus != expected
        {
            return false;
        }
        if let Some(expected) = self.selected
            && node.selected != expected
        {
            return false;
        }
        true
    }

    /// Evaluates this selector against visible (`is_on_screen`) nodes, honoring `index` on both
    /// top-level and nested relational selectors, and ranking matches by spatial distance (when
    /// directional predicates are active), then `hittable && is_on_screen`, then `is_on_screen`,
    /// then original tree order.
    pub fn match_all<'a>(&self, nodes: &'a [UiNode]) -> Vec<&'a UiNode> {
        let graph = TreeIndex::build(nodes);
        self.match_all_with_index(nodes, &graph, true)
    }

    /// Evaluates this selector across ALL nodes including off-screen (`is_on_screen == false`)
    /// nodes, ranking `hittable && is_on_screen` ahead of `is_on_screen` ahead of off-screen.
    pub fn match_all_including_offscreen<'a>(&self, nodes: &'a [UiNode]) -> Vec<&'a UiNode> {
        let graph = TreeIndex::build(nodes);
        self.match_all_with_index(nodes, &graph, false)
    }

    /// Evaluates this selector against visible (`is_on_screen`) nodes and returns the selected
    /// node (honoring `index`, defaulting to `0`).
    pub fn match_one<'a>(&self, nodes: &'a [UiNode]) -> Option<&'a UiNode> {
        self.match_all(nodes).into_iter().next()
    }

    fn match_all_with_index<'a>(
        &self,
        nodes: &'a [UiNode],
        graph: &TreeIndex,
        visible_only: bool,
    ) -> Vec<&'a UiNode> {
        let below_anchors = self
            .below
            .as_deref()
            .map(|s| s.match_all_with_index(nodes, graph, visible_only));
        let above_anchors = self
            .above
            .as_deref()
            .map(|s| s.match_all_with_index(nodes, graph, visible_only));
        let left_anchors = self
            .left_of
            .as_deref()
            .map(|s| s.match_all_with_index(nodes, graph, visible_only));
        let right_anchors = self
            .right_of
            .as_deref()
            .map(|s| s.match_all_with_index(nodes, graph, visible_only));

        let child_of_anchor_ids: Option<HashSet<usize>> = self.child_of.as_deref().map(|s| {
            s.match_all_with_index(nodes, graph, visible_only)
                .into_iter()
                .map(|n| n.index)
                .collect()
        });
        let contains_child_ids: Option<HashSet<usize>> = self.contains_child.as_deref().map(|s| {
            s.match_all_with_index(nodes, graph, visible_only)
                .into_iter()
                .map(|n| n.index)
                .collect()
        });
        let descendant_id_sets: Vec<HashSet<usize>> = self
            .contains_descendants
            .iter()
            .map(|s| {
                s.match_all_with_index(nodes, graph, visible_only)
                    .into_iter()
                    .map(|n| n.index)
                    .collect()
            })
            .collect();

        let has_spatial = below_anchors.is_some()
            || above_anchors.is_some()
            || left_anchors.is_some()
            || right_anchors.is_some();

        let mut ranked: Vec<(f64, usize, &'a UiNode)> = Vec::new();

        for (pos, node) in nodes.iter().enumerate() {
            if visible_only && !node.is_on_screen {
                continue;
            }
            if !self.matches_local_attributes(node) {
                continue;
            }

            if let Some(ref anchors) = child_of_anchor_ids
                && !graph.has_ancestor_in(node.index, anchors)
            {
                continue;
            }

            if let Some(ref child_matches) = contains_child_ids
                && !graph.has_direct_child_in(node.index, child_matches)
            {
                continue;
            }

            if !descendant_id_sets.is_empty()
                && !descendant_id_sets
                    .iter()
                    .all(|targets| graph.has_descendant_in(node.index, targets))
            {
                continue;
            }

            let mut total_dist = 0.0_f64;

            if let Some(ref anchors) = below_anchors {
                let Some(d) = min_directional_distance(node, anchors, Direction::Below, graph)
                else {
                    continue;
                };
                total_dist += d;
            }
            if let Some(ref anchors) = above_anchors {
                let Some(d) = min_directional_distance(node, anchors, Direction::Above, graph)
                else {
                    continue;
                };
                total_dist += d;
            }
            if let Some(ref anchors) = left_anchors {
                let Some(d) = min_directional_distance(node, anchors, Direction::LeftOf, graph)
                else {
                    continue;
                };
                total_dist += d;
            }
            if let Some(ref anchors) = right_anchors {
                let Some(d) = min_directional_distance(node, anchors, Direction::RightOf, graph)
                else {
                    continue;
                };
                total_dist += d;
            }

            ranked.push((if has_spatial { total_dist } else { 0.0 }, pos, node));
        }

        ranked.sort_by(|(da, pa, na), (db, pb, nb)| {
            if has_spatial {
                let ord = da.total_cmp(db);
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            let a_hit_vis = na.hittable && na.is_on_screen;
            let b_hit_vis = nb.hittable && nb.is_on_screen;
            b_hit_vis
                .cmp(&a_hit_vis)
                .then_with(|| nb.is_on_screen.cmp(&na.is_on_screen))
                .then_with(|| pa.cmp(pb))
        });

        let ordered: Vec<&'a UiNode> = ranked.into_iter().map(|(_, _, node)| node).collect();
        if let Some(idx) = self.index {
            ordered.get(idx).copied().into_iter().collect()
        } else {
            ordered
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Below,
    Above,
    LeftOf,
    RightOf,
}

fn min_directional_distance(
    candidate: &UiNode,
    anchors: &[&UiNode],
    dir: Direction,
    graph: &TreeIndex,
) -> Option<f64> {
    let cc = candidate.frame.center();
    let cf = candidate.frame;
    let mut best: Option<f64> = None;

    for anchor in anchors {
        if anchor.index == candidate.index {
            continue;
        }
        // Exclude enclosing ancestor containers from claiming directional relation to their own child.
        if graph.is_proper_ancestor(candidate.index, anchor.index) {
            continue;
        }
        let ac = anchor.frame.center();
        let af = anchor.frame;
        let in_dir = match dir {
            Direction::Below => cc.y > ac.y && cf.y >= ac.y && cf.bottom() > af.bottom(),
            Direction::Above => cc.y < ac.y && cf.bottom() <= ac.y && cf.y < af.y,
            Direction::LeftOf => cc.x < ac.x && cf.right() <= ac.x && cf.x < af.x,
            Direction::RightOf => cc.x > ac.x && cf.x >= ac.x && cf.right() > af.right(),
        };
        if !in_dir {
            continue;
        }
        let dist = ((cc.x - ac.x).powi(2) + (cc.y - ac.y).powi(2)).sqrt();
        if !dist.is_finite() {
            continue;
        }
        best = Some(match best {
            Some(prev) => prev.min(dist),
            None => dist,
        });
    }
    best
}

struct TreeIndex {
    parent_by_id: HashMap<usize, Option<usize>>,
    children_by_id: HashMap<usize, Vec<usize>>,
}

impl TreeIndex {
    fn build(nodes: &[UiNode]) -> Self {
        let mut parent_by_id = HashMap::with_capacity(nodes.len());
        let mut children_by_id: HashMap<usize, Vec<usize>> = HashMap::new();
        for node in nodes {
            parent_by_id.insert(node.index, node.parent_index);
            if let Some(parent) = node.parent_index {
                children_by_id.entry(parent).or_default().push(node.index);
            }
        }
        Self {
            parent_by_id,
            children_by_id,
        }
    }

    /// Walks `parent_index` upward with a cycle guard to test if any proper ancestor is in `targets`.
    fn has_ancestor_in(&self, node_id: usize, targets: &HashSet<usize>) -> bool {
        if targets.is_empty() {
            return false;
        }
        let mut visited = HashSet::new();
        visited.insert(node_id);
        let mut cursor = self.parent_by_id.get(&node_id).copied().flatten();
        while let Some(parent) = cursor {
            if !visited.insert(parent) {
                // Defensive cycle guard.
                break;
            }
            if targets.contains(&parent) {
                return true;
            }
            cursor = self.parent_by_id.get(&parent).copied().flatten();
        }
        false
    }

    /// Returns `true` if `maybe_ancestor` is a proper ancestor of `descendant` (cycle-guarded).
    fn is_proper_ancestor(&self, maybe_ancestor: usize, descendant: usize) -> bool {
        let mut visited = HashSet::new();
        visited.insert(descendant);
        let mut cursor = self.parent_by_id.get(&descendant).copied().flatten();
        while let Some(parent) = cursor {
            if !visited.insert(parent) {
                break;
            }
            if parent == maybe_ancestor {
                return true;
            }
            cursor = self.parent_by_id.get(&parent).copied().flatten();
        }
        false
    }

    fn has_direct_child_in(&self, node_id: usize, targets: &HashSet<usize>) -> bool {
        if targets.is_empty() {
            return false;
        }
        self.children_by_id
            .get(&node_id)
            .is_some_and(|children| children.iter().any(|c| targets.contains(c)))
    }

    fn has_descendant_in(&self, node_id: usize, targets: &HashSet<usize>) -> bool {
        if targets.is_empty() {
            return false;
        }
        let mut visited = HashSet::new();
        visited.insert(node_id);
        let mut queue = VecDeque::new();
        if let Some(children) = self.children_by_id.get(&node_id) {
            for &c in children {
                if visited.insert(c) {
                    queue.push_back(c);
                }
            }
        }
        while let Some(curr) = queue.pop_front() {
            if targets.contains(&curr) {
                return true;
            }
            if let Some(children) = self.children_by_id.get(&curr) {
                for &c in children {
                    if visited.insert(c) {
                        queue.push_back(c);
                    }
                }
            }
        }
        false
    }
}

/// Identifies all volatile system chrome nodes and their descendants in `O(n)` memoized time.
fn volatile_chrome_node_ids(nodes: &[UiNode]) -> HashSet<usize> {
    let mut parent_by_id = HashMap::with_capacity(nodes.len());
    let mut status: HashMap<usize, bool> = HashMap::with_capacity(nodes.len());
    for node in nodes {
        parent_by_id.insert(node.index, node.parent_index);
        if is_volatile_system_chrome(&node.source, &node.element_type, &node.identifier) {
            status.insert(node.index, true);
        }
    }
    let mut chain = Vec::new();
    let mut visiting = HashSet::new();
    for node in nodes {
        if status.contains_key(&node.index) {
            continue;
        }
        chain.clear();
        visiting.clear();
        let mut cursor = Some(node.index);
        let is_chrome = loop {
            let Some(idx) = cursor else {
                break false;
            };
            if let Some(&known) = status.get(&idx) {
                break known;
            }
            if !visiting.insert(idx) {
                break false;
            }
            chain.push(idx);
            cursor = parent_by_id.get(&idx).copied().flatten();
        };
        for idx in &chain {
            status.insert(*idx, is_chrome);
        }
    }
    status
        .into_iter()
        .filter_map(|(idx, chrome)| chrome.then_some(idx))
        .collect()
}

/// Builds a bounded, redacted [`VisibleContext`] excluding volatile system chrome (and its
/// descendants) and off-screen nodes.
pub fn build_visible_context(
    nodes: &[UiNode],
    redactor: Option<&Redactor>,
    provider_redactor: Option<&ProviderKeyRedactor>,
    limit: usize,
) -> VisibleContext {
    let default_provider = provider_key_redactor(std::iter::empty::<(&str, &str)>());
    let provider = provider_redactor.unwrap_or(&default_provider);
    let chrome = volatile_chrome_node_ids(nodes);

    let scrub = |raw: &str| -> String {
        let step1 = match redactor {
            Some(r) => r.redact(raw),
            None => raw.to_string(),
        };
        provider.redact(&step1)
    };

    let mut scrubbed: Vec<String> = Vec::new();
    for node in nodes {
        if !node.is_on_screen || chrome.contains(&node.index) {
            continue;
        }
        for raw in [
            Some(node.label.as_str()),
            Some(node.title.as_str()),
            node.value.as_deref(),
            Some(node.identifier.as_str()),
            node.placeholder_value.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !raw.trim().is_empty() {
                scrubbed.push(scrub(raw));
            }
        }
    }

    VisibleContext::from_candidates(scrubbed.iter().map(String::as_str), limit)
}

/// Adaptive local selector polling loop using [`PollSchedule`], [`SettleTracker`],
/// [`CancellationToken`], and `knownHash` cached-node reuse.
pub async fn wait_local<D: PlatformDriver + ?Sized>(
    driver: &D,
    req: &WaitSelectorRequest,
    cancel: &CancellationToken,
) -> Result<WaitSelectorOutcome, DriverError> {
    if cancel.is_cancelled() {
        return Err(DriverError::Cancelled);
    }

    let compiled = CompiledSelector::compile(&req.selector)
        .map_err(|err| DriverError::InvalidArgument(err.to_string()))?;

    let start = Instant::now();
    let deadline = start + req.timeout;
    let single_probe = req.timeout.is_zero();
    let selector_tracks_unhashed = req.include_system_ui || req.selector.uses_unhashed_fields();
    // `hash_hierarchy_nodes` hashes `hittable` (`enabled && is_on_screen`), not `is_on_screen`
    // independently, so disabled nodes flipping `is_on_screen` keep the same hash. Only use
    // `knownHash` short-circuiting when no unhashed fields/chrome are queried AND disabled nodes
    // cannot match any part of the selector.
    let allow_known_hash = !selector_tracks_unhashed && req.selector.requires_enabled_everywhere();
    let mut schedule = PollSchedule::default();
    let mut settle = SettleTracker::new(req.stable_polls.max(1));
    let mut miss_settle = if !selector_tracks_unhashed {
        req.fail_fast_on_settle_polls
            .map(|n| SettleTracker::new(n.max(2)))
    } else {
        None
    };
    let mut polls: u32 = 0;

    let mut cached_hierarchy: Option<UiHierarchy> = None;
    let mut latest_hierarchy: Option<UiHierarchy> = None;
    let mut known_hash: Option<String> = None;
    let mut last_observed_hash: Option<String> = None;
    let mut had_usable_observation_this_wait = false;

    if let Some(ref init) = req.initial_hierarchy
        && !init.unchanged
    {
        let h_u64 = init.hierarchy_hash_u64();
        let h_hex = if is_valid_hex_hash(&init.hierarchy_hash) {
            init.hierarchy_hash.to_ascii_lowercase()
        } else {
            format!("{h_u64:016x}")
        };
        if allow_known_hash && !init.truncated {
            known_hash = Some(h_hex);
        }
        cached_hierarchy = Some(init.clone());
        latest_hierarchy = Some(init.clone());
    }

    let mut last_transient_err: Option<DriverError> = None;

    loop {
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }
        let remaining_for_fetch = if single_probe {
            None
        } else {
            let rem = deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() {
                break;
            }
            Some(rem)
        };
        polls = polls.saturating_add(1);
        let request_known_hash = if allow_known_hash {
            known_hash.as_deref()
        } else {
            None
        };

        let fetch = match remaining_for_fetch {
            None => tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                res = driver.hierarchy(
                    &req.app_id,
                    req.include_system_ui,
                    request_known_hash,
                    cancel,
                ) => res,
            },
            Some(rem) => tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                timed = tokio::time::timeout(
                    rem,
                    driver.hierarchy(
                        &req.app_id,
                        req.include_system_ui,
                        request_known_hash,
                        cancel,
                    ),
                ) => match timed {
                    Ok(res) => res,
                    Err(_) => break,
                },
            },
        };

        if !single_probe && Instant::now() > deadline {
            break;
        }

        match fetch {
            Ok(resp) => {
                last_transient_err = None;
                let effective: Option<UiHierarchy> = if resp.unchanged {
                    if let Some(ref cached) = cached_hierarchy {
                        // Reuse ONLY `cached.nodes` while preserving the fresh response's
                        // `truncated`, `state`, `viewport`, and `hierarchy_hash` metadata.
                        Some(resp.merge_unchanged_with_cached_nodes(cached))
                    } else {
                        // Driver returned `unchanged: true` without any cached nodes:
                        // never treat empty payload as absence! Reset settle and re-poll uncached.
                        if req.wait_stable {
                            settle.reset();
                        }
                        if let Some(ref mut ms) = miss_settle {
                            ms.reset();
                        }
                        known_hash = None;
                        None
                    }
                } else {
                    let h_u64 = resp.hierarchy_hash_u64();
                    let h_hex = if is_valid_hex_hash(&resp.hierarchy_hash) {
                        resp.hierarchy_hash.to_ascii_lowercase()
                    } else {
                        format!("{h_u64:016x}")
                    };
                    if allow_known_hash && !resp.truncated {
                        known_hash = Some(h_hex);
                    } else {
                        known_hash = None;
                    }
                    cached_hierarchy = Some(resp.clone());
                    Some(resp)
                };

                if let Some(effective) = effective {
                    had_usable_observation_this_wait = true;
                    let effective_truncated = effective.truncated;
                    let hash_u64 = effective.hierarchy_hash_u64();
                    let hash_hex = if is_valid_hex_hash(&effective.hierarchy_hash) {
                        effective.hierarchy_hash.to_ascii_lowercase()
                    } else {
                        format!("{hash_u64:016x}")
                    };
                    last_observed_hash = Some(hash_hex.clone());

                    if req.wait_stable {
                        if effective_truncated {
                            settle.reset();
                        } else {
                            settle.observe_hash(hash_u64);
                        }
                    }

                    let matched_node = compiled
                        .match_one(&effective.nodes)
                        .filter(|node| !req.require_hittable || node.hittable)
                        .cloned();
                    let condition_met = if req.not_visible {
                        // Never falsely pass `not_visible` on a truncated hierarchy capture!
                        if effective_truncated {
                            false
                        } else {
                            matched_node.is_none()
                        }
                    } else {
                        matched_node.is_some()
                    };

                    let stable_ok = !req.wait_stable || settle.is_settled();

                    latest_hierarchy = Some(effective);

                    if condition_met && stable_ok {
                        let elapsed = start.elapsed();
                        return Ok(WaitSelectorOutcome {
                            matched: true,
                            elapsed_ms: elapsed.as_millis() as u64,
                            polls,
                            settled_early: !single_probe && elapsed < req.timeout,
                            obtained_fresh_observation: true,
                            hierarchy_hash: hash_hex,
                            matched_node,
                            visible_context: VisibleContext::default(),
                            hierarchy: if req.return_hierarchy {
                                latest_hierarchy
                            } else {
                                None
                            },
                        });
                    }

                    // Settled fail-fast for missing action selectors on a static, complete screen.
                    if let Some(ref mut ms) = miss_settle {
                        if condition_met || effective_truncated {
                            ms.reset();
                        } else if ms.observe_hash(hash_u64).is_settled() {
                            break;
                        }
                    }
                }
            }
            Err(DriverError::Cancelled) => return Err(DriverError::Cancelled),
            Err(err @ DriverError::AppCrashed(_))
            | Err(err @ DriverError::Unsupported(_))
            | Err(err @ DriverError::SessionClosed(_)) => return Err(err),
            Err(
                err @ DriverError::Transport {
                    transient: true, ..
                },
            ) => {
                if req.wait_stable {
                    settle.reset();
                }
                if let Some(ref mut ms) = miss_settle {
                    ms.reset();
                }
                last_transient_err = Some(err);
            }
            Err(err) => return Err(err),
        }

        if single_probe {
            break;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let Some(delay) = schedule.next_delay(remaining) else {
            break;
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(DriverError::Cancelled),
            _ = tokio::time::sleep(delay) => {}
        }
    }

    if let Some(err) = last_transient_err
        && !had_usable_observation_this_wait
    {
        return Err(err);
    }

    let nodes = latest_hierarchy
        .as_ref()
        .map(|h| h.nodes.as_slice())
        .unwrap_or(&[]);
    let visible_context = build_visible_context(
        nodes,
        req.redactor.as_ref(),
        req.provider_redactor.as_ref(),
        VisibleContext::DEFAULT_LIMIT,
    );

    Ok(WaitSelectorOutcome {
        matched: false,
        elapsed_ms: start.elapsed().as_millis() as u64,
        polls,
        settled_early: false,
        obtained_fresh_observation: had_usable_observation_this_wait,
        hierarchy_hash: last_observed_hash.or(known_hash).unwrap_or_default(),
        matched_node: None,
        visible_context,
        hierarchy: if req.return_hierarchy {
            latest_hierarchy
        } else {
            None
        },
    })
}

#[cfg(test)]
mod tests {
    use super::build_visible_context;
    use crate::driver::UiNode;
    use deja_core::{create_redactor, provider_key_redactor};

    #[test]
    fn build_visible_context_redacts_raw_whitespace_and_newline_secrets_before_normalizing() {
        let secret = "my  secret\npayload";
        let redactor = create_redactor("/tmp/ws", None, &[secret.to_string()]);
        let provider = provider_key_redactor(std::iter::empty::<(&str, &str)>());

        let node = UiNode {
            index: 1,
            parent_index: Some(0),
            depth: 1,
            label: format!("Greeting {secret} end"),
            is_on_screen: true,
            ..UiNode::default()
        };

        let ctx = build_visible_context(&[node], Some(&redactor), Some(&provider), 10);
        let summary = ctx.format_summary();
        assert!(
            !summary.contains("secret") && !summary.contains("payload"),
            "secret with repeated spaces and newline leaked into VisibleContext: {summary}"
        );
        assert!(
            summary.contains("<redacted-fixture-value>")
                && summary.contains("Greeting")
                && summary.contains("end"),
            "expected redacted placeholder with surrounding text: {summary}"
        );
    }
}
