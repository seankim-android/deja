//! Cancel-safe multiplexed Unix-socket daemon client, [`DaemonDriver`] (`PlatformDriver` proxy),
//! [`DaemonDriverFactory`], and background daemon auto-starter.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use deja_core::provider_key_redactor;
use deja_flow::{
    ActOutcome, ActPayload, DriverCapabilities, DriverCrashSignal, DriverError, DriverFuture,
    KeyPress, LaunchOptions, OpenLinkOptions, PlatformDriver, SwipeParams, UiHierarchy, UiPoint,
    VisibleContext, WaitSelectorOutcome, WaitSelectorRequest, build_visible_context,
};
use tokio::net::UnixStream;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::daemon::paths::{
    DaemonMetadata, DaemonPathError, DaemonPaths, connect_verified, ensure_daemon_dir_secure,
    ensure_daemon_tmp_dir_secure, probe_daemon_lock_held,
};
use crate::daemon::pool::compute_session_key;
use crate::daemon::protocol::{
    BoundedNdjsonReader, ClientMessage, DaemonStatusReport, DriverOp, ServerMessage,
    WireDriverContext, WireDriverResult, WireDriverValue, WireWaitSelectorRequest,
    write_ndjson_frame,
};
use crate::daemon::{ALLOWED_DAEMON_SPAWN_ENV_KEYS, DEFAULT_IDLE_TIMEOUT_SECS, is_no_daemon_env};
use crate::driver_factory::{
    DriverContext, DriverFactory, DriverFactoryError, FactoryFuture, SharedStagedBuild,
};

const AUTO_START_POLL_TIMEOUT: Duration = Duration::from_secs(5);
const AUTO_START_POLL_INTERVAL: Duration = Duration::from_millis(20);
const CONTROL_RPC_TIMEOUT: Duration = Duration::from_secs(12);
const CANCEL_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Result returned when starting or connecting to a background daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedDaemonInfo {
    pub started: bool,
    pub already_running: bool,
    pub pid: u32,
    pub version: String,
    pub socket_path: String,
    pub idle_timeout_secs: u64,
}

/// Shared connection dispatch state protected by a SINGLE mutex so `closed` checks and
/// `oneshot::Sender` registration are strictly atomic — eliminating any wakeup race during
/// reader EOF or writer failure.
struct ClientDispatchState {
    closed: bool,
    pending_rpc: HashMap<u64, oneshot::Sender<WireDriverResult>>,
    pending_control: Option<oneshot::Sender<ServerMessage>>,
}

impl ClientDispatchState {
    fn new() -> Self {
        Self {
            closed: false,
            pending_rpc: HashMap::new(),
            pending_control: None,
        }
    }

    fn close_and_wake_all(&mut self) {
        self.closed = true;
        self.pending_rpc.clear();
        let _ = self.pending_control.take();
    }
}

/// Cancel-safe multiplexed client speaking NDJSON over a verified Unix socket to `deja daemon`.
pub struct DaemonClient {
    meta: DaemonMetadata,
    write_tx: mpsc::UnboundedSender<ClientMessage>,
    dispatch: Arc<StdMutex<ClientDispatchState>>,
    prev_cancelled_rx: Arc<StdMutex<Option<oneshot::Receiver<WireDriverResult>>>>,
    rpc_lock: AsyncMutex<()>,
    next_rpc_id: AtomicU64,
    reader_task: JoinHandle<()>,
    writer_task: JoinHandle<()>,
}

struct PendingRpcGuard<'a> {
    id: u64,
    client: &'a DaemonClient,
    rx: Option<oneshot::Receiver<WireDriverResult>>,
    disarmed: bool,
}

impl<'a> PendingRpcGuard<'a> {
    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for PendingRpcGuard<'_> {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        // Immediately ask the daemon to cancel this in-flight RPC without corrupting framing,
        // and stash `rx` so the next RPC or `LeaseRelease` drains the late `RpcResult` cleanly.
        let _ = self
            .client
            .write_tx
            .send(ClientMessage::CancelRpc { id: self.id });
        if let Some(rx) = self.rx.take() {
            let mut slot = self
                .client
                .prev_cancelled_rx
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            *slot = Some(rx);
        }
    }
}

impl Drop for DaemonClient {
    fn drop(&mut self) {
        {
            let mut state = self.dispatch.lock().unwrap_or_else(|p| p.into_inner());
            state.close_and_wake_all();
        }
        self.reader_task.abort();
        self.writer_task.abort();
    }
}

impl DaemonClient {
    /// Connect to an already-running daemon after passing all pre-connect and `peer_cred` checks.
    pub async fn connect(paths: &DaemonPaths) -> Result<Self, DaemonPathError> {
        let (stream, meta) = connect_verified(paths).await?;
        Ok(Self::from_stream(stream, meta))
    }

    fn from_stream(stream: UnixStream, meta: DaemonMetadata) -> Self {
        let (read_half, mut write_half) = stream.into_split();
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<ClientMessage>();
        let dispatch = Arc::new(StdMutex::new(ClientDispatchState::new()));

        let dispatch_w = Arc::clone(&dispatch);
        let writer_task = tokio::spawn(async move {
            while let Some(msg) = write_rx.recv().await {
                if write_ndjson_frame(&mut write_half, &msg).await.is_err() {
                    let mut state = dispatch_w.lock().unwrap_or_else(|p| p.into_inner());
                    state.close_and_wake_all();
                    return;
                }
            }
            let mut state = dispatch_w.lock().unwrap_or_else(|p| p.into_inner());
            state.close_and_wake_all();
        });

        let dispatch_r = Arc::clone(&dispatch);
        let reader_task = tokio::spawn(async move {
            let mut reader = BoundedNdjsonReader::new(read_half);
            while let Ok(Some(msg)) = reader.next_frame::<ServerMessage>().await {
                match msg {
                    ServerMessage::RpcResult { id, result } => {
                        let tx = {
                            let mut state = dispatch_r.lock().unwrap_or_else(|p| p.into_inner());
                            state.pending_rpc.remove(&id)
                        };
                        if let Some(tx) = tx {
                            let _ = tx.send(result);
                        }
                    }
                    other => {
                        let tx = {
                            let mut state = dispatch_r.lock().unwrap_or_else(|p| p.into_inner());
                            state.pending_control.take()
                        };
                        if let Some(tx) = tx {
                            let _ = tx.send(other);
                        }
                    }
                }
            }
            // Atomically mark closed and drop all pending senders so no caller can hang.
            let mut state = dispatch_r.lock().unwrap_or_else(|p| p.into_inner());
            state.close_and_wake_all();
        });

        Self {
            meta,
            write_tx,
            dispatch,
            prev_cancelled_rx: Arc::new(StdMutex::new(None)),
            rpc_lock: AsyncMutex::new(()),
            next_rpc_id: AtomicU64::new(1),
            reader_task,
            writer_task,
        }
    }

    #[must_use]
    pub fn metadata(&self) -> &DaemonMetadata {
        &self.meta
    }

    async fn send_control(
        &self,
        msg: ClientMessage,
        cancel: &CancellationToken,
        timeout: Option<Duration>,
    ) -> Result<ServerMessage, DriverError> {
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }

        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.dispatch.lock().unwrap_or_else(|p| p.into_inner());
            if state.closed {
                return Err(DriverError::SessionClosed(
                    "daemon connection is closed".to_string(),
                ));
            }
            state.pending_control = Some(tx);
            if self.write_tx.send(msg).is_err() {
                state.close_and_wake_all();
                return Err(DriverError::SessionClosed(
                    "daemon writer channel closed".to_string(),
                ));
            }
        }

        let recv_res = match timeout {
            Some(dur) => tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let mut state = self.dispatch.lock().unwrap_or_else(|p| p.into_inner());
                    let _ = state.pending_control.take();
                    return Err(DriverError::Cancelled);
                }
                timed = tokio::time::timeout(dur, rx) => match timed {
                    Ok(inner) => inner,
                    Err(_) => {
                        let mut state = self.dispatch.lock().unwrap_or_else(|p| p.into_inner());
                        let _ = state.pending_control.take();
                        return Err(DriverError::Timeout {
                            operation: "daemon_control".to_string(),
                            elapsed_ms: dur.as_millis() as u64,
                            detail: "timed out waiting for daemon response".to_string(),
                        });
                    }
                },
            },
            None => tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let mut state = self.dispatch.lock().unwrap_or_else(|p| p.into_inner());
                    let _ = state.pending_control.take();
                    return Err(DriverError::Cancelled);
                }
                inner = rx => inner,
            },
        };

        recv_res.map_err(|_| {
            DriverError::SessionClosed("daemon closed connection before replying".to_string())
        })
    }

    /// Send `Ping` and return `(version, pid)`.
    pub async fn ping(&self, cancel: &CancellationToken) -> Result<(String, u32), DriverError> {
        match self
            .send_control(ClientMessage::Ping, cancel, Some(CONTROL_RPC_TIMEOUT))
            .await?
        {
            ServerMessage::Pong { version, pid } => Ok((version, pid)),
            ServerMessage::ProtocolError { message } => Err(DriverError::Transport {
                operation: "daemon_ping".to_string(),
                detail: message,
                transient: false,
            }),
            other => Err(DriverError::Transport {
                operation: "daemon_ping".to_string(),
                detail: format!("unexpected daemon response: {other:?}"),
                transient: false,
            }),
        }
    }

    /// Query live daemon status and warm pooled session metrics.
    pub async fn status(
        &self,
        cancel: &CancellationToken,
    ) -> Result<DaemonStatusReport, DriverError> {
        match self
            .send_control(ClientMessage::Status, cancel, Some(CONTROL_RPC_TIMEOUT))
            .await?
        {
            ServerMessage::StatusReport(report) => Ok(report),
            ServerMessage::ProtocolError { message } => Err(DriverError::Transport {
                operation: "daemon_status".to_string(),
                detail: message,
                transient: false,
            }),
            other => Err(DriverError::Transport {
                operation: "daemon_status".to_string(),
                detail: format!("unexpected daemon response: {other:?}"),
                transient: false,
            }),
        }
    }

    /// Request graceful daemon shutdown and return the number of pooled drivers stopped.
    pub async fn shutdown_daemon(&self, cancel: &CancellationToken) -> Result<usize, DriverError> {
        match self
            .send_control(ClientMessage::Shutdown, cancel, Some(CONTROL_RPC_TIMEOUT))
            .await?
        {
            ServerMessage::ShutdownAck { stopped_sessions } => Ok(stopped_sessions),
            ServerMessage::ProtocolError { message } => Err(DriverError::Transport {
                operation: "daemon_shutdown".to_string(),
                detail: message,
                transient: false,
            }),
            other => Err(DriverError::Transport {
                operation: "daemon_shutdown".to_string(),
                detail: format!("unexpected daemon response: {other:?}"),
                transient: false,
            }),
        }
    }

    /// Acquire an exclusive device lease from the daemon (blocking in the device queue if needed
    /// until acquired or `cancel` fires).
    pub async fn acquire_lease(
        &self,
        ctx: &DriverContext,
        cancel: &CancellationToken,
    ) -> Result<(u64, String, DriverCapabilities, String, bool), DriverFactoryError> {
        let wire_ctx = WireDriverContext::from_driver_context(ctx);
        let session_key = compute_session_key(&wire_ctx.clone().into_driver_context());
        let reply = self
            .send_control(
                ClientMessage::AcquireLease {
                    ctx: wire_ctx,
                    session_key,
                },
                cancel,
                None,
            )
            .await
            .map_err(|err| match err {
                DriverError::Cancelled => DriverFactoryError::Cancelled,
                other => DriverFactoryError::Runtime(other.to_string()),
            })?;

        match reply {
            ServerMessage::LeaseAcquired {
                lease_id,
                platform,
                capabilities,
                session_key,
                warm_hit,
            } => Ok((
                lease_id,
                platform,
                DriverCapabilities::with(capabilities),
                session_key,
                warm_hit,
            )),
            ServerMessage::FactoryError(wire_err) => Err(wire_err.into()),
            ServerMessage::ProtocolError { message } => Err(DriverFactoryError::Runtime(message)),
            other => Err(DriverFactoryError::Runtime(format!(
                "unexpected daemon response during AcquireLease: {other:?}"
            ))),
        }
    }

    async fn drain_previous_cancelled(
        &self,
        cancel: &CancellationToken,
    ) -> Result<(), DriverError> {
        let prev = {
            let mut slot = self
                .prev_cancelled_rx
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            slot.take()
        };
        if let Some(rx) = prev {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                _ = tokio::time::timeout(CANCEL_DRAIN_TIMEOUT, rx) => {}
            }
        }
        Ok(())
    }

    /// Execute one cancel-safe driver RPC over this leased connection.
    ///
    /// If the returned future is dropped mid-flight (for example by `tokio::time::timeout` in
    /// `wait_local`), [`PendingRpcGuard`] sends `CancelRpc { id }` and drains the stale response
    /// before any subsequent RPC without corrupting the NDJSON stream.
    pub async fn call_rpc(
        &self,
        op: DriverOp,
        cancel: &CancellationToken,
    ) -> Result<WireDriverValue, DriverError> {
        if cancel.is_cancelled() {
            return Err(DriverError::Cancelled);
        }

        let _rpc_guard = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(DriverError::Cancelled),
            g = self.rpc_lock.lock() => g,
        };
        self.drain_previous_cancelled(cancel).await?;

        let id = self.next_rpc_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.dispatch.lock().unwrap_or_else(|p| p.into_inner());
            if state.closed {
                return Err(DriverError::SessionClosed(
                    "daemon connection is closed".to_string(),
                ));
            }
            state.pending_rpc.insert(id, tx);
            if self.write_tx.send(ClientMessage::Rpc { id, op }).is_err() {
                state.close_and_wake_all();
                return Err(DriverError::SessionClosed(
                    "daemon writer channel closed".to_string(),
                ));
            }
        }

        let mut guard = PendingRpcGuard {
            id,
            client: self,
            rx: Some(rx),
            disarmed: false,
        };

        let rx_ref = guard.rx.as_mut().expect("rx present");
        let res = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // Dropping `guard` sends `CancelRpc { id }` immediately.
                return Err(DriverError::Cancelled);
            }
            outcome = rx_ref => {
                guard.disarm();
                outcome
            }
        };

        match res {
            Ok(wire_res) => wire_res.into_result(),
            Err(_) => Err(DriverError::SessionClosed(
                "daemon connection closed during driver RPC".to_string(),
            )),
        }
    }

    /// Cleanly release the held driver lease so the underlying driver remains WARM in the daemon.
    pub async fn release_lease(&self) -> Result<(), DriverError> {
        let cancel = CancellationToken::new();
        let _rpc_guard = self.rpc_lock.lock().await;
        let _ = self.drain_previous_cancelled(&cancel).await;
        {
            let state = self.dispatch.lock().unwrap_or_else(|p| p.into_inner());
            if state.closed {
                return Ok(());
            }
        }
        match self
            .send_control(
                ClientMessage::LeaseRelease,
                &cancel,
                Some(Duration::from_secs(5)),
            )
            .await
        {
            Ok(ServerMessage::LeaseReleased) | Err(DriverError::SessionClosed(_)) => Ok(()),
            Ok(other) => Err(DriverError::Transport {
                operation: "lease_release".to_string(),
                detail: format!("unexpected daemon response on LeaseRelease: {other:?}"),
                transient: false,
            }),
            Err(err) => Err(err),
        }
    }
}

fn static_platform_str(raw: &str) -> &'static str {
    match raw {
        "ios" => "ios",
        "android" => "android",
        "web" => "web",
        "mock" => "mock",
        _ => "unknown",
    }
}

/// [`PlatformDriver`] proxy backed by a leased warm driver inside `deja daemon`.
///
/// Calling [`PlatformDriver::shutdown`] sends `LeaseRelease` and closes the client socket while
/// keeping the underlying pooled driver WARM inside the daemon for subsequent CLI runs.
pub struct DaemonDriver {
    client: Arc<DaemonClient>,
    platform: &'static str,
    capabilities: DriverCapabilities,
    lease_id: u64,
    session_key: String,
    warm_hit: bool,
    released: AtomicBool,
}

impl DaemonDriver {
    #[must_use]
    pub fn new(
        client: DaemonClient,
        platform: &str,
        capabilities: DriverCapabilities,
        lease_id: u64,
        session_key: String,
        warm_hit: bool,
    ) -> Self {
        Self {
            client: Arc::new(client),
            platform: static_platform_str(platform),
            capabilities,
            lease_id,
            session_key,
            warm_hit,
            released: AtomicBool::new(false),
        }
    }

    #[must_use]
    pub const fn lease_id(&self) -> u64 {
        self.lease_id
    }

    #[must_use]
    pub fn session_key(&self) -> &str {
        &self.session_key
    }

    #[must_use]
    pub const fn warm_hit(&self) -> bool {
        self.warm_hit
    }

    fn expect_unit(op: &str, val: WireDriverValue) -> Result<(), DriverError> {
        match val {
            WireDriverValue::Unit => Ok(()),
            other => Err(DriverError::Transport {
                operation: op.to_string(),
                detail: format!("unexpected daemon RPC payload for {op}: {other:?}"),
                transient: false,
            }),
        }
    }
}

impl PlatformDriver for DaemonDriver {
    fn platform(&self) -> &'static str {
        self.platform
    }

    fn capabilities(&self) -> &DriverCapabilities {
        &self.capabilities
    }

    fn is_daemon_backed(&self) -> bool {
        true
    }

    fn ensure_session<'a>(&'a self, cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let val = self
                .client
                .call_rpc(DriverOp::EnsureSession, cancel)
                .await?;
            Self::expect_unit("ensure_session", val)
        })
    }

    fn launch_app<'a>(
        &'a self,
        app_id: &'a str,
        opts: &'a LaunchOptions,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        let opts = opts.clone();
        Box::pin(async move {
            let val = self
                .client
                .call_rpc(DriverOp::LaunchApp { app_id, opts }, cancel)
                .await?;
            Self::expect_unit("launch_app", val)
        })
    }

    fn stop_app<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            let val = self
                .client
                .call_rpc(DriverOp::StopApp { app_id }, cancel)
                .await?;
            Self::expect_unit("stop_app", val)
        })
    }

    fn kill_app<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            let val = self
                .client
                .call_rpc(DriverOp::KillApp { app_id }, cancel)
                .await?;
            Self::expect_unit("kill_app", val)
        })
    }

    fn clear_state<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            let val = self
                .client
                .call_rpc(DriverOp::ClearState { app_id }, cancel)
                .await?;
            Self::expect_unit("clear_state", val)
        })
    }

    fn clear_keychain<'a>(&'a self, cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let val = self
                .client
                .call_rpc(DriverOp::ClearKeychain, cancel)
                .await?;
            Self::expect_unit("clear_keychain", val)
        })
    }

    fn open_link<'a>(
        &'a self,
        app_id: Option<&'a str>,
        url: &'a str,
        opts: &'a OpenLinkOptions,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.map(str::to_string);
        let url = url.to_string();
        let opts = opts.clone();
        Box::pin(async move {
            let val = self
                .client
                .call_rpc(DriverOp::OpenLink { app_id, url, opts }, cancel)
                .await?;
            Self::expect_unit("open_link", val)
        })
    }

    fn add_media<'a>(
        &'a self,
        paths: &'a [PathBuf],
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let paths = paths.to_vec();
        Box::pin(async move {
            let val = self
                .client
                .call_rpc(DriverOp::AddMedia { paths }, cancel)
                .await?;
            Self::expect_unit("add_media", val)
        })
    }

    fn hierarchy<'a>(
        &'a self,
        app_id: &'a str,
        include_system_ui: bool,
        known_hash: Option<&'a str>,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, UiHierarchy> {
        let app_id = app_id.to_string();
        let known_hash = known_hash.map(str::to_string);
        Box::pin(async move {
            match self
                .client
                .call_rpc(
                    DriverOp::Hierarchy {
                        app_id,
                        include_system_ui,
                        known_hash,
                    },
                    cancel,
                )
                .await?
            {
                WireDriverValue::Hierarchy(h) => Ok(h),
                other => Err(DriverError::Transport {
                    operation: "hierarchy".to_string(),
                    detail: format!("unexpected daemon RPC payload for hierarchy: {other:?}"),
                    transient: false,
                }),
            }
        })
    }

    fn tap_point<'a>(
        &'a self,
        app_id: &'a str,
        point: UiPoint,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            let val = self
                .client
                .call_rpc(DriverOp::TapPoint { app_id, point }, cancel)
                .await?;
            Self::expect_unit("tap_point", val)
        })
    }

    fn long_press_point<'a>(
        &'a self,
        app_id: &'a str,
        point: UiPoint,
        duration_ms: Option<u64>,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            let val = self
                .client
                .call_rpc(
                    DriverOp::LongPressPoint {
                        app_id,
                        point,
                        duration_ms,
                    },
                    cancel,
                )
                .await?;
            Self::expect_unit("long_press_point", val)
        })
    }

    fn double_tap_point<'a>(
        &'a self,
        app_id: &'a str,
        point: UiPoint,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            let val = self
                .client
                .call_rpc(DriverOp::DoubleTapPoint { app_id, point }, cancel)
                .await?;
            Self::expect_unit("double_tap_point", val)
        })
    }

    fn swipe<'a>(
        &'a self,
        params: &'a SwipeParams,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let params = params.clone();
        Box::pin(async move {
            let val = self
                .client
                .call_rpc(DriverOp::Swipe { params }, cancel)
                .await?;
            Self::expect_unit("swipe", val)
        })
    }

    fn input_text<'a>(
        &'a self,
        app_id: &'a str,
        text: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        let text = text.to_string();
        Box::pin(async move {
            let val = self
                .client
                .call_rpc(DriverOp::InputText { app_id, text }, cancel)
                .await?;
            Self::expect_unit("input_text", val)
        })
    }

    fn erase_text<'a>(
        &'a self,
        app_id: &'a str,
        count: u32,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            let val = self
                .client
                .call_rpc(DriverOp::EraseText { app_id, count }, cancel)
                .await?;
            Self::expect_unit("erase_text", val)
        })
    }

    fn press_key<'a>(
        &'a self,
        app_id: &'a str,
        key: &'a KeyPress,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        let key = key.clone();
        Box::pin(async move {
            let val = self
                .client
                .call_rpc(DriverOp::PressKey { app_id, key }, cancel)
                .await?;
            Self::expect_unit("press_key", val)
        })
    }

    fn hide_keyboard<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            let val = self
                .client
                .call_rpc(DriverOp::HideKeyboard { app_id }, cancel)
                .await?;
            Self::expect_unit("hide_keyboard", val)
        })
    }

    fn back<'a>(&'a self, app_id: &'a str, cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            let val = self
                .client
                .call_rpc(DriverOp::Back { app_id }, cancel)
                .await?;
            Self::expect_unit("back", val)
        })
    }

    fn take_screenshot<'a>(
        &'a self,
        app_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Vec<u8>> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            match self
                .client
                .call_rpc(DriverOp::TakeScreenshot { app_id }, cancel)
                .await?
            {
                WireDriverValue::ScreenshotHex(hex_str) => {
                    hex::decode(&hex_str).map_err(|err| DriverError::Transport {
                        operation: "take_screenshot".to_string(),
                        detail: format!("invalid hex screenshot payload from daemon: {err}"),
                        transient: false,
                    })
                }
                other => Err(DriverError::Transport {
                    operation: "take_screenshot".to_string(),
                    detail: format!("unexpected daemon RPC payload for take_screenshot: {other:?}"),
                    transient: false,
                }),
            }
        })
    }

    fn check_app_crash<'a>(
        &'a self,
        app_id: &'a str,
        since_epoch_ms: u64,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Option<DriverCrashSignal>> {
        let app_id = app_id.to_string();
        Box::pin(async move {
            match self
                .client
                .call_rpc(
                    DriverOp::CheckAppCrash {
                        app_id,
                        since_epoch_ms,
                    },
                    cancel,
                )
                .await?
            {
                WireDriverValue::CrashSignal(sig) => Ok(sig),
                other => Err(DriverError::Transport {
                    operation: "check_app_crash".to_string(),
                    detail: format!("unexpected daemon RPC payload for check_app_crash: {other:?}"),
                    transient: false,
                }),
            }
        })
    }

    fn shutdown<'a>(&'a self) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if self.released.swap(true, Ordering::SeqCst) {
                return Ok(());
            }
            self.client.release_lease().await
        })
    }

    fn act_and_observe<'a>(
        &'a self,
        req: &'a ActPayload,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ActOutcome> {
        let req = req.clone();
        Box::pin(async move {
            match self
                .client
                .call_rpc(DriverOp::ActAndObserve { req }, cancel)
                .await?
            {
                WireDriverValue::ActOutcome(out) => Ok(out),
                other => Err(DriverError::Transport {
                    operation: "act_and_observe".to_string(),
                    detail: format!("unexpected daemon RPC payload for act_and_observe: {other:?}"),
                    transient: false,
                }),
            }
        })
    }

    fn wait_for_selector<'a>(
        &'a self,
        req: &'a WaitSelectorRequest,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, WaitSelectorOutcome> {
        let has_redactor = req.redactor.is_some() || req.provider_redactor.is_some();
        let wire_req = Box::new(WireWaitSelectorRequest::from_request(req, has_redactor));
        Box::pin(async move {
            match self
                .client
                .call_rpc(DriverOp::WaitForSelector { req: wire_req }, cancel)
                .await?
            {
                WireDriverValue::WaitSelectorOutcome(wire_out) => {
                    let mut outcome: WaitSelectorOutcome = (*wire_out).into();
                    if has_redactor {
                        if let Some(ref h) = outcome.hierarchy {
                            outcome.visible_context = build_visible_context(
                                &h.nodes,
                                req.redactor.as_ref(),
                                req.provider_redactor.as_ref(),
                                VisibleContext::DEFAULT_LIMIT,
                            );
                        } else {
                            let default_provider =
                                provider_key_redactor(std::iter::empty::<(&str, &str)>());
                            let provider =
                                req.provider_redactor.as_ref().unwrap_or(&default_provider);
                            outcome.visible_context = outcome.visible_context.map_items(|item| {
                                let step1 = match req.redactor.as_ref() {
                                    Some(r) => r.redact(item),
                                    None => item.to_string(),
                                };
                                provider.redact(&step1)
                            });
                        }
                        if !req.return_hierarchy {
                            outcome.hierarchy = None;
                        }
                    }
                    Ok(outcome)
                }
                other => Err(DriverError::Transport {
                    operation: "wait_for_selector".to_string(),
                    detail: format!(
                        "unexpected daemon RPC payload for wait_for_selector: {other:?}"
                    ),
                    transient: false,
                }),
            }
        })
    }
}

/// Start the background warm daemon (or attach to an already-running one) and poll up to 5s until
/// `Ping` succeeds. Fails closed immediately on any security/permission error.
pub async fn ensure_background_daemon_started(
    paths: &DaemonPaths,
    env: &BTreeMap<String, String>,
    idle_timeout_secs: u64,
    cancel: &CancellationToken,
) -> Result<StartedDaemonInfo, DaemonPathError> {
    let exe = std::env::current_exe().map_err(DaemonPathError::Io)?;
    ensure_background_daemon_started_with_exe(
        paths,
        env,
        idle_timeout_secs,
        &exe,
        AUTO_START_POLL_TIMEOUT,
        cancel,
    )
    .await
}

/// Spawn a daemon process using an explicit executable path and poll deadline (shared by
/// production auto-start and deterministic concurrency tests).
pub async fn ensure_background_daemon_started_with_exe(
    paths: &DaemonPaths,
    env: &BTreeMap<String, String>,
    idle_timeout_secs: u64,
    exe: &std::path::Path,
    poll_timeout: Duration,
    cancel: &CancellationToken,
) -> Result<StartedDaemonInfo, DaemonPathError> {
    // Validate directory security and create stable `<daemon_dir>/tmp` (`0o700`) so the daemon
    // never inherits a short-lived caller's ephemeral `TMPDIR`.
    ensure_daemon_dir_secure(&paths.dir)?;
    let daemon_tmp_dir = ensure_daemon_tmp_dir_secure(paths)?;

    // Fast path: already running.
    match DaemonClient::connect(paths).await {
        Ok(client) => {
            if let Ok((version, pid)) = client.ping(cancel).await {
                let meta = client.metadata().clone();
                return Ok(StartedDaemonInfo {
                    started: false,
                    already_running: true,
                    pid,
                    version,
                    socket_path: meta.socket_path,
                    idle_timeout_secs: meta.idle_timeout_secs,
                });
            }
        }
        Err(err) if err.is_security_violation() => return Err(err),
        Err(_) => {}
    }

    let mut cmd = tokio::process::Command::new(exe);
    cmd.args([
        "daemon",
        "start",
        "--foreground",
        "--idle-timeout",
        &idle_timeout_secs.to_string(),
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .process_group(0)
    .kill_on_drop(false)
    .env_clear();

    for key in ALLOWED_DAEMON_SPAWN_ENV_KEYS {
        if let Some(val) = env.get(*key) {
            cmd.env(key, val);
        }
    }
    cmd.env("DEJA_DAEMON_DIR", &paths.dir);
    cmd.env("TMPDIR", &daemon_tmp_dir);

    let mut child = cmd.spawn().map_err(DaemonPathError::Io)?;
    let spawned_pid = child.id();
    let deadline = Instant::now() + poll_timeout;
    let mut child_exit_status: Option<std::process::ExitStatus> = None;

    while Instant::now() < deadline {
        if cancel.is_cancelled() {
            return Err(DaemonPathError::Io(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "cancelled while waiting for Déjà daemon startup",
            )));
        }

        match DaemonClient::connect(paths).await {
            Ok(client) => {
                if let Ok((version, pid)) = client.ping(cancel).await {
                    let meta = client.metadata().clone();
                    let already_running = spawned_pid.is_some_and(|sp| sp != pid);
                    return Ok(StartedDaemonInfo {
                        started: !already_running,
                        already_running,
                        pid,
                        version,
                        socket_path: meta.socket_path,
                        idle_timeout_secs: meta.idle_timeout_secs,
                    });
                }
            }
            Err(err) if err.is_security_violation() => return Err(err),
            Err(_) => {}
        }

        if child_exit_status.is_none()
            && let Ok(Some(status)) = child.try_wait()
        {
            child_exit_status = Some(status);
        }

        // If our child exited early, ONLY abort early when `daemon.lock` is ALSO unlocked.
        // If another concurrent starter won `daemon.lock` and is still publishing `deja.sock`,
        // keep polling until it is ready or the startup deadline expires.
        if let Some(status) = child_exit_status {
            match probe_daemon_lock_held(paths) {
                Ok(true) => {}
                Ok(false) => {
                    // One final connect check in case the winner already finished publishing &
                    // something transient happened.
                    if let Ok(client) = DaemonClient::connect(paths).await
                        && let Ok((version, pid)) = client.ping(cancel).await
                    {
                        let meta = client.metadata().clone();
                        return Ok(StartedDaemonInfo {
                            started: false,
                            already_running: true,
                            pid,
                            version,
                            socket_path: meta.socket_path,
                            idle_timeout_secs: meta.idle_timeout_secs,
                        });
                    }
                    return Err(DaemonPathError::Io(std::io::Error::other(format!(
                        "Déjà daemon exited prematurely during startup with status {status}"
                    ))));
                }
                Err(err) if err.is_security_violation() => return Err(err),
                Err(_) => {}
            }
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(DaemonPathError::Io(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "cancelled while waiting for Déjà daemon startup",
                )));
            }
            _ = tokio::time::sleep(AUTO_START_POLL_INTERVAL) => {}
        }
    }

    Err(DaemonPathError::Io(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!(
            "timed out waiting {}ms for Déjà daemon socket {}",
            poll_timeout.as_millis(),
            paths.socket.display()
        ),
    )))
}

/// [`DriverFactory`] wrapper that leases warm drivers from the persistent Unix-socket daemon while
/// delegating read-only `resolve_runtime`, `prepare_batch_build`, and `install_app` directly to
/// `inner`.
pub struct DaemonDriverFactory {
    inner: Arc<dyn DriverFactory>,
    default_env: BTreeMap<String, String>,
    daemon_dir_override: Option<PathBuf>,
    auto_start: bool,
    idle_timeout_secs: u64,
}

impl DaemonDriverFactory {
    /// Wrap `inner` in a daemon-backed [`DriverFactory`] using environment-resolved daemon paths.
    #[must_use]
    pub fn new(
        inner: Arc<dyn DriverFactory>,
        default_env: BTreeMap<String, String>,
        auto_start: bool,
    ) -> Self {
        Self {
            inner,
            default_env,
            daemon_dir_override: None,
            auto_start,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
        }
    }

    /// Wrap `inner` targeting an explicit daemon directory (ideal for deterministic tests).
    #[must_use]
    pub fn with_daemon_dir(
        inner: Arc<dyn DriverFactory>,
        daemon_dir: PathBuf,
        auto_start: bool,
    ) -> Self {
        Self {
            inner,
            default_env: BTreeMap::new(),
            daemon_dir_override: Some(daemon_dir),
            auto_start,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
        }
    }

    #[must_use]
    pub fn with_idle_timeout_secs(mut self, secs: u64) -> Self {
        self.idle_timeout_secs = secs.max(1);
        self
    }

    fn resolve_paths(&self, ctx: &DriverContext) -> Result<DaemonPaths, DriverFactoryError> {
        if let Some(ref explicit) = self.daemon_dir_override {
            return DaemonPaths::from_dir(explicit.clone())
                .map_err(|err| DriverFactoryError::UnsupportedOrConfig(err.to_string()));
        }
        let paths_res = DaemonPaths::resolve(&ctx.env).or_else(|_| {
            if !self.default_env.is_empty() {
                DaemonPaths::resolve(&self.default_env)
            } else {
                DaemonPaths::resolve(&ctx.env)
            }
        });
        paths_res.map_err(|err| DriverFactoryError::UnsupportedOrConfig(err.to_string()))
    }

    async fn connect_client(
        &self,
        paths: &DaemonPaths,
        ctx: &DriverContext,
        cancel: &CancellationToken,
    ) -> Result<DaemonClient, DriverFactoryError> {
        match DaemonClient::connect(paths).await {
            Ok(client) => return Ok(client),
            Err(err) if err.is_security_violation() => {
                return Err(DriverFactoryError::Runtime(err.to_string()));
            }
            Err(err) if !self.auto_start => {
                return Err(DriverFactoryError::Runtime(format!(
                    "failed to connect to Déjà daemon at {}: {err}",
                    paths.socket.display()
                )));
            }
            Err(_) => {}
        }

        let effective_env = if ctx.env.is_empty() {
            &self.default_env
        } else {
            &ctx.env
        };
        ensure_background_daemon_started(paths, effective_env, self.idle_timeout_secs, cancel)
            .await
            .map_err(|err| DriverFactoryError::Runtime(err.to_string()))?;

        DaemonClient::connect(paths)
            .await
            .map_err(|err| DriverFactoryError::Runtime(err.to_string()))
    }
}

impl DriverFactory for DaemonDriverFactory {
    fn resolve_runtime<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Option<String>, DriverFactoryError>> {
        // NEVER spawn or connect to the daemon for read-only runtime resolution.
        self.inner.resolve_runtime(ctx, cancel)
    }

    fn prepare_batch_build<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Option<SharedStagedBuild>, DriverFactoryError>> {
        // Delegate batch `.app` bundle staging directly without spawning the daemon.
        self.inner.prepare_batch_build(ctx, cancel)
    }

    fn create<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<Box<dyn PlatformDriver>, DriverFactoryError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverFactoryError::Cancelled);
            }
            if is_no_daemon_env(&ctx.env) {
                return self.inner.create(ctx, cancel).await;
            }
            // Validate locally BEFORE connecting/spawning so invalid configs fail closed exit 2
            // without ever spawning a background daemon.
            self.inner.validate_warm_context(ctx)?;

            // Ensure Web requests always carry the resolved Chromium runtime version so in-place
            // browser upgrades invalidate stale pooled WebDriver instances.
            let mut effective_ctx = ctx.clone();
            if effective_ctx.platform == "web" && effective_ctx.resolved_runtime.is_none() {
                effective_ctx.resolved_runtime =
                    self.inner.resolve_runtime(&effective_ctx, cancel).await?;
            }

            let paths = self.resolve_paths(&effective_ctx)?;
            for attempt in 0..2 {
                let client = self.connect_client(&paths, &effective_ctx, cancel).await?;
                match client.acquire_lease(&effective_ctx, cancel).await {
                    Ok((lease_id, platform, capabilities, session_key, warm_hit)) => {
                        return Ok(Box::new(DaemonDriver::new(
                            client,
                            &platform,
                            capabilities,
                            lease_id,
                            session_key,
                            warm_hit,
                        )) as Box<dyn PlatformDriver>);
                    }
                    Err(DriverFactoryError::Runtime(msg))
                        if attempt == 0
                            && self.auto_start
                            && (msg.contains("shutting down") || msg.contains("closed")) =>
                    {
                        // Daemon expired via idle TTL right as we connected; restart once cleanly.
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        continue;
                    }
                    Err(err) => return Err(err),
                }
            }
            Err(DriverFactoryError::Runtime(
                "failed to acquire driver lease from Déjà daemon".to_string(),
            ))
        })
    }

    fn install_app<'a>(
        &'a self,
        ctx: &'a DriverContext,
        cancel: &'a CancellationToken,
    ) -> FactoryFuture<'a, Result<(), DriverFactoryError>> {
        // Caller already holds the exclusive `DaemonDriver` device lease while installing.
        self.inner.install_app(ctx, cancel)
    }

    fn validate_warm_context(&self, ctx: &DriverContext) -> Result<(), DriverFactoryError> {
        self.inner.validate_warm_context(ctx)
    }

    fn unwrapped_without_daemon(&self) -> Option<Arc<dyn DriverFactory>> {
        Some(Arc::clone(&self.inner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_meta() -> DaemonMetadata {
        DaemonMetadata {
            version: "0.1.0".to_string(),
            pid: std::process::id(),
            uid: 0,
            socket_path: "/tmp/dummy.sock".to_string(),
            started_at: "2026-01-01T00:00:00Z".to_string(),
            idle_timeout_secs: 60,
            proc_start_time: None,
        }
    }

    #[tokio::test]
    async fn closed_connection_and_writer_failure_immediately_wake_callers() {
        // 1. Peer closes immediately -> `ping` and `call_rpc` wake immediately with SessionClosed,
        //    even if registered right around EOF.
        let (a, b) = UnixStream::pair().unwrap();
        let client = DaemonClient::from_stream(a, dummy_meta());
        drop(b);

        let cancel = CancellationToken::new();
        let ping_res = tokio::time::timeout(Duration::from_secs(2), client.ping(&cancel))
            .await
            .expect("ping must not hang on closed stream");
        assert!(matches!(ping_res, Err(DriverError::SessionClosed(_))));

        let rpc_res = tokio::time::timeout(
            Duration::from_secs(2),
            client.call_rpc(DriverOp::EnsureSession, &cancel),
        )
        .await
        .expect("call_rpc must not hang after connection closed");
        assert!(matches!(rpc_res, Err(DriverError::SessionClosed(_))));

        // 2. Peer shuts down its read half while keeping its write half open -> writer fails with
        //    EPIPE and MUST wake pending caller immediately without waiting for reader EOF!
        let (c, d) = UnixStream::pair().unwrap();
        let (d_read, _d_write_kept_open) = d.into_split();
        drop(d_read);
        let client2 = DaemonClient::from_stream(c, dummy_meta());

        // Oversized payload or broken pipe forces write failure; send an oversized input_text
        // (> 32 MiB) so writer immediately rejects FrameTooLarge and closes dispatch state!
        let huge = "x".repeat(crate::daemon::protocol::MAX_FRAME_BYTES + 16);
        let write_fail_res = tokio::time::timeout(
            Duration::from_secs(2),
            client2.call_rpc(
                DriverOp::InputText {
                    app_id: "app".to_string(),
                    text: huge,
                },
                &cancel,
            ),
        )
        .await
        .expect("writer failure must immediately wake pending RPC caller");
        assert!(matches!(write_fail_res, Err(DriverError::SessionClosed(_))));
    }
}
