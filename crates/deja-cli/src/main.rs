//! Native Rust `deja` CLI binary entrypoint.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use deja_cli::exit_codes::{CANCEL_SIGNAL_SIGINT, CANCEL_SIGNAL_SIGTERM};
use deja_cli::{CliDeps, run_cli_from_args};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

pub use deja_cli::EMBEDDED_AGENT_GUIDE_MD;

#[tokio::main]
async fn main() -> ExitCode {
    let filter = EnvFilter::try_from_env("DEJA_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();

    let cancel = CancellationToken::new();
    let cancel_signal = Arc::new(AtomicU8::new(0));
    let signal_cancel = cancel.clone();
    let signal_slot = Arc::clone(&cancel_signal);
    let signal_task = tokio::spawn(async move {
        let sig = wait_for_termination_signal().await;
        signal_slot.store(sig, Ordering::SeqCst);
        signal_cancel.cancel();
    });

    let deps = CliDeps::real_with_signal(cancel, cancel_signal);
    let exit_code = run_cli_from_args(std::env::args_os(), &deps).await;
    signal_task.abort();
    ExitCode::from(exit_code)
}

async fn wait_for_termination_signal() -> u8 {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => CANCEL_SIGNAL_SIGINT,
            _ = async {
                if let Some(ref mut s) = sigterm {
                    s.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => CANCEL_SIGNAL_SIGTERM,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        CANCEL_SIGNAL_SIGINT
    }
}

#[cfg(test)]
mod tests {
    use super::EMBEDDED_AGENT_GUIDE_MD;
    use clap::CommandFactory;
    use deja_cli::{LINKED_WORKSPACE_CRATES, cli::Cli};

    #[test]
    fn verify_cli_definition() {
        Cli::command().debug_assert();
    }

    #[test]
    fn composition_root_links_all_workspace_crates() {
        assert_eq!(LINKED_WORKSPACE_CRATES.len(), 8);
    }

    #[test]
    fn embedded_agent_guide_is_present_and_well_formed() {
        assert!(EMBEDDED_AGENT_GUIDE_MD.starts_with("# Déjà Agent Guide\n"));
        assert!(EMBEDDED_AGENT_GUIDE_MD.contains("deja doctor"));
        assert!(EMBEDDED_AGENT_GUIDE_MD.contains("deja remember"));
        assert!(EMBEDDED_AGENT_GUIDE_MD.contains("deja run"));
    }
}
