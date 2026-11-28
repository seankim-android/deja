//! Structured `DEJA-52` failure classification and assertion counting.
//!
//! Because the native Rust interpreter tracks the exact failing [`crate::parser::FlowStep`]
//! structurally, both text-based and ID-only `assertVisible` as well as `assertNotVisible`
//! classify unambiguously as `FailureCategory::ProductAssertion` with `repair_eligible: false`.

use deja_core::{
    CoreError, FailureCategory, FlowClosure, WalkFailure, closure_assertions, find_flow_assertions,
};
use serde::{Deserialize, Serialize};

use crate::capabilities::VisibleContext;
use crate::driver::DriverCrashSignal;

/// Structural failure kind recorded by [`crate::executor::FlowExecutor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepFailureKind {
    Timeout,
    Cancelled,
    UnsupportedCapability,
    Transport,
    ParseOrConfinement,
    LimitExceeded,
    AppCrash,
    AssertionFailed,
    SelectorNotFound,
    NavigationFailed,
    Other,
}

/// Structured context captured for the first failing step in a flow execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedStepContext {
    pub step_index: usize,
    pub command: String,
    pub label: String,
    pub kind: StepFailureKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_context: Option<VisibleContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crash_signal: Option<DriverCrashSignal>,
}

/// Input to [`FailureClassifier::classify`].
#[derive(Debug, Clone)]
pub struct ClassificationInput<'a> {
    pub passed: bool,
    pub timed_out: bool,
    pub failed_step: Option<&'a FailedStepContext>,
    pub crash_signal: Option<&'a DriverCrashSignal>,
    pub raw_artifact_path: Option<String>,
}

/// Conservative advisory failure classifier implementing `DEJA-52` precedence.
#[derive(Debug, Default, Clone, Copy)]
pub struct FailureClassifier;

impl FailureClassifier {
    /// Classifies a completed flow run according to exact `DEJA-52` precedence:
    ///
    /// 1. `passed` -> `None`
    /// 2. timeout -> `FailureCategory::Timeout` (`repair_eligible: false`)
    /// 3. cancelled / unsupported / transport / parse / limit -> `FailureCategory::Runner`
    /// 4. crash signal -> `FailureCategory::AppCrash` (`repair_eligible: false`)
    /// 5. `assertVisible` (text OR ID-only) & `assertNotVisible` -> `FailureCategory::ProductAssertion` (`repair_eligible: false`)
    /// 6. `extendedWaitUntil` -> `FailureCategory::Timeout` (`repair_eligible: false`)
    /// 7. action selector miss (`tapOn`/`longPressOn`/`doubleTapOn`/`scrollUntilVisible`/`swipe`) -> `FailureCategory::Selector` (`repair_eligible: true`)
    /// 8. `launchApp` / `openLink` navigation/launch failure -> `FailureCategory::Navigation` (`repair_eligible: true`)
    /// 9. anything else -> `FailureCategory::Unknown` (`repair_eligible: false`)
    ///
    /// Always sets `advisory: true`.
    #[must_use]
    pub fn classify(input: &ClassificationInput<'_>) -> Option<WalkFailure> {
        if input.passed {
            return None;
        }

        let failing_step = input.failed_step.map(|s| s.label.clone());
        let raw_artifact_path = input.raw_artifact_path.clone();
        let kind = input.failed_step.map(|s| s.kind);
        let crash = input
            .crash_signal
            .or_else(|| input.failed_step.and_then(|s| s.crash_signal.as_ref()));

        // 2. Overall run timeout or explicit timeout step failure.
        if input.timed_out || matches!(kind, Some(StepFailureKind::Timeout)) {
            return Some(build_failure(
                FailureCategory::Timeout,
                failing_step,
                raw_artifact_path,
                None,
            ));
        }

        // 3. Runner/infrastructure/parse/cancellation errors.
        if matches!(
            kind,
            Some(
                StepFailureKind::Cancelled
                    | StepFailureKind::UnsupportedCapability
                    | StepFailureKind::Transport
                    | StepFailureKind::ParseOrConfinement
                    | StepFailureKind::LimitExceeded
            )
        ) {
            return Some(build_failure(
                FailureCategory::Runner,
                failing_step,
                raw_artifact_path,
                None,
            ));
        }

        // 4. Explicit app crash signal beats assertion, selector, and navigation symptoms.
        if let Some(signal) = crash {
            return Some(build_failure(
                FailureCategory::AppCrash,
                failing_step,
                raw_artifact_path,
                Some(signal.to_core()),
            ));
        }
        if matches!(kind, Some(StepFailureKind::AppCrash)) {
            return Some(build_failure(
                FailureCategory::AppCrash,
                failing_step,
                raw_artifact_path,
                None,
            ));
        }

        // 5-8. Structural step-based classification.
        if let Some(step) = input.failed_step {
            let category = match (step.command.as_str(), step.kind) {
                (
                    "assertVisible" | "assertNotVisible",
                    StepFailureKind::AssertionFailed | StepFailureKind::SelectorNotFound,
                ) => FailureCategory::ProductAssertion,
                (
                    "extendedWaitUntil",
                    StepFailureKind::AssertionFailed
                    | StepFailureKind::SelectorNotFound
                    | StepFailureKind::Timeout,
                ) => FailureCategory::Timeout,
                (
                    "tapOn" | "longPressOn" | "doubleTapOn" | "scrollUntilVisible" | "swipe",
                    StepFailureKind::SelectorNotFound,
                ) => FailureCategory::Selector,
                ("launchApp" | "openLink", StepFailureKind::NavigationFailed) => {
                    FailureCategory::Navigation
                }
                _ => FailureCategory::Unknown,
            };
            return Some(build_failure(
                category,
                failing_step,
                raw_artifact_path,
                None,
            ));
        }

        // 9. Conservative fallback.
        Some(build_failure(
            FailureCategory::Unknown,
            failing_step,
            raw_artifact_path,
            None,
        ))
    }
}

fn build_failure(
    category: FailureCategory,
    failing_step: Option<String>,
    raw_artifact_path: Option<String>,
    crash: Option<deja_core::CrashSignal>,
) -> WalkFailure {
    let repair_eligible = matches!(
        category,
        FailureCategory::Selector | FailureCategory::Navigation
    );
    WalkFailure {
        category,
        advisory: true,
        repair_eligible,
        failing_step,
        raw_artifact_path,
        crash,
    }
}

/// Counts `assert*` commands in a single YAML flow using `deja_core::find_flow_assertions`.
pub fn count_assertions_in_flow_yaml(yaml: &str) -> Result<u64, CoreError> {
    let entries = find_flow_assertions(yaml)?;
    Ok(entries
        .iter()
        .filter(|entry| entry.command.starts_with("assert"))
        .count() as u64)
}

/// Counts `assert*` commands across a full [`FlowClosure`] using `deja_core::closure_assertions`.
pub fn count_assertions_in_closure(closure: &FlowClosure) -> Result<u64, CoreError> {
    let entries = closure_assertions(closure)?;
    Ok(entries
        .iter()
        .filter(|entry| entry.command.starts_with("assert"))
        .count() as u64)
}
