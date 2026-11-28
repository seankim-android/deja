//! Explicit driver capability contracts, observation provenance, and visible-element error context.
//!
//! Backends advertise only the capabilities they genuinely support and fail closed with
//! [`UnsupportedCapability`] rather than silently emulating missing platform primitives.

use std::collections::BTreeSet;
use std::fmt;

/// Discrete platform driver capabilities that callers can query or require.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DriverCapability {
    AccessibilityTree,
    SystemBack,
    AppInventory,
    ClearState,
    ClearKeychain,
    CompoundAct,
    ServerSideWait,
    HierarchyHashShortCircuit,
    HoldScroll,
    DisableAnimations,
    AppCrashReports,
    PixelOcrFallback,
    StateCheckpoints,
}

impl DriverCapability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AccessibilityTree => "accessibility_tree",
            Self::SystemBack => "system_back",
            Self::AppInventory => "app_inventory",
            Self::ClearState => "clear_state",
            Self::ClearKeychain => "clear_keychain",
            Self::CompoundAct => "compound_act",
            Self::ServerSideWait => "server_side_wait",
            Self::HierarchyHashShortCircuit => "hierarchy_hash_short_circuit",
            Self::HoldScroll => "hold_scroll",
            Self::DisableAnimations => "disable_animations",
            Self::AppCrashReports => "app_crash_reports",
            Self::PixelOcrFallback => "pixel_ocr_fallback",
            Self::StateCheckpoints => "state_checkpoints",
        }
    }
}

impl fmt::Display for DriverCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Fail-closed error returned when a flow or caller requests an unsupported platform capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedCapability {
    pub platform: String,
    pub capability: DriverCapability,
    pub reason: String,
}

impl fmt::Display for UnsupportedCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "platform `{}` does not support capability `{}`: {}",
            self.platform, self.capability, self.reason
        )
    }
}

impl std::error::Error for UnsupportedCapability {}

/// Set of capabilities declared by a concrete `PlatformDriver` implementation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriverCapabilities {
    supported: BTreeSet<DriverCapability>,
}

impl FromIterator<DriverCapability> for DriverCapabilities {
    fn from_iter<T: IntoIterator<Item = DriverCapability>>(iter: T) -> Self {
        Self {
            supported: iter.into_iter().collect(),
        }
    }
}

impl DriverCapabilities {
    pub fn with(caps: impl IntoIterator<Item = DriverCapability>) -> Self {
        caps.into_iter().collect()
    }

    pub fn insert(&mut self, cap: DriverCapability) {
        self.supported.insert(cap);
    }

    pub fn supports(&self, cap: DriverCapability) -> bool {
        self.supported.contains(&cap)
    }

    pub fn require(
        &self,
        cap: DriverCapability,
        platform: &str,
        reason: impl Into<String>,
    ) -> Result<(), UnsupportedCapability> {
        if self.supports(cap) {
            Ok(())
        } else {
            Err(UnsupportedCapability {
                platform: platform.to_string(),
                capability: cap,
                reason: reason.into(),
            })
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = DriverCapability> + '_ {
        self.supported.iter().copied()
    }
}

/// Provenance of observed UI text/elements so fuzzy OCR hits are never silently
/// confused with exact accessibility tree nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObservationSource {
    /// Exact OS accessibility or DOM tree node.
    Tree,
    /// Inferred from screenshot pixels via local OCR fallback (e.g. Canvas/Metal/game views).
    Pixels,
}

impl ObservationSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tree => "tree",
            Self::Pixels => "pixels",
        }
    }
}

/// Compact, bounded snapshot of what IS currently visible on screen, attached to
/// selector-miss and assertion errors so exploration, repair, and MCP agents do not
/// need a second round-trip to inspect why a step failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleContext {
    items: Vec<String>,
    truncated: bool,
}

impl VisibleContext {
    /// Default maximum number of distinct visible descriptors captured on failure.
    pub const DEFAULT_LIMIT: usize = 30;

    /// Builds a deduplicated, whitespace-normalized visible summary capped at `limit` items.
    pub fn from_candidates<'a>(
        candidates: impl IntoIterator<Item = &'a str>,
        limit: usize,
    ) -> Self {
        let mut items = Vec::new();
        let mut seen = BTreeSet::new();
        let mut truncated = false;

        for raw in candidates {
            let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.is_empty() {
                continue;
            }
            if !seen.insert(normalized.clone()) {
                continue;
            }
            if items.len() < limit {
                items.push(normalized);
            } else {
                truncated = true;
                break;
            }
        }

        Self { items, truncated }
    }

    pub fn items(&self) -> &[String] {
        &self.items
    }

    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Renders a concise single-line summary suitable for error diagnostics.
    pub fn format_summary(&self) -> String {
        if self.items.is_empty() {
            return "saw: <no visible labels or ids>".to_string();
        }
        let joined = self.items.join(", ");
        if self.truncated {
            format!("saw: [{joined}, ...]")
        } else {
            format!("saw: [{joined}]")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DriverCapabilities, DriverCapability, ObservationSource, VisibleContext};

    #[test]
    fn capabilities_require_fails_closed_for_unsupported_op() {
        let caps = DriverCapabilities::with([
            DriverCapability::AccessibilityTree,
            DriverCapability::CompoundAct,
            DriverCapability::ServerSideWait,
        ]);
        assert!(caps.supports(DriverCapability::CompoundAct));
        assert!(!caps.supports(DriverCapability::SystemBack));
        assert!(
            caps.require(DriverCapability::CompoundAct, "ios", "n/a")
                .is_ok()
        );

        let err = caps
            .require(
                DriverCapability::SystemBack,
                "ios",
                "iOS has no system Back button",
            )
            .expect_err("system_back must fail closed on iOS");
        assert!(err.to_string().contains("system_back"));
        assert!(err.to_string().contains("iOS has no system Back button"));
    }

    #[test]
    fn observation_source_labels_are_distinct() {
        assert_eq!(ObservationSource::Tree.as_str(), "tree");
        assert_eq!(ObservationSource::Pixels.as_str(), "pixels");
        assert_ne!(ObservationSource::Tree, ObservationSource::Pixels);
    }

    #[test]
    fn visible_context_deduplicates_normalizes_and_bounds() {
        let ctx = VisibleContext::from_candidates(
            [
                "  ",
                "Checkout",
                "Checkout",
                "Add   to\ncart",
                "Total $12",
                "Pay",
            ],
            3,
        );
        assert_eq!(ctx.items(), &["Checkout", "Add to cart", "Total $12"]);
        assert!(ctx.is_truncated());
        assert_eq!(
            ctx.format_summary(),
            "saw: [Checkout, Add to cart, Total $12, ...]"
        );

        let empty = VisibleContext::from_candidates(["", "   "], VisibleContext::DEFAULT_LIMIT);
        assert_eq!(empty.format_summary(), "saw: <no visible labels or ids>");
    }
}
