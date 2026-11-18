use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
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
    telemetry::TelemetryHub,
    turn_runner::GatewayRuntimeConfig,
};
use liftgate_types::{
    acp::{
        AcpChunkContent, AcpPromptBlock, AvailableModel, ModelsCatalog, ModesCatalog,
        PromptTurnResult, SessionNewResult, SessionUpdatePayload,
    },
    config::{AcpModeId, GatewayLimits, ModelId, SessionId},
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
}

struct RecordedPrompt {
    session_id: String,
    text_joined: String,
    image_blocks: Vec<(String, usize)>,
}

struct MockAcpBackend {
    next_session_num: AtomicUsize,
    aborted_count: AtomicUsize,
    session_nonces: Mutex<Vec<(String, String)>>,
    recorded_system_appends: Mutex<Vec<String>>,
    recorded_models: Mutex<Vec<String>>,
    recorded_prompts: Mutex<Vec<RecordedPrompt>>,
    scripts: Mutex<VecDeque<MockScript>>,
    hydration_updates: Mutex<Vec<(u64, u64)>>,
}

impl MockAcpBackend {
    fn new(scripts: Vec<MockScript>) -> Self {
        Self {
            next_session_num: AtomicUsize::new(1),
            aborted_count: AtomicUsize::new(0),
            session_nonces: Mutex::new(Vec::new()),
            recorded_system_appends: Mutex::new(Vec::new()),
            recorded_models: Mutex::new(Vec::new()),
            recorded_prompts: Mutex::new(Vec::new()),
            scripts: Mutex::new(VecDeque::from(scripts)),
            hydration_updates: Mutex::new(Vec::new()),
        }
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
        let is_hydration_seed = joined.contains("[Context Hydration Segment ");

        self.recorded_prompts.lock().unwrap().push(RecordedPrompt {
            session_id: session_id.as_str().to_owned(),
            text_joined: joined,
            image_blocks,
        });

        if is_hydration_seed {
            let updates = self.hydration_updates.lock().unwrap().clone();
            for (used, size) in updates {
                let _ = updates_tx
                    .send(SessionUpdatePayload::UsageUpdate { used, size })
                    .await;
            }
            let _ = updates_tx
                .send(SessionUpdatePayload::AgentMessageChunk {
                    content: AcpChunkContent {
                        content_type: "text".to_owned(),
                        text: "OK".to_owned(),
                    },
                })
                .await;
            return Ok(PromptTurnResult {
                stop_reason: "end_turn".to_owned(),
                usage: mock_acp_usage(12_000, 1, 0),
                error: None,
            });
        }

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
        }
    }

    fn abort_session_nonblocking(&self, _session_id: &SessionId) {
        self.aborted_count.fetch_add(1, Ordering::SeqCst);
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
    test_app_with_context(backend, timeout_secs, api_key, false)
}

fn test_app_with_context(
    backend: Arc<MockAcpBackend>,
    timeout_secs: u64,
    api_key: Option<&str>,
    long_context: bool,
) -> axum::Router {
    build_router(test_state(backend, timeout_secs, api_key, long_context))
}

fn test_state(
    backend: Arc<MockAcpBackend>,
    timeout_secs: u64,
    api_key: Option<&str>,
    long_context: bool,
) -> AppState<MockAcpBackend> {
    let limits =
        GatewayLimits::try_new(16 * 1024 * 1024, 12 * 1024 * 1024, 64, 16, 64, timeout_secs)
            .unwrap();

    AppState {
        backend,
        pool: Arc::new(SessionPool::new(limits)),
        telemetry: Arc::new(TelemetryHub::new()),
        config: GatewayRuntimeConfig {
            title_model_file: None,
            long_context,
            default_model: ModelId::try_new("gemini-4-carbon-high").unwrap(),
            acp_mode: AcpModeId::try_new("auto").unwrap(),
            api_key: api_key.map(Arc::<str>::from),
            limits,
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
    let mut state = test_state(Arc::clone(&backend), 30, None, false);
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
async fn context_policy_bounds_inputs_and_stops_compacted_hydration_without_retry() {
    let multiline_200k: String = (1..=4_000)
        .map(|i| format!("LINE {i:04}: {}\n", "x".repeat(40)))
        .collect();
    assert!(multiline_200k.len() > 200_000);

    // 1. Default mode (`long_context == false`): automatically middle-truncates oversized single-turn
    //    inputs (preserving head + tail + structured marker) instead of failing with HTTP 400.
    {
        let backend = Arc::new(MockAcpBackend::new(vec![]));
        let app = test_app_with_context(Arc::clone(&backend), 30, None, false);
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
        }
    }

    // 2. Experimental `long_context == true`: stops immediately after the first seed chunk if
    //    upstream compacts during hydration, returning standard auto-compactable wire errors for
    //    both OpenAI Codex (`context_length_exceeded`) and Claude Code (`prompt is too long: ...`).
    {
        let backend = Arc::new(MockAcpBackend::new(vec![]));
        *backend.hydration_updates.lock().unwrap() =
            vec![(320_000, 1_048_576), (20_000, 1_048_576)];
        let app = test_app_with_context(Arc::clone(&backend), 30, None, true);

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
            long_context: false,
            default_model: ModelId::try_new("gemini-4-carbon-high").unwrap(),
            acp_mode: AcpModeId::try_new("auto").unwrap(),
            api_key: None,
            limits,
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
async fn multi_chunk_flash_hydration_streams_380kb_transcript_losslessly_and_restores_target_model()
{
    let backend = Arc::new(MockAcpBackend::new(vec![MockScript::Immediate {
        thoughts: vec!["Synthesizing across full 380KB hydrated history".to_owned()],
        chunks_template: vec![
            "All needles verified: EARLY_NEEDLE_01, MID_NEEDLE_02, LATE_NEEDLE_03.".to_owned(),
        ],
    }]));

    let app = test_app_with_context(Arc::clone(&backend), 120, None, true);
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
        "All needles verified: EARLY_NEEDLE_01, MID_NEEDLE_02, LATE_NEEDLE_03."
    );
    // OpenAI input includes uncached (100) and cached (128) tokens from the final turn,
    // NOT the sum of intermediate Flash hydration turns (which would trigger premature /compact)!
    assert_eq!(body["usage"]["input_tokens"], 228);

    // Session started on `gemini-4-hydrogen-low` for seed turns, then switched to `gemini-4-carbon-high`!
    let models = backend.recorded_models.lock().unwrap().clone();
    assert_eq!(
        models,
        vec![
            "gemini-4-hydrogen-low".to_owned(),
            "gemini-4-carbon-high".to_owned(),
        ]
    );

    let prompts = backend.recorded_prompts.lock().unwrap();
    assert!(
        prompts.len() >= 3,
        "expected >=2 Flash seed chunks + 1 final turn, got {}",
        prompts.len()
    );
    let mut combined = String::new();
    for p in prompts.iter() {
        assert_eq!(p.session_id, "sess_1");
        assert!(
            p.text_joined.len() <= liftgate_types::session_key::MAX_ACP_PROMPT_TEXT_BYTES,
            "chunk len {} exceeded MAX_ACP_PROMPT_TEXT_BYTES",
            p.text_joined.len()
        );
        combined.push_str(&p.text_joined);
    }
    assert!(!combined.contains("...[omitted"));
    assert!(combined.contains("EARLY_NEEDLE_01"));
    assert!(combined.contains("MID_NEEDLE_02"));
    assert!(combined.contains("LATE_NEEDLE_03"));
    assert!(combined.contains("Verify every needle from the conversation."));
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

    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 3);
    assert_eq!(prompts[0].session_id, "sess_1");
    assert!(
        prompts[0]
            .text_joined
            .contains("mcp__workspace__connector_219"),
        "full 18 KB+ `exec` TypeScript declarations must spill losslessly into Turn 1"
    );
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
    let app = test_app_with_context(Arc::clone(&backend), 10, Some("desktop-key"), true);

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
    use std::sync::atomic::AtomicBool;
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
            long_context: false,
            default_model: ModelId::try_new("gemini-4-carbon-high").unwrap(),
            acp_mode: AcpModeId::try_new("auto").unwrap(),
            api_key: Some(Arc::from("secret-gate-key")),
            limits,
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
