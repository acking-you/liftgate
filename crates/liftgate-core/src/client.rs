use std::fmt::Write as _;
use std::net::SocketAddr;
use std::time::Duration;

pub use liftgate_harness::DiscoveredModel;
use liftgate_server::routes::ADMIN_SHUTDOWN_HEADER;
use liftgate_types::{config::GatewayEndpoint, telemetry::GatewayTelemetrySnapshot};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout_at};

use crate::error::{CliError, GatewayTransportError};

const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SSE_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Proxy-immune HTTP/1.1 client speaking directly over TCP to a local or remote `liftgate` instance.
#[derive(Clone, Debug)]
pub struct GatewayClient {
    endpoint: GatewayEndpoint,
    api_key: Option<String>,
}

impl GatewayClient {
    #[must_use]
    pub fn new(listen: SocketAddr, api_key: Option<String>) -> Self {
        Self::from_endpoint(GatewayEndpoint::from_socket_addr(listen), api_key)
    }

    #[must_use]
    pub fn from_endpoint(endpoint: GatewayEndpoint, api_key: Option<String>) -> Self {
        let clean_key = api_key
            .map(|k| k.trim().to_owned())
            .filter(|k| !k.is_empty());
        Self {
            endpoint,
            api_key: clean_key,
        }
    }

    #[must_use]
    pub const fn endpoint(&self) -> &GatewayEndpoint {
        &self.endpoint
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        self.endpoint.base_url()
    }

    pub async fn check_health(&self, timeout_dur: Duration) -> bool {
        matches!(
            self.raw_request("GET", "/healthz", &[], timeout_dur).await,
            Ok((status, _)) if (200..300).contains(&status)
        )
    }

    pub async fn fetch_models(&self) -> Result<Vec<DiscoveredModel>, CliError> {
        let path = "/v1/models";
        let url = format!("{}{path}", self.base_url());
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
        let url = format!("{}{path}", self.base_url());
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
        let url = format!("{}{path}", self.base_url());
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
        let url = format!("{}{path}", self.base_url());
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

    pub async fn set_vision_offload(&self, enabled: Option<bool>) -> Result<bool, CliError> {
        #[derive(serde::Deserialize)]
        struct VisionOffloadResponse {
            vision_offload_enabled: bool,
        }

        let path = match enabled {
            Some(true) => "/api/telemetry/vision-offload?enabled=true",
            Some(false) => "/api/telemetry/vision-offload?enabled=false",
            None => "/api/telemetry/vision-offload",
        };
        let url = format!("{}{path}", self.base_url());
        let (status, body) = self.authed_request("POST", path, DEFAULT_TIMEOUT).await?;
        if !(200..300).contains(&status) {
            return Err(CliError::GatewayHttpStatus {
                url,
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        let parsed: VisionOffloadResponse = serde_json::from_slice(&body)
            .map_err(|source| CliError::GatewayBadJson { url, source })?;
        Ok(parsed.vision_offload_enabled)
    }

    pub async fn set_mcp_tool_bridge(&self, enabled: Option<bool>) -> Result<bool, CliError> {
        #[derive(serde::Deserialize)]
        struct McpToolBridgeResponse {
            mcp_tool_bridge_enabled: bool,
        }

        let path = match enabled {
            Some(true) => "/api/telemetry/mcp-tool-bridge?enabled=true",
            Some(false) => "/api/telemetry/mcp-tool-bridge?enabled=false",
            None => "/api/telemetry/mcp-tool-bridge",
        };
        let url = format!("{}{path}", self.base_url());
        let (status, body) = self.authed_request("POST", path, DEFAULT_TIMEOUT).await?;
        if !(200..300).contains(&status) {
            return Err(CliError::GatewayHttpStatus {
                url,
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        let parsed: McpToolBridgeResponse = serde_json::from_slice(&body)
            .map_err(|source| CliError::GatewayBadJson { url, source })?;
        Ok(parsed.mcp_tool_bridge_enabled)
    }

    pub async fn request_shutdown(&self, shutdown_token: &str) -> Result<(), CliError> {
        let path = "/api/admin/shutdown";
        let url = format!("{}{path}", self.base_url());
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

    /// Subscribes to `/api/telemetry/events` over TCP SSE, streaming parsed
    /// latest [`GatewayTelemetrySnapshot`] frames into `tx` (coalescing bursts in O(1) memory)
    /// until `cancel` triggers, all receivers drop, or the connection closes.
    pub async fn subscribe_telemetry_events(
        &self,
        tx: tokio::sync::watch::Sender<Option<GatewayTelemetrySnapshot>>,
        mut cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), CliError> {
        let path = "/api/telemetry/events";
        let full_url = format!("{}{path}", self.base_url());
        let mut req_buf = String::with_capacity(256);
        let _ = write!(
            req_buf,
            "GET {path} HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n",
            self.endpoint.host_header()
        );
        if let Some(key) = &self.api_key {
            let clean_key: String = key.chars().filter(|&c| c != '\r' && c != '\n').collect();
            let _ = write!(req_buf, "Authorization: Bearer {clean_key}\r\n");
        }
        req_buf.push_str("\r\n");

        let connect_deadline = Instant::now() + Duration::from_secs(5);
        let connect_target = (self.endpoint.host(), self.endpoint.port());
        let mut stream =
            match timeout_at(connect_deadline, TcpStream::connect(connect_target)).await {
                Ok(Ok(s)) => s,
                Ok(Err(source)) => {
                    return Err(CliError::GatewayUnreachable {
                        url: full_url,
                        source: GatewayTransportError::Io(source),
                    });
                }
                Err(_) => {
                    return Err(CliError::GatewayUnreachable {
                        url: full_url,
                        source: GatewayTransportError::TimedOut,
                    });
                }
            };
        stream
            .write_all(req_buf.as_bytes())
            .await
            .map_err(|source| CliError::GatewayUnreachable {
                url: full_url.clone(),
                source: GatewayTransportError::Io(source),
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
                        source: GatewayTransportError::Io(source),
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
        let full_url = format!("{}{path_and_query}", self.base_url());
        let deadline = Instant::now() + timeout_dur;

        let mut req_buf = String::with_capacity(256);
        let _ = write!(
            req_buf,
            "{method} {path_and_query} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: application/json\r\n",
            self.endpoint.host_header()
        );
        if method == "POST" {
            req_buf.push_str("Content-Length: 0\r\n");
        }
        for (name, val) in extra_headers {
            let clean_val: String = val.chars().filter(|&c| c != '\r' && c != '\n').collect();
            let _ = write!(req_buf, "{name}: {clean_val}\r\n");
        }
        req_buf.push_str("\r\n");

        let connect_target = (self.endpoint.host(), self.endpoint.port());
        let io_fut = async {
            let mut stream = TcpStream::connect(connect_target).await?;
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
                source: GatewayTransportError::TimedOut,
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

fn parse_http_response(raw: &[u8]) -> Result<(u16, Vec<u8>), GatewayTransportError> {
    let Some(header_end) = find_double_crlf(raw) else {
        return Err(GatewayTransportError::IncompleteHeaders);
    };
    let header_bytes = raw.get(..header_end).unwrap_or_default();
    let body_bytes = raw.get(header_end + 4..).unwrap_or_default();
    let header_str =
        std::str::from_utf8(header_bytes).map_err(|_| GatewayTransportError::NonUtf8Headers)?;

    let mut lines = header_str.split("\r\n");
    let status_line = lines
        .next()
        .ok_or(GatewayTransportError::MissingStatusLine)?;
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or(GatewayTransportError::InvalidStatusCode)?;

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

fn decode_chunked_body(mut rest: &[u8]) -> Result<Vec<u8>, GatewayTransportError> {
    let mut out = Vec::new();
    while let Some(crlf) = rest.windows(2).position(|w| w == b"\r\n") {
        let size_line = std::str::from_utf8(rest.get(..crlf).unwrap_or_default())
            .map_err(|_| GatewayTransportError::InvalidChunkHeader)?;
        let hex_part = size_line.split(';').next().unwrap_or_default().trim();
        let chunk_len = usize::from_str_radix(hex_part, 16)
            .map_err(|_| GatewayTransportError::InvalidChunkLength)?;
        rest = rest.get(crlf + 2..).unwrap_or_default();
        if chunk_len == 0 {
            break;
        }
        let Some(chunk_data) = rest.get(..chunk_len) else {
            return Err(GatewayTransportError::TruncatedChunk);
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
                source: GatewayTransportError::SseBufferExceeded {
                    max_bytes: MAX_SSE_BUFFER_BYTES,
                },
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
                    source: GatewayTransportError::NonUtf8Headers,
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
                        source: GatewayTransportError::InvalidChunkHeader,
                    })?;
                let hex_part = size_str.split(';').next().unwrap_or_default().trim();
                let chunk_len = usize::from_str_radix(hex_part, 16).map_err(|_| {
                    CliError::GatewayUnreachable {
                        url: url.to_owned(),
                        source: GatewayTransportError::InvalidChunkLength,
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
                        source: GatewayTransportError::SseBufferExceeded {
                            max_bytes: MAX_SSE_BUFFER_BYTES,
                        },
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
                    source: GatewayTransportError::SseBufferExceeded {
                        max_bytes: MAX_SSE_BUFFER_BYTES,
                    },
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
    fn discovered_model_clamps_legacy_1m_windows_to_safe_500k_policy() {
        let default_500k = DiscoveredModel {
            id: "gemini-4-carbon-high".to_owned(),
            display_name: String::new(),
            context_window: 500_000,
            auto_compact_token_limit: 425_000,
        };
        assert_eq!(default_500k.context_limits(), (500_000, 425_000));

        let legacy_1m = DiscoveredModel {
            id: "gemini-4-carbon-high".to_owned(),
            display_name: String::new(),
            context_window: 1_000_000,
            auto_compact_token_limit: 850_000,
        };
        assert_eq!(legacy_1m.context_limits(), (500_000, 425_000));

        let legacy_zero_compact = DiscoveredModel {
            id: "gemini-4-carbon-high".to_owned(),
            display_name: String::new(),
            context_window: 1_000_000,
            auto_compact_token_limit: 0,
        };
        assert_eq!(legacy_zero_compact.context_limits(), (500_000, 425_000));

        let claude = DiscoveredModel {
            id: "claude-sonnet-5-5-high".to_owned(),
            display_name: String::new(),
            context_window: 200_000,
            auto_compact_token_limit: 170_000,
        };
        assert_eq!(claude.context_limits(), (200_000, 170_000));
    }
}
