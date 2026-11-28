//! `deja migrate` — read-only importer from legacy `walks.json` into SQLite v5 plus config
//! migration hints. Never modifies or deletes `walks.json`.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use deja_store::{
    LEGACY_JSON_MEMORY_PATH, SqliteStoreOptions, SqliteWalkStore, canonical_metadata_target,
    import_legacy_walks_json, parse_legacy_walks_json, store_metadata_path, validate_store_id,
};
use serde_json::json;

use crate::CliDeps;
use crate::cli::MigrateArgs;
use crate::config::{CommandCapability, LoadConfigOptions, load_config};
use crate::exit_codes::{EXIT_OK, EXIT_USAGE_OR_CONFIG, exit_code_for_store_error};
use crate::fs_safe::read_to_string_nofollow_bounded;
use crate::output::{RedactionContext, workspace_relative_display};

const MAX_LEGACY_JSON_BYTES: u64 = 32 * 1024 * 1024;

pub fn run(
    args: &MigrateArgs,
    explicit_config: Option<&Path>,
    json_mode: bool,
    deps: &CliDeps,
) -> u8 {
    if let Some(ref store_id) = args.store
        && let Err(err) = validate_store_id(store_id)
    {
        emit_migrate_error(deps, json_mode, "INVALID_STORE_ID", &err.to_string());
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
            emit_migrate_error(
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

    let legacy_rel = match store_metadata_path(LEGACY_JSON_MEMORY_PATH, args.store.as_deref()) {
        Ok(p) => p,
        Err(err) => {
            let code = exit_code_for_store_error(&err);
            emit_migrate_error(
                deps,
                json_mode,
                err.code(),
                &redaction.redact_str(&err.to_string()),
            );
            return code;
        }
    };

    let target = match canonical_metadata_target(&config.workspace_root, Path::new(&legacy_rel)) {
        Ok(t) => t,
        Err(err) => {
            let code = exit_code_for_store_error(&err);
            emit_migrate_error(
                deps,
                json_mode,
                err.code(),
                &redaction.redact_str(&err.to_string()),
            );
            return code;
        }
    };

    if !target.exists {
        let payload = json!({
            "dryRun": args.dry_run,
            "sourcePath": legacy_rel,
            "status": "nothing-to-migrate",
            "importedWalks": 0,
            "importedValidationRuns": 0,
            "configHints": config.warnings,
        });
        let redacted = redaction.redact_json(&payload);
        if json_mode {
            deps.output
                .writeln_stdout(&serde_json::to_string_pretty(&redacted).expect("migrate JSON"));
        } else {
            deps.output.writeln_stdout(&format!(
                "Déjà migrate: no legacy {legacy_rel} found; nothing to import."
            ));
            for hint in &config.warnings {
                deps.output.writeln_stdout(&format!("  hint: {hint}"));
            }
        }
        return EXIT_OK;
    }

    if args.dry_run {
        let raw = match read_to_string_nofollow_bounded(&target.path, MAX_LEGACY_JSON_BYTES) {
            Ok(text) => text,
            Err(err) => {
                let code = if err.kind() == ErrorKind::NotFound {
                    EXIT_USAGE_OR_CONFIG
                } else {
                    crate::exit_codes::EXIT_INVALID_MEMORY
                };
                emit_migrate_error(
                    deps,
                    json_mode,
                    "INVALID_WALK_MEMORY",
                    &redaction.redact_str(&format!("cannot read {legacy_rel}: {err}")),
                );
                return code;
            }
        };
        let (schema_version, walks) = match parse_legacy_walks_json(&raw) {
            Ok(parsed) => parsed,
            Err(err) => {
                let code = exit_code_for_store_error(&err);
                emit_migrate_error(
                    deps,
                    json_mode,
                    err.code(),
                    &redaction.redact_str(&err.to_string()),
                );
                return code;
            }
        };

        let goals: Vec<&str> = walks.iter().map(|w| w.goal.as_str()).collect();
        let payload = json!({
            "dryRun": true,
            "sourcePath": workspace_relative_display(&config.workspace_root, &target.path),
            "sourceSchemaVersion": schema_version,
            "walksToImport": walks.len(),
            "validationRunsToImport": walks.len(),
            "goals": goals,
            "walksJsonModified": false,
            "configHints": config.warnings,
        });
        let redacted = redaction.redact_json(&payload);
        if json_mode {
            deps.output
                .writeln_stdout(&serde_json::to_string_pretty(&redacted).expect("migrate JSON"));
        } else {
            deps.output.writeln_stdout(&format!(
                "Déjà migrate (dry-run): would import {} walk(s) from {} (schema v{}) into SQLite v5 without modifying walks.json.",
                walks.len(),
                legacy_rel,
                schema_version
            ));
            for hint in &config.warnings {
                deps.output.writeln_stdout(&format!("  hint: {hint}"));
            }
        }
        return EXIT_OK;
    }

    let mut store_opts = SqliteStoreOptions::new(&config.workspace_root);
    if let Some(ref store_id) = args.store {
        store_opts = store_opts.with_store_id(store_id.clone());
    }
    let store = match SqliteWalkStore::open_writable(store_opts) {
        Ok(s) => s,
        Err(err) => {
            let code = exit_code_for_store_error(&err);
            emit_migrate_error(
                deps,
                json_mode,
                err.code(),
                &redaction.redact_str(&err.to_string()),
            );
            return code;
        }
    };

    let report = match import_legacy_walks_json(&store, Some(&PathBuf::from(&legacy_rel))) {
        Ok(rep) => rep,
        Err(err) => {
            store.close();
            let code = exit_code_for_store_error(&err);
            emit_migrate_error(
                deps,
                json_mode,
                err.code(),
                &redaction.redact_str(&err.to_string()),
            );
            return code;
        }
    };
    let target_db = store.relative_memory_path().to_string();
    store.close();

    let payload = json!({
        "dryRun": false,
        "sourcePath": workspace_relative_display(&config.workspace_root, &report.source_path),
        "targetStore": target_db,
        "sourceSchemaVersion": report.source_schema_version,
        "importedWalks": report.imported_walks,
        "importedValidationRuns": report.imported_validation_runs,
        "walksJsonModified": false,
        "configHints": config.warnings,
    });
    let redacted = redaction.redact_json(&payload);
    if json_mode {
        deps.output
            .writeln_stdout(&serde_json::to_string_pretty(&redacted).expect("migrate JSON"));
    } else {
        deps.output.writeln_stdout(&format!(
            "Déjà migrate: imported {} walk(s) and {} validation run(s) from {} (schema v{}) into {} (walks.json left unchanged).",
            report.imported_walks,
            report.imported_validation_runs,
            legacy_rel,
            report.source_schema_version,
            target_db,
        ));
        for hint in &config.warnings {
            deps.output.writeln_stdout(&format!("  hint: {hint}"));
        }
    }

    EXIT_OK
}

fn emit_migrate_error(deps: &CliDeps, json_mode: bool, code: &str, message: &str) {
    if json_mode {
        let payload = json!({
            "error": { "code": code, "message": message }
        });
        deps.output
            .writeln_stdout(&serde_json::to_string_pretty(&payload).expect("error JSON"));
    } else {
        deps.output
            .writeln_stderr(&format!("deja migrate error: {message}"));
    }
}
