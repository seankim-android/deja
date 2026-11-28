//! Unix-socket warm driver daemon server (`deja daemon start --foreground`).
//!
//! Accepts same-UID client connections, runs a cancel-safe per-connection frame loop supporting
//! in-flight `CancelRpc` and abrupt disconnect eviction, enforces idle TTL auto-shutdown without
//! racing active leases, and cleanly shuts down all pooled drivers before releasing `daemon.lock`.

use std::sync::Arc;
use std::time::Duration;

use deja_flow::PlatformDriver;
use deja_store::now_iso8601;
use thiserror::Error;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::daemon::paths::{
    DaemonLock, DaemonMetadata, DaemonPathError, DaemonPaths, acquire_daemon_lock,
    bind_daemon_socket, current_uid, remove_if_not_symlink, write_daemon_metadata,
};
use crate::daemon::pool::{DriverPool, LeaseGuard};
use crate::daemon::protocol::{
    BoundedNdjsonReader, ClientMessage, DriverOp, ServerMessage, WireDriverError, WireDriverResult,
    WireDriverValue, WireWaitSelectorOutcome, write_ndjson_frame,
};
use crate::driver_factory::DriverFactory;

const ACTIVE_RPC_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECTION_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors emitted when binding and starting [`DaemonServer`].
#[derive(Debug, Error)]
pub enum DaemonStartError {
    #[error("a Déjà daemon is already running for this directory")]
    AlreadyRunning,
    #[error(transparent)]
    Path(#[from] DaemonPathError),
}

struct ActiveRpc {
    id: u64,
    cancel: CancellationToken,
    handle: JoinHandle<WireDriverResult>,
}

async fn poll_active_rpc(
    slot: &mut Option<ActiveRpc>,
) -> (u64, Result<WireDriverResult, tokio::task::JoinError>) {
    match slot {
        Some(rpc) => {
            let id = rpc.id;
            let res = (&mut rpc.handle).await;
            *slot = None;
            (id, res)
        }
        None => std::future::pending().await,
    }
}

/// Bound and listening warm driver daemon ready to serve connections until idle TTL, `Shutdown`
/// RPC, or `SIGINT`/`SIGTERM`.
pub struct DaemonServer {
    paths: DaemonPaths,
    listener: UnixListener,
    lock: DaemonLock,
    pool: Arc<DriverPool>,
    metadata: DaemonMetadata,
    idle_timeout: Duration,
    daemon_uid: u32,
}

impl DaemonServer {
    /// Lock `daemon.lock`, bind `deja.<pid>.sock.tmp` (`0o600`), rename to `deja.sock`, and
    /// atomically publish `daemon.json`. Once this returns `Ok`, clients can connect immediately.
    pub fn bind(
        paths: DaemonPaths,
        direct_factory: Arc<dyn DriverFactory>,
        idle_timeout: Duration,
    ) -> Result<Self, DaemonStartError> {
        let Some(lock) = acquire_daemon_lock(&paths)? else {
            return Err(DaemonStartError::AlreadyRunning);
        };
        let daemon_uid = current_uid().map_err(DaemonPathError::Io)?;

        // Remove any stale `daemon.json` BEFORE publishing the new socket so connecting clients
        // never observe a stale PID from a previous crash.
        let _ = remove_if_not_symlink(&paths.metadata)?;

        let listener = bind_daemon_socket(&paths)?;
        let metadata = DaemonMetadata {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            uid: daemon_uid,
            socket_path: paths.socket.to_string_lossy().into_owned(),
            started_at: now_iso8601(),
            idle_timeout_secs: idle_timeout.as_secs(),
        };
        if let Err(err) = write_daemon_metadata(&paths, &metadata) {
            let _ = remove_if_not_symlink(&paths.socket);
            return Err(DaemonStartError::Path(err));
        }

        let direct = direct_factory.without_daemon();
        let pool = DriverPool::new(direct);
        Ok(Self {
            paths,
            listener,
            lock,
            pool,
            metadata,
            idle_timeout,
            daemon_uid,
        })
    }

    #[must_use]
    pub fn paths(&self) -> &DaemonPaths {
        &self.paths
    }

    #[must_use]
    pub fn metadata(&self) -> &DaemonMetadata {
        &self.metadata
    }

    #[must_use]
    pub fn pool(&self) -> Arc<DriverPool> {
        Arc::clone(&self.pool)
    }

    /// Run the daemon accept loop until `external_cancel`, a `Shutdown` RPC, or idle TTL expiry.
    /// Guarantees cleanup ordering: stop accepting -> unlink `deja.sock` + `daemon.json` ->
    /// drain connections -> concurrently shut down all pooled drivers (10s bound each) ->
    /// drop `daemon.lock` LAST.
    pub async fn run(self, external_cancel: CancellationToken) {
        let DaemonServer {
            paths,
            listener,
            lock,
            pool,
            metadata,
            idle_timeout,
            daemon_uid,
        } = self;

        let tick_interval =
            (idle_timeout / 4).clamp(Duration::from_millis(10), Duration::from_secs(5));
        let mut idle_ticker = tokio::time::interval(tick_interval);
        idle_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Consume immediate first tick at t=0.
        idle_ticker.tick().await;

        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<oneshot::Sender<usize>>(4);
        let mut conn_tasks: JoinSet<()> = JoinSet::new();
        let mut shutdown_ack_waiters: Vec<oneshot::Sender<usize>> = Vec::new();
        let metadata = Arc::new(metadata);

        loop {
            // Reap finished connection tasks opportunistically so `JoinSet` stays bounded.
            while conn_tasks.try_join_next().is_some() {}

            tokio::select! {
                biased;
                _ = external_cancel.cancelled() => {
                    debug!("daemon received external cancellation signal");
                    break;
                }
                maybe_reply = shutdown_rx.recv() => {
                    debug!("daemon received Shutdown RPC");
                    if let Some(reply) = maybe_reply {
                        shutdown_ack_waiters.push(reply);
                    }
                    break;
                }
                _ = idle_ticker.tick() => {
                    if pool.try_begin_idle_shutdown(idle_timeout) {
                        debug!(
                            idle_ms = pool.idle_elapsed().as_millis() as u64,
                            "daemon idle TTL reached with zero active leases; shutting down"
                        );
                        break;
                    }
                }
                accept_res = listener.accept() => {
                    match accept_res {
                        Ok((stream, _addr)) => {
                            match stream.peer_cred() {
                                Ok(cred) if cred.uid() == daemon_uid => {
                                    let pool_c = Arc::clone(&pool);
                                    let meta_c = Arc::clone(&metadata);
                                    let stx = shutdown_tx.clone();
                                    conn_tasks.spawn(async move {
                                        handle_connection(
                                            stream,
                                            pool_c,
                                            meta_c,
                                            idle_timeout,
                                            stx,
                                        )
                                        .await;
                                    });
                                }
                                Ok(cred) => {
                                    warn!(
                                        peer_uid = cred.uid(),
                                        expected_uid = daemon_uid,
                                        "rejecting daemon Unix socket connection from mismatched UID"
                                    );
                                    drop(stream);
                                }
                                Err(err) => {
                                    warn!("rejecting daemon connection without readable peer_cred: {err}");
                                    drop(stream);
                                }
                            }
                        }
                        Err(err) => {
                            warn!("daemon listener accept error: {err}");
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                    }
                }
            }
        }

        // Drain any additional Shutdown RPC replies queued right before break.
        while let Ok(reply) = shutdown_rx.try_recv() {
            shutdown_ack_waiters.push(reply);
        }

        // 1. Mark pool shutting down and cancel all active RPC / acquire waiters.
        pool.mark_shutting_down();

        // 2. Stop accepting new connections and remove published socket & metadata files.
        drop(listener);
        let _ = remove_if_not_symlink(&paths.socket);
        let _ = remove_if_not_symlink(&paths.metadata);

        // 3. Drain active non-shutdown connection tasks briefly so their leases release/evict.
        let non_shutdown_drain = async {
            // Note: shutdown-RPC handler tasks await `shutdown_ack_waiters`, so only drain
            // up to CONNECTION_DRAIN_TIMEOUT when no shutdown waiter is waiting on us.
            if shutdown_ack_waiters.is_empty() {
                while conn_tasks.join_next().await.is_some() {}
            } else {
                // Give other connections a bounded window to notice `shutdown_token` and drop leases.
                let deadline = tokio::time::Instant::now() + CONNECTION_DRAIN_TIMEOUT;
                while pool.active_leases() > 0 && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        };
        let _ = tokio::time::timeout(CONNECTION_DRAIN_TIMEOUT, non_shutdown_drain).await;

        // 4. Concurrently shut down ALL pooled drivers (bounded 10s each).
        let stopped_sessions = pool.shutdown_all_drivers().await;

        // 5. Notify any `Shutdown` RPC clients so they receive `ShutdownAck { stoppedSessions }`.
        for waiter in shutdown_ack_waiters {
            let _ = waiter.send(stopped_sessions);
        }
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            while conn_tasks.join_next().await.is_some() {}
        })
        .await;
        conn_tasks.abort_all();

        // 6. Drop `daemon.lock` LAST so `deja daemon stop` observes unlock only after complete cleanup.
        drop(lock);
    }
}

async fn handle_connection(
    stream: UnixStream,
    pool: Arc<DriverPool>,
    metadata: Arc<DaemonMetadata>,
    idle_timeout: Duration,
    shutdown_tx: mpsc::Sender<oneshot::Sender<usize>>,
) {
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BoundedNdjsonReader::new(read_half);
    let mut lease: Option<LeaseGuard> = None;
    let mut active_rpc: Option<ActiveRpc> = None;
    let shutdown_token = pool.shutdown_token().clone();

    loop {
        tokio::select! {
            biased;
            (rpc_id, join_res) = poll_active_rpc(&mut active_rpc) => {
                pool.touch_activity();
                let wire_res = match join_res {
                    Ok(res) => res,
                    Err(err) => {
                        if let Some(ref l) = lease {
                            l.mark_dirty();
                        }
                        WireDriverResult::Err {
                            error: WireDriverError::Transport {
                                operation: "daemon_rpc".to_string(),
                                detail: format!("driver RPC task failed: {err}"),
                                transient: false,
                            },
                        }
                    }
                };
                if write_ndjson_frame(
                    &mut writer,
                    &ServerMessage::RpcResult {
                        id: rpc_id,
                        result: wire_res,
                    },
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            _ = shutdown_token.cancelled() => {
                break;
            }
            frame_res = reader.next_frame::<ClientMessage>() => {
                match frame_res {
                    Ok(None) => {
                        break;
                    }
                    Err(err) => {
                        let _ = write_ndjson_frame(
                            &mut writer,
                            &ServerMessage::ProtocolError {
                                message: err.to_string(),
                            },
                        )
                        .await;
                        break;
                    }
                    Ok(Some(msg)) => {
                        match msg {
                            ClientMessage::Ping => {
                                pool.touch_activity();
                                let pong = ServerMessage::Pong {
                                    version: metadata.version.clone(),
                                    pid: metadata.pid,
                                };
                                if write_ndjson_frame(&mut writer, &pong).await.is_err() {
                                    break;
                                }
                            }
                            ClientMessage::Status => {
                                let report = pool.status_report(&metadata, idle_timeout);
                                if write_ndjson_frame(
                                    &mut writer,
                                    &ServerMessage::StatusReport(report),
                                )
                                .await
                                .is_err()
                                {
                                    break;
                                }
                            }
                            ClientMessage::Shutdown => {
                                // Release any lease held on this connection first to avoid blocking
                                // our own shutdown sequence.
                                cleanup_connection_state(&mut active_rpc, &mut lease, false).await;
                                let (reply_tx, reply_rx) = oneshot::channel();
                                let stopped_sessions = if shutdown_tx.send(reply_tx).await.is_ok() {
                                    reply_rx.await.unwrap_or(0)
                                } else {
                                    0
                                };
                                let _ = write_ndjson_frame(
                                    &mut writer,
                                    &ServerMessage::ShutdownAck { stopped_sessions },
                                )
                                .await;
                                return;
                            }
                            ClientMessage::AcquireLease { ctx, session_key: _ } => {
                                if lease.is_some() {
                                    let _ = write_ndjson_frame(
                                        &mut writer,
                                        &ServerMessage::ProtocolError {
                                            message: "connection already holds an active lease".to_string(),
                                        },
                                    )
                                    .await;
                                    continue;
                                }
                                let driver_ctx = ctx.into_driver_context();
                                let acquire_cancel = shutdown_token.child_token();
                                let acquire_res = tokio::select! {
                                    biased;
                                    res = pool.acquire_lease(&driver_ctx, &acquire_cancel) => Some(res),
                                    // Detect abrupt client disconnect while waiting in the per-device queue!
                                    eof_or_msg = reader.next_frame::<ClientMessage>() => {
                                        acquire_cancel.cancel();
                                        let _ = eof_or_msg;
                                        None
                                    }
                                };
                                let Some(acquire_res) = acquire_res else {
                                    break;
                                };
                                match acquire_res {
                                    Ok(acquired) => {
                                        let driver = acquired.driver();
                                        let msg = ServerMessage::LeaseAcquired {
                                            lease_id: acquired.lease_id(),
                                            platform: driver.platform().to_string(),
                                            capabilities: driver.capabilities().iter().collect(),
                                            session_key: acquired.session_key().to_string(),
                                            warm_hit: acquired.warm_hit(),
                                        };
                                        lease = Some(acquired);
                                        if write_ndjson_frame(&mut writer, &msg).await.is_err() {
                                            break;
                                        }
                                    }
                                    Err(err) => {
                                        if write_ndjson_frame(
                                            &mut writer,
                                            &ServerMessage::FactoryError(err.into()),
                                        )
                                        .await
                                        .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                            }
                            ClientMessage::Rpc { id, op } => {
                                let Some(ref held_lease) = lease else {
                                    let _ = write_ndjson_frame(
                                        &mut writer,
                                        &ServerMessage::RpcResult {
                                            id,
                                            result: WireDriverResult::Err {
                                                error: WireDriverError::SessionClosed {
                                                    message: "no active driver lease on connection".to_string(),
                                                },
                                            },
                                        },
                                    )
                                    .await;
                                    continue;
                                };

                                // If a previous RPC was already cancelled via `CancelRpc` and is
                                // winding down, drain it cleanly before launching the next RPC.
                                if let Some(prev) = active_rpc.take() {
                                    if prev.cancel.is_cancelled() {
                                        match tokio::time::timeout(
                                            ACTIVE_RPC_DRAIN_TIMEOUT,
                                            prev.handle,
                                        )
                                        .await
                                        {
                                            Ok(Ok(prev_res)) => {
                                                if write_ndjson_frame(
                                                    &mut writer,
                                                    &ServerMessage::RpcResult {
                                                        id: prev.id,
                                                        result: prev_res,
                                                    },
                                                )
                                                .await
                                                .is_err()
                                                {
                                                    break;
                                                }
                                            }
                                            Ok(Err(_)) | Err(_) => {
                                                held_lease.mark_dirty();
                                                let _ = write_ndjson_frame(
                                                    &mut writer,
                                                    &ServerMessage::RpcResult {
                                                        id,
                                                        result: WireDriverResult::Err {
                                                            error: WireDriverError::SessionClosed {
                                                                message: "previous cancelled RPC failed to stop within drain timeout".to_string(),
                                                            },
                                                        },
                                                    },
                                                )
                                                .await;
                                                break;
                                            }
                                        }
                                    } else {
                                        active_rpc = Some(prev);
                                        let _ = write_ndjson_frame(
                                            &mut writer,
                                            &ServerMessage::RpcResult {
                                                id,
                                                result: WireDriverResult::Err {
                                                    error: WireDriverError::InvalidArgument {
                                                        message: "at most one concurrent RPC is allowed per lease".to_string(),
                                                    },
                                                },
                                            },
                                        )
                                        .await;
                                        continue;
                                    }
                                }

                                pool.touch_activity();
                                let driver = held_lease.driver();
                                let rpc_cancel = shutdown_token.child_token();
                                let rpc_cancel_for_task = rpc_cancel.clone();
                                let handle = tokio::spawn(async move {
                                    dispatch_driver_op(driver, op, rpc_cancel_for_task).await
                                });
                                active_rpc = Some(ActiveRpc {
                                    id,
                                    cancel: rpc_cancel,
                                    handle,
                                });
                            }
                            ClientMessage::CancelRpc { id } => {
                                if let Some(ref rpc) = active_rpc
                                    && rpc.id == id
                                {
                                    rpc.cancel.cancel();
                                }
                            }
                            ClientMessage::LeaseRelease => {
                                // Drain any still-active RPC cleanly (bounded 5s); if it won't stop,
                                // mark the lease dirty so the driver is evicted on release.
                                if let Some(rpc) = active_rpc.take() {
                                    rpc.cancel.cancel();
                                    match tokio::time::timeout(ACTIVE_RPC_DRAIN_TIMEOUT, rpc.handle)
                                        .await
                                    {
                                        Ok(Ok(res)) => {
                                            let _ = write_ndjson_frame(
                                                &mut writer,
                                                &ServerMessage::RpcResult {
                                                    id: rpc.id,
                                                    result: res,
                                                },
                                            )
                                            .await;
                                        }
                                        Ok(Err(_)) | Err(_) => {
                                            if let Some(ref l) = lease {
                                                l.mark_dirty();
                                            }
                                        }
                                    }
                                }
                                if let Some(l) = lease.take() {
                                    l.release_clean().await;
                                }
                                let _ = write_ndjson_frame(
                                    &mut writer,
                                    &ServerMessage::LeaseReleased,
                                )
                                .await;
                                return;
                            }
                        }
                    }
                }
            }
        }
    }

    // Connection ended without `LeaseRelease` (abrupt disconnect, framing error, or daemon shutdown).
    cleanup_connection_state(&mut active_rpc, &mut lease, true).await;
}

async fn cleanup_connection_state(
    active_rpc: &mut Option<ActiveRpc>,
    lease: &mut Option<LeaseGuard>,
    evict_held_lease: bool,
) {
    if let Some(rpc) = active_rpc.take() {
        rpc.cancel.cancel();
        let _ = tokio::time::timeout(ACTIVE_RPC_DRAIN_TIMEOUT, rpc.handle).await;
        if let Some(l) = lease.as_ref() {
            l.mark_dirty();
        }
    }
    if let Some(l) = lease.take() {
        if evict_held_lease || l.is_dirty() {
            l.evict_dirty().await;
        } else {
            l.release_clean().await;
        }
    }
}

async fn dispatch_driver_op(
    driver: Arc<dyn PlatformDriver>,
    op: DriverOp,
    cancel: CancellationToken,
) -> WireDriverResult {
    let res = match op {
        DriverOp::EnsureSession => driver
            .ensure_session(&cancel)
            .await
            .map(|()| WireDriverValue::Unit),
        DriverOp::LaunchApp { app_id, opts } => driver
            .launch_app(&app_id, &opts, &cancel)
            .await
            .map(|()| WireDriverValue::Unit),
        DriverOp::StopApp { app_id } => driver
            .stop_app(&app_id, &cancel)
            .await
            .map(|()| WireDriverValue::Unit),
        DriverOp::KillApp { app_id } => driver
            .kill_app(&app_id, &cancel)
            .await
            .map(|()| WireDriverValue::Unit),
        DriverOp::ClearState { app_id } => driver
            .clear_state(&app_id, &cancel)
            .await
            .map(|()| WireDriverValue::Unit),
        DriverOp::ClearKeychain => driver
            .clear_keychain(&cancel)
            .await
            .map(|()| WireDriverValue::Unit),
        DriverOp::OpenLink { app_id, url, opts } => driver
            .open_link(app_id.as_deref(), &url, &opts, &cancel)
            .await
            .map(|()| WireDriverValue::Unit),
        DriverOp::AddMedia { paths } => driver
            .add_media(&paths, &cancel)
            .await
            .map(|()| WireDriverValue::Unit),
        DriverOp::Hierarchy {
            app_id,
            include_system_ui,
            known_hash,
        } => driver
            .hierarchy(&app_id, include_system_ui, known_hash.as_deref(), &cancel)
            .await
            .map(WireDriverValue::Hierarchy),
        DriverOp::TapPoint { app_id, point } => driver
            .tap_point(&app_id, point, &cancel)
            .await
            .map(|()| WireDriverValue::Unit),
        DriverOp::LongPressPoint {
            app_id,
            point,
            duration_ms,
        } => driver
            .long_press_point(&app_id, point, duration_ms, &cancel)
            .await
            .map(|()| WireDriverValue::Unit),
        DriverOp::DoubleTapPoint { app_id, point } => driver
            .double_tap_point(&app_id, point, &cancel)
            .await
            .map(|()| WireDriverValue::Unit),
        DriverOp::Swipe { params } => driver
            .swipe(&params, &cancel)
            .await
            .map(|()| WireDriverValue::Unit),
        DriverOp::InputText { app_id, text } => driver
            .input_text(&app_id, &text, &cancel)
            .await
            .map(|()| WireDriverValue::Unit),
        DriverOp::EraseText { app_id, count } => driver
            .erase_text(&app_id, count, &cancel)
            .await
            .map(|()| WireDriverValue::Unit),
        DriverOp::PressKey { app_id, key } => driver
            .press_key(&app_id, &key, &cancel)
            .await
            .map(|()| WireDriverValue::Unit),
        DriverOp::HideKeyboard { app_id } => driver
            .hide_keyboard(&app_id, &cancel)
            .await
            .map(|()| WireDriverValue::Unit),
        DriverOp::Back { app_id } => driver
            .back(&app_id, &cancel)
            .await
            .map(|()| WireDriverValue::Unit),
        DriverOp::TakeScreenshot { app_id } => driver
            .take_screenshot(&app_id, &cancel)
            .await
            .map(|bytes| WireDriverValue::ScreenshotHex(hex::encode(bytes))),
        DriverOp::CheckAppCrash {
            app_id,
            since_epoch_ms,
        } => driver
            .check_app_crash(&app_id, since_epoch_ms, &cancel)
            .await
            .map(WireDriverValue::CrashSignal),
        DriverOp::ActAndObserve { req } => driver
            .act_and_observe(&req, &cancel)
            .await
            .map(WireDriverValue::ActOutcome),
        DriverOp::WaitForSelector { req } => {
            let local_req = (*req).into_request();
            driver
                .wait_for_selector(&local_req, &cancel)
                .await
                .map(|outcome| {
                    WireDriverValue::WaitSelectorOutcome(Box::new(WireWaitSelectorOutcome::from(
                        outcome,
                    )))
                })
        }
    };
    WireDriverResult::from_result(res)
}
