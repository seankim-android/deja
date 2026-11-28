//! MCP tool definitions, strict input validation, warm-session execution, and redacted
//! structured responses for `deja_list`, `deja_inspect`, `deja_run`, `deja_remember`,
//! `deja_exec_batch` (`deja_run_steps` alias), and gated `deja_explore`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deja_core::{
    ArtifactKind, BuildFingerprintCache, CacheMissReason, CacheStatus, CoreError,
    DEFAULT_FLOW_SCHEMA_VERSION, EngineReport, EngineSource, ExecutionResult, FailureCategory,
    FlowClosure, FlowReferenceKind, Goal, LookupResult, ManifestWalk, NewBuildReplayReport,
    RunArtifact, RunAttempt, RunCommand, RunOutcome, RunResult, StaleBuildPolicy, StoredWalk,
    WALK_RESULT_SCHEMA_VERSION, WalkFailure, WalkIdentity, WalkOrigin, assertions_sha256,
    bytes_hold_forbidden_value, check_manifest_flow_path, check_owner_rules, closure_assertions,
    collect_flow_closure, holds_forbidden_value, manifest_walk_key, normalize_goal,
};
use deja_flow::{
    ClassificationInput, DriverCapability, DriverError, EnvStack, FailureClassifier,
    FlowExecutionConfig, FlowExecutionOutcome, FlowExecutor, FlowStep, ParsedFlow, PlatformDriver,
    RunFlowTarget, RunTimingBreakdown, VisibleContext, count_assertions_in_closure, parse_flow,
    validate_closure_app_id_confinement, validate_flow_app_id_confinement,
};
use deja_store::{
    ConsumerFlows, ConsumerFlowsOptions, ConsumerRunPlan, ConsumerRunRequest,
    InspectedConsumerEntry, PersistVerifiedCandidateOptions, PostRunValidationOptions,
    RecordValidationInput, SaveManifestWalkOptions, SqliteStoreOptions, SqliteWalkStore,
    StoreError, UpsertWalkInput, now_iso8601,
};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::backend::{McpDriverContext, McpDriverFactory, McpServerConfig};
use crate::fs_safe::{
    parse_safe_relative_segments, read_confined_report_file, validate_remember_flow_path,
    verify_closure_files_nofollow, workspace_relative_display, write_atomic_confined,
};
use crate::redaction::RedactionContext;

/// Maximum serialized byte size of `steps` accepted by `deja_exec_batch` / `deja_run_steps` (64 KiB).
pub const MAX_BATCH_YAML_BYTES: usize = 64 * 1024;

/// Minimum top-level steps in `deja_exec_batch`.
pub const MIN_BATCH_STEPS: usize = 1;

/// Maximum top-level steps in `deja_exec_batch` (checked post-parse to block YAML anchor amplification).
pub const MAX_BATCH_STEPS: usize = 50;

/// Maximum total AST step nodes allowed across nested inline blocks/hooks in `deja_exec_batch`.
const MAX_BATCH_TOTAL_AST_STEPS: usize = 250;

/// Maximum timeout override accepted by `deja_exec_batch` (10 minutes).
const MAX_BATCH_TIMEOUT_MS: u64 = 600_000;

/// Maximum retries accepted by `deja_run` / `deja_remember` over MCP.
const MAX_MCP_RETRIES: u8 = 5;

/// Maximum characters preserved per error/diagnostic tail entry in `deja_exec_batch`.
const MAX_OUTPUT_TAIL_CHARS: usize = 4_000;

/// Maximum number of error/diagnostic tail entries returned in `deja_exec_batch`.
const MAX_OUTPUT_TAIL_ITEMS: usize = 20;

/// Bounded driver shutdown grace period (5 seconds).
pub(crate) const DRIVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

static MCP_REPORT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Warm device/driver and build-fingerprint state serialized behind `exec_mutex`.
pub struct WarmSessionState {
    pub fingerprint_cache: BuildFingerprintCache,
    pub driver: Option<Box<dyn PlatformDriver>>,
    pub installed_build_fingerprint: Option<String>,
    pub installed_runtime: Option<String>,
}

impl Default for WarmSessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl WarmSessionState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            fingerprint_cache: BuildFingerprintCache::new(),
            driver: None,
            installed_build_fingerprint: None,
            installed_runtime: None,
        }
    }

    /// Tear down any warm driver with a bounded timeout and clear installed-build tracking.
    pub async fn teardown_driver(&mut self) {
        self.installed_build_fingerprint = None;
        self.installed_runtime = None;
        if let Some(driver) = self.driver.take() {
            let _ = tokio::time::timeout(DRIVER_SHUTDOWN_TIMEOUT, driver.shutdown()).await;
        }
    }

    /// Compute or reuse a stat-verified streaming SHA-256 build fingerprint (`DEJA-53`).
    pub fn fingerprint_build(
        &mut self,
        workspace_root: &Path,
        build_path: &Path,
        platform: &str,
    ) -> Result<String, CoreError> {
        let lmeta = fs::symlink_metadata(build_path).map_err(|err| {
            CoreError::Fingerprint(format!(
                "cannot inspect buildPath {}: {err}",
                build_path.display()
            ))
        })?;
        if lmeta.file_type().is_symlink() {
            return Err(CoreError::Fingerprint(format!(
                "buildPath must not be a symbolic link: {}",
                build_path.display()
            )));
        }
        if lmeta.file_type().is_dir() {
            let label = match platform {
                "ios" => "iOS",
                "web" => "Web",
                other => other,
            };
            self.fingerprint_cache
                .fingerprint_tree(workspace_root, build_path, label)
        } else if lmeta.file_type().is_file() {
            self.fingerprint_cache
                .fingerprint_file(workspace_root, build_path)
        } else {
            Err(CoreError::Fingerprint(format!(
                "buildPath must be a regular file or directory: {}",
                build_path.display()
            )))
        }
    }
}

/// Shared execution context passed into tool handlers.
pub struct ToolContext {
    pub config: McpServerConfig,
    pub redaction: RedactionContext,
    pub factory: Arc<dyn McpDriverFactory>,
    pub exec_mutex: Arc<AsyncMutex<WarmSessionState>>,
}

/// Outcome of dispatching a single `tools/call` invocation.
pub enum ToolDispatchResult {
    /// Standard MCP `CallToolResult` JSON object (`{ content, structuredContent, isError }`).
    Response(Value),
    /// Request was cancelled (server replies with JSON-RPC `-32800` unless shutting down).
    Cancelled,
}

/// Returns `true` if `name` is a recognized MCP tool (including the `deja_run_steps` alias
/// and `deja_explore`, even when disabled so `deja_explore` returns `EXPLORE_DISABLED`).
#[must_use]
pub fn is_known_tool(name: &str) -> bool {
    matches!(
        name,
        "deja_list"
            | "deja_inspect"
            | "deja_run"
            | "deja_remember"
            | "deja_exec_batch"
            | "deja_run_steps"
            | "deja_explore"
    )
}

/// Return the MCP `tools/list` schema catalogue.
///
/// Every tool sets `additionalProperties: false` and never exposes confinement overrides
/// (`workspaceRoot`, `appId`, `platform`, `device`, `store`, `buildPath`, `acceptAssertionChange`).
/// `deja_explore` is included ONLY when BOTH `mcp_explore` AND `explore_backend_available` are `true`.
#[must_use]
pub fn tool_definitions(mcp_explore: bool, explore_backend_available: bool) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "deja_list",
            "description": "List valid local Déjà walks from the configured read-only SQLite store without invoking a driver or model.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "name": "deja_inspect",
            "description": "Inspect a stored walk and its validation history by `goal`, or read a saved redacted JSON report confined strictly inside `.deja/reports/` (defaults to latest report when omitted).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "Goal name to inspect in the read-only SQLite store (mutually exclusive with `report`)."
                    },
                    "report": {
                        "type": "string",
                        "description": "Workspace-relative path strictly inside `.deja/reports/` (mutually exclusive with `goal`)."
                    }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "deja_run",
            "description": "Replay a remembered Déjà walk on the current build with zero model calls, warm driver reuse, flaky retry support, and pruned visibleElements diagnostics.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "Goal name of the remembered walk to replay."
                    },
                    "verify": {
                        "type": "boolean",
                        "description": "Re-verify a changed or unvalidated consumer flow in manifest mode."
                    },
                    "rerunFailed": {
                        "type": "boolean",
                        "description": "Rerun a failed replay once on identical bytes to detect flakiness."
                    },
                    "retries": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 5,
                        "description": "Additional retry attempts after an initial failure (0..=5)."
                    }
                },
                "required": ["goal"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "deja_remember",
            "description": "Validate, execute on a warm driver, and atomically persist a reviewed workspace-relative maestro/v1 YAML flow only on a complete pass.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "Goal name to remember on a complete pass."
                    },
                    "flow": {
                        "type": "string",
                        "description": "Workspace-relative path outside `.deja/` pointing to a `.yaml` or `.yml` maestro/v1 flow."
                    },
                    "retries": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 5,
                        "description": "Additional retry attempts after an initial failure (0..=5)."
                    }
                },
                "required": ["goal", "flow"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "deja_exec_batch",
            "description": "Execute a confined batch of 1 to 50 maestro/v1 steps (<= 64 KiB) directly on the warm driver and return per-step timings plus pruned visibleElements (also callable via alias `deja_run_steps`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "steps": {
                        "description": "1 to 50 maestro/v1 steps as a YAML string or array of step objects/strings (serialized <= 64 KiB).",
                        "oneOf": [
                            { "type": "string" },
                            { "type": "array", "minItems": 1, "maxItems": 50 }
                        ]
                    },
                    "timeoutMs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 600000,
                        "description": "Optional batch execution timeout in milliseconds."
                    },
                    "observeFinal": {
                        "type": "boolean",
                        "description": "Whether to return pruned visibleElements (top 30) from the final screen on pass (default: true)."
                    }
                },
                "required": ["steps"],
                "additionalProperties": false
            }
        }),
    ];

    if mcp_explore && explore_backend_available {
        tools.push(json!({
            "name": "deja_explore",
            "description": "Bounded graph-assisted exploration (enabled via `mcp.explore: true`; full engine lands in DEJA-71).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "Optional target goal to explore."
                    },
                    "maxSteps": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 500,
                        "description": "Optional maximum exploration steps."
                    },
                    "maxDepth": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "description": "Optional maximum graph depth."
                    }
                },
                "additionalProperties": false
            }
        }));
    }

    tools
}

/// Dispatch a validated tool call, enforcing cancellation, confinement, and redaction.
pub async fn dispatch_tool_call(
    ctx: Arc<ToolContext>,
    name: &str,
    raw_arguments: Value,
    cancel: CancellationToken,
) -> ToolDispatchResult {
    if cancel.is_cancelled() {
        return ToolDispatchResult::Cancelled;
    }

    match name {
        "deja_list" => handle_deja_list(&ctx, raw_arguments, &cancel),
        "deja_inspect" => handle_deja_inspect(&ctx, raw_arguments, &cancel),
        "deja_run" => handle_deja_run(&ctx, raw_arguments, &cancel).await,
        "deja_remember" => handle_deja_remember(&ctx, raw_arguments, &cancel).await,
        "deja_exec_batch" | "deja_run_steps" => {
            handle_deja_exec_batch(&ctx, raw_arguments, &cancel).await
        }
        "deja_explore" => handle_deja_explore(&ctx, raw_arguments),
        _ => ToolDispatchResult::Response(tool_error(
            &ctx.redaction,
            "INVALID_INPUT",
            &format!("unknown tool `{name}`"),
        )),
    }
}

// ---------------------------------------------------------------------------
// Response & argument validation helpers
// ---------------------------------------------------------------------------

fn tool_response(redaction: &RedactionContext, payload: Value, is_error: bool) -> Value {
    let redacted = redaction.redact_json(&payload);
    let text = serde_json::to_string(&redacted).unwrap_or_else(|_| "{}".to_string());
    json!({
        "content": [
            {
                "type": "text",
                "text": text,
            }
        ],
        "structuredContent": redacted,
        "isError": is_error,
    })
}

fn tool_error(redaction: &RedactionContext, code: &str, message: &str) -> Value {
    let payload = json!({
        "code": code,
        "message": redaction.redact_str(message),
    });
    tool_response(redaction, payload, true)
}

fn parse_arguments_object<'a>(
    raw: &'a Value,
    allowed_keys: &[&str],
) -> Result<&'a Map<String, Value>, String> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    let map = match raw {
        Value::Null => EMPTY.get_or_init(Map::new),
        Value::Object(m) => m,
        _ => return Err("tool `arguments` must be a JSON object".to_string()),
    };

    for key in map.keys() {
        if !allowed_keys.contains(&key.as_str()) {
            return Err(format!(
                "unexpected argument `{key}` (confinement overrides and unknown fields are rejected)"
            ));
        }
    }
    Ok(map)
}

fn require_non_empty_string(map: &Map<String, Value>, key: &str) -> Result<String, String> {
    match map.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err(format!("`{key}` must not be empty")),
        Some(_) => Err(format!("`{key}` must be a string")),
        None => Err(format!("missing required field `{key}`")),
    }
}

fn optional_non_empty_string(
    map: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, String> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.clone())),
        Some(Value::String(_)) => Err(format!("`{key}` must not be empty when provided")),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn optional_bool(map: &Map<String, Value>, key: &str, default: bool) -> Result<bool, String> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("`{key}` must be a boolean")),
    }
}

fn optional_u64_in_range(
    map: &Map<String, Value>,
    key: &str,
    min: u64,
    max: u64,
) -> Result<Option<u64>, String> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            let Some(u) = n.as_u64() else {
                return Err(format!(
                    "`{key}` must be an integer between {min} and {max}"
                ));
            };
            if u < min || u > max {
                return Err(format!("`{key}` must be between {min} and {max}"));
            }
            Ok(Some(u))
        }
        Some(_) => Err(format!(
            "`{key}` must be an integer between {min} and {max}"
        )),
    }
}

fn store_options(config: &McpServerConfig) -> SqliteStoreOptions {
    let mut opts = SqliteStoreOptions::new(&config.canonical_workspace_root);
    if let Some(ref id) = config.store_id {
        opts = opts.with_store_id(id.clone());
    }
    opts
}

fn build_consumer_flows(config: &McpServerConfig) -> Result<Option<ConsumerFlows>, String> {
    let Some(ref manifest_rel) = config.manifest_path else {
        return Ok(None);
    };
    let mut opts = ConsumerFlowsOptions::new(&config.canonical_workspace_root, manifest_rel)
        .with_forbidden_values(config.forbidden_values.clone());
    if let Some(ref flows_dir) = config.flows_dir {
        opts = opts.with_flows_dir(flows_dir);
    }
    ConsumerFlows::new(opts)
        .map(Some)
        .map_err(|err| err.to_string())
}

fn make_walk_identity(
    config: &McpServerConfig,
    goal: &Goal,
    build_fingerprint: String,
) -> WalkIdentity {
    WalkIdentity {
        goal: goal.text.clone(),
        app_id: config.app_id.clone(),
        platform: config.platform.clone(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint,
        adapter: config.adapter.clone(),
        browser: config.browser.clone(),
    }
}

fn check_closure_forbidden_values(
    closure: &FlowClosure,
    forbidden_values: &[String],
) -> Result<(), String> {
    if forbidden_values.is_empty() {
        return Ok(());
    }
    for file in &closure.files {
        let holds = if file.kind == FlowReferenceKind::Media {
            bytes_hold_forbidden_value(&file.bytes, forbidden_values)
        } else {
            let text = String::from_utf8_lossy(&file.bytes);
            holds_forbidden_value(&text, forbidden_values)
        };
        if holds {
            return Err(format!(
                "flow closure file `{}` contains a configured fixture credential or secret value; pass credentials via env instead of committing them",
                file.source
            ));
        }
    }
    Ok(())
}

fn mcp_screenshots_dir(config: &McpServerConfig) -> PathBuf {
    match config.store_id.as_deref() {
        Some(id) => config
            .canonical_workspace_root
            .join(".deja")
            .join("reports")
            .join("stores")
            .join(id)
            .join("mcp-screenshots"),
        None => config
            .canonical_workspace_root
            .join(".deja")
            .join("reports")
            .join("mcp-screenshots"),
    }
}

fn allocate_report_rel_path(command: RunCommand, store_id: Option<&str>) -> String {
    let cmd = match command {
        RunCommand::Run => "run",
        RunCommand::Remember => "remember",
        RunCommand::Explore => "explore",
    };
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = MCP_REPORT_SEQ.fetch_add(1, Ordering::Relaxed);
    match store_id {
        Some(id) => format!(
            ".deja/reports/stores/{id}/{cmd}-{ms}-{}-{seq}.json",
            std::process::id()
        ),
        None => format!(".deja/reports/{cmd}-{ms}-{}-{seq}.json", std::process::id()),
    }
}

fn write_internal_reports(
    canonical_workspace: &Path,
    store_id: Option<&str>,
    immutable_rel_path: &str,
    redacted_json: &Value,
) -> std::io::Result<()> {
    let mut text = serde_json::to_string_pretty(redacted_json)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))?;
    text.push('\n');
    let bytes = text.as_bytes();

    let immutable_abs = canonical_workspace.join(immutable_rel_path);
    write_atomic_confined(canonical_workspace, &immutable_abs, bytes)?;

    let latest_rel = match store_id {
        Some(id) => format!(".deja/reports/stores/{id}/latest.json"),
        None => ".deja/reports/latest.json".to_string(),
    };
    let latest_abs = canonical_workspace.join(latest_rel);
    write_atomic_confined(canonical_workspace, &latest_abs, bytes)?;
    Ok(())
}

const fn failure_category_wire(cat: FailureCategory) -> &'static str {
    match cat {
        FailureCategory::Selector => "selector",
        FailureCategory::Navigation => "navigation",
        FailureCategory::ProductAssertion => "product_assertion",
        FailureCategory::Runner => "runner",
        FailureCategory::Timeout => "timeout",
        FailureCategory::Unknown => "unknown",
        FailureCategory::AppCrash => "app_crash",
    }
}

const fn outcome_wire_name(outcome: RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Passed => "passed",
        RunOutcome::Remembered => "remembered",
        RunOutcome::ColdRecorded => "cold-recorded",
        RunOutcome::Repaired => "repaired",
        RunOutcome::Rejected => "rejected",
        RunOutcome::ExplorationRejected => "exploration-rejected",
        RunOutcome::ProductRegression => "product-regression",
        RunOutcome::RepairRejected => "repair-rejected",
        RunOutcome::Failed => "failed",
        RunOutcome::FlowChanged => "flow-changed",
        RunOutcome::NotValidated => "not-validated",
        RunOutcome::NeedsMigration => "needs-migration",
        RunOutcome::AssertionsChanged => "assertions-changed",
        RunOutcome::Flaky => "flaky",
        RunOutcome::ClosureError => "closure-error",
    }
}

const fn cache_status_wire(status: CacheStatus) -> &'static str {
    match status {
        CacheStatus::Hit => "hit",
        CacheStatus::Miss => "miss",
        CacheStatus::NotChecked => "not-checked",
    }
}

fn failure_run_outcome(failure: Option<&WalkFailure>) -> RunOutcome {
    match failure.map(|f| f.category) {
        Some(FailureCategory::ProductAssertion) => RunOutcome::ProductRegression,
        _ => RunOutcome::Failed,
    }
}

fn failure_signature(failure: Option<&WalkFailure>) -> (Option<FailureCategory>, Option<String>) {
    match failure {
        Some(f) => (Some(f.category), f.failing_step.clone()),
        None => (None, None),
    }
}

fn native_engine_report() -> EngineReport {
    EngineReport {
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        source: EngineSource::Pinned,
        override_via: None,
        in_range: true,
    }
}

fn verify_unchanged_closure(
    workspace_root: &Path,
    flow_abs_path: &Path,
    expected_closure_sha256: &str,
    expected_assertions_sha256: &str,
) -> Result<(), String> {
    let live = collect_flow_closure(workspace_root, flow_abs_path)
        .map_err(|err| format!("flow closure became unreadable during execution: {err}"))?;
    verify_closure_files_nofollow(workspace_root, &live)?;
    if live.sha256 != expected_closure_sha256 {
        return Err(format!(
            "the flow or a file it runs changed during execution (started with {}, now {})",
            expected_closure_sha256, live.sha256
        ));
    }
    let live_assertions = assertions_sha256(&live)
        .map_err(|err| format!("failed to hash flow assertions during recheck: {err}"))?;
    if live_assertions != expected_assertions_sha256 {
        return Err(format!(
            "the flow's assertions changed during execution (started with {}, now {})",
            expected_assertions_sha256, live_assertions
        ));
    }
    Ok(())
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let prefix: String = text.chars().take(max_chars).collect();
    format!("{prefix}…[truncated]")
}

// ---------------------------------------------------------------------------
// Warm driver + app-install manager
// ---------------------------------------------------------------------------

enum WarmAcquireError {
    Cancelled,
    Driver(String),
}

async fn ensure_warm_driver_and_install(
    state: &mut WarmSessionState,
    factory: &dyn McpDriverFactory,
    driver_ctx: &McpDriverContext,
    runtime: &Option<String>,
    current_fingerprint: Option<&str>,
    cancel: &CancellationToken,
) -> Result<u64, WarmAcquireError> {
    if cancel.is_cancelled() {
        state.teardown_driver().await;
        return Err(WarmAcquireError::Cancelled);
    }

    let t0 = Instant::now();

    // If the runtime changed since the warm driver was created, tear down and re-create.
    if state.driver.is_some() && state.installed_runtime != *runtime {
        state.teardown_driver().await;
    }

    // Reuse warm driver when available; if its session dropped, recreate cleanly.
    if let Some(ref driver) = state.driver {
        match driver.ensure_session(cancel).await {
            Ok(()) => {}
            Err(DriverError::Cancelled) => {
                state.teardown_driver().await;
                return Err(WarmAcquireError::Cancelled);
            }
            Err(_) => {
                state.teardown_driver().await;
            }
        }
    }

    if state.driver.is_none() {
        let created = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                state.teardown_driver().await;
                return Err(WarmAcquireError::Cancelled);
            }
            res = factory.create_driver(driver_ctx) => res,
        };
        let driver = match created {
            Ok(d) => d,
            Err(DriverError::Cancelled) => {
                state.teardown_driver().await;
                return Err(WarmAcquireError::Cancelled);
            }
            Err(err) => {
                state.teardown_driver().await;
                return Err(WarmAcquireError::Driver(err.to_string()));
            }
        };
        if let Err(err) = driver.ensure_session(cancel).await {
            let _ = tokio::time::timeout(DRIVER_SHUTDOWN_TIMEOUT, driver.shutdown()).await;
            state.installed_build_fingerprint = None;
            state.installed_runtime = None;
            if matches!(err, DriverError::Cancelled) || cancel.is_cancelled() {
                return Err(WarmAcquireError::Cancelled);
            }
            return Err(WarmAcquireError::Driver(err.to_string()));
        }
        state.driver = Some(driver);
        state.installed_runtime = runtime.clone();
    }

    if let Some(fp) = current_fingerprint {
        let already_installed = state.installed_build_fingerprint.as_deref() == Some(fp)
            && state.installed_runtime == *runtime;
        if !already_installed {
            match factory.install_app(driver_ctx, cancel).await {
                Ok(()) => {
                    state.installed_build_fingerprint = Some(fp.to_string());
                    state.installed_runtime = runtime.clone();
                }
                Err(DriverError::Cancelled) => {
                    state.teardown_driver().await;
                    return Err(WarmAcquireError::Cancelled);
                }
                Err(err) => {
                    state.teardown_driver().await;
                    if cancel.is_cancelled() {
                        return Err(WarmAcquireError::Cancelled);
                    }
                    return Err(WarmAcquireError::Driver(err.to_string()));
                }
            }
        }
    }

    if cancel.is_cancelled() {
        state.teardown_driver().await;
        return Err(WarmAcquireError::Cancelled);
    }

    Ok(t0.elapsed().as_millis() as u64)
}

struct ExecutedAttempt {
    started_at: String,
    ended_at: String,
    duration_ms: u64,
    passed: bool,
    assertions_executed: u64,
    failure: Option<WalkFailure>,
    visible_context: Option<VisibleContext>,
    timing: RunTimingBreakdown,
    errors: Vec<String>,
    artifacts: Vec<RunArtifact>,
}

async fn execute_single_closure_attempt(
    driver: &dyn PlatformDriver,
    config: &McpServerConfig,
    closure: &FlowClosure,
    timeout_ms: u64,
    redaction: &RedactionContext,
    cancel: &CancellationToken,
) -> ExecutedAttempt {
    let started_at = now_iso8601();
    let started_epoch_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let t0 = Instant::now();

    let mut exec_cfg = FlowExecutionConfig::new(config.canonical_workspace_root.clone())
        .with_closure_snapshot(closure);
    exec_cfg.app_id_override = Some(config.app_id.clone());
    exec_cfg.env = config.env.clone();
    exec_cfg.flow_timeout = Some(Duration::from_millis(timeout_ms));
    exec_cfg.redactor = redaction.redactor.clone();
    exec_cfg.provider_redactor = redaction.provider.clone();
    exec_cfg.screenshot_dir = Some(mcp_screenshots_dir(config));
    exec_cfg.screenshot_confine_to_dir = true;
    exec_cfg.screenshot_no_clobber = true;

    let main_source = closure
        .files
        .iter()
        .find(|f| f.path == closure.main)
        .map(|f| f.source.as_str())
        .unwrap_or(closure.main.as_str());
    let main_path = config.canonical_workspace_root.join(main_source);

    let executor = FlowExecutor::new(exec_cfg);
    let mut outcome: FlowExecutionOutcome = executor.execute_file(driver, &main_path, cancel).await;

    if !outcome.passed
        && !outcome.crash_probe_completed
        && !cancel.is_cancelled()
        && driver
            .capabilities()
            .supports(DriverCapability::AppCrashReports)
        && outcome.failure.as_ref().is_some_and(|f| {
            matches!(
                f.category,
                FailureCategory::Selector | FailureCategory::Navigation | FailureCategory::Unknown
            ) && f.crash.is_none()
        })
        && let Ok(Some(crash_sig)) = driver
            .check_app_crash(&config.app_id, started_epoch_ms, cancel)
            .await
    {
        outcome.failure = FailureClassifier::classify(&ClassificationInput {
            passed: false,
            timed_out: false,
            failed_step: outcome.failed_step.as_ref(),
            crash_signal: Some(&crash_sig),
            raw_artifact_path: None,
        });
    }

    let ended_at = now_iso8601();
    let duration_ms = t0.elapsed().as_millis() as u64;

    let visible_context = outcome
        .failed_step
        .as_ref()
        .and_then(|s| s.visible_context.as_ref())
        .map(|vc| redaction.redact_visible_context(vc));

    let mut errors = Vec::new();
    if let Some(ref step) = outcome.failed_step
        && !step.message.is_empty()
    {
        errors.push(redaction.redact_str(&step.message));
    }
    for hook_err in &outcome.hook_errors {
        errors.push(redaction.redact_str(hook_err));
    }

    let mut artifacts = Vec::new();
    for shot in &outcome.screenshots {
        artifacts.push(RunArtifact {
            kind: ArtifactKind::Output,
            path: workspace_relative_display(&config.canonical_workspace_root, shot),
            sha256: None,
        });
    }

    ExecutedAttempt {
        started_at,
        ended_at,
        duration_ms,
        passed: outcome.passed,
        assertions_executed: outcome.assertions_executed,
        failure: outcome.failure,
        visible_context,
        timing: outcome.timing,
        errors,
        artifacts,
    }
}

fn to_execution_result(
    attempt: &ExecutedAttempt,
    closure: Option<&FlowClosure>,
) -> ExecutionResult {
    let assertions = closure
        .and_then(|c| count_assertions_in_closure(c).ok())
        .or(Some(attempt.assertions_executed));
    ExecutionResult {
        passed: attempt.passed,
        started_at: attempt.started_at.clone(),
        ended_at: attempt.ended_at.clone(),
        duration_ms: attempt.duration_ms,
        exit_code: Some(if attempt.passed { 0 } else { 1 }),
        assertions,
        failure: attempt.failure.clone(),
        artifacts: attempt.artifacts.clone(),
    }
}

// ---------------------------------------------------------------------------
// Tool 1: deja_list
// ---------------------------------------------------------------------------

fn handle_deja_list(
    ctx: &ToolContext,
    raw_args: Value,
    cancel: &CancellationToken,
) -> ToolDispatchResult {
    if cancel.is_cancelled() {
        return ToolDispatchResult::Cancelled;
    }
    if let Err(msg) = parse_arguments_object(&raw_args, &[]) {
        return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
    }

    let store = match SqliteWalkStore::open_readonly(store_options(&ctx.config)) {
        Ok(s) => s,
        Err(err) => {
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                err.code(),
                &err.to_string(),
            ));
        }
    };

    let walks = match store.list_walks() {
        Ok(w) => w,
        Err(err) => {
            store.close();
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                err.code(),
                &err.to_string(),
            ));
        }
    };
    let store_label = store.store_label();
    store.close();

    let exported: Vec<StoredWalk> = walks
        .iter()
        .map(|w| ctx.redaction.sanitize_walk_for_export(w))
        .collect();

    let payload = json!({
        "store": store_label,
        "manifest": ctx.config.manifest_path,
        "count": exported.len(),
        "walks": exported,
    });
    ToolDispatchResult::Response(tool_response(&ctx.redaction, payload, false))
}

// ---------------------------------------------------------------------------
// Tool 2: deja_inspect
// ---------------------------------------------------------------------------

fn handle_deja_inspect(
    ctx: &ToolContext,
    raw_args: Value,
    cancel: &CancellationToken,
) -> ToolDispatchResult {
    if cancel.is_cancelled() {
        return ToolDispatchResult::Cancelled;
    }
    let map = match parse_arguments_object(&raw_args, &["goal", "report"]) {
        Ok(m) => m,
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
        }
    };

    let goal_arg = match optional_non_empty_string(map, "goal") {
        Ok(v) => v,
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
        }
    };
    let report_arg = match optional_non_empty_string(map, "report") {
        Ok(v) => v,
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
        }
    };

    if goal_arg.is_some() && report_arg.is_some() {
        return ToolDispatchResult::Response(tool_error(
            &ctx.redaction,
            "INVALID_INPUT",
            "`goal` and `report` are mutually exclusive; specify at most one",
        ));
    }

    if let Some(raw_goal) = goal_arg {
        let norm_goal = match normalize_goal(&raw_goal) {
            Ok(g) => g,
            Err(err) => {
                return ToolDispatchResult::Response(tool_error(
                    &ctx.redaction,
                    "INVALID_INPUT",
                    &err.to_string(),
                ));
            }
        };

        let store = match SqliteWalkStore::open_readonly(store_options(&ctx.config)) {
            Ok(s) => s,
            Err(err) => {
                return ToolDispatchResult::Response(tool_error(
                    &ctx.redaction,
                    err.code(),
                    &err.to_string(),
                ));
            }
        };
        let store_label = store.store_label();

        let walks = match store.list_walks() {
            Ok(w) => w,
            Err(err) => {
                store.close();
                return ToolDispatchResult::Response(tool_error(
                    &ctx.redaction,
                    err.code(),
                    &err.to_string(),
                ));
            }
        };

        let matched = walks
            .iter()
            .find(|w| {
                w.goal_key == norm_goal.key
                    && w.app_id == ctx.config.app_id
                    && w.platform == ctx.config.platform
                    && w.adapter == ctx.config.adapter
                    && w.browser == ctx.config.browser
            })
            .or_else(|| walks.iter().find(|w| w.goal_key == norm_goal.key));

        let Some(walk) = matched else {
            store.close();
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "WALK_NOT_FOUND",
                &format!("no walk found for goal {:?}", raw_goal),
            ));
        };

        let validation_runs = match store.list_validation_runs(&walk.identity()) {
            Ok(runs) => runs,
            Err(err) => {
                store.close();
                return ToolDispatchResult::Response(tool_error(
                    &ctx.redaction,
                    err.code(),
                    &err.to_string(),
                ));
            }
        };
        store.close();

        let exported_walk = ctx.redaction.sanitize_walk_for_export(walk);
        let payload = json!({
            "store": store_label,
            "walk": exported_walk,
            "validationRuns": validation_runs,
        });
        return ToolDispatchResult::Response(tool_response(&ctx.redaction, payload, false));
    }

    let report_rel = report_arg.unwrap_or_else(|| match ctx.config.store_id.as_deref() {
        Some(id) => format!(".deja/reports/stores/{id}/latest.json"),
        None => ".deja/reports/latest.json".to_string(),
    });

    let (_path, raw_text) = match read_confined_report_file(
        &ctx.config.canonical_workspace_root,
        ctx.config.store_id.as_deref(),
        &report_rel,
    ) {
        Ok(r) => r,
        Err((code, msg)) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, code, &msg));
        }
    };

    let mut parsed: Value = match serde_json::from_str(&raw_text) {
        Ok(v) => v,
        Err(err) => {
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "INVALID_REPORT",
                &format!("report `{report_rel}` is not valid JSON: {err}"),
            ));
        }
    };

    if let Some(obj) = parsed.as_object_mut()
        && let Some(raw_goal) = obj
            .get("identity")
            .and_then(|id| id.get("goal"))
            .and_then(Value::as_str)
    {
        let safe_key = ctx.redaction.redact_goal_key(raw_goal);
        obj.insert("goalKey".to_string(), Value::String(safe_key));
    }

    let payload = json!({
        "reportPath": report_rel,
        "report": parsed,
    });
    ToolDispatchResult::Response(tool_response(&ctx.redaction, payload, false))
}

// ---------------------------------------------------------------------------
// Tool 3: deja_run
// ---------------------------------------------------------------------------

enum RunPlan {
    Refuse {
        cache_status: CacheStatus,
        miss_reason: CacheMissReason,
        outcome: RunOutcome,
        detail: String,
        yaml_path: Option<String>,
    },
    Execute(Box<ExecutionPlan>),
}

enum ExecMode {
    ManifestVerify {
        flow_rel: String,
    },
    ManifestHit,
    ManifestReplayInPlace {
        miss_reason: CacheMissReason,
    },
    ClassicHit,
    ClassicStaleBuildReplay {
        stored_flow_path: String,
        stored_flow_sha256: String,
        stored_subflow_files: Option<Vec<deja_core::StoredFlowFile>>,
    },
}

struct ExecutionPlan {
    flow_abs_path: PathBuf,
    display_flow_path: String,
    closure: FlowClosure,
    cache_status: CacheStatus,
    miss_reason: Option<CacheMissReason>,
    validation_origin: Option<WalkOrigin>,
    closure_matches_manifest: bool,
    new_build_replay: Option<NewBuildReplayReport>,
    mode: ExecMode,
}

async fn handle_deja_run(
    ctx: &ToolContext,
    raw_args: Value,
    cancel: &CancellationToken,
) -> ToolDispatchResult {
    let map = match parse_arguments_object(&raw_args, &["goal", "verify", "rerunFailed", "retries"])
    {
        Ok(m) => m,
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
        }
    };

    let raw_goal = match require_non_empty_string(map, "goal") {
        Ok(g) => g,
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
        }
    };
    let goal = match normalize_goal(&raw_goal) {
        Ok(g) => g,
        Err(err) => {
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "INVALID_INPUT",
                &err.to_string(),
            ));
        }
    };
    let verify = match optional_bool(map, "verify", false) {
        Ok(v) => v,
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
        }
    };
    let rerun_failed = match optional_bool(map, "rerunFailed", false) {
        Ok(v) => v,
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
        }
    };
    let retries_opt = match optional_u64_in_range(map, "retries", 0, u64::from(MAX_MCP_RETRIES)) {
        Ok(v) => v.map(|n| n as u8),
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
        }
    };

    if verify && ctx.config.manifest_path.is_none() {
        return ToolDispatchResult::Response(tool_error(
            &ctx.redaction,
            "INVALID_INPUT",
            "`verify` requires `manifest` to be configured in deja.config.json",
        ));
    }

    let timeout_ms = if verify {
        ctx.config.verification_timeout_ms
    } else {
        ctx.config.timeout_ms
    };
    if let Err((code, msg)) = ctx.config.driver_context_requiring_build(timeout_ms, None) {
        return ToolDispatchResult::Response(tool_error(&ctx.redaction, code, &msg));
    }

    let mut session = tokio::select! {
        biased;
        _ = cancel.cancelled() => return ToolDispatchResult::Cancelled,
        guard = ctx.exec_mutex.lock() => guard,
    };
    if cancel.is_cancelled() {
        session.teardown_driver().await;
        return ToolDispatchResult::Cancelled;
    }

    let cmd_started_at = now_iso8601();
    let cmd_t0 = Instant::now();
    let immutable_report_rel =
        allocate_report_rel_path(RunCommand::Run, ctx.config.store_id.as_deref());

    let consumer = match build_consumer_flows(&ctx.config) {
        Ok(c) => c,
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "INVALID_MANIFEST",
                &msg,
            ));
        }
    };

    let store = match SqliteWalkStore::open_writable(store_options(&ctx.config)) {
        Ok(s) => s,
        Err(err) => {
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                err.code(),
                &err.to_string(),
            ));
        }
    };

    let build_path = ctx
        .config
        .build_path
        .as_ref()
        .expect("validated build_path");

    let fp_t0 = Instant::now();
    let build_fp = match session.fingerprint_build(
        &ctx.config.canonical_workspace_root,
        build_path,
        &ctx.config.platform,
    ) {
        Ok(fp) => fp,
        Err(err) => {
            store.close();
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "FINGERPRINT_ERROR",
                &format!("failed to fingerprint build: {err}"),
            ));
        }
    };
    let build_fingerprint_ms = fp_t0.elapsed().as_millis() as u64;
    let identity = make_walk_identity(&ctx.config, &goal, build_fp.clone());

    let driver_ctx = match ctx
        .config
        .driver_context_requiring_build(timeout_ms, Some(build_fp.clone()))
    {
        Ok(c) => c,
        Err((code, msg)) => {
            store.close();
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, code, &msg));
        }
    };

    let runtime = match ctx.factory.resolve_runtime(&driver_ctx, cancel).await {
        Ok(rt) => rt,
        Err(DriverError::Cancelled) => {
            session.teardown_driver().await;
            store.close();
            return ToolDispatchResult::Cancelled;
        }
        Err(err) => {
            store.close();
            if cancel.is_cancelled() {
                session.teardown_driver().await;
                return ToolDispatchResult::Cancelled;
            }
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "DRIVER_ERROR",
                &err.to_string(),
            ));
        }
    };

    if cancel.is_cancelled() {
        session.teardown_driver().await;
        store.close();
        return ToolDispatchResult::Cancelled;
    }

    let plan = match plan_run_execution(
        &ctx.config,
        consumer.as_ref(),
        &store,
        &identity,
        runtime.as_deref(),
        verify,
    ) {
        Ok(p) => p,
        Err(err) => {
            store.close();
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                err.code(),
                &err.to_string(),
            ));
        }
    };

    let exec_plan = match plan {
        RunPlan::Refuse {
            cache_status,
            miss_reason,
            outcome,
            detail,
            yaml_path,
        } => {
            store.close();
            let total_ms = cmd_t0.elapsed().as_millis() as u64;
            let timing = RunTimingBreakdown {
                build_fingerprint_ms,
                total_ms,
                ..RunTimingBreakdown::default()
            };
            let payload = build_run_tool_payload(
                ctx,
                RunCommand::Run,
                &identity,
                &goal,
                cache_status,
                Some(miss_reason),
                outcome,
                false,
                cmd_started_at,
                now_iso8601(),
                runtime,
                yaml_path,
                None,
                None,
                None,
                None,
                None,
                None,
                vec![ctx.redaction.redact_str(&detail)],
                timing,
                &immutable_report_rel,
                None,
            );
            return ToolDispatchResult::Response(tool_response(&ctx.redaction, payload, true));
        }
        RunPlan::Execute(ep) => *ep,
    };

    let closure = exec_plan.closure;
    if let Err(msg) = verify_closure_files_nofollow(&ctx.config.canonical_workspace_root, &closure)
    {
        store.close();
        return ToolDispatchResult::Response(tool_error(&ctx.redaction, "PATH_CONFINEMENT", &msg));
    }
    if let Err(msg) =
        validate_closure_app_id_confinement(&closure, &ctx.config.app_id, &ctx.config.env)
    {
        store.close();
        return ToolDispatchResult::Response(tool_error(
            &ctx.redaction,
            "APP_ID_CONFINEMENT",
            &msg,
        ));
    }
    if let Err((code, msg)) = validate_closure_mcp_policy(
        &closure,
        &mcp_screenshots_dir(&ctx.config),
        &ctx.config.canonical_workspace_root,
        &ctx.config.env,
    ) {
        store.close();
        return ToolDispatchResult::Response(tool_error(&ctx.redaction, code, &msg));
    }
    if consumer.is_some()
        && let Err(msg) = check_closure_forbidden_values(&closure, &ctx.config.forbidden_values)
    {
        store.close();
        return ToolDispatchResult::Response(tool_error(&ctx.redaction, "FORBIDDEN_SECRET", &msg));
    }

    let main_sha256 = closure
        .files
        .iter()
        .find(|f| f.path == closure.main)
        .map(|f| f.sha256.clone())
        .unwrap_or_default();
    let expected_closure_sha256 = closure.sha256.clone();
    let expected_assertions_sha256 = match assertions_sha256(&closure) {
        Ok(h) => h,
        Err(err) => {
            store.close();
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "CLOSURE_ERROR",
                &err.to_string(),
            ));
        }
    };
    let assertion_count = count_assertions_in_closure(&closure).ok();

    let driver_attach_ms = match ensure_warm_driver_and_install(
        &mut session,
        ctx.factory.as_ref(),
        &driver_ctx,
        &runtime,
        Some(build_fp.as_str()),
        cancel,
    )
    .await
    {
        Ok(ms) => ms,
        Err(WarmAcquireError::Cancelled) => {
            store.close();
            return ToolDispatchResult::Cancelled;
        }
        Err(WarmAcquireError::Driver(msg)) => {
            store.close();
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "DRIVER_ATTACH_FAILED",
                &msg,
            ));
        }
    };

    let max_extra_retries = retries_opt
        .map(usize::from)
        .or(if rerun_failed { Some(1) } else { None })
        .unwrap_or_else(|| usize::from(ctx.config.retries).min(usize::from(MAX_MCP_RETRIES)));

    let exec_t0 = Instant::now();
    let mut attempts_raw: Vec<ExecutedAttempt> = Vec::new();
    let mut mid_retry_drift: Option<String> = None;

    let first_attempt = {
        let driver = session.driver.as_deref().expect("warm driver present");
        execute_single_closure_attempt(
            driver,
            &ctx.config,
            &closure,
            timeout_ms,
            &ctx.redaction,
            cancel,
        )
        .await
    };
    if cancel.is_cancelled() {
        session.teardown_driver().await;
        store.close();
        return ToolDispatchResult::Cancelled;
    }
    let first_passed = first_attempt.passed;
    attempts_raw.push(first_attempt);

    if !first_passed && max_extra_retries > 0 && !cancel.is_cancelled() {
        let first_sig = failure_signature(attempts_raw[0].failure.as_ref());
        for _ in 0..max_extra_retries {
            if cancel.is_cancelled() {
                break;
            }
            if let Err(drift_msg) = verify_unchanged_closure(
                &ctx.config.canonical_workspace_root,
                &exec_plan.flow_abs_path,
                &expected_closure_sha256,
                &expected_assertions_sha256,
            ) {
                mid_retry_drift = Some(drift_msg);
                break;
            }
            let retry_attempt = {
                let driver = session.driver.as_deref().expect("warm driver present");
                execute_single_closure_attempt(
                    driver,
                    &ctx.config,
                    &closure,
                    timeout_ms,
                    &ctx.redaction,
                    cancel,
                )
                .await
            };
            if cancel.is_cancelled() {
                break;
            }
            let retry_passed = retry_attempt.passed;
            let retry_sig = failure_signature(retry_attempt.failure.as_ref());
            attempts_raw.push(retry_attempt);
            if retry_passed || retry_sig != first_sig {
                break;
            }
        }
    }

    if cancel.is_cancelled() {
        session.teardown_driver().await;
        store.close();
        return ToolDispatchResult::Cancelled;
    }

    let flow_exec_ms = exec_t0.elapsed().as_millis() as u64;

    if let Some(drift_msg) = mid_retry_drift {
        store.close();
        let total_ms = cmd_t0.elapsed().as_millis() as u64;
        let timing = RunTimingBreakdown {
            build_fingerprint_ms,
            driver_attach_ms,
            flow_exec_ms,
            total_ms,
            steps: attempts_raw[0].timing.steps.clone(),
            ..RunTimingBreakdown::default()
        };
        let payload = build_run_tool_payload(
            ctx,
            RunCommand::Run,
            &identity,
            &goal,
            exec_plan.cache_status,
            Some(CacheMissReason::FlowChanged),
            RunOutcome::FlowChanged,
            false,
            cmd_started_at,
            now_iso8601(),
            runtime,
            Some(exec_plan.display_flow_path),
            Some(main_sha256),
            assertion_count,
            None,
            None,
            None,
            None,
            vec![ctx.redaction.redact_str(&drift_msg)],
            timing,
            &immutable_report_rel,
            None,
        );
        return ToolDispatchResult::Response(tool_response(&ctx.redaction, payload, true));
    }

    let first = &attempts_raw[0];
    let (final_outcome, final_passed, is_flaky) = if first.passed {
        (RunOutcome::Passed, true, false)
    } else if attempts_raw.len() >= 2 {
        let first_sig = failure_signature(first.failure.as_ref());
        let differed = attempts_raw
            .iter()
            .skip(1)
            .any(|a| a.passed || failure_signature(a.failure.as_ref()) != first_sig);
        if differed {
            (RunOutcome::Flaky, false, true)
        } else {
            (failure_run_outcome(first.failure.as_ref()), false, false)
        }
    } else {
        (failure_run_outcome(first.failure.as_ref()), false, false)
    };

    let mut timing = RunTimingBreakdown {
        build_fingerprint_ms,
        driver_attach_ms,
        warmup_ms: first.timing.warmup_ms,
        flow_exec_ms,
        store_ms: 0,
        total_ms: cmd_t0.elapsed().as_millis() as u64,
        steps: first.timing.steps.clone(),
    };
    let phase_timings_json = serde_json::to_string(&timing).ok();

    let attempts_report = if attempts_raw.len() >= 2 {
        Some(
            attempts_raw
                .iter()
                .enumerate()
                .map(|(i, a)| RunAttempt {
                    attempt: (i + 1) as u8,
                    passed: a.passed,
                    started_at: a.started_at.clone(),
                    ended_at: a.ended_at.clone(),
                    duration_ms: a.duration_ms,
                    failure: a.failure.clone(),
                    artifacts: a.artifacts.clone(),
                })
                .collect::<Vec<_>>(),
        )
    } else {
        None
    };

    let visible_context = if final_passed {
        None
    } else {
        first.visible_context.clone()
    };

    // Write the immutable report BEFORE recording `report_path` in SQLite so we NEVER
    // record or advertise a report path that failed to persist on disk.
    let pre_ended_at = now_iso8601();
    let (written_report_path, report_write_warning) = write_run_report(
        ctx,
        RunCommand::Run,
        &identity,
        exec_plan.cache_status,
        exec_plan.miss_reason,
        final_outcome,
        final_passed,
        cmd_started_at.clone(),
        pre_ended_at,
        runtime.clone(),
        Some(exec_plan.display_flow_path.clone()),
        Some(main_sha256.clone()),
        assertion_count,
        if final_passed {
            None
        } else {
            first.failure.clone()
        },
        visible_context.clone(),
        Some(to_execution_result(first, Some(&closure))),
        attempts_report.clone(),
        first.errors.clone(),
        &timing,
        &immutable_report_rel,
        exec_plan.new_build_replay.clone(),
    );

    let store_t0 = Instant::now();
    let validation_at = first.ended_at.clone();
    let mut validation_input = RecordValidationInput::new(final_passed, validation_at)
        .with_flaky(is_flaky)
        .with_duration_ms(cmd_t0.elapsed().as_millis() as u64);
    if let Some(ref rep_path) = written_report_path {
        validation_input = validation_input.with_report_path(rep_path.clone());
    }
    if let Some(ref rt) = runtime {
        validation_input = validation_input.with_runtime(rt.clone());
    }
    if let Some(origin) = exec_plan.validation_origin {
        validation_input = validation_input.with_origin(origin);
    }
    if !final_passed && let Some(ref fail) = first.failure {
        validation_input =
            validation_input.with_failure_category(failure_category_wire(fail.category));
    }
    if let Some(ref pt_json) = phase_timings_json {
        validation_input = validation_input.with_phase_timings_json(pt_json.clone());
    }

    let mut recorded_for_new_build = false;
    let persist_res: Result<(), StoreError> = (|| match &exec_plan.mode {
        ExecMode::ManifestVerify { flow_rel } => {
            if final_passed {
                let cf = consumer.as_ref().expect("manifest mode");
                let _ = cf.save_verified_manifest_walk(
                    &store,
                    &SaveManifestWalkOptions {
                        identity: identity.clone(),
                        flow_path: flow_rel.clone(),
                        expected_closure_sha256: expected_closure_sha256.clone(),
                        expected_assertions_sha256: Some(expected_assertions_sha256.clone()),
                        validation: validation_input,
                        accept_assertion_change: false,
                    },
                )?;
            } else if exec_plan.closure_matches_manifest {
                let cf = consumer.as_ref().expect("manifest mode");
                let _ = cf.record_post_run_validation(
                    &store,
                    &PostRunValidationOptions {
                        identity: identity.clone(),
                        expected_closure_sha256: expected_closure_sha256.clone(),
                        expected_assertions_sha256: expected_assertions_sha256.clone(),
                        validation: validation_input,
                    },
                )?;
            }
            Ok(())
        }
        ExecMode::ManifestHit => {
            let cf = consumer.as_ref().expect("manifest mode");
            let _ = cf.record_post_run_validation(
                &store,
                &PostRunValidationOptions {
                    identity: identity.clone(),
                    expected_closure_sha256: expected_closure_sha256.clone(),
                    expected_assertions_sha256: expected_assertions_sha256.clone(),
                    validation: validation_input,
                },
            )?;
            Ok(())
        }
        ExecMode::ManifestReplayInPlace { miss_reason } => {
            let cf = consumer.as_ref().expect("manifest mode");
            if final_passed || *miss_reason == CacheMissReason::FailedValidation || is_flaky {
                let _ = cf.record_post_run_validation(
                    &store,
                    &PostRunValidationOptions {
                        identity: identity.clone(),
                        expected_closure_sha256: expected_closure_sha256.clone(),
                        expected_assertions_sha256: expected_assertions_sha256.clone(),
                        validation: validation_input,
                    },
                )?;
                recorded_for_new_build = final_passed;
            }
            Ok(())
        }
        ExecMode::ClassicHit => {
            let _ = store.record_validation(&identity, &validation_input)?;
            Ok(())
        }
        ExecMode::ClassicStaleBuildReplay {
            stored_flow_path,
            stored_flow_sha256,
            stored_subflow_files,
        } => {
            if final_passed {
                verify_unchanged_closure(
                    &ctx.config.canonical_workspace_root,
                    &exec_plan.flow_abs_path,
                    &expected_closure_sha256,
                    &expected_assertions_sha256,
                )
                .map_err(StoreError::ManifestChanged)?;
                let _ = store.upsert_walk(&UpsertWalkInput {
                    identity: identity.clone(),
                    flow_path: stored_flow_path.clone(),
                    flow_sha256: stored_flow_sha256.clone(),
                    subflow_files: stored_subflow_files.clone(),
                    legacy_closure: false,
                    closure_sha256: None,
                    validation: validation_input,
                    exploration: None,
                    repair_history: None,
                })?;
                recorded_for_new_build = true;
            }
            Ok(())
        }
    })();
    let store_ms = store_t0.elapsed().as_millis() as u64;
    store.close();

    timing.store_ms = store_ms;
    timing.total_ms = cmd_t0.elapsed().as_millis() as u64;

    if let Err(err) = persist_res {
        let (outcome, miss) = match &err {
            StoreError::ManifestChanged(_) | StoreError::Core(CoreError::FlowChanged { .. }) => {
                (RunOutcome::FlowChanged, Some(CacheMissReason::FlowChanged))
            }
            StoreError::Core(CoreError::AssertionHashChanged { .. }) => (
                RunOutcome::AssertionsChanged,
                Some(CacheMissReason::FlowChanged),
            ),
            _ => {
                return ToolDispatchResult::Response(tool_error(
                    &ctx.redaction,
                    err.code(),
                    &err.to_string(),
                ));
            }
        };
        let payload = build_run_tool_payload(
            ctx,
            RunCommand::Run,
            &identity,
            &goal,
            exec_plan.cache_status,
            miss,
            outcome,
            false,
            cmd_started_at,
            now_iso8601(),
            runtime,
            Some(exec_plan.display_flow_path),
            Some(main_sha256),
            assertion_count,
            None,
            None,
            None,
            None,
            vec![ctx.redaction.redact_str(&err.to_string())],
            timing,
            &immutable_report_rel,
            None,
        );
        return ToolDispatchResult::Response(tool_response(&ctx.redaction, payload, true));
    }

    let new_build_replay = exec_plan.new_build_replay.map(|mut nb| {
        nb.recorded_for_new_build = recorded_for_new_build;
        nb
    });

    // Refresh the already-created report with final `store_ms` and `recorded_for_new_build`.
    let final_report_path = if written_report_path.is_some() {
        let (refreshed, _) = write_run_report(
            ctx,
            RunCommand::Run,
            &identity,
            exec_plan.cache_status,
            exec_plan.miss_reason,
            final_outcome,
            final_passed,
            cmd_started_at,
            now_iso8601(),
            runtime.clone(),
            Some(exec_plan.display_flow_path.clone()),
            Some(main_sha256.clone()),
            assertion_count,
            if final_passed {
                None
            } else {
                first.failure.clone()
            },
            visible_context.clone(),
            Some(to_execution_result(first, Some(&closure))),
            attempts_report.clone(),
            first.errors.clone(),
            &timing,
            &immutable_report_rel,
            new_build_replay.clone(),
        );
        refreshed.or(written_report_path)
    } else {
        None
    };

    let payload = assemble_run_tool_payload(
        ctx,
        &identity,
        &goal,
        exec_plan.cache_status,
        exec_plan.miss_reason,
        final_outcome,
        final_passed,
        runtime,
        Some(exec_plan.display_flow_path),
        Some(main_sha256),
        assertion_count,
        if final_passed {
            None
        } else {
            first.failure.clone()
        },
        visible_context,
        attempts_report,
        first.errors.clone(),
        report_write_warning.into_iter().collect(),
        timing,
        final_report_path.as_deref(),
        new_build_replay,
    );

    ToolDispatchResult::Response(tool_response(&ctx.redaction, payload, !final_passed))
}

fn plan_run_execution(
    config: &McpServerConfig,
    consumer: Option<&ConsumerFlows>,
    store: &SqliteWalkStore,
    identity: &WalkIdentity,
    runtime: Option<&str>,
    verify: bool,
) -> Result<RunPlan, StoreError> {
    if let Some(cf) = consumer {
        if verify {
            let entry = match cf.resolve_manifest_entry(store, identity)? {
                Ok(e) => e,
                Err(reason) => {
                    let outcome = match reason {
                        CacheMissReason::NeedsMigration => RunOutcome::NeedsMigration,
                        _ => RunOutcome::Failed,
                    };
                    return Ok(RunPlan::Refuse {
                        cache_status: CacheStatus::Miss,
                        miss_reason: reason,
                        outcome,
                        detail: format!(
                            "manifest has no matching entry for {:?} ({}); use `deja_remember` to add a new entry",
                            identity.goal,
                            reason.as_str()
                        ),
                        yaml_path: None,
                    });
                }
            };
            let (canonical_flow, closure, current_assertions) = match cf.inspect_entry(&entry)? {
                InspectedConsumerEntry::Missing { file } => {
                    return Ok(RunPlan::Refuse {
                        cache_status: CacheStatus::Miss,
                        miss_reason: CacheMissReason::MissingFlow,
                        outcome: RunOutcome::FlowChanged,
                        detail: format!("consumer flow file is missing: {file}"),
                        yaml_path: Some(entry.flow),
                    });
                }
                InspectedConsumerEntry::Ok {
                    canonical_flow,
                    closure,
                    assertions_sha256,
                    ..
                } => (canonical_flow, closure, assertions_sha256),
            };
            let assertions = closure_assertions(&closure)?;
            check_owner_rules(entry.owner_rules.as_ref(), &assertions, Some(&closure))?;
            if current_assertions != entry.assertions_sha256 {
                return Ok(RunPlan::Refuse {
                    cache_status: CacheStatus::Miss,
                    miss_reason: CacheMissReason::FlowChanged,
                    outcome: RunOutcome::AssertionsChanged,
                    detail: format!(
                        "the flow's assertions changed (manifest {}, now {}); run `deja remember --flow {} --accept-assertion-change` after review",
                        entry.assertions_sha256, current_assertions, entry.flow
                    ),
                    yaml_path: Some(entry.flow),
                });
            }
            let matches_manifest = closure.sha256 == entry.closure_sha256;
            return Ok(RunPlan::Execute(Box::new(ExecutionPlan {
                flow_abs_path: canonical_flow,
                display_flow_path: entry.flow.clone(),
                closure,
                cache_status: CacheStatus::Miss,
                miss_reason: Some(if matches_manifest {
                    CacheMissReason::NotValidated
                } else {
                    CacheMissReason::FlowChanged
                }),
                validation_origin: Some(WalkOrigin::Verify),
                closure_matches_manifest: matches_manifest,
                new_build_replay: None,
                mode: ExecMode::ManifestVerify {
                    flow_rel: entry.flow,
                },
            })));
        }

        let decision = cf.plan_run(
            store,
            &ConsumerRunRequest {
                identity,
                runtime,
                stale_build: config.stale_build,
            },
        )?;
        match decision {
            ConsumerRunPlan::Hit {
                entry,
                flow_path,
                closure,
                ..
            } => Ok(RunPlan::Execute(Box::new(ExecutionPlan {
                flow_abs_path: flow_path,
                display_flow_path: entry.flow,
                closure,
                cache_status: CacheStatus::Hit,
                miss_reason: None,
                validation_origin: None,
                closure_matches_manifest: true,
                new_build_replay: None,
                mode: ExecMode::ManifestHit,
            }))),
            ConsumerRunPlan::ReplayInPlace {
                entry,
                flow_path,
                closure,
                reason,
                previous_successful,
                ..
            } => {
                let nb = previous_successful.map(|prev| NewBuildReplayReport {
                    policy: StaleBuildPolicy::ReplayLastVerified,
                    previous_build_fingerprint: prev.build_fingerprint,
                    previous_flow_sha256: prev.flow_sha256,
                    previous_validated_at: prev.validated_at,
                    recorded_for_new_build: false,
                });
                Ok(RunPlan::Execute(Box::new(ExecutionPlan {
                    flow_abs_path: flow_path,
                    display_flow_path: entry.flow,
                    closure,
                    cache_status: CacheStatus::Miss,
                    miss_reason: Some(reason),
                    validation_origin: Some(WalkOrigin::ReplayedOnNewBuild),
                    closure_matches_manifest: true,
                    new_build_replay: nb,
                    mode: ExecMode::ManifestReplayInPlace {
                        miss_reason: reason,
                    },
                })))
            }
            ConsumerRunPlan::Refuse {
                outcome,
                miss_reason,
                detail,
                ..
            } => Ok(RunPlan::Refuse {
                cache_status: CacheStatus::Miss,
                miss_reason,
                outcome,
                detail,
                yaml_path: None,
            }),
        }
    } else {
        match store.lookup_for_runtime(identity, runtime)? {
            LookupResult::Hit(hit) => {
                let flow_abs = PathBuf::from(&hit.flow_path);
                let display_flow = workspace_relative_display(
                    &config.canonical_workspace_root,
                    Path::new(&hit.flow_path),
                );
                let closure = match collect_and_verify_classic_closure(
                    &config.canonical_workspace_root,
                    &flow_abs,
                    &hit.walk,
                ) {
                    Ok(c) => c,
                    Err(refuse) => return Ok(refuse),
                };
                Ok(RunPlan::Execute(Box::new(ExecutionPlan {
                    flow_abs_path: flow_abs,
                    display_flow_path: display_flow,
                    closure,
                    cache_status: CacheStatus::Hit,
                    miss_reason: None,
                    validation_origin: None,
                    closure_matches_manifest: false,
                    new_build_replay: None,
                    mode: ExecMode::ClassicHit,
                })))
            }
            LookupResult::Miss(miss) => {
                if miss.reason == CacheMissReason::StaleBuild
                    && config.stale_build == Some(StaleBuildPolicy::ReplayLastVerified)
                {
                    let prev_lookup =
                        match store.lookup_previous_successful_for_runtime(identity, runtime)? {
                            LookupResult::Hit(h) => LookupResult::Hit(h),
                            LookupResult::Miss(_) => {
                                store.lookup_previous_successful_for_runtime(identity, None)?
                            }
                        };
                    if let LookupResult::Hit(prev_hit) = prev_lookup {
                        let flow_abs = PathBuf::from(&prev_hit.flow_path);
                        let closure = match collect_and_verify_classic_closure(
                            &config.canonical_workspace_root,
                            &flow_abs,
                            &prev_hit.walk,
                        ) {
                            Ok(c) => c,
                            Err(refuse) => return Ok(refuse),
                        };
                        let nb = NewBuildReplayReport {
                            policy: StaleBuildPolicy::ReplayLastVerified,
                            previous_build_fingerprint: prev_hit.walk.build_fingerprint.clone(),
                            previous_flow_sha256: prev_hit.walk.flow_sha256.clone(),
                            previous_validated_at: prev_hit.walk.last_validation.at.clone(),
                            recorded_for_new_build: false,
                        };
                        return Ok(RunPlan::Execute(Box::new(ExecutionPlan {
                            flow_abs_path: flow_abs,
                            display_flow_path: prev_hit.walk.flow_path.clone(),
                            closure,
                            cache_status: CacheStatus::Miss,
                            miss_reason: Some(CacheMissReason::StaleBuild),
                            validation_origin: Some(WalkOrigin::ReplayedOnNewBuild),
                            closure_matches_manifest: false,
                            new_build_replay: Some(nb),
                            mode: ExecMode::ClassicStaleBuildReplay {
                                stored_flow_path: prev_hit.walk.flow_path,
                                stored_flow_sha256: prev_hit.walk.flow_sha256,
                                stored_subflow_files: prev_hit.walk.subflow_files,
                            },
                        })));
                    }
                    return Ok(RunPlan::Refuse {
                        cache_status: CacheStatus::Miss,
                        miss_reason: CacheMissReason::StaleBuild,
                        outcome: RunOutcome::RepairRejected,
                        detail:
                            "build changed and no previously verified walk is available to replay"
                                .to_string(),
                        yaml_path: None,
                    });
                }

                let (outcome, detail) = match miss.reason {
                    CacheMissReason::NotFound => (
                        RunOutcome::ExplorationRejected,
                        format!(
                            "no remembered walk found for {:?}; record one first with `deja_remember`",
                            identity.goal
                        ),
                    ),
                    CacheMissReason::StaleBuild => (
                        RunOutcome::RepairRejected,
                        "build fingerprint changed; re-remember the walk or enable iOS `staleBuild: \"replay-last-verified\"`"
                            .to_string(),
                    ),
                    CacheMissReason::FlowChanged => (
                        RunOutcome::FlowChanged,
                        "the stored flow or one of its subflows changed on disk".to_string(),
                    ),
                    other => (
                        RunOutcome::Failed,
                        format!("walk cache lookup missed ({})", other.as_str()),
                    ),
                };
                Ok(RunPlan::Refuse {
                    cache_status: CacheStatus::Miss,
                    miss_reason: miss.reason,
                    outcome,
                    detail,
                    yaml_path: miss.flow_path,
                })
            }
        }
    }
}

fn collect_and_verify_classic_closure(
    workspace_root: &Path,
    flow_abs: &Path,
    walk: &StoredWalk,
) -> Result<FlowClosure, RunPlan> {
    let closure =
        collect_flow_closure(workspace_root, flow_abs).map_err(|err| RunPlan::Refuse {
            cache_status: CacheStatus::Miss,
            miss_reason: CacheMissReason::FlowChanged,
            outcome: RunOutcome::ClosureError,
            detail: err.to_string(),
            yaml_path: Some(walk.flow_path.clone()),
        })?;
    let main_sha = closure
        .files
        .iter()
        .find(|f| f.path == closure.main)
        .map(|f| f.sha256.as_str());
    if main_sha != Some(walk.flow_sha256.as_str()) {
        return Err(RunPlan::Refuse {
            cache_status: CacheStatus::Miss,
            miss_reason: CacheMissReason::FlowChanged,
            outcome: RunOutcome::FlowChanged,
            detail: "the stored flow changed on disk before execution".to_string(),
            yaml_path: Some(walk.flow_path.clone()),
        });
    }
    if let Some(ref expected_closure) = walk.closure_sha256
        && closure.sha256 != *expected_closure
    {
        return Err(RunPlan::Refuse {
            cache_status: CacheStatus::Miss,
            miss_reason: CacheMissReason::FlowChanged,
            outcome: RunOutcome::FlowChanged,
            detail: "the stored flow closure changed on disk before execution".to_string(),
            yaml_path: Some(walk.flow_path.clone()),
        });
    }
    if let Some(ref stored_subflows) = walk.subflow_files {
        if closure.files.len() != stored_subflows.len() + 1 {
            return Err(RunPlan::Refuse {
                cache_status: CacheStatus::Miss,
                miss_reason: CacheMissReason::FlowChanged,
                outcome: RunOutcome::FlowChanged,
                detail: "the stored flow's referenced files changed on disk before execution"
                    .to_string(),
                yaml_path: Some(walk.flow_path.clone()),
            });
        }
        for sf in stored_subflows {
            let matched = closure
                .files
                .iter()
                .any(|f| (f.source == sf.path || f.path == sf.path) && f.sha256 == sf.sha256);
            if !matched {
                return Err(RunPlan::Refuse {
                    cache_status: CacheStatus::Miss,
                    miss_reason: CacheMissReason::FlowChanged,
                    outcome: RunOutcome::FlowChanged,
                    detail: format!(
                        "the stored subflow `{}` changed on disk before execution",
                        sf.path
                    ),
                    yaml_path: Some(walk.flow_path.clone()),
                });
            }
        }
    }
    Ok(closure)
}

#[allow(clippy::too_many_arguments)]
fn write_run_report(
    ctx: &ToolContext,
    command: RunCommand,
    identity: &WalkIdentity,
    cache_status: CacheStatus,
    cache_miss_reason: Option<CacheMissReason>,
    outcome: RunOutcome,
    passed: bool,
    started_at: String,
    ended_at: String,
    runtime: Option<String>,
    yaml_path: Option<String>,
    yaml_sha256: Option<String>,
    assertions: Option<u64>,
    failure: Option<WalkFailure>,
    visible_context: Option<VisibleContext>,
    execution: Option<ExecutionResult>,
    attempts: Option<Vec<RunAttempt>>,
    errors: Vec<String>,
    timing: &RunTimingBreakdown,
    immutable_report_rel: &str,
    replayed_on_new_build: Option<NewBuildReplayReport>,
) -> (Option<String>, Option<String>) {
    let safe_goal_key = ctx.redaction.redact_goal_key(&identity.goal);

    let core_report = RunResult {
        schema_version: WALK_RESULT_SCHEMA_VERSION,
        command,
        identity: identity.clone(),
        goal_key: safe_goal_key,
        cache_status,
        cache_miss_reason,
        semantic: None,
        outcome,
        passed,
        started_at,
        ended_at,
        duration_ms: timing.total_ms,
        model_calls: Some(0),
        model_call_basis: "native-rust-direct".to_string(),
        assertions,
        repair_count: 0,
        yaml_path: yaml_path.clone(),
        yaml_sha256: yaml_sha256.clone(),
        failure,
        artifacts: yaml_path
            .as_ref()
            .map(|p| {
                vec![RunArtifact {
                    kind: ArtifactKind::Yaml,
                    path: p.clone(),
                    sha256: yaml_sha256.clone(),
                }]
            })
            .unwrap_or_default(),
        diffs: Vec::new(),
        errors,
        execution,
        exploration: None,
        repair: None,
        replayed_on_new_build,
        engine: Some(native_engine_report()),
        attempts,
    };

    let mut report_val = match serde_json::to_value(&core_report) {
        Ok(val) => val,
        Err(err) => {
            return (
                None,
                Some(
                    ctx.redaction
                        .redact_str(&format!("failed to serialize run report artifact: {err}")),
                ),
            );
        }
    };
    if let Some(obj) = report_val.as_object_mut() {
        if let Some(ref rt) = runtime {
            obj.insert("runtime".to_string(), Value::String(rt.clone()));
        }
        if let Ok(timing_val) = serde_json::to_value(timing) {
            obj.insert("timing".to_string(), timing_val);
        }
        if let Some(ref vc) = visible_context
            && let Ok(vc_val) = serde_json::to_value(vc)
        {
            obj.insert("visibleContext".to_string(), vc_val);
        }
    }
    let redacted_report = ctx.redaction.redact_json(&report_val);
    match write_internal_reports(
        &ctx.config.canonical_workspace_root,
        ctx.config.store_id.as_deref(),
        immutable_report_rel,
        &redacted_report,
    ) {
        Ok(()) => (Some(immutable_report_rel.to_string()), None),
        Err(err) => (
            None,
            Some(
                ctx.redaction
                    .redact_str(&format!("failed to write run report artifact: {err}")),
            ),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn assemble_run_tool_payload(
    ctx: &ToolContext,
    identity: &WalkIdentity,
    goal: &Goal,
    cache_status: CacheStatus,
    cache_miss_reason: Option<CacheMissReason>,
    outcome: RunOutcome,
    passed: bool,
    runtime: Option<String>,
    yaml_path: Option<String>,
    yaml_sha256: Option<String>,
    assertions: Option<u64>,
    failure: Option<WalkFailure>,
    visible_context: Option<VisibleContext>,
    attempts: Option<Vec<RunAttempt>>,
    errors: Vec<String>,
    warnings: Vec<String>,
    timing: RunTimingBreakdown,
    written_report_path: Option<&str>,
    replayed_on_new_build: Option<NewBuildReplayReport>,
) -> Value {
    let safe_goal_key = ctx.redaction.redact_goal_key(&identity.goal);
    let (visible_elements, visible_elements_truncated) = match visible_context {
        Some(ref vc) => (Some(vc.items().to_vec()), vc.is_truncated()),
        None => (None, false),
    };

    let mut out = Map::new();
    out.insert("passed".to_string(), Value::Bool(passed));
    out.insert(
        "outcome".to_string(),
        Value::String(outcome_wire_name(outcome).to_string()),
    );
    out.insert("goal".to_string(), Value::String(goal.text.clone()));
    out.insert("goalKey".to_string(), Value::String(safe_goal_key));
    out.insert(
        "buildFingerprint".to_string(),
        Value::String(identity.build_fingerprint.clone()),
    );
    out.insert(
        "cacheStatus".to_string(),
        Value::String(cache_status_wire(cache_status).to_string()),
    );
    if let Some(reason) = cache_miss_reason {
        out.insert(
            "cacheMissReason".to_string(),
            Value::String(reason.as_str().to_string()),
        );
    }
    if let Some(rt) = runtime {
        out.insert("runtime".to_string(), Value::String(rt));
    }
    if let Some(yp) = yaml_path {
        out.insert("yamlPath".to_string(), Value::String(yp));
    }
    if let Some(ysha) = yaml_sha256 {
        out.insert("yamlSha256".to_string(), Value::String(ysha));
    }
    if let Some(count) = assertions {
        out.insert("assertions".to_string(), json!(count));
    }
    if let Some(fail) = failure {
        out.insert("failure".to_string(), json!(fail));
    }
    if let Some(elems) = visible_elements {
        out.insert("visibleElements".to_string(), json!(elems));
        out.insert(
            "visibleElementsTruncated".to_string(),
            Value::Bool(visible_elements_truncated),
        );
    }
    if let Some(att) = attempts {
        out.insert("attempts".to_string(), json!(att));
    }
    if let Some(nb) = replayed_on_new_build {
        out.insert("replayedOnNewBuild".to_string(), json!(nb));
    }
    if !errors.is_empty() {
        out.insert("errors".to_string(), json!(errors));
    }
    if !warnings.is_empty() {
        out.insert("warnings".to_string(), json!(warnings));
    }
    out.insert("timing".to_string(), json!(timing));
    if let Some(path) = written_report_path {
        out.insert("reportPath".to_string(), Value::String(path.to_string()));
    }

    Value::Object(out)
}

#[allow(clippy::too_many_arguments)]
fn build_run_tool_payload(
    ctx: &ToolContext,
    command: RunCommand,
    identity: &WalkIdentity,
    goal: &Goal,
    cache_status: CacheStatus,
    cache_miss_reason: Option<CacheMissReason>,
    outcome: RunOutcome,
    passed: bool,
    started_at: String,
    ended_at: String,
    runtime: Option<String>,
    yaml_path: Option<String>,
    yaml_sha256: Option<String>,
    assertions: Option<u64>,
    failure: Option<WalkFailure>,
    visible_context: Option<VisibleContext>,
    execution: Option<ExecutionResult>,
    attempts: Option<Vec<RunAttempt>>,
    errors: Vec<String>,
    timing: RunTimingBreakdown,
    immutable_report_rel: &str,
    replayed_on_new_build: Option<NewBuildReplayReport>,
) -> Value {
    let (written_report_path, report_write_warning) = write_run_report(
        ctx,
        command,
        identity,
        cache_status,
        cache_miss_reason,
        outcome,
        passed,
        started_at,
        ended_at,
        runtime.clone(),
        yaml_path.clone(),
        yaml_sha256.clone(),
        assertions,
        failure.clone(),
        visible_context.clone(),
        execution,
        attempts.clone(),
        errors.clone(),
        &timing,
        immutable_report_rel,
        replayed_on_new_build.clone(),
    );

    assemble_run_tool_payload(
        ctx,
        identity,
        goal,
        cache_status,
        cache_miss_reason,
        outcome,
        passed,
        runtime,
        yaml_path,
        yaml_sha256,
        assertions,
        failure,
        visible_context,
        attempts,
        errors,
        report_write_warning.into_iter().collect(),
        timing,
        written_report_path.as_deref(),
        replayed_on_new_build,
    )
}

// ---------------------------------------------------------------------------
// Tool 4: deja_remember
// ---------------------------------------------------------------------------

async fn handle_deja_remember(
    ctx: &ToolContext,
    raw_args: Value,
    cancel: &CancellationToken,
) -> ToolDispatchResult {
    let map = match parse_arguments_object(&raw_args, &["goal", "flow", "retries"]) {
        Ok(m) => m,
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
        }
    };

    let raw_goal = match require_non_empty_string(map, "goal") {
        Ok(g) => g,
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
        }
    };
    let goal = match normalize_goal(&raw_goal) {
        Ok(g) => g,
        Err(err) => {
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "INVALID_INPUT",
                &err.to_string(),
            ));
        }
    };
    let raw_flow = match require_non_empty_string(map, "flow") {
        Ok(f) => f,
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
        }
    };
    let retries = match optional_u64_in_range(map, "retries", 0, u64::from(MAX_MCP_RETRIES)) {
        Ok(v) => v.unwrap_or(0) as usize,
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
        }
    };

    let (candidate_abs, candidate_rel) =
        match validate_remember_flow_path(&ctx.config.canonical_workspace_root, &raw_flow) {
            Ok(pair) => pair,
            Err((code, msg)) => {
                return ToolDispatchResult::Response(tool_error(&ctx.redaction, code, &msg));
            }
        };

    let timeout_ms = ctx.config.verification_timeout_ms;
    if let Err((code, msg)) = ctx.config.driver_context_requiring_build(timeout_ms, None) {
        return ToolDispatchResult::Response(tool_error(&ctx.redaction, code, &msg));
    }

    let mut session = tokio::select! {
        biased;
        _ = cancel.cancelled() => return ToolDispatchResult::Cancelled,
        guard = ctx.exec_mutex.lock() => guard,
    };
    if cancel.is_cancelled() {
        session.teardown_driver().await;
        return ToolDispatchResult::Cancelled;
    }

    let cmd_started_at = now_iso8601();
    let cmd_t0 = Instant::now();
    let immutable_report_rel =
        allocate_report_rel_path(RunCommand::Remember, ctx.config.store_id.as_deref());

    let consumer = match build_consumer_flows(&ctx.config) {
        Ok(c) => c,
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "INVALID_MANIFEST",
                &msg,
            ));
        }
    };

    if consumer.is_some()
        && let Err(err) = check_manifest_flow_path(&candidate_rel, "flow")
    {
        return ToolDispatchResult::Response(tool_error(
            &ctx.redaction,
            "CLOSURE_ERROR",
            &err.to_string(),
        ));
    }

    let canonical_candidate = match fs::canonicalize(&candidate_abs) {
        Ok(p) => p,
        Err(err) => {
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "CANDIDATE_NOT_FOUND",
                &format!("candidate flow `{raw_flow}` cannot be resolved: {err}"),
            ));
        }
    };

    let closure =
        match collect_flow_closure(&ctx.config.canonical_workspace_root, &canonical_candidate) {
            Ok(c) => c,
            Err(err) => {
                return ToolDispatchResult::Response(tool_error(
                    &ctx.redaction,
                    "CLOSURE_ERROR",
                    &err.to_string(),
                ));
            }
        };

    if let Err(msg) = verify_closure_files_nofollow(&ctx.config.canonical_workspace_root, &closure)
    {
        return ToolDispatchResult::Response(tool_error(&ctx.redaction, "PATH_CONFINEMENT", &msg));
    }

    if let Err(msg) =
        validate_closure_app_id_confinement(&closure, &ctx.config.app_id, &ctx.config.env)
    {
        return ToolDispatchResult::Response(tool_error(
            &ctx.redaction,
            "APP_ID_CONFINEMENT",
            &msg,
        ));
    }

    if let Err((code, msg)) = validate_closure_mcp_policy(
        &closure,
        &mcp_screenshots_dir(&ctx.config),
        &ctx.config.canonical_workspace_root,
        &ctx.config.env,
    ) {
        return ToolDispatchResult::Response(tool_error(&ctx.redaction, code, &msg));
    }

    if let Err(msg) = check_closure_forbidden_values(&closure, &ctx.config.forbidden_values) {
        return ToolDispatchResult::Response(tool_error(&ctx.redaction, "FORBIDDEN_SECRET", &msg));
    }

    let main_sha256 = closure
        .files
        .iter()
        .find(|f| f.path == closure.main)
        .map(|f| f.sha256.clone())
        .unwrap_or_default();
    let expected_closure_sha256 = closure.sha256.clone();
    let expected_assertions_sha256 = match assertions_sha256(&closure) {
        Ok(h) => h,
        Err(err) => {
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "CLOSURE_ERROR",
                &err.to_string(),
            ));
        }
    };
    let assertions_list = match closure_assertions(&closure) {
        Ok(a) => a,
        Err(err) => {
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "CLOSURE_ERROR",
                &err.to_string(),
            ));
        }
    };
    let assertion_count = count_assertions_in_closure(&closure).ok();

    let store = match SqliteWalkStore::open_writable(store_options(&ctx.config)) {
        Ok(s) => s,
        Err(err) => {
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                err.code(),
                &err.to_string(),
            ));
        }
    };

    let build_path = ctx
        .config
        .build_path
        .as_ref()
        .expect("validated build_path");

    let fp_t0 = Instant::now();
    let build_fp = match session.fingerprint_build(
        &ctx.config.canonical_workspace_root,
        build_path,
        &ctx.config.platform,
    ) {
        Ok(fp) => fp,
        Err(err) => {
            store.close();
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "FINGERPRINT_ERROR",
                &format!("failed to fingerprint build: {err}"),
            ));
        }
    };
    let build_fingerprint_ms = fp_t0.elapsed().as_millis() as u64;
    let identity = make_walk_identity(&ctx.config, &goal, build_fp.clone());

    // Pre-run manifest owner rules and assertion-change guard (`acceptAssertionChange` is never
    // exposed over MCP).
    if let Some(ref cf) = consumer {
        let manifest = match cf.load() {
            Ok(m) => m,
            Err(err) => {
                store.close();
                return ToolDispatchResult::Response(tool_error(
                    &ctx.redaction,
                    err.code(),
                    &err.to_string(),
                ));
            }
        };
        let adapter = ctx.config.adapter.clone().unwrap_or_default();
        let probe = ManifestWalk {
            goal: goal.text.clone(),
            app_id: ctx.config.app_id.clone(),
            platform: ctx.config.platform.clone(),
            adapter,
            browser: ctx.config.browser.clone(),
            flow: candidate_rel.clone(),
            closure_sha256: expected_closure_sha256.clone(),
            assertions_sha256: expected_assertions_sha256.clone(),
            owner_rules: None,
            profile: None,
        };
        let target_key = match manifest_walk_key(&probe) {
            Ok(k) => k,
            Err(err) => {
                store.close();
                return ToolDispatchResult::Response(tool_error(
                    &ctx.redaction,
                    "INVALID_MANIFEST",
                    &err.to_string(),
                ));
            }
        };
        let existing = manifest
            .walks
            .iter()
            .find(|e| manifest_walk_key(e).ok().as_deref() == Some(target_key.as_str()));

        if let Err(err) = check_owner_rules(
            existing.and_then(|e| e.owner_rules.as_ref()),
            &assertions_list,
            Some(&closure),
        ) {
            store.close();
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "OWNER_RULE",
                &err.to_string(),
            ));
        }

        if let Some(ex) = existing
            && ex.assertions_sha256 != expected_assertions_sha256
        {
            store.close();
            let total_ms = cmd_t0.elapsed().as_millis() as u64;
            let timing = RunTimingBreakdown {
                build_fingerprint_ms,
                total_ms,
                ..RunTimingBreakdown::default()
            };
            let payload = build_run_tool_payload(
                ctx,
                RunCommand::Remember,
                &identity,
                &goal,
                CacheStatus::NotChecked,
                Some(CacheMissReason::FlowChanged),
                RunOutcome::AssertionsChanged,
                false,
                cmd_started_at,
                now_iso8601(),
                None,
                Some(candidate_rel),
                Some(main_sha256),
                assertion_count,
                None,
                None,
                None,
                None,
                vec![format!(
                    "the flow's assertions changed (manifest {}, now {}); review and run `deja remember --accept-assertion-change` from the CLI",
                    ex.assertions_sha256, expected_assertions_sha256
                )],
                timing,
                &immutable_report_rel,
                None,
            );
            return ToolDispatchResult::Response(tool_response(&ctx.redaction, payload, true));
        }
    }

    let driver_ctx = match ctx
        .config
        .driver_context_requiring_build(timeout_ms, Some(build_fp.clone()))
    {
        Ok(c) => c,
        Err((code, msg)) => {
            store.close();
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, code, &msg));
        }
    };

    let runtime = match ctx.factory.resolve_runtime(&driver_ctx, cancel).await {
        Ok(rt) => rt,
        Err(DriverError::Cancelled) => {
            session.teardown_driver().await;
            store.close();
            return ToolDispatchResult::Cancelled;
        }
        Err(err) => {
            store.close();
            if cancel.is_cancelled() {
                session.teardown_driver().await;
                return ToolDispatchResult::Cancelled;
            }
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "DRIVER_ERROR",
                &err.to_string(),
            ));
        }
    };

    let driver_attach_ms = match ensure_warm_driver_and_install(
        &mut session,
        ctx.factory.as_ref(),
        &driver_ctx,
        &runtime,
        Some(build_fp.as_str()),
        cancel,
    )
    .await
    {
        Ok(ms) => ms,
        Err(WarmAcquireError::Cancelled) => {
            store.close();
            return ToolDispatchResult::Cancelled;
        }
        Err(WarmAcquireError::Driver(msg)) => {
            store.close();
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "DRIVER_ATTACH_FAILED",
                &msg,
            ));
        }
    };

    let exec_t0 = Instant::now();
    let mut attempts_raw: Vec<ExecutedAttempt> = Vec::new();
    let mut mid_retry_drift: Option<String> = None;

    for attempt_idx in 0..=retries {
        if cancel.is_cancelled() {
            session.teardown_driver().await;
            store.close();
            return ToolDispatchResult::Cancelled;
        }
        if attempt_idx > 0
            && let Err(drift) = verify_unchanged_closure(
                &ctx.config.canonical_workspace_root,
                &canonical_candidate,
                &expected_closure_sha256,
                &expected_assertions_sha256,
            )
        {
            mid_retry_drift = Some(drift);
            break;
        }

        let attempt = {
            let driver = session.driver.as_deref().expect("warm driver present");
            execute_single_closure_attempt(
                driver,
                &ctx.config,
                &closure,
                timeout_ms,
                &ctx.redaction,
                cancel,
            )
            .await
        };
        if cancel.is_cancelled() {
            session.teardown_driver().await;
            store.close();
            return ToolDispatchResult::Cancelled;
        }
        let passed = attempt.passed;
        attempts_raw.push(attempt);
        if passed {
            break;
        }
    }

    if cancel.is_cancelled() {
        session.teardown_driver().await;
        store.close();
        return ToolDispatchResult::Cancelled;
    }

    let flow_exec_ms = exec_t0.elapsed().as_millis() as u64;
    let last_attempt = attempts_raw.last().expect("at least one attempt");

    let mut timing = RunTimingBreakdown {
        build_fingerprint_ms,
        driver_attach_ms,
        warmup_ms: last_attempt.timing.warmup_ms,
        flow_exec_ms,
        store_ms: 0,
        total_ms: cmd_t0.elapsed().as_millis() as u64,
        steps: last_attempt.timing.steps.clone(),
    };

    if let Some(drift_msg) = mid_retry_drift {
        store.close();
        let payload = build_run_tool_payload(
            ctx,
            RunCommand::Remember,
            &identity,
            &goal,
            CacheStatus::NotChecked,
            Some(CacheMissReason::FlowChanged),
            RunOutcome::FlowChanged,
            false,
            cmd_started_at,
            now_iso8601(),
            runtime,
            Some(candidate_rel),
            Some(main_sha256),
            assertion_count,
            None,
            None,
            None,
            None,
            vec![ctx.redaction.redact_str(&drift_msg)],
            timing,
            &immutable_report_rel,
            None,
        );
        return ToolDispatchResult::Response(tool_response(&ctx.redaction, payload, true));
    }

    // Never persist failing candidates.
    if !last_attempt.passed {
        store.close();
        let outcome = failure_run_outcome(last_attempt.failure.as_ref());
        let attempts_report = if attempts_raw.len() >= 2 {
            Some(
                attempts_raw
                    .iter()
                    .enumerate()
                    .map(|(i, a)| RunAttempt {
                        attempt: (i + 1) as u8,
                        passed: a.passed,
                        started_at: a.started_at.clone(),
                        ended_at: a.ended_at.clone(),
                        duration_ms: a.duration_ms,
                        failure: a.failure.clone(),
                        artifacts: a.artifacts.clone(),
                    })
                    .collect(),
            )
        } else {
            None
        };
        let payload = build_run_tool_payload(
            ctx,
            RunCommand::Remember,
            &identity,
            &goal,
            CacheStatus::NotChecked,
            None,
            outcome,
            false,
            cmd_started_at,
            now_iso8601(),
            runtime,
            Some(candidate_rel),
            Some(main_sha256),
            assertion_count,
            last_attempt.failure.clone(),
            last_attempt.visible_context.clone(),
            Some(to_execution_result(last_attempt, Some(&closure))),
            attempts_report,
            last_attempt.errors.clone(),
            timing,
            &immutable_report_rel,
            None,
        );
        return ToolDispatchResult::Response(tool_response(&ctx.redaction, payload, true));
    }

    // Re-verify post-run closure hash unchanged AND not cancelled before persisting!
    if cancel.is_cancelled() {
        session.teardown_driver().await;
        store.close();
        return ToolDispatchResult::Cancelled;
    }
    if let Err(drift_msg) = verify_unchanged_closure(
        &ctx.config.canonical_workspace_root,
        &canonical_candidate,
        &expected_closure_sha256,
        &expected_assertions_sha256,
    ) {
        store.close();
        let payload = build_run_tool_payload(
            ctx,
            RunCommand::Remember,
            &identity,
            &goal,
            CacheStatus::NotChecked,
            Some(CacheMissReason::FlowChanged),
            RunOutcome::FlowChanged,
            false,
            cmd_started_at,
            now_iso8601(),
            runtime,
            Some(candidate_rel),
            Some(main_sha256),
            assertion_count,
            None,
            None,
            Some(to_execution_result(last_attempt, Some(&closure))),
            None,
            vec![ctx.redaction.redact_str(&drift_msg)],
            timing,
            &immutable_report_rel,
            None,
        );
        return ToolDispatchResult::Response(tool_response(&ctx.redaction, payload, true));
    }

    // Write the immutable report BEFORE recording `report_path` in SQLite so we never record
    // or advertise a report path that failed to persist on disk.
    let (written_report_path, report_write_warning) = write_run_report(
        ctx,
        RunCommand::Remember,
        &identity,
        CacheStatus::NotChecked,
        None,
        RunOutcome::Remembered,
        true,
        cmd_started_at.clone(),
        now_iso8601(),
        runtime.clone(),
        Some(candidate_rel.clone()),
        Some(main_sha256.clone()),
        assertion_count,
        None,
        None,
        Some(to_execution_result(last_attempt, Some(&closure))),
        None,
        Vec::new(),
        &timing,
        &immutable_report_rel,
        None,
    );

    let phase_timings_json = serde_json::to_string(&timing).ok();
    let mut validation = RecordValidationInput::new(true, last_attempt.ended_at.clone())
        .with_origin(WalkOrigin::Remember)
        .with_duration_ms(cmd_t0.elapsed().as_millis() as u64);
    if let Some(ref rep_path) = written_report_path {
        validation = validation.with_report_path(rep_path.clone());
    }
    if let Some(ref rt) = runtime {
        validation = validation.with_runtime(rt.clone());
    }
    if let Some(ref pt_json) = phase_timings_json {
        validation = validation.with_phase_timings_json(pt_json.clone());
    }

    let store_t0 = Instant::now();
    let persist_res: Result<(String, String), StoreError> = (|| {
        if let Some(ref cf) = consumer {
            let (stored, _) = cf.save_verified_manifest_walk(
                &store,
                &SaveManifestWalkOptions {
                    identity: identity.clone(),
                    flow_path: candidate_rel.clone(),
                    expected_closure_sha256: expected_closure_sha256.clone(),
                    expected_assertions_sha256: Some(expected_assertions_sha256.clone()),
                    validation,
                    accept_assertion_change: false,
                },
            )?;
            Ok((stored.flow_path, stored.flow_sha256))
        } else {
            let stored = store.persist_verified_candidate(
                &identity,
                &PersistVerifiedCandidateOptions {
                    candidate_path: canonical_candidate.clone(),
                    validation,
                    expected_flow_sha256: Some(main_sha256.clone()),
                    expected_closure_sha256: Some(expected_closure_sha256.clone()),
                    exploration: None,
                    repair_history: None,
                },
            )?;
            Ok((stored.flow_path, stored.flow_sha256))
        }
    })();
    let store_ms = store_t0.elapsed().as_millis() as u64;
    store.close();

    timing.store_ms = store_ms;
    timing.total_ms = cmd_t0.elapsed().as_millis() as u64;

    let (stored_flow_path, stored_flow_sha256) = match persist_res {
        Ok(pair) => pair,
        Err(err) => {
            let drift_outcome = match &err {
                StoreError::ManifestChanged(_)
                | StoreError::Core(
                    CoreError::FlowChanged { .. }
                    | CoreError::ExpectedFlowChecksumMismatch { .. }
                    | CoreError::ExpectedClosureMismatch { .. },
                ) => Some(RunOutcome::FlowChanged),
                StoreError::Core(CoreError::AssertionHashChanged { .. }) => {
                    Some(RunOutcome::AssertionsChanged)
                }
                _ => None,
            };
            if let Some(outcome) = drift_outcome {
                let payload = build_run_tool_payload(
                    ctx,
                    RunCommand::Remember,
                    &identity,
                    &goal,
                    CacheStatus::NotChecked,
                    Some(CacheMissReason::FlowChanged),
                    outcome,
                    false,
                    cmd_started_at,
                    now_iso8601(),
                    runtime,
                    Some(candidate_rel),
                    Some(main_sha256),
                    assertion_count,
                    None,
                    None,
                    Some(to_execution_result(last_attempt, Some(&closure))),
                    None,
                    vec![ctx.redaction.redact_str(&err.to_string())],
                    timing,
                    &immutable_report_rel,
                    None,
                );
                return ToolDispatchResult::Response(tool_response(&ctx.redaction, payload, true));
            }
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                err.code(),
                &err.to_string(),
            ));
        }
    };

    let final_report_path = if written_report_path.is_some() {
        let (refreshed, _) = write_run_report(
            ctx,
            RunCommand::Remember,
            &identity,
            CacheStatus::NotChecked,
            None,
            RunOutcome::Remembered,
            true,
            cmd_started_at,
            now_iso8601(),
            runtime.clone(),
            Some(stored_flow_path.clone()),
            Some(stored_flow_sha256.clone()),
            assertion_count,
            None,
            None,
            Some(to_execution_result(last_attempt, Some(&closure))),
            None,
            Vec::new(),
            &timing,
            &immutable_report_rel,
            None,
        );
        refreshed.or(written_report_path)
    } else {
        None
    };

    let payload = assemble_run_tool_payload(
        ctx,
        &identity,
        &goal,
        CacheStatus::NotChecked,
        None,
        RunOutcome::Remembered,
        true,
        runtime,
        Some(stored_flow_path),
        Some(stored_flow_sha256),
        assertion_count,
        None,
        None,
        None,
        Vec::new(),
        report_write_warning.into_iter().collect(),
        timing,
        final_report_path.as_deref(),
        None,
    );

    ToolDispatchResult::Response(tool_response(&ctx.redaction, payload, false))
}

// ---------------------------------------------------------------------------
// Tool 5: deja_exec_batch (alias: deja_run_steps)
// ---------------------------------------------------------------------------

async fn handle_deja_exec_batch(
    ctx: &ToolContext,
    raw_args: Value,
    cancel: &CancellationToken,
) -> ToolDispatchResult {
    let map = match parse_arguments_object(&raw_args, &["steps", "timeoutMs", "observeFinal"]) {
        Ok(m) => m,
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
        }
    };

    let observe_final = match optional_bool(map, "observeFinal", true) {
        Ok(v) => v,
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
        }
    };
    let timeout_ms = match optional_u64_in_range(map, "timeoutMs", 1, MAX_BATCH_TIMEOUT_MS) {
        Ok(v) => v.unwrap_or(ctx.config.timeout_ms.min(MAX_BATCH_TIMEOUT_MS)),
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
        }
    };

    let Some(steps_val) = map.get("steps") else {
        return ToolDispatchResult::Response(tool_error(
            &ctx.redaction,
            "INVALID_INPUT",
            "missing required field `steps`",
        ));
    };

    let yaml_source = match steps_val {
        Value::String(s) => {
            if s.trim().is_empty() {
                return ToolDispatchResult::Response(tool_error(
                    &ctx.redaction,
                    "INVALID_INPUT",
                    "`steps` YAML string must not be empty",
                ));
            }
            if s.len() > MAX_BATCH_YAML_BYTES {
                return ToolDispatchResult::Response(tool_error(
                    &ctx.redaction,
                    "INVALID_INPUT",
                    &format!(
                        "`steps` exceeds maximum serialized size of {MAX_BATCH_YAML_BYTES} bytes"
                    ),
                ));
            }
            s.clone()
        }
        Value::Array(items) => {
            if items.len() < MIN_BATCH_STEPS || items.len() > MAX_BATCH_STEPS {
                return ToolDispatchResult::Response(tool_error(
                    &ctx.redaction,
                    "INVALID_INPUT",
                    &format!(
                        "`steps` array must contain between {MIN_BATCH_STEPS} and {MAX_BATCH_STEPS} steps"
                    ),
                ));
            }
            for (idx, item) in items.iter().enumerate() {
                if !matches!(item, Value::String(_) | Value::Object(_)) {
                    return ToolDispatchResult::Response(tool_error(
                        &ctx.redaction,
                        "INVALID_INPUT",
                        &format!("`steps[{idx}]` must be a step string or command object"),
                    ));
                }
            }
            let serialized = match serde_json::to_string(steps_val) {
                Ok(s) => s,
                Err(err) => {
                    return ToolDispatchResult::Response(tool_error(
                        &ctx.redaction,
                        "INVALID_INPUT",
                        &format!("failed to serialize `steps`: {err}"),
                    ));
                }
            };
            if serialized.len() > MAX_BATCH_YAML_BYTES {
                return ToolDispatchResult::Response(tool_error(
                    &ctx.redaction,
                    "INVALID_INPUT",
                    &format!(
                        "`steps` exceeds maximum serialized size of {MAX_BATCH_YAML_BYTES} bytes"
                    ),
                ));
            }
            serialized
        }
        _ => {
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "INVALID_INPUT",
                "`steps` must be a YAML string or an array of step objects/strings",
            ));
        }
    };

    let parsed = match parse_flow(&yaml_source) {
        Ok(p) => p,
        Err(err) => {
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "INVALID_INPUT",
                &format!("invalid `steps` batch: {err}"),
            ));
        }
    };

    // Enforce step count bounds POST-PARSE to defeat YAML anchor/alias amplification.
    if parsed.steps.len() < MIN_BATCH_STEPS || parsed.steps.len() > MAX_BATCH_STEPS {
        return ToolDispatchResult::Response(tool_error(
            &ctx.redaction,
            "INVALID_INPUT",
            &format!(
                "`steps` must expand to between {MIN_BATCH_STEPS} and {MAX_BATCH_STEPS} top-level steps (got {})",
                parsed.steps.len()
            ),
        ));
    }

    let screenshots_dir = mcp_screenshots_dir(&ctx.config);
    let mut total_ast_steps = 0usize;
    if let Err((code, msg)) = validate_batch_steps_confinement(
        &parsed,
        &screenshots_dir,
        &ctx.config.canonical_workspace_root,
        &ctx.config.app_id,
        &ctx.config.env,
        &mut total_ast_steps,
    ) {
        return ToolDispatchResult::Response(tool_error(&ctx.redaction, code, &msg));
    }

    if let Err((code, msg)) = ctx.config.driver_context_for_batch(timeout_ms, None) {
        return ToolDispatchResult::Response(tool_error(&ctx.redaction, code, &msg));
    }

    let mut session = tokio::select! {
        biased;
        _ = cancel.cancelled() => return ToolDispatchResult::Cancelled,
        guard = ctx.exec_mutex.lock() => guard,
    };
    if cancel.is_cancelled() {
        session.teardown_driver().await;
        return ToolDispatchResult::Cancelled;
    }

    let build_path = ctx
        .config
        .build_path
        .as_ref()
        .expect("validated build_path");

    let t0 = Instant::now();
    let fp_t0 = Instant::now();
    let current_fp = match session.fingerprint_build(
        &ctx.config.canonical_workspace_root,
        build_path,
        &ctx.config.platform,
    ) {
        Ok(fp) => fp,
        Err(err) => {
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "FINGERPRINT_ERROR",
                &format!("failed to fingerprint build: {err}"),
            ));
        }
    };
    let build_fingerprint_ms = fp_t0.elapsed().as_millis() as u64;

    let driver_ctx = match ctx
        .config
        .driver_context_for_batch(timeout_ms, Some(current_fp.clone()))
    {
        Ok(c) => c,
        Err((code, msg)) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, code, &msg));
        }
    };

    let runtime = match ctx.factory.resolve_runtime(&driver_ctx, cancel).await {
        Ok(rt) => rt,
        Err(DriverError::Cancelled) => {
            session.teardown_driver().await;
            return ToolDispatchResult::Cancelled;
        }
        Err(err) => {
            if cancel.is_cancelled() {
                session.teardown_driver().await;
                return ToolDispatchResult::Cancelled;
            }
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "DRIVER_ERROR",
                &err.to_string(),
            ));
        }
    };

    let driver_attach_ms = match ensure_warm_driver_and_install(
        &mut session,
        ctx.factory.as_ref(),
        &driver_ctx,
        &runtime,
        Some(current_fp.as_str()),
        cancel,
    )
    .await
    {
        Ok(ms) => ms,
        Err(WarmAcquireError::Cancelled) => return ToolDispatchResult::Cancelled,
        Err(WarmAcquireError::Driver(msg)) => {
            return ToolDispatchResult::Response(tool_error(
                &ctx.redaction,
                "DRIVER_ATTACH_FAILED",
                &msg,
            ));
        }
    };

    let mut exec_cfg = FlowExecutionConfig::new(ctx.config.canonical_workspace_root.clone());
    exec_cfg.app_id_override = Some(ctx.config.app_id.clone());
    exec_cfg.env = ctx.config.env.clone();
    exec_cfg.flow_timeout = Some(Duration::from_millis(timeout_ms));
    exec_cfg.redactor = ctx.redaction.redactor.clone();
    exec_cfg.provider_redactor = ctx.redaction.provider.clone();
    exec_cfg.screenshot_dir = Some(screenshots_dir);
    exec_cfg.screenshot_confine_to_dir = true;
    exec_cfg.screenshot_no_clobber = true;
    exec_cfg.capture_final_visible_context = observe_final;

    let exec_t0 = Instant::now();
    let outcome = {
        let driver = session.driver.as_deref().expect("warm driver present");
        FlowExecutor::new(exec_cfg)
            .execute_yaml(driver, &yaml_source, cancel)
            .await
    };
    if cancel.is_cancelled() {
        session.teardown_driver().await;
        return ToolDispatchResult::Cancelled;
    }

    let flow_exec_ms = exec_t0.elapsed().as_millis() as u64;
    let total_ms = t0.elapsed().as_millis() as u64;
    let timing = RunTimingBreakdown {
        build_fingerprint_ms,
        driver_attach_ms,
        warmup_ms: outcome.timing.warmup_ms,
        flow_exec_ms,
        store_ms: 0,
        total_ms,
        steps: outcome.timing.steps.clone(),
    };

    let visible_context = if outcome.passed {
        outcome
            .final_visible_context
            .as_ref()
            .map(|vc| ctx.redaction.redact_visible_context(vc))
    } else {
        outcome
            .failed_step
            .as_ref()
            .and_then(|s| s.visible_context.as_ref())
            .map(|vc| ctx.redaction.redact_visible_context(vc))
    };

    let mut errors: Vec<String> = Vec::new();
    if let Some(ref step) = outcome.failed_step
        && !step.message.is_empty()
    {
        errors.push(truncate_chars(
            &ctx.redaction.redact_str(&step.message),
            MAX_OUTPUT_TAIL_CHARS,
        ));
    }
    for hook_err in outcome.hook_errors.iter().take(MAX_OUTPUT_TAIL_ITEMS) {
        if errors.len() >= MAX_OUTPUT_TAIL_ITEMS {
            break;
        }
        errors.push(truncate_chars(
            &ctx.redaction.redact_str(hook_err),
            MAX_OUTPUT_TAIL_CHARS,
        ));
    }

    let screenshots: Vec<String> = outcome
        .screenshots
        .iter()
        .map(|p| workspace_relative_display(&ctx.config.canonical_workspace_root, p))
        .collect();

    let mut out = Map::new();
    out.insert("passed".to_string(), Value::Bool(outcome.passed));
    out.insert(
        "stepsExecuted".to_string(),
        json!(outcome.timing.steps.len()),
    );
    out.insert(
        "assertionsExecuted".to_string(),
        json!(outcome.assertions_executed),
    );
    out.insert("durationMs".to_string(), json!(total_ms));
    out.insert("steps".to_string(), json!(outcome.timing.steps));
    out.insert("timing".to_string(), json!(timing));
    if let Some(rt) = runtime {
        out.insert("runtime".to_string(), Value::String(rt));
    }
    if let Some(vc) = visible_context {
        out.insert("visibleElements".to_string(), json!(vc.items()));
        out.insert(
            "visibleElementsTruncated".to_string(),
            Value::Bool(vc.is_truncated()),
        );
    }
    if let Some(failure) = outcome.failure {
        out.insert("failure".to_string(), json!(failure));
    }
    if !errors.is_empty() {
        out.insert("errors".to_string(), json!(errors));
    }
    if !screenshots.is_empty() {
        out.insert("screenshots".to_string(), json!(screenshots));
    }

    ToolDispatchResult::Response(tool_response(
        &ctx.redaction,
        Value::Object(out),
        !outcome.passed,
    ))
}

fn validate_closure_mcp_policy(
    closure: &FlowClosure,
    screenshots_dir: &Path,
    canonical_workspace: &Path,
    base_env: &BTreeMap<String, String>,
) -> Result<(), (&'static str, String)> {
    for file in &closure.files {
        if file.kind != FlowReferenceKind::Flow {
            continue;
        }
        let text = std::str::from_utf8(&file.bytes).map_err(|err| {
            (
                "CLOSURE_ERROR",
                format!("flow `{}` is not valid UTF-8: {err}", file.source),
            )
        })?;
        let parsed = parse_flow(text).map_err(|err| {
            (
                "CLOSURE_ERROR",
                format!("flow `{}` failed to parse: {err}", file.source),
            )
        })?;
        validate_mcp_flow_policy(
            &parsed,
            screenshots_dir,
            canonical_workspace,
            base_env,
            true,
            None,
        )?;
    }
    Ok(())
}

fn validate_batch_steps_confinement(
    parsed: &ParsedFlow,
    screenshots_dir: &Path,
    canonical_workspace: &Path,
    expected_app_id: &str,
    base_env: &BTreeMap<String, String>,
    total_ast_steps: &mut usize,
) -> Result<(), (&'static str, String)> {
    validate_flow_app_id_confinement(parsed, expected_app_id, base_env)
        .map_err(|msg| ("APP_ID_CONFINEMENT", msg))?;

    validate_mcp_flow_policy(
        parsed,
        screenshots_dir,
        canonical_workspace,
        base_env,
        false,
        Some(total_ast_steps),
    )
}

fn validate_mcp_flow_policy(
    parsed: &ParsedFlow,
    screenshots_dir: &Path,
    canonical_workspace: &Path,
    base_env: &BTreeMap<String, String>,
    allow_file_and_media: bool,
    mut total_ast_steps: Option<&mut usize>,
) -> Result<(), (&'static str, String)> {
    let mut env_stack = EnvStack::new(base_env.clone());
    let mut header_frame = BTreeMap::new();
    for (k, v) in &parsed.header.env {
        let interp = env_stack.interpolate(v).map_err(|err| {
            (
                "CONFINEMENT_ERROR",
                format!("header env `{k}` interpolation failed: {err}"),
            )
        })?;
        header_frame.insert(k.clone(), interp);
    }
    if !header_frame.is_empty() {
        env_stack.push(header_frame);
    }

    for steps in [
        &parsed.header.on_flow_start,
        &parsed.steps,
        &parsed.header.on_flow_complete,
    ] {
        check_mcp_steps_recursive(
            steps,
            screenshots_dir,
            canonical_workspace,
            &mut env_stack,
            allow_file_and_media,
            total_ast_steps.as_deref_mut(),
        )?;
    }

    Ok(())
}

fn check_mcp_steps_recursive(
    steps: &[FlowStep],
    screenshots_dir: &Path,
    canonical_workspace: &Path,
    env_stack: &mut EnvStack,
    allow_file_and_media: bool,
    mut total_ast_steps: Option<&mut usize>,
) -> Result<(), (&'static str, String)> {
    for step in steps {
        if let Some(ref mut count) = total_ast_steps {
            **count = count.saturating_add(1);
            if **count > MAX_BATCH_TOTAL_AST_STEPS {
                return Err((
                    "CONFINEMENT_ERROR",
                    format!(
                        "batch exceeds maximum expanded step count ({MAX_BATCH_TOTAL_AST_STEPS})"
                    ),
                ));
            }
        }
        match step {
            FlowStep::ClearKeychain => {
                return Err((
                    "DEVICE_WIDE_COMMAND_FORBIDDEN",
                    "device-wide `clearKeychain` is forbidden in app-confined MCP flows"
                        .to_string(),
                ));
            }
            FlowStep::LaunchApp(launch) if launch.clear_keychain => {
                return Err((
                    "DEVICE_WIDE_COMMAND_FORBIDDEN",
                    "device-wide `launchApp.clearKeychain: true` is forbidden in app-confined MCP flows"
                        .to_string(),
                ));
            }
            FlowStep::RunFlow(rf) => {
                let mut child_frame = BTreeMap::new();
                for (k, v) in &rf.env {
                    let interp = env_stack.interpolate(v).map_err(|err| {
                        (
                            "CONFINEMENT_ERROR",
                            format!("runFlow env `{k}` interpolation failed: {err}"),
                        )
                    })?;
                    child_frame.insert(k.clone(), interp);
                }
                let pushed = !child_frame.is_empty();
                if pushed {
                    env_stack.push(child_frame);
                }
                let res = match &rf.target {
                    RunFlowTarget::File(_) if !allow_file_and_media => Err((
                        "CONFINEMENT_ERROR",
                        "`runFlow.file` is not allowed in `deja_exec_batch`; pass inline steps or use `deja_remember`"
                            .to_string(),
                    )),
                    RunFlowTarget::File(_) => Ok(()),
                    RunFlowTarget::Commands(cmds) => check_mcp_steps_recursive(
                        cmds,
                        screenshots_dir,
                        canonical_workspace,
                        env_stack,
                        allow_file_and_media,
                        total_ast_steps.as_deref_mut(),
                    ),
                };
                if pushed {
                    env_stack.pop();
                }
                res?;
            }
            FlowStep::Retry(rt) => {
                let mut child_frame = BTreeMap::new();
                for (k, v) in &rt.env {
                    let interp = env_stack.interpolate(v).map_err(|err| {
                        (
                            "CONFINEMENT_ERROR",
                            format!("retry env `{k}` interpolation failed: {err}"),
                        )
                    })?;
                    child_frame.insert(k.clone(), interp);
                }
                let pushed = !child_frame.is_empty();
                if pushed {
                    env_stack.push(child_frame);
                }
                let res = match &rt.target {
                    RunFlowTarget::File(_) if !allow_file_and_media => Err((
                        "CONFINEMENT_ERROR",
                        "`retry.file` is not allowed in `deja_exec_batch`; pass inline steps"
                            .to_string(),
                    )),
                    RunFlowTarget::File(_) => Ok(()),
                    RunFlowTarget::Commands(cmds) => check_mcp_steps_recursive(
                        cmds,
                        screenshots_dir,
                        canonical_workspace,
                        env_stack,
                        allow_file_and_media,
                        total_ast_steps.as_deref_mut(),
                    ),
                };
                if pushed {
                    env_stack.pop();
                }
                res?;
            }
            FlowStep::Repeat(rep) => {
                check_mcp_steps_recursive(
                    &rep.commands,
                    screenshots_dir,
                    canonical_workspace,
                    env_stack,
                    allow_file_and_media,
                    total_ast_steps.as_deref_mut(),
                )?;
            }
            FlowStep::AddMedia(_) if !allow_file_and_media => {
                return Err((
                    "CONFINEMENT_ERROR",
                    "`addMedia` is not allowed in `deja_exec_batch`; use `deja_remember` with a workspace flow"
                        .to_string(),
                ));
            }
            FlowStep::TakeScreenshot(ts) => {
                let resolved = env_stack.interpolate(&ts.path).map_err(|err| {
                    (
                        "PATH_CONFINEMENT",
                        format!("takeScreenshot path interpolation failed: {err}"),
                    )
                })?;
                if resolved.contains("${") {
                    if !allow_file_and_media {
                        return Err((
                            "PATH_CONFINEMENT",
                            format!(
                                "takeScreenshot path `{resolved}` contains an unresolvable variable"
                            ),
                        ));
                    }
                    // For file-backed subflows receiving env vars from parent `runFlow.env`,
                    // runtime `exec_take_screenshot` enforces strict confinement and no-clobber
                    // on the fully interpolated path.
                    continue;
                }
                let with_ext = if Path::new(&resolved).extension().is_none() {
                    format!("{resolved}.png")
                } else {
                    resolved.clone()
                };
                let segments = parse_safe_relative_segments(&with_ext).map_err(|msg| {
                    (
                        "PATH_CONFINEMENT",
                        format!("unconfined takeScreenshot path `{resolved}`: {msg}"),
                    )
                })?;
                if segments.iter().any(|s| s.starts_with('.')) {
                    return Err((
                        "PATH_CONFINEMENT",
                        format!(
                            "takeScreenshot path `{resolved}` cannot contain hidden path segments"
                        ),
                    ));
                }
                // Refuse any existing symlink from workspace root down to the dedicated
                // `mcp-screenshots/` artifact target, and refuse overwriting existing files.
                let mut target = screenshots_dir.to_path_buf();
                for seg in &segments {
                    target.push(seg);
                }
                if let Ok(rel_from_ws) = target.strip_prefix(canonical_workspace) {
                    let mut cursor = canonical_workspace.to_path_buf();
                    for comp in rel_from_ws.components() {
                        if let std::path::Component::Normal(seg) = comp {
                            cursor.push(seg);
                            if let Ok(meta) = fs::symlink_metadata(&cursor)
                                && meta.file_type().is_symlink()
                            {
                                return Err((
                                    "PATH_CONFINEMENT",
                                    format!(
                                        "takeScreenshot path `{resolved}` traverses or targets a symbolic link"
                                    ),
                                ));
                            }
                        }
                    }
                }
                if fs::symlink_metadata(&target).is_ok() {
                    return Err((
                        "PATH_CONFINEMENT",
                        format!(
                            "takeScreenshot path `{resolved}` refuses to overwrite existing artifact file"
                        ),
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tool 6: deja_explore (gated by `config.mcp_explore`)
// ---------------------------------------------------------------------------

fn handle_deja_explore(ctx: &ToolContext, raw_args: Value) -> ToolDispatchResult {
    if !ctx.config.mcp_explore {
        return ToolDispatchResult::Response(tool_error(
            &ctx.redaction,
            "EXPLORE_DISABLED",
            "deja_explore is disabled; set `mcp.explore: true` in deja.config.json to enable graph exploration",
        ));
    }

    let map = match parse_arguments_object(&raw_args, &["goal", "maxSteps", "maxDepth"]) {
        Ok(m) => m,
        Err(msg) => {
            return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
        }
    };

    if let Err(msg) = optional_non_empty_string(map, "goal") {
        return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
    }
    if let Err(msg) = optional_u64_in_range(map, "maxSteps", 1, 500) {
        return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
    }
    if let Err(msg) = optional_u64_in_range(map, "maxDepth", 1, 50) {
        return ToolDispatchResult::Response(tool_error(&ctx.redaction, "INVALID_INPUT", &msg));
    }

    ToolDispatchResult::Response(tool_error(
        &ctx.redaction,
        "UNSUPPORTED_PHASE",
        "Graph exploration lands in DEJA-71",
    ))
}
