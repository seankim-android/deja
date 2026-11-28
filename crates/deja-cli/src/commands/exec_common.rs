//! Shared flow validation, live execution, crash-check fallback, timing, and reporting helpers
//! used by `deja remember` and `deja run`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deja_core::{
    ArtifactKind, CacheMissReason, CacheStatus, DEFAULT_FLOW_SCHEMA_VERSION, ExecutionResult,
    FailureCategory, FlowClosure, FlowReferenceKind, Goal, RunArtifact, RunCommand, RunOutcome,
    RunResult, WALK_RESULT_SCHEMA_VERSION, WalkFailure, WalkIdentity, bytes_hold_forbidden_value,
    holds_forbidden_value, normalize_goal,
};
use deja_flow::{
    ClassificationInput, DriverCapability, EnvStack, FailureClassifier, FlowExecutionConfig,
    FlowExecutionOutcome, FlowExecutor, FlowStep, ParsedFlow, PlatformDriver, RunFlowTarget,
    RunTimingBreakdown, VisibleContext, count_assertions_in_closure, parse_flow,
    validate_flow_app_id_confinement,
};
use deja_store::{ConsumerFlows, ConsumerFlowsOptions, now_iso8601};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::CliDeps;
use crate::config::LoadedConfig;
use crate::driver_factory::DriverContext;
use crate::exit_codes::{EXIT_USAGE_OR_CONFIG, exit_code_for_outcome};
use crate::fs_safe::{lexical_normalize, write_atomic_confined, write_atomic_nofollow_excl};
use crate::output::{
    CliRunReport, RedactionContext, emit_summary, format_error_summary_line, format_summary_line,
    native_engine_report, print_human_run_report, workspace_relative_display,
    write_early_error_junit, write_report_junit,
};

static PREALLOC_REPORT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Pre-allocate a unique workspace-relative report path so SQLite `validation_runs.report_path`
/// and the saved `.deja/reports/*.json` file share the exact same path.
#[must_use]
pub fn allocate_report_rel_path(command: RunCommand) -> String {
    let cmd = match command {
        RunCommand::Run => "run",
        RunCommand::Remember => "remember",
        RunCommand::Explore => "explore",
    };
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = PREALLOC_REPORT_SEQ.fetch_add(1, Ordering::Relaxed);
    format!(".deja/reports/{cmd}-{ms}-{}-{seq}.json", std::process::id())
}

/// Write redacted report to the pre-allocated immutable path, per-store latest path (`DEJA-35`),
/// and optional explicit `--report <path>`.
pub fn write_allocated_reports(
    canonical_workspace: &Path,
    store_id: Option<&str>,
    immutable_rel_path: &str,
    redacted_json: &serde_json::Value,
    explicit_report: Option<&Path>,
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

    if let Some(path) = explicit_report {
        write_atomic_nofollow_excl(path, bytes)?;
    }
    Ok(())
}

/// Build a [`ConsumerFlows`] handler when `config.manifest` is configured.
pub fn build_consumer_flows(config: &LoadedConfig) -> Result<Option<ConsumerFlows>, String> {
    let Some(ref manifest_rel) = config.manifest else {
        return Ok(None);
    };
    let mut opts = ConsumerFlowsOptions::new(&config.workspace_root, manifest_rel)
        .with_forbidden_values(config.forbidden_values.clone());
    if let Some(ref flows_dir) = config.flows_dir {
        opts = opts.with_flows_dir(flows_dir);
    }
    ConsumerFlows::new(opts)
        .map(Some)
        .map_err(|err| err.to_string())
}

/// Build a [`DriverContext`] from validated [`LoadedConfig`] and [`CliDeps`].
pub fn build_driver_context(
    config: &LoadedConfig,
    deps: &CliDeps,
    timeout_ms: u64,
) -> DriverContext {
    DriverContext {
        platform: config.platform.clone(),
        device: config.device.clone().unwrap_or_default(),
        app_id: config.app_id.clone(),
        build_path: config.build_path.clone().unwrap_or_default(),
        workspace_root: config.workspace_root.clone(),
        disable_animations: config.disable_animations,
        crash_report_grace_ms: config.crash_report_grace_ms,
        crash_report_dir: config.crash_report_directory.clone(),
        deja_home: deps
            .env
            .get("DEJA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from),
        timeout_ms,
        env: deps.env.clone(),
    }
}

/// Validate static `appId` confinement across every flow and subflow in a collected [`FlowClosure`].
///
/// Walks `runFlow`/`retry` file calls starting from `closure.main` so statically-passed subflow
/// `env` frames propagate accurately, AND checks every remaining `Flow` file in `closure.files`.
pub fn validate_closure_app_id_confinement(
    closure: &FlowClosure,
    expected_app_id: &str,
    base_env: &BTreeMap<String, String>,
) -> Result<(), String> {
    let mut parsed_by_path: BTreeMap<&str, ParsedFlow> = BTreeMap::new();
    for file in &closure.files {
        if file.kind != FlowReferenceKind::Flow {
            continue;
        }
        let text = std::str::from_utf8(&file.bytes)
            .map_err(|err| format!("flow `{}` is not valid UTF-8: {err}", file.source))?;
        let parsed = parse_flow(text)
            .map_err(|err| format!("flow `{}` failed to parse: {err}", file.source))?;
        parsed_by_path.insert(file.path.as_str(), parsed);
    }

    let mut validated: HashSet<String> = HashSet::new();
    let mut call_stack: Vec<String> = Vec::new();

    if parsed_by_path.contains_key(closure.main.as_str()) {
        validate_flow_tree(
            closure.main.as_str(),
            &parsed_by_path,
            expected_app_id,
            base_env,
            &mut validated,
            &mut call_stack,
        )?;
    }

    // Belt-and-suspenders: ensure every YAML flow file in the closure was validated.
    for (path, parsed) in &parsed_by_path {
        if !validated.contains(*path) {
            validate_flow_app_id_confinement(parsed, expected_app_id, base_env)
                .map_err(|err| format!("flow `{path}` violates appId confinement: {err}"))?;
        }
    }
    Ok(())
}

fn validate_flow_tree(
    rel_path: &str,
    parsed_by_path: &BTreeMap<&str, ParsedFlow>,
    expected_app_id: &str,
    env: &BTreeMap<String, String>,
    validated: &mut HashSet<String>,
    call_stack: &mut Vec<String>,
) -> Result<(), String> {
    if call_stack.iter().any(|p| p == rel_path) {
        return Ok(());
    }
    let Some(parsed) = parsed_by_path.get(rel_path) else {
        return Ok(());
    };
    validate_flow_app_id_confinement(parsed, expected_app_id, env)
        .map_err(|err| format!("flow `{rel_path}` violates appId confinement: {err}"))?;
    validated.insert(rel_path.to_string());

    let mut stack = EnvStack::new(env.clone());
    let mut header_frame = BTreeMap::new();
    for (k, v) in &parsed.header.env {
        let interp = stack
            .interpolate(v)
            .map_err(|err| format!("flow `{rel_path}` header env `{k}`: {err}"))?;
        header_frame.insert(k.clone(), interp);
    }
    if !header_frame.is_empty() {
        stack.push(header_frame);
    }

    call_stack.push(rel_path.to_string());
    let dir = Path::new(rel_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    for steps in [
        &parsed.header.on_flow_start,
        &parsed.steps,
        &parsed.header.on_flow_complete,
    ] {
        validate_referenced_subflows(
            dir,
            steps,
            parsed_by_path,
            expected_app_id,
            &mut stack,
            validated,
            call_stack,
        )?;
    }
    call_stack.pop();
    Ok(())
}

fn validate_referenced_subflows(
    current_dir: &Path,
    steps: &[FlowStep],
    parsed_by_path: &BTreeMap<&str, ParsedFlow>,
    expected_app_id: &str,
    stack: &mut EnvStack,
    validated: &mut HashSet<String>,
    call_stack: &mut Vec<String>,
) -> Result<(), String> {
    for step in steps {
        match step {
            FlowStep::RunFlow(rf) => {
                let mut child_frame = BTreeMap::new();
                for (k, v) in &rf.env {
                    let interp = stack
                        .interpolate(v)
                        .map_err(|err| format!("runFlow env `{k}`: {err}"))?;
                    child_frame.insert(k.clone(), interp);
                }
                let pushed = !child_frame.is_empty();
                if pushed {
                    stack.push(child_frame);
                }
                let res = match &rf.target {
                    RunFlowTarget::File(sub) => {
                        let resolved = resolve_closure_rel_path(current_dir, sub);
                        let merged = stack.merged();
                        validate_flow_tree(
                            &resolved,
                            parsed_by_path,
                            expected_app_id,
                            &merged,
                            validated,
                            call_stack,
                        )
                    }
                    RunFlowTarget::Commands(cmds) => validate_referenced_subflows(
                        current_dir,
                        cmds,
                        parsed_by_path,
                        expected_app_id,
                        stack,
                        validated,
                        call_stack,
                    ),
                };
                if pushed {
                    stack.pop();
                }
                res?;
            }
            FlowStep::Retry(rt) => {
                let mut child_frame = BTreeMap::new();
                for (k, v) in &rt.env {
                    let interp = stack
                        .interpolate(v)
                        .map_err(|err| format!("retry env `{k}`: {err}"))?;
                    child_frame.insert(k.clone(), interp);
                }
                let pushed = !child_frame.is_empty();
                if pushed {
                    stack.push(child_frame);
                }
                let res = match &rt.target {
                    RunFlowTarget::File(sub) => {
                        let resolved = resolve_closure_rel_path(current_dir, sub);
                        let merged = stack.merged();
                        validate_flow_tree(
                            &resolved,
                            parsed_by_path,
                            expected_app_id,
                            &merged,
                            validated,
                            call_stack,
                        )
                    }
                    RunFlowTarget::Commands(cmds) => validate_referenced_subflows(
                        current_dir,
                        cmds,
                        parsed_by_path,
                        expected_app_id,
                        stack,
                        validated,
                        call_stack,
                    ),
                };
                if pushed {
                    stack.pop();
                }
                res?;
            }
            FlowStep::Repeat(rep) => {
                validate_referenced_subflows(
                    current_dir,
                    &rep.commands,
                    parsed_by_path,
                    expected_app_id,
                    stack,
                    validated,
                    call_stack,
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn resolve_closure_rel_path(current_dir: &Path, target: &str) -> String {
    let joined = if current_dir.as_os_str().is_empty() {
        PathBuf::from(target)
    } else {
        current_dir.join(target)
    };
    lexical_normalize(&joined)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Check every file in `closure` against configured `forbidden_values` (fixture credentials /
/// secrets) when running in manifest mode.
pub fn check_closure_forbidden_values(
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

/// Result of one executed flow attempt including timestamps, redacted visible context, and
/// post-executor crash-check fallback.
#[derive(Debug, Clone)]
pub struct ExecutedAttempt {
    pub started_at: String,
    pub ended_at: String,
    pub duration_ms: u64,
    pub passed: bool,
    pub assertions_executed: u64,
    pub failure: Option<WalkFailure>,
    pub visible_context: Option<VisibleContext>,
    pub timing: RunTimingBreakdown,
    pub errors: Vec<String>,
    pub artifacts: Vec<RunArtifact>,
}

/// Execute a single flow file on an already-attached `driver`, including post-executor `.ips`
/// crash-report fallback classification (`DEJA-46`, `DEJA-52`).
pub async fn execute_single_attempt(
    driver: &dyn PlatformDriver,
    config: &LoadedConfig,
    flow_path: &Path,
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

    let mut exec_cfg = FlowExecutionConfig::new(config.workspace_root.clone());
    exec_cfg.app_id_override = Some(config.app_id.clone());
    exec_cfg.env = config.env.clone();
    exec_cfg.flow_timeout = Some(Duration::from_millis(timeout_ms));
    exec_cfg.redactor = redaction.redactor.clone();
    exec_cfg.provider_redactor = redaction.provider.clone();

    let executor = FlowExecutor::new(exec_cfg);
    let mut outcome: FlowExecutionOutcome = executor.execute_file(driver, flow_path, cancel).await;

    // Post-executor crash check fallback: if a failed attempt was classified as selector,
    // navigation, or unknown without a crash signal, query the driver's native crash detector.
    if !outcome.passed
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
            path: workspace_relative_display(&config.workspace_root, shot),
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

/// Convert an [`ExecutedAttempt`] into a schema-1 [`ExecutionResult`].
#[must_use]
pub fn to_execution_result(
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

/// Build a base [`CliRunReport`] populated with exact native engine metadata.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn build_base_report(
    command: RunCommand,
    identity: WalkIdentity,
    goal: &Goal,
    cache_status: CacheStatus,
    cache_miss_reason: Option<CacheMissReason>,
    outcome: RunOutcome,
    passed: bool,
    started_at: String,
    ended_at: String,
    duration_ms: u64,
    runtime: Option<String>,
) -> CliRunReport {
    CliRunReport {
        core: RunResult {
            schema_version: WALK_RESULT_SCHEMA_VERSION,
            command,
            identity,
            goal_key: goal.key.clone(),
            cache_status,
            cache_miss_reason,
            semantic: None,
            outcome,
            passed,
            started_at,
            ended_at,
            duration_ms,
            model_calls: Some(0),
            model_call_basis: "native-rust-direct".to_string(),
            assertions: None,
            repair_count: 0,
            yaml_path: None,
            yaml_sha256: None,
            failure: None,
            artifacts: Vec::new(),
            diffs: Vec::new(),
            errors: Vec::new(),
            execution: None,
            exploration: None,
            repair: None,
            replayed_on_new_build: None,
            engine: Some(native_engine_report()),
            attempts: None,
        },
        runtime,
        timing: None,
        visible_context: None,
    }
}

/// Build a [`WalkIdentity`] from config, goal text, and build fingerprint.
#[must_use]
pub fn make_walk_identity(
    config: &LoadedConfig,
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

/// Finalize a [`CliRunReport`]: write redacted JUnit if requested, persist per-store & explicit
/// reports, print JSON or human output, emit `--summary` if requested, and return exit code.
#[allow(clippy::too_many_arguments)]
pub fn finalize_run_report(
    deps: &CliDeps,
    workspace_root: &Path,
    store_id: Option<&str>,
    immutable_rel_path: &str,
    explicit_report: Option<&Path>,
    junit_path: Option<&Path>,
    json_mode: bool,
    summary_mode: bool,
    redaction: &RedactionContext,
    report: &mut CliRunReport,
) -> u8 {
    if let Some(jpath) = junit_path
        && let Err(err) = write_report_junit(jpath, workspace_root, report, redaction)
    {
        report
            .core
            .errors
            .push(redaction.redact_str(&format!("failed to write JUnit report: {err}")));
    }

    let redacted_json = match redaction.redact_report(report) {
        Ok(val) => val,
        Err(err) => {
            deps.output.writeln_stderr(&format!(
                "deja error: failed to serialize run report: {err}"
            ));
            return EXIT_USAGE_OR_CONFIG;
        }
    };

    if let Err(err) = write_allocated_reports(
        workspace_root,
        store_id,
        immutable_rel_path,
        &redacted_json,
        explicit_report,
    ) {
        deps.output.writeln_stderr(&redaction.redact_str(&format!(
            "deja warning: failed to write run report artifact: {err}"
        )));
    }

    if json_mode {
        deps.output.writeln_stdout(
            &serde_json::to_string_pretty(&redacted_json).expect("redacted JSON serializes"),
        );
    } else {
        print_human_run_report(&deps.output, report, redaction);
    }

    if summary_mode {
        let line = format_summary_line(&report.core, redaction);
        emit_summary(&deps.output, json_mode, &line);
    }

    exit_code_for_outcome(report.core.outcome)
}

/// Emit an early error (before a full [`CliRunReport`] exists), writing errored redacted JUnit
/// and error `--summary` trailer if requested.
#[allow(clippy::too_many_arguments)]
pub fn emit_early_exec_error(
    deps: &CliDeps,
    command_name: &str,
    raw_goal: &str,
    junit_path: Option<&Path>,
    json_mode: bool,
    summary_mode: bool,
    redaction: &RedactionContext,
    code: &str,
    message: &str,
    exit_code: u8,
) -> u8 {
    let scrubbed = redaction.redact_str(message);
    if let Some(jpath) = junit_path {
        let _ = write_early_error_junit(jpath, raw_goal, &scrubbed, redaction);
    }
    if json_mode {
        let payload = json!({
            "error": { "code": code, "message": scrubbed }
        });
        deps.output
            .writeln_stdout(&serde_json::to_string_pretty(&payload).expect("error JSON"));
    } else {
        deps.output
            .writeln_stderr(&format!("deja {command_name} error: {scrubbed}"));
    }
    if summary_mode {
        let line = format_error_summary_line(raw_goal, redaction);
        emit_summary(&deps.output, json_mode, &line);
    }
    exit_code
}

/// Map a failed execution's advisory category to `ProductRegression` or `Failed`.
#[must_use]
pub fn failure_run_outcome(failure: Option<&WalkFailure>) -> RunOutcome {
    match failure.map(|f| f.category) {
        Some(FailureCategory::ProductAssertion) => RunOutcome::ProductRegression,
        _ => RunOutcome::Failed,
    }
}

/// Join CLI positional goal tokens and normalize via `deja_core::normalize_goal`.
pub fn parse_goal_tokens(tokens: &[String]) -> Result<(String, Goal), String> {
    let raw = tokens.join(" ");
    let goal = normalize_goal(&raw).map_err(|err| err.to_string())?;
    Ok((raw, goal))
}
