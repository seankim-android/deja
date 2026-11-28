//! Comprehensive in-process duplex integration tests for `deja-mcp` covering:
//! 1. Newline-delimited & Content-Length framing, blank-line skipping, 4 MiB cap, JSON-RPC errors
//! 2. Initialize negotiation, tools/list schemas, and `mcp.explore` gating
//! 3. All 6 tools, `INVALID_INPUT` (DEJA-41 N7), path/symlink/hardlink/appId/url confinement,
//!    and `deja_exec_batch` / `deja_run_steps` alias (DEJA-84)
//! 4. Concurrency, inline `$/cancelRequest` & `notifications/cancelled`, duplicate ID rejection,
//!    driver teardown, WriterLock release, and stdin EOF cleanup (DEJA-41 N6, DEJA-54)
//! 5. Warm `BuildFingerprintCache` + warm driver reuse + disk-change reinstall,
//!    pruned `visibleElements` (DEJA-83), and full secret/path redaction

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deja_core::StaleBuildPolicy;
use deja_flow::{
    DriverCapabilities, DriverCapability, DriverCrashSignal, DriverError, DriverFuture, KeyPress,
    LaunchOptions, OpenLinkOptions, PlatformDriver, SwipeParams, UiHierarchy, UiNode, UiPoint,
    UiRect, UiSize,
};
use deja_mcp::{
    FramingMode, MAX_FRAME_BYTES, McpBoxFuture, McpDriverContext, McpDriverFactory,
    McpServerConfig, serve,
};
use deja_store::{SqliteStoreOptions, SqliteWalkStore};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const TEST_APP_ID: &str = "dev.deja.mcp.test";
const TEST_DEVICE: &str = "11111111-2222-3333-4444-555555555555";
static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

// ---------------------------------------------------------------------------
// Zero-dev-dependency RAII temporary workspace
// ---------------------------------------------------------------------------

struct TempWs {
    path: PathBuf,
}

impl TempWs {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "deja-mcp-test-{label}-{}-{nanos}-{seq}",
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

    fn write_bytes(&self, rel: &str, bytes: &[u8]) -> PathBuf {
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

// ---------------------------------------------------------------------------
// Deterministic Mock PlatformDriver + McpDriverFactory
// ---------------------------------------------------------------------------

fn make_node(index: usize, id: &str, label: &str) -> UiNode {
    UiNode {
        index,
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
        frame: UiRect::new(10.0, 20.0 + (index as f64 * 10.0), 100.0, 44.0),
        ..UiNode::default()
    }
}

fn make_hierarchy(app_id: &str, nodes: Vec<UiNode>) -> UiHierarchy {
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

type OnLaunchHook = Arc<dyn Fn(usize) + Send + Sync>;

#[derive(Clone)]
struct MockDriver {
    caps: DriverCapabilities,
    hierarchies: Arc<Mutex<Vec<UiHierarchy>>>,
    launch_count: Arc<AtomicUsize>,
    shutdown_count: Arc<AtomicUsize>,
    block_on_launch: Option<Arc<Notify>>,
    launch_started: Option<Arc<Notify>>,
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
            block_on_launch: None,
            launch_started: None,
            on_launch: None,
        }
    }

    fn with_blocking_launch(mut self, started: Arc<Notify>, unblock: Arc<Notify>) -> Self {
        self.launch_started = Some(started);
        self.block_on_launch = Some(unblock);
        self
    }

    fn with_on_launch(mut self, hook: OnLaunchHook) -> Self {
        self.on_launch = Some(hook);
        self
    }
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
            if let Some(ref started) = self.launch_started {
                started.notify_waiters();
            }
            if let Some(ref gate) = self.block_on_launch {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    _ = gate.notified() => {}
                }
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
        let app_id = app_id.to_string();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let mut guard = self.hierarchies.lock().unwrap();
            if guard.len() > 1 {
                Ok(guard.remove(0))
            } else if let Some(h) = guard.first() {
                Ok(h.clone())
            } else {
                Ok(make_hierarchy(&app_id, vec![]))
            }
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
        Box::pin(async { Ok(vec![0x89, b'P', b'N', b'G']) })
    }

    fn check_app_crash<'a>(
        &'a self,
        _app_id: &'a str,
        _since_epoch_ms: u64,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Option<DriverCrashSignal>> {
        Box::pin(async { Ok(None) })
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
    create_count: Arc<AtomicUsize>,
    install_count: Arc<AtomicUsize>,
    installed_fps: Arc<Mutex<Vec<Option<String>>>>,
}

impl MockFactory {
    fn new(runtime: Option<&str>, driver: MockDriver) -> Self {
        Self {
            runtime: runtime.map(ToString::to_string),
            driver,
            create_count: Arc::new(AtomicUsize::new(0)),
            install_count: Arc::new(AtomicUsize::new(0)),
            installed_fps: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl McpDriverFactory for MockFactory {
    fn resolve_runtime<'a>(
        &'a self,
        _ctx: &'a McpDriverContext,
        cancel: &'a CancellationToken,
    ) -> McpBoxFuture<'a, Result<Option<String>, DriverError>> {
        let rt = self.runtime.clone();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            Ok(rt)
        })
    }

    fn install_app<'a>(
        &'a self,
        ctx: &'a McpDriverContext,
        cancel: &'a CancellationToken,
    ) -> McpBoxFuture<'a, Result<(), DriverError>> {
        let fp = ctx.expected_build_fingerprint.clone();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            self.install_count.fetch_add(1, Ordering::SeqCst);
            self.installed_fps.lock().unwrap().push(fp);
            Ok(())
        })
    }

    fn create_driver<'a>(
        &'a self,
        _ctx: &'a McpDriverContext,
    ) -> McpBoxFuture<'a, Result<Box<dyn PlatformDriver>, DriverError>> {
        let drv = self.driver.clone();
        Box::pin(async move {
            self.create_count.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(drv) as Box<dyn PlatformDriver>)
        })
    }
}

// ---------------------------------------------------------------------------
// Duplex test harness helpers
// ---------------------------------------------------------------------------

fn default_config(ws: &TempWs, build_path: Option<PathBuf>) -> McpServerConfig {
    McpServerConfig {
        workspace_root: ws.path.clone(),
        canonical_workspace_root: ws.path.clone(),
        home_dir: None,
        store_id: None,
        platform: "ios".to_string(),
        app_id: TEST_APP_ID.to_string(),
        adapter: Some("maestro-ios/v1".to_string()),
        browser: None,
        build_path,
        device: Some(TEST_DEVICE.to_string()),
        manifest_path: None,
        flows_dir: None,
        timeout_ms: 15_000,
        verification_timeout_ms: 15_000,
        retries: 0,
        crash_report_grace_ms: 0,
        crash_report_dir: None,
        disable_animations: true,
        stale_build: None,
        env: BTreeMap::new(),
        process_env: BTreeMap::new(),
        forbidden_values: Vec::new(),
        mcp_explore: false,
        explore_backend_available: false,
        headless: None,
        screen_size: None,
    }
}

struct McpTestClient {
    writer: DuplexStream,
    reader: BufReader<DuplexStream>,
    mode: FramingMode,
    server_task: Option<tokio::task::JoinHandle<Result<(), deja_mcp::McpServerError>>>,
}

impl McpTestClient {
    fn spawn(
        config: McpServerConfig,
        factory: Arc<dyn McpDriverFactory>,
        mode: FramingMode,
        cancel: CancellationToken,
    ) -> Self {
        let (client_to_server_w, server_r) = tokio::io::duplex(256 * 1024);
        let (server_w, server_to_client_r) = tokio::io::duplex(256 * 1024);
        let handle =
            tokio::spawn(async move { serve(server_r, server_w, config, factory, cancel).await });
        Self {
            writer: client_to_server_w,
            reader: BufReader::new(server_to_client_r),
            mode,
            server_task: Some(handle),
        }
    }

    async fn send(&mut self, value: &Value) {
        let bytes = serde_json::to_vec(value).unwrap();
        match self.mode {
            FramingMode::NewlineDelimited => {
                self.writer.write_all(&bytes).await.unwrap();
                self.writer.write_all(b"\n").await.unwrap();
            }
            FramingMode::ContentLength => {
                let header = format!("Content-Length: {}\r\n\r\n", bytes.len());
                self.writer.write_all(header.as_bytes()).await.unwrap();
                self.writer.write_all(&bytes).await.unwrap();
            }
        }
        self.writer.flush().await.unwrap();
    }

    async fn send_raw(&mut self, raw: &[u8]) {
        self.writer.write_all(raw).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(10), async {
            match self.mode {
                FramingMode::NewlineDelimited => {
                    let mut line = String::new();
                    let n = self.reader.read_line(&mut line).await.unwrap();
                    assert!(
                        n > 0,
                        "unexpected EOF while reading newline-delimited response"
                    );
                    serde_json::from_str(line.trim()).unwrap()
                }
                FramingMode::ContentLength => {
                    let mut len: Option<usize> = None;
                    loop {
                        let mut line = String::new();
                        let n = self.reader.read_line(&mut line).await.unwrap();
                        assert!(n > 0, "unexpected EOF while reading Content-Length header");
                        let trimmed = line.trim_end_matches(['\r', '\n']);
                        if trimmed.is_empty() {
                            break;
                        }
                        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
                            len = Some(rest.trim().parse().unwrap());
                        }
                    }
                    let n = len.expect("response must include Content-Length header");
                    let mut body = vec![0u8; n];
                    self.reader.read_exact(&mut body).await.unwrap();
                    serde_json::from_slice(&body).unwrap()
                }
            }
        })
        .await
        .expect("timed out waiting for MCP server response")
    }

    async fn request(&mut self, id: Value, method: &str, params: Value) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await;
        self.recv().await
    }

    async fn call_tool(&mut self, id: Value, name: &str, arguments: Value) -> Value {
        self.request(
            id,
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments,
            }),
        )
        .await
    }

    async fn close_and_join(mut self) {
        drop(self.writer);
        if let Some(handle) = self.server_task.take() {
            tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .expect("server should exit cleanly on EOF")
                .expect("server task should not panic")
                .expect("server should return Ok(())");
        }
    }
}

// ===========================================================================
// Group 1: Framing (newline + Content-Length), blank lines, 4 MiB cap, errors
// ===========================================================================

#[tokio::test]
async fn group1_newline_and_content_length_framing_errors_and_oversized_cap() {
    let ws = TempWs::new("g1-framing");
    let driver = MockDriver::new(vec![]);
    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), driver));

    // 1A: Newline-delimited framing, blank lines, invalid JSON, invalid envelope, oversized frame.
    let mut nl = McpTestClient::spawn(
        default_config(&ws, None),
        factory.clone(),
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );

    // Blank lines skipped cleanly.
    nl.send_raw(b"\n   \r\n\n").await;
    let pong = nl.request(json!(1), "ping", json!({})).await;
    assert_eq!(pong, json!({"jsonrpc": "2.0", "id": 1, "result": {}}));

    // Invalid JSON -> -32700.
    nl.send_raw(b"{not valid json}\n").await;
    let parse_err = nl.recv().await;
    assert_eq!(parse_err["error"]["code"], -32700);
    assert_eq!(parse_err["id"], Value::Null);

    // Non-object envelope -> -32600.
    nl.send_raw(b"[1,2,3]\n").await;
    let inv_req = nl.recv().await;
    assert_eq!(inv_req["error"]["code"], -32600);

    // Unknown method -> -32601.
    let unknown = nl.request(json!("u1"), "no/suchMethod", json!({})).await;
    assert_eq!(unknown["id"], "u1");
    assert_eq!(unknown["error"]["code"], -32601);

    // Unknown tool -> -32602.
    let bad_tool = nl
        .call_tool(json!("t1"), "deja_nonexistent", json!({}))
        .await;
    assert_eq!(bad_tool["id"], "t1");
    assert_eq!(bad_tool["error"]["code"], -32602);

    // Oversized newline frame (> 4 MiB) drains cleanly and returns -32600, stream recovers!
    let mut big = vec![b'x'; MAX_FRAME_BYTES + 1024];
    big.push(b'\n');
    nl.send_raw(&big).await;
    let big_err = nl.recv().await;
    assert_eq!(big_err["error"]["code"], -32600);

    // Stream still responsive after oversized frame!
    let pong2 = nl.request(json!(2), "ping", json!({})).await;
    assert_eq!(pong2, json!({"jsonrpc": "2.0", "id": 2, "result": {}}));
    nl.close_and_join().await;

    // 1B: Content-Length LSP framing auto-detection + oversized Content-Length drain.
    let mut cl = McpTestClient::spawn(
        default_config(&ws, None),
        factory,
        FramingMode::ContentLength,
        CancellationToken::new(),
    );
    let init = cl
        .request(
            json!(10),
            "initialize",
            json!({"protocolVersion": "2024-11-05"}),
        )
        .await;
    assert_eq!(init["id"], 10);
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init["result"]["serverInfo"]["name"], "deja");

    // Oversized Content-Length frame drains cleanly and replies in Content-Length mode.
    let oversized_len = MAX_FRAME_BYTES + 64;
    let header =
        format!("Content-Type: application/json\r\nContent-Length: {oversized_len}\r\n\r\n");
    cl.send_raw(header.as_bytes()).await;
    cl.send_raw(&vec![b' '; oversized_len]).await;
    let cl_err = cl.recv().await;
    assert_eq!(cl_err["error"]["code"], -32600);

    // Stream still works after oversized Content-Length frame!
    let pong3 = cl.request(json!(11), "ping", json!({})).await;
    assert_eq!(pong3["result"], json!({}));
    cl.close_and_join().await;
}

// ===========================================================================
// Group 2: Initialize, tools/list schemas, and mcp.explore gating
// ===========================================================================

#[tokio::test]
async fn group2_handshake_tools_list_and_explore_gating() {
    let ws = TempWs::new("g2-tools");
    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), MockDriver::new(vec![])));

    // 2A: mcp_explore = false (default)
    let mut client = McpTestClient::spawn(
        default_config(&ws, None),
        factory.clone(),
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );

    let init = client
        .request(
            json!(1),
            "initialize",
            json!({"protocolVersion": "2025-03-26", "capabilities": {}}),
        )
        .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(init["result"]["capabilities"], json!({"tools": {}}));
    assert_eq!(init["result"]["serverInfo"]["name"], "deja");

    // notifications/initialized produces no response; verified by subsequent ping.
    client
        .send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    let list = client.request(json!(2), "tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec![
            "deja_list",
            "deja_inspect",
            "deja_run",
            "deja_remember",
            "deja_exec_batch"
        ]
    );

    // Every tool schema enforces additionalProperties: false and bans confinement overrides.
    let forbidden_props = [
        "workspaceRoot",
        "appId",
        "platform",
        "device",
        "store",
        "buildPath",
        "acceptAssertionChange",
    ];
    for tool in tools {
        let schema = &tool["inputSchema"];
        assert_eq!(schema["additionalProperties"], Value::Bool(false));
        let props = schema["properties"].as_object().unwrap();
        for banned in forbidden_props {
            assert!(
                !props.contains_key(banned),
                "tool {} must never expose `{banned}`",
                tool["name"]
            );
        }
    }

    // Calling deja_explore when disabled returns structured EXPLORE_DISABLED (isError: true).
    let exp_disabled = client.call_tool(json!(3), "deja_explore", json!({})).await;
    assert_eq!(exp_disabled["result"]["isError"], true);
    assert_eq!(
        exp_disabled["result"]["structuredContent"]["code"],
        "EXPLORE_DISABLED"
    );
    client.close_and_join().await;

    // 2B: mcp_explore = true, explore_backend_available = false (Phase 3 default!)
    // `deja_explore` must NOT be advertised in `tools/list` until a backend is wired (DEJA-71),
    // while direct `tools/call("deja_explore")` still validates args and returns UNSUPPORTED_PHASE.
    let mut cfg_exp = default_config(&ws, None);
    cfg_exp.mcp_explore = true;
    cfg_exp.explore_backend_available = false;
    let mut client2 = McpTestClient::spawn(
        cfg_exp,
        factory.clone(),
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );
    let list2 = client2.request(json!(1), "tools/list", json!({})).await;
    let names2: Vec<&str> = list2["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        !names2.contains(&"deja_explore"),
        "deja_explore must not be advertised when explore_backend_available is false"
    );

    // Invalid args -> INVALID_INPUT
    let exp_bad = client2
        .call_tool(json!(2), "deja_explore", json!({"maxSteps": 0}))
        .await;
    assert_eq!(exp_bad["result"]["isError"], true);
    assert_eq!(
        exp_bad["result"]["structuredContent"]["code"],
        "INVALID_INPUT"
    );

    // Valid args -> UNSUPPORTED_PHASE referencing DEJA-71
    let exp_ok = client2
        .call_tool(
            json!(3),
            "deja_explore",
            json!({"goal": "open settings", "maxSteps": 10, "maxDepth": 3}),
        )
        .await;
    assert_eq!(exp_ok["result"]["isError"], true);
    assert_eq!(
        exp_ok["result"]["structuredContent"]["code"],
        "UNSUPPORTED_PHASE"
    );
    assert!(
        exp_ok["result"]["structuredContent"]["message"]
            .as_str()
            .unwrap()
            .contains("DEJA-71")
    );
    client2.close_and_join().await;

    // 2C: mcp_explore = true AND explore_backend_available = true -> advertised in tools/list!
    let mut cfg_exp_avail = default_config(&ws, None);
    cfg_exp_avail.mcp_explore = true;
    cfg_exp_avail.explore_backend_available = true;
    let mut client3 = McpTestClient::spawn(
        cfg_exp_avail,
        factory,
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );
    let list3 = client3.request(json!(1), "tools/list", json!({})).await;
    let names3: Vec<&str> = list3["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names3.contains(&"deja_explore"));
    client3.close_and_join().await;
}

// ===========================================================================
// Group 3: All tools, INVALID_INPUT, confinement, and deja_exec_batch/deja_run_steps
// ===========================================================================

#[tokio::test]
async fn group3_tools_confinement_and_batch_execution() {
    let ws = TempWs::new("g3-tools");
    let outside = TempWs::new("g3-outside");
    let outside_secret = outside.write("secret.json", r#"{"secret":"outside"}"#);
    let outside_flow = outside.write(
        "evil.yaml",
        "appId: dev.deja.mcp.test\n---\n- launchApp\n- assertVisible: Ready\n",
    );

    let build_file = ws.write_bytes("build/App.bin", b"build-v1");
    ws.write(
        "flows/good.yaml",
        "appId: dev.deja.mcp.test\n---\n- launchApp\n- assertVisible: Welcome\n",
    );
    ws.write(
        "flows/bad_app.yaml",
        "appId: com.evil.other\n---\n- launchApp\n",
    );
    ws.write(
        "flows/bad_url.yaml",
        "url: https://evil.example\n---\n- assertVisible: Welcome\n",
    );
    ws.write(
        "flows/bad_sub_parent.yaml",
        "appId: dev.deja.mcp.test\n---\n- runFlow: bad_sub_child.yaml\n",
    );
    ws.write(
        "flows/bad_sub_child.yaml",
        "appId: dev.deja.mcp.test\n---\n- killApp: com.apple.Preferences\n",
    );
    ws.write(
        "flows/has_secret.yaml",
        "appId: dev.deja.mcp.test\n---\n- launchApp\n- inputText: \"FixturePass999!\"\n",
    );
    ws.write(
        ".deja/internal.yaml",
        "appId: dev.deja.mcp.test\n---\n- launchApp\n",
    );
    ws.write(
        "flows/fail.yaml",
        "appId: dev.deja.mcp.test\n---\n- launchApp\n- assertVisible:\n    text: MissingElement\n    timeout: 50\n",
    );
    ws.write("deja.config.json", r#"{"platform":"ios"}"#);

    #[cfg(unix)]
    {
        fs::create_dir_all(ws.path.join(".deja/reports")).unwrap();
        std::os::unix::fs::symlink(
            &outside_secret,
            ws.path.join(".deja/reports/symlink-report.json"),
        )
        .unwrap();
        let target_report = ws.write(".deja/reports/real.json", r#"{"ok":true}"#);
        fs::hard_link(&target_report, ws.path.join(".deja/reports/hardlink.json")).unwrap();

        std::os::unix::fs::symlink(&outside_flow, ws.path.join("flows/symlink.yaml")).unwrap();
        fs::hard_link(
            ws.path.join("flows/good.yaml"),
            ws.path.join("flows/hardlink.yaml"),
        )
        .unwrap();
    }
    #[cfg(not(unix))]
    {
        let _ = (&outside_secret, &outside_flow);
    }

    let ok_tree = make_hierarchy(
        TEST_APP_ID,
        vec![
            make_node(1, "welcome_id", "Welcome"),
            make_node(2, "btn_next", "Continue"),
        ],
    );
    let driver = MockDriver::new(vec![ok_tree]);
    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), driver));

    let mut cfg = default_config(&ws, Some(build_file));
    cfg.forbidden_values = vec!["FixturePass999!".to_string()];

    let mut client = McpTestClient::spawn(
        cfg,
        factory,
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );

    // 1. Empty store deja_list
    let empty_list = client.call_tool(json!(1), "deja_list", json!({})).await;
    assert_eq!(empty_list["result"]["isError"], false);
    assert_eq!(empty_list["result"]["structuredContent"]["count"], 0);
    assert_eq!(
        empty_list["result"]["structuredContent"]["walks"],
        json!([])
    );

    // 2. DEJA-41 N7: Malformed arguments -> JSON-RPC success with isError: true + INVALID_INPUT
    for (idx, (tool, bad_args)) in [
        ("deja_list", json!({"workspaceRoot": "/tmp"})),
        (
            "deja_inspect",
            json!({"goal": "a", "report": ".deja/reports/latest.json"}),
        ),
        ("deja_run", json!({"goal": "login", "appId": "com.evil"})),
        ("deja_run", json!({"goal": "login", "retries": 6})),
        ("deja_run", json!({"goal": ""})),
        (
            "deja_remember",
            json!({"goal": "login", "flow": "flows/good.yaml", "acceptAssertionChange": true}),
        ),
        (
            "deja_remember",
            json!({"goal": "login", "flow": "flows/good.txt"}),
        ),
        ("deja_exec_batch", json!({"steps": []})),
        ("deja_exec_batch", json!({"steps": "not: valid: [yaml"})),
    ]
    .into_iter()
    .enumerate()
    {
        let resp = client.call_tool(json!(100 + idx), tool, bad_args).await;
        assert!(
            resp.get("error").is_none(),
            "tool {tool} malformed args must return JSON-RPC result, not protocol error: {resp}"
        );
        assert_eq!(resp["result"]["isError"], true);
        assert_eq!(
            resp["result"]["structuredContent"]["code"], "INVALID_INPUT",
            "expected INVALID_INPUT for {tool}, got: {resp}"
        );
    }

    // 3. deja_inspect report confinement (traversal, outside .deja/reports, symlink, hardlink)
    for (idx, bad_report) in [
        "../outside.json",
        "/etc/passwd",
        "deja.config.json",
        ".deja/reports/../../deja.config.json",
        #[cfg(unix)]
        ".deja/reports/symlink-report.json",
        #[cfg(unix)]
        ".deja/reports/hardlink.json",
    ]
    .into_iter()
    .enumerate()
    {
        let resp = client
            .call_tool(
                json!(200 + idx),
                "deja_inspect",
                json!({"report": bad_report}),
            )
            .await;
        assert_eq!(
            resp["result"]["isError"], true,
            "must reject report {bad_report}"
        );
        assert_eq!(
            resp["result"]["structuredContent"]["code"], "PATH_CONFINEMENT",
            "expected PATH_CONFINEMENT for {bad_report}, got: {resp}"
        );
    }

    // 4. deja_remember confinement (path escape, .deja/, symlink, hardlink, appId, url, subflow, secret)
    for (idx, (flow, expected_code)) in [
        ("../evil.yaml", "PATH_CONFINEMENT"),
        ("/etc/evil.yaml", "PATH_CONFINEMENT"),
        (".deja/internal.yaml", "PATH_CONFINEMENT"),
        #[cfg(unix)]
        ("flows/symlink.yaml", "PATH_CONFINEMENT"),
        #[cfg(unix)]
        ("flows/hardlink.yaml", "PATH_CONFINEMENT"),
        ("flows/bad_app.yaml", "APP_ID_CONFINEMENT"),
        ("flows/bad_url.yaml", "APP_ID_CONFINEMENT"),
        ("flows/bad_sub_parent.yaml", "APP_ID_CONFINEMENT"),
        ("flows/has_secret.yaml", "FORBIDDEN_SECRET"),
    ]
    .into_iter()
    .enumerate()
    {
        let resp = client
            .call_tool(
                json!(300 + idx),
                "deja_remember",
                json!({"goal": "confined test", "flow": flow}),
            )
            .await;
        assert_eq!(resp["result"]["isError"], true, "must reject flow {flow}");
        assert_eq!(
            resp["result"]["structuredContent"]["code"], expected_code,
            "expected {expected_code} for {flow}, got: {resp}"
        );
    }

    // 5. Failing candidate in deja_remember is NOT persisted, returns failure + visibleElements!
    let fail_rem = client
        .call_tool(
            json!(350),
            "deja_remember",
            json!({"goal": "never saved", "flow": "flows/fail.yaml"}),
        )
        .await;
    assert_eq!(fail_rem["result"]["isError"], true);
    assert_eq!(fail_rem["result"]["structuredContent"]["passed"], false);
    assert!(
        fail_rem["result"]["structuredContent"]["visibleElements"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "Welcome")
    );
    let after_fail_list = client.call_tool(json!(351), "deja_list", json!({})).await;
    assert_eq!(after_fail_list["result"]["structuredContent"]["count"], 0);

    // 6. Passing deja_remember persists walk, then deja_list, deja_inspect, and deja_run all work!
    // Remove hardlink on good.yaml first so good.yaml has nlink == 1!
    #[cfg(unix)]
    let _ = fs::remove_file(ws.path.join("flows/hardlink.yaml"));

    let ok_rem = client
        .call_tool(
            json!(400),
            "deja_remember",
            json!({"goal": "Welcome Flow", "flow": "flows/good.yaml"}),
        )
        .await;
    assert_eq!(
        ok_rem["result"]["isError"], false,
        "remember failed: {ok_rem}"
    );
    assert_eq!(
        ok_rem["result"]["structuredContent"]["outcome"],
        "remembered"
    );

    let list_after = client.call_tool(json!(401), "deja_list", json!({})).await;
    assert_eq!(list_after["result"]["structuredContent"]["count"], 1);

    let inspect_goal = client
        .call_tool(json!(402), "deja_inspect", json!({"goal": "welcome flow"}))
        .await;
    assert_eq!(inspect_goal["result"]["isError"], false);
    assert_eq!(
        inspect_goal["result"]["structuredContent"]["walk"]["goal"],
        "Welcome Flow"
    );
    assert!(
        !inspect_goal["result"]["structuredContent"]["validationRuns"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Default deja_inspect reads `.deja/reports/latest.json` written by remember/run.
    let inspect_latest = client
        .call_tool(json!(403), "deja_inspect", json!({}))
        .await;
    assert_eq!(inspect_latest["result"]["isError"], false);
    assert_eq!(
        inspect_latest["result"]["structuredContent"]["reportPath"],
        ".deja/reports/latest.json"
    );

    // Replay with deja_run -> cache hit!
    let run_hit = client
        .call_tool(json!(404), "deja_run", json!({"goal": "Welcome Flow"}))
        .await;
    assert_eq!(run_hit["result"]["isError"], false);
    assert_eq!(run_hit["result"]["structuredContent"]["passed"], true);
    assert_eq!(run_hit["result"]["structuredContent"]["cacheStatus"], "hit");

    // 7. deja_exec_batch & deja_run_steps alias (YAML string + JSON object array + rejections)
    let batch_yaml = client
        .call_tool(
            json!(500),
            "deja_exec_batch",
            json!({
                "steps": "- launchApp\n- assertVisible: Welcome\n",
                "observeFinal": true
            }),
        )
        .await;
    assert_eq!(
        batch_yaml["result"]["isError"], false,
        "batch yaml: {batch_yaml}"
    );
    assert_eq!(batch_yaml["result"]["structuredContent"]["passed"], true);
    assert_eq!(
        batch_yaml["result"]["structuredContent"]["stepsExecuted"],
        2
    );
    assert!(
        batch_yaml["result"]["structuredContent"]["visibleElements"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "Welcome")
    );

    let batch_alias = client
        .call_tool(
            json!(501),
            "deja_run_steps",
            json!({
                "steps": ["launchApp", {"assertVisible": "Continue"}],
                "observeFinal": false
            }),
        )
        .await;
    assert_eq!(
        batch_alias["result"]["isError"], false,
        "alias: {batch_alias}"
    );
    assert_eq!(
        batch_alias["result"]["structuredContent"]["stepsExecuted"],
        2
    );
    assert!(
        batch_alias["result"]["structuredContent"]
            .get("visibleElements")
            .is_none(),
        "observeFinal: false must omit visibleElements"
    );

    // Batch rejections: >50 steps, YAML anchor amplification >50, >64 KiB, runFlow.file, runScript,
    // addMedia, unconfined takeScreenshot, appId mismatch.
    let too_many: Vec<Value> = (0..51).map(|_| json!("scroll")).collect();
    let r_many = client
        .call_tool(json!(510), "deja_exec_batch", json!({"steps": too_many}))
        .await;
    assert_eq!(r_many["result"]["isError"], true);
    assert_eq!(
        r_many["result"]["structuredContent"]["code"],
        "INVALID_INPUT"
    );

    // YAML anchor amplification: short YAML string expanding to 51 top-level steps!
    let mut amp_yaml = String::from("- &s scroll\n");
    for _ in 0..50 {
        amp_yaml.push_str("- *s\n");
    }
    let r_amp = client
        .call_tool(json!(511), "deja_exec_batch", json!({"steps": amp_yaml}))
        .await;
    assert_eq!(r_amp["result"]["isError"], true);
    assert_eq!(
        r_amp["result"]["structuredContent"]["code"],
        "INVALID_INPUT"
    );

    let big_yaml = format!("- inputText: \"{}\"\n", "a".repeat(65 * 1024));
    let r_big = client
        .call_tool(json!(512), "deja_exec_batch", json!({"steps": big_yaml}))
        .await;
    assert_eq!(r_big["result"]["isError"], true);
    assert_eq!(
        r_big["result"]["structuredContent"]["code"],
        "INVALID_INPUT"
    );

    for (idx, bad_yaml) in [
        "- runFlow: flows/good.yaml\n",
        "- retry:\n    maxRetries: 1\n    file: flows/good.yaml\n",
        "- addMedia:\n    - fixtures/pic.png\n",
        "- takeScreenshot: ../escape.png\n",
        "- takeScreenshot: /tmp/escape.png\n",
        "- takeScreenshot: .deja/memory/pwn.png\n",
        "appId: com.evil.app\n---\n- launchApp\n",
        "url: https://evil.example\n---\n- launchApp\n",
        "- runScript: hack.js\n",
    ]
    .into_iter()
    .enumerate()
    {
        let resp = client
            .call_tool(
                json!(520 + idx),
                "deja_exec_batch",
                json!({"steps": bad_yaml}),
            )
            .await;
        assert_eq!(
            resp["result"]["isError"], true,
            "batch must reject: {bad_yaml}"
        );
    }

    client.close_and_join().await;
}

// ===========================================================================
// Group 4: Concurrency, $/cancelRequest, notifications/cancelled, dup ID, EOF
// ===========================================================================

#[tokio::test]
async fn group4_concurrency_cancellation_lock_release_and_eof_teardown() {
    let ws = TempWs::new("g4-cancel");
    let build_file = ws.write_bytes("build/App.bin", b"v1");
    ws.write(
        "flows/slow.yaml",
        "appId: dev.deja.mcp.test\n---\n- launchApp\n- assertVisible: Ready\n",
    );

    let launch_started = Arc::new(Notify::new());
    let unblock_launch = Arc::new(Notify::new());
    let ok_tree = make_hierarchy(TEST_APP_ID, vec![make_node(1, "ready", "Ready")]);
    let driver = MockDriver::new(vec![ok_tree])
        .with_blocking_launch(launch_started.clone(), unblock_launch.clone());
    let shutdown_count = driver.shutdown_count.clone();
    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), driver));
    let install_count = factory.install_count.clone();

    let mut client = McpTestClient::spawn(
        default_config(&ws, Some(build_file)),
        factory.clone(),
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );

    // Kick off blocking deja_remember (id = 42).
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "tools/call",
            "params": {
                "name": "deja_remember",
                "arguments": { "goal": "Slow Goal", "flow": "flows/slow.yaml" }
            }
        }))
        .await;

    // Wait until driver launch_app is actually blocked in-flight.
    tokio::time::timeout(Duration::from_secs(5), launch_started.notified())
        .await
        .expect("launch_app should start");

    // 1. Non-mutating methods stay immediately responsive while exec_mutex is held!
    let ping = client.request(json!(100), "ping", json!({})).await;
    assert_eq!(ping["id"], 100);
    assert_eq!(ping["result"], json!({}));

    let tools = client.request(json!(101), "tools/list", json!({})).await;
    assert_eq!(tools["id"], 101);
    assert!(tools["result"]["tools"].is_array());

    let list = client.call_tool(json!(102), "deja_list", json!({})).await;
    assert_eq!(list["id"], 102);
    assert_eq!(list["result"]["structuredContent"]["count"], 0);

    // 2. Duplicate in-flight request ID rejected with -32600!
    let dup = client.call_tool(json!(42), "deja_list", json!({})).await;
    assert_eq!(dup["id"], 42);
    assert_eq!(dup["error"]["code"], -32600);

    // 3. Cancel in-flight request 42 via $/cancelRequest!
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "$/cancelRequest",
            "params": { "id": 42 }
        }))
        .await;

    let cancelled = client.recv().await;
    assert_eq!(cancelled["id"], 42);
    assert_eq!(cancelled["error"]["code"], -32800);

    // Driver was shut down, WriterLock was released, and cancelled walk was NOT persisted!
    assert_eq!(shutdown_count.load(Ordering::SeqCst), 1);
    let writable = SqliteWalkStore::open_writable(SqliteStoreOptions::new(&ws.path))
        .expect("WriterLock must be released immediately after cancellation");
    assert!(
        writable.list_walks().unwrap().is_empty(),
        "cancelled remember must never persist partial state"
    );
    writable.close();

    // 4. Also verify notifications/cancelled works identically (and re-installs after teardown)!
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": "req-notif-cancel",
            "method": "tools/call",
            "params": {
                "name": "deja_exec_batch",
                "arguments": { "steps": ["launchApp"] }
            }
        }))
        .await;
    tokio::time::timeout(Duration::from_secs(5), launch_started.notified())
        .await
        .expect("second launch_app should start");
    assert_eq!(
        install_count.load(Ordering::SeqCst),
        2,
        "after cancellation teardown cleared installed_build_fingerprint, next call must reinstall"
    );

    client
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": "req-notif-cancel" }
        }))
        .await;
    let cancelled2 = client.recv().await;
    assert_eq!(cancelled2["id"], "req-notif-cancel");
    assert_eq!(cancelled2["error"]["code"], -32800);
    assert_eq!(shutdown_count.load(Ordering::SeqCst), 2);

    // 5. Close stdin (EOF) while a warm driver is idle -> must call driver.shutdown()!
    let ws2 = TempWs::new("g4-eof");
    let build2 = ws2.write_bytes("build/App.bin", b"v1");
    let idle_driver = MockDriver::new(vec![make_hierarchy(
        TEST_APP_ID,
        vec![make_node(1, "ok", "OK")],
    )]);
    let idle_shutdowns = idle_driver.shutdown_count.clone();
    let idle_factory = Arc::new(MockFactory::new(Some("ios-27.2"), idle_driver));
    let mut c2 = McpTestClient::spawn(
        default_config(&ws2, Some(build2)),
        idle_factory,
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );
    let r = c2
        .call_tool(json!(1), "deja_exec_batch", json!({"steps": ["launchApp"]}))
        .await;
    assert_eq!(r["result"]["isError"], false);
    assert_eq!(
        idle_shutdowns.load(Ordering::SeqCst),
        0,
        "warm driver stays alive while idle"
    );
    c2.close_and_join().await;
    assert_eq!(
        idle_shutdowns.load(Ordering::SeqCst),
        1,
        "EOF teardown must shut down idle warm driver"
    );

    // 6. Close stdin (EOF) WHILE a blocking remember is in-flight -> must cancel, shut down driver,
    //    release WriterLock, and leave zero partial writes!
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 999,
            "method": "tools/call",
            "params": {
                "name": "deja_remember",
                "arguments": { "goal": "EOF Cancelled", "flow": "flows/slow.yaml" }
            }
        }))
        .await;
    tokio::time::timeout(Duration::from_secs(5), launch_started.notified())
        .await
        .expect("in-flight launch should start before EOF");
    client.close_and_join().await;
    assert_eq!(shutdown_count.load(Ordering::SeqCst), 3);
    let writable2 = SqliteWalkStore::open_writable(SqliteStoreOptions::new(&ws.path))
        .expect("WriterLock must be released immediately after EOF teardown");
    assert!(writable2.list_walks().unwrap().is_empty());
    writable2.close();
}

// ===========================================================================
// Group 5: Warm BuildFingerprintCache hit + disk-change invalidation,
//          visibleElements pruning (DEJA-83), and full secret/path redaction
// ===========================================================================

#[tokio::test]
async fn group5_warm_cache_invalidation_pruned_visible_elements_and_redaction() {
    let ws = TempWs::new("g5-warm-redact");
    let build_file = ws.write_bytes("build/App.bin", b"binary-v1");
    ws.write(
        "flows/basic.yaml",
        "appId: dev.deja.mcp.test\n---\n- launchApp\n- assertVisible: Home\n",
    );

    // Build a hierarchy with 45 visible elements INCLUDING secrets and paths to test pruning + redaction!
    let dyn_openai_key = ["sk", "-1234567890abcdefghijklmnop"].concat();
    let fixture_secret = "TopSecretPass77!";
    let mut nodes = vec![
        make_node(0, "home", "Home"),
        make_node(
            1,
            "secret_elem",
            &format!(
                "User {fixture_secret} key {dyn_openai_key} at {}",
                ws.path.display()
            ),
        ),
    ];
    for i in 2..45 {
        nodes.push(make_node(
            i,
            &format!("id_{i:02}"),
            &format!("Label {i:02}"),
        ));
    }
    let big_tree = make_hierarchy(TEST_APP_ID, nodes);
    let driver = MockDriver::new(vec![big_tree]);
    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), driver));
    let create_count = factory.create_count.clone();
    let install_count = factory.install_count.clone();

    let mut cfg = default_config(&ws, Some(build_file.clone()));
    cfg.stale_build = Some(StaleBuildPolicy::ReplayLastVerified);
    cfg.forbidden_values = vec![fixture_secret.to_string()];

    let mut client = McpTestClient::spawn(
        cfg,
        factory.clone(),
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );

    // 1. Remember a goal whose name contains the mixed-case fixture secret!
    let rem = client
        .call_tool(
            json!(1),
            "deja_remember",
            json!({
                "goal": format!("Sign in with {fixture_secret}"),
                "flow": "flows/basic.yaml"
            }),
        )
        .await;
    assert_eq!(rem["result"]["isError"], false, "remember failed: {rem}");
    let rem_str = serde_json::to_string(&rem).unwrap();
    assert!(
        !rem_str.contains(fixture_secret),
        "exact secret leaked: {rem_str}"
    );
    assert!(
        !rem_str.contains(&fixture_secret.to_lowercase()),
        "lowercased secret leaked via goalKey: {rem_str}"
    );
    assert!(
        !rem_str.contains(&ws.path.display().to_string()),
        "workspace path leaked: {rem_str}"
    );
    assert_eq!(create_count.load(Ordering::SeqCst), 1);
    assert_eq!(install_count.load(Ordering::SeqCst), 1);

    // 2. Call deja_run twice on unchanged build -> warm driver reused AND install_app skipped!
    let run1 = client
        .call_tool(
            json!(2),
            "deja_run",
            json!({"goal": format!("Sign in with {fixture_secret}")}),
        )
        .await;
    assert_eq!(run1["result"]["isError"], false);
    assert_eq!(run1["result"]["structuredContent"]["cacheStatus"], "hit");
    assert_eq!(create_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        install_count.load(Ordering::SeqCst),
        1,
        "unchanged build must reuse warm install and skip install_app"
    );

    // 3. Mutate build file on disk -> cache invalidates and triggers immediate reinstall!
    fs::write(&build_file, b"binary-v2-changed-size").unwrap();
    let run2 = client
        .call_tool(
            json!(3),
            "deja_run",
            json!({"goal": format!("Sign in with {fixture_secret}")}),
        )
        .await;
    assert_eq!(
        run2["result"]["isError"], false,
        "staleBuild replay failed: {run2}"
    );
    assert_eq!(
        install_count.load(Ordering::SeqCst),
        2,
        "changed build on disk must immediately trigger install_app"
    );
    assert_ne!(
        run1["result"]["structuredContent"]["buildFingerprint"],
        run2["result"]["structuredContent"]["buildFingerprint"]
    );

    // 4. Pruned visibleElements (<= 30 items, truncated: true) AND full redaction on batch pass & fail!
    let batch = client
        .call_tool(
            json!(4),
            "deja_exec_batch",
            json!({
                "steps": ["launchApp", {"assertVisible": "Home"}],
                "observeFinal": true
            }),
        )
        .await;
    assert_eq!(batch["result"]["isError"], false);
    let elems = batch["result"]["structuredContent"]["visibleElements"]
        .as_array()
        .unwrap();
    assert_eq!(elems.len(), 30, "visibleElements must be capped at 30");
    assert_eq!(
        batch["result"]["structuredContent"]["visibleElementsTruncated"],
        true
    );
    let batch_str = serde_json::to_string(&batch).unwrap();
    assert!(
        !batch_str.contains(fixture_secret),
        "fixture secret leaked in visibleElements"
    );
    assert!(
        !batch_str.contains(&dyn_openai_key),
        "provider key leaked in visibleElements"
    );
    assert!(
        !batch_str.contains(&ws.path.display().to_string()),
        "workspace path leaked in visibleElements"
    );

    // 5. Verify deja_list & deja_inspect also redact mixed-case secrets in goalKey!
    let list = client.call_tool(json!(5), "deja_list", json!({})).await;
    let list_str = serde_json::to_string(&list).unwrap();
    assert!(!list_str.contains(fixture_secret));
    assert!(!list_str.contains(&fixture_secret.to_lowercase()));

    client.close_and_join().await;
}

#[tokio::test]
async fn group5_post_run_drift_blocks_remember_persistence() {
    let ws = TempWs::new("g5-drift");
    let build_file = ws.write_bytes("build/App.bin", b"v1");
    let flow_path = ws.write(
        "flows/drift.yaml",
        "appId: dev.deja.mcp.test\n---\n- launchApp\n- assertVisible: Ready\n",
    );

    let flow_path_clone = flow_path.clone();
    let driver = MockDriver::new(vec![make_hierarchy(
        TEST_APP_ID,
        vec![make_node(1, "r", "Ready")],
    )])
    .with_on_launch(Arc::new(move |_| {
        // Modify flow on disk mid-execution!
        fs::write(
            &flow_path_clone,
            "appId: dev.deja.mcp.test\n---\n- launchApp\n- assertVisible: Mutated\n",
        )
        .unwrap();
    }));
    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), driver));

    let mut client = McpTestClient::spawn(
        default_config(&ws, Some(build_file)),
        factory,
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );

    let rem = client
        .call_tool(
            json!(1),
            "deja_remember",
            json!({"goal": "Drift Test", "flow": "flows/drift.yaml"}),
        )
        .await;
    assert_eq!(rem["result"]["isError"], true);
    assert_eq!(
        rem["result"]["structuredContent"]["outcome"],
        "flow-changed"
    );

    let list = client.call_tool(json!(2), "deja_list", json!({})).await;
    assert_eq!(list["result"]["structuredContent"]["count"], 0);
    client.close_and_join().await;
}

#[tokio::test]
async fn group3_deja_run_flaky_retry_and_pruned_failure_visible_elements() {
    let ws = TempWs::new("g3-flaky");
    let build_file = ws.write_bytes("build/App.bin", b"v1");
    ws.write(
        "flows/flaky.yaml",
        "appId: dev.deja.mcp.test\n---\n- launchApp\n- assertVisible:\n    text: TargetReady\n    timeout: 40\n",
    );

    // 1. First remember on a passing screen (TargetReady visible).
    // 2. Then deja_run with rerunFailed: true:
    //    - Attempt 1 sees 35 wrong elements (no TargetReady) -> fails with pruned visibleElements (30 + truncated).
    //    - Attempt 2 sees TargetReady -> passes -> aggregate outcome is "flaky"!
    let ok_tree = make_hierarchy(TEST_APP_ID, vec![make_node(0, "t", "TargetReady")]);
    let mut bad_nodes = Vec::new();
    for i in 0..35 {
        bad_nodes.push(make_node(
            i,
            &format!("wrong_{i:02}"),
            &format!("Wrong {i:02}"),
        ));
    }
    let bad_tree = make_hierarchy(TEST_APP_ID, bad_nodes);

    // Script: remember polls ok_tree; then on launch #2 (run attempt 1) switch to bad_tree;
    // on launch #3 (run attempt 2) switch back to ok_tree.
    let shared_trees = Arc::new(Mutex::new(vec![ok_tree.clone()]));
    let trees_for_hook = shared_trees.clone();
    let bad_clone = bad_tree.clone();
    let ok_clone = ok_tree.clone();
    let mut driver = MockDriver::new(vec![]);
    driver.hierarchies = shared_trees;
    driver = driver.with_on_launch(Arc::new(move |n| {
        let mut g = trees_for_hook.lock().unwrap();
        g.clear();
        if n == 2 {
            g.push(bad_clone.clone());
        } else {
            g.push(ok_clone.clone());
        }
    }));
    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), driver));

    let mut client = McpTestClient::spawn(
        default_config(&ws, Some(build_file)),
        factory,
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );

    let rem = client
        .call_tool(
            json!(1),
            "deja_remember",
            json!({"goal": "Flaky Goal", "flow": "flows/flaky.yaml"}),
        )
        .await;
    assert_eq!(rem["result"]["isError"], false);

    let run = client
        .call_tool(
            json!(2),
            "deja_run",
            json!({"goal": "Flaky Goal", "rerunFailed": true}),
        )
        .await;
    assert_eq!(run["result"]["isError"], true);
    assert_eq!(run["result"]["structuredContent"]["outcome"], "flaky");
    assert_eq!(
        run["result"]["structuredContent"]["attempts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let vis = run["result"]["structuredContent"]["visibleElements"]
        .as_array()
        .unwrap();
    assert_eq!(vis.len(), 30);
    assert_eq!(
        run["result"]["structuredContent"]["visibleElementsTruncated"],
        true
    );
    client.close_and_join().await;
}

// ===========================================================================
// Round 1 Codex cross-review regression tests (Findings #1, #2, #4, #5, #6, #7, #8)
// ===========================================================================

#[tokio::test]
async fn regression_r1_f1_split_write_frame_preserved_when_tool_finishes() {
    let ws = TempWs::new("r1-f1-split");
    let build_file = ws.write_bytes("build/App.bin", b"v1");
    let launch_started = Arc::new(Notify::new());
    let unblock_launch = Arc::new(Notify::new());
    let driver = MockDriver::new(vec![make_hierarchy(
        TEST_APP_ID,
        vec![make_node(0, "ok", "OK")],
    )])
    .with_blocking_launch(launch_started.clone(), unblock_launch.clone());
    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), driver));

    let mut client = McpTestClient::spawn(
        default_config(&ws, Some(build_file)),
        factory,
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );

    // 1. Start a blocking tool call.
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "deja_exec_batch", "arguments": { "steps": ["launchApp"] } }
        }))
        .await;
    tokio::time::timeout(Duration::from_secs(5), launch_started.notified())
        .await
        .expect("launch started");

    // 2. Write ONLY the first half of a `ping` request while the tool is still running!
    let full_ping = r#"{"jsonrpc":"2.0","id":2,"method":"ping","params":{}}"#;
    let (half1, half2) = full_ping.split_at(full_ping.len() / 2);
    client.send_raw(half1.as_bytes()).await;

    // 3. Unblock the tool and receive its completion response (so JoinSet reaps the task
    //    while half1 is sitting in FramedReader's buffer!).
    unblock_launch.notify_waiters();
    let r1 = client.recv().await;
    assert_eq!(r1["id"], 1);
    assert_eq!(r1["result"]["isError"], false);

    // 4. Now write the second half + newline -> ping MUST succeed cleanly (never -32700!).
    client.send_raw(format!("{half2}\n").as_bytes()).await;
    let r2 = client.recv().await;
    assert_eq!(
        r2,
        json!({"jsonrpc": "2.0", "id": 2, "result": {}}),
        "split-write ping must not lose bytes when a tool task finishes"
    );

    client.close_and_join().await;
}

#[tokio::test]
async fn regression_r1_f2_mcp_screenshots_confined_to_dedicated_dir_and_no_clobber() {
    let ws = TempWs::new("r1-f2-shots");
    let build_file = ws.write_bytes("build/App.bin", b"v1");
    ws.write(
        "flows/snap_good.yaml",
        "appId: dev.deja.mcp.test\n---\n- launchApp\n- takeScreenshot: flow-shot\n",
    );
    ws.write(
        "flows/snap_escape.yaml",
        "appId: dev.deja.mcp.test\n---\n- launchApp\n- takeScreenshot: ../escape\n",
    );
    ws.write(
        "flows/snap_existing.yaml",
        "appId: dev.deja.mcp.test\n---\n- launchApp\n- takeScreenshot: preexisting\n",
    );
    ws.write(
        ".deja/reports/stores/sim-s/mcp-screenshots/preexisting.png",
        "original-bytes",
    );

    let driver = MockDriver::new(vec![make_hierarchy(
        TEST_APP_ID,
        vec![make_node(0, "ok", "OK")],
    )]);
    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), driver));

    let mut cfg = default_config(&ws, Some(build_file));
    cfg.store_id = Some("sim-s".to_string());

    let mut client = McpTestClient::spawn(
        cfg,
        factory,
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );

    // 1. Batch screenshot writes ONLY inside `.deja/reports/stores/sim-s/mcp-screenshots/`!
    let batch_ok = client
        .call_tool(
            json!(1),
            "deja_exec_batch",
            json!({"steps": ["launchApp", {"takeScreenshot": "batch-shot"}]}),
        )
        .await;
    assert_eq!(batch_ok["result"]["isError"], false, "batch_ok: {batch_ok}");
    assert!(
        ws.path
            .join(".deja/reports/stores/sim-s/mcp-screenshots/batch-shot.png")
            .is_file(),
        "screenshot must be written in store-scoped mcp-screenshots/"
    );
    assert!(
        !ws.path.join("batch-shot.png").exists(),
        "screenshot must never write to workspace root"
    );

    // 2. Batch refuses overwriting an existing screenshot file (no-clobber)!
    let batch_clobber = client
        .call_tool(
            json!(2),
            "deja_exec_batch",
            json!({"steps": ["launchApp", {"takeScreenshot": "batch-shot"}]}),
        )
        .await;
    assert_eq!(batch_clobber["result"]["isError"], true);
    assert_eq!(
        batch_clobber["result"]["structuredContent"]["code"],
        "PATH_CONFINEMENT"
    );

    // 3. Batch refuses relative traversal / absolute paths!
    let batch_escape = client
        .call_tool(
            json!(3),
            "deja_exec_batch",
            json!({"steps": ["launchApp", {"takeScreenshot": "../escaped"}]}),
        )
        .await;
    assert_eq!(batch_escape["result"]["isError"], true);
    assert_eq!(
        batch_escape["result"]["structuredContent"]["code"],
        "PATH_CONFINEMENT"
    );

    // 4. File-backed `deja_remember` writes ONLY inside `mcp-screenshots/`!
    let rem_ok = client
        .call_tool(
            json!(4),
            "deja_remember",
            json!({"goal": "Snap Goal", "flow": "flows/snap_good.yaml"}),
        )
        .await;
    assert_eq!(rem_ok["result"]["isError"], false, "rem_ok: {rem_ok}");
    assert!(
        ws.path
            .join(".deja/reports/stores/sim-s/mcp-screenshots/flow-shot.png")
            .is_file()
    );
    assert!(!ws.path.join("flow-shot.png").exists());

    // 5. File-backed `deja_remember` refuses traversal AND refuses overwriting existing file!
    let rem_escape = client
        .call_tool(
            json!(5),
            "deja_remember",
            json!({"goal": "Snap Escape", "flow": "flows/snap_escape.yaml"}),
        )
        .await;
    assert_eq!(rem_escape["result"]["isError"], true);
    assert_eq!(
        rem_escape["result"]["structuredContent"]["code"],
        "PATH_CONFINEMENT"
    );

    let rem_clobber = client
        .call_tool(
            json!(6),
            "deja_remember",
            json!({"goal": "Snap Clobber", "flow": "flows/snap_existing.yaml"}),
        )
        .await;
    assert_eq!(rem_clobber["result"]["isError"], true);
    assert_eq!(
        rem_clobber["result"]["structuredContent"]["code"],
        "PATH_CONFINEMENT"
    );
    assert_eq!(
        fs::read_to_string(
            ws.path
                .join(".deja/reports/stores/sim-s/mcp-screenshots/preexisting.png")
        )
        .unwrap(),
        "original-bytes",
        "existing screenshot file must remain untouched"
    );

    client.close_and_join().await;
}

#[tokio::test]
async fn regression_r1_f4_rejects_device_wide_clear_keychain_in_batch_and_remember() {
    let ws = TempWs::new("r1-f4-keychain");
    let build_file = ws.write_bytes("build/App.bin", b"v1");
    ws.write(
        "flows/clear_kc.yaml",
        "appId: dev.deja.mcp.test\n---\n- clearKeychain\n- launchApp\n",
    );
    ws.write(
        "flows/launch_kc.yaml",
        "appId: dev.deja.mcp.test\n---\n- launchApp:\n    clearKeychain: true\n",
    );
    ws.write(
        "flows/sub_parent.yaml",
        "appId: dev.deja.mcp.test\n---\n- runFlow: clear_kc.yaml\n",
    );

    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), MockDriver::new(vec![])));
    let mut client = McpTestClient::spawn(
        default_config(&ws, Some(build_file)),
        factory,
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );

    for (idx, steps) in [
        json!(["clearKeychain", "launchApp"]),
        json!([{"launchApp": {"clearKeychain": true}}]),
        json!("onFlowStart:\n  - clearKeychain\n---\n- launchApp\n"),
    ]
    .into_iter()
    .enumerate()
    {
        let resp = client
            .call_tool(json!(10 + idx), "deja_exec_batch", json!({"steps": steps}))
            .await;
        assert_eq!(resp["result"]["isError"], true);
        assert_eq!(
            resp["result"]["structuredContent"]["code"], "DEVICE_WIDE_COMMAND_FORBIDDEN",
            "expected DEVICE_WIDE_COMMAND_FORBIDDEN for batch: {resp}"
        );
    }

    for (idx, flow) in [
        "flows/clear_kc.yaml",
        "flows/launch_kc.yaml",
        "flows/sub_parent.yaml",
    ]
    .into_iter()
    .enumerate()
    {
        let resp = client
            .call_tool(
                json!(20 + idx),
                "deja_remember",
                json!({"goal": "kc forbidden", "flow": flow}),
            )
            .await;
        assert_eq!(resp["result"]["isError"], true);
        assert_eq!(
            resp["result"]["structuredContent"]["code"], "DEVICE_WIDE_COMMAND_FORBIDDEN",
            "expected DEVICE_WIDE_COMMAND_FORBIDDEN for {flow}: {resp}"
        );
    }

    client.close_and_join().await;
}

#[tokio::test]
async fn regression_r1_f5_store_isolated_report_inspection_and_persistence() {
    let ws = TempWs::new("r1-f5-store-iso");
    let build_file = ws.write_bytes("build/App.bin", b"v1");
    ws.write(
        "flows/ok.yaml",
        "appId: dev.deja.mcp.test\n---\n- launchApp\n- assertVisible: OK\n",
    );
    ws.write(".deja/reports/latest.json", r#"{"store":"default"}"#);
    ws.write(
        ".deja/reports/stores/store-b/latest.json",
        r#"{"store":"store-b"}"#,
    );

    let driver = MockDriver::new(vec![make_hierarchy(
        TEST_APP_ID,
        vec![make_node(0, "ok", "OK")],
    )]);
    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), driver));

    // 1. Named store `store-a` persists reports under `.deja/reports/stores/store-a/` and cannot
    //    inspect default store or `store-b` reports!
    let mut cfg_a = default_config(&ws, Some(build_file.clone()));
    cfg_a.store_id = Some("store-a".to_string());
    let mut client_a = McpTestClient::spawn(
        cfg_a,
        factory.clone(),
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );

    let rem_a = client_a
        .call_tool(
            json!(1),
            "deja_remember",
            json!({"goal": "Store A Goal", "flow": "flows/ok.yaml"}),
        )
        .await;
    assert_eq!(rem_a["result"]["isError"], false);
    let rep_a = rem_a["result"]["structuredContent"]["reportPath"]
        .as_str()
        .expect("reportPath present");
    assert!(
        rep_a.starts_with(".deja/reports/stores/store-a/"),
        "store-a report must be stored inside store-a directory, got: {rep_a}"
    );

    // store-a CAN inspect its own report
    let own = client_a
        .call_tool(json!(2), "deja_inspect", json!({"report": rep_a}))
        .await;
    assert_eq!(own["result"]["isError"], false);

    // store-a CANNOT inspect default store OR store-b reports
    for (idx, forbidden) in [
        ".deja/reports/latest.json",
        ".deja/reports/stores/store-b/latest.json",
    ]
    .into_iter()
    .enumerate()
    {
        let res = client_a
            .call_tool(
                json!(10 + idx),
                "deja_inspect",
                json!({"report": forbidden}),
            )
            .await;
        assert_eq!(res["result"]["isError"], true);
        assert_eq!(
            res["result"]["structuredContent"]["code"],
            "PATH_CONFINEMENT"
        );
    }
    client_a.close_and_join().await;

    // 2. Default store (`store_id: None`) CAN inspect `.deja/reports/latest.json`, CANNOT inspect
    //    `.deja/reports/stores/store-a/latest.json`!
    let mut client_def = McpTestClient::spawn(
        default_config(&ws, Some(build_file)),
        factory,
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );
    let def_ok = client_def
        .call_tool(
            json!(1),
            "deja_inspect",
            json!({"report": ".deja/reports/latest.json"}),
        )
        .await;
    assert_eq!(def_ok["result"]["isError"], false);

    let def_cross = client_def
        .call_tool(
            json!(2),
            "deja_inspect",
            json!({"report": ".deja/reports/stores/store-a/latest.json"}),
        )
        .await;
    assert_eq!(def_cross["result"]["isError"], true);
    assert_eq!(
        def_cross["result"]["structuredContent"]["code"],
        "PATH_CONFINEMENT"
    );
    client_def.close_and_join().await;
}

#[tokio::test]
async fn regression_r1_f6_redacts_dynamic_json_keys_in_inspected_reports() {
    let ws = TempWs::new("r1-f6-dynkeys");
    let secret = "KeySecretValue88!";
    let provider_tok = ["sk", "-1234567890abcdefghijklmnop"].concat();
    let ws_path_str = ws.path.display().to_string();

    let report_json = json!({
        "summary": "ok",
        "nested": {
            format!("secret_{secret}_key"): "v1",
            format!("path_{ws_path_str}_key"): "v2",
            format!("tok {provider_tok} key"): "v3",
        }
    });
    ws.write(
        ".deja/reports/dyn-keys.json",
        &serde_json::to_string_pretty(&report_json).unwrap(),
    );

    let mut cfg = default_config(&ws, None);
    cfg.forbidden_values = vec![secret.to_string()];
    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), MockDriver::new(vec![])));

    let mut client = McpTestClient::spawn(
        cfg,
        factory,
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );

    let resp = client
        .call_tool(
            json!(1),
            "deja_inspect",
            json!({"report": ".deja/reports/dyn-keys.json"}),
        )
        .await;
    assert_eq!(resp["result"]["isError"], false);

    let structured_str = serde_json::to_string(&resp["result"]["structuredContent"]).unwrap();
    let text_str = resp["result"]["content"][0]["text"].as_str().unwrap();
    for haystack in [structured_str.as_str(), text_str] {
        assert!(!haystack.contains(secret), "secret key leaked: {haystack}");
        assert!(
            !haystack.contains(&ws_path_str),
            "workspace path key leaked: {haystack}"
        );
        assert!(
            !haystack.contains(&provider_tok),
            "provider token key leaked: {haystack}"
        );
    }

    client.close_and_join().await;
}

#[tokio::test]
async fn regression_r1_f7_buildless_batch_rejected_with_build_path_required() {
    let ws = TempWs::new("r1-f7-nobuild");
    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), MockDriver::new(vec![])));

    let mut client = McpTestClient::spawn(
        default_config(&ws, None),
        factory,
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );

    let resp = client
        .call_tool(json!(1), "deja_exec_batch", json!({"steps": ["launchApp"]}))
        .await;
    assert_eq!(resp["result"]["isError"], true);
    assert_eq!(
        resp["result"]["structuredContent"]["code"],
        "BUILD_PATH_REQUIRED"
    );
    client.close_and_join().await;
}

#[tokio::test]
async fn regression_r1_f8_unwritable_reports_omits_report_path_in_response_and_sqlite() {
    let ws = TempWs::new("r1-f8-unwritable");
    let build_file = ws.write_bytes("build/App.bin", b"v1");
    ws.write(
        "flows/ok.yaml",
        "appId: dev.deja.mcp.test\n---\n- launchApp\n- assertVisible: OK\n",
    );
    // Block `.deja/reports` with a regular file so creating `.deja/reports/*.json` fails!
    ws.write(".deja/reports", "not-a-directory");

    let driver = MockDriver::new(vec![make_hierarchy(
        TEST_APP_ID,
        vec![make_node(0, "ok", "OK")],
    )]);
    let factory = Arc::new(MockFactory::new(Some("ios-27.2"), driver));

    let mut client = McpTestClient::spawn(
        default_config(&ws, Some(build_file)),
        factory,
        FramingMode::NewlineDelimited,
        CancellationToken::new(),
    );

    let rem = client
        .call_tool(
            json!(1),
            "deja_remember",
            json!({"goal": "Blocked Report Goal", "flow": "flows/ok.yaml"}),
        )
        .await;
    assert_eq!(rem["result"]["isError"], false, "remember passed: {rem}");
    assert!(
        rem["result"]["structuredContent"]
            .get("reportPath")
            .is_none(),
        "reportPath must be omitted when report write fails: {rem}"
    );
    assert!(
        !rem["result"]["structuredContent"]["warnings"]
            .as_array()
            .unwrap()
            .is_empty(),
        "report write failure must be surfaced in warnings: {rem}"
    );

    let insp = client
        .call_tool(
            json!(2),
            "deja_inspect",
            json!({"goal": "Blocked Report Goal"}),
        )
        .await;
    assert_eq!(insp["result"]["isError"], false);
    let runs = insp["result"]["structuredContent"]["validationRuns"]
        .as_array()
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert!(
        runs[0].get("reportPath").is_none() || runs[0]["reportPath"].is_null(),
        "SQLite validation_runs must not record an unwritten reportPath: {runs:?}"
    );

    client.close_and_join().await;
}
