//! Comprehensive deterministic integration tests for `deja daemon` (`DEJA-78`).
//!
//! Covers CLI subprocess lifecycle, strict `0o700`/`0o600` & symlink fail-closed security,
//! stale socket cleanup, warm pool reuse, `session_key` replacement, warm-hit context validation,
//! deterministic same-device lease serialization & cross-device concurrency, abrupt disconnect
//! eviction, idle TTL auto-shutdown, graceful `stop` & `SIGTERM` teardown, `--no-daemon` /
//! `DEJA_NO_DAEMON=1` bypass, cancel-safe `CancelRpc` framing, client-side redactor secret
//! isolation, and full lossless `PlatformDriver` RPC round-tripping.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs::{self, Permissions};
use std::os::unix::fs::{FileTypeExt, PermissionsExt, symlink};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deja_cli::commands::daemon::run_stop_with_timeouts;
use deja_cli::daemon::client::{
    DaemonClient, DaemonDriverFactory, ensure_background_daemon_started_with_exe,
};
use deja_cli::daemon::paths::{
    DaemonMetadata, DaemonPaths, acquire_daemon_lock, connect_verified, current_uid,
    probe_daemon_lock_held, read_daemon_metadata, write_daemon_metadata,
};
use deja_cli::daemon::server::DaemonServer;
use deja_cli::driver_factory::{
    DoctorCheck, DoctorProbeRequest, DoctorProbes, DriverContext, DriverFactory,
    DriverFactoryError, FactoryFuture, RealDriverFactory,
};
use deja_cli::output::OutputSink;
use deja_cli::{CliDeps, run_cli_from_args};
use deja_core::{CrashSignalSource, create_redactor, fingerprint_file, provider_key_redactor};
use deja_flow::{
    ActAction, ActOutcome, ActPayload, DriverCapabilities, DriverCapability, DriverCrashSignal,
    DriverError, DriverFuture, KeyPress, LaunchOptions, OpenLinkOptions, PlatformDriver, Selector,
    SwipeParams, UiHierarchy, UiNode, UiPoint, UiRect, UiSize, VisibleContext, WaitSelectorOutcome,
    WaitSelectorRequest, wait_local,
};
use serde_json::{Value, json};
use tokio::sync::{Barrier, Notify};
use tokio_util::sync::CancellationToken;

static DIR_SEQ: AtomicU64 = AtomicU64::new(1);

/// Short temporary daemon directory under `/tmp` guaranteed strictly `< 104` bytes on macOS.
struct ShortDaemonDir {
    path: PathBuf,
}

impl ShortDaemonDir {
    fn new(tag: &str) -> Self {
        let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            & 0xffff_ffff;
        let path = PathBuf::from(format!(
            "/tmp/dj-{tag}-{}-{seq}-{nanos:x}",
            std::process::id()
        ));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ShortDaemonDir {
    fn drop(&mut self) {
        // Best-effort ensure no background daemon remains tied to this temp dir.
        let _ = deja_bin_with_daemon(&self.path)
            .args(["daemon", "stop"])
            .output();
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn deja_bin_with_daemon(daemon_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_deja"));
    cmd.env("DEJA_DAEMON_DIR", daemon_dir)
        .env_remove("DEJA_NO_DAEMON")
        .env_remove("DEJA_DEVICE")
        .env_remove("DEJA_MAESTRO")
        .stdin(Stdio::null());
    cmd
}

// ---------------------------------------------------------------------------
// Deterministic Mock Driver & Factory for Daemon Pool Tests
// ---------------------------------------------------------------------------

type WaitBarrierHook = Arc<dyn Fn() -> PinBoxUnitFuture + Send + Sync>;
type PinBoxUnitFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

#[derive(Clone)]
struct RecordedWaitCall {
    had_redactor: bool,
    had_provider_redactor: bool,
    return_hierarchy: bool,
}

#[derive(Clone)]
struct DaemonMockDriver {
    platform: &'static str,
    caps: DriverCapabilities,
    attach_count: Arc<AtomicU64>,
    shutdown_count: Arc<AtomicUsize>,
    hierarchy_calls: Arc<AtomicUsize>,
    recorded_ops: Arc<Mutex<Vec<String>>>,
    recorded_waits: Arc<Mutex<Vec<RecordedWaitCall>>>,
    hierarchy_nodes: Arc<Mutex<Vec<UiNode>>>,
    screenshot_bytes: Arc<Mutex<Vec<u8>>>,
    crash_signal: Arc<Mutex<Option<DriverCrashSignal>>>,
    /// Optional async hook invoked inside `hierarchy` (for deterministic barriers / slow cancels).
    hierarchy_hook: Option<WaitBarrierHook>,
    /// Set to `true` if `hierarchy` observed `cancel.cancelled()`.
    hierarchy_cancelled: Arc<AtomicBool>,
    /// Optional notify signalled right as `hierarchy` begins executing.
    hierarchy_entered: Option<Arc<Notify>>,
    /// If `true`, `hierarchy` blocks until `cancel.cancelled()`.
    block_hierarchy_until_cancel: Arc<AtomicBool>,
}

impl DaemonMockDriver {
    fn new(platform: &'static str) -> Self {
        Self {
            platform,
            caps: DriverCapabilities::with([
                DriverCapability::AccessibilityTree,
                DriverCapability::CompoundAct,
                DriverCapability::AppCrashReports,
                DriverCapability::ClearState,
                DriverCapability::SystemBack,
            ]),
            attach_count: Arc::new(AtomicU64::new(0)),
            shutdown_count: Arc::new(AtomicUsize::new(0)),
            hierarchy_calls: Arc::new(AtomicUsize::new(0)),
            recorded_ops: Arc::new(Mutex::new(Vec::new())),
            recorded_waits: Arc::new(Mutex::new(Vec::new())),
            hierarchy_nodes: Arc::new(Mutex::new(vec![sample_node("btn_ok", "Continue")])),
            screenshot_bytes: Arc::new(Mutex::new(vec![
                0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a,
            ])),
            crash_signal: Arc::new(Mutex::new(None)),
            hierarchy_hook: None,
            hierarchy_cancelled: Arc::new(AtomicBool::new(false)),
            hierarchy_entered: None,
            block_hierarchy_until_cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn record(&self, op: impl Into<String>) {
        self.recorded_ops.lock().unwrap().push(op.into());
    }
}

fn sample_node(id: &str, label: &str) -> UiNode {
    UiNode {
        index: 1,
        parent_index: None,
        depth: 0,
        source: "app".to_string(),
        element_type: "Button".to_string(),
        identifier: id.to_string(),
        label: label.to_string(),
        title: label.to_string(),
        enabled: true,
        hittable: true,
        is_on_screen: true,
        frame: UiRect::new(10.0, 20.0, 120.0, 44.0),
        ..UiNode::default()
    }
}

fn build_hierarchy(app_id: &str, nodes: Vec<UiNode>) -> UiHierarchy {
    let mut h = UiHierarchy {
        app_id: app_id.to_string(),
        state: "runningForeground".to_string(),
        viewport: Some(UiSize::new(390.0, 844.0)),
        truncated: false,
        hierarchy_hash: String::new(),
        unchanged: false,
        nodes,
    };
    h.hierarchy_hash = h.hierarchy_hash_hex();
    h
}

impl PlatformDriver for DaemonMockDriver {
    fn platform(&self) -> &'static str {
        self.platform
    }

    fn capabilities(&self) -> &DriverCapabilities {
        &self.caps
    }

    fn session_attach_count(&self) -> Option<u64> {
        Some(self.attach_count.load(Ordering::SeqCst))
    }

    fn ensure_session<'a>(&'a self, cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            // Simulate warm driver reusing an already-attached session (`attach_count` increments
            // only on first attach, matching `IosDriver` and `WebDriver`).
            let _ = self
                .attach_count
                .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
            self.record("ensure_session");
            Ok(())
        })
    }

    fn launch_app<'a>(
        &'a self,
        app_id: &'a str,
        _opts: &'a LaunchOptions,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            self.record(format!("launch_app:{app_id}"));
            Ok(())
        })
    }

    fn stop_app<'a>(
        &'a self,
        app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            self.record(format!("stop_app:{app_id}"));
            Ok(())
        })
    }

    fn kill_app<'a>(
        &'a self,
        app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            self.record(format!("kill_app:{app_id}"));
            Ok(())
        })
    }

    fn clear_state<'a>(
        &'a self,
        app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            self.record(format!("clear_state:{app_id}"));
            Ok(())
        })
    }

    fn clear_keychain<'a>(&'a self, _cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.record("clear_keychain");
            Ok(())
        })
    }

    fn open_link<'a>(
        &'a self,
        _app_id: Option<&'a str>,
        url: &'a str,
        _opts: &'a OpenLinkOptions,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let url = url.to_string();
        Box::pin(async move {
            self.record(format!("open_link:{url}"));
            Ok(())
        })
    }

    fn add_media<'a>(
        &'a self,
        paths: &'a [PathBuf],
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let count = paths.len();
        Box::pin(async move {
            self.record(format!("add_media:{count}"));
            Ok(())
        })
    }

    fn hierarchy<'a>(
        &'a self,
        app_id: &'a str,
        _include_system_ui: bool,
        _known_hash: Option<&'a str>,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, UiHierarchy> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            self.hierarchy_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(ref entered) = self.hierarchy_entered {
                entered.notify_waiters();
            }
            if let Some(ref hook) = self.hierarchy_hook {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        self.hierarchy_cancelled.store(true, Ordering::SeqCst);
                        return Err(DriverError::Cancelled);
                    }
                    _ = hook() => {}
                }
            }
            if self.block_hierarchy_until_cancel.load(Ordering::SeqCst) {
                cancel.cancelled().await;
                self.hierarchy_cancelled.store(true, Ordering::SeqCst);
                return Err(DriverError::Cancelled);
            }
            if cancel.is_cancelled() {
                self.hierarchy_cancelled.store(true, Ordering::SeqCst);
                return Err(DriverError::Cancelled);
            }
            let nodes = self.hierarchy_nodes.lock().unwrap().clone();
            Ok(build_hierarchy(&app_id, nodes))
        })
    }

    fn tap_point<'a>(
        &'a self,
        app_id: &'a str,
        point: UiPoint,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            self.record(format!("tap:{app_id}:{:.0},{:.0}", point.x, point.y));
            Ok(())
        })
    }

    fn long_press_point<'a>(
        &'a self,
        app_id: &'a str,
        point: UiPoint,
        duration_ms: Option<u64>,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            self.record(format!(
                "long_press:{app_id}:{:.0},{:.0}:{duration_ms:?}",
                point.x, point.y
            ));
            Ok(())
        })
    }

    fn double_tap_point<'a>(
        &'a self,
        app_id: &'a str,
        point: UiPoint,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            self.record(format!("double_tap:{app_id}:{:.0},{:.0}", point.x, point.y));
            Ok(())
        })
    }

    fn swipe<'a>(
        &'a self,
        params: &'a SwipeParams,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let dur = params.duration_ms;
        Box::pin(async move {
            self.record(format!("swipe:{dur}"));
            Ok(())
        })
    }

    fn input_text<'a>(
        &'a self,
        _app_id: &'a str,
        text: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let text = text.to_string();
        Box::pin(async move {
            self.record(format!("input_text:{text}"));
            Ok(())
        })
    }

    fn erase_text<'a>(
        &'a self,
        _app_id: &'a str,
        count: u32,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.record(format!("erase_text:{count}"));
            Ok(())
        })
    }

    fn press_key<'a>(
        &'a self,
        _app_id: &'a str,
        key: &'a KeyPress,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let k = key.key.clone();
        Box::pin(async move {
            self.record(format!("press_key:{k}"));
            Ok(())
        })
    }

    fn hide_keyboard<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.record("hide_keyboard");
            Ok(())
        })
    }

    fn back<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.record("back");
            Ok(())
        })
    }

    fn take_screenshot<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Vec<u8>> {
        Box::pin(async move {
            self.record("take_screenshot");
            Ok(self.screenshot_bytes.lock().unwrap().clone())
        })
    }

    fn check_app_crash<'a>(
        &'a self,
        _app_id: &'a str,
        _since_epoch_ms: u64,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Option<DriverCrashSignal>> {
        Box::pin(async move {
            self.record("check_app_crash");
            Ok(self.crash_signal.lock().unwrap().clone())
        })
    }

    fn shutdown<'a>(&'a self) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.shutdown_count.fetch_add(1, Ordering::SeqCst);
            self.record("shutdown");
            Ok(())
        })
    }

    fn act_and_observe<'a>(
        &'a self,
        req: &'a ActPayload,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ActOutcome> {
        let app_id = req.app_id.clone();
        let want_h = req.return_hierarchy;
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            self.record("act_and_observe");
            let h = if want_h {
                Some(build_hierarchy(
                    &app_id,
                    self.hierarchy_nodes.lock().unwrap().clone(),
                ))
            } else {
                None
            };
            Ok(ActOutcome {
                action_ok: true,
                elapsed_ms: 3,
                hierarchy: h,
                hierarchy_error: None,
            })
        })
    }

    fn wait_for_selector<'a>(
        &'a self,
        req: &'a WaitSelectorRequest,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, WaitSelectorOutcome> {
        self.recorded_waits.lock().unwrap().push(RecordedWaitCall {
            had_redactor: req.redactor.is_some(),
            had_provider_redactor: req.provider_redactor.is_some(),
            return_hierarchy: req.return_hierarchy,
        });
        Box::pin(async move { wait_local(self, req, cancel).await })
    }
}

type ValidateWarmFn = Arc<dyn Fn(&DriverContext) -> Result<(), DriverFactoryError> + Send + Sync>;
type DriverBuilderFn = Arc<dyn Fn(&DriverContext, usize) -> DaemonMockDriver + Send + Sync>;

struct DaemonMockFactory {
    create_count: Arc<AtomicUsize>,
    created_drivers: Arc<Mutex<Vec<DaemonMockDriver>>>,
    received_contexts: Arc<Mutex<Vec<DriverContext>>>,
    builder: DriverBuilderFn,
    validate_warm: Option<ValidateWarmFn>,
}

impl DaemonMockFactory {
    fn new(builder: DriverBuilderFn) -> Arc<Self> {
        Arc::new(Self {
            create_count: Arc::new(AtomicUsize::new(0)),
            created_drivers: Arc::new(Mutex::new(Vec::new())),
            received_contexts: Arc::new(Mutex::new(Vec::new())),
            builder,
            validate_warm: None,
        })
    }

    fn with_validate_warm(builder: DriverBuilderFn, validate_warm: ValidateWarmFn) -> Arc<Self> {
        Arc::new(Self {
            create_count: Arc::new(AtomicUsize::new(0)),
            created_drivers: Arc::new(Mutex::new(Vec::new())),
            received_contexts: Arc::new(Mutex::new(Vec::new())),
            builder,
            validate_warm: Some(validate_warm),
        })
    }

    fn total_shutdowns(&self) -> usize {
        self.created_drivers
            .lock()
            .unwrap()
            .iter()
            .map(|d| d.shutdown_count.load(Ordering::SeqCst))
            .sum()
    }
}

impl DriverFactory for DaemonMockFactory {
    fn resolve_runtime<'a>(
        &'a self,
        ctx: &'a DriverContext,
        _cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Option<String>, DriverFactoryError>> {
        let rt = if ctx.platform == "web" {
            "chromium-135.0.0.0@1280x720".to_string()
        } else {
            "ios-27.2".to_string()
        };
        Box::pin(async move { Ok(Some(rt)) })
    }

    fn create<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Box<dyn PlatformDriver>, DriverFactoryError>> {
        let ctx_clone = ctx.clone();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverFactoryError::Cancelled);
            }
            if let Some(ref v) = self.validate_warm {
                v(&ctx_clone)?;
            }
            self.received_contexts
                .lock()
                .unwrap()
                .push(ctx_clone.clone());
            let idx = self.create_count.fetch_add(1, Ordering::SeqCst);
            let driver = (self.builder)(&ctx_clone, idx);
            self.created_drivers.lock().unwrap().push(driver.clone());
            Ok(Box::new(driver) as Box<dyn PlatformDriver>)
        })
    }

    fn install_app<'a>(
        &'a self,
        _ctx: &'a DriverContext,
        _cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<(), DriverFactoryError>> {
        Box::pin(async { Ok(()) })
    }

    fn validate_warm_context(&self, ctx: &DriverContext) -> Result<(), DriverFactoryError> {
        if let Some(ref v) = self.validate_warm {
            v(ctx)
        } else {
            Ok(())
        }
    }
}

struct NoopDoctorProbes;
impl DoctorProbes for NoopDoctorProbes {
    fn probe<'a>(&'a self, _req: &'a DoctorProbeRequest) -> FactoryFuture<'a, Vec<DoctorCheck>> {
        Box::pin(async { Vec::new() })
    }
}

fn sample_ctx(platform: &str, device: &str, app_id: &str) -> DriverContext {
    DriverContext {
        platform: platform.to_string(),
        device: device.to_string(),
        app_id: app_id.to_string(),
        build_path: PathBuf::from("/tmp/App.app"),
        expected_build_fingerprint: None,
        workspace_root: PathBuf::from("/tmp/ws"),
        disable_animations: true,
        crash_report_grace_ms: 0,
        crash_report_dir: None,
        deja_home: None,
        headless: Some(true),
        screen_size: Some(json!("1280x720")),
        timeout_ms: 30_000,
        env: BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
        staged_build: None,
        resolved_runtime: None,
    }
}

// ===========================================================================
// 1. CLI Subprocess Lifecycle, Permissions, Stale Cleanup, Fail-Closed Security
// ===========================================================================

#[test]
fn cli_daemon_start_status_stop_lifecycle_permissions_and_idempotency() {
    let dir = ShortDaemonDir::new("life");

    // Reject `--idle-timeout 0` with exit 2.
    let zero_idle = deja_bin_with_daemon(dir.path())
        .args(["daemon", "start", "--idle-timeout", "0", "--json"])
        .output()
        .expect("spawn daemon start --idle-timeout 0");
    assert_eq!(zero_idle.status.code(), Some(2));
    let zero_json: Value = serde_json::from_slice(&zero_idle.stdout).unwrap();
    assert_eq!(zero_json["error"]["code"], "INVALID_IDLE_TIMEOUT");

    // Status before start -> not running.
    let status0 = deja_bin_with_daemon(dir.path())
        .args(["daemon", "status", "--json"])
        .output()
        .unwrap();
    assert_eq!(status0.status.code(), Some(0));
    let status0_json: Value = serde_json::from_slice(&status0.stdout).unwrap();
    assert_eq!(status0_json["running"], false);

    // Stop before start -> idempotent stopped: false.
    let stop0 = deja_bin_with_daemon(dir.path())
        .args(["daemon", "stop", "--json"])
        .output()
        .unwrap();
    assert_eq!(stop0.status.code(), Some(0));
    let stop0_json: Value = serde_json::from_slice(&stop0.stdout).unwrap();
    assert_eq!(stop0_json["stopped"], false);

    // Start background daemon.
    let start1 = deja_bin_with_daemon(dir.path())
        .args(["daemon", "start", "--idle-timeout", "60", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        start1.status.code(),
        Some(0),
        "daemon start failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&start1.stdout),
        String::from_utf8_lossy(&start1.stderr)
    );
    let start1_json: Value = serde_json::from_slice(&start1.stdout).unwrap();
    assert_eq!(start1_json["started"], true);
    assert_eq!(start1_json["alreadyRunning"], false);
    let daemon_pid = start1_json["pid"].as_u64().unwrap();
    assert!(daemon_pid > 0);
    assert_eq!(start1_json["idleTimeoutSecs"], 60);

    // Verify strict `0o700` dir and `0o600` socket/metadata/lock permissions.
    let dir_meta = fs::symlink_metadata(dir.path()).unwrap();
    assert!(!dir_meta.file_type().is_symlink() && dir_meta.is_dir());
    assert_eq!(dir_meta.permissions().mode() & 0o777, 0o700);

    let sock_meta = fs::symlink_metadata(dir.path().join("deja.sock")).unwrap();
    assert!(!sock_meta.file_type().is_symlink() && sock_meta.file_type().is_socket());
    assert_eq!(sock_meta.permissions().mode() & 0o777, 0o600);

    let json_meta = fs::symlink_metadata(dir.path().join("daemon.json")).unwrap();
    assert!(!json_meta.file_type().is_symlink() && json_meta.is_file());
    assert_eq!(json_meta.permissions().mode() & 0o777, 0o600);

    let lock_meta = fs::symlink_metadata(dir.path().join("daemon.lock")).unwrap();
    assert!(!lock_meta.file_type().is_symlink() && lock_meta.is_file());
    assert_eq!(lock_meta.permissions().mode() & 0o777, 0o600);

    // Second `deja daemon start` is idempotent with `alreadyRunning: true`.
    let start2 = deja_bin_with_daemon(dir.path())
        .args(["daemon", "start", "--json"])
        .output()
        .unwrap();
    assert_eq!(start2.status.code(), Some(0));
    let start2_json: Value = serde_json::from_slice(&start2.stdout).unwrap();
    assert_eq!(start2_json["started"], false);
    assert_eq!(start2_json["alreadyRunning"], true);
    assert_eq!(start2_json["pid"], daemon_pid);

    // Status reports running with exact PID.
    let status1 = deja_bin_with_daemon(dir.path())
        .args(["daemon", "status", "--json"])
        .output()
        .unwrap();
    assert_eq!(status1.status.code(), Some(0));
    let status1_json: Value = serde_json::from_slice(&status1.stdout).unwrap();
    assert_eq!(status1_json["running"], true);
    assert_eq!(status1_json["pid"], daemon_pid);
    assert_eq!(status1_json["activeLeases"], 0);
    assert_eq!(status1_json["pooledSessions"], 0);

    // Stop gracefully removes `deja.sock` & `daemon.json` and unlocks `daemon.lock`.
    let stop1 = deja_bin_with_daemon(dir.path())
        .args(["daemon", "stop", "--json"])
        .output()
        .unwrap();
    assert_eq!(stop1.status.code(), Some(0));
    let stop1_json: Value = serde_json::from_slice(&stop1.stdout).unwrap();
    assert_eq!(stop1_json["stopped"], true);
    assert_eq!(stop1_json["wasRunning"], true);
    assert_eq!(stop1_json["pid"], daemon_pid);
    assert!(!dir.path().join("deja.sock").exists());
    assert!(!dir.path().join("daemon.json").exists());
    let paths = DaemonPaths::from_dir(dir.path().to_path_buf()).unwrap();
    assert!(!probe_daemon_lock_held(&paths).unwrap());

    // Final status reports not running.
    let status2 = deja_bin_with_daemon(dir.path())
        .args(["daemon", "status", "--json"])
        .output()
        .unwrap();
    assert_eq!(status2.status.code(), Some(0));
    let status2_json: Value = serde_json::from_slice(&status2.stdout).unwrap();
    assert_eq!(status2_json["running"], false);
}

#[test]
fn cli_daemon_status_cleans_stale_socket_and_metadata_when_lock_unlocked() {
    let dir = ShortDaemonDir::new("stale");
    fs::create_dir_all(dir.path()).unwrap();
    fs::set_permissions(dir.path(), Permissions::from_mode(0o700)).unwrap();

    // Create a real unbound stale Unix socket (0o600) + stale `daemon.json`.
    let sock_path = dir.path().join("deja.sock");
    let listener = StdUnixListener::bind(&sock_path).unwrap();
    fs::set_permissions(&sock_path, Permissions::from_mode(0o600)).unwrap();
    drop(listener);

    let meta = DaemonMetadata {
        version: "0.1.0".to_string(),
        pid: 999_999,
        uid: current_uid().unwrap(),
        socket_path: sock_path.to_string_lossy().into_owned(),
        started_at: "2026-01-01T00:00:00Z".to_string(),
        idle_timeout_secs: 300,
        proc_start_time: None,
    };
    fs::write(
        dir.path().join("daemon.json"),
        serde_json::to_vec(&meta).unwrap(),
    )
    .unwrap();

    let out = deja_bin_with_daemon(dir.path())
        .args(["daemon", "status", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["running"], false);
    let cleaned: Vec<String> = json["cleanedStaleFiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(cleaned.contains(&"deja.sock".to_string()));
    assert!(cleaned.contains(&"daemon.json".to_string()));
    assert!(!sock_path.exists());
    assert!(!dir.path().join("daemon.json").exists());
}

#[tokio::test]
async fn security_rejects_symlinks_and_world_readable_modes_fail_closed() {
    // 1. Symlink `daemon_dir`.
    let real_dir = ShortDaemonDir::new("sec-real");
    let link_dir = ShortDaemonDir::new("sec-link");
    fs::create_dir_all(real_dir.path()).unwrap();
    fs::set_permissions(real_dir.path(), Permissions::from_mode(0o700)).unwrap();
    symlink(real_dir.path(), link_dir.path()).unwrap();

    let out = deja_bin_with_daemon(link_dir.path())
        .args(["daemon", "status", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "symlink daemon dir must fail closed"
    );
    let err_json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(err_json["error"]["code"], "DAEMON_SECURITY_ERROR");

    // 2. Overly permissive `daemon_dir` (`0o755`).
    let wide_dir = ShortDaemonDir::new("sec-755");
    fs::create_dir_all(wide_dir.path()).unwrap();
    fs::set_permissions(wide_dir.path(), Permissions::from_mode(0o755)).unwrap();
    let out_755 = deja_bin_with_daemon(wide_dir.path())
        .args(["daemon", "start", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        out_755.status.code(),
        Some(1),
        "0o755 daemon dir must fail closed"
    );

    // 3. `0o644` Unix socket fails closed on client `connect_verified` and `deja daemon status`.
    let bad_sock_dir = ShortDaemonDir::new("sec-644");
    fs::create_dir_all(bad_sock_dir.path()).unwrap();
    fs::set_permissions(bad_sock_dir.path(), Permissions::from_mode(0o700)).unwrap();
    let sock_path = bad_sock_dir.path().join("deja.sock");
    let _listener = StdUnixListener::bind(&sock_path).unwrap();
    fs::set_permissions(&sock_path, Permissions::from_mode(0o644)).unwrap();

    let paths = DaemonPaths::from_dir(bad_sock_dir.path().to_path_buf()).unwrap();
    let err = connect_verified(&paths)
        .await
        .expect_err("0o644 socket must fail closed");
    assert!(err.is_security_violation(), "unexpected error: {err}");

    let status_644 = deja_bin_with_daemon(bad_sock_dir.path())
        .args(["daemon", "status", "--json"])
        .output()
        .unwrap();
    assert_eq!(status_644.status.code(), Some(1));
    assert!(
        sock_path.exists(),
        "insecure socket must NOT be silently deleted by status"
    );

    // 4. Symlink `deja.sock` fails closed.
    let sym_sock_dir = ShortDaemonDir::new("sec-symsock");
    fs::create_dir_all(sym_sock_dir.path()).unwrap();
    fs::set_permissions(sym_sock_dir.path(), Permissions::from_mode(0o700)).unwrap();
    symlink(&sock_path, sym_sock_dir.path().join("deja.sock")).unwrap();
    let sym_paths = DaemonPaths::from_dir(sym_sock_dir.path().to_path_buf()).unwrap();
    let sym_err = connect_verified(&sym_paths)
        .await
        .expect_err("symlink socket must fail closed");
    assert!(
        sym_err.is_security_violation(),
        "unexpected error: {sym_err}"
    );
}

// ===========================================================================
// 2. Warm Pool Reuse, `session_key` Mismatch Replacement & Warm Validation
// ===========================================================================

#[tokio::test]
async fn warm_pool_reuses_driver_replaces_on_key_mismatch_and_validates_warm_context() {
    let dir = ShortDaemonDir::new("warm");
    let paths = DaemonPaths::from_dir(dir.path().to_path_buf()).unwrap();
    let factory = DaemonMockFactory::with_validate_warm(
        Arc::new(|_ctx, _idx| DaemonMockDriver::new("ios")),
        Arc::new(|ctx| {
            if ctx.app_id == "dev.deja.bad" {
                Err(DriverFactoryError::UnsupportedOrConfig(
                    "invalid appId on warm validation".to_string(),
                ))
            } else {
                Ok(())
            }
        }),
    );

    let server = DaemonServer::bind(
        paths.clone(),
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        Duration::from_secs(60),
    )
    .unwrap();
    let server_cancel = CancellationToken::new();
    let srv_handle = tokio::spawn(server.run(server_cancel.clone()));

    let daemon_factory = DaemonDriverFactory::with_daemon_dir(
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        dir.path().to_path_buf(),
        false,
    );
    let cancel = CancellationToken::new();
    let ctx1 = sample_ctx(
        "ios",
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        "dev.deja.app1",
    );

    // Run #1: cold create + attach.
    let d1 = daemon_factory.create(&ctx1, &cancel).await.unwrap();
    d1.ensure_session(&cancel).await.unwrap();
    d1.shutdown().await.unwrap();
    assert_eq!(factory.create_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        factory.total_shutdowns(),
        0,
        "warm driver must stay alive after LeaseRelease"
    );

    // Run #2: same simulator UDID (uppercase!) + different appId -> WARM HIT (< 50 ms)!
    let ctx2 = sample_ctx(
        "ios",
        "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE",
        "dev.deja.app2",
    );
    let t0 = Instant::now();
    let d2 = daemon_factory.create(&ctx2, &cancel).await.unwrap();
    d2.ensure_session(&cancel).await.unwrap();
    let warm_attach_ms = t0.elapsed().as_millis();
    d2.shutdown().await.unwrap();
    assert!(
        warm_attach_ms < 50,
        "expected warm daemon attach < 50ms, got {warm_attach_ms}ms"
    );
    assert_eq!(factory.create_count.load(Ordering::SeqCst), 1);

    // Verify `Status` RPC reports `attachCount == 1`, `leaseCount == 2`, `leased == false`.
    let client = DaemonClient::connect(&paths).await.unwrap();
    let status = client.status(&cancel).await.unwrap();
    assert_eq!(status.pooled_sessions, 1);
    assert_eq!(status.active_leases, 0);
    assert_eq!(status.sessions[0].attach_count, Some(1));
    assert_eq!(status.sessions[0].lease_count, 2);
    assert!(!status.sessions[0].leased);

    // Warm-hit validation failure fails closed exit 2 without destroying the warm driver!
    let bad_ctx = sample_ctx(
        "ios",
        "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE",
        "dev.deja.bad",
    );
    let bad_err = daemon_factory
        .create(&bad_ctx, &cancel)
        .await
        .err()
        .expect("bad appId must fail closed on warm hit");
    assert_eq!(bad_err.exit_code(), 2);
    assert_eq!(
        factory.total_shutdowns(),
        0,
        "bad request must not evict warm driver"
    );

    // Run #3: `session_key` mismatch (`disable_animations: false`) cleanly shuts down old driver
    // and creates a replacement.
    let mut ctx3 = ctx2.clone();
    ctx3.disable_animations = false;
    let d3 = daemon_factory.create(&ctx3, &cancel).await.unwrap();
    d3.ensure_session(&cancel).await.unwrap();
    d3.shutdown().await.unwrap();
    assert_eq!(factory.create_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        factory.created_drivers.lock().unwrap()[0]
            .shutdown_count
            .load(Ordering::SeqCst),
        1,
        "old incompatible driver must be shut down cleanly before replacement"
    );
    assert_eq!(
        factory.created_drivers.lock().unwrap()[1]
            .shutdown_count
            .load(Ordering::SeqCst),
        0
    );

    // RealDriverFactory::validate_warm_context also rejects invalid iOS `.app` bundle with exit 2.
    let real = RealDriverFactory;
    let err = real
        .validate_warm_context(&ctx1)
        .expect_err("missing .app bundle must fail closed");
    assert_eq!(err.exit_code(), 2);

    server_cancel.cancel();
    srv_handle.await.unwrap();
    assert_eq!(factory.total_shutdowns(), 2);
}

// ===========================================================================
// 3. Same-Device Serialization (Deterministic Notify) & Cross-Device Concurrency
// ===========================================================================

#[tokio::test]
async fn same_device_serializes_while_different_devices_and_web_viewports_run_concurrently() {
    let dir = ShortDaemonDir::new("conc");
    let paths = DaemonPaths::from_dir(dir.path().to_path_buf()).unwrap();
    let factory = DaemonMockFactory::new(Arc::new(|ctx, _| {
        let p = if ctx.platform == "web" { "web" } else { "ios" };
        DaemonMockDriver::new(p)
    }));
    let server = DaemonServer::bind(
        paths.clone(),
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        Duration::from_secs(60),
    )
    .unwrap();
    let server_cancel = CancellationToken::new();
    let srv_handle = tokio::spawn(server.run(server_cancel.clone()));

    let daemon_factory = Arc::new(DaemonDriverFactory::with_daemon_dir(
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        dir.path().to_path_buf(),
        false,
    ));
    let cancel = CancellationToken::new();

    // Part A: Same iOS device (lowercase vs uppercase UDID) serializes strictly.
    let ctx_a = sample_ctx(
        "ios",
        "11111111-2222-3333-4444-555555555555",
        "dev.deja.app",
    );
    let ctx_b = sample_ctx(
        "ios",
        "11111111-2222-3333-4444-555555555555"
            .to_uppercase()
            .as_str(),
        "dev.deja.app",
    );

    let lease_a = daemon_factory.create(&ctx_a, &cancel).await.unwrap();
    lease_a.ensure_session(&cancel).await.unwrap();

    let b_acquired = Arc::new(AtomicBool::new(false));
    let b_acquired_c = Arc::clone(&b_acquired);
    let b_started = Arc::new(Notify::new());
    let b_started_c = Arc::clone(&b_started);
    let df_b = Arc::clone(&daemon_factory);
    let cancel_b = cancel.clone();
    let task_b = tokio::spawn(async move {
        b_started_c.notify_one();
        let lease_b = df_b.create(&ctx_b, &cancel_b).await.unwrap();
        b_acquired_c.store(true, Ordering::SeqCst);
        lease_b.ensure_session(&cancel_b).await.unwrap();
        lease_b.shutdown().await.unwrap();
    });

    b_started.notified().await;
    // Wait until Client B's AcquireLease reaches the daemon (`active_leases` becomes 2 while only 1 leased).
    let status_client = DaemonClient::connect(&paths).await.unwrap();
    let wait_queued_start = Instant::now();
    loop {
        let st = status_client.status(&cancel).await.unwrap();
        if st.active_leases >= 2 {
            break;
        }
        assert!(wait_queued_start.elapsed() < Duration::from_secs(2));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        !b_acquired.load(Ordering::SeqCst),
        "Client B must remain blocked while Client A holds the same-device lease"
    );

    // Release Client A -> Client B acquires the SAME warm driver immediately!
    lease_a.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), task_b)
        .await
        .expect("Client B did not finish after Client A released")
        .unwrap();
    assert!(b_acquired.load(Ordering::SeqCst));
    assert_eq!(factory.create_count.load(Ordering::SeqCst), 1);

    server_cancel.cancel();
    srv_handle.await.unwrap();

    // Part B: Different devices AND different Web viewports run concurrently (2-party Barrier!).
    let dir2 = ShortDaemonDir::new("conc2");
    let paths2 = DaemonPaths::from_dir(dir2.path().to_path_buf()).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let barrier_c = Arc::clone(&barrier);
    let factory2 = DaemonMockFactory::new(Arc::new(move |ctx, _| {
        let mut d = DaemonMockDriver::new(if ctx.platform == "web" { "web" } else { "ios" });
        let b = Arc::clone(&barrier_c);
        d.hierarchy_hook = Some(Arc::new(move || {
            let b_inner = Arc::clone(&b);
            Box::pin(async move {
                let _ = b_inner.wait().await;
            })
        }));
        d
    }));
    let server2 = DaemonServer::bind(
        paths2,
        Arc::clone(&factory2) as Arc<dyn DriverFactory>,
        Duration::from_secs(60),
    )
    .unwrap();
    let srv2_cancel = CancellationToken::new();
    let srv2_handle = tokio::spawn(server2.run(srv2_cancel.clone()));
    let df2 = Arc::new(DaemonDriverFactory::with_daemon_dir(
        Arc::clone(&factory2) as Arc<dyn DriverFactory>,
        dir2.path().to_path_buf(),
        false,
    ));

    // Two distinct Web viewports (`1280x720` vs `1440x900`) MUST pass through the 2-party barrier concurrently!
    let mut web1 = sample_ctx("web", "chromium", "http://127.0.0.1:4174");
    web1.screen_size = Some(json!("1280x720"));
    let mut web2 = sample_ctx("web", "chromium", "http://127.0.0.1:4174");
    web2.screen_size = Some(json!("1440x900"));

    let (r1, r2) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(
            async {
                let d = df2.create(&web1, &cancel).await.unwrap();
                let _ = d
                    .hierarchy(&web1.app_id, false, None, &cancel)
                    .await
                    .unwrap();
                d.shutdown().await.unwrap();
            },
            async {
                let d = df2.create(&web2, &cancel).await.unwrap();
                let _ = d
                    .hierarchy(&web2.app_id, false, None, &cancel)
                    .await
                    .unwrap();
                d.shutdown().await.unwrap();
            }
        )
    })
    .await
    .expect("distinct Web viewports must execute concurrently without deadlocking 2-party barrier");
    let _ = (r1, r2);
    assert_eq!(factory2.create_count.load(Ordering::SeqCst), 2);

    srv2_cancel.cancel();
    srv2_handle.await.unwrap();
}

// ===========================================================================
// 4. Abrupt Client Disconnect Mid-RPC Evicts Dirty Driver & Unblocks Queue
// ===========================================================================

#[tokio::test]
async fn abrupt_disconnect_mid_rpc_cancels_evicts_dirty_driver_and_unblocks_next_client() {
    let dir = ShortDaemonDir::new("abrupt");
    let paths = DaemonPaths::from_dir(dir.path().to_path_buf()).unwrap();
    let rpc_entered = Arc::new(Notify::new());
    let rpc_entered_c = Arc::clone(&rpc_entered);

    let factory = DaemonMockFactory::new(Arc::new(move |_ctx, idx| {
        let mut d = DaemonMockDriver::new("ios");
        if idx == 0 {
            d.hierarchy_entered = Some(Arc::clone(&rpc_entered_c));
            d.block_hierarchy_until_cancel.store(true, Ordering::SeqCst);
        }
        d
    }));
    let server = DaemonServer::bind(
        paths.clone(),
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        Duration::from_secs(60),
    )
    .unwrap();
    let srv_cancel = CancellationToken::new();
    let srv_handle = tokio::spawn(server.run(srv_cancel.clone()));

    let df = Arc::new(DaemonDriverFactory::with_daemon_dir(
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        dir.path().to_path_buf(),
        false,
    ));
    let cancel = CancellationToken::new();
    let ctx = sample_ctx(
        "ios",
        "aaaaaaaa-1111-2222-3333-444444444444",
        "dev.deja.app",
    );

    // Client A acquires lease and starts a blocked RPC, then drops its connection abruptly!
    let client_a = DaemonClient::connect(&paths).await.unwrap();
    let _ = client_a.acquire_lease(&ctx, &cancel).await.unwrap();
    let client_a = Arc::new(client_a);
    let client_a_rpc = Arc::clone(&client_a);
    let app_id = ctx.app_id.clone();
    let rpc_task = tokio::spawn(async move {
        let _ = client_a_rpc
            .call_rpc(
                deja_cli::daemon::protocol::DriverOp::Hierarchy {
                    app_id,
                    include_system_ui: false,
                    known_hash: None,
                },
                &CancellationToken::new(),
            )
            .await;
    });
    rpc_entered.notified().await;

    // Queue Client B behind Client A on the same device.
    let df_b = Arc::clone(&df);
    let ctx_b = ctx.clone();
    let cancel_b = cancel.clone();
    let client_b_task = tokio::spawn(async move {
        let d = df_b.create(&ctx_b, &cancel_b).await.unwrap();
        let h = d
            .hierarchy(&ctx_b.app_id, false, None, &cancel_b)
            .await
            .unwrap();
        d.shutdown().await.unwrap();
        h
    });

    // Abruptly terminate Client A's connection mid-RPC (no `LeaseRelease`).
    rpc_task.abort();
    let _ = rpc_task.await;
    drop(client_a);

    // Queued Client B MUST succeed on a fresh clean driver!
    let h = tokio::time::timeout(Duration::from_secs(3), client_b_task)
        .await
        .expect("Client B was blocked after Client A disconnected abruptly")
        .unwrap();
    assert_eq!(h.app_id, ctx.app_id);

    let drivers = factory.created_drivers.lock().unwrap().clone();
    assert_eq!(
        drivers.len(),
        2,
        "abruptly disconnected driver must be evicted and replaced"
    );
    assert!(
        drivers[0].hierarchy_cancelled.load(Ordering::SeqCst),
        "daemon must cancel active RPC token on abrupt disconnect"
    );
    assert_eq!(
        drivers[0].shutdown_count.load(Ordering::SeqCst),
        1,
        "evicted dirty driver must be shut down cleanly"
    );
    assert_eq!(
        drivers[1].shutdown_count.load(Ordering::SeqCst),
        0,
        "fresh driver for Client B stays warm after clean LeaseRelease"
    );

    srv_cancel.cancel();
    srv_handle.await.unwrap();
}

// ===========================================================================
// 5. Idle TTL Auto-Shutdown (And Non-Expiry While Leased)
// ===========================================================================

#[tokio::test]
async fn idle_ttl_does_not_expire_while_leased_then_shuts_down_all_drivers_after_release() {
    let dir = ShortDaemonDir::new("idle");
    let paths = DaemonPaths::from_dir(dir.path().to_path_buf()).unwrap();
    let factory = DaemonMockFactory::new(Arc::new(|_, _| DaemonMockDriver::new("ios")));

    let server = DaemonServer::bind(
        paths.clone(),
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        Duration::from_millis(200),
    )
    .unwrap();
    let srv_cancel = CancellationToken::new();
    let srv_handle = tokio::spawn(server.run(srv_cancel));

    let df = DaemonDriverFactory::with_daemon_dir(
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        dir.path().to_path_buf(),
        false,
    );
    let cancel = CancellationToken::new();
    let ctx = sample_ctx(
        "ios",
        "bbbbbbbb-1111-2222-3333-444444444444",
        "dev.deja.app",
    );

    // Hold lease longer than `idle_timeout` (350ms > 200ms) -> daemon MUST NOT shut down!
    let driver = df.create(&ctx, &cancel).await.unwrap();
    driver.ensure_session(&cancel).await.unwrap();
    tokio::time::sleep(Duration::from_millis(350)).await;
    let h = driver
        .hierarchy(&ctx.app_id, false, None, &cancel)
        .await
        .unwrap();
    assert_eq!(h.app_id, ctx.app_id);

    // Release lease cleanly -> idle TTL now fires and tears everything down!
    driver.shutdown().await.unwrap();

    tokio::time::timeout(Duration::from_secs(3), srv_handle)
        .await
        .expect("daemon did not auto-shutdown after idle TTL")
        .unwrap();

    assert_eq!(
        factory.total_shutdowns(),
        1,
        "all pooled drivers must be shut down on idle exit"
    );
    assert!(
        !paths.socket.exists(),
        "deja.sock must be removed on idle exit"
    );
    assert!(
        !paths.metadata.exists(),
        "daemon.json must be removed on idle exit"
    );
    assert!(
        !probe_daemon_lock_held(&paths).unwrap(),
        "daemon.lock must be unlocked on idle exit"
    );
}

// ===========================================================================
// 6. Graceful `Shutdown` RPC & `SIGTERM` To Foreground Daemon Tear Down All Pooled Drivers
// ===========================================================================

#[tokio::test]
async fn shutdown_rpc_and_sigterm_tear_down_all_pooled_drivers_and_files() {
    // Part A: Shutdown RPC tears down 2 pooled drivers cleanly.
    let dir = ShortDaemonDir::new("stopall");
    let paths = DaemonPaths::from_dir(dir.path().to_path_buf()).unwrap();
    let factory = DaemonMockFactory::new(Arc::new(|_, _| DaemonMockDriver::new("ios")));
    let server = DaemonServer::bind(
        paths.clone(),
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        Duration::from_secs(60),
    )
    .unwrap();
    let srv_handle = tokio::spawn(server.run(CancellationToken::new()));

    let df = DaemonDriverFactory::with_daemon_dir(
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        dir.path().to_path_buf(),
        false,
    );
    let cancel = CancellationToken::new();
    for udid in [
        "11111111-1111-1111-1111-111111111111",
        "22222222-2222-2222-2222-222222222222",
    ] {
        let d = df
            .create(&sample_ctx("ios", udid, "dev.deja.app"), &cancel)
            .await
            .unwrap();
        d.ensure_session(&cancel).await.unwrap();
        d.shutdown().await.unwrap();
    }
    assert_eq!(factory.create_count.load(Ordering::SeqCst), 2);
    assert_eq!(factory.total_shutdowns(), 0);

    let client = DaemonClient::connect(&paths).await.unwrap();
    let stopped = client.shutdown_daemon(&cancel).await.unwrap();
    assert_eq!(stopped, 2);

    tokio::time::timeout(Duration::from_secs(3), srv_handle)
        .await
        .expect("server did not finish after Shutdown RPC")
        .unwrap();
    assert_eq!(factory.total_shutdowns(), 2);
    assert!(!paths.socket.exists());
    assert!(!paths.metadata.exists());
    assert!(!probe_daemon_lock_held(&paths).unwrap());

    // Part B: Real foreground daemon subprocess cleans up cleanly on `SIGTERM`.
    let dir_fg = ShortDaemonDir::new("sigterm");
    let paths_fg = DaemonPaths::from_dir(dir_fg.path().to_path_buf()).unwrap();
    let mut child = deja_bin_with_daemon(dir_fg.path())
        .args(["daemon", "start", "--foreground", "--idle-timeout", "60"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn foreground daemon");

    let start = Instant::now();
    loop {
        if let Ok(c) = DaemonClient::connect(&paths_fg).await
            && c.ping(&cancel).await.is_ok()
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "foreground daemon failed to start"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    deja_driver_ios::send_sigterm_to_pid(child.id()).expect("send SIGTERM to foreground daemon");
    let wait_start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        assert!(
            wait_start.elapsed() < Duration::from_secs(5),
            "foreground daemon hung on SIGTERM"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(status.code(), Some(143));
    assert!(!paths_fg.socket.exists());
    assert!(!paths_fg.metadata.exists());
    assert!(!probe_daemon_lock_held(&paths_fg).unwrap());
}

// ===========================================================================
// 7. `--no-daemon` and `DEJA_NO_DAEMON=1` Bypass Daemon Completely
// ===========================================================================

#[tokio::test]
async fn no_daemon_flag_and_env_bypass_daemon_completely() {
    let ws = ShortDaemonDir::new("bypass-ws");
    fs::create_dir_all(ws.path()).unwrap();
    let build = ws.path().join("App.bin");
    fs::write(&build, b"v1").unwrap();
    let _fp = fingerprint_file(ws.path(), &build).unwrap();
    fs::write(
        ws.path().join("deja.config.json"),
        json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": "dev.deja.app",
            "adapter": "maestro-ios/v1",
            "buildPath": "App.bin",
            "device": "11111111-2222-3333-4444-555555555555"
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        ws.path().join("flow.yaml"),
        "appId: dev.deja.app\n---\n- launchApp\n- assertVisible: Continue\n",
    )
    .unwrap();

    // Point `DaemonDriverFactory` at an UNSTARTED daemon dir with `auto_start: false`.
    // Any attempt to route through the daemon would fail!
    let missing_daemon = ShortDaemonDir::new("missing");
    let mock_factory = DaemonMockFactory::new(Arc::new(|_, _| DaemonMockDriver::new("ios")));
    let wrapped: Arc<dyn DriverFactory> = Arc::new(DaemonDriverFactory::with_daemon_dir(
        Arc::clone(&mock_factory) as Arc<dyn DriverFactory>,
        missing_daemon.path().to_path_buf(),
        false,
    ));

    // 1. `--no-daemon` unwraps `DaemonDriverFactory` and executes directly on `mock_factory`!
    let (sink1, buf1) = OutputSink::captured();
    let deps1 = CliDeps {
        cwd: ws.path().to_path_buf(),
        env: BTreeMap::new(),
        home_dir: None,
        output: sink1,
        driver_factory: Arc::clone(&wrapped),
        doctor_probes: Arc::new(NoopDoctorProbes),
        cancel: CancellationToken::new(),
        cancel_signal: Arc::new(AtomicU8::new(0)),
    };
    let code1 = run_cli_from_args(
        [
            "deja",
            "--no-daemon",
            "remember",
            "smoke",
            "--flow",
            "flow.yaml",
            "--json",
        ],
        &deps1,
    )
    .await;
    assert_eq!(code1, 0, "stderr={}", buf1.lock().unwrap().stderr);
    assert_eq!(mock_factory.create_count.load(Ordering::SeqCst), 1);
    assert_eq!(mock_factory.total_shutdowns(), 1);
    assert!(!missing_daemon.path().exists());

    // 2. `DEJA_NO_DAEMON=1` unwraps `DaemonDriverFactory` and executes directly!
    let (sink2, buf2) = OutputSink::captured();
    let deps2 = CliDeps {
        cwd: ws.path().to_path_buf(),
        env: BTreeMap::from([("DEJA_NO_DAEMON".to_string(), "1".to_string())]),
        home_dir: None,
        output: sink2,
        driver_factory: Arc::clone(&wrapped),
        doctor_probes: Arc::new(NoopDoctorProbes),
        cancel: CancellationToken::new(),
        cancel_signal: Arc::new(AtomicU8::new(0)),
    };
    let code2 = run_cli_from_args(["deja", "run", "smoke", "--json"], &deps2).await;
    assert_eq!(code2, 0, "stderr={}", buf2.lock().unwrap().stderr);
    assert_eq!(mock_factory.create_count.load(Ordering::SeqCst), 2);
    assert_eq!(mock_factory.total_shutdowns(), 2);
    assert!(!missing_daemon.path().exists());

    // 3. `resolve_runtime` on `DaemonDriverFactory` delegates directly without touching daemon!
    let rt = wrapped
        .resolve_runtime(
            &sample_ctx(
                "ios",
                "11111111-2222-3333-4444-555555555555",
                "dev.deja.app",
            ),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(rt.as_deref(), Some("ios-27.2"));
    assert!(!missing_daemon.path().exists());

    // 4. `deja daemon start --foreground` unwraps `DaemonDriverFactory` without recursion!
    let fg_dir = ShortDaemonDir::new("fg-unwrap");
    let fg_paths = DaemonPaths::from_dir(fg_dir.path().to_path_buf()).unwrap();
    let fg_cancel = CancellationToken::new();
    let (sink3, _buf3) = OutputSink::captured();
    let deps3 = CliDeps {
        cwd: ws.path().to_path_buf(),
        env: BTreeMap::from([(
            "DEJA_DAEMON_DIR".to_string(),
            fg_dir.path().to_string_lossy().into_owned(),
        )]),
        home_dir: None,
        output: sink3,
        // Intentionally pass wrapped `DaemonDriverFactory` pointing at the SAME dir!
        driver_factory: Arc::new(DaemonDriverFactory::with_daemon_dir(
            Arc::clone(&mock_factory) as Arc<dyn DriverFactory>,
            fg_dir.path().to_path_buf(),
            false,
        )),
        doctor_probes: Arc::new(NoopDoctorProbes),
        cancel: fg_cancel.clone(),
        cancel_signal: Arc::new(AtomicU8::new(0)),
    };
    let fg_task = tokio::spawn(async move {
        run_cli_from_args(
            [
                "deja",
                "daemon",
                "start",
                "--foreground",
                "--idle-timeout",
                "30",
            ],
            &deps3,
        )
        .await
    });
    let wait_t0 = Instant::now();
    loop {
        if let Ok(c) = DaemonClient::connect(&fg_paths).await
            && c.ping(&CancellationToken::new()).await.is_ok()
        {
            break;
        }
        assert!(wait_t0.elapsed() < Duration::from_secs(3));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let client_factory = DaemonDriverFactory::with_daemon_dir(
        Arc::clone(&mock_factory) as Arc<dyn DriverFactory>,
        fg_dir.path().to_path_buf(),
        false,
    );
    let leased = client_factory
        .create(
            &sample_ctx(
                "ios",
                "99999999-2222-3333-4444-555555555555",
                "dev.deja.app",
            ),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    leased
        .ensure_session(&CancellationToken::new())
        .await
        .unwrap();
    leased.shutdown().await.unwrap();
    fg_cancel.cancel();
    assert_eq!(fg_task.await.unwrap(), 0);
}

// ===========================================================================
// 8. Cancel-Safe Dropped Future (`CancelRpc`), Secret Isolation & Full RPC Surface
// ===========================================================================

#[tokio::test]
async fn dropped_hierarchy_future_sends_cancel_rpc_without_corrupting_framing_and_isolates_secrets()
{
    let dir = ShortDaemonDir::new("cancel-sec");
    let paths = DaemonPaths::from_dir(dir.path().to_path_buf()).unwrap();
    let rpc_entered = Arc::new(Notify::new());
    let rpc_entered_c = Arc::clone(&rpc_entered);

    let factory = DaemonMockFactory::new(Arc::new(move |_ctx, _| {
        let mut d = DaemonMockDriver::new("ios");
        d.hierarchy_entered = Some(Arc::clone(&rpc_entered_c));
        *d.crash_signal.lock().unwrap() = Some(DriverCrashSignal {
            source: CrashSignalSource::CrashReport,
            report_file: Some("app.ips".to_string()),
            process: Some("App".to_string()),
            detail: Some("EXC_CRASH".to_string()),
        });
        d
    }));
    let server = DaemonServer::bind(
        paths.clone(),
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        Duration::from_secs(60),
    )
    .unwrap();
    let srv_cancel = CancellationToken::new();
    let srv_handle = tokio::spawn(server.run(srv_cancel.clone()));

    let df = DaemonDriverFactory::with_daemon_dir(
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        dir.path().to_path_buf(),
        false,
    );
    let cancel = CancellationToken::new();

    // Inject forbidden API key & fixture secret into `ctx.env` to verify strict allowlist stripping!
    let mut ctx = sample_ctx(
        "ios",
        "cccccccc-1111-2222-3333-444444444444",
        "dev.deja.app",
    );
    ctx.env.insert(
        "OPENAI_API_KEY".to_string(),
        "test-openai-secret-val".to_string(),
    );
    ctx.env.insert(
        "FIXTURE_SECRET".to_string(),
        "super-secret-password".to_string(),
    );
    ctx.env.insert(
        "DEVELOPER_DIR".to_string(),
        "/Applications/Xcode.app/Contents/Developer".to_string(),
    );

    let driver = df.create(&ctx, &cancel).await.unwrap();
    let received_ctx = factory.received_contexts.lock().unwrap()[0].clone();
    assert!(!received_ctx.env.contains_key("OPENAI_API_KEY"));
    assert!(!received_ctx.env.contains_key("FIXTURE_SECRET"));
    assert_eq!(
        received_ctx.env.get("DEVELOPER_DIR").map(String::as_str),
        Some("/Applications/Xcode.app/Contents/Developer")
    );

    let mock_ref = factory.created_drivers.lock().unwrap()[0].clone();

    // 1. Drop an in-flight `hierarchy()` future via short timeout -> `CancelRpc` sent!
    mock_ref
        .block_hierarchy_until_cancel
        .store(true, Ordering::SeqCst);
    let timed = tokio::time::timeout(
        Duration::from_millis(40),
        driver.hierarchy(&ctx.app_id, false, None, &cancel),
    )
    .await;
    assert!(
        timed.is_err(),
        "expected timeout to drop in-flight hierarchy future"
    );

    // Wait briefly until server processes `CancelRpc`, then unblock subsequent calls.
    let wait_cancel = Instant::now();
    while !mock_ref.hierarchy_cancelled.load(Ordering::SeqCst) {
        assert!(
            wait_cancel.elapsed() < Duration::from_secs(2),
            "daemon did not cancel dropped RPC"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    mock_ref
        .block_hierarchy_until_cancel
        .store(false, Ordering::SeqCst);

    // Immediately issue a fresh `hierarchy()` + full `wait_local` + EVERY `PlatformDriver` RPC on
    // the SAME `DaemonDriver` to prove NDJSON framing stayed 100% intact!
    let h = driver
        .hierarchy(&ctx.app_id, false, None, &cancel)
        .await
        .unwrap();
    assert_eq!(h.nodes[0].label, "Continue");

    let wl_outcome = wait_local(
        driver.as_ref(),
        &WaitSelectorRequest::visible(
            &ctx.app_id,
            Selector::by_text("Continue"),
            Duration::from_millis(500),
        ),
        &cancel,
    )
    .await
    .unwrap();
    assert!(wl_outcome.matched);

    // 2. Client-side redactor secret isolation on `wait_for_selector`:
    let secret_pw = "MySecretFixturePass99";
    let provider_tok = format!("{}abcdefghijklmnopqrstuvwx1234", "sk-");
    *mock_ref.hierarchy_nodes.lock().unwrap() = vec![sample_node(
        "secret_field",
        &format!("Welcome {secret_pw} token={provider_tok}"),
    )];
    let redactor = create_redactor("/tmp/ws", None, &[secret_pw.to_string()]);
    let prov_redactor = provider_key_redactor([("OPENAI_API_KEY", provider_tok.as_str())]);
    let mut wait_req = WaitSelectorRequest::visible(
        &ctx.app_id,
        Selector::by_text("NonExistentButton"),
        Duration::ZERO,
    );
    wait_req.return_hierarchy = false;
    wait_req.redactor = Some(redactor);
    wait_req.provider_redactor = Some(prov_redactor);

    let wait_out = driver.wait_for_selector(&wait_req, &cancel).await.unwrap();
    assert!(!wait_out.matched);
    assert!(
        wait_out.hierarchy.is_none(),
        "must strip hierarchy when caller requested return_hierarchy = false"
    );
    let summary = wait_out.visible_context.format_summary();
    assert!(
        !summary.contains(secret_pw) && !summary.contains(&provider_tok),
        "client-side redactor must scrub VisibleContext without transmitting secrets: {summary}"
    );
    assert_ne!(wait_out.visible_context, VisibleContext::default());
    let server_wait = mock_ref.recorded_waits.lock().unwrap()[0].clone();
    assert!(
        !server_wait.had_redactor,
        "redactor must NEVER be sent over daemon IPC"
    );
    assert!(
        !server_wait.had_provider_redactor,
        "provider redactor must NEVER be sent over daemon IPC"
    );
    assert!(
        server_wait.return_hierarchy,
        "client must request hierarchy to recompute VisibleContext locally"
    );

    // 3. Exercise remaining `PlatformDriver` RPCs losslessly over IPC.
    driver.ensure_session(&cancel).await.unwrap();
    driver
        .launch_app(&ctx.app_id, &LaunchOptions::default(), &cancel)
        .await
        .unwrap();
    driver.stop_app(&ctx.app_id, &cancel).await.unwrap();
    driver.kill_app(&ctx.app_id, &cancel).await.unwrap();
    driver.clear_state(&ctx.app_id, &cancel).await.unwrap();
    driver.clear_keychain(&cancel).await.unwrap();
    driver
        .open_link(
            Some(&ctx.app_id),
            "https://example.com",
            &OpenLinkOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
    driver
        .add_media(&[PathBuf::from("/tmp/img.png")], &cancel)
        .await
        .unwrap();
    driver
        .tap_point(&ctx.app_id, UiPoint::new(10.0, 20.0), &cancel)
        .await
        .unwrap();
    driver
        .long_press_point(&ctx.app_id, UiPoint::new(10.0, 20.0), Some(500), &cancel)
        .await
        .unwrap();
    driver
        .double_tap_point(&ctx.app_id, UiPoint::new(10.0, 20.0), &cancel)
        .await
        .unwrap();
    driver
        .swipe(
            &SwipeParams {
                app_id: ctx.app_id.clone(),
                start: UiPoint::new(10.0, 200.0),
                end: UiPoint::new(10.0, 50.0),
                duration_ms: 250,
                hold_duration_ms: None,
            },
            &cancel,
        )
        .await
        .unwrap();
    driver
        .input_text(&ctx.app_id, "hello", &cancel)
        .await
        .unwrap();
    driver.erase_text(&ctx.app_id, 3, &cancel).await.unwrap();
    driver
        .press_key(&ctx.app_id, &KeyPress::single("Enter"), &cancel)
        .await
        .unwrap();
    driver.hide_keyboard(&ctx.app_id, &cancel).await.unwrap();
    driver.back(&ctx.app_id, &cancel).await.unwrap();

    let png = driver.take_screenshot(&ctx.app_id, &cancel).await.unwrap();
    assert_eq!(png, vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

    let crash = driver
        .check_app_crash(&ctx.app_id, 1000, &cancel)
        .await
        .unwrap();
    assert_eq!(
        crash.as_ref().and_then(|c| c.report_file.as_deref()),
        Some("app.ips")
    );

    let act_out = driver
        .act_and_observe(
            &ActPayload {
                app_id: ctx.app_id.clone(),
                action: ActAction::Tap { x: 25.0, y: 35.0 },
                post_action_delay_ms: None,
                return_hierarchy: true,
                include_system_ui: false,
                known_hash: None,
            },
            &cancel,
        )
        .await
        .unwrap();
    assert!(act_out.action_ok);
    assert!(act_out.hierarchy.is_some());

    // Clean shutdown releases lease while keeping driver warm (`shutdown_count == 0`).
    driver.shutdown().await.unwrap();
    assert_eq!(mock_ref.shutdown_count.load(Ordering::SeqCst), 0);

    srv_cancel.cancel();
    srv_handle.await.unwrap();
    assert_eq!(mock_ref.shutdown_count.load(Ordering::SeqCst), 1);
}

// ===========================================================================
// 9. Codex Round 1 Regression Tests (Findings 1 through 10)
// ===========================================================================

#[tokio::test]
async fn regression_r1_f1_mcp_releases_daemon_lease_after_tool_call_while_staying_idle() {
    use deja_mcp::{McpBoxFuture, McpDriverContext, McpDriverFactory, McpServerConfig};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    struct TestMcpBridge {
        inner: Arc<dyn DriverFactory>,
        cancel: CancellationToken,
    }

    impl TestMcpBridge {
        fn to_ctx(ctx: &McpDriverContext) -> DriverContext {
            DriverContext {
                platform: ctx.platform.clone(),
                device: ctx.device.clone(),
                app_id: ctx.app_id.clone(),
                build_path: ctx.build_path.clone(),
                expected_build_fingerprint: ctx.expected_build_fingerprint.clone(),
                workspace_root: ctx.workspace_root.clone(),
                disable_animations: ctx.disable_animations,
                crash_report_grace_ms: ctx.crash_report_grace_ms,
                crash_report_dir: ctx.crash_report_dir.clone(),
                deja_home: None,
                headless: ctx.headless,
                screen_size: ctx.screen_size.clone(),
                timeout_ms: ctx.timeout_ms,
                env: ctx.env.clone(),
                staged_build: None,
                resolved_runtime: ctx.resolved_runtime.clone(),
            }
        }
    }

    impl McpDriverFactory for TestMcpBridge {
        fn resolve_runtime<'a>(
            &'a self,
            ctx: &'a McpDriverContext,
            cancel: &'a CancellationToken,
        ) -> McpBoxFuture<'a, Result<Option<String>, DriverError>> {
            let dc = Self::to_ctx(ctx);
            let inner = Arc::clone(&self.inner);
            Box::pin(async move {
                inner
                    .resolve_runtime(&dc, cancel)
                    .await
                    .map_err(|e| DriverError::InvalidArgument(e.to_string()))
            })
        }

        fn install_app<'a>(
            &'a self,
            ctx: &'a McpDriverContext,
            cancel: &'a CancellationToken,
        ) -> McpBoxFuture<'a, Result<(), DriverError>> {
            let dc = Self::to_ctx(ctx);
            let inner = Arc::clone(&self.inner);
            Box::pin(async move {
                inner
                    .install_app(&dc, cancel)
                    .await
                    .map_err(|e| DriverError::InvalidArgument(e.to_string()))
            })
        }

        fn create_driver<'a>(
            &'a self,
            ctx: &'a McpDriverContext,
        ) -> McpBoxFuture<'a, Result<Box<dyn PlatformDriver>, DriverError>> {
            let dc = Self::to_ctx(ctx);
            let inner = Arc::clone(&self.inner);
            let cancel = self.cancel.child_token();
            Box::pin(async move {
                inner
                    .create(&dc, &cancel)
                    .await
                    .map_err(|e| DriverError::InvalidArgument(e.to_string()))
            })
        }
    }

    let dir = ShortDaemonDir::new("r1-mcp");
    let paths = DaemonPaths::from_dir(dir.path().to_path_buf()).unwrap();
    let ws = ShortDaemonDir::new("r1-mcp-ws");
    fs::create_dir_all(ws.path()).unwrap();
    let build = ws.path().join("App.bin");
    fs::write(&build, b"v1").unwrap();

    let mock_factory = DaemonMockFactory::new(Arc::new(|_, _| DaemonMockDriver::new("ios")));
    let server = DaemonServer::bind(
        paths.clone(),
        Arc::clone(&mock_factory) as Arc<dyn DriverFactory>,
        Duration::from_secs(60),
    )
    .unwrap();
    let srv_cancel = CancellationToken::new();
    let srv_handle = tokio::spawn(server.run(srv_cancel.clone()));

    let daemon_factory: Arc<dyn DriverFactory> = Arc::new(DaemonDriverFactory::with_daemon_dir(
        Arc::clone(&mock_factory) as Arc<dyn DriverFactory>,
        dir.path().to_path_buf(),
        false,
    ));

    let mcp_cancel = CancellationToken::new();
    let (mut client_to_srv, srv_stdin) = tokio::io::duplex(64 * 1024);
    let (srv_stdout, client_from_srv) = tokio::io::duplex(64 * 1024);
    let mut client_reader = BufReader::new(client_from_srv);

    let mcp_cfg = McpServerConfig {
        workspace_root: ws.path().to_path_buf(),
        canonical_workspace_root: fs::canonicalize(ws.path()).unwrap(),
        home_dir: None,
        store_id: None,
        platform: "ios".to_string(),
        app_id: "dev.deja.app".to_string(),
        adapter: Some("maestro-ios/v1".to_string()),
        browser: None,
        build_path: Some(build.clone()),
        device: Some("11111111-2222-3333-4444-555555555555".to_string()),
        manifest_path: None,
        flows_dir: None,
        timeout_ms: 30_000,
        verification_timeout_ms: 30_000,
        retries: 0,
        crash_report_grace_ms: 0,
        crash_report_dir: None,
        disable_animations: true,
        stale_build: None,
        env: BTreeMap::new(),
        process_env: BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
        forbidden_values: Vec::new(),
        mcp_explore: false,
        explore_backend_available: false,
        headless: None,
        screen_size: None,
    };

    let bridge: Arc<dyn McpDriverFactory> = Arc::new(TestMcpBridge {
        inner: Arc::clone(&daemon_factory),
        cancel: mcp_cancel.clone(),
    });
    let mcp_task = tokio::spawn(deja_mcp::serve(
        srv_stdin,
        srv_stdout,
        mcp_cfg,
        bridge,
        mcp_cancel.clone(),
    ));

    // Initialize MCP and run `deja_exec_batch`.
    client_to_srv
        .write_all(
            format!(
                "{}\n{}\n{}\n",
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
                json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"deja_exec_batch","arguments":{"steps":[{"launchApp":"dev.deja.app"},{"assertVisible":"Continue"}]}}})
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    let mut line = String::new();
    client_reader.read_line(&mut line).await.unwrap(); // init response
    line.clear();
    client_reader.read_line(&mut line).await.unwrap(); // deja_exec_batch response
    let batch_resp: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(batch_resp["result"]["isError"], false);

    // Keep MCP server OPEN & IDLE! Daemon MUST show `activeLeases == 0` and warm pooled driver!
    let status_client = DaemonClient::connect(&paths).await.unwrap();
    let status = status_client
        .status(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        status.active_leases, 0,
        "idle MCP server must release daemon lease immediately after tool call"
    );
    assert_eq!(status.pooled_sessions, 1);
    assert_eq!(status.sessions[0].attach_count, Some(1));
    assert!(!status.sessions[0].leased);

    // A separate CLI client (`deja run`) IMMEDIATELY acquires the SAME warm driver without blocking!
    let cli_ctx = sample_ctx(
        "ios",
        "11111111-2222-3333-4444-555555555555",
        "dev.deja.app",
    );
    let cli_driver = tokio::time::timeout(
        Duration::from_secs(2),
        daemon_factory.create(&cli_ctx, &CancellationToken::new()),
    )
    .await
    .expect("parallel CLI client must not block behind idle MCP server")
    .unwrap();
    cli_driver
        .ensure_session(&CancellationToken::new())
        .await
        .unwrap();
    cli_driver.shutdown().await.unwrap();

    let status2 = status_client
        .status(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(status2.sessions[0].attach_count, Some(1));
    assert_eq!(status2.sessions[0].lease_count, 2);
    assert_eq!(mock_factory.create_count.load(Ordering::SeqCst), 1);

    drop(client_to_srv);
    mcp_cancel.cancel();
    let _ = mcp_task.await;
    srv_cancel.cancel();
    srv_handle.await.unwrap();
}

#[tokio::test]
async fn regression_r1_f2_f3_uncooperative_lock_holder_refuses_unverified_sigterm_and_exits_failure()
 {
    let dir = ShortDaemonDir::new("r1-stop-fail");
    let paths = DaemonPaths::from_dir(dir.path().to_path_buf()).unwrap();

    // Hold `daemon.lock` in-process (the test binary itself is NOT `deja daemon --foreground`!)
    // and write `daemon.json` with a forged/mismatched `procStartTime`.
    let _held_lock = acquire_daemon_lock(&paths).unwrap().expect("acquire lock");
    let forged_meta = DaemonMetadata {
        version: "0.1.0".to_string(),
        pid: std::process::id(),
        uid: current_uid().unwrap(),
        socket_path: paths.socket.to_string_lossy().into_owned(),
        started_at: "2026-01-01T00:00:00Z".to_string(),
        idle_timeout_secs: 300,
        proc_start_time: Some("Mon Jan 01 00:00:00 1990".to_string()),
    };
    write_daemon_metadata(&paths, &forged_meta).unwrap();

    let (sink, buf) = OutputSink::captured();
    let deps = CliDeps {
        cwd: std::env::current_dir().unwrap(),
        env: BTreeMap::from([(
            "DEJA_DAEMON_DIR".to_string(),
            dir.path().to_string_lossy().into_owned(),
        )]),
        home_dir: None,
        output: sink,
        driver_factory: Arc::new(RealDriverFactory),
        doctor_probes: Arc::new(NoopDoctorProbes),
        cancel: CancellationToken::new(),
        cancel_signal: Arc::new(AtomicU8::new(0)),
    };

    // Must NOT send SIGTERM to our own test process, and MUST exit 1 with `stopped: false`!
    let code = run_stop_with_timeouts(
        true,
        &deps,
        Duration::from_millis(80),
        Duration::from_millis(80),
        Duration::from_millis(80),
    )
    .await;
    assert_eq!(code, 1, "unconfirmed daemon stop must exit non-zero");
    let out: Value = serde_json::from_str(&buf.lock().unwrap().stdout).unwrap();
    assert_eq!(out["stopped"], false);
    assert_eq!(out["error"]["code"], "DAEMON_STOP_FAILED");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("refused"),
        "expected process-identity verification refusal, got: {out}"
    );
}

#[tokio::test]
async fn regression_r1_f4_concurrent_starters_wait_for_lock_winner_to_publish_socket() {
    let dir = ShortDaemonDir::new("r1-race");
    let paths = DaemonPaths::from_dir(dir.path().to_path_buf()).unwrap();
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_deja"));
    let env = BTreeMap::from([(
        "DEJA_DAEMON_DIR".to_string(),
        dir.path().to_string_lossy().into_owned(),
    )]);

    // Part A: Simulate a winner holding `daemon.lock` for 150ms BEFORE starting the real daemon
    // while another caller runs `ensure_background_daemon_started_with_exe`: the caller's spawned
    // child loses `daemon.lock` and exits immediately, yet the caller MUST keep polling while
    // `daemon.lock` is held and connect cleanly once the winner publishes `deja.sock`!
    let pre_lock = acquire_daemon_lock(&paths).unwrap().expect("pre-lock");
    let paths_winner = paths.clone();
    let srv_cancel = CancellationToken::new();
    let srv_cancel_c = srv_cancel.clone();
    let winner_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(180)).await;
        drop(pre_lock);
        let mock = DaemonMockFactory::new(Arc::new(|_, _| DaemonMockDriver::new("ios")));
        let server = DaemonServer::bind(
            paths_winner,
            mock as Arc<dyn DriverFactory>,
            Duration::from_secs(60),
        )
        .unwrap();
        server.run(srv_cancel_c).await;
    });

    let info = ensure_background_daemon_started_with_exe(
        &paths,
        &env,
        60,
        &exe,
        Duration::from_secs(5),
        &CancellationToken::new(),
    )
    .await
    .expect("starter must wait while another winner holds daemon.lock");
    assert_eq!(info.pid, std::process::id());
    assert!(info.already_running);

    srv_cancel.cancel();
    winner_task.await.unwrap();

    // Part B: Two real concurrent callers racing against an empty `DEJA_DAEMON_DIR` both succeed!
    let dir2 = ShortDaemonDir::new("r1-race2");
    let paths2 = DaemonPaths::from_dir(dir2.path().to_path_buf()).unwrap();
    let env2 = BTreeMap::from([(
        "DEJA_DAEMON_DIR".to_string(),
        dir2.path().to_string_lossy().into_owned(),
    )]);
    let c = CancellationToken::new();
    let (r1, r2) = tokio::join!(
        ensure_background_daemon_started_with_exe(
            &paths2,
            &env2,
            60,
            &exe,
            Duration::from_secs(5),
            &c
        ),
        ensure_background_daemon_started_with_exe(
            &paths2,
            &env2,
            60,
            &exe,
            Duration::from_secs(5),
            &c
        ),
    );
    let i1 = r1.expect("concurrent starter 1 must succeed");
    let i2 = r2.expect("concurrent starter 2 must succeed");
    assert_eq!(i1.pid, i2.pid);

    // Verify `daemon.json` carries `procStartTime`.
    let meta = read_daemon_metadata(&paths2).unwrap();
    assert!(
        meta.proc_start_time
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty()),
        "daemon.json must record procStartTime"
    );

    let stop = deja_bin_with_daemon(dir2.path())
        .args(["daemon", "stop", "--json"])
        .output()
        .unwrap();
    assert_eq!(stop.status.code(), Some(0));
}

#[tokio::test]
async fn regression_r1_f8_web_runtime_upgrade_invalidates_and_evicts_stale_warm_browser() {
    let dir = ShortDaemonDir::new("r1-web-upg");
    let paths = DaemonPaths::from_dir(dir.path().to_path_buf()).unwrap();
    let factory = DaemonMockFactory::new(Arc::new(|_, _| DaemonMockDriver::new("web")));
    let server = DaemonServer::bind(
        paths.clone(),
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        Duration::from_secs(60),
    )
    .unwrap();
    let srv_cancel = CancellationToken::new();
    let srv_handle = tokio::spawn(server.run(srv_cancel.clone()));

    let df = DaemonDriverFactory::with_daemon_dir(
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        dir.path().to_path_buf(),
        false,
    );
    let cancel = CancellationToken::new();

    // Run #1 with Chromium 158.
    let mut ctx_v1 = sample_ctx("web", "chromium", "http://127.0.0.1:4174");
    ctx_v1.resolved_runtime = Some("chromium-158.0.0.1@1280x720".to_string());
    let d1 = df.create(&ctx_v1, &cancel).await.unwrap();
    d1.ensure_session(&cancel).await.unwrap();
    d1.shutdown().await.unwrap();
    assert_eq!(factory.create_count.load(Ordering::SeqCst), 1);

    // Run #2 with SAME Chromium 158 -> warm hit!
    let d2 = df.create(&ctx_v1, &cancel).await.unwrap();
    d2.ensure_session(&cancel).await.unwrap();
    d2.shutdown().await.unwrap();
    assert_eq!(factory.create_count.load(Ordering::SeqCst), 1);
    assert_eq!(factory.total_shutdowns(), 0);

    // Run #3 after in-place Chromium upgrade to 159 -> MUST evict v158 & spawn fresh v159!
    let mut ctx_v2 = ctx_v1.clone();
    ctx_v2.resolved_runtime = Some("chromium-159.0.0.2@1280x720".to_string());
    let d3 = df.create(&ctx_v2, &cancel).await.unwrap();
    d3.ensure_session(&cancel).await.unwrap();
    d3.shutdown().await.unwrap();

    assert_eq!(factory.create_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        factory.created_drivers.lock().unwrap()[0]
            .shutdown_count
            .load(Ordering::SeqCst),
        1,
        "stale Chromium v158 warm driver must be shut down on runtime upgrade"
    );
    let status = DaemonClient::connect(&paths)
        .await
        .unwrap()
        .status(&cancel)
        .await
        .unwrap();
    assert_eq!(
        status.pooled_sessions, 1,
        "upgraded browser must replace stale slot rather than leaking an extra pooled session"
    );

    srv_cancel.cancel();
    srv_handle.await.unwrap();
}

#[test]
fn regression_r1_f9_daemon_uses_stable_owned_tmpdir_and_survives_caller_tmpdir_deletion() {
    let dir = ShortDaemonDir::new("r1-tmp");
    let ephemeral_tmp = ShortDaemonDir::new("r1-ephem");
    fs::create_dir_all(ephemeral_tmp.path()).unwrap();

    // Client 1 starts daemon with a short-lived ephemeral `TMPDIR`.
    let start = deja_bin_with_daemon(dir.path())
        .env("TMPDIR", ephemeral_tmp.path())
        .args(["daemon", "start", "--idle-timeout", "60", "--json"])
        .output()
        .unwrap();
    assert_eq!(start.status.code(), Some(0));

    // Daemon must have created its own `0o700` `<daemon_dir>/tmp` directory.
    let daemon_tmp = dir.path().join("tmp");
    let tmp_meta = fs::symlink_metadata(&daemon_tmp).expect("daemon tmp dir must exist");
    assert!(tmp_meta.is_dir() && !tmp_meta.file_type().is_symlink());
    assert_eq!(tmp_meta.permissions().mode() & 0o777, 0o700);

    // Delete Client 1's ephemeral `TMPDIR` completely.
    fs::remove_dir_all(ephemeral_tmp.path()).unwrap();
    assert!(!ephemeral_tmp.path().exists());

    // Client 2 queries daemon status cleanly and daemon-owned tmp dir remains intact until stop.
    let status = deja_bin_with_daemon(dir.path())
        .args(["daemon", "status", "--json"])
        .output()
        .unwrap();
    assert_eq!(status.status.code(), Some(0));
    let status_json: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status_json["running"], true);
    assert!(daemon_tmp.is_dir());

    // `deja daemon stop` cleans up `<daemon_dir>/tmp` cleanly.
    let stop = deja_bin_with_daemon(dir.path())
        .args(["daemon", "stop", "--json"])
        .output()
        .unwrap();
    assert_eq!(stop.status.code(), Some(0));
    assert!(!daemon_tmp.exists());
}
