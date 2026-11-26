//! Platform-neutral `maestro/v1` YAML interpreter, `PlatformDriver` abstraction,
//! selector matching, structured failure classification, and JUnit XML writer.

/// Canonical crate identifier used by workspace smoke checks.
pub const CRATE_NAME: &str = "deja-flow";

/// Direct upstream workspace crates linked by `deja-flow`.
pub const UPSTREAM_CRATES: &[&str] = &[deja_core::CRATE_NAME];

#[cfg(test)]
mod tests {
    use super::{CRATE_NAME, UPSTREAM_CRATES};

    #[test]
    fn crate_identity_matches() {
        assert_eq!(CRATE_NAME, "deja-flow");
        assert_eq!(UPSTREAM_CRATES, &["deja-core"]);
    }
}
