//! `deja inspect <target>` — inspect a saved JSON run report file OR a goal's validation
//! history from read-only SQLite.

use std::path::{Path, PathBuf};

use deja_core::{WALK_RESULT_SCHEMA_VERSION, normalize_goal};
use deja_store::{SqliteStoreOptions, SqliteWalkStore, validate_store_id};
use serde_json::json;

use crate::CliDeps;
use crate::cli::InspectArgs;
use crate::config::{
    CommandCapability, DEFAULT_CONFIG_PATH, LoadConfigOptions, LoadedConfig, load_config,
};
use crate::exit_codes::{
    EXIT_OK, EXIT_USAGE_OR_CONFIG, exit_code_for_outcome, exit_code_for_store_error,
};
use crate::fs_safe::{lexical_normalize, read_to_string_nofollow_bounded};
use crate::output::{
    CliRunReport, RedactionContext, emit_summary, format_error_summary_line, format_summary_line,
    print_human_run_report,
};

const MAX_REPORT_BYTES: u64 = 16 * 1024 * 1024;

pub fn run(
    args: &InspectArgs,
    explicit_config: Option<&Path>,
    json_mode: bool,
    deps: &CliDeps,
) -> u8 {
    if let Some(ref store_id) = args.store
        && let Err(err) = validate_store_id(store_id)
    {
        return emit_inspect_error(
            deps,
            json_mode,
            args.summary,
            &args.target,
            &RedactionContext::new(&deps.cwd, deps.home_dir.as_deref(), &[], &deps.env),
            "INVALID_STORE_ID",
            &err.to_string(),
            EXIT_USAGE_OR_CONFIG,
        );
    }

    let candidate_path = resolve_target_path(&deps.cwd, &args.target);
    let looks_like_report_file = candidate_path.symlink_metadata().is_ok()
        || args.target.ends_with(".json")
        || args.target.contains('/')
        || args.target.contains('\\');

    if looks_like_report_file {
        inspect_report_file(args, &candidate_path, explicit_config, json_mode, deps)
    } else {
        inspect_store_goal(args, explicit_config, json_mode, deps)
    }
}

fn resolve_target_path(cwd: &Path, target: &str) -> PathBuf {
    let p = Path::new(target);
    if p.is_absolute() {
        lexical_normalize(p)
    } else {
        lexical_normalize(&cwd.join(p))
    }
}

fn load_optional_static_config(
    explicit_config: Option<&Path>,
    deps: &CliDeps,
) -> Result<Option<LoadedConfig>, String> {
    let default_cfg = deps.cwd.join(DEFAULT_CONFIG_PATH);
    if explicit_config.is_none() && !default_cfg.exists() {
        return Ok(None);
    }
    load_config(&LoadConfigOptions {
        cwd: &deps.cwd,
        explicit_config,
        capability: CommandCapability::Static,
        device_override: None,
        build_override: None,
        env: &deps.env,
    })
    .map(Some)
    .map_err(|err| err.to_string())
}

fn inspect_report_file(
    args: &InspectArgs,
    path: &Path,
    explicit_config: Option<&Path>,
    json_mode: bool,
    deps: &CliDeps,
) -> u8 {
    let config = match load_optional_static_config(explicit_config, deps) {
        Ok(cfg) => cfg,
        Err(msg) => {
            let redaction =
                RedactionContext::new(&deps.cwd, deps.home_dir.as_deref(), &[], &deps.env);
            return emit_inspect_error(
                deps,
                json_mode,
                args.summary,
                &args.target,
                &redaction,
                "CONFIG_ERROR",
                &redaction.redact_str(&msg),
                EXIT_USAGE_OR_CONFIG,
            );
        }
    };

    if let Some(ref cfg) = config {
        for w in &cfg.warnings {
            deps.output.writeln_stderr(w);
        }
    }

    let (ws_root, forbidden) = match &config {
        Some(cfg) => (cfg.workspace_root.clone(), cfg.forbidden_values.clone()),
        None => (deps.cwd.clone(), Vec::new()),
    };
    let redaction =
        RedactionContext::new(&ws_root, deps.home_dir.as_deref(), &forbidden, &deps.env);

    let raw = match read_to_string_nofollow_bounded(path, MAX_REPORT_BYTES) {
        Ok(text) => text,
        Err(err) => {
            return emit_inspect_error(
                deps,
                json_mode,
                args.summary,
                &args.target,
                &redaction,
                "REPORT_READ_ERROR",
                &redaction.redact_str(&format!("cannot read report file {}: {err}", args.target)),
                EXIT_USAGE_OR_CONFIG,
            );
        }
    };

    let report: CliRunReport = match serde_json::from_str(&raw) {
        Ok(rep) => rep,
        Err(err) => {
            return emit_inspect_error(
                deps,
                json_mode,
                args.summary,
                &args.target,
                &redaction,
                "INVALID_REPORT",
                &redaction.redact_str(&format!("invalid Déjà run report {}: {err}", args.target)),
                EXIT_USAGE_OR_CONFIG,
            );
        }
    };

    if report.core.schema_version != WALK_RESULT_SCHEMA_VERSION {
        return emit_inspect_error(
            deps,
            json_mode,
            args.summary,
            &args.target,
            &redaction,
            "UNSUPPORTED_REPORT_SCHEMA",
            &format!(
                "unsupported report schemaVersion {} (expected {WALK_RESULT_SCHEMA_VERSION})",
                report.core.schema_version
            ),
            EXIT_USAGE_OR_CONFIG,
        );
    }

    let redacted_json = match redaction.redact_report(&report) {
        Ok(val) => val,
        Err(err) => {
            return emit_inspect_error(
                deps,
                json_mode,
                args.summary,
                &args.target,
                &redaction,
                "REPORT_SERIALIZE_ERROR",
                &redaction.redact_str(&err.to_string()),
                EXIT_USAGE_OR_CONFIG,
            );
        }
    };

    if json_mode {
        deps.output.writeln_stdout(
            &serde_json::to_string_pretty(&redacted_json).expect("report JSON serializes"),
        );
    } else {
        print_human_run_report(&deps.output, &report, &redaction);
    }

    if args.summary {
        let summary_line = format_summary_line(&report.core, &redaction);
        emit_summary(&deps.output, json_mode, &summary_line);
    }

    exit_code_for_outcome(report.core.outcome)
}

fn inspect_store_goal(
    args: &InspectArgs,
    explicit_config: Option<&Path>,
    json_mode: bool,
    deps: &CliDeps,
) -> u8 {
    let config = match load_config(&LoadConfigOptions {
        cwd: &deps.cwd,
        explicit_config,
        capability: CommandCapability::Static,
        device_override: None,
        build_override: None,
        env: &deps.env,
    }) {
        Ok(cfg) => cfg,
        Err(err) => {
            let redaction =
                RedactionContext::new(&deps.cwd, deps.home_dir.as_deref(), &[], &deps.env);
            return emit_inspect_error(
                deps,
                json_mode,
                args.summary,
                &args.target,
                &redaction,
                "CONFIG_ERROR",
                &redaction.redact_str(&err.to_string()),
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

    let norm_goal = match normalize_goal(&args.target) {
        Ok(g) => g,
        Err(err) => {
            return emit_inspect_error(
                deps,
                json_mode,
                args.summary,
                &args.target,
                &redaction,
                "INVALID_GOAL",
                &err.to_string(),
                EXIT_USAGE_OR_CONFIG,
            );
        }
    };

    let mut store_opts = SqliteStoreOptions::new(&config.workspace_root);
    if let Some(ref store_id) = args.store {
        store_opts = store_opts.with_store_id(store_id.clone());
    }
    let store = match SqliteWalkStore::open_readonly(store_opts) {
        Ok(s) => s,
        Err(err) => {
            let code = exit_code_for_store_error(&err);
            return emit_inspect_error(
                deps,
                json_mode,
                args.summary,
                &args.target,
                &redaction,
                err.code(),
                &redaction.redact_str(&err.to_string()),
                code,
            );
        }
    };

    let walks = match store.list_walks() {
        Ok(w) => w,
        Err(err) => {
            let code = exit_code_for_store_error(&err);
            return emit_inspect_error(
                deps,
                json_mode,
                args.summary,
                &args.target,
                &redaction,
                err.code(),
                &redaction.redact_str(&err.to_string()),
                code,
            );
        }
    };

    let matched = walks
        .iter()
        .find(|w| {
            w.goal_key == norm_goal.key
                && w.app_id == config.app_id
                && w.platform == config.platform
                && w.adapter == config.adapter
                && w.browser == config.browser
        })
        .or_else(|| walks.iter().find(|w| w.goal_key == norm_goal.key));

    let Some(walk) = matched else {
        return emit_inspect_error(
            deps,
            json_mode,
            args.summary,
            &args.target,
            &redaction,
            "WALK_NOT_FOUND",
            &redaction.redact_str(&format!(
                "no saved report file or walk found for {:?}",
                args.target
            )),
            EXIT_USAGE_OR_CONFIG,
        );
    };

    let validation_runs = match store.list_validation_runs(&walk.identity()) {
        Ok(runs) => runs,
        Err(err) => {
            let code = exit_code_for_store_error(&err);
            return emit_inspect_error(
                deps,
                json_mode,
                args.summary,
                &args.target,
                &redaction,
                err.code(),
                &redaction.redact_str(&err.to_string()),
                code,
            );
        }
    };

    if json_mode {
        let payload = json!({
            "store": store.store_label(),
            "walk": walk,
            "validationRuns": validation_runs,
        });
        let redacted = redaction.redact_json(&payload);
        deps.output
            .writeln_stdout(&serde_json::to_string_pretty(&redacted).expect("inspect JSON"));
    } else {
        deps.output.writeln_stdout(&format!(
            "Walk: {} ({}, {})",
            redaction.redact_str(&walk.goal),
            walk.platform,
            redaction.redact_str(&walk.app_id)
        ));
        deps.output.writeln_stdout(&format!(
            "  flow: {} (sha256: {})",
            redaction.redact_str(&walk.flow_path),
            walk.flow_sha256
        ));
        if let Some(ref csha) = walk.closure_sha256 {
            deps.output
                .writeln_stdout(&format!("  closureSha256: {csha}"));
        }
        deps.output.writeln_stdout(&format!(
            "  lastValidation: {} at {}",
            if walk.last_validation.passed {
                "passed"
            } else {
                "failed"
            },
            walk.last_validation.at
        ));
        deps.output
            .writeln_stdout(&format!("  validationRuns ({}):", validation_runs.len()));
        for run in &validation_runs {
            let fp_short: String = run.build_fingerprint.chars().take(12).collect();
            let rt = run.runtime.as_deref().unwrap_or("default");
            let status = if run.flaky {
                "flaky"
            } else if run.passed {
                "passed"
            } else {
                "failed"
            };
            deps.output.writeln_stdout(&format!(
                "    - build={fp_short} runtime={rt} status={status} at={}",
                run.validated_at
            ));
        }
    }

    EXIT_OK
}

#[allow(clippy::too_many_arguments)]
fn emit_inspect_error(
    deps: &CliDeps,
    json_mode: bool,
    summary: bool,
    raw_goal: &str,
    redaction: &RedactionContext,
    code: &str,
    message: &str,
    exit_code: u8,
) -> u8 {
    if json_mode {
        let payload = json!({
            "error": { "code": code, "message": message }
        });
        deps.output
            .writeln_stdout(&serde_json::to_string_pretty(&payload).expect("error JSON"));
    } else {
        deps.output
            .writeln_stderr(&format!("deja inspect error: {message}"));
    }
    if summary {
        let line = format_error_summary_line(raw_goal, redaction);
        emit_summary(&deps.output, json_mode, &line);
    }
    exit_code
}
