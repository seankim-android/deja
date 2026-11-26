use crate::error::{IosDriverError, Result};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tracing::{debug, warn};

/// Bundle identifier of the XCUITest runner bundle installed in the simulator.
pub const RUNNER_BUNDLE_ID: &str = "dev.deja.ios.xctest-runner";
/// Bundle identifier of Xcode's generated XCTRunner wrapper app inside the simulator.
pub const RUNNER_XCTRUNNER_BUNDLE_ID: &str = "dev.deja.ios.xctest-runner.xctrunner";
/// Bundle identifier of the minimal UI test host app installed in the simulator.
pub const RUNNER_HOST_BUNDLE_ID: &str = "dev.deja.ios.runner-host";

const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(1_500);
const SWEEP_POLL_TIMEOUT: Duration = Duration::from_millis(500);
const DROP_SIMCTL_TIMEOUT: Duration = Duration::from_millis(500);

/// Strictly validates a simulator UDID (RFC 4122 UUID `8-4-4-4-12` hex) and returns uppercase.
pub fn validate_udid(udid: &str) -> Result<String> {
    let bytes = udid.as_bytes();
    if bytes.len() != 36 {
        return Err(IosDriverError::InvalidUdid(udid.to_string()));
    }
    for (idx, &b) in bytes.iter().enumerate() {
        let expected_hyphen = matches!(idx, 8 | 13 | 18 | 23);
        if expected_hyphen {
            if b != b'-' {
                return Err(IosDriverError::InvalidUdid(udid.to_string()));
            }
        } else if !b.is_ascii_hexdigit() {
            return Err(IosDriverError::InvalidUdid(udid.to_string()));
        }
    }
    Ok(udid.to_ascii_uppercase())
}

/// Exclusive per-simulator advisory lock held from runner startup through complete teardown
/// (including `simctl terminate`), preventing concurrent sessions from clobbering the same
/// simulator's XCUITest runner process.
#[derive(Debug)]
pub struct SimulatorLockGuard {
    file: File,
    lock_path: PathBuf,
    udid: String,
}

impl SimulatorLockGuard {
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    pub fn udid(&self) -> &str {
        &self.udid
    }
}

impl Drop for SimulatorLockGuard {
    fn drop(&mut self) {
        // Explicitly unlock the open file description before closing so concurrent `fork()`
        // calls in sibling threads cannot delay lock release until `O_CLOEXEC` runs at `exec`.
        let _ = self.file.unlock();
    }
}

/// Resolves the per-user simulator locks directory independent of `$DEJA_HOME`:
/// `$HOME/.deja/locks/ios` (or `$TMPDIR/deja-ios-locks-<uid>` if `HOME` is unset).
pub fn resolve_simulator_locks_dir() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
        && !home.is_empty()
    {
        let home_path = PathBuf::from(home);
        let base = if home_path.is_absolute() {
            home_path
        } else {
            std::env::current_dir()
                .map_err(|source| IosDriverError::Io {
                    context: "resolve current working directory for HOME".to_string(),
                    source,
                })?
                .join(home_path)
        };
        return Ok(base.join(".deja").join("locks").join("ios"));
    }

    Ok(std::env::temp_dir().join(format!("deja-ios-locks-{}", current_uid_suffix())))
}

fn current_uid_suffix() -> String {
    #[cfg(unix)]
    {
        unix_signals::current_euid().to_string()
    }
    #[cfg(not(unix))]
    {
        "user".to_string()
    }
}

/// Non-blockingly acquires an exclusive per-user advisory lock for `udid` under
/// `resolve_simulator_locks_dir()` (independent of `$DEJA_HOME`).
/// Returns `IosDriverError::SimulatorBusy { udid }` if another session already owns the simulator.
pub fn acquire_simulator_lock(udid: &str) -> Result<SimulatorLockGuard> {
    let locks_dir = resolve_simulator_locks_dir()?;
    acquire_simulator_lock_in_dir(&locks_dir, udid)
}

/// Non-blockingly acquires an exclusive advisory lock at `<locks_dir>/<udid>.lock` with mode
/// `0o700` on `locks_dir` (Unix).
pub fn acquire_simulator_lock_in_dir(locks_dir: &Path, udid: &str) -> Result<SimulatorLockGuard> {
    let canonical = validate_udid(udid)?;
    std::fs::create_dir_all(locks_dir).map_err(|source| IosDriverError::Io {
        context: format!("create simulator locks dir {}", locks_dir.display()),
        source,
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(locks_dir, std::fs::Permissions::from_mode(0o700));
    }

    let lock_path = locks_dir.join(format!("{canonical}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| IosDriverError::Io {
            context: format!("open simulator lock {}", lock_path.display()),
            source,
        })?;

    match file.try_lock() {
        Ok(()) => Ok(SimulatorLockGuard {
            file,
            lock_path,
            udid: canonical,
        }),
        Err(TryLockError::WouldBlock) => Err(IosDriverError::SimulatorBusy { udid: canonical }),
        Err(TryLockError::Error(source)) => Err(IosDriverError::Io {
            context: format!("acquire simulator lock {}", lock_path.display()),
            source,
        }),
    }
}

/// Configures a `tokio::process::Command` to spawn in its own process group (PGID == child PID)
/// without using any `unsafe` `pre_exec` hooks.
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

#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) mod unix_signals {
    use std::io;

    /// Sends `sig` to process group `pgid`.
    /// Rejects `pgid <= 1`, the current process PID, and the current process group ID.
    pub(crate) fn kill_process_group(pgid: i32, sig: i32) -> io::Result<()> {
        let self_pid = std::process::id() as i32;
        // SAFETY: `libc::getpgrp` takes no arguments, has no preconditions, and always succeeds.
        let self_pgid = unsafe { libc::getpgrp() };
        if pgid <= 1 || pgid == self_pid || pgid == self_pgid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing to signal unsafe pgid {pgid}"),
            ));
        }

        // SAFETY: `pgid` is validated to be > 1 and distinct from both the current process PID
        // and the current process group ID. `libc::killpg` is thread-safe and delivers `sig`
        // solely to members of that isolated child process group.
        let rc = unsafe { libc::killpg(pgid, sig) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        Err(err)
    }

    /// Returns `true` if any process in process group `pgid` is still alive (`killpg(pgid, 0)`
    /// does not fail with `ESRCH`).
    pub(crate) fn process_group_alive(pgid: i32) -> bool {
        if pgid <= 1 {
            return false;
        }
        // SAFETY: Signal 0 checks only whether any process exists in `pgid` without delivering
        // any signal.
        let rc = unsafe { libc::killpg(pgid, 0) };
        if rc == 0 {
            return true;
        }
        io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    /// Sends `sig` to a single process `pid`.
    /// Rejects `pid <= 1` and the current process PID.
    pub(crate) fn kill_pid(pid: i32, sig: i32) -> io::Result<()> {
        let self_pid = std::process::id() as i32;
        if pid <= 1 || pid == self_pid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing to signal unsafe pid {pid}"),
            ));
        }

        // SAFETY: `pid` is validated to be > 1 and distinct from the current process PID.
        // `libc::kill` is thread-safe and only signals that single process.
        let rc = unsafe { libc::kill(pid, sig) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        Err(err)
    }

    #[cfg(test)]
    pub(crate) fn is_pid_alive(pid: u32) -> bool {
        let Ok(pid_i32) = i32::try_from(pid) else {
            return false;
        };
        if pid_i32 <= 1 {
            return false;
        }
        // SAFETY: Signal 0 performs only existence/permission checking without delivering a signal.
        let rc = unsafe { libc::kill(pid_i32, 0) };
        if rc == 0 {
            return true;
        }
        io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    pub(crate) fn current_euid() -> u32 {
        // SAFETY: `libc::geteuid` takes no arguments, has no preconditions, and always succeeds.
        unsafe { libc::geteuid() }
    }

    pub(crate) const SIGTERM: i32 = libc::SIGTERM;
    pub(crate) const SIGKILL: i32 = libc::SIGKILL;
}

fn is_group_alive(pgid: Option<i32>) -> bool {
    #[cfg(unix)]
    {
        pgid.is_some_and(unix_signals::process_group_alive)
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        false
    }
}

/// Synchronously terminates an isolated child process group (`SIGTERM` -> short poll -> `SIGKILL`),
/// reaping the leader AND waiting for the entire process group to reach `ESRCH` so grandchildren
/// that trap `SIGTERM` cannot survive after the leader exits.
pub(crate) fn terminate_child_group_sync(child: &mut Child, pgid: Option<i32>) {
    let mut leader_reaped = child.try_wait().ok().flatten().is_some();
    if leader_reaped && !is_group_alive(pgid) {
        return;
    }

    #[cfg(unix)]
    if let Some(pgid) = pgid {
        let _ = unix_signals::kill_process_group(pgid, unix_signals::SIGTERM);
        for _ in 0..8 {
            if !leader_reaped && child.try_wait().ok().flatten().is_some() {
                leader_reaped = true;
            }
            if leader_reaped && !unix_signals::process_group_alive(pgid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = unix_signals::kill_process_group(pgid, unix_signals::SIGKILL);
    }

    if !leader_reaped {
        let _ = child.start_kill();
    }

    let reap_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < reap_deadline {
        if !leader_reaped && child.try_wait().ok().flatten().is_some() {
            leader_reaped = true;
        }
        if leader_reaped && !is_group_alive(pgid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// Runs a synchronous `std::process::Command` with a hard wall-clock deadline, polling
/// `try_wait()` and forcefully killing + reaping the child if it exceeds `timeout`.
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

/// Runs an async `tokio::process::Command` with `.kill_on_drop(true)` and a hard wall-clock
/// deadline, explicitly calling `child.kill().await` + bounded `child.wait().await` on timeout.
pub(crate) async fn run_async_command_bounded(
    mut cmd: Command,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn().ok()?;
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(_)) | Err(_) => {
            let _ = child.kill().await;
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            None
        }
    }
}

/// Sends `SIGTERM` to a single child process PID (Unix only).
pub fn send_sigterm_to_pid(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let pid_i32 = i32::try_from(pid)
            .map_err(|_| IosDriverError::InvalidArgument(format!("pid {pid} exceeds i32::MAX")))?;
        unix_signals::kill_pid(pid_i32, unix_signals::SIGTERM).map_err(|source| {
            IosDriverError::Io {
                context: format!("send SIGTERM to pid {pid}"),
                source,
            }
        })
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        Err(IosDriverError::UnsupportedPlatform(
            "SIGTERM is only supported on Unix".to_string(),
        ))
    }
}

/// Joins `ps -e -o pid=,comm=` (kernel executable path/basename) with `ps -e -o pid=,args=` by PID
/// and returns PIDs matching ONLY:
/// - kernel `comm` whose `Path::file_name()` is literally `"xcodebuild"` (never matching wrapper
///   processes like `python3` or `sh` whose arguments merely mention `xcodebuild`), AND
/// - `test-without-building` token in `args`, AND
/// - exact case-insensitive `<udid>` token (standalone or `id=<udid>`), AND
/// - when `xctestrun_path` is `Some`, the exact per-run `.xctestrun` path in `args` so concurrent
///   sessions targeting the same simulator are never matched.
///
/// Never matches `self_pid`, PID <= 1, unrelated simulators, or `build-for-testing`.
pub fn parse_ps_matching_xcodebuild_pids(
    comm_output: &str,
    args_output: &str,
    udid: &str,
    xctestrun_path: Option<&Path>,
    self_pid: u32,
) -> Vec<u32> {
    let Ok(canonical_udid) = validate_udid(udid) else {
        return Vec::new();
    };
    let udid_lower = canonical_udid.to_ascii_lowercase();
    let xctestrun_candidates = build_path_candidates(xctestrun_path);
    let comm_by_pid = parse_ps_pid_rest(comm_output);

    let mut matched = Vec::new();
    for raw_line in args_output.lines() {
        let Some((pid, args_str)) = split_ps_pid_rest(raw_line) else {
            continue;
        };
        if pid <= 1 || pid == self_pid {
            continue;
        }

        let Some(comm) = comm_by_pid.get(&pid) else {
            continue;
        };
        if Path::new(comm).file_name() != Some(OsStr::new("xcodebuild")) {
            continue;
        }

        let tokens: Vec<&str> = args_str.split_whitespace().collect();
        let has_test_without_building = tokens.contains(&"test-without-building");
        let has_exact_udid = tokens
            .iter()
            .any(|tok| token_matches_udid(tok, &udid_lower));
        if !(has_test_without_building && has_exact_udid) {
            continue;
        }

        if let Some(ref candidates) = xctestrun_candidates
            && !candidates
                .iter()
                .any(|needle| args_contain_path_token(args_str, needle))
        {
            continue;
        }

        matched.push(pid);
    }
    matched
}

fn parse_ps_pid_rest(output: &str) -> HashMap<u32, String> {
    let mut map = HashMap::new();
    for line in output.lines() {
        if let Some((pid, rest)) = split_ps_pid_rest(line) {
            map.insert(pid, rest.to_string());
        }
    }
    map
}

fn split_ps_pid_rest(line: &str) -> Option<(u32, &str)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let pid = trimmed[..i].parse::<u32>().ok()?;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    Some((pid, trimmed[i..].trim()))
}

fn build_path_candidates(path: Option<&Path>) -> Option<Vec<String>> {
    let path = path?;
    let mut out = Vec::new();
    if let Some(s) = path.to_str()
        && !s.is_empty()
    {
        out.push(s.to_string());
    }
    if let Ok(canon) = std::fs::canonicalize(path)
        && let Some(s) = canon.to_str()
        && !out.iter().any(|existing| existing == s)
    {
        out.push(s.to_string());
    }
    Some(out)
}

fn args_contain_path_token(args_str: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut start = 0usize;
    while let Some(rel) = args_str[start..].find(needle) {
        let idx = start + rel;
        let end = idx + needle.len();
        let before_ok = idx == 0
            || matches!(
                args_str.as_bytes()[idx - 1],
                b' ' | b'\t' | b'=' | b'"' | b'\''
            );
        let after_ok = end == args_str.len()
            || matches!(args_str.as_bytes()[end], b' ' | b'\t' | b'"' | b'\'');
        if before_ok && after_ok {
            return true;
        }
        start = idx + 1;
    }
    false
}

fn token_matches_udid(token: &str, udid_lower: &str) -> bool {
    // Handles `id=<UDID>`, `platform=iOS Simulator,id=<UDID>`, or standalone `<UDID>`.
    for segment in token.split(',') {
        let candidate = segment
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .strip_prefix("id=")
            .unwrap_or_else(|| segment.trim().trim_matches(|c| c == '"' || c == '\''));
        if candidate.eq_ignore_ascii_case(udid_lower) {
            return true;
        }
    }
    false
}

fn run_ps(columns: &str) -> Result<String> {
    let output = std::process::Command::new("ps")
        .args(["-e", "-o", columns])
        .stdin(Stdio::null())
        .output()
        .map_err(|source| IosDriverError::CommandSpawn {
            program: "ps".to_string(),
            source,
        })?;
    if !output.status.success() {
        return Err(IosDriverError::CommandFailed {
            program: "ps".to_string(),
            args: format!("-e -o {columns}"),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn read_ps_comm_and_args() -> Result<(String, String)> {
    let comm_output = run_ps("pid=,comm=")?;
    let args_output = run_ps("pid=,args=")?;
    Ok((comm_output, args_output))
}

/// Lists all live `xcodebuild test-without-building` PIDs targeting `udid`.
pub fn find_xcodebuild_test_pids_for_udid(udid: &str) -> Result<Vec<u32>> {
    let canonical = validate_udid(udid)?;
    let (comm_output, args_output) = read_ps_comm_and_args()?;
    Ok(parse_ps_matching_xcodebuild_pids(
        &comm_output,
        &args_output,
        &canonical,
        None,
        std::process::id(),
    ))
}

/// Lists live `xcodebuild test-without-building` PIDs targeting `udid` with a specific session's
/// `.xctestrun` path.
pub fn find_xcodebuild_test_pids_for_session(
    udid: &str,
    xctestrun_path: &Path,
) -> Result<Vec<u32>> {
    let canonical = validate_udid(udid)?;
    let (comm_output, args_output) = read_ps_comm_and_args()?;
    Ok(parse_ps_matching_xcodebuild_pids(
        &comm_output,
        &args_output,
        &canonical,
        Some(xctestrun_path),
        std::process::id(),
    ))
}

/// Sweeps any remaining `xcodebuild test-without-building` processes belonging to this exact
/// session (`udid` + unique per-run `xctestrun_path`) (DEJA-54).
/// Sends `SIGTERM`, polls briefly, then `SIGKILL`s any survivors.
pub async fn sweep_xcodebuild_for_session(udid: &str, xctestrun_path: &Path) -> Result<usize> {
    let canonical = validate_udid(udid)?;
    let pids = find_xcodebuild_test_pids_for_session(&canonical, xctestrun_path)?;
    if pids.is_empty() {
        return Ok(0);
    }

    #[cfg(unix)]
    {
        let mut signaled = 0usize;
        for &pid in &pids {
            if let Ok(pid_i32) = i32::try_from(pid)
                && unix_signals::kill_pid(pid_i32, unix_signals::SIGTERM).is_ok()
            {
                signaled += 1;
            }
        }

        let deadline = tokio::time::Instant::now() + SWEEP_POLL_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            let remaining = find_xcodebuild_test_pids_for_session(&canonical, xctestrun_path)?;
            if remaining.is_empty() {
                return Ok(signaled);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let survivors = find_xcodebuild_test_pids_for_session(&canonical, xctestrun_path)?;
        for pid in survivors {
            if let Ok(pid_i32) = i32::try_from(pid) {
                let _ = unix_signals::kill_pid(pid_i32, unix_signals::SIGKILL);
            }
        }
        Ok(signaled)
    }

    #[cfg(not(unix))]
    {
        Ok(pids.len())
    }
}

/// Owns a spawned `xcodebuild test-without-building` process group and the exclusive per-simulator
/// lock, guaranteeing complete cleanup (leader + full process group + session sweep + bounded
/// `simctl terminate` + `run_dir` removal) before releasing the simulator lock on both explicit
/// async `shutdown()` and synchronous `Drop`.
#[derive(Debug)]
pub struct ManagedXcodebuild {
    child: Option<Child>,
    pgid: Option<i32>,
    udid: String,
    run_dir: PathBuf,
    xctestrun_path: PathBuf,
    env_path: Option<OsString>,
    developer_dir: Option<OsString>,
    shut_down: bool,
    // Declared LAST so even on unexpected Drop it releases only after all cleanup completes.
    sim_lock: Option<SimulatorLockGuard>,
}

impl ManagedXcodebuild {
    pub fn new(
        child: Child,
        udid: String,
        run_dir: PathBuf,
        xctestrun_path: PathBuf,
        env_path: Option<OsString>,
        developer_dir: Option<OsString>,
        sim_lock: Option<SimulatorLockGuard>,
    ) -> Self {
        let pgid = child.id().and_then(|id| i32::try_from(id).ok());
        Self {
            child: Some(child),
            pgid,
            udid,
            run_dir,
            xctestrun_path,
            env_path,
            developer_dir,
            shut_down: false,
            sim_lock,
        }
    }

    pub fn child_mut(&mut self) -> Option<&mut Child> {
        self.child.as_mut()
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    /// Gracefully terminates the entire `xcodebuild` process group (`SIGTERM` -> 1500ms ->
    /// `SIGKILL` if leader OR any process-group member remains alive), sweeps any leftover
    /// `xcodebuild test-without-building` processes for this session, runs bounded `simctl
    /// terminate`, deletes the per-run directory, and releases the simulator lock last.
    ///
    /// Keeps `self.child`, `self.shut_down == false`, and `self.sim_lock` armed across all
    /// `.await` points so cancelling/dropping `shutdown()` mid-flight still triggers full `Drop`
    /// escalation.
    pub async fn shutdown(&mut self) -> Result<()> {
        if self.shut_down {
            return Ok(());
        }

        let mut leader_reaped = self
            .child
            .as_mut()
            .is_none_or(|c| c.try_wait().ok().flatten().is_some());

        if !leader_reaped || is_group_alive(self.pgid) {
            #[cfg(unix)]
            if let Some(pgid) = self.pgid {
                debug!(pgid, udid = %self.udid, "sending SIGTERM to xcodebuild process group");
                if let Err(err) = unix_signals::kill_process_group(pgid, unix_signals::SIGTERM) {
                    warn!(pgid, %err, "failed to send SIGTERM to xcodebuild process group");
                }
            }

            let deadline = tokio::time::Instant::now() + GRACEFUL_SHUTDOWN_TIMEOUT;

            if !leader_reaped
                && let Some(child) = self.child.as_mut()
                && tokio::time::timeout_at(deadline, child.wait())
                    .await
                    .is_ok()
            {
                leader_reaped = true;
            }

            // Even after the leader exits, keep polling until all process-group members exit
            // or the graceful deadline expires.
            while is_group_alive(self.pgid) && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }

            if !leader_reaped || is_group_alive(self.pgid) {
                #[cfg(unix)]
                if let Some(pgid) = self.pgid {
                    warn!(
                        pgid,
                        udid = %self.udid,
                        "xcodebuild process group still alive after SIGTERM; sending SIGKILL"
                    );
                    let _ = unix_signals::kill_process_group(pgid, unix_signals::SIGKILL);
                }

                if !leader_reaped && let Some(child) = self.child.as_mut() {
                    let _ = child.kill().await;
                    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                }

                let kill_deadline = tokio::time::Instant::now() + Duration::from_millis(500);
                while is_group_alive(self.pgid) && tokio::time::Instant::now() < kill_deadline {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
        self.child = None;

        if let Err(err) = sweep_xcodebuild_for_session(&self.udid, &self.xctestrun_path).await {
            warn!(udid = %self.udid, %err, "session xcodebuild sweep encountered error");
        }

        // Best-effort bounded terminate of XCTRunner wrapper, test bundle, and host app.
        for bundle_id in [
            RUNNER_XCTRUNNER_BUNDLE_ID,
            RUNNER_BUNDLE_ID,
            RUNNER_HOST_BUNDLE_ID,
        ] {
            let mut cmd = Command::new("xcrun");
            cmd.args(["simctl", "terminate", &self.udid, bundle_id]);
            if let Some(ref path) = self.env_path {
                cmd.env("PATH", path);
            }
            if let Some(ref dev_dir) = self.developer_dir {
                cmd.env("DEVELOPER_DIR", dev_dir);
            }
            let _ = run_async_command_bounded(cmd, Duration::from_secs(5)).await;
        }

        if self.run_dir.exists() {
            let _ = tokio::fs::remove_dir_all(&self.run_dir).await;
        }

        self.shut_down = true;
        self.sim_lock = None;
        Ok(())
    }
}

impl Drop for ManagedXcodebuild {
    fn drop(&mut self) {
        if self.shut_down {
            return;
        }
        self.shut_down = true;

        if let Some(mut child) = self.child.take() {
            terminate_child_group_sync(&mut child, self.pgid);
        } else if is_group_alive(self.pgid) {
            #[cfg(unix)]
            if let Some(pgid) = self.pgid {
                let _ = unix_signals::kill_process_group(pgid, unix_signals::SIGKILL);
                let deadline = Instant::now() + Duration::from_millis(500);
                while unix_signals::process_group_alive(pgid) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(15));
                }
            }
        }

        #[cfg(unix)]
        if let Ok(pids) = find_xcodebuild_test_pids_for_session(&self.udid, &self.xctestrun_path) {
            for pid in pids {
                if let Ok(pid_i32) = i32::try_from(pid) {
                    let _ = unix_signals::kill_pid(pid_i32, unix_signals::SIGKILL);
                }
            }
        }

        for bundle_id in [
            RUNNER_XCTRUNNER_BUNDLE_ID,
            RUNNER_BUNDLE_ID,
            RUNNER_HOST_BUNDLE_ID,
        ] {
            let mut cmd = std::process::Command::new("xcrun");
            cmd.args(["simctl", "terminate", &self.udid, bundle_id]);
            if let Some(ref path) = self.env_path {
                cmd.env("PATH", path);
            }
            if let Some(ref dev_dir) = self.developer_dir {
                cmd.env("DEVELOPER_DIR", dev_dir);
            }
            let _ = run_sync_command_bounded(cmd, DROP_SIMCTL_TIMEOUT);
        }

        if self.run_dir.exists() {
            let _ = std::fs::remove_dir_all(&self.run_dir);
        }

        // Release simulator ownership lock only after complete teardown.
        self.sim_lock = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ManagedXcodebuild, acquire_simulator_lock, acquire_simulator_lock_in_dir,
        configure_process_group, parse_ps_matching_xcodebuild_pids, resolve_simulator_locks_dir,
        run_async_command_bounded, run_sync_command_bounded, terminate_child_group_sync,
        validate_udid,
    };
    use crate::cache::resolve_deja_home;
    use crate::error::IosDriverError;
    use std::ffi::OsString;
    use std::path::Path;
    use std::process::Stdio;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const TARGET_UDID: &str = "BA555783-AC7E-46E3-B749-34F9499B2C2B";
    const OTHER_UDID: &str = "DA910389-DF0B-4254-BB78-7164E0E27609";
    const SYNTHETIC_CROSS_HOME_UDID: &str = "11111111-2222-4333-8444-555555555555";

    #[test]
    fn validate_udid_accepts_and_uppercases_valid_uuid() {
        assert_eq!(
            validate_udid("ba555783-ac7e-46e3-b749-34f9499b2c2b").unwrap(),
            TARGET_UDID
        );
    }

    #[test]
    fn validate_udid_rejects_malformed_and_injection_inputs() {
        for bad in [
            "",
            "not-a-uuid",
            "BA555783-AC7E-46E3-B749-34F9499B2C2",
            "BA555783-AC7E-46E3-B749-34F9499B2C2BB",
            "BA555783-AC7E-46E3-B749-34F9499B2C2G",
            "-BA555783-AC7E-46E3-B749-34F9499B2C2B",
            "../BA555783-AC7E-46E3-B749-34F9499B2C2B",
            "BA555783_AC7E_46E3_B749_34F9499B2C2B",
        ] {
            assert!(validate_udid(bad).is_err(), "expected error for `{bad}`");
        }
    }

    #[test]
    fn simulator_lock_rejects_second_concurrent_holder_until_released() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let locks_dir =
            std::env::temp_dir().join(format!("deja-sim-lock-test-{}-{nanos}", std::process::id()));

        let owner =
            acquire_simulator_lock_in_dir(&locks_dir, TARGET_UDID).expect("first lock succeeds");
        assert_eq!(owner.udid(), TARGET_UDID);
        assert!(owner.lock_path().is_file());

        // Second attempt on the same simulator UDID must fail with SimulatorBusy.
        match acquire_simulator_lock_in_dir(&locks_dir, &TARGET_UDID.to_ascii_lowercase()) {
            Err(IosDriverError::SimulatorBusy { udid }) => assert_eq!(udid, TARGET_UDID),
            other => panic!("expected SimulatorBusy for {TARGET_UDID}, got {other:?}"),
        }

        // A different simulator UDID remains unaffected and lockable concurrently.
        let other_owner = acquire_simulator_lock_in_dir(&locks_dir, OTHER_UDID)
            .expect("different simulator should lock independently");
        drop(other_owner);

        // After releasing owner, the target simulator can be locked again.
        drop(owner);
        let reacquired = acquire_simulator_lock_in_dir(&locks_dir, TARGET_UDID)
            .expect("reacquire after release");
        drop(reacquired);

        let _ = std::fs::remove_dir_all(&locks_dir);
    }

    #[test]
    fn simulator_lock_contends_across_different_deja_homes() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let home_a = resolve_deja_home(Some(Path::new(&format!("deja-home-a-{nanos}"))))
            .expect("resolve home a");
        let home_b = resolve_deja_home(Some(Path::new(&format!("deja-home-b-{nanos}"))))
            .expect("resolve home b");
        assert_ne!(home_a, home_b);

        let per_user_locks = resolve_simulator_locks_dir().expect("per-user locks dir");
        assert!(
            !per_user_locks.starts_with(&home_a) && !per_user_locks.starts_with(&home_b),
            "simulator locks dir must be independent of DEJA_HOME"
        );

        let owner = acquire_simulator_lock(SYNTHETIC_CROSS_HOME_UDID)
            .expect("first per-user simulator lock succeeds");
        let lock_file = owner.lock_path().to_path_buf();
        assert!(lock_file.starts_with(&per_user_locks));

        // Second session targeting the same UDID contends regardless of DEJA_HOME.
        match acquire_simulator_lock(SYNTHETIC_CROSS_HOME_UDID) {
            Err(IosDriverError::SimulatorBusy { udid }) => {
                assert_eq!(udid, SYNTHETIC_CROSS_HOME_UDID);
            }
            other => panic!("expected SimulatorBusy across different DEJA_HOMEs, got {other:?}"),
        }

        drop(owner);
        let _ = std::fs::remove_file(&lock_file);
    }

    #[test]
    fn ps_parser_verifies_kernel_comm_separately_from_args_and_session_xctestrun() {
        let session_xctestrun = format!("/tmp/deja/run-1/{TARGET_UDID}.xctestrun");
        let other_xctestrun = format!("/tmp/deja/run-2/{TARGET_UDID}.xctestrun");

        let comm_sample = "\
    1 /sbin/launchd
 1001 /usr/bin/xcodebuild
 1002 xcodebuild
 1003 /Applications/Xcode 27.2.app/Contents/Developer/usr/bin/xcodebuild
 1004 /usr/bin/xcodebuild
 1005 /usr/bin/xcodebuild
 1006 /usr/bin/xcodebuild
 1007 /usr/bin/xcodebuild
 1008 /usr/bin/python3
 1009 /bin/sh
 1010 /usr/bin/env
 1011 /usr/bin/xcodebuild
 9999 /usr/bin/xcodebuild
";

        let args_sample = format!(
            "\
    1 /sbin/launchd
 1001 /usr/bin/xcodebuild test-without-building -xctestrun {session_xctestrun} -destination id={TARGET_UDID}
 1002 xcodebuild test-without-building -xctestrun {session_xctestrun} -destination platform=iOS Simulator,id={lower}
 1003 /Applications/Xcode 27.2.app/Contents/Developer/usr/bin/xcodebuild test-without-building -xctestrun {session_xctestrun} -destination id={TARGET_UDID}
 1004 /usr/bin/xcodebuild test-without-building -xctestrun {other_xctestrun} -destination id={TARGET_UDID}
 1005 /usr/bin/xcodebuild test-without-building -xctestrun {session_xctestrun}.bak -destination id={TARGET_UDID}
 1006 /usr/bin/xcodebuild build-for-testing -xctestrun {session_xctestrun} -destination id={TARGET_UDID}
 1007 /usr/bin/xcodebuild test-without-building -xctestrun {session_xctestrun} -destination id={OTHER_UDID}
 1008 /usr/bin/python3 helper.py /Applications/Xcode 27.2.app/Contents/Developer/usr/bin/xcodebuild test-without-building -xctestrun {session_xctestrun} id={TARGET_UDID}
 1009 /bin/sh -c /Applications/Xcode 27.2.app/Contents/Developer/usr/bin/xcodebuild test-without-building -xctestrun {session_xctestrun} id={TARGET_UDID}
 1010 /usr/bin/env xcodebuild test-without-building -xctestrun {session_xctestrun} id={TARGET_UDID}
 1011 /usr/bin/xcodebuild test-without-building -xctestrun {session_xctestrun} -destination id={TARGET_UDID}-extra
 9999 /usr/bin/xcodebuild test-without-building -xctestrun {session_xctestrun} -destination id={TARGET_UDID}
",
            lower = TARGET_UDID.to_ascii_lowercase()
        );

        let session_pids = parse_ps_matching_xcodebuild_pids(
            comm_sample,
            &args_sample,
            TARGET_UDID,
            Some(Path::new(&session_xctestrun)),
            9999,
        );
        assert_eq!(session_pids, vec![1001, 1002, 1003]);

        let udid_pids =
            parse_ps_matching_xcodebuild_pids(comm_sample, &args_sample, TARGET_UDID, None, 9999);
        assert_eq!(udid_pids, vec![1001, 1002, 1003, 1004, 1005]);
    }

    #[cfg(unix)]
    #[test]
    fn bounded_sync_command_kills_sleeping_process_within_deadline() {
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");

        let started = Instant::now();
        let status = run_sync_command_bounded(cmd, Duration::from_millis(120));
        let elapsed = started.elapsed();

        assert!(
            status.is_none(),
            "expected None when bounded command times out"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "bounded command took too long: {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_async_command_kills_sleeping_process_on_timeout() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("30");

        let started = Instant::now();
        let status = run_async_command_bounded(cmd, Duration::from_millis(120)).await;
        let elapsed = started.elapsed();

        assert!(status.is_none(), "expected None on async command timeout");
        assert!(
            elapsed < Duration::from_secs(2),
            "bounded async command took too long: {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_managed_xcodebuild_after_aborted_shutdown_still_escalates_and_cleans_run_dir()
    {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let deja_home =
            std::env::temp_dir().join(format!("deja-managed-abort-{}-{nanos}", std::process::id()));
        let run_dir = deja_home.join("run");
        std::fs::create_dir_all(&run_dir).expect("create run_dir");
        let xctestrun_path = run_dir.join(format!("{TARGET_UDID}.xctestrun"));
        std::fs::write(&xctestrun_path, b"dummy").expect("write dummy xctestrun");
        let locks_dir = deja_home.join("locks");
        let sim_lock = acquire_simulator_lock_in_dir(&locks_dir, TARGET_UDID).expect("lock sim");

        // Spawn a process that ignores SIGTERM so `shutdown()` stays parked in its 1500ms wait.
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "trap '' TERM; while true; do sleep 1; done"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_process_group(&mut cmd);
        let child = cmd.spawn().expect("spawn trap-ignoring child");
        let child_pid = child.id().expect("child pid");

        let mut managed = ManagedXcodebuild::new(
            child,
            TARGET_UDID.to_string(),
            run_dir.clone(),
            xctestrun_path,
            Some(OsString::from("/nonexistent-empty-path")),
            None,
            Some(sim_lock),
        );

        // Give the shell a moment to install `trap '' TERM`, then cancel `shutdown()` mid-flight.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let timed_out = tokio::time::timeout(Duration::from_millis(150), managed.shutdown()).await;
        assert!(
            timed_out.is_err(),
            "shutdown should still be awaiting graceful SIGTERM timeout when aborted"
        );
        assert!(
            run_dir.exists(),
            "run_dir should not yet be removed before shutdown finished"
        );
        // Simulator lock must STILL be held while `managed` has not finished shutting down.
        assert!(matches!(
            acquire_simulator_lock_in_dir(&locks_dir, TARGET_UDID),
            Err(IosDriverError::SimulatorBusy { .. })
        ));

        // Dropping `managed` must escalate to SIGKILL, delete `run_dir`, and release `sim_lock`.
        drop(managed);

        assert!(
            !run_dir.exists(),
            "Drop must delete run_dir even after an aborted shutdown()"
        );
        assert!(
            !super::unix_signals::is_pid_alive(child_pid),
            "Drop must kill child pid {child_pid} after an aborted shutdown()"
        );
        acquire_simulator_lock_in_dir(&locks_dir, TARGET_UDID)
            .expect("simulator lock must be released after Drop completes");

        let _ = std::fs::remove_dir_all(&deja_home);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_teardown_kills_sigterm_trapping_grandchild_after_leader_exits() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "deja-pgid-grandchild-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create temp root");

        // Helper: spawns a leader that exits cleanly on SIGTERM while its grandchild ignores SIGTERM.
        async fn spawn_leader_with_stubborn_grandchild(
            pid_file: &Path,
        ) -> (tokio::process::Child, u32, i32) {
            let script = format!(
                "sh -c \"trap '' TERM; while true; do sleep 1; done\" & gc=$!; printf '%s' \"$gc\" > '{}'; trap 'exit 0' TERM; while true; do sleep 1; done",
                pid_file.display()
            );
            let mut cmd = tokio::process::Command::new("sh");
            cmd.args(["-c", &script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            configure_process_group(&mut cmd);
            let child = cmd.spawn().expect("spawn leader");
            let pgid = i32::try_from(child.id().expect("leader pid")).expect("pgid i32");

            let mut gc_pid = None;
            for _ in 0..100 {
                if let Ok(raw) = std::fs::read_to_string(pid_file)
                    && let Ok(parsed) = raw.trim().parse::<u32>()
                {
                    gc_pid = Some(parsed);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let gc_pid = gc_pid.expect("grandchild pid should be written");
            // Give the grandchild a moment to install `trap '' TERM`.
            tokio::time::sleep(Duration::from_millis(80)).await;
            assert!(super::unix_signals::is_pid_alive(gc_pid));
            (child, gc_pid, pgid)
        }

        // 1. Async `ManagedXcodebuild::shutdown()` path.
        let pid_file_async = root.join("gc-async.pid");
        let run_dir_async = root.join("run-async");
        std::fs::create_dir_all(&run_dir_async).expect("create run_dir_async");
        let (child_async, gc_async, pgid_async) =
            spawn_leader_with_stubborn_grandchild(&pid_file_async).await;
        let mut managed = ManagedXcodebuild::new(
            child_async,
            TARGET_UDID.to_string(),
            run_dir_async.clone(),
            run_dir_async.join(format!("{TARGET_UDID}.xctestrun")),
            Some(OsString::from("/nonexistent-empty-path")),
            None,
            None,
        );
        managed.shutdown().await.expect("shutdown succeeds");
        assert!(
            !super::unix_signals::is_pid_alive(gc_async),
            "shutdown() must SIGKILL SIGTERM-trapping grandchild {gc_async} even after leader exits"
        );
        assert!(
            !super::unix_signals::process_group_alive(pgid_async),
            "process group {pgid_async} must reach ESRCH before shutdown() returns"
        );

        // 2. Synchronous `terminate_child_group_sync()` path (used by `BuildGuard::drop` & `ManagedXcodebuild::drop`).
        let pid_file_sync = root.join("gc-sync.pid");
        let (mut child_sync, gc_sync, pgid_sync) =
            spawn_leader_with_stubborn_grandchild(&pid_file_sync).await;
        terminate_child_group_sync(&mut child_sync, Some(pgid_sync));
        assert!(
            !super::unix_signals::is_pid_alive(gc_sync),
            "terminate_child_group_sync() must SIGKILL SIGTERM-trapping grandchild {gc_sync}"
        );
        assert!(
            !super::unix_signals::process_group_alive(pgid_sync),
            "process group {pgid_sync} must reach ESRCH before terminate_child_group_sync() returns"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
