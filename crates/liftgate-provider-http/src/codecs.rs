//! Streaming wire codecs for OpenAI /v1/responses, Anthropic /v1/messages, and Google Gemini streamGenerateContent.

use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;
use liftgate_types::{
    acp::AcpUsage,
    canonical::{CanonicalMessage, CanonicalPart, CanonicalRole, ReasoningEffort},
    config::ProxyUrl,
    provider::ProviderDriverKind,
};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use crate::{
    error::PlatformApiError,
    sse::SseFrame,
    transport::{HttpRequestSpec, HttpSseTransport},
};

/// Outbound tool definition prepared for a stateless Platform HTTP API.
#[derive(Clone, Debug)]
pub struct NativeWireTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    /// When true and targeting OpenAI /v1/responses, emits a native freeform custom tool.
    pub is_custom_freeform: bool,
    pub custom_format: Option<Value>,
}

/// Complete single-step turn request passed to a Platform HTTP driver.
#[derive(Clone, Debug)]
pub struct NativeDriverInput {
    pub provider_id: String,
    pub driver: ProviderDriverKind,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub proxy: Option<ProxyUrl>,
    pub force_direct_proxy: bool,
    pub model: String,
    pub effort: Option<ReasoningEffort>,
    pub max_output_tokens: Option<u64>,
    pub system_blocks: Vec<String>,
    pub messages: Vec<CanonicalMessage>,
    pub tools: Vec<NativeWireTool>,
    pub enable_native_web_search: bool,
    pub prompt_cache_key: Option<String>,
    pub align_cache_breakpoints: bool,
    pub connect_timeout: Duration,
    pub total_timeout: Duration,
}

/// Metadata describing the serialized HTTP wire request before streaming starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PreparedWireMetrics {
    pub wire_body_bytes: usize,
    pub cache_breakpoints_aligned: usize,
}

/// Normalized streaming event emitted by any stateless Platform HTTP driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeStreamEvent {
    ReasoningDelta(String),
    TextDelta(String),
    ToolCall {
        id: String,
        name: String,
        arguments_json: String,
        is_custom: bool,
        thought_signature: Option<String>,
    },
    WebSearch {
        query: String,
    },
    Usage(AcpUsage),
    Completed {
        stop_reason: String,
    },
}

/// Builds the outbound [`HttpRequestSpec`] and wire telemetry metrics for input.
pub fn build_driver_http_request(
    input: &NativeDriverInput,
) -> Result<(HttpRequestSpec, PreparedWireMetrics), PlatformApiError> {
    match input.driver {
        ProviderDriverKind::OpenAiResponses => build_openai_responses_request(input),
        ProviderDriverKind::AnthropicMessages => build_anthropic_messages_request(input),
        ProviderDriverKind::GoogleGemini => build_google_gemini_request(input),
        ProviderDriverKind::AgyAcp
        | ProviderDriverKind::CodexAppServer
        | ProviderDriverKind::GrokAcp => Err(PlatformApiError::Protocol {
            message: format!(
                "stateful driver `{}` cannot be executed via HTTP transport",
                input.driver.as_str()
            ),
        }),
    }
}

/// Executes one streaming HTTP/SSE step and pumps normalized [`NativeStreamEvent`]s into tx.
pub async fn execute_driver_sse_step<T: HttpSseTransport>(
    transport: &T,
    input: &NativeDriverInput,
    tx: &mpsc::Sender<Result<NativeStreamEvent, PlatformApiError>>,
) -> Result<PreparedWireMetrics, PlatformApiError> {
    let (spec, metrics) = build_driver_http_request(input)?;
    let mut sse_stream = transport.open_sse(spec).await?;

    match input.driver {
        ProviderDriverKind::OpenAiResponses => {
            decode_openai_sse(&mut sse_stream.frames_rx, tx).await?;
        }
        ProviderDriverKind::AnthropicMessages => {
            decode_anthropic_sse(&mut sse_stream.frames_rx, tx).await?;
        }
        ProviderDriverKind::GoogleGemini => {
            decode_gemini_sse(&mut sse_stream.frames_rx, tx).await?;
        }
        ProviderDriverKind::AgyAcp
        | ProviderDriverKind::CodexAppServer
        | ProviderDriverKind::GrokAcp => {}
    }

    Ok(metrics)
}

/// Runs a lightweight connectivity & credential probe (GET /models) for lg provider test.
pub async fn probe_http_provider<T: HttpSseTransport>(
    transport: &T,
    driver: ProviderDriverKind,
    endpoint: &str,
    api_key: Option<&str>,
    proxy: Option<ProxyUrl>,
    force_direct_proxy: bool,
) -> Result<usize, PlatformApiError> {
    let base = endpoint.trim().trim_end_matches('/');
    let (url, mut headers) = match driver {
        ProviderDriverKind::OpenAiResponses => {
            let url = if base.ends_with("/v1") {
                format!("{base}/models")
            } else {
                format!("{base}/v1/models")
            };
            let mut h = vec![("Accept".to_owned(), "application/json".to_owned())];
            if let Some(k) = api_key.filter(|s| !s.is_empty()) {
                h.push(("Authorization".to_owned(), format!("Bearer {k}")));
            }
            (url, h)
        }
        ProviderDriverKind::AnthropicMessages => {
            let url = if base.ends_with("/v1") {
                format!("{base}/models")
            } else {
                format!("{base}/v1/models")
            };
            let mut h = vec![
                ("Accept".to_owned(), "application/json".to_owned()),
                ("anthropic-version".to_owned(), "2023-06-01".to_owned()),
            ];
            if let Some(k) = api_key.filter(|s| !s.is_empty()) {
                h.push(("x-api-key".to_owned(), k.to_owned()));
            }
            (url, h)
        }
        ProviderDriverKind::GoogleGemini => {
            let clean = base.trim_end_matches("/v1beta").trim_end_matches("/v1");
            let url = format!("{clean}/v1beta/models");
            let mut h = vec![("Accept".to_owned(), "application/json".to_owned())];
            if let Some(k) = api_key.filter(|s| !s.is_empty()) {
                h.push(("x-goog-api-key".to_owned(), k.to_owned()));
            }
            (url, h)
        }
        ProviderDriverKind::AgyAcp
        | ProviderDriverKind::CodexAppServer
        | ProviderDriverKind::GrokAcp => {
            return Err(PlatformApiError::Protocol {
                message: "stateful driver does not support HTTP probe".to_owned(),
            });
        }
    };
    headers.push(("User-Agent".to_owned(), "liftgate/0.2".to_owned()));

    let resp = transport
        .send_buffered(HttpRequestSpec {
            method: "GET",
            url,
            headers,
            body: None,
            proxy,
            force_direct_proxy,
            connect_timeout: Duration::from_secs(10),
            total_timeout: Duration::from_secs(15),
        })
        .await?;

    if !(200..300).contains(&resp.status) {
        return Err(PlatformApiError::from_http_status(
            resp.status,
            resp.retry_after_ms,
            &resp.body,
        ));
    }

    let count = serde_json::from_str::<Value>(&resp.body)
        .ok()
        .and_then(|v| {
            v.get("data")
                .or_else(|| v.get("models"))
                .and_then(Value::as_array)
                .map(Vec::len)
        })
        .unwrap_or(1);
    Ok(count)
}

// ============================================================================
// 1. OpenAI Platform (/v1/responses)
// ============================================================================

fn build_openai_responses_request(
    input: &NativeDriverInput,
) -> Result<(HttpRequestSpec, PreparedWireMetrics), PlatformApiError> {
    let base = input.endpoint.trim().trim_end_matches('/');
    let url = if base.ends_with("/responses") {
        base.to_owned()
    } else if base.ends_with("/v1") {
        format!("{base}/responses")
    } else {
        format!("{base}/v1/responses")
    };

    let mut items: Vec<Value> = Vec::with_capacity(input.messages.len() + 4);
    let custom_tool_names: std::collections::HashSet<&str> = input
        .tools
        .iter()
        .filter(|t| t.is_custom_freeform)
        .map(|t| t.name.as_str())
        .collect();
    let mut custom_call_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for msg in &input.messages {
        let mut msg_parts: Vec<Value> = Vec::new();
        let role_str = match msg.role {
            CanonicalRole::User => "user",
            CanonicalRole::Assistant => "assistant",
        };
        let text_part_type = match msg.role {
            CanonicalRole::User => "input_text",
            CanonicalRole::Assistant => "output_text",
        };

        let flush_msg_parts = |parts: &mut Vec<Value>, out: &mut Vec<Value>| {
            if !parts.is_empty() {
                out.push(json!({
                    "type": "message",
                    "role": role_str,
                    "content": std::mem::take(parts),
                }));
            }
        };

        for part in &msg.parts {
            match part {
                CanonicalPart::Text(t) => {
                    if !t.is_empty() {
                        msg_parts.push(json!({
                            "type": text_part_type,
                            "text": t,
                        }));
                    }
                }
                CanonicalPart::ImageBase64 { media_type, data } => {
                    msg_parts.push(json!({
                        "type": "input_image",
                        "image_url": format!("data:{media_type};base64,{data}"),
                    }));
                }
                CanonicalPart::Reasoning(_) => {}
                CanonicalPart::ToolCall {
                    call_id,
                    name,
                    arguments,
                    is_custom,
                } => {
                    flush_msg_parts(&mut msg_parts, &mut items);
                    let emit_as_custom = *is_custom || custom_tool_names.contains(name.as_str());
                    if emit_as_custom {
                        custom_call_ids.insert(call_id.clone());
                        items.push(json!({
                            "type": "custom_tool_call",
                            "call_id": call_id,
                            "name": name,
                            "input": arguments,
                        }));
                    } else {
                        items.push(json!({
                            "type": "function_call",
                            "call_id": call_id,
                            "name": name,
                            "arguments": arguments,
                        }));
                    }
                }
                CanonicalPart::ToolResult {
                    call_id,
                    output,
                    is_error: _,
                } => {
                    flush_msg_parts(&mut msg_parts, &mut items);
                    let output_type = if custom_call_ids.contains(call_id.as_str()) {
                        "custom_tool_call_output"
                    } else {
                        "function_call_output"
                    };
                    items.push(json!({
                        "type": output_type,
                        "call_id": call_id,
                        "output": output,
                    }));
                }
            }
        }
        flush_msg_parts(&mut msg_parts, &mut items);
    }

    let mut tools_json: Vec<Value> = Vec::with_capacity(input.tools.len() + 1);
    for t in &input.tools {
        if t.is_custom_freeform {
            let mut obj = json!({
                "type": "custom",
                "name": t.name,
            });
            if let Some(ref desc) = t.description
                && let Some(m) = obj.as_object_mut()
            {
                m.insert("description".to_owned(), Value::String(desc.clone()));
            }
            if let Some(ref fmt) = t.custom_format
                && let Some(m) = obj.as_object_mut()
            {
                m.insert("format".to_owned(), fmt.clone());
            }
            tools_json.push(obj);
        } else {
            let mut obj = json!({
                "type": "function",
                "name": t.name,
                "parameters": t.input_schema,
            });
            if let Some(ref desc) = t.description
                && let Some(m) = obj.as_object_mut()
            {
                m.insert("description".to_owned(), Value::String(desc.clone()));
            }
            tools_json.push(obj);
        }
    }
    if input.enable_native_web_search {
        tools_json.push(json!({ "type": "web_search_preview" }));
    }

    let mut body = Map::new();
    body.insert("model".to_owned(), Value::String(input.model.clone()));
    body.insert("stream".to_owned(), Value::Bool(true));
    body.insert("store".to_owned(), Value::Bool(false));
    body.insert("input".to_owned(), Value::Array(items));

    let joined_sys = input
        .system_blocks
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if !joined_sys.is_empty() {
        body.insert("instructions".to_owned(), Value::String(joined_sys));
    }
    if !tools_json.is_empty() {
        body.insert("parallel_tool_calls".to_owned(), Value::Bool(true));
        body.insert("tools".to_owned(), Value::Array(tools_json));
    }
    if let Some(eff) = input.effort {
        body.insert(
            "reasoning".to_owned(),
            json!({
                "effort": eff.as_suffix(),
                "summary": "auto",
            }),
        );
    }
    if let Some(max_tok) = input.max_output_tokens {
        body.insert(
            "max_output_tokens".to_owned(),
            Value::Number(max_tok.into()),
        );
    }
    if let Some(ref cache_key) = input.prompt_cache_key
        && !cache_key.is_empty()
    {
        body.insert(
            "prompt_cache_key".to_owned(),
            Value::String(cache_key.clone()),
        );
    }

    let raw_bytes =
        serde_json::to_vec(&Value::Object(body)).map_err(|e| PlatformApiError::Protocol {
            message: format!("failed encoding OpenAI Responses payload: {e}"),
        })?;
    let wire_len = raw_bytes.len();

    let mut headers = vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("Accept".to_owned(), "text/event-stream".to_owned()),
    ];
    if let Some(ref key) = input.api_key
        && !key.trim().is_empty()
    {
        headers.push(("Authorization".to_owned(), format!("Bearer {}", key.trim())));
    }

    Ok((
        HttpRequestSpec {
            method: "POST",
            url,
            headers,
            body: Some(Bytes::from(raw_bytes)),
            proxy: input.proxy.clone(),
            force_direct_proxy: input.force_direct_proxy,
            connect_timeout: input.connect_timeout,
            total_timeout: input.total_timeout,
        },
        PreparedWireMetrics {
            wire_body_bytes: wire_len,
            cache_breakpoints_aligned: usize::from(input.prompt_cache_key.is_some()),
        },
    ))
}

#[derive(Default)]
struct PendingOpenAiItem {
    call_id: String,
    name: String,
    is_custom: bool,
    args_buf: String,
    emitted: bool,
}

async fn decode_openai_sse(
    rx: &mut mpsc::Receiver<Result<SseFrame, PlatformApiError>>,
    tx: &mpsc::Sender<Result<NativeStreamEvent, PlatformApiError>>,
) -> Result<(), PlatformApiError> {
    let mut items_by_id: HashMap<String, PendingOpenAiItem> = HashMap::new();
    let mut saw_tool_call = false;
    let mut completed = false;

    while let Some(frame_res) = rx.recv().await {
        let frame = frame_res?;
        let data = frame.data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let ev_type = val
            .get("type")
            .and_then(Value::as_str)
            .or(frame.event.as_deref())
            .unwrap_or("");

        match ev_type {
            "response.output_text.delta" => {
                if let Some(d) = val.get("delta").and_then(Value::as_str)
                    && !d.is_empty()
                {
                    let _ = tx
                        .send(Ok(NativeStreamEvent::TextDelta(d.to_owned())))
                        .await;
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(d) = val.get("delta").and_then(Value::as_str)
                    && !d.is_empty()
                {
                    let _ = tx
                        .send(Ok(NativeStreamEvent::ReasoningDelta(d.to_owned())))
                        .await;
                }
            }
            "response.output_item.added" => {
                if let Some(item) = val.get("item") {
                    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
                    if matches!(item_type, "function_call" | "custom_tool_call") {
                        let item_id = item
                            .get("id")
                            .or_else(|| item.get("call_id"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let call_id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("call_oai")
                            .to_owned();
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        items_by_id.insert(
                            item_id,
                            PendingOpenAiItem {
                                call_id,
                                name,
                                is_custom: item_type == "custom_tool_call",
                                args_buf: String::new(),
                                emitted: false,
                            },
                        );
                    }
                }
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                if let Some(item_id) = val.get("item_id").and_then(Value::as_str)
                    && let Some(delta) = val.get("delta").and_then(Value::as_str)
                    && let Some(entry) = items_by_id.get_mut(item_id)
                {
                    entry.args_buf.push_str(delta);
                }
            }
            "response.output_item.done" => {
                if let Some(item) = val.get("item") {
                    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
                    match item_type {
                        "function_call" | "custom_tool_call" => {
                            let is_custom = item_type == "custom_tool_call";
                            let item_id = item
                                .get("id")
                                .or_else(|| item.get("call_id"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let call_id = item
                                .get("call_id")
                                .or_else(|| item.get("id"))
                                .and_then(Value::as_str)
                                .unwrap_or("call_oai")
                                .to_owned();
                            let name = item
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned();
                            let args = item
                                .get(if is_custom { "input" } else { "arguments" })
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                                .or_else(|| items_by_id.get(item_id).map(|e| e.args_buf.clone()))
                                .unwrap_or_else(|| "{}".to_owned());
                            if let Some(entry) = items_by_id.get_mut(item_id) {
                                entry.emitted = true;
                            }
                            saw_tool_call = true;
                            let _ = tx
                                .send(Ok(NativeStreamEvent::ToolCall {
                                    id: call_id,
                                    name,
                                    arguments_json: args,
                                    is_custom,
                                    thought_signature: None,
                                }))
                                .await;
                        }
                        "web_search_call" => {
                            let query = item
                                .get("action")
                                .and_then(|a| a.get("query"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned();
                            let _ = tx.send(Ok(NativeStreamEvent::WebSearch { query })).await;
                        }
                        _ => {}
                    }
                }
            }
            "response.completed" => {
                if let Some(resp) = val.get("response")
                    && let Some(u) = resp.get("usage")
                {
                    let raw_input_tokens =
                        u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                    let output_tokens = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                    let thought_tokens = u
                        .get("output_tokens_details")
                        .and_then(|d| d.get("reasoning_tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let cached_read_tokens = u
                        .get("input_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let uncached_input_tokens = raw_input_tokens.saturating_sub(cached_read_tokens);
                    let total_tokens = u
                        .get("total_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or_else(|| raw_input_tokens.saturating_add(output_tokens));
                    let _ = tx
                        .send(Ok(NativeStreamEvent::Usage(AcpUsage {
                            total_tokens,
                            input_tokens: uncached_input_tokens,
                            output_tokens,
                            thought_tokens,
                            cached_read_tokens,
                            cached_write_tokens: 0,
                        })))
                        .await;
                }
                let stop_reason = if saw_tool_call {
                    "tool_use"
                } else {
                    "end_turn"
                }
                .to_owned();
                let _ = tx
                    .send(Ok(NativeStreamEvent::Completed { stop_reason }))
                    .await;
                completed = true;
            }
            "response.incomplete" => {
                if let Some(resp) = val.get("response")
                    && let Some(u) = resp.get("usage")
                {
                    let raw_input_tokens =
                        u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                    let output_tokens = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                    let thought_tokens = u
                        .get("output_tokens_details")
                        .and_then(|d| d.get("reasoning_tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let cached_read_tokens = u
                        .get("input_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let uncached_input_tokens = raw_input_tokens.saturating_sub(cached_read_tokens);
                    let total_tokens = u
                        .get("total_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or_else(|| raw_input_tokens.saturating_add(output_tokens));
                    let _ = tx
                        .send(Ok(NativeStreamEvent::Usage(AcpUsage {
                            total_tokens,
                            input_tokens: uncached_input_tokens,
                            output_tokens,
                            thought_tokens,
                            cached_read_tokens,
                            cached_write_tokens: 0,
                        })))
                        .await;
                }
                let stop_reason = if saw_tool_call {
                    "tool_use"
                } else {
                    "max_tokens"
                }
                .to_owned();
                let _ = tx
                    .send(Ok(NativeStreamEvent::Completed { stop_reason }))
                    .await;
                completed = true;
            }
            "response.failed" | "error" => {
                let err_obj = val
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .or_else(|| val.get("error"));
                let code = err_obj
                    .and_then(|e| e.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let msg = err_obj
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("upstream OpenAI stream failed")
                    .to_owned();
                let lower = msg.to_ascii_lowercase();
                if code.contains("rate_limit")
                    || lower.contains("rate limit")
                    || lower.contains("429")
                {
                    return Err(PlatformApiError::RateLimited {
                        status: 429,
                        retry_after_ms: None,
                        message: msg,
                    });
                }
                return Err(PlatformApiError::UpstreamServer {
                    status: 500,
                    message: msg,
                });
            }
            _ => {}
        }
    }

    if let Some(pending) = items_by_id.values().find(|p| !p.emitted) {
        return Err(PlatformApiError::Protocol {
            message: format!(
                "OpenAI SSE stream closed with unfinished {} `{}` ({})",
                if pending.is_custom {
                    "custom_tool_call"
                } else {
                    "function_call"
                },
                pending.name,
                pending.call_id,
            ),
        });
    }

    if !completed {
        return Err(PlatformApiError::Protocol {
            message: "OpenAI SSE stream ended before terminal response.completed event".to_owned(),
        });
    }
    Ok(())
}

// ============================================================================
// 2. Anthropic Platform (/v1/messages) with Prompt Cache Breakpoint Alignment
// ============================================================================

fn build_anthropic_messages_request(
    input: &NativeDriverInput,
) -> Result<(HttpRequestSpec, PreparedWireMetrics), PlatformApiError> {
    let base = input.endpoint.trim().trim_end_matches('/');
    let url = if base.ends_with("/messages") {
        base.to_owned()
    } else if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    };

    let mut breakpoints = 0usize;
    let ephemeral = json!({ "type": "ephemeral" });

    let mut sys_blocks: Vec<Value> = input
        .system_blocks
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| json!({ "type": "text", "text": s }))
        .collect();
    if input.align_cache_breakpoints
        && let Some(last_sys) = sys_blocks.last_mut().and_then(Value::as_object_mut)
    {
        last_sys.insert("cache_control".to_owned(), ephemeral.clone());
        breakpoints += 1;
    }

    let mut tools_json: Vec<Value> = input
        .tools
        .iter()
        .map(|t| {
            let mut obj = json!({
                "name": t.name,
                "input_schema": t.input_schema,
            });
            if let Some(ref d) = t.description
                && let Some(m) = obj.as_object_mut()
            {
                m.insert("description".to_owned(), Value::String(d.clone()));
            }
            obj
        })
        .collect();
    if input.align_cache_breakpoints
        && let Some(last_tool) = tools_json.last_mut().and_then(Value::as_object_mut)
    {
        last_tool.insert("cache_control".to_owned(), ephemeral.clone());
        breakpoints += 1;
    }

    let mut wire_messages: Vec<(CanonicalRole, Vec<Value>)> = Vec::new();
    let push_block =
        |role: CanonicalRole, block: Value, list: &mut Vec<(CanonicalRole, Vec<Value>)>| {
            if let Some((last_role, last_blocks)) = list.last_mut()
                && *last_role == role
            {
                last_blocks.push(block);
            } else {
                list.push((role, vec![block]));
            }
        };

    for msg in &input.messages {
        for part in &msg.parts {
            match part {
                CanonicalPart::Text(t) => {
                    if !t.is_empty() {
                        push_block(
                            msg.role,
                            json!({ "type": "text", "text": t }),
                            &mut wire_messages,
                        );
                    }
                }
                CanonicalPart::ImageBase64 { media_type, data } => {
                    push_block(
                        CanonicalRole::User,
                        json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": media_type,
                                "data": data,
                            }
                        }),
                        &mut wire_messages,
                    );
                }
                CanonicalPart::Reasoning(_) => {}
                CanonicalPart::ToolCall {
                    call_id,
                    name,
                    arguments,
                    is_custom,
                } => {
                    let parsed_input = if *is_custom {
                        json!({ "input": arguments })
                    } else {
                        serde_json::from_str::<Value>(arguments)
                            .ok()
                            .filter(Value::is_object)
                            .unwrap_or_else(|| json!({ "input": arguments }))
                    };
                    push_block(
                        CanonicalRole::Assistant,
                        json!({
                            "type": "tool_use",
                            "id": call_id,
                            "name": name,
                            "input": parsed_input,
                        }),
                        &mut wire_messages,
                    );
                }
                CanonicalPart::ToolResult {
                    call_id,
                    output,
                    is_error,
                } => {
                    push_block(
                        CanonicalRole::User,
                        json!({
                            "type": "tool_result",
                            "tool_use_id": call_id,
                            "content": output,
                            "is_error": is_error,
                        }),
                        &mut wire_messages,
                    );
                }
            }
        }
    }

    if wire_messages
        .first()
        .is_some_and(|(r, _)| *r == CanonicalRole::Assistant)
    {
        wire_messages.insert(
            0,
            (
                CanonicalRole::User,
                vec![json!({ "type": "text", "text": "." })],
            ),
        );
    }
    if input.align_cache_breakpoints
        && let Some((_, blocks)) = wire_messages
            .iter_mut()
            .rev()
            .find(|(r, _)| *r == CanonicalRole::User)
        && let Some(last_blk) = blocks.last_mut().and_then(Value::as_object_mut)
    {
        last_blk.insert("cache_control".to_owned(), ephemeral);
        breakpoints += 1;
    }

    let messages_json: Vec<Value> = wire_messages
        .into_iter()
        .map(|(role, content)| {
            json!({
                "role": match role {
                    CanonicalRole::User => "user",
                    CanonicalRole::Assistant => "assistant",
                },
                "content": content,
            })
        })
        .collect();

    let max_tokens = input.max_output_tokens.unwrap_or(32_768);
    let mut body = Map::new();
    body.insert("model".to_owned(), Value::String(input.model.clone()));
    body.insert("max_tokens".to_owned(), Value::Number(max_tokens.into()));
    body.insert("stream".to_owned(), Value::Bool(true));
    body.insert("messages".to_owned(), Value::Array(messages_json));
    if !sys_blocks.is_empty() {
        body.insert("system".to_owned(), Value::Array(sys_blocks));
    }
    if !tools_json.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools_json));
    }
    if let Some(eff) = input.effort {
        let budget = match eff {
            ReasoningEffort::Low => 2_048u64,
            ReasoningEffort::Medium => 8_192,
            ReasoningEffort::High => 16_384,
            ReasoningEffort::XHigh => 24_576,
        };
        if max_tokens > budget + 1_024 {
            body.insert(
                "thinking".to_owned(),
                json!({
                    "type": "enabled",
                    "budget_tokens": budget,
                }),
            );
        }
    }

    let raw_bytes =
        serde_json::to_vec(&Value::Object(body)).map_err(|e| PlatformApiError::Protocol {
            message: format!("failed encoding Anthropic Messages payload: {e}"),
        })?;
    let wire_len = raw_bytes.len();

    let mut headers = vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("Accept".to_owned(), "text/event-stream".to_owned()),
        ("anthropic-version".to_owned(), "2023-06-01".to_owned()),
    ];
    if let Some(ref key) = input.api_key
        && !key.trim().is_empty()
    {
        headers.push(("x-api-key".to_owned(), key.trim().to_owned()));
    }

    Ok((
        HttpRequestSpec {
            method: "POST",
            url,
            headers,
            body: Some(Bytes::from(raw_bytes)),
            proxy: input.proxy.clone(),
            force_direct_proxy: input.force_direct_proxy,
            connect_timeout: input.connect_timeout,
            total_timeout: input.total_timeout,
        },
        PreparedWireMetrics {
            wire_body_bytes: wire_len,
            cache_breakpoints_aligned: breakpoints,
        },
    ))
}

#[derive(Default)]
struct PendingAnthropicTool {
    id: String,
    name: String,
    json_buf: String,
}

async fn decode_anthropic_sse(
    rx: &mut mpsc::Receiver<Result<SseFrame, PlatformApiError>>,
    tx: &mpsc::Sender<Result<NativeStreamEvent, PlatformApiError>>,
) -> Result<(), PlatformApiError> {
    let mut active_tools: HashMap<usize, PendingAnthropicTool> = HashMap::new();
    let mut prompt_tokens: u64 = 0;
    let mut cached_read: u64 = 0;
    let mut cached_write: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut stop_reason = "end_turn".to_owned();
    let mut completed = false;

    while let Some(frame_res) = rx.recv().await {
        let frame = frame_res?;
        let data = frame.data.trim();
        if data.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let ev_type = val
            .get("type")
            .and_then(Value::as_str)
            .or(frame.event.as_deref())
            .unwrap_or("");

        match ev_type {
            "message_start" => {
                if let Some(u) = val.get("message").and_then(|m| m.get("usage")) {
                    let uncached = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                    let cw = u
                        .get("cache_creation_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let cr = u
                        .get("cache_read_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    prompt_tokens = uncached;
                    cached_read = cr;
                    cached_write = cw;
                    output_tokens = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                }
            }
            "content_block_start" => {
                let idx = val
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
                    .unwrap_or(0);
                if let Some(blk) = val.get("content_block") {
                    let btype = blk.get("type").and_then(Value::as_str).unwrap_or("");
                    if btype == "tool_use" {
                        let id = blk
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("toolu_1")
                            .to_owned();
                        let name = blk
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        active_tools.insert(
                            idx,
                            PendingAnthropicTool {
                                id,
                                name,
                                json_buf: String::new(),
                            },
                        );
                    }
                }
            }
            "content_block_delta" => {
                let idx = val
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
                    .unwrap_or(0);
                if let Some(delta) = val.get("delta") {
                    let dtype = delta.get("type").and_then(Value::as_str).unwrap_or("");
                    match dtype {
                        "text_delta" => {
                            if let Some(t) = delta.get("text").and_then(Value::as_str)
                                && !t.is_empty()
                            {
                                let _ = tx
                                    .send(Ok(NativeStreamEvent::TextDelta(t.to_owned())))
                                    .await;
                            }
                        }
                        "thinking_delta" => {
                            if let Some(t) = delta.get("thinking").and_then(Value::as_str)
                                && !t.is_empty()
                            {
                                let _ = tx
                                    .send(Ok(NativeStreamEvent::ReasoningDelta(t.to_owned())))
                                    .await;
                            }
                        }
                        "input_json_delta" => {
                            if let Some(part) = delta.get("partial_json").and_then(Value::as_str)
                                && let Some(tool) = active_tools.get_mut(&idx)
                            {
                                tool.json_buf.push_str(part);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                let idx = val
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
                    .unwrap_or(0);
                if let Some(tool) = active_tools.remove(&idx) {
                    let args = if tool.json_buf.trim().is_empty() {
                        "{}".to_owned()
                    } else {
                        tool.json_buf
                    };
                    stop_reason = "tool_use".to_owned();
                    let _ = tx
                        .send(Ok(NativeStreamEvent::ToolCall {
                            id: tool.id,
                            name: tool.name,
                            arguments_json: args,
                            is_custom: false,
                            thought_signature: None,
                        }))
                        .await;
                }
            }
            "message_delta" => {
                if let Some(sr) = val
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    stop_reason = sr.to_owned();
                }
                if let Some(out_tok) = val
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64)
                {
                    output_tokens = out_tok;
                }
            }
            "message_stop" => {
                if !active_tools.is_empty() {
                    return Err(PlatformApiError::Protocol {
                        message: "Anthropic message_stop received before content_block_stop"
                            .to_owned(),
                    });
                }
                let total_prompt = prompt_tokens
                    .saturating_add(cached_read)
                    .saturating_add(cached_write);
                let _ = tx
                    .send(Ok(NativeStreamEvent::Usage(AcpUsage {
                        total_tokens: total_prompt.saturating_add(output_tokens),
                        input_tokens: prompt_tokens,
                        output_tokens,
                        thought_tokens: 0,
                        cached_read_tokens: cached_read,
                        cached_write_tokens: cached_write,
                    })))
                    .await;
                let _ = tx
                    .send(Ok(NativeStreamEvent::Completed {
                        stop_reason: stop_reason.clone(),
                    }))
                    .await;
                completed = true;
            }
            "error" => {
                let err_obj = val.get("error");
                let err_type = err_obj
                    .and_then(|e| e.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let msg = err_obj
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Anthropic stream error")
                    .to_owned();
                if matches!(err_type, "rate_limit_error" | "overloaded_error") {
                    return Err(PlatformApiError::RateLimited {
                        status: 429,
                        retry_after_ms: None,
                        message: msg,
                    });
                }
                return Err(PlatformApiError::UpstreamServer {
                    status: 500,
                    message: msg,
                });
            }
            _ => {}
        }
    }

    if !completed {
        return Err(PlatformApiError::Protocol {
            message: "Anthropic SSE stream ended before terminal message_stop event".to_owned(),
        });
    }
    Ok(())
}

// ============================================================================
// 3. Google Gemini API (/v1beta/models/{model}:streamGenerateContent?alt=sse)
// ============================================================================

fn sanitize_gemini_schema(raw: &Value) -> Value {
    match raw {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                if matches!(
                    k.as_str(),
                    "$schema"
                        | "additionalProperties"
                        | "$defs"
                        | "definitions"
                        | "$id"
                        | "default"
                        | "examples"
                        | "strict"
                ) {
                    continue;
                }
                if k == "properties"
                    && let Value::Object(props) = v
                {
                    let mut clean_props = Map::new();
                    for (pk, pv) in props {
                        clean_props.insert(pk.clone(), sanitize_gemini_schema(pv));
                    }
                    out.insert(k.clone(), Value::Object(clean_props));
                    continue;
                }
                if k == "type"
                    && let Value::Array(types) = v
                {
                    let mut non_null: Option<&str> = None;
                    let mut nullable = false;
                    for item in types {
                        if let Some(s) = item.as_str() {
                            if s == "null" {
                                nullable = true;
                            } else if non_null.is_none() {
                                non_null = Some(s);
                            }
                        }
                    }
                    if let Some(primary) = non_null {
                        out.insert("type".to_owned(), Value::String(primary.to_owned()));
                    }
                    if nullable {
                        out.insert("nullable".to_owned(), Value::Bool(true));
                    }
                    continue;
                }
                out.insert(k.clone(), sanitize_gemini_schema(v));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sanitize_gemini_schema).collect()),
        other => other.clone(),
    }
}

fn build_google_gemini_request(
    input: &NativeDriverInput,
) -> Result<(HttpRequestSpec, PreparedWireMetrics), PlatformApiError> {
    let base = input
        .endpoint
        .trim()
        .trim_end_matches('/')
        .trim_end_matches("/v1beta")
        .trim_end_matches("/v1");
    let model_clean = input
        .model
        .trim()
        .strip_prefix("models/")
        .unwrap_or(input.model.trim());
    let url = format!("{base}/v1beta/models/{model_clean}:streamGenerateContent?alt=sse");

    let mut call_names_by_id: HashMap<&str, &str> = HashMap::new();
    for msg in &input.messages {
        for part in &msg.parts {
            if let CanonicalPart::ToolCall { call_id, name, .. } = part {
                call_names_by_id.insert(call_id.as_str(), name.as_str());
            }
        }
    }

    let mut contents: Vec<(&'static str, Vec<Value>)> = Vec::new();
    let push_part =
        |role: &'static str, part: Value, list: &mut Vec<(&'static str, Vec<Value>)>| {
            if let Some((last_role, last_parts)) = list.last_mut()
                && *last_role == role
            {
                last_parts.push(part);
            } else {
                list.push((role, vec![part]));
            }
        };

    for msg in &input.messages {
        let role = match msg.role {
            CanonicalRole::User => "user",
            CanonicalRole::Assistant => "model",
        };
        for part in &msg.parts {
            match part {
                CanonicalPart::Text(t) => {
                    if !t.is_empty() {
                        push_part(role, json!({ "text": t }), &mut contents);
                    }
                }
                CanonicalPart::ImageBase64 { media_type, data } => {
                    push_part(
                        "user",
                        json!({
                            "inlineData": {
                                "mimeType": media_type,
                                "data": data,
                            }
                        }),
                        &mut contents,
                    );
                }
                CanonicalPart::Reasoning(_) => {}
                CanonicalPart::ToolCall {
                    name,
                    arguments,
                    is_custom,
                    ..
                } => {
                    let args_obj = if *is_custom {
                        json!({ "input": arguments })
                    } else {
                        serde_json::from_str::<Value>(arguments)
                            .ok()
                            .filter(Value::is_object)
                            .unwrap_or_else(|| json!({ "input": arguments }))
                    };
                    push_part(
                        "model",
                        json!({
                            "functionCall": {
                                "name": name,
                                "args": args_obj,
                            }
                        }),
                        &mut contents,
                    );
                }
                CanonicalPart::ToolResult {
                    call_id,
                    output,
                    is_error,
                } => {
                    let fn_name = call_names_by_id
                        .get(call_id.as_str())
                        .copied()
                        .unwrap_or("tool");
                    push_part(
                        "user",
                        json!({
                            "functionResponse": {
                                "name": fn_name,
                                "response": {
                                    "result": output,
                                    "is_error": is_error,
                                }
                            }
                        }),
                        &mut contents,
                    );
                }
            }
        }
    }

    let contents_json: Vec<Value> = contents
        .into_iter()
        .map(|(role, parts)| json!({ "role": role, "parts": parts }))
        .collect();

    let mut body = Map::new();
    body.insert("contents".to_owned(), Value::Array(contents_json));

    let joined_sys = input
        .system_blocks
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if !joined_sys.is_empty() {
        body.insert(
            "systemInstruction".to_owned(),
            json!({
                "parts": [{ "text": joined_sys }]
            }),
        );
    }

    if !input.tools.is_empty() {
        let decls: Vec<Value> = input
            .tools
            .iter()
            .map(|t| {
                let mut d = json!({
                    "name": t.name,
                    "parameters": sanitize_gemini_schema(&t.input_schema),
                });
                if let Some(ref desc) = t.description
                    && let Some(m) = d.as_object_mut()
                {
                    m.insert("description".to_owned(), Value::String(desc.clone()));
                }
                d
            })
            .collect();
        body.insert(
            "tools".to_owned(),
            json!([{ "functionDeclarations": decls }]),
        );
    }

    let mut gen_cfg = Map::new();
    if let Some(max_tok) = input.max_output_tokens {
        gen_cfg.insert("maxOutputTokens".to_owned(), Value::Number(max_tok.into()));
    }
    if input.effort.is_some() {
        gen_cfg.insert(
            "thinkingConfig".to_owned(),
            json!({ "includeThoughts": true }),
        );
    }
    if !gen_cfg.is_empty() {
        body.insert("generationConfig".to_owned(), Value::Object(gen_cfg));
    }

    let raw_bytes =
        serde_json::to_vec(&Value::Object(body)).map_err(|e| PlatformApiError::Protocol {
            message: format!("failed encoding Google Gemini payload: {e}"),
        })?;
    let wire_len = raw_bytes.len();

    let mut headers = vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("Accept".to_owned(), "text/event-stream".to_owned()),
    ];
    if let Some(ref key) = input.api_key
        && !key.trim().is_empty()
    {
        headers.push(("x-goog-api-key".to_owned(), key.trim().to_owned()));
    }

    Ok((
        HttpRequestSpec {
            method: "POST",
            url,
            headers,
            body: Some(Bytes::from(raw_bytes)),
            proxy: input.proxy.clone(),
            force_direct_proxy: input.force_direct_proxy,
            connect_timeout: input.connect_timeout,
            total_timeout: input.total_timeout,
        },
        PreparedWireMetrics {
            wire_body_bytes: wire_len,
            cache_breakpoints_aligned: 0,
        },
    ))
}

async fn decode_gemini_sse(
    rx: &mut mpsc::Receiver<Result<SseFrame, PlatformApiError>>,
    tx: &mpsc::Sender<Result<NativeStreamEvent, PlatformApiError>>,
) -> Result<(), PlatformApiError> {
    let mut call_seq: u32 = 0;
    let mut saw_tool_call = false;
    let mut terminal_stop_reason: Option<String> = None;
    let mut latest_usage: Option<AcpUsage> = None;

    while let Some(frame_res) = rx.recv().await {
        let frame = frame_res?;
        let data = frame.data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            if terminal_stop_reason.is_none() && (saw_tool_call || latest_usage.is_some()) {
                terminal_stop_reason = Some(
                    if saw_tool_call {
                        "tool_use"
                    } else {
                        "end_turn"
                    }
                    .to_owned(),
                );
            }
            continue;
        }
        let val = serde_json::from_str::<Value>(data).map_err(|e| PlatformApiError::Protocol {
            message: format!("malformed Gemini SSE JSON frame: {e}"),
        })?;
        if let Some(err_obj) = val.get("error") {
            let code = err_obj.get("code").and_then(Value::as_u64).unwrap_or(500);
            let msg = err_obj
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Gemini stream error")
                .to_owned();
            if code == 429 {
                return Err(PlatformApiError::RateLimited {
                    status: 429,
                    retry_after_ms: None,
                    message: msg,
                });
            }
            return Err(PlatformApiError::UpstreamServer {
                status: u16::try_from(code).unwrap_or(500),
                message: msg,
            });
        }

        if let Some(cand) = val
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        {
            if let Some(parts) = cand
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(Value::as_array)
            {
                for part in parts {
                    let is_thought = part
                        .get("thought")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if let Some(text) = part.get("text").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        if is_thought {
                            let _ = tx
                                .send(Ok(NativeStreamEvent::ReasoningDelta(text.to_owned())))
                                .await;
                        } else {
                            let _ = tx
                                .send(Ok(NativeStreamEvent::TextDelta(text.to_owned())))
                                .await;
                        }
                    }
                    if let Some(fc) = part.get("functionCall") {
                        call_seq += 1;
                        saw_tool_call = true;
                        let name = fc
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let args_json = fc
                            .get("args")
                            .map(Value::to_string)
                            .unwrap_or_else(|| "{}".to_owned());
                        let sig = part
                            .get("thoughtSignature")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        let _ = tx
                            .send(Ok(NativeStreamEvent::ToolCall {
                                id: format!("call_gem_{call_seq}"),
                                name,
                                arguments_json: args_json,
                                is_custom: false,
                                thought_signature: sig,
                            }))
                            .await;
                    }
                }
            }
            if let Some(fr) = cand.get("finishReason").and_then(Value::as_str)
                && !fr.is_empty()
            {
                let mapped = if saw_tool_call {
                    "tool_use"
                } else if fr == "MAX_TOKENS" {
                    "max_tokens"
                } else {
                    "end_turn"
                };
                terminal_stop_reason = Some(mapped.to_owned());
            }
        }

        if let Some(um) = val.get("usageMetadata") {
            let prompt = um
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let candidates = um
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let thoughts = um
                .get("thoughtsTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cached = um
                .get("cachedContentTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let uncached_prompt = prompt.saturating_sub(cached);
            let combined_output = candidates.saturating_add(thoughts);
            let total = um
                .get("totalTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| prompt.saturating_add(combined_output));
            latest_usage = Some(AcpUsage {
                total_tokens: total,
                input_tokens: uncached_prompt,
                output_tokens: combined_output,
                thought_tokens: thoughts,
                cached_read_tokens: cached,
                cached_write_tokens: 0,
            });
        }
    }

    let Some(stop_reason) = terminal_stop_reason else {
        return Err(PlatformApiError::Protocol {
            message: "Gemini SSE stream ended before terminal finishReason".to_owned(),
        });
    };

    if let Some(u) = latest_usage {
        let _ = tx.send(Ok(NativeStreamEvent::Usage(u))).await;
    }
    let _ = tx
        .send(Ok(NativeStreamEvent::Completed { stop_reason }))
        .await;
    Ok(())
}
