//! Adaptive polling schedules and chrome-stripped hierarchy settle tracking.
//!
//! Pure `std` implementation shared by `deja-flow` and native platform drivers.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// Adaptive exponential backoff schedule for UI polling loops.
///
/// Starts fast (`initial`, default `15ms`) so immediate transitions complete without
/// wasting a full fixed-interval sleep, then doubles up to `max` (`250ms`) to keep
/// CPU and IPC load low on slower transitions. Always clamps to remaining deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollSchedule {
    initial: Duration,
    max: Duration,
    current: Duration,
    attempts: u32,
}

impl Default for PollSchedule {
    fn default() -> Self {
        Self::new(Duration::from_millis(15), Duration::from_millis(250))
    }
}

impl PollSchedule {
    /// Creates a new schedule clamped so `initial >= 1ms` and `max >= initial`.
    pub fn new(initial: Duration, max: Duration) -> Self {
        let floor = Duration::from_millis(1);
        let initial = initial.max(floor);
        let max = max.max(initial);
        Self {
            initial,
            max,
            current: initial,
            attempts: 0,
        }
    }

    /// Returns the next sleep duration clamped to `remaining`, or `None` when
    /// `remaining` is zero.
    pub fn next_delay(&mut self, remaining: Duration) -> Option<Duration> {
        if remaining.is_zero() {
            return None;
        }
        let delay = self.current.min(remaining);
        self.attempts = self.attempts.saturating_add(1);
        self.current = self.current.saturating_mul(2).min(self.max);
        Some(delay)
    }

    /// Number of sleep delays emitted since construction or last [`Self::reset`].
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Resets the schedule back to its initial interval.
    pub fn reset(&mut self) {
        self.current = self.initial;
        self.attempts = 0;
    }
}

/// Result of feeding a hierarchy hash into [`SettleTracker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleOutcome {
    /// The required number of consecutive identical hashes has been observed.
    Settled { consecutive: usize },
    /// Screen is still changing or has not yet reached the consecutive threshold.
    Unsettled { consecutive: usize },
}

impl SettleOutcome {
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Settled { .. })
    }
}

/// Tracks consecutive identical hierarchy hashes to detect settled UI states,
/// animation completion, and scroll-to-end bailouts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettleTracker {
    required_consecutive: usize,
    last_hash: Option<u64>,
    consecutive: usize,
}

impl Default for SettleTracker {
    fn default() -> Self {
        Self::new(2)
    }
}

impl SettleTracker {
    /// Creates a tracker requiring at least `required_consecutive` matching
    /// observations (minimum 1) before reporting [`SettleOutcome::Settled`].
    pub fn new(required_consecutive: usize) -> Self {
        Self {
            required_consecutive: required_consecutive.max(1),
            last_hash: None,
            consecutive: 0,
        }
    }

    /// Records the next chrome-stripped hierarchy hash and returns whether the UI has settled.
    pub fn observe_hash(&mut self, hash: u64) -> SettleOutcome {
        if self.last_hash == Some(hash) {
            self.consecutive = self.consecutive.saturating_add(1);
        } else {
            self.last_hash = Some(hash);
            self.consecutive = 1;
        }
        if self.consecutive >= self.required_consecutive {
            SettleOutcome::Settled {
                consecutive: self.consecutive,
            }
        } else {
            SettleOutcome::Unsettled {
                consecutive: self.consecutive,
            }
        }
    }

    /// Returns whether the most recent observation left the tracker settled.
    pub fn is_settled(&self) -> bool {
        self.consecutive >= self.required_consecutive
    }

    /// Most recently observed hash, if any.
    pub fn last_hash(&self) -> Option<u64> {
        self.last_hash
    }

    /// Clears observed history.
    pub fn reset(&mut self) {
        self.last_hash = None;
        self.consecutive = 0;
    }
}

/// Lightweight node projection used for structure-preserving, chrome-stripped stability hashing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashableNode<'a> {
    pub index: usize,
    pub parent_index: Option<usize>,
    pub depth: usize,
    pub source: &'a str,
    pub element_type: &'a str,
    pub identifier: &'a str,
    pub label: &'a str,
    pub title: &'a str,
    pub value: Option<&'a str>,
    pub enabled: bool,
    pub selected: bool,
    pub has_focus: bool,
    pub hittable: bool,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Returns `true` for volatile OS chrome (status bar clocks, battery, signal indicators,
/// home indicator / system navigation bar) whose entire subtree must be excluded from
/// settle and scroll-end comparisons so a ticking clock never registers as UI movement.
pub fn is_volatile_system_chrome(source: &str, element_type: &str, identifier: &str) -> bool {
    if element_type.eq_ignore_ascii_case("StatusBar") {
        return true;
    }
    if source.eq_ignore_ascii_case("systemui")
        || source.eq_ignore_ascii_case("com.android.systemui")
    {
        return true;
    }
    if identifier.starts_with("com.android.systemui:id/")
        || identifier.eq_ignore_ascii_case("statusBarBackground")
        || identifier.eq_ignore_ascii_case("navigationBarBackground")
    {
        return true;
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChromeStatus {
    Pruned,
    Retained,
}

/// Computes a deterministic 64-bit FNV-1a hash over all non-volatile nodes and their tree structure.
///
/// Any node classified by [`is_volatile_system_chrome`] — along with all of its descendants via
/// `parent_index` ancestry — is pruned in `O(n)` memoized time so ticking status-bar children
/// (e.g. a `StaticText` clock under a `StatusBar`) do not invalidate settle detection or
/// `knownHash` short-circuit caches. Retained nodes hash their structural ancestry (`depth` and
/// normalized parent ordinal), text fields (`identifier`, `label`, `title`, `value`), state flags
/// (`enabled`, `selected`, `has_focus`, `hittable`), and bounds.
pub fn hash_hierarchy_nodes<'a>(nodes: impl IntoIterator<Item = HashableNode<'a>>) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;

    #[inline]
    fn write_bytes(hash: &mut u64, bytes: &[u8]) {
        for &b in bytes {
            *hash ^= u64::from(b);
            *hash = hash.wrapping_mul(FNV_PRIME);
        }
    }

    #[inline]
    fn write_field(hash: &mut u64, value: &str) {
        write_bytes(hash, value.as_bytes());
        write_bytes(hash, &[0x1f]);
    }

    let collected: Vec<HashableNode<'a>> = nodes.into_iter().collect();
    let mut parent_by_index = HashMap::with_capacity(collected.len());
    let mut status_by_index = HashMap::with_capacity(collected.len());

    for node in &collected {
        parent_by_index.insert(node.index, node.parent_index);
        if is_volatile_system_chrome(node.source, node.element_type, node.identifier) {
            status_by_index.insert(node.index, ChromeStatus::Pruned);
        }
    }

    // Memoize both Retained and Pruned classifications in O(n) total work while handling cycles.
    let mut chain = Vec::new();
    let mut visiting = HashSet::new();
    for node in &collected {
        if status_by_index.contains_key(&node.index) {
            continue;
        }
        chain.clear();
        visiting.clear();
        let mut cursor = Some(node.index);
        let resolved = loop {
            let Some(idx) = cursor else {
                break ChromeStatus::Retained;
            };
            if let Some(&known) = status_by_index.get(&idx) {
                break known;
            }
            if !visiting.insert(idx) {
                // Defensive cycle guard: a cycle containing no volatile node is retained.
                break ChromeStatus::Retained;
            }
            chain.push(idx);
            cursor = parent_by_index.get(&idx).copied().flatten();
        };
        for idx in &chain {
            status_by_index.insert(*idx, resolved);
        }
    }

    // Assign stable ordinals to retained nodes so pruning an unrelated StatusBar sibling
    // never shifts retained parent references, while reparenting or depth changes always do.
    let mut retained_ordinal = HashMap::with_capacity(collected.len());
    let mut next_ordinal: u64 = 0;
    for node in &collected {
        if status_by_index.get(&node.index) == Some(&ChromeStatus::Retained) {
            retained_ordinal.insert(node.index, next_ordinal);
            next_ordinal = next_ordinal.saturating_add(1);
        }
    }

    let mut hash = FNV_OFFSET_BASIS;
    let mut included: u64 = 0;

    for node in &collected {
        if status_by_index.get(&node.index) != Some(&ChromeStatus::Retained) {
            continue;
        }
        included = included.saturating_add(1);

        let normalized_parent: u64 = node
            .parent_index
            .and_then(|p| retained_ordinal.get(&p).copied())
            .unwrap_or(u64::MAX);
        write_bytes(&mut hash, &normalized_parent.to_le_bytes());
        write_bytes(&mut hash, &(node.depth as u64).to_le_bytes());
        write_field(&mut hash, node.source);
        write_field(&mut hash, node.element_type);
        write_field(&mut hash, node.identifier);
        write_field(&mut hash, node.label);
        write_field(&mut hash, node.title);
        write_field(&mut hash, node.value.unwrap_or(""));
        let flags: u8 = u8::from(node.enabled)
            | (u8::from(node.selected) << 1)
            | (u8::from(node.has_focus) << 2)
            | (u8::from(node.hittable) << 3);
        write_bytes(&mut hash, &[flags]);
        write_bytes(&mut hash, &node.x.to_le_bytes());
        write_bytes(&mut hash, &node.y.to_le_bytes());
        write_bytes(&mut hash, &node.width.to_le_bytes());
        write_bytes(&mut hash, &node.height.to_le_bytes());
        write_bytes(&mut hash, &[0x1e]);
    }

    write_bytes(&mut hash, &included.to_le_bytes());
    hash
}

#[cfg(test)]
mod tests {
    use super::{
        HashableNode, PollSchedule, SettleOutcome, SettleTracker, hash_hierarchy_nodes,
        is_volatile_system_chrome,
    };
    use std::time::Duration;

    fn sample_node<'a>(
        index: usize,
        parent_index: Option<usize>,
        depth: usize,
        source: &'a str,
        element_type: &'a str,
        identifier: &'a str,
        label: &'a str,
    ) -> HashableNode<'a> {
        HashableNode {
            index,
            parent_index,
            depth,
            source,
            element_type,
            identifier,
            label,
            title: "",
            value: None,
            enabled: true,
            selected: false,
            has_focus: false,
            hittable: true,
            x: 20,
            y: 400,
            width: 350,
            height: 48,
        }
    }

    #[test]
    fn poll_schedule_doubles_caps_and_clamps_to_remaining() {
        let mut sched = PollSchedule::default();
        assert_eq!(
            sched.next_delay(Duration::from_secs(5)),
            Some(Duration::from_millis(15))
        );
        assert_eq!(
            sched.next_delay(Duration::from_secs(5)),
            Some(Duration::from_millis(30))
        );
        assert_eq!(
            sched.next_delay(Duration::from_secs(5)),
            Some(Duration::from_millis(60))
        );
        assert_eq!(
            sched.next_delay(Duration::from_secs(5)),
            Some(Duration::from_millis(120))
        );
        assert_eq!(
            sched.next_delay(Duration::from_secs(5)),
            Some(Duration::from_millis(240))
        );
        assert_eq!(
            sched.next_delay(Duration::from_secs(5)),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            sched.next_delay(Duration::from_millis(42)),
            Some(Duration::from_millis(42))
        );
        assert_eq!(sched.next_delay(Duration::ZERO), None);
        assert_eq!(sched.attempts(), 7);

        sched.reset();
        assert_eq!(sched.attempts(), 0);
        assert_eq!(
            sched.next_delay(Duration::from_secs(1)),
            Some(Duration::from_millis(15))
        );
    }

    #[test]
    fn poll_schedule_enforces_sensible_bounds() {
        let mut sched = PollSchedule::new(Duration::ZERO, Duration::ZERO);
        assert_eq!(
            sched.next_delay(Duration::from_millis(10)),
            Some(Duration::from_millis(1))
        );
    }

    #[test]
    fn settle_tracker_requires_consecutive_matching_hashes() {
        let mut tracker = SettleTracker::new(3);
        assert!(!tracker.is_settled());
        assert_eq!(
            tracker.observe_hash(100),
            SettleOutcome::Unsettled { consecutive: 1 }
        );
        assert_eq!(
            tracker.observe_hash(100),
            SettleOutcome::Unsettled { consecutive: 2 }
        );
        assert_eq!(
            tracker.observe_hash(200),
            SettleOutcome::Unsettled { consecutive: 1 }
        );
        assert_eq!(
            tracker.observe_hash(200),
            SettleOutcome::Unsettled { consecutive: 2 }
        );
        assert_eq!(
            tracker.observe_hash(200),
            SettleOutcome::Settled { consecutive: 3 }
        );
        assert!(tracker.is_settled());
        assert_eq!(tracker.last_hash(), Some(200));

        tracker.reset();
        assert!(!tracker.is_settled());
        assert_eq!(tracker.last_hash(), None);
    }

    #[test]
    fn hierarchy_hash_prunes_status_bar_descendants_and_detects_app_changes() {
        let root_app = sample_node(0, None, 0, "app", "Application", "", "DejaFixture");
        let status_bar = sample_node(1, Some(0), 1, "app", "StatusBar", "", "");
        let status_clock_1 = sample_node(2, Some(1), 2, "app", "StaticText", "", "9:41");
        let status_clock_2 = sample_node(2, Some(1), 2, "app", "StaticText", "", "9:42");
        let status_battery = sample_node(3, Some(1), 2, "app", "Other", "battery", "98%");
        let app_button = sample_node(
            4,
            Some(0),
            1,
            "app",
            "Button",
            "checkout_button",
            "Checkout",
        );
        let android_nav = sample_node(
            5,
            None,
            0,
            "com.android.systemui",
            "View",
            "com.android.systemui:id/clock",
            "9:42",
        );

        assert!(is_volatile_system_chrome("springboard", "StatusBar", ""));
        assert!(is_volatile_system_chrome(
            "app",
            "TextView",
            "com.android.systemui:id/battery"
        ));
        assert!(!is_volatile_system_chrome(
            "app",
            "Button",
            "checkout_button"
        ));

        let h1 = hash_hierarchy_nodes([root_app, status_bar, status_clock_1, app_button]);
        let h2 = hash_hierarchy_nodes([
            root_app,
            status_bar,
            status_clock_2,
            status_battery,
            app_button,
            android_nav,
        ]);
        assert_eq!(
            h1, h2,
            "ticking StatusBar StaticText children and extra systemui nodes must not change settle hash"
        );

        let changed_button = sample_node(
            4,
            Some(0),
            1,
            "app",
            "Button",
            "checkout_button",
            "Order placed",
        );
        let h3 = hash_hierarchy_nodes([root_app, status_bar, status_clock_2, changed_button]);
        assert_ne!(h1, h3, "real UI label change must produce a distinct hash");
    }

    #[test]
    fn hierarchy_hash_changes_on_title_only_update_and_reparenting() {
        let root = sample_node(0, None, 0, "app", "Application", "", "App");
        let group = sample_node(1, Some(0), 1, "app", "Group", "card", "");
        let mut child_inside_group = sample_node(2, Some(1), 2, "app", "Button", "buy_button", "");
        child_inside_group.title = "Buy now";

        let mut title_updated = child_inside_group;
        title_updated.title = "Added";

        let mut sibling_after_group = sample_node(2, Some(0), 1, "app", "Button", "buy_button", "");
        sibling_after_group.title = "Buy now";

        let h_nested = hash_hierarchy_nodes([root, group, child_inside_group]);
        let h_title_changed = hash_hierarchy_nodes([root, group, title_updated]);
        let h_sibling = hash_hierarchy_nodes([root, group, sibling_after_group]);

        assert_ne!(
            h_nested, h_title_changed,
            "title-only update must change hierarchy hash"
        );
        assert_ne!(
            h_nested, h_sibling,
            "moving a node out of a Group with identical preorder attributes/bounds must change the hash"
        );
    }

    #[test]
    fn hierarchy_hash_handles_deep_chains_and_cycles_linearly() {
        let deep: Vec<HashableNode<'_>> = (0..4_000)
            .map(|i| {
                sample_node(
                    i,
                    i.checked_sub(1),
                    i,
                    "app",
                    "Other",
                    "",
                    if i == 3_999 { "leaf" } else { "" },
                )
            })
            .collect();
        let h_a = hash_hierarchy_nodes(deep.iter().copied());
        let h_b = hash_hierarchy_nodes(deep.iter().copied());
        assert_eq!(h_a, h_b);

        // Defensive cycle check: cyclic parent_index pointers must terminate cleanly.
        let cycle = [
            sample_node(0, Some(1), 0, "app", "Group", "a", "A"),
            sample_node(1, Some(0), 1, "app", "Group", "b", "B"),
        ];
        assert_ne!(hash_hierarchy_nodes(cycle), 0);
    }
}
