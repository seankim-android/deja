use crate::error::{IosDriverError, Result};
use std::ffi::OsString;
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

    pub(crate) const SIGTERM: i32 = libc::SIGTERM;
    pub(crate) const SIGKILL: i32 = libc::SIGKILL;
}

/// Synchronously terminates an isolated child process group (`SIGTERM` -> short poll -> `SIGKILL`)
/// and reaps the child with a bounded wait. Used by synchronous `Drop` guards.
pub(crate) fn terminate_child_group_sync(child: &mut Child, pgid: Option<i32>) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }

    #[cfg(unix)]
    if let Some(pgid) = pgid {
        let _ = unix_signals::kill_process_group(pgid, unix_signals::SIGTERM);
        for _ in 0..8 {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = unix_signals::kill_process_group(pgid, unix_signals::SIGKILL);
    }

    #[cfg(not(unix))]
    let _ = pgid;

    let _ = child.start_kill();
    let reap_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < reap_deadline {
        if child.try_wait().ok().flatten().is_some() {
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

/// Parses `ps -e -o pid=,ppid=,args=` output and returns PIDs matching ONLY:
/// - executable `argv[0]` whose basename is `xcodebuild` (never `xcodebuild` merely appearing in
///   another process's arguments), AND
/// - `test-without-building` token, AND
/// - exact case-insensitive `<udid>` token (standalone or `id=<udid>`), AND
/// - when `xctestrun_path` is `Some`, the exact per-run `.xctestrun` path in `args` so concurrent
///   sessions targeting the same simulator are never matched.
///
/// Never matches `self_pid`, PID <= 1, unrelated simulators, or `build-for-testing`.
pub fn parse_ps_matching_xcodebuild_pids(
    ps_output: &str,
    udid: &str,
    xctestrun_path: Option<&Path>,
    self_pid: u32,
) -> Vec<u32> {
    let Ok(canonical_udid) = validate_udid(udid) else {
        return Vec::new();
    };
    let udid_lower = canonical_udid.to_ascii_lowercase();
    let xctestrun_candidates = build_path_candidates(xctestrun_path);

    let mut matched = Vec::new();
    for raw_line in ps_output.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((pid, args_str)) = split_ps_pid_ppid_args(line) else {
            continue;
        };
        if pid <= 1 || pid == self_pid {
            continue;
        }

        if !argv0_is_xcodebuild(args_str) {
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

fn split_ps_pid_ppid_args(line: &str) -> Option<(u32, &str)> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let pid_start = i;
    while i < len && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let pid = line[pid_start..i].parse::<u32>().ok()?;

    while i < len && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let ppid_start = i;
    while i < len && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if ppid_start == i {
        return None;
    }
    let _ppid = line[ppid_start..i].parse::<u32>().ok()?;

    while i < len && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= len {
        return None;
    }
    Some((pid, &line[i..]))
}

fn argv0_is_xcodebuild(args_str: &str) -> bool {
    let Some(first_token) = args_str.split_whitespace().next() else {
        return false;
    };
    if is_xcodebuild_basename(first_token) {
        return true;
    }
    // Support macOS Xcode app bundle paths containing spaces, e.g.
    // `/Applications/Xcode 27.2.app/Contents/Developer/usr/bin/xcodebuild ...`
    // while strictly checking only argv[0] before any CLI flags/subcommands.
    if args_str.starts_with('/') {
        let marker = ".app/Contents/Developer/usr/bin/xcodebuild";
        if let Some(idx) = args_str.find(marker) {
            let after_idx = idx + marker.len();
            let boundary_ok =
                after_idx == args_str.len() || args_str.as_bytes()[after_idx].is_ascii_whitespace();
            let prefix = &args_str[..idx];
            // Ensure no earlier flag or separate executable precedes the Xcode `.app` path.
            if boundary_ok && !prefix.contains(" -") {
                return true;
            }
        }
    }
    false
}

fn is_xcodebuild_basename(token: &str) -> bool {
    if token == "xcodebuild" {
        return true;
    }
    Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "xcodebuild")
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

fn read_ps_output() -> Result<String> {
    let output = std::process::Command::new("ps")
        .args(["-e", "-o", "pid=,ppid=,args="])
        .stdin(Stdio::null())
        .output()
        .map_err(|source| IosDriverError::CommandSpawn {
            program: "ps".to_string(),
            source,
        })?;
    if !output.status.success() {
        return Err(IosDriverError::CommandFailed {
            program: "ps".to_string(),
            args: "-e -o pid=,ppid=,args=".to_string(),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Lists all live `xcodebuild test-without-building` PIDs targeting `udid`.
pub fn find_xcodebuild_test_pids_for_udid(udid: &str) -> Result<Vec<u32>> {
    let canonical = validate_udid(udid)?;
    let stdout = read_ps_output()?;
    Ok(parse_ps_matching_xcodebuild_pids(
        &stdout,
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
    let stdout = read_ps_output()?;
    Ok(parse_ps_matching_xcodebuild_pids(
        &stdout,
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

/// Owns a spawned `xcodebuild test-without-building` process group and guarantees cleanup on both
/// explicit async `shutdown()` and synchronous `Drop`.
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
}

impl ManagedXcodebuild {
    pub fn new(
        child: Child,
        udid: String,
        run_dir: PathBuf,
        xctestrun_path: PathBuf,
        env_path: Option<OsString>,
        developer_dir: Option<OsString>,
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
        }
    }

    pub fn child_mut(&mut self) -> Option<&mut Child> {
        self.child.as_mut()
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    /// Gracefully terminates the `xcodebuild` process group (`SIGTERM` -> 1500ms -> `SIGKILL`),
    /// sweeps any leftover `xcodebuild test-without-building` processes for this session,
    /// terminates the simulator runner bundle, and deletes the per-run directory.
    ///
    /// Keeps `self.child` and `self.shut_down == false` armed across all `.await` points so that
    /// cancelling/dropping `shutdown()` mid-flight still triggers full `Drop` escalation.
    pub async fn shutdown(&mut self) -> Result<()> {
        if self.shut_down {
            return Ok(());
        }

        if let Some(child) = self.child.as_mut() {
            let already_exited = child.try_wait().ok().flatten().is_some();
            if !already_exited {
                #[cfg(unix)]
                if let Some(pgid) = self.pgid {
                    debug!(pgid, udid = %self.udid, "sending SIGTERM to xcodebuild process group");
                    if let Err(err) = unix_signals::kill_process_group(pgid, unix_signals::SIGTERM)
                    {
                        warn!(pgid, %err, "failed to send SIGTERM to xcodebuild process group");
                    }
                }

                let wait_result =
                    tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, child.wait()).await;
                if wait_result.is_err() {
                    #[cfg(unix)]
                    if let Some(pgid) = self.pgid {
                        warn!(
                            pgid,
                            udid = %self.udid,
                            "xcodebuild process group did not exit after SIGTERM; sending SIGKILL"
                        );
                        let _ = unix_signals::kill_process_group(pgid, unix_signals::SIGKILL);
                    }
                    let _ = child.kill().await;
                    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                }
            }
        }
        self.child = None;

        if let Err(err) = sweep_xcodebuild_for_session(&self.udid, &self.xctestrun_path).await {
            warn!(udid = %self.udid, %err, "session xcodebuild sweep encountered error");
        }

        // Best-effort terminate the XCTRunner wrapper, test bundle, and host app in the simulator.
        for bundle_id in [
            RUNNER_XCTRUNNER_BUNDLE_ID,
            RUNNER_BUNDLE_ID,
            RUNNER_HOST_BUNDLE_ID,
        ] {
            let mut cmd = Command::new("xcrun");
            cmd.args(["simctl", "terminate", &self.udid, bundle_id])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Some(ref path) = self.env_path {
                cmd.env("PATH", path);
            }
            if let Some(ref dev_dir) = self.developer_dir {
                cmd.env("DEVELOPER_DIR", dev_dir);
            }
            let _ = tokio::time::timeout(Duration::from_secs(5), cmd.status()).await;
        }

        if self.run_dir.exists() {
            let _ = tokio::fs::remove_dir_all(&self.run_dir).await;
        }

        self.shut_down = true;
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
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ManagedXcodebuild, configure_process_group, parse_ps_matching_xcodebuild_pids,
        run_sync_command_bounded, validate_udid,
    };
    use std::ffi::OsString;
    use std::path::Path;
    use std::process::Stdio;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const TARGET_UDID: &str = "BA555783-AC7E-46E3-B749-34F9499B2C2B";
    const OTHER_UDID: &str = "DA910389-DF0B-4254-BB78-7164E0E27609";

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
    fn ps_parser_requires_argv0_xcodebuild_and_session_xctestrun_path() {
        let session_xctestrun = format!("/tmp/deja/run-1/{TARGET_UDID}.xctestrun");
        let other_xctestrun = format!("/tmp/deja/run-2/{TARGET_UDID}.xctestrun");
        let sample = format!(
            "\
    1     0 /sbin/launchd
 1001     1 /usr/bin/xcodebuild test-without-building -xctestrun {session_xctestrun} -destination id={TARGET_UDID}
 1002  1001 xcodebuild test-without-building -xctestrun {session_xctestrun} -destination platform=iOS Simulator,id={lower}
 1003     1 /Applications/Xcode 27.2.app/Contents/Developer/usr/bin/xcodebuild test-without-building -xctestrun {session_xctestrun} -destination id={TARGET_UDID}
 1004     1 /usr/bin/xcodebuild test-without-building -xctestrun {other_xctestrun} -destination id={TARGET_UDID}
 1005     1 /usr/bin/xcodebuild test-without-building -xctestrun {session_xctestrun}.bak -destination id={TARGET_UDID}
 1006     1 /usr/bin/xcodebuild build-for-testing -xctestrun {session_xctestrun} -destination id={TARGET_UDID}
 1007     1 /usr/bin/xcodebuild test-without-building -xctestrun {session_xctestrun} -destination id={OTHER_UDID}
 1008     1 /usr/bin/python3 helper.py /usr/bin/xcodebuild test-without-building -xctestrun {session_xctestrun} id={TARGET_UDID}
 1009     1 /bin/sh -c /usr/bin/xcodebuild test-without-building -xctestrun {session_xctestrun} id={TARGET_UDID}
 1010     1 /usr/bin/env xcodebuild test-without-building -xctestrun {session_xctestrun} id={TARGET_UDID}
 1011     1 /usr/bin/xcodebuild test-without-building -xctestrun {session_xctestrun} -destination id={TARGET_UDID}-extra
 9999     1 /usr/bin/xcodebuild test-without-building -xctestrun {session_xctestrun} -destination id={TARGET_UDID}
",
            lower = TARGET_UDID.to_ascii_lowercase()
        );

        // Session-scoped query matches ONLY PIDs with argv[0]=xcodebuild, target UDID, and exact xctestrun path.
        let session_pids = parse_ps_matching_xcodebuild_pids(
            &sample,
            TARGET_UDID,
            Some(Path::new(&session_xctestrun)),
            9999,
        );
        assert_eq!(session_pids, vec![1001, 1002, 1003]);

        // UDID-wide query still excludes non-xcodebuild argv[0], build-for-testing, other UDIDs, and self_pid.
        let udid_pids = parse_ps_matching_xcodebuild_pids(&sample, TARGET_UDID, None, 9999);
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
    async fn dropping_managed_xcodebuild_after_aborted_shutdown_still_escalates_and_cleans_run_dir()
    {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let run_dir =
            std::env::temp_dir().join(format!("deja-managed-abort-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&run_dir).expect("create run_dir");
        let xctestrun_path = run_dir.join(format!("{TARGET_UDID}.xctestrun"));
        std::fs::write(&xctestrun_path, b"dummy").expect("write dummy xctestrun");

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

        // Dropping `managed` must still escalate to SIGKILL and remove `run_dir`.
        drop(managed);

        assert!(
            !run_dir.exists(),
            "Drop must delete run_dir even after an aborted shutdown()"
        );
        assert!(
            !super::unix_signals::is_pid_alive(child_pid),
            "Drop must kill child pid {child_pid} after an aborted shutdown()"
        );
    }
}
