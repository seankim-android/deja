//! Chromium binary discovery, version probing, private temp profile management,
//! and zero-`unsafe` process-group lifecycle management.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::error::{Result, WebDriverError};
use crate::input::{ScreenSize, validate_loopback_ws_url};

const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const STDERR_TAIL_MAX_BYTES: usize = 32 * 1024;
const STDERR_MAX_LINE_BYTES: usize = 16 * 1024;
const DEVTOOLS_PREFIX: &str = "DevTools listening on ";
const PATH_BINARY_NAMES: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
];

static PROFILE_SEQ: AtomicU64 = AtomicU64::new(1);

#[cfg(target_os = "macos")]
const OS_CHROMIUM_FALLBACKS: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
    "/opt/homebrew/bin/chromium",
    "/usr/local/bin/chromium",
];

#[cfg(target_os = "linux")]
const OS_CHROMIUM_FALLBACKS: &[&str] = &[
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/snap/bin/chromium",
];

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const OS_CHROMIUM_FALLBACKS: &[&str] = &[];

/// Discovers a Chromium or Google Chrome executable following strict precedence:
///
/// 1. `DEJA_CHROME`, then `CHROME_PATH` in `env` — if explicitly set and non-empty, fails closed
///    when missing or non-executable (never silently falling back).
/// 2. `PATH` from `env` (`chromium`, `chromium-browser`, `google-chrome`, `google-chrome-stable`).
/// 3. OS-specific standard install locations (macOS `/Applications/...`, Homebrew; Linux `/usr/bin/...`, `/snap/bin/...`).
pub fn discover_chromium(env: &BTreeMap<String, String>) -> Result<PathBuf> {
    discover_chromium_with_cwd_and_fallbacks(env, None, OS_CHROMIUM_FALLBACKS)
}

#[cfg(test)]
pub(crate) fn discover_chromium_with_fallbacks(
    env: &BTreeMap<String, String>,
    os_fallbacks: &[&str],
) -> Result<PathBuf> {
    discover_chromium_with_cwd_and_fallbacks(env, None, os_fallbacks)
}

pub(crate) fn discover_chromium_with_cwd_and_fallbacks(
    env: &BTreeMap<String, String>,
    cwd_override: Option<&Path>,
    os_fallbacks: &[&str],
) -> Result<PathBuf> {
    for var in ["DEJA_CHROME", "CHROME_PATH"] {
        if let Some(raw) = env.get(var) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                let raw_path = PathBuf::from(trimmed);
                let candidate = resolve_override_path(var, &raw_path, cwd_override)?;
                validate_executable_override(var, &candidate)?;
                return Ok(candidate);
            }
        }
    }

    if let Some(path_var) = env.get("PATH")
        && !path_var.is_empty()
    {
        for dir in std::env::split_paths(path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            for bin in PATH_BINARY_NAMES {
                let candidate = dir.join(bin);
                if is_executable_file(&candidate) {
                    return Ok(candidate);
                }
            }
        }
    }

    for fallback in os_fallbacks {
        let candidate = PathBuf::from(fallback);
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }

    Err(WebDriverError::ChromiumNotFound(
        "install Chromium or Google Chrome, ensure it is on PATH or in /Applications, or set DEJA_CHROME"
            .to_string(),
    ))
}

fn resolve_override_path(var: &str, raw: &Path, cwd_override: Option<&Path>) -> Result<PathBuf> {
    if raw.is_absolute() {
        return Ok(raw.to_path_buf());
    }
    let base = match cwd_override {
        Some(dir) if dir.is_absolute() => dir.to_path_buf(),
        Some(dir) => {
            let cwd =
                std::env::current_dir().map_err(|err| WebDriverError::InvalidChromiumPath {
                    source_var: var.to_string(),
                    path: raw.to_path_buf(),
                    reason: format!("failed to resolve current working directory: {err}"),
                })?;
            cwd.join(dir)
        }
        None => std::env::current_dir().map_err(|err| WebDriverError::InvalidChromiumPath {
            source_var: var.to_string(),
            path: raw.to_path_buf(),
            reason: format!("failed to resolve current working directory: {err}"),
        })?,
    };
    Ok(base.join(raw))
}

fn validate_executable_override(var: &str, path: &Path) -> Result<()> {
    let meta = fs::metadata(path).map_err(|err| WebDriverError::InvalidChromiumPath {
        source_var: var.to_string(),
        path: path.to_path_buf(),
        reason: err.to_string(),
    })?;
    if !meta.is_file() {
        return Err(WebDriverError::InvalidChromiumPath {
            source_var: var.to_string(),
            path: path.to_path_buf(),
            reason: "path is not a regular file".to_string(),
        });
    }
    if !is_metadata_executable(&meta) {
        return Err(WebDriverError::InvalidChromiumPath {
            source_var: var.to_string(),
            path: path.to_path_buf(),
            reason: "file is not executable".to_string(),
        });
    }
    Ok(())
}

pub(crate) fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && is_metadata_executable(&meta))
}

fn is_metadata_executable(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        true
    }
}

/// Probes `path --version` with a bounded 5-second timeout, cancellation support, and
/// `kill_on_drop(true)`, returning the normalized Chromium version token (e.g. `"158.0.8127.38"`).
pub async fn probe_chromium_version(path: &Path, cancel: &CancellationToken) -> Result<String> {
    if cancel.is_cancelled() {
        return Err(WebDriverError::Cancelled);
    }
    let started = Instant::now();
    let mut cmd = Command::new(path);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().map_err(|source| WebDriverError::BrowserSpawn {
        path: path.to_path_buf(),
        source,
    })?;

    let output = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(WebDriverError::Cancelled),
        timed = tokio::time::timeout(VERSION_PROBE_TIMEOUT, child.wait_with_output()) => {
            match timed {
                Ok(Ok(out)) => out,
                Ok(Err(source)) => {
                    return Err(WebDriverError::Io {
                        context: format!("wait for `{} --version`", path.display()),
                        source,
                    });
                }
                Err(_) => {
                    return Err(WebDriverError::Timeout {
                        operation: "probe_chromium_version".to_string(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        detail: format!(
                            "`{} --version` exceeded {}ms",
                            path.display(),
                            VERSION_PROBE_TIMEOUT.as_millis()
                        ),
                    });
                }
            }
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(WebDriverError::BrowserExitedEarly {
            status: output.status.code(),
            stderr_tail: if stderr.is_empty() {
                "`--version` exited non-zero".to_string()
            } else {
                stderr
            },
        });
    }

    let raw_stdout = String::from_utf8_lossy(&output.stdout);
    let raw_stderr = String::from_utf8_lossy(&output.stderr);
    let source = if raw_stdout.trim().is_empty() {
        raw_stderr.as_ref()
    } else {
        raw_stdout.as_ref()
    };

    parse_chromium_version_output(source).ok_or_else(|| {
        WebDriverError::ChromiumNotFound(format!(
            "`{} --version` produced empty or unparseable output",
            path.display()
        ))
    })
}

pub(crate) fn parse_chromium_version_output(raw: &str) -> Option<String> {
    for token in raw.split_whitespace() {
        let trimmed = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.');
        if trimmed.contains('.')
            && trimmed.bytes().next().is_some_and(|b| b.is_ascii_digit())
            && trimmed.bytes().all(|b| b.is_ascii_digit() || b == b'.')
            && !trimmed.ends_with('.')
        {
            return Some(trimmed.to_string());
        }
    }
    let first_line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let sanitized: String = first_line
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .collect();
    (!sanitized.is_empty()).then_some(sanitized)
}

/// Private (`0o700` on Unix) temporary Chromium user-data-dir that cleans up on `shutdown()` and
/// RAII `Drop`.
#[derive(Debug)]
pub(crate) struct TempProfileDir {
    path: PathBuf,
    cleaned: bool,
}

impl TempProfileDir {
    pub(crate) fn create() -> Result<Self> {
        let seq = PROFILE_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "deja-web-profile-{}-{seq}-{nanos:x}",
            std::process::id()
        ));

        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).map_err(|source| WebDriverError::Io {
            context: format!("create temporary Chromium profile dir {}", path.display()),
            source,
        })?;

        Ok(Self {
            path,
            cleaned: false,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;
        if fs::remove_dir_all(&self.path).is_err() && self.path.exists() {
            // Brief retry in case a dying Chromium helper process unlinked a lockfile concurrently.
            std::thread::sleep(Duration::from_millis(25));
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

impl Drop for TempProfileDir {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Parameters for launching a managed Chromium process.
#[derive(Debug, Clone)]
pub(crate) struct BrowserLaunchOptions {
    pub executable: PathBuf,
    pub headless: bool,
    pub screen_size: ScreenSize,
    pub startup_timeout: Duration,
    pub env: BTreeMap<String, String>,
}

/// Running Chromium child process with isolated process group (`pgid == pid`), continuously
/// drained stderr with a 32 KiB tail ring buffer, and private temp profile directory.
#[derive(Debug)]
pub(crate) struct ManagedBrowser {
    child: Child,
    pgid: Option<u32>,
    ws_url: String,
    stderr_tail: Arc<StdMutex<VecDeque<u8>>>,
    stderr_task: Option<JoinHandle<()>>,
    profile_dir: Option<TempProfileDir>,
    shut_down: bool,
}

impl ManagedBrowser {
    /// Spawns Chromium, waits for `DevTools listening on ws://...`, validates the loopback WS URL,
    /// and keeps draining stderr in the background so Chromium never blocks on a full stderr pipe.
    pub(crate) async fn launch(
        opts: &BrowserLaunchOptions,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        if cancel.is_cancelled() {
            return Err(WebDriverError::Cancelled);
        }

        let profile_dir = TempProfileDir::create()?;
        let started = Instant::now();

        let mut cmd = Command::new(&opts.executable);
        #[cfg(unix)]
        {
            // Zero-`unsafe` isolated process group (pgid == child pid).
            cmd.process_group(0);
        }
        if opts.headless {
            cmd.arg("--headless=new");
        }
        cmd.args([
            "--remote-debugging-port=0",
            "--remote-debugging-address=127.0.0.1",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-background-timer-throttling",
            "--disable-backgrounding-occluded-windows",
            "--disable-component-update",
            "--disable-default-apps",
            "--disable-dev-shm-usage",
            "--disable-extensions",
            "--disable-popup-blocking",
            "--disable-renderer-backgrounding",
            "--disable-sync",
            "--disable-translate",
            "--metrics-recording-only",
            "--mute-audio",
            "--password-store=basic",
            "--use-mock-keychain",
        ]);
        cmd.arg(format!("--user-data-dir={}", profile_dir.path().display()));
        cmd.arg(format!(
            "--window-size={},{}",
            opts.screen_size.width, opts.screen_size.height
        ));
        cmd.arg("about:blank");

        if let Some(path) = opts.env.get("PATH") {
            cmd.env("PATH", path);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|source| WebDriverError::BrowserSpawn {
            path: opts.executable.clone(),
            source,
        })?;
        let pgid = child.id();

        let stderr = child.stderr.take().ok_or_else(|| WebDriverError::Io {
            context: "capture Chromium stderr pipe".to_string(),
            source: std::io::Error::other("stderr pipe unavailable"),
        })?;

        let stderr_tail = Arc::new(StdMutex::new(VecDeque::with_capacity(4096)));
        let (ws_tx, ws_rx) = oneshot::channel::<Result<String>>();
        let tail_clone = Arc::clone(&stderr_tail);
        let stderr_task = tokio::spawn(async move {
            drain_stderr_and_detect_ws(stderr, tail_clone, ws_tx).await;
        });

        let mut browser = Self {
            child,
            pgid,
            ws_url: String::new(),
            stderr_tail,
            stderr_task: Some(stderr_task),
            profile_dir: Some(profile_dir),
            shut_down: false,
        };

        let wait_res: Result<String> = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(WebDriverError::Cancelled),
            timed = tokio::time::timeout(opts.startup_timeout, async {
                tokio::select! {
                    biased;
                    ws_res = ws_rx => match ws_res {
                        Ok(res) => res,
                        Err(_) => {
                            let status = browser.child.try_wait().ok().flatten().and_then(|s| s.code());
                            Err(WebDriverError::BrowserExitedEarly {
                                status,
                                stderr_tail: browser.stderr_tail_string(),
                            })
                        }
                    },
                    exit_res = browser.child.wait() => {
                        let status = exit_res.ok().and_then(|s| s.code());
                        // Give stderr task a brief slice to flush remaining lines into tail buffer.
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Err(WebDriverError::BrowserExitedEarly {
                            status,
                            stderr_tail: browser.stderr_tail_string(),
                        })
                    }
                }
            }) => match timed {
                Ok(inner) => inner,
                Err(_) => Err(WebDriverError::Timeout {
                    operation: "launch_chromium".to_string(),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    detail: format!(
                        "Chromium did not emit a valid DevTools WebSocket URL within {}ms (stderr: {})",
                        opts.startup_timeout.as_millis(),
                        browser.stderr_tail_string()
                    ),
                }),
            },
        };

        match wait_res {
            Ok(ws_url) => {
                debug!(
                    pgid = ?browser.pgid,
                    ws_url = %ws_url,
                    "Chromium DevTools endpoint ready"
                );
                browser.ws_url = ws_url;
                Ok(browser)
            }
            Err(err) => {
                browser.terminate_now().await;
                Err(err)
            }
        }
    }

    pub(crate) fn ws_url(&self) -> &str {
        &self.ws_url
    }

    #[cfg(test)]
    pub(crate) fn profile_path(&self) -> Option<&Path> {
        self.profile_dir.as_ref().map(TempProfileDir::path)
    }

    pub(crate) fn stderr_tail_string(&self) -> String {
        let guard = self
            .stderr_tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let bytes: Vec<u8> = guard.iter().copied().collect();
        String::from_utf8_lossy(&bytes).trim().to_string()
    }

    /// Non-blockingly checks if the Chromium leader process has exited unexpectedly.
    pub(crate) fn try_check_exit(&mut self) -> Option<Option<i32>> {
        self.child.try_wait().ok().flatten().map(|s| s.code())
    }

    /// Waits briefly for graceful exit (after caller sends CDP `Browser.close`), then falls back
    /// to zero-`unsafe` process-group `SIGTERM` -> `SIGKILL`, aborts the stderr drain task, and
    /// removes the private temporary profile directory.
    pub(crate) async fn shutdown_after_cdp_close(&mut self) {
        if self.shut_down {
            return;
        }
        // Give Chromium a brief window to exit cleanly following `Browser.close`.
        let exited = tokio::time::timeout(Duration::from_millis(1_000), self.child.wait())
            .await
            .is_ok();
        if !exited || self.is_group_alive() {
            self.terminate_process_group_async().await;
        }
        self.finish_cleanup().await;
    }

    /// Immediately terminates the browser process group and cleans up profile & background task.
    pub(crate) async fn terminate_now(&mut self) {
        if self.shut_down {
            return;
        }
        self.terminate_process_group_async().await;
        self.finish_cleanup().await;
    }

    fn is_group_alive(&self) -> bool {
        #[cfg(unix)]
        {
            self.pgid
                .and_then(safe_pgid)
                .is_some_and(process_group_alive_sync)
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    async fn terminate_process_group_async(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.and_then(safe_pgid) {
            signal_process_group_sync(pgid, "-TERM");
            for _ in 0..25 {
                let leader_done = self.child.try_wait().ok().flatten().is_some();
                if leader_done && !process_group_alive_sync(pgid) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            signal_process_group_sync(pgid, "-KILL");
        }

        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_millis(500), self.child.wait()).await;
    }

    async fn finish_cleanup(&mut self) {
        self.shut_down = true;
        if let Some(task) = self.stderr_task.take() {
            task.abort();
            let _ = tokio::time::timeout(Duration::from_millis(100), task).await;
        }
        if let Some(mut dir) = self.profile_dir.take() {
            dir.cleanup();
        }
    }
}

impl Drop for ManagedBrowser {
    fn drop(&mut self) {
        if self.shut_down {
            return;
        }
        self.shut_down = true;
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
        let mut leader_reaped = self.child.try_wait().ok().flatten().is_some();
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.and_then(safe_pgid)
            && (!leader_reaped || process_group_alive_sync(pgid))
        {
            signal_process_group_sync(pgid, "-TERM");
            for _ in 0..8 {
                if !leader_reaped && self.child.try_wait().ok().flatten().is_some() {
                    leader_reaped = true;
                }
                if leader_reaped && !process_group_alive_sync(pgid) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            if !leader_reaped || process_group_alive_sync(pgid) {
                signal_process_group_sync(pgid, "-KILL");
            }
        }
        if !leader_reaped {
            let _ = self.child.start_kill();
            let _ = self.child.try_wait();
        }
        if let Some(mut dir) = self.profile_dir.take() {
            dir.cleanup();
        }
    }
}

/// Guards a process-group ID so we NEVER signal init (`0`/`1`), ourselves, or out-of-range PIDs.
#[cfg(unix)]
pub(crate) fn safe_pgid(pgid: u32) -> Option<u32> {
    if pgid <= 1 || pgid == std::process::id() || i32::try_from(pgid).is_err() {
        return None;
    }
    Some(pgid)
}

#[cfg(unix)]
fn signal_process_group_sync(pgid: u32, sig_flag: &str) {
    let Some(pgid) = safe_pgid(pgid) else {
        return;
    };
    let _ = std::process::Command::new("/bin/kill")
        .args([sig_flag, &format!("-{pgid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
fn process_group_alive_sync(pgid: u32) -> bool {
    let Some(pgid) = safe_pgid(pgid) else {
        return false;
    };
    std::process::Command::new("/bin/kill")
        .args(["-0", &format!("-{pgid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Extracts and strictly validates a DevTools WebSocket URL from a Chromium stderr line.
pub(crate) fn extract_devtools_ws_url_from_line(line: &str) -> Option<Result<String>> {
    let idx = line.find(DEVTOOLS_PREFIX)?;
    let rest = line[idx + DEVTOOLS_PREFIX.len()..].trim_start();
    let token = rest.split_whitespace().next().unwrap_or("");
    Some(validate_loopback_ws_url(token))
}

async fn drain_stderr_and_detect_ws<R: AsyncRead + Unpin>(
    stderr: R,
    tail: Arc<StdMutex<VecDeque<u8>>>,
    ws_tx: oneshot::Sender<Result<String>>,
) {
    let mut reader = BufReader::new(stderr);
    let mut ws_tx = Some(ws_tx);
    let mut line_buf: Vec<u8> = Vec::with_capacity(256);

    loop {
        line_buf.clear();
        let bytes_read =
            match read_bounded_line(&mut reader, &mut line_buf, STDERR_MAX_LINE_BYTES).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
        push_tail(&tail, &line_buf[..bytes_read]);

        if ws_tx.is_some() {
            let text = String::from_utf8_lossy(&line_buf[..bytes_read]);
            if let Some(found) = extract_devtools_ws_url_from_line(&text)
                && let Some(tx) = ws_tx.take()
            {
                let _ = tx.send(found);
            }
        }
    }
}

/// Reads up to `max_bytes` until `\n` or EOF without unbounded heap growth.
async fn read_bounded_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    out: &mut Vec<u8>,
    max_bytes: usize,
) -> std::io::Result<usize> {
    let mut total = 0usize;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(total);
        }
        let nl_pos = available.iter().position(|&b| b == b'\n');
        let want = match nl_pos {
            Some(pos) => pos + 1,
            None => available.len(),
        };
        let remaining_cap = max_bytes.saturating_sub(total);
        let copy_len = want.min(remaining_cap);
        out.extend_from_slice(&available[..copy_len]);
        total += copy_len;

        if let Some(pos) = nl_pos {
            reader.consume(pos + 1);
            return Ok(total);
        }
        if total >= max_bytes {
            // Discard remaining bytes of an oversized line up to the next newline.
            let n = available.len();
            reader.consume(n);
            loop {
                let more = reader.fill_buf().await?;
                if more.is_empty() {
                    return Ok(total);
                }
                if let Some(pos) = more.iter().position(|&b| b == b'\n') {
                    reader.consume(pos + 1);
                    return Ok(total);
                }
                let n = more.len();
                reader.consume(n);
            }
        }
        let n = available.len();
        reader.consume(n);
    }
}

fn push_tail(tail: &Arc<StdMutex<VecDeque<u8>>>, chunk: &[u8]) {
    let mut guard = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let slice = if chunk.len() >= STDERR_TAIL_MAX_BYTES {
        guard.clear();
        &chunk[chunk.len() - STDERR_TAIL_MAX_BYTES..]
    } else {
        let overflow = guard
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(STDERR_TAIL_MAX_BYTES);
        for _ in 0..overflow {
            guard.pop_front();
        }
        chunk
    };
    guard.extend(slice.iter().copied());
}
