//! Dual-mode streaming HTTP/SSE client:
//! - Pure-Rust cancel-safe tokio::net::TcpStream HTTP/1.1 client for direct http:// endpoints.
//! - Supervised curl -sS -N --http1.1 -D - --config - client for https:// and proxied traffic.

use std::future::Future;
use std::process::Stdio;
use std::time::Duration;

use bytes::Bytes;
use liftgate_provider_stateful::job::{ProcessGroupGuard, configure_command_group};
use liftgate_types::config::ProxyUrl;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    process::Command,
    sync::mpsc,
    time::{Instant, timeout, timeout_at},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    error::PlatformApiError,
    sse::{SseDecoder, SseFrame},
};

const MAX_HEADER_LINE_BYTES: usize = 16 * 1024;
const MAX_SSE_LINE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_HTTP_CHUNK_BYTES: usize = 16 * 1024 * 1024;
const STREAM_READ_SCRATCH_BYTES: usize = 8 * 1024;

/// Outbound HTTP request descriptor.
#[derive(Clone, Debug)]
pub struct HttpRequestSpec {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Bytes>,
    pub proxy: Option<ProxyUrl>,
    pub force_direct_proxy: bool,
    pub connect_timeout: Duration,
    pub total_timeout: Duration,
}

/// Streaming SSE response handle opened after upstream HTTP headers arrive with 2xx.
pub struct HttpSseStream {
    pub status: u16,
    pub frames_rx: mpsc::Receiver<Result<SseFrame, PlatformApiError>>,
    _cancel_guard: tokio_util::sync::DropGuard,
}

/// Plain buffered HTTP response (used by /models discovery and lg provider test).
#[derive(Clone, Debug)]
pub struct HttpJsonResponse {
    pub status: u16,
    pub retry_after_ms: Option<u64>,
    pub body: String,
}

/// Static-dispatch trait for outbound HTTP/SSE execution (no `#[async_trait]`).
pub trait HttpSseTransport: Send + Sync + 'static {
    fn open_sse(
        &self,
        req: HttpRequestSpec,
    ) -> impl Future<Output = Result<HttpSseStream, PlatformApiError>> + Send;

    fn send_buffered(
        &self,
        req: HttpRequestSpec,
    ) -> impl Future<Output = Result<HttpJsonResponse, PlatformApiError>> + Send;
}

/// Production dual-mode transport.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultHttpTransport;

#[derive(Clone, Debug)]
struct ParsedHttpUrl {
    is_https: bool,
    host: String,
    port: u16,
    host_header: String,
    path_and_query: String,
}

fn parse_http_url(raw: &str) -> Result<ParsedHttpUrl, PlatformApiError> {
    let trimmed = raw.trim();
    let (is_https, rest) = if let Some(r) = trimmed.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = trimmed.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(PlatformApiError::InvalidUrl {
            url: trimmed.to_owned(),
            reason: "URL must start with http:// or https://".to_owned(),
        });
    };

    let (authority, path_part) = match rest.find('/') {
        Some(idx) => {
            let (auth, tail) = rest.split_at(idx);
            (auth, tail)
        }
        None => (rest, "/"),
    };
    if authority.is_empty() || authority.contains('@') {
        return Err(PlatformApiError::InvalidUrl {
            url: trimmed.to_owned(),
            reason: "invalid URL authority".to_owned(),
        });
    }

    let default_port = if is_https { 443 } else { 80 };
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((ipv6_host, after_bracket)) = bracketed.split_once(']') else {
            return Err(PlatformApiError::InvalidUrl {
                url: trimmed.to_owned(),
                reason: "unclosed IPv6 bracket".to_owned(),
            });
        };
        let port = if let Some(port_str) = after_bracket.strip_prefix(':') {
            port_str
                .parse::<u16>()
                .map_err(|_| PlatformApiError::InvalidUrl {
                    url: trimmed.to_owned(),
                    reason: "invalid port".to_owned(),
                })?
        } else if after_bracket.is_empty() {
            default_port
        } else {
            return Err(PlatformApiError::InvalidUrl {
                url: trimmed.to_owned(),
                reason: "unexpected characters after IPv6 host".to_owned(),
            });
        };
        (ipv6_host.to_owned(), port)
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        let port = p.parse::<u16>().map_err(|_| PlatformApiError::InvalidUrl {
            url: trimmed.to_owned(),
            reason: "invalid port".to_owned(),
        })?;
        (h.to_owned(), port)
    } else {
        (authority.to_owned(), default_port)
    };

    if host.is_empty() {
        return Err(PlatformApiError::InvalidUrl {
            url: trimmed.to_owned(),
            reason: "empty host".to_owned(),
        });
    }

    Ok(ParsedHttpUrl {
        is_https,
        host,
        port,
        host_header: authority.to_owned(),
        path_and_query: path_part.to_owned(),
    })
}

impl HttpSseTransport for DefaultHttpTransport {
    async fn open_sse(&self, req: HttpRequestSpec) -> Result<HttpSseStream, PlatformApiError> {
        let parsed = parse_http_url(&req.url)?;
        let use_direct_tokio = !parsed.is_https && req.proxy.is_none();
        if use_direct_tokio {
            open_tokio_http1_sse(&parsed, req).await
        } else {
            open_curl_sse(req).await
        }
    }

    async fn send_buffered(
        &self,
        req: HttpRequestSpec,
    ) -> Result<HttpJsonResponse, PlatformApiError> {
        let parsed = parse_http_url(&req.url)?;
        let use_direct_tokio = !parsed.is_https && req.proxy.is_none();
        if use_direct_tokio {
            send_tokio_http1_buffered(&parsed, req).await
        } else {
            send_curl_buffered(req).await
        }
    }
}

#[derive(Debug, Default)]
struct ParsedResponseHeaders {
    status: u16,
    chunked: bool,
    content_length: Option<usize>,
    retry_after_ms: Option<u64>,
}

async fn read_http_response_headers<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<ParsedResponseHeaders, PlatformApiError> {
    let mut line = String::new();
    let mut headers;

    loop {
        line.clear();
        let n = read_bounded_line(reader, &mut line, MAX_HEADER_LINE_BYTES).await?;
        if n == 0 {
            return Err(PlatformApiError::Transport {
                message: "connection closed before HTTP status line".to_owned(),
                source: None,
            });
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(rest) = trimmed
            .strip_prefix("HTTP/1.1 ")
            .or_else(|| trimmed.strip_prefix("HTTP/1.0 "))
            .or_else(|| trimmed.strip_prefix("HTTP/2 "))
        else {
            continue;
        };
        let code_str = rest.split_whitespace().next().unwrap_or("");
        let Ok(status) = code_str.parse::<u16>() else {
            return Err(PlatformApiError::Protocol {
                message: format!("malformed HTTP status line: {trimmed}"),
            });
        };
        let is_connect_tunnel =
            status == 200 && rest.to_ascii_lowercase().contains("connection established");
        headers = ParsedResponseHeaders {
            status,
            ..ParsedResponseHeaders::default()
        };
        loop {
            line.clear();
            let hn = read_bounded_line(reader, &mut line, MAX_HEADER_LINE_BYTES).await?;
            if hn == 0 || line.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                let name = k.trim();
                let val = v.trim();
                if name.eq_ignore_ascii_case("transfer-encoding")
                    && val.to_ascii_lowercase().contains("chunked")
                {
                    headers.chunked = true;
                } else if name.eq_ignore_ascii_case("content-length") {
                    headers.content_length = val.parse::<usize>().ok();
                } else if name.eq_ignore_ascii_case("retry-after")
                    && let Ok(secs) = val.parse::<u64>()
                {
                    headers.retry_after_ms = Some(secs.saturating_mul(1_000).min(60_000));
                } else if name.eq_ignore_ascii_case("retry-after-ms")
                    && let Ok(ms) = val.parse::<u64>()
                {
                    headers.retry_after_ms = Some(ms.min(60_000));
                }
            }
        }
        if (100..200).contains(&headers.status) {
            continue;
        }
        // Skip HTTP CONNECT proxy tunnel establishment blocks (`HTTP/1.1 200 Connection established`)
        // if emitted ahead of the upstream TLS response headers.
        if is_connect_tunnel {
            continue;
        }
        return Ok(headers);
    }
}

async fn read_bounded_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    out: &mut String,
    max_bytes: usize,
) -> Result<usize, PlatformApiError> {
    let mut raw = Vec::new();
    let mut total = 0usize;
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|e| PlatformApiError::Transport {
                message: "stream read failed".to_owned(),
                source: Some(e),
            })?;
        if available.is_empty() {
            break;
        }
        if let Some(nl_pos) = available.iter().position(|&b| b == b'\n') {
            let take = nl_pos + 1;
            if total.saturating_add(take) <= max_bytes
                && let Some(slice) = available.get(..take)
            {
                raw.extend_from_slice(slice);
            }
            reader.consume(take);
            total = total.saturating_add(take);
            break;
        }
        let len = available.len();
        if total.saturating_add(len) <= max_bytes {
            raw.extend_from_slice(available);
        }
        reader.consume(len);
        total = total.saturating_add(len);
        if total > max_bytes {
            return Err(PlatformApiError::Protocol {
                message: format!("HTTP/SSE line exceeded {max_bytes} byte safety bound"),
            });
        }
    }
    if !raw.is_empty() {
        out.push_str(&String::from_utf8_lossy(&raw));
        if out.ends_with('\n') {
            out.pop();
        }
    }
    Ok(total)
}

async fn read_http_body_bounded<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    headers: &ParsedResponseHeaders,
    max_bytes: usize,
) -> Result<String, PlatformApiError> {
    let mut buf = Vec::new();
    if headers.chunked {
        let mut size_line = String::new();
        loop {
            size_line.clear();
            let n = read_bounded_line(reader, &mut size_line, 256).await?;
            if n == 0 {
                break;
            }
            let hex_part = size_line.trim().split(';').next().unwrap_or("").trim();
            if hex_part.is_empty() {
                continue;
            }
            let chunk_len =
                usize::from_str_radix(hex_part, 16).map_err(|_| PlatformApiError::Protocol {
                    message: format!("invalid chunk size `{hex_part}`"),
                })?;
            if chunk_len == 0 {
                let _ = read_bounded_line(reader, &mut size_line, 256).await;
                break;
            }
            if chunk_len > MAX_HTTP_CHUNK_BYTES {
                return Err(PlatformApiError::Protocol {
                    message: format!(
                        "HTTP chunk size {chunk_len} exceeded {MAX_HTTP_CHUNK_BYTES} byte safety bound"
                    ),
                });
            }
            let mut scratch = [0u8; STREAM_READ_SCRATCH_BYTES];
            let mut remaining = chunk_len;
            while remaining > 0 {
                let step = remaining.min(scratch.len());
                let Some(buf_slice) = scratch.get_mut(..step) else {
                    break;
                };
                reader
                    .read_exact(buf_slice)
                    .await
                    .map_err(|e| PlatformApiError::Transport {
                        message: "truncated HTTP chunk".to_owned(),
                        source: Some(e),
                    })?;
                let remaining_cap = max_bytes.saturating_sub(buf.len());
                if remaining_cap > 0
                    && let Some(keep) = buf_slice.get(..step.min(remaining_cap))
                {
                    buf.extend_from_slice(keep);
                }
                remaining -= step;
            }
            let mut crlf = [0u8; 2];
            let _ = reader.read_exact(&mut crlf).await;
        }
    } else if let Some(len) = headers.content_length {
        let to_read = len.min(max_bytes);
        buf.resize(to_read, 0);
        reader
            .read_exact(&mut buf)
            .await
            .map_err(|e| PlatformApiError::Transport {
                message: "truncated Content-Length body".to_owned(),
                source: Some(e),
            })?;
    } else {
        let mut limited = reader.take(max_bytes as u64);
        limited
            .read_to_end(&mut buf)
            .await
            .map_err(|e| PlatformApiError::Transport {
                message: "failed reading response body".to_owned(),
                source: Some(e),
            })?;
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn write_http1_request(
    stream: &mut TcpStream,
    parsed: &ParsedHttpUrl,
    req: &HttpRequestSpec,
) -> Result<(), PlatformApiError> {
    let body_bytes = req.body.as_deref().unwrap_or(&[]);
    let mut head = String::with_capacity(512);
    head.push_str(&format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        req.method, parsed.path_and_query, parsed.host_header
    ));
    let mut has_content_len = false;
    for (k, v) in &req.headers {
        if k.eq_ignore_ascii_case("host") || k.eq_ignore_ascii_case("connection") {
            continue;
        }
        if k.eq_ignore_ascii_case("content-length") {
            has_content_len = true;
        }
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    if !has_content_len && (!body_bytes.is_empty() || req.method == "POST") {
        head.push_str(&format!("Content-Length: {}\r\n", body_bytes.len()));
    }
    head.push_str("\r\n");

    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| PlatformApiError::Transport {
            message: "failed writing HTTP request headers".to_owned(),
            source: Some(e),
        })?;
    if !body_bytes.is_empty() {
        stream
            .write_all(body_bytes)
            .await
            .map_err(|e| PlatformApiError::Transport {
                message: "failed writing HTTP request body".to_owned(),
                source: Some(e),
            })?;
    }
    stream
        .flush()
        .await
        .map_err(|e| PlatformApiError::Transport {
            message: "failed flushing HTTP request".to_owned(),
            source: Some(e),
        })
}

async fn open_tokio_http1_sse(
    parsed: &ParsedHttpUrl,
    req: HttpRequestSpec,
) -> Result<HttpSseStream, PlatformApiError> {
    let total_timeout_ms = u64::try_from(req.total_timeout.as_millis()).unwrap_or(u64::MAX);
    let deadline = Instant::now() + req.total_timeout;
    let connect_limit = req.connect_timeout.min(req.total_timeout);
    let connect_fut = TcpStream::connect((parsed.host.as_str(), parsed.port));
    let mut stream = timeout(connect_limit, connect_fut)
        .await
        .map_err(|_| PlatformApiError::Timeout {
            elapsed_ms: u64::try_from(connect_limit.as_millis()).unwrap_or(u64::MAX),
        })?
        .map_err(|e| PlatformApiError::Transport {
            message: format!("connect failed to {}:{}", parsed.host, parsed.port),
            source: Some(e),
        })?;
    let _ = stream.set_nodelay(true);

    timeout_at(deadline, write_http1_request(&mut stream, parsed, &req))
        .await
        .map_err(|_| PlatformApiError::Timeout {
            elapsed_ms: total_timeout_ms,
        })??;

    let mut reader = BufReader::new(stream);
    let header_deadline = Instant::now() + req.connect_timeout;
    let eff_header_deadline = header_deadline.min(deadline);
    let headers = timeout_at(eff_header_deadline, read_http_response_headers(&mut reader))
        .await
        .map_err(|_| PlatformApiError::Timeout {
            elapsed_ms: u64::try_from(req.connect_timeout.min(req.total_timeout).as_millis())
                .unwrap_or(u64::MAX),
        })??;

    if !(200..300).contains(&headers.status) {
        let err_body = timeout_at(
            deadline,
            read_http_body_bounded(&mut reader, &headers, MAX_ERROR_BODY_BYTES),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
        return Err(PlatformApiError::from_http_status(
            headers.status,
            headers.retry_after_ms,
            &err_body,
        ));
    }

    let (tx, rx) = mpsc::channel(128);
    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();
    let cancel_guard = cancel.drop_guard();
    let is_chunked = headers.chunked;

    tokio::spawn(async move {
        let res = pump_http1_sse_body(
            &mut reader,
            is_chunked,
            deadline,
            total_timeout_ms,
            &tx,
            &cancel_child,
        )
        .await;
        if let Err(err) = res {
            let _ = tx.send(Err(err)).await;
        }
    });

    Ok(HttpSseStream {
        status: headers.status,
        frames_rx: rx,
        _cancel_guard: cancel_guard,
    })
}

async fn pump_http1_sse_body<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    is_chunked: bool,
    deadline: Instant,
    total_timeout_ms: u64,
    tx: &mpsc::Sender<Result<SseFrame, PlatformApiError>>,
    cancel: &CancellationToken,
) -> Result<(), PlatformApiError> {
    let mut decoder = SseDecoder::new();
    if is_chunked {
        let mut size_line = String::new();
        let mut carry = Vec::<u8>::new();
        let mut scratch = [0u8; STREAM_READ_SCRATCH_BYTES];
        loop {
            size_line.clear();
            let read_fut = timeout_at(deadline, read_bounded_line(reader, &mut size_line, 256));
            let n = tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(()),
                r = read_fut => r.map_err(|_| PlatformApiError::Timeout { elapsed_ms: total_timeout_ms })??,
            };
            if n == 0 {
                break;
            }
            let hex_part = size_line.trim().split(';').next().unwrap_or("").trim();
            if hex_part.is_empty() {
                continue;
            }
            let chunk_len =
                usize::from_str_radix(hex_part, 16).map_err(|_| PlatformApiError::Protocol {
                    message: format!("invalid SSE chunk size `{hex_part}`"),
                })?;
            if chunk_len == 0 {
                break;
            }
            if chunk_len > MAX_HTTP_CHUNK_BYTES {
                return Err(PlatformApiError::Protocol {
                    message: format!(
                        "SSE chunk size {chunk_len} exceeded {MAX_HTTP_CHUNK_BYTES} byte safety bound"
                    ),
                });
            }
            let mut remaining = chunk_len;
            while remaining > 0 {
                let step = remaining.min(scratch.len());
                let Some(buf_slice) = scratch.get_mut(..step) else {
                    break;
                };
                let read_chunk = timeout_at(deadline, reader.read_exact(buf_slice));
                let read_res = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Ok(()),
                    r = read_chunk => r.map_err(|_| PlatformApiError::Timeout { elapsed_ms: total_timeout_ms })?,
                };
                read_res.map_err(|e| PlatformApiError::Transport {
                    message: "truncated SSE chunk".to_owned(),
                    source: Some(e),
                })?;
                remaining -= step;

                carry.extend_from_slice(buf_slice);
                while let Some(pos) = carry.iter().position(|&b| b == b'\n') {
                    let line_bytes: Vec<u8> = carry.drain(..=pos).collect();
                    let mut line_str = String::from_utf8_lossy(&line_bytes).into_owned();
                    if line_str.ends_with('\n') {
                        line_str.pop();
                    }
                    let maybe_frame = decoder.try_push_line(&line_str).map_err(|e| {
                        PlatformApiError::Protocol {
                            message: format!(
                                "SSE frame exceeded safety bound ({} bytes)",
                                e.attempted_bytes
                            ),
                        }
                    })?;
                    if let Some(frame) = maybe_frame
                        && tx.send(Ok(frame)).await.is_err()
                    {
                        return Ok(());
                    }
                }
                if carry.len() > MAX_SSE_LINE_BYTES {
                    return Err(PlatformApiError::Protocol {
                        message: "SSE line exceeded safety bound".to_owned(),
                    });
                }
            }
            let mut crlf = [0u8; 2];
            let _ = timeout_at(deadline, reader.read_exact(&mut crlf)).await;
        }
        if !carry.is_empty() {
            let tail = String::from_utf8_lossy(&carry);
            let maybe_frame =
                decoder
                    .try_push_line(&tail)
                    .map_err(|e| PlatformApiError::Protocol {
                        message: format!(
                            "SSE frame exceeded safety bound ({} bytes)",
                            e.attempted_bytes
                        ),
                    })?;
            if let Some(frame) = maybe_frame {
                let _ = tx.send(Ok(frame)).await;
            }
        }
    } else {
        let mut line = String::new();
        loop {
            line.clear();
            let read_fut = timeout_at(
                deadline,
                read_bounded_line(reader, &mut line, MAX_SSE_LINE_BYTES),
            );
            let n = tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(()),
                r = read_fut => r.map_err(|_| PlatformApiError::Timeout { elapsed_ms: total_timeout_ms })??,
            };
            if n == 0 {
                break;
            }
            let maybe_frame =
                decoder
                    .try_push_line(&line)
                    .map_err(|e| PlatformApiError::Protocol {
                        message: format!(
                            "SSE frame exceeded safety bound ({} bytes)",
                            e.attempted_bytes
                        ),
                    })?;
            if let Some(frame) = maybe_frame
                && tx.send(Ok(frame)).await.is_err()
            {
                return Ok(());
            }
        }
    }
    if let Some(tail_frame) = decoder.finish() {
        let _ = tx.send(Ok(tail_frame)).await;
    }
    Ok(())
}

async fn send_tokio_http1_buffered(
    parsed: &ParsedHttpUrl,
    req: HttpRequestSpec,
) -> Result<HttpJsonResponse, PlatformApiError> {
    let total_timeout_ms = u64::try_from(req.total_timeout.as_millis()).unwrap_or(u64::MAX);
    let deadline = Instant::now() + req.total_timeout;
    let connect_limit = req.connect_timeout.min(req.total_timeout);
    let connect_fut = TcpStream::connect((parsed.host.as_str(), parsed.port));
    let mut stream = timeout(connect_limit, connect_fut)
        .await
        .map_err(|_| PlatformApiError::Timeout {
            elapsed_ms: u64::try_from(connect_limit.as_millis()).unwrap_or(u64::MAX),
        })?
        .map_err(|e| PlatformApiError::Transport {
            message: format!("connect failed to {}:{}", parsed.host, parsed.port),
            source: Some(e),
        })?;
    timeout_at(deadline, write_http1_request(&mut stream, parsed, &req))
        .await
        .map_err(|_| PlatformApiError::Timeout {
            elapsed_ms: total_timeout_ms,
        })??;
    let mut reader = BufReader::new(stream);
    let headers = timeout_at(deadline, read_http_response_headers(&mut reader))
        .await
        .map_err(|_| PlatformApiError::Timeout {
            elapsed_ms: total_timeout_ms,
        })??;
    let body = timeout_at(
        deadline,
        read_http_body_bounded(&mut reader, &headers, 4 * 1024 * 1024),
    )
    .await
    .map_err(|_| PlatformApiError::Timeout {
        elapsed_ms: total_timeout_ms,
    })??;
    Ok(HttpJsonResponse {
        status: headers.status,
        retry_after_ms: headers.retry_after_ms,
        body,
    })
}

fn escape_curl_config_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 8);
    out.push('"');
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

struct TempBodyGuard(Option<std::path::PathBuf>);

impl Drop for TempBodyGuard {
    fn drop(&mut self) {
        if let Some(ref p) = self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn write_temp_request_body(bytes: &[u8]) -> Result<TempBodyGuard, PlatformApiError> {
    let path = std::env::temp_dir().join(format!("liftgate-req-{}.json", Uuid::new_v4()));
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| PlatformApiError::Transport {
                message: "failed creating temporary request payload file".to_owned(),
                source: Some(e),
            })?;
        f.write_all(bytes)
            .map_err(|e| PlatformApiError::Transport {
                message: "failed writing temporary request payload".to_owned(),
                source: Some(e),
            })?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, bytes).map_err(|e| PlatformApiError::Transport {
            message: "failed writing temporary request payload".to_owned(),
            source: Some(e),
        })?;
    }
    Ok(TempBodyGuard(Some(path)))
}

fn build_curl_stdin_config(req: &HttpRequestSpec, body_file: Option<&std::path::Path>) -> String {
    let mut cfg = String::with_capacity(1024);
    cfg.push_str(&format!("url = {}\n", escape_curl_config_string(&req.url)));
    cfg.push_str(&format!(
        "request = {}\n",
        escape_curl_config_string(req.method)
    ));
    for (k, v) in &req.headers {
        cfg.push_str(&format!(
            "header = {}\n",
            escape_curl_config_string(&format!("{k}: {v}"))
        ));
    }
    if let Some(proxy) = &req.proxy {
        cfg.push_str(&format!(
            "proxy = {}\n",
            escape_curl_config_string(proxy.as_str())
        ));
    } else if req.force_direct_proxy {
        cfg.push_str("noproxy = \"*\"\n");
    }
    if let Some(path) = body_file {
        cfg.push_str(&format!(
            "data-binary = {}\n",
            escape_curl_config_string(&format!("@{}", path.display()))
        ));
    }
    cfg
}

async fn open_curl_sse(req: HttpRequestSpec) -> Result<HttpSseStream, PlatformApiError> {
    let total_timeout_ms = u64::try_from(req.total_timeout.as_millis()).unwrap_or(u64::MAX);
    let deadline = Instant::now() + req.total_timeout;
    let temp_body = match req.body.as_deref() {
        Some(b) if !b.is_empty() => Some(write_temp_request_body(b)?),
        _ => None,
    };
    let config_text =
        build_curl_stdin_config(&req, temp_body.as_ref().and_then(|g| g.0.as_deref()));

    let curl_bin = if cfg!(windows) { "curl.exe" } else { "curl" };
    let mut cmd = Command::new(curl_bin);
    let connect_secs = req.connect_timeout.as_secs().max(5).to_string();
    let max_secs = req.total_timeout.as_secs().max(30).to_string();
    cmd.args([
        "-sS",
        "-N",
        "--http1.1",
        "--proto",
        "=http,https",
        "--suppress-connect-headers",
        "--connect-timeout",
        &connect_secs,
        "--max-time",
        &max_secs,
        "-D",
        "-",
        "--config",
        "-",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    if req.force_direct_proxy && req.proxy.is_none() {
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            cmd.env_remove(key);
        }
    }
    configure_command_group(&mut cmd);

    let mut child = cmd.spawn().map_err(|e| PlatformApiError::Transport {
        message: "failed to spawn curl for HTTPS SSE transport".to_owned(),
        source: Some(e),
    })?;
    let group_guard =
        ProcessGroupGuard::attach(&child).map_err(|e| PlatformApiError::Transport {
            message: format!("failed to attach curl process group: {e}"),
            source: None,
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(config_text.as_bytes())
            .await
            .map_err(|e| PlatformApiError::Transport {
                message: "failed writing curl stdin config".to_owned(),
                source: Some(e),
            })?;
        drop(stdin);
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| PlatformApiError::Transport {
            message: "missing curl stdout pipe".to_owned(),
            source: None,
        })?;
    let mut stderr = child.stderr.take();
    let mut reader = BufReader::new(stdout);

    let headers = match timeout_at(deadline, read_http_response_headers(&mut reader)).await {
        Ok(Ok(h)) => h,
        Ok(Err(err)) => {
            let mut err_buf = String::new();
            if let Some(ref mut se) = stderr {
                let _ = se.read_to_string(&mut err_buf).await;
            }
            if !err_buf.trim().is_empty() {
                return Err(PlatformApiError::Transport {
                    message: err_buf.trim().to_owned(),
                    source: None,
                });
            }
            return Err(err);
        }
        Err(_) => {
            return Err(PlatformApiError::Timeout {
                elapsed_ms: total_timeout_ms,
            });
        }
    };

    if !(200..300).contains(&headers.status) {
        let mut err_headers = headers;
        err_headers.chunked = false;
        let err_body = read_http_body_bounded(&mut reader, &err_headers, MAX_ERROR_BODY_BYTES)
            .await
            .unwrap_or_default();
        return Err(PlatformApiError::from_http_status(
            err_headers.status,
            err_headers.retry_after_ms,
            &err_body,
        ));
    }

    let (tx, rx) = mpsc::channel(128);
    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();
    let cancel_guard = cancel.drop_guard();

    tokio::spawn(async move {
        let _keep_group = group_guard;
        let _keep_body = temp_body;
        let res = pump_http1_sse_body(
            &mut reader,
            false,
            deadline,
            total_timeout_ms,
            &tx,
            &cancel_child,
        )
        .await;
        if let Err(err) = res {
            let _ = tx.send(Err(err)).await;
        }
        let _ = child.wait().await;
    });

    Ok(HttpSseStream {
        status: headers.status,
        frames_rx: rx,
        _cancel_guard: cancel_guard,
    })
}

async fn send_curl_buffered(req: HttpRequestSpec) -> Result<HttpJsonResponse, PlatformApiError> {
    let temp_body = match req.body.as_deref() {
        Some(b) if !b.is_empty() => Some(write_temp_request_body(b)?),
        _ => None,
    };
    let config_text =
        build_curl_stdin_config(&req, temp_body.as_ref().and_then(|g| g.0.as_deref()));

    let curl_bin = if cfg!(windows) { "curl.exe" } else { "curl" };
    let mut cmd = Command::new(curl_bin);
    let connect_secs = req.connect_timeout.as_secs().max(5).to_string();
    let max_secs = req.total_timeout.as_secs().max(20).to_string();
    cmd.args([
        "-sS",
        "--http1.1",
        "--proto",
        "=http,https",
        "--suppress-connect-headers",
        "--connect-timeout",
        &connect_secs,
        "--max-time",
        &max_secs,
        "-D",
        "-",
        "--config",
        "-",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    if req.force_direct_proxy && req.proxy.is_none() {
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            cmd.env_remove(key);
        }
    }
    configure_command_group(&mut cmd);

    let mut child = cmd.spawn().map_err(|e| PlatformApiError::Transport {
        message: "failed to spawn curl".to_owned(),
        source: Some(e),
    })?;
    let _group = ProcessGroupGuard::attach(&child).ok();
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(config_text.as_bytes()).await;
        drop(stdin);
    }
    let out = timeout(req.total_timeout, child.wait_with_output())
        .await
        .map_err(|_| PlatformApiError::Timeout {
            elapsed_ms: u64::try_from(req.total_timeout.as_millis()).unwrap_or(u64::MAX),
        })?
        .map_err(|e| PlatformApiError::Transport {
            message: "curl execution failed".to_owned(),
            source: Some(e),
        })?;
    drop(temp_body);

    if !out.status.success() && out.stdout.is_empty() {
        return Err(PlatformApiError::Transport {
            message: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            source: None,
        });
    }
    let mut reader = BufReader::new(out.stdout.as_slice());
    let mut headers = read_http_response_headers(&mut reader).await?;
    headers.chunked = false;
    let body = read_http_body_bounded(&mut reader, &headers, 4 * 1024 * 1024).await?;
    Ok(HttpJsonResponse {
        status: headers.status,
        retry_after_ms: headers.retry_after_ms,
        body,
    })
}
