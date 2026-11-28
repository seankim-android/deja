use crate::error::{IosDriverError, Result};
pub use crate::process::validate_udid;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::debug;

const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// Optional environment overrides for `xcrun simctl` commands (used to strip Maestro/Java from
/// `PATH` in tests or pin `DEVELOPER_DIR` without mutating process-global environment).
#[derive(Debug, Clone, Default)]
pub struct SimctlEnv {
    pub env_path: Option<OsString>,
    pub developer_dir: Option<OsString>,
}

/// Wrapper around `xcrun simctl` with optional environment overrides.
#[derive(Debug, Clone, Default)]
pub struct Simctl {
    env: SimctlEnv,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimRuntime {
    pub identifier: String,
    pub normalized: String,
    pub name: String,
    pub version: String,
    pub is_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimDevice {
    pub udid: String,
    pub name: String,
    pub state: String,
    pub is_available: bool,
    pub runtime_identifier: String,
    pub runtime_normalized: String,
    pub device_type_identifier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawRuntimesOutput {
    runtimes: Vec<RawRuntime>,
}

#[derive(Debug, Deserialize)]
struct RawRuntime {
    identifier: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(rename = "isAvailable", default)]
    is_available: bool,
}

#[derive(Debug, Deserialize)]
struct RawDevicesOutput {
    devices: BTreeMap<String, Vec<RawDevice>>,
}

#[derive(Debug, Deserialize)]
struct RawDevice {
    udid: String,
    name: String,
    state: String,
    #[serde(rename = "isAvailable", default)]
    is_available: bool,
    #[serde(rename = "deviceTypeIdentifier")]
    device_type_identifier: Option<String>,
}

/// Validates an Apple bundle identifier (reverse-DNS format, no path/shell/control characters).
pub fn validate_bundle_id(bundle_id: &str) -> Result<String> {
    if bundle_id.is_empty() || bundle_id.len() > 255 || !bundle_id.contains('.') {
        return Err(IosDriverError::InvalidBundleId(bundle_id.to_string()));
    }
    for segment in bundle_id.split('.') {
        if segment.is_empty()
            || !segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(IosDriverError::InvalidBundleId(bundle_id.to_string()));
        }
    }
    Ok(bundle_id.to_string())
}

/// Normalizes Apple CoreSimulator runtime identifiers into a canonical `<platform>-<version>` string.
///
/// Examples:
/// - `com.apple.CoreSimulator.SimRuntime.iOS-27-2` -> `"ios-27.2"`
/// - `com.apple.CoreSimulator.SimRuntime.iOS-27-2-iPhone19-4` -> `"ios-27.2"`
/// - `SimRuntime.iOS-18-3-1` -> `"ios-18.3.1"`
/// - `"ios-27.2"` / `"iOS 27.2"` -> `"ios-27.2"`
pub fn normalize_runtime_identifier(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let suffix = trimmed
        .strip_prefix("com.apple.CoreSimulator.SimRuntime.")
        .or_else(|| trimmed.strip_prefix("SimRuntime."))
        .unwrap_or(trimmed);

    let Some(sep_idx) = suffix
        .bytes()
        .position(|b| b == b'-' || b.is_ascii_whitespace())
    else {
        return suffix.to_ascii_lowercase();
    };
    let platform = suffix[..sep_idx].trim();
    let rest = suffix[sep_idx + 1..].trim_start_matches(['-', ' ', '\t']);
    if platform.is_empty() || rest.is_empty() {
        return suffix.to_ascii_lowercase();
    }

    let mut version_parts: Vec<&str> = Vec::new();
    for token in rest.split(|c: char| c == '-' || c.is_ascii_whitespace()) {
        if token.is_empty() {
            continue;
        }
        if token.bytes().all(|b| b.is_ascii_digit()) {
            version_parts.push(token);
        } else if token.contains('.')
            && token
                .split('.')
                .all(|seg| !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()))
        {
            version_parts.extend(token.split('.'));
        } else {
            // Stop at device-specific suffixes such as `-iPhone19-4` or build qualifiers.
            break;
        }
    }

    if version_parts.is_empty() {
        return suffix.to_ascii_lowercase();
    }

    format!(
        "{}-{}",
        platform.to_ascii_lowercase(),
        version_parts.join(".")
    )
}

/// Normalizes an iOS/CoreSimulator runtime identifier (`DEJA-55`).
///
/// Delegates to [`normalize_runtime_identifier`] so runtime normalization is consistent across
/// device listings, cache keys, and CLI diagnostics.
pub fn normalize_ios_runtime_identifier(raw: &str) -> String {
    normalize_runtime_identifier(raw)
}

/// Resolves the normalized runtime version (for example `"ios-27.2"`) for a specific simulator
/// `udid` from a parsed `simctl list devices -j` slice (case-insensitive UDID match).
pub fn find_device_runtime_version(devices: &[SimDevice], udid: &str) -> Result<String> {
    let canonical = validate_udid(udid)?;
    let device = devices
        .iter()
        .find(|dev| dev.udid.eq_ignore_ascii_case(&canonical))
        .ok_or_else(|| {
            IosDriverError::InvalidArgument(format!(
                "simulator `{canonical}` not found in simctl device list"
            ))
        })?;
    let raw = if !device.runtime_identifier.trim().is_empty() {
        device.runtime_identifier.as_str()
    } else {
        device.runtime_normalized.as_str()
    };
    let normalized = normalize_ios_runtime_identifier(raw);
    if normalized.is_empty() {
        return Err(IosDriverError::InvalidArgument(format!(
            "simulator `{canonical}` has an empty runtime identifier"
        )));
    }
    Ok(normalized)
}

/// Maps Maestro/Déjà privacy permission actions onto `xcrun simctl privacy` actions (fail-closed).
pub fn map_privacy_action(raw: &str) -> Result<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "allow" | "grant" => Ok("grant"),
        "deny" | "revoke" => Ok("revoke"),
        "unset" | "reset" => Ok("reset"),
        _ => Err(IosDriverError::InvalidArgument(format!(
            "unsupported iOS privacy action `{raw}` (expected allow, deny, or unset)"
        ))),
    }
}

/// Maps Maestro/Déjà privacy permission keys onto `xcrun simctl privacy` services (fail-closed).
pub fn map_privacy_service(raw: &str) -> Result<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "all" => Ok("all"),
        "calendar" => Ok("calendar"),
        "contacts-limited" | "contacts_limited" => Ok("contacts-limited"),
        "contacts" => Ok("contacts"),
        "location" => Ok("location"),
        "location-always" | "location_always" => Ok("location-always"),
        "photos-add" | "photos_add" => Ok("photos-add"),
        "photos" => Ok("photos"),
        "media-library" | "media_library" | "medialibrary" => Ok("media-library"),
        "microphone" => Ok("microphone"),
        "motion" => Ok("motion"),
        "reminders" => Ok("reminders"),
        "siri" => Ok("siri"),
        _ => Err(IosDriverError::InvalidArgument(format!(
            "unsupported iOS privacy service `{raw}`"
        ))),
    }
}

/// Validates media paths for `xcrun simctl addmedia` (non-empty list of existing, regular,
/// non-symlink UTF-8 file paths that do not start with `-`).
pub fn validate_media_paths(paths: &[PathBuf]) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Err(IosDriverError::InvalidArgument(
            "add_media requires at least one file path".to_string(),
        ));
    }
    let mut validated = Vec::with_capacity(paths.len());
    for path in paths {
        let path_str = path.to_str().ok_or_else(|| {
            IosDriverError::InvalidPath(format!("non-UTF-8 media path: {}", path.display()))
        })?;
        if path_str.is_empty() || path_str.starts_with('-') || path_str.contains('\0') {
            return Err(IosDriverError::InvalidPath(format!(
                "invalid media path: {path_str}"
            )));
        }
        let meta = std::fs::symlink_metadata(path).map_err(|source| {
            IosDriverError::InvalidPath(format!(
                "media file does not exist or cannot be stat'd ({}): {source}",
                path.display()
            ))
        })?;
        if meta.file_type().is_symlink() {
            return Err(IosDriverError::InvalidPath(format!(
                "refusing symlink media file: {}",
                path.display()
            )));
        }
        if !meta.is_file() {
            return Err(IosDriverError::InvalidPath(format!(
                "media path is not a regular file: {}",
                path.display()
            )));
        }
        validated.push(path_str.to_string());
    }
    Ok(validated)
}

/// Validates a URL for `xcrun simctl openurl` (non-empty, no control/NUL/CR/LF bytes, valid
/// RFC 3986 scheme parsed by `reqwest::Url`).
pub fn validate_open_link_url(url: &str) -> Result<String> {
    if url.bytes().any(|b| matches!(b, b'\0' | b'\r' | b'\n')) {
        return Err(IosDriverError::InvalidArgument(
            "URL must not contain NUL, CR, or LF characters".to_string(),
        ));
    }
    let trimmed = url.trim();
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return Err(IosDriverError::InvalidArgument(format!(
            "invalid URL `{url}`"
        )));
    }
    let parsed = reqwest::Url::parse(trimmed).map_err(|err| {
        IosDriverError::InvalidArgument(format!("invalid URL `{trimmed}`: {err}"))
    })?;
    if parsed.scheme().is_empty() {
        return Err(IosDriverError::InvalidArgument(format!(
            "URL `{trimmed}` is missing a scheme"
        )));
    }
    Ok(trimmed.to_string())
}

pub fn validate_env_key(key: &str) -> Result<()> {
    let mut bytes = key.bytes();
    let Some(first) = bytes.next() else {
        return Err(IosDriverError::InvalidArgument(
            "empty environment variable name".to_string(),
        ));
    };
    if !(first.is_ascii_alphabetic() || first == b'_')
        || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(IosDriverError::InvalidArgument(key.to_string()));
    }
    Ok(())
}

pub fn validate_launch_arg_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.trim_start_matches('-').is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
    {
        return Err(IosDriverError::InvalidArgument(key.to_string()));
    }
    Ok(())
}

/// Validates all `simctl launch` inputs (bundle ID, launch argument keys/values, and environment
/// variable keys/values) up front without mutating simulator state.
pub fn validate_launch_inputs(
    bundle_id: &str,
    args: &BTreeMap<String, String>,
    env: &BTreeMap<String, String>,
) -> Result<String> {
    let bundle = validate_bundle_id(bundle_id)?;
    for (k, v) in env {
        validate_env_key(k)?;
        if v.contains('\0') {
            return Err(IosDriverError::InvalidArgument(format!(
                "environment variable `{k}` value must not contain NUL bytes"
            )));
        }
    }
    for (k, v) in args {
        validate_launch_arg_key(k)?;
        if v.contains('\0') {
            return Err(IosDriverError::InvalidArgument(format!(
                "launch argument `{k}` value must not contain NUL bytes"
            )));
        }
    }
    Ok(bundle)
}

impl Simctl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_env(env: SimctlEnv) -> Self {
        Self { env }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new("xcrun");
        cmd.arg("simctl").stdin(Stdio::null()).kill_on_drop(true);
        if let Some(ref path) = self.env.env_path {
            cmd.env("PATH", path);
        }
        if let Some(ref dev_dir) = self.env.developer_dir {
            cmd.env("DEVELOPER_DIR", dev_dir);
        }
        cmd
    }

    async fn run_simctl(&self, args: &[&str]) -> Result<std::process::Output> {
        let mut cmd = self.command();
        cmd.args(args);
        debug!(args = ?args, "running xcrun simctl");
        let output = cmd
            .output()
            .await
            .map_err(|source| IosDriverError::CommandSpawn {
                program: "xcrun".to_string(),
                source,
            })?;
        if !output.status.success() {
            return Err(IosDriverError::CommandFailed {
                program: "xcrun simctl".to_string(),
                args: args.join(" "),
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(output)
    }

    pub async fn list_runtimes(&self) -> Result<Vec<SimRuntime>> {
        let output = self.run_simctl(&["list", "runtimes", "-j"]).await?;
        parse_runtimes_json(&output.stdout)
    }

    pub async fn list_devices(&self) -> Result<Vec<SimDevice>> {
        let output = self.run_simctl(&["list", "devices", "-j"]).await?;
        parse_devices_json(&output.stdout)
    }

    pub async fn device_runtime_version(&self, udid: &str) -> Result<String> {
        let canonical = validate_udid(udid)?;
        let devices = self.list_devices().await?;
        find_device_runtime_version(&devices, &canonical)
    }

    pub async fn clear_keychain(&self, udid: &str) -> Result<()> {
        let canonical = validate_udid(udid)?;
        self.run_simctl(&["keychain", &canonical, "reset"]).await?;
        Ok(())
    }

    pub async fn set_permissions(
        &self,
        udid: &str,
        bundle_id: &str,
        perms: &BTreeMap<String, String>,
    ) -> Result<()> {
        let canonical = validate_udid(udid)?;
        let bundle = validate_bundle_id(bundle_id)?;
        if perms.is_empty() {
            return Ok(());
        }

        // Validate every permission entry up front before mutating simulator privacy state,
        // applying `"all"` first so specific service entries can override it deterministically.
        let mut validated: Vec<(&'static str, &'static str)> = Vec::with_capacity(perms.len());
        for (raw_service, raw_action) in perms {
            let service = map_privacy_service(raw_service)?;
            let action = map_privacy_action(raw_action)?;
            validated.push((service, action));
        }
        validated.sort_by_key(|(service, _)| if *service == "all" { 0u8 } else { 1u8 });

        for (service, action) in validated {
            self.run_simctl(&["privacy", &canonical, action, service, &bundle])
                .await?;
        }
        Ok(())
    }

    pub async fn add_media(&self, udid: &str, paths: &[PathBuf]) -> Result<()> {
        let canonical = validate_udid(udid)?;
        let validated_paths = validate_media_paths(paths)?;
        let mut args: Vec<&str> = Vec::with_capacity(2 + validated_paths.len());
        args.push("addmedia");
        args.push(&canonical);
        for p in &validated_paths {
            args.push(p.as_str());
        }
        self.run_simctl(&args).await?;
        Ok(())
    }

    pub async fn open_link(&self, udid: &str, url: &str) -> Result<()> {
        let canonical = validate_udid(udid)?;
        let validated_url = validate_open_link_url(url)?;
        self.run_simctl(&["openurl", &canonical, &validated_url])
            .await?;
        Ok(())
    }

    pub async fn ensure_booted(&self, udid: &str) -> Result<()> {
        let canonical = validate_udid(udid)?;
        // `simctl bootstatus <udid> -b` boots the simulator if Shutdown and waits cleanly
        // until ready if already Booting or Booted, avoiding any check-then-boot race.
        self.run_simctl(&["bootstatus", &canonical, "-b"]).await?;
        Ok(())
    }

    pub async fn shutdown_device(&self, udid: &str) -> Result<()> {
        let canonical = validate_udid(udid)?;
        let mut cmd = self.command();
        cmd.args(["shutdown", &canonical]);
        let output = cmd
            .output()
            .await
            .map_err(|source| IosDriverError::CommandSpawn {
                program: "xcrun".to_string(),
                source,
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.contains("Unable to shutdown device in current state: Shutdown") {
                return Err(IosDriverError::CommandFailed {
                    program: "xcrun simctl".to_string(),
                    args: format!("shutdown {canonical}"),
                    status: output.status.code(),
                    stderr: stderr.trim().to_string(),
                });
            }
        }
        Ok(())
    }

    pub async fn install_app(&self, udid: &str, app_path: &Path) -> Result<()> {
        let canonical = validate_udid(udid)?;
        if !app_path.is_dir() || app_path.extension().and_then(|s| s.to_str()) != Some("app") {
            return Err(IosDriverError::InvalidPath(format!(
                "expected existing .app bundle directory, got {}",
                app_path.display()
            )));
        }
        let path_str = app_path.to_str().ok_or_else(|| {
            IosDriverError::InvalidPath(format!("non-UTF-8 app path: {}", app_path.display()))
        })?;
        self.run_simctl(&["install", &canonical, path_str]).await?;
        Ok(())
    }

    pub async fn terminate_app(&self, udid: &str, bundle_id: &str) -> Result<()> {
        let canonical = validate_udid(udid)?;
        let bundle = validate_bundle_id(bundle_id)?;
        let mut cmd = self.command();
        cmd.args(["terminate", &canonical, &bundle]);
        let output = cmd
            .output()
            .await
            .map_err(|source| IosDriverError::CommandSpawn {
                program: "xcrun".to_string(),
                source,
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Exit status 3 / "found nothing to terminate" is expected when the app is not running.
            if output.status.code() != Some(3)
                && !stderr.contains("found nothing to terminate")
                && !stderr.contains("No matching processes")
            {
                return Err(IosDriverError::CommandFailed {
                    program: "xcrun simctl".to_string(),
                    args: format!("terminate {canonical} {bundle}"),
                    status: output.status.code(),
                    stderr: stderr.trim().to_string(),
                });
            }
        }
        Ok(())
    }

    pub async fn launch_app(
        &self,
        udid: &str,
        bundle_id: &str,
        args: &BTreeMap<String, String>,
        env: &BTreeMap<String, String>,
    ) -> Result<Option<u32>> {
        let canonical = validate_udid(udid)?;
        let bundle = validate_launch_inputs(bundle_id, args, env)?;

        let mut cmd = self.command();
        cmd.args(["launch", &canonical, &bundle]);
        for (k, v) in env {
            cmd.env(format!("SIMCTL_CHILD_{k}"), v);
        }
        for (k, v) in args {
            let flag = if k.starts_with('-') {
                k.clone()
            } else {
                format!("-{k}")
            };
            cmd.arg(flag).arg(v);
        }

        let output = cmd
            .output()
            .await
            .map_err(|source| IosDriverError::CommandSpawn {
                program: "xcrun".to_string(),
                source,
            })?;
        if !output.status.success() {
            return Err(IosDriverError::CommandFailed {
                program: "xcrun simctl".to_string(),
                args: format!("launch {canonical} {bundle}"),
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let pid = stdout
            .trim()
            .rsplit(':')
            .next()
            .and_then(|s| s.trim().parse::<u32>().ok());
        Ok(pid)
    }

    pub async fn screenshot(&self, udid: &str, output_path: &Path) -> Result<Vec<u8>> {
        let canonical = validate_udid(udid)?;
        if let Some(parent) = output_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| IosDriverError::Io {
                context: format!("create screenshot directory {}", parent.display()),
                source,
            })?;
        }
        let path_str = output_path.to_str().ok_or_else(|| {
            IosDriverError::InvalidPath(format!(
                "non-UTF-8 screenshot path: {}",
                output_path.display()
            ))
        })?;
        self.run_simctl(&["io", &canonical, "screenshot", "--type=png", path_str])
            .await?;

        let bytes = tokio::fs::read(output_path)
            .await
            .map_err(|source| IosDriverError::Io {
                context: format!("read screenshot {}", output_path.display()),
                source,
            })?;
        verify_png_header(&bytes)?;
        Ok(bytes)
    }

    pub async fn clear_state(&self, udid: &str, bundle_id: &str) -> Result<()> {
        let cancel = CancellationToken::new();
        self.clear_state_cancellable(udid, bundle_id, &cancel).await
    }

    /// Terminates `bundle_id` and wipes both its confined CoreSimulator private `data` container
    /// (`Documents/`, `Library/`, `tmp/`) AND any App Group containers reported by
    /// `xcrun simctl get_app_container <udid> <bundle_id> groups`, cooperatively cancelling any
    /// in-progress directory wipe and awaiting the blocking worker to finish BEFORE returning
    /// `IosDriverError::Cancelled` so no background deletion can race a subsequent app launch.
    pub async fn clear_state_cancellable(
        &self,
        udid: &str,
        bundle_id: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let canonical = validate_udid(udid)?;
        let bundle = validate_bundle_id(bundle_id)?;
        if cancel.is_cancelled() {
            return Err(IosDriverError::Cancelled);
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(IosDriverError::Cancelled),
            res = self.terminate_app(&canonical, &bundle) => res?,
        }

        let container_args = ["get_app_container", &canonical, &bundle, "data"];
        let output = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(IosDriverError::Cancelled),
            res = self.run_simctl(&container_args) => res?,
        };
        let container_raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if container_raw.is_empty() {
            return Err(IosDriverError::InvalidPath(
                "simctl get_app_container returned an empty path".to_string(),
            ));
        }

        let group_paths = self
            .list_app_group_containers(&canonical, &bundle, cancel)
            .await?;

        if cancel.is_cancelled() {
            return Err(IosDriverError::Cancelled);
        }

        let container_path = PathBuf::from(container_raw);
        let udid_clone = canonical.clone();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let flag_for_worker = Arc::clone(&cancel_flag);
        let mut handle = tokio::task::spawn_blocking(move || {
            clear_app_data_container_cancellable(&container_path, &udid_clone, &flag_for_worker)?;
            for group_path in &group_paths {
                clear_app_group_container_cancellable(group_path, &udid_clone, &flag_for_worker)?;
            }
            Ok(())
        });

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                cancel_flag.store(true, Ordering::SeqCst);
                let _ = handle.await;
                Err(IosDriverError::Cancelled)
            }
            res = &mut handle => {
                res.map_err(|err| {
                    IosDriverError::InvalidPath(format!("clear_state task failed: {err}"))
                })??;
                Ok(())
            }
        }
    }

    async fn list_app_group_containers(
        &self,
        canonical_udid: &str,
        bundle_id: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<PathBuf>> {
        if cancel.is_cancelled() {
            return Err(IosDriverError::Cancelled);
        }
        let mut cmd = self.command();
        cmd.args(["get_app_container", canonical_udid, bundle_id, "groups"]);
        let output = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(IosDriverError::Cancelled),
            res = cmd.output() => res.map_err(|source| IosDriverError::CommandSpawn {
                program: "xcrun".to_string(),
                source,
            })?,
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let lower = stderr.to_ascii_lowercase();
            // Apps without any App Group entitlements report empty/missing group containers.
            if lower.contains("no group")
                || lower.contains("no such file")
                || lower.contains("not found")
                || lower.contains("no container")
            {
                return Ok(Vec::new());
            }
            return Err(IosDriverError::CommandFailed {
                program: "xcrun simctl".to_string(),
                args: format!("get_app_container {canonical_udid} {bundle_id} groups"),
                status: output.status.code(),
                stderr: stderr.trim().to_string(),
            });
        }
        parse_app_group_containers(&String::from_utf8_lossy(&output.stdout))
    }
}

pub fn parse_runtimes_json(bytes: &[u8]) -> Result<Vec<SimRuntime>> {
    let raw: RawRuntimesOutput =
        serde_json::from_slice(bytes).map_err(|source| IosDriverError::Json {
            context: "parse simctl list runtimes -j".to_string(),
            source,
        })?;
    Ok(raw
        .runtimes
        .into_iter()
        .map(|rt| {
            let normalized = normalize_runtime_identifier(&rt.identifier);
            SimRuntime {
                identifier: rt.identifier,
                normalized,
                name: rt.name,
                version: rt.version,
                is_available: rt.is_available,
            }
        })
        .collect())
}

pub fn parse_devices_json(bytes: &[u8]) -> Result<Vec<SimDevice>> {
    let raw: RawDevicesOutput =
        serde_json::from_slice(bytes).map_err(|source| IosDriverError::Json {
            context: "parse simctl list devices -j".to_string(),
            source,
        })?;
    let mut devices = Vec::new();
    for (runtime_identifier, list) in raw.devices {
        let runtime_normalized = normalize_runtime_identifier(&runtime_identifier);
        for dev in list {
            devices.push(SimDevice {
                udid: dev.udid,
                name: dev.name,
                state: dev.state,
                is_available: dev.is_available,
                runtime_identifier: runtime_identifier.clone(),
                runtime_normalized: runtime_normalized.clone(),
                device_type_identifier: dev.device_type_identifier,
            });
        }
    }
    Ok(devices)
}

pub fn verify_png_header(bytes: &[u8]) -> Result<()> {
    if bytes.len() < PNG_MAGIC.len() || &bytes[..PNG_MAGIC.len()] != PNG_MAGIC {
        return Err(IosDriverError::InvalidScreenshot(format!(
            "expected PNG header, got {} bytes",
            bytes.len()
        )));
    }
    Ok(())
}

/// Path-confined simulator data container wipe.
///
/// Refuses symlinks, verifies the canonical container path lies directly under
/// `.../CoreSimulator/Devices/<UDID>/data/Containers/Data/Application/<container-id>`,
/// wipes ONLY children inside `Documents/`, `Library/`, and `tmp/`, and preserves
/// `.com.apple.mobile_container_manager.metadata.plist` and the top-level directories.
pub fn clear_app_data_container(container_path: &Path, udid: &str) -> Result<()> {
    clear_app_data_container_cancellable(container_path, udid, &AtomicBool::new(false))
}

/// Cooperative cancellation variant of [`clear_app_data_container`] that checks `cancelled`
/// before each directory and child entry removal and returns `IosDriverError::Cancelled` promptly.
pub fn clear_app_data_container_cancellable(
    container_path: &Path,
    udid: &str,
    cancelled: &AtomicBool,
) -> Result<()> {
    let canonical_udid = validate_udid(udid)?;
    if cancelled.load(Ordering::SeqCst) {
        return Err(IosDriverError::Cancelled);
    }

    let meta = std::fs::symlink_metadata(container_path).map_err(|source| IosDriverError::Io {
        context: format!("stat app container {}", container_path.display()),
        source,
    })?;
    if meta.file_type().is_symlink() {
        return Err(IosDriverError::InvalidPath(format!(
            "refusing symlink app container {}",
            container_path.display()
        )));
    }
    if !meta.is_dir() {
        return Err(IosDriverError::InvalidPath(format!(
            "app container is not a directory: {}",
            container_path.display()
        )));
    }

    let canonical_container =
        std::fs::canonicalize(container_path).map_err(|source| IosDriverError::Io {
            context: format!("canonicalize app container {}", container_path.display()),
            source,
        })?;

    verify_container_confinement(&canonical_container, &canonical_udid)?;

    for subdir_name in ["Documents", "Library", "tmp"] {
        if cancelled.load(Ordering::SeqCst) {
            return Err(IosDriverError::Cancelled);
        }
        let subdir = canonical_container.join(subdir_name);
        let sub_meta = match std::fs::symlink_metadata(&subdir) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(IosDriverError::Io {
                    context: format!("stat container subdir {}", subdir.display()),
                    source,
                });
            }
        };

        if sub_meta.file_type().is_symlink() {
            return Err(IosDriverError::InvalidPath(format!(
                "refusing symlink subdir {}",
                subdir.display()
            )));
        }
        if !sub_meta.is_dir() {
            return Err(IosDriverError::InvalidPath(format!(
                "expected directory for container subdir {}",
                subdir.display()
            )));
        }

        let canonical_sub =
            std::fs::canonicalize(&subdir).map_err(|source| IosDriverError::Io {
                context: format!("canonicalize container subdir {}", subdir.display()),
                source,
            })?;
        if !canonical_sub.starts_with(&canonical_container) {
            return Err(IosDriverError::InvalidPath(format!(
                "container subdir escaped confinement: {}",
                canonical_sub.display()
            )));
        }

        let entries = std::fs::read_dir(&canonical_sub).map_err(|source| IosDriverError::Io {
            context: format!("read container subdir {}", canonical_sub.display()),
            source,
        })?;

        for entry_res in entries {
            if cancelled.load(Ordering::SeqCst) {
                return Err(IosDriverError::Cancelled);
            }
            let entry = entry_res.map_err(|source| IosDriverError::Io {
                context: format!("iterate container subdir {}", canonical_sub.display()),
                source,
            })?;
            remove_entry_cancellable(&entry.path(), cancelled)?;
        }
    }

    Ok(())
}

fn remove_entry_cancellable(path: &Path, cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::SeqCst) {
        return Err(IosDriverError::Cancelled);
    }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(IosDriverError::Io {
                context: format!("stat container entry {}", path.display()),
                source,
            });
        }
    };

    if meta.file_type().is_symlink() || !meta.is_dir() {
        std::fs::remove_file(path).map_err(|source| IosDriverError::Io {
            context: format!("remove container file {}", path.display()),
            source,
        })?;
        return Ok(());
    }

    let entries = std::fs::read_dir(path).map_err(|source| IosDriverError::Io {
        context: format!("read container directory {}", path.display()),
        source,
    })?;
    for entry_res in entries {
        if cancelled.load(Ordering::SeqCst) {
            return Err(IosDriverError::Cancelled);
        }
        let entry = entry_res.map_err(|source| IosDriverError::Io {
            context: format!("iterate container directory {}", path.display()),
            source,
        })?;
        remove_entry_cancellable(&entry.path(), cancelled)?;
    }

    if cancelled.load(Ordering::SeqCst) {
        return Err(IosDriverError::Cancelled);
    }
    std::fs::remove_dir(path).map_err(|source| IosDriverError::Io {
        context: format!("remove container directory {}", path.display()),
        source,
    })?;
    Ok(())
}

/// Parses `xcrun simctl get_app_container <udid> <bundle_id> groups` output (`<group_id>\t<path>`
/// or `<group_id> <path>` per line) into absolute container paths.
pub fn parse_app_group_containers(stdout: &str) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for raw_line in stdout.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let path_part = if let Some((_, rhs)) = line.split_once('\t') {
            rhs.trim()
        } else if let Some(idx) = line.find(char::is_whitespace) {
            line[idx..].trim()
        } else {
            return Err(IosDriverError::InvalidPath(format!(
                "malformed simctl app group container line: {line}"
            )));
        };
        if path_part.is_empty() || !path_part.starts_with('/') {
            return Err(IosDriverError::InvalidPath(format!(
                "expected absolute App Group container path, got `{path_part}`"
            )));
        }
        paths.push(PathBuf::from(path_part));
    }
    Ok(paths)
}

/// Path-confined simulator App Group container wipe with cooperative cancellation.
///
/// Refuses symlinks, verifies the canonical path lies directly under
/// `.../CoreSimulator/Devices/<UDID>/data/Containers/Shared/AppGroup/<container-id>`,
/// preserves `.com.apple.mobile_container_manager.metadata.plist` and top-level `Documents/`,
/// `Library/`, and `tmp/` directories while wiping their children AND any custom app-created
/// shared files/directories placed directly in the App Group root.
pub fn clear_app_group_container_cancellable(
    container_path: &Path,
    udid: &str,
    cancelled: &AtomicBool,
) -> Result<()> {
    let canonical_udid = validate_udid(udid)?;
    if cancelled.load(Ordering::SeqCst) {
        return Err(IosDriverError::Cancelled);
    }

    let meta = std::fs::symlink_metadata(container_path).map_err(|source| IosDriverError::Io {
        context: format!("stat App Group container {}", container_path.display()),
        source,
    })?;
    if meta.file_type().is_symlink() {
        return Err(IosDriverError::InvalidPath(format!(
            "refusing symlink App Group container {}",
            container_path.display()
        )));
    }
    if !meta.is_dir() {
        return Err(IosDriverError::InvalidPath(format!(
            "App Group container is not a directory: {}",
            container_path.display()
        )));
    }

    let canonical_container =
        std::fs::canonicalize(container_path).map_err(|source| IosDriverError::Io {
            context: format!(
                "canonicalize App Group container {}",
                container_path.display()
            ),
            source,
        })?;

    verify_app_group_confinement(&canonical_container, &canonical_udid)?;

    let entries = std::fs::read_dir(&canonical_container).map_err(|source| IosDriverError::Io {
        context: format!("read App Group container {}", canonical_container.display()),
        source,
    })?;

    for entry_res in entries {
        if cancelled.load(Ordering::SeqCst) {
            return Err(IosDriverError::Cancelled);
        }
        let entry = entry_res.map_err(|source| IosDriverError::Io {
            context: format!(
                "iterate App Group container {}",
                canonical_container.display()
            ),
            source,
        })?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == ".com.apple.mobile_container_manager.metadata.plist" {
            continue;
        }
        let child_path = entry.path();
        if matches!(name_str.as_ref(), "Documents" | "Library" | "tmp") {
            clear_confined_subdir_children(&canonical_container, &child_path, cancelled)?;
        } else {
            remove_entry_cancellable(&child_path, cancelled)?;
        }
    }

    Ok(())
}

fn clear_confined_subdir_children(
    canonical_container: &Path,
    subdir: &Path,
    cancelled: &AtomicBool,
) -> Result<()> {
    if cancelled.load(Ordering::SeqCst) {
        return Err(IosDriverError::Cancelled);
    }
    let sub_meta = match std::fs::symlink_metadata(subdir) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(IosDriverError::Io {
                context: format!("stat container subdir {}", subdir.display()),
                source,
            });
        }
    };

    if sub_meta.file_type().is_symlink() {
        return Err(IosDriverError::InvalidPath(format!(
            "refusing symlink subdir {}",
            subdir.display()
        )));
    }
    if !sub_meta.is_dir() {
        return Err(IosDriverError::InvalidPath(format!(
            "expected directory for container subdir {}",
            subdir.display()
        )));
    }

    let canonical_sub = std::fs::canonicalize(subdir).map_err(|source| IosDriverError::Io {
        context: format!("canonicalize container subdir {}", subdir.display()),
        source,
    })?;
    if !canonical_sub.starts_with(canonical_container) {
        return Err(IosDriverError::InvalidPath(format!(
            "container subdir escaped confinement: {}",
            canonical_sub.display()
        )));
    }

    let entries = std::fs::read_dir(&canonical_sub).map_err(|source| IosDriverError::Io {
        context: format!("read container subdir {}", canonical_sub.display()),
        source,
    })?;

    for entry_res in entries {
        if cancelled.load(Ordering::SeqCst) {
            return Err(IosDriverError::Cancelled);
        }
        let entry = entry_res.map_err(|source| IosDriverError::Io {
            context: format!("iterate container subdir {}", canonical_sub.display()),
            source,
        })?;
        remove_entry_cancellable(&entry.path(), cancelled)?;
    }
    Ok(())
}

fn verify_app_group_confinement(canonical_container: &Path, canonical_udid: &str) -> Result<()> {
    let components: Vec<String> = canonical_container
        .components()
        .filter_map(|c| match c {
            Component::Normal(os) => Some(os.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();

    // Expected tail: CoreSimulator / Devices / <UDID> / data / Containers / Shared / AppGroup / <container-uuid>
    let expected_prefix_len = 7;
    if components.len() < expected_prefix_len + 1 {
        return Err(IosDriverError::InvalidPath(format!(
            "App Group container path outside CoreSimulator Devices/<UDID>: {}",
            canonical_container.display()
        )));
    }

    let tail = &components[components.len() - (expected_prefix_len + 1)..];
    let matches_layout = tail[0] == "CoreSimulator"
        && tail[1] == "Devices"
        && tail[2].eq_ignore_ascii_case(canonical_udid)
        && tail[3] == "data"
        && tail[4] == "Containers"
        && tail[5] == "Shared"
        && tail[6] == "AppGroup"
        && !tail[7].is_empty()
        && tail[7] != "."
        && tail[7] != "..";

    if !matches_layout {
        return Err(IosDriverError::InvalidPath(format!(
            "App Group container path failed CoreSimulator confinement for UDID {canonical_udid}: {}",
            canonical_container.display()
        )));
    }
    Ok(())
}

fn verify_container_confinement(canonical_container: &Path, canonical_udid: &str) -> Result<()> {
    let components: Vec<String> = canonical_container
        .components()
        .filter_map(|c| match c {
            Component::Normal(os) => Some(os.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();

    // Expected tail: CoreSimulator / Devices / <UDID> / data / Containers / Data / Application / <container-uuid>
    let expected_prefix_len = 7;
    if components.len() < expected_prefix_len + 1 {
        return Err(IosDriverError::InvalidPath(format!(
            "container path outside CoreSimulator Devices/<UDID>: {}",
            canonical_container.display()
        )));
    }

    let tail = &components[components.len() - (expected_prefix_len + 1)..];
    let matches_layout = tail[0] == "CoreSimulator"
        && tail[1] == "Devices"
        && tail[2].eq_ignore_ascii_case(canonical_udid)
        && tail[3] == "data"
        && tail[4] == "Containers"
        && tail[5] == "Data"
        && tail[6] == "Application"
        && !tail[7].is_empty()
        && tail[7] != "."
        && tail[7] != "..";

    if !matches_layout {
        return Err(IosDriverError::InvalidPath(format!(
            "container path failed CoreSimulator confinement for UDID {canonical_udid}: {}",
            canonical_container.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        SimDevice, clear_app_data_container, find_device_runtime_version, map_privacy_action,
        map_privacy_service, normalize_ios_runtime_identifier, normalize_runtime_identifier,
        parse_devices_json, parse_runtimes_json, validate_bundle_id, validate_media_paths,
        validate_open_link_url, verify_png_header,
    };
    use std::path::PathBuf;

    const TEST_UDID: &str = "BA555783-AC7E-46E3-B749-34F9499B2C2B";

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "deja-simctl-{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    #[test]
    fn runtime_normalization_handles_standard_and_device_suffixed_ids() {
        for (raw, expected) in [
            ("com.apple.CoreSimulator.SimRuntime.iOS-27-2", "ios-27.2"),
            (
                "com.apple.CoreSimulator.SimRuntime.iOS-27-2-iPhone19-4",
                "ios-27.2",
            ),
            ("SimRuntime.iOS-18-3-1", "ios-18.3.1"),
            ("ios-27.2", "ios-27.2"),
            ("iOS 27.2", "ios-27.2"),
            ("iOS 18.3.1", "ios-18.3.1"),
            ("iOS-27.2-iPhone19-4", "ios-27.2"),
            (
                "com.apple.CoreSimulator.SimRuntime.watchOS-11-0",
                "watchos-11.0",
            ),
        ] {
            assert_eq!(normalize_runtime_identifier(raw), expected, "raw={raw}");
            assert_eq!(normalize_ios_runtime_identifier(raw), expected, "raw={raw}");
        }
    }

    #[test]
    fn finds_device_runtime_version_case_insensitively() {
        let devices = vec![SimDevice {
            udid: TEST_UDID.to_string(),
            name: "iPhone 17".to_string(),
            state: "Booted".to_string(),
            is_available: true,
            runtime_identifier: "com.apple.CoreSimulator.SimRuntime.iOS-27-2-iPhone19-4"
                .to_string(),
            runtime_normalized: "ios-27.2".to_string(),
            device_type_identifier: None,
        }];
        assert_eq!(
            find_device_runtime_version(&devices, &TEST_UDID.to_ascii_lowercase()).unwrap(),
            "ios-27.2"
        );
        assert!(find_device_runtime_version(&devices, "not-a-udid").is_err());
        assert!(
            find_device_runtime_version(&devices, "DA910389-DF0B-4254-BB78-7164E0E27609").is_err()
        );
    }

    #[test]
    fn privacy_action_and_service_mapping_fail_closed() {
        assert_eq!(map_privacy_action("allow").unwrap(), "grant");
        assert_eq!(map_privacy_action("GRANT").unwrap(), "grant");
        assert_eq!(map_privacy_action("deny").unwrap(), "revoke");
        assert_eq!(map_privacy_action("revoke").unwrap(), "revoke");
        assert_eq!(map_privacy_action("unset").unwrap(), "reset");
        assert_eq!(map_privacy_action("reset").unwrap(), "reset");
        assert!(map_privacy_action("prompt").is_err());
        assert!(map_privacy_action("").is_err());

        for (raw, expected) in [
            ("all", "all"),
            ("calendar", "calendar"),
            ("contacts-limited", "contacts-limited"),
            ("contacts_limited", "contacts-limited"),
            ("contacts", "contacts"),
            ("location", "location"),
            ("location-always", "location-always"),
            ("location_always", "location-always"),
            ("photos-add", "photos-add"),
            ("photos_add", "photos-add"),
            ("photos", "photos"),
            ("media-library", "media-library"),
            ("media_library", "media-library"),
            ("medialibrary", "media-library"),
            ("microphone", "microphone"),
            ("motion", "motion"),
            ("reminders", "reminders"),
            ("siri", "siri"),
        ] {
            assert_eq!(map_privacy_service(raw).unwrap(), expected, "raw={raw}");
        }
        assert!(map_privacy_service("camera-unknown").is_err());
        assert!(map_privacy_service("").is_err());
    }

    #[test]
    fn validates_media_paths_and_open_link_urls() {
        assert!(validate_media_paths(&[]).is_err());
        assert!(validate_media_paths(&[PathBuf::from("-flag.png")]).is_err());

        let root = unique_temp_dir("media");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("photo.png");
        std::fs::write(&file, b"fake").unwrap();
        assert_eq!(
            validate_media_paths(std::slice::from_ref(&file)).unwrap(),
            vec![file.to_string_lossy().into_owned()]
        );
        assert!(validate_media_paths(std::slice::from_ref(&root)).is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link = root.join("sym.png");
            symlink(&file, &link).unwrap();
            assert!(validate_media_paths(&[link]).is_err());
        }

        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(
            validate_open_link_url("https://example.com/checkout?id=1").unwrap(),
            "https://example.com/checkout?id=1"
        );
        assert_eq!(
            validate_open_link_url("myapp://cart/42").unwrap(),
            "myapp://cart/42"
        );
        for bad in [
            "",
            "   ",
            "no-scheme",
            "-flag",
            "https://example.com/\nnewline",
            "https://example.com/\rcr",
            "https://example.com/\0nul",
        ] {
            assert!(
                validate_open_link_url(bad).is_err(),
                "expected err for {bad:?}"
            );
        }
    }

    #[test]
    fn parses_simctl_runtimes_and_devices_json() {
        let runtimes_json = br#"{
          "runtimes": [
            {
              "identifier": "com.apple.CoreSimulator.SimRuntime.iOS-27-2",
              "name": "iOS 27.2",
              "version": "27.2",
              "isAvailable": true
            }
          ]
        }"#;
        let runtimes = parse_runtimes_json(runtimes_json).expect("runtimes parse");
        assert_eq!(runtimes.len(), 1);
        assert_eq!(runtimes[0].normalized, "ios-27.2");

        let devices_json = br#"{
          "devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-27-2-iPhone19-4": [
              {
                "udid": "BA555783-AC7E-46E3-B749-34F9499B2C2B",
                "name": "iPhone 17 (deja)",
                "state": "Shutdown",
                "isAvailable": true,
                "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17"
              }
            ]
          }
        }"#;
        let devices = parse_devices_json(devices_json).expect("devices parse");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].runtime_normalized, "ios-27.2");
        assert_eq!(devices[0].udid, TEST_UDID);
    }

    #[test]
    fn bundle_id_validation() {
        assert!(validate_bundle_id("dev.deja.ios.checkout").is_ok());
        assert!(validate_bundle_id("dev.deja.ios.runner-host").is_ok());
        for bad in [
            "",
            "nodots",
            ".leading",
            "trailing.",
            "a..b",
            "a/b.c",
            "a b.c",
        ] {
            assert!(validate_bundle_id(bad).is_err(), "expected err for `{bad}`");
        }
    }

    #[test]
    fn png_header_check() {
        assert!(verify_png_header(b"\x89PNG\r\n\x1a\nrest").is_ok());
        assert!(verify_png_header(b"not a png").is_err());
    }

    #[test]
    fn clear_app_data_container_wipes_children_and_preserves_metadata() {
        let root = unique_temp_dir("wipe");
        let container = root
            .join("CoreSimulator")
            .join("Devices")
            .join(TEST_UDID)
            .join("data")
            .join("Containers")
            .join("Data")
            .join("Application")
            .join("11111111-2222-3333-4444-555555555555");

        std::fs::create_dir_all(container.join("Documents/nested")).unwrap();
        std::fs::create_dir_all(container.join("Library/Preferences")).unwrap();
        std::fs::create_dir_all(container.join("tmp")).unwrap();
        std::fs::write(container.join("Documents/nested/user.txt"), b"hello").unwrap();
        std::fs::write(container.join("Library/Preferences/app.plist"), b"prefs").unwrap();
        std::fs::write(container.join("tmp/scratch"), b"tmp").unwrap();
        let metadata = container.join(".com.apple.mobile_container_manager.metadata.plist");
        std::fs::write(&metadata, b"keep-me").unwrap();

        clear_app_data_container(&container, TEST_UDID).expect("clear_state should succeed");

        assert!(metadata.exists(), "metadata plist must be preserved");
        assert!(container.join("Documents").is_dir());
        assert!(container.join("Library").is_dir());
        assert!(container.join("tmp").is_dir());
        assert_eq!(
            std::fs::read_dir(container.join("Documents"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            std::fs::read_dir(container.join("Library"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(std::fs::read_dir(container.join("tmp")).unwrap().count(), 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clear_app_data_container_refuses_wrong_udid_and_outside_paths() {
        let root = unique_temp_dir("wrong-udid");
        let container = root
            .join("CoreSimulator")
            .join("Devices")
            .join("DA910389-DF0B-4254-BB78-7164E0E27609")
            .join("data")
            .join("Containers")
            .join("Data")
            .join("Application")
            .join("11111111-2222-3333-4444-555555555555");
        std::fs::create_dir_all(container.join("Documents")).unwrap();
        std::fs::write(container.join("Documents/keep.txt"), b"untouched").unwrap();

        assert!(clear_app_data_container(&container, TEST_UDID).is_err());
        assert!(container.join("Documents/keep.txt").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn clear_app_data_container_refuses_symlinks() {
        use std::os::unix::fs::symlink;

        let root = unique_temp_dir("symlink");
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"keep").unwrap();

        let container = root
            .join("CoreSimulator")
            .join("Devices")
            .join(TEST_UDID)
            .join("data")
            .join("Containers")
            .join("Data")
            .join("Application")
            .join("22222222-2222-3333-4444-555555555555");
        std::fs::create_dir_all(&container).unwrap();
        symlink(&outside, container.join("Documents")).unwrap();

        assert!(clear_app_data_container(&container, TEST_UDID).is_err());
        assert!(outside.join("secret.txt").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn validate_launch_inputs_rejects_bad_keys_and_nul_values() {
        use super::validate_launch_inputs;
        use std::collections::BTreeMap;

        let mut ok_args = BTreeMap::new();
        ok_args.insert("DEJA_FIXTURE_ORDER".to_string(), "WC-2027".to_string());
        ok_args.insert("-flag.name".to_string(), "1".to_string());
        let mut ok_env = BTreeMap::new();
        ok_env.insert("VALID_ENV_1".to_string(), "value".to_string());
        assert!(validate_launch_inputs("dev.deja.ios.checkout", &ok_args, &ok_env).is_ok());

        for bad_arg_key in ["", "-", "--", "bad key", "k=v"] {
            let mut args = BTreeMap::new();
            args.insert(bad_arg_key.to_string(), "val".to_string());
            assert!(
                validate_launch_inputs("dev.deja.ios.checkout", &args, &BTreeMap::new()).is_err(),
                "expected invalid arg key `{bad_arg_key}`"
            );
        }

        let mut nul_arg = BTreeMap::new();
        nul_arg.insert("KEY".to_string(), "bad\0val".to_string());
        assert!(
            validate_launch_inputs("dev.deja.ios.checkout", &nul_arg, &BTreeMap::new()).is_err()
        );

        for bad_env_key in ["", "1BAD", "BAD-KEY", "BAD KEY"] {
            let mut env = BTreeMap::new();
            env.insert(bad_env_key.to_string(), "val".to_string());
            assert!(
                validate_launch_inputs("dev.deja.ios.checkout", &BTreeMap::new(), &env).is_err(),
                "expected invalid env key `{bad_env_key}`"
            );
        }

        let mut nul_env = BTreeMap::new();
        nul_env.insert("GOOD_KEY".to_string(), "bad\0val".to_string());
        assert!(
            validate_launch_inputs("dev.deja.ios.checkout", &BTreeMap::new(), &nul_env).is_err()
        );
    }

    #[test]
    fn clear_app_data_container_cancellable_stops_immediately_when_cancelled() {
        use super::clear_app_data_container_cancellable;
        use crate::error::IosDriverError;
        use std::sync::atomic::AtomicBool;

        let root = unique_temp_dir("cancel-wipe");
        let container = root
            .join("CoreSimulator")
            .join("Devices")
            .join(TEST_UDID)
            .join("data")
            .join("Containers")
            .join("Data")
            .join("Application")
            .join("33333333-2222-3333-4444-555555555555");
        std::fs::create_dir_all(container.join("Documents")).unwrap();
        let keep = container.join("Documents/keep.txt");
        std::fs::write(&keep, b"untouched").unwrap();

        let cancelled = AtomicBool::new(true);
        let err = clear_app_data_container_cancellable(&container, TEST_UDID, &cancelled)
            .expect_err("cancelled wipe must return Cancelled");
        assert!(matches!(err, IosDriverError::Cancelled));
        assert!(keep.exists(), "cancelled wipe must not delete entries");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parses_app_group_containers_and_wipes_confined_app_groups() {
        use super::{clear_app_group_container_cancellable, parse_app_group_containers};
        use std::sync::atomic::AtomicBool;

        let root = unique_temp_dir("app-group");
        let group_container = root
            .join("CoreSimulator")
            .join("Devices")
            .join(TEST_UDID)
            .join("data")
            .join("Containers")
            .join("Shared")
            .join("AppGroup")
            .join("55555555-2222-3333-4444-555555555555");
        std::fs::create_dir_all(group_container.join("Library/Preferences")).unwrap();
        std::fs::create_dir_all(group_container.join("SharedCache/sub")).unwrap();
        std::fs::write(
            group_container.join("Library/Preferences/group.dev.deja.plist"),
            b"prefs",
        )
        .unwrap();
        std::fs::write(group_container.join("shared.sqlite"), b"db").unwrap();
        std::fs::write(group_container.join("SharedCache/sub/item"), b"cache").unwrap();
        let metadata = group_container.join(".com.apple.mobile_container_manager.metadata.plist");
        std::fs::write(&metadata, b"keep-meta").unwrap();

        let parsed = parse_app_group_containers(&format!(
            "group.dev.deja.one\t{}\ngroup.dev.deja.two   /tmp/other\n\n",
            group_container.display()
        ))
        .expect("parse app groups");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], group_container);
        assert_eq!(parsed[1], PathBuf::from("/tmp/other"));
        assert!(parse_app_group_containers("").unwrap().is_empty());
        assert!(parse_app_group_containers("malformed-without-path").is_err());
        assert!(parse_app_group_containers("group.id\trelative/path").is_err());

        clear_app_group_container_cancellable(&group_container, TEST_UDID, &AtomicBool::new(false))
            .expect("app group wipe should succeed");
        assert!(metadata.exists(), "metadata plist must be preserved");
        assert!(group_container.join("Library").is_dir());
        assert_eq!(
            std::fs::read_dir(group_container.join("Library"))
                .unwrap()
                .count(),
            0
        );
        assert!(!group_container.join("shared.sqlite").exists());
        assert!(!group_container.join("SharedCache").exists());

        // Reject outside-simulator path or wrong UDID.
        let outside = root.join("outside-group");
        std::fs::create_dir_all(&outside).unwrap();
        assert!(
            clear_app_group_container_cancellable(&outside, TEST_UDID, &AtomicBool::new(false))
                .is_err()
        );
        assert!(
            clear_app_group_container_cancellable(
                &group_container,
                "DA910389-DF0B-4254-BB78-7164E0E27609",
                &AtomicBool::new(false)
            )
            .is_err()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clear_state_cancellable_wipes_both_data_and_app_group_containers() {
        use super::{Simctl, SimctlEnv};
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt;
        use tokio_util::sync::CancellationToken;

        let root = unique_temp_dir("clear-state-groups");
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let data_container = root
            .join("CoreSimulator")
            .join("Devices")
            .join(TEST_UDID)
            .join("data")
            .join("Containers")
            .join("Data")
            .join("Application")
            .join("11111111-2222-3333-4444-555555555555");
        let group_container = root
            .join("CoreSimulator")
            .join("Devices")
            .join(TEST_UDID)
            .join("data")
            .join("Containers")
            .join("Shared")
            .join("AppGroup")
            .join("66666666-2222-3333-4444-555555555555");

        std::fs::create_dir_all(data_container.join("Documents")).unwrap();
        std::fs::write(data_container.join("Documents/user.txt"), b"private").unwrap();
        std::fs::create_dir_all(group_container.join("Library/Preferences")).unwrap();
        std::fs::write(
            group_container.join("Library/Preferences/group.dev.deja.plist"),
            b"shared",
        )
        .unwrap();
        std::fs::write(group_container.join("shared.db"), b"sqlite").unwrap();

        let xcrun_path = bin_dir.join("xcrun");
        std::fs::write(
            &xcrun_path,
            format!(
                r#"#!/bin/sh
if [ "$2" = "get_app_container" ] && [ "$5" = "data" ]; then
  echo "{data}"
  exit 0
fi
if [ "$2" = "get_app_container" ] && [ "$5" = "groups" ]; then
  printf 'group.dev.deja.shared\t%s\n' "{group}"
  exit 0
fi
exit 0
"#,
                data = data_container.display(),
                group = group_container.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&xcrun_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let simctl = Simctl::with_env(SimctlEnv {
            env_path: Some(OsString::from(bin_dir.as_os_str())),
            developer_dir: None,
        });
        let cancel = CancellationToken::new();
        simctl
            .clear_state_cancellable(TEST_UDID, "dev.deja.ios.checkout", &cancel)
            .await
            .expect("clear_state should wipe both private data and App Group");

        assert!(!data_container.join("Documents/user.txt").exists());
        assert!(
            !group_container
                .join("Library/Preferences/group.dev.deja.plist")
                .exists()
        );
        assert!(!group_container.join("shared.db").exists());

        let _ = std::fs::remove_dir_all(&root);
    }
}
