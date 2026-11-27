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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Component-wise saturating addition across two [`AcpUsage`] snapshots.
    #[must_use]
    pub const fn saturating_add(self, rhs: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(rhs.input_tokens),
            output_tokens: self.output_tokens.saturating_add(rhs.output_tokens),
            cached_read_tokens: self
                .cached_read_tokens
                .saturating_add(rhs.cached_read_tokens),
            cached_write_tokens: self
                .cached_write_tokens
                .saturating_add(rhs.cached_write_tokens),
            thought_tokens: self.thought_tokens.saturating_add(rhs.thought_tokens),
            total_tokens: self.total_tokens.saturating_add(rhs.total_tokens),
        }
    }

    /// Component-wise saturating subtraction (`self - rhs`) across two [`AcpUsage`] snapshots.
    #[must_use]
    pub const fn saturating_sub(self, rhs: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(rhs.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(rhs.output_tokens),
            cached_read_tokens: self
                .cached_read_tokens
                .saturating_sub(rhs.cached_read_tokens),
            cached_write_tokens: self
                .cached_write_tokens
                .saturating_sub(rhs.cached_write_tokens),
            thought_tokens: self.thought_tokens.saturating_sub(rhs.thought_tokens),
            total_tokens: self.total_tokens.saturating_sub(rhs.total_tokens),
        }
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
///
/// Deserialized via a two-step `RawValue` slice rather than `#[serde(tag = "sessionUpdate")]`
/// because `serde`'s internal-tag `Content` buffer rejects `Box<RawValue>` fields (`args`,
/// `rawInput`, `error`), which would otherwise drop real `tool_call` / `tool_call_update` frames.
#[derive(Clone, Debug)]
pub enum SessionUpdatePayload {
    AgentMessageChunk {
        content: AcpChunkContent,
    },
    AgentThoughtChunk {
        content: AcpChunkContent,
    },
    ToolCall {
        tool_call_id: String,
        name: String,
        status: String,
        kind: String,
        title: String,
        args: Option<Box<RawValue>>,
        raw_input: Option<Box<RawValue>>,
    },
    ToolCallUpdate {
        tool_call_id: String,
        name: String,
        status: String,
        kind: String,
        title: String,
        args: Option<Box<RawValue>>,
        raw_input: Option<Box<RawValue>>,
    },
    UsageUpdate {
        used: u64,
        size: u64,
    },
    Error {
        message: String,
        error: Option<Box<RawValue>>,
    },
    Unknown,
}

impl<'de> Deserialize<'de> for SessionUpdatePayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        Self::from_raw_update_json(raw.get()).map_err(serde::de::Error::custom)
    }
}

impl SessionUpdatePayload {
    fn from_raw_update_json(raw_json: &str) -> Result<Self, serde_json::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct UpdateDiscriminator<'a> {
            #[serde(default, borrow)]
            session_update: Cow<'a, str>,
        }

        #[derive(Deserialize)]
        struct RawChunkFields {
            #[serde(default)]
            content: AcpChunkContent,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RawToolCallFields {
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
        }

        #[derive(Deserialize)]
        struct RawUsageFields {
            #[serde(default)]
            used: u64,
            #[serde(default)]
            size: u64,
        }

        #[derive(Deserialize)]
        struct RawErrorFields {
            #[serde(default)]
            message: String,
            #[serde(default)]
            error: Option<Box<RawValue>>,
        }

        let disc: UpdateDiscriminator<'_> = serde_json::from_str(raw_json)?;
        match disc.session_update.as_ref() {
            "agent_message_chunk" => {
                let f: RawChunkFields = serde_json::from_str(raw_json)?;
                Ok(Self::AgentMessageChunk { content: f.content })
            }
            "agent_thought_chunk" => {
                let f: RawChunkFields = serde_json::from_str(raw_json)?;
                Ok(Self::AgentThoughtChunk { content: f.content })
            }
            "tool_call" => {
                let f: RawToolCallFields = serde_json::from_str(raw_json)?;
                Ok(Self::ToolCall {
                    tool_call_id: f.tool_call_id,
                    name: f.name,
                    status: f.status,
                    kind: f.kind,
                    title: f.title,
                    args: f.args,
                    raw_input: f.raw_input,
                })
            }
            "tool_call_update" => {
                let f: RawToolCallFields = serde_json::from_str(raw_json)?;
                Ok(Self::ToolCallUpdate {
                    tool_call_id: f.tool_call_id,
                    name: f.name,
                    status: f.status,
                    kind: f.kind,
                    title: f.title,
                    args: f.args,
                    raw_input: f.raw_input,
                })
            }
            "usage_update" => {
                let f: RawUsageFields = serde_json::from_str(raw_json)?;
                Ok(Self::UsageUpdate {
                    used: f.used,
                    size: f.size,
                })
            }
            "error" => {
                let f: RawErrorFields = serde_json::from_str(raw_json)?;
                Ok(Self::Error {
                    message: f.message,
                    error: f.error,
                })
            }
            other => {
                if let Ok(f) = serde_json::from_str::<RawErrorFields>(raw_json) {
                    let kind_lower = other.to_ascii_lowercase();
                    let has_non_null_error = f
                        .error
                        .as_ref()
                        .is_some_and(|r| !matches!(r.get().trim(), "" | "null" | "{}"));
                    if kind_lower.contains("error")
                        || kind_lower.contains("fail")
                        || has_non_null_error
                    {
                        return Ok(Self::Error {
                            message: f.message,
                            error: f.error,
                        });
                    }
                }
                Ok(Self::Unknown)
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_update_deserializes_tool_call_and_error_with_raw_value_fields() {
        let tc_json = r#"{
            "sessionId": "8e4d22e6-a67d-4b9e-8c18-111111111111",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "call_3949974099485412213",
                "name": "search_web",
                "args": { "query": "Shenzhen weather tomorrow" },
                "status": "in_progress",
                "kind": "search",
                "title": "\"Shenzhen weather tomorrow\" on Web",
                "rawInput": { "query": "Shenzhen weather tomorrow" }
            }
        }"#;
        let parsed: SessionUpdateNotification =
            serde_json::from_str(tc_json).expect("tool_call with RawValue must deserialize");
        match parsed.update {
            SessionUpdatePayload::ToolCall {
                tool_call_id,
                name,
                status,
                kind,
                title,
                args,
                raw_input,
            } => {
                assert_eq!(tool_call_id, "call_3949974099485412213");
                assert_eq!(name, "search_web");
                assert_eq!(status, "in_progress");
                assert_eq!(kind, "search");
                assert_eq!(title, "\"Shenzhen weather tomorrow\" on Web");
                assert!(
                    args.expect("args")
                        .get()
                        .contains("Shenzhen weather tomorrow")
                );
                assert!(
                    raw_input
                        .expect("rawInput")
                        .get()
                        .contains("Shenzhen weather tomorrow")
                );
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }

        let upd_json = r#"{
            "sessionId": "8e4d22e6-a67d-4b9e-8c18-111111111111",
            "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_3949974099485412213",
                "name": "search_web",
                "args": { "query": "Shenzhen weather tomorrow" },
                "status": "completed",
                "kind": "search",
                "title": "\"Shenzhen weather tomorrow\" on Web",
                "rawInput": { "query": "Shenzhen weather tomorrow" }
            }
        }"#;
        let parsed_upd: SessionUpdateNotification =
            serde_json::from_str(upd_json).expect("tool_call_update must deserialize");
        assert!(matches!(
            parsed_upd.update,
            SessionUpdatePayload::ToolCallUpdate { ref status, .. } if status == "completed"
        ));

        let err_json = r#"{
            "sessionId": "sess_err",
            "update": {
                "sessionUpdate": "turn_failed",
                "message": "RESOURCE_EXHAUSTED",
                "error": { "code": 429, "status": "RESOURCE_EXHAUSTED" }
            }
        }"#;
        let parsed_err: SessionUpdateNotification =
            serde_json::from_str(err_json).expect("rescued error update must deserialize");
        match parsed_err.update {
            SessionUpdatePayload::Error { message, error } => {
                assert_eq!(message, "RESOURCE_EXHAUSTED");
                assert!(error.expect("error raw").get().contains("429"));
            }
            other => panic!("expected Error, got {other:?}"),
        }

        let info_json = r#"{
            "sessionId": "sess_info",
            "update": {
                "sessionUpdate": "session_info_update",
                "title": "Shenzhen Weather Forecast"
            }
        }"#;
        let parsed_info: SessionUpdateNotification =
            serde_json::from_str(info_json).expect("unknown update must map to Unknown");
        assert!(matches!(parsed_info.update, SessionUpdatePayload::Unknown));
    }
}
