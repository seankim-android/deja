//! `deja daemon <start|status|stop>` — lifecycle management for the persistent warm driver daemon.

use serde_json::json;

use crate::CliDeps;
use crate::cli::DaemonArgs;
use crate::exit_codes::EXIT_USAGE_OR_CONFIG;

#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use crate::cli::{DaemonCommand, DaemonStartArgs};
#[cfg(unix)]
use crate::daemon::client::{DaemonClient, ensure_background_daemon_started};
#[cfg(unix)]
use crate::daemon::paths::{
    DaemonPathError, DaemonPaths, acquire_daemon_lock, cleanup_stale_files, probe_daemon_lock_held,
    read_daemon_metadata, validate_daemon_dir_secure,
};
#[cfg(unix)]
use crate::daemon::server::{DaemonServer, DaemonStartError};
#[cfg(unix)]
use crate::exit_codes::{EXIT_FAILURE, EXIT_OK};

fn emit_daemon_error(deps: &CliDeps, json_mode: bool, code: &str, message: &str, exit: u8) -> u8 {
    if json_mode {
        let payload = json!({
            "error": {
                "code": code,
                "message": message,
            }
        });
        deps.output
            .writeln_stdout(&serde_json::to_string_pretty(&payload).expect("daemon error JSON"));
    } else {
        deps.output
            .writeln_stderr(&format!("deja daemon error: {message}"));
    }
    exit
}

pub async fn run(args: &DaemonArgs, json_mode: bool, deps: &CliDeps) -> u8 {
    #[cfg(not(unix))]
    {
        let _ = args;
        emit_daemon_error(
            deps,
            json_mode,
            "DAEMON_UNSUPPORTED_PLATFORM",
            "Déjà persistent warm driver daemon is only supported on Unix (macOS and Linux); pass `--no-daemon` or set `DEJA_NO_DAEMON=1` on this platform.",
            EXIT_USAGE_OR_CONFIG,
        )
    }

    #[cfg(unix)]
    match &args.command {
        DaemonCommand::Start(start_args) => run_start(start_args, json_mode, deps).await,
        DaemonCommand::Status => run_status(json_mode, deps).await,
        DaemonCommand::Stop => run_stop(json_mode, deps).await,
    }
}

#[cfg(unix)]
async fn run_start(args: &DaemonStartArgs, json_mode: bool, deps: &CliDeps) -> u8 {
    if args.idle_timeout == 0 {
        return emit_daemon_error(
            deps,
            json_mode,
            "INVALID_IDLE_TIMEOUT",
            "`--idle-timeout` must be greater than 0 seconds",
            EXIT_USAGE_OR_CONFIG,
        );
    }

    let paths = match DaemonPaths::resolve(&deps.env) {
        Ok(p) => p,
        Err(err) => {
            return emit_daemon_error(
                deps,
                json_mode,
                "DAEMON_PATH_ERROR",
                &err.to_string(),
                EXIT_USAGE_OR_CONFIG,
            );
        }
    };

    if args.foreground {
        let direct_factory = Arc::clone(&deps.driver_factory).without_daemon();
        let server = match DaemonServer::bind(
            paths,
            direct_factory,
            Duration::from_secs(args.idle_timeout),
        ) {
            Ok(srv) => srv,
            Err(DaemonStartError::AlreadyRunning) => {
                return emit_daemon_error(
                    deps,
                    json_mode,
                    "DAEMON_ALREADY_RUNNING",
                    "a Déjà daemon is already running for this daemon directory",
                    EXIT_FAILURE,
                );
            }
            Err(DaemonStartError::Path(DaemonPathError::InvalidConfig(msg))) => {
                return emit_daemon_error(
                    deps,
                    json_mode,
                    "DAEMON_PATH_ERROR",
                    &msg,
                    EXIT_USAGE_OR_CONFIG,
                );
            }
            Err(DaemonStartError::Path(err)) => {
                return emit_daemon_error(
                    deps,
                    json_mode,
                    "DAEMON_START_FAILED",
                    &err.to_string(),
                    EXIT_FAILURE,
                );
            }
        };

        server.run(deps.cancel.clone()).await;
        return EXIT_OK;
    }

    match ensure_background_daemon_started(&paths, &deps.env, args.idle_timeout, &deps.cancel).await
    {
        Ok(info) => {
            if json_mode {
                let payload = json!({
                    "started": info.started,
                    "alreadyRunning": info.already_running,
                    "pid": info.pid,
                    "version": info.version,
                    "socketPath": info.socket_path,
                    "idleTimeoutSecs": info.idle_timeout_secs,
                });
                deps.output.writeln_stdout(
                    &serde_json::to_string_pretty(&payload).expect("daemon start JSON"),
                );
            } else if info.already_running {
                deps.output.writeln_stdout(&format!(
                    "Déjà daemon is already running (pid {}, socket {}).",
                    info.pid, info.socket_path
                ));
            } else {
                deps.output.writeln_stdout(&format!(
                    "Déjà daemon started (pid {}, socket {}, idle timeout {}s).",
                    info.pid, info.socket_path, info.idle_timeout_secs
                ));
            }
            EXIT_OK
        }
        Err(DaemonPathError::InvalidConfig(msg)) => emit_daemon_error(
            deps,
            json_mode,
            "DAEMON_PATH_ERROR",
            &msg,
            EXIT_USAGE_OR_CONFIG,
        ),
        Err(err) => emit_daemon_error(
            deps,
            json_mode,
            "DAEMON_START_FAILED",
            &err.to_string(),
            EXIT_FAILURE,
        ),
    }
}

#[cfg(unix)]
async fn run_status(json_mode: bool, deps: &CliDeps) -> u8 {
    let paths = match DaemonPaths::resolve(&deps.env) {
        Ok(p) => p,
        Err(err) => {
            return emit_daemon_error(
                deps,
                json_mode,
                "DAEMON_PATH_ERROR",
                &err.to_string(),
                EXIT_USAGE_OR_CONFIG,
            );
        }
    };

    match validate_daemon_dir_secure(&paths.dir) {
        Ok(_) => {}
        Err(DaemonPathError::NotFound(_)) => {
            return emit_not_running_status(deps, json_mode, &paths, Vec::new());
        }
        Err(err) => {
            return emit_daemon_error(
                deps,
                json_mode,
                "DAEMON_SECURITY_ERROR",
                &err.to_string(),
                EXIT_FAILURE,
            );
        }
    }

    match DaemonClient::connect(&paths).await {
        Ok(client) => match client.status(&deps.cancel).await {
            Ok(report) => {
                if json_mode {
                    deps.output.writeln_stdout(
                        &serde_json::to_string_pretty(&report).expect("daemon status JSON"),
                    );
                } else {
                    deps.output.writeln_stdout(&format!(
                        "Déjà daemon: running (pid {}, v{}, uptime {}ms, idle {}ms / {}s, active leases {}, pooled sessions {})",
                        report.pid,
                        report.version,
                        report.uptime_ms,
                        report.idle_ms,
                        report.idle_timeout_secs,
                        report.active_leases,
                        report.pooled_sessions
                    ));
                    for sess in &report.sessions {
                        let attach_str = sess
                            .attach_count
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "n/a".to_string());
                        deps.output.writeln_stdout(&format!(
                            "  - {} [{}] leased={} attachCount={} leases={} idle={}ms",
                            sess.device_key,
                            sess.platform,
                            sess.leased,
                            attach_str,
                            sess.lease_count,
                            sess.idle_ms
                        ));
                    }
                }
                EXIT_OK
            }
            Err(err) => emit_daemon_error(
                deps,
                json_mode,
                "DAEMON_STATUS_FAILED",
                &err.to_string(),
                EXIT_FAILURE,
            ),
        },
        Err(err) if err.is_security_violation() => emit_daemon_error(
            deps,
            json_mode,
            "DAEMON_SECURITY_ERROR",
            &err.to_string(),
            EXIT_FAILURE,
        ),
        Err(_) => {
            // Daemon not connectable: attempt non-blocking exclusive lock to clean stale files
            // safely without racing a concurrently starting daemon.
            match acquire_daemon_lock(&paths) {
                Ok(Some(lock)) => match cleanup_stale_files(&paths, &lock) {
                    Ok(cleaned) => emit_not_running_status(deps, json_mode, &paths, cleaned),
                    Err(err) => emit_daemon_error(
                        deps,
                        json_mode,
                        "DAEMON_SECURITY_ERROR",
                        &err.to_string(),
                        EXIT_FAILURE,
                    ),
                },
                Ok(None) => {
                    // Lock is held (daemon may be mid-startup or shutting down). Retry once briefly.
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    if let Ok(client) = DaemonClient::connect(&paths).await
                        && let Ok(report) = client.status(&deps.cancel).await
                    {
                        if json_mode {
                            deps.output.writeln_stdout(
                                &serde_json::to_string_pretty(&report).expect("daemon status JSON"),
                            );
                        } else {
                            deps.output.writeln_stdout(&format!(
                                "Déjà daemon: running (pid {}, v{}, active leases {}, pooled sessions {})",
                                report.pid,
                                report.version,
                                report.active_leases,
                                report.pooled_sessions
                            ));
                        }
                        return EXIT_OK;
                    }
                    emit_daemon_error(
                        deps,
                        json_mode,
                        "DAEMON_UNRESPONSIVE",
                        "Déjà daemon lock is held, but the daemon socket is not responding; run `deja daemon stop` to recover",
                        EXIT_FAILURE,
                    )
                }
                Err(err) => emit_daemon_error(
                    deps,
                    json_mode,
                    "DAEMON_SECURITY_ERROR",
                    &err.to_string(),
                    EXIT_FAILURE,
                ),
            }
        }
    }
}

#[cfg(unix)]
fn emit_not_running_status(
    deps: &CliDeps,
    json_mode: bool,
    paths: &DaemonPaths,
    cleaned_stale_files: Vec<String>,
) -> u8 {
    if json_mode {
        let payload = json!({
            "running": false,
            "socketPath": paths.socket.to_string_lossy(),
            "cleanedStaleFiles": cleaned_stale_files,
        });
        deps.output
            .writeln_stdout(&serde_json::to_string_pretty(&payload).expect("daemon status JSON"));
    } else if cleaned_stale_files.is_empty() {
        deps.output.writeln_stdout("Déjà daemon: not running");
    } else {
        deps.output.writeln_stdout(&format!(
            "Déjà daemon: not running (cleaned stale files: {})",
            cleaned_stale_files.join(", ")
        ));
    }
    EXIT_OK
}

#[cfg(unix)]
async fn run_stop(json_mode: bool, deps: &CliDeps) -> u8 {
    let paths = match DaemonPaths::resolve(&deps.env) {
        Ok(p) => p,
        Err(err) => {
            return emit_daemon_error(
                deps,
                json_mode,
                "DAEMON_PATH_ERROR",
                &err.to_string(),
                EXIT_USAGE_OR_CONFIG,
            );
        }
    };

    match validate_daemon_dir_secure(&paths.dir) {
        Ok(_) => {}
        Err(DaemonPathError::NotFound(_)) => {
            return emit_already_stopped(deps, json_mode, Vec::new());
        }
        Err(err) => {
            return emit_daemon_error(
                deps,
                json_mode,
                "DAEMON_SECURITY_ERROR",
                &err.to_string(),
                EXIT_FAILURE,
            );
        }
    }

    match DaemonClient::connect(&paths).await {
        Ok(client) => {
            let target_pid = client.metadata().pid;
            let stopped_sessions = client.shutdown_daemon(&deps.cancel).await.unwrap_or(0);
            drop(client);

            let unlocked = wait_for_lock_release(&paths, Duration::from_secs(10)).await;
            let mut forced_sigterm = false;
            if !unlocked {
                forced_sigterm = true;
                let _ = send_sigterm_via_bin_kill(target_pid);
                let _ = wait_for_lock_release(&paths, Duration::from_secs(3)).await;
            }

            if let Ok(Some(lock)) = acquire_daemon_lock(&paths) {
                let _ = cleanup_stale_files(&paths, &lock);
            }

            if json_mode {
                let payload = json!({
                    "stopped": true,
                    "wasRunning": true,
                    "pid": target_pid,
                    "stoppedSessions": stopped_sessions,
                    "forcedSigterm": forced_sigterm,
                });
                deps.output.writeln_stdout(
                    &serde_json::to_string_pretty(&payload).expect("daemon stop JSON"),
                );
            } else {
                deps.output.writeln_stdout(&format!(
                    "Déjà daemon stopped (pid {}, stopped {} pooled session(s){}).",
                    target_pid,
                    stopped_sessions,
                    if forced_sigterm {
                        ", fallback SIGTERM"
                    } else {
                        ""
                    }
                ));
            }
            EXIT_OK
        }
        Err(err) if err.is_security_violation() => emit_daemon_error(
            deps,
            json_mode,
            "DAEMON_SECURITY_ERROR",
            &err.to_string(),
            EXIT_FAILURE,
        ),
        Err(_) => match acquire_daemon_lock(&paths) {
            Ok(Some(lock)) => match cleanup_stale_files(&paths, &lock) {
                Ok(cleaned) => emit_already_stopped(deps, json_mode, cleaned),
                Err(err) => emit_daemon_error(
                    deps,
                    json_mode,
                    "DAEMON_SECURITY_ERROR",
                    &err.to_string(),
                    EXIT_FAILURE,
                ),
            },
            Ok(None) => {
                // Lock is held but socket isn't accepting: try waiting briefly, else SIGTERM PID
                // from verified `daemon.json`.
                let meta_pid = read_daemon_metadata(&paths).ok().map(|m| m.pid);
                if wait_for_lock_release(&paths, Duration::from_secs(2)).await {
                    let cleaned = acquire_daemon_lock(&paths)
                        .ok()
                        .flatten()
                        .and_then(|lock| cleanup_stale_files(&paths, &lock).ok())
                        .unwrap_or_default();
                    return emit_already_stopped(deps, json_mode, cleaned);
                }
                if let Some(pid) = meta_pid {
                    let _ = send_sigterm_via_bin_kill(pid);
                    let released = wait_for_lock_release(&paths, Duration::from_secs(5)).await;
                    if let Ok(Some(lock)) = acquire_daemon_lock(&paths) {
                        let _ = cleanup_stale_files(&paths, &lock);
                    }
                    if released {
                        if json_mode {
                            let payload = json!({
                                "stopped": true,
                                "wasRunning": true,
                                "pid": pid,
                                "stoppedSessions": 0,
                                "forcedSigterm": true,
                            });
                            deps.output.writeln_stdout(
                                &serde_json::to_string_pretty(&payload).expect("daemon stop JSON"),
                            );
                        } else {
                            deps.output.writeln_stdout(&format!(
                                "Déjà daemon stopped via SIGTERM (pid {pid})."
                            ));
                        }
                        return EXIT_OK;
                    }
                }
                emit_daemon_error(
                    deps,
                    json_mode,
                    "DAEMON_STOP_FAILED",
                    "Déjà daemon lock remained held after stop attempt",
                    EXIT_FAILURE,
                )
            }
            Err(err) => emit_daemon_error(
                deps,
                json_mode,
                "DAEMON_SECURITY_ERROR",
                &err.to_string(),
                EXIT_FAILURE,
            ),
        },
    }
}

#[cfg(unix)]
fn emit_already_stopped(deps: &CliDeps, json_mode: bool, cleaned_stale_files: Vec<String>) -> u8 {
    if json_mode {
        let payload = json!({
            "stopped": false,
            "wasRunning": false,
            "stoppedSessions": 0,
            "cleanedStaleFiles": cleaned_stale_files,
        });
        deps.output
            .writeln_stdout(&serde_json::to_string_pretty(&payload).expect("daemon stop JSON"));
    } else if cleaned_stale_files.is_empty() {
        deps.output.writeln_stdout("Déjà daemon is not running.");
    } else {
        deps.output.writeln_stdout(&format!(
            "Déjà daemon is not running (cleaned stale files: {}).",
            cleaned_stale_files.join(", ")
        ));
    }
    EXIT_OK
}

#[cfg(unix)]
async fn wait_for_lock_release(paths: &DaemonPaths, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match probe_daemon_lock_held(paths) {
            Ok(false) => return true,
            Ok(true) => tokio::time::sleep(Duration::from_millis(25)).await,
            Err(_) => return false,
        }
    }
    matches!(probe_daemon_lock_held(paths), Ok(false))
}

#[cfg(unix)]
fn send_sigterm_via_bin_kill(pid: u32) -> std::io::Result<()> {
    if pid == 0 {
        return Ok(());
    }
    let _ = std::process::Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(())
}
