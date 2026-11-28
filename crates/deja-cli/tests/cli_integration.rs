//! Comprehensive deterministic CLI integration tests (subprocess + mock-driver in-process).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use deja_cli::driver_factory::{
    DoctorCheck, DoctorCheckStatus, DoctorProbeRequest, DoctorProbes, DriverContext, DriverFactory,
    DriverFactoryError, FactoryFuture,
};
use deja_cli::output::OutputSink;
use deja_cli::{CliDeps, EMBEDDED_AGENT_GUIDE_MD, run_cli_from_args};
use deja_core::{
    CrashSignalSource, DEFAULT_FLOW_SCHEMA_VERSION, WalkIdentity, WalkOrigin, assertions_sha256,
    collect_flow_closure, fingerprint_file,
};
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

#[derive(Clone)]
struct MockDriver {
    caps: DriverCapabilities,
    hierarchies: HierarchyScript,
    launch_count: Arc<AtomicUsize>,
    shutdown_count: Arc<AtomicUsize>,
    crash_signal: Arc<Mutex<Option<DriverCrashSignal>>>,
    on_launch: Option<OnLaunchHook>,
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
            crash_signal: Arc::new(Mutex::new(None)),
            on_launch: None,
        }
    }

    fn with_crash(self, sig: DriverCrashSignal) -> Self {
        *self.crash_signal.lock().unwrap() = Some(sig);
        self
    }

    fn with_on_launch(mut self, hook: OnLaunchHook) -> Self {
        self.on_launch = Some(hook);
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
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Option<DriverCrashSignal>> {
        Box::pin(async move { Ok(self.crash_signal.lock().unwrap().clone()) })
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
}

impl MockFactory {
    fn new(runtime: Option<&str>, driver: MockDriver) -> Self {
        Self {
            runtime: runtime.map(ToString::to_string),
            driver,
            create_calls: Arc::new(AtomicUsize::new(0)),
        }
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
        _ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Box<dyn PlatformDriver>, DriverFactoryError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverFactoryError::Cancelled);
            }
            self.create_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(self.driver.clone()) as Box<dyn PlatformDriver>)
        })
    }

    fn install_app<'a>(
        &'a self,
        _ctx: &'a DriverContext,
        _cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<(), DriverFactoryError>> {
        Box::pin(async { Ok(()) })
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
