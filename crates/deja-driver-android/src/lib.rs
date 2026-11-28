//! Native Android driver (`adb` + zero-Gradle on-device UiAutomator2 HTTP instrumentation APK).

pub mod adb;
pub mod apk;
pub mod cache;
pub mod client;
pub mod crash;
pub mod driver;
pub mod embed;
pub mod error;
pub mod process;

pub use adb::{
    Adb, AdbDevice, AdbForwardGuard, AnimationScaleGuard, DEFAULT_ADB_COMMAND_TIMEOUT,
    DEFAULT_ADB_TRANSFER_TIMEOUT, Wakefulness, discover_adb, is_wireless_or_tcp_serial,
    map_android_permission_action, map_android_permissions, parse_adb_devices_l,
    parse_android_runtime_sdk, parse_display_bounds, parse_keyguard_showing,
    parse_resolved_activity, parse_wakefulness, parse_wm_size, shell_quote, validate_extra_key,
    validate_media_paths, validate_open_link_url, validate_package_id,
};
pub use apk::{extract_apk_package_id, extract_apk_package_id_from_path};
pub use cache::{
    AgentApkReadiness, AndroidSdkBuildTools, ApkSource, ResolvedAgentApk, discover_sdk_build_tools,
    inspect_agent_apk_readiness, resolve_deja_home, resolve_or_build_agent_apk,
    verify_and_hash_apk,
};
pub use client::{
    AGENT_TOKEN_HEADER, ActKind, ActRequest, ActResponse, AgentClient, AppStateRequest,
    AppStateResponse, DisplayBounds, ElementFrame, HealthResponse, HierarchyNode, HierarchyRequest,
    HierarchyResponse, InFlightRpcGuard, TextMatchMode, WaitForSelectorRequest,
    WaitForSelectorResponse, compute_hierarchy_hash, token_digest,
};
pub use crash::{find_android_crash_with_grace, parse_crash_logcat};
pub use driver::{
    AndroidDriver, AndroidDriverConfig, hierarchy_node_to_ui_node, hierarchy_response_to_ui,
};
pub use error::{AndroidDriverError, Result};
pub use process::{
    AGENT_INSTRUMENTATION_COMPONENT, AGENT_PACKAGE_ID, DEFAULT_DEVICE_PORT, DeviceLockGuard,
    ManagedInstrumentation, READY_SENTINEL_PREFIX, ReadySentinel, acquire_device_lock,
    acquire_device_lock_in_dir, open_read_nofollow, parse_ready_sentinel_line,
    resolve_device_locks_dir, validate_serial,
};

/// Canonical crate identifier used by workspace smoke checks.
pub const CRATE_NAME: &str = "deja-driver-android";

/// Direct upstream workspace crates linked by `deja-driver-android`.
pub const UPSTREAM_CRATES: &[&str] = &[deja_core::CRATE_NAME, deja_flow::CRATE_NAME];

#[cfg(test)]
mod tests {
    use super::{CRATE_NAME, UPSTREAM_CRATES};

    #[test]
    fn crate_identity_matches() {
        assert_eq!(CRATE_NAME, "deja-driver-android");
        assert_eq!(UPSTREAM_CRATES, &["deja-core", "deja-flow"]);
    }
}
