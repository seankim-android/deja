//! Live iOS Simulator E2E test exercising the real `deja` CLI binary end-to-end with
//! minimal `PATH=/usr/bin:/bin:/usr/sbin:/sbin` (zero Maestro, Bun, or Java).
//!
//! Run explicitly with a dedicated temporary simulator UDID:
//! `DEJA_IOS_TEST_UDID=<UDID> cargo test --locked --offline -p deja-cli --test ios_cli_e2e -- --ignored --nocapture`

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use deja_driver_ios::validate_udid;
use serde_json::{Value, json};

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
            "deja-cli-ios-e2e-{prefix}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self {
            path: fs::canonicalize(&path).expect("canonical temp dir"),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root should exist")
}

fn require_test_udid() -> String {
    let raw = std::env::var("DEJA_IOS_TEST_UDID")
        .expect("DEJA_IOS_TEST_UDID must be set to a dedicated temporary iOS simulator UDID");
    validate_udid(&raw).expect("DEJA_IOS_TEST_UDID must be a valid simulator UDID")
}

fn build_fixture_app(derived_data: &Path) -> PathBuf {
    if let Ok(existing) = std::env::var("DEJA_IOS_FIXTURE_APP")
        && !existing.is_empty()
    {
        let p = PathBuf::from(&existing);
        if p.is_dir() {
            return p;
        }
    }

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
        .env("PATH", SANITIZED_SYSTEM_PATH)
        .stdin(Stdio::null());
    let output = cmd.output().expect("spawn xcodebuild for DejaFixture");
    assert!(
        output.status.success(),
        "DejaFixture build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let app = derived_data.join("Build/Products/Debug-iphonesimulator/DejaFixture.app");
    assert!(
        app.is_dir(),
        "expected DejaFixture.app at {}",
        app.display()
    );
    app
}

fn run_deja(ws: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_deja"));
    cmd.current_dir(ws)
        .args(args)
        .env("PATH", SANITIZED_SYSTEM_PATH)
        .env_remove("DEJA_MAESTRO")
        .env_remove("DEJA_DEVICE");
    cmd.output().expect("spawn deja binary")
}

#[test]
#[ignore = "requires live macOS Xcode + dedicated temporary iOS Simulator (DEJA_IOS_TEST_UDID)"]
fn live_ios_cli_end_to_end_lifecycle_without_maestro_or_java() {
    let udid = require_test_udid();

    // Confirm `maestro` is not on the minimal PATH used for every CLI invocation.
    let which = Command::new("/usr/bin/which")
        .arg("maestro")
        .env("PATH", SANITIZED_SYSTEM_PATH)
        .output()
        .expect("which maestro");
    assert!(
        !which.status.success(),
        "maestro must not exist on sanitized PATH"
    );

    let derived = TempDirGuard::new("derived");
    let app_path = build_fixture_app(derived.path());

    let ws = TempDirGuard::new("ws");
    let flow_src = repo_root().join("spikes/003-ios-deja/flows/checkout.yaml");
    let flow_dst = ws.path().join("flows/checkout.yaml");
    fs::create_dir_all(flow_dst.parent().unwrap()).unwrap();
    fs::copy(&flow_src, &flow_dst).expect("copy checkout.yaml");

    let config = json!({
        "workspaceRoot": ".",
        "platform": "ios",
        "appId": "dev.deja.ios.checkout",
        "adapter": "maestro-ios/v1",
        "buildPath": app_path.to_string_lossy(),
        "device": udid,
        "manifest": "deja.walks.json",
        "flowsDir": "flows",
        "timeoutMs": 120000,
        "verificationTimeoutMs": 120000,
        "crashReportGraceMs": 0,
        "disableAnimations": true
    });
    fs::write(
        ws.path().join("deja.config.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();

    // 1. `deja doctor --json`
    let doc = run_deja(ws.path(), &["doctor", "--json"]);
    assert_eq!(
        doc.status.code(),
        Some(0),
        "doctor failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&doc.stdout),
        String::from_utf8_lossy(&doc.stderr)
    );
    let doc_json: Value = serde_json::from_slice(&doc.stdout).expect("doctor JSON");
    assert_eq!(doc_json["ready"], true);

    // 2. `deja remember`
    let rem = run_deja(
        ws.path(),
        &[
            "remember",
            "complete checkout",
            "--flow",
            "flows/checkout.yaml",
            "--store",
            &udid,
            "--summary",
            "--json",
        ],
    );
    assert_eq!(
        rem.status.code(),
        Some(0),
        "remember failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&rem.stdout),
        String::from_utf8_lossy(&rem.stderr)
    );
    let rem_json: Value = serde_json::from_slice(&rem.stdout).expect("remember JSON");
    assert_eq!(rem_json["outcome"], "remembered");
    assert_eq!(rem_json["modelCalls"], 0);
    let runtime = rem_json["runtime"]
        .as_str()
        .expect("remember must record resolved iOS runtime")
        .to_string();
    eprintln!(
        "LIVE E2E remember: runtime={runtime}, timing={}",
        rem_json["timing"]
    );

    // 3. `deja run --store <udid> --summary --json`
    let run_out = run_deja(
        ws.path(),
        &[
            "run",
            "complete checkout",
            "--store",
            &udid,
            "--summary",
            "--json",
        ],
    );
    assert_eq!(
        run_out.status.code(),
        Some(0),
        "run failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&run_out.stdout),
        String::from_utf8_lossy(&run_out.stderr)
    );
    let run_json: Value = serde_json::from_slice(&run_out.stdout).expect("run JSON");
    assert_eq!(run_json["outcome"], "passed");
    assert_eq!(run_json["cacheStatus"], "hit");
    assert_eq!(run_json["modelCalls"], 0);
    let run_stderr = String::from_utf8_lossy(&run_out.stderr);
    assert!(
        run_stderr.contains("deja-summary v=1 outcome=passed cache=hit"),
        "expected deja-summary v=1 on stderr, got: {run_stderr}"
    );
    eprintln!(
        "LIVE E2E run: timing={}, summary={}",
        run_json["timing"],
        run_stderr.trim()
    );

    // 4. `deja list`
    let list_out = run_deja(ws.path(), &["list", "--store", &udid, "--json"]);
    assert_eq!(list_out.status.code(), Some(0));
    let list_json: Value = serde_json::from_slice(&list_out.stdout).expect("list JSON");
    assert_eq!(list_json["walks"].as_array().unwrap().len(), 1);

    // 5. `deja inspect`
    let insp_out = run_deja(
        ws.path(),
        &["inspect", "complete checkout", "--store", &udid, "--json"],
    );
    assert_eq!(insp_out.status.code(), Some(0));
    let insp_json: Value = serde_json::from_slice(&insp_out.stdout).expect("inspect JSON");
    assert_eq!(insp_json["walk"]["goalKey"], "complete checkout");
    assert!(!insp_json["validationRuns"].as_array().unwrap().is_empty());

    // 6. `deja manifest check --untracked`
    let mcheck = run_deja(ws.path(), &["manifest", "check", "--untracked", "--json"]);
    assert_eq!(mcheck.status.code(), Some(0));
    let mcheck_json: Value = serde_json::from_slice(&mcheck.stdout).expect("manifest check JSON");
    assert_eq!(mcheck_json["ok"], true);

    // 7. `deja gate --goals deja.walks.json --store <udid> --runtime <runtime> --evidence ...`
    let ev_path = ws.path().join(".deja/gate/e2e-evidence.json");
    let gate_out = run_deja(
        ws.path(),
        &[
            "gate",
            "--goals",
            "deja.walks.json",
            "--store",
            &udid,
            "--runtime",
            &runtime,
            "--label",
            "deja-65-e2e",
            "--evidence",
            ev_path.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(
        gate_out.status.code(),
        Some(0),
        "gate failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&gate_out.stdout),
        String::from_utf8_lossy(&gate_out.stderr)
    );
    let gate_json: Value = serde_json::from_slice(&gate_out.stdout).expect("gate JSON");
    assert_eq!(gate_json["verdict"], "passed");
    assert!(ev_path.is_file());
    assert!(ws.path().join(".deja/gate/e2e-evidence.txt").is_file());
}
