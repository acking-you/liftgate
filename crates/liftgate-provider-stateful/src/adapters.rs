//! Stateful JSON-RPC stdio dialect adapters for codex app-server and grok acp.

use std::future::Future;
use std::path::PathBuf;

use liftgate_types::{
    acp::{ACP_PROTOCOL_VERSION, AcpUsage, McpServerConfig},
    canonical::ReasoningEffort,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::stdio_rpc::{StatefulRpcError, StdioRpcPeer, StdioSpawnConfig};

/// Closed set of non-agy stateful stdio session dialects handled by liftgate-provider-stateful.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatefulDialect {
    /// OpenAI codex app-server JSON-RPC 2.0 over stdio.
    CodexAppServer,
    /// Standard ACP JSON-RPC 2.0 over stdio (grok acp and ACP-compliant CLI agents).
    GrokAcp,
}

/// Client tool declaration bridged into a stateful stdio session (`dynamicTools` or ACP tool catalog).
#[derive(Clone, Debug)]
pub struct StatefulClientToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub is_custom_freeform: bool,
}

/// Disposition chosen by the gateway when a stateful peer invokes a bridged tool in-band.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DynamicToolDisposition {
    /// Reply immediately on the open JSON-RPC request (`0-RTT` search / preflight / web_fetch) and keep the turn running.
    LocalReply { text: String, is_error: bool },
    /// Emit [`StatefulStreamEvent::ToolCall`] to the downstream client and conclude this HTTP turn with `stop_reason = "tool_use"`.
    SuspendToClient,
}

/// Base64-encoded image attachment forwarded into a stateful stdio session turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatefulInputImage {
    pub media_type: String,
    pub data: String,
}

/// Normalized turn request passed into a stateful stdio dialect adapter.
#[derive(Clone, Debug)]
pub struct StatefulTurnInput {
    pub dialect: StatefulDialect,
    pub model: String,
    pub effort: Option<ReasoningEffort>,
    pub system_instructions: Option<String>,
    pub prompt_text: String,
    pub images: Vec<StatefulInputImage>,
    pub cwd: PathBuf,
    pub mcp_servers: Vec<McpServerConfig>,
    pub dynamic_tools: Vec<StatefulClientToolSpec>,
}

/// Streaming event emitted by a stateful stdio session turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatefulStreamEvent {
    ReasoningDelta(String),
    TextDelta(String),
    ToolCall {
        id: String,
        name: String,
        arguments_json: String,
        is_custom: bool,
    },
    Usage(AcpUsage),
    Completed {
        stop_reason: String,
    },
}

impl StatefulDialect {
    #[must_use]
    pub fn default_args(self) -> Vec<String> {
        match self {
            Self::CodexAppServer => vec!["app-server".to_owned()],
            Self::GrokAcp => vec!["acp".to_owned()],
        }
    }
}

/// Executes a single turn over a spawned stateful stdio peer, streaming events to events_tx.
pub async fn run_turn_on_spawned_peer(
    spawn_cfg: &StdioSpawnConfig,
    input: &StatefulTurnInput,
    events_tx: &mpsc::Sender<Result<StatefulStreamEvent, StatefulRpcError>>,
) -> Result<(), StatefulRpcError> {
    run_turn_on_spawned_peer_with_handler(spawn_cfg, input, events_tx, |_, _| async {
        DynamicToolDisposition::SuspendToClient
    })
    .await
}

/// Executes a single turn over a spawned stateful stdio peer with a custom in-band tool handler.
pub async fn run_turn_on_spawned_peer_with_handler<F, Fut>(
    spawn_cfg: &StdioSpawnConfig,
    input: &StatefulTurnInput,
    events_tx: &mpsc::Sender<Result<StatefulStreamEvent, StatefulRpcError>>,
    mut on_tool_call: F,
) -> Result<(), StatefulRpcError>
where
    F: FnMut(String, Value) -> Fut + Send,
    Fut: Future<Output = DynamicToolDisposition> + Send,
{
    let peer = StdioRpcPeer::spawn(spawn_cfg)?;
    run_turn_on_peer_with_handler(&peer, input, events_tx, &mut on_tool_call).await
}

/// Executes a single turn over an already-connected [`StdioRpcPeer`] (enabling deterministic duplex unit tests).
pub async fn run_turn_on_peer(
    peer: &StdioRpcPeer,
    input: &StatefulTurnInput,
    events_tx: &mpsc::Sender<Result<StatefulStreamEvent, StatefulRpcError>>,
) -> Result<(), StatefulRpcError> {
    run_turn_on_peer_with_handler(peer, input, events_tx, |_, _| async {
        DynamicToolDisposition::SuspendToClient
    })
    .await
}

/// Executes a single turn over an already-connected [`StdioRpcPeer`] with an in-band tool-call handler.
pub async fn run_turn_on_peer_with_handler<F, Fut>(
    peer: &StdioRpcPeer,
    input: &StatefulTurnInput,
    events_tx: &mpsc::Sender<Result<StatefulStreamEvent, StatefulRpcError>>,
    mut on_tool_call: F,
) -> Result<(), StatefulRpcError>
where
    F: FnMut(String, Value) -> Fut + Send,
    Fut: Future<Output = DynamicToolDisposition> + Send,
{
    match input.dialect {
        StatefulDialect::CodexAppServer => {
            run_codex_app_server_turn(peer, input, events_tx, &mut on_tool_call).await
        }
        StatefulDialect::GrokAcp => {
            run_standard_acp_turn(peer, input, events_tx, &mut on_tool_call).await
        }
    }
}

fn build_codex_mcp_servers_config(mcp_servers: &[McpServerConfig]) -> Option<Value> {
    if mcp_servers.is_empty() {
        return None;
    }
    let mut servers = serde_json::Map::new();
    for srv in mcp_servers {
        if let Some(ref url) = srv.url {
            servers.insert(srv.name.clone(), json!({ "url": url }));
        } else if let Some(ref command) = srv.command {
            servers.insert(
                srv.name.clone(),
                json!({
                    "command": command,
                    "args": srv.args,
                }),
            );
        }
    }
    (!servers.is_empty()).then(|| json!({ "mcp_servers": servers }))
}

fn serialize_tool_arguments(args_val: &Value, is_custom: bool) -> String {
    if let Some(s) = args_val.as_str() {
        return s.to_owned();
    }
    if is_custom
        && let Some(obj) = args_val.as_object()
        && let Some(raw) = obj
            .get("input")
            .or_else(|| obj.get("code"))
            .or_else(|| obj.get("patch"))
            .and_then(Value::as_str)
    {
        return raw.to_owned();
    }
    args_val.to_string()
}

async fn run_codex_app_server_turn<F, Fut>(
    peer: &StdioRpcPeer,
    input: &StatefulTurnInput,
    events_tx: &mpsc::Sender<Result<StatefulStreamEvent, StatefulRpcError>>,
    on_tool_call: &mut F,
) -> Result<(), StatefulRpcError>
where
    F: FnMut(String, Value) -> Fut + Send,
    Fut: Future<Output = DynamicToolDisposition> + Send,
{
    let _: Value = peer
        .request(
            "initialize",
            &json!({
                "clientInfo": {
                    "name": "liftgate",
                    "title": "Liftgate Multi-Provider Gateway",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {
                    "experimentalApi": true,
                },
            }),
        )
        .await?;
    peer.notify("initialized", &json!({})).await?;

    let custom_names: std::collections::HashSet<&str> = input
        .dynamic_tools
        .iter()
        .filter(|t| t.is_custom_freeform)
        .map(|t| t.name.as_str())
        .collect();

    let mut thread_params = json!({
        "model": input.model,
        "cwd": input.cwd.to_string_lossy(),
        "approvalPolicy": "never",
        "ephemeral": true,
    });
    if let Some(obj) = thread_params.as_object_mut() {
        if let Some(sys) = input
            .system_instructions
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            obj.insert(
                "developerInstructions".to_owned(),
                Value::String(sys.to_owned()),
            );
        }
        if let Some(cfg_val) = build_codex_mcp_servers_config(&input.mcp_servers) {
            obj.insert("config".to_owned(), cfg_val);
        }
        if !input.dynamic_tools.is_empty() {
            let specs: Vec<Value> = input
                .dynamic_tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema,
                    })
                })
                .collect();
            obj.insert("dynamicTools".to_owned(), Value::Array(specs));
        }
    }

    let thread_res: Value = peer.request("thread/start", &thread_params).await?;
    let thread_id = thread_res
        .get("thread")
        .and_then(|t| t.get("id"))
        .or_else(|| thread_res.get("threadId"))
        .and_then(Value::as_str)
        .unwrap_or("thread_default")
        .to_owned();

    let mut input_items: Vec<Value> = Vec::with_capacity(1 + input.images.len());
    if !input.prompt_text.is_empty() || input.images.is_empty() {
        input_items.push(json!({
            "type": "text",
            "text": input.prompt_text,
        }));
    }
    for img in &input.images {
        input_items.push(json!({
            "type": "image",
            "url": format!("data:{};base64,{}", img.media_type, img.data),
        }));
    }
    let mut turn_params = json!({
        "threadId": thread_id,
        "input": input_items,
    });
    if let Some(eff) = input.effort
        && let Some(obj) = turn_params.as_object_mut()
    {
        obj.insert(
            "effort".to_owned(),
            Value::String(eff.as_suffix().to_owned()),
        );
    }

    let _: Value = peer.request("turn/start", &turn_params).await?;

    while let Some(msg) = peer.next_inbound().await {
        if let Some(req_id) = msg.id {
            if msg.method == "item/tool/call" {
                let call_id = msg
                    .params
                    .get("callId")
                    .or_else(|| msg.params.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("call_codex_dyn")
                    .to_owned();
                let tool_name = msg
                    .params
                    .get("tool")
                    .or_else(|| msg.params.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let args_val = msg
                    .params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match on_tool_call(tool_name.clone(), args_val.clone()).await {
                    DynamicToolDisposition::LocalReply { text, is_error } => {
                        let _ = peer
                            .respond(
                                req_id,
                                &json!({
                                    "contentItems": [{
                                        "type": "inputText",
                                        "text": text,
                                    }],
                                    "success": !is_error,
                                }),
                            )
                            .await;
                        continue;
                    }
                    DynamicToolDisposition::SuspendToClient => {
                        let is_custom = custom_names.contains(tool_name.as_str());
                        let args_json = serialize_tool_arguments(&args_val, is_custom);
                        let _ = events_tx
                            .send(Ok(StatefulStreamEvent::ToolCall {
                                id: call_id,
                                name: tool_name,
                                arguments_json: args_json,
                                is_custom,
                            }))
                            .await;
                        let _ = events_tx
                            .send(Ok(StatefulStreamEvent::Completed {
                                stop_reason: "tool_use".to_owned(),
                            }))
                            .await;
                        let _ = peer
                            .notify("turn/interrupt", &json!({ "threadId": thread_id }))
                            .await;
                        return Ok(());
                    }
                }
            }
            let _ = peer.respond(req_id, &json!({ "decision": "accept" })).await;
            continue;
        }
        match msg.method.as_str() {
            "item/agentMessage/delta" => {
                if let Some(delta) = msg.params.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    let _ = events_tx
                        .send(Ok(StatefulStreamEvent::TextDelta(delta.to_owned())))
                        .await;
                }
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                if let Some(delta) = msg.params.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    let _ = events_tx
                        .send(Ok(StatefulStreamEvent::ReasoningDelta(delta.to_owned())))
                        .await;
                }
            }
            "thread/tokenUsage/updated" | "turn/tokenUsage/updated" => {
                if let Some(usage_obj) = msg
                    .params
                    .get("tokenUsage")
                    .and_then(|u| u.get("last"))
                    .or_else(|| msg.params.get("usage"))
                {
                    let raw_input_tokens = usage_obj
                        .get("inputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let output_tokens = usage_obj
                        .get("outputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let thought_tokens = usage_obj
                        .get("reasoningOutputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let cached_read_tokens = usage_obj
                        .get("cachedInputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let uncached_input_tokens = raw_input_tokens.saturating_sub(cached_read_tokens);
                    let _ = events_tx
                        .send(Ok(StatefulStreamEvent::Usage(AcpUsage {
                            total_tokens: raw_input_tokens.saturating_add(output_tokens),
                            input_tokens: uncached_input_tokens,
                            output_tokens,
                            thought_tokens,
                            cached_read_tokens,
                            cached_write_tokens: 0,
                        })))
                        .await;
                }
            }
            "turn/completed" => {
                if let Some(err_obj) = msg
                    .params
                    .get("turn")
                    .and_then(|t| t.get("error"))
                    .filter(|v| !v.is_null())
                {
                    let message = err_obj
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("codex app-server turn failed")
                        .to_owned();
                    let lower = message.to_ascii_lowercase();
                    if lower.contains("429")
                        || lower.contains("rate limit")
                        || lower.contains("quota")
                    {
                        return Err(StatefulRpcError::RateLimited { code: 429, message });
                    }
                    return Err(StatefulRpcError::Rpc {
                        code: -32000,
                        message,
                    });
                }
                let stop_reason = msg
                    .params
                    .get("turn")
                    .and_then(|t| t.get("status"))
                    .and_then(Value::as_str)
                    .map(|s| if s == "completed" { "end_turn" } else { s })
                    .unwrap_or("end_turn")
                    .to_owned();
                let _ = events_tx
                    .send(Ok(StatefulStreamEvent::Completed { stop_reason }))
                    .await;
                return Ok(());
            }
            _ => {}
        }
    }

    Err(StatefulRpcError::ChannelClosed)
}

async fn run_standard_acp_turn<F, Fut>(
    peer: &StdioRpcPeer,
    input: &StatefulTurnInput,
    events_tx: &mpsc::Sender<Result<StatefulStreamEvent, StatefulRpcError>>,
    on_tool_call: &mut F,
) -> Result<(), StatefulRpcError>
where
    F: FnMut(String, Value) -> Fut + Send,
    Fut: Future<Output = DynamicToolDisposition> + Send,
{
    let _: Value = peer
        .request(
            "initialize",
            &json!({
                "protocolVersion": ACP_PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false
                },
                "clientInfo": {
                    "name": "liftgate",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )
        .await?;

    let custom_names: std::collections::HashSet<&str> = input
        .dynamic_tools
        .iter()
        .filter(|t| t.is_custom_freeform)
        .map(|t| t.name.as_str())
        .collect();

    let mut session_new_params = json!({
        "cwd": input.cwd.to_string_lossy(),
        "mcpServers": input.mcp_servers,
    });
    if !input.dynamic_tools.is_empty()
        && input.mcp_servers.is_empty()
        && let Some(obj) = session_new_params.as_object_mut()
    {
        let tools_arr: Vec<Value> = input
            .dynamic_tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                })
            })
            .collect();
        obj.insert("tools".to_owned(), Value::Array(tools_arr));
    }
    let new_res: Value = peer.request("session/new", &session_new_params).await?;
    let session_id = new_res
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("sess_default")
        .to_owned();

    if !input.model.trim().is_empty() {
        peer.request::<_, Value>(
            "session/set_model",
            &json!({
                "sessionId": session_id,
                "modelId": input.model,
            }),
        )
        .await?;
    }

    let full_prompt = match input
        .system_instructions
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(sys) => format!(
            "<developer_instructions>\n{sys}\n</developer_instructions>\n\n{}",
            input.prompt_text
        ),
        None => input.prompt_text.clone(),
    };

    let mut prompt_blocks: Vec<Value> = Vec::with_capacity(1 + input.images.len());
    if !full_prompt.is_empty() || input.images.is_empty() {
        prompt_blocks.push(json!({
            "type": "text",
            "text": full_prompt,
        }));
    }
    for img in &input.images {
        prompt_blocks.push(json!({
            "type": "image",
            "data": img.data,
            "mimeType": img.media_type,
        }));
    }
    let prompt_params = json!({
        "sessionId": session_id,
        "prompt": prompt_blocks,
    });
    let prompt_fut = peer.request::<_, Value>("session/prompt", &prompt_params);
    tokio::pin!(prompt_fut);

    loop {
        tokio::select! {
            biased;
            maybe_msg = peer.next_inbound() => {
                let Some(msg) = maybe_msg else {
                    let prompt_res = prompt_fut.await?;
                    let stop_reason = prompt_res
                        .get("stopReason")
                        .and_then(Value::as_str)
                        .unwrap_or("end_turn")
                        .to_owned();
                    let _ = events_tx
                        .send(Ok(StatefulStreamEvent::Completed { stop_reason }))
                        .await;
                    return Ok(());
                };
                if let Some(req_id) = msg.id {
                    if matches!(
                        msg.method.as_str(),
                        "tool/call" | "session/tool_call" | "item/tool/call"
                    ) {
                        let call_id = msg
                            .params
                            .get("callId")
                            .or_else(|| msg.params.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("call_acp_dyn")
                            .to_owned();
                        let tool_name = msg
                            .params
                            .get("tool")
                            .or_else(|| msg.params.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let args_val = msg
                            .params
                            .get("arguments")
                            .or_else(|| msg.params.get("input"))
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        match on_tool_call(tool_name.clone(), args_val.clone()).await {
                            DynamicToolDisposition::LocalReply { text, is_error } => {
                                let _ = peer
                                    .respond(
                                        req_id,
                                        &json!({
                                            "content": [{ "type": "text", "text": text }],
                                            "isError": is_error,
                                        }),
                                    )
                                    .await;
                                continue;
                            }
                            DynamicToolDisposition::SuspendToClient => {
                                let is_custom = custom_names.contains(tool_name.as_str());
                                let args_json = serialize_tool_arguments(&args_val, is_custom);
                                let _ = events_tx
                                    .send(Ok(StatefulStreamEvent::ToolCall {
                                        id: call_id,
                                        name: tool_name,
                                        arguments_json: args_json,
                                        is_custom,
                                    }))
                                    .await;
                                let _ = events_tx
                                    .send(Ok(StatefulStreamEvent::Completed {
                                        stop_reason: "tool_use".to_owned(),
                                    }))
                                    .await;
                                let _ = peer
                                    .notify(
                                        "session/cancel",
                                        &json!({ "sessionId": session_id }),
                                    )
                                    .await;
                                return Ok(());
                            }
                        }
                    }
                    let _ = peer
                        .respond(
                            req_id,
                            &json!({ "outcome": { "outcome": "selected", "optionId": "allow_once" } }),
                        )
                        .await;
                    continue;
                }
                forward_acp_notification(&msg.method, &msg.params, events_tx).await;
            }
            res = &mut prompt_fut => {
                let prompt_res = res?;
                while let Some(msg) = peer.try_next_inbound().await {
                    forward_acp_notification(&msg.method, &msg.params, events_tx).await;
                }
                if let Some(usage_val) = prompt_res.get("usage")
                    && let Ok(usage) = serde_json::from_value::<AcpUsage>(usage_val.clone())
                {
                    let _ = events_tx.send(Ok(StatefulStreamEvent::Usage(usage))).await;
                }
                let stop_reason = prompt_res
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or("end_turn")
                    .to_owned();
                let _ = events_tx
                    .send(Ok(StatefulStreamEvent::Completed { stop_reason }))
                    .await;
                return Ok(());
            }
        }
    }
}

async fn forward_acp_notification(
    method: &str,
    params: &Value,
    events_tx: &mpsc::Sender<Result<StatefulStreamEvent, StatefulRpcError>>,
) {
    if method != "session/update" {
        return;
    }
    let Some(upd) = params.get("update") else {
        return;
    };
    let kind = upd
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "agent_message_chunk" => {
            if let Some(t) = upd
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
                && !t.is_empty()
            {
                let _ = events_tx
                    .send(Ok(StatefulStreamEvent::TextDelta(t.to_owned())))
                    .await;
            }
        }
        "agent_thought_chunk" => {
            if let Some(t) = upd
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
                && !t.is_empty()
            {
                let _ = events_tx
                    .send(Ok(StatefulStreamEvent::ReasoningDelta(t.to_owned())))
                    .await;
            }
        }
        _ => {}
    }
}
