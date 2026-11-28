//! Multi-goal batch execution (`deja run --goals <FILE>` / `deja run --all`), dynamic
//! `--devices` LPT sharding, single build-fingerprint broadcast (`DEJA-53`), shared staged `.app`
//! reuse, persistent per-shard driver reuse, merged redacted JUnit output, and graceful signal
//! cancellation (`DEJA-66`).

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use deja_core::{
    ArtifactKind, CacheMissReason, CoreError, Goal, RunArtifact, RunAttempt, RunCommand,
    RunOutcome, WALK_RESULT_SCHEMA_VERSION, assertions_sha256, normalize_goal,
};
use deja_driver_ios::validate_udid;
use deja_flow::{
    BatchJunitAttemptSummary, BatchJunitCase, BatchJunitCaseStatus, PlatformDriver,
    RunTimingBreakdown, count_assertions_in_closure,
};
use deja_store::{
    ConsumerFlows, PostRunValidationOptions, RecordValidationInput, SaveManifestWalkOptions,
    SqliteStoreOptions, SqliteWalkStore, StoreError, UpsertWalkInput, now_iso8601,
    validate_store_id,
};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard, OnceCell, watch};
use tokio_util::sync::CancellationToken;

use crate::CliDeps;
use crate::cli::RunArgs;
use crate::commands::changed_from::{ChangedFromError, resolve_and_select_goals};
use crate::commands::exec_common::{
    ExecutedAttempt, GoalTargetFilter, allocate_batch_id, allocate_batch_report_rel_path_for_id,
    allocate_report_rel_path, build_base_report, build_consumer_flows, build_driver_context,
    check_closure_forbidden_values, collect_config_manifest_protected_paths, emit_early_exec_error,
    execute_single_attempt_with_screenshot_dir, failure_run_outcome, make_walk_identity,
    parse_goals_file_for_target, to_execution_result, validate_closure_app_id_confinement,
    validate_explicit_output_path_with_precomputed_manifest, validate_no_implicit_report_collision,
    validate_non_overlapping_outputs, write_internal_batch_reports, write_internal_reports,
};
use crate::commands::manifest::has_unsafe_control_or_bidi_char;
use crate::commands::run::{
    ExecMode, RunPlan, failure_signature, plan_execution, verify_unchanged_closure,
};
use crate::config::{CommandCapability, LoadConfigOptions, LoadedConfig, load_config};
use crate::driver_factory::{DriverContext, DriverFactoryError, SharedStagedBuild};
use crate::exit_codes::{
    EXIT_EXPLORATION_REJECTED, EXIT_FAILURE, EXIT_FLAKY, EXIT_INVALID_MEMORY, EXIT_MANIFEST_DRIFT,
    EXIT_OK, EXIT_STORE_LOCKED, EXIT_USAGE_OR_CONFIG, exit_code_for_cancel_signal,
    exit_code_for_outcome, exit_code_for_store_error,
};
use crate::fs_safe::write_atomic_nofollow_excl;
use crate::output::{
    BatchCounts, BatchGoalResult, BatchSelectionSummary, BatchShardSummary, CliBatchRunReport,
    CliRunReport, RedactionContext, SelectionReason, emit_summary, failure_category_wire,
    format_batch_status_summary_line_with_selection, format_cli_summary_line, outcome_wire_name,
    prepare_safe_junit, print_human_batch_report, workspace_relative_display, write_batch_junit,
};

#[derive(Debug, Clone)]
struct ShardSpec {
    device: String,
    store_id: Option<String>,
}

struct OpenedShard {
    orig_idx: usize,
    device: String,
    store_id: Option<String>,
    store_label: String,
    store: Arc<SqliteWalkStore>,
    runtime: Option<String>,
    base_driver_ctx: DriverContext,
    goals_executed: Arc<AtomicUsize>,
}

#[derive(Debug, Clone)]
struct BatchGoal {
    index: usize,
    goal: Goal,
    historical_median_ms: Option<u64>,
    /// Selection reason when `--changed-from` is active (`None` for full unselected batches).
    selection_reason: Option<SelectionReason>,
    /// Strict non-empty subset of shard indices eligible to execute this goal (`None` means all
    /// shards are eligible, or no shard is eligible so any shard may emit the refusal/error).
    eligible_shards: Option<BTreeSet<usize>>,
}

impl BatchGoal {
    fn is_eligible_for(&self, shard_idx: usize) -> bool {
        self.eligible_shards
            .as_ref()
            .is_none_or(|allowed| allowed.contains(&shard_idx))
    }
}

#[derive(Debug, Clone)]
struct BatchGoalOutcomeRecord {
    result: BatchGoalResult,
    junit_case: BatchJunitCase,
}

/// Convert a device identifier into a deterministic, valid per-device isolated `--store` ID.
///
/// Uses the raw device ID directly when it already satisfies [`validate_store_id`] (such as
/// canonical iOS simulator UDIDs or simple shard labels like `d1`), otherwise builds a safe
/// ASCII prefix plus `-{8-hex SHA-256 of device}` (`<= 128` bytes, guaranteed valid).
#[must_use]
pub fn device_to_store_id(device: &str) -> String {
    if validate_store_id(device).is_ok() {
        return device.to_string();
    }
    let digest = hex::encode(Sha256::digest(device.as_bytes()));
    let suffix = &digest[..8];
    let mut prefix = String::with_capacity(device.len());
    for ch in device.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            prefix.push(ch);
        } else if !prefix.ends_with('-') && !prefix.is_empty() {
            prefix.push('-');
        }
    }
    let trimmed_start = prefix.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    let trimmed = trimmed_start.trim_end_matches('-');
    let base = if trimmed.is_empty() {
        "device"
    } else {
        trimmed
    };
    let max_base_len = 128 - 9; // reserve `-{8-hex}`
    let truncated: String = base.chars().take(max_base_len).collect();
    let candidate = format!("{truncated}-{suffix}");
    debug_assert!(validate_store_id(&candidate).is_ok());
    candidate
}

/// Compute the exact prioritized exit code for a batch run:
/// `143` (SIGTERM) / `130` (SIGINT) / `1` (token-cancel) >
/// `2` (explicit artifact write failure) >
/// `4` (corrupt) >
/// `5` (lock) >
/// `2` (usage-closure) >
/// `1` (fail) >
/// `3` (explore-rejected) >
/// `6` (drift) >
/// `7` (flaky-only) >
/// `0` (all passed).
#[must_use]
pub fn compute_batch_exit_code(
    cancelled: bool,
    cancel_signal: u8,
    explicit_artifact_failed: bool,
    goal_exit_codes: &[u8],
) -> u8 {
    if cancelled {
        return exit_code_for_cancel_signal(cancel_signal);
    }
    if explicit_artifact_failed {
        return EXIT_USAGE_OR_CONFIG;
    }
    if goal_exit_codes.contains(&EXIT_INVALID_MEMORY) {
        return EXIT_INVALID_MEMORY;
    }
    if goal_exit_codes.contains(&EXIT_STORE_LOCKED) {
        return EXIT_STORE_LOCKED;
    }
    if goal_exit_codes.contains(&EXIT_USAGE_OR_CONFIG) {
        return EXIT_USAGE_OR_CONFIG;
    }
    if goal_exit_codes.contains(&EXIT_FAILURE) {
        return EXIT_FAILURE;
    }
    if goal_exit_codes.contains(&EXIT_EXPLORATION_REJECTED) {
        return EXIT_EXPLORATION_REJECTED;
    }
    if goal_exit_codes.contains(&EXIT_MANIFEST_DRIFT) {
        return EXIT_MANIFEST_DRIFT;
    }
    if goal_exit_codes.contains(&EXIT_FLAKY) {
        return EXIT_FLAKY;
    }
    if goal_exit_codes.iter().any(|&c| c != EXIT_OK) {
        return EXIT_FAILURE;
    }
    EXIT_OK
}

fn integer_median(values: &mut [u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let n = values.len();
    if n % 2 == 1 {
        Some(values[n / 2])
    } else {
        Some(((u128::from(values[n / 2 - 1]) + u128::from(values[n / 2])) / 2) as u64)
    }
}

fn close_all_stores(stores: &[Arc<SqliteWalkStore>]) {
    for store in stores {
        store.close();
    }
}

async fn scan_duration_medians_cancellable(
    opened_stores: &[Arc<SqliteWalkStore>],
    cancel: &CancellationToken,
) -> Result<BTreeMap<String, u64>, Option<StoreError>> {
    if cancel.is_cancelled() {
        return Err(None);
    }
    let stores_for_scan: Vec<Arc<SqliteWalkStore>> = opened_stores.to_vec();
    let dur_cancel_flag = Arc::new(AtomicBool::new(false));
    let dur_worker_flag = Arc::clone(&dur_cancel_flag);
    let mut dur_handle = tokio::task::spawn_blocking(move || {
        let mut pooled: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for store in &stores_for_scan {
            if dur_worker_flag.load(Ordering::Relaxed) {
                return Err(StoreError::Cancelled);
            }
            let map = store.goal_duration_samples_ms_cancellable(&dur_worker_flag)?;
            for (goal_key, samples) in map {
                pooled.entry(goal_key).or_default().extend(samples);
            }
        }
        Ok(pooled)
    });
    let pooled = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            dur_cancel_flag.store(true, Ordering::SeqCst);
            let _ = dur_handle.await;
            return Err(None);
        }
        res = &mut dur_handle => match res {
            Ok(Ok(pooled)) => pooled,
            Ok(Err(StoreError::Cancelled)) => return Err(None),
            Ok(Err(err)) => {
                if cancel.is_cancelled() {
                    return Err(None);
                }
                return Err(Some(err));
            }
            Err(join_err) => {
                return Err(Some(StoreError::Io(std::io::Error::other(format!(
                    "duration history scan task failed: {join_err}"
                )))));
            }
        },
    };
    let mut medians = BTreeMap::new();
    for (goal_key, mut vals) in pooled {
        if let Some(med) = integer_median(&mut vals) {
            medians.insert(goal_key, med);
        }
    }
    Ok(medians)
}

/// Entrypoint for `deja run --goals <FILE>` and `deja run --all`.
pub async fn run_batch(
    args: &RunArgs,
    explicit_config: Option<&Path>,
    json_mode: bool,
    deps: &CliDeps,
    goals_abs: Option<PathBuf>,
    cli_junit: Option<PathBuf>,
    explicit_report: Option<PathBuf>,
) -> u8 {
    run_batch_inner(
        args,
        explicit_config,
        json_mode,
        deps,
        goals_abs,
        cli_junit,
        explicit_report,
        Arc::new(AsyncMutex::new(())),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_batch_inner(
    args: &RunArgs,
    explicit_config: Option<&Path>,
    json_mode: bool,
    deps: &CliDeps,
    goals_abs: Option<PathBuf>,
    cli_junit: Option<PathBuf>,
    explicit_report: Option<PathBuf>,
    manifest_mutex: Arc<AsyncMutex<()>>,
) -> u8 {
    let early_redaction =
        RedactionContext::new(&deps.cwd, deps.home_dir.as_deref(), &[], &deps.env);
    let first_devices_override: Option<String> = args.devices.first().map(|d| d.trim().to_string());
    let device_override_for_config = if !args.devices.is_empty() {
        first_devices_override.as_deref()
    } else {
        args.device.as_deref()
    };

    let config = match load_config(&LoadConfigOptions {
        cwd: &deps.cwd,
        explicit_config,
        capability: CommandCapability::Execute,
        device_override: device_override_for_config,
        build_override: None,
        env: &deps.env,
    }) {
        Ok(cfg) => Arc::new(cfg),
        Err(err) => {
            return emit_early_exec_error(
                deps,
                "run",
                "batch",
                cli_junit.as_deref(),
                json_mode,
                args.summary,
                &early_redaction,
                "CONFIG_ERROR",
                &err.to_string(),
                EXIT_USAGE_OR_CONFIG,
            );
        }
    };

    for w in &config.warnings {
        deps.output.writeln_stderr(w);
    }

    let redaction = Arc::new(RedactionContext::new(
        &config.workspace_root,
        deps.home_dir.as_deref(),
        &config.forbidden_values,
        &deps.env,
    ));

    let extra_protected_owned: Vec<PathBuf> = goals_abs.iter().cloned().collect();
    let extra_protected_refs: Vec<&Path> =
        extra_protected_owned.iter().map(PathBuf::as_path).collect();

    let effective_junit: Option<PathBuf> = cli_junit.clone().or_else(|| config.junit.clone());
    if let Err(msg) =
        validate_non_overlapping_outputs(effective_junit.as_deref(), explicit_report.as_deref())
    {
        return emit_early_exec_error(
            deps,
            "run",
            "batch",
            None,
            json_mode,
            args.summary,
            &redaction,
            "OUTPUT_PATH_COLLISION",
            &msg,
            EXIT_USAGE_OR_CONFIG,
        );
    }

    // Collect manifest-wide protected paths ONCE for the entire batch (preflight + every worker).
    let manifest_protected = Arc::new(collect_config_manifest_protected_paths(&config));

    if let Some(ref jpath) = effective_junit
        && let Err(msg) = validate_explicit_output_path_with_precomputed_manifest(
            jpath,
            "--junit",
            &config,
            None,
            &manifest_protected,
            &extra_protected_refs,
        )
    {
        return emit_early_exec_error(
            deps,
            "run",
            "batch",
            None,
            json_mode,
            args.summary,
            &redaction,
            "JUNIT_ERROR",
            &msg,
            EXIT_USAGE_OR_CONFIG,
        );
    }
    if let Some(ref rpath) = explicit_report
        && let Err(msg) = validate_explicit_output_path_with_precomputed_manifest(
            rpath,
            "--report",
            &config,
            None,
            &manifest_protected,
            &extra_protected_refs,
        )
    {
        return emit_early_exec_error(
            deps,
            "run",
            "batch",
            cli_junit.as_deref(),
            json_mode,
            args.summary,
            &redaction,
            "REPORT_PATH_REFUSED",
            &msg,
            EXIT_USAGE_OR_CONFIG,
        );
    }

    // Resolve shard device list and per-device isolated store IDs (pure validation, no disk I/O)
    // before checking implicit report collisions or preparing `config.junit`.
    let shard_specs = match resolve_shard_specs(args, &config) {
        Ok(specs) => specs,
        Err(msg) => {
            return emit_early_exec_error(
                deps,
                "run",
                "batch",
                cli_junit.as_deref(),
                json_mode,
                args.summary,
                &redaction,
                "INVALID_DEVICES",
                &msg,
                EXIT_USAGE_OR_CONFIG,
            );
        }
    };

    let batch_id = allocate_batch_id();
    let immutable_batch_rel = allocate_batch_report_rel_path_for_id(&batch_id);
    let batch_screenshots_rel = format!(".deja/reports/batch-screenshots/{batch_id}");

    // Ensure Déjà's implicit batch/per-store latest reports and screenshot folder never overwrite
    // `--goals`, config, manifest, `--junit`, or `--report` BEFORE touching disk!
    let manifest_abs_opt = config
        .manifest
        .as_ref()
        .map(|m| config.workspace_root.join(m));
    let mut implicit_protected: Vec<(&str, &Path)> =
        vec![("--config", config.config_path.as_path())];
    if let Some(ref gpath) = goals_abs {
        implicit_protected.push(("--goals", gpath.as_path()));
    }
    if let Some(ref mpath) = manifest_abs_opt {
        implicit_protected.push(("manifest", mpath.as_path()));
    }
    if let Some(ref jpath) = effective_junit {
        implicit_protected.push(("--junit", jpath.as_path()));
    }
    if let Some(ref rpath) = explicit_report {
        implicit_protected.push(("--report", rpath.as_path()));
    }
    let shard_store_id_refs: Vec<Option<&str>> =
        shard_specs.iter().map(|s| s.store_id.as_deref()).collect();
    if let Err(msg) = validate_no_implicit_report_collision(
        &config.workspace_root,
        &shard_store_id_refs,
        &immutable_batch_rel,
        &batch_screenshots_rel,
        &implicit_protected,
    ) {
        return emit_early_exec_error(
            deps,
            "run",
            "batch",
            None,
            json_mode,
            args.summary,
            &redaction,
            "IMPLICIT_REPORT_COLLISION",
            &msg,
            EXIT_USAGE_OR_CONFIG,
        );
    }

    if let Some(ref jpath) = effective_junit
        && cli_junit.is_none()
        && let Err(err) = prepare_safe_junit(jpath)
    {
        return emit_early_exec_error(
            deps,
            "run",
            "batch",
            None,
            json_mode,
            args.summary,
            &redaction,
            "JUNIT_ERROR",
            &format!("cannot prepare JUnit path {}: {err}", jpath.display()),
            EXIT_USAGE_OR_CONFIG,
        );
    }

    if args.verify && config.manifest.is_none() {
        return emit_early_exec_error(
            deps,
            "run",
            "batch",
            effective_junit.as_deref(),
            json_mode,
            args.summary,
            &redaction,
            "USAGE_ERROR",
            "`--verify` requires `manifest` to be configured in deja.config.json",
            EXIT_USAGE_OR_CONFIG,
        );
    }

    if args.all && config.manifest.is_none() {
        return emit_early_exec_error(
            deps,
            "run",
            "batch",
            effective_junit.as_deref(),
            json_mode,
            args.summary,
            &redaction,
            "USAGE_ERROR",
            "`--all` requires `manifest` to be configured in deja.config.json",
            EXIT_USAGE_OR_CONFIG,
        );
    }

    let consumer: Option<Arc<ConsumerFlows>> = match build_consumer_flows(&config) {
        Ok(c) => c.map(Arc::new),
        Err(msg) => {
            return emit_early_exec_error(
                deps,
                "run",
                "batch",
                effective_junit.as_deref(),
                json_mode,
                args.summary,
                &redaction,
                "INVALID_MANIFEST",
                &msg,
                EXIT_USAGE_OR_CONFIG,
            );
        }
    };

    // Load & validate batch goals (from `--goals <FILE>` or filtered manifest `--all`),
    // filtering manifest entries by `(appId, platform, adapter, browser)` in BOTH modes.
    let target_filter = GoalTargetFilter::from_config(&config);
    let raw_goals = if let Some(ref gpath) = goals_abs {
        match parse_goals_file_for_target(gpath, Some(target_filter)) {
            Ok(list) => list,
            Err(msg) => {
                return emit_early_exec_error(
                    deps,
                    "run",
                    "batch",
                    effective_junit.as_deref(),
                    json_mode,
                    args.summary,
                    &redaction,
                    "INVALID_GOALS",
                    &msg,
                    EXIT_USAGE_OR_CONFIG,
                );
            }
        }
    } else {
        let cf = consumer.as_deref().expect("--all requires manifest");
        let manifest = match cf.load() {
            Ok(m) => m,
            Err(err) => {
                let code = exit_code_for_store_error(&err);
                return emit_early_exec_error(
                    deps,
                    "run",
                    "batch",
                    effective_junit.as_deref(),
                    json_mode,
                    args.summary,
                    &redaction,
                    err.code(),
                    &err.to_string(),
                    code,
                );
            }
        };
        let matching: Vec<String> = manifest
            .walks
            .into_iter()
            .filter(|entry| target_filter.matches_manifest_walk(entry))
            .map(|entry| entry.goal)
            .collect();
        if matching.is_empty() {
            return emit_early_exec_error(
                deps,
                "run",
                "batch",
                effective_junit.as_deref(),
                json_mode,
                args.summary,
                &redaction,
                "EMPTY_MANIFEST",
                "manifest contains no walks matching this configuration's appId, platform, adapter, and browser",
                EXIT_USAGE_OR_CONFIG,
            );
        }
        for g in &matching {
            if has_unsafe_control_or_bidi_char(g) {
                return emit_early_exec_error(
                    deps,
                    "run",
                    "batch",
                    effective_junit.as_deref(),
                    json_mode,
                    args.summary,
                    &redaction,
                    "INVALID_GOAL",
                    &format!(
                        "manifest goal contains unsafe control, line-break, bidi, or zero-width characters: {g:?}"
                    ),
                    EXIT_USAGE_OR_CONFIG,
                );
            }
        }
        matching
    };

    // Normalize every goal and reject duplicate normalized `goal.key`s in batch mode.
    let mut normalized_goals: Vec<Goal> = Vec::with_capacity(raw_goals.len());
    let mut seen_goal_keys: HashSet<String> = HashSet::with_capacity(raw_goals.len());
    for raw in raw_goals {
        let goal = match normalize_goal(&raw) {
            Ok(g) => g,
            Err(err) => {
                return emit_early_exec_error(
                    deps,
                    "run",
                    "batch",
                    effective_junit.as_deref(),
                    json_mode,
                    args.summary,
                    &redaction,
                    "INVALID_GOAL",
                    &err.to_string(),
                    EXIT_USAGE_OR_CONFIG,
                );
            }
        };
        if !seen_goal_keys.insert(goal.key.clone()) {
            return emit_early_exec_error(
                deps,
                "run",
                "batch",
                effective_junit.as_deref(),
                json_mode,
                args.summary,
                &redaction,
                "DUPLICATE_GOAL",
                &format!(
                    "duplicate goal in batch: {:?} (normalizes to {:?})",
                    raw, goal.key
                ),
                EXIT_USAGE_OR_CONFIG,
            );
        }
        normalized_goals.push(goal);
    }

    let batch_started_at = now_iso8601();
    let batch_t0 = Instant::now();

    // Open all shard stores upfront (`open_writable`); close already-opened stores on any error.
    let mut opened_stores: Vec<Arc<SqliteWalkStore>> = Vec::with_capacity(shard_specs.len());
    for spec in &shard_specs {
        let mut opts = SqliteStoreOptions::new(&config.workspace_root);
        if let Some(ref sid) = spec.store_id {
            opts = opts.with_store_id(sid.clone());
        }
        match SqliteWalkStore::open_writable(opts) {
            Ok(s) => opened_stores.push(Arc::new(s)),
            Err(err) => {
                close_all_stores(&opened_stores);
                let code = exit_code_for_store_error(&err);
                return emit_early_exec_error(
                    deps,
                    "run",
                    "batch",
                    effective_junit.as_deref(),
                    json_mode,
                    args.summary,
                    &redaction,
                    err.code(),
                    &err.to_string(),
                    code,
                );
            }
        }
    }

    // Compute `build_fingerprint` ONCE via `stores[0]` inside a cancellable `spawn_blocking` task
    // and broadcast read-only (`Arc<String>`).
    let build_path = config
        .build_path
        .as_ref()
        .expect("Execute capability requires build_path");
    let fp_t0 = Instant::now();
    let mut cancelled_during_setup = deps.cancel.is_cancelled();
    let build_fingerprint = if cancelled_during_setup {
        Arc::new(String::new())
    } else {
        let fp_store = Arc::clone(&opened_stores[0]);
        let fp_ws = config.workspace_root.clone();
        let fp_build_path = build_path.clone();
        let fp_platform = config.platform.clone();
        let fp_cancel_flag = Arc::new(AtomicBool::new(false));
        let fp_worker_flag = Arc::clone(&fp_cancel_flag);
        let mut fp_handle = tokio::task::spawn_blocking(move || {
            fp_store.fingerprint_with_cache_cancellable(
                &fp_ws,
                &fp_build_path,
                &fp_platform,
                &fp_worker_flag,
            )
        });
        let fp_outcome: Result<String, Option<StoreError>> = tokio::select! {
            biased;
            _ = deps.cancel.cancelled() => {
                fp_cancel_flag.store(true, Ordering::SeqCst);
                let _ = fp_handle.await;
                Err(None)
            }
            res = &mut fp_handle => match res {
                Ok(Ok(r)) => Ok(r.digest),
                Ok(Err(StoreError::Core(CoreError::Cancelled))) => Err(None),
                Ok(Err(err)) => {
                    if deps.cancel.is_cancelled() {
                        Err(None)
                    } else {
                        Err(Some(err))
                    }
                }
                Err(join_err) => Err(Some(StoreError::Io(std::io::Error::other(format!(
                    "build fingerprinting task failed: {join_err}"
                ))))),
            },
        };
        match fp_outcome {
            Ok(digest) => Arc::new(digest),
            Err(None) => {
                cancelled_during_setup = true;
                Arc::new(String::new())
            }
            Err(Some(err)) => {
                close_all_stores(&opened_stores);
                let code = exit_code_for_store_error(&err);
                return emit_early_exec_error(
                    deps,
                    "run",
                    "batch",
                    effective_junit.as_deref(),
                    json_mode,
                    args.summary,
                    &redaction,
                    err.code(),
                    &format!("failed to fingerprint build: {err}"),
                    code,
                );
            }
        }
    };
    let build_fingerprint_ms = fp_t0.elapsed().as_millis() as u64;

    // Affected-flow closure selection (`--changed-from <git-ref>`, `DEJA-80`).
    // Runs BEFORE duration scanning, runtime lookup, build staging, and driver attach so a
    // zero-selected batch closes shard stores immediately and performs zero device/driver work.
    let mut selection_summary: Option<BatchSelectionSummary> = None;
    let selected_goals_with_reasons: Vec<(Goal, Option<SelectionReason>)> =
        if !cancelled_during_setup && let Some(ref base_ref) = args.changed_from {
            match resolve_and_select_goals(
                base_ref,
                &args.always_include,
                &args.fallback_all_on,
                &config,
                consumer.as_deref(),
                &opened_stores,
                &build_fingerprint,
                &normalized_goals,
                &deps.env,
                &deps.cancel,
            )
            .await
            {
                Ok(outcome) => {
                    selection_summary = Some(outcome.summary);
                    outcome
                        .selected
                        .into_iter()
                        .map(|(g, reason)| (g, Some(reason)))
                        .collect()
                }
                Err(ChangedFromError::Cancelled) => {
                    cancelled_during_setup = true;
                    normalized_goals.into_iter().map(|g| (g, None)).collect()
                }
            }
        } else {
            normalized_goals.into_iter().map(|g| (g, None)).collect()
        };

    let zero_selected = !cancelled_during_setup
        && selection_summary.is_some()
        && selected_goals_with_reasons.is_empty();
    if zero_selected {
        close_all_stores(&opened_stores);
    }

    // Pool all integrity-verified historical `duration_ms` samples across EVERY shard store inside
    // a cancellable `spawn_blocking` task so LPT uses the exact pooled median while remaining
    // promptly interruptible on cancellation.
    let mut aggregated_medians: BTreeMap<String, u64> = BTreeMap::new();
    if !cancelled_during_setup && !zero_selected {
        match scan_duration_medians_cancellable(&opened_stores, &deps.cancel).await {
            Ok(medians) => aggregated_medians = medians,
            Err(None) => {
                cancelled_during_setup = true;
            }
            Err(Some(err)) => {
                close_all_stores(&opened_stores);
                let code = exit_code_for_store_error(&err);
                return emit_early_exec_error(
                    deps,
                    "run",
                    "batch",
                    effective_junit.as_deref(),
                    json_mode,
                    args.summary,
                    &redaction,
                    err.code(),
                    &err.to_string(),
                    code,
                );
            }
        }
    }

    // Build ordered input goals + LPT work queue: `(Reverse(historical_median_ms), index)`.
    let mut input_goals: Vec<BatchGoal> = selected_goals_with_reasons
        .into_iter()
        .enumerate()
        .map(|(index, (goal, selection_reason))| {
            let historical_median_ms = aggregated_medians.get(&goal.key).copied();
            BatchGoal {
                index,
                goal,
                historical_median_ms,
                selection_reason,
                eligible_shards: None,
            }
        })
        .collect();

    let mut lpt_sorted = input_goals.clone();
    lpt_sorted.sort_by_key(|g| (Reverse(g.historical_median_ms), g.index));
    let scheduled_order: Vec<String> = lpt_sorted.iter().map(|g| g.goal.text.clone()).collect();

    // Resolve runtime ONCE per shard via `driver_factory.resolve_runtime`.
    // In multi-device mode, if one shard fails runtime lookup, record the error against that shard,
    // close its store, exclude it from eligible shards, and continue running on healthy shards.
    let mut opened_shards: Vec<OpenedShard> = Vec::with_capacity(shard_specs.len());
    let mut shard_runtime_errors: BTreeMap<usize, (u8, String)> = BTreeMap::new();
    if !cancelled_during_setup && !zero_selected {
        for (orig_idx, (spec, store)) in shard_specs.iter().zip(opened_stores.iter()).enumerate() {
            let mut base_ctx = build_driver_context(
                &config,
                deps,
                config.timeout_ms,
                Some((*build_fingerprint).clone()),
            );
            base_ctx.device = spec.device.clone();

            match deps
                .driver_factory
                .resolve_runtime(&base_ctx, &deps.cancel)
                .await
            {
                Ok(runtime) => {
                    opened_shards.push(OpenedShard {
                        orig_idx,
                        device: spec.device.clone(),
                        store_id: spec.store_id.clone(),
                        store_label: store.store_label(),
                        store: Arc::clone(store),
                        runtime,
                        base_driver_ctx: base_ctx,
                        goals_executed: Arc::new(AtomicUsize::new(0)),
                    });
                }
                Err(DriverFactoryError::Cancelled) => {
                    cancelled_during_setup = true;
                    break;
                }
                Err(err) => {
                    if deps.cancel.is_cancelled() {
                        cancelled_during_setup = true;
                        break;
                    }
                    store.close();
                    let code = err.exit_code();
                    let msg = redaction.redact_str(&err.to_string());
                    deps.output.writeln_stderr(&format!(
                        "deja warning: skipping shard {} (runtime lookup failed): {msg}",
                        redaction.redact_str(&spec.device)
                    ));
                    shard_runtime_errors.insert(orig_idx, (code, msg));
                }
            }
        }
    }

    // In multi-device sharding, probe read-only replay eligibility per `(goal, shard)` ONLY across
    // healthy resolved shards (NEVER treating an unresolved/failed shard as eligible) BEFORE
    // spawning workers so each goal runs only on shards whose store/runtime can execute it.
    if !cancelled_during_setup && !zero_selected && opened_shards.len() > 1 {
        for bg in &mut lpt_sorted {
            if deps.cancel.is_cancelled() {
                cancelled_during_setup = true;
                break;
            }
            let identity = make_walk_identity(&config, &bg.goal, (*build_fingerprint).clone());
            let mut eligible = BTreeSet::new();
            for (shard_idx, shard) in opened_shards.iter().enumerate() {
                if let Ok(RunPlan::Execute(_)) = plan_execution(
                    &config,
                    consumer.as_deref(),
                    &shard.store,
                    &identity,
                    shard.runtime.as_deref(),
                    args.verify,
                ) {
                    eligible.insert(shard_idx);
                }
            }
            if !eligible.is_empty() && eligible.len() < opened_shards.len() {
                bg.eligible_shards = Some(eligible.clone());
                input_goals[bg.index].eligible_shards = Some(eligible);
            }
        }
    }

    let total_goals = input_goals.len();
    let results: Arc<AsyncMutex<Vec<Option<BatchGoalOutcomeRecord>>>> =
        Arc::new(AsyncMutex::new(vec![None; total_goals]));
    let refused_junit = Arc::new(AtomicBool::new(false));
    let refused_report = Arc::new(AtomicBool::new(false));
    let last_attach_error: Arc<AsyncMutex<Option<(u8, String)>>> = Arc::new(AsyncMutex::new(None));
    let shard_attach_errors: Arc<AsyncMutex<BTreeMap<usize, (u8, String)>>> =
        Arc::new(AsyncMutex::new(BTreeMap::new()));

    if cancelled_during_setup || zero_selected || opened_shards.is_empty() {
        close_all_stores(&opened_stores);
    } else {
        let queue_state = Arc::new(AsyncMutex::new(SharedQueueState {
            queue: VecDeque::from(lpt_sorted),
            attaching_shards: BTreeSet::new(),
            dead_shards: BTreeSet::new(),
        }));
        let (queue_version_tx, _queue_version_rx) = watch::channel(0u64);
        let queue_version_tx = Arc::new(queue_version_tx);
        let shared_staged: Arc<OnceCell<Option<SharedStagedBuild>>> = Arc::new(OnceCell::new());
        let extra_protected_arc = Arc::new(extra_protected_owned);
        let batch_screenshot_root = Arc::new(config.workspace_root.join(&batch_screenshots_rel));

        let max_extra_retries = args
            .retries
            .map(usize::from)
            .or(if args.rerun_failed { Some(1) } else { None })
            .unwrap_or_else(|| usize::from(config.retries));

        let mut handles = Vec::with_capacity(opened_shards.len());
        for (shard_idx, shard) in opened_shards.iter().enumerate() {
            let worker_ctx = WorkerSharedContext {
                config: Arc::clone(&config),
                consumer: consumer.clone(),
                redaction: Arc::clone(&redaction),
                deps: deps.clone(),
                build_fingerprint: Arc::clone(&build_fingerprint),
                build_fingerprint_ms,
                args_verify: args.verify,
                max_extra_retries,
                effective_junit: effective_junit.clone(),
                explicit_report: explicit_report.clone(),
                manifest_protected: Arc::clone(&manifest_protected),
                extra_protected: Arc::clone(&extra_protected_arc),
                batch_screenshot_root: Arc::clone(&batch_screenshot_root),
                queue_state: Arc::clone(&queue_state),
                queue_version_tx: Arc::clone(&queue_version_tx),
                shared_staged: Arc::clone(&shared_staged),
                manifest_mutex: Arc::clone(&manifest_mutex),
                results: Arc::clone(&results),
                refused_junit: Arc::clone(&refused_junit),
                refused_report: Arc::clone(&refused_report),
                last_attach_error: Arc::clone(&last_attach_error),
                shard_attach_errors: Arc::clone(&shard_attach_errors),
            };
            let worker_shard = WorkerShard {
                shard_idx,
                device: shard.device.clone(),
                store_id: shard.store_id.clone(),
                store: Arc::clone(&shard.store),
                runtime: shard.runtime.clone(),
                base_driver_ctx: shard.base_driver_ctx.clone(),
                goals_executed: Arc::clone(&shard.goals_executed),
            };
            handles.push(tokio::spawn(run_shard_worker(worker_shard, worker_ctx)));
        }

        // Await ALL worker handles — NEVER abort them so driver.shutdown() & store.close() always run!
        for handle in handles {
            let _ = handle.await;
        }

        // Defensively close all stores and explicitly drop `shared_staged` BEFORE returning so
        // temporary `.app` staging directories are cleaned up immediately.
        close_all_stores(&opened_stores);
        drop(shared_staged);
    }

    let cancelled = deps.cancel.is_cancelled() || cancelled_during_setup;
    let cancel_signal = deps.cancel_signal.load(Ordering::SeqCst);
    let attach_fallback = last_attach_error.lock().await.clone();
    let per_shard_attach = shard_attach_errors.lock().await.clone();
    let first_runtime_err = shard_runtime_errors.values().next().cloned();

    // Assemble deterministic `0..N` input-order results, marking any unstarted goals as skipped
    // (on cancellation) or errored (if eligible shard(s) failed runtime lookup or driver attach).
    let mut locked_results = results.lock().await;
    let mut final_records: Vec<BatchGoalOutcomeRecord> = Vec::with_capacity(total_goals);
    for bg in &input_goals {
        if let Some(rec) = locked_results[bg.index].take() {
            final_records.push(rec);
        } else if cancelled {
            final_records.push(make_skipped_goal_record(bg, &redaction));
        } else {
            let eligible_err = bg.eligible_shards.as_ref().and_then(|allowed| {
                allowed
                    .iter()
                    .find_map(|idx| per_shard_attach.get(idx).cloned())
            });
            let (code, msg) = eligible_err
                .or_else(|| attach_fallback.clone())
                .or_else(|| first_runtime_err.clone())
                .unwrap_or_else(|| {
                    (
                        EXIT_FAILURE,
                        "no available shard worker to execute remaining goal".to_string(),
                    )
                });
            final_records.push(make_error_goal_record(
                bg,
                code,
                &msg,
                None,
                None,
                None,
                "DriverError",
                &redaction,
            ));
        }
    }
    drop(locked_results);

    // Build shard summaries in original `--devices` order, recording any runtime-lookup or attach
    // error directly on the corresponding shard entry.
    let opened_by_orig: BTreeMap<usize, (usize, &OpenedShard)> = opened_shards
        .iter()
        .enumerate()
        .map(|(worker_idx, s)| (s.orig_idx, (worker_idx, s)))
        .collect();
    let shard_summaries: Vec<BatchShardSummary> = shard_specs
        .iter()
        .zip(opened_stores.iter())
        .enumerate()
        .map(|(orig_idx, (spec, store))| {
            if let Some(&(worker_idx, s)) = opened_by_orig.get(&orig_idx) {
                let attach_err = per_shard_attach
                    .get(&worker_idx)
                    .map(|(_, msg)| redaction.redact_str(msg));
                BatchShardSummary {
                    device: s.device.clone(),
                    store: s.store_id.clone(),
                    store_label: s.store_label.clone(),
                    runtime: s.runtime.clone(),
                    goals_executed: s.goals_executed.load(Ordering::SeqCst),
                    error: attach_err,
                }
            } else {
                let rt_err = shard_runtime_errors
                    .get(&orig_idx)
                    .map(|(_, msg)| msg.clone());
                BatchShardSummary {
                    device: spec.device.clone(),
                    store: spec.store_id.clone(),
                    store_label: store.store_label(),
                    runtime: None,
                    goals_executed: 0,
                    error: rt_err,
                }
            }
        })
        .collect();

    let mut counts = BatchCounts {
        total: final_records.len(),
        skipped_by_selection: selection_summary.as_ref().map(|s| s.skipped_count),
        ..BatchCounts::default()
    };
    let mut goal_exit_codes: Vec<u8> = Vec::with_capacity(final_records.len());
    let mut junit_cases: Vec<BatchJunitCase> = Vec::with_capacity(final_records.len());
    let mut goal_results: Vec<BatchGoalResult> = Vec::with_capacity(final_records.len());

    for rec in final_records {
        goal_exit_codes.push(rec.result.exit_code);
        match rec.result.outcome.as_str() {
            "passed" | "remembered" | "cold-recorded" | "repaired" => counts.passed += 1,
            "flaky" => counts.flaky += 1,
            "flow-changed" | "not-validated" | "needs-migration" | "assertions-changed" => {
                counts.drifted += 1
            }
            "cancelled" => counts.cancelled += 1,
            "skipped" => counts.skipped += 1,
            "error" => counts.errored += 1,
            _ => counts.failed += 1,
        }
        junit_cases.push(rec.junit_case);
        goal_results.push(rec.result);
    }

    let prelim_exit = compute_batch_exit_code(
        cancelled,
        cancel_signal,
        refused_junit.load(Ordering::SeqCst) || refused_report.load(Ordering::SeqCst),
        &goal_exit_codes,
    );
    let batch_outcome = derive_batch_outcome(cancelled, prelim_exit, &goal_results);
    let total_ms = batch_t0.elapsed().as_millis() as u64;

    let mut explicit_artifact_failed =
        refused_junit.load(Ordering::SeqCst) || refused_report.load(Ordering::SeqCst);
    let mut batch_artifacts: Vec<RunArtifact> = Vec::new();
    let mut batch_errors: Vec<String> = Vec::new();

    if let Some(ref jpath) = effective_junit {
        if refused_junit.load(Ordering::SeqCst) {
            let msg = redaction.redact_str(&format!(
                "refusing to write --junit over a protected flow closure path: {}",
                jpath.display()
            ));
            batch_errors.push(msg.clone());
            deps.output.writeln_stderr(&format!("deja error: {msg}"));
        } else if let Err(err) = write_batch_junit(
            jpath,
            "Test Suite",
            &batch_outcome,
            &junit_cases,
            total_ms,
            &redaction,
        ) {
            let msg = redaction.redact_str(&format!("failed to write JUnit report: {err}"));
            batch_errors.push(msg.clone());
            deps.output.writeln_stderr(&format!("deja error: {msg}"));
            explicit_artifact_failed = true;
        } else {
            batch_artifacts.push(RunArtifact {
                kind: ArtifactKind::Junit,
                path: workspace_relative_display(&config.workspace_root, jpath),
                sha256: None,
            });
        }
    }

    let mut final_exit = compute_batch_exit_code(
        cancelled,
        cancel_signal,
        explicit_artifact_failed,
        &goal_exit_codes,
    );
    let final_outcome = derive_batch_outcome(cancelled, final_exit, &goal_results);

    let mut batch_report = CliBatchRunReport {
        schema_version: WALK_RESULT_SCHEMA_VERSION,
        kind: "deja-batch-run".to_string(),
        command: "run".to_string(),
        mode: "batch".to_string(),
        outcome: final_outcome,
        passed: final_exit == EXIT_OK && !cancelled,
        cancelled,
        exit_code: final_exit,
        started_at: batch_started_at,
        ended_at: now_iso8601(),
        duration_ms: total_ms,
        build_fingerprint: (*build_fingerprint).clone(),
        build_fingerprint_ms,
        app_id: config.app_id.clone(),
        platform: config.platform.clone(),
        adapter: config.adapter.clone(),
        browser: config.browser.clone(),
        devices: shard_specs.iter().map(|s| s.device.clone()).collect(),
        shards: shard_summaries,
        scheduled_order,
        selection: selection_summary,
        counts,
        goals: goal_results,
        artifacts: batch_artifacts,
        errors: batch_errors,
    };

    let mut redacted_json = match redaction.redact_batch_report(&batch_report) {
        Ok(val) => val,
        Err(err) => {
            deps.output.writeln_stderr(&format!(
                "deja error: failed to serialize batch run report: {err}"
            ));
            return if cancelled {
                exit_code_for_cancel_signal(cancel_signal)
            } else {
                EXIT_USAGE_OR_CONFIG
            };
        }
    };

    // Attempt explicit `--report` write BEFORE emitting stdout JSON / human output and BEFORE
    // persisting internal reports so any write error is accurately reflected in `batch_report`
    // (`errors`, `exitCode`, `passed`, `outcome`) and matches the process exit code!
    if let Some(ref rpath) = explicit_report {
        let write_err_msg = if refused_report.load(Ordering::SeqCst) {
            Some(redaction.redact_str(&format!(
                "refusing to write --report over a protected flow closure path: {}",
                rpath.display()
            )))
        } else {
            let mut text = serde_json::to_string_pretty(&redacted_json)
                .expect("redacted batch JSON serializes");
            text.push('\n');
            write_atomic_nofollow_excl(rpath, text.as_bytes())
                .err()
                .map(|err| {
                    redaction.redact_str(&format!(
                        "failed to write explicit --report {}: {err}",
                        rpath.display()
                    ))
                })
        };

        if let Some(msg) = write_err_msg {
            deps.output.writeln_stderr(&format!("deja error: {msg}"));
            batch_report.errors.push(msg);
            explicit_artifact_failed = true;
            final_exit = compute_batch_exit_code(
                cancelled,
                cancel_signal,
                explicit_artifact_failed,
                &goal_exit_codes,
            );
            batch_report.exit_code = final_exit;
            batch_report.outcome = derive_batch_outcome(cancelled, final_exit, &batch_report.goals);
            batch_report.passed = final_exit == EXIT_OK && !cancelled;
            if let Ok(updated) = redaction.redact_batch_report(&batch_report) {
                redacted_json = updated;
            }
        }
    }

    if let Err(err) =
        write_internal_batch_reports(&config.workspace_root, &immutable_batch_rel, &redacted_json)
    {
        deps.output.writeln_stderr(&redaction.redact_str(&format!(
            "deja warning: failed to write batch report artifact: {err}"
        )));
    }

    if json_mode {
        deps.output.writeln_stdout(
            &serde_json::to_string_pretty(&redacted_json).expect("redacted batch JSON serializes"),
        );
    } else {
        print_human_batch_report(&deps.output, &batch_report, &redaction);
    }

    if args.summary {
        for entry in &batch_report.goals {
            let line = if let Some(ref rep) = entry.report {
                format_cli_summary_line(rep, &redaction)
            } else {
                // Keep `deja-summary v=1` strictly compatible with the v1 summary grammar
                // (`outcome=error`, `failure=error`) while preserving `"cancelled"`/`"skipped"`
                // in `CliBatchRunReport` JSON and merged JUnit XML.
                format_batch_status_summary_line_with_selection(
                    &entry.goal,
                    "error",
                    "error",
                    &batch_report.build_fingerprint,
                    entry.selection_reason,
                    &redaction,
                )
            };
            emit_summary(&deps.output, json_mode, &line);
        }
    }

    final_exit
}

fn resolve_shard_specs(args: &RunArgs, config: &LoadedConfig) -> Result<Vec<ShardSpec>, String> {
    if args.devices.is_empty() {
        let device = config
            .device
            .clone()
            .ok_or_else(|| "a target device is required".to_string())?;
        return Ok(vec![ShardSpec {
            device,
            store_id: args.store.clone(),
        }]);
    }

    let mut specs = Vec::with_capacity(args.devices.len());
    let mut seen_devices: HashSet<String> = HashSet::new();
    let mut seen_store_ids: HashSet<String> = HashSet::new();

    for raw in &args.devices {
        let trimmed = raw.trim();
        let canonical_device = if config.platform == "ios" {
            validate_udid(trimmed).map_err(|err| {
                format!("iOS `--devices` entry must be an explicit simulator UDID (got {trimmed:?}): {err}")
            })?
        } else {
            trimmed.to_string()
        };

        if !seen_devices.insert(canonical_device.clone()) {
            continue;
        }

        let store_id = device_to_store_id(&canonical_device);
        validate_store_id(&store_id).map_err(|err| {
            format!("derived store ID for device {canonical_device:?} is invalid: {err}")
        })?;
        if !seen_store_ids.insert(store_id.clone()) {
            return Err(format!(
                "devices map to colliding store ID {store_id:?}; pass distinct device identifiers"
            ));
        }

        specs.push(ShardSpec {
            device: canonical_device,
            store_id: Some(store_id),
        });
    }

    if specs.is_empty() {
        return Err("`--devices` produced no valid device identifiers".to_string());
    }
    Ok(specs)
}

fn derive_batch_outcome(cancelled: bool, exit_code: u8, goals: &[BatchGoalResult]) -> String {
    if cancelled {
        return "cancelled".to_string();
    }
    match exit_code {
        EXIT_OK => "passed".to_string(),
        EXIT_FLAKY => "flaky".to_string(),
        EXIT_EXPLORATION_REJECTED => "exploration-rejected".to_string(),
        EXIT_MANIFEST_DRIFT => {
            let mut drift_outcomes = goals
                .iter()
                .filter(|g| g.exit_code == EXIT_MANIFEST_DRIFT)
                .map(|g| g.outcome.as_str());
            if let Some(first) = drift_outcomes.next()
                && drift_outcomes.all(|o| o == first)
            {
                return first.to_string();
            }
            "flow-changed".to_string()
        }
        EXIT_USAGE_OR_CONFIG | EXIT_INVALID_MEMORY | EXIT_STORE_LOCKED => "error".to_string(),
        _ => "failed".to_string(),
    }
}

struct WorkerShard {
    shard_idx: usize,
    device: String,
    store_id: Option<String>,
    store: Arc<SqliteWalkStore>,
    runtime: Option<String>,
    base_driver_ctx: DriverContext,
    goals_executed: Arc<AtomicUsize>,
}

struct SharedQueueState {
    queue: VecDeque<BatchGoal>,
    /// Shards currently performing lazy first-time driver attach for an unstarted goal that may be
    /// pushed back onto `queue` if attach fails.
    attaching_shards: BTreeSet<usize>,
    /// Shards that failed lazy attach and exited permanently.
    dead_shards: BTreeSet<usize>,
}

struct WorkerSharedContext {
    config: Arc<LoadedConfig>,
    consumer: Option<Arc<ConsumerFlows>>,
    redaction: Arc<RedactionContext>,
    deps: CliDeps,
    build_fingerprint: Arc<String>,
    build_fingerprint_ms: u64,
    args_verify: bool,
    max_extra_retries: usize,
    effective_junit: Option<PathBuf>,
    explicit_report: Option<PathBuf>,
    manifest_protected: Arc<BTreeSet<PathBuf>>,
    extra_protected: Arc<Vec<PathBuf>>,
    batch_screenshot_root: Arc<PathBuf>,
    queue_state: Arc<AsyncMutex<SharedQueueState>>,
    queue_version_tx: Arc<watch::Sender<u64>>,
    shared_staged: Arc<OnceCell<Option<SharedStagedBuild>>>,
    manifest_mutex: Arc<AsyncMutex<()>>,
    results: Arc<AsyncMutex<Vec<Option<BatchGoalOutcomeRecord>>>>,
    refused_junit: Arc<AtomicBool>,
    refused_report: Arc<AtomicBool>,
    last_attach_error: Arc<AsyncMutex<Option<(u8, String)>>>,
    shard_attach_errors: Arc<AsyncMutex<BTreeMap<usize, (u8, String)>>>,
}

async fn clear_attaching_shard(ctx: &WorkerSharedContext, shard_idx: usize) {
    let mut state = ctx.queue_state.lock().await;
    if state.attaching_shards.remove(&shard_idx) {
        ctx.queue_version_tx.send_modify(|v| *v = v.wrapping_add(1));
    }
}

async fn fail_attach_and_requeue(
    ctx: &WorkerSharedContext,
    shard_idx: usize,
    bg: BatchGoal,
    code: u8,
    msg: String,
) {
    *ctx.last_attach_error.lock().await = Some((code, msg.clone()));
    ctx.shard_attach_errors
        .lock()
        .await
        .insert(shard_idx, (code, msg));
    let mut state = ctx.queue_state.lock().await;
    state.attaching_shards.remove(&shard_idx);
    state.dead_shards.insert(shard_idx);
    let insert_pos = state
        .queue
        .iter()
        .position(|other| {
            (Reverse(bg.historical_median_ms), bg.index)
                < (Reverse(other.historical_median_ms), other.index)
        })
        .unwrap_or(state.queue.len());
    state.queue.insert(insert_pos, bg);
    ctx.queue_version_tx.send_modify(|v| *v = v.wrapping_add(1));
}

async fn acquire_manifest_lock_cancellable<'a>(
    manifest_mutex: &'a AsyncMutex<()>,
    cancel: &CancellationToken,
) -> Option<MutexGuard<'a, ()>> {
    let guard = tokio::select! {
        biased;
        _ = cancel.cancelled() => return None,
        guard = manifest_mutex.lock() => guard,
    };
    if cancel.is_cancelled() {
        drop(guard);
        return None;
    }
    Some(guard)
}

async fn run_shard_worker(shard: WorkerShard, ctx: WorkerSharedContext) {
    let mut persistent_driver: Option<Box<dyn PlatformDriver>> = None;
    let mut queue_version_rx = ctx.queue_version_tx.subscribe();

    loop {
        if ctx.deps.cancel.is_cancelled() {
            break;
        }

        let maybe_goal = loop {
            let mut state = ctx.queue_state.lock().await;
            let _ = queue_version_rx.borrow_and_update();
            if ctx.deps.cancel.is_cancelled() || state.dead_shards.contains(&shard.shard_idx) {
                break None;
            }
            // Strictly pick the FIRST (longest-LPT) eligible goal in the queue — never prioritizing
            // shorter constrained goals ahead of longer unconstrained goals.
            let pos = state
                .queue
                .iter()
                .position(|bg| bg.is_eligible_for(shard.shard_idx));
            if let Some(idx) = pos {
                let bg = state.queue.remove(idx).expect("queue index valid");
                if persistent_driver.is_none() {
                    state.attaching_shards.insert(shard.shard_idx);
                }
                break Some(bg);
            }
            if state.attaching_shards.is_empty() {
                break None;
            }
            // Another shard is performing lazy driver attach and may return its unstarted goal to
            // the queue if attach fails; wait until queue state changes or cancellation triggers.
            drop(state);
            tokio::select! {
                biased;
                _ = ctx.deps.cancel.cancelled() => break None,
                changed = queue_version_rx.changed() => {
                    if changed.is_err() {
                        break None;
                    }
                }
            }
        };
        let Some(bg) = maybe_goal else {
            break;
        };

        let goal_t0 = Instant::now();
        let goal_started_at = now_iso8601();
        let immutable_report_rel = allocate_report_rel_path(RunCommand::Run);
        let identity = make_walk_identity(&ctx.config, &bg.goal, (*ctx.build_fingerprint).clone());

        let plan = match plan_execution(
            &ctx.config,
            ctx.consumer.as_deref(),
            &shard.store,
            &identity,
            shard.runtime.as_deref(),
            ctx.args_verify,
        ) {
            Ok(p) => p,
            Err(err) => {
                clear_attaching_shard(&ctx, shard.shard_idx).await;
                let code = exit_code_for_store_error(&err);
                let rec = make_error_goal_record(
                    &bg,
                    code,
                    &err.to_string(),
                    Some(shard.device.clone()),
                    shard.store_id.clone(),
                    shard.runtime.clone(),
                    err.code(),
                    &ctx.redaction,
                );
                store_goal_record(&ctx.results, bg.index, rec).await;
                shard.goals_executed.fetch_add(1, Ordering::SeqCst);
                continue;
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
                // Refused goals NEVER trigger build staging or driver attach.
                clear_attaching_shard(&ctx, shard.shard_idx).await;
                let total_ms = goal_t0.elapsed().as_millis() as u64;
                let mut report = build_base_report(
                    RunCommand::Run,
                    identity,
                    &bg.goal,
                    cache_status,
                    Some(miss_reason),
                    outcome,
                    false,
                    goal_started_at,
                    now_iso8601(),
                    total_ms,
                    shard.runtime.clone(),
                );
                report.core.yaml_path = yaml_path;
                report.core.errors.push(ctx.redaction.redact_str(&detail));
                report.timing = Some(RunTimingBreakdown {
                    build_fingerprint_ms: ctx.build_fingerprint_ms,
                    total_ms,
                    ..RunTimingBreakdown::default()
                });
                report.selection_reason = bg.selection_reason;
                persist_per_goal_internal_report(
                    &ctx.config.workspace_root,
                    shard.store_id.as_deref(),
                    &immutable_report_rel,
                    &report,
                    &ctx.redaction,
                );
                let rec = make_report_goal_record(
                    &bg,
                    report,
                    Some(immutable_report_rel),
                    &shard.device,
                    shard.store_id.as_deref(),
                    shard.runtime.as_deref(),
                    &ctx.redaction,
                );
                store_goal_record(&ctx.results, bg.index, rec).await;
                shard.goals_executed.fetch_add(1, Ordering::SeqCst);
                continue;
            }
            RunPlan::Execute(ep) => *ep,
        };

        let closure = exec_plan.closure;
        let extra_prot_refs: Vec<&Path> =
            ctx.extra_protected.iter().map(PathBuf::as_path).collect();

        if let Some(ref jpath) = ctx.effective_junit
            && let Err(msg) = validate_explicit_output_path_with_precomputed_manifest(
                jpath,
                "--junit",
                &ctx.config,
                Some(&closure),
                &ctx.manifest_protected,
                &extra_prot_refs,
            )
        {
            clear_attaching_shard(&ctx, shard.shard_idx).await;
            ctx.refused_junit.store(true, Ordering::SeqCst);
            let rec = make_error_goal_record(
                &bg,
                EXIT_USAGE_OR_CONFIG,
                &msg,
                Some(shard.device.clone()),
                shard.store_id.clone(),
                shard.runtime.clone(),
                "JunitPathRefused",
                &ctx.redaction,
            );
            store_goal_record(&ctx.results, bg.index, rec).await;
            shard.goals_executed.fetch_add(1, Ordering::SeqCst);
            continue;
        }
        if let Some(ref rpath) = ctx.explicit_report
            && let Err(msg) = validate_explicit_output_path_with_precomputed_manifest(
                rpath,
                "--report",
                &ctx.config,
                Some(&closure),
                &ctx.manifest_protected,
                &extra_prot_refs,
            )
        {
            clear_attaching_shard(&ctx, shard.shard_idx).await;
            ctx.refused_report.store(true, Ordering::SeqCst);
            let rec = make_error_goal_record(
                &bg,
                EXIT_USAGE_OR_CONFIG,
                &msg,
                Some(shard.device.clone()),
                shard.store_id.clone(),
                shard.runtime.clone(),
                "ReportPathRefused",
                &ctx.redaction,
            );
            store_goal_record(&ctx.results, bg.index, rec).await;
            shard.goals_executed.fetch_add(1, Ordering::SeqCst);
            continue;
        }

        if let Err(msg) =
            validate_closure_app_id_confinement(&closure, &ctx.config.app_id, &ctx.config.env)
        {
            clear_attaching_shard(&ctx, shard.shard_idx).await;
            let rec = make_error_goal_record(
                &bg,
                EXIT_USAGE_OR_CONFIG,
                &msg,
                Some(shard.device.clone()),
                shard.store_id.clone(),
                shard.runtime.clone(),
                "AppIdConfinement",
                &ctx.redaction,
            );
            store_goal_record(&ctx.results, bg.index, rec).await;
            shard.goals_executed.fetch_add(1, Ordering::SeqCst);
            continue;
        }

        if ctx.consumer.is_some()
            && let Err(msg) = check_closure_forbidden_values(&closure, &ctx.config.forbidden_values)
        {
            clear_attaching_shard(&ctx, shard.shard_idx).await;
            let rec = make_error_goal_record(
                &bg,
                EXIT_USAGE_OR_CONFIG,
                &msg,
                Some(shard.device.clone()),
                shard.store_id.clone(),
                shard.runtime.clone(),
                "ForbiddenSecret",
                &ctx.redaction,
            );
            store_goal_record(&ctx.results, bg.index, rec).await;
            shard.goals_executed.fetch_add(1, Ordering::SeqCst);
            continue;
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
                clear_attaching_shard(&ctx, shard.shard_idx).await;
                let rec = make_error_goal_record(
                    &bg,
                    EXIT_USAGE_OR_CONFIG,
                    &err.to_string(),
                    Some(shard.device.clone()),
                    shard.store_id.clone(),
                    shard.runtime.clone(),
                    "ClosureError",
                    &ctx.redaction,
                );
                store_goal_record(&ctx.results, bg.index, rec).await;
                shard.goals_executed.fetch_add(1, Ordering::SeqCst);
                continue;
            }
        };
        let assertion_count = count_assertions_in_closure(&closure).ok();

        // Lazily stage the shared `.app` ONCE and attach ONE persistent driver per shard.
        let mut driver_attach_ms = 0u64;
        if persistent_driver.is_none() {
            if ctx.deps.cancel.is_cancelled() {
                clear_attaching_shard(&ctx, shard.shard_idx).await;
                let rec = make_cancelled_goal_record(
                    &bg,
                    goal_t0.elapsed().as_millis() as u64,
                    &shard.device,
                    shard.store_id.as_deref(),
                    shard.runtime.as_deref(),
                    "cancelled before driver attach",
                    &ctx.redaction,
                );
                store_goal_record(&ctx.results, bg.index, rec).await;
                break;
            }

            let attach_t0 = Instant::now();
            let staged_opt = match ctx
                .shared_staged
                .get_or_try_init(|| async {
                    ctx.deps
                        .driver_factory
                        .prepare_batch_build(&shard.base_driver_ctx, &ctx.deps.cancel)
                        .await
                })
                .await
            {
                Ok(opt) => opt.clone(),
                Err(DriverFactoryError::Cancelled) => {
                    clear_attaching_shard(&ctx, shard.shard_idx).await;
                    let rec = make_cancelled_goal_record(
                        &bg,
                        goal_t0.elapsed().as_millis() as u64,
                        &shard.device,
                        shard.store_id.as_deref(),
                        shard.runtime.as_deref(),
                        "cancelled while staging build",
                        &ctx.redaction,
                    );
                    store_goal_record(&ctx.results, bg.index, rec).await;
                    break;
                }
                Err(err) => {
                    if ctx.deps.cancel.is_cancelled() {
                        clear_attaching_shard(&ctx, shard.shard_idx).await;
                        let rec = make_cancelled_goal_record(
                            &bg,
                            goal_t0.elapsed().as_millis() as u64,
                            &shard.device,
                            shard.store_id.as_deref(),
                            shard.runtime.as_deref(),
                            "cancelled while staging build",
                            &ctx.redaction,
                        );
                        store_goal_record(&ctx.results, bg.index, rec).await;
                        break;
                    }
                    fail_attach_and_requeue(
                        &ctx,
                        shard.shard_idx,
                        bg,
                        err.exit_code(),
                        err.to_string(),
                    )
                    .await;
                    break;
                }
            };

            let mut attach_ctx = shard.base_driver_ctx.clone();
            attach_ctx.staged_build = staged_opt;

            let driver = match ctx
                .deps
                .driver_factory
                .create(&attach_ctx, &ctx.deps.cancel)
                .await
            {
                Ok(d) => d,
                Err(DriverFactoryError::Cancelled) => {
                    clear_attaching_shard(&ctx, shard.shard_idx).await;
                    let rec = make_cancelled_goal_record(
                        &bg,
                        goal_t0.elapsed().as_millis() as u64,
                        &shard.device,
                        shard.store_id.as_deref(),
                        shard.runtime.as_deref(),
                        "cancelled while attaching driver",
                        &ctx.redaction,
                    );
                    store_goal_record(&ctx.results, bg.index, rec).await;
                    break;
                }
                Err(err) => {
                    if ctx.deps.cancel.is_cancelled() {
                        clear_attaching_shard(&ctx, shard.shard_idx).await;
                        let rec = make_cancelled_goal_record(
                            &bg,
                            goal_t0.elapsed().as_millis() as u64,
                            &shard.device,
                            shard.store_id.as_deref(),
                            shard.runtime.as_deref(),
                            "cancelled while attaching driver",
                            &ctx.redaction,
                        );
                        store_goal_record(&ctx.results, bg.index, rec).await;
                        break;
                    }
                    fail_attach_and_requeue(
                        &ctx,
                        shard.shard_idx,
                        bg,
                        err.exit_code(),
                        err.to_string(),
                    )
                    .await;
                    break;
                }
            };

            let attach_res: Result<(), (u8, String, bool)> = async {
                driver.ensure_session(&ctx.deps.cancel).await.map_err(|e| {
                    let is_cancel = matches!(e, deja_flow::DriverError::Cancelled)
                        || ctx.deps.cancel.is_cancelled();
                    (EXIT_FAILURE, e.to_string(), is_cancel)
                })?;
                ctx.deps
                    .driver_factory
                    .install_app(&attach_ctx, &ctx.deps.cancel)
                    .await
                    .map_err(|e| {
                        let is_cancel = matches!(e, DriverFactoryError::Cancelled)
                            || ctx.deps.cancel.is_cancelled();
                        (e.exit_code(), e.to_string(), is_cancel)
                    })?;
                Ok(())
            }
            .await;

            if let Err((code, msg, is_cancel)) = attach_res {
                let _ = driver.shutdown().await;
                if is_cancel || ctx.deps.cancel.is_cancelled() {
                    clear_attaching_shard(&ctx, shard.shard_idx).await;
                    let rec = make_cancelled_goal_record(
                        &bg,
                        goal_t0.elapsed().as_millis() as u64,
                        &shard.device,
                        shard.store_id.as_deref(),
                        shard.runtime.as_deref(),
                        "cancelled during driver attach",
                        &ctx.redaction,
                    );
                    store_goal_record(&ctx.results, bg.index, rec).await;
                } else {
                    fail_attach_and_requeue(&ctx, shard.shard_idx, bg, code, msg).await;
                }
                break;
            }

            driver_attach_ms = attach_t0.elapsed().as_millis() as u64;
            persistent_driver = Some(driver);
            clear_attaching_shard(&ctx, shard.shard_idx).await;
        }

        let driver_ref = persistent_driver
            .as_deref()
            .expect("persistent driver attached");

        let exec_t0 = Instant::now();
        let timeout_ms = if ctx.args_verify {
            ctx.config.verification_timeout_ms
        } else {
            ctx.config.timeout_ms
        };

        let mut attempts_raw: Vec<ExecutedAttempt> = Vec::new();
        let mut mid_retry_drift: Option<String> = None;

        let first_shot_dir = ctx
            .batch_screenshot_root
            .join(format!("goal-{}-attempt-1", bg.index));
        let first_attempt = execute_single_attempt_with_screenshot_dir(
            driver_ref,
            &ctx.config,
            &closure,
            timeout_ms,
            Some(&first_shot_dir),
            &ctx.redaction,
            &ctx.deps.cancel,
        )
        .await;

        // If cancelled mid-flight: skip retries, crash grace, and SQLite writes!
        if ctx.deps.cancel.is_cancelled() {
            let rec = make_cancelled_goal_record(
                &bg,
                goal_t0.elapsed().as_millis() as u64,
                &shard.device,
                shard.store_id.as_deref(),
                shard.runtime.as_deref(),
                "cancelled during execution",
                &ctx.redaction,
            );
            store_goal_record(&ctx.results, bg.index, rec).await;
            shard.goals_executed.fetch_add(1, Ordering::SeqCst);
            break;
        }

        let first_passed = first_attempt.passed;
        attempts_raw.push(first_attempt);

        let mut cancelled_in_retry = false;
        if !first_passed && ctx.max_extra_retries > 0 && !ctx.deps.cancel.is_cancelled() {
            let first_sig = failure_signature(attempts_raw[0].failure.as_ref());
            for retry_idx in 0..ctx.max_extra_retries {
                if ctx.deps.cancel.is_cancelled() {
                    cancelled_in_retry = true;
                    break;
                }
                match verify_unchanged_closure(
                    &ctx.config.workspace_root,
                    &exec_plan.flow_abs_path,
                    &expected_closure_sha256,
                    &expected_assertions_sha256,
                ) {
                    Ok(()) => {}
                    Err(drift_msg) => {
                        mid_retry_drift = Some(drift_msg);
                        break;
                    }
                }
                let retry_shot_dir = ctx.batch_screenshot_root.join(format!(
                    "goal-{}-attempt-{}",
                    bg.index,
                    retry_idx + 2
                ));
                let retry_attempt = execute_single_attempt_with_screenshot_dir(
                    driver_ref,
                    &ctx.config,
                    &closure,
                    timeout_ms,
                    Some(&retry_shot_dir),
                    &ctx.redaction,
                    &ctx.deps.cancel,
                )
                .await;
                if ctx.deps.cancel.is_cancelled() {
                    cancelled_in_retry = true;
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

        if cancelled_in_retry || ctx.deps.cancel.is_cancelled() {
            let rec = make_cancelled_goal_record(
                &bg,
                goal_t0.elapsed().as_millis() as u64,
                &shard.device,
                shard.store_id.as_deref(),
                shard.runtime.as_deref(),
                "cancelled during retry execution",
                &ctx.redaction,
            );
            store_goal_record(&ctx.results, bg.index, rec).await;
            shard.goals_executed.fetch_add(1, Ordering::SeqCst);
            break;
        }

        let flow_exec_ms = exec_t0.elapsed().as_millis() as u64;

        // Mid-retry closure drift -> `flow-changed` (exit 6), zero SQLite writes.
        if let Some(drift_msg) = mid_retry_drift {
            let total_ms = goal_t0.elapsed().as_millis() as u64;
            let mut report = build_base_report(
                RunCommand::Run,
                identity,
                &bg.goal,
                exec_plan.cache_status,
                Some(CacheMissReason::FlowChanged),
                RunOutcome::FlowChanged,
                false,
                goal_started_at,
                now_iso8601(),
                total_ms,
                shard.runtime.clone(),
            );
            report.core.yaml_path = Some(exec_plan.display_flow_path);
            report.core.yaml_sha256 = Some(main_sha256);
            report.core.assertions = assertion_count;
            report
                .core
                .errors
                .push(ctx.redaction.redact_str(&drift_msg));
            report.timing = Some(RunTimingBreakdown {
                build_fingerprint_ms: ctx.build_fingerprint_ms,
                driver_attach_ms,
                flow_exec_ms,
                total_ms,
                steps: attempts_raw[0].timing.steps.clone(),
                ..RunTimingBreakdown::default()
            });
            report.selection_reason = bg.selection_reason;
            persist_per_goal_internal_report(
                &ctx.config.workspace_root,
                shard.store_id.as_deref(),
                &immutable_report_rel,
                &report,
                &ctx.redaction,
            );
            let rec = make_report_goal_record(
                &bg,
                report,
                Some(immutable_report_rel),
                &shard.device,
                shard.store_id.as_deref(),
                shard.runtime.as_deref(),
                &ctx.redaction,
            );
            store_goal_record(&ctx.results, bg.index, rec).await;
            shard.goals_executed.fetch_add(1, Ordering::SeqCst);
            continue;
        }

        // Classify aggregate outcome across attempts.
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
            build_fingerprint_ms: ctx.build_fingerprint_ms,
            driver_attach_ms,
            warmup_ms: first.timing.warmup_ms,
            flow_exec_ms,
            store_ms: 0,
            total_ms: goal_t0.elapsed().as_millis() as u64,
            steps: first.timing.steps.clone(),
        };
        let phase_timings_json = serde_json::to_string(&timing).ok();

        let validation_at = first.ended_at.clone();
        let mut validation_input = RecordValidationInput::new(final_passed, validation_at)
            .with_flaky(is_flaky)
            .with_report_path(immutable_report_rel.clone())
            .with_duration_ms(goal_t0.elapsed().as_millis() as u64);
        if let Some(ref rt) = shard.runtime {
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

        // Final check right before persisting to SQLite so cancelled runs never write aborted state.
        if ctx.deps.cancel.is_cancelled() {
            let rec = make_cancelled_goal_record(
                &bg,
                goal_t0.elapsed().as_millis() as u64,
                &shard.device,
                shard.store_id.as_deref(),
                shard.runtime.as_deref(),
                "cancelled before persistence",
                &ctx.redaction,
            );
            store_goal_record(&ctx.results, bg.index, rec).await;
            shard.goals_executed.fetch_add(1, Ordering::SeqCst);
            break;
        }

        let store_t0 = Instant::now();
        let mut manifest_written = false;
        let mut recorded_for_new_build = false;
        let persist_res: Result<(), StoreError> = match &exec_plan.mode {
            ExecMode::ManifestVerify { flow_rel } => {
                if final_passed {
                    let Some(_manifest_guard) =
                        acquire_manifest_lock_cancellable(&ctx.manifest_mutex, &ctx.deps.cancel)
                            .await
                    else {
                        let rec = make_cancelled_goal_record(
                            &bg,
                            goal_t0.elapsed().as_millis() as u64,
                            &shard.device,
                            shard.store_id.as_deref(),
                            shard.runtime.as_deref(),
                            "cancelled while waiting for manifest lock",
                            &ctx.redaction,
                        );
                        store_goal_record(&ctx.results, bg.index, rec).await;
                        shard.goals_executed.fetch_add(1, Ordering::SeqCst);
                        break;
                    };
                    let cf = ctx.consumer.as_deref().expect("manifest mode");
                    cf.save_verified_manifest_walk(
                        &shard.store,
                        &SaveManifestWalkOptions {
                            identity: identity.clone(),
                            flow_path: flow_rel.clone(),
                            expected_closure_sha256: expected_closure_sha256.clone(),
                            expected_assertions_sha256: Some(expected_assertions_sha256.clone()),
                            validation: validation_input,
                            accept_assertion_change: false,
                        },
                    )
                    .map(|(_, changed)| {
                        manifest_written = changed;
                    })
                } else if exec_plan.closure_matches_manifest {
                    let cf = ctx.consumer.as_deref().expect("manifest mode");
                    cf.record_post_run_validation(
                        &shard.store,
                        &PostRunValidationOptions {
                            identity: identity.clone(),
                            expected_closure_sha256: expected_closure_sha256.clone(),
                            expected_assertions_sha256: expected_assertions_sha256.clone(),
                            validation: validation_input,
                        },
                    )
                    .map(|_| ())
                } else {
                    Ok(())
                }
            }
            ExecMode::ManifestHit => {
                let cf = ctx.consumer.as_deref().expect("manifest mode");
                cf.record_post_run_validation(
                    &shard.store,
                    &PostRunValidationOptions {
                        identity: identity.clone(),
                        expected_closure_sha256: expected_closure_sha256.clone(),
                        expected_assertions_sha256: expected_assertions_sha256.clone(),
                        validation: validation_input,
                    },
                )
                .map(|_| ())
            }
            ExecMode::ManifestReplayInPlace { .. } => {
                // Always persist pass, flaky, AND consistent failure outcomes against the verified
                // manifest closure in this shard's store (creating the shard's `walks` row if this
                // is the first time the goal ran on this shard) so multi-store `deja gate` never
                // ignores a failure on shard B because shard A had a prior passing validation.
                let cf = ctx.consumer.as_deref().expect("manifest mode");
                cf.record_post_run_validation(
                    &shard.store,
                    &PostRunValidationOptions {
                        identity: identity.clone(),
                        expected_closure_sha256: expected_closure_sha256.clone(),
                        expected_assertions_sha256: expected_assertions_sha256.clone(),
                        validation: validation_input,
                    },
                )
                .map(|_| {
                    recorded_for_new_build = final_passed;
                })
            }
            ExecMode::ClassicHit => shard
                .store
                .record_validation(&identity, &validation_input)
                .map(|_| ()),
            ExecMode::ClassicStaleBuildReplay {
                stored_flow_path,
                stored_flow_sha256,
                stored_subflow_files,
            } => {
                if final_passed {
                    match verify_unchanged_closure(
                        &ctx.config.workspace_root,
                        &exec_plan.flow_abs_path,
                        &expected_closure_sha256,
                        &expected_assertions_sha256,
                    ) {
                        Ok(()) => shard
                            .store
                            .upsert_walk(&UpsertWalkInput {
                                identity: identity.clone(),
                                flow_path: stored_flow_path.clone(),
                                flow_sha256: stored_flow_sha256.clone(),
                                subflow_files: stored_subflow_files.clone(),
                                legacy_closure: false,
                                closure_sha256: None,
                                validation: validation_input,
                                exploration: None,
                                repair_history: None,
                            })
                            .map(|_| {
                                recorded_for_new_build = true;
                            }),
                        Err(msg) => Err(StoreError::ManifestChanged(msg)),
                    }
                } else {
                    Ok(())
                }
            }
        };

        let store_ms = store_t0.elapsed().as_millis() as u64;
        timing.store_ms = store_ms;
        timing.total_ms = goal_t0.elapsed().as_millis() as u64;

        if let Err(err) = persist_res {
            let (outcome, miss) = match &err {
                StoreError::ManifestChanged(_)
                | StoreError::Core(CoreError::FlowChanged { .. }) => {
                    (RunOutcome::FlowChanged, Some(CacheMissReason::FlowChanged))
                }
                StoreError::Core(CoreError::AssertionHashChanged { .. }) => (
                    RunOutcome::AssertionsChanged,
                    Some(CacheMissReason::FlowChanged),
                ),
                _ => {
                    let code = exit_code_for_store_error(&err);
                    let rec = make_error_goal_record(
                        &bg,
                        code,
                        &err.to_string(),
                        Some(shard.device.clone()),
                        shard.store_id.clone(),
                        shard.runtime.clone(),
                        err.code(),
                        &ctx.redaction,
                    );
                    store_goal_record(&ctx.results, bg.index, rec).await;
                    shard.goals_executed.fetch_add(1, Ordering::SeqCst);
                    continue;
                }
            };
            let mut report = build_base_report(
                RunCommand::Run,
                identity,
                &bg.goal,
                exec_plan.cache_status,
                miss,
                outcome,
                false,
                goal_started_at,
                now_iso8601(),
                timing.total_ms,
                shard.runtime.clone(),
            );
            report.core.yaml_path = Some(exec_plan.display_flow_path);
            report.core.yaml_sha256 = Some(main_sha256);
            report.core.assertions = assertion_count;
            report
                .core
                .errors
                .push(ctx.redaction.redact_str(&err.to_string()));
            report.timing = Some(timing);
            report.selection_reason = bg.selection_reason;
            persist_per_goal_internal_report(
                &ctx.config.workspace_root,
                shard.store_id.as_deref(),
                &immutable_report_rel,
                &report,
                &ctx.redaction,
            );
            let rec = make_report_goal_record(
                &bg,
                report,
                Some(immutable_report_rel),
                &shard.device,
                shard.store_id.as_deref(),
                shard.runtime.as_deref(),
                &ctx.redaction,
            );
            store_goal_record(&ctx.results, bg.index, rec).await;
            shard.goals_executed.fetch_add(1, Ordering::SeqCst);
            continue;
        }

        // Build full per-goal schema-1 `CliRunReport`.
        let mut report = build_base_report(
            RunCommand::Run,
            identity,
            &bg.goal,
            exec_plan.cache_status,
            exec_plan.miss_reason,
            final_outcome,
            final_passed,
            goal_started_at,
            now_iso8601(),
            timing.total_ms,
            shard.runtime.clone(),
        );
        report.core.yaml_path = Some(exec_plan.display_flow_path.clone());
        report.core.yaml_sha256 = Some(main_sha256.clone());
        report.core.assertions = assertion_count;
        report.core.failure = if final_passed {
            None
        } else {
            first.failure.clone()
        };
        report.core.execution = Some(to_execution_result(first, Some(&closure)));
        report.core.errors = first.errors.clone();
        report.core.artifacts.push(RunArtifact {
            kind: ArtifactKind::Yaml,
            path: exec_plan.display_flow_path,
            sha256: Some(main_sha256),
        });
        if manifest_written && let Some(ref manifest_rel) = ctx.config.manifest {
            report.core.artifacts.push(RunArtifact {
                kind: ArtifactKind::Manifest,
                path: manifest_rel.clone(),
                sha256: None,
            });
        }
        report.core.artifacts.extend(first.artifacts.clone());

        if attempts_raw.len() >= 2 {
            report.core.attempts = Some(
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
            );
        }

        if let Some(mut nb) = exec_plan.new_build_replay {
            nb.recorded_for_new_build = recorded_for_new_build;
            report.core.replayed_on_new_build = Some(nb);
        }

        report.timing = Some(timing);
        report.visible_context = if final_passed {
            None
        } else {
            first.visible_context.clone()
        };
        report.selection_reason = bg.selection_reason;

        persist_per_goal_internal_report(
            &ctx.config.workspace_root,
            shard.store_id.as_deref(),
            &immutable_report_rel,
            &report,
            &ctx.redaction,
        );

        let rec = make_report_goal_record(
            &bg,
            report,
            Some(immutable_report_rel),
            &shard.device,
            shard.store_id.as_deref(),
            shard.runtime.as_deref(),
            &ctx.redaction,
        );
        store_goal_record(&ctx.results, bg.index, rec).await;
        shard.goals_executed.fetch_add(1, Ordering::SeqCst);
    }

    // Guaranteed teardown: ALWAYS clear attaching state, shut down persistent driver, and close shard store.
    clear_attaching_shard(&ctx, shard.shard_idx).await;
    if let Some(driver) = persistent_driver.take() {
        let _ = driver.shutdown().await;
    }
    shard.store.close();
}

async fn store_goal_record(
    results: &AsyncMutex<Vec<Option<BatchGoalOutcomeRecord>>>,
    index: usize,
    rec: BatchGoalOutcomeRecord,
) {
    let mut guard = results.lock().await;
    guard[index] = Some(rec);
}

fn persist_per_goal_internal_report(
    workspace_root: &Path,
    store_id: Option<&str>,
    immutable_rel_path: &str,
    report: &CliRunReport,
    redaction: &RedactionContext,
) {
    if let Ok(redacted_json) = redaction.redact_report(report) {
        let _ =
            write_internal_reports(workspace_root, store_id, immutable_rel_path, &redacted_json);
    }
}

fn make_report_goal_record(
    bg: &BatchGoal,
    report: CliRunReport,
    report_path: Option<String>,
    device: &str,
    store_id: Option<&str>,
    runtime: Option<&str>,
    redaction: &RedactionContext,
) -> BatchGoalOutcomeRecord {
    let outcome_str = outcome_wire_name(report.core.outcome).to_string();
    let passed = report.core.passed;
    let exit_code = exit_code_for_outcome(report.core.outcome);
    let duration_ms = report.core.duration_ms;

    let mut detail_parts = Vec::new();
    if let Some(ref failure) = report.core.failure {
        detail_parts.push(format!(
            "label: {}",
            failure_category_wire(failure.category)
        ));
        if let Some(ref step) = failure.failing_step {
            detail_parts.push(format!("step: {step}"));
        }
    }
    if let Some(ref vc) = report.visible_context {
        detail_parts.push(vc.format_summary());
    }
    for err in &report.core.errors {
        detail_parts.push(err.clone());
    }
    let detail = if detail_parts.is_empty() {
        None
    } else {
        Some(detail_parts.join("; "))
    };

    let attempts = report
        .core
        .attempts
        .as_ref()
        .map(|list| list.iter().map(BatchJunitAttemptSummary::from).collect())
        .unwrap_or_default();

    let junit_case = BatchJunitCase {
        goal: bg.goal.text.clone(),
        status: if passed {
            BatchJunitCaseStatus::Passed
        } else {
            BatchJunitCaseStatus::Failed
        },
        outcome: outcome_str.clone(),
        duration_ms,
        detail,
        device: Some(device.to_string()),
        error_type: None,
        attempts,
    };

    let errors = report.core.errors.clone();
    let safe_goal_key = redaction.redact_goal_key(&bg.goal.text);

    BatchGoalOutcomeRecord {
        result: BatchGoalResult {
            index: bg.index,
            goal: bg.goal.text.clone(),
            goal_key: safe_goal_key,
            outcome: outcome_str,
            passed,
            exit_code,
            duration_ms,
            device: Some(device.to_string()),
            store: store_id.map(ToString::to_string),
            runtime: runtime.map(ToString::to_string),
            report_path,
            historical_median_ms: bg.historical_median_ms,
            selection_reason: bg.selection_reason,
            report: Some(report),
            errors,
        },
        junit_case,
    }
}

fn make_cancelled_goal_record(
    bg: &BatchGoal,
    duration_ms: u64,
    device: &str,
    store_id: Option<&str>,
    runtime: Option<&str>,
    detail: &str,
    redaction: &RedactionContext,
) -> BatchGoalOutcomeRecord {
    let safe_detail = redaction.redact_str(detail);
    let safe_goal_key = redaction.redact_goal_key(&bg.goal.text);
    BatchGoalOutcomeRecord {
        result: BatchGoalResult {
            index: bg.index,
            goal: bg.goal.text.clone(),
            goal_key: safe_goal_key,
            outcome: "cancelled".to_string(),
            passed: false,
            exit_code: EXIT_FAILURE,
            duration_ms,
            device: Some(device.to_string()),
            store: store_id.map(ToString::to_string),
            runtime: runtime.map(ToString::to_string),
            report_path: None,
            historical_median_ms: bg.historical_median_ms,
            selection_reason: bg.selection_reason,
            report: None,
            errors: vec![safe_detail.clone()],
        },
        junit_case: BatchJunitCase {
            goal: bg.goal.text.clone(),
            status: BatchJunitCaseStatus::Errored,
            outcome: "cancelled".to_string(),
            duration_ms,
            detail: Some(safe_detail),
            device: Some(device.to_string()),
            error_type: Some("Cancelled".to_string()),
            attempts: Vec::new(),
        },
    }
}

fn make_skipped_goal_record(
    bg: &BatchGoal,
    redaction: &RedactionContext,
) -> BatchGoalOutcomeRecord {
    let safe_goal_key = redaction.redact_goal_key(&bg.goal.text);
    let reason = "cancelled before execution".to_string();
    BatchGoalOutcomeRecord {
        result: BatchGoalResult {
            index: bg.index,
            goal: bg.goal.text.clone(),
            goal_key: safe_goal_key,
            outcome: "skipped".to_string(),
            passed: false,
            exit_code: EXIT_FAILURE,
            duration_ms: 0,
            device: None,
            store: None,
            runtime: None,
            report_path: None,
            historical_median_ms: bg.historical_median_ms,
            selection_reason: bg.selection_reason,
            report: None,
            errors: vec![reason.clone()],
        },
        junit_case: BatchJunitCase {
            goal: bg.goal.text.clone(),
            status: BatchJunitCaseStatus::Skipped,
            outcome: "skipped".to_string(),
            duration_ms: 0,
            detail: Some(reason),
            device: None,
            error_type: None,
            attempts: Vec::new(),
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn make_error_goal_record(
    bg: &BatchGoal,
    exit_code: u8,
    detail: &str,
    device: Option<String>,
    store_id: Option<String>,
    runtime: Option<String>,
    error_type: &str,
    redaction: &RedactionContext,
) -> BatchGoalOutcomeRecord {
    let safe_detail = redaction.redact_str(detail);
    let safe_goal_key = redaction.redact_goal_key(&bg.goal.text);
    BatchGoalOutcomeRecord {
        result: BatchGoalResult {
            index: bg.index,
            goal: bg.goal.text.clone(),
            goal_key: safe_goal_key,
            outcome: "error".to_string(),
            passed: false,
            exit_code,
            duration_ms: 0,
            device: device.clone(),
            store: store_id,
            runtime,
            report_path: None,
            historical_median_ms: bg.historical_median_ms,
            selection_reason: bg.selection_reason,
            report: None,
            errors: vec![safe_detail.clone()],
        },
        junit_case: BatchJunitCase {
            goal: bg.goal.text.clone(),
            status: BatchJunitCaseStatus::Errored,
            outcome: "error".to_string(),
            duration_ms: 0,
            detail: Some(safe_detail),
            device,
            error_type: Some(error_type.to_string()),
            attempts: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_to_store_id_preserves_valid_and_sanitizes_complex_ids() {
        assert_eq!(device_to_store_id("d1"), "d1");
        assert_eq!(
            device_to_store_id("11111111-2222-3333-4444-555555555555"),
            "11111111-2222-3333-4444-555555555555"
        );

        let a = device_to_store_id("127.0.0.1:5555");
        let b = device_to_store_id("127.0.0.1/5555");
        assert_ne!(a, b);
        assert!(validate_store_id(&a).is_ok());
        assert!(validate_store_id(&b).is_ok());
        assert!(a.len() <= 128 && b.len() <= 128);

        let leading_punct = device_to_store_id(":::weird/device:::");
        assert!(validate_store_id(&leading_punct).is_ok());

        let very_long = "a".repeat(200) + ":port";
        let shortened = device_to_store_id(&very_long);
        assert!(shortened.len() <= 128);
        assert!(validate_store_id(&shortened).is_ok());
    }

    #[test]
    fn compute_batch_exit_code_enforces_exact_priority() {
        assert_eq!(compute_batch_exit_code(true, 15, true, &[4, 5, 1]), 143);
        assert_eq!(compute_batch_exit_code(true, 2, true, &[4, 5, 1]), 130);
        assert_eq!(compute_batch_exit_code(true, 0, true, &[4, 5, 1]), 1);
        assert_eq!(compute_batch_exit_code(false, 0, true, &[4, 5, 1, 0]), 2);
        assert_eq!(
            compute_batch_exit_code(false, 0, false, &[7, 6, 3, 1, 2, 5, 4]),
            4
        );
        assert_eq!(
            compute_batch_exit_code(false, 0, false, &[7, 6, 3, 1, 2, 5]),
            5
        );
        assert_eq!(
            compute_batch_exit_code(false, 0, false, &[7, 6, 3, 1, 2]),
            2
        );
        assert_eq!(compute_batch_exit_code(false, 0, false, &[7, 6, 3, 1]), 1);
        assert_eq!(compute_batch_exit_code(false, 0, false, &[7, 6, 3]), 3);
        assert_eq!(compute_batch_exit_code(false, 0, false, &[7, 6]), 6);
        assert_eq!(compute_batch_exit_code(false, 0, false, &[7, 0]), 7);
        assert_eq!(compute_batch_exit_code(false, 0, false, &[0, 0]), 0);
    }

    #[tokio::test]
    async fn acquire_manifest_lock_cancellable_aborts_on_pre_cancel_and_while_waiting() {
        // 1. Pre-cancelled token on an UNCONTENDED mutex must immediately return `None` (testing
        //    both biased select and the post-acquire cancellation recheck).
        let uncontended = Arc::new(AsyncMutex::new(()));
        let pre_cancel = CancellationToken::new();
        pre_cancel.cancel();
        assert!(
            acquire_manifest_lock_cancellable(&uncontended, &pre_cancel)
                .await
                .is_none()
        );

        // 2. Contended mutex held by another task: cancelling unblocks immediately WITHOUT waiting
        //    for the holder to release the lock.
        let contended = Arc::new(AsyncMutex::new(()));
        let held = contended.lock().await;
        let cancel = CancellationToken::new();
        let cancel_bg = cancel.clone();
        let waiter_mutex = Arc::clone(&contended);
        let waiter = tokio::spawn(async move {
            acquire_manifest_lock_cancellable(&waiter_mutex, &cancel_bg)
                .await
                .is_some()
        });
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        cancel.cancel();
        let acquired = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("waiting on manifest_mutex must unblock promptly on cancel")
            .unwrap();
        assert!(!acquired);
        drop(held);
    }

    #[tokio::test]
    async fn regression_deja66_r2_f3_held_manifest_mutex_cancel_writes_no_sqlite_or_manifest() {
        use std::collections::BTreeMap;
        use std::fs;
        use std::sync::Mutex as StdMutex;
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        use deja_core::{
            DEFAULT_FLOW_SCHEMA_VERSION, MANIFEST_HASH_VERSION, ManifestWalk,
            WALK_MANIFEST_VERSION, WalkIdentity, WalkManifest, collect_flow_closure,
            fingerprint_file, serialize_walk_manifest,
        };
        use deja_flow::{
            DriverCapabilities, DriverCapability, DriverCrashSignal, DriverFuture, KeyPress,
            LaunchOptions, OpenLinkOptions, PlatformDriver, SwipeParams, UiHierarchy, UiNode,
            UiPoint, UiRect, UiSize,
        };
        use tokio::sync::oneshot;

        use crate::driver_factory::{
            DoctorCheck, DoctorProbeRequest, DoctorProbes, DriverFactory, FactoryFuture,
        };
        use crate::output::OutputSink;

        struct NoopProbes;
        impl DoctorProbes for NoopProbes {
            fn probe<'a>(
                &'a self,
                _req: &'a DoctorProbeRequest,
            ) -> FactoryFuture<'a, Vec<DoctorCheck>> {
                Box::pin(async { Vec::new() })
            }
        }

        struct NotifyPassDriver {
            caps: DriverCapabilities,
            on_first_observe: Arc<StdMutex<Option<oneshot::Sender<()>>>>,
        }
        impl PlatformDriver for NotifyPassDriver {
            fn platform(&self) -> &'static str {
                "ios"
            }
            fn capabilities(&self) -> &DriverCapabilities {
                &self.caps
            }
            fn ensure_session<'a>(
                &'a self,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn launch_app<'a>(
                &'a self,
                _app_id: &'a str,
                _opts: &'a LaunchOptions,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn stop_app<'a>(
                &'a self,
                _app_id: &'a str,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn kill_app<'a>(
                &'a self,
                _app_id: &'a str,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn clear_state<'a>(
                &'a self,
                _app_id: &'a str,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn clear_keychain<'a>(
                &'a self,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn open_link<'a>(
                &'a self,
                _app_id: Option<&'a str>,
                _url: &'a str,
                _opts: &'a OpenLinkOptions,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn add_media<'a>(
                &'a self,
                _paths: &'a [PathBuf],
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn hierarchy<'a>(
                &'a self,
                app_id: &'a str,
                _include_system_ui: bool,
                _known_hash: Option<&'a str>,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, UiHierarchy> {
                let tx = self.on_first_observe.lock().unwrap().take();
                let app = app_id.to_string();
                Box::pin(async move {
                    if let Some(tx) = tx {
                        let _ = tx.send(());
                    }
                    let node = UiNode {
                        identifier: "ok".to_string(),
                        label: "Done".to_string(),
                        title: "Done".to_string(),
                        hittable: true,
                        frame: UiRect::new(10.0, 10.0, 100.0, 44.0),
                        ..UiNode::default()
                    };
                    Ok(UiHierarchy {
                        app_id: app,
                        state: "foreground".to_string(),
                        viewport: Some(UiSize::new(390.0, 844.0)),
                        truncated: false,
                        hierarchy_hash: String::new(),
                        unchanged: false,
                        nodes: vec![node],
                    })
                })
            }
            fn tap_point<'a>(
                &'a self,
                _app_id: &'a str,
                _point: UiPoint,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn long_press_point<'a>(
                &'a self,
                _app_id: &'a str,
                _point: UiPoint,
                _duration_ms: Option<u64>,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn double_tap_point<'a>(
                &'a self,
                _app_id: &'a str,
                _point: UiPoint,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn swipe<'a>(
                &'a self,
                _params: &'a SwipeParams,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn input_text<'a>(
                &'a self,
                _app_id: &'a str,
                _text: &'a str,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn erase_text<'a>(
                &'a self,
                _app_id: &'a str,
                _count: u32,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn press_key<'a>(
                &'a self,
                _app_id: &'a str,
                _key: &'a KeyPress,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn hide_keyboard<'a>(
                &'a self,
                _app_id: &'a str,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn back<'a>(
                &'a self,
                _app_id: &'a str,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn take_screenshot<'a>(
                &'a self,
                _app_id: &'a str,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, Vec<u8>> {
                Box::pin(async { Ok(Vec::new()) })
            }
            fn check_app_crash<'a>(
                &'a self,
                _app_id: &'a str,
                _since_epoch_ms: u64,
                _cancel: &'a CancellationToken,
            ) -> DriverFuture<'a, Option<DriverCrashSignal>> {
                Box::pin(async { Ok(None) })
            }
            fn shutdown<'a>(&'a self) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
        }

        struct NotifyFactory {
            on_first_observe: Arc<StdMutex<Option<oneshot::Sender<()>>>>,
        }
        impl DriverFactory for NotifyFactory {
            fn resolve_runtime<'a>(
                &'a self,
                _ctx: &'a DriverContext,
                _cancel: &'a CancellationToken,
            ) -> FactoryFuture<'a, Result<Option<String>, DriverFactoryError>> {
                Box::pin(async { Ok(Some("ios-27.2".to_string())) })
            }
            fn create<'a>(
                &'a self,
                _ctx: &'a DriverContext,
                _cancel: &'a CancellationToken,
            ) -> FactoryFuture<'a, Result<Box<dyn PlatformDriver>, DriverFactoryError>>
            {
                let slot = Arc::clone(&self.on_first_observe);
                Box::pin(async move {
                    Ok(Box::new(NotifyPassDriver {
                        caps: DriverCapabilities::with([
                            DriverCapability::AccessibilityTree,
                            DriverCapability::AppCrashReports,
                        ]),
                        on_first_observe: slot,
                    }) as Box<dyn PlatformDriver>)
                })
            }
            fn install_app<'a>(
                &'a self,
                _ctx: &'a DriverContext,
                _cancel: &'a CancellationToken,
            ) -> FactoryFuture<'a, Result<(), DriverFactoryError>> {
                Box::pin(async { Ok(()) })
            }
        }

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "deja-r2f3-manifest-mutex-{}-{nanos:x}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("flows")).unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join("build/App.bin"), b"build-v1").unwrap();
        fs::write(
            root.join("flows/g1.yaml"),
            "appId: com.example.r2f3\n---\n- assertVisible: Done\n",
        )
        .unwrap();
        let cl = collect_flow_closure(&root, &root.join("flows/g1.yaml")).unwrap();
        let orig_assertions = assertions_sha256(&cl).unwrap();
        // Put a DIFFERENT closure hash in `deja.walks.json` (same assertions) so `--verify` would
        // rewrite `deja.walks.json` if `save_verified_manifest_walk` were ever called!
        let stale_manifest = WalkManifest {
            manifest_version: WALK_MANIFEST_VERSION,
            hash_version: MANIFEST_HASH_VERSION,
            walks: vec![ManifestWalk {
                goal: "g1".to_string(),
                app_id: "com.example.r2f3".to_string(),
                platform: "ios".to_string(),
                adapter: "maestro-ios/v1".to_string(),
                browser: None,
                flow: "flows/g1.yaml".to_string(),
                closure_sha256: "0".repeat(64),
                assertions_sha256: orig_assertions,
                owner_rules: None,
                profile: None,
            }],
        };
        let manifest_before = serialize_walk_manifest(&stale_manifest).unwrap();
        fs::write(root.join("deja.walks.json"), &manifest_before).unwrap();
        fs::write(
            root.join("deja.config.json"),
            serde_json::json!({
                "workspaceRoot": ".",
                "platform": "ios",
                "appId": "com.example.r2f3",
                "adapter": "maestro-ios/v1",
                "device": "00000000-0000-4000-8000-000000000001",
                "buildPath": "build/App.bin",
                "manifest": "deja.walks.json"
            })
            .to_string(),
        )
        .unwrap();

        let (obs_tx, obs_rx) = oneshot::channel();
        let factory = Arc::new(NotifyFactory {
            on_first_observe: Arc::new(StdMutex::new(Some(obs_tx))),
        });
        let (sink, captured) = OutputSink::captured();
        let cancel = CancellationToken::new();
        let deps = CliDeps {
            cwd: root.clone(),
            home_dir: Some(root.join(".home")),
            env: BTreeMap::new(),
            output: sink,
            driver_factory: factory,
            doctor_probes: Arc::new(NoopProbes),
            cancel: cancel.clone(),
            cancel_signal: Arc::new(std::sync::atomic::AtomicU8::new(0)),
        };
        let args = RunArgs {
            goal: Vec::new(),
            goals: None,
            all: true,
            devices: Vec::new(),
            changed_from: None,
            always_include: Vec::new(),
            fallback_all_on: Vec::new(),
            verify: true,
            flow: None,
            device: None,
            store: None,
            junit: None,
            report: None,
            summary: false,
            rerun_failed: false,
            retries: None,
        };

        // Hold `manifest_mutex` BEFORE starting `run_batch_inner` so the worker blocks right at
        // `acquire_manifest_lock_cancellable` after executing `g1`!
        let manifest_mutex = Arc::new(AsyncMutex::new(()));
        let held_guard = manifest_mutex.lock().await;

        let batch_fut = run_batch_inner(
            &args,
            None,
            true,
            &deps,
            None,
            None,
            None,
            Arc::clone(&manifest_mutex),
        );
        let cancel_trigger = cancel.clone();
        let canceller = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(3), obs_rx)
                .await
                .expect("driver hierarchy observe timed out")
                .expect("driver observe must run");
            // Brief settle so worker reaches `manifest_mutex.lock()` before cancel fires.
            tokio::time::sleep(Duration::from_millis(30)).await;
            cancel_trigger.cancel();
        });

        let code = tokio::time::timeout(Duration::from_secs(5), batch_fut)
            .await
            .expect(
                "worker waiting on manifest_mutex must cancel without waiting for guard release",
            );
        canceller.await.unwrap();
        drop(held_guard);

        assert_eq!(code, 1, "cancelled verify batch must exit 1");
        let stdout = captured.lock().unwrap().stdout.clone();
        let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(parsed["cancelled"], true);
        assert_eq!(parsed["goals"][0]["outcome"], "cancelled");

        // Manifest MUST be untouched (`"0" * 64` still intact)!
        assert_eq!(
            fs::read_to_string(root.join("deja.walks.json")).unwrap(),
            manifest_before,
            "cancelled worker waiting on manifest mutex must NOT update deja.walks.json"
        );
        // SQLite MUST contain zero validation runs for `g1`!
        let fp = fingerprint_file(&root, &root.join("build/App.bin")).unwrap();
        let ro = SqliteWalkStore::open_readonly(SqliteStoreOptions::new(&root)).unwrap();
        let runs = ro
            .list_validation_runs(&WalkIdentity {
                goal: "g1".to_string(),
                app_id: "com.example.r2f3".to_string(),
                platform: "ios".to_string(),
                flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
                build_fingerprint: fp,
                adapter: Some("maestro-ios/v1".to_string()),
                browser: None,
            })
            .unwrap();
        assert!(
            runs.is_empty(),
            "cancelled worker waiting on manifest mutex must NOT write validation_runs to SQLite"
        );
        assert!(
            !root.join(".deja/memory/walks.sqlite.lock").exists(),
            "store lock must be cleaned up"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn regression_deja66_r3_f4_duration_scan_cancellable_aborts_and_releases_locks() {
        use std::fs;
        use std::time::{SystemTime, UNIX_EPOCH};

        use deja_core::{DEFAULT_FLOW_SCHEMA_VERSION, WalkIdentity, collect_flow_closure};

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "deja-r3f4-dur-scan-{}-{nanos:x}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("flows")).unwrap();
        let flow = root.join("flows/g.yaml");
        fs::write(&flow, "appId: com.example.r3f4\n---\n- assertVisible: Ok\n").unwrap();
        let cl = collect_flow_closure(&root, &flow).unwrap();
        let main_sha = cl
            .files
            .iter()
            .find(|f| f.path == cl.main)
            .unwrap()
            .sha256
            .clone();

        let s1 = Arc::new(
            SqliteWalkStore::open_writable(SqliteStoreOptions::new(&root).with_store_id("d1"))
                .unwrap(),
        );
        for i in 0..20u64 {
            s1.upsert_walk(&UpsertWalkInput {
                identity: WalkIdentity {
                    goal: format!("goal {i}"),
                    app_id: "com.example.r3f4".to_string(),
                    platform: "ios".to_string(),
                    flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
                    build_fingerprint: format!("{i:064x}"),
                    adapter: Some("maestro-ios/v1".to_string()),
                    browser: None,
                },
                flow_path: "flows/g.yaml".to_string(),
                flow_sha256: main_sha.clone(),
                subflow_files: None,
                legacy_closure: false,
                closure_sha256: Some(cl.sha256.clone()),
                validation: RecordValidationInput::new(true, now_iso8601())
                    .with_duration_ms(100 + i),
                exploration: None,
                repair_history: None,
            })
            .unwrap();
        }
        let stores = vec![Arc::clone(&s1)];

        // 1. Pre-cancelled scan returns `Err(None)` promptly.
        let pre_cancel = CancellationToken::new();
        pre_cancel.cancel();
        let res = scan_duration_medians_cancellable(&stores, &pre_cancel).await;
        assert!(matches!(res, Err(None)));

        // 2. Uncancelled scan succeeds with exact medians.
        let ok_cancel = CancellationToken::new();
        let medians = scan_duration_medians_cancellable(&stores, &ok_cancel)
            .await
            .expect("uncancelled scan succeeds");
        assert_eq!(medians.get("goal 0"), Some(&100));
        assert_eq!(medians.get("goal 19"), Some(&119));

        close_all_stores(&stores);
        assert!(
            !root
                .join(".deja/memory/stores/d1/walks.sqlite.lock")
                .exists(),
            "closing stores after cancelled duration scan must leave zero lock files"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
