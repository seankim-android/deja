use crate::error::{IosDriverError, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
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
mod unix_signals {
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

    pub(crate) const SIGTERM: i32 = libc::SIGTERM;
    pub(crate) const SIGKILL: i32 = libc::SIGKILL;
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
/// - `xcodebuild` executable token, AND
/// - `test-without-building` token, AND
/// - exact case-insensitive `<udid>` token (standalone or `id=<udid>`).
///
/// Never matches `self_pid`, PID <= 1, unrelated simulators, or `build-for-testing`.
pub fn parse_ps_matching_xcodebuild_pids(ps_output: &str, udid: &str, self_pid: u32) -> Vec<u32> {
    let Ok(canonical_udid) = validate_udid(udid) else {
        return Vec::new();
    };
    let udid_lower = canonical_udid.to_ascii_lowercase();

    let mut matched = Vec::new();
    for raw_line in ps_output.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(pid_str) = parts.next() else {
            continue;
        };
        let Some(_ppid_str) = parts.next() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if pid <= 1 || pid == self_pid {
            continue;
        }

        let tokens: Vec<&str> = parts.collect();
        if tokens.is_empty() {
            continue;
        }

        let has_xcodebuild = tokens.iter().any(|tok| is_xcodebuild_token(tok));
        let has_test_without_building = tokens.contains(&"test-without-building");
        let has_exact_udid = tokens
            .iter()
            .any(|tok| token_matches_udid(tok, &udid_lower));

        if has_xcodebuild && has_test_without_building && has_exact_udid {
            matched.push(pid);
        }
    }
    matched
}

fn is_xcodebuild_token(token: &str) -> bool {
    if token == "xcodebuild" {
        return true;
    }
    Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "xcodebuild")
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

/// Lists all live `xcodebuild test-without-building` PIDs targeting `udid`.
pub fn find_xcodebuild_test_pids_for_udid(udid: &str) -> Result<Vec<u32>> {
    let canonical = validate_udid(udid)?;
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_ps_matching_xcodebuild_pids(
        &stdout,
        &canonical,
        std::process::id(),
    ))
}

/// Sweeps any remaining `xcodebuild test-without-building` processes targeting `udid` (DEJA-54).
/// Sends `SIGTERM`, polls briefly, then `SIGKILL`s any survivors.
pub async fn sweep_xcodebuild_for_udid(udid: &str) -> Result<usize> {
    let canonical = validate_udid(udid)?;
    let pids = find_xcodebuild_test_pids_for_udid(&canonical)?;
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
            let remaining = find_xcodebuild_test_pids_for_udid(&canonical)?;
            if remaining.is_empty() {
                return Ok(signaled);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let survivors = find_xcodebuild_test_pids_for_udid(&canonical)?;
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
    env_path: Option<OsString>,
    developer_dir: Option<OsString>,
    shut_down: bool,
}

impl ManagedXcodebuild {
    pub fn new(
        child: Child,
        udid: String,
        run_dir: PathBuf,
        env_path: Option<OsString>,
        developer_dir: Option<OsString>,
    ) -> Self {
        let pgid = child.id().and_then(|id| i32::try_from(id).ok());
        Self {
            child: Some(child),
            pgid,
            udid,
            run_dir,
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
    /// sweeps any leftover `xcodebuild test-without-building` processes for this UDID, terminates
    /// the simulator runner bundle, and deletes the per-run directory.
    pub async fn shutdown(&mut self) -> Result<()> {
        if self.shut_down {
            return Ok(());
        }
        self.shut_down = true;

        if let Some(mut child) = self.child.take() {
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

        if let Err(err) = sweep_xcodebuild_for_udid(&self.udid).await {
            warn!(udid = %self.udid, %err, "UDID xcodebuild sweep encountered error");
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
            let already_exited = child.try_wait().ok().flatten().is_some();
            if !already_exited {
                #[cfg(unix)]
                if let Some(pgid) = self.pgid {
                    let _ = unix_signals::kill_process_group(pgid, unix_signals::SIGTERM);
                    for _ in 0..8 {
                        if child.try_wait().ok().flatten().is_some() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    if child.try_wait().ok().flatten().is_none() {
                        let _ = unix_signals::kill_process_group(pgid, unix_signals::SIGKILL);
                    }
                }
                let _ = child.start_kill();
            }
        }

        #[cfg(unix)]
        if let Ok(pids) = find_xcodebuild_test_pids_for_udid(&self.udid) {
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
            let _ = cmd.status();
        }

        if self.run_dir.exists() {
            let _ = std::fs::remove_dir_all(&self.run_dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_ps_matching_xcodebuild_pids, validate_udid};

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
    fn ps_parser_matches_only_target_udid_test_without_building() {
        let sample = format!(
            "\
    1     0 /sbin/launchd
 1001     1 /usr/bin/xcodebuild test-without-building -xctestrun /tmp/a.xctestrun -destination id={TARGET_UDID}
 1002  1001 xcodebuild test-without-building -destination platform=iOS Simulator,id={lower}
 1003     1 /usr/bin/xcodebuild build-for-testing -destination id={TARGET_UDID}
 1004     1 /usr/bin/xcodebuild test-without-building -destination id={OTHER_UDID}
 1005     1 /usr/bin/python3 helper.py test-without-building id={TARGET_UDID}
 1006     1 /usr/bin/xcodebuild test-without-building -destination id={TARGET_UDID}-extra
 9999     1 /usr/bin/xcodebuild test-without-building -destination id={TARGET_UDID}
",
            lower = TARGET_UDID.to_ascii_lowercase()
        );

        let pids = parse_ps_matching_xcodebuild_pids(&sample, TARGET_UDID, 9999);
        assert_eq!(pids, vec![1001, 1002]);
    }
}
