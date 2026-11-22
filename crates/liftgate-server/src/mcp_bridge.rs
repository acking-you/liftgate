//! Loopback HTTP MCP tool bridge (`127.0.0.1:0`) enabling native `session/prompt` tool suspension
//! alongside coexisting `<agy_tool_call_{nonce}>` XML tools.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
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
use liftgate_types::{
    acp::McpServerConfig,
    canonical::{CanonicalToolKind, CanonicalToolSpec},
    session_key::{
        TruncationSpillSink, compact_mcp_tool_description, is_code_mode_custom_exec,
        is_code_mode_orchestrator_tool, split_code_mode_ts_declaration,
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

/// Maximum tools exposed natively on the `lg` loopback MCP server per session.
///
/// Bounded to `24` so `agy acp`'s 50-tool Eager ceiling (shared with host MCP plugins) never
/// triggers size-sorted Eager eviction on large coding tools (`exec`, `exec_command`, `Bash`).
pub const MAX_NATIVE_MCP_TOOLS: usize = 24;

/// Eligible tool count up to which all tools are exposed 1:1 without namespace virtualization
/// (when non-orchestrator wire bytes also stay within [`DIRECT_PASSTHROUGH_MAX_WIRE_BYTES`]).
pub const DIRECT_PASSTHROUGH_MAX_TOOLS: usize = 20;

/// Maximum cumulative non-orchestrator wire description + schema bytes for 1:1 passthrough mode.
pub const DIRECT_PASSTHROUGH_MAX_WIRE_BYTES: usize = 22 * 1024;

/// Maximum core Tier-0 / Tier-1 coding tools kept 1:1 direct when virtualization activates.
pub const MAX_DIRECT_CORE_MCP_TOOLS: usize = 12;

/// Maximum actions in a namespace hub for embedding `"enum": [...]` directly on `action`.
pub const SMALL_HUB_ENUM_MAX_ACTIONS: usize = 12;

/// Gateway-local 0-RTT search tool mounted on `lg` (`mcp_lg_search_client_tools`).
pub const SEARCH_CLIENT_TOOLS_MCP_NAME: &str = "search_client_tools";

/// Universal dispatcher MCP tool mounted on `lg` (`mcp_lg_call_client_tool`).
pub const CALL_CLIENT_TOOL_MCP_NAME: &str = "call_client_tool";

/// Prefix for namespace action hub MCP tools (`mcp_lg_ns__<namespace>`).
pub const NAMESPACE_HUB_MCP_PREFIX: &str = "ns__";

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
const DEFAULT_SEARCH_RESULTS_LIMIT: usize = 8;
const MAX_SEARCH_RESULTS_LIMIT: usize = 20;
const MAX_SEARCH_REPLY_BYTES: usize = 32 * 1024;
const MAX_SINGLE_SCHEMA_IN_SEARCH_BYTES: usize = 12 * 1024;

const _: () = assert!(
    DIRECT_PASSTHROUGH_MAX_TOOLS <= MAX_NATIVE_MCP_TOOLS,
    "direct passthrough threshold must fit within native MCP ceiling"
);
const _: () = assert!(
    MAX_DIRECT_CORE_MCP_TOOLS + 2 <= MAX_NATIVE_MCP_TOOLS,
    "core direct tools plus search and universal dispatcher must fit within native MCP ceiling"
);
const _: () = assert!(
    DEFAULT_SEARCH_RESULTS_LIMIT <= MAX_SEARCH_RESULTS_LIMIT,
    "default search limit must not exceed max search limit"
);

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

/// Classification of an MCP call resolved synchronously inside Liftgate without client suspension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalReplyKind {
    /// Zero-round-trip `search_client_tools` catalog lookup.
    Search,
    /// Schema preflight or unknown action error with self-healing guidance.
    PreflightError,
    /// Tool is disabled for the current turn.
    DisabledTool,
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
}

#[derive(Clone, Debug)]
struct IndexedClientTool {
    client_name: String,
    namespace: Option<String>,
    short_action: Option<String>,
    invoke_via_mcp_name: String,
    is_custom: bool,
    unwrap_custom_key: Option<&'static str>,
    prose_description: String,
    ts_declaration: Option<String>,
    parameters_json: Option<String>,
    required_fields: Vec<String>,
    declares_tool_summary: bool,
    has_root_combinator: bool,
    searchable_lower: String,
}

impl IndexedClientTool {
    fn estimated_heap_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.client_name.len())
            .saturating_add(self.namespace.as_deref().map_or(0, str::len))
            .saturating_add(self.short_action.as_deref().map_or(0, str::len))
            .saturating_add(self.invoke_via_mcp_name.len())
            .saturating_add(self.prose_description.len())
            .saturating_add(self.ts_declaration.as_deref().map_or(0, str::len))
            .saturating_add(self.parameters_json.as_deref().map_or(0, str::len))
            .saturating_add(self.searchable_lower.len())
            .saturating_add(
                self.required_fields
                    .iter()
                    .fold(0usize, |acc, s| acc.saturating_add(s.len())),
            )
    }
}

#[derive(Clone, Debug)]
struct HubNamespaceTable {
    namespace: String,
    hub_mcp_name: String,
    actions: Vec<IndexedClientTool>,
}

/// Live routing and search snapshot for a session's active client tools.
#[derive(Clone, Debug, Default)]
struct McpRoutingTable {
    source_tools: Vec<CanonicalToolSpec>,
    direct_by_mcp: HashMap<String, McpBridgedTool>,
    hub_names_by_namespace: HashMap<String, String>,
    hubs_by_mcp: HashMap<String, HubNamespaceTable>,
    all_indexed_tools: Vec<IndexedClientTool>,
    has_virtualized_dispatch: bool,
}

/// Partitioned client tool catalog for a session created with the Hybrid Loopback MCP Bridge.
#[derive(Clone, Debug, Default)]
pub struct McpSessionCatalog {
    pub wire_tools: Arc<[McpWireTool]>,
    pub by_mcp_name: HashMap<String, McpBridgedTool>,
    /// `(client_name, mcp_name)` pairs in declaration order for `<api_gateway_bridge_rules>`.
    pub bridged_pairs: Vec<(String, String)>,
    /// `(hub_mcp_name, namespace_label, action_count)` summaries when namespace virtualization is active.
    pub virtualized_hubs: Vec<(String, String, usize)>,
    /// Whether `search_client_tools` and `call_client_tool` are mounted on `lg`.
    pub has_virtualized_dispatch: bool,
    /// Freeform `Custom` tools (e.g. `apply_patch`) and non-object schema tools routed via XML.
    pub xml_only_tools: Vec<CanonicalToolSpec>,
    routing_table: Arc<McpRoutingTable>,
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
    routing: RwLock<Arc<McpRoutingTable>>,
    call_rx: tokio::sync::Mutex<mpsc::Receiver<IncomingMcpToolCall>>,
    registry: Arc<SessionRegistryMap>,
}

impl fmt::Debug for McpSessionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpSessionHandle")
            .field("slot_id", &self.slot_id)
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

    #[must_use]
    pub const fn call_rx(&self) -> &tokio::sync::Mutex<mpsc::Receiver<IncomingMcpToolCall>> {
        &self.call_rx
    }

    /// Synchronizes the session's live in-memory routing and search index with `client_tools`
    /// when a warm or resumed turn updates the active tool catalog.
    pub fn sync_client_tools(&self, client_tools: &[CanonicalToolSpec]) {
        let (direct_by_mcp, hub_names_by_namespace, has_virtualized_dispatch) = {
            let guard = match self.routing.read() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if guard.source_tools == client_tools {
                return;
            }
            (
                guard.direct_by_mcp.clone(),
                guard.hub_names_by_namespace.clone(),
                guard.has_virtualized_dispatch,
            )
        };

        let rebuilt = Arc::new(McpRoutingTable::build(
            client_tools,
            direct_by_mcp,
            hub_names_by_namespace,
            has_virtualized_dispatch,
        ));
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
        table.resolve_call(incoming, active_tools)
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
            guard.all_indexed_tools.iter().fold(0usize, |acc, item| {
                acc.saturating_add(item.estimated_heap_bytes())
            })
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
        catalog.xml_only_tools = Vec::new();
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

/// Partitions caller-declared tools into native loopback MCP tools (up to [`MAX_NATIVE_MCP_TOOLS`],
/// prioritizing CodeMode `exec` + `wait` + `REPL` and core harness tools, collapsing large
/// connector families into `ns__<namespace>` hubs + 0-RTT `search_client_tools`) and XML tools.
#[must_use]
pub fn partition_tools_for_mcp<S: TruncationSpillSink>(
    client_tools: &[CanonicalToolSpec],
    spill: &mut S,
) -> McpSessionCatalog {
    struct EligibleCandidate {
        orig_idx: usize,
        tier: u8,
        is_orchestrator: bool,
        is_custom: bool,
        unwrap_custom_key: Option<&'static str>,
        description: Option<String>,
        input_schema: Value,
        parameters_json: Option<String>,
        est_wire_bytes: usize,
    }

    let mut eligible: Vec<EligibleCandidate> = Vec::with_capacity(client_tools.len());
    let mut ineligible_indices: HashSet<usize> = HashSet::new();

    for (orig_idx, spec) in client_tools.iter().enumerate() {
        if is_code_mode_custom_exec(spec) {
            let description =
                compact_mcp_tool_description(spec, MAX_CODE_MODE_MCP_DESC_BYTES, spill);
            let input_schema = code_mode_exec_input_schema();
            let est_wire_bytes =
                estimate_wire_tool_bytes(&spec.name, description.as_deref(), &input_schema);
            eligible.push(EligibleCandidate {
                orig_idx,
                tier: 0,
                is_orchestrator: true,
                is_custom: true,
                unwrap_custom_key: Some("code"),
                description,
                input_schema,
                parameters_json: None,
                est_wire_bytes,
            });
            continue;
        }

        match &spec.kind {
            CanonicalToolKind::Custom { .. } => {
                ineligible_indices.insert(orig_idx);
            }
            CanonicalToolKind::Function { parameters_json } => {
                if let Some(schema_val) = normalize_mcp_object_schema(parameters_json) {
                    let is_orchestrator = is_code_mode_orchestrator_tool(spec);
                    let tier = classify_mcp_priority_tier(spec);
                    let max_desc = if is_orchestrator {
                        MAX_CODE_MODE_MCP_DESC_BYTES
                    } else {
                        MAX_STANDARD_MCP_DESC_BYTES
                    };
                    let description = compact_mcp_tool_description(spec, max_desc, spill);
                    let est_wire_bytes =
                        estimate_wire_tool_bytes(&spec.name, description.as_deref(), &schema_val);
                    eligible.push(EligibleCandidate {
                        orig_idx,
                        tier,
                        is_orchestrator,
                        is_custom: false,
                        unwrap_custom_key: None,
                        description,
                        input_schema: schema_val,
                        parameters_json: (!parameters_json.trim().is_empty())
                            .then(|| parameters_json.clone()),
                        est_wire_bytes,
                    });
                } else {
                    ineligible_indices.insert(orig_idx);
                }
            }
        }
    }

    let non_orchestrator_wire_bytes = eligible
        .iter()
        .filter(|c| !c.is_orchestrator)
        .fold(0usize, |acc, c| acc.saturating_add(c.est_wire_bytes));
    let has_virtualizable_candidates = eligible.iter().any(|c| c.tier > 1)
        || eligible.iter().filter(|c| c.tier <= 1).count() > MAX_DIRECT_CORE_MCP_TOOLS;
    let should_virtualize = (eligible.len() > DIRECT_PASSTHROUGH_MAX_TOOLS
        || non_orchestrator_wire_bytes > DIRECT_PASSTHROUGH_MAX_WIRE_BYTES)
        && has_virtualizable_candidates;

    let mut used_mcp_names: HashSet<String> = HashSet::with_capacity(MAX_NATIVE_MCP_TOOLS);
    let mut wire_tools: Vec<McpWireTool> = Vec::with_capacity(MAX_NATIVE_MCP_TOOLS);
    let mut by_mcp_name: HashMap<String, McpBridgedTool> = HashMap::new();
    let mut bridged_pairs: Vec<(String, String)> = Vec::new();
    let mut virtualized_hubs: Vec<(String, String, usize)> = Vec::new();
    let mut hub_names_by_namespace: HashMap<String, String> = HashMap::new();

    if !should_virtualize {
        for cand in eligible {
            let Some(spec) = client_tools.get(cand.orig_idx) else {
                continue;
            };
            let mcp_name = sanitize_mcp_tool_name(&spec.name, &mut used_mcp_names);
            wire_tools.push(McpWireTool {
                name: mcp_name.clone(),
                description: cand.description,
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
    } else {
        used_mcp_names.insert(SEARCH_CLIENT_TOOLS_MCP_NAME.to_owned());
        used_mcp_names.insert(CALL_CLIENT_TOOL_MCP_NAME.to_owned());

        let mut core_candidates: Vec<EligibleCandidate> = Vec::new();
        let mut virtual_pool: Vec<EligibleCandidate> = Vec::new();
        for cand in eligible {
            if cand.tier <= 1 {
                core_candidates.push(cand);
            } else {
                virtual_pool.push(cand);
            }
        }
        if core_candidates.len() > MAX_DIRECT_CORE_MCP_TOOLS {
            core_candidates.sort_by_key(|c| (c.tier, c.orig_idx));
            virtual_pool.extend(core_candidates.drain(MAX_DIRECT_CORE_MCP_TOOLS..));
            core_candidates.sort_by_key(|c| c.orig_idx);
        }
        virtual_pool.sort_by_key(|c| c.orig_idx);

        for cand in core_candidates {
            let Some(spec) = client_tools.get(cand.orig_idx) else {
                continue;
            };
            let mcp_name = sanitize_mcp_tool_name(&spec.name, &mut used_mcp_names);
            wire_tools.push(McpWireTool {
                name: mcp_name.clone(),
                description: cand.description,
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

        let virtual_names: Vec<&str> = virtual_pool
            .iter()
            .filter_map(|c| client_tools.get(c.orig_idx).map(|s| s.name.as_str()))
            .collect();
        let stem_counts = count_fallback_prefix_stems(&virtual_names);

        struct PendingHubGroup {
            namespace: String,
            first_idx: usize,
            actions: Vec<String>,
        }
        let mut groups_by_ns: HashMap<String, usize> = HashMap::new();
        let mut groups: Vec<PendingHubGroup> = Vec::new();
        for cand in &virtual_pool {
            let Some(spec) = client_tools.get(cand.orig_idx) else {
                continue;
            };
            let Some((ns, short_action)) =
                extract_tool_namespace_and_action(&spec.name, &stem_counts)
            else {
                continue;
            };
            if let Some(&g_idx) = groups_by_ns.get(&ns) {
                if let Some(grp) = groups.get_mut(g_idx) {
                    grp.actions.push(short_action);
                }
            } else {
                let g_idx = groups.len();
                groups_by_ns.insert(ns.clone(), g_idx);
                groups.push(PendingHubGroup {
                    namespace: ns,
                    first_idx: cand.orig_idx,
                    actions: vec![short_action],
                });
            }
        }

        groups.retain(|g| g.actions.len() >= 2);
        let max_hub_slots = MAX_NATIVE_MCP_TOOLS
            .saturating_sub(wire_tools.len())
            .saturating_sub(2);
        if groups.len() > max_hub_slots {
            groups.sort_by(|a, b| {
                b.actions
                    .len()
                    .cmp(&a.actions.len())
                    .then_with(|| a.first_idx.cmp(&b.first_idx))
            });
            groups.truncate(max_hub_slots);
            groups.sort_by_key(|g| g.first_idx);
        }

        for grp in groups {
            let raw_hub_name = format!("{NAMESPACE_HUB_MCP_PREFIX}{}", grp.namespace);
            let hub_mcp_name = sanitize_mcp_tool_name(&raw_hub_name, &mut used_mcp_names);
            let wire_hub =
                build_namespace_hub_wire_tool(&hub_mcp_name, &grp.namespace, &grp.actions);
            wire_tools.push(wire_hub);
            virtualized_hubs.push((
                hub_mcp_name.clone(),
                grp.namespace.clone(),
                grp.actions.len(),
            ));
            hub_names_by_namespace.insert(grp.namespace, hub_mcp_name);
        }

        wire_tools.push(build_call_client_tool_wire_tool());
        wire_tools.push(build_search_client_tools_wire_tool());
    }

    let mut xml_only_tools = Vec::with_capacity(ineligible_indices.len());
    for (idx, spec) in client_tools.iter().enumerate() {
        if ineligible_indices.contains(&idx) {
            xml_only_tools.push(spec.clone());
        }
    }

    let routing_table = Arc::new(McpRoutingTable::build(
        client_tools,
        by_mcp_name.clone(),
        hub_names_by_namespace,
        should_virtualize,
    ));

    McpSessionCatalog {
        wire_tools: Arc::from(wire_tools.into_boxed_slice()),
        by_mcp_name,
        bridged_pairs,
        virtualized_hubs,
        has_virtualized_dispatch: should_virtualize,
        xml_only_tools,
        routing_table,
    }
}

fn estimate_wire_tool_bytes(name: &str, desc: Option<&str>, schema: &Value) -> usize {
    struct ByteCounter(usize);
    impl std::io::Write for ByteCounter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(buf.len());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = ByteCounter(0);
    let _ = serde_json::to_writer(&mut counter, schema);
    name.len()
        .saturating_add(desc.map_or(0, str::len))
        .saturating_add(counter.0)
}

fn count_fallback_prefix_stems(names: &[&str]) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for &name in names {
        if extract_explicit_namespace_and_action(name).is_some() {
            continue;
        }
        if let Some((stem, rest)) = name.split_once('_')
            && stem.len() >= 3
            && !rest.is_empty()
        {
            let key = stem.to_ascii_lowercase();
            *counts.entry(key).or_insert(0) += 1;
        }
    }
    counts
}

fn extract_explicit_namespace_and_action(name: &str) -> Option<(String, String)> {
    if let Some(after_mcp) = name.strip_prefix("mcp__") {
        let trimmed_vendor = after_mcp
            .strip_prefix("codex_apps__")
            .or_else(|| after_mcp.strip_prefix("claude_ai_"))
            .unwrap_or(after_mcp);
        let dot_pos = trimmed_vendor.find('.');
        let dunder_pos = trimmed_vendor.find("__");
        let split_choice = match (dot_pos, dunder_pos) {
            (Some(d), Some(u)) => {
                if d < u {
                    Some((d, 1usize))
                } else {
                    Some((u, 2usize))
                }
            }
            (Some(d), None) => Some((d, 1usize)),
            (None, Some(u)) => Some((u, 2usize)),
            (None, None) => None,
        };
        if let Some((idx, sep_len)) = split_choice
            && let Some((ns_raw, rest_with_sep)) = trimmed_vendor.split_at_checked(idx)
            && let Some((_, action_raw)) = rest_with_sep.split_at_checked(sep_len)
            && !ns_raw.is_empty()
            && !action_raw.is_empty()
        {
            return Some((ns_raw.to_ascii_lowercase(), action_raw.to_owned()));
        }
    }

    if let Some((ns_raw, action_raw)) = name.split_once('.')
        && !ns_raw.is_empty()
        && !action_raw.is_empty()
    {
        return Some((ns_raw.to_ascii_lowercase(), action_raw.to_owned()));
    }

    match name {
        "list_mcp_resources" | "list_mcp_resource_templates" | "read_mcp_resource" => {
            Some(("mcp_resources".to_owned(), name.to_owned()))
        }
        "request_user_input" | "request_user_input_async" | "send_message_to_user_async" => {
            Some(("user_input".to_owned(), name.to_owned()))
        }
        "get_goal" | "create_goal" | "update_goal" => Some(("goals".to_owned(), name.to_owned())),
        _ => None,
    }
}

fn extract_tool_namespace_and_action(
    name: &str,
    fallback_stem_counts: &HashMap<String, usize>,
) -> Option<(String, String)> {
    if let Some(pair) = extract_explicit_namespace_and_action(name) {
        return Some(pair);
    }
    if let Some((stem, rest)) = name.split_once('_')
        && stem.len() >= 3
        && !rest.is_empty()
    {
        let key = stem.to_ascii_lowercase();
        if fallback_stem_counts.get(&key).copied().unwrap_or(0) >= 3 {
            return Some((key, name.to_owned()));
        }
    }
    None
}

fn build_namespace_hub_wire_tool(
    hub_mcp_name: &str,
    namespace: &str,
    actions: &[String],
) -> McpWireTool {
    let count = actions.len();
    let (description, action_prop) = if count <= SMALL_HUB_ENUM_MAX_ACTIONS {
        let joined = actions.join(", ");
        (
            format!(
                "`{namespace}` tool hub ({count} actions: {joined}). Pass `action` and JSON `args`; call `{SEARCH_CLIENT_TOOLS_MCP_NAME}` with `namespace: \"{namespace}\"` to inspect exact parameter schemas."
            ),
            json!({
                "type": "string",
                "enum": actions,
                "description": format!("Action within `{namespace}` to invoke.")
            }),
        )
    } else {
        let sample: Vec<&str> = actions.iter().take(6).map(String::as_str).collect();
        let sample_joined = sample.join(", ");
        (
            format!(
                "`{namespace}` tool hub ({count} actions, e.g. {sample_joined}, ...). Call `{SEARCH_CLIENT_TOOLS_MCP_NAME}` with `namespace: \"{namespace}\"` and a `query` keyword FIRST to inspect exact `action` names and required `args` schemas."
            ),
            json!({
                "type": "string",
                "description": format!("Exact action name within `{namespace}` (discover via `{SEARCH_CLIENT_TOOLS_MCP_NAME}`).")
            }),
        )
    };

    McpWireTool {
        name: hub_mcp_name.to_owned(),
        description: Some(description),
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": action_prop,
                "args": {
                    "type": "object",
                    "description": "JSON arguments object matching the selected action's parameter schema."
                }
            },
            "required": ["action"]
        }),
    }
}

fn build_call_client_tool_wire_tool() -> McpWireTool {
    McpWireTool {
        name: CALL_CLIENT_TOOL_MCP_NAME.to_owned(),
        description: Some(format!(
            "Invoke any active client tool by exact `name` and JSON `arguments` object (for standalone, long-tail, or mid-session dynamically added tools). Use `{SEARCH_CLIENT_TOOLS_MCP_NAME}` first when parameter schemas are needed."
        )),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Exact client tool name."
                },
                "arguments": {
                    "type": "object",
                    "description": "JSON arguments object matching the target tool's schema."
                }
            },
            "required": ["name"]
        }),
    }
}

fn build_search_client_tools_wire_tool() -> McpWireTool {
    McpWireTool {
        name: SEARCH_CLIENT_TOOLS_MCP_NAME.to_owned(),
        description: Some(
            "Zero-round-trip local lookup across all active client tools in this session. Query by `query` keywords and/or `namespace`, or pass exact `names` to retrieve full JSON `inputSchema` and Code Mode TypeScript signatures."
                .to_owned(),
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Space-separated keywords matching tool name, action, parameter names, or description."
                },
                "namespace": {
                    "type": "string",
                    "description": "Optional namespace filter (e.g. 'github', 'codex_tui', 'multi_agent_v1')."
                },
                "names": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional exact tool or action names to inspect."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum tools to return (1..20, default 8)."
                }
            }
        }),
    }
}

impl McpRoutingTable {
    fn build(
        client_tools: &[CanonicalToolSpec],
        direct_by_mcp: HashMap<String, McpBridgedTool>,
        hub_names_by_namespace: HashMap<String, String>,
        has_virtualized_dispatch: bool,
    ) -> Self {
        let mut direct_by_client: HashMap<&str, &McpBridgedTool> =
            HashMap::with_capacity(direct_by_mcp.len());
        for bridged in direct_by_mcp.values() {
            direct_by_client.insert(bridged.client_name.as_str(), bridged);
        }

        let non_direct_names: Vec<&str> = client_tools
            .iter()
            .filter(|t| !direct_by_client.contains_key(t.name.as_str()))
            .map(|t| t.name.as_str())
            .collect();
        let stem_counts = count_fallback_prefix_stems(&non_direct_names);

        let mut hubs_by_mcp: HashMap<String, HubNamespaceTable> =
            HashMap::with_capacity(hub_names_by_namespace.len());
        for (ns, hub_mcp) in &hub_names_by_namespace {
            hubs_by_mcp.insert(
                hub_mcp.clone(),
                HubNamespaceTable {
                    namespace: ns.clone(),
                    hub_mcp_name: hub_mcp.clone(),
                    actions: Vec::new(),
                },
            );
        }

        let mut all_indexed_tools = Vec::with_capacity(client_tools.len());
        for spec in client_tools {
            let is_exec = is_code_mode_custom_exec(spec);
            if matches!(spec.kind, CanonicalToolKind::Custom { .. }) && !is_exec {
                continue;
            }
            let params_json = match &spec.kind {
                CanonicalToolKind::Custom { .. } => None,
                CanonicalToolKind::Function { parameters_json } => {
                    if normalize_mcp_object_schema(parameters_json).is_none() {
                        continue;
                    }
                    (!parameters_json.trim().is_empty()).then(|| parameters_json.clone())
                }
            };

            let raw_desc = spec.description.as_deref().unwrap_or("");
            let (prose, ts_decl) = if is_code_mode_orchestrator_tool(spec) {
                (raw_desc.trim(), None)
            } else {
                split_code_mode_ts_declaration(raw_desc)
            };

            let (required_fields, declares_tool_summary, has_root_combinator, prop_names_joined) =
                extract_schema_metadata(params_json.as_deref());

            let (namespace, short_action, invoke_via_mcp_name, is_custom, unwrap_custom_key) =
                if let Some(direct) = direct_by_client.get(spec.name.as_str()) {
                    (
                        None,
                        None,
                        direct.mcp_name.clone(),
                        direct.is_custom,
                        direct.unwrap_custom_key,
                    )
                } else if let Some((ns, act)) =
                    extract_tool_namespace_and_action(&spec.name, &stem_counts)
                {
                    let via = hub_names_by_namespace
                        .get(&ns)
                        .cloned()
                        .unwrap_or_else(|| CALL_CLIENT_TOOL_MCP_NAME.to_owned());
                    (Some(ns), Some(act), via, is_exec, is_exec.then_some("code"))
                } else {
                    (
                        None,
                        None,
                        CALL_CLIENT_TOOL_MCP_NAME.to_owned(),
                        is_exec,
                        is_exec.then_some("code"),
                    )
                };

            let searchable_lower = format!(
                "{} {} {} {} {}",
                spec.name.to_ascii_lowercase(),
                namespace.as_deref().unwrap_or(""),
                short_action.as_deref().unwrap_or("").to_ascii_lowercase(),
                prop_names_joined.to_ascii_lowercase(),
                prose.to_ascii_lowercase()
            );

            let indexed = IndexedClientTool {
                client_name: spec.name.clone(),
                namespace,
                short_action,
                invoke_via_mcp_name: invoke_via_mcp_name.clone(),
                is_custom,
                unwrap_custom_key,
                prose_description: prose.to_owned(),
                ts_declaration: ts_decl.map(str::to_owned),
                parameters_json: params_json,
                required_fields,
                declares_tool_summary,
                has_root_combinator,
                searchable_lower,
            };

            if let Some(hub_table) = hubs_by_mcp.get_mut(&invoke_via_mcp_name) {
                hub_table.actions.push(indexed.clone());
            }
            all_indexed_tools.push(indexed);
        }

        Self {
            source_tools: client_tools.to_vec(),
            direct_by_mcp,
            hub_names_by_namespace,
            hubs_by_mcp,
            all_indexed_tools,
            has_virtualized_dispatch,
        }
    }

    fn resolve_call(
        &self,
        incoming: IncomingMcpToolCall,
        active_tools: &[CanonicalToolSpec],
    ) -> ResolvedMcpIncoming {
        if self.has_virtualized_dispatch && incoming.mcp_name == SEARCH_CLIENT_TOOLS_MCP_NAME {
            let text = self.execute_search(&incoming.arguments, active_tools);
            return ResolvedMcpIncoming::ImmediateLocalReply {
                reply_tx: incoming.reply_tx,
                reply: McpToolCallReply {
                    content: vec![McpContentItem::Text { text }],
                    is_error: false,
                },
                kind: LocalReplyKind::Search,
            };
        }

        if let Some(bridged) = self.direct_by_mcp.get(&incoming.mcp_name) {
            if !is_tool_active(&bridged.client_name, active_tools) {
                return disabled_tool_reply(incoming.reply_tx, &bridged.client_name);
            }
            let arguments = format_direct_tool_arguments(&incoming.arguments, bridged);
            return ResolvedMcpIncoming::ForwardToClient {
                client_name: bridged.client_name.clone(),
                arguments,
                is_custom: bridged.is_custom,
                reply_tx: incoming.reply_tx,
            };
        }

        if let Some(hub) = self.hubs_by_mcp.get(&incoming.mcp_name) {
            return self.resolve_hub_call(hub, incoming, active_tools);
        }

        if self.has_virtualized_dispatch && incoming.mcp_name == CALL_CLIENT_TOOL_MCP_NAME {
            return self.resolve_universal_dispatch_call(incoming, active_tools);
        }

        if is_tool_active(&incoming.mcp_name, active_tools)
            && let Some(indexed) = self
                .all_indexed_tools
                .iter()
                .find(|t| t.client_name == incoming.mcp_name)
        {
            return finalize_indexed_tool_call(
                indexed,
                &incoming.arguments,
                false,
                &[],
                incoming.reply_tx,
            );
        }

        disabled_tool_reply(incoming.reply_tx, &incoming.mcp_name)
    }

    fn resolve_hub_call(
        &self,
        hub: &HubNamespaceTable,
        incoming: IncomingMcpToolCall,
        active_tools: &[CanonicalToolSpec],
    ) -> ResolvedMcpIncoming {
        let requested_action = incoming
            .arguments
            .get("action")
            .or_else(|| incoming.arguments.get("name"))
            .or_else(|| incoming.arguments.get("tool"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");

        if requested_action.is_empty() {
            let sample: Vec<&str> = hub
                .actions
                .iter()
                .filter_map(|a| a.short_action.as_deref())
                .take(10)
                .collect();
            return preflight_error_reply(
                incoming.reply_tx,
                format!(
                    "Missing `action` string for `mcp_lg_{}`. Available `{}` actions include: [{}]. Call `mcp_lg_{SEARCH_CLIENT_TOOLS_MCP_NAME}` with `namespace: \"{}\"` to inspect schemas.",
                    hub.hub_mcp_name,
                    hub.namespace,
                    sample.join(", "),
                    hub.namespace
                ),
            );
        }

        let matched = find_action_in_hub(&hub.actions, requested_action).or_else(|| {
            self.all_indexed_tools
                .iter()
                .find(|t| t.client_name == requested_action)
        });

        let Some(indexed) = matched else {
            let suggestions = suggest_closest_actions(&hub.actions, requested_action, 8);
            return preflight_error_reply(
                incoming.reply_tx,
                format!(
                    "Unknown action `{requested_action}` on `mcp_lg_{}`. Closest `{}` actions: [{}]. Call `mcp_lg_{SEARCH_CLIENT_TOOLS_MCP_NAME}` with `namespace: \"{}\"` and `query` to inspect exact action names and schemas.",
                    hub.hub_mcp_name,
                    hub.namespace,
                    suggestions.join(", "),
                    hub.namespace
                ),
            );
        };

        if !is_tool_active(&indexed.client_name, active_tools) {
            return disabled_tool_reply(incoming.reply_tx, &indexed.client_name);
        }

        let inner_args = match extract_virtualized_inner_args(
            &incoming.arguments,
            &["action", "toolSummary"],
            &indexed.required_fields,
        ) {
            Ok(v) => v,
            Err(msg) => return preflight_error_reply(incoming.reply_tx, msg),
        };

        finalize_indexed_tool_call(
            indexed,
            &inner_args,
            true,
            &["action", "toolSummary"],
            incoming.reply_tx,
        )
    }

    fn resolve_universal_dispatch_call(
        &self,
        incoming: IncomingMcpToolCall,
        active_tools: &[CanonicalToolSpec],
    ) -> ResolvedMcpIncoming {
        let requested_name = incoming
            .arguments
            .get("name")
            .or_else(|| incoming.arguments.get("tool"))
            .or_else(|| incoming.arguments.get("action"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");

        if requested_name.is_empty() {
            return preflight_error_reply(
                incoming.reply_tx,
                format!(
                    "Missing `name` string for `mcp_lg_{CALL_CLIENT_TOOL_MCP_NAME}`. Call `mcp_lg_{SEARCH_CLIENT_TOOLS_MCP_NAME}` first to discover active tool names."
                ),
            );
        }

        let matched = self
            .all_indexed_tools
            .iter()
            .find(|t| t.client_name == requested_name)
            .or_else(|| {
                self.all_indexed_tools
                    .iter()
                    .find(|t| t.client_name.eq_ignore_ascii_case(requested_name))
            })
            .or_else(|| {
                let mut iter = self.all_indexed_tools.iter().filter(|t| {
                    t.short_action
                        .as_deref()
                        .is_some_and(|a| a == requested_name)
                });
                let first = iter.next()?;
                iter.next().is_none().then_some(first)
            });

        let Some(indexed) = matched else {
            let suggestions = suggest_closest_tools(&self.all_indexed_tools, requested_name, 8);
            return preflight_error_reply(
                incoming.reply_tx,
                format!(
                    "Unknown client tool `{requested_name}`. Closest active tools: [{}]. Call `mcp_lg_{SEARCH_CLIENT_TOOLS_MCP_NAME}` to search tool names and schemas.",
                    suggestions.join(", ")
                ),
            );
        };

        if !is_tool_active(&indexed.client_name, active_tools) {
            return disabled_tool_reply(incoming.reply_tx, &indexed.client_name);
        }

        let inner_args = match extract_virtualized_inner_args(
            &incoming.arguments,
            &["name", "tool", "action", "toolSummary"],
            &indexed.required_fields,
        ) {
            Ok(v) => v,
            Err(msg) => return preflight_error_reply(incoming.reply_tx, msg),
        };

        finalize_indexed_tool_call(
            indexed,
            &inner_args,
            true,
            &["name", "tool", "action", "toolSummary"],
            incoming.reply_tx,
        )
    }

    fn execute_search(&self, args: &Value, active_tools: &[CanonicalToolSpec]) -> String {
        let query_raw = args
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let ns_filter = args
            .get("namespace")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.trim_start_matches("mcp_lg_ns__")
                    .trim_start_matches("ns__")
                    .to_ascii_lowercase()
            });
        let exact_names: Vec<String> = args
            .get("names")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.trim().to_ascii_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(DEFAULT_SEARCH_RESULTS_LIMIT)
            .clamp(1, MAX_SEARCH_RESULTS_LIMIT);

        let tokens: Vec<String> = query_raw
            .split(|c: char| c.is_whitespace() || c == ',' || c == '/')
            .map(|t| t.trim().to_ascii_lowercase())
            .filter(|t| !t.is_empty())
            .collect();

        let mut scored: Vec<(u32, usize, &IndexedClientTool)> = Vec::new();
        for (idx, item) in self.all_indexed_tools.iter().enumerate() {
            if !is_tool_active(&item.client_name, active_tools) {
                continue;
            }
            if let Some(ref ns_req) = ns_filter {
                let item_ns = item.namespace.as_deref().unwrap_or("");
                if item_ns != ns_req && !item.client_name.to_ascii_lowercase().contains(ns_req) {
                    continue;
                }
            }
            let score = score_indexed_tool(item, &exact_names, &tokens);
            if (!exact_names.is_empty() || !tokens.is_empty()) && score == 0 {
                continue;
            }
            scored.push((score, idx, item));
        }

        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let total_matches = scored.len();
        let has_code_mode_exec = active_tools.iter().any(is_code_mode_custom_exec);

        if scored.is_empty() {
            let mut ns_counts: HashMap<&str, usize> = HashMap::new();
            for item in &self.all_indexed_tools {
                if is_tool_active(&item.client_name, active_tools) {
                    let label = item.namespace.as_deref().unwrap_or("standalone");
                    *ns_counts.entry(label).or_insert(0) += 1;
                }
            }
            let mut ns_list: Vec<(&str, usize)> = ns_counts.into_iter().collect();
            ns_list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            let formatted_ns: Vec<String> = ns_list
                .into_iter()
                .map(|(ns, cnt)| format!("{ns} ({cnt})"))
                .collect();
            let client_tool_search_hint = if active_tools.iter().any(|t| {
                matches!(
                    t.name.as_str(),
                    "tool_search" | "ToolSearch" | "__codex_tool_search"
                )
            }) {
                "\nHint: if the connector has not been loaded by the client yet, invoke the client's own `tool_search` / `ToolSearch` tool."
            } else {
                ""
            };
            return format!(
                "No active client tools matched query={query_raw:?}, namespace={ns_filter:?}, names={exact_names:?}.\nAvailable namespaces: [{}].{client_tool_search_hint}",
                formatted_ns.join(", ")
            );
        }

        let mut out = String::with_capacity(2048);
        out.push_str(&format!(
            "Matched {total_matches} active client tool(s) (showing top {}):\n",
            total_matches.min(limit)
        ));

        for (_, _, item) in scored.into_iter().take(limit) {
            let mut entry_text = String::with_capacity(512);
            entry_text.push_str(&format!("\n- tool: `{}`\n", item.client_name));
            if item.invoke_via_mcp_name == CALL_CLIENT_TOOL_MCP_NAME {
                entry_text.push_str(&format!(
                    "  invoke_via: `mcp_lg_{CALL_CLIENT_TOOL_MCP_NAME}` with `{{\"name\": \"{}\", \"arguments\": {{ ... }}}}`\n",
                    item.client_name
                ));
            } else if item
                .invoke_via_mcp_name
                .starts_with(NAMESPACE_HUB_MCP_PREFIX)
            {
                let act = item
                    .short_action
                    .as_deref()
                    .unwrap_or(item.client_name.as_str());
                entry_text.push_str(&format!(
                    "  invoke_via: `mcp_lg_{}` with `{{\"action\": \"{act}\", \"args\": {{ ... }}}}`\n",
                    item.invoke_via_mcp_name
                ));
            } else {
                entry_text.push_str(&format!(
                    "  invoke_via: `mcp_lg_{}` (direct 1:1 native MCP call)\n",
                    item.invoke_via_mcp_name
                ));
            }
            if !item.prose_description.is_empty() {
                let desc_preview = truncate_utf8_prefix(&item.prose_description, 600);
                entry_text.push_str(&format!("  description: {desc_preview}\n"));
            }
            if let Some(ref schema_str) = item.parameters_json {
                let schema_preview =
                    truncate_utf8_prefix(schema_str, MAX_SINGLE_SCHEMA_IN_SEARCH_BYTES);
                entry_text.push_str(&format!("  inputSchema: {schema_preview}\n"));
            }
            if has_code_mode_exec && let Some(ref ts_sig) = item.ts_declaration {
                let ts_preview = truncate_utf8_prefix(ts_sig, 1_200);
                entry_text.push_str(&format!("  codeModeTs: {ts_preview}\n"));
            }
            if out.len().saturating_add(entry_text.len()) > MAX_SEARCH_REPLY_BYTES {
                out.push_str("\n[Search output capped at 32 KiB; pass narrower `query` or `names` for remaining schemas.]\n");
                break;
            }
            out.push_str(&entry_text);
        }
        out
    }
}

fn truncate_utf8_prefix(s: &str, max_bytes: usize) -> Cow<'_, str> {
    if s.len() <= max_bytes {
        return Cow::Borrowed(s);
    }
    let cut = s.floor_char_boundary(max_bytes);
    let head = s.get(..cut).unwrap_or("");
    Cow::Owned(format!("{head}...[truncated]"))
}

fn extract_schema_metadata(parameters_json: Option<&str>) -> (Vec<String>, bool, bool, String) {
    let Some(raw) = parameters_json else {
        return (Vec::new(), false, false, String::new());
    };
    let Ok(val) = serde_json::from_str::<Value>(raw) else {
        return (
            Vec::new(),
            raw.contains("\"toolSummary\""),
            false,
            String::new(),
        );
    };
    let required_fields: Vec<String> = val
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let mut prop_names = Vec::new();
    let mut declares_tool_summary = false;
    if let Some(props) = val.get("properties").and_then(Value::as_object) {
        for key in props.keys() {
            if key == "toolSummary" {
                declares_tool_summary = true;
            }
            prop_names.push(key.as_str());
        }
    }
    let has_root_combinator =
        val.get("oneOf").is_some() || val.get("anyOf").is_some() || val.get("allOf").is_some();
    (
        required_fields,
        declares_tool_summary,
        has_root_combinator,
        prop_names.join(" "),
    )
}

fn is_tool_active(client_name: &str, active_tools: &[CanonicalToolSpec]) -> bool {
    active_tools.iter().any(|t| t.name == client_name)
}

fn disabled_tool_reply(
    reply_tx: oneshot::Sender<McpToolCallReply>,
    tool_name: &str,
) -> ResolvedMcpIncoming {
    ResolvedMcpIncoming::ImmediateLocalReply {
        reply_tx,
        reply: McpToolCallReply {
            content: vec![McpContentItem::Text {
                text: format!("Tool `{tool_name}` is disabled for this turn; do not call it."),
            }],
            is_error: true,
        },
        kind: LocalReplyKind::DisabledTool,
    }
}

fn preflight_error_reply(
    reply_tx: oneshot::Sender<McpToolCallReply>,
    message: String,
) -> ResolvedMcpIncoming {
    ResolvedMcpIncoming::ImmediateLocalReply {
        reply_tx,
        reply: McpToolCallReply {
            content: vec![McpContentItem::Text { text: message }],
            is_error: true,
        },
        kind: LocalReplyKind::PreflightError,
    }
}

fn format_direct_tool_arguments(raw_args: &Value, bridged: &McpBridgedTool) -> String {
    if bridged.is_custom {
        return extract_custom_tool_input(raw_args, bridged.unwrap_custom_key);
    }
    let mut args_val = raw_args.clone();
    if args_val.is_null() {
        args_val = Value::Object(serde_json::Map::new());
    }
    if let Value::Object(ref mut map) = args_val {
        let declares_summary = bridged
            .parameters_json
            .as_deref()
            .is_some_and(|s| s.contains("\"toolSummary\""));
        if !declares_summary {
            let _ = map.remove("toolSummary");
        }
        if let Some(ref params_json) = bridged.parameters_json {
            let _ = crate::turn_runner::coerce_stringified_container_fields(map, params_json);
        }
    }
    serde_json::to_string(&args_val).unwrap_or_else(|_| "{}".to_owned())
}

fn extract_virtualized_inner_args(
    outer: &Value,
    wrapper_keys: &[&str],
    target_required: &[String],
) -> Result<Value, String> {
    let Value::Object(outer_map) = outer else {
        return Ok(Value::Object(serde_json::Map::new()));
    };
    for key in ["args", "arguments", "params"] {
        if let Some(inner) = outer_map.get(key) {
            return match inner {
                Value::Null => Ok(Value::Object(serde_json::Map::new())),
                Value::Object(_) => Ok(inner.clone()),
                Value::String(raw_str) => {
                    let trimmed = raw_str.trim();
                    if trimmed.is_empty() {
                        Ok(Value::Object(serde_json::Map::new()))
                    } else {
                        match serde_json::from_str::<Value>(trimmed) {
                            Ok(parsed @ Value::Object(_)) => Ok(parsed),
                            _ => Err(format!(
                                "`{key}` must be a JSON object, received non-object string."
                            )),
                        }
                    }
                }
                _ => Err(format!("`{key}` must be a JSON object.")),
            };
        }
    }

    let mut fallback = serde_json::Map::new();
    for (k, v) in outer_map {
        let is_wrapper = wrapper_keys.contains(&k.as_str());
        let required_by_target = target_required.iter().any(|r| r == k);
        if !is_wrapper || (required_by_target && k != "action" && k != "toolSummary") {
            fallback.insert(k.clone(), v.clone());
        }
    }
    Ok(Value::Object(fallback))
}

fn finalize_indexed_tool_call(
    indexed: &IndexedClientTool,
    inner_args: &Value,
    enforce_required_preflight: bool,
    _stripped_keys: &[&str],
    reply_tx: oneshot::Sender<McpToolCallReply>,
) -> ResolvedMcpIncoming {
    if indexed.is_custom {
        let raw_code = extract_custom_tool_input(inner_args, indexed.unwrap_custom_key);
        return ResolvedMcpIncoming::ForwardToClient {
            client_name: indexed.client_name.clone(),
            arguments: raw_code,
            is_custom: true,
            reply_tx,
        };
    }

    let mut map = match inner_args {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    if !indexed.declares_tool_summary {
        let _ = map.remove("toolSummary");
    }
    if let Some(ref params_json) = indexed.parameters_json {
        let _ = crate::turn_runner::coerce_stringified_container_fields(&mut map, params_json);
    }

    if enforce_required_preflight
        && !indexed.has_root_combinator
        && !indexed.required_fields.is_empty()
    {
        let missing: Vec<&str> = indexed
            .required_fields
            .iter()
            .filter(|req_key| map.get(req_key.as_str()).is_none_or(Value::is_null))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            let provided: Vec<&str> = map.keys().map(String::as_str).collect();
            let schema_snippet = indexed
                .parameters_json
                .as_deref()
                .map(|s| truncate_utf8_prefix(s, MAX_SINGLE_SCHEMA_IN_SEARCH_BYTES))
                .unwrap_or(Cow::Borrowed("{}"));
            return preflight_error_reply(
                reply_tx,
                format!(
                    "Missing required parameter(s) [{}] for `{}` (provided keys: [{}]).\nExact inputSchema:\n{schema_snippet}\nRetry the call with all required parameters.",
                    missing.join(", "),
                    indexed.client_name,
                    provided.join(", ")
                ),
            );
        }
    }

    let arguments = serde_json::to_string(&Value::Object(map)).unwrap_or_else(|_| "{}".to_owned());
    ResolvedMcpIncoming::ForwardToClient {
        client_name: indexed.client_name.clone(),
        arguments,
        is_custom: false,
        reply_tx,
    }
}

fn find_action_in_hub<'a>(
    actions: &'a [IndexedClientTool],
    requested: &str,
) -> Option<&'a IndexedClientTool> {
    if let Some(exact) = actions
        .iter()
        .find(|a| a.short_action.as_deref() == Some(requested) || a.client_name == requested)
    {
        return Some(exact);
    }
    let trimmed_req = requested.trim_start_matches('_');
    if !trimmed_req.is_empty()
        && let Some(stripped) = actions.iter().find(|a| {
            a.short_action
                .as_deref()
                .is_some_and(|sa| sa.trim_start_matches('_').eq_ignore_ascii_case(trimmed_req))
                || a.client_name.eq_ignore_ascii_case(requested)
        })
    {
        return Some(stripped);
    }
    None
}

fn suggest_closest_actions(
    actions: &[IndexedClientTool],
    requested: &str,
    max_items: usize,
) -> Vec<String> {
    let req_lower = requested.trim_start_matches('_').to_ascii_lowercase();
    let parts: Vec<&str> = req_lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| p.len() >= 2)
        .collect();
    let mut ranked: Vec<(usize, usize, &str)> = actions
        .iter()
        .enumerate()
        .map(|(idx, a)| {
            let label = a.short_action.as_deref().unwrap_or(a.client_name.as_str());
            let lower = label.to_ascii_lowercase();
            let mut score = usize::from(lower.contains(&req_lower)) * 10;
            for p in &parts {
                if lower.contains(p) {
                    score = score.saturating_add(3);
                }
            }
            (score, idx, label)
        })
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    ranked
        .into_iter()
        .take(max_items)
        .map(|(_, _, s)| s.to_owned())
        .collect()
}

fn suggest_closest_tools(
    tools: &[IndexedClientTool],
    requested: &str,
    max_items: usize,
) -> Vec<String> {
    let req_lower = requested.to_ascii_lowercase();
    let parts: Vec<&str> = req_lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| p.len() >= 2)
        .collect();
    let mut ranked: Vec<(usize, usize, &str)> = tools
        .iter()
        .enumerate()
        .map(|(idx, t)| {
            let lower = t.client_name.to_ascii_lowercase();
            let mut score = usize::from(lower.contains(&req_lower)) * 10;
            for p in &parts {
                if lower.contains(p) {
                    score = score.saturating_add(3);
                }
            }
            (score, idx, t.client_name.as_str())
        })
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    ranked
        .into_iter()
        .take(max_items)
        .map(|(_, _, s)| s.to_owned())
        .collect()
}

fn score_indexed_tool(item: &IndexedClientTool, exact_names: &[String], tokens: &[String]) -> u32 {
    let mut score: u32 = 0;
    let client_lower = item.client_name.to_ascii_lowercase();
    let action_lower = item
        .short_action
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    let action_stripped = action_lower.trim_start_matches('_');

    if !exact_names.is_empty() {
        for target in exact_names {
            let target_stripped = target.trim_start_matches('_');
            if client_lower == *target
                || action_lower == *target
                || action_stripped == target_stripped
            {
                score = score.saturating_add(1_000);
            } else if client_lower.ends_with(target) {
                score = score.saturating_add(400);
            }
        }
    }

    if !tokens.is_empty() {
        let mut matched_tokens: u32 = 0;
        for tok in tokens {
            let mut tok_score: u32 = 0;
            if action_lower == *tok || action_stripped == tok {
                tok_score = tok_score.saturating_add(120);
            } else if action_lower.contains(tok) || client_lower.contains(tok) {
                tok_score = tok_score.saturating_add(60);
            }
            if item
                .namespace
                .as_deref()
                .is_some_and(|ns| ns.contains(tok.as_str()))
            {
                tok_score = tok_score.saturating_add(30);
            }
            if item.searchable_lower.contains(tok.as_str()) {
                tok_score = tok_score.saturating_add(15);
            }
            if tok_score > 0 {
                matched_tokens = matched_tokens.saturating_add(1);
                score = score.saturating_add(tok_score);
            }
        }
        if matched_tokens == u32::try_from(tokens.len()).unwrap_or(u32::MAX) {
            score = score.saturating_add(80);
        }
    }

    if exact_names.is_empty() && tokens.is_empty() {
        score = 1;
    }
    score
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
        let mut non_summary_iter = map.iter().filter(|(k, _)| k.as_str() != "toolSummary");
        if let Some((_, val)) = non_summary_iter.next()
            && non_summary_iter.next().is_none()
            && let Some(s) = val.as_str()
        {
            return s.to_owned();
        }
    }
    if arguments.is_null() {
        return String::new();
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
    fn partitions_large_catalogs_into_eager_safe_hubs_preserving_direct_exec_and_wait() {
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
        assert!(cat.wire_tools.len() <= MAX_NATIVE_MCP_TOOLS);
        assert!(cat.has_virtualized_dispatch);
        assert!(cat.lookup("exec").is_some_and(|b| b.is_custom));
        assert!(cat.lookup("wait").is_some_and(|b| !b.is_custom));
        assert!(cat.lookup("apply_patch").is_none());
        assert!(cat.xml_only_tools.iter().any(|t| t.name == "apply_patch"));
        assert_eq!(cat.xml_only_tools.len(), 1);
        assert!(
            cat.wire_tools
                .iter()
                .any(|w| w.name == SEARCH_CLIENT_TOOLS_MCP_NAME)
        );
        assert!(
            cat.wire_tools
                .iter()
                .any(|w| w.name == CALL_CLIENT_TOOL_MCP_NAME)
        );
        assert!(cat.wire_tools.iter().any(|w| w.name == "ns__extra"));

        assert_eq!(strip_call_prefix("call_abc123"), "abc123");
        assert_eq!(strip_call_prefix("toolu_abc123"), "abc123");
        assert_eq!(strip_call_prefix("raw_99"), "raw_99");
    }

    #[tokio::test]
    async fn codex_128_tool_catalog_keeps_exec_and_exec_command_eager_and_resolves_github_hub_with_preflight()
     {
        let srv = LoopbackMcpServer::bind(
            NonZeroUsize::new(1024 * 1024).expect("nonzero"),
            NonZeroUsize::new(16).expect("nonzero"),
        )
        .await
        .expect("bind loopback MCP server");

        let ts_footer = "\n\nexec tool declaration:\n```ts\ndeclare const tools: { sample(args: { id: number }): Promise<unknown>; };\n```";
        let mut tools = vec![
            CanonicalToolSpec {
                name: "exec".to_owned(),
                description: Some("Execute JavaScript in Code Mode. globalThis.tools has nested declarations.".repeat(40)),
                kind: CanonicalToolKind::Custom { format_json: None },
            },
            CanonicalToolSpec {
                name: "wait".to_owned(),
                description: Some(format!("Wait on running exec cell.{ts_footer}")),
                kind: CanonicalToolKind::Function {
                    parameters_json: r#"{"type":"object","properties":{"cell_id":{"type":"string"}},"required":["cell_id"]}"#.to_owned(),
                },
            },
            CanonicalToolSpec {
                name: "exec_command".to_owned(),
                description: Some(format!("Run shell command in PTY.{ts_footer}")),
                kind: CanonicalToolKind::Function {
                    parameters_json: r#"{"type":"object","properties":{"cmd":{"type":"string"}},"required":["cmd"]}"#.to_owned(),
                },
            },
            CanonicalToolSpec {
                name: "apply_patch".to_owned(),
                description: Some("Freeform patch.".to_owned()),
                kind: CanonicalToolKind::Custom { format_json: None },
            },
        ];
        for i in 0..80 {
            tools.push(CanonicalToolSpec {
                name: format!("mcp__codex_apps__github._action_{i:02}"),
                description: Some(format!("GitHub connector endpoint #{i}.{ts_footer}")),
                kind: CanonicalToolKind::Function {
                    parameters_json: r#"{"type":"object","properties":{"repository_full_name":{"type":"string"},"pr_number":{"type":"integer"}},"required":["repository_full_name","pr_number"]}"#.to_owned(),
                },
            });
        }

        let cat = partition_tools_for_mcp(&tools, &mut NoopSpillSink);
        assert!(cat.wire_tools.len() <= 10);
        assert!(cat.lookup("exec").is_some());
        assert!(cat.lookup("exec_command").is_some());
        assert!(cat.lookup("wait").is_some());
        assert_eq!(cat.xml_only_tools.len(), 1);

        let handle = srv.register_session(cat);

        // 1. 0-RTT gateway-local search returns both JSON schema and extracted Code Mode TS footer
        let (tx_s, rx_s) = oneshot::channel();
        match handle.resolve_incoming_call(
            IncomingMcpToolCall {
                mcp_name: SEARCH_CLIENT_TOOLS_MCP_NAME.to_owned(),
                arguments: json!({"namespace": "github", "query": "action_42"}),
                reply_tx: tx_s,
            },
            &tools,
        ) {
            ResolvedMcpIncoming::ImmediateLocalReply {
                reply_tx,
                reply,
                kind,
            } => {
                assert_eq!(kind, LocalReplyKind::Search);
                assert!(!reply.is_error);
                let _ = reply_tx.send(reply);
            }
            ResolvedMcpIncoming::ForwardToClient { .. } => panic!("search must resolve locally"),
        }
        let search_out = rx_s.await.expect("search reply");
        let McpContentItem::Text { ref text } = search_out.content[0] else {
            panic!("expected text search output");
        };
        assert!(text.contains("mcp__codex_apps__github._action_42"));
        assert!(text.contains("repository_full_name"));
        assert!(text.contains("declare const tools:"));

        // 2. Missing required parameter triggers preflight self-healing locally with exact schema
        let (tx_bad, _rx_bad) = oneshot::channel();
        match handle.resolve_incoming_call(
            IncomingMcpToolCall {
                mcp_name: "ns__github".to_owned(),
                arguments: json!({"action": "_action_42", "args": {"repo_full_name": "a/b", "pr_number": 7}}),
                reply_tx: tx_bad,
            },
            &tools,
        ) {
            ResolvedMcpIncoming::ImmediateLocalReply { reply, kind, .. } => {
                assert_eq!(kind, LocalReplyKind::PreflightError);
                assert!(reply.is_error);
                let McpContentItem::Text { ref text } = reply.content[0] else {
                    panic!("expected text error");
                };
                assert!(text.contains("repository_full_name"));
            }
            ResolvedMcpIncoming::ForwardToClient { .. } => {
                panic!("missing required field must be caught by preflight")
            }
        }

        // 3. Valid Hub call unwraps cleanly to exact original client tool name & JSON arguments
        let (tx_ok, _rx_ok) = oneshot::channel();
        match handle.resolve_incoming_call(
            IncomingMcpToolCall {
                mcp_name: "ns__github".to_owned(),
                arguments: json!({"action": "action_42", "args": {"repository_full_name": "acking-you/liftgate", "pr_number": 14}}),
                reply_tx: tx_ok,
            },
            &tools,
        ) {
            ResolvedMcpIncoming::ForwardToClient {
                client_name,
                arguments,
                is_custom,
                ..
            } => {
                assert_eq!(client_name, "mcp__codex_apps__github._action_42");
                assert!(!is_custom);
                let parsed: Value = serde_json::from_str(&arguments).expect("valid JSON");
                assert_eq!(parsed["repository_full_name"], "acking-you/liftgate");
                assert_eq!(parsed["pr_number"], 14);
            }
            ResolvedMcpIncoming::ImmediateLocalReply { .. } => {
                panic!("valid hub call must forward to client")
            }
        }
    }

    #[test]
    fn extract_custom_tool_input_ignores_tool_summary_and_null() {
        assert_eq!(
            extract_custom_tool_input(
                &json!({"script": "console.log(42);", "toolSummary": "Execute JS"}),
                None
            ),
            "console.log(42);"
        );
        assert_eq!(
            extract_custom_tool_input(&json!({"toolSummary": "Execute JS"}), None),
            r#"{"toolSummary":"Execute JS"}"#
        );
        assert_eq!(extract_custom_tool_input(&Value::Null, None), "");
    }

    #[tokio::test]
    async fn register_session_clears_xml_overflow_and_accounts_schema_bytes() {
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
                name: "apply_patch".to_owned(),
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
        assert_eq!(cat.xml_only_tools.len(), 1);

        let handle = srv.register_session(cat);
        assert!(handle.catalog().xml_only_tools.is_empty());
        assert!(handle.estimated_retained_bytes() >= desc.len() + params.len());
    }
}
