//! Single-WebSocket multiplexed Chrome DevTools Protocol (CDP) client supporting both
//! browser-level calls and flat `sessionId`-scoped page calls (`Target.attachToTarget` with
//! `flatten: true`), RAII cancellation cleanup, event routing, and isolated `BrowserContext` setup.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::error::{Result, WebDriverError};
use crate::input::ScreenSize;

const MAX_WS_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_WS_FRAME_BYTES: usize = 16 * 1024 * 1024;
const WRITER_CHANNEL_CAPACITY: usize = 256;

/// Crash observation captured from CDP target crash events or unexpected browser process exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordedCrash {
    pub epoch_ms: u64,
    pub process: String,
    pub detail: String,
}

/// Returns current wall-clock epoch time in milliseconds.
#[must_use]
pub(crate) fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug)]
enum WriterCmd {
    Text(String),
    Pong(Vec<u8>),
    Close,
}

struct PendingEntry {
    method: String,
    tx: oneshot::Sender<Result<Value>>,
}

#[derive(Debug)]
struct NetworkTracker {
    in_flight: HashSet<String>,
    last_activity: Instant,
}

impl NetworkTracker {
    fn new() -> Self {
        Self {
            in_flight: HashSet::new(),
            last_activity: Instant::now(),
        }
    }
}

/// Shared CDP event & crash tracking state updated by the WebSocket reader task.
#[derive(Debug)]
pub(crate) struct CdpEventState {
    active_target_id: StdMutex<Option<String>>,
    active_session_id: StdMutex<Option<String>>,
    main_frame_id: StdMutex<Option<String>>,
    nav_seq: AtomicU64,
    nav_notify: Notify,
    load_seq: AtomicU64,
    load_notify: Notify,
    network: StdMutex<NetworkTracker>,
    network_notify: Notify,
    detached: AtomicBool,
    detached_notify: Notify,
    crash_sink: Arc<StdMutex<Option<RecordedCrash>>>,
}

impl CdpEventState {
    fn new(crash_sink: Arc<StdMutex<Option<RecordedCrash>>>) -> Self {
        Self {
            active_target_id: StdMutex::new(None),
            active_session_id: StdMutex::new(None),
            main_frame_id: StdMutex::new(None),
            nav_seq: AtomicU64::new(0),
            nav_notify: Notify::new(),
            load_seq: AtomicU64::new(0),
            load_notify: Notify::new(),
            network: StdMutex::new(NetworkTracker::new()),
            network_notify: Notify::new(),
            detached: AtomicBool::new(false),
            detached_notify: Notify::new(),
            crash_sink,
        }
    }

    pub(crate) fn set_active_target(&self, target_id: Option<String>, session_id: Option<String>) {
        {
            let mut guard = self
                .active_target_id
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            *guard = target_id;
        }
        {
            let mut guard = self
                .active_session_id
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            *guard = session_id;
        }
        self.set_main_frame_id(None);
        self.detached.store(false, Ordering::SeqCst);
        self.reset_network_state();
    }

    pub(crate) fn set_main_frame_id(&self, frame_id: Option<String>) {
        let mut guard = self.main_frame_id.lock().unwrap_or_else(|p| p.into_inner());
        *guard = frame_id;
    }

    pub(crate) fn reset_network_state(&self) {
        {
            let mut guard = self.network.lock().unwrap_or_else(|p| p.into_inner());
            guard.in_flight.clear();
            guard.last_activity = Instant::now();
        }
        self.network_notify.notify_waiters();
    }

    pub(crate) fn network_snapshot(&self) -> (usize, Instant) {
        let guard = self.network.lock().unwrap_or_else(|p| p.into_inner());
        (guard.in_flight.len(), guard.last_activity)
    }

    fn wake_all_waiters(&self) {
        self.detached_notify.notify_waiters();
        self.nav_notify.notify_waiters();
        self.load_notify.notify_waiters();
        self.network_notify.notify_waiters();
    }

    fn matches_active_session(&self, session_id: Option<&str>) -> bool {
        let guard = self
            .active_session_id
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        match (guard.as_deref(), session_id) {
            (Some(active), Some(ev)) => active == ev,
            _ => false,
        }
    }

    fn matches_active_target(&self, target_id: Option<&str>) -> bool {
        let guard = self
            .active_target_id
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        match (guard.as_deref(), target_id) {
            (Some(active), Some(ev)) => active == ev,
            _ => false,
        }
    }

    fn record_crash(&self, process: impl Into<String>, detail: impl Into<String>) {
        let crash = RecordedCrash {
            epoch_ms: now_epoch_ms(),
            process: process.into(),
            detail: detail.into(),
        };
        let mut guard = self.crash_sink.lock().unwrap_or_else(|p| p.into_inner());
        *guard = Some(crash);
    }

    fn handle_event(&self, method: &str, session_id: Option<&str>, params: &Value) {
        match method {
            "Inspector.targetCrashed" => {
                if session_id.is_none() || self.matches_active_session(session_id) {
                    debug!(session_id = ?session_id, "received CDP Inspector.targetCrashed");
                    self.record_crash(
                        "chromium-renderer",
                        "Chromium renderer target crashed (Inspector.targetCrashed)",
                    );
                    self.detached.store(true, Ordering::SeqCst);
                    self.wake_all_waiters();
                }
            }
            "Target.targetCrashed" => {
                let tid = params.get("targetId").and_then(Value::as_str);
                if self.matches_active_target(tid) {
                    let status = params
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("crashed");
                    let error_code = params.get("errorCode").and_then(Value::as_i64).unwrap_or(0);
                    debug!(
                        target_id = ?tid,
                        status,
                        error_code,
                        "received CDP Target.targetCrashed"
                    );
                    self.record_crash(
                        "chromium-renderer",
                        format!(
                            "Chromium target crashed (Target.targetCrashed: status={status}, errorCode={error_code})"
                        ),
                    );
                    self.detached.store(true, Ordering::SeqCst);
                    self.wake_all_waiters();
                }
            }
            "Target.detachedFromTarget" => {
                let detached_sid = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .or(session_id);
                if self.matches_active_session(detached_sid) {
                    debug!(session_id = ?detached_sid, "received CDP Target.detachedFromTarget");
                    self.detached.store(true, Ordering::SeqCst);
                    self.wake_all_waiters();
                }
            }
            "Page.frameNavigated" if self.matches_active_session(session_id) => {
                // Only main-frame navigations (`frame.parentId` absent or null) advance `nav_seq`.
                let frame = params.get("frame");
                let is_main_frame = frame
                    .and_then(|f| f.get("parentId"))
                    .is_none_or(Value::is_null);
                if is_main_frame {
                    if let Some(frame_id) = frame
                        .and_then(|f| f.get("id"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    {
                        self.set_main_frame_id(Some(frame_id.to_string()));
                    }
                    trace!(session_id = ?session_id, "received main-frame Page.frameNavigated");
                    self.nav_seq.fetch_add(1, Ordering::SeqCst);
                    self.nav_notify.notify_waiters();
                }
            }
            "Page.navigatedWithinDocument" if self.matches_active_session(session_id) => {
                // Only main-frame same-document navigations advance `nav_seq`.
                let event_frame_id = params.get("frameId").and_then(Value::as_str);
                let matches_main = {
                    let guard = self.main_frame_id.lock().unwrap_or_else(|p| p.into_inner());
                    match (guard.as_deref(), event_frame_id) {
                        (Some(main), Some(ev)) => main == ev,
                        (Some(_), None) => false,
                        (None, _) => true,
                    }
                };
                if matches_main {
                    trace!(
                        session_id = ?session_id,
                        frame_id = ?event_frame_id,
                        "received main-frame Page.navigatedWithinDocument"
                    );
                    self.nav_seq.fetch_add(1, Ordering::SeqCst);
                    self.nav_notify.notify_waiters();
                }
            }
            "Page.loadEventFired" | "Page.frameStoppedLoading"
                if self.matches_active_session(session_id) =>
            {
                trace!(method, session_id = ?session_id, "received page load event");
                self.load_seq.fetch_add(1, Ordering::SeqCst);
                self.load_notify.notify_waiters();
            }
            "Network.requestWillBeSent" if self.matches_active_session(session_id) => {
                let is_data_url = params
                    .get("request")
                    .and_then(|r| r.get("url"))
                    .and_then(Value::as_str)
                    .is_some_and(|u| u.starts_with("data:"));
                if !is_data_url
                    && let Some(req_id) = params.get("requestId").and_then(Value::as_str)
                {
                    {
                        let mut net = self.network.lock().unwrap_or_else(|p| p.into_inner());
                        net.in_flight.insert(req_id.to_string());
                        net.last_activity = Instant::now();
                    }
                    self.network_notify.notify_waiters();
                }
            }
            "Network.loadingFinished" | "Network.loadingFailed"
                if self.matches_active_session(session_id) =>
            {
                if let Some(req_id) = params.get("requestId").and_then(Value::as_str) {
                    {
                        let mut net = self.network.lock().unwrap_or_else(|p| p.into_inner());
                        net.in_flight.remove(req_id);
                        net.last_activity = Instant::now();
                    }
                    self.network_notify.notify_waiters();
                }
            }
            _ => {}
        }
    }
}

/// RAII guard removing an in-flight RPC `id` from `pending` if the future is dropped, cancelled,
/// or timed out before a response arrives.
struct PendingCallGuard<'a> {
    pending: &'a StdMutex<HashMap<u64, PendingEntry>>,
    id: u64,
    armed: bool,
}

impl Drop for PendingCallGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut guard = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            guard.remove(&self.id);
        }
    }
}

struct CdpClientInner {
    next_id: AtomicU64,
    tx: mpsc::Sender<WriterCmd>,
    pending: Arc<StdMutex<HashMap<u64, PendingEntry>>>,
    closed: Arc<AtomicBool>,
    close_reason: Arc<StdMutex<Option<String>>>,
    events: Arc<CdpEventState>,
    io_tasks: StdMutex<Option<(JoinHandle<()>, JoinHandle<()>)>>,
}

impl std::fmt::Debug for CdpClientInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CdpClientInner")
            .field("closed", &self.closed.load(Ordering::Relaxed))
            .finish()
    }
}

impl Drop for CdpClientInner {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some((writer, reader)) = self
            .io_tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            writer.abort();
            reader.abort();
        }
    }
}

/// Cloneable multiplexed Chromium CDP client over a single WebSocket connection.
#[derive(Debug, Clone)]
pub(crate) struct CdpClient {
    inner: Arc<CdpClientInner>,
}

impl CdpClient {
    /// Connects to a Chromium browser DevTools WebSocket endpoint and spawns dedicated reader and
    /// writer tasks.
    pub(crate) async fn connect(
        ws_url: &str,
        timeout: Duration,
        crash_sink: Arc<StdMutex<Option<RecordedCrash>>>,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        if cancel.is_cancelled() {
            return Err(WebDriverError::Cancelled);
        }
        let started = Instant::now();
        let mut ws_cfg = WebSocketConfig::default();
        ws_cfg.max_message_size = Some(MAX_WS_MESSAGE_BYTES);
        ws_cfg.max_frame_size = Some(MAX_WS_FRAME_BYTES);

        let (ws_stream, _resp) = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(WebDriverError::Cancelled),
            timed = tokio::time::timeout(
                timeout,
                connect_async_with_config(ws_url, Some(ws_cfg), true),
            ) => match timed {
                Ok(Ok(pair)) => pair,
                Ok(Err(err)) => return Err(WebDriverError::WebSocket(err.to_string())),
                Err(_) => {
                    return Err(WebDriverError::Timeout {
                        operation: "cdp_connect".to_string(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        detail: format!(
                            "timed out connecting to CDP WebSocket after {}ms",
                            timeout.as_millis()
                        ),
                    });
                }
            },
        };

        let (mut ws_sink, mut ws_source) = ws_stream.split();
        let (tx, mut rx) = mpsc::channel::<WriterCmd>(WRITER_CHANNEL_CAPACITY);
        let pending: Arc<StdMutex<HashMap<u64, PendingEntry>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let close_reason: Arc<StdMutex<Option<String>>> = Arc::new(StdMutex::new(None));
        let events = Arc::new(CdpEventState::new(crash_sink));

        let writer_closed = Arc::clone(&closed);
        let writer_reason = Arc::clone(&close_reason);
        let writer_pending = Arc::clone(&pending);
        let writer_events = Arc::clone(&events);
        let writer_task = tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let send_res = match cmd {
                    WriterCmd::Text(payload) => ws_sink.send(Message::Text(payload.into())).await,
                    WriterCmd::Pong(bytes) => ws_sink.send(Message::Pong(bytes.into())).await,
                    WriterCmd::Close => {
                        let _ = ws_sink.send(Message::Close(None)).await;
                        let _ = ws_sink.close().await;
                        break;
                    }
                };
                if let Err(err) = send_res {
                    mark_closed_and_drain(
                        &writer_closed,
                        &writer_reason,
                        &writer_pending,
                        &writer_events,
                        format!("WebSocket write failed: {err}"),
                    );
                    break;
                }
            }
        });

        let reader_tx = tx.clone();
        let reader_closed = Arc::clone(&closed);
        let reader_reason = Arc::clone(&close_reason);
        let reader_pending = Arc::clone(&pending);
        let reader_events = Arc::clone(&events);
        let reader_task = tokio::spawn(async move {
            let mut end_reason = "WebSocket stream closed".to_string();
            while let Some(item) = ws_source.next().await {
                match item {
                    Ok(Message::Text(txt)) => {
                        dispatch_incoming_text(txt.as_str(), &reader_pending, &reader_events);
                    }
                    Ok(Message::Binary(bin)) => {
                        if let Ok(txt) = std::str::from_utf8(&bin) {
                            dispatch_incoming_text(txt, &reader_pending, &reader_events);
                        }
                    }
                    Ok(Message::Ping(payload)) => {
                        let _ = reader_tx.try_send(WriterCmd::Pong(payload.to_vec()));
                    }
                    Ok(Message::Pong(_)) => {}
                    Ok(Message::Close(frame)) => {
                        end_reason = match frame {
                            Some(f) if !f.reason.is_empty() => {
                                format!("WebSocket closed ({}): {}", f.code, f.reason)
                            }
                            Some(f) => format!("WebSocket closed ({})", f.code),
                            None => "WebSocket closed by peer".to_string(),
                        };
                        break;
                    }
                    Ok(Message::Frame(_)) => {}
                    Err(err) => {
                        end_reason = format!("WebSocket read error: {err}");
                        break;
                    }
                }
            }
            mark_closed_and_drain(
                &reader_closed,
                &reader_reason,
                &reader_pending,
                &reader_events,
                end_reason,
            );
        });

        Ok(Self {
            inner: Arc::new(CdpClientInner {
                next_id: AtomicU64::new(1),
                tx,
                pending,
                closed,
                close_reason,
                events,
                io_tasks: StdMutex::new(Some((writer_task, reader_task))),
            }),
        })
    }

    /// Sends a browser-level CDP command (without `sessionId`).
    pub(crate) async fn call_browser(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.call_raw(None, method, params, timeout, cancel).await
    }

    /// Sends a flat session-scoped CDP command (with `sessionId`).
    pub(crate) async fn call_session(
        &self,
        session_id: &str,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.call_raw(Some(session_id), method, params, timeout, cancel)
            .await
    }

    async fn call_raw(
        &self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        if cancel.is_cancelled() {
            return Err(WebDriverError::Cancelled);
        }
        if self.is_closed() {
            return Err(WebDriverError::CdpClosed(self.closed_message()));
        }

        let started = Instant::now();
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (resp_tx, resp_rx) = oneshot::channel();

        // Register pending entry and re-check `closed` under the same mutex so the reader task
        // can never close and drain before our entry is visible.
        {
            let mut guard = self.inner.pending.lock().unwrap_or_else(|p| p.into_inner());
            if self.inner.closed.load(Ordering::SeqCst) {
                return Err(WebDriverError::CdpClosed(self.closed_message()));
            }
            guard.insert(
                id,
                PendingEntry {
                    method: method.to_string(),
                    tx: resp_tx,
                },
            );
        }

        // RAII guard ensures `id` is removed from `pending` if this future is dropped, cancelled,
        // or timed out at any `.await` point.
        let mut guard = PendingCallGuard {
            pending: &self.inner.pending,
            id,
            armed: true,
        };

        let mut frame = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        if let Some(sid) = session_id
            && let Some(obj) = frame.as_object_mut()
        {
            obj.insert("sessionId".to_string(), Value::String(sid.to_string()));
        }
        let payload = serde_json::to_string(&frame).map_err(|source| WebDriverError::Json {
            context: format!("serialize CDP request `{method}`"),
            source,
        })?;

        let deadline = tokio::time::Instant::now() + timeout;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(WebDriverError::Cancelled),
            timed = tokio::time::timeout_at(deadline, self.inner.tx.send(WriterCmd::Text(payload))) => {
                match timed {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => return Err(WebDriverError::CdpClosed(self.closed_message())),
                    Err(_) => {
                        return Err(WebDriverError::Timeout {
                            operation: format!("cdp:{method}"),
                            elapsed_ms: started.elapsed().as_millis() as u64,
                            detail: format!("timed out sending CDP `{method}` after {}ms", timeout.as_millis()),
                        });
                    }
                }
            }
        }

        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(WebDriverError::Cancelled),
            timed = tokio::time::timeout_at(deadline, resp_rx) => {
                match timed {
                    Ok(Ok(res)) => {
                        guard.armed = false;
                        res
                    }
                    Ok(Err(_)) => {
                        guard.armed = false;
                        Err(WebDriverError::CdpClosed(self.closed_message()))
                    }
                    Err(_) => Err(WebDriverError::Timeout {
                        operation: format!("cdp:{method}"),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        detail: format!("timed out waiting for CDP `{method}` response after {}ms", timeout.as_millis()),
                    }),
                }
            }
        };

        response
    }

    pub(crate) fn set_active_target(&self, target_id: Option<String>, session_id: Option<String>) {
        self.inner.events.set_active_target(target_id, session_id);
    }

    pub(crate) fn set_main_frame_id(&self, frame_id: Option<String>) {
        self.inner.events.set_main_frame_id(frame_id);
    }

    pub(crate) fn reset_network_state(&self) {
        self.inner.events.reset_network_state();
    }

    pub(crate) fn nav_seq(&self) -> u64 {
        self.inner.events.nav_seq.load(Ordering::SeqCst)
    }

    pub(crate) fn nav_notify(&self) -> &Notify {
        &self.inner.events.nav_notify
    }

    pub(crate) fn load_seq(&self) -> u64 {
        self.inner.events.load_seq.load(Ordering::SeqCst)
    }

    pub(crate) fn load_notify(&self) -> &Notify {
        &self.inner.events.load_notify
    }

    pub(crate) fn network_snapshot(&self) -> (usize, Instant) {
        self.inner.events.network_snapshot()
    }

    pub(crate) fn network_notify(&self) -> &Notify {
        &self.inner.events.network_notify
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    pub(crate) fn is_detached(&self) -> bool {
        self.inner.events.detached.load(Ordering::SeqCst)
    }

    fn closed_message(&self) -> String {
        self.inner
            .close_reason
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .unwrap_or_else(|| "CDP WebSocket closed".to_string())
    }

    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    /// Gracefully closes the WebSocket connection and aborts reader/writer tasks.
    pub(crate) async fn close(&self) {
        let _ = self.inner.tx.try_send(WriterCmd::Close);
        tokio::time::sleep(Duration::from_millis(15)).await;
        mark_closed_and_drain(
            &self.inner.closed,
            &self.inner.close_reason,
            &self.inner.pending,
            &self.inner.events,
            "CDP client closed".to_string(),
        );
        if let Some((writer, reader)) = self
            .inner
            .io_tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            writer.abort();
            reader.abort();
        }
    }
}

fn dispatch_incoming_text(
    text: &str,
    pending: &StdMutex<HashMap<u64, PendingEntry>>,
    events: &CdpEventState,
) {
    let Ok(msg) = serde_json::from_str::<Value>(text) else {
        trace!("ignoring non-JSON CDP WebSocket frame");
        return;
    };

    if let Some(id) = msg.get("id").and_then(Value::as_u64) {
        let entry = {
            let mut guard = pending.lock().unwrap_or_else(|p| p.into_inner());
            guard.remove(&id)
        };
        if let Some(entry) = entry {
            let result = if let Some(err_obj) = msg.get("error") {
                let code = err_obj.get("code").and_then(Value::as_i64).unwrap_or(-1);
                let message = err_obj
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown CDP error")
                    .to_string();
                Err(WebDriverError::CdpProtocol {
                    method: entry.method,
                    code,
                    message,
                })
            } else {
                Ok(msg.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = entry.tx.send(result);
        }
        return;
    }

    if let Some(method) = msg.get("method").and_then(Value::as_str) {
        let session_id = msg.get("sessionId").and_then(Value::as_str);
        let params = msg.get("params").unwrap_or(&Value::Null);
        events.handle_event(method, session_id, params);
    }
}

fn mark_closed_and_drain(
    closed: &AtomicBool,
    close_reason: &StdMutex<Option<String>>,
    pending: &StdMutex<HashMap<u64, PendingEntry>>,
    events: &CdpEventState,
    reason: String,
) {
    {
        let mut r = close_reason.lock().unwrap_or_else(|p| p.into_inner());
        if r.is_none() {
            *r = Some(reason.clone());
        }
    }
    closed.store(true, Ordering::SeqCst);

    let drained: Vec<PendingEntry> = {
        let mut guard = pending.lock().unwrap_or_else(|p| p.into_inner());
        guard.drain().map(|(_, v)| v).collect()
    };
    for entry in drained {
        let _ = entry
            .tx
            .send(Err(WebDriverError::CdpClosed(reason.clone())));
    }
    events.wake_all_waiters();
}

/// Active Chromium BrowserContext + attached flat Target session identifiers.
#[derive(Debug, Clone)]
pub(crate) struct PageSessionIds {
    pub browser_context_id: String,
    pub target_id: String,
    pub session_id: String,
}

/// Enables target discovery and creates a fresh isolated `BrowserContext` + `about:blank` target
/// attached with `flatten: true`, configured with viewport metrics and optional reduced-motion.
pub(crate) async fn create_isolated_context_and_page(
    cdp: &CdpClient,
    screen_size: ScreenSize,
    disable_animations: bool,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<PageSessionIds> {
    let _ = cdp
        .call_browser(
            "Target.setDiscoverTargets",
            json!({ "discover": true }),
            timeout,
            cancel,
        )
        .await?;

    let ctx_res = cdp
        .call_browser(
            "Target.createBrowserContext",
            json!({ "disposeOnDetach": true }),
            timeout,
            cancel,
        )
        .await?;
    let browser_context_id = ctx_res
        .get("browserContextId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| WebDriverError::CdpProtocol {
            method: "Target.createBrowserContext".to_string(),
            code: -32603,
            message: "missing browserContextId in response".to_string(),
        })?
        .to_string();

    let (target_id, session_id) = create_and_attach_page_in_context(
        cdp,
        &browser_context_id,
        screen_size,
        disable_animations,
        timeout,
        cancel,
    )
    .await?;

    Ok(PageSessionIds {
        browser_context_id,
        target_id,
        session_id,
    })
}

/// Creates a new `about:blank` target inside an existing `browser_context_id` (preserving cookies,
/// `localStorage`, and `IndexedDB` for that context), attaches with `flatten: true`, and applies
/// page domain enables + device metrics + optional reduced motion.
pub(crate) async fn create_and_attach_page_in_context(
    cdp: &CdpClient,
    browser_context_id: &str,
    screen_size: ScreenSize,
    disable_animations: bool,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<(String, String)> {
    let target_res = cdp
        .call_browser(
            "Target.createTarget",
            json!({
                "url": "about:blank",
                "browserContextId": browser_context_id,
            }),
            timeout,
            cancel,
        )
        .await?;
    let target_id = target_res
        .get("targetId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| WebDriverError::CdpProtocol {
            method: "Target.createTarget".to_string(),
            code: -32603,
            message: "missing targetId in response".to_string(),
        })?
        .to_string();

    let attach_res = cdp
        .call_browser(
            "Target.attachToTarget",
            json!({
                "targetId": target_id,
                "flatten": true,
            }),
            timeout,
            cancel,
        )
        .await?;
    let session_id = attach_res
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| WebDriverError::CdpProtocol {
            method: "Target.attachToTarget".to_string(),
            code: -32603,
            message: "missing sessionId in response".to_string(),
        })?
        .to_string();

    cdp.set_active_target(Some(target_id.clone()), Some(session_id.clone()));

    configure_page_session(
        cdp,
        &session_id,
        screen_size,
        disable_animations,
        timeout,
        cancel,
    )
    .await?;

    Ok((target_id, session_id))
}

async fn configure_page_session(
    cdp: &CdpClient,
    session_id: &str,
    screen_size: ScreenSize,
    disable_animations: bool,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    let _ = cdp
        .call_session(session_id, "Page.enable", json!({}), timeout, cancel)
        .await?;
    let frame_tree = cdp
        .call_session(session_id, "Page.getFrameTree", json!({}), timeout, cancel)
        .await?;
    if let Some(main_frame_id) = frame_tree
        .get("frameTree")
        .and_then(|ft| ft.get("frame"))
        .and_then(|f| f.get("id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        cdp.set_main_frame_id(Some(main_frame_id.to_string()));
    }
    let _ = cdp
        .call_session(session_id, "Runtime.enable", json!({}), timeout, cancel)
        .await?;
    let _ = cdp
        .call_session(session_id, "Network.enable", json!({}), timeout, cancel)
        .await?;
    let _ = cdp
        .call_session(
            session_id,
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": screen_size.width,
                "height": screen_size.height,
                "deviceScaleFactor": 1,
                "mobile": false,
            }),
            timeout,
            cancel,
        )
        .await?;
    if disable_animations {
        let _ = cdp
            .call_session(
                session_id,
                "Emulation.setEmulatedMedia",
                json!({
                    "features": [
                        { "name": "prefers-reduced-motion", "value": "reduce" }
                    ]
                }),
                timeout,
                cancel,
            )
            .await?;
    }
    Ok(())
}
