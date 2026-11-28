//! Stable Déjà CLI exit codes and mapping helpers.

use deja_core::{CoreError, RunOutcome};
use deja_store::StoreError;

/// Successful `run`, `remember`, `list`, `inspect` (passing report), `manifest check`, `gate`, `doctor`, `migrate`, `guide`, or `--help`/`--version`.
pub const EXIT_OK: u8 = 0;

/// Execution, assertion, product regression, rejected candidate, or `doctor` not-ready failure.
pub const EXIT_FAILURE: u8 = 1;

/// Usage, configuration, appId confinement, closure error, missing/malformed manifest, `engine install`, or direct I/O error.
pub const EXIT_USAGE_OR_CONFIG: u8 = 2;

/// Cold exploration was rejected or produced no verified walk.
pub const EXIT_EXPLORATION_REJECTED: u8 = 3;

/// Invalid, corrupt, or tampered walk memory.
pub const EXIT_INVALID_MEMORY: u8 = 4;

/// Store locked, stale-lock contention, or lock filesystem unavailable.
pub const EXIT_STORE_LOCKED: u8 = 5;

/// Manifest/flow/assertion drift (`flow-changed`, `assertions-changed`, `not-validated`, `needs-migration`, `manifest-locked`).
pub const EXIT_MANIFEST_DRIFT: u8 = 6;

/// Flaky run (`--rerun-failed` / `--retries` failed then passed or failed differently).
pub const EXIT_FLAKY: u8 = 7;

/// `deja manifest check` found one or more failing entries.
pub const EXIT_MANIFEST_CHECK_FAILED: u8 = 8;

/// `deja gate`: gate closed (at least one goal is `failed`, `flaky`, `not-run`, or `flow-changed`).
pub const EXIT_GATE_CLOSED: u8 = 10;

/// `deja gate`: error or undecidable (`invalid`, unreadable/corrupt store, build fingerprint failure, or evidence write failure).
pub const EXIT_GATE_ERROR: u8 = 11;

/// Process terminated by `SIGINT` (`128 + 2`).
pub const EXIT_SIGINT: u8 = 130;

/// Process terminated by `SIGTERM` (`128 + 15`).
pub const EXIT_SIGTERM: u8 = 143;

/// Signal code recorded on `CliDeps::cancel_signal` when no OS signal triggered cancellation.
pub const CANCEL_SIGNAL_NONE: u8 = 0;

/// Signal code recorded on `CliDeps::cancel_signal` when `SIGINT` triggered cancellation.
pub const CANCEL_SIGNAL_SIGINT: u8 = 2;

/// Signal code recorded on `CliDeps::cancel_signal` when `SIGTERM` triggered cancellation.
pub const CANCEL_SIGNAL_SIGTERM: u8 = 15;

/// Map a recorded `cancel_signal` value (`0`, `2`, `15`) to its exact CLI exit code (`1`, `130`, `143`).
#[must_use]
pub const fn exit_code_for_cancel_signal(signal: u8) -> u8 {
    match signal {
        CANCEL_SIGNAL_SIGTERM => EXIT_SIGTERM,
        CANCEL_SIGNAL_SIGINT => EXIT_SIGINT,
        _ => EXIT_FAILURE,
    }
}

/// Map a schema-1 [`RunOutcome`] to its exact Déjà CLI exit code.
#[must_use]
pub const fn exit_code_for_outcome(outcome: RunOutcome) -> u8 {
    match outcome {
        RunOutcome::Passed
        | RunOutcome::Remembered
        | RunOutcome::ColdRecorded
        | RunOutcome::Repaired => EXIT_OK,
        RunOutcome::Failed
        | RunOutcome::ProductRegression
        | RunOutcome::Rejected
        | RunOutcome::RepairRejected => EXIT_FAILURE,
        RunOutcome::ClosureError => EXIT_USAGE_OR_CONFIG,
        RunOutcome::ExplorationRejected => EXIT_EXPLORATION_REJECTED,
        RunOutcome::FlowChanged
        | RunOutcome::NotValidated
        | RunOutcome::NeedsMigration
        | RunOutcome::AssertionsChanged => EXIT_MANIFEST_DRIFT,
        RunOutcome::Flaky => EXIT_FLAKY,
    }
}

/// Map a [`StoreError`] to its exact CLI exit code for normal commands (`run`, `remember`, `list`, `inspect`, `migrate`).
#[must_use]
pub fn exit_code_for_store_error(err: &StoreError) -> u8 {
    match err {
        StoreError::Io(_) => EXIT_USAGE_OR_CONFIG,
        StoreError::Core(CoreError::Io(_)) => EXIT_USAGE_OR_CONFIG,
        other => other.exit_code(),
    }
}

/// Map a [`StoreError`] during `deja gate` evaluation to its exact exit code (`11` on unreadable/corrupt store).
#[must_use]
pub fn exit_code_for_gate_store_error(err: &StoreError) -> u8 {
    err.gate_exit_code()
}
