//! Consumer-owned flow manifest (`deja.walks.json`) lifecycle and `DEJA-51` manifest replay
//! decision state machine.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use deja_core::{
    CacheMissReason, CoreError, DEFAULT_FLOW_SCHEMA_VERSION, FlowChange, FlowClosure, ManifestWalk,
    RunOutcome, StaleBuildPolicy, StoredWalk, WalkIdentity, WalkManifest, WalkOrigin,
    assertions_sha256, check_manifest_flow_path, check_owner_rules, closure_assertions,
    collect_flow_closure, empty_walk_manifest, json_holds_forbidden_value, manifest_walk_key,
    normalize_goal, parse_walk_manifest, serialize_walk_manifest, validate_forbidden_values,
};

use crate::error::StoreError;
use crate::lock::{
    WriterLock, create_new_nofollow, generate_token, is_eloop, lexical_normalize,
    open_read_nofollow, path_inside,
};
use crate::store::{
    RecordValidationInput, SqliteWalkStore, UpsertWalkInput, ValidationRunRecord,
    normalize_walk_identity, validate_runtime,
};

const DEFAULT_MANIFEST_LOCK_RETRIES: usize = 40;
const MANIFEST_LOCK_WAIT: Duration = Duration::from_millis(50);

/// Validate that a manifest path is a relative `.json` file inside the workspace and outside `.deja/`.
pub fn check_manifest_path(path: &str) -> Result<String, StoreError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path
            .chars()
            .any(|c| matches!(c, '\u{0000}'..='\u{001f}' | '\u{007f}'))
        || path
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
        || path
            .split('/')
            .next()
            .is_some_and(|first| first.eq_ignore_ascii_case(".deja"))
        || !path.ends_with(".json")
    {
        return Err(StoreError::Core(CoreError::InvalidManifest(format!(
            "the manifest path must be a relative .json path outside .deja/: {path}"
        ))));
    }
    Ok(path.to_string())
}

/// Validate that an optional `flowsDir` is a relative directory inside the workspace and outside `.deja/`.
pub fn check_flows_dir(path: &str) -> Result<String, StoreError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path
            .chars()
            .any(|c| matches!(c, '\u{0000}'..='\u{001f}' | '\u{007f}'))
        || path
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
        || path
            .split('/')
            .next()
            .is_some_and(|first| first.eq_ignore_ascii_case(".deja"))
    {
        return Err(StoreError::Core(CoreError::InvalidManifest(format!(
            "the flows folder must be a relative folder outside .deja/: {path}"
        ))));
    }
    Ok(path.to_string())
}

/// Options for configuring [`ConsumerFlows`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerFlowsOptions {
    pub workspace_root: PathBuf,
    pub manifest_path: String,
    pub flows_dir: Option<String>,
    pub forbidden_values: Vec<String>,
    /// Maximum retry attempts when acquiring `.deja/manifest.lock` (defaults to 40).
    pub lock_retries: Option<usize>,
}

impl ConsumerFlowsOptions {
    #[must_use]
    pub fn new(workspace_root: impl Into<PathBuf>, manifest_path: impl Into<String>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            manifest_path: manifest_path.into(),
            flows_dir: None,
            forbidden_values: Vec::new(),
            lock_retries: None,
        }
    }

    #[must_use]
    pub fn with_flows_dir(mut self, flows_dir: impl Into<String>) -> Self {
        self.flows_dir = Some(flows_dir.into());
        self
    }

    #[must_use]
    pub fn with_forbidden_values(mut self, values: Vec<String>) -> Self {
        self.forbidden_values = values;
        self
    }

    #[must_use]
    pub fn with_lock_retries(mut self, retries: usize) -> Self {
        self.lock_retries = Some(retries);
        self
    }
}

/// Request passed to [`ConsumerFlows::plan_run`] (`DEJA-51` decision state machine).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerRunRequest<'a> {
    pub identity: &'a WalkIdentity,
    pub runtime: Option<&'a str>,
    pub stale_build: Option<StaleBuildPolicy>,
}

/// Decision produced by [`ConsumerFlows::plan_run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsumerRunPlan {
    /// Exact `(build_fingerprint, runtime)` cache hit whose disk closure & assertions match the manifest.
    Hit {
        entry: ManifestWalk,
        flow_path: PathBuf,
        flow_sha256: String,
        closure_sha256: String,
        assertions_sha256: String,
        run: ValidationRunRecord,
    },
    /// `DEJA-51`: `staleBuild == Some(ReplayLastVerified)` and disk strictly matches BOTH
    /// `closureSha256` and `assertionsSha256`. Replay `entry.flow` in place without `--verify`
    /// and without touching `deja.walks.json`.
    ReplayInPlace {
        entry: ManifestWalk,
        flow_path: PathBuf,
        flow_sha256: String,
        closure_sha256: String,
        assertions_sha256: String,
        reason: CacheMissReason,
        previous_successful: Option<ValidationRunRecord>,
    },
    /// Fail closed without running the flow or mutating `deja.walks.json` or SQLite.
    Refuse {
        outcome: RunOutcome,
        miss_reason: CacheMissReason,
        exit_code: u8,
        detail: String,
        flow_change: Option<FlowChange>,
        manifest_assertions_sha256: Option<String>,
        current_assertions_sha256: Option<String>,
    },
}

/// Result of inspecting a single manifest entry's files on disk.
#[derive(Debug, Clone)]
pub enum InspectedConsumerEntry {
    Missing {
        file: String,
    },
    Ok {
        canonical_flow: PathBuf,
        closure: FlowClosure,
        main_sha256: String,
        assertions_sha256: String,
    },
}

/// Options for saving a verified flow back into `deja.walks.json` and SQLite (`remember` / `--verify`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveManifestWalkOptions {
    pub identity: WalkIdentity,
    pub flow_path: String,
    pub validation: RecordValidationInput,
    pub accept_assertion_change: bool,
}

/// Manages `deja.walks.json`, `.deja/manifest.lock`, disk closure/assertion verification,
/// owner rules, forbidden fixture scanning, and the `DEJA-51` manifest replay state machine.
#[derive(Debug, Clone)]
pub struct ConsumerFlows {
    workspace_root: PathBuf,
    manifest_path: String,
    flows_dir: Option<String>,
    forbidden_values: Vec<String>,
    lock_retries: usize,
}

impl ConsumerFlows {
    /// Construct and strictly validate a [`ConsumerFlows`] handler.
    pub fn new(options: ConsumerFlowsOptions) -> Result<Self, StoreError> {
        let manifest_path = check_manifest_path(&options.manifest_path)?;
        let flows_dir = options
            .flows_dir
            .as_deref()
            .map(check_flows_dir)
            .transpose()?;
        validate_forbidden_values(&options.forbidden_values)?;
        Ok(Self {
            workspace_root: options.workspace_root,
            manifest_path,
            flows_dir,
            forbidden_values: options.forbidden_values,
            lock_retries: options
                .lock_retries
                .unwrap_or(DEFAULT_MANIFEST_LOCK_RETRIES),
        })
    }

    #[must_use]
    pub fn manifest_path(&self) -> &str {
        &self.manifest_path
    }

    #[must_use]
    pub fn flows_dir(&self) -> Option<&str> {
        self.flows_dir.as_deref()
    }

    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    fn canonical_root(&self) -> Result<PathBuf, StoreError> {
        Ok(fs::canonicalize(&self.workspace_root)?)
    }

    fn manifest_file(&self) -> Result<(PathBuf, PathBuf), StoreError> {
        let root = self.canonical_root()?;
        let target = lexical_normalize(&root.join(&self.manifest_path));
        if !path_inside(&root, &target) {
            return Err(StoreError::Core(CoreError::InvalidManifest(
                "the manifest path escapes the workspace".to_string(),
            )));
        }
        Ok((root, target))
    }

    /// Read and strictly parse `deja.walks.json`.
    ///
    /// Returns an empty manifest when the file does not exist; refuses symlinks, workspace escapes,
    /// invalid JSON, duplicate keys, or schema violations.
    pub fn load(&self) -> Result<WalkManifest, StoreError> {
        let (root, target) = self.manifest_file()?;
        let lmeta = match fs::symlink_metadata(&target) {
            Ok(meta) => meta,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(empty_walk_manifest()),
            Err(err) => {
                return Err(StoreError::Core(CoreError::InvalidManifest(format!(
                    "the manifest cannot be read: {err}"
                ))));
            }
        };
        if lmeta.file_type().is_symlink() || !lmeta.file_type().is_file() {
            return Err(StoreError::Core(CoreError::InvalidManifest(
                "the manifest is not a regular file inside the workspace".to_string(),
            )));
        }
        let canonical = fs::canonicalize(&target).map_err(|err| {
            StoreError::Core(CoreError::InvalidManifest(format!(
                "the manifest cannot be resolved: {err}"
            )))
        })?;
        if !path_inside(&root, &canonical) {
            return Err(StoreError::Core(CoreError::InvalidManifest(
                "the manifest escapes the workspace".to_string(),
            )));
        }

        let mut file = open_read_nofollow(&canonical).map_err(|err| {
            let detail = if is_eloop(&err) {
                "the manifest is a symbolic link".to_string()
            } else {
                format!("the manifest cannot be read: {err}")
            };
            StoreError::Core(CoreError::InvalidManifest(detail))
        })?;
        let mut contents = String::new();
        file.read_to_string(&mut contents).map_err(|err| {
            StoreError::Core(CoreError::InvalidManifest(format!(
                "the manifest cannot be read: {err}"
            )))
        })?;
        Ok(parse_walk_manifest(&contents)?)
    }

    /// Validate that `manifest` serializes cleanly, contains zero forbidden fixture values,
    /// and round-trips through [`parse_walk_manifest`].
    pub fn check_manifest(&self, manifest: &WalkManifest) -> Result<String, StoreError> {
        let text = serialize_walk_manifest(manifest)?;
        if json_holds_forbidden_value(&text, &self.forbidden_values) {
            return Err(StoreError::Core(CoreError::InvalidManifest(
                "the manifest would hold a fixture credential value; nothing was written"
                    .to_string(),
            )));
        }
        parse_walk_manifest(&text).map_err(|err| {
            StoreError::Core(CoreError::InvalidManifest(format!(
                "the manifest would not read back ({err}); nothing was written"
            )))
        })?;
        Ok(text)
    }

    /// Execute `work` while holding the workspace-wide `.deja/manifest.lock`.
    pub fn with_manifest_lock<T>(
        &self,
        work: impl FnOnce() -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let root = self.canonical_root()?;
        let lock_path = root.join(".deja/manifest.lock");
        let mut lock = None;
        for attempt in 0..=self.lock_retries {
            match WriterLock::acquire(lock_path.clone()) {
                Ok(acquired) => {
                    lock = Some(acquired);
                    break;
                }
                Err(err) if err.is_lock_error() => {
                    if attempt >= self.lock_retries {
                        return Err(StoreError::ManifestLocked {
                            lock_path: lock_path.display().to_string(),
                        });
                    }
                    thread::sleep(MANIFEST_LOCK_WAIT);
                }
                Err(other) => return Err(other),
            }
        }
        let mut guard = lock.expect("manifest lock acquired");
        let result = work();
        guard.release();
        result
    }

    /// Atomically write `manifest` to disk (caller should hold [`ConsumerFlows::with_manifest_lock`]).
    pub fn write_manifest(&self, manifest: &WalkManifest) -> Result<(), StoreError> {
        let text = self.check_manifest(manifest)?;
        let (root, target) = self.manifest_file()?;
        let parent = target.parent().ok_or_else(|| {
            StoreError::Core(CoreError::InvalidManifest(
                "the manifest path has no parent directory".to_string(),
            ))
        })?;
        fs::create_dir_all(parent)?;
        let canonical_parent = fs::canonicalize(parent)?;
        if canonical_parent != root && !path_inside(&root, &canonical_parent) {
            return Err(StoreError::Core(CoreError::InvalidManifest(
                "the manifest path escapes the workspace".to_string(),
            )));
        }
        if let Ok(lmeta) = fs::symlink_metadata(&target)
            && lmeta.file_type().is_symlink()
        {
            return Err(StoreError::Core(CoreError::InvalidManifest(
                "the manifest is a symbolic link; replace it with a regular file".to_string(),
            )));
        }

        let mut tmp_os = target.as_os_str().to_os_string();
        tmp_os.push(format!(".{}.{}.tmp", std::process::id(), generate_token()));
        let tmp = PathBuf::from(tmp_os);
        let write_res = (|| -> Result<(), StoreError> {
            let mut f = create_new_nofollow(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            Ok(())
        })();
        if let Err(err) = write_res {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
        if let Err(err) = fs::rename(&tmp, &target) {
            let _ = fs::remove_file(&tmp);
            return Err(StoreError::Io(err));
        }
        Ok(())
    }

    /// Inspect one manifest entry's files on disk, computing live `closureSha256` and `assertionsSha256`.
    pub fn inspect_entry(
        &self,
        entry: &ManifestWalk,
    ) -> Result<InspectedConsumerEntry, StoreError> {
        let _ = check_manifest_flow_path(&entry.flow, "flow")?;
        let root = self.canonical_root()?;
        let absolute = lexical_normalize(&root.join(&entry.flow));
        if !path_inside(&root, &absolute) {
            return Err(StoreError::Core(CoreError::InvalidManifest(format!(
                "flow escapes the workspace: {}",
                entry.flow
            ))));
        }

        let canonical = match fs::canonicalize(&absolute) {
            Ok(p) => p,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Ok(InspectedConsumerEntry::Missing {
                    file: entry.flow.clone(),
                });
            }
            Err(err) => return Err(StoreError::Io(err)),
        };
        if !path_inside(&root, &canonical) || !fs::metadata(&canonical)?.is_file() {
            return Ok(InspectedConsumerEntry::Missing {
                file: entry.flow.clone(),
            });
        }

        let closure = match collect_flow_closure(&root, &canonical) {
            Ok(c) => c,
            Err(CoreError::MissingFlowFile { path }) => {
                return Ok(InspectedConsumerEntry::Missing { file: path });
            }
            Err(err) => return Err(StoreError::Core(err)),
        };
        let main = closure
            .files
            .iter()
            .find(|f| f.path == closure.main)
            .ok_or_else(|| {
                StoreError::ConsumerFlow("collected closure is missing its main flow".to_string())
            })?;
        let assertions_hash = assertions_sha256(&closure)?;
        Ok(InspectedConsumerEntry::Ok {
            canonical_flow: canonical,
            main_sha256: main.sha256.clone(),
            closure,
            assertions_sha256: assertions_hash,
        })
    }

    /// Find the matching [`ManifestWalk`] entry or classify the exact identity miss reason.
    pub fn resolve_manifest_entry(
        &self,
        store: &SqliteWalkStore,
        identity: &WalkIdentity,
    ) -> Result<Result<ManifestWalk, CacheMissReason>, StoreError> {
        let norm = normalize_walk_identity(identity)?;
        let manifest = self.load()?;

        let same_goal: Vec<&ManifestWalk> = manifest
            .walks
            .iter()
            .filter(|entry| {
                normalize_goal(&entry.goal)
                    .map(|g| g.key == norm.goal_key)
                    .unwrap_or(false)
            })
            .collect();
        if same_goal.is_empty() {
            let legacy = store.has_legacy_walk_for_goal(&norm.goal_key)?;
            return Ok(Err(if legacy {
                CacheMissReason::NeedsMigration
            } else {
                CacheMissReason::NotFound
            }));
        }
        let same_app: Vec<&ManifestWalk> = same_goal
            .into_iter()
            .filter(|e| e.app_id == norm.app_id)
            .collect();
        if same_app.is_empty() {
            return Ok(Err(CacheMissReason::WrongApp));
        }
        let same_platform: Vec<&ManifestWalk> = same_app
            .into_iter()
            .filter(|e| e.platform == norm.platform)
            .collect();
        if same_platform.is_empty() {
            return Ok(Err(CacheMissReason::WrongPlatform));
        }
        if norm.flow_schema_version != DEFAULT_FLOW_SCHEMA_VERSION {
            return Ok(Err(CacheMissReason::WrongFlowSchema));
        }
        let same_adapter: Vec<&ManifestWalk> = same_platform
            .into_iter()
            .filter(|e| Some(e.adapter.as_str()) == norm.adapter.as_deref())
            .collect();
        if same_adapter.is_empty() {
            return Ok(Err(CacheMissReason::WrongAdapter));
        }
        let Some(entry) = same_adapter
            .into_iter()
            .find(|e| e.browser.as_deref() == norm.browser.as_deref())
        else {
            return Ok(Err(CacheMissReason::WrongBrowser));
        };
        Ok(Ok(entry.clone()))
    }

    /// `DEJA-51` manifest replay decision state machine (strictly read-only: zero writes).
    ///
    /// 1. Resolves the exact manifest entry and inspects the consumer's flow closure on disk.
    /// 2. Strictly checks BOTH `closureSha256` and `assertionsSha256`: any drift refuses with
    ///    exit 6 (`FlowChanged` / `AssertionsChanged`) regardless of `stale_build`.
    /// 3. Queries `validation_runs` for `(walk_id, build_fingerprint, runtime_key)` where
    ///    `closure_sha256 == entry.closure_sha256`: latest `passed == true` -> [`ConsumerRunPlan::Hit`].
    /// 4. On `NotValidated`, `FailedValidation`, or `StaleBuild`:
    ///    - With `stale_build == Some(StaleBuildPolicy::ReplayLastVerified)`: returns
    ///      [`ConsumerRunPlan::ReplayInPlace`] so `entry.flow` replays in place without `--verify`
    ///      and without touching `deja.walks.json`.
    ///    - Without `ReplayLastVerified`: refuses (`NotValidated` exit 6; `FailedValidation` /
    ///      `StaleBuild` exit 1).
    pub fn plan_run(
        &self,
        store: &SqliteWalkStore,
        req: &ConsumerRunRequest<'_>,
    ) -> Result<ConsumerRunPlan, StoreError> {
        let norm = normalize_walk_identity(req.identity)?;
        let rt = validate_runtime(req.runtime)?;

        let entry = match self.resolve_manifest_entry(store, req.identity)? {
            Ok(entry) => entry,
            Err(reason) => {
                let (outcome, exit_code) = match reason {
                    CacheMissReason::NeedsMigration => (RunOutcome::NeedsMigration, 6),
                    _ => (RunOutcome::Failed, 1),
                };
                return Ok(ConsumerRunPlan::Refuse {
                    outcome,
                    miss_reason: reason,
                    exit_code,
                    detail: format!("manifest lookup missed ({})", reason.as_str()),
                    flow_change: None,
                    manifest_assertions_sha256: None,
                    current_assertions_sha256: None,
                });
            }
        };

        let (canonical_flow, closure, main_sha256, current_assertions_sha256) =
            match self.inspect_entry(&entry)? {
                InspectedConsumerEntry::Missing { file } => {
                    return Ok(ConsumerRunPlan::Refuse {
                        outcome: RunOutcome::Failed,
                        miss_reason: CacheMissReason::MissingFlow,
                        exit_code: 1,
                        detail: format!("consumer flow file is missing: {file}"),
                        flow_change: None,
                        manifest_assertions_sha256: None,
                        current_assertions_sha256: None,
                    });
                }
                InspectedConsumerEntry::Ok {
                    canonical_flow,
                    closure,
                    main_sha256,
                    assertions_sha256,
                } => (canonical_flow, closure, main_sha256, assertions_sha256),
            };

        if closure.sha256 != entry.closure_sha256 {
            return Ok(ConsumerRunPlan::Refuse {
                outcome: RunOutcome::FlowChanged,
                miss_reason: CacheMissReason::FlowChanged,
                exit_code: 6,
                detail: format!(
                    "the flow or a file it runs changed (manifest {}, now {}); re-verify it",
                    entry.closure_sha256, closure.sha256
                ),
                flow_change: Some(FlowChange {
                    manifest_closure_sha256: entry.closure_sha256,
                    current_closure_sha256: closure.sha256,
                }),
                manifest_assertions_sha256: Some(entry.assertions_sha256),
                current_assertions_sha256: Some(current_assertions_sha256),
            });
        }

        if current_assertions_sha256 != entry.assertions_sha256 {
            return Ok(ConsumerRunPlan::Refuse {
                outcome: RunOutcome::AssertionsChanged,
                miss_reason: CacheMissReason::FlowChanged,
                exit_code: 6,
                detail: format!(
                    "the flow's assertions changed (manifest {}, now {}); only a reviewed remember can accept that",
                    entry.assertions_sha256, current_assertions_sha256
                ),
                flow_change: None,
                manifest_assertions_sha256: Some(entry.assertions_sha256),
                current_assertions_sha256: Some(current_assertions_sha256),
            });
        }

        let all_runs = store
            .fetch_logical_walk_and_runs(req.identity)?
            .map(|(_, runs)| runs)
            .unwrap_or_default();

        let matching_closure_runs: Vec<ValidationRunRecord> = all_runs
            .into_iter()
            .filter(|r| r.closure_sha256.as_deref() == Some(entry.closure_sha256.as_str()))
            .collect();

        let rt_key = rt.unwrap_or("");
        let exact_run = matching_closure_runs.iter().find(|r| {
            r.build_fingerprint == norm.build_fingerprint
                && r.runtime.as_deref().unwrap_or("") == rt_key
        });

        let previous_successful = matching_closure_runs
            .iter()
            .filter(|r| r.passed && r.runtime.as_deref().unwrap_or("") == rt_key)
            .max_by(|a, b| a.validated_at.cmp(&b.validated_at))
            .cloned()
            .or_else(|| {
                matching_closure_runs
                    .iter()
                    .filter(|r| r.passed)
                    .max_by(|a, b| a.validated_at.cmp(&b.validated_at))
                    .cloned()
            });

        if let Some(run) = exact_run {
            if run.passed {
                return Ok(ConsumerRunPlan::Hit {
                    entry,
                    flow_path: canonical_flow,
                    flow_sha256: main_sha256,
                    closure_sha256: closure.sha256,
                    assertions_sha256: current_assertions_sha256,
                    run: run.clone(),
                });
            }
            if req.stale_build == Some(StaleBuildPolicy::ReplayLastVerified) {
                return Ok(ConsumerRunPlan::ReplayInPlace {
                    entry,
                    flow_path: canonical_flow,
                    flow_sha256: main_sha256,
                    closure_sha256: closure.sha256,
                    assertions_sha256: current_assertions_sha256,
                    reason: CacheMissReason::FailedValidation,
                    previous_successful,
                });
            }
            return Ok(ConsumerRunPlan::Refuse {
                outcome: RunOutcome::Failed,
                miss_reason: CacheMissReason::FailedValidation,
                exit_code: 1,
                detail: "the latest validation on this build failed".to_string(),
                flow_change: None,
                manifest_assertions_sha256: None,
                current_assertions_sha256: None,
            });
        }

        let miss_reason = if matching_closure_runs.is_empty() {
            CacheMissReason::NotValidated
        } else {
            CacheMissReason::StaleBuild
        };

        if req.stale_build == Some(StaleBuildPolicy::ReplayLastVerified) {
            return Ok(ConsumerRunPlan::ReplayInPlace {
                entry,
                flow_path: canonical_flow,
                flow_sha256: main_sha256,
                closure_sha256: closure.sha256,
                assertions_sha256: current_assertions_sha256,
                reason: miss_reason,
                previous_successful,
            });
        }

        let (outcome, exit_code, detail) = match miss_reason {
            CacheMissReason::NotValidated => (
                RunOutcome::NotValidated,
                6,
                "this manifest entry has never been validated in this store".to_string(),
            ),
            _ => (
                RunOutcome::Failed,
                1,
                "this manifest entry was validated on a different build".to_string(),
            ),
        };

        Ok(ConsumerRunPlan::Refuse {
            outcome,
            miss_reason,
            exit_code,
            detail,
            flow_change: None,
            manifest_assertions_sha256: None,
            current_assertions_sha256: None,
        })
    }

    /// Record post-run validation in `store` for an unchanged manifest entry WITHOUT modifying
    /// `deja.walks.json` (`DEJA-51`).
    ///
    /// Re-verifies BOTH `closureSha256` and `assertionsSha256` on disk before writing anything.
    pub fn record_post_run_validation(
        &self,
        store: &SqliteWalkStore,
        identity: &WalkIdentity,
        validation: &RecordValidationInput,
    ) -> Result<StoredWalk, StoreError> {
        let entry = match self.resolve_manifest_entry(store, identity)? {
            Ok(e) => e,
            Err(reason) => {
                return Err(StoreError::ConsumerFlow(format!(
                    "manifest has no matching entry ({})",
                    reason.as_str()
                )));
            }
        };

        let (_, closure, main_sha256, current_assertions_sha256) =
            match self.inspect_entry(&entry)? {
                InspectedConsumerEntry::Missing { file } => {
                    return Err(StoreError::CandidateNotFound(file));
                }
                InspectedConsumerEntry::Ok {
                    canonical_flow,
                    closure,
                    main_sha256,
                    assertions_sha256,
                } => (canonical_flow, closure, main_sha256, assertions_sha256),
            };

        if closure.sha256 != entry.closure_sha256 {
            return Err(StoreError::Core(CoreError::FlowChanged {
                manifest_closure_sha256: entry.closure_sha256,
                current_closure_sha256: closure.sha256,
            }));
        }
        if current_assertions_sha256 != entry.assertions_sha256 {
            return Err(StoreError::Core(CoreError::AssertionHashChanged {
                manifest_assertions_sha256: entry.assertions_sha256,
                current_assertions_sha256,
            }));
        }

        store.upsert_walk(&UpsertWalkInput {
            identity: identity.clone(),
            flow_path: entry.flow,
            flow_sha256: main_sha256,
            subflow_files: None,
            legacy_closure: false,
            closure_sha256: Some(entry.closure_sha256),
            validation: validation.clone(),
            exploration: None,
            repair_history: None,
        })
    }

    /// Persist a verified consumer flow to SQLite and (when changed) update `deja.walks.json`
    /// under `.deja/manifest.lock`, enforcing `ManifestOwnerRules`, assertion-change guards,
    /// forbidden fixture scanning, and optimistic `ManifestChanged` detection.
    pub fn save_verified_manifest_walk(
        &self,
        store: &SqliteWalkStore,
        options: &SaveManifestWalkOptions,
    ) -> Result<(StoredWalk, bool), StoreError> {
        let norm = normalize_walk_identity(&options.identity)?;
        let adapter = norm.adapter.clone().ok_or_else(|| {
            StoreError::InvalidIdentity("A manifest walk needs the adapter in its identity".into())
        })?;
        let flow = check_manifest_flow_path(&options.flow_path, "flow")?;
        let root = self.canonical_root()?;
        let abs = lexical_normalize(&root.join(&flow));
        if !path_inside(&root, &abs) {
            return Err(StoreError::ConsumerFlow(format!(
                "flow escapes workspace: {flow}"
            )));
        }
        let canonical = fs::canonicalize(&abs).map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                StoreError::CandidateNotFound(flow.clone())
            } else {
                StoreError::Io(err)
            }
        })?;
        if !path_inside(&root, &canonical) || !fs::metadata(&canonical)?.is_file() {
            return Err(StoreError::ConsumerFlow(format!(
                "the flow must be a file inside the workspace: {flow}"
            )));
        }

        let closure = collect_flow_closure(&root, &canonical)?;
        let main = closure
            .files
            .iter()
            .find(|f| f.path == closure.main)
            .ok_or_else(|| {
                StoreError::ConsumerFlow("closure is missing its main flow".to_string())
            })?;
        let assertions = closure_assertions(&closure)?;
        let assertions_hash = assertions_sha256(&closure)?;

        self.with_manifest_lock(|| {
            let manifest = self.load()?;
            let probe = ManifestWalk {
                goal: norm.goal.clone(),
                app_id: norm.app_id.clone(),
                platform: norm.platform.clone(),
                adapter: adapter.clone(),
                browser: norm.browser.clone(),
                flow: flow.clone(),
                closure_sha256: closure.sha256.clone(),
                assertions_sha256: assertions_hash.clone(),
                owner_rules: None,
                profile: None,
            };
            let target_key = manifest_walk_key(&probe)?;
            let existing = manifest
                .walks
                .iter()
                .find(|e| manifest_walk_key(e).ok().as_deref() == Some(target_key.as_str()))
                .cloned();

            if matches!(
                options.validation.origin,
                Some(WalkOrigin::Verify | WalkOrigin::ReplayedOnNewBuild)
            ) {
                let Some(ref ex) = existing else {
                    return Err(StoreError::ConsumerFlow(format!(
                        "the manifest has no entry for {:?}; only remember can add one",
                        norm.goal
                    )));
                };
                if ex.flow != flow {
                    return Err(StoreError::ConsumerFlow(format!(
                        "the manifest lists {}, not {flow}; only remember can move a flow",
                        ex.flow
                    )));
                }
                if options.validation.origin == Some(WalkOrigin::ReplayedOnNewBuild)
                    && ex.closure_sha256 != closure.sha256
                {
                    return Err(StoreError::Core(CoreError::FlowChanged {
                        manifest_closure_sha256: ex.closure_sha256.clone(),
                        current_closure_sha256: closure.sha256.clone(),
                    }));
                }
            }

            check_owner_rules(
                existing.as_ref().and_then(|e| e.owner_rules.as_ref()),
                &assertions,
                Some(&closure),
            )?;

            if let Some(ref ex) = existing
                && ex.assertions_sha256 != assertions_hash
            {
                let may_accept = matches!(
                    options.validation.origin,
                    Some(WalkOrigin::Remember | WalkOrigin::Explore)
                ) && options.accept_assertion_change;
                if !may_accept {
                    return Err(StoreError::Core(CoreError::AssertionHashChanged {
                        manifest_assertions_sha256: ex.assertions_sha256.clone(),
                        current_assertions_sha256: assertions_hash.clone(),
                    }));
                }
            }

            let next_entry = ManifestWalk {
                goal: existing
                    .as_ref()
                    .map(|e| e.goal.clone())
                    .unwrap_or_else(|| norm.goal.clone()),
                app_id: norm.app_id.clone(),
                platform: norm.platform.clone(),
                adapter: adapter.clone(),
                browser: norm.browser.clone(),
                flow: flow.clone(),
                closure_sha256: closure.sha256.clone(),
                assertions_sha256: assertions_hash.clone(),
                owner_rules: existing.as_ref().and_then(|e| e.owner_rules.clone()),
                profile: existing.as_ref().and_then(|e| e.profile.clone()),
            };

            let manifest_changed = existing.as_ref() != Some(&next_entry);
            let mut next_walks: Vec<ManifestWalk> = manifest
                .walks
                .iter()
                .filter(|e| manifest_walk_key(e).ok().as_deref() != Some(target_key.as_str()))
                .cloned()
                .collect();
            next_walks.push(next_entry.clone());
            let next_manifest = WalkManifest {
                manifest_version: manifest.manifest_version,
                hash_version: manifest.hash_version,
                walks: next_walks,
            };
            if manifest_changed {
                let _ = self.check_manifest(&next_manifest)?;
            }

            // Write memory first, then manifest last (atomic write under .deja/manifest.lock).
            let stored = store.upsert_walk(&UpsertWalkInput {
                identity: options.identity.clone(),
                flow_path: flow.clone(),
                flow_sha256: main.sha256.clone(),
                subflow_files: None,
                legacy_closure: false,
                closure_sha256: Some(closure.sha256.clone()),
                validation: options.validation.clone(),
                exploration: None,
                repair_history: None,
            })?;

            if manifest_changed {
                // Re-read under lock to detect concurrent manifest edits.
                let current = self.load()?;
                let current_entry = current
                    .walks
                    .iter()
                    .find(|e| manifest_walk_key(e).ok().as_deref() == Some(target_key.as_str()));
                if current_entry != existing.as_ref() {
                    return Err(StoreError::ManifestChanged(
                        "The manifest changed while this run was saving; nothing was written. Run again."
                            .to_string(),
                    ));
                }
                self.write_manifest(&next_manifest)?;
            }

            Ok((stored, manifest_changed))
        })
    }
}
