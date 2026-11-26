use crate::cache::{self, RunnerBuildArtifact};
use crate::client::AgentClient;
use crate::error::{IosDriverError, Result};
use crate::process::{ManagedXcodebuild, configure_process_group, validate_udid};
use crate::simctl::SimctlEnv;
use serde::Deserialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

const READY_SENTINEL_PREFIX: &str = "DEJA_AGENT_READY ";
const MAX_OUTPUT_TAIL_BYTES: usize = 65_536;
static RUN_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct IosRunnerConfig {
    pub udid: String,
    pub deja_home: Option<PathBuf>,
    pub env: SimctlEnv,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
}

impl IosRunnerConfig {
    pub fn new(udid: impl Into<String>) -> Self {
        Self {
            udid: udid.into(),
            deja_home: None,
            env: SimctlEnv::default(),
            startup_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReadyFilePayload {
    port: u16,
    #[serde(default)]
    pid: Option<u32>,
}

#[derive(Debug, Default)]
struct OutputTail {
    buf: String,
}

impl OutputTail {
    fn push_line(&mut self, stream: &str, line: &str) {
        self.buf.push_str(stream);
        self.buf.push_str(": ");
        self.buf.push_str(line);
        self.buf.push('\n');
        if self.buf.len() > MAX_OUTPUT_TAIL_BYTES {
            let mut drop_to = self.buf.len() - MAX_OUTPUT_TAIL_BYTES;
            while !self.buf.is_char_boundary(drop_to) {
                drop_to += 1;
            }
            self.buf.drain(..drop_to);
        }
    }

    fn snapshot(&self) -> String {
        self.buf.clone()
    }
}

/// Active `DejaXCTestRunner` session bound to a specific iOS Simulator UDID.
#[derive(Debug)]
pub struct IosRunnerSession {
    client: AgentClient,
    port: u16,
    agent_pid: Option<u32>,
    udid: String,
    build_artifact: RunnerBuildArtifact,
    attach_duration: Duration,
    managed: ManagedXcodebuild,
}

impl IosRunnerSession {
    /// Builds (or reuses cached) `DejaXCTestRunner`, launches `xcodebuild test-without-building`
    /// inside an isolated process group, waits for port discovery + `/health` 200 OK while racing
    /// child exit and `cancel`, and returns a ready session.
    pub async fn start(config: IosRunnerConfig, cancel: CancellationToken) -> Result<Self> {
        let udid = validate_udid(&config.udid)?;
        if cancel.is_cancelled() {
            return Err(IosDriverError::Cancelled);
        }

        let deja_home = cache::resolve_deja_home(config.deja_home.as_deref())?;
        let build_artifact = tokio::select! {
            _ = cancel.cancelled() => return Err(IosDriverError::Cancelled),
            res = cache::ensure_runner_built(&deja_home, &config.env) => res?,
        };

        let attach_started = Instant::now();
        let token = generate_agent_token()?;
        let run_dir = create_private_run_dir(&deja_home)?;
        let xctestrun_path = run_dir.join("DejaXCTestRunner.xctestrun");
        cache::materialize_run_xctestrun(&build_artifact, &xctestrun_path)?;
        let ready_file = run_dir.join("ready.json");

        let mut cmd = Command::new("xcodebuild");
        cmd.arg("test-without-building")
            .arg("-xctestrun")
            .arg(&xctestrun_path)
            .arg("-destination")
            .arg(format!("id={udid}"))
            .arg("-only-testing:DejaXCTestRunner/DejaXCTestRunnerAgent/testRunAgent")
            .env("TEST_RUNNER_DEJA_IOS_AGENT_TOKEN", &token)
            .env("TEST_RUNNER_DEJA_IOS_AGENT_PORT", "0")
            .env("TEST_RUNNER_DEJA_IOS_AGENT_READY_FILE", &ready_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(ref path) = config.env.env_path {
            cmd.env("PATH", path);
        }
        if let Some(ref dev_dir) = config.env.developer_dir {
            cmd.env("DEVELOPER_DIR", dev_dir);
        }
        configure_process_group(&mut cmd);

        debug!(udid = %udid, run_dir = %run_dir.display(), "spawning xcodebuild test-without-building");
        let mut child = cmd.spawn().map_err(|source| IosDriverError::CommandSpawn {
            program: "xcodebuild".to_string(),
            source,
        })?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let mut managed = ManagedXcodebuild::new(
            child,
            udid.clone(),
            run_dir.clone(),
            config.env.env_path.clone(),
            config.env.developer_dir.clone(),
        );

        let output_tail = Arc::new(Mutex::new(OutputTail::default()));
        let (sentinel_tx, mut sentinel_rx) = watch::channel::<Option<u16>>(None);

        if let Some(out) = stdout {
            let tail = Arc::clone(&output_tail);
            let tx = sentinel_tx.clone();
            tokio::spawn(async move {
                drain_stream(out, "stdout", tail, tx).await;
            });
        }
        if let Some(err) = stderr {
            let tail = Arc::clone(&output_tail);
            let tx = sentinel_tx;
            tokio::spawn(async move {
                drain_stream(err, "stderr", tail, tx).await;
            });
        }

        let startup_deadline = tokio::time::Instant::now() + config.startup_timeout;
        let startup_res = wait_for_agent_ready(
            &mut managed,
            &ready_file,
            &mut sentinel_rx,
            &token,
            config.request_timeout,
            startup_deadline,
            &cancel,
            &output_tail,
        )
        .await;

        let (port, agent_pid, client) = match startup_res {
            Ok(ready) => ready,
            Err(err) => {
                let _ = managed.shutdown().await;
                return Err(err);
            }
        };

        let attach_duration = attach_started.elapsed();
        info!(
            udid = %udid,
            port,
            agent_pid = ?agent_pid,
            attach_ms = attach_duration.as_millis() as u64,
            was_cache_hit = build_artifact.was_cache_hit,
            "DejaXCTestRunner session ready"
        );

        Ok(Self {
            client,
            port,
            agent_pid,
            udid,
            build_artifact,
            attach_duration,
            managed,
        })
    }

    pub fn client(&self) -> &AgentClient {
        &self.client
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn agent_pid(&self) -> Option<u32> {
        self.agent_pid
    }

    pub fn xcodebuild_pid(&self) -> Option<u32> {
        self.managed.pid()
    }

    pub fn udid(&self) -> &str {
        &self.udid
    }

    pub fn build_artifact(&self) -> &RunnerBuildArtifact {
        &self.build_artifact
    }

    pub fn attach_duration(&self) -> Duration {
        self.attach_duration
    }

    /// Gracefully shuts down the `xcodebuild` runner process group, sweeps any stray processes
    /// for this UDID, terminates the simulator runner, and deletes per-run temp artifacts.
    pub async fn shutdown(mut self) -> Result<()> {
        self.managed.shutdown().await
    }
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_agent_ready(
    managed: &mut ManagedXcodebuild,
    ready_file: &Path,
    sentinel_rx: &mut watch::Receiver<Option<u16>>,
    token: &str,
    request_timeout: Duration,
    deadline: tokio::time::Instant,
    cancel: &CancellationToken,
    output_tail: &Arc<Mutex<OutputTail>>,
) -> Result<(u16, Option<u32>, AgentClient)> {
    let mut discovered_port: Option<u16> = None;
    let mut discovered_pid: Option<u32> = None;
    let mut last_health_err: Option<String> = None;
    let mut poll_tick = tokio::time::interval(Duration::from_millis(50));
    poll_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if let Some(port) = discovered_port {
            let probe_timeout = Duration::from_secs(2).min(request_timeout);
            let probe_client = AgentClient::new(port, token.to_string(), probe_timeout)?;

            let health_res = tokio::select! {
                _ = cancel.cancelled() => return Err(IosDriverError::Cancelled),
                _ = tokio::time::sleep_until(deadline) => {
                    let tail = snapshot_tail(output_tail);
                    return Err(IosDriverError::Timeout(format!(
                        "timed out waiting for DejaXCTestRunner /health on port {port} (last_err={last_health_err:?}). Output tail:\n{tail}"
                    )));
                }
                status = wait_child(managed) => {
                    // Brief yield so stdout/stderr drainer flushes final lines.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let tail = snapshot_tail(output_tail);
                    return Err(IosDriverError::AgentExitedEarly {
                        status,
                        output: tail,
                    });
                }
                res = probe_client.health() => res,
            };

            match health_res {
                Ok(health) if health.status == "ok" => {
                    let client = AgentClient::new(port, token.to_string(), request_timeout)?;
                    return Ok((port, Some(health.pid), client));
                }
                Ok(health) => {
                    last_health_err = Some(format!("unexpected status: {}", health.status));
                    debug!(status = %health.status, "unexpected /health status while starting");
                }
                Err(err @ IosDriverError::AgentHttpStatus { .. })
                | Err(err @ IosDriverError::Json { .. }) => {
                    return Err(err);
                }
                Err(err) => {
                    let msg = err.to_string();
                    if last_health_err.as_deref() != Some(msg.as_str()) {
                        tracing::warn!(port, error = %msg, "waiting for DejaXCTestRunner /health");
                        last_health_err = Some(msg);
                    }
                }
            }

            tokio::select! {
                _ = cancel.cancelled() => return Err(IosDriverError::Cancelled),
                _ = tokio::time::sleep_until(deadline) => {
                    let tail = snapshot_tail(output_tail);
                    return Err(IosDriverError::Timeout(format!(
                        "timed out waiting for DejaXCTestRunner /health on port {port} (last_err={last_health_err:?}). Output tail:\n{tail}"
                    )));
                }
                status = wait_child(managed) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let tail = snapshot_tail(output_tail);
                    return Err(IosDriverError::AgentExitedEarly { status, output: tail });
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
            continue;
        }

        // Check ready file first (atomic write, preferred over stderr sentinel).
        if let Some(payload) = read_ready_file(ready_file) {
            discovered_port = Some(payload.port);
            discovered_pid = payload.pid;
            continue;
        }

        // Check stderr/stdout sentinel fallback.
        if let Some(port) = *sentinel_rx.borrow() {
            discovered_port = Some(port);
            continue;
        }

        tokio::select! {
            _ = cancel.cancelled() => return Err(IosDriverError::Cancelled),
            _ = tokio::time::sleep_until(deadline) => {
                let tail = snapshot_tail(output_tail);
                return Err(IosDriverError::Timeout(format!(
                    "timed out waiting for DejaXCTestRunner ready file/sentinel (pid={discovered_pid:?}). Output tail:\n{tail}"
                )));
            }
            status = wait_child(managed) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let tail = snapshot_tail(output_tail);
                return Err(IosDriverError::AgentExitedEarly { status, output: tail });
            }
            changed = sentinel_rx.changed() => {
                if changed.is_ok()
                    && let Some(port) = *sentinel_rx.borrow()
                {
                    discovered_port = Some(port);
                }
            }
            _ = poll_tick.tick() => {}
        }
    }
}

async fn wait_child(managed: &mut ManagedXcodebuild) -> Option<i32> {
    match managed.child_mut() {
        Some(child) => child.wait().await.ok().and_then(|s| s.code()),
        None => None,
    }
}

fn snapshot_tail(tail: &Arc<Mutex<OutputTail>>) -> String {
    tail.lock()
        .map(|guard| guard.snapshot())
        .unwrap_or_default()
}

fn read_ready_file(path: &Path) -> Option<ReadyFilePayload> {
    let bytes = std::fs::read(path).ok()?;
    let payload: ReadyFilePayload = serde_json::from_slice(&bytes).ok()?;
    if payload.port == 0 {
        return None;
    }
    Some(payload)
}

async fn drain_stream<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    label: &str,
    tail: Arc<Mutex<OutputTail>>,
    sentinel_tx: watch::Sender<Option<u16>>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(port) = parse_ready_sentinel_line(&line) {
            let _ = sentinel_tx.send(Some(port));
        }
        if let Ok(mut guard) = tail.lock() {
            guard.push_line(label, &line);
        }
    }
}

/// Parses a `DEJA_AGENT_READY {"port":<u16>}` sentinel line emitted on stderr as fallback.
pub fn parse_ready_sentinel_line(line: &str) -> Option<u16> {
    let idx = line.find(READY_SENTINEL_PREFIX)?;
    let json_part = line[idx + READY_SENTINEL_PREFIX.len()..].trim();
    let payload: ReadyFilePayload = serde_json::from_str(json_part).ok()?;
    if payload.port == 0 {
        return None;
    }
    Some(payload.port)
}

fn create_private_run_dir(deja_home: &Path) -> Result<PathBuf> {
    let runs_root = deja_home.join("ios-runner").join("runs");
    std::fs::create_dir_all(&runs_root).map_err(|source| IosDriverError::Io {
        context: format!("create runs root {}", runs_root.display()),
        source,
    })?;

    let nonce = RUN_NONCE.fetch_add(1, Ordering::Relaxed);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let run_dir = runs_root.join(format!("run-{}-{ts}-{nonce}", std::process::id()));
    std::fs::create_dir(&run_dir).map_err(|source| IosDriverError::Io {
        context: format!("create private run dir {}", run_dir.display()),
        source,
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&run_dir, std::fs::Permissions::from_mode(0o700));
    }

    Ok(run_dir)
}

fn generate_agent_token() -> Result<String> {
    #[cfg(unix)]
    {
        let mut bytes = [0u8; 32];
        let mut urandom =
            std::fs::File::open("/dev/urandom").map_err(|source| IosDriverError::Io {
                context: "open /dev/urandom for agent token".to_string(),
                source,
            })?;
        urandom
            .read_exact(&mut bytes)
            .map_err(|source| IosDriverError::Io {
                context: "read random bytes for agent token".to_string(),
                source,
            })?;
        Ok(hex::encode(bytes))
    }
    #[cfg(not(unix))]
    {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(std::process::id().to_be_bytes());
        hasher.update(RUN_NONCE.fetch_add(1, Ordering::Relaxed).to_be_bytes());
        if let Ok(dur) = SystemTime::now().duration_since(UNIX_EPOCH) {
            hasher.update(dur.as_nanos().to_be_bytes());
        }
        Ok(hex::encode(hasher.finalize()))
    }
}

#[cfg(test)]
mod tests {
    use super::{generate_agent_token, parse_ready_sentinel_line};

    #[test]
    fn parses_ready_sentinel_line() {
        assert_eq!(
            parse_ready_sentinel_line(r#"DEJA_AGENT_READY {"port":51234}"#),
            Some(51234)
        );
        assert_eq!(
            parse_ready_sentinel_line(
                r#"2026-11-26 xcodebuild DEJA_AGENT_READY {"port":49152,"pid":99}"#
            ),
            Some(49152)
        );
        assert_eq!(
            parse_ready_sentinel_line(r#"DEJA_AGENT_READY {"port":0}"#),
            None
        );
        assert_eq!(parse_ready_sentinel_line("unrelated log line"), None);
    }

    #[test]
    fn generates_distinct_64_hex_tokens() {
        let a = generate_agent_token().expect("token a");
        let b = generate_agent_token().expect("token b");
        assert_eq!(a.len(), 64);
        assert_eq!(b.len(), 64);
        assert_ne!(a, b);
    }
}
