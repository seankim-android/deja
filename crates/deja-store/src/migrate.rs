//! One-way read-only importer from legacy `walks.json` (schemas 1–4) into SQLite v5.
//!
//! Never modifies, rewrites, or deletes `walks.json`. Imports all validated walks inside a
//! single `BEGIN IMMEDIATE` transaction while preserving `legacy_closure = 1` for schemas 1–2
//! and rejecting invalid identities, timestamps, checksums, or duplicate keys.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use deja_core::{
    DEFAULT_FLOW_SCHEMA_VERSION, ExplorationDetails, RepairProvenance, StoredFlowFile,
    StoredValidation, StoredWalk, WalkOrigin, WalkValidation, check_manifest_flow_path,
    normalize_goal, read_stored_flow_files,
};
use rusqlite::params;
use serde_json::Value;

use crate::error::StoreError;
use crate::lock::{
    LEGACY_JSON_MEMORY_PATH, canonical_metadata_target, is_eloop, is_valid_iso8601,
    open_read_nofollow, store_metadata_path,
};
use crate::schema::{
    ValidationRunIntegrityFields, WalkIntegrityFields, compute_repair_history_integrity,
    compute_validation_run_integrity, compute_walk_file_integrity, compute_walk_integrity,
    is_non_empty, is_sha256_hex, origin_to_str, parse_origin_str, validate_repair_provenance,
};
use crate::store::{SqliteWalkStore, append_validation_history};

/// Summary returned after importing a legacy `walks.json` file into SQLite v5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyImportReport {
    pub source_path: PathBuf,
    pub source_schema_version: u32,
    pub imported_walks: usize,
    pub imported_validation_runs: usize,
}

/// Read-only legacy `walks.json` (schemas 1–4) importer into a writable [`SqliteWalkStore`].
///
/// Resolves the legacy JSON path inside `store.workspace_root()` (respecting `store.store_id()`
/// when `json_path` is `None`), verifies workspace confinement and refuses symlinks via
/// `O_NOFOLLOW`, validates every walk and rejects duplicate JSON keys & duplicate identities,
/// then imports all records atomically in a single `BEGIN IMMEDIATE` SQLite transaction without
/// touching the source `walks.json` file.
pub fn import_legacy_walks_json(
    store: &SqliteWalkStore,
    json_path: Option<&Path>,
) -> Result<LegacyImportReport, StoreError> {
    let source_path = match json_path {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(store_metadata_path(
            LEGACY_JSON_MEMORY_PATH,
            store.store_id(),
        )?),
    };

    let target = canonical_metadata_target(store.workspace_root(), &source_path)?;
    if !target.exists {
        return Err(StoreError::InvalidWalkMemory(format!(
            "legacy walks.json does not exist: {}",
            source_path.display()
        )));
    }

    let mut file = open_read_nofollow(&target.path).map_err(|err| {
        if is_eloop(&err) {
            StoreError::InvalidWalkMemory(format!(
                "legacy walks.json is a symbolic link: {}",
                target.path.display()
            ))
        } else {
            StoreError::InvalidWalkMemory(format!(
                "legacy walks.json cannot be read ({}): {err}",
                target.path.display()
            ))
        }
    })?;
    let mut contents = String::new();
    file.read_to_string(&mut contents).map_err(|err| {
        StoreError::InvalidWalkMemory(format!("legacy walks.json cannot be read: {err}"))
    })?;

    let (source_schema_version, walks) = parse_legacy_walks_json(&contents)?;

    let mut distinct_walk_ids = HashSet::new();
    let mut imported_runs = 0usize;

    store.with_write_tx(|tx| {
        for walk in &walks {
            let has_files = walk.closure_sha256.is_none();
            let legacy_closure = if has_files {
                source_schema_version <= 2 || walk.legacy_closure == Some(true)
            } else {
                false
            };
            let origin_str = walk.origin.map(origin_to_str);
            let exploration_json = walk
                .exploration
                .as_ref()
                .map(|e| serde_json::to_string(e).expect("ExplorationDetails serializes"));
            let subflow_list: &[StoredFlowFile] = walk.subflow_files.as_deref().unwrap_or(&[]);
            let subflow_json = if has_files && !legacy_closure {
                Some(serde_json::to_string(subflow_list).expect("subflow_files serialize"))
            } else {
                None
            };

            let walk_integrity = compute_walk_integrity(&WalkIntegrityFields {
                goal: &walk.goal,
                goal_key: &walk.goal_key,
                app_id: &walk.app_id,
                platform: &walk.platform,
                flow_schema_version: &walk.flow_schema_version,
                build_fingerprint: &walk.build_fingerprint,
                adapter: walk.adapter.as_deref(),
                browser: walk.browser.as_deref(),
                flow_path: &walk.flow_path,
                flow_sha256: &walk.flow_sha256,
                last_passed: walk.last_validation.passed,
                last_at: &walk.last_validation.at,
                origin: origin_str,
                exploration_json: exploration_json.as_deref(),
                has_files,
                legacy_closure,
                closure_sha256: walk.closure_sha256.as_deref(),
            });

            let walk_id: i64 = tx.query_row(
                "INSERT INTO walks (
                    goal, goal_key, app_id, platform, flow_schema_version, build_fingerprint,
                    adapter, browser, flow_path, flow_sha256, last_passed, last_at, origin,
                    exploration_json, has_files, legacy_closure, closure_sha256, integrity_sha256
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
                ON CONFLICT(goal_key, app_id, platform, flow_schema_version, adapter_key, browser_key)
                DO UPDATE SET
                    goal = excluded.goal,
                    build_fingerprint = excluded.build_fingerprint,
                    flow_path = excluded.flow_path,
                    flow_sha256 = excluded.flow_sha256,
                    last_passed = excluded.last_passed,
                    last_at = excluded.last_at,
                    origin = excluded.origin,
                    exploration_json = excluded.exploration_json,
                    has_files = excluded.has_files,
                    legacy_closure = excluded.legacy_closure,
                    closure_sha256 = excluded.closure_sha256,
                    integrity_sha256 = excluded.integrity_sha256
                RETURNING id",
                params![
                    walk.goal,
                    walk.goal_key,
                    walk.app_id,
                    walk.platform,
                    walk.flow_schema_version,
                    walk.build_fingerprint,
                    walk.adapter,
                    walk.browser,
                    walk.flow_path,
                    walk.flow_sha256,
                    i64::from(walk.last_validation.passed),
                    walk.last_validation.at,
                    origin_str,
                    exploration_json,
                    i64::from(has_files),
                    i64::from(legacy_closure),
                    walk.closure_sha256,
                    walk_integrity,
                ],
                |r| r.get(0),
            )?;
            distinct_walk_ids.insert(walk_id);

            let run_integrity = compute_validation_run_integrity(&ValidationRunIntegrityFields {
                walk_id,
                build_fingerprint: &walk.build_fingerprint,
                runtime: None,
                flow_path: &walk.flow_path,
                flow_sha256: &walk.flow_sha256,
                closure_sha256: walk.closure_sha256.as_deref(),
                subflow_files_json: subflow_json.as_deref(),
                passed: walk.last_validation.passed,
                flaky: false,
                validated_at: &walk.last_validation.at,
                origin: origin_str,
                failure_category: None,
                report_path: None,
                duration_ms: None,
                phase_timings_json: None,
            });
            tx.execute(
                "INSERT INTO validation_runs (
                    walk_id, build_fingerprint, runtime, flow_path, flow_sha256, closure_sha256,
                    subflow_files_json, passed, flaky, validated_at, origin, failure_category,
                    report_path, duration_ms, phase_timings_json, integrity_sha256
                ) VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9, NULL, NULL, NULL, NULL, ?10)
                ON CONFLICT(walk_id, build_fingerprint, runtime_key) DO UPDATE SET
                    flow_path = excluded.flow_path,
                    flow_sha256 = excluded.flow_sha256,
                    closure_sha256 = excluded.closure_sha256,
                    subflow_files_json = excluded.subflow_files_json,
                    passed = excluded.passed,
                    flaky = excluded.flaky,
                    validated_at = excluded.validated_at,
                    origin = excluded.origin,
                    failure_category = NULL,
                    report_path = NULL,
                    duration_ms = NULL,
                    phase_timings_json = NULL,
                    integrity_sha256 = excluded.integrity_sha256",
                params![
                    walk_id,
                    walk.build_fingerprint,
                    walk.flow_path,
                    walk.flow_sha256,
                    walk.closure_sha256,
                    subflow_json,
                    i64::from(walk.last_validation.passed),
                    walk.last_validation.at,
                    origin_str,
                    run_integrity,
                ],
            )?;
            imported_runs += 1;

            for entry in &walk.validation_history {
                append_validation_history(
                    tx,
                    walk_id,
                    entry.passed,
                    &entry.at,
                    &entry.flow_sha256,
                    Some(&walk.build_fingerprint),
                    None,
                    false,
                )?;
            }

            tx.execute("DELETE FROM walk_files WHERE walk_id = ?1", [walk_id])?;
            if has_files {
                for (pos, wf) in subflow_list.iter().enumerate() {
                    let wf_integrity =
                        compute_walk_file_integrity(walk_id, pos as i64, &wf.path, &wf.sha256);
                    tx.execute(
                        "INSERT INTO walk_files (walk_id, position, path, sha256, integrity_sha256)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![walk_id, pos as i64, wf.path, wf.sha256, wf_integrity],
                    )?;
                }
            }

            if let Some(ref repairs) = walk.repair_history {
                tx.execute("DELETE FROM repair_history WHERE walk_id = ?1", [walk_id])?;
                for (pos, repair) in repairs.iter().enumerate() {
                    let rh_integrity = compute_repair_history_integrity(
                        walk_id,
                        pos as i64,
                        &repair.attempt_id,
                        &repair.patch_path,
                        &repair.previous_build_fingerprint,
                        &repair.previous_flow_sha256,
                    );
                    tx.execute(
                        "INSERT INTO repair_history (
                            walk_id, position, attempt_id, patch_path, previous_build_fingerprint,
                            previous_flow_sha256, integrity_sha256
                        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            walk_id,
                            pos as i64,
                            repair.attempt_id,
                            repair.patch_path,
                            repair.previous_build_fingerprint,
                            repair.previous_flow_sha256,
                            rh_integrity,
                        ],
                    )?;
                }
            }
        }
        Ok(())
    })?;

    Ok(LegacyImportReport {
        source_path: target.path,
        source_schema_version,
        imported_walks: distinct_walk_ids.len(),
        imported_validation_runs: imported_runs,
    })
}

/// Parse and strictly validate legacy `walks.json` contents (schemas 1–4).
pub fn parse_legacy_walks_json(contents: &str) -> Result<(u32, Vec<StoredWalk>), StoreError> {
    let parsed: Value = serde_json::from_str(contents)
        .map_err(|_| StoreError::InvalidWalkMemory("walks.json is not valid JSON".to_string()))?;

    if let Some(dup) = find_duplicate_json_key(contents) {
        return Err(StoreError::InvalidWalkMemory(format!(
            "walks.json contains duplicate JSON key {dup:?}"
        )));
    }

    let Some(root) = parsed.as_object() else {
        return Err(StoreError::InvalidWalkMemory(
            "walks.json requires a schemaVersion and walks array".to_string(),
        ));
    };
    let schema_version = match root.get("schemaVersion").and_then(Value::as_u64) {
        Some(v @ 1..=4) => v as u32,
        other => {
            return Err(StoreError::InvalidWalkMemory(format!(
                "unsupported memory schemaVersion: {}",
                other.map_or_else(|| "invalid".to_string(), |v| v.to_string())
            )));
        }
    };
    let Some(raw_walks) = root.get("walks").and_then(Value::as_array) else {
        return Err(StoreError::InvalidWalkMemory(
            "walks.json requires a walks array".to_string(),
        ));
    };

    let mut walks = Vec::with_capacity(raw_walks.len());
    let mut seen_identities = HashSet::with_capacity(raw_walks.len());
    for raw in raw_walks {
        let walk = validate_legacy_walk(raw, schema_version)?;
        let exact_id = serde_json::to_string(&(
            &walk.goal_key,
            &walk.app_id,
            &walk.platform,
            &walk.flow_schema_version,
            &walk.build_fingerprint,
            &walk.adapter,
            &walk.browser,
        ))
        .expect("identity tuple serializes");
        if !seen_identities.insert(exact_id) {
            return Err(StoreError::InvalidWalkMemory(
                "duplicate exact walk identities are ambiguous".to_string(),
            ));
        }
        walks.push(walk);
    }

    Ok((schema_version, walks))
}

fn validate_legacy_walk(value: &Value, schema_version: u32) -> Result<StoredWalk, StoreError> {
    let legacy_v1 = schema_version == 1;
    let Some(obj) = value.as_object() else {
        return Err(StoreError::InvalidWalkMemory(
            "walk entry must be an object".to_string(),
        ));
    };

    let goal = req_non_empty_str(obj, "goal")?;
    let goal_key = req_non_empty_str(obj, "goalKey")?;
    let norm = normalize_goal(&goal)
        .map_err(|_| StoreError::InvalidWalkMemory("walk entry has empty goal".to_string()))?;
    if norm.key != goal_key {
        return Err(StoreError::InvalidWalkMemory(
            "walk goalKey does not match normalized goal".to_string(),
        ));
    }

    let app_id = req_non_empty_str(obj, "appId")?;
    let platform = req_non_empty_str(obj, "platform")?;
    let build_fingerprint = req_sha256(obj, "buildFingerprint")?;
    let flow_path = req_non_empty_str(obj, "flowPath")?;
    let flow_sha256 = req_sha256(obj, "flowSha256")?;

    let flow_schema_version = match obj.get("flowSchemaVersion") {
        None if legacy_v1 => DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        Some(Value::String(s)) if s == DEFAULT_FLOW_SCHEMA_VERSION => s.clone(),
        other => {
            return Err(StoreError::InvalidWalkMemory(format!(
                "unsupported flow schema in walk entry: {other:?}"
            )));
        }
    };

    let adapter = opt_non_empty_str(obj, "adapter")?;
    let browser = opt_non_empty_str(obj, "browser")?;
    if platform == "web" && (adapter.is_none() || browser.is_none()) {
        return Err(StoreError::InvalidWalkMemory(
            "walk adapter/browser identity is missing for web platform".to_string(),
        ));
    }

    let last_validation = read_validation(obj.get("lastValidation"))?;
    let validation_history = if legacy_v1 && obj.get("validationHistory").is_none() {
        vec![StoredValidation {
            passed: last_validation.passed,
            at: last_validation.at.clone(),
            flow_sha256: flow_sha256.clone(),
        }]
    } else {
        let Some(arr) = obj.get("validationHistory").and_then(Value::as_array) else {
            return Err(StoreError::InvalidWalkMemory(
                "walk validationHistory must be a non-empty array".to_string(),
            ));
        };
        if arr.is_empty() {
            return Err(StoreError::InvalidWalkMemory(
                "walk validationHistory must not be empty".to_string(),
            ));
        }
        let mut hist = Vec::with_capacity(arr.len());
        for item in arr {
            let v = read_validation(Some(item))?;
            let item_obj = item.as_object().expect("validated object");
            let sha = req_sha256(item_obj, "flowSha256")?;
            hist.push(StoredValidation {
                passed: v.passed,
                at: v.at,
                flow_sha256: sha,
            });
        }
        hist
    };

    let latest = validation_history.last().expect("non-empty history");
    if latest.passed != last_validation.passed
        || latest.at != last_validation.at
        || latest.flow_sha256 != flow_sha256
    {
        return Err(StoreError::InvalidWalkMemory(
            "lastValidation must equal the latest history entry for the stored flow".to_string(),
        ));
    }

    let origin: Option<WalkOrigin> = match obj.get("origin") {
        None => None,
        Some(Value::String(s)) => Some(parse_origin_str(s)?),
        Some(_) => {
            return Err(StoreError::InvalidWalkMemory(
                "walk origin must be a string".to_string(),
            ));
        }
    };

    let exploration = match obj.get("exploration") {
        None => None,
        Some(val) => Some(read_exploration(val)?),
    };

    let repair_history = match obj.get("repairHistory") {
        None => None,
        Some(val) => {
            let Some(arr) = val.as_array() else {
                return Err(StoreError::InvalidWalkMemory(
                    "walk repairHistory must be a non-empty array when present".to_string(),
                ));
            };
            if arr.is_empty() {
                return Err(StoreError::InvalidWalkMemory(
                    "walk repairHistory must be a non-empty array when present".to_string(),
                ));
            }
            let mut repairs = Vec::with_capacity(arr.len());
            for item in arr {
                let Some(robj) = item.as_object() else {
                    return Err(StoreError::InvalidWalkMemory(
                        "walk repair provenance must be an object".to_string(),
                    ));
                };
                let rep = RepairProvenance {
                    attempt_id: req_non_empty_str(robj, "attemptId")?,
                    patch_path: req_non_empty_str(robj, "patchPath")?,
                    previous_build_fingerprint: req_sha256(robj, "previousBuildFingerprint")?,
                    previous_flow_sha256: req_sha256(robj, "previousFlowSha256")?,
                };
                validate_repair_provenance(&rep)?;
                repairs.push(rep);
            }
            Some(repairs)
        }
    };

    let closure_sha256 = match obj.get("closureSha256") {
        None => None,
        Some(Value::String(s)) => {
            if schema_version < 4
                || obj.contains_key("subflowFiles")
                || obj.contains_key("legacyClosure")
                || !is_sha256_hex(s)
            {
                return Err(StoreError::InvalidWalkMemory(
                    "closureSha256 requires schema 4, valid SHA-256, and excludes subflowFiles"
                        .to_string(),
                ));
            }
            check_manifest_flow_path(&flow_path, "flowPath").map_err(|err| {
                StoreError::InvalidWalkMemory(format!("walk closure is invalid: {err}"))
            })?;
            Some(s.clone())
        }
        Some(_) => {
            return Err(StoreError::InvalidWalkMemory(
                "closureSha256 must be a SHA-256 string".to_string(),
            ));
        }
    };

    let mut subflow_files: Option<Vec<StoredFlowFile>> = None;
    let mut legacy_closure: Option<bool> = None;

    if closure_sha256.is_none() {
        let has_subflows = obj.contains_key("subflowFiles");
        if (schema_version >= 3) != has_subflows {
            return Err(StoreError::InvalidWalkMemory(if schema_version >= 3 {
                "walk is missing subflowFiles, which memory schema 3 requires".to_string()
            } else {
                "subflowFiles needs memory schema 3".to_string()
            }));
        }
        if let Some(raw_sf) = obj.get("subflowFiles") {
            let parsed_sf = read_stored_flow_files(raw_sf).ok_or_else(|| {
                StoreError::InvalidWalkMemory("walk subflowFiles is invalid".to_string())
            })?;
            subflow_files = Some(parsed_sf);
        }
        if let Some(raw_lc) = obj.get("legacyClosure") {
            if raw_lc.as_bool() != Some(true) || schema_version < 3 {
                return Err(StoreError::InvalidWalkMemory(
                    "walk legacyClosure is invalid".to_string(),
                ));
            }
            legacy_closure = Some(true);
        } else if schema_version <= 2 {
            legacy_closure = Some(true);
        }
    }

    Ok(StoredWalk {
        goal,
        app_id,
        platform,
        flow_schema_version,
        build_fingerprint,
        adapter,
        browser,
        goal_key,
        flow_path,
        flow_sha256,
        subflow_files,
        legacy_closure,
        closure_sha256,
        last_validation,
        validation_history,
        origin,
        exploration,
        repair_history,
    })
}

fn req_non_empty_str(
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String, StoreError> {
    match obj.get(key) {
        Some(Value::String(s)) if is_non_empty(s) => Ok(s.clone()),
        _ => Err(StoreError::InvalidWalkMemory(format!(
            "walk entry has missing or malformed {key}"
        ))),
    }
}

fn opt_non_empty_str(
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, StoreError> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::String(s)) if is_non_empty(s) => Ok(Some(s.clone())),
        _ => Err(StoreError::InvalidWalkMemory(format!(
            "walk {key} must be a non-empty string when present"
        ))),
    }
}

fn req_sha256(obj: &serde_json::Map<String, Value>, key: &str) -> Result<String, StoreError> {
    match obj.get(key) {
        Some(Value::String(s)) if is_sha256_hex(s) => Ok(s.clone()),
        _ => Err(StoreError::InvalidWalkMemory(format!(
            "walk {key} must be a 64-char lowercase hex SHA-256"
        ))),
    }
}

fn read_validation(value: Option<&Value>) -> Result<WalkValidation, StoreError> {
    let Some(obj) = value.and_then(Value::as_object) else {
        return Err(StoreError::InvalidWalkMemory(
            "walk validation requires passed: boolean and an ISO timestamp".to_string(),
        ));
    };
    let Some(passed) = obj.get("passed").and_then(Value::as_bool) else {
        return Err(StoreError::InvalidWalkMemory(
            "walk validation requires passed: boolean and an ISO timestamp".to_string(),
        ));
    };
    let Some(at) = obj.get("at").and_then(Value::as_str) else {
        return Err(StoreError::InvalidWalkMemory(
            "walk validation requires passed: boolean and an ISO timestamp".to_string(),
        ));
    };
    if !is_valid_iso8601(at) {
        return Err(StoreError::InvalidWalkMemory(format!(
            "walk validation timestamp is invalid: {at}"
        )));
    }
    Ok(WalkValidation {
        passed,
        at: at.to_string(),
    })
}

fn read_optional_metric(
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<u64>, StoreError> {
    match obj.get(key) {
        Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            if let Some(u) = n.as_u64() {
                Ok(Some(u))
            } else if let Some(f) = n.as_f64()
                && f.is_finite()
                && f >= 0.0
                && f.fract() == 0.0
            {
                Ok(Some(f as u64))
            } else {
                Err(StoreError::InvalidWalkMemory(format!(
                    "walk exploration {key} must be a non-negative integer or null"
                )))
            }
        }
        _ => Err(StoreError::InvalidWalkMemory(format!(
            "walk exploration {key} must be a non-negative integer or null"
        ))),
    }
}

fn read_exploration(value: &Value) -> Result<ExplorationDetails, StoreError> {
    let Some(obj) = value.as_object() else {
        return Err(StoreError::InvalidWalkMemory(
            "walk exploration metadata is invalid".to_string(),
        ));
    };
    let model_call_basis = req_non_empty_str(obj, "modelCallBasis")?;
    Ok(ExplorationDetails {
        exploration_duration_ms: read_optional_metric(obj, "explorationDurationMs")?,
        verification_duration_ms: read_optional_metric(obj, "verificationDurationMs")?,
        mcp_calls: read_optional_metric(obj, "mcpCalls")?,
        model_calls: read_optional_metric(obj, "modelCalls")?,
        model_call_basis,
    })
}

/// Detect duplicate keys inside any JSON object at any nesting depth (assumes syntactically valid JSON).
fn find_duplicate_json_key(text: &str) -> Option<String> {
    struct Scanner<'a> {
        src: &'a str,
        bytes: &'a [u8],
        pos: usize,
    }

    impl<'a> Scanner<'a> {
        fn skip_ws(&mut self) {
            while self.pos < self.bytes.len()
                && matches!(self.bytes[self.pos], b' ' | b'\t' | b'\r' | b'\n')
            {
                self.pos += 1;
            }
        }

        fn scan_string(&mut self) -> Option<String> {
            let start = self.pos;
            if self.pos >= self.bytes.len() || self.bytes[self.pos] != b'"' {
                return None;
            }
            self.pos += 1;
            while self.pos < self.bytes.len() && self.bytes[self.pos] != b'"' {
                if self.bytes[self.pos] == b'\\' {
                    self.pos += 2;
                } else {
                    self.pos += 1;
                }
            }
            if self.pos < self.bytes.len() {
                self.pos += 1;
            }
            serde_json::from_str::<String>(&self.src[start..self.pos]).ok()
        }

        fn scan_value(&mut self) -> Option<String> {
            self.skip_ws();
            let ch = *self.bytes.get(self.pos)?;
            match ch {
                b'{' => {
                    self.pos += 1;
                    let mut seen = HashSet::new();
                    self.skip_ws();
                    while self.pos < self.bytes.len() && self.bytes[self.pos] != b'}' {
                        self.skip_ws();
                        let key = self.scan_string()?;
                        if !seen.insert(key.clone()) {
                            return Some(key);
                        }
                        self.skip_ws();
                        if self.pos < self.bytes.len() && self.bytes[self.pos] == b':' {
                            self.pos += 1;
                        }
                        if let Some(dup) = self.scan_value() {
                            return Some(dup);
                        }
                        self.skip_ws();
                        if self.pos < self.bytes.len() && self.bytes[self.pos] == b',' {
                            self.pos += 1;
                        }
                        self.skip_ws();
                    }
                    if self.pos < self.bytes.len() {
                        self.pos += 1;
                    }
                    None
                }
                b'[' => {
                    self.pos += 1;
                    self.skip_ws();
                    while self.pos < self.bytes.len() && self.bytes[self.pos] != b']' {
                        if let Some(dup) = self.scan_value() {
                            return Some(dup);
                        }
                        self.skip_ws();
                        if self.pos < self.bytes.len() && self.bytes[self.pos] == b',' {
                            self.pos += 1;
                        }
                        self.skip_ws();
                    }
                    if self.pos < self.bytes.len() {
                        self.pos += 1;
                    }
                    None
                }
                b'"' => {
                    let _ = self.scan_string();
                    None
                }
                _ => {
                    while self.pos < self.bytes.len()
                        && !matches!(
                            self.bytes[self.pos],
                            b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n'
                        )
                    {
                        self.pos += 1;
                    }
                    None
                }
            }
        }
    }

    let mut scanner = Scanner {
        src: text,
        bytes: text.as_bytes(),
        pos: 0,
    };
    scanner.scan_value()
}
