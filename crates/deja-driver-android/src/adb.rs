//! Native `adb` wrapper: fail-closed binary discovery and `ro.serialno` pinning,
//! rotation-aware display bounds (`DEJA-85`), cached wake/keyguard lock guard,
//! `AnimationScaleGuard`, `AdbForwardGuard`, and Android app lifecycle commands.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deja_core::LaunchArgumentValue;
use deja_flow::UiSize;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::cache::is_executable_file;
use crate::error::{AndroidDriverError, Result};
use crate::process::{apply_adb_env, apply_adb_env_std, run_sync_command_bounded, validate_serial};

const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const DROP_ADB_TIMEOUT: Duration = Duration::from_millis(800);
const ANIMATION_SCALE_KEYS: [&str; 3] = [
    "window_animation_scale",
    "transition_animation_scale",
    "animator_duration_scale",
];

static MEDIA_NONCE: AtomicU64 = AtomicU64::new(0);

/// Parsed device line from `adb devices -l`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdbDevice {
    pub serial: String,
    pub state: String,
    pub product: Option<String>,
    pub model: Option<String>,
    pub device: Option<String>,
    pub transport_id: Option<String>,
}

/// Power wakefulness parsed from `dumpsys power`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wakefulness {
    Awake,
    Dozing,
    Asleep,
    Unknown,
}

/// Discovers the `adb` executable following strict precedence:
///
/// 1. `DEJA_ADB` (if set and non-empty): resolved to an absolute path and validated as an
///    existing regular executable file — **fails closed** on error without falling back.
/// 2. `ANDROID_HOME/platform-tools/adb`
/// 3. `ANDROID_SDK_ROOT/platform-tools/adb`
/// 4. `PATH` lookup for `adb`
/// 5. Standard OS fallback (`$HOME/Library/Android/sdk/platform-tools/adb` / `$HOME/Android/Sdk/platform-tools/adb`)
pub fn discover_adb(env: &BTreeMap<String, String>) -> Result<PathBuf> {
    discover_adb_with_cwd(env, None)
}

pub(crate) fn discover_adb_with_cwd(
    env: &BTreeMap<String, String>,
    cwd_override: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(raw) = env.get("DEJA_ADB") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let raw_path = PathBuf::from(trimmed);
            let resolved = if raw_path.is_absolute() {
                raw_path
            } else {
                let base = match cwd_override {
                    Some(d) => d.to_path_buf(),
                    None => std::env::current_dir().map_err(|err| {
                        AndroidDriverError::InvalidAdbPath {
                            source_var: "DEJA_ADB".to_string(),
                            path: raw_path.clone(),
                            reason: format!("failed to resolve current working directory: {err}"),
                        }
                    })?,
                };
                base.join(raw_path)
            };
            validate_executable_override("DEJA_ADB", &resolved)?;
            return Ok(resolved);
        }
    }

    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(sdk) = env.get(var).map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let cand = PathBuf::from(sdk)
                .join("platform-tools")
                .join(adb_bin_name());
            if is_executable_file(&cand) {
                return Ok(cand);
            }
        }
    }

    if let Some(path_var) = env.get("PATH").filter(|v| !v.is_empty()) {
        for dir in std::env::split_paths(path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let cand = dir.join(adb_bin_name());
            if is_executable_file(&cand) {
                return Ok(cand);
            }
        }
    }

    if let Some(home) = env
        .get("HOME")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
    {
        for rel in [
            "Library/Android/sdk/platform-tools",
            "Android/Sdk/platform-tools",
        ] {
            let cand = home.join(rel).join(adb_bin_name());
            if is_executable_file(&cand) {
                return Ok(cand);
            }
        }
    }

    Err(AndroidDriverError::AdbNotFound(
        "install Android Platform-Tools and add platform-tools to PATH, set ANDROID_HOME, or set DEJA_ADB"
            .to_string(),
    ))
}

fn adb_bin_name() -> &'static str {
    #[cfg(windows)]
    {
        "adb.exe"
    }
    #[cfg(not(windows))]
    {
        "adb"
    }
}

fn validate_executable_override(var: &str, path: &Path) -> Result<()> {
    let meta = fs::metadata(path).map_err(|err| AndroidDriverError::InvalidAdbPath {
        source_var: var.to_string(),
        path: path.to_path_buf(),
        reason: err.to_string(),
    })?;
    if !meta.is_file() {
        return Err(AndroidDriverError::InvalidAdbPath {
            source_var: var.to_string(),
            path: path.to_path_buf(),
            reason: "path is not a regular file".to_string(),
        });
    }
    if !is_executable_file(path) {
        return Err(AndroidDriverError::InvalidAdbPath {
            source_var: var.to_string(),
            path: path.to_path_buf(),
            reason: "file is not executable".to_string(),
        });
    }
    Ok(())
}

/// Validates an Android application package ID (reverse-DNS format, no shell/control chars).
pub fn validate_package_id(pkg: &str) -> Result<String> {
    if pkg.is_empty() || pkg.len() > 255 || !pkg.contains('.') {
        return Err(AndroidDriverError::InvalidPackageId(pkg.to_string()));
    }
    for segment in pkg.split('.') {
        let mut bytes = segment.bytes();
        let Some(first) = bytes.next() else {
            return Err(AndroidDriverError::InvalidPackageId(pkg.to_string()));
        };
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return Err(AndroidDriverError::InvalidPackageId(pkg.to_string()));
        }
        if !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(AndroidDriverError::InvalidPackageId(pkg.to_string()));
        }
    }
    Ok(pkg.to_string())
}

/// Validates a launch argument extra key (no shell metacharacters, NUL, or whitespace).
pub fn validate_extra_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.starts_with('-')
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b':'))
    {
        return Err(AndroidDriverError::InvalidArgument(format!(
            "invalid Android launch argument key `{key}`"
        )));
    }
    Ok(())
}

/// Validates a URL for `openLink` (no NUL/CR/LF, valid RFC 3986 scheme).
pub fn validate_open_link_url(url: &str) -> Result<String> {
    if url.bytes().any(|b| matches!(b, b'\0' | b'\r' | b'\n')) {
        return Err(AndroidDriverError::InvalidArgument(
            "URL must not contain NUL, CR, or LF characters".to_string(),
        ));
    }
    let trimmed = url.trim();
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return Err(AndroidDriverError::InvalidArgument(format!(
            "invalid URL `{url}`"
        )));
    }
    let parsed = reqwest::Url::parse(trimmed).map_err(|err| {
        AndroidDriverError::InvalidArgument(format!("invalid URL `{trimmed}`: {err}"))
    })?;
    if parsed.scheme().is_empty() {
        return Err(AndroidDriverError::InvalidArgument(format!(
            "URL `{trimmed}` is missing a scheme"
        )));
    }
    Ok(trimmed.to_string())
}

/// Validates media paths for `addMedia` (non-empty list of existing regular non-symlink UTF-8 files).
pub fn validate_media_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if paths.is_empty() {
        return Err(AndroidDriverError::InvalidArgument(
            "add_media requires at least one file path".to_string(),
        ));
    }
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let path_str = path.to_str().ok_or_else(|| {
            AndroidDriverError::InvalidPath(format!("non-UTF-8 media path: {}", path.display()))
        })?;
        if path_str.is_empty() || path_str.starts_with('-') || path_str.contains('\0') {
            return Err(AndroidDriverError::InvalidPath(format!(
                "invalid media path: {path_str}"
            )));
        }
        let meta = fs::symlink_metadata(path).map_err(|err| {
            AndroidDriverError::InvalidPath(format!(
                "media file does not exist or cannot be stat'd ({}): {err}",
                path.display()
            ))
        })?;
        if meta.file_type().is_symlink() {
            return Err(AndroidDriverError::InvalidPath(format!(
                "refusing symlink media file: {}",
                path.display()
            )));
        }
        if !meta.is_file() {
            return Err(AndroidDriverError::InvalidPath(format!(
                "media path is not a regular file: {}",
                path.display()
            )));
        }
        out.push(path.clone());
    }
    Ok(out)
}

/// POSIX single-quote escaping for arguments concatenated by `adb shell`.
#[must_use]
pub fn shell_quote(raw: &str) -> String {
    let mut quoted = String::with_capacity(raw.len() + 2);
    quoted.push('\'');
    for ch in raw.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

/// Parses `adb devices -l` stdout into structured [`AdbDevice`] entries.
#[must_use]
pub fn parse_adb_devices_l(stdout: &str) -> Vec<AdbDevice> {
    let mut devices = Vec::new();
    for raw_line in stdout.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("List of devices") || line.starts_with("* daemon") {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(serial) = parts.next() else {
            continue;
        };
        let Some(state) = parts.next() else {
            continue;
        };
        let mut product = None;
        let mut model = None;
        let mut device = None;
        let mut transport_id = None;
        for token in parts {
            if let Some(v) = token.strip_prefix("product:") {
                product = Some(v.to_string());
            } else if let Some(v) = token.strip_prefix("model:") {
                model = Some(v.to_string());
            } else if let Some(v) = token.strip_prefix("device:") {
                device = Some(v.to_string());
            } else if let Some(v) = token.strip_prefix("transport_id:") {
                transport_id = Some(v.to_string());
            }
        }
        devices.push(AdbDevice {
            serial: serial.to_string(),
            state: state.to_string(),
            product,
            model,
            device,
            transport_id,
        });
    }
    devices
}

/// Parses rotation-aware display bounds from `dumpsys window displays` (`cur=<W>x<H>`).
#[must_use]
pub fn parse_display_bounds(dumpsys_output: &str) -> Option<UiSize> {
    for line in dumpsys_output.lines() {
        if let Some(pos) = line.find("cur=") {
            let after = &line[pos + 4..];
            let token = after.split_whitespace().next().unwrap_or("");
            if let Some(size) = parse_wxh(token) {
                return Some(size);
            }
        }
    }
    None
}

/// Parses `wm size` output, preferring `Override size:` over `Physical size:`.
#[must_use]
pub fn parse_wm_size(wm_output: &str) -> Option<UiSize> {
    let mut physical = None;
    for line in wm_output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Override size:")
            && let Some(size) = parse_wxh(rest.trim())
        {
            return Some(size);
        }
        if let Some(rest) = trimmed.strip_prefix("Physical size:")
            && let Some(size) = parse_wxh(rest.trim())
        {
            physical = Some(size);
        }
    }
    physical
}

fn parse_wxh(token: &str) -> Option<UiSize> {
    let cleaned = token.trim().trim_end_matches([',', ')']);
    let (w_str, h_str) = cleaned
        .split_once('x')
        .or_else(|| cleaned.split_once('X'))?;
    let w: u32 = w_str.trim().parse().ok()?;
    let h: u32 = h_str.trim().parse().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some(UiSize::new(f64::from(w), f64::from(h)))
}

/// Parses `mWakefulness=...` from `dumpsys power`.
#[must_use]
pub fn parse_wakefulness(dumpsys_power: &str) -> Wakefulness {
    for line in dumpsys_power.lines() {
        let trimmed = line.trim();
        if let Some(pos) = trimmed.find("mWakefulness=") {
            let val = trimmed[pos + "mWakefulness=".len()..]
                .split_whitespace()
                .next()
                .unwrap_or("");
            if val.eq_ignore_ascii_case("Awake") {
                return Wakefulness::Awake;
            }
            if val.eq_ignore_ascii_case("Dozing") {
                return Wakefulness::Dozing;
            }
            if val.eq_ignore_ascii_case("Asleep") {
                return Wakefulness::Asleep;
            }
        }
    }
    Wakefulness::Unknown
}

/// Parses keyguard/lock-screen visibility from `dumpsys window` (API 24..36 variations).
#[must_use]
pub fn parse_keyguard_showing(dumpsys_window: &str) -> Option<bool> {
    let mut saw_false = false;
    for line in dumpsys_window.lines() {
        let trimmed = line.trim();
        for key in [
            "isKeyguardShowing=",
            "mShowingLockscreen=",
            "mDreamingLockscreen=",
            "isStatusBarKeyguard=",
            "mKeyguardShowing=",
        ] {
            if let Some(pos) = trimmed.find(key) {
                let raw = trimmed[pos + key.len()..]
                    .split(|c: char| c.is_ascii_whitespace() || c == ',' || c == '}')
                    .next()
                    .unwrap_or("");
                if raw.eq_ignore_ascii_case("true") {
                    return Some(true);
                }
                if raw.eq_ignore_ascii_case("false") {
                    saw_false = true;
                }
            }
        }
    }
    saw_false.then_some(false)
}

/// Parses `getprop ro.build.version.sdk` into `"android-<sdk>"` (`DEJA-55`).
pub fn parse_android_runtime_sdk(raw_sdk: &str) -> Result<String> {
    let trimmed = raw_sdk.trim();
    let sdk: u32 = trimmed.parse().map_err(|_| {
        AndroidDriverError::InvalidArgument(format!(
            "unexpected `ro.build.version.sdk` output: {raw_sdk:?}"
        ))
    })?;
    if sdk == 0 {
        return Err(AndroidDriverError::InvalidArgument(
            "`ro.build.version.sdk` reported 0".to_string(),
        ));
    }
    Ok(format!("android-{sdk}"))
}

/// Maps Maestro/Déjà permission names onto Android runtime permissions (fail-closed).
pub fn map_android_permissions(raw_key: &str) -> Result<Vec<String>> {
    let trimmed = raw_key.trim();
    if trimmed.is_empty() {
        return Err(AndroidDriverError::InvalidArgument(
            "empty Android permission key".to_string(),
        ));
    }
    if trimmed.starts_with("android.permission.") {
        if !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
        {
            return Err(AndroidDriverError::InvalidArgument(format!(
                "invalid Android permission `{trimmed}`"
            )));
        }
        return Ok(vec![trimmed.to_string()]);
    }

    let mapped: &[&str] = match trimmed.to_ascii_lowercase().as_str() {
        "all" => &[
            "android.permission.CAMERA",
            "android.permission.ACCESS_FINE_LOCATION",
            "android.permission.ACCESS_COARSE_LOCATION",
            "android.permission.READ_CONTACTS",
            "android.permission.WRITE_CONTACTS",
            "android.permission.READ_CALENDAR",
            "android.permission.WRITE_CALENDAR",
            "android.permission.RECORD_AUDIO",
            "android.permission.POST_NOTIFICATIONS",
            "android.permission.READ_MEDIA_IMAGES",
            "android.permission.READ_MEDIA_VIDEO",
            "android.permission.READ_EXTERNAL_STORAGE",
            "android.permission.WRITE_EXTERNAL_STORAGE",
        ],
        "camera" => &["android.permission.CAMERA"],
        "location" | "location-always" | "location_always" => &[
            "android.permission.ACCESS_FINE_LOCATION",
            "android.permission.ACCESS_COARSE_LOCATION",
        ],
        "contacts" | "contacts-limited" | "contacts_limited" => &[
            "android.permission.READ_CONTACTS",
            "android.permission.WRITE_CONTACTS",
        ],
        "calendar" => &[
            "android.permission.READ_CALENDAR",
            "android.permission.WRITE_CALENDAR",
        ],
        "photos" | "photos-add" | "photos_add" | "medialibrary" | "media-library"
        | "media_library" | "storage" => &[
            "android.permission.READ_MEDIA_IMAGES",
            "android.permission.READ_MEDIA_VIDEO",
            "android.permission.READ_EXTERNAL_STORAGE",
            "android.permission.WRITE_EXTERNAL_STORAGE",
        ],
        "microphone" => &["android.permission.RECORD_AUDIO"],
        "notifications" => &["android.permission.POST_NOTIFICATIONS"],
        "phone" => &[
            "android.permission.READ_PHONE_STATE",
            "android.permission.CALL_PHONE",
        ],
        "sms" => &["android.permission.READ_SMS", "android.permission.SEND_SMS"],
        "bluetooth" => &[
            "android.permission.BLUETOOTH_CONNECT",
            "android.permission.BLUETOOTH_SCAN",
        ],
        "activity_recognition" | "motion" => &["android.permission.ACTIVITY_RECOGNITION"],
        _ => {
            return Err(AndroidDriverError::InvalidArgument(format!(
                "unsupported Android permission `{raw_key}`"
            )));
        }
    };
    Ok(mapped.iter().map(|s| (*s).to_string()).collect())
}

/// Maps Maestro/Déjà permission action (`allow`/`deny`/`unset`) to `grant` or `revoke`.
pub fn map_android_permission_action(raw: &str) -> Result<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "allow" | "grant" => Ok("grant"),
        "deny" | "revoke" | "unset" | "reset" => Ok("revoke"),
        _ => Err(AndroidDriverError::InvalidArgument(format!(
            "unsupported Android permission action `{raw}` (expected allow, deny, or unset)"
        ))),
    }
}

/// Wrapper around an explicit `adb` executable path and isolated environment map.
#[derive(Debug, Clone)]
pub struct Adb {
    path: PathBuf,
    env: BTreeMap<String, String>,
}

impl Adb {
    pub fn discover(env: BTreeMap<String, String>) -> Result<Self> {
        let path = discover_adb(&env)?;
        Ok(Self { path, env })
    }

    #[must_use]
    pub fn from_path(path: PathBuf, env: BTreeMap<String, String>) -> Self {
        Self { path, env }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    async fn run_raw(
        &self,
        args: &[&str],
        cancel: &CancellationToken,
    ) -> Result<std::process::Output> {
        if cancel.is_cancelled() {
            return Err(AndroidDriverError::Cancelled);
        }
        let mut cmd = Command::new(&self.path);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        apply_adb_env(&mut cmd, &self.env);

        debug!(
            program = %self.path.display(),
            subcommand = args.first().copied().unwrap_or(""),
            "running adb"
        );
        let child = cmd
            .spawn()
            .map_err(|source| AndroidDriverError::CommandSpawn {
                program: self.path.display().to_string(),
                source,
            })?;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(AndroidDriverError::Cancelled),
            out = child.wait_with_output() => out.map_err(|source| AndroidDriverError::Io {
                context: format!("wait for `adb {}`", args.join(" ")),
                source,
            }),
        }
    }

    async fn run_ok(
        &self,
        args: &[&str],
        cancel: &CancellationToken,
    ) -> Result<std::process::Output> {
        let out = self.run_raw(args, cancel).await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let detail = if stderr.is_empty() { stdout } else { stderr };
            return Err(AndroidDriverError::CommandFailed {
                program: self.path.display().to_string(),
                args: args.join(" "),
                status: out.status.code(),
                stderr: detail,
            });
        }
        Ok(out)
    }

    pub async fn version(&self, cancel: &CancellationToken) -> Result<String> {
        let out = self.run_ok(&["version"], cancel).await?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("adb available")
            .trim()
            .to_string())
    }

    pub async fn list_devices(&self, cancel: &CancellationToken) -> Result<Vec<AdbDevice>> {
        let out = self.run_ok(&["devices", "-l"], cancel).await?;
        Ok(parse_adb_devices_l(&String::from_utf8_lossy(&out.stdout)))
    }

    /// Runs `adb -s <serial> shell <cmd_args...>` and returns stdout as UTF-8.
    pub async fn shell(
        &self,
        serial: &str,
        shell_args: &[&str],
        cancel: &CancellationToken,
    ) -> Result<String> {
        let canonical = validate_serial(serial)?;
        let mut args: Vec<&str> = Vec::with_capacity(3 + shell_args.len());
        args.push("-s");
        args.push(&canonical);
        args.push("shell");
        args.extend_from_slice(shell_args);
        let out = self.run_ok(&args, cancel).await?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Queries `getprop <key>` on `serial`.
    pub async fn getprop(
        &self,
        serial: &str,
        key: &str,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let out = self.shell(serial, &["getprop", key], cancel).await?;
        Ok(out.trim().to_string())
    }

    /// Resolves and pins the exact target `serial`, failing closed if missing, offline,
    /// unauthorized, or if `ro.serialno` disagrees for a non-emulator physical device.
    pub async fn resolve_target_device(
        &self,
        serial: &str,
        cancel: &CancellationToken,
    ) -> Result<AdbDevice> {
        let canonical = validate_serial(serial)?;
        let devices = self.list_devices(cancel).await?;
        let Some(found) = devices.iter().find(|d| d.serial == canonical).cloned() else {
            let available: Vec<String> = devices
                .iter()
                .map(|d| format!("{} ({})", d.serial, d.state))
                .collect();
            return Err(AndroidDriverError::DeviceNotFound {
                serial: canonical,
                available,
            });
        };

        if found.state != "device" {
            let remediation = match found.state.as_str() {
                "unauthorized" => {
                    "Unlock the Android device and accept the 'Allow USB debugging' RSA prompt"
                }
                "offline" => {
                    "Reconnect the device or restart the emulator (`adb reconnect` or restart emulator)"
                }
                "bootloader" | "recovery" => {
                    "Boot the Android device into normal Android OS mode before running Déjà"
                }
                _ => "Wait for `adb devices` to report this serial in the `device` state",
            };
            return Err(AndroidDriverError::DeviceNotReady {
                serial: canonical,
                state: found.state,
                remediation: remediation.to_string(),
            });
        }

        // Fail-closed `ro.serialno` verification for non-emulator devices (`DEJA-85`).
        if !canonical.starts_with("emulator-") {
            let actual_serialno = self.getprop(&canonical, "ro.serialno", cancel).await?;
            let is_tcp_transport = canonical.contains(':');
            if actual_serialno.is_empty() || (!is_tcp_transport && actual_serialno != canonical) {
                return Err(AndroidDriverError::DeviceIdentityMismatch {
                    serial: canonical.clone(),
                    expected_serial: canonical,
                    actual_serialno,
                });
            }
        }

        Ok(found)
    }

    /// Resolves the normalized runtime identifier (`"android-<sdk>"`, `DEJA-55`).
    pub async fn runtime_version(
        &self,
        serial: &str,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let sdk_raw = self.getprop(serial, "ro.build.version.sdk", cancel).await?;
        parse_android_runtime_sdk(&sdk_raw)
    }

    /// Queries rotation-aware display bounds via `dumpsys window displays` (`cur=WxH`),
    /// falling back to `wm size` (`Override size` then `Physical size`).
    pub async fn display_bounds(
        &self,
        serial: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<UiSize>> {
        if let Ok(displays_out) = self
            .shell(serial, &["dumpsys", "window", "displays"], cancel)
            .await
            && let Some(size) = parse_display_bounds(&displays_out)
        {
            return Ok(Some(size));
        }
        if let Ok(wm_out) = self.shell(serial, &["wm", "size"], cancel).await
            && let Some(size) = parse_wm_size(&wm_out)
        {
            return Ok(Some(size));
        }
        Ok(None)
    }

    /// Checks wakefulness and keyguard state (`DEJA-85`).
    ///
    /// - If keyguard/lock screen is showing -> **fails closed** with [`AndroidDriverError::DeviceLocked`].
    /// - If sleeping/dozing and unlocked -> sends `KEYCODE_WAKEUP` and re-verifies unlocked state.
    pub async fn ensure_awake_and_unlocked(
        &self,
        serial: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let power_out = self.shell(serial, &["dumpsys", "power"], cancel).await?;
        let window_out = self.shell(serial, &["dumpsys", "window"], cancel).await?;

        if parse_keyguard_showing(&window_out) == Some(true) {
            return Err(AndroidDriverError::DeviceLocked {
                serial: serial.to_string(),
                detail: "keyguard/lock screen is showing; unlock the Android device before running Déjà flows".to_string(),
            });
        }

        match parse_wakefulness(&power_out) {
            Wakefulness::Asleep | Wakefulness::Dozing => {
                self.shell(serial, &["input", "keyevent", "KEYCODE_WAKEUP"], cancel)
                    .await?;
                let post_window = self.shell(serial, &["dumpsys", "window"], cancel).await?;
                if parse_keyguard_showing(&post_window) == Some(true) {
                    return Err(AndroidDriverError::DeviceLocked {
                        serial: serial.to_string(),
                        detail: "device woke to a locked keyguard screen; unlock the Android device before running Déjà flows".to_string(),
                    });
                }
            }
            Wakefulness::Awake | Wakefulness::Unknown => {}
        }

        Ok(())
    }

    /// Sends a non-mutating `KEYCODE_UNKNOWN` (0) event to refresh the screen stay-awake timer.
    pub async fn keep_awake_ping(&self, serial: &str, cancel: &CancellationToken) -> Result<()> {
        let _ = self
            .shell(serial, &["input", "keyevent", "0"], cancel)
            .await?;
        Ok(())
    }

    pub async fn is_package_installed(
        &self,
        serial: &str,
        pkg: &str,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        let pkg = validate_package_id(pkg)?;
        let out = self
            .run_raw(&["-s", serial, "shell", "pm", "path", &pkg], cancel)
            .await?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        Ok(stdout.lines().any(|l| l.trim().starts_with("package:")))
    }

    pub async fn install_apk(
        &self,
        serial: &str,
        apk_path: &Path,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let canonical = validate_serial(serial)?;
        let apk_str = apk_path.to_str().ok_or_else(|| {
            AndroidDriverError::InvalidPath(format!("non-UTF-8 APK path: {}", apk_path.display()))
        })?;
        let out = self
            .run_ok(&["-s", &canonical, "install", "-r", "-g", apk_str], cancel)
            .await?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stdout.contains("Failure [") || stderr.contains("Failure [") {
            return Err(AndroidDriverError::CommandFailed {
                program: self.path.display().to_string(),
                args: format!("-s {canonical} install -r -g {apk_str}"),
                status: out.status.code(),
                stderr: format!("{} {}", stdout.trim(), stderr.trim())
                    .trim()
                    .to_string(),
            });
        }
        Ok(())
    }

    pub async fn force_stop(
        &self,
        serial: &str,
        pkg: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let pkg = validate_package_id(pkg)?;
        let _ = self
            .shell(serial, &["am", "force-stop", &pkg], cancel)
            .await?;
        Ok(())
    }

    pub async fn clear_state(
        &self,
        serial: &str,
        pkg: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let pkg = validate_package_id(pkg)?;
        let out = self.shell(serial, &["pm", "clear", &pkg], cancel).await?;
        if out.contains("Failed") {
            return Err(AndroidDriverError::CommandFailed {
                program: self.path.display().to_string(),
                args: format!("-s {serial} shell pm clear {pkg}"),
                status: Some(1),
                stderr: out.trim().to_string(),
            });
        }
        Ok(())
    }

    pub async fn set_permissions(
        &self,
        serial: &str,
        pkg: &str,
        perms: &BTreeMap<String, String>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let pkg = validate_package_id(pkg)?;
        if perms.is_empty() {
            return Ok(());
        }

        // Validate all entries up front before mutating any device permission state.
        let mut plan: Vec<(&str, Vec<String>, bool)> = Vec::with_capacity(perms.len());
        for (raw_key, raw_action) in perms {
            let action = map_android_permission_action(raw_action)?;
            let android_perms = map_android_permissions(raw_key)?;
            let is_all = raw_key.trim().eq_ignore_ascii_case("all");
            plan.push((action, android_perms, is_all));
        }
        // Apply `"all"` first so specific overrides apply deterministically after.
        plan.sort_by_key(|(_, _, is_all)| if *is_all { 0u8 } else { 1u8 });

        for (action, android_perms, is_all) in plan {
            for perm in android_perms {
                let res = self
                    .shell(serial, &["pm", action, &pkg, &perm], cancel)
                    .await;
                if let Err(err) = res {
                    if is_all {
                        // Ignore unrequested/non-runtime permissions when expanding `all`.
                        debug!(
                            pkg = %pkg,
                            permission = %perm,
                            action,
                            error = %err,
                            "skipping unapplicable runtime permission during `all`"
                        );
                        continue;
                    }
                    // On Android API variations, some compound aliases (e.g. photos -> READ_MEDIA_IMAGES
                    // on API < 33) may warn about unknown permissions; ignore only benign unknown-permission errors.
                    let msg = err.to_string();
                    if msg.contains("Unknown permission")
                        || msg.contains("has not requested permission")
                    {
                        debug!(
                            pkg = %pkg,
                            permission = %perm,
                            action,
                            "permission not requested by APK; skipping"
                        );
                        continue;
                    }
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    /// Resolves the package's launcher component and launches it via `am start -W` with typed
    /// intent extras for `arguments`.
    pub async fn launch_app(
        &self,
        serial: &str,
        pkg: &str,
        arguments: &BTreeMap<String, LaunchArgumentValue>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let pkg = validate_package_id(pkg)?;
        for (k, v) in arguments {
            validate_extra_key(k)?;
            if let LaunchArgumentValue::String(s) = v
                && s.contains('\0')
            {
                return Err(AndroidDriverError::InvalidArgument(format!(
                    "launch argument `{k}` value must not contain NUL bytes"
                )));
            }
        }

        let resolved = self
            .shell(
                serial,
                &[
                    "cmd",
                    "package",
                    "resolve-activity",
                    "--brief",
                    "-c",
                    "android.intent.category.LAUNCHER",
                    &pkg,
                ],
                cancel,
            )
            .await?;

        let component = parse_resolved_activity(&resolved).ok_or_else(|| {
            AndroidDriverError::InvalidArgument(format!(
                "no LAUNCHER activity resolved for package `{pkg}` (output: {})",
                resolved.trim()
            ))
        })?;

        let mut cmd_parts: Vec<String> = vec![
            "am".to_string(),
            "start".to_string(),
            "-W".to_string(),
            "-n".to_string(),
            shell_quote(&component),
        ];

        for (key, value) in arguments {
            match value {
                LaunchArgumentValue::String(s) => {
                    cmd_parts.push("--es".to_string());
                    cmd_parts.push(shell_quote(key));
                    cmd_parts.push(shell_quote(s));
                }
                LaunchArgumentValue::Bool(b) => {
                    cmd_parts.push("--ez".to_string());
                    cmd_parts.push(shell_quote(key));
                    cmd_parts.push(if *b { "true" } else { "false" }.to_string());
                }
                LaunchArgumentValue::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        if i32::try_from(i).is_ok() {
                            cmd_parts.push("--ei".to_string());
                        } else {
                            cmd_parts.push("--el".to_string());
                        }
                        cmd_parts.push(shell_quote(key));
                        cmd_parts.push(i.to_string());
                    } else if let Some(u) = n.as_u64() {
                        if i32::try_from(u).is_ok() {
                            cmd_parts.push("--ei".to_string());
                            cmd_parts.push(shell_quote(key));
                            cmd_parts.push(u.to_string());
                        } else if i64::try_from(u).is_ok() {
                            cmd_parts.push("--el".to_string());
                            cmd_parts.push(shell_quote(key));
                            cmd_parts.push(u.to_string());
                        } else {
                            cmd_parts.push("--es".to_string());
                            cmd_parts.push(shell_quote(key));
                            cmd_parts.push(shell_quote(&u.to_string()));
                        }
                    } else if let Some(f) = n.as_f64() {
                        if !f.is_finite() {
                            return Err(AndroidDriverError::InvalidArgument(format!(
                                "launch argument `{key}` number must be finite"
                            )));
                        }
                        cmd_parts.push("--ef".to_string());
                        cmd_parts.push(shell_quote(key));
                        cmd_parts.push(deja_core::js_number_to_string(f));
                    }
                }
            }
        }

        let shell_line = cmd_parts.join(" ");
        let out = self.shell(serial, &[&shell_line], cancel).await?;
        if out.contains("Error:") || out.contains("Error type") {
            return Err(AndroidDriverError::CommandFailed {
                program: self.path.display().to_string(),
                args: format!("-s {serial} shell {shell_line}"),
                status: Some(1),
                stderr: out.trim().to_string(),
            });
        }
        Ok(())
    }

    pub async fn open_link(
        &self,
        serial: &str,
        url: &str,
        browser: bool,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let validated = validate_open_link_url(url)?;
        if browser {
            let parsed = reqwest::Url::parse(&validated).map_err(|err| {
                AndroidDriverError::InvalidArgument(format!("invalid URL `{validated}`: {err}"))
            })?;
            let scheme = parsed.scheme();
            if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
                return Err(AndroidDriverError::InvalidArgument(format!(
                    "openLink with `browser: true` requires an http or https URL, got `{scheme}`"
                )));
            }
        }

        let mut parts = vec![
            "am".to_string(),
            "start".to_string(),
            "-W".to_string(),
            "-a".to_string(),
            "android.intent.action.VIEW".to_string(),
        ];
        if browser {
            parts.push("-c".to_string());
            parts.push("android.intent.category.BROWSABLE".to_string());
        }
        parts.push("-d".to_string());
        parts.push(shell_quote(&validated));

        let shell_line = parts.join(" ");
        let out = self.shell(serial, &[&shell_line], cancel).await?;
        if out.contains("Error:") || out.contains("Error type") {
            return Err(AndroidDriverError::CommandFailed {
                program: self.path.display().to_string(),
                args: format!("-s {serial} shell {shell_line}"),
                status: Some(1),
                stderr: out.trim().to_string(),
            });
        }
        Ok(())
    }

    pub async fn add_media(
        &self,
        serial: &str,
        paths: &[PathBuf],
        cancel: &CancellationToken,
    ) -> Result<()> {
        let canonical = validate_serial(serial)?;
        let validated = validate_media_paths(paths)?;
        for path in validated {
            let orig_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("media.bin");
            let safe_name: String = orig_name
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let nonce = MEDIA_NONCE.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let remote = format!("/sdcard/Pictures/deja-{nanos:x}-{nonce}-{safe_name}");
            let local_str = path.to_str().ok_or_else(|| {
                AndroidDriverError::InvalidPath(format!("non-UTF-8 media path: {}", path.display()))
            })?;
            let _ = self
                .run_ok(&["-s", &canonical, "push", local_str, &remote], cancel)
                .await?;
            let broadcast = format!(
                "am broadcast -a android.intent.action.MEDIA_SCANNER_SCAN_FILE -d {}",
                shell_quote(&format!("file://{remote}"))
            );
            let _ = self.shell(&canonical, &[&broadcast], cancel).await?;
        }
        Ok(())
    }

    pub async fn screencap(&self, serial: &str, cancel: &CancellationToken) -> Result<Vec<u8>> {
        let canonical = validate_serial(serial)?;
        let out = self
            .run_ok(&["-s", &canonical, "exec-out", "screencap", "-p"], cancel)
            .await?;
        if out.stdout.len() < PNG_MAGIC.len() || &out.stdout[..PNG_MAGIC.len()] != PNG_MAGIC {
            return Err(AndroidDriverError::InvalidScreenshot(format!(
                "adb screencap returned {} bytes without PNG signature",
                out.stdout.len()
            )));
        }
        Ok(out.stdout)
    }

    pub async fn logcat_crash_epoch(
        &self,
        serial: &str,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let canonical = validate_serial(serial)?;
        let out = self
            .run_ok(
                &[
                    "-s", &canonical, "logcat", "-d", "-b", "crash", "-v", "epoch",
                ],
                cancel,
            )
            .await?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Parses `cmd package resolve-activity --brief` output to extract `<pkg>/<activity>`.
#[must_use]
pub fn parse_resolved_activity(output: &str) -> Option<String> {
    output
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| {
            !line.is_empty()
                && line.contains('/')
                && !line.starts_with("priority=")
                && !line.contains(' ')
        })
        .map(ToString::to_string)
}

/// RAII guard that manages `adb -s <serial> forward tcp:0 tcp:<device_port>` and removes the
/// forward rule cleanly on both async `remove()` and synchronous `Drop`.
#[derive(Debug)]
pub struct AdbForwardGuard {
    adb_path: PathBuf,
    serial: String,
    host_port: u16,
    device_port: u16,
    env: BTreeMap<String, String>,
    removed: bool,
}

impl AdbForwardGuard {
    pub async fn create(
        adb: &Adb,
        serial: &str,
        device_port: u16,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let canonical = validate_serial(serial)?;
        let device_spec = format!("tcp:{device_port}");
        let out = adb
            .run_ok(
                &["-s", &canonical, "forward", "tcp:0", &device_spec],
                cancel,
            )
            .await?;
        let raw_port = String::from_utf8_lossy(&out.stdout);
        let host_port: u16 = raw_port
            .trim()
            .parse()
            .ok()
            .filter(|&p| p > 0)
            .ok_or_else(|| {
                AndroidDriverError::AgentStartupFailed(format!(
                    "`adb forward tcp:0 {device_spec}` returned invalid host port: {:?}",
                    raw_port.trim()
                ))
            })?;
        Ok(Self {
            adb_path: adb.path().to_path_buf(),
            serial: canonical,
            host_port,
            device_port,
            env: adb.env().clone(),
            removed: false,
        })
    }

    #[must_use]
    pub fn host_port(&self) -> u16 {
        self.host_port
    }

    #[must_use]
    pub fn device_port(&self) -> u16 {
        self.device_port
    }

    pub async fn remove(&mut self) {
        if self.removed {
            return;
        }
        self.removed = true;
        let host_spec = format!("tcp:{}", self.host_port);
        let mut cmd = Command::new(&self.adb_path);
        cmd.args(["-s", &self.serial, "forward", "--remove", &host_spec])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        apply_adb_env(&mut cmd, &self.env);
        if let Ok(mut child) = cmd.spawn() {
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        }
    }
}

impl Drop for AdbForwardGuard {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        self.removed = true;
        let host_spec = format!("tcp:{}", self.host_port);
        let mut cmd = std::process::Command::new(&self.adb_path);
        cmd.args(["-s", &self.serial, "forward", "--remove", &host_spec]);
        apply_adb_env_std(&mut cmd, &self.env);
        let _ = run_sync_command_bounded(cmd, DROP_ADB_TIMEOUT);
    }
}

/// RAII guard that captures original Android global animation scale values
/// (`window_animation_scale`, `transition_animation_scale`, `animator_duration_scale`),
/// zeroes them on attach, and restores exact original values (`null` -> `settings delete global`,
/// value -> `settings put global`) on both async `restore()` and synchronous `Drop` (`DEJA-85`).
#[derive(Debug)]
pub struct AnimationScaleGuard {
    adb_path: PathBuf,
    serial: String,
    env: BTreeMap<String, String>,
    originals: Vec<(&'static str, Option<String>)>,
    restored: bool,
}

impl AnimationScaleGuard {
    pub async fn disable_animations(
        adb: &Adb,
        serial: &str,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let canonical = validate_serial(serial)?;
        let mut originals = Vec::with_capacity(ANIMATION_SCALE_KEYS.len());

        for key in ANIMATION_SCALE_KEYS {
            let raw = adb
                .shell(&canonical, &["settings", "get", "global", key], cancel)
                .await?;
            let trimmed = raw.trim();
            let orig = if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
                None
            } else {
                Some(trimmed.to_string())
            };
            originals.push((key, orig));
        }

        let mut guard = Self {
            adb_path: adb.path().to_path_buf(),
            serial: canonical.clone(),
            env: adb.env().clone(),
            originals,
            restored: false,
        };

        for key in ANIMATION_SCALE_KEYS {
            if let Err(err) = adb
                .shell(&canonical, &["settings", "put", "global", key, "0"], cancel)
                .await
            {
                guard.restore().await;
                return Err(err);
            }
        }

        Ok(guard)
    }

    #[must_use]
    pub fn originals(&self) -> &[(&'static str, Option<String>)] {
        &self.originals
    }

    pub async fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        for (key, orig) in &self.originals {
            let mut cmd = Command::new(&self.adb_path);
            match orig {
                Some(val) => {
                    cmd.args([
                        "-s",
                        &self.serial,
                        "shell",
                        "settings",
                        "put",
                        "global",
                        key,
                        val,
                    ]);
                }
                None => {
                    cmd.args([
                        "-s",
                        &self.serial,
                        "shell",
                        "settings",
                        "delete",
                        "global",
                        key,
                    ]);
                }
            }
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            apply_adb_env(&mut cmd, &self.env);
            if let Ok(mut child) = cmd.spawn() {
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            }
        }
    }
}

impl Drop for AnimationScaleGuard {
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        for (key, orig) in &self.originals {
            let mut cmd = std::process::Command::new(&self.adb_path);
            match orig {
                Some(val) => {
                    cmd.args([
                        "-s",
                        &self.serial,
                        "shell",
                        "settings",
                        "put",
                        "global",
                        key,
                        val,
                    ]);
                }
                None => {
                    cmd.args([
                        "-s",
                        &self.serial,
                        "shell",
                        "settings",
                        "delete",
                        "global",
                        key,
                    ]);
                }
            }
            apply_adb_env_std(&mut cmd, &self.env);
            let _ = run_sync_command_bounded(cmd, DROP_ADB_TIMEOUT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Adb, AdbForwardGuard, AnimationScaleGuard, Wakefulness, discover_adb_with_cwd,
        parse_adb_devices_l, parse_android_runtime_sdk, parse_display_bounds,
        parse_keyguard_showing, parse_resolved_activity, parse_wakefulness, parse_wm_size,
        shell_quote, validate_package_id,
    };
    use crate::error::AndroidDriverError;
    use deja_flow::UiSize;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio_util::sync::CancellationToken;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "deja-adb-test-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn write_exec(path: &std::path::Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn parses_adb_devices_l_and_runtime_sdk() {
        let raw = "\
* daemon not running; starting now at tcp:5037
* daemon started successfully
List of devices attached
emulator-5554          device product:sdk_gphone64_arm64 model:sdk_gphone64_arm64 device:emu64a transport_id:1
R3CR10ABCDE            unauthorized usb:1-1 transport_id:2
127.0.0.1:5555         offline transport_id:3
";
        let devices = parse_adb_devices_l(raw);
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].serial, "emulator-5554");
        assert_eq!(devices[0].state, "device");
        assert_eq!(devices[0].model.as_deref(), Some("sdk_gphone64_arm64"));
        assert_eq!(devices[1].state, "unauthorized");
        assert_eq!(devices[2].state, "offline");

        assert_eq!(parse_android_runtime_sdk("35\n").unwrap(), "android-35");
        assert!(parse_android_runtime_sdk("0").is_err());
        assert!(parse_android_runtime_sdk("not-a-number").is_err());
    }

    #[test]
    fn parses_rotated_display_bounds_wm_size_wakefulness_and_keyguard() {
        let rotated = "\
WINDOW MANAGER DISPLAY CONTENTS (dumpsys window displays)
  Display: mDisplayId=0 (organized)
    init=1080x2400 420dpi mMinSizeOfResizeableTaskDp=220 cur=2400x1080 app=2256x1080 rng=1080x960-2256x2256
";
        assert_eq!(
            parse_display_bounds(rotated),
            Some(UiSize::new(2400.0, 1080.0))
        );
        assert_eq!(
            parse_wm_size("Physical size: 1080x2400\nOverride size: 720x1600\n"),
            Some(UiSize::new(720.0, 1600.0))
        );
        assert_eq!(
            parse_wm_size("Physical size: 1080x2400\n"),
            Some(UiSize::new(1080.0, 2400.0))
        );

        assert_eq!(
            parse_wakefulness("  mWakefulness=Awake\n"),
            Wakefulness::Awake
        );
        assert_eq!(
            parse_wakefulness("  mWakefulness=Asleep\n"),
            Wakefulness::Asleep
        );
        assert_eq!(
            parse_wakefulness("  mWakefulness=Dozing\n"),
            Wakefulness::Dozing
        );
        assert_eq!(
            parse_keyguard_showing("  mShowingLockscreen=true mDreamingLockscreen=false\n"),
            Some(true)
        );
        assert_eq!(
            parse_keyguard_showing("  isKeyguardShowing=false\n"),
            Some(false)
        );
        assert_eq!(
            parse_resolved_activity("priority=0 preferredOrder=0\ncom.example.app/.MainActivity\n"),
            Some("com.example.app/.MainActivity".to_string())
        );
        assert_eq!(shell_quote("a'b c"), "'a'\\''b c'");
        assert!(validate_package_id("com.example.app_1").is_ok());
        assert!(validate_package_id("bad").is_err());
        assert!(validate_package_id("com.bad;rm").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn discover_adb_respects_precedence_and_deja_adb_fails_closed() {
        let dir = temp_dir("discover");
        let sdk = dir.join("sdk");
        fs::create_dir_all(sdk.join("platform-tools")).unwrap();
        let sdk_adb = sdk.join("platform-tools/adb");
        write_exec(&sdk_adb, "#!/bin/sh\necho sdk\n");

        let path_bin = dir.join("bin");
        fs::create_dir_all(&path_bin).unwrap();
        let path_adb = path_bin.join("adb");
        write_exec(&path_adb, "#!/bin/sh\necho path\n");

        // 1. ANDROID_HOME beats PATH.
        let mut env = BTreeMap::new();
        env.insert(
            "ANDROID_HOME".to_string(),
            sdk.to_string_lossy().into_owned(),
        );
        env.insert("PATH".to_string(), path_bin.to_string_lossy().into_owned());
        assert_eq!(discover_adb_with_cwd(&env, Some(&dir)).unwrap(), sdk_adb);

        // 2. DEJA_ADB explicit override fails closed when broken (never falls back to ANDROID_HOME).
        env.insert(
            "DEJA_ADB".to_string(),
            dir.join("missing-adb").to_string_lossy().into_owned(),
        );
        let err = discover_adb_with_cwd(&env, Some(&dir)).expect_err("broken DEJA_ADB must fail");
        assert!(matches!(err, AndroidDriverError::InvalidAdbPath { .. }));

        // 3. Relative DEJA_ADB resolves to absolute path.
        let custom_adb = dir.join("custom-adb");
        write_exec(&custom_adb, "#!/bin/sh\necho custom\n");
        env.insert("DEJA_ADB".to_string(), "custom-adb".to_string());
        assert_eq!(discover_adb_with_cwd(&env, Some(&dir)).unwrap(), custom_adb);

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mock_adb_pins_serialno_enforces_keyguard_and_restores_scales_and_forwards() {
        let dir = temp_dir("mock-adb");
        let log_file = dir.join("adb.log");
        let adb_bin = dir.join("adb");

        let script = format!(
            r#"#!/bin/sh
echo "$*" >> "{log}"
case "$*" in
  "devices -l")
    echo "List of devices attached"
    echo "emulator-5554          device product:emu model:Pixel transport_id:1"
    echo "USB12345               device product:phone model:Pixel9 transport_id:2"
    echo "WRONG999               device product:phone model:Pixel9 transport_id:3"
    echo "OFFLINE1               offline transport_id:4"
    echo "UNAUTH1                unauthorized transport_id:5"
    ;;
  "-s USB12345 shell getprop ro.serialno")
    echo "USB12345"
    ;;
  "-s WRONG999 shell getprop ro.serialno")
    echo "ACTUAL_OTHER_DEVICE"
    ;;
  "-s emulator-5554 shell getprop ro.build.version.sdk")
    echo "35"
    ;;
  "-s emulator-5554 shell dumpsys power")
    echo "  mWakefulness=Asleep"
    ;;
  "-s emulator-5554 shell dumpsys window")
    echo "  isKeyguardShowing=false"
    ;;
  "-s USB12345 shell dumpsys power")
    echo "  mWakefulness=Awake"
    ;;
  "-s USB12345 shell dumpsys window")
    echo "  mShowingLockscreen=true"
    ;;
  "-s emulator-5554 shell settings get global window_animation_scale")
    echo "1.0"
    ;;
  "-s emulator-5554 shell settings get global transition_animation_scale")
    echo "null"
    ;;
  "-s emulator-5554 shell settings get global animator_duration_scale")
    echo "0.5"
    ;;
  "-s emulator-5554 forward tcp:0 tcp:7007")
    echo "45123"
    ;;
  *)
    exit 0
    ;;
esac
"#,
            log = log_file.display()
        );
        write_exec(&adb_bin, &script);

        let adb = Adb::from_path(adb_bin, BTreeMap::new());
        let cancel = CancellationToken::new();

        // Serial pinning: emulator-5554 & matching USB12345 succeed; WRONG999 fails closed on ro.serialno;
        // OFFLINE1 / UNAUTH1 / MISSING fail closed with actionable errors.
        adb.resolve_target_device("emulator-5554", &cancel)
            .await
            .expect("emulator should resolve");
        adb.resolve_target_device("USB12345", &cancel)
            .await
            .expect("matching physical serialno should resolve");

        let mismatch = adb
            .resolve_target_device("WRONG999", &cancel)
            .await
            .expect_err("ro.serialno mismatch must fail closed");
        assert!(matches!(
            mismatch,
            AndroidDriverError::DeviceIdentityMismatch { .. }
        ));

        let offline = adb
            .resolve_target_device("OFFLINE1", &cancel)
            .await
            .expect_err("offline device must fail closed");
        assert!(matches!(offline, AndroidDriverError::DeviceNotReady { .. }));

        let unauth = adb
            .resolve_target_device("UNAUTH1", &cancel)
            .await
            .expect_err("unauthorized device must fail closed");
        assert!(matches!(unauth, AndroidDriverError::DeviceNotReady { .. }));

        let missing = adb
            .resolve_target_device("NO_SUCH_DEVICE", &cancel)
            .await
            .expect_err("missing serial must never switch devices");
        assert!(matches!(missing, AndroidDriverError::DeviceNotFound { .. }));

        // Runtime version probe (`DEJA-55`).
        assert_eq!(
            adb.runtime_version("emulator-5554", &cancel).await.unwrap(),
            "android-35"
        );

        // Wake & keyguard guard: sleeping unlocked emulator wakes cleanly; locked USB12345 fails closed!
        adb.ensure_awake_and_unlocked("emulator-5554", &cancel)
            .await
            .expect("asleep unlocked emulator should wake");
        let locked = adb
            .ensure_awake_and_unlocked("USB12345", &cancel)
            .await
            .expect_err("locked keyguard must fail closed");
        assert!(matches!(locked, AndroidDriverError::DeviceLocked { .. }));

        // Animation scale guard saves originals, zeroes, and restores exact values (including `null` -> delete).
        {
            let mut guard = AnimationScaleGuard::disable_animations(&adb, "emulator-5554", &cancel)
                .await
                .expect("disable animations");
            guard.restore().await;
        }

        // AdbForwardGuard creates forward and removes it cleanly on Drop.
        {
            let fwd = AdbForwardGuard::create(&adb, "emulator-5554", 7007, &cancel)
                .await
                .expect("create forward");
            assert_eq!(fwd.host_port(), 45123);
        }

        let log = fs::read_to_string(&log_file).unwrap();
        assert!(log.contains("-s emulator-5554 shell input keyevent KEYCODE_WAKEUP"));
        assert!(
            log.contains("-s emulator-5554 shell settings put global window_animation_scale 0")
        );
        assert!(
            log.contains("-s emulator-5554 shell settings put global window_animation_scale 1.0")
        );
        assert!(
            log.contains(
                "-s emulator-5554 shell settings delete global transition_animation_scale"
            )
        );
        assert!(
            log.contains("-s emulator-5554 shell settings put global animator_duration_scale 0.5")
        );
        assert!(log.contains("-s emulator-5554 forward --remove tcp:45123"));

        let _ = fs::remove_dir_all(&dir);
    }
}
