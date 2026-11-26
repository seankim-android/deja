//! Pure domain core for Déjà: contracts, YAML 1.2 closure, assertion hashing,
//! manifest handling, redaction, and streaming build fingerprinting.

/// Canonical crate identifier used by workspace smoke checks.
pub const CRATE_NAME: &str = "deja-core";

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn crate_identity_matches() {
        assert_eq!(CRATE_NAME, "deja-core");
    }
}
