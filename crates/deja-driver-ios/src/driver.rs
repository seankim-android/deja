//! Production iOS Simulator [`PlatformDriver`] implementation (`IosDriver`, `DEJA-64`).
//!
//! Combines `xcrun simctl` lifecycle management, warm `DejaXCTestRunner` XCUITest HTTP sessions
//! (reused across sequential flows with 0 ms re-attach), compound `/act` and `/waitForSelector`,
//! and native `.ips` crash report detection (`DEJA-46`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex as StdMutex, MutexGuard as StdMutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deja_core::{LaunchArgumentValue, provider_key_redactor};
use deja_flow::{
    ActAction, ActOutcome, ActPayload, DriverCapabilities, DriverCapability, DriverCrashSignal,
    DriverError, DriverFuture, KeyPress, LaunchOptions, ObservationSource, OpenLinkOptions,
    PlatformDriver, SwipeParams, UiHierarchy, UiNode, UiPoint, UiRect, UiSize,
    UnsupportedCapability, VisibleContext, WaitSelectorOutcome, WaitSelectorRequest,
    build_visible_context, wait_local,
};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::client::{
    ActKind, ActRequest, AgentClient, HierarchyNode, HierarchyResponse, KeyRequest, SwipeRequest,
    TapRequest, TextMatchMode, TypeRequest, WaitForSelectorRequest, WarmupRequest,
};
use crate::crash::{
    CrashSearchOptions, default_crash_report_dir, find_crash_report, find_crash_report_with_grace,
};
use crate::error::{IosDriverError, Result};
#[cfg(test)]
use crate::process::acquire_simulator_lock_in_dir;
use crate::process::{SimulatorLockGuard, acquire_simulator_lock, validate_udid};
use crate::runner::{IosRunnerConfig, IosRunnerSession};
use crate::simctl::{
    Simctl, SimctlEnv, clear_state_targets_sync, map_privacy_action, map_privacy_service,
    validate_launch_inputs, validate_open_link_url,
};

const MAX_KEY_BATCH: u32 = 100;
const MAX_SERVER_WARMUP_MS: u64 = 60_000;
const DEFAULT_LONG_PRESS_MS: u64 = 1_000;
const MAX_SERVER_WAIT_TIMEOUT: Duration = Duration::from_secs(60);
const SERVER_WAIT_GRACE_MS: u64 = 250;
const ZERO_TIMEOUT_SINGLE_PROBE_CAP: Duration = Duration::from_secs(10);
const MAX_SERVER_POST_ACTION_DELAY_MS: u64 = 5_000;
const KEYBOARD_DISMISS_VERIFY_TIMEOUT: Duration = Duration::from_millis(500);

static SCREENSHOT_NONCE: AtomicU64 = AtomicU64::new(0);

/// Configuration for [`IosDriver`].
#[derive(Debug, Clone)]
pub struct IosDriverConfig {
    pub udid: String,
    pub deja_home: Option<PathBuf>,
    pub env: SimctlEnv,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub app_launch_timeout: Duration,
    pub crash_report_dir: Option<PathBuf>,
    pub crash_report_grace: Duration,
    pub auto_boot_simulator: bool,
    /// When `true` (default), configures simulator `ReduceMotionEnabled = 1` on attach and
    /// advertises [`DriverCapability::DisableAnimations`]. Set to `false` to opt out.
    pub disable_animations: bool,
}

impl IosDriverConfig {
    pub fn new(udid: impl Into<String>) -> Self {
        Self {
            udid: udid.into(),
            deja_home: None,
            env: SimctlEnv::default(),
            startup_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(30),
            app_launch_timeout: Duration::from_secs(15),
            crash_report_dir: default_crash_report_dir(),
            crash_report_grace: Duration::from_secs(2),
            auto_boot_simulator: true,
            disable_animations: true,
        }
    }

    /// Configures whether simulator Reduce Motion (`ReduceMotionEnabled`) is enabled on attach.
    #[must_use]
    pub fn with_disable_animations(mut self, enabled: bool) -> Self {
        self.disable_animations = enabled;
        self
    }
}

#[derive(Debug, Clone)]
struct SessionHandle {
    client: AgentClient,
    /// Set synchronously if an in-flight RPC on this session is dropped/aborted (e.g., via an
    /// outer `tokio::time::timeout`), forcing teardown before the runner can ever be reused.
    dirty: Arc<AtomicBool>,
}

impl SessionHandle {
    fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::SeqCst)
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }
}

/// RAII guard for an in-flight `busyLock`-acquiring XCTest HTTP RPC.
///
/// Disarmed only when the RPC completes cleanly; if the future is dropped early (for example when
/// `deja-flow`'s local selector loop drops `driver.hierarchy(...)` on timeout), `Drop`
/// synchronously marks the session dirty so the next driver call tears down and recycles the
/// runner instead of hitting a stale `409 Conflict`.
struct InFlightRpcGuard {
    dirty: Arc<AtomicBool>,
    armed: bool,
}

impl InFlightRpcGuard {
    fn new(dirty: Arc<AtomicBool>) -> Self {
        Self { dirty, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for InFlightRpcGuard {
    fn drop(&mut self) {
        if self.armed {
            self.dirty.store(true, Ordering::SeqCst);
        }
    }
}

#[derive(Debug)]
enum SessionState {
    Detached,
    Stopping,
    Runner {
        session: Box<IosRunnerSession>,
        dirty: Arc<AtomicBool>,
    },
    #[cfg(test)]
    Mock {
        client: AgentClient,
        dirty: Arc<AtomicBool>,
        teardown_delay: Duration,
        teardown_done: Option<Arc<AtomicBool>>,
    },
}

impl SessionState {
    fn handle(&self) -> Option<SessionHandle> {
        match self {
            Self::Detached | Self::Stopping => None,
            Self::Runner { session, dirty } => Some(SessionHandle {
                client: session.client().clone(),
                dirty: Arc::clone(dirty),
            }),
            #[cfg(test)]
            Self::Mock { client, dirty, .. } => Some(SessionHandle {
                client: client.clone(),
                dirty: Arc::clone(dirty),
            }),
        }
    }
}

/// Ensures `SessionState::Stopping` transitions to `SessionState::Detached` only AFTER the taken
/// session finishes shutting down (or is synchronously dropped on cancellation), while still under
/// the `lifecycle` lock.
struct ResetStoppingGuard<'a> {
    session: &'a StdMutex<SessionState>,
}

impl Drop for ResetStoppingGuard<'_> {
    fn drop(&mut self) {
        let mut guard = self
            .session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(*guard, SessionState::Stopping) {
            *guard = SessionState::Detached;
        }
    }
}

async fn teardown_session(taken: SessionState) -> Result<()> {
    match taken {
        SessionState::Runner { session, .. } => session.shutdown().await,
        SessionState::Detached | SessionState::Stopping => Ok(()),
        #[cfg(test)]
        SessionState::Mock {
            teardown_delay,
            teardown_done,
            ..
        } => {
            if !teardown_delay.is_zero() {
                tokio::time::sleep(teardown_delay).await;
            }
            if let Some(flag) = teardown_done {
                flag.store(true, Ordering::SeqCst);
            }
            Ok(())
        }
    }
}

/// RAII guard for a blocking state-wipe worker launched by [`IosDriver`].
///
/// If the async future is dropped before the worker exits (for example by an outer timeout),
/// `Drop` synchronously signals cooperative cancellation AND stores the live `JoinHandle` in
/// `IosDriver::pending_wipe` so subsequent driver operations and `shutdown` wait for the worker
/// to finish before mutating state or releasing simulator ownership.
struct ActiveWipeGuard<'a> {
    cancel_flag: Arc<AtomicBool>,
    slot: &'a StdMutex<Option<tokio::task::JoinHandle<()>>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for ActiveWipeGuard<'_> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.cancel_flag.store(true, Ordering::SeqCst);
            let mut guard = self
                .slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = Some(handle);
        }
    }
}

/// Restores a taken `pending_wipe` handle back into `IosDriver::pending_wipe` if the caller
/// awaiting it is itself dropped before the worker exits.
struct RestorePendingWipeGuard<'a> {
    slot: &'a StdMutex<Option<tokio::task::JoinHandle<()>>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for RestorePendingWipeGuard<'_> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let mut guard = self
                .slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = Some(handle);
        }
    }
}

#[cfg(test)]
#[derive(Clone)]
struct ClearStateTestHook(Arc<dyn Fn(&AtomicBool) + Send + Sync>);

#[cfg(test)]
impl std::fmt::Debug for ClearStateTestHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClearStateTestHook(..)")
    }
}

struct TempFileGuard {
    path: PathBuf,
}

impl TempFileGuard {
    fn new(prefix: &str, ext: &str) -> Self {
        let nonce = SCREENSHOT_NONCE.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "deja-ios-{prefix}-{}-{nanos}-{nonce}.{ext}",
            std::process::id()
        ));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Guards destructive simulator operations (`simctl terminate/clear_state/keychain/privacy/
/// addmedia/openurl/launch`) so they only execute while this driver holds exclusive simulator
/// ownership — either through its active runner session or via a temporary per-UDID
/// [`SimulatorLockGuard`] when detached.
struct SimulatorOwnershipLease<'a> {
    // Dropped before `_lifecycle` so the temporary simulator lock is released before any waiting
    // `ensure_session` or `shutdown` acquires `lifecycle`. Shared via `Arc` with any spawned
    // blocking state-wipe worker so an aborted future cannot release the OS lock early.
    temp_lock: Option<Arc<SimulatorLockGuard>>,
    _lifecycle: tokio::sync::MutexGuard<'a, ()>,
}

/// Production iOS Simulator [`PlatformDriver`].
#[derive(Debug)]
pub struct IosDriver {
    udid: String,
    config: IosDriverConfig,
    simctl: Simctl,
    capabilities: DriverCapabilities,
    /// Short-held synchronous slot for fast-path `SessionHandle` cloning (never held across `.await`).
    session: StdMutex<SessionState>,
    /// Unfinished blocking state-wipe worker handed off on future drop so subsequent operations
    /// and shutdown await its exit before proceeding or releasing simulator ownership.
    pending_wipe: StdMutex<Option<tokio::task::JoinHandle<()>>>,
    /// Serializes runner startup, dead-session invalidation teardown, detached simulator ownership
    /// leases, and explicit `shutdown` so a replacement runner never races teardown or lock release.
    lifecycle: AsyncMutex<()>,
    attach_count: AtomicU64,
    runner_pid: AtomicU32,
    #[cfg(test)]
    max_server_post_action_delay_ms: u64,
    #[cfg(test)]
    simulator_locks_dir: Option<PathBuf>,
    #[cfg(test)]
    keyboard_dismiss_verify_timeout: Duration,
    #[cfg(test)]
    on_clear_state_worker: Option<ClearStateTestHook>,
}

impl IosDriver {
    /// Creates a new [`IosDriver`] bound to a validated simulator UDID.
    ///
    /// Does not spawn `xcodebuild` until [`PlatformDriver::ensure_session`] (or an RPC requiring
    /// the companion runner) is called.
    pub fn new(config: IosDriverConfig) -> Result<Self> {
        let udid = validate_udid(&config.udid)?;
        let simctl = Simctl::with_env(config.env.clone());
        let capabilities = ios_capabilities(config.disable_animations);
        Ok(Self {
            udid,
            config,
            simctl,
            capabilities,
            session: StdMutex::new(SessionState::Detached),
            pending_wipe: StdMutex::new(None),
            lifecycle: AsyncMutex::new(()),
            attach_count: AtomicU64::new(0),
            runner_pid: AtomicU32::new(0),
            #[cfg(test)]
            max_server_post_action_delay_ms: MAX_SERVER_POST_ACTION_DELAY_MS,
            #[cfg(test)]
            simulator_locks_dir: None,
            #[cfg(test)]
            keyboard_dismiss_verify_timeout: KEYBOARD_DISMISS_VERIFY_TIMEOUT,
            #[cfg(test)]
            on_clear_state_worker: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_mock_client_for_tests(
        config: IosDriverConfig,
        client: AgentClient,
    ) -> Result<Self> {
        Self::with_mock_client_and_teardown_for_tests(config, client, Duration::ZERO, None)
    }

    #[cfg(test)]
    pub(crate) fn with_mock_client_and_teardown_for_tests(
        config: IosDriverConfig,
        client: AgentClient,
        teardown_delay: Duration,
        teardown_done: Option<Arc<AtomicBool>>,
    ) -> Result<Self> {
        let driver = Self::new(config)?;
        driver.attach_count.store(1, Ordering::SeqCst);
        let mut guard = driver.lock_session();
        *guard = SessionState::Mock {
            client,
            dirty: Arc::new(AtomicBool::new(false)),
            teardown_delay,
            teardown_done,
        };
        drop(guard);
        Ok(driver)
    }

    fn lock_session(&self) -> StdMutexGuard<'_, SessionState> {
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn max_server_post_action_delay_ms(&self) -> u64 {
        #[cfg(test)]
        {
            self.max_server_post_action_delay_ms
        }
        #[cfg(not(test))]
        {
            MAX_SERVER_POST_ACTION_DELAY_MS
        }
    }

    #[cfg(test)]
    pub(crate) fn has_active_session(&self) -> bool {
        matches!(
            *self.lock_session(),
            SessionState::Runner { .. } | SessionState::Mock { .. }
        )
    }

    #[cfg(test)]
    pub(crate) fn is_detached(&self) -> bool {
        matches!(*self.lock_session(), SessionState::Detached)
    }

    #[cfg(test)]
    pub(crate) fn session_needs_quiesce(&self) -> bool {
        self.lock_session()
            .handle()
            .is_some_and(|handle| handle.is_dirty())
    }

    /// Canonical uppercase simulator UDID.
    pub fn udid(&self) -> &str {
        &self.udid
    }

    /// Returns the underlying `xcrun simctl` helper configured with this driver's environment.
    pub fn simctl(&self) -> &Simctl {
        &self.simctl
    }

    /// Number of times this driver started/attached a `DejaXCTestRunner` session.
    pub fn attach_count(&self) -> u64 {
        self.attach_count.load(Ordering::SeqCst)
    }

    /// Last observed companion runner PID (if an active runner session has reported one).
    pub fn runner_pid(&self) -> Option<u32> {
        let pid = self.runner_pid.load(Ordering::SeqCst);
        (pid != 0).then_some(pid)
    }

    /// Queries `xcrun simctl list devices -j` and returns this simulator's normalized iOS runtime
    /// version (for example `"ios-27.2"`).
    pub async fn runtime_version(&self) -> Result<String> {
        self.simctl.device_runtime_version(&self.udid).await
    }

    async fn await_pending_wipe_locked(&self) {
        let pending = {
            let mut guard = self
                .pending_wipe
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.take()
        };
        if let Some(handle) = pending {
            let mut restore = RestorePendingWipeGuard {
                slot: &self.pending_wipe,
                handle: Some(handle),
            };
            if let Some(h) = restore.handle.as_mut() {
                let _ = h.await;
            }
            restore.handle = None;
        }
    }

    async fn ensure_session_handle(
        &self,
        cancel: &CancellationToken,
    ) -> std::result::Result<SessionHandle, DriverError> {
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }

        // Fast path: clone existing clean session handle under the short synchronous lock and
        // probe `/health` without blocking on `lifecycle`. If an earlier RPC future was dropped
        // mid-flight (`is_dirty()`), fall through to tear down and recycle the runner.
        let existing_handle = self.lock_session().handle();
        if let Some(ref handle) = existing_handle
            && !handle.is_dirty()
        {
            let health_res = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                res = handle.client.health() => res,
            };
            match health_res {
                Ok(h) if h.status == "ok" => {
                    if !handle.is_dirty() {
                        self.runner_pid.store(h.pid, Ordering::SeqCst);
                        return Ok(handle.clone());
                    }
                }
                Ok(h) => {
                    debug!(
                        udid = %self.udid,
                        status = %h.status,
                        "warm DejaXCTestRunner reported non-ok health; recycling session"
                    );
                }
                Err(err) => {
                    debug!(
                        udid = %self.udid,
                        error = %err,
                        "warm DejaXCTestRunner health probe failed; recycling session"
                    );
                }
            }
        }

        // Slow path: serialize teardown and startup behind `lifecycle` so concurrent callers wait
        // for any in-progress runner shutdown, aborted state wipe, and simulator-lock release
        // before starting a replacement.
        let _lifecycle = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(DriverError::Cancelled),
            guard = self.lifecycle.lock() => guard,
        };
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }

        self.await_pending_wipe_locked().await;
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }

        let startup_started = Instant::now();
        let startup_deadline = tokio::time::Instant::now() + self.config.startup_timeout;

        // Re-check under `lifecycle` in case a sibling caller already started a clean session
        // or recycled the unhealthy one.
        let current_handle = self.lock_session().handle();
        if let Some(handle) = current_handle {
            let same_unhealthy = existing_handle
                .as_ref()
                .is_some_and(|prev| prev.client.base_url() == handle.client.base_url());
            if !same_unhealthy && !handle.is_dirty() {
                let health_res = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    res = handle.client.health() => res,
                };
                if let Ok(h) = health_res
                    && h.status == "ok"
                    && !handle.is_dirty()
                {
                    self.runner_pid.store(h.pid, Ordering::SeqCst);
                    return Ok(handle);
                }
            }
            self.invalidate_session_locked(handle.client.base_url())
                .await;
        }

        if self.config.auto_boot_simulator || self.config.disable_animations {
            let setup_lock = self
                .acquire_temp_simulator_lock()
                .map_err(|err| map_ios_err("ensure_session", err))?;

            if self.config.auto_boot_simulator {
                let boot_outcome = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        drop(setup_lock);
                        return Err(DriverError::Cancelled);
                    }
                    timed = tokio::time::timeout_at(
                        startup_deadline,
                        self.simctl.ensure_booted(&self.udid),
                    ) => timed,
                };
                match boot_outcome {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        drop(setup_lock);
                        return Err(map_ios_err("ensure_booted", err));
                    }
                    Err(_) => {
                        drop(setup_lock);
                        return Err(DriverError::Timeout {
                            operation: "ensure_booted".to_string(),
                            elapsed_ms: startup_started.elapsed().as_millis() as u64,
                            detail: format!(
                                "timed out after {}ms waiting for simulator boot (`xcrun simctl bootstatus -b`)",
                                self.config.startup_timeout.as_millis()
                            ),
                        });
                    }
                }
            }

            if self.config.disable_animations {
                let motion_outcome = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        drop(setup_lock);
                        return Err(DriverError::Cancelled);
                    }
                    timed = tokio::time::timeout_at(
                        startup_deadline,
                        self.simctl.set_reduce_motion(&self.udid, true),
                    ) => timed,
                };
                match motion_outcome {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        drop(setup_lock);
                        return Err(map_ios_err("set_reduce_motion", err));
                    }
                    Err(_) => {
                        drop(setup_lock);
                        return Err(DriverError::Timeout {
                            operation: "set_reduce_motion".to_string(),
                            elapsed_ms: startup_started.elapsed().as_millis() as u64,
                            detail: format!(
                                "timed out after {}ms configuring simulator ReduceMotionEnabled",
                                self.config.startup_timeout.as_millis()
                            ),
                        });
                    }
                }
            }

            drop(setup_lock);
        }

        let remaining_startup =
            startup_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining_startup.is_zero() {
            return Err(DriverError::Timeout {
                operation: "ensure_session".to_string(),
                elapsed_ms: startup_started.elapsed().as_millis() as u64,
                detail: format!(
                    "startup timeout ({}ms) exhausted before launching DejaXCTestRunner",
                    self.config.startup_timeout.as_millis()
                ),
            });
        }

        let runner_cfg = IosRunnerConfig {
            udid: self.udid.clone(),
            deja_home: self.config.deja_home.clone(),
            env: self.config.env.clone(),
            startup_timeout: remaining_startup,
            request_timeout: self.config.request_timeout,
        };
        let session = IosRunnerSession::start(runner_cfg, cancel.clone())
            .await
            .map_err(|err| map_ios_err("ensure_session", err))?;

        let dirty = Arc::new(AtomicBool::new(false));
        let handle = SessionHandle {
            client: session.client().clone(),
            dirty: Arc::clone(&dirty),
        };
        let pid = session.agent_pid().unwrap_or(0);
        self.runner_pid.store(pid, Ordering::SeqCst);
        self.attach_count.fetch_add(1, Ordering::SeqCst);

        let previous = {
            let mut guard = self.lock_session();
            std::mem::replace(
                &mut *guard,
                SessionState::Runner {
                    session: Box::new(session),
                    dirty,
                },
            )
        };
        let _ = teardown_session(previous).await;

        Ok(handle)
    }

    async fn get_handle_or_start(
        &self,
        cancel: &CancellationToken,
    ) -> std::result::Result<SessionHandle, DriverError> {
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }
        if let Some(handle) = self.lock_session().handle()
            && !handle.is_dirty()
        {
            return Ok(handle);
        }
        self.ensure_session_handle(cancel).await
    }

    async fn invalidate_session_if_matching(&self, base_url: &str) {
        let _lifecycle = self.lifecycle.lock().await;
        self.await_pending_wipe_locked().await;
        self.invalidate_session_locked(base_url).await;
    }

    async fn invalidate_session_locked(&self, base_url: &str) {
        self.await_pending_wipe_locked().await;
        let taken = {
            let mut guard = self.lock_session();
            let matches = guard
                .handle()
                .is_some_and(|handle| handle.client.base_url() == base_url);
            if matches {
                self.runner_pid.store(0, Ordering::SeqCst);
                Some(std::mem::replace(&mut *guard, SessionState::Stopping))
            } else {
                None
            }
        };
        if let Some(old) = taken {
            let _reset_guard = ResetStoppingGuard {
                session: &self.session,
            };
            let _ = teardown_session(old).await;
        }
    }

    fn acquire_temp_simulator_lock(&self) -> Result<SimulatorLockGuard> {
        #[cfg(test)]
        if let Some(ref locks_dir) = self.simulator_locks_dir {
            return acquire_simulator_lock_in_dir(locks_dir, &self.udid);
        }
        acquire_simulator_lock(&self.udid)
    }

    async fn acquire_simulator_ownership(
        &self,
        operation: &str,
        cancel: &CancellationToken,
    ) -> std::result::Result<SimulatorOwnershipLease<'_>, DriverError> {
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }
        let lifecycle_guard = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(DriverError::Cancelled),
            guard = self.lifecycle.lock() => guard,
        };
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }

        // Never start a mutating simulator operation while an earlier aborted state wipe is still
        // winding down.
        self.await_pending_wipe_locked().await;
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }

        // If an active session was marked dirty by a dropped in-flight RPC, tear it down first so
        // we never treat a dirty runner as healthy ownership.
        let existing_handle = self.lock_session().handle();
        if let Some(handle) = existing_handle
            && handle.is_dirty()
        {
            self.invalidate_session_locked(handle.client.base_url())
                .await;
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
        }

        let already_owned = self.lock_session().handle().is_some();
        let temp_lock = if already_owned {
            None
        } else {
            Some(Arc::new(
                self.acquire_temp_simulator_lock()
                    .map_err(|err| map_ios_err(operation, err))?,
            ))
        };
        Ok(SimulatorOwnershipLease {
            temp_lock,
            _lifecycle: lifecycle_guard,
        })
    }

    /// Executes a path-confined `clear_state` wipe while holding `lease`, keeping simulator
    /// ownership alive across the blocking worker thread even if the async future is dropped
    /// mid-wipe (e.g., by an outer timeout).
    async fn run_clear_state_with_lease(
        &self,
        bundle_id: &str,
        lease: &SimulatorOwnershipLease<'_>,
        cancel: &CancellationToken,
    ) -> std::result::Result<(), DriverError> {
        let targets = self
            .simctl
            .prepare_clear_state_targets(&self.udid, bundle_id, cancel)
            .await
            .map_err(|err| map_ios_err("clear_state", err))?;

        let cancel_flag = Arc::new(AtomicBool::new(false));
        let flag_for_worker = Arc::clone(&cancel_flag);
        // Keep the temporary OS simulator lock alive inside the blocking worker thread until the
        // wipe actually exits, even if the async future is dropped mid-wipe.
        let worker_lock = lease.temp_lock.clone();
        #[cfg(test)]
        let test_hook = self.on_clear_state_worker.clone();

        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::task::spawn_blocking(move || {
            let _lock_guard = worker_lock;
            #[cfg(test)]
            if let Some(hook) = test_hook {
                (hook.0)(&flag_for_worker);
            }
            let res = clear_state_targets_sync(&targets, &flag_for_worker);
            let _ = tx.send(res);
        });

        let mut wipe_guard = ActiveWipeGuard {
            cancel_flag,
            slot: &self.pending_wipe,
            handle: Some(handle),
        };

        let cancelled = {
            let handle_ref = wipe_guard.handle.as_mut().expect("active wipe handle");
            tokio::select! {
                biased;
                _ = cancel.cancelled() => true,
                res = handle_ref => {
                    wipe_guard.handle = None;
                    res.map_err(|err| DriverError::Io(std::io::Error::other(err.to_string())))?;
                    false
                }
            }
        };

        if cancelled {
            wipe_guard.cancel_flag.store(true, Ordering::SeqCst);
            if let Some(h) = wipe_guard.handle.as_mut() {
                let _ = h.await;
            }
            wipe_guard.handle = None;
            return Err(DriverError::Cancelled);
        }

        let worker_res = rx
            .await
            .map_err(|_| DriverError::Io(std::io::Error::other("clear_state worker dropped")))?;
        worker_res.map_err(|err| map_ios_err("clear_state", err))
    }

    fn keyboard_dismiss_verify_timeout(&self) -> Duration {
        #[cfg(test)]
        {
            self.keyboard_dismiss_verify_timeout
        }
        #[cfg(not(test))]
        {
            KEYBOARD_DISMISS_VERIFY_TIMEOUT
        }
    }

    async fn call_agent<T, Fut>(
        &self,
        handle: &SessionHandle,
        operation: &str,
        fut: Fut,
        cancel: &CancellationToken,
    ) -> std::result::Result<T, DriverError>
    where
        Fut: Future<Output = Result<T>>,
    {
        self.call_agent_with_timeout(handle, operation, fut, None, cancel)
            .await
    }

    async fn call_agent_with_timeout<T, Fut>(
        &self,
        handle: &SessionHandle,
        operation: &str,
        fut: Fut,
        local_timeout: Option<Duration>,
        cancel: &CancellationToken,
    ) -> std::result::Result<T, DriverError>
    where
        Fut: Future<Output = Result<T>>,
    {
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }
        let started = Instant::now();
        let mut rpc_guard = InFlightRpcGuard::new(Arc::clone(&handle.dirty));
        let outcome = match local_timeout {
            None => tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                res = fut => Some(Ok(res)),
            },
            Some(limit) => tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                timed = tokio::time::timeout(limit, fut) => Some(timed),
            },
        };

        // If cancellation or a local deadline interrupted an in-flight XCTest RPC, immediately
        // mark dirty and quiesce/invalidate the runner before returning so the next caller never
        // hits a stale `busyLock` 409 or suffers late UI mutations from the interrupted request.
        let res = match outcome {
            None => {
                handle.mark_dirty();
                self.invalidate_session_if_matching(handle.client.base_url())
                    .await;
                return Err(DriverError::Cancelled);
            }
            Some(Err(_)) => {
                handle.mark_dirty();
                self.invalidate_session_if_matching(handle.client.base_url())
                    .await;
                return Err(DriverError::Transport {
                    operation: operation.to_string(),
                    detail: format!(
                        "runner RPC `{operation}` exceeded local {}ms deadline; invalidated runner session",
                        local_timeout.map_or(0, |d| d.as_millis() as u64)
                    ),
                    transient: true,
                });
            }
            Some(Ok(res)) => res,
        };

        match res {
            Ok(val) => {
                rpc_guard.disarm();
                Ok(val)
            }
            Err(err) => {
                if is_dead_session_error(&err) {
                    handle.mark_dirty();
                    self.invalidate_session_if_matching(handle.client.base_url())
                        .await;
                    return Err(DriverError::Transport {
                        operation: operation.to_string(),
                        detail: err.to_string(),
                        transient: false,
                    });
                }
                if is_reqwest_timeout_error(&err) {
                    handle.mark_dirty();
                    self.invalidate_session_if_matching(handle.client.base_url())
                        .await;
                } else {
                    rpc_guard.disarm();
                }
                Err(map_agent_err(
                    operation,
                    started.elapsed().as_millis() as u64,
                    err,
                ))
            }
        }
    }

    async fn send_key_batches(
        &self,
        handle: &SessionHandle,
        app_id: &str,
        key: &str,
        total_count: u32,
        cancel: &CancellationToken,
    ) -> std::result::Result<(), DriverError> {
        let mut remaining = total_count;
        while remaining > 0 {
            let batch = remaining.min(MAX_KEY_BATCH);
            let req = KeyRequest {
                app_id: app_id.to_string(),
                key: key.to_string(),
                modifiers: Vec::new(),
                count: Some(batch),
            };
            let resp = self
                .call_agent(handle, "press_key", handle.client.press_key(&req), cancel)
                .await?;
            if !resp.ok {
                return Err(DriverError::Transport {
                    operation: "press_key".to_string(),
                    detail: format!("agent returned ok=false for key `{key}`"),
                    transient: false,
                });
            }
            remaining -= batch;
        }
        Ok(())
    }

    async fn observe_post_action(
        &self,
        req: &ActPayload,
        started: Instant,
        cancel: &CancellationToken,
    ) -> std::result::Result<ActOutcome, DriverError> {
        if let Some(delay_ms) = req.post_action_delay_ms
            && delay_ms > 0
        {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
            }
        }

        let (hierarchy, hierarchy_error) = if req.return_hierarchy {
            match self
                .hierarchy(
                    &req.app_id,
                    req.include_system_ui,
                    req.known_hash.as_deref(),
                    cancel,
                )
                .await
            {
                Ok(h) => (Some(h), None),
                Err(DriverError::Cancelled) => return Err(DriverError::Cancelled),
                Err(err) => (None, Some(err.to_string())),
            }
        } else {
            (None, None)
        };

        Ok(ActOutcome {
            action_ok: true,
            elapsed_ms: started.elapsed().as_millis() as u64,
            hierarchy,
            hierarchy_error,
        })
    }

    async fn scan_crash_immediate(
        &self,
        app_id: &str,
        since_ms: u64,
        until_ms: u64,
    ) -> std::result::Result<Option<DriverCrashSignal>, DriverError> {
        let Some(crash_dir) = self.config.crash_report_dir.clone() else {
            return Ok(None);
        };
        let opts = CrashSearchOptions {
            directory: crash_dir,
            app_id: app_id.to_string(),
            device_udid: Some(self.udid.clone()),
            since_ms,
            until_ms: until_ms.max(since_ms),
        };
        tokio::task::spawn_blocking(move || find_crash_report(&opts))
            .await
            .map_err(|err| DriverError::Io(std::io::Error::other(err.to_string())))?
            .map_err(|err| map_ios_err("check_app_crash", err))
    }

    async fn scan_crash_with_grace(
        &self,
        app_id: &str,
        since_ms: u64,
        until_ms: u64,
        grace: Duration,
        cancel: &CancellationToken,
    ) -> std::result::Result<Option<DriverCrashSignal>, DriverError> {
        let Some(crash_dir) = self.config.crash_report_dir.clone() else {
            return Ok(None);
        };
        let opts = CrashSearchOptions {
            directory: crash_dir,
            app_id: app_id.to_string(),
            device_udid: Some(self.udid.clone()),
            since_ms,
            until_ms: until_ms.max(since_ms),
        };
        find_crash_report_with_grace(&opts, grace, cancel)
            .await
            .map_err(|err| map_ios_err("check_app_crash", err))
    }

    async fn server_wait_for_selector(
        &self,
        req: &WaitSelectorRequest,
        cancel: &CancellationToken,
    ) -> std::result::Result<WaitSelectorOutcome, DriverError> {
        let handle = self.get_handle_or_start(cancel).await?;
        let timeout_ms = u64::try_from(req.timeout.as_millis()).unwrap_or(60_000);
        let wire_req = WaitForSelectorRequest {
            app_id: req.app_id.clone(),
            identifier: req.selector.id.clone(),
            text: req.selector.text.clone(),
            text_match: req.selector.text.as_ref().map(|_| TextMatchMode::Exact),
            not_visible: Some(req.not_visible),
            wait_stable: Some(req.wait_stable),
            stable_polls: req.wait_stable.then_some(req.stable_polls as u32),
            timeout_ms: Some(timeout_ms),
            include_system_ui: Some(req.include_system_ui),
            return_hierarchy: Some(req.return_hierarchy),
        };

        let rpc_timeout = if timeout_ms > 0 {
            Duration::from_millis(timeout_ms.saturating_add(SERVER_WAIT_GRACE_MS))
        } else {
            self.config
                .request_timeout
                .min(ZERO_TIMEOUT_SINGLE_PROBE_CAP)
        };

        let resp = self
            .call_agent_with_timeout(
                &handle,
                "wait_for_selector",
                handle.client.wait_for_selector(&wire_req),
                Some(rpc_timeout),
                cancel,
            )
            .await?;

        let ui_hierarchy = resp.hierarchy.as_ref().map(hierarchy_response_to_ui);
        let matched_node = resp.node.as_ref().map(hierarchy_node_to_ui_node);
        // Prefer building VisibleContext from the raw `ui_hierarchy` whenever present so exact
        // redaction always runs on raw node strings before whitespace collapsing; fall back to
        // `resp.visible_sample` (which Swift also sends raw/uncollapsed) only when no hierarchy
        // was returned.
        let visible_context = if resp.matched {
            VisibleContext::default()
        } else if let Some(ref h) = ui_hierarchy {
            build_visible_context(
                &h.nodes,
                req.redactor.as_ref(),
                req.provider_redactor.as_ref(),
                VisibleContext::DEFAULT_LIMIT,
            )
        } else if !resp.visible_sample.is_empty() {
            scrub_visible_sample(
                &resp.visible_sample,
                req.redactor.as_ref(),
                req.provider_redactor.as_ref(),
            )
        } else {
            VisibleContext::default()
        };

        let obtained_fresh =
            resp.matched || !resp.hierarchy_hash.is_empty() || ui_hierarchy.is_some();
        if !resp.matched && !obtained_fresh {
            return Err(DriverError::Transport {
                operation: "wait_for_selector".to_string(),
                detail: format!(
                    "server-side waitForSelector completed {} poll(s) without capturing a readable UI hierarchy snapshot",
                    resp.polls
                ),
                transient: false,
            });
        }
        let settled_early = !req.timeout.is_zero()
            && resp.matched
            && Duration::from_millis(resp.elapsed_ms) < req.timeout;

        Ok(WaitSelectorOutcome {
            matched: resp.matched,
            elapsed_ms: resp.elapsed_ms,
            polls: resp.polls,
            settled_early,
            obtained_fresh_observation: obtained_fresh,
            hierarchy_hash: resp.hierarchy_hash,
            matched_node,
            visible_context,
            hierarchy: if req.return_hierarchy {
                ui_hierarchy
            } else {
                None
            },
        })
    }
}

impl PlatformDriver for IosDriver {
    fn platform(&self) -> &'static str {
        "ios"
    }

    fn capabilities(&self) -> &DriverCapabilities {
        &self.capabilities
    }

    fn ensure_session<'a>(&'a self, cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let _ = self.ensure_session_handle(cancel).await?;
            Ok(())
        })
    }

    fn launch_app<'a>(
        &'a self,
        app_id: &'a str,
        opts: &'a LaunchOptions,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }

            // Preserve Maestro's default `all: allow` permission grant when `permissions` is
            // omitted (`None`); an explicit empty map (`Some({})`) applies no permission changes.
            let default_permissions: BTreeMap<String, String>;
            let effective_permissions: &BTreeMap<String, String> = match &opts.permissions {
                None => {
                    default_permissions =
                        BTreeMap::from([("all".to_string(), "allow".to_string())]);
                    &default_permissions
                }
                Some(explicit) => explicit,
            };

            // Validate the entire launch specification (permissions, bundle ID, argument keys/values,
            // and env keys/values) BEFORE touching runner/simulator state.
            for (service, action) in effective_permissions {
                map_privacy_service(service)
                    .map_err(|err| DriverError::InvalidArgument(err.to_string()))?;
                map_privacy_action(action)
                    .map_err(|err| DriverError::InvalidArgument(err.to_string()))?;
            }
            let mut formatted_args = BTreeMap::new();
            for (key, value) in &opts.arguments {
                let formatted = format_launch_argument_value(value)
                    .map_err(|err| DriverError::InvalidArgument(err.to_string()))?;
                formatted_args.insert(key.clone(), formatted);
            }
            let bundle_id = validate_launch_inputs(app_id, &formatted_args, &opts.env)
                .map_err(|err| DriverError::InvalidArgument(err.to_string()))?;

            let handle = self.ensure_session_handle(cancel).await?;

            // Hold simulator ownership across all pre-launch and `simctl launch` mutations so a
            // concurrent `shutdown()` cannot release the simulator lock mid-launch.
            let ownership_lease = self
                .acquire_simulator_ownership("launch_app", cancel)
                .await?;

            if opts.clear_state {
                // Keep simulator ownership bound to the blocking wipe worker even if the caller
                // drops this future mid-wipe.
                self.run_clear_state_with_lease(&bundle_id, &ownership_lease, cancel)
                    .await?;
            } else if opts.stop_app {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    res = self.simctl.terminate_app(&self.udid, &bundle_id) => {
                        res.map_err(|err| map_ios_err("terminate_app", err))?;
                    }
                }
            }

            if opts.clear_keychain {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    res = self.simctl.clear_keychain(&self.udid) => {
                        res.map_err(|err| map_ios_err("clear_keychain", err))?;
                    }
                }
            }

            if !effective_permissions.is_empty() {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    res = self.simctl.set_permissions(&self.udid, &bundle_id, effective_permissions) => {
                        res.map_err(|err| map_ios_err("set_permissions", err))?;
                    }
                }
            }

            let launch_started_epoch_ms = now_epoch_ms();
            let deadline = tokio::time::Instant::now() + self.config.app_launch_timeout;
            let launch_timed = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                timed = tokio::time::timeout_at(
                    deadline,
                    self.simctl.launch_app(&self.udid, &bundle_id, &formatted_args, &opts.env),
                ) => timed,
            };
            // Release the lifecycle ownership lease before entering `/warmup` so cancel/timeout
            // invalidation inside the warmup loop can acquire `self.lifecycle` cleanly.
            drop(ownership_lease);

            match launch_timed {
                Ok(Ok(_pid)) => {}
                Ok(Err(err)) => {
                    return Err(match err {
                        IosDriverError::InvalidArgument(_)
                        | IosDriverError::InvalidBundleId(_)
                        | IosDriverError::InvalidUdid(_) => {
                            DriverError::InvalidArgument(err.to_string())
                        }
                        other => DriverError::LaunchFailed {
                            app_id: bundle_id.clone(),
                            detail: other.to_string(),
                        },
                    });
                }
                Err(_) => {
                    if let Some(sig) = self
                        .scan_crash_with_grace(
                            &bundle_id,
                            launch_started_epoch_ms,
                            now_epoch_ms(),
                            self.config.crash_report_grace,
                            cancel,
                        )
                        .await?
                    {
                        return Err(DriverError::AppCrashed(sig));
                    }
                    return Err(DriverError::LaunchFailed {
                        app_id: bundle_id,
                        detail: format!(
                            "timed out after {}ms waiting for `xcrun simctl launch`",
                            self.config.app_launch_timeout.as_millis()
                        ),
                    });
                }
            }

            let mut last_detail = "app did not reach running_foreground".to_string();

            loop {
                if cancel.is_cancelled() {
                    return Err(DriverError::Cancelled);
                }

                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                // Pass the full remaining launch budget (up to Swift's 60 s `/warmup` cap) so
                // slow simulator AX snapshots within `app_launch_timeout` are never discarded
                // by an artificial 1.8 s sub-chunk timeout.
                let request_ms = u64::try_from(remaining.as_millis())
                    .unwrap_or(MAX_SERVER_WARMUP_MS)
                    .min(MAX_SERVER_WARMUP_MS);
                let warmup_req = WarmupRequest {
                    app_id: bundle_id.clone(),
                    timeout_ms: Some(request_ms),
                    include_system_ui: Some(false),
                    return_hierarchy: Some(false),
                };

                let mut rpc_guard = InFlightRpcGuard::new(Arc::clone(&handle.dirty));
                let outcome = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => None,
                    timed = tokio::time::timeout(remaining, handle.client.warmup(&warmup_req)) => Some(timed),
                };

                let res = match outcome {
                    None => {
                        handle.mark_dirty();
                        self.invalidate_session_if_matching(handle.client.base_url())
                            .await;
                        return Err(DriverError::Cancelled);
                    }
                    Some(Err(_)) => {
                        handle.mark_dirty();
                        self.invalidate_session_if_matching(handle.client.base_url())
                            .await;
                        last_detail = format!(
                            "warmup HTTP request exceeded remaining launch budget ({}ms)",
                            remaining.as_millis()
                        );
                        break;
                    }
                    Some(Ok(res)) => res,
                };

                match res {
                    Ok(resp) if resp.ok && resp.state == "running_foreground" => {
                        rpc_guard.disarm();
                        return Ok(());
                    }
                    Ok(resp) if resp.state == "not_running" => {
                        rpc_guard.disarm();
                        if let Some(sig) = self
                            .scan_crash_with_grace(
                                &bundle_id,
                                launch_started_epoch_ms,
                                now_epoch_ms(),
                                self.config.crash_report_grace,
                                cancel,
                            )
                            .await?
                        {
                            return Err(DriverError::AppCrashed(sig));
                        }
                        return Err(DriverError::LaunchFailed {
                            app_id: bundle_id,
                            detail: format!(
                                "app exited before reaching foreground (state={})",
                                resp.state
                            ),
                        });
                    }
                    Ok(resp) => {
                        rpc_guard.disarm();
                        last_detail = format!("app state was `{}` (ok={})", resp.state, resp.ok);
                        let sleep_for = Duration::from_millis(50)
                            .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
                        if sleep_for.is_zero() {
                            break;
                        }
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                            _ = tokio::time::sleep(sleep_for) => {}
                        }
                    }
                    Err(IosDriverError::AgentHttpStatus { status: 409, .. }) => {
                        rpc_guard.disarm();
                        let sleep_for = Duration::from_millis(50)
                            .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
                        if !sleep_for.is_zero() {
                            tokio::select! {
                                biased;
                                _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                                _ = tokio::time::sleep(sleep_for) => {}
                            }
                        }
                        last_detail = "companion runner busy (HTTP 409) during warmup".to_string();
                    }
                    Err(IosDriverError::AgentHttpStatus {
                        status: 500, body, ..
                    }) => {
                        rpc_guard.disarm();
                        if body.contains("not_running")
                            && let Some(sig) = self
                                .scan_crash_immediate(
                                    &bundle_id,
                                    launch_started_epoch_ms,
                                    now_epoch_ms(),
                                )
                                .await?
                        {
                            return Err(DriverError::AppCrashed(sig));
                        }
                        last_detail = body;
                        let sleep_for = Duration::from_millis(50)
                            .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
                        if sleep_for.is_zero() {
                            break;
                        }
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                            _ = tokio::time::sleep(sleep_for) => {}
                        }
                    }
                    Err(err) => {
                        if is_dead_session_error(&err) || is_reqwest_timeout_error(&err) {
                            handle.mark_dirty();
                            self.invalidate_session_if_matching(handle.client.base_url())
                                .await;
                            return Err(DriverError::Transport {
                                operation: "launch_app_warmup".to_string(),
                                detail: err.to_string(),
                                transient: false,
                            });
                        }
                        rpc_guard.disarm();
                        return Err(DriverError::LaunchFailed {
                            app_id: bundle_id,
                            detail: err.to_string(),
                        });
                    }
                }
            }

            if let Some(sig) = self
                .scan_crash_with_grace(
                    &bundle_id,
                    launch_started_epoch_ms,
                    now_epoch_ms(),
                    self.config.crash_report_grace,
                    cancel,
                )
                .await?
            {
                return Err(DriverError::AppCrashed(sig));
            }

            Err(DriverError::LaunchFailed {
                app_id: bundle_id,
                detail: format!(
                    "timed out after {}ms waiting for app warmup: {last_detail}",
                    self.config.app_launch_timeout.as_millis()
                ),
            })
        })
    }

    fn stop_app<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let _lease = self.acquire_simulator_ownership("stop_app", cancel).await?;
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(DriverError::Cancelled),
                res = self.simctl.terminate_app(&self.udid, app_id) => {
                    res.map_err(|err| map_ios_err("stop_app", err))
                }
            }
        })
    }

    fn kill_app<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let _lease = self.acquire_simulator_ownership("kill_app", cancel).await?;
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(DriverError::Cancelled),
                res = self.simctl.terminate_app(&self.udid, app_id) => {
                    res.map_err(|err| map_ios_err("kill_app", err))
                }
            }
        })
    }

    fn clear_state<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let lease = self
                .acquire_simulator_ownership("clear_state", cancel)
                .await?;
            // Keep simulator ownership bound to the blocking worker until it exits, even if this
            // future is dropped early by an outer timeout.
            self.run_clear_state_with_lease(app_id, &lease, cancel)
                .await
        })
    }

    fn clear_keychain<'a>(&'a self, cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let _lease = self
                .acquire_simulator_ownership("clear_keychain", cancel)
                .await?;
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(DriverError::Cancelled),
                res = self.simctl.clear_keychain(&self.udid) => {
                    res.map_err(|err| map_ios_err("clear_keychain", err))
                }
            }
        })
    }

    fn open_link<'a>(
        &'a self,
        _app_id: Option<&'a str>,
        url: &'a str,
        opts: &'a OpenLinkOptions,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let validated_url = validate_open_link_url(url)
                .map_err(|err| DriverError::InvalidArgument(err.to_string()))?;

            if opts.auto_verify == Some(true) {
                return Err(DriverError::InvalidArgument(
                    "openLink `autoVerify` is Android-only and not supported on iOS".to_string(),
                ));
            }
            if opts.browser == Some(true) {
                let parsed = reqwest::Url::parse(&validated_url)
                    .map_err(|err| DriverError::InvalidArgument(err.to_string()))?;
                let scheme = parsed.scheme();
                if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
                    return Err(DriverError::InvalidArgument(format!(
                        "openLink with `browser: true` requires an http or https URL on iOS, got `{scheme}`"
                    )));
                }
            }

            let _lease = self
                .acquire_simulator_ownership("open_link", cancel)
                .await?;

            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(DriverError::Cancelled),
                res = self.simctl.open_link(&self.udid, &validated_url) => {
                    res.map_err(|err| match err {
                        IosDriverError::InvalidArgument(_) | IosDriverError::InvalidUdid(_) => {
                            DriverError::InvalidArgument(err.to_string())
                        }
                        other => DriverError::NavigationFailed {
                            target: validated_url,
                            detail: other.to_string(),
                        },
                    })
                }
            }
        })
    }

    fn add_media<'a>(
        &'a self,
        paths: &'a [PathBuf],
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let _lease = self
                .acquire_simulator_ownership("add_media", cancel)
                .await?;
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(DriverError::Cancelled),
                res = self.simctl.add_media(&self.udid, paths) => {
                    res.map_err(|err| map_ios_err("add_media", err))
                }
            }
        })
    }

    fn hierarchy<'a>(
        &'a self,
        app_id: &'a str,
        include_system_ui: bool,
        known_hash: Option<&'a str>,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, UiHierarchy> {
        Box::pin(async move {
            let handle = self.get_handle_or_start(cancel).await?;
            let resp = self
                .call_agent(
                    &handle,
                    "hierarchy",
                    handle
                        .client
                        .hierarchy_with_known_hash(app_id, include_system_ui, known_hash),
                    cancel,
                )
                .await?;
            Ok(hierarchy_response_to_ui(&resp))
        })
    }

    fn tap_point<'a>(
        &'a self,
        app_id: &'a str,
        point: UiPoint,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            validate_point(point)?;
            let handle = self.get_handle_or_start(cancel).await?;
            let req = TapRequest {
                app_id: app_id.to_string(),
                x: point.x,
                y: point.y,
                count: Some(1),
                duration_ms: None,
            };
            let resp = self
                .call_agent(&handle, "tap_point", handle.client.tap(&req), cancel)
                .await?;
            if !resp.ok {
                return Err(DriverError::Transport {
                    operation: "tap_point".to_string(),
                    detail: "agent returned ok=false".to_string(),
                    transient: false,
                });
            }
            Ok(())
        })
    }

    fn long_press_point<'a>(
        &'a self,
        app_id: &'a str,
        point: UiPoint,
        duration_ms: Option<u64>,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            validate_point(point)?;
            let handle = self.get_handle_or_start(cancel).await?;
            let req = TapRequest {
                app_id: app_id.to_string(),
                x: point.x,
                y: point.y,
                count: Some(1),
                duration_ms: Some(duration_ms.unwrap_or(DEFAULT_LONG_PRESS_MS).max(1)),
            };
            let resp = self
                .call_agent(&handle, "long_press_point", handle.client.tap(&req), cancel)
                .await?;
            if !resp.ok {
                return Err(DriverError::Transport {
                    operation: "long_press_point".to_string(),
                    detail: "agent returned ok=false".to_string(),
                    transient: false,
                });
            }
            Ok(())
        })
    }

    fn double_tap_point<'a>(
        &'a self,
        app_id: &'a str,
        point: UiPoint,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            validate_point(point)?;
            let handle = self.get_handle_or_start(cancel).await?;
            let req = TapRequest {
                app_id: app_id.to_string(),
                x: point.x,
                y: point.y,
                count: Some(2),
                duration_ms: None,
            };
            let resp = self
                .call_agent(&handle, "double_tap_point", handle.client.tap(&req), cancel)
                .await?;
            if !resp.ok {
                return Err(DriverError::Transport {
                    operation: "double_tap_point".to_string(),
                    detail: "agent returned ok=false".to_string(),
                    transient: false,
                });
            }
            Ok(())
        })
    }

    fn swipe<'a>(
        &'a self,
        params: &'a SwipeParams,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            validate_point(params.start)?;
            validate_point(params.end)?;
            let handle = self.get_handle_or_start(cancel).await?;
            let req = SwipeRequest {
                app_id: params.app_id.clone(),
                start_x: params.start.x,
                start_y: params.start.y,
                end_x: params.end.x,
                end_y: params.end.y,
                duration_ms: Some(params.duration_ms),
                hold_duration_ms: params.hold_duration_ms,
            };
            let resp = self
                .call_agent(&handle, "swipe", handle.client.swipe(&req), cancel)
                .await?;
            if !resp.ok {
                return Err(DriverError::Transport {
                    operation: "swipe".to_string(),
                    detail: "agent returned ok=false".to_string(),
                    transient: false,
                });
            }
            Ok(())
        })
    }

    fn input_text<'a>(
        &'a self,
        app_id: &'a str,
        text: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let handle = self.get_handle_or_start(cancel).await?;
            let req = TypeRequest {
                app_id: app_id.to_string(),
                text: text.to_string(),
                settle_keyboard: Some(true),
            };
            let resp = self
                .call_agent(&handle, "input_text", handle.client.type_text(&req), cancel)
                .await?;
            if !resp.ok {
                return Err(DriverError::Transport {
                    operation: "input_text".to_string(),
                    detail: "agent returned ok=false".to_string(),
                    transient: false,
                });
            }
            Ok(())
        })
    }

    fn erase_text<'a>(
        &'a self,
        app_id: &'a str,
        count: u32,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            if count == 0 {
                return Ok(());
            }
            let handle = self.get_handle_or_start(cancel).await?;
            self.send_key_batches(&handle, app_id, "delete", count, cancel)
                .await
        })
    }

    fn press_key<'a>(
        &'a self,
        app_id: &'a str,
        key: &'a KeyPress,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let normalized = validate_key_press(&key.key, &key.modifiers)?;
            if key.count == 0 {
                return Ok(());
            }
            let handle = self.get_handle_or_start(cancel).await?;
            self.send_key_batches(&handle, app_id, &normalized, key.count, cancel)
                .await
        })
    }

    fn hide_keyboard<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let tree = self.hierarchy(app_id, true, None, cancel).await?;
            if !has_visible_keyboard(&tree.nodes) {
                return Ok(());
            }
            let Some(dismiss_point) = find_keyboard_dismiss_target(&tree.nodes) else {
                return Err(DriverError::Transport {
                    operation: "hide_keyboard".to_string(),
                    detail: "visible iOS keyboard has no safe non-submitting dismissal button; refusing to press Return".to_string(),
                    transient: false,
                });
            };
            self.tap_point(app_id, dismiss_point, cancel).await?;

            // Re-check hierarchy to verify the keyboard actually dismissed (allowing a brief
            // animation settle window before failing closed).
            let deadline = tokio::time::Instant::now() + self.keyboard_dismiss_verify_timeout();
            let mut delay = Duration::from_millis(40);
            loop {
                let post = self.hierarchy(app_id, true, None, cancel).await?;
                if !has_visible_keyboard(&post.nodes) {
                    return Ok(());
                }
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(DriverError::Transport {
                        operation: "hide_keyboard".to_string(),
                        detail: "tapped keyboard dismissal button, but the iOS keyboard remained visible".to_string(),
                        transient: false,
                    });
                }
                let sleep_for = delay.min(remaining);
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    _ = tokio::time::sleep(sleep_for) => {}
                }
                delay = (delay * 2).min(Duration::from_millis(120));
            }
        })
    }

    fn back<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Err(unsupported_system_back()) })
    }

    fn take_screenshot<'a>(
        &'a self,
        _app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Vec<u8>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let guard = TempFileGuard::new("screenshot", "png");
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(DriverError::Cancelled),
                res = self.simctl.screenshot(&self.udid, guard.path()) => {
                    res.map_err(|err| map_ios_err("take_screenshot", err))
                }
            }
        })
    }

    fn check_app_crash<'a>(
        &'a self,
        app_id: &'a str,
        since_epoch_ms: u64,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Option<DriverCrashSignal>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let now_ms = now_epoch_ms();

            // 1. Fast immediate scan without sleeping.
            if let Some(sig) = self
                .scan_crash_immediate(app_id, since_epoch_ms, now_ms)
                .await?
            {
                return Ok(Some(sig));
            }

            // 2. If a clean runner session is live, probe `/appState`: when the app is still
            //    alive, skip the grace window completely (0 ms penalty on normal assertion
            //    failures). If the runner was marked dirty by a dropped RPC, tear it down first.
            let existing_handle = self.lock_session().handle();
            if let Some(handle) = existing_handle {
                if handle.is_dirty() {
                    self.invalidate_session_if_matching(handle.client.base_url())
                        .await;
                } else {
                    let probe_timeout = Duration::from_secs(2).min(self.config.request_timeout);
                    let mut rpc_guard = InFlightRpcGuard::new(Arc::clone(&handle.dirty));
                    let probe_outcome = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => None,
                        timed = tokio::time::timeout(probe_timeout, handle.client.app_state(app_id)) => Some(timed),
                    };
                    match probe_outcome {
                        None => {
                            handle.mark_dirty();
                            self.invalidate_session_if_matching(handle.client.base_url())
                                .await;
                            return Err(DriverError::Cancelled);
                        }
                        Some(Err(_)) => {
                            handle.mark_dirty();
                            self.invalidate_session_if_matching(handle.client.base_url())
                                .await;
                        }
                        Some(Ok(Ok(state))) if is_app_alive_state(&state.state) => {
                            rpc_guard.disarm();
                            return Ok(None);
                        }
                        Some(Ok(Err(ref err)))
                            if is_dead_session_error(err) || is_reqwest_timeout_error(err) =>
                        {
                            handle.mark_dirty();
                            self.invalidate_session_if_matching(handle.client.base_url())
                                .await;
                        }
                        Some(Ok(_)) => {
                            rpc_guard.disarm();
                        }
                    }
                }
            }

            // 3. App is `not_running`/`unknown` (or `/appState` could not confirm liveness):
            //    capture a fresh timestamp AFTER the `/appState` probe so reports written
            //    throughout the full `crash_report_grace` period are included.
            let grace_start_ms = now_epoch_ms();
            self.scan_crash_with_grace(
                app_id,
                since_epoch_ms,
                grace_start_ms,
                self.config.crash_report_grace,
                cancel,
            )
            .await
        })
    }

    fn shutdown<'a>(&'a self) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let _lifecycle = self.lifecycle.lock().await;
            // Wait for any aborted blocking state-wipe worker to exit BEFORE tearing down the
            // runner session and releasing the simulator advisory lock.
            self.await_pending_wipe_locked().await;
            let taken = {
                let mut guard = self.lock_session();
                self.runner_pid.store(0, Ordering::SeqCst);
                if matches!(*guard, SessionState::Detached) {
                    SessionState::Detached
                } else {
                    std::mem::replace(&mut *guard, SessionState::Stopping)
                }
            };
            let _reset_guard = ResetStoppingGuard {
                session: &self.session,
            };
            teardown_session(taken)
                .await
                .map_err(|err| map_ios_err("shutdown", err))
        })
    }

    fn act_and_observe<'a>(
        &'a self,
        req: &'a ActPayload,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ActOutcome> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let started = Instant::now();

            // `DejaXCTestRunner`'s `/act` clamps `postActionDelayMs` at 5,000 ms. For longer
            // delays, run the primitive action, wait the full duration locally (racing `cancel`),
            // and then capture the post-action hierarchy so long delays are never silently truncated.
            if req
                .post_action_delay_ms
                .is_some_and(|ms| ms > self.max_server_post_action_delay_ms())
            {
                match &req.action {
                    ActAction::Tap { x, y } => {
                        self.tap_point(&req.app_id, UiPoint::new(*x, *y), cancel)
                            .await?;
                    }
                    ActAction::LongPress { x, y, duration_ms } => {
                        self.long_press_point(
                            &req.app_id,
                            UiPoint::new(*x, *y),
                            *duration_ms,
                            cancel,
                        )
                        .await?;
                    }
                    ActAction::DoubleTap { x, y } => {
                        self.double_tap_point(&req.app_id, UiPoint::new(*x, *y), cancel)
                            .await?;
                    }
                    ActAction::Swipe {
                        start,
                        end,
                        duration_ms,
                        hold_duration_ms,
                    } => {
                        let params = SwipeParams {
                            app_id: req.app_id.clone(),
                            start: *start,
                            end: *end,
                            duration_ms: *duration_ms,
                            hold_duration_ms: *hold_duration_ms,
                        };
                        self.swipe(&params, cancel).await?;
                    }
                    ActAction::InputText { text } => {
                        self.input_text(&req.app_id, text, cancel).await?;
                    }
                    ActAction::EraseText { count } => {
                        self.erase_text(&req.app_id, *count, cancel).await?;
                    }
                    ActAction::PressKey {
                        key,
                        modifiers,
                        count,
                    } => {
                        let kp = KeyPress {
                            key: key.clone(),
                            modifiers: modifiers.clone(),
                            count: *count,
                        };
                        self.press_key(&req.app_id, &kp, cancel).await?;
                    }
                    ActAction::HideKeyboard => {
                        self.hide_keyboard(&req.app_id, cancel).await?;
                    }
                    ActAction::Back => return Err(unsupported_system_back()),
                }
                return self.observe_post_action(req, started, cancel).await;
            }

            let act_kind = match &req.action {
                ActAction::Back => return Err(unsupported_system_back()),
                ActAction::HideKeyboard => {
                    self.hide_keyboard(&req.app_id, cancel).await?;
                    return self.observe_post_action(req, started, cancel).await;
                }
                ActAction::Tap { x, y } => {
                    validate_point(UiPoint::new(*x, *y))?;
                    ActKind::Tap {
                        x: *x,
                        y: *y,
                        count: Some(1),
                        duration_ms: None,
                    }
                }
                ActAction::LongPress { x, y, duration_ms } => {
                    validate_point(UiPoint::new(*x, *y))?;
                    ActKind::Tap {
                        x: *x,
                        y: *y,
                        count: Some(1),
                        duration_ms: Some(duration_ms.unwrap_or(DEFAULT_LONG_PRESS_MS).max(1)),
                    }
                }
                ActAction::DoubleTap { x, y } => {
                    validate_point(UiPoint::new(*x, *y))?;
                    ActKind::Tap {
                        x: *x,
                        y: *y,
                        count: Some(2),
                        duration_ms: None,
                    }
                }
                ActAction::Swipe {
                    start,
                    end,
                    duration_ms,
                    hold_duration_ms,
                } => {
                    validate_point(*start)?;
                    validate_point(*end)?;
                    ActKind::Swipe {
                        start_x: start.x,
                        start_y: start.y,
                        end_x: end.x,
                        end_y: end.y,
                        duration_ms: Some(*duration_ms),
                        hold_duration_ms: *hold_duration_ms,
                    }
                }
                ActAction::InputText { text } => ActKind::Type {
                    text: text.clone(),
                    settle_keyboard: Some(true),
                },
                ActAction::EraseText { count } => {
                    if *count == 0 {
                        return self.observe_post_action(req, started, cancel).await;
                    }
                    let handle = self.get_handle_or_start(cancel).await?;
                    let mut remaining = *count;
                    if remaining > MAX_KEY_BATCH {
                        let pre_count = remaining - ((remaining - 1) % MAX_KEY_BATCH + 1);
                        self.send_key_batches(&handle, &req.app_id, "delete", pre_count, cancel)
                            .await?;
                        remaining -= pre_count;
                    }
                    ActKind::Key {
                        key: "delete".to_string(),
                        modifiers: Vec::new(),
                        count: Some(remaining),
                    }
                }
                ActAction::PressKey {
                    key,
                    modifiers,
                    count,
                } => {
                    let normalized = validate_key_press(key, modifiers)?;
                    if *count == 0 {
                        return self.observe_post_action(req, started, cancel).await;
                    }
                    let handle = self.get_handle_or_start(cancel).await?;
                    let mut remaining = *count;
                    if remaining > MAX_KEY_BATCH {
                        let pre_count = remaining - ((remaining - 1) % MAX_KEY_BATCH + 1);
                        self.send_key_batches(&handle, &req.app_id, &normalized, pre_count, cancel)
                            .await?;
                        remaining -= pre_count;
                    }
                    ActKind::Key {
                        key: normalized,
                        modifiers: Vec::new(),
                        count: Some(remaining),
                    }
                }
            };

            let handle = self.get_handle_or_start(cancel).await?;
            let wire_req = ActRequest {
                app_id: req.app_id.clone(),
                kind: act_kind,
                post_action_delay_ms: req.post_action_delay_ms,
                return_hierarchy: Some(req.return_hierarchy),
                include_system_ui: Some(req.include_system_ui),
                known_hash: req.known_hash.clone(),
            };
            let resp = self
                .call_agent(
                    &handle,
                    "act_and_observe",
                    handle.client.act(&wire_req),
                    cancel,
                )
                .await?;
            Ok(ActOutcome {
                action_ok: resp.ok,
                elapsed_ms: if resp.elapsed_ms > 0 {
                    resp.elapsed_ms
                } else {
                    started.elapsed().as_millis() as u64
                },
                hierarchy: resp.hierarchy.as_ref().map(hierarchy_response_to_ui),
                hierarchy_error: resp.hierarchy_error,
            })
        })
    }

    fn wait_for_selector<'a>(
        &'a self,
        req: &'a WaitSelectorRequest,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, WaitSelectorOutcome> {
        Box::pin(async move {
            let use_server_wait = req.selector.is_simple_literal_for_server_wait()
                && req.timeout <= MAX_SERVER_WAIT_TIMEOUT
                && req.fail_fast_on_settle_polls.is_none()
                && (!req.wait_stable || (2..=10).contains(&req.stable_polls));

            if use_server_wait {
                self.server_wait_for_selector(req, cancel).await
            } else {
                wait_local(self, req, cancel).await
            }
        })
    }
}

/// Capabilities genuinely supported by [`IosDriver`].
///
/// Always omits [`DriverCapability::SystemBack`]. Advertises [`DriverCapability::DisableAnimations`]
/// when `disable_animations` is enabled (the default).
fn ios_capabilities(disable_animations: bool) -> DriverCapabilities {
    let mut caps = vec![
        DriverCapability::AccessibilityTree,
        DriverCapability::ClearState,
        DriverCapability::ClearKeychain,
        DriverCapability::CompoundAct,
        DriverCapability::ServerSideWait,
        DriverCapability::HierarchyHashShortCircuit,
        DriverCapability::HoldScroll,
        DriverCapability::AppCrashReports,
    ];
    if disable_animations {
        caps.push(DriverCapability::DisableAnimations);
    }
    DriverCapabilities::with(caps)
}

/// Derives platform-neutral `checked` state for XCTest checkable controls (`Switch`, `CheckBox`,
/// `Toggle`, `RadioButton`, and their `XCUIElementType*` forms) from `node.value`.
fn derive_checked_state(element_type: &str, value: Option<&str>) -> bool {
    let checkable = matches!(
        element_type.trim().to_ascii_lowercase().as_str(),
        "switch"
            | "checkbox"
            | "toggle"
            | "radiobutton"
            | "xcuielementtypeswitch"
            | "xcuielementtypecheckbox"
            | "xcuielementtypetoggle"
            | "xcuielementtyperadiobutton"
    );
    if !checkable {
        return false;
    }
    let Some(raw) = value else {
        return false;
    };
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "on"
    )
}

/// Converts a `DejaXCTestRunner` [`HierarchyNode`] into a platform-neutral [`UiNode`].
#[must_use]
pub fn hierarchy_node_to_ui_node(node: &HierarchyNode) -> UiNode {
    UiNode {
        index: node.index,
        parent_index: node.parent_index,
        depth: node.depth,
        source: node.source.clone(),
        observation_source: ObservationSource::Tree,
        element_type: node.element_type.clone(),
        identifier: node.identifier.clone(),
        label: node.label.clone(),
        title: node.title.clone(),
        value: node.value.clone(),
        placeholder_value: node.placeholder_value.clone(),
        enabled: node.enabled,
        checked: derive_checked_state(&node.element_type, node.value.as_deref()),
        selected: node.selected,
        has_focus: node.has_focus,
        hittable: node.hittable,
        is_on_screen: node.effective_is_on_screen(),
        frame: UiRect::new(
            node.frame.x,
            node.frame.y,
            node.frame.width,
            node.frame.height,
        ),
        visible_frame: None,
    }
}

/// Converts a `DejaXCTestRunner` [`HierarchyResponse`] into a platform-neutral [`UiHierarchy`].
///
/// Extracts `viewport` from the first positive `Application` node (`source == "app"` preferred,
/// else any `Application`, else `depth == 0`) and returns `None` when `resp.unchanged == true`.
#[must_use]
pub fn hierarchy_response_to_ui(resp: &HierarchyResponse) -> UiHierarchy {
    let viewport = if resp.unchanged {
        None
    } else {
        extract_viewport(&resp.nodes)
    };
    let nodes = resp.nodes.iter().map(hierarchy_node_to_ui_node).collect();
    UiHierarchy {
        app_id: resp.app_id.clone(),
        state: resp.state.clone(),
        viewport,
        truncated: resp.truncated,
        hierarchy_hash: resp.hierarchy_hash.clone(),
        unchanged: resp.unchanged,
        nodes,
    }
}

/// Formats a [`LaunchArgumentValue`] into a CLI string argument for `xcrun simctl launch`.
pub fn format_launch_argument_value(value: &LaunchArgumentValue) -> Result<String> {
    match value {
        LaunchArgumentValue::String(s) => Ok(s.clone()),
        LaunchArgumentValue::Bool(b) => Ok(if *b { "true" } else { "false" }.to_string()),
        LaunchArgumentValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i.to_string())
            } else if let Some(u) = n.as_u64() {
                Ok(u.to_string())
            } else if let Some(f) = n.as_f64() {
                if !f.is_finite() {
                    return Err(IosDriverError::InvalidArgument(
                        "launch argument number must be finite".to_string(),
                    ));
                }
                Ok(deja_core::js_number_to_string(f))
            } else {
                Ok(n.to_string())
            }
        }
    }
}

fn extract_viewport(nodes: &[HierarchyNode]) -> Option<UiSize> {
    let positive_size = |n: &HierarchyNode| -> Option<UiSize> {
        let size = UiSize::new(n.frame.width, n.frame.height);
        size.is_positive().then_some(size)
    };
    nodes
        .iter()
        .filter(|n| n.element_type == "Application" && n.source == "app")
        .find_map(positive_size)
        .or_else(|| {
            nodes
                .iter()
                .filter(|n| n.element_type == "Application")
                .find_map(positive_size)
        })
        .or_else(|| {
            nodes
                .iter()
                .filter(|n| n.depth == 0 && n.source == "app")
                .find_map(positive_size)
        })
        .or_else(|| {
            nodes
                .iter()
                .filter(|n| n.depth == 0)
                .find_map(positive_size)
        })
}

fn has_visible_keyboard(nodes: &[UiNode]) -> bool {
    nodes
        .iter()
        .any(|n| n.element_type.eq_ignore_ascii_case("Keyboard") && n.is_on_screen)
}

fn find_keyboard_dismiss_target(nodes: &[UiNode]) -> Option<UiPoint> {
    let keyboard_ids: HashSet<usize> = nodes
        .iter()
        .filter(|n| n.element_type.eq_ignore_ascii_case("Keyboard") && n.is_on_screen)
        .map(|n| n.index)
        .collect();
    if keyboard_ids.is_empty() {
        return None;
    }

    let parent_by_id: HashMap<usize, Option<usize>> =
        nodes.iter().map(|n| (n.index, n.parent_index)).collect();

    let is_in_keyboard = |node_idx: usize| -> bool {
        let mut visited = HashSet::new();
        visited.insert(node_idx);
        let mut cursor = parent_by_id.get(&node_idx).copied().flatten();
        while let Some(parent) = cursor {
            if keyboard_ids.contains(&parent) {
                return true;
            }
            if !visited.insert(parent) {
                break;
            }
            cursor = parent_by_id.get(&parent).copied().flatten();
        }
        false
    };

    let matches_dismiss_label = |raw: &str| -> bool {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "hide keyboard" | "dismiss keyboard" | "keyboard dismiss" | "dismiss"
        )
    };
    let is_submitting_label = |raw: &str| -> bool {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "return"
                | "enter"
                | "done"
                | "search"
                | "go"
                | "send"
                | "join"
                | "route"
                | "next"
                | "continue"
        )
    };

    let mut fallback: Option<UiPoint> = None;
    for node in nodes {
        if !node.is_on_screen || !node.enabled || node.frame.is_empty() {
            continue;
        }
        if !is_in_keyboard(node.index) {
            continue;
        }
        if is_submitting_label(&node.identifier)
            || is_submitting_label(&node.label)
            || is_submitting_label(&node.title)
        {
            continue;
        }
        let is_dismiss = matches_dismiss_label(&node.identifier)
            || matches_dismiss_label(&node.label)
            || matches_dismiss_label(&node.title);
        if !is_dismiss {
            continue;
        }
        let pt = node.frame.center();
        if node.hittable {
            return Some(pt);
        }
        if fallback.is_none() {
            fallback = Some(pt);
        }
    }
    fallback
}

fn unsupported_system_back() -> DriverError {
    DriverError::Unsupported(UnsupportedCapability {
        platform: "ios".to_string(),
        capability: DriverCapability::SystemBack,
        reason: "iOS has no system Back button".to_string(),
    })
}

fn validate_point(point: UiPoint) -> std::result::Result<(), DriverError> {
    if !point.x.is_finite() || !point.y.is_finite() || point.x < 0.0 || point.y < 0.0 {
        return Err(DriverError::InvalidArgument(format!(
            "coordinates must be finite non-negative numbers, got ({}, {})",
            point.x, point.y
        )));
    }
    Ok(())
}

fn validate_key_press(key: &str, modifiers: &[String]) -> std::result::Result<String, DriverError> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err(DriverError::InvalidArgument(
            "press_key requires a non-empty key name".to_string(),
        ));
    }
    if trimmed.eq_ignore_ascii_case("back") {
        return Err(unsupported_system_back());
    }
    if !modifiers.is_empty() {
        return Err(DriverError::InvalidArgument(
            "keyboard modifiers are not supported on iOS Simulator".to_string(),
        ));
    }
    let normalized = match trimmed.to_ascii_lowercase().as_str() {
        "forward delete" => "forward_delete".to_string(),
        "page up" => "page_up".to_string(),
        "page down" => "page_down".to_string(),
        "up arrow" => "uparrow".to_string(),
        "down arrow" => "downarrow".to_string(),
        "left arrow" => "leftarrow".to_string(),
        "right arrow" => "rightarrow".to_string(),
        _ => trimmed.to_string(),
    };
    Ok(normalized)
}

fn is_app_alive_state(state: &str) -> bool {
    matches!(
        state,
        "running_foreground" | "running_background" | "running_background_suspended"
    )
}

fn is_dead_session_error(err: &IosDriverError) -> bool {
    match err {
        IosDriverError::Http { source, .. } => !source.is_timeout(),
        IosDriverError::AgentHttpStatus { status: 503, .. } => true,
        _ => false,
    }
}

fn is_reqwest_timeout_error(err: &IosDriverError) -> bool {
    matches!(err, IosDriverError::Http { source, .. } if source.is_timeout())
}

fn map_agent_err(operation: &str, elapsed_ms: u64, err: IosDriverError) -> DriverError {
    match err {
        IosDriverError::Cancelled => DriverError::Cancelled,
        IosDriverError::AgentHttpStatus {
            status: 409,
            endpoint,
            body,
        } => DriverError::Transport {
            operation: operation.to_string(),
            detail: format!("runner busy on `{endpoint}` (HTTP 409): {body}"),
            transient: true,
        },
        IosDriverError::AgentHttpStatus {
            status: 400,
            endpoint,
            body,
        } => {
            DriverError::InvalidArgument(format!("agent rejected `{endpoint}` (HTTP 400): {body}"))
        }
        IosDriverError::Http { endpoint, source } if source.is_timeout() => DriverError::Timeout {
            operation: operation.to_string(),
            elapsed_ms,
            detail: format!("HTTP timeout calling `{endpoint}`: {source}"),
        },
        other => map_ios_err(operation, other),
    }
}

fn map_ios_err(operation: &str, err: IosDriverError) -> DriverError {
    match err {
        IosDriverError::Cancelled => DriverError::Cancelled,
        IosDriverError::InvalidUdid(_)
        | IosDriverError::InvalidBundleId(_)
        | IosDriverError::InvalidPath(_)
        | IosDriverError::InvalidArgument(_)
        | IosDriverError::UnsupportedPlatform(_) => DriverError::InvalidArgument(err.to_string()),
        IosDriverError::Timeout(detail) => DriverError::Timeout {
            operation: operation.to_string(),
            elapsed_ms: 0,
            detail,
        },
        IosDriverError::Io { source, .. } => DriverError::Io(source),
        other => DriverError::Transport {
            operation: operation.to_string(),
            detail: other.to_string(),
            transient: false,
        },
    }
}

fn scrub_visible_sample(
    sample: &[String],
    redactor: Option<&deja_core::Redactor>,
    provider_redactor: Option<&deja_core::ProviderKeyRedactor>,
) -> VisibleContext {
    let default_provider = provider_key_redactor(std::iter::empty::<(&str, &str)>());
    let provider = provider_redactor.unwrap_or(&default_provider);

    let scrubbed: Vec<String> = sample
        .iter()
        .map(|item| {
            let step1 = match redactor {
                Some(r) => r.redact(item),
                None => item.clone(),
            };
            provider.redact(&step1)
        })
        .collect();

    VisibleContext::from_candidates(
        scrubbed.iter().map(String::as_str),
        VisibleContext::DEFAULT_LIMIT,
    )
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{
        IosDriver, IosDriverConfig, format_launch_argument_value, hierarchy_response_to_ui,
    };
    use crate::client::{AgentClient, ElementFrame, HierarchyNode, HierarchyResponse};
    use deja_core::{
        CrashSignalSource, LaunchArgumentValue, create_redactor, provider_key_redactor,
    };
    use deja_flow::{
        ActAction, ActPayload, DriverCapability, DriverError, KeyPress, ObservationSource,
        OpenLinkOptions, PlatformDriver, Selector, SwipeParams, UiPoint, UiSize,
        WaitSelectorRequest,
    };
    use serde_json::{Value, json};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_util::sync::CancellationToken;

    const TEST_UDID: &str = "BA555783-AC7E-46E3-B749-34F9499B2C2B";
    const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[derive(Debug, Clone)]
    struct RecordedRequest {
        method: String,
        target: String,
        body: Option<Value>,
    }

    type Responder = Arc<dyn Fn(&RecordedRequest) -> (u16, String) + Send + Sync>;
    type AsyncResponder = Arc<
        dyn Fn(
                RecordedRequest,
            )
                -> std::pin::Pin<Box<dyn std::future::Future<Output = (u16, String)> + Send>>
            + Send
            + Sync,
    >;

    struct MockAgent {
        port: u16,
        requests: Arc<StdMutex<Vec<RecordedRequest>>>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl MockAgent {
        async fn start(responder: Responder) -> Self {
            Self::start_async(Arc::new(move |req| {
                let res = responder(&req);
                Box::pin(async move { res })
            }))
            .await
        }

        async fn start_async(responder: AsyncResponder) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock agent");
            let port = listener.local_addr().expect("mock addr").port();
            let requests = Arc::new(StdMutex::new(Vec::new()));
            let reqs_clone = Arc::clone(&requests);

            let handle = tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        break;
                    };
                    let reqs = Arc::clone(&reqs_clone);
                    let resp_fn = Arc::clone(&responder);
                    tokio::spawn(async move {
                        let Some(raw) = read_http_request(&mut stream).await else {
                            return;
                        };
                        let recorded = parse_http_request(&raw);
                        reqs.lock().unwrap().push(recorded.clone());
                        let (status, body) = resp_fn(recorded).await;
                        let status_text = match status {
                            200 => "OK",
                            400 => "Bad Request",
                            409 => "Conflict",
                            500 => "Internal Server Error",
                            503 => "Service Unavailable",
                            _ => "Status",
                        };
                        let http = format!(
                            "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(http.as_bytes()).await;
                    });
                }
            });

            Self {
                port,
                requests,
                handle,
            }
        }

        fn client(&self) -> AgentClient {
            self.client_with_timeout(Duration::from_secs(5))
        }

        fn client_with_timeout(&self, timeout: Duration) -> AgentClient {
            AgentClient::new(self.port, TEST_TOKEN.to_string(), timeout).expect("mock client")
        }

        fn recorded(&self) -> Vec<RecordedRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for MockAgent {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    async fn read_http_request(stream: &mut tokio::net::TcpStream) -> Option<Vec<u8>> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut header_end = None;
        let mut content_len = 0usize;

        loop {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if header_end.is_none()
                && let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n")
            {
                let end = pos + 4;
                header_end = Some(end);
                let headers_str = String::from_utf8_lossy(&buf[..pos]);
                for line in headers_str.lines().skip(1) {
                    if let Some((k, v)) = line.split_once(':')
                        && k.trim().eq_ignore_ascii_case("content-length")
                    {
                        content_len = v.trim().parse::<usize>().unwrap_or(0);
                    }
                }
            }
            if let Some(end) = header_end
                && buf.len() >= end + content_len
            {
                break;
            }
        }
        if buf.is_empty() { None } else { Some(buf) }
    }

    fn parse_http_request(raw: &[u8]) -> RecordedRequest {
        let header_end = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or(raw.len());
        let header_str = String::from_utf8_lossy(&raw[..header_end]);
        let request_line = header_str.lines().next().unwrap_or("");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("").to_string();
        let body_start = (header_end + 4).min(raw.len());
        let body_bytes = &raw[body_start..];
        let body = if body_bytes.is_empty() {
            None
        } else {
            serde_json::from_slice(body_bytes).ok()
        };
        RecordedRequest {
            method,
            target,
            body,
        }
    }

    fn sample_hierarchy_node(
        index: usize,
        element_type: &str,
        id: &str,
        label: &str,
    ) -> HierarchyNode {
        HierarchyNode {
            index,
            parent_index: (index > 0).then_some(0),
            depth: usize::from(index > 0),
            source: "app".to_string(),
            element_type: element_type.to_string(),
            identifier: id.to_string(),
            label: label.to_string(),
            title: String::new(),
            value: None,
            placeholder_value: None,
            enabled: true,
            selected: false,
            has_focus: false,
            hittable: true,
            is_on_screen: Some(true),
            frame: ElementFrame {
                x: 0.0,
                y: 0.0,
                width: 393.0,
                height: 852.0,
            },
        }
    }

    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "deja-driver-ios-test-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn capabilities_and_platform_contract() {
        let driver = IosDriver::new(IosDriverConfig::new(TEST_UDID)).expect("new driver");
        assert_eq!(driver.platform(), "ios");
        assert_eq!(driver.udid(), TEST_UDID);
        assert_eq!(driver.attach_count(), 0);

        let caps = driver.capabilities();
        for expected in [
            DriverCapability::AccessibilityTree,
            DriverCapability::ClearState,
            DriverCapability::ClearKeychain,
            DriverCapability::CompoundAct,
            DriverCapability::ServerSideWait,
            DriverCapability::HierarchyHashShortCircuit,
            DriverCapability::HoldScroll,
            DriverCapability::AppCrashReports,
            DriverCapability::DisableAnimations,
        ] {
            assert!(caps.supports(expected), "missing capability {expected}");
        }
        assert!(!caps.supports(DriverCapability::SystemBack));

        let opt_out =
            IosDriver::new(IosDriverConfig::new(TEST_UDID).with_disable_animations(false))
                .expect("opt-out driver");
        assert!(
            !opt_out
                .capabilities()
                .supports(DriverCapability::DisableAnimations)
        );
        assert!(
            !opt_out
                .capabilities()
                .supports(DriverCapability::SystemBack)
        );
    }

    #[test]
    fn hierarchy_conversion_extracts_viewport_and_maps_nodes() {
        let mut disabled_onscreen = sample_hierarchy_node(1, "Button", "btn_disabled", "Disabled");
        disabled_onscreen.enabled = false;
        disabled_onscreen.hittable = false;
        disabled_onscreen.is_on_screen = Some(true);

        let resp = HierarchyResponse {
            app_id: "dev.deja.ios.checkout".to_string(),
            state: "running_foreground".to_string(),
            state_code: 4,
            truncated: false,
            hierarchy_hash: "0123456789abcdef".to_string(),
            unchanged: false,
            node_count: 2,
            nodes: vec![
                sample_hierarchy_node(0, "Application", "", "DejaFixture"),
                disabled_onscreen,
            ],
        };
        let ui = hierarchy_response_to_ui(&resp);
        assert_eq!(ui.viewport, Some(UiSize::new(393.0, 852.0)));
        assert_eq!(ui.nodes.len(), 2);
        assert_eq!(ui.nodes[1].observation_source, ObservationSource::Tree);
        assert!(!ui.nodes[1].checked);
        assert!(ui.nodes[1].is_on_screen);
        assert!(!ui.nodes[1].hittable);

        let unchanged_resp = HierarchyResponse {
            unchanged: true,
            nodes: Vec::new(),
            node_count: 0,
            ..resp
        };
        assert_eq!(hierarchy_response_to_ui(&unchanged_resp).viewport, None);
    }

    #[test]
    fn formats_launch_argument_scalars() {
        assert_eq!(
            format_launch_argument_value(&LaunchArgumentValue::String("WC-2027".into())).unwrap(),
            "WC-2027"
        );
        assert_eq!(
            format_launch_argument_value(&LaunchArgumentValue::Bool(true)).unwrap(),
            "true"
        );
        assert_eq!(
            format_launch_argument_value(&LaunchArgumentValue::Bool(false)).unwrap(),
            "false"
        );
        assert_eq!(
            format_launch_argument_value(&LaunchArgumentValue::Number(
                serde_json::Number::from_f64(1.5).unwrap()
            ))
            .unwrap(),
            "1.5"
        );
        assert_eq!(
            format_launch_argument_value(&LaunchArgumentValue::Number(
                serde_json::Number::from_f64(10.0).unwrap()
            ))
            .unwrap(),
            "10"
        );
    }

    #[tokio::test]
    async fn back_and_key_validation_and_open_link_options_fail_closed() {
        let mock = MockAgent::start(Arc::new(|_| (200, r#"{"ok":true}"#.to_string()))).await;
        let driver =
            IosDriver::with_mock_client_for_tests(IosDriverConfig::new(TEST_UDID), mock.client())
                .expect("driver");
        let cancel = CancellationToken::new();

        // back() -> Unsupported(SystemBack)
        match driver.back("dev.deja.ios.checkout", &cancel).await {
            Err(DriverError::Unsupported(u)) => {
                assert_eq!(u.platform, "ios");
                assert_eq!(u.capability, DriverCapability::SystemBack);
            }
            other => panic!("expected Unsupported(SystemBack), got {other:?}"),
        }

        // pressKey "back" -> Unsupported(SystemBack)
        match driver
            .press_key("dev.deja.ios.checkout", &KeyPress::single("Back"), &cancel)
            .await
        {
            Err(DriverError::Unsupported(u)) => {
                assert_eq!(u.capability, DriverCapability::SystemBack);
            }
            other => panic!("expected Unsupported(SystemBack), got {other:?}"),
        }

        // pressKey with modifiers -> InvalidArgument
        let mod_key = KeyPress {
            key: "a".to_string(),
            modifiers: vec!["command".to_string()],
            count: 1,
        };
        assert!(matches!(
            driver
                .press_key("dev.deja.ios.checkout", &mod_key, &cancel)
                .await,
            Err(DriverError::InvalidArgument(_))
        ));

        // openLink autoVerify: true -> InvalidArgument
        assert!(matches!(
            driver
                .open_link(
                    None,
                    "https://example.com",
                    &OpenLinkOptions {
                        auto_verify: Some(true),
                        browser: None,
                    },
                    &cancel
                )
                .await,
            Err(DriverError::InvalidArgument(_))
        ));

        // openLink browser: true with custom scheme -> InvalidArgument
        assert!(matches!(
            driver
                .open_link(
                    None,
                    "myapp://checkout",
                    &OpenLinkOptions {
                        auto_verify: None,
                        browser: Some(true),
                    },
                    &cancel
                )
                .await,
            Err(DriverError::InvalidArgument(_))
        ));

        // Zero-count erase_text / press_key make ZERO HTTP calls.
        driver
            .erase_text("dev.deja.ios.checkout", 0, &cancel)
            .await
            .expect("zero erase");
        driver
            .press_key(
                "dev.deja.ios.checkout",
                &KeyPress {
                    key: "return".to_string(),
                    modifiers: Vec::new(),
                    count: 0,
                },
                &cancel,
            )
            .await
            .expect("zero key");
        assert!(mock.recorded().is_empty());
    }

    #[tokio::test]
    async fn erase_text_and_act_and_observe_chunk_over_100_and_preserve_partial_success() {
        let mock = MockAgent::start(Arc::new(|req| {
            if req.target == "/key" {
                (200, r#"{"ok":true}"#.to_string())
            } else if req.target == "/act" {
                let kind = req
                    .body
                    .as_ref()
                    .and_then(|b| b.get("action"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if kind == "tap" {
                    (
                        200,
                        json!({
                            "ok": true,
                            "elapsedMs": 12,
                            "hierarchyError": "AX snapshot transient failure"
                        })
                        .to_string(),
                    )
                } else {
                    (200, json!({"ok": true, "elapsedMs": 15}).to_string())
                }
            } else {
                (404, r#"{"error":"not found"}"#.to_string())
            }
        }))
        .await;

        let driver =
            IosDriver::with_mock_client_for_tests(IosDriverConfig::new(TEST_UDID), mock.client())
                .expect("driver");
        let cancel = CancellationToken::new();

        // 250 erases -> 100, 100, 50.
        driver
            .erase_text("dev.deja.ios.checkout", 250, &cancel)
            .await
            .expect("chunked erase");
        let counts: Vec<u64> = mock
            .recorded()
            .iter()
            .filter(|r| r.target == "/key")
            .map(|r| r.body.as_ref().unwrap()["count"].as_u64().unwrap())
            .collect();
        assert_eq!(counts, vec![100, 100, 50]);

        // Compound act_and_observe with 250 erases -> 2x /key (100, 100) + 1x /act (50).
        let act_outcome = driver
            .act_and_observe(
                &ActPayload {
                    app_id: "dev.deja.ios.checkout".to_string(),
                    action: ActAction::EraseText { count: 250 },
                    post_action_delay_ms: None,
                    return_hierarchy: false,
                    include_system_ui: false,
                    known_hash: None,
                },
                &cancel,
            )
            .await
            .expect("compound chunked erase");
        assert!(act_outcome.action_ok);
        let last_act = mock
            .recorded()
            .into_iter()
            .rev()
            .find(|r| r.target == "/act")
            .expect("final /act call");
        assert_eq!(last_act.body.as_ref().unwrap()["count"], json!(50));

        // Partial success on /act preserves `action_ok: true` and `hierarchy_error`.
        let partial = driver
            .act_and_observe(
                &ActPayload {
                    app_id: "dev.deja.ios.checkout".to_string(),
                    action: ActAction::LongPress {
                        x: 50.0,
                        y: 60.0,
                        duration_ms: None,
                    },
                    post_action_delay_ms: None,
                    return_hierarchy: true,
                    include_system_ui: false,
                    known_hash: None,
                },
                &cancel,
            )
            .await
            .expect("partial success");
        assert!(partial.action_ok);
        assert!(partial.hierarchy.is_none());
        assert_eq!(
            partial.hierarchy_error.as_deref(),
            Some("AX snapshot transient failure")
        );
        let lp_req = mock
            .recorded()
            .into_iter()
            .rev()
            .find(|r| r.target == "/act")
            .unwrap();
        assert_eq!(lp_req.body.as_ref().unwrap()["durationMs"], json!(1000));
    }

    #[tokio::test]
    async fn hide_keyboard_noop_dismiss_key_and_never_submits_return() {
        let mode = Arc::new(StdMutex::new(0u8));
        let mode_clone = Arc::clone(&mode);

        let make_keyboard = || HierarchyNode {
            index: 1,
            parent_index: Some(0),
            depth: 1,
            source: "app".into(),
            element_type: "Keyboard".into(),
            identifier: String::new(),
            label: String::new(),
            title: String::new(),
            value: None,
            placeholder_value: None,
            enabled: true,
            selected: false,
            has_focus: false,
            hittable: true,
            is_on_screen: Some(true),
            frame: ElementFrame {
                x: 0.0,
                y: 500.0,
                width: 393.0,
                height: 300.0,
            },
        };
        let make_key = |index: usize, id: &str, label: &str, x: f64| HierarchyNode {
            index,
            parent_index: Some(1),
            depth: 2,
            source: "app".into(),
            element_type: "Button".into(),
            identifier: id.into(),
            label: label.into(),
            title: String::new(),
            value: None,
            placeholder_value: None,
            enabled: true,
            selected: false,
            has_focus: false,
            hittable: true,
            is_on_screen: Some(true),
            frame: ElementFrame {
                x,
                y: 740.0,
                width: 60.0,
                height: 40.0,
            },
        };

        let mock = MockAgent::start(Arc::new(move |req| {
            if req.target == "/tap" {
                let mut m = mode_clone.lock().unwrap();
                // Mode 1 simulates a successful keyboard dismissal after tapping the dismiss key.
                // Mode 3 leaves the keyboard visible after tapping to test verification failure.
                if *m == 1 {
                    *m = 0;
                }
                return (200, r#"{"ok":true}"#.to_string());
            }
            if req.target.starts_with("/hierarchy") {
                let m = *mode_clone.lock().unwrap();
                let nodes = match m {
                    0 => vec![sample_hierarchy_node(0, "Application", "", "App")],
                    1 | 3 => vec![
                        sample_hierarchy_node(0, "Application", "", "App"),
                        make_keyboard(),
                        make_key(2, "Hide keyboard", "Hide keyboard", 320.0),
                    ],
                    _ => vec![
                        sample_hierarchy_node(0, "Application", "", "App"),
                        make_keyboard(),
                        make_key(2, "Return", "Return", 320.0),
                        make_key(3, "Return", "Done", 250.0),
                        make_key(4, "Done", "Done", 210.0),
                        make_key(5, "Hide", "Hide", 170.0),
                        make_key(6, "Search", "Search", 130.0),
                        make_key(7, "Send", "Send", 90.0),
                    ],
                };
                let resp = HierarchyResponse {
                    app_id: "dev.deja.ios.checkout".into(),
                    state: "running_foreground".into(),
                    state_code: 4,
                    truncated: false,
                    hierarchy_hash: "0123456789abcdef".into(),
                    unchanged: false,
                    node_count: nodes.len(),
                    nodes,
                };
                (200, serde_json::to_string(&resp).unwrap())
            } else {
                (200, r#"{"ok":true}"#.to_string())
            }
        }))
        .await;

        let mut driver =
            IosDriver::with_mock_client_for_tests(IosDriverConfig::new(TEST_UDID), mock.client())
                .expect("driver");
        driver.keyboard_dismiss_verify_timeout = Duration::from_millis(80);
        let cancel = CancellationToken::new();

        // Mode 0: no keyboard -> no tap/key call.
        driver
            .hide_keyboard("dev.deja.ios.checkout", &cancel)
            .await
            .expect("noop hide keyboard");
        assert!(
            mock.recorded()
                .iter()
                .all(|r| r.target.starts_with("/hierarchy"))
        );

        // Mode 1: explicit Hide keyboard key -> taps center (350, 760) and verifies dismissal.
        *mode.lock().unwrap() = 1;
        driver
            .hide_keyboard("dev.deja.ios.checkout", &cancel)
            .await
            .expect("tap hide keyboard");
        let tap = mock
            .recorded()
            .into_iter()
            .find(|r| r.target == "/tap")
            .expect("expected /tap on dismiss key");
        assert_eq!(tap.body.as_ref().unwrap()["x"], json!(350.0));
        assert_eq!(tap.body.as_ref().unwrap()["y"], json!(760.0));
        assert!(
            !mock.recorded().iter().any(|r| r.target == "/key"),
            "hide_keyboard must never send /key"
        );

        // Mode 2: keyboard with only submitting keys (Return, Return/Done, Search, Send) ->
        // must fail closed without tapping any submitting key or pressing Return.
        let taps_before_mode2 = mock
            .recorded()
            .iter()
            .filter(|r| r.target == "/tap")
            .count();
        *mode.lock().unwrap() = 2;
        match driver.hide_keyboard("dev.deja.ios.checkout", &cancel).await {
            Err(DriverError::Transport {
                operation,
                transient: false,
                detail,
            }) => {
                assert_eq!(operation, "hide_keyboard");
                assert!(detail.contains("dismiss"), "unexpected detail: {detail}");
            }
            other => panic!("expected fail-closed hide_keyboard error, got {other:?}"),
        }
        assert_eq!(
            mock.recorded()
                .iter()
                .filter(|r| r.target == "/tap")
                .count(),
            taps_before_mode2,
            "must not tap submitting keys"
        );
        assert!(
            !mock.recorded().iter().any(|r| r.target == "/key"),
            "must never fall back to Return /key"
        );

        // Mode 3: safe dismiss button tapped, but keyboard remains visible -> fails verification.
        *mode.lock().unwrap() = 3;
        match driver.hide_keyboard("dev.deja.ios.checkout", &cancel).await {
            Err(DriverError::Transport {
                operation,
                transient: false,
                detail,
            }) => {
                assert_eq!(operation, "hide_keyboard");
                assert!(
                    detail.contains("remained visible"),
                    "unexpected detail: {detail}"
                );
            }
            other => panic!("expected verification failure error, got {other:?}"),
        }
        assert!(
            !mock.recorded().iter().any(|r| r.target == "/key"),
            "verification failure must never fall back to Return /key"
        );
    }

    #[tokio::test]
    async fn wait_for_selector_routes_simple_to_server_with_exact_and_scrubs_visible_sample() {
        let synthetic_provider_key = ["sk", "-1234567890abcdefghijklmnop"].concat();
        let mock_key = synthetic_provider_key.clone();
        let mock = MockAgent::start(Arc::new(move |req| {
            if req.target == "/waitForSelector" {
                (
                    200,
                    json!({
                        "matched": false,
                        "elapsedMs": 55,
                        "polls": 3,
                        "hierarchyHash": "0123456789abcdef",
                        "visibleSample": [
                            "Order confirmed",
                            format!("token={mock_key}"),
                            "secret=my-super-secret-value"
                        ]
                    })
                    .to_string(),
                )
            } else if req.target.starts_with("/hierarchy") {
                (
                    200,
                    json!({
                        "appId": "dev.deja.ios.checkout",
                        "state": "running_foreground",
                        "stateCode": 4,
                        "truncated": false,
                        "hierarchyHash": "0123456789abcdef",
                        "unchanged": false,
                        "nodeCount": 1,
                        "nodes": [sample_hierarchy_node(0, "Application", "root", "hello.world")]
                    })
                    .to_string(),
                )
            } else {
                (404, "{}".to_string())
            }
        }))
        .await;

        let driver =
            IosDriver::with_mock_client_for_tests(IosDriverConfig::new(TEST_UDID), mock.client())
                .expect("driver");
        let cancel = CancellationToken::new();

        // 1. Simple literal with `fail_fast_on_settle_polls: None` -> server `/waitForSelector` with `textMatch: "exact"`.
        let mut req = WaitSelectorRequest::visible(
            "dev.deja.ios.checkout",
            Selector::by_text("Order confirmed"),
            Duration::from_secs(2),
        );
        req.redactor = Some(create_redactor(
            "/tmp/ws",
            None,
            &["my-super-secret-value".to_string()],
        ));
        req.provider_redactor = Some(provider_key_redactor(std::iter::empty::<(&str, &str)>()));

        let outcome = driver
            .wait_for_selector(&req, &cancel)
            .await
            .expect("server wait");
        assert!(!outcome.matched);
        let joined = outcome.visible_context.format_summary();
        assert!(joined.contains("Order confirmed"), "got: {joined}");
        assert!(
            !joined.contains(&synthetic_provider_key),
            "provider key leaked: {joined}"
        );
        assert!(
            !joined.contains("my-super-secret-value"),
            "redacted secret leaked: {joined}"
        );
        let server_req = mock
            .recorded()
            .into_iter()
            .find(|r| r.method == "POST" && r.target == "/waitForSelector")
            .expect("must use POST /waitForSelector");
        assert_eq!(
            server_req.body.as_ref().unwrap()["textMatch"],
            json!("exact")
        );

        // 2. Regex selector OR `fail_fast_on_settle_polls: Some(3)` -> local `wait_local` via `/hierarchy`.
        let mut local_req = WaitSelectorRequest::visible(
            "dev.deja.ios.checkout",
            Selector::by_text("hello.world"),
            Duration::from_millis(200),
        );
        local_req.fail_fast_on_settle_polls = Some(3);
        let local_outcome = driver
            .wait_for_selector(&local_req, &cancel)
            .await
            .expect("local wait");
        assert!(local_outcome.matched);
        assert!(
            mock.recorded()
                .iter()
                .any(|r| r.target.starts_with("/hierarchy"))
        );
    }

    #[tokio::test]
    async fn maps_409_to_transient_and_dead_connection_invalidates_session() {
        let mock = MockAgent::start(Arc::new(|_| {
            (409, r#"{"error":"runner busy"}"#.to_string())
        }))
        .await;
        let driver =
            IosDriver::with_mock_client_for_tests(IosDriverConfig::new(TEST_UDID), mock.client())
                .expect("driver");
        let cancel = CancellationToken::new();

        match driver
            .tap_point("dev.deja.ios.checkout", UiPoint::new(10.0, 20.0), &cancel)
            .await
        {
            Err(DriverError::Transport {
                transient: true, ..
            }) => {}
            other => panic!("expected transient Transport for 409, got {other:?}"),
        }
        assert!(
            driver.has_active_session(),
            "409 busy must NOT invalidate active session"
        );

        // Dead listener: bind ephemeral port and drop listener immediately so connect refuses.
        let dead_port = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let dead_client =
            AgentClient::new(dead_port, TEST_TOKEN.to_string(), Duration::from_secs(2)).unwrap();
        let dead_driver =
            IosDriver::with_mock_client_for_tests(IosDriverConfig::new(TEST_UDID), dead_client)
                .unwrap();
        assert!(dead_driver.has_active_session());

        match dead_driver
            .swipe(
                &SwipeParams {
                    app_id: "dev.deja.ios.checkout".into(),
                    start: UiPoint::new(10.0, 20.0),
                    end: UiPoint::new(10.0, 100.0),
                    duration_ms: 200,
                    hold_duration_ms: None,
                },
                &cancel,
            )
            .await
        {
            Err(DriverError::Transport {
                transient: false, ..
            }) => {}
            other => panic!("expected non-transient Transport for dead session, got {other:?}"),
        }
        assert!(
            !dead_driver.has_active_session(),
            "dead HTTP connection must clear active session"
        );
    }

    #[tokio::test]
    async fn check_app_crash_skips_grace_when_alive_and_detects_when_not_running() {
        let app_state = Arc::new(StdMutex::new("running_foreground".to_string()));
        let state_clone = Arc::clone(&app_state);
        let mock = MockAgent::start(Arc::new(move |req| {
            if req.target.starts_with("/appState") {
                let s = state_clone.lock().unwrap().clone();
                let code = if s == "running_foreground" { 4 } else { 1 };
                (
                    200,
                    json!({
                        "appId": "dev.deja.ios.checkout",
                        "state": s,
                        "stateCode": code
                    })
                    .to_string(),
                )
            } else {
                (404, "{}".to_string())
            }
        }))
        .await;

        let crash_dir = unique_temp_dir("crash-check");
        let mut cfg = IosDriverConfig::new(TEST_UDID);
        cfg.crash_report_dir = Some(crash_dir.clone());
        cfg.crash_report_grace = Duration::from_secs(2);
        let driver = IosDriver::with_mock_client_for_tests(cfg, mock.client()).expect("driver");
        let cancel = CancellationToken::new();
        let since = super::now_epoch_ms().saturating_sub(1_000);

        // 1. App alive -> returns Ok(None) immediately with 0 ms grace sleep!
        let t0 = Instant::now();
        let alive_res = driver
            .check_app_crash("dev.deja.ios.checkout", since, &cancel)
            .await
            .expect("alive crash check");
        assert!(alive_res.is_none());
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "alive app must skip 2s grace sleep (took {:?})",
            t0.elapsed()
        );

        // 2. App not_running -> polls through grace and finds delayed .ips report.
        *app_state.lock().unwrap() = "not_running".to_string();
        let dir_for_writer = crash_dir.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            let content = format!(
                "{{\"app_name\":\"DejaFixture\",\"bundleID\":\"dev.deja.ios.checkout\"}}\n{{\"procName\":\"DejaFixture\",\"procPath\":\"/sim/Library/Developer/CoreSimulator/Devices/{TEST_UDID}/data/Containers/Bundle/Application/11111111-2222-3333-4444-555555555555/DejaFixture.app/DejaFixture\"}}\n"
            );
            std::fs::write(dir_for_writer.join("DejaFixture-late.ips"), content).unwrap();
        });

        let crashed = driver
            .check_app_crash("dev.deja.ios.checkout", since, &cancel)
            .await
            .expect("crashed check")
            .expect("must detect delayed crash report");
        writer.await.unwrap();
        assert_eq!(crashed.source, CrashSignalSource::CrashReport);
        assert_eq!(crashed.report_file.as_deref(), Some("DejaFixture-late.ips"));
        assert_eq!(crashed.process.as_deref(), Some("DejaFixture"));

        let _ = std::fs::remove_dir_all(&crash_dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launch_app_crash_and_timeout_and_simctl_operations_with_stub_xcrun() {
        use deja_flow::{FlowExecutionConfig, FlowExecutor, LaunchOptions};
        use std::collections::BTreeMap;
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt;

        let root = unique_temp_dir("stub-xcrun");
        let bin_dir = root.join("bin");
        let log_file = root.join("xcrun.log");
        let crash_dir = root.join("crashes");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&crash_dir).unwrap();

        let media_file = root.join("sample.jpg");
        std::fs::write(&media_file, b"jpeg-bytes").unwrap();

        let xcrun_path = bin_dir.join("xcrun");
        let script = format!(
            r#"#!/bin/sh
echo "$@" >> "{log}"
if [ "$1" = "simctl" ] && [ "$2" = "io" ] && [ "$4" = "screenshot" ]; then
  printf '\211PNG\r\n\032\nFAKEPNGPAYLOAD' > "$6"
  exit 0
fi
if [ "$1" = "simctl" ] && [ "$2" = "launch" ]; then
  echo "dev.deja.ios.checkout: 4242"
  exit 0
fi
if [ "$1" = "simctl" ] && [ "$2" = "openurl" ] && [ "$4" = "failscheme://boom" ]; then
  echo "invalid url open" >&2
  exit 1
fi
exit 0
"#,
            log = log_file.display()
        );
        std::fs::write(&xcrun_path, script).unwrap();
        std::fs::set_permissions(&xcrun_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let warmup_mode = Arc::new(StdMutex::new(0u8));
        let warmup_mode_clone = Arc::clone(&warmup_mode);
        let mock = MockAgent::start(Arc::new(move |req| {
            if req.target == "/health" {
                (
                    200,
                    json!({
                        "status": "ok",
                        "agent": "DejaXCTestRunner",
                        "version": "1",
                        "pid": 7777
                    })
                    .to_string(),
                )
            } else if req.target == "/warmup" {
                match *warmup_mode_clone.lock().unwrap() {
                    0 => (
                        200,
                        json!({
                            "ok": true,
                            "appId": "dev.deja.ios.checkout",
                            "state": "running_foreground",
                            "stateCode": 4,
                            "elapsedMs": 10,
                            "nodeCount": 2
                        })
                        .to_string(),
                    ),
                    1 => (
                        200,
                        json!({
                            "ok": false,
                            "appId": "dev.deja.ios.checkout",
                            "state": "not_running",
                            "stateCode": 1,
                            "elapsedMs": 10,
                            "nodeCount": 0
                        })
                        .to_string(),
                    ),
                    _ => (
                        500,
                        r#"{"error":"warmup timed out: app state is running_background"}"#
                            .to_string(),
                    ),
                }
            } else if req.target == "/waitForSelector" {
                (
                    200,
                    json!({
                        "matched": true,
                        "elapsedMs": 5,
                        "polls": 1,
                        "hierarchyHash": "0123456789abcdef",
                        "node": sample_hierarchy_node(1, "StaticText", "screen_login", "Déjà iOS Shop"),
                        "visibleSample": []
                    })
                    .to_string(),
                )
            } else {
                (200, r#"{"ok":true}"#.to_string())
            }
        }))
        .await;

        let mut cfg = IosDriverConfig::new(TEST_UDID);
        cfg.env.env_path = Some(OsString::from(bin_dir.as_os_str()));
        cfg.crash_report_dir = Some(crash_dir.clone());
        cfg.crash_report_grace = Duration::from_millis(120);
        cfg.app_launch_timeout = Duration::from_millis(200);
        let driver = IosDriver::with_mock_client_for_tests(cfg, mock.client()).expect("driver");
        let cancel = CancellationToken::new();

        // 1. launch_app happy path with permissions, clear_keychain, and launch arguments.
        let mut perms = BTreeMap::new();
        perms.insert("all".to_string(), "deny".to_string());
        perms.insert("photos_add".to_string(), "allow".to_string());
        let mut args = BTreeMap::new();
        args.insert(
            "DEJA_FIXTURE_ORDER".to_string(),
            LaunchArgumentValue::String("WC-2027".to_string()),
        );
        args.insert("FLAG".to_string(), LaunchArgumentValue::Bool(true));
        let launch_opts = LaunchOptions {
            clear_state: false,
            clear_keychain: true,
            stop_app: true,
            permissions: Some(perms),
            arguments: args,
            env: BTreeMap::new(),
        };
        driver
            .launch_app("dev.deja.ios.checkout", &launch_opts, &cancel)
            .await
            .expect("launch_app should succeed");
        assert_eq!(driver.runner_pid(), Some(7777));

        let logged = std::fs::read_to_string(&log_file).unwrap();
        assert!(logged.contains(&format!(
            "simctl terminate {TEST_UDID} dev.deja.ios.checkout"
        )));
        assert!(logged.contains(&format!("simctl keychain {TEST_UDID} reset")));
        assert!(logged.contains(&format!(
            "simctl privacy {TEST_UDID} revoke all dev.deja.ios.checkout"
        )));
        assert!(logged.contains(&format!(
            "simctl privacy {TEST_UDID} grant photos-add dev.deja.ios.checkout"
        )));
        assert!(logged.contains("-DEJA_FIXTURE_ORDER WC-2027"));
        assert!(logged.contains("-FLAG true"));

        // 2. Screenshot capture returns valid PNG bytes.
        let png = driver
            .take_screenshot("dev.deja.ios.checkout", &cancel)
            .await
            .expect("screenshot");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));

        // 3. add_media, open_link success + open_link failure -> NavigationFailed.
        driver
            .add_media(std::slice::from_ref(&media_file), &cancel)
            .await
            .expect("add_media");
        driver
            .open_link(
                None,
                "https://example.com/cart",
                &OpenLinkOptions {
                    auto_verify: None,
                    browser: Some(true),
                },
                &cancel,
            )
            .await
            .expect("open_link https");
        assert!(matches!(
            driver
                .open_link(
                    None,
                    "failscheme://boom",
                    &OpenLinkOptions::default(),
                    &cancel
                )
                .await,
            Err(DriverError::NavigationFailed { .. })
        ));

        // 4. launch_app crash detection via .ips when warmup sees not_running.
        *warmup_mode.lock().unwrap() = 1;
        let crash_dir_for_writer = crash_dir.clone();
        let crash_writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let ips_body = format!(
                "{{\"app_name\":\"DejaFixture\",\"bundleID\":\"dev.deja.ios.checkout\"}}\n{{\"procName\":\"DejaFixture\",\"procPath\":\"/var/mobile/Library/Developer/CoreSimulator/Devices/{TEST_UDID}/data/Containers/Bundle/Application/11111111-2222-3333-4444-555555555555/DejaFixture.app/DejaFixture\"}}\n"
            );
            std::fs::write(crash_dir_for_writer.join("launch-crash.ips"), ips_body).unwrap();
        });
        match driver
            .launch_app("dev.deja.ios.checkout", &LaunchOptions::default(), &cancel)
            .await
        {
            Err(DriverError::AppCrashed(sig)) => {
                assert_eq!(sig.report_file.as_deref(), Some("launch-crash.ips"));
            }
            other => panic!("expected AppCrashed on launch crash, got {other:?}"),
        }
        crash_writer.await.unwrap();

        // 5. launch_app timeout without .ips -> LaunchFailed.
        let _ = std::fs::remove_file(crash_dir.join("launch-crash.ips"));
        *warmup_mode.lock().unwrap() = 2;
        assert!(matches!(
            driver
                .launch_app("dev.deja.ios.checkout", &LaunchOptions::default(), &cancel)
                .await,
            Err(DriverError::LaunchFailed { .. })
        ));

        // 6. FlowExecutor integration against IosDriver (ensure_session reuse + launchApp + assertVisible).
        *warmup_mode.lock().unwrap() = 0;
        let executor = FlowExecutor::new(FlowExecutionConfig::new(root.clone()));
        let outcome = executor
            .execute_yaml(
                &driver,
                "appId: dev.deja.ios.checkout\n---\n- launchApp\n- assertVisible: Déjà iOS Shop\n",
                &cancel,
            )
            .await;
        assert!(
            outcome.passed,
            "mock flow failed: {:?}",
            outcome.failed_step
        );
        assert_eq!(outcome.assertions_executed, 1);
        assert_eq!(driver.attach_count(), 1);

        driver.shutdown().await.expect("shutdown");
        assert!(!driver.has_active_session());
        assert!(driver.is_detached());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hierarchy_node_derives_checked_state_for_checkable_controls_only() {
        use super::hierarchy_node_to_ui_node;

        for element_type in [
            "Switch",
            "CheckBox",
            "Toggle",
            "RadioButton",
            "XCUIElementTypeSwitch",
            "XCUIElementTypeCheckBox",
            "XCUIElementTypeToggle",
            "XCUIElementTypeRadioButton",
        ] {
            for truthy in ["1", "true", "TRUE", "on", "ON"] {
                let mut node = sample_hierarchy_node(1, element_type, "ctrl", "Ctrl");
                node.value = Some(truthy.to_string());
                assert!(
                    hierarchy_node_to_ui_node(&node).checked,
                    "expected checked=true for {element_type} value={truthy:?}"
                );
            }
            for falsy in [Some("0"), Some("false"), Some("off"), Some(""), None] {
                let mut node = sample_hierarchy_node(1, element_type, "ctrl", "Ctrl");
                node.value = falsy.map(str::to_string);
                assert!(
                    !hierarchy_node_to_ui_node(&node).checked,
                    "expected checked=false for {element_type} value={falsy:?}"
                );
            }
        }

        for non_checkable in ["TextField", "StaticText", "Button", "Application"] {
            let mut node = sample_hierarchy_node(1, non_checkable, "x", "X");
            node.value = Some("1".to_string());
            assert!(
                !hierarchy_node_to_ui_node(&node).checked,
                "non-checkable {non_checkable} must stay checked=false"
            );
        }
    }

    #[tokio::test]
    async fn server_wait_without_any_hierarchy_capture_returns_transport_and_classifies_runner() {
        use deja_core::FailureCategory;
        use deja_flow::{FlowExecutionConfig, FlowExecutor};

        let mock = MockAgent::start(Arc::new(|req| {
            if req.target == "/health" {
                (
                    200,
                    json!({
                        "status": "ok",
                        "agent": "DejaXCTestRunner",
                        "version": "1",
                        "pid": 1234
                    })
                    .to_string(),
                )
            } else if req.target == "/waitForSelector" {
                (
                    200,
                    json!({
                        "matched": false,
                        "elapsedMs": 150,
                        "polls": 4,
                        "hierarchyHash": "",
                        "visibleSample": []
                    })
                    .to_string(),
                )
            } else {
                (500, r#"{"error":"AX snapshot failure"}"#.to_string())
            }
        }))
        .await;

        let driver =
            IosDriver::with_mock_client_for_tests(IosDriverConfig::new(TEST_UDID), mock.client())
                .expect("driver");
        let cancel = CancellationToken::new();

        let req = WaitSelectorRequest::visible(
            "dev.deja.ios.checkout",
            Selector::by_text("Order confirmed"),
            Duration::from_millis(200),
        );
        assert!(matches!(
            driver.wait_for_selector(&req, &cancel).await,
            Err(DriverError::Transport {
                transient: false,
                ..
            })
        ));

        let ws = unique_temp_dir("wait-capture-err");
        let mut flow_cfg = FlowExecutionConfig::new(ws.clone());
        flow_cfg.default_step_timeout = Duration::from_millis(200);
        let outcome = FlowExecutor::new(flow_cfg)
            .execute_yaml(
                &driver,
                "appId: dev.deja.ios.checkout\n---\n- assertVisible: Order confirmed\n",
                &cancel,
            )
            .await;
        assert!(!outcome.passed);
        assert_eq!(
            outcome.failure.as_ref().map(|f| f.category),
            Some(FailureCategory::Runner),
            "empty-capture server wait must classify as Runner, not ProductAssertion"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launch_app_validates_all_inputs_before_mutating_simulator_state() {
        use deja_flow::LaunchOptions;
        use std::collections::BTreeMap;
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt;

        let root = unique_temp_dir("launch-prevalidate");
        let bin_dir = root.join("bin");
        let log_file = root.join("xcrun.log");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let xcrun_path = bin_dir.join("xcrun");
        std::fs::write(
            &xcrun_path,
            format!(
                "#!/bin/sh\necho \"$@\" >> \"{}\"\nexit 0\n",
                log_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&xcrun_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mock = MockAgent::start(Arc::new(|_| (200, r#"{"ok":true}"#.to_string()))).await;
        let mut cfg = IosDriverConfig::new(TEST_UDID);
        cfg.env.env_path = Some(OsString::from(bin_dir.as_os_str()));
        let driver = IosDriver::with_mock_client_for_tests(cfg, mock.client()).expect("driver");
        let cancel = CancellationToken::new();

        let base_opts = LaunchOptions {
            clear_state: true,
            clear_keychain: true,
            stop_app: true,
            permissions: Some(BTreeMap::new()),
            arguments: BTreeMap::new(),
            env: BTreeMap::new(),
        };

        // 1. Invalid env key.
        let mut bad_env_key = base_opts.clone();
        bad_env_key
            .env
            .insert("BAD-KEY".to_string(), "val".to_string());
        assert!(matches!(
            driver
                .launch_app("dev.deja.ios.checkout", &bad_env_key, &cancel)
                .await,
            Err(DriverError::InvalidArgument(_))
        ));

        // 2. NUL in env value.
        let mut bad_env_val = base_opts.clone();
        bad_env_val
            .env
            .insert("GOOD_KEY".to_string(), "bad\0val".to_string());
        assert!(matches!(
            driver
                .launch_app("dev.deja.ios.checkout", &bad_env_val, &cancel)
                .await,
            Err(DriverError::InvalidArgument(_))
        ));

        // 3. Bare `--` launch arg key.
        let mut bad_arg_key = base_opts.clone();
        bad_arg_key
            .arguments
            .insert("--".to_string(), LaunchArgumentValue::String("x".into()));
        assert!(matches!(
            driver
                .launch_app("dev.deja.ios.checkout", &bad_arg_key, &cancel)
                .await,
            Err(DriverError::InvalidArgument(_))
        ));

        // 4. NUL in launch arg value.
        let mut bad_arg_val = base_opts.clone();
        bad_arg_val.arguments.insert(
            "DEJA_ARG".to_string(),
            LaunchArgumentValue::String("a\0b".into()),
        );
        assert!(matches!(
            driver
                .launch_app("dev.deja.ios.checkout", &bad_arg_val, &cancel)
                .await,
            Err(DriverError::InvalidArgument(_))
        ));

        // 5. Invalid bundle ID.
        assert!(matches!(
            driver.launch_app("notabundle", &base_opts, &cancel).await,
            Err(DriverError::InvalidArgument(_))
        ));

        assert!(
            !log_file.exists(),
            "no simctl terminate/clear_state/keychain/privacy commands may run on invalid launch inputs"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clear_state_cancellation_awaits_blocking_wipe_before_returning() {
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt;

        let root = unique_temp_dir("clear-state-cancel");
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let container = root
            .join("CoreSimulator")
            .join("Devices")
            .join(TEST_UDID)
            .join("data")
            .join("Containers")
            .join("Data")
            .join("Application")
            .join("44444444-2222-3333-4444-555555555555");
        let docs = container.join("Documents");
        for d in 0..20 {
            let sub = docs.join(format!("dir-{d:02}"));
            std::fs::create_dir_all(&sub).unwrap();
            for f in 0..25 {
                std::fs::write(sub.join(format!("f-{f:02}.txt")), b"payload").unwrap();
            }
        }

        let xcrun_path = bin_dir.join("xcrun");
        std::fs::write(
            &xcrun_path,
            format!(
                "#!/bin/sh\nif [ \"$2\" = \"get_app_container\" ] && [ \"$5\" = \"data\" ]; then\n  echo \"{}\"\nfi\nexit 0\n",
                container.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&xcrun_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut cfg = IosDriverConfig::new(TEST_UDID);
        cfg.env.env_path = Some(OsString::from(bin_dir.as_os_str()));
        let mut driver = IosDriver::new(cfg).expect("driver");
        driver.simulator_locks_dir = Some(root.join("locks"));

        let cancel = CancellationToken::new();
        let cancel_trigger = cancel.clone();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(2)).await;
            cancel_trigger.cancel();
        });

        let res = driver.clear_state("dev.deja.ios.checkout", &cancel).await;
        canceller.await.unwrap();
        assert!(
            matches!(res, Err(DriverError::Cancelled) | Ok(())),
            "unexpected clear_state result: {res:?}"
        );

        // Immediately after `clear_state` returns, no background wipe task may keep deleting files.
        let count_entries = |dir: &std::path::Path| -> usize {
            let mut total = 0usize;
            if let Ok(rd) = std::fs::read_dir(dir) {
                for entry in rd.flatten() {
                    total += 1;
                    if let Ok(sub) = std::fs::read_dir(entry.path()) {
                        total += sub.count();
                    }
                }
            }
            total
        };
        let count_at_return = count_entries(&docs);
        tokio::time::sleep(Duration::from_millis(80)).await;
        let count_later = count_entries(&docs);
        assert_eq!(
            count_at_return, count_later,
            "blocking wipe continued deleting files after clear_state returned"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launch_app_enforces_launch_deadline_on_stalled_warmup_and_checks_crashes() {
        use deja_flow::LaunchOptions;
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt;

        let root = unique_temp_dir("stalled-warmup");
        let bin_dir = root.join("bin");
        let crash_dir = root.join("crashes");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&crash_dir).unwrap();

        let xcrun_path = bin_dir.join("xcrun");
        std::fs::write(
            &xcrun_path,
            "#!/bin/sh\nif [ \"$2\" = \"launch\" ]; then echo \"dev.deja.ios.checkout: 999\"; fi\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&xcrun_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mock = MockAgent::start_async(Arc::new(|req| {
            Box::pin(async move {
                if req.target == "/health" {
                    (
                        200,
                        json!({"status": "ok", "agent": "DejaXCTestRunner", "version": "1", "pid": 88}).to_string(),
                    )
                } else if req.target == "/warmup" {
                    // Simulate a stalled HTTP response much longer than `app_launch_timeout`.
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    (200, r#"{"ok":false}"#.to_string())
                } else {
                    (200, r#"{"ok":true}"#.to_string())
                }
            })
        }))
        .await;

        let mut cfg = IosDriverConfig::new(TEST_UDID);
        cfg.env.env_path = Some(OsString::from(bin_dir.as_os_str()));
        cfg.crash_report_dir = Some(crash_dir.clone());
        cfg.app_launch_timeout = Duration::from_millis(180);
        cfg.crash_report_grace = Duration::from_millis(30);
        cfg.request_timeout = Duration::from_secs(30);
        let driver = IosDriver::with_mock_client_for_tests(
            cfg.clone(),
            mock.client_with_timeout(Duration::from_secs(30)),
        )
        .expect("driver");
        let cancel = CancellationToken::new();

        // Subcase A: No .ips report -> LaunchFailed promptly within launch budget (not 30s)
        // AND stalled `/warmup` invalidates the runner session so it is never reused busy.
        let t0 = Instant::now();
        let err = driver
            .launch_app("dev.deja.ios.checkout", &LaunchOptions::default(), &cancel)
            .await
            .expect_err("stalled warmup must fail");
        assert!(
            matches!(err, DriverError::LaunchFailed { .. }),
            "got {err:?}"
        );
        assert!(
            t0.elapsed() < Duration::from_millis(1_000),
            "stalled warmup overshot app_launch_timeout: {:?}",
            t0.elapsed()
        );
        assert!(
            !driver.has_active_session(),
            "stalled warmup timeout must invalidate active runner session"
        );

        // Subcase B: .ips written while warmup is stalled -> routes through crash check -> AppCrashed.
        let driver_b = IosDriver::with_mock_client_for_tests(
            cfg,
            mock.client_with_timeout(Duration::from_secs(30)),
        )
        .expect("driver_b");
        let dir_for_writer = crash_dir.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let content = format!(
                "{{\"app_name\":\"DejaFixture\",\"bundleID\":\"dev.deja.ios.checkout\"}}\n{{\"procName\":\"DejaFixture\",\"procPath\":\"/sim/Library/Developer/CoreSimulator/Devices/{TEST_UDID}/data/Containers/Bundle/Application/11111111-2222-3333-4444-555555555555/DejaFixture.app/DejaFixture\"}}\n"
            );
            std::fs::write(dir_for_writer.join("stalled-crash.ips"), content).unwrap();
        });
        let t1 = Instant::now();
        match driver_b
            .launch_app("dev.deja.ios.checkout", &LaunchOptions::default(), &cancel)
            .await
        {
            Err(DriverError::AppCrashed(sig)) => {
                assert_eq!(sig.report_file.as_deref(), Some("stalled-crash.ips"));
            }
            other => panic!("expected AppCrashed after stalled warmup, got {other:?}"),
        }
        writer.await.unwrap();
        assert!(
            t1.elapsed() < Duration::from_millis(1_000),
            "crash attribution after stalled warmup took {:?}",
            t1.elapsed()
        );
        assert!(!driver_b.has_active_session());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn check_app_crash_includes_reports_written_after_slow_app_state_probe() {
        let mock = MockAgent::start_async(Arc::new(|req| {
            Box::pin(async move {
                if req.target.starts_with("/appState") {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    (
                        200,
                        json!({
                            "appId": "dev.deja.ios.checkout",
                            "state": "not_running",
                            "stateCode": 1
                        })
                        .to_string(),
                    )
                } else {
                    (404, "{}".to_string())
                }
            })
        }))
        .await;

        let crash_dir = unique_temp_dir("crash-post-probe-grace");
        let mut cfg = IosDriverConfig::new(TEST_UDID);
        cfg.crash_report_dir = Some(crash_dir.clone());
        cfg.crash_report_grace = Duration::from_millis(350);
        let driver = IosDriver::with_mock_client_for_tests(cfg, mock.client()).expect("driver");
        let cancel = CancellationToken::new();
        let since = super::now_epoch_ms().saturating_sub(500);

        // Write the .ips file at t=450ms (AFTER initial `now + 350ms`, but well INSIDE
        // `grace_start + 350ms` after the 300ms `/appState` probe completes).
        let dir_for_writer = crash_dir.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(450)).await;
            let content = format!(
                "{{\"app_name\":\"DejaFixture\",\"bundleID\":\"dev.deja.ios.checkout\"}}\n{{\"procName\":\"DejaFixture\",\"procPath\":\"/sim/Library/Developer/CoreSimulator/Devices/{TEST_UDID}/data/Containers/Bundle/Application/11111111-2222-3333-4444-555555555555/DejaFixture.app/DejaFixture\"}}\n"
            );
            std::fs::write(dir_for_writer.join("late-grace.ips"), content).unwrap();
        });

        let found = driver
            .check_app_crash("dev.deja.ios.checkout", since, &cancel)
            .await
            .expect("check_app_crash")
            .expect("report written during post-probe grace period must be discovered");
        writer.await.unwrap();
        assert_eq!(found.report_file.as_deref(), Some("late-grace.ips"));

        let _ = std::fs::remove_dir_all(&crash_dir);
    }

    #[tokio::test]
    async fn shutdown_serializes_teardown_before_publishing_detached_and_replacement_start() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let mock = MockAgent::start(Arc::new(|_| (200, r#"{"ok":true}"#.to_string()))).await;
        let mut cfg = IosDriverConfig::new(TEST_UDID);
        cfg.auto_boot_simulator = false;
        cfg.disable_animations = false;
        // Point PATH to an empty directory so replacement startup cannot spawn real xcodebuild.
        let empty_bin = unique_temp_dir("empty-bin-serialize");
        cfg.env.env_path = Some(empty_bin.as_os_str().to_os_string());

        let teardown_done = Arc::new(AtomicBool::new(false));
        let mut raw_driver = IosDriver::with_mock_client_and_teardown_for_tests(
            cfg,
            mock.client(),
            Duration::from_millis(180),
            Some(Arc::clone(&teardown_done)),
        )
        .expect("driver");
        raw_driver.simulator_locks_dir = Some(empty_bin.join("locks"));
        let driver = Arc::new(raw_driver);

        let driver_a = Arc::clone(&driver);
        let shutdown_task = tokio::spawn(async move { driver_a.shutdown().await });

        // Wait until shutdown has taken the session and entered the 180ms teardown.
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(
            !teardown_done.load(Ordering::SeqCst),
            "teardown should still be in progress"
        );
        assert!(
            !driver.has_active_session(),
            "dying session must not be served during teardown"
        );
        assert!(
            !driver.is_detached(),
            "Detached must NOT be published before teardown completes"
        );

        // Concurrent replacement attempt must block on `lifecycle` until teardown completes.
        let driver_b = Arc::clone(&driver);
        let done_for_b = Arc::clone(&teardown_done);
        let replacement_task = tokio::spawn(async move {
            let cancel = CancellationToken::new();
            let res = driver_b.ensure_session(&cancel).await;
            let saw_teardown_done = done_for_b.load(Ordering::SeqCst);
            (res, saw_teardown_done)
        });

        shutdown_task.await.unwrap().expect("shutdown ok");
        assert!(teardown_done.load(Ordering::SeqCst));
        assert!(driver.is_detached());

        let (replace_res, saw_teardown_done) = replacement_task.await.unwrap();
        assert!(
            saw_teardown_done,
            "replacement ensure_session must wait until prior runner teardown finishes"
        );
        assert!(
            replace_res.is_err(),
            "replacement startup with empty PATH should fail cleanly after waiting for teardown"
        );

        let _ = std::fs::remove_dir_all(&empty_bin);
    }

    #[tokio::test]
    async fn act_and_observe_honors_post_action_delays_longer_than_5s_clamp() {
        let mock = MockAgent::start(Arc::new(|req| {
            if req.target.starts_with("/hierarchy") {
                (
                    200,
                    json!({
                        "appId": "dev.deja.ios.checkout",
                        "state": "running_foreground",
                        "stateCode": 4,
                        "truncated": false,
                        "hierarchyHash": "0123456789abcdef",
                        "unchanged": false,
                        "nodeCount": 1,
                        "nodes": [sample_hierarchy_node(0, "Application", "", "App")]
                    })
                    .to_string(),
                )
            } else {
                (200, r#"{"ok":true}"#.to_string())
            }
        }))
        .await;

        let mut driver =
            IosDriver::with_mock_client_for_tests(IosDriverConfig::new(TEST_UDID), mock.client())
                .expect("driver");

        // Subcase A: Real > 5,000 ms threshold bypasses `/act`, executes primitive `/tap`,
        // waits locally (cancellable), and does not call `/hierarchy` if cancelled during delay.
        let cancel_a = CancellationToken::new();
        let cancel_trigger = cancel_a.clone();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            cancel_trigger.cancel();
        });
        let t0 = Instant::now();
        let err = driver
            .act_and_observe(
                &ActPayload {
                    app_id: "dev.deja.ios.checkout".to_string(),
                    action: ActAction::Tap { x: 10.0, y: 20.0 },
                    post_action_delay_ms: Some(6_000),
                    return_hierarchy: true,
                    include_system_ui: false,
                    known_hash: None,
                },
                &cancel_a,
            )
            .await
            .expect_err("long local delay must honor cancellation");
        canceller.await.unwrap();
        assert!(matches!(err, DriverError::Cancelled));
        assert!(t0.elapsed() >= Duration::from_millis(100));
        let recorded_a = mock.recorded();
        assert!(recorded_a.iter().any(|r| r.target == "/tap"));
        assert!(
            !recorded_a.iter().any(|r| r.target == "/act"),
            "delay > 5,000 ms must bypass clamped /act endpoint"
        );
        assert!(
            !recorded_a
                .iter()
                .any(|r| r.target.starts_with("/hierarchy")),
            "cancelled delay must not fetch hierarchy"
        );

        // Subcase B: Full local delay completion + post-delay hierarchy fetch.
        driver.max_server_post_action_delay_ms = 50;
        let cancel_b = CancellationToken::new();
        let t1 = Instant::now();
        let outcome = driver
            .act_and_observe(
                &ActPayload {
                    app_id: "dev.deja.ios.checkout".to_string(),
                    action: ActAction::Tap { x: 15.0, y: 25.0 },
                    post_action_delay_ms: Some(120),
                    return_hierarchy: true,
                    include_system_ui: false,
                    known_hash: None,
                },
                &cancel_b,
            )
            .await
            .expect("full local delay + hierarchy capture");
        assert!(outcome.action_ok);
        assert!(outcome.hierarchy.is_some());
        assert!(outcome.hierarchy_error.is_none());
        assert!(
            t1.elapsed() >= Duration::from_millis(110),
            "expected full >=120ms delay, elapsed={:?}",
            t1.elapsed()
        );
        assert!(
            !mock.recorded().iter().any(|r| r.target == "/act"),
            "delay above server clamp must route through primitive action + local delay + /hierarchy"
        );
    }

    #[tokio::test]
    async fn cancelling_in_flight_rpc_quiesces_runner_before_returning_cancelled() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let mock = MockAgent::start_async(Arc::new(|req| {
            Box::pin(async move {
                if req.target == "/tap" {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                (200, r#"{"ok":true}"#.to_string())
            })
        }))
        .await;

        let empty_bin = unique_temp_dir("empty-bin-quiesce");
        let mut cfg = IosDriverConfig::new(TEST_UDID).with_disable_animations(false);
        cfg.auto_boot_simulator = false;
        cfg.env.env_path = Some(empty_bin.as_os_str().to_os_string());

        let teardown_done = Arc::new(AtomicBool::new(false));
        let mut driver = IosDriver::with_mock_client_and_teardown_for_tests(
            cfg,
            mock.client(),
            Duration::from_millis(120),
            Some(Arc::clone(&teardown_done)),
        )
        .expect("driver");
        driver.simulator_locks_dir = Some(empty_bin.join("locks"));

        let cancel = CancellationToken::new();
        let cancel_trigger = cancel.clone();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            cancel_trigger.cancel();
        });

        let res = driver
            .tap_point("dev.deja.ios.checkout", UiPoint::new(10.0, 20.0), &cancel)
            .await;
        canceller.await.unwrap();

        assert!(
            matches!(res, Err(DriverError::Cancelled)),
            "expected Cancelled, got {res:?}"
        );
        assert!(
            teardown_done.load(Ordering::SeqCst),
            "runner teardown must finish BEFORE returning Cancelled on an in-flight RPC"
        );
        assert!(
            driver.is_detached(),
            "driver must be detached after quiescing a cancelled in-flight RPC"
        );

        // Next call must start fresh instead of reusing the torn-down mock runner.
        let fresh_cancel = CancellationToken::new();
        let next_res = driver
            .tap_point(
                "dev.deja.ios.checkout",
                UiPoint::new(15.0, 25.0),
                &fresh_cancel,
            )
            .await;
        assert!(
            next_res.is_err(),
            "detached driver with empty PATH must attempt fresh startup rather than reusing stale mock"
        );

        let _ = std::fs::remove_dir_all(&empty_bin);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detached_destructive_calls_require_simulator_lock_and_fail_closed_when_busy() {
        use crate::process::acquire_simulator_lock_in_dir;
        use deja_flow::{LaunchOptions, OpenLinkOptions};
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt;

        let root = unique_temp_dir("sim-ownership");
        let locks_dir = root.join("locks");
        let bin_dir = root.join("bin");
        let log_file = root.join("simctl.log");
        let media_file = root.join("sample.png");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(&media_file, b"\x89PNG\r\n\x1a\n").unwrap();

        let xcrun_path = bin_dir.join("xcrun");
        std::fs::write(
            &xcrun_path,
            format!(
                "#!/bin/sh\necho \"$@\" >> \"{}\"\nexit 0\n",
                log_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&xcrun_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut cfg = IosDriverConfig::new(TEST_UDID);
        cfg.env.env_path = Some(OsString::from(bin_dir.as_os_str()));
        cfg.auto_boot_simulator = true;
        let mut driver = IosDriver::new(cfg).expect("driver");
        driver.simulator_locks_dir = Some(locks_dir.clone());
        let cancel = CancellationToken::new();

        // Another process/runner holds the exclusive simulator lease for TEST_UDID.
        let external_lock =
            acquire_simulator_lock_in_dir(&locks_dir, TEST_UDID).expect("external lock");

        let assert_busy = |label: &str, res: std::result::Result<(), DriverError>| match res {
            Err(DriverError::Transport {
                transient: false,
                detail,
                ..
            }) => {
                let lower = detail.to_ascii_lowercase();
                assert!(
                    lower.contains("already locked") || lower.contains("busy"),
                    "{label}: expected busy/locked detail, got `{detail}`"
                );
            }
            other => panic!("{label}: expected fail-closed Transport lock error, got {other:?}"),
        };

        assert_busy(
            "stop_app",
            driver.stop_app("dev.deja.ios.checkout", &cancel).await,
        );
        assert_busy(
            "kill_app",
            driver.kill_app("dev.deja.ios.checkout", &cancel).await,
        );
        assert_busy(
            "clear_state",
            driver.clear_state("dev.deja.ios.checkout", &cancel).await,
        );
        assert_busy("clear_keychain", driver.clear_keychain(&cancel).await);
        assert_busy(
            "open_link",
            driver
                .open_link(
                    None,
                    "https://example.com/cart",
                    &OpenLinkOptions::default(),
                    &cancel,
                )
                .await,
        );
        assert_busy(
            "add_media",
            driver
                .add_media(std::slice::from_ref(&media_file), &cancel)
                .await,
        );
        assert_busy(
            "launch_app",
            driver
                .launch_app("dev.deja.ios.checkout", &LaunchOptions::default(), &cancel)
                .await,
        );
        assert!(
            !log_file.exists(),
            "no simctl commands may run while another runner holds the simulator lock"
        );

        // Once the external owner releases the simulator lease, detached mutating calls succeed
        // and release their temporary lock immediately upon completion.
        drop(external_lock);
        driver
            .stop_app("dev.deja.ios.checkout", &cancel)
            .await
            .expect("stop_app when unowned");
        driver
            .kill_app("dev.deja.ios.checkout", &cancel)
            .await
            .expect("kill_app when unowned");
        driver
            .clear_keychain(&cancel)
            .await
            .expect("clear_keychain when unowned");
        driver
            .open_link(
                None,
                "https://example.com/cart",
                &OpenLinkOptions::default(),
                &cancel,
            )
            .await
            .expect("open_link when unowned");
        driver
            .add_media(std::slice::from_ref(&media_file), &cancel)
            .await
            .expect("add_media when unowned");

        let log_contents = std::fs::read_to_string(&log_file).expect("simctl log written");
        assert!(log_contents.contains("terminate"));
        assert!(log_contents.contains("keychain"));
        assert!(log_contents.contains("openurl"));
        assert!(log_contents.contains("addmedia"));

        // Temporary locks must be released cleanly so another runner can lock immediately.
        let reacquired =
            acquire_simulator_lock_in_dir(&locks_dir, TEST_UDID).expect("reacquire lock");
        drop(reacquired);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launch_app_passes_full_remaining_launch_budget_to_warmup() {
        use deja_flow::LaunchOptions;
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt;

        let root = unique_temp_dir("warmup-budget");
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let xcrun_path = bin_dir.join("xcrun");
        std::fs::write(
            &xcrun_path,
            "#!/bin/sh\nif [ \"$2\" = \"launch\" ]; then echo \"dev.deja.ios.checkout: 123\"; fi\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&xcrun_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mock = MockAgent::start_async(Arc::new(|req| {
            Box::pin(async move {
                if req.target == "/health" {
                    (
                        200,
                        json!({"status": "ok", "agent": "DejaXCTestRunner", "version": "1", "pid": 88}).to_string(),
                    )
                } else if req.target == "/warmup" {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    (
                        200,
                        json!({
                            "ok": true,
                            "appId": "dev.deja.ios.checkout",
                            "state": "running_foreground",
                            "stateCode": 4,
                            "elapsedMs": 40,
                            "nodeCount": 5,
                            "hierarchyHash": "0123456789abcdef"
                        })
                        .to_string(),
                    )
                } else {
                    (200, r#"{"ok":true}"#.to_string())
                }
            })
        }))
        .await;

        let mut cfg = IosDriverConfig::new(TEST_UDID);
        cfg.env.env_path = Some(OsString::from(bin_dir.as_os_str()));
        cfg.app_launch_timeout = Duration::from_secs(15);
        let driver = IosDriver::with_mock_client_for_tests(cfg, mock.client()).expect("driver");
        let cancel = CancellationToken::new();

        driver
            .launch_app("dev.deja.ios.checkout", &LaunchOptions::default(), &cancel)
            .await
            .expect("launch_app should succeed");

        let warmup_req = mock
            .recorded()
            .into_iter()
            .find(|r| r.target == "/warmup")
            .expect("expected POST /warmup");
        let sent_timeout_ms = warmup_req.body.as_ref().unwrap()["timeoutMs"]
            .as_u64()
            .expect("timeoutMs in /warmup request");
        assert!(
            (10_000..=15_000).contains(&sent_timeout_ms),
            "expected full remaining launch budget (~15_000ms, not capped at 1_800ms), got {sent_timeout_ms}ms"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launch_app_bounds_stalled_simctl_launch_with_app_launch_timeout() {
        use deja_flow::LaunchOptions;
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt;

        let root = unique_temp_dir("stalled-simctl-launch");
        let bin_dir = root.join("bin");
        let crash_dir = root.join("crashes");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&crash_dir).unwrap();

        let xcrun_path = bin_dir.join("xcrun");
        std::fs::write(
            &xcrun_path,
            "#!/bin/sh\nif [ \"$2\" = \"launch\" ]; then exec /bin/sleep 5; fi\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&xcrun_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mock = MockAgent::start(Arc::new(|req| {
            if req.target == "/health" {
                (
                    200,
                    json!({"status": "ok", "agent": "DejaXCTestRunner", "version": "1", "pid": 88})
                        .to_string(),
                )
            } else {
                (200, r#"{"ok":true}"#.to_string())
            }
        }))
        .await;

        let mut cfg = IosDriverConfig::new(TEST_UDID);
        cfg.env.env_path = Some(OsString::from(bin_dir.as_os_str()));
        cfg.crash_report_dir = Some(crash_dir);
        cfg.app_launch_timeout = Duration::from_millis(180);
        cfg.crash_report_grace = Duration::from_millis(20);
        let driver = IosDriver::with_mock_client_for_tests(cfg, mock.client()).expect("driver");
        let cancel = CancellationToken::new();

        let t0 = Instant::now();
        let err = driver
            .launch_app("dev.deja.ios.checkout", &LaunchOptions::default(), &cancel)
            .await
            .expect_err("stalled simctl launch must fail");
        match err {
            DriverError::LaunchFailed { app_id, detail } => {
                assert_eq!(app_id, "dev.deja.ios.checkout");
                assert!(
                    detail.contains("simctl launch"),
                    "unexpected detail: {detail}"
                );
            }
            other => panic!("expected LaunchFailed for stalled simctl launch, got {other:?}"),
        }
        assert!(
            t0.elapsed() < Duration::from_millis(1_000),
            "stalled simctl launch overshot app_launch_timeout: {:?}",
            t0.elapsed()
        );
        assert!(
            !mock.recorded().iter().any(|r| r.target == "/warmup"),
            "timed-out simctl launch must not proceed to /warmup"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn server_wait_for_selector_enforces_deadline_and_preserves_zero_timeout_probe() {
        use std::sync::atomic::{AtomicBool, Ordering};

        // Subcase A: positive selector timeout stalls on server -> aborts promptly around
        // `timeout_ms + 250ms` (NOT 30s), invalidates the stalled runner, and returns transient error.
        let stalled_mock = MockAgent::start_async(Arc::new(|req| {
            Box::pin(async move {
                if req.target == "/waitForSelector" {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                (200, r#"{"matched":false,"elapsedMs":5000,"polls":1,"hierarchyHash":"","visibleSample":[]}"#.to_string())
            })
        }))
        .await;

        let teardown_done = Arc::new(AtomicBool::new(false));
        let mut cfg = IosDriverConfig::new(TEST_UDID);
        cfg.request_timeout = Duration::from_secs(30);
        let stalled_driver = IosDriver::with_mock_client_and_teardown_for_tests(
            cfg,
            stalled_mock.client_with_timeout(Duration::from_secs(30)),
            Duration::from_millis(40),
            Some(Arc::clone(&teardown_done)),
        )
        .expect("stalled driver");
        let cancel = CancellationToken::new();

        let req_pos = WaitSelectorRequest::visible(
            "dev.deja.ios.checkout",
            Selector::by_text("Order confirmed"),
            Duration::from_millis(150),
        );
        let t0 = Instant::now();
        let err = stalled_driver
            .wait_for_selector(&req_pos, &cancel)
            .await
            .expect_err("stalled /waitForSelector must time out locally");
        assert!(
            matches!(
                err,
                DriverError::Transport {
                    transient: true,
                    ..
                }
            ),
            "expected transient Transport error, got {err:?}"
        );
        assert!(
            t0.elapsed() < Duration::from_millis(1_200),
            "stalled /waitForSelector overshot selector deadline: {:?}",
            t0.elapsed()
        );
        assert!(
            teardown_done.load(Ordering::SeqCst),
            "timed-out /waitForSelector must tear down the stalled runner before returning"
        );
        assert!(stalled_driver.is_detached());

        // Subcase B: `timeout == Duration::ZERO` preserves single-probe semantics (does NOT time
        // out at 0ms) and keeps the runner session healthy.
        let probe_mock = MockAgent::start_async(Arc::new(|req| {
            Box::pin(async move {
                if req.target == "/waitForSelector" {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    (
                        200,
                        json!({
                            "matched": true,
                            "elapsedMs": 60,
                            "polls": 1,
                            "hierarchyHash": "0123456789abcdef",
                            "visibleSample": [],
                            "node": sample_hierarchy_node(0, "StaticText", "order_ok", "Order confirmed")
                        })
                        .to_string(),
                    )
                } else {
                    (404, "{}".to_string())
                }
            })
        }))
        .await;

        let probe_driver = IosDriver::with_mock_client_for_tests(
            IosDriverConfig::new(TEST_UDID),
            probe_mock.client(),
        )
        .expect("probe driver");
        let req_zero = WaitSelectorRequest::visible(
            "dev.deja.ios.checkout",
            Selector::by_text("Order confirmed"),
            Duration::ZERO,
        );
        let outcome = probe_driver
            .wait_for_selector(&req_zero, &cancel)
            .await
            .expect("zero-timeout single probe should succeed");
        assert!(outcome.matched);
        assert_eq!(outcome.polls, 1);
        assert!(probe_driver.has_active_session());
        let sent = probe_mock
            .recorded()
            .into_iter()
            .find(|r| r.target == "/waitForSelector")
            .expect("recorded /waitForSelector");
        assert_eq!(sent.body.as_ref().unwrap()["timeoutMs"], json!(0));
    }

    #[tokio::test]
    async fn redacts_raw_whitespace_and_newline_secrets_before_normalizing_across_hierarchy_and_sample_paths()
     {
        use deja_flow::{FlowExecutionConfig, FlowExecutor};

        let secret = "my  secret\npayload";
        let secret_for_mock = secret.to_string();
        let mode = Arc::new(StdMutex::new(0u8));
        let mode_clone = Arc::clone(&mode);

        let mock = MockAgent::start(Arc::new(move |req| {
            if req.target == "/waitForSelector" {
                let m = *mode_clone.lock().unwrap();
                match m {
                    0 => {
                        // Both raw `hierarchy` AND a pre-collapsed `visibleSample` are present:
                        // `server_wait_for_selector` MUST prefer raw `hierarchy` so the secret
                        // containing repeated spaces + newline is redacted BEFORE normalization.
                        (
                            200,
                            json!({
                                "matched": false,
                                "elapsedMs": 20,
                                "polls": 2,
                                "hierarchyHash": "0123456789abcdef",
                                "visibleSample": ["Token my secret payload ok"],
                                "hierarchy": {
                                    "appId": "dev.deja.ios.checkout",
                                    "state": "running_foreground",
                                    "stateCode": 4,
                                    "truncated": false,
                                    "hierarchyHash": "0123456789abcdef",
                                    "unchanged": false,
                                    "nodeCount": 1,
                                    "nodes": [sample_hierarchy_node(0, "StaticText", "tok", &format!("Token {secret_for_mock} ok"))]
                                }
                            })
                            .to_string(),
                        )
                    }
                    _ => {
                        // Fallback path: no `hierarchy`, only raw uncollapsed `visibleSample`
                        // matching Swift's updated `extractVisibleSample`.
                        (
                            200,
                            json!({
                                "matched": false,
                                "elapsedMs": 20,
                                "polls": 2,
                                "hierarchyHash": "0123456789abcdef",
                                "visibleSample": [format!("Banner {secret_for_mock} footer")]
                            })
                            .to_string(),
                        )
                    }
                }
            } else if req.target == "/health" {
                (
                    200,
                    json!({
                        "status": "ok",
                        "agent": "DejaXCTestRunner",
                        "version": "1",
                        "pid": 88
                    })
                    .to_string(),
                )
            } else if req.target.starts_with("/appState") {
                (
                    200,
                    json!({
                        "appId": "dev.deja.ios.checkout",
                        "state": "running_foreground",
                        "stateCode": 4
                    })
                    .to_string(),
                )
            } else {
                (200, r#"{"ok":true}"#.to_string())
            }
        }))
        .await;

        let driver =
            IosDriver::with_mock_client_for_tests(IosDriverConfig::new(TEST_UDID), mock.client())
                .expect("driver");
        let cancel = CancellationToken::new();

        // Subcase A: raw hierarchy preferred over pre-collapsed visibleSample.
        let mut req_a = WaitSelectorRequest::visible(
            "dev.deja.ios.checkout",
            Selector::by_text("Missing"),
            Duration::from_millis(200),
        );
        req_a.redactor = Some(create_redactor("/tmp/ws", None, &[secret.to_string()]));
        req_a.provider_redactor = Some(provider_key_redactor(std::iter::empty::<(&str, &str)>()));
        let out_a = driver.wait_for_selector(&req_a, &cancel).await.unwrap();
        let sum_a = out_a.visible_context.format_summary();
        assert!(
            !sum_a.contains("secret") && !sum_a.contains("payload"),
            "raw hierarchy path leaked multi-space/newline secret: {sum_a}"
        );
        assert!(
            sum_a.contains("<redacted-fixture-value>")
                && sum_a.contains("Token")
                && sum_a.contains("ok"),
            "unexpected raw hierarchy summary: {sum_a}"
        );

        // Subcase B: fallback `visibleSample` path redacts raw text before normalization.
        *mode.lock().unwrap() = 1;
        let out_b = driver.wait_for_selector(&req_a, &cancel).await.unwrap();
        let sum_b = out_b.visible_context.format_summary();
        assert!(
            !sum_b.contains("secret") && !sum_b.contains("payload"),
            "visibleSample fallback leaked multi-space/newline secret: {sum_b}"
        );
        assert!(
            sum_b.contains("<redacted-fixture-value>")
                && sum_b.contains("Banner")
                && sum_b.contains("footer"),
            "unexpected visibleSample summary: {sum_b}"
        );

        // Subcase C: end-to-end FlowExecutor assertion failure message + VisibleContext redaction.
        let ws = unique_temp_dir("redact-flow");
        let mut exec_cfg = FlowExecutionConfig::new(ws.clone());
        exec_cfg.default_step_timeout = Duration::from_millis(120);
        exec_cfg.redactor =
            create_redactor(ws.to_string_lossy().as_ref(), None, &[secret.to_string()]);
        let executor = FlowExecutor::new(exec_cfg);
        let outcome = executor
            .execute_yaml(
                &driver,
                "appId: dev.deja.ios.checkout\n---\n- assertVisible: Not present\n",
                &cancel,
            )
            .await;
        assert!(!outcome.passed);
        let failed = outcome.failed_step.expect("failed step");
        assert!(
            !failed.message.contains("secret") && !failed.message.contains("payload"),
            "assertion failure message leaked secret: {}",
            failed.message
        );
        let vis = failed
            .visible_context
            .expect("visible context")
            .format_summary();
        assert!(
            !vis.contains("secret") && !vis.contains("payload"),
            "assertion VisibleContext leaked secret: {vis}"
        );
        assert!(vis.contains("<redacted-fixture-value>"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn dropped_hierarchy_rpc_marks_session_dirty_and_quiesces_runner_on_next_call() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let mock = MockAgent::start_async(Arc::new(|req| {
            Box::pin(async move {
                if req.target == "/health" {
                    // XCTest still answers `/health` 200 OK while `busyLock` is stuck on a
                    // dropped `/hierarchy` request.
                    (
                        200,
                        json!({"status": "ok", "agent": "DejaXCTestRunner", "version": "1", "pid": 88})
                            .to_string(),
                    )
                } else if req.target.starts_with("/hierarchy") {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    (409, r#"{"error":"runner busy"}"#.to_string())
                } else {
                    (200, r#"{"ok":true}"#.to_string())
                }
            })
        }))
        .await;

        let empty_bin = unique_temp_dir("empty-bin-drop-quiesce");
        let mut cfg = IosDriverConfig::new(TEST_UDID).with_disable_animations(false);
        cfg.auto_boot_simulator = false;
        cfg.env.env_path = Some(empty_bin.as_os_str().to_os_string());

        let teardown_done = Arc::new(AtomicBool::new(false));
        let mut driver = IosDriver::with_mock_client_and_teardown_for_tests(
            cfg,
            mock.client(),
            Duration::from_millis(50),
            Some(Arc::clone(&teardown_done)),
        )
        .expect("driver");
        driver.simulator_locks_dir = Some(empty_bin.join("locks"));
        let cancel = CancellationToken::new();

        // Simulate `deja-flow`'s local selector timeout dropping `driver.hierarchy(...)` directly
        // without signalling `cancel`.
        let timed = tokio::time::timeout(
            Duration::from_millis(60),
            driver.hierarchy("dev.deja.ios.checkout", false, None, &cancel),
        )
        .await;
        assert!(
            timed.is_err(),
            "outer timeout should drop /hierarchy future"
        );
        assert!(
            driver.session_needs_quiesce(),
            "dropping an in-flight RPC future must synchronously mark the session dirty"
        );
        assert!(
            !teardown_done.load(Ordering::SeqCst),
            "teardown runs asynchronously on the next lifecycle entry"
        );

        let hierarchies_before = mock
            .recorded()
            .iter()
            .filter(|r| r.target.starts_with("/hierarchy"))
            .count();

        // Next call must tear down the dirty runner rather than reusing it via `/health` 200.
        let next_res = driver
            .hierarchy("dev.deja.ios.checkout", false, None, &cancel)
            .await;
        assert!(
            next_res.is_err(),
            "replacement startup with empty PATH should fail after tearing down dirty runner"
        );
        assert!(
            teardown_done.load(Ordering::SeqCst),
            "next driver call must tear down dirty runner before attempting reuse"
        );
        assert!(driver.is_detached());
        assert_eq!(
            mock.recorded()
                .iter()
                .filter(|r| r.target.starts_with("/hierarchy"))
                .count(),
            hierarchies_before,
            "dirty runner must NOT receive another /hierarchy request"
        );

        let _ = std::fs::remove_dir_all(&empty_bin);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn aborted_clear_state_signals_cancel_retains_simulator_lock_and_blocks_subsequent_ops() {
        use super::ClearStateTestHook;
        use crate::error::IosDriverError;
        use crate::process::acquire_simulator_lock_in_dir;
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicBool, Ordering};

        let root = unique_temp_dir("aborted-wipe-ownership");
        let locks_dir = root.join("locks");
        let bin_dir = root.join("bin");
        let log_file = root.join("simctl.log");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let container = root
            .join("CoreSimulator/Devices")
            .join(TEST_UDID)
            .join("data/Containers/Data/Application/99999999-2222-3333-4444-555555555555");
        std::fs::create_dir_all(container.join("Documents")).unwrap();
        std::fs::write(container.join("Documents/keep_if_cancelled.txt"), b"data").unwrap();

        let xcrun_path = bin_dir.join("xcrun");
        std::fs::write(
            &xcrun_path,
            format!(
                r#"#!/bin/sh
echo "$@" >> "{log}"
if [ "$2" = "get_app_container" ] && [ "$5" = "data" ]; then
  echo "{container}"
  exit 0
fi
exit 0
"#,
                log = log_file.display(),
                container = container.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&xcrun_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        struct ReleaseOnDrop(Arc<AtomicBool>);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let worker_entered = Arc::new(AtomicBool::new(false));
        let worker_entered_notify = Arc::new(tokio::sync::Notify::new());
        let release_worker = Arc::new(AtomicBool::new(false));
        let _release_guard = ReleaseOnDrop(Arc::clone(&release_worker));
        let worker_saw_cancel = Arc::new(AtomicBool::new(false));
        let worker_saw_cancel_notify = Arc::new(tokio::sync::Notify::new());

        let entered_clone = Arc::clone(&worker_entered);
        let notify_clone = Arc::clone(&worker_entered_notify);
        let release_clone = Arc::clone(&release_worker);
        let saw_cancel_clone = Arc::clone(&worker_saw_cancel);
        let saw_cancel_notify_clone = Arc::clone(&worker_saw_cancel_notify);

        let mut cfg = IosDriverConfig::new(TEST_UDID);
        cfg.env.env_path = Some(OsString::from(bin_dir.as_os_str()));
        let mut driver = IosDriver::new(cfg).expect("driver");
        driver.simulator_locks_dir = Some(locks_dir.clone());
        driver.on_clear_state_worker = Some(ClearStateTestHook(Arc::new(move |cancel_flag| {
            entered_clone.store(true, Ordering::SeqCst);
            notify_clone.notify_one();
            let worker_deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut notified_cancel = false;
            while !release_clone.load(Ordering::SeqCst)
                && std::time::Instant::now() < worker_deadline
            {
                if cancel_flag.load(Ordering::SeqCst) {
                    saw_cancel_clone.store(true, Ordering::SeqCst);
                    if !notified_cancel {
                        notified_cancel = true;
                        saw_cancel_notify_clone.notify_one();
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            if cancel_flag.load(Ordering::SeqCst) {
                saw_cancel_clone.store(true, Ordering::SeqCst);
                if !notified_cancel {
                    saw_cancel_notify_clone.notify_one();
                }
            }
        })));
        let driver = Arc::new(driver);
        let cancel = CancellationToken::new();

        // Deterministically drop the `clear_state` future mid-wipe right after the blocking worker
        // has entered (simulating an outer `tokio::time::timeout` dropping the future).
        tokio::select! {
            biased;
            _ = worker_entered_notify.notified() => {}
            res = driver.clear_state("dev.deja.ios.checkout", &cancel) => {
                panic!("clear_state unexpectedly completed before abort: {res:?}");
            }
        }
        assert!(
            worker_entered.load(Ordering::SeqCst),
            "blocking wipe worker should have entered before outer drop"
        );

        // Wait deterministically (bounded) for the spinning worker to observe `cancel_flag == true`
        // written synchronously by `ActiveWipeGuard::drop`.
        if !worker_saw_cancel.load(Ordering::SeqCst) {
            tokio::time::timeout(Duration::from_secs(2), worker_saw_cancel_notify.notified())
                .await
                .expect("dropping clear_state future must synchronously signal cancel_flag");
        }
        assert!(
            worker_saw_cancel.load(Ordering::SeqCst),
            "dropping clear_state future must synchronously signal cancel_flag"
        );

        // 1. OS simulator advisory lock MUST still be held by the running wipe worker.
        assert!(
            matches!(
                acquire_simulator_lock_in_dir(&locks_dir, TEST_UDID),
                Err(IosDriverError::SimulatorBusy { .. })
            ),
            "simulator lock must remain held until the aborted wipe worker exits"
        );

        // 2. A subsequent operation on the same IosDriver MUST wait for the aborted wipe worker.
        let log_before_stop = std::fs::read_to_string(&log_file).unwrap_or_default();
        let driver_for_stop = Arc::clone(&driver);
        let cancel_for_stop = cancel.clone();
        let stop_task = tokio::spawn(async move {
            driver_for_stop
                .stop_app("dev.deja.ios.checkout", &cancel_for_stop)
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let log_while_blocked = std::fs::read_to_string(&log_file).unwrap_or_default();
        assert_eq!(
            log_before_stop, log_while_blocked,
            "subsequent stop_app must not execute while aborted wipe worker is still active"
        );

        // 3. Let the aborted wipe worker finish; stop_app completes and lock releases cleanly.
        release_worker.store(true, Ordering::SeqCst);
        stop_task
            .await
            .unwrap()
            .expect("stop_app succeeds after aborted wipe exits");
        assert!(
            container.join("Documents/keep_if_cancelled.txt").exists(),
            "cancelled wipe should not have deleted files after cancel_flag was signaled"
        );
        let reacquired = acquire_simulator_lock_in_dir(&locks_dir, TEST_UDID)
            .expect("simulator lock must be released once aborted wipe and stop_app finish");
        drop(reacquired);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stalled_simulator_boot_respects_startup_timeout_and_releases_lock() {
        use crate::process::acquire_simulator_lock_in_dir;
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt;

        let root = unique_temp_dir("stalled-boot-timeout");
        let locks_dir = root.join("locks");
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let xcrun_path = bin_dir.join("xcrun");
        std::fs::write(
            &xcrun_path,
            "#!/bin/sh\nif [ \"$2\" = \"bootstatus\" ]; then exec /bin/sleep 5; fi\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&xcrun_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut cfg = IosDriverConfig::new(TEST_UDID);
        cfg.env.env_path = Some(OsString::from(bin_dir.as_os_str()));
        cfg.startup_timeout = Duration::from_millis(180);
        cfg.auto_boot_simulator = true;
        let mut driver = IosDriver::new(cfg).expect("driver");
        driver.simulator_locks_dir = Some(locks_dir.clone());
        let cancel = CancellationToken::new();

        let t0 = Instant::now();
        let err = driver
            .ensure_session(&cancel)
            .await
            .expect_err("stalled simulator boot must time out");
        match err {
            DriverError::Timeout { operation, .. } => assert_eq!(operation, "ensure_booted"),
            other => panic!("expected Timeout(ensure_booted), got {other:?}"),
        }
        assert!(
            t0.elapsed() < Duration::from_millis(1_000),
            "stalled simulator boot overshot startup_timeout: {:?}",
            t0.elapsed()
        );
        let lock = acquire_simulator_lock_in_dir(&locks_dir, TEST_UDID)
            .expect("simulator lock must be released after boot timeout");
        drop(lock);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launch_app_applies_default_all_allow_when_permissions_omitted_and_skips_when_empty() {
        use deja_flow::LaunchOptions;
        use std::collections::BTreeMap;
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt;

        let root = unique_temp_dir("launch-default-perms");
        let bin_dir = root.join("bin");
        let log_file = root.join("simctl.log");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let xcrun_path = bin_dir.join("xcrun");
        std::fs::write(
            &xcrun_path,
            format!(
                r#"#!/bin/sh
echo "$@" >> "{log}"
if [ "$2" = "launch" ]; then echo "dev.deja.ios.checkout: 555"; fi
exit 0
"#,
                log = log_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&xcrun_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mock = MockAgent::start(Arc::new(|req| {
            if req.target == "/health" {
                (
                    200,
                    json!({"status": "ok", "agent": "DejaXCTestRunner", "version": "1", "pid": 88})
                        .to_string(),
                )
            } else if req.target == "/warmup" {
                (
                    200,
                    json!({
                        "ok": true,
                        "appId": "dev.deja.ios.checkout",
                        "state": "running_foreground",
                        "stateCode": 4,
                        "elapsedMs": 10,
                        "nodeCount": 2
                    })
                    .to_string(),
                )
            } else {
                (200, r#"{"ok":true}"#.to_string())
            }
        }))
        .await;

        let mut cfg = IosDriverConfig::new(TEST_UDID);
        cfg.env.env_path = Some(OsString::from(bin_dir.as_os_str()));
        let driver = IosDriver::with_mock_client_for_tests(cfg, mock.client()).expect("driver");
        let cancel = CancellationToken::new();

        // 1. Omitted (`permissions: None`) -> grants Maestro's default `all: allow`.
        driver
            .launch_app("dev.deja.ios.checkout", &LaunchOptions::default(), &cancel)
            .await
            .expect("launch with default permissions");
        let log_a = std::fs::read_to_string(&log_file).unwrap();
        assert!(
            log_a.contains(&format!(
                "simctl privacy {TEST_UDID} grant all dev.deja.ios.checkout"
            )),
            "omitted permissions must apply Maestro default `grant all`, log:\n{log_a}"
        );

        // 2. Explicitly empty (`permissions: Some({})`) -> zero `privacy` calls.
        std::fs::write(&log_file, "").unwrap();
        let explicit_empty = LaunchOptions {
            permissions: Some(BTreeMap::new()),
            ..LaunchOptions::default()
        };
        driver
            .launch_app("dev.deja.ios.checkout", &explicit_empty, &cancel)
            .await
            .expect("launch with explicit empty permissions");
        let log_b = std::fs::read_to_string(&log_file).unwrap();
        assert!(
            !log_b.contains("privacy"),
            "explicit empty `permissions: {{}}` must make zero privacy calls, log:\n{log_b}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn disable_animations_sets_reduce_motion_by_default_and_supports_opt_out() {
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt;

        let root = unique_temp_dir("disable-animations-attach");
        let locks_dir = root.join("locks");
        let bin_dir = root.join("bin");
        let log_file = root.join("simctl.log");
        std::fs::create_dir_all(&bin_dir).unwrap();

        // Stub `xcrun` records all simctl invocations and succeeds; `xcodebuild` is absent so
        // `ensure_session` stops cleanly right after simulator boot + ReduceMotionEnabled setup.
        let xcrun_path = bin_dir.join("xcrun");
        std::fs::write(
            &xcrun_path,
            format!(
                "#!/bin/sh\necho \"$@\" >> \"{}\"\nexit 0\n",
                log_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&xcrun_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cancel = CancellationToken::new();

        // 1. Default (`disable_animations: true`): advertises DisableAnimations and writes
        //    `ReduceMotionEnabled -int 1` on attach.
        let mut default_cfg = IosDriverConfig::new(TEST_UDID);
        default_cfg.env.env_path = Some(OsString::from(bin_dir.as_os_str()));
        default_cfg.deja_home = Some(root.join("deja-home-default"));
        let mut default_driver = IosDriver::new(default_cfg).expect("default driver");
        default_driver.simulator_locks_dir = Some(locks_dir.clone());
        assert!(
            default_driver
                .capabilities()
                .supports(DriverCapability::DisableAnimations)
        );
        let _ = default_driver.ensure_session(&cancel).await;
        let log_default = std::fs::read_to_string(&log_file).unwrap_or_default();
        assert!(
            log_default.contains(&format!(
                "simctl spawn {TEST_UDID} defaults write com.apple.Accessibility ReduceMotionEnabled -int 1"
            )),
            "expected ReduceMotionEnabled=1 on default attach, got:\n{log_default}"
        );

        // 2. Opt-out (`disable_animations: false`): omits DisableAnimations and makes ZERO
        //    `defaults write` / `ReduceMotionEnabled` calls.
        std::fs::write(&log_file, "").unwrap();
        let mut opt_out_cfg = IosDriverConfig::new(TEST_UDID).with_disable_animations(false);
        opt_out_cfg.env.env_path = Some(OsString::from(bin_dir.as_os_str()));
        opt_out_cfg.deja_home = Some(root.join("deja-home-optout"));
        let mut opt_out_driver = IosDriver::new(opt_out_cfg).expect("opt-out driver");
        opt_out_driver.simulator_locks_dir = Some(locks_dir);
        assert!(
            !opt_out_driver
                .capabilities()
                .supports(DriverCapability::DisableAnimations)
        );
        let _ = opt_out_driver.ensure_session(&cancel).await;
        let log_opt_out = std::fs::read_to_string(&log_file).unwrap_or_default();
        assert!(
            !log_opt_out.contains("ReduceMotionEnabled") && !log_opt_out.contains("defaults write"),
            "opt-out must not write ReduceMotionEnabled, got:\n{log_opt_out}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
