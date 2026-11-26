use std::path::PathBuf;
use thiserror::Error;

/// Typed error surface for the native iOS simulator driver.
#[derive(Debug, Error)]
pub enum IosDriverError {
    #[error("invalid simulator UDID `{0}` (expected RFC 4122 UUID)")]
    InvalidUdid(String),

    #[error("invalid bundle identifier `{0}`")]
    InvalidBundleId(String),

    #[error("invalid or unsafe path: {0}")]
    InvalidPath(String),

    #[error("invalid environment or launch argument key `{0}`")]
    InvalidArgument(String),

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

    #[error("agent endpoint `{endpoint}` returned HTTP {status}: {body}")]
    AgentHttpStatus {
        endpoint: String,
        status: u16,
        body: String,
    },

    #[error("agent failed to start: {0}")]
    AgentStartupFailed(String),

    #[error("xcodebuild runner exited before becoming ready (exit={status:?}): {output}")]
    AgentExitedEarly { status: Option<i32>, output: String },

    #[error("operation cancelled")]
    Cancelled,

    #[error("operation timed out: {0}")]
    Timeout(String),

    #[error("Xcode toolchain detection failed: {0}")]
    Toolchain(String),

    #[error("invalid `.xctestrun` plist at `{path}`: {detail}")]
    InvalidXctestrun { path: PathBuf, detail: String },

    #[error("invalid screenshot payload: {0}")]
    InvalidScreenshot(String),

    #[error("unsupported platform operation: {0}")]
    UnsupportedPlatform(String),
}

pub type Result<T> = std::result::Result<T, IosDriverError>;
