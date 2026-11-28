//! `maestro/v1` flow execution engine ([`FlowExecutor`]), scoped [`EnvStack`] interpolation,
//! workspace-confined subflow/media/screenshot path resolution, lifecycle hooks, and telemetry.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::future::Future;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deja_core::{
    LaunchArgumentValue, MAX_DEPTH, MAX_FILE_BYTES, MAX_MEDIA_BYTES, ProviderKeyRedactor, Redactor,
    WalkFailure, create_redactor, provider_key_redactor,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::backoff::{PollSchedule, SettleTracker};
use crate::capabilities::{DriverCapability, VisibleContext};
use crate::classifier::{
    ClassificationInput, FailedStepContext, FailureClassifier, StepFailureKind,
};
use crate::driver::{
    ActAction, ActPayload, DriverError, KeyPress, LaunchOptions, OpenLinkOptions, PlatformDriver,
    SwipeParams, UiHierarchy, UiPoint, UiSize, WaitSelectorRequest, is_valid_hex_hash,
};
use crate::parser::{
    AppTargetStep, AssertNotVisibleStep, AssertVisibleStep, Condition, EraseTextStep,
    ExtendedWaitUntilStep, FlowParseError, FlowStep, InputTextStep, LaunchAppStep, OpenLinkStep,
    ParsedFlow, PressKeyStep, RepeatStep, RetryStep, RunFlowStep, RunFlowTarget, ScrollDirection,
    ScrollUntilVisibleStep, SwipeStep, TakeScreenshotStep, TapStep, WaitForAnimationToEndStep,
    parse_flow,
};
use crate::selector::{self, CompiledSelector, Selector, build_visible_context};
use crate::timing::{RunTimingBreakdown, StepTiming};

/// Maximum byte length produced by a single `${VAR}` interpolation pass (64 KiB).
pub const MAX_INTERPOLATED_BYTES: usize = 64 * 1024;

/// Maximum iterations allowed for a `repeat` loop (`times` or `while`).
pub const MAX_REPEAT_ITERATIONS: usize = 1_000;

/// Maximum retries allowed for a `retry` block (`maxRetries`).
pub const MAX_RETRIES: usize = 20;

/// Maximum nested `runFlow` / `retry` file depth (matches `deja_core::MAX_DEPTH`).
pub const MAX_FLOW_DEPTH: usize = MAX_DEPTH;

/// Default timeout for selector resolution on actions and assertions.
pub const DEFAULT_STEP_TIMEOUT: Duration = Duration::from_secs(5);

/// Default timeout for `waitForAnimationToEnd`.
pub const DEFAULT_ANIMATION_TIMEOUT: Duration = Duration::from_secs(5);

/// Default hold-before-lift duration in milliseconds for `scrollUntilVisible` on drivers
/// advertising [`DriverCapability::HoldScroll`].
pub const DEFAULT_HOLD_SCROLL_MS: u64 = 150;

/// Default number of consecutive identical non-truncated hierarchy polls before a missing
/// action selector (`tapOn`/`longPressOn`/`doubleTapOn`) fails fast.
pub const DEFAULT_ACTION_FAIL_FAST_SETTLE_POLLS: usize = 3;

/// Errors emitted internally during interpolation, confinement, or execution limits.
#[derive(Debug, Error)]
pub enum FlowExecutionError {
    #[error("interpolated string exceeds maximum size of {limit} bytes")]
    InterpolationLimitExceeded { limit: usize },

    #[error("path confinement violation: {0}")]
    PathConfinement(String),

    #[error("execution limit exceeded: {0}")]
    LimitExceeded(String),

    #[error(transparent)]
    Parse(#[from] FlowParseError),

    #[error(transparent)]
    Driver(#[from] DriverError),
}

/// Scoped environment variable stack supporting single-pass, non-recursive `${VAR}` interpolation.
///
/// Child frames pushed for `runFlow`/`retry` are popped on exit so child variables never leak
/// back into the parent scope.
#[derive(Debug, Clone, Default)]
pub struct EnvStack {
    frames: Vec<BTreeMap<String, String>>,
}

impl EnvStack {
    pub fn new(base: BTreeMap<String, String>) -> Self {
        Self { frames: vec![base] }
    }

    pub fn push(&mut self, frame: BTreeMap<String, String>) {
        self.frames.push(frame);
    }

    pub fn pop(&mut self) -> Option<BTreeMap<String, String>> {
        self.frames.pop()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        for frame in self.frames.iter().rev() {
            if let Some(val) = frame.get(key) {
                return Some(val.as_str());
            }
        }
        None
    }

    /// Flattens the stack into a single map (innermost frames shadow outer frames).
    pub fn merged(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for frame in &self.frames {
            for (k, v) in frame {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }

    /// Performs single-pass, non-recursive `${VAR}` interpolation over `input`.
    ///
    /// Only variable names matching `^[A-Za-z_][A-Za-z0-9_]*$` are eligible for substitution,
    /// substituted values are never re-scanned, and output is capped at [`MAX_INTERPOLATED_BYTES`].
    pub fn interpolate(&self, input: &str) -> Result<String, FlowExecutionError> {
        let bytes = input.as_bytes();
        let mut out = String::with_capacity(input.len().min(MAX_INTERPOLATED_BYTES));
        let mut i = 0;

        while i < bytes.len() {
            if i + 1 < bytes.len() && bytes[i] == b'$' && bytes[i + 1] == b'{' {
                let name_start = i + 2;
                if let Some(rel_close) = input[name_start..].find('}') {
                    let var_name = &input[name_start..name_start + rel_close];
                    if is_valid_env_ident(var_name)
                        && let Some(val) = self.get(var_name)
                    {
                        push_bounded(&mut out, val)?;
                        i = name_start + rel_close + 1;
                        continue;
                    }
                    // Unmatched or non-identifier `${...}`: emit literally and advance past `}`.
                    let literal = &input[i..name_start + rel_close + 1];
                    push_bounded(&mut out, literal)?;
                    i = name_start + rel_close + 1;
                    continue;
                }
            }
            let ch = input[i..].chars().next().expect("valid UTF-8");
            push_char_bounded(&mut out, ch)?;
            i += ch.len_utf8();
        }

        Ok(out)
    }

    /// Interpolates every string field inside a [`Selector`] recursively.
    pub fn interpolate_selector(
        &self,
        selector: &Selector,
    ) -> Result<Selector, FlowExecutionError> {
        let mut out = selector.clone();
        if let Some(ref id) = selector.id {
            out.id = Some(self.interpolate(id)?);
        }
        if let Some(ref text) = selector.text {
            out.text = Some(self.interpolate(text)?);
        }
        if let Some(ref below) = selector.below {
            out.below = Some(Box::new(self.interpolate_selector(below)?));
        }
        if let Some(ref above) = selector.above {
            out.above = Some(Box::new(self.interpolate_selector(above)?));
        }
        if let Some(ref left_of) = selector.left_of {
            out.left_of = Some(Box::new(self.interpolate_selector(left_of)?));
        }
        if let Some(ref right_of) = selector.right_of {
            out.right_of = Some(Box::new(self.interpolate_selector(right_of)?));
        }
        if let Some(ref child_of) = selector.child_of {
            out.child_of = Some(Box::new(self.interpolate_selector(child_of)?));
        }
        if let Some(ref contains_child) = selector.contains_child {
            out.contains_child = Some(Box::new(self.interpolate_selector(contains_child)?));
        }
        if !selector.contains_descendants.is_empty() {
            let mut descs = Vec::with_capacity(selector.contains_descendants.len());
            for d in &selector.contains_descendants {
                descs.push(self.interpolate_selector(d)?);
            }
            out.contains_descendants = descs;
        }
        Ok(out)
    }
}

fn is_valid_env_ident(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn push_bounded(out: &mut String, s: &str) -> Result<(), FlowExecutionError> {
    if out.len().saturating_add(s.len()) > MAX_INTERPOLATED_BYTES {
        return Err(FlowExecutionError::InterpolationLimitExceeded {
            limit: MAX_INTERPOLATED_BYTES,
        });
    }
    out.push_str(s);
    Ok(())
}

fn push_char_bounded(out: &mut String, ch: char) -> Result<(), FlowExecutionError> {
    if out.len().saturating_add(ch.len_utf8()) > MAX_INTERPOLATED_BYTES {
        return Err(FlowExecutionError::InterpolationLimitExceeded {
            limit: MAX_INTERPOLATED_BYTES,
        });
    }
    out.push(ch);
    Ok(())
}

/// Configuration for [`FlowExecutor`].
#[derive(Debug, Clone)]
pub struct FlowExecutionConfig {
    pub workspace_root: PathBuf,
    pub flow_dir: Option<PathBuf>,
    pub screenshot_dir: Option<PathBuf>,
    /// When `true`, `takeScreenshot` targets are strictly confined inside `screenshot_dir`
    /// (rejecting `..`, absolute paths, symlinks, or writes anywhere else in the workspace).
    pub screenshot_confine_to_dir: bool,
    /// When `true`, `takeScreenshot` uses atomic create-new (`O_CREAT | O_EXCL`) and refuses
    /// to overwrite existing files or symbolic links.
    pub screenshot_no_clobber: bool,
    pub app_id_override: Option<String>,
    pub env: BTreeMap<String, String>,
    pub default_step_timeout: Duration,
    pub default_animation_timeout: Duration,
    pub flow_timeout: Option<Duration>,
    /// When `Some(n)`, missing action selectors (`tapOn`, `longPressOn`, `doubleTapOn`, `swipe.from`)
    /// fail fast once the screen stays static & non-truncated for `n` consecutive polls.
    /// Explicit waits (`extendedWaitUntil`, `assertVisible`, `assertNotVisible`) ignore this and
    /// always wait up to their timeout.
    pub action_fail_fast_settle_polls: Option<usize>,
    pub include_system_ui: bool,
    pub redactor: Redactor,
    pub provider_redactor: ProviderKeyRedactor,
    pub raw_artifact_path: Option<String>,
    /// Optional immutable preflight closure snapshot keyed by workspace-relative `/` paths
    /// (matching [`deja_core::ClosureFile::source`]). When set, [`FlowExecutor::execute_file`]
    /// and nested `runFlow`/`retry` file steps read exclusively from this in-memory map and
    /// never reopen flow files from disk.
    pub closure_snapshot: Option<BTreeMap<String, Vec<u8>>>,
    /// When `true`, captures a scrubbed, bounded [`VisibleContext`] snapshot of the final screen
    /// on passing flows (reusing a fresh cached hierarchy observation or issuing one fresh
    /// `driver.hierarchy` call if none is fresh yet).
    pub capture_final_visible_context: bool,
}

impl FlowExecutionConfig {
    pub fn new(workspace_root: PathBuf) -> Self {
        let ws_str = workspace_root.to_string_lossy().into_owned();
        Self {
            workspace_root,
            flow_dir: None,
            screenshot_dir: None,
            screenshot_confine_to_dir: false,
            screenshot_no_clobber: false,
            app_id_override: None,
            env: BTreeMap::new(),
            default_step_timeout: DEFAULT_STEP_TIMEOUT,
            default_animation_timeout: DEFAULT_ANIMATION_TIMEOUT,
            flow_timeout: None,
            action_fail_fast_settle_polls: Some(DEFAULT_ACTION_FAIL_FAST_SETTLE_POLLS),
            include_system_ui: false,
            redactor: create_redactor(&ws_str, None, &[]),
            provider_redactor: provider_key_redactor(std::iter::empty::<(&str, &str)>()),
            raw_artifact_path: None,
            closure_snapshot: None,
            capture_final_visible_context: false,
        }
    }

    /// Populate an immutable in-memory snapshot of every file (flows, subflows, and media) from
    /// a preflight [`deja_core::FlowClosure`].
    #[must_use]
    pub fn with_closure_snapshot(mut self, closure: &deja_core::FlowClosure) -> Self {
        let mut map = BTreeMap::new();
        for file in &closure.files {
            map.insert(file.source.clone(), file.bytes.clone());
        }
        self.closure_snapshot = Some(map);
        self
    }
}

/// Complete structured outcome of executing a `maestro/v1` flow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowExecutionOutcome {
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    pub duration_ms: u64,
    pub timing: RunTimingBreakdown,
    pub assertions_executed: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_step: Option<FailedStepContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<WalkFailure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hook_errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub screenshots: Vec<PathBuf>,
    /// `true` when [`FlowExecutor`] already ran `driver.check_app_crash` to completion
    /// (without timeout truncation or cancellation), so callers do not wait through a second
    /// crash-report grace window.
    #[serde(default)]
    pub crash_probe_completed: bool,
    /// Optional scrubbed, bounded final visible context captured on passing flows when
    /// [`FlowExecutionConfig::capture_final_visible_context`] is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_visible_context: Option<VisibleContext>,
}

/// Platform-neutral `maestro/v1` YAML flow executor.
#[derive(Debug, Clone)]
pub struct FlowExecutor {
    config: FlowExecutionConfig,
}

struct TimeoutGuard {
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for TimeoutGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

type StepResult<T = ()> = Result<T, Box<FailedStepContext>>;

struct RuntimeState {
    canonical_workspace: PathBuf,
    platform: &'static str,
    env: EnvStack,
    app_id: Option<String>,
    step_counter: usize,
    assertions_executed: u64,
    timing: RunTimingBreakdown,
    call_stack: Vec<PathBuf>,
    last_hierarchy: Option<UiHierarchy>,
    last_hierarchy_is_fresh: bool,
    screenshots: Vec<PathBuf>,
    created_strict_screenshot_dir: bool,
    hook_errors: Vec<String>,
    run_started_epoch_ms: u64,
}

impl FlowExecutor {
    pub fn new(config: FlowExecutionConfig) -> Self {
        Self { config }
    }

    /// Parses and executes a `maestro/v1` YAML string.
    pub async fn execute_yaml<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        yaml: &str,
        cancel: &CancellationToken,
    ) -> FlowExecutionOutcome {
        let started = Instant::now();
        let canonical_workspace = match fs::canonicalize(&self.config.workspace_root) {
            Ok(p) => p,
            Err(err) => {
                return self.early_error_outcome(
                    started,
                    "parseFlow",
                    "Resolve workspace root",
                    StepFailureKind::ParseOrConfinement,
                    format!("cannot resolve workspace root: {err}"),
                );
            }
        };

        let parsed = match parse_flow(yaml) {
            Ok(p) => p,
            Err(err) => {
                return self.early_error_outcome(
                    started,
                    "parseFlow",
                    "Parse flow YAML",
                    StepFailureKind::ParseOrConfinement,
                    err.to_string(),
                );
            }
        };

        let flow_dir = self
            .config
            .flow_dir
            .as_deref()
            .and_then(|d| {
                let lexical = if d.is_relative() {
                    lexical_normalize(&canonical_workspace.join(d))
                } else if let Ok(rel) = d.strip_prefix(&self.config.workspace_root) {
                    lexical_normalize(&canonical_workspace.join(rel))
                } else {
                    lexical_normalize(d)
                };
                let canonical = fs::canonicalize(&lexical).ok()?;
                if (lexical == canonical_workspace || lexical.starts_with(&canonical_workspace))
                    && (canonical == canonical_workspace
                        || canonical.starts_with(&canonical_workspace))
                {
                    Some(lexical)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| canonical_workspace.clone());

        self.run_root_flow(
            driver,
            canonical_workspace,
            flow_dir,
            None,
            parsed,
            cancel,
            started,
        )
        .await
    }

    /// Loads (with workspace confinement), parses, and executes a `maestro/v1` YAML file.
    pub async fn execute_file<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        flow_path: &Path,
        cancel: &CancellationToken,
    ) -> FlowExecutionOutcome {
        let started = Instant::now();
        let canonical_workspace = match fs::canonicalize(&self.config.workspace_root) {
            Ok(p) => p,
            Err(err) => {
                return self.early_error_outcome(
                    started,
                    "parseFlow",
                    "Resolve workspace root",
                    StepFailureKind::ParseOrConfinement,
                    format!("cannot resolve workspace root: {err}"),
                );
            }
        };

        // Resolve the root flow path lexically inside `canonical_workspace` (matching
        // `deja_core::collect_flow_closure`) so relative `runFlow`/`retry`/`addMedia` references
        // resolve against a symlink's own directory rather than its target's directory, while
        // still enforcing canonical workspace confinement and regular-file bounds.
        let lexical_file = if flow_path.is_relative() {
            lexical_normalize(&canonical_workspace.join(flow_path))
        } else if let Ok(rel) = flow_path.strip_prefix(&self.config.workspace_root) {
            lexical_normalize(&canonical_workspace.join(rel))
        } else {
            lexical_normalize(flow_path)
        };
        if lexical_file == canonical_workspace || !lexical_file.starts_with(&canonical_workspace) {
            return self.early_error_outcome(
                started,
                "parseFlow",
                "Load flow file",
                StepFailureKind::ParseOrConfinement,
                format!("flow file escapes workspace: {}", flow_path.display()),
            );
        }

        let (root_file, bytes) = if let Some(ref snapshot) = self.config.closure_snapshot {
            let rel_key = lexical_file
                .strip_prefix(&canonical_workspace)
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let Some(snap_bytes) = snapshot.get(&rel_key) else {
                return self.early_error_outcome(
                    started,
                    "parseFlow",
                    "Load flow file",
                    StepFailureKind::ParseOrConfinement,
                    format!("flow file missing from preflight closure snapshot: {rel_key}"),
                );
            };
            (lexical_file.clone(), snap_bytes.clone())
        } else {
            let canonical_file = match confine_existing_file(
                &canonical_workspace,
                &lexical_file,
                MAX_FILE_BYTES,
                "flow",
            ) {
                Ok(p) => p,
                Err(err) => {
                    return self.early_error_outcome(
                        started,
                        "parseFlow",
                        "Load flow file",
                        StepFailureKind::ParseOrConfinement,
                        err.to_string(),
                    );
                }
            };

            let bytes = match read_bounded_file(&canonical_file, MAX_FILE_BYTES) {
                Ok(b) => b,
                Err(err) => {
                    return self.early_error_outcome(
                        started,
                        "parseFlow",
                        "Read flow file",
                        StepFailureKind::ParseOrConfinement,
                        err.to_string(),
                    );
                }
            };
            (canonical_file, bytes)
        };
        let yaml = String::from_utf8_lossy(&bytes);
        let parsed = match parse_flow(&yaml) {
            Ok(p) => p,
            Err(err) => {
                return self.early_error_outcome(
                    started,
                    "parseFlow",
                    "Parse flow YAML",
                    StepFailureKind::ParseOrConfinement,
                    err.to_string(),
                );
            }
        };

        let flow_dir = lexical_file
            .parent()
            .unwrap_or(&canonical_workspace)
            .to_path_buf();

        self.run_root_flow(
            driver,
            canonical_workspace,
            flow_dir,
            Some(root_file),
            parsed,
            cancel,
            started,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_root_flow<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        canonical_workspace: PathBuf,
        flow_dir: PathBuf,
        root_file: Option<PathBuf>,
        parsed: ParsedFlow,
        parent_cancel: &CancellationToken,
        started: Instant,
    ) -> FlowExecutionOutcome {
        let child_cancel = parent_cancel.child_token();
        let timed_out = Arc::new(AtomicBool::new(false));
        let _timeout_guard = if let Some(limit) = self.config.flow_timeout {
            let cancel_clone = child_cancel.clone();
            let flag = Arc::clone(&timed_out);
            let handle = tokio::spawn(async move {
                tokio::time::sleep(limit).await;
                flag.store(true, Ordering::SeqCst);
                cancel_clone.cancel();
            });
            TimeoutGuard {
                handle: Some(handle),
            }
        } else {
            TimeoutGuard { handle: None }
        };

        let run_started_epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let mut state = RuntimeState {
            canonical_workspace,
            platform: driver.platform(),
            env: EnvStack::new(self.config.env.clone()),
            app_id: self.config.app_id_override.clone(),
            step_counter: 0,
            assertions_executed: 0,
            timing: RunTimingBreakdown::default(),
            call_stack: root_file.into_iter().collect(),
            last_hierarchy: None,
            last_hierarchy_is_fresh: false,
            screenshots: Vec::new(),
            created_strict_screenshot_dir: false,
            hook_errors: Vec::new(),
            run_started_epoch_ms,
        };

        let exec_started = Instant::now();
        let result = match driver.ensure_session(&child_cancel).await {
            Ok(()) => {
                self.execute_parsed_flow(
                    driver,
                    &parsed,
                    &flow_dir,
                    false,
                    &mut state,
                    &child_cancel,
                )
                .await
            }
            Err(err) => Err(self.driver_err_to_failed_step(
                0,
                "ensureSession",
                "Ensure driver session".to_string(),
                err,
            )),
        };
        let flow_exec_ms = exec_started.elapsed().as_millis() as u64;
        let total_ms = started.elapsed().as_millis() as u64;
        state.timing.flow_exec_ms = flow_exec_ms;
        state.timing.total_ms = total_ms;

        let did_time_out = timed_out.load(Ordering::SeqCst) && !parent_cancel.is_cancelled();
        let scrubbed_app_id = state.app_id.as_deref().map(|id| self.scrub(id));

        match result {
            Ok(()) if !did_time_out => {
                let final_visible_context = if self.config.capture_final_visible_context {
                    let final_hierarchy =
                        if state.last_hierarchy_is_fresh && state.last_hierarchy.is_some() {
                            state.last_hierarchy.clone()
                        } else if !child_cancel.is_cancelled()
                            && let Some(app_id) = state.app_id.clone().filter(|id| !id.is_empty())
                        {
                            let remaining = self
                                .config
                                .flow_timeout
                                .map(|limit| limit.saturating_sub(started.elapsed()));
                            match remaining {
                                Some(rem) if rem.is_zero() => None,
                                Some(rem) => tokio::select! {
                                    biased;
                                    _ = child_cancel.cancelled() => None,
                                    res = tokio::time::timeout(
                                        rem,
                                        self.fetch_fresh_hierarchy_with_cache(
                                            driver,
                                            &app_id,
                                            false,
                                            &mut state,
                                            &child_cancel,
                                        ),
                                    ) => res.ok().and_then(Result::ok),
                                },
                                None => tokio::select! {
                                    biased;
                                    _ = child_cancel.cancelled() => None,
                                    res = self.fetch_fresh_hierarchy_with_cache(
                                        driver,
                                        &app_id,
                                        false,
                                        &mut state,
                                        &child_cancel,
                                    ) => res.ok(),
                                },
                            }
                        } else {
                            None
                        };
                    final_hierarchy.map(|h| {
                        build_visible_context(
                            &h.nodes,
                            Some(&self.config.redactor),
                            Some(&self.config.provider_redactor),
                            VisibleContext::DEFAULT_LIMIT,
                        )
                    })
                } else {
                    None
                };

                let total_ms = started.elapsed().as_millis() as u64;
                state.timing.total_ms = total_ms;

                FlowExecutionOutcome {
                    passed: true,
                    app_id: scrubbed_app_id,
                    duration_ms: total_ms,
                    timing: state.timing,
                    assertions_executed: state.assertions_executed,
                    failed_step: None,
                    failure: None,
                    hook_errors: state.hook_errors,
                    screenshots: state.screenshots,
                    crash_probe_completed: false,
                    final_visible_context,
                }
            }
            Ok(()) => {
                let failed_step = FailedStepContext {
                    step_index: state.step_counter,
                    command: "flow".to_string(),
                    label: "Flow timeout".to_string(),
                    kind: StepFailureKind::Timeout,
                    message: "flow exceeded configured timeout".to_string(),
                    failed_app_id: scrubbed_app_id.clone(),
                    visible_context: None,
                    crash_signal: None,
                };
                let failure = FailureClassifier::classify(&ClassificationInput {
                    passed: false,
                    timed_out: true,
                    failed_step: Some(&failed_step),
                    crash_signal: None,
                    raw_artifact_path: self.config.raw_artifact_path.clone(),
                });
                FlowExecutionOutcome {
                    passed: false,
                    app_id: scrubbed_app_id,
                    duration_ms: total_ms,
                    timing: state.timing,
                    assertions_executed: state.assertions_executed,
                    failed_step: Some(failed_step),
                    failure,
                    hook_errors: state.hook_errors,
                    screenshots: state.screenshots,
                    crash_probe_completed: false,
                    final_visible_context: None,
                }
            }
            Err(mut failed_step) => {
                if did_time_out && failed_step.kind == StepFailureKind::Cancelled {
                    failed_step.kind = StepFailureKind::Timeout;
                    failed_step.message = "flow exceeded configured timeout".to_string();
                }

                // Check OS/driver crash signal for the exact app targeted by the failing step
                // (e.g. a child subflow's `appId` or an explicit `launchApp` override) before
                // scrubbing and classifying, strictly bounded by remaining flow budget & caller cancel.
                let crash_app_id = failed_step
                    .failed_app_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .or_else(|| state.app_id.as_deref().filter(|id| !id.is_empty()));
                let crash_budget = self
                    .config
                    .flow_timeout
                    .map(|limit| limit.saturating_sub(started.elapsed()));
                let can_probe_crash = !did_time_out
                    && !child_cancel.is_cancelled()
                    && crash_budget.is_none_or(|rem| !rem.is_zero())
                    && failed_step.crash_signal.is_none()
                    && !matches!(
                        failed_step.kind,
                        StepFailureKind::Cancelled
                            | StepFailureKind::UnsupportedCapability
                            | StepFailureKind::ParseOrConfinement
                            | StepFailureKind::LimitExceeded
                    )
                    && driver
                        .capabilities()
                        .supports(DriverCapability::AppCrashReports);
                let mut crash_probe_completed = false;
                if can_probe_crash && let Some(app_id) = crash_app_id {
                    let probe_cancel = child_cancel.child_token();
                    let (completed, probe_result) = match crash_budget {
                        Some(rem) => tokio::select! {
                            biased;
                            _ = probe_cancel.cancelled() => (false, None),
                            timed = tokio::time::timeout(
                                rem,
                                driver.check_app_crash(
                                    app_id,
                                    state.run_started_epoch_ms,
                                    &probe_cancel,
                                ),
                            ) => match timed {
                                Ok(Ok(sig)) => (true, sig),
                                Ok(Err(_)) | Err(_) => {
                                    probe_cancel.cancel();
                                    (false, None)
                                }
                            },
                        },
                        None => tokio::select! {
                            biased;
                            _ = probe_cancel.cancelled() => (false, None),
                            res = driver.check_app_crash(
                                app_id,
                                state.run_started_epoch_ms,
                                &probe_cancel,
                            ) => match res {
                                Ok(sig) => (true, sig),
                                Err(_) => (false, None),
                            },
                        },
                    };
                    crash_probe_completed = completed;
                    if let Some(sig) = probe_result {
                        failed_step.crash_signal = Some(sig);
                    }
                }

                // Always scrub failure labels, messages, app_id, and crash detail before classification/serialization.
                self.scrub_failed_step(&mut failed_step);

                let failure = FailureClassifier::classify(&ClassificationInput {
                    passed: false,
                    timed_out: did_time_out,
                    failed_step: Some(&failed_step),
                    crash_signal: failed_step.crash_signal.as_ref(),
                    raw_artifact_path: self.config.raw_artifact_path.clone(),
                });

                FlowExecutionOutcome {
                    passed: false,
                    app_id: scrubbed_app_id,
                    duration_ms: total_ms,
                    timing: state.timing,
                    assertions_executed: state.assertions_executed,
                    failed_step: Some(*failed_step),
                    failure,
                    hook_errors: state.hook_errors,
                    screenshots: state.screenshots,
                    crash_probe_completed,
                    final_visible_context: None,
                }
            }
        }
    }

    fn early_error_outcome(
        &self,
        started: Instant,
        command: &str,
        label: &str,
        kind: StepFailureKind,
        message: String,
    ) -> FlowExecutionOutcome {
        let elapsed = started.elapsed().as_millis() as u64;
        let failed_step = FailedStepContext {
            step_index: 0,
            command: command.to_string(),
            label: self.scrub(label),
            kind,
            message: self.scrub(&message),
            failed_app_id: None,
            visible_context: None,
            crash_signal: None,
        };
        let failure = FailureClassifier::classify(&ClassificationInput {
            passed: false,
            timed_out: false,
            failed_step: Some(&failed_step),
            crash_signal: None,
            raw_artifact_path: self.config.raw_artifact_path.clone(),
        });
        FlowExecutionOutcome {
            passed: false,
            app_id: self
                .config
                .app_id_override
                .as_deref()
                .map(|id| self.scrub(id)),
            duration_ms: elapsed,
            timing: RunTimingBreakdown {
                total_ms: elapsed,
                ..RunTimingBreakdown::default()
            },
            assertions_executed: 0,
            failed_step: Some(failed_step),
            failure,
            hook_errors: Vec::new(),
            screenshots: Vec::new(),
            crash_probe_completed: false,
            final_visible_context: None,
        }
    }

    fn scrub(&self, text: &str) -> String {
        self.config
            .provider_redactor
            .redact(&self.config.redactor.redact(text))
    }

    fn scrub_failed_step(&self, ctx: &mut FailedStepContext) {
        ctx.label = self.scrub(&ctx.label);
        ctx.message = self.scrub(&ctx.message);
        if let Some(ref app_id) = ctx.failed_app_id {
            ctx.failed_app_id = Some(self.scrub(app_id));
        }
        if let Some(ref vc) = ctx.visible_context {
            ctx.visible_context = Some(vc.map_items(|item| self.scrub(item)));
        }
        if let Some(ref mut sig) = ctx.crash_signal
            && let Some(ref detail) = sig.detail
        {
            sig.detail = Some(self.scrub(detail));
        }
    }

    fn execute_parsed_flow<'a, D: PlatformDriver + ?Sized>(
        &'a self,
        driver: &'a D,
        flow: &'a ParsedFlow,
        flow_dir: &'a Path,
        is_child_flow: bool,
        state: &'a mut RuntimeState,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = StepResult> + Send + 'a>> {
        Box::pin(async move {
            // Push header env frame (interpolated in parent scope) BEFORE resolving header appId/url.
            let mut header_env = BTreeMap::new();
            for (k, v) in &flow.header.env {
                let interp = state.env.interpolate(v).map_err(|err| FailedStepContext {
                    step_index: state.step_counter,
                    command: "env".to_string(),
                    label: self.scrub(&format!("Interpolate header env `{k}`")),
                    kind: StepFailureKind::LimitExceeded,
                    message: self.scrub(&err.to_string()),
                    failed_app_id: None,
                    visible_context: None,
                    crash_signal: None,
                })?;
                header_env.insert(k.clone(), interp);
            }
            let pushed_header_env = !header_env.is_empty();
            if pushed_header_env {
                state.env.push(header_env);
            }

            // Resolve header target (`appId` or URL-only Web `url`) using loaded header `env`.
            // Explicit `config.app_id_override` takes precedence for the root flow; child flows
            // temporarily scope their own header target and restore the parent on return.
            let prev_app_id = state.app_id.clone();
            let raw_header_target = flow.header.app_id.as_deref().or(flow.header.url.as_deref());
            let header_target = match raw_header_target {
                Some(raw) => match state.env.interpolate(raw) {
                    Ok(resolved) => Some(resolved),
                    Err(err) => {
                        if pushed_header_env {
                            state.env.pop();
                        }
                        return Err(Box::new(FailedStepContext {
                            step_index: state.step_counter,
                            command: "header".to_string(),
                            label: "Interpolate flow header target".to_string(),
                            kind: StepFailureKind::LimitExceeded,
                            message: self.scrub(&err.to_string()),
                            failed_app_id: None,
                            visible_context: None,
                            crash_signal: None,
                        }));
                    }
                },
                None => None,
            };

            if is_child_flow {
                if let Some(target) = header_target {
                    state.app_id = Some(target);
                }
            } else if self.config.app_id_override.is_none()
                && let Some(target) = header_target
            {
                state.app_id = Some(target);
            }

            // 1. onFlowStart -> 2. main steps.
            let mut primary_result = self
                .execute_steps(driver, &flow.header.on_flow_start, flow_dir, state, cancel)
                .await;
            if primary_result.is_ok() {
                primary_result = self
                    .execute_steps(driver, &flow.steps, flow_dir, state, cancel)
                    .await;
            }

            // Capture the primary failing app target BEFORE running `onFlowComplete` hooks so
            // a cleanup hook launching another app cannot overwrite the primary crash-probe target.
            if let Err(ref mut failed) = primary_result
                && failed.failed_app_id.is_none()
            {
                failed.failed_app_id = state.app_id.clone();
            }

            // 3. onFlowComplete acts like `finally` on non-cancelled runs, preserving primary error.
            if !cancel.is_cancelled() && !flow.header.on_flow_complete.is_empty() {
                let complete_result = self
                    .execute_steps(
                        driver,
                        &flow.header.on_flow_complete,
                        flow_dir,
                        state,
                        cancel,
                    )
                    .await;
                if let Err(mut hook_err) = complete_result {
                    if hook_err.failed_app_id.is_none() {
                        hook_err.failed_app_id = state.app_id.clone();
                    }
                    if primary_result.is_err() {
                        self.scrub_failed_step(&mut hook_err);
                        warn!(
                            command = %hook_err.command,
                            "onFlowComplete hook failed after primary flow failure; preserving primary error"
                        );
                        state
                            .hook_errors
                            .push(format!("{}: {}", hook_err.label, hook_err.message));
                    } else {
                        primary_result = Err(hook_err);
                    }
                }
            }

            if is_child_flow {
                state.app_id = prev_app_id;
            }
            if pushed_header_env {
                state.env.pop();
            }

            primary_result
        })
    }

    fn execute_steps<'a, D: PlatformDriver + ?Sized>(
        &'a self,
        driver: &'a D,
        steps: &'a [FlowStep],
        flow_dir: &'a Path,
        state: &'a mut RuntimeState,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = StepResult> + Send + 'a>> {
        Box::pin(async move {
            for step in steps {
                if cancel.is_cancelled() {
                    return Err(Box::new(FailedStepContext {
                        step_index: state.step_counter,
                        command: step.command_name().to_string(),
                        label: step.display_label(),
                        kind: StepFailureKind::Cancelled,
                        message: "flow execution cancelled".to_string(),
                        failed_app_id: None,
                        visible_context: None,
                        crash_signal: None,
                    }));
                }
                self.execute_step(driver, step, flow_dir, state, cancel)
                    .await?;
            }
            Ok(())
        })
    }

    fn execute_step<'a, D: PlatformDriver + ?Sized>(
        &'a self,
        driver: &'a D,
        step: &'a FlowStep,
        flow_dir: &'a Path,
        state: &'a mut RuntimeState,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = StepResult> + Send + 'a>> {
        Box::pin(async move {
            match step {
                FlowStep::RunFlow(s) => {
                    return self.exec_run_flow(driver, s, flow_dir, state, cancel).await;
                }
                FlowStep::Repeat(s) => {
                    return self.exec_repeat(driver, s, flow_dir, state, cancel).await;
                }
                FlowStep::Retry(s) => {
                    return self.exec_retry(driver, s, flow_dir, state, cancel).await;
                }
                _ => {}
            }

            let step_index = state.step_counter;
            state.step_counter = state.step_counter.saturating_add(1);
            let command = step.command_name();
            let started = Instant::now();
            let mut settled_early = false;
            let mut retries: u32 = 0;

            let res = match step {
                FlowStep::LaunchApp(s) => {
                    self.exec_launch_app(driver, s, step_index, state, cancel)
                        .await
                }
                FlowStep::StopApp(s) => {
                    self.exec_app_lifecycle(driver, s, "stopApp", step_index, state, cancel)
                        .await
                }
                FlowStep::KillApp(s) => {
                    self.exec_app_lifecycle(driver, s, "killApp", step_index, state, cancel)
                        .await
                }
                FlowStep::ClearState(s) => {
                    self.exec_app_lifecycle(driver, s, "clearState", step_index, state, cancel)
                        .await
                }
                FlowStep::ClearKeychain => {
                    self.exec_clear_keychain(driver, step_index, cancel).await
                }
                FlowStep::OpenLink(s) => {
                    self.exec_open_link(driver, s, step_index, state, cancel)
                        .await
                }
                FlowStep::AddMedia(s) => {
                    self.exec_add_media(driver, s, flow_dir, step_index, state, cancel)
                        .await
                }
                FlowStep::TapOn(s) => self
                    .exec_tap(driver, s, TapKind::Tap, step_index, state, cancel)
                    .await
                    .map(|(early, r)| {
                        settled_early = early;
                        retries = r;
                    }),
                FlowStep::LongPressOn(s) => self
                    .exec_tap(driver, s, TapKind::LongPress, step_index, state, cancel)
                    .await
                    .map(|(early, r)| {
                        settled_early = early;
                        retries = r;
                    }),
                FlowStep::DoubleTapOn(s) => self
                    .exec_tap(driver, s, TapKind::DoubleTap, step_index, state, cancel)
                    .await
                    .map(|(early, r)| {
                        settled_early = early;
                        retries = r;
                    }),
                FlowStep::InputText(s) => {
                    self.exec_input_text(driver, s, step_index, state, cancel)
                        .await
                }
                FlowStep::EraseText(s) => {
                    self.exec_erase_text(driver, s, step_index, state, cancel)
                        .await
                }
                FlowStep::PressKey(s) => {
                    self.exec_press_key(driver, s, step_index, state, cancel)
                        .await
                }
                FlowStep::HideKeyboard => {
                    self.exec_hide_keyboard(driver, step_index, state, cancel)
                        .await
                }
                FlowStep::Scroll => self.exec_scroll(driver, step_index, state, cancel).await,
                FlowStep::ScrollUntilVisible(s) => self
                    .exec_scroll_until_visible(driver, s, step_index, state, cancel)
                    .await
                    .map(|early| settled_early = early),
                FlowStep::Swipe(s) => self.exec_swipe(driver, s, step_index, state, cancel).await,
                FlowStep::WaitForAnimationToEnd(s) => self
                    .exec_wait_animation(driver, s, step_index, state, cancel)
                    .await
                    .map(|early| settled_early = early),
                FlowStep::TakeScreenshot(s) => {
                    self.exec_take_screenshot(driver, s, step_index, state, cancel)
                        .await
                }
                FlowStep::Back => self.exec_back(driver, step_index, state, cancel).await,
                FlowStep::AssertVisible(s) => {
                    state.assertions_executed = state.assertions_executed.saturating_add(1);
                    self.exec_assert_visible(driver, s, step_index, state, cancel)
                        .await
                        .map(|early| settled_early = early)
                }
                FlowStep::AssertNotVisible(s) => {
                    state.assertions_executed = state.assertions_executed.saturating_add(1);
                    self.exec_assert_not_visible(driver, s, step_index, state, cancel)
                        .await
                        .map(|early| settled_early = early)
                }
                FlowStep::ExtendedWaitUntil(s) => self
                    .exec_extended_wait(driver, s, step_index, state, cancel)
                    .await
                    .map(|early| settled_early = early),
                FlowStep::RunFlow(_) | FlowStep::Repeat(_) | FlowStep::Retry(_) => unreachable!(),
            };

            state.timing.steps.push(StepTiming {
                step_index,
                command: command.to_string(),
                duration_ms: started.elapsed().as_millis() as u64,
                retries,
                settled_early,
            });

            res
        })
    }

    fn resolve_app_id(
        &self,
        override_id: Option<&str>,
        state: &RuntimeState,
        step_index: usize,
        command: &str,
    ) -> StepResult<String> {
        if let Some(raw) = override_id {
            let interpolated = state
                .env
                .interpolate(raw)
                .map_err(|err| FailedStepContext {
                    step_index,
                    command: command.to_string(),
                    label: format!("{command} (interpolate appId)"),
                    kind: StepFailureKind::LimitExceeded,
                    message: self.scrub(&err.to_string()),
                    failed_app_id: None,
                    visible_context: None,
                    crash_signal: None,
                })?;
            return Ok(interpolated);
        }
        if let Some(ref current) = state.app_id {
            return Ok(current.clone());
        }
        if state.platform.eq_ignore_ascii_case("web") {
            return Ok(String::new());
        }
        Err(Box::new(FailedStepContext {
            step_index,
            command: command.to_string(),
            label: command.to_string(),
            kind: StepFailureKind::ParseOrConfinement,
            message: format!("`{command}` requires an `appId` in header or step parameters"),
            failed_app_id: None,
            visible_context: None,
            crash_signal: None,
        }))
    }

    fn require_capability<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        cap: DriverCapability,
        reason: &str,
        step_index: usize,
        command: &str,
        label: String,
    ) -> StepResult {
        driver
            .capabilities()
            .require(cap, driver.platform(), reason)
            .map_err(|err| {
                Box::new(FailedStepContext {
                    step_index,
                    command: command.to_string(),
                    label,
                    kind: StepFailureKind::UnsupportedCapability,
                    message: self.scrub(&err.to_string()),
                    failed_app_id: None,
                    visible_context: None,
                    crash_signal: None,
                })
            })
    }

    fn driver_err_to_failed_step(
        &self,
        step_index: usize,
        command: &str,
        label: String,
        err: DriverError,
    ) -> Box<FailedStepContext> {
        let (kind, crash_signal) = match &err {
            DriverError::Cancelled => (StepFailureKind::Cancelled, None),
            DriverError::Unsupported(_) => (StepFailureKind::UnsupportedCapability, None),
            DriverError::Timeout { .. } => (StepFailureKind::Timeout, None),
            DriverError::LaunchFailed { .. } | DriverError::NavigationFailed { .. } => {
                (StepFailureKind::NavigationFailed, None)
            }
            DriverError::AppCrashed(sig) => (StepFailureKind::AppCrash, Some(sig.clone())),
            DriverError::Transport { .. } | DriverError::SessionClosed(_) => {
                (StepFailureKind::Transport, None)
            }
            DriverError::InvalidArgument(_) => (StepFailureKind::ParseOrConfinement, None),
            DriverError::Io(_) => (StepFailureKind::Transport, None),
        };
        Box::new(FailedStepContext {
            step_index,
            command: command.to_string(),
            label: self.scrub(&label),
            kind,
            message: self.scrub(&err.to_string()),
            failed_app_id: None,
            visible_context: None,
            crash_signal,
        })
    }

    async fn exec_launch_app<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &LaunchAppStep,
        step_index: usize,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult {
        let app_id = self.resolve_app_id(step.app_id.as_deref(), state, step_index, "launchApp")?;
        let label = format!("Launch app \"{app_id}\"");

        if step.clear_state {
            self.require_capability(
                driver,
                DriverCapability::ClearState,
                "launchApp with clearState requires ClearState capability",
                step_index,
                "launchApp",
                label.clone(),
            )?;
        }
        if step.clear_keychain {
            self.require_capability(
                driver,
                DriverCapability::ClearKeychain,
                "launchApp with clearKeychain requires ClearKeychain capability",
                step_index,
                "launchApp",
                label.clone(),
            )?;
        }

        let mut args = BTreeMap::new();
        for (k, v) in &step.arguments {
            let resolved_v = match v {
                LaunchArgumentValue::String(s) => {
                    let interp = state.env.interpolate(s).map_err(|err| FailedStepContext {
                        step_index,
                        command: "launchApp".to_string(),
                        label: label.clone(),
                        kind: StepFailureKind::LimitExceeded,
                        message: self.scrub(&err.to_string()),
                        failed_app_id: None,
                        visible_context: None,
                        crash_signal: None,
                    })?;
                    LaunchArgumentValue::String(interp)
                }
                other => other.clone(),
            };
            args.insert(k.clone(), resolved_v);
        }

        let opts = LaunchOptions {
            clear_state: step.clear_state,
            clear_keychain: step.clear_keychain,
            stop_app: step.stop_app,
            permissions: step.permissions.clone(),
            arguments: args,
            env: state.env.merged(),
        };

        driver
            .launch_app(&app_id, &opts, cancel)
            .await
            .map_err(|err| {
                let mut ctx = self.driver_err_to_failed_step(step_index, "launchApp", label, err);
                ctx.failed_app_id = Some(app_id.clone());
                ctx
            })?;

        state.app_id = Some(app_id);
        state.last_hierarchy = None;
        state.last_hierarchy_is_fresh = false;
        Ok(())
    }

    async fn exec_app_lifecycle<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &AppTargetStep,
        command: &str,
        step_index: usize,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult {
        let app_id = self.resolve_app_id(step.app_id.as_deref(), state, step_index, command)?;
        let label = format!("{command} \"{app_id}\"");

        if command == "clearState" {
            self.require_capability(
                driver,
                DriverCapability::ClearState,
                "clearState requires ClearState capability",
                step_index,
                command,
                label.clone(),
            )?;
        }

        let res = match command {
            "stopApp" => driver.stop_app(&app_id, cancel).await,
            "killApp" => driver.kill_app(&app_id, cancel).await,
            "clearState" => driver.clear_state(&app_id, cancel).await,
            _ => unreachable!(),
        };
        res.map_err(|err| {
            let mut ctx = self.driver_err_to_failed_step(step_index, command, label, err);
            ctx.failed_app_id = Some(app_id.clone());
            ctx
        })?;
        state.last_hierarchy = None;
        state.last_hierarchy_is_fresh = false;
        Ok(())
    }

    async fn exec_clear_keychain<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step_index: usize,
        cancel: &CancellationToken,
    ) -> StepResult {
        let label = "Clear keychain".to_string();
        self.require_capability(
            driver,
            DriverCapability::ClearKeychain,
            "clearKeychain is not supported on this platform",
            step_index,
            "clearKeychain",
            label.clone(),
        )?;
        driver
            .clear_keychain(cancel)
            .await
            .map_err(|err| self.driver_err_to_failed_step(step_index, "clearKeychain", label, err))
    }

    async fn exec_open_link<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &OpenLinkStep,
        step_index: usize,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult {
        let url = state
            .env
            .interpolate(&step.link)
            .map_err(|err| FailedStepContext {
                step_index,
                command: "openLink".to_string(),
                label: "Open link".to_string(),
                kind: StepFailureKind::LimitExceeded,
                message: self.scrub(&err.to_string()),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            })?;
        let label = format!("Open link \"{}\"", self.scrub(&url));
        let opts = OpenLinkOptions {
            auto_verify: step.auto_verify,
            browser: step.browser,
        };
        driver
            .open_link(state.app_id.as_deref(), &url, &opts, cancel)
            .await
            .map_err(|err| self.driver_err_to_failed_step(step_index, "openLink", label, err))?;
        state.last_hierarchy = None;
        state.last_hierarchy_is_fresh = false;
        Ok(())
    }

    async fn exec_add_media<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &crate::parser::AddMediaStep,
        flow_dir: &Path,
        step_index: usize,
        state: &RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult {
        let label = format!("Add media ({} file(s))", step.files.len());
        if let Some(ref snapshot) = self.config.closure_snapshot {
            let staging = create_media_staging_dir().map_err(|err| FailedStepContext {
                step_index,
                command: "addMedia".to_string(),
                label: label.clone(),
                kind: StepFailureKind::ParseOrConfinement,
                message: self.scrub(&format!("failed to create media staging directory: {err}")),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            })?;
            let mut resolved = Vec::with_capacity(step.files.len());
            for (idx, raw_path) in step.files.iter().enumerate() {
                let staged_path = stage_snapshot_media_file(
                    &staging,
                    idx,
                    &state.canonical_workspace,
                    flow_dir,
                    raw_path,
                    snapshot,
                )
                .map_err(|err| FailedStepContext {
                    step_index,
                    command: "addMedia".to_string(),
                    label: label.clone(),
                    kind: StepFailureKind::ParseOrConfinement,
                    message: self.scrub(&err.to_string()),
                    failed_app_id: None,
                    visible_context: None,
                    crash_signal: None,
                })?;
                resolved.push(staged_path);
            }
            let res = driver
                .add_media(&resolved, cancel)
                .await
                .map_err(|err| self.driver_err_to_failed_step(step_index, "addMedia", label, err));
            drop(staging);
            return res;
        }

        let mut resolved = Vec::with_capacity(step.files.len());
        for raw_path in &step.files {
            let confined = confine_relative_read_path(
                &state.canonical_workspace,
                flow_dir,
                raw_path,
                MAX_MEDIA_BYTES,
                "media",
            )
            .map_err(|err| FailedStepContext {
                step_index,
                command: "addMedia".to_string(),
                label: label.clone(),
                kind: StepFailureKind::ParseOrConfinement,
                message: self.scrub(&err.to_string()),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            })?;
            resolved.push(confined.canonical);
        }
        driver
            .add_media(&resolved, cancel)
            .await
            .map_err(|err| self.driver_err_to_failed_step(step_index, "addMedia", label, err))
    }

    async fn dispatch_action<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        app_id: &str,
        action: ActAction,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> Result<(), DriverError> {
        self.dispatch_action_observed(driver, app_id, action, state, cancel)
            .await
            .map(|_| ())
    }

    /// Dispatches an action primitive (or compound `act_and_observe`), returning `true` when
    /// `CompoundAct` updated `state.last_hierarchy` with a fresh, non-truncated observation.
    async fn dispatch_action_observed<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        app_id: &str,
        action: ActAction,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> Result<bool, DriverError> {
        if driver
            .capabilities()
            .supports(DriverCapability::CompoundAct)
        {
            let known_hash = if self.config.include_system_ui {
                None
            } else {
                state
                    .last_hierarchy
                    .as_ref()
                    .filter(|h| !h.unchanged && !h.truncated)
                    .map(|h| {
                        if is_valid_hex_hash(&h.hierarchy_hash) {
                            h.hierarchy_hash.to_ascii_lowercase()
                        } else {
                            h.hierarchy_hash_hex()
                        }
                    })
            };
            let payload = ActPayload {
                app_id: app_id.to_string(),
                action,
                post_action_delay_ms: None,
                return_hierarchy: driver
                    .capabilities()
                    .supports(DriverCapability::AccessibilityTree),
                include_system_ui: self.config.include_system_ui,
                known_hash,
            };
            let outcome = driver.act_and_observe(&payload, cancel).await?;
            if !outcome.action_ok {
                state.last_hierarchy_is_fresh = false;
                return Err(DriverError::Transport {
                    operation: "act_and_observe".to_string(),
                    detail: outcome
                        .hierarchy_error
                        .unwrap_or_else(|| "compound action reported action_ok=false".to_string()),
                    transient: false,
                });
            }
            let mut got_complete_hierarchy = false;
            if let Some(h) = outcome.hierarchy {
                if h.unchanged {
                    if let Some(ref cached) = state.last_hierarchy {
                        let merged = h.merge_unchanged_with_cached_nodes(cached);
                        got_complete_hierarchy = !merged.truncated;
                        state.last_hierarchy = Some(merged);
                    } else {
                        state.last_hierarchy = None;
                    }
                } else {
                    got_complete_hierarchy = !h.truncated;
                    state.last_hierarchy = Some(h);
                }
            } else if outcome.hierarchy_error.is_some() {
                state.last_hierarchy = None;
            }
            state.last_hierarchy_is_fresh = got_complete_hierarchy;
            return Ok(got_complete_hierarchy);
        }

        match action {
            ActAction::Tap { x, y } => driver.tap_point(app_id, UiPoint::new(x, y), cancel).await?,
            ActAction::LongPress { x, y, duration_ms } => {
                driver
                    .long_press_point(app_id, UiPoint::new(x, y), duration_ms, cancel)
                    .await?;
            }
            ActAction::DoubleTap { x, y } => {
                driver
                    .double_tap_point(app_id, UiPoint::new(x, y), cancel)
                    .await?;
            }
            ActAction::Swipe {
                start,
                end,
                duration_ms,
                hold_duration_ms,
            } => {
                driver
                    .swipe(
                        &SwipeParams {
                            app_id: app_id.to_string(),
                            start,
                            end,
                            duration_ms,
                            hold_duration_ms,
                        },
                        cancel,
                    )
                    .await?;
            }
            ActAction::InputText { text } => driver.input_text(app_id, &text, cancel).await?,
            ActAction::EraseText { count } => driver.erase_text(app_id, count, cancel).await?,
            ActAction::PressKey {
                key,
                modifiers,
                count,
            } => {
                driver
                    .press_key(
                        app_id,
                        &KeyPress {
                            key,
                            modifiers,
                            count,
                        },
                        cancel,
                    )
                    .await?;
            }
            ActAction::HideKeyboard => driver.hide_keyboard(app_id, cancel).await?,
            ActAction::Back => driver.back(app_id, cancel).await?,
        }
        state.last_hierarchy_is_fresh = false;
        Ok(false)
    }

    #[allow(clippy::too_many_arguments)]
    async fn wait_selector<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        app_id: &str,
        selector: Selector,
        not_visible: bool,
        timeout: Duration,
        fail_fast_on_settle_polls: Option<usize>,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> Result<crate::driver::WaitSelectorOutcome, DriverError> {
        self.wait_selector_with_options(
            driver,
            app_id,
            selector,
            not_visible,
            false,
            timeout,
            fail_fast_on_settle_polls,
            state,
            cancel,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn wait_selector_with_options<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        app_id: &str,
        selector: Selector,
        not_visible: bool,
        require_hittable: bool,
        timeout: Duration,
        fail_fast_on_settle_polls: Option<usize>,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> Result<crate::driver::WaitSelectorOutcome, DriverError> {
        let req = WaitSelectorRequest {
            app_id: app_id.to_string(),
            selector: selector.clone(),
            not_visible,
            require_hittable,
            wait_stable: false,
            stable_polls: 2,
            fail_fast_on_settle_polls,
            timeout,
            include_system_ui: self.config.include_system_ui,
            return_hierarchy: true,
            initial_hierarchy: state.last_hierarchy.clone(),
            redactor: Some(self.config.redactor.clone()),
            provider_redactor: Some(self.config.provider_redactor.clone()),
        };

        // Regex, relational, indexed, state-filtered, and hittable-required selectors ALWAYS
        // execute locally.
        let outcome = if driver
            .capabilities()
            .supports(DriverCapability::ServerSideWait)
            && !require_hittable
            && selector.is_simple_literal_for_server_wait()
        {
            driver.wait_for_selector(&req, cancel).await?
        } else {
            selector::wait_local(driver, &req, cancel).await?
        };

        if let Some(ref h) = outcome.hierarchy
            && !h.unchanged
        {
            state.last_hierarchy_is_fresh = outcome.obtained_fresh_observation && !h.truncated;
            state.last_hierarchy = Some(h.clone());
        } else {
            state.last_hierarchy_is_fresh = false;
        }
        Ok(outcome)
    }

    async fn fetch_fresh_hierarchy<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        app_id: &str,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> Result<UiHierarchy, DriverError> {
        self.fetch_fresh_hierarchy_with_cache(
            driver,
            app_id,
            !self.config.include_system_ui,
            state,
            cancel,
        )
        .await
    }

    async fn fetch_fresh_hierarchy_with_cache<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        app_id: &str,
        allow_known_hash: bool,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> Result<UiHierarchy, DriverError> {
        let known_hash = if allow_known_hash {
            state
                .last_hierarchy
                .as_ref()
                .filter(|h| !h.unchanged && !h.truncated)
                .map(|h| {
                    if is_valid_hex_hash(&h.hierarchy_hash) {
                        h.hierarchy_hash.to_ascii_lowercase()
                    } else {
                        h.hierarchy_hash_hex()
                    }
                })
        } else {
            None
        };
        let resp = driver
            .hierarchy(
                app_id,
                self.config.include_system_ui,
                known_hash.as_deref(),
                cancel,
            )
            .await?;

        if resp.unchanged {
            if let Some(ref cached) = state.last_hierarchy {
                let merged = resp.merge_unchanged_with_cached_nodes(cached);
                state.last_hierarchy_is_fresh = !merged.truncated;
                state.last_hierarchy = Some(merged.clone());
                return Ok(merged);
            }
            // No cached nodes yet; re-fetch uncached.
            let fresh = driver
                .hierarchy(app_id, self.config.include_system_ui, None, cancel)
                .await?;
            if !fresh.unchanged {
                state.last_hierarchy_is_fresh = !fresh.truncated;
                state.last_hierarchy = Some(fresh.clone());
            } else {
                state.last_hierarchy_is_fresh = false;
            }
            return Ok(fresh);
        }

        state.last_hierarchy_is_fresh = !resp.truncated;
        state.last_hierarchy = Some(resp.clone());
        Ok(resp)
    }

    async fn exec_tap<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &TapStep,
        kind: TapKind,
        step_index: usize,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult<(bool, u32)> {
        let command = kind.command_name();
        let app_id = self.resolve_app_id(None, state, step_index, command)?;
        let selector = state
            .env
            .interpolate_selector(&step.selector)
            .map_err(|err| FailedStepContext {
                step_index,
                command: command.to_string(),
                label: self.scrub(step.label.as_deref().unwrap_or(command)),
                kind: StepFailureKind::LimitExceeded,
                message: self.scrub(&err.to_string()),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            })?;
        let label = self.scrub(
            &step
                .label
                .clone()
                .unwrap_or_else(|| format!("{} {}", kind.label_prefix(), selector.describe())),
        );

        let mut settled_early = false;
        let mut retries: u32 = 0;
        let target_point = if selector.has_element_criteria() {
            self.require_capability(
                driver,
                DriverCapability::AccessibilityTree,
                "selector-based tap requires AccessibilityTree capability",
                step_index,
                command,
                label.clone(),
            )?;

            let require_hittable = driver.platform() == "web";
            let wait = self
                .wait_selector_with_options(
                    driver,
                    &app_id,
                    selector.clone(),
                    false,
                    require_hittable,
                    self.config.default_step_timeout,
                    self.config.action_fail_fast_settle_polls,
                    state,
                    cancel,
                )
                .await
                .map_err(|err| {
                    self.driver_err_to_failed_step(step_index, command, label.clone(), err)
                })?;
            settled_early = wait.settled_early;

            let Some(node) = wait.matched_node else {
                if selector.optional {
                    debug!(command, selector = %self.scrub(&selector.describe()), "skipping optional tap miss");
                    return Ok((false, 0));
                }
                let occluded = require_hittable
                    && wait.hierarchy.as_ref().is_some_and(|h| {
                        CompiledSelector::compile(&selector)
                            .ok()
                            .and_then(|c| c.match_one(&h.nodes))
                            .is_some_and(|n| !n.hittable)
                    });
                let detail_msg = if occluded {
                    format!(
                        "element is occluded / not hittable for {}: {}",
                        selector.describe(),
                        wait.visible_context.format_summary()
                    )
                } else {
                    format!(
                        "element not found for {}: {}",
                        selector.describe(),
                        wait.visible_context.format_summary()
                    )
                };
                return Err(Box::new(FailedStepContext {
                    step_index,
                    command: command.to_string(),
                    label,
                    kind: StepFailureKind::SelectorNotFound,
                    message: self.scrub(&detail_msg),
                    failed_app_id: None,
                    visible_context: Some(wait.visible_context),
                    crash_signal: None,
                }));
            };

            if let Some(rel_point) = selector.point {
                let offset = rel_point.resolve(UiSize::new(node.frame.width, node.frame.height));
                UiPoint::new(node.frame.x + offset.x, node.frame.y + offset.y)
            } else {
                node.tap_point()
            }
        } else if let Some(pt_spec) = selector.point {
            let viewport = if !pt_spec.uses_percent() {
                UiSize::new(
                    crate::driver::DEFAULT_VIEWPORT_WIDTH,
                    crate::driver::DEFAULT_VIEWPORT_HEIGHT,
                )
            } else if let Some(ref h) = state.last_hierarchy {
                h.effective_viewport()
            } else if driver
                .capabilities()
                .supports(DriverCapability::AccessibilityTree)
            {
                self.fetch_fresh_hierarchy(driver, &app_id, state, cancel)
                    .await
                    .map(|h| h.effective_viewport())
                    .unwrap_or_else(|_| {
                        UiSize::new(
                            crate::driver::DEFAULT_VIEWPORT_WIDTH,
                            crate::driver::DEFAULT_VIEWPORT_HEIGHT,
                        )
                    })
            } else {
                UiSize::new(
                    crate::driver::DEFAULT_VIEWPORT_WIDTH,
                    crate::driver::DEFAULT_VIEWPORT_HEIGHT,
                )
            };
            pt_spec.resolve(viewport)
        } else {
            return Err(Box::new(FailedStepContext {
                step_index,
                command: command.to_string(),
                label,
                kind: StepFailureKind::ParseOrConfinement,
                message: "tap selector has neither element criteria nor point".to_string(),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            }));
        };

        let repeats = selector.repeat.unwrap_or(1).max(1) as usize;
        if repeats > MAX_REPEAT_ITERATIONS {
            return Err(Box::new(FailedStepContext {
                step_index,
                command: command.to_string(),
                label,
                kind: StepFailureKind::LimitExceeded,
                message: format!(
                    "tap repeat count {repeats} exceeds limit {MAX_REPEAT_ITERATIONS}"
                ),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            }));
        }

        let retry_if_no_change = selector.retry_tap_if_no_change.unwrap_or(false)
            && driver
                .capabilities()
                .supports(DriverCapability::AccessibilityTree);

        for rep in 0..repeats {
            let pre_tap_hash = if retry_if_no_change {
                if state.last_hierarchy_is_fresh
                    && let Some(h) = state.last_hierarchy.as_ref().filter(|h| !h.truncated)
                {
                    Some(h.hierarchy_hash_u64())
                } else {
                    match self
                        .fetch_fresh_hierarchy(driver, &app_id, state, cancel)
                        .await
                    {
                        Ok(h) if !h.truncated => Some(h.hierarchy_hash_u64()),
                        Ok(_) => None,
                        Err(err) => {
                            return Err(
                                self.driver_err_to_failed_step(step_index, command, label, err)
                            );
                        }
                    }
                }
            } else {
                None
            };

            let action = match kind {
                TapKind::Tap => ActAction::Tap {
                    x: target_point.x,
                    y: target_point.y,
                },
                TapKind::LongPress => ActAction::LongPress {
                    x: target_point.x,
                    y: target_point.y,
                    duration_ms: step.duration_ms,
                },
                TapKind::DoubleTap => ActAction::DoubleTap {
                    x: target_point.x,
                    y: target_point.y,
                },
            };
            let got_complete_post = self
                .dispatch_action_observed(driver, &app_id, action.clone(), state, cancel)
                .await
                .map_err(|err| {
                    self.driver_err_to_failed_step(step_index, command, label.clone(), err)
                })?;

            // Bounded single retry when `retryTapIfNoChange: true` and the chrome-stripped
            // non-truncated hierarchy hash did not change after the tap.
            if let Some(before_hash) = pre_tap_hash {
                let post_hash = if got_complete_post
                    && let Some(ref h) = state.last_hierarchy
                    && !h.truncated
                {
                    Some(h.hierarchy_hash_u64())
                } else {
                    match self
                        .fetch_fresh_hierarchy(driver, &app_id, state, cancel)
                        .await
                    {
                        Ok(h) if !h.truncated => Some(h.hierarchy_hash_u64()),
                        Ok(_) => None,
                        Err(err) => {
                            return Err(
                                self.driver_err_to_failed_step(step_index, command, label, err)
                            );
                        }
                    }
                };
                if post_hash == Some(before_hash) {
                    debug!(
                        command,
                        "retryTapIfNoChange detected unchanged hierarchy; retrying tap once"
                    );
                    retries = retries.saturating_add(1);
                    self.dispatch_action(driver, &app_id, action, state, cancel)
                        .await
                        .map_err(|err| {
                            self.driver_err_to_failed_step(step_index, command, label.clone(), err)
                        })?;
                }
            }

            if rep + 1 < repeats
                && let Some(delay_ms) = selector.delay_ms
                && delay_ms > 0
            {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        return Err(self.driver_err_to_failed_step(
                            step_index,
                            command,
                            label,
                            DriverError::Cancelled,
                        ));
                    }
                    _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                }
            }
        }

        if let Some(settle_ms) = selector.wait_to_settle_timeout_ms
            && settle_ms > 0
            && driver
                .capabilities()
                .supports(DriverCapability::AccessibilityTree)
        {
            self.wait_for_hierarchy_settle(
                driver,
                &app_id,
                Duration::from_millis(settle_ms),
                state,
                cancel,
            )
            .await
            .map_err(|err| self.driver_err_to_failed_step(step_index, command, label, err))?;
        }

        Ok((settled_early, retries))
    }

    async fn exec_input_text<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &InputTextStep,
        step_index: usize,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult {
        let app_id = self.resolve_app_id(None, state, step_index, "inputText")?;
        let text = state
            .env
            .interpolate(&step.text)
            .map_err(|err| FailedStepContext {
                step_index,
                command: "inputText".to_string(),
                label: "Input text".to_string(),
                kind: StepFailureKind::LimitExceeded,
                message: self.scrub(&err.to_string()),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            })?;
        self.dispatch_action(
            driver,
            &app_id,
            ActAction::InputText { text },
            state,
            cancel,
        )
        .await
        .map_err(|err| {
            self.driver_err_to_failed_step(step_index, "inputText", "Input text".into(), err)
        })
    }

    async fn exec_erase_text<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &EraseTextStep,
        step_index: usize,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult {
        let app_id = self.resolve_app_id(None, state, step_index, "eraseText")?;
        self.dispatch_action(
            driver,
            &app_id,
            ActAction::EraseText {
                count: step.characters_to_erase,
            },
            state,
            cancel,
        )
        .await
        .map_err(|err| {
            self.driver_err_to_failed_step(
                step_index,
                "eraseText",
                format!("Erase {} character(s)", step.characters_to_erase),
                err,
            )
        })
    }

    async fn exec_press_key<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &PressKeyStep,
        step_index: usize,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult {
        let app_id = self.resolve_app_id(None, state, step_index, "pressKey")?;
        let key = state
            .env
            .interpolate(&step.key)
            .map_err(|err| FailedStepContext {
                step_index,
                command: "pressKey".to_string(),
                label: "Press key".to_string(),
                kind: StepFailureKind::LimitExceeded,
                message: self.scrub(&err.to_string()),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            })?;
        let label = format!("Press key \"{key}\"");
        if key.eq_ignore_ascii_case("back") {
            self.require_capability(
                driver,
                DriverCapability::SystemBack,
                "pressKey Back requires SystemBack capability",
                step_index,
                "pressKey",
                label.clone(),
            )?;
        }
        self.dispatch_action(
            driver,
            &app_id,
            ActAction::PressKey {
                key,
                modifiers: step.modifiers.clone(),
                count: step.count,
            },
            state,
            cancel,
        )
        .await
        .map_err(|err| self.driver_err_to_failed_step(step_index, "pressKey", label, err))
    }

    async fn exec_hide_keyboard<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step_index: usize,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult {
        let app_id = self.resolve_app_id(None, state, step_index, "hideKeyboard")?;
        self.dispatch_action(driver, &app_id, ActAction::HideKeyboard, state, cancel)
            .await
            .map_err(|err| {
                self.driver_err_to_failed_step(
                    step_index,
                    "hideKeyboard",
                    "Hide keyboard".into(),
                    err,
                )
            })
    }

    async fn exec_back<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step_index: usize,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult {
        let label = "Back".to_string();
        self.require_capability(
            driver,
            DriverCapability::SystemBack,
            "system back is not supported on this platform",
            step_index,
            "back",
            label.clone(),
        )?;
        let app_id = self.resolve_app_id(None, state, step_index, "back")?;
        self.dispatch_action(driver, &app_id, ActAction::Back, state, cancel)
            .await
            .map_err(|err| self.driver_err_to_failed_step(step_index, "back", label, err))
    }

    async fn exec_scroll<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step_index: usize,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult {
        let app_id = self.resolve_app_id(None, state, step_index, "scroll")?;
        let viewport = self.current_viewport(driver, &app_id, state, cancel).await;
        let (start, end) = scroll_endpoints(ScrollDirection::Down, viewport);
        self.dispatch_action(
            driver,
            &app_id,
            ActAction::Swipe {
                start,
                end,
                duration_ms: 400,
                hold_duration_ms: None,
            },
            state,
            cancel,
        )
        .await
        .map_err(|err| self.driver_err_to_failed_step(step_index, "scroll", "Scroll".into(), err))
    }

    async fn exec_swipe<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &SwipeStep,
        step_index: usize,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult {
        let app_id = self.resolve_app_id(None, state, step_index, "swipe")?;
        let label = step.label.clone().unwrap_or_else(|| "Swipe".to_string());

        let bounds = if let Some(ref from_sel) = step.from {
            self.require_capability(
                driver,
                DriverCapability::AccessibilityTree,
                "swipe `from` selector requires AccessibilityTree capability",
                step_index,
                "swipe",
                label.clone(),
            )?;
            let interpolated =
                state
                    .env
                    .interpolate_selector(from_sel)
                    .map_err(|err| FailedStepContext {
                        step_index,
                        command: "swipe".to_string(),
                        label: label.clone(),
                        kind: StepFailureKind::LimitExceeded,
                        message: self.scrub(&err.to_string()),
                        failed_app_id: None,
                        visible_context: None,
                        crash_signal: None,
                    })?;
            let wait = self
                .wait_selector(
                    driver,
                    &app_id,
                    interpolated.clone(),
                    false,
                    self.config.default_step_timeout,
                    self.config.action_fail_fast_settle_polls,
                    state,
                    cancel,
                )
                .await
                .map_err(|err| {
                    self.driver_err_to_failed_step(step_index, "swipe", label.clone(), err)
                })?;
            let Some(node) = wait.matched_node else {
                return Err(Box::new(FailedStepContext {
                    step_index,
                    command: "swipe".to_string(),
                    label,
                    kind: StepFailureKind::SelectorNotFound,
                    message: self.scrub(&format!(
                        "swipe `from` element not found ({}): {}",
                        interpolated.describe(),
                        wait.visible_context.format_summary()
                    )),
                    failed_app_id: None,
                    visible_context: Some(wait.visible_context),
                    crash_signal: None,
                }));
            };
            node.frame
        } else {
            let vp = self.current_viewport(driver, &app_id, state, cancel).await;
            crate::driver::UiRect::new(0.0, 0.0, vp.width, vp.height)
        };

        let size = UiSize::new(bounds.width, bounds.height);
        let (start, end) = if let (Some(s), Some(e)) = (step.start, step.end) {
            let sp = s.resolve(size);
            let ep = e.resolve(size);
            (
                UiPoint::new(bounds.x + sp.x, bounds.y + sp.y),
                UiPoint::new(bounds.x + ep.x, bounds.y + ep.y),
            )
        } else if let Some(dir) = step.direction {
            let (sp, ep) = swipe_direction_endpoints(dir, size);
            (
                UiPoint::new(bounds.x + sp.x, bounds.y + sp.y),
                UiPoint::new(bounds.x + ep.x, bounds.y + ep.y),
            )
        } else {
            unreachable!("validated in parser");
        };

        self.dispatch_action(
            driver,
            &app_id,
            ActAction::Swipe {
                start,
                end,
                duration_ms: step.duration_ms,
                hold_duration_ms: None,
            },
            state,
            cancel,
        )
        .await
        .map_err(|err| self.driver_err_to_failed_step(step_index, "swipe", label, err))
    }

    async fn exec_scroll_until_visible<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &ScrollUntilVisibleStep,
        step_index: usize,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult<bool> {
        let app_id = self.resolve_app_id(None, state, step_index, "scrollUntilVisible")?;
        let selector = state
            .env
            .interpolate_selector(&step.element)
            .map_err(|err| FailedStepContext {
                step_index,
                command: "scrollUntilVisible".to_string(),
                label: step
                    .label
                    .clone()
                    .unwrap_or_else(|| "Scroll until visible".to_string()),
                kind: StepFailureKind::LimitExceeded,
                message: self.scrub(&err.to_string()),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            })?;
        let label = step
            .label
            .clone()
            .unwrap_or_else(|| format!("Scroll until visible {}", selector.describe()));

        self.require_capability(
            driver,
            DriverCapability::AccessibilityTree,
            "scrollUntilVisible requires AccessibilityTree capability",
            step_index,
            "scrollUntilVisible",
            label.clone(),
        )?;

        let compiled = CompiledSelector::compile(&selector).map_err(|err| FailedStepContext {
            step_index,
            command: "scrollUntilVisible".to_string(),
            label: label.clone(),
            kind: StepFailureKind::ParseOrConfinement,
            message: self.scrub(&err.to_string()),
            failed_app_id: None,
            visible_context: None,
            crash_signal: None,
        })?;

        let started = Instant::now();
        let timeout = Duration::from_millis(step.timeout_ms);
        let single_probe = timeout.is_zero();
        let deadline = started + timeout;
        let mut settle = SettleTracker::new(2);
        let swipe_duration_ms = scroll_speed_to_duration_ms(step.speed);
        let hold_duration_ms = driver
            .capabilities()
            .supports(DriverCapability::HoldScroll)
            .then_some(DEFAULT_HOLD_SCROLL_MS);
        let min_vis_fraction = f64::from(step.visibility_percentage) / 100.0;
        let allow_known_hash = !self.config.include_system_ui
            && !selector.uses_unhashed_fields()
            && selector.requires_enabled_everywhere();

        let mut end_of_scroll = false;
        let mut timed_out = false;

        loop {
            if cancel.is_cancelled() {
                return Err(self.driver_err_to_failed_step(
                    step_index,
                    "scrollUntilVisible",
                    label,
                    DriverError::Cancelled,
                ));
            }

            let remaining_for_fetch = if single_probe {
                None
            } else {
                let rem = deadline.saturating_duration_since(Instant::now());
                if rem.is_zero() {
                    timed_out = true;
                    break;
                }
                Some(rem)
            };

            let hierarchy = match remaining_for_fetch {
                None => {
                    self.fetch_fresh_hierarchy_with_cache(
                        driver,
                        &app_id,
                        allow_known_hash,
                        state,
                        cancel,
                    )
                    .await
                }
                Some(rem) => match tokio::time::timeout(
                    rem,
                    self.fetch_fresh_hierarchy_with_cache(
                        driver,
                        &app_id,
                        allow_known_hash,
                        state,
                        cancel,
                    ),
                )
                .await
                {
                    Ok(res) => res,
                    Err(_) => {
                        timed_out = true;
                        break;
                    }
                },
            }
            .map_err(|err| {
                self.driver_err_to_failed_step(step_index, "scrollUntilVisible", label.clone(), err)
            })?;

            // Reject any late observation completed after the deadline before accepting or nudging.
            if !single_probe && Instant::now() > deadline {
                timed_out = true;
                break;
            }

            let viewport = hierarchy.effective_viewport();
            if let Some(node) = compiled.match_one(&hierarchy.nodes) {
                let vis_fraction = node.visible_fraction(viewport);
                if node.is_on_screen && vis_fraction + 1e-6 >= min_vis_fraction {
                    if step.center_element
                        && let Some((nudge_start, nudge_end)) =
                            compute_center_nudge(node.frame.center(), viewport, step.direction)
                    {
                        let nudge_action = ActAction::Swipe {
                            start: nudge_start,
                            end: nudge_end,
                            duration_ms: swipe_duration_ms.min(400),
                            hold_duration_ms,
                        };
                        let nudge_res = if single_probe {
                            self.dispatch_action(driver, &app_id, nudge_action, state, cancel)
                                .await
                        } else {
                            let rem = deadline.saturating_duration_since(Instant::now());
                            if rem.is_zero() {
                                timed_out = true;
                                break;
                            }
                            match tokio::time::timeout(
                                rem,
                                self.dispatch_action(driver, &app_id, nudge_action, state, cancel),
                            )
                            .await
                            {
                                Ok(res) => res,
                                Err(_) => {
                                    timed_out = true;
                                    break;
                                }
                            }
                        };
                        nudge_res.map_err(|err| {
                            self.driver_err_to_failed_step(
                                step_index,
                                "scrollUntilVisible",
                                label.clone(),
                                err,
                            )
                        })?;
                        if !single_probe && Instant::now() > deadline {
                            timed_out = true;
                            break;
                        }
                    }
                    return Ok(!single_probe && started.elapsed() < timeout);
                }
            }

            // End-of-scroll bailout over non-truncated captures.
            if hierarchy.truncated {
                settle.reset();
            } else if settle
                .observe_hash(hierarchy.hierarchy_hash_u64())
                .is_settled()
            {
                end_of_scroll = true;
                break;
            }

            if single_probe {
                break;
            }
            let remaining_for_swipe = deadline.saturating_duration_since(Instant::now());
            if remaining_for_swipe.is_zero() {
                timed_out = true;
                break;
            }

            let (start, end) = scroll_endpoints(step.direction, viewport);
            let swipe_action = ActAction::Swipe {
                start,
                end,
                duration_ms: swipe_duration_ms,
                hold_duration_ms,
            };
            match tokio::time::timeout(
                remaining_for_swipe,
                self.dispatch_action(driver, &app_id, swipe_action, state, cancel),
            )
            .await
            {
                Ok(res) => res.map_err(|err| {
                    self.driver_err_to_failed_step(
                        step_index,
                        "scrollUntilVisible",
                        label.clone(),
                        err,
                    )
                })?,
                Err(_) => {
                    timed_out = true;
                    break;
                }
            }
            if Instant::now() > deadline {
                timed_out = true;
                break;
            }
        }

        let visible_context = state
            .last_hierarchy
            .as_ref()
            .map(|h| {
                build_visible_context(
                    &h.nodes,
                    Some(&self.config.redactor),
                    Some(&self.config.provider_redactor),
                    VisibleContext::DEFAULT_LIMIT,
                )
            })
            .unwrap_or_default();

        let reason = if end_of_scroll {
            format!(
                "reached end of scroll without finding {}: {}",
                selector.describe(),
                visible_context.format_summary()
            )
        } else {
            format!(
                "timed out scrolling for {}: {}",
                selector.describe(),
                visible_context.format_summary()
            )
        };

        Err(Box::new(FailedStepContext {
            step_index,
            command: "scrollUntilVisible".to_string(),
            label,
            kind: if timed_out {
                StepFailureKind::Timeout
            } else {
                StepFailureKind::SelectorNotFound
            },
            message: self.scrub(&reason),
            failed_app_id: Some(app_id),
            visible_context: Some(visible_context),
            crash_signal: None,
        }))
    }

    async fn exec_wait_animation<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &WaitForAnimationToEndStep,
        step_index: usize,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult<bool> {
        let app_id = self.resolve_app_id(None, state, step_index, "waitForAnimationToEnd")?;
        let label = "Wait for animation to end".to_string();
        self.require_capability(
            driver,
            DriverCapability::AccessibilityTree,
            "waitForAnimationToEnd requires AccessibilityTree capability",
            step_index,
            "waitForAnimationToEnd",
            label.clone(),
        )?;
        let timeout = step
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.config.default_animation_timeout);
        self.wait_for_hierarchy_settle(driver, &app_id, timeout, state, cancel)
            .await
            .map_err(|err| {
                self.driver_err_to_failed_step(step_index, "waitForAnimationToEnd", label, err)
            })
    }

    async fn wait_for_hierarchy_settle<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        app_id: &str,
        timeout: Duration,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> Result<bool, DriverError> {
        let started = Instant::now();
        let single_probe = timeout.is_zero();
        let deadline = started + timeout;
        let mut schedule = PollSchedule::default();
        let mut settle = SettleTracker::new(2);

        loop {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let h = if single_probe {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    res = self.fetch_fresh_hierarchy(driver, app_id, state, cancel) => res?,
                }
            } else {
                let rem = deadline.saturating_duration_since(Instant::now());
                if rem.is_zero() {
                    return Ok(false);
                }
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    timed = tokio::time::timeout(
                        rem,
                        self.fetch_fresh_hierarchy(driver, app_id, state, cancel),
                    ) => match timed {
                        Ok(res) => res?,
                        Err(_) => return Ok(false),
                    },
                }
            };
            if !single_probe && Instant::now() > deadline {
                return Ok(false);
            }
            if h.truncated {
                settle.reset();
            } else if settle.observe_hash(h.hierarchy_hash_u64()).is_settled() {
                return Ok(!single_probe && started.elapsed() < timeout);
            }
            if single_probe {
                return Ok(false);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(delay) = schedule.next_delay(remaining) else {
                return Ok(false);
            };
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    async fn exec_take_screenshot<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &TakeScreenshotStep,
        step_index: usize,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult {
        let app_id = self.resolve_app_id(None, state, step_index, "takeScreenshot")?;
        let raw_path = state
            .env
            .interpolate(&step.path)
            .map_err(|err| FailedStepContext {
                step_index,
                command: "takeScreenshot".to_string(),
                label: "Take screenshot".to_string(),
                kind: StepFailureKind::LimitExceeded,
                message: self.scrub(&err.to_string()),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            })?;
        let label = format!("Take screenshot \"{}\"", self.scrub(&raw_path));

        let with_ext = if Path::new(&raw_path).extension().is_none() {
            format!("{raw_path}.png")
        } else {
            raw_path
        };

        let target_path = if self.config.screenshot_confine_to_dir {
            let Some(ref strict_dir) = self.config.screenshot_dir else {
                return Err(Box::new(FailedStepContext {
                    step_index,
                    command: "takeScreenshot".to_string(),
                    label: label.clone(),
                    kind: StepFailureKind::ParseOrConfinement,
                    message: "screenshot output directory is not configured".to_string(),
                    failed_app_id: None,
                    visible_context: None,
                    crash_signal: None,
                }));
            };
            confine_write_path_strict_dir(
                &state.canonical_workspace,
                strict_dir,
                &with_ext,
                "screenshot",
            )
        } else {
            let base_dir = self
                .config
                .screenshot_dir
                .as_deref()
                .unwrap_or(&state.canonical_workspace);
            confine_write_path(
                &state.canonical_workspace,
                base_dir,
                &with_ext,
                "screenshot",
            )
        }
        .map_err(|err| FailedStepContext {
            step_index,
            command: "takeScreenshot".to_string(),
            label: label.clone(),
            kind: StepFailureKind::ParseOrConfinement,
            message: self.scrub(&err.to_string()),
            failed_app_id: None,
            visible_context: None,
            crash_signal: None,
        })?;

        if self.config.screenshot_no_clobber && fs::symlink_metadata(&target_path).is_ok() {
            return Err(Box::new(FailedStepContext {
                step_index,
                command: "takeScreenshot".to_string(),
                label: label.clone(),
                kind: StepFailureKind::ParseOrConfinement,
                message: self.scrub(&format!(
                    "refusing to overwrite existing screenshot file: {}",
                    target_path.display()
                )),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            }));
        }

        let png_bytes = driver
            .take_screenshot(&app_id, cancel)
            .await
            .map_err(|err| {
                self.driver_err_to_failed_step(step_index, "takeScreenshot", label.clone(), err)
            })?;

        if self.config.screenshot_confine_to_dir {
            let strict_dir = self
                .config
                .screenshot_dir
                .as_ref()
                .expect("validated strict screenshot dir");
            ensure_strict_screenshot_dir(
                &state.canonical_workspace,
                strict_dir,
                &mut state.created_strict_screenshot_dir,
            )
            .map_err(|err| FailedStepContext {
                step_index,
                command: "takeScreenshot".to_string(),
                label: label.clone(),
                kind: StepFailureKind::ParseOrConfinement,
                message: self.scrub(&err.to_string()),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            })?;

            // Re-verify `strict_dir` directly before atomic file creation; `target_path` is a
            // single flat filename directly inside `strict_dir` (zero intermediate subdirectories).
            let strict_meta =
                fs::symlink_metadata(strict_dir).map_err(|err| FailedStepContext {
                    step_index,
                    command: "takeScreenshot".to_string(),
                    label: label.clone(),
                    kind: StepFailureKind::ParseOrConfinement,
                    message: self.scrub(&format!("cannot inspect screenshot directory: {err}")),
                    failed_app_id: None,
                    visible_context: None,
                    crash_signal: None,
                })?;
            if strict_meta.file_type().is_symlink() || !strict_meta.file_type().is_dir() {
                return Err(Box::new(FailedStepContext {
                    step_index,
                    command: "takeScreenshot".to_string(),
                    label: label.clone(),
                    kind: StepFailureKind::ParseOrConfinement,
                    message: "screenshot output directory is not a safe regular directory"
                        .to_string(),
                    failed_app_id: None,
                    visible_context: None,
                    crash_signal: None,
                }));
            }
            let canon_strict = fs::canonicalize(strict_dir).map_err(|err| FailedStepContext {
                step_index,
                command: "takeScreenshot".to_string(),
                label: label.clone(),
                kind: StepFailureKind::ParseOrConfinement,
                message: self.scrub(&format!("cannot resolve screenshot directory: {err}")),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            })?;
            if canon_strict == state.canonical_workspace
                || !canon_strict.starts_with(&state.canonical_workspace)
            {
                return Err(Box::new(FailedStepContext {
                    step_index,
                    command: "takeScreenshot".to_string(),
                    label: label.clone(),
                    kind: StepFailureKind::ParseOrConfinement,
                    message: "screenshot output directory escaped confinement via symlink"
                        .to_string(),
                    failed_app_id: None,
                    visible_context: None,
                    crash_signal: None,
                }));
            }
            let flat_name = target_path.file_name().ok_or_else(|| FailedStepContext {
                step_index,
                command: "takeScreenshot".to_string(),
                label: label.clone(),
                kind: StepFailureKind::ParseOrConfinement,
                message: "screenshot target has no filename".to_string(),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            })?;
            let final_target = canon_strict.join(flat_name);
            let write_res = (|| -> std::io::Result<()> {
                let mut f = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&final_target)?;
                if !f.metadata()?.is_file() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "screenshot target is not a regular file",
                    ));
                }
                std::io::Write::write_all(&mut f, &png_bytes)?;
                f.sync_all()?;
                Ok(())
            })();
            write_res.map_err(|err| {
                let kind = if err.kind() == std::io::ErrorKind::AlreadyExists {
                    StepFailureKind::ParseOrConfinement
                } else {
                    StepFailureKind::Transport
                };
                FailedStepContext {
                    step_index,
                    command: "takeScreenshot".to_string(),
                    label,
                    kind,
                    message: self.scrub(&format!(
                        "refusing to write screenshot {}: {err}",
                        final_target.display()
                    )),
                    failed_app_id: None,
                    visible_context: None,
                    crash_signal: None,
                }
            })?;
            state.screenshots.push(final_target);
            return Ok(());
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).map_err(|err| FailedStepContext {
                step_index,
                command: "takeScreenshot".to_string(),
                label: label.clone(),
                kind: StepFailureKind::Transport,
                message: self.scrub(&err.to_string()),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            })?;
        }

        if self.config.screenshot_no_clobber {
            let write_res = (|| -> std::io::Result<()> {
                let mut f = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target_path)?;
                std::io::Write::write_all(&mut f, &png_bytes)?;
                f.sync_all()?;
                Ok(())
            })();
            write_res.map_err(|err| {
                let kind = if err.kind() == std::io::ErrorKind::AlreadyExists {
                    StepFailureKind::ParseOrConfinement
                } else {
                    StepFailureKind::Transport
                };
                FailedStepContext {
                    step_index,
                    command: "takeScreenshot".to_string(),
                    label,
                    kind,
                    message: self.scrub(&format!(
                        "refusing to write screenshot {}: {err}",
                        target_path.display()
                    )),
                    failed_app_id: None,
                    visible_context: None,
                    crash_signal: None,
                }
            })?;
        } else {
            fs::write(&target_path, png_bytes).map_err(|err| FailedStepContext {
                step_index,
                command: "takeScreenshot".to_string(),
                label,
                kind: StepFailureKind::Transport,
                message: self.scrub(&err.to_string()),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            })?;
        }

        state.screenshots.push(target_path);
        Ok(())
    }

    async fn exec_assert_visible<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &AssertVisibleStep,
        step_index: usize,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult<bool> {
        let app_id = self.resolve_app_id(None, state, step_index, "assertVisible")?;
        let selector = state
            .env
            .interpolate_selector(&step.selector)
            .map_err(|err| FailedStepContext {
                step_index,
                command: "assertVisible".to_string(),
                label: step
                    .label
                    .clone()
                    .unwrap_or_else(|| "Assert visible".to_string()),
                kind: StepFailureKind::LimitExceeded,
                message: self.scrub(&err.to_string()),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            })?;
        let label = self.scrub(
            &step
                .label
                .clone()
                .unwrap_or_else(|| format!("Assert that {} is visible", selector.describe())),
        );

        self.require_capability(
            driver,
            DriverCapability::AccessibilityTree,
            "assertVisible requires AccessibilityTree capability",
            step_index,
            "assertVisible",
            label.clone(),
        )?;

        let timeout = step
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.config.default_step_timeout);
        let wait = self
            .wait_selector(
                driver,
                &app_id,
                selector.clone(),
                false,
                timeout,
                None,
                state,
                cancel,
            )
            .await
            .map_err(|err| {
                self.driver_err_to_failed_step(step_index, "assertVisible", label.clone(), err)
            })?;

        if wait.matched {
            return Ok(wait.settled_early);
        }
        if selector.optional {
            debug!(selector = %self.scrub(&selector.describe()), "skipping optional assertVisible miss");
            return Ok(false);
        }

        Err(Box::new(FailedStepContext {
            step_index,
            command: "assertVisible".to_string(),
            label,
            kind: StepFailureKind::AssertionFailed,
            message: self.scrub(&format!(
                "assertion failed: expected {} to be visible ({})",
                selector.describe(),
                wait.visible_context.format_summary()
            )),
            failed_app_id: None,
            visible_context: Some(wait.visible_context),
            crash_signal: None,
        }))
    }

    async fn exec_assert_not_visible<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &AssertNotVisibleStep,
        step_index: usize,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult<bool> {
        let app_id = self.resolve_app_id(None, state, step_index, "assertNotVisible")?;
        let selector = state
            .env
            .interpolate_selector(&step.selector)
            .map_err(|err| FailedStepContext {
                step_index,
                command: "assertNotVisible".to_string(),
                label: step
                    .label
                    .clone()
                    .unwrap_or_else(|| "Assert not visible".to_string()),
                kind: StepFailureKind::LimitExceeded,
                message: self.scrub(&err.to_string()),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            })?;
        let label = self.scrub(
            &step
                .label
                .clone()
                .unwrap_or_else(|| format!("Assert that {} is not visible", selector.describe())),
        );

        self.require_capability(
            driver,
            DriverCapability::AccessibilityTree,
            "assertNotVisible requires AccessibilityTree capability",
            step_index,
            "assertNotVisible",
            label.clone(),
        )?;

        let timeout = step
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.config.default_step_timeout);
        let wait = self
            .wait_selector(
                driver,
                &app_id,
                selector.clone(),
                true,
                timeout,
                None,
                state,
                cancel,
            )
            .await
            .map_err(|err| {
                self.driver_err_to_failed_step(step_index, "assertNotVisible", label.clone(), err)
            })?;

        if wait.matched {
            return Ok(wait.settled_early);
        }
        if selector.optional {
            debug!(selector = %self.scrub(&selector.describe()), "skipping optional assertNotVisible miss");
            return Ok(false);
        }

        Err(Box::new(FailedStepContext {
            step_index,
            command: "assertNotVisible".to_string(),
            label,
            kind: StepFailureKind::AssertionFailed,
            message: self.scrub(&format!(
                "assertion failed: expected {} to not be visible ({})",
                selector.describe(),
                wait.visible_context.format_summary()
            )),
            failed_app_id: None,
            visible_context: Some(wait.visible_context),
            crash_signal: None,
        }))
    }

    async fn exec_extended_wait<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &ExtendedWaitUntilStep,
        step_index: usize,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult<bool> {
        let app_id = self.resolve_app_id(None, state, step_index, "extendedWaitUntil")?;
        let (raw_sel, not_visible) = match (&step.visible, &step.not_visible) {
            (Some(s), None) => (s, false),
            (None, Some(s)) => (s, true),
            _ => unreachable!("validated in parser"),
        };
        let selector =
            state
                .env
                .interpolate_selector(raw_sel)
                .map_err(|err| FailedStepContext {
                    step_index,
                    command: "extendedWaitUntil".to_string(),
                    label: step
                        .label
                        .clone()
                        .unwrap_or_else(|| "Extended wait until".to_string()),
                    kind: StepFailureKind::LimitExceeded,
                    message: self.scrub(&err.to_string()),
                    failed_app_id: None,
                    visible_context: None,
                    crash_signal: None,
                })?;
        let label = self.scrub(&step.label.clone().unwrap_or_else(|| {
            if not_visible {
                format!("Extended wait until {} is not visible", selector.describe())
            } else {
                format!("Extended wait until {} is visible", selector.describe())
            }
        }));

        self.require_capability(
            driver,
            DriverCapability::AccessibilityTree,
            "extendedWaitUntil requires AccessibilityTree capability",
            step_index,
            "extendedWaitUntil",
            label.clone(),
        )?;

        let wait = self
            .wait_selector(
                driver,
                &app_id,
                selector.clone(),
                not_visible,
                Duration::from_millis(step.timeout_ms),
                None,
                state,
                cancel,
            )
            .await
            .map_err(|err| {
                self.driver_err_to_failed_step(step_index, "extendedWaitUntil", label.clone(), err)
            })?;

        if wait.matched {
            return Ok(wait.settled_early);
        }

        Err(Box::new(FailedStepContext {
            step_index,
            command: "extendedWaitUntil".to_string(),
            label,
            kind: StepFailureKind::Timeout,
            message: self.scrub(&format!(
                "extendedWaitUntil timed out after {}ms waiting for {}: {}",
                step.timeout_ms,
                selector.describe(),
                wait.visible_context.format_summary()
            )),
            failed_app_id: None,
            visible_context: Some(wait.visible_context),
            crash_signal: None,
        }))
    }

    async fn evaluate_condition<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        cond: &Condition,
        step_index: usize,
        command: &str,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult<bool> {
        if let Some(ref platform) = cond.platform
            && !driver.platform().eq_ignore_ascii_case(platform)
        {
            return Ok(false);
        }
        if cond.visible.is_none() && cond.not_visible.is_none() {
            return Ok(true);
        }

        let app_id = self.resolve_app_id(None, state, step_index, command)?;
        self.require_capability(
            driver,
            DriverCapability::AccessibilityTree,
            "visible/notVisible condition requires AccessibilityTree capability",
            step_index,
            command,
            format!("{command} condition"),
        )?;

        let compiled_vis = if let Some(ref vis_sel) = cond.visible {
            let interpolated =
                state
                    .env
                    .interpolate_selector(vis_sel)
                    .map_err(|err| FailedStepContext {
                        step_index,
                        command: command.to_string(),
                        label: format!("{command} condition"),
                        kind: StepFailureKind::LimitExceeded,
                        message: self.scrub(&err.to_string()),
                        failed_app_id: None,
                        visible_context: None,
                        crash_signal: None,
                    })?;
            let allows_known_hash =
                !interpolated.uses_unhashed_fields() && interpolated.requires_enabled_everywhere();
            let compiled =
                CompiledSelector::compile(&interpolated).map_err(|err| FailedStepContext {
                    step_index,
                    command: command.to_string(),
                    label: format!("{command} condition"),
                    kind: StepFailureKind::ParseOrConfinement,
                    message: self.scrub(&err.to_string()),
                    failed_app_id: None,
                    visible_context: None,
                    crash_signal: None,
                })?;
            Some((compiled, allows_known_hash))
        } else {
            None
        };

        let compiled_not_vis =
            if let Some(ref not_vis_sel) = cond.not_visible {
                let interpolated = state.env.interpolate_selector(not_vis_sel).map_err(|err| {
                    FailedStepContext {
                        step_index,
                        command: command.to_string(),
                        label: format!("{command} condition"),
                        kind: StepFailureKind::LimitExceeded,
                        message: self.scrub(&err.to_string()),
                        failed_app_id: None,
                        visible_context: None,
                        crash_signal: None,
                    }
                })?;
                let allows_known_hash = !interpolated.uses_unhashed_fields()
                    && interpolated.requires_enabled_everywhere();
                let compiled =
                    CompiledSelector::compile(&interpolated).map_err(|err| FailedStepContext {
                        step_index,
                        command: command.to_string(),
                        label: format!("{command} condition"),
                        kind: StepFailureKind::ParseOrConfinement,
                        message: self.scrub(&err.to_string()),
                        failed_app_id: None,
                        visible_context: None,
                        crash_signal: None,
                    })?;
                Some((compiled, allows_known_hash))
            } else {
                None
            };

        let allow_known_hash = !self.config.include_system_ui
            && compiled_vis.as_ref().is_none_or(|(_, ok)| *ok)
            && compiled_not_vis.as_ref().is_none_or(|(_, ok)| *ok);

        let hierarchy = self
            .fetch_fresh_hierarchy_with_cache(driver, &app_id, allow_known_hash, state, cancel)
            .await
            .map_err(|err| {
                self.driver_err_to_failed_step(
                    step_index,
                    command,
                    format!("{command} condition"),
                    err,
                )
            })?;

        if let Some((ref compiled, _)) = compiled_vis
            && compiled.match_one(&hierarchy.nodes).is_none()
        {
            return Ok(false);
        }

        if let Some((ref compiled, _)) = compiled_not_vis {
            if hierarchy.truncated {
                // Never falsely satisfy `notVisible` on a truncated hierarchy capture.
                return Ok(false);
            }
            if compiled.match_one(&hierarchy.nodes).is_some() {
                return Ok(false);
            }
        }

        Ok(true)
    }

    async fn exec_run_flow<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &RunFlowStep,
        flow_dir: &Path,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult {
        let step_index = state.step_counter;
        if let Some(ref cond) = step.when {
            let should_run = self
                .evaluate_condition(driver, cond, step_index, "runFlow", state, cancel)
                .await?;
            if !should_run {
                debug!("skipping runFlow because `when` condition was false");
                return Ok(());
            }
        }

        let res = self
            .exec_subflow_target(
                driver,
                &step.target,
                &step.env,
                flow_dir,
                "runFlow",
                state,
                cancel,
            )
            .await;

        if let Err(err) = res {
            if step.optional
                && !matches!(
                    err.kind,
                    StepFailureKind::Cancelled
                        | StepFailureKind::ParseOrConfinement
                        | StepFailureKind::UnsupportedCapability
                        | StepFailureKind::LimitExceeded
                        | StepFailureKind::AppCrash
                )
            {
                debug!(error = %self.scrub(&err.message), "ignoring failure of optional runFlow");
                return Ok(());
            }
            return Err(err);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn exec_subflow_target<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        target: &RunFlowTarget,
        child_env_raw: &BTreeMap<String, String>,
        flow_dir: &Path,
        command: &str,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult {
        let step_index = state.step_counter;

        // Evaluate child env values in the PARENT scope before pushing child frame.
        let mut child_env = BTreeMap::new();
        for (k, v) in child_env_raw {
            let interp = state.env.interpolate(v).map_err(|err| FailedStepContext {
                step_index,
                command: command.to_string(),
                label: format!("{command} env `{k}`"),
                kind: StepFailureKind::LimitExceeded,
                message: self.scrub(&err.to_string()),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            })?;
            child_env.insert(k.clone(), interp);
        }

        let pushed = !child_env.is_empty();
        if pushed {
            state.env.push(child_env);
        }

        let result = match target {
            RunFlowTarget::Commands(commands) => {
                self.execute_steps(driver, commands, flow_dir, state, cancel)
                    .await
            }
            RunFlowTarget::File(raw_rel) => {
                if raw_rel.contains('$') || raw_rel.contains('{') {
                    Err(Box::new(FailedStepContext {
                        step_index,
                        command: command.to_string(),
                        label: format!("{command} \"{raw_rel}\""),
                        kind: StepFailureKind::ParseOrConfinement,
                        message: format!(
                            "subflow path must be static (no variable interpolation): {raw_rel}"
                        ),
                        failed_app_id: None,
                        visible_context: None,
                        crash_signal: None,
                    }))
                } else if let Some(ref snapshot) = self.config.closure_snapshot {
                    match validate_relative_path_syntax(raw_rel, "subflow").and_then(|()| {
                        let lexical = lexical_normalize(&flow_dir.join(raw_rel));
                        if lexical == state.canonical_workspace
                            || !lexical.starts_with(&state.canonical_workspace)
                        {
                            return Err(FlowExecutionError::PathConfinement(format!(
                                "subflow escapes workspace: {raw_rel}"
                            )));
                        }
                        Ok(lexical)
                    }) {
                        Err(err) => Err(Box::new(FailedStepContext {
                            step_index,
                            command: command.to_string(),
                            label: format!("{command} \"{raw_rel}\""),
                            kind: StepFailureKind::ParseOrConfinement,
                            message: self.scrub(&err.to_string()),
                            failed_app_id: None,
                            visible_context: None,
                            crash_signal: None,
                        })),
                        Ok(lexical_subflow) => {
                            if state.call_stack.contains(&lexical_subflow) {
                                Err(Box::new(FailedStepContext {
                                    step_index,
                                    command: command.to_string(),
                                    label: format!("{command} \"{raw_rel}\""),
                                    kind: StepFailureKind::ParseOrConfinement,
                                    message: format!("subflow cycle detected: {raw_rel}"),
                                    failed_app_id: None,
                                    visible_context: None,
                                    crash_signal: None,
                                }))
                            } else if state.call_stack.len() >= MAX_FLOW_DEPTH {
                                Err(Box::new(FailedStepContext {
                                    step_index,
                                    command: command.to_string(),
                                    label: format!("{command} \"{raw_rel}\""),
                                    kind: StepFailureKind::ParseOrConfinement,
                                    message: format!(
                                        "subflow nesting exceeds maximum depth of {MAX_FLOW_DEPTH}"
                                    ),
                                    failed_app_id: None,
                                    visible_context: None,
                                    crash_signal: None,
                                }))
                            } else {
                                let rel_key = lexical_subflow
                                    .strip_prefix(&state.canonical_workspace)
                                    .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                                    .unwrap_or_default();
                                match snapshot.get(&rel_key).ok_or_else(|| {
                                    FlowExecutionError::PathConfinement(format!(
                                        "subflow missing from preflight closure snapshot: {rel_key}"
                                    ))
                                }).and_then(|bytes| {
                                    let text = String::from_utf8_lossy(bytes);
                                    parse_flow(&text).map_err(FlowExecutionError::from)
                                }) {
                                    Err(err) => Err(Box::new(FailedStepContext {
                                        step_index,
                                        command: command.to_string(),
                                        label: format!("{command} \"{raw_rel}\""),
                                        kind: StepFailureKind::ParseOrConfinement,
                                        message: self.scrub(&err.to_string()),
                                        failed_app_id: None,
                                        visible_context: None,
                                        crash_signal: None,
                                    })),
                                    Ok(sub_parsed) => {
                                        let sub_dir = lexical_subflow
                                            .parent()
                                            .unwrap_or(&state.canonical_workspace)
                                            .to_path_buf();
                                        state.call_stack.push(lexical_subflow);
                                        let sub_res = self
                                            .execute_parsed_flow(
                                                driver,
                                                &sub_parsed,
                                                &sub_dir,
                                                true,
                                                state,
                                                cancel,
                                            )
                                            .await;
                                        state.call_stack.pop();
                                        sub_res
                                    }
                                }
                            }
                        }
                    }
                } else {
                    match confine_relative_read_path(
                        &state.canonical_workspace,
                        flow_dir,
                        raw_rel,
                        MAX_FILE_BYTES,
                        "subflow",
                    ) {
                        Err(err) => Err(Box::new(FailedStepContext {
                            step_index,
                            command: command.to_string(),
                            label: format!("{command} \"{raw_rel}\""),
                            kind: StepFailureKind::ParseOrConfinement,
                            message: self.scrub(&err.to_string()),
                            failed_app_id: None,
                            visible_context: None,
                            crash_signal: None,
                        })),
                        Ok(confined_subflow) => {
                            if state.call_stack.contains(&confined_subflow.canonical) {
                                Err(Box::new(FailedStepContext {
                                    step_index,
                                    command: command.to_string(),
                                    label: format!("{command} \"{raw_rel}\""),
                                    kind: StepFailureKind::ParseOrConfinement,
                                    message: format!("subflow cycle detected: {raw_rel}"),
                                    failed_app_id: None,
                                    visible_context: None,
                                    crash_signal: None,
                                }))
                            } else if state.call_stack.len() >= MAX_FLOW_DEPTH {
                                Err(Box::new(FailedStepContext {
                                    step_index,
                                    command: command.to_string(),
                                    label: format!("{command} \"{raw_rel}\""),
                                    kind: StepFailureKind::ParseOrConfinement,
                                    message: format!(
                                        "subflow nesting exceeds maximum depth of {MAX_FLOW_DEPTH}"
                                    ),
                                    failed_app_id: None,
                                    visible_context: None,
                                    crash_signal: None,
                                }))
                            } else {
                                match read_bounded_file(&confined_subflow.canonical, MAX_FILE_BYTES)
                                    .and_then(|bytes| {
                                        let text = String::from_utf8_lossy(&bytes);
                                        parse_flow(&text).map_err(FlowExecutionError::from)
                                    }) {
                                    Err(err) => Err(Box::new(FailedStepContext {
                                        step_index,
                                        command: command.to_string(),
                                        label: format!("{command} \"{raw_rel}\""),
                                        kind: StepFailureKind::ParseOrConfinement,
                                        message: self.scrub(&err.to_string()),
                                        failed_app_id: None,
                                        visible_context: None,
                                        crash_signal: None,
                                    })),
                                    Ok(sub_parsed) => {
                                        // Keep the lexical parent directory (matching
                                        // `deja_core::collect_flow_closure`) so nested relative
                                        // `runFlow`/`retry`/`addMedia` resolve from a symlink's own
                                        // directory rather than its canonical target directory.
                                        let sub_dir = confined_subflow
                                            .lexical
                                            .parent()
                                            .unwrap_or(&state.canonical_workspace)
                                            .to_path_buf();
                                        state.call_stack.push(confined_subflow.canonical);
                                        let sub_res = self
                                            .execute_parsed_flow(
                                                driver,
                                                &sub_parsed,
                                                &sub_dir,
                                                true,
                                                state,
                                                cancel,
                                            )
                                            .await;
                                        state.call_stack.pop();
                                        sub_res
                                    }
                                }
                            }
                        }
                    }
                }
            }
        };

        // Always pop child env so it never leaks to the parent scope.
        if pushed {
            state.env.pop();
        }
        result
    }

    async fn exec_repeat<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &RepeatStep,
        flow_dir: &Path,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult {
        let step_index = state.step_counter;
        if let Some(times) = step.times
            && (times as usize) > MAX_REPEAT_ITERATIONS
        {
            return Err(Box::new(FailedStepContext {
                step_index,
                command: "repeat".to_string(),
                label: step.label.clone().unwrap_or_else(|| "Repeat".to_string()),
                kind: StepFailureKind::LimitExceeded,
                message: format!(
                    "`repeat.times` ({times}) exceeds maximum of {MAX_REPEAT_ITERATIONS}"
                ),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            }));
        }

        let max_iters = step
            .times
            .map(|t| t as usize)
            .unwrap_or(MAX_REPEAT_ITERATIONS);
        let mut iteration: usize = 0;

        while iteration < max_iters {
            if cancel.is_cancelled() {
                return Err(Box::new(FailedStepContext {
                    step_index: state.step_counter,
                    command: "repeat".to_string(),
                    label: step.label.clone().unwrap_or_else(|| "Repeat".to_string()),
                    kind: StepFailureKind::Cancelled,
                    message: "repeat loop cancelled".to_string(),
                    failed_app_id: None,
                    visible_context: None,
                    crash_signal: None,
                }));
            }
            if let Some(ref cond) = step.while_cond {
                let keep_going = self
                    .evaluate_condition(driver, cond, state.step_counter, "repeat", state, cancel)
                    .await?;
                if !keep_going {
                    return Ok(());
                }
            }
            self.execute_steps(driver, &step.commands, flow_dir, state, cancel)
                .await?;
            iteration += 1;
        }

        // If `while` was unbounded by `times` and still holds after MAX_REPEAT_ITERATIONS, fail closed.
        if step.times.is_none()
            && let Some(ref cond) = step.while_cond
            && self
                .evaluate_condition(driver, cond, state.step_counter, "repeat", state, cancel)
                .await?
        {
            return Err(Box::new(FailedStepContext {
                step_index: state.step_counter,
                command: "repeat".to_string(),
                label: step.label.clone().unwrap_or_else(|| "Repeat".to_string()),
                kind: StepFailureKind::LimitExceeded,
                message: format!(
                    "`repeat.while` condition still held after {MAX_REPEAT_ITERATIONS} iterations"
                ),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            }));
        }

        Ok(())
    }

    async fn exec_retry<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        step: &RetryStep,
        flow_dir: &Path,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> StepResult {
        let step_index = state.step_counter;
        if (step.max_retries as usize) > MAX_RETRIES {
            return Err(Box::new(FailedStepContext {
                step_index,
                command: "retry".to_string(),
                label: step.label.clone().unwrap_or_else(|| "Retry".to_string()),
                kind: StepFailureKind::LimitExceeded,
                message: format!(
                    "`retry.maxRetries` ({}) exceeds maximum of {MAX_RETRIES}",
                    step.max_retries
                ),
                failed_app_id: None,
                visible_context: None,
                crash_signal: None,
            }));
        }

        let total_attempts = step.max_retries.saturating_add(1);
        let mut last_err: Option<Box<FailedStepContext>> = None;

        for attempt in 0..total_attempts {
            if cancel.is_cancelled() {
                return Err(Box::new(FailedStepContext {
                    step_index: state.step_counter,
                    command: "retry".to_string(),
                    label: step.label.clone().unwrap_or_else(|| "Retry".to_string()),
                    kind: StepFailureKind::Cancelled,
                    message: "retry block cancelled".to_string(),
                    failed_app_id: None,
                    visible_context: None,
                    crash_signal: None,
                }));
            }
            match self
                .exec_subflow_target(
                    driver,
                    &step.target,
                    &step.env,
                    flow_dir,
                    "retry",
                    state,
                    cancel,
                )
                .await
            {
                Ok(()) => return Ok(()),
                Err(err) => {
                    // Never retry fatal runner/parse/capability/crash/cancel errors.
                    if matches!(
                        err.kind,
                        StepFailureKind::Cancelled
                            | StepFailureKind::UnsupportedCapability
                            | StepFailureKind::ParseOrConfinement
                            | StepFailureKind::LimitExceeded
                            | StepFailureKind::AppCrash
                    ) {
                        return Err(err);
                    }
                    debug!(
                        attempt = attempt + 1,
                        total_attempts,
                        error = %self.scrub(&err.message),
                        "retry attempt failed"
                    );
                    last_err = Some(err);
                }
            }
        }

        Err(last_err.expect("total_attempts >= 1 always records last_err on failure"))
    }

    async fn current_viewport<D: PlatformDriver + ?Sized>(
        &self,
        driver: &D,
        app_id: &str,
        state: &mut RuntimeState,
        cancel: &CancellationToken,
    ) -> UiSize {
        if let Some(ref h) = state.last_hierarchy {
            return h.effective_viewport();
        }
        if driver
            .capabilities()
            .supports(DriverCapability::AccessibilityTree)
            && let Ok(h) = self
                .fetch_fresh_hierarchy(driver, app_id, state, cancel)
                .await
        {
            return h.effective_viewport();
        }
        UiSize::new(
            crate::driver::DEFAULT_VIEWPORT_WIDTH,
            crate::driver::DEFAULT_VIEWPORT_HEIGHT,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TapKind {
    Tap,
    LongPress,
    DoubleTap,
}

impl TapKind {
    const fn command_name(self) -> &'static str {
        match self {
            Self::Tap => "tapOn",
            Self::LongPress => "longPressOn",
            Self::DoubleTap => "doubleTapOn",
        }
    }

    const fn label_prefix(self) -> &'static str {
        match self {
            Self::Tap => "Tap on",
            Self::LongPress => "Long press on",
            Self::DoubleTap => "Double tap on",
        }
    }
}

fn scroll_speed_to_duration_ms(speed: u8) -> u64 {
    let clamped = u64::from(speed.clamp(1, 100));
    1200_u64.saturating_sub(clamped * 10).clamp(150, 1500)
}

/// Computes swipe endpoints for content scrolling (`direction: DOWN` moves finger UP).
fn scroll_endpoints(direction: ScrollDirection, viewport: UiSize) -> (UiPoint, UiPoint) {
    let cx = viewport.width * 0.5;
    let cy = viewport.height * 0.5;
    match direction {
        ScrollDirection::Down => (
            UiPoint::new(cx, viewport.height * 0.75),
            UiPoint::new(cx, viewport.height * 0.25),
        ),
        ScrollDirection::Up => (
            UiPoint::new(cx, viewport.height * 0.25),
            UiPoint::new(cx, viewport.height * 0.75),
        ),
        ScrollDirection::Right => (
            UiPoint::new(viewport.width * 0.80, cy),
            UiPoint::new(viewport.width * 0.20, cy),
        ),
        ScrollDirection::Left => (
            UiPoint::new(viewport.width * 0.20, cy),
            UiPoint::new(viewport.width * 0.80, cy),
        ),
    }
}

/// Computes finger movement endpoints for explicit `swipe: { direction }`.
fn swipe_direction_endpoints(direction: ScrollDirection, size: UiSize) -> (UiPoint, UiPoint) {
    let cx = size.width * 0.5;
    let cy = size.height * 0.5;
    match direction {
        ScrollDirection::Up => (
            UiPoint::new(cx, size.height * 0.80),
            UiPoint::new(cx, size.height * 0.20),
        ),
        ScrollDirection::Down => (
            UiPoint::new(cx, size.height * 0.20),
            UiPoint::new(cx, size.height * 0.80),
        ),
        ScrollDirection::Left => (
            UiPoint::new(size.width * 0.85, cy),
            UiPoint::new(size.width * 0.15, cy),
        ),
        ScrollDirection::Right => (
            UiPoint::new(size.width * 0.15, cy),
            UiPoint::new(size.width * 0.85, cy),
        ),
    }
}

/// If `center` lies outside the `[20%, 80%]` band along the scroll axis, returns a bounded
/// nudge swipe that moves the element toward the 50% center line.
fn compute_center_nudge(
    center: UiPoint,
    viewport: UiSize,
    direction: ScrollDirection,
) -> Option<(UiPoint, UiPoint)> {
    match direction {
        ScrollDirection::Down | ScrollDirection::Up => {
            let low = viewport.height * 0.20;
            let high = viewport.height * 0.80;
            if center.y >= low && center.y <= high {
                return None;
            }
            let cx = viewport.width * 0.5;
            let target_y = viewport.height * 0.5;
            let delta =
                (target_y - center.y).clamp(-viewport.height * 0.35, viewport.height * 0.35);
            let start_y = (viewport.height * 0.5 - delta * 0.5)
                .clamp(viewport.height * 0.15, viewport.height * 0.85);
            let end_y = (start_y + delta).clamp(viewport.height * 0.15, viewport.height * 0.85);
            Some((UiPoint::new(cx, start_y), UiPoint::new(cx, end_y)))
        }
        ScrollDirection::Left | ScrollDirection::Right => {
            let low = viewport.width * 0.20;
            let high = viewport.width * 0.80;
            if center.x >= low && center.x <= high {
                return None;
            }
            let cy = viewport.height * 0.5;
            let target_x = viewport.width * 0.5;
            let delta = (target_x - center.x).clamp(-viewport.width * 0.35, viewport.width * 0.35);
            let start_x = (viewport.width * 0.5 - delta * 0.5)
                .clamp(viewport.width * 0.15, viewport.width * 0.85);
            let end_x = (start_x + delta).clamp(viewport.width * 0.15, viewport.width * 0.85);
            Some((UiPoint::new(start_x, cy), UiPoint::new(end_x, cy)))
        }
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

fn validate_relative_path_syntax(raw: &str, kind: &str) -> Result<(), FlowExecutionError> {
    if raw.is_empty() || raw.contains('\0') {
        return Err(FlowExecutionError::PathConfinement(format!(
            "{kind} path is empty or contains NUL byte"
        )));
    }
    if raw.starts_with('/') || raw.contains('\\') {
        return Err(FlowExecutionError::PathConfinement(format!(
            "{kind} path must be a relative POSIX path: {raw}"
        )));
    }
    if raw.len() >= 2 && raw.as_bytes()[0].is_ascii_alphabetic() && raw.as_bytes()[1] == b':' {
        return Err(FlowExecutionError::PathConfinement(format!(
            "{kind} path must not use a Windows drive prefix: {raw}"
        )));
    }
    Ok(())
}

struct ConfinedReadPath {
    lexical: PathBuf,
    canonical: PathBuf,
}

fn confine_relative_read_path(
    canonical_workspace: &Path,
    base_dir: &Path,
    raw_rel: &str,
    max_bytes: u64,
    kind: &str,
) -> Result<ConfinedReadPath, FlowExecutionError> {
    validate_relative_path_syntax(raw_rel, kind)?;
    let lexical = lexical_normalize(&base_dir.join(raw_rel));
    if lexical == canonical_workspace || !lexical.starts_with(canonical_workspace) {
        return Err(FlowExecutionError::PathConfinement(format!(
            "{kind} path escapes workspace: {raw_rel}"
        )));
    }
    let canonical = confine_existing_file(canonical_workspace, &lexical, max_bytes, kind)?;
    Ok(ConfinedReadPath { lexical, canonical })
}

fn confine_existing_file(
    canonical_workspace: &Path,
    candidate: &Path,
    max_bytes: u64,
    kind: &str,
) -> Result<PathBuf, FlowExecutionError> {
    let canonical = fs::canonicalize(candidate).map_err(|err| {
        FlowExecutionError::PathConfinement(format!(
            "{kind} file does not exist or cannot be resolved: {err}"
        ))
    })?;
    if canonical == canonical_workspace || !canonical.starts_with(canonical_workspace) {
        return Err(FlowExecutionError::PathConfinement(format!(
            "{kind} file escapes workspace after symlink resolution"
        )));
    }
    let meta = fs::metadata(&canonical).map_err(|err| {
        FlowExecutionError::PathConfinement(format!("cannot stat {kind} file: {err}"))
    })?;
    if !meta.is_file() {
        return Err(FlowExecutionError::PathConfinement(format!(
            "{kind} target is not a regular file"
        )));
    }
    if meta.len() > max_bytes {
        return Err(FlowExecutionError::PathConfinement(format!(
            "{kind} file exceeds maximum size of {max_bytes} bytes"
        )));
    }
    Ok(canonical)
}

fn confine_write_path(
    canonical_workspace: &Path,
    base_dir: &Path,
    raw_rel: &str,
    kind: &str,
) -> Result<PathBuf, FlowExecutionError> {
    validate_relative_path_syntax(raw_rel, kind)?;
    let canonical_base = fs::canonicalize(base_dir).unwrap_or_else(|_| lexical_normalize(base_dir));
    if canonical_base != canonical_workspace && !canonical_base.starts_with(canonical_workspace) {
        return Err(FlowExecutionError::PathConfinement(format!(
            "{kind} output base directory escapes workspace"
        )));
    }
    let lexical = lexical_normalize(&canonical_base.join(raw_rel));
    if lexical == canonical_workspace || !lexical.starts_with(canonical_workspace) {
        return Err(FlowExecutionError::PathConfinement(format!(
            "{kind} output path escapes workspace: {raw_rel}"
        )));
    }

    // Walk up to nearest existing ancestor and verify canonical confinement (no symlink escape).
    let mut ancestor = lexical.as_path();
    while !ancestor.exists() {
        let Some(parent) = ancestor.parent() else {
            return Err(FlowExecutionError::PathConfinement(format!(
                "{kind} output path has no existing ancestor inside workspace"
            )));
        };
        ancestor = parent;
    }
    let canonical_ancestor = fs::canonicalize(ancestor).map_err(|err| {
        FlowExecutionError::PathConfinement(format!("cannot resolve {kind} output ancestor: {err}"))
    })?;
    if canonical_ancestor != canonical_workspace
        && !canonical_ancestor.starts_with(canonical_workspace)
    {
        return Err(FlowExecutionError::PathConfinement(format!(
            "{kind} output ancestor escapes workspace via symlink"
        )));
    }
    if let Ok(lmeta) = fs::symlink_metadata(&lexical)
        && lmeta.file_type().is_symlink()
    {
        return Err(FlowExecutionError::PathConfinement(format!(
            "{kind} output target must not be a symlink"
        )));
    }
    Ok(lexical)
}

fn confine_write_path_strict_dir(
    canonical_workspace: &Path,
    strict_dir: &Path,
    raw_rel: &str,
    kind: &str,
) -> Result<PathBuf, FlowExecutionError> {
    validate_relative_path_syntax(raw_rel, kind)?;
    if raw_rel.contains('\\') || raw_rel.contains('\0') {
        return Err(FlowExecutionError::PathConfinement(format!(
            "{kind} output path contains invalid characters: {raw_rel}"
        )));
    }
    let mut flat_segments: Vec<String> = Vec::new();
    for component in Path::new(raw_rel).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(FlowExecutionError::PathConfinement(format!(
                    "{kind} output path cannot traverse outside its artifact directory: {raw_rel}"
                )));
            }
            Component::Normal(seg) => {
                let seg_str = seg.to_str().ok_or_else(|| {
                    FlowExecutionError::PathConfinement(format!(
                        "{kind} output path contains invalid UTF-8"
                    ))
                })?;
                if seg_str.is_empty() {
                    continue;
                }
                if seg_str.starts_with('.') {
                    return Err(FlowExecutionError::PathConfinement(format!(
                        "{kind} output path cannot contain hidden path segments: {raw_rel}"
                    )));
                }
                flat_segments.push(seg_str.to_string());
            }
        }
    }
    if flat_segments.is_empty() {
        return Err(FlowExecutionError::PathConfinement(format!(
            "{kind} output path must name a file: {raw_rel}"
        )));
    }

    // Flatten relative subpaths (e.g. `sub/checkout.png` -> `sub__checkout.png`) into a single
    // leaf filename directly inside the private `0o700` per-attempt directory so ZERO
    // intermediate subdirectories are ever created beneath `strict_dir`.
    let flat_filename = flat_segments.join("__");
    if flat_filename.is_empty()
        || flat_filename.starts_with('.')
        || flat_filename.contains('/')
        || flat_filename.contains('\\')
        || flat_filename.contains('\0')
    {
        return Err(FlowExecutionError::PathConfinement(format!(
            "{kind} output filename is not safe: {raw_rel}"
        )));
    }

    let norm_strict = lexical_normalize(strict_dir);
    if norm_strict == canonical_workspace || !norm_strict.starts_with(canonical_workspace) {
        return Err(FlowExecutionError::PathConfinement(format!(
            "{kind} artifact directory escapes workspace"
        )));
    }

    // Walk every existing component from `canonical_workspace` down to `norm_strict` refusing symlinks.
    let rel_from_ws = norm_strict
        .strip_prefix(canonical_workspace)
        .map_err(|_| FlowExecutionError::PathConfinement(format!("{kind} escapes workspace")))?;
    let mut cursor = canonical_workspace.to_path_buf();
    for comp in rel_from_ws.components() {
        if let Component::Normal(seg) = comp {
            cursor.push(seg);
            if let Ok(lmeta) = fs::symlink_metadata(&cursor)
                && lmeta.file_type().is_symlink()
            {
                return Err(FlowExecutionError::PathConfinement(format!(
                    "{kind} output directory traverses or targets a symlink"
                )));
            }
        }
    }

    let target = norm_strict.join(&flat_filename);
    if let Ok(lmeta) = fs::symlink_metadata(&target)
        && lmeta.file_type().is_symlink()
    {
        return Err(FlowExecutionError::PathConfinement(format!(
            "{kind} output path targets a symlink"
        )));
    }

    Ok(target)
}

/// Ensure `strict_dir` exists as a confined private (`0o700` on Unix) directory strictly inside
/// `canonical_workspace`, creating missing parent components step-by-step refusing symlinks and
/// atomically creating `strict_dir` with `DirBuilder::mode(0o700)` on first use.
fn ensure_strict_screenshot_dir(
    canonical_workspace: &Path,
    strict_dir: &Path,
    already_initialized: &mut bool,
) -> Result<(), FlowExecutionError> {
    let norm_strict = lexical_normalize(strict_dir);
    if norm_strict == canonical_workspace || !norm_strict.starts_with(canonical_workspace) {
        return Err(FlowExecutionError::PathConfinement(
            "screenshot artifact directory escapes workspace".to_string(),
        ));
    }
    let rel = norm_strict.strip_prefix(canonical_workspace).map_err(|_| {
        FlowExecutionError::PathConfinement(
            "screenshot artifact directory escapes workspace".to_string(),
        )
    })?;
    let comps: Vec<String> = rel
        .components()
        .map(|c| match c {
            Component::Normal(s) => s.to_str().map(ToString::to_string).ok_or_else(|| {
                FlowExecutionError::PathConfinement(
                    "screenshot directory contains non-UTF-8 segment".to_string(),
                )
            }),
            _ => Err(FlowExecutionError::PathConfinement(
                "screenshot directory contains invalid path component".to_string(),
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if comps.is_empty() {
        return Err(FlowExecutionError::PathConfinement(
            "screenshot artifact directory must be inside workspace".to_string(),
        ));
    }

    let mut cursor = canonical_workspace.to_path_buf();
    for seg in &comps[..comps.len() - 1] {
        cursor.push(seg);
        match fs::symlink_metadata(&cursor) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(FlowExecutionError::PathConfinement(
                        "screenshot directory ancestor is a symlink".to_string(),
                    ));
                }
                if !meta.file_type().is_dir() {
                    return Err(FlowExecutionError::PathConfinement(
                        "screenshot directory ancestor is not a directory".to_string(),
                    ));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                match fs::DirBuilder::new().create(&cursor) {
                    Ok(()) => {}
                    Err(create_err) if create_err.kind() == std::io::ErrorKind::AlreadyExists => {
                        let meta = fs::symlink_metadata(&cursor).map_err(|e| {
                            FlowExecutionError::PathConfinement(format!(
                                "cannot inspect screenshot ancestor: {e}"
                            ))
                        })?;
                        if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
                            return Err(FlowExecutionError::PathConfinement(
                                "screenshot ancestor is not a safe directory".to_string(),
                            ));
                        }
                    }
                    Err(create_err) => {
                        return Err(FlowExecutionError::PathConfinement(format!(
                            "cannot create screenshot parent directory: {create_err}"
                        )));
                    }
                }
            }
            Err(err) => {
                return Err(FlowExecutionError::PathConfinement(format!(
                    "cannot inspect screenshot ancestor: {err}"
                )));
            }
        }
    }

    cursor.push(&comps[comps.len() - 1]);
    if !*already_initialized {
        match fs::symlink_metadata(&cursor) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(&cursor).map_err(|e| {
                    FlowExecutionError::PathConfinement(format!(
                        "cannot create private screenshot attempt directory: {e}"
                    ))
                })?;
            }
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
                    return Err(FlowExecutionError::PathConfinement(
                        "screenshot attempt directory is a symlink or non-directory".to_string(),
                    ));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if (meta.mode() & 0o077) != 0 {
                        return Err(FlowExecutionError::PathConfinement(
                            "screenshot attempt directory must have private 0700 permissions"
                                .to_string(),
                        ));
                    }
                }
            }
            Err(err) => {
                return Err(FlowExecutionError::PathConfinement(format!(
                    "cannot inspect screenshot attempt directory: {err}"
                )));
            }
        }
        *already_initialized = true;
    }

    Ok(())
}

static MEDIA_STAGING_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct MediaStagingDir {
    dir: PathBuf,
}

impl Drop for MediaStagingDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn create_media_staging_dir() -> Result<MediaStagingDir, std::io::Error> {
    let seq = MEDIA_STAGING_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("deja-media-{}-{seq}-{nanos:x}", std::process::id()));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&dir)?;
    Ok(MediaStagingDir { dir })
}

fn stage_snapshot_media_file(
    staging: &MediaStagingDir,
    idx: usize,
    canonical_workspace: &Path,
    flow_dir: &Path,
    raw_path: &str,
    snapshot: &BTreeMap<String, Vec<u8>>,
) -> Result<PathBuf, FlowExecutionError> {
    validate_relative_path_syntax(raw_path, "media")?;
    let lexical = lexical_normalize(&flow_dir.join(raw_path));
    if lexical == canonical_workspace || !lexical.starts_with(canonical_workspace) {
        return Err(FlowExecutionError::PathConfinement(format!(
            "media file escapes workspace: {raw_path}"
        )));
    }
    let rel_key = lexical
        .strip_prefix(canonical_workspace)
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let Some(bytes) = snapshot.get(&rel_key) else {
        return Err(FlowExecutionError::PathConfinement(format!(
            "media file missing from preflight closure snapshot: {rel_key}"
        )));
    };
    if (bytes.len() as u64) > MAX_MEDIA_BYTES {
        return Err(FlowExecutionError::PathConfinement(format!(
            "media file exceeds maximum size of {MAX_MEDIA_BYTES} bytes: {rel_key}"
        )));
    }
    let file_name = lexical.file_name().ok_or_else(|| {
        FlowExecutionError::PathConfinement(format!("media file has no filename: {raw_path}"))
    })?;
    let slot_dir = staging.dir.join(idx.to_string());
    fs::create_dir(&slot_dir).map_err(|err| {
        FlowExecutionError::PathConfinement(format!("failed to create media staging slot: {err}"))
    })?;
    let dest = slot_dir.join(file_name);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&dest).map_err(|err| {
        FlowExecutionError::PathConfinement(format!("failed to create staged media file: {err}"))
    })?;
    std::io::Write::write_all(&mut file, bytes).map_err(|err| {
        FlowExecutionError::PathConfinement(format!("failed to write staged media file: {err}"))
    })?;
    file.sync_all().map_err(|err| {
        FlowExecutionError::PathConfinement(format!("failed to sync staged media file: {err}"))
    })?;
    Ok(dest)
}

fn read_bounded_file(path: &Path, limit: u64) -> Result<Vec<u8>, FlowExecutionError> {
    let file = File::open(path).map_err(|err| {
        FlowExecutionError::PathConfinement(format!("failed to open file: {err}"))
    })?;
    let meta = file.metadata().map_err(|err| {
        FlowExecutionError::PathConfinement(format!("failed to stat file: {err}"))
    })?;
    if !meta.is_file() || meta.len() > limit {
        return Err(FlowExecutionError::PathConfinement(format!(
            "file is not a regular file under {limit} bytes"
        )));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len().min(limit)).unwrap_or(0));
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|err| {
            FlowExecutionError::PathConfinement(format!("failed to read file: {err}"))
        })?;
    if (bytes.len() as u64) > limit {
        return Err(FlowExecutionError::PathConfinement(format!(
            "file exceeds {limit} byte limit"
        )));
    }
    Ok(bytes)
}

#[cfg(test)]
mod scrub_tests {
    use super::{FlowExecutionConfig, FlowExecutor};
    use crate::capabilities::VisibleContext;
    use crate::classifier::{FailedStepContext, StepFailureKind};
    use deja_core::{create_redactor, provider_key_redactor};
    use std::path::PathBuf;

    #[test]
    fn scrub_failed_step_redacts_visible_context_fixture_and_provider_secrets() {
        let secret_token = ["sk", "-1234567890abcdefghijklmnop"].concat();
        let fixture_pw = "fixture-secret-pw-99";
        let mut cfg = FlowExecutionConfig::new(PathBuf::from("/tmp/fake-ws"));
        cfg.redactor = create_redactor("/tmp/fake-ws", None, &[fixture_pw.to_string()]);
        cfg.provider_redactor = provider_key_redactor(std::iter::empty::<(&str, &str)>());
        let executor = FlowExecutor::new(cfg);

        let mut failed = FailedStepContext {
            step_index: 1,
            command: "assertVisible".to_string(),
            label: format!("Assert {fixture_pw}"),
            kind: StepFailureKind::AssertionFailed,
            message: format!("token {secret_token}"),
            failed_app_id: None,
            visible_context: Some(VisibleContext::from_candidates(
                [
                    "Sign in",
                    &format!("Email: {fixture_pw}"),
                    &format!("Header: {secret_token}"),
                    "Workspace: /tmp/fake-ws/flows/a.yaml",
                ],
                3,
            )),
            crash_signal: None,
        };

        executor.scrub_failed_step(&mut failed);

        assert!(!failed.label.contains(fixture_pw));
        assert!(!failed.message.contains(&secret_token));
        let vc = failed.visible_context.expect("visible context preserved");
        assert!(vc.is_truncated(), "truncated flag must be preserved");
        assert_eq!(vc.items().len(), 3);
        let joined = vc.items().join(" | ");
        assert!(joined.contains("Sign in"));
        assert!(joined.contains("<redacted-fixture-value>"));
        assert!(!joined.contains(fixture_pw));
        assert!(!joined.contains(&secret_token));
    }

    #[tokio::test]
    async fn capture_final_visible_context_populates_scrubbed_bounded_snapshot_on_pass() {
        use crate::capabilities::{DriverCapabilities, DriverCapability};
        use crate::driver::{
            DriverCrashSignal, DriverError, DriverFuture, KeyPress, LaunchOptions, OpenLinkOptions,
            PlatformDriver, SwipeParams, UiHierarchy, UiNode, UiPoint, UiRect, UiSize,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio_util::sync::CancellationToken;

        struct MiniDriver {
            caps: DriverCapabilities,
            hierarchy: UiHierarchy,
            hierarchy_calls: AtomicUsize,
        }

        impl PlatformDriver for MiniDriver {
            fn platform(&self) -> &'static str {
                "ios"
            }
            fn capabilities(&self) -> &DriverCapabilities {
                &self.caps
            }
            fn ensure_session<'a>(&'a self, _c: &'a CancellationToken) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn launch_app<'a>(
                &'a self,
                _a: &'a str,
                _o: &'a LaunchOptions,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn stop_app<'a>(
                &'a self,
                _a: &'a str,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn kill_app<'a>(
                &'a self,
                _a: &'a str,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn clear_state<'a>(
                &'a self,
                _a: &'a str,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn clear_keychain<'a>(&'a self, _c: &'a CancellationToken) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn open_link<'a>(
                &'a self,
                _a: Option<&'a str>,
                _u: &'a str,
                _o: &'a OpenLinkOptions,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn add_media<'a>(
                &'a self,
                _p: &'a [PathBuf],
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn hierarchy<'a>(
                &'a self,
                _a: &'a str,
                _s: bool,
                _k: Option<&'a str>,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, UiHierarchy> {
                Box::pin(async move {
                    self.hierarchy_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(self.hierarchy.clone())
                })
            }
            fn tap_point<'a>(
                &'a self,
                _a: &'a str,
                _p: UiPoint,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn long_press_point<'a>(
                &'a self,
                _a: &'a str,
                _p: UiPoint,
                _d: Option<u64>,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn double_tap_point<'a>(
                &'a self,
                _a: &'a str,
                _p: UiPoint,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn swipe<'a>(
                &'a self,
                _p: &'a SwipeParams,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn input_text<'a>(
                &'a self,
                _a: &'a str,
                _t: &'a str,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn erase_text<'a>(
                &'a self,
                _a: &'a str,
                _n: u32,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn press_key<'a>(
                &'a self,
                _a: &'a str,
                _k: &'a KeyPress,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn hide_keyboard<'a>(
                &'a self,
                _a: &'a str,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn back<'a>(&'a self, _a: &'a str, _c: &'a CancellationToken) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn take_screenshot<'a>(
                &'a self,
                _a: &'a str,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, Vec<u8>> {
                Box::pin(async { Err(DriverError::InvalidArgument("unused".to_string())) })
            }
            fn check_app_crash<'a>(
                &'a self,
                _a: &'a str,
                _s: u64,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, Option<DriverCrashSignal>> {
                Box::pin(async { Ok(None) })
            }
            fn shutdown<'a>(&'a self) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
        }

        let secret = ["sk", "-1234567890abcdefghijklmnop"].concat();
        let mut nodes = Vec::new();
        for i in 0..35 {
            nodes.push(UiNode {
                index: i,
                label: if i == 0 {
                    format!("Token {secret}")
                } else {
                    format!("Row {i:02}")
                },
                enabled: true,
                hittable: true,
                is_on_screen: true,
                frame: UiRect::new(0.0, i as f64 * 10.0, 100.0, 10.0),
                ..UiNode::default()
            });
        }
        let mut h = UiHierarchy {
            app_id: "dev.deja.app".to_string(),
            viewport: Some(UiSize::new(390.0, 844.0)),
            nodes,
            ..UiHierarchy::default()
        };
        h.hierarchy_hash = h.hierarchy_hash_hex();

        let driver = MiniDriver {
            caps: DriverCapabilities::with([DriverCapability::AccessibilityTree]),
            hierarchy: h,
            hierarchy_calls: AtomicUsize::new(0),
        };
        let ws = std::env::temp_dir();
        let mut cfg = FlowExecutionConfig::new(ws);
        cfg.app_id_override = Some("dev.deja.app".to_string());
        cfg.capture_final_visible_context = true;
        let exec = FlowExecutor::new(cfg);

        // Flow with only `launchApp` has no cached hierarchy yet -> must fetch 1 fresh hierarchy.
        let outcome = exec
            .execute_yaml(&driver, "- launchApp\n", &CancellationToken::new())
            .await;
        assert!(outcome.passed);
        assert_eq!(driver.hierarchy_calls.load(Ordering::SeqCst), 1);
        let vc = outcome
            .final_visible_context
            .expect("final_visible_context populated");
        assert_eq!(vc.items().len(), VisibleContext::DEFAULT_LIMIT);
        assert!(vc.is_truncated());
        assert!(!vc.items().join(" ").contains(&secret));
    }

    #[tokio::test]
    async fn capture_final_visible_context_omits_stale_hierarchy_when_fresh_fetch_fails() {
        use crate::capabilities::{DriverCapabilities, DriverCapability};
        use crate::driver::{
            DriverCrashSignal, DriverError, DriverFuture, KeyPress, LaunchOptions, OpenLinkOptions,
            PlatformDriver, SwipeParams, UiHierarchy, UiNode, UiPoint, UiRect, UiSize,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio_util::sync::CancellationToken;

        struct FailSecondHierarchyDriver {
            caps: DriverCapabilities,
            hierarchy: UiHierarchy,
            calls: AtomicUsize,
        }

        impl PlatformDriver for FailSecondHierarchyDriver {
            fn platform(&self) -> &'static str {
                "ios"
            }
            fn capabilities(&self) -> &DriverCapabilities {
                &self.caps
            }
            fn ensure_session<'a>(&'a self, _c: &'a CancellationToken) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn launch_app<'a>(
                &'a self,
                _a: &'a str,
                _o: &'a LaunchOptions,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn stop_app<'a>(
                &'a self,
                _a: &'a str,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn kill_app<'a>(
                &'a self,
                _a: &'a str,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn clear_state<'a>(
                &'a self,
                _a: &'a str,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn clear_keychain<'a>(&'a self, _c: &'a CancellationToken) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn open_link<'a>(
                &'a self,
                _a: Option<&'a str>,
                _u: &'a str,
                _o: &'a OpenLinkOptions,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn add_media<'a>(
                &'a self,
                _p: &'a [PathBuf],
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn hierarchy<'a>(
                &'a self,
                _a: &'a str,
                _s: bool,
                _k: Option<&'a str>,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, UiHierarchy> {
                Box::pin(async move {
                    let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                    if n == 1 {
                        Ok(self.hierarchy.clone())
                    } else {
                        Err(DriverError::Transport {
                            operation: "hierarchy".to_string(),
                            detail: "simulated transient hierarchy failure".to_string(),
                            transient: true,
                        })
                    }
                })
            }
            fn tap_point<'a>(
                &'a self,
                _a: &'a str,
                _p: UiPoint,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn long_press_point<'a>(
                &'a self,
                _a: &'a str,
                _p: UiPoint,
                _d: Option<u64>,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn double_tap_point<'a>(
                &'a self,
                _a: &'a str,
                _p: UiPoint,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn swipe<'a>(
                &'a self,
                _p: &'a SwipeParams,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn input_text<'a>(
                &'a self,
                _a: &'a str,
                _t: &'a str,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn erase_text<'a>(
                &'a self,
                _a: &'a str,
                _n: u32,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn press_key<'a>(
                &'a self,
                _a: &'a str,
                _k: &'a KeyPress,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn hide_keyboard<'a>(
                &'a self,
                _a: &'a str,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn back<'a>(&'a self, _a: &'a str, _c: &'a CancellationToken) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
            fn take_screenshot<'a>(
                &'a self,
                _a: &'a str,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, Vec<u8>> {
                Box::pin(async { Ok(vec![0x89, b'P', b'N', b'G']) })
            }
            fn check_app_crash<'a>(
                &'a self,
                _a: &'a str,
                _s: u64,
                _c: &'a CancellationToken,
            ) -> DriverFuture<'a, Option<DriverCrashSignal>> {
                Box::pin(async { Ok(None) })
            }
            fn shutdown<'a>(&'a self) -> DriverFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
        }

        let mut h = UiHierarchy {
            app_id: "dev.deja.app".to_string(),
            viewport: Some(UiSize::new(390.0, 844.0)),
            nodes: vec![UiNode {
                index: 0,
                label: "StalePreTapButton".to_string(),
                enabled: true,
                hittable: true,
                is_on_screen: true,
                frame: UiRect::new(10.0, 10.0, 100.0, 44.0),
                ..UiNode::default()
            }],
            ..UiHierarchy::default()
        };
        h.hierarchy_hash = h.hierarchy_hash_hex();

        let driver = FailSecondHierarchyDriver {
            caps: DriverCapabilities::with([DriverCapability::AccessibilityTree]),
            hierarchy: h,
            calls: AtomicUsize::new(0),
        };
        let mut cfg = FlowExecutionConfig::new(std::env::temp_dir());
        cfg.app_id_override = Some("dev.deja.app".to_string());
        cfg.capture_final_visible_context = true;
        let exec = FlowExecutor::new(cfg);

        // `tapOn` succeeds on call #1 and invalidates `last_hierarchy_is_fresh`;
        // the post-flow fresh fetch (call #2) fails -> must NOT return stale pre-tap context!
        let outcome = exec
            .execute_yaml(
                &driver,
                "- tapOn: StalePreTapButton\n",
                &CancellationToken::new(),
            )
            .await;
        assert!(outcome.passed);
        assert_eq!(driver.calls.load(Ordering::SeqCst), 2);
        assert!(
            outcome.final_visible_context.is_none(),
            "stale pre-action hierarchy must never be returned when final fresh fetch fails"
        );
    }
}
