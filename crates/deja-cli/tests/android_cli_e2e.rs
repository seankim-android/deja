//! Live Android Emulator / Device E2E test exercising the real `deja` CLI binary end-to-end
//! with zero Maestro dependency.
//!
//! Run explicitly against a dedicated Android emulator or device:
//! `DEJA_ANDROID_TEST_SERIAL=emulator-5554 DEJA_ANDROID_TEST_APK=/path/to/app.apk cargo test --locked --offline -p deja-cli --test android_cli_e2e -- --ignored --nocapture`

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use deja_driver_android::validate_serial;
use serde_json::{Value, json};

const TEST_STORE_ID: &str = "android-e2e";

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
            "deja-cli-android-e2e-{prefix}-{}-{nanos}",
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

fn require_test_serial() -> String {
    let raw = std::env::var("DEJA_ANDROID_TEST_SERIAL").expect(
        "DEJA_ANDROID_TEST_SERIAL must be set to a connected Android emulator or device serial",
    );
    validate_serial(&raw).expect("DEJA_ANDROID_TEST_SERIAL must be a valid Android serial")
}

fn require_test_apk() -> PathBuf {
    let raw = std::env::var("DEJA_ANDROID_TEST_APK")
        .expect("DEJA_ANDROID_TEST_APK must be set to an existing test `.apk` path");
    let path = PathBuf::from(raw);
    assert!(
        path.is_file(),
        "DEJA_ANDROID_TEST_APK does not exist as a regular file: {}",
        path.display()
    );
    path
}

fn run_deja(ws: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_deja"));
    cmd.current_dir(ws)
        .args(args)
        .env_remove("DEJA_MAESTRO")
        .env_remove("DEJA_DEVICE");
    cmd.output().expect("spawn deja binary")
}

#[test]
#[ignore = "requires live Android emulator/device (DEJA_ANDROID_TEST_SERIAL & DEJA_ANDROID_TEST_APK)"]
fn live_android_cli_end_to_end_lifecycle_without_maestro() {
    let serial = require_test_serial();
    let apk_path = require_test_apk();
    let app_id = std::env::var("DEJA_ANDROID_TEST_APP_ID")
        .unwrap_or_else(|_| "dev.deja.android.fixture".to_string());

    let ws = TempDirGuard::new("ws");
    let flow_path = ws.path().join("flows/smoke.yaml");
    fs::create_dir_all(flow_path.parent().unwrap()).unwrap();
    let flow_yaml = format!(
        "appId: {app_id}\n---\n- launchApp:\n    clearState: true\n- assertVisible:\n    text: \".*\"\n"
    );
    fs::write(&flow_path, flow_yaml).unwrap();

    let config = json!({
        "workspaceRoot": ".",
        "platform": "android",
        "appId": app_id,
        "adapter": "maestro-android/v1",
        "buildPath": apk_path.to_string_lossy(),
        "device": serial,
        "manifest": "deja.walks.json",
        "flowsDir": "flows",
        "timeoutMs": 120000,
        "verificationTimeoutMs": 120000,
        "crashReportGraceMs": 1000,
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
            "launch android smoke",
            "--flow",
            "flows/smoke.yaml",
            "--store",
            TEST_STORE_ID,
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
        .expect("remember must record resolved Android runtime")
        .to_string();
    assert!(
        runtime.starts_with("android-"),
        "expected android-<sdk> runtime, got {runtime}"
    );

    // 3. `deja run`
    let run_out = run_deja(
        ws.path(),
        &[
            "run",
            "launch android smoke",
            "--store",
            TEST_STORE_ID,
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

    // 4. `deja gate`
    let ev_path = ws.path().join(".deja/gate/android-e2e.json");
    let gate_out = run_deja(
        ws.path(),
        &[
            "gate",
            "--goals",
            "deja.walks.json",
            "--store",
            TEST_STORE_ID,
            "--runtime",
            &runtime,
            "--label",
            "deja-68-e2e",
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
}
