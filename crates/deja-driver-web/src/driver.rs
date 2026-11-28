//! Production native Web [`PlatformDriver`] implementation (`WebDriver`) using direct Chromium
//! Chrome DevTools Protocol WebSockets with zero Maestro, Node, or Playwright dependencies.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use deja_core::CrashSignalSource;
use deja_flow::{
    ActAction, ActOutcome, ActPayload, DriverCapabilities, DriverCapability, DriverCrashSignal,
    DriverError, DriverFuture, KeyPress, LaunchOptions, OpenLinkOptions, PlatformDriver,
    SwipeParams, UiHierarchy, UiPoint, UnsupportedCapability,
};
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::browser::{BrowserLaunchOptions, ManagedBrowser, discover_working_chromium};
use crate::cdp::{
    CdpClient, PageSessionIds, RecordedCrash, create_and_attach_page_in_context,
    create_isolated_context_and_page, now_epoch_ms,
};
use crate::dom::{
    AX_DOM_SCRIPT, clipped_geometry_hash, decode_base64_png, dom_snapshot_to_hierarchy,
};
use crate::error::{Result, WebDriverError, map_web_err};
use crate::input::{
    CdpKeySpec, MAX_KEY_REPEAT_COUNT, ResolvedKeyAction, ScreenSize, backspace_key_spec,
    resolve_key_press, validate_loopback_http_url,
};

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_NAVIGATION_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_NETWORK_QUIET_WINDOW: Duration = Duration::from_millis(150);
const DEFAULT_NETWORK_SETTLE_CAP: Duration = Duration::from_millis(2_000);
const BROWSER_CLOSE_TIMEOUT: Duration = Duration::from_millis(1_500);
const DEFAULT_LONG_PRESS_MS: u64 = 600;
const MAX_LONG_PRESS_MS: u64 = 10_000;
const RAF_SETTLE_EXPRESSION: &str =
    "new Promise(r => requestAnimationFrame(() => requestAnimationFrame(() => r(true))))";
const SINGLE_RAF_EXPRESSION: &str = "new Promise(r => requestAnimationFrame(() => r(true)))";
const READY_STATE_EXPRESSION: &str = "document.readyState";

/// Configuration for [`WebDriver`].
#[derive(Debug, Clone)]
pub struct WebDriverConfig {
    /// Launch Chromium with `--headless=new` (default `true`).
    pub headless: bool,
    /// Viewport dimensions applied via `--window-size` and `Emulation.setDeviceMetricsOverride`.
    pub screen_size: ScreenSize,
    /// When `true` (default), emulates `prefers-reduced-motion: reduce` and advertises
    /// [`DriverCapability::DisableAnimations`].
    pub disable_animations: bool,
    /// Startup budget for spawning Chromium and connecting to its DevTools WebSocket.
    pub startup_timeout: Duration,
    /// Per-RPC timeout for CDP requests.
    pub request_timeout: Duration,
    /// Timeout for page navigation + load event + network + rAF settle.
    pub navigation_timeout: Duration,
    /// Environment map used for Chromium binary discovery (`DEJA_CHROME`, `CHROME_PATH`, `PATH`).
    pub env: BTreeMap<String, String>,
    #[cfg(test)]
    pub(crate) cdp_ws_url_override: Option<String>,
    #[cfg(test)]
    pub(crate) network_quiet_window: Duration,
    #[cfg(test)]
    pub(crate) network_settle_cap: Duration,
}

impl Default for WebDriverConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl WebDriverConfig {
    /// Creates a [`WebDriverConfig`] with headless `1280x720`, `disable_animations: true`, and
    /// 30-second startup/request/navigation timeouts.
    #[must_use]
    pub fn new() -> Self {
        Self {
            headless: true,
            screen_size: ScreenSize::default(),
            disable_animations: true,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            navigation_timeout: DEFAULT_NAVIGATION_TIMEOUT,
            env: BTreeMap::new(),
            #[cfg(test)]
            cdp_ws_url_override: None,
            #[cfg(test)]
            network_quiet_window: DEFAULT_NETWORK_QUIET_WINDOW,
            #[cfg(test)]
            network_settle_cap: DEFAULT_NETWORK_SETTLE_CAP,
        }
    }

    #[must_use]
    pub fn with_headless(mut self, headless: bool) -> Self {
        self.headless = headless;
        self
    }

    #[must_use]
    pub fn with_screen_size(mut self, screen_size: ScreenSize) -> Self {
        self.screen_size = screen_size;
        self
    }

    #[must_use]
    pub fn with_disable_animations(mut self, disable_animations: bool) -> Self {
        self.disable_animations = disable_animations;
        self
    }

    #[must_use]
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }

    #[must_use]
    pub fn with_startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_navigation_timeout(mut self, timeout: Duration) -> Self {
        self.navigation_timeout = timeout;
        self
    }
}

#[derive(Debug, Clone)]
struct ActiveHandle {
    cdp: CdpClient,
    ids: PageSessionIds,
}

/// Native Chromium CDP [`PlatformDriver`].
#[derive(Debug)]
pub struct WebDriver {
    config: WebDriverConfig,
    capabilities: DriverCapabilities,
    active: StdMutex<Option<ActiveHandle>>,
    browser: AsyncMutex<Option<ManagedBrowser>>,
    lifecycle: AsyncMutex<()>,
    crashes: Arc<StdMutex<Option<RecordedCrash>>>,
    browser_exit_crash: StdMutex<Option<RecordedCrash>>,
    attach_count: AtomicU64,
    /// `true` once the active `BrowserContext` has performed navigation or mutating UI actions;
    /// triggers fresh `BrowserContext` rotation on the next [`PlatformDriver::ensure_session`]
    /// while keeping Chromium warm.
    context_dirty: AtomicBool,
    /// `true` once an active session crashes, detaches, disconnects, or fails mid-rotation.
    /// While `true`, mid-flow operations fail closed instead of silently respawning `about:blank`;
    /// only an explicit [`PlatformDriver::ensure_session`] (or [`PlatformDriver::shutdown`]) clears it.
    session_broken: AtomicBool,
    /// Last non-truncated `(hierarchy_hash, clipped_geometry_hash)` observed on the active page,
    /// ensuring `known_hash` short-circuits invalidate whenever any node's `visible_frame` changes
    /// even if unclipped `frame` & `hittable` are unchanged.
    last_clipped_hash: StdMutex<Option<(String, u64)>>,
}

impl WebDriver {
    /// Creates a new [`WebDriver`] without spawning Chromium until [`PlatformDriver::ensure_session`]
    /// (or any operation requiring an active browser session) is called.
    #[must_use]
    pub fn new(config: WebDriverConfig) -> Self {
        let capabilities = web_capabilities(config.disable_animations);
        Self {
            config,
            capabilities,
            active: StdMutex::new(None),
            browser: AsyncMutex::new(None),
            lifecycle: AsyncMutex::new(()),
            crashes: Arc::new(StdMutex::new(None)),
            browser_exit_crash: StdMutex::new(None),
            attach_count: AtomicU64::new(0),
            context_dirty: AtomicBool::new(false),
            session_broken: AtomicBool::new(false),
            last_clipped_hash: StdMutex::new(None),
        }
    }

    fn network_quiet_window(&self) -> Duration {
        #[cfg(test)]
        {
            self.config.network_quiet_window
        }
        #[cfg(not(test))]
        {
            DEFAULT_NETWORK_QUIET_WINDOW
        }
    }

    fn network_settle_cap(&self) -> Duration {
        #[cfg(test)]
        {
            self.config.network_settle_cap
        }
        #[cfg(not(test))]
        {
            DEFAULT_NETWORK_SETTLE_CAP
        }
    }

    fn mark_context_dirty(&self) {
        self.context_dirty.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn with_cdp_url_for_tests(mut config: WebDriverConfig, ws_url: String) -> Self {
        config.cdp_ws_url_override = Some(ws_url);
        Self::new(config)
    }

    /// Number of times this driver attached/launched a Chromium CDP session.
    #[must_use]
    pub fn attach_count(&self) -> u64 {
        self.attach_count.load(Ordering::SeqCst)
    }

    fn current_handle(&self) -> Option<ActiveHandle> {
        self.active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn clear_clipped_hash(&self) {
        let mut guard = self
            .last_clipped_hash
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *guard = None;
    }

    fn set_handle(&self, handle: Option<ActiveHandle>) {
        let mut guard = self.active.lock().unwrap_or_else(|p| p.into_inner());
        *guard = handle;
        drop(guard);
        self.clear_clipped_hash();
    }

    fn record_browser_exit_crash(&self, status: Option<i32>) {
        let crash = RecordedCrash {
            epoch_ms: now_epoch_ms(),
            process: "chromium".to_string(),
            detail: format!("Chromium process exited unexpectedly (exit={status:?})"),
        };
        {
            let mut exit_guard = self
                .browser_exit_crash
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            *exit_guard = Some(crash.clone());
        }
        let mut guard = self.crashes.lock().unwrap_or_else(|p| p.into_inner());
        *guard = Some(crash);
    }

    fn current_session_crash(&self, handle: Option<&ActiveHandle>) -> Option<RecordedCrash> {
        if let Some(crash) = handle.and_then(|h| h.cdp.session_crash()) {
            return Some(crash);
        }
        self.browser_exit_crash
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn broken_session_error(&self, handle: Option<&ActiveHandle>, fallback: &str) -> DriverError {
        if let Some(crash) = self.current_session_crash(handle) {
            return DriverError::AppCrashed(DriverCrashSignal {
                source: CrashSignalSource::CrashReport,
                report_file: None,
                process: Some(crash.process),
                detail: Some(crash.detail),
            });
        }
        DriverError::SessionClosed(fallback.to_string())
    }

    fn map_active_err(
        &self,
        handle: &ActiveHandle,
        operation: &str,
        err: WebDriverError,
    ) -> DriverError {
        if matches!(
            err,
            WebDriverError::CdpClosed(_)
                | WebDriverError::TargetCrashed { .. }
                | WebDriverError::WebSocket(_)
        ) || handle.cdp.is_closed()
            || handle.cdp.is_detached()
        {
            self.session_broken.store(true, Ordering::SeqCst);
            if let Some(crash) = self.current_session_crash(Some(handle)) {
                return DriverError::AppCrashed(DriverCrashSignal {
                    source: CrashSignalSource::CrashReport,
                    report_file: None,
                    process: Some(crash.process),
                    detail: Some(crash.detail),
                });
            }
        }
        map_web_err(operation, err)
    }

    async fn poll_unexpected_browser_exit(&self) -> bool {
        let mut guard = self.browser.lock().await;
        if let Some(browser) = guard.as_mut()
            && let Some(status) = browser.try_check_exit()
        {
            self.record_browser_exit_crash(status);
            return true;
        }
        false
    }

    /// Returns the active, healthy browser handle for mid-flow operations without EVER silently
    /// respawning a crashed, detached, or unexpectedly closed browser session.
    ///
    /// Lazily attaches only when the driver has no session yet and has not entered a broken state;
    /// once a session crashes or disconnects, callers receive `AppCrashed` / `SessionClosed`
    /// immediately until an explicit [`PlatformDriver::ensure_session`] recovers it.
    async fn require_active_handle(
        &self,
        cancel: &CancellationToken,
    ) -> std::result::Result<ActiveHandle, DriverError> {
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }

        if self.poll_unexpected_browser_exit().await {
            self.session_broken.store(true, Ordering::SeqCst);
            let current = self.current_handle();
            return Err(self.broken_session_error(
                current.as_ref(),
                "Chromium process exited unexpectedly during active session",
            ));
        }

        if let Some(handle) = self.current_handle() {
            if handle.cdp.is_detached() {
                self.session_broken.store(true, Ordering::SeqCst);
                return Err(self.broken_session_error(
                    Some(&handle),
                    "Chromium target detached unexpectedly during active session",
                ));
            }
            if handle.cdp.is_closed() {
                self.session_broken.store(true, Ordering::SeqCst);
                let _ = self.poll_unexpected_browser_exit().await;
                return Err(self.broken_session_error(
                    Some(&handle),
                    "CDP WebSocket closed unexpectedly during active session",
                ));
            }
            if self.session_broken.load(Ordering::SeqCst) {
                return Err(self.broken_session_error(
                    Some(&handle),
                    "Chromium session is unhealthy; call ensure_session to recover",
                ));
            }
            return Ok(handle);
        }

        if self.session_broken.load(Ordering::SeqCst) {
            return Err(self.broken_session_error(
                None,
                "Chromium session crashed or closed; call ensure_session to recover",
            ));
        }

        // Brand-new or cleanly shut-down driver: lazily initialize a fresh session.
        self.ensure_or_recover_session(cancel).await
    }

    /// Ensures a healthy warm browser session exists, tearing down and recovering any crashed,
    /// detached, or disconnected session. Invoked explicitly by [`PlatformDriver::ensure_session`]
    /// (and lazily on first use of a brand-new driver).
    async fn ensure_or_recover_session(
        &self,
        cancel: &CancellationToken,
    ) -> std::result::Result<ActiveHandle, DriverError> {
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }

        // Fast path: reuse healthy warm browser + page session with zero relaunch.
        if !self.session_broken.load(Ordering::SeqCst)
            && let Some(handle) = self.current_handle()
            && !handle.cdp.is_closed()
            && !handle.cdp.is_detached()
            && !self.poll_unexpected_browser_exit().await
        {
            return Ok(handle);
        }

        let _lifecycle = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(DriverError::Cancelled),
            guard = self.lifecycle.lock() => guard,
        };
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }

        // Re-check under `lifecycle` in case a concurrent caller already initialized the session.
        if !self.session_broken.load(Ordering::SeqCst)
            && let Some(handle) = self.current_handle()
            && !handle.cdp.is_closed()
            && !handle.cdp.is_detached()
            && !self.poll_unexpected_browser_exit().await
        {
            return Ok(handle);
        }

        // Tear down any stale/crashed/detached browser before starting a clean replacement.
        self.teardown_locked().await;
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }

        let (managed_browser, ws_url) = self
            .spawn_or_override_browser(cancel)
            .await
            .map_err(|err| map_web_err("ensure_session", err))?;

        let connect_res = async {
            let cdp = CdpClient::connect(
                &ws_url,
                self.config.startup_timeout,
                Arc::clone(&self.crashes),
                cancel,
            )
            .await?;
            let ids = create_isolated_context_and_page(
                &cdp,
                self.config.screen_size,
                self.config.disable_animations,
                self.config.request_timeout,
                cancel,
            )
            .await?;
            Ok::<_, WebDriverError>(ActiveHandle { cdp, ids })
        }
        .await;

        match connect_res {
            Ok(handle) => {
                {
                    let mut b_guard = self.browser.lock().await;
                    *b_guard = managed_browser;
                }
                self.set_handle(Some(handle.clone()));
                self.context_dirty.store(false, Ordering::SeqCst);
                self.session_broken.store(false, Ordering::SeqCst);
                self.attach_count.fetch_add(1, Ordering::SeqCst);
                debug!(
                    attach_count = self.attach_count(),
                    target_id = %handle.ids.target_id,
                    "attached Chromium CDP session"
                );
                Ok(handle)
            }
            Err(err) => {
                if let Some(mut browser) = managed_browser {
                    browser.terminate_now().await;
                }
                Err(map_web_err("ensure_session", err))
            }
        }
    }

    async fn spawn_or_override_browser(
        &self,
        cancel: &CancellationToken,
    ) -> Result<(Option<ManagedBrowser>, String)> {
        #[cfg(test)]
        if let Some(ref ws_url) = self.config.cdp_ws_url_override {
            return Ok((None, ws_url.clone()));
        }

        let (executable, _version) = discover_working_chromium(&self.config.env, cancel).await?;
        let launch_opts = BrowserLaunchOptions {
            executable,
            headless: self.config.headless,
            screen_size: self.config.screen_size,
            startup_timeout: self.config.startup_timeout,
            env: self.config.env.clone(),
        };
        let browser = ManagedBrowser::launch(&launch_opts, cancel).await?;
        let ws_url = browser.ws_url().to_string();
        Ok((Some(browser), ws_url))
    }

    async fn teardown_locked(&self) {
        self.context_dirty.store(false, Ordering::SeqCst);
        self.session_broken.store(false, Ordering::SeqCst);
        {
            let mut exit_guard = self
                .browser_exit_crash
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            *exit_guard = None;
        }
        self.clear_clipped_hash();
        let old_handle = {
            let mut guard = self.active.lock().unwrap_or_else(|p| p.into_inner());
            guard.take()
        };
        let old_browser = {
            let mut guard = self.browser.lock().await;
            guard.take()
        };

        if let Some(handle) = old_handle {
            handle.cdp.set_active_target(None, None);
            if old_browser.is_some() && !handle.cdp.is_closed() {
                let noop_cancel = CancellationToken::new();
                let _ = handle
                    .cdp
                    .call_browser(
                        "Browser.close",
                        json!({}),
                        BROWSER_CLOSE_TIMEOUT,
                        &noop_cancel,
                    )
                    .await;
            }
            handle.cdp.close().await;
        }

        if let Some(mut browser) = old_browser {
            browser.shutdown_after_cdp_close().await;
        }
    }

    /// Disposes the current isolated `BrowserContext` and creates a brand-new `BrowserContext` +
    /// page target on the SAME warm browser process (zero Chromium respawn).
    ///
    /// Invalidates the cached [`ActiveHandle`] BEFORE disposing so any failure or cancellation
    /// mid-replacement can never leave callers reusing a disposed context/target.
    async fn reset_browser_context_locked(
        &self,
        handle: &ActiveHandle,
        cancel: &CancellationToken,
    ) -> Result<ActiveHandle> {
        self.set_handle(None);
        self.session_broken.store(true, Ordering::SeqCst);
        self.context_dirty.store(false, Ordering::SeqCst);
        // Clear active target before disposing so intentional detach is never misclassified.
        handle.cdp.set_active_target(None, None);
        handle.cdp.reset_network_state();
        if let Err(WebDriverError::Cancelled) = handle
            .cdp
            .call_browser(
                "Target.disposeBrowserContext",
                json!({ "browserContextId": handle.ids.browser_context_id }),
                self.config.request_timeout,
                cancel,
            )
            .await
        {
            return Err(WebDriverError::Cancelled);
        }

        let new_ids = create_isolated_context_and_page(
            &handle.cdp,
            self.config.screen_size,
            self.config.disable_animations,
            self.config.request_timeout,
            cancel,
        )
        .await?;
        let new_handle = ActiveHandle {
            cdp: handle.cdp.clone(),
            ids: new_ids,
        };
        self.set_handle(Some(new_handle.clone()));
        self.session_broken.store(false, Ordering::SeqCst);
        self.context_dirty.store(false, Ordering::SeqCst);
        Ok(new_handle)
    }

    /// Closes the current page target and opens a fresh `about:blank` target inside the SAME
    /// `BrowserContext`, preserving cookies / `localStorage` / `IndexedDB` when `clear_state: false`.
    ///
    /// Invalidates the cached [`ActiveHandle`] BEFORE closing the old target so any failure or
    /// cancellation mid-replacement forces clean recovery rather than reusing a closed target.
    async fn replace_target_same_context_locked(
        &self,
        handle: &ActiveHandle,
        cancel: &CancellationToken,
    ) -> Result<ActiveHandle> {
        self.set_handle(None);
        self.session_broken.store(true, Ordering::SeqCst);
        handle.cdp.set_active_target(None, None);
        handle.cdp.reset_network_state();
        if let Err(WebDriverError::Cancelled) = handle
            .cdp
            .call_browser(
                "Target.closeTarget",
                json!({ "targetId": handle.ids.target_id }),
                self.config.request_timeout,
                cancel,
            )
            .await
        {
            return Err(WebDriverError::Cancelled);
        }

        let (target_id, session_id) = create_and_attach_page_in_context(
            &handle.cdp,
            &handle.ids.browser_context_id,
            self.config.screen_size,
            self.config.disable_animations,
            self.config.request_timeout,
            cancel,
        )
        .await?;
        let new_handle = ActiveHandle {
            cdp: handle.cdp.clone(),
            ids: PageSessionIds {
                browser_context_id: handle.ids.browser_context_id.clone(),
                target_id,
                session_id,
            },
        };
        self.set_handle(Some(new_handle.clone()));
        self.session_broken.store(false, Ordering::SeqCst);
        Ok(new_handle)
    }

    async fn navigate_and_settle(
        &self,
        handle: &ActiveHandle,
        url: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.mark_context_dirty();
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + self.config.navigation_timeout;
        let seq_before = handle.cdp.load_seq();

        let nav_res = handle
            .cdp
            .call_session(
                &handle.ids.session_id,
                "Page.navigate",
                json!({ "url": url }),
                self.config.navigation_timeout,
                cancel,
            )
            .await?;

        if let Some(error_text) = nav_res
            .get("errorText")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return Err(WebDriverError::NavigationFailed {
                target: url.to_string(),
                detail: error_text.to_string(),
            });
        }

        // Wait for `Page.loadEventFired` / `Page.frameStoppedLoading` when a full document
        // navigation occurred (`loaderId` present), otherwise fall straight through to settle.
        let is_document_navigation = nav_res
            .get("loaderId")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());

        if is_document_navigation {
            self.wait_for_load_event(handle, seq_before, deadline, started, cancel)
                .await?;
        }

        self.wait_ready_state_network_and_raf(handle, deadline, started, cancel)
            .await
    }

    async fn wait_for_history_commit(
        &self,
        handle: &ActiveHandle,
        nav_before: u64,
        deadline: tokio::time::Instant,
        started: Instant,
        cancel: &CancellationToken,
    ) -> Result<()> {
        loop {
            if cancel.is_cancelled() {
                return Err(WebDriverError::Cancelled);
            }
            if handle.cdp.nav_seq() > nav_before {
                return Ok(());
            }
            if handle.cdp.is_closed() {
                return Err(WebDriverError::CdpClosed(
                    "CDP closed while waiting for history navigation commit".to_string(),
                ));
            }
            if handle.cdp.is_detached() {
                return Err(WebDriverError::TargetCrashed {
                    detail: "target detached while waiting for history navigation commit"
                        .to_string(),
                });
            }

            let nav_notified = handle.cdp.nav_notify().notified();
            tokio::pin!(nav_notified);
            nav_notified.as_mut().enable();

            if handle.cdp.nav_seq() > nav_before {
                return Ok(());
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(WebDriverError::Timeout {
                    operation: "history_commit".to_string(),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    detail: format!(
                        "timed out waiting for history traversal commit after {}ms",
                        self.config.navigation_timeout.as_millis()
                    ),
                });
            }

            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(WebDriverError::Cancelled),
                _ = nav_notified => {}
                _ = tokio::time::sleep(remaining) => {
                    return Err(WebDriverError::Timeout {
                        operation: "history_commit".to_string(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        detail: format!(
                            "timed out waiting for history traversal commit after {}ms",
                            self.config.navigation_timeout.as_millis()
                        ),
                    });
                }
            }
        }
    }

    async fn wait_for_load_event(
        &self,
        handle: &ActiveHandle,
        seq_before: u64,
        deadline: tokio::time::Instant,
        started: Instant,
        cancel: &CancellationToken,
    ) -> Result<()> {
        loop {
            if cancel.is_cancelled() {
                return Err(WebDriverError::Cancelled);
            }
            if handle.cdp.load_seq() > seq_before {
                return Ok(());
            }
            if handle.cdp.is_closed() {
                return Err(WebDriverError::CdpClosed(
                    "CDP closed while waiting for page load".to_string(),
                ));
            }
            if handle.cdp.is_detached() {
                return Err(WebDriverError::TargetCrashed {
                    detail: "target detached while waiting for page load".to_string(),
                });
            }

            let notified = handle.cdp.load_notify().notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if handle.cdp.load_seq() > seq_before {
                return Ok(());
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(WebDriverError::Timeout {
                    operation: "page_load".to_string(),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    detail: format!(
                        "timed out waiting for Page.loadEventFired after {}ms",
                        self.config.navigation_timeout.as_millis()
                    ),
                });
            }

            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(WebDriverError::Cancelled),
                _ = notified => {}
                _ = tokio::time::sleep(remaining) => {
                    return Err(WebDriverError::Timeout {
                        operation: "page_load".to_string(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        detail: format!(
                            "timed out waiting for Page.loadEventFired after {}ms",
                            self.config.navigation_timeout.as_millis()
                        ),
                    });
                }
            }
        }
    }

    /// Waits for `in_flight == 0` for `network_quiet_window`, hard-capped at `network_settle_cap`
    /// (and outer navigation `deadline`) so SSE/long-polling connections never hang navigation.
    async fn wait_network_quiet(
        &self,
        handle: &ActiveHandle,
        nav_deadline: tokio::time::Instant,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let quiet_window = self.network_quiet_window();
        let cap = self.network_settle_cap();
        if cap.is_zero() {
            return Ok(());
        }
        let settle_start = Instant::now();
        let cap_deadline = (tokio::time::Instant::now() + cap).min(nav_deadline);

        loop {
            if cancel.is_cancelled() {
                return Err(WebDriverError::Cancelled);
            }
            if handle.cdp.is_closed() || handle.cdp.is_detached() {
                return Ok(());
            }

            let (in_flight, last_activity) = handle.cdp.network_snapshot();
            let remaining_cap = cap_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining_cap.is_zero() {
                return Ok(());
            }

            if in_flight == 0 {
                let effective_last = last_activity.max(settle_start);
                let quiet_elapsed = Instant::now().saturating_duration_since(effective_last);
                if quiet_elapsed >= quiet_window {
                    return Ok(());
                }
                let wait_for = quiet_window
                    .saturating_sub(quiet_elapsed)
                    .min(remaining_cap);

                let notified = handle.cdp.network_notify().notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let (in_flight2, _) = handle.cdp.network_snapshot();
                if in_flight2 > 0 {
                    continue;
                }

                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(WebDriverError::Cancelled),
                    _ = notified => {}
                    _ = tokio::time::sleep(wait_for) => {}
                }
            } else {
                let notified = handle.cdp.network_notify().notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let (in_flight2, _) = handle.cdp.network_snapshot();
                if in_flight2 == 0 {
                    continue;
                }

                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(WebDriverError::Cancelled),
                    _ = notified => {}
                    _ = tokio::time::sleep(remaining_cap) => return Ok(()),
                }
            }
        }
    }

    async fn wait_ready_state_network_and_raf(
        &self,
        handle: &ActiveHandle,
        deadline: tokio::time::Instant,
        started: Instant,
        cancel: &CancellationToken,
    ) -> Result<()> {
        // Verify `document.readyState === "complete"` (polling briefly if `frameStoppedLoading`
        // raced just ahead of `complete`).
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(WebDriverError::Timeout {
                    operation: "document_ready_state".to_string(),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    detail: "timed out waiting for document.readyState === \"complete\""
                        .to_string(),
                });
            }
            let ready = self
                .eval_by_value(handle, READY_STATE_EXPRESSION, false, remaining, cancel)
                .await?;
            if ready.as_str() == Some("complete") {
                break;
            }
            let step = Duration::from_millis(25).min(remaining);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(WebDriverError::Cancelled),
                _ = tokio::time::sleep(step) => {}
            }
        }

        // Bounded network quiet settle so post-load fetches complete before returning.
        self.wait_network_quiet(handle, deadline, cancel).await?;

        // 2x requestAnimationFrame settle so layout/paint following network updates has completed.
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        let _ = self
            .eval_by_value(handle, RAF_SETTLE_EXPRESSION, true, remaining, cancel)
            .await?;
        Ok(())
    }

    async fn eval_by_value(
        &self,
        handle: &ActiveHandle,
        expression: &str,
        await_promise: bool,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let resp = handle
            .cdp
            .call_session(
                &handle.ids.session_id,
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": await_promise,
                }),
                timeout,
                cancel,
            )
            .await?;

        if let Some(exc) = resp.get("exceptionDetails") {
            let detail = exc
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .or_else(|| exc.get("text").and_then(Value::as_str))
                .unwrap_or("JavaScript evaluation threw an exception");
            return Err(WebDriverError::CdpProtocol {
                method: "Runtime.evaluate".to_string(),
                code: -32000,
                message: detail.to_string(),
            });
        }

        Ok(resp
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    async fn dispatch_mouse_event(
        &self,
        handle: &ActiveHandle,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let _ = handle
            .cdp
            .call_session(
                &handle.ids.session_id,
                "Input.dispatchMouseEvent",
                params,
                self.config.request_timeout,
                cancel,
            )
            .await?;
        Ok(())
    }

    async fn dispatch_key_spec(
        &self,
        handle: &ActiveHandle,
        spec: &CdpKeySpec,
        count: u32,
        cancel: &CancellationToken,
    ) -> Result<()> {
        for _ in 0..count {
            if cancel.is_cancelled() {
                return Err(WebDriverError::Cancelled);
            }
            let mut down = json!({
                "type": "keyDown",
                "key": spec.key,
                "code": spec.code,
                "windowsVirtualKeyCode": spec.windows_virtual_key_code,
                "nativeVirtualKeyCode": spec.windows_virtual_key_code,
                "modifiers": spec.modifiers,
            });
            if let Some(obj) = down.as_object_mut() {
                if let Some(ref text) = spec.text {
                    obj.insert("text".to_string(), Value::String(text.clone()));
                }
                if let Some(ref unmodified) = spec.unmodified_text {
                    obj.insert(
                        "unmodifiedText".to_string(),
                        Value::String(unmodified.clone()),
                    );
                }
            }
            let _ = handle
                .cdp
                .call_session(
                    &handle.ids.session_id,
                    "Input.dispatchKeyEvent",
                    down,
                    self.config.request_timeout,
                    cancel,
                )
                .await?;

            let up = json!({
                "type": "keyUp",
                "key": spec.key,
                "code": spec.code,
                "windowsVirtualKeyCode": spec.windows_virtual_key_code,
                "nativeVirtualKeyCode": spec.windows_virtual_key_code,
                "modifiers": spec.modifiers,
            });
            let _ = handle
                .cdp
                .call_session(
                    &handle.ids.session_id,
                    "Input.dispatchKeyEvent",
                    up,
                    self.config.request_timeout,
                    cancel,
                )
                .await?;
        }
        Ok(())
    }

    async fn fetch_hierarchy_with_handle(
        &self,
        handle: &ActiveHandle,
        app_id: &str,
        known_hash: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<UiHierarchy> {
        let value = self
            .eval_by_value(
                handle,
                AX_DOM_SCRIPT,
                false,
                self.config.request_timeout,
                cancel,
            )
            .await?;
        let mut hierarchy = dom_snapshot_to_hierarchy(value, app_id, None)?;
        let current_clipped = clipped_geometry_hash(&hierarchy.nodes);

        let mut last_clipped = self
            .last_clipped_hash
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if !hierarchy.truncated
            && let Some(expected) = known_hash
            && expected.eq_ignore_ascii_case(&hierarchy.hierarchy_hash)
            && last_clipped
                .as_ref()
                .is_some_and(|(prev_hash, prev_clipped)| {
                    prev_hash.eq_ignore_ascii_case(&hierarchy.hierarchy_hash)
                        && *prev_clipped == current_clipped
                })
        {
            hierarchy.unchanged = true;
            hierarchy.nodes.clear();
        } else if !hierarchy.truncated {
            *last_clipped = Some((hierarchy.hierarchy_hash.clone(), current_clipped));
        } else {
            *last_clipped = None;
        }
        Ok(hierarchy)
    }

    async fn tap_with_handle(
        &self,
        handle: &ActiveHandle,
        point: UiPoint,
        cancel: &CancellationToken,
    ) -> Result<()> {
        validate_finite_point(point, "tap")?;
        self.mark_context_dirty();
        self.dispatch_mouse_event(
            handle,
            json!({ "type": "mouseMoved", "x": point.x, "y": point.y, "button": "none", "buttons": 0 }),
            cancel,
        )
        .await?;
        self.dispatch_mouse_event(
            handle,
            json!({ "type": "mousePressed", "x": point.x, "y": point.y, "button": "left", "buttons": 1, "clickCount": 1 }),
            cancel,
        )
        .await?;
        self.dispatch_mouse_event(
            handle,
            json!({ "type": "mouseReleased", "x": point.x, "y": point.y, "button": "left", "buttons": 0, "clickCount": 1 }),
            cancel,
        )
        .await
    }

    async fn double_tap_with_handle(
        &self,
        handle: &ActiveHandle,
        point: UiPoint,
        cancel: &CancellationToken,
    ) -> Result<()> {
        validate_finite_point(point, "doubleTap")?;
        self.mark_context_dirty();
        self.dispatch_mouse_event(
            handle,
            json!({ "type": "mouseMoved", "x": point.x, "y": point.y, "button": "none", "buttons": 0 }),
            cancel,
        )
        .await?;
        for click_count in [1u32, 2u32] {
            self.dispatch_mouse_event(
                handle,
                json!({ "type": "mousePressed", "x": point.x, "y": point.y, "button": "left", "buttons": 1, "clickCount": click_count }),
                cancel,
            )
            .await?;
            self.dispatch_mouse_event(
                handle,
                json!({ "type": "mouseReleased", "x": point.x, "y": point.y, "button": "left", "buttons": 0, "clickCount": click_count }),
                cancel,
            )
            .await?;
        }
        Ok(())
    }

    async fn long_press_with_handle(
        &self,
        handle: &ActiveHandle,
        point: UiPoint,
        duration_ms: Option<u64>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        validate_finite_point(point, "longPress")?;
        self.mark_context_dirty();
        let hold_ms = duration_ms
            .unwrap_or(DEFAULT_LONG_PRESS_MS)
            .min(MAX_LONG_PRESS_MS);
        self.dispatch_mouse_event(
            handle,
            json!({ "type": "mouseMoved", "x": point.x, "y": point.y, "button": "none", "buttons": 0 }),
            cancel,
        )
        .await?;
        self.dispatch_mouse_event(
            handle,
            json!({ "type": "mousePressed", "x": point.x, "y": point.y, "button": "left", "buttons": 1, "clickCount": 1 }),
            cancel,
        )
        .await?;
        if hold_ms > 0 {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(WebDriverError::Cancelled),
                _ = tokio::time::sleep(Duration::from_millis(hold_ms)) => {}
            }
        }
        self.dispatch_mouse_event(
            handle,
            json!({ "type": "mouseReleased", "x": point.x, "y": point.y, "button": "left", "buttons": 0, "clickCount": 1 }),
            cancel,
        )
        .await
    }

    async fn swipe_with_handle(
        &self,
        handle: &ActiveHandle,
        params: &SwipeParams,
        cancel: &CancellationToken,
    ) -> Result<()> {
        validate_finite_point(params.start, "swipe.start")?;
        validate_finite_point(params.end, "swipe.end")?;
        self.mark_context_dirty();

        // Sign convention: dragging finger UP (`start.y > end.y`) scrolls content DOWN (`deltaY > 0`).
        let total_dx = params.start.x - params.end.x;
        let total_dy = params.start.y - params.end.y;
        let steps = ((params.duration_ms / 40).clamp(1, 10)) as usize;
        let step_dx = total_dx / (steps as f64);
        let step_dy = total_dy / (steps as f64);
        let per_step_delay = if steps > 1 && params.duration_ms > 0 {
            Duration::from_millis((params.duration_ms / (steps as u64)).min(50))
        } else {
            Duration::ZERO
        };

        for idx in 0..steps {
            if cancel.is_cancelled() {
                return Err(WebDriverError::Cancelled);
            }
            self.dispatch_mouse_event(
                handle,
                json!({
                    "type": "mouseWheel",
                    "x": params.start.x,
                    "y": params.start.y,
                    "deltaX": step_dx,
                    "deltaY": step_dy,
                }),
                cancel,
            )
            .await?;
            if idx + 1 < steps && !per_step_delay.is_zero() {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(WebDriverError::Cancelled),
                    _ = tokio::time::sleep(per_step_delay) => {}
                }
            }
        }

        let _ = self
            .eval_by_value(
                handle,
                SINGLE_RAF_EXPRESSION,
                true,
                self.config.request_timeout,
                cancel,
            )
            .await?;
        Ok(())
    }

    async fn input_text_with_handle(
        &self,
        handle: &ActiveHandle,
        text: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.mark_context_dirty();
        let _ = handle
            .cdp
            .call_session(
                &handle.ids.session_id,
                "Input.insertText",
                json!({ "text": text }),
                self.config.request_timeout,
                cancel,
            )
            .await?;
        Ok(())
    }

    async fn erase_text_with_handle(
        &self,
        handle: &ActiveHandle,
        count: u32,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if count > MAX_KEY_REPEAT_COUNT {
            return Err(WebDriverError::InvalidArgument(format!(
                "eraseText count {count} exceeds maximum {MAX_KEY_REPEAT_COUNT}"
            )));
        }
        self.mark_context_dirty();
        let spec = backspace_key_spec();
        self.dispatch_key_spec(handle, &spec, count, cancel).await
    }

    async fn press_key_with_handle(
        &self,
        handle: &ActiveHandle,
        key: &KeyPress,
        cancel: &CancellationToken,
    ) -> Result<()> {
        match resolve_key_press(key)? {
            ResolvedKeyAction::BrowserBack { count } => {
                for _ in 0..count {
                    self.back_with_handle(handle, cancel).await?;
                }
                Ok(())
            }
            ResolvedKeyAction::Dispatch { spec, count } => {
                self.mark_context_dirty();
                self.dispatch_key_spec(handle, &spec, count, cancel).await
            }
        }
    }

    async fn back_with_handle(
        &self,
        handle: &ActiveHandle,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let hist = handle
            .cdp
            .call_session(
                &handle.ids.session_id,
                "Page.getNavigationHistory",
                json!({}),
                self.config.request_timeout,
                cancel,
            )
            .await?;

        let current_idx = hist
            .get("currentIndex")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let Some(entries) = hist.get("entries").and_then(Value::as_array) else {
            return Ok(());
        };
        if current_idx == 0 {
            return Ok(());
        }

        // Walk backwards from `currentIndex - 1`, skipping `about:blank` or empty URLs.
        let target_entry_id = (0..current_idx).rev().find_map(|idx| {
            let entry = entries.get(idx)?;
            let url = entry.get("url").and_then(Value::as_str).unwrap_or("");
            if url.is_empty() || url.eq_ignore_ascii_case("about:blank") {
                return None;
            }
            entry.get("id").and_then(Value::as_i64)
        });

        let Some(entry_id) = target_entry_id else {
            return Ok(());
        };

        self.mark_context_dirty();
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + self.config.navigation_timeout;
        let nav_before = handle.cdp.nav_seq();

        let _ = handle
            .cdp
            .call_session(
                &handle.ids.session_id,
                "Page.navigateToHistoryEntry",
                json!({ "entryId": entry_id }),
                self.config.navigation_timeout,
                cancel,
            )
            .await?;

        // Wait strictly for main-frame history traversal commit (`Page.frameNavigated` on main
        // frame or main-frame `Page.navigatedWithinDocument`) BEFORE evaluating
        // `document.readyState` so iframe load events never let us settle on the outgoing page!
        self.wait_for_history_commit(handle, nav_before, deadline, started, cancel)
            .await?;

        self.wait_ready_state_network_and_raf(handle, deadline, started, cancel)
            .await
    }
}

fn web_capabilities(disable_animations: bool) -> DriverCapabilities {
    let mut caps = DriverCapabilities::with([
        DriverCapability::AccessibilityTree,
        DriverCapability::SystemBack,
        DriverCapability::ClearState,
        DriverCapability::CompoundAct,
        DriverCapability::HierarchyHashShortCircuit,
        DriverCapability::AppCrashReports,
    ]);
    if disable_animations {
        caps.insert(DriverCapability::DisableAnimations);
    }
    caps
}

fn validate_finite_point(point: UiPoint, label: &str) -> Result<()> {
    if !point.x.is_finite() || !point.y.is_finite() {
        return Err(WebDriverError::InvalidArgument(format!(
            "{label} point coordinates must be finite (got ({}, {}))",
            point.x, point.y
        )));
    }
    Ok(())
}

fn clear_keychain_unsupported() -> DriverError {
    DriverError::Unsupported(UnsupportedCapability {
        platform: "web".to_string(),
        capability: DriverCapability::ClearKeychain,
        reason: "web driver does not have a native OS keychain".to_string(),
    })
}

impl PlatformDriver for WebDriver {
    fn platform(&self) -> &'static str {
        "web"
    }

    fn capabilities(&self) -> &DriverCapabilities {
        &self.capabilities
    }

    fn ensure_session<'a>(&'a self, cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let _ = self.ensure_or_recover_session(cancel).await?;
            if self.context_dirty.load(Ordering::SeqCst) {
                let _lifecycle = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    guard = self.lifecycle.lock() => guard,
                };
                if cancel.is_cancelled() {
                    return Err(DriverError::Cancelled);
                }
                if self.context_dirty.load(Ordering::SeqCst)
                    && let Some(current) = self.current_handle()
                    && !current.cdp.is_closed()
                    && !current.cdp.is_detached()
                {
                    let _ = self
                        .reset_browser_context_locked(&current, cancel)
                        .await
                        .map_err(|err| map_web_err("ensure_session.rotate_context", err))?;
                } else if self.current_handle().is_none()
                    || self.session_broken.load(Ordering::SeqCst)
                {
                    drop(_lifecycle);
                    let _ = self.ensure_or_recover_session(cancel).await?;
                }
            }
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
                return Err(clear_keychain_unsupported());
            }
            if !opts.arguments.is_empty() {
                return Err(DriverError::InvalidArgument(
                    "web `launchApp` does not support `arguments`".to_string(),
                ));
            }
            if opts.permissions.as_ref().is_some_and(|p| !p.is_empty()) {
                return Err(DriverError::InvalidArgument(
                    "web `launchApp` does not support `permissions`".to_string(),
                ));
            }
            // Note: `opts.env` is populated by `FlowExecutor` with flow environment variables and
            // MUST NOT be rejected.
            let url =
                validate_loopback_http_url(app_id).map_err(|err| map_web_err("launch_app", err))?;

            let mut handle = self.require_active_handle(cancel).await?;

            if opts.clear_state || opts.stop_app {
                let _lifecycle = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    guard = self.lifecycle.lock() => guard,
                };
                if opts.clear_state {
                    handle = self
                        .reset_browser_context_locked(&handle, cancel)
                        .await
                        .map_err(|err| {
                            self.map_active_err(&handle, "launch_app.clear_state", err)
                        })?;
                } else if opts.stop_app {
                    handle = self
                        .replace_target_same_context_locked(&handle, cancel)
                        .await
                        .map_err(|err| self.map_active_err(&handle, "launch_app.stop_app", err))?;
                }
            }

            self.navigate_and_settle(&handle, &url, cancel)
                .await
                .map_err(|err| self.map_active_err(&handle, "launch_app", err))
        })
    }

    fn stop_app<'a>(
        &'a self,
        _app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let handle = self.require_active_handle(cancel).await?;
            let _lifecycle = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                guard = self.lifecycle.lock() => guard,
            };
            let _ = self
                .replace_target_same_context_locked(&handle, cancel)
                .await
                .map_err(|err| self.map_active_err(&handle, "stop_app", err))?;
            Ok(())
        })
    }

    fn kill_app<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        self.stop_app(app_id, cancel)
    }

    fn clear_state<'a>(
        &'a self,
        _app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let handle = self.require_active_handle(cancel).await?;
            let _lifecycle = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                guard = self.lifecycle.lock() => guard,
            };
            let _ = self
                .reset_browser_context_locked(&handle, cancel)
                .await
                .map_err(|err| self.map_active_err(&handle, "clear_state", err))?;
            Ok(())
        })
    }

    fn clear_keychain<'a>(&'a self, cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            Err(clear_keychain_unsupported())
        })
    }

    fn open_link<'a>(
        &'a self,
        _app_id: Option<&'a str>,
        url: &'a str,
        _opts: &'a OpenLinkOptions,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let validated =
                validate_loopback_http_url(url).map_err(|err| map_web_err("open_link", err))?;
            let handle = self.require_active_handle(cancel).await?;
            self.navigate_and_settle(&handle, &validated, cancel)
                .await
                .map_err(|err| self.map_active_err(&handle, "open_link", err))
        })
    }

    fn add_media<'a>(
        &'a self,
        _paths: &'a [PathBuf],
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            Err(DriverError::InvalidArgument(
                "web driver does not support `addMedia`".to_string(),
            ))
        })
    }

    fn hierarchy<'a>(
        &'a self,
        app_id: &'a str,
        _include_system_ui: bool,
        known_hash: Option<&'a str>,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, UiHierarchy> {
        Box::pin(async move {
            let handle = self.require_active_handle(cancel).await?;
            self.fetch_hierarchy_with_handle(&handle, app_id, known_hash, cancel)
                .await
                .map_err(|err| self.map_active_err(&handle, "hierarchy", err))
        })
    }

    fn tap_point<'a>(
        &'a self,
        _app_id: &'a str,
        point: UiPoint,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let handle = self.require_active_handle(cancel).await?;
            self.tap_with_handle(&handle, point, cancel)
                .await
                .map_err(|err| self.map_active_err(&handle, "tap_point", err))
        })
    }

    fn long_press_point<'a>(
        &'a self,
        _app_id: &'a str,
        point: UiPoint,
        duration_ms: Option<u64>,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let handle = self.require_active_handle(cancel).await?;
            self.long_press_with_handle(&handle, point, duration_ms, cancel)
                .await
                .map_err(|err| self.map_active_err(&handle, "long_press_point", err))
        })
    }

    fn double_tap_point<'a>(
        &'a self,
        _app_id: &'a str,
        point: UiPoint,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let handle = self.require_active_handle(cancel).await?;
            self.double_tap_with_handle(&handle, point, cancel)
                .await
                .map_err(|err| self.map_active_err(&handle, "double_tap_point", err))
        })
    }

    fn swipe<'a>(
        &'a self,
        params: &'a SwipeParams,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let handle = self.require_active_handle(cancel).await?;
            self.swipe_with_handle(&handle, params, cancel)
                .await
                .map_err(|err| self.map_active_err(&handle, "swipe", err))
        })
    }

    fn input_text<'a>(
        &'a self,
        _app_id: &'a str,
        text: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let handle = self.require_active_handle(cancel).await?;
            self.input_text_with_handle(&handle, text, cancel)
                .await
                .map_err(|err| self.map_active_err(&handle, "input_text", err))
        })
    }

    fn erase_text<'a>(
        &'a self,
        _app_id: &'a str,
        count: u32,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let handle = self.require_active_handle(cancel).await?;
            self.erase_text_with_handle(&handle, count, cancel)
                .await
                .map_err(|err| self.map_active_err(&handle, "erase_text", err))
        })
    }

    fn press_key<'a>(
        &'a self,
        _app_id: &'a str,
        key: &'a KeyPress,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let handle = self.require_active_handle(cancel).await?;
            self.press_key_with_handle(&handle, key, cancel)
                .await
                .map_err(|err| self.map_active_err(&handle, "press_key", err))
        })
    }

    fn hide_keyboard<'a>(
        &'a self,
        _app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            Ok(())
        })
    }

    fn back<'a>(&'a self, _app_id: &'a str, cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let handle = self.require_active_handle(cancel).await?;
            self.back_with_handle(&handle, cancel)
                .await
                .map_err(|err| self.map_active_err(&handle, "back", err))
        })
    }

    fn take_screenshot<'a>(
        &'a self,
        _app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let handle = self.require_active_handle(cancel).await?;
            let resp = handle
                .cdp
                .call_session(
                    &handle.ids.session_id,
                    "Page.captureScreenshot",
                    json!({ "format": "png" }),
                    self.config.request_timeout,
                    cancel,
                )
                .await
                .map_err(|err| self.map_active_err(&handle, "take_screenshot", err))?;
            let data = resp.get("data").and_then(Value::as_str).ok_or_else(|| {
                map_web_err(
                    "take_screenshot",
                    WebDriverError::InvalidScreenshot(
                        "missing base64 `data` field in Page.captureScreenshot response"
                            .to_string(),
                    ),
                )
            })?;
            decode_base64_png(data).map_err(|err| map_web_err("take_screenshot", err))
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
            let handle = self.require_active_handle(cancel).await?;

            let action_res = match &req.action {
                ActAction::Tap { x, y } => {
                    self.tap_with_handle(&handle, UiPoint::new(*x, *y), cancel)
                        .await
                }
                ActAction::LongPress { x, y, duration_ms } => {
                    self.long_press_with_handle(&handle, UiPoint::new(*x, *y), *duration_ms, cancel)
                        .await
                }
                ActAction::DoubleTap { x, y } => {
                    self.double_tap_with_handle(&handle, UiPoint::new(*x, *y), cancel)
                        .await
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
                    self.swipe_with_handle(&handle, &params, cancel).await
                }
                ActAction::InputText { text } => {
                    self.input_text_with_handle(&handle, text, cancel).await
                }
                ActAction::EraseText { count } => {
                    self.erase_text_with_handle(&handle, *count, cancel).await
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
                    self.press_key_with_handle(&handle, &kp, cancel).await
                }
                ActAction::HideKeyboard => Ok(()),
                ActAction::Back => self.back_with_handle(&handle, cancel).await,
            };
            action_res.map_err(|err| self.map_active_err(&handle, "act_and_observe", err))?;

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
                    .fetch_hierarchy_with_handle(
                        &handle,
                        &req.app_id,
                        req.known_hash.as_deref(),
                        cancel,
                    )
                    .await
                    .map_err(|err| self.map_active_err(&handle, "act_and_observe.hierarchy", err))
                {
                    Ok(h) => (Some(h), None),
                    Err(DriverError::Cancelled) => return Err(DriverError::Cancelled),
                    Err(err @ DriverError::AppCrashed(_))
                    | Err(err @ DriverError::SessionClosed(_)) => return Err(err),
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
        })
    }

    fn check_app_crash<'a>(
        &'a self,
        _app_id: &'a str,
        since_epoch_ms: u64,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Option<DriverCrashSignal>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let _ = self.poll_unexpected_browser_exit().await;

            let recorded = self
                .crashes
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();

            Ok(recorded
                .filter(|c| c.epoch_ms >= since_epoch_ms)
                .map(|c| DriverCrashSignal {
                    source: CrashSignalSource::CrashReport,
                    report_file: None,
                    process: Some(c.process),
                    detail: Some(c.detail),
                }))
        })
    }

    fn shutdown<'a>(&'a self) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let _lifecycle = self.lifecycle.lock().await;
            self.teardown_locked().await;
            Ok(())
        })
    }
}
