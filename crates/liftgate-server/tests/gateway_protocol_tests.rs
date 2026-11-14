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
    routes::{AppState, build_router},
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
    ChunksThenHang {
        chunks_template: Vec<String>,
    },
    WebSearchSplitDelta {
        query: String,
        answer: String,
    },
    HangForever,
}

struct RecordedPrompt {
    session_id: String,
    text_joined: String,
}

struct MockAcpBackend {
    next_session_num: AtomicUsize,
    aborted_count: AtomicUsize,
    session_nonces: Mutex<Vec<(String, String)>>,
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
            session_nonces: Mutex::new(Vec::new()),
            recorded_system_appends: Mutex::new(Vec::new()),
            recorded_models: Mutex::new(Vec::new()),
            recorded_prompts: Mutex::new(Vec::new()),
            scripts: Mutex::new(VecDeque::from(scripts)),
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

        self.recorded_prompts.lock().unwrap().push(RecordedPrompt {
            session_id: session_id.as_str().to_owned(),
            text_joined: joined,
        });

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
                })
            }
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
    let limits =
        GatewayLimits::try_new(16 * 1024 * 1024, 12 * 1024 * 1024, 64, 16, 64, timeout_secs)
            .unwrap();

    build_router(AppState {
        backend,
        pool: Arc::new(SessionPool::new(limits)),
        telemetry: Arc::new(TelemetryHub::new()),
        config: GatewayRuntimeConfig {
            default_model: ModelId::try_new("gemini-4-carbon-high").unwrap(),
            acp_mode: AcpModeId::try_new("auto").unwrap(),
            api_key: api_key.map(Arc::<str>::from),
            limits,
        },
        ui_dist_dir: None,
    })
}

fn test_app(backend: Arc<MockAcpBackend>, timeout_secs: u64) -> axum::Router {
    test_app_with_key(backend, timeout_secs, None)
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

    // `x-api-key` -> 200 `/v1/messages/count_tokens`
    let count_req = json!({
        "model": "claude-opus-4-6",
        "messages": [{ "role": "user", "content": "Hello Liftgate!" }]
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

    // 2. Auto Mode Stage 1 classifier requesting heavy `gemini-4-carbon-high` -> must override to `gemini-3.8-flash-low`!
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
            "gemini-3.8-flash-low".to_owned(),
            "gemini-3.8-flash-low".to_owned(),
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
async fn codex_0_166_massive_developer_prompt_namespace_tools_and_429_failover() {
    let backend = Arc::new(MockAcpBackend::new(vec![
        // 1. Primary `gemini-4-carbon-high` (`sess_1`) emits 429 banner WITHOUT trailing newline
        //    and attempts to hang 3600s -> gateway must abort in 0ms and fail over immediately!
        MockScript::ChunksThenHang {
            chunks_template: vec![
                "⚠ API error (attempt 1/8): RESOURCE_EXHAUSTED (code 429): No capacity available"
                    .to_owned(),
            ],
        },
        // 2. Backup model (`claude-sonnet-5-5-high`, `sess_2`) succeeds immediately and calls
        //    a namespaced Codex tool (`multi_agent_v1.spawn_agent`)!
        MockScript::Immediate {
            thoughts: vec!["Delegating sub-task".to_owned()],
            chunks_template: vec![
                "<agy_tool_call_{NONCE}>{\"name\":\"multi_agent_v1.spawn_agent\",\"arguments\":{\"task\":\"audit workspace\"}}</agy_tool_call_{NONCE}>".to_owned(),
            ],
        },
        // 3. Concurrent Codex TUI task-title generator -> routed to `gemini-3.8-flash-low` (`sess_3`)
        MockScript::Immediate {
            thoughts: vec![],
            chunks_template: vec!["Audit workspace codebase".to_owned()],
        },
        // 4. Turn 2 continuation with echoed `namespace: "multi_agent_v1"` -> reuses `sess_2`
        //    pinned to healthy failover model without hitting congested `gemini-4-carbon-high`!
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

    // Turn 1: Codex 0.166.0 sends `reasoning.effort = "xhigh"` default + 20KB developer prompt
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
    assert_eq!(appends.len(), 3);
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
        !appends[2].contains("\n<client_tools>\n") && !appends[2].contains("exec_command"),
        "auxiliary Codex title-generator call must strip client tool schemas"
    );

    // 2) Exact `-high` model was NOT mutated to `-xhigh`, failed over immediately on 429 to
    //    `claude-sonnet-5-5-high`, title call routed to `gemini-3.8-flash-low`, and Turn 2
    //    stayed pinned on `sess_2` without calling `set_session_model` back to `gemini-4-carbon-high`!
    let models = backend.recorded_models.lock().unwrap().clone();
    assert_eq!(
        models,
        vec![
            "gemini-4-carbon-high".to_owned(),
            "claude-sonnet-5-5-high".to_owned(),
            "gemini-3.8-flash-low".to_owned(),
        ]
    );

    let prompts = backend.recorded_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 4);
    assert_eq!(prompts[0].session_id, "sess_1");
    assert!(prompts[0].text_joined.contains("<developer_instructions>"));
    assert_eq!(prompts[1].session_id, "sess_2");
    assert_eq!(prompts[2].session_id, "sess_3");
    assert_eq!(
        prompts[3].session_id, "sess_2",
        "Turn 2 must reuse failover-pinned sess_2 with only the tool output delta"
    );
    assert!(prompts[3].text_joined.contains("worker spawned ok"));
    assert!(
        !prompts[3]
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
