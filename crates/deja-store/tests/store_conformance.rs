//! Conformance, security, crash-atomicity, DEJA-51, multi-store gate, legacy migration,
//! and performance test suite for `deja-store`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use deja_core::{
    CacheMissReason, DEFAULT_FLOW_SCHEMA_VERSION, GateFlakyFailure, GateIdentity, GateOutcome,
    GoalEmbeddingProvider, LocalLexicalEmbeddingProvider, LookupResult, ManifestOwnerRules,
    ManifestWalk, RepairProvenance, RunOutcome, StaleBuildPolicy, StoredFlowFile, WalkIdentity,
    WalkManifest, WalkOrigin, assertions_sha256, collect_flow_closure, fingerprint_file,
    fingerprint_tree, serialize_walk_manifest,
};
use deja_store::{
    ConsumerFlows, ConsumerFlowsOptions, ConsumerRunPlan, ConsumerRunRequest,
    GATE_INVALID_EXIT_CODE, LEGACY_JSON_MEMORY_PATH, LockHolder, PersistVerifiedCandidateOptions,
    PostRunValidationOptions, RecordValidationInput, SQLITE_SCHEMA_VERSION,
    SaveManifestWalkOptions, SqliteStoreOptions, SqliteWalkStore, StoreError, StoreGateOptions,
    UpsertWalkInput, ValidationRunIntegrityFields, WriterLock, compute_validation_run_integrity,
    current_hostname, evaluate_gate, import_legacy_walks_json, inspect_lock_holder, machine_id,
    now_iso8601, parse_proc_start_ticks, parse_rfc3339_utc, process_start, store_gate_passed,
    store_metadata_path, validate_store_id,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

struct TempWorkspace {
    root: PathBuf,
}

impl TempWorkspace {
    fn new(prefix: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "deja-store-test-{prefix}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp workspace");
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn write_file(&self, rel: &str, contents: &[u8]) -> PathBuf {
        let dest = self.root.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(&dest, contents).expect("write test file");
        dest
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn fake_sha(seed: &str) -> String {
    sha256_hex(seed.as_bytes())
}

fn ios_identity(goal: &str, build_seed: &str) -> WalkIdentity {
    WalkIdentity {
        goal: goal.to_string(),
        app_id: "com.example.app".to_string(),
        platform: "ios".to_string(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint: fake_sha(build_seed),
        adapter: Some("ios-native".to_string()),
        browser: None,
    }
}

fn write_classic_flow(ws: &TempWorkspace, rel: &str, yaml: &str) -> (String, String) {
    ws.write_file(rel, yaml.as_bytes());
    (rel.to_string(), sha256_hex(yaml.as_bytes()))
}

fn spawn_and_reap_dead_pid() -> u32 {
    let mut child = Command::new("/usr/bin/true")
        .spawn()
        .or_else(|_| Command::new("true").spawn())
        .expect("spawn short-lived child");
    let pid = child.id();
    let _ = child.wait();
    pid
}

// =============================================================================================
// 1. WriterLock, Confinement, Symlink (O_NOFOLLOW), Stale Takeover & .break/.aside Race

#[test]
fn single_writer_lock_contention_release_and_verify_held() {
    let ws = TempWorkspace::new("single-writer");
    let store1 = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path()))
        .expect("first writer acquires lock");

    // Second writer is refused with StoreLocked (exit 5).
    let err = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path()))
        .expect_err("second writer must be refused");
    assert!(matches!(err, StoreError::StoreLocked { .. }));
    assert_eq!(err.exit_code(), 5);

    // Tampering/removing .lock while held stops subsequent writes via verify_held().
    let lock_path = fs::canonicalize(ws.path())
        .unwrap()
        .join(".deja/memory/walks.sqlite.lock");
    let holder = inspect_lock_holder(&lock_path)
        .unwrap()
        .expect("lock holder present");
    let mut hijacked = holder.clone();
    hijacked.token = "hijacked-by-someone-else".to_string();
    fs::write(
        &lock_path,
        format!("{}\n", serde_json::to_string(&hijacked).unwrap()),
    )
    .unwrap();

    let (flow_path, flow_sha) = write_classic_flow(
        &ws,
        ".deja/flows/login/ios-a.yaml",
        "appId: com.example.app\n---\n- assertVisible: Home\n",
    );
    let lost_err = store1
        .upsert_walk(&UpsertWalkInput {
            identity: ios_identity("Login", "build-1"),
            flow_path,
            flow_sha256: flow_sha,
            subflow_files: Some(vec![]),
            legacy_closure: false,
            closure_sha256: None,
            validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z"),
            exploration: None,
            repair_history: None,
        })
        .expect_err("lost lock must abort write");
    assert!(matches!(lost_err, StoreError::StoreLocked { .. }));
    assert_eq!(lost_err.exit_code(), 5);

    // Dropping store1 must NOT unlink the hijacked lock (token mismatch).
    drop(store1);
    assert!(
        lock_path.exists(),
        "Drop must not delete lock belonging to another token"
    );

    // Clean up hijacked lock, re-acquire, then normal close() removes lock.
    fs::remove_file(&lock_path).unwrap();
    let store2 = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
    assert!(lock_path.exists());
    store2.close();
    assert!(!lock_path.exists(), "close() must release matching lock");
}

#[test]
fn stale_lock_dead_pid_and_reused_pid_takeover_and_fail_closed_cases() {
    let ws = TempWorkspace::new("stale-lock");
    let lock_path = ws.path().join(".deja/memory/walks.sqlite.lock");
    fs::create_dir_all(lock_path.parent().unwrap()).unwrap();

    // Case A: Dead PID on same machine -> taken over cleanly.
    let dead_pid = spawn_and_reap_dead_pid();
    let dead_holder = LockHolder {
        version: 1,
        pid: dead_pid,
        started_at: Some("Thu Jan  1 00:00:00 1970".to_string()),
        host: current_hostname(),
        machine_id: machine_id(),
        token: "dead-token-1".to_string(),
        acquired_at: now_iso8601(),
    };
    fs::write(
        &lock_path,
        format!("{}\n", serde_json::to_string(&dead_holder).unwrap()),
    )
    .unwrap();

    let mut acquired = WriterLock::acquire(lock_path.clone()).expect("dead PID lock taken over");
    assert_ne!(acquired.token(), Some("dead-token-1"));
    acquired.release();

    // Case B: Reused PID (our own alive PID, but different started_at) -> taken over cleanly.
    let reused_holder = LockHolder {
        version: 1,
        pid: std::process::id(),
        started_at: Some("Mon Jan  1 00:00:00 1990".to_string()),
        host: current_hostname(),
        machine_id: machine_id(),
        token: "reused-pid-token".to_string(),
        acquired_at: now_iso8601(),
    };
    fs::write(
        &lock_path,
        format!("{}\n", serde_json::to_string(&reused_holder).unwrap()),
    )
    .unwrap();
    let mut acquired2 = WriterLock::acquire(lock_path.clone()).expect("reused PID taken over");
    assert_ne!(acquired2.token(), Some("reused-pid-token"));
    acquired2.release();

    // Case C: Foreign machine (even with dead PID) -> fails closed (StoreLocked).
    let foreign_holder = LockHolder {
        version: 1,
        pid: dead_pid,
        started_at: Some("Thu Jan  1 00:00:00 1970".to_string()),
        host: "other-build-machine.local".to_string(),
        machine_id: Some("00000000-0000-0000-0000-ffffffffffff".to_string()),
        token: "foreign-token".to_string(),
        acquired_at: now_iso8601(),
    };
    fs::write(
        &lock_path,
        format!("{}\n", serde_json::to_string(&foreign_holder).unwrap()),
    )
    .unwrap();
    let err = WriterLock::acquire(lock_path.clone()).expect_err("foreign lock must fail closed");
    assert!(matches!(err, StoreError::StoreLocked { .. }));
    fs::remove_file(&lock_path).unwrap();

    // Case D: pid == 0 -> fails closed (StoreLocked).
    let zero_pid_holder = LockHolder {
        version: 1,
        pid: 0,
        started_at: None,
        host: current_hostname(),
        machine_id: machine_id(),
        token: "zero-pid-token".to_string(),
        acquired_at: now_iso8601(),
    };
    fs::write(
        &lock_path,
        format!("{}\n", serde_json::to_string(&zero_pid_holder).unwrap()),
    )
    .unwrap();
    let err = WriterLock::acquire(lock_path.clone()).expect_err("pid 0 lock must fail closed");
    assert!(matches!(err, StoreError::StoreLocked { .. }));
}

#[test]
fn break_aside_race_restores_winner_lock_and_returns_contention() {
    let ws = TempWorkspace::new("break-aside-race");
    let lock_path = ws.path().join(".deja/memory/walks.sqlite.lock");
    fs::create_dir_all(lock_path.parent().unwrap()).unwrap();

    let dead_pid = spawn_and_reap_dead_pid();
    let stale = LockHolder {
        version: 1,
        pid: dead_pid,
        started_at: Some("Thu Jan  1 00:00:00 1970".to_string()),
        host: current_hostname(),
        machine_id: machine_id(),
        token: "stale-observed-token".to_string(),
        acquired_at: now_iso8601(),
    };
    fs::write(
        &lock_path,
        format!("{}\n", serde_json::to_string(&stale).unwrap()),
    )
    .unwrap();

    let winner = LockHolder {
        version: 1,
        pid: std::process::id(),
        started_at: process_start(std::process::id()),
        host: current_hostname(),
        machine_id: machine_id(),
        token: "live-winner-token".to_string(),
        acquired_at: now_iso8601(),
    };
    let winner_json = format!("{}\n", serde_json::to_string(&winner).unwrap());
    let injected = Arc::new(Mutex::new(false));
    let injected_clone = Arc::clone(&injected);
    let target_lock = lock_path.clone();

    let hook = Arc::new(move |path: &Path| {
        if path == target_lock {
            let mut done = injected_clone.lock().unwrap();
            if !*done {
                *done = true;
                // Simulate a racing live writer replacing the stale lock right before move-aside.
                fs::write(path, &winner_json).expect("replace stale lock with winner");
            }
        }
    });

    let err = WriterLock::acquire_with_hook(lock_path.clone(), Some(hook))
        .expect_err("race loser must get StoreLockContention");
    assert!(matches!(err, StoreError::StoreLockContention { .. }));
    assert_eq!(err.exit_code(), 5);

    // Winner's lock MUST have been restored atomically via hard_link!
    let restored = inspect_lock_holder(&lock_path)
        .unwrap()
        .expect("winner lock must be restored");
    assert_eq!(restored.token, "live-winner-token");
}

#[cfg(unix)]
#[test]
fn rejects_symlinks_on_lock_sqlite_wal_shm_and_workspace_escape() {
    use std::os::unix::fs::symlink;

    let outside = TempWorkspace::new("outside-target");
    let decoy = outside.write_file("secret.txt", b"do-not-touch");

    // 1. Symlinked .lock rejected via O_NOFOLLOW (exit 5).
    {
        let ws = TempWorkspace::new("symlink-lock");
        let mem_dir = ws.path().join(".deja/memory");
        fs::create_dir_all(&mem_dir).unwrap();
        symlink(&decoy, mem_dir.join("walks.sqlite.lock")).unwrap();
        let err = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path()))
            .expect_err("symlinked .lock must fail");
        assert!(matches!(err, StoreError::StoreLockUnavailable { .. }));
        assert_eq!(err.exit_code(), 5);
        assert_eq!(fs::read(&decoy).unwrap(), b"do-not-touch");
    }

    // 2. Symlinked walks.sqlite rejected (InvalidWalkMemory, exit 4).
    {
        let ws = TempWorkspace::new("symlink-sqlite");
        let mem_dir = ws.path().join(".deja/memory");
        fs::create_dir_all(&mem_dir).unwrap();
        symlink(&decoy, mem_dir.join("walks.sqlite")).unwrap();
        let err = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path()))
            .expect_err("symlinked sqlite must fail");
        assert!(matches!(err, StoreError::InvalidWalkMemory(_)));
        assert_eq!(err.exit_code(), 4);
    }

    // 3. Symlinked walks.sqlite-wal rejected (InvalidWalkMemory, exit 4).
    {
        let ws = TempWorkspace::new("symlink-wal");
        let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
        store.close();
        let wal = ws.path().join(".deja/memory/walks.sqlite-wal");
        let _ = fs::remove_file(&wal);
        symlink(&decoy, &wal).unwrap();
        let err = SqliteWalkStore::open_readonly(SqliteStoreOptions::new(ws.path()))
            .expect_err("symlinked -wal must fail");
        assert!(matches!(err, StoreError::InvalidWalkMemory(_)));
        assert_eq!(err.exit_code(), 4);
    }

    // 4. Symlinked walks.sqlite-shm rejected (InvalidWalkMemory, exit 4).
    {
        let ws = TempWorkspace::new("symlink-shm");
        let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
        store.close();
        let shm = ws.path().join(".deja/memory/walks.sqlite-shm");
        let _ = fs::remove_file(&shm);
        symlink(&decoy, &shm).unwrap();
        let err = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path()))
            .expect_err("symlinked -shm must fail");
        assert!(matches!(err, StoreError::InvalidWalkMemory(_)));
        assert_eq!(err.exit_code(), 4);
    }

    // 5. Symlinked .deja/memory escaping workspace rejected.
    {
        let ws = TempWorkspace::new("symlink-escape");
        fs::create_dir_all(ws.path().join(".deja")).unwrap();
        symlink(outside.path(), ws.path().join(".deja/memory")).unwrap();
        let err = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path()))
            .expect_err("escaped .deja/memory must fail");
        assert!(matches!(err, StoreError::InvalidWalkMemory(_)));
    }
}

#[test]
fn validates_store_ids_and_proc_stat_parser_and_readonly_creates_zero_files() {
    for bad in [
        "",
        ".leading-dot",
        "-leading-dash",
        "_leading-underscore",
        "../escape",
        "a/b",
        "bad space",
        &"a".repeat(129),
    ] {
        let err = validate_store_id(bad).expect_err("bad store id must fail");
        assert!(matches!(err, StoreError::InvalidStoreId(_)));
        assert_eq!(err.exit_code(), 2);
    }
    assert_eq!(
        store_metadata_path(".deja/memory/walks.sqlite", Some("iphone-16.pro_1")).unwrap(),
        ".deja/memory/stores/iphone-16.pro_1/walks.sqlite"
    );

    // /proc/<pid>/stat with spaces and nested parens in comm name.
    let stat_line =
        "4242 (my (weird) proc) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 987654321 20 21";
    assert_eq!(
        parse_proc_start_ticks(stat_line).as_deref(),
        Some("987654321")
    );

    // open_readonly creates zero files/dirs when DB does not exist.
    let ws = TempWorkspace::new("readonly-empty");
    let ro = SqliteWalkStore::open_readonly(SqliteStoreOptions::new(ws.path())).unwrap();
    assert!(ro.list_walks().unwrap().is_empty());
    assert!(!ws.path().join(".deja").exists());
}

// =============================================================================================
// 2. Schema PRAGMAs, User-Version Checks, Row Integrity Tamper Detection & Embeddings

#[test]
fn schema_pragmas_user_version_bounds_and_integrity_tamper_detection_across_all_tables() {
    let ws = TempWorkspace::new("tamper-all-tables");
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();

    let (main_path, main_sha) = write_classic_flow(
        &ws,
        ".deja/flows/checkout/ios-main.yaml",
        "appId: com.example.app\n---\n- runFlow: sub.yaml\n- assertVisible: Done\n",
    );
    let (sub_path, sub_sha) = write_classic_flow(
        &ws,
        ".deja/flows/checkout/sub.yaml",
        "appId: com.example.app\n---\n- tapOn: Pay\n",
    );

    let id = ios_identity("Checkout", "build-v1");
    store
        .upsert_walk(&UpsertWalkInput {
            identity: id.clone(),
            flow_path: main_path,
            flow_sha256: main_sha,
            subflow_files: Some(vec![StoredFlowFile {
                path: sub_path,
                sha256: sub_sha,
            }]),
            legacy_closure: false,
            closure_sha256: None,
            validation: RecordValidationInput::new(true, "2026-05-01T12:00:00Z")
                .with_runtime("ios-18.4")
                .with_origin(WalkOrigin::Remember)
                .with_duration_ms(550)
                .with_phase_timings_json(r#"{"launchMs":120}"#),
            exploration: None,
            repair_history: Some(vec![RepairProvenance {
                attempt_id: "attempt-1".to_string(),
                patch_path: ".deja/patches/p1.diff".to_string(),
                previous_build_fingerprint: fake_sha("build-v0"),
                previous_flow_sha256: fake_sha("old-flow"),
            }]),
        })
        .unwrap();

    let walk_id = store.find_walk_id(&id).unwrap().expect("walk id");
    let provider = LocalLexicalEmbeddingProvider::new();
    let vec = provider.embed("Checkout").unwrap();
    store
        .upsert_goal_embedding(walk_id, provider.id(), &vec, "2026-05-01T12:01:00Z")
        .unwrap();
    let loaded_emb = store
        .get_goal_embedding(walk_id, provider.id())
        .unwrap()
        .expect("embedding stored");
    assert_eq!(loaded_emb.embedding, vec);

    // Seed build_fingerprint_cache too.
    let artifact = ws.write_file("build/app.apk", b"apk-binary-payload-v1");
    let fp = store
        .fingerprint_with_cache(ws.path(), &artifact, "Android")
        .unwrap();
    assert!(!fp.cache_hit);

    let db_path = store.memory_path().to_path_buf();
    store.close();

    // Check writable/readonly PRAGMAs and user_version.
    {
        let raw = Connection::open(&db_path).unwrap();
        let uv: u32 = raw
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, SQLITE_SCHEMA_VERSION);
        let jm: String = raw
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(jm.to_ascii_lowercase(), "wal");
    }

    // Helper: run SQL tamper, assert check returns InvalidWalkMemory, then restore original value.
    let raw = Connection::open(&db_path).unwrap();
    let ro = SqliteWalkStore::open_readonly(SqliteStoreOptions::new(ws.path())).unwrap();

    // 2a. Tamper `walks`
    raw.execute(
        "UPDATE walks SET origin = 'explore' WHERE id = ?1",
        [walk_id],
    )
    .unwrap();
    assert!(matches!(
        ro.lookup_for_runtime(&id, Some("ios-18.4")),
        Err(StoreError::InvalidWalkMemory(_))
    ));
    raw.execute(
        "UPDATE walks SET origin = 'remember' WHERE id = ?1",
        [walk_id],
    )
    .unwrap();
    assert!(matches!(
        ro.lookup_for_runtime(&id, Some("ios-18.4")),
        Ok(LookupResult::Hit(_))
    ));

    // 2b. Tamper `validation_runs`
    raw.execute(
        "UPDATE validation_runs SET duration_ms = 9999 WHERE walk_id = ?1",
        [walk_id],
    )
    .unwrap();
    assert!(matches!(
        ro.lookup_for_runtime(&id, Some("ios-18.4")),
        Err(StoreError::InvalidWalkMemory(_))
    ));
    raw.execute(
        "UPDATE validation_runs SET duration_ms = 550 WHERE walk_id = ?1",
        [walk_id],
    )
    .unwrap();

    // 2c. Tamper `validation_history`
    raw.execute(
        "UPDATE validation_history SET passed = 0 WHERE walk_id = ?1",
        [walk_id],
    )
    .unwrap();
    assert!(matches!(
        ro.list_walks(),
        Err(StoreError::InvalidWalkMemory(_))
    ));
    raw.execute(
        "UPDATE validation_history SET passed = 1 WHERE walk_id = ?1",
        [walk_id],
    )
    .unwrap();

    // 2d. Tamper `walk_files`
    raw.execute(
        "UPDATE walk_files SET sha256 = ?1 WHERE walk_id = ?2",
        rusqlite::params![fake_sha("tampered-subflow"), walk_id],
    )
    .unwrap();
    assert!(matches!(
        ro.list_walks(),
        Err(StoreError::InvalidWalkMemory(_))
    ));
    // Restore walk_files sha256.
    let real_sub_sha = sha256_hex(b"appId: com.example.app\n---\n- tapOn: Pay\n");
    raw.execute(
        "UPDATE walk_files SET sha256 = ?1 WHERE walk_id = ?2",
        rusqlite::params![real_sub_sha, walk_id],
    )
    .unwrap();

    // 2e. Tamper `repair_history`
    raw.execute(
        "UPDATE repair_history SET attempt_id = 'tampered' WHERE walk_id = ?1",
        [walk_id],
    )
    .unwrap();
    assert!(matches!(
        ro.list_walks(),
        Err(StoreError::InvalidWalkMemory(_))
    ));
    raw.execute(
        "UPDATE repair_history SET attempt_id = 'attempt-1' WHERE walk_id = ?1",
        [walk_id],
    )
    .unwrap();

    // 2f. Tamper `goal_embeddings`
    raw.execute(
        "UPDATE goal_embeddings SET updated_at = '2026-05-02T00:00:00Z' WHERE walk_id = ?1",
        [walk_id],
    )
    .unwrap();
    assert!(matches!(
        ro.get_goal_embedding(walk_id, provider.id()),
        Err(StoreError::InvalidWalkMemory(_))
    ));

    // 2g. Tamper `build_fingerprint_cache`
    raw.execute(
        "UPDATE build_fingerprint_cache SET digest_sha256 = ?1",
        [fake_sha("forged-digest")],
    )
    .unwrap();
    assert!(matches!(
        ro.fingerprint_with_cache(ws.path(), &artifact, "Android"),
        Err(StoreError::InvalidWalkMemory(_))
    ));

    // 2h. Reject user_version < 2 and > 5.
    drop(ro);
    for bad_uv in [1u32, 6u32] {
        raw.pragma_update(None, "user_version", bad_uv).unwrap();
        let err = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path()))
            .expect_err("unsupported user_version must fail");
        assert!(matches!(err, StoreError::InvalidWalkMemory(_)));
        assert_eq!(err.exit_code(), 4);
    }
}

// =============================================================================================
// 3. Crash / Rollback Atomicity

#[test]
fn begin_immediate_transaction_rollback_leaves_zero_partial_state() {
    let ws = TempWorkspace::new("rollback-atomicity");
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
    let db_path = store.memory_path().to_path_buf();

    // Inject a trigger that aborts mid-transaction on validation_history insert.
    let side = Connection::open(&db_path).unwrap();
    side.execute(
        "CREATE TRIGGER abort_hist BEFORE INSERT ON validation_history
         BEGIN SELECT RAISE(ABORT, 'injected crash mid-transaction'); END;",
        [],
    )
    .unwrap();

    let candidate = ws.write_file(
        "flows/atomic.yaml",
        b"appId: com.example.app\n---\n- assertVisible: Atomic\n",
    );
    let err = store
        .persist_verified_candidate(
            &ios_identity("Atomic Test", "build-atomic"),
            &PersistVerifiedCandidateOptions {
                candidate_path: candidate,
                validation: RecordValidationInput::new(true, "2026-05-01T14:00:00Z"),
                expected_flow_sha256: None,
                expected_closure_sha256: None,
                exploration: None,
                repair_history: None,
            },
        )
        .expect_err("trigger must abort transaction");
    assert!(matches!(err, StoreError::Sqlite(_)));

    side.execute("DROP TRIGGER abort_hist", []).unwrap();

    // Zero partial rows in walks, validation_runs, validation_history, or walk_files!
    for table in [
        "walks",
        "validation_runs",
        "validation_history",
        "walk_files",
    ] {
        let count: i64 = side
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "table {table} must be empty after rollback");
    }

    // Published content-addressed flow files are NOT deleted on SQLite failure (another shard may
    // already reference the same content-addressed path), while ZERO private `.tmp` staging files
    // may remain behind.
    let flows_dir = ws.path().join(".deja/flows/atomic-test");
    if flows_dir.exists() {
        for entry in fs::read_dir(&flows_dir).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(
                !name.ends_with(".tmp"),
                "private staging temp file must not remain after rollback: {name}"
            );
        }
    }
}

// =============================================================================================
// 4. Multi-Build (DEJA-12), Multi-Runtime (DEJA-55), Durable Flaky (DEJA-42), Snapshot Closure

#[test]
fn multi_build_multi_runtime_flaky_and_snapshot_candidate() {
    let ws = TempWorkspace::new("multi-build-runtime");
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();

    ws.write_file(
        "candidate/main.yaml",
        b"appId: com.example.app\n---\n- runFlow: helper.yaml\n- assertVisible: Ready\n",
    );
    ws.write_file(
        "candidate/helper.yaml",
        b"appId: com.example.app\n---\n- tapOn: Start\n",
    );

    // Snapshot candidate closure under .deja/attempts/.
    let snap = store
        .snapshot_candidate(Path::new("candidate/main.yaml"))
        .expect("snapshot multi-file closure");
    assert_eq!(snap.files.len(), 2);
    assert!(ws.path().join(&snap.candidate_path).exists());

    // Build A passes on ios-18.2.
    let id_a = ios_identity("Start Flow", "build-A");
    store
        .persist_verified_candidate(
            &id_a,
            &PersistVerifiedCandidateOptions {
                candidate_path: PathBuf::from(&snap.candidate_path),
                validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z")
                    .with_runtime("ios-18.2")
                    .with_origin(WalkOrigin::Remember)
                    .with_duration_ms(1100),
                expected_flow_sha256: Some(snap.flow_sha256.clone()),
                expected_closure_sha256: Some(snap.closure_sha256.clone()),
                exploration: None,
                repair_history: None,
            },
        )
        .unwrap();

    // Build B passes on ios-18.2.
    let id_b = ios_identity("Start Flow", "build-B");
    store
        .record_validation(
            &id_b,
            &RecordValidationInput::new(true, "2026-05-02T10:00:00Z")
                .with_runtime("ios-18.2")
                .with_origin(WalkOrigin::ReplayedOnNewBuild)
                .with_duration_ms(980),
        )
        .unwrap();

    // Build B fails (flaky!) on ios-18.4.
    store
        .record_validation(
            &id_b,
            &RecordValidationInput::new(false, "2026-05-02T10:05:00Z")
                .with_runtime("ios-18.4")
                .with_flaky(true)
                .with_failure_category("selector")
                .with_report_path(".deja/reports/flaky-1.json")
                .with_duration_ms(1450)
                .with_phase_timings_json(r#"{"retryMs":320}"#),
        )
        .unwrap();

    // Build A is STILL a hit on ios-18.2 (DEJA-12 rollback preserved!).
    assert!(matches!(
        store.lookup_for_runtime(&id_a, Some("ios-18.2")).unwrap(),
        LookupResult::Hit(_)
    ));
    // Build B is a hit on ios-18.2, and FailedValidation on ios-18.4 (DEJA-55 runtime isolation!).
    assert!(matches!(
        store.lookup_for_runtime(&id_b, Some("ios-18.2")).unwrap(),
        LookupResult::Hit(_)
    ));
    assert!(matches!(
        store.lookup_for_runtime(&id_b, Some("ios-18.4")).unwrap(),
        LookupResult::Miss(m) if m.reason == CacheMissReason::FailedValidation
    ));

    let runs = store.list_validation_runs(&id_b).unwrap();
    assert_eq!(runs.len(), 3);
    let flaky_run = runs
        .iter()
        .find(|r| r.runtime.as_deref() == Some("ios-18.4"))
        .unwrap();
    assert!(flaky_run.flaky);
    assert!(!flaky_run.passed);
    assert_eq!(flaky_run.duration_ms, Some(1450));
    assert_eq!(flaky_run.failure_category.as_deref(), Some("selector"));
    assert_eq!(
        flaky_run.report_path.as_deref(),
        Some(".deja/reports/flaky-1.json")
    );
    assert_eq!(
        flaky_run.phase_timings_json.as_deref(),
        Some(r#"{"retryMs":320}"#)
    );

    // lookup_previous_successful on brand new Build C finds Build B's passing run on ios-18.2.
    let id_c = ios_identity("Start Flow", "build-C");
    let prev = store
        .lookup_previous_successful_for_runtime(&id_c, Some("ios-18.2"))
        .unwrap();
    match prev {
        LookupResult::Hit(hit) => assert_eq!(hit.walk.build_fingerprint, fake_sha("build-B")),
        other => panic!("expected previous successful Hit, got {other:?}"),
    }
}

// =============================================================================================
// 5. ConsumerFlows + DEJA-51 Manifest Replay State Machine

#[test]
fn consumer_flows_deja51_replay_and_fail_closed_drift() {
    let ws = TempWorkspace::new("consumer-deja51");
    let flow_rel = ".maestro/flows/checkout.yaml";
    let flow_yaml = "appId: com.example.app\n---\n- tapOn:\n    id: pay_btn\n- assertVisible:\n    id: order_done\n";
    let flow_abs = ws.write_file(flow_rel, flow_yaml.as_bytes());

    let closure = collect_flow_closure(ws.path(), &flow_abs).unwrap();
    let assert_sha = assertions_sha256(&closure).unwrap();

    let manifest = WalkManifest {
        manifest_version: 1,
        hash_version: 1,
        walks: vec![ManifestWalk {
            goal: "Complete checkout".to_string(),
            app_id: "com.example.app".to_string(),
            platform: "ios".to_string(),
            adapter: "ios-native".to_string(),
            browser: None,
            flow: flow_rel.to_string(),
            closure_sha256: closure.sha256.clone(),
            assertions_sha256: assert_sha.clone(),
            owner_rules: Some(ManifestOwnerRules {
                min_assertions: Some(1),
                required_assertion_ids: Some(vec!["order_done".to_string()]),
                disallow_text_selectors: Some(true),
                allowed_text_selectors: None,
            }),
            profile: None,
        }],
    };
    let manifest_bytes = serialize_walk_manifest(&manifest).unwrap();
    let manifest_abs = ws.write_file("deja.walks.json", manifest_bytes.as_bytes());

    let consumer = ConsumerFlows::new(
        ConsumerFlowsOptions::new(ws.path(), "deja.walks.json")
            .with_forbidden_values(vec!["super-secret-pass".to_string()])
            .with_lock_retries(1),
    )
    .unwrap();
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
    let id_b1 = ios_identity("Complete checkout", "build-1");

    // 5a. Fresh store + unchanged manifest WITHOUT ReplayLastVerified -> NotValidated (exit 6).
    let plan = consumer
        .plan_run(
            &store,
            &ConsumerRunRequest {
                identity: &id_b1,
                runtime: Some("ios-18.4"),
                stale_build: None,
            },
        )
        .unwrap();
    assert!(matches!(
        plan,
        ConsumerRunPlan::Refuse {
            outcome: RunOutcome::NotValidated,
            miss_reason: CacheMissReason::NotValidated,
            exit_code: 6,
            ..
        }
    ));

    // 5b. Fresh store + unchanged manifest WITH ReplayLastVerified -> ReplayInPlace without --verify!
    let plan_replay = consumer
        .plan_run(
            &store,
            &ConsumerRunRequest {
                identity: &id_b1,
                runtime: Some("ios-18.4"),
                stale_build: Some(StaleBuildPolicy::ReplayLastVerified),
            },
        )
        .unwrap();
    assert!(matches!(
        plan_replay,
        ConsumerRunPlan::ReplayInPlace {
            reason: CacheMissReason::NotValidated,
            ..
        }
    ));

    // Record post-run pass without touching deja.walks.json!
    consumer
        .record_post_run_validation(
            &store,
            &PostRunValidationOptions {
                identity: id_b1.clone(),
                expected_closure_sha256: closure.sha256.clone(),
                expected_assertions_sha256: assert_sha.clone(),
                validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z")
                    .with_runtime("ios-18.4")
                    .with_origin(WalkOrigin::ReplayedOnNewBuild),
            },
        )
        .unwrap();
    assert_eq!(
        fs::read_to_string(&manifest_abs).unwrap(),
        manifest_bytes,
        "DEJA-51 replay must never modify deja.walks.json"
    );

    // Next plan_run is an exact Hit!
    let plan_hit = consumer
        .plan_run(
            &store,
            &ConsumerRunRequest {
                identity: &id_b1,
                runtime: Some("ios-18.4"),
                stale_build: None,
            },
        )
        .unwrap();
    assert!(matches!(plan_hit, ConsumerRunPlan::Hit { .. }));

    // 5c. Stale build (build-2): refuses exit 1 without policy, replays in place with policy.
    let id_b2 = ios_identity("Complete checkout", "build-2");
    assert!(matches!(
        consumer
            .plan_run(
                &store,
                &ConsumerRunRequest {
                    identity: &id_b2,
                    runtime: Some("ios-18.4"),
                    stale_build: None,
                },
            )
            .unwrap(),
        ConsumerRunPlan::Refuse {
            miss_reason: CacheMissReason::StaleBuild,
            exit_code: 1,
            ..
        }
    ));
    assert!(matches!(
        consumer
            .plan_run(
                &store,
                &ConsumerRunRequest {
                    identity: &id_b2,
                    runtime: Some("ios-18.4"),
                    stale_build: Some(StaleBuildPolicy::ReplayLastVerified),
                },
            )
            .unwrap(),
        ConsumerRunPlan::ReplayInPlace {
            reason: CacheMissReason::StaleBuild,
            previous_successful: Some(_),
            ..
        }
    ));

    // 5d. Failed validation on build-2: refuses exit 1 without policy, replays in place with policy.
    consumer
        .record_post_run_validation(
            &store,
            &PostRunValidationOptions {
                identity: id_b2.clone(),
                expected_closure_sha256: closure.sha256.clone(),
                expected_assertions_sha256: assert_sha.clone(),
                validation: RecordValidationInput::new(false, "2026-05-01T11:00:00Z")
                    .with_runtime("ios-18.4"),
            },
        )
        .unwrap();
    assert!(matches!(
        consumer
            .plan_run(
                &store,
                &ConsumerRunRequest {
                    identity: &id_b2,
                    runtime: Some("ios-18.4"),
                    stale_build: None,
                },
            )
            .unwrap(),
        ConsumerRunPlan::Refuse {
            miss_reason: CacheMissReason::FailedValidation,
            exit_code: 1,
            ..
        }
    ));
    assert!(matches!(
        consumer
            .plan_run(
                &store,
                &ConsumerRunRequest {
                    identity: &id_b2,
                    runtime: Some("ios-18.4"),
                    stale_build: Some(StaleBuildPolicy::ReplayLastVerified),
                },
            )
            .unwrap(),
        ConsumerRunPlan::ReplayInPlace {
            reason: CacheMissReason::FailedValidation,
            ..
        }
    ));

    // 5e. Flow changed on disk -> refuses exit 6 even WITH ReplayLastVerified, zero writes!
    ws.write_file(
        flow_rel,
        b"appId: com.example.app\n---\n- tapOn:\n    id: pay_btn_v2\n- assertVisible:\n    id: order_done\n",
    );
    let plan_changed = consumer
        .plan_run(
            &store,
            &ConsumerRunRequest {
                identity: &id_b1,
                runtime: Some("ios-18.4"),
                stale_build: Some(StaleBuildPolicy::ReplayLastVerified),
            },
        )
        .unwrap();
    assert!(matches!(
        plan_changed,
        ConsumerRunPlan::Refuse {
            outcome: RunOutcome::FlowChanged,
            exit_code: 6,
            flow_change: Some(_),
            ..
        }
    ));
    let post_err = consumer
        .record_post_run_validation(
            &store,
            &PostRunValidationOptions {
                identity: id_b1.clone(),
                expected_closure_sha256: closure.sha256.clone(),
                expected_assertions_sha256: assert_sha.clone(),
                validation: RecordValidationInput::new(true, "2026-05-01T12:00:00Z")
                    .with_runtime("ios-18.4"),
            },
        )
        .expect_err("changed flow must fail closed on post-run validation");
    assert_eq!(post_err.exit_code(), 6);

    // 5f. Assertions mismatch while closure matches -> AssertionsChanged (exit 6), zero writes!
    ws.write_file(flow_rel, flow_yaml.as_bytes());
    let mut bad_assert_manifest = manifest.clone();
    bad_assert_manifest.walks[0].assertions_sha256 = fake_sha("stale-assertions");
    ws.write_file(
        "deja.walks.json",
        serialize_walk_manifest(&bad_assert_manifest)
            .unwrap()
            .as_bytes(),
    );
    let plan_assert = consumer
        .plan_run(
            &store,
            &ConsumerRunRequest {
                identity: &id_b1,
                runtime: Some("ios-18.4"),
                stale_build: Some(StaleBuildPolicy::ReplayLastVerified),
            },
        )
        .unwrap();
    assert!(matches!(
        plan_assert,
        ConsumerRunPlan::Refuse {
            outcome: RunOutcome::AssertionsChanged,
            exit_code: 6,
            ..
        }
    ));
    let post_assert_err = consumer
        .record_post_run_validation(
            &store,
            &PostRunValidationOptions {
                identity: id_b1.clone(),
                expected_closure_sha256: closure.sha256.clone(),
                expected_assertions_sha256: assert_sha.clone(),
                validation: RecordValidationInput::new(true, "2026-05-01T12:05:00Z")
                    .with_runtime("ios-18.4"),
            },
        )
        .expect_err("assertions mismatch must fail closed");
    assert_eq!(post_assert_err.exit_code(), 6);

    // 5g. Owner rule violation (disallowTextSelectors) rejected by save_verified_manifest_walk.
    ws.write_file("deja.walks.json", manifest_bytes.as_bytes());
    let edited_abs = ws.write_file(
        flow_rel,
        b"appId: com.example.app\n---\n- tapOn: Pay Now\n- assertVisible:\n    id: order_done\n",
    );
    let edited_closure = collect_flow_closure(ws.path(), &edited_abs).unwrap();
    let owner_err = consumer
        .save_verified_manifest_walk(
            &store,
            &SaveManifestWalkOptions {
                identity: id_b1.clone(),
                flow_path: flow_rel.to_string(),
                expected_closure_sha256: edited_closure.sha256,
                expected_assertions_sha256: None,
                validation: RecordValidationInput::new(true, "2026-05-01T12:10:00Z")
                    .with_origin(WalkOrigin::Remember),
                accept_assertion_change: true,
            },
        )
        .expect_err("text selector violates ownerRules.disallowTextSelectors");
    assert!(matches!(
        owner_err,
        StoreError::Core(deja_core::CoreError::OwnerRule(_))
    ));
}

// =============================================================================================
// 6. Multi-Store & Multi-Runtime Release Gate (DEJA-49, DEJA-55, DEJA-42)

#[test]
fn multi_store_evaluate_gate_sharding_conflicts_flaky_runtime_and_corrupt_store() {
    let ws = TempWorkspace::new("multi-store-gate");
    let build_fp = fake_sha("release-build-1");
    let gate_id = GateIdentity {
        app_id: "com.example.app".to_string(),
        platform: "ios".to_string(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint: build_fp.clone(),
        adapter: Some("ios-native".to_string()),
        browser: None,
    };

    let (flow_a, sha_a) = write_classic_flow(
        &ws,
        ".deja/flows/goal-a/ios-a.yaml",
        "appId: com.example.app\n---\n- assertVisible: A\n",
    );
    let (flow_a_alt, sha_a_alt) = write_classic_flow(
        &ws,
        ".deja/flows/goal-a/ios-a-alt.yaml",
        "appId: com.example.app\n---\n- assertVisible: A_ALT\n",
    );
    let (flow_b, sha_b) = write_classic_flow(
        &ws,
        ".deja/flows/goal-b/ios-b.yaml",
        "appId: com.example.app\n---\n- assertVisible: B\n",
    );

    // Shard 1 passes Goal A on ios-18.2.
    {
        let s1 = SqliteWalkStore::open_writable(
            SqliteStoreOptions::new(ws.path()).with_store_id("shard-1"),
        )
        .unwrap();
        s1.upsert_walk(&UpsertWalkInput {
            identity: ios_identity("Goal A", "release-build-1"),
            flow_path: flow_a.clone(),
            flow_sha256: sha_a.clone(),
            subflow_files: Some(vec![]),
            legacy_closure: false,
            closure_sha256: None,
            validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z")
                .with_runtime("ios-18.2"),
            exploration: None,
            repair_history: None,
        })
        .unwrap();
    }

    // Shard 2 passes Goal B on ios-18.2.
    {
        let s2 = SqliteWalkStore::open_writable(
            SqliteStoreOptions::new(ws.path()).with_store_id("shard-2"),
        )
        .unwrap();
        s2.upsert_walk(&UpsertWalkInput {
            identity: ios_identity("Goal B", "release-build-1"),
            flow_path: flow_b.clone(),
            flow_sha256: sha_b.clone(),
            subflow_files: Some(vec![]),
            legacy_closure: false,
            closure_sha256: None,
            validation: RecordValidationInput::new(true, "2026-05-01T10:01:00Z")
                .with_runtime("ios-18.2"),
            exploration: None,
            repair_history: None,
        })
        .unwrap();
    }

    let ro1 =
        SqliteWalkStore::open_readonly(SqliteStoreOptions::new(ws.path()).with_store_id("shard-1"))
            .unwrap();
    let ro2 =
        SqliteWalkStore::open_readonly(SqliteStoreOptions::new(ws.path()).with_store_id("shard-2"))
            .unwrap();

    // 6a. Sharded pass + goal deduplication!
    let results = evaluate_gate(
        &[&ro1, &ro2],
        &[
            "Goal A".to_string(),
            "Goal B".to_string(),
            "  goal   a ".to_string(),
        ],
        &gate_id,
        &StoreGateOptions {
            runtime: Some("ios-18.2"),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(results.len(), 2);
    assert!(store_gate_passed(&results));
    assert_eq!(results[0].source_stores, vec!["shard-1"]);
    assert_eq!(results[1].source_stores, vec!["shard-2"]);

    // 6b. Runtime isolation: Goal A passes on ios-18.2, fails on ios-18.4 in shard-1.
    {
        let s1 = SqliteWalkStore::open_writable(
            SqliteStoreOptions::new(ws.path()).with_store_id("shard-1"),
        )
        .unwrap();
        s1.record_validation(
            &ios_identity("Goal A", "release-build-1"),
            &RecordValidationInput::new(false, "2026-05-01T10:02:00Z").with_runtime("ios-18.4"),
        )
        .unwrap();
    }
    let only_18_2 = evaluate_gate(
        &[&ro1, &ro2],
        &["Goal A".to_string()],
        &gate_id,
        &StoreGateOptions {
            runtime: Some("ios-18.2"),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(only_18_2[0].outcome, GateOutcome::Passed);

    let only_18_4 = evaluate_gate(
        &[&ro1, &ro2],
        &["Goal A".to_string()],
        &gate_id,
        &StoreGateOptions {
            runtime: Some("ios-18.4"),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(only_18_4[0].outcome, GateOutcome::Failed);

    let all_runtimes = evaluate_gate(
        &[&ro1, &ro2],
        &["Goal A".to_string()],
        &gate_id,
        &StoreGateOptions::default(),
    )
    .unwrap();
    assert_eq!(
        all_runtimes[0].outcome,
        GateOutcome::Failed,
        "runtime=None must fail closed across all runtimes"
    );

    // 6c. Single-shard fail & flaky precedence (Flaky > Failed > Passed).
    {
        let s2 = SqliteWalkStore::open_writable(
            SqliteStoreOptions::new(ws.path()).with_store_id("shard-2"),
        )
        .unwrap();
        s2.upsert_walk(&UpsertWalkInput {
            identity: ios_identity("Goal A", "release-build-1"),
            flow_path: flow_a.clone(),
            flow_sha256: sha_a.clone(),
            subflow_files: Some(vec![]),
            legacy_closure: false,
            closure_sha256: None,
            validation: RecordValidationInput::new(false, "2026-05-01T10:03:00Z")
                .with_runtime("ios-18.2")
                .with_flaky(true),
            exploration: None,
            repair_history: None,
        })
        .unwrap();
    }
    let flaky_res = evaluate_gate(
        &[&ro1, &ro2],
        &["Goal A".to_string()],
        &gate_id,
        &StoreGateOptions {
            runtime: Some("ios-18.2"),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(flaky_res[0].outcome, GateOutcome::Flaky);
    assert_eq!(flaky_res[0].source_stores, vec!["shard-1", "shard-2"]);

    // External GateFlakyFailure also marks a non-flaky failure as Flaky.
    let ext_flaky = evaluate_gate(
        &[&ro1],
        &["Goal A".to_string()],
        &gate_id,
        &StoreGateOptions {
            runtime: Some("ios-18.4"),
            flaky_failures: &[GateFlakyFailure {
                goal_key: "goal a".to_string(),
                build_fingerprint: build_fp.clone(),
                at: "2026-05-01T10:02:00Z".to_string(),
            }],
            consumer: None,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(ext_flaky[0].outcome, GateOutcome::Flaky);

    // 6d. Cross-shard hash conflict (two Passed shards with different flow_sha256) -> Invalid!
    {
        let s2 = SqliteWalkStore::open_writable(
            SqliteStoreOptions::new(ws.path()).with_store_id("shard-2"),
        )
        .unwrap();
        s2.upsert_walk(&UpsertWalkInput {
            identity: ios_identity("Goal A", "release-build-1"),
            flow_path: flow_a_alt,
            flow_sha256: sha_a_alt,
            subflow_files: Some(vec![]),
            legacy_closure: false,
            closure_sha256: None,
            validation: RecordValidationInput::new(true, "2026-05-01T10:04:00Z")
                .with_runtime("ios-18.2"),
            exploration: None,
            repair_history: None,
        })
        .unwrap();
    }
    let conflict = evaluate_gate(
        &[&ro1, &ro2],
        &["Goal A".to_string()],
        &gate_id,
        &StoreGateOptions {
            runtime: Some("ios-18.2"),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(conflict[0].outcome, GateOutcome::Invalid);
    assert!(
        conflict[0]
            .detail
            .as_deref()
            .unwrap_or("")
            .contains("conflicting")
    );

    // 6e. Corrupt/tampered store returns Err (gate_exit_code == 11).
    let corrupt_dir = TempWorkspace::new("corrupt-gate-shard");
    corrupt_dir.write_file(".deja/memory/stores/bad/walks.sqlite", b"not a sqlite file");
    let bad_open = SqliteWalkStore::open_readonly(
        SqliteStoreOptions::new(corrupt_dir.path()).with_store_id("bad"),
    );
    let err = bad_open.expect_err("corrupt sqlite file must return Err");
    assert_eq!(err.gate_exit_code(), GATE_INVALID_EXIT_CODE);

    // Also verify row-level tamper inside a valid SQLite file causes evaluate_gate to return Err (exit 11).
    {
        let raw = Connection::open(ro1.memory_path()).unwrap();
        raw.execute(
            "UPDATE validation_runs SET passed = 0 WHERE runtime = 'ios-18.2'",
            [],
        )
        .unwrap();
    }
    let tamper_err = evaluate_gate(
        &[&ro1],
        &["Goal A".to_string()],
        &gate_id,
        &StoreGateOptions {
            runtime: Some("ios-18.2"),
            ..Default::default()
        },
    )
    .expect_err("tampered SQLite row must make evaluate_gate return Err");
    assert!(matches!(tamper_err, StoreError::InvalidWalkMemory(_)));
    assert_eq!(tamper_err.gate_exit_code(), GATE_INVALID_EXIT_CODE);
}

// =============================================================================================
// 7. Legacy walks.json (Schemas 1-4) Read-Only Importer

#[test]
fn legacy_walks_json_import_schemas_1_through_4_read_only_and_rejections() {
    let build = fake_sha("legacy-build");
    let flow_yaml = "appId: com.example.app\n---\n- assertVisible: Legacy\n";
    let flow_sha = sha256_hex(flow_yaml.as_bytes());
    let closure_sha = fake_sha("manifest-closure");

    // Schemas 1, 2, 3, and 4 all import cleanly without modifying walks.json.
    for schema_version in [1u32, 2, 3, 4] {
        let ws = TempWorkspace::new(&format!("legacy-v{schema_version}"));
        let flow_rel = if schema_version == 4 {
            ".maestro/flows/legacy.yaml"
        } else {
            ".deja/flows/legacy-goal/ios-legacy.yaml"
        };
        ws.write_file(flow_rel, flow_yaml.as_bytes());

        let walk_json = match schema_version {
            1 => serde_json::json!({
                "goal": "Legacy Goal",
                "goalKey": "legacy goal",
                "appId": "com.example.app",
                "platform": "ios",
                "buildFingerprint": build,
                "flowPath": flow_rel,
                "flowSha256": flow_sha,
                "lastValidation": { "passed": true, "at": "2026-04-01T08:00:00Z" }
            }),
            2 => serde_json::json!({
                "goal": "Legacy Goal",
                "goalKey": "legacy goal",
                "appId": "com.example.app",
                "platform": "ios",
                "flowSchemaVersion": "maestro/v1",
                "buildFingerprint": build,
                "flowPath": flow_rel,
                "flowSha256": flow_sha,
                "lastValidation": { "passed": true, "at": "2026-04-01T08:00:00Z" },
                "validationHistory": [{ "passed": true, "at": "2026-04-01T08:00:00Z", "flowSha256": flow_sha }],
                "origin": "remember"
            }),
            3 => serde_json::json!({
                "goal": "Legacy Goal",
                "goalKey": "legacy goal",
                "appId": "com.example.app",
                "platform": "ios",
                "flowSchemaVersion": "maestro/v1",
                "buildFingerprint": build,
                "flowPath": flow_rel,
                "flowSha256": flow_sha,
                "subflowFiles": [],
                "lastValidation": { "passed": true, "at": "2026-04-01T08:00:00Z" },
                "validationHistory": [{ "passed": true, "at": "2026-04-01T08:00:00Z", "flowSha256": flow_sha }],
                "origin": "remember"
            }),
            4 => serde_json::json!({
                "goal": "Legacy Goal",
                "goalKey": "legacy goal",
                "appId": "com.example.app",
                "platform": "ios",
                "flowSchemaVersion": "maestro/v1",
                "buildFingerprint": build,
                "adapter": "ios-native",
                "flowPath": flow_rel,
                "flowSha256": flow_sha,
                "closureSha256": closure_sha,
                "lastValidation": { "passed": true, "at": "2026-04-01T08:00:00Z" },
                "validationHistory": [{ "passed": true, "at": "2026-04-01T08:00:00Z", "flowSha256": flow_sha }],
                "origin": "verify"
            }),
            _ => unreachable!(),
        };

        let raw_json = serde_json::to_string_pretty(&serde_json::json!({
            "schemaVersion": schema_version,
            "walks": [walk_json],
        }))
        .unwrap();
        let json_path = ws.write_file(LEGACY_JSON_MEMORY_PATH, raw_json.as_bytes());

        let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
        let report = import_legacy_walks_json(&store, None).expect("import legacy walks.json");
        assert_eq!(report.source_schema_version, schema_version);
        assert_eq!(report.imported_walks, 1);
        assert_eq!(report.imported_validation_runs, 1);

        // walks.json MUST remain untouched byte-for-byte!
        assert_eq!(fs::read_to_string(&json_path).unwrap(), raw_json);

        let walks = store.list_walks().unwrap();
        assert_eq!(walks.len(), 1);
        if schema_version <= 2 {
            assert_eq!(walks[0].legacy_closure, Some(true));
        } else {
            assert_eq!(walks[0].legacy_closure, None);
        }
        if schema_version == 4 {
            assert_eq!(
                walks[0].closure_sha256.as_deref(),
                Some(closure_sha.as_str())
            );
        }
    }

    // Reject duplicate JSON keys, invalid timestamp, invalid checksum, and duplicate identities.
    let ws = TempWorkspace::new("legacy-invalid");
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();

    let dup_key_json = ws.write_file(
        ".deja/memory/dup.json",
        br#"{"schemaVersion":2,"schemaVersion":2,"walks":[]}"#,
    );
    assert!(matches!(
        import_legacy_walks_json(&store, Some(&dup_key_json)),
        Err(StoreError::InvalidWalkMemory(_))
    ));

    let bad_ts_json = ws.write_file(
        ".deja/memory/bad-ts.json",
        serde_json::to_string(&serde_json::json!({
            "schemaVersion": 2,
            "walks": [{
                "goal": "G",
                "goalKey": "g",
                "appId": "app",
                "platform": "ios",
                "flowSchemaVersion": "maestro/v1",
                "buildFingerprint": build,
                "flowPath": ".deja/flows/g/ios.yaml",
                "flowSha256": flow_sha,
                "lastValidation": { "passed": true, "at": "not-a-timestamp" },
                "validationHistory": [{ "passed": true, "at": "not-a-timestamp", "flowSha256": flow_sha }]
            }]
        }))
        .unwrap()
        .as_bytes(),
    );
    assert!(matches!(
        import_legacy_walks_json(&store, Some(&bad_ts_json)),
        Err(StoreError::InvalidWalkMemory(_))
    ));
    assert!(store.list_walks().unwrap().is_empty());
}

// =============================================================================================
// 8. Performance Assertions (1,000-Walk O(1) Lookup + Persistent Build Fingerprint Cache)

#[test]
fn perf_thousand_walk_point_lookup_uses_index_under_2ms() {
    let ws = TempWorkspace::new("perf-1000-walks");
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();

    let (flow_path, flow_sha) = write_classic_flow(
        &ws,
        ".deja/flows/shared/ios-flow.yaml",
        "appId: com.example.app\n---\n- assertVisible: Perf\n",
    );

    // Seed 1,000 walks directly via store API (batching inside one transaction via legacy JSON import
    // keeps test setup fast while exercising full SQLite v5 integrity checksums and indexes).
    let build_fp = fake_sha("perf-build");
    let mut legacy_entries = Vec::with_capacity(1_000);
    for i in 0..1_000usize {
        let goal = format!("Perf Goal {i:04}");
        let goal_key = format!("perf goal {i:04}");
        legacy_entries.push(serde_json::json!({
            "goal": goal,
            "goalKey": goal_key,
            "appId": "com.example.app",
            "platform": "ios",
            "flowSchemaVersion": "maestro/v1",
            "buildFingerprint": build_fp,
            "adapter": "ios-native",
            "flowPath": flow_path,
            "flowSha256": flow_sha,
            "subflowFiles": [],
            "lastValidation": { "passed": true, "at": "2026-05-01T12:00:00Z" },
            "validationHistory": [{ "passed": true, "at": "2026-05-01T12:00:00Z", "flowSha256": flow_sha }],
            "origin": "remember"
        }));
    }
    let seed_json = serde_json::to_string(&serde_json::json!({
        "schemaVersion": 3,
        "walks": legacy_entries,
    }))
    .unwrap();
    let seed_path = ws.write_file(".deja/memory/seed-1000.json", seed_json.as_bytes());
    let report = import_legacy_walks_json(&store, Some(&seed_path)).unwrap();
    assert_eq!(report.imported_walks, 1_000);

    // Verify SQLite query planner uses idx_walks_identity (never a full table scan!).
    {
        let raw = Connection::open(store.memory_path()).unwrap();
        let plan: String = raw
            .query_row(
                "EXPLAIN QUERY PLAN
                 SELECT id FROM walks
                 WHERE goal_key = ?1 AND app_id = ?2 AND platform = ?3
                   AND flow_schema_version = ?4 AND adapter_key = ?5 AND browser_key = ?6",
                rusqlite::params![
                    "perf goal 0777",
                    "com.example.app",
                    "ios",
                    "maestro/v1",
                    "ios-native",
                    ""
                ],
                |r| r.get(3),
            )
            .unwrap();
        assert!(
            plan.contains("idx_walks_identity"),
            "lookup must use idx_walks_identity, got plan: {plan}"
        );
    }

    let target = ios_identity("Perf Goal 0777", "perf-build");
    // Warm prepared statement cache.
    assert!(matches!(
        store.lookup(&target).unwrap(),
        LookupResult::Hit(_)
    ));

    let mut samples = Vec::with_capacity(50);
    for _ in 0..50 {
        let start = Instant::now();
        let hit = store.lookup(&target).unwrap();
        let elapsed = start.elapsed();
        assert!(matches!(hit, LookupResult::Hit(_)));
        samples.push(elapsed);
    }
    samples.sort();
    let median = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95) / 100];
    eprintln!(
        "[perf] 1000-walk SQLite v5 point lookup: median={:.3}ms, p95={:.3}ms",
        median.as_secs_f64() * 1_000.0,
        p95.as_secs_f64() * 1_000.0
    );
    assert!(
        median < Duration::from_millis(2),
        "expected 1000-walk median point lookup < 2ms, got {:?}",
        median
    );
}

#[test]
fn persistent_build_fingerprint_cache_zero_bytes_on_hit_and_instant_invalidation() {
    let ws = TempWorkspace::new("perf-fingerprint-cache");
    ws.write_file("App.app/Info.plist", b"<plist>v1</plist>");
    ws.write_file("App.app/Frameworks/Core.bin", &vec![0xABu8; 32_768]);
    let apk_path = ws.write_file("dist/app-debug.apk", &vec![0xCDu8; 16_384]);

    let expected_tree_v1 = fingerprint_tree(ws.path(), Path::new("App.app"), "iOS").unwrap();
    let expected_file_v1 = fingerprint_file(ws.path(), &apk_path).unwrap();

    // Cold call populates build_fingerprint_cache on disk.
    {
        let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
        let tree_cold = store
            .fingerprint_with_cache(ws.path(), Path::new("App.app"), "iOS")
            .unwrap();
        assert!(!tree_cold.cache_hit);
        assert!(tree_cold.bytes_read > 32_768);
        assert_eq!(tree_cold.digest, expected_tree_v1);

        let file_cold = store
            .fingerprint_with_cache(ws.path(), &apk_path, "Android")
            .unwrap();
        assert!(!file_cold.cache_hit);
        assert_eq!(file_cold.bytes_read, 16_384);
        assert_eq!(file_cold.digest, expected_file_v1);
    }

    // Re-open fresh store (simulating a new CLI invocation) -> persistent cache HIT with 0 bytes read!
    {
        let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
        let tree_warm = store
            .fingerprint_with_cache(ws.path(), Path::new("App.app"), "iOS")
            .unwrap();
        assert!(
            tree_warm.cache_hit,
            "unchanged build tree must hit persistent cache"
        );
        assert_eq!(tree_warm.bytes_read, 0, "cache hit must read 0 file bytes");
        assert_eq!(tree_warm.digest, expected_tree_v1);

        let file_warm = store
            .fingerprint_with_cache(ws.path(), &apk_path, "Android")
            .unwrap();
        assert!(
            file_warm.cache_hit,
            "unchanged build file must hit persistent cache"
        );
        assert_eq!(file_warm.bytes_read, 0);
        assert_eq!(file_warm.digest, expected_file_v1);

        // Modify a nested file inside App.app -> immediate cache invalidation!
        ws.write_file("App.app/Frameworks/Core.bin", &vec![0xEFu8; 32_800]);
        let expected_tree_v2 = fingerprint_tree(ws.path(), Path::new("App.app"), "iOS").unwrap();
        assert_ne!(expected_tree_v2, expected_tree_v1);

        let tree_after_edit = store
            .fingerprint_with_cache(ws.path(), Path::new("App.app"), "iOS")
            .unwrap();
        assert!(
            !tree_after_edit.cache_hit,
            "nested file modification must invalidate persistent cache"
        );
        assert!(tree_after_edit.bytes_read > 32_768);
        assert_eq!(tree_after_edit.digest, expected_tree_v2);

        // Next call hits the updated cache entry with 0 bytes read.
        let tree_warm_v2 = store
            .fingerprint_with_cache(ws.path(), Path::new("App.app"), "iOS")
            .unwrap();
        assert!(tree_warm_v2.cache_hit);
        assert_eq!(tree_warm_v2.bytes_read, 0);
        assert_eq!(tree_warm_v2.digest, expected_tree_v2);
    }
}

#[test]
fn legacy_sqlite_v3_upgrades_transactionally_to_v5_and_manifest_lock_and_gate_flow_changed() {
    // 1. Legacy SQLite v3 database -> automatic transactional upgrade to v5 on writable open.
    {
        let ws = TempWorkspace::new("sqlite-v3-upgrade");
        let (flow_path, flow_sha) = write_classic_flow(
            &ws,
            ".deja/flows/upgrade-me/ios-up.yaml",
            "appId: com.example.app\n---\n- assertVisible: Upgraded\n",
        );
        let db_path = ws.path().join(".deja/memory/walks.sqlite");
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        {
            let raw = Connection::open(&db_path).unwrap();
            raw.execute_batch(
                "CREATE TABLE walks (
                    id INTEGER PRIMARY KEY,
                    goal TEXT NOT NULL,
                    goal_key TEXT NOT NULL,
                    app_id TEXT NOT NULL,
                    platform TEXT NOT NULL,
                    flow_schema_version TEXT NOT NULL,
                    build_fingerprint TEXT NOT NULL,
                    adapter TEXT,
                    browser TEXT,
                    flow_path TEXT NOT NULL,
                    flow_sha256 TEXT NOT NULL,
                    last_passed INTEGER NOT NULL,
                    last_at TEXT NOT NULL,
                    origin TEXT,
                    exploration_json TEXT,
                    has_files INTEGER NOT NULL DEFAULT 1,
                    legacy_closure INTEGER NOT NULL DEFAULT 0,
                    closure_sha256 TEXT
                );
                CREATE TABLE validation_history (
                    walk_id INTEGER NOT NULL,
                    position INTEGER NOT NULL,
                    passed INTEGER NOT NULL,
                    at TEXT NOT NULL,
                    flow_sha256 TEXT NOT NULL,
                    PRIMARY KEY (walk_id, position)
                );
                CREATE TABLE walk_files (
                    walk_id INTEGER NOT NULL,
                    position INTEGER NOT NULL,
                    path TEXT NOT NULL,
                    sha256 TEXT NOT NULL,
                    PRIMARY KEY (walk_id, position)
                );
                CREATE TABLE repair_history (
                    walk_id INTEGER NOT NULL,
                    position INTEGER NOT NULL,
                    attempt_id TEXT NOT NULL,
                    patch_path TEXT NOT NULL,
                    previous_build_fingerprint TEXT NOT NULL,
                    previous_flow_sha256 TEXT NOT NULL,
                    PRIMARY KEY (walk_id, position)
                );
                PRAGMA user_version = 3;",
            )
            .unwrap();
            let build = fake_sha("v3-build");
            raw.execute(
                "INSERT INTO walks (
                    id, goal, goal_key, app_id, platform, flow_schema_version, build_fingerprint,
                    adapter, browser, flow_path, flow_sha256, last_passed, last_at, origin,
                    has_files, legacy_closure
                ) VALUES (1, 'Upgrade Me', 'upgrade me', 'com.example.app', 'ios', 'maestro/v1',
                          ?1, 'ios-native', NULL, ?2, ?3, 1, '2026-04-10T09:00:00Z', 'remember', 1, 0)",
                rusqlite::params![build, flow_path, flow_sha],
            )
            .unwrap();
            raw.execute(
                "INSERT INTO validation_history (walk_id, position, passed, at, flow_sha256)
                 VALUES (1, 0, 1, '2026-04-10T09:00:00Z', ?1)",
                [&flow_sha],
            )
            .unwrap();
        }

        // Read-only refuses un-upgraded v3; writable upgrades to v5 and serves exact Hit.
        assert!(matches!(
            SqliteWalkStore::open_readonly(SqliteStoreOptions::new(ws.path())),
            Err(StoreError::InvalidWalkMemory(_))
        ));
        let upgraded = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
        let hit = upgraded
            .lookup(&ios_identity("Upgrade Me", "v3-build"))
            .unwrap();
        assert!(matches!(hit, LookupResult::Hit(_)));
    }

    // 2. Manifest lock contention (exit 6), forbidden value refusal, and manifest-mode gate FlowChanged.
    {
        let ws = TempWorkspace::new("manifest-lock-and-gate");
        let flow_rel = ".maestro/flows/home.yaml";
        let flow_abs = ws.write_file(
            flow_rel,
            b"appId: com.example.app\n---\n- assertVisible:\n    id: home_title\n",
        );
        let closure = collect_flow_closure(ws.path(), &flow_abs).unwrap();
        let assert_sha = assertions_sha256(&closure).unwrap();

        let consumer = ConsumerFlows::new(
            ConsumerFlowsOptions::new(ws.path(), "deja.walks.json")
                .with_forbidden_values(vec!["top-secret-token".to_string()])
                .with_lock_retries(0),
        )
        .unwrap();
        let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();

        // Forbidden value in goal refuses before writing manifest.
        let secret_err = consumer
            .save_verified_manifest_walk(
                &store,
                &SaveManifestWalkOptions {
                    identity: ios_identity("Use top-secret-token to sign in", "b1"),
                    flow_path: flow_rel.to_string(),
                    expected_closure_sha256: closure.sha256.clone(),
                    expected_assertions_sha256: Some(assert_sha.clone()),
                    validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z")
                        .with_origin(WalkOrigin::Remember),
                    accept_assertion_change: false,
                },
            )
            .expect_err("forbidden value in manifest must be rejected");
        assert_eq!(secret_err.exit_code(), 2);

        // Held .deja/manifest.lock -> ManifestLocked (exit 6).
        let manifest_lock_path = fs::canonicalize(ws.path())
            .unwrap()
            .join(".deja/manifest.lock");
        let _held = WriterLock::acquire(manifest_lock_path).unwrap();
        let lock_err = consumer
            .save_verified_manifest_walk(
                &store,
                &SaveManifestWalkOptions {
                    identity: ios_identity("Open home", "b1"),
                    flow_path: flow_rel.to_string(),
                    expected_closure_sha256: closure.sha256.clone(),
                    expected_assertions_sha256: Some(assert_sha.clone()),
                    validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z")
                        .with_origin(WalkOrigin::Remember),
                    accept_assertion_change: false,
                },
            )
            .expect_err("held manifest.lock must fail with ManifestLocked");
        assert!(matches!(lock_err, StoreError::ManifestLocked { .. }));
        assert_eq!(lock_err.exit_code(), 6);
        drop(_held);

        // Save cleanly, pass gate, then mutate consumer flow -> gate returns FlowChanged.
        consumer
            .save_verified_manifest_walk(
                &store,
                &SaveManifestWalkOptions {
                    identity: ios_identity("Open home", "b1"),
                    flow_path: flow_rel.to_string(),
                    expected_closure_sha256: closure.sha256.clone(),
                    expected_assertions_sha256: Some(assert_sha.clone()),
                    validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z")
                        .with_origin(WalkOrigin::Remember),
                    accept_assertion_change: false,
                },
            )
            .unwrap();
        let gate_id = GateIdentity {
            app_id: "com.example.app".to_string(),
            platform: "ios".to_string(),
            flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
            build_fingerprint: fake_sha("b1"),
            adapter: Some("ios-native".to_string()),
            browser: None,
        };
        let ro = SqliteWalkStore::open_readonly(SqliteStoreOptions::new(ws.path())).unwrap();
        let ok_gate = evaluate_gate(
            &[&ro],
            &["Open home".to_string()],
            &gate_id,
            &StoreGateOptions {
                consumer: Some(&consumer),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ok_gate[0].outcome, GateOutcome::Passed);
        assert_eq!(
            ok_gate[0].closure_sha256.as_deref(),
            Some(closure.sha256.as_str())
        );
        let _ = assert_sha;

        // Mutate disk flow -> manifest gate immediately fails closed with FlowChanged.
        ws.write_file(
            flow_rel,
            b"appId: com.example.app\n---\n- assertVisible:\n    id: home_title_edited\n",
        );
        let changed_gate = evaluate_gate(
            &[&ro],
            &["Open home".to_string()],
            &gate_id,
            &StoreGateOptions {
                consumer: Some(&consumer),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(changed_gate[0].outcome, GateOutcome::FlowChanged);
    }
}

// =============================================================================================
// 9. Codex Round 1 Regression Tests (Findings 1 through 12)

fn relative_path_from(base: &Path, target: &Path) -> PathBuf {
    let base_parts: Vec<_> = base.components().collect();
    let target_parts: Vec<_> = target.components().collect();
    let common = base_parts
        .iter()
        .zip(target_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut rel = PathBuf::new();
    for _ in common..base_parts.len() {
        rel.push("..");
    }
    for comp in &target_parts[common..] {
        rel.push(comp.as_os_str());
    }
    rel
}

#[test]
fn regression_r1_f1_f4_f7_f12_manifest_closure_binding_failed_rejection_merge_and_missing_flow() {
    let ws = TempWorkspace::new("r1-manifest-checks");
    let main_rel = ".maestro/flows/pay.yaml";
    let sub_rel = ".maestro/flows/pay_sub.yaml";
    let main_abs = ws.write_file(
        main_rel,
        b"appId: com.example.app\n---\n- runFlow: pay_sub.yaml\n- assertVisible:\n    id: paid\n",
    );
    ws.write_file(
        sub_rel,
        b"appId: com.example.app\n---\n- tapOn:\n    id: confirm_btn\n",
    );
    let initial_closure = collect_flow_closure(ws.path(), &main_abs).unwrap();
    let initial_assert = assertions_sha256(&initial_closure).unwrap();

    let consumer =
        ConsumerFlows::new(ConsumerFlowsOptions::new(ws.path(), "deja.walks.json")).unwrap();
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
    let id = ios_identity("Pay order", "build-pay-1");

    // Finding 4: Failed validation MUST be rejected before touching SQLite or deja.walks.json.
    let fail_err = consumer
        .save_verified_manifest_walk(
            &store,
            &SaveManifestWalkOptions {
                identity: id.clone(),
                flow_path: main_rel.to_string(),
                expected_closure_sha256: initial_closure.sha256.clone(),
                expected_assertions_sha256: Some(initial_assert.clone()),
                validation: RecordValidationInput::new(false, "2026-05-01T10:00:00Z")
                    .with_origin(WalkOrigin::Remember),
                accept_assertion_change: false,
            },
        )
        .expect_err("failed validation must be rejected before manifest update");
    assert!(matches!(fail_err, StoreError::ConsumerFlow(_)));
    assert_eq!(fail_err.exit_code(), 2);
    assert!(store.list_walks().unwrap().is_empty());
    assert!(!ws.path().join("deja.walks.json").exists());

    // Finding 1: Post-verify edit to subflow breaks expected_closure_sha256 -> ManifestChanged (exit 6), zero writes!
    ws.write_file(
        sub_rel,
        b"appId: com.example.app\n---\n- tapOn:\n    id: confirm_btn_tampered\n",
    );
    let drift_err = consumer
        .save_verified_manifest_walk(
            &store,
            &SaveManifestWalkOptions {
                identity: id.clone(),
                flow_path: main_rel.to_string(),
                expected_closure_sha256: initial_closure.sha256.clone(),
                expected_assertions_sha256: Some(initial_assert.clone()),
                validation: RecordValidationInput::new(true, "2026-05-01T10:01:00Z")
                    .with_origin(WalkOrigin::Verify),
                accept_assertion_change: false,
            },
        )
        .expect_err("edited flow after verify must fail closed with ManifestChanged");
    assert!(matches!(drift_err, StoreError::ManifestChanged(_)));
    assert_eq!(drift_err.exit_code(), 6);
    assert!(store.list_walks().unwrap().is_empty());
    assert!(!ws.path().join("deja.walks.json").exists());

    // Restore subflow and save cleanly, while injecting a concurrent edit to an UNRELATED goal (Finding 7).
    ws.write_file(
        sub_rel,
        b"appId: com.example.app\n---\n- tapOn:\n    id: confirm_btn\n",
    );
    let other_flow_abs = ws.write_file(
        ".maestro/flows/other.yaml",
        b"appId: com.example.app\n---\n- assertVisible:\n    id: other_ok\n",
    );
    let other_closure = collect_flow_closure(ws.path(), &other_flow_abs).unwrap();
    let other_assert = assertions_sha256(&other_closure).unwrap();
    let other_manifest_text = serialize_walk_manifest(&WalkManifest {
        manifest_version: 1,
        hash_version: 1,
        walks: vec![ManifestWalk {
            goal: "Unrelated Goal".to_string(),
            app_id: "com.example.app".to_string(),
            platform: "ios".to_string(),
            adapter: "ios-native".to_string(),
            browser: None,
            flow: ".maestro/flows/other.yaml".to_string(),
            closure_sha256: other_closure.sha256,
            assertions_sha256: other_assert,
            owner_rules: None,
            profile: None,
        }],
    })
    .unwrap();
    let injected_text = other_manifest_text.clone();
    let hook = Arc::new(move |manifest_path: &Path| {
        fs::write(manifest_path, injected_text.as_bytes()).expect("inject concurrent goal");
    });

    consumer
        .save_verified_manifest_walk_with_hook(
            &store,
            &SaveManifestWalkOptions {
                identity: id.clone(),
                flow_path: main_rel.to_string(),
                expected_closure_sha256: initial_closure.sha256.clone(),
                expected_assertions_sha256: Some(initial_assert.clone()),
                validation: RecordValidationInput::new(true, "2026-05-01T10:02:00Z")
                    .with_origin(WalkOrigin::Remember),
                accept_assertion_change: false,
            },
            Some(hook),
        )
        .expect("concurrent edit to unrelated goal must merge cleanly");

    let merged = consumer.load().unwrap();
    assert_eq!(merged.walks.len(), 2);
    assert!(merged.walks.iter().any(|w| w.goal == "Pay order"));
    assert!(merged.walks.iter().any(|w| w.goal == "Unrelated Goal"));

    // Finding 12: Missing subflow (and missing main flow) in plan_run -> FlowChanged + MissingFlow + exit 6!
    fs::remove_file(ws.path().join(sub_rel)).unwrap();
    let missing_sub_plan = consumer
        .plan_run(
            &store,
            &ConsumerRunRequest {
                identity: &id,
                runtime: None,
                stale_build: Some(StaleBuildPolicy::ReplayLastVerified),
            },
        )
        .unwrap();
    assert!(matches!(
        missing_sub_plan,
        ConsumerRunPlan::Refuse {
            outcome: RunOutcome::FlowChanged,
            miss_reason: CacheMissReason::MissingFlow,
            exit_code: 6,
            ..
        }
    ));

    fs::remove_file(&main_abs).unwrap();
    let missing_main_plan = consumer
        .plan_run(
            &store,
            &ConsumerRunRequest {
                identity: &id,
                runtime: None,
                stale_build: Some(StaleBuildPolicy::ReplayLastVerified),
            },
        )
        .unwrap();
    assert!(matches!(
        missing_main_plan,
        ConsumerRunPlan::Refuse {
            outcome: RunOutcome::FlowChanged,
            miss_reason: CacheMissReason::MissingFlow,
            exit_code: 6,
            ..
        }
    ));
}

#[test]
fn regression_r1_f2_gate_without_consumer_verifies_full_consumer_closure() {
    let ws = TempWorkspace::new("r1-full-consumer-closure");
    let main_rel = ".maestro/flows/checkout.yaml";
    let sub_rel = ".maestro/flows/step.yaml";
    let main_abs = ws.write_file(
        main_rel,
        b"appId: com.example.app\n---\n- runFlow: step.yaml\n- assertVisible:\n    id: done\n",
    );
    ws.write_file(
        sub_rel,
        b"appId: com.example.app\n---\n- tapOn:\n    id: next_btn\n",
    );
    let closure = collect_flow_closure(ws.path(), &main_abs).unwrap();
    let main_sha = closure
        .files
        .iter()
        .find(|f| f.path == closure.main)
        .unwrap()
        .sha256
        .clone();

    let id = ios_identity("Checkout Closure", "build-closure-1");
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
    store
        .upsert_walk(&UpsertWalkInput {
            identity: id.clone(),
            flow_path: main_rel.to_string(),
            flow_sha256: main_sha,
            subflow_files: None,
            legacy_closure: false,
            closure_sha256: Some(closure.sha256),
            validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z"),
            exploration: None,
            repair_history: None,
        })
        .unwrap();
    store.close();

    let gate_id = GateIdentity {
        app_id: "com.example.app".to_string(),
        platform: "ios".to_string(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint: fake_sha("build-closure-1"),
        adapter: Some("ios-native".to_string()),
        browser: None,
    };
    let ro = SqliteWalkStore::open_readonly(SqliteStoreOptions::new(ws.path())).unwrap();

    // Unchanged subflow -> Passed!
    let pass = evaluate_gate(
        &[&ro],
        &["Checkout Closure".to_string()],
        &gate_id,
        &StoreGateOptions::default(),
    )
    .unwrap();
    assert_eq!(pass[0].outcome, GateOutcome::Passed);

    // Edit ONLY subflow (main YAML unchanged!) -> gate without consumer MUST report FlowChanged!
    ws.write_file(
        sub_rel,
        b"appId: com.example.app\n---\n- tapOn:\n    id: next_btn_changed\n",
    );
    let edited = evaluate_gate(
        &[&ro],
        &["Checkout Closure".to_string()],
        &gate_id,
        &StoreGateOptions::default(),
    )
    .unwrap();
    assert_eq!(edited[0].outcome, GateOutcome::FlowChanged);
    assert!(matches!(
        ro.lookup(&id).unwrap(),
        LookupResult::Miss(m) if m.reason == CacheMissReason::FlowChanged
    ));

    // Delete ONLY subflow -> gate without consumer MUST report FlowChanged!
    fs::remove_file(ws.path().join(sub_rel)).unwrap();
    let deleted = evaluate_gate(
        &[&ro],
        &["Checkout Closure".to_string()],
        &gate_id,
        &StoreGateOptions::default(),
    )
    .unwrap();
    assert_eq!(deleted[0].outcome, GateOutcome::FlowChanged);
}

#[test]
fn regression_r1_f3_relative_workspace_root_never_double_joins() {
    let ws = TempWorkspace::new("r1-relative-ws");
    let canonical_ws = fs::canonicalize(ws.path()).unwrap();
    let cwd = fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
    let rel_ws = relative_path_from(&cwd, &canonical_ws);
    assert!(rel_ws.is_relative());

    let (flow_path, flow_sha) = write_classic_flow(
        &ws,
        ".deja/flows/rel-goal/ios-rel.yaml",
        "appId: com.example.app\n---\n- assertVisible: Relative\n",
    );
    let expected_db = canonical_ws.join(".deja/memory/walks.sqlite");

    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(&rel_ws)).unwrap();
    assert_eq!(store.memory_path(), expected_db);
    assert!(expected_db.exists());
    // Ensure NO nested duplicate directory was created!
    let ws_name = canonical_ws.file_name().unwrap();
    assert!(!canonical_ws.join(ws_name).exists());

    let id = ios_identity("Rel Goal", "build-rel");
    store
        .upsert_walk(&UpsertWalkInput {
            identity: id.clone(),
            flow_path: flow_path.clone(),
            flow_sha256: flow_sha.clone(),
            subflow_files: Some(vec![]),
            legacy_closure: false,
            closure_sha256: None,
            validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z"),
            exploration: None,
            repair_history: None,
        })
        .unwrap();

    // Also test legacy walks.json import with relative workspace_root!
    let legacy_json = serde_json::to_string(&serde_json::json!({
        "schemaVersion": 3,
        "walks": [{
            "goal": "Rel Goal Two",
            "goalKey": "rel goal two",
            "appId": "com.example.app",
            "platform": "ios",
            "flowSchemaVersion": "maestro/v1",
            "buildFingerprint": fake_sha("build-rel-2"),
            "adapter": "ios-native",
            "flowPath": flow_path,
            "flowSha256": flow_sha,
            "subflowFiles": [],
            "lastValidation": { "passed": true, "at": "2026-05-01T10:05:00Z" },
            "validationHistory": [{ "passed": true, "at": "2026-05-01T10:05:00Z", "flowSha256": flow_sha }],
            "origin": "remember"
        }]
    }))
    .unwrap();
    ws.write_file(LEGACY_JSON_MEMORY_PATH, legacy_json.as_bytes());
    let imported = import_legacy_walks_json(&store, None).expect("relative workspace import");
    assert_eq!(imported.imported_walks, 1);
    store.close();

    // Read-only open via relative workspace root sees both walks!
    let ro = SqliteWalkStore::open_readonly(SqliteStoreOptions::new(&rel_ws)).unwrap();
    assert_eq!(ro.memory_path(), expected_db);
    assert_eq!(ro.list_walks().unwrap().len(), 2);
}

#[test]
fn regression_r1_f5_legacy_sqlite_upgrade_rejects_last_validation_history_mismatch() {
    let ws = TempWorkspace::new("r1-legacy-mismatch");
    let db_path = ws.path().join(".deja/memory/walks.sqlite");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let flow_sha = fake_sha("flow-sha");
    {
        let raw = Connection::open(&db_path).unwrap();
        raw.execute_batch(
            "CREATE TABLE walks (
                id INTEGER PRIMARY KEY, goal TEXT NOT NULL, goal_key TEXT NOT NULL,
                app_id TEXT NOT NULL, platform TEXT NOT NULL, flow_schema_version TEXT NOT NULL,
                build_fingerprint TEXT NOT NULL, adapter TEXT, browser TEXT,
                flow_path TEXT NOT NULL, flow_sha256 TEXT NOT NULL,
                last_passed INTEGER NOT NULL, last_at TEXT NOT NULL,
                origin TEXT, exploration_json TEXT,
                has_files INTEGER NOT NULL DEFAULT 1, legacy_closure INTEGER NOT NULL DEFAULT 0,
                closure_sha256 TEXT
            );
            CREATE TABLE validation_history (
                walk_id INTEGER NOT NULL, position INTEGER NOT NULL,
                passed INTEGER NOT NULL, at TEXT NOT NULL, flow_sha256 TEXT NOT NULL,
                PRIMARY KEY (walk_id, position)
            );
            CREATE TABLE walk_files (
                walk_id INTEGER NOT NULL, position INTEGER NOT NULL,
                path TEXT NOT NULL, sha256 TEXT NOT NULL, PRIMARY KEY (walk_id, position)
            );
            CREATE TABLE repair_history (
                walk_id INTEGER NOT NULL, position INTEGER NOT NULL,
                attempt_id TEXT NOT NULL, patch_path TEXT NOT NULL,
                previous_build_fingerprint TEXT NOT NULL, previous_flow_sha256 TEXT NOT NULL,
                PRIMARY KEY (walk_id, position)
            );
            PRAGMA user_version = 3;",
        )
        .unwrap();
        // Summary claims passed=1, but latest history row has passed=0!
        raw.execute(
            "INSERT INTO walks (
                id, goal, goal_key, app_id, platform, flow_schema_version, build_fingerprint,
                adapter, browser, flow_path, flow_sha256, last_passed, last_at, origin,
                has_files, legacy_closure
            ) VALUES (1, 'Bad Legacy', 'bad legacy', 'com.example.app', 'ios', 'maestro/v1',
                      ?1, 'ios-native', NULL, '.deja/flows/x/ios.yaml', ?2, 1,
                      '2026-04-10T09:00:00Z', 'remember', 1, 0)",
            rusqlite::params![fake_sha("b-legacy"), flow_sha],
        )
        .unwrap();
        raw.execute(
            "INSERT INTO validation_history (walk_id, position, passed, at, flow_sha256)
             VALUES (1, 0, 0, '2026-04-10T09:00:00Z', ?1)",
            [&flow_sha],
        )
        .unwrap();
    }

    let err = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path()))
        .expect_err("mismatched legacy summary vs history must be rejected");
    assert!(matches!(err, StoreError::InvalidWalkMemory(_)));
    assert_eq!(err.exit_code(), 4);

    // Verify legacy DB was NOT altered (still user_version = 3).
    let raw = Connection::open(&db_path).unwrap();
    let uv: u32 = raw
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(uv, 3);
}

#[test]
fn regression_r1_f6_readonly_open_creates_zero_wal_shm_sidecars_and_sees_subsequent_writes() {
    let ws = TempWorkspace::new("r1-readonly-no-sidecars");
    let (flow_path, flow_sha) = write_classic_flow(
        &ws,
        ".deja/flows/clean/ios-clean.yaml",
        "appId: com.example.app\n---\n- assertVisible: Clean\n",
    );
    let id = ios_identity("Clean ReadOnly", "build-ro-1");
    let db_path = {
        let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
        store
            .upsert_walk(&UpsertWalkInput {
                identity: id.clone(),
                flow_path,
                flow_sha256: flow_sha,
                subflow_files: Some(vec![]),
                legacy_closure: false,
                closure_sha256: None,
                validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z"),
                exploration: None,
                repair_history: None,
            })
            .unwrap();
        let path = store.memory_path().to_path_buf();
        store.close();
        path
    };

    let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
    let shm_path = PathBuf::from(format!("{}-shm", db_path.display()));
    assert!(
        !wal_path.exists(),
        "cleanly closed store should not leave -wal"
    );
    assert!(
        !shm_path.exists(),
        "cleanly closed store should not leave -shm"
    );

    // Open read-only, perform lookup and evaluate_gate -> ZERO -wal or -shm created!
    let ro = SqliteWalkStore::open_readonly(SqliteStoreOptions::new(ws.path())).unwrap();
    assert!(matches!(ro.lookup(&id).unwrap(), LookupResult::Hit(_)));
    let gate_id = GateIdentity {
        app_id: "com.example.app".to_string(),
        platform: "ios".to_string(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint: fake_sha("build-ro-1"),
        adapter: Some("ios-native".to_string()),
        browser: None,
    };
    let gate_res = evaluate_gate(
        &[&ro],
        &["Clean ReadOnly".to_string()],
        &gate_id,
        &StoreGateOptions::default(),
    )
    .unwrap();
    assert_eq!(gate_res[0].outcome, GateOutcome::Passed);
    assert!(!wal_path.exists(), "open_readonly must NOT create -wal");
    assert!(!shm_path.exists(), "open_readonly must NOT create -shm");

    // Subsequent live write (creating -wal) is immediately visible to the existing ro handle!
    let writer = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
    writer
        .record_validation(
            &id,
            &RecordValidationInput::new(false, "2026-05-01T10:05:00Z"),
        )
        .unwrap();
    assert!(matches!(
        ro.lookup(&id).unwrap(),
        LookupResult::Miss(m) if m.reason == CacheMissReason::FailedValidation
    ));
    writer.close();
    // And after writer closes cleanly, ro still sees the failed validation.
    assert!(matches!(
        ro.lookup(&id).unwrap(),
        LookupResult::Miss(m) if m.reason == CacheMissReason::FailedValidation
    ));
}

#[test]
fn regression_r1_f8_f9_revalidation_mode_switch_and_legacy_import_conflict_clears_telemetry() {
    let ws = TempWorkspace::new("r1-reval-and-import-conflict");
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();

    // Finding 8: Build 1 is manifest-backed, Build 2 of the SAME logical walk is classic.
    let manifest_rel = ".maestro/flows/mixed.yaml";
    let manifest_abs = ws.write_file(
        manifest_rel,
        b"appId: com.example.app\n---\n- assertVisible: MixedManifest\n",
    );
    let m_closure = collect_flow_closure(ws.path(), &manifest_abs).unwrap();
    let m_main_sha = m_closure
        .files
        .iter()
        .find(|f| f.path == m_closure.main)
        .unwrap()
        .sha256
        .clone();

    let (classic_rel, classic_sha) = write_classic_flow(
        &ws,
        ".deja/flows/mixed-goal/ios-classic.yaml",
        "appId: com.example.app\n---\n- assertVisible: MixedClassic\n",
    );

    let id_b1 = ios_identity("Mixed Goal", "mixed-b1");
    let id_b2 = ios_identity("Mixed Goal", "mixed-b2");

    // Upsert build 1 (manifest-backed: has_files=0, closure_sha256=Some).
    store
        .upsert_walk(&UpsertWalkInput {
            identity: id_b1.clone(),
            flow_path: manifest_rel.to_string(),
            flow_sha256: m_main_sha,
            subflow_files: None,
            legacy_closure: false,
            closure_sha256: Some(m_closure.sha256.clone()),
            validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z"),
            exploration: None,
            repair_history: None,
        })
        .unwrap();

    // Upsert build 2 (classic: has_files=1, closure_sha256=None).
    store
        .upsert_walk(&UpsertWalkInput {
            identity: id_b2.clone(),
            flow_path: classic_rel.clone(),
            flow_sha256: classic_sha.clone(),
            subflow_files: Some(vec![]),
            legacy_closure: false,
            closure_sha256: None,
            validation: RecordValidationInput::new(true, "2026-05-01T10:01:00Z")
                .with_failure_category("selector")
                .with_report_path(".deja/reports/r.json")
                .with_duration_ms(777)
                .with_phase_timings_json(r#"{"step":1}"#),
            exploration: None,
            repair_history: None,
        })
        .unwrap();

    // Now revalidate older manifest-backed build 1 -> walks row MUST remain valid and consistent!
    store
        .record_validation(
            &id_b1,
            &RecordValidationInput::new(true, "2026-05-01T10:02:00Z"),
        )
        .expect(
            "revalidating manifest-backed build after classic build must keep walks consistent",
        );
    let listed = store.list_walks().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].closure_sha256.as_deref(),
        Some(m_closure.sha256.as_str())
    );
    assert!(listed[0].subflow_files.is_none());

    // Revalidate classic build 2 -> walks row cleanly switches back to classic!
    store
        .record_validation(
            &id_b2,
            &RecordValidationInput::new(true, "2026-05-01T10:03:00Z")
                .with_failure_category("runner")
                .with_report_path(".deja/reports/r2.json")
                .with_duration_ms(888)
                .with_phase_timings_json(r#"{"step":2}"#),
        )
        .expect("revalidating classic build after manifest build must keep walks consistent");
    let listed2 = store.list_walks().unwrap();
    assert!(listed2[0].closure_sha256.is_none());
    assert!(listed2[0].subflow_files.is_some());

    // Finding 9: Legacy walks.json import conflicting on build 2 clears non-null telemetry columns
    // in sync with the recomputed integrity_sha256 (subsequent reads must succeed!).
    let legacy_overwrite = serde_json::to_string(&serde_json::json!({
        "schemaVersion": 3,
        "walks": [{
            "goal": "Mixed Goal",
            "goalKey": "mixed goal",
            "appId": "com.example.app",
            "platform": "ios",
            "flowSchemaVersion": "maestro/v1",
            "buildFingerprint": fake_sha("mixed-b2"),
            "adapter": "ios-native",
            "flowPath": classic_rel,
            "flowSha256": classic_sha,
            "subflowFiles": [],
            "lastValidation": { "passed": true, "at": "2026-05-01T10:04:00Z" },
            "validationHistory": [{ "passed": true, "at": "2026-05-01T10:04:00Z", "flowSha256": classic_sha }],
            "origin": "remember"
        }]
    }))
    .unwrap();
    let json_path = ws.write_file(".deja/memory/overwrite.json", legacy_overwrite.as_bytes());
    import_legacy_walks_json(&store, Some(&json_path)).unwrap();

    // Must NOT fail with InvalidWalkMemory!
    assert!(matches!(
        store.lookup(&id_b2).unwrap(),
        LookupResult::Hit(_)
    ));
    let runs = store.list_validation_runs(&id_b2).unwrap();
    let b2_run = runs
        .iter()
        .find(|r| r.build_fingerprint == fake_sha("mixed-b2"))
        .unwrap();
    assert!(b2_run.failure_category.is_none());
    assert!(b2_run.report_path.is_none());
    assert!(b2_run.duration_ms.is_none());
    assert!(b2_run.phase_timings_json.is_none());
}

#[cfg(unix)]
#[test]
fn regression_r1_f10_f11_symlinked_build_path_and_symlinked_workspace_alias() {
    use std::os::unix::fs::symlink;

    // Finding 10: Symlinked build path hashes the exact canonical target snapshotted by the cache.
    {
        let ws = TempWorkspace::new("r1-symlink-build");
        let v1 = ws.write_file("builds/app-v1.apk", b"apk-v1-bytes");
        let v2 = ws.write_file("builds/app-v2.apk", b"apk-v2-different-bytes");
        let link = ws.path().join("builds/latest.apk");
        symlink(&v1, &link).unwrap();

        let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
        let fp1 = store
            .fingerprint_with_cache(ws.path(), Path::new("builds/latest.apk"), "Android")
            .unwrap();
        assert!(!fp1.cache_hit);
        assert_eq!(fp1.digest, fingerprint_file(ws.path(), &v1).unwrap());

        let fp1_hit = store
            .fingerprint_with_cache(ws.path(), Path::new("builds/latest.apk"), "Android")
            .unwrap();
        assert!(fp1_hit.cache_hit);
        assert_eq!(fp1_hit.bytes_read, 0);
        assert_eq!(fp1_hit.digest, fp1.digest);

        // Retarget symlink to v2 -> misses v2 and does not poison v1's cache entry.
        fs::remove_file(&link).unwrap();
        symlink(&v2, &link).unwrap();
        let fp2 = store
            .fingerprint_with_cache(ws.path(), Path::new("builds/latest.apk"), "Android")
            .unwrap();
        assert!(!fp2.cache_hit);
        assert_eq!(fp2.digest, fingerprint_file(ws.path(), &v2).unwrap());
        assert_ne!(fp2.digest, fp1.digest);

        // Original v1 still hits cleanly under its canonical path.
        let v1_still_valid = store
            .fingerprint_with_cache(ws.path(), &v1, "Android")
            .unwrap();
        assert!(v1_still_valid.cache_hit);
        assert_eq!(v1_still_valid.digest, fp1.digest);
    }

    // Finding 11: Symlinked workspace root alias (like /tmp -> /private/tmp) preserves candidate closures.
    {
        let real_ws = TempWorkspace::new("r1-alias-real");
        let alias_holder = TempWorkspace::new("r1-alias-holder");
        let alias_ws = alias_holder.path().join("workspace-link");
        symlink(real_ws.path(), &alias_ws).unwrap();

        real_ws.write_file(
            "flows/main.yaml",
            b"appId: com.example.app\n---\n- runFlow: sub.yaml\n- assertVisible: Aliased\n",
        );
        real_ws.write_file(
            "flows/sub.yaml",
            b"appId: com.example.app\n---\n- tapOn: Go\n",
        );

        let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(&alias_ws)).unwrap();
        let aliased_main = alias_ws.join("flows/main.yaml");

        let snap = store
            .snapshot_candidate(&aliased_main)
            .expect("snapshot_candidate must accept aliased workspace path");
        assert_eq!(snap.files.len(), 2);

        let id = ios_identity("Aliased Flow", "build-alias");
        store
            .persist_verified_candidate(
                &id,
                &PersistVerifiedCandidateOptions {
                    candidate_path: aliased_main,
                    validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z"),
                    expected_flow_sha256: Some(snap.flow_sha256),
                    expected_closure_sha256: Some(snap.closure_sha256),
                    exploration: None,
                    repair_history: None,
                },
            )
            .expect("persist_verified_candidate must accept aliased workspace path");

        assert!(matches!(store.lookup(&id).unwrap(), LookupResult::Hit(_)));
    }
}

// =============================================================================================
// 10. Codex Round 2 Regression Tests (Findings 1 through 7)

#[test]
fn regression_r2_f1_f3_post_run_closure_binding_and_manifest_conflict_preserves_prior_validation() {
    let ws = TempWorkspace::new("r2-post-run-and-conflict");
    let flow_rel = ".maestro/flows/r2.yaml";
    let flow_v1 =
        "appId: com.example.app\n---\n- tapOn:\n    id: btn_v1\n- assertVisible:\n    id: ok_v1\n";
    let flow_abs = ws.write_file(flow_rel, flow_v1.as_bytes());
    let closure_v1 = collect_flow_closure(ws.path(), &flow_abs).unwrap();
    let assert_v1 = assertions_sha256(&closure_v1).unwrap();

    let consumer =
        ConsumerFlows::new(ConsumerFlowsOptions::new(ws.path(), "deja.walks.json")).unwrap();
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
    let id_b1 = ios_identity("R2 Flow", "r2-b1");

    // Persist initial verified v1 in manifest + SQLite.
    consumer
        .save_verified_manifest_walk(
            &store,
            &SaveManifestWalkOptions {
                identity: id_b1.clone(),
                flow_path: flow_rel.to_string(),
                expected_closure_sha256: closure_v1.sha256.clone(),
                expected_assertions_sha256: Some(assert_v1.clone()),
                validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z")
                    .with_origin(WalkOrigin::Remember),
                accept_assertion_change: false,
            },
        )
        .unwrap();

    // Finding 1: Suppose a run started at v1, and DURING execution both the flow AND deja.walks.json
    // advanced to v2 (so disk matches the NEW manifest, NOT the closure that actually ran!).
    let flow_v2 =
        "appId: com.example.app\n---\n- tapOn:\n    id: btn_v2\n- assertVisible:\n    id: ok_v1\n";
    ws.write_file(flow_rel, flow_v2.as_bytes());
    let closure_v2 = collect_flow_closure(ws.path(), &flow_abs).unwrap();
    let assert_v2 = assertions_sha256(&closure_v2).unwrap();
    let manifest_v2 = WalkManifest {
        manifest_version: 1,
        hash_version: 1,
        walks: vec![ManifestWalk {
            goal: "R2 Flow".to_string(),
            app_id: "com.example.app".to_string(),
            platform: "ios".to_string(),
            adapter: "ios-native".to_string(),
            browser: None,
            flow: flow_rel.to_string(),
            closure_sha256: closure_v2.sha256.clone(),
            assertions_sha256: assert_v2.clone(),
            owner_rules: None,
            profile: None,
        }],
    };
    ws.write_file(
        "deja.walks.json",
        serialize_walk_manifest(&manifest_v2).unwrap().as_bytes(),
    );

    let id_b2 = ios_identity("R2 Flow", "r2-b2");
    let drift_err = consumer
        .record_post_run_validation(
            &store,
            &PostRunValidationOptions {
                identity: id_b2.clone(),
                expected_closure_sha256: closure_v1.sha256.clone(),
                expected_assertions_sha256: assert_v1.clone(),
                validation: RecordValidationInput::new(true, "2026-05-01T10:05:00Z"),
            },
        )
        .expect_err("concurrent manifest+flow update to v2 must reject post-run v1 validation");
    assert!(matches!(drift_err, StoreError::ManifestChanged(_)));
    assert_eq!(drift_err.exit_code(), 6);
    // Build 2 must NOT have been written into SQLite!
    assert_eq!(store.list_validation_runs(&id_b1).unwrap().len(), 1);

    // Finding 3: Conflict on the SAME goal during save_verified_manifest_walk rolls back SQLite
    // and preserves Build 1's prior validation untouched!
    let flow_v3 =
        "appId: com.example.app\n---\n- tapOn:\n    id: btn_v3\n- assertVisible:\n    id: ok_v1\n";
    ws.write_file(flow_rel, flow_v3.as_bytes());
    let closure_v3 = collect_flow_closure(ws.path(), &flow_abs).unwrap();
    let assert_v3 = assertions_sha256(&closure_v3).unwrap();

    // Hook mutates the SAME goal entry in deja.walks.json right before manifest commit!
    let racing_manifest = WalkManifest {
        manifest_version: 1,
        hash_version: 1,
        walks: vec![ManifestWalk {
            goal: "R2 Flow".to_string(),
            app_id: "com.example.app".to_string(),
            platform: "ios".to_string(),
            adapter: "ios-native".to_string(),
            browser: None,
            flow: flow_rel.to_string(),
            closure_sha256: fake_sha("racing-writer-closure"),
            assertions_sha256: assert_v2.clone(),
            owner_rules: None,
            profile: None,
        }],
    };
    let racing_bytes = serialize_walk_manifest(&racing_manifest).unwrap();
    let conflict_hook = Arc::new(move |manifest_path: &Path| {
        fs::write(manifest_path, racing_bytes.as_bytes()).expect("write racing manifest");
    });

    let conflict_err = consumer
        .save_verified_manifest_walk_with_hook(
            &store,
            &SaveManifestWalkOptions {
                identity: id_b1.clone(),
                flow_path: flow_rel.to_string(),
                expected_closure_sha256: closure_v3.sha256,
                expected_assertions_sha256: Some(assert_v3),
                validation: RecordValidationInput::new(true, "2026-05-01T10:10:00Z")
                    .with_origin(WalkOrigin::Verify),
                accept_assertion_change: false,
            },
            Some(conflict_hook),
        )
        .expect_err("conflicting manifest edit must return ManifestChanged");
    assert!(matches!(conflict_err, StoreError::ManifestChanged(_)));
    assert_eq!(conflict_err.exit_code(), 6);

    // SQLite MUST have rolled back — Build 1 still holds its original v1 closure & 10:00 timestamp!
    let runs = store.list_validation_runs(&id_b1).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].closure_sha256.as_deref(),
        Some(closure_v1.sha256.as_str())
    );
    assert_eq!(runs[0].validated_at, "2026-05-01T10:00:00Z");
}

#[test]
fn regression_r2_f2_gate_rejects_corrupt_validation_history_walk_files_and_repair_history() {
    let ws = TempWorkspace::new("r2-gate-corrupt-children");
    let (main_rel, main_sha) = write_classic_flow(
        &ws,
        ".deja/flows/child-check/ios-main.yaml",
        "appId: com.example.app\n---\n- runFlow: sub.yaml\n- assertVisible: ChildOk\n",
    );
    let (sub_rel, sub_sha) = write_classic_flow(
        &ws,
        ".deja/flows/child-check/sub.yaml",
        "appId: com.example.app\n---\n- tapOn: Go\n",
    );

    let id = ios_identity("Child Check", "build-child-1");
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
    store
        .upsert_walk(&UpsertWalkInput {
            identity: id.clone(),
            flow_path: main_rel,
            flow_sha256: main_sha,
            subflow_files: Some(vec![StoredFlowFile {
                path: sub_rel,
                sha256: sub_sha.clone(),
            }]),
            legacy_closure: false,
            closure_sha256: None,
            validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z"),
            exploration: None,
            repair_history: Some(vec![RepairProvenance {
                attempt_id: "rep-1".to_string(),
                patch_path: ".deja/patches/r1.diff".to_string(),
                previous_build_fingerprint: fake_sha("prev-b"),
                previous_flow_sha256: fake_sha("prev-f"),
            }]),
        })
        .unwrap();
    let walk_id = store.find_walk_id(&id).unwrap().unwrap();
    let db_path = store.memory_path().to_path_buf();
    store.close();

    let gate_id = GateIdentity {
        app_id: "com.example.app".to_string(),
        platform: "ios".to_string(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint: fake_sha("build-child-1"),
        adapter: Some("ios-native".to_string()),
        browser: None,
    };
    let ro = SqliteWalkStore::open_readonly(SqliteStoreOptions::new(ws.path())).unwrap();
    assert_eq!(
        evaluate_gate(
            &[&ro],
            &["Child Check".to_string()],
            &gate_id,
            &StoreGateOptions::default(),
        )
        .unwrap()[0]
            .outcome,
        GateOutcome::Passed
    );

    let raw = Connection::open(&db_path).unwrap();

    // 1. Tamper validation_history -> gate returns Err(InvalidWalkMemory) (exit 11)!
    raw.execute(
        "UPDATE validation_history SET passed = 0 WHERE walk_id = ?1",
        [walk_id],
    )
    .unwrap();
    let err = evaluate_gate(
        &[&ro],
        &["Child Check".to_string()],
        &gate_id,
        &StoreGateOptions::default(),
    )
    .expect_err("tampered validation_history must fail gate with Err");
    assert!(matches!(err, StoreError::InvalidWalkMemory(_)));
    assert_eq!(err.gate_exit_code(), GATE_INVALID_EXIT_CODE);
    raw.execute(
        "UPDATE validation_history SET passed = 1 WHERE walk_id = ?1",
        [walk_id],
    )
    .unwrap();

    // 2. Tamper walk_files -> gate returns Err(InvalidWalkMemory) (exit 11)!
    raw.execute(
        "UPDATE walk_files SET sha256 = ?1 WHERE walk_id = ?2",
        rusqlite::params![fake_sha("bad-sub"), walk_id],
    )
    .unwrap();
    let err = evaluate_gate(
        &[&ro],
        &["Child Check".to_string()],
        &gate_id,
        &StoreGateOptions::default(),
    )
    .expect_err("tampered walk_files must fail gate with Err");
    assert!(matches!(err, StoreError::InvalidWalkMemory(_)));
    assert_eq!(err.gate_exit_code(), GATE_INVALID_EXIT_CODE);
    raw.execute(
        "UPDATE walk_files SET sha256 = ?1 WHERE walk_id = ?2",
        rusqlite::params![sub_sha, walk_id],
    )
    .unwrap();

    // 3. Tamper repair_history -> gate returns Err(InvalidWalkMemory) (exit 11)!
    raw.execute(
        "UPDATE repair_history SET attempt_id = 'forged' WHERE walk_id = ?1",
        [walk_id],
    )
    .unwrap();
    let err = evaluate_gate(
        &[&ro],
        &["Child Check".to_string()],
        &gate_id,
        &StoreGateOptions::default(),
    )
    .expect_err("tampered repair_history must fail gate with Err");
    assert!(matches!(err, StoreError::InvalidWalkMemory(_)));
    assert_eq!(err.gate_exit_code(), GATE_INVALID_EXIT_CODE);
}

#[test]
fn regression_r2_f4_f5_f7_failed_candidate_rejection_per_run_legacy_and_external_flaky_scoping() {
    let ws = TempWorkspace::new("r2-candidate-legacy-flaky");
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();

    // Finding 4: persist_verified_candidate rejects validation.passed == false before writing anything!
    let cand = ws.write_file(
        "flows/cand.yaml",
        b"appId: com.example.app\n---\n- assertVisible: FailCand\n",
    );
    let err = store
        .persist_verified_candidate(
            &ios_identity("Fail Cand", "b-fail"),
            &PersistVerifiedCandidateOptions {
                candidate_path: cand,
                validation: RecordValidationInput::new(false, "2026-05-01T10:00:00Z"),
                expected_flow_sha256: None,
                expected_closure_sha256: None,
                exploration: None,
                repair_history: None,
            },
        )
        .expect_err("failed candidate must be rejected before writing flows or DB");
    assert!(matches!(err, StoreError::ConsumerFlow(_)));
    assert_eq!(err.exit_code(), 2);
    assert!(!ws.path().join(".deja/flows").exists());
    assert!(store.list_walks().unwrap().is_empty());

    // Finding 5: Verified multi-file Build 1 stays a Hit even after a legacy v2 Build 2 is stored on the same walk!
    ws.write_file(
        "src_flows/main.yaml",
        b"appId: com.example.app\n---\n- runFlow: helper.yaml\n- assertVisible: MultiOk\n",
    );
    ws.write_file(
        "src_flows/helper.yaml",
        b"appId: com.example.app\n---\n- tapOn: Go\n",
    );
    let id_verified = ios_identity("Multi Goal", "build-verified");
    let persisted = store
        .persist_verified_candidate(
            &id_verified,
            &PersistVerifiedCandidateOptions {
                candidate_path: PathBuf::from("src_flows/main.yaml"),
                validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z")
                    .with_origin(WalkOrigin::Remember),
                expected_flow_sha256: None,
                expected_closure_sha256: None,
                exploration: None,
                repair_history: None,
            },
        )
        .unwrap();

    // Now import a legacy schema-2 walk for Build 2 of the SAME logical walk (sets walks.legacy_closure = 1).
    let legacy_v2 = serde_json::to_string(&serde_json::json!({
        "schemaVersion": 2,
        "walks": [{
            "goal": "Multi Goal",
            "goalKey": "multi goal",
            "appId": "com.example.app",
            "platform": "ios",
            "flowSchemaVersion": "maestro/v1",
            "buildFingerprint": fake_sha("build-legacy-v2"),
            "adapter": "ios-native",
            "flowPath": persisted.flow_path,
            "flowSha256": persisted.flow_sha256,
            "lastValidation": { "passed": true, "at": "2026-05-01T10:05:00Z" },
            "validationHistory": [{ "passed": true, "at": "2026-05-01T10:05:00Z", "flowSha256": persisted.flow_sha256 }]
        }]
    }))
    .unwrap();
    let legacy_path = ws.write_file(".deja/memory/legacy-v2.json", legacy_v2.as_bytes());
    import_legacy_walks_json(&store, Some(&legacy_path)).unwrap();

    // Build 1 (verified multi-file) MUST STILL HIT (NOT LegacyClosureUnknown)!
    let hit = store.lookup(&id_verified).unwrap();
    match hit {
        LookupResult::Hit(h) => {
            assert_eq!(h.walk.legacy_closure, None);
            assert_eq!(h.walk.subflow_files.as_ref().map(Vec::len), Some(1));
        }
        other => panic!("expected verified Build 1 to stay Hit, got {other:?}"),
    }
    // Build 2 (legacy v2 referencing helper.yaml) MUST miss with LegacyClosureUnknown!
    let id_legacy = ios_identity("Multi Goal", "build-legacy-v2");
    assert!(matches!(
        store.lookup(&id_legacy).unwrap(),
        LookupResult::Miss(m) if m.reason == CacheMissReason::LegacyClosureUnknown
    ));

    // Finding 7: External GateFlakyFailure matching a PASSING run's timestamp MUST NOT mark it Flaky!
    store.close();
    let ro = SqliteWalkStore::open_readonly(SqliteStoreOptions::new(ws.path())).unwrap();
    let gate_id = GateIdentity {
        app_id: "com.example.app".to_string(),
        platform: "ios".to_string(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint: fake_sha("build-verified"),
        adapter: Some("ios-native".to_string()),
        browser: None,
    };
    let res = evaluate_gate(
        &[&ro],
        &["Multi Goal".to_string()],
        &gate_id,
        &StoreGateOptions {
            runtime: None,
            flaky_failures: &[GateFlakyFailure {
                goal_key: "multi goal".to_string(),
                build_fingerprint: fake_sha("build-verified"),
                at: "2026-05-01T10:00:00Z".to_string(),
            }],
            consumer: None,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        res[0].outcome,
        GateOutcome::Passed,
        "external flaky_failures must only apply to failed runs, never passing runs"
    );
}

#[test]
fn regression_r2_f6_readonly_reads_uncheckpointed_wal_without_creating_shm_in_workspace() {
    let src_ws = TempWorkspace::new("r2-wal-src");
    let (flow_path, flow_sha) = write_classic_flow(
        &src_ws,
        ".deja/flows/wal-only/ios-wal.yaml",
        "appId: com.example.app\n---\n- assertVisible: WalOnly\n",
    );
    let id_base = ios_identity("Base Walk", "b-base");
    let id_wal = ios_identity("Wal Only Walk", "b-wal");

    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(src_ws.path())).unwrap();
    store
        .upsert_walk(&UpsertWalkInput {
            identity: id_base.clone(),
            flow_path: flow_path.clone(),
            flow_sha256: flow_sha.clone(),
            subflow_files: Some(vec![]),
            legacy_closure: false,
            closure_sha256: None,
            validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z"),
            exploration: None,
            repair_history: None,
        })
        .unwrap();

    // Hold a read transaction on a second connection so SQLite cannot checkpoint Walk 2 out of -wal!
    let db_src = store.memory_path().to_path_buf();
    let wal_src = PathBuf::from(format!("{}-wal", db_src.display()));
    let blocker = Connection::open(&db_src).unwrap();
    blocker
        .execute_batch("BEGIN DEFERRED; SELECT COUNT(*) FROM walks;")
        .unwrap();

    store
        .upsert_walk(&UpsertWalkInput {
            identity: id_wal.clone(),
            flow_path: flow_path.clone(),
            flow_sha256: flow_sha.clone(),
            subflow_files: Some(vec![]),
            legacy_closure: false,
            closure_sha256: None,
            validation: RecordValidationInput::new(true, "2026-05-01T10:05:00Z"),
            exploration: None,
            repair_history: None,
        })
        .unwrap();

    assert!(
        fs::metadata(&wal_src).unwrap().len() > 0,
        "source -wal must hold uncheckpointed frames for Walk 2"
    );

    // Copy ONLY walks.sqlite + walks.sqlite-wal (NO -shm!) into a fresh workspace directory!
    let dst_ws = TempWorkspace::new("r2-wal-dst");
    dst_ws.write_file(
        &flow_path,
        b"appId: com.example.app\n---\n- assertVisible: WalOnly\n",
    );
    let dst_mem_dir = dst_ws.path().join(".deja/memory");
    fs::create_dir_all(&dst_mem_dir).unwrap();
    let dst_db = dst_mem_dir.join("walks.sqlite");
    let dst_wal = dst_mem_dir.join("walks.sqlite-wal");
    let dst_shm = dst_mem_dir.join("walks.sqlite-shm");
    fs::copy(&db_src, &dst_db).unwrap();
    fs::copy(&wal_src, &dst_wal).unwrap();

    drop(blocker);
    store.close();

    assert!(dst_wal.exists() && fs::metadata(&dst_wal).unwrap().len() > 0);
    assert!(!dst_shm.exists());

    // Open read-only on dst_ws -> MUST read Walk 2 from -wal AND create ZERO -shm in dst_ws!
    let ro = SqliteWalkStore::open_readonly(SqliteStoreOptions::new(dst_ws.path())).unwrap();
    assert!(
        matches!(ro.lookup(&id_wal).unwrap(), LookupResult::Hit(_)),
        "WAL-committed Walk 2 must be readable via open_readonly"
    );
    assert_eq!(ro.list_walks().unwrap().len(), 2);

    let gate_id = GateIdentity {
        app_id: "com.example.app".to_string(),
        platform: "ios".to_string(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint: fake_sha("b-wal"),
        adapter: Some("ios-native".to_string()),
        browser: None,
    };
    let gate_res = evaluate_gate(
        &[&ro],
        &["Wal Only Walk".to_string()],
        &gate_id,
        &StoreGateOptions::default(),
    )
    .unwrap();
    assert_eq!(gate_res[0].outcome, GateOutcome::Passed);

    assert!(
        !dst_shm.exists(),
        "open_readonly must NEVER create walks.sqlite-shm inside the workspace directory"
    );
    ro.close();
    assert!(!dst_shm.exists());
}

// =============================================================================================
// 11. Codex Round 3 Regression Tests (Findings 1 through 7)

#[test]
fn regression_r3_f1_readonly_direct_reopens_after_atomic_sqlite_replacement() {
    let ws = TempWorkspace::new("r3-direct-replace");
    let (flow_a, sha_a) = write_classic_flow(
        &ws,
        ".deja/flows/direct-a/ios-a.yaml",
        "appId: com.example.app\n---\n- assertVisible: DirectA\n",
    );
    let (flow_b, sha_b) = write_classic_flow(
        &ws,
        ".deja/flows/direct-b/ios-b.yaml",
        "appId: com.example.app\n---\n- assertVisible: DirectB\n",
    );
    let id_a = ios_identity("Direct A", "b-direct-a");
    let id_b = ios_identity("Direct B", "b-direct-b");

    // Keep writer A open so `-wal` and `-shm` exist when `ro` opens (entering `Direct` mode!).
    let writer_a = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
    writer_a
        .upsert_walk(&UpsertWalkInput {
            identity: id_a.clone(),
            flow_path: flow_a,
            flow_sha256: sha_a,
            subflow_files: Some(vec![]),
            legacy_closure: false,
            closure_sha256: None,
            validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z"),
            exploration: None,
            repair_history: None,
        })
        .unwrap();

    let db_path = writer_a.memory_path().to_path_buf();
    let shm_path = PathBuf::from(format!("{}-shm", db_path.display()));
    assert!(
        shm_path.exists(),
        "live writer must have -shm so open_readonly enters Direct mode"
    );

    let ro = SqliteWalkStore::open_readonly(SqliteStoreOptions::new(ws.path())).unwrap();
    assert!(matches!(ro.lookup(&id_a).unwrap(), LookupResult::Hit(_)));
    assert!(matches!(
        ro.lookup(&id_b).unwrap(),
        LookupResult::Miss(m) if m.reason == CacheMissReason::NotFound
    ));

    // Close writer A while `ro` stays open holding its `Direct` connection.
    writer_a.close();

    // Build a completely separate DB containing ONLY Walk B in another temp workspace.
    let ws_b = TempWorkspace::new("r3-direct-replacement-src");
    ws_b.write_file(
        &flow_b,
        b"appId: com.example.app\n---\n- assertVisible: DirectB\n",
    );
    let writer_b = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws_b.path())).unwrap();
    writer_b
        .upsert_walk(&UpsertWalkInput {
            identity: id_b.clone(),
            flow_path: flow_b,
            flow_sha256: sha_b,
            subflow_files: Some(vec![]),
            legacy_closure: false,
            closure_sha256: None,
            validation: RecordValidationInput::new(true, "2026-05-01T10:05:00Z"),
            exploration: None,
            repair_history: None,
        })
        .unwrap();
    let src_b_db = writer_b.memory_path().to_path_buf();
    writer_b.close();

    // Atomically replace `walks.sqlite` via `fs::rename` (new inode!).
    let staged = db_path.with_extension("sqlite.replacement");
    fs::copy(&src_b_db, &staged).unwrap();
    let _ = fs::remove_file(PathBuf::from(format!("{}-wal", db_path.display())));
    let _ = fs::remove_file(&shm_path);
    fs::rename(&staged, &db_path).unwrap();

    // Existing `ro` handle MUST detect inode/file replacement, reopen, and serve Walk B only!
    assert!(
        matches!(ro.lookup(&id_b).unwrap(), LookupResult::Hit(_)),
        "Direct read-only handle must reopen and see Walk B after atomic DB replacement"
    );
    assert!(
        matches!(
            ro.lookup(&id_a).unwrap(),
            LookupResult::Miss(m) if m.reason == CacheMissReason::NotFound
        ),
        "Direct read-only handle must NOT serve stale Walk A from unlinked inode"
    );
    assert_eq!(ro.list_walks().unwrap().len(), 1);

    // Removing `walks.sqlite` resets the open handle to empty cleanly.
    fs::remove_file(&db_path).unwrap();
    assert!(ro.list_walks().unwrap().is_empty());
}

#[test]
fn regression_r3_f2_record_validation_hashes_stored_exploration_json_text_directly() {
    let ws = TempWorkspace::new("r3-exploration-json-text");
    let (flow_path, flow_sha) = write_classic_flow(
        &ws,
        ".deja/flows/exp-legacy/ios-exp.yaml",
        "appId: com.example.app\n---\n- assertVisible: ExpLegacy\n",
    );
    let db_path = ws.path().join(".deja/memory/walks.sqlite");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    // Non-canonical JSON key ordering + whitespace + omitted optional fields that differ from
    // `serde_json::to_string(&ExplorationDetails)`!
    let non_canonical_exp = r#"{ "modelCallBasis": "provider-reported", "modelCalls": 4, "explorationDurationMs": 1200 }"#;

    {
        let raw = Connection::open(&db_path).unwrap();
        raw.execute_batch(
            "CREATE TABLE walks (
                id INTEGER PRIMARY KEY, goal TEXT NOT NULL, goal_key TEXT NOT NULL,
                app_id TEXT NOT NULL, platform TEXT NOT NULL, flow_schema_version TEXT NOT NULL,
                build_fingerprint TEXT NOT NULL, adapter TEXT, browser TEXT,
                flow_path TEXT NOT NULL, flow_sha256 TEXT NOT NULL,
                last_passed INTEGER NOT NULL, last_at TEXT NOT NULL,
                origin TEXT, exploration_json TEXT,
                has_files INTEGER NOT NULL DEFAULT 1, legacy_closure INTEGER NOT NULL DEFAULT 0,
                closure_sha256 TEXT
            );
            CREATE TABLE validation_history (
                walk_id INTEGER NOT NULL, position INTEGER NOT NULL,
                passed INTEGER NOT NULL, at TEXT NOT NULL, flow_sha256 TEXT NOT NULL,
                PRIMARY KEY (walk_id, position)
            );
            CREATE TABLE walk_files (
                walk_id INTEGER NOT NULL, position INTEGER NOT NULL,
                path TEXT NOT NULL, sha256 TEXT NOT NULL, PRIMARY KEY (walk_id, position)
            );
            CREATE TABLE repair_history (
                walk_id INTEGER NOT NULL, position INTEGER NOT NULL,
                attempt_id TEXT NOT NULL, patch_path TEXT NOT NULL,
                previous_build_fingerprint TEXT NOT NULL, previous_flow_sha256 TEXT NOT NULL,
                PRIMARY KEY (walk_id, position)
            );
            PRAGMA user_version = 3;",
        )
        .unwrap();
        raw.execute(
            "INSERT INTO walks (
                id, goal, goal_key, app_id, platform, flow_schema_version, build_fingerprint,
                adapter, browser, flow_path, flow_sha256, last_passed, last_at, origin,
                exploration_json, has_files, legacy_closure
            ) VALUES (1, 'Explored Legacy', 'explored legacy', 'com.example.app', 'ios',
                      'maestro/v1', ?1, 'ios-native', NULL, ?2, ?3, 1,
                      '2026-04-10T09:00:00Z', 'explore', ?4, 1, 0)",
            rusqlite::params![fake_sha("b-exp-1"), flow_path, flow_sha, non_canonical_exp],
        )
        .unwrap();
        raw.execute(
            "INSERT INTO validation_history (walk_id, position, passed, at, flow_sha256)
             VALUES (1, 0, 1, '2026-04-10T09:00:00Z', ?1)",
            [&flow_sha],
        )
        .unwrap();
    }

    // Upgrade legacy v3 -> v5, then record a new validation on the upgraded walk.
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
    let id = ios_identity("Explored Legacy", "b-exp-2");
    store
        .record_validation(
            &id,
            &RecordValidationInput::new(true, "2026-05-01T12:00:00Z"),
        )
        .expect("record_validation must preserve integrity with non-canonical exploration_json");

    // Subsequent lookup and list_walks MUST verify integrity cleanly (no InvalidWalkMemory!).
    assert!(matches!(store.lookup(&id).unwrap(), LookupResult::Hit(_)));
    let listed = store.list_walks().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0]
            .exploration
            .as_ref()
            .map(|e| e.model_call_basis.as_str()),
        Some("provider-reported")
    );
}

#[test]
fn regression_r3_f3_failed_shard_save_preserves_other_shards_content_addressed_flow() {
    let ws = TempWorkspace::new("r3-shared-content-addressed");
    let candidate = ws.write_file(
        "candidate/shared.yaml",
        b"appId: com.example.app\n---\n- assertVisible: SharedShard\n",
    );

    // Shard 1 persists the candidate cleanly under shared `.deja/flows/`.
    let shard1 =
        SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path()).with_store_id("shard-1"))
            .unwrap();
    let id1 = ios_identity("Shared Goal", "b-shared-1");
    let stored1 = shard1
        .persist_verified_candidate(
            &id1,
            &PersistVerifiedCandidateOptions {
                candidate_path: candidate.clone(),
                validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z"),
                expected_flow_sha256: None,
                expected_closure_sha256: None,
                exploration: None,
                repair_history: None,
            },
        )
        .unwrap();
    let shared_flow_abs = ws.path().join(&stored1.flow_path);
    assert!(shared_flow_abs.exists());

    // Shard 2 fails its SQLite commit after publishing the identical content-addressed flow.
    let shard2 =
        SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path()).with_store_id("shard-2"))
            .unwrap();
    let side2 = Connection::open(shard2.memory_path()).unwrap();
    side2
        .execute_batch(
            "CREATE TRIGGER abort_shard2 BEFORE INSERT ON validation_history
             BEGIN SELECT RAISE(ABORT, 'simulated shard-2 failure'); END;",
        )
        .unwrap();

    let id2 = ios_identity("Shared Goal", "b-shared-2");
    let err = shard2
        .persist_verified_candidate(
            &id2,
            &PersistVerifiedCandidateOptions {
                candidate_path: candidate,
                validation: RecordValidationInput::new(true, "2026-05-01T10:05:00Z"),
                expected_flow_sha256: None,
                expected_closure_sha256: None,
                exploration: None,
                repair_history: None,
            },
        )
        .expect_err("shard-2 SQLite commit must abort");
    assert!(matches!(err, StoreError::Sqlite(_)));
    side2.execute("DROP TRIGGER abort_shard2", []).unwrap();

    // Shard 2 rolled back cleanly, zero `.tmp` files remain, AND Shard 1's shared flow survives!
    assert!(shard2.list_walks().unwrap().is_empty());
    assert!(
        shared_flow_abs.exists(),
        "failed shard-2 save MUST NOT delete shard-1's shared content-addressed flow"
    );
    for entry in fs::read_dir(shared_flow_abs.parent().unwrap()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(!name.ends_with(".tmp"), "no .tmp file may remain: {name}");
    }
    assert!(matches!(shard1.lookup(&id1).unwrap(), LookupResult::Hit(_)));
}

#[test]
fn regression_r3_f4_manifest_rollback_preserves_concurrent_unrelated_edits() {
    let ws = TempWorkspace::new("r3-manifest-rollback");
    let main_rel = ".maestro/flows/rollback_goal.yaml";
    let other_rel = ".maestro/flows/concurrent_goal.yaml";
    let main_abs = ws.write_file(
        main_rel,
        b"appId: com.example.app\n---\n- assertVisible:\n    id: rollback_ok\n",
    );
    let other_abs = ws.write_file(
        other_rel,
        b"appId: com.example.app\n---\n- assertVisible:\n    id: concurrent_ok\n",
    );
    let main_closure = collect_flow_closure(ws.path(), &main_abs).unwrap();
    let main_assert = assertions_sha256(&main_closure).unwrap();
    let other_closure = collect_flow_closure(ws.path(), &other_abs).unwrap();
    let other_assert = assertions_sha256(&other_closure).unwrap();

    let concurrent_manifest_text = serialize_walk_manifest(&WalkManifest {
        manifest_version: 1,
        hash_version: 1,
        walks: vec![ManifestWalk {
            goal: "Concurrent Unrelated Goal".to_string(),
            app_id: "com.example.app".to_string(),
            platform: "ios".to_string(),
            adapter: "ios-native".to_string(),
            browser: None,
            flow: other_rel.to_string(),
            closure_sha256: other_closure.sha256,
            assertions_sha256: other_assert,
            owner_rules: None,
            profile: None,
        }],
    })
    .unwrap();

    let consumer =
        ConsumerFlows::new(ConsumerFlowsOptions::new(ws.path(), "deja.walks.json")).unwrap();
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
    let lock_path = PathBuf::from(format!("{}.lock", store.memory_path().display()));

    // Hook simulates:
    // 1. Another process writes an unrelated goal to `deja.walks.json` right before our locked re-read.
    // 2. Our SQLite writer lock is hijacked so `with_write_tx` fails right AFTER `merged_bytes` is
    //    written and BEFORE `tx.commit()`.
    let injected = concurrent_manifest_text.clone();
    let lock_path_clone = lock_path.clone();
    let hook = Arc::new(move |manifest_path: &Path| {
        fs::write(manifest_path, injected.as_bytes()).expect("inject concurrent unrelated goal");
        let hijacked = LockHolder {
            version: 1,
            pid: std::process::id(),
            started_at: process_start(std::process::id()),
            host: current_hostname(),
            machine_id: machine_id(),
            token: "hijacked-token-r3-f4".to_string(),
            acquired_at: now_iso8601(),
        };
        fs::write(
            &lock_path_clone,
            format!("{}\n", serde_json::to_string(&hijacked).unwrap()),
        )
        .expect("hijack writer lock");
    });

    let err = consumer
        .save_verified_manifest_walk_with_hook(
            &store,
            &SaveManifestWalkOptions {
                identity: ios_identity("Rollback Goal", "b-rb-1"),
                flow_path: main_rel.to_string(),
                expected_closure_sha256: main_closure.sha256,
                expected_assertions_sha256: Some(main_assert),
                validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z")
                    .with_origin(WalkOrigin::Remember),
                accept_assertion_change: false,
            },
            Some(hook),
        )
        .expect_err("hijacked SQLite writer lock must abort commit and trigger rollback");
    assert!(matches!(err, StoreError::StoreLocked { .. }));

    // SQLite rolled back cleanly!
    assert!(store.list_walks().unwrap().is_empty());

    // Manifest rollback restored `pre_merge_bytes`: the concurrent unrelated goal is preserved,
    // while our aborted goal was rolled back!
    let after = consumer.load().unwrap();
    assert_eq!(after.walks.len(), 1);
    assert_eq!(after.walks[0].goal, "Concurrent Unrelated Goal");
    assert_eq!(
        fs::read_to_string(ws.path().join("deja.walks.json")).unwrap(),
        concurrent_manifest_text
    );
    let _ = fs::remove_file(&lock_path);
}

#[test]
fn regression_r3_f5_manifest_gate_rejects_mismatched_run_flow_sha256() {
    let ws = TempWorkspace::new("r3-manifest-flow-sha");
    let flow_rel = ".maestro/flows/manifest_sha.yaml";
    let flow_abs = ws.write_file(
        flow_rel,
        b"appId: com.example.app\n---\n- assertVisible:\n    id: sha_ok\n",
    );
    let closure = collect_flow_closure(ws.path(), &flow_abs).unwrap();
    let assert_sha = assertions_sha256(&closure).unwrap();

    let consumer =
        ConsumerFlows::new(ConsumerFlowsOptions::new(ws.path(), "deja.walks.json")).unwrap();
    let id = ios_identity("Manifest Flow Sha", "b-msha-1");
    let db_path = {
        let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
        consumer
            .save_verified_manifest_walk(
                &store,
                &SaveManifestWalkOptions {
                    identity: id.clone(),
                    flow_path: flow_rel.to_string(),
                    expected_closure_sha256: closure.sha256.clone(),
                    expected_assertions_sha256: Some(assert_sha),
                    validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z")
                        .with_origin(WalkOrigin::Remember),
                    accept_assertion_change: false,
                },
            )
            .unwrap();
        let p = store.memory_path().to_path_buf();
        store.close();
        p
    };

    // Rewrite `validation_runs.flow_sha256` to a DIFFERENT hash (with a valid `integrity_sha256`!)
    // while keeping `closure_sha256 == entry.closure_sha256`!
    let wrong_flow_sha = fake_sha("mismatched-main-flow-sha");
    {
        let raw = Connection::open(&db_path).unwrap();
        let (walk_id, build_fp, validated_at): (i64, String, String) = raw
            .query_row(
                "SELECT walk_id, build_fingerprint, validated_at FROM validation_runs LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        let new_integrity = compute_validation_run_integrity(&ValidationRunIntegrityFields {
            walk_id,
            build_fingerprint: &build_fp,
            runtime: None,
            flow_path: flow_rel,
            flow_sha256: &wrong_flow_sha,
            closure_sha256: Some(&closure.sha256),
            subflow_files_json: None,
            passed: true,
            flaky: false,
            validated_at: &validated_at,
            origin: Some("remember"),
            failure_category: None,
            report_path: None,
            duration_ms: None,
            phase_timings_json: None,
        });
        raw.execute(
            "UPDATE validation_runs SET flow_sha256 = ?1, integrity_sha256 = ?2 WHERE walk_id = ?3",
            rusqlite::params![wrong_flow_sha, new_integrity, walk_id],
        )
        .unwrap();
    }

    let ro = SqliteWalkStore::open_readonly(SqliteStoreOptions::new(ws.path())).unwrap();
    let gate_id = GateIdentity {
        app_id: "com.example.app".to_string(),
        platform: "ios".to_string(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint: fake_sha("b-msha-1"),
        adapter: Some("ios-native".to_string()),
        browser: None,
    };
    let gate_res = evaluate_gate(
        &[&ro],
        &["Manifest Flow Sha".to_string()],
        &gate_id,
        &StoreGateOptions {
            consumer: Some(&consumer),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        gate_res[0].outcome,
        GateOutcome::FlowChanged,
        "manifest gate MUST fail closed when run.flow_sha256 differs from live main flow"
    );

    // `plan_run` must also NOT treat the mismatched run as a `Hit`!
    let plan = consumer
        .plan_run(
            &ro,
            &ConsumerRunRequest {
                identity: &id,
                runtime: None,
                stale_build: None,
            },
        )
        .unwrap();
    assert!(
        !matches!(plan, ConsumerRunPlan::Hit { .. }),
        "plan_run must NOT return Hit when run.flow_sha256 differs from live main flow"
    );
}

#[cfg(unix)]
#[test]
fn regression_r3_f6_symlink_escape_rejected_before_create_dir_all() {
    use std::os::unix::fs::symlink;

    let ws = TempWorkspace::new("r3-symlink-mkdir-ws");
    let outside = TempWorkspace::new("r3-symlink-mkdir-outside");

    // 1. Manifest writer: `ws/escape_dir -> outside`, target `escape_dir/never_created/deja.walks.json`.
    let escape_manifest_dir = ws.path().join("escape_dir");
    symlink(outside.path(), &escape_manifest_dir).unwrap();
    let consumer = ConsumerFlows::new(ConsumerFlowsOptions::new(
        ws.path(),
        "escape_dir/never_created/deja.walks.json",
    ))
    .unwrap();
    let empty_manifest = WalkManifest {
        manifest_version: 1,
        hash_version: 1,
        walks: Vec::new(),
    };
    let err = consumer
        .write_manifest(&empty_manifest)
        .expect_err("symlinked manifest ancestor escaping workspace must be rejected");
    assert!(matches!(err, StoreError::Core(_)));
    assert!(
        !outside.path().join("never_created").exists(),
        "write_manifest MUST reject symlink escape BEFORE create_dir_all creates dirs outside workspace"
    );

    // 2. Stored-flow writer (`persist_verified_candidate`): `ws/.deja/flows -> outside`.
    let flows_link = ws.path().join(".deja/flows");
    fs::create_dir_all(flows_link.parent().unwrap()).unwrap();
    symlink(outside.path(), &flows_link).unwrap();

    let candidate = ws.write_file(
        "candidate/escape.yaml",
        b"appId: com.example.app\n---\n- assertVisible: Escape\n",
    );
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();
    let flow_err = store
        .persist_verified_candidate(
            &ios_identity("Escape Goal", "b-esc-1"),
            &PersistVerifiedCandidateOptions {
                candidate_path: candidate.clone(),
                validation: RecordValidationInput::new(true, "2026-05-01T10:00:00Z"),
                expected_flow_sha256: None,
                expected_closure_sha256: None,
                exploration: None,
                repair_history: None,
            },
        )
        .expect_err("symlinked .deja/flows escaping workspace must be rejected");
    assert!(matches!(flow_err, StoreError::InvalidWalkMemory(_)));
    assert!(
        !outside.path().join("escape-goal").exists(),
        "persist_verified_candidate MUST reject symlink escape BEFORE create_dir_all"
    );

    // 3. Candidate snapshot writer (`snapshot_candidate`): `ws/.deja/attempts -> outside`.
    let attempts_link = ws.path().join(".deja/attempts");
    symlink(outside.path(), &attempts_link).unwrap();
    let snap_err = store
        .snapshot_candidate(&candidate)
        .expect_err("symlinked .deja/attempts escaping workspace must be rejected");
    assert!(matches!(snap_err, StoreError::InvalidWalkMemory(_)));
    assert_eq!(
        fs::read_dir(outside.path()).unwrap().count(),
        0,
        "snapshot_candidate MUST NOT create any snapshot directory outside the workspace"
    );
}

#[test]
fn regression_r3_f7_previous_successful_orders_by_parsed_utc_instant() {
    // Direct chronological checks on `parse_rfc3339_utc`:
    // ASCII `'.'` (0x2E) < `'Z'` (0x5A), so lexical order wrongly puts `.500Z` BEFORE `Z`!
    assert!(
        parse_rfc3339_utc("2026-01-01T12:00:00.500Z") > parse_rfc3339_utc("2026-01-01T12:00:00Z")
    );
    // `12:00:00+02:00` is `10:00:00Z`, which is EARLIER than `10:30:00Z`!
    assert!(
        parse_rfc3339_utc("2026-01-01T10:30:00Z") > parse_rfc3339_utc("2026-01-01T12:00:00+02:00")
    );
    // `08:30:00-02:00` equals `10:30:00Z`.
    assert_eq!(
        parse_rfc3339_utc("2026-01-01T08:30:00-02:00"),
        parse_rfc3339_utc("2026-01-01T10:30:00Z")
    );

    let ws = TempWorkspace::new("r3-utc-ordering");
    let (classic_path, classic_sha) = write_classic_flow(
        &ws,
        ".deja/flows/utc-order/ios-utc.yaml",
        "appId: com.example.app\n---\n- assertVisible: UtcOrder\n",
    );
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(ws.path())).unwrap();

    // 1. Classic `lookup_previous_successful`:
    //    - Build A: 2026-01-01T12:00:00+02:00 (= 10:00:00Z, lexicographically largest prefix!)
    //    - Build B: 2026-01-01T12:00:00Z      (= 12:00:00Z)
    //    - Build C: 2026-01-01T12:00:00.500Z  (= 12:00:00.500Z, true latest despite '.' < 'Z'!)
    for (build, ts) in [
        ("b-utc-c", "2026-01-01T12:00:00.500Z"),
        ("b-utc-b", "2026-01-01T12:00:00Z"),
        ("b-utc-a", "2026-01-01T12:00:00+02:00"),
    ] {
        store
            .upsert_walk(&UpsertWalkInput {
                identity: ios_identity("UTC Classic", build),
                flow_path: classic_path.clone(),
                flow_sha256: classic_sha.clone(),
                subflow_files: Some(vec![]),
                legacy_closure: false,
                closure_sha256: None,
                validation: RecordValidationInput::new(true, ts),
                exploration: None,
                repair_history: None,
            })
            .unwrap();
    }

    let prev = match store
        .lookup_previous_successful(&ios_identity("UTC Classic", "b-utc-new"))
        .unwrap()
    {
        LookupResult::Hit(hit) => hit.walk,
        other => panic!("expected Hit from lookup_previous_successful, got {other:?}"),
    };
    assert_eq!(
        prev.last_validation.at, "2026-01-01T12:00:00.500Z",
        "fractional seconds must sort AFTER whole seconds"
    );
    assert_eq!(prev.build_fingerprint, fake_sha("b-utc-c"));

    // Add Build D with negative offset: `10:30:00-02:00` (= 12:30:00Z), lexicographically smaller
    // than `12:00:00Z` yet chronologically NEWEST!
    store
        .upsert_walk(&UpsertWalkInput {
            identity: ios_identity("UTC Classic", "b-utc-d"),
            flow_path: classic_path,
            flow_sha256: classic_sha,
            subflow_files: Some(vec![]),
            legacy_closure: false,
            closure_sha256: None,
            validation: RecordValidationInput::new(true, "2026-01-01T10:30:00-02:00"),
            exploration: None,
            repair_history: None,
        })
        .unwrap();
    let prev2 = match store
        .lookup_previous_successful(&ios_identity("UTC Classic", "b-utc-new"))
        .unwrap()
    {
        LookupResult::Hit(hit) => hit.walk,
        other => panic!("expected Hit from lookup_previous_successful, got {other:?}"),
    };
    assert_eq!(
        prev2.last_validation.at, "2026-01-01T10:30:00-02:00",
        "negative timezone offset resolving to later UTC instant must win"
    );
    assert_eq!(prev2.build_fingerprint, fake_sha("b-utc-d"));

    // 2. Manifest `ConsumerFlows::plan_run` (`previous_successful` selection):
    let m_rel = ".maestro/flows/utc_manifest.yaml";
    let m_abs = ws.write_file(
        m_rel,
        b"appId: com.example.app\n---\n- assertVisible:\n    id: utc_m\n",
    );
    let m_closure = collect_flow_closure(ws.path(), &m_abs).unwrap();
    let m_assert = assertions_sha256(&m_closure).unwrap();
    let consumer =
        ConsumerFlows::new(ConsumerFlowsOptions::new(ws.path(), "deja.walks.json")).unwrap();

    for (build, ts) in [
        ("bm-1", "2026-01-01T12:00:00Z"),
        ("bm-2", "2026-01-01T12:00:00.500Z"),
        ("bm-3", "2026-01-01T12:00:00+02:00"),
        ("bm-4", "2026-01-01T10:30:00-02:00"),
    ] {
        consumer
            .save_verified_manifest_walk(
                &store,
                &SaveManifestWalkOptions {
                    identity: ios_identity("UTC Manifest", build),
                    flow_path: m_rel.to_string(),
                    expected_closure_sha256: m_closure.sha256.clone(),
                    expected_assertions_sha256: Some(m_assert.clone()),
                    validation: RecordValidationInput::new(true, ts)
                        .with_origin(WalkOrigin::Remember),
                    accept_assertion_change: false,
                },
            )
            .unwrap();
    }

    let stale_id = ios_identity("UTC Manifest", "bm-stale");
    let plan = consumer
        .plan_run(
            &store,
            &ConsumerRunRequest {
                identity: &stale_id,
                runtime: None,
                stale_build: Some(StaleBuildPolicy::ReplayLastVerified),
            },
        )
        .unwrap();
    match plan {
        ConsumerRunPlan::ReplayInPlace {
            previous_successful: Some(prev_run),
            ..
        } => {
            assert_eq!(prev_run.validated_at, "2026-01-01T10:30:00-02:00");
            assert_eq!(prev_run.build_fingerprint, fake_sha("bm-4"));
        }
        other => {
            panic!("expected ReplayInPlace with UTC-latest previous_successful, got {other:?}")
        }
    }
}
