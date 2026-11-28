//! Internal library exports and testable CLI entrypoint for `deja-cli`.

pub mod cli;
pub mod commands;
pub mod config;
pub mod daemon;
pub mod driver_factory;
pub mod exit_codes;
pub mod fs_safe;
pub mod output;
pub mod repo_check;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use clap::Parser;
use tokio_util::sync::CancellationToken;

use crate::cli::{Cli, Commands};
use crate::driver_factory::{DoctorProbes, DriverFactory, RealDoctorProbes, RealDriverFactory};
use crate::exit_codes::{EXIT_OK, EXIT_USAGE_OR_CONFIG};
use crate::output::OutputSink;

/// Embedded Déjà agent workflow guide (`docs/AGENT_GUIDE.md`).
pub use crate::commands::guide::EMBEDDED_AGENT_GUIDE_MD;

/// Linked workspace crates verifying `deja-cli` stays the sole composition root.
pub const LINKED_WORKSPACE_CRATES: [&str; 8] = [
    deja_core::CRATE_NAME,
    deja_store::CRATE_NAME,
    deja_flow::CRATE_NAME,
    deja_driver_ios::CRATE_NAME,
    deja_driver_android::CRATE_NAME,
    deja_driver_web::CRATE_NAME,
    deja_explore::CRATE_NAME,
    deja_mcp::CRATE_NAME,
];

/// Dependency bundle for running `deja` in production or in deterministic in-process tests.
#[derive(Clone)]
pub struct CliDeps {
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub home_dir: Option<PathBuf>,
    pub output: OutputSink,
    pub driver_factory: Arc<dyn DriverFactory>,
    pub doctor_probes: Arc<dyn DoctorProbes>,
    pub cancel: CancellationToken,
    /// OS termination signal recorded BEFORE `cancel.cancel()` (`0` = none/token, `2` = SIGINT, `15` = SIGTERM).
    pub cancel_signal: Arc<AtomicU8>,
}

impl CliDeps {
    /// Construct production [`CliDeps`] reading real process cwd, environment, stdio,
    /// `RealDriverFactory`, and `RealDoctorProbes`.
    #[must_use]
    pub fn real(cancel: CancellationToken) -> Self {
        Self::real_with_signal(cancel, Arc::new(AtomicU8::new(0)))
    }

    /// Construct production [`CliDeps`] wired with a shared `cancel_signal` slot so signal
    /// handlers can report exact POSIX exit codes (`130` for `SIGINT`, `143` for `SIGTERM`).
    #[must_use]
    pub fn real_with_signal(cancel: CancellationToken, cancel_signal: Arc<AtomicU8>) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let env: BTreeMap<String, String> = std::env::vars().collect();
        let home_dir = env
            .get("HOME")
            .or_else(|| env.get("USERPROFILE"))
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let direct_factory: Arc<dyn DriverFactory> = Arc::new(RealDriverFactory);
        #[cfg(unix)]
        let driver_factory: Arc<dyn DriverFactory> = Arc::new(
            crate::daemon::client::DaemonDriverFactory::new(direct_factory, env.clone(), true),
        );
        #[cfg(not(unix))]
        let driver_factory = direct_factory;

        Self {
            cwd,
            env,
            home_dir,
            output: OutputSink::Stdio,
            driver_factory,
            doctor_probes: Arc::new(RealDoctorProbes),
            cancel,
            cancel_signal,
        }
    }
}

/// Dispatch a parsed [`Cli`] command using `deps` and return its exact CLI exit code.
pub async fn run_cli(cli: Cli, deps: &CliDeps) -> u8 {
    let _ = LINKED_WORKSPACE_CRATES;
    let explicit_config = cli.config.as_deref();

    let bypass_daemon = cli.no_daemon || crate::daemon::is_no_daemon_env(&deps.env);
    let owned_deps;
    let effective_deps = if bypass_daemon {
        let mut d = deps.clone();
        d.driver_factory = Arc::clone(&deps.driver_factory).without_daemon();
        owned_deps = d;
        &owned_deps
    } else {
        deps
    };

    match cli.command {
        None => {
            effective_deps.output.writeln_stderr(
                "No command specified. Run `deja --help` for usage or `deja guide` for the agent workflow guide.",
            );
            EXIT_USAGE_OR_CONFIG
        }
        Some(Commands::Run(ref args)) => {
            commands::run::run(args, explicit_config, cli.json, effective_deps).await
        }
        Some(Commands::Remember(ref args)) => {
            commands::remember::run(args, explicit_config, cli.json, effective_deps).await
        }
        Some(Commands::List(ref args)) => {
            commands::list::run(args, explicit_config, cli.json, effective_deps)
        }
        Some(Commands::Inspect(ref args)) => {
            commands::inspect::run(args, explicit_config, cli.json, effective_deps)
        }
        Some(Commands::Manifest(ref args)) => {
            commands::manifest::run(args, explicit_config, cli.json, effective_deps)
        }
        Some(Commands::Gate(ref args)) => {
            commands::gate::run(args, explicit_config, cli.json, effective_deps)
        }
        Some(Commands::Doctor(ref args)) => {
            commands::doctor::run(args, explicit_config, cli.json, effective_deps).await
        }
        Some(Commands::Migrate(ref args)) => {
            commands::migrate::run(args, explicit_config, cli.json, effective_deps)
        }
        Some(Commands::Guide) => commands::guide::run(effective_deps),
        Some(Commands::Engine(ref args)) => commands::engine::run(args, cli.json, effective_deps),
        Some(Commands::Mcp(ref args)) => {
            commands::mcp::run(args, explicit_config, cli.json, effective_deps).await
        }
        Some(Commands::Daemon(ref args)) => {
            commands::daemon::run(args, cli.json, effective_deps).await
        }
    }
}

/// Parse CLI arguments from an iterator and execute [`run_cli`], mapping `--help`/`--version`
/// to exit `0` and `clap` parse errors cleanly to exit `2`.
pub async fn run_cli_from_args<I, T>(args: I, deps: &CliDeps) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    match Cli::try_parse_from(args) {
        Ok(cli) => run_cli(cli, deps).await,
        Err(err) => {
            use clap::error::ErrorKind;
            match err.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                    deps.output.write_stdout(&err.to_string());
                    EXIT_OK
                }
                _ => {
                    deps.output.write_stderr(&err.to_string());
                    EXIT_USAGE_OR_CONFIG
                }
            }
        }
    }
}
