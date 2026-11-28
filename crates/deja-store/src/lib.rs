//! Unified SQLite v5 store, writer locks, and consumer flow manifest persistence.

/// Canonical crate identifier used by workspace smoke checks.
pub const CRATE_NAME: &str = "deja-store";

/// Direct upstream workspace crates linked by `deja-store`.
pub const UPSTREAM_CRATES: &[&str] = &[deja_core::CRATE_NAME];

#[cfg(test)]
mod tests {
    use super::{CRATE_NAME, UPSTREAM_CRATES};

    #[test]
    fn crate_identity_matches() {
        assert_eq!(CRATE_NAME, "deja-store");
        assert_eq!(UPSTREAM_CRATES, &["deja-core"]);
    }
}
