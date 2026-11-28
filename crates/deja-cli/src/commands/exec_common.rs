//! Shared flow validation, live execution, crash-check fallback, timing, and reporting helpers
//! used by `deja remember` and `deja run`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deja_core::{
    ArtifactKind, CacheMissReason, CacheStatus, DEFAULT_FLOW_SCHEMA_VERSION, ExecutionResult,
    FailureCategory, FlowClosure, FlowReferenceKind, Goal, RunArtifact, RunCommand, RunOutcome,
    RunResult, WALK_RESULT_SCHEMA_VERSION, WalkFailure, WalkIdentity, bytes_hold_forbidden_value,
    holds_forbidden_value, normalize_goal, parse_walk_manifest,
};
pub use deja_flow::validate_closure_app_id_confinement;
use deja_flow::{
    ClassificationInput, DriverCapability, FailureClassifier, FlowExecutionConfig,
    FlowExecutionOutcome, FlowExecutor, PlatformDriver, RunTimingBreakdown, VisibleContext,
    count_assertions_in_closure,
};
use deja_store::{ConsumerFlows, ConsumerFlowsOptions, now_iso8601};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::CliDeps;
use crate::commands::manifest::has_unsafe_control_or_bidi_char;
use crate::config::{DEFAULT_CONFIG_PATH, LoadedConfig};
use crate::driver_factory::DriverContext;
use crate::exit_codes::{EXIT_USAGE_OR_CONFIG, exit_code_for_outcome};
use crate::fs_safe::{
    lexical_normalize, path_inside, read_to_string_nofollow_bounded, resolve_with_nearest_ancestor,
    write_atomic_confined, write_atomic_nofollow_excl,
};
use crate::output::{
    CliRunReport, RedactionContext, emit_summary, format_cli_summary_line,
    format_error_summary_line, native_engine_report, print_human_run_report,
    workspace_relative_display, write_early_error_junit, write_report_junit,
};

const MAX_GOALS_FILE_BYTES: u64 = 4 * 1024 * 1024;
static PREALLOC_REPORT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Target identity filter used when selecting goals from a multi-platform `deja.walks.json`
/// manifest (`deja run --goals <manifest>`, `deja run --all`, `deja gate --goals <manifest>`).
#[derive(Debug, Clone, Copy)]
pub struct GoalTargetFilter<'a> {
    pub app_id: &'a str,
    pub platform: &'a str,
    pub adapter: Option<&'a str>,
    pub browser: Option<&'a str>,
}

impl<'a> GoalTargetFilter<'a> {
    /// Construct a [`GoalTargetFilter`] directly from a loaded [`LoadedConfig`].
    #[must_use]
    pub fn from_config(config: &'a LoadedConfig) -> Self {
        Self {
            app_id: &config.app_id,
            platform: &config.platform,
            adapter: config.adapter.as_deref(),
            browser: config.browser.as_deref(),
        }
    }

    /// Returns `true` when `entry` belongs to this target configuration.
    #[must_use]
    pub fn matches_manifest_walk(&self, entry: &deja_core::ManifestWalk) -> bool {
        entry.app_id == self.app_id
            && entry.platform == self.platform
            && self.adapter.is_none_or(|a| entry.adapter == a)
            && entry.browser.as_deref() == self.browser
    }
}

/// Parse a goals file shared by `deja gate --goals <FILE>` and `deja run --goals <FILE>`.
///
/// Supports plain `#`-comment text lines, a JSON array of strings, or a `deja.walks.json`
/// manifest object. Reads safely via `O_NOFOLLOW` bounded to 4 MiB and fails closed on empty
/// lists, control/bidi characters, or invalid goals.
pub fn parse_goals_file(path: &Path) -> Result<Vec<String>, String> {
    parse_goals_file_for_target(path, None)
}

/// Variant of [`parse_goals_file`] that filters `deja.walks.json` manifest entries by `target`
/// (`appId`, `platform`, `adapter`, `browser`) when `target` is `Some`.
pub fn parse_goals_file_for_target(
    path: &Path,
    target: Option<GoalTargetFilter<'_>>,
) -> Result<Vec<String>, String> {
    let raw = read_to_string_nofollow_bounded(path, MAX_GOALS_FILE_BYTES)
        .map_err(|err| format!("cannot read goals file {}: {err}", path.display()))?;
    let trimmed = raw.trim_start();
    let raw_goals: Vec<String> = if trimmed.starts_with('[') {
        let val: Value =
            serde_json::from_str(&raw).map_err(|err| format!("invalid JSON goals array: {err}"))?;
        let arr = val
            .as_array()
            .ok_or_else(|| "goals JSON must be an array of strings".to_string())?;
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            let s = item
                .as_str()
                .ok_or_else(|| "each entry in JSON goals array must be a string".to_string())?;
            out.push(s.to_string());
        }
        out
    } else if trimmed.starts_with('{') {
        let manifest = parse_walk_manifest(&raw)
            .map_err(|err| format!("goals JSON object is not a valid walk manifest: {err}"))?;
        let walks: Vec<String> = match target {
            Some(filter) => manifest
                .walks
                .into_iter()
                .filter(|w| filter.matches_manifest_walk(w))
                .map(|w| w.goal)
                .collect(),
            None => manifest.walks.into_iter().map(|w| w.goal).collect(),
        };
        if walks.is_empty() && target.is_some() {
            return Err(
                "manifest contains no walks matching this configuration's appId, platform, adapter, and browser"
                    .to_string(),
            );
        }
        walks
    } else {
        raw.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(ToString::to_string)
            .collect()
    };

    if raw_goals.is_empty() {
        return Err("goals list is empty".to_string());
    }
    for goal in &raw_goals {
        if has_unsafe_control_or_bidi_char(goal) {
            return Err(format!(
                "goal contains unsafe control, line-break, bidi, or zero-width characters: {goal:?}"
            ));
        }
        normalize_goal(goal).map_err(|err| err.to_string())?;
    }
    Ok(raw_goals)
}

/// Pre-allocate a unique batch identifier (`batch-<ms>-<pid>-<seq>`) shared between the batch
/// report filename and isolated per-goal screenshot directories.
#[must_use]
pub fn allocate_batch_id() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = PREALLOC_REPORT_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("batch-{ms}-{}-{seq}", std::process::id())
}

/// Pre-allocate a unique workspace-relative batch report path under `.deja/reports/`.
#[must_use]
pub fn allocate_batch_report_rel_path_for_id(batch_id: &str) -> String {
    format!(".deja/reports/batch-run-{batch_id}.json")
}

/// Verify that none of Déjà's implicit batch/per-store report paths (`.deja/reports/batch-latest.json`,
/// `.deja/reports/latest.json`, `.deja/reports/stores/<id>/latest.json`, or the allocated
/// immutable report / screenshot directory) collide with any protected input or explicit output
/// path (`--goals`, `--config`, manifest, `--junit`, `--report`).
pub fn validate_no_implicit_report_collision(
    workspace_root: &Path,
    store_ids: &[Option<&str>],
    immutable_batch_rel: &str,
    batch_screenshots_rel: &str,
    protected_entries: &[(&str, &Path)],
) -> Result<(), String> {
    let mut implicit_files = vec![
        lexical_normalize(&workspace_root.join(".deja/reports/batch-latest.json")),
        lexical_normalize(&workspace_root.join(immutable_batch_rel)),
    ];
    for sid in store_ids {
        let rel = match sid {
            Some(id) => format!(".deja/reports/stores/{id}/latest.json"),
            None => ".deja/reports/latest.json".to_string(),
        };
        implicit_files.push(lexical_normalize(&workspace_root.join(rel)));
    }
    let screenshots_dir = lexical_normalize(&workspace_root.join(batch_screenshots_rel));

    for &(label, raw_path) in protected_entries {
        let norm_prot = lexical_normalize(raw_path);
        let resolved_prot =
            resolve_with_nearest_ancestor(&norm_prot).unwrap_or_else(|_| norm_prot.clone());

        if norm_prot == screenshots_dir
            || path_inside(&screenshots_dir, &norm_prot)
            || resolved_prot == screenshots_dir
            || path_inside(&screenshots_dir, &resolved_prot)
        {
            return Err(format!(
                "refusing to run because {label} ({}) collides with Déjà's batch screenshot directory",
                raw_path.display()
            ));
        }

        for imp in &implicit_files {
            let resolved_imp = resolve_with_nearest_ancestor(imp).unwrap_or_else(|_| imp.clone());
            if norm_prot == *imp
                || resolved_prot == *imp
                || norm_prot == resolved_imp
                || resolved_prot == resolved_imp
            {
                return Err(format!(
                    "refusing to run because {label} ({}) collides with Déjà's implicit report file ({})",
                    raw_path.display(),
                    imp.display()
                ));
            }
        }
    }
    Ok(())
}

/// Write redacted batch report JSON to `.deja/reports/batch-run-*.json` and `.deja/reports/batch-latest.json`.
pub fn write_internal_batch_reports(
    canonical_workspace: &Path,
    immutable_rel_path: &str,
    redacted_json: &serde_json::Value,
) -> std::io::Result<()> {
    let mut text = serde_json::to_string_pretty(redacted_json)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))?;
    text.push('\n');
    let bytes = text.as_bytes();

    let immutable_abs = canonical_workspace.join(immutable_rel_path);
    write_atomic_confined(canonical_workspace, &immutable_abs, bytes)?;

    let latest_abs = canonical_workspace.join(".deja/reports/batch-latest.json");
    write_atomic_confined(canonical_workspace, &latest_abs, bytes)?;
    Ok(())
}

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

/// Write redacted report to internal workspace paths (pre-allocated immutable path + per-store latest).
pub fn write_internal_reports(
    canonical_workspace: &Path,
    store_id: Option<&str>,
    immutable_rel_path: &str,
    redacted_json: &serde_json::Value,
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

/// Reject `--report` and `--junit` (or `config.junit`) destinations that resolve to the same
/// file path (comparing both lexically normalized paths and resolved-ancestor paths) so writing
/// both outputs cannot clobber one with the other.
pub fn validate_non_overlapping_outputs(
    junit_path: Option<&Path>,
    report_path: Option<&Path>,
) -> Result<(), String> {
    let (Some(jpath), Some(rpath)) = (junit_path, report_path) else {
        return Ok(());
    };
    let norm_j = lexical_normalize(jpath);
    let norm_r = lexical_normalize(rpath);
    let res_j = resolve_with_nearest_ancestor(&norm_j).unwrap_or_else(|_| norm_j.clone());
    let res_r = resolve_with_nearest_ancestor(&norm_r).unwrap_or_else(|_| norm_r.clone());
    if norm_j == norm_r || res_j == norm_r || norm_j == res_r || res_j == res_r {
        return Err(format!(
            "refusing to write --report and --junit to the same path: {}",
            rpath.display()
        ));
    }
    Ok(())
}

/// Early safety check for explicit `--report` / `--junit` paths BEFORE `load_config` or
/// `prepare_safe_junit` touches disk, preventing accidental clobbering/deletion of config,
/// manifest, goals input files, flow closure files, symlinks, directories, or `.deja/memory/`
/// walk state.
pub fn validate_early_output_path(
    target: &Path,
    flag_label: &str,
    cwd: &Path,
    explicit_config: Option<&Path>,
    extra_flow: Option<&Path>,
    extra_protected: &[&Path],
) -> Result<(), String> {
    check_output_target_not_symlink_or_dir(target, flag_label)?;

    let config_abs = match explicit_config {
        Some(p) if p.is_absolute() => lexical_normalize(p),
        Some(p) => lexical_normalize(&cwd.join(p)),
        None => lexical_normalize(&cwd.join(DEFAULT_CONFIG_PATH)),
    };
    let config_dir = config_abs
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.to_path_buf());

    let mut protected_files = vec![config_abs.clone()];
    for extra in extra_protected {
        let extra_abs = if extra.is_absolute() {
            lexical_normalize(extra)
        } else {
            lexical_normalize(&cwd.join(extra))
        };
        protected_files.push(extra_abs);
    }
    let mut candidate_ws = fs::canonicalize(&config_dir)
        .or_else(|_| fs::canonicalize(cwd))
        .unwrap_or_else(|_| lexical_normalize(cwd));

    if let Ok(raw_cfg) = read_to_string_nofollow_bounded(&config_abs, 1024 * 1024)
        && let Ok(val) = serde_json::from_str::<Value>(&raw_cfg)
        && let Some(obj) = val.as_object()
    {
        if let Some(ws_str) = obj.get("workspaceRoot").and_then(Value::as_str)
            && !ws_str.trim().is_empty()
        {
            let ws_joined = if Path::new(ws_str).is_absolute() {
                PathBuf::from(ws_str)
            } else {
                config_dir.join(ws_str)
            };
            if let Ok(canon_ws) = fs::canonicalize(&ws_joined)
                && canon_ws.is_dir()
            {
                candidate_ws = canon_ws;
            }
        }
        if let Some(manifest_str) = obj.get("manifest").and_then(Value::as_str)
            && !manifest_str.trim().is_empty()
        {
            let manifest_abs = lexical_normalize(&candidate_ws.join(manifest_str));
            let mut manifest_set = BTreeSet::new();
            manifest_set.insert(manifest_abs.clone());
            collect_manifest_protected_paths(&candidate_ws, &manifest_abs, &mut manifest_set);
            protected_files.extend(manifest_set);
        }
    }

    if let Some(flow_arg) = extra_flow {
        let flow_abs = if flow_arg.is_absolute() {
            lexical_normalize(flow_arg)
        } else {
            lexical_normalize(&cwd.join(flow_arg))
        };
        protected_files.push(flow_abs.clone());
        if let Ok(canon_flow) = fs::canonicalize(&flow_abs)
            && let Ok(closure) = deja_core::collect_flow_closure(&candidate_ws, &canon_flow)
        {
            for file in &closure.files {
                protected_files.push(lexical_normalize(&candidate_ws.join(&file.source)));
            }
        }
    }

    check_output_target_against_protected(target, flag_label, &candidate_ws, &protected_files)
}

fn collect_manifest_protected_paths(
    workspace_root: &Path,
    manifest_abs: &Path,
    protected_files: &mut BTreeSet<PathBuf>,
) {
    // Protect every flow file (and its referenced closure) tracked in `deja.walks.json` BEFORE
    // `prepare_safe_junit` runs so neither CLI `--junit` nor `config.junit` can ever unlink a flow!
    if let Ok(raw_manifest) = read_to_string_nofollow_bounded(manifest_abs, 16 * 1024 * 1024)
        && let Ok(manifest) = deja_core::parse_walk_manifest(&raw_manifest)
    {
        for entry in &manifest.walks {
            let flow_abs = lexical_normalize(&workspace_root.join(&entry.flow));
            protected_files.insert(flow_abs.clone());
            if let Ok(canon_flow) = fs::canonicalize(&flow_abs)
                && let Ok(closure) = deja_core::collect_flow_closure(workspace_root, &canon_flow)
            {
                for file in &closure.files {
                    protected_files.insert(lexical_normalize(&workspace_root.join(&file.source)));
                }
            }
        }
    }
}

/// Collect all protected paths implied by `config.manifest` (the manifest file itself plus every
/// entry's flow file and transitive closure files) ONCE so multi-goal batch runs never rescan
/// every manifest closure per goal.
#[must_use]
pub fn collect_config_manifest_protected_paths(config: &LoadedConfig) -> BTreeSet<PathBuf> {
    let mut protected = BTreeSet::new();
    if let Some(ref manifest_rel) = config.manifest {
        let manifest_abs = lexical_normalize(&config.workspace_root.join(manifest_rel));
        protected.insert(manifest_abs.clone());
        collect_manifest_protected_paths(&config.workspace_root, &manifest_abs, &mut protected);
    }
    protected
}

/// Full walk-state protection check reusing a precomputed set of manifest-protected paths
/// (avoiding `O(N^2)` manifest/closure rescans across multi-goal batches).
pub fn validate_explicit_output_path_with_precomputed_manifest(
    target: &Path,
    flag_label: &str,
    config: &LoadedConfig,
    closure: Option<&FlowClosure>,
    precomputed_manifest_protected: &BTreeSet<PathBuf>,
    extra_protected: &[&Path],
) -> Result<(), String> {
    check_output_target_not_symlink_or_dir(target, flag_label)?;

    let mut protected_files = Vec::with_capacity(
        1 + precomputed_manifest_protected.len()
            + closure.map_or(0, |c| c.files.len())
            + extra_protected.len(),
    );
    protected_files.push(config.config_path.clone());
    protected_files.extend(precomputed_manifest_protected.iter().cloned());
    if let Some(c) = closure {
        for file in &c.files {
            protected_files.push(lexical_normalize(&config.workspace_root.join(&file.source)));
        }
    }
    for extra in extra_protected {
        let extra_abs = if extra.is_absolute() {
            lexical_normalize(extra)
        } else {
            lexical_normalize(&config.workspace_root.join(extra))
        };
        protected_files.push(extra_abs);
    }

    check_output_target_against_protected(
        target,
        flag_label,
        &config.workspace_root,
        &protected_files,
    )
}

/// Full walk-state protection check for explicit `--report` / `--junit` (and `config.junit`)
/// destinations once config (and optionally the active [`FlowClosure`]) have been resolved.
pub fn validate_explicit_output_path(
    target: &Path,
    flag_label: &str,
    config: &LoadedConfig,
    closure: Option<&FlowClosure>,
    extra_protected: &[&Path],
) -> Result<(), String> {
    let manifest_protected = collect_config_manifest_protected_paths(config);
    validate_explicit_output_path_with_precomputed_manifest(
        target,
        flag_label,
        config,
        closure,
        &manifest_protected,
        extra_protected,
    )
}

fn check_output_target_not_symlink_or_dir(target: &Path, flag_label: &str) -> Result<(), String> {
    if let Ok(meta) = fs::symlink_metadata(target) {
        if meta.file_type().is_symlink() {
            return Err(format!(
                "refusing to write {flag_label} to symbolic link: {}",
                target.display()
            ));
        }
        if !meta.file_type().is_file() {
            return Err(format!(
                "refusing to write {flag_label} to non-regular file or directory: {}",
                target.display()
            ));
        }
    }
    Ok(())
}

fn check_output_target_against_protected(
    target: &Path,
    flag_label: &str,
    canonical_workspace: &Path,
    protected_files: &[PathBuf],
) -> Result<(), String> {
    let norm_target = lexical_normalize(target);
    let resolved_target =
        resolve_with_nearest_ancestor(&norm_target).unwrap_or_else(|_| norm_target.clone());

    let memory_dir = lexical_normalize(&canonical_workspace.join(".deja/memory"));
    let flows_dir = lexical_normalize(&canonical_workspace.join(".deja/flows"));
    let manifest_lock = lexical_normalize(&canonical_workspace.join(".deja/manifest.lock"));
    let legacy_walks = lexical_normalize(&canonical_workspace.join("walks.json"));
    let batch_latest =
        lexical_normalize(&canonical_workspace.join(".deja/reports/batch-latest.json"));
    let default_latest = lexical_normalize(&canonical_workspace.join(".deja/reports/latest.json"));
    let store_reports_dir = lexical_normalize(&canonical_workspace.join(".deja/reports/stores"));

    for candidate in [&norm_target, &resolved_target] {
        if candidate == &memory_dir || path_inside(&memory_dir, candidate) {
            return Err(format!(
                "refusing to write {flag_label} inside Déjà walk memory (.deja/memory/): {}",
                target.display()
            ));
        }
        if candidate == &flows_dir || path_inside(&flows_dir, candidate) {
            return Err(format!(
                "refusing to write {flag_label} inside Déjà stored flows (.deja/flows/): {}",
                target.display()
            ));
        }
        if candidate == &manifest_lock || candidate == &legacy_walks {
            return Err(format!(
                "refusing to overwrite protected Déjà walk state with {flag_label}: {}",
                target.display()
            ));
        }
        if candidate == &batch_latest
            || candidate == &default_latest
            || (path_inside(&store_reports_dir, candidate)
                && candidate.file_name().and_then(|s| s.to_str()) == Some("latest.json"))
        {
            return Err(format!(
                "refusing to write {flag_label} to Déjà's implicit report path: {}",
                target.display()
            ));
        }
    }

    for protected in protected_files {
        let norm_prot = lexical_normalize(protected);
        let resolved_prot =
            resolve_with_nearest_ancestor(&norm_prot).unwrap_or_else(|_| norm_prot.clone());
        if norm_target == norm_prot
            || resolved_target == norm_prot
            || norm_target == resolved_prot
            || resolved_target == resolved_prot
        {
            return Err(format!(
                "refusing to overwrite protected workspace file with {flag_label}: {}",
                target.display()
            ));
        }
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
    expected_build_fingerprint: Option<String>,
) -> DriverContext {
    DriverContext {
        platform: config.platform.clone(),
        device: config.device.clone().unwrap_or_default(),
        app_id: config.app_id.clone(),
        build_path: config.build_path.clone().unwrap_or_default(),
        expected_build_fingerprint,
        workspace_root: config.workspace_root.clone(),
        disable_animations: config.disable_animations,
        crash_report_grace_ms: config.crash_report_grace_ms,
        crash_report_dir: config.crash_report_directory.clone(),
        deja_home: deps
            .env
            .get("DEJA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from),
        headless: config.headless,
        screen_size: config.screen_size.clone(),
        timeout_ms,
        env: deps.env.clone(),
        staged_build: None,
    }
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

/// Execute a single flow closure on an already-attached `driver` exclusively from the
/// preflight-validated in-memory `closure` bytes (never reopening flow/subflow files from disk),
/// and run the post-executor `.ips` crash-report fallback ONLY when [`FlowExecutor`] did not
/// already complete a full crash probe (`DEJA-46`, `DEJA-52`).
pub async fn execute_single_attempt(
    driver: &dyn PlatformDriver,
    config: &LoadedConfig,
    closure: &FlowClosure,
    timeout_ms: u64,
    redaction: &RedactionContext,
    cancel: &CancellationToken,
) -> ExecutedAttempt {
    execute_single_attempt_with_screenshot_dir(
        driver, config, closure, timeout_ms, None, redaction, cancel,
    )
    .await
}

/// Variant of [`execute_single_attempt`] allowing batch runs to isolate `takeScreenshot` paths
/// per goal and attempt via [`FlowExecutionConfig::screenshot_dir_override`].
pub async fn execute_single_attempt_with_screenshot_dir(
    driver: &dyn PlatformDriver,
    config: &LoadedConfig,
    closure: &FlowClosure,
    timeout_ms: u64,
    screenshot_dir_override: Option<&Path>,
    redaction: &RedactionContext,
    cancel: &CancellationToken,
) -> ExecutedAttempt {
    let started_at = now_iso8601();
    let started_epoch_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let t0 = Instant::now();

    let mut exec_cfg =
        FlowExecutionConfig::new(config.workspace_root.clone()).with_closure_snapshot(closure);
    exec_cfg.app_id_override = Some(config.app_id.clone());
    exec_cfg.env = config.env.clone();
    exec_cfg.flow_timeout = Some(Duration::from_millis(timeout_ms));
    exec_cfg.screenshot_dir_override = screenshot_dir_override.map(Path::to_path_buf);
    exec_cfg.redactor = redaction.redactor.clone();
    exec_cfg.provider_redactor = redaction.provider.clone();

    let main_source = closure
        .files
        .iter()
        .find(|f| f.path == closure.main)
        .map(|f| f.source.as_str())
        .unwrap_or(closure.main.as_str());
    let main_path = config.workspace_root.join(main_source);

    let executor = FlowExecutor::new(exec_cfg);
    let mut outcome: FlowExecutionOutcome = executor.execute_file(driver, &main_path, cancel).await;

    // Post-executor crash check fallback: ONLY run when `FlowExecutor` did not already finish
    // a full `driver.check_app_crash` probe (e.g., if `flow_timeout` truncated the in-flow probe).
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
        selection_reason: None,
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
    let mut explicit_artifact_failed = false;

    if let Some(jpath) = junit_path
        && let Err(err) = write_report_junit(jpath, workspace_root, report, redaction)
    {
        let msg = redaction.redact_str(&format!("failed to write JUnit report: {err}"));
        report.core.errors.push(msg.clone());
        deps.output.writeln_stderr(&format!("deja error: {msg}"));
        explicit_artifact_failed = true;
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

    if let Err(err) =
        write_internal_reports(workspace_root, store_id, immutable_rel_path, &redacted_json)
    {
        deps.output.writeln_stderr(&redaction.redact_str(&format!(
            "deja warning: failed to write run report artifact: {err}"
        )));
    }

    if let Some(path) = explicit_report {
        let mut text =
            serde_json::to_string_pretty(&redacted_json).expect("redacted JSON serializes");
        text.push('\n');
        if let Err(err) = write_atomic_nofollow_excl(path, text.as_bytes()) {
            let msg = redaction.redact_str(&format!(
                "failed to write explicit --report {}: {err}",
                path.display()
            ));
            deps.output.writeln_stderr(&format!("deja error: {msg}"));
            explicit_artifact_failed = true;
        }
    }

    if json_mode {
        deps.output.writeln_stdout(
            &serde_json::to_string_pretty(&redacted_json).expect("redacted JSON serializes"),
        );
    } else {
        print_human_run_report(&deps.output, report, redaction);
    }

    if summary_mode {
        let line = format_cli_summary_line(report, redaction);
        emit_summary(&deps.output, json_mode, &line);
    }

    if explicit_artifact_failed {
        EXIT_USAGE_OR_CONFIG
    } else {
        exit_code_for_outcome(report.core.outcome)
    }
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{CommandCapability, LoadConfigOptions, load_config};
    use deja_core::{ManifestWalk, WalkManifest, collect_flow_closure, serialize_walk_manifest};

    #[test]
    fn precomputed_manifest_protection_never_rescans_manifest_per_goal() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "deja-exec-common-precomp-{}-{nanos:x}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("flows")).unwrap();
        fs::write(root.join("build.bin"), b"build-v1").unwrap();
        fs::write(
            root.join("flows/sub.yaml"),
            "appId: com.example.app\n---\n- assertVisible: Sub\n",
        )
        .unwrap();
        fs::write(
            root.join("flows/g1.yaml"),
            "appId: com.example.app\n---\n- runFlow: sub.yaml\n",
        )
        .unwrap();
        fs::write(
            root.join("flows/g2.yaml"),
            "appId: com.example.app\n---\n- assertVisible: G2\n",
        )
        .unwrap();
        fs::write(
            root.join("flows/active_only.yaml"),
            "appId: com.example.app\n---\n- assertVisible: Active\n",
        )
        .unwrap();

        let c1 = collect_flow_closure(&root, &root.join("flows/g1.yaml")).unwrap();
        let c2 = collect_flow_closure(&root, &root.join("flows/g2.yaml")).unwrap();
        let manifest = WalkManifest {
            manifest_version: deja_core::WALK_MANIFEST_VERSION,
            hash_version: deja_core::MANIFEST_HASH_VERSION,
            walks: vec![
                ManifestWalk {
                    goal: "g1".to_string(),
                    app_id: "com.example.app".to_string(),
                    platform: "ios".to_string(),
                    adapter: "maestro-ios/v1".to_string(),
                    browser: None,
                    flow: "flows/g1.yaml".to_string(),
                    closure_sha256: c1.sha256.clone(),
                    assertions_sha256: deja_core::assertions_sha256(&c1).unwrap(),
                    owner_rules: None,
                    profile: None,
                },
                ManifestWalk {
                    goal: "g2".to_string(),
                    app_id: "com.example.app".to_string(),
                    platform: "ios".to_string(),
                    adapter: "maestro-ios/v1".to_string(),
                    browser: None,
                    flow: "flows/g2.yaml".to_string(),
                    closure_sha256: c2.sha256.clone(),
                    assertions_sha256: deja_core::assertions_sha256(&c2).unwrap(),
                    owner_rules: None,
                    profile: None,
                },
            ],
        };
        let manifest_path = root.join("deja.walks.json");
        fs::write(&manifest_path, serialize_walk_manifest(&manifest).unwrap()).unwrap();
        fs::write(
            root.join("deja.config.json"),
            json!({
                "workspaceRoot": ".",
                "platform": "ios",
                "appId": "com.example.app",
                "adapter": "maestro-ios/v1",
                "device": "00000000-0000-4000-8000-000000000001",
                "buildPath": "build.bin",
                "manifest": "deja.walks.json"
            })
            .to_string(),
        )
        .unwrap();

        let config = load_config(&LoadConfigOptions {
            cwd: &root,
            explicit_config: None,
            capability: CommandCapability::Execute,
            device_override: None,
            build_override: None,
            env: &BTreeMap::new(),
        })
        .unwrap();

        let precomputed = collect_config_manifest_protected_paths(&config);
        assert!(precomputed.contains(&lexical_normalize(
            &config.workspace_root.join("flows/sub.yaml")
        )));
        assert!(precomputed.contains(&lexical_normalize(
            &config.workspace_root.join("flows/g2.yaml")
        )));

        // Remove `deja.walks.json` from disk to prove per-goal validation reuses `precomputed`
        // without re-reading or rescanning the manifest on disk!
        fs::remove_file(&manifest_path).unwrap();

        let active_closure = collect_flow_closure(
            &config.workspace_root,
            &config.workspace_root.join("flows/active_only.yaml"),
        )
        .unwrap();

        // Still refuses overwriting manifest subflow (`flows/sub.yaml`) and sibling flow (`flows/g2.yaml`)!
        assert!(
            validate_explicit_output_path_with_precomputed_manifest(
                &config.workspace_root.join("flows/sub.yaml"),
                "--junit",
                &config,
                Some(&active_closure),
                &precomputed,
                &[],
            )
            .is_err()
        );
        assert!(
            validate_explicit_output_path_with_precomputed_manifest(
                &config.workspace_root.join("flows/g2.yaml"),
                "--report",
                &config,
                Some(&active_closure),
                &precomputed,
                &[],
            )
            .is_err()
        );
        // Also refuses overwriting the active goal's closure file (`flows/active_only.yaml`)!
        assert!(
            validate_explicit_output_path_with_precomputed_manifest(
                &config.workspace_root.join("flows/active_only.yaml"),
                "--junit",
                &config,
                Some(&active_closure),
                &precomputed,
                &[],
            )
            .is_err()
        );
        // Safe output path outside protected files succeeds!
        assert!(
            validate_explicit_output_path_with_precomputed_manifest(
                &config.workspace_root.join("out/junit.xml"),
                "--junit",
                &config,
                Some(&active_closure),
                &precomputed,
                &[],
            )
            .is_ok()
        );

        let _ = fs::remove_dir_all(&root);
    }
}
