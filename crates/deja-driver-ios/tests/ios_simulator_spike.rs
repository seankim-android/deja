use deja_driver_ios::{
    ActKind, ActRequest, AgentClient, HierarchyResponse, IosDriverError, IosRunnerConfig,
    IosRunnerSession, KeyRequest, Simctl, SimctlEnv, SwipeRequest, TapRequest, TextMatchMode,
    TypeRequest, WaitForSelectorRequest, WarmupRequest, compute_hierarchy_hash,
    find_xcodebuild_test_pids_for_udid, send_sigterm_to_pid, validate_udid,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const FIXTURE_BUNDLE_ID: &str = "dev.deja.ios.checkout";
const SANITIZED_SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "deja-ios-spike-{prefix}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
        }
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn require_test_udid() -> String {
    let raw = std::env::var("DEJA_IOS_TEST_UDID")
        .expect("DEJA_IOS_TEST_UDID must be set to a dedicated iOS simulator UDID");
    validate_udid(&raw).expect("DEJA_IOS_TEST_UDID must be a valid UUID")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root should exist")
}

fn sanitized_simctl_env() -> SimctlEnv {
    SimctlEnv {
        env_path: Some(OsString::from(SANITIZED_SYSTEM_PATH)),
        developer_dir: std::env::var_os("DEVELOPER_DIR"),
    }
}

async fn assert_no_maestro_or_java_on_sanitized_path() {
    let maestro = Command::new("/usr/bin/which")
        .arg("maestro")
        .env("PATH", SANITIZED_SYSTEM_PATH)
        .output()
        .await
        .expect("run which maestro");
    assert!(
        !maestro.status.success(),
        "maestro must not be resolvable on sanitized PATH `{SANITIZED_SYSTEM_PATH}`"
    );

    // On macOS, `/usr/bin/java` may exist as an Apple stub; verify no real Maestro/Java toolchain
    // is on our PATH (neither `maestro` nor a third-party Homebrew/SDKMAN/asdf/JDK PATH entry).
    for segment in SANITIZED_SYSTEM_PATH.split(':') {
        let lower = segment.to_ascii_lowercase();
        assert!(
            !lower.contains("maestro") && !lower.contains("java") && !lower.contains("jdk"),
            "sanitized PATH must not contain Maestro or JDK directories: {segment}"
        );
    }
}

async fn build_deja_fixture(derived_data: &Path, env: &SimctlEnv) -> PathBuf {
    let project = repo_root().join("spikes/003-ios-deja/DejaFixture.xcodeproj");
    assert!(
        project.exists(),
        "DejaFixture.xcodeproj missing at {}",
        project.display()
    );

    let mut cmd = Command::new("xcodebuild");
    cmd.arg("-project")
        .arg(&project)
        .args([
            "-scheme",
            "DejaFixture",
            "-configuration",
            "Debug",
            "-sdk",
            "iphonesimulator",
            "-derivedDataPath",
        ])
        .arg(derived_data)
        .args(["CODE_SIGNING_ALLOWED=NO", "build"])
        .stdin(Stdio::null());
    if let Some(ref path) = env.env_path {
        cmd.env("PATH", path);
    }
    if let Some(ref dev_dir) = env.developer_dir {
        cmd.env("DEVELOPER_DIR", dev_dir);
    }

    let output = cmd
        .output()
        .await
        .expect("spawn xcodebuild for DejaFixture");
    assert!(
        output.status.success(),
        "DejaFixture build failed (exit={:?}):\nstderr:\n{}\nstdout:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );

    let app_path = derived_data.join("Build/Products/Debug-iphonesimulator/DejaFixture.app");
    assert!(
        app_path.is_dir(),
        "expected built DejaFixture.app at {}",
        app_path.display()
    );
    app_path
}

async fn wait_for_hierarchy<F>(
    client: &AgentClient,
    app_id: &str,
    include_system_ui: bool,
    timeout: Duration,
    latencies_ms: &mut Vec<f64>,
    description: &str,
    mut predicate: F,
) -> HierarchyResponse
where
    F: FnMut(&HierarchyResponse) -> bool,
{
    let deadline = Instant::now() + timeout;
    let mut last_resp: Option<HierarchyResponse> = None;

    while Instant::now() < deadline {
        let t0 = Instant::now();
        let resp = client
            .hierarchy(app_id, include_system_ui)
            .await
            .expect("GET /hierarchy should succeed");
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        latencies_ms.push(elapsed_ms);

        if predicate(&resp) {
            return resp;
        }
        last_resp = Some(resp);
        tokio::time::sleep(Duration::from_millis(120)).await;
    }

    panic!(
        "timed out waiting for hierarchy condition `{description}`; last response: {last_resp:#?}"
    );
}

async fn tap_id(client: &AgentClient, hierarchy: &HierarchyResponse, id: &str) {
    let node = hierarchy
        .find_by_id(id)
        .unwrap_or_else(|| panic!("expected node with identifier `{id}` in hierarchy"));
    let (x, y) = node.frame.center();
    assert!(
        x > 0.0 && y > 0.0,
        "node `{id}` has invalid frame {:?}",
        node.frame
    );
    let resp = client
        .tap(&TapRequest {
            app_id: FIXTURE_BUNDLE_ID.to_string(),
            x,
            y,
            count: Some(1),
            duration_ms: None,
        })
        .await
        .unwrap_or_else(|err| panic!("POST /tap on `{id}` ({x}, {y}) failed: {err}"));
    assert!(resp.ok, "POST /tap on `{id}` returned ok=false");
}

fn summarize_latencies(samples: &[f64]) -> (f64, f64, f64, f64) {
    assert!(!samples.is_empty(), "expected at least one latency sample");
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let min = sorted[0];
    let max = *sorted.last().unwrap();
    let mid = sorted.len() / 2;
    let median = if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    };
    let p95_idx = ((sorted.len() as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    let p95 = sorted[p95_idx];
    (min, median, p95, max)
}

#[tokio::test]
#[ignore = "requires macOS + Xcode + dedicated iOS simulator (DEJA_IOS_TEST_UDID)"]
async fn spike_deja_fixture_checkout_no_maestro() {
    let udid = require_test_udid();
    assert_no_maestro_or_java_on_sanitized_path().await;

    let env = sanitized_simctl_env();
    let simctl = Simctl::with_env(env.clone());
    simctl
        .ensure_booted(&udid)
        .await
        .expect("boot dedicated iOS simulator");

    let runtimes = simctl.list_runtimes().await.expect("list runtimes");
    assert!(
        !runtimes.is_empty(),
        "expected at least one simulator runtime"
    );

    let fixture_build_dir = TempDirGuard::new("fixture-build");
    let app_path = build_deja_fixture(fixture_build_dir.path(), &env).await;
    simctl
        .install_app(&udid, &app_path)
        .await
        .expect("install DejaFixture.app");

    let deja_home = TempDirGuard::new("deja-home");
    let cancel = CancellationToken::new();

    // 1. Cold build + first attach (empty cache).
    let mut cold_cfg = IosRunnerConfig::new(&udid);
    cold_cfg.deja_home = Some(deja_home.path().to_path_buf());
    cold_cfg.env = env.clone();
    cold_cfg.startup_timeout = Duration::from_secs(90);

    let cold_session = IosRunnerSession::start(cold_cfg, cancel.clone())
        .await
        .expect("cold runner start should succeed");
    assert!(
        !cold_session.build_artifact().was_cache_hit,
        "first start in fresh DEJA_HOME must be a cold build"
    );
    let cold_build_ms = cold_session.build_artifact().build_duration_ms;
    let cold_attach_ms = cold_session.attach_duration().as_millis() as u64;

    let health = cold_session
        .client()
        .health()
        .await
        .expect("GET /health should return 200");
    assert_eq!(health.status, "ok");
    assert_eq!(health.agent, "DejaXCTestRunner");
    assert_eq!(health.version, "1");

    // Verify constant-time auth rejects invalid tokens on /health with 401.
    let bad_client = cold_session
        .client()
        .with_token("0".repeat(64), Duration::from_secs(5))
        .expect("construct bad token client");
    match bad_client.health().await {
        Err(IosDriverError::AgentHttpStatus { status: 401, .. }) => {}
        other => panic!("expected 401 Unauthorized for wrong token, got: {other:?}"),
    }

    cold_session
        .shutdown()
        .await
        .expect("cold session shutdown");
    let after_cold = find_xcodebuild_test_pids_for_udid(&udid).expect("ps check");
    assert!(
        after_cold.is_empty(),
        "expected 0 xcodebuild processes after cold session shutdown, found {after_cold:?}"
    );

    // 2. Warm attach (cache hit).
    let mut warm_cfg = IosRunnerConfig::new(&udid);
    warm_cfg.deja_home = Some(deja_home.path().to_path_buf());
    warm_cfg.env = env.clone();
    warm_cfg.startup_timeout = Duration::from_secs(60);

    let session = IosRunnerSession::start(warm_cfg, cancel.clone())
        .await
        .expect("warm runner start should succeed");
    assert!(
        session.build_artifact().was_cache_hit,
        "second start in same DEJA_HOME must be a cache hit"
    );
    let warm_attach_ms = session.attach_duration().as_millis() as u64;
    let client = session.client();

    // 3. Full DejaFixture checkout flow with DEJA_FIXTURE_ORDER=WC-2026.
    let flow_started = Instant::now();
    let mut hierarchy_latencies_ms = Vec::new();

    simctl
        .clear_state(&udid, FIXTURE_BUNDLE_ID)
        .await
        .expect("clear_state DejaFixture");

    let mut launch_args = BTreeMap::new();
    launch_args.insert("DEJA_FIXTURE_ORDER".to_string(), "WC-2026".to_string());
    let launch_env = BTreeMap::new();
    let app_pid = simctl
        .launch_app(&udid, FIXTURE_BUNDLE_ID, &launch_args, &launch_env)
        .await
        .expect("launch DejaFixture");
    assert!(app_pid.is_some(), "simctl launch should return a PID");

    // Wait for running_foreground via /appState.
    let state_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let app_state = client
            .app_state(FIXTURE_BUNDLE_ID)
            .await
            .expect("GET /appState");
        if app_state.state == "running_foreground" && app_state.state_code == 4 {
            break;
        }
        assert!(
            Instant::now() < state_deadline,
            "timed out waiting for running_foreground, last state: {app_state:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Login screen.
    let login_tree = wait_for_hierarchy(
        client,
        FIXTURE_BUNDLE_ID,
        false,
        Duration::from_secs(10),
        &mut hierarchy_latencies_ms,
        "login screen",
        |h| h.find_by_id("screen_login").is_some() && h.contains_text("Déjà iOS Shop"),
    )
    .await;
    assert!(
        login_tree.nodes.iter().all(|n| n.source == "app"),
        "foreground hierarchy with includeSystemUi=false should not query SpringBoard"
    );

    tap_id(client, &login_tree, "username_input").await;
    let type_email = client
        .type_text(&TypeRequest {
            app_id: FIXTURE_BUNDLE_ID.to_string(),
            text: "qa@example.com".to_string(),
            settle_keyboard: Some(true),
        })
        .await
        .expect("POST /type email");
    assert!(type_email.ok);

    let after_email = wait_for_hierarchy(
        client,
        FIXTURE_BUNDLE_ID,
        false,
        Duration::from_secs(10),
        &mut hierarchy_latencies_ms,
        "email value visible",
        |h| h.contains_text("qa@example.com"),
    )
    .await;

    tap_id(client, &after_email, "password_input").await;
    let type_pw = client
        .type_text(&TypeRequest {
            app_id: FIXTURE_BUNDLE_ID.to_string(),
            text: "testing123".to_string(),
            settle_keyboard: Some(true),
        })
        .await
        .expect("POST /type password");
    assert!(type_pw.ok);

    let ready_login = wait_for_hierarchy(
        client,
        FIXTURE_BUNDLE_ID,
        false,
        Duration::from_secs(10),
        &mut hierarchy_latencies_ms,
        "login button visible",
        |h| h.find_by_id("login_button").is_some(),
    )
    .await;
    tap_id(client, &ready_login, "login_button").await;

    // Catalog screen.
    let catalog_tree = wait_for_hierarchy(
        client,
        FIXTURE_BUNDLE_ID,
        false,
        Duration::from_secs(10),
        &mut hierarchy_latencies_ms,
        "catalog screen",
        |h| {
            h.find_by_id("screen_catalog").is_some()
                && h.contains_text("Mochi sticker pack")
                && h.contains_text("$5.00 · 6 stickers")
        },
    )
    .await;
    tap_id(client, &catalog_tree, "add_to_cart").await;

    // Cart screen.
    let cart_tree = wait_for_hierarchy(
        client,
        FIXTURE_BUNDLE_ID,
        false,
        Duration::from_secs(10),
        &mut hierarchy_latencies_ms,
        "cart screen",
        |h| {
            h.find_by_id("screen_cart").is_some()
                && h.contains_text("1 × Mochi sticker pack")
                && h.contains_text("Total: $5.00")
        },
    )
    .await;
    tap_id(client, &cart_tree, "checkout_button").await;

    // Confirmation screen with custom WC-2026 launch arg.
    let confirm_tree = wait_for_hierarchy(
        client,
        FIXTURE_BUNDLE_ID,
        false,
        Duration::from_secs(10),
        &mut hierarchy_latencies_ms,
        "order confirmation #WC-2026",
        |h| {
            h.find_by_id("order_confirmation").is_some()
                && h.contains_text("Order confirmed")
                && h.contains_text("Order #WC-2026 · Mochi sticker pack")
        },
    )
    .await;
    let flow_duration_ms = flow_started.elapsed().as_millis() as u64;
    assert!(!confirm_tree.truncated);

    // Screenshot verification (PNG magic + non-trivial bytes).
    let screenshot_dir = TempDirGuard::new("screenshot");
    let screenshot_path = screenshot_dir.path().join("confirmation.png");
    let png_bytes = simctl
        .screenshot(&udid, &screenshot_path)
        .await
        .expect("capture PNG screenshot");
    assert!(
        png_bytes.len() > 1_024,
        "expected non-trivial PNG screenshot, got {} bytes",
        png_bytes.len()
    );

    // Collect additional /hierarchy samples for robust min/median/p95/max latency statistics.
    for _ in 0..10 {
        let t0 = Instant::now();
        let h = client
            .hierarchy(FIXTURE_BUNDLE_ID, false)
            .await
            .expect("benchmark /hierarchy");
        hierarchy_latencies_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert!(h.contains_text("Order #WC-2026 · Mochi sticker pack"));
    }

    // Exercise POST /swipe and POST /key ("home") to verify all endpoints live.
    let swipe_resp = client
        .swipe(&SwipeRequest {
            app_id: FIXTURE_BUNDLE_ID.to_string(),
            start_x: 200.0,
            start_y: 500.0,
            end_x: 200.0,
            end_y: 250.0,
            duration_ms: Some(300),
            hold_duration_ms: None,
        })
        .await
        .expect("POST /swipe");
    assert!(swipe_resp.ok);

    let key_resp = client
        .press_key(&KeyRequest {
            app_id: FIXTURE_BUNDLE_ID.to_string(),
            key: "home".to_string(),
            modifiers: Vec::new(),
            count: Some(1),
        })
        .await
        .expect("POST /key home");
    assert!(key_resp.ok);

    // Verify background app state + automatic SpringBoard hierarchy inclusion.
    let bg_deadline = Instant::now() + Duration::from_secs(8);
    let mut saw_springboard = false;
    while Instant::now() < bg_deadline {
        let state = client
            .app_state(FIXTURE_BUNDLE_ID)
            .await
            .expect("GET /appState after home");
        if state.state != "running_foreground" {
            let h = client
                .hierarchy(FIXTURE_BUNDLE_ID, false)
                .await
                .expect("GET /hierarchy while backgrounded");
            if h.nodes.iter().any(|n| n.source == "springboard") {
                saw_springboard = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert!(
        saw_springboard,
        "expected SpringBoard nodes in /hierarchy when app is not foreground"
    );

    let (h_min, h_median, h_p95, h_max) = summarize_latencies(&hierarchy_latencies_ms);
    let toolchain = session.build_artifact().toolchain.clone();
    let cache_key = session.build_artifact().cache_key.clone();

    session.shutdown().await.expect("warm session shutdown");
    let orphans = find_xcodebuild_test_pids_for_udid(&udid).expect("ps orphan check");
    assert!(
        orphans.is_empty(),
        "expected 0 orphaned xcodebuild processes after shutdown, found {orphans:?}"
    );

    println!("=== DEJA-60 SPIKE EVIDENCE (CHECKOUT) ===");
    println!(
        "toolchain: Xcode {} ({}) / iOS Simulator SDK {}",
        toolchain.xcode_version, toolchain.xcode_build, toolchain.ios_sdk
    );
    println!("cache_key: {cache_key}");
    println!("cold_build_ms: {cold_build_ms}");
    println!("cold_attach_ms: {cold_attach_ms}");
    println!("warm_attach_ms: {warm_attach_ms}");
    println!("full_flow_ms: {flow_duration_ms}");
    println!(
        "warm_attach_plus_flow_ms: {}",
        warm_attach_ms + flow_duration_ms
    );
    println!(
        "hierarchy_samples: {} | min={:.1}ms median={:.1}ms p95={:.1}ms max={:.1}ms",
        hierarchy_latencies_ms.len(),
        h_min,
        h_median,
        h_p95,
        h_max
    );
    println!("screenshot_png_bytes: {}", png_bytes.len());
    println!("orphaned_xcodebuild_pids: {}", orphans.len());
}

#[derive(Debug, Serialize, Deserialize)]
struct SigtermChildReady {
    port: u16,
    token: String,
    child_pid: u32,
    xcodebuild_pid: Option<u32>,
}

#[tokio::test]
#[ignore = "internal child helper invoked by spike_sigterm_mid_run_leaves_zero_orphans"]
async fn spike_sigterm_child_helper() {
    let Some(ready_path) = std::env::var_os("DEJA_SIG_READY_FILE") else {
        return;
    };
    let udid = require_test_udid();
    let cancel = CancellationToken::new();

    let mut cfg = IosRunnerConfig::new(&udid);
    cfg.env = sanitized_simctl_env();
    cfg.startup_timeout = Duration::from_secs(90);

    let session = IosRunnerSession::start(cfg, cancel.clone())
        .await
        .expect("child helper runner start");
    let health = session.client().health().await.expect("child /health");
    assert_eq!(health.status, "ok");

    // Register SIGTERM handler BEFORE publishing the ready file so parent cannot race ahead.
    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("register SIGTERM handler");

    let payload = SigtermChildReady {
        port: session.port(),
        token: session.client().token().to_string(),
        child_pid: std::process::id(),
        xcodebuild_pid: session.xcodebuild_pid(),
    };
    let tmp_path = PathBuf::from(&ready_path).with_extension("tmp");
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp_path).expect("create private tmp ready file");
        file.write_all(&serde_json::to_vec(&payload).unwrap())
            .expect("write tmp ready");
    }
    std::fs::rename(&tmp_path, &ready_path).expect("rename ready");

    #[cfg(unix)]
    {
        let _ = tokio::time::timeout(Duration::from_secs(30), sigterm.recv()).await;
    }

    cancel.cancel();
    session.shutdown().await.expect("child helper shutdown");
}

#[tokio::test]
#[ignore = "requires macOS + Xcode + dedicated iOS simulator (DEJA_IOS_TEST_UDID)"]
async fn spike_sigterm_mid_run_leaves_zero_orphans() {
    let udid = require_test_udid();
    let simctl = Simctl::with_env(sanitized_simctl_env());
    simctl
        .ensure_booted(&udid)
        .await
        .expect("boot dedicated iOS simulator");

    let temp_dir = TempDirGuard::new("sigterm");
    let ready_file = temp_dir.path().join("child-ready.json");
    let deja_home = temp_dir.path().join("deja-home");
    std::fs::create_dir_all(&deja_home).expect("create child deja_home");

    let current_exe = std::env::current_exe().expect("current test binary");
    let mut child = Command::new(&current_exe)
        .args([
            "spike_sigterm_child_helper",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env("DEJA_IOS_TEST_UDID", &udid)
        .env("DEJA_SIG_READY_FILE", &ready_file)
        .env("DEJA_HOME", &deja_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sigterm child helper");

    let child_pid = child.id().expect("child pid");
    let ready_deadline = Instant::now() + Duration::from_secs(120);
    let ready: SigtermChildReady = loop {
        if let Ok(bytes) = std::fs::read(&ready_file)
            && let Ok(parsed) = serde_json::from_slice::<SigtermChildReady>(&bytes)
        {
            break parsed;
        }
        if let Ok(Some(status)) = child.try_wait() {
            let output = child.wait_with_output().await.expect("read child output");
            panic!(
                "sigterm child exited early with {status:?}:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(
            Instant::now() < ready_deadline,
            "timed out waiting for sigterm child helper ready file"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // Confirm /health is 200 OK and xcodebuild test-without-building is alive for this UDID.
    let client = AgentClient::new(ready.port, ready.token.clone(), Duration::from_secs(5))
        .expect("build client");
    let health = client
        .health()
        .await
        .expect("parent /health before SIGTERM");
    assert_eq!(health.status, "ok");

    let live_pids = find_xcodebuild_test_pids_for_udid(&udid).expect("ps before SIGTERM");
    assert!(
        !live_pids.is_empty(),
        "expected >= 1 live xcodebuild test-without-building process for {udid} before SIGTERM"
    );

    // Send real SIGTERM to child process and verify graceful zero-orphan exit.
    let sigterm_sent = Instant::now();
    send_sigterm_to_pid(child_pid).expect("send SIGTERM to child helper");

    let exit_status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("child helper should exit promptly on SIGTERM")
        .expect("wait on child helper");
    let sigterm_exit_ms = sigterm_sent.elapsed().as_millis() as u64;
    assert!(
        exit_status.success(),
        "child helper should exit cleanly after SIGTERM shutdown, got {exit_status:?}"
    );

    // Allow a brief settle tick and assert zero orphaned xcodebuild processes remain.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let remaining_pids = find_xcodebuild_test_pids_for_udid(&udid).expect("ps after SIGTERM");
    assert!(
        remaining_pids.is_empty(),
        "expected 0 orphaned xcodebuild processes after SIGTERM, found {remaining_pids:?}"
    );

    println!("=== DEJA-60 SPIKE EVIDENCE (SIGTERM) ===");
    println!("live_xcodebuild_pids_before_sigterm: {live_pids:?}");
    println!("sigterm_cleanup_exit_ms: {sigterm_exit_ms}");
    println!(
        "orphaned_xcodebuild_pids_after_sigterm: {}",
        remaining_pids.len()
    );
}

async fn run_baseline_checkout_flow(
    simctl: &Simctl,
    udid: &str,
    client: &AgentClient,
    order_code: &str,
) -> (u64, u32) {
    let started = Instant::now();
    let mut http_calls: u32 = 0;

    simctl
        .clear_state(udid, FIXTURE_BUNDLE_ID)
        .await
        .expect("clear_state DejaFixture");

    let mut launch_args = BTreeMap::new();
    launch_args.insert("DEJA_FIXTURE_ORDER".to_string(), order_code.to_string());
    let launch_env = BTreeMap::new();
    let app_pid = simctl
        .launch_app(udid, FIXTURE_BUNDLE_ID, &launch_args, &launch_env)
        .await
        .expect("launch DejaFixture");
    assert!(app_pid.is_some());

    let state_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        http_calls += 1;
        let app_state = client
            .app_state(FIXTURE_BUNDLE_ID)
            .await
            .expect("GET /appState");
        if app_state.state == "running_foreground" && app_state.state_code == 4 {
            break;
        }
        assert!(
            Instant::now() < state_deadline,
            "timed out waiting for foreground"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let mut latencies = Vec::new();
    let login_tree = wait_for_hierarchy(
        client,
        FIXTURE_BUNDLE_ID,
        false,
        Duration::from_secs(10),
        &mut latencies,
        "baseline login screen",
        |h| h.find_by_id("screen_login").is_some() && h.contains_text("Déjà iOS Shop"),
    )
    .await;
    http_calls += latencies.len() as u32;
    latencies.clear();

    tap_id(client, &login_tree, "username_input").await;
    http_calls += 1;
    let type_email = client
        .type_text(&TypeRequest {
            app_id: FIXTURE_BUNDLE_ID.to_string(),
            text: "qa@example.com".to_string(),
            settle_keyboard: Some(true),
        })
        .await
        .expect("POST /type email");
    assert!(type_email.ok);
    http_calls += 1;

    let after_email = wait_for_hierarchy(
        client,
        FIXTURE_BUNDLE_ID,
        false,
        Duration::from_secs(10),
        &mut latencies,
        "baseline email visible",
        |h| h.contains_text("qa@example.com"),
    )
    .await;
    http_calls += latencies.len() as u32;
    latencies.clear();

    tap_id(client, &after_email, "password_input").await;
    http_calls += 1;
    let type_pw = client
        .type_text(&TypeRequest {
            app_id: FIXTURE_BUNDLE_ID.to_string(),
            text: "testing123".to_string(),
            settle_keyboard: Some(true),
        })
        .await
        .expect("POST /type password");
    assert!(type_pw.ok);
    http_calls += 1;

    let ready_login = wait_for_hierarchy(
        client,
        FIXTURE_BUNDLE_ID,
        false,
        Duration::from_secs(10),
        &mut latencies,
        "baseline login button",
        |h| h.find_by_id("login_button").is_some(),
    )
    .await;
    http_calls += latencies.len() as u32;
    latencies.clear();

    tap_id(client, &ready_login, "login_button").await;
    http_calls += 1;

    let catalog_tree = wait_for_hierarchy(
        client,
        FIXTURE_BUNDLE_ID,
        false,
        Duration::from_secs(10),
        &mut latencies,
        "baseline catalog",
        |h| {
            h.find_by_id("screen_catalog").is_some()
                && h.contains_text("Mochi sticker pack")
                && h.contains_text("$5.00 · 6 stickers")
        },
    )
    .await;
    http_calls += latencies.len() as u32;
    latencies.clear();

    tap_id(client, &catalog_tree, "add_to_cart").await;
    http_calls += 1;

    let cart_tree = wait_for_hierarchy(
        client,
        FIXTURE_BUNDLE_ID,
        false,
        Duration::from_secs(10),
        &mut latencies,
        "baseline cart",
        |h| {
            h.find_by_id("screen_cart").is_some()
                && h.contains_text("1 × Mochi sticker pack")
                && h.contains_text("Total: $5.00")
        },
    )
    .await;
    http_calls += latencies.len() as u32;
    latencies.clear();

    tap_id(client, &cart_tree, "checkout_button").await;
    http_calls += 1;

    let expected_order = format!("Order #{order_code} · Mochi sticker pack");
    let _confirm = wait_for_hierarchy(
        client,
        FIXTURE_BUNDLE_ID,
        false,
        Duration::from_secs(10),
        &mut latencies,
        "baseline confirmation",
        |h| {
            h.find_by_id("order_confirmation").is_some()
                && h.contains_text("Order confirmed")
                && h.contains_text(&expected_order)
        },
    )
    .await;
    http_calls += latencies.len() as u32;

    (started.elapsed().as_millis() as u64, http_calls)
}

async fn run_optimized_checkout_flow(
    simctl: &Simctl,
    udid: &str,
    client: &AgentClient,
    order_code: &str,
) -> (u64, u32, HierarchyResponse) {
    let started = Instant::now();
    let mut http_calls: u32 = 0;

    simctl
        .clear_state(udid, FIXTURE_BUNDLE_ID)
        .await
        .expect("clear_state DejaFixture");

    let mut launch_args = BTreeMap::new();
    launch_args.insert("DEJA_FIXTURE_ORDER".to_string(), order_code.to_string());
    let launch_env = BTreeMap::new();
    let app_pid = simctl
        .launch_app(udid, FIXTURE_BUNDLE_ID, &launch_args, &launch_env)
        .await
        .expect("launch DejaFixture");
    assert!(app_pid.is_some());

    // 1. Single POST /warmup waits for running_foreground, warms AX, and returns initial hierarchy.
    http_calls += 1;
    let warmup = client
        .warmup(&WarmupRequest {
            app_id: FIXTURE_BUNDLE_ID.to_string(),
            timeout_ms: Some(10_000),
            include_system_ui: Some(false),
            return_hierarchy: Some(true),
        })
        .await
        .expect("POST /warmup");
    assert!(warmup.ok);
    assert_eq!(warmup.state, "running_foreground");
    assert_eq!(warmup.state_code, 4);

    let login_tree = match warmup.hierarchy {
        Some(h) if h.find_by_id("username_input").is_some() && h.contains_text("Déjà iOS Shop") => {
            h
        }
        _ => {
            http_calls += 1;
            let wait = client
                .wait_for_selector(&WaitForSelectorRequest {
                    app_id: FIXTURE_BUNDLE_ID.to_string(),
                    identifier: Some("username_input".to_string()),
                    text: None,
                    text_match: None,
                    not_visible: None,
                    wait_stable: None,
                    stable_polls: None,
                    timeout_ms: Some(10_000),
                    include_system_ui: Some(false),
                    return_hierarchy: Some(true),
                })
                .await
                .expect("POST /waitForSelector login");
            assert!(wait.matched, "login screen should match");
            wait.hierarchy.expect("login hierarchy")
        }
    };
    assert_eq!(
        login_tree.hierarchy_hash,
        compute_hierarchy_hash(&login_tree.nodes),
        "Swift and Rust hierarchy hashes must match bit-for-bit on login screen"
    );

    // 2. Tap username_input, then compound type + returnHierarchy (gives password_input coords).
    tap_id(client, &login_tree, "username_input").await;
    http_calls += 1;

    http_calls += 1;
    let email_act = client
        .act(&ActRequest {
            app_id: FIXTURE_BUNDLE_ID.to_string(),
            kind: ActKind::Type {
                text: "qa@example.com".to_string(),
                settle_keyboard: Some(true),
            },
            post_action_delay_ms: None,
            return_hierarchy: Some(true),
            include_system_ui: Some(false),
            known_hash: None,
        })
        .await
        .expect("POST /act type email");
    assert!(email_act.ok);
    let after_email = email_act.hierarchy.expect("post-email hierarchy");
    assert!(after_email.contains_text("qa@example.com"));
    assert_eq!(
        after_email.hierarchy_hash,
        compute_hierarchy_hash(&after_email.nodes),
        "Swift and Rust hierarchy hashes must match after typing email"
    );

    // 3. Tap password_input, then compound type + returnHierarchy (gives login_button coords).
    tap_id(client, &after_email, "password_input").await;
    http_calls += 1;

    http_calls += 1;
    let pw_act = client
        .act(&ActRequest {
            app_id: FIXTURE_BUNDLE_ID.to_string(),
            kind: ActKind::Type {
                text: "testing123".to_string(),
                settle_keyboard: Some(true),
            },
            post_action_delay_ms: None,
            return_hierarchy: Some(true),
            include_system_ui: Some(false),
            known_hash: None,
        })
        .await
        .expect("POST /act type password");
    assert!(pw_act.ok);
    let ready_login = pw_act.hierarchy.expect("post-password hierarchy");

    // 4. Compound tap login_button + post-action hierarchy; fallback to server-side /waitForSelector.
    let login_btn = ready_login
        .find_by_id("login_button")
        .expect("login_button present");
    let (lx, ly) = login_btn.frame.center();
    http_calls += 1;
    let login_act = client
        .act(&ActRequest {
            app_id: FIXTURE_BUNDLE_ID.to_string(),
            kind: ActKind::Tap {
                x: lx,
                y: ly,
                count: Some(1),
                duration_ms: None,
            },
            post_action_delay_ms: None,
            return_hierarchy: Some(true),
            include_system_ui: Some(false),
            known_hash: None,
        })
        .await
        .expect("POST /act tap login_button");
    assert!(login_act.ok);

    let catalog_tree = match login_act.hierarchy {
        Some(h)
            if h.find_by_id("add_to_cart").is_some()
                && h.contains_text("Mochi sticker pack")
                && h.contains_text("$5.00 · 6 stickers") =>
        {
            h
        }
        _ => {
            http_calls += 1;
            let wait = client
                .wait_for_selector(&WaitForSelectorRequest {
                    app_id: FIXTURE_BUNDLE_ID.to_string(),
                    identifier: Some("add_to_cart".to_string()),
                    text: None,
                    text_match: None,
                    not_visible: None,
                    wait_stable: None,
                    stable_polls: None,
                    timeout_ms: Some(10_000),
                    include_system_ui: Some(false),
                    return_hierarchy: Some(true),
                })
                .await
                .expect("POST /waitForSelector add_to_cart");
            assert!(wait.matched, "catalog screen should appear");
            let h = wait.hierarchy.expect("catalog hierarchy");
            assert!(h.contains_text("Mochi sticker pack"));
            assert!(h.contains_text("$5.00 · 6 stickers"));
            h
        }
    };

    // 5. Compound tap add_to_cart + post-action hierarchy.
    let add_btn = catalog_tree
        .find_by_id("add_to_cart")
        .expect("add_to_cart present");
    let (ax, ay) = add_btn.frame.center();
    http_calls += 1;
    let add_act = client
        .act(&ActRequest {
            app_id: FIXTURE_BUNDLE_ID.to_string(),
            kind: ActKind::Tap {
                x: ax,
                y: ay,
                count: Some(1),
                duration_ms: None,
            },
            post_action_delay_ms: None,
            return_hierarchy: Some(true),
            include_system_ui: Some(false),
            known_hash: None,
        })
        .await
        .expect("POST /act tap add_to_cart");
    assert!(add_act.ok);

    let cart_tree = match add_act.hierarchy {
        Some(h)
            if h.find_by_id("checkout_button").is_some()
                && h.contains_text("1 × Mochi sticker pack")
                && h.contains_text("Total: $5.00") =>
        {
            h
        }
        _ => {
            http_calls += 1;
            let wait = client
                .wait_for_selector(&WaitForSelectorRequest {
                    app_id: FIXTURE_BUNDLE_ID.to_string(),
                    identifier: Some("checkout_button".to_string()),
                    text: None,
                    text_match: None,
                    not_visible: None,
                    wait_stable: None,
                    stable_polls: None,
                    timeout_ms: Some(10_000),
                    include_system_ui: Some(false),
                    return_hierarchy: Some(true),
                })
                .await
                .expect("POST /waitForSelector checkout_button");
            assert!(wait.matched, "cart screen should appear");
            let h = wait.hierarchy.expect("cart hierarchy");
            assert!(h.contains_text("1 × Mochi sticker pack"));
            assert!(h.contains_text("Total: $5.00"));
            h
        }
    };

    // 6. Compound tap checkout_button + verify confirmation screen.
    let checkout_btn = cart_tree
        .find_by_id("checkout_button")
        .expect("checkout_button present");
    let (cx, cy) = checkout_btn.frame.center();
    http_calls += 1;
    let checkout_act = client
        .act(&ActRequest {
            app_id: FIXTURE_BUNDLE_ID.to_string(),
            kind: ActKind::Tap {
                x: cx,
                y: cy,
                count: Some(1),
                duration_ms: None,
            },
            post_action_delay_ms: None,
            return_hierarchy: Some(true),
            include_system_ui: Some(false),
            known_hash: None,
        })
        .await
        .expect("POST /act tap checkout_button");
    assert!(checkout_act.ok);

    let expected_order = format!("Order #{order_code} · Mochi sticker pack");
    let confirm_tree = match checkout_act.hierarchy {
        Some(h)
            if h.find_by_id("order_confirmation").is_some()
                && h.contains_text("Order confirmed")
                && h.contains_text(&expected_order) =>
        {
            h
        }
        _ => {
            http_calls += 1;
            let wait = client
                .wait_for_selector(&WaitForSelectorRequest {
                    app_id: FIXTURE_BUNDLE_ID.to_string(),
                    identifier: None,
                    text: Some(expected_order.clone()),
                    text_match: Some(TextMatchMode::Contains),
                    not_visible: None,
                    wait_stable: None,
                    stable_polls: None,
                    timeout_ms: Some(10_000),
                    include_system_ui: Some(false),
                    return_hierarchy: Some(true),
                })
                .await
                .expect("POST /waitForSelector order confirmation");
            assert!(
                wait.matched,
                "order confirmation `{expected_order}` should appear"
            );
            let h = wait.hierarchy.expect("confirmation hierarchy");
            assert!(h.find_by_id("order_confirmation").is_some());
            assert!(h.contains_text("Order confirmed"));
            h
        }
    };
    assert_eq!(
        confirm_tree.hierarchy_hash,
        compute_hierarchy_hash(&confirm_tree.nodes),
        "Swift and Rust hierarchy hashes must match on confirmation screen"
    );

    (
        started.elapsed().as_millis() as u64,
        http_calls,
        confirm_tree,
    )
}

#[tokio::test]
#[ignore = "requires macOS + Xcode + dedicated iOS simulator (DEJA_IOS_TEST_UDID)"]
async fn benchmark_deja_fixture_optimized_and_session_reuse() {
    let udid = require_test_udid();
    assert_no_maestro_or_java_on_sanitized_path().await;

    let env = sanitized_simctl_env();
    let simctl = Simctl::with_env(env.clone());
    simctl
        .ensure_booted(&udid)
        .await
        .expect("boot dedicated iOS simulator");

    let fixture_build_dir = TempDirGuard::new("fixture-bench");
    let app_path = build_deja_fixture(fixture_build_dir.path(), &env).await;
    simctl
        .install_app(&udid, &app_path)
        .await
        .expect("install DejaFixture.app");

    let deja_home = TempDirGuard::new("deja-home-bench");
    let cancel = CancellationToken::new();

    // Warm build cache once, then measure warm attach.
    let mut seed_cfg = IosRunnerConfig::new(&udid);
    seed_cfg.deja_home = Some(deja_home.path().to_path_buf());
    seed_cfg.env = env.clone();
    seed_cfg.startup_timeout = Duration::from_secs(90);
    let seed = IosRunnerSession::start(seed_cfg, cancel.clone())
        .await
        .expect("seed runner start");
    seed.shutdown().await.expect("seed shutdown");

    let mut warm_cfg = IosRunnerConfig::new(&udid);
    warm_cfg.deja_home = Some(deja_home.path().to_path_buf());
    warm_cfg.env = env.clone();
    warm_cfg.startup_timeout = Duration::from_secs(60);
    let session = IosRunnerSession::start(warm_cfg, cancel.clone())
        .await
        .expect("warm runner start");
    assert!(session.build_artifact().was_cache_hit);
    let warm_attach_ms = session.attach_duration().as_millis() as u64;
    let client = session.client();

    // 1. Baseline legacy flow (per-endpoint /appState + /hierarchy poll loops + /tap + /type).
    let (baseline_flow_ms, baseline_http_calls) =
        run_baseline_checkout_flow(&simctl, &udid, client, "WC-2026").await;

    // 2. Optimized flow #1 on the same warm runner session (DEJA-77 endpoints).
    let verify_t0 = Instant::now();
    let health = session.verify_healthy().await.expect("verify_healthy #1");
    let verify_1_ms = verify_t0.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(health.status, "ok");

    let (optimized_flow_1_ms, optimized_flow_1_http_calls, confirm_tree) =
        run_optimized_checkout_flow(&simctl, &udid, client, "WC-2026").await;

    // 3. Verify GET /hierarchy?knownHash=<hash> short-circuit & compare latency.
    let mut full_samples_ms = Vec::new();
    let mut short_circuit_samples_ms = Vec::new();
    for _ in 0..8 {
        let t_full = Instant::now();
        let full = client
            .hierarchy(FIXTURE_BUNDLE_ID, false)
            .await
            .expect("full /hierarchy");
        full_samples_ms.push(t_full.elapsed().as_secs_f64() * 1000.0);
        assert!(!full.unchanged);
        assert_eq!(
            full.hierarchy_hash,
            compute_hierarchy_hash(&full.nodes),
            "live /hierarchy hash must match Rust compute_hierarchy_hash"
        );

        let t_short = Instant::now();
        let short = client
            .hierarchy_with_known_hash(FIXTURE_BUNDLE_ID, false, Some(&confirm_tree.hierarchy_hash))
            .await
            .expect("short-circuit /hierarchy");
        short_circuit_samples_ms.push(t_short.elapsed().as_secs_f64() * 1000.0);
        assert!(short.unchanged, "knownHash match must short-circuit");
        assert!(short.nodes.is_empty(), "unchanged response must omit nodes");
        assert_eq!(short.node_count, 0);
        assert_eq!(short.hierarchy_hash, confirm_tree.hierarchy_hash);
    }

    // 4. Verify waitForSelector waitStable=true and timeout visibleSample payload.
    let stable_wait = client
        .wait_for_selector(&WaitForSelectorRequest {
            app_id: FIXTURE_BUNDLE_ID.to_string(),
            identifier: Some("order_confirmation".to_string()),
            text: None,
            text_match: None,
            not_visible: Some(false),
            wait_stable: Some(true),
            stable_polls: Some(2),
            timeout_ms: Some(2_000),
            include_system_ui: Some(false),
            return_hierarchy: Some(false),
        })
        .await
        .expect("wait_for_selector waitStable");
    assert!(stable_wait.matched);
    assert!(stable_wait.polls >= 2);
    assert_eq!(stable_wait.hierarchy_hash, confirm_tree.hierarchy_hash);

    let miss_wait = client
        .wait_for_selector(&WaitForSelectorRequest {
            app_id: FIXTURE_BUNDLE_ID.to_string(),
            identifier: Some("nonexistent_checkout_id_77".to_string()),
            text: None,
            text_match: None,
            not_visible: Some(false),
            wait_stable: Some(false),
            stable_polls: None,
            timeout_ms: Some(250),
            include_system_ui: Some(false),
            return_hierarchy: Some(false),
        })
        .await
        .expect("wait_for_selector timeout");
    assert!(!miss_wait.matched);
    assert!(
        !miss_wait.visible_sample.is_empty() && miss_wait.visible_sample.len() <= 30,
        "expected 1..=30 visibleSample items on timeout, got {:?}",
        miss_wait.visible_sample
    );
    assert!(
        miss_wait
            .visible_sample
            .iter()
            .any(|s| s.contains("Order confirmed")),
        "expected visibleSample to include 'Order confirmed', got {:?}",
        miss_wait.visible_sample
    );

    // 5. Verify zero-fling drag-and-hold swipe (`holdDurationMs: 150`).
    let hold_swipe = client
        .swipe(&SwipeRequest {
            app_id: FIXTURE_BUNDLE_ID.to_string(),
            start_x: 200.0,
            start_y: 500.0,
            end_x: 200.0,
            end_y: 250.0,
            duration_ms: Some(300),
            hold_duration_ms: Some(150),
        })
        .await
        .expect("POST /swipe with holdDurationMs");
    assert!(hold_swipe.ok);

    // 6. Warm session reuse for Flow #2 (`DEJA_FIXTURE_ORDER=WC-2027`, 0 ms attach!).
    let verify_2_t0 = Instant::now();
    let health_2 = session.verify_healthy().await.expect("verify_healthy #2");
    let verify_2_ms = verify_2_t0.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(
        health_2.pid, health.pid,
        "must reuse the exact same runner PID"
    );

    let (optimized_flow_2_ms, optimized_flow_2_http_calls, confirm_2) =
        run_optimized_checkout_flow(&simctl, &udid, client, "WC-2027").await;
    assert!(confirm_2.contains_text("Order #WC-2027 · Mochi sticker pack"));

    session.shutdown().await.expect("bench session shutdown");
    let orphans = find_xcodebuild_test_pids_for_udid(&udid).expect("ps orphan check");
    assert!(
        orphans.is_empty(),
        "expected 0 orphaned xcodebuild PIDs after benchmark shutdown, found {orphans:?}"
    );

    let (_, full_med, full_p95, _) = summarize_latencies(&full_samples_ms);
    let (_, short_med, short_p95, _) = summarize_latencies(&short_circuit_samples_ms);

    println!("=== DEJA-77 BENCHMARK EVIDENCE ===");
    println!("warm_attach_ms: {warm_attach_ms}");
    println!("baseline_flow_ms: {baseline_flow_ms}");
    println!("baseline_http_calls: {baseline_http_calls}");
    println!("optimized_flow_1_ms: {optimized_flow_1_ms}");
    println!("optimized_flow_1_http_calls: {optimized_flow_1_http_calls}");
    println!("optimized_flow_2_ms: {optimized_flow_2_ms}");
    println!("optimized_flow_2_http_calls: {optimized_flow_2_http_calls}");
    println!("second_flow_attach_ms: 0");
    println!("verify_healthy_ms: run1={verify_1_ms:.2}ms run2={verify_2_ms:.2}ms");
    println!(
        "hierarchy_full_ms: median={full_med:.2}ms p95={full_p95:.2}ms | knownHash_short_circuit_ms: median={short_med:.2}ms p95={short_p95:.2}ms"
    );
    println!(
        "timeout_visible_sample_count: {}",
        miss_wait.visible_sample.len()
    );
    println!("orphaned_xcodebuild_pids: {}", orphans.len());
}
