//! Capability-scoped `deja.config.json` loading, validation, and legacy migration checks.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use deja_core::{StaleBuildPolicy, validate_forbidden_values};
use deja_driver_ios::validate_udid;
use deja_store::{check_flows_dir, check_manifest_path};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fs_safe::{lexical_normalize, read_to_string_nofollow_bounded};

/// Maximum `deja.config.json` file size (1 MiB).
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Default command timeout in milliseconds (3 minutes).
pub const DEFAULT_TIMEOUT_MS: u64 = 180_000;

/// Default iOS `.ips` crash report grace period in milliseconds.
pub const DEFAULT_CRASH_REPORT_GRACE_MS: u64 = 10_000;

/// Maximum iOS `.ips` crash report grace period in milliseconds.
pub const MAX_CRASH_REPORT_GRACE_MS: u64 = 120_000;

/// Default `flowsDir` when `manifest` is configured.
pub const DEFAULT_FLOWS_DIR: &str = ".maestro/flows/deja";

/// Default config filename relative to cwd.
pub const DEFAULT_CONFIG_PATH: &str = "./deja.config.json";

/// Capability tier controlling which config fields are required (`DEJA-47`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandCapability {
    /// Does not read `deja.config.json` (`guide`, `engine install`).
    Standalone,
    /// Read-only diagnostics (`doctor`): never requires `buildPath` on disk or `device`.
    Doctor,
    /// Local read-only/static commands (`list`, `inspect`, `manifest check`, `migrate`): never requires `buildPath` or `device`.
    Static,
    /// Release gate (`gate`): requires build path (via `--build` or `config.buildPath`), never requires `device`.
    Gate,
    /// Live device execution (`run`, `remember`): requires `buildPath` on disk and resolved `device`.
    Execute,
}

/// Deterministic fixture credential entry configured in `deja.config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FixtureCredential {
    pub label: String,
    pub value: String,
}

/// Semantic retrieval configuration section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticConfig {
    pub provider: Option<String>,
    pub minimum_similarity: Option<f64>,
    pub minimum_margin: Option<f64>,
}

/// MCP server configuration section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpConfig {
    #[serde(default)]
    pub explore: bool,
}

/// Loaded, validated Déjà configuration.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// Original config path as passed (or `./deja.config.json`).
    pub raw_config_arg: Option<String>,
    /// Resolved path of the config file.
    pub config_path: PathBuf,
    /// Parent directory of the config file.
    pub config_dir: PathBuf,
    /// Canonicalized workspace root directory.
    pub workspace_root: PathBuf,
    /// Platform (`"ios"`, `"android"`, or `"web"`).
    pub platform: String,
    /// Application bundle ID / package ID / loopback URL.
    pub app_id: String,
    /// Optional execution adapter identifier (e.g. `"maestro-ios/v1"`).
    pub adapter: Option<String>,
    /// Optional browser identifier (required on `"web"`).
    pub browser: Option<String>,
    /// Optional resolved build path (`.app`, `.apk`, static dir).
    pub build_path: Option<PathBuf>,
    /// Resolved target device (`--device` > `DEJA_DEVICE` > `config.device`).
    pub device: Option<String>,
    /// Default flow execution timeout in milliseconds.
    pub timeout_ms: u64,
    /// Independent verification timeout in milliseconds.
    pub verification_timeout_ms: u64,
    /// Configured default retry count after initial failure.
    pub retries: u8,
    /// iOS `.ips` crash report grace period in milliseconds (`0..=120_000`).
    pub crash_report_grace_ms: u64,
    /// Optional override directory for iOS `.ips` crash reports (test/host seam).
    pub crash_report_directory: Option<PathBuf>,
    /// Whether simulator animations/Reduce Motion are disabled on attach (default `true`).
    pub disable_animations: bool,
    /// Static flow environment variables configured in `deja.config.json`.
    pub env: BTreeMap<String, String>,
    /// Configured deterministic fixture credentials.
    pub fixture_credentials: Vec<FixtureCredential>,
    /// Combined forbidden/redacted secret values (`fixtureCredentials` + `secrets` + `redact`).
    pub forbidden_values: Vec<String>,
    /// Optional workspace-relative manifest path (`deja.walks.json`).
    pub manifest: Option<String>,
    /// Optional workspace-relative consumer flows directory (when `manifest` is configured).
    pub flows_dir: Option<String>,
    /// Optional resolved JUnit output path.
    pub junit: Option<PathBuf>,
    /// Optional iOS-only stale build replay policy.
    pub stale_build: Option<StaleBuildPolicy>,
    /// Optional headless flag (web).
    pub headless: Option<bool>,
    /// Optional screen size setting (web).
    pub screen_size: Option<Value>,
    /// Optional semantic lookup settings.
    pub semantic: Option<SemanticConfig>,
    /// Optional MCP settings.
    pub mcp: Option<McpConfig>,
    /// Non-fatal deprecation warnings to emit to stderr.
    pub warnings: Vec<String>,
}

/// Configuration loading/validation error (always maps to CLI exit `2`).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

/// Options passed into [`load_config`].
#[derive(Debug, Clone)]
pub struct LoadConfigOptions<'a> {
    pub cwd: &'a Path,
    pub explicit_config: Option<&'a Path>,
    pub capability: CommandCapability,
    pub device_override: Option<&'a str>,
    pub build_override: Option<&'a Path>,
    pub env: &'a BTreeMap<String, String>,
}

/// Load and validate `deja.config.json` according to `opts.capability` (`DEJA-47`, `DEJA-48`).
pub fn load_config(opts: &LoadConfigOptions<'_>) -> Result<LoadedConfig, ConfigError> {
    // Reject retired `DEJA_MAESTRO` environment override everywhere a config is loaded.
    if opts.env.get("DEJA_MAESTRO").is_some_and(|v| !v.is_empty()) {
        return Err(ConfigError(
            "maestro.executable / DEJA_MAESTRO is no longer supported: Déjà executes maestro/v1 flows natively in Rust; unset DEJA_MAESTRO and remove maestro.executable from deja.config.json"
                .to_string(),
        ));
    }

    let raw_config_arg = opts
        .explicit_config
        .map(|p| p.to_string_lossy().into_owned());
    let unresolved = opts
        .explicit_config
        .unwrap_or_else(|| Path::new(DEFAULT_CONFIG_PATH));
    let config_path = if unresolved.is_absolute() {
        lexical_normalize(unresolved)
    } else {
        lexical_normalize(&opts.cwd.join(unresolved))
    };

    let contents =
        read_to_string_nofollow_bounded(&config_path, MAX_CONFIG_BYTES).map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                ConfigError(format!(
                    "configuration file not found: {}",
                    unresolved.display()
                ))
            } else {
                ConfigError(format!(
                    "cannot read configuration file {}: {err}",
                    unresolved.display()
                ))
            }
        })?;

    let root_val: Value = serde_json::from_str(&contents)
        .map_err(|err| ConfigError(format!("invalid JSON in configuration file: {err}")))?;

    if let Some(dup) = find_duplicate_json_key(&contents) {
        return Err(ConfigError(format!(
            "configuration file contains duplicate JSON key {dup:?}"
        )));
    }

    let Some(obj) = root_val.as_object() else {
        return Err(ConfigError(
            "configuration root must be a JSON object".to_string(),
        ));
    };

    let mut warnings = Vec::new();

    // Reject retired `openClaw` config section.
    if let Some(val) = obj.get("openClaw")
        && !val.is_null()
    {
        return Err(ConfigError(
            "openClaw config section is no longer supported: Déjà executes flows natively without OpenClaw; remove openClaw from deja.config.json"
                .to_string(),
        ));
    }

    let config_dir = config_path.parent().unwrap_or(opts.cwd).to_path_buf();

    // `workspaceRoot` (default ".") resolved relative to config file parent.
    let ws_raw = match obj.get("workspaceRoot") {
        None | Some(Value::Null) => ".".to_string(),
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        Some(_) => {
            return Err(ConfigError(
                "`workspaceRoot` must be a non-empty string".to_string(),
            ));
        }
    };
    let ws_joined = if Path::new(&ws_raw).is_absolute() {
        PathBuf::from(&ws_raw)
    } else {
        config_dir.join(&ws_raw)
    };
    let workspace_root = fs::canonicalize(&ws_joined).map_err(|err| {
        ConfigError(format!(
            "`workspaceRoot` ({ws_raw}) cannot be resolved: {err}"
        ))
    })?;
    if !workspace_root.is_dir() {
        return Err(ConfigError(format!(
            "`workspaceRoot` ({ws_raw}) is not a directory"
        )));
    }

    // `platform`
    let platform = req_non_empty_string(obj, "platform")?;
    if !matches!(platform.as_str(), "ios" | "android" | "web") {
        return Err(ConfigError(format!(
            "unsupported `platform` {platform:?}: expected \"ios\", \"android\", or \"web\""
        )));
    }

    // `appId`
    let app_id = req_non_empty_string(obj, "appId")?;
    if platform == "web" && !is_loopback_url(&app_id) {
        return Err(ConfigError(format!(
            "web `appId` must be a loopback http:// or https:// URL (got {app_id:?})"
        )));
    }

    // `adapter` and `browser`
    let adapter = opt_non_empty_string(obj, "adapter")?;
    let browser = opt_non_empty_string(obj, "browser")?;
    if platform == "web" {
        if adapter.is_none() {
            return Err(ConfigError(
                "web configuration requires `adapter`".to_string(),
            ));
        }
        if browser.as_deref() != Some("chromium") {
            return Err(ConfigError(
                "web configuration requires `browser: \"chromium\"`".to_string(),
            ));
        }
    }

    // `manifest` & `flowsDir`
    let manifest = match opt_non_empty_string(obj, "manifest")? {
        Some(path) => Some(
            check_manifest_path(&path)
                .map_err(|err| ConfigError(format!("invalid `manifest`: {err}")))?,
        ),
        None => None,
    };
    let flows_dir = match opt_non_empty_string(obj, "flowsDir")? {
        Some(dir) => {
            if manifest.is_none() {
                return Err(ConfigError(
                    "`flowsDir` is only allowed when `manifest` is configured".to_string(),
                ));
            }
            Some(
                check_flows_dir(&dir)
                    .map_err(|err| ConfigError(format!("invalid `flowsDir`: {err}")))?,
            )
        }
        None => manifest.as_ref().map(|_| DEFAULT_FLOWS_DIR.to_string()),
    };

    // `repair` refusal rules
    if let Some(repair_val) = obj.get("repair")
        && !repair_val.is_null()
    {
        if manifest.is_some() {
            return Err(ConfigError(
                "`repair` cannot be used together with `manifest`".to_string(),
            ));
        }
        if matches!(platform.as_str(), "ios" | "web") {
            return Err(ConfigError(format!(
                "`repair` is not supported on platform {platform:?}"
            )));
        }
    }

    // `staleBuild` (iOS-only `"replay-last-verified"`)
    let stale_build = match obj.get("staleBuild") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s == "replay-last-verified" => {
            if platform != "ios" {
                return Err(ConfigError(
                    "`staleBuild: \"replay-last-verified\"` is only supported on platform \"ios\""
                        .to_string(),
                ));
            }
            Some(StaleBuildPolicy::ReplayLastVerified)
        }
        Some(_) => {
            return Err(ConfigError(
                "`staleBuild` must be \"replay-last-verified\" (and is iOS-only)".to_string(),
            ));
        }
    };

    // `maestro` legacy block handling
    let mut legacy_maestro_timeout: Option<u64> = None;
    if let Some(maestro_val) = obj.get("maestro")
        && !maestro_val.is_null()
    {
        let Some(m_obj) = maestro_val.as_object() else {
            return Err(ConfigError(
                "`maestro` must be a JSON object when present".to_string(),
            ));
        };
        if let Some(exec_val) = m_obj.get("executable")
            && !exec_val.is_null()
        {
            return Err(ConfigError(
                "maestro.executable / DEJA_MAESTRO is no longer supported: Déjà executes maestro/v1 flows natively in Rust; remove maestro.executable from deja.config.json"
                    .to_string(),
            ));
        }
        if let Some(t_val) = m_obj.get("timeoutMs")
            && !t_val.is_null()
        {
            legacy_maestro_timeout = Some(parse_positive_u64(t_val, "maestro.timeoutMs")?);
            warnings.push(
                "deja: warning: \"maestro.timeoutMs\" is deprecated; move it to top-level \"timeoutMs\""
                    .to_string(),
            );
        }
        if let Some(allow_val) = m_obj.get("allowUntested")
            && !allow_val.is_null()
        {
            warnings.push(
                "deja: warning: \"maestro.allowUntested\" is deprecated; Déjà executes flows natively without Maestro"
                    .to_string(),
            );
        }
    }

    // `timeoutMs` resolution: top-level `timeoutMs` > `maestro.timeoutMs` > `DEJA_TIMEOUT_MS` > default.
    let top_timeout = match obj.get("timeoutMs") {
        None | Some(Value::Null) => None,
        Some(val) => Some(parse_positive_u64(val, "timeoutMs")?),
    };
    let env_timeout = match opts.env.get("DEJA_TIMEOUT_MS") {
        None => None,
        Some(raw) if raw.trim().is_empty() => {
            return Err(ConfigError(
                "DEJA_TIMEOUT_MS must be a positive integer when set".to_string(),
            ));
        }
        Some(raw) => {
            let parsed = raw.trim().parse::<u64>().map_err(|_| {
                ConfigError(format!(
                    "DEJA_TIMEOUT_MS must be a positive integer (got {raw:?})"
                ))
            })?;
            if parsed == 0 {
                return Err(ConfigError(
                    "DEJA_TIMEOUT_MS must be greater than 0".to_string(),
                ));
            }
            Some(parsed)
        }
    };
    let timeout_ms = top_timeout
        .or(legacy_maestro_timeout)
        .or(env_timeout)
        .unwrap_or(DEFAULT_TIMEOUT_MS);

    let verification_timeout_ms = match obj.get("verificationTimeoutMs") {
        None | Some(Value::Null) => timeout_ms,
        Some(val) => parse_positive_u64(val, "verificationTimeoutMs")?,
    };

    // `retries`
    let retries = match obj.get("retries") {
        None | Some(Value::Null) => 0u8,
        Some(val) => {
            let n = parse_non_negative_u64(val, "retries")?;
            if n > 10 {
                return Err(ConfigError(
                    "`retries` must be an integer between 0 and 10".to_string(),
                ));
            }
            n as u8
        }
    };

    // `crashReportGraceMs` (iOS only, 0..=120_000, default 10_000)
    let crash_report_grace_ms = match obj.get("crashReportGraceMs") {
        None | Some(Value::Null) => {
            if platform == "ios" {
                DEFAULT_CRASH_REPORT_GRACE_MS
            } else {
                0
            }
        }
        Some(val) => {
            if platform != "ios" {
                return Err(ConfigError(
                    "`crashReportGraceMs` is only supported on platform \"ios\"".to_string(),
                ));
            }
            let ms = parse_non_negative_u64(val, "crashReportGraceMs")?;
            if ms > MAX_CRASH_REPORT_GRACE_MS {
                return Err(ConfigError(format!(
                    "`crashReportGraceMs` must be between 0 and {MAX_CRASH_REPORT_GRACE_MS}"
                )));
            }
            ms
        }
    };

    // `crashReportDirectory` (iOS only seam)
    let crash_report_directory = match opt_non_empty_string(obj, "crashReportDirectory")? {
        None => None,
        Some(raw) => {
            if platform != "ios" {
                return Err(ConfigError(
                    "`crashReportDirectory` is only supported on platform \"ios\"".to_string(),
                ));
            }
            let p = Path::new(&raw);
            Some(if p.is_absolute() {
                p.to_path_buf()
            } else {
                lexical_normalize(&config_dir.join(p))
            })
        }
    };

    // `disableAnimations` (default true)
    let disable_animations = match obj.get("disableAnimations") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Err(ConfigError(
                "`disableAnimations` must be a boolean".to_string(),
            ));
        }
    };

    // `env`
    let mut flow_env = BTreeMap::new();
    if let Some(env_val) = obj.get("env")
        && !env_val.is_null()
    {
        let Some(env_obj) = env_val.as_object() else {
            return Err(ConfigError("`env` must be a JSON object".to_string()));
        };
        for (k, v) in env_obj {
            let Some(s) = v.as_str() else {
                return Err(ConfigError(format!("`env.{k}` must be a string")));
            };
            flow_env.insert(k.clone(), s.to_string());
        }
    }

    // `fixtureCredentials`, `secrets`, `redact`
    let mut fixture_credentials = Vec::new();
    let mut forbidden_values = Vec::new();
    let mut seen_forbidden = HashSet::new();

    if let Some(fc_val) = obj.get("fixtureCredentials")
        && !fc_val.is_null()
    {
        let Some(arr) = fc_val.as_array() else {
            return Err(ConfigError(
                "`fixtureCredentials` must be an array".to_string(),
            ));
        };
        for item in arr {
            let Some(item_obj) = item.as_object() else {
                return Err(ConfigError(
                    "each `fixtureCredentials` entry must be an object with `label` and `value`"
                        .to_string(),
                ));
            };
            let label = req_non_empty_string(item_obj, "label")?;
            let value = match item_obj.get("value").and_then(Value::as_str) {
                Some(v) => v.to_string(),
                None => {
                    return Err(ConfigError(
                        "each `fixtureCredentials` entry requires a string `value`".to_string(),
                    ));
                }
            };
            if seen_forbidden.insert(value.clone()) {
                forbidden_values.push(value.clone());
            }
            fixture_credentials.push(FixtureCredential { label, value });
        }
    }

    for field in ["secrets", "redact"] {
        if let Some(val) = obj.get(field)
            && !val.is_null()
        {
            let Some(arr) = val.as_array() else {
                return Err(ConfigError(format!(
                    "`{field}` must be an array of strings"
                )));
            };
            for item in arr {
                let Some(s) = item.as_str() else {
                    return Err(ConfigError(format!(
                        "each `{field}` entry must be a string"
                    )));
                };
                if seen_forbidden.insert(s.to_string()) {
                    forbidden_values.push(s.to_string());
                }
            }
        }
    }

    validate_forbidden_values(&forbidden_values).map_err(|err| ConfigError(err.to_string()))?;

    // `junit`
    let junit = opt_non_empty_string(obj, "junit")?.map(|raw| {
        let p = Path::new(&raw);
        if p.is_absolute() {
            lexical_normalize(p)
        } else {
            lexical_normalize(&config_dir.join(p))
        }
    });

    // `headless`, `screenSize`, `semantic`, `mcp`
    let headless = match obj.get("headless") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err(ConfigError("`headless` must be a boolean".to_string())),
    };
    let screen_size = obj.get("screenSize").filter(|v| !v.is_null()).cloned();
    let semantic = match obj.get("semantic") {
        None | Some(Value::Null) => None,
        Some(val) => Some(
            serde_json::from_value::<SemanticConfig>(val.clone())
                .map_err(|err| ConfigError(format!("invalid `semantic` config: {err}")))?,
        ),
    };
    let mcp = match obj.get("mcp") {
        None | Some(Value::Null) => None,
        Some(val) => Some(
            serde_json::from_value::<McpConfig>(val.clone())
                .map_err(|err| ConfigError(format!("invalid `mcp` config: {err}")))?,
        ),
    };

    // `buildPath` (capability-scoped!)
    let config_build_raw = match obj.get("buildPath") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        Some(_) => {
            return Err(ConfigError(
                "`buildPath` must be a non-empty string when present".to_string(),
            ));
        }
    };
    let build_path = if let Some(override_path) = opts.build_override {
        Some(if override_path.is_absolute() {
            lexical_normalize(override_path)
        } else {
            lexical_normalize(&opts.cwd.join(override_path))
        })
    } else {
        config_build_raw.map(|raw| {
            let p = Path::new(&raw);
            if p.is_absolute() {
                lexical_normalize(p)
            } else {
                lexical_normalize(&config_dir.join(p))
            }
        })
    };

    // `device` precedence (`DEJA-48`): `--device` > `DEJA_DEVICE` > `config.device`
    let config_device = match obj.get("device") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            if s.trim().is_empty() {
                return Err(ConfigError(
                    "`device` in configuration must not be empty or whitespace".to_string(),
                ));
            }
            Some(s.trim().to_string())
        }
        Some(_) => {
            return Err(ConfigError(
                "`device` must be a string when present".to_string(),
            ));
        }
    };
    let device = resolve_device(opts.device_override, opts.env, config_device.as_deref())?;

    // Capability-scoped requirements (`DEJA-47`):
    match opts.capability {
        CommandCapability::Standalone | CommandCapability::Doctor | CommandCapability::Static => {
            // NEVER require `buildPath` on disk or `device`!
            if opts.capability == CommandCapability::Doctor
                && platform == "ios"
                && let Some(ref udid) = device
            {
                let _ = validate_udid(udid).map_err(|err| {
                    ConfigError(format!("invalid iOS simulator UDID {udid:?}: {err}"))
                })?;
            }
        }
        CommandCapability::Gate => {
            // Requires a build path (either `--build` or `config.buildPath`); never requires `device`.
            if build_path.is_none() {
                return Err(ConfigError(
                    "`deja gate` requires `--build <path>` or `buildPath` in deja.config.json"
                        .to_string(),
                ));
            }
        }
        CommandCapability::Execute => {
            let Some(ref bp) = build_path else {
                return Err(ConfigError(
                    "`buildPath` is required in deja.config.json for `run` and `remember`"
                        .to_string(),
                ));
            };
            if !bp.exists() {
                return Err(ConfigError(format!(
                    "`buildPath` does not exist on disk: {}",
                    bp.display()
                )));
            }
            let Some(ref dev) = device else {
                return Err(ConfigError(
                    "a target device is required (pass `--device <id>`, set `DEJA_DEVICE`, or configure `device` in deja.config.json)"
                        .to_string(),
                ));
            };
            if platform == "ios" {
                let _ = validate_udid(dev).map_err(|err| {
                    ConfigError(format!(
                        "iOS `device` must be an explicit simulator UDID (got {dev:?}): {err}"
                    ))
                })?;
            }
        }
    }

    // In manifest mode, `ConsumerFlows` requires `adapter` in `WalkIdentity`.
    if manifest.is_some() && adapter.is_none() {
        return Err(ConfigError(
            "`adapter` is required in deja.config.json when `manifest` is configured".to_string(),
        ));
    }

    Ok(LoadedConfig {
        raw_config_arg,
        config_path,
        config_dir,
        workspace_root,
        platform,
        app_id,
        adapter,
        browser,
        build_path,
        device,
        timeout_ms,
        verification_timeout_ms,
        retries,
        crash_report_grace_ms,
        crash_report_directory,
        disable_animations,
        env: flow_env,
        fixture_credentials,
        forbidden_values,
        manifest,
        flows_dir,
        junit,
        stale_build,
        headless,
        screen_size,
        semantic,
        mcp,
        warnings,
    })
}

/// Resolve device with strict precedence `--device` > `DEJA_DEVICE` > `config.device` (`DEJA-48`).
/// Any empty or whitespace-only explicit override fails closed.
pub fn resolve_device(
    cli_override: Option<&str>,
    env: &BTreeMap<String, String>,
    config_device: Option<&str>,
) -> Result<Option<String>, ConfigError> {
    if let Some(raw) = cli_override {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ConfigError(
                "`--device` must not be empty or whitespace".to_string(),
            ));
        }
        return Ok(Some(trimmed.to_string()));
    }
    if let Some(raw) = env.get("DEJA_DEVICE") {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ConfigError(
                "`DEJA_DEVICE` must not be empty or whitespace when set".to_string(),
            ));
        }
        return Ok(Some(trimmed.to_string()));
    }
    Ok(config_device.map(ToString::to_string))
}

fn req_non_empty_string(
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String, ConfigError> {
    match obj.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
        _ => Err(ConfigError(format!(
            "`{key}` is required and must be a non-empty string"
        ))),
    }
}

fn opt_non_empty_string(
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, ConfigError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.clone())),
        Some(_) => Err(ConfigError(format!(
            "`{key}` must be a non-empty string when present"
        ))),
    }
}

fn parse_non_negative_u64(val: &Value, field: &str) -> Result<u64, ConfigError> {
    if let Some(u) = val.as_u64() {
        return Ok(u);
    }
    if let Some(f) = val.as_f64()
        && f.is_finite()
        && f >= 0.0
        && f.fract() == 0.0
        && f <= 9_007_199_254_740_991.0
    {
        return Ok(f as u64);
    }
    Err(ConfigError(format!(
        "`{field}` must be a non-negative integer"
    )))
}

fn parse_positive_u64(val: &Value, field: &str) -> Result<u64, ConfigError> {
    let n = parse_non_negative_u64(val, field)?;
    if n == 0 {
        return Err(ConfigError(format!("`{field}` must be greater than 0")));
    }
    Ok(n)
}

fn is_loopback_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"));
    let Some(authority_and_path) = rest else {
        return false;
    };
    let authority = authority_and_path
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// Detect duplicate keys inside any JSON object (assumes syntactically valid JSON).
fn find_duplicate_json_key(text: &str) -> Option<String> {
    struct Scanner<'a> {
        src: &'a str,
        bytes: &'a [u8],
        pos: usize,
    }
    impl<'a> Scanner<'a> {
        fn skip_ws(&mut self) {
            while self.pos < self.bytes.len()
                && matches!(self.bytes[self.pos], b' ' | b'\t' | b'\r' | b'\n')
            {
                self.pos += 1;
            }
        }
        fn scan_string(&mut self) -> Option<String> {
            let start = self.pos;
            if self.pos >= self.bytes.len() || self.bytes[self.pos] != b'"' {
                return None;
            }
            self.pos += 1;
            while self.pos < self.bytes.len() && self.bytes[self.pos] != b'"' {
                if self.bytes[self.pos] == b'\\' {
                    self.pos += 2;
                } else {
                    self.pos += 1;
                }
            }
            if self.pos < self.bytes.len() {
                self.pos += 1;
            }
            serde_json::from_str::<String>(&self.src[start..self.pos]).ok()
        }
        fn scan_value(&mut self) -> Option<String> {
            self.skip_ws();
            let ch = *self.bytes.get(self.pos)?;
            match ch {
                b'{' => {
                    self.pos += 1;
                    let mut seen = HashSet::new();
                    self.skip_ws();
                    while self.pos < self.bytes.len() && self.bytes[self.pos] != b'}' {
                        let iter_start = self.pos;
                        self.skip_ws();
                        let key = self.scan_string()?;
                        if !seen.insert(key.clone()) {
                            return Some(key);
                        }
                        self.skip_ws();
                        if self.pos >= self.bytes.len() || self.bytes[self.pos] != b':' {
                            return None;
                        }
                        self.pos += 1;
                        if let Some(dup) = self.scan_value() {
                            return Some(dup);
                        }
                        self.skip_ws();
                        if self.pos < self.bytes.len() && self.bytes[self.pos] == b',' {
                            self.pos += 1;
                        } else if self.pos < self.bytes.len() && self.bytes[self.pos] != b'}' {
                            return None;
                        }
                        self.skip_ws();
                        if self.pos <= iter_start {
                            return None;
                        }
                    }
                    if self.pos < self.bytes.len() && self.bytes[self.pos] == b'}' {
                        self.pos += 1;
                    }
                    None
                }
                b'[' => {
                    self.pos += 1;
                    self.skip_ws();
                    while self.pos < self.bytes.len() && self.bytes[self.pos] != b']' {
                        let iter_start = self.pos;
                        if let Some(dup) = self.scan_value() {
                            return Some(dup);
                        }
                        self.skip_ws();
                        if self.pos < self.bytes.len() && self.bytes[self.pos] == b',' {
                            self.pos += 1;
                        } else if self.pos < self.bytes.len() && self.bytes[self.pos] != b']' {
                            return None;
                        }
                        self.skip_ws();
                        if self.pos <= iter_start {
                            return None;
                        }
                    }
                    if self.pos < self.bytes.len() && self.bytes[self.pos] == b']' {
                        self.pos += 1;
                    }
                    None
                }
                b'"' => {
                    let _ = self.scan_string()?;
                    None
                }
                _ => {
                    let start = self.pos;
                    while self.pos < self.bytes.len()
                        && !matches!(
                            self.bytes[self.pos],
                            b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n'
                        )
                    {
                        self.pos += 1;
                    }
                    if self.pos == start {
                        // Unexpected closing delimiter in malformed JSON: advance fail-closed.
                        self.pos += 1;
                    }
                    None
                }
            }
        }
    }
    let mut s = Scanner {
        src: text,
        bytes: text.as_bytes(),
        pos: 0,
    };
    s.scan_value()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_ws(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("deja-cfg-{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn capability_scoping_allows_missing_build_and_device_for_static_and_doctor() {
        let ws = temp_ws("cap");
        let cfg = ws.join("deja.config.json");
        fs::write(
            &cfg,
            r#"{"workspaceRoot":".","platform":"ios","appId":"dev.deja.app"}"#,
        )
        .unwrap();
        let empty = BTreeMap::new();

        for cap in [CommandCapability::Static, CommandCapability::Doctor] {
            let loaded = load_config(&LoadConfigOptions {
                cwd: &ws,
                explicit_config: Some(&cfg),
                capability: cap,
                device_override: None,
                build_override: None,
                env: &empty,
            })
            .expect("static/doctor should not require buildPath or device");
            assert_eq!(loaded.platform, "ios");
            assert!(loaded.build_path.is_none());
            assert!(loaded.device.is_none());
        }

        // Execute requires buildPath and device.
        let err = load_config(&LoadConfigOptions {
            cwd: &ws,
            explicit_config: Some(&cfg),
            capability: CommandCapability::Execute,
            device_override: None,
            build_override: None,
            env: &empty,
        })
        .expect_err("execute must require buildPath");
        assert!(err.to_string().contains("buildPath"));

        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn device_precedence_and_empty_override_fail_closed() {
        let mut env = BTreeMap::new();
        env.insert(
            "DEJA_DEVICE".to_string(),
            "22222222-2222-2222-2222-222222222222".to_string(),
        );
        assert_eq!(
            resolve_device(
                Some("11111111-1111-1111-1111-111111111111"),
                &env,
                Some("33333333-3333-3333-3333-333333333333"),
            )
            .unwrap()
            .as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );
        assert_eq!(
            resolve_device(None, &env, Some("33333333-3333-3333-3333-333333333333"))
                .unwrap()
                .as_deref(),
            Some("22222222-2222-2222-2222-222222222222")
        );
        assert!(resolve_device(Some("   "), &env, None).is_err());

        let mut empty_env = BTreeMap::new();
        empty_env.insert("DEJA_DEVICE".to_string(), "  ".to_string());
        assert!(
            resolve_device(
                None,
                &empty_env,
                Some("33333333-3333-3333-3333-333333333333")
            )
            .is_err()
        );
    }

    #[test]
    fn legacy_maestro_timeout_warns_and_executable_fails_closed() {
        let ws = temp_ws("legacy");
        let cfg = ws.join("deja.config.json");
        fs::write(
            &cfg,
            r#"{"workspaceRoot":".","platform":"ios","appId":"dev.deja.app","maestro":{"timeoutMs":45000}}"#,
        )
        .unwrap();
        let empty = BTreeMap::new();
        let loaded = load_config(&LoadConfigOptions {
            cwd: &ws,
            explicit_config: Some(&cfg),
            capability: CommandCapability::Static,
            device_override: None,
            build_override: None,
            env: &empty,
        })
        .unwrap();
        assert_eq!(loaded.timeout_ms, 45_000);
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("maestro.timeoutMs"))
        );

        fs::write(
            &cfg,
            r#"{"workspaceRoot":".","platform":"ios","appId":"dev.deja.app","maestro":{"executable":"/bin/maestro"}}"#,
        )
        .unwrap();
        let err = load_config(&LoadConfigOptions {
            cwd: &ws,
            explicit_config: Some(&cfg),
            capability: CommandCapability::Static,
            device_override: None,
            build_override: None,
            env: &empty,
        })
        .expect_err("maestro.executable must fail closed");
        assert!(err.to_string().contains("no longer supported"));

        let _ = fs::remove_dir_all(&ws);
    }
}
