//! SQLite v5 schema, exact PRAGMA initialization, transactional legacy SQLite v2-v4 upgrade,
//! and domain-prefixed canonical JSON `integrity_sha256` builders & verifiers.

use std::collections::HashSet;

use deja_core::{
    DEFAULT_FLOW_SCHEMA_VERSION, ExplorationDetails, RepairProvenance, StoredFlowFile, WalkOrigin,
    check_manifest_flow_path, cmp_utf16, normalize_goal, read_stored_flow_files,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::StoreError;
use crate::lock::is_valid_iso8601;

/// Unified SQLite schema version (`PRAGMA user_version = 5`).
pub const SQLITE_SCHEMA_VERSION: u32 = 5;

/// Lowest supported legacy SQLite schema version accepted for automatic upgrade.
pub const MIN_SQLITE_SCHEMA_VERSION: u32 = 2;

/// Domain prefixes for per-row canonical JSON SHA-256 integrity digests.
const DOMAIN_WALK: &str = "deja-sqlite-v5:walk\n";
const DOMAIN_VALIDATION_RUN: &str = "deja-sqlite-v5:validation_run\n";
const DOMAIN_VALIDATION_HISTORY: &str = "deja-sqlite-v5:validation_history\n";
const DOMAIN_WALK_FILE: &str = "deja-sqlite-v5:walk_file\n";
const DOMAIN_REPAIR_HISTORY: &str = "deja-sqlite-v5:repair_history\n";
const DOMAIN_GOAL_EMBEDDING: &str = "deja-sqlite-v5:goal_embedding\n";
const DOMAIN_BUILD_FINGERPRINT_CACHE: &str = "deja-sqlite-v5:build_fingerprint_cache\n";

/// Complete v5 DDL executed inside a `BEGIN IMMEDIATE` transaction on brand-new or upgraded stores.
const SCHEMA_V5_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS walks (
    id INTEGER PRIMARY KEY,
    goal TEXT NOT NULL,
    goal_key TEXT NOT NULL,
    app_id TEXT NOT NULL,
    platform TEXT NOT NULL,
    flow_schema_version TEXT NOT NULL,
    build_fingerprint TEXT NOT NULL,
    adapter TEXT,
    browser TEXT,
    adapter_key TEXT GENERATED ALWAYS AS (COALESCE(adapter, '')) STORED,
    browser_key TEXT GENERATED ALWAYS AS (COALESCE(browser, '')) STORED,
    flow_path TEXT NOT NULL,
    flow_sha256 TEXT NOT NULL,
    last_passed INTEGER NOT NULL CHECK (last_passed IN (0, 1)),
    last_at TEXT NOT NULL,
    origin TEXT,
    exploration_json TEXT,
    has_files INTEGER NOT NULL DEFAULT 1 CHECK (has_files IN (0, 1)),
    legacy_closure INTEGER NOT NULL DEFAULT 0 CHECK (legacy_closure IN (0, 1)),
    closure_sha256 TEXT,
    integrity_sha256 TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_walks_identity
    ON walks(goal_key, app_id, platform, flow_schema_version, adapter_key, browser_key);

CREATE INDEX IF NOT EXISTS idx_walks_goal_key
    ON walks(goal_key);

CREATE TABLE IF NOT EXISTS validation_runs (
    id INTEGER PRIMARY KEY,
    walk_id INTEGER NOT NULL REFERENCES walks(id) ON DELETE CASCADE,
    build_fingerprint TEXT NOT NULL,
    runtime TEXT,
    runtime_key TEXT GENERATED ALWAYS AS (COALESCE(runtime, '')) STORED,
    flow_path TEXT NOT NULL,
    flow_sha256 TEXT NOT NULL,
    closure_sha256 TEXT,
    subflow_files_json TEXT,
    passed INTEGER NOT NULL CHECK (passed IN (0, 1)),
    flaky INTEGER NOT NULL DEFAULT 0 CHECK (flaky IN (0, 1)),
    validated_at TEXT NOT NULL,
    origin TEXT,
    failure_category TEXT,
    report_path TEXT,
    duration_ms INTEGER,
    phase_timings_json TEXT,
    integrity_sha256 TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_vruns_identity
    ON validation_runs(walk_id, build_fingerprint, runtime_key);

CREATE INDEX IF NOT EXISTS idx_vruns_walk_build
    ON validation_runs(walk_id, build_fingerprint, passed, validated_at);

CREATE TABLE IF NOT EXISTS validation_history (
    walk_id INTEGER NOT NULL REFERENCES walks(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    passed INTEGER NOT NULL CHECK (passed IN (0, 1)),
    at TEXT NOT NULL,
    flow_sha256 TEXT NOT NULL,
    build_fingerprint TEXT,
    runtime TEXT,
    flaky INTEGER NOT NULL DEFAULT 0 CHECK (flaky IN (0, 1)),
    integrity_sha256 TEXT NOT NULL,
    PRIMARY KEY (walk_id, position)
);

CREATE TABLE IF NOT EXISTS walk_files (
    walk_id INTEGER NOT NULL REFERENCES walks(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    path TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    integrity_sha256 TEXT NOT NULL,
    PRIMARY KEY (walk_id, position)
);

CREATE TABLE IF NOT EXISTS repair_history (
    walk_id INTEGER NOT NULL REFERENCES walks(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    attempt_id TEXT NOT NULL,
    patch_path TEXT NOT NULL,
    previous_build_fingerprint TEXT NOT NULL,
    previous_flow_sha256 TEXT NOT NULL,
    integrity_sha256 TEXT NOT NULL,
    PRIMARY KEY (walk_id, position)
);

CREATE TABLE IF NOT EXISTS goal_embeddings (
    walk_id INTEGER NOT NULL REFERENCES walks(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    dimensions INTEGER NOT NULL CHECK (dimensions > 0),
    embedding_blob BLOB NOT NULL,
    updated_at TEXT NOT NULL,
    integrity_sha256 TEXT NOT NULL,
    PRIMARY KEY (walk_id, provider)
);

CREATE TABLE IF NOT EXISTS build_fingerprint_cache (
    canonical_path TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('file', 'tree')),
    size INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    ino INTEGER NOT NULL,
    dev INTEGER NOT NULL,
    ctime_sec INTEGER NOT NULL,
    ctime_nsec INTEGER NOT NULL,
    entries_json TEXT,
    digest_sha256 TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    integrity_sha256 TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_bfc_stat
    ON build_fingerprint_cache(canonical_path, size, mtime_ns, ino);
"#;

/// Apply the exact writable PRAGMA sequence required by SQLite v5:
/// `busy_timeout = 5000`, `journal_mode = WAL`, `synchronous = FULL`,
/// `foreign_keys = ON`, `temp_store = MEMORY`, `cache_size = -16384`.
pub fn apply_writable_pragmas(conn: &Connection) -> Result<(), StoreError> {
    conn.pragma_update(None, "busy_timeout", 5000)?;
    let _mode: String =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", -16384)?;
    Ok(())
}

/// Apply the exact read-only PRAGMA sequence required by SQLite v5:
/// `busy_timeout = 5000`, `foreign_keys = ON`, `temp_store = MEMORY`,
/// `cache_size = -16384`, `query_only = ON`.
pub fn apply_readonly_pragmas(conn: &Connection) -> Result<(), StoreError> {
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", -16384)?;
    conn.pragma_update(None, "query_only", "ON")?;
    Ok(())
}

/// Query `PRAGMA user_version` safely.
pub fn read_user_version(conn: &Connection) -> Result<u32, StoreError> {
    let raw: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|err| {
            StoreError::InvalidWalkMemory(format!("walks.sqlite cannot be read: {err}"))
        })?;
    u32::try_from(raw).map_err(|_| {
        StoreError::InvalidWalkMemory(format!("unsupported memory schemaVersion: {raw}"))
    })
}

/// Verify that a read-only database is a valid SQLite v5 store.
pub fn verify_readonly_schema(conn: &Connection) -> Result<(), StoreError> {
    let version = read_user_version(conn)?;
    if !(MIN_SQLITE_SCHEMA_VERSION..=SQLITE_SCHEMA_VERSION).contains(&version) {
        return Err(StoreError::InvalidWalkMemory(format!(
            "unsupported memory schemaVersion: {version}"
        )));
    }
    if version != SQLITE_SCHEMA_VERSION {
        return Err(StoreError::InvalidWalkMemory(format!(
            "SQLite memory schema version {version} requires writable upgrade to v{SQLITE_SCHEMA_VERSION}"
        )));
    }
    verify_v5_tables_present(conn)
}

/// Initialize a brand-new database (`user_version = 0`) to v5, reject `< 2` or `> 5`,
/// or upgrade a valid SQLite v2/v3/v4 database to v5 inside a single `BEGIN IMMEDIATE` transaction.
pub fn ensure_writable_schema(conn: &mut Connection) -> Result<(), StoreError> {
    let version = read_user_version(conn)?;
    match version {
        0 => {
            let table_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            if table_count > 0 {
                return Err(StoreError::InvalidWalkMemory(
                    "walks.sqlite has tables with user_version = 0".to_string(),
                ));
            }
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute_batch(SCHEMA_V5_DDL)?;
            tx.pragma_update(None, "user_version", SQLITE_SCHEMA_VERSION)?;
            tx.commit()?;
            Ok(())
        }
        v if v == SQLITE_SCHEMA_VERSION => verify_v5_tables_present(conn),
        v if (MIN_SQLITE_SCHEMA_VERSION..SQLITE_SCHEMA_VERSION).contains(&v) => {
            upgrade_legacy_sqlite_to_v5(conn, v)
        }
        other => Err(StoreError::InvalidWalkMemory(format!(
            "unsupported memory schemaVersion: {other}"
        ))),
    }
}

fn verify_v5_tables_present(conn: &Connection) -> Result<(), StoreError> {
    const REQUIRED: &[&str] = &[
        "walks",
        "validation_runs",
        "validation_history",
        "walk_files",
        "repair_history",
        "goal_embeddings",
        "build_fingerprint_cache",
    ];
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .map_err(|err| {
            StoreError::InvalidWalkMemory(format!("walks.sqlite metadata is unreadable: {err}"))
        })?;
    let present: HashSet<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|err| {
            StoreError::InvalidWalkMemory(format!("walks.sqlite metadata is unreadable: {err}"))
        })?
        .collect::<Result<HashSet<_>, _>>()
        .map_err(|err| {
            StoreError::InvalidWalkMemory(format!("walks.sqlite metadata is unreadable: {err}"))
        })?;
    for table in REQUIRED {
        if !present.contains(*table) {
            return Err(StoreError::InvalidWalkMemory(format!(
                "walks.sqlite v5 is missing required table `{table}`"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Canonical JSON & Domain-Prefixed Row Integrity Hashing

/// Deterministic canonical JSON over `serde_json::Value` with UTF-16 key ordering.
#[must_use]
pub fn canonical_json_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(true) => "true".to_string(),
        Value::Bool(false) => "false".to_string(),
        Value::Number(num) => num.to_string(),
        Value::String(text) => serde_json::to_string(text).expect("UTF-8 string serializes"),
        Value::Array(items) => {
            let mut out = String::from("[");
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                out.push_str(&canonical_json_value(item));
            }
            out.push(']');
            out
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|left, right| cmp_utf16(left.0, right.0));
            let mut out = String::from("{");
            for (idx, (key, val)) in entries.into_iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).expect("UTF-8 key serializes"));
                out.push(':');
                out.push_str(&canonical_json_value(val));
            }
            out.push('}');
            out
        }
    }
}

fn domain_sha256(domain: &str, payload: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update(canonical_json_value(payload).as_bytes());
    hex::encode(hasher.finalize())
}

/// Borrowed view of every column covered by `walks.integrity_sha256`.
#[derive(Debug, Clone, Copy)]
pub struct WalkIntegrityFields<'a> {
    pub goal: &'a str,
    pub goal_key: &'a str,
    pub app_id: &'a str,
    pub platform: &'a str,
    pub flow_schema_version: &'a str,
    pub build_fingerprint: &'a str,
    pub adapter: Option<&'a str>,
    pub browser: Option<&'a str>,
    pub flow_path: &'a str,
    pub flow_sha256: &'a str,
    pub last_passed: bool,
    pub last_at: &'a str,
    pub origin: Option<&'a str>,
    pub exploration_json: Option<&'a str>,
    pub has_files: bool,
    pub legacy_closure: bool,
    pub closure_sha256: Option<&'a str>,
}

/// Compute `walks.integrity_sha256`.
#[must_use]
pub fn compute_walk_integrity(fields: &WalkIntegrityFields<'_>) -> String {
    domain_sha256(
        DOMAIN_WALK,
        &json!({
            "adapter": fields.adapter,
            "app_id": fields.app_id,
            "browser": fields.browser,
            "build_fingerprint": fields.build_fingerprint,
            "closure_sha256": fields.closure_sha256,
            "exploration_json": fields.exploration_json,
            "flow_path": fields.flow_path,
            "flow_schema_version": fields.flow_schema_version,
            "flow_sha256": fields.flow_sha256,
            "goal": fields.goal,
            "goal_key": fields.goal_key,
            "has_files": fields.has_files,
            "last_at": fields.last_at,
            "last_passed": fields.last_passed,
            "legacy_closure": fields.legacy_closure,
            "origin": fields.origin,
            "platform": fields.platform,
        }),
    )
}

/// Verify `walks.integrity_sha256`, returning `InvalidWalkMemory` on any mismatch.
pub fn verify_walk_integrity(
    fields: &WalkIntegrityFields<'_>,
    stored: &str,
) -> Result<(), StoreError> {
    let expected = compute_walk_integrity(fields);
    if expected != stored {
        return Err(StoreError::InvalidWalkMemory(format!(
            "walk row integrity checksum mismatch for goal {:?}",
            fields.goal_key
        )));
    }
    Ok(())
}

/// Borrowed view of every column covered by `validation_runs.integrity_sha256`.
#[derive(Debug, Clone, Copy)]
pub struct ValidationRunIntegrityFields<'a> {
    pub walk_id: i64,
    pub build_fingerprint: &'a str,
    pub runtime: Option<&'a str>,
    pub flow_path: &'a str,
    pub flow_sha256: &'a str,
    pub closure_sha256: Option<&'a str>,
    pub subflow_files_json: Option<&'a str>,
    pub passed: bool,
    pub flaky: bool,
    pub validated_at: &'a str,
    pub origin: Option<&'a str>,
    pub failure_category: Option<&'a str>,
    pub report_path: Option<&'a str>,
    pub duration_ms: Option<i64>,
    pub phase_timings_json: Option<&'a str>,
}

/// Compute `validation_runs.integrity_sha256`.
#[must_use]
pub fn compute_validation_run_integrity(fields: &ValidationRunIntegrityFields<'_>) -> String {
    domain_sha256(
        DOMAIN_VALIDATION_RUN,
        &json!({
            "build_fingerprint": fields.build_fingerprint,
            "closure_sha256": fields.closure_sha256,
            "duration_ms": fields.duration_ms,
            "failure_category": fields.failure_category,
            "flaky": fields.flaky,
            "flow_path": fields.flow_path,
            "flow_sha256": fields.flow_sha256,
            "origin": fields.origin,
            "passed": fields.passed,
            "phase_timings_json": fields.phase_timings_json,
            "report_path": fields.report_path,
            "runtime": fields.runtime,
            "subflow_files_json": fields.subflow_files_json,
            "validated_at": fields.validated_at,
            "walk_id": fields.walk_id,
        }),
    )
}

/// Verify `validation_runs.integrity_sha256`, returning `InvalidWalkMemory` on any mismatch.
pub fn verify_validation_run_integrity(
    fields: &ValidationRunIntegrityFields<'_>,
    stored: &str,
) -> Result<(), StoreError> {
    let expected = compute_validation_run_integrity(fields);
    if expected != stored {
        return Err(StoreError::InvalidWalkMemory(format!(
            "validation_runs integrity checksum mismatch for walk_id={} build={}",
            fields.walk_id, fields.build_fingerprint
        )));
    }
    Ok(())
}

/// Borrowed view of every column covered by `validation_history.integrity_sha256`.
#[derive(Debug, Clone, Copy)]
pub struct ValidationHistoryIntegrityFields<'a> {
    pub walk_id: i64,
    pub position: i64,
    pub passed: bool,
    pub at: &'a str,
    pub flow_sha256: &'a str,
    pub build_fingerprint: Option<&'a str>,
    pub runtime: Option<&'a str>,
    pub flaky: bool,
}

/// Compute `validation_history.integrity_sha256`.
#[must_use]
pub fn compute_validation_history_integrity(
    fields: &ValidationHistoryIntegrityFields<'_>,
) -> String {
    domain_sha256(
        DOMAIN_VALIDATION_HISTORY,
        &json!({
            "at": fields.at,
            "build_fingerprint": fields.build_fingerprint,
            "flaky": fields.flaky,
            "flow_sha256": fields.flow_sha256,
            "passed": fields.passed,
            "position": fields.position,
            "runtime": fields.runtime,
            "walk_id": fields.walk_id,
        }),
    )
}

/// Verify `validation_history.integrity_sha256`.
pub fn verify_validation_history_integrity(
    fields: &ValidationHistoryIntegrityFields<'_>,
    stored: &str,
) -> Result<(), StoreError> {
    let expected = compute_validation_history_integrity(fields);
    if expected != stored {
        return Err(StoreError::InvalidWalkMemory(format!(
            "validation_history integrity checksum mismatch for walk_id={} position={}",
            fields.walk_id, fields.position
        )));
    }
    Ok(())
}

/// Compute `walk_files.integrity_sha256`.
#[must_use]
pub fn compute_walk_file_integrity(
    walk_id: i64,
    position: i64,
    path: &str,
    sha256: &str,
) -> String {
    domain_sha256(
        DOMAIN_WALK_FILE,
        &json!({
            "path": path,
            "position": position,
            "sha256": sha256,
            "walk_id": walk_id,
        }),
    )
}

/// Verify `walk_files.integrity_sha256`.
pub fn verify_walk_file_integrity(
    walk_id: i64,
    position: i64,
    path: &str,
    sha256: &str,
    stored: &str,
) -> Result<(), StoreError> {
    let expected = compute_walk_file_integrity(walk_id, position, path, sha256);
    if expected != stored {
        return Err(StoreError::InvalidWalkMemory(format!(
            "walk_files integrity checksum mismatch for walk_id={} position={}",
            walk_id, position
        )));
    }
    Ok(())
}

/// Compute `repair_history.integrity_sha256`.
#[must_use]
pub fn compute_repair_history_integrity(
    walk_id: i64,
    position: i64,
    attempt_id: &str,
    patch_path: &str,
    previous_build_fingerprint: &str,
    previous_flow_sha256: &str,
) -> String {
    domain_sha256(
        DOMAIN_REPAIR_HISTORY,
        &json!({
            "attempt_id": attempt_id,
            "patch_path": patch_path,
            "position": position,
            "previous_build_fingerprint": previous_build_fingerprint,
            "previous_flow_sha256": previous_flow_sha256,
            "walk_id": walk_id,
        }),
    )
}

/// Verify `repair_history.integrity_sha256`.
pub fn verify_repair_history_integrity(
    walk_id: i64,
    position: i64,
    attempt_id: &str,
    patch_path: &str,
    previous_build_fingerprint: &str,
    previous_flow_sha256: &str,
    stored: &str,
) -> Result<(), StoreError> {
    let expected = compute_repair_history_integrity(
        walk_id,
        position,
        attempt_id,
        patch_path,
        previous_build_fingerprint,
        previous_flow_sha256,
    );
    if expected != stored {
        return Err(StoreError::InvalidWalkMemory(format!(
            "repair_history integrity checksum mismatch for walk_id={} position={}",
            walk_id, position
        )));
    }
    Ok(())
}

/// Compute `goal_embeddings.integrity_sha256`.
#[must_use]
pub fn compute_goal_embedding_integrity(
    walk_id: i64,
    provider: &str,
    dimensions: i64,
    embedding_blob: &[u8],
    updated_at: &str,
) -> String {
    let blob_hex = hex::encode(embedding_blob);
    domain_sha256(
        DOMAIN_GOAL_EMBEDDING,
        &json!({
            "dimensions": dimensions,
            "embedding_blob_hex": blob_hex,
            "provider": provider,
            "updated_at": updated_at,
            "walk_id": walk_id,
        }),
    )
}

/// Verify `goal_embeddings.integrity_sha256`.
pub fn verify_goal_embedding_integrity(
    walk_id: i64,
    provider: &str,
    dimensions: i64,
    embedding_blob: &[u8],
    updated_at: &str,
    stored: &str,
) -> Result<(), StoreError> {
    let expected =
        compute_goal_embedding_integrity(walk_id, provider, dimensions, embedding_blob, updated_at);
    if expected != stored {
        return Err(StoreError::InvalidWalkMemory(format!(
            "goal_embeddings integrity checksum mismatch for walk_id={} provider={provider}",
            walk_id
        )));
    }
    Ok(())
}

/// Borrowed view of every column covered by `build_fingerprint_cache.integrity_sha256`.
#[derive(Debug, Clone, Copy)]
pub struct BuildFingerprintCacheIntegrityFields<'a> {
    pub canonical_path: &'a str,
    pub kind: &'a str,
    pub size: i64,
    pub mtime_ns: i64,
    pub ino: i64,
    pub dev: i64,
    pub ctime_sec: i64,
    pub ctime_nsec: i64,
    pub entries_json: Option<&'a str>,
    pub digest_sha256: &'a str,
    pub updated_at: &'a str,
}

/// Compute `build_fingerprint_cache.integrity_sha256`.
#[must_use]
pub fn compute_build_fingerprint_cache_integrity(
    fields: &BuildFingerprintCacheIntegrityFields<'_>,
) -> String {
    domain_sha256(
        DOMAIN_BUILD_FINGERPRINT_CACHE,
        &json!({
            "canonical_path": fields.canonical_path,
            "ctime_nsec": fields.ctime_nsec,
            "ctime_sec": fields.ctime_sec,
            "dev": fields.dev,
            "digest_sha256": fields.digest_sha256,
            "entries_json": fields.entries_json,
            "ino": fields.ino,
            "kind": fields.kind,
            "mtime_ns": fields.mtime_ns,
            "size": fields.size,
            "updated_at": fields.updated_at,
        }),
    )
}

/// Verify `build_fingerprint_cache.integrity_sha256`.
pub fn verify_build_fingerprint_cache_integrity(
    fields: &BuildFingerprintCacheIntegrityFields<'_>,
    stored: &str,
) -> Result<(), StoreError> {
    let expected = compute_build_fingerprint_cache_integrity(fields);
    if expected != stored {
        return Err(StoreError::InvalidWalkMemory(format!(
            "build_fingerprint_cache integrity checksum mismatch for {}",
            fields.canonical_path
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Field Validators Shared Across Schema, Store, and Migration

#[must_use]
pub(crate) fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[must_use]
pub(crate) fn is_non_empty(value: &str) -> bool {
    !value.trim().is_empty()
}

pub(crate) fn origin_to_str(origin: WalkOrigin) -> &'static str {
    match origin {
        WalkOrigin::Remember => "remember",
        WalkOrigin::Explore => "explore",
        WalkOrigin::Repair => "repair",
        WalkOrigin::ReplayedOnNewBuild => "replayed-on-new-build",
        WalkOrigin::Verify => "verify",
    }
}

pub(crate) fn parse_origin_str(raw: &str) -> Result<WalkOrigin, StoreError> {
    match raw {
        "remember" => Ok(WalkOrigin::Remember),
        "explore" => Ok(WalkOrigin::Explore),
        "repair" => Ok(WalkOrigin::Repair),
        "replayed-on-new-build" => Ok(WalkOrigin::ReplayedOnNewBuild),
        "verify" => Ok(WalkOrigin::Verify),
        other => Err(StoreError::InvalidWalkMemory(format!(
            "invalid walk origin: {other}"
        ))),
    }
}

pub(crate) fn validate_repair_provenance(repair: &RepairProvenance) -> Result<(), StoreError> {
    if !is_non_empty(&repair.attempt_id)
        || !is_non_empty(&repair.patch_path)
        || !repair.patch_path.starts_with(".deja/patches/")
        || repair.patch_path.starts_with('/')
        || repair.patch_path.contains('\\')
        || repair.patch_path.split('/').any(|seg| seg == "..")
        || !is_sha256_hex(&repair.previous_build_fingerprint)
        || !is_sha256_hex(&repair.previous_flow_sha256)
    {
        return Err(StoreError::InvalidWalkMemory(
            "walk repair provenance is invalid".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_exploration(raw: &str) -> Result<ExplorationDetails, StoreError> {
    if !is_non_empty(raw) {
        return Err(StoreError::InvalidWalkMemory(
            "walk exploration metadata is invalid".to_string(),
        ));
    }
    let details: ExplorationDetails = serde_json::from_str(raw).map_err(|err| {
        StoreError::InvalidWalkMemory(format!(
            "walk exploration metadata is malformed JSON: {err}"
        ))
    })?;
    if !is_non_empty(&details.model_call_basis) {
        return Err(StoreError::InvalidWalkMemory(
            "walk exploration metadata requires non-empty modelCallBasis".to_string(),
        ));
    }
    Ok(details)
}

// ---------------------------------------------------------------------------------------------
// Transactional Legacy SQLite v2/v3/v4 -> v5 Upgrade

struct LegacyWalkData {
    goal: String,
    goal_key: String,
    app_id: String,
    platform: String,
    flow_schema_version: String,
    build_fingerprint: String,
    adapter: Option<String>,
    browser: Option<String>,
    flow_path: String,
    flow_sha256: String,
    last_passed: bool,
    last_at: String,
    origin: Option<String>,
    exploration_json: Option<String>,
    has_files: bool,
    legacy_closure: bool,
    closure_sha256: Option<String>,
    history: Vec<(bool, String, String)>,
    files: Vec<StoredFlowFile>,
    repairs: Vec<RepairProvenance>,
}

fn upgrade_legacy_sqlite_to_v5(conn: &mut Connection, old_version: u32) -> Result<(), StoreError> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    let cols: HashSet<String> = {
        let mut stmt = tx.prepare("PRAGMA table_info(walks)")?;
        stmt.query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<HashSet<_>, _>>()?
    };
    if cols.is_empty() {
        return Err(StoreError::InvalidWalkMemory(format!(
            "legacy SQLite v{old_version} database is missing walks table"
        )));
    }

    let has_files_col = if cols.contains("has_files") {
        "has_files"
    } else {
        "0"
    };
    let legacy_closure_col = if cols.contains("legacy_closure") {
        "legacy_closure"
    } else {
        "1"
    };
    let closure_sha_col = if cols.contains("closure_sha256") {
        "closure_sha256"
    } else {
        "NULL"
    };
    let has_walk_files_table: bool = tx
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'walk_files'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();

    let select_sql = format!(
        "SELECT id, goal, goal_key, app_id, platform, flow_schema_version, build_fingerprint, \
                adapter, browser, flow_path, flow_sha256, last_passed, last_at, origin, \
                exploration_json, {has_files_col}, {legacy_closure_col}, {closure_sha_col} \
         FROM walks ORDER BY id"
    );

    let mut legacy_rows = Vec::new();
    {
        let mut stmt = tx.prepare(&select_sql)?;
        let mapped = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, Option<String>>(13)?,
                row.get::<_, Option<String>>(14)?,
                row.get::<_, i64>(15)?,
                row.get::<_, i64>(16)?,
                row.get::<_, Option<String>>(17)?,
            ))
        })?;
        for item in mapped {
            let (
                old_id,
                goal,
                goal_key,
                app_id,
                platform,
                flow_schema_version,
                build_fingerprint,
                adapter,
                browser,
                flow_path,
                flow_sha256,
                last_passed_i,
                last_at,
                origin,
                exploration_json,
                has_files_i,
                legacy_closure_i,
                closure_sha256,
            ) = item?;

            let norm = normalize_goal(&goal).map_err(|_| {
                StoreError::InvalidWalkMemory("legacy walk has empty goal".to_string())
            })?;
            if norm.key != goal_key
                || !is_non_empty(&app_id)
                || !is_non_empty(&platform)
                || flow_schema_version != DEFAULT_FLOW_SCHEMA_VERSION
                || !is_sha256_hex(&build_fingerprint)
                || !is_non_empty(&flow_path)
                || !is_sha256_hex(&flow_sha256)
                || !(last_passed_i == 0 || last_passed_i == 1)
                || !is_valid_iso8601(&last_at)
            {
                return Err(StoreError::InvalidWalkMemory(
                    "legacy SQLite walk row has malformed fields".to_string(),
                ));
            }
            if let Some(ref orig) = origin {
                let _ = parse_origin_str(orig)?;
            }
            if let Some(ref exp) = exploration_json {
                let _ = validate_exploration(exp)?;
            }
            if platform == "web"
                && (adapter.as_deref().is_none_or(|a| !is_non_empty(a))
                    || browser.as_deref().is_none_or(|b| !is_non_empty(b)))
            {
                return Err(StoreError::InvalidWalkMemory(
                    "legacy web walk is missing adapter or browser".to_string(),
                ));
            }

            let mut h_stmt = tx.prepare(
                "SELECT passed, at, flow_sha256 FROM validation_history WHERE walk_id = ?1 ORDER BY position",
            )?;
            let history: Vec<(bool, String, String)> = h_stmt
                .query_map([old_id], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?
                .map(|res| {
                    let (p, at, sha) = res?;
                    if !(p == 0 || p == 1) || !is_valid_iso8601(&at) || !is_sha256_hex(&sha) {
                        return Err(StoreError::InvalidWalkMemory(
                            "legacy validation_history row is invalid".to_string(),
                        ));
                    }
                    Ok((p == 1, at, sha))
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            if history.is_empty() {
                return Err(StoreError::InvalidWalkMemory(
                    "legacy walk validation history must not be empty".to_string(),
                ));
            }
            let latest = history.last().expect("non-empty history");
            if latest.0 != (last_passed_i == 1) || latest.1 != last_at || latest.2 != flow_sha256 {
                return Err(StoreError::InvalidWalkMemory(
                    "legacy lastValidation must equal the latest validation_history entry for the stored flow"
                        .to_string(),
                ));
            }

            let mut files = Vec::new();
            let (has_files, legacy_closure) = if let Some(ref csha) = closure_sha256 {
                if old_version < 4
                    || has_files_i != 0
                    || legacy_closure_i != 0
                    || !is_sha256_hex(csha)
                {
                    return Err(StoreError::InvalidWalkMemory(
                        "legacy closureSha256 walk is invalid".to_string(),
                    ));
                }
                check_manifest_flow_path(&flow_path, "flowPath").map_err(|e| {
                    StoreError::InvalidWalkMemory(format!("legacy closure flowPath invalid: {e}"))
                })?;
                (false, false)
            } else if old_version == 2 {
                (true, true)
            } else {
                if has_files_i != 1 {
                    return Err(StoreError::InvalidWalkMemory(
                        "legacy schema 3 walk is missing subflowFiles".to_string(),
                    ));
                }
                if has_walk_files_table {
                    let mut f_stmt = tx.prepare(
                        "SELECT path, sha256 FROM walk_files WHERE walk_id = ?1 ORDER BY position",
                    )?;
                    let raw_files: Vec<Value> = f_stmt
                        .query_map([old_id], |r| {
                            Ok(json!({
                                "path": r.get::<_, String>(0)?,
                                "sha256": r.get::<_, String>(1)?,
                            }))
                        })?
                        .collect::<Result<Vec<_>, _>>()?;
                    files = read_stored_flow_files(&Value::Array(raw_files)).ok_or_else(|| {
                        StoreError::InvalidWalkMemory("legacy walk_files entry is invalid".into())
                    })?;
                }
                (true, legacy_closure_i == 1)
            };

            let mut r_stmt = tx.prepare(
                "SELECT attempt_id, patch_path, previous_build_fingerprint, previous_flow_sha256 \
                 FROM repair_history WHERE walk_id = ?1 ORDER BY position",
            )?;
            let repairs: Vec<RepairProvenance> = r_stmt
                .query_map([old_id], |r| {
                    Ok(RepairProvenance {
                        attempt_id: r.get(0)?,
                        patch_path: r.get(1)?,
                        previous_build_fingerprint: r.get(2)?,
                        previous_flow_sha256: r.get(3)?,
                    })
                })?
                .map(|res| {
                    let rep = res?;
                    validate_repair_provenance(&rep)?;
                    Ok(rep)
                })
                .collect::<Result<Vec<_>, StoreError>>()?;

            legacy_rows.push(LegacyWalkData {
                goal,
                goal_key,
                app_id,
                platform,
                flow_schema_version,
                build_fingerprint,
                adapter,
                browser,
                flow_path,
                flow_sha256,
                last_passed: last_passed_i == 1,
                last_at,
                origin,
                exploration_json,
                has_files,
                legacy_closure,
                closure_sha256,
                history,
                files,
                repairs,
            });
        }
    }

    // Drop legacy tables and recreate clean v5 tables.
    tx.execute_batch(
        "DROP TABLE IF EXISTS repair_history;
         DROP TABLE IF EXISTS walk_files;
         DROP TABLE IF EXISTS validation_history;
         DROP TABLE IF EXISTS validation_runs;
         DROP TABLE IF EXISTS goal_embeddings;
         DROP TABLE IF EXISTS build_fingerprint_cache;
         DROP TABLE IF EXISTS walks;",
    )?;
    tx.execute_batch(SCHEMA_V5_DDL)?;

    for item in legacy_rows {
        let walk_integrity = compute_walk_integrity(&WalkIntegrityFields {
            goal: &item.goal,
            goal_key: &item.goal_key,
            app_id: &item.app_id,
            platform: &item.platform,
            flow_schema_version: &item.flow_schema_version,
            build_fingerprint: &item.build_fingerprint,
            adapter: item.adapter.as_deref(),
            browser: item.browser.as_deref(),
            flow_path: &item.flow_path,
            flow_sha256: &item.flow_sha256,
            last_passed: item.last_passed,
            last_at: &item.last_at,
            origin: item.origin.as_deref(),
            exploration_json: item.exploration_json.as_deref(),
            has_files: item.has_files,
            legacy_closure: item.legacy_closure,
            closure_sha256: item.closure_sha256.as_deref(),
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
                item.goal,
                item.goal_key,
                item.app_id,
                item.platform,
                item.flow_schema_version,
                item.build_fingerprint,
                item.adapter,
                item.browser,
                item.flow_path,
                item.flow_sha256,
                i64::from(item.last_passed),
                item.last_at,
                item.origin,
                item.exploration_json,
                i64::from(item.has_files),
                i64::from(item.legacy_closure),
                item.closure_sha256,
                walk_integrity,
            ],
            |r| r.get(0),
        )?;

        let subflow_json = if item.has_files && !item.legacy_closure {
            Some(serde_json::to_string(&item.files).expect("subflow_files serialize"))
        } else {
            None
        };
        let run_integrity = compute_validation_run_integrity(&ValidationRunIntegrityFields {
            walk_id,
            build_fingerprint: &item.build_fingerprint,
            runtime: None,
            flow_path: &item.flow_path,
            flow_sha256: &item.flow_sha256,
            closure_sha256: item.closure_sha256.as_deref(),
            subflow_files_json: subflow_json.as_deref(),
            passed: item.last_passed,
            flaky: false,
            validated_at: &item.last_at,
            origin: item.origin.as_deref(),
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
                failure_category = excluded.failure_category,
                report_path = excluded.report_path,
                duration_ms = excluded.duration_ms,
                phase_timings_json = excluded.phase_timings_json,
                integrity_sha256 = excluded.integrity_sha256",
            params![
                walk_id,
                item.build_fingerprint,
                item.flow_path,
                item.flow_sha256,
                item.closure_sha256,
                subflow_json,
                i64::from(item.last_passed),
                item.last_at,
                item.origin,
                run_integrity,
            ],
        )?;

        let next_hist_pos: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM validation_history WHERE walk_id = ?1",
            [walk_id],
            |r| r.get(0),
        )?;
        for (offset, (passed, at, sha)) in item.history.iter().enumerate() {
            let pos = next_hist_pos + offset as i64;
            let hist_integrity =
                compute_validation_history_integrity(&ValidationHistoryIntegrityFields {
                    walk_id,
                    position: pos,
                    passed: *passed,
                    at,
                    flow_sha256: sha,
                    build_fingerprint: Some(&item.build_fingerprint),
                    runtime: None,
                    flaky: false,
                });
            tx.execute(
                "INSERT INTO validation_history (
                    walk_id, position, passed, at, flow_sha256, build_fingerprint, runtime, flaky, integrity_sha256
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 0, ?7)",
                params![
                    walk_id,
                    pos,
                    i64::from(*passed),
                    at,
                    sha,
                    item.build_fingerprint,
                    hist_integrity,
                ],
            )?;
        }

        tx.execute("DELETE FROM walk_files WHERE walk_id = ?1", [walk_id])?;
        for (pos, file) in item.files.iter().enumerate() {
            let wf_integrity =
                compute_walk_file_integrity(walk_id, pos as i64, &file.path, &file.sha256);
            tx.execute(
                "INSERT INTO walk_files (walk_id, position, path, sha256, integrity_sha256)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![walk_id, pos as i64, file.path, file.sha256, wf_integrity],
            )?;
        }

        tx.execute("DELETE FROM repair_history WHERE walk_id = ?1", [walk_id])?;
        for (pos, repair) in item.repairs.iter().enumerate() {
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

    tx.pragma_update(None, "user_version", SQLITE_SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}
