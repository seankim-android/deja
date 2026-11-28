//! Zero-`unsafe` process-group lifecycle, `O_NOFOLLOW` filesystem helpers, per-device
//! advisory locking, and managed `am instrument` supervision.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::error::{AndroidDriverError, Result};

/// Package identifier of the on-device Déjà UiAutomator2 HTTP instrumentation APK.
pub const AGENT_PACKAGE_ID: &str = "dev.deja.agent";

/// Fully-qualified instrumentation component launched via `am instrument`.
pub const AGENT_INSTRUMENTATION_COMPONENT: &str = "dev.deja.agent/.DejaAgentInstrumentation";

/// Default TCP port inside the Android device loopback namespace (`127.0.0.1:7007`).
pub const DEFAULT_DEVICE_PORT: u16 = 7007;

/// Sentinel prefix printed by `DejaAgentInstrumentation` via `sendStatus` once bound.
pub const READY_SENTINEL_PREFIX: &str = "DEJA_ANDROID_AGENT_READY ";

const OUTPUT_TAIL_MAX_BYTES: usize = 32 * 1024;
const OUTPUT_MAX_LINE_BYTES: usize = 16 * 1024;
const DROP_ADB_TIMEOUT: Duration = Duration::from_millis(800);

/// Strictly validates an Android ADB device serial (emulator, USB, TCP/IP, or mDNS serial).
///
/// Rejects empty strings, leading `-` (option injection), whitespace, control/NUL bytes,
/// path separators, and `..` traversal components.
pub fn validate_serial(serial: &str) -> Result<String> {
    let trimmed = serial.trim();
    if trimmed.is_empty()
        || trimmed.len() > 128
        || trimmed.starts_with('-')
        || trimmed.contains("..")
        || trimmed.bytes().any(|b| {
            b == b'\0' || b.is_ascii_control() || b.is_ascii_whitespace() || b == b'/' || b == b'\\'
        })
    {
        return Err(AndroidDriverError::InvalidSerial(serial.to_string()));
    }
    Ok(trimmed.to_string())
}

/// Opens an existing regular file for reading without following symlinks (`O_NOFOLLOW` on Unix).
pub fn open_read_nofollow(path: &Path) -> io::Result<File> {
    let lmeta = fs::symlink_metadata(path)?;
    if lmeta.file_type().is_symlink() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("refusing to follow symbolic link: {}", path.display()),
        ));
    }
    if !lmeta.file_type().is_file() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("expected a regular file: {}", path.display()),
        ));
    }
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts.open(path)?;
    let fmeta = file.metadata()?;
    if !fmeta.is_file() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("expected a regular file: {}", path.display()),
        ));
    }
    Ok(file)
}

/// Opens (or creates) a lock file for read/write without following symlinks (`O_NOFOLLOW` on Unix).
pub(crate) fn open_lockfile_nofollow(path: &Path) -> io::Result<File> {
    if let Ok(lmeta) = fs::symlink_metadata(path) {
        if lmeta.file_type().is_symlink() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("refusing symlink lock file: {}", path.display()),
            ));
        }
        if !lmeta.file_type().is_file() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("lock path is not a regular file: {}", path.display()),
            ));
        }
    }
    let mut opts = OpenOptions::new();
    opts.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
        opts.mode(0o600);
    }
    let file = opts.open(path)?;
    let fmeta = file.metadata()?;
    if !fmeta.is_file() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("lock path is not a regular file: {}", path.display()),
        ));
    }
    Ok(file)
}

/// Exclusive per-device advisory lock held across active Android sessions so concurrent
/// Déjà processes cannot collide on the same Android serial.
#[derive(Debug)]
pub struct DeviceLockGuard {
    file: File,
    lock_path: PathBuf,
    serial: String,
}

impl DeviceLockGuard {
    #[must_use]
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    #[must_use]
    pub fn serial(&self) -> &str {
        &self.serial
    }
}

impl Drop for DeviceLockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Resolves the per-user Android device locks directory independent of `$DEJA_HOME`:
/// `$HOME/.deja/locks/android` (or `$TMPDIR/deja-android-locks` if `HOME` is unset).
pub fn resolve_device_locks_dir(env: &BTreeMap<String, String>) -> Result<PathBuf> {
    let home = env
        .get("HOME")
        .filter(|v| !v.is_empty())
        .cloned()
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .and_then(|v| v.into_string().ok())
                .filter(|v| !v.is_empty())
        });

    if let Some(home_str) = home {
        let home_path = PathBuf::from(home_str);
        let base = if home_path.is_absolute() {
            home_path
        } else {
            std::env::current_dir()
                .map_err(|source| AndroidDriverError::Io {
                    context: "resolve current working directory for HOME".to_string(),
                    source,
                })?
                .join(home_path)
        };
        return Ok(base.join(".deja").join("locks").join("android"));
    }

    Ok(std::env::temp_dir().join("deja-android-locks"))
}

/// Non-blockingly acquires an exclusive per-device advisory lock for `serial`.
pub fn acquire_device_lock(
    serial: &str,
    env: &BTreeMap<String, String>,
) -> Result<DeviceLockGuard> {
    let locks_dir = resolve_device_locks_dir(env)?;
    acquire_device_lock_in_dir(&locks_dir, serial)
}

/// Non-blockingly acquires an exclusive advisory lock at `<locks_dir>/<serial-hash>.lock`
/// using `O_NOFOLLOW` and `0o700` directory permissions on Unix.
pub fn acquire_device_lock_in_dir(locks_dir: &Path, serial: &str) -> Result<DeviceLockGuard> {
    let canonical = validate_serial(serial)?;
    fs::create_dir_all(locks_dir).map_err(|source| AndroidDriverError::Io {
        context: format!("create Android locks dir {}", locks_dir.display()),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(locks_dir, fs::Permissions::from_mode(0o700));
    }

    let lock_name = lock_filename_for_serial(&canonical);
    let lock_path = locks_dir.join(lock_name);
    let file = open_lockfile_nofollow(&lock_path).map_err(|source| AndroidDriverError::Io {
        context: format!("open Android device lock {}", lock_path.display()),
        source,
    })?;

    match file.try_lock() {
        Ok(()) => Ok(DeviceLockGuard {
            file,
            lock_path,
            serial: canonical,
        }),
        Err(TryLockError::WouldBlock) => Err(AndroidDriverError::DeviceBusy { serial: canonical }),
        Err(TryLockError::Error(source)) => Err(AndroidDriverError::Io {
            context: format!("acquire Android device lock {}", lock_path.display()),
            source,
        }),
    }
}

fn lock_filename_for_serial(serial: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(serial.as_bytes());
    let digest = hex::encode(hasher.finalize());
    let safe_prefix: String = serial
        .chars()
        .take(24)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe_prefix}-{}.lock", &digest[..16])
}

/// Configures a `tokio::process::Command` to spawn in its own isolated process group
/// (`pgid == child pid`) without using any `unsafe`.
pub fn configure_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
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
fn signal_process_group_sync(pgid: u32, sig_name: &str) {
    let Some(pgid) = safe_pgid(pgid) else {
        return;
    };
    let _ = std::process::Command::new("/bin/kill")
        .args(["-s", sig_name, "--", &format!("-{pgid}")])
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
        .args(["-s", "0", "--", &format!("-{pgid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Synchronously terminates an isolated child process group via `/bin/kill -s TERM/KILL -- -<pgid>`
/// (zero `unsafe`), reaping the leader and waiting briefly for grandchildren to exit.
pub(crate) fn terminate_child_group_sync(child: &mut Child, pgid: Option<u32>) {
    let mut leader_reaped = child.try_wait().ok().flatten().is_some();

    #[cfg(unix)]
    if let Some(pgid) = pgid.and_then(safe_pgid)
        && (!leader_reaped || process_group_alive_sync(pgid))
    {
        signal_process_group_sync(pgid, "TERM");
        for _ in 0..8 {
            if !leader_reaped && child.try_wait().ok().flatten().is_some() {
                leader_reaped = true;
            }
            if leader_reaped && !process_group_alive_sync(pgid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        if !leader_reaped || process_group_alive_sync(pgid) {
            signal_process_group_sync(pgid, "KILL");
        }
    }

    if !leader_reaped {
        let _ = child.start_kill();
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }
}

/// Runs a synchronous `std::process::Command` with a hard wall-clock deadline (used in `Drop`
/// guards so teardown can never block indefinitely).
pub(crate) fn run_sync_command_bounded(
    mut cmd: std::process::Command,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Parsed ready payload emitted by `DejaAgentInstrumentation` via `sendStatus`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReadySentinel {
    pub port: u16,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub sdk_int: Option<u32>,
    #[serde(default)]
    pub token_digest: Option<String>,
}

/// Parses a `DEJA_ANDROID_AGENT_READY {...}` line from `am instrument -w -r` output.
#[must_use]
pub fn parse_ready_sentinel_line(line: &str) -> Option<ReadySentinel> {
    let idx = line.find(READY_SENTINEL_PREFIX)?;
    let json_part = line[idx + READY_SENTINEL_PREFIX.len()..].trim();
    let parsed: ReadySentinel = serde_json::from_str(json_part).ok()?;
    if parsed.port == 0 {
        return None;
    }
    Some(parsed)
}

/// Managed `adb -s <serial> shell am instrument -w -r ...` child process running in its own
/// isolated process group with continuous output draining and bounded cleanup on `shutdown`/`Drop`.
#[derive(Debug)]
pub struct ManagedInstrumentation {
    child: Child,
    pgid: Option<u32>,
    adb_path: PathBuf,
    serial: String,
    env: BTreeMap<String, String>,
    output_tail: Arc<StdMutex<VecDeque<u8>>>,
    stdout_task: Option<JoinHandle<()>>,
    stderr_task: Option<JoinHandle<()>>,
    ready_rx: watch::Receiver<Option<ReadySentinel>>,
    shut_down: bool,
}

impl ManagedInstrumentation {
    /// Spawns `adb -s <serial> shell am instrument -w -r -e dejaToken <token> -e dejaPort <port> dev.deja.agent/.DejaAgentInstrumentation`.
    pub fn spawn(
        adb_path: PathBuf,
        serial: String,
        token: &str,
        device_port: u16,
        env: BTreeMap<String, String>,
    ) -> Result<Self> {
        let validated_serial = validate_serial(&serial)?;
        let mut cmd = Command::new(&adb_path);
        cmd.args([
            "-s",
            &validated_serial,
            "shell",
            "am",
            "instrument",
            "-w",
            "-r",
            "-e",
            "dejaToken",
            token,
            "-e",
            "dejaPort",
            &device_port.to_string(),
            AGENT_INSTRUMENTATION_COMPONENT,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

        apply_adb_env(&mut cmd, &env);
        configure_process_group(&mut cmd);

        debug!(
            serial = %validated_serial,
            device_port,
            "spawning Android UiAutomator2 HTTP instrumentation"
        );
        let mut child = cmd
            .spawn()
            .map_err(|source| AndroidDriverError::CommandSpawn {
                program: adb_path.display().to_string(),
                source,
            })?;
        let pgid = child.id();

        let output_tail = Arc::new(StdMutex::new(VecDeque::with_capacity(4096)));
        let (ready_tx, ready_rx) = watch::channel::<Option<ReadySentinel>>(None);

        let stdout_task = child.stdout.take().map(|out| {
            let tail = Arc::clone(&output_tail);
            let tx = ready_tx.clone();
            tokio::spawn(async move {
                drain_stream(out, tail, tx).await;
            })
        });
        let stderr_task = child.stderr.take().map(|err| {
            let tail = Arc::clone(&output_tail);
            let tx = ready_tx;
            tokio::spawn(async move {
                drain_stream(err, tail, tx).await;
            })
        });

        Ok(Self {
            child,
            pgid,
            adb_path,
            serial: validated_serial,
            env,
            output_tail,
            stdout_task,
            stderr_task,
            ready_rx,
            shut_down: false,
        })
    }

    pub fn pid(&self) -> Option<u32> {
        self.pgid
    }

    pub fn ready_receiver(&mut self) -> &mut watch::Receiver<Option<ReadySentinel>> {
        &mut self.ready_rx
    }

    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    #[must_use]
    pub fn output_tail_string(&self) -> String {
        let guard = self
            .output_tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let bytes: Vec<u8> = guard.iter().copied().collect();
        String::from_utf8_lossy(&bytes).trim().to_string()
    }

    /// Gracefully terminates the instrumentation `-pgid` and runs `am force-stop dev.deja.agent`.
    pub async fn shutdown(&mut self) {
        if self.shut_down {
            return;
        }
        self.shut_down = true;

        #[cfg(unix)]
        if let Some(pgid) = self.pgid.and_then(safe_pgid) {
            signal_process_group_sync(pgid, "TERM");
            for _ in 0..15 {
                let leader_done = self.child.try_wait().ok().flatten().is_some();
                if leader_done && !process_group_alive_sync(pgid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            if self.child.try_wait().ok().flatten().is_none() || process_group_alive_sync(pgid) {
                signal_process_group_sync(pgid, "KILL");
            }
        }
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_millis(400), self.child.wait()).await;

        if let Some(task) = self.stdout_task.take() {
            task.abort();
            let _ = tokio::time::timeout(Duration::from_millis(100), task).await;
        }
        if let Some(task) = self.stderr_task.take() {
            task.abort();
            let _ = tokio::time::timeout(Duration::from_millis(100), task).await;
        }

        // Best-effort stop on device so no orphan instrumentation stays bound to port 7007.
        let mut stop_cmd = Command::new(&self.adb_path);
        stop_cmd
            .args([
                "-s",
                &self.serial,
                "shell",
                "am",
                "force-stop",
                AGENT_PACKAGE_ID,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        apply_adb_env(&mut stop_cmd, &self.env);
        if let Ok(mut stop_child) = stop_cmd.spawn() {
            let _ = tokio::time::timeout(Duration::from_secs(2), stop_child.wait()).await;
        }
    }
}

impl Drop for ManagedInstrumentation {
    fn drop(&mut self) {
        if self.shut_down {
            return;
        }
        self.shut_down = true;
        if let Some(task) = self.stdout_task.take() {
            task.abort();
        }
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
        terminate_child_group_sync(&mut self.child, self.pgid);

        let mut stop_cmd = std::process::Command::new(&self.adb_path);
        stop_cmd.args([
            "-s",
            &self.serial,
            "shell",
            "am",
            "force-stop",
            AGENT_PACKAGE_ID,
        ]);
        apply_adb_env_std(&mut stop_cmd, &self.env);
        let _ = run_sync_command_bounded(stop_cmd, DROP_ADB_TIMEOUT);
    }
}

pub(crate) fn apply_adb_env(cmd: &mut Command, env: &BTreeMap<String, String>) {
    for key in [
        "PATH",
        "HOME",
        "ANDROID_HOME",
        "ANDROID_SDK_ROOT",
        "ADB_SERVER_SOCKET",
        "ANDROID_ADB_SERVER_PORT",
    ] {
        if let Some(val) = env.get(key) {
            cmd.env(key, val);
        }
    }
}

pub(crate) fn apply_adb_env_std(cmd: &mut std::process::Command, env: &BTreeMap<String, String>) {
    for key in [
        "PATH",
        "HOME",
        "ANDROID_HOME",
        "ANDROID_SDK_ROOT",
        "ADB_SERVER_SOCKET",
        "ANDROID_ADB_SERVER_PORT",
    ] {
        if let Some(val) = env.get(key) {
            cmd.env(key, val);
        }
    }
}

async fn drain_stream<R: AsyncRead + Unpin>(
    stream: R,
    tail: Arc<StdMutex<VecDeque<u8>>>,
    ready_tx: watch::Sender<Option<ReadySentinel>>,
) {
    let mut reader = BufReader::new(stream);
    let mut line_buf = Vec::with_capacity(256);
    loop {
        line_buf.clear();
        let n = match read_bounded_line(&mut reader, &mut line_buf, OUTPUT_MAX_LINE_BYTES).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        push_tail(&tail, &line_buf[..n]);
        let text = String::from_utf8_lossy(&line_buf[..n]);
        if let Some(sentinel) = parse_ready_sentinel_line(&text) {
            let _ = ready_tx.send(Some(sentinel));
        }
    }
}

async fn read_bounded_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    out: &mut Vec<u8>,
    max_bytes: usize,
) -> io::Result<usize> {
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
    let slice = if chunk.len() >= OUTPUT_TAIL_MAX_BYTES {
        guard.clear();
        &chunk[chunk.len() - OUTPUT_TAIL_MAX_BYTES..]
    } else {
        let overflow = guard
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(OUTPUT_TAIL_MAX_BYTES);
        for _ in 0..overflow {
            guard.pop_front();
        }
        chunk
    };
    guard.extend(slice.iter().copied());
}

#[cfg(test)]
mod tests {
    use super::{
        acquire_device_lock_in_dir, open_read_nofollow, parse_ready_sentinel_line, validate_serial,
    };
    use crate::error::AndroidDriverError;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn validate_serial_accepts_standard_and_rejects_unsafe() {
        assert_eq!(validate_serial("emulator-5554").unwrap(), "emulator-5554");
        assert_eq!(validate_serial("R3CR10ABCDE").unwrap(), "R3CR10ABCDE");
        assert_eq!(validate_serial("127.0.0.1:5555").unwrap(), "127.0.0.1:5555");
        assert_eq!(
            validate_serial("adb-XYZ._adb-tls-connect._tcp").unwrap(),
            "adb-XYZ._adb-tls-connect._tcp"
        );

        for bad in [
            "",
            "   ",
            "-s",
            "../etc",
            "a/b",
            "bad serial",
            "a\0b",
            "a\nb",
        ] {
            assert!(
                validate_serial(bad).is_err(),
                "serial {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn parse_ready_sentinel_extracts_port_pid_sdk_and_digest() {
        let line = r#"INSTRUMENTATION_STATUS: stream=DEJA_ANDROID_AGENT_READY {"port":7007,"pid":4242,"sdkInt":35,"tokenDigest":"0123456789abcdef"}"#;
        let parsed = parse_ready_sentinel_line(line).expect("sentinel should parse");
        assert_eq!(parsed.port, 7007);
        assert_eq!(parsed.pid, Some(4242));
        assert_eq!(parsed.sdk_int, Some(35));
        assert_eq!(parsed.token_digest.as_deref(), Some("0123456789abcdef"));

        assert!(parse_ready_sentinel_line(r#"DEJA_ANDROID_AGENT_READY {"port":0}"#).is_none());
        assert!(parse_ready_sentinel_line("unrelated log output").is_none());
    }

    #[test]
    fn device_lock_is_exclusive_and_releases_on_drop() {
        let dir = std::env::temp_dir().join(format!(
            "deja-android-lock-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let guard = acquire_device_lock_in_dir(&dir, "emulator-5554").expect("acquire lock");
        let busy = acquire_device_lock_in_dir(&dir, "emulator-5554")
            .expect_err("second lock on same serial must fail with DeviceBusy");
        assert!(matches!(busy, AndroidDriverError::DeviceBusy { .. }));

        // Distinct serial acquires cleanly.
        let other = acquire_device_lock_in_dir(&dir, "emulator-5556").expect("other serial");
        drop(other);
        drop(guard);

        let reacquired =
            acquire_device_lock_in_dir(&dir, "emulator-5554").expect("reacquire after drop");
        drop(reacquired);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn open_read_nofollow_refuses_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!(
            "deja-android-nofollow-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.apk");
        std::fs::write(&real, b"PK\x03\x04test").unwrap();
        let link = dir.join("link.apk");
        symlink(&real, &link).unwrap();

        assert!(open_read_nofollow(&real).is_ok());
        assert!(open_read_nofollow(&link).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
