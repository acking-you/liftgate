//! Loopback HTTP MCP tool bridge (`127.0.0.1:0`) enabling native `session/prompt` tool suspension
//! alongside coexisting `<agy_tool_call_{nonce}>` XML tools.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::net::Ipv4Addr;
use std::num::NonZeroUsize;
use std::sync::{
    Arc, Mutex, RwLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
pub use liftgate_tool_virtualizer::{
    CALL_CLIENT_TOOL_MCP_NAME, ClaudeCodeToolProfile, CodexToolProfile, DetectedHarness,
    GATEWAY_WEB_FETCH_MCP_NAME, GenericToolProfile, HarnessToolProfile, LocalReplyKind,
    MAX_NATIVE_MCP_TOOLS, McpBridgedTool, McpRoutingTable, McpSessionCatalog, McpSessionOptions,
    McpWireTool, NAMESPACE_HUB_MCP_PREFIX, OmpToolProfile, OpenCodeToolProfile, ProtocolHint,
    READ_CONTEXT_SPILL_MCP_NAME, SEARCH_CLIENT_TOOLS_MCP_NAME, ToolPlacement,
    VirtualizedCallDecision, VirtualizerBudget, apply_patch_input_schema, detect_harness,
    extract_custom_tool_input, freeform_custom_input_schema, gateway_web_fetch_input_schema,
    normalize_apply_patch_text, partition_tools_for_harness, partition_tools_for_mcp,
    partition_tools_for_mcp_session, partition_tools_for_mcp_session_with_options,
    partition_tools_for_mcp_with_hint, plan_virtualization, plan_virtualization_with_options,
    plan_virtualization_with_session_options, read_context_spill_input_schema,
    sanitize_mcp_tool_name, strip_call_prefix, validate_apply_patch_preflight,
};
use liftgate_types::{acp::McpServerConfig, canonical::CanonicalToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json, value::RawValue};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use uuid::Uuid;

/// Name of the loopback MCP server registered in `session/new.mcpServers` (`mcp_lg_<tool>`).
pub const LOOPBACK_MCP_SERVER_NAME: &str = "lg";

/// Prefix injected by `agy acp` onto tools exposed by [`LOOPBACK_MCP_SERVER_NAME`].
pub const ACP_MCP_TOOL_PREFIX: &str = "mcp_lg_";

const INITIAL_MCP_BODY_ALLOC_CAP: usize = 64 * 1024;
const MCP_BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);
const MCP_CANCEL_SOCKET_GRACE: Duration = Duration::from_millis(50);
const MCP_PROTOCOL_VERSION: &str = "2025-11-25";

/// Structured content block returned inside an MCP `tools/call` `CallToolResult`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpContentItem {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

/// Reply payload delivered into a parked loopback MCP `tools/call` HTTP request.
#[derive(Clone, Debug)]
pub struct McpToolCallReply {
    pub content: Vec<McpContentItem>,
    pub is_error: bool,
}

/// Live incoming `tools/call` invocation parked on the loopback MCP server awaiting client execution.
pub struct IncomingMcpToolCall {
    pub mcp_name: String,
    pub arguments: Value,
    pub reply_tx: oneshot::Sender<McpToolCallReply>,
}

impl fmt::Debug for IncomingMcpToolCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IncomingMcpToolCall")
            .field("mcp_name", &self.mcp_name)
            .finish_non_exhaustive()
    }
}

/// Resolution outcome for an incoming `POST /mcp/{token}` `tools/call` invocation.
#[derive(Debug)]
pub enum ResolvedMcpIncoming {
    /// Answered locally inside Liftgate (`search_client_tools`, schema preflight, or disabled tool).
    ImmediateLocalReply {
        reply_tx: oneshot::Sender<McpToolCallReply>,
        reply: McpToolCallReply,
        kind: LocalReplyKind,
    },
    /// Validated client tool call ready to suspend and emit over SSE/JSON to the caller.
    ForwardToClient {
        client_name: String,
        arguments: String,
        is_custom: bool,
        reply_tx: oneshot::Sender<McpToolCallReply>,
    },
    /// Server-managed `gateway_web_fetch` executed inside the gateway in-flight.
    GatewayWebFetch {
        url: String,
        reply_tx: oneshot::Sender<McpToolCallReply>,
    },
    /// Gateway-local 0-RTT context spill slice/search read (`mcp_lg_read_context_spill`).
    ReadContextSpill {
        path: String,
        start_line: usize,
        byte_offset: usize,
        max_lines: usize,
        query: Option<String>,
        reply_tx: oneshot::Sender<McpToolCallReply>,
    },
}

struct RegisteredMcpSession {
    secret_hex: String,
    wire_tools: Arc<[McpWireTool]>,
    call_tx: mpsc::Sender<IncomingMcpToolCall>,
}

type SessionRegistryMap = Mutex<HashMap<u64, Arc<RegisteredMcpSession>>>;

/// Per-session capability and channel handle owned by `SessionPool` and `turn_runner`.
///
/// Unregisters its endpoint from [`LoopbackMcpServer`] automatically on `Drop`.
pub struct McpSessionHandle {
    slot_id: u64,
    server_config: McpServerConfig,
    catalog: McpSessionCatalog,
    routing: RwLock<Arc<McpRoutingTable>>,
    call_rx: tokio::sync::Mutex<mpsc::Receiver<IncomingMcpToolCall>>,
    registry: Arc<SessionRegistryMap>,
}

impl fmt::Debug for McpSessionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpSessionHandle")
            .field("slot_id", &self.slot_id)
            .field("profile", &self.catalog.profile_id)
            .field("bridged_tools", &self.catalog.bridged_pairs.len())
            .field("virtualized_hubs", &self.catalog.virtualized_hubs.len())
            .finish_non_exhaustive()
    }
}

impl McpSessionHandle {
    #[must_use]
    pub const fn server_config(&self) -> &McpServerConfig {
        &self.server_config
    }

    #[must_use]
    pub const fn catalog(&self) -> &McpSessionCatalog {
        &self.catalog
    }

    #[must_use]
    pub const fn has_virtualized_dispatch(&self) -> bool {
        self.catalog.has_virtualized_dispatch
    }

    /// Builds a live MCP routing & virtualization telemetry summary for `SessionSummary.mcp`.
    #[must_use]
    pub fn telemetry_summary(
        &self,
        declared_tools_len: usize,
    ) -> liftgate_types::telemetry::SessionMcpSummary {
        self.catalog.telemetry_summary(declared_tools_len)
    }

    #[must_use]
    pub const fn call_rx(&self) -> &tokio::sync::Mutex<mpsc::Receiver<IncomingMcpToolCall>> {
        &self.call_rx
    }

    /// Synchronizes the session's live in-memory routing and search index with `client_tools`
    /// when a warm or resumed turn updates the active tool catalog.
    pub fn sync_client_tools(&self, client_tools: &[CanonicalToolSpec]) {
        let rebuilt_opt = {
            let guard = match self.routing.read() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.rebuild_if_changed(client_tools)
        };
        let Some(rebuilt_table) = rebuilt_opt else {
            return;
        };

        let rebuilt = Arc::new(rebuilt_table);
        match self.routing.write() {
            Ok(mut guard) => {
                *guard = rebuilt;
            }
            Err(poisoned) => {
                *poisoned.into_inner() = rebuilt;
            }
        }
    }

    /// Resolves an incoming loopback MCP `tools/call` against the live routing table, handling
    /// `search_client_tools`, Hub unwrapping, and required-parameter preflight locally.
    #[must_use]
    pub fn resolve_incoming_call(
        &self,
        incoming: IncomingMcpToolCall,
        active_tools: &[CanonicalToolSpec],
    ) -> ResolvedMcpIncoming {
        let table = {
            let guard = match self.routing.read() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            Arc::clone(&guard)
        };
        match table.resolve_call(&incoming.mcp_name, &incoming.arguments, active_tools) {
            VirtualizedCallDecision::LocalReply {
                text,
                is_error,
                kind,
            } => ResolvedMcpIncoming::ImmediateLocalReply {
                reply_tx: incoming.reply_tx,
                reply: McpToolCallReply {
                    content: vec![McpContentItem::Text { text }],
                    is_error,
                },
                kind,
            },
            VirtualizedCallDecision::ForwardToClient {
                client_name,
                arguments,
                is_custom,
            } => ResolvedMcpIncoming::ForwardToClient {
                client_name,
                arguments,
                is_custom,
                reply_tx: incoming.reply_tx,
            },
            VirtualizedCallDecision::GatewayWebFetch { url } => {
                ResolvedMcpIncoming::GatewayWebFetch {
                    url,
                    reply_tx: incoming.reply_tx,
                }
            }
            VirtualizedCallDecision::ReadContextSpill {
                path,
                start_line,
                byte_offset,
                max_lines,
                query,
            } => ResolvedMcpIncoming::ReadContextSpill {
                path,
                start_line,
                byte_offset,
                max_lines,
                query,
                reply_tx: incoming.reply_tx,
            },
        }
    }

    /// Estimates structural + heap bytes retained by this MCP handle while cached in `SessionPool`.
    #[must_use]
    pub fn estimated_retained_bytes(&self) -> usize {
        let pairs_bytes = self
            .catalog
            .bridged_pairs
            .iter()
            .fold(0usize, |acc, (c, m)| acc.saturating_add(c.len() + m.len()));
        let wire_tools_bytes = self.catalog.wire_tools.iter().fold(0usize, |acc, wt| {
            acc.saturating_add(std::mem::size_of::<McpWireTool>())
                .saturating_add(wt.name.len())
                .saturating_add(wt.description.as_deref().map_or(0, str::len))
        });
        let bridged_map_bytes = self.catalog.by_mcp_name.iter().fold(0usize, |acc, (k, b)| {
            acc.saturating_add(std::mem::size_of::<McpBridgedTool>())
                .saturating_add(k.len())
                .saturating_add(b.mcp_name.len())
                .saturating_add(b.client_name.len())
                .saturating_add(b.parameters_json.as_deref().map_or(0, str::len))
        });
        let routing_bytes = {
            let guard = match self.routing.read() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.estimated_retained_bytes()
        };
        std::mem::size_of::<Self>()
            .saturating_add(self.server_config.url.as_deref().map_or(0, str::len))
            .saturating_add(pairs_bytes)
            .saturating_add(wire_tools_bytes)
            .saturating_add(bridged_map_bytes)
            .saturating_add(routing_bytes)
    }
}

impl Drop for McpSessionHandle {
    fn drop(&mut self) {
        match self.registry.lock() {
            Ok(mut guard) => {
                let _ = guard.remove(&self.slot_id);
            }
            Err(poisoned) => {
                let _ = poisoned.into_inner().remove(&self.slot_id);
            }
        }
    }
}

#[derive(Clone)]
struct McpHttpState {
    registry: Arc<SessionRegistryMap>,
    max_body_bytes: NonZeroUsize,
    shutdown_rx: watch::Receiver<bool>,
}

/// Ephemeral loopback-only (`127.0.0.1:0`) Streamable HTTP MCP server multiplexing active ACP sessions.
pub struct LoopbackMcpServer {
    port: u16,
    channel_capacity: NonZeroUsize,
    next_slot_id: AtomicU64,
    registry: Arc<SessionRegistryMap>,
    shutdown_tx: watch::Sender<bool>,
    server_task: JoinHandle<()>,
}

impl fmt::Debug for LoopbackMcpServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoopbackMcpServer")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl LoopbackMcpServer {
    /// Binds a dedicated loopback-only (`127.0.0.1:0`) listener for `agy acp` MCP tool suspension.
    pub async fn bind(
        max_body_bytes: NonZeroUsize,
        channel_capacity: NonZeroUsize,
    ) -> std::io::Result<Arc<Self>> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let registry: Arc<SessionRegistryMap> = Arc::new(Mutex::new(HashMap::new()));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let http_state = McpHttpState {
            registry: Arc::clone(&registry),
            max_body_bytes,
            shutdown_rx: shutdown_rx.clone(),
        };
        let router = Router::new()
            .route("/mcp/{token}", post(handle_mcp_post))
            .layer(DefaultBodyLimit::disable())
            .with_state(http_state);

        let mut graceful_rx = shutdown_rx;
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = graceful_rx.wait_for(|&stop| stop).await;
                })
                .await;
        });

        Ok(Arc::new(Self {
            port,
            channel_capacity,
            next_slot_id: AtomicU64::new(1),
            registry,
            shutdown_tx,
            server_task,
        }))
    }

    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Registers a new per-session MCP capability token (`<16-hex-slot><48-hex-secret>`) and returns
    /// an RAII [`McpSessionHandle`] whose `Drop` unregisters the endpoint.
    #[must_use]
    pub fn register_session(&self, mut catalog: McpSessionCatalog) -> Arc<McpSessionHandle> {
        let routing = RwLock::new(Arc::clone(&catalog.routing_table));
        catalog.routing_table = Arc::new(McpRoutingTable::default());
        let slot_id = self.next_slot_id.fetch_add(1, Ordering::Relaxed);
        let raw_a = Uuid::new_v4().simple().to_string();
        let raw_b = Uuid::new_v4().simple().to_string();
        let combined = format!("{raw_a}{raw_b}");
        let secret_hex = combined.get(..48).unwrap_or(&raw_a).to_owned();
        let full_token = format!("{slot_id:016x}{secret_hex}");
        let url = format!("http://127.0.0.1:{}/mcp/{full_token}", self.port);

        let (call_tx, call_rx) = mpsc::channel(self.channel_capacity.get().clamp(8, 64));
        let registered = Arc::new(RegisteredMcpSession {
            secret_hex,
            wire_tools: Arc::clone(&catalog.wire_tools),
            call_tx,
        });
        {
            match self.registry.lock() {
                Ok(mut guard) => {
                    guard.insert(slot_id, registered);
                }
                Err(poisoned) => {
                    poisoned.into_inner().insert(slot_id, registered);
                }
            }
        }

        let server_config = McpServerConfig {
            name: LOOPBACK_MCP_SERVER_NAME.to_owned(),
            transport_type: Some("http".to_owned()),
            url: Some(url),
            command: None,
            args: Vec::new(),
        };

        Arc::new(McpSessionHandle {
            slot_id,
            server_config,
            catalog,
            routing,
            call_rx: tokio::sync::Mutex::new(call_rx),
            registry: Arc::clone(&self.registry),
        })
    }
}

impl Drop for LoopbackMcpServer {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send_replace(true);
        self.server_task.abort();
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (&lhs, &rhs) in a.iter().zip(b.iter()) {
        diff |= lhs ^ rhs;
    }
    diff == 0
}

fn lookup_authenticated_session(
    registry: &SessionRegistryMap,
    token: &str,
) -> Option<Arc<RegisteredMcpSession>> {
    if token.len() < 32 || token.len() > 128 {
        return None;
    }
    let (slot_hex, secret_part) = token.split_at_checked(16)?;
    if secret_part.is_empty() || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let slot_id = u64::from_str_radix(slot_hex, 16).ok()?;
    let candidate = match registry.lock() {
        Ok(guard) => guard.get(&slot_id).cloned(),
        Err(poisoned) => poisoned.into_inner().get(&slot_id).cloned(),
    }?;
    constant_time_eq(candidate.secret_hex.as_bytes(), secret_part.as_bytes()).then_some(candidate)
}

#[derive(Deserialize)]
struct McpRpcEnvelope<'a> {
    #[serde(default)]
    id: Option<Box<RawValue>>,
    #[serde(default, borrow)]
    method: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    params: Option<&'a RawValue>,
}

fn empty_json_object() -> Value {
    Value::Object(serde_json::Map::new())
}

#[derive(Deserialize)]
struct McpToolsCallParams {
    name: String,
    #[serde(default = "empty_json_object")]
    arguments: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct McpCallToolWireResult {
    content: Vec<McpContentItem>,
    is_error: bool,
}

enum BoundedReadError {
    TooLarge,
    BadRequest,
}

async fn read_bounded_mcp_body(req: Request, max_body: usize) -> Result<Vec<u8>, BoundedReadError> {
    let header_len = parse_mcp_content_length(req.headers(), max_body)?;
    let read_deadline = tokio::time::Instant::now() + MCP_BODY_READ_TIMEOUT;
    let mut body = req.into_body();
    let mut buf = Vec::with_capacity(header_len.unwrap_or(0).min(INITIAL_MCP_BODY_ALLOC_CAP));

    loop {
        let next_frame =
            tokio::time::timeout_at(read_deadline, http_body_util::BodyExt::frame(&mut body))
                .await
                .map_err(|_| BoundedReadError::BadRequest)?;
        let Some(frame_res) = next_frame else {
            break;
        };
        let frame = frame_res.map_err(|_| BoundedReadError::BadRequest)?;
        let Ok(chunk) = frame.into_data() else {
            continue;
        };
        if chunk.is_empty() {
            continue;
        }
        let next_len = buf.len().saturating_add(chunk.len());
        if next_len > max_body {
            return Err(BoundedReadError::TooLarge);
        }
        if let Some(exact) = header_len
            && next_len > exact
        {
            return Err(BoundedReadError::BadRequest);
        }
        buf.extend_from_slice(&chunk);
    }

    if let Some(exact) = header_len
        && buf.len() != exact
    {
        return Err(BoundedReadError::BadRequest);
    }
    Ok(buf)
}

fn parse_mcp_content_length(
    headers: &HeaderMap,
    max_body: usize,
) -> Result<Option<usize>, BoundedReadError> {
    let Some(raw) = headers.get(header::CONTENT_LENGTH) else {
        return Ok(None);
    };
    let trimmed = raw
        .to_str()
        .map_err(|_| BoundedReadError::BadRequest)?
        .trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return Err(BoundedReadError::BadRequest);
    }
    let len = trimmed
        .parse::<usize>()
        .map_err(|_| BoundedReadError::TooLarge)?;
    if len > max_body {
        return Err(BoundedReadError::TooLarge);
    }
    Ok(Some(len))
}

fn json_rpc_result_response<R: Serialize>(id: &RawValue, result: R) -> Response {
    #[derive(Serialize)]
    struct RpcOk<'a, T> {
        jsonrpc: &'static str,
        id: &'a RawValue,
        result: T,
    }

    let body = serde_json::to_vec(&RpcOk {
        jsonrpc: "2.0",
        id,
        result,
    })
    .unwrap_or_else(|_| b"{\"jsonrpc\":\"2.0\",\"id\":null,\"result\":{}}".to_vec());
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        Bytes::from(body),
    )
        .into_response()
}

fn json_rpc_error_response(id: &RawValue, code: i64, message: &'static str) -> Response {
    #[derive(Serialize)]
    struct RpcErr<'a> {
        jsonrpc: &'static str,
        id: &'a RawValue,
        error: RpcErrBody,
    }

    #[derive(Serialize)]
    struct RpcErrBody {
        code: i64,
        message: &'static str,
    }

    let body = serde_json::to_vec(&RpcErr {
        jsonrpc: "2.0",
        id,
        error: RpcErrBody { code, message },
    })
    .unwrap_or_else(|_| {
        b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32603,\"message\":\"Internal error\"}}"
            .to_vec()
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        Bytes::from(body),
    )
        .into_response()
}

async fn wait_mcp_shutdown(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|&stop| stop).await;
}

async fn handle_mcp_post(
    State(state): State<McpHttpState>,
    Path(token): Path<String>,
    req: Request,
) -> Response {
    let Some(session) = lookup_authenticated_session(&state.registry, token.trim()) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };

    let body = match read_bounded_mcp_body(req, state.max_body_bytes.get()).await {
        Ok(b) => b,
        Err(BoundedReadError::TooLarge) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        Err(BoundedReadError::BadRequest) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let envelope: McpRpcEnvelope<'_> = match serde_json::from_slice(&body) {
        Ok(env) => env,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let Some(method) = envelope.method else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    // JSON-RPC notifications (`notifications/initialized`, `notifications/roots/list_changed`, etc.)
    // carry no `id` and expect `202 Accepted` with an empty body.
    let Some(req_id) = envelope.id else {
        return StatusCode::ACCEPTED.into_response();
    };

    match method.as_ref() {
        // `agy acp` probes `server/discover` (`2026-07-28`) first and immediately falls back in
        // <1ms to standard `initialize` on JSON-RPC `-32601 Method not found`.
        "server/discover" => json_rpc_error_response(&req_id, -32601, "Method not found"),
        "initialize" => json_rpc_result_response(
            &req_id,
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "liftgate",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        ),
        "ping" => json_rpc_result_response(&req_id, json!({})),
        "tools/list" => json_rpc_result_response(
            &req_id,
            json!({
                "tools": &*session.wire_tools
            }),
        ),
        "tools/call" => {
            let Some(params_raw) = envelope.params else {
                return json_rpc_error_response(&req_id, -32602, "Invalid params");
            };
            let parsed_params: McpToolsCallParams = match serde_json::from_str(params_raw.get()) {
                Ok(p) => p,
                Err(_) => return json_rpc_error_response(&req_id, -32602, "Invalid params"),
            };

            let (reply_tx, reply_rx) = oneshot::channel::<McpToolCallReply>();
            let call_msg = IncomingMcpToolCall {
                mcp_name: parsed_params.name,
                arguments: parsed_params.arguments,
                reply_tx,
            };

            let mut shutdown_rx = state.shutdown_rx.clone();
            let sent = tokio::select! {
                biased;
                () = wait_mcp_shutdown(&mut shutdown_rx) => {
                    return json_rpc_error_response(&req_id, -32000, "Server shutting down");
                }
                res = session.call_tx.send(call_msg) => res.is_ok(),
            };
            if !sent {
                tokio::select! {
                    biased;
                    () = wait_mcp_shutdown(&mut shutdown_rx) => {}
                    () = tokio::time::sleep(MCP_CANCEL_SOCKET_GRACE) => {}
                }
                return json_rpc_error_response(&req_id, -32000, "Session closed");
            }

            tokio::select! {
                biased;
                () = wait_mcp_shutdown(&mut shutdown_rx) => {
                    json_rpc_error_response(&req_id, -32000, "Server shutting down")
                }
                reply_res = reply_rx => {
                    match reply_res {
                        Ok(reply) => json_rpc_result_response(
                            &req_id,
                            McpCallToolWireResult {
                                content: reply.content,
                                is_error: reply.is_error,
                            },
                        ),
                        Err(_) => {
                            // Give `session/cancel` (`+1.23ms` socket abort inside `agy acp`) a
                            // 50ms head start before sending `-32000`, avoiding a spurious
                            // `GENERIC status=ERROR` step in `agy`'s session transcript.
                            let mut grace_shutdown_rx = state.shutdown_rx.clone();
                            tokio::select! {
                                biased;
                                () = wait_mcp_shutdown(&mut grace_shutdown_rx) => {}
                                () = tokio::time::sleep(MCP_CANCEL_SOCKET_GRACE) => {}
                            }
                            json_rpc_error_response(&req_id, -32000, "Tool call cancelled")
                        }
                    }
                }
            }
        }
        _ => json_rpc_error_response(&req_id, -32601, "Method not found"),
    }
}

#[cfg(test)]
mod tests {
    use liftgate_types::{canonical::CanonicalToolKind, session_key::NoopSpillSink};

    use super::*;

    #[tokio::test]
    async fn register_session_bridges_all_custom_and_function_tools_and_accounts_schema_bytes() {
        let srv = LoopbackMcpServer::bind(
            NonZeroUsize::new(1024 * 1024).expect("nonzero"),
            NonZeroUsize::new(16).expect("nonzero"),
        )
        .await
        .expect("bind loopback MCP server");
        let desc = "x".repeat(4_096);
        let params = format!(
            r#"{{"type":"object","properties":{{"q":{{"type":"string","description":"{}"}}}}}}"#,
            "y".repeat(2_048)
        );
        let tools = vec![
            CanonicalToolSpec {
                name: "custom_grammar_tool".to_owned(),
                description: Some("Freeform patch tool.".to_owned()),
                kind: CanonicalToolKind::Custom { format_json: None },
            },
            CanonicalToolSpec {
                name: "search_docs".to_owned(),
                description: Some(desc.clone()),
                kind: CanonicalToolKind::Function {
                    parameters_json: params.clone(),
                },
            },
        ];
        let cat = partition_tools_for_mcp(&tools, &mut NoopSpillSink);
        assert_eq!(cat.bridged_pairs.len(), 2);

        let handle = srv.register_session(cat);
        assert!(handle.has_virtualized_dispatch());
        assert!(handle.estimated_retained_bytes() >= desc.len() + params.len());
    }
}
