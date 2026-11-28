//! `deja run <goal>...` — replay a remembered walk with zero model calls, supporting manifest
//! mode (`DEJA-51`, `--verify`), classic mode (`staleBuild: "replay-last-verified"`),
//! flaky detection (`--rerun-failed` / `--retries <N>`), crash precedence (`DEJA-46`),
//! visible context (`DEJA-83`), and phase timing telemetry.

use std::path::{Path, PathBuf};
use std::time::Instant;

use deja_core::{
    ArtifactKind, CacheMissReason, CacheStatus, CoreError, FailureCategory, FlowClosure,
    LookupResult, NewBuildReplayReport, RunArtifact, RunAttempt, RunCommand, RunOutcome,
    StaleBuildPolicy, StoredWalk, WalkOrigin, assertions_sha256, check_owner_rules,
    closure_assertions, collect_flow_closure,
};
use deja_flow::{RunTimingBreakdown, count_assertions_in_closure};
use deja_store::{
    ConsumerFlows, ConsumerRunPlan, ConsumerRunRequest, InspectedConsumerEntry,
    PostRunValidationOptions, RecordValidationInput, SaveManifestWalkOptions, SqliteStoreOptions,
    SqliteWalkStore, StoreError, UpsertWalkInput, now_iso8601, validate_store_id,
};

use crate::CliDeps;
use crate::cli::RunArgs;
use crate::commands::exec_common::{
    ExecutedAttempt, allocate_report_rel_path, build_base_report, build_consumer_flows,
    build_driver_context, check_closure_forbidden_values, emit_early_exec_error,
    execute_single_attempt, failure_run_outcome, finalize_run_report, make_walk_identity,
    parse_goal_tokens, to_execution_result, validate_closure_app_id_confinement,
};
use crate::config::{CommandCapability, LoadConfigOptions, LoadedConfig, load_config};
use crate::exit_codes::{EXIT_USAGE_OR_CONFIG, exit_code_for_store_error};
use crate::fs_safe::lexical_normalize;
use crate::output::{
    RedactionContext, failure_category_wire, prepare_safe_junit, workspace_relative_display,
};

pub async fn run(
    args: &RunArgs,
    explicit_config: Option<&Path>,
    json_mode: bool,
    deps: &CliDeps,
) -> u8 {
    let raw_goal_fallback = args.goal.join(" ");
    let early_redaction =
        RedactionContext::new(&deps.cwd, deps.home_dir.as_deref(), &[], &deps.env);

    // Prepare explicit `--junit` as early as possible so a stale file is never left behind.
    let cli_junit: Option<PathBuf> = args.junit.as_ref().map(|p| {
        if p.is_absolute() {
            lexical_normalize(p)
        } else {
            lexical_normalize(&deps.cwd.join(p))
        }
    });
    if let Some(ref jpath) = cli_junit
        && let Err(err) = prepare_safe_junit(jpath)
    {
        return emit_early_exec_error(
            deps,
            "run",
            &raw_goal_fallback,
            None,
            json_mode,
            args.summary,
            &early_redaction,
            "JUNIT_ERROR",
            &format!("cannot prepare JUnit path {}: {err}", jpath.display()),
            EXIT_USAGE_OR_CONFIG,
        );
    }

    // Reject `--flow` on `deja run` with exit 2.
    if args.flow.is_some() {
        return emit_early_exec_error(
            deps,
            "run",
            &raw_goal_fallback,
            cli_junit.as_deref(),
            json_mode,
            args.summary,
            &early_redaction,
            "USAGE_ERROR",
            "`--flow` is only valid with `deja remember`",
            EXIT_USAGE_OR_CONFIG,
        );
    }

    if let Some(n) = args.retries
        && n > 10
    {
        return emit_early_exec_error(
            deps,
            "run",
            &raw_goal_fallback,
            cli_junit.as_deref(),
            json_mode,
            args.summary,
            &early_redaction,
            "USAGE_ERROR",
            "`--retries` must be between 0 and 10",
            EXIT_USAGE_OR_CONFIG,
        );
    }

    if let Some(ref store_id) = args.store
        && let Err(err) = validate_store_id(store_id)
    {
        return emit_early_exec_error(
            deps,
            "run",
            &raw_goal_fallback,
            cli_junit.as_deref(),
            json_mode,
            args.summary,
            &early_redaction,
            "INVALID_STORE_ID",
            &err.to_string(),
            EXIT_USAGE_OR_CONFIG,
        );
    }

    let (raw_goal, goal) = match parse_goal_tokens(&args.goal) {
        Ok(parsed) => parsed,
        Err(msg) => {
            return emit_early_exec_error(
                deps,
                "run",
                &raw_goal_fallback,
                cli_junit.as_deref(),
                json_mode,
                args.summary,
                &early_redaction,
                "INVALID_GOAL",
                &msg,
                EXIT_USAGE_OR_CONFIG,
            );
        }
    };

    let config = match load_config(&LoadConfigOptions {
        cwd: &deps.cwd,
        explicit_config,
        capability: CommandCapability::Execute,
        device_override: args.device.as_deref(),
        build_override: None,
        env: &deps.env,
    }) {
        Ok(cfg) => cfg,
        Err(err) => {
            return emit_early_exec_error(
                deps,
                "run",
                &raw_goal,
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

    let redaction = RedactionContext::new(
        &config.workspace_root,
        deps.home_dir.as_deref(),
        &config.forbidden_values,
        &deps.env,
    );

    let effective_junit: Option<PathBuf> = cli_junit.clone().or_else(|| config.junit.clone());
    if cli_junit.is_none()
        && let Some(ref jpath) = effective_junit
        && let Err(err) = prepare_safe_junit(jpath)
    {
        return emit_early_exec_error(
            deps,
            "run",
            &raw_goal,
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
            &raw_goal,
            effective_junit.as_deref(),
            json_mode,
            args.summary,
            &redaction,
            "USAGE_ERROR",
            "`--verify` requires `manifest` to be configured in deja.config.json",
            EXIT_USAGE_OR_CONFIG,
        );
    }

    let explicit_report: Option<PathBuf> = args.report.as_ref().map(|p| {
        if p.is_absolute() {
            lexical_normalize(p)
        } else {
            lexical_normalize(&deps.cwd.join(p))
        }
    });

    let cmd_started_at = now_iso8601();
    let cmd_t0 = Instant::now();
    let immutable_report_rel = allocate_report_rel_path(RunCommand::Run);

    let consumer = match build_consumer_flows(&config) {
        Ok(c) => c,
        Err(msg) => {
            return emit_early_exec_error(
                deps,
                "run",
                &raw_goal,
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

    // Acquire writable SQLite store upfront so store lock contention fails closed (exit 5).
    let mut store_opts = SqliteStoreOptions::new(&config.workspace_root);
    if let Some(ref store_id) = args.store {
        store_opts = store_opts.with_store_id(store_id.clone());
    }
    let store = match SqliteWalkStore::open_writable(store_opts) {
        Ok(s) => s,
        Err(err) => {
            let code = exit_code_for_store_error(&err);
            return emit_early_exec_error(
                deps,
                "run",
                &raw_goal,
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

    let build_path = config
        .build_path
        .as_ref()
        .expect("Execute capability requires build_path");

    let fp_t0 = Instant::now();
    let fp_res =
        match store.fingerprint_with_cache(&config.workspace_root, build_path, &config.platform) {
            Ok(r) => r,
            Err(err) => {
                store.close();
                let code = exit_code_for_store_error(&err);
                return emit_early_exec_error(
                    deps,
                    "run",
                    &raw_goal,
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
    let identity = make_walk_identity(&config, &goal, fp_res.digest);

    // Resolve runtime (e.g. "ios-27.2") via driver factory.
    let driver_ctx = build_driver_context(
        &config,
        deps,
        config.timeout_ms,
        Some(identity.build_fingerprint.clone()),
    );
    let runtime = match deps
        .driver_factory
        .resolve_runtime(&driver_ctx, &deps.cancel)
        .await
    {
        Ok(rt) => rt,
        Err(err) => {
            store.close();
            let code = err.exit_code();
            return emit_early_exec_error(
                deps,
                "run",
                &raw_goal,
                effective_junit.as_deref(),
                json_mode,
                args.summary,
                &redaction,
                "DRIVER_ERROR",
                &err.to_string(),
                code,
            );
        }
    };

    // Plan what to run (manifest mode vs classic mode).
    let plan = match plan_execution(
        &config,
        consumer.as_ref(),
        &store,
        &identity,
        runtime.as_deref(),
        args.verify,
    ) {
        Ok(p) => p,
        Err(err) => {
            store.close();
            let code = exit_code_for_store_error(&err);
            return emit_early_exec_error(
                deps,
                "run",
                &raw_goal,
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
            let mut report = build_base_report(
                RunCommand::Run,
                identity,
                &goal,
                cache_status,
                Some(miss_reason),
                outcome,
                false,
                cmd_started_at,
                now_iso8601(),
                total_ms,
                runtime,
            );
            report.core.yaml_path = yaml_path;
            report.core.errors.push(redaction.redact_str(&detail));
            report.timing = Some(RunTimingBreakdown {
                build_fingerprint_ms,
                total_ms,
                ..RunTimingBreakdown::default()
            });
            return finalize_run_report(
                deps,
                &config.workspace_root,
                args.store.as_deref(),
                &immutable_report_rel,
                explicit_report.as_deref(),
                effective_junit.as_deref(),
                json_mode,
                args.summary,
                &redaction,
                &mut report,
            );
        }
        RunPlan::Execute(ep) => *ep,
    };

    // Reuse the exact `FlowClosure` collected and approved during `plan_execution` without
    // reopening flow files from disk, then check static appId confinement and forbidden secrets.
    let closure = exec_plan.closure;

    if let Err(msg) = validate_closure_app_id_confinement(&closure, &config.app_id, &config.env) {
        store.close();
        return emit_early_exec_error(
            deps,
            "run",
            &raw_goal,
            effective_junit.as_deref(),
            json_mode,
            args.summary,
            &redaction,
            "APP_ID_CONFINEMENT",
            &msg,
            EXIT_USAGE_OR_CONFIG,
        );
    }

    if consumer.is_some()
        && let Err(msg) = check_closure_forbidden_values(&closure, &config.forbidden_values)
    {
        store.close();
        return emit_early_exec_error(
            deps,
            "run",
            &raw_goal,
            effective_junit.as_deref(),
            json_mode,
            args.summary,
            &redaction,
            "FORBIDDEN_SECRET",
            &msg,
            EXIT_USAGE_OR_CONFIG,
        );
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
            return emit_early_exec_error(
                deps,
                "run",
                &raw_goal,
                effective_junit.as_deref(),
                json_mode,
                args.summary,
                &redaction,
                "CLOSURE_ERROR",
                &err.to_string(),
                EXIT_USAGE_OR_CONFIG,
            );
        }
    };
    let assertion_count = count_assertions_in_closure(&closure).ok();

    // Attach driver + ensure session + install app.
    let attach_t0 = Instant::now();
    let driver = match deps.driver_factory.create(&driver_ctx, &deps.cancel).await {
        Ok(d) => d,
        Err(err) => {
            store.close();
            let code = err.exit_code();
            return emit_early_exec_error(
                deps,
                "run",
                &raw_goal,
                effective_junit.as_deref(),
                json_mode,
                args.summary,
                &redaction,
                "DRIVER_ERROR",
                &err.to_string(),
                code,
            );
        }
    };

    let attach_res: Result<(), (u8, String)> = async {
        driver
            .ensure_session(&deps.cancel)
            .await
            .map_err(|e| (crate::exit_codes::EXIT_FAILURE, e.to_string()))?;
        deps.driver_factory
            .install_app(&driver_ctx, &deps.cancel)
            .await
            .map_err(|e| (e.exit_code(), e.to_string()))?;
        Ok(())
    }
    .await;

    if let Err((code, err)) = attach_res {
        let _ = driver.shutdown().await;
        store.close();
        return emit_early_exec_error(
            deps,
            "run",
            &raw_goal,
            effective_junit.as_deref(),
            json_mode,
            args.summary,
            &redaction,
            "DRIVER_ATTACH_FAILED",
            &err,
            code,
        );
    }
    let driver_attach_ms = attach_t0.elapsed().as_millis() as u64;

    // Execute attempt 1 + optional retry loop (`--rerun-failed` / `--retries <N>`).
    let max_extra_retries = args
        .retries
        .map(usize::from)
        .or(if args.rerun_failed { Some(1) } else { None })
        .unwrap_or_else(|| usize::from(config.retries));

    let exec_t0 = Instant::now();
    let timeout_ms = if args.verify {
        config.verification_timeout_ms
    } else {
        config.timeout_ms
    };

    let mut attempts_raw: Vec<ExecutedAttempt> = Vec::new();
    let mut mid_retry_drift: Option<String> = None;

    let first_attempt = execute_single_attempt(
        driver.as_ref(),
        &config,
        &closure,
        timeout_ms,
        &redaction,
        &deps.cancel,
    )
    .await;
    let first_passed = first_attempt.passed;
    attempts_raw.push(first_attempt);

    if !first_passed && max_extra_retries > 0 && !deps.cancel.is_cancelled() {
        let first_sig = failure_signature(attempts_raw[0].failure.as_ref());
        for _ in 0..max_extra_retries {
            if deps.cancel.is_cancelled() {
                break;
            }
            // Re-verify closure bytes BEFORE re-running so retries only execute identical bytes.
            match verify_unchanged_closure(
                &config.workspace_root,
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
                driver.as_ref(),
                &config,
                &closure,
                timeout_ms,
                &redaction,
                &deps.cancel,
            )
            .await;
            let retry_passed = retry_attempt.passed;
            let retry_sig = failure_signature(retry_attempt.failure.as_ref());
            attempts_raw.push(retry_attempt);
            if retry_passed || retry_sig != first_sig {
                break;
            }
        }
    }

    let flow_exec_ms = exec_t0.elapsed().as_millis() as u64;

    // Guaranteed driver shutdown BEFORE releasing store lock or finalizing reports.
    let _ = driver.shutdown().await;

    // If files drifted between retry attempts, refuse with `flow-changed` (exit 6), zero writes.
    if let Some(drift_msg) = mid_retry_drift {
        store.close();
        let total_ms = cmd_t0.elapsed().as_millis() as u64;
        let mut report = build_base_report(
            RunCommand::Run,
            identity,
            &goal,
            exec_plan.cache_status,
            Some(CacheMissReason::FlowChanged),
            RunOutcome::FlowChanged,
            false,
            cmd_started_at,
            now_iso8601(),
            total_ms,
            runtime,
        );
        report.core.yaml_path = Some(exec_plan.display_flow_path);
        report.core.yaml_sha256 = Some(main_sha256);
        report.core.assertions = assertion_count;
        report.core.errors.push(redaction.redact_str(&drift_msg));
        report.timing = Some(RunTimingBreakdown {
            build_fingerprint_ms,
            driver_attach_ms,
            flow_exec_ms,
            total_ms,
            steps: attempts_raw[0].timing.steps.clone(),
            ..RunTimingBreakdown::default()
        });
        return finalize_run_report(
            deps,
            &config.workspace_root,
            args.store.as_deref(),
            &immutable_report_rel,
            explicit_report.as_deref(),
            effective_junit.as_deref(),
            json_mode,
            args.summary,
            &redaction,
            &mut report,
        );
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

    // Build preliminary timing breakdown (store_ms filled in after store write).
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

    // Persist validation to SQLite (and manifest on `--verify` pass).
    let store_t0 = Instant::now();
    let validation_at = first.ended_at.clone();
    let mut validation_input = RecordValidationInput::new(final_passed, validation_at)
        .with_flaky(is_flaky)
        .with_report_path(immutable_report_rel.clone())
        .with_duration_ms(cmd_t0.elapsed().as_millis() as u64);
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

    let mut manifest_written = false;
    let mut recorded_for_new_build = false;
    let persist_res: Result<(), StoreError> = (|| match &exec_plan.mode {
        ExecMode::ManifestVerify { flow_rel } => {
            if final_passed {
                let cf = consumer.as_ref().expect("manifest mode");
                let (_, changed) = cf.save_verified_manifest_walk(
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
                manifest_written = changed;
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
                // Re-verify stored bytes didn't drift before persisting on the new build.
                verify_unchanged_closure(
                    &config.workspace_root,
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
                let code = exit_code_for_store_error(&err);
                return emit_early_exec_error(
                    deps,
                    "run",
                    &raw_goal,
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
        let mut report = build_base_report(
            RunCommand::Run,
            identity,
            &goal,
            exec_plan.cache_status,
            miss,
            outcome,
            false,
            cmd_started_at,
            now_iso8601(),
            timing.total_ms,
            runtime,
        );
        report.core.yaml_path = Some(exec_plan.display_flow_path);
        report.core.yaml_sha256 = Some(main_sha256);
        report.core.assertions = assertion_count;
        report
            .core
            .errors
            .push(redaction.redact_str(&err.to_string()));
        report.timing = Some(timing);
        return finalize_run_report(
            deps,
            &config.workspace_root,
            args.store.as_deref(),
            &immutable_report_rel,
            explicit_report.as_deref(),
            effective_junit.as_deref(),
            json_mode,
            args.summary,
            &redaction,
            &mut report,
        );
    }

    // Build full schema-1 report.
    let mut report = build_base_report(
        RunCommand::Run,
        identity,
        &goal,
        exec_plan.cache_status,
        exec_plan.miss_reason,
        final_outcome,
        final_passed,
        cmd_started_at,
        now_iso8601(),
        timing.total_ms,
        runtime,
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
    if manifest_written && let Some(ref manifest_rel) = config.manifest {
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

    finalize_run_report(
        deps,
        &config.workspace_root,
        args.store.as_deref(),
        &immutable_report_rel,
        explicit_report.as_deref(),
        effective_junit.as_deref(),
        json_mode,
        args.summary,
        &redaction,
        &mut report,
    )
}

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

fn plan_execution(
    config: &LoadedConfig,
    consumer: Option<&ConsumerFlows>,
    store: &SqliteWalkStore,
    identity: &deja_core::WalkIdentity,
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
                            "manifest has no matching entry for {:?} ({}); use `deja remember` to add a new entry",
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
                let display_flow =
                    workspace_relative_display(&config.workspace_root, Path::new(&hit.flow_path));
                let closure = match collect_and_verify_classic_closure(
                    &config.workspace_root,
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
                            &config.workspace_root,
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
                            "no remembered walk found for {:?}; record one first with `deja remember {:?} --flow <path>`",
                            identity.goal, identity.goal
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

fn failure_signature(
    failure: Option<&deja_core::WalkFailure>,
) -> (Option<FailureCategory>, Option<String>) {
    match failure {
        Some(f) => (Some(f.category), f.failing_step.clone()),
        None => (None, None),
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
    if live.sha256 != expected_closure_sha256 {
        return Err(format!(
            "the flow or a file it runs changed during execution (started with {}, now {})",
            expected_closure_sha256, live.sha256
        ));
    }
    let live_assertions = assertions_sha256(&live)
        .map_err(|err| format!("failed to hash flow assertions during retry check: {err}"))?;
    if live_assertions != expected_assertions_sha256 {
        return Err(format!(
            "the flow's assertions changed during execution (started with {}, now {})",
            expected_assertions_sha256, live_assertions
        ));
    }
    Ok(())
}
