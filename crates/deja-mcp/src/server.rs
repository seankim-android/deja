//! Async stdio JSON-RPC 2.0 MCP server loop with inline cancellation, non-blocking read-only
//! tools, serialized warm-driver execution, and guaranteed EOF/signal teardown.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::backend::{McpDriverFactory, McpServerConfig, McpServerError};
use crate::protocol::{
    ERR_INVALID_PARAMS, ERR_INVALID_REQUEST, ERR_METHOD_NOT_FOUND, ERR_PARSE_ERROR,
    ERR_REQUEST_CANCELLED, FramedReader, FramedWriter, FramingMode, ReadFrameError, RequestIdKey,
    jsonrpc_error, jsonrpc_result,
};
use crate::redaction::RedactionContext;
use crate::tools::{
    DRIVER_SHUTDOWN_TIMEOUT, ToolContext, ToolDispatchResult, WarmSessionState, dispatch_tool_call,
    is_known_tool, tool_definitions, tool_error,
};

const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2024-11-05"];
const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
const WRITER_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);
const WRITER_QUEUE_CAPACITY: usize = 128;

/// Maximum concurrent `tools/call` tasks allowed in flight before immediate `SERVER_BUSY` rejection.
pub const MAX_IN_FLIGHT_TOOL_CALLS: usize = 32;

/// Maximum time the dispatcher/tasks wait for a saturated stdout response queue before initiating
/// clean server shutdown so stalled stdout backpressure can never hang the server.
pub const DEFAULT_WRITE_BACKPRESSURE_TIMEOUT: Duration = Duration::from_secs(2);

/// Tunable operational bounds for [`serve_with_limits`].
#[derive(Debug, Clone, Copy)]
pub struct McpServerLimits {
    pub max_in_flight_tool_calls: usize,
    pub write_backpressure_timeout: Duration,
}

impl Default for McpServerLimits {
    fn default() -> Self {
        Self {
            max_in_flight_tool_calls: MAX_IN_FLIGHT_TOOL_CALLS,
            write_backpressure_timeout: DEFAULT_WRITE_BACKPRESSURE_TIMEOUT,
        }
    }
}

struct InFlightGuard {
    map: Arc<StdMutex<HashMap<RequestIdKey, CancellationToken>>>,
    key: RequestIdKey,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.map.lock() {
            guard.remove(&self.key);
        }
    }
}

enum InFlightInsert {
    Inserted,
    Duplicate,
    Busy,
}

/// Serve the Déjà MCP stdio JSON-RPC 2.0 protocol over `input` and `output` until EOF or
/// `cancel` fires, guaranteeing bounded teardown of all in-flight tasks and warm drivers.
pub async fn serve<R, W>(
    input: R,
    output: W,
    config: McpServerConfig,
    factory: Arc<dyn McpDriverFactory>,
    cancel: CancellationToken,
) -> Result<(), McpServerError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    serve_with_limits(
        input,
        output,
        config,
        factory,
        cancel,
        McpServerLimits::default(),
    )
    .await
}

/// Serve the Déjà MCP stdio JSON-RPC 2.0 protocol with explicit concurrency and stdout
/// backpressure bounds.
pub async fn serve_with_limits<R, W>(
    input: R,
    output: W,
    config: McpServerConfig,
    factory: Arc<dyn McpDriverFactory>,
    cancel: CancellationToken,
    limits: McpServerLimits,
) -> Result<(), McpServerError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    config.validate()?;
    let max_in_flight = limits.max_in_flight_tool_calls.max(1);
    let backpressure_timeout = limits.write_backpressure_timeout;

    let redaction = RedactionContext::new(
        &config.canonical_workspace_root,
        config.home_dir.as_deref(),
        &config.forbidden_values,
        &config.process_env,
    );

    let tool_ctx = Arc::new(ToolContext {
        config,
        redaction: redaction.clone(),
        factory,
        exec_mutex: Arc::new(AsyncMutex::new(WarmSessionState::new())),
    });

    let server_cancel = cancel.child_token();
    let writer_cancel = cancel.child_token();
    let stdin_closed = server_cancel.child_token();
    let in_flight: Arc<StdMutex<HashMap<RequestIdKey, CancellationToken>>> =
        Arc::new(StdMutex::new(HashMap::new()));
    let mut tasks: JoinSet<()> = JoinSet::new();

    // Dedicated writer task owns `FramedWriter` so a stalled stdout pipe can never block the
    // request loop from reading `$/cancelRequest` / stdin EOF or prevent warm driver teardown.
    let (write_tx, mut write_rx) = mpsc::channel::<(Value, FramingMode)>(WRITER_QUEUE_CAPACITY);
    let writer_stop = writer_cancel.clone();
    let writer_err_cancel = server_cancel.clone();
    let mut writer_handle = tokio::spawn(async move {
        let mut writer = FramedWriter::new(output);
        loop {
            let maybe_msg = tokio::select! {
                biased;
                _ = writer_stop.cancelled() => break,
                msg = write_rx.recv() => msg,
            };
            let Some((value, mode)) = maybe_msg else {
                break;
            };
            writer.set_mode(mode);
            let write_res = tokio::select! {
                biased;
                _ = writer_stop.cancelled() => break,
                res = writer.write_json(&value, Some(mode)) => res,
            };
            if let Err(io_err) = write_res {
                warn!(error = %io_err, "deja-mcp stdout write error");
                writer_err_cancel.cancel();
                break;
            }
        }
    });

    // Dedicated reader task owns `FramedReader` so `read_frame()` is NEVER dropped mid-frame
    // when a tool task finishes in the main `tokio::select!` loop (`mpsc::Receiver::recv` is
    // strictly cancel-safe).
    let (frame_tx, mut frame_rx) =
        mpsc::channel::<Result<(FramingMode, Vec<u8>), ReadFrameError>>(64);
    let reader_cancel = server_cancel.clone();
    let reader_eof = stdin_closed.clone();
    let reader_handle = tokio::spawn(async move {
        let mut reader = FramedReader::new(BufReader::new(input));
        loop {
            let item = tokio::select! {
                biased;
                _ = reader_cancel.cancelled() => break,
                res = reader.read_frame() => res,
            };
            let terminal = matches!(
                item,
                Err(ReadFrameError::Eof)
                    | Err(ReadFrameError::Io(_))
                    | Err(ReadFrameError::FrameTooLarge {
                        recoverable: false,
                        ..
                    })
            );
            let send_res = tokio::select! {
                biased;
                _ = reader_cancel.cancelled() => break,
                res = frame_tx.send(item) => res,
            };
            if terminal {
                reader_eof.cancel();
                break;
            }
            if send_res.is_err() {
                break;
            }
        }
    });

    let sink = ResponseSink {
        tx: write_tx.clone(),
        server_cancel: server_cancel.clone(),
        writer_cancel: writer_cancel.clone(),
        stdin_closed: stdin_closed.clone(),
        backpressure_timeout,
    };

    debug!("deja-mcp stdio server started");

    loop {
        let frame_res = tokio::select! {
            biased;
            _ = server_cancel.cancelled() => {
                debug!("deja-mcp received cancellation signal; exiting reader loop");
                break;
            }
            Some(join_res) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(err) = join_res {
                    warn!(error = %err, "deja-mcp tool task panicked or aborted");
                }
                continue;
            }
            maybe = frame_rx.recv() => match maybe {
                Some(res) => res,
                None => Err(ReadFrameError::Eof),
            },
        };

        let (mode, raw_bytes) = match frame_res {
            Ok(frame) => frame,
            Err(ReadFrameError::Eof) => {
                debug!("deja-mcp reached stdin EOF; tearing down");
                break;
            }
            Err(ReadFrameError::FrameTooLarge {
                mode,
                limit,
                recoverable,
            }) => {
                let err_resp = jsonrpc_error(
                    Value::Null,
                    ERR_INVALID_REQUEST,
                    &format!("Frame exceeds maximum size of {limit} bytes"),
                );
                if sink.send(err_resp, mode).await.is_err() {
                    break;
                }
                if !recoverable {
                    break;
                }
                continue;
            }
            Err(ReadFrameError::InvalidHeader { mode, detail }) => {
                let msg = redaction.redact_str(&format!("Invalid frame header: {detail}"));
                let err_resp = jsonrpc_error(Value::Null, ERR_INVALID_REQUEST, &msg);
                if sink.send(err_resp, mode).await.is_err() {
                    break;
                }
                continue;
            }
            Err(ReadFrameError::Io(io_err)) => {
                warn!(error = %io_err, "stdin read error");
                break;
            }
        };

        let msg: Value = match serde_json::from_slice(&raw_bytes) {
            Ok(v) => v,
            Err(err) => {
                let detail = redaction.redact_str(&format!("Parse error: {err}"));
                let resp = jsonrpc_error(Value::Null, ERR_PARSE_ERROR, &detail);
                if sink.send(resp, mode).await.is_err() {
                    break;
                }
                continue;
            }
        };

        let Some(obj) = msg.as_object() else {
            let resp = jsonrpc_error(
                Value::Null,
                ERR_INVALID_REQUEST,
                "Invalid Request: expected JSON object",
            );
            if sink.send(resp, mode).await.is_err() {
                break;
            }
            continue;
        };

        // Check optional `id` first so error responses can echo valid request IDs.
        let parsed_id: Option<(Value, RequestIdKey)> = match obj.get("id") {
            None => None,
            Some(id_val) => match RequestIdKey::from_json(id_val) {
                Ok(key) => Some((id_val.clone(), key)),
                Err(reason) => {
                    let resp = jsonrpc_error(Value::Null, ERR_INVALID_REQUEST, reason);
                    if sink.send(resp, mode).await.is_err() {
                        break;
                    }
                    continue;
                }
            },
        };

        if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            let err_id = parsed_id
                .as_ref()
                .map(|(v, _)| v.clone())
                .unwrap_or(Value::Null);
            let resp = jsonrpc_error(
                err_id,
                ERR_INVALID_REQUEST,
                "Invalid Request: `jsonrpc` must be \"2.0\"",
            );
            if sink.send(resp, mode).await.is_err() {
                break;
            }
            continue;
        }

        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            let err_id = parsed_id
                .as_ref()
                .map(|(v, _)| v.clone())
                .unwrap_or(Value::Null);
            let resp = jsonrpc_error(
                err_id,
                ERR_INVALID_REQUEST,
                "Invalid Request: missing string `method`",
            );
            if sink.send(resp, mode).await.is_err() {
                break;
            }
            continue;
        };

        let params = obj.get("params").cloned().unwrap_or(Value::Null);

        match method {
            // Immediate inline cancellation handlers (never blocked by `exec_mutex`!).
            "notifications/cancelled" => {
                if let Some(req_id_val) = params.get("requestId")
                    && let Ok(key) = RequestIdKey::from_json(req_id_val)
                {
                    cancel_in_flight(&in_flight, &key);
                }
            }
            "$/cancelRequest" => {
                if let Some(req_id_val) = params.get("id")
                    && let Ok(key) = RequestIdKey::from_json(req_id_val)
                {
                    cancel_in_flight(&in_flight, &key);
                }
            }
            "notifications/initialized" => {
                // Standard MCP notification: no response required.
                if let Some((id_val, _)) = parsed_id {
                    let resp = jsonrpc_result(id_val, json!({}));
                    if sink.send(resp, mode).await.is_err() {
                        break;
                    }
                }
            }
            "initialize" => {
                let Some((id_val, _)) = parsed_id else {
                    continue;
                };
                let resp = handle_initialize(id_val, &params);
                if sink.send(resp, mode).await.is_err() {
                    break;
                }
            }
            "ping" => {
                let Some((id_val, _)) = parsed_id else {
                    continue;
                };
                let resp = jsonrpc_result(id_val, json!({}));
                if sink.send(resp, mode).await.is_err() {
                    break;
                }
            }
            "tools/list" => {
                let Some((id_val, _)) = parsed_id else {
                    continue;
                };
                if !params.is_null() && !params.is_object() {
                    let resp = jsonrpc_error(
                        id_val,
                        ERR_INVALID_PARAMS,
                        "Invalid params: expected object",
                    );
                    if sink.send(resp, mode).await.is_err() {
                        break;
                    }
                    continue;
                }
                let resp = jsonrpc_result(
                    id_val,
                    json!({
                        "tools": tool_definitions(
                            tool_ctx.config.mcp_explore,
                            tool_ctx.config.explore_backend_available,
                        )
                    }),
                );
                if sink.send(resp, mode).await.is_err() {
                    break;
                }
            }
            "tools/call" => {
                let Some((id_val, id_key)) = parsed_id else {
                    continue;
                };
                let Some(params_obj) = params.as_object() else {
                    let resp = jsonrpc_error(
                        id_val,
                        ERR_INVALID_PARAMS,
                        "Invalid params: `tools/call` requires a JSON object",
                    );
                    if sink.send(resp, mode).await.is_err() {
                        break;
                    }
                    continue;
                };
                let Some(tool_name) = params_obj.get("name").and_then(Value::as_str) else {
                    let resp = jsonrpc_error(
                        id_val,
                        ERR_INVALID_PARAMS,
                        "Invalid params: `tools/call` requires string `name`",
                    );
                    if sink.send(resp, mode).await.is_err() {
                        break;
                    }
                    continue;
                };
                if !is_known_tool(tool_name) {
                    let detail = redaction.redact_str(&format!("Unknown tool: {tool_name}"));
                    let resp = jsonrpc_error(id_val, ERR_INVALID_PARAMS, &detail);
                    if sink.send(resp, mode).await.is_err() {
                        break;
                    }
                    continue;
                }
                let raw_arguments = params_obj
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));

                let req_cancel = server_cancel.child_token();
                let insert_status = {
                    let mut map = in_flight.lock().expect("in_flight mutex");
                    if map.contains_key(&id_key) {
                        InFlightInsert::Duplicate
                    } else if map.len() >= max_in_flight {
                        InFlightInsert::Busy
                    } else {
                        map.insert(id_key.clone(), req_cancel.clone());
                        InFlightInsert::Inserted
                    }
                };
                match insert_status {
                    InFlightInsert::Duplicate => {
                        let resp = jsonrpc_error(
                            id_val,
                            ERR_INVALID_REQUEST,
                            "Duplicate in-flight request id",
                        );
                        if sink.send(resp, mode).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    InFlightInsert::Busy => {
                        let busy = tool_error(
                            &redaction,
                            "SERVER_BUSY",
                            "too many concurrent tool calls in flight",
                        );
                        let resp = jsonrpc_result(id_val, busy);
                        if sink.send(resp, mode).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    InFlightInsert::Inserted => {}
                }

                let guard = InFlightGuard {
                    map: Arc::clone(&in_flight),
                    key: id_key,
                };
                let task_ctx = Arc::clone(&tool_ctx);
                let task_sink = sink.clone();
                let task_server_cancel = server_cancel.clone();
                let tool_name_owned = tool_name.to_string();

                tasks.spawn(async move {
                    let _in_flight_guard = guard;
                    let result = dispatch_tool_call(
                        task_ctx,
                        &tool_name_owned,
                        raw_arguments,
                        req_cancel.clone(),
                    )
                    .await;

                    // Do not write responses once the server itself is shutting down on EOF/signal.
                    if task_server_cancel.is_cancelled() {
                        return;
                    }

                    let response = match result {
                        ToolDispatchResult::Cancelled => {
                            jsonrpc_error(id_val, ERR_REQUEST_CANCELLED, "Request cancelled")
                        }
                        ToolDispatchResult::Response(_) if req_cancel.is_cancelled() => {
                            jsonrpc_error(id_val, ERR_REQUEST_CANCELLED, "Request cancelled")
                        }
                        ToolDispatchResult::Response(tool_result) => {
                            jsonrpc_result(id_val, tool_result)
                        }
                    };
                    let _ = task_sink.send(response, mode).await;
                });
            }
            other => {
                if let Some((id_val, _)) = parsed_id {
                    let detail = redaction.redact_str(&format!("Method not found: {other}"));
                    let resp = jsonrpc_error(id_val, ERR_METHOD_NOT_FOUND, &detail);
                    if sink.send(resp, mode).await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    // Graceful shutdown: cancel every in-flight task, abort the dedicated reader task, drain
    // tool tasks bounded, drain the dedicated writer task bounded (never hanging on a stalled
    // stdout pipe), and shut down any idle warm driver so zero processes or locks leak.
    server_cancel.cancel();
    reader_handle.abort();
    drop(sink);
    drop(write_tx);

    let drain_res = tokio::time::timeout(SHUTDOWN_JOIN_TIMEOUT, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    if drain_res.is_err() {
        warn!("deja-mcp shutdown grace elapsed; aborting remaining tasks");
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }

    if tokio::time::timeout(WRITER_SHUTDOWN_TIMEOUT, &mut writer_handle)
        .await
        .is_err()
    {
        warn!("deja-mcp stdout writer drain timed out; cancelling stalled writer");
        writer_cancel.cancel();
        writer_handle.abort();
        let _ = writer_handle.await;
    }

    if let Ok(mut session) =
        tokio::time::timeout(DRIVER_SHUTDOWN_TIMEOUT, tool_ctx.exec_mutex.lock()).await
    {
        session.teardown_driver().await;
    }

    debug!("deja-mcp stdio server stopped cleanly");
    Ok(())
}

fn handle_initialize(id: Value, params: &Value) -> Value {
    if !params.is_null() && !params.is_object() {
        return jsonrpc_error(
            id,
            ERR_INVALID_PARAMS,
            "Invalid params: `initialize` expects a JSON object",
        );
    }

    let requested = params
        .as_object()
        .and_then(|o| o.get("protocolVersion"))
        .and_then(Value::as_str);

    let negotiated = match requested {
        Some(v) if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => v,
        _ => DEFAULT_PROTOCOL_VERSION,
    };

    jsonrpc_result(
        id,
        json!({
            "protocolVersion": negotiated,
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "deja",
                "version": env!("CARGO_PKG_VERSION"),
            }
        }),
    )
}

fn cancel_in_flight(
    in_flight: &Arc<StdMutex<HashMap<RequestIdKey, CancellationToken>>>,
    key: &RequestIdKey,
) {
    if let Ok(guard) = in_flight.lock()
        && let Some(token) = guard.get(key)
    {
        debug!(id = ?key, "cancelling in-flight MCP request");
        token.cancel();
    }
}

#[derive(Clone)]
struct ResponseSink {
    tx: mpsc::Sender<(Value, FramingMode)>,
    server_cancel: CancellationToken,
    writer_cancel: CancellationToken,
    stdin_closed: CancellationToken,
    backpressure_timeout: Duration,
}

impl ResponseSink {
    async fn send(&self, response: Value, mode: FramingMode) -> Result<(), ()> {
        if self.server_cancel.is_cancelled() {
            return Err(());
        }
        match self.tx.try_send((response, mode)) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(()),
            Err(mpsc::error::TrySendError::Full(msg)) => {
                let send_res = tokio::select! {
                    biased;
                    _ = self.server_cancel.cancelled() => return Err(()),
                    _ = self.stdin_closed.cancelled() => return Err(()),
                    res = tokio::time::timeout(self.backpressure_timeout, self.tx.send(msg)) => res,
                };
                match send_res {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(_)) => Err(()),
                    Err(_elapsed) => {
                        warn!(
                            timeout_ms = self.backpressure_timeout.as_millis() as u64,
                            "deja-mcp stdout response queue saturated past overload deadline; shutting down"
                        );
                        self.server_cancel.cancel();
                        self.writer_cancel.cancel();
                        Err(())
                    }
                }
            }
        }
    }
}
