//! Native macOS/iOS Simulator `.ips` (v2) crash report scanner (`DEJA-46`).
//!
//! Scans `$HOME/Library/Logs/DiagnosticReports` (or a caller-supplied directory) for `.ips` crash
//! reports produced during a test window, strictly attributing crashes ONLY when:
//! - a valid target simulator UDID is supplied (fail-closed when `None`/invalid),
//! - `payload.procPath` lies under `.../CoreSimulator/Devices/<UDID>/...`, and
//! - the target app matches by exact bundle identifier (or exact process name fallback when
//!   permitted).

use std::fs::OpenOptions;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

use deja_core::CrashSignalSource;
use deja_flow::DriverCrashSignal;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::error::{IosDriverError, Result};
use crate::process::validate_udid;

/// Maximum `.ips` file size inspected by the crash scanner (8 MiB).
pub const MAX_CRASH_REPORT_BYTES: u64 = 8 * 1024 * 1024;

const GRACE_POLL_INTERVAL: Duration = Duration::from_millis(75);

/// Returns macOS's default crash report directory (`$HOME/Library/Logs/DiagnosticReports`).
#[must_use]
pub fn default_crash_report_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(
        PathBuf::from(home)
            .join("Library")
            .join("Logs")
            .join("DiagnosticReports"),
    )
}

/// Parameters controlling a bounded `.ips` simulator crash report lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashSearchOptions {
    pub directory: PathBuf,
    pub app_id: String,
    pub device_udid: Option<String>,
    pub since_ms: u64,
    pub until_ms: u64,
}

#[derive(Debug, Deserialize)]
struct IpsHeader {
    #[serde(default)]
    app_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "bundleID", default)]
    bundle_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IpsBundleInfo {
    #[serde(rename = "CFBundleIdentifier", default)]
    cf_bundle_identifier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IpsPayload {
    #[serde(rename = "procName", default)]
    proc_name: Option<String>,
    #[serde(rename = "procPath", default)]
    proc_path: Option<String>,
    #[serde(rename = "bundleInfo", default)]
    bundle_info: Option<IpsBundleInfo>,
}

#[derive(Debug)]
struct CandidateFile {
    mtime_ms: u64,
    filename: String,
    path: PathBuf,
}

/// Synchronously scans `opts.directory` for the newest `.ips` crash report matching `opts.app_id`
/// and `opts.device_udid` with modification time in `[opts.since_ms, opts.until_ms]`.
///
/// Fails closed (returns `Ok(None)`) when `opts.device_udid` is `None` or invalid, when
/// `opts.directory` does not exist, or when a report cannot be strictly attributed to the target
/// simulator UDID via `payload.procPath`.
pub fn find_crash_report(opts: &CrashSearchOptions) -> Result<Option<DriverCrashSignal>> {
    let Some(raw_udid) = opts.device_udid.as_deref() else {
        return Ok(None);
    };
    let Ok(canonical_udid) = validate_udid(raw_udid) else {
        return Ok(None);
    };
    let target_app_id = opts.app_id.trim();
    if target_app_id.is_empty() || opts.until_ms < opts.since_ms {
        return Ok(None);
    }

    let entries = match std::fs::read_dir(&opts.directory) {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(IosDriverError::Io {
                context: format!("read crash report directory {}", opts.directory.display()),
                source,
            });
        }
    };

    let mut candidates = Vec::new();
    for entry_res in entries {
        let Ok(entry) = entry_res else {
            continue;
        };
        let path = entry.path();
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !filename.ends_with(".ips") {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > MAX_CRASH_REPORT_BYTES {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Ok(dur) = modified.duration_since(UNIX_EPOCH) else {
            continue;
        };
        let mtime_ms = u64::try_from(dur.as_millis()).unwrap_or(u64::MAX);
        if mtime_ms < opts.since_ms || mtime_ms > opts.until_ms {
            continue;
        }
        candidates.push(CandidateFile {
            mtime_ms,
            filename: filename.to_string(),
            path,
        });
    }

    candidates.sort_by(|a, b| {
        b.mtime_ms
            .cmp(&a.mtime_ms)
            .then_with(|| a.filename.cmp(&b.filename))
    });

    for candidate in candidates {
        let Some(bytes) = read_regular_ips_bounded(&candidate.path) else {
            continue;
        };
        let Some((header, payload)) = parse_ips_v2(&bytes) else {
            continue;
        };

        let Some(proc_path) = payload.proc_path.as_deref() else {
            continue;
        };
        let Some(report_udid) = extract_simulator_udid_from_proc_path(proc_path) else {
            continue;
        };
        if !report_udid.eq_ignore_ascii_case(&canonical_udid) {
            continue;
        }

        if !matches_app(target_app_id, &header, &payload) {
            continue;
        }

        let raw_process = non_empty_trimmed(payload.proc_name.as_deref())
            .or_else(|| non_empty_trimmed(header.app_name.as_deref()))
            .or_else(|| non_empty_trimmed(header.name.as_deref()));
        let process = validate_process_name(raw_process);

        return Ok(Some(DriverCrashSignal {
            source: CrashSignalSource::CrashReport,
            report_file: Some(candidate.filename),
            process,
            detail: None,
        }));
    }

    Ok(None)
}

/// Polls for a matching `.ips` crash report across `[opts.since_ms, opts.until_ms + grace_ms]`
/// every ~75 ms until found, `grace` elapses, or `cancel` fires.
pub async fn find_crash_report_with_grace(
    opts: &CrashSearchOptions,
    grace: Duration,
    cancel: &CancellationToken,
) -> Result<Option<DriverCrashSignal>> {
    if cancel.is_cancelled() {
        return Err(IosDriverError::Cancelled);
    }

    // Fail closed immediately without sleeping when UDID is missing or malformed.
    let Some(raw_udid) = opts.device_udid.as_deref() else {
        return Ok(None);
    };
    if validate_udid(raw_udid).is_err() || opts.app_id.trim().is_empty() {
        return Ok(None);
    }

    let grace_ms = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX);
    let mut effective = opts.clone();
    effective.until_ms = opts.until_ms.saturating_add(grace_ms);

    let deadline = tokio::time::Instant::now() + grace;

    loop {
        if cancel.is_cancelled() {
            return Err(IosDriverError::Cancelled);
        }

        let search_opts = effective.clone();
        let found = tokio::task::spawn_blocking(move || find_crash_report(&search_opts))
            .await
            .map_err(|err| IosDriverError::Io {
                context: "join crash report scan task".to_string(),
                source: std::io::Error::other(err.to_string()),
            })??;
        if found.is_some() {
            return Ok(found);
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(None);
        }

        let sleep_for = GRACE_POLL_INTERVAL.min(deadline.saturating_duration_since(now));
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(IosDriverError::Cancelled),
            _ = tokio::time::sleep(sleep_for) => {}
        }
    }
}

fn read_regular_ips_bounded(path: &std::path::Path) -> Option<Vec<u8>> {
    let mut open_opts = OpenOptions::new();
    open_opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.custom_flags(libc::O_NOFOLLOW);
    }

    let file = open_opts.open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_CRASH_REPORT_BYTES {
        return None;
    }

    let initial_cap = usize::try_from(meta.len()).unwrap_or(0).min(64 * 1024);
    let mut bytes = Vec::with_capacity(initial_cap);
    file.take(MAX_CRASH_REPORT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if (bytes.len() as u64) > MAX_CRASH_REPORT_BYTES {
        return None;
    }
    Some(bytes)
}

fn parse_ips_v2(bytes: &[u8]) -> Option<(IpsHeader, IpsPayload)> {
    let mut header_iter = serde_json::Deserializer::from_slice(bytes).into_iter::<IpsHeader>();
    let header = header_iter.next()?.ok()?;
    let offset = header_iter.byte_offset();
    if offset >= bytes.len() {
        return None;
    }

    let mut payload_iter =
        serde_json::Deserializer::from_slice(&bytes[offset..]).into_iter::<IpsPayload>();
    let payload = payload_iter.next()?.ok()?;
    Some((header, payload))
}

fn extract_simulator_udid_from_proc_path(proc_path: &str) -> Option<String> {
    if proc_path.is_empty() || proc_path.contains('\0') {
        return None;
    }
    let segments: Vec<&str> = proc_path.split('/').collect();
    if segments.contains(&"..") {
        return None;
    }
    for window in segments.windows(4) {
        if window[0] == "CoreSimulator"
            && window[1] == "Devices"
            && !window[3].is_empty()
            && let Ok(udid) = validate_udid(window[2])
        {
            return Some(udid);
        }
    }
    None
}

fn matches_app(target_app_id: &str, header: &IpsHeader, payload: &IpsPayload) -> bool {
    let header_bid = non_empty_trimmed(header.bundle_id.as_deref());
    let payload_bid = payload
        .bundle_info
        .as_ref()
        .and_then(|b| non_empty_trimmed(b.cf_bundle_identifier.as_deref()));

    if header_bid == Some(target_app_id) || payload_bid == Some(target_app_id) {
        return true;
    }

    let has_bundle_id = header_bid.is_some() || payload_bid.is_some();
    let can_fallback_to_name = if !has_bundle_id {
        true
    } else if !target_app_id.contains('.') {
        [header_bid, payload_bid]
            .into_iter()
            .flatten()
            .all(|bid| bid.contains('.'))
    } else {
        false
    };

    if !can_fallback_to_name {
        return false;
    }

    [
        non_empty_trimmed(payload.proc_name.as_deref()),
        non_empty_trimmed(header.app_name.as_deref()),
        non_empty_trimmed(header.name.as_deref()),
    ]
    .into_iter()
    .flatten()
    .any(|name| name == target_app_id)
}

fn non_empty_trimmed(raw: Option<&str>) -> Option<&str> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn validate_process_name(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() || trimmed.len() > 64 {
        return None;
    }
    if trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'.' | b'_' | b'+' | b'-'))
    {
        Some(trimmed.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CrashSearchOptions, MAX_CRASH_REPORT_BYTES, extract_simulator_udid_from_proc_path,
        find_crash_report, find_crash_report_with_grace, validate_process_name,
    };
    use deja_core::CrashSignalSource;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio_util::sync::CancellationToken;

    const UDID_A: &str = "BA555783-AC7E-46E3-B749-34F9499B2C2B";
    const UDID_B: &str = "DA910389-DF0B-4254-BB78-7164E0E27609";

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "deja-crash-test-{tag}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("create temp crash dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis() as u64
    }

    fn write_ips(dir: &Path, name: &str, header_json: &str, payload_json: &str) -> PathBuf {
        let path = dir.join(name);
        let content = format!("{header_json}\n{payload_json}\n");
        std::fs::write(&path, content).expect("write .ips file");
        path
    }

    fn sim_proc_path(udid: &str, binary: &str) -> String {
        format!(
            "/sim/Library/Developer/CoreSimulator/Devices/{udid}/data/Containers/Bundle/Application/11111111-2222-3333-4444-555555555555/{binary}.app/{binary}"
        )
    }

    #[test]
    fn matches_valid_v2_ips_for_target_udid_and_bundle_id() {
        let dir = TempDirGuard::new("match");
        let start = now_ms().saturating_sub(5_000);
        let proc_path = sim_proc_path(UDID_A, "DejaFixture");
        write_ips(
            dir.path(),
            "DejaFixture-2026-11-28-010203.ips",
            r#"{"app_name":"DejaFixture","name":"DejaFixture","bundleID":"dev.deja.ios.checkout"}"#,
            &format!(
                r#"{{"procName":"DejaFixture","procPath":"{proc_path}","bundleInfo":{{"CFBundleIdentifier":"dev.deja.ios.checkout"}},"threads":[{{"frames":[1,2,3]}}]}}"#
            ),
        );
        let end = now_ms().saturating_add(5_000);

        let sig = find_crash_report(&CrashSearchOptions {
            directory: dir.path().to_path_buf(),
            app_id: "dev.deja.ios.checkout".to_string(),
            device_udid: Some(UDID_A.to_ascii_lowercase()),
            since_ms: start,
            until_ms: end,
        })
        .expect("scan should succeed")
        .expect("matching crash report must be found");

        assert_eq!(sig.source, CrashSignalSource::CrashReport);
        assert_eq!(
            sig.report_file.as_deref(),
            Some("DejaFixture-2026-11-28-010203.ips")
        );
        assert_eq!(sig.process.as_deref(), Some("DejaFixture"));
        assert!(sig.detail.is_none());
    }

    #[test]
    fn fails_closed_when_udid_is_missing_invalid_or_mismatched() {
        let dir = TempDirGuard::new("udid");
        let start = now_ms().saturating_sub(5_000);
        let proc_path = sim_proc_path(UDID_A, "DejaFixture");
        write_ips(
            dir.path(),
            "DejaFixture.ips",
            r#"{"app_name":"DejaFixture","bundleID":"dev.deja.ios.checkout"}"#,
            &format!(
                r#"{{"procName":"DejaFixture","procPath":"{proc_path}","bundleInfo":{{"CFBundleIdentifier":"dev.deja.ios.checkout"}}}}"#
            ),
        );
        let end = now_ms().saturating_add(5_000);

        // Missing UDID -> None.
        assert!(
            find_crash_report(&CrashSearchOptions {
                directory: dir.path().to_path_buf(),
                app_id: "dev.deja.ios.checkout".to_string(),
                device_udid: None,
                since_ms: start,
                until_ms: end,
            })
            .unwrap()
            .is_none()
        );

        // Malformed UDID -> None.
        assert!(
            find_crash_report(&CrashSearchOptions {
                directory: dir.path().to_path_buf(),
                app_id: "dev.deja.ios.checkout".to_string(),
                device_udid: Some("not-a-uuid".to_string()),
                since_ms: start,
                until_ms: end,
            })
            .unwrap()
            .is_none()
        );

        // Different simulator UDID -> None.
        assert!(
            find_crash_report(&CrashSearchOptions {
                directory: dir.path().to_path_buf(),
                app_id: "dev.deja.ios.checkout".to_string(),
                device_udid: Some(UDID_B.to_string()),
                since_ms: start,
                until_ms: end,
            })
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn rejects_host_macos_paths_missing_proc_path_and_non_proc_path_udid() {
        let dir = TempDirGuard::new("host-paths");
        let start = now_ms().saturating_sub(5_000);

        // 1. Host macOS process path (even with target UDID smuggled into another field).
        write_ips(
            dir.path(),
            "host-app.ips",
            r#"{"app_name":"DejaFixture","bundleID":"dev.deja.ios.checkout"}"#,
            &format!(
                r#"{{"procName":"DejaFixture","procPath":"/Applications/DejaFixture.app/Contents/MacOS/DejaFixture","notes":"CoreSimulator/Devices/{UDID_A}/fake"}}"#
            ),
        );

        // 2. Missing procPath completely.
        write_ips(
            dir.path(),
            "missing-proc.ips",
            r#"{"app_name":"DejaFixture","bundleID":"dev.deja.ios.checkout"}"#,
            &format!(r#"{{"procName":"DejaFixture","deviceUdid":"{UDID_A}"}}"#),
        );

        // 3. Non-.ips extension ignored.
        write_ips(
            dir.path(),
            "ignored.crash",
            r#"{"app_name":"DejaFixture","bundleID":"dev.deja.ios.checkout"}"#,
            &format!(
                r#"{{"procName":"DejaFixture","procPath":"{}"}}"#,
                sim_proc_path(UDID_A, "DejaFixture")
            ),
        );

        let end = now_ms().saturating_add(5_000);
        let found = find_crash_report(&CrashSearchOptions {
            directory: dir.path().to_path_buf(),
            app_id: "dev.deja.ios.checkout".to_string(),
            device_udid: Some(UDID_A.to_string()),
            since_ms: start,
            until_ms: end,
        })
        .expect("scan succeeds");
        assert!(found.is_none(), "must not attribute non-simulator reports");
    }

    #[test]
    fn rejects_prefix_matches_and_respects_bundle_id_vs_name_fallback() {
        let dir = TempDirGuard::new("app-match");
        let start = now_ms().saturating_sub(5_000);
        let proc_path = sim_proc_path(UDID_A, "DejaFixture");

        // Prefix bundle ID must NOT match.
        write_ips(
            dir.path(),
            "prefix.ips",
            r#"{"app_name":"DejaFixture","bundleID":"dev.deja.ios.checkout.extra"}"#,
            &format!(r#"{{"procName":"DejaFixture","procPath":"{proc_path}"}}"#),
        );
        let end = now_ms().saturating_add(5_000);

        assert!(
            find_crash_report(&CrashSearchOptions {
                directory: dir.path().to_path_buf(),
                app_id: "dev.deja.ios.checkout".to_string(),
                device_udid: Some(UDID_A.to_string()),
                since_ms: start,
                until_ms: end,
            })
            .unwrap()
            .is_none()
        );

        // Non-dotted process-name query matches when bundleID is reverse-DNS.
        let by_name = find_crash_report(&CrashSearchOptions {
            directory: dir.path().to_path_buf(),
            app_id: "DejaFixture".to_string(),
            device_udid: Some(UDID_A.to_string()),
            since_ms: start,
            until_ms: end,
        })
        .unwrap()
        .expect("process-name fallback should match");
        assert_eq!(by_name.process.as_deref(), Some("DejaFixture"));
    }

    #[test]
    fn skips_oversized_and_malformed_reports_and_sanitizes_process_name() {
        let dir = TempDirGuard::new("bounds");
        let start = now_ms().saturating_sub(5_000);

        // Oversized > 8 MiB file must be skipped.
        let big = dir.path().join("z-huge.ips");
        let file = std::fs::File::create(&big).expect("create big file");
        file.set_len(MAX_CRASH_REPORT_BYTES + 1)
            .expect("set_len sparse");

        // Malformed single-object JSON must be skipped.
        std::fs::write(dir.path().join("bad.ips"), r#"{"only":"header"}"#).unwrap();

        // Valid report with unsafe process name -> `process: None`.
        let proc_path = sim_proc_path(UDID_A, "DejaFixture");
        write_ips(
            dir.path(),
            "a-valid.ips",
            r#"{"app_name":"../bad/name","bundleID":"dev.deja.ios.checkout"}"#,
            &format!(r#"{{"procName":"../bad/name","procPath":"{proc_path}"}}"#),
        );

        let end = now_ms().saturating_add(5_000);
        let found = find_crash_report(&CrashSearchOptions {
            directory: dir.path().to_path_buf(),
            app_id: "dev.deja.ios.checkout".to_string(),
            device_udid: Some(UDID_A.to_string()),
            since_ms: start,
            until_ms: end,
        })
        .unwrap()
        .expect("valid report should still be found");
        assert_eq!(found.report_file.as_deref(), Some("a-valid.ips"));
        assert_eq!(found.process, None);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_ips_files() {
        use std::os::unix::fs::symlink;

        let dir = TempDirGuard::new("symlink");
        let outside = TempDirGuard::new("symlink-target");
        let start = now_ms().saturating_sub(5_000);
        let proc_path = sim_proc_path(UDID_A, "DejaFixture");
        let real = write_ips(
            outside.path(),
            "real.ips",
            r#"{"app_name":"DejaFixture","bundleID":"dev.deja.ios.checkout"}"#,
            &format!(r#"{{"procName":"DejaFixture","procPath":"{proc_path}"}}"#),
        );
        symlink(&real, dir.path().join("linked.ips")).expect("create symlink");
        let end = now_ms().saturating_add(5_000);

        let found = find_crash_report(&CrashSearchOptions {
            directory: dir.path().to_path_buf(),
            app_id: "dev.deja.ios.checkout".to_string(),
            device_udid: Some(UDID_A.to_string()),
            since_ms: start,
            until_ms: end,
        })
        .unwrap();
        assert!(found.is_none(), "symlink .ips files must never be read");
    }

    #[test]
    fn missing_directory_returns_none_and_respects_time_window() {
        let missing = std::env::temp_dir().join("deja-nonexistent-diagnostic-reports-dir-46");
        assert!(
            find_crash_report(&CrashSearchOptions {
                directory: missing,
                app_id: "dev.deja.ios.checkout".to_string(),
                device_udid: Some(UDID_A.to_string()),
                since_ms: 0,
                until_ms: u64::MAX,
            })
            .unwrap()
            .is_none()
        );

        let dir = TempDirGuard::new("window");
        let proc_path = sim_proc_path(UDID_A, "DejaFixture");
        write_ips(
            dir.path(),
            "old.ips",
            r#"{"app_name":"DejaFixture","bundleID":"dev.deja.ios.checkout"}"#,
            &format!(r#"{{"procName":"DejaFixture","procPath":"{proc_path}"}}"#),
        );
        let future = now_ms().saturating_add(60_000);
        assert!(
            find_crash_report(&CrashSearchOptions {
                directory: dir.path().to_path_buf(),
                app_id: "dev.deja.ios.checkout".to_string(),
                device_udid: Some(UDID_A.to_string()),
                since_ms: future,
                until_ms: future + 10_000,
            })
            .unwrap()
            .is_none()
        );
    }

    #[tokio::test]
    async fn find_crash_report_with_grace_detects_delayed_ips_and_honors_cancel() {
        let dir = TempDirGuard::new("grace");
        let start = now_ms().saturating_sub(1_000);
        let until = now_ms();
        let dir_clone = dir.path().to_path_buf();

        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            let proc_path = sim_proc_path(UDID_A, "DejaFixture");
            write_ips(
                &dir_clone,
                "delayed.ips",
                r#"{"app_name":"DejaFixture","bundleID":"dev.deja.ios.checkout"}"#,
                &format!(r#"{{"procName":"DejaFixture","procPath":"{proc_path}"}}"#),
            );
        });

        let cancel = CancellationToken::new();
        let sig = find_crash_report_with_grace(
            &CrashSearchOptions {
                directory: dir.path().to_path_buf(),
                app_id: "dev.deja.ios.checkout".to_string(),
                device_udid: Some(UDID_A.to_string()),
                since_ms: start,
                until_ms: until,
            },
            Duration::from_millis(800),
            &cancel,
        )
        .await
        .expect("grace scan succeeds")
        .expect("delayed crash report should be discovered within grace window");
        writer.await.expect("writer join");
        assert_eq!(sig.report_file.as_deref(), Some("delayed.ips"));

        // Cancellation returns Cancelled promptly.
        let empty_dir = TempDirGuard::new("grace-cancel");
        let cancel2 = CancellationToken::new();
        cancel2.cancel();
        let err = find_crash_report_with_grace(
            &CrashSearchOptions {
                directory: empty_dir.path().to_path_buf(),
                app_id: "dev.deja.ios.checkout".to_string(),
                device_udid: Some(UDID_A.to_string()),
                since_ms: start,
                until_ms: until,
            },
            Duration::from_secs(5),
            &cancel2,
        )
        .await
        .expect_err("cancelled token must abort grace scan");
        assert!(matches!(err, crate::error::IosDriverError::Cancelled));
    }

    #[test]
    fn helpers_validate_proc_path_and_process_name() {
        assert_eq!(
            extract_simulator_udid_from_proc_path(&sim_proc_path(UDID_A, "App")),
            Some(UDID_A.to_string())
        );
        assert_eq!(
            extract_simulator_udid_from_proc_path(&format!(
                "/CoreSimulator/Devices/{UDID_A}/../escape/App"
            )),
            None
        );
        assert_eq!(
            validate_process_name(Some("DejaFixture 1.0_beta+2-rc")),
            Some("DejaFixture 1.0_beta+2-rc".to_string())
        );
        assert_eq!(validate_process_name(Some("bad/path")), None);
        assert_eq!(validate_process_name(Some(&"a".repeat(65))), None);
    }
}
