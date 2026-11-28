//! Comprehensive deterministic CLI integration tests (subprocess + mock-driver in-process).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deja_cli::driver_factory::{
    DoctorCheck, DoctorCheckStatus, DoctorProbeRequest, DoctorProbes, DriverContext, DriverFactory,
    DriverFactoryError, FactoryFuture, RealDriverFactory, SharedStagedBuild,
    stage_and_verify_ios_app_bundle, verify_ios_app_bundle,
};
use deja_cli::output::OutputSink;
use deja_cli::{CliDeps, EMBEDDED_AGENT_GUIDE_MD, run_cli_from_args};
use deja_core::{
    CrashSignalSource, DEFAULT_FLOW_SCHEMA_VERSION, WalkIdentity, WalkOrigin, assertions_sha256,
    collect_flow_closure, fingerprint_file, fingerprint_tree,
};
use deja_driver_ios::{Simctl, SimctlEnv};
use deja_flow::{
    DriverCapabilities, DriverCapability, DriverCrashSignal, DriverError, DriverFuture, KeyPress,
    LaunchOptions, OpenLinkOptions, PlatformDriver, SwipeParams, UiHierarchy, UiNode, UiPoint,
    UiRect, UiSize,
};
use deja_store::{RecordValidationInput, SqliteStoreOptions, SqliteWalkStore, now_iso8601};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

static TEST_NONCE: AtomicU64 = AtomicU64::new(1);
const TEST_UDID: &str = "11111111-2222-3333-4444-555555555555";
const TEST_APP_ID: &str = "dev.deja.test.app";

struct TempWs {
    path: PathBuf,
}

impl TempWs {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = TEST_NONCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "deja-cli-int-{name}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let canonical = fs::canonicalize(&dir).unwrap();
        Self { path: canonical }
    }

    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let full = self.path.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full, content).unwrap();
        full
    }

    fn write_build(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let full = self.path.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full, bytes).unwrap();
        full
    }
}

impl Drop for TempWs {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn deja_bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_deja"));
    cmd.env_remove("DEJA_DEVICE")
        .env_remove("DEJA_MAESTRO")
        .env_remove("DEJA_TIMEOUT_MS");
    cmd
}

// ---------------------------------------------------------------------------
// Mock Driver + Factory + Doctor Probes for deterministic in-process testing
// ---------------------------------------------------------------------------

type HierarchyScript = Arc<Mutex<Vec<UiHierarchy>>>;
type OnLaunchHook = Arc<dyn Fn(usize) + Send + Sync>;
type OnEnsureSessionHook = Arc<dyn Fn() + Send + Sync>;
type OnCreateHook = Arc<dyn Fn(&DriverContext) + Send + Sync>;
type CapturedMedia = Arc<Mutex<Vec<(PathBuf, Vec<u8>)>>>;

#[derive(Clone)]
struct MockDriver {
    caps: DriverCapabilities,
    hierarchies: HierarchyScript,
    launch_count: Arc<AtomicUsize>,
    shutdown_count: Arc<AtomicUsize>,
    killed_apps: Arc<Mutex<Vec<String>>>,
    captured_media: CapturedMedia,
    crash_probe_calls: Arc<AtomicUsize>,
    crash_probe_delay_first: Option<Duration>,
    crash_signal: Arc<Mutex<Option<DriverCrashSignal>>>,
    on_launch: Option<OnLaunchHook>,
    on_ensure_session: Option<OnEnsureSessionHook>,
}

impl MockDriver {
    fn new(hierarchies: Vec<UiHierarchy>) -> Self {
        Self {
            caps: DriverCapabilities::with([
                DriverCapability::AccessibilityTree,
                DriverCapability::AppCrashReports,
            ]),
            hierarchies: Arc::new(Mutex::new(hierarchies)),
            launch_count: Arc::new(AtomicUsize::new(0)),
            shutdown_count: Arc::new(AtomicUsize::new(0)),
            killed_apps: Arc::new(Mutex::new(Vec::new())),
            captured_media: Arc::new(Mutex::new(Vec::new())),
            crash_probe_calls: Arc::new(AtomicUsize::new(0)),
            crash_probe_delay_first: None,
            crash_signal: Arc::new(Mutex::new(None)),
            on_launch: None,
            on_ensure_session: None,
        }
    }

    fn with_crash(self, sig: DriverCrashSignal) -> Self {
        *self.crash_signal.lock().unwrap() = Some(sig);
        self
    }

    fn with_crash_probe_delay_first(mut self, delay: Duration) -> Self {
        self.crash_probe_delay_first = Some(delay);
        self
    }

    fn with_on_launch(mut self, hook: OnLaunchHook) -> Self {
        self.on_launch = Some(hook);
        self
    }

    fn with_on_ensure_session(mut self, hook: OnEnsureSessionHook) -> Self {
        self.on_ensure_session = Some(hook);
        self
    }
}

fn node_with_label(id: &str, label: &str) -> UiNode {
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
        frame: UiRect::new(10.0, 20.0, 100.0, 44.0),
        ..UiNode::default()
    }
}

fn hierarchy_with_nodes(app_id: &str, nodes: Vec<UiNode>) -> UiHierarchy {
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

impl PlatformDriver for MockDriver {
    fn platform(&self) -> &'static str {
        "ios"
    }

    fn capabilities(&self) -> &DriverCapabilities {
        &self.caps
    }

    fn ensure_session<'a>(&'a self, cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if let Some(ref hook) = self.on_ensure_session {
                hook();
            }
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            Ok(())
        })
    }

    fn launch_app<'a>(
        &'a self,
        _app_id: &'a str,
        _opts: &'a LaunchOptions,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let n = self.launch_count.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some(ref hook) = self.on_launch {
                hook(n);
            }
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            Ok(())
        })
    }

    fn stop_app<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn kill_app<'a>(
        &'a self,
        app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            self.killed_apps.lock().unwrap().push(app_id);
            Ok(())
        })
    }

    fn clear_state<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn clear_keychain<'a>(&'a self, _cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn open_link<'a>(
        &'a self,
        _app_id: Option<&'a str>,
        _url: &'a str,
        _opts: &'a OpenLinkOptions,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn add_media<'a>(
        &'a self,
        paths: &'a [PathBuf],
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let paths = paths.to_vec();
        Box::pin(async move {
            let mut guard = self.captured_media.lock().unwrap();
            for p in paths {
                let bytes = fs::read(&p).unwrap_or_default();
                guard.push((p, bytes));
            }
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
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let mut guard = self.hierarchies.lock().unwrap();
            let h = if guard.len() > 1 {
                guard.remove(0)
            } else if let Some(last) = guard.first() {
                last.clone()
            } else {
                hierarchy_with_nodes(app_id, Vec::new())
            };
            Ok(h)
        })
    }

    fn tap_point<'a>(
        &'a self,
        _app_id: &'a str,
        _point: UiPoint,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn long_press_point<'a>(
        &'a self,
        _app_id: &'a str,
        _point: UiPoint,
        _duration_ms: Option<u64>,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn double_tap_point<'a>(
        &'a self,
        _app_id: &'a str,
        _point: UiPoint,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn swipe<'a>(
        &'a self,
        _params: &'a SwipeParams,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn input_text<'a>(
        &'a self,
        _app_id: &'a str,
        _text: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn erase_text<'a>(
        &'a self,
        _app_id: &'a str,
        _count: u32,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn press_key<'a>(
        &'a self,
        _app_id: &'a str,
        _key: &'a KeyPress,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn hide_keyboard<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn back<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn take_screenshot<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Vec<u8>> {
        Box::pin(async { Ok(b"\x89PNG\r\n\x1a\n".to_vec()) })
    }

    fn check_app_crash<'a>(
        &'a self,
        _app_id: &'a str,
        _since_epoch_ms: u64,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Option<DriverCrashSignal>> {
        Box::pin(async move {
            let call_num = self.crash_probe_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call_num == 1
                && let Some(delay) = self.crash_probe_delay_first
            {
                tokio::select! {
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            Ok(self.crash_signal.lock().unwrap().clone())
        })
    }

    fn shutdown<'a>(&'a self) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.shutdown_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

struct MockFactory {
    runtime: Option<String>,
    driver: MockDriver,
    create_calls: Arc<AtomicUsize>,
    on_create: Option<OnCreateHook>,
    verify_ios_stage_on_install: bool,
}

impl MockFactory {
    fn new(runtime: Option<&str>, driver: MockDriver) -> Self {
        Self {
            runtime: runtime.map(ToString::to_string),
            driver,
            create_calls: Arc::new(AtomicUsize::new(0)),
            on_create: None,
            verify_ios_stage_on_install: false,
        }
    }

    fn with_on_create(mut self, hook: OnCreateHook) -> Self {
        self.on_create = Some(hook);
        self
    }

    fn with_ios_stage_verification(mut self) -> Self {
        self.verify_ios_stage_on_install = true;
        self
    }
}

impl DriverFactory for MockFactory {
    fn resolve_runtime<'a>(
        &'a self,
        _ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Option<String>, DriverFactoryError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverFactoryError::Cancelled);
            }
            Ok(self.runtime.clone())
        })
    }

    fn create<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Box<dyn PlatformDriver>, DriverFactoryError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverFactoryError::Cancelled);
            }
            if let Some(ref hook) = self.on_create {
                hook(ctx);
            }
            self.create_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(self.driver.clone()) as Box<dyn PlatformDriver>)
        })
    }

    fn install_app<'a>(
        &'a self,
        ctx: &'a DriverContext,
        _cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<(), DriverFactoryError>> {
        Box::pin(async move {
            if self.verify_ios_stage_on_install && ctx.platform == "ios" {
                let _staged = stage_and_verify_ios_app_bundle(
                    &ctx.build_path,
                    &ctx.app_id,
                    ctx.expected_build_fingerprint.as_deref(),
                )
                .map_err(DriverFactoryError::UnsupportedOrConfig)?;
            }
            Ok(())
        })
    }
}

struct MockDoctorProbes {
    checks: Vec<DoctorCheck>,
}

impl DoctorProbes for MockDoctorProbes {
    fn probe<'a>(&'a self, _req: &'a DoctorProbeRequest) -> FactoryFuture<'a, Vec<DoctorCheck>> {
        let out = self.checks.clone();
        Box::pin(async move { out })
    }
}

fn make_deps(
    cwd: &Path,
    factory: Arc<dyn DriverFactory>,
    probes: Arc<dyn DoctorProbes>,
    cancel: CancellationToken,
) -> (CliDeps, Arc<Mutex<deja_cli::output::CapturedOutput>>) {
    let (sink, captured) = OutputSink::captured();
    let deps = CliDeps {
        cwd: cwd.to_path_buf(),
        env: BTreeMap::new(),
        home_dir: None,
        output: sink,
        driver_factory: factory,
        doctor_probes: probes,
        cancel,
        cancel_signal: Arc::new(AtomicU8::new(0)),
    };
    (deps, captured)
}

fn default_probes() -> Arc<dyn DoctorProbes> {
    Arc::new(MockDoctorProbes {
        checks: vec![DoctorCheck {
            name: "native-engine".to_string(),
            status: DoctorCheckStatus::Ready,
            required: true,
            detail: "Déjà native engine ready".to_string(),
            remediation: None,
        }],
    })
}

// ===========================================================================
// 1. Subprocess CLI tests (`CARGO_BIN_EXE_deja`)
// ===========================================================================

#[test]
fn subprocess_version_help_guide_engine_install_and_usage_errors() {
    let ver = deja_bin().arg("--version").output().unwrap();
    assert_eq!(ver.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&ver.stdout).contains(env!("CARGO_PKG_VERSION")));

    let help = deja_bin().arg("--help").output().unwrap();
    assert_eq!(help.status.code(), Some(0));
    let help_text = String::from_utf8_lossy(&help.stdout);
    for cmd in [
        "run", "remember", "list", "inspect", "manifest", "gate", "doctor", "migrate", "guide",
    ] {
        assert!(help_text.contains(cmd), "help missing subcommand `{cmd}`");
    }

    let no_cmd = deja_bin().output().unwrap();
    assert_eq!(no_cmd.status.code(), Some(2));

    let guide = deja_bin().arg("guide").output().unwrap();
    assert_eq!(guide.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(guide.stdout).unwrap(),
        EMBEDDED_AGENT_GUIDE_MD
    );

    let eng = deja_bin().args(["engine", "install"]).output().unwrap();
    assert_eq!(eng.status.code(), Some(2));
    let eng_err = String::from_utf8_lossy(&eng.stderr);
    assert!(eng_err.contains("Déjà executes flows natively; Maestro is no longer required"));
    assert!(eng_err.contains("deja doctor --json"));

    let eng_json = deja_bin()
        .args(["engine", "install", "--json"])
        .output()
        .unwrap();
    assert_eq!(eng_json.status.code(), Some(2));
    let parsed: Value = serde_json::from_slice(&eng_json.stdout).unwrap();
    assert_eq!(
        parsed["error"]["message"],
        "Déjà executes flows natively; Maestro is no longer required"
    );

    // Reject `--flow` on `run` and `--verify` on `remember` with exit 2.
    let bad_run = deja_bin()
        .args(["run", "checkout", "--flow", "a.yaml"])
        .output()
        .unwrap();
    assert_eq!(bad_run.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad_run.stderr).contains("--flow"));

    let bad_rem = deja_bin()
        .args(["remember", "checkout", "--flow", "a.yaml", "--verify"])
        .output()
        .unwrap();
    assert_eq!(bad_rem.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad_rem.stderr).contains("--verify"));
}

#[test]
fn subprocess_capability_scoping_device_precedence_and_legacy_config_checks() {
    let ws = TempWs::new("cap-legacy");
    // Config WITHOUT buildPath or device (`DEJA-47`).
    ws.write(
        "deja.config.json",
        r#"{"workspaceRoot":".","platform":"ios","appId":"dev.deja.test.app","maestro":{"timeoutMs":60000}}"#,
    );

    // `list` succeeds without buildPath/device and emits `maestro.timeoutMs` deprecation warning.
    let list = deja_bin()
        .current_dir(&ws.path)
        .args(["list", "--json"])
        .output()
        .unwrap();
    assert_eq!(list.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&list.stderr).contains("maestro.timeoutMs"),
        "expected deprecation warning on stderr"
    );
    let list_json: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(list_json["walks"].as_array().unwrap().len(), 0);

    // `run` with `--` dash-goal fails on missing buildPath (exit 2), proving `--` goal parsing works!
    let dash_run = deja_bin()
        .current_dir(&ws.path)
        .args(["run", "--summary", "--", "-dash-goal"])
        .output()
        .unwrap();
    assert_eq!(dash_run.status.code(), Some(2));
    let out_str = String::from_utf8_lossy(&dash_run.stdout);
    assert!(out_str.contains("deja-summary v=1 outcome=error"));
    assert!(out_str.contains("goal=-dash-goal"));

    // `DEJA_MAESTRO` env var fails closed exit 2.
    let maestro_env = deja_bin()
        .current_dir(&ws.path)
        .env("DEJA_MAESTRO", "/usr/local/bin/maestro")
        .args(["list"])
        .output()
        .unwrap();
    assert_eq!(maestro_env.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&maestro_env.stderr).contains("no longer supported"));

    // `openClaw` config section fails closed exit 2.
    ws.write(
        "openclaw.config.json",
        r#"{"workspaceRoot":".","platform":"ios","appId":"dev.deja.test.app","openClaw":{"executable":"openclaw"}}"#,
    );
    let oc = deja_bin()
        .current_dir(&ws.path)
        .args(["--config", "openclaw.config.json", "list"])
        .output()
        .unwrap();
    assert_eq!(oc.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&oc.stderr).contains("openClaw"));

    // `DEJA-48`: empty `--device ""` and empty `DEJA_DEVICE=""` fail closed exit 2.
    ws.write_build("app.bin", b"build-v1");
    ws.write(
        "exec.config.json",
        r#"{"workspaceRoot":".","platform":"ios","appId":"dev.deja.test.app","buildPath":"app.bin","device":"11111111-2222-3333-4444-555555555555"}"#,
    );
    let empty_flag = deja_bin()
        .current_dir(&ws.path)
        .args([
            "--config",
            "exec.config.json",
            "run",
            "checkout",
            "--device",
            "   ",
        ])
        .output()
        .unwrap();
    assert_eq!(empty_flag.status.code(), Some(2));

    let empty_env = deja_bin()
        .current_dir(&ws.path)
        .env("DEJA_DEVICE", " ")
        .args(["--config", "exec.config.json", "run", "checkout"])
        .output()
        .unwrap();
    assert_eq!(empty_env.status.code(), Some(2));

    // Invalid iOS UDID via DEJA_DEVICE overrides valid config.device and fails closed exit 2.
    let bad_udid = deja_bin()
        .current_dir(&ws.path)
        .env("DEJA_DEVICE", "not-a-valid-udid")
        .args(["--config", "exec.config.json", "run", "checkout"])
        .output()
        .unwrap();
    assert_eq!(bad_udid.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad_udid.stderr).contains("UDID"));
}

#[test]
fn subprocess_manifest_check_all_statuses_untracked_and_unsafe_bidi_fix() {
    let ws = TempWs::new("manifest-check");
    ws.write(
        "deja.config.json",
        r#"{
          "workspaceRoot": ".",
          "platform": "ios",
          "appId": "dev.deja.test.app",
          "adapter": "maestro-ios/v1",
          "manifest": "deja.walks.json",
          "flowsDir": "flows"
        }"#,
    );

    // Absent manifest fails closed with exit 2.
    let absent = deja_bin()
        .current_dir(&ws.path)
        .args(["manifest", "check", "--json"])
        .output()
        .unwrap();
    assert_eq!(absent.status.code(), Some(2));

    // Create flow files.
    let ok_path = ws.write(
        "flows/ok.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Home\n",
    );
    let changed_path = ws.write(
        "flows/changed.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Checkout\n",
    );
    let assert_only_path = ws.write(
        "flows/assert_only.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Cart\n",
    );
    let mismatch_path = ws.write(
        "flows/mismatch.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Profile\n",
    );
    let bidi_path = ws.write(
        "flows/bidi.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Bidi\n",
    );
    let _untracked = ws.write(
        "flows/stray.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n",
    );

    let ok_cl = collect_flow_closure(&ws.path, &ok_path).unwrap();
    let ok_as = assertions_sha256(&ok_cl).unwrap();
    let ch_cl = collect_flow_closure(&ws.path, &changed_path).unwrap();
    let ch_as = assertions_sha256(&ch_cl).unwrap();
    let ao_cl = collect_flow_closure(&ws.path, &assert_only_path).unwrap();
    let mm_cl = collect_flow_closure(&ws.path, &mismatch_path).unwrap();
    let mm_as = assertions_sha256(&mm_cl).unwrap();
    let bi_cl = collect_flow_closure(&ws.path, &bidi_path).unwrap();
    let bi_as = assertions_sha256(&bi_cl).unwrap();

    // First test clean `ok` manifest + `--untracked` -> exit 0!
    let clean_manifest = json!({
        "manifestVersion": 1,
        "hashVersion": 1,
        "walks": [
            {
                "goal": "ok goal",
                "appId": "dev.deja.test.app",
                "platform": "ios",
                "adapter": "maestro-ios/v1",
                "flow": "flows/ok.yaml",
                "closureSha256": ok_cl.sha256,
                "assertionsSha256": ok_as
            }
        ]
    });
    ws.write(
        "deja.walks.json",
        &serde_json::to_string_pretty(&clean_manifest).unwrap(),
    );
    let ok_run = deja_bin()
        .current_dir(&ws.path)
        .args(["manifest", "check", "--untracked", "--json"])
        .output()
        .unwrap();
    assert_eq!(ok_run.status.code(), Some(0));
    let ok_json: Value = serde_json::from_slice(&ok_run.stdout).unwrap();
    assert_eq!(ok_json["ok"], true);
    let untracked = ok_json["untracked"].as_array().unwrap();
    assert!(untracked.iter().any(|v| v == "flows/stray.yaml"));

    // Now test all failing statuses + unsafe bidi goal -> exit 8!
    ws.write(
        "flows/changed.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- tapOn: Next\n- assertVisible: Different\n",
    );
    ws.write(
        "flows/bidi.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- tapOn: Changed\n- assertVisible: Bidi\n",
    );
    let bad_assert_hash = "0".repeat(64);
    let mixed_manifest = json!({
        "manifestVersion": 1,
        "hashVersion": 1,
        "walks": [
            {
                "goal": "ok goal",
                "appId": "dev.deja.test.app",
                "platform": "ios",
                "adapter": "maestro-ios/v1",
                "flow": "flows/ok.yaml",
                "closureSha256": ok_cl.sha256,
                "assertionsSha256": ok_as
            },
            {
                "goal": "changed goal",
                "appId": "dev.deja.test.app",
                "platform": "ios",
                "adapter": "maestro-ios/v1",
                "flow": "flows/changed.yaml",
                "closureSha256": ch_cl.sha256,
                "assertionsSha256": ch_as
            },
            {
                "goal": "assert drift",
                "appId": "dev.deja.test.app",
                "platform": "ios",
                "adapter": "maestro-ios/v1",
                "flow": "flows/assert_only.yaml",
                "closureSha256": ao_cl.sha256,
                "assertionsSha256": bad_assert_hash
            },
            {
                "goal": "other platform only",
                "appId": "dev.deja.test.app",
                "platform": "android",
                "adapter": "maestro-android/v1",
                "flow": "flows/mismatch.yaml",
                "closureSha256": mm_cl.sha256,
                "assertionsSha256": mm_as
            },
            {
                "goal": "missing flow goal",
                "appId": "dev.deja.test.app",
                "platform": "ios",
                "adapter": "maestro-ios/v1",
                "flow": "flows/does_not_exist.yaml",
                "closureSha256": ok_cl.sha256,
                "assertionsSha256": ok_as
            },
            {
                "goal": "bidi\u{202e}goal",
                "appId": "dev.deja.test.app",
                "platform": "ios",
                "adapter": "maestro-ios/v1",
                "flow": "flows/bidi.yaml",
                "closureSha256": bi_cl.sha256,
                "assertionsSha256": bi_as
            }
        ]
    });
    ws.write(
        "deja.walks.json",
        &serde_json::to_string_pretty(&mixed_manifest).unwrap(),
    );

    let check = deja_bin()
        .current_dir(&ws.path)
        .args(["manifest", "check", "--json"])
        .output()
        .unwrap();
    assert_eq!(check.status.code(), Some(8));
    let parsed: Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(parsed["ok"], false);
    let entries = parsed["entries"].as_array().unwrap();

    let by_goal = |g: &str| entries.iter().find(|e| e["goal"] == g).unwrap();
    assert_eq!(by_goal("ok goal")["status"], "ok");
    assert_eq!(by_goal("changed goal")["status"], "flow-changed");
    assert_eq!(by_goal("changed goal")["assertionsAlsoChanged"], true);
    assert!(
        by_goal("changed goal")["fix"]
            .as_str()
            .unwrap()
            .contains("--verify")
    );
    assert_eq!(by_goal("assert drift")["status"], "assertions-changed");
    assert!(
        by_goal("assert drift")["fix"]
            .as_str()
            .unwrap()
            .contains("--accept-assertion-change")
    );
    assert_eq!(
        by_goal("other platform only")["status"],
        "identity-mismatch"
    );
    assert_eq!(
        by_goal("other platform only")["identity"]["field"],
        "platform"
    );
    assert_eq!(by_goal("missing flow goal")["status"], "missing-file");
    assert_eq!(by_goal("bidi\u{202e}goal")["status"], "flow-changed");
    assert!(by_goal("bidi\u{202e}goal")["fix"].is_null());
    assert_eq!(by_goal("bidi\u{202e}goal")["fixUnsafe"], true);
}

#[test]
fn subprocess_gate_multistore_runtime_and_tamper_refusal() {
    let ws = TempWs::new("gate");
    let build = ws.write_build("build/App.bin", b"binary-v1");
    let fp = fingerprint_file(&ws.path, &build).unwrap();
    ws.write(
        "deja.config.json",
        r#"{"workspaceRoot":".","platform":"ios","appId":"dev.deja.test.app","adapter":"maestro-ios/v1","buildPath":"build/App.bin"}"#,
    );
    let flow_a = ws.write(
        "flows/a.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: A\n",
    );
    let flow_b = ws.write(
        "flows/b.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: B\n",
    );

    // Seed goal A in store `sim-1` on `ios-27.2`, and goal B in store `sim-2` on `ios-27.2`.
    let id_a = WalkIdentity {
        goal: "goal a".to_string(),
        app_id: TEST_APP_ID.to_string(),
        platform: "ios".to_string(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint: fp.clone(),
        adapter: Some("maestro-ios/v1".to_string()),
        browser: None,
    };
    let id_b = WalkIdentity {
        goal: "goal b".to_string(),
        ..id_a.clone()
    };

    let s1 =
        SqliteWalkStore::open_writable(SqliteStoreOptions::new(&ws.path).with_store_id("sim-1"))
            .unwrap();
    s1.persist_verified_candidate(
        &id_a,
        &deja_store::PersistVerifiedCandidateOptions {
            candidate_path: flow_a,
            validation: RecordValidationInput::new(true, now_iso8601())
                .with_origin(WalkOrigin::Remember)
                .with_runtime("ios-27.2"),
            expected_flow_sha256: None,
            expected_closure_sha256: None,
            exploration: None,
            repair_history: None,
        },
    )
    .unwrap();
    s1.close();

    let s2 =
        SqliteWalkStore::open_writable(SqliteStoreOptions::new(&ws.path).with_store_id("sim-2"))
            .unwrap();
    s2.persist_verified_candidate(
        &id_b,
        &deja_store::PersistVerifiedCandidateOptions {
            candidate_path: flow_b,
            validation: RecordValidationInput::new(true, now_iso8601())
                .with_origin(WalkOrigin::Remember)
                .with_runtime("ios-27.2"),
            expected_flow_sha256: None,
            expected_closure_sha256: None,
            exploration: None,
            repair_history: None,
        },
    )
    .unwrap();
    s2.close();

    ws.write("goals.txt", "# release goals\ngoal a\ngoal b\n");

    // Multi-store gate with matching runtime -> exit 0!
    let ev_path = ws.path.join(".deja/gate/release.json");
    let pass = deja_bin()
        .current_dir(&ws.path)
        .args([
            "gate",
            "--goals",
            "goals.txt",
            "--store",
            "sim-1",
            "--store",
            "sim-2",
            "--runtime",
            "ios-27.2",
            "--label",
            "deadbeef",
            "--evidence",
            ev_path.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(pass.status.code(), Some(0));
    let pass_json: Value = serde_json::from_slice(&pass.stdout).unwrap();
    assert_eq!(pass_json["verdict"], "passed");
    assert_eq!(pass_json["kind"], "deja-gate-evidence");
    assert!(ev_path.is_file());
    assert!(ws.path.join(".deja/gate/release.txt").is_file());

    // Re-running gate over valid previous evidence + untouched `.txt` succeeds!
    let rerun = deja_bin()
        .current_dir(&ws.path)
        .args([
            "gate",
            "--goals",
            "goals.txt",
            "--store",
            "sim-1",
            "--store",
            "sim-2",
            "--runtime",
            "ios-27.2",
            "--evidence",
            ev_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(rerun.status.code(), Some(0));

    // Tampering with sibling `.txt` makes gate refuse to clobber it -> exit 2!
    fs::write(ws.path.join(".deja/gate/release.txt"), "tampered").unwrap();
    let tampered_txt = deja_bin()
        .current_dir(&ws.path)
        .args([
            "gate",
            "--goals",
            "goals.txt",
            "--store",
            "sim-1",
            "--store",
            "sim-2",
            "--evidence",
            ev_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(tampered_txt.status.code(), Some(2));
    assert_eq!(
        fs::read_to_string(ws.path.join(".deja/gate/release.txt")).unwrap(),
        "tampered"
    );

    // Refuses writing inside `.deja/` outside `.deja/gate/` -> exit 2!
    let bad_dir = deja_bin()
        .current_dir(&ws.path)
        .args([
            "gate",
            "--goals",
            "goals.txt",
            "--store",
            "sim-1",
            "--evidence",
            ".deja/memory/evil.json",
        ])
        .output()
        .unwrap();
    assert_eq!(bad_dir.status.code(), Some(2));

    // Refuses clobbering unrelated JSON file -> exit 2!
    ws.write("unrelated.json", r#"{"hello":"world"}"#);
    let unrelated = deja_bin()
        .current_dir(&ws.path)
        .args([
            "gate",
            "--goals",
            "goals.txt",
            "--store",
            "sim-1",
            "--evidence",
            "unrelated.json",
        ])
        .output()
        .unwrap();
    assert_eq!(unrelated.status.code(), Some(2));

    // Runtime mismatch (`ios-18.0`) -> gate closed (exit 10)!
    let closed = deja_bin()
        .current_dir(&ws.path)
        .args([
            "gate",
            "--goals",
            "goals.txt",
            "--store",
            "sim-1",
            "--store",
            "sim-2",
            "--runtime",
            "ios-18.0",
            "--evidence",
            ".deja/gate/closed.json",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(closed.status.code(), Some(10));
    let closed_json: Value = serde_json::from_slice(&closed.stdout).unwrap();
    assert_eq!(closed_json["verdict"], "closed");

    // Corrupt SQLite store -> exit 11 (`verdict == "error"`)!
    ws.write(
        ".deja/memory/stores/corrupt/walks.sqlite",
        "not a sqlite db",
    );
    let err_gate = deja_bin()
        .current_dir(&ws.path)
        .args([
            "gate",
            "--goals",
            "goals.txt",
            "--store",
            "corrupt",
            "--evidence",
            ".deja/gate/err.json",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(err_gate.status.code(), Some(11));
    let err_json: Value = serde_json::from_slice(&err_gate.stdout).unwrap();
    assert_eq!(err_json["verdict"], "error");
}

#[test]
fn subprocess_migrate_dry_run_real_import_list_and_inspect() {
    let ws = TempWs::new("migrate-inspect");
    ws.write(
        "deja.config.json",
        r#"{"workspaceRoot":".","platform":"ios","appId":"dev.deja.test.app","adapter":"maestro-ios/v1"}"#,
    );
    let flow_sha = "a".repeat(64);
    let build_fp = "b".repeat(64);
    let legacy_json = json!({
        "schemaVersion": 3,
        "walks": [{
            "goal": "Complete Checkout",
            "goalKey": "complete checkout",
            "appId": "dev.deja.test.app",
            "platform": "ios",
            "flowSchemaVersion": "maestro/v1",
            "buildFingerprint": build_fp,
            "adapter": "maestro-ios/v1",
            "flowPath": ".deja/flows/complete-checkout/ios-aaa.yaml",
            "flowSha256": flow_sha,
            "subflowFiles": [],
            "lastValidation": { "passed": true, "at": "2026-01-01T00:00:00Z" },
            "validationHistory": [{ "passed": true, "at": "2026-01-01T00:00:00Z", "flowSha256": flow_sha }],
            "origin": "remember"
        }]
    });
    let legacy_text = serde_json::to_string_pretty(&legacy_json).unwrap();
    let legacy_path = ws.write(".deja/memory/walks.json", &legacy_text);

    // `--dry-run` does not create SQLite or alter `walks.json`.
    let dry = deja_bin()
        .current_dir(&ws.path)
        .args(["migrate", "--dry-run", "--json"])
        .output()
        .unwrap();
    assert_eq!(dry.status.code(), Some(0));
    let dry_json: Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(dry_json["dryRun"], true);
    assert_eq!(dry_json["walksToImport"], 1);
    assert!(!ws.path.join(".deja/memory/walks.sqlite").exists());

    // Real migrate imports into SQLite v5 without modifying `walks.json`.
    let mig = deja_bin()
        .current_dir(&ws.path)
        .args(["migrate", "--json"])
        .output()
        .unwrap();
    assert_eq!(mig.status.code(), Some(0));
    let mig_json: Value = serde_json::from_slice(&mig.stdout).unwrap();
    assert_eq!(mig_json["importedWalks"], 1);
    assert_eq!(fs::read_to_string(&legacy_path).unwrap(), legacy_text);

    // `list` sees imported walk; `inspect <goal>` returns walk + validationRuns.
    let list = deja_bin()
        .current_dir(&ws.path)
        .args(["list", "--json"])
        .output()
        .unwrap();
    assert_eq!(list.status.code(), Some(0));
    let list_json: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(list_json["walks"][0]["goalKey"], "complete checkout");

    let insp = deja_bin()
        .current_dir(&ws.path)
        .args(["inspect", "Complete Checkout", "--json"])
        .output()
        .unwrap();
    assert_eq!(insp.status.code(), Some(0));
    let insp_json: Value = serde_json::from_slice(&insp.stdout).unwrap();
    assert_eq!(insp_json["walk"]["goalKey"], "complete checkout");
    assert_eq!(insp_json["validationRuns"].as_array().unwrap().len(), 1);
}

// ===========================================================================
// 2. In-process mock-driver tests (`run_cli_from_args` + `CliDeps`)
// ===========================================================================

#[tokio::test]
async fn remember_and_run_hit_timing_reports_junit_summary_and_per_store_isolation() {
    let ws = TempWs::new("remember-run");
    ws.write_build("build/App.bin", b"v1-bytes");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "manifest": "deja.walks.json",
            "fixtureCredentials": [
                { "label": "Secret", "value": "fixture-secret-value" }
            ]
        })
        .to_string(),
    );
    ws.write(
        "flows/checkout.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Order confirmed\n",
    );

    let ok_tree = hierarchy_with_nodes(
        TEST_APP_ID,
        vec![node_with_label("order_confirmation", "Order confirmed")],
    );
    let driver = MockDriver::new(vec![ok_tree.clone()]);
    let shutdowns = Arc::clone(&driver.shutdown_count);
    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), driver));

    // 1. `remember` passes, writes `deja.walks.json`, SQLite, per-store latest, `--report`, `--junit`, `--summary`.
    let (deps, cap) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    let code = run_cli_from_args(
        [
            "deja",
            "remember",
            "complete checkout",
            "--flow",
            "flows/checkout.yaml",
            "--store",
            "sim-1",
            "--report",
            "out/rem.json",
            "--junit",
            "out/rem.xml",
            "--summary",
            "--json",
        ],
        &deps,
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", cap.lock().unwrap().stderr);
    assert_eq!(shutdowns.load(Ordering::SeqCst), 1);

    let out = cap.lock().unwrap().clone();
    let rem_json: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(rem_json["outcome"], "remembered");
    assert_eq!(rem_json["modelCalls"], 0);
    assert_eq!(rem_json["runtime"], "ios-27.2");
    assert_eq!(rem_json["engine"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(rem_json["timing"]["totalMs"].is_u64());
    assert!(out.stderr.contains("deja-summary v=1 outcome=remembered"));
    assert!(out.stderr.contains("goal=complete%20checkout"));

    // Per-store latest written under `.deja/reports/stores/sim-1/latest.json`, NOT default latest!
    assert!(
        ws.path
            .join(".deja/reports/stores/sim-1/latest.json")
            .is_file()
    );
    assert!(!ws.path.join(".deja/reports/latest.json").exists());
    assert!(ws.path.join("out/rem.json").is_file());
    assert!(ws.path.join("out/rem.xml").is_file());
    assert!(ws.path.join("deja.walks.json").is_file());
    // Lock file released cleanly.
    assert!(
        !ws.path
            .join(".deja/memory/stores/sim-1/walks.sqlite.lock")
            .exists()
    );

    // Verify `phase_timings_json` persisted in SQLite `validation_runs`.
    let ro =
        SqliteWalkStore::open_readonly(SqliteStoreOptions::new(&ws.path).with_store_id("sim-1"))
            .unwrap();
    let walks = ro.list_walks().unwrap();
    assert_eq!(walks.len(), 1);
    let runs = ro.list_validation_runs(&walks[0].identity()).unwrap();
    assert_eq!(runs.len(), 1);
    assert!(runs[0].phase_timings_json.is_some());
    assert_eq!(runs[0].runtime.as_deref(), Some("ios-27.2"));

    // 2. `run` hits cache cleanly on same store + runtime.
    let (deps2, cap2) = make_deps(
        &ws.path,
        factory,
        default_probes(),
        CancellationToken::new(),
    );
    let run_code = run_cli_from_args(
        [
            "deja",
            "run",
            "complete checkout",
            "--store",
            "sim-1",
            "--summary",
            "--json",
        ],
        &deps2,
    )
    .await;
    assert_eq!(run_code, 0);
    let run_out = cap2.lock().unwrap().clone();
    let run_json: Value = serde_json::from_str(&run_out.stdout).unwrap();
    assert_eq!(run_json["outcome"], "passed");
    assert_eq!(run_json["cacheStatus"], "hit");
    assert!(run_out.stderr.contains("outcome=passed cache=hit"));
    assert_eq!(shutdowns.load(Ordering::SeqCst), 2);

    // 3. `inspect` of saved report preserves exit 0 and emits summary.
    let (deps3, cap3) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(Some("ios-27.2"), MockDriver::new(vec![]))),
        default_probes(),
        CancellationToken::new(),
    );
    let insp_code = run_cli_from_args(
        ["deja", "inspect", "out/rem.json", "--summary", "--json"],
        &deps3,
    )
    .await;
    assert_eq!(insp_code, 0);
    assert!(cap3.lock().unwrap().stderr.contains("outcome=remembered"));
}

#[tokio::test]
async fn remember_rejects_app_id_escape_forbidden_secret_and_mid_run_drift() {
    let ws = TempWs::new("remember-guards");
    ws.write_build("build/App.bin", b"v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "manifest": "deja.walks.json",
            "fixtureCredentials": [
                { "label": "Fixture PW", "value": "super-secret-fixture" }
            ]
        })
        .to_string(),
    );

    // 1. Wrong appId in nested step rejected with exit 2 and ZERO driver creation.
    ws.write(
        "flows/evil_app.yaml",
        "appId: dev.deja.test.app\n---\n- repeat:\n    times: 1\n    commands:\n      - killApp: com.apple.Preferences\n",
    );
    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), MockDriver::new(vec![])));
    let (deps, _) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    let code = run_cli_from_args(
        ["deja", "remember", "evil", "--flow", "flows/evil_app.yaml"],
        &deps,
    )
    .await;
    assert_eq!(code, 2);
    assert_eq!(factory.create_calls.load(Ordering::SeqCst), 0);

    // 2. Hardcoded fixture secret in manifest mode rejected with exit 2 and ZERO driver creation.
    ws.write(
        "flows/secret.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- inputText: super-secret-fixture\n",
    );
    let (deps2, cap2) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    let code2 = run_cli_from_args(
        ["deja", "remember", "sec", "--flow", "flows/secret.yaml"],
        &deps2,
    )
    .await;
    assert_eq!(code2, 2);
    assert_eq!(factory.create_calls.load(Ordering::SeqCst), 0);
    assert!(!cap2.lock().unwrap().stderr.contains("super-secret-fixture"));

    // 3. Mid-run drift: flow file changes while executing -> exit 6 (`flow-changed`), nothing saved!
    let drift_file = ws.write(
        "flows/drift.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Ready\n",
    );
    let drift_file_clone = drift_file.clone();
    let drift_driver = MockDriver::new(vec![hierarchy_with_nodes(
        TEST_APP_ID,
        vec![node_with_label("ready", "Ready")],
    )])
    .with_on_launch(Arc::new(move |_| {
        fs::write(
            &drift_file_clone,
            "appId: dev.deja.test.app\n---\n- launchApp\n- tapOn: Sneaky\n- assertVisible: Ready\n",
        )
        .unwrap();
    }));
    let drift_factory = Arc::new(MockFactory::new(Some("ios-27.2"), drift_driver));
    let (deps3, cap3) = make_deps(
        &ws.path,
        drift_factory,
        default_probes(),
        CancellationToken::new(),
    );
    let code3 = run_cli_from_args(
        [
            "deja",
            "remember",
            "drift",
            "--flow",
            "flows/drift.yaml",
            "--json",
        ],
        &deps3,
    )
    .await;
    assert_eq!(code3, 6);
    let out3: Value = serde_json::from_str(&cap3.lock().unwrap().stdout).unwrap();
    assert_eq!(out3["outcome"], "flow-changed");
    assert!(!ws.path.join("deja.walks.json").exists());
}

#[tokio::test]
async fn run_flaky_retry_persists_flaky_failure_and_redacts_visible_context_and_junit() {
    let ws = TempWs::new("flaky-redact");
    let build = ws.write_build("build/App.bin", b"v1");
    let fp = fingerprint_file(&ws.path, &build).unwrap();
    let fake_provider_key = ["sk", "-1234567890abcdefghijklmnop"].concat();
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "fixtureCredentials": [
                { "label": "Secret", "value": "fixture-pw-999" }
            ]
        })
        .to_string(),
    );
    let flow = ws.write(
        "flows/checkout.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- tapOn:\n    id: buy_btn\n- assertVisible: Done\n",
    );

    // Seed passing walk into classic store.
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(&ws.path)).unwrap();
    let id = WalkIdentity {
        goal: "checkout".to_string(),
        app_id: TEST_APP_ID.to_string(),
        platform: "ios".to_string(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint: fp,
        adapter: Some("maestro-ios/v1".to_string()),
        browser: None,
    };
    store
        .persist_verified_candidate(
            &id,
            &deja_store::PersistVerifiedCandidateOptions {
                candidate_path: flow,
                validation: RecordValidationInput::new(true, now_iso8601())
                    .with_origin(WalkOrigin::Remember)
                    .with_runtime("ios-27.2"),
                expected_flow_sha256: None,
                expected_closure_sha256: None,
                exploration: None,
                repair_history: None,
            },
        )
        .unwrap();
    store.close();

    // Attempt 1 sees a screen WITHOUT `buy_btn` (so action fail-fast settles in 3 polls!)
    // containing BOTH a configured fixture value and an OpenAI-shaped provider key!
    // Attempt 2 sees `buy_btn` + `Done` and PASSES -> Flaky (exit 7)!
    let miss_tree = hierarchy_with_nodes(
        TEST_APP_ID,
        vec![node_with_label(
            "secret_field",
            &format!("pw=fixture-pw-999 key={fake_provider_key}"),
        )],
    );
    let hit_tree = hierarchy_with_nodes(
        TEST_APP_ID,
        vec![
            node_with_label("buy_btn", "Buy"),
            node_with_label("done", "Done"),
        ],
    );
    let driver = MockDriver::new(vec![
        miss_tree.clone(),
        miss_tree.clone(),
        miss_tree.clone(),
        hit_tree,
    ]);
    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), driver));

    let (deps, cap) = make_deps(
        &ws.path,
        factory,
        default_probes(),
        CancellationToken::new(),
    );
    let code = run_cli_from_args(
        [
            "deja",
            "run",
            "checkout",
            "--rerun-failed",
            "--report",
            "out/flaky.json",
            "--junit",
            "out/flaky.xml",
            "--summary",
            "--json",
        ],
        &deps,
    )
    .await;
    assert_eq!(code, 7, "flaky run must exit 7");

    let out = cap.lock().unwrap().clone();
    assert!(!out.stdout.contains("fixture-pw-999"));
    assert!(!out.stdout.contains(&fake_provider_key));
    assert!(!out.stderr.contains("fixture-pw-999"));
    assert!(!out.stderr.contains(&fake_provider_key));

    let rep: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(rep["outcome"], "flaky");
    assert_eq!(rep["passed"], false);
    assert_eq!(rep["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(rep["attempts"][0]["passed"], false);
    assert_eq!(rep["attempts"][1]["passed"], true);
    assert!(
        rep["visibleContext"]["items"].as_array().unwrap()[0]
            .as_str()
            .unwrap()
            .contains("<redacted-fixture-value>")
    );
    assert!(out.stderr.contains("outcome=flaky"));
    assert!(
        out.stderr
            .contains("attempts=2 attempt1=selector attempt2=none")
    );

    // Redacted multi-attempt JUnit has 2 cases and zero leaked secrets.
    let junit = fs::read_to_string(ws.path.join("out/flaky.xml")).unwrap();
    assert!(junit.contains("tests=\"2\""));
    assert!(junit.contains("failures=\"1\""));
    assert!(!junit.contains("fixture-pw-999"));
    assert!(!junit.contains(&fake_provider_key));

    // SQLite MUST record attempt 1 failure with `flaky: true`, NEVER a passing run!
    let ro = SqliteWalkStore::open_readonly(SqliteStoreOptions::new(&ws.path)).unwrap();
    let runs = ro.list_validation_runs(&id).unwrap();
    assert_eq!(runs.len(), 1);
    assert!(!runs[0].passed);
    assert!(runs[0].flaky);

    // Inspecting the saved flaky report preserves exit 7!
    let (deps_insp, _) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(None, MockDriver::new(vec![]))),
        default_probes(),
        CancellationToken::new(),
    );
    let insp_code =
        run_cli_from_args(["deja", "inspect", "out/flaky.json", "--json"], &deps_insp).await;
    assert_eq!(insp_code, 7);
}

#[tokio::test]
async fn run_crash_precedence_and_cancellation_guarantees_shutdown_and_lock_release() {
    let ws = TempWs::new("crash-cancel");
    let build = ws.write_build("build/App.bin", b"v1");
    let fp = fingerprint_file(&ws.path, &build).unwrap();
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID
        })
        .to_string(),
    );
    let flow = ws.write(
        "flows/crash.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- tapOn:\n    id: missing_btn\n",
    );

    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(&ws.path)).unwrap();
    let id = WalkIdentity {
        goal: "crash test".to_string(),
        app_id: TEST_APP_ID.to_string(),
        platform: "ios".to_string(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint: fp,
        adapter: Some("maestro-ios/v1".to_string()),
        browser: None,
    };
    store
        .persist_verified_candidate(
            &id,
            &deja_store::PersistVerifiedCandidateOptions {
                candidate_path: flow,
                validation: RecordValidationInput::new(true, now_iso8601())
                    .with_origin(WalkOrigin::Remember)
                    .with_runtime("ios-27.2"),
                expected_flow_sha256: None,
                expected_closure_sha256: None,
                exploration: None,
                repair_history: None,
            },
        )
        .unwrap();
    store.close();

    // 1. `.ips` crash precedence beats `selector` failure.
    let empty_tree = hierarchy_with_nodes(TEST_APP_ID, vec![]);
    let crash_driver = MockDriver::new(vec![empty_tree.clone(), empty_tree.clone(), empty_tree])
        .with_crash(DriverCrashSignal {
            source: CrashSignalSource::CrashReport,
            report_file: Some("DejaFixture-2026-01-01.ips".to_string()),
            process: Some("DejaFixture".to_string()),
            detail: Some("EXC_BAD_ACCESS".to_string()),
        });
    let (deps, cap) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(Some("ios-27.2"), crash_driver)),
        default_probes(),
        CancellationToken::new(),
    );
    let code = run_cli_from_args(["deja", "run", "crash test", "--json"], &deps).await;
    assert_eq!(code, 1);
    let rep: Value = serde_json::from_str(&cap.lock().unwrap().stdout).unwrap();
    assert_eq!(rep["failure"]["category"], "app_crash");
    assert_eq!(rep["failure"]["repairEligible"], false);
    assert_eq!(
        rep["failure"]["crash"]["reportFile"],
        "DejaFixture-2026-01-01.ips"
    );

    // 2. CancellationToken mid-run guarantees `driver.shutdown()` and `WriterLock` release!
    let cancel = CancellationToken::new();
    let cancel_trigger = cancel.clone();
    let cancel_driver = MockDriver::new(vec![]).with_on_launch(Arc::new(move |_| {
        cancel_trigger.cancel();
    }));
    let shutdowns = Arc::clone(&cancel_driver.shutdown_count);
    let (deps_c, _) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(Some("ios-27.2"), cancel_driver)),
        default_probes(),
        cancel,
    );
    let cancel_code = run_cli_from_args(
        [
            "deja",
            "remember",
            "cancel goal",
            "--flow",
            "flows/crash.yaml",
        ],
        &deps_c,
    )
    .await;
    assert_eq!(cancel_code, 1);
    assert_eq!(
        shutdowns.load(Ordering::SeqCst),
        1,
        "driver.shutdown() must run on cancellation"
    );
    assert!(
        !ws.path.join(".deja/memory/walks.sqlite.lock").exists(),
        "WriterLock must be released on cancellation"
    );
}

#[tokio::test]
async fn doctor_emits_remediation_without_java_or_maestro_checks() {
    let ws = TempWs::new("doctor");
    ws.write(
        "deja.config.json",
        r#"{"workspaceRoot":".","platform":"ios","appId":"dev.deja.test.app"}"#,
    );
    let probes = Arc::new(MockDoctorProbes {
        checks: vec![
            DoctorCheck {
                name: "native-engine".to_string(),
                status: DoctorCheckStatus::Ready,
                required: true,
                detail: "Déjà native Rust engine".to_string(),
                remediation: None,
            },
            DoctorCheck {
                name: "ios-simulators".to_string(),
                status: DoctorCheckStatus::NotReady,
                required: true,
                detail: "No iOS runtimes found".to_string(),
                remediation: Some("Run: xcodebuild -downloadPlatform iOS".to_string()),
            },
        ],
    });
    let (deps, cap) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(None, MockDriver::new(vec![]))),
        probes,
        CancellationToken::new(),
    );
    let code = run_cli_from_args(["deja", "doctor", "--json"], &deps).await;
    assert_eq!(code, 1);
    let out = cap.lock().unwrap().stdout.clone();
    let parsed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["ready"], false);
    assert_eq!(parsed["engine"]["native"], true);
    for check in parsed["checks"].as_array().unwrap() {
        let name = check["name"].as_str().unwrap().to_ascii_lowercase();
        assert!(!name.contains("java") && !name.contains("jdk") && !name.contains("maestro"));
        if check["status"] != "ready" {
            assert!(
                check["remediation"].as_str().is_some_and(|s| !s.is_empty()),
                "every non-ready doctor check must carry actionable remediation"
            );
        }
    }
}

#[tokio::test]
async fn manifest_verify_assertion_change_stale_build_and_retry_edge_cases() {
    let ws = TempWs::new("verify-stale-retry");
    ws.write_build("build/App.bin", b"build-v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "manifest": "deja.walks.json",
            "staleBuild": "replay-last-verified"
        })
        .to_string(),
    );
    ws.write(
        "flows/login.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- tapOn:\n    id: login_btn\n- assertVisible: Welcome\n",
    );

    let ok_tree = hierarchy_with_nodes(
        TEST_APP_ID,
        vec![
            node_with_label("login_btn", "Log in"),
            node_with_label("welcome", "Welcome"),
        ],
    );
    let factory = Arc::new(MockFactory::new(
        Some("ios-27.2"),
        MockDriver::new(vec![ok_tree.clone()]),
    ));

    // 1. Initial `remember` succeeds.
    let (deps, _) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    let code = run_cli_from_args(
        ["deja", "remember", "login", "--flow", "flows/login.yaml"],
        &deps,
    )
    .await;
    assert_eq!(code, 0);

    // 2. New build with unchanged flow triggers `staleBuild: replay-last-verified` in-place replay!
    ws.write_build("build/App.bin", b"build-v2");
    let (deps_sb, cap_sb) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    let sb_code = run_cli_from_args(["deja", "run", "login", "--json"], &deps_sb).await;
    assert_eq!(sb_code, 0);
    let sb_json: Value = serde_json::from_str(&cap_sb.lock().unwrap().stdout).unwrap();
    assert_eq!(sb_json["outcome"], "passed");
    assert_eq!(
        sb_json["replayedOnNewBuild"]["policy"],
        "replay-last-verified"
    );
    assert_eq!(sb_json["replayedOnNewBuild"]["recordedForNewBuild"], true);

    // 3. Action-only edit -> normal `run` exits 6 (`flow-changed`), `run --verify` passes & updates manifest!
    ws.write(
        "flows/login.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- tapOn:\n    id: login_btn\n- hideKeyboard\n- assertVisible: Welcome\n",
    );
    let (deps_fc, cap_fc) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(["deja", "run", "login", "--json"], &deps_fc).await,
        6
    );
    let fc_json: Value = serde_json::from_str(&cap_fc.lock().unwrap().stdout).unwrap();
    assert_eq!(fc_json["outcome"], "flow-changed");

    let (deps_v, cap_v) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(["deja", "run", "login", "--verify", "--json"], &deps_v).await,
        0
    );
    let v_json: Value = serde_json::from_str(&cap_v.lock().unwrap().stdout).unwrap();
    assert_eq!(v_json["outcome"], "passed");

    // 4. Assertion edit -> both `run --verify` and `remember` without `--accept-assertion-change` exit 6!
    ws.write(
        "flows/login.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- tapOn:\n    id: login_btn\n- assertVisible: Dashboard\n",
    );
    let (deps_ac1, cap_ac1) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(["deja", "run", "login", "--verify", "--json"], &deps_ac1).await,
        6
    );
    assert_eq!(
        serde_json::from_str::<Value>(&cap_ac1.lock().unwrap().stdout).unwrap()["outcome"],
        "assertions-changed"
    );

    let (deps_ac2, _) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            ["deja", "remember", "login", "--flow", "flows/login.yaml"],
            &deps_ac2,
        )
        .await,
        6
    );

    // With `--accept-assertion-change` + matching UI -> passes and updates manifest!
    let dash_tree = hierarchy_with_nodes(
        TEST_APP_ID,
        vec![
            node_with_label("login_btn", "Log in"),
            node_with_label("dash", "Dashboard"),
        ],
    );
    let dash_factory = Arc::new(MockFactory::new(
        Some("ios-27.2"),
        MockDriver::new(vec![dash_tree]),
    ));
    let (deps_ac3, _) = make_deps(
        &ws.path,
        dash_factory,
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            [
                "deja",
                "remember",
                "login",
                "--flow",
                "flows/login.yaml",
                "--accept-assertion-change",
            ],
            &deps_ac3,
        )
        .await,
        0
    );

    // 5. Consistent failure across `--rerun-failed` stays exit 1 (`flaky: false`)!
    let miss = hierarchy_with_nodes(TEST_APP_ID, vec![node_with_label("other", "Other")]);
    let fail_factory = Arc::new(MockFactory::new(
        Some("ios-27.2"),
        MockDriver::new(vec![miss]),
    ));
    let (deps_cf, cap_cf) = make_deps(
        &ws.path,
        fail_factory,
        default_probes(),
        CancellationToken::new(),
    );
    let cf_code = run_cli_from_args(
        ["deja", "run", "login", "--rerun-failed", "--json"],
        &deps_cf,
    )
    .await;
    assert_eq!(cf_code, 1);
    let cf_json: Value = serde_json::from_str(&cap_cf.lock().unwrap().stdout).unwrap();
    assert_eq!(cf_json["outcome"], "failed");
    assert_eq!(cf_json["attempts"].as_array().unwrap().len(), 2);

    // 6. Read-only `list` succeeds even while another writer holds `.lock` (and a second writer exits 5!).
    let held_writer = SqliteWalkStore::open_writable(SqliteStoreOptions::new(&ws.path)).unwrap();
    let (deps_ro, _) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(["deja", "list", "--json"], &deps_ro).await,
        0
    );
    let (deps_locked, _) = make_deps(
        &ws.path,
        factory,
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(["deja", "run", "login", "--json"], &deps_locked).await,
        5
    );
    held_writer.close();
}

// ===========================================================================
// 3. Codex Round 1 regression tests (Findings 1 through 9)
// ===========================================================================

#[tokio::test]
async fn regression_r1_f1_ios_bundle_validation_rejects_non_app_missing_plist_mismatch_and_accepts_match()
 {
    let ws = TempWs::new("r1-f1-ios-bundle");
    let cancel = CancellationToken::new();
    let factory = RealDriverFactory;

    let make_ctx = |build_path: PathBuf| DriverContext {
        platform: "ios".to_string(),
        device: TEST_UDID.to_string(),
        app_id: TEST_APP_ID.to_string(),
        build_path,
        expected_build_fingerprint: None,
        workspace_root: ws.path.clone(),
        disable_animations: true,
        crash_report_grace_ms: 0,
        crash_report_dir: None,
        deja_home: None,
        headless: None,
        screen_size: None,
        timeout_ms: 5_000,
        env: BTreeMap::new(),
        staged_build: None,
        resolved_runtime: None,
    };

    // 1. Plain regular file (`build/App.bin`) rejected with exit 2.
    let plain_bin = ws.write_build("build/App.bin", b"not-a-bundle");
    assert!(verify_ios_app_bundle(&plain_bin, TEST_APP_ID).is_err());
    let err_create = factory
        .create(&make_ctx(plain_bin.clone()), &cancel)
        .await
        .err()
        .expect("create must reject non-.app file");
    assert_eq!(err_create.exit_code(), 2);
    let err_install = factory
        .install_app(&make_ctx(plain_bin), &cancel)
        .await
        .expect_err("install_app must reject non-.app file");
    assert_eq!(err_install.exit_code(), 2);

    // 2. Plain file named `.app` rejected.
    let fake_file_app = ws.write_build("build/Fake.app", b"not-a-dir");
    assert!(verify_ios_app_bundle(&fake_file_app, TEST_APP_ID).is_err());

    // 3. Directory NOT ending in `.app` rejected even with matching Info.plist.
    let xml_matching = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <!-- <key>CFBundleIdentifier</key><string>com.comment.decoy</string> -->
  <key>Nested</key>
  <dict>
    <key>CFBundleIdentifier</key>
    <string>com.nested.decoy</string>
  </dict>
  <key>CFBundleIdentifier</key>
  <string>{TEST_APP_ID}</string>
</dict>
</plist>
"#
    );
    ws.write("build/NotABundle/Info.plist", &xml_matching);
    let non_app_dir = ws.path.join("build/NotABundle");
    assert!(verify_ios_app_bundle(&non_app_dir, TEST_APP_ID).is_err());

    // 4. `.app` directory missing `Info.plist` rejected.
    let missing_plist_app = ws.path.join("build/MissingPlist.app");
    fs::create_dir_all(&missing_plist_app).unwrap();
    assert!(verify_ios_app_bundle(&missing_plist_app, TEST_APP_ID).is_err());
    let err_missing = factory
        .install_app(&make_ctx(missing_plist_app), &cancel)
        .await
        .expect_err("install_app must reject missing Info.plist");
    assert_eq!(err_missing.exit_code(), 2);

    // 5. `.app` directory with mismatched `CFBundleIdentifier` rejected.
    let xml_mismatch = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key>
  <string>dev.deja.other.app</string>
</dict>
</plist>
"#;
    ws.write("build/WrongId.app/Info.plist", xml_mismatch);
    let wrong_app = ws.path.join("build/WrongId.app");
    let mismatch_err = verify_ios_app_bundle(&wrong_app, TEST_APP_ID).unwrap_err();
    assert!(mismatch_err.contains("dev.deja.other.app"));
    let err_wrong = factory
        .create(&make_ctx(wrong_app), &cancel)
        .await
        .err()
        .expect("create must reject mismatched CFBundleIdentifier");
    assert_eq!(err_wrong.exit_code(), 2);

    // 6. Valid `.app` directory with matching XML `Info.plist` (including comment + nested dict decoy) accepted.
    ws.write("build/Good.app/Info.plist", &xml_matching);
    let good_app = ws.path.join("build/Good.app");
    verify_ios_app_bundle(&good_app, TEST_APP_ID).expect("matching XML .app must pass");

    // 7. Symlinked `.app` bundle and symlinked `Info.plist` rejected on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let sym_app = ws.path.join("build/Sym.app");
        symlink(&good_app, &sym_app).unwrap();
        assert!(verify_ios_app_bundle(&sym_app, TEST_APP_ID).is_err());

        let sym_plist_app = ws.path.join("build/SymPlist.app");
        fs::create_dir_all(&sym_plist_app).unwrap();
        symlink(
            good_app.join("Info.plist"),
            sym_plist_app.join("Info.plist"),
        )
        .unwrap();
        assert!(verify_ios_app_bundle(&sym_plist_app, TEST_APP_ID).is_err());
    }

    // 8. Binary `Info.plist` validated via `plutil` on macOS (both matching and mismatched).
    #[cfg(target_os = "macos")]
    {
        let bin_good = ws.path.join("build/BinGood.app");
        fs::create_dir_all(&bin_good).unwrap();
        let bin_good_plist = bin_good.join("Info.plist");
        fs::write(&bin_good_plist, &xml_matching).unwrap();
        let st = Command::new("/usr/bin/plutil")
            .args(["-convert", "binary1"])
            .arg(&bin_good_plist)
            .status()
            .unwrap();
        assert!(st.success(), "plutil -convert binary1 failed");
        assert!(fs::read(&bin_good_plist).unwrap().starts_with(b"bplist"));
        verify_ios_app_bundle(&bin_good, TEST_APP_ID)
            .expect("matching binary Info.plist must pass");

        let bin_bad = ws.path.join("build/BinBad.app");
        fs::create_dir_all(&bin_bad).unwrap();
        let bin_bad_plist = bin_bad.join("Info.plist");
        fs::write(&bin_bad_plist, xml_mismatch).unwrap();
        let st2 = Command::new("/usr/bin/plutil")
            .args(["-convert", "binary1"])
            .arg(&bin_bad_plist)
            .status()
            .unwrap();
        assert!(st2.success());
        assert!(verify_ios_app_bundle(&bin_bad, TEST_APP_ID).is_err());
    }
}

#[tokio::test]
async fn regression_r1_f2_executes_preflight_closure_bytes_even_when_disk_mutated_during_attach() {
    let ws = TempWs::new("r1-f2-snapshot");
    ws.write_build("build/App.bin", b"v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "manifest": "deja.walks.json"
        })
        .to_string(),
    );
    let main_yaml =
        "appId: dev.deja.test.app\n---\n- launchApp\n- runFlow: sub.yaml\n- assertVisible: Ready\n";
    let sub_yaml = "appId: dev.deja.test.app\n---\n- tapOn:\n    id: go_btn\n";
    let main_path = ws.write("flows/main.yaml", main_yaml);
    let sub_path = ws.write("flows/sub.yaml", sub_yaml);

    let ok_tree = hierarchy_with_nodes(
        TEST_APP_ID,
        vec![
            node_with_label("go_btn", "Go"),
            node_with_label("ready", "Ready"),
        ],
    );

    // 1. During `remember`, mutate BOTH root and subflow on disk inside `ensure_session`
    // (after preflight closure hashing/validation, right before FlowExecutor starts).
    let mutated_main = main_path.clone();
    let mutated_sub = sub_path.clone();
    let driver =
        MockDriver::new(vec![ok_tree.clone()]).with_on_ensure_session(Arc::new(move || {
            fs::write(
            &mutated_main,
            "appId: dev.deja.test.app\n---\n- killApp: com.evil.main\n- assertVisible: NeverSeen\n",
        )
        .unwrap();
            fs::write(
                &mutated_sub,
                "appId: dev.deja.test.app\n---\n- killApp: com.evil.sub\n",
            )
            .unwrap();
        }));
    let killed = Arc::clone(&driver.killed_apps);
    let (deps, cap) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(Some("ios-27.2"), driver)),
        default_probes(),
        CancellationToken::new(),
    );
    let code = run_cli_from_args(
        [
            "deja",
            "remember",
            "snapshot goal",
            "--flow",
            "flows/main.yaml",
            "--json",
        ],
        &deps,
    )
    .await;
    // Executor MUST have run ONLY the preflight closure bytes (zero `killApp` calls),
    // and post-run drift check MUST catch the on-disk mutation and refuse persistence (exit 6).
    assert!(
        killed.lock().unwrap().is_empty(),
        "executor must never execute mutated disk bytes after preflight"
    );
    assert_eq!(
        code, 6,
        "post-run drift check must catch mutation during remember"
    );
    let rep: Value = serde_json::from_str(&cap.lock().unwrap().stdout).unwrap();
    assert_eq!(rep["outcome"], "flow-changed");
    assert!(
        !ws.path.join("deja.walks.json").exists(),
        "drifted remember must not persist manifest"
    );

    // 2. Restore original files, remember cleanly, then mutate on disk during `run` attach.
    fs::write(&main_path, main_yaml).unwrap();
    fs::write(&sub_path, sub_yaml).unwrap();
    let (deps_ok, _) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(
            Some("ios-27.2"),
            MockDriver::new(vec![ok_tree.clone()]),
        )),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            [
                "deja",
                "remember",
                "snapshot goal",
                "--flow",
                "flows/main.yaml",
            ],
            &deps_ok,
        )
        .await,
        0
    );

    let mutated_main2 = main_path.clone();
    let mutated_sub2 = sub_path.clone();
    let driver2 = MockDriver::new(vec![ok_tree]).with_on_ensure_session(Arc::new(move || {
        fs::write(
            &mutated_main2,
            "appId: dev.deja.test.app\n---\n- killApp: com.evil.run\n",
        )
        .unwrap();
        fs::write(
            &mutated_sub2,
            "appId: dev.deja.test.app\n---\n- killApp: com.evil.subrun\n",
        )
        .unwrap();
    }));
    let killed2 = Arc::clone(&driver2.killed_apps);
    let (deps_run, cap_run) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(Some("ios-27.2"), driver2)),
        default_probes(),
        CancellationToken::new(),
    );
    let run_code = run_cli_from_args(["deja", "run", "snapshot goal", "--json"], &deps_run).await;
    assert!(
        killed2.lock().unwrap().is_empty(),
        "`deja run` must execute immutable preflight closure snapshot, never post-preflight disk edits"
    );
    assert_eq!(
        run_code, 6,
        "manifest-mode `deja run` post-run drift check must refuse persistence when disk changed mid-run"
    );
    let run_rep: Value = serde_json::from_str(&cap_run.lock().unwrap().stdout).unwrap();
    assert_eq!(run_rep["outcome"], "flow-changed");
}

#[tokio::test]
async fn regression_r1_f3_gate_clears_stale_evidence_on_invalid_goals_but_protects_unrelated_files()
{
    let ws = TempWs::new("r1-f3-gate-stale");
    let build = ws.write_build("build/App.bin", b"v1");
    let fp = fingerprint_file(&ws.path, &build).unwrap();
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin"
        })
        .to_string(),
    );
    let flow = ws.write(
        "flows/checkout.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Done\n",
    );
    ws.write("goals.txt", "checkout\n");
    ws.write("empty-goals.txt", "# only comments\n   \n");

    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(&ws.path)).unwrap();
    let id = WalkIdentity {
        goal: "checkout".to_string(),
        app_id: TEST_APP_ID.to_string(),
        platform: "ios".to_string(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint: fp,
        adapter: Some("maestro-ios/v1".to_string()),
        browser: None,
    };
    store
        .persist_verified_candidate(
            &id,
            &deja_store::PersistVerifiedCandidateOptions {
                candidate_path: flow,
                validation: RecordValidationInput::new(true, now_iso8601())
                    .with_origin(WalkOrigin::Remember)
                    .with_runtime("ios-27.2"),
                expected_flow_sha256: None,
                expected_closure_sha256: None,
                exploration: None,
                repair_history: None,
            },
        )
        .unwrap();
    store.close();

    let ev_json = ws.path.join(".deja/gate/evidence.json");
    let ev_txt = ws.path.join(".deja/gate/evidence.txt");

    // 1. Produce passing evidence first.
    let (deps1, _) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(None, MockDriver::new(vec![]))),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            [
                "deja",
                "gate",
                "--goals",
                "goals.txt",
                "--evidence",
                ".deja/gate/evidence.json",
                "--json",
            ],
            &deps1,
        )
        .await,
        0
    );
    assert!(ev_json.is_file());
    assert!(ev_txt.is_file());

    // 2. Missing goals file -> exits 2 AND removes stale passing evidence `.json` + `.txt`.
    let (deps2, _) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(None, MockDriver::new(vec![]))),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            [
                "deja",
                "gate",
                "--goals",
                "does-not-exist.txt",
                "--evidence",
                ".deja/gate/evidence.json",
                "--json",
            ],
            &deps2,
        )
        .await,
        2
    );
    assert!(
        !ev_json.exists(),
        "stale evidence.json must be removed on missing goals file"
    );
    assert!(
        !ev_txt.exists(),
        "stale evidence.txt must be removed on missing goals file"
    );

    // 3. Re-create passing evidence, then run with empty goals file -> exits 2 AND removes stale evidence.
    let (deps3, _) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(None, MockDriver::new(vec![]))),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            [
                "deja",
                "gate",
                "--goals",
                "goals.txt",
                "--evidence",
                ".deja/gate/evidence.json",
                "--json",
            ],
            &deps3,
        )
        .await,
        0
    );
    assert!(ev_json.is_file() && ev_txt.is_file());
    let (deps4, _) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(None, MockDriver::new(vec![]))),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            [
                "deja",
                "gate",
                "--goals",
                "empty-goals.txt",
                "--evidence",
                ".deja/gate/evidence.json",
                "--json",
            ],
            &deps4,
        )
        .await,
        2
    );
    assert!(!ev_json.exists() && !ev_txt.exists());

    // 4. Unrelated non-evidence file is NEVER deleted even when goals file is missing.
    let unrelated = ws.write("notes/important.json", r#"{"owner":"user","keep":true}"#);
    let (deps5, _) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(None, MockDriver::new(vec![]))),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            [
                "deja",
                "gate",
                "--goals",
                "does-not-exist.txt",
                "--evidence",
                "notes/important.json",
                "--json",
            ],
            &deps5,
        )
        .await,
        2
    );
    assert_eq!(
        fs::read_to_string(&unrelated).unwrap(),
        r#"{"owner":"user","keep":true}"#
    );
}

#[test]
fn regression_r1_f4_malformed_config_json_exits_promptly_without_hanging() {
    let ws = TempWs::new("r1-f4-malformed-json");
    for malformed in [
        r#"{"platform": "ios", "arr": [1}"#,
        r#"{"platform": "ios", "arr": [1, 2 3]}"#,
        r#"{"platform": "ios", "nested": {"a": ["#,
    ] {
        ws.write("deja.config.json", malformed);
        let out = deja_bin()
            .args(["list", "--json"])
            .current_dir(&ws.path)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "malformed JSON must exit 2 promptly: {malformed}"
        );
    }
}

#[tokio::test]
async fn regression_r1_f5_multi_retry_preserves_v1_summary_grammar_while_keeping_all_attempts() {
    let ws = TempWs::new("r1-f5-multi-retry");
    let build = ws.write_build("build/App.bin", b"v1");
    let fp = fingerprint_file(&ws.path, &build).unwrap();
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID
        })
        .to_string(),
    );
    let flow = ws.write(
        "flows/checkout.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- tapOn:\n    id: buy_btn\n- assertVisible: Done\n",
    );

    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(&ws.path)).unwrap();
    let id = WalkIdentity {
        goal: "checkout".to_string(),
        app_id: TEST_APP_ID.to_string(),
        platform: "ios".to_string(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint: fp,
        adapter: Some("maestro-ios/v1".to_string()),
        browser: None,
    };
    store
        .persist_verified_candidate(
            &id,
            &deja_store::PersistVerifiedCandidateOptions {
                candidate_path: flow,
                validation: RecordValidationInput::new(true, now_iso8601())
                    .with_origin(WalkOrigin::Remember)
                    .with_runtime("ios-27.2"),
                expected_flow_sha256: None,
                expected_closure_sha256: None,
                exploration: None,
                repair_history: None,
            },
        )
        .unwrap();
    store.close();

    // 3 polls for attempt 1 (miss) + 3 polls for attempt 2 (miss) + 1 hit for attempt 3 (pass)!
    let miss_tree = hierarchy_with_nodes(TEST_APP_ID, vec![node_with_label("other", "Other")]);
    let hit_tree = hierarchy_with_nodes(
        TEST_APP_ID,
        vec![
            node_with_label("buy_btn", "Buy"),
            node_with_label("done", "Done"),
        ],
    );
    let driver = MockDriver::new(vec![
        miss_tree.clone(),
        miss_tree.clone(),
        miss_tree.clone(),
        miss_tree.clone(),
        miss_tree.clone(),
        miss_tree,
        hit_tree,
    ]);
    let (deps, cap) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(Some("ios-27.2"), driver)),
        default_probes(),
        CancellationToken::new(),
    );
    let code = run_cli_from_args(
        [
            "deja",
            "run",
            "checkout",
            "--retries",
            "2",
            "--junit",
            "out/multi.xml",
            "--summary",
            "--json",
        ],
        &deps,
    )
    .await;
    assert_eq!(code, 7, "flaky multi-retry run must exit 7");

    let out = cap.lock().unwrap().clone();
    let rep: Value = serde_json::from_str(&out.stdout).unwrap();
    let attempts = rep["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 3, "JSON report must keep all 3 attempts");
    assert_eq!(attempts[0]["passed"], false);
    assert_eq!(attempts[1]["passed"], false);
    assert_eq!(attempts[2]["passed"], true);

    // Summary line MUST obey strict `deja-summary v=1` grammar (`attempts=2`, never `attempts=3` / `attempt3=`).
    assert!(
        out.stderr
            .contains("attempts=2 attempt1=selector attempt2=none"),
        "expected strict v1 attempts=2 trailer, got: {}",
        out.stderr
    );
    assert!(!out.stderr.contains("attempts=3"));
    assert!(!out.stderr.contains("attempt3="));

    // JUnit XML must contain all 3 attempts.
    let junit = fs::read_to_string(ws.path.join("out/multi.xml")).unwrap();
    assert!(junit.contains("tests=\"3\""));
    assert!(junit.contains("failures=\"2\""));
    assert_eq!(junit.matches("<testcase ").count(), 3);
}

#[tokio::test]
async fn regression_r1_f6_crash_probe_runs_once_normally_and_falls_back_when_flow_timeout_truncates()
 {
    let ws = TempWs::new("r1-f6-crash-probe");
    let build = ws.write_build("build/App.bin", b"v1");
    let fp = fingerprint_file(&ws.path, &build).unwrap();
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "timeoutMs": 10000
        })
        .to_string(),
    );
    let flow = ws.write(
        "flows/missing.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- tapOn:\n    id: missing_btn\n",
    );

    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(&ws.path)).unwrap();
    for goal_name in ["crash probe normal", "crash probe fallback"] {
        let id = WalkIdentity {
            goal: goal_name.to_string(),
            app_id: TEST_APP_ID.to_string(),
            platform: "ios".to_string(),
            flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
            build_fingerprint: fp.clone(),
            adapter: Some("maestro-ios/v1".to_string()),
            browser: None,
        };
        store
            .persist_verified_candidate(
                &id,
                &deja_store::PersistVerifiedCandidateOptions {
                    candidate_path: flow.clone(),
                    validation: RecordValidationInput::new(true, now_iso8601())
                        .with_origin(WalkOrigin::Remember)
                        .with_runtime("ios-27.2"),
                    expected_flow_sha256: None,
                    expected_closure_sha256: None,
                    exploration: None,
                    repair_history: None,
                },
            )
            .unwrap();
    }
    store.close();

    // Case A: Normal non-crash failure -> `check_app_crash` runs ONCE (inside FlowExecutor), NOT twice.
    let empty_tree = hierarchy_with_nodes(TEST_APP_ID, vec![]);
    let driver_a = MockDriver::new(vec![empty_tree.clone()]);
    let calls_a = Arc::clone(&driver_a.crash_probe_calls);
    let (deps_a, cap_a) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(Some("ios-27.2"), driver_a)),
        default_probes(),
        CancellationToken::new(),
    );
    let code_a = run_cli_from_args(["deja", "run", "crash probe normal", "--json"], &deps_a).await;
    assert_eq!(code_a, 1);
    let rep_a: Value = serde_json::from_str(&cap_a.lock().unwrap().stdout).unwrap();
    assert_eq!(rep_a["failure"]["category"], "selector");
    assert_eq!(
        calls_a.load(Ordering::SeqCst),
        1,
        "non-crash failure must wait through only ONE crash probe"
    );

    // Case B: In-flow crash probe truncated by `flow_timeout` -> post-executor fallback runs
    // and classifies as `app_crash`.
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "timeoutMs": 1200
        })
        .to_string(),
    );
    let driver_b = MockDriver::new(vec![empty_tree])
        .with_crash_probe_delay_first(Duration::from_secs(5))
        .with_crash(DriverCrashSignal {
            source: CrashSignalSource::CrashReport,
            report_file: Some("DejaFixture-fallback.ips".to_string()),
            process: Some("DejaFixture".to_string()),
            detail: Some("SIGABRT".to_string()),
        });
    let calls_b = Arc::clone(&driver_b.crash_probe_calls);
    let (deps_b, cap_b) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(Some("ios-27.2"), driver_b)),
        default_probes(),
        CancellationToken::new(),
    );
    let code_b =
        run_cli_from_args(["deja", "run", "crash probe fallback", "--json"], &deps_b).await;
    assert_eq!(code_b, 1);
    let rep_b: Value = serde_json::from_str(&cap_b.lock().unwrap().stdout).unwrap();
    assert_eq!(rep_b["failure"]["category"], "app_crash");
    assert_eq!(
        rep_b["failure"]["crash"]["reportFile"],
        "DejaFixture-fallback.ips"
    );
    assert_eq!(
        calls_b.load(Ordering::SeqCst),
        2,
        "fallback crash probe must run when flow_timeout truncates the in-flow probe"
    );
}

#[tokio::test]
async fn regression_r1_f7_gate_links_exact_store_report_path_even_on_identical_timestamps() {
    let ws = TempWs::new("r1-f7-gate-report-path");
    let build = ws.write_build("build/App.bin", b"v1");
    let fp = fingerprint_file(&ws.path, &build).unwrap();
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin"
        })
        .to_string(),
    );
    let flow = ws.write(
        "flows/checkout.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Done\n",
    );
    ws.write("goals.txt", "checkout\n");

    let shared_ts = "2026-06-01T12:00:00Z";
    let id = WalkIdentity {
        goal: "checkout".to_string(),
        app_id: TEST_APP_ID.to_string(),
        platform: "ios".to_string(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint: fp,
        adapter: Some("maestro-ios/v1".to_string()),
        browser: None,
    };

    for (store_id, report_rel) in [
        ("shard-a", ".deja/reports/run-a.json"),
        ("shard-b", ".deja/reports/run-b.json"),
    ] {
        ws.write(
            report_rel,
            &json!({
                "identity": { "goal": "checkout" },
                "execution": { "startedAt": shared_ts, "endedAt": shared_ts }
            })
            .to_string(),
        );
        let store = SqliteWalkStore::open_writable(
            SqliteStoreOptions::new(&ws.path).with_store_id(store_id),
        )
        .unwrap();
        store
            .persist_verified_candidate(
                &id,
                &deja_store::PersistVerifiedCandidateOptions {
                    candidate_path: flow.clone(),
                    validation: RecordValidationInput::new(true, shared_ts)
                        .with_origin(WalkOrigin::Remember)
                        .with_runtime("ios-27.2")
                        .with_report_path(report_rel),
                    expected_flow_sha256: None,
                    expected_closure_sha256: None,
                    exploration: None,
                    repair_history: None,
                },
            )
            .unwrap();
        store.close();
    }

    for (store_id, expected_report) in [
        ("shard-a", ".deja/reports/run-a.json"),
        ("shard-b", ".deja/reports/run-b.json"),
    ] {
        let (deps, cap) = make_deps(
            &ws.path,
            Arc::new(MockFactory::new(None, MockDriver::new(vec![]))),
            default_probes(),
            CancellationToken::new(),
        );
        let code = run_cli_from_args(
            [
                "deja",
                "gate",
                "--goals",
                "goals.txt",
                "--store",
                store_id,
                "--json",
            ],
            &deps,
        )
        .await;
        assert_eq!(code, 0);
        let parsed: Value = serde_json::from_str(&cap.lock().unwrap().stdout).unwrap();
        assert_eq!(
            parsed["goals"][0]["reportPath"], expected_report,
            "gate must link the exact reportPath stored on shard {store_id}"
        );
    }
}

#[tokio::test]
async fn regression_r1_f8_manifest_check_fix_flow_path_works_from_parent_cwd() {
    let parent = TempWs::new("r1-f8-manifest-cwd");
    let app_dir = parent.path.join("app");
    fs::create_dir_all(&app_dir).unwrap();

    parent.write(
        "app/deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "manifest": "deja.walks.json"
        })
        .to_string(),
    );
    let assert_only_flow = parent.write(
        "app/flows/assert_only.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Original\n",
    );
    let ao_closure = collect_flow_closure(&app_dir, &assert_only_flow).unwrap();
    let both_changed_flow = parent.write(
        "app/flows/both_changed.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Original\n",
    );
    let bc_closure = collect_flow_closure(&app_dir, &both_changed_flow).unwrap();
    let bc_assert = assertions_sha256(&bc_closure).unwrap();

    parent.write(
        "app/deja.walks.json",
        &json!({
            "manifestVersion": 1,
            "hashVersion": 1,
            "walks": [
                {
                    "goal": "checkout",
                    "appId": TEST_APP_ID,
                    "platform": "ios",
                    "adapter": "maestro-ios/v1",
                    "flow": "flows/assert_only.yaml",
                    "closureSha256": ao_closure.sha256,
                    "assertionsSha256": "0".repeat(64)
                },
                {
                    "goal": "checkout edited",
                    "appId": TEST_APP_ID,
                    "platform": "ios",
                    "adapter": "maestro-ios/v1",
                    "flow": "flows/both_changed.yaml",
                    "closureSha256": bc_closure.sha256,
                    "assertionsSha256": bc_assert
                }
            ]
        })
        .to_string(),
    );

    // Edit `both_changed.yaml` assertion so `flow-changed` + `assertionsAlsoChanged: true` also triggers.
    parent.write(
        "app/flows/both_changed.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Updated\n",
    );

    // Run from `parent` (OUTSIDE `workspaceRoot = parent/app`) with `--config app/deja.config.json`.
    let (deps_json, cap_json) = make_deps(
        &parent.path,
        Arc::new(MockFactory::new(None, MockDriver::new(vec![]))),
        default_probes(),
        CancellationToken::new(),
    );
    let code = run_cli_from_args(
        [
            "deja",
            "manifest",
            "check",
            "--config",
            "app/deja.config.json",
            "--json",
        ],
        &deps_json,
    )
    .await;
    assert_eq!(code, 8);
    let parsed: Value = serde_json::from_str(&cap_json.lock().unwrap().stdout).unwrap();
    let entries = parsed["entries"].as_array().unwrap();
    assert_eq!(entries[0]["status"], "assertions-changed");
    assert_eq!(entries[0]["flow"], "flows/assert_only.yaml");
    let fix = entries[0]["fix"].as_str().unwrap();
    assert!(
        fix.contains("--flow app/flows/assert_only.yaml"),
        "fix command must resolve relative to caller cwd, got: {fix}"
    );
    assert!(parent.path.join("app/flows/assert_only.yaml").is_file());
    assert_eq!(entries[1]["status"], "flow-changed");
    assert_eq!(entries[1]["assertionsAlsoChanged"], true);

    // Human output must resolve BOTH the `assertions-changed` fix and `assertionsAlsoChanged` note to caller cwd.
    let (deps_human, cap_human) = make_deps(
        &parent.path,
        Arc::new(MockFactory::new(None, MockDriver::new(vec![]))),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            [
                "deja",
                "manifest",
                "check",
                "--config",
                "app/deja.config.json",
            ],
            &deps_human,
        )
        .await,
        8
    );
    let human_stdout = cap_human.lock().unwrap().stdout.clone();
    assert!(
        human_stdout.contains("--flow app/flows/assert_only.yaml"),
        "human assertions-changed fix must use cwd-relative flow path: {human_stdout}"
    );
    assert!(
        human_stdout.contains("--flow app/flows/both_changed.yaml"),
        "human assertionsAlsoChanged note must use cwd-relative flow path: {human_stdout}"
    );
}

#[tokio::test]
async fn regression_r1_f9_manifest_check_redacts_identity_mismatch_secrets_in_human_and_json_output()
 {
    let ws = TempWs::new("r1-f9-manifest-redact");
    let secret_val = ["top", "secret-adapter-tok-42"].concat();
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": format!("custom-{secret_val}"),
            "manifest": "deja.walks.json",
            "secrets": [secret_val]
        })
        .to_string(),
    );
    let flow = ws.write(
        "flows/checkout.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Done\n",
    );
    let closure = collect_flow_closure(&ws.path, &flow).unwrap();
    let assert_hash = assertions_sha256(&closure).unwrap();
    ws.write(
        "deja.walks.json",
        &json!({
            "manifestVersion": 1,
            "hashVersion": 1,
            "walks": [
                {
                    "goal": "checkout",
                    "appId": TEST_APP_ID,
                    "platform": "ios",
                    "adapter": "maestro-ios/v1",
                    "flow": "flows/checkout.yaml",
                    "closureSha256": closure.sha256,
                    "assertionsSha256": assert_hash
                }
            ]
        })
        .to_string(),
    );

    // 1. Human output must exit 8, omit raw secret everywhere, and show `<redacted-fixture-value>`.
    let (deps_human, cap_human) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(None, MockDriver::new(vec![]))),
        default_probes(),
        CancellationToken::new(),
    );
    let code_human = run_cli_from_args(["deja", "manifest", "check"], &deps_human).await;
    assert_eq!(code_human, 8);
    let human_out = cap_human.lock().unwrap().clone();
    assert!(!human_out.stdout.contains(&secret_val));
    assert!(!human_out.stderr.contains(&secret_val));
    assert!(
        human_out.stdout.contains("<redacted-fixture-value>"),
        "expected redacted placeholder in human mismatch output, got: {}",
        human_out.stdout
    );

    // 2. JSON output must also exit 8 and omit raw secret everywhere.
    let (deps_json, cap_json) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(None, MockDriver::new(vec![]))),
        default_probes(),
        CancellationToken::new(),
    );
    let code_json = run_cli_from_args(["deja", "manifest", "check", "--json"], &deps_json).await;
    assert_eq!(code_json, 8);
    let json_out = cap_json.lock().unwrap().clone();
    assert!(!json_out.stdout.contains(&secret_val));
    assert!(!json_out.stderr.contains(&secret_val));
}

// ===========================================================================
// 4. Codex Round 2 regression tests (Findings 1 through 6)
// ===========================================================================

#[tokio::test]
async fn regression_r2_f1_ios_bundle_staging_binds_install_to_fingerprinted_build_and_rejects_drift()
 {
    let ws = TempWs::new("r2-f1-ios-stage");
    let xml_plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key>
  <string>{TEST_APP_ID}</string>
</dict>
</plist>
"#
    );
    ws.write("build/MyApp.app/Info.plist", &xml_plist);
    let bin_path = ws.write_build("build/MyApp.app/MyApp", b"binary-v1");
    let app_dir = ws.path.join("build/MyApp.app");
    let fp_v1 = fingerprint_tree(&ws.path, &app_dir, "iOS").unwrap();

    // 1. Unchanged bundle stages cleanly with matching fingerprint, preserves `.app` name,
    // and RAII Drop removes the temporary staging directory.
    let staged = stage_and_verify_ios_app_bundle(&app_dir, TEST_APP_ID, Some(&fp_v1))
        .expect("unchanged bundle must stage and match fingerprint");
    assert_eq!(staged.fingerprint(), fp_v1);
    assert_eq!(
        staged.app_path().extension().and_then(|s| s.to_str()),
        Some("app")
    );
    assert_ne!(staged.app_path(), app_dir);
    assert_eq!(
        fs::read(staged.app_path().join("MyApp")).unwrap(),
        b"binary-v1"
    );
    let staging_dir = staged.staging_dir().to_path_buf();
    assert!(staging_dir.is_dir());
    drop(staged);
    assert!(
        !staging_dir.exists(),
        "RAII Drop must delete temporary iOS staging directory"
    );

    // 2. If the bundle drifts on disk AFTER fingerprinting, `RealDriverFactory::install_app`
    // MUST fail closed with exit 2 BEFORE invoking `simctl`.
    fs::write(&bin_path, b"binary-v2-drifted").unwrap();
    let cancel = CancellationToken::new();
    let drift_ctx = DriverContext {
        platform: "ios".to_string(),
        device: TEST_UDID.to_string(),
        app_id: TEST_APP_ID.to_string(),
        build_path: app_dir.clone(),
        expected_build_fingerprint: Some(fp_v1.clone()),
        workspace_root: ws.path.clone(),
        disable_animations: true,
        crash_report_grace_ms: 0,
        crash_report_dir: None,
        deja_home: None,
        headless: None,
        screen_size: None,
        timeout_ms: 5_000,
        env: BTreeMap::new(),
        staged_build: None,
        resolved_runtime: None,
    };
    let drift_err = RealDriverFactory
        .install_app(&drift_ctx, &cancel)
        .await
        .expect_err("drifted bundle must fail before simctl install");
    assert_eq!(drift_err.exit_code(), 2);
    assert!(
        drift_err
            .to_string()
            .contains("changed between fingerprinting"),
        "unexpected error: {drift_err}"
    );

    // 3. End-to-end `deja remember`: mutating the `.app` bundle between fingerprinting and
    // `install_app` fails with exit 2 and persists NOTHING under the stale build fingerprint.
    fs::write(&bin_path, b"binary-v1").unwrap();
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/MyApp.app",
            "device": TEST_UDID,
            "manifest": "deja.walks.json"
        })
        .to_string(),
    );
    ws.write(
        "flows/open.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Home\n",
    );
    let ok_tree = hierarchy_with_nodes(TEST_APP_ID, vec![node_with_label("home", "Home")]);
    let mutate_bin = bin_path.clone();
    let factory = Arc::new(
        MockFactory::new(Some("ios-27.2"), MockDriver::new(vec![ok_tree]))
            .with_on_create(Arc::new(move |_| {
                fs::write(&mutate_bin, b"binary-v3-mutated-before-install").unwrap();
            }))
            .with_ios_stage_verification(),
    );
    let (deps, _) = make_deps(
        &ws.path,
        factory,
        default_probes(),
        CancellationToken::new(),
    );
    let code = run_cli_from_args(
        [
            "deja",
            "remember",
            "open home",
            "--flow",
            "flows/open.yaml",
            "--json",
        ],
        &deps,
    )
    .await;
    assert_eq!(
        code, 2,
        "remember must exit 2 when bundle drifts before install"
    );
    assert!(
        !ws.path.join("deja.walks.json").exists(),
        "drifted build must not write manifest"
    );
    let ro = SqliteWalkStore::open_readonly(SqliteStoreOptions::new(&ws.path)).unwrap();
    assert!(
        ro.list_walks().unwrap().is_empty(),
        "drifted build must not record any walk under old fingerprint"
    );
}

#[tokio::test]
async fn regression_r2_f2_run_executes_exact_closure_approved_by_plan_without_reopening_disk() {
    // Part A: Manifest mode — mutate flow on disk inside `DriverFactory::create` (right after
    // `plan_execution` approved the closure, BEFORE execution). Because `run` reuses the exact
    // planned closure instead of reopening disk, it executes the approved bytes (zero `killApp`,
    // never fails with `CLOSURE_ERROR` or `APP_ID_CONFINEMENT`), then post-run drift check refuses
    // persistence cleanly with exit 6 (`flow-changed`).
    let ws = TempWs::new("r2-f2-plan-closure");
    ws.write_build("build/App.bin", b"v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "manifest": "deja.walks.json"
        })
        .to_string(),
    );
    let main_path = ws.write(
        "flows/main.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Ready\n",
    );
    let ok_tree = hierarchy_with_nodes(TEST_APP_ID, vec![node_with_label("ready", "Ready")]);

    let (deps_rem, _) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(
            Some("ios-27.2"),
            MockDriver::new(vec![ok_tree.clone()]),
        )),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            [
                "deja",
                "remember",
                "open ready",
                "--flow",
                "flows/main.yaml"
            ],
            &deps_rem,
        )
        .await,
        0
    );

    let mutate_main = main_path.clone();
    let driver_a = MockDriver::new(vec![ok_tree.clone()]);
    let launches_a = Arc::clone(&driver_a.launch_count);
    let killed_a = Arc::clone(&driver_a.killed_apps);
    let factory_a = Arc::new(MockFactory::new(Some("ios-27.2"), driver_a).with_on_create(
        Arc::new(move |_| {
            // Corrupt disk file right after `plan_execution` returned (inside `create()`).
            fs::write(
                &mutate_main,
                "appId: com.evil.unapproved\n---\n- killApp: com.evil.unapproved\n",
            )
            .unwrap();
        }),
    ));
    let (deps_run_a, cap_a) = make_deps(
        &ws.path,
        factory_a,
        default_probes(),
        CancellationToken::new(),
    );
    let code_a = run_cli_from_args(["deja", "run", "open ready", "--json"], &deps_run_a).await;
    assert_eq!(
        launches_a.load(Ordering::SeqCst),
        1,
        "run must execute the exact closure approved by plan_execution without re-reading disk"
    );
    assert!(
        killed_a.lock().unwrap().is_empty(),
        "mutated post-plan bytes must never execute"
    );
    assert_eq!(code_a, 6);
    let rep_a: Value = serde_json::from_str(&cap_a.lock().unwrap().stdout).unwrap();
    assert_eq!(rep_a["outcome"], "flow-changed");

    // Part B: Classic mode — mutate stored `.deja/flows/...` file inside `DriverFactory::create`
    // (after `plan_execution` verified and captured the classic closure).
    let ws_classic = TempWs::new("r2-f2-classic-closure");
    let build_c = ws_classic.write_build("build/App.bin", b"v1");
    let fp_c = fingerprint_file(&ws_classic.path, &build_c).unwrap();
    ws_classic.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID
        })
        .to_string(),
    );
    let src_flow = ws_classic.write(
        "flows/classic.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Ready\n",
    );
    let store_c =
        SqliteWalkStore::open_writable(SqliteStoreOptions::new(&ws_classic.path)).unwrap();
    let id_c = WalkIdentity {
        goal: "classic ready".to_string(),
        app_id: TEST_APP_ID.to_string(),
        platform: "ios".to_string(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint: fp_c,
        adapter: Some("maestro-ios/v1".to_string()),
        browser: None,
    };
    let saved = store_c
        .persist_verified_candidate(
            &id_c,
            &deja_store::PersistVerifiedCandidateOptions {
                candidate_path: src_flow,
                validation: RecordValidationInput::new(true, now_iso8601())
                    .with_origin(WalkOrigin::Remember)
                    .with_runtime("ios-27.2"),
                expected_flow_sha256: None,
                expected_closure_sha256: None,
                exploration: None,
                repair_history: None,
            },
        )
        .unwrap();
    let stored_abs = ws_classic.path.join(&saved.flow_path);
    store_c.close();

    let driver_b = MockDriver::new(vec![ok_tree]);
    let launches_b = Arc::clone(&driver_b.launch_count);
    let killed_b = Arc::clone(&driver_b.killed_apps);
    let mutate_stored = stored_abs.clone();
    let factory_b = Arc::new(MockFactory::new(Some("ios-27.2"), driver_b).with_on_create(
        Arc::new(move |_| {
            fs::write(
                &mutate_stored,
                "appId: com.evil.classic\n---\n- killApp: com.evil.classic\n",
            )
            .unwrap();
        }),
    ));
    let (deps_run_b, _) = make_deps(
        &ws_classic.path,
        factory_b,
        default_probes(),
        CancellationToken::new(),
    );
    let code_b = run_cli_from_args(["deja", "run", "classic ready", "--json"], &deps_run_b).await;
    assert_eq!(code_b, 0);
    assert_eq!(launches_b.load(Ordering::SeqCst), 1);
    assert!(
        killed_b.lock().unwrap().is_empty(),
        "classic mode must execute the exact closure verified during plan_execution"
    );
}

#[tokio::test]
async fn regression_r2_f3_add_media_stages_exact_preflight_closure_bytes_and_cleans_up() {
    let ws = TempWs::new("r2-f3-add-media");
    ws.write_build("build/App.bin", b"v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "manifest": "deja.walks.json"
        })
        .to_string(),
    );
    let original_png = b"\x89PNG\r\n\x1a\nPREFLIGHT_ORIGINAL_BYTES";
    let media_path = ws.write_build("flows/assets/photo.png", original_png);
    ws.write(
        "flows/upload.yaml",
        "appId: dev.deja.test.app\n---\n- addMedia:\n    - assets/photo.png\n- launchApp\n- assertVisible: Uploaded\n",
    );

    let ok_tree = hierarchy_with_nodes(TEST_APP_ID, vec![node_with_label("up", "Uploaded")]);
    let mutate_media = media_path.clone();
    let driver = MockDriver::new(vec![ok_tree]).with_on_ensure_session(Arc::new(move || {
        fs::write(&mutate_media, b"\x89PNG\r\n\x1a\nMUTATED_ATTACKER_BYTES").unwrap();
    }));
    let captured_media = Arc::clone(&driver.captured_media);

    let (deps, cap) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(Some("ios-27.2"), driver)),
        default_probes(),
        CancellationToken::new(),
    );
    let code = run_cli_from_args(
        [
            "deja",
            "remember",
            "upload photo",
            "--flow",
            "flows/upload.yaml",
            "--json",
        ],
        &deps,
    )
    .await;

    let recorded = captured_media.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1, "addMedia must have been invoked once");
    let (staged_path, staged_bytes) = &recorded[0];
    assert_eq!(
        staged_path.file_name().and_then(|s| s.to_str()),
        Some("photo.png"),
        "staged media file must preserve original filename and extension"
    );
    assert_ne!(
        staged_path, &media_path,
        "addMedia must pass staged temp file, not live mutable workspace file"
    );
    assert_eq!(
        staged_bytes.as_slice(),
        original_png,
        "addMedia must stage exact preflight closure bytes, never post-preflight disk edits"
    );
    assert!(
        !staged_path.exists(),
        "temporary media staging file must be cleaned up on drop"
    );

    // Post-run drift check also catches the mutated media fixture and refuses persistence (exit 6).
    assert_eq!(code, 6);
    let rep: Value = serde_json::from_str(&cap.lock().unwrap().stdout).unwrap();
    assert_eq!(rep["outcome"], "flow-changed");
    assert!(!ws.path.join("deja.walks.json").exists());
}

#[tokio::test]
async fn regression_r2_f4_gate_invalidates_prior_evidence_on_malformed_config_and_invalid_label_while_protecting_unrelated()
 {
    let ws = TempWs::new("r2-f4-gate-early");
    let build = ws.write_build("build/App.bin", b"v1");
    let fp = fingerprint_file(&ws.path, &build).unwrap();
    let valid_cfg = json!({
        "workspaceRoot": ".",
        "platform": "ios",
        "appId": TEST_APP_ID,
        "adapter": "maestro-ios/v1",
        "buildPath": "build/App.bin"
    })
    .to_string();
    ws.write("deja.config.json", &valid_cfg);
    let flow = ws.write(
        "flows/checkout.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Done\n",
    );
    ws.write("goals.txt", "checkout\n");

    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(&ws.path)).unwrap();
    let id = WalkIdentity {
        goal: "checkout".to_string(),
        app_id: TEST_APP_ID.to_string(),
        platform: "ios".to_string(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint: fp,
        adapter: Some("maestro-ios/v1".to_string()),
        browser: None,
    };
    store
        .persist_verified_candidate(
            &id,
            &deja_store::PersistVerifiedCandidateOptions {
                candidate_path: flow,
                validation: RecordValidationInput::new(true, now_iso8601())
                    .with_origin(WalkOrigin::Remember)
                    .with_runtime("ios-27.2"),
                expected_flow_sha256: None,
                expected_closure_sha256: None,
                exploration: None,
                repair_history: None,
            },
        )
        .unwrap();
    store.close();

    let run_gate = |args: Vec<&'static str>| {
        let ws_path = ws.path.clone();
        async move {
            let (deps, _) = make_deps(
                &ws_path,
                Arc::new(MockFactory::new(None, MockDriver::new(vec![]))),
                default_probes(),
                CancellationToken::new(),
            );
            run_cli_from_args(args, &deps).await
        }
    };

    // 1. Explicit `--evidence` + invalid `--label` -> exits 2 AND removes prior evidence `.json` + `.txt`.
    let explicit_json = ws.path.join("reports/gate-ev.json");
    let explicit_txt = ws.path.join("reports/gate-ev.txt");
    assert_eq!(
        run_gate(vec![
            "deja",
            "gate",
            "--goals",
            "goals.txt",
            "--evidence",
            "reports/gate-ev.json",
            "--json",
        ])
        .await,
        0
    );
    assert!(explicit_json.is_file() && explicit_txt.is_file());

    assert_eq!(
        run_gate(vec![
            "deja",
            "gate",
            "--goals",
            "goals.txt",
            "--evidence",
            "reports/gate-ev.json",
            "--label",
            "release\nbad",
            "--json",
        ])
        .await,
        2
    );
    assert!(
        !explicit_json.exists() && !explicit_txt.exists(),
        "prior explicit evidence must be cleared on invalid --label"
    );

    // 2. Default `.deja/gate/evidence.json` + invalid `--label` -> exits 2 AND removes default evidence.
    let default_json = ws.path.join(".deja/gate/evidence.json");
    let default_txt = ws.path.join(".deja/gate/evidence.txt");
    assert_eq!(
        run_gate(vec!["deja", "gate", "--goals", "goals.txt", "--json"]).await,
        0
    );
    assert!(default_json.is_file() && default_txt.is_file());
    assert_eq!(
        run_gate(vec![
            "deja",
            "gate",
            "--goals",
            "goals.txt",
            "--label",
            "bad\u{202e}label",
            "--json",
        ])
        .await,
        2
    );
    assert!(
        !default_json.exists() && !default_txt.exists(),
        "default evidence must be cleared on invalid --label"
    );

    // 3. Explicit `--evidence` + malformed `deja.config.json` -> exits 2 AND removes prior evidence.
    assert_eq!(
        run_gate(vec![
            "deja",
            "gate",
            "--goals",
            "goals.txt",
            "--evidence",
            "reports/gate-ev.json",
            "--json",
        ])
        .await,
        0
    );
    assert!(explicit_json.is_file() && explicit_txt.is_file());
    ws.write("deja.config.json", r#"{"platform": "ios", "broken": [1}"#);
    assert_eq!(
        run_gate(vec![
            "deja",
            "gate",
            "--goals",
            "goals.txt",
            "--evidence",
            "reports/gate-ev.json",
            "--json",
        ])
        .await,
        2
    );
    assert!(
        !explicit_json.exists() && !explicit_txt.exists(),
        "prior explicit evidence must be cleared on malformed config"
    );

    // 4. Unrelated non-evidence file is STILL protected on BOTH malformed config and invalid `--label`.
    let unrelated = ws.write("notes/keep.json", r#"{"important":true}"#);
    assert_eq!(
        run_gate(vec![
            "deja",
            "gate",
            "--goals",
            "goals.txt",
            "--evidence",
            "notes/keep.json",
            "--json",
        ])
        .await,
        2
    );
    assert_eq!(
        fs::read_to_string(&unrelated).unwrap(),
        r#"{"important":true}"#
    );

    ws.write("deja.config.json", &valid_cfg);
    assert_eq!(
        run_gate(vec![
            "deja",
            "gate",
            "--goals",
            "goals.txt",
            "--evidence",
            "notes/keep.json",
            "--label",
            "bad\nlabel",
            "--json",
        ])
        .await,
        2
    );
    assert_eq!(
        fs::read_to_string(&unrelated).unwrap(),
        r#"{"important":true}"#
    );
}

#[test]
fn regression_r2_f5_binary_and_xml_plist_with_data_and_date_extracts_bundle_id() {
    let ws = TempWs::new("r2-f5-plist-data-date");
    let xml_with_non_json_types = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>RawBlob</key>
  <data>AAECAwQFBgc=</data>
  <key>BuildDate</key>
  <date>2026-06-01T12:00:00Z</date>
  <key>Nested</key>
  <dict>
    <key>CFBundleIdentifier</key>
    <string>dev.deja.nested.wrong</string>
  </dict>
  <key>CFBundleIdentifier</key>
  <string>{TEST_APP_ID}</string>
</dict>
</plist>
"#
    );
    ws.write("XmlApp.app/Info.plist", &xml_with_non_json_types);
    verify_ios_app_bundle(&ws.path.join("XmlApp.app"), TEST_APP_ID)
        .expect("XML plist with <data> and <date> must pass");

    #[cfg(target_os = "macos")]
    {
        let bin_app = ws.path.join("BinDataDate.app");
        fs::create_dir_all(&bin_app).unwrap();
        let bin_plist = bin_app.join("Info.plist");
        fs::write(&bin_plist, &xml_with_non_json_types).unwrap();
        let st = Command::new("/usr/bin/plutil")
            .args(["-convert", "binary1"])
            .arg(&bin_plist)
            .status()
            .unwrap();
        assert!(st.success(), "plutil -convert binary1 failed");
        assert!(fs::read(&bin_plist).unwrap().starts_with(b"bplist"));
        verify_ios_app_bundle(&bin_app, TEST_APP_ID)
            .expect("binary plist containing NSData/NSDate must decode via xml1 cleanly");

        assert!(
            verify_ios_app_bundle(&bin_app, "dev.deja.wrong.app").is_err(),
            "binary plist with mismatched CFBundleIdentifier must still fail"
        );
    }
}

#[tokio::test]
async fn regression_r2_f6_debug_logs_never_emit_simctl_args_or_open_link_secrets() {
    let ws = TempWs::new("r2-f6-debug-logs");
    let url_secret = ["tok", "_openlink_secret_987654321"].concat();
    let deep_link = format!("myapp://auth/callback?token={url_secret}");

    // 1. Exercise `Simctl::open_link` under a DEBUG-level tracing subscriber against a stub `xcrun`
    // script and confirm `tracing::debug!` emits ONLY subcommand + arg_count, NEVER raw args/URL!
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let stub_bin = ws.path.join("stub-bin");
        fs::create_dir_all(&stub_bin).unwrap();
        let xcrun_stub = stub_bin.join("xcrun");
        fs::write(&xcrun_stub, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&xcrun_stub, fs::Permissions::from_mode(0o755)).unwrap();

        #[derive(Clone, Default)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
            type Writer = SharedBuf;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(buf.clone())
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        let simctl = Simctl::with_env(SimctlEnv {
            env_path: Some(stub_bin.as_os_str().to_os_string()),
            developer_dir: None,
        });

        let fut = tracing::dispatcher::with_default(&dispatch, || {
            simctl.open_link(TEST_UDID, &deep_link)
        });
        // Run future with subscriber bound for the async poll as well via `tracing::Instrument`.
        let res = {
            use tracing::instrument::WithSubscriber;
            fut.with_subscriber(dispatch).await
        };
        assert!(res.is_ok(), "stub xcrun simctl openurl must succeed");

        let logged = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
        assert!(
            logged.contains("running xcrun simctl"),
            "expected simctl debug log line, got: {logged}"
        );
        assert!(
            logged.contains("openurl"),
            "expected simctl subcommand name in debug log, got: {logged}"
        );
        assert!(
            !logged.contains(&url_secret),
            "simctl debug log MUST NEVER contain openLink URL secret, got: {logged}"
        );
        assert!(
            !logged.contains("myapp://"),
            "simctl debug log MUST NEVER contain raw URL argument, got: {logged}"
        );
    }

    // 2. Subprocess check with `DEJA_LOG=debug` and a secret in config/flow: stderr must NEVER leak it.
    let cfg_secret = ["fixture", "_debug_secret_443322"].concat();
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "secrets": [cfg_secret],
            "env": {
                "AUTH_URL": deep_link
            }
        })
        .to_string(),
    );
    let out = deja_bin()
        .current_dir(&ws.path)
        .env("DEJA_LOG", "debug")
        .args(["list", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stderr_str = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr_str.contains(&url_secret));
    assert!(!stderr_str.contains(&cfg_secret));
}

// ===========================================================================
// 5. Codex Round 3 regression tests (Findings 1 through 8)
// ===========================================================================

#[tokio::test]
async fn regression_r3_f1_gate_does_not_publish_passing_json_when_txt_write_fails() {
    let ws = TempWs::new("r3-f1-gate-pair");
    let build = ws.write_build("build/App.bin", b"v1");
    let fp = fingerprint_file(&ws.path, &build).unwrap();
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin"
        })
        .to_string(),
    );
    let flow = ws.write(
        "flows/checkout.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Done\n",
    );
    ws.write("goals.txt", "checkout\n");

    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(&ws.path)).unwrap();
    let id = WalkIdentity {
        goal: "checkout".to_string(),
        app_id: TEST_APP_ID.to_string(),
        platform: "ios".to_string(),
        flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
        build_fingerprint: fp,
        adapter: Some("maestro-ios/v1".to_string()),
        browser: None,
    };
    store
        .persist_verified_candidate(
            &id,
            &deja_store::PersistVerifiedCandidateOptions {
                candidate_path: flow,
                validation: RecordValidationInput::new(true, now_iso8601())
                    .with_origin(WalkOrigin::Remember)
                    .with_runtime("ios-27.2"),
                expected_flow_sha256: None,
                expected_closure_sha256: None,
                exploration: None,
                repair_history: None,
            },
        )
        .unwrap();
    store.close();

    // 1. Direct atomic-pair invariant: when the `.txt` target cannot be written (parent is a
    // regular file), `write_atomic_pair_nofollow_excl` fails BEFORE publishing `.json` and leaves
    // zero `.json` and zero `.tmp` files in the writable `.json` directory.
    let blocker = ws.write("blocker", "not a dir");
    let ok_dir = ws.path.join("ok-out");
    fs::create_dir_all(&ok_dir).unwrap();
    let bad_txt = blocker.join("ev.txt");
    let good_json = ok_dir.join("ev.json");
    assert!(
        deja_cli::fs_safe::write_atomic_pair_nofollow_excl(
            &bad_txt,
            b"table\n",
            &good_json,
            b"{\"verdict\":\"passed\"}\n",
        )
        .is_err()
    );
    assert!(
        !good_json.exists(),
        ".json must never be published when .txt fails"
    );
    assert_eq!(
        fs::read_dir(&ok_dir).unwrap().count(),
        0,
        "no leftover temp files in .json directory"
    );

    // 2. End-to-end `deja gate`: an overly long evidence basename triggers ENAMETOOLONG when
    // staging sibling temporaries -> gate exits 11 and leaves NO `.json` or `.txt` on disk.
    let long_name = format!("{}.json", "e".repeat(240));
    let long_rel = format!("out/{long_name}");
    let (deps, _) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(None, MockDriver::new(vec![]))),
        default_probes(),
        CancellationToken::new(),
    );
    let code = run_cli_from_args(
        [
            "deja",
            "gate",
            "--goals",
            "goals.txt",
            "--evidence",
            &long_rel,
            "--json",
        ],
        &deps,
    )
    .await;
    assert_eq!(code, 11, "gate must exit 11 when evidence write fails");
    assert!(!ws.path.join(&long_rel).exists());
    let out_dir = ws.path.join("out");
    if out_dir.exists() {
        assert_eq!(
            fs::read_dir(&out_dir).unwrap().count(),
            0,
            "failed gate evidence write must leave zero partial or temp files"
        );
    }
}

#[tokio::test]
async fn regression_r3_f2_file_backed_subflow_with_url_header_rejected_at_preflight() {
    let ws = TempWs::new("r3-f2-subflow-url");
    ws.write_build("build/App.bin", b"v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID
        })
        .to_string(),
    );
    ws.write(
        "flows/main.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- runFlow: sub.yaml\n",
    );
    ws.write(
        "flows/sub.yaml",
        "url: https://evil.example/phish\n---\n- assertVisible: Welcome\n",
    );

    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), MockDriver::new(vec![])));
    let create_calls = Arc::clone(&factory.create_calls);
    let (deps, cap) = make_deps(
        &ws.path,
        factory,
        default_probes(),
        CancellationToken::new(),
    );
    let code = run_cli_from_args(
        [
            "deja",
            "remember",
            "subflow url escape",
            "--flow",
            "flows/main.yaml",
            "--json",
        ],
        &deps,
    )
    .await;
    assert_eq!(
        code, 2,
        "subflow with url: header must fail preflight exit 2"
    );
    assert_eq!(
        create_calls.load(Ordering::SeqCst),
        0,
        "driver must never be created when subflow url header fails confinement"
    );
    let out = cap.lock().unwrap().stdout.clone();
    assert!(out.contains("APP_ID_CONFINEMENT"));
}

#[tokio::test]
async fn regression_r3_f3_manifest_check_suppresses_fix_when_goal_contains_apostrophe_secret() {
    let ws = TempWs::new("r3-f3-manifest-apostrophe");
    let apo_secret = ["sec", "'ret-val-99"].concat();
    let goal_with_secret = format!("buy {apo_secret} item");

    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "manifest": "deja.walks.json",
            "secrets": [apo_secret]
        })
        .to_string(),
    );
    let f1 = ws.write(
        "flows/changed.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Before\n",
    );
    let cl1 = collect_flow_closure(&ws.path, &f1).unwrap();
    let as1 = assertions_sha256(&cl1).unwrap();

    let f2 = ws.write(
        "flows/assert_drift.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Stay\n",
    );
    let cl2 = collect_flow_closure(&ws.path, &f2).unwrap();

    ws.write(
        "deja.walks.json",
        &json!({
            "manifestVersion": 1,
            "hashVersion": 1,
            "walks": [
                {
                    "goal": goal_with_secret,
                    "appId": TEST_APP_ID,
                    "platform": "ios",
                    "adapter": "maestro-ios/v1",
                    "flow": "flows/changed.yaml",
                    "closureSha256": cl1.sha256,
                    "assertionsSha256": as1
                },
                {
                    "goal": format!("assert {apo_secret} goal"),
                    "appId": TEST_APP_ID,
                    "platform": "ios",
                    "adapter": "maestro-ios/v1",
                    "flow": "flows/assert_drift.yaml",
                    "closureSha256": cl2.sha256,
                    "assertionsSha256": "0".repeat(64)
                }
            ]
        })
        .to_string(),
    );
    // Edit flow 1 so `flow-changed` triggers.
    ws.write(
        "flows/changed.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: After\n",
    );

    let (deps_json, cap_json) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(None, MockDriver::new(vec![]))),
        default_probes(),
        CancellationToken::new(),
    );
    let code_json = run_cli_from_args(["deja", "manifest", "check", "--json"], &deps_json).await;
    assert_eq!(code_json, 8);
    let out_json = cap_json.lock().unwrap().clone();
    assert!(!out_json.stdout.contains(&apo_secret));
    assert!(!out_json.stdout.contains("ret-val-99"));
    assert!(!out_json.stderr.contains(&apo_secret));
    assert!(!out_json.stderr.contains("ret-val-99"));

    let parsed: Value = serde_json::from_str(&out_json.stdout).unwrap();
    for entry in parsed["entries"].as_array().unwrap() {
        assert!(
            entry["goal"]
                .as_str()
                .unwrap()
                .contains("<redacted-fixture-value>")
        );
        assert!(
            entry["fix"].is_null(),
            "fix command must be suppressed (null) when goal contains a redacted secret"
        );
        assert_eq!(entry["fixUnsafe"], true);
    }

    let (deps_human, cap_human) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(None, MockDriver::new(vec![]))),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(["deja", "manifest", "check"], &deps_human).await,
        8
    );
    let out_human = cap_human.lock().unwrap().clone();
    assert!(!out_human.stdout.contains(&apo_secret));
    assert!(!out_human.stdout.contains("ret-val-99"));
    assert!(!out_human.stderr.contains("ret-val-99"));
}

#[tokio::test]
async fn regression_r3_f4_mixed_case_secret_never_leaks_via_goal_key_across_commands() {
    let ws = TempWs::new("r3-f4-goal-key-redact");
    let mixed_secret = ["MyMixed", "CaseSecret77"].concat();
    let lower_secret = mixed_secret.to_ascii_lowercase();
    let raw_goal = format!("Checkout {mixed_secret}");

    ws.write_build("build/App.bin", b"v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "secrets": [mixed_secret]
        })
        .to_string(),
    );
    ws.write(
        "flows/checkout.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Done\n",
    );
    ws.write("goals.txt", &format!("{raw_goal}\n"));

    let ok_tree = hierarchy_with_nodes(TEST_APP_ID, vec![node_with_label("done", "Done")]);
    let factory = Arc::new(MockFactory::new(
        Some("ios-27.2"),
        MockDriver::new(vec![ok_tree]),
    ));

    // 1. `deja remember --report out/rem.json --json`
    let (deps_rem, cap_rem) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            [
                "deja",
                "remember",
                &raw_goal,
                "--flow",
                "flows/checkout.yaml",
                "--report",
                "out/rem.json",
                "--json",
            ],
            &deps_rem,
        )
        .await,
        0
    );
    let rem_out = cap_rem.lock().unwrap().clone();
    assert!(!rem_out.stdout.contains(&mixed_secret));
    assert!(!rem_out.stdout.contains(&lower_secret));
    let rem_json: Value = serde_json::from_str(&rem_out.stdout).unwrap();
    assert_eq!(rem_json["goalKey"], "checkout <redacted-fixture-value>");

    // 2. `deja run --json`
    let (deps_run, cap_run) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(["deja", "run", &raw_goal, "--json"], &deps_run).await,
        0
    );
    let run_out = cap_run.lock().unwrap().stdout.clone();
    assert!(!run_out.contains(&mixed_secret));
    assert!(!run_out.contains(&lower_secret));
    let run_json: Value = serde_json::from_str(&run_out).unwrap();
    assert_eq!(run_json["goalKey"], "checkout <redacted-fixture-value>");

    // 3. `deja list --json`
    let (deps_list, cap_list) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(["deja", "list", "--json"], &deps_list).await,
        0
    );
    let list_out = cap_list.lock().unwrap().stdout.clone();
    assert!(!list_out.contains(&mixed_secret));
    assert!(!list_out.contains(&lower_secret));
    let list_json: Value = serde_json::from_str(&list_out).unwrap();
    assert_eq!(
        list_json["walks"][0]["goalKey"],
        "checkout <redacted-fixture-value>"
    );

    // 4. `deja inspect --goal <raw_goal> --json` AND `deja inspect out/rem.json --json`
    let (deps_ig, cap_ig) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(["deja", "inspect", "--goal", &raw_goal, "--json"], &deps_ig).await,
        0
    );
    let ig_out = cap_ig.lock().unwrap().stdout.clone();
    assert!(!ig_out.contains(&mixed_secret));
    assert!(!ig_out.contains(&lower_secret));
    let ig_json: Value = serde_json::from_str(&ig_out).unwrap();
    assert_eq!(
        ig_json["walk"]["goalKey"],
        "checkout <redacted-fixture-value>"
    );

    let (deps_ir, cap_ir) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(["deja", "inspect", "out/rem.json", "--json"], &deps_ir).await,
        0
    );
    let ir_out = cap_ir.lock().unwrap().stdout.clone();
    assert!(!ir_out.contains(&mixed_secret));
    assert!(!ir_out.contains(&lower_secret));

    // 5. `deja gate --goals goals.txt --evidence .deja/gate/ev.json --json`
    let (deps_gate, cap_gate) = make_deps(
        &ws.path,
        factory,
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            [
                "deja",
                "gate",
                "--goals",
                "goals.txt",
                "--evidence",
                ".deja/gate/ev.json",
                "--json",
            ],
            &deps_gate,
        )
        .await,
        0
    );
    let gate_out = cap_gate.lock().unwrap().stdout.clone();
    assert!(!gate_out.contains(&mixed_secret));
    assert!(!gate_out.contains(&lower_secret));
    let gate_json: Value = serde_json::from_str(&gate_out).unwrap();
    assert_eq!(
        gate_json["goals"][0]["goalKey"],
        "checkout <redacted-fixture-value>"
    );

    let ev_json_disk = fs::read_to_string(ws.path.join(".deja/gate/ev.json")).unwrap();
    let ev_txt_disk = fs::read_to_string(ws.path.join(".deja/gate/ev.txt")).unwrap();
    assert!(!ev_json_disk.contains(&mixed_secret) && !ev_json_disk.contains(&lower_secret));
    assert!(!ev_txt_disk.contains(&mixed_secret) && !ev_txt_disk.contains(&lower_secret));
}

#[tokio::test]
async fn regression_r3_f5_explicit_report_and_junit_protect_config_manifest_closure_and_memory() {
    let ws = TempWs::new("r3-f5-protect-state");
    ws.write_build("build/App.bin", b"v1");
    let cfg_text = json!({
        "workspaceRoot": ".",
        "platform": "ios",
        "appId": TEST_APP_ID,
        "adapter": "maestro-ios/v1",
        "buildPath": "build/App.bin",
        "device": TEST_UDID,
        "manifest": "deja.walks.json"
    })
    .to_string();
    let cfg_path = ws.write("deja.config.json", &cfg_text);
    let main_text =
        "appId: dev.deja.test.app\n---\n- launchApp\n- runFlow: sub.yaml\n- assertVisible: Done\n";
    let sub_text = "appId: dev.deja.test.app\n---\n- assertVisible: Done\n";
    let main_path = ws.write("flows/main.yaml", main_text);
    let sub_path = ws.write("flows/sub.yaml", sub_text);

    // Seed a valid walk so `deja.walks.json` and `.deja/memory/walks.sqlite` both exist!
    let ok_tree = hierarchy_with_nodes(TEST_APP_ID, vec![node_with_label("done", "Done")]);
    let seed_factory = Arc::new(MockFactory::new(
        Some("ios-27.2"),
        MockDriver::new(vec![ok_tree.clone()]),
    ));
    let (deps_seed, _) = make_deps(
        &ws.path,
        seed_factory,
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            ["deja", "remember", "checkout", "--flow", "flows/main.yaml"],
            &deps_seed,
        )
        .await,
        0
    );

    let manifest_path = ws.path.join("deja.walks.json");
    let sqlite_path = ws.path.join(".deja/memory/walks.sqlite");
    let manifest_before = fs::read(&manifest_path).unwrap();
    let sqlite_before = fs::read(&sqlite_path).unwrap();

    for protected in [
        "deja.walks.json",
        "deja.config.json",
        ".deja/memory/walks.sqlite",
        "flows/main.yaml",
        "flows/sub.yaml",
    ] {
        for flag in ["--report", "--junit"] {
            let f_rem = Arc::new(MockFactory::new(
                Some("ios-27.2"),
                MockDriver::new(vec![ok_tree.clone()]),
            ));
            let calls_rem = Arc::clone(&f_rem.create_calls);
            let (deps_r, _) =
                make_deps(&ws.path, f_rem, default_probes(), CancellationToken::new());
            let code_r = run_cli_from_args(
                [
                    "deja",
                    "remember",
                    "checkout",
                    "--flow",
                    "flows/main.yaml",
                    flag,
                    protected,
                    "--json",
                ],
                &deps_r,
            )
            .await;
            assert_eq!(
                code_r, 2,
                "remember {flag} {protected} must be refused with exit 2"
            );
            assert_eq!(
                calls_rem.load(Ordering::SeqCst),
                0,
                "driver must not run when {flag} targets {protected}"
            );

            let f_run = Arc::new(MockFactory::new(
                Some("ios-27.2"),
                MockDriver::new(vec![ok_tree.clone()]),
            ));
            let calls_run = Arc::clone(&f_run.create_calls);
            let (deps_run, _) =
                make_deps(&ws.path, f_run, default_probes(), CancellationToken::new());
            let code_run = run_cli_from_args(
                ["deja", "run", "checkout", flag, protected, "--json"],
                &deps_run,
            )
            .await;
            assert_eq!(
                code_run, 2,
                "run {flag} {protected} must be refused with exit 2"
            );
            assert_eq!(
                calls_run.load(Ordering::SeqCst),
                0,
                "driver must not run when `run {flag}` targets {protected}"
            );
        }
    }

    // Confirm EVERY protected file remained completely untouched!
    assert_eq!(fs::read_to_string(&cfg_path).unwrap(), cfg_text);
    assert_eq!(fs::read_to_string(&main_path).unwrap(), main_text);
    assert_eq!(fs::read_to_string(&sub_path).unwrap(), sub_text);
    assert_eq!(fs::read(&manifest_path).unwrap(), manifest_before);
    assert_eq!(fs::read(&sqlite_path).unwrap(), sqlite_before);
}

#[tokio::test]
async fn regression_r3_f6_unwritable_explicit_report_or_junit_returns_exit_2_even_when_run_passes()
{
    let ws = TempWs::new("r3-f6-unwritable-out");
    ws.write_build("build/App.bin", b"v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID
        })
        .to_string(),
    );
    ws.write(
        "flows/ok.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Done\n",
    );
    let ok_tree = hierarchy_with_nodes(TEST_APP_ID, vec![node_with_label("done", "Done")]);

    // Remember cleanly first.
    let (deps_seed, _) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(
            Some("ios-27.2"),
            MockDriver::new(vec![ok_tree.clone()]),
        )),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            ["deja", "remember", "ok goal", "--flow", "flows/ok.yaml"],
            &deps_seed,
        )
        .await,
        0
    );

    // 1. `--report` under a regular file (parent is NOT a directory) on a PASSING run -> exit 2!
    ws.write("not_a_dir", "file contents");
    let (deps_rep, cap_rep) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(
            Some("ios-27.2"),
            MockDriver::new(vec![ok_tree.clone()]),
        )),
        default_probes(),
        CancellationToken::new(),
    );
    let code_rep = run_cli_from_args(
        [
            "deja",
            "run",
            "ok goal",
            "--report",
            "not_a_dir/report.json",
            "--json",
        ],
        &deps_rep,
    )
    .await;
    assert_eq!(
        code_rep, 2,
        "unwritable explicit --report must return exit 2 even when the flow passed"
    );
    let rep_out = cap_rep.lock().unwrap().clone();
    let rep_json: Value = serde_json::from_str(&rep_out.stdout).unwrap();
    assert_eq!(rep_json["outcome"], "passed");
    assert!(
        rep_out.stderr.contains("failed to write explicit --report"),
        "expected stderr diagnostic, got: {}",
        rep_out.stderr
    );

    // 2. `--junit` whose parent directory becomes unwritable/replaced during run -> exit 2!
    let jdir = ws.path.join("junit_parent");
    let jdir_for_hook = jdir.clone();
    let driver_j = MockDriver::new(vec![ok_tree]).with_on_ensure_session(Arc::new(move || {
        // Replace prepared empty directory with a regular file so final JUnit write fails.
        let _ = fs::remove_dir_all(&jdir_for_hook);
        fs::write(&jdir_for_hook, "blocked").unwrap();
    }));
    let (deps_j, cap_j) = make_deps(
        &ws.path,
        Arc::new(MockFactory::new(Some("ios-27.2"), driver_j)),
        default_probes(),
        CancellationToken::new(),
    );
    let code_j = run_cli_from_args(
        [
            "deja",
            "run",
            "ok goal",
            "--junit",
            "junit_parent/out.xml",
            "--json",
        ],
        &deps_j,
    )
    .await;
    assert_eq!(
        code_j, 2,
        "unwritable explicit --junit must return exit 2 even when the flow passed"
    );
    assert!(
        cap_j
            .lock()
            .unwrap()
            .stderr
            .contains("failed to write JUnit report")
    );
}

#[test]
fn regression_r3_f7_diamond_dag_preflight_confinement_memoizes_and_bounds_work() {
    // 1. 14-layer diamond DAG (2^14 = 16,384 paths without memoization, 15 unique states with
    // memoization): must complete almost instantaneously.
    let ws = TempWs::new("r3-f7-diamond");
    let layers = 14usize;
    for i in 0..layers {
        let next = format!("layer_{:02}.yaml", i + 1);
        ws.write(
            &format!("flows/layer_{i:02}.yaml"),
            &format!("appId: {TEST_APP_ID}\n---\n- runFlow: {next}\n- runFlow: {next}\n"),
        );
    }
    ws.write(
        &format!("flows/layer_{layers:02}.yaml"),
        &format!("appId: {TEST_APP_ID}\n---\n- assertVisible: Leaf\n"),
    );

    let root = ws.path.join("flows/layer_00.yaml");
    let closure = collect_flow_closure(&ws.path, &root).unwrap();
    let t0 = std::time::Instant::now();
    deja_cli::commands::exec_common::validate_closure_app_id_confinement(
        &closure,
        TEST_APP_ID,
        &BTreeMap::new(),
    )
    .expect("diamond DAG with identical env must memoize and pass");
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "diamond DAG preflight took too long: {:?}",
        t0.elapsed()
    );

    // 2. Adversarial DAG with exponentially varying envs (2^13 = 8,192 distinct states > 4,096
    // cap) must fail closed promptly on the visit budget instead of hanging.
    let ws_adv = TempWs::new("r3-f7-adv-dag");
    let adv_layers = 13usize;
    for i in 0..adv_layers {
        let next = format!("adv_{:02}.yaml", i + 1);
        ws_adv.write(
            &format!("flows/adv_{i:02}.yaml"),
            &format!(
                "appId: {TEST_APP_ID}\n---\n- runFlow:\n    file: {next}\n    env:\n      BIT_{i}: \"0\"\n- runFlow:\n    file: {next}\n    env:\n      BIT_{i}: \"1\"\n"
            ),
        );
    }
    ws_adv.write(
        &format!("flows/adv_{adv_layers:02}.yaml"),
        &format!("appId: {TEST_APP_ID}\n---\n- assertVisible: Leaf\n"),
    );

    let adv_root = ws_adv.path.join("flows/adv_00.yaml");
    let adv_closure = collect_flow_closure(&ws_adv.path, &adv_root).unwrap();
    let err = deja_cli::commands::exec_common::validate_closure_app_id_confinement(
        &adv_closure,
        TEST_APP_ID,
        &BTreeMap::new(),
    )
    .expect_err("exponentially varying env fan-out must hit visit budget");
    assert!(
        err.contains("maximum visit budget"),
        "expected visit budget error, got: {err}"
    );
}

#[tokio::test]
async fn regression_r3_f8_inspect_resolves_path_like_goals_and_explicit_goal_flag() {
    let ws = TempWs::new("r3-f8-inspect-goal");
    ws.write_build("build/App.bin", b"v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID
        })
        .to_string(),
    );
    ws.write(
        "flows/export.yaml",
        "appId: dev.deja.test.app\n---\n- launchApp\n- assertVisible: Exported\n",
    );
    let ok_tree = hierarchy_with_nodes(TEST_APP_ID, vec![node_with_label("exp", "Exported")]);
    let factory = Arc::new(MockFactory::new(
        Some("ios-27.2"),
        MockDriver::new(vec![ok_tree]),
    ));

    // Remember two goals: one looking like a JSON file path (`flows/export.json`), and one
    // colliding with an existing directory on disk (`reports`).
    for goal_name in ["flows/export.json", "reports"] {
        let (deps, _) = make_deps(
            &ws.path,
            factory.clone(),
            default_probes(),
            CancellationToken::new(),
        );
        assert_eq!(
            run_cli_from_args(
                ["deja", "remember", goal_name, "--flow", "flows/export.yaml"],
                &deps,
            )
            .await,
            0
        );
    }

    // Create a directory named `reports/` on disk!
    fs::create_dir_all(ws.path.join("reports")).unwrap();

    // 1. Without `--goal`: missing path `flows/export.json` falls back to SQLite and succeeds!
    let (deps1, cap1) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(["deja", "inspect", "flows/export.json", "--json"], &deps1).await,
        0
    );
    let j1: Value = serde_json::from_str(&cap1.lock().unwrap().stdout).unwrap();
    assert_eq!(j1["walk"]["goal"], "flows/export.json");

    // 2. Without `--goal`: existing directory `reports/` falls back to SQLite and succeeds!
    let (deps2, cap2) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(["deja", "inspect", "reports", "--json"], &deps2).await,
        0
    );
    let j2: Value = serde_json::from_str(&cap2.lock().unwrap().stdout).unwrap();
    assert_eq!(j2["walk"]["goal"], "reports");

    // 3. Even when an actual file `flows/export.json` exists on disk, `--goal` forces SQLite lookup!
    ws.write("flows/export.json", r#"{"not":"a report"}"#);
    let (deps3, cap3) = make_deps(
        &ws.path,
        factory,
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            ["deja", "inspect", "--goal", "flows/export.json", "--json"],
            &deps3,
        )
        .await,
        0
    );
    let j3: Value = serde_json::from_str(&cap3.lock().unwrap().stdout).unwrap();
    assert_eq!(j3["walk"]["goal"], "flows/export.json");
}

// ===========================================================================
// 3. DEJA-66: Multi-goal batch execution, dynamic `--devices` sharding,
//    LPT scheduling, merged JUnit, and graceful signal cancellation
// ===========================================================================

const TEST_UDID_2: &str = "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE";

type AsyncGoalHook = Arc<
    dyn Fn(
            String,
            String,
            CancellationToken,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

#[derive(Clone)]
struct BatchMockDriver {
    device: String,
    caps: DriverCapabilities,
    executed_log: Arc<Mutex<Vec<(String, String)>>>,
    shutdown_count: Arc<AtomicUsize>,
    current_goal: Arc<Mutex<String>>,
    fail_goals: Arc<Mutex<std::collections::HashSet<String>>>,
    flaky_goals_seen: Arc<Mutex<std::collections::HashMap<String, usize>>>,
    on_goal: Option<AsyncGoalHook>,
}

impl BatchMockDriver {
    fn new(
        device: String,
        executed_log: Arc<Mutex<Vec<(String, String)>>>,
        shutdown_count: Arc<AtomicUsize>,
        fail_goals: Arc<Mutex<std::collections::HashSet<String>>>,
        flaky_goals_seen: Arc<Mutex<std::collections::HashMap<String, usize>>>,
        on_goal: Option<AsyncGoalHook>,
    ) -> Self {
        Self {
            device,
            caps: DriverCapabilities::with([
                DriverCapability::AccessibilityTree,
                DriverCapability::AppCrashReports,
            ]),
            executed_log,
            shutdown_count,
            current_goal: Arc::new(Mutex::new(String::new())),
            fail_goals,
            flaky_goals_seen,
            on_goal,
        }
    }
}

impl PlatformDriver for BatchMockDriver {
    fn platform(&self) -> &'static str {
        "ios"
    }

    fn capabilities(&self) -> &DriverCapabilities {
        &self.caps
    }

    fn ensure_session<'a>(&'a self, cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            Ok(())
        })
    }

    fn launch_app<'a>(
        &'a self,
        _app_id: &'a str,
        _opts: &'a LaunchOptions,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            Ok(())
        })
    }

    fn stop_app<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn kill_app<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn clear_state<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn clear_keychain<'a>(&'a self, _cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn open_link<'a>(
        &'a self,
        _app_id: Option<&'a str>,
        _url: &'a str,
        _opts: &'a OpenLinkOptions,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn add_media<'a>(
        &'a self,
        _paths: &'a [PathBuf],
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn hierarchy<'a>(
        &'a self,
        app_id: &'a str,
        _include_system_ui: bool,
        _known_hash: Option<&'a str>,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, UiHierarchy> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let tag = self.current_goal.lock().unwrap().clone();
            let should_fail = self.fail_goals.lock().unwrap().contains(&tag) || {
                let flaky = self.flaky_goals_seen.lock().unwrap();
                flaky.get(&tag).copied() == Some(1)
            };
            let nodes = if should_fail {
                Vec::new()
            } else {
                vec![node_with_label("done", "Done")]
            };
            Ok(hierarchy_with_nodes(app_id, nodes))
        })
    }

    fn tap_point<'a>(
        &'a self,
        _app_id: &'a str,
        _point: UiPoint,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            Ok(())
        })
    }

    fn long_press_point<'a>(
        &'a self,
        _app_id: &'a str,
        _point: UiPoint,
        _duration_ms: Option<u64>,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn double_tap_point<'a>(
        &'a self,
        _app_id: &'a str,
        _point: UiPoint,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn swipe<'a>(
        &'a self,
        _params: &'a SwipeParams,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn input_text<'a>(
        &'a self,
        _app_id: &'a str,
        text: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let tag = text.to_string();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            *self.current_goal.lock().unwrap() = tag.clone();
            {
                let mut flaky = self.flaky_goals_seen.lock().unwrap();
                if let Some(count) = flaky.get_mut(&tag) {
                    *count += 1;
                }
            }
            self.executed_log
                .lock()
                .unwrap()
                .push((self.device.clone(), tag.clone()));
            if let Some(ref hook) = self.on_goal {
                hook(self.device.clone(), tag, cancel.clone()).await;
            }
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            Ok(())
        })
    }

    fn erase_text<'a>(
        &'a self,
        _app_id: &'a str,
        _count: u32,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn press_key<'a>(
        &'a self,
        _app_id: &'a str,
        _key: &'a KeyPress,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn hide_keyboard<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn back<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn take_screenshot<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Vec<u8>> {
        Box::pin(async { Ok(b"\x89PNG\r\n\x1a\n".to_vec()) })
    }

    fn check_app_crash<'a>(
        &'a self,
        _app_id: &'a str,
        _since_epoch_ms: u64,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Option<DriverCrashSignal>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            Ok(None)
        })
    }

    fn shutdown<'a>(&'a self) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.shutdown_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

struct BatchMockFactory {
    runtime: Option<String>,
    resolve_runtime_calls: Arc<AtomicUsize>,
    prepare_calls: Arc<AtomicUsize>,
    create_calls_by_device: Arc<Mutex<BTreeMap<String, usize>>>,
    install_calls: Arc<AtomicUsize>,
    shutdown_count: Arc<AtomicUsize>,
    executed_log: Arc<Mutex<Vec<(String, String)>>>,
    fail_goals: Arc<Mutex<std::collections::HashSet<String>>>,
    flaky_goals_seen: Arc<Mutex<std::collections::HashMap<String, usize>>>,
    staged_dirs: Arc<Mutex<Vec<PathBuf>>>,
    installed_app_paths: Arc<Mutex<Vec<PathBuf>>>,
    fail_runtime_devices: BTreeMap<String, String>,
    fail_create_devices: BTreeMap<String, String>,
    create_delay_by_device: BTreeMap<String, Duration>,
    stage_real_ios_bundle: bool,
    on_goal: Option<AsyncGoalHook>,
}

impl BatchMockFactory {
    fn new(runtime: Option<&str>) -> Self {
        Self {
            runtime: runtime.map(ToString::to_string),
            resolve_runtime_calls: Arc::new(AtomicUsize::new(0)),
            prepare_calls: Arc::new(AtomicUsize::new(0)),
            create_calls_by_device: Arc::new(Mutex::new(BTreeMap::new())),
            install_calls: Arc::new(AtomicUsize::new(0)),
            shutdown_count: Arc::new(AtomicUsize::new(0)),
            executed_log: Arc::new(Mutex::new(Vec::new())),
            fail_goals: Arc::new(Mutex::new(std::collections::HashSet::new())),
            flaky_goals_seen: Arc::new(Mutex::new(std::collections::HashMap::new())),
            staged_dirs: Arc::new(Mutex::new(Vec::new())),
            installed_app_paths: Arc::new(Mutex::new(Vec::new())),
            fail_runtime_devices: BTreeMap::new(),
            fail_create_devices: BTreeMap::new(),
            create_delay_by_device: BTreeMap::new(),
            stage_real_ios_bundle: false,
            on_goal: None,
        }
    }

    fn with_on_goal(mut self, hook: AsyncGoalHook) -> Self {
        self.on_goal = Some(hook);
        self
    }

    fn with_real_ios_staging(mut self) -> Self {
        self.stage_real_ios_bundle = true;
        self
    }

    fn add_failing_goal(&self, tag: &str) {
        self.fail_goals.lock().unwrap().insert(tag.to_string());
    }

    fn add_flaky_goal(&self, tag: &str) {
        self.flaky_goals_seen
            .lock()
            .unwrap()
            .insert(tag.to_string(), 0);
    }

    fn total_create_calls(&self) -> usize {
        self.create_calls_by_device.lock().unwrap().values().sum()
    }
}

impl DriverFactory for BatchMockFactory {
    fn resolve_runtime<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Option<String>, DriverFactoryError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverFactoryError::Cancelled);
            }
            self.resolve_runtime_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(err_msg) = self.fail_runtime_devices.get(&ctx.device) {
                return Err(DriverFactoryError::Runtime(err_msg.clone()));
            }
            Ok(self.runtime.clone())
        })
    }

    fn prepare_batch_build<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Option<SharedStagedBuild>, DriverFactoryError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverFactoryError::Cancelled);
            }
            self.prepare_calls.fetch_add(1, Ordering::SeqCst);
            if self.stage_real_ios_bundle && ctx.platform == "ios" {
                let staged = stage_and_verify_ios_app_bundle(
                    &ctx.build_path,
                    &ctx.app_id,
                    ctx.expected_build_fingerprint.as_deref(),
                )
                .map_err(DriverFactoryError::UnsupportedOrConfig)?;
                self.staged_dirs
                    .lock()
                    .unwrap()
                    .push(staged.staging_dir().to_path_buf());
                return Ok(Some(SharedStagedBuild::new(staged)));
            }
            Ok(None)
        })
    }

    fn create<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Box<dyn PlatformDriver>, DriverFactoryError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverFactoryError::Cancelled);
            }
            if let Some(&delay) = self.create_delay_by_device.get(&ctx.device) {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DriverFactoryError::Cancelled),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            if let Some(err_msg) = self.fail_create_devices.get(&ctx.device) {
                return Err(DriverFactoryError::Runtime(err_msg.clone()));
            }
            if self.stage_real_ios_bundle {
                assert!(
                    ctx.staged_build.is_some(),
                    "batch worker create() must receive shared staged_build"
                );
            }
            *self
                .create_calls_by_device
                .lock()
                .unwrap()
                .entry(ctx.device.clone())
                .or_insert(0) += 1;
            Ok(Box::new(BatchMockDriver::new(
                ctx.device.clone(),
                Arc::clone(&self.executed_log),
                Arc::clone(&self.shutdown_count),
                Arc::clone(&self.fail_goals),
                Arc::clone(&self.flaky_goals_seen),
                self.on_goal.clone(),
            )) as Box<dyn PlatformDriver>)
        })
    }

    fn install_app<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<(), DriverFactoryError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverFactoryError::Cancelled);
            }
            self.install_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(ref staged) = ctx.staged_build {
                assert!(
                    staged.app_path().exists(),
                    "shared staged .app must remain alive during install_app"
                );
                self.installed_app_paths
                    .lock()
                    .unwrap()
                    .push(staged.app_path().to_path_buf());
            }
            Ok(())
        })
    }
}

fn write_tagged_flow(ws: &TempWs, rel: &str, tag: &str) -> PathBuf {
    ws.write(
        rel,
        &format!(
            "appId: {TEST_APP_ID}\n---\n- launchApp\n- inputText: {tag:?}\n- tapOn:\n    id: done\n- assertVisible: Done\n"
        ),
    )
}

fn seed_manifest_walk(
    ws: &TempWs,
    entries: &[(&str, &str, &str, &str, &str)], // (goal, app_id, platform, adapter, flow_rel)
) {
    let mut walks = Vec::new();
    for &(goal, app_id, platform, adapter, flow_rel) in entries {
        let abs = ws.path.join(flow_rel);
        let closure = collect_flow_closure(&ws.path, &abs).unwrap();
        let assert_sha = assertions_sha256(&closure).unwrap();
        let mut obj = json!({
            "goal": goal,
            "appId": app_id,
            "platform": platform,
            "adapter": adapter,
            "flow": flow_rel,
            "closureSha256": closure.sha256,
            "assertionsSha256": assert_sha,
        });
        if platform == "web" {
            obj["browser"] = json!("chromium");
        }
        walks.push(obj);
    }
    let manifest = json!({
        "manifestVersion": 1,
        "hashVersion": 1,
        "walks": walks,
    });
    ws.write(
        "deja.walks.json",
        &serde_json::to_string_pretty(&manifest).unwrap(),
    );
}

#[tokio::test]
async fn batch_cli_goals_formats_all_filtering_and_validation_rejections() {
    let ws = TempWs::new("deja66-validation");
    ws.write_build("build/App.bin", b"build-v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "manifest": "deja.walks.json",
            "staleBuild": "replay-last-verified"
        })
        .to_string(),
    );

    write_tagged_flow(&ws, "flows/g1.yaml", "g1");
    write_tagged_flow(&ws, "flows/g2.yaml", "g2");
    ws.write(
        "flows/android_only.yaml",
        "appId: dev.deja.android.app\n---\n- launchApp\n- assertVisible: Android\n",
    );
    seed_manifest_walk(
        &ws,
        &[
            (
                "goal one",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/g1.yaml",
            ),
            (
                "goal two",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/g2.yaml",
            ),
            (
                "android only",
                "dev.deja.android.app",
                "android",
                "maestro-android/v1",
                "flows/android_only.yaml",
            ),
        ],
    );

    let goals_txt = ws.write("goals.txt", "# comment\n\ngoal one\ngoal two\n");
    let goals_txt_before = fs::read_to_string(&goals_txt).unwrap();
    ws.write("goals.json", r#"["goal one", "goal two"]"#);
    ws.write("dup_goals.txt", "Goal One\n  goal   one  \n");

    let factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")));
    let run_with_args = |argv: Vec<&str>, env_extra: Vec<(&str, &str)>| {
        let ws_path = ws.path.clone();
        let f = Arc::clone(&factory);
        let argv_owned: Vec<String> = argv.into_iter().map(ToString::to_string).collect();
        let env_owned: Vec<(String, String)> = env_extra
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        async move {
            let (mut deps, cap) =
                make_deps(&ws_path, f, default_probes(), CancellationToken::new());
            for (k, v) in env_owned {
                deps.env.insert(k, v);
            }
            let code = run_cli_from_args(argv_owned, &deps).await;
            let out = cap.lock().unwrap().clone();
            (code, out)
        }
    };

    // 1. All invalid flag combinations fail closed exit 2 with ZERO driver attach.
    let bad_cases: Vec<Vec<&str>> = vec![
        vec!["deja", "run"],
        vec!["deja", "run", "goal one", "--goals", "goals.txt"],
        vec!["deja", "run", "goal one", "--all"],
        vec!["deja", "run", "--goals", "goals.txt", "--all"],
        vec!["deja", "run", "goal one", "--devices", TEST_UDID],
        vec![
            "deja",
            "run",
            "--goals",
            "goals.txt",
            "--device",
            TEST_UDID,
            "--devices",
            TEST_UDID,
        ],
        vec![
            "deja",
            "run",
            "--goals",
            "goals.txt",
            "--store",
            "s1",
            "--devices",
            TEST_UDID,
        ],
        vec!["deja", "run", "--goals", "goals.txt", "--devices", ""],
        vec![
            "deja",
            "run",
            "--goals",
            "goals.txt",
            "--devices",
            "11111111-2222-3333-4444-555555555555,,AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE",
        ],
        vec![
            "deja",
            "run",
            "--goals",
            "goals.txt",
            "--devices",
            "11111111-2222-3333-4444-555555555555,not-a-udid",
        ],
        vec!["deja", "run", "--goals", "dup_goals.txt"],
        vec![
            "deja",
            "run",
            "--goals",
            "goals.txt",
            "--report",
            "goals.txt",
        ],
        vec![
            "deja",
            "run",
            "--goals",
            "goals.txt",
            "--junit",
            "goals.txt",
        ],
    ];
    for case in bad_cases {
        let (code, out) = run_with_args(case.clone(), vec![]).await;
        assert_eq!(
            code, 2,
            "expected exit 2 for {case:?}, got {code} (stdout={}, stderr={})",
            out.stdout, out.stderr
        );
    }
    assert_eq!(
        factory.total_create_calls(),
        0,
        "invalid invocations must never attach a driver"
    );
    assert_eq!(
        fs::read_to_string(&goals_txt).unwrap(),
        goals_txt_before,
        "--report/--junit targeting goals.txt must not touch disk"
    );

    // Also reject bidi char in `--devices`.
    let bidi_dev = format!("{TEST_UDID}\u{202E}");
    let (code_bidi, _) = run_with_args(
        vec![
            "deja",
            "run",
            "--goals",
            "goals.txt",
            "--devices",
            &bidi_dev,
        ],
        vec![],
    )
    .await;
    assert_eq!(code_bidi, 2);

    // Reject `--all` when `manifest` is absent from config.
    ws.write(
        "no_manifest.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "buildPath": "build/App.bin",
            "device": TEST_UDID
        })
        .to_string(),
    );
    let (code_no_man, _) = run_with_args(
        vec![
            "deja",
            "--config",
            "no_manifest.config.json",
            "run",
            "--all",
        ],
        vec![],
    )
    .await;
    assert_eq!(code_no_man, 2);

    // Reject `--all` when manifest has zero entries matching config.
    ws.write(
        "other_app.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": "dev.deja.other.app",
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "manifest": "deja.walks.json"
        })
        .to_string(),
    );
    let (code_empty_all, _) = run_with_args(
        vec!["deja", "--config", "other_app.config.json", "run", "--all"],
        vec![],
    )
    .await;
    assert_eq!(code_empty_all, 2);

    // 2. `--goals` supports plain text, JSON array, and `deja.walks.json` (plus `--all` filters
    // out the Android entry!), and `--devices` overrides invalid/empty `DEJA_DEVICE`!
    for (mode_name, argv) in [
        (
            "text",
            vec!["deja", "run", "--goals", "goals.txt", "--json"],
        ),
        (
            "json",
            vec!["deja", "run", "--goals", "goals.json", "--json"],
        ),
        ("all", vec!["deja", "run", "--all", "--json"]),
    ] {
        let (code, out) = run_with_args(argv, vec![]).await;
        assert_eq!(code, 0, "batch {mode_name} failed: {}", out.stderr);
        let parsed: Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(parsed["mode"], "batch");
        assert_eq!(parsed["passed"], true);
        let goals = parsed["goals"].as_array().unwrap();
        assert_eq!(goals.len(), 2, "expected 2 iOS goals for {mode_name}");
        assert_eq!(goals[0]["goal"], "goal one");
        assert_eq!(goals[1]["goal"], "goal two");
    }

    // Also test `--goals` directly reading a manifest JSON file (single-platform manifest).
    seed_manifest_walk(
        &ws,
        &[
            (
                "goal one",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/g1.yaml",
            ),
            (
                "goal two",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/g2.yaml",
            ),
        ],
    );
    let devices_arg = format!("{},{}", TEST_UDID.to_lowercase(), TEST_UDID_2);
    let (code_man, out_man) = run_with_args(
        vec![
            "deja",
            "run",
            "--goals",
            "deja.walks.json",
            "--devices",
            &devices_arg,
            "--json",
        ],
        vec![("DEJA_DEVICE", "   ")], // overridden by `--devices`!
    )
    .await;
    assert_eq!(code_man, 0, "stderr: {}", out_man.stderr);
    let parsed_man: Value = serde_json::from_str(&out_man.stdout).unwrap();
    assert_eq!(parsed_man["devices"], json!([TEST_UDID, TEST_UDID_2]));
}

#[tokio::test]
async fn batch_dynamic_devices_sharding_non_stalling_and_persistent_driver_reuse() {
    let ws = TempWs::new("deja66-dynamic-sharding");
    ws.write_build("build/App.bin", b"build-v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "manifest": "deja.walks.json",
            "staleBuild": "replay-last-verified"
        })
        .to_string(),
    );

    for tag in ["g1", "g2", "g3", "g4"] {
        write_tagged_flow(&ws, &format!("flows/{tag}.yaml"), tag);
    }
    seed_manifest_walk(
        &ws,
        &[
            ("g1", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g1.yaml"),
            ("g2", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g2.yaml"),
            ("g3", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g3.yaml"),
            ("g4", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g4.yaml"),
        ],
    );

    // Deterministic non-stalling proof via `Arc<tokio::sync::Notify>` (ZERO sleep flakiness!):
    // Whichever shard pulls `g1` blocks on `release_g1`; the OTHER shard must dynamically pull
    // and execute ALL 3 remaining goals (`g2`, `g3`, `g4`) before releasing `g1`.
    let release_g1 = Arc::new(tokio::sync::Notify::new());
    let fast_done = Arc::new(AtomicUsize::new(0));
    let release_clone = Arc::clone(&release_g1);
    let fast_done_clone = Arc::clone(&fast_done);

    let hook: AsyncGoalHook = Arc::new(move |_device, tag, _cancel| {
        let release = Arc::clone(&release_clone);
        let done = Arc::clone(&fast_done_clone);
        Box::pin(async move {
            if tag == "g1" {
                release.notified().await;
            } else if done.fetch_add(1, Ordering::SeqCst) + 1 == 3 {
                release.notify_one();
            }
        })
    });

    let factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")).with_on_goal(hook));
    let (deps, cap) = make_deps(
        &ws.path,
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let devices_arg = format!("{TEST_UDID},{TEST_UDID_2}");
    let code = tokio::time::timeout(
        Duration::from_secs(10),
        run_cli_from_args(
            ["deja", "run", "--all", "--devices", &devices_arg, "--json"],
            &deps,
        ),
    )
    .await
    .expect("dynamic sharding must not deadlock/stall on slow worker");
    assert_eq!(code, 0, "stderr: {}", cap.lock().unwrap().stderr);

    let parsed: Value = serde_json::from_str(&cap.lock().unwrap().stdout).unwrap();
    assert_eq!(parsed["passed"], true);
    let shards = parsed["shards"].as_array().unwrap();
    let mut counts: Vec<u64> = shards
        .iter()
        .map(|s| s["goalsExecuted"].as_u64().unwrap())
        .collect();
    counts.sort_unstable();
    assert_eq!(
        counts,
        vec![1, 3],
        "expected dynamic 1/3 split across 2 devices, got {counts:?}"
    );

    // Persistent single attach per shard across 4 goals (`attach_count == 1` per worker!).
    let creates = factory.create_calls_by_device.lock().unwrap().clone();
    assert_eq!(creates.get(TEST_UDID), Some(&1));
    assert_eq!(creates.get(TEST_UDID_2), Some(&1));
    assert_eq!(factory.shutdown_count.load(Ordering::SeqCst), 2);
    assert_eq!(factory.resolve_runtime_calls.load(Ordering::SeqCst), 2);

    // Refused-only batch must NEVER attach a driver or stage the build (`attach_count == 0`).
    ws.write(
        "unknown_goals.txt",
        "unremembered alpha\nunremembered beta\n",
    );
    let refuse_factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")));
    let (deps_refuse, _) = make_deps(
        &ws.path,
        Arc::clone(&refuse_factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let refuse_code = run_cli_from_args(
        [
            "deja",
            "run",
            "--goals",
            "unknown_goals.txt",
            "--devices",
            &devices_arg,
            "--json",
        ],
        &deps_refuse,
    )
    .await;
    assert_ne!(refuse_code, 0);
    assert_eq!(
        refuse_factory.total_create_calls(),
        0,
        "all-refused batch must have attach_count == 0"
    );
    assert_eq!(
        refuse_factory.prepare_calls.load(Ordering::SeqCst),
        0,
        "all-refused batch must never stage the build"
    );
}

#[tokio::test]
async fn batch_lpt_scheduling_order_single_fingerprint_and_single_app_staging() {
    let ws = TempWs::new("deja66-lpt-stage");
    // Build a real iOS `.app` bundle directory so staging & tree fingerprinting exercise real code.
    let app_dir = ws.path.join("build/Demo.app");
    fs::create_dir_all(&app_dir).unwrap();
    ws.write(
        "build/Demo.app/Info.plist",
        &format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{TEST_APP_ID}</string></dict></plist>\n"
        ),
    );
    ws.write_build("build/Demo.app/Demo", b"ios-macho-binary-v1");
    let expected_fp = fingerprint_tree(&ws.path, &app_dir, "ios").unwrap();

    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/Demo.app",
            "device": TEST_UDID,
            "manifest": "deja.walks.json",
            "staleBuild": "replay-last-verified"
        })
        .to_string(),
    );

    for tag in ["fast", "unseen", "slow", "medium"] {
        write_tagged_flow(&ws, &format!("flows/{tag}.yaml"), tag);
    }
    seed_manifest_walk(
        &ws,
        &[
            (
                "fast",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/fast.yaml",
            ),
            (
                "unseen",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/unseen.yaml",
            ),
            (
                "slow",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/slow.yaml",
            ),
            (
                "medium",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/medium.yaml",
            ),
        ],
    );

    // Seed historical durations in default store: slow=5000ms, medium=500ms, fast=100ms, unseen=none.
    let store = SqliteWalkStore::open_writable(SqliteStoreOptions::new(&ws.path)).unwrap();
    for (goal_name, flow_rel, dur_ms) in [
        ("fast", "flows/fast.yaml", 100u64),
        ("medium", "flows/medium.yaml", 500u64),
        ("slow", "flows/slow.yaml", 5000u64),
    ] {
        let abs = ws.path.join(flow_rel);
        let cl = collect_flow_closure(&ws.path, &abs).unwrap();
        let main_sha = cl
            .files
            .iter()
            .find(|f| f.path == cl.main)
            .unwrap()
            .sha256
            .clone();
        store
            .upsert_walk(&deja_store::UpsertWalkInput {
                identity: WalkIdentity {
                    goal: goal_name.to_string(),
                    app_id: TEST_APP_ID.to_string(),
                    platform: "ios".to_string(),
                    flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
                    build_fingerprint: expected_fp.clone(),
                    adapter: Some("maestro-ios/v1".to_string()),
                    browser: None,
                },
                flow_path: flow_rel.to_string(),
                flow_sha256: main_sha,
                subflow_files: None,
                legacy_closure: false,
                closure_sha256: Some(cl.sha256),
                validation: RecordValidationInput::new(true, now_iso8601())
                    .with_runtime("ios-27.2")
                    .with_duration_ms(dur_ms),
                exploration: None,
                repair_history: None,
            })
            .unwrap();
    }
    store.close();

    // 1. Single-shard LPT scheduling proof: input order is [fast, unseen, slow, medium],
    // scheduledOrder & actual execution order MUST be [slow, medium, fast, unseen],
    // while JSON `goals` and `--summary` lines remain in original input order!
    ws.write("lpt_goals.txt", "fast\nunseen\nslow\nmedium\n");
    let lpt_factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")));
    let (deps_lpt, cap_lpt) = make_deps(
        &ws.path,
        Arc::clone(&lpt_factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let code_lpt = run_cli_from_args(
        [
            "deja",
            "run",
            "--goals",
            "lpt_goals.txt",
            "--json",
            "--summary",
        ],
        &deps_lpt,
    )
    .await;
    assert_eq!(code_lpt, 0, "stderr: {}", cap_lpt.lock().unwrap().stderr);
    let out_lpt = cap_lpt.lock().unwrap().clone();
    let parsed_lpt: Value = serde_json::from_str(&out_lpt.stdout).unwrap();
    assert_eq!(
        parsed_lpt["scheduledOrder"],
        json!(["slow", "medium", "fast", "unseen"])
    );
    let exec_order: Vec<String> = lpt_factory
        .executed_log
        .lock()
        .unwrap()
        .iter()
        .map(|(_, tag)| tag.clone())
        .collect();
    assert_eq!(exec_order, vec!["slow", "medium", "fast", "unseen"]);

    let out_goals: Vec<&str> = parsed_lpt["goals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["goal"].as_str().unwrap())
        .collect();
    assert_eq!(out_goals, vec!["fast", "unseen", "slow", "medium"]);

    // In `--json --summary` mode, summary lines are emitted to stderr in deterministic input order.
    let summary_lines: Vec<&str> = out_lpt
        .stderr
        .lines()
        .filter(|l| l.starts_with("deja-summary v=1 "))
        .collect();
    assert_eq!(summary_lines.len(), 4);
    assert!(summary_lines[0].ends_with(" goal=fast"));
    assert!(summary_lines[1].ends_with(" goal=unseen"));
    assert!(summary_lines[2].ends_with(" goal=slow"));
    assert!(summary_lines[3].ends_with(" goal=medium"));

    // 2. Single `.app` staging + single build-fingerprint broadcast across `--devices`, with
    // verified RAII temp staging directory cleanup after batch returns!
    let stage_factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")).with_real_ios_staging());
    let (deps_stage, cap_stage) = make_deps(
        &ws.path,
        Arc::clone(&stage_factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let devices_arg = format!("{TEST_UDID},{TEST_UDID_2}");
    let code_stage = run_cli_from_args(
        [
            "deja",
            "run",
            "--goals",
            "lpt_goals.txt",
            "--devices",
            &devices_arg,
            "--json",
        ],
        &deps_stage,
    )
    .await;
    assert_eq!(
        code_stage,
        0,
        "stderr: {}",
        cap_stage.lock().unwrap().stderr
    );
    let parsed_stage: Value = serde_json::from_str(&cap_stage.lock().unwrap().stdout).unwrap();
    assert_eq!(parsed_stage["buildFingerprint"], expected_fp);
    assert_eq!(
        stage_factory.prepare_calls.load(Ordering::SeqCst),
        1,
        "iOS `.app` bundle must be staged ONCE across all shards"
    );
    let installed_paths = stage_factory.installed_app_paths.lock().unwrap().clone();
    assert!(
        !installed_paths.is_empty(),
        "at least one shard installed the staged bundle"
    );
    assert!(
        installed_paths.iter().all(|p| p == &installed_paths[0]),
        "every shard must reuse the exact same staged `.app` path"
    );
    let staged_dirs = stage_factory.staged_dirs.lock().unwrap().clone();
    assert_eq!(staged_dirs.len(), 1);
    assert!(
        !staged_dirs[0].exists(),
        "temporary `.app` staging directory must be cleaned up after batch returns: {}",
        staged_dirs[0].display()
    );
}

#[tokio::test]
async fn batch_partial_failure_exit_priority_gate_interop_and_mixed_case_secret_redaction() {
    let ws = TempWs::new("deja66-partial-gate-redact");
    ws.write_build("build/App.bin", b"build-v1");
    let mixed_secret = "MixedCaseSecret99";
    let lower_secret = mixed_secret.to_lowercase();
    let api_key = ["sk", "-1234567890abcdefghijklmnop"].concat();

    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "manifest": "deja.walks.json",
            "staleBuild": "replay-last-verified",
            "fixtureCredentials": [
                { "label": "password", "value": mixed_secret }
            ]
        })
        .to_string(),
    );

    let secret_goal = format!("Checkout {mixed_secret} {api_key}");
    write_tagged_flow(&ws, "flows/ok1.yaml", "ok1");
    write_tagged_flow(&ws, "flows/ok2.yaml", "ok2");
    write_tagged_flow(&ws, "flows/flaky.yaml", "flaky");
    write_tagged_flow(&ws, "flows/fail.yaml", "fail");
    seed_manifest_walk(
        &ws,
        &[
            (
                &secret_goal,
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/ok1.yaml",
            ),
            (
                "ok two",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/ok2.yaml",
            ),
            (
                "flaky goal",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/flaky.yaml",
            ),
            (
                "fail goal",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/fail.yaml",
            ),
        ],
    );

    // 1. Passing multi-device batch + `deja gate --store <UDID1> --store <UDID2>` interop +
    // mixed-case secret & provider-key redaction across JSON, --report, --summary, and --junit!
    ws.write("pass_goals.txt", &format!("{secret_goal}\nok two\n"));
    let devices_arg = format!("{TEST_UDID},{TEST_UDID_2}");
    let pass_factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")));
    let (mut deps_pass, cap_pass) = make_deps(
        &ws.path,
        Arc::clone(&pass_factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    deps_pass
        .env
        .insert("OPENAI_API_KEY".to_string(), api_key.clone());

    let code_pass = run_cli_from_args(
        [
            "deja",
            "run",
            "--goals",
            "pass_goals.txt",
            "--devices",
            &devices_arg,
            "--report",
            "out/batch.json",
            "--junit",
            "out/batch.xml",
            "--json",
            "--summary",
        ],
        &deps_pass,
    )
    .await;
    assert_eq!(code_pass, 0, "stderr: {}", cap_pass.lock().unwrap().stderr);

    let pass_out = cap_pass.lock().unwrap().clone();
    let batch_report_disk = fs::read_to_string(ws.path.join("out/batch.json")).unwrap();
    let batch_latest_disk =
        fs::read_to_string(ws.path.join(".deja/reports/batch-latest.json")).unwrap();
    let batch_junit_disk = fs::read_to_string(ws.path.join("out/batch.xml")).unwrap();
    for (label, text) in [
        ("stdout", &pass_out.stdout),
        ("stderr", &pass_out.stderr),
        ("out/batch.json", &batch_report_disk),
        ("batch-latest.json", &batch_latest_disk),
        ("out/batch.xml", &batch_junit_disk),
    ] {
        assert!(
            !text.contains(mixed_secret),
            "mixed-case secret leaked in {label}: {text}"
        );
        assert!(
            !text.contains(&lower_secret),
            "lowercased secret leaked in {label}: {text}"
        );
        assert!(
            !text.contains(&api_key),
            "provider API key leaked in {label}: {text}"
        );
    }
    let parsed_pass: Value = serde_json::from_str(&pass_out.stdout).unwrap();
    assert!(
        parsed_pass["goals"][0]["goalKey"]
            .as_str()
            .unwrap()
            .contains("<redacted-fixture-value>")
    );

    // Run `deja gate --goals pass_goals.txt --store <UDID1> --store <UDID2> --json` -> exit 0!
    let (deps_gate, cap_gate) = make_deps(
        &ws.path,
        Arc::clone(&pass_factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let gate_code = run_cli_from_args(
        [
            "deja",
            "gate",
            "--goals",
            "pass_goals.txt",
            "--store",
            TEST_UDID,
            "--store",
            TEST_UDID_2,
            "--json",
        ],
        &deps_gate,
    )
    .await;
    assert_eq!(
        gate_code,
        0,
        "gate stderr: {}",
        cap_gate.lock().unwrap().stderr
    );
    let gate_json: Value = serde_json::from_str(&cap_gate.lock().unwrap().stdout).unwrap();
    assert_eq!(gate_json["verdict"], "passed");

    // 2. Partial failure continues remaining goals + exit code priority:
    //    a) flaky + pass -> exit 7 (`EXIT_FLAKY`)
    ws.write("flaky_batch.txt", "ok two\nflaky goal\n");
    let flaky_factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")));
    flaky_factory.add_flaky_goal("flaky");
    let (deps_flaky, _cap_flaky) = make_deps(
        &ws.path,
        Arc::clone(&flaky_factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let code_flaky = run_cli_from_args(
        [
            "deja",
            "run",
            "--goals",
            "flaky_batch.txt",
            "--devices",
            &devices_arg,
            "--rerun-failed",
            "--junit",
            "out/flaky.xml",
            "--json",
        ],
        &deps_flaky,
    )
    .await;
    assert_eq!(code_flaky, 7, "flaky-only batch must exit 7");
    let flaky_xml = fs::read_to_string(ws.path.join("out/flaky.xml")).unwrap();
    assert!(deja_flow::reports_failure(&flaky_xml));
    assert!(flaky_xml.contains("outcome: flaky"));
    assert!(flaky_xml.contains("attempt 1: failed"));
    assert!(flaky_xml.contains("attempt 2: passed"));

    // `deja gate` on flaky_batch.txt MUST close (exit 10) because `flaky goal` was flaky!
    let (deps_gate2, _) = make_deps(
        &ws.path,
        Arc::clone(&flaky_factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let gate_code2 = run_cli_from_args(
        [
            "deja",
            "gate",
            "--goals",
            "flaky_batch.txt",
            "--store",
            TEST_UDID,
            "--store",
            TEST_UDID_2,
            "--json",
        ],
        &deps_gate2,
    )
    .await;
    assert_eq!(
        gate_code2, 10,
        "gate must close when any shard recorded flaky"
    );

    //    b) fail + pass + flaky -> fail continues remaining goals, exit 1 beats exit 7!
    ws.write("mixed_batch.txt", "fail goal\nok two\nflaky goal\n");
    let mixed_factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")));
    mixed_factory.add_failing_goal("fail");
    mixed_factory.add_flaky_goal("flaky");
    let (deps_mixed, cap_mixed) = make_deps(
        &ws.path,
        Arc::clone(&mixed_factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let code_mixed = run_cli_from_args(
        [
            "deja",
            "run",
            "--goals",
            "mixed_batch.txt",
            "--devices",
            &devices_arg,
            "--rerun-failed",
            "--json",
        ],
        &deps_mixed,
    )
    .await;
    assert_eq!(code_mixed, 1, "failure (1) must outrank flaky (7)");
    let parsed_mixed: Value = serde_json::from_str(&cap_mixed.lock().unwrap().stdout).unwrap();
    assert_eq!(parsed_mixed["counts"]["passed"], 1);
    assert_eq!(parsed_mixed["counts"]["failed"], 1);
    assert_eq!(parsed_mixed["counts"]["flaky"], 1);
    assert_eq!(
        parsed_mixed["goals"][1]["outcome"], "passed",
        "partial failure must still execute remaining goals"
    );
}

#[tokio::test]
async fn batch_in_process_cancellation_teardown_locks_sqlite_and_junit() {
    let ws = TempWs::new("deja66-cancel-inproc");
    ws.write_build("build/App.bin", b"build-v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "manifest": "deja.walks.json",
            "staleBuild": "replay-last-verified"
        })
        .to_string(),
    );

    for tag in ["g1", "g2", "g3"] {
        write_tagged_flow(&ws, &format!("flows/{tag}.yaml"), tag);
    }
    seed_manifest_walk(
        &ws,
        &[
            ("g1", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g1.yaml"),
            ("g2", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g2.yaml"),
            ("g3", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g3.yaml"),
        ],
    );
    ws.write("goals.txt", "g1\ng2\ng3\n");

    let cancel = CancellationToken::new();
    let cancel_for_hook = cancel.clone();
    let hook: AsyncGoalHook = Arc::new(move |_device, tag, _token| {
        let cancel_trigger = cancel_for_hook.clone();
        Box::pin(async move {
            if tag == "g2" {
                cancel_trigger.cancel();
            }
        })
    });

    let factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")).with_on_goal(hook));
    let (deps, cap) = make_deps(
        &ws.path,
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        default_probes(),
        cancel,
    );
    let code = run_cli_from_args(
        [
            "deja",
            "run",
            "--goals",
            "goals.txt",
            "--junit",
            "out/cancelled.xml",
            "--json",
            "--summary",
        ],
        &deps,
    )
    .await;
    assert_eq!(code, 1, "token-cancel must exit 1");

    // Driver shutdown MUST run cleanly and zero `.lock` files may remain.
    assert_eq!(factory.shutdown_count.load(Ordering::SeqCst), 1);
    assert!(
        !ws.path.join(".deja/memory/walks.sqlite.lock").exists(),
        "stale SQLite writer lock must not remain after cancellation"
    );

    let out = cap.lock().unwrap().clone();
    let parsed: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(parsed["cancelled"], true);
    assert_eq!(parsed["outcome"], "cancelled");
    assert_eq!(parsed["goals"][0]["outcome"], "passed");
    assert_eq!(parsed["goals"][1]["outcome"], "cancelled");
    assert_eq!(parsed["goals"][2]["outcome"], "skipped");

    // Verify SQLite: `g1` WAS persisted, `g2` (cancelled mid-flight) was NOT persisted!
    let fp = fingerprint_file(&ws.path, &ws.path.join("build/App.bin")).unwrap();
    let ro = SqliteWalkStore::open_readonly(SqliteStoreOptions::new(&ws.path)).unwrap();
    let g1_runs = ro
        .list_validation_runs(&WalkIdentity {
            goal: "g1".to_string(),
            app_id: TEST_APP_ID.to_string(),
            platform: "ios".to_string(),
            flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
            build_fingerprint: fp.clone(),
            adapter: Some("maestro-ios/v1".to_string()),
            browser: None,
        })
        .unwrap();
    assert_eq!(g1_runs.len(), 1, "completed g1 must be persisted in SQLite");
    let g2_runs = ro
        .list_validation_runs(&WalkIdentity {
            goal: "g2".to_string(),
            app_id: TEST_APP_ID.to_string(),
            platform: "ios".to_string(),
            flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
            build_fingerprint: fp,
            adapter: Some("maestro-ios/v1".to_string()),
            browser: None,
        })
        .unwrap();
    assert!(
        g2_runs.is_empty(),
        "cancelled g2 must NOT write aborted validation row to SQLite"
    );

    // Merged JUnit contains g1 pass + g2 <error type="Cancelled"> + g3 <skipped status="SKIPPED">.
    let xml = fs::read_to_string(ws.path.join("out/cancelled.xml")).unwrap();
    assert!(deja_flow::reports_failure(&xml));
    assert!(xml.contains(r#"<testsuites tests="3" failures="0" errors="1" skipped="1""#));
    assert!(xml.contains(r#"<error type="Cancelled""#));
    assert!(xml.contains(r#"status="SKIPPED""#));
    assert!(xml.contains(r#"<skipped message="cancelled before execution"/>"#));

    // `--summary` lines MUST strictly obey the `deja-summary v=1` grammar (`outcome=error` and
    // `failure=error` when no `RunResult` exists; never non-v1 `outcome=cancelled`/`skipped` or
    // `failure=cancelled`/`skipped`).
    let summary_lines: Vec<&str> = out
        .stderr
        .lines()
        .filter(|l| l.starts_with("deja-summary v=1 "))
        .collect();
    assert_eq!(
        summary_lines.len(),
        3,
        "expected 3 summary lines, got: {summary_lines:?}"
    );
    assert!(
        summary_lines[0].contains("outcome=passed ")
            && summary_lines[0].contains(" failure=none goal=g1"),
        "unexpected g1 summary line: {}",
        summary_lines[0]
    );
    for (idx, goal) in [(1usize, "g2"), (2usize, "g3")] {
        let line = summary_lines[idx];
        assert!(
            line.contains("outcome=error ")
                && line.contains(" cache=not-checked ")
                && line.contains(" model_calls=0 ")
                && line.contains(&format!(" failure=error goal={goal}")),
            "cancelled/skipped goal {goal} must emit strict v1 outcome=error failure=error summary, got: {line}"
        );
        assert!(
            !line.contains("cancelled") && !line.contains("skipped"),
            "deja-summary v=1 must not emit non-v1 cancelled/skipped tokens: {line}"
        );
    }
}

#[cfg(unix)]
#[test]
fn subprocess_batch_sigterm_exits_143_cleans_locks_and_writes_failing_junit() {
    use std::os::unix::fs::PermissionsExt;

    let ws = TempWs::new("deja66-sigterm");
    ws.write_build("build/App.bin", b"build-v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "manifest": "deja.walks.json",
            "staleBuild": "replay-last-verified"
        })
        .to_string(),
    );
    write_tagged_flow(&ws, "flows/g1.yaml", "g1");
    write_tagged_flow(&ws, "flows/g2.yaml", "g2");
    seed_manifest_walk(
        &ws,
        &[
            ("g1", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g1.yaml"),
            ("g2", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g2.yaml"),
        ],
    );
    ws.write("goals.txt", "g1\ng2\n");

    // Fake `xcrun` in temp PATH: writes a ready marker once BOTH shard store locks are already
    // held by `deja`, then sleeps until `deja` handles SIGTERM and drops the child.
    let bin_dir = ws.path.join("fake-bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let ready_marker = ws.path.join("xcrun.ready");
    let fake_xcrun = bin_dir.join("xcrun");
    fs::write(
        &fake_xcrun,
        format!(
            "#!/bin/sh\ntouch '{}'\nexec sleep 30\n",
            ready_marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_xcrun, fs::Permissions::from_mode(0o755)).unwrap();

    let orig_path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());
    let custom_path = format!("{}:{orig_path}", bin_dir.display());
    let devices_arg = format!("{TEST_UDID},{TEST_UDID_2}");

    let child = deja_bin()
        .current_dir(&ws.path)
        .env("PATH", &custom_path)
        .args([
            "run",
            "--goals",
            "goals.txt",
            "--devices",
            &devices_arg,
            "--junit",
            "out/sigterm.xml",
            "--json",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn deja subprocess");

    let lock1 = ws
        .path
        .join(format!(".deja/memory/stores/{TEST_UDID}/walks.sqlite.lock"));
    let lock2 = ws.path.join(format!(
        ".deja/memory/stores/{TEST_UDID_2}/walks.sqlite.lock"
    ));

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !ready_marker.exists() || !lock1.exists() || !lock2.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for deja to acquire both shard locks and invoke xcrun (ready={}, lock1={}, lock2={})",
            ready_marker.exists(),
            lock1.exists(),
            lock2.exists()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Send real POSIX SIGTERM to the running `deja` subprocess!
    deja_driver_ios::send_sigterm_to_pid(child.id()).expect("send SIGTERM to deja");

    let output = child.wait_with_output().expect("wait for deja exit");
    assert_eq!(
        output.status.code(),
        Some(143),
        "SIGTERM must exit 143 (stdout={}, stderr={})",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !lock1.exists() && !lock2.exists(),
        "both shard store .lock files must be cleaned up on SIGTERM"
    );

    let xml = fs::read_to_string(ws.path.join("out/sigterm.xml"))
        .expect("valid failing merged JUnit must be written on SIGTERM");
    assert!(deja_flow::reports_failure(&xml));
    assert!(xml.contains(r#"<property name="mode" value="batch"/>"#));
    assert!(xml.contains(r#"status="SKIPPED""#));
    assert!(xml.contains(r#"<skipped message="cancelled before execution"/>"#));
}

#[tokio::test]
async fn batch_verify_serializes_manifest_commits_across_shards_and_real_factory_reuses_staged_build()
 {
    let ws = TempWs::new("deja66-verify-real-stage");
    let app_dir = ws.path.join("build/VerifyApp.app");
    fs::create_dir_all(&app_dir).unwrap();
    ws.write(
        "build/VerifyApp.app/Info.plist",
        &format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{TEST_APP_ID}</string></dict></plist>\n"
        ),
    );
    ws.write_build("build/VerifyApp.app/VerifyApp", b"verify-binary-v1");
    let expected_fp = fingerprint_tree(&ws.path, &app_dir, "ios").unwrap();

    // 1. Direct `RealDriverFactory::prepare_batch_build` + `create` reuse + RAII drop cleanup.
    let cancel = CancellationToken::new();
    let base_ctx = DriverContext {
        platform: "ios".to_string(),
        device: TEST_UDID.to_string(),
        app_id: TEST_APP_ID.to_string(),
        build_path: app_dir.clone(),
        expected_build_fingerprint: Some(expected_fp.clone()),
        workspace_root: ws.path.clone(),
        disable_animations: true,
        crash_report_grace_ms: 0,
        crash_report_dir: None,
        deja_home: None,
        headless: None,
        screen_size: None,
        timeout_ms: 5_000,
        env: BTreeMap::new(),
        staged_build: None,
        resolved_runtime: None,
    };
    let staged = RealDriverFactory
        .prepare_batch_build(&base_ctx, &cancel)
        .await
        .expect("prepare_batch_build succeeds")
        .expect("iOS returns SharedStagedBuild");
    let stage_dir = staged.staging_dir().to_path_buf();
    assert!(stage_dir.exists());
    assert_eq!(staged.fingerprint(), expected_fp);

    let mut ctx_with_stage = base_ctx.clone();
    ctx_with_stage.staged_build = Some(staged.clone());
    // `RealDriverFactory::create` reuses `staged_build` cleanly.
    let driver = RealDriverFactory
        .create(&ctx_with_stage, &cancel)
        .await
        .expect("create reuses staged_build");
    drop(driver);
    drop(ctx_with_stage);
    drop(staged);
    assert!(
        !stage_dir.exists(),
        "SharedStagedBuild drop must clean up temporary staging directory"
    );

    // 2. Concurrent `--verify --devices` across 2 shards updates BOTH changed flows in
    // `deja.walks.json` without lost updates (serialized via `manifest_mutex`)!
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/VerifyApp.app",
            "manifest": "deja.walks.json"
        })
        .to_string(),
    );
    write_tagged_flow(&ws, "flows/v1.yaml", "v1");
    write_tagged_flow(&ws, "flows/v2.yaml", "v2");
    seed_manifest_walk(
        &ws,
        &[
            (
                "verify one",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/v1.yaml",
            ),
            (
                "verify two",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/v2.yaml",
            ),
        ],
    );
    // Modify non-assertion steps in BOTH flows so both need `--verify` manifest updates!
    ws.write(
        "flows/v1.yaml",
        &format!(
            "appId: {TEST_APP_ID}\n---\n- launchApp\n- hideKeyboard\n- inputText: \"v1\"\n- tapOn:\n    id: done\n- assertVisible: Done\n"
        ),
    );
    ws.write(
        "flows/v2.yaml",
        &format!(
            "appId: {TEST_APP_ID}\n---\n- launchApp\n- hideKeyboard\n- inputText: \"v2\"\n- tapOn:\n    id: done\n- assertVisible: Done\n"
        ),
    );

    let factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")));
    let (deps, cap) = make_deps(
        &ws.path,
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let devices_arg = format!("{TEST_UDID},{TEST_UDID_2}");
    let code = run_cli_from_args(
        [
            "deja",
            "run",
            "--all",
            "--verify",
            "--devices",
            &devices_arg,
            "--json",
        ],
        &deps,
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", cap.lock().unwrap().stderr);

    // `deja manifest check --json` must now pass with exit 0 for BOTH updated entries!
    let (deps_chk, cap_chk) = make_deps(
        &ws.path,
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let chk_code = run_cli_from_args(["deja", "manifest", "check", "--json"], &deps_chk).await;
    assert_eq!(
        chk_code,
        0,
        "both --verify manifest updates must persist cleanly: {}",
        cap_chk.lock().unwrap().stdout
    );
}

#[tokio::test]
async fn regression_deja66_r1_f1_failed_replay_on_new_shard_closes_multi_store_gate() {
    let ws = TempWs::new("deja66-r1-f1-shard-fail");
    ws.write_build("build/App.bin", b"build-v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "manifest": "deja.walks.json",
            "staleBuild": "replay-last-verified"
        })
        .to_string(),
    );
    write_tagged_flow(&ws, "flows/checkout.yaml", "checkout");
    seed_manifest_walk(
        &ws,
        &[(
            "checkout",
            TEST_APP_ID,
            "ios",
            "maestro-ios/v1",
            "flows/checkout.yaml",
        )],
    );
    ws.write("goals.txt", "checkout\n");

    // 1. Goal passes on shard A (`TEST_UDID`).
    let pass_factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")));
    let (deps_a, cap_a) = make_deps(
        &ws.path,
        Arc::clone(&pass_factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let code_a = run_cli_from_args(
        [
            "deja",
            "run",
            "--goals",
            "goals.txt",
            "--devices",
            TEST_UDID,
            "--json",
        ],
        &deps_a,
    )
    .await;
    assert_eq!(code_a, 0, "stderr: {}", cap_a.lock().unwrap().stderr);

    // 2. Goal now moves to brand-new shard B (`TEST_UDID_2`, zero prior history in shard B) and
    // FAILS consistently on the exact same build!
    let fail_factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")));
    fail_factory.add_failing_goal("checkout");
    let (deps_b, _) = make_deps(
        &ws.path,
        Arc::clone(&fail_factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let code_b = run_cli_from_args(
        [
            "deja",
            "run",
            "--goals",
            "goals.txt",
            "--devices",
            TEST_UDID_2,
            "--json",
        ],
        &deps_b,
    )
    .await;
    assert_eq!(code_b, 1, "consistent failure on shard B must exit 1");

    // 3. `deja gate --store A --store B` MUST see shard B's failure and close (exit 10),
    // NEVER passing on shard A's stale success!
    let (deps_gate, cap_gate) = make_deps(
        &ws.path,
        Arc::clone(&pass_factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let gate_code = run_cli_from_args(
        [
            "deja",
            "gate",
            "--goals",
            "goals.txt",
            "--store",
            TEST_UDID,
            "--store",
            TEST_UDID_2,
            "--json",
        ],
        &deps_gate,
    )
    .await;
    assert_eq!(
        gate_code,
        10,
        "gate must close when shard B failed replay (stdout={})",
        cap_gate.lock().unwrap().stdout
    );
    let gate_json: Value = serde_json::from_str(&cap_gate.lock().unwrap().stdout).unwrap();
    assert_eq!(gate_json["verdict"], "closed");
    assert_eq!(gate_json["goals"][0]["outcome"], "failed");
}

#[tokio::test]
async fn regression_deja66_r1_f2_config_junit_protects_manifest_flows_and_closures() {
    let ws = TempWs::new("deja66-r1-f2-cfg-junit");
    ws.write_build("build/App.bin", b"build-v1");
    let flow_text = format!(
        "appId: {TEST_APP_ID}\n---\n- launchApp\n- runFlow: sub.yaml\n- assertVisible: Done\n"
    );
    let sub_text = format!("appId: {TEST_APP_ID}\n---\n- assertVisible: Done\n");
    let flow_path = ws.write("flows/checkout.yaml", &flow_text);
    let sub_path = ws.write("flows/sub.yaml", &sub_text);
    seed_manifest_walk(
        &ws,
        &[(
            "checkout",
            TEST_APP_ID,
            "ios",
            "maestro-ios/v1",
            "flows/checkout.yaml",
        )],
    );

    for protected_junit in ["flows/checkout.yaml", "flows/sub.yaml"] {
        ws.write(
            "deja.config.json",
            &json!({
                "workspaceRoot": ".",
                "platform": "ios",
                "appId": TEST_APP_ID,
                "adapter": "maestro-ios/v1",
                "buildPath": "build/App.bin",
                "device": TEST_UDID,
                "manifest": "deja.walks.json",
                "junit": protected_junit
            })
            .to_string(),
        );
        let factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")));
        let (deps, _) = make_deps(
            &ws.path,
            Arc::clone(&factory) as Arc<dyn DriverFactory>,
            default_probes(),
            CancellationToken::new(),
        );
        let code = run_cli_from_args(["deja", "run", "--all", "--json"], &deps).await;
        assert_eq!(
            code, 2,
            "config.junit targeting {protected_junit} must fail closed exit 2"
        );
        assert_eq!(factory.total_create_calls(), 0);
    }

    assert_eq!(
        fs::read_to_string(&flow_path).unwrap(),
        flow_text,
        "flows/checkout.yaml must remain untouched"
    );
    assert_eq!(
        fs::read_to_string(&sub_path).unwrap(),
        sub_text,
        "flows/sub.yaml must remain untouched"
    );
}

#[tokio::test]
async fn regression_deja66_r1_f3_implicit_batch_reports_never_overwrite_goals_or_explicit_outputs()
{
    let ws = TempWs::new("deja66-r1-f3-implicit");
    ws.write_build("build/App.bin", b"build-v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "manifest": "deja.walks.json",
            "staleBuild": "replay-last-verified"
        })
        .to_string(),
    );
    write_tagged_flow(&ws, "flows/g1.yaml", "g1");
    seed_manifest_walk(
        &ws,
        &[("g1", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g1.yaml")],
    );

    let batch_latest_content = "g1\n";
    let batch_latest = ws.write(".deja/reports/batch-latest.json", batch_latest_content);
    let default_latest_content = "g1\n";
    let default_latest = ws.write(".deja/reports/latest.json", default_latest_content);

    let factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")));
    for argv in [
        vec![
            "deja",
            "run",
            "--goals",
            ".deja/reports/batch-latest.json",
            "--json",
        ],
        vec![
            "deja",
            "run",
            "--goals",
            ".deja/reports/latest.json",
            "--json",
        ],
        vec![
            "deja",
            "run",
            "--all",
            "--report",
            ".deja/reports/batch-latest.json",
            "--json",
        ],
        vec![
            "deja",
            "run",
            "--all",
            "--junit",
            ".deja/reports/batch-latest.json",
            "--json",
        ],
    ] {
        let (deps, _) = make_deps(
            &ws.path,
            Arc::clone(&factory) as Arc<dyn DriverFactory>,
            default_probes(),
            CancellationToken::new(),
        );
        let code = run_cli_from_args(argv.clone(), &deps).await;
        assert_eq!(
            code, 2,
            "implicit report collision for {argv:?} must fail closed exit 2"
        );
    }
    assert_eq!(factory.total_create_calls(), 0);
    assert_eq!(
        fs::read_to_string(&batch_latest).unwrap(),
        batch_latest_content,
        ".deja/reports/batch-latest.json used as --goals must NEVER be overwritten"
    );
    assert_eq!(
        fs::read_to_string(&default_latest).unwrap(),
        default_latest_content,
        ".deja/reports/latest.json used as --goals must NEVER be overwritten"
    );
}

#[tokio::test]
async fn regression_deja66_r1_f4_goals_manifest_filters_multi_platform_same_goal_entries() {
    let ws = TempWs::new("deja66-r1-f4-multiplat");
    ws.write_build("build/App.bin", b"build-v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "manifest": "deja.walks.json",
            "staleBuild": "replay-last-verified"
        })
        .to_string(),
    );
    write_tagged_flow(&ws, "flows/ios_checkout.yaml", "ios_checkout");
    ws.write(
        "flows/android_checkout.yaml",
        &format!("appId: {TEST_APP_ID}\n---\n- launchApp\n- assertVisible: Done\n"),
    );

    // Same goal name `"checkout"` for BOTH `ios` and `android` in the same `deja.walks.json`!
    seed_manifest_walk(
        &ws,
        &[
            (
                "checkout",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/ios_checkout.yaml",
            ),
            (
                "checkout",
                TEST_APP_ID,
                "android",
                "maestro-android/v1",
                "flows/android_checkout.yaml",
            ),
        ],
    );

    let factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")));
    let (deps, cap) = make_deps(
        &ws.path,
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let code = run_cli_from_args(
        ["deja", "run", "--goals", "deja.walks.json", "--json"],
        &deps,
    )
    .await;
    assert_eq!(
        code,
        0,
        "multi-platform manifest passed to --goals must filter cleanly without duplicate-goal error (stderr={})",
        cap.lock().unwrap().stderr
    );
    let parsed: Value = serde_json::from_str(&cap.lock().unwrap().stdout).unwrap();
    assert_eq!(parsed["goals"].as_array().unwrap().len(), 1);
    assert_eq!(parsed["goals"][0]["goal"], "checkout");
    assert_eq!(parsed["goals"][0]["outcome"], "passed");

    // `deja gate --goals deja.walks.json` also filters cleanly and passes!
    let (deps_gate, cap_gate) = make_deps(
        &ws.path,
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let gate_code = run_cli_from_args(
        ["deja", "gate", "--goals", "deja.walks.json", "--json"],
        &deps_gate,
    )
    .await;
    assert_eq!(
        gate_code,
        0,
        "gate --goals deja.walks.json must pass (stdout={})",
        cap_gate.lock().unwrap().stdout
    );
}

#[tokio::test]
async fn regression_deja66_r1_f5_unwritable_explicit_report_reflected_in_emitted_batch_json() {
    let ws = TempWs::new("deja66-r1-f5-report-err");
    ws.write_build("build/App.bin", b"build-v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "device": TEST_UDID,
            "manifest": "deja.walks.json",
            "staleBuild": "replay-last-verified"
        })
        .to_string(),
    );
    write_tagged_flow(&ws, "flows/g1.yaml", "g1");
    seed_manifest_walk(
        &ws,
        &[("g1", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g1.yaml")],
    );
    ws.write("not_a_dir", "blocking file");

    let factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")));
    let (deps, cap) = make_deps(
        &ws.path,
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let code = run_cli_from_args(
        [
            "deja",
            "run",
            "--all",
            "--report",
            "not_a_dir/batch.json",
            "--json",
        ],
        &deps,
    )
    .await;
    assert_eq!(code, 2, "unwritable explicit --report must exit 2");

    let out = cap.lock().unwrap().clone();
    let parsed: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(parsed["exitCode"], 2);
    assert_eq!(parsed["passed"], false);
    assert_eq!(parsed["outcome"], "error");
    assert!(
        parsed["errors"].as_array().unwrap().iter().any(|e| e
            .as_str()
            .unwrap_or("")
            .contains("failed to write explicit --report")),
        "stdout JSON errors must include explicit --report write error: {parsed}"
    );
}

#[tokio::test]
async fn regression_deja66_r1_f7_parallel_batch_goals_isolate_shared_take_screenshot_paths() {
    let ws = TempWs::new("deja66-r1-f7-shots");
    ws.write_build("build/App.bin", b"build-v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "manifest": "deja.walks.json",
            "staleBuild": "replay-last-verified"
        })
        .to_string(),
    );
    // Shared subflow writing to identical relative path `shared.png`!
    ws.write(
        "flows/capture_sub.yaml",
        &format!(
            "appId: {TEST_APP_ID}\n---\n- takeScreenshot: shared.png\n- assertVisible: Done\n"
        ),
    );
    ws.write(
        "flows/g1.yaml",
        &format!("appId: {TEST_APP_ID}\n---\n- launchApp\n- inputText: \"g1\"\n- runFlow: capture_sub.yaml\n"),
    );
    ws.write(
        "flows/g2.yaml",
        &format!("appId: {TEST_APP_ID}\n---\n- launchApp\n- inputText: \"g2\"\n- runFlow: capture_sub.yaml\n"),
    );
    seed_manifest_walk(
        &ws,
        &[
            ("g1", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g1.yaml"),
            ("g2", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g2.yaml"),
        ],
    );

    // Synchronize both shards with a 2-party Barrier so both goals run concurrently!
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let barrier_clone = Arc::clone(&barrier);
    let hook: AsyncGoalHook = Arc::new(move |_device, _tag, _cancel| {
        let b = Arc::clone(&barrier_clone);
        Box::pin(async move {
            b.wait().await;
        })
    });

    let factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")).with_on_goal(hook));
    let (deps, cap) = make_deps(
        &ws.path,
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let devices_arg = format!("{TEST_UDID},{TEST_UDID_2}");
    let code = tokio::time::timeout(
        Duration::from_secs(10),
        run_cli_from_args(
            ["deja", "run", "--all", "--devices", &devices_arg, "--json"],
            &deps,
        ),
    )
    .await
    .expect("parallel screenshot batch must not stall");
    assert_eq!(code, 0, "stderr: {}", cap.lock().unwrap().stderr);

    let parsed: Value = serde_json::from_str(&cap.lock().unwrap().stdout).unwrap();
    let goals = parsed["goals"].as_array().unwrap();
    assert_eq!(goals.len(), 2);

    let extract_shot = |g: &Value| -> String {
        g["report"]["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["kind"] == "output")
            .expect("goal report must contain screenshot output artifact")["path"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let shot0 = extract_shot(&goals[0]);
    let shot1 = extract_shot(&goals[1]);
    assert_ne!(
        shot0, shot1,
        "parallel goals must NOT share or overwrite the same screenshot path ({shot0})"
    );
    assert!(
        shot0.contains("goal-0-attempt-1") && shot0.ends_with("/shared.png"),
        "unexpected shot0 path: {shot0}"
    );
    assert!(
        shot1.contains("goal-1-attempt-1") && shot1.ends_with("/shared.png"),
        "unexpected shot1 path: {shot1}"
    );
    assert!(ws.path.join(&shot0).is_file());
    assert!(ws.path.join(&shot1).is_file());
    assert!(
        !ws.path.join("shared.png").exists(),
        "batch goals must not write shared.png at workspace root"
    );
}

#[tokio::test]
async fn regression_deja66_r1_f8_lpt_uses_pooled_samples_not_median_of_shard_medians() {
    let ws = TempWs::new("deja66-r1-f8-pooled-lpt");
    ws.write_build("build/App.bin", b"build-v-current");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "manifest": "deja.walks.json",
            "staleBuild": "replay-last-verified"
        })
        .to_string(),
    );
    write_tagged_flow(&ws, "flows/x.yaml", "x");
    write_tagged_flow(&ws, "flows/y.yaml", "y");
    seed_manifest_walk(
        &ws,
        &[
            (
                "goal x",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/x.yaml",
            ),
            (
                "goal y",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/y.yaml",
            ),
        ],
    );
    ws.write("goals.txt", "goal x\ngoal y\n");

    let x_cl = collect_flow_closure(&ws.path, &ws.path.join("flows/x.yaml")).unwrap();
    let x_main = x_cl
        .files
        .iter()
        .find(|f| f.path == x_cl.main)
        .unwrap()
        .sha256
        .clone();
    let y_cl = collect_flow_closure(&ws.path, &ws.path.join("flows/y.yaml")).unwrap();
    let y_main = y_cl
        .files
        .iter()
        .find(|f| f.path == y_cl.main)
        .unwrap()
        .sha256
        .clone();

    // Seed Shard A (`TEST_UDID`) with NINE 100ms runs for `goal x` AND one 500ms run for `goal y`.
    let store_a =
        SqliteWalkStore::open_writable(SqliteStoreOptions::new(&ws.path).with_store_id(TEST_UDID))
            .unwrap();
    for i in 0..9 {
        let fp = format!("{i:064x}");
        store_a
            .upsert_walk(&deja_store::UpsertWalkInput {
                identity: WalkIdentity {
                    goal: "goal x".to_string(),
                    app_id: TEST_APP_ID.to_string(),
                    platform: "ios".to_string(),
                    flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
                    build_fingerprint: fp,
                    adapter: Some("maestro-ios/v1".to_string()),
                    browser: None,
                },
                flow_path: "flows/x.yaml".to_string(),
                flow_sha256: x_main.clone(),
                subflow_files: None,
                legacy_closure: false,
                closure_sha256: Some(x_cl.sha256.clone()),
                validation: RecordValidationInput::new(true, now_iso8601())
                    .with_runtime("ios-27.2")
                    .with_duration_ms(100),
                exploration: None,
                repair_history: None,
            })
            .unwrap();
    }
    store_a
        .upsert_walk(&deja_store::UpsertWalkInput {
            identity: WalkIdentity {
                goal: "goal y".to_string(),
                app_id: TEST_APP_ID.to_string(),
                platform: "ios".to_string(),
                flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
                build_fingerprint: format!("{:064x}", 100),
                adapter: Some("maestro-ios/v1".to_string()),
                browser: None,
            },
            flow_path: "flows/y.yaml".to_string(),
            flow_sha256: y_main,
            subflow_files: None,
            legacy_closure: false,
            closure_sha256: Some(y_cl.sha256),
            validation: RecordValidationInput::new(true, now_iso8601())
                .with_runtime("ios-27.2")
                .with_duration_ms(500),
            exploration: None,
            repair_history: None,
        })
        .unwrap();
    store_a.close();

    // Seed Shard B (`TEST_UDID_2`) with ONE 9_900ms outlier run for `goal x`.
    // Median-of-medians would compute (100 + 9900)/2 = 5000ms (scheduling X first — WRONG!).
    // Pooled median computes median([100 x 9, 9900]) = 100ms (scheduling Y=500ms BEFORE X=100ms!).
    let store_b = SqliteWalkStore::open_writable(
        SqliteStoreOptions::new(&ws.path).with_store_id(TEST_UDID_2),
    )
    .unwrap();
    store_b
        .upsert_walk(&deja_store::UpsertWalkInput {
            identity: WalkIdentity {
                goal: "goal x".to_string(),
                app_id: TEST_APP_ID.to_string(),
                platform: "ios".to_string(),
                flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
                build_fingerprint: format!("{:064x}", 200),
                adapter: Some("maestro-ios/v1".to_string()),
                browser: None,
            },
            flow_path: "flows/x.yaml".to_string(),
            flow_sha256: x_main,
            subflow_files: None,
            legacy_closure: false,
            closure_sha256: Some(x_cl.sha256),
            validation: RecordValidationInput::new(true, now_iso8601())
                .with_runtime("ios-27.2")
                .with_duration_ms(9_900),
            exploration: None,
            repair_history: None,
        })
        .unwrap();
    store_b.close();

    let factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")));
    let (deps, cap) = make_deps(
        &ws.path,
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let devices_arg = format!("{TEST_UDID},{TEST_UDID_2}");
    let code = run_cli_from_args(
        [
            "deja",
            "run",
            "--goals",
            "goals.txt",
            "--devices",
            &devices_arg,
            "--json",
        ],
        &deps,
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", cap.lock().unwrap().stderr);
    let parsed: Value = serde_json::from_str(&cap.lock().unwrap().stdout).unwrap();
    assert_eq!(
        parsed["scheduledOrder"],
        json!(["goal y", "goal x"]),
        "pooled LPT median must schedule goal y (500ms) BEFORE goal x (100ms)"
    );
    assert_eq!(parsed["goals"][0]["historicalMedianMs"], 100);
    assert_eq!(parsed["goals"][1]["historicalMedianMs"], 500);
}

#[tokio::test]
async fn regression_deja66_r2_f1_strict_replay_schedules_each_goal_on_its_eligible_shard() {
    let ws = TempWs::new("deja66-r2-f1-shard-elig");
    ws.write_build("build/App.bin", b"build-v1");
    // Strict replay mode (NO `staleBuild: replay-last-verified`!): a shard can only replay a goal
    // if THAT shard's store has a verified validation for the current build + runtime.
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "manifest": "deja.walks.json"
        })
        .to_string(),
    );
    write_tagged_flow(&ws, "flows/a.yaml", "goal a");
    write_tagged_flow(&ws, "flows/b.yaml", "goal b");
    seed_manifest_walk(
        &ws,
        &[
            (
                "goal a",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/a.yaml",
            ),
            (
                "goal b",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/b.yaml",
            ),
        ],
    );

    // 1. Verify `--all --verify --devices TEST_UDID,TEST_UDID_2` so each shard verifies ONE goal
    //    into its own isolated store (use a 2-party barrier so each shard claims exactly 1 goal).
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let verify_hook: AsyncGoalHook = {
        let b = Arc::clone(&barrier);
        Arc::new(
            move |_device: String, _tag: String, _cancel: CancellationToken| {
                let b = Arc::clone(&b);
                Box::pin(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(3), b.wait()).await;
                })
            },
        )
    };
    let verify_factory =
        Arc::new(BatchMockFactory::new(Some("ios-27.2")).with_on_goal(verify_hook));
    let (verify_deps, verify_cap) = make_deps(
        &ws.path,
        Arc::clone(&verify_factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let devices_forward = format!("{TEST_UDID},{TEST_UDID_2}");
    let verify_code = run_cli_from_args(
        [
            "deja",
            "run",
            "--all",
            "--verify",
            "--devices",
            &devices_forward,
            "--json",
        ],
        &verify_deps,
    )
    .await;
    assert_eq!(
        verify_code,
        0,
        "verify stderr: {}",
        verify_cap.lock().unwrap().stderr
    );
    let verify_json: Value = serde_json::from_str(&verify_cap.lock().unwrap().stdout).unwrap();
    let goal_a_dev = verify_json["goals"][0]["device"]
        .as_str()
        .unwrap()
        .to_string();
    let goal_b_dev = verify_json["goals"][1]["device"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(
        goal_a_dev, goal_b_dev,
        "barrier must assign goal a and goal b to distinct shards"
    );

    // 2. Now run strict replay `--all --devices <goal_b_dev>,<goal_a_dev>` (NO `--verify`!)
    //    putting `goal_b_dev` FIRST as shard 0 while `goal a` sits ahead in the queue (or even if
    //    shard 0 wakes up first!). Both goals MUST route to their respective verified shards and
    //    pass with exit 0!
    let devices_reversed = format!("{goal_b_dev},{goal_a_dev}");
    let replay_factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")));
    let (replay_deps, replay_cap) = make_deps(
        &ws.path,
        Arc::clone(&replay_factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let replay_code = run_cli_from_args(
        [
            "deja",
            "run",
            "--all",
            "--devices",
            &devices_reversed,
            "--json",
        ],
        &replay_deps,
    )
    .await;
    assert_eq!(
        replay_code,
        0,
        "strict replay must route each goal to its eligible shard; stdout={} stderr={}",
        replay_cap.lock().unwrap().stdout,
        replay_cap.lock().unwrap().stderr
    );
    let replay_json: Value = serde_json::from_str(&replay_cap.lock().unwrap().stdout).unwrap();
    assert_eq!(replay_json["passed"], true);
    assert_eq!(replay_json["goals"][0]["outcome"], "passed");
    assert_eq!(replay_json["goals"][0]["device"], goal_a_dev);
    assert_eq!(replay_json["goals"][1]["outcome"], "passed");
    assert_eq!(replay_json["goals"][1]["device"], goal_b_dev);

    // 3. Verify no hang if one eligible shard fails driver attach: the healthy shard executes its
    //    eligible goal cleanly and exits, and the stranded goal reports the failed shard's error.
    let mut failing_factory = BatchMockFactory::new(Some("ios-27.2"));
    failing_factory.fail_create_devices.insert(
        goal_a_dev.clone(),
        "simulator unavailable on shard a".to_string(),
    );
    let (fail_deps, fail_cap) = make_deps(
        &ws.path,
        Arc::new(failing_factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let fail_code = tokio::time::timeout(
        Duration::from_secs(5),
        run_cli_from_args(
            [
                "deja",
                "run",
                "--all",
                "--devices",
                &devices_reversed,
                "--json",
            ],
            &fail_deps,
        ),
    )
    .await
    .expect("batch run must not hang when an eligible shard fails to attach");
    assert_eq!(fail_code, 1);
    let fail_json: Value = serde_json::from_str(&fail_cap.lock().unwrap().stdout).unwrap();
    assert_eq!(fail_json["goals"][0]["outcome"], "error");
    assert!(
        fail_json["goals"][0]["errors"][0]
            .as_str()
            .unwrap_or("")
            .contains("simulator unavailable on shard a")
    );
    assert_eq!(fail_json["goals"][1]["outcome"], "passed");
    assert_eq!(fail_json["goals"][1]["device"], goal_b_dev);
}

#[tokio::test]
async fn regression_deja66_r2_f2_rejects_overlapping_junit_and_report_destinations() {
    let ws = TempWs::new("deja66-r2-f2-overlap");
    ws.write_build("build/App.bin", b"build-v1");
    ws.write(
        "flows/g1.yaml",
        &format!("appId: {TEST_APP_ID}\n---\n- assertVisible: g1\n"),
    );
    ws.write("goals.txt", "g1\n");
    let sentinel = "SENTINEL_MUST_STAY_UNTOUCHED\n";
    ws.write("out/shared.xml", sentinel);

    // Case A: batch `deja run --goals goals.txt --junit out/shared.xml --report out/shared.xml`
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "device": TEST_UDID,
            "buildPath": "build/App.bin"
        })
        .to_string(),
    );
    let (deps_a, cap_a) = make_deps(
        &ws.path,
        Arc::new(BatchMockFactory::new(Some("ios-27.2"))),
        default_probes(),
        CancellationToken::new(),
    );
    let code_a = run_cli_from_args(
        [
            "deja",
            "run",
            "--goals",
            "goals.txt",
            "--junit",
            "out/shared.xml",
            "--report",
            "out/shared.xml",
            "--json",
        ],
        &deps_a,
    )
    .await;
    assert_eq!(code_a, 2, "stderr: {}", cap_a.lock().unwrap().stderr);
    assert_eq!(
        fs::read_to_string(ws.path.join("out/shared.xml")).unwrap(),
        sentinel,
        "overlapping --junit and --report must fail before touching disk"
    );

    // Case B: single-goal `deja run g1 --junit out/shared.xml --report ./out/../out/shared.xml`
    let (deps_b, _) = make_deps(
        &ws.path,
        Arc::new(BatchMockFactory::new(Some("ios-27.2"))),
        default_probes(),
        CancellationToken::new(),
    );
    let code_b = run_cli_from_args(
        [
            "deja",
            "run",
            "g1",
            "--junit",
            "out/shared.xml",
            "--report",
            "./out/../out/shared.xml",
            "--json",
        ],
        &deps_b,
    )
    .await;
    assert_eq!(code_b, 2);
    assert_eq!(
        fs::read_to_string(ws.path.join("out/shared.xml")).unwrap(),
        sentinel
    );

    // Case C: `deja remember g1 --flow flows/g1.yaml --junit out/shared.xml --report out/shared.xml`
    let (deps_c, _) = make_deps(
        &ws.path,
        Arc::new(BatchMockFactory::new(Some("ios-27.2"))),
        default_probes(),
        CancellationToken::new(),
    );
    let code_c = run_cli_from_args(
        [
            "deja",
            "remember",
            "g1",
            "--flow",
            "flows/g1.yaml",
            "--junit",
            "out/shared.xml",
            "--report",
            "out/shared.xml",
            "--json",
        ],
        &deps_c,
    )
    .await;
    assert_eq!(code_c, 2);
    assert_eq!(
        fs::read_to_string(ws.path.join("out/shared.xml")).unwrap(),
        sentinel
    );

    // Case D: `config.junit` matches `--report` across batch `run`, single-goal `run`, and `remember`.
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "device": TEST_UDID,
            "buildPath": "build/App.bin",
            "junit": "out/shared.xml"
        })
        .to_string(),
    );
    for argv in [
        vec![
            "deja",
            "run",
            "--goals",
            "goals.txt",
            "--report",
            "out/shared.xml",
            "--json",
        ],
        vec!["deja", "run", "g1", "--report", "out/shared.xml", "--json"],
        vec![
            "deja",
            "remember",
            "g1",
            "--flow",
            "flows/g1.yaml",
            "--report",
            "out/shared.xml",
            "--json",
        ],
    ] {
        let (deps_d, _) = make_deps(
            &ws.path,
            Arc::new(BatchMockFactory::new(Some("ios-27.2"))),
            default_probes(),
            CancellationToken::new(),
        );
        let code_d = run_cli_from_args(argv, &deps_d).await;
        assert_eq!(code_d, 2);
        assert_eq!(
            fs::read_to_string(ws.path.join("out/shared.xml")).unwrap(),
            sentinel,
            "config.junit overlapping --report must fail before prepare_safe_junit unlinks file"
        );
    }
}

#[tokio::test]
async fn regression_deja66_r2_f4_batch_build_fingerprint_cancellation_closes_stores_and_exits_cleanly()
 {
    let ws = TempWs::new("deja66-r2-f4-fp-cancel");
    let app_dir = ws.path.join("build/Big.app");
    fs::create_dir_all(&app_dir).unwrap();
    let blob = vec![0x77u8; deja_core::FINGERPRINT_CHUNK_BYTES * 2];
    for i in 0..8 {
        fs::write(app_dir.join(format!("blob-{i:02}.bin")), &blob).unwrap();
    }
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "device": TEST_UDID,
            "buildPath": "build/Big.app",
            "manifest": "deja.walks.json"
        })
        .to_string(),
    );
    write_tagged_flow(&ws, "flows/g1.yaml", "g1");
    seed_manifest_walk(
        &ws,
        &[("g1", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g1.yaml")],
    );

    let cancel = CancellationToken::new();
    let (deps, cap) = make_deps(
        &ws.path,
        Arc::new(BatchMockFactory::new(Some("ios-27.2"))),
        default_probes(),
        cancel.clone(),
    );
    deps.cancel_signal.store(
        deja_cli::exit_codes::CANCEL_SIGNAL_SIGTERM,
        Ordering::SeqCst,
    );
    cancel.cancel();

    let code = run_cli_from_args(
        [
            "deja",
            "run",
            "--all",
            "--junit",
            "out/fp-cancel.xml",
            "--json",
        ],
        &deps,
    )
    .await;
    assert_eq!(
        code, 143,
        "SIGTERM cancel before/during fingerprinting must exit 143"
    );
    assert!(
        !ws.path.join(".deja/memory/walks.sqlite.lock").exists(),
        "all shard store locks must be released when cancelled during fingerprinting"
    );

    let parsed: Value = serde_json::from_str(&cap.lock().unwrap().stdout).unwrap();
    assert_eq!(parsed["cancelled"], true);
    assert_eq!(parsed["outcome"], "cancelled");
    assert_eq!(parsed["goals"][0]["outcome"], "skipped");

    let xml = fs::read_to_string(ws.path.join("out/fp-cancel.xml")).unwrap();
    assert!(deja_flow::reports_failure(&xml));
    assert!(xml.contains(r#"status="SKIPPED""#));
}

#[tokio::test]
async fn web_real_driver_factory_runtime_create_install_doctor_and_invalid_device_exit_2() {
    let ws = TempWs::new("web-factory");
    fs::create_dir_all(ws.path.join("public")).unwrap();
    let fake_chrome = ws.path.join("fake-chromium.sh");
    fs::write(
        &fake_chrome,
        "#!/bin/sh\necho 'Google Chrome 145.0.7000.12'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_chrome, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut env = BTreeMap::new();
    env.insert(
        "DEJA_CHROME".to_string(),
        fake_chrome.to_string_lossy().into_owned(),
    );

    let make_web_ctx = |device: &str, screen_size: Option<Value>| DriverContext {
        platform: "web".to_string(),
        device: device.to_string(),
        app_id: "http://127.0.0.1:4174".to_string(),
        build_path: ws.path.join("public"),
        expected_build_fingerprint: None,
        workspace_root: ws.path.clone(),
        disable_animations: true,
        crash_report_grace_ms: 0,
        crash_report_dir: None,
        deja_home: None,
        headless: Some(true),
        screen_size,
        timeout_ms: 30_000,
        env: env.clone(),
        staged_build: None,
        resolved_runtime: None,
    };

    let cancel = CancellationToken::new();
    let factory = RealDriverFactory;

    // 1. `resolve_runtime` returns `chromium-<version>@<W>x<H>` (and `-headed` when `headless == false`) via `DEJA_CHROME`.
    let valid_ctx = make_web_ctx("chromium", Some(json!("1280x800")));
    let rt = factory
        .resolve_runtime(&valid_ctx, &cancel)
        .await
        .expect("resolve_runtime should succeed");
    assert_eq!(rt.as_deref(), Some("chromium-145.0.7000.12@1280x800"));

    let mut headed_ctx = make_web_ctx("chromium", Some(json!("1280x800")));
    headed_ctx.headless = Some(false);
    let rt_headed = factory
        .resolve_runtime(&headed_ctx, &cancel)
        .await
        .expect("headed resolve_runtime should succeed");
    assert_eq!(
        rt_headed.as_deref(),
        Some("chromium-145.0.7000.12@1280x800-headed")
    );

    let default_sz_ctx = make_web_ctx("chromium", None);
    let rt_default = factory
        .resolve_runtime(&default_sz_ctx, &cancel)
        .await
        .expect("default screenSize resolve_runtime should succeed");
    assert_eq!(
        rt_default.as_deref(),
        Some("chromium-145.0.7000.12@1280x720")
    );

    // 2. `create` constructs a native `WebDriver` with `"web"` platform and `DisableAnimations`.
    let driver = factory
        .create(&valid_ctx, &cancel)
        .await
        .expect("create should construct WebDriver");
    assert_eq!(driver.platform(), "web");
    assert!(
        driver
            .capabilities()
            .supports(DriverCapability::DisableAnimations)
    );
    assert!(
        driver
            .capabilities()
            .supports(DriverCapability::CompoundAct)
    );
    driver.shutdown().await.unwrap();

    // 3. `install_app` is a clean no-op (`Ok(())`) on web.
    factory
        .install_app(&valid_ctx, &cancel)
        .await
        .expect("install_app must be Ok(()) on web");

    // 4. Non-`chromium` device fails closed with exit 2 on both `resolve_runtime` and `create`.
    let bad_dev_ctx = make_web_ctx("firefox", None);
    let err_rt = factory
        .resolve_runtime(&bad_dev_ctx, &cancel)
        .await
        .expect_err("non-chromium device must fail resolve_runtime");
    assert_eq!(err_rt.exit_code(), 2);
    let err_create = factory
        .create(&bad_dev_ctx, &cancel)
        .await
        .err()
        .expect("non-chromium device must fail create");
    assert_eq!(err_create.exit_code(), 2);

    // 5. Invalid `screenSize` fails closed with exit 2 on both `resolve_runtime` and `create`.
    let bad_sz_ctx = make_web_ctx("chromium", Some(json!("99x50")));
    let err_rt_sz = factory
        .resolve_runtime(&bad_sz_ctx, &cancel)
        .await
        .expect_err("invalid screenSize must fail resolve_runtime");
    assert_eq!(err_rt_sz.exit_code(), 2);
    let err_sz = factory
        .create(&bad_sz_ctx, &cancel)
        .await
        .err()
        .expect("invalid screenSize must fail create");
    assert_eq!(err_sz.exit_code(), 2);

    // 6. `RealDoctorProbes` delegates to `deja_driver_web::{discover_chromium, probe_chromium_version}`
    // honoring `DEJA_CHROME`.
    let probes = deja_cli::driver_factory::RealDoctorProbes;
    let checks = probes
        .probe(&DoctorProbeRequest {
            platform: Some("web".to_string()),
            device: Some("chromium".to_string()),
            env: env.clone(),
            deja_home: None,
        })
        .await;
    let web_check = checks
        .iter()
        .find(|c| c.name == "web-chromium")
        .expect("web-chromium check must be present");
    assert_eq!(web_check.status, DoctorCheckStatus::Ready);
    assert!(web_check.required);
    assert!(
        web_check.detail.contains("145.0.7000.12"),
        "expected version from DEJA_CHROME, got: {}",
        web_check.detail
    );

    // 7. CLI `deja remember --device firefox` on web config exits with code 2.
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "web",
            "appId": "http://127.0.0.1:4174",
            "adapter": "maestro-web@2.12.0",
            "browser": "chromium",
            "device": "chromium",
            "buildPath": "public",
            "screenSize": "1280x800"
        })
        .to_string(),
    );
    ws.write(
        "flows/checkout.yaml",
        "url: http://127.0.0.1:4174\n---\n- openLink: http://127.0.0.1:4174\n- assertVisible: Shop\n",
    );
    let (deps, _) = make_deps(
        &ws.path,
        Arc::new(RealDriverFactory),
        default_probes(),
        CancellationToken::new(),
    );
    let exit = run_cli_from_args(
        [
            "deja",
            "remember",
            "checkout",
            "--flow",
            "flows/checkout.yaml",
            "--device",
            "firefox",
        ],
        &deps,
    )
    .await;
    assert_eq!(exit, 2, "invalid non-chromium web device must exit 2");
}

#[tokio::test]
async fn codex_r2_f1_web_gate_enforces_configured_viewport_and_headless_mode() {
    let ws = TempWs::new("web-gate-viewport");
    ws.write("public/index.html", "<!doctype html><h1>Shop</h1>");
    ws.write(
        "flows/checkout.yaml",
        "url: http://127.0.0.1:4174\n---\n- openLink: http://127.0.0.1:4174\n- assertVisible: Shop\n",
    );
    ws.write("goals.txt", "complete checkout\n");

    let write_web_config = |screen_size: &str, headless: bool| {
        ws.write(
            "deja.config.json",
            &json!({
                "workspaceRoot": ".",
                "platform": "web",
                "appId": "http://127.0.0.1:4174",
                "adapter": "maestro-web@2.12.0",
                "browser": "chromium",
                "device": "chromium",
                "buildPath": "public",
                "screenSize": screen_size,
                "headless": headless
            })
            .to_string(),
        );
    };

    // 1. Record a passing validation at desktop `1280x800` (headless).
    write_web_config("1280x800", true);
    let ok_tree = hierarchy_with_nodes(
        "http://127.0.0.1:4174",
        vec![node_with_label("heading", "Shop")],
    );
    let desktop_rt = "chromium-145.0.7000.12@1280x800";
    let factory = Arc::new(MockFactory::new(
        Some(desktop_rt),
        MockDriver::new(vec![ok_tree]),
    ));
    let (deps_rem, _) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    let rem_code = run_cli_from_args(
        [
            "deja",
            "remember",
            "complete checkout",
            "--flow",
            "flows/checkout.yaml",
        ],
        &deps_rem,
    )
    .await;
    assert_eq!(rem_code, 0);

    // 2. With matching `1280x800` config, BOTH `deja gate` (without `--runtime`) AND
    // `deja gate --runtime chromium-145.0.7000.12@1280x800` pass.
    let (deps_g1, cap_g1) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(["deja", "gate", "--goals", "goals.txt", "--json"], &deps_g1).await,
        0
    );
    let g1: Value = serde_json::from_str(&cap_g1.lock().unwrap().stdout).unwrap();
    assert_eq!(g1["verdict"], "passed");

    let (deps_g2, cap_g2) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            [
                "deja",
                "gate",
                "--goals",
                "goals.txt",
                "--runtime",
                desktop_rt,
                "--json",
            ],
            &deps_g2,
        )
        .await,
        0
    );
    let g2: Value = serde_json::from_str(&cap_g2.lock().unwrap().stdout).unwrap();
    assert_eq!(g2["verdict"], "passed");

    // Passing `--runtime` without the normalized viewport suffix MUST NOT match.
    let (deps_g_bare, cap_g_bare) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            [
                "deja",
                "gate",
                "--goals",
                "goals.txt",
                "--runtime",
                "chromium-145.0.7000.12",
                "--json",
            ],
            &deps_g_bare,
        )
        .await,
        10
    );
    let g_bare: Value = serde_json::from_str(&cap_g_bare.lock().unwrap().stdout).unwrap();
    assert_eq!(g_bare["verdict"], "closed");
    assert_eq!(g_bare["goals"][0]["outcome"], "not-run");

    // 3. Change ONLY `screenSize` to mobile `"390x844"` (same `buildPath`!):
    // BOTH `deja gate` (omitting `--runtime`) AND `deja gate --runtime <desktop_rt>` MUST close!
    write_web_config("390x844", true);

    let (deps_m1, cap_m1) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(["deja", "gate", "--goals", "goals.txt", "--json"], &deps_m1).await,
        10,
        "desktop 1280x800 pass must NEVER approve a 390x844 web config when --runtime is omitted"
    );
    let m1: Value = serde_json::from_str(&cap_m1.lock().unwrap().stdout).unwrap();
    assert_eq!(m1["verdict"], "closed");
    assert_eq!(m1["goals"][0]["outcome"], "not-run");

    let (deps_m2, cap_m2) = make_deps(
        &ws.path,
        factory.clone(),
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(
            [
                "deja",
                "gate",
                "--goals",
                "goals.txt",
                "--runtime",
                desktop_rt,
                "--json",
            ],
            &deps_m2,
        )
        .await,
        10,
        "desktop 1280x800 --runtime must NEVER approve a 390x844 web config"
    );
    let m2: Value = serde_json::from_str(&cap_m2.lock().unwrap().stdout).unwrap();
    assert_eq!(m2["verdict"], "closed");
    assert_eq!(m2["goals"][0]["outcome"], "not-run");

    // 4. Switching `headless: false` at `1280x800` also isolates against headless evidence.
    write_web_config("1280x800", false);
    let (deps_h, cap_h) = make_deps(
        &ws.path,
        factory,
        default_probes(),
        CancellationToken::new(),
    );
    assert_eq!(
        run_cli_from_args(["deja", "gate", "--goals", "goals.txt", "--json"], &deps_h).await,
        10
    );
    let h_json: Value = serde_json::from_str(&cap_h.lock().unwrap().stdout).unwrap();
    assert_eq!(h_json["verdict"], "closed");
    assert_eq!(h_json["goals"][0]["outcome"], "not-run");
}

#[tokio::test]
async fn codex_r3_f3_doctor_and_runtime_skip_broken_path_chromium_candidate() {
    let ws = TempWs::new("web-skip-broken");
    let bin_dir = ws.path.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let broken_chromium = bin_dir.join("chromium");
    let working_chrome = bin_dir.join("google-chrome");
    fs::write(
        &broken_chromium,
        "#!/bin/sh\necho 'broken stub' >&2\nexit 1\n",
    )
    .unwrap();
    fs::write(
        &working_chrome,
        "#!/bin/sh\necho 'Google Chrome 147.0.1.2'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&broken_chromium, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&working_chrome, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut env = BTreeMap::new();
    env.insert("PATH".to_string(), bin_dir.to_string_lossy().into_owned());

    // 1. `RealDoctorProbes` skips broken `chromium` and reports ready with working `google-chrome`.
    let probes = deja_cli::driver_factory::RealDoctorProbes;
    let checks = probes
        .probe(&DoctorProbeRequest {
            platform: Some("web".to_string()),
            device: Some("chromium".to_string()),
            env: env.clone(),
            deja_home: None,
        })
        .await;
    let web_check = checks
        .iter()
        .find(|c| c.name == "web-chromium")
        .expect("web-chromium check");
    assert_eq!(web_check.status, DoctorCheckStatus::Ready);
    assert!(
        web_check.detail.contains("147.0.1.2"),
        "expected doctor to skip broken chromium and find 147.0.1.2, got {}",
        web_check.detail
    );

    // 2. `RealDriverFactory::resolve_runtime` also skips broken `chromium` and uses `google-chrome`.
    let ctx = DriverContext {
        platform: "web".to_string(),
        device: "chromium".to_string(),
        app_id: "http://127.0.0.1:4174".to_string(),
        build_path: ws.path.clone(),
        expected_build_fingerprint: None,
        workspace_root: ws.path.clone(),
        disable_animations: true,
        crash_report_grace_ms: 0,
        crash_report_dir: None,
        deja_home: None,
        headless: Some(true),
        screen_size: Some(json!("1280x800")),
        timeout_ms: 30_000,
        env: env.clone(),
        staged_build: None,
        resolved_runtime: None,
    };
    let cancel = CancellationToken::new();
    let rt = RealDriverFactory
        .resolve_runtime(&ctx, &cancel)
        .await
        .expect("resolve_runtime must skip broken PATH candidate");
    assert_eq!(rt.as_deref(), Some("chromium-147.0.1.2@1280x800"));

    // 3. Explicit broken `DEJA_CHROME` fails closed in BOTH doctor and `resolve_runtime`.
    env.insert(
        "DEJA_CHROME".to_string(),
        broken_chromium.to_string_lossy().into_owned(),
    );
    let broken_checks = probes
        .probe(&DoctorProbeRequest {
            platform: Some("web".to_string()),
            device: Some("chromium".to_string()),
            env: env.clone(),
            deja_home: None,
        })
        .await;
    let broken_web = broken_checks
        .iter()
        .find(|c| c.name == "web-chromium")
        .expect("web-chromium check");
    assert_eq!(broken_web.status, DoctorCheckStatus::NotReady);

    let mut broken_ctx = ctx;
    broken_ctx.env = env;
    assert!(
        RealDriverFactory
            .resolve_runtime(&broken_ctx, &cancel)
            .await
            .is_err(),
        "explicit broken DEJA_CHROME must fail closed"
    );
}

#[tokio::test]
async fn regression_deja66_r3_f1_batch_take_screenshot_parent_escape_fails_closed() {
    let ws = TempWs::new("deja66-r3-f1-batch-shot-escape");
    ws.write_build("build/App.bin", b"build-v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "device": TEST_UDID,
            "buildPath": "build/App.bin",
            "manifest": "deja.walks.json",
            "staleBuild": "replay-last-verified"
        })
        .to_string(),
    );
    ws.write(
        "flows/escape.yaml",
        &format!(
            "appId: {TEST_APP_ID}\n---\n- takeScreenshot: \"../shared.png\"\n- assertVisible: Done\n"
        ),
    );
    seed_manifest_walk(
        &ws,
        &[(
            "escape goal",
            TEST_APP_ID,
            "ios",
            "maestro-ios/v1",
            "flows/escape.yaml",
        )],
    );

    let (deps, cap) = make_deps(
        &ws.path,
        Arc::new(BatchMockFactory::new(Some("ios-27.2"))),
        default_probes(),
        CancellationToken::new(),
    );
    let code = run_cli_from_args(["deja", "run", "--all", "--json"], &deps).await;
    assert_ne!(
        code,
        0,
        "batch goal escaping its attempt screenshot directory must fail closed; stdout={}",
        cap.lock().unwrap().stdout
    );
    let parsed: Value = serde_json::from_str(&cap.lock().unwrap().stdout).unwrap();
    assert_eq!(parsed["goals"][0]["passed"], false);

    // Ensure `shared.png` was NEVER written anywhere under `.deja/reports/batch-screenshots/`.
    let shot_root = ws.path.join(".deja/reports/batch-screenshots");
    if shot_root.exists() {
        for entry in fs::read_dir(&shot_root).unwrap() {
            let entry = entry.unwrap();
            assert!(
                !entry.path().join("shared.png").exists(),
                "escaping screenshot ../shared.png must never exist in {}",
                entry.path().display()
            );
        }
    }
}

#[tokio::test]
async fn regression_deja66_r3_f2_runtime_lookup_failure_on_one_shard_continues_on_healthy_shard() {
    let ws = TempWs::new("deja66-r3-f2-rt-fail-shard");
    ws.write_build("build/App.bin", b"build-v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "manifest": "deja.walks.json",
            "staleBuild": "replay-last-verified"
        })
        .to_string(),
    );
    write_tagged_flow(&ws, "flows/g1.yaml", "g1");
    write_tagged_flow(&ws, "flows/g2.yaml", "g2");
    seed_manifest_walk(
        &ws,
        &[
            ("g1", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g1.yaml"),
            ("g2", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g2.yaml"),
        ],
    );

    // 1. Shard 0 (`TEST_UDID`) fails `resolve_runtime`, shard 1 (`TEST_UDID_2`) is healthy.
    //    Batch MUST continue on `TEST_UDID_2`, execute ALL goals cleanly, release `TEST_UDID`'s
    //    store lock, and record the runtime lookup failure on `shards[0]`.
    let mut factory = BatchMockFactory::new(Some("ios-27.2"));
    factory
        .fail_runtime_devices
        .insert(TEST_UDID.to_string(), "simulator UDID deleted".to_string());
    let (deps, cap) = make_deps(
        &ws.path,
        Arc::new(factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let devices_arg = format!("{TEST_UDID},{TEST_UDID_2}");
    let code = run_cli_from_args(
        ["deja", "run", "--all", "--devices", &devices_arg, "--json"],
        &deps,
    )
    .await;
    assert_eq!(
        code,
        0,
        "healthy shard must execute all eligible goals; stdout={} stderr={}",
        cap.lock().unwrap().stdout,
        cap.lock().unwrap().stderr
    );
    let parsed: Value = serde_json::from_str(&cap.lock().unwrap().stdout).unwrap();
    assert_eq!(parsed["passed"], true);
    assert_eq!(parsed["counts"]["passed"], 2);
    assert_eq!(parsed["goals"][0]["device"], TEST_UDID_2);
    assert_eq!(parsed["goals"][1]["device"], TEST_UDID_2);

    assert_eq!(parsed["shards"][0]["device"], TEST_UDID);
    assert!(parsed["shards"][0]["runtime"].is_null());
    assert_eq!(parsed["shards"][0]["goalsExecuted"], 0);
    assert!(
        parsed["shards"][0]["error"]
            .as_str()
            .unwrap_or("")
            .contains("simulator UDID deleted")
    );
    assert_eq!(parsed["shards"][1]["device"], TEST_UDID_2);
    assert_eq!(parsed["shards"][1]["runtime"], "ios-27.2");
    assert_eq!(parsed["shards"][1]["goalsExecuted"], 2);
    assert!(parsed["shards"][1].get("error").is_none());

    assert!(
        !ws.path
            .join(format!(".deja/memory/stores/{TEST_UDID}/walks.sqlite.lock"))
            .exists(),
        "failed shard's store lock must be released"
    );

    // 2. When ALL shards fail `resolve_runtime`, batch emits per-goal error records + failing JUnit.
    let mut all_fail_factory = BatchMockFactory::new(Some("ios-27.2"));
    all_fail_factory
        .fail_runtime_devices
        .insert(TEST_UDID.to_string(), "sim 1 missing".to_string());
    all_fail_factory
        .fail_runtime_devices
        .insert(TEST_UDID_2.to_string(), "sim 2 missing".to_string());
    let (deps2, cap2) = make_deps(
        &ws.path,
        Arc::new(all_fail_factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let code2 = run_cli_from_args(
        [
            "deja",
            "run",
            "--all",
            "--devices",
            &devices_arg,
            "--junit",
            "out/all-rt-fail.xml",
            "--json",
        ],
        &deps2,
    )
    .await;
    assert_eq!(code2, 1);
    let parsed2: Value = serde_json::from_str(&cap2.lock().unwrap().stdout).unwrap();
    assert_eq!(parsed2["counts"]["errored"], 2);
    assert_eq!(parsed2["goals"][0]["outcome"], "error");
    assert_eq!(parsed2["goals"][1]["outcome"], "error");
    let xml = fs::read_to_string(ws.path.join("out/all-rt-fail.xml")).unwrap();
    assert!(deja_flow::reports_failure(&xml));
    assert!(xml.contains(r#"errors="2""#));
}

#[tokio::test]
async fn regression_deja66_r3_f3_unstarted_goal_reassigned_when_shard_cannot_attach() {
    let ws = TempWs::new("deja66-r3-f3-reassign-attach");
    ws.write_build("build/App.bin", b"build-v1");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "manifest": "deja.walks.json",
            "staleBuild": "replay-last-verified"
        })
        .to_string(),
    );
    write_tagged_flow(&ws, "flows/g1.yaml", "g1");
    write_tagged_flow(&ws, "flows/g2.yaml", "g2");
    seed_manifest_walk(
        &ws,
        &[
            ("g1", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g1.yaml"),
            ("g2", TEST_APP_ID, "ios", "maestro-ios/v1", "flows/g2.yaml"),
        ],
    );

    // Shard 0 (`TEST_UDID`) delays 50ms in `create()` and then fails attach AFTER shard 1
    // (`TEST_UDID_2`) has already finished `g2`! Shard 1 MUST wait for the attaching shard, wake
    // up when `g1` is pushed back onto the queue, and execute BOTH `g1` and `g2` to exit 0!
    let mut factory = BatchMockFactory::new(Some("ios-27.2"));
    factory
        .create_delay_by_device
        .insert(TEST_UDID.to_string(), Duration::from_millis(50));
    factory.fail_create_devices.insert(
        TEST_UDID.to_string(),
        "WDA attach failed on shard 0".to_string(),
    );
    let (deps, cap) = make_deps(
        &ws.path,
        Arc::new(factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let devices_arg = format!("{TEST_UDID},{TEST_UDID_2}");
    let code = tokio::time::timeout(
        Duration::from_secs(5),
        run_cli_from_args(
            ["deja", "run", "--all", "--devices", &devices_arg, "--json"],
            &deps,
        ),
    )
    .await
    .expect("reassignment across shards must not hang");
    assert_eq!(
        code,
        0,
        "healthy shard must execute reassigned unstarted goal; stdout={} stderr={}",
        cap.lock().unwrap().stdout,
        cap.lock().unwrap().stderr
    );
    let parsed: Value = serde_json::from_str(&cap.lock().unwrap().stdout).unwrap();
    assert_eq!(parsed["passed"], true);
    assert_eq!(parsed["counts"]["passed"], 2);
    assert_eq!(parsed["goals"][0]["outcome"], "passed");
    assert_eq!(parsed["goals"][0]["device"], TEST_UDID_2);
    assert_eq!(parsed["goals"][1]["outcome"], "passed");
    assert_eq!(parsed["goals"][1]["device"], TEST_UDID_2);
    assert_eq!(parsed["shards"][0]["goalsExecuted"], 0);
    assert!(
        parsed["shards"][0]["error"]
            .as_str()
            .unwrap_or("")
            .contains("WDA attach failed on shard 0")
    );
    assert_eq!(parsed["shards"][1]["goalsExecuted"], 2);
}

#[tokio::test]
async fn regression_deja66_r3_f5_lpt_dequeues_longest_unconstrained_goal_before_shorter_constrained_goals()
 {
    let ws = TempWs::new("deja66-r3-f5-lpt-longest-first");
    ws.write_build("build/App.bin", b"build-v1");
    // Strict replay (no `staleBuild: replay-last-verified`): each shard can only replay goals
    // verified in its own store for this build + runtime.
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1",
            "buildPath": "build/App.bin",
            "manifest": "deja.walks.json"
        })
        .to_string(),
    );
    write_tagged_flow(&ws, "flows/short_d1.yaml", "short-d1");
    write_tagged_flow(&ws, "flows/short_d2.yaml", "short-d2");
    write_tagged_flow(&ws, "flows/long_both.yaml", "long-both");
    seed_manifest_walk(
        &ws,
        &[
            (
                "short d1",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/short_d1.yaml",
            ),
            (
                "short d2",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/short_d2.yaml",
            ),
            (
                "long both",
                TEST_APP_ID,
                "ios",
                "maestro-ios/v1",
                "flows/long_both.yaml",
            ),
        ],
    );

    let fp = fingerprint_file(&ws.path, &ws.path.join("build/App.bin")).unwrap();
    let seed_store_goal = |store_id: &str, goal: &str, flow_rel: &str, dur_ms: u64| {
        let cl = collect_flow_closure(&ws.path, &ws.path.join(flow_rel)).unwrap();
        let main_sha = cl
            .files
            .iter()
            .find(|f| f.path == cl.main)
            .unwrap()
            .sha256
            .clone();
        let store = SqliteWalkStore::open_writable(
            SqliteStoreOptions::new(&ws.path).with_store_id(store_id),
        )
        .unwrap();
        store
            .upsert_walk(&deja_store::UpsertWalkInput {
                identity: WalkIdentity {
                    goal: goal.to_string(),
                    app_id: TEST_APP_ID.to_string(),
                    platform: "ios".to_string(),
                    flow_schema_version: DEFAULT_FLOW_SCHEMA_VERSION.to_string(),
                    build_fingerprint: fp.clone(),
                    adapter: Some("maestro-ios/v1".to_string()),
                    browser: None,
                },
                flow_path: flow_rel.to_string(),
                flow_sha256: main_sha,
                subflow_files: None,
                legacy_closure: false,
                closure_sha256: Some(cl.sha256),
                validation: RecordValidationInput::new(true, now_iso8601())
                    .with_runtime("ios-27.2")
                    .with_duration_ms(dur_ms),
                exploration: None,
                repair_history: None,
            })
            .unwrap();
        store.close();
    };

    // `long both` (2000ms) is eligible on BOTH `TEST_UDID` and `TEST_UDID_2`.
    // `short d1` (100ms) is constrained to `TEST_UDID` ONLY.
    // `short d2` (100ms) is constrained to `TEST_UDID_2` ONLY.
    seed_store_goal(TEST_UDID, "long both", "flows/long_both.yaml", 2_000);
    seed_store_goal(TEST_UDID_2, "long both", "flows/long_both.yaml", 2_000);
    seed_store_goal(TEST_UDID, "short d1", "flows/short_d1.yaml", 100);
    seed_store_goal(TEST_UDID_2, "short d2", "flows/short_d2.yaml", 100);

    // Synchronize the very first goal dequeued by BOTH shards with a 2-party barrier:
    // under pure LPT-first eligible selection, one of the first two concurrently started goals
    // MUST be `"long-both"` (NEVER both `"short-d1"` and `"short-d2"` jumping ahead!).
    let first_wave: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let hook: AsyncGoalHook = {
        let wave = Arc::clone(&first_wave);
        let b = Arc::clone(&barrier);
        Arc::new(
            move |_dev: String, tag: String, _cancel: CancellationToken| {
                let wave = Arc::clone(&wave);
                let b = Arc::clone(&b);
                Box::pin(async move {
                    let in_first_wave = {
                        let mut g = wave.lock().unwrap();
                        if g.len() < 2 {
                            g.push(tag);
                            true
                        } else {
                            false
                        }
                    };
                    if in_first_wave {
                        let _ = tokio::time::timeout(Duration::from_secs(2), b.wait()).await;
                    }
                })
            },
        )
    };

    let factory = Arc::new(BatchMockFactory::new(Some("ios-27.2")).with_on_goal(hook));
    let (deps, cap) = make_deps(
        &ws.path,
        Arc::clone(&factory) as Arc<dyn DriverFactory>,
        default_probes(),
        CancellationToken::new(),
    );
    let devices_arg = format!("{TEST_UDID},{TEST_UDID_2}");
    let code = run_cli_from_args(
        ["deja", "run", "--all", "--devices", &devices_arg, "--json"],
        &deps,
    )
    .await;
    assert_eq!(
        code,
        0,
        "all 3 goals must pass; stdout={} stderr={}",
        cap.lock().unwrap().stdout,
        cap.lock().unwrap().stderr
    );
    let wave = first_wave.lock().unwrap().clone();
    assert!(
        wave.iter().any(|t| t == "long-both"),
        "longest unconstrained goal (2000ms) MUST be dequeued in the first wave before both 100ms constrained goals; got first_wave={wave:?}"
    );
}

#[test]
fn mcp_subprocess_stdio_roundtrip_zero_stdout_leak_and_rejects_json_flag() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::Stdio;

    let ws = TempWs::new("mcp-subprocess");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1"
        })
        .to_string(),
    );

    // 1. `deja mcp --json` must exit 2 with zero stdout bytes.
    let bad = deja_bin()
        .current_dir(&ws.path)
        .args(["mcp", "--json"])
        .output()
        .expect("spawn deja mcp --json");
    assert_eq!(bad.status.code(), Some(2));
    assert!(
        bad.stdout.is_empty(),
        "deja mcp --json must not write anything to stdout, got: {:?}",
        String::from_utf8_lossy(&bad.stdout)
    );
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("--json"),
        "expected stderr usage message for --json"
    );

    // 2. Real stdin/stdout JSON-RPC 2.0 session with DEJA_LOG=debug verifying ZERO stdout log leakage.
    let mut child = deja_bin()
        .current_dir(&ws.path)
        .env("DEJA_LOG", "debug")
        .args(["mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn deja mcp");

    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stdout_reader = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut stderr = child.stderr.take().expect("piped stderr");

    let read_json_line = |reader: &mut BufReader<std::process::ChildStdout>| -> Value {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("read stdout line");
        assert!(n > 0, "unexpected EOF reading MCP stdout");
        serde_json::from_str(line.trim())
            .unwrap_or_else(|err| panic!("stdout contained non-JSON-RPC output ({err}): {line:?}"))
    };

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}})
    )
    .unwrap();
    stdin.flush().unwrap();
    let r1 = read_json_line(&mut stdout_reader);
    assert_eq!(r1["id"], 1);
    assert_eq!(r1["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(r1["result"]["serverInfo"]["name"], "deja");

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
    )
    .unwrap();
    stdin.flush().unwrap();
    let r2 = read_json_line(&mut stdout_reader);
    assert_eq!(r2["id"], 2);
    let tool_names: Vec<&str> = r2["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(tool_names.contains(&"deja_list"));
    assert!(tool_names.contains(&"deja_exec_batch"));
    assert!(!tool_names.contains(&"deja_explore"));

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "deja_list", "arguments": {}}})
    )
    .unwrap();
    stdin.flush().unwrap();
    let r3 = read_json_line(&mut stdout_reader);
    assert_eq!(r3["id"], 3);
    assert_eq!(r3["result"]["isError"], false);
    assert_eq!(r3["result"]["structuredContent"]["count"], 0);

    // Close stdin (EOF) -> server must exit 0 with zero trailing stdout bytes.
    drop(stdin);
    let mut trailing = String::new();
    stdout_reader.read_to_string(&mut trailing).unwrap();
    assert!(
        trailing.is_empty(),
        "expected zero trailing stdout bytes after EOF, got: {trailing:?}"
    );

    let status = child.wait().expect("wait on deja mcp");
    let mut stderr_text = String::new();
    let _ = stderr.read_to_string(&mut stderr_text);
    assert_eq!(
        status.code(),
        Some(0),
        "deja mcp must exit 0 on EOF; stderr:\n{stderr_text}"
    );
}

#[cfg(unix)]
#[test]
fn regression_r1_f3_mcp_exits_promptly_on_sigterm_with_stdin_open() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    use std::time::Instant;

    let ws = TempWs::new("r1-f3-mcp-sigterm");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1"
        })
        .to_string(),
    );

    let mut child = deja_bin()
        .current_dir(&ws.path)
        .args(["mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn deja mcp");

    // Keep `_stdin` OPEN throughout SIGTERM handling!
    let mut _stdin = child.stdin.take().expect("piped stdin");
    let mut stdout_reader = BufReader::new(child.stdout.take().expect("piped stdout"));

    writeln!(
        _stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}})
    )
    .unwrap();
    _stdin.flush().unwrap();

    let mut init_line = String::new();
    let n = stdout_reader
        .read_line(&mut init_line)
        .expect("read initialize response");
    assert!(n > 0);
    let init_resp: Value = serde_json::from_str(init_line.trim()).unwrap();
    assert_eq!(init_resp["id"], 1);

    // Send SIGTERM while parent STILL holds `_stdin` open!
    deja_driver_ios::send_sigterm_to_pid(child.id()).expect("send SIGTERM to deja mcp");

    let start = Instant::now();
    let exit_status = loop {
        if let Some(status) = child.try_wait().expect("try_wait on deja mcp") {
            break status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "deja mcp hung on SIGTERM while stdin remained open"
        );
        std::thread::sleep(Duration::from_millis(25));
    };

    assert_eq!(
        exit_status.code(),
        Some(143),
        "expected exit code 143 on SIGTERM, got: {exit_status:?}"
    );
}

#[test]
fn regression_r2_f8_invalid_mcp_store_arg_not_echoed_to_stderr() {
    let ws = TempWs::new("r2-f8-store-redact");
    ws.write(
        "deja.config.json",
        &json!({
            "workspaceRoot": ".",
            "platform": "ios",
            "appId": TEST_APP_ID,
            "adapter": "maestro-ios/v1"
        })
        .to_string(),
    );

    let secret_store_path = ["/", "Users", "/secret-home/foo"].concat();
    let out = deja_bin()
        .current_dir(&ws.path)
        .args(["mcp", "--store", &secret_store_path])
        .output()
        .expect("spawn deja mcp --store");

    assert_eq!(out.status.code(), Some(2));
    assert!(
        out.stdout.is_empty(),
        "invalid --store must not write to stdout"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid `--store`"),
        "expected invalid --store diagnostic, got: {stderr}"
    );
    assert!(
        !stderr.contains(&secret_store_path) && !stderr.contains("secret-home"),
        "raw rejected --store path must never be echoed to stderr: {stderr}"
    );
}
