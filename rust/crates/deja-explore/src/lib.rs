//! Graph explorer, choosers, path memory, and bounded step repair.

/// Canonical crate identifier used by workspace smoke checks.
pub const CRATE_NAME: &str = "deja-explore";

/// Direct upstream workspace crates linked by `deja-explore`.
pub const UPSTREAM_CRATES: &[&str] = &[
    deja_core::CRATE_NAME,
    deja_flow::CRATE_NAME,
    deja_store::CRATE_NAME,
];

#[cfg(test)]
mod tests {
    use super::{CRATE_NAME, UPSTREAM_CRATES};

    #[test]
    fn crate_identity_matches() {
        assert_eq!(CRATE_NAME, "deja-explore");
        assert_eq!(UPSTREAM_CRATES, &["deja-core", "deja-flow", "deja-store"]);
    }
}
