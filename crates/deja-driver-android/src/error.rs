//! Typed error surface for `deja-driver-android`.

use std::path::PathBuf;
use thiserror::Error;

/// Typed errors emitted by the native Android driver and its `adb` / HTTP agent subsystems.
#[derive(Debug, Error)]
pub enum AndroidDriverError {
    #[error("Android `adb` binary not found: {0}")]
    AdbNotFound(String),

    #[error("invalid `{source_var}` executable path `{}`: {reason}", path.display())]
    InvalidAdbPath {
        source_var: String,
        path: PathBuf,
        reason: String,
    },

    #[error("invalid Android device serial `{0}`")]
    InvalidSerial(String),

    #[error("Android device `{serial}` not found (connected: {})", if available.is_empty() { "<none>".to_string() } else { available.join(", ") })]
    DeviceNotFound {
        serial: String,
        available: Vec<String>,
    },

    #[error("Android device `{serial}` is not ready (state={state}): {remediation}")]
    DeviceNotReady {
        serial: String,
        state: String,
        remediation: String,
    },

    #[error(
        "Android device serial mismatch for `{serial}`: `ro.serialno` reported `{actual_serialno}` (expected `{expected_serial}`); refusing to switch devices"
    )]
    DeviceIdentityMismatch {
        serial: String,
        expected_serial: String,
        actual_serialno: String,
    },

    #[error("Android device `{serial}` is locked: {detail}")]
    DeviceLocked { serial: String, detail: String },

    #[error("Android device `{serial}` is already locked by another active Déjà Android session")]
    DeviceBusy { serial: String },

    #[error("invalid Android package identifier `{0}`")]
    InvalidPackageId(String),

    #[error("invalid or unsafe path: {0}")]
    InvalidPath(String),

    #[error("invalid Android driver argument: {0}")]
    InvalidArgument(String),

    #[error("Déjà Android agent APK unavailable: {0}")]
    ApkUnavailable(String),

    #[error(
        "Déjà Android agent APK SHA-256 mismatch for `{}`: expected {expected}, got {actual}",
        path.display()
    )]
    ApkHashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    #[error("failed to spawn `{program}`: {source}")]
    CommandSpawn {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("command `{program} {args}` failed (exit={status:?}): {stderr}")]
    CommandFailed {
        program: String,
        args: String,
        status: Option<i32>,
        stderr: String,
    },

    #[error("I/O error ({context}): {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("JSON error ({context}): {source}")]
    Json {
        context: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("HTTP request to `{endpoint}` failed: {source}")]
    Http {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("Android agent endpoint `{endpoint}` returned HTTP {status}: {body}")]
    AgentHttpStatus {
        endpoint: String,
        status: u16,
        body: String,
    },

    #[error("Android agent failed to start: {0}")]
    AgentStartupFailed(String),

    #[error("Android instrumentation exited before becoming ready (exit={status:?}): {output}")]
    AgentExitedEarly { status: Option<i32>, output: String },

    #[error(
        "Android accessibility tree-busy circuit breaker opened after {consecutive} consecutive failures: {detail}"
    )]
    TreeBusyCircuitOpen { consecutive: u32, detail: String },

    #[error("invalid screenshot payload: {0}")]
    InvalidScreenshot(String),

    #[error("operation timed out: {0}")]
    Timeout(String),

    #[error("operation cancelled")]
    Cancelled,

    #[error("unsupported Android operation: {0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, AndroidDriverError>;
