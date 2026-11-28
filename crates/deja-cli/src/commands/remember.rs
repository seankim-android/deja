//! `deja remember <goal>... --flow <candidate.yaml>` — verify a reviewed YAML flow on device
//! and persist to SQLite (and `deja.walks.json` in manifest mode) only on a complete pass.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use deja_core::{
    ArtifactKind, CacheMissReason, CacheStatus, CoreError, ManifestWalk, RunArtifact, RunCommand,
    RunOutcome, WalkOrigin, assertions_sha256, check_manifest_flow_path, check_owner_rules,
    closure_assertions, collect_flow_closure, manifest_walk_key,
};
use deja_flow::{RunTimingBreakdown, count_assertions_in_closure};
use deja_store::{
    PersistVerifiedCandidateOptions, RecordValidationInput, SaveManifestWalkOptions,
    SqliteStoreOptions, SqliteWalkStore, StoreError, now_iso8601, validate_store_id,
};

use crate::CliDeps;
use crate::cli::RememberArgs;
use crate::commands::exec_common::{
    allocate_report_rel_path, build_base_report, build_consumer_flows, build_driver_context,
    check_closure_forbidden_values, emit_early_exec_error, execute_single_attempt,
    failure_run_outcome, finalize_run_report, make_walk_identity, parse_goal_tokens,
    to_execution_result, validate_closure_app_id_confinement, validate_early_output_path,
    validate_explicit_output_path,
};
use crate::config::{CommandCapability, LoadConfigOptions, load_config};
use crate::exit_codes::{EXIT_USAGE_OR_CONFIG, exit_code_for_store_error};
use crate::fs_safe::{lexical_normalize, path_inside};
use crate::output::{
    RedactionContext, failure_category_wire, prepare_safe_junit, workspace_relative_display,
};

pub async fn run(
    args: &RememberArgs,
    explicit_config: Option<&Path>,
    json_mode: bool,
    deps: &CliDeps,
) -> u8 {
    let raw_goal_fallback = args.goal.join(" ");
    let early_redaction =
        RedactionContext::new(&deps.cwd, deps.home_dir.as_deref(), &[], &deps.env);

    // Resolve & protect explicit `--junit` and `--report` destinations BEFORE touching disk.
    let cli_junit: Option<PathBuf> = args.junit.as_ref().map(|p| {
        if p.is_absolute() {
            lexical_normalize(p)
        } else {
            lexical_normalize(&deps.cwd.join(p))
        }
    });
    let explicit_report: Option<PathBuf> = args.report.as_ref().map(|p| {
        if p.is_absolute() {
            lexical_normalize(p)
        } else {
            lexical_normalize(&deps.cwd.join(p))
        }
    });
    if let Some(ref jpath) = cli_junit {
        if let Err(msg) = validate_early_output_path(
            jpath,
            "--junit",
            &deps.cwd,
            explicit_config,
            args.flow.as_deref(),
        ) {
            return emit_early_exec_error(
                deps,
                "remember",
                &raw_goal_fallback,
                None,
                json_mode,
                args.summary,
                &early_redaction,
                "JUNIT_ERROR",
                &msg,
                EXIT_USAGE_OR_CONFIG,
            );
        }
        if let Err(err) = prepare_safe_junit(jpath) {
            return emit_early_exec_error(
                deps,
                "remember",
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
    }
    if let Some(ref rpath) = explicit_report
        && let Err(msg) = validate_early_output_path(
            rpath,
            "--report",
            &deps.cwd,
            explicit_config,
            args.flow.as_deref(),
        )
    {
        return emit_early_exec_error(
            deps,
            "remember",
            &raw_goal_fallback,
            cli_junit.as_deref(),
            json_mode,
            args.summary,
            &early_redaction,
            "REPORT_PATH_REFUSED",
            &msg,
            EXIT_USAGE_OR_CONFIG,
        );
    }

    // Reject `--verify` on `deja remember` with exit 2.
    if args.verify {
        return emit_early_exec_error(
            deps,
            "remember",
            &raw_goal_fallback,
            cli_junit.as_deref(),
            json_mode,
            args.summary,
            &early_redaction,
            "USAGE_ERROR",
            "`--verify` is only valid with `deja run`",
            EXIT_USAGE_OR_CONFIG,
        );
    }

    let Some(ref raw_flow) = args.flow else {
        return emit_early_exec_error(
            deps,
            "remember",
            &raw_goal_fallback,
            cli_junit.as_deref(),
            json_mode,
            args.summary,
            &early_redaction,
            "USAGE_ERROR",
            "`deja remember` requires `--flow <candidate.yaml>`",
            EXIT_USAGE_OR_CONFIG,
        );
    };

    if let Some(ref store_id) = args.store
        && let Err(err) = validate_store_id(store_id)
    {
        return emit_early_exec_error(
            deps,
            "remember",
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
                "remember",
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
                "remember",
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
    if let Some(ref jpath) = effective_junit {
        if let Err(msg) = validate_explicit_output_path(jpath, "--junit", &config, None) {
            return emit_early_exec_error(
                deps,
                "remember",
                &raw_goal,
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
                "remember",
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
    }
    if let Some(ref rpath) = explicit_report
        && let Err(msg) = validate_explicit_output_path(rpath, "--report", &config, None)
    {
        return emit_early_exec_error(
            deps,
            "remember",
            &raw_goal,
            effective_junit.as_deref(),
            json_mode,
            args.summary,
            &redaction,
            "REPORT_PATH_REFUSED",
            &msg,
            EXIT_USAGE_OR_CONFIG,
        );
    }

    if args.accept_assertion_change && config.manifest.is_none() {
        return emit_early_exec_error(
            deps,
            "remember",
            &raw_goal,
            effective_junit.as_deref(),
            json_mode,
            args.summary,
            &redaction,
            "USAGE_ERROR",
            "`--accept-assertion-change` requires `manifest` to be configured in deja.config.json",
            EXIT_USAGE_OR_CONFIG,
        );
    }

    let cmd_started_at = now_iso8601();
    let cmd_t0 = Instant::now();
    let immutable_report_rel = allocate_report_rel_path(RunCommand::Remember);

    // Resolve and validate candidate flow file inside workspace.
    let candidate_abs = if raw_flow.is_absolute() {
        lexical_normalize(raw_flow)
    } else {
        lexical_normalize(&deps.cwd.join(raw_flow))
    };
    let canonical_candidate = match fs::canonicalize(&candidate_abs) {
        Ok(p) => p,
        Err(err) => {
            return emit_early_exec_error(
                deps,
                "remember",
                &raw_goal,
                effective_junit.as_deref(),
                json_mode,
                args.summary,
                &redaction,
                "CANDIDATE_NOT_FOUND",
                &format!(
                    "candidate flow does not exist ({}): {err}",
                    raw_flow.display()
                ),
                EXIT_USAGE_OR_CONFIG,
            );
        }
    };
    if !path_inside(&config.workspace_root, &canonical_candidate)
        || !fs::metadata(&canonical_candidate)
            .map(|m| m.is_file())
            .unwrap_or(false)
    {
        return emit_early_exec_error(
            deps,
            "remember",
            &raw_goal,
            effective_junit.as_deref(),
            json_mode,
            args.summary,
            &redaction,
            "CLOSURE_ERROR",
            &format!(
                "candidate flow must be a regular file inside the workspace: {}",
                raw_flow.display()
            ),
            EXIT_USAGE_OR_CONFIG,
        );
    }

    let candidate_rel = workspace_relative_display(&config.workspace_root, &canonical_candidate);

    let consumer = match build_consumer_flows(&config) {
        Ok(c) => c,
        Err(msg) => {
            return emit_early_exec_error(
                deps,
                "remember",
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

    if consumer.is_some()
        && let Err(err) = check_manifest_flow_path(&candidate_rel, "flow")
    {
        return emit_early_exec_error(
            deps,
            "remember",
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

    // Collect full flow closure, protect closure files from output clobber, validate static
    // appId confinement, and check forbidden secrets.
    let closure = match collect_flow_closure(&config.workspace_root, &canonical_candidate) {
        Ok(c) => c,
        Err(err) => {
            return emit_early_exec_error(
                deps,
                "remember",
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

    if let Some(ref jpath) = effective_junit
        && let Err(msg) = validate_explicit_output_path(jpath, "--junit", &config, Some(&closure))
    {
        return emit_early_exec_error(
            deps,
            "remember",
            &raw_goal,
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
        && let Err(msg) = validate_explicit_output_path(rpath, "--report", &config, Some(&closure))
    {
        return emit_early_exec_error(
            deps,
            "remember",
            &raw_goal,
            effective_junit.as_deref(),
            json_mode,
            args.summary,
            &redaction,
            "REPORT_PATH_REFUSED",
            &msg,
            EXIT_USAGE_OR_CONFIG,
        );
    }

    if let Err(msg) = validate_closure_app_id_confinement(&closure, &config.app_id, &config.env) {
        return emit_early_exec_error(
            deps,
            "remember",
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
        return emit_early_exec_error(
            deps,
            "remember",
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
            return emit_early_exec_error(
                deps,
                "remember",
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
    let assertions_list = match closure_assertions(&closure) {
        Ok(a) => a,
        Err(err) => {
            return emit_early_exec_error(
                deps,
                "remember",
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

    // Acquire writable SQLite store upfront so lock contention fails closed (exit 5) before running.
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
                "remember",
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
                    "remember",
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

    // Pre-run manifest `ownerRules` and assertion-change guard (before touching device!).
    if let Some(ref cf) = consumer {
        let manifest = match cf.load() {
            Ok(m) => m,
            Err(err) => {
                store.close();
                let code = exit_code_for_store_error(&err);
                return emit_early_exec_error(
                    deps,
                    "remember",
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
        let adapter = config.adapter.clone().unwrap_or_default();
        let probe = ManifestWalk {
            goal: goal.text.clone(),
            app_id: config.app_id.clone(),
            platform: config.platform.clone(),
            adapter,
            browser: config.browser.clone(),
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
                return emit_early_exec_error(
                    deps,
                    "remember",
                    &raw_goal,
                    effective_junit.as_deref(),
                    json_mode,
                    args.summary,
                    &redaction,
                    "INVALID_MANIFEST",
                    &err.to_string(),
                    EXIT_USAGE_OR_CONFIG,
                );
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
            return emit_early_exec_error(
                deps,
                "remember",
                &raw_goal,
                effective_junit.as_deref(),
                json_mode,
                args.summary,
                &redaction,
                "OWNER_RULE",
                &err.to_string(),
                EXIT_USAGE_OR_CONFIG,
            );
        }

        if let Some(ex) = existing
            && ex.assertions_sha256 != expected_assertions_sha256
            && !args.accept_assertion_change
        {
            store.close();
            let total_ms = cmd_t0.elapsed().as_millis() as u64;
            let mut report = build_base_report(
                RunCommand::Remember,
                identity,
                &goal,
                CacheStatus::NotChecked,
                Some(CacheMissReason::FlowChanged),
                RunOutcome::AssertionsChanged,
                false,
                cmd_started_at,
                now_iso8601(),
                total_ms,
                None,
            );
            report.core.yaml_path = Some(candidate_rel);
            report.core.yaml_sha256 = Some(main_sha256);
            report.core.assertions = assertion_count;
            report.core.errors.push(format!(
                "the flow's assertions changed (manifest {}, now {}); pass `--accept-assertion-change` after review",
                ex.assertions_sha256, expected_assertions_sha256
            ));
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
    }

    // Resolve runtime, create driver, attach session, and install app.
    let driver_ctx = build_driver_context(
        &config,
        deps,
        config.verification_timeout_ms,
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
                "remember",
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

    let attach_t0 = Instant::now();
    let driver = match deps.driver_factory.create(&driver_ctx, &deps.cancel).await {
        Ok(d) => d,
        Err(err) => {
            store.close();
            let code = err.exit_code();
            return emit_early_exec_error(
                deps,
                "remember",
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
            "remember",
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

    // Live execution on device using exact immutable preflight closure bytes.
    let exec_t0 = Instant::now();
    let attempt = execute_single_attempt(
        driver.as_ref(),
        &config,
        &closure,
        config.verification_timeout_ms,
        &redaction,
        &deps.cancel,
    )
    .await;
    let flow_exec_ms = exec_t0.elapsed().as_millis() as u64;

    // Guaranteed driver shutdown BEFORE persistence / lock release.
    let _ = driver.shutdown().await;

    let mut timing = RunTimingBreakdown {
        build_fingerprint_ms,
        driver_attach_ms,
        warmup_ms: attempt.timing.warmup_ms,
        flow_exec_ms,
        store_ms: 0,
        total_ms: cmd_t0.elapsed().as_millis() as u64,
        steps: attempt.timing.steps.clone(),
    };

    // A failing or cancelled candidate is NEVER persisted.
    if !attempt.passed || deps.cancel.is_cancelled() {
        store.close();
        timing.total_ms = cmd_t0.elapsed().as_millis() as u64;
        let outcome = failure_run_outcome(attempt.failure.as_ref());
        let mut report = build_base_report(
            RunCommand::Remember,
            identity,
            &goal,
            CacheStatus::NotChecked,
            None,
            outcome,
            false,
            cmd_started_at,
            now_iso8601(),
            timing.total_ms,
            runtime,
        );
        report.core.yaml_path = Some(candidate_rel.clone());
        report.core.yaml_sha256 = Some(main_sha256.clone());
        report.core.assertions = assertion_count;
        report.core.failure = attempt.failure.clone();
        report.core.execution = Some(to_execution_result(&attempt, Some(&closure)));
        report.core.errors = attempt.errors.clone();
        report.core.artifacts.push(RunArtifact {
            kind: ArtifactKind::Yaml,
            path: candidate_rel,
            sha256: Some(main_sha256),
        });
        report.core.artifacts.extend(attempt.artifacts.clone());
        report.timing = Some(timing);
        report.visible_context = attempt.visible_context;
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

    // Complete pass -> persist atomically (re-verifying post-run closure/assertion hashes!).
    let phase_timings_json = serde_json::to_string(&timing).ok();
    let mut validation = RecordValidationInput::new(true, attempt.ended_at.clone())
        .with_origin(WalkOrigin::Remember)
        .with_report_path(immutable_report_rel.clone())
        .with_duration_ms(cmd_t0.elapsed().as_millis() as u64);
    if let Some(ref rt) = runtime {
        validation = validation.with_runtime(rt.clone());
    }
    if let Some(ref pt_json) = phase_timings_json {
        validation = validation.with_phase_timings_json(pt_json.clone());
    }
    if let Some(ref fail) = attempt.failure {
        validation = validation.with_failure_category(failure_category_wire(fail.category));
    }

    let store_t0 = Instant::now();
    let persist_res: Result<(String, String, bool), StoreError> = (|| {
        if let Some(ref cf) = consumer {
            let (stored, manifest_changed) = cf.save_verified_manifest_walk(
                &store,
                &SaveManifestWalkOptions {
                    identity: identity.clone(),
                    flow_path: candidate_rel.clone(),
                    expected_closure_sha256: expected_closure_sha256.clone(),
                    expected_assertions_sha256: Some(expected_assertions_sha256.clone()),
                    validation,
                    accept_assertion_change: args.accept_assertion_change,
                },
            )?;
            Ok((stored.flow_path, stored.flow_sha256, manifest_changed))
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
            Ok((stored.flow_path, stored.flow_sha256, false))
        }
    })();
    let store_ms = store_t0.elapsed().as_millis() as u64;
    store.close();

    timing.store_ms = store_ms;
    timing.total_ms = cmd_t0.elapsed().as_millis() as u64;

    let (stored_flow_path, stored_flow_sha256, manifest_changed) = match persist_res {
        Ok(res) => res,
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
                let mut report = build_base_report(
                    RunCommand::Remember,
                    identity,
                    &goal,
                    CacheStatus::NotChecked,
                    Some(CacheMissReason::FlowChanged),
                    outcome,
                    false,
                    cmd_started_at,
                    now_iso8601(),
                    timing.total_ms,
                    runtime,
                );
                report.core.yaml_path = Some(candidate_rel);
                report.core.yaml_sha256 = Some(main_sha256);
                report.core.assertions = assertion_count;
                report.core.execution = Some(to_execution_result(&attempt, Some(&closure)));
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
            let code = exit_code_for_store_error(&err);
            return emit_early_exec_error(
                deps,
                "remember",
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
        RunCommand::Remember,
        identity,
        &goal,
        CacheStatus::NotChecked,
        None,
        RunOutcome::Remembered,
        true,
        cmd_started_at,
        now_iso8601(),
        timing.total_ms,
        runtime,
    );
    report.core.yaml_path = Some(stored_flow_path.clone());
    report.core.yaml_sha256 = Some(stored_flow_sha256.clone());
    report.core.assertions = assertion_count;
    report.core.execution = Some(to_execution_result(&attempt, Some(&closure)));
    report.core.artifacts.push(RunArtifact {
        kind: ArtifactKind::Yaml,
        path: stored_flow_path,
        sha256: Some(stored_flow_sha256),
    });
    if manifest_changed && let Some(ref manifest_rel) = config.manifest {
        report.core.artifacts.push(RunArtifact {
            kind: ArtifactKind::Manifest,
            path: manifest_rel.clone(),
            sha256: None,
        });
    }
    report.core.artifacts.extend(attempt.artifacts.clone());
    report.timing = Some(timing);

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
