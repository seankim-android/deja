use crate::error::{IosDriverError, Result};
pub use crate::process::validate_udid;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;
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
pub fn normalize_runtime_identifier(raw: &str) -> String {
    let trimmed = raw.trim();
    let suffix = trimmed
        .strip_prefix("com.apple.CoreSimulator.SimRuntime.")
        .or_else(|| trimmed.strip_prefix("SimRuntime."))
        .unwrap_or(trimmed);

    let mut parts = suffix.split('-');
    let Some(platform) = parts.next() else {
        return trimmed.to_ascii_lowercase();
    };
    let version_parts: Vec<&str> = parts
        .take_while(|seg| !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()))
        .collect();

    if version_parts.is_empty() {
        return suffix.to_ascii_lowercase();
    }

    format!(
        "{}-{}",
        platform.to_ascii_lowercase(),
        version_parts.join(".")
    )
}

fn validate_env_key(key: &str) -> Result<()> {
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

fn validate_launch_arg_key(key: &str) -> Result<()> {
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
    {
        return Err(IosDriverError::InvalidArgument(key.to_string()));
    }
    Ok(())
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
        cmd.arg("simctl").stdin(Stdio::null());
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

    pub async fn ensure_booted(&self, udid: &str) -> Result<()> {
        let canonical = validate_udid(udid)?;
        let devices = self.list_devices().await?;
        let device = devices
            .iter()
            .find(|d| d.udid.eq_ignore_ascii_case(&canonical))
            .ok_or_else(|| {
                IosDriverError::InvalidUdid(format!("{canonical} (not found in simctl devices)"))
            })?;

        if !device.state.eq_ignore_ascii_case("Booted") {
            let mut cmd = self.command();
            cmd.args(["boot", &canonical]);
            let output = cmd
                .output()
                .await
                .map_err(|source| IosDriverError::CommandSpawn {
                    program: "xcrun".to_string(),
                    source,
                })?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.contains("Unable to boot device in current state: Booted") {
                    return Err(IosDriverError::CommandFailed {
                        program: "xcrun simctl".to_string(),
                        args: format!("boot {canonical}"),
                        status: output.status.code(),
                        stderr: stderr.trim().to_string(),
                    });
                }
            }
        }

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
        let bundle = validate_bundle_id(bundle_id)?;

        let mut cmd = self.command();
        cmd.args(["launch", &canonical, &bundle]);
        for (k, v) in env {
            validate_env_key(k)?;
            cmd.env(format!("SIMCTL_CHILD_{k}"), v);
        }
        for (k, v) in args {
            validate_launch_arg_key(k)?;
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
        let canonical = validate_udid(udid)?;
        let bundle = validate_bundle_id(bundle_id)?;
        self.terminate_app(&canonical, &bundle).await?;

        let output = self
            .run_simctl(&["get_app_container", &canonical, &bundle, "data"])
            .await?;
        let container_raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if container_raw.is_empty() {
            return Err(IosDriverError::InvalidPath(
                "simctl get_app_container returned an empty path".to_string(),
            ));
        }

        let container_path = PathBuf::from(container_raw);
        let udid_clone = canonical.clone();
        tokio::task::spawn_blocking(move || clear_app_data_container(&container_path, &udid_clone))
            .await
            .map_err(|err| {
                IosDriverError::InvalidPath(format!("clear_state task failed: {err}"))
            })??;
        Ok(())
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
    let canonical_udid = validate_udid(udid)?;

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
            let entry = entry_res.map_err(|source| IosDriverError::Io {
                context: format!("iterate container subdir {}", canonical_sub.display()),
                source,
            })?;
            let child_path = entry.path();
            let child_meta =
                std::fs::symlink_metadata(&child_path).map_err(|source| IosDriverError::Io {
                    context: format!("stat container entry {}", child_path.display()),
                    source,
                })?;

            if child_meta.file_type().is_symlink() || !child_meta.is_dir() {
                std::fs::remove_file(&child_path).map_err(|source| IosDriverError::Io {
                    context: format!("remove container file {}", child_path.display()),
                    source,
                })?;
            } else {
                std::fs::remove_dir_all(&child_path).map_err(|source| IosDriverError::Io {
                    context: format!("remove container directory {}", child_path.display()),
                    source,
                })?;
            }
        }
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
        clear_app_data_container, normalize_runtime_identifier, parse_devices_json,
        parse_runtimes_json, validate_bundle_id, verify_png_header,
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
        assert_eq!(
            normalize_runtime_identifier("com.apple.CoreSimulator.SimRuntime.iOS-27-2"),
            "ios-27.2"
        );
        assert_eq!(
            normalize_runtime_identifier("com.apple.CoreSimulator.SimRuntime.iOS-27-2-iPhone19-4"),
            "ios-27.2"
        );
        assert_eq!(
            normalize_runtime_identifier("SimRuntime.iOS-18-3-1"),
            "ios-18.3.1"
        );
        assert_eq!(
            normalize_runtime_identifier("com.apple.CoreSimulator.SimRuntime.watchOS-11-0"),
            "watchos-11.0"
        );
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
}
