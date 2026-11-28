//! Pure domain core for Déjà: contracts, YAML 1.2 closure, assertion hashing,
//! manifest handling, secret scanning, redaction, and streaming build fingerprinting.

pub mod canonical_json;
pub mod contracts;
pub mod error;
pub mod fingerprint;
pub mod flow_assertions;
pub mod flow_closure;
pub mod flow_references;
pub mod flow_scalars;
pub mod manifest;
pub mod redaction;
pub mod secret_scan;
pub mod semantic;
pub mod yaml;

/// Canonical crate identifier used by workspace smoke checks.
pub const CRATE_NAME: &str = "deja-core";

pub use canonical_json::{
    canonical_json, cmp_utf16, js_json_number_to_string, js_number_to_string,
};
pub use contracts::{
    ArtifactKind, CONTRACT_VERSION, CacheMissReason, CacheStatus, CrashSignal, CrashSignalSource,
    DEFAULT_FLOW_SCHEMA_VERSION, EngineOverrideVia, EngineReport, EngineSource, ExecutionResult,
    ExplorationDetails, ExplorationResult, FailureCategory, FlowChange, GateFlakyFailure,
    GateGoalResult, GateIdentity, GateOptions, GateOutcome, Goal, LookupHit, LookupMiss,
    LookupResult, MAX_OBSERVED_ELEMENTS, NewBuildReplayReport, ROUTE_GRAPH_FALLBACK,
    ROUTE_GRAPH_SCHEMA_VERSION, RepairProvenance, RepairResult, RepairStatus, RunArtifact,
    RunAttempt, RunCommand, RunOutcome, RunResult, SCREEN_FINGERPRINT_V2,
    SCREEN_FINGERPRINT_VERSION, SemanticLookupReason, SemanticLookupReport, SemanticLookupStatus,
    SemanticSelectedWalk, SourceDiff, StaleBuildPolicy, StoredFlowFile, StoredValidation,
    StoredWalk, VERIFIED_SUBFLOW_SCHEMA_VERSION, WALK_RESULT_SCHEMA_VERSION, WalkFailure,
    WalkIdentity, WalkOrigin, WalkValidation, evaluate_gate_from_records, gate_passed,
    normalize_goal, trim_es_whitespace,
};
pub use error::CoreError;
pub use fingerprint::{
    BuildFingerprintCache, FINGERPRINT_CHUNK_BYTES, fingerprint_file, fingerprint_tree,
};
pub use flow_assertions::{
    ASSERTION_HASH_VERSION, ClosureAssertion, FlowAssertion, FlowAssertionCall,
    FlowAssertionCallKind, MAX_CLOSURE_ASSERTION_BYTES, MAX_CLOSURE_ASSERTIONS, assertions_sha256,
    closure_assertions, find_flow_assertions,
};
pub use flow_closure::{
    ClosureFile, FlowClosure, MAX_DEPTH, MAX_FILE_BYTES, MAX_FILES, MAX_MEDIA_BYTES,
    MAX_TOTAL_MEDIA_BYTES, PathChecksum, StoredClosureCheck, StoredClosurePlan,
    check_stored_subflows, closure_sha256_of, collect_flow_closure, plan_stored_closure,
    read_stored_flow_files, spelling_key,
};
pub use flow_references::{
    FlowReference, FlowReferenceCommand, FlowReferenceKind, find_flow_references,
};
pub use flow_scalars::find_flow_strings;
pub use manifest::{
    LaunchArgumentValue, MANIFEST_HASH_VERSION, ManifestOwnerRules, ManifestProfile, ManifestWalk,
    TextSelectorViolation, WALK_MANIFEST_VERSION, WalkManifest, check_manifest_flow_path,
    check_owner_rules, empty_walk_manifest, find_disallowed_text_selectors, manifest_walk_key,
    parse_walk_manifest, serialize_walk_manifest,
};
pub use redaction::{
    KEY_FLOOR, PASSWORD_FLOOR, ProviderKeyRedactor, Redactor, create_redactor, is_key_like_name,
    provider_key_redactor, redact_json, secret_floor,
};
pub use secret_scan::{
    MIN_FORBIDDEN_VALUE_LENGTH, bytes_hold_forbidden_value, holds_forbidden_value,
    json_holds_forbidden_value, spellings, validate_forbidden_values,
};
pub use semantic::{
    DEFAULT_MINIMUM_MARGIN, DEFAULT_MINIMUM_SIMILARITY, GoalEmbeddingProvider,
    LOCAL_LEXICAL_EMBEDDING_DIMENSIONS, LOCAL_LEXICAL_EMBEDDING_PROVIDER,
    LocalLexicalEmbeddingProvider, canonical_goal_tokens, cosine_similarity,
    goals_are_contradictory, has_adversarial_goal_marker,
};
pub use yaml::{MAX_ALIAS_EXPANSIONS, MAX_MATERIALIZED_BYTES, YamlValue, parse_yaml_documents};

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn crate_identity_matches() {
        assert_eq!(CRATE_NAME, "deja-core");
    }
}
