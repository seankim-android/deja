//! Live headless Chromium E2E test exercising the real `deja` CLI binary end-to-end against
//! the `spikes/002-web-deja` web shop fixture with zero Maestro, Node, Bun, or Playwright.
//!
//! Run explicitly:
//! `cargo test --locked --offline -p deja-cli --test web_cli_e2e -- --ignored --nocapture`

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const SANITIZED_SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'";

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
            "deja-cli-web-e2e-{prefix}-{}-{nanos}",
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

/// Minimal in-process loopback HTTP/1.1 fixture server matching `spikes/002-web-deja/server.py`,
/// with a live-toggleable `order_number` (`WC-1001` vs `WC-9999`).
struct FixtureWebServer {
    port: u16,
    order_number: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl FixtureWebServer {
    fn start(public_root: PathBuf, initial_order: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback HTTP server");
        listener
            .set_nonblocking(true)
            .expect("set nonblocking listener");
        let port = listener.local_addr().expect("local_addr").port();
        let order_number = Arc::new(Mutex::new(initial_order.to_string()));
        let stop = Arc::new(AtomicBool::new(false));

        let order_c = Arc::clone(&order_number);
        let stop_c = Arc::clone(&stop);
        let root = fs::canonicalize(&public_root).expect("canonical public root");
        let handle = thread::spawn(move || {
            while !stop_c.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let root_c = root.clone();
                        let ord = Arc::clone(&order_c);
                        thread::spawn(move || {
                            handle_http_conn(stream, &root_c, &ord);
                        });
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            port,
            order_number,
            stop,
            handle: Some(handle),
        }
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn set_order_number(&self, order: &str) {
        *self.order_number.lock().unwrap() = order.to_string();
    }
}

impl Drop for FixtureWebServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn handle_http_conn(mut stream: TcpStream, root: &Path, order_number: &Arc<Mutex<String>>) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let mut buf = [0u8; 8192];
    let Ok(n) = stream.read(&mut buf) else {
        return;
    };
    if n == 0 {
        return;
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let Some(first_line) = req.lines().next() else {
        return;
    };
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let raw_target = parts.next().unwrap_or("/");
    let send_body = method != "HEAD";
    let pathname = raw_target.split(['?', '#']).next().unwrap_or("/");

    if pathname == "/api/order" {
        let ord = order_number.lock().unwrap().clone();
        let body = format!(r#"{{"orderNumber":"{ord}"}}"#);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(resp.as_bytes());
        if send_body {
            let _ = stream.write_all(body.as_bytes());
        }
        return;
    }

    let rel = if pathname == "/" {
        "index.html"
    } else {
        pathname.trim_start_matches('/')
    };
    let candidate = root.join(rel);
    let Ok(canon) = fs::canonicalize(&candidate) else {
        send_404(&mut stream, send_body);
        return;
    };
    if !canon.starts_with(root) || !canon.is_file() {
        send_404(&mut stream, send_body);
        return;
    }
    let Ok(data) = fs::read(&canon) else {
        send_404(&mut stream, send_body);
        return;
    };
    let content_type = match canon.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nCache-Control: no-store\r\nContent-Security-Policy: {CSP}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        data.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    if send_body {
        let _ = stream.write_all(&data);
    }
}

fn send_404(stream: &mut TcpStream, send_body: bool) {
    let body = b"Not found";
    let resp = format!(
        "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    if send_body {
        let _ = stream.write_all(body);
    }
}

fn run_deja(ws: &Path, tmp_dir: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_deja"));
    cmd.current_dir(ws)
        .args(args)
        .env("PATH", SANITIZED_SYSTEM_PATH)
        .env("TMPDIR", tmp_dir)
        .env_remove("DEJA_MAESTRO")
        .env_remove("DEJA_DEVICE")
        .stdin(Stdio::null());
    if let Ok(chrome) = std::env::var("DEJA_CHROME") {
        cmd.env("DEJA_CHROME", chrome);
    }
    if let Ok(chrome) = std::env::var("CHROME_PATH") {
        cmd.env("CHROME_PATH", chrome);
    }
    cmd.output().expect("spawn deja binary")
}

#[test]
#[ignore = "requires live local Chromium / Google Chrome binary"]
fn live_web_cli_end_to_end_lifecycle_without_maestro_node_or_playwright() {
    // Confirm Maestro, Node, and Playwright are absent on the sanitized PATH.
    for tool in ["maestro", "node", "playwright"] {
        let which = Command::new("/usr/bin/which")
            .arg(tool)
            .env("PATH", SANITIZED_SYSTEM_PATH)
            .output()
            .expect("which");
        assert!(
            !which.status.success(),
            "{tool} must not exist on sanitized PATH"
        );
    }

    let spike_dir = repo_root().join("spikes/002-web-deja");
    let ws = TempDirGuard::new("ws");
    let isolated_tmp = TempDirGuard::new("tmp");

    // Copy public/, flows/checkout.yaml, and deja.config.json into temp workspace.
    let public_dst = ws.path().join("public");
    fs::create_dir_all(&public_dst).unwrap();
    for file in ["index.html", "app.js", "styles.css"] {
        fs::copy(spike_dir.join("public").join(file), public_dst.join(file))
            .expect("copy public fixture file");
    }

    let server = FixtureWebServer::start(public_dst, "WC-1001");
    let old_origin = "127.0.0.1:4174";
    let new_origin = format!("127.0.0.1:{}", server.port());

    let raw_cfg = fs::read_to_string(spike_dir.join("deja.config.json")).unwrap();
    fs::write(
        ws.path().join("deja.config.json"),
        raw_cfg.replace(old_origin, &new_origin),
    )
    .unwrap();

    fs::create_dir_all(ws.path().join("flows")).unwrap();
    let raw_flow = fs::read_to_string(spike_dir.join("flows/checkout.yaml")).unwrap();
    fs::write(
        ws.path().join("flows/checkout.yaml"),
        raw_flow.replace(old_origin, &new_origin),
    )
    .unwrap();
    fs::write(ws.path().join("goals.txt"), "complete checkout\n").unwrap();

    // 1. `deja doctor --json`
    let doc = run_deja(ws.path(), isolated_tmp.path(), &["doctor", "--json"]);
    assert_eq!(
        doc.status.code(),
        Some(0),
        "doctor failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&doc.stdout),
        String::from_utf8_lossy(&doc.stderr)
    );
    let doc_json: Value = serde_json::from_slice(&doc.stdout).expect("doctor JSON");
    assert_eq!(doc_json["ready"], true);
    let web_check = doc_json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "web-chromium")
        .expect("web-chromium doctor check");
    assert_eq!(web_check["status"], "ready");
    eprintln!("LIVE WEB E2E doctor: {web_check}");

    // 2. `deja remember "complete checkout" --flow flows/checkout.yaml --summary --json`
    let rem = run_deja(
        ws.path(),
        isolated_tmp.path(),
        &[
            "remember",
            "complete checkout",
            "--flow",
            "flows/checkout.yaml",
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
    assert_eq!(rem_json["assertions"], 12);
    let runtime = rem_json["runtime"]
        .as_str()
        .expect("remember must record chromium-<version>@<W>x<H> runtime")
        .to_string();
    assert!(
        runtime.starts_with("chromium-") && runtime.ends_with("@1280x800"),
        "expected chromium-<version>@1280x800 runtime, got {runtime}"
    );
    eprintln!(
        "LIVE WEB E2E remember: runtime={runtime}, assertions={}, durationMs={}, timing={}",
        rem_json["assertions"], rem_json["durationMs"], rem_json["timing"]
    );

    // 3. `deja run "complete checkout" --summary --json` -> cache hit, passed, 0 model calls
    let run_ok = run_deja(
        ws.path(),
        isolated_tmp.path(),
        &["run", "complete checkout", "--summary", "--json"],
    );
    assert_eq!(
        run_ok.status.code(),
        Some(0),
        "run pass failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&run_ok.stdout),
        String::from_utf8_lossy(&run_ok.stderr)
    );
    let run_ok_json: Value = serde_json::from_slice(&run_ok.stdout).expect("run pass JSON");
    assert_eq!(run_ok_json["cacheStatus"], "hit");
    assert_eq!(run_ok_json["outcome"], "passed");
    assert_eq!(run_ok_json["modelCalls"], 0);
    eprintln!(
        "LIVE WEB E2E run pass: durationMs={}, timing={}",
        run_ok_json["durationMs"], run_ok_json["timing"]
    );

    // 4. Toggle server to `WC-9999` -> `deja run` exits 1 with `product_assertion`, `repairEligible: false`
    server.set_order_number("WC-9999");
    let run_fail = run_deja(
        ws.path(),
        isolated_tmp.path(),
        &["run", "complete checkout", "--summary", "--json"],
    );
    assert_eq!(
        run_fail.status.code(),
        Some(1),
        "expected exit 1 on WC-9999 regression:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&run_fail.stdout),
        String::from_utf8_lossy(&run_fail.stderr)
    );
    let run_fail_json: Value = serde_json::from_slice(&run_fail.stdout).expect("run fail JSON");
    assert_eq!(run_fail_json["passed"], false);
    assert_eq!(run_fail_json["failure"]["category"], "product_assertion");
    assert_eq!(run_fail_json["failure"]["repairEligible"], false);
    eprintln!(
        "LIVE WEB E2E WC-9999 regression: failure={}",
        run_fail_json["failure"]
    );

    // 5. Restore `WC-1001`: per WALK-010 classic-mode rules, re-remember the known-good YAML to
    // clear failed validation, then `deja run` passes with a warm cache hit.
    server.set_order_number("WC-1001");
    let rem_recover = run_deja(
        ws.path(),
        isolated_tmp.path(),
        &[
            "remember",
            "complete checkout",
            "--flow",
            "flows/checkout.yaml",
            "--summary",
            "--json",
        ],
    );
    assert_eq!(
        rem_recover.status.code(),
        Some(0),
        "recovery remember failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&rem_recover.stdout),
        String::from_utf8_lossy(&rem_recover.stderr)
    );
    let rem_recover_json: Value =
        serde_json::from_slice(&rem_recover.stdout).expect("remember recover JSON");
    assert_eq!(rem_recover_json["outcome"], "remembered");

    let run_recover = run_deja(
        ws.path(),
        isolated_tmp.path(),
        &["run", "complete checkout", "--summary", "--json"],
    );
    assert_eq!(
        run_recover.status.code(),
        Some(0),
        "recovery run failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&run_recover.stdout),
        String::from_utf8_lossy(&run_recover.stderr)
    );
    let run_recover_json: Value =
        serde_json::from_slice(&run_recover.stdout).expect("run recover JSON");
    assert_eq!(run_recover_json["outcome"], "passed");
    assert_eq!(run_recover_json["cacheStatus"], "hit");

    // 6. `deja gate --goals goals.txt --runtime <runtime> --json` -> `verdict: "passed"`
    let ev_path = ws.path().join(".deja/gate/web-e2e-evidence.json");
    let gate = run_deja(
        ws.path(),
        isolated_tmp.path(),
        &[
            "gate",
            "--goals",
            "goals.txt",
            "--runtime",
            &runtime,
            "--label",
            "deja-69-web-e2e",
            "--evidence",
            ev_path.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(
        gate.status.code(),
        Some(0),
        "gate failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&gate.stdout),
        String::from_utf8_lossy(&gate.stderr)
    );
    let gate_json: Value = serde_json::from_slice(&gate.stdout).expect("gate JSON");
    assert_eq!(gate_json["verdict"], "passed");
    assert!(ev_path.is_file());
    assert!(ws.path().join(".deja/gate/web-e2e-evidence.txt").is_file());
    eprintln!("LIVE WEB E2E gate: verdict={}", gate_json["verdict"]);

    // 7. Verify ZERO leftover `deja-web-profile-*` temp dirs and ZERO orphaned Chromium processes.
    let leftover_profiles: Vec<String> = fs::read_dir(isolated_tmp.path())
        .unwrap()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("deja-web-profile-").then_some(name)
        })
        .collect();
    assert!(
        leftover_profiles.is_empty(),
        "leftover Chromium profile dirs in TMPDIR: {leftover_profiles:?}"
    );

    #[cfg(unix)]
    {
        let marker = isolated_tmp.path().to_string_lossy().into_owned();
        let ps = Command::new("/bin/ps")
            .args(["-eo", "pid,args"])
            .output()
            .expect("ps");
        let ps_out = String::from_utf8_lossy(&ps.stdout);
        let orphans: Vec<&str> = ps_out
            .lines()
            .filter(|line| line.contains(&marker))
            .collect();
        assert!(
            orphans.is_empty(),
            "orphaned Chromium processes referencing test TMPDIR survived:\n{}",
            orphans.join("\n")
        );
    }
}

#[tokio::test]
#[ignore = "requires live local Chromium / Google Chrome binary"]
async fn live_web_dom_ancestor_opacity_and_clipped_visible_frame_in_chromium() {
    use deja_driver_web::{ScreenSize, WebDriver, WebDriverConfig};
    use deja_flow::{CompiledSelector, OpenLinkOptions, PlatformDriver, Selector};
    use std::collections::BTreeMap;
    use tokio_util::sync::CancellationToken;

    let ws = TempDirGuard::new("dom-edge");
    let public = ws.path().join("public");
    fs::create_dir_all(&public).unwrap();
    fs::write(
        public.join("index.html"),
        r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <style>
      body { margin: 0; padding: 16px; }
      #opacity_parent { opacity: 0; width: 200px; height: 60px; }
      #opacity_child { opacity: 1; width: 160px; height: 40px; }
      #clip_box { width: 120px; height: 60px; overflow: hidden; margin-top: 20px; }
      #clipped_btn { width: 320px; height: 60px; border: 0; }
      #pay_btn { width: 160px; height: 44px; margin-top: 16px; }
      #border_clip { position: relative; width: 200px; height: 120px; border: 40px solid #333; overflow: hidden; margin-top: 16px; }
      #behind_border { position: absolute; left: -35px; top: 10px; width: 30px; height: 30px; border: 0; }
    </style>
  </head>
  <body>
    <div id="opacity_parent">
      <button id="opacity_child" type="button">Ghost button</button>
    </div>
    <div id="clip_box">
      <button id="clipped_btn" type="button">Wide clipped button</button>
    </div>
    <button id="pay_btn" aria-label="Submit order" type="button">Pay now</button>
    <span id="lbl_ref">Referenced Label</span>
    <button id="labelled_btn" aria-labelledby="lbl_ref" aria-label="Fallback Label" type="button">Btn</button>
    <div id="border_clip">
      <button id="behind_border" type="button">Hidden Behind Border</button>
    </div>
    <fieldset disabled>
      <legend><button id="in_legend" type="button">OK</button></legend>
      <button id="in_body" type="button">No</button>
    </fieldset>
    <select id="opt_select">
      <optgroup disabled>
        <option id="disabled_opt">Locked</option>
      </optgroup>
    </select>
    <p id="click_status">idle</p>
    <script>
      document.getElementById('clipped_btn').addEventListener('click', () => {
        document.getElementById('click_status').textContent = 'clicked';
      });
    </script>
  </body>
</html>
"#,
    )
    .unwrap();

    // Serve without strict CSP so the inline test click handler runs.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let html_bytes = fs::read(public.join("index.html")).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_c = Arc::clone(&stop);
    let srv_handle = thread::spawn(move || {
        while !stop_c.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        html_bytes.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.write_all(&html_bytes);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let mut env = BTreeMap::new();
    env.insert("PATH".to_string(), SANITIZED_SYSTEM_PATH.to_string());
    if let Ok(c) = std::env::var("DEJA_CHROME") {
        env.insert("DEJA_CHROME".to_string(), c);
    }
    if let Ok(c) = std::env::var("CHROME_PATH") {
        env.insert("CHROME_PATH".to_string(), c);
    }
    let driver = WebDriver::new(
        WebDriverConfig::new()
            .with_headless(true)
            .with_screen_size(ScreenSize::new(1280, 720).unwrap())
            .with_env(env),
    );
    let cancel = CancellationToken::new();
    let url = format!("http://127.0.0.1:{port}/");

    driver.ensure_session(&cancel).await.unwrap();
    driver
        .open_link(None, &url, &OpenLinkOptions::default(), &cancel)
        .await
        .unwrap();

    let h = driver.hierarchy(&url, false, None, &cancel).await.unwrap();

    // Finding 7: child inside `opacity: 0` ancestor must be invisible and non-hittable.
    let ghost = h
        .nodes
        .iter()
        .find(|n| n.identifier == "opacity_child")
        .expect("opacity_child node present");
    assert!(
        !ghost.is_on_screen && !ghost.hittable && ghost.visible_frame.is_none(),
        "descendant of opacity:0 ancestor must not be on-screen or hittable: {ghost:#?}"
    );

    // Finding 2: overflow-clipped wide button keeps full unclipped `frame` (~320px wide) while
    // `visible_frame` matches the ~120px visible slice, and tapping `tap_point()` triggers click!
    let clipped = h
        .nodes
        .iter()
        .find(|n| n.identifier == "clipped_btn")
        .expect("clipped_btn node present");
    assert!(clipped.is_on_screen && clipped.hittable);
    assert!(
        clipped.frame.width >= 300.0,
        "full frame width should remain unclipped (~320px), got {}",
        clipped.frame.width
    );
    let vf = clipped
        .visible_frame
        .expect("clipped button must populate visible_frame");
    assert!(
        vf.width <= 125.0 && vf.width >= 100.0,
        "visible_frame width should reflect 120px overflow clip, got {vf:#?}"
    );
    let tap = clipped.tap_point();
    assert!(
        tap.x < clipped.frame.x + 120.0,
        "tap_point ({tap:?}) must lie inside clipped visible region, not unclipped center ({:?})",
        clipped.frame.center()
    );

    driver.tap_point(&url, tap, &cancel).await.unwrap();
    let after = driver.hierarchy(&url, false, None, &cancel).await.unwrap();
    let status = after
        .nodes
        .iter()
        .find(|n| n.identifier == "click_status")
        .expect("click_status node");
    assert_eq!(status.label, "clicked");

    // Codex R2 Finding 6: `<button aria-label="Submit order">Pay now</button>` exposes BOTH
    // `label == "Submit order"` AND `title == "Pay now"` so selectors match either!
    let pay_btn = h
        .nodes
        .iter()
        .find(|n| n.identifier == "pay_btn")
        .expect("pay_btn node present");
    assert_eq!(pay_btn.label, "Submit order");
    assert_eq!(pay_btn.title, "Pay now");
    let match_accessible = CompiledSelector::compile(&Selector::by_text("Submit order"))
        .unwrap()
        .match_one(&h.nodes)
        .expect("by_text(Submit order) must match pay_btn");
    assert_eq!(match_accessible.identifier, "pay_btn");
    let match_visible = CompiledSelector::compile(&Selector::by_text("Pay now"))
        .unwrap()
        .match_one(&h.nodes)
        .expect("by_text(Pay now) must match pay_btn");
    assert_eq!(match_visible.identifier, "pay_btn");

    // Codex R3 Finding 4: overflow container clips descendants to scrollport (excluding 40px
    // border), so a button positioned solely behind the border is NOT on-screen or hittable!
    let behind_border = h
        .nodes
        .iter()
        .find(|n| n.identifier == "behind_border")
        .expect("behind_border node present");
    assert!(
        !behind_border.is_on_screen
            && !behind_border.hittable
            && behind_border.visible_frame.is_none(),
        "element positioned solely behind overflow container border must not be on-screen or hittable: {behind_border:#?}"
    );

    // Codex R3 Finding 5: `aria-labelledby` takes precedence over `aria-label` per W3C AccName.
    let labelled_btn = h
        .nodes
        .iter()
        .find(|n| n.identifier == "labelled_btn")
        .expect("labelled_btn node present");
    assert_eq!(labelled_btn.label, "Referenced Label");
    assert_eq!(labelled_btn.title, "Btn");

    // Codex R3 Finding 7: Chromium native `:disabled` keeps controls inside a disabled
    // `<fieldset>`'s first `<legend>` enabled while disabling controls in its body and `<option>`
    // inside `<optgroup disabled>`.
    let in_legend = h
        .nodes
        .iter()
        .find(|n| n.identifier == "in_legend")
        .expect("in_legend node present");
    let in_body = h
        .nodes
        .iter()
        .find(|n| n.identifier == "in_body")
        .expect("in_body node present");
    let disabled_opt = h
        .nodes
        .iter()
        .find(|n| n.identifier == "disabled_opt")
        .expect("disabled_opt node present");
    assert!(
        in_legend.enabled,
        "button inside disabled fieldset's first <legend> must remain enabled: {in_legend:#?}"
    );
    assert!(
        !in_body.enabled,
        "button inside disabled fieldset body must be disabled: {in_body:#?}"
    );
    assert!(
        !disabled_opt.enabled,
        "option inside <optgroup disabled> must be disabled: {disabled_opt:#?}"
    );

    driver.shutdown().await.unwrap();
    stop.store(true, Ordering::SeqCst);
    let _ = srv_handle.join();
}
