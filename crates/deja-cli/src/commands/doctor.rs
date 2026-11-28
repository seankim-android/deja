//! `deja doctor` — read-only native toolchain diagnostics (zero Java/JDK or Maestro checks).

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::CliDeps;
use crate::cli::DoctorArgs;
use crate::config::{
    CommandCapability, DEFAULT_CONFIG_PATH, LoadConfigOptions, load_config, resolve_device,
};
use crate::driver_factory::{DoctorCheck, DoctorCheckStatus, DoctorProbeRequest};
use crate::exit_codes::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE_OR_CONFIG};
use crate::output::RedactionContext;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorEngineSummary {
    version: String,
    native: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorReport {
    ready: bool,
    engine: DoctorEngineSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device: Option<String>,
    checks: Vec<DoctorCheck>,
}

pub async fn run(
    args: &DoctorArgs,
    explicit_config: Option<&Path>,
    json_mode: bool,
    deps: &CliDeps,
) -> u8 {
    let default_cfg_path = deps.cwd.join(DEFAULT_CONFIG_PATH);
    let should_load_config = explicit_config.is_some() || default_cfg_path.exists();

    let loaded_config = if should_load_config {
        match load_config(&LoadConfigOptions {
            cwd: &deps.cwd,
            explicit_config,
            capability: CommandCapability::Doctor,
            device_override: args.device.as_deref(),
            build_override: None,
            env: &deps.env,
        }) {
            Ok(cfg) => {
                for w in &cfg.warnings {
                    deps.output.writeln_stderr(w);
                }
                Some(cfg)
            }
            Err(err) => {
                let redaction =
                    RedactionContext::new(&deps.cwd, deps.home_dir.as_deref(), &[], &deps.env);
                let msg = redaction.redact_str(&err.to_string());
                if json_mode {
                    let payload = serde_json::json!({
                        "error": { "code": "CONFIG_ERROR", "message": msg }
                    });
                    deps.output.writeln_stdout(
                        &serde_json::to_string_pretty(&payload).expect("json serializes"),
                    );
                } else {
                    deps.output
                        .writeln_stderr(&format!("deja doctor error: {msg}"));
                }
                return EXIT_USAGE_OR_CONFIG;
            }
        }
    } else {
        None
    };

    let (workspace_root, platform, device, forbidden_values) = match &loaded_config {
        Some(cfg) => (
            cfg.workspace_root.clone(),
            Some(cfg.platform.clone()),
            cfg.device.clone(),
            cfg.forbidden_values.clone(),
        ),
        None => {
            let dev = match resolve_device(args.device.as_deref(), &deps.env, None) {
                Ok(d) => d,
                Err(err) => {
                    deps.output
                        .writeln_stderr(&format!("deja doctor error: {err}"));
                    return EXIT_USAGE_OR_CONFIG;
                }
            };
            (deps.cwd.clone(), None, dev, Vec::new())
        }
    };

    let redaction = RedactionContext::new(
        &workspace_root,
        deps.home_dir.as_deref(),
        &forbidden_values,
        &deps.env,
    );

    let req = DoctorProbeRequest {
        platform: platform.clone(),
        device: device.clone(),
        env: deps.env.clone(),
        deja_home: deps
            .env
            .get("DEJA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from),
    };
    let checks = deps.doctor_probes.probe(&req).await;
    let ready = checks
        .iter()
        .all(|c| !c.required || c.status == DoctorCheckStatus::Ready);

    let report = DoctorReport {
        ready,
        engine: DoctorEngineSummary {
            version: env!("CARGO_PKG_VERSION").to_string(),
            native: true,
        },
        platform,
        device,
        checks,
    };

    let redacted_val =
        redaction.redact_json(&serde_json::to_value(&report).expect("doctor report serializes"));

    if json_mode {
        deps.output.writeln_stdout(
            &serde_json::to_string_pretty(&redacted_val).expect("redacted doctor json"),
        );
    } else {
        deps.output.writeln_stdout(&format!(
            "Déjà doctor: {}",
            if ready { "ready" } else { "not ready" }
        ));
        for check in &report.checks {
            let tag = match check.status {
                DoctorCheckStatus::Ready => "ready",
                DoctorCheckStatus::Warn => "warn",
                DoctorCheckStatus::NotReady => "not-ready",
            };
            deps.output.writeln_stdout(&format!(
                "  [{tag}] {}: {}",
                check.name,
                redaction.redact_str(&check.detail)
            ));
            if let Some(ref rem) = check.remediation {
                deps.output
                    .writeln_stdout(&format!("        fix: {}", redaction.redact_str(rem)));
            }
        }
    }

    if ready { EXIT_OK } else { EXIT_FAILURE }
}
