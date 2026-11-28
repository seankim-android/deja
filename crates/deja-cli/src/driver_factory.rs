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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deja_core::{fingerprint_file, fingerprint_tree};
use deja_driver_android::{
    Adb, AgentApkReadiness, AndroidDriver, AndroidDriverConfig, AndroidDriverError, discover_adb,
    inspect_agent_apk_readiness, verify_and_hash_apk,
};
use deja_driver_ios::cache::{
    build_cache_key, detect_toolchain, native_simulator_arch, resolve_deja_home,
};
use deja_driver_ios::{
    IosDriver, IosDriverConfig, Simctl, SimctlEnv, default_crash_report_dir, embed,
    find_device_runtime_version,
};
use deja_driver_web::{
    ScreenSize, WebDriver, WebDriverConfig, WebDriverError, discover_working_chromium,
    format_web_runtime_suffix,
};
use deja_flow::{PlatformDriver, decode_xml};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::fs_safe::{create_new_nofollow, open_read_nofollow};

const MAX_INFO_PLIST_BYTES: u64 = 4 * 1024 * 1024;
static BUNDLE_STAGE_SEQ: AtomicU64 = AtomicU64::new(1);

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
    pub expected_build_fingerprint: Option<String>,
    pub workspace_root: PathBuf,
    pub disable_animations: bool,
    pub crash_report_grace_ms: u64,
    pub crash_report_dir: Option<PathBuf>,
    pub deja_home: Option<PathBuf>,
    pub headless: Option<bool>,
    pub screen_size: Option<serde_json::Value>,
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

/// Production [`DriverFactory`] wiring `IosDriver`, `AndroidDriver`, and `WebDriver`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealDriverFactory;

fn map_android_factory_err(err: AndroidDriverError) -> DriverFactoryError {
    match err {
        AndroidDriverError::Cancelled => DriverFactoryError::Cancelled,
        AndroidDriverError::InvalidAdbPath { .. }
        | AndroidDriverError::InvalidSerial(_)
        | AndroidDriverError::InvalidPackageId(_)
        | AndroidDriverError::InvalidPath(_)
        | AndroidDriverError::InvalidArgument(_) => {
            DriverFactoryError::UnsupportedOrConfig(err.to_string())
        }
        other => DriverFactoryError::Runtime(other.to_string()),
    }
}

/// Validates that an Android `buildPath` is an existing regular, non-symlink `.apk` archive.
pub fn verify_android_apk_build(build_path: &Path) -> Result<String, String> {
    let lmeta = fs::symlink_metadata(build_path).map_err(|err| {
        format!(
            "Android buildPath cannot be inspected ({}): {err}",
            build_path.display()
        )
    })?;
    if lmeta.file_type().is_symlink() {
        return Err(format!(
            "Android buildPath must not be a symbolic link: {}",
            build_path.display()
        ));
    }
    if !lmeta.file_type().is_file()
        || build_path
            .extension()
            .and_then(|s| s.to_str())
            .is_none_or(|ext| !ext.eq_ignore_ascii_case("apk"))
    {
        return Err(format!(
            "Android buildPath must be an existing `.apk` regular file (got {})",
            build_path.display()
        ));
    }
    verify_and_hash_apk(build_path).map_err(|err| err.to_string())
}

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
                "android" => {
                    let adb = Adb::discover(ctx.env.clone()).map_err(map_android_factory_err)?;
                    adb.resolve_target_device(&ctx.device, cancel)
                        .await
                        .map_err(map_android_factory_err)?;
                    let rt = adb
                        .runtime_version(&ctx.device, cancel)
                        .await
                        .map_err(map_android_factory_err)?;
                    Ok(Some(rt))
                }
                "web" => {
                    if ctx.device != "chromium" {
                        return Err(DriverFactoryError::UnsupportedOrConfig(format!(
                            "web `device` must be \"chromium\" (got {:?})",
                            ctx.device
                        )));
                    }
                    let screen_size = match ctx.screen_size.as_ref() {
                        Some(val) => ScreenSize::parse(val).map_err(|err| {
                            DriverFactoryError::UnsupportedOrConfig(err.to_string())
                        })?,
                        None => ScreenSize::default(),
                    };
                    let suffix =
                        format_web_runtime_suffix(screen_size, ctx.headless.unwrap_or(true));
                    let (_binary, version) = discover_working_chromium(&ctx.env, cancel)
                        .await
                        .map_err(|err| match err {
                            WebDriverError::Cancelled => DriverFactoryError::Cancelled,
                            WebDriverError::InvalidChromiumPath { .. } => {
                                DriverFactoryError::UnsupportedOrConfig(err.to_string())
                            }
                            other => DriverFactoryError::Runtime(other.to_string()),
                        })?;
                    Ok(Some(format!("chromium-{version}{suffix}")))
                }
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
                "android" => {
                    verify_android_apk_build(&ctx.build_path)
                        .map_err(DriverFactoryError::UnsupportedOrConfig)?;
                    let mut cfg = AndroidDriverConfig::new(&ctx.device)
                        .with_disable_animations(ctx.disable_animations)
                        .with_env(ctx.env.clone());
                    cfg.deja_home = ctx.deja_home.clone().or_else(|| {
                        ctx.env
                            .get("DEJA_HOME")
                            .filter(|v| !v.is_empty())
                            .map(PathBuf::from)
                    });
                    cfg.crash_report_grace = Duration::from_millis(ctx.crash_report_grace_ms);
                    let timeout_cap = Duration::from_millis(ctx.timeout_ms.max(1_000));
                    cfg.startup_timeout = cfg.startup_timeout.min(timeout_cap);
                    cfg.request_timeout = cfg.request_timeout.min(timeout_cap);
                    cfg.app_launch_timeout = cfg.app_launch_timeout.min(timeout_cap);
                    let driver = AndroidDriver::new(cfg).map_err(map_android_factory_err)?;
                    Ok(Box::new(driver) as Box<dyn PlatformDriver>)
                }
                "web" => {
                    if ctx.device != "chromium" {
                        return Err(DriverFactoryError::UnsupportedOrConfig(format!(
                            "web `device` must be \"chromium\" (got {:?})",
                            ctx.device
                        )));
                    }
                    let screen_size = match ctx.screen_size.as_ref() {
                        Some(val) => ScreenSize::parse(val).map_err(|err| {
                            DriverFactoryError::UnsupportedOrConfig(err.to_string())
                        })?,
                        None => ScreenSize::default(),
                    };
                    let mut cfg = WebDriverConfig::new()
                        .with_headless(ctx.headless.unwrap_or(true))
                        .with_screen_size(screen_size)
                        .with_disable_animations(ctx.disable_animations)
                        .with_env(ctx.env.clone());
                    let timeout_cap = Duration::from_millis(ctx.timeout_ms.max(1_000));
                    cfg.startup_timeout = cfg.startup_timeout.min(timeout_cap);
                    cfg.request_timeout = cfg.request_timeout.min(timeout_cap);
                    cfg.navigation_timeout = cfg.navigation_timeout.min(timeout_cap);
                    Ok(Box::new(WebDriver::new(cfg)) as Box<dyn PlatformDriver>)
                }
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
                let build_path = ctx.build_path.clone();
                let app_id = ctx.app_id.clone();
                let expected_fp = ctx.expected_build_fingerprint.clone();
                let staged = tokio::select! {
                    _ = cancel.cancelled() => return Err(DriverFactoryError::Cancelled),
                    res = tokio::task::spawn_blocking(move || {
                        stage_and_verify_ios_app_bundle(
                            &build_path,
                            &app_id,
                            expected_fp.as_deref(),
                        )
                    }) => match res {
                        Ok(inner) => inner.map_err(DriverFactoryError::UnsupportedOrConfig)?,
                        Err(join_err) => {
                            return Err(DriverFactoryError::Runtime(format!(
                                "iOS `.app` bundle staging task failed: {join_err}"
                            )));
                        }
                    },
                };
                let simctl = Simctl::with_env(simctl_env_from_map(&ctx.env));
                tokio::select! {
                    _ = cancel.cancelled() => return Err(DriverFactoryError::Cancelled),
                    res = simctl.install_app(&ctx.device, staged.app_path()) => res,
                }
                .map_err(|err| {
                    DriverFactoryError::Runtime(format!(
                        "failed to install {} onto simulator {}: {err}",
                        ctx.build_path.display(),
                        ctx.device
                    ))
                })?;
                drop(staged);
            } else if ctx.platform == "android" {
                let build_path = ctx.build_path.clone();
                let workspace_root = ctx.workspace_root.clone();
                let expected_fp = ctx.expected_build_fingerprint.clone();
                tokio::select! {
                    _ = cancel.cancelled() => return Err(DriverFactoryError::Cancelled),
                    res = tokio::task::spawn_blocking(move || -> Result<(), String> {
                        let _ = verify_android_apk_build(&build_path)?;
                        if let Some(ref expected) = expected_fp {
                            let actual = fingerprint_file(&workspace_root, &build_path)
                                .map_err(|err| format!("failed to fingerprint Android APK: {err}"))?;
                            if actual != *expected {
                                return Err(format!(
                                    "Android `.apk` changed between fingerprinting ({expected}) and installation ({actual}); rebuild or retry with an immutable build artifact"
                                ));
                            }
                        }
                        Ok(())
                    }) => match res {
                        Ok(inner) => inner.map_err(DriverFactoryError::UnsupportedOrConfig)?,
                        Err(join_err) => {
                            return Err(DriverFactoryError::Runtime(format!(
                                "Android APK verification task failed: {join_err}"
                            )));
                        }
                    },
                };
                let adb = Adb::discover(ctx.env.clone()).map_err(map_android_factory_err)?;
                adb.resolve_target_device(&ctx.device, cancel)
                    .await
                    .map_err(map_android_factory_err)?;
                adb.install_apk(&ctx.device, &ctx.build_path, cancel)
                    .await
                    .map_err(map_android_factory_err)?;
            }
            Ok(())
        })
    }
}

/// RAII temporary immutable copy of an iOS `.app` bundle whose `Info.plist` `CFBundleIdentifier`
/// and streaming tree SHA-256 fingerprint have both been verified before installation.
pub struct StagedIosAppBundle {
    temp_dir: PathBuf,
    app_path: PathBuf,
    fingerprint: String,
}

impl StagedIosAppBundle {
    #[must_use]
    pub fn app_path(&self) -> &Path {
        &self.app_path
    }

    #[must_use]
    pub fn staging_dir(&self) -> &Path {
        &self.temp_dir
    }

    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

impl Drop for StagedIosAppBundle {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

/// Stage an immutable temporary copy of an iOS `.app` bundle (using APFS copy-on-write `/bin/cp -cR`
/// on macOS with a safe symlink-refusing recursive copy fallback), verify its `Info.plist`
/// `CFBundleIdentifier == expected_app_id`, and assert its streaming tree SHA-256 matches
/// `expected_fingerprint` before returning the staged copy for `simctl install`.
pub fn stage_and_verify_ios_app_bundle(
    build_path: &Path,
    expected_app_id: &str,
    expected_fingerprint: Option<&str>,
) -> Result<StagedIosAppBundle, String> {
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

    let app_name = build_path
        .file_name()
        .ok_or_else(|| format!("iOS buildPath has no bundle name: {}", build_path.display()))?;

    let seq = BUNDLE_STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp_dir = std::env::temp_dir().join(format!(
        "deja-ios-bundle-{}-{seq}-{nanos:x}",
        std::process::id()
    ));
    let mut dir_builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dir_builder.mode(0o700);
    }
    dir_builder
        .create(&temp_dir)
        .map_err(|err| format!("failed to create temporary iOS bundle staging directory: {err}"))?;

    let app_path = temp_dir.join(app_name);
    let mut staged = StagedIosAppBundle {
        temp_dir,
        app_path,
        fingerprint: String::new(),
    };

    stage_ios_bundle_tree(build_path, &staged.app_path)?;
    verify_ios_app_bundle(&staged.app_path, expected_app_id)?;

    let staged_fp = fingerprint_tree(&staged.temp_dir, &staged.app_path, "iOS")
        .map_err(|err| format!("failed to fingerprint staged iOS `.app` bundle: {err}"))?;
    if let Some(expected) = expected_fingerprint
        && staged_fp != expected
    {
        return Err(format!(
            "iOS `.app` bundle changed between fingerprinting ({expected}) and installation ({staged_fp}); rebuild or retry with an immutable build artifact"
        ));
    }
    staged.fingerprint = staged_fp;
    Ok(staged)
}

fn stage_ios_bundle_tree(src: &Path, dst: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("/bin/cp")
            .args(["-cR", "--"])
            .arg(src)
            .arg(dst)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Ok(st) = status
            && st.success()
        {
            return Ok(());
        }
        let _ = fs::remove_dir_all(dst);
    }
    copy_tree_nofollow(src, dst)
}

fn copy_tree_nofollow(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir(dst)
        .map_err(|err| format!("failed to create staged directory {}: {err}", dst.display()))?;
    let entries = fs::read_dir(src).map_err(|err| {
        format!(
            "failed to read iOS `.app` directory {}: {err}",
            src.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("failed to iterate iOS `.app` bundle: {err}"))?;
        let ftype = entry
            .file_type()
            .map_err(|err| format!("failed to inspect iOS `.app` entry type: {err}"))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ftype.is_symlink() {
            return Err(format!(
                "iOS `.app` bundle cannot contain symbolic links: {}",
                src_path.display()
            ));
        }
        if ftype.is_dir() {
            copy_tree_nofollow(&src_path, &dst_path)?;
        } else if ftype.is_file() {
            let mut reader = open_read_nofollow(&src_path).map_err(|err| {
                format!(
                    "failed to open iOS `.app` file {}: {err}",
                    src_path.display()
                )
            })?;
            let meta = reader.metadata().map_err(|err| {
                format!(
                    "failed to stat iOS `.app` file {}: {err}",
                    src_path.display()
                )
            })?;
            let mut writer = create_new_nofollow(&dst_path).map_err(|err| {
                format!(
                    "failed to create staged iOS `.app` file {}: {err}",
                    dst_path.display()
                )
            })?;
            std::io::copy(&mut reader, &mut writer).map_err(|err| {
                format!(
                    "failed to copy iOS `.app` file {}: {err}",
                    src_path.display()
                )
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode() & 0o777;
                let _ = writer.set_permissions(fs::Permissions::from_mode(mode));
            }
            #[cfg(not(unix))]
            {
                let _ = meta;
            }
        } else {
            return Err(format!(
                "iOS `.app` bundle cannot contain special files: {}",
                src_path.display()
            ));
        }
    }
    Ok(())
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
            .args(["-convert", "xml1", "-o", "-", "-"])
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
        let xml = std::str::from_utf8(&output.stdout)
            .map_err(|err| format!("plutil XML1 output is not valid UTF-8: {err}"))?;
        extract_cf_bundle_id_from_xml_plist(xml)
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

            // 3. Android `adb`, device pinning, and agent APK probes (required only when platform == android).
            let android_required = target_platform == Some("android");
            if android_required || target_platform.is_none() {
                checks.extend(
                    probe_android(
                        &req.env,
                        req.device.as_deref(),
                        req.deja_home.as_deref(),
                        android_required,
                    )
                    .await,
                );
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

async fn probe_android(
    env: &BTreeMap<String, String>,
    device: Option<&str>,
    deja_home: Option<&Path>,
    required: bool,
) -> Vec<DoctorCheck> {
    let mut out = Vec::new();
    let cancel = CancellationToken::new();

    let adb_path = match discover_adb(env) {
        Ok(p) => p,
        Err(err) => {
            out.push(DoctorCheck {
                name: "android-adb".to_string(),
                status: if required {
                    DoctorCheckStatus::NotReady
                } else {
                    DoctorCheckStatus::Warn
                },
                required,
                detail: err.to_string(),
                remediation: Some(
                    "Install Android Platform-Tools and ensure `adb` is on PATH, set ANDROID_HOME, or set DEJA_ADB"
                        .to_string(),
                ),
            });
            return out;
        }
    };

    let adb = Adb::from_path(adb_path.clone(), env.clone());
    match adb.version(&cancel).await {
        Ok(ver) => {
            out.push(DoctorCheck {
                name: "android-adb".to_string(),
                status: DoctorCheckStatus::Ready,
                required,
                detail: format!("{ver} ({})", adb_path.display()),
                remediation: None,
            });
        }
        Err(err) => {
            out.push(DoctorCheck {
                name: "android-adb".to_string(),
                status: if required {
                    DoctorCheckStatus::NotReady
                } else {
                    DoctorCheckStatus::Warn
                },
                required,
                detail: format!("`adb version` failed: {err}"),
                remediation: Some(
                    "Verify the `adb` binary at ANDROID_HOME/platform-tools/adb or DEJA_ADB is executable"
                        .to_string(),
                ),
            });
            return out;
        }
    }

    if required || device.is_some() {
        match adb.list_devices(&cancel).await {
            Ok(devices) => {
                let ready_devices: Vec<&str> = devices
                    .iter()
                    .filter(|d| d.state == "device")
                    .map(|d| d.serial.as_str())
                    .collect();

                if let Some(target_serial) = device {
                    match devices.iter().find(|d| d.serial == target_serial) {
                        Some(dev) if dev.state == "device" => {
                            let rt = adb
                                .runtime_version(target_serial, &cancel)
                                .await
                                .unwrap_or_else(|_| "android-unknown".to_string());
                            out.push(DoctorCheck {
                                name: "android-devices".to_string(),
                                status: DoctorCheckStatus::Ready,
                                required,
                                detail: format!(
                                    "Device {target_serial} ready ({rt}); {} device(s) online",
                                    ready_devices.len()
                                ),
                                remediation: None,
                            });
                        }
                        Some(dev) => {
                            let remediation = match dev.state.as_str() {
                                "unauthorized" => {
                                    "Unlock the Android device and accept the USB debugging RSA prompt"
                                }
                                "offline" => {
                                    "Reconnect the device or restart the emulator (`adb reconnect`)"
                                }
                                _ => {
                                    "Ensure `adb devices` reports this serial in the `device` state"
                                }
                            };
                            out.push(DoctorCheck {
                                name: "android-devices".to_string(),
                                status: DoctorCheckStatus::NotReady,
                                required: true,
                                detail: format!(
                                    "Target Android device {target_serial} is not ready (state={})",
                                    dev.state
                                ),
                                remediation: Some(remediation.to_string()),
                            });
                        }
                        None => {
                            out.push(DoctorCheck {
                                name: "android-devices".to_string(),
                                status: DoctorCheckStatus::NotReady,
                                required: true,
                                detail: format!(
                                    "Target Android device {target_serial} not found in `adb devices` (online: {})",
                                    if ready_devices.is_empty() {
                                        "<none>".to_string()
                                    } else {
                                        ready_devices.join(", ")
                                    }
                                ),
                                remediation: Some(
                                    "Connect the Android device or start an emulator, then verify its serial via `adb devices -l` or pass `--device <serial>`"
                                        .to_string(),
                                ),
                            });
                        }
                    }
                } else if ready_devices.is_empty() {
                    out.push(DoctorCheck {
                        name: "android-devices".to_string(),
                        status: if required {
                            DoctorCheckStatus::NotReady
                        } else {
                            DoctorCheckStatus::Warn
                        },
                        required,
                        detail: "No ready Android devices or emulators connected".to_string(),
                        remediation: Some(
                            "Start an Android emulator or connect a USB device with USB debugging enabled (`adb devices`)"
                                .to_string(),
                        ),
                    });
                } else {
                    out.push(DoctorCheck {
                        name: "android-devices".to_string(),
                        status: DoctorCheckStatus::Ready,
                        required,
                        detail: format!(
                            "{} ready Android device(s): {}",
                            ready_devices.len(),
                            ready_devices.join(", ")
                        ),
                        remediation: None,
                    });
                }
            }
            Err(err) => {
                out.push(DoctorCheck {
                    name: "android-devices".to_string(),
                    status: if required {
                        DoctorCheckStatus::NotReady
                    } else {
                        DoctorCheckStatus::Warn
                    },
                    required,
                    detail: format!("`adb devices -l` probe failed: {err}"),
                    remediation: Some(
                        "Check that the ADB daemon is running (`adb start-server`)".to_string(),
                    ),
                });
            }
        }
    }

    // Read-only Android agent APK readiness check (never builds or writes to disk).
    let readiness = inspect_agent_apk_readiness(deja_home, env);
    match readiness {
        AgentApkReadiness::PrebuiltReady { apk_path, sha256 } => {
            out.push(DoctorCheck {
                name: "android-agent".to_string(),
                status: DoctorCheckStatus::Ready,
                required,
                detail: format!(
                    "Prebuilt Déjà Android agent APK ready ({}, sha256={}...)",
                    apk_path.display(),
                    &sha256[..12]
                ),
                remediation: None,
            });
        }
        AgentApkReadiness::PrebuiltInvalid { detail } => {
            out.push(DoctorCheck {
                name: "android-agent".to_string(),
                status: if required {
                    DoctorCheckStatus::NotReady
                } else {
                    DoctorCheckStatus::Warn
                },
                required,
                detail,
                remediation: Some(
                    "Fix DEJA_ANDROID_AGENT_APK / DEJA_ANDROID_AGENT_APK_SHA256 or unset them to use on-demand SDK compilation"
                        .to_string(),
                ),
            });
        }
        AgentApkReadiness::CacheHit { source_sha256, .. } => {
            out.push(DoctorCheck {
                name: "android-agent".to_string(),
                status: DoctorCheckStatus::Ready,
                required: false,
                detail: format!(
                    "Déjà Android agent APK cache hit ({}...)",
                    &source_sha256[..12]
                ),
                remediation: None,
            });
        }
        AgentApkReadiness::CacheColdSdkReady {
            source_sha256,
            build_tools_version,
        } => {
            out.push(DoctorCheck {
                name: "android-agent".to_string(),
                status: DoctorCheckStatus::Warn,
                required: false,
                detail: format!(
                    "Déjà Android agent APK cache cold ({}...), Android build-tools {build_tools_version} ready",
                    &source_sha256[..12]
                ),
                remediation: Some(
                    "No action required: Déjà automatically builds and caches deja-android-agent.apk on the first Android run"
                        .to_string(),
                ),
            });
        }
        AgentApkReadiness::Unavailable { detail } => {
            out.push(DoctorCheck {
                name: "android-agent".to_string(),
                status: if required {
                    DoctorCheckStatus::NotReady
                } else {
                    DoctorCheckStatus::Warn
                },
                required,
                detail,
                remediation: Some(
                    "Install Android SDK Build-Tools + Platform (API 24+) and a JDK, or set DEJA_ANDROID_AGENT_APK=/path/to/deja-android-agent.apk"
                        .to_string(),
                ),
            });
        }
    }

    out
}

async fn probe_chromium(env: &BTreeMap<String, String>, required: bool) -> DoctorCheck {
    let cancel = CancellationToken::new();
    match discover_working_chromium(env, &cancel).await {
        Ok((binary, version)) => DoctorCheck {
            name: "web-chromium".to_string(),
            status: DoctorCheckStatus::Ready,
            required,
            detail: format!("Chromium {version} ({})", binary.display()),
            remediation: None,
        },
        Err(err) => DoctorCheck {
            name: "web-chromium".to_string(),
            status: if required {
                DoctorCheckStatus::NotReady
            } else {
                DoctorCheckStatus::Warn
            },
            required,
            detail: err.to_string(),
            remediation: Some(
                "Install Chromium or Google Chrome and ensure it is available on PATH, in /Applications, or via DEJA_CHROME"
                    .to_string(),
            ),
        },
    }
}

#[cfg(all(test, unix))]
mod android_factory_tests {
    use super::{
        DoctorCheckStatus, DoctorProbeRequest, DoctorProbes, DriverContext, DriverFactory,
        DriverFactoryError, RealDoctorProbes, RealDriverFactory,
    };
    use deja_core::fingerprint_file;
    use deja_flow::DriverCapability;
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio_util::sync::CancellationToken;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "deja-cli-android-factory-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_mock_adb(dir: &std::path::Path, log: &std::path::Path) -> PathBuf {
        let adb = dir.join("mock-adb");
        let script = format!(
            r#"#!/bin/sh
echo "$*" >> "{log}"
case "$*" in
  "version")
    echo "Android Debug Bridge version 1.0.41"
    ;;
  "devices -l")
    echo "List of devices attached"
    echo "emulator-5554          device product:emu model:Pixel transport_id:1"
    echo "OFFLINE9               offline transport_id:2"
    ;;
  "-s emulator-5554 shell getprop ro.build.version.sdk")
    echo "35"
    ;;
  "-s emulator-5554 install -r -g "*)
    echo "Performing Streamed Install"
    echo "Success"
    ;;
  *)
    exit 0
    ;;
esac
"#,
            log = log.display()
        );
        fs::write(&adb, script).unwrap();
        fs::set_permissions(&adb, fs::Permissions::from_mode(0o755)).unwrap();
        adb
    }

    #[tokio::test]
    async fn real_driver_factory_wires_android_runtime_create_and_install_with_fingerprint_check() {
        let dir = temp_dir("wire");
        let log = dir.join("adb.log");
        let adb = write_mock_adb(&dir, &log);
        let apk = dir.join("app-debug.apk");
        fs::write(&apk, b"PK\x03\x04test-app-apk-bytes").unwrap();
        let fp = fingerprint_file(&dir, &apk).unwrap();

        let mut env = BTreeMap::new();
        env.insert("DEJA_ADB".to_string(), adb.to_string_lossy().into_owned());

        let ctx = DriverContext {
            platform: "android".to_string(),
            device: "emulator-5554".to_string(),
            app_id: "com.example.app".to_string(),
            build_path: apk.clone(),
            expected_build_fingerprint: Some(fp.clone()),
            workspace_root: dir.clone(),
            disable_animations: true,
            crash_report_grace_ms: 1_500,
            crash_report_dir: None,
            deja_home: Some(dir.join("home")),
            headless: None,
            screen_size: None,
            timeout_ms: 30_000,
            env: env.clone(),
        };
        let cancel = CancellationToken::new();
        let factory = RealDriverFactory;

        // 1. resolve_runtime -> "android-35"
        let rt = factory
            .resolve_runtime(&ctx, &cancel)
            .await
            .expect("resolve_runtime should succeed");
        assert_eq!(rt.as_deref(), Some("android-35"));

        // 2. create -> AndroidDriver
        let drv = factory
            .create(&ctx, &cancel)
            .await
            .expect("create should succeed");
        assert_eq!(drv.platform(), "android");
        assert!(drv.capabilities().supports(DriverCapability::SystemBack));
        assert!(drv.capabilities().supports(DriverCapability::HoldScroll));
        assert!(!drv.capabilities().supports(DriverCapability::ClearKeychain));

        // 3. install_app succeeds with matching fingerprint and invokes `adb install -r -g`.
        factory
            .install_app(&ctx, &cancel)
            .await
            .expect("install_app should succeed");
        let adb_log = fs::read_to_string(&log).unwrap();
        assert!(adb_log.contains("-s emulator-5554 install -r -g"));

        // 4. install_app rejects fingerprint drift (TOCTOU protection) with exit-2 config error.
        let mut drifted = ctx.clone();
        drifted.expected_build_fingerprint = Some("0".repeat(64));
        let drift_err = factory
            .install_app(&drifted, &cancel)
            .await
            .expect_err("drifted APK fingerprint must fail closed");
        assert!(matches!(
            drift_err,
            DriverFactoryError::UnsupportedOrConfig(_)
        ));

        // 5. Symlink APK buildPath fails closed in both create and install_app.
        let sym_apk = dir.join("symlink.apk");
        symlink(&apk, &sym_apk).unwrap();
        let mut sym_ctx = ctx.clone();
        sym_ctx.build_path = sym_apk;
        assert!(matches!(
            factory.create(&sym_ctx, &cancel).await,
            Err(DriverFactoryError::UnsupportedOrConfig(_))
        ));
        assert!(matches!(
            factory.install_app(&sym_ctx, &cancel).await,
            Err(DriverFactoryError::UnsupportedOrConfig(_))
        ));

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn real_doctor_probes_android_adb_devices_and_agent_readiness() {
        let dir = temp_dir("doctor");
        let log = dir.join("adb.log");
        let adb = write_mock_adb(&dir, &log);
        let agent_apk = dir.join("deja-android-agent.apk");
        fs::write(&agent_apk, b"PK\x03\x04agent-bytes").unwrap();

        let mut env = BTreeMap::new();
        env.insert("DEJA_ADB".to_string(), adb.to_string_lossy().into_owned());
        env.insert(
            "DEJA_ANDROID_AGENT_APK".to_string(),
            agent_apk.to_string_lossy().into_owned(),
        );

        let probes = RealDoctorProbes;
        let checks = probes
            .probe(&DoctorProbeRequest {
                platform: Some("android".to_string()),
                device: Some("emulator-5554".to_string()),
                env: env.clone(),
                deja_home: Some(dir.join("home")),
            })
            .await;

        let by_name = |name: &str| {
            checks
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("missing doctor check `{name}` in {checks:#?}"))
        };
        assert_eq!(by_name("android-adb").status, DoctorCheckStatus::Ready);
        assert_eq!(by_name("android-devices").status, DoctorCheckStatus::Ready);
        assert!(by_name("android-devices").detail.contains("android-35"));
        assert_eq!(by_name("android-agent").status, DoctorCheckStatus::Ready);

        // Offline serial fails closed with NotReady + remediation.
        let offline_checks = probes
            .probe(&DoctorProbeRequest {
                platform: Some("android".to_string()),
                device: Some("OFFLINE9".to_string()),
                env,
                deja_home: Some(dir.join("home")),
            })
            .await;
        let dev_check = offline_checks
            .iter()
            .find(|c| c.name == "android-devices")
            .unwrap();
        assert_eq!(dev_check.status, DoctorCheckStatus::NotReady);
        assert!(dev_check.remediation.is_some());

        let _ = fs::remove_dir_all(&dir);
    }
}
