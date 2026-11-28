//! Output sinks, `CliRunReport`, full secret/path redaction, per-store report persistence
//! (`DEJA-35`), redacted JUnit emission, and exact `deja-summary v=1` formatting.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use deja_core::{
    ArtifactKind, CacheStatus, EngineReport, EngineSource, FailureCategory, ProviderKeyRedactor,
    Redactor, RunArtifact, RunAttempt, RunCommand, RunOutcome, RunResult, create_redactor,
    normalize_goal, provider_key_redactor, redact_json,
};
use deja_flow::{
    RunTimingBreakdown, SynthesizedAttemptsJunitInput, SynthesizedJunitInput, VisibleContext,
    prepare_junit, synthesized_attempts_junit, synthesized_junit, write_redacted_junit,
};
use deja_store::validate_store_id;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fs_safe::{lexical_normalize, write_atomic_confined, write_atomic_nofollow_excl};

static REPORT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Captured stdout and stderr buffers used by in-process CLI tests.
#[derive(Debug, Default, Clone)]
pub struct CapturedOutput {
    pub stdout: String,
    pub stderr: String,
}

/// Output destination for CLI stdout and stderr.
#[derive(Clone)]
pub enum OutputSink {
    Stdio,
    Captured(Arc<Mutex<CapturedOutput>>),
}

impl std::fmt::Debug for OutputSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdio => write!(f, "OutputSink::Stdio"),
            Self::Captured(_) => write!(f, "OutputSink::Captured(..)"),
        }
    }
}

impl OutputSink {
    /// Construct an in-memory capturing output sink for tests.
    #[must_use]
    pub fn captured() -> (Self, Arc<Mutex<CapturedOutput>>) {
        let buf = Arc::new(Mutex::new(CapturedOutput::default()));
        (Self::Captured(Arc::clone(&buf)), buf)
    }

    pub fn write_stdout(&self, text: &str) {
        match self {
            Self::Stdio => {
                let mut out = io::stdout().lock();
                let _ = out.write_all(text.as_bytes());
                let _ = out.flush();
            }
            Self::Captured(buf) => {
                if let Ok(mut guard) = buf.lock() {
                    guard.stdout.push_str(text);
                }
            }
        }
    }

    pub fn writeln_stdout(&self, line: &str) {
        self.write_stdout(&format!("{line}\n"));
    }

    pub fn write_stderr(&self, text: &str) {
        match self {
            Self::Stdio => {
                let mut err = io::stderr().lock();
                let _ = err.write_all(text.as_bytes());
                let _ = err.flush();
            }
            Self::Captured(buf) => {
                if let Ok(mut guard) = buf.lock() {
                    guard.stderr.push_str(text);
                }
            }
        }
    }

    pub fn writeln_stderr(&self, line: &str) {
        self.write_stderr(&format!("{line}\n"));
    }
}

/// Additive schema-1 CLI run report wrapping [`RunResult`] with runtime, timing breakdown,
/// and bounded visible error context (`DEJA-83`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliRunReport {
    #[serde(flatten)]
    pub core: RunResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<RunTimingBreakdown>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_context: Option<VisibleContext>,
}

/// Construct the canonical native Déjà engine provenance record for schema-1 reports.
#[must_use]
pub fn native_engine_report() -> EngineReport {
    EngineReport {
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        source: EngineSource::Pinned,
        override_via: None,
        in_range: true,
    }
}

/// Combined path, fixture, JSON-field, environment secret, and provider-key redactor.
#[derive(Debug, Clone)]
pub struct RedactionContext {
    pub redactor: Redactor,
    pub provider: ProviderKeyRedactor,
}

impl RedactionContext {
    /// Construct a [`RedactionContext`] from workspace root, optional home dir, configured
    /// fixture/secret values, and process environment variables.
    #[must_use]
    pub fn new(
        workspace_root: &Path,
        home_dir: Option<&Path>,
        forbidden_values: &[String],
        env_vars: &BTreeMap<String, String>,
    ) -> Self {
        let ws = workspace_root.to_string_lossy().into_owned();
        let home_owned = home_dir.map(|p| p.to_string_lossy().into_owned());
        let mut expanded_forbidden: Vec<String> = Vec::with_capacity(forbidden_values.len() * 2);
        for v in forbidden_values {
            expanded_forbidden.push(v.clone());
            let lower = v.to_lowercase();
            if lower != *v && lower.chars().count() >= 4 {
                expanded_forbidden.push(lower.clone());
            }
            let slug = slugify_for_redaction(&lower);
            if slug != lower && slug != *v && slug.chars().count() >= 4 {
                expanded_forbidden.push(slug);
            }
        }
        let redactor = create_redactor(&ws, home_owned.as_deref(), &expanded_forbidden);
        let provider =
            provider_key_redactor(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        Self { redactor, provider }
    }

    /// Redact an arbitrary text string through both configured and provider-key redactors.
    #[must_use]
    pub fn redact_str(&self, text: &str) -> String {
        self.provider.redact(&self.redactor.redact(text))
    }

    /// Derive a safe display/export `goalKey` by redacting `raw_goal` BEFORE normalizing/lowercasing,
    /// so mixed-case configured secrets never leak in lowercased form through exported `goalKey` fields.
    #[must_use]
    pub fn redact_goal_key(&self, raw_goal: &str) -> String {
        let redacted = self.redact_str(raw_goal);
        normalize_goal(&redacted)
            .map(|g| g.key)
            .unwrap_or_else(|_| redacted.trim().to_ascii_lowercase())
    }

    /// Redact a [`VisibleContext`] while preserving deduplication and truncation markers.
    #[must_use]
    pub fn redact_visible_context(&self, vc: &VisibleContext) -> VisibleContext {
        vc.map_items(|item| self.redact_str(item))
    }

    /// Recursively redact a JSON value (secret key names + workspace/home/fixture + provider patterns).
    #[must_use]
    pub fn redact_json(&self, value: &Value) -> Value {
        let first = redact_json(value, &self.redactor);
        redact_json_provider_pass(&first, &self.provider)
    }

    /// Serialize and redact a [`CliRunReport`] to a JSON value.
    pub fn redact_report(&self, report: &CliRunReport) -> Result<Value, serde_json::Error> {
        let mut raw = serde_json::to_value(report)?;
        if let Some(obj) = raw.as_object_mut() {
            let safe_key = self.redact_goal_key(&report.core.identity.goal);
            obj.insert("goalKey".to_string(), Value::String(safe_key));
        }
        Ok(self.redact_json(&raw))
    }
}

fn slugify_for_redaction(lower: &str) -> String {
    let mut slug = String::with_capacity(lower.len());
    for ch in lower.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

fn redact_json_provider_pass(value: &Value, provider: &ProviderKeyRedactor) -> Value {
    match value {
        Value::String(s) => Value::String(provider.redact(s)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| redact_json_provider_pass(item, provider))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), redact_json_provider_pass(v, provider));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Convert a path inside `workspace_root` to a clean workspace-relative `/` string when possible.
#[must_use]
pub fn workspace_relative_display(workspace_root: &Path, path: &Path) -> String {
    let norm_ws = lexical_normalize(workspace_root);
    let norm_p = lexical_normalize(path);
    if let Ok(rel) = norm_p.strip_prefix(&norm_ws) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    if let (Ok(cws), Ok(cp)) = (
        std::fs::canonicalize(workspace_root),
        std::fs::canonicalize(path),
    ) && let Ok(rel) = cp.strip_prefix(&cws)
    {
        return rel.to_string_lossy().replace('\\', "/");
    }
    norm_p.to_string_lossy().into_owned()
}

/// Paths written by [`persist_run_reports`].
#[derive(Debug, Clone)]
pub struct WrittenReports {
    pub immutable_rel_path: String,
    pub latest_rel_path: String,
    pub explicit_path: Option<PathBuf>,
}

/// Persist the redacted run report to `.deja/reports/<cmd>-<ms>-<pid>-<seq>.json`,
/// update the per-store latest report (`DEJA-35`), and optionally write `--report <path>`.
pub fn persist_run_reports(
    canonical_workspace: &Path,
    store_id: Option<&str>,
    command: RunCommand,
    redacted_json: &Value,
    explicit_report: Option<&Path>,
) -> io::Result<WrittenReports> {
    if let Some(id) = store_id {
        validate_store_id(id)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))?;
    }

    let mut payload = serde_json::to_string_pretty(redacted_json)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    payload.push('\n');
    let bytes = payload.as_bytes();

    let cmd_str = match command {
        RunCommand::Run => "run",
        RunCommand::Remember => "remember",
        RunCommand::Explore => "explore",
    };
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let pid = std::process::id();
    let seq = REPORT_SEQ.fetch_add(1, Ordering::Relaxed);

    let immutable_rel_path = format!(".deja/reports/{cmd_str}-{ms}-{pid}-{seq}.json");
    let immutable_abs = canonical_workspace.join(&immutable_rel_path);
    write_atomic_confined(canonical_workspace, &immutable_abs, bytes)?;

    let latest_rel_path = match store_id {
        Some(id) => format!(".deja/reports/stores/{id}/latest.json"),
        None => ".deja/reports/latest.json".to_string(),
    };
    let latest_abs = canonical_workspace.join(&latest_rel_path);
    write_atomic_confined(canonical_workspace, &latest_abs, bytes)?;

    let explicit_path = if let Some(path) = explicit_report {
        write_atomic_nofollow_excl(path, bytes)?;
        Some(path.to_path_buf())
    } else {
        None
    };

    Ok(WrittenReports {
        immutable_rel_path,
        latest_rel_path,
        explicit_path,
    })
}

/// Prepare a JUnit output path (refusing symlinks, removing stale file, creating parent dir).
pub fn prepare_safe_junit(path: &Path) -> io::Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to use symbolic link as JUnit path: {}",
                path.display()
            ),
        ));
    }
    prepare_junit(path)
}

/// Write a redacted JUnit XML report for `report` to `path` and attach it as an artifact.
pub fn write_report_junit(
    path: &Path,
    workspace_root: &Path,
    report: &mut CliRunReport,
    redaction: &RedactionContext,
) -> io::Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to overwrite symbolic link JUnit path: {}",
                path.display()
            ),
        ));
    }

    let xml = if let Some(ref attempts) = report.core.attempts
        && attempts.len() >= 2
    {
        let outcome_str = outcome_wire_name(report.core.outcome);
        synthesized_attempts_junit(&SynthesizedAttemptsJunitInput {
            goal: &report.core.identity.goal,
            outcome: outcome_str,
            attempts,
            duration_ms: report.core.duration_ms,
        })
    } else {
        let mut parts = Vec::new();
        parts.push(format!(
            "outcome: {}",
            outcome_wire_name(report.core.outcome)
        ));
        if let Some(ref failure) = report.core.failure {
            parts.push(format!(
                "label: {}",
                failure_category_wire(failure.category)
            ));
            if let Some(ref step) = failure.failing_step {
                parts.push(format!("step: {step}"));
            }
        }
        if let Some(ref vc) = report.visible_context {
            parts.push(vc.format_summary());
        }
        for err in &report.core.errors {
            parts.push(err.clone());
        }
        let reason = parts.join("; ");
        synthesized_junit(&SynthesizedJunitInput {
            goal: &report.core.identity.goal,
            passed: report.core.passed,
            reason: &reason,
            duration_ms: report.core.duration_ms,
        })
    };

    write_redacted_junit(path, &xml, &redaction.redactor, &redaction.provider)?;
    report.core.artifacts.push(RunArtifact {
        kind: ArtifactKind::Junit,
        path: workspace_relative_display(workspace_root, path),
        sha256: None,
    });
    Ok(())
}

/// Write a redacted fallback JUnit XML file on early CLI/config/store errors.
pub fn write_early_error_junit(
    path: &Path,
    goal: &str,
    message: &str,
    redaction: &RedactionContext,
) -> io::Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to overwrite symbolic link JUnit path: {}",
                path.display()
            ),
        ));
    }
    let xml = synthesized_junit(&SynthesizedJunitInput {
        goal,
        passed: false,
        reason: message,
        duration_ms: 0,
    });
    write_redacted_junit(path, &xml, &redaction.redactor, &redaction.provider)
}

/// Wire kebab-case name of a [`RunOutcome`].
#[must_use]
pub const fn outcome_wire_name(outcome: RunOutcome) -> &'static str {
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

/// Wire snake_case name of a [`FailureCategory`].
#[must_use]
pub const fn failure_category_wire(cat: FailureCategory) -> &'static str {
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

/// Wire kebab-case name of a [`CacheStatus`].
#[must_use]
pub const fn cache_status_wire(status: CacheStatus) -> &'static str {
    match status {
        CacheStatus::Hit => "hit",
        CacheStatus::Miss => "miss",
        CacheStatus::NotChecked => "not-checked",
    }
}

/// RFC 3986 unreserved percent-encoding used by `deja-summary v=1` (`[A-Za-z0-9._~-]`).
#[must_use]
pub fn percent_encode_summary(text: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len());
    for &b in text.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

fn format_fingerprint_12(fp: &str) -> String {
    let prefix: String = fp.chars().take(12).collect();
    if prefix.len() == 12
        && prefix
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        prefix
    } else {
        "unknown".to_string()
    }
}

fn format_engine_semver(engine: Option<&EngineReport>) -> String {
    let Some(ver) = engine.and_then(|e| e.version.as_deref()) else {
        return "unknown".to_string();
    };
    let parts: Vec<&str> = ver.split('.').collect();
    if parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        ver.to_string()
    } else {
        "unknown".to_string()
    }
}

fn sanitize_attempt_label(attempt: &RunAttempt) -> &'static str {
    if attempt.passed {
        "none"
    } else if let Some(ref failure) = attempt.failure {
        failure_category_wire(failure.category)
    } else {
        "unclassified"
    }
}

fn encode_bounded_summary_field(raw: &str, redaction: &RedactionContext) -> String {
    let redacted = redaction.redact_str(raw);
    let truncated: String = redacted.chars().take(200).collect();
    percent_encode_summary(&truncated)
}

/// Format a [`RunResult`] into the exact `deja-summary v=1` single-line grammar.
#[must_use]
pub fn format_summary_line(result: &RunResult, redaction: &RedactionContext) -> String {
    let outcome = outcome_wire_name(result.outcome);
    let cache = cache_status_wire(result.cache_status);
    let fingerprint = format_fingerprint_12(&result.identity.build_fingerprint);
    let engine = format_engine_semver(result.engine.as_ref());
    let model_calls = match result.model_calls {
        Some(n) => n.to_string(),
        None => "unknown".to_string(),
    };
    let failure = if let Some(ref f) = result.failure {
        failure_category_wire(f.category)
    } else if result.passed {
        "none"
    } else {
        "unclassified"
    };

    let mut line = format!(
        "deja-summary v=1 outcome={outcome} cache={cache} fingerprint={fingerprint} engine={engine} model_calls={model_calls} failure={failure}"
    );

    // Strictly preserve `deja-summary v=1` grammar (`attempts=2 attempt1=... attempt2=...`):
    // summarize the initial attempt (`attempts[0]`) and final rerun attempt (`attempts.last()`)
    // even when `--retries >= 2` produced > 2 attempts (all attempts stay in JSON & JUnit).
    if let Some(ref attempts) = result.attempts
        && attempts.len() >= 2
    {
        let first = &attempts[0];
        let last = &attempts[attempts.len() - 1];
        line.push_str(&format!(
            " attempts=2 attempt1={} attempt2={}",
            sanitize_attempt_label(first),
            sanitize_attempt_label(last),
        ));
        for (num, attempt) in [(1usize, first), (2usize, last)] {
            if let Some(ref fail) = attempt.failure {
                if let Some(ref step) = fail.failing_step {
                    line.push_str(&format!(
                        " attempt{num}_step={}",
                        encode_bounded_summary_field(step, redaction)
                    ));
                }
                if let Some(ref artifact) = fail.raw_artifact_path {
                    line.push_str(&format!(
                        " attempt{num}_artifact={}",
                        encode_bounded_summary_field(artifact, redaction)
                    ));
                }
            }
        }
    }

    let goal_redacted = if !result.identity.goal.trim().is_empty() {
        redaction.redact_goal_key(&result.identity.goal)
    } else {
        redaction.redact_str(&result.goal_key)
    };
    let goal_encoded = percent_encode_summary(&goal_redacted);
    line.push_str(&format!(" goal={goal_encoded}"));
    line
}

/// Format an error `deja-summary v=1` line when a command fails before producing a [`RunResult`].
#[must_use]
pub fn format_error_summary_line(raw_goal: &str, redaction: &RedactionContext) -> String {
    let normalized = normalize_goal(raw_goal)
        .map(|g| g.key)
        .unwrap_or_else(|_| raw_goal.trim().to_string());
    let goal_encoded = percent_encode_summary(&redaction.redact_str(&normalized));
    let engine = format_engine_semver(Some(&native_engine_report()));
    format!(
        "deja-summary v=1 outcome=error cache=unknown fingerprint=unknown engine={engine} model_calls=unknown failure=error goal={goal_encoded}"
    )
}

/// Emit the `deja-summary v=1` trailer to the right stream (stderr when `--json`, stdout otherwise).
pub fn emit_summary(out: &OutputSink, json_mode: bool, summary_line: &str) {
    if json_mode {
        out.writeln_stderr(summary_line);
    } else {
        out.writeln_stdout(summary_line);
    }
}

/// Print a human-readable summary of a [`CliRunReport`] (already redacted).
pub fn print_human_run_report(
    out: &OutputSink,
    report: &CliRunReport,
    redaction: &RedactionContext,
) {
    let outcome = outcome_wire_name(report.core.outcome);
    let goal = redaction.redact_str(&report.core.identity.goal);
    let fp_short = format_fingerprint_12(&report.core.identity.build_fingerprint);
    out.writeln_stdout(&format!(
        "Déjà {}: {} (outcome={}, cache={}, build={}, duration={}ms, model_calls={})",
        match report.core.command {
            RunCommand::Run => "run",
            RunCommand::Remember => "remember",
            RunCommand::Explore => "explore",
        },
        goal,
        outcome,
        cache_status_wire(report.core.cache_status),
        fp_short,
        report.core.duration_ms,
        report
            .core
            .model_calls
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
    ));
    if let Some(ref rt) = report.runtime {
        out.writeln_stdout(&format!("  runtime: {}", redaction.redact_str(rt)));
    }
    if let Some(ref yaml_path) = report.core.yaml_path {
        out.writeln_stdout(&format!("  flow: {}", redaction.redact_str(yaml_path)));
    }
    if let Some(ref timing) = report.timing {
        out.writeln_stdout(&format!(
            "  timing: fingerprint={}ms attach={}ms flow={}ms store={}ms total={}ms",
            timing.build_fingerprint_ms,
            timing.driver_attach_ms,
            timing.flow_exec_ms,
            timing.store_ms,
            timing.total_ms
        ));
    }
    if let Some(ref failure) = report.core.failure {
        out.writeln_stdout(&format!(
            "  failure: {} (step: {})",
            failure_category_wire(failure.category),
            failure
                .failing_step
                .as_deref()
                .map(|s| redaction.redact_str(s))
                .unwrap_or_else(|| "unknown".to_string())
        ));
        if let Some(ref crash) = failure.crash {
            out.writeln_stdout(&format!(
                "  crash: {:?} (process={}, report={})",
                crash.source,
                crash
                    .process
                    .as_deref()
                    .map(|s| redaction.redact_str(s))
                    .unwrap_or_else(|| "unknown".to_string()),
                crash
                    .report_file
                    .as_deref()
                    .map(|s| redaction.redact_str(s))
                    .unwrap_or_else(|| "none".to_string()),
            ));
        }
    }
    if let Some(ref vc) = report.visible_context {
        let scrubbed = redaction.redact_visible_context(vc);
        out.writeln_stdout(&format!("  {}", scrubbed.format_summary()));
    }
    for err in &report.core.errors {
        out.writeln_stderr(&format!("  error: {}", redaction.redact_str(err)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deja_core::{WALK_RESULT_SCHEMA_VERSION, WalkFailure, WalkIdentity};

    #[test]
    fn summary_line_matches_v1_grammar_and_escapes_goal() {
        let env = BTreeMap::new();
        let redaction = RedactionContext::new(Path::new("/tmp/ws"), None, &[], &env);
        let result = RunResult {
            schema_version: WALK_RESULT_SCHEMA_VERSION,
            command: RunCommand::Run,
            identity: WalkIdentity {
                goal: "Complete checkout!".to_string(),
                app_id: "dev.deja.app".to_string(),
                platform: "ios".to_string(),
                flow_schema_version: "maestro/v1".to_string(),
                build_fingerprint: "a".repeat(64),
                adapter: Some("maestro-ios/v1".to_string()),
                browser: None,
            },
            goal_key: "complete checkout!".to_string(),
            cache_status: CacheStatus::Hit,
            cache_miss_reason: None,
            semantic: None,
            outcome: RunOutcome::Passed,
            passed: true,
            started_at: "2026-01-01T00:00:00Z".to_string(),
            ended_at: "2026-01-01T00:00:01Z".to_string(),
            duration_ms: 1000,
            model_calls: Some(0),
            model_call_basis: "direct".to_string(),
            assertions: Some(3),
            repair_count: 0,
            yaml_path: Some("flows/checkout.yaml".to_string()),
            yaml_sha256: Some("b".repeat(64)),
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
        };
        let line = format_summary_line(&result, &redaction);
        assert_eq!(
            line,
            format!(
                "deja-summary v=1 outcome=passed cache=hit fingerprint=aaaaaaaaaaaa engine={} model_calls=0 failure=none goal=complete%20checkout%21",
                env!("CARGO_PKG_VERSION")
            )
        );

        let mut flaky = result.clone();
        flaky.outcome = RunOutcome::Flaky;
        flaky.passed = false;
        let fail = WalkFailure {
            category: FailureCategory::Selector,
            advisory: true,
            repair_eligible: true,
            failing_step: Some("Tap on Checkout".to_string()),
            raw_artifact_path: None,
            crash: None,
        };
        flaky.failure = Some(fail.clone());
        flaky.attempts = Some(vec![
            RunAttempt {
                attempt: 1,
                passed: false,
                started_at: "2026-01-01T00:00:00Z".to_string(),
                ended_at: "2026-01-01T00:00:01Z".to_string(),
                duration_ms: 500,
                failure: Some(fail),
                artifacts: Vec::new(),
            },
            RunAttempt {
                attempt: 2,
                passed: true,
                started_at: "2026-01-01T00:00:01Z".to_string(),
                ended_at: "2026-01-01T00:00:02Z".to_string(),
                duration_ms: 400,
                failure: None,
                artifacts: Vec::new(),
            },
        ]);
        let flaky_line = format_summary_line(&flaky, &redaction);
        assert!(flaky_line.contains("outcome=flaky"));
        assert!(flaky_line.contains("attempts=2 attempt1=selector attempt2=none"));
        assert!(flaky_line.contains("attempt1_step=Tap%20on%20Checkout"));
        assert!(flaky_line.ends_with(" goal=complete%20checkout%21"));
    }
}
