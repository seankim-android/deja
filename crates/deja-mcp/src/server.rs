//! Async stdio JSON-RPC 2.0 MCP server loop with inline cancellation, non-blocking read-only
//! tools, serialized warm-driver execution, and guaranteed EOF/signal teardown.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::Mutex as AsyncMutex;
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
    is_known_tool, tool_definitions,
};

const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-03-26", "2024-11-05"];
const DEFAULT_PROTOCOL_VERSION: &str = "2025-03-26";
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

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
    config.validate()?;

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
    let in_flight: Arc<StdMutex<HashMap<RequestIdKey, CancellationToken>>> =
        Arc::new(StdMutex::new(HashMap::new()));
    let writer = Arc::new(AsyncMutex::new(FramedWriter::new(output)));
    let mut tasks: JoinSet<()> = JoinSet::new();

    // Dedicated reader task owns `FramedReader` so `read_frame()` is NEVER dropped mid-frame
    // when a tool task finishes in the main `tokio::select!` loop (`mpsc::Receiver::recv` is
    // strictly cancel-safe).
    let (frame_tx, mut frame_rx) =
        tokio::sync::mpsc::channel::<Result<(FramingMode, Vec<u8>), ReadFrameError>>(64);
    let reader_cancel = server_cancel.clone();
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
            if frame_tx.send(item).await.is_err() || terminal {
                break;
            }
        }
    });

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
                let mut w = writer.lock().await;
                w.set_mode(mode);
                if let Err(io_err) = w.write_json(&err_resp, Some(mode)).await {
                    warn!(error = %io_err, "failed to write oversized-frame error");
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
                let mut w = writer.lock().await;
                w.set_mode(mode);
                if let Err(io_err) = w.write_json(&err_resp, Some(mode)).await {
                    warn!(error = %io_err, "failed to write header error");
                    break;
                }
                continue;
            }
            Err(ReadFrameError::Io(io_err)) => {
                warn!(error = %io_err, "stdin read error");
                break;
            }
        };

        {
            let mut w = writer.lock().await;
            w.set_mode(mode);
        }

        let msg: Value = match serde_json::from_slice(&raw_bytes) {
            Ok(v) => v,
            Err(err) => {
                let detail = redaction.redact_str(&format!("Parse error: {err}"));
                let resp = jsonrpc_error(Value::Null, ERR_PARSE_ERROR, &detail);
                if write_response(&writer, &resp, mode).await.is_err() {
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
            if write_response(&writer, &resp, mode).await.is_err() {
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
                    if write_response(&writer, &resp, mode).await.is_err() {
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
            if write_response(&writer, &resp, mode).await.is_err() {
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
            if write_response(&writer, &resp, mode).await.is_err() {
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
                    if write_response(&writer, &resp, mode).await.is_err() {
                        break;
                    }
                }
            }
            "initialize" => {
                let Some((id_val, _)) = parsed_id else {
                    continue;
                };
                let resp = handle_initialize(id_val, &params);
                if write_response(&writer, &resp, mode).await.is_err() {
                    break;
                }
            }
            "ping" => {
                let Some((id_val, _)) = parsed_id else {
                    continue;
                };
                let resp = jsonrpc_result(id_val, json!({}));
                if write_response(&writer, &resp, mode).await.is_err() {
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
                    if write_response(&writer, &resp, mode).await.is_err() {
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
                if write_response(&writer, &resp, mode).await.is_err() {
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
                    if write_response(&writer, &resp, mode).await.is_err() {
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
                    if write_response(&writer, &resp, mode).await.is_err() {
                        break;
                    }
                    continue;
                };
                if !is_known_tool(tool_name) {
                    let detail = redaction.redact_str(&format!("Unknown tool: {tool_name}"));
                    let resp = jsonrpc_error(id_val, ERR_INVALID_PARAMS, &detail);
                    if write_response(&writer, &resp, mode).await.is_err() {
                        break;
                    }
                    continue;
                }
                let raw_arguments = params_obj
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));

                let req_cancel = server_cancel.child_token();
                let is_duplicate = {
                    let mut map = in_flight.lock().expect("in_flight mutex");
                    if map.contains_key(&id_key) {
                        true
                    } else {
                        map.insert(id_key.clone(), req_cancel.clone());
                        false
                    }
                };
                if is_duplicate {
                    let resp = jsonrpc_error(
                        id_val,
                        ERR_INVALID_REQUEST,
                        "Duplicate in-flight request id",
                    );
                    if write_response(&writer, &resp, mode).await.is_err() {
                        break;
                    }
                    continue;
                }

                let guard = InFlightGuard {
                    map: Arc::clone(&in_flight),
                    key: id_key,
                };
                let task_ctx = Arc::clone(&tool_ctx);
                let task_writer = Arc::clone(&writer);
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
                    let _ = write_response(&task_writer, &response, mode).await;
                });
            }
            other => {
                if let Some((id_val, _)) = parsed_id {
                    let detail = redaction.redact_str(&format!("Method not found: {other}"));
                    let resp = jsonrpc_error(id_val, ERR_METHOD_NOT_FOUND, &detail);
                    if write_response(&writer, &resp, mode).await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    // Graceful shutdown: cancel every in-flight task, abort the dedicated reader task, drain
    // bounded, and shut down any idle warm driver so zero processes or locks leak.
    server_cancel.cancel();
    reader_handle.abort();

    let drain_res = tokio::time::timeout(SHUTDOWN_JOIN_TIMEOUT, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    if drain_res.is_err() {
        warn!("deja-mcp shutdown grace elapsed; aborting remaining tasks");
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
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

async fn write_response<W: AsyncWrite + Unpin>(
    writer: &Arc<AsyncMutex<FramedWriter<W>>>,
    response: &Value,
    mode: FramingMode,
) -> std::io::Result<()> {
    let mut guard = writer.lock().await;
    guard.write_json(response, Some(mode)).await
}
