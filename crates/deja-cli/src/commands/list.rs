//! `deja list` — list valid local walks from read-only SQLite without invoking drivers or models.

use std::path::Path;

use deja_store::{SqliteStoreOptions, SqliteWalkStore, validate_store_id};
use serde_json::json;

use crate::CliDeps;
use crate::cli::ListArgs;
use crate::config::{CommandCapability, LoadConfigOptions, load_config};
use crate::exit_codes::{EXIT_OK, EXIT_USAGE_OR_CONFIG, exit_code_for_store_error};
use crate::output::RedactionContext;

pub fn run(args: &ListArgs, explicit_config: Option<&Path>, json_mode: bool, deps: &CliDeps) -> u8 {
    if let Some(ref store_id) = args.store
        && let Err(err) = validate_store_id(store_id)
    {
        emit_list_error(deps, json_mode, "INVALID_STORE_ID", &err.to_string());
        return EXIT_USAGE_OR_CONFIG;
    }

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
            emit_list_error(
                deps,
                json_mode,
                "CONFIG_ERROR",
                &redaction.redact_str(&err.to_string()),
            );
            return EXIT_USAGE_OR_CONFIG;
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

    let mut store_opts = SqliteStoreOptions::new(&config.workspace_root);
    if let Some(ref store_id) = args.store {
        store_opts = store_opts.with_store_id(store_id.clone());
    }
    let store = match SqliteWalkStore::open_readonly(store_opts) {
        Ok(s) => s,
        Err(err) => {
            let code = exit_code_for_store_error(&err);
            emit_list_error(
                deps,
                json_mode,
                err.code(),
                &redaction.redact_str(&err.to_string()),
            );
            return code;
        }
    };

    let walks = match store.list_walks() {
        Ok(w) => w,
        Err(err) => {
            let code = exit_code_for_store_error(&err);
            emit_list_error(
                deps,
                json_mode,
                err.code(),
                &redaction.redact_str(&err.to_string()),
            );
            return code;
        }
    };

    if json_mode {
        let exported_walks: Vec<_> = walks
            .iter()
            .map(|w| {
                let mut cloned = w.clone();
                cloned.goal_key = redaction.redact_goal_key(&w.goal);
                cloned
            })
            .collect();
        let payload = json!({
            "store": store.store_label(),
            "manifest": config.manifest,
            "walks": exported_walks,
        });
        let redacted = redaction.redact_json(&payload);
        deps.output
            .writeln_stdout(&serde_json::to_string_pretty(&redacted).expect("list JSON"));
    } else if walks.is_empty() {
        deps.output
            .writeln_stdout("No walks recorded in this Déjà store.");
    } else {
        for walk in &walks {
            let status = if walk.last_validation.passed {
                "passed"
            } else {
                "failed"
            };
            let fp_short: String = walk.build_fingerprint.chars().take(12).collect();
            deps.output.writeln_stdout(&format!(
                "{} | {} | {} | {} | {} | {} | {}",
                redaction.redact_str(&walk.goal),
                walk.platform,
                redaction.redact_str(&walk.app_id),
                fp_short,
                status,
                walk.last_validation.at,
                redaction.redact_str(&walk.flow_path),
            ));
        }
    }

    EXIT_OK
}

fn emit_list_error(deps: &CliDeps, json_mode: bool, code: &str, message: &str) {
    if json_mode {
        let payload = json!({
            "error": { "code": code, "message": message }
        });
        deps.output
            .writeln_stdout(&serde_json::to_string_pretty(&payload).expect("error JSON"));
    } else {
        deps.output
            .writeln_stderr(&format!("deja list error: {message}"));
    }
}
