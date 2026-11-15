use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

use crate::acp::AcpUsage;
use crate::canonical::{
    CanonicalMessage, CanonicalPart, CanonicalRole, CanonicalToolKind, CanonicalToolSpec,
    NormalizedTurnRequest, ReasoningEffort, detect_auxiliary_task,
};
use crate::error::ProtocolError;
use crate::session_key::{
    materialize_system_prompt_into_messages, strip_billing_noise, wrap_developer_instructions_block,
};

pub const ANTHROPIC_THOUGHT_SIGNATURE: &str = "agy-acp-thought-v1";

/// Incoming `POST /v1/messages` request body.
#[derive(Debug, Deserialize)]
pub struct AnthropicMessagesRequest {
    pub model: String,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub system: Option<AnthropicSystemPrompt>,
    #[serde(default)]
    pub messages: Vec<AnthropicMessage>,
    #[serde(default)]
    pub tools: Vec<AnthropicToolSpec>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub thinking: Option<AnthropicThinkingConfig>,
    #[serde(default)]
    pub output_config: Option<AnthropicOutputConfig>,
    #[serde(default)]
    pub stream: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicOutputConfig {
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub format: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum AnthropicSystemPrompt {
    Text(String),
    Blocks(Vec<AnthropicSystemBlock>),
}

impl AnthropicSystemPrompt {
    #[must_use]
    pub fn into_joined_text(self) -> String {
        match self {
            Self::Text(s) => sanitize_system_segment(&s).unwrap_or_default(),
            Self::Blocks(blocks) => blocks
                .into_iter()
                .filter_map(|b| b.text.and_then(|t| sanitize_system_segment(&t)))
                .collect::<Vec<_>>()
                .join("\n\n"),
        }
    }
}

fn sanitize_system_segment(raw: &str) -> Option<String> {
    let cleaned = strip_billing_noise(raw);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "You are a Claude agent, built on Anthropic's Claude Agent SDK." {
        return Some("You are a coding agent assisting the user via their CLI harness.".to_owned());
    }
    Some(cleaned)
}

#[derive(Debug, Deserialize)]
pub struct AnthropicSystemBlock {
    #[serde(rename = "type", default)]
    pub block_type: String,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: AnthropicMessageContent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum AnthropicMessageContent {
    Text(String),
    Blocks(Vec<AnthropicInputBlock>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicInputBlock {
    Text {
        text: String,
    },
    Image {
        source: AnthropicImageSource,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    RedactedThinking {
        #[serde(default)]
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Option<AnthropicToolResultContent>,
        #[serde(default)]
        is_error: Option<bool>,
    },
    ServerToolUse {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        input: Value,
    },
    WebSearchToolResult {
        #[serde(default)]
        tool_use_id: String,
        #[serde(default)]
        content: Value,
    },
    WebFetchToolResult {
        #[serde(default)]
        tool_use_id: String,
        #[serde(default)]
        content: Value,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicImageSource {
    #[serde(rename = "type")]
    pub source_type: String,
    #[serde(default)]
    pub media_type: String,
    #[serde(default)]
    pub data: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum AnthropicToolResultContent {
    Text(String),
    Blocks(Vec<AnthropicToolResultBlock>),
}

impl AnthropicToolResultContent {
    #[must_use]
    pub fn to_plain_string(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| b.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AnthropicToolResultBlock {
    #[serde(rename = "type", default)]
    pub block_type: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub source: Option<AnthropicImageSource>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicToolSpec {
    #[serde(rename = "type", default)]
    pub tool_type: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Option<Box<RawValue>>,
    #[serde(default)]
    pub max_uses: Option<u32>,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    #[serde(default)]
    pub blocked_domains: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicThinkingConfig {
    #[serde(rename = "type")]
    pub thinking_type: String,
    #[serde(default)]
    pub budget_tokens: Option<u64>,
    #[serde(default)]
    pub display: Option<String>,
}

/// Normalizes an `AnthropicMessagesRequest` into a `NormalizedTurnRequest`.
///
/// Important ACP compatibility invariant: caller `system` prompts (such as Claude Code's ~30–40 KB
/// workspace + harness system instructions) are emitted as `<developer_instructions>` blocks
/// inside `messages` rather than `_meta.antigravity.systemPrompt.append`, avoiding upstream
/// `429 RESOURCE_EXHAUSTED` rejections on large harness prompts while preserving deterministic
/// `SessionPool` prefix and `/compact` dialogue hashes across turns.
pub fn normalize_anthropic_request(
    req: AnthropicMessagesRequest,
) -> Result<NormalizedTurnRequest, ProtocolError> {
    let mut aux_system = req
        .system
        .map(AnthropicSystemPrompt::into_joined_text)
        .map(|s| strip_billing_noise(&s))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());

    if let Some(schema) = req
        .output_config
        .as_ref()
        .and_then(|cfg| cfg.format.as_ref())
        .filter(|fmt| fmt.get("type").and_then(Value::as_str) == Some("json_schema"))
        .and_then(|fmt| fmt.get("schema"))
        && let Ok(schema_json) = serde_json::to_string(schema)
    {
        let directive = format!(
            "Respond ONLY with valid JSON conforming strictly to this JSON schema:\n{schema_json}"
        );
        match &mut aux_system {
            Some(existing) => {
                existing.push_str("\n\n");
                existing.push_str(&directive);
            }
            None => aux_system = Some(directive),
        }
    }

    let mut conv_messages = Vec::with_capacity(req.messages.len());
    let mut has_conversational_input = false;
    for msg in req.messages {
        let is_system_or_developer = matches!(msg.role.as_str(), "system" | "developer");
        let role = if msg.role == "assistant" {
            CanonicalRole::Assistant
        } else {
            CanonicalRole::User
        };

        let mut parts = Vec::new();
        match msg.content {
            AnthropicMessageContent::Text(s) => {
                if is_system_or_developer {
                    if let Some(wrapped) = wrap_developer_instructions_block(&s) {
                        parts.push(CanonicalPart::Text(wrapped));
                    }
                } else {
                    has_conversational_input = true;
                    parts.push(CanonicalPart::Text(s));
                }
            }
            AnthropicMessageContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        AnthropicInputBlock::Text { text } => {
                            if is_system_or_developer {
                                if let Some(wrapped) = wrap_developer_instructions_block(&text) {
                                    parts.push(CanonicalPart::Text(wrapped));
                                }
                            } else {
                                has_conversational_input = true;
                                parts.push(CanonicalPart::Text(text));
                            }
                        }
                        AnthropicInputBlock::Image { source } => {
                            has_conversational_input = true;
                            if source.source_type == "base64"
                                && !source.media_type.is_empty()
                                && !source.data.is_empty()
                            {
                                parts.push(CanonicalPart::ImageBase64 {
                                    media_type: source.media_type,
                                    data: source.data,
                                });
                            } else {
                                return Err(ProtocolError::UnsupportedImageUrl);
                            }
                        }
                        AnthropicInputBlock::Thinking { thinking, .. } => {
                            has_conversational_input = true;
                            parts.push(CanonicalPart::Reasoning(thinking));
                        }
                        AnthropicInputBlock::ToolUse { id, name, input } => {
                            has_conversational_input = true;
                            let arguments = serde_json::to_string(&input)?;
                            parts.push(CanonicalPart::ToolCall {
                                call_id: id,
                                name,
                                arguments,
                                is_custom: false,
                            });
                        }
                        AnthropicInputBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            has_conversational_input = true;
                            let mut image_parts = Vec::new();
                            let output = match content {
                                Some(AnthropicToolResultContent::Text(s)) => s,
                                Some(AnthropicToolResultContent::Blocks(blocks)) => {
                                    let mut text_chunks = Vec::new();
                                    for b in blocks {
                                        if b.block_type == "image" {
                                            if let Some(src) = b.source {
                                                if src.source_type == "base64"
                                                    && !src.media_type.is_empty()
                                                    && !src.data.is_empty()
                                                {
                                                    image_parts.push(CanonicalPart::ImageBase64 {
                                                        media_type: src.media_type,
                                                        data: src.data,
                                                    });
                                                } else {
                                                    return Err(ProtocolError::UnsupportedImageUrl);
                                                }
                                            }
                                        } else if let Some(t) = b.text {
                                            text_chunks.push(t);
                                        }
                                    }
                                    if text_chunks.is_empty() && !image_parts.is_empty() {
                                        "[Image attached in tool result]".to_owned()
                                    } else {
                                        text_chunks.join("\n")
                                    }
                                }
                                None => String::new(),
                            };
                            parts.push(CanonicalPart::ToolResult {
                                call_id: tool_use_id,
                                output,
                                is_error: is_error.unwrap_or(false),
                            });
                            parts.extend(image_parts);
                        }
                        AnthropicInputBlock::RedactedThinking { .. }
                        | AnthropicInputBlock::ServerToolUse { .. }
                        | AnthropicInputBlock::WebSearchToolResult { .. }
                        | AnthropicInputBlock::WebFetchToolResult { .. }
                        | AnthropicInputBlock::Unknown => {}
                    }
                }
            }
        }

        if !parts.is_empty() {
            conv_messages.push(CanonicalMessage { role, parts });
        }
    }

    if !has_conversational_input || conv_messages.is_empty() {
        return Err(ProtocolError::EmptyInput);
    }

    let choice_type = req
        .tool_choice
        .as_ref()
        .and_then(|v| v.get("type"))
        .and_then(Value::as_str);
    let choice_tool_name = req
        .tool_choice
        .as_ref()
        .and_then(|v| v.get("name"))
        .and_then(Value::as_str);
    let tool_choice_is_none = choice_type == Some("none");

    let mut client_tools = Vec::new();
    let mut enable_native_web_search = false;
    let mut enable_native_web_fetch = false;
    let mut web_fetch_max_uses = None;
    let mut web_fetch_allowed_domains = Vec::new();
    let mut web_fetch_blocked_domains = Vec::new();

    if !tool_choice_is_none {
        for tool in req.tools {
            if let Some(tt) = &tool.tool_type {
                if tt.starts_with("web_search") {
                    enable_native_web_search = true;
                    continue;
                }
                if tt.starts_with("web_fetch") {
                    enable_native_web_fetch = true;
                    if tool.max_uses.is_some() {
                        web_fetch_max_uses = tool.max_uses;
                    }
                    web_fetch_allowed_domains.extend(tool.allowed_domains);
                    web_fetch_blocked_domains.extend(tool.blocked_domains);
                    continue;
                }
            }
            if !tool.name.trim().is_empty() {
                let schema_json = tool.input_schema.as_ref().map_or_else(
                    || "{\"type\":\"object\"}".to_owned(),
                    |r| r.get().to_owned(),
                );
                client_tools.push(CanonicalToolSpec {
                    name: tool.name,
                    description: tool.description,
                    kind: CanonicalToolKind::Function {
                        parameters_json: schema_json,
                    },
                });
            }
        }
    }

    let auxiliary_task = detect_auxiliary_task(
        aux_system.as_deref(),
        &conv_messages,
        &client_tools,
        enable_native_web_search,
        enable_native_web_fetch,
    );

    let mut messages = conv_messages;
    materialize_system_prompt_into_messages(
        aux_system.as_deref(),
        false,
        &client_tools,
        &mut messages,
    );

    let force_native_web_search = enable_native_web_search
        && (choice_tool_name == Some("web_search")
            || auxiliary_task == Some(crate::canonical::AuxiliaryTaskKind::WebSearchSubrequest)
            || (choice_type == Some("any") && client_tools.is_empty() && !enable_native_web_fetch));

    let force_native_web_fetch = enable_native_web_fetch
        && (choice_tool_name == Some("web_fetch")
            || (choice_type == Some("any")
                && client_tools.is_empty()
                && !enable_native_web_search));

    let omit_thinking_text =
        req.thinking.as_ref().and_then(|cfg| cfg.display.as_deref()) == Some("omitted");

    let explicit_effort = req
        .output_config
        .as_ref()
        .and_then(|cfg| cfg.effort.as_deref())
        .and_then(ReasoningEffort::from_effort_str);

    let (include_thinking, reasoning_effort) = if auxiliary_task.is_some() {
        (false, Some(ReasoningEffort::Low))
    } else {
        match &req.thinking {
            Some(cfg) if cfg.thinking_type == "enabled" || cfg.thinking_type == "adaptive" => {
                let effort = explicit_effort.unwrap_or_else(|| {
                    cfg.budget_tokens
                        .map_or(ReasoningEffort::High, ReasoningEffort::from_budget_tokens)
                });
                (true, Some(effort))
            }
            _ => (false, explicit_effort),
        }
    };

    Ok(NormalizedTurnRequest {
        requested_model: Some(req.model),
        system_instructions: None,
        messages,
        client_tools,
        enable_native_web_search,
        force_native_web_search,
        enable_native_web_fetch,
        force_native_web_fetch,
        web_fetch_max_uses,
        web_fetch_allowed_domains,
        web_fetch_blocked_domains,
        auxiliary_task,
        previous_response_id: None,
        reasoning_effort,
        include_thinking,
        omit_thinking_text,
        include_encrypted_reasoning: false,
        stream: req.stream.unwrap_or(false),
    })
}

/// Estimates token count for `POST /v1/messages/count_tokens`.
#[must_use]
pub fn estimate_anthropic_tokens(req: &AnthropicMessagesRequest) -> u64 {
    let mut chars: usize = 0;
    if let Some(sys) = &req.system {
        match sys {
            AnthropicSystemPrompt::Text(s) => chars = chars.saturating_add(s.len()),
            AnthropicSystemPrompt::Blocks(blocks) => {
                for b in blocks {
                    if let Some(t) = &b.text {
                        chars = chars.saturating_add(t.len());
                    }
                }
            }
        }
    }
    for msg in &req.messages {
        match &msg.content {
            AnthropicMessageContent::Text(s) => chars = chars.saturating_add(s.len()),
            AnthropicMessageContent::Blocks(blocks) => {
                for b in blocks {
                    match b {
                        AnthropicInputBlock::Text { text } => {
                            chars = chars.saturating_add(text.len());
                        }
                        AnthropicInputBlock::Image { source } => {
                            chars = chars.saturating_add(source.data.len() / 16 + 1024);
                        }
                        AnthropicInputBlock::Thinking { thinking, .. } => {
                            chars = chars.saturating_add(thinking.len());
                        }
                        AnthropicInputBlock::ToolUse { name, input, .. }
                        | AnthropicInputBlock::ServerToolUse { name, input, .. } => {
                            chars = chars.saturating_add(name.len() + input.to_string().len());
                        }
                        AnthropicInputBlock::ToolResult { content, .. } => {
                            if let Some(c) = content {
                                chars = chars.saturating_add(c.to_plain_string().len());
                            }
                        }
                        AnthropicInputBlock::WebSearchToolResult { content, .. }
                        | AnthropicInputBlock::WebFetchToolResult { content, .. } => {
                            chars = chars.saturating_add(content.to_string().len());
                        }
                        AnthropicInputBlock::RedactedThinking { .. }
                        | AnthropicInputBlock::Unknown => {}
                    }
                }
            }
        }
    }
    for tool in &req.tools {
        chars = chars.saturating_add(tool.name.len() + 32);
        if let Some(desc) = &tool.description {
            chars = chars.saturating_add(desc.len());
        }
        if let Some(schema) = &tool.input_schema {
            chars = chars.saturating_add(schema.get().len());
        }
    }
    ((chars / 4) as u64).max(1)
}

// ============================================================================
// Outgoing Anthropic `/v1/messages` Wire Objects
// ============================================================================

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnthropicMessagesResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: Cow<'static, str>,
    pub role: Cow<'static, str>,
    pub model: String,
    pub content: Vec<AnthropicOutputBlock>,
    pub stop_reason: Option<Cow<'static, str>>,
    pub stop_sequence: Option<String>,
    pub usage: AnthropicUsage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnthropicWebSearchResultItem {
    #[serde(rename = "type")]
    pub item_type: Cow<'static, str>,
    pub title: String,
    pub url: String,
    pub encrypted_content: String,
    pub page_age: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicOutputBlock {
    Thinking {
        thinking: String,
        signature: String,
    },
    ServerToolUse {
        id: String,
        name: String,
        input: Value,
    },
    WebSearchToolResult {
        tool_use_id: String,
        content: Vec<AnthropicWebSearchResultItem>,
    },
    WebFetchToolResult {
        tool_use_id: String,
        content: Value,
    },
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct AnthropicServerToolUsage {
    pub web_search_requests: u64,
    #[serde(default)]
    pub web_fetch_requests: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct AnthropicUsage {
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<AnthropicServerToolUsage>,
}

impl From<AcpUsage> for AnthropicUsage {
    fn from(u: AcpUsage) -> Self {
        Self {
            input_tokens: u.input_tokens,
            cache_creation_input_tokens: u.cached_write_tokens,
            cache_read_input_tokens: u.cached_read_tokens,
            output_tokens: u.output_tokens,
            server_tool_use: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct AnthropicCountTokensResponse {
    pub input_tokens: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AnthropicErrorEnvelope {
    #[serde(rename = "type")]
    pub envelope_type: &'static str,
    pub error: AnthropicErrorBody,
}

#[derive(Clone, Debug, Serialize)]
pub struct AnthropicErrorBody {
    #[serde(rename = "type")]
    pub error_type: &'static str,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_base64_images_nested_inside_anthropic_tool_result_blocks() {
        let req: AnthropicMessagesRequest = serde_json::from_value(json!({
            "model": "claude-opus-4-6",
            "max_tokens": 1024,
            "messages": [
                { "role": "user", "content": "Read diagram.png" },
                {
                    "role": "assistant",
                    "content": [
                        { "type": "tool_use", "id": "toolu_1", "name": "Read", "input": { "file_path": "diagram.png" } }
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_1",
                            "content": [
                                { "type": "text", "text": "Image read successfully" },
                                {
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": "image/png",
                                        "data": "iVBORw0KGgoAAAANSUhEUg=="
                                    }
                                }
                            ]
                        }
                    ]
                }
            ]
        }))
        .expect("valid AnthropicMessagesRequest");

        let norm = normalize_anthropic_request(req).expect("normalized turn");
        let last_msg = norm.messages.last().expect("tool result message");
        assert_eq!(last_msg.parts.len(), 2);
        assert!(matches!(
            &last_msg.parts[0],
            CanonicalPart::ToolResult { call_id, output, is_error: false }
                if call_id == "toolu_1" && output == "Image read successfully"
        ));
        assert!(matches!(
            &last_msg.parts[1],
            CanonicalPart::ImageBase64 { media_type, data }
                if media_type == "image/png" && data == "iVBORw0KGgoAAAANSUhEUg=="
        ));
    }

    #[test]
    fn moves_claude_system_prompt_out_of_system_append_and_preserves_prefix_and_compact_hashes() {
        use crate::canonical::AuxiliaryTaskKind;
        use crate::config::ModelId;
        use crate::session_key::{
            WebToolBridgeConfig, compute_routing_plan, hash_conversation_with_reply,
            hash_dialogue_with_reply,
        };

        let model = ModelId::try_new("gemini-4-carbon-high").expect("valid model");
        let web_cfg = WebToolBridgeConfig::default();

        // Turn 1: massive Claude Code system prompt + volatile billing header
        let req1: AnthropicMessagesRequest = serde_json::from_value(json!({
            "model": "gemini-4-carbon-high",
            "max_tokens": 8192,
            "system": [
                { "type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.325; cch=11111;" },
                { "type": "text", "text": format!("You are Claude Code.\n{}", "rule\n".repeat(8_000)) }
            ],
            "tools": [{ "name": "Read", "description": "Read file" }],
            "messages": [{ "role": "user", "content": "Inspect Cargo.toml" }]
        }))
        .expect("valid Turn 1 request");
        let norm1 = normalize_anthropic_request(req1).expect("Turn 1 normalized");
        assert!(
            norm1.system_instructions.is_none(),
            "Claude system prompt must never populate _meta.antigravity.systemPrompt.append"
        );
        assert_eq!(norm1.messages.len(), 2);
        assert!(matches!(
            norm1.messages[0].parts.first(),
            Some(CanonicalPart::Text(t))
                if t.starts_with("<developer_instructions>")
                    && !t.contains("x-anthropic-billing-header")
        ));

        let a1 = CanonicalMessage {
            role: CanonicalRole::Assistant,
            parts: vec![CanonicalPart::Text(
                "Cargo.toml has version 0.1.4.".to_owned(),
            )],
        };
        let turn1_state_hash = hash_conversation_with_reply(
            None,
            &norm1.client_tools,
            web_cfg,
            &model,
            &norm1.messages,
            &a1,
        );
        let turn1_dialogue_hash = hash_dialogue_with_reply(&model, &norm1.messages, &a1);

        // Turn 2: mutated `cch=...` billing header -> must hit Turn 1's warm SessionPool hash
        let req2: AnthropicMessagesRequest = serde_json::from_value(json!({
            "model": "gemini-4-carbon-high",
            "max_tokens": 8192,
            "system": [
                { "type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.325; cch=99999;" },
                { "type": "text", "text": format!("You are Claude Code.\n{}", "rule\n".repeat(8_000)) }
            ],
            "tools": [{ "name": "Read", "description": "Read file" }],
            "messages": [
                { "role": "user", "content": "Inspect Cargo.toml" },
                { "role": "assistant", "content": "Cargo.toml has version 0.1.4." },
                { "role": "user", "content": "Now inspect CHANGELOG.md" }
            ]
        }))
        .expect("valid Turn 2 request");
        let norm2 = normalize_anthropic_request(req2).expect("Turn 2 normalized");
        let plan2 =
            compute_routing_plan(None, &norm2.client_tools, web_cfg, &model, &norm2.messages);
        assert_eq!(plan2.prefix_lookup, Some((turn1_state_hash, 3)));

        // `/compact` turn: replaces `system` with summarization instructions and clears `tools`
        let req_compact: AnthropicMessagesRequest = serde_json::from_value(json!({
            "model": "gemini-4-carbon-high",
            "max_tokens": 8192,
            "system": "You are a helpful AI assistant tasked with summarizing conversations.",
            "tools": [],
            "messages": [
                { "role": "user", "content": "Inspect Cargo.toml" },
                { "role": "assistant", "content": "Cargo.toml has version 0.1.4." },
                { "role": "user", "content": "Your task is to create a detailed summary of the conversation so far." }
            ]
        }))
        .expect("valid compact request");
        let norm_compact = normalize_anthropic_request(req_compact).expect("compact normalized");
        let plan_compact = compute_routing_plan(
            None,
            &norm_compact.client_tools,
            web_cfg,
            &model,
            &norm_compact.messages,
        );
        assert_eq!(
            plan_compact.compaction_lookup,
            Some((turn1_dialogue_hash, 2))
        );

        // Auto Mode classifier detection remains intact
        let req_cls: AnthropicMessagesRequest = serde_json::from_value(json!({
            "model": "gemini-4-carbon-high",
            "max_tokens": 256,
            "system": "You are an auto mode classifier evaluating tool safety.",
            "messages": [{ "role": "user", "content": "<transcript>Bash: cargo test</transcript>" }]
        }))
        .expect("valid classifier request");
        let norm_cls = normalize_anthropic_request(req_cls).expect("classifier normalized");
        assert_eq!(
            norm_cls.auxiliary_task,
            Some(AuxiliaryTaskKind::AutoModeClassifier)
        );
    }
}
