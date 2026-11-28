//! Driver factory abstraction and immutable server launch configuration for `deja-mcp`.
//!
//! `deja-mcp` never depends on concrete platform driver crates (`deja-driver-ios`,
//! `deja-driver-android`, `deja-driver-web`) or `deja-cli`. The composition root (`deja-cli`)
//! injects a concrete [`McpDriverFactory`], while tests can supply an in-process mock factory.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use deja_core::StaleBuildPolicy;
use deja_flow::{DriverError, PlatformDriver};
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// Dyn-compatible boxed future returned by [`McpDriverFactory`] methods.
pub type McpBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Top-level error emitted by [`crate::serve`].
#[derive(Debug, Error)]
pub enum McpServerError {
    #[error("invalid MCP server configuration: {0}")]
    InvalidConfig(String),

    #[error("MCP stdio transport error: {0}")]
    Io(#[from] std::io::Error),
}

/// Execution context passed to [`McpDriverFactory`] when resolving runtime, installing an app,
/// or instantiating a [`PlatformDriver`].
#[derive(Debug, Clone)]
pub struct McpDriverContext {
    pub platform: String,
    pub device: String,
    pub app_id: String,
    pub build_path: PathBuf,
    pub expected_build_fingerprint: Option<String>,
    pub workspace_root: PathBuf,
    pub disable_animations: bool,
    pub crash_report_grace_ms: u64,
    pub crash_report_dir: Option<PathBuf>,
    pub timeout_ms: u64,
    pub env: BTreeMap<String, String>,
    pub headless: Option<bool>,
    pub screen_size: Option<Value>,
}

/// Trait-abstracted driver lifecycle injected by `deja-cli` (or test mocks) into `deja-mcp`.
pub trait McpDriverFactory: Send + Sync {
    /// Resolve the normalized OS runtime identifier for the target device (e.g. `"ios-27.2"`).
    fn resolve_runtime<'a>(
        &'a self,
        ctx: &'a McpDriverContext,
        cancel: &'a CancellationToken,
    ) -> McpBoxFuture<'a, Result<Option<String>, DriverError>>;

    /// Ensure the fingerprinted build artifact is installed on the target device.
    fn install_app<'a>(
        &'a self,
        ctx: &'a McpDriverContext,
        cancel: &'a CancellationToken,
    ) -> McpBoxFuture<'a, Result<(), DriverError>>;

    /// Instantiate a [`PlatformDriver`] bound to `ctx`.
    fn create_driver<'a>(
        &'a self,
        ctx: &'a McpDriverContext,
    ) -> McpBoxFuture<'a, Result<Box<dyn PlatformDriver>, DriverError>>;
}

/// Immutable server-launch confinement snapshot captured once at `deja mcp` startup.
///
/// No MCP tool call may override `workspace_root`, `store_id`, `platform`, `app_id`,
/// `device`, `build_path`, or `acceptAssertionChange`.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub workspace_root: PathBuf,
    pub canonical_workspace_root: PathBuf,
    pub home_dir: Option<PathBuf>,
    pub store_id: Option<String>,
    pub platform: String,
    pub app_id: String,
    pub adapter: Option<String>,
    pub browser: Option<String>,
    pub build_path: Option<PathBuf>,
    pub device: Option<String>,
    pub manifest_path: Option<String>,
    pub flows_dir: Option<String>,
    pub timeout_ms: u64,
    pub verification_timeout_ms: u64,
    pub retries: u8,
    pub crash_report_grace_ms: u64,
    pub crash_report_dir: Option<PathBuf>,
    pub disable_animations: bool,
    pub stale_build: Option<StaleBuildPolicy>,
    /// Flow-level environment variables configured in `deja.config.json`.
    pub env: BTreeMap<String, String>,
    /// Process environment variables used for driver invocation and provider-key redaction.
    pub process_env: BTreeMap<String, String>,
    /// Configured fixture credentials / secrets / redaction literals.
    pub forbidden_values: Vec<String>,
    /// Gates whether `deja_explore` is enabled by config (`mcp.explore`).
    pub mcp_explore: bool,
    /// Indicates whether a concrete exploration backend is wired (remains `false` until `DEJA-71`).
    /// `deja_explore` is advertised in `tools/list` ONLY when BOTH `mcp_explore` AND
    /// `explore_backend_available` are `true`.
    pub explore_backend_available: bool,
    pub headless: Option<bool>,
    pub screen_size: Option<Value>,
}

impl McpServerConfig {
    /// Validate that the configured `store_id` (when set) and `canonical_workspace_root` are valid.
    pub fn validate(&self) -> Result<(), McpServerError> {
        if let Some(ref store_id) = self.store_id {
            deja_store::validate_store_id(store_id)
                .map_err(|err| McpServerError::InvalidConfig(err.to_string()))?;
        }
        if !self.canonical_workspace_root.is_dir() {
            return Err(McpServerError::InvalidConfig(format!(
                "workspace root is not an existing directory: {}",
                self.canonical_workspace_root.display()
            )));
        }
        if self.platform.trim().is_empty() {
            return Err(McpServerError::InvalidConfig(
                "platform must not be empty".to_string(),
            ));
        }
        if self.app_id.trim().is_empty() {
            return Err(McpServerError::InvalidConfig(
                "appId must not be empty".to_string(),
            ));
        }
        if self.timeout_ms == 0 {
            return Err(McpServerError::InvalidConfig(
                "timeoutMs must be greater than 0".to_string(),
            ));
        }
        Ok(())
    }

    /// Build a [`McpDriverContext`] for tools that require both an existing `build_path` and a
    /// configured `device` (`deja_run`, `deja_remember`).
    pub fn driver_context_requiring_build(
        &self,
        timeout_ms: u64,
        expected_build_fingerprint: Option<String>,
    ) -> Result<McpDriverContext, (&'static str, String)> {
        let Some(ref build_path) = self.build_path else {
            return Err((
                "BUILD_PATH_REQUIRED",
                "`buildPath` must be configured in deja.config.json before running device tools"
                    .to_string(),
            ));
        };
        if !build_path.exists() {
            return Err((
                "BUILD_PATH_MISSING",
                format!(
                    "`buildPath` does not exist on disk: {}",
                    build_path.display()
                ),
            ));
        }
        let device = self.require_device()?;
        Ok(self.make_driver_context(
            device,
            build_path.clone(),
            timeout_ms,
            expected_build_fingerprint,
        ))
    }

    /// Build a [`McpDriverContext`] for `deja_exec_batch` / `deja_run_steps`, requiring both
    /// an existing `build_path` and a configured `device` up front (consistent with `deja_run`
    /// and `deja_remember`).
    pub fn driver_context_for_batch(
        &self,
        timeout_ms: u64,
        expected_build_fingerprint: Option<String>,
    ) -> Result<McpDriverContext, (&'static str, String)> {
        self.driver_context_requiring_build(timeout_ms, expected_build_fingerprint)
    }

    fn require_device(&self) -> Result<String, (&'static str, String)> {
        match self.device.as_deref().map(str::trim) {
            Some(dev) if !dev.is_empty() => Ok(dev.to_string()),
            _ => Err((
                "DEVICE_REQUIRED",
                "a target device is required (pass `--device <id>` when starting `deja mcp`, set `DEJA_DEVICE`, or configure `device` in deja.config.json)"
                    .to_string(),
            )),
        }
    }

    fn make_driver_context(
        &self,
        device: String,
        build_path: PathBuf,
        timeout_ms: u64,
        expected_build_fingerprint: Option<String>,
    ) -> McpDriverContext {
        McpDriverContext {
            platform: self.platform.clone(),
            device,
            app_id: self.app_id.clone(),
            build_path,
            expected_build_fingerprint,
            workspace_root: self.canonical_workspace_root.clone(),
            disable_animations: self.disable_animations,
            crash_report_grace_ms: self.crash_report_grace_ms,
            crash_report_dir: self.crash_report_dir.clone(),
            timeout_ms,
            env: self.process_env.clone(),
            headless: self.headless,
            screen_size: self.screen_size.clone(),
        }
    }
}
