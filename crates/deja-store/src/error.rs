//! Error types and CLI exit-code mapping for `deja-store`.

use deja_core::CoreError;
use thiserror::Error;

use crate::lock::LockHolder;

/// Exit code used by `deja gate` when a store is corrupt, tampered, or unreadable.
pub const GATE_INVALID_EXIT_CODE: u8 = 11;

/// Errors returned by SQLite store, writer-lock, consumer-manifest, gate, and migration operations.
#[derive(Debug, Error)]
pub enum StoreError {
    /// Another writer currently holds the store's writer lock.
    #[error(
        "Store is locked by {reason}: {lock_path}. Only one writer may use a store. \
         Give each concurrent run its own store (storeId, or --store <id> on the CLI). \
         If no run is active, delete the lock file."
    )]
    StoreLocked {
        lock_path: String,
        holder: Option<Box<LockHolder>>,
        reason: String,
    },

    /// Two writers raced to take over the same stale lock.
    #[error(
        "Store lock contention ({reason}): {lock_path}. Another run is taking over a stale lock. \
         Try again. If it keeps failing, delete the .lock and .break files next to it."
    )]
    StoreLockContention {
        lock_path: String,
        holder: Option<Box<LockHolder>>,
        reason: String,
    },

    /// The filesystem or path cannot safely host the writer lock (e.g. symlink, no hard links, read-only).
    #[error(
        "Cannot create or verify the writer lock {lock_path}: {message}. \
         The writer lock needs a local filesystem that supports hard links."
    )]
    StoreLockUnavailable { lock_path: String, message: String },

    /// Walk memory, SQLite schema, row integrity checksum, or on-disk stored flow is corrupt or unsafe.
    #[error("Invalid walk memory: {0}")]
    InvalidWalkMemory(String),

    /// Consumer-owned flow violation in manifest mode.
    #[error("Consumer flow: {0}")]
    ConsumerFlow(String),

    /// Another process holds the shared `.deja/manifest.lock`.
    #[error(
        "Another run is writing the walk manifest ({lock_path}). \
         All stores of one workspace share it: run again in a moment. \
         If no run is active, delete that lock file."
    )]
    ManifestLocked { lock_path: String },

    /// `deja.walks.json` changed on disk between planning and commit.
    #[error("{0}")]
    ManifestChanged(String),

    /// Invalid `--store <id>` identifier.
    #[error(
        "Invalid store id {0:?}: use 1 to 128 letters, digits, dot, dash or underscore, \
         starting with a letter or digit."
    )]
    InvalidStoreId(String),

    /// Malformed ISO-8601 / RFC 3339 timestamp.
    #[error("Invalid ISO-8601 timestamp: {0}")]
    InvalidTimestamp(String),

    /// Missing or malformed walk/gate identity fields.
    #[error("Invalid walk identity: {0}")]
    InvalidIdentity(String),

    /// Candidate flow file does not exist.
    #[error("Candidate flow does not exist: {0}")]
    CandidateNotFound(String),

    /// Operation was cancelled via an explicit cancellation flag.
    #[error("Operation cancelled")]
    Cancelled,

    /// Underlying pure domain error from `deja-core`.
    #[error(transparent)]
    Core(#[from] CoreError),

    /// Underlying SQLite error from `rusqlite`.
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Underlying filesystem I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl StoreError {
    /// Stable machine-readable error code for JSON diagnostics and CLI mapping.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::StoreLocked { .. } => "STORE_LOCKED",
            Self::StoreLockContention { .. } => "STORE_LOCK_CONTENTION",
            Self::StoreLockUnavailable { .. } => "STORE_LOCK_UNAVAILABLE",
            Self::InvalidWalkMemory(_) => "INVALID_WALK_MEMORY",
            Self::ConsumerFlow(_) => "CONSUMER_FLOW",
            Self::ManifestLocked { .. } => "MANIFEST_LOCKED",
            Self::ManifestChanged(_) => "MANIFEST_CHANGED",
            Self::InvalidStoreId(_) => "INVALID_STORE_ID",
            Self::InvalidTimestamp(_) => "INVALID_TIMESTAMP",
            Self::InvalidIdentity(_) => "INVALID_IDENTITY",
            Self::CandidateNotFound(_) => "CANDIDATE_NOT_FOUND",
            Self::Cancelled => "CANCELLED",
            Self::Core(CoreError::FlowChanged { .. }) => "FLOW_CHANGED",
            Self::Core(CoreError::AssertionHashChanged { .. }) => "ASSERTIONS_CHANGED",
            Self::Core(CoreError::InvalidManifest(_)) => "INVALID_MANIFEST",
            Self::Core(CoreError::Cancelled) => "CANCELLED",
            Self::Core(_) => "CORE_ERROR",
            Self::Sqlite(_) => "SQLITE_ERROR",
            Self::Io(_) => "IO_ERROR",
        }
    }

    /// Returns `true` when this error is any writer-lock error.
    #[must_use]
    pub fn is_lock_error(&self) -> bool {
        matches!(
            self,
            Self::StoreLocked { .. }
                | Self::StoreLockContention { .. }
                | Self::StoreLockUnavailable { .. }
        )
    }

    /// Standard CLI exit code mapping:
    /// - `5`: writer lock errors (`StoreLocked`, `StoreLockContention`, `StoreLockUnavailable`)
    /// - `4`: corrupt/invalid walk memory (`InvalidWalkMemory`)
    /// - `6`: manifest changed/locked or flow/assertion drift (`ManifestLocked`, `ManifestChanged`,
    ///   `FlowChanged`, `AssertionHashChanged`)
    /// - `2`: invalid arguments, identity, store id, timestamp, consumer flow, or closure refusal
    /// - `1`: all other errors
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::StoreLocked { .. }
            | Self::StoreLockContention { .. }
            | Self::StoreLockUnavailable { .. } => 5,
            Self::InvalidWalkMemory(_) => 4,
            Self::ManifestLocked { .. }
            | Self::ManifestChanged(_)
            | Self::Core(CoreError::FlowChanged { .. } | CoreError::AssertionHashChanged { .. }) => {
                6
            }
            Self::InvalidStoreId(_)
            | Self::InvalidTimestamp(_)
            | Self::InvalidIdentity(_)
            | Self::ConsumerFlow(_)
            | Self::CandidateNotFound(_) => 2,
            Self::Core(err)
                if err.is_closure_refused()
                    || matches!(
                        err,
                        CoreError::EmptyGoal
                            | CoreError::OwnerRule(_)
                            | CoreError::ForbiddenValueTooShort { .. }
                    ) =>
            {
                2
            }
            Self::Cancelled | Self::Core(_) | Self::Sqlite(_) | Self::Io(_) => 1,
        }
    }

    /// Exit code mapping when this error occurs during release-gate evaluation (`DEJA-49`).
    ///
    /// Any corrupt, tampered, or unreadable store exits `11` (`GATE_INVALID_EXIT_CODE`).
    #[must_use]
    pub fn gate_exit_code(&self) -> u8 {
        match self {
            Self::InvalidWalkMemory(_)
            | Self::Sqlite(_)
            | Self::Io(_)
            | Self::StoreLocked { .. }
            | Self::StoreLockContention { .. }
            | Self::StoreLockUnavailable { .. }
            | Self::Core(CoreError::InvalidManifest(_)) => GATE_INVALID_EXIT_CODE,
            _ => self.exit_code(),
        }
    }
}
