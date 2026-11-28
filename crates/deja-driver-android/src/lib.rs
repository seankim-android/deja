//! Native Android driver (`adb` + bundled UiAutomator2 instrumentation APK).

/// Canonical crate identifier used by workspace smoke checks.
pub const CRATE_NAME: &str = "deja-driver-android";

/// Direct upstream workspace crates linked by `deja-driver-android`.
pub const UPSTREAM_CRATES: &[&str] = &[deja_core::CRATE_NAME, deja_flow::CRATE_NAME];

#[cfg(test)]
mod tests {
    use super::{CRATE_NAME, UPSTREAM_CRATES};

    #[test]
    fn crate_identity_matches() {
        assert_eq!(CRATE_NAME, "deja-driver-android");
        assert_eq!(UPSTREAM_CRATES, &["deja-core", "deja-flow"]);
    }
}
