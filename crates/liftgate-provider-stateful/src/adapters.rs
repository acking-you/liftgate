//! Stateful JSON-RPC stdio dialect adapters for codex app-server and grok acp.

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

/// Normalized turn request passed into a stateful stdio dialect adapter.
#[derive(Clone, Debug)]
pub struct StatefulTurnInput {
    pub dialect: StatefulDialect,
    pub model: String,
    pub effort: Option<ReasoningEffort>,
    pub system_instructions: Option<String>,
    pub prompt_text: String,
    pub cwd: PathBuf,
    pub mcp_servers: Vec<McpServerConfig>,
}

/// Streaming event emitted by a stateful stdio session turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatefulStreamEvent {
    ReasoningDelta(String),
    TextDelta(String),
    Usage(AcpUsage),
    Completed { stop_reason: String },
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
    let peer = StdioRpcPeer::spawn(spawn_cfg)?;
    run_turn_on_peer(&peer, input, events_tx).await
}

/// Executes a single turn over an already-connected [`StdioRpcPeer`] (enabling deterministic duplex unit tests).
pub async fn run_turn_on_peer(
    peer: &StdioRpcPeer,
    input: &StatefulTurnInput,
    events_tx: &mpsc::Sender<Result<StatefulStreamEvent, StatefulRpcError>>,
) -> Result<(), StatefulRpcError> {
    match input.dialect {
        StatefulDialect::CodexAppServer => run_codex_app_server_turn(peer, input, events_tx).await,
        StatefulDialect::GrokAcp => run_standard_acp_turn(peer, input, events_tx).await,
    }
}

async fn run_codex_app_server_turn(
    peer: &StdioRpcPeer,
    input: &StatefulTurnInput,
    events_tx: &mpsc::Sender<Result<StatefulStreamEvent, StatefulRpcError>>,
) -> Result<(), StatefulRpcError> {
    let _: Value = peer
        .request(
            "initialize",
            &json!({
                "clientInfo": {
                    "name": "liftgate",
                    "title": "Liftgate Multi-Provider Gateway",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
        )
        .await?;
    peer.notify("initialized", &json!({})).await?;

    let mut thread_params = json!({
        "model": input.model,
        "cwd": input.cwd.to_string_lossy(),
        "approvalPolicy": "never",
        "ephemeral": true,
    });
    if let Some(sys) = input
        .system_instructions
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && let Some(obj) = thread_params.as_object_mut()
    {
        obj.insert(
            "developerInstructions".to_owned(),
            Value::String(sys.to_owned()),
        );
    }

    let thread_res: Value = peer.request("thread/start", &thread_params).await?;
    let thread_id = thread_res
        .get("thread")
        .and_then(|t| t.get("id"))
        .or_else(|| thread_res.get("threadId"))
        .and_then(Value::as_str)
        .unwrap_or("thread_default")
        .to_owned();

    let mut turn_params = json!({
        "threadId": thread_id,
        "input": [{
            "type": "text",
            "text": input.prompt_text,
        }],
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
                    let input_tokens = usage_obj
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
                    let _ = events_tx
                        .send(Ok(StatefulStreamEvent::Usage(AcpUsage {
                            total_tokens: input_tokens.saturating_add(output_tokens),
                            input_tokens,
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

async fn run_standard_acp_turn(
    peer: &StdioRpcPeer,
    input: &StatefulTurnInput,
    events_tx: &mpsc::Sender<Result<StatefulStreamEvent, StatefulRpcError>>,
) -> Result<(), StatefulRpcError> {
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

    let new_res: Value = peer
        .request(
            "session/new",
            &json!({
                "cwd": input.cwd.to_string_lossy(),
                "mcpServers": input.mcp_servers,
            }),
        )
        .await?;
    let session_id = new_res
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("sess_default")
        .to_owned();

    let _ = peer
        .request::<_, Value>(
            "session/set_model",
            &json!({
                "sessionId": session_id,
                "modelId": input.model,
            }),
        )
        .await;

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

    let prompt_params = json!({
        "sessionId": session_id,
        "prompt": [{
            "type": "text",
            "text": full_prompt,
        }]
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
