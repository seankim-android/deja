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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deja_core::{fingerprint_file_cancellable, fingerprint_tree_cancellable};
use deja_driver_android::{
    Adb, AgentApkReadiness, AndroidDriver, AndroidDriverConfig, AndroidDriverError, discover_adb,
    extract_apk_package_id_from_path, inspect_agent_apk_readiness, verify_and_hash_apk,
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

enum StagedBuildArtifact {
    Ios(StagedIosAppBundle),
    Android(StagedAndroidApk),
}

impl StagedBuildArtifact {
    fn app_path(&self) -> &Path {
        match self {
            Self::Ios(staged) => staged.app_path(),
            Self::Android(staged) => staged.apk_path(),
        }
    }

    fn staging_dir(&self) -> &Path {
        match self {
            Self::Ios(staged) => staged.staging_dir(),
            Self::Android(staged) => staged.staging_dir(),
        }
    }

    fn fingerprint(&self) -> &str {
        match self {
            Self::Ios(staged) => staged.fingerprint(),
            Self::Android(staged) => staged.fingerprint(),
        }
    }

    fn platform(&self) -> &'static str {
        match self {
            Self::Ios(_) => "ios",
            Self::Android(_) => "android",
        }
    }
}

/// Cloneable, reference-counted handle to a verified, staged iOS `.app` bundle or Android `.apk`
/// artifact shared across parallel batch workers so the build artifact is copied and SHA-256
/// verified only once per batch.
#[derive(Clone)]
pub struct SharedStagedBuild {
    inner: Arc<StagedBuildArtifact>,
}

impl std::fmt::Debug for SharedStagedBuild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedStagedBuild")
            .field("platform", &self.inner.platform())
            .field("app_path", &self.inner.app_path())
            .field("fingerprint", &self.inner.fingerprint())
            .finish()
    }
}

impl SharedStagedBuild {
    /// Wrap a verified [`StagedIosAppBundle`] for shared read-only reuse across workers.
    #[must_use]
    pub fn new(staged: StagedIosAppBundle) -> Self {
        Self {
            inner: Arc::new(StagedBuildArtifact::Ios(staged)),
        }
    }

    /// Wrap a verified [`StagedAndroidApk`] for shared read-only reuse across workers.
    #[must_use]
    pub fn new_android(staged: StagedAndroidApk) -> Self {
        Self {
            inner: Arc::new(StagedBuildArtifact::Android(staged)),
        }
    }

    #[must_use]
    pub fn app_path(&self) -> &Path {
        self.inner.app_path()
    }

    #[must_use]
    pub fn staging_dir(&self) -> &Path {
        self.inner.staging_dir()
    }

    #[must_use]
    pub fn fingerprint(&self) -> &str {
        self.inner.fingerprint()
    }

    #[must_use]
    pub fn is_platform(&self, platform: &str) -> bool {
        self.inner.platform() == platform
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
    /// Optional batch-shared staged build artifact verified once and reused across shard workers.
    pub staged_build: Option<SharedStagedBuild>,
}

/// Abstract factory for constructing concrete [`PlatformDriver`] instances.
pub trait DriverFactory: Send + Sync {
    /// Resolve the normalized OS runtime identifier for the target device (for example `"ios-27.2"`).
    fn resolve_runtime<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Option<String>, DriverFactoryError>>;

    /// Optionally stage and verify a shared build artifact once for a multi-goal batch so shard
    /// workers reuse it without re-copying or re-hashing on every device.
    fn prepare_batch_build<'a>(
        &'a self,
        _ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Option<SharedStagedBuild>, DriverFactoryError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverFactoryError::Cancelled);
            }
            Ok(None)
        })
    }

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

/// Validates that an Android `buildPath` is an existing regular, non-symlink `.apk` archive
/// whose binary `AndroidManifest.xml` `<manifest package="...">` matches `expected_app_id`.
pub fn verify_android_apk_build(
    build_path: &Path,
    expected_app_id: &str,
) -> Result<String, String> {
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
    let sha = verify_and_hash_apk(build_path).map_err(|err| err.to_string())?;
    let apk_pkg = extract_apk_package_id_from_path(build_path).map_err(|err| {
        format!(
            "Android APK manifest package check failed ({}): {err}",
            build_path.display()
        )
    })?;
    if apk_pkg != expected_app_id {
        return Err(format!(
            "Android `.apk` manifest package ({apk_pkg:?}) does not match configured appId ({expected_app_id:?})"
        ));
    }
    Ok(sha)
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

    fn prepare_batch_build<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Option<SharedStagedBuild>, DriverFactoryError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverFactoryError::Cancelled);
            }
            if ctx.platform == "ios" {
                let staged = run_cancellable_ios_bundle_staging(
                    ctx.build_path.clone(),
                    ctx.app_id.clone(),
                    ctx.expected_build_fingerprint.clone(),
                    cancel,
                )
                .await?;
                return Ok(Some(SharedStagedBuild::new(staged)));
            }
            if ctx.platform == "android" {
                let staged = run_cancellable_android_apk_staging(
                    ctx.build_path.clone(),
                    ctx.app_id.clone(),
                    ctx.expected_build_fingerprint.clone(),
                    cancel,
                )
                .await?;
                return Ok(Some(SharedStagedBuild::new_android(staged)));
            }
            Ok(None)
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
                    if let Some(ref staged) = ctx.staged_build
                        && staged.is_platform("ios")
                    {
                        if let Some(ref expected) = ctx.expected_build_fingerprint
                            && staged.fingerprint() != expected
                        {
                            return Err(DriverFactoryError::UnsupportedOrConfig(format!(
                                "staged iOS `.app` bundle fingerprint ({}) does not match expected ({expected})",
                                staged.fingerprint()
                            )));
                        }
                    } else {
                        verify_ios_app_bundle(&ctx.build_path, &ctx.app_id)
                            .map_err(DriverFactoryError::UnsupportedOrConfig)?;
                    }
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
                    if let Some(ref staged) = ctx.staged_build
                        && staged.is_platform("android")
                    {
                        if let Some(ref expected) = ctx.expected_build_fingerprint
                            && staged.fingerprint() != expected
                        {
                            return Err(DriverFactoryError::UnsupportedOrConfig(format!(
                                "staged Android `.apk` fingerprint ({}) does not match expected ({expected})",
                                staged.fingerprint()
                            )));
                        }
                    } else {
                        verify_android_apk_build(&ctx.build_path, &ctx.app_id)
                            .map_err(DriverFactoryError::UnsupportedOrConfig)?;
                    }
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
                let simctl = Simctl::with_env(simctl_env_from_map(&ctx.env));
                if let Some(ref staged) = ctx.staged_build
                    && staged.is_platform("ios")
                {
                    if let Some(ref expected) = ctx.expected_build_fingerprint
                        && staged.fingerprint() != expected
                    {
                        return Err(DriverFactoryError::UnsupportedOrConfig(format!(
                            "staged iOS `.app` bundle fingerprint ({}) does not match expected ({expected})",
                            staged.fingerprint()
                        )));
                    }
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
                    return Ok(());
                }

                let staged = run_cancellable_ios_bundle_staging(
                    ctx.build_path.clone(),
                    ctx.app_id.clone(),
                    ctx.expected_build_fingerprint.clone(),
                    cancel,
                )
                .await?;
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
                if let Some(ref staged) = ctx.staged_build
                    && staged.is_platform("android")
                {
                    if let Some(ref expected) = ctx.expected_build_fingerprint
                        && staged.fingerprint() != expected
                    {
                        return Err(DriverFactoryError::UnsupportedOrConfig(format!(
                            "staged Android `.apk` fingerprint ({}) does not match expected ({expected})",
                            staged.fingerprint()
                        )));
                    }
                    let adb = Adb::discover(ctx.env.clone()).map_err(map_android_factory_err)?;
                    adb.resolve_target_device(&ctx.device, cancel)
                        .await
                        .map_err(map_android_factory_err)?;
                    adb.install_apk(&ctx.device, staged.app_path(), cancel)
                        .await
                        .map_err(map_android_factory_err)?;
                    return Ok(());
                }

                let staged = run_cancellable_android_apk_staging(
                    ctx.build_path.clone(),
                    ctx.app_id.clone(),
                    ctx.expected_build_fingerprint.clone(),
                    cancel,
                )
                .await?;
                let adb = Adb::discover(ctx.env.clone()).map_err(map_android_factory_err)?;
                adb.resolve_target_device(&ctx.device, cancel)
                    .await
                    .map_err(map_android_factory_err)?;
                adb.install_apk(&ctx.device, staged.apk_path(), cancel)
                    .await
                    .map_err(map_android_factory_err)?;
                drop(staged);
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

/// RAII temporary immutable copy of an Android `.apk` archive whose binary `AndroidManifest.xml`
/// package ID and streaming SHA-256 fingerprint have both been verified before `adb install`.
pub struct StagedAndroidApk {
    temp_dir: PathBuf,
    apk_path: PathBuf,
    fingerprint: String,
}

impl StagedAndroidApk {
    #[must_use]
    pub fn apk_path(&self) -> &Path {
        &self.apk_path
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

impl Drop for StagedAndroidApk {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

async fn run_cancellable_android_apk_staging(
    build_path: PathBuf,
    app_id: String,
    expected_fp: Option<String>,
    cancel: &CancellationToken,
) -> Result<StagedAndroidApk, DriverFactoryError> {
    if cancel.is_cancelled() {
        return Err(DriverFactoryError::Cancelled);
    }
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let worker_flag = Arc::clone(&cancel_flag);
    let mut handle = tokio::task::spawn_blocking(move || {
        stage_and_verify_android_apk_cancellable(
            &build_path,
            &app_id,
            expected_fp.as_deref(),
            &worker_flag,
        )
    });
    tokio::select! {
        res = &mut handle => match res {
            Ok(Ok(staged)) => Ok(staged),
            Ok(Err(StageBundleError::Cancelled)) => Err(DriverFactoryError::Cancelled),
            Ok(Err(StageBundleError::Failed(msg))) => Err(DriverFactoryError::UnsupportedOrConfig(msg)),
            Err(join_err) => Err(DriverFactoryError::Runtime(format!(
                "Android `.apk` staging task failed: {join_err}"
            ))),
        },
        _ = cancel.cancelled() => {
            cancel_flag.store(true, Ordering::SeqCst);
            let _ = handle.await;
            Err(DriverFactoryError::Cancelled)
        }
    }
}

/// Stage an immutable private temporary copy of an Android `.apk` (`O_NOFOLLOW`), verify its
/// binary `AndroidManifest.xml` `<manifest package="..."> == expected_app_id`, and assert its
/// streaming SHA-256 matches `expected_fingerprint` on that exact staged copy before `adb install`.
pub fn stage_and_verify_android_apk_cancellable(
    build_path: &Path,
    expected_app_id: &str,
    expected_fingerprint: Option<&str>,
    cancel_flag: &AtomicBool,
) -> Result<StagedAndroidApk, StageBundleError> {
    use std::io::Write;
    if cancel_flag.load(Ordering::Relaxed) {
        return Err(StageBundleError::Cancelled);
    }
    let lmeta = fs::symlink_metadata(build_path).map_err(|err| {
        StageBundleError::Failed(format!(
            "Android buildPath cannot be inspected ({}): {err}",
            build_path.display()
        ))
    })?;
    if lmeta.file_type().is_symlink() {
        return Err(StageBundleError::Failed(format!(
            "Android buildPath must not be a symbolic link: {}",
            build_path.display()
        )));
    }
    if !lmeta.file_type().is_file()
        || build_path
            .extension()
            .and_then(|s| s.to_str())
            .is_none_or(|ext| !ext.eq_ignore_ascii_case("apk"))
    {
        return Err(StageBundleError::Failed(format!(
            "Android buildPath must be an existing `.apk` regular file (got {})",
            build_path.display()
        )));
    }
    let apk_name = build_path.file_name().ok_or_else(|| {
        StageBundleError::Failed(format!(
            "Android buildPath has no filename: {}",
            build_path.display()
        ))
    })?;

    let seq = BUNDLE_STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp_dir = std::env::temp_dir().join(format!(
        "deja-android-apk-{}-{seq}-{nanos:x}",
        std::process::id()
    ));
    let mut dir_builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dir_builder.mode(0o700);
    }
    dir_builder.create(&temp_dir).map_err(|err| {
        StageBundleError::Failed(format!(
            "failed to create temporary Android APK staging directory: {err}"
        ))
    })?;

    let apk_path = temp_dir.join(apk_name);
    let mut staged = StagedAndroidApk {
        temp_dir,
        apk_path,
        fingerprint: String::new(),
    };

    let mut reader = open_read_nofollow(build_path).map_err(|err| {
        StageBundleError::Failed(format!(
            "failed to open Android APK {}: {err}",
            build_path.display()
        ))
    })?;
    let mut writer = create_new_nofollow(&staged.apk_path).map_err(|err| {
        StageBundleError::Failed(format!(
            "failed to create staged Android APK {}: {err}",
            staged.apk_path.display()
        ))
    })?;
    let mut buf = [0u8; 65_536];
    loop {
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(StageBundleError::Cancelled);
        }
        let n = reader.read(&mut buf).map_err(|err| {
            StageBundleError::Failed(format!(
                "failed to copy Android APK {}: {err}",
                build_path.display()
            ))
        })?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n]).map_err(|err| {
            StageBundleError::Failed(format!(
                "failed to write staged Android APK {}: {err}",
                staged.apk_path.display()
            ))
        })?;
    }
    drop(writer);
    drop(reader);

    if cancel_flag.load(Ordering::Relaxed) {
        return Err(StageBundleError::Cancelled);
    }

    // Verify package ID directly on the private staged copy (eliminating TOCTOU drift).
    let apk_pkg = extract_apk_package_id_from_path(&staged.apk_path).map_err(|err| {
        StageBundleError::Failed(format!(
            "staged Android APK manifest package check failed ({}): {err}",
            build_path.display()
        ))
    })?;
    if apk_pkg != expected_app_id {
        return Err(StageBundleError::Failed(format!(
            "Android `.apk` manifest package ({apk_pkg:?}) does not match configured appId ({expected_app_id:?})"
        )));
    }
    if cancel_flag.load(Ordering::Relaxed) {
        return Err(StageBundleError::Cancelled);
    }

    // Compute streaming SHA-256 directly over the private staged copy and check against expected.
    let staged_fp = fingerprint_file_cancellable(&staged.temp_dir, &staged.apk_path, cancel_flag)
        .map_err(|err| match err {
        deja_core::CoreError::Cancelled => StageBundleError::Cancelled,
        other => StageBundleError::Failed(format!(
            "failed to fingerprint staged Android `.apk`: {other}"
        )),
    })?;
    if cancel_flag.load(Ordering::Relaxed) {
        return Err(StageBundleError::Cancelled);
    }
    if let Some(expected) = expected_fingerprint
        && staged_fp != expected
    {
        return Err(StageBundleError::Failed(format!(
            "Android `.apk` changed between fingerprinting ({expected}) and installation ({staged_fp}); rebuild or retry with an immutable build artifact"
        )));
    }
    staged.fingerprint = staged_fp;
    Ok(staged)
}

/// Error returned by [`stage_and_verify_ios_app_bundle_cancellable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageBundleError {
    Cancelled,
    Failed(String),
}

async fn run_cancellable_ios_bundle_staging(
    build_path: PathBuf,
    app_id: String,
    expected_fp: Option<String>,
    cancel: &CancellationToken,
) -> Result<StagedIosAppBundle, DriverFactoryError> {
    run_cancellable_ios_bundle_staging_with_hook(build_path, app_id, expected_fp, cancel, None)
        .await
}

type StageTestHook = Box<dyn FnOnce(&Path, &AtomicBool) + Send + 'static>;

async fn run_cancellable_ios_bundle_staging_with_hook(
    build_path: PathBuf,
    app_id: String,
    expected_fp: Option<String>,
    cancel: &CancellationToken,
    after_copy_hook: Option<StageTestHook>,
) -> Result<StagedIosAppBundle, DriverFactoryError> {
    if cancel.is_cancelled() {
        return Err(DriverFactoryError::Cancelled);
    }
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let worker_flag = Arc::clone(&cancel_flag);
    let mut handle = tokio::task::spawn_blocking(move || {
        stage_and_verify_ios_app_bundle_inner(
            &build_path,
            &app_id,
            expected_fp.as_deref(),
            &worker_flag,
            after_copy_hook,
        )
    });
    tokio::select! {
        res = &mut handle => match res {
            Ok(Ok(staged)) => Ok(staged),
            Ok(Err(StageBundleError::Cancelled)) => Err(DriverFactoryError::Cancelled),
            Ok(Err(StageBundleError::Failed(msg))) => Err(DriverFactoryError::UnsupportedOrConfig(msg)),
            Err(join_err) => Err(DriverFactoryError::Runtime(format!(
                "iOS `.app` bundle staging task failed: {join_err}"
            ))),
        },
        _ = cancel.cancelled() => {
            cancel_flag.store(true, Ordering::SeqCst);
            // Always await the blocking staging worker to completion so any child `/bin/cp` is
            // killed, partial temporary directories are deleted, and Tokio runtime shutdown never
            // stalls on a detached blocking task.
            let _ = handle.await;
            Err(DriverFactoryError::Cancelled)
        }
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
    let never_cancel = AtomicBool::new(false);
    stage_and_verify_ios_app_bundle_cancellable(
        build_path,
        expected_app_id,
        expected_fingerprint,
        &never_cancel,
    )
    .map_err(|err| match err {
        StageBundleError::Cancelled => "operation cancelled".to_string(),
        StageBundleError::Failed(msg) => msg,
    })
}

/// Cancellable variant of [`stage_and_verify_ios_app_bundle`] that checks `cancel_flag` before
/// and between filesystem copy/hash steps, terminates any spawned `/bin/cp` child on cancel, and
/// cleans up the temporary staging directory via [`StagedIosAppBundle::drop`] before returning.
pub fn stage_and_verify_ios_app_bundle_cancellable(
    build_path: &Path,
    expected_app_id: &str,
    expected_fingerprint: Option<&str>,
    cancel_flag: &AtomicBool,
) -> Result<StagedIosAppBundle, StageBundleError> {
    stage_and_verify_ios_app_bundle_inner(
        build_path,
        expected_app_id,
        expected_fingerprint,
        cancel_flag,
        None,
    )
}

fn stage_and_verify_ios_app_bundle_inner(
    build_path: &Path,
    expected_app_id: &str,
    expected_fingerprint: Option<&str>,
    cancel_flag: &AtomicBool,
    after_copy_hook: Option<StageTestHook>,
) -> Result<StagedIosAppBundle, StageBundleError> {
    if cancel_flag.load(Ordering::Relaxed) {
        return Err(StageBundleError::Cancelled);
    }
    let lmeta = fs::symlink_metadata(build_path).map_err(|err| {
        StageBundleError::Failed(format!(
            "iOS buildPath cannot be inspected ({}): {err}",
            build_path.display()
        ))
    })?;
    if lmeta.file_type().is_symlink() {
        return Err(StageBundleError::Failed(format!(
            "iOS buildPath must not be a symbolic link: {}",
            build_path.display()
        )));
    }
    if !lmeta.file_type().is_dir() || build_path.extension().and_then(|s| s.to_str()) != Some("app")
    {
        return Err(StageBundleError::Failed(format!(
            "iOS buildPath must be an existing `.app` bundle directory (got {})",
            build_path.display()
        )));
    }

    let app_name = build_path.file_name().ok_or_else(|| {
        StageBundleError::Failed(format!(
            "iOS buildPath has no bundle name: {}",
            build_path.display()
        ))
    })?;

    if cancel_flag.load(Ordering::Relaxed) {
        return Err(StageBundleError::Cancelled);
    }

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
    dir_builder.create(&temp_dir).map_err(|err| {
        StageBundleError::Failed(format!(
            "failed to create temporary iOS bundle staging directory: {err}"
        ))
    })?;

    let app_path = temp_dir.join(app_name);
    let mut staged = StagedIosAppBundle {
        temp_dir,
        app_path,
        fingerprint: String::new(),
    };

    stage_ios_bundle_tree(build_path, &staged.app_path, cancel_flag)?;
    if let Some(hook) = after_copy_hook {
        hook(&staged.temp_dir, cancel_flag);
    }
    if cancel_flag.load(Ordering::Relaxed) {
        return Err(StageBundleError::Cancelled);
    }
    verify_ios_app_bundle(&staged.app_path, expected_app_id).map_err(StageBundleError::Failed)?;
    if cancel_flag.load(Ordering::Relaxed) {
        return Err(StageBundleError::Cancelled);
    }

    let staged_fp =
        fingerprint_tree_cancellable(&staged.temp_dir, &staged.app_path, "iOS", cancel_flag)
            .map_err(|err| match err {
                deja_core::CoreError::Cancelled => StageBundleError::Cancelled,
                other => StageBundleError::Failed(format!(
                    "failed to fingerprint staged iOS `.app` bundle: {other}"
                )),
            })?;
    if cancel_flag.load(Ordering::Relaxed) {
        return Err(StageBundleError::Cancelled);
    }
    if let Some(expected) = expected_fingerprint
        && staged_fp != expected
    {
        return Err(StageBundleError::Failed(format!(
            "iOS `.app` bundle changed between fingerprinting ({expected}) and installation ({staged_fp}); rebuild or retry with an immutable build artifact"
        )));
    }
    staged.fingerprint = staged_fp;
    Ok(staged)
}

fn stage_ios_bundle_tree(
    src: &Path,
    dst: &Path,
    cancel_flag: &AtomicBool,
) -> Result<(), StageBundleError> {
    if cancel_flag.load(Ordering::Relaxed) {
        return Err(StageBundleError::Cancelled);
    }
    #[cfg(target_os = "macos")]
    {
        let spawned = std::process::Command::new("/bin/cp")
            .args(["-cR", "--"])
            .arg(src)
            .arg(dst)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = spawned {
            loop {
                if cancel_flag.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = fs::remove_dir_all(dst);
                    return Err(StageBundleError::Cancelled);
                }
                match child.try_wait() {
                    Ok(Some(st)) if st.success() => return Ok(()),
                    Ok(Some(_)) => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    Err(_) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
            let _ = fs::remove_dir_all(dst);
        }
    }
    copy_tree_nofollow(src, dst, cancel_flag)
}

fn copy_tree_nofollow(
    src: &Path,
    dst: &Path,
    cancel_flag: &AtomicBool,
) -> Result<(), StageBundleError> {
    use std::io::Write;
    if cancel_flag.load(Ordering::Relaxed) {
        return Err(StageBundleError::Cancelled);
    }
    fs::create_dir(dst).map_err(|err| {
        StageBundleError::Failed(format!(
            "failed to create staged directory {}: {err}",
            dst.display()
        ))
    })?;
    let entries = fs::read_dir(src).map_err(|err| {
        StageBundleError::Failed(format!(
            "failed to read iOS `.app` directory {}: {err}",
            src.display()
        ))
    })?;
    let mut buf = [0u8; 65_536];
    for entry in entries {
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(StageBundleError::Cancelled);
        }
        let entry = entry.map_err(|err| {
            StageBundleError::Failed(format!("failed to iterate iOS `.app` bundle: {err}"))
        })?;
        let ftype = entry.file_type().map_err(|err| {
            StageBundleError::Failed(format!("failed to inspect iOS `.app` entry type: {err}"))
        })?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ftype.is_symlink() {
            return Err(StageBundleError::Failed(format!(
                "iOS `.app` bundle cannot contain symbolic links: {}",
                src_path.display()
            )));
        }
        if ftype.is_dir() {
            copy_tree_nofollow(&src_path, &dst_path, cancel_flag)?;
        } else if ftype.is_file() {
            let mut reader = open_read_nofollow(&src_path).map_err(|err| {
                StageBundleError::Failed(format!(
                    "failed to open iOS `.app` file {}: {err}",
                    src_path.display()
                ))
            })?;
            let meta = reader.metadata().map_err(|err| {
                StageBundleError::Failed(format!(
                    "failed to stat iOS `.app` file {}: {err}",
                    src_path.display()
                ))
            })?;
            let mut writer = create_new_nofollow(&dst_path).map_err(|err| {
                StageBundleError::Failed(format!(
                    "failed to create staged iOS `.app` file {}: {err}",
                    dst_path.display()
                ))
            })?;
            loop {
                if cancel_flag.load(Ordering::Relaxed) {
                    return Err(StageBundleError::Cancelled);
                }
                let n = reader.read(&mut buf).map_err(|err| {
                    StageBundleError::Failed(format!(
                        "failed to copy iOS `.app` file {}: {err}",
                        src_path.display()
                    ))
                })?;
                if n == 0 {
                    break;
                }
                writer.write_all(&buf[..n]).map_err(|err| {
                    StageBundleError::Failed(format!(
                        "failed to copy iOS `.app` file {}: {err}",
                        src_path.display()
                    ))
                })?;
            }
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
            return Err(StageBundleError::Failed(format!(
                "iOS `.app` bundle cannot contain special files: {}",
                src_path.display()
            )));
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
        AgentApkReadiness::EmbeddedPrebuiltReady { sha256 } => {
            out.push(DoctorCheck {
                name: "android-agent".to_string(),
                status: DoctorCheckStatus::Ready,
                required,
                detail: format!(
                    "Embedded prebuilt Déjà Android agent APK ready (sha256={}...; no JDK or Android SDK required)",
                    &sha256[..12]
                ),
                remediation: None,
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
    use deja_driver_android::embed::PREBUILT_AGENT_APK;
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
    async fn real_driver_factory_wires_android_runtime_stages_apk_checks_package_id_and_drift() {
        let dir = temp_dir("wire");
        let log = dir.join("adb.log");
        let adb = write_mock_adb(&dir, &log);
        let apk = dir.join("app-debug.apk");
        // Use the real compiled APK (package="dev.deja.agent") so AXML extraction runs end-to-end!
        fs::write(&apk, PREBUILT_AGENT_APK).unwrap();
        let fp = fingerprint_file(&dir, &apk).unwrap();

        let mut env = BTreeMap::new();
        env.insert("DEJA_ADB".to_string(), adb.to_string_lossy().into_owned());

        let ctx = DriverContext {
            platform: "android".to_string(),
            device: "emulator-5554".to_string(),
            app_id: "dev.deja.agent".to_string(),
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
            staged_build: None,
        };
        let cancel = CancellationToken::new();
        let factory = RealDriverFactory;

        // 1. resolve_runtime -> "android-35"
        let rt = factory
            .resolve_runtime(&ctx, &cancel)
            .await
            .expect("resolve_runtime should succeed");
        assert_eq!(rt.as_deref(), Some("android-35"));

        // 2. create -> AndroidDriver (verifies manifest package == app_id)
        let drv = factory
            .create(&ctx, &cancel)
            .await
            .expect("create should succeed with matching APK package");
        assert_eq!(drv.platform(), "android");
        assert!(drv.capabilities().supports(DriverCapability::SystemBack));
        assert!(drv.capabilities().supports(DriverCapability::HoldScroll));
        assert!(!drv.capabilities().supports(DriverCapability::ClearKeychain));

        // 3. Mismatched appId fails closed with exit-2 UnsupportedOrConfig in both create and install_app.
        let mut wrong_pkg = ctx.clone();
        wrong_pkg.app_id = "com.example.wrong".to_string();
        let pkg_err = match factory.create(&wrong_pkg, &cancel).await {
            Err(err) => err,
            Ok(_) => panic!("APK package mismatch must fail closed"),
        };
        assert!(matches!(
            pkg_err,
            DriverFactoryError::UnsupportedOrConfig(_)
        ));
        assert!(pkg_err.to_string().contains("dev.deja.agent"));
        assert!(matches!(
            factory.install_app(&wrong_pkg, &cancel).await,
            Err(DriverFactoryError::UnsupportedOrConfig(_))
        ));

        // 4. Non-APK / corrupt file fails closed with exit-2 UnsupportedOrConfig.
        let fake_apk = dir.join("fake.apk");
        fs::write(&fake_apk, b"PK\x03\x04not-a-real-apk").unwrap();
        let mut fake_ctx = ctx.clone();
        fake_ctx.build_path = fake_apk;
        fake_ctx.expected_build_fingerprint = None;
        assert!(matches!(
            factory.create(&fake_ctx, &cancel).await,
            Err(DriverFactoryError::UnsupportedOrConfig(_))
        ));

        // 5. install_app stages a private copy and passes the STAGED path (not raw workspace path) to `adb install`.
        factory
            .install_app(&ctx, &cancel)
            .await
            .expect("install_app should succeed");
        let adb_log = fs::read_to_string(&log).unwrap();
        let install_line = adb_log
            .lines()
            .find(|l| l.contains("-s emulator-5554 install -r -g "))
            .expect("adb install logged");
        assert!(
            !install_line.contains(&apk.display().to_string()),
            "adb install must receive the private staged APK copy, not raw buildPath: {install_line}"
        );
        assert!(
            install_line.contains("deja-android-apk-"),
            "expected staged APK temp path in adb log: {install_line}"
        );

        // 6. prepare_batch_build stages once, install_app reuses it, and Drop cleans up the temp dir.
        let staged = factory
            .prepare_batch_build(&ctx, &cancel)
            .await
            .expect("prepare_batch_build succeeds")
            .expect("Android returns SharedStagedBuild");
        let staging_dir = staged.staging_dir().to_path_buf();
        assert!(staging_dir.is_dir());
        assert_eq!(staged.fingerprint(), fp);
        let mut batch_ctx = ctx.clone();
        batch_ctx.staged_build = Some(staged.clone());
        factory
            .install_app(&batch_ctx, &cancel)
            .await
            .expect("batch install_app reuses staged APK");
        drop(batch_ctx);
        drop(staged);
        assert!(
            !staging_dir.exists(),
            "dropping SharedStagedBuild must remove temporary Android APK staging dir"
        );

        // 7. Mutating the `.apk` file on disk between fingerprinting and `install_app` fails closed (exit 2).
        let mutated_apk = dir.join("mutated.apk");
        fs::write(&mutated_apk, PREBUILT_AGENT_APK).unwrap();
        let orig_fp = fingerprint_file(&dir, &mutated_apk).unwrap();
        let mut mutated_bytes = PREBUILT_AGENT_APK.to_vec();
        mutated_bytes.extend_from_slice(b"drift");
        fs::write(&mutated_apk, &mutated_bytes).unwrap();
        let mut drift_ctx = ctx.clone();
        drift_ctx.build_path = mutated_apk;
        drift_ctx.expected_build_fingerprint = Some(orig_fp);
        let drift_err = factory
            .install_app(&drift_ctx, &cancel)
            .await
            .expect_err("mutated APK on disk must fail closed");
        assert!(matches!(
            drift_err,
            DriverFactoryError::UnsupportedOrConfig(_)
        ));

        // 8. Symlink APK buildPath fails closed in both create and install_app.
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
    async fn real_doctor_probes_android_ready_on_jdk_free_host_via_embedded_prebuilt_apk() {
        let dir = temp_dir("doctor");
        let log = dir.join("adb.log");
        let adb = write_mock_adb(&dir, &log);

        // Minimal JDK-free / SDK-free env: ONLY `DEJA_ADB` (no PATH, no JAVA_HOME, no ANDROID_HOME, no DEJA_ANDROID_AGENT_APK).
        let mut env = BTreeMap::new();
        env.insert("DEJA_ADB".to_string(), adb.to_string_lossy().into_owned());

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
        assert!(
            by_name("android-agent")
                .detail
                .contains("Embedded prebuilt"),
            "JDK-free host should report embedded prebuilt agent ready: {}",
            by_name("android-agent").detail
        );

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[tokio::test]
    async fn prepare_batch_build_cancellation_signals_worker_awaits_and_cleans_staging_dir() {
        let unique = format!(
            "deja-stage-cancel-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let root = std::env::temp_dir().join(unique);
        let app_dir = root.join("Demo.app");
        fs::create_dir_all(&app_dir).unwrap();
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>com.example.demo</string>
</dict></plist>
"#;
        fs::write(app_dir.join("Info.plist"), plist).unwrap();
        fs::write(app_dir.join("Demo"), b"fake-mach-o").unwrap();

        // 1. Pre-cancelled `RealDriverFactory::prepare_batch_build` returns `Cancelled` immediately.
        let pre_cancel = CancellationToken::new();
        pre_cancel.cancel();
        let factory = RealDriverFactory;
        let ctx = DriverContext {
            platform: "ios".to_string(),
            app_id: "com.example.demo".to_string(),
            device: "00000000-0000-4000-8000-000000000001".to_string(),
            build_path: app_dir.clone(),
            expected_build_fingerprint: None,
            workspace_root: root.clone(),
            staged_build: None,
            disable_animations: true,
            crash_report_grace_ms: 0,
            crash_report_dir: None,
            deja_home: None,
            headless: None,
            screen_size: None,
            timeout_ms: 30_000,
            env: BTreeMap::new(),
        };
        let pre_res = factory.prepare_batch_build(&ctx, &pre_cancel).await;
        assert!(
            matches!(pre_res, Err(DriverFactoryError::Cancelled)),
            "pre-cancelled prepare_batch_build must return Cancelled"
        );

        // 2. Mid-staging cancellation sets `cancel_flag`, awaits the `spawn_blocking` worker to
        //    completion, and deletes the temporary staging directory via `StagedIosAppBundle::drop`.
        let cancel = CancellationToken::new();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<PathBuf>();
        let worker_saw_cancel = Arc::new(AtomicBool::new(false));
        let worker_saw_cancel_hook = Arc::clone(&worker_saw_cancel);
        let staged_dir_slot: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let staged_dir_slot_hook = Arc::clone(&staged_dir_slot);

        let hook: StageTestHook = Box::new(move |temp_dir: &Path, flag: &AtomicBool| {
            *staged_dir_slot_hook.lock().unwrap() = Some(temp_dir.to_path_buf());
            let _ = started_tx.send(temp_dir.to_path_buf());
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !flag.load(Ordering::SeqCst) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "cancel_flag was never signaled to blocking staging worker"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            worker_saw_cancel_hook.store(true, Ordering::SeqCst);
        });

        let cancel_trigger = cancel.clone();
        let trigger_task = tokio::spawn(async move {
            let dir = started_rx.await.expect("staging hook must run");
            assert!(
                dir.exists(),
                "temp staging directory must exist while staging is in flight"
            );
            cancel_trigger.cancel();
        });

        let res = run_cancellable_ios_bundle_staging_with_hook(
            app_dir.clone(),
            "com.example.demo".to_string(),
            None,
            &cancel,
            Some(hook),
        )
        .await;
        trigger_task.await.unwrap();

        assert!(
            matches!(res, Err(DriverFactoryError::Cancelled)),
            "cancelled staging must return DriverFactoryError::Cancelled"
        );
        assert!(
            worker_saw_cancel.load(Ordering::SeqCst),
            "cancel path must signal cancel_flag AND await the spawn_blocking worker before returning"
        );
        let staged_dir = staged_dir_slot
            .lock()
            .unwrap()
            .clone()
            .expect("staging temp dir recorded");
        assert!(
            !staged_dir.exists(),
            "cancelled staging must clean up temporary staging directory {}",
            staged_dir.display()
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stage_ios_bundle_cancellable_aborts_during_fingerprinting_and_cleans_up() {
        let unique = format!(
            "deja-stage-fp-cancel-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let root = std::env::temp_dir().join(unique);
        let app_dir = root.join("Big.app");
        fs::create_dir_all(&app_dir).unwrap();
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>com.example.big</string>
</dict></plist>
"#;
        fs::write(app_dir.join("Info.plist"), plist).unwrap();
        let blob = vec![0x42u8; deja_core::FINGERPRINT_CHUNK_BYTES * 4];
        for i in 0..8 {
            fs::write(app_dir.join(format!("asset-{i:02}.bin")), &blob).unwrap();
        }

        // Flip `cancel_flag` from a concurrent thread as soon as the copy completes (inside the
        // fingerprinting window) and ensure either Cancelled (with staging dir cleaned) or valid
        // completion occurs — never a leaked temp dir or panic.
        let staged_dir_slot: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let staged_dir_slot_hook = Arc::clone(&staged_dir_slot);
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let worker_flag = Arc::clone(&cancel_flag);

        let hook: StageTestHook = Box::new(move |temp_dir: &Path, flag: &AtomicBool| {
            *staged_dir_slot_hook.lock().unwrap() = Some(temp_dir.to_path_buf());
            // Flip flag directly after copy so `verify_ios_app_bundle` / `fingerprint_tree_cancellable`
            // sees cancellation before hashing completes.
            flag.store(true, Ordering::SeqCst);
        });

        let res = stage_and_verify_ios_app_bundle_inner(
            &app_dir,
            "com.example.big",
            None,
            &worker_flag,
            Some(hook),
        );
        assert!(
            matches!(res, Err(StageBundleError::Cancelled)),
            "expected Cancelled when flag flips before fingerprinting"
        );
        let staged_dir = staged_dir_slot
            .lock()
            .unwrap()
            .clone()
            .expect("temp dir recorded");
        assert!(
            !staged_dir.exists(),
            "temp staging directory must be removed on cancellation: {}",
            staged_dir.display()
        );

        let _ = fs::remove_dir_all(&root);
    }
}
