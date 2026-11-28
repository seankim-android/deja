//! Platform-neutral `maestro/v1` YAML interpreter, `PlatformDriver` abstraction,
//! selector matching, structured failure classification, adaptive polling/settle primitives,
//! driver capability contracts, latency telemetry, and JUnit XML writer.

pub mod backoff;
pub mod capabilities;
pub mod classifier;
pub mod driver;
pub mod executor;
pub mod junit;
pub mod parser;
pub mod selector;
pub mod timing;

/// Canonical crate identifier used by workspace smoke checks.
pub const CRATE_NAME: &str = "deja-flow";

/// Direct upstream workspace crates linked by `deja-flow`.
pub const UPSTREAM_CRATES: &[&str] = &[deja_core::CRATE_NAME];

pub use backoff::{
    HashableNode, PollSchedule, SettleOutcome, SettleTracker, hash_hierarchy_nodes,
    is_volatile_system_chrome,
};
pub use capabilities::{
    DriverCapabilities, DriverCapability, ObservationSource, UnsupportedCapability, VisibleContext,
};
pub use classifier::{
    ClassificationInput, FailedStepContext, FailureClassifier, StepFailureKind,
    count_assertions_in_closure, count_assertions_in_flow_yaml,
};
pub use driver::{
    ActAction, ActOutcome, ActPayload, DriverCrashSignal, DriverError, DriverFuture, KeyPress,
    LaunchOptions, OpenLinkOptions, PlatformDriver, SwipeParams, UiHierarchy, UiNode, UiPoint,
    UiRect, UiSize, WaitSelectorOutcome, WaitSelectorRequest,
};
pub use executor::{
    DEFAULT_ACTION_FAIL_FAST_SETTLE_POLLS, DEFAULT_HOLD_SCROLL_MS, EnvStack, FlowExecutionConfig,
    FlowExecutionError, FlowExecutionOutcome, FlowExecutor, MAX_FLOW_DEPTH, MAX_INTERPOLATED_BYTES,
    MAX_REPEAT_ITERATIONS, MAX_RETRIES,
};
pub use junit::{
    BatchJunitAttemptSummary, BatchJunitCase, BatchJunitCaseStatus, SynthesizedAttemptsJunitInput,
    SynthesizedJunitInput, atomic_write_junit, decode_xml, escape_cdata, escape_xml,
    merge_junit_suites, prepare_junit, redact_junit, redact_junit_with_redactors, reports_failure,
    strip_illegal_xml_chars, synthesized_attempts_junit, synthesized_batch_junit,
    synthesized_junit, write_errored_junit, write_redacted_junit,
};
pub use parser::{
    AddMediaStep, AppTargetStep, AssertNotVisibleStep, AssertVisibleStep, Condition, EraseTextStep,
    ExtendedWaitUntilStep, FlowHeader, FlowParseError, FlowStep, InputTextStep, LaunchAppStep,
    OpenLinkStep, ParsedFlow, PressKeyStep, RepeatStep, RetryStep, RunFlowStep, RunFlowTarget,
    ScrollDirection, ScrollUntilVisibleStep, SwipeStep, TakeScreenshotStep, TapStep,
    WaitForAnimationToEndStep, parse_flow, validate_flow_app_id_confinement,
};
pub use selector::{
    CompiledSelector, MAX_SELECTOR_DEPTH, MAX_SELECTOR_PATTERN_BYTES, PointCoord, PointSpec,
    REGEX_DFA_SIZE_LIMIT, REGEX_SIZE_LIMIT, Selector, SelectorError, build_visible_context,
    wait_local,
};
pub use timing::{LatencyStats, RunTimingBreakdown, StepTiming};

#[cfg(test)]
mod tests {
    use super::{CRATE_NAME, UPSTREAM_CRATES};

    #[test]
    fn crate_identity_matches() {
        assert_eq!(CRATE_NAME, "deja-flow");
        assert_eq!(UPSTREAM_CRATES, &["deja-core"]);
    }
}
