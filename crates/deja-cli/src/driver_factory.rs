//! Concrete driver factory and read-only native toolchain probes for `deja-cli`.
//!
//! `deja-cli` is the sole composition root permitted to wire concrete platform drivers.
//! Both [`DriverFactory`] and [`DoctorProbes`] are trait-abstracted so deterministic unit
//! and CLI tests can inject mock drivers/probes with zero simulator or toolchain dependency.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use deja_driver_ios::cache::{
    build_cache_key, detect_toolchain, native_simulator_arch, resolve_deja_home,
};
use deja_driver_ios::{
    IosDriver, IosDriverConfig, Simctl, SimctlEnv, default_crash_report_dir, embed,
    find_device_runtime_version,
};
use deja_flow::{PlatformDriver, decode_xml};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::fs_safe::open_read_nofollow;

const MAX_INFO_PLIST_BYTES: u64 = 4 * 1024 * 1024;

/// Future type returned by [`DriverFactory`] methods.
pub type FactoryFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Error returned while creating a driver or resolving a device runtime.
#[derive(Debug, thiserror::Error)]
pub enum DriverFactoryError {
    /// Unsupported platform or invalid configuration -> CLI exit `2`.
    #[error("{0}")]
    UnsupportedOrConfig(String),
    /// Cancelled before driver startup completed -> CLI exit `1`.
    #[error("operation cancelled")]
    Cancelled,
    /// Live driver/toolchain failure -> CLI exit `1`.
    #[error("{0}")]
    Runtime(String),
}

impl DriverFactoryError {
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::UnsupportedOrConfig(_) => crate::exit_codes::EXIT_USAGE_OR_CONFIG,
            Self::Cancelled | Self::Runtime(_) => crate::exit_codes::EXIT_FAILURE,
        }
    }
}

/// Parameters passed to [`DriverFactory`] to resolve runtime, build a driver, and install an app.
#[derive(Debug, Clone)]
pub struct DriverContext {
    pub platform: String,
    pub device: String,
    pub app_id: String,
    pub build_path: PathBuf,
    pub workspace_root: PathBuf,
    pub disable_animations: bool,
    pub crash_report_grace_ms: u64,
    pub crash_report_dir: Option<PathBuf>,
    pub deja_home: Option<PathBuf>,
    pub timeout_ms: u64,
    pub env: BTreeMap<String, String>,
}

/// Abstract factory for constructing concrete [`PlatformDriver`] instances.
pub trait DriverFactory: Send + Sync {
    /// Resolve the normalized OS runtime identifier for the target device (for example `"ios-27.2"`).
    fn resolve_runtime<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Option<String>, DriverFactoryError>>;

    /// Instantiate a concrete [`PlatformDriver`] for the target platform and device.
    fn create<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Box<dyn PlatformDriver>, DriverFactoryError>>;

    /// Ensure the fingerprinted build artifact is installed on the target device (no-op for mocks/web).
    fn install_app<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<(), DriverFactoryError>>;
}

/// Production [`DriverFactory`] wiring `IosDriver` (and clear exit-2 notices for Phase 4 stubs).
#[derive(Debug, Default, Clone, Copy)]
pub struct RealDriverFactory;

fn simctl_env_from_map(env: &BTreeMap<String, String>) -> SimctlEnv {
    SimctlEnv {
        env_path: env.get("PATH").map(OsString::from),
        developer_dir: env.get("DEVELOPER_DIR").map(OsString::from),
    }
}

impl DriverFactory for RealDriverFactory {
    fn resolve_runtime<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Option<String>, DriverFactoryError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverFactoryError::Cancelled);
            }
            match ctx.platform.as_str() {
                "ios" => {
                    let simctl = Simctl::with_env(simctl_env_from_map(&ctx.env));
                    let rt = tokio::select! {
                        _ = cancel.cancelled() => return Err(DriverFactoryError::Cancelled),
                        res = simctl.device_runtime_version(&ctx.device) => res,
                    }
                    .map_err(|err| {
                        DriverFactoryError::Runtime(format!(
                            "failed to resolve iOS simulator runtime for {}: {err}",
                            ctx.device
                        ))
                    })?;
                    Ok(Some(rt))
                }
                "android" => Err(DriverFactoryError::UnsupportedOrConfig(
                    "native Android driver execution is not wired yet (tracked in Phase 4 / DEJA-66)"
                        .to_string(),
                )),
                "web" => Err(DriverFactoryError::UnsupportedOrConfig(
                    "native Web CDP driver execution is not wired yet (tracked in Phase 4 / DEJA-67)"
                        .to_string(),
                )),
                other => Err(DriverFactoryError::UnsupportedOrConfig(format!(
                    "unsupported platform {other:?}"
                ))),
            }
        })
    }

    fn create<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Box<dyn PlatformDriver>, DriverFactoryError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverFactoryError::Cancelled);
            }
            match ctx.platform.as_str() {
                "ios" => {
                    verify_ios_app_bundle(&ctx.build_path, &ctx.app_id)
                        .map_err(DriverFactoryError::UnsupportedOrConfig)?;
                    let mut cfg = IosDriverConfig::new(&ctx.device)
                        .with_disable_animations(ctx.disable_animations);
                    cfg.env = simctl_env_from_map(&ctx.env);
                    cfg.deja_home = ctx.deja_home.clone().or_else(|| {
                        ctx.env
                            .get("DEJA_HOME")
                            .filter(|v| !v.is_empty())
                            .map(PathBuf::from)
                    });
                    cfg.crash_report_grace = Duration::from_millis(ctx.crash_report_grace_ms);
                    cfg.crash_report_dir = ctx
                        .crash_report_dir
                        .clone()
                        .or_else(default_crash_report_dir);
                    cfg.auto_boot_simulator = true;
                    let driver = IosDriver::new(cfg).map_err(|err| {
                        DriverFactoryError::UnsupportedOrConfig(format!(
                            "failed to initialize iOS driver: {err}"
                        ))
                    })?;
                    Ok(Box::new(driver) as Box<dyn PlatformDriver>)
                }
                "android" => Err(DriverFactoryError::UnsupportedOrConfig(
                    "native Android driver execution is not wired yet (tracked in Phase 4 / DEJA-66)"
                        .to_string(),
                )),
                "web" => Err(DriverFactoryError::UnsupportedOrConfig(
                    "native Web CDP driver execution is not wired yet (tracked in Phase 4 / DEJA-67)"
                        .to_string(),
                )),
                other => Err(DriverFactoryError::UnsupportedOrConfig(format!(
                    "unsupported platform {other:?}"
                ))),
            }
        })
    }

    fn install_app<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<(), DriverFactoryError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverFactoryError::Cancelled);
            }
            if ctx.platform == "ios" {
                verify_ios_app_bundle(&ctx.build_path, &ctx.app_id)
                    .map_err(DriverFactoryError::UnsupportedOrConfig)?;
                let simctl = Simctl::with_env(simctl_env_from_map(&ctx.env));
                tokio::select! {
                    _ = cancel.cancelled() => return Err(DriverFactoryError::Cancelled),
                    res = simctl.install_app(&ctx.device, &ctx.build_path) => res,
                }
                .map_err(|err| {
                    DriverFactoryError::Runtime(format!(
                        "failed to install {} onto simulator {}: {err}",
                        ctx.build_path.display(),
                        ctx.device
                    ))
                })?;
            }
            Ok(())
        })
    }
}

/// Strictly validate that an iOS `buildPath` is a non-symlink `.app` bundle directory containing
/// a regular, non-symlink `Info.plist` whose `CFBundleIdentifier` matches `expected_app_id`.
///
/// Supports both XML plists (pure Rust) and binary plists (via `/usr/bin/plutil` on macOS).
pub fn verify_ios_app_bundle(build_path: &Path, expected_app_id: &str) -> Result<(), String> {
    let lmeta = fs::symlink_metadata(build_path).map_err(|err| {
        format!(
            "iOS buildPath cannot be inspected ({}): {err}",
            build_path.display()
        )
    })?;
    if lmeta.file_type().is_symlink() {
        return Err(format!(
            "iOS buildPath must not be a symbolic link: {}",
            build_path.display()
        ));
    }
    if !lmeta.file_type().is_dir() || build_path.extension().and_then(|s| s.to_str()) != Some("app")
    {
        return Err(format!(
            "iOS buildPath must be an existing `.app` bundle directory (got {})",
            build_path.display()
        ));
    }

    let info_plist = build_path.join("Info.plist");
    let file = open_read_nofollow(&info_plist).map_err(|err| {
        format!(
            "iOS `.app` bundle is missing a readable non-symlink Info.plist ({}): {err}",
            info_plist.display()
        )
    })?;
    let meta = file
        .metadata()
        .map_err(|err| format!("cannot stat Info.plist ({}): {err}", info_plist.display()))?;
    if meta.len() > MAX_INFO_PLIST_BYTES {
        return Err(format!(
            "Info.plist exceeds {MAX_INFO_PLIST_BYTES} byte limit: {}",
            info_plist.display()
        ));
    }
    let mut bytes =
        Vec::with_capacity(usize::try_from(meta.len().min(MAX_INFO_PLIST_BYTES)).unwrap_or(0));
    file.take(MAX_INFO_PLIST_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|err| {
            format!(
                "failed to read Info.plist ({}): {err}",
                info_plist.display()
            )
        })?;
    if (bytes.len() as u64) > MAX_INFO_PLIST_BYTES {
        return Err(format!(
            "Info.plist exceeds {MAX_INFO_PLIST_BYTES} byte limit: {}",
            info_plist.display()
        ));
    }

    let bundle_id = if bytes.starts_with(b"bplist") {
        extract_cf_bundle_id_from_binary_plist(&info_plist, &bytes)?
    } else {
        let xml = std::str::from_utf8(&bytes).map_err(|err| {
            format!(
                "XML Info.plist is not valid UTF-8 ({}): {err}",
                info_plist.display()
            )
        })?;
        extract_cf_bundle_id_from_xml_plist(xml)?
    };

    if bundle_id != expected_app_id {
        return Err(format!(
            "iOS `.app` bundle CFBundleIdentifier ({bundle_id:?}) does not match configured appId ({expected_app_id:?})"
        ));
    }
    Ok(())
}

fn extract_cf_bundle_id_from_binary_plist(
    info_plist: &Path,
    bytes: &[u8],
) -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        use std::io::Write;
        let mut child = std::process::Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("failed to execute plutil for binary Info.plist: {err}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(bytes).map_err(|err| {
                format!(
                    "failed to stream binary Info.plist ({}) to plutil: {err}",
                    info_plist.display()
                )
            })?;
        }
        let output = child
            .wait_with_output()
            .map_err(|err| format!("failed waiting for plutil: {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "plutil failed to decode binary Info.plist ({}): {}",
                info_plist.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let val: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|err| format!("failed to parse plutil JSON output: {err}"))?;
        let bundle_id = val
            .get("CFBundleIdentifier")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                format!(
                    "Info.plist ({}) is missing string CFBundleIdentifier",
                    info_plist.display()
                )
            })?;
        Ok(bundle_id.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (info_plist, bytes);
        Err("binary Info.plist decoding is only supported on macOS".to_string())
    }
}

/// Extract top-level `CFBundleIdentifier` from an XML Apple property list without external XML crates.
fn extract_cf_bundle_id_from_xml_plist(xml: &str) -> Result<String, String> {
    let cleaned = strip_xml_comments(xml);
    let mut rest = cleaned.as_str();
    let mut dict_depth: usize = 0;

    while let Some(lt) = rest.find('<') {
        rest = &rest[lt..];
        let Some(gt) = rest.find('>') else {
            return Err("malformed XML Info.plist: unclosed tag".to_string());
        };
        let tag = rest[1..gt].trim();
        rest = &rest[gt + 1..];

        if tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        let self_closing = tag.ends_with('/');
        let tag_name = tag
            .trim_end_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");

        match tag_name {
            "dict" if !self_closing => dict_depth = dict_depth.saturating_add(1),
            "/dict" => dict_depth = dict_depth.saturating_sub(1),
            "key" if !self_closing && dict_depth == 1 => {
                let Some(key_close) = rest.find("</key>") else {
                    return Err("malformed XML Info.plist: unclosed <key> element".to_string());
                };
                let key_name = decode_xml(rest[..key_close].trim());
                rest = &rest[key_close + 6..];
                if key_name != "CFBundleIdentifier" {
                    continue;
                }
                let trimmed = rest.trim_start();
                if let Some(after_str_open) = trimmed.strip_prefix("<string>") {
                    let Some(str_close) = after_str_open.find("</string>") else {
                        return Err(
                            "malformed XML Info.plist: unclosed <string> for CFBundleIdentifier"
                                .to_string(),
                        );
                    };
                    let value = decode_xml(&after_str_open[..str_close]);
                    if value.trim().is_empty() {
                        return Err("Info.plist CFBundleIdentifier is empty".to_string());
                    }
                    return Ok(value);
                }
                return Err(
                    "Info.plist CFBundleIdentifier value must be a <string> element".to_string(),
                );
            }
            _ => {}
        }
    }

    Err("Info.plist is missing CFBundleIdentifier".to_string())
}

fn strip_xml_comments(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 4..];
        if let Some(end) = after.find("-->") {
            rest = &after[end + 3..];
        } else {
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// Status of an individual `deja doctor` probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DoctorCheckStatus {
    Ready,
    Warn,
    NotReady,
}

/// One structured check entry emitted by `deja doctor --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorCheck {
    pub name: String,
    pub status: DoctorCheckStatus,
    pub required: bool,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

/// Request context for read-only `deja doctor` probes.
#[derive(Debug, Clone)]
pub struct DoctorProbeRequest {
    pub platform: Option<String>,
    pub device: Option<String>,
    pub env: BTreeMap<String, String>,
    pub deja_home: Option<PathBuf>,
}

/// Read-only native toolchain probe abstraction for `deja doctor`.
///
/// Implementations must NEVER boot simulators, build Xcode targets, check Java/JDK, or check Maestro.
pub trait DoctorProbes: Send + Sync {
    fn probe<'a>(&'a self, req: &'a DoctorProbeRequest) -> FactoryFuture<'a, Vec<DoctorCheck>>;
}

/// Production read-only toolchain probes (Xcode + `simctl` + `DejaXCTestRunner` cache, `adb`, Chromium).
#[derive(Debug, Default, Clone, Copy)]
pub struct RealDoctorProbes;

impl DoctorProbes for RealDoctorProbes {
    fn probe<'a>(&'a self, req: &'a DoctorProbeRequest) -> FactoryFuture<'a, Vec<DoctorCheck>> {
        Box::pin(async move {
            let mut checks = Vec::new();
            let target_platform = req.platform.as_deref();
            let sim_env = simctl_env_from_map(&req.env);

            // 1. Native engine check (always ready — built directly into `deja`).
            checks.push(DoctorCheck {
                name: "native-engine".to_string(),
                status: DoctorCheckStatus::Ready,
                required: true,
                detail: format!(
                    "Déjà {} native Rust maestro/v1 engine (no Maestro or JVM required)",
                    env!("CARGO_PKG_VERSION")
                ),
                remediation: None,
            });

            // 2. iOS toolchain checks (required when platform == ios, or default on macOS).
            let ios_required = target_platform == Some("ios");
            let probe_ios = ios_required || target_platform.is_none();
            if probe_ios {
                let toolchain_res = detect_toolchain(&sim_env).await;
                match &toolchain_res {
                    Ok(tc) => {
                        checks.push(DoctorCheck {
                            name: "xcode".to_string(),
                            status: DoctorCheckStatus::Ready,
                            required: ios_required,
                            detail: format!(
                                "Xcode {} ({}), iOS Simulator SDK {}",
                                tc.xcode_version, tc.xcode_build, tc.ios_sdk
                            ),
                            remediation: None,
                        });
                    }
                    Err(err) => {
                        checks.push(DoctorCheck {
                            name: "xcode".to_string(),
                            status: if ios_required {
                                DoctorCheckStatus::NotReady
                            } else {
                                DoctorCheckStatus::Warn
                            },
                            required: ios_required,
                            detail: format!("Xcode toolchain unavailable: {err}"),
                            remediation: Some(
                                "Install Xcode on macOS and select it with: sudo xcode-select -s /Applications/Xcode.app/Contents/Developer"
                                    .to_string(),
                            ),
                        });
                    }
                }

                // Read-only `xcrun simctl list` (never boots any simulator).
                let simctl = Simctl::with_env(sim_env.clone());
                match (simctl.list_runtimes().await, simctl.list_devices().await) {
                    (Ok(runtimes), Ok(devices)) => {
                        let available_ios: Vec<String> = runtimes
                            .iter()
                            .filter(|rt| rt.is_available && rt.normalized.starts_with("ios-"))
                            .map(|rt| rt.normalized.clone())
                            .collect();
                        if available_ios.is_empty() {
                            checks.push(DoctorCheck {
                                name: "ios-simulators".to_string(),
                                status: if ios_required {
                                    DoctorCheckStatus::NotReady
                                } else {
                                    DoctorCheckStatus::Warn
                                },
                                required: ios_required,
                                detail: "No available iOS simulator runtimes found".to_string(),
                                remediation: Some(
                                    "Download an iOS Simulator runtime with: xcodebuild -downloadPlatform iOS"
                                        .to_string(),
                                ),
                            });
                        } else if let Some(ref udid) = req.device {
                            match find_device_runtime_version(&devices, udid) {
                                Ok(rt) => {
                                    let state = devices
                                        .iter()
                                        .find(|d| d.udid.eq_ignore_ascii_case(udid))
                                        .map(|d| d.state.as_str())
                                        .unwrap_or("Unknown");
                                    checks.push(DoctorCheck {
                                        name: "ios-simulators".to_string(),
                                        status: DoctorCheckStatus::Ready,
                                        required: ios_required,
                                        detail: format!(
                                            "Simulator {udid} found ({rt}, state={state}); {} iOS runtime(s) available",
                                            available_ios.len()
                                        ),
                                        remediation: None,
                                    });
                                }
                                Err(err) => {
                                    checks.push(DoctorCheck {
                                        name: "ios-simulators".to_string(),
                                        status: DoctorCheckStatus::NotReady,
                                        required: true,
                                        detail: format!("Target simulator {udid} not ready: {err}"),
                                        remediation: Some(
                                            "Verify the simulator UDID via `xcrun simctl list devices available` or pass `--device <UDID>`"
                                                .to_string(),
                                        ),
                                    });
                                }
                            }
                        } else {
                            checks.push(DoctorCheck {
                                name: "ios-simulators".to_string(),
                                status: DoctorCheckStatus::Ready,
                                required: ios_required,
                                detail: format!(
                                    "{} available iOS runtime(s): {}",
                                    available_ios.len(),
                                    available_ios.join(", ")
                                ),
                                remediation: None,
                            });
                        }
                    }
                    (Err(err), _) | (_, Err(err)) => {
                        checks.push(DoctorCheck {
                            name: "ios-simulators".to_string(),
                            status: if ios_required {
                                DoctorCheckStatus::NotReady
                            } else {
                                DoctorCheckStatus::Warn
                            },
                            required: ios_required,
                            detail: format!("`xcrun simctl` probe failed: {err}"),
                            remediation: Some(
                                "Ensure Xcode Command Line Tools are installed and CoreSimulator is available"
                                    .to_string(),
                            ),
                        });
                    }
                }

                // Read-only `DejaXCTestRunner` cache status (never builds Xcode!).
                if let Ok(tc) = toolchain_res {
                    let deja_home_override = req.deja_home.as_deref().or_else(|| {
                        req.env
                            .get("DEJA_HOME")
                            .filter(|v| !v.is_empty())
                            .map(Path::new)
                    });
                    match resolve_deja_home(deja_home_override) {
                        Ok(home) => {
                            let source_sha = embed::source_sha256();
                            let arch = native_simulator_arch();
                            let cache_key =
                                build_cache_key(&tc.xcode_build, &tc.ios_sdk, arch, &source_sha);
                            let marker = home
                                .join("ios-runner")
                                .join("builds")
                                .join(&cache_key)
                                .join("ready.marker");
                            if marker.is_file() {
                                checks.push(DoctorCheck {
                                    name: "ios-runner-cache".to_string(),
                                    status: DoctorCheckStatus::Ready,
                                    required: false,
                                    detail: format!(
                                        "DejaXCTestRunner prebuilt cache hit ({cache_key})"
                                    ),
                                    remediation: None,
                                });
                            } else {
                                checks.push(DoctorCheck {
                                    name: "ios-runner-cache".to_string(),
                                    status: DoctorCheckStatus::Warn,
                                    required: false,
                                    detail: format!(
                                        "DejaXCTestRunner cache cold for key {cache_key} (not built yet)"
                                    ),
                                    remediation: Some(
                                        "No action required: Déjà automatically builds and caches DejaXCTestRunner on the first iOS run"
                                            .to_string(),
                                    ),
                                });
                            }
                        }
                        Err(err) => {
                            checks.push(DoctorCheck {
                                name: "ios-runner-cache".to_string(),
                                status: DoctorCheckStatus::Warn,
                                required: false,
                                detail: format!("Cannot resolve DEJA_HOME: {err}"),
                                remediation: Some(
                                    "Set DEJA_HOME or HOME to a writable directory".to_string(),
                                ),
                            });
                        }
                    }
                }
            }

            // 3. Android `adb` probe (required only when platform == android).
            let android_required = target_platform == Some("android");
            if android_required || target_platform.is_none() {
                checks.push(probe_adb(&req.env, android_required).await);
            }

            // 4. Web Chromium probe (required only when platform == web).
            let web_required = target_platform == Some("web");
            if web_required || target_platform.is_none() {
                checks.push(probe_chromium(&req.env, web_required).await);
            }

            checks
        })
    }
}

async fn probe_adb(env: &BTreeMap<String, String>, required: bool) -> DoctorCheck {
    let mut cmd = Command::new("adb");
    cmd.arg("version").stdin(Stdio::null());
    if let Some(path) = env.get("PATH") {
        cmd.env("PATH", path);
    }
    match cmd.output().await {
        Ok(out) if out.status.success() => {
            let first_line = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("adb available")
                .trim()
                .to_string();
            DoctorCheck {
                name: "android-adb".to_string(),
                status: DoctorCheckStatus::Ready,
                required,
                detail: first_line,
                remediation: None,
            }
        }
        Ok(out) => DoctorCheck {
            name: "android-adb".to_string(),
            status: if required {
                DoctorCheckStatus::NotReady
            } else {
                DoctorCheckStatus::Warn
            },
            required,
            detail: format!(
                "`adb version` exited with {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            remediation: Some(
                "Install Android Platform-Tools and ensure `adb` is on PATH (or set ANDROID_HOME)"
                    .to_string(),
            ),
        },
        Err(_) => DoctorCheck {
            name: "android-adb".to_string(),
            status: if required {
                DoctorCheckStatus::NotReady
            } else {
                DoctorCheckStatus::Warn
            },
            required,
            detail: "`adb` was not found on PATH".to_string(),
            remediation: Some(
                "Install Android Platform-Tools and add platform-tools to PATH".to_string(),
            ),
        },
    }
}

async fn probe_chromium(env: &BTreeMap<String, String>, required: bool) -> DoctorCheck {
    let candidates = [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ];
    for candidate in candidates {
        let mut cmd = Command::new(candidate);
        cmd.arg("--version").stdin(Stdio::null());
        if let Some(path) = env.get("PATH") {
            cmd.env("PATH", path);
        }
        if let Ok(out) = cmd.output().await
            && out.status.success()
        {
            let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
            return DoctorCheck {
                name: "web-chromium".to_string(),
                status: DoctorCheckStatus::Ready,
                required,
                detail: if version.is_empty() {
                    format!("Chromium available ({candidate})")
                } else {
                    version
                },
                remediation: None,
            };
        }
    }
    DoctorCheck {
        name: "web-chromium".to_string(),
        status: if required {
            DoctorCheckStatus::NotReady
        } else {
            DoctorCheckStatus::Warn
        },
        required,
        detail: "Chromium / Google Chrome executable not found".to_string(),
        remediation: Some(
            "Install Chromium or Google Chrome and ensure it is available on PATH or in /Applications"
                .to_string(),
        ),
    }
}
