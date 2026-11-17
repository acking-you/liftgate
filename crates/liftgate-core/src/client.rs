use std::fmt::Write as _;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use liftgate_server::routes::ADMIN_SHUTDOWN_HEADER;
use liftgate_types::{acp::AvailableModel, telemetry::GatewayTelemetrySnapshot};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout_at};

use crate::error::CliError;
use crate::state::format_connect_url;

const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SSE_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DiscoveredModel {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub context_window: u64,
    #[serde(default)]
    pub auto_compact_token_limit: u64,
}

impl DiscoveredModel {
    /// Uses the live gateway's complete context policy, or a conservative fallback for old servers.
    #[must_use]
    pub fn context_limits(&self, long_context: bool) -> (u64, u64) {
        if self.auto_compact_token_limit >= 100_000
            && self.auto_compact_token_limit <= self.context_window
            && self.context_window <= 1_000_000
        {
            (self.context_window, self.auto_compact_token_limit)
        } else {
            liftgate_types::model::context_limits(&self.id, long_context)
        }
    }

    #[must_use]
    pub fn to_available_model(&self) -> AvailableModel {
        AvailableModel {
            model_id: self.id.clone(),
            name: if self.display_name.is_empty() {
                self.id.clone()
            } else {
                self.display_name.clone()
            },
        }
    }
}

/// Infers whether a running gateway has experimental `--long-context` active from its `/v1/models`
/// catalog, falling back to `managed_fallback` only when no Gemini model has reported yet.
///
/// Legacy gateways that omit `auto_compact_token_limit` (`0`) always resolve to `false` so
/// local preferences never promote an older server into experimental 1M limits.
#[must_use]
pub fn infer_live_long_context(
    discovered: &[DiscoveredModel],
    managed_fallback: Option<bool>,
) -> bool {
    let mut saw_gemini = false;
    for model in discovered {
        if !model
            .id
            .trim()
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("gemini-"))
        {
            continue;
        }
        saw_gemini = true;
        if liftgate_types::model::is_gemini_long_context(
            model.context_window,
            model.auto_compact_token_limit,
        ) {
            return true;
        }
    }
    if saw_gemini {
        false
    } else {
        managed_fallback.unwrap_or(false)
    }
}

#[derive(Debug, Deserialize)]
struct DiscoveredModelList {
    #[serde(default)]
    data: Vec<DiscoveredModel>,
}

#[derive(Debug, Deserialize)]
struct CloseSessionResponse {
    #[serde(default)]
    closed: bool,
}

/// Proxy-immune HTTP/1.1 client speaking directly over loopback TCP to a local `liftgate` instance.
#[derive(Clone, Debug)]
pub struct GatewayClient {
    connect_addr: SocketAddr,
    base_url: String,
    host_header: String,
    api_key: Option<String>,
}

impl GatewayClient {
    #[must_use]
    pub fn new(listen: SocketAddr, api_key: Option<String>) -> Self {
        let connect_addr = match listen {
            SocketAddr::V4(v4) if v4.ip().is_unspecified() => {
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), v4.port())
            }
            SocketAddr::V6(v6) if v6.ip().is_unspecified() => {
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), v6.port())
            }
            other => other,
        };
        let base_url = format_connect_url(connect_addr);
        let host_header = match connect_addr {
            SocketAddr::V4(v4) => format!("{}:{}", v4.ip(), v4.port()),
            SocketAddr::V6(v6) => format!("[{}]:{}", v6.ip(), v6.port()),
        };
        let clean_key = api_key
            .map(|k| k.trim().to_owned())
            .filter(|k| !k.is_empty());
        Self {
            connect_addr,
            base_url,
            host_header,
            api_key: clean_key,
        }
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn check_health(&self, timeout_dur: Duration) -> bool {
        matches!(
            self.raw_request("GET", "/healthz", &[], timeout_dur).await,
            Ok((status, _)) if (200..300).contains(&status)
        )
    }

    pub async fn fetch_models(&self) -> Result<Vec<DiscoveredModel>, CliError> {
        let path = "/v1/models";
        let url = format!("{}{path}", self.base_url);
        let (status, body) = self.authed_get(path, DEFAULT_TIMEOUT).await?;
        if !(200..300).contains(&status) {
            return Err(CliError::GatewayHttpStatus {
                url,
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        let parsed: DiscoveredModelList = serde_json::from_slice(&body)
            .map_err(|source| CliError::GatewayBadJson { url, source })?;
        Ok(parsed.data)
    }

    pub async fn fetch_codex_catalog_json_with_mode(
        &self,
        default_model: &str,
        tool_mode: Option<&str>,
    ) -> Result<Vec<u8>, CliError> {
        let encoded = percent_encode_query_component(default_model);
        let path = match tool_mode.map(str::trim).filter(|s| !s.is_empty()) {
            Some(mode) => {
                let encoded_mode = percent_encode_query_component(mode);
                format!("/v1/codex-models?default_model={encoded}&tool_mode={encoded_mode}")
            }
            None => format!("/v1/codex-models?default_model={encoded}"),
        };
        let url = format!("{}{path}", self.base_url);
        let (status, body) = self.authed_get(&path, DEFAULT_TIMEOUT).await?;
        if !(200..300).contains(&status) {
            return Err(CliError::GatewayHttpStatus {
                url,
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        let _: serde::de::IgnoredAny = serde_json::from_slice(&body)
            .map_err(|source| CliError::GatewayBadJson { url, source })?;
        Ok(body)
    }

    pub async fn fetch_telemetry(&self) -> Result<GatewayTelemetrySnapshot, CliError> {
        let path = "/api/telemetry/snapshot";
        let url = format!("{}{path}", self.base_url);
        let (status, body) = self.authed_get(path, DEFAULT_TIMEOUT).await?;
        if !(200..300).contains(&status) {
            return Err(CliError::GatewayHttpStatus {
                url,
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        serde_json::from_slice(&body).map_err(|source| CliError::GatewayBadJson { url, source })
    }

    pub async fn close_idle_session(&self, session_id: &str) -> Result<bool, CliError> {
        let encoded = percent_encode_query_component(session_id);
        let path = format!("/api/telemetry/sessions/{encoded}/close");
        let url = format!("{}{path}", self.base_url);
        let (status, body) = self.authed_request("POST", &path, DEFAULT_TIMEOUT).await?;
        if !(200..300).contains(&status) {
            return Err(CliError::GatewayHttpStatus {
                url,
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        let parsed: CloseSessionResponse = serde_json::from_slice(&body)
            .map_err(|source| CliError::GatewayBadJson { url, source })?;
        Ok(parsed.closed)
    }

    pub async fn request_shutdown(&self, shutdown_token: &str) -> Result<(), CliError> {
        let path = "/api/admin/shutdown";
        let url = format!("{}{path}", self.base_url);
        let headers = [(ADMIN_SHUTDOWN_HEADER, shutdown_token)];
        let (status, body) = self
            .raw_request("POST", path, &headers, Duration::from_secs(3))
            .await?;
        if !(200..300).contains(&status) {
            return Err(CliError::GatewayHttpStatus {
                url,
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        Ok(())
    }

    /// Subscribes to `/api/telemetry/events` over loopback SSE, streaming parsed
    /// latest [`GatewayTelemetrySnapshot`] frames into `tx` (coalescing bursts in O(1) memory)
    /// until `cancel` triggers, all receivers drop, or the connection closes.
    pub async fn subscribe_telemetry_events(
        &self,
        tx: tokio::sync::watch::Sender<Option<GatewayTelemetrySnapshot>>,
        mut cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), CliError> {
        let path = "/api/telemetry/events";
        let full_url = format!("{}{path}", self.base_url);
        let mut req_buf = String::with_capacity(256);
        let _ = write!(
            req_buf,
            "GET {path} HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n",
            self.host_header
        );
        if let Some(key) = &self.api_key {
            let clean_key: String = key.chars().filter(|&c| c != '\r' && c != '\n').collect();
            let _ = write!(req_buf, "Authorization: Bearer {clean_key}\r\n");
        }
        req_buf.push_str("\r\n");

        let connect_deadline = Instant::now() + Duration::from_secs(5);
        let mut stream =
            match timeout_at(connect_deadline, TcpStream::connect(self.connect_addr)).await {
                Ok(Ok(s)) => s,
                Ok(Err(source)) => {
                    return Err(CliError::GatewayUnreachable {
                        url: full_url,
                        source,
                    });
                }
                Err(_) => {
                    return Err(CliError::GatewayUnreachable {
                        url: full_url,
                        source: io::Error::new(io::ErrorKind::TimedOut, "SSE connect timed out"),
                    });
                }
            };
        stream
            .write_all(req_buf.as_bytes())
            .await
            .map_err(|source| CliError::GatewayUnreachable {
                url: full_url.clone(),
                source,
            })?;

        let mut decoder = IncrementalSseDecoder::new();
        let mut read_buf = [0u8; 16_384];
        loop {
            if *cancel.borrow() || tx.is_closed() {
                break;
            }
            tokio::select! {
                biased;
                _ = cancel.wait_for(|&stopped| stopped) => break,
                () = tx.closed() => break,
                read_res = stream.read(&mut read_buf) => {
                    let n = read_res.map_err(|source| CliError::GatewayUnreachable {
                        url: full_url.clone(),
                        source,
                    })?;
                    if n == 0 {
                        break;
                    }
                    let Some(bytes) = read_buf.get(..n) else {
                        continue;
                    };
                    match decoder.push_bytes(bytes, &full_url)? {
                        SseFeedOutcome::Continue(snapshots) => {
                            if let Some(latest) = snapshots.into_iter().last()
                                && tx.send(Some(latest)).is_err()
                            {
                                return Ok(());
                            }
                        }
                        SseFeedOutcome::EndOfStream(snapshots) => {
                            if let Some(latest) = snapshots.into_iter().last() {
                                let _ = tx.send(Some(latest));
                            }
                            break;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn authed_get(
        &self,
        path_and_query: &str,
        timeout_dur: Duration,
    ) -> Result<(u16, Vec<u8>), CliError> {
        self.authed_request("GET", path_and_query, timeout_dur)
            .await
    }

    async fn authed_request(
        &self,
        method: &str,
        path_and_query: &str,
        timeout_dur: Duration,
    ) -> Result<(u16, Vec<u8>), CliError> {
        if let Some(key) = &self.api_key {
            let auth_val = format!("Bearer {key}");
            let headers = [("Authorization", auth_val.as_str())];
            self.raw_request(method, path_and_query, &headers, timeout_dur)
                .await
        } else {
            self.raw_request(method, path_and_query, &[], timeout_dur)
                .await
        }
    }

    async fn raw_request(
        &self,
        method: &str,
        path_and_query: &str,
        extra_headers: &[(&str, &str)],
        timeout_dur: Duration,
    ) -> Result<(u16, Vec<u8>), CliError> {
        let full_url = format!("{}{path_and_query}", self.base_url);
        let deadline = Instant::now() + timeout_dur;

        let mut req_buf = String::with_capacity(256);
        let _ = write!(
            req_buf,
            "{method} {path_and_query} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: application/json\r\n",
            self.host_header
        );
        if method == "POST" {
            req_buf.push_str("Content-Length: 0\r\n");
        }
        for (name, val) in extra_headers {
            let clean_val: String = val.chars().filter(|&c| c != '\r' && c != '\n').collect();
            let _ = write!(req_buf, "{name}: {clean_val}\r\n");
        }
        req_buf.push_str("\r\n");

        let io_fut = async {
            let mut stream = TcpStream::connect(self.connect_addr).await?;
            stream.write_all(req_buf.as_bytes()).await?;
            let mut raw = Vec::with_capacity(4096);
            stream
                .take(MAX_RESPONSE_BYTES)
                .read_to_end(&mut raw)
                .await?;
            parse_http_response(&raw)
        };

        match timeout_at(deadline, io_fut).await {
            Ok(Ok(res)) => Ok(res),
            Ok(Err(source)) => Err(CliError::GatewayUnreachable {
                url: full_url,
                source,
            }),
            Err(_) => Err(CliError::GatewayUnreachable {
                url: full_url,
                source: io::Error::new(io::ErrorKind::TimedOut, "HTTP request timed out"),
            }),
        }
    }
}

#[must_use]
pub fn percent_encode_query_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn parse_http_response(raw: &[u8]) -> io::Result<(u16, Vec<u8>)> {
    let Some(header_end) = find_double_crlf(raw) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incomplete HTTP response headers",
        ));
    };
    let header_bytes = raw.get(..header_end).unwrap_or_default();
    let body_bytes = raw.get(header_end + 4..).unwrap_or_default();
    let header_str = std::str::from_utf8(header_bytes).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "non-UTF8 HTTP response headers")
    })?;

    let mut lines = header_str.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing HTTP status line"))?;
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP status code"))?;

    let is_chunked = lines.any(|line| {
        line.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
        })
    });

    let decoded_body = if is_chunked {
        decode_chunked_body(body_bytes)?
    } else {
        body_bytes.to_vec()
    };
    Ok((status_code, decoded_body))
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn decode_chunked_body(mut rest: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(crlf) = rest.windows(2).position(|w| w == b"\r\n") {
        let size_line = std::str::from_utf8(rest.get(..crlf).unwrap_or_default())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid chunk header"))?;
        let hex_part = size_line.split(';').next().unwrap_or_default().trim();
        let chunk_len = usize::from_str_radix(hex_part, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid chunk length"))?;
        rest = rest.get(crlf + 2..).unwrap_or_default();
        if chunk_len == 0 {
            break;
        }
        let Some(chunk_data) = rest.get(..chunk_len) else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated HTTP chunk",
            ));
        };
        out.extend_from_slice(chunk_data);
        rest = rest.get(chunk_len + 2..).unwrap_or_default();
    }
    Ok(out)
}

enum SseFeedOutcome {
    Continue(Vec<GatewayTelemetrySnapshot>),
    EndOfStream(Vec<GatewayTelemetrySnapshot>),
}

struct IncrementalSseDecoder {
    headers_parsed: bool,
    is_chunked: bool,
    raw_buf: Vec<u8>,
    sse_buf: Vec<u8>,
}

impl IncrementalSseDecoder {
    fn new() -> Self {
        Self {
            headers_parsed: false,
            is_chunked: false,
            raw_buf: Vec::with_capacity(8192),
            sse_buf: Vec::with_capacity(8192),
        }
    }

    fn push_bytes(&mut self, incoming: &[u8], url: &str) -> Result<SseFeedOutcome, CliError> {
        if self.raw_buf.len().saturating_add(incoming.len()) > MAX_SSE_BUFFER_BYTES {
            return Err(CliError::GatewayUnreachable {
                url: url.to_owned(),
                source: io::Error::new(io::ErrorKind::InvalidData, "SSE raw buffer exceeded 4 MiB"),
            });
        }
        self.raw_buf.extend_from_slice(incoming);

        if !self.headers_parsed {
            let Some(hdr_end) = find_double_crlf(&self.raw_buf) else {
                return Ok(SseFeedOutcome::Continue(Vec::new()));
            };
            let header_bytes = self.raw_buf.get(..hdr_end).unwrap_or_default();
            let header_str =
                std::str::from_utf8(header_bytes).map_err(|_| CliError::GatewayUnreachable {
                    url: url.to_owned(),
                    source: io::Error::new(io::ErrorKind::InvalidData, "non-UTF8 SSE headers"),
                })?;
            let mut lines = header_str.split("\r\n");
            let status = lines
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(0);
            self.is_chunked = lines.any(|line| {
                line.split_once(':').is_some_and(|(k, v)| {
                    k.trim().eq_ignore_ascii_case("transfer-encoding")
                        && v.to_ascii_lowercase().contains("chunked")
                })
            });
            self.raw_buf.drain(..hdr_end + 4);
            self.headers_parsed = true;
            if !(200..300).contains(&status) {
                return Err(CliError::GatewayHttpStatus {
                    url: url.to_owned(),
                    status,
                    body: String::from_utf8_lossy(&self.raw_buf).into_owned(),
                });
            }
        }

        let mut reached_eof = false;
        if self.is_chunked {
            while let Some(crlf) = self.raw_buf.windows(2).position(|w| w == b"\r\n") {
                let size_str = std::str::from_utf8(self.raw_buf.get(..crlf).unwrap_or_default())
                    .map_err(|_| CliError::GatewayUnreachable {
                        url: url.to_owned(),
                        source: io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid SSE chunk header",
                        ),
                    })?;
                let hex_part = size_str.split(';').next().unwrap_or_default().trim();
                let chunk_len = usize::from_str_radix(hex_part, 16).map_err(|_| {
                    CliError::GatewayUnreachable {
                        url: url.to_owned(),
                        source: io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid SSE chunk length",
                        ),
                    }
                })?;
                if chunk_len == 0 {
                    reached_eof = true;
                    self.raw_buf.clear();
                    break;
                }
                let needed = crlf
                    .saturating_add(2)
                    .saturating_add(chunk_len)
                    .saturating_add(2);
                if self.raw_buf.len() < needed {
                    break;
                }
                if self.sse_buf.len().saturating_add(chunk_len) > MAX_SSE_BUFFER_BYTES {
                    return Err(CliError::GatewayUnreachable {
                        url: url.to_owned(),
                        source: io::Error::new(
                            io::ErrorKind::InvalidData,
                            "SSE payload exceeded 4 MiB",
                        ),
                    });
                }
                if let Some(payload) = self.raw_buf.get(crlf + 2..crlf + 2 + chunk_len) {
                    self.sse_buf.extend_from_slice(payload);
                }
                self.raw_buf.drain(..needed);
            }
        } else {
            if self.sse_buf.len().saturating_add(self.raw_buf.len()) > MAX_SSE_BUFFER_BYTES {
                return Err(CliError::GatewayUnreachable {
                    url: url.to_owned(),
                    source: io::Error::new(
                        io::ErrorKind::InvalidData,
                        "SSE payload exceeded 4 MiB",
                    ),
                });
            }
            self.sse_buf.append(&mut self.raw_buf);
        }

        let mut out = Vec::new();
        while let Some((frame_end, delim_len)) = find_sse_event_boundary(&self.sse_buf) {
            let frame_bytes = self.sse_buf.get(..frame_end).unwrap_or_default();
            if let Ok(frame_str) = std::str::from_utf8(frame_bytes)
                && let Some(snap) = parse_sse_snapshot_event(frame_str)
            {
                out.push(snap);
            }
            self.sse_buf.drain(..frame_end + delim_len);
        }

        if reached_eof {
            Ok(SseFeedOutcome::EndOfStream(out))
        } else {
            Ok(SseFeedOutcome::Continue(out))
        }
    }
}

fn find_sse_event_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    for (idx, win) in buf.windows(2).enumerate() {
        if win == b"\n\n" {
            return Some((idx, 2));
        }
        if win == b"\r\n" && buf.get(idx + 2..idx + 4) == Some(b"\r\n") {
            return Some((idx, 4));
        }
    }
    None
}

fn parse_sse_snapshot_event(frame: &str) -> Option<GatewayTelemetrySnapshot> {
    let mut event_type = "message";
    let mut data_lines: Vec<&str> = Vec::new();
    for raw_line in frame.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.starts_with(':') || line.is_empty() {
            continue;
        }
        if let Some(ev) = line.strip_prefix("event:") {
            event_type = ev.trim();
        } else if let Some(d) = line.strip_prefix("data:") {
            data_lines.push(d.strip_prefix(' ').unwrap_or(d));
        }
    }
    if event_type != "snapshot" || data_lines.is_empty() {
        return None;
    }
    let joined = data_lines.join("\n");
    serde_json::from_str(&joined).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_chunked_http_responses() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"status\":\"ok\"}";
        let (code, body) = parse_http_response(plain).unwrap();
        assert_eq!(code, 200);
        assert_eq!(body, b"{\"status\":\"ok\"}");

        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{\"ok\":1\r\n1\r\n}\r\n0\r\n\r\n";
        let (c2, b2) = parse_http_response(chunked).unwrap();
        assert_eq!(c2, 200);
        assert_eq!(b2, b"{\"ok\":1}");
    }

    #[test]
    fn infers_live_long_context_without_promoting_legacy_gateways() {
        let default_500k = vec![DiscoveredModel {
            id: "gemini-4-carbon-high".to_owned(),
            display_name: String::new(),
            context_window: 500_000,
            auto_compact_token_limit: 425_000,
        }];
        assert!(!infer_live_long_context(&default_500k, Some(true)));

        let default_v013 = vec![DiscoveredModel {
            id: "gemini-4-carbon-high".to_owned(),
            display_name: String::new(),
            context_window: 280_000,
            auto_compact_token_limit: 245_000,
        }];
        assert!(!infer_live_long_context(&default_v013, Some(true)));

        let experimental_v013 = vec![DiscoveredModel {
            id: "gemini-4-carbon-high".to_owned(),
            display_name: String::new(),
            context_window: 1_000_000,
            auto_compact_token_limit: 850_000,
        }];
        assert!(infer_live_long_context(&experimental_v013, None));

        let legacy_v012 = vec![DiscoveredModel {
            id: "gemini-4-carbon-high".to_owned(),
            display_name: String::new(),
            context_window: 1_000_000,
            auto_compact_token_limit: 0,
        }];
        assert!(!infer_live_long_context(&legacy_v012, Some(true)));

        let non_gemini_only = vec![DiscoveredModel {
            id: "claude-sonnet-5-5-high".to_owned(),
            display_name: String::new(),
            context_window: 200_000,
            auto_compact_token_limit: 170_000,
        }];
        assert!(infer_live_long_context(&non_gemini_only, Some(true)));
        assert!(!infer_live_long_context(&non_gemini_only, None));
    }
}
