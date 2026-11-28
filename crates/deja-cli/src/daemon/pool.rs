//! Warm driver pool with exclusive per-device lease serialization, canonical SHA-256 session keys,
//! RAII lease guards with dirty-driver eviction, and race-free idle TTL / shutdown coordination.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use deja_driver_web::ScreenSize;
use deja_flow::PlatformDriver;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;

use crate::daemon::ALLOWED_WEB_HEADFUL_ENV_KEYS;
use crate::daemon::paths::DaemonMetadata;
use crate::daemon::protocol::{
    DaemonStatusReport, PooledSessionStatus, absolutize_chrome_env_paths,
};
use crate::driver_factory::{DriverContext, DriverFactory, DriverFactoryError};

/// Bounded shutdown budget per pooled driver during graceful daemon shutdown or replacement.
pub const DRIVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounded shutdown budget when evicting an aborted/dirty driver after an abrupt client disconnect.
pub const DIRTY_DRIVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const WEB_MAX_DRIVER_TIMEOUT_MS: u64 = 30_000;

/// Canonical lease-gate key isolating concurrent callers by physical device or Web configuration.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GateKey {
    pub platform: String,
    pub device: String,
    pub web_session_key: Option<String>,
}

impl GateKey {
    #[must_use]
    pub fn for_context(ctx: &DriverContext, _session_key: &str) -> Self {
        match ctx.platform.as_str() {
            "ios" => Self {
                platform: "ios".to_string(),
                device: ctx.device.trim().to_ascii_uppercase(),
                web_session_key: None,
            },
            "android" => Self {
                platform: "android".to_string(),
                device: ctx.device.trim().to_string(),
                web_session_key: None,
            },
            "web" => Self {
                platform: "web".to_string(),
                device: ctx.device.trim().to_string(),
                // Exclude `resolved_runtime` from the Web gate key so an in-place Chromium version
                // upgrade hits the same gate, evicts the stale browser process, and replaces it,
                // while distinct viewports / headless modes still run concurrently.
                web_session_key: Some(compute_web_config_hash(ctx, false)),
            },
            other => Self {
                platform: other.to_string(),
                device: ctx.device.trim().to_string(),
                web_session_key: None,
            },
        }
    }
}

impl fmt::Display for GateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.web_session_key {
            Some(sk) => {
                let short = &sk[..sk.len().min(12)];
                write!(f, "{}:{}:{short}", self.platform, self.device)
            }
            None => write!(f, "{}:{}", self.platform, self.device),
        }
    }
}

fn hash_str_field(hasher: &mut Sha256, tag: &str, value: &str) {
    hasher.update((tag.len() as u64).to_be_bytes());
    hasher.update(tag.as_bytes());
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value.as_bytes());
}

fn hash_opt_str_field(hasher: &mut Sha256, tag: &str, value: Option<&str>) {
    match value {
        Some(v) => {
            hasher.update([1u8]);
            hash_str_field(hasher, tag, v);
        }
        None => {
            hasher.update([0u8]);
            hash_str_field(hasher, tag, "");
        }
    }
}

fn compute_web_config_hash(ctx: &DriverContext, include_resolved_runtime: bool) -> String {
    let mut hasher = Sha256::new();
    let mut normalized_env = ctx.env.clone();
    absolutize_chrome_env_paths(&mut normalized_env);

    hash_str_field(&mut hasher, "schema", "deja-daemon-session-v1");
    hash_str_field(&mut hasher, "platform", "web");
    hash_str_field(&mut hasher, "device", ctx.device.trim());
    let is_headless = ctx.headless.unwrap_or(true);
    hash_str_field(&mut hasher, "headless", if is_headless { "1" } else { "0" });
    let normalized_screen = match ctx.screen_size.as_ref() {
        Some(val) => match ScreenSize::parse(val) {
            Ok(sz) => format!("{}x{}", sz.width, sz.height),
            Err(_) => format!("invalid:{val}"),
        },
        None => {
            let def = ScreenSize::default();
            format!("{}x{}", def.width, def.height)
        }
    };
    hash_str_field(&mut hasher, "screen_size", &normalized_screen);
    hash_str_field(
        &mut hasher,
        "disable_animations",
        if ctx.disable_animations { "1" } else { "0" },
    );
    let capped_timeout = ctx.timeout_ms.clamp(1_000, WEB_MAX_DRIVER_TIMEOUT_MS);
    hash_str_field(
        &mut hasher,
        "capped_timeout_ms",
        &capped_timeout.to_string(),
    );
    hash_opt_str_field(
        &mut hasher,
        "env.DEJA_CHROME",
        normalized_env.get("DEJA_CHROME").map(String::as_str),
    );
    hash_opt_str_field(
        &mut hasher,
        "env.CHROME_PATH",
        normalized_env.get("CHROME_PATH").map(String::as_str),
    );
    hash_opt_str_field(
        &mut hasher,
        "env.PATH",
        normalized_env.get("PATH").map(String::as_str),
    );
    if !is_headless {
        for key in ALLOWED_WEB_HEADFUL_ENV_KEYS {
            hash_opt_str_field(
                &mut hasher,
                &format!("env.{key}"),
                normalized_env.get(*key).map(String::as_str),
            );
        }
    }
    if include_resolved_runtime {
        hash_opt_str_field(
            &mut hasher,
            "resolved_runtime",
            ctx.resolved_runtime.as_deref(),
        );
    }
    hex::encode(hasher.finalize())
}

/// Compute the canonical SHA-256 `session_key` hashing ONLY driver-affecting configuration fields.
///
/// Unrelated per-run fields (`app_id`, `build_path`, `workspace_root`, `expected_build_fingerprint`)
/// are deliberately excluded so consecutive runs on the same simulator/browser reuse the warm
/// driver while still validating build/appId on every warm lease acquisition.
#[must_use]
pub fn compute_session_key(ctx: &DriverContext) -> String {
    if ctx.platform == "web" {
        return compute_web_config_hash(ctx, true);
    }

    let mut hasher = Sha256::new();
    hash_str_field(&mut hasher, "schema", "deja-daemon-session-v1");
    hash_str_field(&mut hasher, "platform", &ctx.platform);

    match ctx.platform.as_str() {
        "ios" => {
            let udid = ctx.device.trim().to_ascii_uppercase();
            hash_str_field(&mut hasher, "device", &udid);
            hash_str_field(
                &mut hasher,
                "disable_animations",
                if ctx.disable_animations { "1" } else { "0" },
            );
            hash_str_field(
                &mut hasher,
                "crash_report_grace_ms",
                &ctx.crash_report_grace_ms.to_string(),
            );
            let crash_dir = ctx
                .crash_report_dir
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned());
            hash_opt_str_field(&mut hasher, "crash_report_dir", crash_dir.as_deref());
            let deja_home = ctx
                .deja_home
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .or_else(|| ctx.env.get("DEJA_HOME").filter(|v| !v.is_empty()).cloned());
            hash_opt_str_field(&mut hasher, "deja_home", deja_home.as_deref());
            hash_opt_str_field(
                &mut hasher,
                "env.PATH",
                ctx.env.get("PATH").map(String::as_str),
            );
            hash_opt_str_field(
                &mut hasher,
                "env.DEVELOPER_DIR",
                ctx.env.get("DEVELOPER_DIR").map(String::as_str),
            );
        }
        _ => {
            hash_str_field(&mut hasher, "device", ctx.device.trim());
            hash_str_field(
                &mut hasher,
                "disable_animations",
                if ctx.disable_animations { "1" } else { "0" },
            );
            hash_opt_str_field(
                &mut hasher,
                "env.PATH",
                ctx.env.get("PATH").map(String::as_str),
            );
            hash_opt_str_field(
                &mut hasher,
                "env.ANDROID_HOME",
                ctx.env.get("ANDROID_HOME").map(String::as_str),
            );
            hash_opt_str_field(
                &mut hasher,
                "env.ANDROID_SDK_ROOT",
                ctx.env.get("ANDROID_SDK_ROOT").map(String::as_str),
            );
            hash_opt_str_field(
                &mut hasher,
                "env.DEJA_ADB",
                ctx.env.get("DEJA_ADB").map(String::as_str),
            );
        }
    }

    hex::encode(hasher.finalize())
}

struct PooledEntry {
    driver: Arc<dyn PlatformDriver>,
    session_key: String,
    lease_count: u64,
}

struct SlotState {
    entry: Option<PooledEntry>,
}

struct SlotSnapshot {
    driver: Option<Arc<dyn PlatformDriver>>,
    platform: String,
    device: String,
    session_key: String,
    leased: bool,
    lease_id: Option<u64>,
    lease_count: u64,
    last_activity: Instant,
}

struct DeviceSlot {
    gate_key: GateKey,
    state: Arc<AsyncMutex<SlotState>>,
    meta: StdMutex<SlotSnapshot>,
}

struct PoolLifecycle {
    shutting_down: bool,
}

/// Shared pool of warm [`PlatformDriver`] instances managed by the daemon.
pub struct DriverPool {
    factory: Arc<dyn DriverFactory>,
    slots: StdMutex<BTreeMap<GateKey, Arc<DeviceSlot>>>,
    lifecycle: StdMutex<PoolLifecycle>,
    active_leases: AtomicUsize,
    next_lease_id: AtomicU64,
    started_at: Instant,
    last_activity_ms: AtomicU64,
    shutdown_token: CancellationToken,
}

/// RAII ticket keeping `active_leases` incremented while a client is acquiring or holding a lease.
struct ActivityTicket {
    pool: Arc<DriverPool>,
}

impl Drop for ActivityTicket {
    fn drop(&mut self) {
        let _guard = self
            .pool
            .lifecycle
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        self.pool.active_leases.fetch_sub(1, Ordering::SeqCst);
        self.pool.touch_activity();
    }
}

/// Exclusive RAII lease on one warm pooled [`PlatformDriver`].
///
/// - Calling [`Self::release_clean`] returns the driver to the warm pool and immediately releases
///   the device gate for queued waiters.
/// - Calling [`Self::evict_dirty`] (or dropping without releasing cleanly) shuts down and removes
///   the driver so a subsequent caller starts from a clean session.
pub struct LeaseGuard {
    lease_id: u64,
    session_key: String,
    warm_hit: bool,
    driver: Arc<dyn PlatformDriver>,
    slot: Arc<DeviceSlot>,
    slot_guard: Option<OwnedMutexGuard<SlotState>>,
    _ticket: ActivityTicket,
    released_clean: bool,
    dirty: AtomicBool,
}

impl LeaseGuard {
    #[must_use]
    pub const fn lease_id(&self) -> u64 {
        self.lease_id
    }

    #[must_use]
    pub fn session_key(&self) -> &str {
        &self.session_key
    }

    #[must_use]
    pub const fn warm_hit(&self) -> bool {
        self.warm_hit
    }

    #[must_use]
    pub fn driver(&self) -> Arc<dyn PlatformDriver> {
        Arc::clone(&self.driver)
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::SeqCst)
    }

    /// Release the lease cleanly while keeping the underlying driver warm in the pool.
    ///
    /// When the driver advertises [`PlatformDriver::needs_idle_reuse_prep`] (e.g. `WebDriver`
    /// rotating a dirty `BrowserContext`), the reset runs asynchronously in the background after
    /// the client lease finishes so the next CLI invocation attaches in single-digit milliseconds.
    pub async fn release_clean(mut self) {
        if self.is_dirty() {
            self.evict_dirty().await;
            return;
        }
        self.released_clean = true;
        {
            let mut snap = self.slot.meta.lock().unwrap_or_else(|p| p.into_inner());
            snap.leased = false;
            snap.lease_id = None;
            snap.last_activity = Instant::now();
        }
        let slot_guard = self.slot_guard.take();
        let shutdown_token = self._ticket.pool.shutdown_token().clone();
        if let Some(mut guard) = slot_guard
            && self.driver.needs_idle_reuse_prep()
            && !shutdown_token.is_cancelled()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            let driver = Arc::clone(&self.driver);
            let slot = Arc::clone(&self.slot);
            handle.spawn(async move {
                let res = driver.prepare_idle_reuse(&shutdown_token).await;
                if let Err(err) = res
                    && !matches!(err, deja_flow::DriverError::Cancelled)
                    && !shutdown_token.is_cancelled()
                {
                    let evicted = guard.entry.take();
                    {
                        let mut snap = slot.meta.lock().unwrap_or_else(|p| p.into_inner());
                        snap.driver = None;
                        snap.last_activity = Instant::now();
                    }
                    if let Some(entry) = evicted {
                        shutdown_driver_bounded(&entry.driver, DIRTY_DRIVER_SHUTDOWN_TIMEOUT).await;
                    }
                }
                drop(guard);
            });
        }
    }

    /// Evict and shut down the leased driver (bounded 5s) before releasing the device gate.
    pub async fn evict_dirty(mut self) {
        self.released_clean = true;
        let evicted = self
            .slot_guard
            .as_mut()
            .and_then(|guard| guard.entry.take());
        {
            let mut snap = self.slot.meta.lock().unwrap_or_else(|p| p.into_inner());
            snap.driver = None;
            snap.leased = false;
            snap.lease_id = None;
            snap.last_activity = Instant::now();
        }
        if let Some(entry) = evicted {
            shutdown_driver_bounded(&entry.driver, DIRTY_DRIVER_SHUTDOWN_TIMEOUT).await;
        }
        drop(self.slot_guard.take());
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if self.released_clean {
            return;
        }
        let evicted = self
            .slot_guard
            .as_mut()
            .and_then(|guard| guard.entry.take());
        {
            let mut snap = self.slot.meta.lock().unwrap_or_else(|p| p.into_inner());
            snap.driver = None;
            snap.leased = false;
            snap.lease_id = None;
            snap.last_activity = Instant::now();
        }
        drop(self.slot_guard.take());
        if let Some(entry) = evicted
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            handle.spawn(async move {
                shutdown_driver_bounded(&entry.driver, DIRTY_DRIVER_SHUTDOWN_TIMEOUT).await;
            });
        }
    }
}

/// Run `driver.shutdown()` with a strict upper time bound.
pub async fn shutdown_driver_bounded(driver: &Arc<dyn PlatformDriver>, timeout: Duration) {
    let _ = tokio::time::timeout(timeout, driver.shutdown()).await;
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl DriverPool {
    #[must_use]
    pub fn new(factory: Arc<dyn DriverFactory>) -> Arc<Self> {
        Arc::new(Self {
            factory,
            slots: StdMutex::new(BTreeMap::new()),
            lifecycle: StdMutex::new(PoolLifecycle {
                shutting_down: false,
            }),
            active_leases: AtomicUsize::new(0),
            next_lease_id: AtomicU64::new(1),
            started_at: Instant::now(),
            last_activity_ms: AtomicU64::new(0),
            shutdown_token: CancellationToken::new(),
        })
    }

    #[must_use]
    pub fn shutdown_token(&self) -> &CancellationToken {
        &self.shutdown_token
    }

    /// Record recent client activity to reset the idle TTL clock.
    pub fn touch_activity(&self) {
        let now_ms = self.started_at.elapsed().as_millis() as u64;
        self.last_activity_ms.store(now_ms, Ordering::SeqCst);
    }

    #[must_use]
    pub fn active_leases(&self) -> usize {
        self.active_leases.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn idle_elapsed(&self) -> Duration {
        let now_ms = self.started_at.elapsed().as_millis() as u64;
        let last_ms = self.last_activity_ms.load(Ordering::SeqCst);
        Duration::from_millis(now_ms.saturating_sub(last_ms))
    }

    /// Atomically verify under the lifecycle mutex that `active_leases == 0` and
    /// `idle_elapsed >= idle_timeout`, transitioning to `shutting_down = true` when eligible.
    pub fn try_begin_idle_shutdown(&self, idle_timeout: Duration) -> bool {
        let mut lc = self.lifecycle.lock().unwrap_or_else(|p| p.into_inner());
        if lc.shutting_down {
            return false;
        }
        if self.active_leases.load(Ordering::SeqCst) == 0 && self.idle_elapsed() >= idle_timeout {
            lc.shutting_down = true;
            self.shutdown_token.cancel();
            true
        } else {
            false
        }
    }

    /// Unconditionally mark the pool as shutting down (e.g. `Shutdown` RPC or `SIGTERM`/`SIGINT`).
    pub fn mark_shutting_down(&self) {
        let mut lc = self.lifecycle.lock().unwrap_or_else(|p| p.into_inner());
        lc.shutting_down = true;
        self.shutdown_token.cancel();
    }

    fn try_begin_activity(self: &Arc<Self>) -> Result<ActivityTicket, DriverFactoryError> {
        let lc = self.lifecycle.lock().unwrap_or_else(|p| p.into_inner());
        if lc.shutting_down {
            return Err(DriverFactoryError::Runtime(
                "Déjà daemon is shutting down".to_string(),
            ));
        }
        self.active_leases.fetch_add(1, Ordering::SeqCst);
        self.touch_activity();
        Ok(ActivityTicket {
            pool: Arc::clone(self),
        })
    }

    fn get_or_create_slot(
        &self,
        gate_key: GateKey,
        platform: &str,
        device: &str,
    ) -> Arc<DeviceSlot> {
        let mut slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
        Arc::clone(slots.entry(gate_key.clone()).or_insert_with(|| {
            Arc::new(DeviceSlot {
                gate_key,
                state: Arc::new(AsyncMutex::new(SlotState { entry: None })),
                meta: StdMutex::new(SlotSnapshot {
                    driver: None,
                    platform: platform.to_string(),
                    device: device.to_string(),
                    session_key: String::new(),
                    leased: false,
                    lease_id: None,
                    lease_count: 0,
                    last_activity: Instant::now(),
                }),
            })
        }))
    }

    /// Acquire an exclusive [`LeaseGuard`] for the target device/config, reusing an existing warm
    /// driver when `session_key` matches, replacing an incompatible pooled driver cleanly, or
    /// cold-creating a new driver via `self.factory`.
    pub async fn acquire_lease(
        self: &Arc<Self>,
        ctx: &DriverContext,
        cancel: &CancellationToken,
    ) -> Result<LeaseGuard, DriverFactoryError> {
        if cancel.is_cancelled() {
            return Err(DriverFactoryError::Cancelled);
        }

        let ticket = self.try_begin_activity()?;
        let session_key = compute_session_key(ctx);
        let gate_key = GateKey::for_context(ctx, &session_key);
        let slot = self.get_or_create_slot(gate_key, &ctx.platform, &ctx.device);

        let mut slot_guard = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(DriverFactoryError::Cancelled),
            _ = self.shutdown_token.cancelled() => {
                return Err(DriverFactoryError::Runtime(
                    "Déjà daemon is shutting down".to_string(),
                ));
            }
            guard = Arc::clone(&slot.state).lock_owned() => guard,
        };

        if cancel.is_cancelled() {
            return Err(DriverFactoryError::Cancelled);
        }

        // Warm hit: validate context first so invalid builds or mismatched `appId`s still fail
        // closed with exit 2 without disturbing the warm pooled driver.
        if let Some(ref mut entry) = slot_guard.entry
            && entry.session_key == session_key
        {
            self.factory.validate_warm_context(ctx)?;
            entry.lease_count = entry.lease_count.saturating_add(1);
            let lease_id = self.next_lease_id.fetch_add(1, Ordering::SeqCst);
            let driver = Arc::clone(&entry.driver);
            {
                let mut snap = slot.meta.lock().unwrap_or_else(|p| p.into_inner());
                snap.driver = Some(Arc::clone(&driver));
                snap.session_key = session_key.clone();
                snap.leased = true;
                snap.lease_id = Some(lease_id);
                snap.lease_count = entry.lease_count;
                snap.last_activity = Instant::now();
            }
            self.touch_activity();
            return Ok(LeaseGuard {
                lease_id,
                session_key,
                warm_hit: true,
                driver,
                slot,
                slot_guard: Some(slot_guard),
                _ticket: ticket,
                released_clean: false,
                dirty: AtomicBool::new(false),
            });
        }

        // Validate before evicting an existing driver with a different session_key so bad requests
        // never tear down a healthy warm driver.
        self.factory.validate_warm_context(ctx)?;

        if let Some(old_entry) = slot_guard.entry.take() {
            {
                let mut snap = slot.meta.lock().unwrap_or_else(|p| p.into_inner());
                snap.driver = None;
                snap.last_activity = Instant::now();
            }
            shutdown_driver_bounded(&old_entry.driver, DRIVER_SHUTDOWN_TIMEOUT).await;
        }

        // Cold create racing against both client cancellation and daemon shutdown.
        let create_cancel = cancel.child_token();
        let shutdown_link = self.shutdown_token.clone();
        let create_cancel_clone = create_cancel.clone();
        let _link_guard = AbortOnDrop(tokio::spawn(async move {
            shutdown_link.cancelled().await;
            create_cancel_clone.cancel();
        }));
        let created_res = self.factory.create(ctx, &create_cancel).await;
        drop(_link_guard);

        let boxed = match created_res {
            Ok(d) => d,
            Err(DriverFactoryError::Cancelled) if self.shutdown_token.is_cancelled() => {
                return Err(DriverFactoryError::Runtime(
                    "Déjà daemon is shutting down".to_string(),
                ));
            }
            Err(err) => return Err(err),
        };

        let driver: Arc<dyn PlatformDriver> = Arc::from(boxed);
        let lease_id = self.next_lease_id.fetch_add(1, Ordering::SeqCst);
        slot_guard.entry = Some(PooledEntry {
            driver: Arc::clone(&driver),
            session_key: session_key.clone(),
            lease_count: 1,
        });
        {
            let mut snap = slot.meta.lock().unwrap_or_else(|p| p.into_inner());
            snap.driver = Some(Arc::clone(&driver));
            snap.platform = ctx.platform.clone();
            snap.device = ctx.device.clone();
            snap.session_key = session_key.clone();
            snap.leased = true;
            snap.lease_id = Some(lease_id);
            snap.lease_count = 1;
            snap.last_activity = Instant::now();
        }
        self.touch_activity();

        Ok(LeaseGuard {
            lease_id,
            session_key,
            warm_hit: false,
            driver,
            slot,
            slot_guard: Some(slot_guard),
            _ticket: ticket,
            released_clean: false,
            dirty: AtomicBool::new(false),
        })
    }

    /// Snapshot live pool status without ever blocking on active device leases.
    #[must_use]
    pub fn status_report(
        &self,
        metadata: &DaemonMetadata,
        idle_timeout: Duration,
    ) -> DaemonStatusReport {
        let slots: Vec<Arc<DeviceSlot>> = {
            let map = self.slots.lock().unwrap_or_else(|p| p.into_inner());
            map.values().cloned().collect()
        };

        let mut sessions = Vec::new();
        for slot in slots {
            let snap = slot.meta.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(ref driver) = snap.driver {
                sessions.push(PooledSessionStatus {
                    device_key: slot.gate_key.to_string(),
                    platform: snap.platform.clone(),
                    device: snap.device.clone(),
                    session_key: snap.session_key.clone(),
                    leased: snap.leased,
                    lease_id: snap.lease_id,
                    attach_count: driver.session_attach_count(),
                    lease_count: snap.lease_count,
                    idle_ms: if snap.leased {
                        0
                    } else {
                        snap.last_activity.elapsed().as_millis() as u64
                    },
                });
            }
        }

        DaemonStatusReport {
            running: true,
            version: metadata.version.clone(),
            pid: metadata.pid,
            uid: metadata.uid,
            socket_path: metadata.socket_path.clone(),
            started_at: metadata.started_at.clone(),
            uptime_ms: self.started_at.elapsed().as_millis() as u64,
            idle_ms: self.idle_elapsed().as_millis() as u64,
            idle_timeout_secs: idle_timeout.as_secs(),
            active_leases: self.active_leases(),
            pooled_sessions: sessions.len(),
            sessions,
        }
    }

    /// Concurrently shut down every pooled driver with a bounded 10s timeout per driver.
    /// Returns the number of pooled drivers that were shut down.
    pub async fn shutdown_all_drivers(&self) -> usize {
        self.mark_shutting_down();
        let slots: Vec<Arc<DeviceSlot>> = {
            let mut map = self.slots.lock().unwrap_or_else(|p| p.into_inner());
            std::mem::take(&mut *map).into_values().collect()
        };

        let mut handles = Vec::new();
        for slot in slots {
            handles.push(tokio::spawn(async move {
                let maybe_entry =
                    match tokio::time::timeout(Duration::from_secs(5), slot.state.lock()).await {
                        Ok(mut guard) => guard.entry.take(),
                        Err(_) => {
                            let snap = slot.meta.lock().unwrap_or_else(|p| p.into_inner());
                            snap.driver.as_ref().map(|d| PooledEntry {
                                driver: Arc::clone(d),
                                session_key: snap.session_key.clone(),
                                lease_count: snap.lease_count,
                            })
                        }
                    };
                {
                    let mut snap = slot.meta.lock().unwrap_or_else(|p| p.into_inner());
                    snap.driver = None;
                    snap.leased = false;
                    snap.lease_id = None;
                }
                if let Some(entry) = maybe_entry {
                    shutdown_driver_bounded(&entry.driver, DRIVER_SHUTDOWN_TIMEOUT).await;
                    1usize
                } else {
                    0usize
                }
            }));
        }

        let mut stopped = 0usize;
        for h in handles {
            if let Ok(count) = h.await {
                stopped = stopped.saturating_add(count);
            }
        }
        stopped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_ios_ctx(device: &str, app_id: &str) -> DriverContext {
        DriverContext {
            platform: "ios".to_string(),
            device: device.to_string(),
            app_id: app_id.to_string(),
            build_path: PathBuf::from("/tmp/App.app"),
            expected_build_fingerprint: None,
            workspace_root: PathBuf::from("/tmp/ws"),
            disable_animations: true,
            crash_report_grace_ms: 500,
            crash_report_dir: None,
            deja_home: None,
            headless: None,
            screen_size: None,
            timeout_ms: 60_000,
            env: BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
            staged_build: None,
            resolved_runtime: None,
        }
    }

    #[test]
    fn ios_session_key_normalizes_udid_and_ignores_app_or_secret_env() {
        let mut a = sample_ios_ctx("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "dev.deja.app1");
        let mut b = sample_ios_ctx("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE", "dev.deja.app2");
        a.env.insert("OPENAI_API_KEY".to_string(), "k1".to_string());
        b.env.insert("OPENAI_API_KEY".to_string(), "k2".to_string());
        assert_eq!(compute_session_key(&a), compute_session_key(&b));

        b.disable_animations = false;
        assert_ne!(compute_session_key(&a), compute_session_key(&b));
    }

    #[test]
    fn web_session_key_normalizes_screen_size_representations() {
        let mut a = sample_ios_ctx("chromium", "http://127.0.0.1:4174");
        a.platform = "web".to_string();
        a.headless = Some(true);
        a.screen_size = Some(serde_json::json!("1280x800"));
        a.timeout_ms = 60_000;

        let mut b = a.clone();
        b.headless = None; // defaults to true
        b.screen_size = Some(serde_json::json!({ "width": 1280, "height": 800 }));
        b.timeout_ms = 120_000; // both cap at 30_000ms
        assert_eq!(compute_session_key(&a), compute_session_key(&b));

        b.screen_size = Some(serde_json::json!("1440x900"));
        assert_ne!(compute_session_key(&a), compute_session_key(&b));
    }

    #[test]
    fn web_session_key_includes_headful_display_and_runtime_version_while_sharing_gate_for_upgrade()
    {
        let mut headful_a = sample_ios_ctx("chromium", "http://127.0.0.1:4174");
        headful_a.platform = "web".to_string();
        headful_a.headless = Some(false);
        headful_a
            .env
            .insert("DISPLAY".to_string(), ":0".to_string());
        headful_a.resolved_runtime = Some("chromium-158.0.0.1@1280x720-headed".to_string());

        let mut headful_b = headful_a.clone();
        headful_b
            .env
            .insert("DISPLAY".to_string(), ":1".to_string());
        assert_ne!(
            compute_session_key(&headful_a),
            compute_session_key(&headful_b),
            "different headful DISPLAY must produce distinct Web session keys"
        );

        let mut upgraded = headful_a.clone();
        upgraded.resolved_runtime = Some("chromium-159.0.0.2@1280x720-headed".to_string());
        let key_old = compute_session_key(&headful_a);
        let key_new = compute_session_key(&upgraded);
        assert_ne!(
            key_old, key_new,
            "Chromium version upgrade must invalidate warm Web session key"
        );
        assert_eq!(
            GateKey::for_context(&headful_a, &key_old),
            GateKey::for_context(&upgraded, &key_new),
            "Chromium upgrade on same config shares GateKey so stale browser is evicted"
        );
    }
}
