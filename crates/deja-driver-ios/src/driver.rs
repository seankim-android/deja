//! Production iOS Simulator [`PlatformDriver`] implementation (`IosDriver`, `DEJA-64`).
//!
//! Combines `xcrun simctl` lifecycle management, warm `DejaXCTestRunner` XCUITest HTTP sessions
//! (reused across sequential flows with 0 ms re-attach), compound `/act` and `/waitForSelector`,
//! and native `.ips` crash report detection (`DEJA-46`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deja_core::{LaunchArgumentValue, provider_key_redactor};
use deja_flow::{
    ActAction, ActOutcome, ActPayload, DriverCapabilities, DriverCapability, DriverCrashSignal,
    DriverError, DriverFuture, KeyPress, LaunchOptions, ObservationSource, OpenLinkOptions,
    PlatformDriver, SwipeParams, UiHierarchy, UiNode, UiPoint, UiRect, UiSize,
    UnsupportedCapability, VisibleContext, WaitSelectorOutcome, WaitSelectorRequest,
    build_visible_context, wait_local,
};
use tokio::sync::Mutex;
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
use crate::process::validate_udid;
use crate::runner::{IosRunnerConfig, IosRunnerSession};
use crate::simctl::{
    Simctl, SimctlEnv, map_privacy_action, map_privacy_service, validate_bundle_id,
    validate_open_link_url,
};

const MAX_KEY_BATCH: u32 = 100;
const WARMUP_CHUNK_MS: u64 = 1_800;
const DEFAULT_LONG_PRESS_MS: u64 = 1_000;
const MAX_SERVER_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

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
        }
    }
}

#[derive(Debug)]
enum SessionState {
    Detached,
    Runner(Box<IosRunnerSession>),
    #[cfg(test)]
    Mock(AgentClient),
}

impl SessionState {
    fn client(&self) -> Option<AgentClient> {
        match self {
            Self::Detached => None,
            Self::Runner(s) => Some(s.client().clone()),
            #[cfg(test)]
            Self::Mock(c) => Some(c.clone()),
        }
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

/// Production iOS Simulator [`PlatformDriver`].
#[derive(Debug)]
pub struct IosDriver {
    udid: String,
    config: IosDriverConfig,
    simctl: Simctl,
    capabilities: DriverCapabilities,
    session: Mutex<SessionState>,
    start_lock: Mutex<()>,
    attach_count: AtomicU64,
    runner_pid: AtomicU32,
}

impl IosDriver {
    /// Creates a new [`IosDriver`] bound to a validated simulator UDID.
    ///
    /// Does not spawn `xcodebuild` until [`PlatformDriver::ensure_session`] (or an RPC requiring
    /// the companion runner) is called.
    pub fn new(config: IosDriverConfig) -> Result<Self> {
        let udid = validate_udid(&config.udid)?;
        let simctl = Simctl::with_env(config.env.clone());
        Ok(Self {
            udid,
            config,
            simctl,
            capabilities: ios_capabilities(),
            session: Mutex::new(SessionState::Detached),
            start_lock: Mutex::new(()),
            attach_count: AtomicU64::new(0),
            runner_pid: AtomicU32::new(0),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_mock_client_for_tests(
        config: IosDriverConfig,
        client: AgentClient,
    ) -> Result<Self> {
        let driver = Self::new(config)?;
        driver.attach_count.store(1, Ordering::SeqCst);
        let mut guard = driver
            .session
            .try_lock()
            .expect("fresh driver mutex must be unlocked");
        *guard = SessionState::Mock(client);
        drop(guard);
        Ok(driver)
    }

    #[cfg(test)]
    pub(crate) async fn has_active_session(&self) -> bool {
        !matches!(*self.session.lock().await, SessionState::Detached)
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

    async fn ensure_session_client(
        &self,
        cancel: &CancellationToken,
    ) -> std::result::Result<AgentClient, DriverError> {
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }

        // Fast path: clone existing client under a short Mutex lock and probe `/health` outside the lock.
        let existing_client = {
            let guard = self.session.lock().await;
            guard.client()
        };

        if let Some(client) = existing_client {
            let health_res = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                res = client.health() => res,
            };
            match health_res {
                Ok(h) if h.status == "ok" => {
                    self.runner_pid.store(h.pid, Ordering::SeqCst);
                    return Ok(client);
                }
                Ok(h) => {
                    debug!(
                        udid = %self.udid,
                        status = %h.status,
                        "warm DejaXCTestRunner reported non-ok health; recycling session"
                    );
                    self.invalidate_session_if_matching(client.base_url()).await;
                }
                Err(err) => {
                    debug!(
                        udid = %self.udid,
                        error = %err,
                        "warm DejaXCTestRunner health probe failed; recycling session"
                    );
                    self.invalidate_session_if_matching(client.base_url()).await;
                }
            }
        }

        // Serialize cold/recovery session starts so concurrent callers never race the simulator lock.
        let _start_guard = self.start_lock.lock().await;
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }

        // Re-check after acquiring `start_lock` in case a sibling task just started a healthy session.
        let raced_client = {
            let guard = self.session.lock().await;
            guard.client()
        };
        if let Some(client) = raced_client {
            let health_res = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                res = client.health() => res,
            };
            if let Ok(h) = health_res
                && h.status == "ok"
            {
                self.runner_pid.store(h.pid, Ordering::SeqCst);
                return Ok(client);
            }
            self.invalidate_session_if_matching(client.base_url()).await;
        }

        if self.config.auto_boot_simulator {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                res = self.simctl.ensure_booted(&self.udid) => {
                    res.map_err(|err| map_ios_err("ensure_booted", err))?;
                }
            }
        }

        let runner_cfg = IosRunnerConfig {
            udid: self.udid.clone(),
            deja_home: self.config.deja_home.clone(),
            env: self.config.env.clone(),
            startup_timeout: self.config.startup_timeout,
            request_timeout: self.config.request_timeout,
        };
        let session = IosRunnerSession::start(runner_cfg, cancel.clone())
            .await
            .map_err(|err| map_ios_err("ensure_session", err))?;

        let client = session.client().clone();
        let pid = session.agent_pid().unwrap_or(0);
        self.runner_pid.store(pid, Ordering::SeqCst);
        self.attach_count.fetch_add(1, Ordering::SeqCst);

        let previous = {
            let mut guard = self.session.lock().await;
            std::mem::replace(&mut *guard, SessionState::Runner(Box::new(session)))
        };
        if let SessionState::Runner(old) = previous {
            let _ = old.shutdown().await;
        }

        Ok(client)
    }

    async fn get_client_or_start(
        &self,
        cancel: &CancellationToken,
    ) -> std::result::Result<AgentClient, DriverError> {
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }
        let existing = {
            let guard = self.session.lock().await;
            guard.client()
        };
        if let Some(client) = existing {
            return Ok(client);
        }
        self.ensure_session_client(cancel).await
    }

    async fn invalidate_session_if_matching(&self, base_url: &str) {
        let taken = {
            let mut guard = self.session.lock().await;
            let matches = guard
                .client()
                .is_some_and(|client| client.base_url() == base_url);
            if matches {
                self.runner_pid.store(0, Ordering::SeqCst);
                Some(std::mem::replace(&mut *guard, SessionState::Detached))
            } else {
                None
            }
        };
        if let Some(SessionState::Runner(old)) = taken {
            let _ = old.shutdown().await;
        }
    }

    async fn call_agent<T, Fut>(
        &self,
        client: &AgentClient,
        operation: &str,
        fut: Fut,
        cancel: &CancellationToken,
    ) -> std::result::Result<T, DriverError>
    where
        Fut: Future<Output = Result<T>>,
    {
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }
        let started = Instant::now();
        let res = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(DriverError::Cancelled),
            res = fut => res,
        };
        match res {
            Ok(val) => Ok(val),
            Err(err) => {
                if is_dead_session_error(&err) {
                    self.invalidate_session_if_matching(client.base_url()).await;
                    return Err(DriverError::Transport {
                        operation: operation.to_string(),
                        detail: err.to_string(),
                        transient: false,
                    });
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
        client: &AgentClient,
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
                .call_agent(client, "press_key", client.press_key(&req), cancel)
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
        let client = self.get_client_or_start(cancel).await?;
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

        let resp = self
            .call_agent(
                &client,
                "wait_for_selector",
                client.wait_for_selector(&wire_req),
                cancel,
            )
            .await?;

        let ui_hierarchy = resp.hierarchy.as_ref().map(hierarchy_response_to_ui);
        let matched_node = resp.node.as_ref().map(hierarchy_node_to_ui_node);
        let visible_context = if resp.matched {
            VisibleContext::default()
        } else if !resp.visible_sample.is_empty() {
            scrub_visible_sample(
                &resp.visible_sample,
                req.redactor.as_ref(),
                req.provider_redactor.as_ref(),
            )
        } else if let Some(ref h) = ui_hierarchy {
            build_visible_context(
                &h.nodes,
                req.redactor.as_ref(),
                req.provider_redactor.as_ref(),
                VisibleContext::DEFAULT_LIMIT,
            )
        } else {
            VisibleContext::default()
        };

        let obtained_fresh =
            resp.matched || !resp.hierarchy_hash.is_empty() || ui_hierarchy.is_some();
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
            let _ = self.ensure_session_client(cancel).await?;
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
            let bundle_id = validate_bundle_id(app_id)
                .map_err(|err| DriverError::InvalidArgument(err.to_string()))?;

            // Pre-validate launch permissions and argument scalars before mutating simulator state.
            for (service, action) in &opts.permissions {
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

            let client = self.ensure_session_client(cancel).await?;

            if opts.clear_state {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    res = self.simctl.clear_state(&self.udid, &bundle_id) => {
                        res.map_err(|err| map_ios_err("clear_state", err))?;
                    }
                }
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

            if !opts.permissions.is_empty() {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    res = self.simctl.set_permissions(&self.udid, &bundle_id, &opts.permissions) => {
                        res.map_err(|err| map_ios_err("set_permissions", err))?;
                    }
                }
            }

            let launch_started_epoch_ms = now_epoch_ms();
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                res = self.simctl.launch_app(&self.udid, &bundle_id, &formatted_args, &opts.env) => {
                    res.map_err(|err| match err {
                        IosDriverError::InvalidArgument(_)
                        | IosDriverError::InvalidBundleId(_)
                        | IosDriverError::InvalidUdid(_) => {
                            DriverError::InvalidArgument(err.to_string())
                        }
                        other => DriverError::LaunchFailed {
                            app_id: bundle_id.clone(),
                            detail: other.to_string(),
                        },
                    })?;
                }
            }

            let deadline = Instant::now() + self.config.app_launch_timeout;
            let last_detail = loop {
                if cancel.is_cancelled() {
                    return Err(DriverError::Cancelled);
                }

                let remaining = deadline.saturating_duration_since(Instant::now());
                let chunk_ms = (remaining.as_millis() as u64).min(WARMUP_CHUNK_MS);
                let warmup_req = WarmupRequest {
                    app_id: bundle_id.clone(),
                    timeout_ms: Some(chunk_ms),
                    include_system_ui: Some(false),
                    return_hierarchy: Some(false),
                };

                let res = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    res = client.warmup(&warmup_req) => res,
                };

                let detail = match res {
                    Ok(resp) if resp.ok && resp.state == "running_foreground" => return Ok(()),
                    Ok(resp) if resp.state == "not_running" => {
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
                    Ok(resp) => format!("app state was `{}` (ok={})", resp.state, resp.ok),
                    Err(IosDriverError::AgentHttpStatus { status: 409, .. }) => {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                        }
                        "companion runner busy (HTTP 409) during warmup".to_string()
                    }
                    Err(IosDriverError::AgentHttpStatus {
                        status: 500, body, ..
                    }) => {
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
                        body
                    }
                    Err(err) => {
                        if is_dead_session_error(&err) {
                            self.invalidate_session_if_matching(client.base_url()).await;
                            return Err(DriverError::Transport {
                                operation: "launch_app_warmup".to_string(),
                                detail: err.to_string(),
                                transient: false,
                            });
                        }
                        return Err(DriverError::LaunchFailed {
                            app_id: bundle_id,
                            detail: err.to_string(),
                        });
                    }
                };

                if Instant::now() >= deadline {
                    break detail;
                }
            };

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
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(DriverError::Cancelled),
                res = self.simctl.clear_state(&self.udid, app_id) => {
                    res.map_err(|err| map_ios_err("clear_state", err))
                }
            }
        })
    }

    fn clear_keychain<'a>(&'a self, cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
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
            let client = self.get_client_or_start(cancel).await?;
            let resp = self
                .call_agent(
                    &client,
                    "hierarchy",
                    client.hierarchy_with_known_hash(app_id, include_system_ui, known_hash),
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
            let client = self.get_client_or_start(cancel).await?;
            let req = TapRequest {
                app_id: app_id.to_string(),
                x: point.x,
                y: point.y,
                count: Some(1),
                duration_ms: None,
            };
            let resp = self
                .call_agent(&client, "tap_point", client.tap(&req), cancel)
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
            let client = self.get_client_or_start(cancel).await?;
            let req = TapRequest {
                app_id: app_id.to_string(),
                x: point.x,
                y: point.y,
                count: Some(1),
                duration_ms: Some(duration_ms.unwrap_or(DEFAULT_LONG_PRESS_MS).max(1)),
            };
            let resp = self
                .call_agent(&client, "long_press_point", client.tap(&req), cancel)
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
            let client = self.get_client_or_start(cancel).await?;
            let req = TapRequest {
                app_id: app_id.to_string(),
                x: point.x,
                y: point.y,
                count: Some(2),
                duration_ms: None,
            };
            let resp = self
                .call_agent(&client, "double_tap_point", client.tap(&req), cancel)
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
            let client = self.get_client_or_start(cancel).await?;
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
                .call_agent(&client, "swipe", client.swipe(&req), cancel)
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
            let client = self.get_client_or_start(cancel).await?;
            let req = TypeRequest {
                app_id: app_id.to_string(),
                text: text.to_string(),
                settle_keyboard: Some(true),
            };
            let resp = self
                .call_agent(&client, "input_text", client.type_text(&req), cancel)
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
            let client = self.get_client_or_start(cancel).await?;
            self.send_key_batches(&client, app_id, "delete", count, cancel)
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
            let client = self.get_client_or_start(cancel).await?;
            self.send_key_batches(&client, app_id, &normalized, key.count, cancel)
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
            let Some(dismiss_point) = find_keyboard_dismiss_target(&tree.nodes) else {
                // No visible keyboard node at all.
                if !has_visible_keyboard(&tree.nodes) {
                    return Ok(());
                }
                return self
                    .press_key(app_id, &KeyPress::single("return"), cancel)
                    .await;
            };
            self.tap_point(app_id, dismiss_point, cancel).await
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

            // 2. If a runner session is live, probe `/appState`: when the app is still alive,
            //    skip the grace window completely (0 ms penalty on normal assertion failures).
            let existing_client = {
                let guard = self.session.lock().await;
                guard.client()
            };
            if let Some(client) = existing_client {
                let probe_timeout = Duration::from_secs(2).min(self.config.request_timeout);
                let state_res = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    timed = tokio::time::timeout(probe_timeout, client.app_state(app_id)) => timed,
                };
                if let Ok(Ok(state)) = state_res
                    && is_app_alive_state(&state.state)
                {
                    return Ok(None);
                }
            }

            // 3. App is `not_running`/`unknown` (or `/appState` could not confirm liveness):
            //    poll `.ips` reports through `crash_report_grace` so ReportCrash has time to flush.
            self.scan_crash_with_grace(
                app_id,
                since_epoch_ms,
                now_ms,
                self.config.crash_report_grace,
                cancel,
            )
            .await
        })
    }

    fn shutdown<'a>(&'a self) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let taken = {
                let mut guard = self.session.lock().await;
                self.runner_pid.store(0, Ordering::SeqCst);
                std::mem::replace(&mut *guard, SessionState::Detached)
            };
            match taken {
                SessionState::Runner(sess) => sess
                    .shutdown()
                    .await
                    .map_err(|err| map_ios_err("shutdown", err)),
                SessionState::Detached => Ok(()),
                #[cfg(test)]
                SessionState::Mock(_) => Ok(()),
            }
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
                    let client = self.get_client_or_start(cancel).await?;
                    let mut remaining = *count;
                    if remaining > MAX_KEY_BATCH {
                        let pre_count = remaining - ((remaining - 1) % MAX_KEY_BATCH + 1);
                        self.send_key_batches(&client, &req.app_id, "delete", pre_count, cancel)
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
                    let client = self.get_client_or_start(cancel).await?;
                    let mut remaining = *count;
                    if remaining > MAX_KEY_BATCH {
                        let pre_count = remaining - ((remaining - 1) % MAX_KEY_BATCH + 1);
                        self.send_key_batches(&client, &req.app_id, &normalized, pre_count, cancel)
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

            let client = self.get_client_or_start(cancel).await?;
            let wire_req = ActRequest {
                app_id: req.app_id.clone(),
                kind: act_kind,
                post_action_delay_ms: req.post_action_delay_ms,
                return_hierarchy: Some(req.return_hierarchy),
                include_system_ui: Some(req.include_system_ui),
                known_hash: req.known_hash.clone(),
            };
            let resp = self
                .call_agent(&client, "act_and_observe", client.act(&wire_req), cancel)
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
/// Explicitly omits [`DriverCapability::SystemBack`] and [`DriverCapability::DisableAnimations`].
fn ios_capabilities() -> DriverCapabilities {
    DriverCapabilities::with([
        DriverCapability::AccessibilityTree,
        DriverCapability::ClearState,
        DriverCapability::ClearKeychain,
        DriverCapability::CompoundAct,
        DriverCapability::ServerSideWait,
        DriverCapability::HierarchyHashShortCircuit,
        DriverCapability::HoldScroll,
        DriverCapability::AppCrashReports,
    ])
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
        checked: false,
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
            "hide keyboard" | "dismiss keyboard" | "dismiss" | "hide" | "done"
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

    struct MockAgent {
        port: u16,
        requests: Arc<StdMutex<Vec<RecordedRequest>>>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl MockAgent {
        async fn start(responder: Responder) -> Self {
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
                        let (status, body) = resp_fn(&recorded);
                        reqs.lock().unwrap().push(recorded);
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
            AgentClient::new(self.port, TEST_TOKEN.to_string(), Duration::from_secs(5))
                .expect("mock client")
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
        ] {
            assert!(caps.supports(expected), "missing capability {expected}");
        }
        assert!(!caps.supports(DriverCapability::SystemBack));
        assert!(!caps.supports(DriverCapability::DisableAnimations));
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
    async fn hide_keyboard_noop_dismiss_key_and_return_fallback() {
        let mode = Arc::new(StdMutex::new(0u8));
        let mode_clone = Arc::clone(&mode);

        let mock = MockAgent::start(Arc::new(move |req| {
            if req.target.starts_with("/hierarchy") {
                let m = *mode_clone.lock().unwrap();
                let nodes = match m {
                    0 => vec![sample_hierarchy_node(0, "Application", "", "App")],
                    1 => vec![
                        sample_hierarchy_node(0, "Application", "", "App"),
                        HierarchyNode {
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
                        },
                        HierarchyNode {
                            index: 2,
                            parent_index: Some(1),
                            depth: 2,
                            source: "app".into(),
                            element_type: "Button".into(),
                            identifier: "Hide keyboard".into(),
                            label: "Hide keyboard".into(),
                            title: String::new(),
                            value: None,
                            placeholder_value: None,
                            enabled: true,
                            selected: false,
                            has_focus: false,
                            hittable: true,
                            is_on_screen: Some(true),
                            frame: ElementFrame {
                                x: 320.0,
                                y: 740.0,
                                width: 60.0,
                                height: 40.0,
                            },
                        },
                    ],
                    _ => vec![
                        sample_hierarchy_node(0, "Application", "", "App"),
                        HierarchyNode {
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
                        },
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

        let driver =
            IosDriver::with_mock_client_for_tests(IosDriverConfig::new(TEST_UDID), mock.client())
                .expect("driver");
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

        // Mode 1: explicit Hide keyboard key -> taps center (350, 760).
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

        // Mode 2: keyboard without dismiss key -> presses return.
        *mode.lock().unwrap() = 2;
        driver
            .hide_keyboard("dev.deja.ios.checkout", &cancel)
            .await
            .expect("fallback return");
        let key = mock
            .recorded()
            .into_iter()
            .find(|r| r.target == "/key")
            .expect("expected /key return fallback");
        assert_eq!(key.body.as_ref().unwrap()["key"], json!("return"));
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
            driver.has_active_session().await,
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
        assert!(dead_driver.has_active_session().await);

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
            !dead_driver.has_active_session().await,
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
            permissions: perms,
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
        assert!(!driver.has_active_session().await);

        let _ = std::fs::remove_dir_all(&root);
    }
}
