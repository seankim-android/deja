//! Persistent warm driver daemon (`deja daemon`) and Unix-socket session pooling (`DEJA-78`).
//!
//! Eliminates repeated per-command driver startup (`DejaXCTestRunner` / headless Chromium cold
//! launch) across consecutive `deja run`, `deja remember`, and `deja mcp` invocations while
//! preserving strict per-device lease isolation, secret hygiene, and automatic idle TTL cleanup.

use std::collections::BTreeMap;

pub mod paths;
pub mod pool;
pub mod protocol;

#[cfg(unix)]
pub mod client;
#[cfg(unix)]
pub mod server;

/// Default daemon idle auto-shutdown timeout in seconds (5 minutes).
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;

/// Strict allowlist of toolchain environment variables forwarded over daemon IPC inside
/// [`protocol::WireDriverContext`]. Never forwards model API keys or fixture secrets.
pub const ALLOWED_DRIVER_ENV_KEYS: &[&str] = &[
    "PATH",
    "DEVELOPER_DIR",
    "DEJA_HOME",
    "HOME",
    "TMPDIR",
    "DEJA_CHROME",
    "CHROME_PATH",
    "ANDROID_HOME",
    "ANDROID_SDK_ROOT",
    "DEJA_ADB",
];

/// Safe environment variable allowlist inherited when auto-starting or spawning the background
/// daemon process (`deja daemon start --foreground`). Excludes `DEJA_NO_DAEMON`, model API keys,
/// and fixture secrets.
#[cfg(unix)]
pub const ALLOWED_DAEMON_SPAWN_ENV_KEYS: &[&str] = &[
    "PATH",
    "DEVELOPER_DIR",
    "DEJA_HOME",
    "DEJA_DAEMON_DIR",
    "DEJA_LOG",
    "RUST_LOG",
    "HOME",
    "TMPDIR",
    "DEJA_CHROME",
    "CHROME_PATH",
    "ANDROID_HOME",
    "ANDROID_SDK_ROOT",
    "DEJA_ADB",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
];

/// Returns `true` when `DEJA_NO_DAEMON` is set to a truthy value (`1`, `true`, `yes`,
/// case-insensitive).
#[must_use]
pub fn is_no_daemon_env(env: &BTreeMap<String, String>) -> bool {
    env.get("DEJA_NO_DAEMON").is_some_and(|raw| {
        let trimmed = raw.trim();
        trimmed == "1"
            || trimmed.eq_ignore_ascii_case("true")
            || trimmed.eq_ignore_ascii_case("yes")
    })
}

/// Filter an environment map down to [`ALLOWED_DRIVER_ENV_KEYS`] for safe daemon IPC transport.
#[must_use]
pub fn filter_driver_env(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut filtered = BTreeMap::new();
    for key in ALLOWED_DRIVER_ENV_KEYS {
        if let Some(val) = env.get(*key) {
            filtered.insert((*key).to_string(), val.clone());
        }
    }
    filtered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_daemon_env_recognizes_truthy_values_only() {
        for truthy in ["1", "true", "TRUE", "True", "yes", "YES", " 1 "] {
            let env = BTreeMap::from([("DEJA_NO_DAEMON".to_string(), truthy.to_string())]);
            assert!(is_no_daemon_env(&env), "expected truthy for {truthy:?}");
        }
        for falsy in ["", "0", "false", "no", "off", "2"] {
            let env = BTreeMap::from([("DEJA_NO_DAEMON".to_string(), falsy.to_string())]);
            assert!(!is_no_daemon_env(&env), "expected falsy for {falsy:?}");
        }
        assert!(!is_no_daemon_env(&BTreeMap::new()));
    }

    #[test]
    fn filter_driver_env_strips_api_keys_and_secrets() {
        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
        env.insert("DEJA_CHROME".to_string(), "/usr/bin/chromium".to_string());
        env.insert(
            "OPENAI_API_KEY".to_string(),
            "redacted-test-key".to_string(),
        );
        env.insert(
            "ANTHROPIC_API_KEY".to_string(),
            "redacted-anthropic".to_string(),
        );
        env.insert("FIXTURE_PASSWORD".to_string(), "super-secret".to_string());
        let filtered = filter_driver_env(&env);
        assert_eq!(filtered.len(), 2);
        assert_eq!(
            filtered.get("PATH").map(String::as_str),
            Some("/usr/bin:/bin")
        );
        assert_eq!(
            filtered.get("DEJA_CHROME").map(String::as_str),
            Some("/usr/bin/chromium")
        );
        assert!(!filtered.contains_key("OPENAI_API_KEY"));
        assert!(!filtered.contains_key("ANTHROPIC_API_KEY"));
        assert!(!filtered.contains_key("FIXTURE_PASSWORD"));
    }
}
