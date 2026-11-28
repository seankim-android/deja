//! Scoped Android crash detection via `adb -s <serial> logcat -d -b crash -v epoch`.
//!
//! Matches BOTH ART Java crashes (`FATAL EXCEPTION:` + `Process: <app_id>, PID: <pid>`) and
//! native tombstones (`>>> <app_id> <<<`) strictly within `[since_epoch_ms, until_epoch_ms]`
//! for the exact target `app_id` (or `<app_id>:<subproc>`).

use std::time::Duration;

use deja_core::CrashSignalSource;
use deja_flow::DriverCrashSignal;
use tokio_util::sync::CancellationToken;

use crate::adb::Adb;
use crate::error::{AndroidDriverError, Result};

const GRACE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Parsed single `logcat -v epoch` line.
#[derive(Debug, Clone)]
struct LogcatLine<'a> {
    epoch_ms: u64,
    pid: &'a str,
    tag: &'a str,
    message: &'a str,
}

/// Parses `adb logcat -d -b crash -v epoch` output and returns the newest crash matching
/// `app_id` strictly within `[since_epoch_ms, until_epoch_ms]`.
#[must_use]
pub fn parse_crash_logcat(
    logcat_output: &str,
    app_id: &str,
    since_epoch_ms: u64,
    until_epoch_ms: u64,
) -> Option<DriverCrashSignal> {
    let target = app_id.trim();
    if target.is_empty() || until_epoch_ms < since_epoch_ms {
        return None;
    }

    let mut latest_match: Option<(u64, DriverCrashSignal)> = None;
    // Track recent `FATAL EXCEPTION:` headers by PID so we only attribute `Process: <app_id>, PID:`
    // that belong to a real fatal exception block in the crash buffer.
    let mut active_art_fatal: Option<(u64, String, String)> = None;
    // Track pending tombstone for `app_id` so we can attach the `signal ...` summary line if present.
    let mut pending_tombstone: Option<(u64, String, String)> = None;

    for raw_line in logcat_output.lines() {
        let Some(parsed) = parse_epoch_logcat_line(raw_line) else {
            continue;
        };
        if parsed.epoch_ms < since_epoch_ms || parsed.epoch_ms > until_epoch_ms {
            continue;
        }

        let tag = parsed.tag.trim();
        let msg = parsed.message.trim();

        // 1. ART Java crash (`AndroidRuntime`):
        //    FATAL EXCEPTION: main
        //    Process: com.example.app, PID: 12345
        if tag.eq_ignore_ascii_case("AndroidRuntime") {
            if msg.starts_with("FATAL EXCEPTION:") {
                active_art_fatal = Some((parsed.epoch_ms, parsed.pid.to_string(), msg.to_string()));
                continue;
            }
            if let Some(proc_name) = parse_art_process_line(msg)
                && matches_target_package(proc_name, target)
            {
                let summary = match &active_art_fatal {
                    Some((fatal_ts, fatal_pid, fatal_msg))
                        if fatal_pid == parsed.pid
                            && parsed.epoch_ms.saturating_sub(*fatal_ts) <= 2_000 =>
                    {
                        format!("{fatal_msg}; {msg}")
                    }
                    _ => format!("ART crash: {msg}"),
                };
                let signal = DriverCrashSignal {
                    source: CrashSignalSource::CrashReport,
                    report_file: None,
                    process: Some(proc_name.to_string()),
                    detail: Some(summary),
                };
                if latest_match
                    .as_ref()
                    .is_none_or(|(ts, _)| parsed.epoch_ms >= *ts)
                {
                    latest_match = Some((parsed.epoch_ms, signal));
                }
            }
            continue;
        }

        // 2. Native tombstone (`DEBUG` / `libc` / `tombstoned`):
        //    pid: 1234, tid: 1250, name: RenderThread  >>> com.example.app <<<
        //    signal 11 (SIGSEGV), code 1 (SEGV_MAPERR), fault addr 0x0
        if tag.eq_ignore_ascii_case("DEBUG") || tag.eq_ignore_ascii_case("tombstoned") {
            if let Some(proc_name) = parse_tombstone_process(msg) {
                if matches_target_package(proc_name, target) {
                    pending_tombstone = Some((
                        parsed.epoch_ms,
                        parsed.pid.to_string(),
                        proc_name.to_string(),
                    ));
                    let signal = DriverCrashSignal {
                        source: CrashSignalSource::CrashReport,
                        report_file: None,
                        process: Some(proc_name.to_string()),
                        detail: Some(format!("native tombstone: {msg}")),
                    };
                    if latest_match
                        .as_ref()
                        .is_none_or(|(ts, _)| parsed.epoch_ms >= *ts)
                    {
                        latest_match = Some((parsed.epoch_ms, signal));
                    }
                } else {
                    pending_tombstone = None;
                }
                continue;
            }

            if let Some((tomb_ts, ref tomb_pid, ref tomb_proc)) = pending_tombstone
                && tomb_pid == parsed.pid
                && parsed.epoch_ms.saturating_sub(tomb_ts) <= 2_000
                && msg.starts_with("signal ")
            {
                let signal = DriverCrashSignal {
                    source: CrashSignalSource::CrashReport,
                    report_file: None,
                    process: Some(tomb_proc.clone()),
                    detail: Some(format!("native crash ({tomb_proc}): {msg}")),
                };
                latest_match = Some((parsed.epoch_ms, signal));
            }
        }
    }

    latest_match.map(|(_, sig)| sig)
}

/// Scans `adb -s <serial> logcat -d -b crash -v epoch` immediately, polling across `grace`
/// if no crash is visible yet while racing `cancel`.
pub async fn find_android_crash_with_grace(
    adb: &Adb,
    serial: &str,
    app_id: &str,
    since_epoch_ms: u64,
    until_epoch_ms: u64,
    grace: Duration,
    cancel: &CancellationToken,
) -> Result<Option<DriverCrashSignal>> {
    if cancel.is_cancelled() {
        return Err(AndroidDriverError::Cancelled);
    }
    if app_id.trim().is_empty() {
        return Ok(None);
    }

    let grace_ms = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX);
    let upper_ms = until_epoch_ms.max(since_epoch_ms).saturating_add(grace_ms);
    let deadline = tokio::time::Instant::now() + grace;

    loop {
        if cancel.is_cancelled() {
            return Err(AndroidDriverError::Cancelled);
        }

        let output = adb.logcat_crash_epoch(serial, cancel).await?;
        if let Some(sig) = parse_crash_logcat(&output, app_id, since_epoch_ms, upper_ms) {
            return Ok(Some(sig));
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        let sleep_for = GRACE_POLL_INTERVAL.min(deadline.saturating_duration_since(now));
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AndroidDriverError::Cancelled),
            _ = tokio::time::sleep(sleep_for) => {}
        }
    }
}

fn parse_epoch_logcat_line(line: &str) -> Option<LogcatLine<'_>> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with("--------- ") {
        return None;
    }

    // Format: `<epoch_sec>.<frac> <pid> <tid> <level> <tag>: <message>`
    let (ts_tok, rest) = split_first_token(trimmed)?;
    let epoch_ms = parse_epoch_ms(ts_tok)?;
    let (pid_tok, rest) = split_first_token(rest)?;
    let (_tid_tok, rest) = split_first_token(rest)?;
    let (_level_tok, rest) = split_first_token(rest)?;
    let colon = rest.find(':')?;
    let tag = rest[..colon].trim();
    let message = rest[colon + 1..].trim_start();
    Some(LogcatLine {
        epoch_ms,
        pid: pid_tok,
        tag,
        message,
    })
}

fn split_first_token(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    let idx = s.find(|c: char| c.is_ascii_whitespace())?;
    Some((&s[..idx], s[idx..].trim_start()))
}

fn parse_epoch_ms(token: &str) -> Option<u64> {
    let (sec_str, frac_str) = match token.split_once('.') {
        Some((s, f)) => (s, f),
        None => (token, ""),
    };
    if sec_str.is_empty() || !sec_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let secs = sec_str.parse::<u64>().ok()?;
    let millis: u64 = if frac_str.is_empty() {
        0
    } else if !frac_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    } else {
        let mut padded = [b'0'; 3];
        for (i, b) in frac_str.bytes().take(3).enumerate() {
            padded[i] = b;
        }
        std::str::from_utf8(&padded).ok()?.parse::<u64>().ok()?
    };
    secs.checked_mul(1_000)?.checked_add(millis)
}

fn parse_art_process_line(msg: &str) -> Option<&str> {
    let rest = msg.strip_prefix("Process:")?.trim_start();
    let (proc_part, pid_part) = rest.split_once(", PID:")?;
    let proc_name = proc_part.trim();
    if proc_name.is_empty() || pid_part.trim().is_empty() {
        return None;
    }
    Some(proc_name)
}

fn parse_tombstone_process(msg: &str) -> Option<&str> {
    let start = msg.find(">>> ")? + 4;
    let tail = &msg[start..];
    let end = tail.find(" <<<")?;
    let proc_name = tail[..end].trim();
    (!proc_name.is_empty()).then_some(proc_name)
}

fn matches_target_package(process_name: &str, app_id: &str) -> bool {
    if process_name == app_id {
        return true;
    }
    process_name
        .strip_prefix(app_id)
        .is_some_and(|suffix| suffix.starts_with(':') && suffix.len() > 1)
}

#[cfg(test)]
mod tests {
    use super::parse_crash_logcat;
    use deja_core::CrashSignalSource;

    const SAMPLE_LOGCAT: &str = "\
--------- beginning of crash
         1700000000.100  1111  1111 E AndroidRuntime: FATAL EXCEPTION: main
         1700000000.101  1111  1111 E AndroidRuntime: Process: com.example.other, PID: 1111
         1700000005.200  2222  2222 E AndroidRuntime: FATAL EXCEPTION: main
         1700000005.201  2222  2222 E AndroidRuntime: Process: com.example.app.extra, PID: 2222
         1700000010.500  3333  3333 E AndroidRuntime: FATAL EXCEPTION: main
         1700000010.501  3333  3333 E AndroidRuntime: Process: com.example.app, PID: 3333
         1700000010.502  3333  3333 E AndroidRuntime: java.lang.NullPointerException: boom
         1700000020.750   999   999 F DEBUG   : *** *** *** *** *** *** *** ***
         1700000020.751   999   999 F DEBUG   : pid: 4444, tid: 4450, name: RenderThread  >>> com.example.nativeapp <<<
         1700000020.752   999   999 F DEBUG   : signal 11 (SIGSEGV), code 1 (SEGV_MAPERR), fault addr 0x0
";

    #[test]
    fn matches_art_crash_within_window_and_rejects_other_packages_and_prefixes() {
        // Stale window before 1700000010.500 finds nothing.
        assert!(
            parse_crash_logcat(
                SAMPLE_LOGCAT,
                "com.example.app",
                1_700_000_000_000,
                1_700_000_009_999
            )
            .is_none()
        );

        // Exact window matches com.example.app ART crash.
        let sig = parse_crash_logcat(
            SAMPLE_LOGCAT,
            "com.example.app",
            1_700_000_010_000,
            1_700_000_015_000,
        )
        .expect("ART crash must match");
        assert_eq!(sig.source, CrashSignalSource::CrashReport);
        assert_eq!(sig.process.as_deref(), Some("com.example.app"));
        assert!(
            sig.detail
                .as_deref()
                .unwrap_or("")
                .contains("FATAL EXCEPTION: main")
        );

        // Window after the crash finds nothing.
        assert!(
            parse_crash_logcat(
                SAMPLE_LOGCAT,
                "com.example.app",
                1_700_000_011_000,
                1_700_000_030_000
            )
            .is_none()
        );
    }

    #[test]
    fn matches_native_tombstone_with_signal_detail() {
        let sig = parse_crash_logcat(
            SAMPLE_LOGCAT,
            "com.example.nativeapp",
            1_700_000_019_000,
            1_700_000_025_000,
        )
        .expect("native tombstone must match");
        assert_eq!(sig.source, CrashSignalSource::CrashReport);
        assert_eq!(sig.process.as_deref(), Some("com.example.nativeapp"));
        assert!(sig.detail.as_deref().unwrap_or("").contains("SIGSEGV"));
    }

    #[test]
    fn matches_colon_subprocess_but_not_dot_prefix_package() {
        let subproc_log = "\
         1700000030.100  5555  5555 E AndroidRuntime: FATAL EXCEPTION: worker
         1700000030.101  5555  5555 E AndroidRuntime: Process: com.example.app:remote, PID: 5555
";
        let sig = parse_crash_logcat(
            subproc_log,
            "com.example.app",
            1_700_000_029_000,
            1_700_000_035_000,
        )
        .expect("subprocess :remote should match target app");
        assert_eq!(sig.process.as_deref(), Some("com.example.app:remote"));
    }
}
