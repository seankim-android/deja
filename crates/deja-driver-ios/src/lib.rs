//! Native iOS simulator driver (`xcrun simctl` + `DejaXCTestRunner` + `.ips` crash detection).

pub mod cache;
pub mod client;
pub mod crash;
pub mod driver;
pub mod embed;
pub mod error;
pub mod process;
pub mod runner;
pub mod simctl;

pub use cache::{RunnerBuildArtifact, ToolchainInfo, ensure_runner_built, resolve_deja_home};
pub use client::{
    AGENT_TOKEN_HEADER, ActKind, ActRequest, ActResponse, ActionResponse, AgentClient,
    AppStateResponse, ElementFrame, HealthResponse, HierarchyNode, HierarchyResponse, KeyRequest,
    SwipeRequest, TapRequest, TextMatchMode, TypeRequest, WaitForSelectorRequest,
    WaitForSelectorResponse, WarmupRequest, WarmupResponse, compute_hierarchy_hash,
};
pub use crash::{
    CrashSearchOptions, MAX_CRASH_REPORT_BYTES, default_crash_report_dir, find_crash_report,
    find_crash_report_with_grace,
};
pub use driver::{
    IosDriver, IosDriverConfig, format_launch_argument_value, hierarchy_node_to_ui_node,
    hierarchy_response_to_ui,
};
pub use error::{IosDriverError, Result};
pub use process::{
    ManagedXcodebuild, RUNNER_BUNDLE_ID, RUNNER_HOST_BUNDLE_ID, RUNNER_XCTRUNNER_BUNDLE_ID,
    SimulatorLockGuard, acquire_simulator_lock, acquire_simulator_lock_in_dir,
    find_xcodebuild_test_pids_for_session, find_xcodebuild_test_pids_for_udid,
    resolve_simulator_locks_dir, send_sigterm_to_pid, sweep_xcodebuild_for_session, validate_udid,
};
pub use runner::{IosRunnerConfig, IosRunnerSession};
pub use simctl::{
    SimDevice, SimRuntime, Simctl, SimctlEnv, clear_app_data_container,
    clear_app_data_container_cancellable, clear_app_group_container_cancellable,
    find_device_runtime_version, map_privacy_action, map_privacy_service,
    normalize_ios_runtime_identifier, normalize_runtime_identifier, parse_app_group_containers,
    validate_env_key, validate_launch_arg_key, validate_launch_inputs, validate_media_paths,
    validate_open_link_url,
};

/// Canonical crate identifier used by workspace smoke checks.
pub const CRATE_NAME: &str = "deja-driver-ios";

/// Direct upstream workspace crates linked by `deja-driver-ios`.
pub const UPSTREAM_CRATES: &[&str] = &[deja_core::CRATE_NAME, deja_flow::CRATE_NAME];

#[cfg(test)]
mod tests {
    use super::{CRATE_NAME, UPSTREAM_CRATES};

    #[test]
    fn crate_identity_matches() {
        assert_eq!(CRATE_NAME, "deja-driver-ios");
        assert_eq!(UPSTREAM_CRATES, &["deja-core", "deja-flow"]);
    }
}
