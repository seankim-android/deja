//! Multi-goal batch execution (`deja run --goals <FILE>` / `deja run --all`), dynamic
//! `--devices` LPT sharding, single build-fingerprint broadcast (`DEJA-53`), shared staged `.app`
//! reuse, persistent per-shard driver reuse, merged redacted JUnit output, and graceful signal
//! cancellation (`DEJA-66`).

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashSet, VecDeque};
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
use tokio::sync::{Mutex as AsyncMutex, OnceCell};

use crate::CliDeps;
use crate::cli::RunArgs;
use crate::commands::exec_common::{
    ExecutedAttempt, allocate_batch_report_rel_path, allocate_report_rel_path, build_base_report,
    build_consumer_flows, build_driver_context, check_closure_forbidden_values,
    emit_early_exec_error, execute_single_attempt, failure_run_outcome, make_walk_identity,
    parse_goals_file, to_execution_result, validate_closure_app_id_confinement,
    validate_explicit_output_path, write_internal_batch_reports, write_internal_reports,
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
    BatchCounts, BatchGoalResult, BatchShardSummary, CliBatchRunReport, CliRunReport,
    RedactionContext, emit_summary, failure_category_wire, format_batch_status_summary_line,
    format_summary_line, outcome_wire_name, prepare_safe_junit, print_human_batch_report,
    workspace_relative_display, write_batch_junit,
};

#[derive(Debug, Clone)]
struct ShardSpec {
    device: String,
    store_id: Option<String>,
}

struct OpenedShard {
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
    if let Some(ref jpath) = effective_junit {
        if let Err(msg) =
            validate_explicit_output_path(jpath, "--junit", &config, None, &extra_protected_refs)
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
        if cli_junit.is_none()
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
    }
    if let Some(ref rpath) = explicit_report
        && let Err(msg) =
            validate_explicit_output_path(rpath, "--report", &config, None, &extra_protected_refs)
    {
        return emit_early_exec_error(
            deps,
            "run",
            "batch",
            effective_junit.as_deref(),
            json_mode,
            args.summary,
            &redaction,
            "REPORT_PATH_REFUSED",
            &msg,
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

    // Resolve shard device list and per-device isolated store IDs.
    let shard_specs = match resolve_shard_specs(args, &config) {
        Ok(specs) => specs,
        Err(msg) => {
            return emit_early_exec_error(
                deps,
                "run",
                "batch",
                effective_junit.as_deref(),
                json_mode,
                args.summary,
                &redaction,
                "INVALID_DEVICES",
                &msg,
                EXIT_USAGE_OR_CONFIG,
            );
        }
    };

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

    // Load & validate batch goals (from `--goals <FILE>` or filtered manifest `--all`).
    let raw_goals = if let Some(ref gpath) = goals_abs {
        match parse_goals_file(gpath) {
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
            .filter(|entry| {
                entry.app_id == config.app_id
                    && entry.platform == config.platform
                    && Some(entry.adapter.as_str()) == config.adapter.as_deref()
                    && entry.browser.as_deref() == config.browser.as_deref()
            })
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
    let immutable_batch_rel = allocate_batch_report_rel_path();

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

    // Compute `build_fingerprint` ONCE via `stores[0]` and broadcast read-only (`Arc<String>`).
    let build_path = config
        .build_path
        .as_ref()
        .expect("Execute capability requires build_path");
    let fp_t0 = Instant::now();
    let fp_res = match opened_stores[0].fingerprint_with_cache(
        &config.workspace_root,
        build_path,
        &config.platform,
    ) {
        Ok(r) => r,
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
                &format!("failed to fingerprint build: {err}"),
                code,
            );
        }
    };
    let build_fingerprint_ms = fp_t0.elapsed().as_millis() as u64;
    let build_fingerprint = Arc::new(fp_res.digest);

    // Read integrity-verified historical medians from EVERY shard store and aggregate per goal.
    let mut shard_medians_by_goal: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for store in &opened_stores {
        match store.median_goal_durations_ms() {
            Ok(map) => {
                for (goal_key, med_ms) in map {
                    shard_medians_by_goal
                        .entry(goal_key)
                        .or_default()
                        .push(med_ms);
                }
            }
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
    let mut aggregated_medians: BTreeMap<String, u64> = BTreeMap::new();
    for (goal_key, mut vals) in shard_medians_by_goal {
        if let Some(med) = integer_median(&mut vals) {
            aggregated_medians.insert(goal_key, med);
        }
    }

    // Build ordered input goals + LPT work queue: `(Reverse(historical_median_ms), index)`.
    let input_goals: Vec<BatchGoal> = normalized_goals
        .into_iter()
        .enumerate()
        .map(|(index, goal)| {
            let historical_median_ms = aggregated_medians.get(&goal.key).copied();
            BatchGoal {
                index,
                goal,
                historical_median_ms,
            }
        })
        .collect();

    let mut lpt_sorted = input_goals.clone();
    lpt_sorted.sort_by_key(|g| (Reverse(g.historical_median_ms), g.index));
    let scheduled_order: Vec<String> = lpt_sorted.iter().map(|g| g.goal.text.clone()).collect();

    // Resolve runtime ONCE per shard via `driver_factory.resolve_runtime`.
    let mut opened_shards: Vec<OpenedShard> = Vec::with_capacity(shard_specs.len());
    let mut cancelled_during_setup = deps.cancel.is_cancelled();
    if !cancelled_during_setup {
        for (spec, store) in shard_specs.iter().zip(opened_stores.iter()) {
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
                    close_all_stores(&opened_stores);
                    let code = err.exit_code();
                    return emit_early_exec_error(
                        deps,
                        "run",
                        "batch",
                        effective_junit.as_deref(),
                        json_mode,
                        args.summary,
                        &redaction,
                        "DRIVER_ERROR",
                        &err.to_string(),
                        code,
                    );
                }
            }
        }
    }

    let total_goals = input_goals.len();
    let results: Arc<AsyncMutex<Vec<Option<BatchGoalOutcomeRecord>>>> =
        Arc::new(AsyncMutex::new(vec![None; total_goals]));
    let refused_junit = Arc::new(AtomicBool::new(false));
    let refused_report = Arc::new(AtomicBool::new(false));
    let last_attach_error: Arc<AsyncMutex<Option<(u8, String)>>> = Arc::new(AsyncMutex::new(None));

    if cancelled_during_setup {
        close_all_stores(&opened_stores);
    } else {
        let queue: Arc<AsyncMutex<VecDeque<BatchGoal>>> =
            Arc::new(AsyncMutex::new(VecDeque::from(lpt_sorted)));
        let shared_staged: Arc<OnceCell<Option<SharedStagedBuild>>> = Arc::new(OnceCell::new());
        let manifest_mutex: Arc<AsyncMutex<()>> = Arc::new(AsyncMutex::new(()));
        let extra_protected_arc = Arc::new(extra_protected_owned);

        let max_extra_retries = args
            .retries
            .map(usize::from)
            .or(if args.rerun_failed { Some(1) } else { None })
            .unwrap_or_else(|| usize::from(config.retries));

        let mut handles = Vec::with_capacity(opened_shards.len());
        for shard in &opened_shards {
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
                extra_protected: Arc::clone(&extra_protected_arc),
                queue: Arc::clone(&queue),
                shared_staged: Arc::clone(&shared_staged),
                manifest_mutex: Arc::clone(&manifest_mutex),
                results: Arc::clone(&results),
                refused_junit: Arc::clone(&refused_junit),
                refused_report: Arc::clone(&refused_report),
                last_attach_error: Arc::clone(&last_attach_error),
            };
            let worker_shard = WorkerShard {
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

    // Assemble deterministic `0..N` input-order results, marking any unstarted goals as skipped
    // (on cancellation) or errored (if all shards failed to attach).
    let mut locked_results = results.lock().await;
    let mut final_records: Vec<BatchGoalOutcomeRecord> = Vec::with_capacity(total_goals);
    for bg in &input_goals {
        if let Some(rec) = locked_results[bg.index].take() {
            final_records.push(rec);
        } else if cancelled {
            final_records.push(make_skipped_goal_record(bg, &redaction));
        } else {
            let (code, msg) = attach_fallback.clone().unwrap_or_else(|| {
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

    // Build shard summaries, counts, JUnit cases, and preliminary exit code.
    let shard_summaries: Vec<BatchShardSummary> = if opened_shards.is_empty() {
        shard_specs
            .iter()
            .zip(opened_stores.iter())
            .map(|(spec, store)| BatchShardSummary {
                device: spec.device.clone(),
                store: spec.store_id.clone(),
                store_label: store.store_label(),
                runtime: None,
                goals_executed: 0,
            })
            .collect()
    } else {
        opened_shards
            .iter()
            .map(|s| BatchShardSummary {
                device: s.device.clone(),
                store: s.store_id.clone(),
                store_label: s.store_label.clone(),
                runtime: s.runtime.clone(),
                goals_executed: s.goals_executed.load(Ordering::SeqCst),
            })
            .collect()
    };

    let mut counts = BatchCounts {
        total: final_records.len(),
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

    let batch_report = CliBatchRunReport {
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
        counts,
        goals: goal_results,
        artifacts: batch_artifacts,
        errors: batch_errors,
    };

    let redacted_json = match redaction.redact_batch_report(&batch_report) {
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

    if let Err(err) =
        write_internal_batch_reports(&config.workspace_root, &immutable_batch_rel, &redacted_json)
    {
        deps.output.writeln_stderr(&redaction.redact_str(&format!(
            "deja warning: failed to write batch report artifact: {err}"
        )));
    }

    if let Some(ref rpath) = explicit_report {
        if refused_report.load(Ordering::SeqCst) {
            let msg = redaction.redact_str(&format!(
                "refusing to write --report over a protected flow closure path: {}",
                rpath.display()
            ));
            deps.output.writeln_stderr(&format!("deja error: {msg}"));
            explicit_artifact_failed = true;
        } else {
            let mut text = serde_json::to_string_pretty(&redacted_json)
                .expect("redacted batch JSON serializes");
            text.push('\n');
            if let Err(err) = write_atomic_nofollow_excl(rpath, text.as_bytes()) {
                let msg = redaction.redact_str(&format!(
                    "failed to write explicit --report {}: {err}",
                    rpath.display()
                ));
                deps.output.writeln_stderr(&format!("deja error: {msg}"));
                explicit_artifact_failed = true;
            }
        }
    }

    final_exit = compute_batch_exit_code(
        cancelled,
        cancel_signal,
        explicit_artifact_failed,
        &goal_exit_codes,
    );

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
                format_summary_line(&rep.core, &redaction)
            } else {
                let failure_label = match entry.outcome.as_str() {
                    "cancelled" => "cancelled",
                    "skipped" => "skipped",
                    _ => "error",
                };
                format_batch_status_summary_line(
                    &entry.goal,
                    &entry.outcome,
                    failure_label,
                    &batch_report.build_fingerprint,
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
    device: String,
    store_id: Option<String>,
    store: Arc<SqliteWalkStore>,
    runtime: Option<String>,
    base_driver_ctx: DriverContext,
    goals_executed: Arc<AtomicUsize>,
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
    extra_protected: Arc<Vec<PathBuf>>,
    queue: Arc<AsyncMutex<VecDeque<BatchGoal>>>,
    shared_staged: Arc<OnceCell<Option<SharedStagedBuild>>>,
    manifest_mutex: Arc<AsyncMutex<()>>,
    results: Arc<AsyncMutex<Vec<Option<BatchGoalOutcomeRecord>>>>,
    refused_junit: Arc<AtomicBool>,
    refused_report: Arc<AtomicBool>,
    last_attach_error: Arc<AsyncMutex<Option<(u8, String)>>>,
}

async fn run_shard_worker(shard: WorkerShard, ctx: WorkerSharedContext) {
    let mut persistent_driver: Option<Box<dyn PlatformDriver>> = None;

    loop {
        if ctx.deps.cancel.is_cancelled() {
            break;
        }

        let maybe_goal = {
            let mut q = ctx.queue.lock().await;
            if ctx.deps.cancel.is_cancelled() {
                None
            } else {
                q.pop_front()
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
            && let Err(msg) = validate_explicit_output_path(
                jpath,
                "--junit",
                &ctx.config,
                Some(&closure),
                &extra_prot_refs,
            )
        {
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
            && let Err(msg) = validate_explicit_output_path(
                rpath,
                "--report",
                &ctx.config,
                Some(&closure),
                &extra_prot_refs,
            )
        {
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
                    let code = err.exit_code();
                    let msg = err.to_string();
                    *ctx.last_attach_error.lock().await = Some((code, msg.clone()));
                    let rec = make_error_goal_record(
                        &bg,
                        code,
                        &msg,
                        Some(shard.device.clone()),
                        shard.store_id.clone(),
                        shard.runtime.clone(),
                        "DriverError",
                        &ctx.redaction,
                    );
                    store_goal_record(&ctx.results, bg.index, rec).await;
                    shard.goals_executed.fetch_add(1, Ordering::SeqCst);
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
                    let code = err.exit_code();
                    let msg = err.to_string();
                    *ctx.last_attach_error.lock().await = Some((code, msg.clone()));
                    let rec = make_error_goal_record(
                        &bg,
                        code,
                        &msg,
                        Some(shard.device.clone()),
                        shard.store_id.clone(),
                        shard.runtime.clone(),
                        "DriverError",
                        &ctx.redaction,
                    );
                    store_goal_record(&ctx.results, bg.index, rec).await;
                    shard.goals_executed.fetch_add(1, Ordering::SeqCst);
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
                    *ctx.last_attach_error.lock().await = Some((code, msg.clone()));
                    let rec = make_error_goal_record(
                        &bg,
                        code,
                        &msg,
                        Some(shard.device.clone()),
                        shard.store_id.clone(),
                        shard.runtime.clone(),
                        "DriverError",
                        &ctx.redaction,
                    );
                    store_goal_record(&ctx.results, bg.index, rec).await;
                    shard.goals_executed.fetch_add(1, Ordering::SeqCst);
                }
                break;
            }

            driver_attach_ms = attach_t0.elapsed().as_millis() as u64;
            persistent_driver = Some(driver);
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

        let first_attempt = execute_single_attempt(
            driver_ref,
            &ctx.config,
            &closure,
            timeout_ms,
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
            for _ in 0..ctx.max_extra_retries {
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
                let retry_attempt = execute_single_attempt(
                    driver_ref,
                    &ctx.config,
                    &closure,
                    timeout_ms,
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
                    let _manifest_guard = ctx.manifest_mutex.lock().await;
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
            ExecMode::ManifestReplayInPlace { miss_reason } => {
                let cf = ctx.consumer.as_deref().expect("manifest mode");
                if final_passed || *miss_reason == CacheMissReason::FailedValidation || is_flaky {
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
                } else {
                    Ok(())
                }
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

    // Guaranteed teardown: ALWAYS shut down persistent driver and close shard store before exit.
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
}
