//! Bounded stdio JSON-RPC 2.0 framing supporting both newline-delimited JSON and LSP-style
//! `Content-Length:` headers with automatic per-stream detection.

use std::io;

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum permitted JSON-RPC payload size (4 MiB).
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Maximum single LSP header line length (8 KiB).
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;

/// Maximum number of LSP headers before the blank separator line.
const MAX_HEADER_COUNT: usize = 32;

/// Maximum oversized `Content-Length` body we drain before aborting the connection (16 MiB).
const MAX_DRAIN_BYTES: usize = 16 * 1024 * 1024;

// Standard JSON-RPC 2.0 and MCP error codes.
pub const ERR_PARSE_ERROR: i64 = -32700;
pub const ERR_INVALID_REQUEST: i64 = -32600;
pub const ERR_METHOD_NOT_FOUND: i64 = -32601;
pub const ERR_INVALID_PARAMS: i64 = -32602;
pub const ERR_INTERNAL_ERROR: i64 = -32603;
pub const ERR_REQUEST_CANCELLED: i64 = -32800;

/// Wire framing mode negotiated automatically from incoming frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramingMode {
    NewlineDelimited,
    ContentLength,
}

/// Typed request ID key preserving JSON string vs integer distinction for in-flight tracking.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestIdKey {
    String(String),
    Int(i64),
    Uint(u64),
}

impl RequestIdKey {
    /// Parse a JSON-RPC request/cancellation ID value (`string` or finite integer, never `null`).
    pub fn from_json(value: &Value) -> Result<Self, &'static str> {
        match value {
            Value::String(s) => Ok(Self::String(s.clone())),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(Self::Int(i))
                } else if let Some(u) = n.as_u64() {
                    Ok(Self::Uint(u))
                } else {
                    Err("request id must be a string or integer")
                }
            }
            _ => Err("request id must be a string or integer"),
        }
    }
}

/// Result of reading a single framed message from stdin.
#[derive(Debug)]
pub enum ReadFrameError {
    Eof,
    FrameTooLarge {
        mode: FramingMode,
        limit: usize,
        recoverable: bool,
    },
    InvalidHeader {
        mode: FramingMode,
        detail: String,
    },
    Io(io::Error),
}

/// Construct a JSON-RPC 2.0 success response object.
#[must_use]
pub fn jsonrpc_result(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

/// Construct a JSON-RPC 2.0 error response object.
#[must_use]
pub fn jsonrpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    })
}

/// Bounded auto-detecting JSON-RPC frame reader.
pub struct FramedReader<R> {
    inner: R,
}

impl<R: AsyncBufRead + Unpin> FramedReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Read the next framed JSON payload, returning the detected [`FramingMode`] and raw bytes.
    pub async fn read_frame(&mut self) -> Result<(FramingMode, Vec<u8>), ReadFrameError> {
        loop {
            let line_res = read_bounded_line(&mut self.inner, MAX_FRAME_BYTES).await?;
            let Some(first_line) = line_res else {
                return Err(ReadFrameError::Eof);
            };

            if first_line.oversized {
                return Err(ReadFrameError::FrameTooLarge {
                    mode: FramingMode::NewlineDelimited,
                    limit: MAX_FRAME_BYTES,
                    recoverable: true,
                });
            }

            let trimmed = strip_line_ending(&first_line.bytes);
            if trimmed.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }

            if looks_like_lsp_header(trimmed) {
                return self.read_content_length_frame(trimmed).await;
            }

            return Ok((FramingMode::NewlineDelimited, trimmed.to_vec()));
        }
    }

    async fn read_content_length_frame(
        &mut self,
        first_header: &[u8],
    ) -> Result<(FramingMode, Vec<u8>), ReadFrameError> {
        let mut content_length: Option<usize> = None;
        let mut header_count = 0usize;

        parse_lsp_header(first_header, &mut content_length)?;
        header_count += 1;

        loop {
            let line_res = read_bounded_line(&mut self.inner, MAX_HEADER_LINE_BYTES).await?;
            let Some(line) = line_res else {
                return Err(ReadFrameError::Eof);
            };
            if line.oversized {
                return Err(ReadFrameError::InvalidHeader {
                    mode: FramingMode::ContentLength,
                    detail: format!(
                        "header line exceeds maximum length of {MAX_HEADER_LINE_BYTES} bytes"
                    ),
                });
            }
            let trimmed = strip_line_ending(&line.bytes);
            if trimmed.is_empty() {
                break;
            }
            header_count += 1;
            if header_count > MAX_HEADER_COUNT {
                return Err(ReadFrameError::InvalidHeader {
                    mode: FramingMode::ContentLength,
                    detail: format!("too many headers (max {MAX_HEADER_COUNT})"),
                });
            }
            parse_lsp_header(trimmed, &mut content_length)?;
        }

        let Some(len) = content_length else {
            return Err(ReadFrameError::InvalidHeader {
                mode: FramingMode::ContentLength,
                detail: "missing required Content-Length header".to_string(),
            });
        };

        if len > MAX_FRAME_BYTES {
            if len <= MAX_DRAIN_BYTES {
                drain_exact(&mut self.inner, len).await?;
                return Err(ReadFrameError::FrameTooLarge {
                    mode: FramingMode::ContentLength,
                    limit: MAX_FRAME_BYTES,
                    recoverable: true,
                });
            }
            return Err(ReadFrameError::FrameTooLarge {
                mode: FramingMode::ContentLength,
                limit: MAX_FRAME_BYTES,
                recoverable: false,
            });
        }

        let mut payload = vec![0u8; len];
        match self.inner.read_exact(&mut payload).await {
            Ok(_) => Ok((FramingMode::ContentLength, payload)),
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Err(ReadFrameError::Eof),
            Err(err) => Err(ReadFrameError::Io(err)),
        }
    }
}

/// Framed JSON-RPC writer flushing after every response.
pub struct FramedWriter<W> {
    inner: W,
    mode: FramingMode,
}

impl<W: AsyncWrite + Unpin> FramedWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            mode: FramingMode::NewlineDelimited,
        }
    }

    pub fn set_mode(&mut self, mode: FramingMode) {
        self.mode = mode;
    }

    pub fn mode(&self) -> FramingMode {
        self.mode
    }

    /// Serialize `value` as compact JSON and write it using `override_mode` (or the writer's
    /// active framing mode), flushing immediately.
    pub async fn write_json(
        &mut self,
        value: &Value,
        override_mode: Option<FramingMode>,
    ) -> io::Result<()> {
        let mode = override_mode.unwrap_or(self.mode);
        let payload = serde_json::to_vec(value)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
        match mode {
            FramingMode::NewlineDelimited => {
                self.inner.write_all(&payload).await?;
                self.inner.write_all(b"\n").await?;
            }
            FramingMode::ContentLength => {
                let header = format!("Content-Length: {}\r\n\r\n", payload.len());
                self.inner.write_all(header.as_bytes()).await?;
                self.inner.write_all(&payload).await?;
            }
        }
        self.inner.flush().await
    }
}

struct BoundedLine {
    bytes: Vec<u8>,
    oversized: bool,
}

/// Read up to `\n` while capping buffered memory at `limit` bytes (draining any overflow until
/// `\n` or EOF so oversized lines never OOM the server and leave the stream aligned).
async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Option<BoundedLine>, ReadFrameError> {
    let mut buf = Vec::new();
    let mut oversized = false;
    let mut saw_any = false;

    loop {
        let chunk = reader.fill_buf().await.map_err(ReadFrameError::Io)?;
        if chunk.is_empty() {
            if !saw_any {
                return Ok(None);
            }
            return Ok(Some(BoundedLine {
                bytes: buf,
                oversized,
            }));
        }
        saw_any = true;

        if let Some(nl_pos) = chunk.iter().position(|&b| b == b'\n') {
            let take = nl_pos + 1;
            if !oversized {
                let content_len = if nl_pos > 0 && chunk[nl_pos - 1] == b'\r' {
                    nl_pos - 1
                } else {
                    nl_pos
                };
                if buf.len().saturating_add(content_len) > limit {
                    oversized = true;
                    buf.clear();
                } else {
                    buf.extend_from_slice(&chunk[..take]);
                }
            }
            reader.consume(take);
            return Ok(Some(BoundedLine {
                bytes: buf,
                oversized,
            }));
        }

        let len = chunk.len();
        if !oversized {
            // Allow 1 extra byte for a trailing `\r` split across chunks before `\n`.
            if buf.len().saturating_add(len) > limit.saturating_add(1) {
                oversized = true;
                buf.clear();
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        reader.consume(len);
    }
}

async fn drain_exact<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    mut remaining: usize,
) -> Result<(), ReadFrameError> {
    let mut scratch = [0u8; 8192];
    while remaining > 0 {
        let to_read = remaining.min(scratch.len());
        let n = reader
            .read(&mut scratch[..to_read])
            .await
            .map_err(ReadFrameError::Io)?;
        if n == 0 {
            return Err(ReadFrameError::Eof);
        }
        remaining -= n;
    }
    Ok(())
}

fn strip_line_ending(bytes: &[u8]) -> &[u8] {
    let s = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    s.strip_suffix(b"\r").unwrap_or(s)
}

fn looks_like_lsp_header(line: &[u8]) -> bool {
    let trimmed = trim_ascii_ws(line);
    if trimmed.starts_with(b"{") || trimmed.starts_with(b"[") {
        return false;
    }
    let Some(colon) = trimmed.iter().position(|&b| b == b':') else {
        return false;
    };
    let name = trim_ascii_ws(&trimmed[..colon]);
    name.eq_ignore_ascii_case(b"content-length") || name.eq_ignore_ascii_case(b"content-type")
}

fn parse_lsp_header(line: &[u8], content_length: &mut Option<usize>) -> Result<(), ReadFrameError> {
    let Ok(text) = std::str::from_utf8(line) else {
        return Err(ReadFrameError::InvalidHeader {
            mode: FramingMode::ContentLength,
            detail: "header line is not valid UTF-8".to_string(),
        });
    };
    let Some((raw_name, raw_val)) = text.split_once(':') else {
        return Err(ReadFrameError::InvalidHeader {
            mode: FramingMode::ContentLength,
            detail: "malformed header line (missing `:`)".to_string(),
        });
    };
    let name = raw_name.trim();
    let val = raw_val.trim();
    if name.eq_ignore_ascii_case("content-length") {
        let parsed = val
            .parse::<usize>()
            .map_err(|_| ReadFrameError::InvalidHeader {
                mode: FramingMode::ContentLength,
                detail: format!("invalid Content-Length value `{val}`"),
            })?;
        if let Some(prev) = *content_length
            && prev != parsed
        {
            return Err(ReadFrameError::InvalidHeader {
                mode: FramingMode::ContentLength,
                detail: "conflicting duplicate Content-Length headers".to_string(),
            });
        }
        *content_length = Some(parsed);
    }
    Ok(())
}

fn trim_ascii_ws(mut slice: &[u8]) -> &[u8] {
    while let Some((&first, rest)) = slice.split_first() {
        if first.is_ascii_whitespace() {
            slice = rest;
        } else {
            break;
        }
    }
    while let Some((&last, rest)) = slice.split_last() {
        if last.is_ascii_whitespace() {
            slice = rest;
        } else {
            break;
        }
    }
    slice
}
