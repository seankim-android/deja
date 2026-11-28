//! Pure domain contracts, schema versions, goal normalization, and gate evaluation.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// Schema version for [`RunResult`].
pub const WALK_RESULT_SCHEMA_VERSION: u32 = 1;

/// Contract version for plugin boundaries.
pub const CONTRACT_VERSION: u32 = 1;

/// Default Maestro flow schema version.
pub const DEFAULT_FLOW_SCHEMA_VERSION: &str = "maestro/v1";

/// Screen fingerprint schema version 1 (diagnostic Android route graphs).
pub const SCREEN_FINGERPRINT_VERSION: u32 = 1;

/// Screen fingerprint schema version 2 (cross-platform exploration).
pub const SCREEN_FINGERPRINT_V2: u32 = 2;

/// Schema version for diagnostic route graphs.
pub const ROUTE_GRAPH_SCHEMA_VERSION: u32 = 1;

/// Fallback mode label when route graph matching does not authorize replay.
pub const ROUTE_GRAPH_FALLBACK: &str = "existing-exact-replay-or-exploration";

/// Upper bound on observed elements per screen capture.
pub const MAX_OBSERVED_ELEMENTS: usize = 400;

/// Schema version for verified subflow contracts.
pub const VERIFIED_SUBFLOW_SCHEMA_VERSION: u32 = 1;

/// A normalized goal: trimmed display text plus collapsed lowercase cache key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Goal {
    pub text: String,
    pub key: String,
}

pub(crate) fn is_js_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// Trims leading and trailing ECMAScript whitespace (`WhiteSpace` and `LineTerminator`
/// code points, including `\u{00A0}` and `\u{FEFF}`) matching TypeScript `String.prototype.trim`.
#[must_use]
pub fn trim_es_whitespace(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// Trim, lowercase, and collapse whitespace in a goal, matching TypeScript `normalizeGoal`.
pub fn normalize_goal(goal: &str) -> Result<Goal, CoreError> {
    let text = trim_es_whitespace(goal).to_string();
    let lower = text.to_lowercase();
    let key = lower
        .split(is_js_whitespace)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if key.is_empty() {
        return Err(CoreError::EmptyGoal);
    }
    Ok(Goal { text, key })
}

/// Exact cache identity for a walk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkIdentity {
    pub goal: String,
    pub app_id: String,
    pub platform: String,
    pub flow_schema_version: String,
    pub build_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<String>,
}

/// Latest validation timestamp and pass/fail status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkValidation {
    pub passed: bool,
    pub at: String,
}

/// Validation history entry bound to the flow checksum that ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredValidation {
    pub passed: bool,
    pub at: String,
    pub flow_sha256: String,
}

/// Provenance recorded when a bounded repair replaced a walk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepairProvenance {
    pub attempt_id: String,
    pub patch_path: String,
    pub previous_build_fingerprint: String,
    pub previous_flow_sha256: String,
}

/// Metrics recorded when a walk originated from cold exploration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplorationDetails {
    pub exploration_duration_ms: Option<u64>,
    pub verification_duration_ms: Option<u64>,
    pub mcp_calls: Option<u64>,
    pub model_calls: Option<u64>,
    pub model_call_basis: String,
}

/// A file referenced by a stored flow, paired with its SHA-256 checksum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredFlowFile {
    pub path: String,
    pub sha256: String,
}

/// How a stored walk was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WalkOrigin {
    Remember,
    Explore,
    Repair,
    ReplayedOnNewBuild,
    Verify,
}

/// A persisted walk record across JSON, SQLite, and manifest-backed stores.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredWalk {
    pub goal: String,
    pub app_id: String,
    pub platform: String,
    pub flow_schema_version: String,
    pub build_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<String>,
    pub goal_key: String,
    pub flow_path: String,
    pub flow_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subflow_files: Option<Vec<StoredFlowFile>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_closure: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closure_sha256: Option<String>,
    pub last_validation: WalkValidation,
    pub validation_history: Vec<StoredValidation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<WalkOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exploration: Option<ExplorationDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_history: Option<Vec<RepairProvenance>>,
}

impl StoredWalk {
    /// Extract the exact cache identity for this stored walk.
    #[must_use]
    pub fn identity(&self) -> WalkIdentity {
        WalkIdentity {
            goal: self.goal.clone(),
            app_id: self.app_id.clone(),
            platform: self.platform.clone(),
            flow_schema_version: self.flow_schema_version.clone(),
            build_fingerprint: self.build_fingerprint.clone(),
            adapter: self.adapter.clone(),
            browser: self.browser.clone(),
        }
    }
}

/// Exact cache hit from a walk store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LookupHit {
    pub walk: StoredWalk,
    pub flow_path: String,
}

/// Why an exact walk store lookup missed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheMissReason {
    NotFound,
    WrongApp,
    WrongPlatform,
    WrongFlowSchema,
    WrongAdapter,
    WrongBrowser,
    StaleBuild,
    MissingFlow,
    FailedValidation,
    LegacyClosureUnknown,
    FlowChanged,
    NotValidated,
    NeedsMigration,
}

impl CacheMissReason {
    /// Wire kebab-case name matching TypeScript `CacheMissReason`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not-found",
            Self::WrongApp => "wrong-app",
            Self::WrongPlatform => "wrong-platform",
            Self::WrongFlowSchema => "wrong-flow-schema",
            Self::WrongAdapter => "wrong-adapter",
            Self::WrongBrowser => "wrong-browser",
            Self::StaleBuild => "stale-build",
            Self::MissingFlow => "missing-flow",
            Self::FailedValidation => "failed-validation",
            Self::LegacyClosureUnknown => "legacy-closure-unknown",
            Self::FlowChanged => "flow-changed",
            Self::NotValidated => "not-validated",
            Self::NeedsMigration => "needs-migration",
        }
    }
}

/// Closure hash pair attached to a `flow-changed` miss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowChange {
    pub manifest_closure_sha256: String,
    pub current_closure_sha256: String,
}

/// Exact cache miss from a walk store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LookupMiss {
    pub reason: CacheMissReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_change: Option<FlowChange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closure_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_path: Option<String>,
}

/// Tagged union matching TypeScript `LookupHit | LookupMiss`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum LookupResult {
    Hit(Box<LookupHit>),
    Miss(LookupMiss),
}

/// Status of optional semantic retrieval on a run report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SemanticLookupStatus {
    Hit,
    Miss,
    Unavailable,
    Disabled,
    SkippedExactHit,
    NotApplicable,
}

/// Reason a semantic retrieval attempt missed or became unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SemanticLookupReason {
    ProviderUnavailable,
    AdversarialGoal,
    ContradictoryGoal,
    NoEligibleCandidates,
    LowConfidence,
    Ambiguous,
}

/// Walk selected by semantic retrieval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticSelectedWalk {
    pub goal: String,
    pub goal_key: String,
    pub flow_path: String,
    pub flow_sha256: String,
}

/// Additive schema-1 report data for semantic retrieval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticLookupReport {
    pub status: SemanticLookupStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<SemanticLookupReason>,
    pub provider: Option<String>,
    pub embedding_calls: u64,
    pub duration_ms: u64,
    pub candidates: usize,
    pub eligible_candidates: usize,
    pub minimum_similarity: Option<f64>,
    pub minimum_margin: Option<f64>,
    pub goal_similarity: Option<f64>,
    pub runner_up_similarity: Option<f64>,
    pub selected_walk: Option<SemanticSelectedWalk>,
}

/// Conservative advisory failure category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    Selector,
    Navigation,
    ProductAssertion,
    Runner,
    Timeout,
    Unknown,
    AppCrash,
}

/// Explicit evidence source for an `app_crash` failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CrashSignalSource {
    MaestroOutput,
    CrashReport,
}

/// Metadata-only crash signal evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashSignal {
    pub source: CrashSignalSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
}

/// Structured advisory failure summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkFailure {
    pub category: FailureCategory,
    pub advisory: bool,
    pub repair_eligible: bool,
    pub failing_step: Option<String>,
    pub raw_artifact_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crash: Option<CrashSignal>,
}

/// Kind of artifact emitted by a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    Yaml,
    Output,
    Attempt,
    RepairReport,
    Diff,
    Memory,
    ModelMetrics,
    Junit,
    Manifest,
}

/// Artifact path (and optional checksum) recorded on a run report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunArtifact {
    pub kind: ArtifactKind,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// Non-auto-committed source diff produced by a bounded selector repair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceDiff {
    pub path: Option<String>,
    pub affected_files: Vec<String>,
    pub contents: String,
    pub auto_commit: bool,
}

/// Execution result returned by a walk runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionResult {
    pub passed: bool,
    pub started_at: String,
    pub ended_at: String,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    pub assertions: Option<u64>,
    pub failure: Option<WalkFailure>,
    pub artifacts: Vec<RunArtifact>,
}

/// Result returned by a cold walk explorer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplorationResult {
    pub passed: bool,
    pub duration_ms: u64,
    pub exploration_duration_ms: Option<u64>,
    pub verification_duration_ms: Option<u64>,
    pub mcp_calls: Option<u64>,
    pub model_calls: Option<u64>,
    pub model_call_basis: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub assertions: Option<u64>,
    pub flow_path: Option<String>,
    pub flow_sha256: Option<String>,
    pub failure: Option<WalkFailure>,
    pub artifacts: Vec<RunArtifact>,
    pub errors: Vec<String>,
}

/// Status of a bounded selector repair attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RepairStatus {
    Repaired,
    Rejected,
    Failed,
}

/// Result returned by a bounded selector repairer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepairResult {
    pub status: RepairStatus,
    pub passed: bool,
    pub duration_ms: u64,
    pub model_calls: u64,
    pub repair_count: u8,
    pub assertions: Option<u64>,
    pub flow_path: Option<String>,
    pub flow_sha256: Option<String>,
    pub failure: Option<WalkFailure>,
    pub artifacts: Vec<RunArtifact>,
    pub diffs: Vec<SourceDiff>,
    pub errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Whether exact walk memory was checked and hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheStatus {
    Hit,
    Miss,
    NotChecked,
}

/// Top-level command recorded in a run report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunCommand {
    Explore,
    Run,
    Remember,
}

/// Terminal outcome of a run, remember, or explore command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunOutcome {
    Passed,
    Remembered,
    ColdRecorded,
    Repaired,
    Rejected,
    ExplorationRejected,
    ProductRegression,
    RepairRejected,
    Failed,
    FlowChanged,
    NotValidated,
    NeedsMigration,
    AssertionsChanged,
    Flaky,
    ClosureError,
}

/// One replay attempt of the same stored bytes (opt-in rerun).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunAttempt {
    pub attempt: u8,
    pub passed: bool,
    pub started_at: String,
    pub ended_at: String,
    pub duration_ms: u64,
    pub failure: Option<WalkFailure>,
    pub artifacts: Vec<RunArtifact>,
}

/// Opt-in policy for replaying the latest verified walk on a new build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StaleBuildPolicy {
    ReplayLastVerified,
}

/// Additive schema-1 report data for new-build replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewBuildReplayReport {
    pub policy: StaleBuildPolicy,
    pub previous_build_fingerprint: String,
    pub previous_flow_sha256: String,
    pub previous_validated_at: String,
    pub recorded_for_new_build: bool,
}

/// Where the execution engine binary came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngineSource {
    Pinned,
    Override,
}

/// How an execution engine override was supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngineOverrideVia {
    Config,
    Env,
}

/// Additive schema-1 report data for execution engine provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineReport {
    pub version: Option<String>,
    pub source: EngineSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_via: Option<EngineOverrideVia>,
    pub in_range: bool,
}

/// Schema-1 run report written by CLI and MCP commands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub schema_version: u32,
    pub command: RunCommand,
    pub identity: WalkIdentity,
    pub goal_key: String,
    pub cache_status: CacheStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_miss_reason: Option<CacheMissReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic: Option<SemanticLookupReport>,
    pub outcome: RunOutcome,
    pub passed: bool,
    pub started_at: String,
    pub ended_at: String,
    pub duration_ms: u64,
    pub model_calls: Option<u64>,
    pub model_call_basis: String,
    pub assertions: Option<u64>,
    pub repair_count: u8,
    pub yaml_path: Option<String>,
    pub yaml_sha256: Option<String>,
    pub failure: Option<WalkFailure>,
    pub artifacts: Vec<RunArtifact>,
    pub diffs: Vec<SourceDiff>,
    pub errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exploration: Option<ExplorationResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<RepairResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replayed_on_new_build: Option<NewBuildReplayReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<EngineReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<Vec<RunAttempt>>,
}

/// Release gate outcome for one goal on one exact build fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GateOutcome {
    Passed,
    Failed,
    Flaky,
    NotRun,
    FlowChanged,
    Invalid,
}

/// Release gate verdict for a single goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateGoalResult {
    pub goal: String,
    pub goal_key: String,
    pub outcome: GateOutcome,
    pub validated_at: Option<String>,
    pub flow_sha256: Option<String>,
    pub closure_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Exact build and platform identity evaluated by the release gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateIdentity {
    pub app_id: String,
    pub platform: String,
    pub flow_schema_version: String,
    pub build_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<String>,
}

/// A failed validation attributed to a flaky run by a saved run report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateFlakyFailure {
    pub goal_key: String,
    pub build_fingerprint: String,
    pub at: String,
}

/// Options for evaluating the release gate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateOptions {
    #[serde(default)]
    pub flaky_failures: Vec<GateFlakyFailure>,
}

/// Returns `true` only when at least one goal was checked and every goal passed.
#[must_use]
pub fn gate_passed(results: &[GateGoalResult]) -> bool {
    !results.is_empty()
        && results
            .iter()
            .all(|result| result.outcome == GateOutcome::Passed)
}

fn same_gate_logical_identity(walk: &StoredWalk, identity: &GateIdentity) -> bool {
    walk.app_id == identity.app_id
        && walk.platform == identity.platform
        && walk.flow_schema_version == identity.flow_schema_version
        && walk.adapter == identity.adapter
        && walk.browser == identity.browser
        && walk.build_fingerprint == identity.build_fingerprint
}

/// Evaluate release-gate outcomes from recorded walks and a read-only lookup callback.
///
/// Mirrors TypeScript `evaluateGate`: normalizes and deduplicates goals, never compares
/// timestamps, treats any non-`passed` outcome as closed, and turns lookup errors into `invalid`.
pub fn evaluate_gate_from_records<F>(
    walks: &[StoredWalk],
    goals: &[String],
    identity: &GateIdentity,
    manifest_mode: bool,
    options: &GateOptions,
    mut lookup: F,
) -> Result<Vec<GateGoalResult>, CoreError>
where
    F: FnMut(&WalkIdentity) -> Result<LookupResult, String>,
{
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    for raw in goals {
        let Goal { text, key } = normalize_goal(raw)?;
        if !seen.insert(key.clone()) {
            continue;
        }

        let walk_identity = WalkIdentity {
            goal: text.clone(),
            app_id: identity.app_id.clone(),
            platform: identity.platform.clone(),
            flow_schema_version: identity.flow_schema_version.clone(),
            build_fingerprint: identity.build_fingerprint.clone(),
            adapter: identity.adapter.clone(),
            browser: identity.browser.clone(),
        };

        let resolved = match lookup(&walk_identity) {
            Ok(res) => res,
            Err(message) => {
                results.push(GateGoalResult {
                    goal: text,
                    goal_key: key,
                    outcome: GateOutcome::Invalid,
                    validated_at: None,
                    flow_sha256: None,
                    closure_sha256: None,
                    detail: Some(message),
                });
                continue;
            }
        };

        match resolved {
            LookupResult::Hit(hit) => {
                results.push(GateGoalResult {
                    goal: text,
                    goal_key: key,
                    outcome: GateOutcome::Passed,
                    validated_at: Some(hit.walk.last_validation.at),
                    flow_sha256: Some(hit.walk.flow_sha256),
                    closure_sha256: hit.walk.closure_sha256,
                    detail: None,
                });
            }
            LookupResult::Miss(miss) => match miss.reason {
                CacheMissReason::FlowChanged => {
                    let (closure_sha256, detail) = match miss.flow_change {
                        Some(change) => (
                            Some(change.current_closure_sha256.clone()),
                            format!(
                                "the flow or a file it runs changed (manifest {}, now {})",
                                change.manifest_closure_sha256, change.current_closure_sha256
                            ),
                        ),
                        None => (None, "the flow or a file it runs changed".to_string()),
                    };
                    results.push(GateGoalResult {
                        goal: text,
                        goal_key: key,
                        outcome: GateOutcome::FlowChanged,
                        validated_at: None,
                        flow_sha256: None,
                        closure_sha256,
                        detail: Some(detail),
                    });
                }
                CacheMissReason::MissingFlow => {
                    results.push(GateGoalResult {
                        goal: text,
                        goal_key: key,
                        outcome: GateOutcome::Invalid,
                        validated_at: None,
                        flow_sha256: None,
                        closure_sha256: None,
                        detail: Some("the flow file is missing".to_string()),
                    });
                }
                CacheMissReason::FailedValidation => {
                    let failed = walks.iter().find(|walk| {
                        walk.goal_key == key
                            && same_gate_logical_identity(walk, identity)
                            && !walk.last_validation.passed
                            && (!manifest_mode || walk.closure_sha256 == miss.closure_sha256)
                    });
                    if let Some(failed) = failed {
                        let flaky = options.flaky_failures.iter().any(|item| {
                            item.goal_key == key
                                && item.build_fingerprint == identity.build_fingerprint
                                && item.at == failed.last_validation.at
                        });
                        results.push(GateGoalResult {
                            goal: text,
                            goal_key: key,
                            outcome: if flaky {
                                GateOutcome::Flaky
                            } else {
                                GateOutcome::Failed
                            },
                            validated_at: Some(failed.last_validation.at.clone()),
                            flow_sha256: Some(failed.flow_sha256.clone()),
                            closure_sha256: failed.closure_sha256.clone(),
                            detail: Some(if flaky {
                                "the latest run on this build failed, then passed on a rerun or failed differently"
                                    .to_string()
                            } else {
                                "the latest validation on this build is a failure".to_string()
                            }),
                        });
                    } else {
                        results.push(GateGoalResult {
                            goal: text,
                            goal_key: key,
                            outcome: GateOutcome::NotRun,
                            validated_at: None,
                            flow_sha256: None,
                            closure_sha256: None,
                            detail: Some(format!(
                                "no passing validation on this build ({})",
                                miss.reason.as_str()
                            )),
                        });
                    }
                }
                other => {
                    results.push(GateGoalResult {
                        goal: text,
                        goal_key: key,
                        outcome: GateOutcome::NotRun,
                        validated_at: None,
                        flow_sha256: None,
                        closure_sha256: None,
                        detail: Some(format!(
                            "no passing validation on this build ({})",
                            other.as_str()
                        )),
                    });
                }
            },
        }
    }

    Ok(results)
}
