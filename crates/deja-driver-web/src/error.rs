//! Typed errors for the native Chromium CDP Web driver (`deja-driver-web`).

use std::path::PathBuf;

use deja_core::CrashSignalSource;
use deja_flow::{DriverCrashSignal, DriverError, UnsupportedCapability};
use thiserror::Error;

/// Typed error surface for `deja-driver-web`.
#[derive(Debug, Error)]
pub enum WebDriverError {
    #[error("Chromium executable not found: {0}")]
    ChromiumNotFound(String),

    #[error("configured {source_var} path `{}` is invalid: {reason}", path.display())]
    InvalidChromiumPath {
        source_var: String,
        path: PathBuf,
        reason: String,
    },

    #[error("invalid loopback web URL: {0}")]
    InvalidUrl(String),

    #[error("invalid screenSize: {0}")]
    InvalidScreenSize(String),

    #[error("invalid web driver argument: {0}")]
    InvalidArgument(String),

    #[error(transparent)]
    Unsupported(#[from] UnsupportedCapability),

    #[error("failed to spawn Chromium at `{}`: {source}", path.display())]
    BrowserSpawn {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Chromium exited before DevTools became ready (exit={status:?}): {stderr_tail}")]
    BrowserExitedEarly {
        status: Option<i32>,
        stderr_tail: String,
    },

    #[error("invalid Chromium DevTools WebSocket URL: {0}")]
    DevToolsUrlInvalid(String),

    #[error("CDP WebSocket error: {0}")]
    WebSocket(String),

    #[error("CDP method `{method}` failed (code {code}): {message}")]
    CdpProtocol {
        method: String,
        code: i64,
        message: String,
    },

    #[error("CDP session closed: {0}")]
    CdpClosed(String),

    #[error("navigation to `{target}` failed: {detail}")]
    NavigationFailed { target: String, detail: String },

    #[error("Chromium target crashed: {detail}")]
    TargetCrashed { detail: String },

    #[error("invalid screenshot payload: {0}")]
    InvalidScreenshot(String),

    #[error("JSON error ({context}): {source}")]
    Json {
        context: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("I/O error ({context}): {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("timed out after {elapsed_ms}ms during {operation}: {detail}")]
    Timeout {
        operation: String,
        elapsed_ms: u64,
        detail: String,
    },

    #[error("operation cancelled")]
    Cancelled,
}

/// Convenience result alias for `deja-driver-web`.
pub type Result<T> = std::result::Result<T, WebDriverError>;

/// Maps a [`WebDriverError`] into a platform-neutral [`DriverError`].
#[must_use]
pub fn map_web_err(operation: &str, err: WebDriverError) -> DriverError {
    match err {
        WebDriverError::Cancelled => DriverError::Cancelled,
        WebDriverError::Unsupported(u) => DriverError::Unsupported(u),
        WebDriverError::Timeout {
            operation: op,
            elapsed_ms,
            detail,
        } => DriverError::Timeout {
            operation: if op.is_empty() {
                operation.to_string()
            } else {
                op
            },
            elapsed_ms,
            detail,
        },
        WebDriverError::InvalidUrl(msg)
        | WebDriverError::InvalidScreenSize(msg)
        | WebDriverError::InvalidArgument(msg) => DriverError::InvalidArgument(msg),
        WebDriverError::NavigationFailed { target, detail } => {
            DriverError::NavigationFailed { target, detail }
        }
        WebDriverError::TargetCrashed { detail } => DriverError::AppCrashed(DriverCrashSignal {
            source: CrashSignalSource::CrashReport,
            report_file: None,
            process: Some("chromium-renderer".to_string()),
            detail: Some(detail),
        }),
        WebDriverError::CdpClosed(detail) => DriverError::SessionClosed(detail),
        WebDriverError::WebSocket(detail) => DriverError::Transport {
            operation: operation.to_string(),
            detail,
            transient: true,
        },
        other => DriverError::Transport {
            operation: operation.to_string(),
            detail: other.to_string(),
            transient: false,
        },
    }
}
