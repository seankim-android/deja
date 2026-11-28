//! `deja engine install` — retired Maestro installer notice (exit 2).

use serde_json::json;

use crate::CliDeps;
use crate::cli::{EngineArgs, EngineCommand};
use crate::exit_codes::EXIT_USAGE_OR_CONFIG;

pub const NATIVE_ENGINE_MESSAGE: &str =
    "Déjà executes flows natively; Maestro is no longer required";

pub fn run(args: &EngineArgs, json_mode: bool, deps: &CliDeps) -> u8 {
    match args.command {
        EngineCommand::Install => {
            if json_mode {
                let payload = json!({
                    "error": {
                        "code": "ENGINE_INSTALL_RETIRED",
                        "message": NATIVE_ENGINE_MESSAGE,
                        "remediation": "Run `deja doctor --json` to verify native toolchain readiness."
                    }
                });
                deps.output.writeln_stdout(
                    &serde_json::to_string_pretty(&payload).expect("engine JSON serializes"),
                );
            } else {
                deps.output.writeln_stderr(NATIVE_ENGINE_MESSAGE);
                deps.output.writeln_stderr(
                    "Run `deja doctor --json` to verify native toolchain readiness.",
                );
            }
            EXIT_USAGE_OR_CONFIG
        }
    }
}
