//! Native Rust `deja` CLI binary entrypoint.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use deja_cli::{CliDeps, run_cli_from_args};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

pub use deja_cli::EMBEDDED_AGENT_GUIDE_MD;

fn main() -> ExitCode {
    let filter = EnvFilter::try_from_env("DEJA_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build Tokio runtime");

    let signal_exit_code = Arc::new(AtomicU8::new(0));
    let signal_exit_clone = Arc::clone(&signal_exit_code);

    let exit_code = runtime.block_on(async move {
        let cancel = CancellationToken::new();
        let signal_cancel = cancel.clone();
        let signal_task = tokio::spawn(async move {
            let code = wait_for_termination_signal().await;
            signal_exit_clone.store(code, Ordering::SeqCst);
            signal_cancel.cancel();
        });

        let deps = CliDeps::real(cancel);
        let code = run_cli_from_args(std::env::args_os(), &deps).await;
        signal_task.abort();
        let sig = signal_exit_code.load(Ordering::SeqCst);
        if sig != 0 && code == 0 { sig } else { code }
    });

    // Bounded runtime shutdown so a blocked Tokio blocking-pool `stdin` read (`read(0, ...)`)
    // cannot hang process exit on SIGTERM/SIGINT after `deja_mcp::serve` has already finished
    // draining tasks, shutting down drivers, releasing SQLite locks, and flushing stdio.
    runtime.shutdown_timeout(Duration::from_millis(200));
    ExitCode::from(exit_code)
}

async fn wait_for_termination_signal() -> u8 {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => 130,
            _ = async {
                if let Some(ref mut s) = sigterm {
                    s.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => 143,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        130
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
