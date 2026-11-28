//! Zero-`unsafe` single-writer locking, store path validation, canonical workspace confinement,
//! and timestamp/token utilities.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::debug;

use crate::error::StoreError;

/// Default workspace-relative path for the unified SQLite v5 walk store.
pub const SQLITE_MEMORY_PATH: &str = ".deja/memory/walks.sqlite";

/// Default workspace-relative path for the legacy JSON walk store.
pub const LEGACY_JSON_MEMORY_PATH: &str = ".deja/memory/walks.json";

/// An unreadable `.break` file older than 10 seconds is treated as a crash leftover.
const UNREADABLE_BREAKER_STALE_MS: u128 = 10_000;

/// Maximum attempts to acquire `.lock` before reporting contention.
const MAX_LOCK_ATTEMPTS: usize = 4;

/// Maximum attempts to acquire `.lock.break` during stale-lock takeover.
const MAX_BREAKER_ATTEMPTS: usize = 5;

/// Delay while waiting for another live process taking over a stale lock.
const BREAKER_WAIT: Duration = Duration::from_millis(40);

static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Metadata written inside a `.lock` or `.lock.break` file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockHolder {
    pub version: u32,
    pub pid: u32,
    pub started_at: Option<String>,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    pub token: String,
    pub acquired_at: String,
}

/// Optional test hook invoked immediately before `remove_observed` renames a lock or breaker aside.
pub type BeforeMoveAsideHook = Arc<dyn Fn(&Path) + Send + Sync>;

/// Canonicalized metadata path checked for workspace confinement and symlink safety.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalMetadataTarget {
    pub canonical_root: PathBuf,
    pub path: PathBuf,
    pub exists: bool,
}

/// Validate a `--store <id>` identifier: 1..=128 ASCII `[A-Za-z0-9._-]`, starting with `[A-Za-z0-9]`.
pub fn validate_store_id(id: &str) -> Result<(), StoreError> {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 {
        return Err(StoreError::InvalidStoreId(id.to_string()));
    }
    if !bytes[0].is_ascii_alphanumeric() {
        return Err(StoreError::InvalidStoreId(id.to_string()));
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(StoreError::InvalidStoreId(id.to_string()));
    }
    Ok(())
}

/// Resolve the workspace-relative metadata path for an optional `store_id`.
///
/// Without `store_id`, returns `default_path` unchanged (e.g. `.deja/memory/walks.sqlite`).
/// With `store_id`, places history under `.deja/memory/stores/<id>/walks.sqlite`.
pub fn store_metadata_path(
    default_path: &str,
    store_id: Option<&str>,
) -> Result<String, StoreError> {
    let Some(id) = store_id else {
        return Ok(default_path.to_string());
    };
    validate_store_id(id)?;
    match default_path.rfind('/') {
        Some(slash) => Ok(format!(
            "{}/stores/{}{}",
            &default_path[..slash],
            id,
            &default_path[slash..]
        )),
        None => Ok(format!("stores/{id}/{default_path}")),
    }
}

pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

pub(crate) fn path_inside(root: &Path, candidate: &Path) -> bool {
    candidate != root && candidate.starts_with(root)
}

fn sidecar_path(base: &Path, suffix: &str) -> PathBuf {
    let mut os = OsString::from(base.as_os_str());
    os.push(suffix);
    PathBuf::from(os)
}

fn check_sqlite_sidecars(target: &Path, canonical_root: &Path) -> Result<(), StoreError> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = sidecar_path(target, suffix);
        match fs::symlink_metadata(&sidecar) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(StoreError::InvalidWalkMemory(format!(
                        "SQLite sidecar file is a symbolic link: {}",
                        sidecar.display()
                    )));
                }
                if !meta.file_type().is_file() {
                    return Err(StoreError::InvalidWalkMemory(format!(
                        "SQLite sidecar path is not a regular file: {}",
                        sidecar.display()
                    )));
                }
                let canonical = fs::canonicalize(&sidecar).map_err(|err| {
                    StoreError::InvalidWalkMemory(format!(
                        "SQLite sidecar cannot be resolved: {}: {err}",
                        sidecar.display()
                    ))
                })?;
                if !path_inside(canonical_root, &canonical) {
                    return Err(StoreError::InvalidWalkMemory(format!(
                        "SQLite sidecar escapes workspace: {}",
                        sidecar.display()
                    )));
                }
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) if is_enotdir(&err) => {
                return Err(StoreError::InvalidWalkMemory(format!(
                    "SQLite sidecar traverses a non-directory: {}",
                    sidecar.display()
                )));
            }
            Err(err) => {
                return Err(StoreError::InvalidWalkMemory(format!(
                    "SQLite sidecar cannot be inspected: {}: {err}",
                    sidecar.display()
                )));
            }
        }
    }
    Ok(())
}

fn is_enotdir(err: &io::Error) -> bool {
    if err.kind() == ErrorKind::NotADirectory {
        return true;
    }
    #[cfg(unix)]
    {
        if err.raw_os_error() == Some(libc::ENOTDIR) {
            return true;
        }
    }
    false
}

pub(crate) fn is_eloop(err: &io::Error) -> bool {
    #[cfg(unix)]
    {
        if err.raw_os_error() == Some(libc::ELOOP) {
            return true;
        }
    }
    let _ = err;
    false
}

/// Resolve a store metadata path safely inside `workspace_root` without following unchecked
/// paths outside the canonical workspace.
///
/// Rejects:
/// - non-directory workspace roots or non-directory intermediate segments (`ENOTDIR`),
/// - workspace escapes (lexical or via symlinked ancestor directories),
/// - symlinked metadata files themselves, and
/// - symlinked or non-regular SQLite `-wal`, `-shm`, and `-journal` sidecars.
pub fn canonical_metadata_target(
    workspace_root: &Path,
    memory_path: &Path,
) -> Result<CanonicalMetadataTarget, StoreError> {
    let canonical_root = fs::canonicalize(workspace_root).map_err(|err| {
        StoreError::InvalidWalkMemory(format!(
            "workspace root cannot be resolved ({}): {err}",
            workspace_root.display()
        ))
    })?;
    let root_meta = fs::metadata(&canonical_root).map_err(|err| {
        StoreError::InvalidWalkMemory(format!("workspace root cannot be read: {err}"))
    })?;
    if !root_meta.is_dir() {
        return Err(StoreError::InvalidWalkMemory(
            "workspace root is not a directory".to_string(),
        ));
    }

    let lexical_root = if workspace_root.is_absolute() {
        lexical_normalize(workspace_root)
    } else {
        let cwd = std::env::current_dir().map_err(|err| {
            StoreError::InvalidWalkMemory(format!("current directory unavailable: {err}"))
        })?;
        lexical_normalize(&cwd.join(workspace_root))
    };

    let candidate = if memory_path.is_relative() {
        let normalized_mem = lexical_normalize(memory_path);
        if !workspace_root.is_absolute()
            && let Ok(cwd) = std::env::current_dir()
            && let Ok(rel) =
                lexical_normalize(&cwd.join(&normalized_mem)).strip_prefix(&lexical_root)
            && !rel.as_os_str().is_empty()
        {
            lexical_normalize(&canonical_root.join(rel))
        } else {
            lexical_normalize(&canonical_root.join(&normalized_mem))
        }
    } else {
        let normalized_mem = lexical_normalize(memory_path);
        if let Ok(rel) = normalized_mem.strip_prefix(&canonical_root) {
            lexical_normalize(&canonical_root.join(rel))
        } else if let Ok(rel) = normalized_mem.strip_prefix(&lexical_root) {
            lexical_normalize(&canonical_root.join(rel))
        } else {
            return Err(StoreError::InvalidWalkMemory(format!(
                "metadata path escapes workspace: {}",
                memory_path.display()
            )));
        }
    };

    if !path_inside(&canonical_root, &candidate) {
        return Err(StoreError::InvalidWalkMemory(format!(
            "metadata path escapes workspace: {}",
            memory_path.display()
        )));
    }

    // Walk ancestors up to canonical_root to locate the nearest existing filesystem entry.
    let mut ancestor = candidate.clone();
    let ancestor_lmeta = loop {
        match fs::symlink_metadata(&ancestor) {
            Ok(meta) => break meta,
            Err(err) if is_enotdir(&err) => {
                return Err(StoreError::InvalidWalkMemory(
                    "metadata path traverses a non-directory".to_string(),
                ));
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                let Some(parent) = ancestor.parent() else {
                    return Err(StoreError::InvalidWalkMemory(
                        "metadata path has no safe existing parent".to_string(),
                    ));
                };
                if parent == ancestor
                    || (parent != canonical_root && !path_inside(&canonical_root, parent))
                {
                    return Err(StoreError::InvalidWalkMemory(
                        "metadata path has no safe existing parent".to_string(),
                    ));
                }
                ancestor = parent.to_path_buf();
            }
            Err(err) => {
                return Err(StoreError::InvalidWalkMemory(format!(
                    "metadata path cannot be resolved safely: {err}"
                )));
            }
        }
    };

    if ancestor == candidate {
        if ancestor_lmeta.file_type().is_symlink() {
            return Err(StoreError::InvalidWalkMemory(format!(
                "metadata target is a symbolic link: {}",
                candidate.display()
            )));
        }
        if !ancestor_lmeta.file_type().is_file() {
            return Err(StoreError::InvalidWalkMemory(
                "metadata target is not a regular file".to_string(),
            ));
        }
        let canonical_target = fs::canonicalize(&ancestor).map_err(|err| {
            StoreError::InvalidWalkMemory(format!("metadata target cannot be resolved: {err}"))
        })?;
        if !path_inside(&canonical_root, &canonical_target) {
            return Err(StoreError::InvalidWalkMemory(
                "metadata path escapes workspace".to_string(),
            ));
        }
        let target_meta = fs::metadata(&canonical_target).map_err(|err| {
            StoreError::InvalidWalkMemory(format!("metadata target cannot be read: {err}"))
        })?;
        if !target_meta.is_file() {
            return Err(StoreError::InvalidWalkMemory(
                "metadata target is not a regular file".to_string(),
            ));
        }
        check_sqlite_sidecars(&canonical_target, &canonical_root)?;
        return Ok(CanonicalMetadataTarget {
            canonical_root,
            path: canonical_target,
            exists: true,
        });
    }

    let canonical_ancestor = fs::canonicalize(&ancestor).map_err(|err| {
        StoreError::InvalidWalkMemory(format!("metadata ancestor cannot be resolved: {err}"))
    })?;
    if canonical_ancestor != canonical_root && !path_inside(&canonical_root, &canonical_ancestor) {
        return Err(StoreError::InvalidWalkMemory(
            "metadata path escapes workspace".to_string(),
        ));
    }
    let ancestor_meta = fs::metadata(&canonical_ancestor).map_err(|err| {
        StoreError::InvalidWalkMemory(format!("metadata ancestor cannot be read: {err}"))
    })?;
    if !ancestor_meta.is_dir() {
        return Err(StoreError::InvalidWalkMemory(
            "metadata path traverses a non-directory".to_string(),
        ));
    }

    let rel = candidate.strip_prefix(&ancestor).map_err(|_| {
        StoreError::InvalidWalkMemory("metadata path cannot be projected safely".to_string())
    })?;
    let projected = lexical_normalize(&canonical_ancestor.join(rel));
    if !path_inside(&canonical_root, &projected) {
        return Err(StoreError::InvalidWalkMemory(
            "metadata path escapes workspace".to_string(),
        ));
    }
    check_sqlite_sidecars(&projected, &canonical_root)?;

    Ok(CanonicalMetadataTarget {
        canonical_root,
        path: projected,
        exists: false,
    })
}

/// Safe `O_NOFOLLOW` file open for reading (zero `unsafe`).
pub(crate) fn open_read_nofollow(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "refusing to follow symbolic link",
            ));
        }
        File::open(path)
    }
}

/// Safe `O_CREAT | O_EXCL | O_NOFOLLOW` file creation (zero `unsafe`).
pub(crate) fn create_new_nofollow(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new().write(true).create_new(true).open(path)
    }
}

/// Generate a unique 32-hex-character token using OS entropy mixed with PID, time, and counter.
#[must_use]
pub(crate) fn generate_token() -> String {
    let mut hasher = Sha256::new();
    let seq = TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(seq.to_le_bytes());
    if let Ok(dur) = SystemTime::now().duration_since(UNIX_EPOCH) {
        hasher.update(dur.as_nanos().to_le_bytes());
    }
    let mut entropy = [0u8; 16];
    if let Ok(mut urandom) = File::open("/dev/urandom")
        && urandom.read_exact(&mut entropy).is_ok()
    {
        hasher.update(entropy);
    }
    let digest = hasher.finalize();
    hex::encode(&digest[..16])
}

/// Return current UTC wall-clock time formatted as RFC 3339 / ISO-8601 (`YYYY-MM-DDTHH:MM:SS.mmmZ`).
#[must_use]
pub fn now_iso8601() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let millis = dur.subsec_millis();
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant's civil-from-days algorithm (days since 1970-01-01 -> Gregorian `(y, m, d)`).
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(m <= 2);
    (year, m as u32, d as u32)
}

fn is_leap_year(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn parse_ascii_digits(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse::<u32>().ok()
}

/// Strictly validate an ISO-8601 / RFC 3339 timestamp string (`YYYY-MM-DDTHH:MM:SS[.fff](Z|±HH:MM)`).
#[must_use]
pub fn is_valid_iso8601(value: &str) -> bool {
    let b = value.as_bytes();
    if b.len() < 20 {
        return false;
    }
    if b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return false;
    }
    let Some(year) = parse_ascii_digits(&b[0..4]) else {
        return false;
    };
    let Some(month) = parse_ascii_digits(&b[5..7]) else {
        return false;
    };
    let Some(day) = parse_ascii_digits(&b[8..10]) else {
        return false;
    };
    let Some(hour) = parse_ascii_digits(&b[11..13]) else {
        return false;
    };
    let Some(minute) = parse_ascii_digits(&b[14..16]) else {
        return false;
    };
    let Some(second) = parse_ascii_digits(&b[17..19]) else {
        return false;
    };
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return false;
    }

    let mut pos = 19;
    if pos < b.len() && b[pos] == b'.' {
        pos += 1;
        let frac_start = pos;
        while pos < b.len() && b[pos].is_ascii_digit() {
            pos += 1;
        }
        let frac_len = pos - frac_start;
        if frac_len == 0 || frac_len > 9 {
            return false;
        }
    }

    if pos >= b.len() {
        return false;
    }
    match b[pos] {
        b'Z' | b'z' => pos + 1 == b.len(),
        b'+' | b'-' => {
            if b.len() != pos + 6 || b[pos + 3] != b':' {
                return false;
            }
            let Some(off_h) = parse_ascii_digits(&b[pos + 1..pos + 3]) else {
                return false;
            };
            let Some(off_m) = parse_ascii_digits(&b[pos + 4..pos + 6]) else {
                return false;
            };
            off_h <= 23 && off_m <= 59
        }
        _ => false,
    }
}

/// Extract field 22 (process start time in clock ticks since boot) from a Linux `/proc/<pid>/stat` line.
///
/// Because field 2 (the executable name) sits inside parentheses and may contain spaces or
/// parentheses, parsing always begins after the *last* `')'`.
#[must_use]
pub fn parse_proc_start_ticks(stat: &str) -> Option<String> {
    let end = stat.rfind(')')?;
    if stat.as_bytes().get(end + 1) != Some(&b' ') {
        return None;
    }
    let after = stat.get(end + 2..)?;
    let ticks = after.split(' ').nth(22 - 3)?;
    if !ticks.is_empty() && ticks.bytes().all(|b| b.is_ascii_digit()) {
        Some(ticks.to_string())
    } else {
        None
    }
}

fn run_command_fallback(
    paths: &[&str],
    args: &[&str],
    envs: &[(&str, &str)],
) -> Option<std::process::Output> {
    for &program in paths {
        let mut cmd = Command::new(program);
        cmd.args(args);
        for &(k, v) in envs {
            cmd.env(k, v);
        }
        match cmd.output() {
            Ok(out) => return Some(out),
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(_) => return None,
        }
    }
    None
}

/// Read the start time identifier of `pid`, or `None` when it cannot be determined safely.
#[must_use]
pub fn process_start(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some(ticks) = parse_proc_start_ticks(&stat)
        {
            return Some(format!("proc:{ticks}"));
        }
    }
    let pid_str = pid.to_string();
    let out = run_command_fallback(
        &["/bin/ps", "ps"],
        &["-o", "lstart=", "-p", &pid_str],
        &[("LC_ALL", "C"), ("TZ", "UTC0")],
    )?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn own_process_start() -> Option<String> {
    static OWN: OnceLock<Option<String>> = OnceLock::new();
    OWN.get_or_init(|| process_start(std::process::id()))
        .clone()
}

/// Return a stable machine identifier when available (`/etc/machine-id` on Linux,
/// `IOPlatformUUID` via `ioreg` on macOS).
#[must_use]
pub fn machine_id() -> Option<String> {
    static ID: OnceLock<Option<String>> = OnceLock::new();
    ID.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            if let Ok(raw) = fs::read_to_string("/etc/machine-id") {
                let trimmed = raw.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
            None
        }
        #[cfg(target_os = "macos")]
        {
            let out = run_command_fallback(
                &["/usr/sbin/ioreg", "ioreg"],
                &["-rd1", "-c", "IOPlatformExpertDevice"],
                &[("LC_ALL", "C")],
            )?;
            if !out.status.success() {
                return None;
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                if let Some(idx) = line.find("\"IOPlatformUUID\"") {
                    let rest = &line[idx + "\"IOPlatformUUID\"".len()..];
                    if let Some(eq) = rest.find('=') {
                        let quoted = rest[eq + 1..].trim();
                        if let Some(inner) =
                            quoted.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
                            && !inner.is_empty()
                        {
                            return Some(inner.to_string());
                        }
                    }
                }
            }
            None
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            None
        }
    })
    .clone()
}

/// Return the local hostname without `unsafe` FFI.
#[must_use]
pub fn current_hostname() -> String {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            if let Ok(raw) = fs::read_to_string("/etc/hostname") {
                let trimmed = raw.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
        if let Some(out) = run_command_fallback(&["/bin/hostname", "hostname"], &[], &[])
            && out.status.success()
        {
            let trimmed = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
        std::env::var("HOSTNAME")
            .ok()
            .filter(|h| !h.trim().is_empty())
            .unwrap_or_else(|| "unknown-host".to_string())
    })
    .clone()
}

fn same_machine(holder: &LockHolder) -> bool {
    if let (Some(mine), Some(theirs)) = (machine_id(), holder.machine_id.as_deref())
        && !theirs.is_empty()
    {
        return mine == theirs;
    }
    holder.host == current_hostname()
}

/// Probe process liveness using `/bin/kill -0 <pid>` with zero `unsafe`.
///
/// Returns `Some(true)` only when the OS confirms `No such process` (ESRCH);
/// `Some(false)` when alive or `Operation not permitted` (EPERM); `None` when uncertain.
fn is_pid_definitely_dead(pid: u32) -> Option<bool> {
    if pid == 0 {
        return Some(false);
    }
    let pid_str = pid.to_string();
    let out = run_command_fallback(
        &["/bin/kill", "kill"],
        &["-0", &pid_str],
        &[("LC_ALL", "C")],
    )?;
    if out.status.success() {
        return Some(false);
    }
    let stderr = String::from_utf8_lossy(&out.stderr).to_ascii_lowercase();
    if stderr.contains("no such process") {
        return Some(true);
    }
    if stderr.contains("operation not permitted") || stderr.contains("not permitted") {
        return Some(false);
    }
    None
}

/// Returns `true` only when `holder` is provably dead on this machine:
/// - `pid > 0`
/// - same machine (stable `machine_id` when both present, otherwise hostname)
/// - `pid` does not exist (`ESRCH`), OR `started_at` is present and differs from the running pid's start time.
///
/// Fails closed (`false`) on different machines, `pid == 0`, permission errors (`EPERM`), or unknown start times.
#[must_use]
pub fn is_stale(holder: &LockHolder) -> bool {
    if holder.pid == 0 || !same_machine(holder) {
        return false;
    }
    match is_pid_definitely_dead(holder.pid) {
        Some(true) => return true,
        None => return false,
        Some(false) => {}
    }
    let Some(ref recorded_start) = holder.started_at else {
        return false;
    };
    let current = if holder.pid == std::process::id() {
        own_process_start()
    } else {
        process_start(holder.pid)
    };
    current.is_some_and(|now_start| &now_start != recorded_start)
}

fn parse_holder(text: &str) -> Option<LockHolder> {
    let holder: LockHolder = serde_json::from_str(text).ok()?;
    if holder.version != 1
        || holder.host.is_empty()
        || holder.token.is_empty()
        || holder.acquired_at.is_empty()
    {
        return None;
    }
    Some(holder)
}

#[derive(Debug)]
struct ReadHolderResult {
    holder: Option<LockHolder>,
    exists: bool,
}

fn read_holder(path: &Path) -> Result<ReadHolderResult, StoreError> {
    let mut file = match open_read_nofollow(path) {
        Ok(f) => f,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Ok(ReadHolderResult {
                holder: None,
                exists: false,
            });
        }
        Err(err) if is_eloop(&err) => {
            return Err(StoreError::StoreLockUnavailable {
                lock_path: path.display().to_string(),
                message: "lock path is a symbolic link".to_string(),
            });
        }
        Err(err) => {
            return Err(StoreError::StoreLockUnavailable {
                lock_path: path.display().to_string(),
                message: err.to_string(),
            });
        }
    };
    let meta = file
        .metadata()
        .map_err(|err| StoreError::StoreLockUnavailable {
            lock_path: path.display().to_string(),
            message: err.to_string(),
        })?;
    if !meta.is_file() {
        return Err(StoreError::StoreLockUnavailable {
            lock_path: path.display().to_string(),
            message: "lock path is not a regular file".to_string(),
        });
    }
    // Bound lock file reads to 64 KiB to prevent unbounded allocation on corrupt files.
    let mut buf = String::new();
    let mut limited = Read::by_ref(&mut file).take(65_536);
    if limited.read_to_string(&mut buf).is_err() {
        return Ok(ReadHolderResult {
            holder: None,
            exists: true,
        });
    }
    Ok(ReadHolderResult {
        holder: parse_holder(&buf),
        exists: true,
    })
}

fn new_holder() -> LockHolder {
    LockHolder {
        version: 1,
        pid: std::process::id(),
        started_at: own_process_start(),
        host: current_hostname(),
        machine_id: machine_id(),
        token: generate_token(),
        acquired_at: now_iso8601(),
    }
}

/// Atomically publish `holder` at `target` via `create_new(O_NOFOLLOW)` temp file + `hard_link`.
/// Returns `Ok(true)` if published, `Ok(false)` if `target` already exists.
fn publish(target: &Path, holder: &LockHolder, unavailable_for: &Path) -> Result<bool, StoreError> {
    let temp = sidecar_path(
        target,
        &format!(".{}.{}.tmp", std::process::id(), holder.token),
    );
    let payload = format!(
        "{}\n",
        serde_json::to_string(holder).expect("LockHolder serializes to JSON")
    );
    let write_res = (|| -> io::Result<()> {
        let mut file = create_new_nofollow(&temp)?;
        file.write_all(payload.as_bytes())?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = fs::remove_file(&temp);
        return Err(StoreError::StoreLockUnavailable {
            lock_path: unavailable_for.display().to_string(),
            message: err.to_string(),
        });
    }

    let link_res = fs::hard_link(&temp, target);
    let _ = fs::remove_file(&temp);
    match link_res {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(StoreError::StoreLockUnavailable {
            lock_path: unavailable_for.display().to_string(),
            message: err.to_string(),
        }),
    }
}

/// Atomically move `path` aside to `<path>.aside.<pid>.<token>`, verify its token matches
/// `observed`, and unlink only on an exact match. If another process replaced `path` before our
/// rename, restore it via `hard_link` and return `Ok(false)`.
fn remove_observed(
    path: &Path,
    observed: Option<&LockHolder>,
    hook: Option<&BeforeMoveAsideHook>,
) -> Result<bool, StoreError> {
    if let Some(h) = hook {
        h(path);
    }
    // Never rename/remove a symlinked lock file.
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(StoreError::StoreLockUnavailable {
                lock_path: path.display().to_string(),
                message: "lock path is a symbolic link".to_string(),
            });
        }
        Ok(_) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(true),
        Err(err) => {
            return Err(StoreError::StoreLockUnavailable {
                lock_path: path.display().to_string(),
                message: err.to_string(),
            });
        }
    }

    let aside = sidecar_path(
        path,
        &format!(".aside.{}.{}", std::process::id(), generate_token()),
    );
    match fs::rename(path, &aside) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(true),
        Err(err) => {
            return Err(StoreError::StoreLockUnavailable {
                lock_path: path.display().to_string(),
                message: err.to_string(),
            });
        }
    }

    let moved = read_holder(&aside)?;
    let same_file = match observed {
        Some(obs) => moved
            .holder
            .as_ref()
            .is_some_and(|actual| actual.token == obs.token),
        None => moved.holder.is_none(),
    };
    if same_file {
        let _ = fs::remove_file(&aside);
        return Ok(true);
    }

    // We moved a newer holder's file! Restore it via exclusive hard_link if the slot is free.
    let _ = fs::hard_link(&aside, path);
    let _ = fs::remove_file(&aside);
    Ok(false)
}

struct BreakerGuard {
    path: PathBuf,
    token: String,
}

impl Drop for BreakerGuard {
    fn drop(&mut self) {
        if let Ok(current) = read_holder(&self.path)
            && current
                .holder
                .as_ref()
                .is_some_and(|h| h.token == self.token)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Zero-`unsafe` single-writer lock backed by atomic `O_NOFOLLOW` + `hard_link` publication.
///
/// Re-verifies ownership before commits via [`WriterLock::verify_held`] and releases the lock
/// file on [`WriterLock::release`] or [`Drop`] *only* when the on-disk token still matches.
#[derive(Debug)]
pub struct WriterLock {
    path: PathBuf,
    token: Option<String>,
}

impl WriterLock {
    /// Acquire the writer lock at `lock_path`, taking over a provably stale lock atomically.
    pub fn acquire(lock_path: PathBuf) -> Result<Self, StoreError> {
        Self::acquire_with_hook(lock_path, None)
    }

    /// Acquire the writer lock with an optional deterministic race hook for tests.
    pub fn acquire_with_hook(
        lock_path: PathBuf,
        hook: Option<BeforeMoveAsideHook>,
    ) -> Result<Self, StoreError> {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|err| StoreError::StoreLockUnavailable {
                lock_path: lock_path.display().to_string(),
                message: err.to_string(),
            })?;
        }

        // Refuse immediately if lock_path itself is already a symlink.
        if let Ok(meta) = fs::symlink_metadata(&lock_path)
            && meta.file_type().is_symlink()
        {
            return Err(StoreError::StoreLockUnavailable {
                lock_path: lock_path.display().to_string(),
                message: "lock path is a symbolic link".to_string(),
            });
        }

        let holder = new_holder();
        for _ in 0..MAX_LOCK_ATTEMPTS {
            if publish(&lock_path, &holder, &lock_path)? {
                debug!(path = %lock_path.display(), token = %holder.token, "acquired writer lock");
                return Ok(Self {
                    path: lock_path,
                    token: Some(holder.token),
                });
            }

            let current = read_holder(&lock_path)?;
            if !current.exists {
                continue;
            }
            let Some(existing) = current.holder else {
                return Err(StoreError::StoreLocked {
                    lock_path: lock_path.display().to_string(),
                    holder: None,
                    reason: "an unreadable lock file".to_string(),
                });
            };
            if !is_stale(&existing) {
                let reason = format!(
                    "process {} on {} (since {})",
                    existing.pid, existing.host, existing.acquired_at
                );
                return Err(StoreError::StoreLocked {
                    lock_path: lock_path.display().to_string(),
                    holder: Some(Box::new(existing)),
                    reason,
                });
            }
            break_stale(&lock_path, &existing, hook.as_ref())?;
        }

        Err(StoreError::StoreLockContention {
            lock_path: lock_path.display().to_string(),
            holder: None,
            reason: "the lock kept changing".to_string(),
        })
    }

    /// Path of the `.lock` file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Unique token held by this lock instance, or `None` after release.
    #[must_use]
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Re-verify that the on-disk `.lock` file still belongs to this writer instance.
    ///
    /// Must be called before starting and before committing every write transaction.
    pub fn verify_held(&self) -> Result<(), StoreError> {
        let Some(ref expected) = self.token else {
            return Err(StoreError::StoreLocked {
                lock_path: self.path.display().to_string(),
                holder: None,
                reason: "the lock has already been released".to_string(),
            });
        };
        let current = read_holder(&self.path)?;
        if current
            .holder
            .as_ref()
            .is_some_and(|h| &h.token == expected)
        {
            return Ok(());
        }
        Err(StoreError::StoreLocked {
            lock_path: self.path.display().to_string(),
            holder: current.holder.map(Box::new),
            reason: "the lock was taken from this process".to_string(),
        })
    }

    /// Release the lock file if and only if the on-disk token still matches ours.
    pub fn release(&mut self) {
        let Some(expected) = self.token.take() else {
            return;
        };
        if let Ok(current) = read_holder(&self.path)
            && current.holder.as_ref().is_some_and(|h| h.token == expected)
        {
            let _ = fs::remove_file(&self.path);
            debug!(path = %self.path.display(), "released writer lock");
        }
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        self.release();
    }
}

fn break_stale(
    lock_path: &Path,
    stale: &LockHolder,
    hook: Option<&BeforeMoveAsideHook>,
) -> Result<(), StoreError> {
    let breaker = sidecar_path(lock_path, ".break");
    let mine = new_holder();
    let mut acquired = false;

    for _ in 0..MAX_BREAKER_ATTEMPTS {
        if publish(&breaker, &mine, lock_path)? {
            acquired = true;
            break;
        }
        let other = read_holder(&breaker)?;
        if !other.exists {
            continue;
        }
        let unreadable_and_old = other.holder.is_none()
            && fs::symlink_metadata(&breaker)
                .ok()
                .filter(|m| !m.file_type().is_symlink())
                .and_then(|m| m.modified().ok())
                .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
                .is_some_and(|age| age.as_millis() > UNREADABLE_BREAKER_STALE_MS);

        if other.holder.as_ref().is_some_and(is_stale) || unreadable_and_old {
            let removed = remove_observed(&breaker, other.holder.as_ref(), hook)?;
            if !removed {
                return Err(StoreError::StoreLockContention {
                    lock_path: lock_path.display().to_string(),
                    holder: Some(Box::new(stale.clone())),
                    reason: "breaker changed during stale takeover".to_string(),
                });
            }
            continue;
        }
        thread::sleep(BREAKER_WAIT);
    }

    if !acquired {
        return Err(StoreError::StoreLockContention {
            lock_path: lock_path.display().to_string(),
            holder: Some(Box::new(stale.clone())),
            reason: "another process is taking over the stale lock".to_string(),
        });
    }

    let _guard = BreakerGuard {
        path: breaker,
        token: mine.token,
    };

    let again = read_holder(lock_path)?;
    if let Some(actual) = again.holder
        && actual.token == stale.token
        && is_stale(&actual)
    {
        let removed = remove_observed(lock_path, Some(&actual), hook)?;
        if !removed {
            return Err(StoreError::StoreLockContention {
                lock_path: lock_path.display().to_string(),
                holder: Some(Box::new(stale.clone())),
                reason: "lock was replaced during stale takeover".to_string(),
            });
        }
    }

    Ok(())
}

/// Read the current holder of `lock_path` without following symlinks (diagnostic/test helper).
pub fn inspect_lock_holder(lock_path: &Path) -> Result<Option<LockHolder>, StoreError> {
    Ok(read_holder(lock_path)?.holder)
}
