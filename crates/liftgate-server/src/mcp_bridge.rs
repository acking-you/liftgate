//! Loopback HTTP MCP tool bridge (`127.0.0.1:0`) enabling native `session/prompt` tool suspension
//! alongside coexisting `<agy_tool_call_{nonce}>` XML tools.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::Ipv4Addr;
use std::num::NonZeroUsize;
use std::sync::{
    Arc, Mutex,
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
use liftgate_types::{
    acp::McpServerConfig,
    canonical::{CanonicalToolKind, CanonicalToolSpec},
    session_key::{
        TruncationSpillSink, compact_mcp_tool_description, is_code_mode_custom_exec,
        is_code_mode_orchestrator_tool,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json, value::RawValue};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use uuid::Uuid;

/// Maximum client tools exposed natively on the `lg` loopback MCP server per session; any
/// additional tools beyond this cap seamlessly fall back to `<client_tools>` XML coexistence.
pub const MAX_NATIVE_MCP_TOOLS: usize = 64;

/// Name of the loopback MCP server registered in `session/new.mcpServers` (`mcp_lg_<tool>`).
pub const LOOPBACK_MCP_SERVER_NAME: &str = "lg";

/// Prefix injected by `agy acp` onto tools exposed by [`LOOPBACK_MCP_SERVER_NAME`].
pub const ACP_MCP_TOOL_PREFIX: &str = "mcp_lg_";

const MAX_CODE_MODE_MCP_DESC_BYTES: usize = 64 * 1024;
const MAX_STANDARD_MCP_DESC_BYTES: usize = 8 * 1024;
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

/// Metadata mapping an MCP-exposed tool back to its exact caller-declared tool definition.
#[derive(Clone, Debug)]
pub struct McpBridgedTool {
    pub mcp_name: String,
    pub client_name: String,
    pub is_custom: bool,
    pub unwrap_custom_key: Option<&'static str>,
    pub parameters_json: Option<String>,
}

/// Wire tool descriptor served in MCP `tools/list`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpWireTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
}

/// Partitioned client tool catalog for a session created with the Hybrid Loopback MCP Bridge.
#[derive(Clone, Debug, Default)]
pub struct McpSessionCatalog {
    pub wire_tools: Arc<[McpWireTool]>,
    pub by_mcp_name: HashMap<String, McpBridgedTool>,
    /// `(client_name, mcp_name)` pairs in declaration order for `<api_gateway_bridge_rules>`.
    pub bridged_pairs: Vec<(String, String)>,
    /// Freeform `Custom` tools (e.g. `apply_patch`) and `#65+` overflow tools routed via XML.
    pub xml_only_tools: Vec<CanonicalToolSpec>,
}

impl McpSessionCatalog {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.wire_tools.is_empty()
    }

    #[must_use]
    pub fn lookup(&self, mcp_name: &str) -> Option<&McpBridgedTool> {
        self.by_mcp_name.get(mcp_name)
    }
}

struct RegisteredMcpSession {
    secret_hex: String,
    wire_tools: Arc<[McpWireTool]>,
    call_tx: mpsc::Sender<IncomingMcpToolCall>,
}

type SessionRegistryMap = Mutex<HashMap<u64, Arc<RegisteredMcpSession>>>;

/// Per-session capability handle keeping a session's `/mcp/{token}` endpoint alive across turns.
///
/// Dropping the final `Arc<McpSessionHandle>` synchronously unregisters the token from
/// [`LoopbackMcpServer`] and closes the session's `tools/call` channel.
pub struct McpSessionHandle {
    slot_id: u64,
    server_config: McpServerConfig,
    catalog: McpSessionCatalog,
    call_rx: tokio::sync::Mutex<mpsc::Receiver<IncomingMcpToolCall>>,
    registry: Arc<SessionRegistryMap>,
}

impl fmt::Debug for McpSessionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpSessionHandle")
            .field("slot_id", &self.slot_id)
            .field("bridged_tools", &self.catalog.bridged_pairs.len())
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
    pub const fn call_rx(&self) -> &tokio::sync::Mutex<mpsc::Receiver<IncomingMcpToolCall>> {
        &self.call_rx
    }

    /// Estimates structural + heap bytes retained by this MCP handle while cached in `SessionPool`.
    #[must_use]
    pub fn estimated_retained_bytes(&self) -> usize {
        let pairs_bytes = self
            .catalog
            .bridged_pairs
            .iter()
            .fold(0usize, |acc, (c, m)| acc.saturating_add(c.len() + m.len()));
        std::mem::size_of::<Self>()
            .saturating_add(self.server_config.url.as_deref().map_or(0, str::len))
            .saturating_add(pairs_bytes)
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
    pub fn register_session(&self, catalog: McpSessionCatalog) -> Arc<McpSessionHandle> {
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

/// Partitions caller-declared tools into native loopback MCP tools (up to [`MAX_NATIVE_MCP_TOOLS`],
/// prioritizing CodeMode `exec` + `wait` + `REPL` and core harness tools) and coexisting XML tools.
#[must_use]
pub fn partition_tools_for_mcp<S: TruncationSpillSink>(
    client_tools: &[CanonicalToolSpec],
    spill: &mut S,
) -> McpSessionCatalog {
    struct EligibleCandidate {
        orig_idx: usize,
        tier: u8,
        is_custom: bool,
        unwrap_custom_key: Option<&'static str>,
        input_schema: Value,
        parameters_json: Option<String>,
    }

    let mut eligible: Vec<EligibleCandidate> = Vec::with_capacity(client_tools.len());
    let mut ineligible_indices: HashSet<usize> = HashSet::new();

    for (orig_idx, spec) in client_tools.iter().enumerate() {
        if is_code_mode_custom_exec(spec) {
            eligible.push(EligibleCandidate {
                orig_idx,
                tier: 0,
                is_custom: true,
                unwrap_custom_key: Some("code"),
                input_schema: code_mode_exec_input_schema(),
                parameters_json: None,
            });
            continue;
        }

        match &spec.kind {
            CanonicalToolKind::Custom { .. } => {
                ineligible_indices.insert(orig_idx);
            }
            CanonicalToolKind::Function { parameters_json } => {
                if let Some(schema_val) = normalize_mcp_object_schema(parameters_json) {
                    eligible.push(EligibleCandidate {
                        orig_idx,
                        tier: classify_mcp_priority_tier(spec),
                        is_custom: false,
                        unwrap_custom_key: None,
                        input_schema: schema_val,
                        parameters_json: (!parameters_json.trim().is_empty())
                            .then(|| parameters_json.clone()),
                    });
                } else {
                    ineligible_indices.insert(orig_idx);
                }
            }
        }
    }

    if eligible.len() > MAX_NATIVE_MCP_TOOLS {
        eligible.sort_by_key(|c| (c.tier, c.orig_idx));
        for overflow in eligible.drain(MAX_NATIVE_MCP_TOOLS..) {
            ineligible_indices.insert(overflow.orig_idx);
        }
        eligible.sort_by_key(|c| c.orig_idx);
    }

    let mut used_mcp_names: HashSet<String> = HashSet::with_capacity(eligible.len());
    let mut wire_tools: Vec<McpWireTool> = Vec::with_capacity(eligible.len());
    let mut by_mcp_name: HashMap<String, McpBridgedTool> = HashMap::with_capacity(eligible.len());
    let mut bridged_pairs: Vec<(String, String)> = Vec::with_capacity(eligible.len());

    for cand in eligible {
        let Some(spec) = client_tools.get(cand.orig_idx) else {
            continue;
        };
        let mcp_name = sanitize_mcp_tool_name(&spec.name, &mut used_mcp_names);
        let max_desc = if cand.tier == 0 && is_code_mode_orchestrator_tool(spec) {
            MAX_CODE_MODE_MCP_DESC_BYTES
        } else {
            MAX_STANDARD_MCP_DESC_BYTES
        };
        let description = compact_mcp_tool_description(spec, max_desc, spill);

        wire_tools.push(McpWireTool {
            name: mcp_name.clone(),
            description,
            input_schema: cand.input_schema,
        });
        bridged_pairs.push((spec.name.clone(), mcp_name.clone()));
        by_mcp_name.insert(
            mcp_name.clone(),
            McpBridgedTool {
                mcp_name,
                client_name: spec.name.clone(),
                is_custom: cand.is_custom,
                unwrap_custom_key: cand.unwrap_custom_key,
                parameters_json: cand.parameters_json,
            },
        );
    }

    let mut xml_only_tools = Vec::with_capacity(ineligible_indices.len());
    for (idx, spec) in client_tools.iter().enumerate() {
        if ineligible_indices.contains(&idx) {
            xml_only_tools.push(spec.clone());
        }
    }

    McpSessionCatalog {
        wire_tools: Arc::from(wire_tools.into_boxed_slice()),
        by_mcp_name,
        bridged_pairs,
        xml_only_tools,
    }
}

fn code_mode_exec_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "code": {
                "type": "string",
                "description": "Raw JavaScript source code to execute in the Code Mode runtime (do not wrap in markdown code fences)."
            }
        },
        "required": ["code"],
        "additionalProperties": false
    })
}

fn classify_mcp_priority_tier(spec: &CanonicalToolSpec) -> u8 {
    if matches!(spec.name.as_str(), "exec" | "wait" | "REPL")
        || is_code_mode_orchestrator_tool(spec)
    {
        return 0;
    }
    if matches!(
        spec.name.as_str(),
        "exec_command"
            | "write_stdin"
            | "Bash"
            | "Read"
            | "Edit"
            | "Write"
            | "MultiEdit"
            | "Grep"
            | "Glob"
            | "LS"
            | "NotebookEdit"
            | "Agent"
            | "Task"
            | "TodoWrite"
            | "WebSearch"
            | "WebFetch"
            | "__codex_local_shell"
            | "__codex_tool_search"
            | "ToolSearch"
            | "tool_search"
    ) {
        return 1;
    }
    2
}

fn normalize_mcp_object_schema(parameters_json: &str) -> Option<Value> {
    let trimmed = parameters_json.trim();
    if trimmed.is_empty() {
        return Some(json!({
            "type": "object",
            "properties": {}
        }));
    }
    let Value::Object(mut map) = serde_json::from_str::<Value>(trimmed).ok()? else {
        return None;
    };
    let _ = map.remove("$schema");

    match map.get("type").and_then(Value::as_str) {
        Some("object") => {
            if !map.contains_key("properties") {
                map.insert(
                    "properties".to_owned(),
                    Value::Object(serde_json::Map::new()),
                );
            }
            Some(Value::Object(map))
        }
        Some(_) => None,
        None => {
            if map.contains_key("properties") || map.is_empty() {
                map.insert("type".to_owned(), Value::String("object".to_owned()));
                if !map.contains_key("properties") {
                    map.insert(
                        "properties".to_owned(),
                        Value::Object(serde_json::Map::new()),
                    );
                }
                Some(Value::Object(map))
            } else {
                None
            }
        }
    }
}

/// Sanitizes an arbitrary client tool identifier into a unique MCP tool name (`[a-zA-Z_][a-zA-Z0-9_-]{0,51}`).
#[must_use]
pub fn sanitize_mcp_tool_name(raw_name: &str, used: &mut HashSet<String>) -> String {
    let base_raw = if let Some(after_dunder) = raw_name.strip_prefix("__") {
        format!("lg_internal_{after_dunder}")
    } else {
        raw_name.to_owned()
    };

    let mut clean = String::with_capacity(base_raw.len().min(52));
    for ch in base_raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            clean.push(ch);
        } else if ch == '.' {
            clean.push_str("__");
        } else {
            clean.push('_');
        }
    }
    if clean.is_empty()
        || clean
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphabetic() || c == '_'))
    {
        clean.insert_str(0, "tool_");
    }
    if clean.len() > 52
        && let Some(cut) = clean.get(..52)
    {
        clean = cut.to_owned();
    }

    if used.insert(clean.clone()) {
        return clean;
    }

    let stem = if clean.len() > 46 {
        clean.get(..46).unwrap_or(&clean).to_owned()
    } else {
        clean
    };
    let mut suffix = 2u32;
    loop {
        let candidate = format!("{stem}_{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        suffix = suffix.saturating_add(1);
    }
}

/// Extracts a custom freeform string (such as CodeMode `exec` raw JavaScript) from MCP `arguments`.
#[must_use]
pub fn extract_custom_tool_input(arguments: &Value, preferred_key: Option<&str>) -> String {
    if let Some(key) = preferred_key
        && let Some(s) = arguments.get(key).and_then(Value::as_str)
    {
        return s.to_owned();
    }
    if let Some(s) = arguments.as_str() {
        return s.to_owned();
    }
    if let Value::Object(map) = arguments {
        for fallback in ["code", "input"] {
            if let Some(s) = map.get(fallback).and_then(Value::as_str) {
                return s.to_owned();
            }
        }
        if map.len() == 1
            && let Some(s) = map.values().next().and_then(Value::as_str)
        {
            return s.to_owned();
        }
    }
    serde_json::to_string(arguments).unwrap_or_default()
}

/// Normalizes OpenAI (`call_<nonce>`) and Anthropic (`toolu_<nonce>`) tool call identifiers to
/// their shared `<nonce>` suffix so suspended MCP continuations match seamlessly across protocols.
#[must_use]
pub fn strip_call_prefix(raw_id: &str) -> &str {
    let trimmed = raw_id.trim();
    trimmed
        .strip_prefix("call_")
        .or_else(|| trimmed.strip_prefix("toolu_"))
        .unwrap_or(trimmed)
}

#[cfg(test)]
mod tests {
    use liftgate_types::session_key::NoopSpillSink;

    use super::*;

    #[test]
    fn sanitizes_names_bijectively_and_preserves_standard_identifiers() {
        let mut used = HashSet::new();
        assert_eq!(sanitize_mcp_tool_name("exec", &mut used), "exec");
        assert_eq!(sanitize_mcp_tool_name("wait", &mut used), "wait");
        assert_eq!(sanitize_mcp_tool_name("Bash", &mut used), "Bash");
        assert_eq!(sanitize_mcp_tool_name("read-item", &mut used), "read-item");
        assert_eq!(
            sanitize_mcp_tool_name("multi_agent_v1.spawn_agent", &mut used),
            "multi_agent_v1__spawn_agent"
        );
        assert_eq!(
            sanitize_mcp_tool_name("multi_agent_v1__spawn_agent", &mut used),
            "multi_agent_v1__spawn_agent_2"
        );
        assert_eq!(
            sanitize_mcp_tool_name("__codex_local_shell", &mut used),
            "lg_internal_codex_local_shell"
        );
    }

    #[test]
    fn partitions_code_mode_exec_custom_apply_patch_and_enforces_64_tool_priority_cap() {
        let mut tools = vec![
            CanonicalToolSpec {
                name: "apply_patch".to_owned(),
                description: Some("Freeform patch tool.".to_owned()),
                kind: CanonicalToolKind::Custom { format_json: None },
            },
            CanonicalToolSpec {
                name: "exec".to_owned(),
                description: Some("declare const tools: { wait(): Promise<void>; };".to_owned()),
                kind: CanonicalToolKind::Custom { format_json: None },
            },
        ];
        for i in 0..70 {
            tools.push(CanonicalToolSpec {
                name: format!("extra_tool_{i:02}"),
                description: Some("Extra function.".to_owned()),
                kind: CanonicalToolKind::Function {
                    parameters_json: r#"{"type":"object","properties":{"x":{"type":"number"}}}"#
                        .to_owned(),
                },
            });
        }
        tools.push(CanonicalToolSpec {
            name: "wait".to_owned(),
            description: Some("Wait on CodeMode cell.".to_owned()),
            kind: CanonicalToolKind::Function {
                parameters_json: r#"{"type":"object","properties":{"cell_id":{"type":"string"}}}"#
                    .to_owned(),
            },
        });

        let cat = partition_tools_for_mcp(&tools, &mut NoopSpillSink);
        assert_eq!(cat.wire_tools.len(), MAX_NATIVE_MCP_TOOLS);
        assert!(cat.lookup("exec").is_some_and(|b| b.is_custom));
        assert!(cat.lookup("wait").is_some_and(|b| !b.is_custom));
        assert!(cat.lookup("apply_patch").is_none());
        assert!(cat.xml_only_tools.iter().any(|t| t.name == "apply_patch"));
        // 73 total tools (`apply_patch` + `exec` + 70 `extra_tool_*` + `wait`) - 64 MCP = 9 XML
        assert_eq!(cat.xml_only_tools.len(), 9);

        assert_eq!(strip_call_prefix("call_abc123"), "abc123");
        assert_eq!(strip_call_prefix("toolu_abc123"), "abc123");
        assert_eq!(strip_call_prefix("raw_99"), "raw_99");
    }
}
