use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use liftgate_server::{
    acp_client::AcpBackend,
    error::AcpError,
    routes::{ADMIN_SHUTDOWN_HEADER, AdminShutdownHandle, AppState, EmbeddedUiAsset, build_router},
    session_pool::SessionPool,
    telemetry::{CacheTelemetryConfig, TelemetryHub},
    turn_runner::GatewayRuntimeConfig,
};
use liftgate_types::{
    acp::{
        AcpChunkContent, AcpPromptBlock, AcpUsage, AvailableModel, McpServerConfig, ModelsCatalog,
        ModesCatalog, PromptTurnResult, SessionNewResult, SessionUpdatePayload,
    },
    config::{
        AcpModeId, CacheHitThresholdBps, GatewayLimits, LowCacheRecordLimit, ModelId, SessionId,
    },
    test_support::{mock_acp_usage, mock_available_models},
};
use serde_json::{Value, json, value::RawValue};
use tokio::sync::mpsc;
use tower::ServiceExt;

/// Scripted behavior for a single mock ACP turn.
enum MockScript {
    Immediate {
        thoughts: Vec<String>,
        chunks_template: Vec<String>,
    },
    ImmediateWithExactUsage {
        usage: AcpUsage,
        reply: String,
    },
    ImmediateWithUsage {
        usage_updates: Vec<(u64, u64)>,
        chunks_template: Vec<String>,
    },
    ChunksThenHang {
        chunks_template: Vec<String>,
    },
    PacedChunks {
        chunk_delay: Duration,
        chunks_template: Vec<String>,
    },
    UpdatesThenHang {
        initial_chunk_template: String,
        trailing_updates: Vec<SessionUpdatePayload>,
    },
    WebSearchSplitDelta {
        query: String,
        answer: String,
    },
    RpcError {
        code: i64,
        message: String,
    },
    HangForever,
    SoftCancellableHang {
        cancel_rx: Arc<tokio::sync::Notify>,
        cancelled_usage: AcpUsage,
        initial_updates: Vec<SessionUpdatePayload>,
    },
    McpInteractive {
        steps: Vec<McpMockStep>,
        cancelled_usage: AcpUsage,
    },
}

enum McpMockStep {
    CallTool {
        usage_used: u64,
        thoughts: Vec<String>,
        pre_text_templates: Vec<String>,
        mcp_name: String,
        arguments: Value,
        expect_reply_substrings: Vec<&'static str>,
    },
    FinalReply {
        usage_used: u64,
        thoughts: Vec<String>,
        chunks_template: Vec<String>,
        final_usage: AcpUsage,
    },
}

async fn post_loopback_mcp_raw(url: &str, body_bytes: &[u8]) -> (u16, Vec<u8>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let rest = url
        .strip_prefix("http://")
        .expect("loopback MCP URL must start with http://");
    let (host_port, path) = rest.split_once('/').map_or((rest, "/"), |(hp, p)| (hp, p));
    let req_path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    let mut stream = tokio::net::TcpStream::connect(host_port)
        .await
        .expect("must connect to loopback MCP listener");
    let header = format!(
        "POST {req_path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body_bytes.len()
    );
    stream.write_all(header.as_bytes()).await.unwrap();
    if let Err(err) = stream.write_all(body_bytes).await {
        assert!(
            matches!(
                err.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            ),
            "unexpected loopback MCP write error: {err}"
        );
    }
    let _ = stream.flush().await;

    let mut raw_resp = Vec::new();
    if let Err(err) = stream.read_to_end(&mut raw_resp).await {
        assert!(
            matches!(
                err.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ) && !raw_resp.is_empty(),
            "unexpected loopback MCP read error: {err} (bytes_read={})",
            raw_resp.len()
        );
    }
    let sep = raw_resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("HTTP response header separator");
    let head_str = String::from_utf8_lossy(&raw_resp[..sep]);
    let status = head_str
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .expect("HTTP status code");
    let resp_body = raw_resp[sep + 4..].to_vec();
    (status, resp_body)
}

async fn post_loopback_mcp_json(url: &str, payload: &Value) -> (u16, Option<Value>) {
    let bytes = serde_json::to_vec(payload).unwrap();
    let (status, resp_bytes) = post_loopback_mcp_raw(url, &bytes).await;
    let parsed = if resp_bytes.is_empty() {
        None
    } else {
        Some(serde_json::from_slice(&resp_bytes).unwrap())
    };
    (status, parsed)
}

struct RecordedPrompt {
    session_id: String,
    text_joined: String,
    image_blocks: Vec<(String, usize)>,
}

struct MockAcpBackend {
    next_session_num: AtomicUsize,
    aborted_count: AtomicUsize,
    soft_cancel_enabled: std::sync::atomic::AtomicBool,
    soft_cancel_notify_slot: Mutex<Option<Arc<tokio::sync::Notify>>>,
    session_cancel_notifiers: Mutex<std::collections::HashMap<String, Arc<tokio::sync::Notify>>>,
    session_nonces: Mutex<Vec<(String, String)>>,
    session_mcp_servers: Mutex<Vec<(String, Vec<McpServerConfig>)>>,
    recorded_system_appends: Mutex<Vec<String>>,
    recorded_models: Mutex<Vec<String>>,
    recorded_prompts: Mutex<Vec<RecordedPrompt>>,
    scripts: Mutex<VecDeque<MockScript>>,
}

impl MockAcpBackend {
    fn new(scripts: Vec<MockScript>) -> Self {
        Self {
            next_session_num: AtomicUsize::new(1),
            aborted_count: AtomicUsize::new(0),
            soft_cancel_enabled: std::sync::atomic::AtomicBool::new(false),
            soft_cancel_notify_slot: Mutex::new(None),
            session_cancel_notifiers: Mutex::new(std::collections::HashMap::new()),
            session_nonces: Mutex::new(Vec::new()),
            session_mcp_servers: Mutex::new(Vec::new()),
            recorded_system_appends: Mutex::new(Vec::new()),
            recorded_models: Mutex::new(Vec::new()),
            recorded_prompts: Mutex::new(Vec::new()),
            scripts: Mutex::new(VecDeque::from(scripts)),
        }
    }

    fn enable_soft_cancel(&self) {
        self.soft_cancel_enabled.store(true, Ordering::SeqCst);
    }

    fn enable_soft_cancel_with_notify(&self, notify: Arc<tokio::sync::Notify>) {
        self.soft_cancel_enabled.store(true, Ordering::SeqCst);
        *self.soft_cancel_notify_slot.lock().unwrap() = Some(notify);
    }

    fn extract_nonce(system_append: &str) -> String {
        let marker = "<agy_tool_call_";
        if let Some((_, after)) = system_append.split_once(marker)
            && let Some((nonce, _)) = after.split_once('>')
        {
            return nonce.to_owned();
        }
        "none".to_owned()
    }
}

impl AcpBackend for MockAcpBackend {
    fn available_models(&self) -> Vec<AvailableModel> {
        mock_available_models()
    }

    async fn create_session(&self, system_append: &str) -> Result<SessionNewResult, AcpError> {
        self.create_session_with_mcp(system_append, &[]).await
    }

    async fn create_session_with_mcp(
        &self,
        system_append: &str,
        mcp_servers: &[McpServerConfig],
    ) -> Result<SessionNewResult, AcpError> {
        let n = self.next_session_num.fetch_add(1, Ordering::SeqCst);
        let sid_str = format!("sess_{n}");
        let nonce = Self::extract_nonce(system_append);
        self.recorded_system_appends
            .lock()
            .unwrap()
            .push(system_append.to_owned());
        self.session_nonces
            .lock()
            .unwrap()
            .push((sid_str.clone(), nonce));
        self.session_mcp_servers
            .lock()
            .unwrap()
            .push((sid_str.clone(), mcp_servers.to_vec()));
        self.session_cancel_notifiers
            .lock()
            .unwrap()
            .insert(sid_str.clone(), Arc::new(tokio::sync::Notify::new()));

        Ok(SessionNewResult {
            session_id: SessionId::try_new(sid_str).unwrap(),
            models: ModelsCatalog {
                available_models: mock_available_models(),
                current_model_id: Some("gemini-4-carbon-medium".to_owned()),
            },
            modes: ModesCatalog::default(),
        })
    }

    async fn set_session_model(
        &self,
        _session_id: &SessionId,
        model_id: &ModelId,
    ) -> Result<(), AcpError> {
        self.recorded_models
            .lock()
            .unwrap()
            .push(model_id.as_str().to_owned());
        Ok(())
    }

    async fn set_session_mode(
        &self,
        _session_id: &SessionId,
        _mode_id: &AcpModeId,
    ) -> Result<(), AcpError> {
        Ok(())
    }

    async fn start_prompt(
        &self,
        session_id: &SessionId,
        prompt: &[AcpPromptBlock],
        updates_tx: mpsc::Sender<SessionUpdatePayload>,
    ) -> Result<PromptTurnResult, AcpError> {
        let joined = prompt
            .iter()
            .filter_map(|b| match b {
                AcpPromptBlock::Text { text } => Some(text.as_str()),
                AcpPromptBlock::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let image_blocks = prompt
            .iter()
            .filter_map(|b| match b {
                AcpPromptBlock::Image { data, mime_type } => Some((mime_type.clone(), data.len())),
                AcpPromptBlock::Text { .. } => None,
            })
            .collect::<Vec<_>>();

        self.recorded_prompts.lock().unwrap().push(RecordedPrompt {
            session_id: session_id.as_str().to_owned(),
            text_joined: joined,
            image_blocks,
        });

        let turn_cancel = Arc::new(tokio::sync::Notify::new());
        self.session_cancel_notifiers
            .lock()
            .unwrap()
            .insert(session_id.as_str().to_owned(), Arc::clone(&turn_cancel));

        let nonce = self
            .session_nonces
            .lock()
            .unwrap()
            .iter()
            .find(|(sid, _)| sid == session_id.as_str())
            .map(|(_, n)| n.clone())
            .unwrap_or_default();

        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(MockScript::Immediate {
                thoughts: vec![],
                chunks_template: vec!["OK".to_owned()],
            });

        match script {
            MockScript::Immediate {
                thoughts,
                chunks_template,
            } => {
                for th in thoughts {
                    let _ = updates_tx
                        .send(SessionUpdatePayload::AgentThoughtChunk {
                            content: AcpChunkContent {
                                content_type: "text".to_owned(),
                                text: th,
                            },
                        })
                        .await;
                }
                for raw_chunk in chunks_template {
                    let hydrated = raw_chunk.replace("{NONCE}", &nonce);
                    let _ = updates_tx
                        .send(SessionUpdatePayload::AgentMessageChunk {
                            content: AcpChunkContent {
                                content_type: "text".to_owned(),
                                text: hydrated,
                            },
                        })
                        .await;
                }
                Ok(PromptTurnResult {
                    stop_reason: "end_turn".to_owned(),
                    usage: mock_acp_usage(100, 25, 10),
                    error: None,
                })
            }
            MockScript::ImmediateWithExactUsage { usage, reply } => {
                let hydrated = reply.replace("{NONCE}", &nonce);
                let _ = updates_tx
                    .send(SessionUpdatePayload::AgentMessageChunk {
                        content: AcpChunkContent {
                            content_type: "text".to_owned(),
                            text: hydrated,
                        },
                    })
                    .await;
                Ok(PromptTurnResult {
                    stop_reason: "end_turn".to_owned(),
                    usage,
                    error: None,
                })
            }
            MockScript::ImmediateWithUsage {
                usage_updates,
                chunks_template,
            } => {
                if let Some(&(first_used, first_size)) = usage_updates.first() {
                    let _ = updates_tx
                        .send(SessionUpdatePayload::UsageUpdate {
                            used: first_used,
                            size: first_size,
                        })
                        .await;
                }
                for raw_chunk in chunks_template {
                    let hydrated = raw_chunk.replace("{NONCE}", &nonce);
                    let _ = updates_tx
                        .send(SessionUpdatePayload::AgentMessageChunk {
                            content: AcpChunkContent {
                                content_type: "text".to_owned(),
                                text: hydrated,
                            },
                        })
                        .await;
                }
                for &(used, size) in usage_updates.iter().skip(1) {
                    let _ = updates_tx
                        .send(SessionUpdatePayload::UsageUpdate { used, size })
                        .await;
                }
                Ok(PromptTurnResult {
                    stop_reason: "end_turn".to_owned(),
                    usage: mock_acp_usage(100, 25, 10),
                    error: None,
                })
            }
            MockScript::ChunksThenHang { chunks_template } => {
                for raw_chunk in chunks_template {
                    let hydrated = raw_chunk.replace("{NONCE}", &nonce);
                    let _ = updates_tx
                        .send(SessionUpdatePayload::AgentMessageChunk {
                            content: AcpChunkContent {
                                content_type: "text".to_owned(),
                                text: hydrated,
                            },
                        })
                        .await;
                }
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(PromptTurnResult::default())
            }
            MockScript::PacedChunks {
                chunk_delay,
                chunks_template,
            } => {
                for (idx, raw_chunk) in chunks_template.into_iter().enumerate() {
                    if idx > 0 {
                        tokio::time::sleep(chunk_delay).await;
                    }
                    let hydrated = raw_chunk.replace("{NONCE}", &nonce);
                    let _ = updates_tx
                        .send(SessionUpdatePayload::AgentMessageChunk {
                            content: AcpChunkContent {
                                content_type: "text".to_owned(),
                                text: hydrated,
                            },
                        })
                        .await;
                }
                Ok(PromptTurnResult {
                    stop_reason: "end_turn".to_owned(),
                    usage: mock_acp_usage(100, 40, 10),
                    error: None,
                })
            }
            MockScript::UpdatesThenHang {
                initial_chunk_template,
                trailing_updates,
            } => {
                let hydrated = initial_chunk_template.replace("{NONCE}", &nonce);
                let _ = updates_tx
                    .send(SessionUpdatePayload::AgentMessageChunk {
                        content: AcpChunkContent {
                            content_type: "text".to_owned(),
                            text: hydrated,
                        },
                    })
                    .await;
                for upd in trailing_updates {
                    let _ = updates_tx.send(upd).await;
                }
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(PromptTurnResult::default())
            }
            MockScript::WebSearchSplitDelta { query, answer } => {
                let raw_in = RawValue::from_string(json!({ "query": query }).to_string()).unwrap();
                let _ = updates_tx
                    .send(SessionUpdatePayload::ToolCall {
                        tool_call_id: "search_1".to_owned(),
                        name: "search_web".to_owned(),
                        status: "in_progress".to_owned(),
                        kind: "search".to_owned(),
                        title: format!("\"{query}\" on Web"),
                        args: None,
                        raw_input: Some(raw_in),
                    })
                    .await;
                // ACP `tool_call_update` omits unchanged `kind` and `rawInput`
                let _ = updates_tx
                    .send(SessionUpdatePayload::ToolCallUpdate {
                        tool_call_id: "search_1".to_owned(),
                        name: String::new(),
                        status: "completed".to_owned(),
                        kind: String::new(),
                        title: String::new(),
                        args: None,
                        raw_input: None,
                    })
                    .await;
                let _ = updates_tx
                    .send(SessionUpdatePayload::AgentMessageChunk {
                        content: AcpChunkContent {
                            content_type: "text".to_owned(),
                            text: answer,
                        },
                    })
                    .await;
                Ok(PromptTurnResult {
                    stop_reason: "end_turn".to_owned(),
                    usage: mock_acp_usage(80, 20, 0),
                    error: None,
                })
            }
            MockScript::RpcError { code, message } => Err(AcpError::Rpc {
                method: "session/prompt",
                code,
                message,
            }),
            MockScript::HangForever => {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(PromptTurnResult::default())
            }
            MockScript::SoftCancellableHang {
                cancel_rx,
                cancelled_usage,
                initial_updates,
            } => {
                for mut upd in initial_updates {
                    if let SessionUpdatePayload::AgentMessageChunk { content } = &mut upd {
                        content.text = content.text.replace("{NONCE}", &nonce);
                    }
                    let _ = updates_tx.send(upd).await;
                }
                cancel_rx.notified().await;
                Ok(PromptTurnResult {
                    stop_reason: "cancelled".to_owned(),
                    usage: cancelled_usage,
                    error: None,
                })
            }
            MockScript::McpInteractive {
                steps,
                cancelled_usage,
            } => {
                let mcp_url = self
                    .session_mcp_servers
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(sid, _)| sid == session_id.as_str())
                    .and_then(|(_, cfgs)| cfgs.first())
                    .and_then(|cfg| cfg.url.clone())
                    .expect("McpInteractive requires loopback MCP server URL on session/new");
                let sess_cancel = self
                    .session_cancel_notifiers
                    .lock()
                    .unwrap()
                    .get(session_id.as_str())
                    .cloned()
                    .unwrap_or_else(|| Arc::new(tokio::sync::Notify::new()));

                // Simulate exact `agy acp` v1.5.14 MCP handshake on `session/new`:
                // 1) `server/discover` -> `-32601`
                let (disc_status, disc_body) = post_loopback_mcp_json(
                    &mcp_url,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "server/discover",
                        "params": { "protocolVersion": "2026-07-28" }
                    }),
                )
                .await;
                assert_eq!(disc_status, 200);
                assert_eq!(disc_body.unwrap()["error"]["code"], -32601);

                // 2) `initialize` -> `"2025-11-25"`
                let (init_status, init_body) = post_loopback_mcp_json(
                    &mcp_url,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "method": "initialize",
                        "params": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": {},
                            "clientInfo": { "name": "antigravity-client", "version": "1.0.0" }
                        }
                    }),
                )
                .await;
                assert_eq!(init_status, 200);
                assert_eq!(
                    init_body.unwrap()["result"]["protocolVersion"],
                    "2025-11-25"
                );

                // 3) `notifications/initialized` -> `202 Accepted`
                let (notif_status, notif_body) = post_loopback_mcp_json(
                    &mcp_url,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/initialized"
                    }),
                )
                .await;
                assert_eq!(notif_status, 202);
                assert!(notif_body.is_none());

                // 4) `tools/list` -> `200 OK`
                let (list_status, list_body) = post_loopback_mcp_json(
                    &mcp_url,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": 3,
                        "method": "tools/list",
                        "params": {}
                    }),
                )
                .await;
                assert_eq!(list_status, 200);
                assert!(list_body.unwrap()["result"]["tools"].is_array());

                for (step_idx, step) in steps.into_iter().enumerate() {
                    match step {
                        McpMockStep::CallTool {
                            usage_used,
                            thoughts,
                            pre_text_templates,
                            mcp_name,
                            arguments,
                            expect_reply_substrings,
                        } => {
                            if usage_used > 0 {
                                let _ = updates_tx
                                    .send(SessionUpdatePayload::UsageUpdate {
                                        used: usage_used,
                                        size: 1_048_576,
                                    })
                                    .await;
                            }
                            for th in thoughts {
                                let _ = updates_tx
                                    .send(SessionUpdatePayload::AgentThoughtChunk {
                                        content: AcpChunkContent {
                                            content_type: "text".to_owned(),
                                            text: th,
                                        },
                                    })
                                    .await;
                            }
                            for tmpl in pre_text_templates {
                                let hydrated = tmpl.replace("{NONCE}", &nonce);
                                let _ = updates_tx
                                    .send(SessionUpdatePayload::AgentMessageChunk {
                                        content: AcpChunkContent {
                                            content_type: "text".to_owned(),
                                            text: hydrated,
                                        },
                                    })
                                    .await;
                            }
                            let acp_tc_id = format!("tc_mcp_{}_{}", session_id.as_str(), step_idx);
                            let raw_args = RawValue::from_string(arguments.to_string()).unwrap();
                            let _ = updates_tx
                                .send(SessionUpdatePayload::ToolCall {
                                    tool_call_id: acp_tc_id.clone(),
                                    name: format!("mcp_lg_{mcp_name}"),
                                    status: "in_progress".to_owned(),
                                    kind: "other".to_owned(),
                                    title: format!("Run {mcp_name}"),
                                    args: None,
                                    raw_input: Some(raw_args),
                                })
                                .await;

                            let rpc_call_payload = json!({
                                "jsonrpc": "2.0",
                                "id": 100 + step_idx,
                                "method": "tools/call",
                                "params": {
                                    "name": mcp_name,
                                    "arguments": arguments
                                }
                            });
                            let call_res = tokio::select! {
                                biased;
                                () = sess_cancel.notified() => None,
                                out = post_loopback_mcp_json(&mcp_url, &rpc_call_payload) => Some(out),
                            };
                            let Some((call_status, Some(call_json))) = call_res else {
                                return Ok(PromptTurnResult {
                                    stop_reason: "cancelled".to_owned(),
                                    usage: cancelled_usage,
                                    error: None,
                                });
                            };
                            assert_eq!(call_status, 200);
                            if call_json.get("error").is_some() {
                                return Ok(PromptTurnResult {
                                    stop_reason: "cancelled".to_owned(),
                                    usage: cancelled_usage,
                                    error: None,
                                });
                            }
                            let joined_texts = call_json["result"]["content"]
                                .as_array()
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|item| item.get("text").and_then(Value::as_str))
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                })
                                .unwrap_or_default();
                            for needle in &expect_reply_substrings {
                                assert!(
                                    joined_texts.contains(needle),
                                    "expected MCP tools/call text `{joined_texts}` to contain `{needle}`"
                                );
                            }
                            let is_err = call_json["result"]["isError"].as_bool().unwrap_or(false);
                            let _ = updates_tx
                                .send(SessionUpdatePayload::ToolCallUpdate {
                                    tool_call_id: acp_tc_id,
                                    name: String::new(),
                                    status: if is_err {
                                        "failed".to_owned()
                                    } else {
                                        "completed".to_owned()
                                    },
                                    kind: String::new(),
                                    title: String::new(),
                                    args: None,
                                    raw_input: None,
                                })
                                .await;
                        }
                        McpMockStep::FinalReply {
                            usage_used,
                            thoughts,
                            chunks_template,
                            final_usage,
                        } => {
                            if usage_used > 0 {
                                let _ = updates_tx
                                    .send(SessionUpdatePayload::UsageUpdate {
                                        used: usage_used,
                                        size: 1_048_576,
                                    })
                                    .await;
                            }
                            for th in thoughts {
                                let _ = updates_tx
                                    .send(SessionUpdatePayload::AgentThoughtChunk {
                                        content: AcpChunkContent {
                                            content_type: "text".to_owned(),
                                            text: th,
                                        },
                                    })
                                    .await;
                            }
                            for tmpl in chunks_template {
                                let hydrated = tmpl.replace("{NONCE}", &nonce);
                                let _ = updates_tx
                                    .send(SessionUpdatePayload::AgentMessageChunk {
                                        content: AcpChunkContent {
                                            content_type: "text".to_owned(),
                                            text: hydrated,
                                        },
                                    })
                                    .await;
                            }
                            return Ok(PromptTurnResult {
                                stop_reason: "end_turn".to_owned(),
                                usage: final_usage,
                                error: None,
                            });
                        }
                    }
                }

                Ok(PromptTurnResult {
                    stop_reason: "end_turn".to_owned(),
                    usage: cancelled_usage,
                    error: None,
                })
            }
        }
    }

    fn cancel_prompt_for_reuse(&self, session_id: &SessionId) -> bool {
        if self.soft_cancel_enabled.load(Ordering::SeqCst) {
            if let Some(custom) = self.soft_cancel_notify_slot.lock().unwrap().as_ref() {
                custom.notify_one();
            }
            if let Some(sess_n) = self
                .session_cancel_notifiers
                .lock()
                .unwrap()
                .get(session_id.as_str())
            {
                sess_n.notify_waiters();
                sess_n.notify_one();
            }
            true
        } else {
            false
        }
    }

    fn abort_session_nonblocking(&self, session_id: &SessionId) {
        self.aborted_count.fetch_add(1, Ordering::SeqCst);
        if let Some(sess_n) = self
            .session_cancel_notifiers
            .lock()
            .unwrap()
            .get(session_id.as_str())
        {
            sess_n.notify_waiters();
            sess_n.notify_one();
        }
    }

    async fn close_session(&self, _session_id: &SessionId) -> Result<(), AcpError> {
        Ok(())
    }

    async fn fetch_web_url(
        &self,
        url: &str,
        allowed_domains: &[String],
        blocked_domains: &[String],
    ) -> liftgate_server::web_fetch::WebFetchOutcome {
        if url.contains("127.0.0.1") || url.contains("localhost") {
            return liftgate_server::web_fetch::fetch_url_sanitized(
                url,
                allowed_domains,
                blocked_domains,
            )
            .await;
        }
        liftgate_server::web_fetch::WebFetchOutcome::Success {
            url: url.to_owned(),
            title: Some("Singapore 24-Hour Official Forecast".to_owned()),
            media_type: "text/plain".to_owned(),
            content: "Partly cloudy, 25C to 31C, humidity 70%-75%.".to_owned(),
        }
    }
}

fn test_app_with_key(
    backend: Arc<MockAcpBackend>,
    timeout_secs: u64,
    api_key: Option<&str>,
) -> axum::Router {
    build_router(test_state(backend, timeout_secs, api_key))
}

fn test_state(
    backend: Arc<MockAcpBackend>,
    timeout_secs: u64,
    api_key: Option<&str>,
) -> AppState<MockAcpBackend> {
    let limits =
        GatewayLimits::try_new(16 * 1024 * 1024, 12 * 1024 * 1024, 64, 16, 64, timeout_secs)
            .unwrap();
    let spill_dir = std::env::temp_dir().join("liftgate-protocol-test-spills");

    AppState {
        backend,
        pool: Arc::new(SessionPool::new(limits)),
        telemetry: Arc::new(TelemetryHub::new()),
        config: GatewayRuntimeConfig {
            title_model_file: None,
            spill_dir: Some(spill_dir),
            settings_path: None,
            default_model: ModelId::try_new("gemini-4-carbon-high").unwrap(),
            acp_mode: AcpModeId::try_new("auto").unwrap(),
            api_key: api_key.map(Arc::<str>::from),
            limits,
            vision_offload: Arc::new(AtomicBool::new(false)),
            mcp_tool_bridge: Arc::new(AtomicBool::new(true)),
            mcp_server: None,
        },
        ui_dist_dir: None,
        embedded_ui: &[],
        admin_shutdown: None,
    }
}

fn test_app(backend: Arc<MockAcpBackend>, timeout_secs: u64) -> axum::Router {
    test_app_with_key(backend, timeout_secs, None)
}

#[tokio::test]
async fn title_model_change_applies_to_the_next_request_on_the_same_gateway() {
    let backend = Arc::new(MockAcpBackend::new(
        (0..2)
            .map(|_| MockScript::Immediate {
                thoughts: vec![],
                chunks_template: vec!["Compile report".to_owned()],
            })
            .collect(),
    ));
    let path = std::env::temp_dir().join(format!("liftgate-title-{}", uuid::Uuid::new_v4()));
    let mut state = test_state(Arc::clone(&backend), 30, None);
    state.config.title_model_file = Some(path.clone());
    let app = build_router(state);
    for (uri, request) in [
        (
            "/v1/responses",
            json!({
                "model": "gemini-4-carbon-high",
                "input": "Generate a concise, single-line task title for compiling a report."
            }),
        ),
        (
            "/v1/messages",
            json!({
                "model": "gemini-4-carbon-high", "max_tokens": 32,
                "system": "Return isNewTopic and a short title.",
                "messages": [{"role": "user", "content": "Compile a report."}]
            }),
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::post(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(request.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        if uri == "/v1/responses" {
            std::fs::write(&path, "gemini-4-carbon-medium").unwrap();
        }
    }
    std::fs::remove_file(path).unwrap();
    assert_eq!(
        backend.recorded_models.lock().unwrap().as_slice(),
        ["gemini-4-hydrogen-low", "gemini-4-carbon-medium"]
    );
}

#[tokio::test]
async fn context_policy_bounds_single_turn_inputs_and_maps_upstream_overflow_to_wire_contracts() {
    let multiline_200k: String = (1..=4_000)
        .map(|i| format!("LINE {i:04}: {}\n", "x".repeat(40)))
        .collect();
    assert!(multiline_200k.len() > 200_000);

    // 1. Single-turn water-fill policy automatically middle-truncates oversized single-turn
    //    inputs (preserving head + tail + structured marker) under `MAX_ACP_PROMPT_TEXT_BYTES`.
    {
        let backend = Arc::new(MockAcpBackend::new(vec![]));
        let app = test_app(Arc::clone(&backend), 30);
        for endpoint in ["/v1/responses", "/v1/messages"] {
            let payload = if endpoint == "/v1/responses" {
                json!({"model":"gemini-4-carbon-high", "input": &multiline_200k})
            } else {
                json!({"model":"gemini-4-carbon-high", "max_tokens": 100,
                    "messages":[{"role":"user", "content": &multiline_200k}]})
            };
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(endpoint)
                        .header("content-type", "application/json")
                        .body(Body::from(payload.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
        let recorded = backend.recorded_prompts.lock().unwrap();
        assert_eq!(recorded.len(), 2);
        for prompt in recorded.iter() {
            assert!(
                prompt.text_joined.len() <= liftgate_types::session_key::MAX_ACP_PROMPT_TEXT_BYTES
            );
            assert!(prompt.text_joined.contains("LINE 0001:"));
            assert!(prompt.text_joined.contains("LINE 4000:"));
            assert!(prompt.text_joined.contains("[Gateway truncated "));
            assert!(
                prompt
                    .text_joined
                    .contains("Full lossless content saved locally at `"),
                "expected local spill file path in truncation notice"
            );
            let (_, after_prefix) = prompt
                .text_joined
                .split_once("Full lossless content saved locally at `")
                .unwrap();
            let (spilled_path, _) = after_prefix.split_once('`').unwrap();
            assert_eq!(
                std::fs::read_to_string(spilled_path).unwrap(),
                multiline_200k.trim()
            );
        }
    }

    // 2. Non-streaming HTTP 400 wire contract when upstream reports context window exceeded:
    //    returns standard auto-compactable errors for both OpenAI Codex (`context_length_exceeded`)
    //    and Claude Code (`prompt is too long: ...`).
    {
        let backend = Arc::new(MockAcpBackend::new(vec![
            MockScript::RpcError {
                code: -32603,
                message: "Context window exceeded: maximum context length reached".to_owned(),
            },
            MockScript::RpcError {
                code: -32603,
                message: "Context window exceeded: maximum context length reached".to_owned(),
            },
        ]));
        let app = test_app(Arc::clone(&backend), 30);

        let oai_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model":"gemini-4-carbon-high", "input": &multiline_200k})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(oai_resp.status(), StatusCode::BAD_REQUEST);
        let oai_body: Value =
            serde_json::from_slice(&oai_resp.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(oai_body["error"]["type"], "invalid_request_error");
        assert_eq!(oai_body["error"]["code"], "context_length_exceeded");
        assert_eq!(oai_body["error"]["param"], "input");
        assert!(
            oai_body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .starts_with("Your input exceeds the context window of this model.")
        );

        let anth_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "gemini-4-carbon-high",
                            "max_tokens": 100,
                            "messages": [{"role": "user", "content": &multiline_200k}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anth_resp.status(), StatusCode::BAD_REQUEST);
        let anth_body: Value =
            serde_json::from_slice(&anth_resp.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(anth_body["error"]["type"], "invalid_request_error");
        assert_eq!(
            anth_body["error"]["message"],
            "prompt is too long: 500001 tokens > 500000 maximum"
        );

        assert_eq!(backend.recorded_prompts.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn oversized_tool_result_auto_truncates_head_and_tail_and_preserves_warm_session_reuse() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![
                "<agy_tool_call_{NONCE}>{\"name\":\"exec_command\",\"arguments\":{\"cmd\":\"cargo test\"}}</agy_tool_call_{NONCE}>\n<agy_tool_call_{NONCE}>{\"name\":\"exec_command\",\"arguments\":{\"cmd\":\"git status -s\"}}</agy_tool_call_{NONCE}>".to_owned(),
            ],
        },
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Analyzed truncated cargo test output.".to_owned()],
        },
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Warm follow-up succeeded.".to_owned()],
        },
    ]));
    let app = test_app(Arc::clone(&backend), 30);

    let tools = json!([{
        "type": "function",
        "name": "exec_command",
        "description": "Run shell command",
        "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}
    }]);

    // Turn 1: Assistant requests two tool calls.
    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "tools": tools,
                        "input": [{"role": "user", "content": "Run tests and check git status"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let b1: Value =
        serde_json::from_slice(&r1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let out_items = b1["output"].as_array().unwrap();
    assert_eq!(out_items.len(), 2);
    let call_id_1 = out_items[0]["call_id"].as_str().unwrap().to_owned();
    let call_id_2 = out_items[1]["call_id"].as_str().unwrap().to_owned();

    // Turn 2: Client returns a ~196 KB `cargo test` output + a tiny `git status` sibling output.
    let huge_tool_output: String = (1..=4_000)
        .map(|i| format!("CARGO_LINE_{i:04}: {}\n", "z".repeat(32)))
        .collect();
    assert!(huge_tool_output.len() > 195_000);

    let mut turn2_input = vec![
        json!({"role": "user", "content": "Run tests and check git status"}),
        out_items[0].clone(),
        out_items[1].clone(),
        json!({"type": "function_call_output", "call_id": call_id_1, "output": &huge_tool_output}),
        json!({"type": "function_call_output", "call_id": call_id_2, "output": "SMALL_GIT_STATUS_CLEAN"}),
    ];

    let r2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "tools": tools,
                        "input": turn2_input
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let b2: Value =
        serde_json::from_slice(&r2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        b2["output"][0]["content"][0]["text"],
        "Analyzed truncated cargo test output."
    );

    // Turn 3: Client resends the original un-truncated 196 KB tool output in prior history + follow-up.
    turn2_input.push(b2["output"][0].clone());
    turn2_input.push(json!({"role": "user", "content": "What should we fix next?"}));
    let r3 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "tools": tools,
                        "input": turn2_input
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r3.status(), StatusCode::OK);

    let recorded = backend.recorded_prompts.lock().unwrap();
    assert_eq!(recorded.len(), 3);
    assert_eq!(recorded[0].session_id, "sess_1");
    assert_eq!(recorded[1].session_id, "sess_1");
    assert_eq!(recorded[2].session_id, "sess_1");

    // Turn 2 outbound prompt stayed within budget, kept small sibling lossless, and preserved head + tail.
    assert!(
        recorded[1].text_joined.len() <= liftgate_types::session_key::MAX_ACP_PROMPT_TEXT_BYTES
    );
    assert!(recorded[1].text_joined.contains("SMALL_GIT_STATUS_CLEAN"));
    assert!(recorded[1].text_joined.contains("CARGO_LINE_0001:"));
    assert!(recorded[1].text_joined.contains("CARGO_LINE_4000:"));
    assert!(
        recorded[1]
            .text_joined
            .contains("[Gateway truncated tool output:")
    );
    let (_, after_prefix) = recorded[1]
        .text_joined
        .split_once("Full lossless content saved locally at `")
        .expect("expected local spill file path for truncated tool output");
    let (spilled_tool_path, _) = after_prefix.split_once('`').unwrap();
    assert_eq!(
        std::fs::read_to_string(spilled_tool_path).unwrap(),
        huge_tool_output.trim()
    );

    // Turn 3 matched warm `sess_1` prefix hash and forwarded only the new user delta!
    assert_eq!(recorded[2].text_joined, "What should we fix next?");
}

#[tokio::test]
async fn sse_context_overflow_emits_standard_codex_and_claude_code_compact_errors() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        MockScript::RpcError {
            code: -32603,
            message: "Context window exceeded: maximum context length reached".to_owned(),
        },
        MockScript::RpcError {
            code: -32603,
            message: "Context window exceeded: maximum context length reached".to_owned(),
        },
    ]));
    let app = test_app(Arc::clone(&backend), 30);

    let oai_sse_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": true,
                        "input": "hello"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(oai_sse_resp.status(), StatusCode::OK);
    let oai_sse_text = String::from_utf8(
        oai_sse_resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(oai_sse_text.contains("event: response.failed"));
    assert!(oai_sse_text.contains("\"code\":\"context_length_exceeded\""));
    assert!(oai_sse_text.contains("Your input exceeds the context window of this model."));

    let anth_sse_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-opus-4-6-thinking",
                        "max_tokens": 100,
                        "stream": true,
                        "messages": [{"role": "user", "content": "hello"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anth_sse_resp.status(), StatusCode::OK);
    let anth_sse_text = String::from_utf8(
        anth_sse_resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(anth_sse_text.contains("event: error"));
    assert!(anth_sse_text.contains("\"type\":\"invalid_request_error\""));
    assert!(anth_sse_text.contains("prompt is too long: 200001 tokens > 200000 maximum"));
}

#[tokio::test]
async fn clients_receive_context_occupancy_without_double_counting_thought_or_cache() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        MockScript::ImmediateWithUsage {
            usage_updates: vec![(246_000, 1_048_576)],
            chunks_template: vec!["First.".to_owned()],
        },
        MockScript::ImmediateWithUsage {
            usage_updates: vec![(248_000, 1_048_576)],
            chunks_template: vec!["Second.".to_owned()],
        },
    ]));
    let app = test_app(Arc::clone(&backend), 30);
    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(json!({"input":"One."}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&first.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["usage"]["input_tokens"], 245_975);
    assert_eq!(body["usage"]["output_tokens"], 25);
    assert_eq!(body["usage"]["total_tokens"], 246_000);
    let second = app.oneshot(Request::builder().method("POST").uri("/v1/messages")
        .header("content-type","application/json").body(Body::from(json!({"model":"gemini-4-carbon-high", "max_tokens":100,"stream":true,
            "messages":[{"role":"user","content":"One."},{"role":"assistant","content":"First."},{"role":"user","content":"Two."}]}).to_string())).unwrap()).await.unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let sse = String::from_utf8(
        second
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    let delta = sse
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|s| serde_json::from_str::<Value>(s).ok())
        .find(|v| v["type"] == "message_delta")
        .unwrap();
    let usage = &delta["usage"];
    assert_eq!(
        usage["input_tokens"].as_u64().unwrap()
            + usage["cache_read_input_tokens"].as_u64().unwrap()
            + usage["cache_creation_input_tokens"].as_u64().unwrap(),
        247_975
    );
    assert_eq!(usage["output_tokens"], 25);
    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts[0].session_id, prompts[1].session_id);
}

#[tokio::test(start_paused = true)]
async fn openai_responses_sse_function_call_and_stateful_continuation() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        MockScript::Immediate {
            thoughts: vec!["Need to inspect Cargo.toml".to_owned()],
            chunks_template: vec![
                "Let me check.\n<agy_tool_call_{NONCE}>".to_owned(),
                "{\"name\":\"shell\",\"arguments\":{\"cmd\":\"cargo check\"}}</agy_tool_call_{NONCE}>".to_owned(),
            ],
        },
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Build succeeded!".to_owned()],
        },
    ]));

    let app = test_app(Arc::clone(&backend), 30);

    // Turn 1: Streaming `/v1/responses` with function tool
    let req_body = json!({
        "model": "gpt-5-codex",
        "reasoning": { "effort": "high", "summary": "auto" },
        "stream": true,
        "instructions": "You are Codex.",
        "input": [
            { "role": "user", "content": "Run cargo check" }
        ],
        "tools": [
            {
                "type": "function",
                "name": "shell",
                "description": "Execute command",
                "parameters": { "type": "object", "properties": { "cmd": { "type": "string" } } }
            }
        ]
    });

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(req_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let raw_sse = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();

    assert!(raw_sse.contains("event: response.created"));
    assert!(raw_sse.contains("event: response.reasoning_summary_text.delta"));
    assert!(raw_sse.contains("event: response.output_text.delta"));
    assert!(raw_sse.contains("event: response.function_call_arguments.done"));
    assert!(raw_sse.contains("event: response.completed"));

    // Parse completed event to extract `response_id` and `call_id`
    let completed_data = raw_sse
        .lines()
        .rev()
        .filter_map(|l| l.strip_prefix("data: "))
        .find(|d| d.contains("\"type\":\"response.completed\""))
        .unwrap();
    let completed_json: Value = serde_json::from_str(completed_data).unwrap();
    let resp_id = completed_json["response"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let output_arr = completed_json["response"]["output"].as_array().unwrap();
    let fn_item = output_arr
        .iter()
        .find(|i| i["type"] == "function_call")
        .unwrap();
    assert_eq!(fn_item["name"], "shell");
    let call_id = fn_item["call_id"].as_str().unwrap().to_owned();

    // Turn 2: Continuation via `previous_response_id` must reuse `sess_1`!
    let turn2_body = json!({
        "model": "gpt-5-codex",
        "previous_response_id": resp_id,
        "stream": false,
        "input": [
            {
                "type": "function_call_output",
                "call_id": call_id,
                "output": "Finished dev profile"
            }
        ]
    });

    let resp2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(turn2_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp2.status(), StatusCode::OK);
    let body2: Value =
        serde_json::from_slice(&resp2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body2["output"][0]["content"][0]["text"], "Build succeeded!");

    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0].session_id, "sess_1");
    assert_eq!(prompts[1].session_id, "sess_1");
    assert!(prompts[1].text_joined.contains("Finished dev profile"));
    assert!(!prompts[1].text_joined.contains("Run cargo check"));
}

#[tokio::test(start_paused = true)]
async fn anthropic_messages_sse_tool_use_and_prefix_hash_reuse() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        MockScript::Immediate {
            thoughts: vec!["Thinking about reading README".to_owned()],
            chunks_template: vec![
                "\n<agy_tool_call_{NONCE}>{\"name\":\"Read\",\"arguments\":{\"file_path\":\"README.md\"}}</agy_tool_call_{NONCE}>".to_owned(),
            ],
        },
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["README says hello.".to_owned()],
        },
    ]));

    let app = test_app(Arc::clone(&backend), 30);

    let tools = json!([
        {
            "name": "Read",
            "description": "Read a file",
            "input_schema": { "type": "object", "properties": { "file_path": { "type": "string" } } }
        }
    ]);

    // Turn 1: Claude Code streaming request
    let req1 = json!({
        "model": "claude-sonnet-4-5-20250929",
        "max_tokens": 8192,
        "thinking": { "type": "enabled", "budget_tokens": 16000 },
        "stream": true,
        "system": [{ "type": "text", "text": "You are Claude Code." }],
        "tools": tools,
        "messages": [
            { "role": "user", "content": "Read README.md" }
        ]
    });

    let resp1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(req1.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp1.status(), StatusCode::OK);
    let sse1 = String::from_utf8(
        resp1
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();

    assert!(sse1.contains("event: message_start"));
    assert!(sse1.contains("\"type\":\"thinking_delta\""));
    assert!(sse1.contains("\"type\":\"signature_delta\""));
    assert!(sse1.contains("\"type\":\"tool_use\""));
    assert!(sse1.contains("\"stop_reason\":\"tool_use\""));

    // Extract generated `toolu_...` ID
    let start_tool_data = sse1
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .find(|d| d.contains("\"type\":\"tool_use\""))
        .unwrap();
    let start_tool_val: Value = serde_json::from_str(start_tool_data).unwrap();
    let toolu_id = start_tool_val["content_block"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Turn 2: Stateless full-history request from Claude Code appending `tool_result`
    let req2 = json!({
        "model": "claude-sonnet-4-5-20250929",
        "max_tokens": 8192,
        "stream": false,
        "system": [{ "type": "text", "text": "You are Claude Code." }],
        "tools": tools,
        "messages": [
            { "role": "user", "content": "Read README.md" },
            {
                "role": "assistant",
                "content": [
                    {
                        "type": "tool_use",
                        "id": toolu_id,
                        "name": "Read",
                        "input": { "file_path": "README.md" }
                    }
                ]
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": toolu_id,
                        "content": "# Hello World"
                    }
                ]
            }
        ]
    });

    let resp2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(req2.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp2.status(), StatusCode::OK);
    let body2: Value =
        serde_json::from_slice(&resp2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body2["stop_reason"], "end_turn");
    assert_eq!(body2["content"][0]["text"], "README says hello.");

    // Verify prefix-hash matched `sess_1` and sent ONLY the delta `tool_result`!
    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0].session_id, "sess_1");
    assert_eq!(prompts[1].session_id, "sess_1");
    assert!(prompts[1].text_joined.contains("# Hello World"));
    assert!(!prompts[1].text_joined.contains("<conversation_history>"));
}

#[tokio::test(start_paused = true)]
async fn turn_timeout_aborts_leased_session_without_real_sleep() {
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::HangForever]));
    let app = test_app(Arc::clone(&backend), 5);

    let req = json!({
        "model": "claude-sonnet-5-5-high",
        "messages": [{ "role": "user", "content": "hang" }]
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(backend.aborted_count.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn codex_custom_apply_patch_and_get_stored_response() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![
                "<agy_tool_call_{NONCE}>{\"name\":\"apply_patch\",\"input\":\"*** Begin Patch\\n*** End Patch\"}</agy_tool_call_{NONCE}>".to_owned(),
            ],
        },
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Patch applied cleanly.".to_owned()],
        },
    ]));

    let app = test_app(Arc::clone(&backend), 30);

    let req1 = json!({
        "model": "gpt-5-codex",
        "stream": false,
        "input": [{ "role": "user", "content": "Apply patch" }],
        "tools": [
            {
                "type": "custom",
                "name": "apply_patch",
                "description": "Freeform patch tool"
            }
        ]
    });

    let resp1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(req1.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);
    let body1: Value =
        serde_json::from_slice(&resp1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let resp_id = body1["id"].as_str().unwrap().to_owned();
    let output_arr = body1["output"].as_array().unwrap();
    let custom_item = output_arr
        .iter()
        .find(|item| item["type"] == "custom_tool_call")
        .unwrap();
    assert_eq!(custom_item["name"], "apply_patch");
    assert_eq!(custom_item["input"], "*** Begin Patch\n*** End Patch");
    let call_id = custom_item["call_id"].as_str().unwrap().to_owned();

    // Retrieve stored response via GET `/v1/responses/{response_id}`
    let get_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/responses/{resp_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_resp.status(), StatusCode::OK);
    let fetched: Value =
        serde_json::from_slice(&get_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(fetched["id"], resp_id);

    // Send `custom_tool_call_output` in Turn 2
    let req2 = json!({
        "model": "gpt-5-codex",
        "previous_response_id": resp_id,
        "stream": false,
        "input": [
            {
                "type": "custom_tool_call_output",
                "call_id": call_id,
                "output": "Success. Updated 1 file."
            }
        ]
    });
    let resp2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(req2.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn models_count_tokens_and_api_key_enforcement() {
    let backend = Arc::new(MockAcpBackend::new(vec![]));
    let app = test_app_with_key(backend, 30, Some("secret-key"));

    // Missing API key -> 401
    let unauth = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    // Bearer token -> 200 `/v1/models`
    let models_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/models")
                .header("authorization", "Bearer secret-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(models_resp.status(), StatusCode::OK);
    let models_json: Value =
        serde_json::from_slice(&models_resp.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    let live_count = models_json["data"].as_array().unwrap().len();
    assert!(live_count > 0);
    let carbon_card = models_json["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "gemini-4-carbon-high")
        .expect("gemini-4-carbon-high in /v1/models");
    assert_eq!(carbon_card["context_window"], 500_000);
    assert_eq!(carbon_card["auto_compact_token_limit"], 425_000);
    assert_eq!(
        carbon_card["description"],
        "500k ctx · Antigravity ACP (gemini-4-carbon-high)"
    );
    assert_eq!(
        carbon_card["chat"]["modalities"]["input"],
        serde_json::json!(["text", "image"])
    );
    assert_eq!(
        carbon_card["chat"]["modalities"]["output"],
        serde_json::json!(["text"])
    );

    // Bearer token -> 200 `/v1/codex-models` full Codex picker catalog replacement
    let codex_catalog_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/codex-models?default_model=claude-sonnet-5-5-high")
                .header("authorization", "Bearer secret-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(codex_catalog_resp.status(), StatusCode::OK);
    let codex_catalog_json: Value = serde_json::from_slice(
        &codex_catalog_resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    let codex_models = codex_catalog_json["models"].as_array().unwrap();
    assert_eq!(codex_models.len(), live_count);
    assert_eq!(codex_models[0]["slug"], "claude-sonnet-5-5-high");
    assert_eq!(codex_models[0]["priority"], 0);
    assert_eq!(codex_models[0]["visibility"], "list");
    assert_eq!(codex_models[0]["shell_type"], "shell_command");
    assert_eq!(codex_models[0]["default_reasoning_level"], "high");
    assert_eq!(codex_models[0]["apply_patch_tool_type"], "freeform");
    assert_eq!(codex_models[0]["context_window"], 200_000);
    assert_eq!(codex_models[0]["truncation_policy"]["limit"], 40_000);
    let codex_carbon = codex_models
        .iter()
        .find(|m| m["slug"] == "gemini-4-carbon-high")
        .expect("gemini-4-carbon-high in /v1/codex-models");
    assert_eq!(codex_carbon["context_window"], 500_000);
    assert_eq!(codex_carbon["max_context_window"], 500_000);
    assert_eq!(codex_carbon["auto_compact_token_limit"], 425_000);
    assert_eq!(codex_carbon["truncation_policy"]["limit"], 80_000);

    // `x-api-key` -> 200 `/v1/messages/count_tokens`
    let count_req = json!({
        "model": "claude-opus-4-6",
        "messages": [
            { "role": "user", "content": "Hello Liftgate!" },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": [{
                        "type": "image",
                        "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" }
                    }]
                }]
            }
        ]
    });
    let count_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages/count_tokens")
                .header("x-api-key", "secret-key")
                .header("content-type", "application/json")
                .body(Body::from(count_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(count_resp.status(), StatusCode::OK);
    let count_json: Value =
        serde_json::from_slice(&count_resp.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert!(count_json["input_tokens"].as_u64().unwrap() >= 256);
    assert!(count_json["input_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test(start_paused = true)]
async fn sse_client_drop_cancels_upstream_session() {
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::HangForever]));
    let app = test_app(Arc::clone(&backend), 120);

    let req = json!({
        "model": "claude-sonnet-5-5-high",
        "stream": true,
        "messages": [{ "role": "user", "content": "start streaming and disconnect" }]
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Dropping the SSE response body drops `ReceiverStream`, closing `sse_rx`, which
    // immediately wakes `sse_tx.closed()` -> drops `ActiveTurnStream` (`DropGuard`) ->
    // cancels `run_turn_pipeline` -> drops `SessionLeaseGuard` -> aborts session without waiting for timeout!
    drop(resp);
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    assert_eq!(backend.aborted_count.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn acp_web_search_split_delta_merges_into_responses_item() {
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::WebSearchSplitDelta {
        query: "rust 2024 edition".to_owned(),
        answer: "Grounded answer.".to_owned(),
    }]));
    let app = test_app(backend, 30);

    let req = json!({
        "model": "gemini-3.8-flash-high",
        "stream": false,
        "input": "Search Rust 2024",
        "tools": [{ "type": "web_search_preview" }]
    });

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let out = body["output"].as_array().unwrap();
    let ws = out
        .iter()
        .find(|i| i["type"] == "web_search_call")
        .expect("expected merged web_search_call output item");
    assert_eq!(ws["action"]["query"], "rust 2024 edition");

    // Verify `/api/telemetry/snapshot` recorded the completed turn, tokens, and idle session
    let snap_resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/telemetry/snapshot")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(snap_resp.status(), StatusCode::OK);
    let snap: Value =
        serde_json::from_slice(&snap_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(snap["metrics"]["completed_requests"], 1);
    assert_eq!(snap["metrics"]["total_input_tokens"], 80);
    assert_eq!(snap["session_pool"]["idle_cached_sessions"], 1);
    assert_eq!(snap["recent_tasks"][0]["phase"], "completed");
}

#[tokio::test(start_paused = true)]
async fn claude_code_web_search_three_request_flow_and_dynamic_billing_headers() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        // req_1: main agent emits client `WebSearch` tool_use with omitted adaptive thinking
        MockScript::Immediate {
            thoughts: vec!["Hidden adaptive reasoning".to_owned()],
            chunks_template: vec![
                "<agy_tool_call_{NONCE}>{\"name\":\"WebSearch\",\"arguments\":{\"query\":\"Rust 2024 edition\"}}</agy_tool_call_{NONCE}>".to_owned(),
            ],
        },
        // req_2: Claude Code WebSearch sub-request (`web_search_20250305` + forced `tool_choice`)
        MockScript::WebSearchSplitDelta {
            query: "Rust 2024 edition".to_owned(),
            answer: "Rust 2024 stabilized async closures.\n\nSources:\n- [Rust 2024 Guide](https://doc.rust-lang.org/edition-guide/rust-2024/index.html)".to_owned(),
        },
        // req_3: Turn 2 reuses `sess_1` despite changed billing nonce + `<total_tokens>`, and strips ACP retry banner
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![
                "\n⚠ API error (attempt 1): transient 503 — retrying in 1s...\n".to_owned(),
                "Rust 2024 includes async closures ([Rust 2024 Guide](https://doc.rust-lang.org/edition-guide/rust-2024/index.html)).".to_owned(),
            ],
        },
    ]));

    let app = test_app(Arc::clone(&backend), 30);
    let client_tools = json!([
        {
            "name": "WebSearch",
            "description": "Search the web",
            "input_schema": {
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"],
                "additionalProperties": false
            }
        }
    ]);

    // 1. Turn 1 (`req_1`)
    let req1 = json!({
        "model": "gemini-4-carbon-high",
        "max_tokens": 64000,
        "thinking": { "type": "adaptive", "display": "omitted" },
        "output_config": { "effort": "high" },
        "stream": true,
        "system": [
            { "type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.296.16a; cc_entrypoint=sdk-cli;" },
            { "type": "text", "text": "You are a Claude agent, built on Anthropic's Claude Agent SDK." },
            { "type": "text", "text": "Helpful coding assistant.\n<total_tokens>15000000 tokens left</total_tokens>" }
        ],
        "tools": client_tools,
        "messages": [
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": "<system-reminder>\n<total_tokens>15000000 tokens left</total_tokens>\n</system-reminder>" },
                    { "type": "text", "text": "Search Rust 2024 edition" }
                ]
            }
        ]
    });

    let resp1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages?beta=true")
                .header("content-type", "application/json")
                .body(Body::from(req1.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);
    let sse1 = String::from_utf8(
        resp1
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(
        !sse1.contains("\"thinking_delta\""),
        "display=omitted must suppress raw thinking SSE deltas"
    );
    assert!(sse1.contains("\"name\":\"WebSearch\""));

    let tool_start_line = sse1
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .find(|d| d.contains("\"type\":\"tool_use\""))
        .unwrap();
    let tool_start_json: Value = serde_json::from_str(tool_start_line).unwrap();
    let toolu_id = tool_start_json["content_block"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // 2. Sub-request (`req_2`) from Claude Code's WebSearch executor
    let req2 = json!({
        "model": "gemini-4-carbon-high",
        "max_tokens": 64000,
        "thinking": { "type": "disabled" },
        "output_config": { "effort": "high" },
        "stream": true,
        "system": [
            { "type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.296.4f2; cc_entrypoint=sdk-cli;" },
            { "type": "text", "text": "You run a web search for another Claude model." }
        ],
        "tools": [{ "type": "web_search_20250305", "name": "web_search", "max_uses": 8 }],
        "tool_choice": { "type": "tool", "name": "web_search" },
        "messages": [
            {
                "role": "user",
                "content": [{ "type": "text", "text": "Perform a web search for the query: Rust 2024 edition" }]
            }
        ]
    });

    let resp2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages?beta=true")
                .header("content-type", "application/json")
                .body(Body::from(req2.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let sse2 = String::from_utf8(
        resp2
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(sse2.contains("Rust 2024 Guide"));
    assert!(sse2.contains("\"type\":\"server_tool_use\""));
    assert!(sse2.contains("\"type\":\"web_search_tool_result\""));
    assert!(
        sse2.contains("\"url\":\"https://doc.rust-lang.org/edition-guide/rust-2024/index.html\"")
    );
    assert!(sse2.contains("\"web_search_requests\":1"));

    // 3. Turn 2 (`req_3`) with mutated billing header & mutated `<total_tokens>`
    let req3 = json!({
        "model": "gemini-4-carbon-high",
        "max_tokens": 64000,
        "thinking": { "type": "adaptive", "display": "omitted" },
        "output_config": { "effort": "high" },
        "stream": false,
        "system": [
            { "type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.296.693; cc_entrypoint=sdk-cli;" },
            { "type": "text", "text": "You are a Claude agent, built on Anthropic's Claude Agent SDK." },
            { "type": "text", "text": "Helpful coding assistant.\n<total_tokens>14999975 tokens left</total_tokens>" }
        ],
        "tools": client_tools,
        "messages": [
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": "<system-reminder>\n<total_tokens>14999975 tokens left</total_tokens>\n</system-reminder>" },
                    { "type": "text", "text": "Search Rust 2024 edition" }
                ]
            },
            {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": toolu_id, "name": "WebSearch", "input": { "query": "Rust 2024 edition" } }
                ]
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": toolu_id,
                        "content": "Web search results...\n<system-reminder>\n<total_tokens>14999975 tokens left</total_tokens>\n</system-reminder>"
                    }
                ]
            }
        ]
    });

    let resp3 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages?beta=true")
                .header("content-type", "application/json")
                .body(Body::from(req3.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp3.status(), StatusCode::OK);
    let body3: Value =
        serde_json::from_slice(&resp3.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let reply_text = body3["content"][0]["text"].as_str().unwrap();
    assert!(!reply_text.contains("⚠ API error"));
    assert!(reply_text.contains("Rust 2024 includes async closures"));

    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 3);
    assert_eq!(prompts[0].session_id, "sess_1");
    assert_eq!(prompts[1].session_id, "sess_2");
    assert_eq!(
        prompts[2].session_id, "sess_1",
        "Turn 2 must reuse sess_1 despite mutated billing header and <total_tokens>"
    );
}

#[tokio::test(start_paused = true)]
async fn claude_code_parallel_tool_calls_and_effort_override() {
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::Immediate {
        thoughts: vec![],
        chunks_template: vec![
            "Reading both files in parallel.\n<agy_tool_call_{NONCE}>{\"name\":\"Read\",\"arguments\":{\"file_path\":\"Cargo.toml\"}}</agy_tool_call_{NONCE}>\n<agy_tool_call_{NONCE}>{\"name\":\"Read\",\"arguments\":{\"file_path\":\"web/package.json\"}}</agy_tool_call_{NONCE}>".to_owned(),
        ],
    }]));

    let app = test_app(Arc::clone(&backend), 30);
    let req = json!({
        "model": "gemini-4-carbon-high",
        "max_tokens": 32000,
        "thinking": { "type": "adaptive", "display": "omitted" },
        "output_config": { "effort": "medium" },
        "stream": false,
        "tools": [
            {
                "name": "Read",
                "description": "Read file",
                "input_schema": { "type": "object", "properties": { "file_path": { "type": "string" } } }
            }
        ],
        "messages": [{ "role": "user", "content": "Read Cargo.toml and web/package.json" }]
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["stop_reason"], "tool_use");
    let content = body["content"].as_array().unwrap();
    let tool_blocks: Vec<&Value> = content.iter().filter(|b| b["type"] == "tool_use").collect();
    assert_eq!(tool_blocks.len(), 2);

    let models = backend.recorded_models.lock().unwrap();
    assert_eq!(
        models.first().map(String::as_str),
        Some("gemini-4-carbon-medium")
    );
}

#[tokio::test(start_paused = true)]
async fn claude_code_server_side_web_fetch_loop_and_ssrf_protection() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![
                "Let me fetch the official weather page.\n<agy_tool_call_{NONCE}>{\"name\":\"__gateway_web_fetch\",\"arguments\":{\"url\":\"https://www.weather.gov.sg/weather-forecast-24hrforecast/\"}}</agy_tool_call_{NONCE}>\nHallucinated pre-fetch text that must be suppressed".to_owned(),
            ],
        },
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![
                "Official forecast: Partly cloudy, 25C to 31C ([MSS](https://www.weather.gov.sg/weather-forecast-24hrforecast/)).".to_owned(),
            ],
        },
    ]));

    let app = test_app(Arc::clone(&backend), 30);
    let req = json!({
        "model": "gemini-4-carbon-high",
        "max_tokens": 16000,
        "stream": true,
        "tools": [
            {
                "type": "web_fetch_20250910",
                "name": "web_fetch",
                "max_uses": 3
            }
        ],
        "messages": [
            { "role": "user", "content": "Fetch https://www.weather.gov.sg/weather-forecast-24hrforecast/" }
        ]
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let sse = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();

    assert!(sse.contains("\"type\":\"server_tool_use\""));
    assert!(sse.contains("\"name\":\"web_fetch\""));
    assert!(sse.contains("\"type\":\"web_fetch_tool_result\""));
    assert!(sse.contains("Singapore 24-Hour Official Forecast"));
    assert!(sse.contains("\"web_fetch_requests\":1"));
    assert!(!sse.contains("Hallucinated pre-fetch text"));
    assert!(sse.contains("Official forecast: Partly cloudy, 25C to 31C"));

    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0].session_id, "sess_1");
    assert_eq!(prompts[1].session_id, "sess_1");
    assert!(
        prompts[1]
            .text_joined
            .contains("<gateway_web_fetch_result url=\"https://www.weather.gov.sg/weather-forecast-24hrforecast/\"")
    );
}

#[tokio::test(start_paused = true)]
async fn auto_mode_classifier_and_away_recap_route_to_flash_without_stealing_session() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        // 1. Main interactive Turn 1 on `gemini-4-hydrogen-xhigh`
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![
                "<agy_tool_call_{NONCE}>{\"name\":\"Bash\",\"arguments\":{\"command\":\"cargo test\"}}</agy_tool_call_{NONCE}>".to_owned(),
            ],
        },
        // 2. Claude Code Auto Mode Stage 1 classifier (`stream: false`)
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["<block>no</block>".to_owned()],
        },
        // 3. Claude Code background away-recap sharing main session prefix
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Running cargo test in workspace.".to_owned()],
        },
        // 4. Main interactive Turn 2 continuing `sess_1`
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["All tests passed!".to_owned()],
        },
    ]));

    let app = test_app(Arc::clone(&backend), 30);
    let tools = json!([
        {
            "name": "Bash",
            "description": "Execute bash command",
            "input_schema": { "type": "object", "properties": { "command": { "type": "string" } } }
        }
    ]);

    // 1. Interactive Turn 1 (`cch=11111`)
    let turn1 = json!({
        "model": "gemini-4-hydrogen-xhigh",
        "stream": false,
        "system": "x-anthropic-billing-header: cc_version=2.1.296; cch=11111;\nMain coding prompt; cch=11111;",
        "tools": tools,
        "messages": [{ "role": "user", "content": "Run cargo test" }]
    });
    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(turn1.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let b1: Value =
        serde_json::from_slice(&r1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let toolu_id = b1["content"][0]["id"].as_str().unwrap().to_owned();

    // 2. Auto Mode Stage 1 classifier requesting heavy `gemini-4-carbon-high` -> must override to `gemini-4-hydrogen-low`!
    let auto_req = json!({
        "model": "gemini-4-carbon-high",
        "stream": false,
        "thinking": { "type": "adaptive", "display": "omitted" },
        "system": "Transcript classifier for auto mode.",
        "messages": [
            {
                "role": "user",
                "content": "Err on the side of blocking. Stage 1 does NOT apply user intent or ALLOW exceptions — stage 2 will handle those.\nRespond with <block>yes</block> or <block>no</block>."
            }
        ]
    });
    let r_auto = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(auto_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let b_auto: Value =
        serde_json::from_slice(&r_auto.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(b_auto["content"][0]["text"], "<block>no</block>");

    // 3. Forked away-recap sharing main session history -> must NOT steal `sess_1`!
    let recap_req = json!({
        "model": "gemini-4-hydrogen-xhigh",
        "stream": false,
        "system": "x-anthropic-billing-header: cc_version=2.1.296; cch=22222;\nMain coding prompt; cch=22222;",
        "messages": [
            { "role": "user", "content": "Run cargo test" },
            { "role": "assistant", "content": [{ "type": "tool_use", "id": toolu_id, "name": "Bash", "input": { "command": "cargo test" } }] },
            { "role": "user", "content": "The user stepped away and is coming back. Recap in under 40 words." }
        ]
    });
    let r_recap = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(recap_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_recap.status(), StatusCode::OK);

    // 4. Interactive Turn 2 (`cch=99999`) -> must STILL reuse `sess_1`!
    let turn2 = json!({
        "model": "gemini-4-hydrogen-xhigh",
        "stream": false,
        "system": "x-anthropic-billing-header: cc_version=2.1.296; cch=99999;\nMain coding prompt; cch=99999;",
        "tools": tools,
        "messages": [
            { "role": "user", "content": "Run cargo test" },
            { "role": "assistant", "content": [{ "type": "tool_use", "id": toolu_id, "name": "Bash", "input": { "command": "cargo test" } }] },
            { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": toolu_id, "content": "test result: ok." }] }
        ]
    });
    let r2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(turn2.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);

    let models = backend.recorded_models.lock().unwrap().clone();
    assert_eq!(
        models,
        vec![
            "gemini-4-hydrogen-xhigh".to_owned(),
            "gemini-4-hydrogen-low".to_owned(),
            "gemini-4-hydrogen-low".to_owned(),
        ]
    );
    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts[0].session_id, "sess_1");
    assert_eq!(prompts[1].session_id, "sess_2");
    assert_eq!(prompts[2].session_id, "sess_3");
    assert_eq!(
        prompts[3].session_id, "sess_1",
        "Main interactive Turn 2 must reuse sess_1 untouched by Auto Mode & away-recap"
    );
}

#[tokio::test(start_paused = true)]
async fn runaway_post_tool_hallucination_and_malformed_call_banner_cutoff_immediately() {
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::ChunksThenHang {
        chunks_template: vec![
            "</client_tool_result>\n\n</user>\n<assistant>\n".to_owned(),
            "Checking repository status.\n<agy_tool_call_{NONCE}>{\"name\":\"Bash\",\"arguments\":{\"command\":\"git status\"}}</agy_tool_call_{NONCE}>\n".to_owned(),
            "⚠ Your previous response contained an improperly formatted function call\nPlease retry with a properly formatted function call\nRetries remaining: 3\n".to_owned(),
            // Duplicate retried tool call from internal ACP retry -> must be deduplicated
            "<agy_tool_call_{NONCE}>{\"name\":\"Bash\",\"arguments\":{\"command\":\"git status\"}}</agy_tool_call_{NONCE}>\n".to_owned(),
            // Runaway multi-turn hallucination -> must trigger instant turn completion WITHOUT hanging on the 3600s sleep!
            "\n</assistant>\n<user>\n<client_tool_result call_id=\"toolu_fake\" status=\"ok\">\nhallucinated output\n</client_tool_result>\n</user>\n<assistant>\n<agy_tool_call_{NONCE}>{\"name\":\"Bash\",\"arguments\":{\"command\":\"rm -rf fake\"}}</agy_tool_call_{NONCE}>".to_owned(),
        ],
    }]));

    // 120s turn timeout, yet `ChunksThenHang` sleeps 3600s: if runaway cutoff did not exit
    // immediately, this request would time out with 504 instead of returning 200 OK!
    let app = test_app(Arc::clone(&backend), 120);
    let req = json!({
        "model": "gemini-4-hydrogen-xhigh",
        "stream": false,
        "tools": [
            {
                "name": "Bash",
                "description": "Run command",
                "input_schema": { "type": "object", "properties": { "command": { "type": "string" } } }
            }
        ],
        "messages": [{ "role": "user", "content": "Check git status" }]
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();

    assert_eq!(body["stop_reason"], "tool_use");
    let content = body["content"].as_array().unwrap();
    let text_blocks: Vec<&str> = content
        .iter()
        .filter(|b| b["type"] == "text")
        .map(|b| b["text"].as_str().unwrap())
        .collect();
    assert_eq!(text_blocks, vec!["Checking repository status."]);

    let tool_blocks: Vec<&Value> = content.iter().filter(|b| b["type"] == "tool_use").collect();
    assert_eq!(
        tool_blocks.len(),
        1,
        "duplicate retry tool call and post-</assistant> fake tool call must both be dropped"
    );
    assert_eq!(tool_blocks[0]["input"]["command"], "git status");
    assert_eq!(
        backend.aborted_count.load(Ordering::SeqCst),
        1,
        "tainted runaway session must be torn down immediately in the background"
    );
}

#[tokio::test(start_paused = true)]
async fn openai_codex_local_shell_encrypted_reasoning_and_mid_turn_developer_prefix_reuse() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        MockScript::Immediate {
            thoughts: vec!["Inspecting workspace".to_owned()],
            chunks_template: vec![
                "<agy_tool_call_{NONCE}>{\"name\":\"__codex_local_shell\",\"arguments\":{\"command\":[\"git\",\"status\"],\"working_directory\":\"D:/rust_pro/agy\"}}</agy_tool_call_{NONCE}>".to_owned(),
            ],
        },
        MockScript::WebSearchSplitDelta {
            query: "codex responses api".to_owned(),
            answer: "Clean working tree; see [OpenAI Docs](https://platform.openai.com/docs/api-reference/responses).".to_owned(),
        },
    ]));

    let app = test_app(Arc::clone(&backend), 30);
    let tools = json!([
        { "type": "local_shell" },
        { "type": "web_search" }
    ]);

    let req1 = json!({
        "model": "gemini-4-carbon-high",
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "instructions": "You are Codex.",
        "tools": tools,
        "input": [
            { "role": "user", "content": "Run git status and check docs" }
        ]
    });

    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(req1.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let sse1 =
        String::from_utf8(r1.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(sse1.contains("\"encrypted_content\":\"agy_reasoning_v1\""));
    assert!(sse1.contains("\"type\":\"local_shell_call\""));

    let done_line = sse1
        .lines()
        .rev()
        .filter_map(|l| l.strip_prefix("data: "))
        .find(|d| d.contains("\"type\":\"response.completed\""))
        .unwrap();
    let done_val: Value = serde_json::from_str(done_line).unwrap();
    let lsh_item = done_val["response"]["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "local_shell_call")
        .unwrap();
    let call_id = lsh_item["call_id"].as_str().unwrap().to_owned();

    // Turn 2: Stateless Codex replay with mid-turn `developer` permissions block!
    let req2 = json!({
        "model": "gemini-4-carbon-high",
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "instructions": "You are Codex.",
        "tools": tools,
        "input": [
            { "role": "user", "content": "Run git status and check docs" },
            {
                "type": "local_shell_call",
                "call_id": call_id,
                "action": {
                    "type": "exec",
                    "command": ["git", "status"],
                    "working_directory": "D:/rust_pro/agy"
                }
            },
            { "role": "developer", "content": "<permissions instructions>sandbox=workspace-write</permissions instructions>" },
            {
                "type": "local_shell_call_output",
                "call_id": call_id,
                "output": "On branch main, nothing to commit"
            }
        ]
    });

    let r2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(req2.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let sse2 =
        String::from_utf8(r2.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(sse2.contains("event: response.web_search_call.in_progress"));
    assert!(sse2.contains("event: response.web_search_call.searching"));
    assert!(sse2.contains("event: response.web_search_call.completed"));
    assert!(sse2.contains("\"type\":\"url_citation\""));

    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0].session_id, "sess_1");
    assert_eq!(
        prompts[1].session_id, "sess_1",
        "Codex mid-turn developer item must NOT mutate system_instructions or break SessionPool prefix hash"
    );
}

#[tokio::test(start_paused = true)]
async fn codex_0_166_massive_developer_prompt_namespace_tools_and_429_no_silent_failover() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        // 1. Primary `gemini-4-carbon-high` (`sess_1`) emits 429 banner WITHOUT trailing newline
        //    and attempts to hang 3600s -> gateway must abort in 0ms and surface 429 directly
        //    WITHOUT silently failing over to another model!
        MockScript::ChunksThenHang {
            chunks_template: vec![
                "⚠ API error (attempt 1/8): RESOURCE_EXHAUSTED (code 429): No capacity available"
                    .to_owned(),
            ],
        },
        // 2. Non-streaming retry also hits 429 -> returns HTTP 429 `rate_limit_error` directly.
        MockScript::ChunksThenHang {
            chunks_template: vec![
                "⚠ API error (attempt 1/8): RESOURCE_EXHAUSTED (code 429): No capacity available"
                    .to_owned(),
            ],
        },
        // 3. Caller retries on `gemini-4-carbon-high` (`sess_3`), succeeds, and calls
        //    a namespaced Codex tool (`multi_agent_v1.spawn_agent`)!
        MockScript::Immediate {
            thoughts: vec!["Delegating sub-task".to_owned()],
            chunks_template: vec![
                "<agy_tool_call_{NONCE}>{\"name\":\"multi_agent_v1.spawn_agent\",\"arguments\":{\"task\":\"audit workspace\"}}</agy_tool_call_{NONCE}>".to_owned(),
            ],
        },
        // 4. Concurrent Codex TUI task-title generator -> routed to `gemini-4-hydrogen-low` (`sess_4`)
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Audit workspace codebase".to_owned()],
        },
        // 5. Turn 2 continuation with echoed `namespace: "multi_agent_v1"` -> reuses warm `sess_3`!
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Audit finished cleanly.".to_owned()],
        },
    ]));

    let app = test_app(Arc::clone(&backend), 120);
    let massive_dev = format!(
        "You are a coding agent running in the Codex CLI, a terminal-based coding assistant. {}",
        "Follow workspace engineering conventions carefully. ".repeat(380)
    );
    let tools = json!([
        {
            "type": "function",
            "name": "exec_command",
            "description": "Execute shell command",
            "parameters": { "type": "object", "properties": { "cmd": { "type": "string" } } }
        },
        {
            "type": "namespace",
            "name": "multi_agent_v1",
            "description": "Multi-agent orchestration tools",
            "tools": [
                {
                    "type": "function",
                    "name": "spawn_agent",
                    "description": "Spawn a worker agent",
                    "defer_loading": true,
                    "parameters": { "type": "object", "properties": { "task": { "type": "string" } } }
                }
            ]
        },
        {
            "type": "namespace",
            "name": "mcp__codex_apps__pets",
            "description": "Deferred remote Pets connector",
            "tools": [
                {
                    "type": "function",
                    "name": "adopt_pet",
                    "description": "Adopt a virtual pet",
                    "defer_loading": true,
                    "parameters": { "type": "object", "properties": {} }
                }
            ]
        }
    ]);

    // Attempt 1 (streaming): hits 429 -> must emit `response.failed` (`rate_limit_exceeded`) in 0ms
    // with ZERO silent model failover!
    let req1 = json!({
        "model": "gemini-4-carbon-high",
        "stream": true,
        "reasoning": { "effort": "xhigh", "summary": "auto" },
        "tools": tools,
        "input": [
            { "role": "developer", "content": massive_dev },
            { "role": "user", "content": "hi, audit the workspace" }
        ]
    });

    let r_429_stream = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(req1.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_429_stream.status(), StatusCode::OK);
    let sse_429 = String::from_utf8(
        r_429_stream
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(sse_429.contains("event: response.failed"));
    assert!(sse_429.contains("\"code\":\"rate_limit_exceeded\""));
    assert!(!sse_429.contains("claude-sonnet"));

    // Attempt 2 (non-streaming): hits 429 -> must return HTTP 429 `rate_limit_error` directly.
    let mut req1_sync = req1.clone();
    req1_sync["stream"] = json!(false);
    let r_429_sync = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(req1_sync.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_429_sync.status(), StatusCode::TOO_MANY_REQUESTS);
    let b_429: Value =
        serde_json::from_slice(&r_429_sync.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(b_429["error"]["type"], "rate_limit_error");
    assert_eq!(b_429["error"]["code"], "rate_limit_exceeded");

    // Attempt 3: Caller retries on `gemini-4-carbon-high` (`sess_3`) and succeeds!
    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(req1.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let sse1 =
        String::from_utf8(r1.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(sse1.contains("\"namespace\":\"multi_agent_v1\""));
    assert!(sse1.contains("\"name\":\"spawn_agent\""));

    let done_line = sse1
        .lines()
        .rev()
        .filter_map(|l| l.strip_prefix("data: "))
        .find(|d| d.contains("\"type\":\"response.completed\""))
        .unwrap();
    let done_val: Value = serde_json::from_str(done_line).unwrap();
    let fc_item = done_val["response"]["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .unwrap();
    assert_eq!(fc_item["namespace"], "multi_agent_v1");
    assert_eq!(fc_item["name"], "spawn_agent");
    let call_id = fc_item["call_id"].as_str().unwrap().to_owned();

    // Concurrent Codex TUI title generator (carrying tools + developer prompt)
    let title_req = json!({
        "model": "gemini-4-carbon-high",
        "stream": false,
        "reasoning": { "effort": "xhigh" },
        "tools": tools,
        "input": [
            { "role": "developer", "content": "You generate short task titles." },
            { "role": "user", "content": "Generate a concise, single-line task title of at most 36 characters and under five words where possible:\nhi, audit the workspace" }
        ]
    });
    let r_title = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(title_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_title.status(), StatusCode::OK);

    // Turn 2: Codex echoes `namespace: "multi_agent_v1", name: "spawn_agent"` + `function_call_output`
    let req2 = json!({
        "model": "gemini-4-carbon-high",
        "stream": false,
        "reasoning": { "effort": "xhigh", "summary": "auto" },
        "tools": tools,
        "input": [
            { "role": "developer", "content": massive_dev },
            { "role": "user", "content": "hi, audit the workspace" },
            {
                "type": "function_call",
                "call_id": call_id,
                "namespace": "multi_agent_v1",
                "name": "spawn_agent",
                "arguments": "{\"task\":\"audit workspace\"}"
            },
            {
                "type": "function_call_output",
                "call_id": call_id,
                "output": "worker spawned ok"
            }
        ]
    });
    let r2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(req2.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let b2: Value =
        serde_json::from_slice(&r2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        b2["output"][0]["content"][0]["text"],
        "Audit finished cleanly."
    );

    // Verify invariants:
    // 1) `_meta.antigravity.systemPrompt.append` NEVER contained Codex's 20KB developer prompt,
    //    unpacked `multi_agent_v1.spawn_agent`, omitted `mcp__codex_apps__pets.adopt_pet`,
    //    and stripped all tools from the `sess_3` title-generator call!
    let appends = backend.recorded_system_appends.lock().unwrap().clone();
    assert_eq!(appends.len(), 4);
    assert!(
        !appends[0].contains("You are a coding agent running in the Codex CLI"),
        "massive Codex developer prompt must be moved out of systemPrompt.append"
    );
    assert!(appends[0].contains("multi_agent_v1.spawn_agent"));
    assert!(
        !appends[0].contains("adopt_pet"),
        "unused mcp__codex_apps__* bundles must remain deferred"
    );
    assert!(
        !appends[3].contains("\n<client_tools>\n") && !appends[3].contains("exec_command"),
        "auxiliary Codex title-generator call must strip client tool schemas"
    );

    // 2) Exact `-high` model was NOT mutated to `-xhigh`, NEVER silently failed over to
    //    `claude-sonnet-5-5-high` on 429, title call routed to `gemini-4-hydrogen-low`, and Turn 2
    //    reused warm `sess_3` on `gemini-4-carbon-high`!
    let models = backend.recorded_models.lock().unwrap().clone();
    assert_eq!(
        models,
        vec![
            "gemini-4-carbon-high".to_owned(),
            "gemini-4-carbon-high".to_owned(),
            "gemini-4-carbon-high".to_owned(),
            "gemini-4-hydrogen-low".to_owned(),
        ]
    );

    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 5);
    assert_eq!(prompts[0].session_id, "sess_1");
    assert!(prompts[0].text_joined.contains("<developer_instructions>"));
    assert_eq!(prompts[1].session_id, "sess_2");
    assert_eq!(prompts[2].session_id, "sess_3");
    assert_eq!(prompts[3].session_id, "sess_4");
    assert_eq!(
        prompts[4].session_id, "sess_3",
        "Turn 2 must reuse warm sess_3 with only the tool output delta"
    );
    assert!(prompts[4].text_joined.contains("worker spawned ok"));
    assert!(
        !prompts[4]
            .text_joined
            .contains("You are a coding agent running in the Codex CLI")
    );
}

#[tokio::test(start_paused = true)]
async fn codex_web_search_live_enables_server_side_open_page_web_fetch() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        MockScript::Immediate {
            thoughts: vec!["Opening official Singapore forecast URL".to_owned()],
            chunks_template: vec![
                "<agy_tool_call_{NONCE}>{\"name\":\"__gateway_web_fetch\",\"arguments\":{\"url\":\"https://www.weather.gov.sg/weather-forecast-24hrforecast/\"}}</agy_tool_call_{NONCE}>\n</assistant>\n<client_tool_result>fake</client_tool_result>".to_owned(),
            ],
        },
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![
                "Singapore forecast from [MSS](https://www.weather.gov.sg/weather-forecast-24hrforecast/): 25C-31C.".to_owned(),
            ],
        },
    ]));

    let app = test_app(Arc::clone(&backend), 30);
    let req = json!({
        "model": "gemini-4-carbon-high",
        "stream": true,
        "tools": [
            {
                "type": "web_search",
                "external_web_access": true
            }
        ],
        "input": [
            { "role": "user", "content": "Read https://www.weather.gov.sg/weather-forecast-24hrforecast/" }
        ]
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let sse = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();

    assert!(sse.contains("\"type\":\"web_search_call\""));
    assert!(sse.contains("\"type\":\"open_page\""));
    assert!(sse.contains("\"url\":\"https://www.weather.gov.sg/weather-forecast-24hrforecast/\""));
    assert!(sse.contains("event: response.web_search_call.in_progress"));
    assert!(sse.contains("event: response.web_search_call.completed"));
    assert!(!sse.contains("fake"));
    assert!(sse.contains("Singapore forecast from [MSS]"));
    assert!(sse.contains("\"type\":\"url_citation\""));
}

#[tokio::test(start_paused = true)]
async fn individual_quota_reached_banner_exposes_non_retryable_error_without_empty_session_poisoning()
 {
    let quota_banner = "⚠ Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 48m36s.\n".to_owned();
    let backend = Arc::new(MockAcpBackend::new(vec![
        // 1. Claude Code streaming turn hits individual quota -> must emit `event: error`
        //    (`invalid_request_error`), skip failover, and NOT poison SessionPool!
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![quota_banner.clone()],
        },
        // 2. Non-streaming `/v1/messages` turn hits individual quota -> 400 `invalid_request_error`
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![quota_banner.clone()],
        },
        // 3. Codex streaming `/v1/responses` turn hits individual quota -> `response.failed` (`insufficient_quota`)
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![quota_banner.clone()],
        },
        // 4. Codex non-streaming `/v1/responses` turn -> 429 `insufficient_quota`
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![quota_banner],
        },
        // 5. After quota resets, same Claude Code session history recovers cleanly on a fresh session!
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Quota reset; committed and pushed!".to_owned()],
        },
    ]));

    let app = test_app(Arc::clone(&backend), 30);

    // 1. Streaming `/v1/messages` (Claude Code interactive turn)
    let cc_stream_req = json!({
        "model": "gemini-4-carbon-high",
        "stream": true,
        "messages": [{ "role": "user", "content": "你问他解决完了就帮忙提交并推送啊" }]
    });
    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages?beta=true")
                .header("content-type", "application/json")
                .body(Body::from(cc_stream_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let sse1 =
        String::from_utf8(r1.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(
        !sse1.contains("event: message_start"),
        "pre-output quota error must not emit an empty message_start: {sse1}"
    );
    assert!(sse1.contains("event: error"));
    assert!(sse1.contains("\"type\":\"invalid_request_error\""));
    assert!(sse1.contains("Individual quota reached"));
    assert!(sse1.contains("Resets in 48m36s."));

    // Verify no silent model failover occurred (`recorded_models` has only the 1 requested model)
    // and SessionPool has 0 idle sessions (never committed an empty assistant turn!).
    assert_eq!(
        backend.recorded_models.lock().unwrap().len(),
        1,
        "account quota exhaustion must stop immediately without failing over across models"
    );
    let snap_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/telemetry/snapshot")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let snap: Value =
        serde_json::from_slice(&snap_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(snap["session_pool"]["idle_cached_sessions"], 0);
    assert_eq!(snap["metrics"]["failed_requests"], 1);

    // 2. Non-streaming `/v1/messages`
    let cc_sync_req = json!({
        "model": "gemini-4-carbon-high",
        "stream": false,
        "messages": [{ "role": "user", "content": "check quota sync" }]
    });
    let r2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(cc_sync_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::BAD_REQUEST);
    let b2: Value =
        serde_json::from_slice(&r2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(b2["error"]["type"], "invalid_request_error");
    assert!(
        b2["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Resets in 48m36s.")
    );

    // 3. Streaming `/v1/responses` (Codex CLI)
    let codex_stream_req = json!({
        "model": "gemini-4-carbon-high",
        "stream": true,
        "input": [{ "role": "user", "content": "check codex stream quota" }]
    });
    let r3 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(codex_stream_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r3.status(), StatusCode::OK);
    let sse3 =
        String::from_utf8(r3.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(sse3.contains("event: response.failed"));
    assert!(sse3.contains("\"code\":\"insufficient_quota\""));
    assert!(sse3.contains("Resets in 48m36s."));

    // 4. Non-streaming `/v1/responses`
    let codex_sync_req = json!({
        "model": "gemini-4-carbon-high",
        "stream": false,
        "input": [{ "role": "user", "content": "check codex sync quota" }]
    });
    let r4 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(codex_sync_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r4.status(), StatusCode::TOO_MANY_REQUESTS);
    let b4: Value =
        serde_json::from_slice(&r4.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(b4["error"]["type"], "insufficient_quota");
    assert_eq!(b4["error"]["code"], "insufficient_quota");

    // 5. Subsequent retry after quota resets works cleanly
    let r5 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(
                    Body::from(
                        json!({
                            "model": "gemini-4-carbon-high",
                            "stream": false,
                            "messages": [{ "role": "user", "content": "你问他解决完了就帮忙提交并推送啊" }]
                        })
                        .to_string(),
                    ),
                )
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r5.status(), StatusCode::OK);
    let b5: Value =
        serde_json::from_slice(&r5.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        b5["content"][0]["text"],
        "Quota reset; committed and pushed!"
    );
}

#[tokio::test(start_paused = true)]
async fn silent_empty_upstream_turn_fails_cleanly_instead_of_returning_blank_reply() {
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::Immediate {
        thoughts: vec![],
        chunks_template: vec!["   \n  ".to_owned()],
    }]));
    let app = test_app(Arc::clone(&backend), 30);

    let req = json!({
        "model": "gemini-4-carbon-high",
        "stream": false,
        "messages": [{ "role": "user", "content": "hello?" }]
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("returned an empty response")
    );
    assert_eq!(backend.aborted_count.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn malformed_empty_function_call_retry_after_xml_tool_call_short_circuits_in_zero_ms() {
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::ChunksThenHang {
        chunks_template: vec![
            "<agy_tool_call_{NONCE}>{\"name\":\"Read\",\"arguments\":{\"file_path\":\"a.rs\"}}</agy_tool_call_{NONCE}>\n\
             <agy_tool_call_{NONCE}>{\"name\":\"Read\",\"arguments\":{\"file_path\":\"b.rs\"}}</agy_tool_call_{NONCE}>\n\
             <agy_tool_call_{NONCE}>{\"name\":\"Read\",\"arguments\":{\"file_path\":\"c.rs\"}}</agy_tool_call_{NONCE}>\n"
                .to_owned(),
            "⚠ Your previous response contained an improperly formatted function call: Malformed function call: Failed to parse function call: Function call is empty - no input to parse.\nPlease retry with a properly formatted function call\nRetries remaining: 3\n".to_owned(),
        ],
    }]));

    // `ChunksThenHang` sleeps 3600s after Chunk 2; 120s timeout proves Liftgate cuts off in 0ms
    // right when the internal malformed-call retry banner arrives, without waiting for ACP retry!
    let app = test_app(Arc::clone(&backend), 120);
    let req = json!({
        "model": "gemini-4-carbon-high",
        "stream": false,
        "tools": [
            {
                "name": "Read",
                "description": "Read file",
                "input_schema": { "type": "object", "properties": { "file_path": { "type": "string" } } }
            }
        ],
        "messages": [{ "role": "user", "content": "Read a.rs, b.rs, and c.rs" }]
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["stop_reason"], "tool_use");
    let content = body["content"].as_array().unwrap();
    assert_eq!(content.len(), 3);
    assert_eq!(backend.aborted_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn serves_embedded_ui_assets_and_enforces_admin_shutdown_token() {
    static MOCK_UI: &[EmbeddedUiAsset] = &[
        ("index.html", b"<!doctype html><title>Liftgate</title>"),
        ("assets/app.js", b"console.log('ready');"),
    ];

    let backend = Arc::new(MockAcpBackend::new(Vec::new()));
    let limits =
        GatewayLimits::try_new(16 * 1024 * 1024, 12 * 1024 * 1024, 64, 16, 64, 30).unwrap();
    let (shutdown_handle, shutdown_rx) = AdminShutdownHandle::new(Arc::from("secret-stop-token"));
    let telemetry = Arc::new(TelemetryHub::new());
    let app = build_router(AppState {
        backend,
        pool: Arc::new(SessionPool::new(limits)),
        telemetry: Arc::clone(&telemetry),
        config: GatewayRuntimeConfig {
            title_model_file: None,
            spill_dir: None,
            settings_path: None,
            default_model: ModelId::try_new("gemini-4-carbon-high").unwrap(),
            acp_mode: AcpModeId::try_new("auto").unwrap(),
            api_key: None,
            limits,
            vision_offload: Arc::new(AtomicBool::new(false)),
            mcp_tool_bridge: Arc::new(AtomicBool::new(true)),
            mcp_server: None,
        },
        ui_dist_dir: None,
        embedded_ui: MOCK_UI,
        admin_shutdown: Some(shutdown_handle),
    });

    let root_resp = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(root_resp.status(), StatusCode::OK);
    assert_eq!(
        root_resp.headers().get("content-type").unwrap(),
        "text/html; charset=utf-8"
    );

    let js_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/assets/app.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(js_resp.status(), StatusCode::OK);
    assert_eq!(
        js_resp.headers().get("content-type").unwrap(),
        "application/javascript; charset=utf-8"
    );

    let unknown_api_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/nonexistent-endpoint")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unknown_api_resp.status(), StatusCode::NOT_FOUND);

    let bad_shutdown = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/admin/shutdown")
                .header(ADMIN_SHUTDOWN_HEADER, "wrong-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_shutdown.status(), StatusCode::UNAUTHORIZED);
    assert!(!*shutdown_rx.borrow());

    let ok_shutdown = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/admin/shutdown")
                .header(ADMIN_SHUTDOWN_HEADER, "secret-stop-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok_shutdown.status(), StatusCode::OK);
    assert!(*shutdown_rx.borrow());

    let sse_resp = app
        .oneshot(
            Request::builder()
                .uri("/api/telemetry/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sse_resp.status(), StatusCode::OK);
    let sse_bytes = sse_resp.into_body().collect().await.unwrap().to_bytes();
    assert!(sse_bytes.is_empty());
}

#[tokio::test]
async fn codex_responses_repairs_malformed_tool_escapes_raw_newlines_and_backtick_wrappers() {
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::Immediate {
        thoughts: Vec::new(),
        chunks_template: vec![
            "我现在更新 `routes.rs` 并检索会话记录。`<agy_tool_call_{NONCE}>\n\
             {\"name\": \"apply_patch\", \"input\": \"*** Begin Patch\\n*** Update File: src/routes.rs\\n@@\\n+#[derive(Clone, Debug)]\n+pub struct Guard {}\\n*** End Patch\"}\n\
             </agy_tool_call_{NONCE}>`\n\
             <agy_tool_call_{NONCE}>\n\
             {\"name\": \"exec_command\", \"arguments\": {\"cmd\": \"if ($t -notmatch '^# AGENTS\\.md') { Write-Output \\\"ok\\\" }\", \"workdir\": \"D:\\\\rust_pro\\\\agy\"}}\n\
             </agy_tool_call_{NONCE}>"
                .to_owned(),
        ],
    }]));

    let app = test_app(Arc::clone(&backend), 30);
    let req = json!({
        "model": "gpt-5.4",
        "stream": false,
        "tools": [
            {
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply patch"
            },
            {
                "type": "function",
                "name": "exec_command",
                "description": "Run command",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "cmd": { "type": "string" },
                        "workdir": { "type": "string" }
                    },
                    "required": ["cmd"]
                }
            }
        ],
        "input": [
            { "type": "message", "role": "user", "content": "Update routes.rs and verify." }
        ]
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let output = body["output"].as_array().unwrap();
    assert_eq!(output.len(), 3);

    assert_eq!(output[0]["type"], "message");
    let visible_text = output[0]["content"][0]["text"].as_str().unwrap();
    assert_eq!(visible_text, "我现在更新 `routes.rs` 并检索会话记录。");
    assert!(!visible_text.contains("agy_tool_call_"));

    assert_eq!(output[1]["type"], "custom_tool_call");
    assert_eq!(output[1]["name"], "apply_patch");
    assert!(
        output[1]["input"]
            .as_str()
            .unwrap()
            .contains("+#[derive(Clone, Debug)]\n+pub struct Guard {}")
    );

    assert_eq!(output[2]["type"], "function_call");
    assert_eq!(output[2]["name"], "exec_command");
    let parsed_args: Value = serde_json::from_str(output[2]["arguments"].as_str().unwrap())
        .expect("function_call arguments must be valid RFC-8259 JSON");
    assert!(
        parsed_args["cmd"]
            .as_str()
            .unwrap()
            .contains(r"'^# AGENTS\.md'")
    );
}

#[tokio::test(start_paused = true)]
async fn codex_cold_replay_after_tainted_session_preserves_late_user_task_and_resumes_pool() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        // Turn 1: Tainted by internal ACP malformed-call retry after emitting `exec_command`
        MockScript::ChunksThenHang {
            chunks_template: vec![
                "<agy_tool_call_{NONCE}>{\"name\":\"exec_command\",\"arguments\":{\"cmd\":\"Get-Content src/parser.rs\"}}</agy_tool_call_{NONCE}>\n".to_owned(),
                "⚠ Your previous response contained an improperly formatted function call: Malformed function call: Function call is empty.\nRetries remaining: 3\n".to_owned(),
            ],
        },
        // Turn 2: Cold replay on `sess_2` (with >1.6 MB history!) -> emits another `exec_command` cleanly
        MockScript::Immediate {
            thoughts: Vec::new(),
            chunks_template: vec![
                "<agy_tool_call_{NONCE}>{\"name\":\"exec_command\",\"arguments\":{\"cmd\":\"cargo test -p liftgate-types\"}}</agy_tool_call_{NONCE}>".to_owned(),
            ],
        },
        // Turn 3: Must reuse warm `sess_2` via prefix-hash match (delta only!)
        MockScript::Immediate {
            thoughts: Vec::new(),
            chunks_template: vec!["两个 bad case 已全部修复并通过测试。".to_owned()],
        },
    ]));

    let app = test_app(Arc::clone(&backend), 120);
    let tools = json!([
        {
            "type": "function",
            "name": "exec_command",
            "description": "Execute shell command",
            "parameters": {
                "type": "object",
                "properties": { "cmd": { "type": "string" } },
                "required": ["cmd"]
            }
        }
    ]);

    // Build a realistic >1.6 MB multi-task Codex `input` history
    let mut input_items = vec![
        json!({
            "type": "message",
            "role": "developer",
            "content": "Follow repository Rust engineering rules strictly."
        }),
        json!({
            "type": "message",
            "role": "user",
            "content": "Task 1: build the `liftgate` CLI binary."
        }),
    ];
    for i in 0..100 {
        input_items.push(json!({
            "type": "function_call",
            "call_id": format!("call_arch_{i}"),
            "name": "exec_command",
            "arguments": format!("{{\"cmd\":\"Get-Content mod_{i}.rs\"}}")
        }));
        input_items.push(json!({
            "type": "function_call_output",
            "call_id": format!("call_arch_{i}"),
            "output": format!("// mod_{i}.rs\n{}", "a".repeat(16_500))
        }));
    }
    input_items.push(json!({
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "output_text", "text": "Task 1 (`liftgate` CLI) is finished." }]
    }));
    let late_user_prompt =
        "Task 2: 目前使用 codex 遇到了两个 bad case（工具调用未解析 + 突然失忆），请一并修复！";
    input_items.push(json!({
        "type": "message",
        "role": "user",
        "content": late_user_prompt
    }));

    // Turn 1 (`sess_1`, tainted by malformed retry banner)
    let resp1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "tools": tools,
                        "input": input_items,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);
    let body1: Value =
        serde_json::from_slice(&resp1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let call1 = &body1["output"][0];
    assert_eq!(call1["type"], "function_call");
    let call1_id = call1["call_id"].as_str().unwrap().to_owned();

    // Append Turn 1 tool call + output -> Turn 2 triggers cold replay on `sess_2`
    input_items.push(json!({
        "type": "function_call",
        "call_id": call1_id,
        "name": "exec_command",
        "arguments": call1["arguments"]
    }));
    input_items.push(json!({
        "type": "function_call_output",
        "call_id": call1["call_id"],
        "output": "fn parse_tool_call_body(raw_body: &str) -> Option<ParsedToolCall> { ... }"
    }));

    let resp2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "tools": tools,
                        "input": input_items,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let body2: Value =
        serde_json::from_slice(&resp2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let call2 = &body2["output"][0];
    assert_eq!(call2["type"], "function_call");

    // Append Turn 2 tool call + output -> Turn 3 must reuse warm `sess_2`!
    input_items.push(json!({
        "type": "function_call",
        "call_id": call2["call_id"],
        "name": "exec_command",
        "arguments": call2["arguments"]
    }));
    input_items.push(json!({
        "type": "function_call_output",
        "call_id": call2["call_id"],
        "output": "test result: ok. 42 passed; 0 failed"
    }));

    let resp3 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "tools": tools,
                        "input": input_items,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp3.status(), StatusCode::OK);

    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 3);
    assert_eq!(prompts[0].session_id, "sess_1");
    assert_eq!(prompts[1].session_id, "sess_2");
    // Turn 3 reused `sess_2` seamlessly!
    assert_eq!(prompts[2].session_id, "sess_2");

    // Verify Turn 2's cold replay stayed strictly below the upstream tail-truncation limit AND
    // preserved both the late user task and current caller input at the tail!
    assert!(
        prompts[1].text_joined.len() <= liftgate_types::session_key::MAX_ACP_PROMPT_TEXT_BYTES,
        "cold replay len={} exceeded limit",
        prompts[1].text_joined.len()
    );
    assert!(
        prompts[1]
            .text_joined
            .contains("Task 1 (`liftgate` CLI) is finished.")
    );
    assert!(prompts[1].text_joined.contains(late_user_prompt));
    assert!(prompts[1].text_joined.contains("### Current Caller Input"));
    assert!(prompts[1].text_joined.contains("fn parse_tool_call_body"));
    // And Turn 3 sent only the compact delta on warm `sess_2`!
    assert!(prompts[2].text_joined.len() < 512);
    assert!(
        prompts[2]
            .text_joined
            .contains("test result: ok. 42 passed")
    );
}

#[tokio::test]
async fn anthropic_streaming_coerces_raw_string_input_for_function_tool_to_valid_json_object() {
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::Immediate {
        thoughts: Vec::new(),
        chunks_template: vec![
            "<agy_tool_call_{NONCE}>{\"name\":\"Bash\",\"input\":\"cargo test --workspace\"}</agy_tool_call_{NONCE}>"
                .to_owned(),
        ],
    }]));

    let app = test_app(Arc::clone(&backend), 30);
    let req = json!({
        "model": "claude-opus-4-6",
        "stream": true,
        "tools": [
            {
                "name": "Bash",
                "description": "Run shell command",
                "input_schema": {
                    "type": "object",
                    "properties": { "command": { "type": "string" } },
                    "required": ["command"]
                }
            }
        ],
        "messages": [{ "role": "user", "content": "Run workspace tests" }]
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let raw_sse = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(raw_sse.contains("\"type\":\"input_json_delta\""));
    assert!(raw_sse.contains("{\\\"command\\\":\\\"cargo test --workspace\\\"}"));
}

#[tokio::test]
async fn large_cold_transcript_compacts_into_bounded_single_turn_without_model_thrashing() {
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::Immediate {
        thoughts: vec!["Synthesizing across bounded single-turn history".to_owned()],
        chunks_template: vec!["Verified bounded single-turn cold transcript.".to_owned()],
    }]));

    let app = test_app(Arc::clone(&backend), 120);
    let req = json!({
        "model": "gemini-4-carbon-high",
        "stream": false,
        "input": [
            {
                "role": "user",
                "content": format!("EARLY_NEEDLE_01\n{}", "x".repeat(120_000))
            },
            {
                "role": "assistant",
                "content": [{ "type": "output_text", "text": format!("MID_NEEDLE_02\n{}", "y".repeat(130_000)) }]
            },
            {
                "role": "user",
                "content": format!("LATE_NEEDLE_03\n{}", "z".repeat(120_000))
            },
            {
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "Ready for final check." }]
            },
            {
                "role": "user",
                "content": "Verify every needle from the conversation."
            }
        ]
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        body["output"][1]["content"][0]["text"],
        "Verified bounded single-turn cold transcript."
    );
    assert_eq!(body["usage"]["input_tokens"], 228);

    // Session runs directly on `gemini-4-carbon-high` with zero hidden Flash replay turns!
    let models = backend.recorded_models.lock().unwrap().clone();
    assert_eq!(models, vec!["gemini-4-carbon-high".to_owned()]);

    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].session_id, "sess_1");
    assert!(
        prompts[0].text_joined.len() <= liftgate_types::session_key::MAX_ACP_PROMPT_TEXT_BYTES,
        "prompt len {} exceeded MAX_ACP_PROMPT_TEXT_BYTES",
        prompts[0].text_joined.len()
    );
    assert!(prompts[0].text_joined.contains("Ready for final check."));
    assert!(
        prompts[0]
            .text_joined
            .contains("Verify every needle from the conversation.")
    );
}

#[tokio::test]
async fn usage_update_compaction_drop_delivers_current_turn_then_retires_session_for_clean_rehydration()
 {
    let backend = Arc::new(MockAcpBackend::new(vec![
        // Turn 1: `agy acp` executes at 340k tokens, streams the full answer, then post-compacts to 20.5k!
        MockScript::ImmediateWithUsage {
            usage_updates: vec![(340_000, 1_048_576), (20_500, 1_048_576)],
            chunks_template: vec!["Full uncompacted answer on Turn 1.".to_owned()],
        },
        // Turn 2: `sess_1` was retired, so Turn 2 rehydrates cleanly on `sess_2`!
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Clean answer on fresh sess_2.".to_owned()],
        },
    ]));

    let app = test_app(Arc::clone(&backend), 60);
    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "input": [{ "role": "user", "content": "Analyze full architecture." }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let b1: Value =
        serde_json::from_slice(&r1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        b1["output"][0]["content"][0]["text"],
        "Full uncompacted answer on Turn 1."
    );

    // Verify compacted `sess_1` was retired instead of poisoning SessionPool!
    let snap_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/telemetry/snapshot")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let snap: Value =
        serde_json::from_slice(&snap_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(snap["session_pool"]["idle_cached_sessions"], 0);

    // Turn 2 creates fresh `sess_2` with full history!
    let r2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "input": [
                            { "role": "user", "content": "Analyze full architecture." },
                            { "role": "assistant", "content": [{ "type": "output_text", "text": "Full uncompacted answer on Turn 1." }] },
                            { "role": "user", "content": "Follow up without losing Turn 1 context." }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0].session_id, "sess_1");
    assert_eq!(prompts[1].session_id, "sess_2");
    assert!(
        prompts[1]
            .text_joined
            .contains("Analyze full architecture.")
    );
}

#[tokio::test]
async fn local_compaction_reuses_warm_coding_session_then_keeps_post_compaction_cache_warm() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        // 1. Pre-compaction coding turn (`sess_1`)
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Implemented parser module.".to_owned()],
        },
        // 2. `/compact` summarization turn -> MUST reuse warm `sess_1` via `compaction_lookup`
        //    and then close `sess_1`!
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Summary: parser module implemented and verified.".to_owned()],
        },
        // 3. First post-compaction turn (`sess_2`)
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Ready to continue from summary.".to_owned()],
        },
        // 4. Second post-compaction turn (mutated timestamp + token budget + added tool + effort switch)
        //    -> MUST reuse warm `sess_2`!
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Warm post-compaction continuation succeeded.".to_owned()],
        },
    ]));

    let app = test_app(Arc::clone(&backend), 60);
    let bash_tool = json!({
        "type": "function",
        "name": "exec_command",
        "description": "Run command",
        "parameters": { "type": "object", "properties": { "cmd": { "type": "string" } } }
    });
    let edit_tool = json!({
        "type": "function",
        "name": "edit_file",
        "description": "Edit file",
        "parameters": { "type": "object", "properties": { "path": { "type": "string" } } }
    });

    // Turn 1: Coding turn on `gemini-4-carbon-high`
    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "tools": [bash_tool.clone()],
                        "input": [
                            { "role": "developer", "content": "You are a coding agent running in the Codex CLI. Follow strict Rust rules." },
                            { "role": "user", "content": "Implement the parser module." }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);

    // Turn 2: Codex local `/compact` request (swaps `instructions` to summarization prompt,
    // drops `<developer_instructions>`, clears `tools = []`, appends `CONTEXT CHECKPOINT COMPACTION`)
    let r_compact = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "instructions": "You are performing a CONTEXT CHECKPOINT COMPACTION. Summarize key state.",
                        "tools": [],
                        "input": [
                            { "role": "user", "content": "Implement the parser module." },
                            { "role": "assistant", "content": [{ "type": "output_text", "text": "Implemented parser module." }] },
                            { "role": "user", "content": "Create a handoff summary for another LLM that will resume the task." }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_compact.status(), StatusCode::OK);

    // Turn 3: Post-compaction turn with summary + `<current_time_reminder>` + `<context_window>`
    let r3 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "tools": [bash_tool.clone()],
                        "input": [
                            { "role": "developer", "content": "You are a coding agent running in the Codex CLI. Follow strict Rust rules." },
                            { "role": "user", "content": "Another language model started to solve this problem:\nSummary: parser module implemented and verified." },
                            { "role": "user", "content": "<current_time_reminder>2026-11-15T12:00:00+08:00</current_time_reminder>\n<context_window>used=14200 limit=1048576</context_window>\nContinue with server integration." }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r3.status(), StatusCode::OK);

    // Turn 4: Mutated `<context_window>`, removed old `<current_time_reminder>`, added `edit_file` tool,
    // and switched model to `gemini-4-carbon-xhigh` -> MUST still hit warm `sess_2`!
    let r4 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-xhigh",
                        "stream": false,
                        "tools": [bash_tool, edit_tool],
                        "input": [
                            { "role": "developer", "content": "You are a coding agent running in the Codex CLI. Follow strict Rust rules." },
                            { "role": "user", "content": "Another language model started to solve this problem:\nSummary: parser module implemented and verified." },
                            { "role": "user", "content": "<context_window>used=18900 limit=1048576</context_window>\nContinue with server integration." },
                            { "role": "assistant", "content": [{ "type": "output_text", "text": "Ready to continue from summary." }] },
                            { "role": "user", "content": "<current_time_reminder>2026-11-15T12:05:00+08:00</current_time_reminder>\nNow edit turn_runner.rs." }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r4.status(), StatusCode::OK);

    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 4);
    assert_eq!(prompts[0].session_id, "sess_1");
    assert_eq!(
        prompts[1].session_id, "sess_1",
        "`/compact` summarization turn must reuse warm coding session `sess_1`"
    );
    assert!(prompts[1].text_joined.contains("<client_tools_update>"));
    assert!(
        prompts[1]
            .text_joined
            .contains("CONTEXT CHECKPOINT COMPACTION")
    );
    assert!(
        !prompts[1]
            .text_joined
            .contains("Implement the parser module.")
    );

    assert_eq!(prompts[2].session_id, "sess_2");
    assert!(
        prompts[2]
            .text_joined
            .contains("<current_time_reminder>2026-11-15T12:00:00+08:00</current_time_reminder>")
    );
    assert!(
        prompts[2]
            .text_joined
            .contains("<context_window>used=14200 limit=1048576</context_window>")
    );

    assert_eq!(
        prompts[3].session_id, "sess_2",
        "post-compaction Turn 4 must reuse warm `sess_2` despite tool/reminder/effort updates"
    );
    assert!(prompts[3].text_joined.contains("<client_tools_update>"));
    assert!(prompts[3].text_joined.contains("name: edit_file"));
    assert!(
        prompts[3]
            .text_joined
            .contains("<current_time_reminder>2026-11-15T12:05:00+08:00</current_time_reminder>")
    );
}

#[tokio::test]
async fn multimodal_tool_outputs_deliver_native_acp_image_blocks_and_codex_async_user_catalog() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Inspected Codex view_image PNG.".to_owned()],
        },
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Inspected Claude Read PNG.".to_owned()],
        },
    ]));
    let app = test_app(Arc::clone(&backend), 30);

    // 1. Verify `/v1/codex-models` advertises async user tools + `clock` + multimodal capabilities
    let cat_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/codex-models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cat_resp.status(), StatusCode::OK);
    let cat: Value =
        serde_json::from_slice(&cat_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let exp_tools = cat["models"][0]["experimental_supported_tools"]
        .as_array()
        .unwrap();
    assert!(exp_tools.contains(&json!("request_user_input_async")));
    assert!(exp_tools.contains(&json!("send_user_message_async")));
    assert!(exp_tools.contains(&json!("send_message_to_user_async")));
    assert!(exp_tools.contains(&json!("clock")));
    assert_eq!(cat["models"][0]["supports_image_detail_original"], true);

    // 2. OpenAI `/v1/responses` `function_call_output` array with `input_image`
    let oai_req = json!({
        "model": "gemini-4-carbon-high",
        "stream": false,
        "input": [
            { "role": "user", "content": "Check UI layout" },
            { "type": "function_call", "call_id": "call_v1", "name": "view_image", "arguments": "{\"path\":\"ui.png\"}" },
            {
                "type": "function_call_output",
                "call_id": "call_v1",
                "output": [
                    { "type": "input_text", "text": "Viewed ui.png" },
                    { "type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==" }
                ]
            }
        ]
    });
    let r_oai = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(oai_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_oai.status(), StatusCode::OK);

    // 3. Anthropic `/v1/messages` `tool_result` with nested base64 `image`
    let cc_req = json!({
        "model": "claude-opus-4-6",
        "max_tokens": 512,
        "stream": false,
        "messages": [
            { "role": "user", "content": "Read chart.webp" },
            {
                "role": "assistant",
                "content": [{ "type": "tool_use", "id": "toolu_img", "name": "Read", "input": { "file_path": "chart.webp" } }]
            },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_img",
                    "content": [
                        { "type": "text", "text": "Loaded chart.webp" },
                        {
                            "type": "image",
                            "source": { "type": "base64", "media_type": "image/webp", "data": "UklGRh4AAABXRUJQVlA4IA==" }
                        }
                    ]
                }]
            }
        ]
    });
    let r_cc = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(cc_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_cc.status(), StatusCode::OK);

    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0].image_blocks, vec![("image/png".to_owned(), 24)]);
    assert!(!prompts[0].text_joined.contains("iVBORw0KGgo"));
    assert_eq!(prompts[1].image_blocks, vec![("image/webp".to_owned(), 24)]);
    assert!(!prompts[1].text_joined.contains("UklGRh4AAABXRUJQ"));
}

#[tokio::test(start_paused = true)]
async fn codex_code_mode_multi_turn_sse_guardian_classifier_and_effort_update() {
    let exec_js = "// @exec: {\"yield_time_ms\": 5000}\nconst [files, git] = await Promise.all([\n  tools.exec_command({ cmd: \"rg --files crates\" }),\n  tools.exec_command({ cmd: \"git status --short\" }),\n]);\nstore(\"files\", files.output);\ntext(`scanned ${String(files.output).split(/\\n/).length} lines`);";

    let backend = Arc::new(MockAcpBackend::new(vec![
        // 1. Turn 1: model emits `exec` JS cell over XML bridge
        MockScript::Immediate {
            thoughts: vec!["Batching workspace inspection in a single exec cell.".to_owned()],
            chunks_template: vec![format!(
                "Inspecting workspace files and git status in parallel.\n<agy_tool_call_{{NONCE}}>\n{}\n</agy_tool_call_{{NONCE}}>",
                json!({ "name": "exec", "input": exec_js })
            )],
        },
        // 2. Interleaved Codex Guardian v2 recursive security classifier sub-request
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![
                "{\"needs_review\":false,\"risk_score\":5,\"rationale\":\"read-only rg and git status\"}".to_owned(),
            ],
        },
        // 3. Turn 2: `exec` yielded running `cell_1` -> model calls `wait` on warm session
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![
                "<agy_tool_call_{NONCE}>{\"name\":\"wait\",\"arguments\":{\"cell_id\":\"cell_1\",\"yield_time_ms\":5000}}</agy_tool_call_{NONCE}>".to_owned(),
            ],
        },
    ]));

    let app = test_app(Arc::clone(&backend), 30);

    // Verify `/v1/codex-models?tool_mode=code_mode` sets `tool_mode` and `supports_reasoning_effort_updates`
    let cat_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/codex-models?tool_mode=code_mode")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cat_resp.status(), StatusCode::OK);
    let cat: Value =
        serde_json::from_slice(&cat_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(cat["models"][0]["tool_mode"], "code_mode");
    assert_eq!(cat["models"][0]["supports_reasoning_effort_updates"], true);
    assert_eq!(cat["models"][0]["supports_search_tool"], false);

    // Build an 18 KB+ `exec` tool description (simulating `code_mode_only` with ~170 nested tools)
    let mut exec_desc = String::from(
        "Run JavaScript code to orchestrate tool calls. Accepts raw JavaScript source text, not JSON, quoted strings, or markdown code fences.\n\
         Tool TypeScript declarations:\n```ts\ndeclare const tools: {\n  exec_command(args: { cmd: string }): Promise<{ output: string }>;\n",
    );
    for idx in 0..220 {
        exec_desc.push_str(&format!(
            "  mcp__workspace__connector_{idx:03}(args: {{ query: string; limit?: number }}): Promise<{{ items: string[] }}>;\n"
        ));
    }
    exec_desc.push_str("};\n```");
    assert!(exec_desc.len() > 18_000);

    let tools = json!([
        {
            "type": "custom",
            "name": "exec",
            "description": exec_desc,
            "format": {
                "type": "grammar",
                "syntax": "lark",
                "definition": "start: /[\\s\\S]+/"
            }
        },
        {
            "type": "function",
            "name": "wait",
            "description": "Wait on a running `exec` cell or terminate it.",
            "parameters": {
                "type": "object",
                "properties": {
                    "cell_id": { "type": "string" },
                    "yield_time_ms": { "type": "integer" },
                    "terminate": { "type": "boolean" }
                },
                "required": ["cell_id"]
            }
        }
    ]);

    // Turn 1 SSE request
    let turn1_req = json!({
        "model": "gemini-4-carbon-high",
        "stream": true,
        "instructions": "You are Codex running in Code Mode.",
        "tools": tools,
        "input": [
            { "role": "user", "content": "Inspect the crate files and git status." }
        ]
    });
    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(turn1_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let sse1 =
        String::from_utf8(r1.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();

    // Parse SSE events to verify `response.custom_tool_call_input.delta` carries `call_id`
    // and `response.output_item.done` carries the exact uncorrupted `exec` JavaScript source!
    let mut delta_call_id: Option<String> = None;
    let mut done_call_id: Option<String> = None;
    let mut done_input: Option<String> = None;
    for line in sse1.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(ev) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        match ev["type"].as_str() {
            Some("response.custom_tool_call_input.delta") => {
                delta_call_id = ev["call_id"].as_str().map(str::to_owned);
            }
            Some("response.output_item.done") if ev["item"]["type"] == "custom_tool_call" => {
                done_call_id = ev["item"]["call_id"].as_str().map(str::to_owned);
                done_input = ev["item"]["input"].as_str().map(str::to_owned);
            }
            _ => {}
        }
    }
    let exec_call_id = done_call_id.expect("expected custom_tool_call output item");
    assert_eq!(
        delta_call_id.as_deref(),
        Some(exec_call_id.as_str()),
        "Codex 0.169.0 requires `call_id` on `response.custom_tool_call_input.delta`"
    );
    assert_eq!(done_input.as_deref(), Some(exec_js));

    // Interleaved Codex Guardian v2 classifier request -> must route to fast Flash ephemerally
    let guardian_req = json!({
        "model": "gemini-4-carbon-high",
        "stream": false,
        "instructions": "Predict whether the agent's computer and browser activity needs blocking security review. Review nested calls recursively.",
        "input": [
            { "role": "user", "content": "exec cell calling exec_command(rg --files crates) and exec_command(git status --short)" }
        ]
    });
    let r_guard = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(guardian_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_guard.status(), StatusCode::OK);

    // Turn 2: `exec` yielded running `cell_1` + screenshot + mid-thread `configuration_update` (`low`)
    let turn2_req = json!({
        "model": "gemini-4-carbon-high",
        "stream": true,
        "instructions": "You are Codex running in Code Mode.",
        "tools": tools,
        "input": [
            { "role": "user", "content": "Inspect the crate files and git status." },
            {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "Inspecting workspace files and git status in parallel.\n" }]
            },
            {
                "type": "custom_tool_call",
                "call_id": exec_call_id,
                "name": "exec",
                "input": exec_js
            },
            {
                "type": "custom_tool_call_output",
                "call_id": exec_call_id,
                "output": [
                    { "type": "input_text", "text": "Script running with cell ID cell_1\nWall time: 5.0 seconds\nOutput:\npartial scan..." },
                    { "type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==" }
                ]
            },
            {
                "type": "configuration_update",
                "reasoning": { "effort": "low" }
            }
        ]
    });
    let r2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(turn2_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let sse2 =
        String::from_utf8(r2.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(sse2.contains("\"type\":\"function_call\""));
    assert!(sse2.contains("\"name\":\"wait\""));
    assert!(sse2.contains("cell_1"));

    let appends = backend.recorded_system_appends.lock().unwrap();
    assert_eq!(
        appends.len(),
        2,
        "only 1 main session + 1 ephemeral Guardian classifier session should be created"
    );
    assert!(appends[0].len() <= liftgate_types::session_key::MAX_SYSTEM_APPEND_BYTES);
    assert!(
        appends[0].contains("mcp__workspace__connector_219"),
        "full 18 KB+ `exec` TypeScript declarations fit inline inside the calibrated 48 KB `MAX_SYSTEM_APPEND_BYTES` window"
    );

    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 3);
    assert_eq!(prompts[0].session_id, "sess_1");
    assert_eq!(prompts[1].session_id, "sess_2");
    assert_eq!(
        prompts[2].session_id, "sess_1",
        "Turn 2 must reuse warm `sess_1` after `custom_tool_call_output` + `configuration_update`"
    );
    assert_eq!(prompts[2].image_blocks, vec![("image/png".to_owned(), 24)]);

    let models = backend.recorded_models.lock().unwrap();
    assert_eq!(
        models.as_slice(),
        &[
            "gemini-4-carbon-high",
            "gemini-4-hydrogen-low",
            "gemini-4-carbon-medium"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn claude_code_mode_repl_fable_model_and_warm_session_roundtrip() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![
                "Running a single REPL script to inspect the workspace.\n<agy_tool_call_{NONCE}>\n{\"name\": \"REPL\", \"arguments\": {\"code\": \"const files = await $`rg --files \"crates\"`.text();\nconsole.log(files);\"}}\n</agy_tool_call_{NONCE}>".to_owned(),
            ],
        },
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Found 3 workspace crates.".to_owned()],
        },
    ]));

    let app = test_app(Arc::clone(&backend), 30);
    let repl_desc = format!(
        "Execute JavaScript inside the persistent Bun Code Mode sandbox.\n{}\nHelpers: `$`, `edit(path, old, new)`, `create(path, content)`, `view(path)`.",
        "// REPL sandbox helper line\n".repeat(40)
    );
    let tools = json!([
        {
            "name": "REPL",
            "description": repl_desc,
            "input_schema": {
                "type": "object",
                "properties": { "code": { "type": "string" } },
                "required": ["code"]
            }
        },
        {
            "name": "mcp__github__list_issues",
            "description": "List repository issues.",
            "allowed_callers": ["code_execution_20260120"],
            "defer_loading": true,
            "input_schema": { "type": "object" }
        }
    ]);

    let req1 = json!({
        "model": "claude-fable-5-2",
        "max_tokens": 8192,
        "container": "cntr_local_1",
        "stream": false,
        "tools": tools,
        "messages": [
            { "role": "user", "content": "List workspace crates via REPL" }
        ]
    });
    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(req1.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let body1: Value =
        serde_json::from_slice(&r1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body1["stop_reason"], "tool_use");
    let tool_use = body1["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("REPL tool_use block");
    assert_eq!(tool_use["name"], "REPL");
    assert_eq!(
        tool_use["input"]["code"],
        "const files = await $`rg --files \"crates\"`.text();\nconsole.log(files);"
    );
    let repl_id = tool_use["id"].as_str().unwrap().to_owned();

    let req2 = json!({
        "model": "claude-fable-5-2",
        "max_tokens": 8192,
        "container": "cntr_local_1",
        "stream": false,
        "tools": tools,
        "messages": [
            { "role": "user", "content": "List workspace crates via REPL" },
            { "role": "assistant", "content": body1["content"] },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": repl_id,
                    "content": "crates/liftgate/Cargo.toml\ncrates/liftgate-server/Cargo.toml\ncrates/liftgate-types/Cargo.toml\n"
                }]
            }
        ]
    });
    let r2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(req2.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let body2: Value =
        serde_json::from_slice(&r2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body2["stop_reason"], "end_turn");

    let appends = backend.recorded_system_appends.lock().unwrap();
    assert_eq!(appends.len(), 1, "Turn 2 must reuse warm REPL session");
    assert!(
        appends[0].contains("`edit(path, old, new)`, `create(path, content)`, `view(path)`"),
        "REPL sandbox description must not be truncated at 600 chars"
    );

    let models = backend.recorded_models.lock().unwrap();
    assert!(
        models[0].starts_with("claude-sonnet-") || models[0].starts_with("claude-opus-"),
        "claude-fable-5-2 should map to Claude Sonnet/Opus family when available, got {}",
        models[0]
    );
}

#[tokio::test]
async fn claude_desktop_gateway_routes_discover_safe_models_and_unwrap_route_ids() {
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::Immediate {
        thoughts: vec![],
        chunks_template: vec!["Hello from Claude Desktop 3P".to_owned()],
    }]));
    let app = test_app_with_key(Arc::clone(&backend), 10, Some("desktop-key"));

    let unauth = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/claude-desktop/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    let preflight = app
        .clone()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/claude-desktop/v1/messages")
                .header(
                    "access-control-request-headers",
                    "anthropic-dangerous-direct-browser-access, x-stainless-lang, x-api-key",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        preflight.headers()["access-control-allow-headers"]
            .to_str()
            .unwrap(),
        "anthropic-dangerous-direct-browser-access, x-stainless-lang, x-api-key"
    );

    let models_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/claude-desktop/v1/models")
                .header("authorization", "bearer desktop-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(models_resp.status(), StatusCode::OK);
    let models_json: Value =
        serde_json::from_slice(&models_resp.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    let data = models_json["data"].as_array().expect("models data array");
    assert!(!data.is_empty());
    for item in data {
        let id = item["id"].as_str().unwrap();
        assert!(
            liftgate_types::model::is_claude_desktop_safe_model_id(id),
            "unsafe Claude Desktop model ID: {id}"
        );
    }

    let bootstrap_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/claude-desktop/user/bootstrap")
                .header("authorization", "Bearer desktop-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bootstrap_resp.status(), StatusCode::OK);
    let bootstrap_json: Value = serde_json::from_slice(
        &bootstrap_resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert_eq!(bootstrap_json["builtinBrowserEnabled"], true);

    let flash_route = liftgate_types::model::encode_claude_desktop_route_id("gemini-3.8-flash-low");
    let msg_req = json!({
        "model": format!("{flash_route}[1m]"),
        "max_tokens": 1024,
        "stream": false,
        "messages": [{ "role": "user", "content": "ping" }]
    });
    let msg_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/claude-desktop/v1/messages")
                .header("authorization", "Bearer desktop-key")
                .header("content-type", "application/json")
                .body(Body::from(msg_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(msg_resp.status(), StatusCode::OK);
    let recorded = backend.recorded_models.lock().unwrap();
    assert_eq!(recorded.as_slice(), &["gemini-3.8-flash-low"]);
}

#[tokio::test(start_paused = true)]
async fn codex_sse_streams_apply_patch_start_and_deltas_live_before_closing_tag() {
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::PacedChunks {
        chunk_delay: Duration::from_secs(25),
        chunks_template: vec![
            "我先扩展 `config_sync.rs`、`cli.rs` 与 `harness.rs`。\n<agy_tool_call_{NONCE}>\n{\"name\": \"apply_patch\", \"input\": \"*** Begin Patch\\n*** Update File: crates/liftgate/src/app/config_sync.rs\\n".to_owned(),
            "@@\\n-old_line\\n+new_line_with_\\\"quotes\\\"\\n".to_owned(),
            "*** End Patch\"}\n</agy_tool_call_{NONCE}>".to_owned(),
        ],
    }]));

    let app = test_app(Arc::clone(&backend), 120);
    let req = json!({
        "model": "gpt-5.4",
        "stream": true,
        "tools": [
            {
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply a patch to files",
                "format": { "type": "text" }
            }
        ],
        "input": [{ "role": "user", "content": "Extend config_sync.rs" }]
    });

    let start_instant = tokio::time::Instant::now();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body();

    // Read early SSE frames BEFORE the 25s sleep for Chunk 2 elapses!
    let mut early_sse = String::new();
    while !(early_sse.contains("response.custom_tool_call_input.delta")
        && early_sse.contains("\"type\":\"custom_tool_call\""))
    {
        let frame = tokio::time::timeout(Duration::from_secs(1), body.frame())
            .await
            .expect("must flush message.done + custom_tool_call.added + first delta in <1s without waiting 50s for closing tag")
            .expect("SSE stream still open")
            .expect("valid body frame");
        if let Ok(bytes) = frame.into_data() {
            early_sse.push_str(std::str::from_utf8(&bytes).unwrap());
        }
    }
    assert!(
        start_instant.elapsed() < Duration::from_secs(1),
        "early SSE frames must arrive at 0s before Chunk 2's 25s sleep, got {:?}",
        start_instant.elapsed()
    );

    // Verify preceding AgentMessage already closed (`response.output_item.done`) AND `apply_patch`
    // `custom_tool_call` was added (`status: "in_progress"`) before Chunk 2 even starts!
    let mut saw_early_msg_done = false;
    let mut saw_early_ctc_added = false;
    let mut streamed_deltas = Vec::new();
    for line in early_sse.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let ev: Value = serde_json::from_str(data).unwrap();
        match ev["type"].as_str() {
            Some("response.output_item.done") if ev["item"]["type"] == "message" => {
                assert_eq!(
                    ev["item"]["content"][0]["text"],
                    "我先扩展 `config_sync.rs`、`cli.rs` 与 `harness.rs`。\n"
                );
                saw_early_msg_done = true;
            }
            Some("response.output_item.added") if ev["item"]["type"] == "custom_tool_call" => {
                assert!(
                    saw_early_msg_done,
                    "AgentMessage must finalize before CustomToolCall opens"
                );
                assert_eq!(ev["item"]["name"], "apply_patch");
                assert_eq!(ev["item"]["status"], "in_progress");
                assert_eq!(ev["item"]["input"], "");
                saw_early_ctc_added = true;
            }
            Some("response.custom_tool_call_input.delta") => {
                streamed_deltas.push(ev["delta"].as_str().unwrap().to_owned());
            }
            _ => {}
        }
    }
    assert!(saw_early_msg_done);
    assert!(saw_early_ctc_added);
    assert!(!streamed_deltas.is_empty());

    // Now collect remaining SSE frames (advancing past the two 25s chunk delays)
    let rest_bytes = body.collect().await.unwrap().to_bytes();
    let rest_sse = std::str::from_utf8(&rest_bytes).unwrap();
    let mut done_input: Option<String> = None;
    let mut item_done_input: Option<String> = None;
    for line in rest_sse.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let ev: Value = serde_json::from_str(data).unwrap();
        match ev["type"].as_str() {
            Some("response.custom_tool_call_input.delta") => {
                streamed_deltas.push(ev["delta"].as_str().unwrap().to_owned());
            }
            Some("response.custom_tool_call_input.done") => {
                done_input = ev["input"].as_str().map(str::to_owned);
            }
            Some("response.output_item.done") if ev["item"]["type"] == "custom_tool_call" => {
                item_done_input = ev["item"]["input"].as_str().map(str::to_owned);
            }
            _ => {}
        }
    }

    let expected_patch = "*** Begin Patch\n*** Update File: crates/liftgate/src/app/config_sync.rs\n@@\n-old_line\n+new_line_with_\"quotes\"\n*** End Patch";
    assert!(
        streamed_deltas.len() >= 3,
        "expected >=3 incremental delta frames across 3 chunks, got {:?}",
        streamed_deltas
    );
    assert_eq!(streamed_deltas.concat(), expected_patch);
    assert_eq!(done_input.as_deref(), Some(expected_patch));
    assert_eq!(item_done_input.as_deref(), Some(expected_patch));
}

#[tokio::test(start_paused = true)]
async fn post_tool_thought_native_tool_and_inline_banners_short_circuit_in_zero_ms() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        // Case 1: Post-tool `AgentThoughtChunk` (internal ACP second sub-call)
        MockScript::UpdatesThenHang {
            initial_chunk_template: "<agy_tool_call_{NONCE}>{\"name\":\"apply_patch\",\"input\":\"*** Begin Patch\\n*** End Patch\"}</agy_tool_call_{NONCE}>".to_owned(),
            trailing_updates: vec![SessionUpdatePayload::AgentThoughtChunk {
                content: AcpChunkContent {
                    content_type: "text".to_owned(),
                    text: "Wait, let me inspect the workspace again...".to_owned(),
                },
            }],
        },
        // Case 2: Post-tool native ACP `ToolCall` (`edit_file` inside `liftgate-empty-cwd`)
        MockScript::UpdatesThenHang {
            initial_chunk_template: "<agy_tool_call_{NONCE}>{\"name\":\"apply_patch\",\"input\":\"*** Begin Patch\\n*** End Patch\"}</agy_tool_call_{NONCE}>".to_owned(),
            trailing_updates: vec![SessionUpdatePayload::ToolCall {
                tool_call_id: "native_edit_1".to_owned(),
                name: "edit_file".to_owned(),
                status: "in_progress".to_owned(),
                kind: "edit".to_owned(),
                title: "Editing config_sync.rs".to_owned(),
                args: None,
                raw_input: None,
            }],
        },
        // Case 3: Inline `⚠ Your previous response...` glued directly after `</agy_tool_call_{NONCE}>` with NO newline
        MockScript::ChunksThenHang {
            chunks_template: vec![
                "<agy_tool_call_{NONCE}>{\"name\":\"apply_patch\",\"input\":\"*** Begin Patch\\n*** End Patch\"}</agy_tool_call_{NONCE}>⚠ Your previous response contained an improperly formatted function call".to_owned(),
            ],
        },
        // Case 4: Inline `⚠ API error (attempt 1): transient 503 — retrying in 1s...` with NO newline
        MockScript::ChunksThenHang {
            chunks_template: vec![
                "<agy_tool_call_{NONCE}>{\"name\":\"apply_patch\",\"input\":\"*** Begin Patch\\n*** End Patch\"}</agy_tool_call_{NONCE}>⚠ API error (attempt 1): transient 503 — retrying in 1s...".to_owned(),
            ],
        },
    ]));

    let app = test_app(Arc::clone(&backend), 120);
    let req = json!({
        "model": "gpt-5.4",
        "stream": true,
        "tools": [
            {
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply a patch",
                "format": { "type": "text" }
            }
        ],
        "input": [{ "role": "user", "content": "Apply patch" }]
    });

    for case_num in 1..=4 {
        let t0 = tokio::time::Instant::now();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(req.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "case {case_num} HTTP status");
        let sse = String::from_utf8(
            tokio::time::timeout(Duration::from_secs(1), resp.into_body().collect())
                .await
                .unwrap_or_else(|_| panic!("case {case_num} hung instead of short-circuiting"))
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert_eq!(
            t0.elapsed(),
            Duration::ZERO,
            "case {case_num} must complete in 0ms virtual time without waiting on 3600s hang"
        );
        assert!(
            sse.contains("event: response.completed"),
            "case {case_num} must complete cleanly: {sse}"
        );
        assert!(
            sse.contains("\"name\":\"apply_patch\""),
            "case {case_num} must deliver captured apply_patch tool call"
        );
        assert!(
            !sse.contains("Your previous response") && !sse.contains("transient 503"),
            "case {case_num} must strip ACP retry banners cleanly"
        );
    }

    assert_eq!(
        backend.aborted_count.load(Ordering::SeqCst),
        4,
        "all 4 tainted runaway child sessions must be aborted in background instead of pooled"
    );
}

#[tokio::test(start_paused = true)]
async fn anthropic_sse_streams_early_tool_use_start_and_truncated_xml_tool_fails_cleanly() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        // 1. Anthropic `/v1/messages` SSE: multi-chunk `Edit` tool call with 20s delay before closing tag
        MockScript::PacedChunks {
            chunk_delay: Duration::from_secs(20),
            chunks_template: vec![
                "Updating main.rs.\n<agy_tool_call_{NONCE}>\n{\"name\": \"Edit\", \"arguments\": {\"file_path\": \"src/main.rs\",".to_owned(),
                " \"old_string\": \"foo\", \"new_string\": \"bar\"}}\n</agy_tool_call_{NONCE}>".to_owned(),
            ],
        },
        // 2. Truncated tool call at EOF on OpenAI `/v1/responses` SSE -> must emit `response.failed`
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![
                "Applying patch.\n<agy_tool_call_{NONCE}>\n{\"name\": \"apply_patch\", \"input\": \"*** Begin Patch\\n+unfinished".to_owned(),
            ],
        },
    ]));

    let app = test_app(Arc::clone(&backend), 120);
    let anth_req = json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 4096,
        "stream": true,
        "tools": [
            {
                "name": "Edit",
                "description": "Edit file",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "file_path": { "type": "string" },
                        "old_string": { "type": "string" },
                        "new_string": { "type": "string" }
                    },
                    "required": ["file_path", "old_string", "new_string"]
                }
            }
        ],
        "messages": [{ "role": "user", "content": "Edit src/main.rs" }]
    });

    let t0 = tokio::time::Instant::now();
    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(anth_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let mut body1 = r1.into_body();

    let mut early_anth = String::new();
    while !early_anth.contains("\"type\":\"tool_use\"") {
        let frame = tokio::time::timeout(Duration::from_secs(1), body1.frame())
            .await
            .expect("Anthropic `content_block_start(tool_use)` must flush before 20s closing tag")
            .expect("stream open")
            .expect("valid frame");
        if let Ok(bytes) = frame.into_data() {
            early_anth.push_str(std::str::from_utf8(&bytes).unwrap());
        }
    }
    assert!(t0.elapsed() < Duration::from_secs(1));
    assert!(early_anth.contains("\"name\":\"Edit\""));

    let rest1 = String::from_utf8(body1.collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(rest1.contains("\"type\":\"input_json_delta\""));
    assert!(
        rest1.contains("\\\"file_path\\\":\\\"src/main.rs\\\"")
            || rest1.contains("\\\"file_path\\\": \\\"src/main.rs\\\"")
    );
    assert!(rest1.contains("\"stop_reason\":\"tool_use\""));

    // 2. Truncated tool call on `/v1/responses` SSE emits `response.failed` instead of `response.completed`
    let trunc_req = json!({
        "model": "gpt-5.4",
        "stream": true,
        "tools": [
            {
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply patch",
                "format": { "type": "text" }
            }
        ],
        "input": [{ "role": "user", "content": "Apply patch" }]
    });
    let r2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(trunc_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let sse2 =
        String::from_utf8(r2.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(
        sse2.contains("event: response.failed"),
        "truncated in-progress XML tool call must emit response.failed, got:\n{sse2}"
    );
    assert!(
        !sse2.contains("event: response.completed"),
        "truncated in-progress XML tool call must never emit response.completed"
    );
}

#[tokio::test(start_paused = true)]
async fn apply_patch_containing_acp_warning_banner_literals_streams_intact_without_truncation() {
    let expected_patch = "*** Begin Patch\n*** Update File: crates/liftgate-server/src/turn_runner.rs\n@@\n+    \"⚠ API error (attempt 1): transient 503 — retrying in 1s...\",\n+    \"⚠ Your previous response contained an improperly formatted function call\",\n*** End Patch";
    let escaped_patch = expected_patch
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::PacedChunks {
        chunk_delay: Duration::from_millis(10),
        chunks_template: vec![
            "Docs note: `⚠ API error (attempt 1)` is handled below.\n<agy_tool_call_{NONCE}>{\"name\":\"apply_patch\",\"input\":\""
                .to_owned(),
            escaped_patch,
            "\"}</agy_tool_call_{NONCE}>".to_owned(),
        ],
    }]));

    let app = test_app(Arc::clone(&backend), 60);
    let req = json!({
        "model": "gpt-5.4",
        "stream": true,
        "tools": [
            {
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply patch",
                "format": { "type": "text" }
            }
        ],
        "input": [{ "role": "user", "content": "Update banner constants" }]
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let sse = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(
        sse.contains("event: response.completed"),
        "patch containing `⚠ API error` must complete cleanly, got:\n{sse}"
    );
    assert!(
        !sse.contains("event: response.failed"),
        "patch containing `⚠ API error` must not be truncated by banner filter"
    );

    let mut done_patch: Option<String> = None;
    let mut message_text = String::new();
    for line in sse.lines() {
        if let Some(data) = line.strip_prefix("data: ")
            && let Ok(val) = serde_json::from_str::<Value>(data)
        {
            match val.get("type").and_then(Value::as_str) {
                Some("response.output_text.delta") => {
                    if let Some(d) = val.get("delta").and_then(Value::as_str) {
                        message_text.push_str(d);
                    }
                }
                Some("response.output_item.done") => {
                    if let Some(item) = val.get("item")
                        && item.get("type").and_then(Value::as_str) == Some("custom_tool_call")
                    {
                        done_patch = item.get("input").and_then(Value::as_str).map(str::to_owned);
                    }
                }
                _ => {}
            }
        }
    }
    assert_eq!(
        message_text,
        "Docs note: `⚠ API error (attempt 1)` is handled below.\n"
    );
    assert_eq!(done_patch.as_deref(), Some(expected_patch));
}

#[tokio::test(start_paused = true)]
async fn active_streamed_tool_interrupted_by_thought_or_error_fails_instead_of_completing() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        // 1. OpenAI `/v1/responses`: first tool (`exec_command`) completes, second tool (`apply_patch`)
        //    starts streaming and is interrupted mid-tag by an `AgentThoughtChunk` before closing tag.
        MockScript::UpdatesThenHang {
            initial_chunk_template: "<agy_tool_call_{NONCE}>{\"name\":\"exec_command\",\"arguments\":{\"cmd\":\"git status\"}}</agy_tool_call_{NONCE}><agy_tool_call_{NONCE}>{\"name\":\"apply_patch\",\"input\":\"*** Begin Patch\\n+partial".to_owned(),
            trailing_updates: vec![SessionUpdatePayload::AgentThoughtChunk {
                content: AcpChunkContent {
                    content_type: "text".to_owned(),
                    text: "Wait, rethinking...".to_owned(),
                },
            }],
        },
        // 2. Anthropic `/v1/messages`: assistant prose followed by an open `Edit` tool call that is
        //    interrupted mid-tag by an upstream `429 RESOURCE_EXHAUSTED` error update.
        MockScript::UpdatesThenHang {
            initial_chunk_template: "Applying edit:\n<agy_tool_call_{NONCE}>{\"name\":\"Edit\",\"arguments\":{\"file_path\":\"src/lib.rs\"".to_owned(),
            trailing_updates: vec![SessionUpdatePayload::Error {
                message: "RESOURCE_EXHAUSTED (code 429): No capacity available".to_owned(),
                error: None,
            }],
        },
    ]));

    let app = test_app(Arc::clone(&backend), 120);

    // Case 1: OpenAI `/v1/responses`
    let oai_req = json!({
        "model": "gpt-5.4",
        "stream": true,
        "tools": [
            {
                "type": "function",
                "name": "exec_command",
                "description": "Run command",
                "parameters": {
                    "type": "object",
                    "properties": { "cmd": { "type": "string" } },
                    "required": ["cmd"]
                }
            },
            {
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply patch",
                "format": { "type": "text" }
            }
        ],
        "input": [{ "role": "user", "content": "Check status and patch" }]
    });
    let t0 = tokio::time::Instant::now();
    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(oai_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let sse1 =
        String::from_utf8(r1.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "interrupted second streamed tool must abort in 0ms instead of hanging 120s"
    );
    assert!(
        sse1.contains("event: response.custom_tool_call_input.delta"),
        "expected partial second tool input delta before interruption, got:\n{sse1}"
    );
    assert!(
        sse1.contains("event: response.failed"),
        "unclosed second streamed tool must emit response.failed, got:\n{sse1}"
    );
    assert!(
        !sse1.contains("event: response.completed"),
        "unclosed second streamed tool must never emit response.completed"
    );

    // Case 2: Anthropic `/v1/messages`
    let anth_req = json!({
        "model": "claude-opus-4-6",
        "max_tokens": 4096,
        "stream": true,
        "tools": [
            {
                "name": "Edit",
                "description": "Edit file",
                "input_schema": {
                    "type": "object",
                    "properties": { "file_path": { "type": "string" } },
                    "required": ["file_path"]
                }
            }
        ],
        "messages": [{ "role": "user", "content": "Edit src/lib.rs" }]
    });
    let t1 = tokio::time::Instant::now();
    let r2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(anth_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let sse2 =
        String::from_utf8(r2.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(
        t1.elapsed() < Duration::from_secs(1),
        "mid-tool 429 error must abort in 0ms instead of hanging"
    );
    assert!(
        sse2.contains("\"type\":\"tool_use\""),
        "expected early tool_use block start before 429 error, got:\n{sse2}"
    );
    assert!(
        sse2.contains("event: error") && sse2.contains("rate_limit_error"),
        "mid-tool 429 after assistant prose must emit SSE error(rate_limit_error), got:\n{sse2}"
    );
    assert!(
        !sse2.contains("event: message_stop"),
        "mid-tool 429 with unclosed tool_use block must never emit message_stop"
    );
}

#[tokio::test(start_paused = true)]
async fn pre_body_auth_payload_caps_and_aggregate_memory_budgets_hold() {
    use std::convert::Infallible;
    use tokio_stream::wrappers::ReceiverStream;

    let max_body = 32 * 1024;
    let limits = GatewayLimits::try_new(64 * 1024, max_body, 64, 32, 32, 30).unwrap();
    let mut scripts = Vec::new();
    for _ in 0..14 {
        scripts.push(MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["ack".to_owned()],
        });
    }

    let backend = Arc::new(MockAcpBackend::new(scripts));
    let state = AppState {
        backend: Arc::clone(&backend),
        pool: Arc::new(SessionPool::new(limits)),
        telemetry: Arc::new(TelemetryHub::new()),
        config: GatewayRuntimeConfig {
            title_model_file: None,
            spill_dir: None,
            settings_path: None,
            default_model: ModelId::try_new("gemini-4-carbon-high").unwrap(),
            acp_mode: AcpModeId::try_new("auto").unwrap(),
            api_key: Some(Arc::from("secret-gate-key")),
            limits,
            vision_offload: Arc::new(AtomicBool::new(false)),
            mcp_tool_bridge: Arc::new(AtomicBool::new(true)),
            mcp_server: None,
        },
        ui_dist_dir: None,
        embedded_ui: &[],
        admin_shutdown: None,
    };
    let app = build_router(state.clone());

    // 1. Unauthenticated requests must be rejected BEFORE polling a single body frame.
    for uri in ["/v1/responses", "/v1/messages", "/v1/messages/count_tokens"] {
        let polled = Arc::new(AtomicBool::new(false));
        let polled_flag = Arc::clone(&polled);
        let (tx, rx) = mpsc::channel::<Result<axum::body::Bytes, Infallible>>(2);
        let producer = tokio::spawn(async move {
            if tx
                .send(Ok(axum::body::Bytes::from(vec![b'x'; max_body + 4096])))
                .await
                .is_ok()
            {
                let _ = tx
                    .send(Ok(axum::body::Bytes::from_static(b"trailing")))
                    .await;
            }
        });
        let stream = tokio_stream::StreamExt::map(ReceiverStream::new(rx), move |item| {
            polled_flag.store(true, Ordering::SeqCst);
            item
        });
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from_stream(stream))
                    .unwrap(),
            )
            .await
            .unwrap();
        producer.abort();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "uri={uri}");
        assert!(
            !polled.load(Ordering::SeqCst),
            "unauthenticated request on {uri} must not read body frames before check_auth"
        );
    }

    // 2. Authenticated oversized Content-Length rejects immediately with 413 without reading body,
    //    and chunked oversized bodies abort mid-stream with 413 (`request_too_large`).
    let polled_cl = Arc::new(AtomicBool::new(false));
    let polled_cl_flag = Arc::clone(&polled_cl);
    let (_cl_tx, cl_rx) = mpsc::channel::<Result<axum::body::Bytes, Infallible>>(1);
    let cl_stream = tokio_stream::StreamExt::map(ReceiverStream::new(cl_rx), move |item| {
        polled_cl_flag.store(true, Ordering::SeqCst);
        item
    });
    let cl_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("authorization", "Bearer secret-gate-key")
                .header("content-type", "application/json")
                .header("content-length", (max_body + 1).to_string())
                .body(Body::from_stream(cl_stream))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cl_resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!polled_cl.load(Ordering::SeqCst));
    let cl_json: Value =
        serde_json::from_slice(&cl_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(cl_json["error"]["code"], "request_too_large");

    let chunked_oversized = json!({
        "model": "claude-opus-4-6",
        "max_tokens": 64,
        "messages": [{ "role": "user", "content": "a".repeat(max_body + 1024) }]
    })
    .to_string();
    let anth_413 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("x-api-key", "secret-gate-key")
                .header("content-type", "application/json")
                .body(Body::from_stream(tokio_stream::iter([
                    Ok::<_, Infallible>(axum::body::Bytes::from(chunked_oversized)),
                ])))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anth_413.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let anth_413_json: Value =
        serde_json::from_slice(&anth_413.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(anth_413_json["error"]["type"], "request_too_large");

    // 3. Stateless Anthropic & OpenAI(store:false) turns retain zero message history bytes
    //    while preserving exact telemetry `message_count` and warm prefix-hash continuation.
    let anth_payload = json!({
        "model": "claude-opus-4-6",
        "max_tokens": 128,
        "messages": [{ "role": "user", "content": "z".repeat(16 * 1024) }]
    });
    let anth_ok = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("x-api-key", "secret-gate-key")
                .header("content-type", "application/json")
                .body(Body::from(anth_payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anth_ok.status(), StatusCode::OK);
    let (idle_after_anth, stored_after_anth) = state.pool.retained_memory_bytes();
    assert_eq!(stored_after_anth, 0);
    assert!(
        idle_after_anth < 2048,
        "stateless Anthropic session should retain only compact metadata, got {idle_after_anth} bytes"
    );
    let snap = state.pool.snapshot();
    assert_eq!(snap.sessions.first().map(|s| s.message_count), Some(2));

    let oai_nostore = json!({
        "model": "gpt-5.4",
        "store": false,
        "input": [{ "role": "user", "content": "w".repeat(16 * 1024) }]
    });
    let oai_nostore_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("authorization", "Bearer secret-gate-key")
                .header("content-type", "application/json")
                .body(Body::from(oai_nostore.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(oai_nostore_resp.status(), StatusCode::OK);
    let (_, stored_after_nostore) = state.pool.retained_memory_bytes();
    assert_eq!(stored_after_nostore, 0);

    // 4. Stateful OpenAI(store:true) turns enforce global retained idle & stored response byte caps.
    let aborted_before_evict = backend.aborted_count.load(Ordering::SeqCst);
    let mut last_stateful_resp_id = String::new();
    for idx in 0..10 {
        let stateful_req = json!({
            "model": "gpt-5.4",
            "store": true,
            "instructions": format!("inst-{idx}-{}", "p".repeat(8 * 1024)),
            "input": [{ "role": "user", "content": format!("turn-{idx}-{}", "q".repeat(18 * 1024)) }]
        });
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("authorization", "Bearer secret-gate-key")
                    .header("content-type", "application/json")
                    .body(Body::from(stateful_req.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        if idx == 9 {
            let body_json: Value =
                serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
            last_stateful_resp_id = body_json["id"].as_str().unwrap().to_owned();
        }
    }
    let (retained_idle, retained_resp) = state.pool.retained_memory_bytes();
    assert!(
        retained_idle <= state.pool.max_idle_retained_bytes(),
        "retained_idle={retained_idle} exceeded cap={}",
        state.pool.max_idle_retained_bytes()
    );
    assert!(
        retained_resp <= state.pool.max_stored_response_bytes(),
        "retained_resp={retained_resp} exceeded cap={}",
        state.pool.max_stored_response_bytes()
    );
    let snap_after = state.pool.snapshot();
    assert!(
        snap_after.idle_cached_sessions < 10,
        "expected byte-budget LRU eviction before 32-session count cap, got {}",
        snap_after.idle_cached_sessions
    );
    assert!(backend.aborted_count.load(Ordering::SeqCst) > aborted_before_evict);

    // 5. Stored responses retain compact serialized `Bytes` (eliminating `Value` heap amplification),
    //    and `IdleCheckoutGuard` restores warm sessions if dropped before lease promotion.
    let scalar_tc_req = json!({
        "model": "gpt-5.4",
        "store": true,
        "tool_choice": vec![0_u8; 4000],
        "input": "scalar-tc-stored"
    });
    let scalar_tc_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("authorization", "Bearer secret-gate-key")
                .header("content-type", "application/json")
                .body(Body::from(scalar_tc_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(scalar_tc_resp.status(), StatusCode::OK);
    let scalar_tc_json: Value = serde_json::from_slice(
        &scalar_tc_resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    let scalar_tc_id = scalar_tc_json["id"].as_str().unwrap();
    let fetched_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/responses/{scalar_tc_id}"))
                .header("authorization", "Bearer secret-gate-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(fetched_resp.status(), StatusCode::OK);
    let fetched_json: Value =
        serde_json::from_slice(&fetched_resp.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(fetched_json["tool_choice"].as_array().unwrap().len(), 4000);

    let aborted_before_checkout = backend.aborted_count.load(Ordering::SeqCst);
    {
        let guard = state
            .pool
            .checkout_by_response_id(&last_stateful_resp_id, Arc::clone(&backend))
            .expect("warm stateful session must still be cached");
        drop(guard);
    }
    assert_eq!(
        backend.aborted_count.load(Ordering::SeqCst),
        aborted_before_checkout,
        "dropping IdleCheckoutGuard before RPC must restore warm session without aborting"
    );

    let reused_before = state.pool.snapshot().total_sessions_reused;
    let warm_cont_req = json!({
        "model": "gpt-5.4",
        "store": true,
        "previous_response_id": last_stateful_resp_id,
        "input": [{ "role": "user", "content": "follow up on restored session" }]
    });
    let warm_cont_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("authorization", "Bearer secret-gate-key")
                .header("content-type", "application/json")
                .body(Body::from(warm_cont_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(warm_cont_resp.status(), StatusCode::OK);
    assert_eq!(
        state.pool.snapshot().total_sessions_reused,
        reused_before + 1,
        "warm session restored by IdleCheckoutGuard must be reused cleanly"
    );
}

#[tokio::test]
async fn warm_session_above_300k_offloads_images_when_enabled_using_low_effort_and_preserving_prefix_hash()
 {
    let backend = Arc::new(MockAcpBackend::new(vec![
        // Turn 1 on `sess_1` (`gemini-4-hydrogen-xhigh`): reaches 315,000 tokens (>= 300,000 threshold)
        MockScript::ImmediateWithUsage {
            usage_updates: vec![(315_000, 1_048_576)],
            chunks_template: vec!["Ready to inspect screenshot.".to_owned()],
        },
        // Turn 2 with default `vision_offload == false`: warm `sess_1` receives raw image block directly
        MockScript::ImmediateWithUsage {
            usage_updates: vec![(316_500, 1_048_576)],
            chunks_template: vec!["Inspected screenshot directly on primary session.".to_owned()],
        },
        // Turn 3 (after toggling vision offload ON) ephemeral vision sub-session (`sess_2`): receives raw image on `-low`
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![
                "Dialog title: 'Payment Failed', error code E-402, red button '#retry-btn'."
                    .to_owned(),
            ],
        },
        // Turn 3 primary warm session (`sess_1`): receives text-only `<ephemeral_vision_inspection>`
        MockScript::ImmediateWithUsage {
            usage_updates: vec![(317_700, 1_048_576)],
            chunks_template: vec!["Fix `#retry-btn` handler for E-402.".to_owned()],
        },
        // Turn 4 continuation on warm `sess_1`: verifies prefix hash remained 100% intact!
        MockScript::ImmediateWithUsage {
            usage_updates: vec![(318_300, 1_048_576)],
            chunks_template: vec!["Applied E-402 fix.".to_owned()],
        },
    ]));
    let state = test_state(Arc::clone(&backend), 30, Some("secret-gate-key"));
    let app = build_router(state.clone());

    // Default snapshot must advertise `vision_offload_enabled == false`.
    let snap_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/telemetry/snapshot")
                .header("authorization", "Bearer secret-gate-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(snap_resp.status(), StatusCode::OK);
    let snap_json: Value =
        serde_json::from_slice(&snap_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(snap_json["vision_offload_enabled"], false);

    let msg_u1 = json!({ "role": "user", "content": "Help debug payment UI." });
    let t1_req = json!({
        "model": "gemini-4-hydrogen-xhigh",
        "max_tokens": 1024,
        "messages": [msg_u1]
    });
    let t1_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("x-api-key", "secret-gate-key")
                .header("content-type", "application/json")
                .body(Body::from(t1_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t1_resp.status(), StatusCode::OK);

    // Turn 2 (vision offload disabled by default): raw image block goes directly to warm `sess_1` even at 315k
    let msg_a1 = json!({
        "role": "assistant",
        "content": [{ "type": "text", "text": "Ready to inspect screenshot." }]
    });
    let msg_u2 = json!({
        "role": "user",
        "content": [
            { "type": "text", "text": "Verify initial modal screenshot:" },
            {
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="
                }
            }
        ]
    });
    let t2_req = json!({
        "model": "gemini-4-hydrogen-xhigh",
        "max_tokens": 1024,
        "messages": [msg_u1, msg_a1, msg_u2]
    });
    let t2_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("x-api-key", "secret-gate-key")
                .header("content-type", "application/json")
                .body(Body::from(t2_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t2_resp.status(), StatusCode::OK);
    {
        let prompts = backend.recorded_prompts.lock().unwrap();
        assert_eq!(
            prompts.len(),
            2,
            "by default vision offload is disabled so no ephemeral sub-session may be spawned"
        );
        assert_eq!(prompts[1].session_id, "sess_1");
        assert_eq!(prompts[1].image_blocks.len(), 1);
    }

    // Bodyless POST toggles vision offload ON (`true`)
    let toggle_on = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/telemetry/vision-offload")
                .header("authorization", "Bearer secret-gate-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(toggle_on.status(), StatusCode::OK);
    let toggle_on_json: Value =
        serde_json::from_slice(&toggle_on.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(toggle_on_json["vision_offload_enabled"], true);

    // Turn 3 (vision offload enabled at >=300k): offloads image to `-low` ephemeral sub-session `sess_2`
    let msg_a2 = json!({
        "role": "assistant",
        "content": [{ "type": "text", "text": "Inspected screenshot directly on primary session." }]
    });
    let msg_u3 = json!({
        "role": "user",
        "content": [
            { "type": "text", "text": "Here is the failing checkout modal screenshot:" },
            {
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="
                }
            }
        ]
    });
    let t3_req = json!({
        "model": "gemini-4-hydrogen-xhigh",
        "max_tokens": 1024,
        "messages": [msg_u1, msg_a1, msg_u2, msg_a2, msg_u3]
    });
    let t3_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("x-api-key", "secret-gate-key")
                .header("content-type", "application/json")
                .body(Body::from(t3_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t3_resp.status(), StatusCode::OK);
    let t3_body: Value =
        serde_json::from_slice(&t3_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        t3_body["content"][0]["text"],
        "Fix `#retry-btn` handler for E-402."
    );

    {
        let prompts = backend.recorded_prompts.lock().unwrap();
        assert_eq!(prompts.len(), 4);
        // Ephemeral vision sub-session (`sess_2`) received the raw PNG image
        assert_eq!(prompts[2].session_id, "sess_2");
        assert_eq!(prompts[2].image_blocks.len(), 1);
        assert_eq!(prompts[2].image_blocks[0].0, "image/png");
        // Warm >=300k session (`sess_1`) received ZERO raw images and got the text report instead
        assert_eq!(prompts[3].session_id, "sess_1");
        assert!(
            prompts[3].image_blocks.is_empty(),
            "warm >=300k session with vision offload enabled must receive zero raw image blocks"
        );
        assert!(
            prompts[3]
                .text_joined
                .contains("<ephemeral_vision_inspection images=\"1\">")
        );
        assert!(prompts[3].text_joined.contains("error code E-402"));

        let models = backend.recorded_models.lock().unwrap();
        assert_eq!(
            models.as_slice(),
            &["gemini-4-hydrogen-xhigh", "gemini-4-hydrogen-low"],
            "ephemeral vision sub-session must downgrade reasoning effort to `-low` within the same model family"
        );
    }

    // Verify telemetry recorded `vision_offloaded_images == 1` and `ttft_ms`
    let (_metrics, _cache, _active, recent) = state.telemetry.snapshot_tasks();
    assert_eq!(recent[0].vision_offloaded_images, 1);
    assert!(recent[0].ttft_ms.is_some());

    // Turn 4: verify prefix hash on `sess_1` still matches seamlessly on subsequent coding turns!
    let msg_a3 = json!({
        "role": "assistant",
        "content": [{ "type": "text", "text": "Fix `#retry-btn` handler for E-402." }]
    });
    let msg_u4 = json!({ "role": "user", "content": "Apply the fix now." });
    let t4_req = json!({
        "model": "gemini-4-hydrogen-xhigh",
        "max_tokens": 1024,
        "messages": [msg_u1, msg_a1, msg_u2, msg_a2, msg_u3, msg_a3, msg_u4]
    });
    let t4_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("x-api-key", "secret-gate-key")
                .header("content-type", "application/json")
                .body(Body::from(t4_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t4_resp.status(), StatusCode::OK);
    {
        let prompts = backend.recorded_prompts.lock().unwrap();
        assert_eq!(prompts.len(), 5);
        assert_eq!(
            prompts[4].session_id, "sess_1",
            "subsequent turn after vision offload must still hit warm session prefix hash"
        );
    }

    // Live toggle vision offload OFF via `/api/telemetry/vision-offload?enabled=false`
    let toggle_off = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/telemetry/vision-offload?enabled=false")
                .header("authorization", "Bearer secret-gate-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(toggle_off.status(), StatusCode::OK);
    let toggle_off_json: Value =
        serde_json::from_slice(&toggle_off.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(toggle_off_json["vision_offload_enabled"], false);
}

#[tokio::test]
async fn session_pool_classifies_cache_warmth_and_reaps_expired_idle_sessions() {
    use liftgate_server::session_pool::DEFAULT_MAX_IDLE_SESSION_AGE_MS;
    use liftgate_types::telemetry::PromptCacheWarmth;

    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::ImmediateWithUsage {
        usage_updates: vec![(42_500, 500_000)],
        chunks_template: vec!["Warm pool check.".to_owned()],
    }]));
    let state = test_state(Arc::clone(&backend), 30, None);
    let app = build_router(state.clone());

    let req = json!({
        "model": "claude-opus-4-6",
        "max_tokens": 256,
        "messages": [{ "role": "user", "content": "hello" }]
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let initial_snap = state.pool.snapshot();
    assert_eq!(initial_snap.sessions.len(), 1);
    let base_active_ms = initial_snap.sessions[0].last_active_unix_ms;
    assert_eq!(initial_snap.sessions[0].last_context_tokens, 42_500);

    // < 60s -> Hot
    let hot_snap = state.pool.snapshot_at(base_active_ms + 15_000);
    assert_eq!(hot_snap.sessions[0].cache_warmth, PromptCacheWarmth::Hot);

    // 60s..300s -> Warm
    let warm_snap = state.pool.snapshot_at(base_active_ms + 120_000);
    assert_eq!(warm_snap.sessions[0].cache_warmth, PromptCacheWarmth::Warm);

    // >= 300s -> Cold
    let cold_snap = state.pool.snapshot_at(base_active_ms + 360_000);
    assert_eq!(cold_snap.sessions[0].cache_warmth, PromptCacheWarmth::Cold);

    // Not yet at 45m expiry -> 0 reaped
    assert_eq!(
        state.pool.reap_expired_idle_sessions(
            base_active_ms + 360_000,
            DEFAULT_MAX_IDLE_SESSION_AGE_MS,
            &*backend,
        ),
        0
    );
    assert_eq!(state.pool.idle_and_active_counts(), (0, 1));

    // Past 45m expiry -> reaped and aborted cleanly
    let aborted_before = backend.aborted_count.load(Ordering::SeqCst);
    assert_eq!(
        state.pool.reap_expired_idle_sessions(
            base_active_ms + DEFAULT_MAX_IDLE_SESSION_AGE_MS + 1,
            DEFAULT_MAX_IDLE_SESSION_AGE_MS,
            &*backend,
        ),
        1
    );
    assert_eq!(state.pool.idle_and_active_counts(), (0, 0));
    assert_eq!(
        backend.aborted_count.load(Ordering::SeqCst),
        aborted_before + 1
    );
}

#[tokio::test]
async fn cache_hit_rates_and_bounded_low_cache_records_persist_and_reconfigure_atomically() {
    use liftgate_types::telemetry::{LowCacheHitReason, LowCacheHitsResponse};

    let tmp_dir = std::env::temp_dir().join(format!(
        "liftgate-cache-persist-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let persist_path = tmp_dir.join("cache-telemetry.json");
    let settings_path = tmp_dir.join("settings.json");

    let scripts = vec![
        // Turn 1: Cold session start -> 0 / 1_000 (0.00% < 50% -> recorded as ColdSessionStart)
        MockScript::ImmediateWithExactUsage {
            usage: AcpUsage {
                input_tokens: 1_000,
                cached_read_tokens: 0,
                cached_write_tokens: 0,
                output_tokens: 50,
                thought_tokens: 0,
                total_tokens: 1_050,
            },
            reply: "Reply 1".to_owned(),
        },
        // Turn 2: Warm session reused -> 1_800 / 2_000 (90.00% >= 50% -> NOT low-hit)
        MockScript::ImmediateWithExactUsage {
            usage: AcpUsage {
                input_tokens: 200,
                cached_read_tokens: 1_800,
                cached_write_tokens: 0,
                output_tokens: 60,
                thought_tokens: 0,
                total_tokens: 2_060,
            },
            reply: "Reply 2".to_owned(),
        },
        // Turn 3: Warm session reused, upstream KV cache expired -> 0 / 2_000 (0.00% < 50% -> WarmCacheMiss)
        MockScript::ImmediateWithExactUsage {
            usage: AcpUsage {
                input_tokens: 2_000,
                cached_read_tokens: 0,
                cached_write_tokens: 0,
                output_tokens: 40,
                thought_tokens: 0,
                total_tokens: 2_040,
            },
            reply: "Reply 3".to_owned(),
        },
        // Turn 4: Warm session reused, partial prefix hit -> 500 / 2_000 (25.00% < 50% -> WarmPartialHit)
        MockScript::ImmediateWithExactUsage {
            usage: AcpUsage {
                input_tokens: 1_500,
                cached_read_tokens: 500,
                cached_write_tokens: 0,
                output_tokens: 45,
                thought_tokens: 0,
                total_tokens: 2_045,
            },
            reply: "Reply 4".to_owned(),
        },
        // Turn 5: Fresh cold session -> 0 / 1_000 (0.00% < 50% -> ColdSessionStart; evicts Turn 1 under cap=3)
        MockScript::ImmediateWithExactUsage {
            usage: AcpUsage {
                input_tokens: 1_000,
                cached_read_tokens: 0,
                cached_write_tokens: 0,
                output_tokens: 30,
                thought_tokens: 0,
                total_tokens: 1_030,
            },
            reply: "Reply 5".to_owned(),
        },
    ];

    let backend = Arc::new(MockAcpBackend::new(scripts));
    let mut state = test_state(Arc::clone(&backend), 30, None);
    state.telemetry = Arc::new(TelemetryHub::with_config(CacheTelemetryConfig {
        persist_path: Some(persist_path.clone()),
        settings_path: Some(settings_path.clone()),
        max_low_cache_records: LowCacheRecordLimit::try_new(3).unwrap(),
        low_cache_threshold_bps: CacheHitThresholdBps::try_from_percent(50).unwrap(),
        capture_request_details: true,
    }));
    let app = build_router(state.clone());

    let mut prev_id: Option<String> = None;
    for idx in 1..=4 {
        let mut payload = json!({
            "model": "gemini-4-carbon-high",
            "stream": false,
            "input": format!("Chain turn {idx}"),
        });
        if let Some(pid) = &prev_id {
            payload["previous_response_id"] = json!(pid);
        }
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        prev_id = Some(body["id"].as_str().unwrap().to_owned());
    }

    // Turn 5 without previous_response_id -> new cold session
    let resp5 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "input": "Fresh cold turn 5",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp5.status(), StatusCode::OK);

    // Total prompt = 1000 + 2000 + 2000 + 2000 + 1000 = 8_000; cached_read = 2_300 -> 28.75% (2_875 bps)
    // Warm prompt = 2000 + 2000 + 2000 = 6_000; warm cached_read = 2_300 -> 38.33% (3_833 bps)
    let low_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/telemetry/low-cache-hits")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(low_resp.status(), StatusCode::OK);
    let low_bytes = low_resp.into_body().collect().await.unwrap().to_bytes();
    let low_data: LowCacheHitsResponse = serde_json::from_slice(&low_bytes).unwrap();

    assert_eq!(low_data.summary.total_prompt_tokens, 8_000);
    assert_eq!(low_data.summary.total_cached_read_tokens, 2_300);
    assert_eq!(low_data.summary.total_cache_hit_rate_bps, Some(2_875));
    assert_eq!(low_data.summary.warm_prompt_tokens, 6_000);
    assert_eq!(low_data.summary.warm_cached_read_tokens, 2_300);
    assert_eq!(low_data.summary.warm_cache_hit_rate_bps, Some(3_833));
    assert_eq!(low_data.summary.evaluated_turns, 5);
    assert_eq!(low_data.summary.warm_evaluated_turns, 3);
    assert_eq!(low_data.summary.total_low_cache_turns, 4);
    assert_eq!(low_data.summary.stored_low_cache_count, 3);
    assert_eq!(low_data.records.len(), 3);

    // Newest-first order: Turn 5 (ColdSessionStart), Turn 4 (WarmPartialHit), Turn 3 (WarmCacheMiss)
    assert_eq!(
        low_data.records[0].reason,
        LowCacheHitReason::ColdSessionStart
    );
    assert_eq!(low_data.records[0].prompt_preview, "Fresh cold turn 5");
    assert_eq!(
        low_data.records[1].reason,
        LowCacheHitReason::WarmPartialHit
    );
    assert_eq!(low_data.records[1].cache_hit_rate_bps, 2_500);
    assert_eq!(low_data.records[2].reason, LowCacheHitReason::WarmCacheMiss);
    assert_eq!(low_data.records[2].cache_hit_rate_bps, 0);

    // Verify durable disk persistence across TelemetryHub restart!
    assert!(persist_path.is_file());
    let reloaded_hub = TelemetryHub::with_config(CacheTelemetryConfig {
        persist_path: Some(persist_path.clone()),
        settings_path: Some(settings_path.clone()),
        max_low_cache_records: LowCacheRecordLimit::try_new(3).unwrap(),
        low_cache_threshold_bps: CacheHitThresholdBps::try_from_percent(50).unwrap(),
        capture_request_details: true,
    });
    let reloaded = reloaded_hub.low_cache_hits();
    assert_eq!(reloaded.summary.total_cache_hit_rate_bps, Some(2_875));
    assert_eq!(reloaded.summary.warm_cache_hit_rate_bps, Some(3_833));
    assert_eq!(reloaded.records.len(), 3);
    assert_eq!(reloaded.records[0].prompt_preview, "Fresh cold turn 5");

    // Reject out-of-range limit (> 50_000) with 400 Bad Request
    let bad_cfg = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/telemetry/cache-config")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "max_low_cache_records": 99_999 }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_cfg.status(), StatusCode::BAD_REQUEST);

    // Live resize max_low_cache_records -> 2 and threshold -> 65%, verifying immediate trim & settings.json sync
    let update_cfg = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/telemetry/cache-config")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "max_low_cache_records": 2,
                        "low_cache_threshold_pct": 65,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(update_cfg.status(), StatusCode::OK);
    let after_trim = state.telemetry.low_cache_hits();
    assert_eq!(after_trim.summary.max_low_cache_records, 2);
    assert_eq!(after_trim.summary.low_cache_threshold_bps, 6_500);
    assert_eq!(after_trim.records.len(), 2);

    let saved_settings: Value =
        serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
    assert_eq!(saved_settings["max_low_cache_records"], json!(2));
    assert_eq!(saved_settings["low_cache_threshold_bps"], json!(6_500));

    // Clear low-cache records while preserving cumulative totals
    let clear_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/telemetry/low-cache-hits/clear")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(clear_resp.status(), StatusCode::OK);
    let after_clear = state.telemetry.low_cache_hits();
    assert!(after_clear.records.is_empty());
    assert_eq!(after_clear.summary.total_cache_hit_rate_bps, Some(2_875));

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn soft_cancel_preserves_warm_session_across_post_tool_thought_and_mid_turn_steering_cancel()
{
    let cancel_notify = Arc::new(tokio::sync::Notify::new());
    let backend = Arc::new(MockAcpBackend::new(vec![
        MockScript::SoftCancellableHang {
            cancel_rx: Arc::clone(&cancel_notify),
            cancelled_usage: AcpUsage {
                input_tokens: 8_000,
                output_tokens: 120,
                cached_read_tokens: 142_000,
                cached_write_tokens: 0,
                thought_tokens: 20,
                total_tokens: 150_120,
            },
            initial_updates: vec![
                SessionUpdatePayload::AgentMessageChunk {
                    content: AcpChunkContent {
                        content_type: "text".to_owned(),
                        text: "<agy_tool_call_{NONCE}>\n*** Begin Patch\n*** Update File: a.rs\n@@\n-a\n+b\n*** End Patch\n</agy_tool_call_{NONCE}>".to_owned(),
                    },
                },
                SessionUpdatePayload::AgentThoughtChunk {
                    content: AcpChunkContent {
                        content_type: "text".to_owned(),
                        text: "Post-tool internal thinking that triggers short-circuit".to_owned(),
                    },
                },
            ],
        },
        MockScript::SoftCancellableHang {
            cancel_rx: Arc::clone(&cancel_notify),
            cancelled_usage: AcpUsage {
                input_tokens: 5_000,
                output_tokens: 0,
                cached_read_tokens: 150_000,
                cached_write_tokens: 0,
                thought_tokens: 0,
                total_tokens: 155_000,
            },
            initial_updates: vec![SessionUpdatePayload::UsageUpdate {
                used: 155_000,
                size: 500_000,
            }],
        },
        MockScript::ImmediateWithExactUsage {
            usage: AcpUsage {
                input_tokens: 200,
                output_tokens: 40,
                cached_read_tokens: 155_000,
                cached_write_tokens: 0,
                thought_tokens: 0,
                total_tokens: 155_240,
            },
            reply: "Steering handled on warm session-1!".to_owned(),
        },
    ]));
    backend.enable_soft_cancel_with_notify(Arc::clone(&cancel_notify));

    let state = test_state(Arc::clone(&backend), 30, None);
    let app = build_router(state.clone());

    let tools_json = json!([{
        "type": "custom",
        "name": "apply_patch",
        "description": "Apply patch"
    }]);

    let t1_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "tools": tools_json,
                        "input": [{
                            "role": "user",
                            "content": "Fix a.rs"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t1_resp.status(), StatusCode::OK);
    let t1_body: serde_json::Value =
        serde_json::from_slice(&t1_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let call_id = t1_body["output"][0]["call_id"].as_str().unwrap().to_owned();
    assert_eq!(t1_body["output"][0]["name"], "apply_patch");
    assert_eq!(
        t1_body["output"][0]["input"],
        "*** Begin Patch\n*** Update File: a.rs\n@@\n-a\n+b\n*** End Patch"
    );
    assert_eq!(backend.aborted_count.load(Ordering::SeqCst), 0);

    let t2_input = json!([
        { "role": "user", "content": "Fix a.rs" },
        {
            "type": "custom_tool_call",
            "call_id": call_id,
            "name": "apply_patch",
            "input": "*** Begin Patch\n*** Update File: a.rs\n@@\n-a\n+b\n*** End Patch"
        },
        {
            "type": "custom_tool_call_output",
            "call_id": call_id,
            "output": "Success. Updated a.rs"
        }
    ]);
    let t2_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": true,
                        "tools": tools_json,
                        "input": t2_input,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t2_resp.status(), StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(30)).await;
    drop(t2_resp);
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    let t3_input = json!([
        { "role": "user", "content": "Fix a.rs" },
        {
            "type": "custom_tool_call",
            "call_id": call_id,
            "name": "apply_patch",
            "input": "*** Begin Patch\n*** Update File: a.rs\n@@\n-a\n+b\n*** End Patch"
        },
        {
            "type": "custom_tool_call_output",
            "call_id": call_id,
            "output": "Success. Updated a.rs"
        },
        {
            "role": "user",
            "content": "Also keep reusing the warm session without creating a new one!"
        }
    ]);
    let t3_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "tools": tools_json,
                        "input": t3_input,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t3_resp.status(), StatusCode::OK);

    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 3);
    assert_eq!(prompts[0].session_id, "sess_1");
    assert_eq!(
        prompts[1].session_id, "sess_1",
        "Turn 2 must reuse sess_1 after post-tool thought short-circuit"
    );
    assert_eq!(
        prompts[2].session_id, "sess_1",
        "Turn 3 must reuse sess_1 after mid-turn SSE cancel"
    );
    assert!(
        !prompts[2].text_joined.contains("Success. Updated a.rs"),
        "Turn 3 must send ONLY the new steering message delta, not replay Turn 2 tool output: {}",
        prompts[2].text_joined
    );
    assert!(
        prompts[2]
            .text_joined
            .contains("Also keep reusing the warm session")
    );
    assert_eq!(backend.aborted_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn auxiliary_turns_excluded_and_low_cache_full_payload_capture_toggles_cleanly() {
    use liftgate_types::telemetry::{
        LowCacheDiagnosticCause, LowCacheHitsResponse, LowCachePayloadDetailResponse,
        LowCacheRoutingStrategy,
    };

    let tmp_dir = std::env::temp_dir().join(format!(
        "liftgate-cache-payload-detail-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let persist_path = tmp_dir.join("cache-telemetry.json");
    let settings_path = tmp_dir.join("settings.json");
    let payload_dir = tmp_dir.join("low-cache-payloads");

    let scripts = vec![
        // 1. Ephemeral background title task -> 0 / 12_000 (MUST NOT dilute primary cache hit rate or enter low_cache_records)
        MockScript::ImmediateWithExactUsage {
            usage: AcpUsage {
                input_tokens: 12_000,
                cached_read_tokens: 0,
                cached_write_tokens: 0,
                output_tokens: 12,
                thought_tokens: 0,
                total_tokens: 12_012,
            },
            reply: "Inspect cache telemetry".to_owned(),
        },
        // 2. Interactive turn 1 (with instructions + tool) -> 0 / 4_000 (ColdSessionStart -> full payload captured)
        MockScript::ImmediateWithExactUsage {
            usage: AcpUsage {
                input_tokens: 4_000,
                cached_read_tokens: 0,
                cached_write_tokens: 0,
                output_tokens: 80,
                thought_tokens: 20,
                total_tokens: 4_100,
            },
            reply: "First reply".to_owned(),
        },
        // 3. Interactive turn 2 across <current_date> midnight rollover -> MUST reuse warm session via prefix_hash!
        //    Upstream Gemini implicit replica miss (0 / 5_000) -> WarmUpstreamImplicitMiss with full payload
        MockScript::ImmediateWithExactUsage {
            usage: AcpUsage {
                input_tokens: 5_000,
                cached_read_tokens: 0,
                cached_write_tokens: 0,
                output_tokens: 90,
                thought_tokens: 15,
                total_tokens: 5_105,
            },
            reply: "Second reply".to_owned(),
        },
        // 4. Interactive turn 3 after disabling capture_request_details -> metrics captured, payload omitted
        MockScript::ImmediateWithExactUsage {
            usage: AcpUsage {
                input_tokens: 3_000,
                cached_read_tokens: 0,
                cached_write_tokens: 0,
                output_tokens: 50,
                thought_tokens: 0,
                total_tokens: 3_050,
            },
            reply: "Third reply".to_owned(),
        },
    ];

    let backend = Arc::new(MockAcpBackend::new(scripts));
    let mut state = test_state(Arc::clone(&backend), 30, None);
    state.telemetry = Arc::new(TelemetryHub::with_config(CacheTelemetryConfig {
        persist_path: Some(persist_path.clone()),
        settings_path: Some(settings_path.clone()),
        max_low_cache_records: LowCacheRecordLimit::try_new(10).unwrap(),
        low_cache_threshold_bps: CacheHitThresholdBps::try_from_percent(50).unwrap(),
        capture_request_details: true,
    }));
    let app = build_router(state.clone());

    // Step 1: Send auxiliary title request
    let aux_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "input": "Generate a concise, single-line task title of at most 36 characters for: why is cache hit low",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(aux_resp.status(), StatusCode::OK);

    let after_aux = state.telemetry.low_cache_hits();
    assert_eq!(after_aux.summary.evaluated_turns, 0);
    assert_eq!(after_aux.summary.total_prompt_tokens, 0);
    assert_eq!(after_aux.summary.auxiliary_turns, 1);
    assert_eq!(after_aux.summary.auxiliary_prompt_tokens, 12_000);
    assert!(after_aux.records.is_empty());

    let tools = json!([{
        "type": "function",
        "name": "exec_command",
        "description": "Execute shell command",
        "parameters": {
            "type": "object",
            "properties": { "cmd": { "type": "string" } },
            "required": ["cmd"]
        }
    }]);

    // Step 2: Interactive turn 1 (date = 2026-11-22)
    let t1_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "instructions": "Be concise and precise.",
                        "tools": tools,
                        "input": [
                            { "role": "user", "content": "<environment_context><current_date>2026-11-22</current_date></environment_context>\nAnalyze cache hit rate" }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t1_resp.status(), StatusCode::OK);

    // Step 3: Interactive turn 2 without previous_response_id AND with <current_date> rolled over to 2026-11-23
    let t2_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "instructions": "Be concise and precise.",
                        "tools": tools,
                        "input": [
                            { "role": "user", "content": "<environment_context><current_date>2026-11-23</current_date></environment_context>\nAnalyze cache hit rate" },
                            { "role": "assistant", "content": "First reply" },
                            { "role": "user", "content": "Now optimize it" }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t2_resp.status(), StatusCode::OK);

    let snap = state.telemetry.low_cache_hits();
    assert_eq!(snap.summary.evaluated_turns, 2);
    assert_eq!(snap.summary.warm_evaluated_turns, 1);
    assert_eq!(snap.summary.auxiliary_turns, 1);
    assert_eq!(snap.records.len(), 2);

    // Verify Turn 2 reused the warm session via PrefixHash despite <current_date> midnight rollover!
    let rec_t2 = &snap.records[0];
    assert!(rec_t2.session_reused);
    assert_eq!(
        rec_t2.diagnostic_cause,
        LowCacheDiagnosticCause::WarmUpstreamImplicitMiss
    );
    let m2 = rec_t2.request_metrics.as_ref().expect("metrics populated");
    assert_eq!(m2.routing_strategy, LowCacheRoutingStrategy::PrefixHash);
    assert_eq!(m2.session_turn_index, 2);
    assert!(m2.session_idle_ms.is_some());
    assert_eq!(m2.delta_message_count, 1);
    assert_eq!(m2.client_tool_count, 1);
    assert!(m2.developer_instructions_bytes > 0);
    assert!(m2.outbound_acp_prompt_bytes > 0);
    assert!(rec_t2.request_capture.as_ref().unwrap().full_payload_saved);

    // Fetch full lossless payload over HTTP endpoint
    let detail_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/telemetry/low-cache-hits/{}", rec_t2.task_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(detail_resp.status(), StatusCode::OK);
    let detail_bytes = detail_resp.into_body().collect().await.unwrap().to_bytes();
    let detail: LowCachePayloadDetailResponse = serde_json::from_slice(&detail_bytes).unwrap();
    let full = detail.full_payload.expect("full payload saved on disk");
    assert_eq!(full.client_tools.len(), 1);
    assert_eq!(full.client_tools[0].name, "exec_command");
    assert!(full.messages.iter().any(|m| m.in_delta));
    assert!(!full.outbound_acp_prompt_blocks.is_empty());

    // Step 4: Disable full payload capture dynamically via /api/telemetry/cache-config
    let toggle_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/telemetry/cache-config")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "capture_request_details": false }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(toggle_resp.status(), StatusCode::OK);

    let saved_settings: Value =
        serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
    assert_eq!(
        saved_settings["capture_low_cache_request_details"],
        json!(false)
    );

    // Step 5: Send cold turn 3 while capture_request_details == false
    let t3_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "input": "Turn with payload capture disabled",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t3_resp.status(), StatusCode::OK);

    let after_t3: LowCacheHitsResponse = state.telemetry.low_cache_hits();
    assert!(!after_t3.summary.capture_request_details);
    assert!(after_t3.records[0].request_metrics.is_some());
    assert!(after_t3.records[0].request_capture.is_none());

    // Clearing low-cache records must also delete saved payload JSON files
    assert!(payload_dir.is_dir());
    let _ = state.telemetry.clear_low_cache_hits(true);
    let remaining_files: Vec<_> = std::fs::read_dir(&payload_dir)
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    assert!(remaining_files.is_empty());

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

// ============================================================================
// Hybrid Loopback MCP Tool Bridge + XML Coexistence Integration Suite
// ============================================================================

async fn test_state_with_mcp(
    backend: Arc<MockAcpBackend>,
    timeout_secs: u64,
    settings_path: Option<std::path::PathBuf>,
) -> AppState<MockAcpBackend> {
    let limits =
        GatewayLimits::try_new(16 * 1024 * 1024, 12 * 1024 * 1024, 64, 16, 64, timeout_secs)
            .unwrap();
    let mcp_srv = liftgate_server::mcp_bridge::LoopbackMcpServer::bind(
        limits.max_body_bytes(),
        limits.channel_capacity(),
    )
    .await
    .unwrap();
    let spill_dir = std::env::temp_dir().join("liftgate-mcp-test-spills");

    AppState {
        backend,
        pool: Arc::new(SessionPool::new(limits)),
        telemetry: Arc::new(TelemetryHub::new()),
        config: GatewayRuntimeConfig {
            title_model_file: None,
            spill_dir: Some(spill_dir),
            settings_path,
            default_model: ModelId::try_new("gemini-4-carbon-high").unwrap(),
            acp_mode: AcpModeId::try_new("auto").unwrap(),
            api_key: None,
            limits,
            vision_offload: Arc::new(AtomicBool::new(false)),
            mcp_tool_bridge: Arc::new(AtomicBool::new(true)),
            mcp_server: Some(mcp_srv),
        },
        ui_dist_dir: None,
        embedded_ui: &[],
        admin_shutdown: None,
    }
}

fn parse_sse_events(raw_bytes: &[u8]) -> Vec<(String, Value)> {
    let text = String::from_utf8_lossy(raw_bytes);
    let mut out = Vec::new();
    for frame in text.split("\n\n") {
        let mut ev_name = String::new();
        let mut data_str = String::new();
        for line in frame.lines() {
            if let Some(rest) = line.strip_prefix("event: ") {
                ev_name = rest.trim().to_owned();
            } else if let Some(rest) = line.strip_prefix("data: ") {
                data_str = rest.trim().to_owned();
            }
        }
        if !ev_name.is_empty()
            && !data_str.is_empty()
            && let Ok(val) = serde_json::from_str::<Value>(&data_str)
        {
            out.push((ev_name, val));
        }
    }
    out
}

#[tokio::test]
async fn mcp_handshake_discover_fallback_auth_and_session_drop_unregistration() {
    use liftgate_types::{
        canonical::{CanonicalToolKind, CanonicalToolSpec},
        session_key::NoopSpillSink,
    };

    let limits = GatewayLimits::default();
    let srv = liftgate_server::mcp_bridge::LoopbackMcpServer::bind(
        limits.max_body_bytes(),
        limits.channel_capacity(),
    )
    .await
    .unwrap();
    let tools = vec![
        CanonicalToolSpec {
            name: "exec".to_owned(),
            description: Some("declare const tools: { wait(): Promise<void>; };".to_owned()),
            kind: CanonicalToolKind::Custom { format_json: None },
        },
        CanonicalToolSpec {
            name: "apply_patch".to_owned(),
            description: Some("Freeform patch.".to_owned()),
            kind: CanonicalToolKind::Custom { format_json: None },
        },
        CanonicalToolSpec {
            name: "sql_lark".to_owned(),
            description: Some("Arbitrary custom Lark grammar.".to_owned()),
            kind: CanonicalToolKind::Custom { format_json: None },
        },
    ];
    let catalog = liftgate_server::mcp_bridge::partition_tools_for_mcp(&tools, &mut NoopSpillSink);
    let handle = srv.register_session(catalog);
    let url = handle.server_config().url.clone().unwrap();

    // 1. Invalid / forged tokens must return 401 Unauthorized
    let bad_url = format!(
        "http://127.0.0.1:{}/mcp/0000000000000001deadbeefdeadbeefdeadbeef",
        srv.port()
    );
    let (bad_status, _) = post_loopback_mcp_json(
        &bad_url,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }),
    )
    .await;
    assert_eq!(bad_status, 401);

    // 2. `server/discover` -> 200 OK with `-32601 Method not found`
    let (disc_status, disc_json) = post_loopback_mcp_json(
        &url,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover",
            "params": { "protocolVersion": "2026-07-28" }
        }),
    )
    .await;
    assert_eq!(disc_status, 200);
    assert_eq!(disc_json.unwrap()["error"]["code"], -32601);

    // 3. `tools/list` bridges 100% of custom tools natively (`exec` -> `{code}`,
    //    `apply_patch` -> `{patch}`, `sql_lark` -> `{input}`) plus `call_client_tool` + `search_client_tools`
    let (list_status, list_json) = post_loopback_mcp_json(
        &url,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await;
    assert_eq!(list_status, 200);
    let wire_tools = list_json.unwrap()["result"]["tools"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(wire_tools.len(), 5);
    assert_eq!(wire_tools[0]["name"], "exec");
    assert_eq!(
        wire_tools[0]["inputSchema"]["properties"]["code"]["type"],
        "string"
    );
    assert_eq!(wire_tools[1]["name"], "apply_patch");
    assert_eq!(
        wire_tools[1]["inputSchema"]["properties"]["patch"]["type"],
        "string"
    );
    assert_eq!(wire_tools[2]["name"], "sql_lark");
    assert_eq!(
        wire_tools[2]["inputSchema"]["properties"]["input"]["type"],
        "string"
    );
    assert_eq!(wire_tools[3]["name"], "call_client_tool");
    assert_eq!(wire_tools[4]["name"], "search_client_tools");

    // 4. Dropping `McpSessionHandle` immediately revokes the capability token
    drop(handle);
    let (revoked_status, _) = post_loopback_mcp_json(
        &url,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "ping" }),
    )
    .await;
    assert_eq!(revoked_status, 401);
}

#[tokio::test]
async fn openai_responses_stateful_and_stateless_mcp_multi_step_cascade_and_parallel_siblings() {
    let auth_sum_1 = AcpUsage {
        input_tokens: 10_400,
        output_tokens: 600,
        cached_read_tokens: 19_200,
        cached_write_tokens: 0,
        thought_tokens: 180,
        total_tokens: 30_200,
    };
    let auth_sum_2 = AcpUsage {
        input_tokens: 8_500,
        output_tokens: 300,
        cached_read_tokens: 7_900,
        cached_write_tokens: 0,
        thought_tokens: 80,
        total_tokens: 16_700,
    };

    let backend = Arc::new(MockAcpBackend::new(vec![
        // Session 1: Stateful `previous_response_id` streaming SSE cascade with 2 parallel sibling
        // pops (`U_2 == 0` on second sibling!) + 1 follow-up tool + final reply inside ONE `start_prompt`!
        MockScript::McpInteractive {
            steps: vec![
                McpMockStep::CallTool {
                    usage_used: 9_800,
                    thoughts: vec!["Inspecting Cargo.toml and src/lib.rs in parallel.".to_owned()],
                    pre_text_templates: vec![],
                    mcp_name: "Read".to_owned(),
                    arguments: json!({
                        "file_path": "Cargo.toml",
                        "toolSummary": "Read Cargo.toml"
                    }),
                    expect_reply_substrings: vec!["edition = \"2024\""],
                },
                // Sequential sibling #2 (`maxConcurrent = 1`: `usage_used == 0`, zero LLM call!)
                McpMockStep::CallTool {
                    usage_used: 0,
                    thoughts: vec![],
                    pre_text_templates: vec![],
                    mcp_name: "Read".to_owned(),
                    arguments: json!({
                        "file_path": "src/lib.rs",
                        "toolSummary": "Read src/lib.rs"
                    }),
                    expect_reply_substrings: vec!["pub mod mcp_bridge;"],
                },
                // Step 3: LLM sees both files (`usage_used = 10_900`) and runs `Bash`
                McpMockStep::CallTool {
                    usage_used: 10_900,
                    thoughts: vec!["Running unit tests now.".to_owned()],
                    pre_text_templates: vec![],
                    mcp_name: "Bash".to_owned(),
                    arguments: json!({ "command": "cargo test --locked" }),
                    expect_reply_substrings: vec!["test result: ok. 42 passed"],
                },
                // Step 4: Final answer (`usage_used = 11_800`)
                McpMockStep::FinalReply {
                    usage_used: 11_800,
                    thoughts: vec!["All checks green.".to_owned()],
                    chunks_template: vec!["Workspace verified cleanly.".to_owned()],
                    final_usage: auth_sum_1,
                },
            ],
            cancelled_usage: AcpUsage::default(),
        },
        // Session 2: Stateless `store: false` full-input replay non-streaming cascade
        MockScript::McpInteractive {
            steps: vec![
                McpMockStep::CallTool {
                    usage_used: 8_200,
                    thoughts: vec![],
                    pre_text_templates: vec![],
                    mcp_name: "Read".to_owned(),
                    arguments: json!({ "file_path": "README.md" }),
                    expect_reply_substrings: vec!["# Liftgate"],
                },
                McpMockStep::FinalReply {
                    usage_used: 8_900,
                    thoughts: vec![],
                    chunks_template: vec!["Stateless MCP continuation succeeded.".to_owned()],
                    final_usage: auth_sum_2,
                },
            ],
            cancelled_usage: AcpUsage::default(),
        },
    ]));

    let state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    let app = build_router(state.clone());

    let tools_json = json!([
        {
            "type": "function",
            "name": "Read",
            "description": "Read file contents",
            "parameters": {
                "type": "object",
                "properties": { "file_path": { "type": "string" } },
                "required": ["file_path"]
            }
        },
        {
            "type": "function",
            "name": "Bash",
            "description": "Execute shell command",
            "parameters": {
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }
        }
    ]);

    // --- Part A: Stateful SSE streaming across 4 HTTP requests inside 1 `start_prompt` ---
    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": true,
                        "store": true,
                        "instructions": "Inspect workspace and run tests.",
                        "tools": tools_json,
                        "input": "Check Cargo.toml and src/lib.rs, then run tests."
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let r1_events = parse_sse_events(&r1.into_body().collect().await.unwrap().to_bytes());
    let r1_done = &r1_events
        .iter()
        .find(|(ev, _)| ev == "response.completed")
        .unwrap()
        .1["response"];
    let resp_id_1 = r1_done["id"].as_str().unwrap().to_owned();
    let fc_1 = r1_done["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("expected Read #1 function_call");
    assert_eq!(fc_1["name"], "Read");
    // Injected `toolSummary` must be stripped!
    let fc_1_args: Value = serde_json::from_str(fc_1["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(fc_1_args, json!({ "file_path": "Cargo.toml" }));
    assert_eq!(r1_done["usage"]["input_tokens_details"]["cached_tokens"], 0);

    // Pool must hold 1 `Suspended` session right now!
    let snap_mid = state.pool.snapshot();
    assert_eq!(snap_mid.suspended_mcp_sessions, 1);
    assert_eq!(snap_mid.idle_cached_sessions, 0);
    assert_eq!(
        snap_mid.sessions[0].state,
        liftgate_types::telemetry::SessionSlotState::Suspended
    );

    // HTTP Request 2: Send `function_call_output` for Read #1 -> immediately pops sibling Read #2 (`U_2 == 0`)!
    let call_id_1 = fc_1["call_id"].as_str().unwrap();
    let r2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": true,
                        "store": true,
                        "previous_response_id": resp_id_1,
                        "tools": tools_json,
                        "input": [{
                            "type": "function_call_output",
                            "call_id": call_id_1,
                            "output": "[package]\nname = \"liftgate\"\nedition = \"2024\"\n"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let r2_events = parse_sse_events(&r2.into_body().collect().await.unwrap().to_bytes());
    let r2_done = &r2_events
        .iter()
        .find(|(ev, _)| ev == "response.completed")
        .unwrap()
        .1["response"];
    let resp_id_2 = r2_done["id"].as_str().unwrap().to_owned();
    let fc_2 = r2_done["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("expected sibling Read #2");
    assert_eq!(fc_2["name"], "Read");
    let fc_2_args: Value = serde_json::from_str(fc_2["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(fc_2_args, json!({ "file_path": "src/lib.rs" }));
    assert_eq!(r2_done["usage"]["output_tokens"], 0);

    // HTTP Request 3: Send `function_call_output` for Read #2 -> LLM emits `Bash` (`cached_tokens > 0`)!
    let call_id_2 = fc_2["call_id"].as_str().unwrap();
    let r3 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "store": true,
                        "previous_response_id": resp_id_2,
                        "tools": tools_json,
                        "input": [{
                            "type": "function_call_output",
                            "call_id": call_id_2,
                            "output": "pub mod mcp_bridge;\n"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r3.status(), StatusCode::OK);
    let r3_json: Value =
        serde_json::from_slice(&r3.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let resp_id_3 = r3_json["id"].as_str().unwrap().to_owned();
    assert!(
        r3_json["usage"]["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap()
            > 5_000
    );
    let fc_3 = r3_json["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .unwrap();
    assert_eq!(fc_3["name"], "Bash");

    // HTTP Request 4: Send `function_call_output` for `Bash` -> Final prose answer completes `start_prompt`!
    let call_id_3 = fc_3["call_id"].as_str().unwrap();
    let r4 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "store": true,
                        "previous_response_id": resp_id_3,
                        "tools": tools_json,
                        "input": [{
                            "type": "function_call_output",
                            "call_id": call_id_3,
                            "output": "\u{1b}[32mtest result: ok. 42 passed\u{1b}[0m\n"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r4.status(), StatusCode::OK);
    let r4_json: Value =
        serde_json::from_slice(&r4.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let msg_text = r4_json["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "message")
        .unwrap()["content"][0]["text"]
        .as_str()
        .unwrap();
    assert_eq!(msg_text, "Workspace verified cleanly.");

    // ALL 4 HTTP requests ran inside ONE `start_prompt` RPC with ZERO token drift!
    assert_eq!(backend.recorded_prompts.lock().unwrap().len(), 1);
    let snap_after_part_a = state.pool.snapshot();
    assert_eq!(snap_after_part_a.suspended_mcp_sessions, 0);
    assert_eq!(snap_after_part_a.idle_cached_sessions, 1);
    assert_eq!(snap_after_part_a.mcp_continuations, 3);
    assert_eq!(snap_after_part_a.total_sessions_created, 1);

    let (metrics_a, _, _, _) = state.telemetry.snapshot_tasks();
    assert_eq!(metrics_a.total_input_tokens, auth_sum_1.input_tokens);
    assert_eq!(metrics_a.total_output_tokens, auth_sum_1.output_tokens);
    assert_eq!(metrics_a.total_cached_tokens, auth_sum_1.cached_read_tokens);
    assert_eq!(metrics_a.total_thought_tokens, auth_sum_1.thought_tokens);

    // --- Part B: Stateless (`store: false`, no `previous_response_id`) full-input replay ---
    let s1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "store": false,
                        "instructions": "Read README statelessly.",
                        "tools": tools_json,
                        "input": [{ "role": "user", "content": "Read README.md" }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(s1.status(), StatusCode::OK);
    let s1_json: Value =
        serde_json::from_slice(&s1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let s1_fc = s1_json["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .unwrap();
    let s1_call_id = s1_fc["call_id"].as_str().unwrap();

    let s2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "store": false,
                        "instructions": "Read README statelessly.",
                        "tools": tools_json,
                        "input": [
                            { "role": "user", "content": "Read README.md" },
                            {
                                "type": "function_call",
                                "call_id": s1_call_id,
                                "name": "Read",
                                "arguments": s1_fc["arguments"]
                            },
                            {
                                "type": "function_call_output",
                                "call_id": s1_call_id,
                                "output": "# Liftgate\nFast ACP gateway.\n"
                            }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(s2.status(), StatusCode::OK);
    let s2_json: Value =
        serde_json::from_slice(&s2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        s2_json["output"][0]["content"][0]["text"],
        "Stateless MCP continuation succeeded."
    );
    assert_eq!(backend.recorded_prompts.lock().unwrap().len(), 2);
    assert_eq!(state.pool.snapshot().mcp_continuations, 4);
}

#[tokio::test]
async fn anthropic_messages_streaming_and_nonstreaming_mcp_tool_cascade_preserves_toolu_ids() {
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::McpInteractive {
        steps: vec![
            McpMockStep::CallTool {
                usage_used: 7_400,
                thoughts: vec!["Checking git status first.".to_owned()],
                pre_text_templates: vec![],
                mcp_name: "Bash".to_owned(),
                arguments: json!({ "command": "git status --short" }),
                expect_reply_substrings: vec![
                    "M crates/liftgate-server/src/lib.rs",
                    "<user_followup_message>",
                    "Also summarize in one sentence.",
                ],
            },
            McpMockStep::FinalReply {
                usage_used: 8_100,
                thoughts: vec!["Clean single-file diff.".to_owned()],
                chunks_template: vec!["Only `lib.rs` is modified.".to_owned()],
                final_usage: AcpUsage {
                    input_tokens: 7_900,
                    output_tokens: 240,
                    cached_read_tokens: 5_100,
                    cached_write_tokens: 0,
                    thought_tokens: 90,
                    total_tokens: 13_240,
                },
            },
        ],
        cancelled_usage: AcpUsage::default(),
    }]));

    let state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    let app = build_router(state.clone());

    let anth_tools = json!([{
        "name": "Bash",
        "description": "Execute shell command",
        "input_schema": {
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        }
    }]);

    // Step 1: Streaming Anthropic `/v1/messages` -> emits `toolu_<suffix>` + `stop_reason: "tool_use"`
    let m1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-opus-4-6",
                        "max_tokens": 4096,
                        "stream": true,
                        "thinking": { "type": "enabled", "budget_tokens": 2048 },
                        "system": "You are Claude Code.\nx-anthropic-billing-header: cc_version=2.1.90; cch=11111;",
                        "tools": anth_tools,
                        "messages": [{ "role": "user", "content": "Check git status." }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(m1.status(), StatusCode::OK);
    let m1_events = parse_sse_events(&m1.into_body().collect().await.unwrap().to_bytes());
    let tool_start = m1_events
        .iter()
        .find(|(ev, d)| ev == "content_block_start" && d["content_block"]["type"] == "tool_use")
        .expect("expected tool_use content_block_start");
    let toolu_id = tool_start.1["content_block"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(toolu_id.starts_with("toolu_"));
    assert_eq!(tool_start.1["content_block"]["name"], "Bash");
    let msg_delta = m1_events
        .iter()
        .find(|(ev, _)| ev == "message_delta")
        .unwrap();
    assert_eq!(msg_delta.1["delta"]["stop_reason"], "tool_use");

    // Step 2: Non-streaming Anthropic `/v1/messages` with changing `cch=` billing header + `tool_result` + follow-up text!
    let m2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-opus-4-6",
                        "max_tokens": 4096,
                        "stream": false,
                        "thinking": { "type": "enabled", "budget_tokens": 2048 },
                        "system": "You are Claude Code.\nx-anthropic-billing-header: cc_version=2.1.90; cch=99999;",
                        "tools": anth_tools,
                        "messages": [
                            { "role": "user", "content": "Check git status." },
                            {
                                "role": "assistant",
                                "content": [
                                    { "type": "thinking", "thinking": "Checking git status first.", "signature": "agy_thought_v1" },
                                    { "type": "tool_use", "id": toolu_id, "name": "Bash", "input": { "command": "git status --short" } }
                                ]
                            },
                            {
                                "role": "user",
                                "content": [
                                    { "type": "tool_result", "tool_use_id": toolu_id, "content": " M crates/liftgate-server/src/lib.rs\n" },
                                    { "type": "text", "text": "Also summarize in one sentence." }
                                ]
                            }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(m2.status(), StatusCode::OK);
    let m2_json: Value =
        serde_json::from_slice(&m2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(m2_json["stop_reason"], "end_turn");
    let text_block = m2_json["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "text")
        .unwrap();
    assert_eq!(text_block["text"], "Only `lib.rs` is modified.");
    assert_eq!(backend.recorded_prompts.lock().unwrap().len(), 1);
    assert_eq!(state.pool.snapshot().mcp_continuations, 1);
}

#[tokio::test]
async fn code_mode_exec_wait_and_custom_apply_patch_xml_coexistence_plus_dynamic_tools_update() {
    let raw_js_code =
        "const res = await tools.Read({ file_path: 'src/main.rs' });\nreturn res.slice(0, 64);";
    let raw_patch = "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-fn old() {}\n+fn new_fn() {}\n*** End Patch";

    let backend = Arc::new(MockAcpBackend::new(vec![
        // Single uninterrupted `session/prompt`: `exec` -> dynamic `mcp__slack__post_message` sync
        // -> `wait` -> native 1:1 `apply_patch` -> dynamically added `call_client_tool` -> final reply
        // (with literal `<agy_tool_call_...>` text in prose passing through unintercepted!).
        MockScript::McpInteractive {
            steps: vec![
                McpMockStep::CallTool {
                    usage_used: 11_000,
                    thoughts: vec![],
                    pre_text_templates: vec![],
                    mcp_name: "exec".to_owned(),
                    arguments: json!({
                        "code": raw_js_code,
                        "toolSummary": "Execute CodeMode script"
                    }),
                    expect_reply_substrings: vec![
                        "cell_42 running",
                        "<client_tools_update>",
                        "mcp__slack__post_message",
                    ],
                },
                McpMockStep::CallTool {
                    usage_used: 12_100,
                    thoughts: vec![],
                    pre_text_templates: vec![],
                    mcp_name: "wait".to_owned(),
                    arguments: json!({ "cell_id": "cell_42" }),
                    expect_reply_substrings: vec!["fn old() {}"],
                },
                McpMockStep::CallTool {
                    usage_used: 13_000,
                    thoughts: vec![],
                    pre_text_templates: vec![],
                    mcp_name: "apply_patch".to_owned(),
                    arguments: json!({ "patch": raw_patch }),
                    expect_reply_substrings: vec!["Updated the following files"],
                },
                McpMockStep::CallTool {
                    usage_used: 14_000,
                    thoughts: vec![],
                    pre_text_templates: vec![],
                    mcp_name: "exec".to_owned(),
                    arguments: json!({ "code": "return 'verified';" }),
                    expect_reply_substrings: vec!["verified"],
                },
                McpMockStep::FinalReply {
                    usage_used: 14_500,
                    thoughts: vec![],
                    chunks_template: vec![
                        "Pure MCP complete (literal `<agy_tool_call_demo>ignored</agy_tool_call_demo>` intact).".to_owned(),
                    ],
                    final_usage: AcpUsage {
                        input_tokens: 2_100,
                        output_tokens: 180,
                        cached_read_tokens: 26_200,
                        cached_write_tokens: 0,
                        thought_tokens: 0,
                        total_tokens: 28_480,
                    },
                },
            ],
            cancelled_usage: AcpUsage::default(),
        },
    ]));

    let state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    let app = build_router(state.clone());

    let initial_tools = json!([
        {
            "type": "custom",
            "name": "exec",
            "description": "Execute JavaScript in Code Mode.\ndeclare const tools: {\n  Read(args: { file_path: string }): Promise<string>;\n};"
        },
        {
            "type": "function",
            "name": "wait",
            "description": "Wait on a running Code Mode cell.",
            "parameters": {
                "type": "object",
                "properties": { "cell_id": { "type": "string" } },
                "required": ["cell_id"]
            }
        },
        {
            "type": "custom",
            "name": "apply_patch",
            "description": "Apply a V4A freeform patch.",
            "format": { "type": "grammar", "syntax": "lark", "definition": "start: /.+/" }
        }
    ]);

    // Step 1: Model invokes `mcp_lg_exec` -> OpenAI response must emit `custom_tool_call` with raw JS `input`!
    let t1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "store": true,
                        "tools": initial_tools,
                        "input": "Update src/main.rs using CodeMode and apply_patch."
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t1.status(), StatusCode::OK);
    let t1_json: Value =
        serde_json::from_slice(&t1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let resp_1 = t1_json["id"].as_str().unwrap().to_owned();
    let ctc_exec = &t1_json["output"][0];
    assert_eq!(ctc_exec["type"], "custom_tool_call");
    assert_eq!(ctc_exec["name"], "exec");
    assert_eq!(ctc_exec["input"], raw_js_code);
    let exec_call_id = ctc_exec["call_id"].as_str().unwrap();

    // Verify `system_append` instructed native `mcp_lg_*` AND `<client_tools>` XML ONLY for `apply_patch`
    // (moving `exec` and `wait` onto the native loopback MCP server)!
    {
        let appends = backend.recorded_system_appends.lock().unwrap();
        assert!(appends[0].contains("mcp_lg_*"));
        assert!(
            appends[0].contains("\"name\":\"apply_patch\"") || appends[0].contains("apply_patch")
        );
        assert!(!appends[0].contains("\"name\":\"exec\""));
        assert!(!appends[0].contains("\"name\":\"wait\""));
    }

    // Step 2: Client returns `custom_tool_call_output` AND dynamically adds `mcp__slack__post_message`!
    let mut expanded_tools = initial_tools.as_array().unwrap().clone();
    expanded_tools.push(json!({
        "type": "function",
        "name": "mcp__slack__post_message",
        "description": "Post Slack notification",
        "parameters": {
            "type": "object",
            "properties": { "channel": { "type": "string" } },
            "required": ["channel"]
        }
    }));
    let t2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "store": true,
                        "previous_response_id": resp_1,
                        "tools": expanded_tools,
                        "input": [{
                            "type": "custom_tool_call_output",
                            "call_id": exec_call_id,
                            "output": "cell_42 running"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t2.status(), StatusCode::OK);
    let t2_json: Value =
        serde_json::from_slice(&t2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let resp_2 = t2_json["id"].as_str().unwrap().to_owned();
    let fc_wait = &t2_json["output"][0];
    assert_eq!(fc_wait["type"], "function_call");
    assert_eq!(fc_wait["name"], "wait");
    let wait_call_id = fc_wait["call_id"].as_str().unwrap();

    // Step 3: Client returns `wait` output -> model calls native 1:1 `mcp_lg_apply_patch`!
    let t3 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "store": true,
                        "previous_response_id": resp_2,
                        "tools": expanded_tools,
                        "input": [{
                            "type": "function_call_output",
                            "call_id": wait_call_id,
                            "output": "fn old() {}"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t3.status(), StatusCode::OK);
    let t3_json: Value =
        serde_json::from_slice(&t3.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let resp_3 = t3_json["id"].as_str().unwrap().to_owned();
    let ctc_patch = &t3_json["output"][0];
    assert_eq!(ctc_patch["type"], "custom_tool_call");
    assert_eq!(ctc_patch["name"], "apply_patch");
    assert_eq!(ctc_patch["input"], raw_patch);
    let patch_call_id = ctc_patch["call_id"].as_str().unwrap();

    // Session stays `Suspended` inside the SAME 1 `start_prompt` (zero prompt restart)!
    assert_eq!(backend.recorded_prompts.lock().unwrap().len(), 1);
    assert_eq!(state.pool.snapshot().suspended_mcp_sessions, 1);

    // Step 4 & 5: Client returns `apply_patch` output -> resumes the SAME suspended `session/prompt`, calls `exec`, and finishes!
    let t4 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "store": true,
                        "previous_response_id": resp_3,
                        "tools": expanded_tools,
                        "input": [{
                            "type": "custom_tool_call_output",
                            "call_id": patch_call_id,
                            "output": "Success. Updated the following files:\nM src/main.rs"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t4.status(), StatusCode::OK);
    let t4_json: Value =
        serde_json::from_slice(&t4.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let resp_4 = t4_json["id"].as_str().unwrap().to_owned();
    let exec2_id = t4_json["output"][0]["call_id"].as_str().unwrap();

    let t5 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "store": true,
                        "previous_response_id": resp_4,
                        "tools": expanded_tools,
                        "input": [{
                            "type": "custom_tool_call_output",
                            "call_id": exec2_id,
                            "output": "verified"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t5.status(), StatusCode::OK);
    let t5_json: Value =
        serde_json::from_slice(&t5.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        t5_json["output"][0]["content"][0]["text"],
        "Pure MCP complete (literal `<agy_tool_call_demo>ignored</agy_tool_call_demo>` intact)."
    );
    assert_eq!(backend.recorded_prompts.lock().unwrap().len(), 1);
    assert_eq!(state.pool.snapshot().mcp_continuations, 4);
    assert_eq!(state.pool.snapshot().total_sessions_created, 1);
}

#[tokio::test]
async fn suspended_mcp_graceful_unwind_on_user_interrupt_model_switch_compact_close_and_runtime_toggle()
 {
    let tmp_dir =
        std::env::temp_dir().join(format!("liftgate-mcp-unwind-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let settings_path = tmp_dir.join("settings.json");
    let subtest_e_cancel = Arc::new(tokio::sync::Notify::new());

    let backend = Arc::new(MockAcpBackend::new(vec![
        // 1. Subtest A: Suspends on `Bash`, then unwound on `Esc` user interrupt (no `ToolResult`)
        MockScript::McpInteractive {
            steps: vec![McpMockStep::CallTool {
                usage_used: 6_000,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "Bash".to_owned(),
                arguments: json!({ "command": "sleep 60" }),
                expect_reply_substrings: vec![],
            }],
            cancelled_usage: AcpUsage {
                input_tokens: 5_900,
                output_tokens: 100,
                cached_read_tokens: 0,
                cached_write_tokens: 0,
                thought_tokens: 0,
                total_tokens: 6_000,
            },
        },
        // Warm prompt on same `sess_1` after `Esc` interrupt -> suspends on `Read` for Subtest B (model switch)!
        MockScript::McpInteractive {
            steps: vec![McpMockStep::CallTool {
                usage_used: 6_800,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "Read".to_owned(),
                arguments: json!({ "file_path": "AGENTS.md" }),
                expect_reply_substrings: vec![],
            }],
            cancelled_usage: AcpUsage {
                input_tokens: 800,
                output_tokens: 90,
                cached_read_tokens: 5_910,
                cached_write_tokens: 0,
                thought_tokens: 0,
                total_tokens: 6_800,
            },
        },
        // Warm prompt on same `sess_1` after model switch (`claude-opus-4-6-thinking`) -> suspends on `Read` for Subtest C (`/compact`)!
        MockScript::McpInteractive {
            steps: vec![McpMockStep::CallTool {
                usage_used: 7_500,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "Read".to_owned(),
                arguments: json!({ "file_path": "Cargo.toml" }),
                expect_reply_substrings: vec![],
            }],
            cancelled_usage: AcpUsage {
                input_tokens: 700,
                output_tokens: 80,
                cached_read_tokens: 6_720,
                cached_write_tokens: 0,
                thought_tokens: 0,
                total_tokens: 7_500,
            },
        },
        // Warm `/compact` summarization turn on same `sess_1` (retires `sess_1` on completion!)
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["<summary>Compacted conversation summary.</summary>".to_owned()],
        },
        // 2. Subtest D1 (`sess_2`): Suspends on `Bash`, then manually closed via `POST /api/telemetry/sessions/sess_2/close`
        MockScript::McpInteractive {
            steps: vec![McpMockStep::CallTool {
                usage_used: 5_000,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "Bash".to_owned(),
                arguments: json!({ "command": "echo hold" }),
                expect_reply_substrings: vec![],
            }],
            cancelled_usage: AcpUsage::default(),
        },
        // 3. Subtest D2 (`sess_3`): Runtime toggle `POST /api/telemetry/mcp-tool-bridge` -> pure XML!
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![
                "<agy_tool_call_{NONCE}>{\"name\":\"Bash\",\"arguments\":{\"command\":\"pwd\"}}</agy_tool_call_{NONCE}>".to_owned(),
            ],
        },
        // 4. Subtest E (`sess_4`): Streaming SSE client disconnect mid-MCP-turn cleanly soft-cancels & releases active lease!
        MockScript::SoftCancellableHang {
            cancel_rx: Arc::clone(&subtest_e_cancel),
            cancelled_usage: AcpUsage {
                input_tokens: 4_000,
                output_tokens: 40,
                cached_read_tokens: 0,
                cached_write_tokens: 0,
                thought_tokens: 20,
                total_tokens: 4_060,
            },
            initial_updates: vec![SessionUpdatePayload::AgentMessageChunk {
                content: AcpChunkContent {
                    content_type: "text".to_owned(),
                    text: "Starting MCP streaming step...".to_owned(),
                },
            }],
        },
    ]));
    backend.enable_soft_cancel();

    let state = test_state_with_mcp(Arc::clone(&backend), 30, Some(settings_path.clone())).await;
    let app = build_router(state.clone());

    let tools = json!([
        {
            "name": "Bash",
            "description": "Run shell command",
            "input_schema": {
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }
        },
        {
            "name": "Read",
            "description": "Read file",
            "input_schema": {
                "type": "object",
                "properties": { "file_path": { "type": "string" } },
                "required": ["file_path"]
            }
        }
    ]);

    // --- Subtest A: User `Esc` interrupt (discards assistant `tool_use` and sends replacement user prompt) ---
    let a1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "max_tokens": 1024,
                        "system": "Coding assistant.",
                        "tools": tools,
                        "messages": [{ "role": "user", "content": "Run a slow command." }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(a1.status(), StatusCode::OK);
    assert_eq!(state.pool.snapshot().suspended_mcp_sessions, 1);

    // User presses Esc while `sleep 60` runs -> harness discards assistant turn and sends `[user_1, user_2]`!
    let a2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "max_tokens": 1024,
                        "system": "Coding assistant.",
                        "tools": tools,
                        "messages": [
                            { "role": "user", "content": "Run a slow command." },
                            { "role": "user", "content": "Actually read AGENTS.md instead." }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(a2.status(), StatusCode::OK);
    let a2_json: Value =
        serde_json::from_slice(&a2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let read_toolu_id = a2_json["content"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(a2_json["content"][0]["name"], "Read");
    // Unwound and reused `sess_1` (zero new sessions created)!
    assert_eq!(state.pool.snapshot().total_sessions_created, 1);
    assert_eq!(
        backend.recorded_prompts.lock().unwrap()[1].session_id,
        "sess_1"
    );

    // --- Subtest B: Mid-suspension model switch (`gemini-4-carbon-high` -> `claude-opus-4-6-thinking`) ---
    let b1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-opus-4-6",
                        "max_tokens": 2048,
                        "thinking": { "type": "enabled", "budget_tokens": 1024 },
                        "system": "Coding assistant.",
                        "tools": tools,
                        "messages": [
                            { "role": "user", "content": "Run a slow command." },
                            { "role": "user", "content": "Actually read AGENTS.md instead." },
                            {
                                "role": "assistant",
                                "content": [{
                                    "type": "tool_use",
                                    "id": read_toolu_id,
                                    "name": "Read",
                                    "input": { "file_path": "AGENTS.md" }
                                }]
                            },
                            {
                                "role": "user",
                                "content": [{
                                    "type": "tool_result",
                                    "tool_use_id": read_toolu_id,
                                    "content": "# Repository Guidelines\n"
                                }]
                            }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(b1.status(), StatusCode::OK);
    let b1_json: Value =
        serde_json::from_slice(&b1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let cargo_toolu_id = b1_json["content"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(state.pool.snapshot().total_sessions_created, 1);
    assert!(
        backend
            .recorded_models
            .lock()
            .unwrap()
            .iter()
            .any(|m| m.contains("claude-opus-5-5-high"))
    );

    // --- Subtest C: `/compact` summarization while `sess_1` is suspended on `Read(Cargo.toml)` ---
    let c1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-opus-4-6",
                        "max_tokens": 2048,
                        "system": "You are a helpful AI assistant tasked with summarizing conversations.",
                        "messages": [
                            { "role": "user", "content": "Run a slow command." },
                            { "role": "user", "content": "Actually read AGENTS.md instead." },
                            {
                                "role": "assistant",
                                "content": [{
                                    "type": "tool_use",
                                    "id": read_toolu_id,
                                    "name": "Read",
                                    "input": { "file_path": "AGENTS.md" }
                                }]
                            },
                            {
                                "role": "user",
                                "content": [{
                                    "type": "tool_result",
                                    "tool_use_id": read_toolu_id,
                                    "content": "# Repository Guidelines\n"
                                }]
                            },
                            {
                                "role": "assistant",
                                "content": [{
                                    "type": "tool_use",
                                    "id": cargo_toolu_id,
                                    "name": "Read",
                                    "input": { "file_path": "Cargo.toml" }
                                }]
                            },
                            {
                                "role": "user",
                                "content": "Your task is to create a detailed summary of the conversation so far, paying close attention to the user's explicit requests and your previous actions."
                            }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(c1.status(), StatusCode::OK);
    let c1_json: Value =
        serde_json::from_slice(&c1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(
        c1_json["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Compacted conversation summary")
    );
    // Compaction reused `sess_1` and then retired it!
    assert_eq!(state.pool.snapshot().total_sessions_created, 1);
    assert_eq!(state.pool.snapshot().suspended_mcp_sessions, 0);
    assert_eq!(state.pool.snapshot().idle_cached_sessions, 0);

    // --- Subtest D: Manual close of `Suspended` session + runtime toggle `/api/telemetry/mcp-tool-bridge` ---
    let d1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "max_tokens": 1024,
                        "tools": tools,
                        "messages": [{ "role": "user", "content": "Hold suspended." }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(d1.status(), StatusCode::OK);

    // Check `/api/telemetry/snapshot` JSON contract (`mcp_tool_bridge_enabled: true`, `state: "suspended"`)
    let snap_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/telemetry/snapshot")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let snap_json: Value =
        serde_json::from_slice(&snap_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(snap_json["mcp_tool_bridge_enabled"], true);
    assert_eq!(snap_json["session_pool"]["suspended_mcp_sessions"], 1);
    assert_eq!(
        snap_json["session_pool"]["sessions"][0]["state"],
        "suspended"
    );
    let susp_sid = snap_json["session_pool"]["sessions"][0]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Manually close the `Suspended` session via `/api/telemetry/sessions/{id}/close`
    let close_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/telemetry/sessions/{susp_sid}/close"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(close_resp.status(), StatusCode::OK);
    let close_json: Value =
        serde_json::from_slice(&close_resp.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(close_json["closed"], true);
    assert_eq!(state.pool.snapshot().suspended_mcp_sessions, 0);

    // Toggle MCP Tool Bridge OFF at runtime and verify `settings.json` persistence + pure XML fallback!
    let toggle_off = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/telemetry/mcp-tool-bridge")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(toggle_off.status(), StatusCode::OK);
    let toggle_json: Value =
        serde_json::from_slice(&toggle_off.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(toggle_json["mcp_tool_bridge_enabled"], false);
    let persisted_settings: Value =
        serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
    assert_eq!(persisted_settings["mcp_tool_bridge"], false);

    let xml_turn = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "max_tokens": 1024,
                        "tools": tools,
                        "messages": [{ "role": "user", "content": "Run pwd in XML mode." }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(xml_turn.status(), StatusCode::OK);
    let xml_json: Value =
        serde_json::from_slice(&xml_turn.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(xml_json["stop_reason"], "tool_use");
    assert_eq!(xml_json["content"][0]["name"], "Bash");
    // Session 3 must have been created with `mcp_servers == []`!
    {
        let recorded_mcp = backend.session_mcp_servers.lock().unwrap();
        assert!(
            recorded_mcp
                .iter()
                .find(|(sid, _)| sid == "sess_3")
                .unwrap()
                .1
                .is_empty()
        );
    }

    // --- Subtest E: Toggle MCP Tool Bridge back ON and drop an in-flight SSE stream (client disconnect) ---
    let toggle_on = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/telemetry/mcp-tool-bridge?enabled=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(toggle_on.status(), StatusCode::OK);
    backend.enable_soft_cancel_with_notify(subtest_e_cancel);

    let sse_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": true,
                        "max_tokens": 1024,
                        "tools": tools,
                        "messages": [{ "role": "user", "content": "Stream and disconnect." }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sse_resp.status(), StatusCode::OK);
    let mut sse_body = sse_resp.into_body();
    // Wait until the first SSE data frame arrives (proving `sess_4` MCP driver is actively mid-turn)
    let first_frame = sse_body.frame().await.expect("first SSE frame").unwrap();
    assert!(!first_frame.into_data().unwrap().is_empty());
    // Simulate client disconnect by dropping the SSE body stream!
    drop(sse_body);

    // Driver must immediately cancel and release its active lease cleanly (zero leaked active sessions)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let snap = state.pool.snapshot();
        if snap.active_leased_sessions == 0 {
            assert_eq!(snap.idle_cached_sessions, 2);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for disconnected MCP SSE turn to release active session lease: snap={snap:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn dropping_session_pool_with_parked_mcp_session_breaks_cycle_and_revokes_token() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        MockScript::McpInteractive {
            steps: vec![McpMockStep::CallTool {
                usage_used: 4_000,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "Bash".to_owned(),
                arguments: json!({ "command": "pwd" }),
                expect_reply_substrings: vec![],
            }],
            cancelled_usage: AcpUsage::default(),
        },
        MockScript::McpInteractive {
            steps: vec![McpMockStep::CallTool {
                usage_used: 4_200,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "Bash".to_owned(),
                arguments: json!({ "command": "ls" }),
                expect_reply_substrings: vec![],
            }],
            cancelled_usage: AcpUsage::default(),
        },
    ]));

    let tools = json!([{
        "name": "Bash",
        "description": "Execute shell command",
        "input_schema": {
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        }
    }]);

    // Part 1: Dropping the sole external `Arc<SessionPool>` with a parked `SuspendedMcpEntry`
    // must break cleanly with zero cycle and unregister `McpSessionHandle`.
    let state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    let weak_pool = Arc::downgrade(&state.pool);
    let _mcp_server_keepalive = state.config.mcp_server.clone().expect("mcp server bound");
    let app = build_router(state);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "max_tokens": 1024,
                        "tools": tools,
                        "messages": [{ "role": "user", "content": "Park on MCP tool." }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.into_body().collect().await.unwrap();

    let sess1_mcp_url = {
        let servers = backend.session_mcp_servers.lock().unwrap();
        servers
            .iter()
            .find(|(sid, _)| sid == "sess_1")
            .and_then(|(_, cfgs)| cfgs.first())
            .and_then(|c| c.url.clone())
            .expect("sess_1 MCP capability URL")
    };
    let (alive_status, _) = post_loopback_mcp_json(
        &sess1_mcp_url,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }),
    )
    .await;
    assert_eq!(alive_status, 200);

    // Drop router (the final external owner of `Arc<SessionPool>`).
    drop(app);
    assert!(
        weak_pool.upgrade().is_none(),
        "parked run_mcp_session_driver must not hold a strong Arc<SessionPool> cycle"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let (status, _) = post_loopback_mcp_json(
            &sess1_mcp_url,
            &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
        )
        .await;
        if status == 401 && backend.aborted_count.load(Ordering::SeqCst) >= 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for parked MCP session handle unregistration: last_status={status}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Part 2: `SessionPool::clear_for_shutdown()` drains parked entries and zeroes retained bytes.
    let state2 = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    let app2 = build_router(state2.clone());
    let resp2 = app2
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "max_tokens": 1024,
                        "tools": tools,
                        "messages": [{ "role": "user", "content": "Park second session." }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let _ = resp2.into_body().collect().await.unwrap();
    assert_eq!(state2.pool.snapshot().suspended_mcp_sessions, 1);
    assert!(state2.pool.retained_memory_bytes().0 > 0);

    let sess2_mcp_url = {
        let servers = backend.session_mcp_servers.lock().unwrap();
        servers
            .iter()
            .find(|(sid, _)| sid == "sess_2")
            .and_then(|(_, cfgs)| cfgs.first())
            .and_then(|c| c.url.clone())
            .expect("sess_2 MCP capability URL")
    };
    state2.pool.clear_for_shutdown();
    assert_eq!(state2.pool.snapshot().suspended_mcp_sessions, 0);
    assert_eq!(state2.pool.retained_memory_bytes().0, 0);

    let deadline2 = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let (status, _) = post_loopback_mcp_json(
            &sess2_mcp_url,
            &json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {} }),
        )
        .await;
        if status == 401 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline2,
            "timed out waiting for clear_for_shutdown MCP unregistration"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn mcp_explicit_null_arguments_normalize_to_empty_object_across_openai_and_anthropic() {
    let final_usage = AcpUsage {
        input_tokens: 1_200,
        output_tokens: 120,
        cached_read_tokens: 800,
        cached_write_tokens: 0,
        thought_tokens: 0,
        total_tokens: 2_120,
    };

    let backend = Arc::new(MockAcpBackend::new(vec![
        // Session 1: OpenAI `/v1/responses` receiving explicit `"arguments": null`
        MockScript::McpInteractive {
            steps: vec![
                McpMockStep::CallTool {
                    usage_used: 1_000,
                    thoughts: vec![],
                    pre_text_templates: vec![],
                    mcp_name: "GetStatus".to_owned(),
                    arguments: Value::Null,
                    expect_reply_substrings: vec!["ok_openai"],
                },
                McpMockStep::FinalReply {
                    usage_used: 1_200,
                    thoughts: vec![],
                    chunks_template: vec!["OpenAI status checked.".to_owned()],
                    final_usage,
                },
            ],
            cancelled_usage: AcpUsage::default(),
        },
        // Session 2: Anthropic `/v1/messages` receiving explicit `"arguments": null`
        MockScript::McpInteractive {
            steps: vec![
                McpMockStep::CallTool {
                    usage_used: 1_000,
                    thoughts: vec![],
                    pre_text_templates: vec![],
                    mcp_name: "GetStatus".to_owned(),
                    arguments: Value::Null,
                    expect_reply_substrings: vec!["ok_anthropic"],
                },
                McpMockStep::FinalReply {
                    usage_used: 1_200,
                    thoughts: vec![],
                    chunks_template: vec!["Anthropic status checked.".to_owned()],
                    final_usage,
                },
            ],
            cancelled_usage: AcpUsage::default(),
        },
    ]));

    let state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    let app = build_router(state);

    // 1. OpenAI `/v1/responses` -> `function_call.arguments` MUST be `"{}"` (never `"null"`).
    let oai_tools = json!([{
        "type": "function",
        "name": "GetStatus",
        "description": "Zero-argument status check",
        "parameters": { "type": "object", "properties": {} }
    }]);
    let oai_step1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "tools": oai_tools,
                        "input": "Check OpenAI status."
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(oai_step1.status(), StatusCode::OK);
    let oai_json1: Value =
        serde_json::from_slice(&oai_step1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let resp_id = oai_json1["id"].as_str().unwrap().to_owned();
    let fc_item = oai_json1["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("expected OpenAI function_call item");
    assert_eq!(fc_item["name"], "GetStatus");
    assert_eq!(fc_item["arguments"], "{}");
    let call_id = fc_item["call_id"].as_str().unwrap().to_owned();

    let oai_step2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "previous_response_id": resp_id,
                        "tools": oai_tools,
                        "input": [{
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": "ok_openai"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(oai_step2.status(), StatusCode::OK);
    let oai_json2: Value =
        serde_json::from_slice(&oai_step2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        oai_json2["output"][0]["content"][0]["text"],
        "OpenAI status checked."
    );

    // 2. Anthropic `/v1/messages` -> `tool_use.input` MUST be `{}` (never `null`).
    let anth_tools = json!([{
        "name": "GetStatus",
        "description": "Zero-argument status check",
        "input_schema": { "type": "object", "properties": {} }
    }]);
    let anth_step1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "max_tokens": 1024,
                        "tools": anth_tools,
                        "messages": [{ "role": "user", "content": "Check Anthropic status." }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anth_step1.status(), StatusCode::OK);
    let anth_json1: Value =
        serde_json::from_slice(&anth_step1.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(anth_json1["stop_reason"], "tool_use");
    let tu_block = &anth_json1["content"][0];
    assert_eq!(tu_block["type"], "tool_use");
    assert_eq!(tu_block["name"], "GetStatus");
    assert_eq!(tu_block["input"], json!({}));
    let toolu_id = tu_block["id"].as_str().unwrap().to_owned();

    let anth_step2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "max_tokens": 1024,
                        "tools": anth_tools,
                        "messages": [
                            { "role": "user", "content": "Check Anthropic status." },
                            { "role": "assistant", "content": anth_json1["content"].clone() },
                            {
                                "role": "user",
                                "content": [{
                                    "type": "tool_result",
                                    "tool_use_id": toolu_id,
                                    "content": "ok_anthropic"
                                }]
                            }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anth_step2.status(), StatusCode::OK);
    let anth_json2: Value =
        serde_json::from_slice(&anth_step2.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(
        anth_json2["content"][0]["text"],
        "Anthropic status checked."
    );
}

#[tokio::test]
async fn warm_mcp_turn_rejects_disabled_or_removed_tools_with_is_error_and_completes_text() {
    let usage_turn1 = AcpUsage {
        input_tokens: 2_000,
        output_tokens: 150,
        cached_read_tokens: 1_500,
        cached_write_tokens: 0,
        thought_tokens: 0,
        total_tokens: 3_650,
    };
    let usage_turn2 = AcpUsage {
        input_tokens: 500,
        output_tokens: 180,
        cached_read_tokens: 3_500,
        cached_write_tokens: 0,
        thought_tokens: 0,
        total_tokens: 4_180,
    };
    let usage_turn3 = AcpUsage {
        input_tokens: 400,
        output_tokens: 120,
        cached_read_tokens: 4_000,
        cached_write_tokens: 0,
        thought_tokens: 0,
        total_tokens: 4_520,
    };

    let backend = Arc::new(MockAcpBackend::new(vec![
        // Prompt 1 (Turn 1 cold): both `Read` and `Edit` enabled -> `Read` executes normally.
        MockScript::McpInteractive {
            steps: vec![
                McpMockStep::CallTool {
                    usage_used: 1_800,
                    thoughts: vec![],
                    pre_text_templates: vec![],
                    mcp_name: "Read".to_owned(),
                    arguments: json!({ "file_path": "src/main.rs" }),
                    expect_reply_substrings: vec!["fn main() {}"],
                },
                McpMockStep::FinalReply {
                    usage_used: 2_100,
                    thoughts: vec![],
                    chunks_template: vec!["Inspected src/main.rs.".to_owned()],
                    final_usage: usage_turn1,
                },
            ],
            cancelled_usage: AcpUsage::default(),
        },
        // Prompt 2 (Turn 2 warm Plan Mode): `Edit` removed from `tools` -> calling `mcp_lg_Edit`
        // receives immediate `isError: true` rejection and model finishes with plain text plan!
        MockScript::McpInteractive {
            steps: vec![
                McpMockStep::CallTool {
                    usage_used: 3_900,
                    thoughts: vec!["Attempting Edit in plan mode.".to_owned()],
                    pre_text_templates: vec!["Plan: ".to_owned()],
                    mcp_name: "Edit".to_owned(),
                    arguments: json!({ "file_path": "src/main.rs", "new_str": "nope" }),
                    expect_reply_substrings: vec![
                        "Tool `Edit` is disabled for this turn; do not call it.",
                    ],
                },
                McpMockStep::FinalReply {
                    usage_used: 4_100,
                    thoughts: vec![],
                    chunks_template: vec!["propose changes without editing.".to_owned()],
                    final_usage: usage_turn2,
                },
            ],
            cancelled_usage: AcpUsage::default(),
        },
        // Prompt 3 (Turn 3 warm `tool_choice: "none"`): ALL tools disabled -> calling `mcp_lg_Read`
        // receives immediate `isError: true` rejection and model finishes with plain text!
        MockScript::McpInteractive {
            steps: vec![
                McpMockStep::CallTool {
                    usage_used: 4_300,
                    thoughts: vec![],
                    pre_text_templates: vec![],
                    mcp_name: "Read".to_owned(),
                    arguments: json!({ "file_path": "src/main.rs" }),
                    expect_reply_substrings: vec![
                        "Tool `Read` is disabled for this turn; do not call it.",
                    ],
                },
                McpMockStep::FinalReply {
                    usage_used: 4_500,
                    thoughts: vec![],
                    chunks_template: vec!["Summary with all tools disabled.".to_owned()],
                    final_usage: usage_turn3,
                },
            ],
            cancelled_usage: AcpUsage::default(),
        },
    ]));

    let state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    let app = build_router(state.clone());

    let read_tool = json!({
        "type": "function",
        "name": "Read",
        "description": "Read file",
        "parameters": {
            "type": "object",
            "properties": { "file_path": { "type": "string" } },
            "required": ["file_path"]
        }
    });
    let edit_tool = json!({
        "type": "function",
        "name": "Edit",
        "description": "Edit file",
        "parameters": {
            "type": "object",
            "properties": {
                "file_path": { "type": "string" },
                "new_str": { "type": "string" }
            },
            "required": ["file_path", "new_str"]
        }
    });

    // Turn 1a + 1b: Cold session with `[Read, Edit]`
    let r1a = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "tools": [read_tool.clone(), edit_tool],
                        "input": "Read main.rs."
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1a.status(), StatusCode::OK);
    let j1a: Value =
        serde_json::from_slice(&r1a.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let resp_1a_id = j1a["id"].as_str().unwrap().to_owned();
    let call_1a_id = j1a["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .unwrap()["call_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let r1b = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "previous_response_id": resp_1a_id,
                        "tools": [read_tool.clone(), {
                            "type": "function",
                            "name": "Edit",
                            "description": "Edit file",
                            "parameters": {
                                "type": "object",
                                "properties": {
                                    "file_path": { "type": "string" },
                                    "new_str": { "type": "string" }
                                },
                                "required": ["file_path", "new_str"]
                            }
                        }],
                        "input": [{
                            "type": "function_call_output",
                            "call_id": call_1a_id,
                            "output": "fn main() {}"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1b.status(), StatusCode::OK);
    let j1b: Value =
        serde_json::from_slice(&r1b.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let resp_1b_id = j1b["id"].as_str().unwrap().to_owned();
    assert_eq!(state.pool.snapshot().idle_cached_sessions, 1);

    // Turn 2: Warm Plan Mode (`Edit` removed -> only `[Read]` passed).
    let r2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "previous_response_id": resp_1b_id,
                        "tools": [read_tool.clone()],
                        "input": "Enter plan mode and outline changes."
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let j2: Value =
        serde_json::from_slice(&r2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let resp_2_id = j2["id"].as_str().unwrap().to_owned();
    assert!(
        !j2["output"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call"),
        "removed tool `Edit` must never leak as a function_call: output={}",
        j2["output"]
    );
    let msg2_text: String = j2["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "message")
        .flat_map(|item| item["content"].as_array().unwrap())
        .filter_map(|c| c["text"].as_str())
        .collect();
    assert_eq!(msg2_text, "Plan: propose changes without editing.");

    // Turn 3: Warm `tool_choice: "none"` (all tools disabled).
    let r3 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "previous_response_id": resp_2_id,
                        "tools": [read_tool],
                        "tool_choice": "none",
                        "input": "Summarize without calling any tools."
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r3.status(), StatusCode::OK);
    let j3: Value =
        serde_json::from_slice(&r3.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(
        !j3["output"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call"),
        "tool_choice: none must never leak a function_call: output={}",
        j3["output"]
    );
    assert_eq!(
        j3["output"][0]["content"][0]["text"],
        "Summary with all tools disabled."
    );

    // Verify Turn 3's outbound `<client_tools_update>` explicitly forbade `mcp_lg_*` calls,
    // and all 3 turns reused the single warm `sess_1` session!
    assert_eq!(state.pool.snapshot().total_sessions_created, 1);
    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 3);
    assert!(
        prompts[2].text_joined.contains("mcp_lg_*"),
        "expected empty-tools <client_tools_update> to forbid mcp_lg_* calls: {}",
        prompts[2].text_joined
    );
}

#[tokio::test]
async fn openai_responses_130_tool_virtualized_hubs_zero_rtt_search_preflight_heal_and_dynamic_sync()
 {
    let final_sum = AcpUsage {
        input_tokens: 18_500,
        output_tokens: 920,
        cached_read_tokens: 46_000,
        cached_write_tokens: 0,
        thought_tokens: 210,
        total_tokens: 65_420,
    };

    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::McpInteractive {
        steps: vec![
            // Substep 1 (0-RTT inside Turn 1): search `github` namespace for `merge_pr_42`
            McpMockStep::CallTool {
                usage_used: 12_000,
                thoughts: vec!["Searching GitHub hub for merge_pr_42 schema.".to_owned()],
                pre_text_templates: vec![],
                mcp_name: "search_client_tools".to_owned(),
                arguments: json!({
                    "namespace": "github",
                    "query": "merge_pr_42"
                }),
                expect_reply_substrings: vec![
                    "mcp__codex_apps__github._merge_pr_42",
                    "repository_full_name",
                    "declare const tools:",
                ],
            },
            // Substep 2 (0-RTT preflight self-heal inside Turn 1): pass wrong key `repo_full_name`
            McpMockStep::CallTool {
                usage_used: 12_400,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "ns__github".to_owned(),
                arguments: json!({
                    "action": "merge_pr_42",
                    "args": { "repo_full_name": "acking-you/liftgate", "pr_number": 42 }
                }),
                expect_reply_substrings: vec![
                    "Missing required parameter(s)",
                    "repository_full_name",
                ],
            },
            // Substep 3 (Suspends Turn 1 -> emits `mcp__codex_apps__github._merge_pr_42` over SSE)
            McpMockStep::CallTool {
                usage_used: 12_900,
                thoughts: vec!["Retrying with exact required parameter.".to_owned()],
                pre_text_templates: vec![],
                mcp_name: "ns__github".to_owned(),
                arguments: json!({
                    "action": "_merge_pr_42",
                    "args": { "repository_full_name": "acking-you/liftgate", "pr_number": 42 },
                    "toolSummary": "Merge PR #42"
                }),
                expect_reply_substrings: vec!["merged_sha=deadbeef"],
            },
            // Substep 4 (0-RTT inside Turn 2): search newly added dynamic `slack` tool
            McpMockStep::CallTool {
                usage_used: 14_100,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "search_client_tools".to_owned(),
                arguments: json!({ "query": "slack post_release" }),
                expect_reply_substrings: vec!["mcp__slack__post_release", "call_client_tool"],
            },
            // Substep 5 (Suspends Turn 2 -> emits dynamically added `mcp__slack__post_release`)
            McpMockStep::CallTool {
                usage_used: 14_600,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "call_client_tool".to_owned(),
                arguments: json!({
                    "name": "mcp__slack__post_release",
                    "arguments": { "channel": "#releases", "text": "Merged #42" }
                }),
                expect_reply_substrings: vec!["slack_ok=true"],
            },
            // Substep 6 (Suspends Turn 3 -> invokes direct Code Mode `exec` custom JS tool!)
            McpMockStep::CallTool {
                usage_used: 15_200,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "exec".to_owned(),
                arguments: json!({
                    "code": "const out = await tools.exec_command({ cmd: 'git status -sb' }); text(out.output);",
                    "toolSummary": "Check git status"
                }),
                expect_reply_substrings: vec!["## feat/mcp-tool-virtualization"],
            },
            // Substep 7 (Completes Turn 4)
            McpMockStep::FinalReply {
                usage_used: 16_000,
                thoughts: vec!["All tasks complete.".to_owned()],
                chunks_template: vec![
                    "PR #42 merged, Slack notified, and branch clean.".to_owned(),
                ],
                final_usage: final_sum,
            },
        ],
        cancelled_usage: AcpUsage::default(),
    }]));

    let state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    let app = build_router(state.clone());

    let ts_footer = "\n\nexec tool declaration:\n```ts\ndeclare const tools: { helper(args: { x: string }): Promise<unknown>; };\n```";
    let mut tools_vec = vec![
        json!({
            "type": "custom",
            "name": "exec",
            "description": "Execute JavaScript in Code Mode. globalThis.tools exposes nested tools."
        }),
        json!({
            "type": "function",
            "name": "wait",
            "description": format!("Wait on running exec cell.{ts_footer}"),
            "parameters": {
                "type": "object",
                "properties": { "cell_id": { "type": "string" } },
                "required": ["cell_id"]
            }
        }),
        json!({
            "type": "function",
            "name": "exec_command",
            "description": format!("Execute command in PTY.{ts_footer}"),
            "parameters": {
                "type": "object",
                "properties": { "cmd": { "type": "string" } },
                "required": ["cmd"]
            }
        }),
        json!({
            "type": "custom",
            "name": "apply_patch",
            "description": "Patch files with raw freeform grammar."
        }),
    ];
    for i in 0..85 {
        tools_vec.push(json!({
            "type": "function",
            "name": format!("mcp__codex_apps__github._merge_pr_{i:02}"),
            "description": format!("Merge pull request variant #{i}.{ts_footer}"),
            "parameters": {
                "type": "object",
                "properties": {
                    "repository_full_name": { "type": "string" },
                    "pr_number": { "type": "integer" }
                },
                "required": ["repository_full_name", "pr_number"]
            }
        }));
    }
    for i in 0..40 {
        tools_vec.push(json!({
            "type": "function",
            "name": format!("mcp__codex_tui.thread_op_{i:02}"),
            "description": format!("Codex TUI thread operation #{i}.{ts_footer}"),
            "parameters": {
                "type": "object",
                "properties": { "thread_id": { "type": "string" } },
                "required": ["thread_id"]
            }
        }));
    }
    assert_eq!(tools_vec.len(), 129);

    // Turn 1: Cold request with 129 tools -> 0-RTT search + 0-RTT preflight self-heal -> suspends on `_merge_pr_42`
    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": true,
                        "store": true,
                        "instructions": "Merge PR #42 and announce release.",
                        "tools": tools_vec,
                        "input": "Please merge PR #42, post to Slack, and verify git status."
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let r1_events = parse_sse_events(&r1.into_body().collect().await.unwrap().to_bytes());
    let r1_done = &r1_events
        .iter()
        .find(|(ev, _)| ev == "response.completed")
        .unwrap()
        .1["response"];
    let resp_id_1 = r1_done["id"].as_str().unwrap().to_owned();
    let r1_outputs = r1_done["output"].as_array().unwrap();
    let r1_fcs: Vec<&Value> = r1_outputs
        .iter()
        .filter(|i| i["type"] == "function_call")
        .collect();
    // Neither `search_client_tools` nor the failed preflight call may leak to the caller!
    assert_eq!(r1_fcs.len(), 1);
    assert_eq!(r1_fcs[0]["namespace"], "mcp__codex_apps__github");
    assert_eq!(r1_fcs[0]["name"], "_merge_pr_42");
    let fc1_args: Value = serde_json::from_str(r1_fcs[0]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(
        fc1_args,
        json!({ "repository_full_name": "acking-you/liftgate", "pr_number": 42 })
    );
    let call_id_1 = r1_fcs[0]["call_id"].as_str().unwrap().to_owned();

    // Cold turn outbound prompt must NOT carry any overflow `<client_tools_update>` spill!
    {
        let prompts = backend.recorded_prompts.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(
            !prompts[0].text_joined.contains("<client_tools_update>"),
            "cold virtualized turn should not spill overflow XML tools: {}",
            prompts[0].text_joined
        );
    }

    // Turn 2: Resume `mcp-cont` AND dynamically add `mcp__slack__post_release` mid-session!
    let mut tools_turn_2 = tools_vec.clone();
    tools_turn_2.push(json!({
        "type": "function",
        "name": "mcp__slack__post_release",
        "description": "Post release announcement to Slack channel.",
        "parameters": {
            "type": "object",
            "properties": {
                "channel": { "type": "string" },
                "text": { "type": "string" }
            },
            "required": ["channel", "text"]
        }
    }));
    let r2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": true,
                        "store": true,
                        "previous_response_id": resp_id_1,
                        "tools": tools_turn_2,
                        "input": [{
                            "type": "function_call_output",
                            "call_id": call_id_1,
                            "output": "merged_sha=deadbeef"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let r2_events = parse_sse_events(&r2.into_body().collect().await.unwrap().to_bytes());
    let r2_done = &r2_events
        .iter()
        .find(|(ev, _)| ev == "response.completed")
        .unwrap()
        .1["response"];
    let resp_id_2 = r2_done["id"].as_str().unwrap().to_owned();
    let fc_2 = r2_done["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("expected dynamic slack function_call");
    assert_eq!(fc_2["name"], "mcp__slack__post_release");
    let call_id_2 = fc_2["call_id"].as_str().unwrap().to_owned();

    // Turn 3: Resume `mcp-cont` -> receives direct Code Mode `exec` `custom_tool_call`!
    let r3 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": true,
                        "store": true,
                        "previous_response_id": resp_id_2,
                        "tools": tools_turn_2,
                        "input": [{
                            "type": "function_call_output",
                            "call_id": call_id_2,
                            "output": "slack_ok=true"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r3.status(), StatusCode::OK);
    let r3_events = parse_sse_events(&r3.into_body().collect().await.unwrap().to_bytes());
    let r3_done = &r3_events
        .iter()
        .find(|(ev, _)| ev == "response.completed")
        .unwrap()
        .1["response"];
    let resp_id_3 = r3_done["id"].as_str().unwrap().to_owned();
    let custom_3 = r3_done["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "custom_tool_call")
        .expect("expected direct exec custom_tool_call");
    assert_eq!(custom_3["name"], "exec");
    assert_eq!(
        custom_3["input"],
        "const out = await tools.exec_command({ cmd: 'git status -sb' }); text(out.output);"
    );
    let call_id_3 = custom_3["call_id"].as_str().unwrap().to_owned();

    // Turn 4: Resume `mcp-cont` with `custom_tool_call_output` -> Final answer!
    let r4 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": true,
                        "store": true,
                        "previous_response_id": resp_id_3,
                        "tools": tools_turn_2,
                        "input": [{
                            "type": "custom_tool_call_output",
                            "call_id": call_id_3,
                            "output": "## feat/mcp-tool-virtualization"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r4.status(), StatusCode::OK);
    let r4_events = parse_sse_events(&r4.into_body().collect().await.unwrap().to_bytes());
    let r4_done = &r4_events
        .iter()
        .find(|(ev, _)| ev == "response.completed")
        .unwrap()
        .1["response"];
    let r4_msg = r4_done["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "message")
        .expect("expected final assistant message item");
    assert_eq!(
        r4_msg["content"][0]["text"],
        "PR #42 merged, Slack notified, and branch clean."
    );

    let pool_snap = state.pool.snapshot();
    assert_eq!(pool_snap.total_sessions_created, 1);
    assert_eq!(pool_snap.mcp_continuations, 3);
}

#[tokio::test]
async fn omp_profile_virtualizes_eval_codemode_preserves_hashline_edit_and_routes_mcp_hubs() {
    let hashline_patch =
        "*** Begin Patch\n[src/main.rs#A1B2]\nPUT 5.=7:\n    let port = 8787;\n*** End Patch";

    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::McpInteractive {
        steps: vec![
            // Step 1: Model calls `omp` polyglot `eval` (`py` cell)
            McpMockStep::CallTool {
                usage_used: 9_000,
                thoughts: vec!["Running Python eval cell with unprefixed tool.read.".to_owned()],
                pre_text_templates: vec![],
                mcp_name: "eval".to_owned(),
                arguments: json!({
                    "language": "py",
                    "code": "res = await tool.read({'path': 'src/main.rs'})\nprint(len(res))",
                    "toolSummary": "Read main.rs via eval"
                }),
                expect_reply_substrings: vec!["1024"],
            },
            // Step 2: Model calls `omp` hashline `edit` (`[path#TAG]` + `PUT` inside `*** Begin Patch`)
            McpMockStep::CallTool {
                usage_used: 9_400,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "edit".to_owned(),
                arguments: json!({
                    "input": hashline_patch,
                    "toolSummary": "Apply hashline edit"
                }),
                expect_reply_substrings: vec!["Updated src/main.rs"],
            },
            // Step 3: Model calls single-underscore MCP tool `mcp__github_issue_op_04` via `ns__github`
            McpMockStep::CallTool {
                usage_used: 9_800,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "ns__github".to_owned(),
                arguments: json!({
                    "action": "issue_op_04",
                    "args": { "repo": "can1357/oh-my-pi" }
                }),
                expect_reply_substrings: vec!["issue #128 open"],
            },
            // Step 4: Model calls `omp` memory tool `retain` via `ns__memory`
            McpMockStep::CallTool {
                usage_used: 10_200,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "ns__memory".to_owned(),
                arguments: json!({
                    "action": "retain",
                    "args": { "fact": "omp uses openai-responses with stateless history" }
                }),
                expect_reply_substrings: vec!["retained fact"],
            },
            McpMockStep::FinalReply {
                usage_used: 10_600,
                thoughts: vec![],
                chunks_template: vec!["Oh My Pi workflow verified.".to_owned()],
                final_usage: AcpUsage {
                    input_tokens: 10_600,
                    output_tokens: 48,
                    cached_read_tokens: 10_000,
                    cached_write_tokens: 0,
                    thought_tokens: 32,
                    total_tokens: 10_680,
                },
            },
        ],
        cancelled_usage: AcpUsage::default(),
    }]));

    let state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    let _mcp_keepalive = state.config.mcp_server.clone().expect("mcp server bound");
    let app = build_router(state.clone());

    let mut tools = vec![
        json!({
            "type": "function",
            "name": "eval",
            "description": "Persistent kernels (`py` / `js`); top-level state persists.\n<prelude>await tool.read({ path: 'x' })</prelude>",
            "parameters": {
                "type": "object",
                "properties": {
                    "language": { "type": "string", "enum": ["py", "js"] },
                    "code": { "type": "string" }
                },
                "required": ["language", "code"]
            }
        }),
        json!({
            "type": "function",
            "name": "wait",
            "description": "Wait for background jobs",
            "parameters": { "type": "object", "properties": { "jobs": { "type": "array" } } }
        }),
        json!({
            "type": "function",
            "name": "bash",
            "description": "Execute shell commands",
            "parameters": { "type": "object", "properties": { "command": { "type": "string" } }, "required": ["command"] }
        }),
        json!({
            "type": "function",
            "name": "read",
            "description": "Read files and omp:// URLs",
            "parameters": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }
        }),
        json!({
            "type": "function",
            "name": "edit",
            "description": "Edit files in place with hashline (`[path#TAG]` + `PUT`)",
            "parameters": { "type": "object", "properties": { "input": { "type": "string" } }, "required": ["input"] }
        }),
        json!({
            "type": "function",
            "name": "write",
            "description": "Write files",
            "parameters": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }
        }),
        json!({
            "type": "function",
            "name": "grep",
            "description": "Search contents",
            "parameters": { "type": "object", "properties": { "pattern": { "type": "string" } }, "required": ["pattern"] }
        }),
        json!({
            "type": "function",
            "name": "glob",
            "description": "Find files",
            "parameters": { "type": "object", "properties": { "pattern": { "type": "string" } }, "required": ["pattern"] }
        }),
        json!({
            "type": "function",
            "name": "task",
            "description": "Launch subagents",
            "parameters": { "type": "object", "properties": { "prompt": { "type": "string" } }, "required": ["prompt"] }
        }),
        json!({
            "type": "function",
            "name": "lsp",
            "description": "LSP queries",
            "parameters": { "type": "object", "properties": { "action": { "type": "string" } }, "required": ["action"] }
        }),
    ];
    for mem in ["retain", "recall", "reflect", "memory_edit", "learn"] {
        tools.push(json!({
            "type": "function",
            "name": mem,
            "description": "Memory operation",
            "parameters": {
                "type": "object",
                "properties": { "fact": { "type": "string" } },
                "required": ["fact"]
            }
        }));
    }
    for idx in 0..32 {
        tools.push(json!({
            "type": "function",
            "name": format!("mcp__github_issue_op_{idx:02}"),
            "description": "GitHub MCP operation",
            "parameters": {
                "type": "object",
                "properties": { "repo": { "type": "string" } },
                "required": ["repo"]
            }
        }));
    }

    // Stateless `/v1/responses` history (`store: false`, matching `statefulResponses: false` in `omp`)
    let mut input_items = vec![json!({
        "role": "user",
        "content": "Inspect main.rs, update port to 8787, check GitHub issues, and retain the fact."
    })];

    let send_turn = |items: Vec<Value>| {
        let app_clone = app.clone();
        let tools_clone = tools.clone();
        async move {
            let resp = app_clone
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/responses")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({
                                "model": "gemini-4-carbon-high",
                                "stream": false,
                                "store": false,
                                "instructions": "You are Oh My Pi.",
                                "tools": tools_clone,
                                "input": items
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            serde_json::from_slice::<Value>(&resp.into_body().collect().await.unwrap().to_bytes())
                .unwrap()
        }
    };

    // Turn 1 -> `eval`
    let r1 = send_turn(input_items.clone()).await;
    let fc1 = r1["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("expected eval function_call");
    assert_eq!(fc1["name"], "eval");
    let fc1_args: Value = serde_json::from_str(fc1["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(fc1_args["language"], "py");
    assert!(fc1_args.get("toolSummary").is_none());
    let cid1 = fc1["call_id"].as_str().unwrap().to_owned();
    input_items.push(fc1.clone());
    input_items.push(json!({ "type": "function_call_output", "call_id": cid1, "output": "1024" }));

    // Turn 2 -> `edit` (hashline preserved verbatim!)
    let r2 = send_turn(input_items.clone()).await;
    let fc2 = r2["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("expected edit function_call");
    assert_eq!(fc2["name"], "edit");
    let fc2_args: Value = serde_json::from_str(fc2["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(fc2_args["input"], hashline_patch);
    let cid2 = fc2["call_id"].as_str().unwrap().to_owned();
    input_items.push(fc2.clone());
    input_items.push(
        json!({ "type": "function_call_output", "call_id": cid2, "output": "Updated src/main.rs" }),
    );

    // Turn 3 -> `mcp__github_issue_op_04` via `ns__github`
    let r3 = send_turn(input_items.clone()).await;
    let fc3 = r3["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("expected mcp__github_issue_op_04 function_call");
    assert_eq!(fc3["name"], "mcp__github_issue_op_04");
    let fc3_args: Value = serde_json::from_str(fc3["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(fc3_args["repo"], "can1357/oh-my-pi");
    let cid3 = fc3["call_id"].as_str().unwrap().to_owned();
    input_items.push(fc3.clone());
    input_items.push(
        json!({ "type": "function_call_output", "call_id": cid3, "output": "issue #128 open" }),
    );

    // Turn 4 -> `retain` via `ns__memory`
    let r4 = send_turn(input_items.clone()).await;
    let fc4 = r4["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("expected retain function_call");
    assert_eq!(fc4["name"], "retain");
    let fc4_args: Value = serde_json::from_str(fc4["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(
        fc4_args["fact"],
        "omp uses openai-responses with stateless history"
    );
    let cid4 = fc4["call_id"].as_str().unwrap().to_owned();
    input_items.push(fc4.clone());
    input_items.push(
        json!({ "type": "function_call_output", "call_id": cid4, "output": "retained fact" }),
    );

    // Turn 5 -> Final assistant message
    let r5 = send_turn(input_items).await;
    let msg5 = r5["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "message")
        .expect("expected final assistant message");
    assert_eq!(msg5["content"][0]["text"], "Oh My Pi workflow verified.");

    let pool_snap = state.pool.snapshot();
    assert_eq!(pool_snap.total_sessions_created, 1);
    assert_eq!(pool_snap.mcp_continuations, 4);
}

#[tokio::test]
async fn opencode_profile_virtualizes_codemode_execute_heals_patchtext_and_routes_mcp_hubs() {
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::McpInteractive {
        steps: vec![
            // Step 1: Model calls OpenCode `@opencode/codemode` `execute` directly
            McpMockStep::CallTool {
                usage_used: 8_000,
                thoughts: vec!["Running Code Mode script via execute.".to_owned()],
                pre_text_templates: vec![],
                mcp_name: "execute".to_owned(),
                arguments: json!({
                    "code": "const rows = await tools.playwright.browser_op_02({ selector: '#app' }); return rows;",
                    "toolSummary": "Inspect app selector"
                }),
                expect_reply_substrings: vec!["matched 1 element"],
            },
            // Step 2: Model calls `patch` with malformed hunk line (caught in 0-RTT preflight!)
            McpMockStep::CallTool {
                usage_used: 8_400,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "patch".to_owned(),
                arguments: json!({
                    "patchText": "*** Begin Patch\n*** Update File: src/app.ts\n@@\nmissing_prefix_line()\n*** End Patch"
                }),
                expect_reply_substrings: vec!["Invalid `apply_patch` syntax at line 4"],
            },
            // Step 3: Model retries `patch` with duplicate file sections & line-number header (auto-healed!)
            McpMockStep::CallTool {
                usage_used: 8_900,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "patch".to_owned(),
                arguments: json!({
                    "patchText": "*** Begin Patch\n*** Update File: src/app.ts\n@@ -1,2 +1,2 @@\n const x = 1;\n\n-oldCall();\n+newCall();\n*** Update File: src/app.ts\n@@\n-export const v = 1;\n+export const v = 2;\n*** End Patch"
                }),
                expect_reply_substrings: vec!["patched src/app.ts"],
            },
            // Step 4: Model dispatches an `<server>_<tool>` MCP call via `ns__playwright`
            McpMockStep::CallTool {
                usage_used: 9_400,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "ns__playwright".to_owned(),
                arguments: json!({
                    "action": "browser_op_02",
                    "args": { "selector": "#submit" }
                }),
                expect_reply_substrings: vec!["clicked #submit"],
            },
            McpMockStep::FinalReply {
                usage_used: 9_800,
                thoughts: vec![],
                chunks_template: vec!["OpenCode workflow verified.".to_owned()],
                final_usage: AcpUsage {
                    input_tokens: 9_800,
                    output_tokens: 60,
                    cached_read_tokens: 9_200,
                    cached_write_tokens: 0,
                    thought_tokens: 40,
                    total_tokens: 9_900,
                },
            },
        ],
        cancelled_usage: AcpUsage::default(),
    }]));

    let state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    let _mcp_keepalive = state.config.mcp_server.clone().expect("mcp server bound");
    let app = build_router(state.clone());

    let mut tools = vec![
        json!({
            "name": "execute",
            "description": "Run JavaScript in a confined Code Mode runtime to script tool calls and HTTP requests.",
            "input_schema": {
                "type": "object",
                "properties": { "code": { "type": "string" } },
                "required": ["code"]
            }
        }),
        json!({"name": "shell", "description": "Run shell", "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}}),
        json!({"name": "read", "description": "Read file", "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}),
        json!({"name": "edit", "description": "Edit file", "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}),
        json!({"name": "write", "description": "Write file", "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}),
        json!({"name": "patch", "description": "Apply V4A patch", "input_schema": {"type": "object", "properties": {"patchText": {"type": "string"}}, "required": ["patchText"]}}),
        json!({"name": "grep", "description": "Search contents", "input_schema": {"type": "object", "properties": {"pattern": {"type": "string"}}, "required": ["pattern"]}}),
        json!({"name": "glob", "description": "Match files", "input_schema": {"type": "object", "properties": {"pattern": {"type": "string"}}, "required": ["pattern"]}}),
        json!({"name": "subagent", "description": "Run subagent", "input_schema": {"type": "object", "properties": {"prompt": {"type": "string"}}, "required": ["prompt"]}}),
        json!({"name": "question", "description": "Ask user", "input_schema": {"type": "object", "properties": {"questions": {"type": "array"}}, "required": ["questions"]}}),
    ];
    for idx in 0..30 {
        tools.push(json!({
            "name": format!("playwright_browser_op_{idx:02}"),
            "description": "Playwright browser action",
            "input_schema": {
                "type": "object",
                "properties": { "selector": { "type": "string" } },
                "required": ["selector"]
            }
        }));
    }

    let mut messages = vec![json!({ "role": "user", "content": "Update app.ts with OpenCode." })];

    // Turn 1 -> `execute`
    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "max_tokens": 2048,
                        "tools": tools,
                        "messages": messages
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let j1: Value =
        serde_json::from_slice(&r1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let tu1 = j1["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("execute tool_use");
    assert_eq!(tu1["name"], "execute");
    assert!(tu1["input"].get("toolSummary").is_none());
    let tu1_id = tu1["id"].as_str().unwrap().to_owned();
    messages.push(json!({ "role": "assistant", "content": j1["content"] }));
    messages.push(json!({
        "role": "user",
        "content": [{ "type": "tool_result", "tool_use_id": tu1_id, "content": "matched 1 element" }]
    }));

    // Turn 2 -> 0-RTT preflight catches malformed `patch` in-memory, then returns healed `patch`!
    let r2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "max_tokens": 2048,
                        "tools": tools,
                        "messages": messages
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let j2: Value =
        serde_json::from_slice(&r2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let tu2 = j2["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("healed patch tool_use");
    assert_eq!(tu2["name"], "patch");
    let patch_text = tu2["input"]["patchText"]
        .as_str()
        .expect("patchText string");
    assert_eq!(patch_text.matches("*** Update File: src/app.ts").count(), 1);
    assert!(!patch_text.contains("@@ -1,2 +1,2 @@"));
    let tu2_id = tu2["id"].as_str().unwrap().to_owned();
    messages.push(json!({ "role": "assistant", "content": j2["content"] }));
    messages.push(json!({
        "role": "user",
        "content": [{ "type": "tool_result", "tool_use_id": tu2_id, "content": "patched src/app.ts" }]
    }));

    // Turn 3 -> `ns__playwright` unwraps to `playwright_browser_op_02`
    let r3 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "max_tokens": 2048,
                        "tools": tools,
                        "messages": messages
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r3.status(), StatusCode::OK);
    let j3: Value =
        serde_json::from_slice(&r3.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let tu3 = j3["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("playwright tool_use");
    assert_eq!(tu3["name"], "playwright_browser_op_02");
    assert_eq!(tu3["input"]["selector"], "#submit");
    let tu3_id = tu3["id"].as_str().unwrap().to_owned();
    messages.push(json!({ "role": "assistant", "content": j3["content"] }));
    messages.push(json!({
        "role": "user",
        "content": [{ "type": "tool_result", "tool_use_id": tu3_id, "content": "clicked #submit" }]
    }));

    // Turn 4 -> Final completion in the exact same single ACP session
    let r4 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "max_tokens": 2048,
                        "tools": tools,
                        "messages": messages
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r4.status(), StatusCode::OK);
    let j4: Value =
        serde_json::from_slice(&r4.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let final_text = j4["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "text")
        .unwrap()["text"]
        .as_str()
        .unwrap();
    assert_eq!(final_text, "OpenCode workflow verified.");

    let pool_snap = state.pool.snapshot();
    assert_eq!(pool_snap.total_sessions_created, 1);
    assert_eq!(pool_snap.mcp_continuations, 3);
    let eio = state.telemetry.elastic_io_snapshot();
    assert_eq!(eio.virtualized_sessions, 1);
    assert_eq!(eio.mcp_forwarded_tool_calls, 3);
    assert_eq!(eio.mcp_tool_using_turns, 1);
    assert_eq!(eio.mcp_orchestrator_calls, 1);
    assert_eq!(eio.mcp_patch_calls, 1);
    assert_eq!(eio.mcp_patch_files_total, 1);
    assert_eq!(eio.mcp_zero_rtt_preflight_heals, 1);
}

#[tokio::test]
async fn pure_mcp_zero_tool_warm_upgrade_and_inflight_gateway_web_fetch_preserve_single_session() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        // Turn 1 (0 client tools, e.g. Plan mode) + in-flight `mcp_lg_gateway_web_fetch` in 1 prompt!
        MockScript::McpInteractive {
            steps: vec![
                McpMockStep::CallTool {
                    usage_used: 4_200,
                    thoughts: vec![],
                    pre_text_templates: vec![],
                    mcp_name: "gateway_web_fetch".to_owned(),
                    arguments: json!({ "url": "https://www.weather.gov.sg/weather-forecast-24hrforecast/" }),
                    expect_reply_substrings: vec!["<gateway_web_fetch_result"],
                },
                McpMockStep::FinalReply {
                    usage_used: 4_800,
                    thoughts: vec![],
                    chunks_template: vec!["Plan ready from fetched spec.".to_owned()],
                    final_usage: AcpUsage {
                        input_tokens: 4_200,
                        output_tokens: 120,
                        cached_read_tokens: 0,
                        cached_write_tokens: 0,
                        thought_tokens: 0,
                        total_tokens: 4_320,
                    },
                },
            ],
            cancelled_usage: AcpUsage::default(),
        },
        // Turn 2 (Execute mode attaches `apply_patch` + `exec_command`): reuses `sess_1` with 100%
        // prefix cache preservation (zero cold session creation, zero `<agy_tool_call_>` XML)!
        MockScript::McpInteractive {
            steps: vec![
                McpMockStep::CallTool {
                    usage_used: 5_100,
                    thoughts: vec![],
                    pre_text_templates: vec![],
                    mcp_name: "call_client_tool".to_owned(),
                    arguments: json!({
                        "name": "apply_patch",
                        "arguments": {
                            "patch": "*** Update File: src/plan.rs\n@@\n-old_plan()\n+new_plan()"
                        }
                    }),
                    expect_reply_substrings: vec!["Applied cleanly"],
                },
                McpMockStep::FinalReply {
                    usage_used: 5_400,
                    thoughts: vec![],
                    chunks_template: vec!["Execution complete on warm session.".to_owned()],
                    final_usage: AcpUsage {
                        input_tokens: 400,
                        output_tokens: 90,
                        cached_read_tokens: 4_320,
                        cached_write_tokens: 0,
                        thought_tokens: 0,
                        total_tokens: 4_810,
                    },
                },
            ],
            cancelled_usage: AcpUsage::default(),
        },
    ]));

    let state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    let app = build_router(state.clone());

    // Turn 1: 0 client tools + hosted `web_search` (`external_web_access: true`) -> mounts pure MCP + in-flight `gateway_web_fetch`
    let t1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "store": true,
                        "instructions": "Plan carefully.",
                        "tools": [{ "type": "web_search", "external_web_access": true }],
                        "input": "Read the spec URL and propose a plan."
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t1.status(), StatusCode::OK);
    let t1_json: Value =
        serde_json::from_slice(&t1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let resp_1 = t1_json["id"].as_str().unwrap().to_owned();
    assert_eq!(t1_json["output"][0]["type"], "web_search_call");
    let t1_msg = t1_json["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "message")
        .expect("assistant message output item");
    assert_eq!(
        t1_msg["content"][0]["text"],
        "Plan ready from fetched spec."
    );
    assert_eq!(state.pool.snapshot().total_sessions_created, 1);
    assert_eq!(backend.recorded_prompts.lock().unwrap().len(), 1);
    // System append must be 100% free of `<agy_tool_call_` XML simulation
    let sys_append = backend.recorded_system_appends.lock().unwrap()[0].clone();
    assert!(!sys_append.contains("<agy_tool_call_"));
    assert!(sys_append.contains("mcp_lg_gateway_web_fetch"));

    // Turn 2: Client switches from 0 tools to `[apply_patch, exec_command]` via `previous_response_id`
    let exec_tools = json!([
        {
            "type": "web_search",
            "external_web_access": true
        },
        {
            "type": "custom",
            "name": "apply_patch",
            "description": "Apply V4A patch."
        },
        {
            "type": "function",
            "name": "exec_command",
            "description": "Run command.",
            "parameters": {
                "type": "object",
                "properties": { "cmd": { "type": "string" } },
                "required": ["cmd"]
            }
        }
    ]);
    let t2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "store": true,
                        "previous_response_id": resp_1,
                        "tools": exec_tools,
                        "input": "Proceed and apply the plan."
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t2.status(), StatusCode::OK);
    let t2_json: Value =
        serde_json::from_slice(&t2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let resp_2 = t2_json["id"].as_str().unwrap().to_owned();
    let ctc = &t2_json["output"][0];
    assert_eq!(ctc["type"], "custom_tool_call");
    assert_eq!(ctc["name"], "apply_patch");
    assert_eq!(
        ctc["input"],
        "*** Begin Patch\n*** Update File: src/plan.rs\n@@\n-old_plan()\n+new_plan()\n*** End Patch"
    );
    let patch_cid = ctc["call_id"].as_str().unwrap();
    // Warm session `sess_1` was preserved across 0 -> 2 tool upgrade without cold eviction!
    assert_eq!(state.pool.snapshot().total_sessions_created, 1);
    let turn2_prompt_text = backend.recorded_prompts.lock().unwrap()[1]
        .text_joined
        .clone();
    assert!(turn2_prompt_text.contains("<client_tools_update>"));
    assert!(!turn2_prompt_text.contains("<agy_tool_call_"));

    let t3 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "store": true,
                        "previous_response_id": resp_2,
                        "tools": exec_tools,
                        "input": [{
                            "type": "custom_tool_call_output",
                            "call_id": patch_cid,
                            "output": "Applied cleanly"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(t3.status(), StatusCode::OK);
    let t3_json: Value =
        serde_json::from_slice(&t3.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        t3_json["output"][0]["content"][0]["text"],
        "Execution complete on warm session."
    );
    assert_eq!(state.pool.snapshot().total_sessions_created, 1);
}

#[tokio::test]
async fn codex_native_mcp_apply_patch_preflight_heals_bad_syntax_and_coalesces_duplicate_files() {
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::McpInteractive {
        steps: vec![
            McpMockStep::CallTool {
                usage_used: 9_000,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "apply_patch".to_owned(),
                arguments: json!({
                    "patch": "*** Begin Patch\n*** Update File: src/router.rs\n@@\nunprefixed_context_line\n-old()\n+new()\n*** End Patch",
                    "toolSummary": "Patch router.rs"
                }),
                expect_reply_substrings: vec![
                    "Invalid `apply_patch` syntax at line 4",
                    "src/router.rs",
                ],
            },
            McpMockStep::CallTool {
                usage_used: 9_600,
                thoughts: vec![],
                pre_text_templates: vec!["正在应用修复后的补丁。".to_owned()],
                mcp_name: "apply_patch".to_owned(),
                arguments: json!({
                    "patch": "```patch\r\n*** Update File: src/router.rs\r\n@@\r\n-let a = 1;\r\n+let a = 2;\r\n\r\n let stay = true;\r\n*** Update File: src/router.rs\r\n@@ fn tail()\r\n-let b = 3;\r\n+let b = 4;\r\n```",
                    "toolSummary": "Patch router.rs cleanly"
                }),
                expect_reply_substrings: vec!["Success. Updated the following files:"],
            },
            McpMockStep::FinalReply {
                usage_used: 10_200,
                thoughts: vec![],
                chunks_template: vec!["`src/router.rs` 更新完成。".to_owned()],
                final_usage: AcpUsage {
                    input_tokens: 1_500,
                    output_tokens: 180,
                    cached_read_tokens: 8_500,
                    cached_write_tokens: 0,
                    thought_tokens: 20,
                    total_tokens: 10_200,
                },
            },
        ],
        cancelled_usage: AcpUsage::default(),
    }]));

    let state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    let app = build_router(state.clone());

    let tools = json!([
        {
            "type": "custom",
            "name": "exec",
            "description": "Run JavaScript in Code Mode."
        },
        {
            "type": "custom",
            "name": "apply_patch",
            "description": "Freeform V4A patch tool.",
            "format": { "type": "grammar", "syntax": "lark", "definition": "start: /.+/" }
        }
    ]);

    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": true,
                        "tools": tools,
                        "input": [{ "role": "user", "content": "Update src/router.rs" }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let r1_events = parse_sse_events(&r1.into_body().collect().await.unwrap().to_bytes());
    let r1_done = &r1_events
        .iter()
        .find(|(ev, _)| ev == "response.completed")
        .unwrap()
        .1["response"];
    let r1_id = r1_done["id"].as_str().unwrap().to_owned();
    let ctc_items: Vec<&Value> = r1_done["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "custom_tool_call")
        .collect();
    assert_eq!(
        ctc_items.len(),
        1,
        "failed preflight call must never leak to client"
    );
    assert_eq!(ctc_items[0]["name"], "apply_patch");
    assert_eq!(
        ctc_items[0]["input"].as_str().unwrap(),
        "*** Begin Patch\n*** Update File: src/router.rs\n@@\n-let a = 1;\n+let a = 2;\n \n let stay = true;\n@@ fn tail()\n-let b = 3;\n+let b = 4;\n*** End Patch"
    );
    let ctc_call_id = ctc_items[0]["call_id"].as_str().unwrap().to_owned();

    let r2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "previous_response_id": r1_id,
                        "tools": tools,
                        "input": [{
                            "type": "custom_tool_call_output",
                            "call_id": ctc_call_id,
                            "output": "Success. Updated the following files:\nM src/router.rs"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let r2_json: Value =
        serde_json::from_slice(&r2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let final_msg = r2_json["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "message")
        .unwrap()["content"][0]["text"]
        .as_str()
        .unwrap();
    assert_eq!(final_msg, "`src/router.rs` 更新完成。");
}

#[tokio::test]
async fn anthropic_messages_claude_code_profile_virtualizes_pascal_families_plugins_and_backfills_standalones()
 {
    let final_usage = AcpUsage {
        input_tokens: 14_200,
        output_tokens: 610,
        cached_read_tokens: 51_000,
        cached_write_tokens: 0,
        thought_tokens: 180,
        total_tokens: 65_810,
    };

    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::McpInteractive {
        steps: vec![
            McpMockStep::CallTool {
                usage_used: 11_000,
                thoughts: vec!["Creating task and loading skill.".to_owned()],
                pre_text_templates: vec![],
                mcp_name: "ns__tasks".to_owned(),
                arguments: json!({
                    "action": "TaskCreate",
                    "args": { "subject": "Audit Playwright E2E", "description": "Check login selector" },
                    "toolSummary": "Create tracking task"
                }),
                expect_reply_substrings: vec!["task_id=task-1"],
            },
            McpMockStep::CallTool {
                usage_used: 12_100,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "Skill".to_owned(),
                arguments: json!({
                    "skill": "chrome-devtools",
                    "toolSummary": "Load browser skill"
                }),
                expect_reply_substrings: vec!["skill_loaded=true"],
            },
            McpMockStep::CallTool {
                usage_used: 12_800,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "search_client_tools".to_owned(),
                arguments: json!({
                    "namespace": "playwright",
                    "query": "browser_op_07"
                }),
                expect_reply_substrings: vec![
                    "mcp__plugin_playwright_playwright__browser_op_07",
                    "ns__playwright",
                ],
            },
            McpMockStep::CallTool {
                usage_used: 13_400,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "ns__playwright".to_owned(),
                arguments: json!({
                    "action": "browser_op_07",
                    "args": { "selector": "#login-btn" }
                }),
                expect_reply_substrings: vec!["clicked=#login-btn"],
            },
            McpMockStep::FinalReply {
                usage_used: 14_200,
                thoughts: vec!["Playwright audit complete.".to_owned()],
                chunks_template: vec!["Claude Code workflow verified.".to_owned()],
                final_usage,
            },
        ],
        cancelled_usage: AcpUsage::default(),
    }]));

    let state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    let app = build_router(state.clone());

    let mut claude_tools = Vec::new();
    for core in [
        "REPL",
        "Bash",
        "Read",
        "Edit",
        "Write",
        "MultiEdit",
        "Grep",
        "Glob",
        "Agent",
        "LSP",
        "ToolSearch",
    ] {
        claude_tools.push(json!({
            "name": core,
            "description": format!("Claude Code core {core}"),
            "input_schema": {
                "type": "object",
                "properties": { "cmd": { "type": "string" } }
            }
        }));
    }
    claude_tools.push(json!({
        "name": "TaskCreate",
        "description": "Create a structured task item.",
        "input_schema": {
            "type": "object",
            "properties": {
                "subject": { "type": "string" },
                "description": { "type": "string" }
            },
            "required": ["subject", "description"]
        }
    }));
    for family in [
        "TodoWrite",
        "TaskUpdate",
        "TaskList",
        "EnterPlanMode",
        "ExitPlanMode",
        "EnterWorktree",
        "TeamCreate",
        "SendMessage",
        "CronCreate",
        "ScheduleWakeup",
        "ListMcpResourcesTool",
        "ReadMcpResourceTool",
    ] {
        claude_tools.push(json!({
            "name": family,
            "description": format!("Claude Code built-in {family}"),
            "input_schema": {
                "type": "object",
                "properties": { "id": { "type": "string" } }
            }
        }));
    }
    claude_tools.push(json!({
        "name": "AskUserQuestion",
        "description": "Ask user clarifying questions.",
        "input_schema": {
            "type": "object",
            "properties": { "question": { "type": "string" } },
            "required": ["question"]
        }
    }));
    claude_tools.push(json!({
        "name": "Skill",
        "description": "Load a Claude Code skill.",
        "input_schema": {
            "type": "object",
            "properties": { "skill": { "type": "string" } },
            "required": ["skill"]
        }
    }));
    for i in 0..25 {
        claude_tools.push(json!({
            "name": format!("mcp__plugin_playwright_playwright__browser_op_{i:02}"),
            "description": format!("Playwright plugin op #{i}"),
            "input_schema": {
                "type": "object",
                "properties": { "selector": { "type": "string" } },
                "required": ["selector"]
            }
        }));
    }
    for i in 0..34 {
        claude_tools.push(json!({
            "name": format!("mcp__claude_ai_Linear__issue_op_{i:02}"),
            "description": format!("Linear cloud connector op #{i}"),
            "input_schema": {
                "type": "object",
                "properties": { "issue_id": { "type": "string" } },
                "required": ["issue_id"]
            }
        }));
    }
    assert_eq!(claude_tools.len(), 85);

    let mut messages =
        vec![json!({ "role": "user", "content": "Audit the login button via Playwright." })];
    let m1_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-sonnet-4-6",
                        "max_tokens": 4096,
                        "stream": false,
                        "tools": claude_tools,
                        "messages": messages
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(m1_resp.status(), StatusCode::OK);
    let m1_json: Value =
        serde_json::from_slice(&m1_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let tu1 = m1_json["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("expected TaskCreate tool_use");
    assert_eq!(tu1["name"], "TaskCreate");
    assert_eq!(tu1["input"]["subject"], "Audit Playwright E2E");
    assert!(tu1["input"].get("toolSummary").is_none());
    let tu1_id = tu1["id"].as_str().unwrap().to_owned();

    messages.push(json!({ "role": "assistant", "content": m1_json["content"] }));
    messages.push(json!({
        "role": "user",
        "content": [{ "type": "tool_result", "tool_use_id": tu1_id, "content": "task_id=task-1" }]
    }));
    let m2_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-sonnet-4-6",
                        "max_tokens": 4096,
                        "stream": false,
                        "tools": claude_tools,
                        "messages": messages
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(m2_resp.status(), StatusCode::OK);
    let m2_json: Value =
        serde_json::from_slice(&m2_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let tu2 = m2_json["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("expected backfilled Skill tool_use");
    assert_eq!(tu2["name"], "Skill");
    assert_eq!(tu2["input"]["skill"], "chrome-devtools");
    let tu2_id = tu2["id"].as_str().unwrap().to_owned();

    messages.push(json!({ "role": "assistant", "content": m2_json["content"] }));
    messages.push(json!({
        "role": "user",
        "content": [{ "type": "tool_result", "tool_use_id": tu2_id, "content": "skill_loaded=true" }]
    }));
    let m3_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-sonnet-4-6",
                        "max_tokens": 4096,
                        "stream": false,
                        "tools": claude_tools,
                        "messages": messages
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(m3_resp.status(), StatusCode::OK);
    let m3_json: Value =
        serde_json::from_slice(&m3_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let tu3 = m3_json["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("expected Playwright plugin tool_use");
    assert_eq!(
        tu3["name"],
        "mcp__plugin_playwright_playwright__browser_op_07"
    );
    assert_eq!(tu3["input"]["selector"], "#login-btn");
    let tu3_id = tu3["id"].as_str().unwrap().to_owned();

    messages.push(json!({ "role": "assistant", "content": m3_json["content"] }));
    messages.push(json!({
        "role": "user",
        "content": [{ "type": "tool_result", "tool_use_id": tu3_id, "content": "clicked=#login-btn" }]
    }));
    let m4_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-sonnet-4-6",
                        "max_tokens": 4096,
                        "stream": false,
                        "tools": claude_tools,
                        "messages": messages
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(m4_resp.status(), StatusCode::OK);
    let m4_json: Value =
        serde_json::from_slice(&m4_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let final_text = m4_json["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "text")
        .unwrap()["text"]
        .as_str()
        .unwrap();
    assert_eq!(final_text, "Claude Code workflow verified.");

    let pool_snap = state.pool.snapshot();
    assert_eq!(pool_snap.total_sessions_created, 1);
    assert_eq!(pool_snap.mcp_continuations, 3);
}

#[tokio::test]
async fn mcp_tool_spill_below_10kb_cliff_zero_rtt_read_context_spill_and_no_false_compaction_retire()
 {
    let large_file_output = (1..=320)
        .map(|ln| format!("line_{ln:03}: pub fn exact_symbol_{ln:03}() -> usize {{ {ln} }}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        large_file_output.len() > 14_000,
        "output must exceed 9.6KB inline spill threshold"
    );
    let expected_spill_name = liftgate_types::session_key::format_content_addressed_spill_filename(
        "tool_result:exec_command",
        &large_file_output,
    );

    let backend = Arc::new(MockAcpBackend::new(vec![
        // Turn 1:
        // Step 1 (`usage_used = 300_000`): calls `exec_command` -> receives >14KB output spilled <9.6KB
        // Step 2 (0-RTT inside same prompt): calls `read_context_spill` on `expected_spill_name`
        // Step 3 (`usage_used = 0` on FinalReply, cumulative `final_usage.total_tokens = 1_850_000`
        //         across internal subcalls): must NOT poison `last_context_used`!
        MockScript::McpInteractive {
            steps: vec![
                McpMockStep::CallTool {
                    usage_used: 300_000,
                    thoughts: vec![],
                    pre_text_templates: vec![],
                    mcp_name: "exec_command".to_owned(),
                    arguments: json!({ "cmd": "cat src/planner.rs" }),
                    expect_reply_substrings: vec![
                        "mcp_lg_read_context_spill",
                        "tool-result-exec-command-",
                    ],
                },
                McpMockStep::CallTool {
                    usage_used: 302_000,
                    thoughts: vec![],
                    pre_text_templates: vec![],
                    mcp_name: "read_context_spill".to_owned(),
                    arguments: json!({
                        "path": expected_spill_name,
                        "start_line": 150,
                        "max_lines": 5
                    }),
                    expect_reply_substrings: vec![
                        "lines 150..=154 of 320",
                        "L150: line_150: pub fn exact_symbol_150()",
                        "L154: line_154: pub fn exact_symbol_154()",
                    ],
                },
                McpMockStep::FinalReply {
                    usage_used: 0,
                    thoughts: vec![],
                    chunks_template: vec!["Read middle lines 150..154 losslessly.".to_owned()],
                    final_usage: AcpUsage {
                        input_tokens: 48_000,
                        output_tokens: 2_000,
                        cached_read_tokens: 1_800_000,
                        cached_write_tokens: 0,
                        thought_tokens: 400,
                        total_tokens: 1_850_000,
                    },
                },
            ],
            cancelled_usage: AcpUsage::default(),
        },
        // Turn 2: Follow-up turn emits `UsageUpdate { used: 306_000 }` (`306k * 2 = 612k`).
        // Previously `last_context_used` was poisoned to `1_850_000`, causing `612k < 1_850_000`
        // to falsely retire `sess_1` as internally compacted! Now `sess_1` stays warm!
        MockScript::ImmediateWithUsage {
            usage_updates: vec![(306_000, 1_048_576)],
            chunks_template: vec!["Warm session preserved without false compaction!".to_owned()],
        },
    ]));

    let spill_dir =
        std::env::temp_dir().join(format!("liftgate-mcp-spill-e2e-{}", uuid::Uuid::new_v4()));
    let mut state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    state.config.spill_dir = Some(spill_dir.clone());
    let app = build_router(state.clone());

    let tools = json!([
        {
            "type": "function",
            "name": "exec_command",
            "description": "Execute command",
            "parameters": {
                "type": "object",
                "properties": { "cmd": { "type": "string" } },
                "required": ["cmd"]
            }
        }
    ]);

    // Step 1a: initial user request -> suspends on `exec_command`
    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "store": true,
                        "tools": tools,
                        "input": "Inspect planner.rs lines 150..154"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let b1: Value =
        serde_json::from_slice(&r1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let resp1_id = b1["id"].as_str().unwrap().to_owned();
    let call_id = b1["output"][0]["call_id"].as_str().unwrap().to_owned();

    // Step 1b: client returns >14KB tool output -> gateway spills <9.6KB, serves `read_context_spill`
    // in 0-RTT (never forwarded to client!), and finishes Turn 1 with bounded single-window usage!
    let r2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "store": true,
                        "previous_response_id": resp1_id,
                        "tools": tools,
                        "input": [{
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": large_file_output
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let b2: Value =
        serde_json::from_slice(&r2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let resp2_id = b2["id"].as_str().unwrap().to_owned();
    assert_eq!(
        b2["output"][0]["content"][0]["text"],
        "Read middle lines 150..154 losslessly."
    );
    let reported_total = b2["usage"]["total_tokens"].as_u64().unwrap();
    assert!(
        reported_total < 500_000,
        "wire total_tokens must reflect single-window occupancy instead of 1.85M multi-subcall sum, got {reported_total}"
    );

    // Turn 2: Follow-up request reuses warm `sess_1` (`total_sessions_created == 1`, 0 retirements!)
    let r3 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "store": true,
                        "previous_response_id": resp2_id,
                        "tools": tools,
                        "input": "Now summarize what you found."
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r3.status(), StatusCode::OK);
    let b3: Value =
        serde_json::from_slice(&r3.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        b3["output"][0]["content"][0]["text"],
        "Warm session preserved without false compaction!"
    );

    let pool_snap = state.pool.snapshot();
    assert_eq!(
        pool_snap.total_sessions_created, 1,
        "warm session must not be falsely retired as compacted after multi-subcall MCP turn"
    );
    assert_eq!(pool_snap.total_sessions_closed_or_evicted, 0);

    let _ = std::fs::remove_dir_all(&spill_dir);
}

#[tokio::test]
async fn elastic_io_telemetry_tracks_30_tool_codex_passthrough_compound_exec_multi_file_patch_and_zero_rtt_spill()
 {
    let large_exec_output = (1..=300)
        .map(|ln| format!("src/mod_{ln:03}.rs:{ln}: pub fn handler_{ln:03}() -> bool {{ true }}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(large_exec_output.len() > 14_000);
    let spill_filename = liftgate_types::session_key::format_content_addressed_spill_filename(
        "tool_result:exec",
        &large_exec_output,
    );
    let coalesced_patch = "*** Begin Patch\n\
*** Update File: src/mod_001.rs\n\
@@\n\
-pub fn handler_001() -> bool { true }\n\
+pub fn handler_001() -> bool { false }\n\
*** Update File: src/mod_150.rs\n\
@@\n\
-pub fn handler_150() -> bool { true }\n\
+pub fn handler_150() -> bool { false }\n\
*** End Patch";

    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::McpInteractive {
        steps: vec![
            McpMockStep::CallTool {
                usage_used: 42_000,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "exec".to_owned(),
                arguments: json!({
                    "code": "const [a, b] = await Promise.all([tools.exec_command({ cmd: 'rg handler src' }), tools.exec_command({ cmd: 'git status -s' })]); store('scan', a.output); text(a.output);"
                }),
                expect_reply_substrings: vec!["mcp_lg_read_context_spill", "tool-result-exec-"],
            },
            McpMockStep::CallTool {
                usage_used: 44_000,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "read_context_spill".to_owned(),
                arguments: json!({
                    "path": spill_filename,
                    "start_line": 149,
                    "max_lines": 4
                }),
                expect_reply_substrings: vec!["L150: src/mod_150.rs:150: pub fn handler_150()"],
            },
            McpMockStep::CallTool {
                usage_used: 45_500,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "apply_patch".to_owned(),
                arguments: json!({ "patch": coalesced_patch }),
                expect_reply_substrings: vec!["Success. Updated 2 files."],
            },
            McpMockStep::FinalReply {
                usage_used: 46_200,
                thoughts: vec![],
                chunks_template: vec!["Updated both handlers in one atomic patch.".to_owned()],
                final_usage: AcpUsage {
                    input_tokens: 12_000,
                    output_tokens: 1_200,
                    cached_read_tokens: 120_000,
                    cached_write_tokens: 0,
                    thought_tokens: 200,
                    total_tokens: 133_200,
                },
            },
        ],
        cancelled_usage: AcpUsage::default(),
    }]));

    let spill_dir =
        std::env::temp_dir().join(format!("liftgate-elastic-io-e2e-{}", uuid::Uuid::new_v4()));
    let mut state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    state.config.spill_dir = Some(spill_dir.clone());
    let app = build_router(state.clone());

    let mut tools_arr = vec![
        json!({
            "type": "custom",
            "name": "exec",
            "description": "Run JavaScript code to orchestrate tool calls.\nTool TypeScript declarations:\n```ts\ndeclare const tools: { exec_command(args: { cmd: string }): Promise<{ output: string }>; };\n```",
            "format": { "type": "grammar", "syntax": "lark", "definition": "start: /[\\s\\S]+/" }
        }),
        json!({
            "type": "function",
            "name": "wait",
            "description": "Wait on a running exec cell.",
            "parameters": {
                "type": "object",
                "properties": { "cell_id": { "type": "string" } },
                "required": ["cell_id"]
            }
        }),
        json!({
            "type": "custom",
            "name": "apply_patch",
            "description": "Patch files with V4A format."
        }),
    ];
    for idx in 0..27 {
        tools_arr.push(json!({
            "type": "function",
            "name": format!("mcp__linear__op_{idx:02}"),
            "description": format!("Linear connector operation {idx}"),
            "parameters": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }
        }));
    }
    assert_eq!(tools_arr.len(), 30);

    // Step 1: initial turn -> suspends on `exec`
    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "store": true,
                        "tools": tools_arr,
                        "input": "Update handlers 001 and 150."
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let b1: Value =
        serde_json::from_slice(&r1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let resp1_id = b1["id"].as_str().unwrap().to_owned();
    let exec_call_id = b1["output"][0]["call_id"].as_str().unwrap().to_owned();

    // Step 2: client returns >14KB `exec` output -> gateway spills, answers `read_context_spill`
    // in 0-RTT, and suspends on coalesced 2-file `apply_patch`
    let r2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "store": true,
                        "previous_response_id": resp1_id,
                        "tools": tools_arr,
                        "input": [{
                            "type": "custom_tool_call_output",
                            "call_id": exec_call_id,
                            "output": large_exec_output
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let b2: Value =
        serde_json::from_slice(&r2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let resp2_id = b2["id"].as_str().unwrap().to_owned();
    let patch_call_id = b2["output"][0]["call_id"].as_str().unwrap().to_owned();

    // Step 3: client returns `apply_patch` success -> final assistant completion
    let r3 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "stream": false,
                        "store": true,
                        "previous_response_id": resp2_id,
                        "tools": tools_arr,
                        "input": [{
                            "type": "custom_tool_call_output",
                            "call_id": patch_call_id,
                            "output": "Success. Updated 2 files."
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r3.status(), StatusCode::OK);

    // Inspect `/api/telemetry/snapshot` for Elastic I/O & per-session MCP summary
    let snap_resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/telemetry/snapshot")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(snap_resp.status(), StatusCode::OK);
    let snap: Value =
        serde_json::from_slice(&snap_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();

    let eio = &snap["elastic_io"];
    assert_eq!(eio["passthrough_sessions"], 1);
    assert_eq!(eio["virtualized_sessions"], 0);
    assert_eq!(eio["mcp_forwarded_tool_calls"], 2);
    assert_eq!(eio["mcp_orchestrator_calls"], 1);
    assert_eq!(eio["mcp_tool_using_turns"], 1);
    assert_eq!(eio["mcp_patch_calls"], 1);
    assert_eq!(eio["mcp_multi_file_patch_calls"], 1);
    assert_eq!(eio["mcp_patch_files_total"], 2);
    assert_eq!(eio["mcp_zero_rtt_spill_reads"], 1);
    assert_eq!(eio["mcp_tool_results_total"], 2);
    assert_eq!(eio["mcp_tool_results_spilled"], 1);
    assert!(eio["context_spills_created"].as_u64().unwrap_or(0) >= 1);
    assert!(eio["context_spills_bytes"].as_u64().unwrap_or(0) > 14_000);

    let sess_mcp = &snap["session_pool"]["sessions"][0]["mcp"];
    assert_eq!(sess_mcp["harness_profile"], "codex");
    assert_eq!(sess_mcp["orchestrator_mode"], "codex_exec");
    assert_eq!(sess_mcp["is_passthrough"], true);
    assert_eq!(sess_mcp["declared_tools"], 30);
    assert_eq!(sess_mcp["virtualized_hubs"], 0);

    let _ = std::fs::remove_dir_all(&spill_dir);
}

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post_json(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn zero_usage_mcp_suspension_still_samples_and_captures_context() {
    // No usage update arrives before the first client tool call suspends the cascade.
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::McpInteractive {
        steps: vec![
            McpMockStep::CallTool {
                usage_used: 0,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "Bash".to_owned(),
                arguments: json!({ "command": "ls" }),
                expect_reply_substrings: vec!["README.md"],
            },
            McpMockStep::FinalReply {
                usage_used: 4_000,
                thoughts: vec![],
                chunks_template: vec!["Listed.".to_owned()],
                final_usage: AcpUsage {
                    input_tokens: 1_000,
                    output_tokens: 10,
                    cached_read_tokens: 3_000,
                    cached_write_tokens: 0,
                    thought_tokens: 0,
                    total_tokens: 4_010,
                },
            },
        ],
        cancelled_usage: AcpUsage::default(),
    }]));
    let root = std::env::temp_dir().join(format!(
        "liftgate-zero-usage-capture-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let capture_dir = root.join("context-captures");
    std::fs::create_dir_all(&capture_dir).unwrap();
    let mut state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    state.telemetry = Arc::new(TelemetryHub::with_telemetry_config(
        liftgate_server::telemetry::TelemetryConfig {
            context_capture: liftgate_server::context_capture::ContextCaptureConfig {
                enabled: true,
                dir: Some(capture_dir.clone()),
                ..liftgate_server::context_capture::ContextCaptureConfig::default()
            },
            ..liftgate_server::telemetry::TelemetryConfig::default()
        },
    ));
    let app = build_router(state.clone());
    let tools_json = json!([{
        "type": "function",
        "name": "Bash",
        "description": "Execute shell command",
        "parameters": {
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        }
    }]);

    let (status, r1) = post_json(
        &app,
        "/v1/responses",
        json!({
            "model": "gpt-5.4",
            "stream": false,
            "store": true,
            "tools": tools_json,
            "input": "List the files."
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let call_id = r1["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .and_then(|fc| fc["call_id"].as_str())
        .expect("Bash function_call")
        .to_owned();

    // The suspended turn is captured although its usage is still zero.
    let mut captured = false;
    for _ in 0..250 {
        let (_, listed) = get_json(&app, "/api/telemetry/context-captures").await;
        if listed["records"]
            .as_array()
            .is_some_and(|r| r.iter().any(|c| c["task_id"] == r1["id"]))
        {
            captured = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(captured, "zero-usage suspension must still be captured");

    // ... and the session's context composition is sampled for the tool catalog.
    let (_, calls) = get_json(&app, "/api/telemetry/tool-calls").await;
    let session_id = calls["records"][0]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    let (status, catalog) = get_json(
        &app,
        &format!("/api/telemetry/sessions/{session_id}/tool-catalog"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        catalog["segments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["key"] == "user_text" && s["bytes"].as_u64().unwrap_or(0) > 0),
        "context sample recorded: {catalog}"
    );

    let (status, _) = post_json(
        &app,
        "/v1/responses",
        json!({
            "model": "gpt-5.4",
            "stream": false,
            "store": true,
            "previous_response_id": r1["id"],
            "tools": tools_json,
            "input": [{ "type": "function_call_output", "call_id": call_id, "output": "README.md" }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn tool_call_io_catalog_and_full_context_capture_follow_an_mcp_cascade() {
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::McpInteractive {
        steps: vec![
            McpMockStep::CallTool {
                usage_used: 6_000,
                thoughts: vec!["Look up the shell tool first.".to_owned()],
                pre_text_templates: vec![],
                mcp_name: "search_client_tools".to_owned(),
                arguments: json!({ "query": "Bash" }),
                expect_reply_substrings: vec!["Bash"],
            },
            McpMockStep::CallTool {
                usage_used: 6_400,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "Bash".to_owned(),
                arguments: json!({ "command": "git status --short" }),
                expect_reply_substrings: vec!["M src/lib.rs"],
            },
            McpMockStep::CallTool {
                usage_used: 7_000,
                thoughts: vec![],
                pre_text_templates: vec![],
                mcp_name: "Read".to_owned(),
                arguments: json!({}),
                expect_reply_substrings: vec!["file_path"],
            },
            McpMockStep::FinalReply {
                usage_used: 7_300,
                thoughts: vec!["One modified file.".to_owned()],
                chunks_template: vec!["Only src/lib.rs changed.".to_owned()],
                final_usage: AcpUsage {
                    input_tokens: 2_000,
                    output_tokens: 120,
                    cached_read_tokens: 5_000,
                    cached_write_tokens: 0,
                    thought_tokens: 40,
                    total_tokens: 7_120,
                },
            },
        ],
        cancelled_usage: AcpUsage::default(),
    }]));

    let root = std::env::temp_dir().join(format!(
        "liftgate-tool-telemetry-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let settings_path = root.join("settings.json");
    let capture_dir = root.join("context-captures");
    let mut state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    state.telemetry = Arc::new(TelemetryHub::with_telemetry_config(
        liftgate_server::telemetry::TelemetryConfig {
            cache: CacheTelemetryConfig {
                settings_path: Some(settings_path.clone()),
                ..CacheTelemetryConfig::default()
            },
            tool_calls: liftgate_server::tool_calls::ToolCallLimits::default(),
            context_capture: liftgate_server::context_capture::ContextCaptureConfig {
                enabled: false,
                dir: Some(capture_dir.clone()),
                ..liftgate_server::context_capture::ContextCaptureConfig::default()
            },
        },
    ));
    let app = build_router(state.clone());

    // Capture is opt-in: switch it on from the dashboard endpoint and persist it to settings.json.
    let (status, summary) = post_json(
        &app,
        "/api/telemetry/context-capture",
        json!({ "enabled": true, "max_records": 50, "max_total_mib": 64 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(summary["enabled"], true);
    assert_eq!(summary["max_total_bytes"], 64 * 1024 * 1024);
    let settings: Value = serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
    assert_eq!(settings["capture_full_context"], true);
    assert_eq!(settings["max_context_captures"], 50);
    let (status, _) = post_json(
        &app,
        "/api/telemetry/context-capture",
        json!({ "max_total_mib": 1 }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "quota below 16 MiB is rejected"
    );

    let tools_json = json!([
        {
            "type": "function",
            "name": "Read",
            "description": "Read file contents",
            "parameters": {
                "type": "object",
                "properties": { "file_path": { "type": "string" } },
                "required": ["file_path"]
            }
        },
        {
            "type": "function",
            "name": "Bash",
            "description": "Execute shell command",
            "parameters": {
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }
        }
    ]);

    let (status, r1) = post_json(
        &app,
        "/v1/responses",
        json!({
            "model": "gpt-5.4",
            "stream": false,
            "store": true,
            "instructions": "Check the workspace.",
            "tools": tools_json,
            "input": "What changed?"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let fc = r1["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("Bash function_call")
        .clone();
    let call_id = fc["call_id"].as_str().unwrap().to_owned();

    let (_, calls) = get_json(&app, "/api/telemetry/tool-calls").await;
    let records = calls["records"].as_array().unwrap();
    assert_eq!(records.len(), 2, "0-RTT search plus the pending Bash call");
    let search = records
        .iter()
        .find(|r| r["route"] == "zero_rtt_search")
        .expect("search record");
    assert_eq!(search["status"], "ok");
    assert_eq!(search["wire_tool_name"], "mcp_lg_search_client_tools");
    assert!(search["client_tool_name"].is_null());
    let bash = records
        .iter()
        .find(|r| r["call_id"] == call_id.as_str())
        .expect("bash record");
    assert_eq!(bash["status"], "pending");
    assert_eq!(bash["route"], "client_roundtrip");
    assert_eq!(bash["wire_tool_name"], "mcp_lg_Bash");
    assert_eq!(bash["client_tool_name"], "Bash");
    assert_eq!(bash["harness_profile"], "claude-code");
    assert!(bash["session_id"].is_string());
    let revision_after_r1 = calls["summary"]["revision"].as_u64().unwrap();

    let (status, r2) = post_json(
        &app,
        "/v1/responses",
        json!({
            "model": "gpt-5.4",
            "stream": false,
            "store": true,
            "previous_response_id": r1["id"],
            "tools": tools_json,
            "input": [{
                "type": "function_call_output",
                "call_id": call_id,
                "output": "\u{1b}[31m M src/lib.rs\u{1b}[0m\n"
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let final_text = r2["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "message")
        .unwrap()["content"][0]["text"]
        .clone();
    assert_eq!(final_text, "Only src/lib.rs changed.");

    let (_, delta) = get_json(
        &app,
        &format!("/api/telemetry/tool-calls?since={revision_after_r1}"),
    )
    .await;
    let changed = delta["records"].as_array().unwrap();
    assert!(changed.iter().any(|r| r["call_id"] == call_id.as_str()));
    assert!(changed.iter().all(|r| r["route"] != "zero_rtt_search"));
    let preflight = changed
        .iter()
        .find(|r| r["route"] == "zero_rtt_preflight")
        .expect("Read preflight rejection");
    assert_eq!(preflight["status"], "preflight_rejected");
    assert_eq!(preflight["error_kind"], "preflight_rejected");
    assert_eq!(delta["summary"]["stored_records"], 3);

    let (status, detail) = get_json(&app, &format!("/api/telemetry/tool-calls/{call_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["status"], "ok");
    assert_eq!(detail["output_sanitized"], true);
    assert!(detail["duration_ms"].is_u64());
    assert!(
        detail["wire_arguments"]
            .as_str()
            .unwrap()
            .contains("git status --short")
    );
    let out_text = detail["output_blocks"][0]["text"].as_str().unwrap();
    assert!(out_text.contains("M src/lib.rs") && !out_text.contains('\u{1b}'));
    let (status, _) = get_json(&app, "/api/telemetry/tool-calls/call_nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The session catalog explains how the declared tools were mounted for the model.
    let session_id = state.pool.snapshot().sessions[0].session_id.clone();
    let (status, catalog) = get_json(
        &app,
        &format!("/api/telemetry/sessions/{session_id}/tool-catalog"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(catalog["session_id"], session_id.as_str());
    let tools = catalog["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert!(tools.iter().all(|t| t["tier"] == "direct"));
    assert!(
        catalog["wire_tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["wire_name"] == "search_client_tools" && w["role"] == "gateway")
    );
    assert!(
        catalog["bridge_rules_prompt"]
            .as_str()
            .unwrap()
            .starts_with("<api_gateway_bridge_rules>")
    );
    assert!(catalog["captured_at_unix_ms"].as_u64().unwrap() > 0);
    let segments = catalog["segments"].as_array().unwrap();
    let wire_segment = segments
        .iter()
        .find(|s| s["key"] == "wire_tools")
        .expect("wire tools segment");
    assert!(wire_segment["bytes"].as_u64().unwrap() > 0);
    let (status, _) = get_json(&app, "/api/telemetry/sessions/nope/tool-catalog").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Full-context captures are written off the async workers; wait for both turns.
    let mut captures = Value::Null;
    for _ in 0..250 {
        let (_, listed) = get_json(&app, "/api/telemetry/context-captures").await;
        if listed["records"].as_array().is_some_and(|r| r.len() >= 2) {
            captures = listed;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let capture_records = captures["records"].as_array().expect("two captures");
    let newest = &capture_records[0];
    assert_eq!(newest["task_id"], r2["id"]);
    assert!(newest["payload_bytes"].as_u64().unwrap() > 0);
    let (status, detail) = get_json(
        &app,
        &format!(
            "/api/telemetry/context-captures/{}",
            newest["task_id"].as_str().unwrap()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        detail["payload"]["assistant_output"],
        "Only src/lib.rs changed."
    );
    assert!(
        detail["payload"]["assistant_reasoning"]
            .as_str()
            .is_some_and(|r| r.contains("One modified file."))
    );
    assert!(!detail["payload"]["messages"].as_array().unwrap().is_empty());
    let capture_files = std::fs::read_dir(&capture_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("ctx-"))
        .count();
    assert_eq!(capture_files, 2);

    let (_, trimmed) = post_json(
        &app,
        "/api/telemetry/context-capture",
        json!({ "max_records": 1 }),
    )
    .await;
    assert_eq!(trimmed["stored_records"], 1);
    let (_, cleared) = post_json(&app, "/api/telemetry/context-captures/clear", json!({})).await;
    assert_eq!(cleared["stored_records"], 0);
    let leftover = std::fs::read_dir(&capture_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("ctx-"))
        .count();
    assert_eq!(leftover, 0);

    let (_, snap) = get_json(&app, "/api/telemetry/snapshot").await;
    assert_eq!(snap["tool_call_telemetry"]["stored_records"], 3);
    assert_eq!(snap["context_capture"]["enabled"], true);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(start_paused = true)]
async fn xml_tool_calls_are_recorded_and_completed_by_the_next_request() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![
                "<agy_tool_call_{NONCE}>".to_owned(),
                "{\"name\":\"shell\",\"arguments\":{\"cmd\":\"cargo check\"}}</agy_tool_call_{NONCE}>".to_owned(),
            ],
        },
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Build succeeded!".to_owned()],
        },
    ]));
    let state = test_state(Arc::clone(&backend), 30, None);
    let app = build_router(state.clone());
    let tools = json!([{
        "type": "function",
        "name": "shell",
        "description": "Execute command",
        "parameters": { "type": "object", "properties": { "cmd": { "type": "string" } } }
    }]);

    let (status, r1) = post_json(
        &app,
        "/v1/responses",
        json!({
            "model": "gpt-5-codex",
            "stream": false,
            "instructions": "You are Codex.",
            "input": [{ "role": "user", "content": "Run cargo check" }],
            "tools": tools
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let call_id = r1["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .unwrap()["call_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let pending = state
        .telemetry
        .tool_call_detail(&call_id)
        .expect("recorded");
    assert_eq!(
        pending.record.status,
        liftgate_types::telemetry::ToolCallStatus::Pending
    );
    assert_eq!(pending.record.wire_tool_name, "shell");
    assert_eq!(
        pending.record.route,
        liftgate_types::telemetry::ToolCallRoute::ClientRoundtrip
    );

    let (status, _) = post_json(
        &app,
        "/v1/responses",
        json!({
            "model": "gpt-5-codex",
            "previous_response_id": r1["id"],
            "stream": false,
            "input": [{
                "type": "function_call_output",
                "call_id": call_id,
                "output": "Finished dev profile"
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let done = state
        .telemetry
        .tool_call_detail(&call_id)
        .expect("still recorded");
    assert_eq!(
        done.record.status,
        liftgate_types::telemetry::ToolCallStatus::Ok
    );
    assert_eq!(done.record.output_preview, "Finished dev profile");
    assert!(done.record.duration_ms.is_some());
}

#[tokio::test]
async fn distills_raw_harness_system_prompts_strictly_by_characteristic_tools_end_to_end() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Claude distilled ok.".to_owned()],
        },
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Claude Turn 2 warm ok.".to_owned()],
        },
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["OMP distilled ok.".to_owned()],
        },
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Generic SDK untouched.".to_owned()],
        },
    ]));

    let state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    let _mcp_keepalive = state.config.mcp_server.clone().expect("mcp server bound");
    let app = build_router(state.clone());

    // 1. Claude Code Turn 1 -> Turn 2 (with raw 30 KB system prompt + `<env>` + custom append)
    let cc_system = json!([
        { "type": "text", "text": "x-anthropic-billing-header: cc_version=2.50.0;" },
        { "type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude." },
        {
            "type": "text",
            "text": format!(
                "Raw Claude Code CLI manual v2.50.\n{}\n<env>\nWorking directory: /workspace/app\n</env>",
                "Verbose Bash/Edit/TodoWrite paragraph.\n".repeat(700)
            )
        },
        { "type": "text", "text": "Custom team rule: keep functions short." }
    ]);
    let cc_tools = json!([
        { "name": "Bash", "input_schema": { "type": "object", "properties": { "command": { "type": "string" } } } },
        { "name": "Read", "input_schema": { "type": "object", "properties": { "file_path": { "type": "string" } } } },
        { "name": "Edit", "input_schema": { "type": "object", "properties": { "file_path": { "type": "string" } } } }
    ]);
    let (st1, _) = post_json(
        &app,
        "/v1/messages",
        json!({
            "model": "gemini-4-carbon-high",
            "max_tokens": 1024,
            "system": cc_system,
            "tools": cc_tools,
            "messages": [{ "role": "user", "content": "hi" }]
        }),
    )
    .await;
    assert_eq!(st1, StatusCode::OK);

    let (st2, _) = post_json(
        &app,
        "/v1/messages",
        json!({
            "model": "gemini-4-carbon-high",
            "max_tokens": 1024,
            "system": cc_system,
            "tools": cc_tools,
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": "Claude distilled ok." },
                { "role": "user", "content": "check status" }
            ]
        }),
    )
    .await;
    assert_eq!(st2, StatusCode::OK);

    // 2. Oh My Pi (`omp`) on `/v1/responses` (28 KB raw instructions + internal eval prelude + `<project-context>`)
    let (st3, _) = post_json(
        &app,
        "/v1/responses",
        json!({
            "model": "gemini-4-carbon-high",
            "stream": false,
            "instructions": format!("You are omp's trusted coding assistant.\n{}", "OMP internal manual.\n".repeat(1200)),
            "tools": [
                { "type": "function", "name": "eval", "description": "Execute Python or JavaScript in persistent kernel with tool.<name>", "parameters": { "type": "object", "properties": { "code": { "type": "string" } } } },
                { "type": "function", "name": "edit", "parameters": { "type": "object", "properties": { "input": { "type": "string" } } } },
                { "type": "function", "name": "bash", "parameters": { "type": "object", "properties": { "command": { "type": "string" }, "pty": { "type": "boolean" }, "ready": { "type": "string" } } } }
            ],
            "input": [
                { "role": "developer", "content": "Binary analysis lives in eval's `dsm` namespace; read `help://eval/dsm` before use." },
                { "role": "developer", "content": "<project-context><repo-rules>Strict zero-copy Rust</repo-rules></project-context>" },
                { "role": "user", "content": "hi" }
            ]
        }),
    )
    .await;
    assert_eq!(st3, StatusCode::OK);

    // 3. Generic caller WITHOUT harness tools retains full custom prompt even if wording mentions Codex/Claude
    let (st4, _) = post_json(
        &app,
        "/v1/messages",
        json!({
            "model": "gemini-4-carbon-high",
            "max_tokens": 512,
            "system": format!("You are Claude Code lookalike.\n{}", "PRESERVE_GENERIC_SDK_RULE\n".repeat(120)),
            "tools": [{ "name": "fetch_invoice", "input_schema": { "type": "object" } }],
            "messages": [{ "role": "user", "content": "invoice #7" }]
        }),
    )
    .await;
    assert_eq!(st4, StatusCode::OK);

    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 4);

    // Claude Code Turn 1: raw 28 KB manual stripped, `<env>` and custom append preserved, Turn 2 reuses warm session!
    assert!(
        !prompts[0]
            .text_joined
            .contains("Verbose Bash/Edit/TodoWrite paragraph")
    );
    assert!(
        prompts[0]
            .text_joined
            .contains("Working directory: /workspace/app")
    );
    assert!(
        prompts[0]
            .text_joined
            .contains("Custom team rule: keep functions short.")
    );
    assert!(prompts[0].text_joined.len() < 2_500);
    assert_eq!(prompts[1].session_id, prompts[0].session_id);
    assert!(
        !prompts[1]
            .text_joined
            .contains("Working directory: /workspace/app")
    );

    // OMP: 28 KB manual and `help://eval/dsm` stripped, `<project-context>` preserved!
    assert!(!prompts[2].text_joined.contains("OMP internal manual."));
    assert!(!prompts[2].text_joined.contains("help://eval/dsm"));
    assert!(prompts[2].text_joined.contains("Strict zero-copy Rust"));
    assert!(prompts[2].text_joined.len() < 2_500);

    // Generic caller: custom prompt preserved intact!
    assert!(prompts[3].text_joined.contains("PRESERVE_GENERIC_SDK_RULE"));
}

#[tokio::test]
async fn warm_session_survives_tool_threshold_mutations_and_literal_assistant_developer_blocks() {
    let literal_assistant = "<developer_instructions><apps_instructions>literal assistant example</apps_instructions></developer_instructions>";
    let backend = Arc::new(MockAcpBackend::new(vec![
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec![literal_assistant.to_owned()],
        },
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Turn 2 warm ok.".to_owned()],
        },
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Turn 3 warm ok.".to_owned()],
        },
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Cold replay preserves assistant literal.".to_owned()],
        },
    ]));

    let state = test_state_with_mcp(Arc::clone(&backend), 30, None).await;
    let _mcp_keepalive = state.config.mcp_server.clone().expect("mcp server bound");
    let app = build_router(state.clone());

    let raw_sys = format!(
        "You are Claude Code, Anthropic's official CLI for Claude.\n{}",
        "Verbose CLI manual line.\n".repeat(400)
    );
    let read_only_tools = json!([
        { "name": "Read", "input_schema": { "type": "object", "properties": { "file_path": { "type": "string" } } } }
    ]);
    let read_and_glob_tools = json!([
        { "name": "Read", "input_schema": { "type": "object", "properties": { "file_path": { "type": "string" } } } },
        { "name": "Glob", "input_schema": { "type": "object", "properties": { "pattern": { "type": "string" } } } }
    ]);

    // Turn 1: Only `[Read]` declared (`detect_harness_by_tools == None`); assistant emits literal `<developer_instructions>`.
    let (st1, _) = post_json(
        &app,
        "/v1/messages",
        json!({
            "model": "gemini-4-carbon-high",
            "max_tokens": 1024,
            "system": raw_sys,
            "tools": read_only_tools,
            "messages": [{ "role": "user", "content": "Show literal wrapper example" }]
        }),
    )
    .await;
    assert_eq!(st1, StatusCode::OK);

    // Turn 2: Enables `[Read, Glob]` (`Some(ClaudeCode)`), replays literal assistant block, and adds a protocol developer item.
    let (st2, _) = post_json(
        &app,
        "/v1/messages",
        json!({
            "model": "gemini-4-carbon-high",
            "max_tokens": 1024,
            "system": raw_sys,
            "tools": read_and_glob_tools,
            "messages": [
                { "role": "user", "content": "Show literal wrapper example" },
                { "role": "assistant", "content": literal_assistant },
                { "role": "developer", "content": "<apps_instructions>drop protocol apps</apps_instructions>\n<permissions instructions>sandbox_mode is read-only</permissions instructions>" },
                { "role": "user", "content": "Continue with Glob enabled" }
            ]
        }),
    )
    .await;
    assert_eq!(st2, StatusCode::OK);

    // Turn 3: Drops `Glob` back to `[Read]` (`None`); warm session MUST still match prefix hash!
    let (st3, _) = post_json(
        &app,
        "/v1/messages",
        json!({
            "model": "gemini-4-carbon-high",
            "max_tokens": 1024,
            "system": raw_sys,
            "tools": read_only_tools,
            "messages": [
                { "role": "user", "content": "Show literal wrapper example" },
                { "role": "assistant", "content": literal_assistant },
                { "role": "developer", "content": "<apps_instructions>drop protocol apps</apps_instructions>\n<permissions instructions>sandbox_mode is read-only</permissions instructions>" },
                { "role": "user", "content": "Continue with Glob enabled" },
                { "role": "assistant", "content": "Turn 2 warm ok." },
                { "role": "user", "content": "Continue with Read only" }
            ]
        }),
    )
    .await;
    assert_eq!(st3, StatusCode::OK);

    // Turn 4: Cold session with `[Read, Glob]` and pre-existing literal assistant `<developer_instructions>` in history.
    let (st4, _) = post_json(
        &app,
        "/v1/messages",
        json!({
            "model": "gemini-4-carbon-high",
            "max_tokens": 1024,
            "system": raw_sys,
            "tools": read_and_glob_tools,
            "messages": [
                { "role": "user", "content": "Cold history question" },
                { "role": "assistant", "content": literal_assistant },
                { "role": "user", "content": "Follow up" }
            ]
        }),
    )
    .await;
    assert_eq!(st4, StatusCode::OK);

    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 4);

    // Turns 1 -> 2 -> 3 all reused the exact same warm ACP session across `[Read] -> [Read, Glob] -> [Read]`!
    assert_eq!(prompts[1].session_id, prompts[0].session_id);
    assert_eq!(prompts[2].session_id, prompts[0].session_id);
    assert!(!prompts[1].text_joined.contains("drop protocol apps"));
    assert!(prompts[1].text_joined.contains("sandbox_mode is read-only"));
    assert!(prompts[1].text_joined.contains("<client_tools_update>"));

    // Cold Turn 4 distilled the built-in system prompt while keeping the assistant's literal `<developer_instructions>` intact!
    assert!(!prompts[3].text_joined.contains("Verbose CLI manual line."));
    assert!(prompts[3].text_joined.contains(literal_assistant));
}
