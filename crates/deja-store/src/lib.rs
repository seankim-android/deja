//! Unified SQLite v5 store, zero-`unsafe` writer locking, consumer flow manifest lifecycle
//! (`DEJA-51`), multi-store/multi-runtime release gate (`DEJA-49`, `DEJA-55`, `DEJA-42`),
//! persistent build fingerprint cache (`DEJA-53`), and legacy `walks.json` read-only importer.

pub mod consumer_flows;
pub mod error;
pub mod gate;
pub mod lock;
pub mod migrate;
pub mod schema;
pub mod store;

/// Canonical crate identifier used by workspace smoke checks.
pub const CRATE_NAME: &str = "deja-store";

/// Direct upstream workspace crates linked by `deja-store`.
pub const UPSTREAM_CRATES: &[&str] = &[deja_core::CRATE_NAME];

pub use consumer_flows::{
    BeforeManifestCommitHook, ConsumerFlows, ConsumerFlowsOptions, ConsumerRunPlan,
    ConsumerRunRequest, InspectedConsumerEntry, PostRunValidationOptions, SaveManifestWalkOptions,
    check_flows_dir, check_manifest_path,
};
pub use error::{GATE_INVALID_EXIT_CODE, StoreError};
pub use gate::{StoreGateGoalResult, StoreGateOptions, evaluate_gate, store_gate_passed};
pub use lock::{
    BeforeMoveAsideHook, CanonicalMetadataTarget, LEGACY_JSON_MEMORY_PATH, LockHolder,
    SQLITE_MEMORY_PATH, WriterLock, canonical_metadata_target, current_hostname,
    inspect_lock_holder, is_stale, is_valid_iso8601, machine_id, now_iso8601,
    parse_proc_start_ticks, parse_rfc3339_utc, process_start, store_metadata_path,
    validate_store_id,
};
pub use migrate::{LegacyImportReport, import_legacy_walks_json, parse_legacy_walks_json};
pub use schema::{
    BuildFingerprintCacheIntegrityFields, MIN_SQLITE_SCHEMA_VERSION, SQLITE_SCHEMA_VERSION,
    ValidationHistoryIntegrityFields, ValidationRunIntegrityFields, WalkIntegrityFields,
    apply_readonly_pragmas, apply_writable_pragmas, canonical_json_value,
    compute_build_fingerprint_cache_integrity, compute_goal_embedding_integrity,
    compute_repair_history_integrity, compute_validation_history_integrity,
    compute_validation_run_integrity, compute_walk_file_integrity, compute_walk_integrity,
    ensure_writable_schema, read_user_version, verify_build_fingerprint_cache_integrity,
    verify_goal_embedding_integrity, verify_readonly_schema, verify_repair_history_integrity,
    verify_validation_history_integrity, verify_validation_run_integrity,
    verify_walk_file_integrity, verify_walk_integrity,
};
pub use store::{
    CandidateSnapshot, CandidateSnapshotFile, FingerprintCacheResult, GoalEmbeddingRecord,
    NormalizedWalkIdentity, PersistVerifiedCandidateOptions, RecordValidationInput,
    SqliteStoreOptions, SqliteWalkStore, UpsertWalkInput, ValidationRunRecord,
    normalize_walk_identity,
};

#[cfg(test)]
mod tests {
    use super::{CRATE_NAME, SqliteWalkStore, UPSTREAM_CRATES};

    #[test]
    fn crate_identity_matches() {
        assert_eq!(CRATE_NAME, "deja-store");
        assert_eq!(UPSTREAM_CRATES, &["deja-core"]);
    }

    #[test]
    fn sqlite_walk_store_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SqliteWalkStore>();
    }
}
