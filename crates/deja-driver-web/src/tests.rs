//! Comprehensive deterministic tests for `deja-driver-web` covering all 6 Phase 5 areas:
//! 1. Chromium binary discovery & loopback URL / `ScreenSize` safety
//! 2. Multiplexed CDP WebSocket client & RAII cancellation cleanup
//! 3. Warm browser reuse, `BrowserContext` isolation, and `prefers-reduced-motion` default/opt-out
//! 4. AX-DOM hierarchy conversion & `known_hash` short-circuiting
//! 5. Pointer, keyboard, wheel swipe, base64 PNG, `act_and_observe`, and fail-closed checks
//! 6. Target crash signals & zero-`unsafe` process-group teardown

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deja_core::{CrashSignalSource, LaunchArgumentValue};
use deja_flow::{
    ActAction, ActPayload, DriverCapability, DriverError, KeyPress, LaunchOptions,
    ObservationSource, OpenLinkOptions, PlatformDriver, SwipeParams, UiPoint,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::browser::{
    BrowserLaunchOptions, ManagedBrowser, TempProfileDir, discover_chromium_with_fallbacks,
    extract_devtools_ws_url_from_line, parse_chromium_version_output, probe_chromium_version,
    safe_pgid,
};
use crate::cdp::{CdpClient, RecordedCrash, now_epoch_ms};
use crate::dom::{decode_base64, decode_base64_png, dom_snapshot_to_hierarchy};
use crate::driver::{WebDriver, WebDriverConfig};
use crate::error::WebDriverError;
use crate::input::{
    MODIFIER_ALT, MODIFIER_CTRL, MODIFIER_META, MODIFIER_SHIFT, ResolvedKeyAction, ScreenSize,
    resolve_key_press, validate_loopback_http_url, validate_loopback_ws_url,
};
use crate::{CRATE_NAME, UPSTREAM_CRATES};

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "deja-web-test-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn write_executable(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn write_non_executable(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }
}

#[test]
fn crate_identity_matches() {
    assert_eq!(CRATE_NAME, "deja-driver-web");
    assert_eq!(UPSTREAM_CRATES, &["deja-core", "deja-flow"]);
}

// ===========================================================================
// Area 1: Discovery, version probing, loopback URLs, ScreenSize
// ===========================================================================

#[test]
fn discovery_precedence_and_fail_closed_overrides() {
    let dir = TempDir::new("discover");
    let deja_bin = dir.path().join("deja-chrome");
    let chrome_bin = dir.path().join("chrome-path");
    let path_dir = dir.path().join("bin");
    let path_bin = path_dir.join("google-chrome");
    let fallback_bin = dir.path().join("fallback/chromium");
    let non_exec = dir.path().join("not-exec");

    write_executable(&deja_bin, "#!/bin/sh\necho 'Google Chrome 131.0.0.1'\n");
    write_executable(&chrome_bin, "#!/bin/sh\necho 'Chromium 130.0.0.2'\n");
    write_executable(&path_bin, "#!/bin/sh\necho 'Google Chrome 129.0.0.3'\n");
    write_executable(&fallback_bin, "#!/bin/sh\necho 'Chromium 128.0.0.4'\n");
    write_non_executable(&non_exec, "not executable");

    let fb_str = fallback_bin.to_string_lossy().into_owned();
    let fallbacks = [fb_str.as_str()];

    // 1. DEJA_CHROME wins over CHROME_PATH, PATH, and fallback.
    let mut env = BTreeMap::new();
    env.insert(
        "DEJA_CHROME".into(),
        deja_bin.to_string_lossy().into_owned(),
    );
    env.insert(
        "CHROME_PATH".into(),
        chrome_bin.to_string_lossy().into_owned(),
    );
    env.insert("PATH".into(), path_dir.to_string_lossy().into_owned());
    assert_eq!(
        discover_chromium_with_fallbacks(&env, &fallbacks).unwrap(),
        deja_bin
    );

    // 2. CHROME_PATH wins when DEJA_CHROME unset.
    env.remove("DEJA_CHROME");
    assert_eq!(
        discover_chromium_with_fallbacks(&env, &fallbacks).unwrap(),
        chrome_bin
    );

    // 3. PATH wins when overrides unset.
    env.remove("CHROME_PATH");
    assert_eq!(
        discover_chromium_with_fallbacks(&env, &fallbacks).unwrap(),
        path_bin
    );

    // 4. OS fallback used when PATH has no chromium.
    env.insert(
        "PATH".into(),
        dir.path().join("empty").to_string_lossy().into_owned(),
    );
    assert_eq!(
        discover_chromium_with_fallbacks(&env, &fallbacks).unwrap(),
        fallback_bin
    );

    // 5. Missing explicit DEJA_CHROME fails closed (never falls back to PATH or OS!).
    let mut bad_env = BTreeMap::new();
    bad_env.insert(
        "DEJA_CHROME".into(),
        dir.path()
            .join("does-not-exist")
            .to_string_lossy()
            .into_owned(),
    );
    bad_env.insert("PATH".into(), path_dir.to_string_lossy().into_owned());
    let err = discover_chromium_with_fallbacks(&bad_env, &fallbacks)
        .expect_err("missing DEJA_CHROME must fail closed");
    assert!(matches!(
        err,
        WebDriverError::InvalidChromiumPath { ref source_var, .. } if source_var == "DEJA_CHROME"
    ));

    // 6. Non-executable CHROME_PATH fails closed on Unix.
    #[cfg(unix)]
    {
        let mut non_exec_env = BTreeMap::new();
        non_exec_env.insert(
            "CHROME_PATH".into(),
            non_exec.to_string_lossy().into_owned(),
        );
        non_exec_env.insert("PATH".into(), path_dir.to_string_lossy().into_owned());
        let err = discover_chromium_with_fallbacks(&non_exec_env, &fallbacks)
            .expect_err("non-executable CHROME_PATH must fail closed");
        assert!(matches!(
            err,
            WebDriverError::InvalidChromiumPath { ref source_var, .. } if source_var == "CHROME_PATH"
        ));
    }
}

#[tokio::test]
async fn probe_chromium_version_parses_output_and_honors_cancel() {
    assert_eq!(
        parse_chromium_version_output("Google Chrome 158.0.8127.38 \n").as_deref(),
        Some("158.0.8127.38")
    );
    assert_eq!(
        parse_chromium_version_output("Chromium 131.0.6778.85 snap").as_deref(),
        Some("131.0.6778.85")
    );

    let dir = TempDir::new("version");
    let ok_bin = dir.path().join("fake-chrome");
    write_executable(&ok_bin, "#!/bin/sh\necho 'Google Chrome 142.0.1234.56'\n");
    let cancel = CancellationToken::new();
    let version = probe_chromium_version(&ok_bin, &cancel).await.unwrap();
    assert_eq!(version, "142.0.1234.56");

    let fail_bin = dir.path().join("fail-chrome");
    write_executable(&fail_bin, "#!/bin/sh\necho 'boom' >&2\nexit 1\n");
    assert!(probe_chromium_version(&fail_bin, &cancel).await.is_err());

    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        probe_chromium_version(&ok_bin, &cancelled).await,
        Err(WebDriverError::Cancelled)
    ));
}

#[test]
fn loopback_http_and_ws_url_validators_fail_closed() {
    for valid in [
        "http://127.0.0.1:4174",
        "http://127.0.0.1:4174/checkout?a=1#done",
        "https://localhost:8443/app",
        "http://localhost",
        "http://[::1]:3000/test",
        "https://[::1]/",
    ] {
        assert!(
            validate_loopback_http_url(valid).is_ok(),
            "expected valid loopback HTTP URL: {valid}"
        );
    }

    for invalid in [
        "",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "data:text/html,<h1>hi</h1>",
        "chrome://settings",
        "ws://127.0.0.1:9222/devtools/browser/abc",
        "http://example.com",
        "http://127.0.0.1.evil.com:4174",
        "http://127.0.0.1@evil.com:4174",
        "http://localhost@evil.com/path",
        "http://127.0.0.1\\@evil.com",
        "http://127.0.0.1:0/app",
        "http://127.0.0.1:99999/app",
        "http://[::2]:3000",
        "http://127.0.0.1:4174/path with space",
        "http://127.0.0.1:4174/\n",
    ] {
        assert!(
            validate_loopback_http_url(invalid).is_err(),
            "expected invalid loopback HTTP URL to fail closed: {invalid:?}"
        );
    }

    assert_eq!(
        extract_devtools_ws_url_from_line(
            "\nDevTools listening on ws://127.0.0.1:9222/devtools/browser/1234-abcd\r\n"
        )
        .unwrap()
        .unwrap(),
        "ws://127.0.0.1:9222/devtools/browser/1234-abcd"
    );
    assert!(validate_loopback_ws_url("ws://[::1]:9222/devtools/browser/xyz").is_ok());
    assert!(validate_loopback_ws_url("ws://localhost:9222/devtools/browser/xyz").is_ok());

    for bad_ws in [
        "wss://127.0.0.1:9222/devtools/browser/xyz",
        "ws://127.0.0.1/devtools/browser/xyz",
        "ws://127.0.0.1:0/devtools/browser/xyz",
        "ws://127.0.0.1@evil.com:9222/devtools/browser/xyz",
        "ws://evil.com:9222/devtools/browser/xyz",
        "ws://127.0.0.1:9222/other/path",
        "ws://127.0.0.1:9222/devtools/browser/",
    ] {
        assert!(
            validate_loopback_ws_url(bad_ws).is_err(),
            "expected bad WS URL to fail closed: {bad_ws:?}"
        );
    }
}

#[test]
fn screen_size_parsing_and_bounds() {
    assert_eq!(
        ScreenSize::default(),
        ScreenSize {
            width: 1280,
            height: 720
        }
    );
    assert_eq!(
        ScreenSize::parse(&json!("1280x800")).unwrap(),
        ScreenSize {
            width: 1280,
            height: 800
        }
    );
    assert_eq!(
        ScreenSize::parse(&json!("1920X1080")).unwrap(),
        ScreenSize {
            width: 1920,
            height: 1080
        }
    );
    assert_eq!(
        ScreenSize::parse(&json!({ "width": 1024, "height": 768 })).unwrap(),
        ScreenSize {
            width: 1024,
            height: 768
        }
    );

    for bad in [
        json!("99x800"),
        json!("1280x99"),
        json!("8000x800"),
        json!("1280x5000"),
        json!("1280x800x2"),
        json!("abc"),
        json!({ "width": 1280.5, "height": 800 }),
        json!({ "width": -10, "height": 800 }),
        json!({ "width": 1280, "height": 800, "extra": true }),
        json!(1280),
    ] {
        assert!(
            ScreenSize::parse(&bad).is_err(),
            "expected invalid screenSize to fail: {bad}"
        );
    }
}

// ===========================================================================
// Mock CDP Server Harness
// ===========================================================================

#[derive(Debug, Clone)]
struct RecordedCdpCall {
    id: u64,
    session_id: Option<String>,
    method: String,
    params: Value,
}

type MockCdpHandler =
    Arc<dyn Fn(RecordedCdpCall, mpsc::UnboundedSender<Value>) -> Option<Value> + Send + Sync>;

struct MockCdpServer {
    ws_url: String,
    calls: Arc<StdMutex<Vec<RecordedCdpCall>>>,
    event_tx: Arc<StdMutex<Option<mpsc::UnboundedSender<Value>>>>,
    task: tokio::task::JoinHandle<()>,
}

impl MockCdpServer {
    async fn start(handler: MockCdpHandler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let ws_url = format!("ws://127.0.0.1:{}/devtools/browser/mock", addr.port());
        let calls: Arc<StdMutex<Vec<RecordedCdpCall>>> = Arc::new(StdMutex::new(Vec::new()));
        let event_tx: Arc<StdMutex<Option<mpsc::UnboundedSender<Value>>>> =
            Arc::new(StdMutex::new(None));

        let calls_clone = Arc::clone(&calls);
        let event_slot = Arc::clone(&event_tx);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let Ok(ws) = accept_async(stream).await else {
                    continue;
                };
                let (mut sink, mut source) = ws.split();
                let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
                {
                    let mut slot = event_slot.lock().unwrap();
                    *slot = Some(out_tx.clone());
                }
                let writer = tokio::spawn(async move {
                    while let Some(msg) = out_rx.recv().await {
                        let txt = serde_json::to_string(&msg).unwrap();
                        if sink.send(Message::Text(txt.into())).await.is_err() {
                            break;
                        }
                    }
                });

                let handler_c = Arc::clone(&handler);
                let calls_c = Arc::clone(&calls_clone);
                while let Some(Ok(frame)) = source.next().await {
                    let Message::Text(txt) = frame else {
                        continue;
                    };
                    let Ok(val) = serde_json::from_str::<Value>(txt.as_str()) else {
                        continue;
                    };
                    let call = RecordedCdpCall {
                        id: val.get("id").and_then(Value::as_u64).unwrap_or(0),
                        session_id: val
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .map(ToString::to_string),
                        method: val
                            .get("method")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        params: val.get("params").cloned().unwrap_or(Value::Null),
                    };
                    calls_c.lock().unwrap().push(call.clone());
                    if let Some(resp) = handler_c(call, out_tx.clone()) {
                        let _ = out_tx.send(resp);
                    }
                }
                writer.abort();
            }
        });

        Self {
            ws_url,
            calls,
            event_tx,
            task,
        }
    }

    fn calls(&self) -> Vec<RecordedCdpCall> {
        self.calls.lock().unwrap().clone()
    }

    fn push_event(&self, event: Value) {
        if let Some(tx) = self.event_tx.lock().unwrap().as_ref() {
            let _ = tx.send(event);
        }
    }
}

impl Drop for MockCdpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Standard mock handler implementing target lifecycle, emulation, navigation, input, DOM,
/// screenshot, and navigation history.
fn standard_mock_handler(
    dom_snapshot: Arc<StdMutex<Value>>,
    fail_dom_eval: Arc<StdMutex<Option<String>>>,
) -> MockCdpHandler {
    let ctx_seq = Arc::new(AtomicUsize::new(0));
    let target_seq = Arc::new(AtomicUsize::new(0));
    let session_seq = Arc::new(AtomicUsize::new(0));

    Arc::new(
        move |call: RecordedCdpCall, out_tx: mpsc::UnboundedSender<Value>| {
            let id = call.id;
            let result = match call.method.as_str() {
                "Target.setDiscoverTargets"
                | "Target.disposeBrowserContext"
                | "Target.closeTarget"
                | "Page.enable"
                | "Runtime.enable"
                | "Network.enable"
                | "Emulation.setDeviceMetricsOverride"
                | "Emulation.setEmulatedMedia"
                | "Input.dispatchMouseEvent"
                | "Input.dispatchKeyEvent"
                | "Input.insertText"
                | "Browser.close" => json!({}),
                "Target.createBrowserContext" => {
                    let n = ctx_seq.fetch_add(1, Ordering::SeqCst) + 1;
                    json!({ "browserContextId": format!("ctx-{n}") })
                }
                "Target.createTarget" => {
                    let n = target_seq.fetch_add(1, Ordering::SeqCst) + 1;
                    json!({ "targetId": format!("target-{n}") })
                }
                "Target.attachToTarget" => {
                    let n = session_seq.fetch_add(1, Ordering::SeqCst) + 1;
                    json!({ "sessionId": format!("sess-{n}") })
                }
                "Page.navigate" => {
                    let url = call.params.get("url").and_then(Value::as_str).unwrap_or("");
                    if url.contains("fail-nav") {
                        json!({ "frameId": "f1", "errorText": "net::ERR_CONNECTION_REFUSED" })
                    } else {
                        if let Some(sid) = call.session_id.clone() {
                            let _ = out_tx.send(json!({
                                "method": "Page.loadEventFired",
                                "sessionId": sid,
                                "params": { "timestamp": 1.0 }
                            }));
                        }
                        json!({ "frameId": "f1", "loaderId": "loader-1" })
                    }
                }
                "Page.getNavigationHistory" => json!({
                    "currentIndex": 2,
                    "entries": [
                        { "id": 10, "url": "about:blank" },
                        { "id": 11, "url": "http://127.0.0.1:4174/page1" },
                        { "id": 12, "url": "http://127.0.0.1:4174/page2" }
                    ]
                }),
                "Page.navigateToHistoryEntry" => {
                    if let Some(sid) = call.session_id.clone() {
                        let _ = out_tx.send(json!({
                            "method": "Page.frameNavigated",
                            "sessionId": sid,
                            "params": { "frame": { "id": "f1", "url": "http://127.0.0.1:4174/page1" } }
                        }));
                    }
                    json!({})
                }
                "Page.captureScreenshot" => {
                    // Valid 8-byte PNG magic header + IHDR bytes encoded in RFC 4648 base64:
                    // \x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR
                    json!({ "data": "iVBORw0KGgoAAAAN" })
                }
                "Runtime.evaluate" => {
                    let expr = call
                        .params
                        .get("expression")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if expr == "document.readyState" {
                        json!({ "result": { "type": "string", "value": "complete" } })
                    } else if expr.contains("requestAnimationFrame") {
                        json!({ "result": { "type": "boolean", "value": true } })
                    } else if let Some(err_msg) = fail_dom_eval.lock().unwrap().clone() {
                        return Some(json!({
                            "id": id,
                            "error": { "code": -32000, "message": err_msg }
                        }));
                    } else {
                        let snap = dom_snapshot.lock().unwrap().clone();
                        json!({ "result": { "type": "object", "value": snap } })
                    }
                }
                other => {
                    return Some(json!({
                        "id": id,
                        "error": { "code": -32601, "message": format!("unhandled method {other}") }
                    }));
                }
            };
            Some(json!({ "id": id, "result": result }))
        },
    )
}

fn sample_dom_snapshot(truncated: bool) -> Value {
    json!({
        "url": "http://127.0.0.1:4174/",
        "viewport": { "width": 1280, "height": 720 },
        "truncated": truncated,
        "nodes": [
            {
                "index": 0,
                "parentIndex": null,
                "depth": 0,
                "elementType": "Region",
                "identifier": "screen_login",
                "label": "Déjà Web Shop",
                "title": "",
                "value": null,
                "placeholderValue": null,
                "enabled": true,
                "checked": false,
                "selected": false,
                "hasFocus": false,
                "hittable": true,
                "isOnScreen": true,
                "frame": { "x": 100, "y": 50, "width": 500, "height": 400 }
            },
            {
                "index": 1,
                "parentIndex": 0,
                "depth": 1,
                "elementType": "Button",
                "identifier": "login_button",
                "label": "Sign in",
                "title": "Submit form",
                "value": null,
                "placeholderValue": null,
                "enabled": true,
                "checked": false,
                "selected": false,
                "hasFocus": false,
                "hittable": true,
                "isOnScreen": true,
                "frame": { "x": 120, "y": 300, "width": 140, "height": 44 }
            },
            {
                "index": 2,
                "parentIndex": 0,
                "depth": 1,
                "elementType": "StaticText",
                "identifier": "below_fold",
                "label": "Footer item",
                "title": "",
                "value": null,
                "placeholderValue": null,
                "enabled": true,
                "checked": false,
                "selected": false,
                "hasFocus": false,
                "hittable": false,
                "isOnScreen": false,
                "frame": { "x": 120, "y": 1500, "width": 200, "height": 30 }
            }
        ]
    })
}

// ===========================================================================
// Area 2: CDP multiplexing & RAII cancellation cleanup
// ===========================================================================

#[tokio::test]
async fn cdp_multiplexing_out_of_order_and_cancellation_cleanup() {
    let server = MockCdpServer::start(Arc::new(|call, out_tx| {
        if call.method == "Slow.hang" {
            // Never responds -> exercises cancellation and PendingCallGuard drop cleanup.
            return None;
        }
        if call.method == "Test.delayed" {
            let id = call.id;
            let val = call.params.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(60)).await;
                let _ = out_tx.send(json!({ "id": id, "result": { "echo": val } }));
            });
            return None;
        }
        Some(json!({ "id": call.id, "result": { "echo": call.params, "sid": call.session_id } }))
    }))
    .await;

    let crashes = Arc::new(StdMutex::new(None));
    let cancel = CancellationToken::new();
    let client = CdpClient::connect(&server.ws_url, Duration::from_secs(5), crashes, &cancel)
        .await
        .unwrap();

    // Concurrent out-of-order browser + session-scoped calls.
    let (slow_res, fast_res) = tokio::join!(
        client.call_browser(
            "Test.delayed",
            json!({ "order": 1 }),
            Duration::from_secs(5),
            &cancel
        ),
        client.call_session(
            "sess-42",
            "Test.fast",
            json!({ "order": 2 }),
            Duration::from_secs(5),
            &cancel
        )
    );
    assert_eq!(slow_res.unwrap()["echo"]["order"], 1);
    let fast_val = fast_res.unwrap();
    assert_eq!(fast_val["echo"]["order"], 2);
    assert_eq!(fast_val["sid"], "sess-42");

    // Cancel an in-flight hung RPC -> PendingCallGuard removes entry so pending_len == 0.
    let call_cancel = CancellationToken::new();
    let cancel_trigger = call_cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel_trigger.cancel();
    });
    let err = client
        .call_browser("Slow.hang", json!({}), Duration::from_secs(5), &call_cancel)
        .await
        .expect_err("hung call must cancel");
    assert!(matches!(err, WebDriverError::Cancelled));
    assert_eq!(client.pending_len(), 0);

    // Dropped future (via outer timeout) also cleans up `pending` and never wedges subsequent calls.
    let dropped = tokio::time::timeout(
        Duration::from_millis(30),
        client.call_browser("Slow.hang", json!({}), Duration::from_secs(5), &cancel),
    )
    .await;
    assert!(dropped.is_err());
    assert_eq!(client.pending_len(), 0);

    // Internal response timeout (Codex R1 Finding 4) MUST remove the timed-out entry from
    // `pending` immediately so warm connections never leak map entries.
    let timed_out = client
        .call_browser("Slow.hang", json!({}), Duration::from_millis(40), &cancel)
        .await
        .expect_err("hung RPC must hit internal response timeout");
    assert!(matches!(timed_out, WebDriverError::Timeout { .. }));
    assert_eq!(
        client.pending_len(),
        0,
        "timed-out CDP request must be removed from pending map"
    );

    // Follow-up RPC succeeds cleanly.
    let ok = client
        .call_browser(
            "Test.afterCancel",
            json!({ "ok": true }),
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(ok["echo"]["ok"], true);
    client.close().await;
}

// ===========================================================================
// Area 3: Warm reuse, BrowserContext isolation, prefers-reduced-motion
// ===========================================================================

#[tokio::test]
async fn warm_reuse_context_isolation_and_reduced_motion_toggle() {
    let snap = Arc::new(StdMutex::new(sample_dom_snapshot(false)));
    let fail = Arc::new(StdMutex::new(None));
    let server = MockCdpServer::start(standard_mock_handler(snap, fail)).await;
    let cancel = CancellationToken::new();

    // Default `disable_animations: true`: advertises capability + sets prefers-reduced-motion.
    let driver = WebDriver::with_cdp_url_for_tests(
        WebDriverConfig::new().with_screen_size(ScreenSize::new(1280, 800).unwrap()),
        server.ws_url.clone(),
    );
    assert!(
        driver
            .capabilities()
            .supports(DriverCapability::DisableAnimations)
    );
    driver.ensure_session(&cancel).await.unwrap();
    driver.ensure_session(&cancel).await.unwrap();
    assert_eq!(driver.attach_count(), 1, "warm browser must be reused");

    let initial_calls = server.calls();
    assert!(initial_calls.iter().any(|c| {
        c.method == "Emulation.setEmulatedMedia"
            && c.params["features"][0]["name"] == "prefers-reduced-motion"
            && c.params["features"][0]["value"] == "reduce"
    }));
    assert!(initial_calls.iter().any(|c| {
        c.method == "Emulation.setDeviceMetricsOverride"
            && c.params["width"] == 1280
            && c.params["height"] == 800
    }));

    // `stop_app` closes target and opens a fresh target in the SAME BrowserContext (`ctx-1`).
    driver
        .stop_app("http://127.0.0.1:4174", &cancel)
        .await
        .unwrap();
    let after_stop = server.calls();
    assert!(
        after_stop
            .iter()
            .any(|c| c.method == "Target.closeTarget" && c.params["targetId"] == "target-1")
    );
    let second_create = after_stop
        .iter()
        .filter(|c| c.method == "Target.createTarget")
        .nth(1)
        .expect("second Target.createTarget after stop_app");
    assert_eq!(second_create.params["browserContextId"], "ctx-1");
    assert_eq!(driver.attach_count(), 1);

    // `clear_state` disposes `ctx-1` and creates a brand new BrowserContext (`ctx-2`).
    driver
        .clear_state("http://127.0.0.1:4174", &cancel)
        .await
        .unwrap();
    let after_clear = server.calls();
    assert!(after_clear.iter().any(|c| {
        c.method == "Target.disposeBrowserContext" && c.params["browserContextId"] == "ctx-1"
    }));
    let third_create = after_clear
        .iter()
        .filter(|c| c.method == "Target.createTarget")
        .nth(2)
        .expect("third Target.createTarget after clear_state");
    assert_eq!(third_create.params["browserContextId"], "ctx-2");
    assert_eq!(driver.attach_count(), 1);
    driver.shutdown().await.unwrap();

    // Opt-out `disable_animations: false`: no DisableAnimations cap, no Emulation.setEmulatedMedia.
    let server2 = MockCdpServer::start(standard_mock_handler(
        Arc::new(StdMutex::new(sample_dom_snapshot(false))),
        Arc::new(StdMutex::new(None)),
    ))
    .await;
    let driver2 = WebDriver::with_cdp_url_for_tests(
        WebDriverConfig::new().with_disable_animations(false),
        server2.ws_url.clone(),
    );
    assert!(
        !driver2
            .capabilities()
            .supports(DriverCapability::DisableAnimations)
    );
    driver2.ensure_session(&cancel).await.unwrap();
    assert!(
        !server2
            .calls()
            .iter()
            .any(|c| c.method == "Emulation.setEmulatedMedia"),
        "disable_animations=false must not call Emulation.setEmulatedMedia"
    );
    driver2.shutdown().await.unwrap();
}

// ===========================================================================
// Area 4: AX-DOM conversion & known_hash short-circuit
// ===========================================================================

#[test]
fn ax_dom_conversion_and_known_hash_short_circuit() {
    let full = dom_snapshot_to_hierarchy(sample_dom_snapshot(false), "http://127.0.0.1:4174", None)
        .unwrap();
    assert!(!full.unchanged);
    assert!(!full.truncated);
    assert_eq!(full.nodes.len(), 3);
    assert_eq!(full.nodes[0].source, "web");
    assert_eq!(full.nodes[0].observation_source, ObservationSource::Tree);
    assert_eq!(full.nodes[1].identifier, "login_button");
    assert_eq!(full.nodes[1].label, "Sign in");
    assert!(!full.nodes[2].is_on_screen && !full.nodes[2].hittable);
    assert_eq!(full.hierarchy_hash, full.hierarchy_hash_hex());

    // Matching `known_hash` on non-truncated snapshot short-circuits cleanly.
    let cached = dom_snapshot_to_hierarchy(
        sample_dom_snapshot(false),
        "http://127.0.0.1:4174",
        Some(&full.hierarchy_hash),
    )
    .unwrap();
    assert!(cached.unchanged);
    assert!(cached.nodes.is_empty());
    assert_eq!(cached.hierarchy_hash, full.hierarchy_hash);

    // Truncated snapshot NEVER short-circuits even when hashes match.
    let trunc_base =
        dom_snapshot_to_hierarchy(sample_dom_snapshot(true), "http://127.0.0.1:4174", None)
            .unwrap();
    let trunc_again = dom_snapshot_to_hierarchy(
        sample_dom_snapshot(true),
        "http://127.0.0.1:4174",
        Some(&trunc_base.hierarchy_hash),
    )
    .unwrap();
    assert!(trunc_again.truncated);
    assert!(!trunc_again.unchanged);
    assert_eq!(trunc_again.nodes.len(), 3);
}

// ===========================================================================
// Area 5: Actions, keys, wheel swipe, base64 PNG, act_and_observe, fail-closed
// ===========================================================================

#[test]
fn key_mapping_and_base64_png_validation() {
    let enter = resolve_key_press(&KeyPress::single("Return")).unwrap();
    match enter {
        ResolvedKeyAction::Dispatch { spec, count } => {
            assert_eq!(count, 1);
            assert_eq!(spec.key, "Enter");
            assert_eq!(spec.windows_virtual_key_code, 13);
            assert_eq!(spec.text.as_deref(), Some("\r"));
        }
        _ => panic!("expected Dispatch"),
    }

    let combo = resolve_key_press(&KeyPress {
        key: "a".into(),
        modifiers: vec!["Meta".into(), "Shift".into(), "Ctrl".into(), "Alt".into()],
        count: 2,
    })
    .unwrap();
    match combo {
        ResolvedKeyAction::Dispatch { spec, count } => {
            assert_eq!(count, 2);
            assert_eq!(spec.key, "A");
            assert_eq!(spec.code, "KeyA");
            assert_eq!(
                spec.modifiers,
                MODIFIER_META | MODIFIER_SHIFT | MODIFIER_CTRL | MODIFIER_ALT
            );
            assert!(
                spec.text.is_none(),
                "shortcut modifier should suppress text insertion"
            );
        }
        _ => panic!("expected Dispatch"),
    }

    assert_eq!(
        resolve_key_press(&KeyPress::single("Back")).unwrap(),
        ResolvedKeyAction::BrowserBack { count: 1 }
    );
    assert!(
        resolve_key_press(&KeyPress {
            key: "a".into(),
            modifiers: vec!["Hyper".into()],
            count: 1,
        })
        .is_err()
    );
    assert!(resolve_key_press(&KeyPress::single("VolumeUp")).is_err());

    // Base64 + PNG validation.
    assert_eq!(decode_base64("aGVsbG8=").unwrap(), b"hello");
    assert!(decode_base64("not!valid").is_err());
    assert!(decode_base64("abc").is_err());
    let png = decode_base64_png("iVBORw0KGgoAAAAN").unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(decode_base64_png("aGVsbG8=").is_err());
}

#[tokio::test]
async fn actions_swipe_keys_back_screenshot_act_and_observe_and_fail_closed() {
    let snap = Arc::new(StdMutex::new(sample_dom_snapshot(false)));
    let fail_dom = Arc::new(StdMutex::new(None));
    let server = MockCdpServer::start(standard_mock_handler(
        Arc::clone(&snap),
        Arc::clone(&fail_dom),
    ))
    .await;
    let driver = WebDriver::with_cdp_url_for_tests(WebDriverConfig::new(), server.ws_url.clone());
    let cancel = CancellationToken::new();
    let app = "http://127.0.0.1:4174";

    // Fail-closed checks.
    assert!(matches!(
        driver.clear_keychain(&cancel).await,
        Err(DriverError::Unsupported(u)) if u.capability == DriverCapability::ClearKeychain
    ));
    assert!(matches!(
        driver
            .launch_app(
                app,
                &LaunchOptions {
                    clear_keychain: true,
                    ..LaunchOptions::default()
                },
                &cancel
            )
            .await,
        Err(DriverError::Unsupported(u)) if u.capability == DriverCapability::ClearKeychain
    ));
    assert!(matches!(
        driver
            .add_media(&[PathBuf::from("photo.png")], &cancel)
            .await,
        Err(DriverError::InvalidArgument(_))
    ));
    let mut bad_args = LaunchOptions::default();
    bad_args
        .arguments
        .insert("flag".into(), LaunchArgumentValue::Bool(true));
    assert!(matches!(
        driver.launch_app(app, &bad_args, &cancel).await,
        Err(DriverError::InvalidArgument(_))
    ));
    let bad_perms = LaunchOptions {
        permissions: Some(BTreeMap::from([("camera".into(), "allow".into())])),
        ..LaunchOptions::default()
    };
    assert!(matches!(
        driver.launch_app(app, &bad_perms, &cancel).await,
        Err(DriverError::InvalidArgument(_))
    ));
    assert!(matches!(
        driver
            .open_link(
                None,
                "https://example.com",
                &OpenLinkOptions::default(),
                &cancel
            )
            .await,
        Err(DriverError::InvalidArgument(_))
    ));

    // `launch_app` MUST accept non-empty `opts.env` (populated by FlowExecutor).
    let mut ok_launch = LaunchOptions::default();
    ok_launch
        .env
        .insert("EMAIL".into(), "qa@example.com".into());
    ok_launch.permissions = Some(BTreeMap::new());
    driver.launch_app(app, &ok_launch, &cancel).await.unwrap();

    // Navigation errorText maps to DriverError::NavigationFailed.
    let nav_err = driver
        .open_link(
            Some(app),
            "http://127.0.0.1:4174/fail-nav",
            &OpenLinkOptions::default(),
            &cancel,
        )
        .await
        .expect_err("errorText must fail navigation");
    assert!(matches!(nav_err, DriverError::NavigationFailed { .. }));

    // Tap, double-tap, long-press, input, erase, pressKey, hideKeyboard, back, screenshot.
    driver
        .tap_point(app, UiPoint::new(150.0, 220.0), &cancel)
        .await
        .unwrap();
    driver
        .double_tap_point(app, UiPoint::new(150.0, 220.0), &cancel)
        .await
        .unwrap();
    driver
        .long_press_point(app, UiPoint::new(150.0, 220.0), Some(15), &cancel)
        .await
        .unwrap();
    driver
        .input_text(app, "qa@example.com", &cancel)
        .await
        .unwrap();
    driver.erase_text(app, 3, &cancel).await.unwrap();
    driver
        .press_key(app, &KeyPress::single("Enter"), &cancel)
        .await
        .unwrap();
    driver
        .press_key(app, &KeyPress::single("Back"), &cancel)
        .await
        .unwrap();
    driver.hide_keyboard(app, &cancel).await.unwrap();
    let png = driver.take_screenshot(app, &cancel).await.unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));

    // Swipe dispatches mouseWheel chunks summing to exact (start.x - end.x, start.y - end.y).
    driver
        .swipe(
            &SwipeParams {
                app_id: app.to_string(),
                start: UiPoint::new(500.0, 600.0),
                end: UiPoint::new(300.0, 200.0),
                duration_ms: 120,
                hold_duration_ms: None,
            },
            &cancel,
        )
        .await
        .unwrap();

    let calls = server.calls();
    let wheel_events: Vec<&RecordedCdpCall> = calls
        .iter()
        .filter(|c| c.method == "Input.dispatchMouseEvent" && c.params["type"] == "mouseWheel")
        .collect();
    assert!(!wheel_events.is_empty());
    let sum_dx: f64 = wheel_events
        .iter()
        .map(|c| c.params["deltaX"].as_f64().unwrap())
        .sum();
    let sum_dy: f64 = wheel_events
        .iter()
        .map(|c| c.params["deltaY"].as_f64().unwrap())
        .sum();
    assert!((sum_dx - 200.0).abs() < 1e-6, "sum_dx={sum_dx}");
    assert!((sum_dy - 400.0).abs() < 1e-6, "sum_dy={sum_dy}");

    // `back` navigated to entryId 11 (skipping `about:blank` entry 10).
    assert!(
        calls
            .iter()
            .any(|c| { c.method == "Page.navigateToHistoryEntry" && c.params["entryId"] == 11 })
    );

    // `act_and_observe` succeeds and returns hierarchy.
    let act_ok = driver
        .act_and_observe(
            &ActPayload {
                app_id: app.to_string(),
                action: ActAction::Tap { x: 100.0, y: 100.0 },
                post_action_delay_ms: Some(10),
                return_hierarchy: true,
                include_system_ui: false,
                known_hash: None,
            },
            &cancel,
        )
        .await
        .unwrap();
    assert!(act_ok.action_ok);
    assert!(act_ok.hierarchy.is_some());
    assert!(act_ok.hierarchy_error.is_none());

    // When follow-up hierarchy fails (non-cancelled), `act_and_observe` preserves `action_ok: true`
    // and records `hierarchy_error` so callers never replay the action.
    *fail_dom.lock().unwrap() = Some("execution context destroyed".to_string());
    let act_partial = driver
        .act_and_observe(
            &ActPayload {
                app_id: app.to_string(),
                action: ActAction::Tap { x: 100.0, y: 100.0 },
                post_action_delay_ms: None,
                return_hierarchy: true,
                include_system_ui: false,
                known_hash: None,
            },
            &cancel,
        )
        .await
        .unwrap();
    assert!(act_partial.action_ok);
    assert!(act_partial.hierarchy.is_none());
    assert!(
        act_partial
            .hierarchy_error
            .as_deref()
            .is_some_and(|m| m.contains("execution context destroyed"))
    );

    driver.shutdown().await.unwrap();
}

// ===========================================================================
// Area 6: Crash signals & zero-unsafe process-group teardown
// ===========================================================================

#[tokio::test]
async fn cdp_target_crash_signals_surface_via_check_app_crash() {
    let server = MockCdpServer::start(standard_mock_handler(
        Arc::new(StdMutex::new(sample_dom_snapshot(false))),
        Arc::new(StdMutex::new(None)),
    ))
    .await;
    let driver = WebDriver::with_cdp_url_for_tests(WebDriverConfig::new(), server.ws_url.clone());
    let cancel = CancellationToken::new();
    let app = "http://127.0.0.1:4174";
    let before = now_epoch_ms();

    driver.ensure_session(&cancel).await.unwrap();
    assert!(
        driver
            .check_app_crash(app, before, &cancel)
            .await
            .unwrap()
            .is_none()
    );

    // Crash for an unrelated targetId is ignored.
    server.push_event(json!({
        "method": "Target.targetCrashed",
        "params": { "targetId": "target-other", "status": "crashed", "errorCode": 11 }
    }));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        driver
            .check_app_crash(app, before, &cancel)
            .await
            .unwrap()
            .is_none()
    );

    // Crash for active targetId is captured.
    server.push_event(json!({
        "method": "Target.targetCrashed",
        "params": { "targetId": "target-1", "status": "killed", "errorCode": 9 }
    }));
    tokio::time::sleep(Duration::from_millis(30)).await;
    let sig = driver
        .check_app_crash(app, before, &cancel)
        .await
        .unwrap()
        .expect("crash signal must be reported");
    assert_eq!(sig.source, CrashSignalSource::CrashReport);
    assert_eq!(sig.process.as_deref(), Some("chromium-renderer"));
    assert!(sig.detail.as_deref().unwrap().contains("errorCode=9"));

    // Future `since_epoch_ms` filters out older crash.
    assert!(
        driver
            .check_app_crash(app, now_epoch_ms() + 60_000, &cancel)
            .await
            .unwrap()
            .is_none()
    );

    // Inspector.targetCrashed on active session also records a crash.
    let before_insp = now_epoch_ms();
    server.push_event(json!({
        "method": "Inspector.targetCrashed",
        "sessionId": "sess-1",
        "params": {}
    }));
    tokio::time::sleep(Duration::from_millis(30)).await;
    let insp_sig = driver
        .check_app_crash(app, before_insp, &cancel)
        .await
        .unwrap()
        .expect("Inspector.targetCrashed must be reported");
    assert!(
        insp_sig
            .detail
            .as_deref()
            .unwrap()
            .contains("Inspector.targetCrashed")
    );

    driver.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn managed_browser_process_group_teardown_and_profile_cleanup() {
    // Verify safe_pgid rejects dangerous PIDs.
    assert!(safe_pgid(0).is_none());
    assert!(safe_pgid(1).is_none());
    assert!(safe_pgid(std::process::id()).is_none());

    // Verify TempProfileDir creates private 0o700 dir and cleans up on Drop.
    let profile_path = {
        let dir = TempProfileDir::create().unwrap();
        let p = dir.path().to_path_buf();
        assert!(p.is_dir());
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        p
    };
    assert!(!profile_path.exists());

    // Spawn a fake browser script that emits a valid DevTools URL and spawns a child sleeper
    // in the same process group, recording the child's PID to a file so we verify the ENTIRE
    // process group is terminated cleanly without leaving orphans or temp profile dirs!
    let ws_dir = TempDir::new("proc-group");
    let child_pid_file = ws_dir.path().join("child.pid");
    let fake_chrome = ws_dir.path().join("fake-chrome.sh");
    write_executable(
        &fake_chrome,
        &format!(
            "#!/bin/sh\n/bin/sleep 60 &\necho $! > \"{}\"\necho \"DevTools listening on ws://127.0.0.1:45555/devtools/browser/fake-pg\" >&2\nwait\n",
            child_pid_file.display()
        ),
    );

    let cancel = CancellationToken::new();
    let mut browser = ManagedBrowser::launch(
        &BrowserLaunchOptions {
            executable: fake_chrome.clone(),
            headless: true,
            screen_size: ScreenSize::default(),
            startup_timeout: Duration::from_secs(5),
            env: BTreeMap::new(),
        },
        &cancel,
    )
    .await
    .unwrap();

    assert_eq!(
        browser.ws_url(),
        "ws://127.0.0.1:45555/devtools/browser/fake-pg"
    );
    let temp_profile = browser.profile_path().unwrap().to_path_buf();
    assert!(temp_profile.is_dir());

    // Wait briefly until child PID file is written.
    let mut child_pid = None;
    for _ in 0..40 {
        if let Ok(raw) = fs::read_to_string(&child_pid_file)
            && let Ok(pid) = raw.trim().parse::<u32>()
        {
            child_pid = Some(pid);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let child_pid = child_pid.expect("fake chrome should have spawned child sleep");

    browser.terminate_now().await;
    assert!(
        !temp_profile.exists(),
        "temp profile dir must be removed on shutdown"
    );

    // Confirm the background grandchild sleep process was terminated by process-group signal.
    let mut alive = true;
    for _ in 0..20 {
        let status = std::process::Command::new("/bin/kill")
            .args(["-0", &child_pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        if !status.success() {
            alive = false;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !alive,
        "child process {child_pid} in Chromium process group must not survive teardown"
    );

    // Early exit surfaces BrowserExitedEarly with captured stderr tail and removes temp profile.
    let crash_chrome = ws_dir.path().join("crash-chrome.sh");
    write_executable(
        &crash_chrome,
        "#!/bin/sh\necho 'fatal gpu error' >&2\nexit 42\n",
    );
    let err = ManagedBrowser::launch(
        &BrowserLaunchOptions {
            executable: crash_chrome,
            headless: true,
            screen_size: ScreenSize::default(),
            startup_timeout: Duration::from_secs(5),
            env: BTreeMap::new(),
        },
        &cancel,
    )
    .await
    .expect_err("early exit must fail launch");
    match err {
        WebDriverError::BrowserExitedEarly {
            status,
            stderr_tail,
        } => {
            assert_eq!(status, Some(42));
            assert!(stderr_tail.contains("fatal gpu error"));
        }
        other => panic!("expected BrowserExitedEarly, got {other:?}"),
    }
}

#[test]
fn recorded_crash_struct_roundtrip_fields() {
    let crash = RecordedCrash {
        epoch_ms: 1_700_000_000_000,
        process: "chromium".to_string(),
        detail: "unexpected exit".to_string(),
    };
    assert_eq!(crash.process, "chromium");
}

// ===========================================================================
// Codex Round 1 Regression Tests (Findings 1 through 7)
// ===========================================================================

#[tokio::test]
async fn codex_r1_f1_ensure_session_rotates_dirty_context_while_keeping_browser_warm() {
    let server = MockCdpServer::start(standard_mock_handler(
        Arc::new(StdMutex::new(sample_dom_snapshot(false))),
        Arc::new(StdMutex::new(None)),
    ))
    .await;
    let mut cfg = WebDriverConfig::new();
    cfg.network_quiet_window = Duration::from_millis(10);
    let driver = WebDriver::with_cdp_url_for_tests(cfg, server.ws_url.clone());
    let cancel = CancellationToken::new();

    // Pre-run `ensure_session` followed by attempt-1 `ensure_session` incurs 0 extra rotation.
    driver.ensure_session(&cancel).await.unwrap();
    driver.ensure_session(&cancel).await.unwrap();
    assert_eq!(driver.attach_count(), 1);
    assert_eq!(
        server
            .calls()
            .iter()
            .filter(|c| c.method == "Target.createBrowserContext")
            .count(),
        1
    );
    assert_eq!(
        server
            .calls()
            .iter()
            .filter(|c| c.method == "Target.disposeBrowserContext")
            .count(),
        0
    );

    // Attempt 1 runs an `openLink`-only flow, marking the BrowserContext dirty.
    driver
        .open_link(
            None,
            "http://127.0.0.1:4174/",
            &OpenLinkOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

    // Attempt 2 / sequential Flow #2 calls `ensure_session`: rotates to a fresh isolated
    // BrowserContext (`ctx-2`) while keeping the warm browser connection alive (`attach_count == 1`).
    driver.ensure_session(&cancel).await.unwrap();
    assert_eq!(
        driver.attach_count(),
        1,
        "context rotation must keep warm browser alive"
    );
    let calls = server.calls();
    assert!(calls.iter().any(|c| {
        c.method == "Target.disposeBrowserContext" && c.params["browserContextId"] == "ctx-1"
    }));
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.method == "Target.createBrowserContext")
            .count(),
        2
    );

    // Calling `ensure_session` again before mutating the fresh context does NOT rotate again.
    driver.ensure_session(&cancel).await.unwrap();
    assert_eq!(
        server
            .calls()
            .iter()
            .filter(|c| c.method == "Target.createBrowserContext")
            .count(),
        2
    );
    driver.shutdown().await.unwrap();
}

#[test]
fn codex_r1_f2_and_f7_dom_visible_frame_and_ancestor_opacity_script() {
    let snap = json!({
        "url": "http://127.0.0.1:4174/",
        "viewport": { "width": 1280, "height": 720 },
        "truncated": false,
        "nodes": [
            {
                "index": 0,
                "parentIndex": null,
                "depth": 0,
                "elementType": "Button",
                "identifier": "clipped_btn",
                "label": "Pay now",
                "title": "",
                "value": null,
                "placeholderValue": null,
                "enabled": true,
                "checked": false,
                "selected": false,
                "hasFocus": false,
                "hittable": true,
                "isOnScreen": true,
                "frame": { "x": 100, "y": 660, "width": 200, "height": 120 },
                "visibleFrame": { "x": 100, "y": 660, "width": 200, "height": 60 }
            },
            {
                "index": 1,
                "parentIndex": null,
                "depth": 0,
                "elementType": "Button",
                "identifier": "hidden_by_ancestor_opacity",
                "label": "Ghost",
                "title": "",
                "value": null,
                "placeholderValue": null,
                "enabled": true,
                "checked": false,
                "selected": false,
                "hasFocus": false,
                "hittable": false,
                "isOnScreen": false,
                "frame": { "x": 10, "y": 10, "width": 100, "height": 40 },
                "visibleFrame": null
            }
        ]
    });

    let h = dom_snapshot_to_hierarchy(snap, "http://127.0.0.1:4174", None).unwrap();
    let clipped = &h.nodes[0];
    assert_eq!(
        clipped.frame,
        deja_flow::UiRect::new(100.0, 660.0, 200.0, 120.0)
    );
    assert_eq!(
        clipped.visible_frame,
        Some(deja_flow::UiRect::new(100.0, 660.0, 200.0, 60.0))
    );
    assert_eq!(clipped.tap_point(), UiPoint::new(200.0, 690.0));
    assert!(
        (clipped
            .frame
            .visible_fraction_in(deja_flow::UiSize::new(1280.0, 720.0))
            - 0.5)
            .abs()
            < 1e-6
    );
    assert!(h.nodes[1].visible_frame.is_none());
    assert!(!h.nodes[1].is_on_screen && !h.nodes[1].hittable);

    // Ensure `AX_DOM_SCRIPT` carries `ancestorOpaque` and emits `visibleFrame`.
    assert!(crate::dom::AX_DOM_SCRIPT.contains("ancestorOpaque"));
    assert!(crate::dom::AX_DOM_SCRIPT.contains("effectiveOpaque"));
    assert!(crate::dom::AX_DOM_SCRIPT.contains("visibleFrame"));
}

#[tokio::test]
async fn codex_r1_f3_back_waits_for_cross_document_and_same_document_history_commit() {
    use std::sync::atomic::AtomicBool;

    // 1. Cross-document back: subframe `Page.frameNavigated` is ignored; `document.readyState`
    // MUST NOT be evaluated until after main-frame `Page.frameNavigated` commits.
    let main_committed = Arc::new(AtomicBool::new(false));
    let saw_premature_ready_state = Arc::new(AtomicBool::new(false));
    let main_committed_c = Arc::clone(&main_committed);
    let premature_c = Arc::clone(&saw_premature_ready_state);
    let server = MockCdpServer::start(Arc::new(move |call, out_tx| {
        let id = call.id;
        let res = match call.method.as_str() {
            "Target.setDiscoverTargets"
            | "Page.enable"
            | "Runtime.enable"
            | "Network.enable"
            | "Emulation.setDeviceMetricsOverride"
            | "Emulation.setEmulatedMedia" => json!({}),
            "Target.createBrowserContext" => json!({ "browserContextId": "ctx-1" }),
            "Target.createTarget" => json!({ "targetId": "target-1" }),
            "Target.attachToTarget" => json!({ "sessionId": "sess-1" }),
            "Page.getNavigationHistory" => json!({
                "currentIndex": 1,
                "entries": [
                    { "id": 1, "url": "http://127.0.0.1:4174/first" },
                    { "id": 2, "url": "http://127.0.0.1:4174/second" }
                ]
            }),
            "Page.navigateToHistoryEntry" => {
                let sid = call.session_id.clone().unwrap();
                let flag = Arc::clone(&main_committed_c);
                tokio::spawn(async move {
                    // Subframe navigation arrives first and MUST be ignored.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let _ = out_tx.send(json!({
                        "method": "Page.frameNavigated",
                        "sessionId": sid,
                        "params": { "frame": { "id": "sub", "parentId": "main", "url": "http://127.0.0.1:4174/iframe" } }
                    }));
                    // Main-frame commit arrives later.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    flag.store(true, Ordering::SeqCst);
                    let _ = out_tx.send(json!({
                        "method": "Page.frameNavigated",
                        "sessionId": sid,
                        "params": { "frame": { "id": "main", "url": "http://127.0.0.1:4174/first" } }
                    }));
                });
                json!({})
            }
            "Runtime.evaluate" => {
                let expr = call
                    .params
                    .get("expression")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if expr == "document.readyState" {
                    if !main_committed_c.load(Ordering::SeqCst) {
                        premature_c.store(true, Ordering::SeqCst);
                    }
                    json!({ "result": { "type": "string", "value": "complete" } })
                } else {
                    json!({ "result": { "type": "boolean", "value": true } })
                }
            }
            _ => json!({}),
        };
        Some(json!({ "id": id, "result": res }))
    }))
    .await;

    let mut cfg = WebDriverConfig::new();
    cfg.network_quiet_window = Duration::from_millis(10);
    let driver = WebDriver::with_cdp_url_for_tests(cfg, server.ws_url.clone());
    let cancel = CancellationToken::new();
    driver.ensure_session(&cancel).await.unwrap();
    driver.back("http://127.0.0.1:4174", &cancel).await.unwrap();
    assert!(main_committed.load(Ordering::SeqCst));
    assert!(
        !saw_premature_ready_state.load(Ordering::SeqCst),
        "back() must wait for main-frame commit before checking document.readyState"
    );
    driver.shutdown().await.unwrap();

    // 2. Same-document SPA/hash back: waits for `Page.navigatedWithinDocument`.
    let spa_committed = Arc::new(AtomicBool::new(false));
    let spa_premature = Arc::new(AtomicBool::new(false));
    let spa_committed_c = Arc::clone(&spa_committed);
    let spa_premature_c = Arc::clone(&spa_premature);
    let spa_server = MockCdpServer::start(Arc::new(move |call, out_tx| {
        let id = call.id;
        let res = match call.method.as_str() {
            "Target.createBrowserContext" => json!({ "browserContextId": "ctx-1" }),
            "Target.createTarget" => json!({ "targetId": "target-1" }),
            "Target.attachToTarget" => json!({ "sessionId": "sess-1" }),
            "Page.getNavigationHistory" => json!({
                "currentIndex": 1,
                "entries": [
                    { "id": 1, "url": "http://127.0.0.1:4174/#a" },
                    { "id": 2, "url": "http://127.0.0.1:4174/#b" }
                ]
            }),
            "Page.navigateToHistoryEntry" => {
                let sid = call.session_id.clone().unwrap();
                let flag = Arc::clone(&spa_committed_c);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(45)).await;
                    flag.store(true, Ordering::SeqCst);
                    let _ = out_tx.send(json!({
                        "method": "Page.navigatedWithinDocument",
                        "sessionId": sid,
                        "params": { "frameId": "main", "url": "http://127.0.0.1:4174/#a" }
                    }));
                });
                json!({})
            }
            "Runtime.evaluate" => {
                let expr = call
                    .params
                    .get("expression")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if expr == "document.readyState" {
                    if !spa_committed_c.load(Ordering::SeqCst) {
                        spa_premature_c.store(true, Ordering::SeqCst);
                    }
                    json!({ "result": { "type": "string", "value": "complete" } })
                } else {
                    json!({ "result": { "type": "boolean", "value": true } })
                }
            }
            _ => json!({}),
        };
        Some(json!({ "id": id, "result": res }))
    }))
    .await;

    let mut spa_cfg = WebDriverConfig::new();
    spa_cfg.network_quiet_window = Duration::from_millis(10);
    let spa_driver = WebDriver::with_cdp_url_for_tests(spa_cfg, spa_server.ws_url.clone());
    spa_driver.ensure_session(&cancel).await.unwrap();
    spa_driver
        .back("http://127.0.0.1:4174", &cancel)
        .await
        .unwrap();
    assert!(spa_committed.load(Ordering::SeqCst));
    assert!(
        !spa_premature.load(Ordering::SeqCst),
        "back() must wait for Page.navigatedWithinDocument before settling"
    );
    spa_driver.shutdown().await.unwrap();
}

#[tokio::test]
async fn codex_r1_f5_failed_context_or_target_replacement_invalidates_old_handle_and_recovers() {
    let create_target_calls = Arc::new(AtomicUsize::new(0));
    let fail_on_target_call = Arc::new(AtomicUsize::new(2));
    let ctx_seq = Arc::new(AtomicUsize::new(0));
    let sess_seq = Arc::new(AtomicUsize::new(0));
    let snap = sample_dom_snapshot(false);

    let ct_c = Arc::clone(&create_target_calls);
    let fail_c = Arc::clone(&fail_on_target_call);
    let ctx_c = Arc::clone(&ctx_seq);
    let sess_c = Arc::clone(&sess_seq);
    let server = MockCdpServer::start(Arc::new(move |call, _out_tx| {
        let id = call.id;
        match call.method.as_str() {
            "Target.createBrowserContext" => {
                let n = ctx_c.fetch_add(1, Ordering::SeqCst) + 1;
                Some(json!({ "id": id, "result": { "browserContextId": format!("ctx-{n}") } }))
            }
            "Target.createTarget" => {
                let n = ct_c.fetch_add(1, Ordering::SeqCst) + 1;
                if n == fail_c.load(Ordering::SeqCst) {
                    return Some(json!({
                        "id": id,
                        "error": { "code": -32000, "message": "simulated Target.createTarget failure" }
                    }));
                }
                Some(json!({ "id": id, "result": { "targetId": format!("target-{n}") } }))
            }
            "Target.attachToTarget" => {
                let n = sess_c.fetch_add(1, Ordering::SeqCst) + 1;
                Some(json!({ "id": id, "result": { "sessionId": format!("sess-{n}") } }))
            }
            "Runtime.evaluate" => Some(json!({
                "id": id,
                "result": { "result": { "type": "object", "value": snap } }
            })),
            _ => Some(json!({ "id": id, "result": {} })),
        }
    }))
    .await;

    let driver = WebDriver::with_cdp_url_for_tests(WebDriverConfig::new(), server.ws_url.clone());
    let cancel = CancellationToken::new();

    // 1. Initial session attaches `sess-1`.
    driver.ensure_session(&cancel).await.unwrap();
    assert_eq!(driver.attach_count(), 1);

    // 2. `clear_state` disposes `ctx-1` and fails on the 2nd `Target.createTarget`.
    let err = driver
        .clear_state("http://127.0.0.1:4174", &cancel)
        .await
        .expect_err("simulated createTarget failure must surface");
    assert!(matches!(err, DriverError::Transport { .. }));

    // 3. Because the old handle was invalidated BEFORE disposal, `ensure_session` MUST NOT reuse
    // `sess-1` and instead cleanly recovers a brand-new session (`sess-2`, `attach_count == 2`).
    driver.ensure_session(&cancel).await.unwrap();
    assert_eq!(driver.attach_count(), 2);
    let h = driver
        .hierarchy("http://127.0.0.1:4174", false, None, &cancel)
        .await
        .unwrap();
    assert_eq!(h.nodes.len(), 3);
    let last_eval = server
        .calls()
        .into_iter()
        .rev()
        .find(|c| c.method == "Runtime.evaluate")
        .unwrap();
    assert_eq!(last_eval.session_id.as_deref(), Some("sess-2"));

    // 4. Same check for `stop_app` failing on the 4th `Target.createTarget` after `Target.closeTarget`.
    fail_on_target_call.store(4, Ordering::SeqCst);
    assert!(
        driver
            .stop_app("http://127.0.0.1:4174", &cancel)
            .await
            .is_err()
    );
    driver.ensure_session(&cancel).await.unwrap();
    assert_eq!(driver.attach_count(), 3);
    driver.shutdown().await.unwrap();
}

#[tokio::test]
async fn codex_r1_f6_open_link_waits_for_post_load_network_settle_and_caps_streaming() {
    use std::sync::atomic::AtomicBool;

    // 1. Post-load fetch stays in-flight for 100ms after `Page.loadEventFired`; `open_link`
    // MUST wait until `Network.loadingFinished` arrives before completing!
    let fetch_finished = Arc::new(AtomicBool::new(false));
    let fetch_finished_c = Arc::clone(&fetch_finished);
    let server = MockCdpServer::start(Arc::new(move |call, out_tx| {
        let id = call.id;
        let res = match call.method.as_str() {
            "Target.createBrowserContext" => json!({ "browserContextId": "ctx-1" }),
            "Target.createTarget" => json!({ "targetId": "target-1" }),
            "Target.attachToTarget" => json!({ "sessionId": "sess-1" }),
            "Page.navigate" => {
                let sid = call.session_id.clone().unwrap();
                let url = call.params.get("url").and_then(Value::as_str).unwrap_or("");
                if url.contains("sse") {
                    let _ = out_tx.send(json!({
                        "method": "Page.loadEventFired",
                        "sessionId": sid,
                        "params": { "timestamp": 1.0 }
                    }));
                    let _ = out_tx.send(json!({
                        "method": "Network.requestWillBeSent",
                        "sessionId": sid,
                        "params": { "requestId": "req-sse", "request": { "url": "http://127.0.0.1:4174/events" } }
                    }));
                } else {
                    let flag = Arc::clone(&fetch_finished_c);
                    let _ = out_tx.send(json!({
                        "method": "Page.loadEventFired",
                        "sessionId": sid,
                        "params": { "timestamp": 1.0 }
                    }));
                    let _ = out_tx.send(json!({
                        "method": "Network.requestWillBeSent",
                        "sessionId": sid,
                        "params": { "requestId": "req-1", "request": { "url": "http://127.0.0.1:4174/api/init" } }
                    }));
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        flag.store(true, Ordering::SeqCst);
                        let _ = out_tx.send(json!({
                            "method": "Network.loadingFinished",
                            "sessionId": sid,
                            "params": { "requestId": "req-1" }
                        }));
                    });
                }
                json!({ "frameId": "f1", "loaderId": "loader-1" })
            }
            "Runtime.evaluate" => {
                let expr = call
                    .params
                    .get("expression")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if expr == "document.readyState" {
                    json!({ "result": { "type": "string", "value": "complete" } })
                } else {
                    json!({ "result": { "type": "boolean", "value": true } })
                }
            }
            _ => json!({}),
        };
        Some(json!({ "id": id, "result": res }))
    }))
    .await;

    let mut cfg = WebDriverConfig::new();
    cfg.network_quiet_window = Duration::from_millis(25);
    cfg.network_settle_cap = Duration::from_millis(120);
    let driver = WebDriver::with_cdp_url_for_tests(cfg, server.ws_url.clone());
    let cancel = CancellationToken::new();

    driver.ensure_session(&cancel).await.unwrap();
    assert!(
        server.calls().iter().any(|c| c.method == "Network.enable"),
        "target setup must enable Network domain"
    );

    let started = std::time::Instant::now();
    driver
        .open_link(
            None,
            "http://127.0.0.1:4174/app",
            &OpenLinkOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
    assert!(
        fetch_finished.load(Ordering::SeqCst),
        "open_link must wait for post-load fetch req-1 to finish"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(95),
        "open_link returned too early before post-load fetch completed: {:?}",
        started.elapsed()
    );

    // 2. Perpetual SSE/long-polling request hits `network_settle_cap` cleanly without hanging.
    let sse_started = std::time::Instant::now();
    driver
        .open_link(
            None,
            "http://127.0.0.1:4174/sse",
            &OpenLinkOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
    assert!(
        sse_started.elapsed() >= Duration::from_millis(100)
            && sse_started.elapsed() < Duration::from_secs(2),
        "SSE page must exit cleanly at network_settle_cap, took {:?}",
        sse_started.elapsed()
    );
    driver.shutdown().await.unwrap();
}
