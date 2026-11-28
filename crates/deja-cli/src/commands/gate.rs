//! `deja gate` — strictly read-only multi-store, runtime-aware release gate with tamper-checked
//! `O_EXCL` JSON + `.txt` evidence output (`DEJA-49`, `DEJA-55`, `DEJA-42`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use deja_core::{
    DEFAULT_FLOW_SCHEMA_VERSION, GateFlakyFailure, GateIdentity, GateOutcome, RunOutcome,
    fingerprint_file, fingerprint_tree,
};
use deja_store::{
    ConsumerFlows, ConsumerFlowsOptions, SqliteStoreOptions, SqliteWalkStore, StoreGateGoalResult,
    StoreGateOptions, evaluate_gate, now_iso8601, store_gate_passed, validate_store_id,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::CliDeps;
use crate::cli::GateArgs;
use crate::commands::exec_common::parse_goals_file;
use crate::commands::manifest::has_unsafe_control_or_bidi_char;
use crate::config::{CommandCapability, DEFAULT_CONFIG_PATH, LoadConfigOptions, load_config};
use crate::exit_codes::{EXIT_GATE_CLOSED, EXIT_GATE_ERROR, EXIT_OK, EXIT_USAGE_OR_CONFIG};
use crate::fs_safe::{
    GateEvidenceError, lexical_normalize, precheck_gate_evidence_paths,
    read_to_string_nofollow_bounded, remove_verified_gate_evidence, sibling_evidence_txt_path,
    write_atomic_pair_nofollow_excl,
};
use crate::output::{CliRunReport, RedactionContext, workspace_relative_display};

const MAX_REPORT_SCAN_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GateEvidenceGoal {
    goal: String,
    goal_key: String,
    outcome: GateOutcome,
    validated_at: Option<String>,
    flow_sha256: Option<String>,
    closure_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    report_path: Option<String>,
    #[serde(default)]
    source_stores: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GateEvidenceIdentity {
    app_id: String,
    platform: String,
    flow_schema_version: String,
    build_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    adapter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    browser: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GateEvidenceDocument {
    schema_version: u32,
    kind: String,
    verdict: String,
    evaluated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runtime: Option<String>,
    stores: Vec<String>,
    identity: GateEvidenceIdentity,
    goals: Vec<GateEvidenceGoal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub fn run(args: &GateArgs, explicit_config: Option<&Path>, json_mode: bool, deps: &CliDeps) -> u8 {
    if let Some(ref label) = args.label
        && (label.trim().is_empty() || has_unsafe_control_or_bidi_char(label))
    {
        invalidate_prior_gate_evidence_early(args, explicit_config, deps);
        emit_gate_error(
            deps,
            json_mode,
            "INVALID_LABEL",
            "`--label` must be non-empty and must not contain control, line-break, bidi, or zero-width characters",
        );
        return EXIT_USAGE_OR_CONFIG;
    }
    if let Some(ref rt) = args.runtime
        && (rt.trim().is_empty() || has_unsafe_control_or_bidi_char(rt))
    {
        invalidate_prior_gate_evidence_early(args, explicit_config, deps);
        emit_gate_error(
            deps,
            json_mode,
            "INVALID_RUNTIME",
            "`--runtime` must be non-empty and must not contain control or bidi characters",
        );
        return EXIT_USAGE_OR_CONFIG;
    }

    let mut store_ids: Vec<String> = Vec::new();
    let mut seen_stores = BTreeSet::new();
    for id in &args.stores {
        if let Err(err) = validate_store_id(id) {
            invalidate_prior_gate_evidence_early(args, explicit_config, deps);
            emit_gate_error(deps, json_mode, "INVALID_STORE_ID", &err.to_string());
            return EXIT_USAGE_OR_CONFIG;
        }
        if seen_stores.insert(id.clone()) {
            store_ids.push(id.clone());
        }
    }

    let config = match load_config(&LoadConfigOptions {
        cwd: &deps.cwd,
        explicit_config,
        capability: CommandCapability::Gate,
        device_override: None,
        build_override: args.build.as_deref(),
        env: &deps.env,
    }) {
        Ok(cfg) => cfg,
        Err(err) => {
            invalidate_prior_gate_evidence_early(args, explicit_config, deps);
            let redaction =
                RedactionContext::new(&deps.cwd, deps.home_dir.as_deref(), &[], &deps.env);
            emit_gate_error(
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

    // Resolve raw goals path and evidence paths FIRST, then precheck & unlink verified previous
    // evidence BEFORE reading/validating the goals file so an invalid or missing goals file
    // never leaves a stale passing evidence file behind!
    let goals_abs = if args.goals.is_absolute() {
        lexical_normalize(&args.goals)
    } else {
        lexical_normalize(&deps.cwd.join(&args.goals))
    };

    let evidence_json_path = match &args.evidence {
        Some(p) if p.is_absolute() => lexical_normalize(p),
        Some(p) => lexical_normalize(&deps.cwd.join(p)),
        None => config.workspace_root.join(".deja/gate/evidence.json"),
    };
    let evidence_txt_path = sibling_evidence_txt_path(&evidence_json_path);

    let mut forbidden_paths = vec![goals_abs.clone(), config.config_path.clone()];
    if let Some(ref manifest_rel) = config.manifest {
        forbidden_paths.push(config.workspace_root.join(manifest_rel));
    }

    let precheck = match precheck_gate_evidence_paths(
        &config.workspace_root,
        &evidence_json_path,
        &evidence_txt_path,
        &forbidden_paths,
        render_gate_table_from_json,
    ) {
        Ok(p) => p,
        Err(GateEvidenceError::Refused(msg)) => {
            emit_gate_error(
                deps,
                json_mode,
                "EVIDENCE_PATH_REFUSED",
                &redaction.redact_str(&msg),
            );
            return EXIT_USAGE_OR_CONFIG;
        }
        Err(GateEvidenceError::Io(msg)) => {
            emit_gate_error(
                deps,
                json_mode,
                "EVIDENCE_IO_ERROR",
                &redaction.redact_str(&msg),
            );
            return EXIT_GATE_ERROR;
        }
    };

    // Delete any previously verified evidence BEFORE validating goals or evaluating so stale
    // passes never survive invalid-goals or runtime errors.
    if let Err(err) = remove_verified_gate_evidence(&precheck) {
        let code = match err {
            GateEvidenceError::Refused(_) => EXIT_USAGE_OR_CONFIG,
            GateEvidenceError::Io(_) => EXIT_GATE_ERROR,
        };
        emit_gate_error(
            deps,
            json_mode,
            "EVIDENCE_DELETE_FAILED",
            &redaction.redact_str(&err.to_string()),
        );
        return code;
    }

    // Now load & strictly validate the goals file.
    let goals = match parse_goals_file(&goals_abs) {
        Ok(g) => g,
        Err(msg) => {
            emit_gate_error(
                deps,
                json_mode,
                "INVALID_GOALS",
                &redaction.redact_str(&msg),
            );
            return EXIT_USAGE_OR_CONFIG;
        }
    };

    let evaluated_at = now_iso8601();
    let build_path = config
        .build_path
        .as_ref()
        .expect("Gate capability requires build_path");

    // Open all read-only SQLite stores (zero writer locks, zero files created).
    let mut stores: Vec<SqliteWalkStore> = Vec::new();
    let mut open_error: Option<String> = None;
    if store_ids.is_empty() {
        match SqliteWalkStore::open_readonly(SqliteStoreOptions::new(&config.workspace_root)) {
            Ok(s) => stores.push(s),
            Err(err) => open_error = Some(err.to_string()),
        }
    } else {
        for id in &store_ids {
            match SqliteWalkStore::open_readonly(
                SqliteStoreOptions::new(&config.workspace_root).with_store_id(id.clone()),
            ) {
                Ok(s) => stores.push(s),
                Err(err) => {
                    open_error = Some(format!("store {id}: {err}"));
                    break;
                }
            }
        }
    }

    let store_labels: Vec<String> = if !stores.is_empty() {
        stores.iter().map(SqliteWalkStore::store_label).collect()
    } else if !store_ids.is_empty() {
        store_ids.clone()
    } else {
        vec![deja_store::SQLITE_MEMORY_PATH.to_string()]
    };

    // Fingerprint the build (read-only).
    let fingerprint_res: Result<String, String> = if open_error.is_none() {
        fingerprint_build_readonly(
            &config.workspace_root,
            build_path,
            &config.platform,
            stores.first(),
        )
    } else {
        Err(open_error.clone().unwrap_or_default())
    };

    let (verdict, exit_code, build_fingerprint, goal_entries, top_error) =
        match (&open_error, fingerprint_res) {
            (Some(err), _) => (
                "error".to_string(),
                EXIT_GATE_ERROR,
                None,
                Vec::new(),
                Some(err.clone()),
            ),
            (None, Err(err)) => (
                "error".to_string(),
                EXIT_GATE_ERROR,
                None,
                Vec::new(),
                Some(format!("failed to fingerprint build: {err}")),
            ),
            (None, Ok(fp)) => {
                let scanned = scan_reports_for_build(&config.workspace_root, &fp);
                let consumer = match &config.manifest {
                    Some(manifest_rel) => {
                        let mut opts =
                            ConsumerFlowsOptions::new(&config.workspace_root, manifest_rel)
                                .with_forbidden_values(config.forbidden_values.clone());
                        if let Some(ref fdir) = config.flows_dir {
                            opts = opts.with_flows_dir(fdir);
                        }
                        match ConsumerFlows::new(opts) {
                            Ok(c) => Some(c),
                            Err(err) => {
                                return finalize_and_write_evidence(
                                    deps,
                                    json_mode,
                                    &redaction,
                                    &precheck.json_path,
                                    &precheck.txt_path,
                                    GateEvidenceDocument {
                                        schema_version: 1,
                                        kind: "deja-gate-evidence".to_string(),
                                        verdict: "error".to_string(),
                                        evaluated_at,
                                        label: args.label.clone(),
                                        runtime: args.runtime.clone(),
                                        stores: store_labels,
                                        identity: GateEvidenceIdentity {
                                            app_id: config.app_id.clone(),
                                            platform: config.platform.clone(),
                                            flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION
                                                .to_string(),
                                            build_fingerprint: Some(fp),
                                            adapter: config.adapter.clone(),
                                            browser: config.browser.clone(),
                                        },
                                        goals: Vec::new(),
                                        error: Some(err.to_string()),
                                    },
                                    EXIT_GATE_ERROR,
                                );
                            }
                        }
                    }
                    None => None,
                };

                let gate_identity = GateIdentity {
                    app_id: config.app_id.clone(),
                    platform: config.platform.clone(),
                    flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
                    build_fingerprint: fp.clone(),
                    adapter: config.adapter.clone(),
                    browser: config.browser.clone(),
                };
                let store_refs: Vec<&SqliteWalkStore> = stores.iter().collect();
                let gate_opts = StoreGateOptions {
                    runtime: args.runtime.as_deref(),
                    flaky_failures: &scanned.flaky_failures,
                    consumer: consumer.as_ref(),
                };

                match evaluate_gate(&store_refs, &goals, &gate_identity, &gate_opts) {
                    Ok(results) => {
                        let has_invalid = results.iter().any(|r| r.outcome == GateOutcome::Invalid);
                        let passed = store_gate_passed(&results);
                        let (verdict, code) = if has_invalid {
                            ("error".to_string(), EXIT_GATE_ERROR)
                        } else if passed {
                            ("passed".to_string(), EXIT_OK)
                        } else {
                            ("closed".to_string(), EXIT_GATE_CLOSED)
                        };
                        let entries = results
                            .into_iter()
                            .map(|r| to_evidence_goal(r, &fp, &scanned.report_paths, &redaction))
                            .collect();
                        (verdict, code, Some(fp), entries, None)
                    }
                    Err(err) => (
                        "error".to_string(),
                        EXIT_GATE_ERROR,
                        Some(fp),
                        Vec::new(),
                        Some(err.to_string()),
                    ),
                }
            }
        };

    let doc = GateEvidenceDocument {
        schema_version: 1,
        kind: "deja-gate-evidence".to_string(),
        verdict,
        evaluated_at,
        label: args.label.clone(),
        runtime: args.runtime.clone(),
        stores: store_labels,
        identity: GateEvidenceIdentity {
            app_id: config.app_id.clone(),
            platform: config.platform.clone(),
            flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
            build_fingerprint,
            adapter: config.adapter.clone(),
            browser: config.browser.clone(),
        },
        goals: goal_entries,
        error: top_error,
    };

    finalize_and_write_evidence(
        deps,
        json_mode,
        &redaction,
        &precheck.json_path,
        &precheck.txt_path,
        doc,
        exit_code,
    )
}

fn finalize_and_write_evidence(
    deps: &CliDeps,
    json_mode: bool,
    redaction: &RedactionContext,
    json_path: &Path,
    txt_path: &Path,
    doc: GateEvidenceDocument,
    exit_code: u8,
) -> u8 {
    let raw_val = serde_json::to_value(&doc).expect("gate evidence serializes");
    let redacted_val = redaction.redact_json(&raw_val);
    let mut json_text =
        serde_json::to_string_pretty(&redacted_val).expect("redacted evidence JSON serializes");
    json_text.push('\n');

    let table_text = match render_gate_table_from_json(&redacted_val) {
        Ok(t) => t,
        Err(err) => {
            emit_gate_error(
                deps,
                json_mode,
                "EVIDENCE_RENDER_ERROR",
                &redaction.redact_str(&err),
            );
            return EXIT_GATE_ERROR;
        }
    };

    // Stage BOTH `.txt` and `.json` temporaries first, publish `.txt` BEFORE `.json`, and clean up
    // on any failure so a passing `.json` is NEVER published without its matching `.txt` table.
    if let Err(err) = write_atomic_pair_nofollow_excl(
        txt_path,
        table_text.as_bytes(),
        json_path,
        json_text.as_bytes(),
    ) {
        emit_gate_error(
            deps,
            json_mode,
            "EVIDENCE_WRITE_FAILED",
            &redaction.redact_str(&format!("failed to write gate evidence: {err}")),
        );
        return EXIT_GATE_ERROR;
    }

    if json_mode {
        deps.output.write_stdout(&json_text);
    } else {
        deps.output.write_stdout(&table_text);
    }
    exit_code
}

fn to_evidence_goal(
    r: StoreGateGoalResult,
    build_fingerprint: &str,
    report_paths: &BTreeMap<(String, String, String), String>,
    redaction: &RedactionContext,
) -> GateEvidenceGoal {
    // Prefer the exact `report_path` persisted on the selected SQLite validation row so
    // concurrent stores finishing at identical timestamps never misattribute reports.
    let report_path = r.report_path.or_else(|| {
        r.validated_at.as_ref().and_then(|at| {
            report_paths
                .get(&(
                    r.goal_key.clone(),
                    build_fingerprint.to_string(),
                    at.clone(),
                ))
                .cloned()
        })
    });
    // Derive exported `goalKey` by redacting `r.goal` BEFORE lowercasing/normalizing so
    // mixed-case secrets never leak in lowercased form through `goalKey`.
    let safe_goal_key = redaction.redact_goal_key(&r.goal);
    GateEvidenceGoal {
        goal: r.goal,
        goal_key: safe_goal_key,
        outcome: r.outcome,
        validated_at: r.validated_at,
        flow_sha256: r.flow_sha256,
        closure_sha256: r.closure_sha256,
        detail: r.detail,
        report_path,
        source_stores: r.source_stores,
    }
}

fn fingerprint_build_readonly(
    workspace_root: &Path,
    build_path: &Path,
    platform: &str,
    first_store: Option<&SqliteWalkStore>,
) -> Result<String, String> {
    if let Some(store) = first_store {
        return store
            .fingerprint_with_cache(workspace_root, build_path, platform)
            .map(|r| r.digest)
            .map_err(|err| err.to_string());
    }
    let meta = fs::metadata(build_path).map_err(|err| err.to_string())?;
    if meta.is_dir() {
        fingerprint_tree(workspace_root, build_path, platform).map_err(|err| err.to_string())
    } else {
        fingerprint_file(workspace_root, build_path).map_err(|err| err.to_string())
    }
}

/// Deterministic table renderer built SOLELY from the serialized gate evidence JSON so that
/// subsequent runs can verify sibling `.txt` integrity byte-for-byte.
pub fn render_gate_table_from_json(val: &Value) -> Result<String, String> {
    let obj = val
        .as_object()
        .ok_or_else(|| "gate evidence must be a JSON object".to_string())?;
    let verdict = obj
        .get("verdict")
        .and_then(Value::as_str)
        .ok_or_else(|| "gate evidence missing verdict".to_string())?;
    let evaluated_at = obj
        .get("evaluatedAt")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let identity = obj
        .get("identity")
        .and_then(Value::as_object)
        .ok_or_else(|| "gate evidence missing identity".to_string())?;
    let app_id = identity
        .get("appId")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let platform = identity
        .get("platform")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let fp = identity
        .get("buildFingerprint")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    let mut out = String::new();
    out.push_str(&format!("Déjà release gate: {verdict}\n"));
    out.push_str(&format!("Evaluated at: {evaluated_at}\n"));
    out.push_str(&format!("App: {app_id} ({platform})\n"));
    out.push_str(&format!("Build fingerprint: {fp}\n"));
    if let Some(label) = obj.get("label").and_then(Value::as_str) {
        out.push_str(&format!("Label: {label}\n"));
    }
    if let Some(rt) = obj.get("runtime").and_then(Value::as_str) {
        out.push_str(&format!("Runtime: {rt}\n"));
    }
    if let Some(stores) = obj.get("stores").and_then(Value::as_array) {
        let names: Vec<&str> = stores.iter().filter_map(Value::as_str).collect();
        out.push_str(&format!("Stores: {}\n", names.join(", ")));
    }
    if let Some(err) = obj.get("error").and_then(Value::as_str) {
        out.push_str(&format!("Error: {err}\n"));
    }

    if let Some(goals) = obj.get("goals").and_then(Value::as_array)
        && !goals.is_empty()
    {
        out.push_str("\nGOAL\tOUTCOME\tVALIDATED_AT\tSTORES\tDETAIL\n");
        for g in goals {
            let goal = g.get("goal").and_then(Value::as_str).unwrap_or("");
            let outcome = g
                .get("outcome")
                .and_then(Value::as_str)
                .unwrap_or("invalid");
            let validated_at = g.get("validatedAt").and_then(Value::as_str).unwrap_or("-");
            let stores = g
                .get("sourceStores")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "-".to_string());
            let detail = g.get("detail").and_then(Value::as_str).unwrap_or("-");
            out.push_str(&format!(
                "{goal}\t{outcome}\t{validated_at}\t{stores}\t{detail}\n"
            ));
        }
    }
    Ok(out)
}

#[derive(Default)]
struct ScannedReports {
    flaky_failures: Vec<GateFlakyFailure>,
    report_paths: BTreeMap<(String, String, String), String>,
}

fn scan_reports_for_build(workspace_root: &Path, build_fingerprint: &str) -> ScannedReports {
    let mut out = ScannedReports::default();
    let reports_dir = workspace_root.join(".deja/reports");
    let Ok(entries) = fs::read_dir(&reports_dir) else {
        return out;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()) == Some("json")
                && p.file_name().and_then(|s| s.to_str()) != Some("latest.json")
        })
        .collect();
    paths.sort();

    for path in paths {
        let Ok(raw) = read_to_string_nofollow_bounded(&path, MAX_REPORT_SCAN_BYTES) else {
            continue;
        };
        let Ok(rep) = serde_json::from_str::<CliRunReport>(&raw) else {
            continue;
        };
        if rep.core.identity.build_fingerprint != build_fingerprint {
            continue;
        }
        let rel = workspace_relative_display(workspace_root, &path);
        if rep.core.outcome == RunOutcome::Flaky {
            let attempt1_at = rep
                .core
                .attempts
                .as_ref()
                .and_then(|a| a.first())
                .map(|a| a.ended_at.clone())
                .unwrap_or_else(|| rep.core.ended_at.clone());
            out.flaky_failures.push(GateFlakyFailure {
                goal_key: rep.core.goal_key.clone(),
                build_fingerprint: build_fingerprint.to_string(),
                at: attempt1_at.clone(),
            });
            out.report_paths.insert(
                (
                    rep.core.goal_key.clone(),
                    build_fingerprint.to_string(),
                    attempt1_at,
                ),
                rel,
            );
        } else {
            let at = rep
                .core
                .execution
                .as_ref()
                .map(|e| e.ended_at.clone())
                .unwrap_or_else(|| rep.core.ended_at.clone());
            out.report_paths.insert(
                (rep.core.goal_key.clone(), build_fingerprint.to_string(), at),
                rel,
            );
        }
    }
    out
}

/// Best-effort, fail-safe invalidation of previously verified `deja-gate-evidence` files
/// before early validation errors (`--label`, `--runtime`, `--store`, malformed config) exit.
///
/// Always runs the full [`precheck_gate_evidence_paths`] safety checks (symlink refusal,
/// `.deja/` outside `.deja/gate/` refusal, protected input path refusal, `"kind":
/// "deja-gate-evidence"` verification, and byte-exact `.txt` table match) so unrelated user
/// files are NEVER touched.
fn invalidate_prior_gate_evidence_early(
    args: &GateArgs,
    explicit_config: Option<&Path>,
    deps: &CliDeps,
) {
    let config_abs = match explicit_config {
        Some(p) if p.is_absolute() => lexical_normalize(p),
        Some(p) => lexical_normalize(&deps.cwd.join(p)),
        None => lexical_normalize(&deps.cwd.join(DEFAULT_CONFIG_PATH)),
    };
    let config_dir = config_abs
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| deps.cwd.clone());

    // Best-effort workspace root resolution: if config is readable JSON with a valid
    // `workspaceRoot`, honor it (for example when `--label` failed on a valid config pointing
    // to a sibling workspace); otherwise fall back to `config_dir` / `deps.cwd`.
    let mut forbidden_paths = vec![
        if args.goals.is_absolute() {
            lexical_normalize(&args.goals)
        } else {
            lexical_normalize(&deps.cwd.join(&args.goals))
        },
        config_abs.clone(),
    ];

    let mut candidate_ws = fs::canonicalize(&config_dir)
        .or_else(|_| fs::canonicalize(&deps.cwd))
        .unwrap_or_else(|_| lexical_normalize(&deps.cwd));
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
            forbidden_paths.push(lexical_normalize(&candidate_ws.join(manifest_str)));
        }
    }

    let evidence_json_path = match &args.evidence {
        Some(p) if p.is_absolute() => lexical_normalize(p),
        Some(p) => lexical_normalize(&deps.cwd.join(p)),
        None => candidate_ws.join(".deja/gate/evidence.json"),
    };
    let evidence_txt_path = sibling_evidence_txt_path(&evidence_json_path);

    if let Ok(precheck) = precheck_gate_evidence_paths(
        &candidate_ws,
        &evidence_json_path,
        &evidence_txt_path,
        &forbidden_paths,
        render_gate_table_from_json,
    ) {
        let _ = remove_verified_gate_evidence(&precheck);
    }
}

fn emit_gate_error(deps: &CliDeps, json_mode: bool, code: &str, message: &str) {
    if json_mode {
        let payload = json!({
            "error": { "code": code, "message": message }
        });
        deps.output
            .writeln_stdout(&serde_json::to_string_pretty(&payload).expect("error JSON"));
    } else {
        deps.output
            .writeln_stderr(&format!("deja gate error: {message}"));
    }
}
