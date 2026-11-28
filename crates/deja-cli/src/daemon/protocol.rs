//! Bounded NDJSON IPC protocol for the persistent warm driver daemon (`DEJA-78`).
//!
//! Enforces a hard 32 MiB maximum frame size with a cancel-safe bounded line reader, lossless
//! round-trip serialization for every [`PlatformDriver`] method and all 10 [`DriverError`]
//! variants, and strict secret isolation (never transmitting client redactors or API-key env vars).

use std::collections::BTreeMap;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::Duration;

use deja_flow::{
    ActOutcome, ActPayload, DriverCapability, DriverCrashSignal, DriverError, KeyPress,
    LaunchOptions, OpenLinkOptions, Selector, SwipeParams, UiHierarchy, UiNode, UiPoint,
    UnsupportedCapability, VisibleContext, WaitSelectorOutcome, WaitSelectorRequest,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::daemon::filter_driver_env_for_context;
use crate::driver_factory::{DriverContext, DriverFactoryError};
use crate::fs_safe::lexical_normalize;

/// Hard maximum size of a single NDJSON frame (32 MiB) to bound memory usage.
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// Errors emitted while framing or decoding daemon IPC messages.
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("daemon IPC frame exceeded maximum size of {MAX_FRAME_BYTES} bytes")]
    FrameTooLarge,
    #[error("unexpected EOF while reading daemon IPC frame")]
    UnexpectedEof,
    #[error("invalid daemon IPC JSON frame: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Cancel-safe bounded NDJSON frame reader.
///
/// Retaining the in-progress line buffer on `self` guarantees that cancelling `next_frame()`
/// inside a `tokio::select!` never discards already-buffered bytes or corrupts framing.
pub struct BoundedNdjsonReader<R> {
    reader: BufReader<R>,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> BoundedNdjsonReader<R> {
    #[must_use]
    pub fn new(inner: R) -> Self {
        Self {
            reader: BufReader::new(inner),
            buf: Vec::new(),
        }
    }

    /// Read and deserialize the next newline-delimited JSON frame up to [`MAX_FRAME_BYTES`].
    /// Returns `Ok(None)` on clean EOF at a frame boundary.
    pub async fn next_frame<T: DeserializeOwned>(&mut self) -> Result<Option<T>, ProtocolError> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                return Err(ProtocolError::UnexpectedEof);
            }
            if let Some(newline_idx) = available.iter().position(|&b| b == b'\n') {
                if self.buf.len().saturating_add(newline_idx) > MAX_FRAME_BYTES {
                    self.reader.consume(newline_idx + 1);
                    self.buf.clear();
                    return Err(ProtocolError::FrameTooLarge);
                }
                self.buf.extend_from_slice(&available[..newline_idx]);
                self.reader.consume(newline_idx + 1);

                if self.buf.last() == Some(&b'\r') {
                    self.buf.pop();
                }
                if self.buf.is_empty() {
                    continue;
                }
                let parsed = serde_json::from_slice(&self.buf);
                self.buf.clear();
                return Ok(Some(parsed?));
            }
            let chunk_len = available.len();
            if self.buf.len().saturating_add(chunk_len) > MAX_FRAME_BYTES {
                self.buf.clear();
                return Err(ProtocolError::FrameTooLarge);
            }
            self.buf.extend_from_slice(available);
            self.reader.consume(chunk_len);
        }
    }
}

/// Serialize `msg` as a single newline-delimited JSON frame enforcing [`MAX_FRAME_BYTES`].
pub async fn write_ndjson_frame<W, T>(writer: &mut W, msg: &T) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut bytes = serde_json::to_vec(msg)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge);
    }
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Resolve relative `DEJA_CHROME` / `CHROME_PATH` overrides against `cwd` so a persistent daemon
/// launched from another directory always executes the exact binary validated by the client.
pub fn absolutize_chrome_env_paths_with_cwd(env: &mut BTreeMap<String, String>, cwd: &Path) {
    for key in ["DEJA_CHROME", "CHROME_PATH"] {
        if let Some(val) = env.get_mut(key) {
            let trimmed = val.trim();
            if trimmed.is_empty() {
                continue;
            }
            let p = Path::new(trimmed);
            if !p.is_absolute() {
                let resolved = lexical_normalize(&cwd.join(p));
                *val = resolved.to_string_lossy().into_owned();
            }
        }
    }
}

/// Resolve relative `DEJA_CHROME` / `CHROME_PATH` overrides against `std::env::current_dir()`.
pub fn absolutize_chrome_env_paths(env: &mut BTreeMap<String, String>) {
    if let Ok(cwd) = std::env::current_dir() {
        absolutize_chrome_env_paths_with_cwd(env, &cwd);
    }
}

/// Sanitized driver creation context sent over daemon IPC.
///
/// Only allowlisted toolchain env vars ([`crate::daemon::ALLOWED_DRIVER_ENV_KEYS`], plus
/// [`crate::daemon::ALLOWED_WEB_HEADFUL_ENV_KEYS`] for headful Web runs) are included;
/// API keys and fixture credentials are never placed on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireDriverContext {
    pub platform: String,
    pub device: String,
    pub app_id: String,
    pub build_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_build_fingerprint: Option<String>,
    pub workspace_root: PathBuf,
    pub disable_animations: bool,
    pub crash_report_grace_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crash_report_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deja_home: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headless: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_size: Option<serde_json::Value>,
    pub timeout_ms: u64,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_runtime: Option<String>,
}

impl WireDriverContext {
    /// Convert a local [`DriverContext`] into a sanitized wire payload stripping non-allowlisted
    /// environment variables and absolutizing relative Chromium binary overrides.
    #[must_use]
    pub fn from_driver_context(ctx: &DriverContext) -> Self {
        let mut env = filter_driver_env_for_context(&ctx.platform, ctx.headless, &ctx.env);
        absolutize_chrome_env_paths(&mut env);
        Self {
            platform: ctx.platform.clone(),
            device: ctx.device.clone(),
            app_id: ctx.app_id.clone(),
            build_path: ctx.build_path.clone(),
            expected_build_fingerprint: ctx.expected_build_fingerprint.clone(),
            workspace_root: ctx.workspace_root.clone(),
            disable_animations: ctx.disable_animations,
            crash_report_grace_ms: ctx.crash_report_grace_ms,
            crash_report_dir: ctx.crash_report_dir.clone(),
            deja_home: ctx.deja_home.clone(),
            headless: ctx.headless,
            screen_size: ctx.screen_size.clone(),
            timeout_ms: ctx.timeout_ms,
            env,
            resolved_runtime: ctx.resolved_runtime.clone(),
        }
    }

    /// Reconstruct a [`DriverContext`] on the daemon side, re-applying the env allowlist as
    /// defense in depth.
    #[must_use]
    pub fn into_driver_context(self) -> DriverContext {
        let env = filter_driver_env_for_context(&self.platform, self.headless, &self.env);
        DriverContext {
            platform: self.platform,
            device: self.device,
            app_id: self.app_id,
            build_path: self.build_path,
            expected_build_fingerprint: self.expected_build_fingerprint,
            workspace_root: self.workspace_root,
            disable_animations: self.disable_animations,
            crash_report_grace_ms: self.crash_report_grace_ms,
            crash_report_dir: self.crash_report_dir,
            deja_home: self.deja_home,
            headless: self.headless,
            screen_size: self.screen_size,
            timeout_ms: self.timeout_ms,
            env,
            staged_build: None,
            resolved_runtime: self.resolved_runtime,
        }
    }
}

/// Wire representation of [`WaitSelectorRequest`] that NEVER transmits client redactors or
/// provider-key secrets over IPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireWaitSelectorRequest {
    pub app_id: String,
    pub selector: Selector,
    pub not_visible: bool,
    pub require_hittable: bool,
    pub wait_stable: bool,
    pub stable_polls: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_fast_on_settle_polls: Option<usize>,
    pub timeout_secs: u64,
    pub timeout_nanos: u32,
    pub include_system_ui: bool,
    pub return_hierarchy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_hierarchy: Option<UiHierarchy>,
}

impl WireWaitSelectorRequest {
    #[must_use]
    pub fn from_request(req: &WaitSelectorRequest, force_return_hierarchy: bool) -> Self {
        Self {
            app_id: req.app_id.clone(),
            selector: req.selector.clone(),
            not_visible: req.not_visible,
            require_hittable: req.require_hittable,
            wait_stable: req.wait_stable,
            stable_polls: req.stable_polls,
            fail_fast_on_settle_polls: req.fail_fast_on_settle_polls,
            timeout_secs: req.timeout.as_secs(),
            timeout_nanos: req.timeout.subsec_nanos(),
            include_system_ui: req.include_system_ui,
            return_hierarchy: req.return_hierarchy || force_return_hierarchy,
            initial_hierarchy: req.initial_hierarchy.clone(),
        }
    }

    #[must_use]
    pub fn into_request(self) -> WaitSelectorRequest {
        WaitSelectorRequest {
            app_id: self.app_id,
            selector: self.selector,
            not_visible: self.not_visible,
            require_hittable: self.require_hittable,
            wait_stable: self.wait_stable,
            stable_polls: self.stable_polls,
            fail_fast_on_settle_polls: self.fail_fast_on_settle_polls,
            timeout: Duration::new(self.timeout_secs, self.timeout_nanos),
            include_system_ui: self.include_system_ui,
            return_hierarchy: self.return_hierarchy,
            initial_hierarchy: self.initial_hierarchy,
            redactor: None,
            provider_redactor: None,
        }
    }
}

/// Wire representation of [`WaitSelectorOutcome`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireWaitSelectorOutcome {
    pub matched: bool,
    pub elapsed_ms: u64,
    pub polls: u32,
    pub settled_early: bool,
    pub obtained_fresh_observation: bool,
    pub hierarchy_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_node: Option<UiNode>,
    pub visible_context: VisibleContext,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hierarchy: Option<UiHierarchy>,
}

impl From<WaitSelectorOutcome> for WireWaitSelectorOutcome {
    fn from(v: WaitSelectorOutcome) -> Self {
        Self {
            matched: v.matched,
            elapsed_ms: v.elapsed_ms,
            polls: v.polls,
            settled_early: v.settled_early,
            obtained_fresh_observation: v.obtained_fresh_observation,
            hierarchy_hash: v.hierarchy_hash,
            matched_node: v.matched_node,
            visible_context: v.visible_context,
            hierarchy: v.hierarchy,
        }
    }
}

impl From<WireWaitSelectorOutcome> for WaitSelectorOutcome {
    fn from(v: WireWaitSelectorOutcome) -> Self {
        Self {
            matched: v.matched,
            elapsed_ms: v.elapsed_ms,
            polls: v.polls,
            settled_early: v.settled_early,
            obtained_fresh_observation: v.obtained_fresh_observation,
            hierarchy_hash: v.hierarchy_hash,
            matched_node: v.matched_node,
            visible_context: v.visible_context,
            hierarchy: v.hierarchy,
        }
    }
}

/// Lossless wire representation of all [`DriverFactoryError`] variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WireFactoryError {
    UnsupportedOrConfig { message: String },
    Cancelled,
    Runtime { message: String },
}

impl From<DriverFactoryError> for WireFactoryError {
    fn from(err: DriverFactoryError) -> Self {
        match err {
            DriverFactoryError::UnsupportedOrConfig(message) => {
                Self::UnsupportedOrConfig { message }
            }
            DriverFactoryError::Cancelled => Self::Cancelled,
            DriverFactoryError::Runtime(message) => Self::Runtime { message },
        }
    }
}

impl From<WireFactoryError> for DriverFactoryError {
    fn from(err: WireFactoryError) -> Self {
        match err {
            WireFactoryError::UnsupportedOrConfig { message } => Self::UnsupportedOrConfig(message),
            WireFactoryError::Cancelled => Self::Cancelled,
            WireFactoryError::Runtime { message } => Self::Runtime(message),
        }
    }
}

/// Lossless wire representation of all 10 [`DriverError`] variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WireDriverError {
    Unsupported(UnsupportedCapability),
    Cancelled,
    Timeout {
        operation: String,
        elapsed_ms: u64,
        detail: String,
    },
    LaunchFailed {
        app_id: String,
        detail: String,
    },
    NavigationFailed {
        target: String,
        detail: String,
    },
    AppCrashed(DriverCrashSignal),
    InvalidArgument {
        message: String,
    },
    Transport {
        operation: String,
        detail: String,
        transient: bool,
    },
    SessionClosed {
        message: String,
    },
    Io {
        io_kind: String,
        message: String,
    },
}

fn error_kind_to_wire(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::NotFound => "not_found",
        ErrorKind::PermissionDenied => "permission_denied",
        ErrorKind::ConnectionRefused => "connection_refused",
        ErrorKind::ConnectionReset => "connection_reset",
        ErrorKind::ConnectionAborted => "connection_aborted",
        ErrorKind::NotConnected => "not_connected",
        ErrorKind::AddrInUse => "addr_in_use",
        ErrorKind::AddrNotAvailable => "addr_not_available",
        ErrorKind::BrokenPipe => "broken_pipe",
        ErrorKind::AlreadyExists => "already_exists",
        ErrorKind::WouldBlock => "would_block",
        ErrorKind::InvalidInput => "invalid_input",
        ErrorKind::InvalidData => "invalid_data",
        ErrorKind::TimedOut => "timed_out",
        ErrorKind::WriteZero => "write_zero",
        ErrorKind::Interrupted => "interrupted",
        ErrorKind::Unsupported => "unsupported",
        ErrorKind::UnexpectedEof => "unexpected_eof",
        ErrorKind::OutOfMemory => "out_of_memory",
        _ => "other",
    }
}

fn error_kind_from_wire(raw: &str) -> ErrorKind {
    match raw {
        "not_found" => ErrorKind::NotFound,
        "permission_denied" => ErrorKind::PermissionDenied,
        "connection_refused" => ErrorKind::ConnectionRefused,
        "connection_reset" => ErrorKind::ConnectionReset,
        "connection_aborted" => ErrorKind::ConnectionAborted,
        "not_connected" => ErrorKind::NotConnected,
        "addr_in_use" => ErrorKind::AddrInUse,
        "addr_not_available" => ErrorKind::AddrNotAvailable,
        "broken_pipe" => ErrorKind::BrokenPipe,
        "already_exists" => ErrorKind::AlreadyExists,
        "would_block" => ErrorKind::WouldBlock,
        "invalid_input" => ErrorKind::InvalidInput,
        "invalid_data" => ErrorKind::InvalidData,
        "timed_out" => ErrorKind::TimedOut,
        "write_zero" => ErrorKind::WriteZero,
        "interrupted" => ErrorKind::Interrupted,
        "unsupported" => ErrorKind::Unsupported,
        "unexpected_eof" => ErrorKind::UnexpectedEof,
        "out_of_memory" => ErrorKind::OutOfMemory,
        _ => ErrorKind::Other,
    }
}

impl From<DriverError> for WireDriverError {
    fn from(err: DriverError) -> Self {
        match err {
            DriverError::Unsupported(u) => Self::Unsupported(u),
            DriverError::Cancelled => Self::Cancelled,
            DriverError::Timeout {
                operation,
                elapsed_ms,
                detail,
            } => Self::Timeout {
                operation,
                elapsed_ms,
                detail,
            },
            DriverError::LaunchFailed { app_id, detail } => Self::LaunchFailed { app_id, detail },
            DriverError::NavigationFailed { target, detail } => {
                Self::NavigationFailed { target, detail }
            }
            DriverError::AppCrashed(sig) => Self::AppCrashed(sig),
            DriverError::InvalidArgument(message) => Self::InvalidArgument { message },
            DriverError::Transport {
                operation,
                detail,
                transient,
            } => Self::Transport {
                operation,
                detail,
                transient,
            },
            DriverError::SessionClosed(message) => Self::SessionClosed { message },
            DriverError::Io(io_err) => Self::Io {
                io_kind: error_kind_to_wire(io_err.kind()).to_string(),
                message: io_err.to_string(),
            },
        }
    }
}

impl From<WireDriverError> for DriverError {
    fn from(err: WireDriverError) -> Self {
        match err {
            WireDriverError::Unsupported(u) => Self::Unsupported(u),
            WireDriverError::Cancelled => Self::Cancelled,
            WireDriverError::Timeout {
                operation,
                elapsed_ms,
                detail,
            } => Self::Timeout {
                operation,
                elapsed_ms,
                detail,
            },
            WireDriverError::LaunchFailed { app_id, detail } => {
                Self::LaunchFailed { app_id, detail }
            }
            WireDriverError::NavigationFailed { target, detail } => {
                Self::NavigationFailed { target, detail }
            }
            WireDriverError::AppCrashed(sig) => Self::AppCrashed(sig),
            WireDriverError::InvalidArgument { message } => Self::InvalidArgument(message),
            WireDriverError::Transport {
                operation,
                detail,
                transient,
            } => Self::Transport {
                operation,
                detail,
                transient,
            },
            WireDriverError::SessionClosed { message } => Self::SessionClosed(message),
            WireDriverError::Io { io_kind, message } => {
                Self::Io(io::Error::new(error_kind_from_wire(&io_kind), message))
            }
        }
    }
}

/// Every [`deja_flow::PlatformDriver`] RPC operation forwarded to a leased pooled driver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum DriverOp {
    EnsureSession,
    LaunchApp {
        app_id: String,
        opts: LaunchOptions,
    },
    StopApp {
        app_id: String,
    },
    KillApp {
        app_id: String,
    },
    ClearState {
        app_id: String,
    },
    ClearKeychain,
    OpenLink {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app_id: Option<String>,
        url: String,
        opts: OpenLinkOptions,
    },
    AddMedia {
        paths: Vec<PathBuf>,
    },
    Hierarchy {
        app_id: String,
        include_system_ui: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        known_hash: Option<String>,
    },
    TapPoint {
        app_id: String,
        point: UiPoint,
    },
    LongPressPoint {
        app_id: String,
        point: UiPoint,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    DoubleTapPoint {
        app_id: String,
        point: UiPoint,
    },
    Swipe {
        params: SwipeParams,
    },
    InputText {
        app_id: String,
        text: String,
    },
    EraseText {
        app_id: String,
        count: u32,
    },
    PressKey {
        app_id: String,
        key: KeyPress,
    },
    HideKeyboard {
        app_id: String,
    },
    Back {
        app_id: String,
    },
    TakeScreenshot {
        app_id: String,
    },
    CheckAppCrash {
        app_id: String,
        since_epoch_ms: u64,
    },
    ActAndObserve {
        req: ActPayload,
    },
    WaitForSelector {
        req: Box<WireWaitSelectorRequest>,
    },
}

/// Successful return payload for [`DriverOp`]. Screenshot PNG bytes are hex-encoded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum WireDriverValue {
    Unit,
    Hierarchy(UiHierarchy),
    ScreenshotHex(String),
    CrashSignal(Option<DriverCrashSignal>),
    ActOutcome(ActOutcome),
    WaitSelectorOutcome(Box<WireWaitSelectorOutcome>),
}

/// RPC result envelope matching `Result<WireDriverValue, WireDriverError>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum WireDriverResult {
    Ok { data: WireDriverValue },
    Err { error: WireDriverError },
}

impl WireDriverResult {
    pub fn from_result(res: Result<WireDriverValue, DriverError>) -> Self {
        match res {
            Ok(data) => Self::Ok { data },
            Err(err) => Self::Err { error: err.into() },
        }
    }

    pub fn into_result(self) -> Result<WireDriverValue, DriverError> {
        match self {
            Self::Ok { data } => Ok(data),
            Self::Err { error } => Err(error.into()),
        }
    }
}

/// Status of one warm pooled driver slot reported by `deja daemon status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PooledSessionStatus {
    pub device_key: String,
    pub platform: String,
    pub device: String,
    pub session_key: String,
    pub leased: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_count: Option<u64>,
    pub lease_count: u64,
    pub idle_ms: u64,
}

/// Full structured daemon status report emitted by `Status` RPC and `deja daemon status --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonStatusReport {
    pub running: bool,
    pub version: String,
    pub pid: u32,
    pub uid: u32,
    pub socket_path: String,
    pub started_at: String,
    pub uptime_ms: u64,
    pub idle_ms: u64,
    pub idle_timeout_secs: u64,
    pub active_leases: usize,
    pub pooled_sessions: usize,
    pub sessions: Vec<PooledSessionStatus>,
}

/// Client-to-daemon NDJSON message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientMessage {
    Ping,
    Status,
    Shutdown,
    AcquireLease {
        ctx: WireDriverContext,
        session_key: String,
    },
    Rpc {
        id: u64,
        op: DriverOp,
    },
    CancelRpc {
        id: u64,
    },
    LeaseRelease,
}

/// Daemon-to-client NDJSON message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMessage {
    Pong {
        version: String,
        pid: u32,
    },
    StatusReport(DaemonStatusReport),
    ShutdownAck {
        stopped_sessions: usize,
    },
    LeaseAcquired {
        lease_id: u64,
        platform: String,
        capabilities: Vec<DriverCapability>,
        session_key: String,
        warm_hit: bool,
    },
    FactoryError(WireFactoryError),
    RpcResult {
        id: u64,
        result: WireDriverResult,
    },
    LeaseReleased,
    ProtocolError {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use deja_core::CrashSignalSource;

    #[test]
    fn all_ten_driver_error_variants_round_trip_losslessly() {
        let cases = vec![
            DriverError::Unsupported(UnsupportedCapability {
                platform: "web".to_string(),
                capability: DriverCapability::ClearKeychain,
                reason: "no keychain".to_string(),
            }),
            DriverError::Cancelled,
            DriverError::Timeout {
                operation: "hierarchy".to_string(),
                elapsed_ms: 1500,
                detail: "timed out".to_string(),
            },
            DriverError::LaunchFailed {
                app_id: "dev.deja.app".to_string(),
                detail: "missing".to_string(),
            },
            DriverError::NavigationFailed {
                target: "http://127.0.0.1:4174".to_string(),
                detail: "refused".to_string(),
            },
            DriverError::AppCrashed(DriverCrashSignal {
                source: CrashSignalSource::CrashReport,
                report_file: Some("app.ips".to_string()),
                process: Some("App".to_string()),
                detail: Some("SIGABRT".to_string()),
            }),
            DriverError::InvalidArgument("bad coord".to_string()),
            DriverError::Transport {
                operation: "tap".to_string(),
                detail: "socket reset".to_string(),
                transient: true,
            },
            DriverError::SessionClosed("gone".to_string()),
            DriverError::Io(io::Error::new(ErrorKind::BrokenPipe, "broken pipe")),
        ];

        for orig in cases {
            let orig_str = orig.to_string();
            let wire = WireDriverError::from(orig);
            let json = serde_json::to_string(&wire).expect("serialize WireDriverError");
            let parsed: WireDriverError =
                serde_json::from_str(&json).expect("deserialize WireDriverError");
            assert_eq!(wire, parsed);
            let round_tripped = DriverError::from(parsed);
            assert_eq!(round_tripped.to_string(), orig_str);
        }
    }

    #[tokio::test]
    async fn bounded_ndjson_reader_rejects_oversized_frame_without_oom() {
        let mut oversized = vec![b'a'; MAX_FRAME_BYTES + 16];
        oversized.push(b'\n');
        let mut reader = BoundedNdjsonReader::new(std::io::Cursor::new(oversized));
        let err = reader
            .next_frame::<ClientMessage>()
            .await
            .expect_err("must reject oversized frame");
        assert!(matches!(err, ProtocolError::FrameTooLarge));
    }

    #[tokio::test]
    async fn bounded_ndjson_reader_round_trips_messages_and_eof() {
        let mut wire = Vec::new();
        write_ndjson_frame(&mut wire, &ClientMessage::Ping)
            .await
            .unwrap();
        write_ndjson_frame(&mut wire, &ClientMessage::CancelRpc { id: 42 })
            .await
            .unwrap();

        let mut reader = BoundedNdjsonReader::new(std::io::Cursor::new(wire));
        assert_eq!(
            reader.next_frame::<ClientMessage>().await.unwrap(),
            Some(ClientMessage::Ping)
        );
        assert_eq!(
            reader.next_frame::<ClientMessage>().await.unwrap(),
            Some(ClientMessage::CancelRpc { id: 42 })
        );
        assert_eq!(reader.next_frame::<ClientMessage>().await.unwrap(), None);
    }

    #[test]
    fn wire_driver_context_absolutizes_relative_chrome_paths_and_preserves_headful_display() {
        let cwd = std::env::current_dir().unwrap();
        let mut env = BTreeMap::new();
        env.insert("DEJA_CHROME".to_string(), "./bin/my-chrome".to_string());
        env.insert("CHROME_PATH".to_string(), "tools/chrome-bin".to_string());
        env.insert("DISPLAY".to_string(), ":1".to_string());
        env.insert("WAYLAND_DISPLAY".to_string(), "wayland-0".to_string());
        env.insert("XAUTHORITY".to_string(), "/tmp/.Xauthority".to_string());
        env.insert("XDG_RUNTIME_DIR".to_string(), "/run/user/1000".to_string());
        env.insert("OPENAI_API_KEY".to_string(), "secret-val".to_string());

        let ctx = DriverContext {
            platform: "web".to_string(),
            device: "chromium".to_string(),
            app_id: "http://127.0.0.1:4174".to_string(),
            build_path: PathBuf::from("/tmp/app"),
            expected_build_fingerprint: None,
            workspace_root: PathBuf::from("/tmp/ws"),
            disable_animations: true,
            crash_report_grace_ms: 0,
            crash_report_dir: None,
            deja_home: None,
            headless: Some(false),
            screen_size: None,
            timeout_ms: 30_000,
            env,
            staged_build: None,
            resolved_runtime: Some("chromium-158.0.0.1@1280x720-headed".to_string()),
        };

        let wire = WireDriverContext::from_driver_context(&ctx);
        assert_eq!(
            wire.env.get("DEJA_CHROME").map(String::as_str),
            Some(
                lexical_normalize(&cwd.join("bin/my-chrome"))
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert_eq!(
            wire.env.get("CHROME_PATH").map(String::as_str),
            Some(
                lexical_normalize(&cwd.join("tools/chrome-bin"))
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert_eq!(wire.env.get("DISPLAY").map(String::as_str), Some(":1"));
        assert_eq!(
            wire.env.get("WAYLAND_DISPLAY").map(String::as_str),
            Some("wayland-0")
        );
        assert_eq!(
            wire.env.get("XAUTHORITY").map(String::as_str),
            Some("/tmp/.Xauthority")
        );
        assert_eq!(
            wire.env.get("XDG_RUNTIME_DIR").map(String::as_str),
            Some("/run/user/1000")
        );
        assert!(!wire.env.contains_key("OPENAI_API_KEY"));
        assert_eq!(
            wire.resolved_runtime.as_deref(),
            Some("chromium-158.0.0.1@1280x720-headed")
        );
    }
}
