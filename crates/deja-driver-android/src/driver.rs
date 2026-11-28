//! Production native Android [`PlatformDriver`] implementation (`AndroidDriver`, `DEJA-68` + `DEJA-85`).
//!
//! Combines `adb` device pinning (`ro.serialno`), rotation-aware display bounds, cached
//! wake/keyguard lock guard, `AnimationScaleGuard`, per-device advisory locking, warm
//! `deja-android-agent` UiAutomator2 HTTP sessions over `adb forward`, compound `/act` with
//! hold-before-lift zero-fling swipe, adaptive `/waitForSelector`, tree-busy circuit breaker,
//! and scoped `logcat -b crash` crash detection.

use std::collections::BTreeMap;
use std::future::Future;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex as StdMutex, MutexGuard as StdMutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deja_core::provider_key_redactor;
use deja_flow::{
    ActAction, ActOutcome, ActPayload, DriverCapabilities, DriverCapability, DriverCrashSignal,
    DriverError, DriverFuture, KeyPress, LaunchOptions, ObservationSource, OpenLinkOptions,
    PlatformDriver, SwipeParams, UiHierarchy, UiNode, UiPoint, UiRect, UiSize,
    UnsupportedCapability, VisibleContext, WaitSelectorOutcome, WaitSelectorRequest,
    build_visible_context, wait_local,
};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::adb::{Adb, AdbForwardGuard, AnimationScaleGuard, validate_package_id};
use crate::cache::{resolve_deja_home, resolve_or_build_agent_apk};
use crate::client::{
    ActKind, ActRequest, AgentClient, HierarchyNode, HierarchyRequest, HierarchyResponse,
    InFlightRpcGuard, TextMatchMode, WaitForSelectorRequest,
};
use crate::crash::{find_android_crash_with_grace, parse_crash_logcat};
use crate::error::{AndroidDriverError, Result};
#[cfg(test)]
use crate::process::acquire_device_lock_in_dir;
use crate::process::{
    AGENT_PACKAGE_ID, DEFAULT_DEVICE_PORT, DeviceLockGuard, ManagedInstrumentation,
    acquire_device_lock, validate_serial,
};

const MAX_KEY_BATCH: u32 = 100;
const MAX_ERASE_BATCH: u32 = 200;
const DEFAULT_LONG_PRESS_MS: u64 = 1_000;
const MAX_SERVER_WAIT_TIMEOUT: Duration = Duration::from_secs(60);
const SERVER_WAIT_GRACE_MS: u64 = 500;
const ZERO_TIMEOUT_SINGLE_PROBE_CAP: Duration = Duration::from_secs(10);
const MAX_SERVER_POST_ACTION_DELAY_MS: u64 = 5_000;
const DEFAULT_TREE_BUSY_THRESHOLD: u32 = 5;
const DEFAULT_WAKE_CACHE_TTL: Duration = Duration::from_secs(2);
const KEEP_AWAKE_MIN_INTERVAL: Duration = Duration::from_secs(15);

#[cfg(not(unix))]
static TOKEN_NONCE: AtomicU64 = AtomicU64::new(0);

/// Configuration for [`AndroidDriver`].
#[derive(Debug, Clone)]
pub struct AndroidDriverConfig {
    pub serial: String,
    pub deja_home: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub device_port: u16,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub app_launch_timeout: Duration,
    pub crash_report_grace: Duration,
    /// When `true` (default), zeroes Android global animation scales on attach, restores the
    /// exact originals on shutdown/drop, and advertises [`DriverCapability::DisableAnimations`].
    pub disable_animations: bool,
    /// Opt-in non-mutating `input keyevent 0` wake-timer keep-alive (`false` by default).
    pub keep_awake: bool,
    /// Maximum consecutive `409`/empty-tree responses before opening the tree-busy circuit breaker.
    pub tree_busy_threshold: u32,
    /// TTL for cached wakefulness + keyguard lock-screen checks (`DEJA-85`, default 2s).
    pub wake_cache_ttl: Duration,
}

impl AndroidDriverConfig {
    pub fn new(serial: impl Into<String>) -> Self {
        Self {
            serial: serial.into(),
            deja_home: None,
            env: BTreeMap::new(),
            device_port: DEFAULT_DEVICE_PORT,
            startup_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(30),
            app_launch_timeout: Duration::from_secs(20),
            crash_report_grace: Duration::from_secs(2),
            disable_animations: true,
            keep_awake: false,
            tree_busy_threshold: DEFAULT_TREE_BUSY_THRESHOLD,
            wake_cache_ttl: DEFAULT_WAKE_CACHE_TTL,
        }
    }

    #[must_use]
    pub fn with_disable_animations(mut self, enabled: bool) -> Self {
        self.disable_animations = enabled;
        self
    }

    #[must_use]
    pub fn with_keep_awake(mut self, enabled: bool) -> Self {
        self.keep_awake = enabled;
        self
    }

    #[must_use]
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }
}

#[derive(Debug, Clone)]
struct SessionHandle {
    client: AgentClient,
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

#[derive(Debug)]
struct LiveSession {
    client: AgentClient,
    dirty: Arc<AtomicBool>,
    instrumentation: ManagedInstrumentation,
    forward: AdbForwardGuard,
    animation_guard: Option<AnimationScaleGuard>,
    _device_lock: DeviceLockGuard,
}

impl LiveSession {
    async fn shutdown(mut self) {
        let _ = self.client.shutdown().await;
        self.instrumentation.shutdown().await;
        self.forward.remove().await;
        if let Some(mut anim) = self.animation_guard.take() {
            anim.restore().await;
        }
        // `_device_lock` drops last, after instrumentation, port forward, and animation restore complete.
    }
}

#[derive(Debug)]
enum SessionState {
    Detached,
    Stopping,
    Live(Box<LiveSession>),
    #[cfg(test)]
    Mock {
        client: AgentClient,
        dirty: Arc<AtomicBool>,
        teardown_done: Option<Arc<AtomicBool>>,
    },
}

impl SessionState {
    fn handle(&self) -> Option<SessionHandle> {
        match self {
            Self::Detached | Self::Stopping => None,
            Self::Live(live) => Some(SessionHandle {
                client: live.client.clone(),
                dirty: Arc::clone(&live.dirty),
            }),
            #[cfg(test)]
            Self::Mock { client, dirty, .. } => Some(SessionHandle {
                client: client.clone(),
                dirty: Arc::clone(dirty),
            }),
        }
    }
}

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

async fn teardown_session(taken: SessionState) {
    match taken {
        SessionState::Live(live) => live.shutdown().await,
        SessionState::Detached | SessionState::Stopping => {}
        #[cfg(test)]
        SessionState::Mock { teardown_done, .. } => {
            if let Some(flag) = teardown_done {
                flag.store(true, Ordering::SeqCst);
            }
        }
    }
}

struct DeviceOwnershipLease<'a> {
    _temp_lock: Option<DeviceLockGuard>,
    _lifecycle: tokio::sync::MutexGuard<'a, ()>,
}

/// Production native Android [`PlatformDriver`].
#[derive(Debug)]
pub struct AndroidDriver {
    serial: String,
    config: AndroidDriverConfig,
    capabilities: DriverCapabilities,
    adb: StdMutex<Option<Adb>>,
    session: StdMutex<SessionState>,
    lifecycle: AsyncMutex<()>,
    attach_count: AtomicU64,
    agent_pid: AtomicU32,
    consecutive_tree_busy: AtomicU32,
    last_wake_check: StdMutex<Option<Instant>>,
    last_keep_awake: StdMutex<Option<Instant>>,
    installed_agent_sha: StdMutex<Option<String>>,
    #[cfg(test)]
    device_locks_dir: Option<PathBuf>,
}

impl AndroidDriver {
    /// Creates a new [`AndroidDriver`] bound to a validated Android device serial.
    ///
    /// Does not invoke `adb` or launch instrumentation until [`PlatformDriver::ensure_session`]
    /// or a device operation is called.
    pub fn new(config: AndroidDriverConfig) -> Result<Self> {
        let serial = validate_serial(&config.serial)?;
        let capabilities = android_capabilities(config.disable_animations);
        Ok(Self {
            serial,
            config,
            capabilities,
            adb: StdMutex::new(None),
            session: StdMutex::new(SessionState::Detached),
            lifecycle: AsyncMutex::new(()),
            attach_count: AtomicU64::new(0),
            agent_pid: AtomicU32::new(0),
            consecutive_tree_busy: AtomicU32::new(0),
            last_wake_check: StdMutex::new(None),
            last_keep_awake: StdMutex::new(None),
            installed_agent_sha: StdMutex::new(None),
            #[cfg(test)]
            device_locks_dir: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_mock_client_for_tests(
        config: AndroidDriverConfig,
        client: AgentClient,
        adb: Option<Adb>,
        teardown_done: Option<Arc<AtomicBool>>,
    ) -> Result<Self> {
        let driver = Self::new(config)?;
        if let Some(a) = adb {
            *driver.adb.lock().unwrap_or_else(|p| p.into_inner()) = Some(a);
        }
        driver.attach_count.store(1, Ordering::SeqCst);
        let mut guard = driver.lock_session();
        *guard = SessionState::Mock {
            client,
            dirty: Arc::new(AtomicBool::new(false)),
            teardown_done,
        };
        drop(guard);
        Ok(driver)
    }

    #[must_use]
    pub fn serial(&self) -> &str {
        &self.serial
    }

    #[must_use]
    pub fn attach_count(&self) -> u64 {
        self.attach_count.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn agent_pid(&self) -> Option<u32> {
        let pid = self.agent_pid.load(Ordering::SeqCst);
        (pid != 0).then_some(pid)
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

    fn lock_session(&self) -> StdMutexGuard<'_, SessionState> {
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn resolve_adb(&self) -> Result<Adb> {
        let mut slot = self.adb.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(ref existing) = *slot {
            return Ok(existing.clone());
        }
        let discovered = Adb::discover(self.config.env.clone())?;
        *slot = Some(discovered.clone());
        Ok(discovered)
    }

    fn acquire_device_lock_now(&self) -> Result<DeviceLockGuard> {
        #[cfg(test)]
        if let Some(ref dir) = self.device_locks_dir {
            return acquire_device_lock_in_dir(dir, &self.serial);
        }
        acquire_device_lock(&self.serial, &self.config.env)
    }

    async fn check_wake_and_lock_cached(
        &self,
        adb: &Adb,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if !self.config.wake_cache_ttl.is_zero() {
            let guard = self
                .last_wake_check
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(last) = *guard
                && last.elapsed() < self.config.wake_cache_ttl
            {
                return Ok(());
            }
        }
        let res = adb.ensure_awake_and_unlocked(&self.serial, cancel).await;
        match res {
            Ok(()) => {
                *self
                    .last_wake_check
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
                Ok(())
            }
            Err(err) => {
                *self
                    .last_wake_check
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = None;
                Err(err)
            }
        }
    }

    async fn maybe_keep_awake(&self, cancel: &CancellationToken) {
        if !self.config.keep_awake || cancel.is_cancelled() {
            return;
        }
        let should_ping = {
            let mut slot = self
                .last_keep_awake
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if slot.is_none_or(|last| last.elapsed() >= KEEP_AWAKE_MIN_INTERVAL) {
                *slot = Some(Instant::now());
                true
            } else {
                false
            }
        };
        if should_ping && let Ok(adb) = self.resolve_adb() {
            let _ = adb.keep_awake_ping(&self.serial, cancel).await;
        }
    }

    fn reset_tree_busy(&self) {
        self.consecutive_tree_busy.store(0, Ordering::SeqCst);
    }

    fn record_tree_busy(
        &self,
        handle: &SessionHandle,
        operation: &str,
        reason: &str,
    ) -> std::result::Result<(), DriverError> {
        let count = self.consecutive_tree_busy.fetch_add(1, Ordering::SeqCst) + 1;
        let threshold = self.config.tree_busy_threshold.max(1);
        if count >= threshold {
            handle.mark_dirty();
            return Err(DriverError::Transport {
                operation: operation.to_string(),
                detail: AndroidDriverError::TreeBusyCircuitOpen {
                    consecutive: count,
                    detail: reason.to_string(),
                }
                .to_string(),
                transient: false,
            });
        }
        Ok(())
    }

    async fn ensure_session_handle(
        &self,
        cancel: &CancellationToken,
    ) -> std::result::Result<SessionHandle, DriverError> {
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }

        // Fast path: reuse warm healthy session verified via `/health` `tokenDigest`.
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
                Ok(h)
                    if h.status == "ok"
                        && (h.token_digest.is_empty()
                            || h.token_digest == handle.client.expected_token_digest()) =>
                {
                    if !handle.is_dirty() {
                        self.agent_pid.store(h.pid, Ordering::SeqCst);
                        return Ok(handle.clone());
                    }
                }
                Ok(h) => {
                    debug!(
                        serial = %self.serial,
                        status = %h.status,
                        token_digest = %h.token_digest,
                        "warm Android agent failed health or tokenDigest check; recycling session"
                    );
                }
                Err(err) => {
                    debug!(
                        serial = %self.serial,
                        error = %err,
                        "warm Android agent health probe failed; recycling session"
                    );
                }
            }
        }

        let _lifecycle = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(DriverError::Cancelled),
            guard = self.lifecycle.lock() => guard,
        };
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }

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
                    && (h.token_digest.is_empty()
                        || h.token_digest == handle.client.expected_token_digest())
                    && !handle.is_dirty()
                {
                    self.agent_pid.store(h.pid, Ordering::SeqCst);
                    return Ok(handle);
                }
            }
            self.invalidate_session_locked(handle.client.base_url())
                .await;
        }

        let adb = self
            .resolve_adb()
            .map_err(|err| map_android_err("ensure_session", err))?;

        // 1. Fail-closed target device & `ro.serialno` verification.
        adb.resolve_target_device(&self.serial, cancel)
            .await
            .map_err(|err| map_android_err("ensure_session", err))?;

        // 2. Exclusive per-device advisory lock (`O_NOFOLLOW`).
        let device_lock = self
            .acquire_device_lock_now()
            .map_err(|err| map_android_err("ensure_session", err))?;

        // 3. Cached wake & keyguard lock-screen guard (`DEJA-85`).
        self.check_wake_and_lock_cached(&adb, cancel)
            .await
            .map_err(|err| map_android_err("ensure_session", err))?;

        // 4. Optional animation scale guard (`disable_animations == true` by default).
        let mut animation_guard = if self.config.disable_animations {
            Some(
                AnimationScaleGuard::disable_animations(&adb, &self.serial, cancel)
                    .await
                    .map_err(|err| map_android_err("disable_animations", err))?,
            )
        } else {
            None
        };

        // 5. Resolve or compile `deja-android-agent.apk` and ensure installed on device.
        let deja_home = resolve_deja_home(self.config.deja_home.as_deref(), &self.config.env)
            .map_err(|err| map_android_err("resolve_deja_home", err))?;
        let apk = match resolve_or_build_agent_apk(&deja_home, &self.config.env, cancel).await {
            Ok(apk) => apk,
            Err(err) => {
                if let Some(ref mut anim) = animation_guard {
                    anim.restore().await;
                }
                return Err(map_android_err("resolve_agent_apk", err));
            }
        };

        let needs_install = {
            let last_sha = self
                .installed_agent_sha
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            if last_sha.as_deref() == Some(apk.sha256.as_str()) {
                !adb.is_package_installed(&self.serial, AGENT_PACKAGE_ID, cancel)
                    .await
                    .unwrap_or(false)
            } else {
                true
            }
        };
        if needs_install {
            if let Err(err) = adb
                .install_agent_apk(&self.serial, &apk.apk_path, cancel)
                .await
            {
                if let Some(ref mut anim) = animation_guard {
                    anim.restore().await;
                }
                return Err(map_android_err("install_agent_apk", err));
            }
            *self
                .installed_agent_sha
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(apk.sha256.clone());
        }

        // 6. Stop any stale `dev.deja.agent` instrumentation before re-binding port 7007.
        let _ = adb.force_stop(&self.serial, AGENT_PACKAGE_ID, cancel).await;

        // 7. Create dynamic loopback forward `tcp:0 -> tcp:<device_port>`.
        let mut forward = match AdbForwardGuard::create(
            &adb,
            &self.serial,
            self.config.device_port,
            cancel,
        )
        .await
        {
            Ok(fwd) => fwd,
            Err(err) => {
                if let Some(ref mut anim) = animation_guard {
                    anim.restore().await;
                }
                return Err(map_android_err("adb_forward", err));
            }
        };

        // 8. Spawn instrumentation with per-session high-entropy token.
        let token = generate_agent_token().map_err(|err| map_android_err("generate_token", err))?;
        let mut instrumentation = match ManagedInstrumentation::spawn(
            adb.path().to_path_buf(),
            self.serial.clone(),
            &token,
            self.config.device_port,
            self.config.env.clone(),
        ) {
            Ok(inst) => inst,
            Err(err) => {
                forward.remove().await;
                if let Some(ref mut anim) = animation_guard {
                    anim.restore().await;
                }
                return Err(map_android_err("spawn_instrumentation", err));
            }
        };

        let client = match AgentClient::new(forward.host_port(), token, self.config.request_timeout)
        {
            Ok(c) => c,
            Err(err) => {
                instrumentation.shutdown().await;
                forward.remove().await;
                if let Some(ref mut anim) = animation_guard {
                    anim.restore().await;
                }
                return Err(map_android_err("init_agent_client", err));
            }
        };

        // 9. Wait for instrumentation ready sentinel + `/health` verifying `tokenDigest`.
        let startup_deadline = tokio::time::Instant::now() + self.config.startup_timeout;
        let health =
            match wait_for_agent_ready(&mut instrumentation, &client, startup_deadline, cancel)
                .await
            {
                Ok(h) => h,
                Err(err) => {
                    instrumentation.shutdown().await;
                    forward.remove().await;
                    if let Some(ref mut anim) = animation_guard {
                        anim.restore().await;
                    }
                    return Err(map_android_err("wait_for_agent_ready", err));
                }
            };

        let dirty = Arc::new(AtomicBool::new(false));
        let handle = SessionHandle {
            client: client.clone(),
            dirty: Arc::clone(&dirty),
        };
        self.agent_pid.store(health.pid, Ordering::SeqCst);
        self.attach_count.fetch_add(1, Ordering::SeqCst);
        self.reset_tree_busy();

        info!(
            serial = %self.serial,
            host_port = forward.host_port(),
            device_port = self.config.device_port,
            agent_pid = health.pid,
            sdk_int = health.sdk_int,
            "Déjà Android UiAutomator2 HTTP agent ready"
        );

        let live = LiveSession {
            client,
            dirty,
            instrumentation,
            forward,
            animation_guard,
            _device_lock: device_lock,
        };
        let previous = {
            let mut guard = self.lock_session();
            std::mem::replace(&mut *guard, SessionState::Live(Box::new(live)))
        };
        teardown_session(previous).await;

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
        self.invalidate_session_locked(base_url).await;
    }

    async fn invalidate_session_locked(&self, base_url: &str) {
        let taken = {
            let mut guard = self.lock_session();
            let matches = guard
                .handle()
                .is_some_and(|handle| handle.client.base_url() == base_url);
            if matches {
                self.agent_pid.store(0, Ordering::SeqCst);
                Some(std::mem::replace(&mut *guard, SessionState::Stopping))
            } else {
                None
            }
        };
        if let Some(old) = taken {
            let _reset_guard = ResetStoppingGuard {
                session: &self.session,
            };
            teardown_session(old).await;
        }
    }

    async fn acquire_device_ownership(
        &self,
        operation: &str,
        cancel: &CancellationToken,
    ) -> std::result::Result<DeviceOwnershipLease<'_>, DriverError> {
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

        let existing_handle = self.lock_session().handle();
        if let Some(handle) = existing_handle
            && handle.is_dirty()
        {
            self.invalidate_session_locked(handle.client.base_url())
                .await;
        }

        let already_owned = self.lock_session().handle().is_some();
        let temp_lock = if already_owned {
            None
        } else {
            Some(
                self.acquire_device_lock_now()
                    .map_err(|err| map_android_err(operation, err))?,
            )
        };
        Ok(DeviceOwnershipLease {
            _temp_lock: temp_lock,
            _lifecycle: lifecycle_guard,
        })
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
                        "Android agent RPC `{operation}` exceeded local {}ms deadline; recycled session",
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
                if let AndroidDriverError::AgentHttpStatus {
                    status: 409,
                    ref body,
                    ..
                } = err
                {
                    rpc_guard.disarm();
                    self.record_tree_busy(handle, operation, body)?;
                }
                if is_dead_session_error(&err) || is_reqwest_timeout_error(&err) {
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

    async fn enrich_viewport_if_needed(
        &self,
        mut ui: UiHierarchy,
        cancel: &CancellationToken,
    ) -> UiHierarchy {
        if !ui.unchanged
            && ui.viewport.is_none_or(|vp| !vp.is_positive())
            && let Ok(adb) = self.resolve_adb()
            && let Ok(Some(bounds)) = adb.display_bounds(&self.serial, cancel).await
        {
            ui.viewport = Some(bounds);
        }
        ui
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
            id: req.selector.id.clone(),
            text: req.selector.text.clone(),
            regex: Some(false),
            text_match: req.selector.text.as_ref().map(|_| TextMatchMode::Exact),
            not_visible: Some(req.not_visible),
            wait_stable: Some(req.wait_stable),
            stable_polls: req.wait_stable.then_some(req.stable_polls as u32),
            timeout_ms: Some(timeout_ms),
            include_system_ui: Some(req.include_system_ui),
            return_hierarchy: Some(req.return_hierarchy),
            max_depth: None,
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

        let ui_hierarchy = match resp.hierarchy.as_ref() {
            Some(raw) => Some(
                self.enrich_viewport_if_needed(hierarchy_response_to_ui(raw), cancel)
                    .await,
            ),
            None => None,
        };
        let matched_node = resp.node.as_ref().map(hierarchy_node_to_ui_node);

        // Redact raw uncollapsed visibleSample BEFORE whitespace normalization (`DEJA-64` P1 invariant).
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

        // Defense-in-depth: never accept a positive `notVisible` confirmation backed by a
        // truncated hierarchy snapshot.
        if req.not_visible && resp.matched && ui_hierarchy.as_ref().is_some_and(|h| h.truncated) {
            return Err(DriverError::Transport {
                operation: "wait_for_selector".to_string(),
                detail: "server-side waitForSelector confirmed notVisible against a truncated UI hierarchy".to_string(),
                transient: false,
            });
        }

        let obtained_fresh =
            resp.matched || !resp.hierarchy_hash.is_empty() || ui_hierarchy.is_some();
        if !resp.matched && !obtained_fresh {
            self.record_tree_busy(
                &handle,
                "wait_for_selector",
                "server-side waitForSelector captured no readable UI hierarchy snapshot",
            )?;
            return Err(DriverError::Transport {
                operation: "wait_for_selector".to_string(),
                detail: format!(
                    "server-side waitForSelector completed {} poll(s) without capturing a readable UI hierarchy snapshot",
                    resp.polls
                ),
                transient: false,
            });
        }
        self.reset_tree_busy();

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

    async fn execute_single_act(
        &self,
        app_id: &str,
        kind: ActKind,
        operation: &str,
        cancel: &CancellationToken,
    ) -> std::result::Result<(), DriverError> {
        self.maybe_keep_awake(cancel).await;
        let handle = self.get_handle_or_start(cancel).await?;
        let req = ActRequest {
            app_id: app_id.to_string(),
            kind,
            post_action_delay_ms: None,
            return_hierarchy: Some(false),
            include_system_ui: Some(false),
            known_hash: None,
            max_depth: None,
        };
        let resp = self
            .call_agent(&handle, operation, handle.client.act(&req), cancel)
            .await?;
        if !resp.ok {
            return Err(DriverError::Transport {
                operation: operation.to_string(),
                detail: "Android agent returned ok=false".to_string(),
                transient: false,
            });
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
}

impl Drop for AndroidDriver {
    fn drop(&mut self) {
        // Synchronously drop any live session so instrumentation `-pgid`, `adb forward --remove`,
        // `AnimationScaleGuard` restoration, and `DeviceLockGuard` release always execute.
        let mut guard = self
            .session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _taken = std::mem::replace(&mut *guard, SessionState::Detached);
    }
}

impl PlatformDriver for AndroidDriver {
    fn platform(&self) -> &'static str {
        "android"
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
            if opts.clear_keychain {
                return Err(unsupported_clear_keychain());
            }
            let pkg = validate_package_id(app_id)
                .map_err(|err| DriverError::InvalidArgument(err.to_string()))?;

            // Note: `FlowExecutor` always passes `state.env.merged()` into `opts.env`.
            // Android intents do not accept arbitrary POSIX process env maps, so `opts.env`
            // is accepted and ignored cleanly rather than failing normal flows with `env:` headers.
            let _ = &opts.env;

            let handle = self.ensure_session_handle(cancel).await?;
            let adb = self
                .resolve_adb()
                .map_err(|err| map_android_err("launch_app", err))?;

            let _lease = self.acquire_device_ownership("launch_app", cancel).await?;
            self.check_wake_and_lock_cached(&adb, cancel)
                .await
                .map_err(|err| map_android_err("launch_app", err))?;

            if opts.clear_state {
                adb.clear_state(&self.serial, &pkg, cancel)
                    .await
                    .map_err(|err| map_android_err("clear_state", err))?;
            } else if opts.stop_app {
                adb.force_stop(&self.serial, &pkg, cancel)
                    .await
                    .map_err(|err| map_android_err("stop_app", err))?;
            }

            let default_permissions: BTreeMap<String, String>;
            let effective_permissions = match &opts.permissions {
                None => {
                    default_permissions =
                        BTreeMap::from([("all".to_string(), "allow".to_string())]);
                    &default_permissions
                }
                Some(explicit) => explicit,
            };
            if !effective_permissions.is_empty() {
                adb.set_permissions(&self.serial, &pkg, effective_permissions, cancel)
                    .await
                    .map_err(|err| map_android_err("set_permissions", err))?;
            }

            let launch_started_epoch_ms = now_epoch_ms();
            let deadline = tokio::time::Instant::now() + self.config.app_launch_timeout;

            let launch_outcome = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                timed = tokio::time::timeout_at(
                    deadline,
                    adb.launch_app(&self.serial, &pkg, &opts.arguments, cancel),
                ) => timed,
            };
            drop(_lease);

            match launch_outcome {
                Ok(Ok(())) => {}
                Ok(Err(AndroidDriverError::InvalidArgument(msg))) => {
                    return Err(DriverError::InvalidArgument(msg));
                }
                Ok(Err(err)) => {
                    return Err(DriverError::LaunchFailed {
                        app_id: pkg,
                        detail: err.to_string(),
                    });
                }
                Err(_) => {
                    if let Some(sig) = find_android_crash_with_grace(
                        &adb,
                        &self.serial,
                        &pkg,
                        launch_started_epoch_ms,
                        now_epoch_ms(),
                        self.config.crash_report_grace,
                        cancel,
                    )
                    .await
                    .map_err(|err| map_android_err("check_app_crash", err))?
                    {
                        return Err(DriverError::AppCrashed(sig));
                    }
                    return Err(DriverError::LaunchFailed {
                        app_id: pkg,
                        detail: format!(
                            "timed out after {}ms waiting for `am start -W`",
                            self.config.app_launch_timeout.as_millis()
                        ),
                    });
                }
            }

            // Wait until the app reaches foreground with a readable accessibility tree.
            let mut delay_ms = 25u64;
            let mut last_detail =
                "app did not reach foreground with readable hierarchy".to_string();
            loop {
                if cancel.is_cancelled() {
                    return Err(DriverError::Cancelled);
                }
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }

                let state_res = self
                    .call_agent_with_timeout(
                        &handle,
                        "launch_app_state",
                        handle.client.app_state(&pkg),
                        Some(remaining.min(Duration::from_secs(3))),
                        cancel,
                    )
                    .await;

                match state_res {
                    Ok(state) if state.foreground => {
                        let req = HierarchyRequest {
                            app_id: Some(pkg.clone()),
                            include_system_ui: Some(false),
                            known_hash: None,
                            max_depth: None,
                        };
                        if let Ok(h) = self
                            .call_agent_with_timeout(
                                &handle,
                                "launch_app_warmup",
                                handle.client.hierarchy(&req),
                                Some(remaining.min(Duration::from_secs(4))),
                                cancel,
                            )
                            .await
                            && !h.nodes.is_empty()
                        {
                            self.reset_tree_busy();
                            return Ok(());
                        }
                        last_detail =
                            "app reached foreground but hierarchy is not ready yet".to_string();
                    }
                    Ok(state) => {
                        last_detail = format!(
                            "foreground package is `{}` (running={})",
                            state.foreground_package, state.running
                        );
                        if !state.running
                            && let Ok(logcat) = adb.logcat_crash_epoch(&self.serial, cancel).await
                            && let Some(sig) = parse_crash_logcat(
                                &logcat,
                                &pkg,
                                launch_started_epoch_ms,
                                now_epoch_ms(),
                            )
                        {
                            return Err(DriverError::AppCrashed(sig));
                        }
                    }
                    Err(DriverError::Cancelled) => return Err(DriverError::Cancelled),
                    Err(err) => {
                        last_detail = err.to_string();
                    }
                }

                let sleep_for = Duration::from_millis(delay_ms)
                    .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
                if sleep_for.is_zero() {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    _ = tokio::time::sleep(sleep_for) => {}
                }
                delay_ms = (delay_ms * 2).min(150);
            }

            if let Some(sig) = find_android_crash_with_grace(
                &adb,
                &self.serial,
                &pkg,
                launch_started_epoch_ms,
                now_epoch_ms(),
                self.config.crash_report_grace,
                cancel,
            )
            .await
            .map_err(|err| map_android_err("check_app_crash", err))?
            {
                return Err(DriverError::AppCrashed(sig));
            }

            Err(DriverError::LaunchFailed {
                app_id: pkg,
                detail: format!(
                    "timed out after {}ms waiting for Android app foreground warmup: {last_detail}",
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
            let adb = self
                .resolve_adb()
                .map_err(|err| map_android_err("stop_app", err))?;
            let _lease = self.acquire_device_ownership("stop_app", cancel).await?;
            adb.force_stop(&self.serial, app_id, cancel)
                .await
                .map_err(|err| map_android_err("stop_app", err))
        })
    }

    fn kill_app<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let adb = self
                .resolve_adb()
                .map_err(|err| map_android_err("kill_app", err))?;
            let _lease = self.acquire_device_ownership("kill_app", cancel).await?;
            adb.force_stop(&self.serial, app_id, cancel)
                .await
                .map_err(|err| map_android_err("kill_app", err))
        })
    }

    fn clear_state<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let adb = self
                .resolve_adb()
                .map_err(|err| map_android_err("clear_state", err))?;
            let _lease = self.acquire_device_ownership("clear_state", cancel).await?;
            adb.clear_state(&self.serial, app_id, cancel)
                .await
                .map_err(|err| map_android_err("clear_state", err))
        })
    }

    fn clear_keychain<'a>(&'a self, _cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async { Err(unsupported_clear_keychain()) })
    }

    fn open_link<'a>(
        &'a self,
        _app_id: Option<&'a str>,
        url: &'a str,
        opts: &'a OpenLinkOptions,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let adb = self
                .resolve_adb()
                .map_err(|err| map_android_err("open_link", err))?;
            let _lease = self.acquire_device_ownership("open_link", cancel).await?;
            adb.open_link(&self.serial, url, opts.browser == Some(true), cancel)
                .await
                .map_err(|err| match err {
                    AndroidDriverError::InvalidArgument(_) => {
                        DriverError::InvalidArgument(err.to_string())
                    }
                    AndroidDriverError::Cancelled => DriverError::Cancelled,
                    other => DriverError::NavigationFailed {
                        target: url.to_string(),
                        detail: other.to_string(),
                    },
                })
        })
    }

    fn add_media<'a>(
        &'a self,
        paths: &'a [PathBuf],
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let adb = self
                .resolve_adb()
                .map_err(|err| map_android_err("add_media", err))?;
            let _lease = self.acquire_device_ownership("add_media", cancel).await?;
            adb.add_media(&self.serial, paths, cancel)
                .await
                .map_err(|err| map_android_err("add_media", err))
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
            let req = HierarchyRequest {
                app_id: (!app_id.is_empty()).then(|| app_id.to_string()),
                include_system_ui: Some(include_system_ui),
                known_hash: known_hash
                    .filter(|h| !h.is_empty())
                    .map(ToString::to_string),
                max_depth: None,
            };
            let resp = self
                .call_agent(&handle, "hierarchy", handle.client.hierarchy(&req), cancel)
                .await?;

            if !resp.unchanged && resp.nodes.is_empty() {
                self.record_tree_busy(
                    &handle,
                    "hierarchy",
                    "accessibility tree returned zero nodes",
                )?;
            } else {
                self.reset_tree_busy();
            }

            let ui = hierarchy_response_to_ui(&resp);
            Ok(self.enrich_viewport_if_needed(ui, cancel).await)
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
            self.execute_single_act(
                app_id,
                ActKind::Tap {
                    x: point.x,
                    y: point.y,
                    count: Some(1),
                    duration_ms: None,
                },
                "tap_point",
                cancel,
            )
            .await
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
            self.execute_single_act(
                app_id,
                ActKind::LongPress {
                    x: point.x,
                    y: point.y,
                    duration_ms: Some(duration_ms.unwrap_or(DEFAULT_LONG_PRESS_MS).max(1)),
                },
                "long_press_point",
                cancel,
            )
            .await
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
            self.execute_single_act(
                app_id,
                ActKind::DoubleTap {
                    x: point.x,
                    y: point.y,
                },
                "double_tap_point",
                cancel,
            )
            .await
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
            self.execute_single_act(
                &params.app_id,
                ActKind::Swipe {
                    start_x: params.start.x,
                    start_y: params.start.y,
                    end_x: params.end.x,
                    end_y: params.end.y,
                    duration_ms: Some(params.duration_ms),
                    hold_duration_ms: params.hold_duration_ms,
                },
                "swipe",
                cancel,
            )
            .await
        })
    }

    fn input_text<'a>(
        &'a self,
        app_id: &'a str,
        text: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.execute_single_act(
                app_id,
                ActKind::Type {
                    text: text.to_string(),
                },
                "input_text",
                cancel,
            )
            .await
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
            let mut remaining = count;
            while remaining > 0 {
                let batch = remaining.min(MAX_ERASE_BATCH);
                self.execute_single_act(
                    app_id,
                    ActKind::Erase { count: batch },
                    "erase_text",
                    cancel,
                )
                .await?;
                remaining -= batch;
            }
            Ok(())
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
            if key.key.trim().is_empty() {
                return Err(DriverError::InvalidArgument(
                    "press_key requires a non-empty key name".to_string(),
                ));
            }
            let mut remaining = key.count;
            while remaining > 0 {
                let batch = remaining.min(MAX_KEY_BATCH);
                self.execute_single_act(
                    app_id,
                    ActKind::Key {
                        key: key.key.trim().to_string(),
                        modifiers: key.modifiers.clone(),
                        count: Some(batch),
                    },
                    "press_key",
                    cancel,
                )
                .await?;
                remaining -= batch;
            }
            Ok(())
        })
    }

    fn hide_keyboard<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.execute_single_act(app_id, ActKind::HideKeyboard, "hide_keyboard", cancel)
                .await
        })
    }

    fn back<'a>(&'a self, app_id: &'a str, cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.execute_single_act(app_id, ActKind::Back, "back", cancel)
                .await
        })
    }

    fn take_screenshot<'a>(
        &'a self,
        _app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let adb = self
                .resolve_adb()
                .map_err(|err| map_android_err("take_screenshot", err))?;
            adb.screencap(&self.serial, cancel)
                .await
                .map_err(|err| map_android_err("take_screenshot", err))
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
            let adb = self
                .resolve_adb()
                .map_err(|err| map_android_err("check_app_crash", err))?;
            let now_ms = now_epoch_ms();

            // 1. Immediate logcat crash buffer scan.
            let immediate = adb
                .logcat_crash_epoch(&self.serial, cancel)
                .await
                .map_err(|err| map_android_err("check_app_crash", err))?;
            if let Some(sig) = parse_crash_logcat(&immediate, app_id, since_epoch_ms, now_ms) {
                return Ok(Some(sig));
            }

            // 2. If a clean agent session is running and reports the app still alive, skip grace.
            let existing_handle = self.lock_session().handle();
            if let Some(handle) = existing_handle
                && !handle.is_dirty()
            {
                let probe_timeout = Duration::from_secs(2).min(self.config.request_timeout);
                if let Ok(state) = self
                    .call_agent_with_timeout(
                        &handle,
                        "check_app_crash_state",
                        handle.client.app_state(app_id),
                        Some(probe_timeout),
                        cancel,
                    )
                    .await
                    && (state.foreground || state.running)
                {
                    return Ok(None);
                }
            }

            // 3. App is not running or liveness unknown: poll logcat crash buffer for grace period.
            let grace_start = now_epoch_ms();
            find_android_crash_with_grace(
                &adb,
                &self.serial,
                app_id,
                since_epoch_ms,
                grace_start,
                self.config.crash_report_grace,
                cancel,
            )
            .await
            .map_err(|err| map_android_err("check_app_crash", err))
        })
    }

    fn shutdown<'a>(&'a self) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let _lifecycle = self.lifecycle.lock().await;
            let taken = {
                let mut guard = self.lock_session();
                self.agent_pid.store(0, Ordering::SeqCst);
                if matches!(*guard, SessionState::Detached) {
                    SessionState::Detached
                } else {
                    std::mem::replace(&mut *guard, SessionState::Stopping)
                }
            };
            let _reset_guard = ResetStoppingGuard {
                session: &self.session,
            };
            teardown_session(taken).await;
            Ok(())
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
            self.maybe_keep_awake(cancel).await;

            // Delays longer than the agent's 5s clamp run locally so we never silently truncate.
            if req
                .post_action_delay_ms
                .is_some_and(|ms| ms > MAX_SERVER_POST_ACTION_DELAY_MS)
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
                    ActAction::Back => {
                        self.back(&req.app_id, cancel).await?;
                    }
                }
                return self.observe_post_action(req, started, cancel).await;
            }

            let act_kind = match &req.action {
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
                    ActKind::LongPress {
                        x: *x,
                        y: *y,
                        duration_ms: Some(duration_ms.unwrap_or(DEFAULT_LONG_PRESS_MS).max(1)),
                    }
                }
                ActAction::DoubleTap { x, y } => {
                    validate_point(UiPoint::new(*x, *y))?;
                    ActKind::DoubleTap { x: *x, y: *y }
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
                ActAction::InputText { text } => ActKind::Type { text: text.clone() },
                ActAction::EraseText { count } => {
                    if *count == 0 {
                        return self.observe_post_action(req, started, cancel).await;
                    }
                    let mut remaining = *count;
                    if remaining > MAX_ERASE_BATCH {
                        let pre = remaining - ((remaining - 1) % MAX_ERASE_BATCH + 1);
                        self.erase_text(&req.app_id, pre, cancel).await?;
                        remaining -= pre;
                    }
                    ActKind::Erase { count: remaining }
                }
                ActAction::PressKey {
                    key,
                    modifiers,
                    count,
                } => {
                    if key.trim().is_empty() {
                        return Err(DriverError::InvalidArgument(
                            "press_key requires a non-empty key name".to_string(),
                        ));
                    }
                    if *count == 0 {
                        return self.observe_post_action(req, started, cancel).await;
                    }
                    let mut remaining = *count;
                    if remaining > MAX_KEY_BATCH {
                        let pre = remaining - ((remaining - 1) % MAX_KEY_BATCH + 1);
                        let kp = KeyPress {
                            key: key.clone(),
                            modifiers: modifiers.clone(),
                            count: pre,
                        };
                        self.press_key(&req.app_id, &kp, cancel).await?;
                        remaining -= pre;
                    }
                    ActKind::Key {
                        key: key.trim().to_string(),
                        modifiers: modifiers.clone(),
                        count: Some(remaining),
                    }
                }
                ActAction::HideKeyboard => ActKind::HideKeyboard,
                ActAction::Back => ActKind::Back,
            };

            let handle = self.get_handle_or_start(cancel).await?;
            let wire_req = ActRequest {
                app_id: req.app_id.clone(),
                kind: act_kind,
                post_action_delay_ms: req.post_action_delay_ms,
                return_hierarchy: Some(req.return_hierarchy),
                include_system_ui: Some(req.include_system_ui),
                known_hash: req.known_hash.clone(),
                max_depth: None,
            };
            let resp = self
                .call_agent(
                    &handle,
                    "act_and_observe",
                    handle.client.act(&wire_req),
                    cancel,
                )
                .await?;

            let hierarchy = match resp.hierarchy.as_ref() {
                Some(raw) => {
                    if !raw.unchanged && raw.nodes.is_empty() {
                        self.record_tree_busy(
                            &handle,
                            "act_and_observe",
                            "post-action hierarchy returned zero nodes",
                        )?;
                    } else {
                        self.reset_tree_busy();
                    }
                    Some(
                        self.enrich_viewport_if_needed(hierarchy_response_to_ui(raw), cancel)
                            .await,
                    )
                }
                None => None,
            };

            Ok(ActOutcome {
                action_ok: resp.ok,
                elapsed_ms: if resp.elapsed_ms > 0 {
                    resp.elapsed_ms
                } else {
                    started.elapsed().as_millis() as u64
                },
                hierarchy,
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
                && !req.require_hittable
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

async fn wait_for_agent_ready(
    instrumentation: &mut ManagedInstrumentation,
    client: &AgentClient,
    deadline: tokio::time::Instant,
    cancel: &CancellationToken,
) -> Result<crate::client::HealthResponse> {
    let mut last_health_err: Option<String> = None;
    let mut ready_rx = instrumentation.ready_receiver().clone();
    let mut poll = tokio::time::interval(Duration::from_millis(60));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if cancel.is_cancelled() {
            return Err(AndroidDriverError::Cancelled);
        }
        let sentinel_seen = ready_rx.borrow().is_some();

        // Probe `/health` either once sentinel arrives or periodically after a short delay.
        if sentinel_seen || tokio::time::Instant::now() + Duration::from_secs(59) >= deadline {
            let probe = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AndroidDriverError::Cancelled),
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(AndroidDriverError::Timeout(format!(
                        "timed out waiting for deja-android-agent /health (last_err={last_health_err:?}). Output:\n{}",
                        instrumentation.output_tail_string()
                    )));
                }
                status = instrumentation.child_mut().wait() => {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    return Err(AndroidDriverError::AgentExitedEarly {
                        status: status.ok().and_then(|s| s.code()),
                        output: instrumentation.output_tail_string(),
                    });
                }
                res = client.health() => res,
            };

            match probe {
                Ok(health)
                    if health.status == "ok"
                        && (health.token_digest.is_empty()
                            || health.token_digest == client.expected_token_digest()) =>
                {
                    return Ok(health);
                }
                Ok(health) => {
                    last_health_err = Some(format!(
                        "unexpected /health payload (status={}, tokenDigest={})",
                        health.status, health.token_digest
                    ));
                }
                Err(err) => {
                    last_health_err = Some(err.to_string());
                }
            }
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AndroidDriverError::Cancelled),
            _ = tokio::time::sleep_until(deadline) => {
                return Err(AndroidDriverError::Timeout(format!(
                    "timed out waiting for deja-android-agent startup (last_err={last_health_err:?}). Output:\n{}",
                    instrumentation.output_tail_string()
                )));
            }
            status = instrumentation.child_mut().wait() => {
                tokio::time::sleep(Duration::from_millis(40)).await;
                return Err(AndroidDriverError::AgentExitedEarly {
                    status: status.ok().and_then(|s| s.code()),
                    output: instrumentation.output_tail_string(),
                });
            }
            _ = ready_rx.changed() => {}
            _ = poll.tick() => {}
        }
    }
}

fn android_capabilities(disable_animations: bool) -> DriverCapabilities {
    let mut caps = vec![
        DriverCapability::AccessibilityTree,
        DriverCapability::SystemBack,
        DriverCapability::AppInventory,
        DriverCapability::ClearState,
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

fn unsupported_clear_keychain() -> DriverError {
    DriverError::Unsupported(UnsupportedCapability {
        platform: "android".to_string(),
        capability: DriverCapability::ClearKeychain,
        reason: "Android has no global Keychain reset equivalent; use `clearState` to clear app Keystore/SharedPreferences".to_string(),
    })
}

/// Converts a `deja-android-agent` [`HierarchyNode`] into a platform-neutral [`UiNode`].
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
        checked: node.checked,
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
        visible_frame: node
            .visible_frame
            .map(|vf| UiRect::new(vf.x, vf.y, vf.width, vf.height)),
    }
}

/// Converts a `deja-android-agent` [`HierarchyResponse`] into a platform-neutral [`UiHierarchy`].
#[must_use]
pub fn hierarchy_response_to_ui(resp: &HierarchyResponse) -> UiHierarchy {
    let viewport = if resp.unchanged {
        None
    } else {
        resp.viewport
            .map(|vp| UiSize::new(vp.width, vp.height))
            .filter(|vp| vp.is_positive())
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

/// Redacts raw uncollapsed `visibleSample` items BEFORE whitespace collapsing (`DEJA-64` P1 invariant).
pub(crate) fn scrub_visible_sample(
    sample: &[String],
    redactor: Option<&deja_core::Redactor>,
    provider_redactor: Option<&deja_core::ProviderKeyRedactor>,
) -> VisibleContext {
    let default_provider = provider_key_redactor(std::iter::empty::<(&str, &str)>());
    let provider = provider_redactor.unwrap_or(&default_provider);

    let scrubbed: Vec<String> = sample
        .iter()
        .map(|raw| {
            let step1 = match redactor {
                Some(r) => r.redact(raw),
                None => raw.clone(),
            };
            provider.redact(&step1)
        })
        .collect();

    VisibleContext::from_candidates(
        scrubbed.iter().map(String::as_str),
        VisibleContext::DEFAULT_LIMIT,
    )
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

fn is_dead_session_error(err: &AndroidDriverError) -> bool {
    match err {
        AndroidDriverError::Http { source, .. } => !source.is_timeout(),
        AndroidDriverError::AgentHttpStatus { status: 503, .. } => true,
        _ => false,
    }
}

fn is_reqwest_timeout_error(err: &AndroidDriverError) -> bool {
    matches!(err, AndroidDriverError::Http { source, .. } if source.is_timeout())
}

fn map_agent_err(operation: &str, elapsed_ms: u64, err: AndroidDriverError) -> DriverError {
    match err {
        AndroidDriverError::Cancelled => DriverError::Cancelled,
        AndroidDriverError::AgentHttpStatus {
            status: 409,
            endpoint,
            body,
        } => DriverError::Transport {
            operation: operation.to_string(),
            detail: format!("Android agent busy on `{endpoint}` (HTTP 409): {body}"),
            transient: true,
        },
        AndroidDriverError::AgentHttpStatus {
            status: 400,
            endpoint,
            body,
        } => DriverError::InvalidArgument(format!(
            "Android agent rejected `{endpoint}` (HTTP 400): {body}"
        )),
        AndroidDriverError::Http { endpoint, source } if source.is_timeout() => {
            DriverError::Timeout {
                operation: operation.to_string(),
                elapsed_ms,
                detail: format!("HTTP timeout calling `{endpoint}`: {source}"),
            }
        }
        other => map_android_err(operation, other),
    }
}

fn map_android_err(operation: &str, err: AndroidDriverError) -> DriverError {
    match err {
        AndroidDriverError::Cancelled => DriverError::Cancelled,
        AndroidDriverError::InvalidSerial(_)
        | AndroidDriverError::InvalidPackageId(_)
        | AndroidDriverError::InvalidPath(_)
        | AndroidDriverError::InvalidArgument(_)
        | AndroidDriverError::InvalidAdbPath { .. } => {
            DriverError::InvalidArgument(err.to_string())
        }
        AndroidDriverError::Timeout(detail) => DriverError::Timeout {
            operation: operation.to_string(),
            elapsed_ms: 0,
            detail,
        },
        AndroidDriverError::Io { source, .. } => DriverError::Io(source),
        other => DriverError::Transport {
            operation: operation.to_string(),
            detail: other.to_string(),
            transient: false,
        },
    }
}

fn generate_agent_token() -> Result<String> {
    #[cfg(unix)]
    {
        let mut bytes = [0u8; 32];
        let mut urandom =
            std::fs::File::open("/dev/urandom").map_err(|source| AndroidDriverError::Io {
                context: "open /dev/urandom for Android agent token".to_string(),
                source,
            })?;
        urandom
            .read_exact(&mut bytes)
            .map_err(|source| AndroidDriverError::Io {
                context: "read random bytes for Android agent token".to_string(),
                source,
            })?;
        Ok(hex::encode(bytes))
    }
    #[cfg(not(unix))]
    {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(std::process::id().to_be_bytes());
        hasher.update(TOKEN_NONCE.fetch_add(1, Ordering::Relaxed).to_be_bytes());
        if let Ok(dur) = SystemTime::now().duration_since(UNIX_EPOCH) {
            hasher.update(dur.as_nanos().to_be_bytes());
        }
        Ok(hex::encode(hasher.finalize()))
    }
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{AndroidDriver, AndroidDriverConfig, scrub_visible_sample};
    use crate::adb::Adb;
    use crate::client::{AgentClient, token_digest};
    use deja_core::{create_redactor, provider_key_redactor};
    use deja_flow::{
        ActAction, ActPayload, DriverCapability, DriverError, LaunchOptions, PlatformDriver,
        Selector, UiPoint, WaitSelectorRequest,
    };
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_util::sync::CancellationToken;

    async fn spawn_mock_agent<F>(handler: F) -> (u16, tokio::task::JoinHandle<()>)
    where
        F: Fn(String, String, String) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let header_end = loop {
                        let Ok(n) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break pos + 4;
                        }
                    };
                    let header_str = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let content_len: usize = header_str
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    while buf.len() < header_end + content_len {
                        let Ok(n) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let req_line = header_str.lines().next().unwrap_or("");
                    let mut parts = req_line.split_whitespace();
                    let method = parts.next().unwrap_or("").to_string();
                    let path = parts.next().unwrap_or("").to_string();
                    let body = String::from_utf8_lossy(
                        &buf[header_end..(header_end + content_len).min(buf.len())],
                    )
                    .to_string();

                    let (status, resp_body) = handler(method, path, body);
                    let resp = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        });
        (port, task)
    }

    #[test]
    fn capabilities_reflect_disable_animations_opt_out_and_omit_clear_keychain() {
        let default_drv =
            AndroidDriver::new(AndroidDriverConfig::new("emulator-5554")).expect("driver");
        assert_eq!(default_drv.platform(), "android");
        let caps = default_drv.capabilities();
        assert!(caps.supports(DriverCapability::AccessibilityTree));
        assert!(caps.supports(DriverCapability::SystemBack));
        assert!(caps.supports(DriverCapability::AppInventory));
        assert!(caps.supports(DriverCapability::ClearState));
        assert!(caps.supports(DriverCapability::CompoundAct));
        assert!(caps.supports(DriverCapability::ServerSideWait));
        assert!(caps.supports(DriverCapability::HierarchyHashShortCircuit));
        assert!(caps.supports(DriverCapability::HoldScroll));
        assert!(caps.supports(DriverCapability::AppCrashReports));
        assert!(caps.supports(DriverCapability::DisableAnimations));
        assert!(!caps.supports(DriverCapability::ClearKeychain));

        let opt_out = AndroidDriver::new(
            AndroidDriverConfig::new("emulator-5554").with_disable_animations(false),
        )
        .expect("opt-out driver");
        assert!(
            !opt_out
                .capabilities()
                .supports(DriverCapability::DisableAnimations)
        );
    }

    #[tokio::test]
    async fn clear_keychain_and_launch_with_clear_keychain_fail_closed_unsupported() {
        let drv = AndroidDriver::new(AndroidDriverConfig::new("emulator-5554")).unwrap();
        let cancel = CancellationToken::new();

        let err = drv
            .clear_keychain(&cancel)
            .await
            .expect_err("clear_keychain must fail closed on Android");
        assert!(matches!(
            err,
            DriverError::Unsupported(u) if u.capability == DriverCapability::ClearKeychain
        ));

        let opts = LaunchOptions {
            clear_keychain: true,
            ..LaunchOptions::default()
        };
        let err2 = drv
            .launch_app("com.example.app", &opts, &cancel)
            .await
            .expect_err("launch_app with clear_keychain=true must fail closed");
        assert!(matches!(
            err2,
            DriverError::Unsupported(u) if u.capability == DriverCapability::ClearKeychain
        ));
    }

    #[tokio::test]
    async fn compound_act_forwards_hold_duration_ms_and_wait_for_selector_redacts_raw_visible_sample()
     {
        let captured_act = Arc::new(std::sync::Mutex::new(Option::<Value>::None));
        let captured_clone = Arc::clone(&captured_act);
        let token = "test-android-token-secret".to_string();
        let digest = token_digest(&token);

        let (port, server) = spawn_mock_agent(move |_method, path, body| match path.as_str() {
            "/health" => (
                200,
                json!({
                    "status": "ok",
                    "agent": "deja-android-agent",
                    "protocolVersion": 1,
                    "pid": 777,
                    "sdkInt": 35,
                    "tokenDigest": digest,
                    "display": {"width": 1080, "height": 2400}
                })
                .to_string(),
            ),
            "/act" => {
                let parsed: Value = serde_json::from_str(&body).unwrap();
                *captured_clone.lock().unwrap() = Some(parsed);
                (
                    200,
                    json!({
                        "ok": true,
                        "elapsedMs": 120,
                        "hierarchy": {
                            "appId": "com.example.app",
                            "state": "running_foreground",
                            "viewport": {"width": 1080, "height": 2400},
                            "truncated": false,
                            "hierarchyHash": "28940cf4aa2f4599",
                            "unchanged": false,
                            "nodeCount": 1,
                            "nodes": [{
                                "index": 0,
                                "depth": 0,
                                "source": "app",
                                "elementType": "Button",
                                "identifier": "com.example.app:id/pay",
                                "label": "Pay now",
                                "title": "",
                                "enabled": true,
                                "checked": false,
                                "selected": false,
                                "hasFocus": false,
                                "hittable": true,
                                "isOnScreen": true,
                                "frame": {"x": 10, "y": 20, "width": 200, "height": 60}
                            }]
                        }
                    })
                    .to_string(),
                )
            }
            "/waitForSelector" => (
                200,
                json!({
                    "matched": false,
                    "elapsedMs": 45,
                    "polls": 2,
                    "hierarchyHash": "28940cf4aa2f4599",
                    // Raw uncollapsed whitespace containing a multi-space secret!
                    "visibleSample": ["Token: my   secret\npass", "Checkout"]
                })
                .to_string(),
            ),
            _ => (404, json!({"error": "not found"}).to_string()),
        })
        .await;

        let client = AgentClient::new(port, token, Duration::from_secs(5)).unwrap();
        let drv = AndroidDriver::with_mock_client_for_tests(
            AndroidDriverConfig::new("emulator-5554"),
            client,
            None,
            None,
        )
        .unwrap();
        let cancel = CancellationToken::new();

        // 1. Compound `/act` forwards `holdDurationMs` for zero-fling scroll (`DEJA-85`).
        let outcome = drv
            .act_and_observe(
                &ActPayload {
                    app_id: "com.example.app".to_string(),
                    action: ActAction::Swipe {
                        start: UiPoint::new(500.0, 1800.0),
                        end: UiPoint::new(500.0, 600.0),
                        duration_ms: 400,
                        hold_duration_ms: Some(180),
                    },
                    post_action_delay_ms: Some(50),
                    return_hierarchy: true,
                    include_system_ui: false,
                    known_hash: None,
                },
                &cancel,
            )
            .await
            .expect("compound swipe act should succeed");
        assert!(outcome.action_ok);
        assert_eq!(outcome.hierarchy.unwrap().nodes.len(), 1);
        let sent = captured_act.lock().unwrap().clone().expect("act captured");
        assert_eq!(sent["action"], "swipe");
        assert_eq!(sent["holdDurationMs"], 180);

        // 2. `/waitForSelector` timeout redacts raw multi-space secret BEFORE whitespace collapse!
        let redactor = create_redactor("/ws", None, &["my   secret\npass".to_string()]);
        let mut req = WaitSelectorRequest::visible(
            "com.example.app",
            Selector::by_text("MissingButton"),
            Duration::from_millis(500),
        );
        req.return_hierarchy = false;
        req.redactor = Some(redactor);
        req.provider_redactor = Some(provider_key_redactor(std::iter::empty::<(&str, &str)>()));

        let wait_out = drv
            .wait_for_selector(&req, &cancel)
            .await
            .expect("wait_for_selector should return timeout outcome");
        assert!(!wait_out.matched);
        let summary = wait_out.visible_context.format_summary();
        assert!(
            !summary.contains("secret") && !summary.contains("pass"),
            "raw multi-space secret leaked into VisibleContext: {summary}"
        );
        assert!(summary.contains("Checkout"));

        server.abort();
    }

    #[tokio::test]
    async fn tree_busy_circuit_breaker_trips_and_marks_session_dirty() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let (port, server) = spawn_mock_agent(move |_method, path, _body| {
            if path == "/hierarchy" {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                (409, json!({"error": "agent busy"}).to_string())
            } else {
                (200, json!({"status": "ok"}).to_string())
            }
        })
        .await;

        let mut cfg = AndroidDriverConfig::new("emulator-5554");
        cfg.tree_busy_threshold = 3;
        let client = AgentClient::new(port, "tok".to_string(), Duration::from_secs(5)).unwrap();
        let drv = AndroidDriver::with_mock_client_for_tests(cfg, client, None, None).unwrap();
        let cancel = CancellationToken::new();

        for attempt in 1..=2 {
            let err = drv
                .hierarchy("com.example.app", false, None, &cancel)
                .await
                .expect_err("409 should error");
            assert!(
                matches!(
                    err,
                    DriverError::Transport {
                        transient: true,
                        ..
                    }
                ),
                "attempt {attempt} before threshold should remain transient: {err:?}"
            );
            assert!(!drv.session_needs_quiesce());
        }

        // 3rd consecutive 409 trips the circuit breaker (non-transient + marks session dirty).
        let tripped = drv
            .hierarchy("com.example.app", false, None, &cancel)
            .await
            .expect_err("3rd 409 must trip circuit breaker");
        assert!(matches!(
            tripped,
            DriverError::Transport { transient: false, ref detail, .. }
                if detail.contains("tree-busy circuit breaker")
        ));
        assert!(drv.session_needs_quiesce());
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        server.abort();
    }

    #[tokio::test]
    async fn cancellation_during_in_flight_rpc_quiesces_and_tears_down_session() {
        let teardown_done = Arc::new(AtomicBool::new(false));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let slow_server = tokio::spawn(async move {
            if let Ok((mut s, _)) = listener.accept().await {
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf).await;
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });

        let client = AgentClient::new(port, "tok".to_string(), Duration::from_secs(10)).unwrap();
        let drv = AndroidDriver::with_mock_client_for_tests(
            AndroidDriverConfig::new("emulator-5554"),
            client,
            None,
            Some(Arc::clone(&teardown_done)),
        )
        .unwrap();

        let cancel = CancellationToken::new();
        let cancel_trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            cancel_trigger.cancel();
        });

        let err = drv
            .hierarchy("com.example.app", false, None, &cancel)
            .await
            .expect_err("cancellation must abort in-flight RPC");
        assert!(matches!(err, DriverError::Cancelled));
        assert!(
            teardown_done.load(Ordering::SeqCst),
            "session teardown must run on cancellation"
        );
        assert!(drv.is_detached());

        slow_server.abort();
    }

    #[test]
    fn scrub_visible_sample_redacts_before_collapsing_whitespace() {
        let redactor = create_redactor("/ws", None, &["alpha   beta\tgamma".to_string()]);
        let ctx = scrub_visible_sample(
            &["prefix alpha   beta\tgamma suffix".to_string()],
            Some(&redactor),
            None,
        );
        let summary = ctx.format_summary();
        assert!(!summary.contains("alpha"));
        assert!(!summary.contains("beta"));
        assert!(summary.contains("prefix"));
        assert!(summary.contains("suffix"));
    }

    #[tokio::test]
    async fn warm_session_reused_when_health_and_token_digest_match() {
        let token = "warm-session-token-xyz".to_string();
        let digest = token_digest(&token);
        let (port, server) = spawn_mock_agent(move |_m, path, _b| {
            if path == "/health" {
                (
                    200,
                    json!({
                        "status": "ok",
                        "agent": "deja-android-agent",
                        "protocolVersion": 1,
                        "pid": 9001,
                        "sdkInt": 35,
                        "tokenDigest": digest
                    })
                    .to_string(),
                )
            } else {
                (404, "{}".to_string())
            }
        })
        .await;

        let client = AgentClient::new(port, token, Duration::from_secs(5)).unwrap();
        let drv = AndroidDriver::with_mock_client_for_tests(
            AndroidDriverConfig::new("emulator-5554"),
            client,
            Some(Adb::from_path(
                std::path::PathBuf::from("/nonexistent/adb"),
                BTreeMap::new(),
            )),
            None,
        )
        .unwrap();
        let cancel = CancellationToken::new();

        drv.ensure_session(&cancel)
            .await
            .expect("warm healthy session with matching tokenDigest should reuse without adb");
        assert_eq!(drv.attach_count(), 1);
        assert_eq!(drv.agent_pid(), Some(9001));

        drv.shutdown().await.expect("shutdown succeeds");
        assert!(drv.is_detached());
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launch_app_ignores_env_passes_typed_extras_and_checks_crash_and_actions() {
        use deja_core::{CrashSignalSource, LaunchArgumentValue};
        use deja_flow::{KeyPress, OpenLinkOptions};
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "deja-android-driver-full-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let log_file = dir.join("adb.log");
        let adb_bin = dir.join("adb");
        let media_file = dir.join("photo.png");
        fs::write(&media_file, b"\x89PNG\r\n\x1a\nfake").unwrap();

        let script = format!(
            r#"#!/bin/sh
echo "$*" >> "{log}"
case "$*" in
  *"dumpsys power"*)
    echo "  mWakefulness=Awake"
    ;;
  *"dumpsys window displays"*)
    echo "  init=1080x2400 cur=2400x1080 app=2256x1080"
    ;;
  *"dumpsys window"*)
    echo "  isKeyguardShowing=false"
    ;;
  *"resolve-activity"*)
    echo "priority=0"
    echo "com.example.app/.MainActivity"
    ;;
  *"exec-out screencap -p"*)
    printf '\211PNG\r\n\032\nFAKEPNG'
    ;;
  *"logcat -d -b crash -v epoch"*)
    echo "         1700000010.500  3333  3333 E AndroidRuntime: FATAL EXCEPTION: main"
    echo "         1700000010.501  3333  3333 E AndroidRuntime: Process: com.example.app, PID: 3333"
    ;;
  *)
    exit 0
    ;;
esac
"#,
            log = log_file.display()
        );
        fs::write(&adb_bin, script).unwrap();
        fs::set_permissions(&adb_bin, fs::Permissions::from_mode(0o755)).unwrap();

        let token = "full-flow-token".to_string();
        let digest = token_digest(&token);
        let actions_seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let actions_clone = Arc::clone(&actions_seen);

        let (port, server) = spawn_mock_agent(move |_m, path, body| match path.as_str() {
            "/health" => (
                200,
                json!({
                    "status": "ok",
                    "agent": "deja-android-agent",
                    "protocolVersion": 1,
                    "pid": 500,
                    "sdkInt": 35,
                    "tokenDigest": digest
                })
                .to_string(),
            ),
            "/appState" => (
                200,
                json!({
                    "appId": "com.example.app",
                    "foreground": true,
                    "foregroundPackage": "com.example.app",
                    "running": true
                })
                .to_string(),
            ),
            "/hierarchy" => (
                200,
                json!({
                    "appId": "com.example.app",
                    "state": "running_foreground",
                    "truncated": false,
                    "hierarchyHash": "28940cf4aa2f4599",
                    "unchanged": false,
                    "nodeCount": 1,
                    "nodes": [{
                        "index": 0,
                        "depth": 0,
                        "source": "app",
                        "elementType": "FrameLayout",
                        "identifier": "root",
                        "label": "Home",
                        "title": "",
                        "enabled": true,
                        "checked": false,
                        "selected": false,
                        "hasFocus": false,
                        "hittable": true,
                        "isOnScreen": true,
                        "frame": {"x": 0, "y": 0, "width": 2400, "height": 1080}
                    }]
                })
                .to_string(),
            ),
            "/act" => {
                let parsed: Value = serde_json::from_str(&body).unwrap();
                actions_clone
                    .lock()
                    .unwrap()
                    .push(parsed["action"].as_str().unwrap_or("").to_string());
                (200, json!({"ok": true, "elapsedMs": 10}).to_string())
            }
            _ => (404, "{}".to_string()),
        })
        .await;

        let mut cfg = AndroidDriverConfig::new("emulator-5554").with_keep_awake(true);
        cfg.crash_report_grace = Duration::from_millis(50);
        let client = AgentClient::new(port, token, Duration::from_secs(5)).unwrap();
        let adb = Adb::from_path(adb_bin, BTreeMap::new());
        let drv = AndroidDriver::with_mock_client_for_tests(cfg, client, Some(adb), None).unwrap();
        let cancel = CancellationToken::new();

        // 1. launch_app accepts & ignores `opts.env`, passes typed extras, grants permissions.
        let mut launch_opts = LaunchOptions {
            clear_state: true,
            clear_keychain: false,
            stop_app: true,
            permissions: Some(BTreeMap::from([(
                "camera".to_string(),
                "allow".to_string(),
            )])),
            arguments: BTreeMap::from([
                (
                    "mode".to_string(),
                    LaunchArgumentValue::String("demo".to_string()),
                ),
                ("flag".to_string(), LaunchArgumentValue::Bool(true)),
                ("count".to_string(), LaunchArgumentValue::Number(42.into())),
            ]),
            env: BTreeMap::from([("FLOW_VAR".to_string(), "should-not-fail".to_string())]),
        };
        drv.launch_app("com.example.app", &launch_opts, &cancel)
            .await
            .expect("launch_app should succeed even when opts.env is non-empty");
        launch_opts.clear_state = false;

        // 2. Hierarchy falls back to rotated `dumpsys window displays` bounds (2400x1080) when agent omits viewport.
        let h = drv
            .hierarchy("com.example.app", false, None, &cancel)
            .await
            .expect("hierarchy");
        assert_eq!(h.viewport, Some(deja_flow::UiSize::new(2400.0, 1080.0)));

        // 3. Primitives: back, hide_keyboard, input_text, erase_text, press_key, open_link, add_media, screenshot.
        drv.back("com.example.app", &cancel).await.unwrap();
        drv.hide_keyboard("com.example.app", &cancel).await.unwrap();
        drv.input_text("com.example.app", "hello", &cancel)
            .await
            .unwrap();
        drv.erase_text("com.example.app", 3, &cancel).await.unwrap();
        drv.press_key("com.example.app", &KeyPress::single("enter"), &cancel)
            .await
            .unwrap();
        drv.open_link(
            Some("com.example.app"),
            "https://deja.run/demo",
            &OpenLinkOptions {
                auto_verify: None,
                browser: Some(true),
            },
            &cancel,
        )
        .await
        .unwrap();
        drv.add_media(&[media_file], &cancel).await.unwrap();
        let png = drv
            .take_screenshot("com.example.app", &cancel)
            .await
            .unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));

        // 4. Scoped crash detection detects ART crash inside window.
        let crash = drv
            .check_app_crash("com.example.app", 1_700_000_000_000, &cancel)
            .await
            .expect("check_app_crash")
            .expect("should detect ART crash");
        assert_eq!(crash.source, CrashSignalSource::CrashReport);
        assert_eq!(crash.process.as_deref(), Some("com.example.app"));

        let seen = actions_seen.lock().unwrap().clone();
        assert_eq!(seen, vec!["back", "hideKeyboard", "type", "erase", "key"]);

        let log = fs::read_to_string(&log_file).unwrap();
        assert!(log.contains("pm clear com.example.app"));
        assert!(log.contains("pm grant com.example.app android.permission.CAMERA"));
        assert!(log.contains("--es 'mode' 'demo'"));
        assert!(log.contains("--ez 'flag' true"));
        assert!(log.contains("--ei 'count' 42"));
        assert!(log.contains("input keyevent 0"));
        assert!(log.contains("android.intent.action.VIEW"));
        assert!(log.contains("MEDIA_SCANNER_SCAN_FILE"));

        server.abort();
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn wait_for_selector_not_visible_and_act_injection_failure_fail_closed() {
        let mode = Arc::new(AtomicUsize::new(0));
        let mode_clone = Arc::clone(&mode);

        let (port, server) = spawn_mock_agent(move |_m, path, _body| match path.as_str() {
            "/waitForSelector" => match mode_clone.load(Ordering::SeqCst) {
                0 => (
                    200,
                    // Unreadable root capture timeout: empty hierarchyHash + no hierarchy.
                    json!({
                        "matched": false,
                        "elapsedMs": 120,
                        "polls": 4,
                        "hierarchyHash": "",
                        "visibleSample": []
                    })
                    .to_string(),
                ),
                1 => (
                    200,
                    // Truncated capture timeout for notVisible: matched=false (never falsely passes).
                    json!({
                        "matched": false,
                        "elapsedMs": 120,
                        "polls": 4,
                        "hierarchyHash": "28940cf4aa2f4599",
                        "visibleSample": ["Checkout"],
                        "hierarchy": {
                            "appId": "com.example.app",
                            "state": "running_foreground",
                            "truncated": true,
                            "hierarchyHash": "28940cf4aa2f4599",
                            "unchanged": false,
                            "nodeCount": 1,
                            "nodes": []
                        }
                    })
                    .to_string(),
                ),
                _ => (
                    200,
                    // Misbehaving server falsely confirming notVisible on a truncated tree.
                    json!({
                        "matched": true,
                        "elapsedMs": 15,
                        "polls": 1,
                        "hierarchyHash": "28940cf4aa2f4599",
                        "visibleSample": [],
                        "hierarchy": {
                            "appId": "com.example.app",
                            "state": "running_foreground",
                            "truncated": true,
                            "hierarchyHash": "28940cf4aa2f4599",
                            "unchanged": false,
                            "nodeCount": 1,
                            "nodes": []
                        }
                    })
                    .to_string(),
                ),
            },
            "/act" => (
                500,
                json!({
                    "ok": false,
                    "error": "UiAutomation.injectInputEvent rejected ACTION_DOWN (missing INJECT_EVENTS permission or blocked window)"
                })
                .to_string(),
            ),
            _ => (404, "{}".to_string()),
        })
        .await;

        let client = AgentClient::new(port, "tok".to_string(), Duration::from_secs(5)).unwrap();
        let drv = AndroidDriver::with_mock_client_for_tests(
            AndroidDriverConfig::new("emulator-5554"),
            client,
            None,
            None,
        )
        .unwrap();
        let cancel = CancellationToken::new();

        let mut not_vis_req = WaitSelectorRequest::not_visible(
            "com.example.app",
            Selector::by_text("Loading"),
            Duration::from_millis(200),
        );
        not_vis_req.return_hierarchy = true;

        // 1. Unreadable capture timeout surfaces DriverError::Transport.
        mode.store(0, Ordering::SeqCst);
        let unreadable_err = drv
            .wait_for_selector(&not_vis_req, &cancel)
            .await
            .expect_err("unreadable capture timeout must return DriverError::Transport");
        assert!(matches!(unreadable_err, DriverError::Transport { .. }));

        // 2. Truncated hierarchy timeout returns matched=false (never falsely confirms absence).
        mode.store(1, Ordering::SeqCst);
        let trunc_out = drv
            .wait_for_selector(&not_vis_req, &cancel)
            .await
            .expect("truncated timeout returns matched=false outcome");
        assert!(!trunc_out.matched);
        assert!(trunc_out.hierarchy.as_ref().is_some_and(|h| h.truncated));

        // 3. Defense-in-depth rejects matched=true with truncated hierarchy on notVisible.
        mode.store(2, Ordering::SeqCst);
        let misbehaving_err = drv
            .wait_for_selector(&not_vis_req, &cancel)
            .await
            .expect_err("truncated matched=true on notVisible must fail closed");
        assert!(matches!(misbehaving_err, DriverError::Transport { .. }));

        // 4. Rejected input injection HTTP 500 surfaces as DriverError::Transport.
        let act_err = drv
            .tap_point("com.example.app", UiPoint::new(100.0, 200.0), &cancel)
            .await
            .expect_err("rejected injectInputEvent HTTP 500 must fail");
        assert!(matches!(
            act_err,
            DriverError::Transport { ref detail, .. } if detail.contains("injectInputEvent")
        ));

        server.abort();
    }
}
