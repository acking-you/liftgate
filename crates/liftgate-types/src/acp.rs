use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::config::SessionId;

pub const JSONRPC_VERSION: &str = "2.0";
pub const ACP_PROTOCOL_VERSION: u32 = 1;

/// Outbound JSON-RPC 2.0 request frame.
#[derive(Debug, Serialize)]
pub struct JsonRpcRequest<'a, P> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    pub params: P,
}

impl<'a, P> JsonRpcRequest<'a, P> {
    #[must_use]
    pub const fn new(id: u64, method: &'a str, params: P) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            method,
            params,
        }
    }
}

/// Outbound JSON-RPC 2.0 notification frame.
#[derive(Debug, Serialize)]
pub struct JsonRpcNotification<'a, P> {
    pub jsonrpc: &'static str,
    pub method: &'a str,
    pub params: P,
}

impl<'a, P> JsonRpcNotification<'a, P> {
    #[must_use]
    pub const fn new(method: &'a str, params: P) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            method,
            params,
        }
    }
}

/// Outbound JSON-RPC 2.0 response frame (for answering agent-initiated requests).
#[derive(Debug, Serialize)]
pub struct JsonRpcResponseOut<'a, R> {
    pub jsonrpc: &'static str,
    pub id: &'a RawValue,
    pub result: R,
}

/// Zero-copy borrowed JSON-RPC frame decoded from a single ACP stdout line.
#[derive(Debug, Deserialize)]
pub struct IncomingRpcFrame<'a> {
    #[serde(default, borrow)]
    pub id: Option<&'a RawValue>,
    #[serde(default, borrow)]
    pub method: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    pub params: Option<&'a RawValue>,
    #[serde(default, borrow)]
    pub result: Option<&'a RawValue>,
    #[serde(default)]
    pub error: Option<JsonRpcErrorPayload>,
}

/// JSON-RPC 2.0 error object.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JsonRpcErrorPayload {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Box<RawValue>>,
}

/// `initialize` request parameters.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams<'a> {
    pub protocol_version: u32,
    pub client_capabilities: ClientCapabilities,
    pub client_info: ClientInfo<'a>,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    pub fs: FsCapabilities,
    pub terminal: bool,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct ClientInfo<'a> {
    pub name: &'a str,
    pub version: &'a str,
}

/// `initialize` response payload.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: u32,
    #[serde(default)]
    pub agent_info: Option<AgentInfo>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AgentInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
}

/// `session/new` request parameters.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNewParams<'a> {
    pub cwd: &'a str,
    pub mcp_servers: &'a [McpServerConfig],
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<SessionNewMeta<'a>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    pub name: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub transport_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionNewMeta<'a> {
    pub antigravity: AntigravityNewMeta<'a>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityNewMeta<'a> {
    pub system_prompt: SystemPromptAppendMeta<'a>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SystemPromptAppendMeta<'a> {
    pub append: &'a str,
}

/// `session/new` response payload.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNewResult {
    pub session_id: SessionId,
    #[serde(default)]
    pub models: ModelsCatalog,
    #[serde(default)]
    pub modes: ModesCatalog,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsCatalog {
    #[serde(default)]
    pub available_models: Vec<AvailableModel>,
    #[serde(default)]
    pub current_model_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableModel {
    pub model_id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModesCatalog {
    #[serde(default)]
    pub current_mode_id: Option<String>,
    #[serde(default)]
    pub available_modes: Vec<AvailableMode>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AvailableMode {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

/// `session/set_model` request parameters.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSetModelParams<'a> {
    pub session_id: &'a str,
    pub model_id: &'a str,
}

/// `session/set_mode` request parameters.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSetModeParams<'a> {
    pub session_id: &'a str,
    pub mode_id: &'a str,
}

/// `session/prompt` request parameters.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPromptParams<'a> {
    pub session_id: &'a str,
    pub prompt: &'a [AcpPromptBlock],
}

/// Content blocks accepted by ACP `session/prompt`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AcpPromptBlock {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

/// `session/prompt` final response payload.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptTurnResult {
    #[serde(default)]
    pub stop_reason: String,
    #[serde(default)]
    pub usage: AcpUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Box<RawValue>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_read_tokens: u64,
    #[serde(default)]
    pub cached_write_tokens: u64,
    #[serde(default)]
    pub thought_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

/// Computes prompt cache hit rate in rounded basis points (`0..=10_000`, where `10_000 == 100.00%`),
/// returning `None` when `total_prompt_tokens == 0`.
#[must_use]
pub fn compute_cache_hit_bps(cached_read_tokens: u64, total_prompt_tokens: u64) -> Option<u16> {
    if total_prompt_tokens == 0 {
        return None;
    }
    let clamped_cached = cached_read_tokens.min(total_prompt_tokens);
    let bps = (u128::from(clamped_cached)
        .saturating_mul(10_000)
        .saturating_add(u128::from(total_prompt_tokens) / 2))
        / u128::from(total_prompt_tokens);
    Some(u16::try_from(bps.min(10_000)).unwrap_or(10_000))
}

impl AcpUsage {
    /// Total prompt/context input tokens (`input_tokens + cached_read_tokens + cached_write_tokens`).
    #[must_use]
    pub const fn total_prompt_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cached_read_tokens)
            .saturating_add(self.cached_write_tokens)
    }

    /// Prompt cache hit rate in basis points (`0..=10_000`), or `None` when `total_prompt_tokens == 0`.
    #[must_use]
    pub fn cache_hit_bps(&self) -> Option<u16> {
        compute_cache_hit_bps(self.cached_read_tokens, self.total_prompt_tokens())
    }
}

/// `session/cancel` notification/request parameters.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCancelParams<'a> {
    pub session_id: &'a str,
}

/// `session/close` request parameters.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCloseParams<'a> {
    pub session_id: &'a str,
}

/// `session/update` notification parameters.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdateNotification {
    pub session_id: SessionId,
    pub update: SessionUpdatePayload,
}

/// Tagged variants inside `session/update` notifications.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdatePayload {
    AgentMessageChunk {
        content: AcpChunkContent,
    },
    AgentThoughtChunk {
        content: AcpChunkContent,
    },
    #[serde(rename_all = "camelCase")]
    ToolCall {
        #[serde(default)]
        tool_call_id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        status: String,
        #[serde(default)]
        kind: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        args: Option<Box<RawValue>>,
        #[serde(default)]
        raw_input: Option<Box<RawValue>>,
    },
    #[serde(rename_all = "camelCase")]
    ToolCallUpdate {
        #[serde(default)]
        tool_call_id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        status: String,
        #[serde(default)]
        kind: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        args: Option<Box<RawValue>>,
        #[serde(default)]
        raw_input: Option<Box<RawValue>>,
    },
    UsageUpdate {
        #[serde(default)]
        used: u64,
        #[serde(default)]
        size: u64,
    },
    Error {
        #[serde(default)]
        message: String,
        #[serde(default)]
        error: Option<Box<RawValue>>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AcpChunkContent {
    #[serde(rename = "type", default)]
    pub content_type: String,
    #[serde(default)]
    pub text: String,
}

/// `session/request_permission` incoming request parameters (in case agent requests approval).
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionParams {
    #[serde(default)]
    pub options: Vec<PermissionOption>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    #[serde(default)]
    pub kind: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RequestPermissionResult {
    pub outcome: PermissionOutcome,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "camelCase")]
pub enum PermissionOutcome {
    Selected {
        #[serde(rename = "optionId")]
        option_id: String,
    },
}
