//! Unified SQLite v5 walk store (`SqliteWalkStore`) with `O(1)` indexed lookups,
//! multi-build & multi-runtime `validation_runs`, durable `flaky` markers, `goal_embeddings`,
//! and persistent stat-verified `build_fingerprint_cache`.

use std::ffi::OsStr;
use std::fs::{self, Metadata};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use deja_core::{
    CacheMissReason, CoreError, DEFAULT_FLOW_SCHEMA_VERSION, ExplorationDetails, FlowChange,
    LookupHit, LookupMiss, LookupResult, RepairProvenance, StoredClosureCheck, StoredFlowFile,
    StoredValidation, StoredWalk, WalkIdentity, WalkOrigin, WalkValidation,
    check_manifest_flow_path, check_stored_subflows, cmp_utf16, collect_flow_closure,
    fingerprint_file, fingerprint_tree, normalize_goal, plan_stored_closure,
    read_stored_flow_files,
};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::debug;

use crate::error::StoreError;
use crate::lock::{
    BeforeMoveAsideHook, SQLITE_MEMORY_PATH, WriterLock, canonical_metadata_target,
    create_new_nofollow, generate_token, is_valid_iso8601, lexical_normalize, now_iso8601,
    open_read_nofollow, path_inside, store_metadata_path,
};
use crate::schema::{
    BuildFingerprintCacheIntegrityFields, ValidationHistoryIntegrityFields,
    ValidationRunIntegrityFields, WalkIntegrityFields, apply_readonly_pragmas,
    apply_writable_pragmas, compute_build_fingerprint_cache_integrity,
    compute_goal_embedding_integrity, compute_repair_history_integrity,
    compute_validation_history_integrity, compute_validation_run_integrity,
    compute_walk_file_integrity, compute_walk_integrity, ensure_writable_schema, is_non_empty,
    is_sha256_hex, origin_to_str, parse_origin_str, validate_exploration,
    validate_repair_provenance, verify_build_fingerprint_cache_integrity,
    verify_goal_embedding_integrity, verify_readonly_schema, verify_repair_history_integrity,
    verify_validation_history_integrity, verify_validation_run_integrity,
    verify_walk_file_integrity, verify_walk_integrity,
};

const MAX_HISTORY_ENTRIES: i64 = 100;
const MAX_EMBEDDING_DIMENSIONS: usize = 4096;
const STREAM_CHUNK_BYTES: usize = 65_536;

/// Configuration for opening a [`SqliteWalkStore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteStoreOptions {
    pub workspace_root: PathBuf,
    pub store_id: Option<String>,
}

impl SqliteStoreOptions {
    /// Construct options for the default workspace store (`.deja/memory/walks.sqlite`).
    #[must_use]
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            store_id: None,
        }
    }

    /// Assign an isolated `--store <id>` directory under `.deja/memory/stores/<id>/`.
    #[must_use]
    pub fn with_store_id(mut self, store_id: impl Into<String>) -> Self {
        self.store_id = Some(store_id.into());
        self
    }
}

/// Normalized and validated walk identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedWalkIdentity {
    pub goal: String,
    pub goal_key: String,
    pub app_id: String,
    pub platform: String,
    pub flow_schema_version: String,
    pub build_fingerprint: String,
    pub adapter: Option<String>,
    pub browser: Option<String>,
}

/// Normalize and validate a [`WalkIdentity`], failing closed on empty fields, invalid SHA-256
/// build fingerprints, or missing Web `adapter`/`browser`.
pub fn normalize_walk_identity(
    identity: &WalkIdentity,
) -> Result<NormalizedWalkIdentity, StoreError> {
    let norm = normalize_goal(&identity.goal)?;
    if !is_non_empty(&identity.app_id)
        || !is_non_empty(&identity.platform)
        || !is_non_empty(&identity.flow_schema_version)
        || !is_sha256_hex(&identity.build_fingerprint)
    {
        return Err(StoreError::InvalidIdentity(
            "A goal, app, platform, flow schema, and SHA-256 build fingerprint are required"
                .to_string(),
        ));
    }
    if identity
        .adapter
        .as_deref()
        .is_some_and(|a| !is_non_empty(a))
        || identity
            .browser
            .as_deref()
            .is_some_and(|b| !is_non_empty(b))
    {
        return Err(StoreError::InvalidIdentity(
            "Adapter and browser identity must be non-empty when present".to_string(),
        ));
    }
    if identity.platform == "web" && (identity.adapter.is_none() || identity.browser.is_none()) {
        return Err(StoreError::InvalidIdentity(
            "Web cache identity requires explicit adapter and browser names".to_string(),
        ));
    }
    Ok(NormalizedWalkIdentity {
        goal: norm.text,
        goal_key: norm.key,
        app_id: identity.app_id.clone(),
        platform: identity.platform.clone(),
        flow_schema_version: identity.flow_schema_version.clone(),
        build_fingerprint: identity.build_fingerprint.clone(),
        adapter: identity.adapter.clone(),
        browser: identity.browser.clone(),
    })
}

pub(crate) fn validate_runtime(runtime: Option<&str>) -> Result<Option<&str>, StoreError> {
    match runtime {
        None => Ok(None),
        Some(rt) if is_non_empty(rt) => Ok(Some(rt)),
        Some(_) => Err(StoreError::InvalidIdentity(
            "runtime must be non-empty when present".to_string(),
        )),
    }
}

pub(crate) fn flow_slug(goal_key: &str) -> Result<String, StoreError> {
    let mut slug = String::with_capacity(goal_key.len());
    let mut prev_dash = true;
    for ch in goal_key.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            slug.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        return Err(StoreError::InvalidIdentity(
            "The normalized goal must contain a letter or number".to_string(),
        ));
    }
    Ok(slug)
}

/// One persisted per-`(build_fingerprint, runtime)` validation outcome (`DEJA-12`, `DEJA-42`, `DEJA-55`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationRunRecord {
    pub walk_id: i64,
    pub build_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    pub flow_path: String,
    pub flow_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closure_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subflow_files: Option<Vec<StoredFlowFile>>,
    pub passed: bool,
    pub flaky: bool,
    pub validated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<WalkOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_timings_json: Option<String>,
}

/// Parameters for recording a validation result against a walk + `(build_fingerprint, runtime)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordValidationInput {
    pub passed: bool,
    pub at: String,
    pub runtime: Option<String>,
    pub flaky: bool,
    pub origin: Option<WalkOrigin>,
    pub failure_category: Option<String>,
    pub report_path: Option<String>,
    pub duration_ms: Option<u64>,
    pub phase_timings_json: Option<String>,
}

impl RecordValidationInput {
    /// Convenience constructor for a passing or failing validation at `at`.
    #[must_use]
    pub fn new(passed: bool, at: impl Into<String>) -> Self {
        Self {
            passed,
            at: at.into(),
            runtime: None,
            flaky: false,
            origin: None,
            failure_category: None,
            report_path: None,
            duration_ms: None,
            phase_timings_json: None,
        }
    }

    #[must_use]
    pub fn with_runtime(mut self, runtime: impl Into<String>) -> Self {
        self.runtime = Some(runtime.into());
        self
    }

    #[must_use]
    pub fn with_flaky(mut self, flaky: bool) -> Self {
        self.flaky = flaky;
        self
    }

    #[must_use]
    pub fn with_origin(mut self, origin: WalkOrigin) -> Self {
        self.origin = Some(origin);
        self
    }

    #[must_use]
    pub fn with_failure_category(mut self, category: impl Into<String>) -> Self {
        self.failure_category = Some(category.into());
        self
    }

    #[must_use]
    pub fn with_report_path(mut self, path: impl Into<String>) -> Self {
        self.report_path = Some(path.into());
        self
    }

    #[must_use]
    pub fn with_duration_ms(mut self, duration_ms: u64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }

    #[must_use]
    pub fn with_phase_timings_json(mut self, json: impl Into<String>) -> Self {
        self.phase_timings_json = Some(json.into());
        self
    }
}

/// Complete input for creating or updating a walk and its `(build_fingerprint, runtime)` validation run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpsertWalkInput {
    pub identity: WalkIdentity,
    pub flow_path: String,
    pub flow_sha256: String,
    pub subflow_files: Option<Vec<StoredFlowFile>>,
    pub legacy_closure: bool,
    pub closure_sha256: Option<String>,
    pub validation: RecordValidationInput,
    pub exploration: Option<ExplorationDetails>,
    pub repair_history: Option<Vec<RepairProvenance>>,
}

/// Input for materializing a verified candidate closure under `.deja/flows/` and persisting its walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistVerifiedCandidateOptions {
    pub candidate_path: PathBuf,
    pub validation: RecordValidationInput,
    pub expected_flow_sha256: Option<String>,
    pub expected_closure_sha256: Option<String>,
    pub exploration: Option<ExplorationDetails>,
    pub repair_history: Option<Vec<RepairProvenance>>,
}

/// One file copied into an isolated candidate snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateSnapshotFile {
    pub source: String,
    pub path: String,
    pub sha256: String,
}

/// Snapshot of a candidate flow closure materialized under `.deja/attempts/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateSnapshot {
    pub candidate_path: String,
    pub flow_sha256: String,
    pub closure_sha256: String,
    pub files: Vec<CandidateSnapshotFile>,
    pub snapshot_dir: PathBuf,
}

/// Persisted embedding vector associated with a stored walk and provider.
#[derive(Debug, Clone, PartialEq)]
pub struct GoalEmbeddingRecord {
    pub walk_id: i64,
    pub provider: String,
    pub embedding: Vec<f64>,
    pub updated_at: String,
}

/// Result of [`SqliteWalkStore::fingerprint_with_cache`] including cache-hit telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FingerprintCacheResult {
    pub digest: String,
    pub cache_hit: bool,
    pub bytes_read: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenMode {
    ReadOnly,
    Writable,
}

struct StoreInner {
    mode: OpenMode,
    conn: Option<Connection>,
    conn_immutable: bool,
    conn_stamp: Option<DbFileStamp>,
    lock: Option<WriterLock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DbFileStamp {
    len: u64,
    modified: SystemTime,
    change_counter: [u8; 4],
}

impl Drop for StoreInner {
    fn drop(&mut self) {
        if self.mode == OpenMode::Writable
            && let Some(ref conn) = self.conn
        {
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        }
        self.conn.take();
        if let Some(mut lock) = self.lock.take() {
            lock.release();
        }
    }
}

/// Validated `walks` table row verified against its `integrity_sha256`.
#[derive(Debug, Clone)]
pub(crate) struct ValidatedWalkRow {
    pub id: i64,
    pub goal: String,
    pub goal_key: String,
    pub app_id: String,
    pub platform: String,
    pub flow_schema_version: String,
    pub build_fingerprint: String,
    pub adapter: Option<String>,
    pub browser: Option<String>,
    pub flow_path: String,
    pub flow_sha256: String,
    pub last_passed: bool,
    pub last_at: String,
    pub origin: Option<WalkOrigin>,
    pub exploration: Option<ExplorationDetails>,
    pub has_files: bool,
    pub legacy_closure: bool,
    pub closure_sha256: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum ClassicFlowCheck {
    Ok(PathBuf),
    Missing,
    Legacy,
    Invalid(String),
    FlowChanged {
        detail: String,
        expected_closure_sha256: String,
        current_closure_sha256: Option<String>,
    },
}

/// Unified, thread-safe (`Send + Sync`) SQLite v5 walk store.
pub struct SqliteWalkStore {
    workspace_root: PathBuf,
    store_id: Option<String>,
    relative_memory_path: String,
    memory_path: PathBuf,
    inner: Mutex<StoreInner>,
}

impl std::fmt::Debug for SqliteWalkStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteWalkStore")
            .field("workspace_root", &self.workspace_root)
            .field("store_id", &self.store_id)
            .field("relative_memory_path", &self.relative_memory_path)
            .finish()
    }
}

impl SqliteWalkStore {
    /// Open or initialize a writable SQLite v5 store, acquiring its single-writer `.lock` first.
    ///
    /// Refuses symlinked DB files and symlinked `-wal`/`-shm`/`-journal` sidecars before opening.
    pub fn open_writable(options: SqliteStoreOptions) -> Result<Self, StoreError> {
        Self::open_writable_with_lock_hook(options, None)
    }

    /// Open a writable SQLite v5 store with an optional deterministic stale-lock test hook.
    pub fn open_writable_with_lock_hook(
        options: SqliteStoreOptions,
        hook: Option<BeforeMoveAsideHook>,
    ) -> Result<Self, StoreError> {
        let relative_memory_path =
            store_metadata_path(SQLITE_MEMORY_PATH, options.store_id.as_deref())?;

        let target =
            canonical_metadata_target(&options.workspace_root, Path::new(&relative_memory_path))?;
        let lock_path = append_suffix(&target.path, ".lock");
        let lock = WriterLock::acquire_with_hook(lock_path, hook)?;

        if let Some(parent) = target.path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Re-verify confinement and sidecars right after directory creation.
        let target =
            canonical_metadata_target(&options.workspace_root, Path::new(&relative_memory_path))?;

        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let mut conn = Connection::open_with_flags(&target.path, flags).map_err(|err| {
            StoreError::InvalidWalkMemory(format!("walks.sqlite cannot be opened: {err}"))
        })?;
        apply_writable_pragmas(&conn)?;
        lock.verify_held()?;
        ensure_writable_schema(&mut conn)?;

        let memory_path = target.path;
        debug!(path = %memory_path.display(), "opened writable SQLite v5 store");
        Ok(Self {
            workspace_root: options.workspace_root,
            store_id: options.store_id,
            relative_memory_path,
            memory_path,
            inner: Mutex::new(StoreInner {
                mode: OpenMode::Writable,
                conn: Some(conn),
                conn_immutable: false,
                conn_stamp: None,
                lock: Some(lock),
            }),
        })
    }

    /// Open a strictly read-only handle to a SQLite v5 store.
    ///
    /// Acquires **zero** writer locks and creates **zero** files or directories (including zero
    /// `-wal` / `-shm` sidecars on cleanly closed databases). If the database does not exist yet,
    /// succeeds with an empty read-only handle.
    pub fn open_readonly(options: SqliteStoreOptions) -> Result<Self, StoreError> {
        let relative_memory_path =
            store_metadata_path(SQLITE_MEMORY_PATH, options.store_id.as_deref())?;
        let target =
            canonical_metadata_target(&options.workspace_root, Path::new(&relative_memory_path))?;

        let (conn, conn_immutable, conn_stamp) = if target.exists {
            let (conn, immutable, stamp) = open_readonly_connection(&target.path)?;
            (Some(conn), immutable, stamp)
        } else {
            (None, false, None)
        };

        Ok(Self {
            workspace_root: options.workspace_root,
            store_id: options.store_id,
            relative_memory_path,
            memory_path: target.path,
            inner: Mutex::new(StoreInner {
                mode: OpenMode::ReadOnly,
                conn,
                conn_immutable,
                conn_stamp,
                lock: None,
            }),
        })
    }

    /// Workspace root path configured for this store.
    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Optional `--store <id>` identifier for this store.
    #[must_use]
    pub fn store_id(&self) -> Option<&str> {
        self.store_id.as_deref()
    }

    /// Human- and machine-readable store label used for multi-store gate attribution (`source_stores`).
    #[must_use]
    pub fn store_label(&self) -> String {
        self.store_id
            .clone()
            .unwrap_or_else(|| self.relative_memory_path.clone())
    }

    /// Workspace-relative SQLite metadata path.
    #[must_use]
    pub fn relative_memory_path(&self) -> &str {
        &self.relative_memory_path
    }

    /// Canonical SQLite metadata path inside the workspace.
    #[must_use]
    pub fn memory_path(&self) -> &Path {
        &self.memory_path
    }

    /// Checkpoint WAL (when writable), close the SQLite connection, and release the writer lock.
    pub fn close(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            if guard.mode == OpenMode::Writable
                && let Some(ref conn) = guard.conn
            {
                let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
            }
            guard.conn.take();
            guard.conn_immutable = false;
            guard.conn_stamp = None;
            if let Some(mut lock) = guard.lock.take() {
                lock.release();
            }
        }
    }

    fn lock_inner(&self) -> Result<std::sync::MutexGuard<'_, StoreInner>, StoreError> {
        self.inner.lock().map_err(|_| {
            StoreError::InvalidWalkMemory("store mutex poisoned after panic".to_string())
        })
    }

    fn ensure_read_conn<'a>(
        &self,
        guard: &'a mut StoreInner,
    ) -> Result<Option<&'a Connection>, StoreError> {
        let target =
            canonical_metadata_target(&self.workspace_root, Path::new(&self.relative_memory_path))?;
        if !target.exists {
            guard.conn.take();
            guard.conn_immutable = false;
            guard.conn_stamp = None;
            return Ok(None);
        }
        if guard.mode == OpenMode::ReadOnly && guard.conn.is_some() && guard.conn_immutable {
            let wal_exists = wal_sidecar_exists(&target.path);
            let current_stamp = read_db_file_stamp(&target.path);
            if wal_exists || current_stamp != guard.conn_stamp {
                guard.conn.take();
                guard.conn_immutable = false;
                guard.conn_stamp = None;
            }
        }
        if guard.conn.is_none() {
            let (conn, immutable, stamp) = open_readonly_connection(&target.path)?;
            guard.conn = Some(conn);
            guard.conn_immutable = immutable;
            guard.conn_stamp = stamp;
        }
        Ok(guard.conn.as_ref())
    }

    /// Verify that the underlying database (if present) is a readable, untampered SQLite v5 handle.
    pub(crate) fn verify_readonly_connection(&self) -> Result<(), StoreError> {
        let mut guard = self.lock_inner()?;
        let _ = self.ensure_read_conn(&mut guard)?;
        Ok(())
    }

    /// Execute a write closure inside a `BEGIN IMMEDIATE` transaction after verifying workspace
    /// confinement and writer-lock ownership both before starting and immediately before commit.
    pub(crate) fn with_write_tx<T>(
        &self,
        work: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut guard = self.lock_inner()?;
        if guard.mode != OpenMode::Writable {
            return Err(StoreError::InvalidWalkMemory(
                "cannot write to a read-only SqliteWalkStore".to_string(),
            ));
        }
        let _ =
            canonical_metadata_target(&self.workspace_root, Path::new(&self.relative_memory_path))?;
        {
            let lock = guard.lock.as_ref().ok_or_else(|| StoreError::StoreLocked {
                lock_path: append_suffix(&self.memory_path, ".lock")
                    .display()
                    .to_string(),
                holder: None,
                reason: "store writer lock has been closed".to_string(),
            })?;
            lock.verify_held()?;
        }

        let StoreInner { conn, lock, .. } = &mut *guard;
        let conn = conn.as_mut().ok_or_else(|| {
            StoreError::InvalidWalkMemory("SQLite connection is closed".to_string())
        })?;
        let lock = lock.as_ref().ok_or_else(|| StoreError::StoreLocked {
            lock_path: append_suffix(&self.memory_path, ".lock")
                .display()
                .to_string(),
            holder: None,
            reason: "store writer lock has been closed".to_string(),
        })?;

        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = work(&tx)?;
        // Final lock ownership check right before commit narrows the lost-lock window to commit itself.
        lock.verify_held()?;
        tx.commit()?;
        Ok(result)
    }

    /// Exact `O(1)` walk lookup for the default (unnamed) runtime (`runtime_key = ''`).
    pub fn lookup(&self, identity: &WalkIdentity) -> Result<LookupResult, StoreError> {
        self.lookup_for_runtime(identity, None)
    }

    /// Exact `O(1)` walk lookup for a specific optional runtime (`DEJA-55`) using `prepare_cached`
    /// + `idx_walks_identity` + `idx_vruns_identity` (never scans all walks).
    pub fn lookup_for_runtime(
        &self,
        identity: &WalkIdentity,
        runtime: Option<&str>,
    ) -> Result<LookupResult, StoreError> {
        let norm = normalize_walk_identity(identity)?;
        let rt = validate_runtime(runtime)?;
        let mut guard = self.lock_inner()?;
        let Some(conn) = self.ensure_read_conn(&mut guard)? else {
            return Ok(miss(CacheMissReason::NotFound));
        };

        let Some(walk) = query_walk_by_identity(conn, &norm)? else {
            let reason = classify_identity_miss(conn, &norm)?;
            return Ok(miss(reason));
        };

        let rt_key = rt.unwrap_or("");
        let Some(run) = query_validation_run(conn, walk.id, &norm.build_fingerprint, rt_key)?
        else {
            return Ok(miss(CacheMissReason::StaleBuild));
        };

        if !run.passed {
            return Ok(LookupResult::Miss(LookupMiss {
                reason: CacheMissReason::FailedValidation,
                flow_change: None,
                closure_sha256: run.closure_sha256.clone(),
                flow_path: Some(run.flow_path.clone()),
            }));
        }

        let canonical_flow =
            match verify_classic_stored_flow_inner(&self.workspace_root, &walk, &run)? {
                ClassicFlowCheck::Ok(path) => path,
                ClassicFlowCheck::Missing => return Ok(miss(CacheMissReason::MissingFlow)),
                ClassicFlowCheck::Legacy => return Ok(miss(CacheMissReason::LegacyClosureUnknown)),
                ClassicFlowCheck::Invalid(msg) => return Err(StoreError::InvalidWalkMemory(msg)),
                ClassicFlowCheck::FlowChanged {
                    expected_closure_sha256,
                    current_closure_sha256,
                    ..
                } => {
                    return Ok(LookupResult::Miss(LookupMiss {
                        reason: CacheMissReason::FlowChanged,
                        flow_change: current_closure_sha256.map(|current| FlowChange {
                            manifest_closure_sha256: expected_closure_sha256.clone(),
                            current_closure_sha256: current,
                        }),
                        closure_sha256: Some(expected_closure_sha256),
                        flow_path: Some(run.flow_path.clone()),
                    }));
                }
            };

        let stored = hydrate_stored_walk(conn, &walk, &run)?;
        Ok(LookupResult::Hit(Box::new(LookupHit {
            walk: stored,
            flow_path: canonical_flow.to_string_lossy().into_owned(),
        })))
    }

    /// Diagnostic-only lookup for the latest passing validation across builds for this logical walk.
    pub fn lookup_previous_successful(
        &self,
        identity: &WalkIdentity,
    ) -> Result<LookupResult, StoreError> {
        self.lookup_previous_successful_for_runtime(identity, None)
    }

    /// Diagnostic-only lookup for the latest passing validation across builds, optionally filtered by `runtime`.
    pub fn lookup_previous_successful_for_runtime(
        &self,
        identity: &WalkIdentity,
        runtime: Option<&str>,
    ) -> Result<LookupResult, StoreError> {
        let norm = normalize_walk_identity(identity)?;
        let rt = validate_runtime(runtime)?;
        let mut guard = self.lock_inner()?;
        let Some(conn) = self.ensure_read_conn(&mut guard)? else {
            return Ok(miss(CacheMissReason::NotFound));
        };

        let Some(walk) = query_walk_by_identity(conn, &norm)? else {
            return Ok(miss(CacheMissReason::NotFound));
        };

        let Some(run) = query_latest_passing_run(conn, walk.id, rt)? else {
            return Ok(miss(CacheMissReason::NotFound));
        };

        let canonical_flow =
            match verify_classic_stored_flow_inner(&self.workspace_root, &walk, &run)? {
                ClassicFlowCheck::Ok(path) => path,
                ClassicFlowCheck::Missing => return Ok(miss(CacheMissReason::MissingFlow)),
                ClassicFlowCheck::Legacy => return Ok(miss(CacheMissReason::LegacyClosureUnknown)),
                ClassicFlowCheck::Invalid(msg) => return Err(StoreError::InvalidWalkMemory(msg)),
                ClassicFlowCheck::FlowChanged {
                    expected_closure_sha256,
                    current_closure_sha256,
                    ..
                } => {
                    return Ok(LookupResult::Miss(LookupMiss {
                        reason: CacheMissReason::FlowChanged,
                        flow_change: current_closure_sha256.map(|current| FlowChange {
                            manifest_closure_sha256: expected_closure_sha256.clone(),
                            current_closure_sha256: current,
                        }),
                        closure_sha256: Some(expected_closure_sha256),
                        flow_path: Some(run.flow_path.clone()),
                    }));
                }
            };

        let stored = hydrate_stored_walk(conn, &walk, &run)?;
        Ok(LookupResult::Hit(Box::new(LookupHit {
            walk: stored,
            flow_path: canonical_flow.to_string_lossy().into_owned(),
        })))
    }

    /// Create or update a walk and its `(build_fingerprint, runtime)` validation run inside a
    /// single `BEGIN IMMEDIATE` transaction, preserving other builds and runtimes (`DEJA-12`, `DEJA-55`).
    pub fn upsert_walk(&self, input: &UpsertWalkInput) -> Result<StoredWalk, StoreError> {
        let norm = normalize_walk_identity(&input.identity)?;
        if norm.flow_schema_version != DEFAULT_FLOW_SCHEMA_VERSION {
            return Err(StoreError::InvalidIdentity(format!(
                "unsupported flow schema version: {}",
                norm.flow_schema_version
            )));
        }
        validate_validation_input(&input.validation)?;
        if !is_non_empty(&input.flow_path) || !is_sha256_hex(&input.flow_sha256) {
            return Err(StoreError::InvalidIdentity(
                "walk requires non-empty flowPath and SHA-256 flowSha256".to_string(),
            ));
        }

        let has_files = if let Some(ref csha) = input.closure_sha256 {
            if !is_sha256_hex(csha) {
                return Err(StoreError::InvalidIdentity(
                    "closureSha256 must be a lowercase 64-char hex SHA-256".to_string(),
                ));
            }
            check_manifest_flow_path(&input.flow_path, "flowPath")?;
            if input.subflow_files.is_some() || input.legacy_closure {
                return Err(StoreError::InvalidIdentity(
                    "manifest closure walks must not carry subflowFiles or legacyClosure"
                        .to_string(),
                ));
            }
            false
        } else {
            if let Some(ref files) = input.subflow_files {
                let raw = serde_json::to_value(files).expect("subflow_files serialize");
                if read_stored_flow_files(&raw).is_none() {
                    return Err(StoreError::InvalidIdentity(
                        "subflowFiles entries must stay under .deja/flows/ with SHA-256 checksums"
                            .to_string(),
                    ));
                }
            }
            true
        };

        let exploration_json = match input.exploration {
            Some(ref details) => {
                if !is_non_empty(&details.model_call_basis) {
                    return Err(StoreError::InvalidIdentity(
                        "exploration.modelCallBasis must be non-empty".to_string(),
                    ));
                }
                Some(serde_json::to_string(details).expect("ExplorationDetails serializes"))
            }
            None => None,
        };

        if let Some(ref repairs) = input.repair_history {
            if repairs.is_empty() {
                return Err(StoreError::InvalidIdentity(
                    "repairHistory must be non-empty when present".to_string(),
                ));
            }
            for repair in repairs {
                validate_repair_provenance(repair)?;
            }
        }

        let origin_str = input.validation.origin.map(origin_to_str);
        let subflow_list = input.subflow_files.clone().unwrap_or_default();
        let subflow_json = if has_files && !input.legacy_closure {
            Some(serde_json::to_string(&subflow_list).expect("subflow_files serialize"))
        } else {
            None
        };
        let duration_i64 = input
            .validation
            .duration_ms
            .map(i64::try_from)
            .transpose()
            .map_err(|_| StoreError::InvalidIdentity("duration_ms exceeds i64::MAX".to_string()))?;

        self.with_write_tx(|tx| {
            let walk_integrity = compute_walk_integrity(&WalkIntegrityFields {
                goal: &norm.goal,
                goal_key: &norm.goal_key,
                app_id: &norm.app_id,
                platform: &norm.platform,
                flow_schema_version: &norm.flow_schema_version,
                build_fingerprint: &norm.build_fingerprint,
                adapter: norm.adapter.as_deref(),
                browser: norm.browser.as_deref(),
                flow_path: &input.flow_path,
                flow_sha256: &input.flow_sha256,
                last_passed: input.validation.passed,
                last_at: &input.validation.at,
                origin: origin_str,
                exploration_json: exploration_json.as_deref(),
                has_files,
                legacy_closure: input.legacy_closure,
                closure_sha256: input.closure_sha256.as_deref(),
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
                    origin = COALESCE(excluded.origin, walks.origin),
                    exploration_json = COALESCE(excluded.exploration_json, walks.exploration_json),
                    has_files = excluded.has_files,
                    legacy_closure = excluded.legacy_closure,
                    closure_sha256 = excluded.closure_sha256,
                    integrity_sha256 = excluded.integrity_sha256
                RETURNING id",
                params![
                    norm.goal,
                    norm.goal_key,
                    norm.app_id,
                    norm.platform,
                    norm.flow_schema_version,
                    norm.build_fingerprint,
                    norm.adapter,
                    norm.browser,
                    input.flow_path,
                    input.flow_sha256,
                    i64::from(input.validation.passed),
                    input.validation.at,
                    origin_str,
                    exploration_json,
                    i64::from(has_files),
                    i64::from(input.legacy_closure),
                    input.closure_sha256,
                    walk_integrity,
                ],
                |r| r.get(0),
            )?;

            // Recompute integrity if COALESCE retained an earlier origin or exploration_json.
            let stored_origin_and_exp: (Option<String>, Option<String>) = tx.query_row(
                "SELECT origin, exploration_json FROM walks WHERE id = ?1",
                [walk_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let final_walk_integrity = compute_walk_integrity(&WalkIntegrityFields {
                goal: &norm.goal,
                goal_key: &norm.goal_key,
                app_id: &norm.app_id,
                platform: &norm.platform,
                flow_schema_version: &norm.flow_schema_version,
                build_fingerprint: &norm.build_fingerprint,
                adapter: norm.adapter.as_deref(),
                browser: norm.browser.as_deref(),
                flow_path: &input.flow_path,
                flow_sha256: &input.flow_sha256,
                last_passed: input.validation.passed,
                last_at: &input.validation.at,
                origin: stored_origin_and_exp.0.as_deref(),
                exploration_json: stored_origin_and_exp.1.as_deref(),
                has_files,
                legacy_closure: input.legacy_closure,
                closure_sha256: input.closure_sha256.as_deref(),
            });
            if final_walk_integrity != walk_integrity {
                tx.execute(
                    "UPDATE walks SET integrity_sha256 = ?1 WHERE id = ?2",
                    params![final_walk_integrity, walk_id],
                )?;
            }

            let run_integrity = compute_validation_run_integrity(&ValidationRunIntegrityFields {
                walk_id,
                build_fingerprint: &norm.build_fingerprint,
                runtime: input.validation.runtime.as_deref(),
                flow_path: &input.flow_path,
                flow_sha256: &input.flow_sha256,
                closure_sha256: input.closure_sha256.as_deref(),
                subflow_files_json: subflow_json.as_deref(),
                passed: input.validation.passed,
                flaky: input.validation.flaky,
                validated_at: &input.validation.at,
                origin: origin_str,
                failure_category: input.validation.failure_category.as_deref(),
                report_path: input.validation.report_path.as_deref(),
                duration_ms: duration_i64,
                phase_timings_json: input.validation.phase_timings_json.as_deref(),
            });

            tx.execute(
                "INSERT INTO validation_runs (
                    walk_id, build_fingerprint, runtime, flow_path, flow_sha256, closure_sha256,
                    subflow_files_json, passed, flaky, validated_at, origin, failure_category,
                    report_path, duration_ms, phase_timings_json, integrity_sha256
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
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
                    norm.build_fingerprint,
                    input.validation.runtime,
                    input.flow_path,
                    input.flow_sha256,
                    input.closure_sha256,
                    subflow_json,
                    i64::from(input.validation.passed),
                    i64::from(input.validation.flaky),
                    input.validation.at,
                    origin_str,
                    input.validation.failure_category,
                    input.validation.report_path,
                    duration_i64,
                    input.validation.phase_timings_json,
                    run_integrity,
                ],
            )?;

            append_validation_history(
                tx,
                walk_id,
                input.validation.passed,
                &input.validation.at,
                &input.flow_sha256,
                Some(&norm.build_fingerprint),
                input.validation.runtime.as_deref(),
                input.validation.flaky,
            )?;

            tx.execute("DELETE FROM walk_files WHERE walk_id = ?1", [walk_id])?;
            if has_files {
                for (pos, file) in subflow_list.iter().enumerate() {
                    let wf_integrity =
                        compute_walk_file_integrity(walk_id, pos as i64, &file.path, &file.sha256);
                    tx.execute(
                        "INSERT INTO walk_files (walk_id, position, path, sha256, integrity_sha256)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![walk_id, pos as i64, file.path, file.sha256, wf_integrity],
                    )?;
                }
            }

            if let Some(ref repairs) = input.repair_history {
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

            let walk_row = query_walk_by_id(tx, walk_id)?;
            let run_row = query_validation_run(
                tx,
                walk_id,
                &norm.build_fingerprint,
                input.validation.runtime.as_deref().unwrap_or(""),
            )?
            .ok_or_else(|| {
                StoreError::InvalidWalkMemory("validation_runs row missing after upsert".into())
            })?;
            hydrate_stored_walk(tx, &walk_row, &run_row)
        })
    }

    /// Record a new validation result (`passed`, `flaky`, `runtime`, `duration_ms`,
    /// `phase_timings_json`, etc.) for an existing logical walk without discarding other builds
    /// or runtimes (`DEJA-12`, `DEJA-42`, `DEJA-55`).
    pub fn record_validation(
        &self,
        identity: &WalkIdentity,
        validation: &RecordValidationInput,
    ) -> Result<StoredWalk, StoreError> {
        let norm = normalize_walk_identity(identity)?;
        validate_validation_input(validation)?;

        let origin_str = validation.origin.map(origin_to_str);
        let duration_i64 = validation
            .duration_ms
            .map(i64::try_from)
            .transpose()
            .map_err(|_| StoreError::InvalidIdentity("duration_ms exceeds i64::MAX".to_string()))?;

        self.with_write_tx(|tx| {
            let walk = query_walk_by_identity(tx, &norm)?.ok_or_else(|| {
                StoreError::InvalidWalkMemory(format!(
                    "cannot record validation for unknown walk {:?}",
                    norm.goal_key
                ))
            })?;

            // Use the matching (build, runtime) run's flow metadata when available, otherwise
            // inherit the walk's latest stored flow metadata (e.g. replaying on a new build/runtime).
            // Keep `(flow_path, flow_sha256, has_files, legacy_closure, closure_sha256)` consistent
            // as a unit even when switching between classic and manifest-backed builds.
            let rt_key = validation.runtime.as_deref().unwrap_or("");
            let existing_run = query_validation_run(tx, walk.id, &norm.build_fingerprint, rt_key)?;
            let (
                flow_path,
                flow_sha256,
                closure_sha256,
                subflow_files_json,
                next_has_files,
                next_legacy_closure,
                next_subflow_files,
            ) = if let Some(ref prev) = existing_run {
                let has_files = prev.closure_sha256.is_none();
                let legacy_closure = has_files && prev.subflow_files.is_none();
                let sf_json = prev
                    .subflow_files
                    .as_ref()
                    .map(|f| serde_json::to_string(f).expect("subflow_files serialize"));
                (
                    prev.flow_path.clone(),
                    prev.flow_sha256.clone(),
                    prev.closure_sha256.clone(),
                    sf_json,
                    has_files,
                    legacy_closure,
                    prev.subflow_files.clone(),
                )
            } else {
                let files = if walk.has_files && !walk.legacy_closure {
                    Some(query_walk_files(tx, walk.id)?)
                } else {
                    None
                };
                let sf_json = files
                    .as_ref()
                    .map(|f| serde_json::to_string(f).expect("subflow_files serialize"));
                (
                    walk.flow_path.clone(),
                    walk.flow_sha256.clone(),
                    walk.closure_sha256.clone(),
                    sf_json,
                    walk.has_files,
                    walk.legacy_closure,
                    files,
                )
            };

            let next_origin = origin_str.or_else(|| walk.origin.map(origin_to_str));
            let exp_json = walk
                .exploration
                .as_ref()
                .map(|e| serde_json::to_string(e).expect("ExplorationDetails serializes"));

            let walk_integrity = compute_walk_integrity(&WalkIntegrityFields {
                goal: &walk.goal,
                goal_key: &walk.goal_key,
                app_id: &walk.app_id,
                platform: &walk.platform,
                flow_schema_version: &walk.flow_schema_version,
                build_fingerprint: &norm.build_fingerprint,
                adapter: walk.adapter.as_deref(),
                browser: walk.browser.as_deref(),
                flow_path: &flow_path,
                flow_sha256: &flow_sha256,
                last_passed: validation.passed,
                last_at: &validation.at,
                origin: next_origin,
                exploration_json: exp_json.as_deref(),
                has_files: next_has_files,
                legacy_closure: next_legacy_closure,
                closure_sha256: closure_sha256.as_deref(),
            });

            tx.execute(
                "UPDATE walks SET
                    build_fingerprint = ?1,
                    flow_path = ?2,
                    flow_sha256 = ?3,
                    last_passed = ?4,
                    last_at = ?5,
                    origin = ?6,
                    has_files = ?7,
                    legacy_closure = ?8,
                    closure_sha256 = ?9,
                    integrity_sha256 = ?10
                 WHERE id = ?11",
                params![
                    norm.build_fingerprint,
                    flow_path,
                    flow_sha256,
                    i64::from(validation.passed),
                    validation.at,
                    next_origin,
                    i64::from(next_has_files),
                    i64::from(next_legacy_closure),
                    closure_sha256,
                    walk_integrity,
                    walk.id,
                ],
            )?;

            tx.execute("DELETE FROM walk_files WHERE walk_id = ?1", [walk.id])?;
            if next_has_files && let Some(ref files) = next_subflow_files {
                for (pos, file) in files.iter().enumerate() {
                    let wf_integrity =
                        compute_walk_file_integrity(walk.id, pos as i64, &file.path, &file.sha256);
                    tx.execute(
                        "INSERT INTO walk_files (walk_id, position, path, sha256, integrity_sha256)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![walk.id, pos as i64, file.path, file.sha256, wf_integrity],
                    )?;
                }
            }

            let run_integrity = compute_validation_run_integrity(&ValidationRunIntegrityFields {
                walk_id: walk.id,
                build_fingerprint: &norm.build_fingerprint,
                runtime: validation.runtime.as_deref(),
                flow_path: &flow_path,
                flow_sha256: &flow_sha256,
                closure_sha256: closure_sha256.as_deref(),
                subflow_files_json: subflow_files_json.as_deref(),
                passed: validation.passed,
                flaky: validation.flaky,
                validated_at: &validation.at,
                origin: next_origin,
                failure_category: validation.failure_category.as_deref(),
                report_path: validation.report_path.as_deref(),
                duration_ms: duration_i64,
                phase_timings_json: validation.phase_timings_json.as_deref(),
            });

            tx.execute(
                "INSERT INTO validation_runs (
                    walk_id, build_fingerprint, runtime, flow_path, flow_sha256, closure_sha256,
                    subflow_files_json, passed, flaky, validated_at, origin, failure_category,
                    report_path, duration_ms, phase_timings_json, integrity_sha256
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
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
                    walk.id,
                    norm.build_fingerprint,
                    validation.runtime,
                    flow_path,
                    flow_sha256,
                    closure_sha256,
                    subflow_files_json,
                    i64::from(validation.passed),
                    i64::from(validation.flaky),
                    validation.at,
                    next_origin,
                    validation.failure_category,
                    validation.report_path,
                    duration_i64,
                    validation.phase_timings_json,
                    run_integrity,
                ],
            )?;

            append_validation_history(
                tx,
                walk.id,
                validation.passed,
                &validation.at,
                &flow_sha256,
                Some(&norm.build_fingerprint),
                validation.runtime.as_deref(),
                validation.flaky,
            )?;

            let updated_walk = query_walk_by_id(tx, walk.id)?;
            let updated_run = query_validation_run(tx, walk.id, &norm.build_fingerprint, rt_key)?
                .ok_or_else(|| {
                StoreError::InvalidWalkMemory(
                    "validation_runs row missing after record_validation".into(),
                )
            })?;
            hydrate_stored_walk(tx, &updated_walk, &updated_run)
        })
    }

    /// Materialize a workspace-confined snapshot of a candidate flow closure under
    /// `.deja/attempts/snapshot-<token>/` with atomic cleanup on failure.
    pub fn snapshot_candidate(
        &self,
        candidate_path: &Path,
    ) -> Result<CandidateSnapshot, StoreError> {
        let canonical_root = fs::canonicalize(&self.workspace_root)?;
        let closure =
            collect_flow_closure(&self.workspace_root, candidate_path).map_err(|err| {
                if matches!(err, deja_core::CoreError::MissingFlowFile { .. }) {
                    StoreError::CandidateNotFound(candidate_path.display().to_string())
                } else {
                    StoreError::Core(err)
                }
            })?;

        let main = closure
            .files
            .iter()
            .find(|f| f.path == closure.main)
            .ok_or_else(|| {
                StoreError::InvalidWalkMemory("collected closure missing main flow".to_string())
            })?;

        let token = generate_token();
        let rel_dir = format!(".deja/attempts/snapshot-{token}");
        let snapshot_dir = canonical_root.join(&rel_dir);

        let write_result = (|| -> Result<Vec<CandidateSnapshotFile>, StoreError> {
            fs::create_dir_all(&snapshot_dir)?;
            let canonical_snap_dir = fs::canonicalize(&snapshot_dir)?;
            if !path_inside(&canonical_root, &canonical_snap_dir) {
                return Err(StoreError::InvalidWalkMemory(
                    "snapshot directory escapes workspace".to_string(),
                ));
            }
            let mut copied = Vec::with_capacity(closure.files.len());
            for file in &closure.files {
                let dest = lexical_normalize(&canonical_snap_dir.join(&file.path));
                if !path_inside(&canonical_snap_dir, &dest) {
                    return Err(StoreError::InvalidWalkMemory(format!(
                        "snapshot file escapes snapshot directory: {}",
                        file.path
                    )));
                }
                atomic_write_confined_file(&canonical_root, &dest, &file.bytes)?;
                copied.push(CandidateSnapshotFile {
                    source: file.source.clone(),
                    path: file.path.clone(),
                    sha256: file.sha256.clone(),
                });
            }
            Ok(copied)
        })();

        match write_result {
            Ok(files) => Ok(CandidateSnapshot {
                candidate_path: format!("{rel_dir}/{}", closure.main),
                flow_sha256: main.sha256.clone(),
                closure_sha256: closure.sha256,
                files,
                snapshot_dir,
            }),
            Err(err) => {
                let _ = fs::remove_dir_all(&snapshot_dir);
                Err(err)
            }
        }
    }

    /// Collect and materialize a verified candidate closure under `.deja/flows/`, then persist
    /// its walk and validation run atomically, rolling back newly created flow files on failure.
    pub fn persist_verified_candidate(
        &self,
        identity: &WalkIdentity,
        options: &PersistVerifiedCandidateOptions,
    ) -> Result<StoredWalk, StoreError> {
        let norm = normalize_walk_identity(identity)?;
        validate_validation_input(&options.validation)?;
        let canonical_root = fs::canonicalize(&self.workspace_root)?;
        let closure =
            collect_flow_closure(&self.workspace_root, &options.candidate_path).map_err(|err| {
                if matches!(err, deja_core::CoreError::MissingFlowFile { .. }) {
                    StoreError::CandidateNotFound(options.candidate_path.display().to_string())
                } else {
                    StoreError::Core(err)
                }
            })?;

        let main = closure
            .files
            .iter()
            .find(|f| f.path == closure.main)
            .ok_or_else(|| {
                StoreError::InvalidWalkMemory("collected closure missing main flow".to_string())
            })?;

        if let Some(ref expected) = options.expected_flow_sha256
            && expected != &main.sha256
        {
            return Err(StoreError::Core(
                deja_core::CoreError::ExpectedFlowChecksumMismatch {
                    expected: expected.clone(),
                    actual: main.sha256.clone(),
                },
            ));
        }
        if let Some(ref expected) = options.expected_closure_sha256
            && expected != &closure.sha256
        {
            return Err(StoreError::Core(
                deja_core::CoreError::ExpectedClosureMismatch {
                    expected: expected.clone(),
                    actual: closure.sha256.clone(),
                },
            ));
        }

        let goal_slug = flow_slug(&norm.goal_key)?;
        let platform_slug = flow_slug(&norm.platform.to_ascii_lowercase())?;
        let plan = plan_stored_closure(&closure, &goal_slug, &platform_slug);

        let flow_root = lexical_normalize(&canonical_root.join(".deja/flows"));
        let mut newly_written: Vec<PathBuf> = Vec::new();
        let write_files_res = (|| -> Result<(), StoreError> {
            for (rel_dest, bytes) in &plan.writes {
                let dest = lexical_normalize(&canonical_root.join(rel_dest));
                if !path_inside(&flow_root, &dest) {
                    return Err(StoreError::InvalidWalkMemory(format!(
                        "planned flow file escapes .deja/flows: {rel_dest}"
                    )));
                }
                let existed = dest.exists();
                atomic_write_confined_file(&canonical_root, &dest, bytes)?;
                if !existed {
                    newly_written.push(dest);
                }
            }
            Ok(())
        })();

        if let Err(err) = write_files_res {
            for path in newly_written.into_iter().rev() {
                let _ = fs::remove_file(path);
            }
            return Err(err);
        }

        let upsert_res = self.upsert_walk(&UpsertWalkInput {
            identity: identity.clone(),
            flow_path: plan.flow_path,
            flow_sha256: plan.flow_sha256,
            subflow_files: Some(plan.subflow_files),
            legacy_closure: false,
            closure_sha256: None,
            validation: options.validation.clone(),
            exploration: options.exploration.clone(),
            repair_history: options.repair_history.clone(),
        });

        if upsert_res.is_err() {
            for path in newly_written.into_iter().rev() {
                let _ = fs::remove_file(path);
            }
        }
        upsert_res
    }

    /// List every stored walk in deterministic `id` order, verifying every row's `integrity_sha256`.
    pub fn list_walks(&self) -> Result<Vec<StoredWalk>, StoreError> {
        let mut guard = self.lock_inner()?;
        let Some(conn) = self.ensure_read_conn(&mut guard)? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare_cached(
            "SELECT id, goal, goal_key, app_id, platform, flow_schema_version, build_fingerprint,
                    adapter, browser, flow_path, flow_sha256, last_passed, last_at, origin,
                    exploration_json, has_files, legacy_closure, closure_sha256, integrity_sha256
             FROM walks ORDER BY id",
        )?;
        let rows: Vec<ValidatedWalkRow> = stmt
            .query_map([], read_walk_row)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(validate_walk_row_struct)
            .collect::<Result<Vec<_>, StoreError>>()?;

        let mut walks = Vec::with_capacity(rows.len());
        for walk in rows {
            let history = query_validation_history(conn, walk.id)?;
            let subflow_files = if walk.has_files {
                Some(query_walk_files(conn, walk.id)?)
            } else {
                None
            };
            let repairs = query_repair_history(conn, walk.id)?;
            walks.push(StoredWalk {
                goal: walk.goal,
                app_id: walk.app_id,
                platform: walk.platform,
                flow_schema_version: walk.flow_schema_version,
                build_fingerprint: walk.build_fingerprint,
                adapter: walk.adapter,
                browser: walk.browser,
                goal_key: walk.goal_key,
                flow_path: walk.flow_path,
                flow_sha256: walk.flow_sha256,
                subflow_files,
                legacy_closure: if walk.legacy_closure {
                    Some(true)
                } else {
                    None
                },
                closure_sha256: walk.closure_sha256,
                last_validation: WalkValidation {
                    passed: walk.last_passed,
                    at: walk.last_at,
                },
                validation_history: history,
                origin: walk.origin,
                exploration: walk.exploration,
                repair_history: if repairs.is_empty() {
                    None
                } else {
                    Some(repairs)
                },
            });
        }
        Ok(walks)
    }

    /// Look up the database `walk_id` for a logical walk identity (`O(1)` via `idx_walks_identity`).
    pub fn find_walk_id(&self, identity: &WalkIdentity) -> Result<Option<i64>, StoreError> {
        let norm = normalize_walk_identity(identity)?;
        let mut guard = self.lock_inner()?;
        let Some(conn) = self.ensure_read_conn(&mut guard)? else {
            return Ok(None);
        };
        Ok(query_walk_by_identity(conn, &norm)?.map(|w| w.id))
    }

    /// List all `validation_runs` rows recorded for a logical walk across builds and runtimes.
    pub fn list_validation_runs(
        &self,
        identity: &WalkIdentity,
    ) -> Result<Vec<ValidationRunRecord>, StoreError> {
        let norm = normalize_walk_identity(identity)?;
        let mut guard = self.lock_inner()?;
        let Some(conn) = self.ensure_read_conn(&mut guard)? else {
            return Ok(Vec::new());
        };
        let Some(walk) = query_walk_by_identity(conn, &norm)? else {
            return Ok(Vec::new());
        };
        query_all_runs_for_walk(conn, walk.id)
    }

    /// Look up `validation_runs` rows for one `(logical walk, build_fingerprint)` and optional `runtime`.
    ///
    /// When `runtime` is `Some(rt)`, returns at most one run for `runtime_key == rt`.
    /// When `runtime` is `None`, returns all runs recorded for that build across all runtimes.
    pub fn lookup_validation_runs_for_build(
        &self,
        identity: &WalkIdentity,
        runtime: Option<&str>,
    ) -> Result<Vec<ValidationRunRecord>, StoreError> {
        let norm = normalize_walk_identity(identity)?;
        let rt = validate_runtime(runtime)?;
        let mut guard = self.lock_inner()?;
        let Some(conn) = self.ensure_read_conn(&mut guard)? else {
            return Ok(Vec::new());
        };
        let Some(walk) = query_walk_by_identity(conn, &norm)? else {
            return Ok(Vec::new());
        };
        query_runs_for_walk_build(conn, walk.id, &norm.build_fingerprint, rt)
    }

    /// Store or update a provider embedding vector for `walk_id` as little-endian `f64` bytes.
    pub fn upsert_goal_embedding(
        &self,
        walk_id: i64,
        provider: &str,
        embedding: &[f64],
        updated_at: &str,
    ) -> Result<(), StoreError> {
        if !is_non_empty(provider) {
            return Err(StoreError::InvalidIdentity(
                "embedding provider must be non-empty".to_string(),
            ));
        }
        if !is_valid_iso8601(updated_at) {
            return Err(StoreError::InvalidTimestamp(updated_at.to_string()));
        }
        if embedding.is_empty()
            || embedding.len() > MAX_EMBEDDING_DIMENSIONS
            || embedding.iter().any(|v| !v.is_finite())
        {
            return Err(StoreError::InvalidIdentity(
                "embedding must be a non-empty finite vector within dimension limits".to_string(),
            ));
        }
        let mut blob = Vec::with_capacity(embedding.len() * 8);
        for &val in embedding {
            blob.extend_from_slice(&val.to_le_bytes());
        }
        let dimensions = embedding.len() as i64;
        let integrity =
            compute_goal_embedding_integrity(walk_id, provider, dimensions, &blob, updated_at);

        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT INTO goal_embeddings (
                    walk_id, provider, dimensions, embedding_blob, updated_at, integrity_sha256
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(walk_id, provider) DO UPDATE SET
                    dimensions = excluded.dimensions,
                    embedding_blob = excluded.embedding_blob,
                    updated_at = excluded.updated_at,
                    integrity_sha256 = excluded.integrity_sha256",
                params![walk_id, provider, dimensions, blob, updated_at, integrity],
            )?;
            Ok(())
        })
    }

    /// Read and integrity-verify a persisted goal embedding vector for `(walk_id, provider)`.
    pub fn get_goal_embedding(
        &self,
        walk_id: i64,
        provider: &str,
    ) -> Result<Option<GoalEmbeddingRecord>, StoreError> {
        let mut guard = self.lock_inner()?;
        let Some(conn) = self.ensure_read_conn(&mut guard)? else {
            return Ok(None);
        };
        let mut stmt = conn.prepare_cached(
            "SELECT dimensions, embedding_blob, updated_at, integrity_sha256
             FROM goal_embeddings WHERE walk_id = ?1 AND provider = ?2",
        )?;
        let Some((dimensions, blob, updated_at, stored_integrity)) = stmt
            .query_row(params![walk_id, provider], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .optional()?
        else {
            return Ok(None);
        };

        verify_goal_embedding_integrity(
            walk_id,
            provider,
            dimensions,
            &blob,
            &updated_at,
            &stored_integrity,
        )?;
        if !is_valid_iso8601(&updated_at) {
            return Err(StoreError::InvalidWalkMemory(
                "goal_embeddings updated_at is not a valid timestamp".to_string(),
            ));
        }
        let dim = usize::try_from(dimensions).map_err(|_| {
            StoreError::InvalidWalkMemory("goal_embeddings dimensions is negative".to_string())
        })?;
        if dim == 0 || dim > MAX_EMBEDDING_DIMENSIONS || blob.len() != dim * 8 {
            return Err(StoreError::InvalidWalkMemory(
                "goal_embeddings blob length does not match dimensions".to_string(),
            ));
        }
        let mut embedding = Vec::with_capacity(dim);
        for chunk in blob.as_chunks::<8>().0 {
            let val = f64::from_le_bytes(*chunk);
            if !val.is_finite() {
                return Err(StoreError::InvalidWalkMemory(
                    "goal_embeddings contains non-finite component".to_string(),
                ));
            }
            embedding.push(val);
        }

        Ok(Some(GoalEmbeddingRecord {
            walk_id,
            provider: provider.to_string(),
            embedding,
            updated_at,
        }))
    }

    /// Compute or reuse a persistent stat-verified SHA-256 build fingerprint (`DEJA-53`).
    ///
    /// On a cache hit, verifies every file's `(rel_posix, size, mtime_ns, ctime_sec, ctime_nsec,
    /// dev, ino)` against `build_fingerprint_cache` AND confirms `pre_stat == post_stat`, returning
    /// the cached SHA-256 with **0 file bytes read** (`cache_hit == true && bytes_read == 0`).
    /// On any stat mismatch, tamper, or miss, streams SHA-256 via `deja_core::{fingerprint_file,
    /// fingerprint_tree}` and updates `build_fingerprint_cache` when opened writable.
    pub fn fingerprint_with_cache(
        &self,
        root: &Path,
        build_path: &Path,
        label: &str,
    ) -> Result<FingerprintCacheResult, StoreError> {
        let target = if build_path.is_absolute() {
            build_path.to_path_buf()
        } else {
            root.join(build_path)
        };
        let target_meta = fs::metadata(&target)?;
        let is_dir = target_meta.is_dir();
        let canonical = fs::canonicalize(&target)?;
        let canonical_str = canonical.to_string_lossy().into_owned();

        let pre_snapshot = collect_build_stat_snapshot(&canonical, is_dir, label)?;

        if let Some(ref pre) = pre_snapshot {
            let cached_digest = {
                let mut guard = self.lock_inner()?;
                if let Some(conn) = self.ensure_read_conn(&mut guard)? {
                    lookup_cached_fingerprint(conn, &canonical_str, pre)?
                } else {
                    None
                }
            };
            if let Some(digest) = cached_digest {
                let post_snapshot = collect_build_stat_snapshot(&canonical, is_dir, label)?;
                if post_snapshot.as_ref() == Some(pre) {
                    return Ok(FingerprintCacheResult {
                        digest,
                        cache_hit: true,
                        bytes_read: 0,
                    });
                }
            }
        }

        let digest = if is_dir {
            fingerprint_tree(root, &canonical, label)?
        } else {
            fingerprint_file(root, &canonical)?
        };
        let bytes_read = pre_snapshot
            .as_ref()
            .map(|snap| snap.total_bytes())
            .unwrap_or_else(|| target_meta.len());

        let post_snapshot = collect_build_stat_snapshot(&canonical, is_dir, label)?;
        if let (Some(pre), Some(post)) = (pre_snapshot, post_snapshot)
            && pre == post
        {
            let is_writable = {
                let guard = self.lock_inner()?;
                guard.mode == OpenMode::Writable
            };
            if is_writable {
                let updated_at = now_iso8601();
                if let Some(row) = build_cache_row(&canonical_str, &post, &digest, &updated_at) {
                    self.with_write_tx(|tx| {
                        tx.execute(
                            "INSERT INTO build_fingerprint_cache (
                                canonical_path, kind, size, mtime_ns, ino, dev, ctime_sec,
                                ctime_nsec, entries_json, digest_sha256, updated_at, integrity_sha256
                            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                            ON CONFLICT(canonical_path) DO UPDATE SET
                                kind = excluded.kind,
                                size = excluded.size,
                                mtime_ns = excluded.mtime_ns,
                                ino = excluded.ino,
                                dev = excluded.dev,
                                ctime_sec = excluded.ctime_sec,
                                ctime_nsec = excluded.ctime_nsec,
                                entries_json = excluded.entries_json,
                                digest_sha256 = excluded.digest_sha256,
                                updated_at = excluded.updated_at,
                                integrity_sha256 = excluded.integrity_sha256",
                            params![
                                row.canonical_path,
                                row.kind,
                                row.size,
                                row.mtime_ns,
                                row.ino,
                                row.dev,
                                row.ctime_sec,
                                row.ctime_nsec,
                                row.entries_json,
                                row.digest_sha256,
                                row.updated_at,
                                row.integrity_sha256,
                            ],
                        )?;
                        Ok(())
                    })?;
                }
            }
        }

        Ok(FingerprintCacheResult {
            digest,
            cache_hit: false,
            bytes_read,
        })
    }

    // -----------------------------------------------------------------------------------------
    // Crate-internal helpers used by `consumer_flows.rs` and `gate.rs`

    pub(crate) fn has_legacy_walk_for_goal(&self, goal_key: &str) -> Result<bool, StoreError> {
        let mut guard = self.lock_inner()?;
        let Some(conn) = self.ensure_read_conn(&mut guard)? else {
            return Ok(false);
        };
        let mut stmt = conn.prepare_cached(
            "SELECT 1 FROM walks WHERE goal_key = ?1 AND closure_sha256 IS NULL LIMIT 1",
        )?;
        Ok(stmt.query_row([goal_key], |_| Ok(())).optional()?.is_some())
    }

    pub(crate) fn fetch_logical_walk_and_runs(
        &self,
        identity: &WalkIdentity,
    ) -> Result<Option<(ValidatedWalkRow, Vec<ValidationRunRecord>)>, StoreError> {
        let norm = normalize_walk_identity(identity)?;
        let mut guard = self.lock_inner()?;
        let Some(conn) = self.ensure_read_conn(&mut guard)? else {
            return Ok(None);
        };
        let Some(walk) = query_walk_by_identity(conn, &norm)? else {
            return Ok(None);
        };
        let runs = query_all_runs_for_walk(conn, walk.id)?;
        Ok(Some((walk, runs)))
    }

    pub(crate) fn verify_classic_stored_flow(
        &self,
        walk: &ValidatedWalkRow,
        run: &ValidationRunRecord,
    ) -> Result<ClassicFlowCheck, StoreError> {
        verify_classic_stored_flow_inner(&self.workspace_root, walk, run)
    }
}

// ---------------------------------------------------------------------------------------------
// Internal SQLite Query & Verification Helpers

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

fn wal_sidecar_exists(db_path: &Path) -> bool {
    fs::symlink_metadata(append_suffix(db_path, "-wal")).is_ok()
}

fn read_db_file_stamp(path: &Path) -> Option<DbFileStamp> {
    let mut file = open_read_nofollow(path).ok()?;
    let meta = file.metadata().ok()?;
    let modified = meta.modified().ok()?;
    let mut header = [0u8; 28];
    let change_counter = if file.read_exact(&mut header).is_ok() {
        [header[24], header[25], header[26], header[27]]
    } else {
        [0u8; 4]
    };
    Some(DbFileStamp {
        len: meta.len(),
        modified,
        change_counter,
    })
}

fn encode_sqlite_uri_path(path: &Path) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let raw = path.to_string_lossy();
    let mut out = String::with_capacity(raw.len() + 32);
    out.push_str("file:");
    for &b in raw.as_bytes() {
        let keep =
            b == b'/' || b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b':');
        if keep {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out.push_str("?mode=ro&immutable=1");
    out
}

fn open_readonly_connection(
    path: &Path,
) -> Result<(Connection, bool, Option<DbFileStamp>), StoreError> {
    let use_immutable = !wal_sidecar_exists(path);
    let conn = if use_immutable {
        let uri = encode_sqlite_uri_path(path);
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        Connection::open_with_flags(&uri, flags).map_err(|err| {
            StoreError::InvalidWalkMemory(format!("walks.sqlite cannot be opened read-only: {err}"))
        })?
    } else {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        Connection::open_with_flags(path, flags).map_err(|err| {
            StoreError::InvalidWalkMemory(format!("walks.sqlite cannot be opened read-only: {err}"))
        })?
    };
    apply_readonly_pragmas(&conn).map_err(|err| {
        StoreError::InvalidWalkMemory(format!("walks.sqlite cannot be read: {err}"))
    })?;
    verify_readonly_schema(&conn)?;
    let stamp = if use_immutable {
        read_db_file_stamp(path)
    } else {
        None
    };
    Ok((conn, use_immutable, stamp))
}

fn miss(reason: CacheMissReason) -> LookupResult {
    LookupResult::Miss(LookupMiss {
        reason,
        flow_change: None,
        closure_sha256: None,
        flow_path: None,
    })
}

fn validate_validation_input(input: &RecordValidationInput) -> Result<(), StoreError> {
    if !is_valid_iso8601(&input.at) {
        return Err(StoreError::InvalidTimestamp(input.at.clone()));
    }
    let _ = validate_runtime(input.runtime.as_deref())?;
    Ok(())
}

struct RawWalkRow {
    id: i64,
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
    last_passed: i64,
    last_at: String,
    origin: Option<String>,
    exploration_json: Option<String>,
    has_files: i64,
    legacy_closure: i64,
    closure_sha256: Option<String>,
    integrity_sha256: String,
}

fn read_walk_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawWalkRow> {
    Ok(RawWalkRow {
        id: row.get(0)?,
        goal: row.get(1)?,
        goal_key: row.get(2)?,
        app_id: row.get(3)?,
        platform: row.get(4)?,
        flow_schema_version: row.get(5)?,
        build_fingerprint: row.get(6)?,
        adapter: row.get(7)?,
        browser: row.get(8)?,
        flow_path: row.get(9)?,
        flow_sha256: row.get(10)?,
        last_passed: row.get(11)?,
        last_at: row.get(12)?,
        origin: row.get(13)?,
        exploration_json: row.get(14)?,
        has_files: row.get(15)?,
        legacy_closure: row.get(16)?,
        closure_sha256: row.get(17)?,
        integrity_sha256: row.get(18)?,
    })
}

fn validate_walk_row_struct(raw: RawWalkRow) -> Result<ValidatedWalkRow, StoreError> {
    if !(raw.last_passed == 0 || raw.last_passed == 1)
        || !(raw.has_files == 0 || raw.has_files == 1)
        || !(raw.legacy_closure == 0 || raw.legacy_closure == 1)
    {
        return Err(StoreError::InvalidWalkMemory(
            "walk row has invalid boolean flag column".to_string(),
        ));
    }
    verify_walk_integrity(
        &WalkIntegrityFields {
            goal: &raw.goal,
            goal_key: &raw.goal_key,
            app_id: &raw.app_id,
            platform: &raw.platform,
            flow_schema_version: &raw.flow_schema_version,
            build_fingerprint: &raw.build_fingerprint,
            adapter: raw.adapter.as_deref(),
            browser: raw.browser.as_deref(),
            flow_path: &raw.flow_path,
            flow_sha256: &raw.flow_sha256,
            last_passed: raw.last_passed == 1,
            last_at: &raw.last_at,
            origin: raw.origin.as_deref(),
            exploration_json: raw.exploration_json.as_deref(),
            has_files: raw.has_files == 1,
            legacy_closure: raw.legacy_closure == 1,
            closure_sha256: raw.closure_sha256.as_deref(),
        },
        &raw.integrity_sha256,
    )?;

    let norm = normalize_goal(&raw.goal)
        .map_err(|_| StoreError::InvalidWalkMemory("walk row has empty goal".to_string()))?;
    if norm.key != raw.goal_key
        || !is_non_empty(&raw.app_id)
        || !is_non_empty(&raw.platform)
        || raw.flow_schema_version != DEFAULT_FLOW_SCHEMA_VERSION
        || !is_sha256_hex(&raw.build_fingerprint)
        || !is_non_empty(&raw.flow_path)
        || !is_sha256_hex(&raw.flow_sha256)
        || !is_valid_iso8601(&raw.last_at)
    {
        return Err(StoreError::InvalidWalkMemory(
            "walk row has missing or malformed identity/checksum fields".to_string(),
        ));
    }
    if raw.adapter.as_deref().is_some_and(|a| !is_non_empty(a))
        || raw.browser.as_deref().is_some_and(|b| !is_non_empty(b))
        || (raw.platform == "web" && (raw.adapter.is_none() || raw.browser.is_none()))
    {
        return Err(StoreError::InvalidWalkMemory(
            "walk adapter/browser identity is malformed".to_string(),
        ));
    }
    if let Some(ref csha) = raw.closure_sha256 {
        if !is_sha256_hex(csha) || raw.has_files != 0 || raw.legacy_closure != 0 {
            return Err(StoreError::InvalidWalkMemory(
                "walk closureSha256 metadata is invalid".to_string(),
            ));
        }
        check_manifest_flow_path(&raw.flow_path, "flowPath").map_err(|err| {
            StoreError::InvalidWalkMemory(format!("walk closure flowPath is invalid: {err}"))
        })?;
    }

    let origin = raw.origin.as_deref().map(parse_origin_str).transpose()?;
    let exploration = raw
        .exploration_json
        .as_deref()
        .map(validate_exploration)
        .transpose()?;

    Ok(ValidatedWalkRow {
        id: raw.id,
        goal: raw.goal,
        goal_key: raw.goal_key,
        app_id: raw.app_id,
        platform: raw.platform,
        flow_schema_version: raw.flow_schema_version,
        build_fingerprint: raw.build_fingerprint,
        adapter: raw.adapter,
        browser: raw.browser,
        flow_path: raw.flow_path,
        flow_sha256: raw.flow_sha256,
        last_passed: raw.last_passed == 1,
        last_at: raw.last_at,
        origin,
        exploration,
        has_files: raw.has_files == 1,
        legacy_closure: raw.legacy_closure == 1,
        closure_sha256: raw.closure_sha256,
    })
}

fn query_walk_by_id(conn: &Connection, walk_id: i64) -> Result<ValidatedWalkRow, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, goal, goal_key, app_id, platform, flow_schema_version, build_fingerprint,
                adapter, browser, flow_path, flow_sha256, last_passed, last_at, origin,
                exploration_json, has_files, legacy_closure, closure_sha256, integrity_sha256
         FROM walks WHERE id = ?1",
    )?;
    let raw = stmt.query_row([walk_id], read_walk_row)?;
    validate_walk_row_struct(raw)
}

fn query_walk_by_identity(
    conn: &Connection,
    norm: &NormalizedWalkIdentity,
) -> Result<Option<ValidatedWalkRow>, StoreError> {
    let adapter_key = norm.adapter.as_deref().unwrap_or("");
    let browser_key = norm.browser.as_deref().unwrap_or("");
    let mut stmt = conn.prepare_cached(
        "SELECT id, goal, goal_key, app_id, platform, flow_schema_version, build_fingerprint,
                adapter, browser, flow_path, flow_sha256, last_passed, last_at, origin,
                exploration_json, has_files, legacy_closure, closure_sha256, integrity_sha256
         FROM walks
         WHERE goal_key = ?1
           AND app_id = ?2
           AND platform = ?3
           AND flow_schema_version = ?4
           AND adapter_key = ?5
           AND browser_key = ?6",
    )?;
    let Some(raw) = stmt
        .query_row(
            params![
                norm.goal_key,
                norm.app_id,
                norm.platform,
                norm.flow_schema_version,
                adapter_key,
                browser_key,
            ],
            read_walk_row,
        )
        .optional()?
    else {
        return Ok(None);
    };
    Ok(Some(validate_walk_row_struct(raw)?))
}

fn classify_identity_miss(
    conn: &Connection,
    norm: &NormalizedWalkIdentity,
) -> Result<CacheMissReason, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT app_id, platform, flow_schema_version, adapter_key, browser_key
         FROM walks WHERE goal_key = ?1",
    )?;
    let rows: Vec<(String, String, String, String, String)> = stmt
        .query_map([&norm.goal_key], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.is_empty() {
        return Ok(CacheMissReason::NotFound);
    }
    let same_app: Vec<_> = rows.into_iter().filter(|r| r.0 == norm.app_id).collect();
    if same_app.is_empty() {
        return Ok(CacheMissReason::WrongApp);
    }
    let same_platform: Vec<_> = same_app
        .into_iter()
        .filter(|r| r.1 == norm.platform)
        .collect();
    if same_platform.is_empty() {
        return Ok(CacheMissReason::WrongPlatform);
    }
    let same_schema: Vec<_> = same_platform
        .into_iter()
        .filter(|r| r.2 == norm.flow_schema_version)
        .collect();
    if same_schema.is_empty() {
        return Ok(CacheMissReason::WrongFlowSchema);
    }
    let adapter_key = norm.adapter.as_deref().unwrap_or("");
    let same_adapter: Vec<_> = same_schema
        .into_iter()
        .filter(|r| r.3 == adapter_key)
        .collect();
    if same_adapter.is_empty() {
        return Ok(CacheMissReason::WrongAdapter);
    }
    Ok(CacheMissReason::WrongBrowser)
}

struct RawValidationRunRow {
    walk_id: i64,
    build_fingerprint: String,
    runtime: Option<String>,
    flow_path: String,
    flow_sha256: String,
    closure_sha256: Option<String>,
    subflow_files_json: Option<String>,
    passed: i64,
    flaky: i64,
    validated_at: String,
    origin: Option<String>,
    failure_category: Option<String>,
    report_path: Option<String>,
    duration_ms: Option<i64>,
    phase_timings_json: Option<String>,
    integrity_sha256: String,
}

fn read_validation_run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawValidationRunRow> {
    Ok(RawValidationRunRow {
        walk_id: row.get(0)?,
        build_fingerprint: row.get(1)?,
        runtime: row.get(2)?,
        flow_path: row.get(3)?,
        flow_sha256: row.get(4)?,
        closure_sha256: row.get(5)?,
        subflow_files_json: row.get(6)?,
        passed: row.get(7)?,
        flaky: row.get(8)?,
        validated_at: row.get(9)?,
        origin: row.get(10)?,
        failure_category: row.get(11)?,
        report_path: row.get(12)?,
        duration_ms: row.get(13)?,
        phase_timings_json: row.get(14)?,
        integrity_sha256: row.get(15)?,
    })
}

fn validate_run_row_struct(raw: RawValidationRunRow) -> Result<ValidationRunRecord, StoreError> {
    if !(raw.passed == 0 || raw.passed == 1) || !(raw.flaky == 0 || raw.flaky == 1) {
        return Err(StoreError::InvalidWalkMemory(
            "validation_runs row has invalid passed/flaky flag".to_string(),
        ));
    }
    verify_validation_run_integrity(
        &ValidationRunIntegrityFields {
            walk_id: raw.walk_id,
            build_fingerprint: &raw.build_fingerprint,
            runtime: raw.runtime.as_deref(),
            flow_path: &raw.flow_path,
            flow_sha256: &raw.flow_sha256,
            closure_sha256: raw.closure_sha256.as_deref(),
            subflow_files_json: raw.subflow_files_json.as_deref(),
            passed: raw.passed == 1,
            flaky: raw.flaky == 1,
            validated_at: &raw.validated_at,
            origin: raw.origin.as_deref(),
            failure_category: raw.failure_category.as_deref(),
            report_path: raw.report_path.as_deref(),
            duration_ms: raw.duration_ms,
            phase_timings_json: raw.phase_timings_json.as_deref(),
        },
        &raw.integrity_sha256,
    )?;

    if !is_sha256_hex(&raw.build_fingerprint)
        || !is_non_empty(&raw.flow_path)
        || !is_sha256_hex(&raw.flow_sha256)
        || !is_valid_iso8601(&raw.validated_at)
    {
        return Err(StoreError::InvalidWalkMemory(
            "validation_runs row has malformed build/flow/timestamp fields".to_string(),
        ));
    }
    if let Some(ref csha) = raw.closure_sha256
        && !is_sha256_hex(csha)
    {
        return Err(StoreError::InvalidWalkMemory(
            "validation_runs closure_sha256 is invalid".to_string(),
        ));
    }
    let origin = raw.origin.as_deref().map(parse_origin_str).transpose()?;
    let duration_ms = raw
        .duration_ms
        .map(|v| {
            u64::try_from(v).map_err(|_| {
                StoreError::InvalidWalkMemory(
                    "validation_runs duration_ms must be non-negative".to_string(),
                )
            })
        })
        .transpose()?;
    let subflow_files = match raw.subflow_files_json {
        Some(ref text) => {
            let val: serde_json::Value = serde_json::from_str(text).map_err(|err| {
                StoreError::InvalidWalkMemory(format!(
                    "validation_runs subflow_files_json is invalid JSON: {err}"
                ))
            })?;
            Some(read_stored_flow_files(&val).ok_or_else(|| {
                StoreError::InvalidWalkMemory(
                    "validation_runs subflow_files_json contains unsafe entries".to_string(),
                )
            })?)
        }
        None => None,
    };

    Ok(ValidationRunRecord {
        walk_id: raw.walk_id,
        build_fingerprint: raw.build_fingerprint,
        runtime: raw.runtime,
        flow_path: raw.flow_path,
        flow_sha256: raw.flow_sha256,
        closure_sha256: raw.closure_sha256,
        subflow_files,
        passed: raw.passed == 1,
        flaky: raw.flaky == 1,
        validated_at: raw.validated_at,
        origin,
        failure_category: raw.failure_category,
        report_path: raw.report_path,
        duration_ms,
        phase_timings_json: raw.phase_timings_json,
    })
}

fn query_validation_run(
    conn: &Connection,
    walk_id: i64,
    build_fingerprint: &str,
    runtime_key: &str,
) -> Result<Option<ValidationRunRecord>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT walk_id, build_fingerprint, runtime, flow_path, flow_sha256, closure_sha256,
                subflow_files_json, passed, flaky, validated_at, origin, failure_category,
                report_path, duration_ms, phase_timings_json, integrity_sha256
         FROM validation_runs
         WHERE walk_id = ?1 AND build_fingerprint = ?2 AND runtime_key = ?3",
    )?;
    let Some(raw) = stmt
        .query_row(
            params![walk_id, build_fingerprint, runtime_key],
            read_validation_run_row,
        )
        .optional()?
    else {
        return Ok(None);
    };
    Ok(Some(validate_run_row_struct(raw)?))
}

fn query_runs_for_walk_build(
    conn: &Connection,
    walk_id: i64,
    build_fingerprint: &str,
    runtime: Option<&str>,
) -> Result<Vec<ValidationRunRecord>, StoreError> {
    if let Some(rt) = runtime {
        return Ok(query_validation_run(conn, walk_id, build_fingerprint, rt)?
            .into_iter()
            .collect());
    }
    let mut stmt = conn.prepare_cached(
        "SELECT walk_id, build_fingerprint, runtime, flow_path, flow_sha256, closure_sha256,
                subflow_files_json, passed, flaky, validated_at, origin, failure_category,
                report_path, duration_ms, phase_timings_json, integrity_sha256
         FROM validation_runs
         WHERE walk_id = ?1 AND build_fingerprint = ?2
         ORDER BY id",
    )?;
    stmt.query_map(params![walk_id, build_fingerprint], read_validation_run_row)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(validate_run_row_struct)
        .collect()
}

fn query_all_runs_for_walk(
    conn: &Connection,
    walk_id: i64,
) -> Result<Vec<ValidationRunRecord>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT walk_id, build_fingerprint, runtime, flow_path, flow_sha256, closure_sha256,
                subflow_files_json, passed, flaky, validated_at, origin, failure_category,
                report_path, duration_ms, phase_timings_json, integrity_sha256
         FROM validation_runs
         WHERE walk_id = ?1
         ORDER BY id",
    )?;
    stmt.query_map([walk_id], read_validation_run_row)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(validate_run_row_struct)
        .collect()
}

fn query_latest_passing_run(
    conn: &Connection,
    walk_id: i64,
    runtime: Option<&str>,
) -> Result<Option<ValidationRunRecord>, StoreError> {
    let raw = if let Some(rt) = runtime {
        let mut stmt = conn.prepare_cached(
            "SELECT walk_id, build_fingerprint, runtime, flow_path, flow_sha256, closure_sha256,
                    subflow_files_json, passed, flaky, validated_at, origin, failure_category,
                    report_path, duration_ms, phase_timings_json, integrity_sha256
             FROM validation_runs
             WHERE walk_id = ?1 AND passed = 1 AND runtime_key = ?2
             ORDER BY validated_at DESC, id DESC
             LIMIT 1",
        )?;
        stmt.query_row(params![walk_id, rt], read_validation_run_row)
            .optional()?
    } else {
        let mut stmt = conn.prepare_cached(
            "SELECT walk_id, build_fingerprint, runtime, flow_path, flow_sha256, closure_sha256,
                    subflow_files_json, passed, flaky, validated_at, origin, failure_category,
                    report_path, duration_ms, phase_timings_json, integrity_sha256
             FROM validation_runs
             WHERE walk_id = ?1 AND passed = 1
             ORDER BY validated_at DESC, id DESC
             LIMIT 1",
        )?;
        stmt.query_row([walk_id], read_validation_run_row)
            .optional()?
    };
    raw.map(validate_run_row_struct).transpose()
}

fn query_validation_history(
    conn: &Connection,
    walk_id: i64,
) -> Result<Vec<StoredValidation>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT position, passed, at, flow_sha256, build_fingerprint, runtime, flaky, integrity_sha256
         FROM validation_history
         WHERE walk_id = ?1
         ORDER BY position",
    )?;
    let rows = stmt
        .query_map([walk_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, String>(7)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut out = Vec::with_capacity(rows.len());
    for (position, passed_i, at, flow_sha256, build_fp, runtime, flaky_i, stored_integrity) in rows
    {
        if !(passed_i == 0 || passed_i == 1) || !(flaky_i == 0 || flaky_i == 1) {
            return Err(StoreError::InvalidWalkMemory(
                "validation_history row has invalid boolean flag".to_string(),
            ));
        }
        verify_validation_history_integrity(
            &ValidationHistoryIntegrityFields {
                walk_id,
                position,
                passed: passed_i == 1,
                at: &at,
                flow_sha256: &flow_sha256,
                build_fingerprint: build_fp.as_deref(),
                runtime: runtime.as_deref(),
                flaky: flaky_i == 1,
            },
            &stored_integrity,
        )?;
        if !is_valid_iso8601(&at) || !is_sha256_hex(&flow_sha256) {
            return Err(StoreError::InvalidWalkMemory(
                "validation_history row has malformed timestamp or flow_sha256".to_string(),
            ));
        }
        out.push(StoredValidation {
            passed: passed_i == 1,
            at,
            flow_sha256,
        });
    }
    if out.is_empty() {
        return Err(StoreError::InvalidWalkMemory(
            "walk validation_history must not be empty".to_string(),
        ));
    }
    Ok(out)
}

fn query_walk_files(conn: &Connection, walk_id: i64) -> Result<Vec<StoredFlowFile>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT position, path, sha256, integrity_sha256
         FROM walk_files
         WHERE walk_id = ?1
         ORDER BY position",
    )?;
    let rows = stmt
        .query_map([walk_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut raw_values = Vec::with_capacity(rows.len());
    for (position, path, sha256, stored_integrity) in rows {
        verify_walk_file_integrity(walk_id, position, &path, &sha256, &stored_integrity)?;
        raw_values.push(serde_json::json!({ "path": path, "sha256": sha256 }));
    }
    read_stored_flow_files(&serde_json::Value::Array(raw_values)).ok_or_else(|| {
        StoreError::InvalidWalkMemory("walk_files contains invalid subflow entry".to_string())
    })
}

fn query_repair_history(
    conn: &Connection,
    walk_id: i64,
) -> Result<Vec<RepairProvenance>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT position, attempt_id, patch_path, previous_build_fingerprint, previous_flow_sha256, integrity_sha256
         FROM repair_history
         WHERE walk_id = ?1
         ORDER BY position",
    )?;
    let rows = stmt
        .query_map([walk_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut out = Vec::with_capacity(rows.len());
    for (position, attempt_id, patch_path, prev_build, prev_sha, stored_integrity) in rows {
        verify_repair_history_integrity(
            walk_id,
            position,
            &attempt_id,
            &patch_path,
            &prev_build,
            &prev_sha,
            &stored_integrity,
        )?;
        let item = RepairProvenance {
            attempt_id,
            patch_path,
            previous_build_fingerprint: prev_build,
            previous_flow_sha256: prev_sha,
        };
        validate_repair_provenance(&item)?;
        out.push(item);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn append_validation_history(
    tx: &Transaction<'_>,
    walk_id: i64,
    passed: bool,
    at: &str,
    flow_sha256: &str,
    build_fingerprint: Option<&str>,
    runtime: Option<&str>,
    flaky: bool,
) -> Result<(), StoreError> {
    let next_pos: i64 = tx.query_row(
        "SELECT COALESCE(MAX(position) + 1, 0) FROM validation_history WHERE walk_id = ?1",
        [walk_id],
        |r| r.get(0),
    )?;
    let integrity = compute_validation_history_integrity(&ValidationHistoryIntegrityFields {
        walk_id,
        position: next_pos,
        passed,
        at,
        flow_sha256,
        build_fingerprint,
        runtime,
        flaky,
    });
    tx.execute(
        "INSERT INTO validation_history (
            walk_id, position, passed, at, flow_sha256, build_fingerprint, runtime, flaky, integrity_sha256
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            walk_id,
            next_pos,
            i64::from(passed),
            at,
            flow_sha256,
            build_fingerprint,
            runtime,
            i64::from(flaky),
            integrity,
        ],
    )?;

    // Bound validation_history to the latest MAX_HISTORY_ENTRIES rows per walk.
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM validation_history WHERE walk_id = ?1",
        [walk_id],
        |r| r.get(0),
    )?;
    if count > MAX_HISTORY_ENTRIES {
        let excess = count - MAX_HISTORY_ENTRIES;
        tx.execute(
            "DELETE FROM validation_history
             WHERE walk_id = ?1 AND position IN (
                 SELECT position FROM validation_history
                 WHERE walk_id = ?1 ORDER BY position ASC LIMIT ?2
             )",
            params![walk_id, excess],
        )?;
    }
    Ok(())
}

fn hydrate_stored_walk(
    conn: &Connection,
    walk: &ValidatedWalkRow,
    run: &ValidationRunRecord,
) -> Result<StoredWalk, StoreError> {
    let history = query_validation_history(conn, walk.id)?;
    let subflow_files = if run.closure_sha256.is_some() {
        None
    } else if let Some(ref sf) = run.subflow_files {
        Some(sf.clone())
    } else if walk.has_files {
        Some(query_walk_files(conn, walk.id)?)
    } else {
        None
    };
    let repairs = query_repair_history(conn, walk.id)?;
    Ok(StoredWalk {
        goal: walk.goal.clone(),
        app_id: walk.app_id.clone(),
        platform: walk.platform.clone(),
        flow_schema_version: walk.flow_schema_version.clone(),
        build_fingerprint: run.build_fingerprint.clone(),
        adapter: walk.adapter.clone(),
        browser: walk.browser.clone(),
        goal_key: walk.goal_key.clone(),
        flow_path: run.flow_path.clone(),
        flow_sha256: run.flow_sha256.clone(),
        subflow_files,
        legacy_closure: if walk.legacy_closure {
            Some(true)
        } else {
            None
        },
        closure_sha256: run.closure_sha256.clone(),
        last_validation: WalkValidation {
            passed: run.passed,
            at: run.validated_at.clone(),
        },
        validation_history: history,
        origin: run.origin.or(walk.origin),
        exploration: walk.exploration.clone(),
        repair_history: if repairs.is_empty() {
            None
        } else {
            Some(repairs)
        },
    })
}

fn verify_classic_stored_flow_inner(
    workspace_root: &Path,
    walk: &ValidatedWalkRow,
    run: &ValidationRunRecord,
) -> Result<ClassicFlowCheck, StoreError> {
    if let Some(ref expected_closure_sha) = run.closure_sha256 {
        // Consumer-owned flow: verify workspace confinement, non-symlink regular entry file,
        // and the FULL transitive closure checksum (`collect_flow_closure`) so modified or deleted
        // subflows/scripts/media never pass a gate or lookup!
        let canonical_root = fs::canonicalize(workspace_root)?;
        let candidate = lexical_normalize(&canonical_root.join(&run.flow_path));
        if Path::new(&run.flow_path).is_absolute() || !path_inside(&canonical_root, &candidate) {
            return Ok(ClassicFlowCheck::Invalid(format!(
                "consumer flow escapes workspace: {}",
                run.flow_path
            )));
        }
        let lmeta = match fs::symlink_metadata(&candidate) {
            Ok(m) => m,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(ClassicFlowCheck::Missing),
            Err(err) => {
                return Ok(ClassicFlowCheck::Invalid(format!(
                    "consumer flow cannot be inspected: {err}"
                )));
            }
        };
        if lmeta.file_type().is_symlink() || !lmeta.file_type().is_file() {
            return Ok(ClassicFlowCheck::Invalid(format!(
                "consumer flow must be a regular file (not a symlink): {}",
                run.flow_path
            )));
        }
        let canonical = fs::canonicalize(&candidate)?;
        if !path_inside(&canonical_root, &canonical) {
            return Ok(ClassicFlowCheck::Invalid(format!(
                "consumer flow escapes workspace: {}",
                run.flow_path
            )));
        }
        let closure = match collect_flow_closure(&canonical_root, &canonical) {
            Ok(c) => c,
            Err(CoreError::MissingFlowFile { path }) => {
                return Ok(ClassicFlowCheck::FlowChanged {
                    detail: format!("a file referenced by {} is missing: {path}", run.flow_path),
                    expected_closure_sha256: expected_closure_sha.clone(),
                    current_closure_sha256: None,
                });
            }
            Err(err) => {
                return Ok(ClassicFlowCheck::Invalid(format!(
                    "consumer flow closure cannot be verified: {err}"
                )));
            }
        };
        if &closure.sha256 != expected_closure_sha {
            return Ok(ClassicFlowCheck::FlowChanged {
                detail: format!(
                    "the flow or a file it runs changed (expected {}, now {})",
                    expected_closure_sha, closure.sha256
                ),
                expected_closure_sha256: expected_closure_sha.clone(),
                current_closure_sha256: Some(closure.sha256),
            });
        }
        let main_sha = closure
            .files
            .iter()
            .find(|f| f.path == closure.main)
            .map(|f| f.sha256.as_str());
        if main_sha != Some(run.flow_sha256.as_str()) {
            return Ok(ClassicFlowCheck::FlowChanged {
                detail: format!("stored flow checksum does not match {}", run.flow_path),
                expected_closure_sha256: expected_closure_sha.clone(),
                current_closure_sha256: Some(closure.sha256),
            });
        }
        return Ok(ClassicFlowCheck::Ok(canonical));
    }

    let canonical_root = fs::canonicalize(workspace_root)?;
    let flow_root = lexical_normalize(&canonical_root.join(".deja/flows"));
    if Path::new(&run.flow_path).is_absolute() || run.flow_path.contains('\\') {
        return Ok(ClassicFlowCheck::Invalid(format!(
            "stored flow must stay under .deja/flows: {}",
            run.flow_path
        )));
    }
    let candidate = lexical_normalize(&canonical_root.join(&run.flow_path));
    if !path_inside(&flow_root, &candidate) {
        return Ok(ClassicFlowCheck::Invalid(format!(
            "stored flow must stay under .deja/flows: {}",
            run.flow_path
        )));
    }
    let lmeta = match fs::symlink_metadata(&candidate) {
        Ok(meta) => meta,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(ClassicFlowCheck::Missing),
        Err(err) => {
            return Ok(ClassicFlowCheck::Invalid(format!(
                "stored flow cannot be inspected: {}: {err}",
                run.flow_path
            )));
        }
    };
    if lmeta.file_type().is_symlink() || !lmeta.file_type().is_file() {
        return Ok(ClassicFlowCheck::Invalid(format!(
            "stored flow must be a regular file (not a symlink): {}",
            run.flow_path
        )));
    }
    let canonical = match fs::canonicalize(&candidate) {
        Ok(p) => p,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(ClassicFlowCheck::Missing),
        Err(err) => {
            return Ok(ClassicFlowCheck::Invalid(format!(
                "stored flow cannot be resolved: {}: {err}",
                run.flow_path
            )));
        }
    };
    let canonical_flow_root = fs::canonicalize(&flow_root).map_err(|err| {
        StoreError::InvalidWalkMemory(format!(".deja/flows cannot be resolved: {err}"))
    })?;
    if !path_inside(&canonical_root, &canonical_flow_root)
        || !path_inside(&canonical_flow_root, &canonical)
    {
        return Ok(ClassicFlowCheck::Invalid(format!(
            "canonical stored flow escapes .deja/flows: {}",
            run.flow_path
        )));
    }

    let actual_sha = stream_sha256_nofollow(&canonical)?;
    if actual_sha != run.flow_sha256 {
        return Ok(ClassicFlowCheck::Invalid(format!(
            "stored flow checksum does not match {}",
            run.flow_path
        )));
    }

    let is_legacy = walk.legacy_closure || run.subflow_files.is_none();
    let subflows = if is_legacy {
        None
    } else {
        run.subflow_files.as_deref()
    };
    match check_stored_subflows(&canonical_root, &run.flow_path, subflows, is_legacy)? {
        StoredClosureCheck::Ok => Ok(ClassicFlowCheck::Ok(canonical)),
        StoredClosureCheck::Legacy => Ok(ClassicFlowCheck::Legacy),
        StoredClosureCheck::Missing { .. } => Ok(ClassicFlowCheck::Missing),
        StoredClosureCheck::Invalid { message } => Ok(ClassicFlowCheck::Invalid(message)),
    }
}

fn stream_sha256_nofollow(path: &Path) -> Result<String, StoreError> {
    let mut file = open_read_nofollow(path).map_err(|err| {
        StoreError::InvalidWalkMemory(format!(
            "cannot open stored flow safely ({}): {err}",
            path.display()
        ))
    })?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; STREAM_CHUNK_BYTES];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn atomic_write_confined_file(
    canonical_root: &Path,
    dest: &Path,
    bytes: &[u8],
) -> Result<(), StoreError> {
    let parent = dest.parent().ok_or_else(|| {
        StoreError::InvalidWalkMemory(format!("file has no parent directory: {}", dest.display()))
    })?;
    fs::create_dir_all(parent)?;
    let canonical_parent = fs::canonicalize(parent)?;
    if canonical_parent != canonical_root && !path_inside(canonical_root, &canonical_parent) {
        return Err(StoreError::InvalidWalkMemory(format!(
            "file parent escapes workspace: {}",
            dest.display()
        )));
    }
    if let Ok(lmeta) = fs::symlink_metadata(dest)
        && lmeta.file_type().is_symlink()
    {
        return Err(StoreError::InvalidWalkMemory(format!(
            "refusing to overwrite symbolic link: {}",
            dest.display()
        )));
    }
    let tmp = append_suffix(
        dest,
        &format!(".{}.{}.tmp", std::process::id(), generate_token()),
    );
    let write_res = (|| -> Result<(), StoreError> {
        let mut f = create_new_nofollow(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = fs::rename(&tmp, dest) {
        let _ = fs::remove_file(&tmp);
        return Err(StoreError::Io(err));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Persistent Build Fingerprint Cache Helpers (`build_fingerprint_cache`)

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedEntryStat {
    rel_posix: String,
    size: u64,
    mtime_ns: i128,
    ctime_sec: i64,
    ctime_nsec: i64,
    dev: u64,
    ino: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BuildStatSnapshot {
    File(CachedEntryStat),
    Tree {
        root_stat: CachedEntryStat,
        entries: Vec<CachedEntryStat>,
    },
}

impl BuildStatSnapshot {
    fn kind_str(&self) -> &'static str {
        match self {
            Self::File(_) => "file",
            Self::Tree { .. } => "tree",
        }
    }

    fn total_bytes(&self) -> u64 {
        match self {
            Self::File(s) => s.size,
            Self::Tree { entries, .. } => entries
                .iter()
                .fold(0u64, |acc, item| acc.saturating_add(item.size)),
        }
    }
}

struct BuildCacheRowToWrite {
    canonical_path: String,
    kind: String,
    size: i64,
    mtime_ns: i64,
    ino: i64,
    dev: i64,
    ctime_sec: i64,
    ctime_nsec: i64,
    entries_json: Option<String>,
    digest_sha256: String,
    updated_at: String,
    integrity_sha256: String,
}

#[cfg(unix)]
fn extract_stat(rel_posix: String, meta: &Metadata) -> Option<CachedEntryStat> {
    use std::os::unix::fs::MetadataExt;
    let modified = meta.modified().ok()?;
    let mtime_ns = match modified.duration_since(UNIX_EPOCH) {
        Ok(dur) => i128::from(dur.as_secs()) * 1_000_000_000 + i128::from(dur.subsec_nanos()),
        Err(err) => {
            let dur = err.duration();
            -(i128::from(dur.as_secs()) * 1_000_000_000 + i128::from(dur.subsec_nanos()))
        }
    };
    Some(CachedEntryStat {
        rel_posix,
        size: meta.len(),
        mtime_ns,
        ctime_sec: meta.ctime(),
        ctime_nsec: meta.ctime_nsec(),
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

#[cfg(not(unix))]
fn extract_stat(_rel_posix: String, _meta: &Metadata) -> Option<CachedEntryStat> {
    None
}

fn u64_to_i64_bits(v: u64) -> i64 {
    i64::from_ne_bytes(v.to_ne_bytes())
}

fn validate_tree_name<'a>(name: &'a OsStr, label: &str) -> Result<&'a str, StoreError> {
    name.to_str().ok_or_else(|| {
        StoreError::Core(deja_core::CoreError::Fingerprint(format!(
            "{label} build tree contains a non-UTF-8 filename"
        )))
    })
}

fn walk_tree_entries(
    dir: &Path,
    prefix: &str,
    label: &str,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), StoreError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        let name_str = validate_tree_name(&name, label)?;
        let rel_posix = if prefix.is_empty() {
            name_str.to_string()
        } else {
            format!("{prefix}/{name_str}")
        };
        let full_path = entry.path();
        if file_type.is_dir() {
            walk_tree_entries(&full_path, &rel_posix, label, out)?;
        } else if file_type.is_file() {
            out.push((rel_posix, full_path));
        } else {
            return Err(StoreError::Core(deja_core::CoreError::Fingerprint(
                format!("{label} build tree cannot contain symbolic links or special files"),
            )));
        }
    }
    Ok(())
}

fn collect_build_stat_snapshot(
    canonical: &Path,
    is_dir: bool,
    label: &str,
) -> Result<Option<BuildStatSnapshot>, StoreError> {
    if !is_dir {
        let meta = fs::metadata(canonical)?;
        if !meta.is_file() {
            return Err(StoreError::Core(deja_core::CoreError::Fingerprint(
                format!("{label} build artifact is not a regular file"),
            )));
        }
        return Ok(extract_stat(String::new(), &meta).map(BuildStatSnapshot::File));
    }

    let root_meta = fs::symlink_metadata(canonical)?;
    let Some(root_stat) = extract_stat(String::new(), &root_meta) else {
        return Ok(None);
    };
    let mut files = Vec::new();
    walk_tree_entries(canonical, "", label, &mut files)?;
    if files.is_empty() {
        return Err(StoreError::Core(deja_core::CoreError::Fingerprint(
            format!("{label} build tree must contain at least one file"),
        )));
    }
    files.sort_by(|left, right| cmp_utf16(&left.0, &right.0));

    let mut entries = Vec::with_capacity(files.len());
    for (rel_posix, full_path) in files {
        let meta = fs::symlink_metadata(&full_path)?;
        if !meta.file_type().is_file() {
            return Err(StoreError::Core(deja_core::CoreError::Fingerprint(
                format!("{label} build tree cannot contain symbolic links or special files"),
            )));
        }
        let Some(stat) = extract_stat(rel_posix, &meta) else {
            return Ok(None);
        };
        entries.push(stat);
    }
    Ok(Some(BuildStatSnapshot::Tree { root_stat, entries }))
}

fn build_cache_row(
    canonical_path: &str,
    snapshot: &BuildStatSnapshot,
    digest_sha256: &str,
    updated_at: &str,
) -> Option<BuildCacheRowToWrite> {
    let (kind, size, mtime_ns, ino, dev, ctime_sec, ctime_nsec, entries_json) = match snapshot {
        BuildStatSnapshot::File(stat) => (
            "file".to_string(),
            i64::try_from(stat.size).ok()?,
            i64::try_from(stat.mtime_ns).ok()?,
            u64_to_i64_bits(stat.ino),
            u64_to_i64_bits(stat.dev),
            stat.ctime_sec,
            stat.ctime_nsec,
            None,
        ),
        BuildStatSnapshot::Tree { root_stat, entries } => {
            let total_size = entries
                .iter()
                .try_fold(0u64, |acc, e| acc.checked_add(e.size))?;
            let max_mtime = entries
                .iter()
                .map(|e| e.mtime_ns)
                .max()
                .unwrap_or(root_stat.mtime_ns);
            let json_str = serde_json::to_string(entries).ok()?;
            (
                "tree".to_string(),
                i64::try_from(total_size).ok()?,
                i64::try_from(max_mtime).ok()?,
                u64_to_i64_bits(root_stat.ino),
                u64_to_i64_bits(root_stat.dev),
                root_stat.ctime_sec,
                root_stat.ctime_nsec,
                Some(json_str),
            )
        }
    };

    let integrity_sha256 =
        compute_build_fingerprint_cache_integrity(&BuildFingerprintCacheIntegrityFields {
            canonical_path,
            kind: &kind,
            size,
            mtime_ns,
            ino,
            dev,
            ctime_sec,
            ctime_nsec,
            entries_json: entries_json.as_deref(),
            digest_sha256,
            updated_at,
        });

    Some(BuildCacheRowToWrite {
        canonical_path: canonical_path.to_string(),
        kind,
        size,
        mtime_ns,
        ino,
        dev,
        ctime_sec,
        ctime_nsec,
        entries_json,
        digest_sha256: digest_sha256.to_string(),
        updated_at: updated_at.to_string(),
        integrity_sha256,
    })
}

fn lookup_cached_fingerprint(
    conn: &Connection,
    canonical_path: &str,
    snapshot: &BuildStatSnapshot,
) -> Result<Option<String>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT kind, size, mtime_ns, ino, dev, ctime_sec, ctime_nsec,
                entries_json, digest_sha256, updated_at, integrity_sha256
         FROM build_fingerprint_cache
         WHERE canonical_path = ?1",
    )?;
    let Some((
        kind,
        size,
        mtime_ns,
        ino,
        dev,
        ctime_sec,
        ctime_nsec,
        entries_json,
        digest_sha256,
        updated_at,
        stored_integrity,
    )) = stmt
        .query_row([canonical_path], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, String>(9)?,
                r.get::<_, String>(10)?,
            ))
        })
        .optional()?
    else {
        return Ok(None);
    };

    verify_build_fingerprint_cache_integrity(
        &BuildFingerprintCacheIntegrityFields {
            canonical_path,
            kind: &kind,
            size,
            mtime_ns,
            ino,
            dev,
            ctime_sec,
            ctime_nsec,
            entries_json: entries_json.as_deref(),
            digest_sha256: &digest_sha256,
            updated_at: &updated_at,
        },
        &stored_integrity,
    )?;

    if !is_sha256_hex(&digest_sha256) || !is_valid_iso8601(&updated_at) {
        return Err(StoreError::InvalidWalkMemory(
            "build_fingerprint_cache row has malformed digest or timestamp".to_string(),
        ));
    }

    if kind != snapshot.kind_str() {
        return Ok(None);
    }

    match snapshot {
        BuildStatSnapshot::File(stat) => {
            let Some(stat_size) = i64::try_from(stat.size).ok() else {
                return Ok(None);
            };
            let Some(stat_mtime) = i64::try_from(stat.mtime_ns).ok() else {
                return Ok(None);
            };
            if size == stat_size
                && mtime_ns == stat_mtime
                && ino == u64_to_i64_bits(stat.ino)
                && dev == u64_to_i64_bits(stat.dev)
                && ctime_sec == stat.ctime_sec
                && ctime_nsec == stat.ctime_nsec
                && entries_json.is_none()
            {
                Ok(Some(digest_sha256))
            } else {
                Ok(None)
            }
        }
        BuildStatSnapshot::Tree { root_stat, entries } => {
            let Some(ref raw_entries) = entries_json else {
                return Ok(None);
            };
            let cached_entries: Vec<CachedEntryStat> =
                serde_json::from_str(raw_entries).map_err(|err| {
                    StoreError::InvalidWalkMemory(format!(
                        "build_fingerprint_cache entries_json is malformed: {err}"
                    ))
                })?;
            if &cached_entries == entries
                && ino == u64_to_i64_bits(root_stat.ino)
                && dev == u64_to_i64_bits(root_stat.dev)
            {
                Ok(Some(digest_sha256))
            } else {
                Ok(None)
            }
        }
    }
}
