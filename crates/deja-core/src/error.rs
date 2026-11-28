//! Error types for the pure domain core (`deja-core`).

use thiserror::Error;

/// Domain errors returned by `deja-core` operations.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("A non-empty goal is required")]
    EmptyGoal,

    #[error("Flow is not valid YAML: {0}")]
    YamlParse(String),

    #[error("{0}")]
    FlowReference(String),

    #[error("{0}")]
    FlowFileCommand(String),

    #[error("Cannot store flow: {0}")]
    FlowClosure(String),

    #[error("Cannot store flow: referenced file does not exist: {path}")]
    MissingFlowFile { path: String },

    #[error("Invalid walk manifest: {0}")]
    InvalidManifest(String),

    #[error(
        "Verified candidate bytes ({actual}) differ from the expected checksum ({expected}); nothing was stored"
    )]
    ExpectedFlowChecksumMismatch { expected: String, actual: String },

    #[error(
        "The candidate or a file it runs changed after it was verified ({actual}, expected {expected}); nothing was stored"
    )]
    ExpectedClosureMismatch { expected: String, actual: String },

    #[error(
        "The flow or a file it runs changed (manifest {manifest_closure_sha256}, now {current_closure_sha256}); re-verify it"
    )]
    FlowChanged {
        manifest_closure_sha256: String,
        current_closure_sha256: String,
    },

    #[error(
        "The flow's assertions changed (manifest {manifest_assertions_sha256}, now {current_assertions_sha256}); only a reviewed remember can accept that"
    )]
    AssertionHashChanged {
        manifest_assertions_sha256: String,
        current_assertions_sha256: String,
    },

    #[error("Owner rule not met: {0}")]
    OwnerRule(String),

    #[error(
        "A fixture credential value is shorter than {min_length} characters, so a written file could not be checked for it. Use a longer fixture value."
    )]
    ForbiddenValueTooShort { min_length: usize },

    #[error("{0}")]
    Semantic(String),

    #[error("{0}")]
    Fingerprint(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl CoreError {
    /// Returns `true` when this error corresponds to a closure refusal
    /// (`ClosureRefusedError` and its subclasses in the TypeScript core).
    #[must_use]
    pub fn is_closure_refused(&self) -> bool {
        matches!(
            self,
            Self::FlowClosure(_)
                | Self::MissingFlowFile { .. }
                | Self::InvalidManifest(_)
                | Self::FlowReference(_)
                | Self::FlowFileCommand(_)
        )
    }
}
