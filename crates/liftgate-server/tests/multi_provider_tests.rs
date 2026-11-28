//! End-to-end integration tests for multi-provider routing, native 0-RTT tool virtualization,
//! cross-protocol translation, Passthrough observe-only mode, and per-provider usage & 3-stage telemetry.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use liftgate_server::{
    acp_client::AcpBackend,
    context_capture::ContextCaptureConfig,
    error::AcpError,
    provider_runtime::ProviderRuntimeRegistry,
    routes::{AppState, build_router},
    session_pool::SessionPool,
    telemetry::{CacheTelemetryConfig, TelemetryConfig, TelemetryHub},
    tool_calls::ToolCallLimits,
    turn_runner::GatewayRuntimeConfig,
};
use liftgate_types::{
    acp::{
        AcpPromptBlock, AvailableModel, ModelsCatalog, ModesCatalog, PromptTurnResult,
        SessionNewResult, SessionUpdatePayload,
    },
    config::{AcpModeId, GatewayLimits, ModelId, SessionId},
    provider::{
        ApiKeySource, ProviderDriverKind, ProviderId, ProviderProfileConfig, ProviderRegistryConfig,
    },
    telemetry::AcpProcessTelemetry,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower::ServiceExt;

struct DummyAcpBackend;

impl AcpBackend for DummyAcpBackend {
    fn available_models(&self) -> Vec<AvailableModel> {
        vec![AvailableModel {
            model_id: "gemini-4-carbon-high".to_owned(),
            name: "Gemini 4 Carbon (High)".to_owned(),
        }]
    }

    async fn create_session(&self, _system_append: &str) -> Result<SessionNewResult, AcpError> {
        Ok(SessionNewResult {
            session_id: SessionId::try_new("dummy_sess").unwrap(),
            modes: ModesCatalog::default(),
            models: ModelsCatalog {
                available_models: Vec::new(),
                current_model_id: None,
            },
        })
    }

    async fn set_session_mode(
        &self,
        _session_id: &SessionId,
        _mode_id: &AcpModeId,
    ) -> Result<(), AcpError> {
        Ok(())
    }

    async fn set_session_model(
        &self,
        _session_id: &SessionId,
        _model_id: &ModelId,
    ) -> Result<(), AcpError> {
        Ok(())
    }

    async fn start_prompt(
        &self,
        _session_id: &SessionId,
        _prompt: &[AcpPromptBlock],
        _updates_tx: mpsc::Sender<SessionUpdatePayload>,
    ) -> Result<PromptTurnResult, AcpError> {
        Err(AcpError::Rpc {
            method: "dummy",
            code: -1,
            message: "dummy ACP backend should not be called on native HTTP provider routes"
                .to_owned(),
        })
    }

    fn abort_session_nonblocking(&self, _session_id: &SessionId) {}

    async fn close_session(&self, _session_id: &SessionId) -> Result<(), AcpError> {
        Ok(())
    }

    fn process_telemetry(&self, default_model: &str, acp_mode: &str) -> AcpProcessTelemetry {
        AcpProcessTelemetry {
            connected: true,
            pid: None,
            binary_path: "dummy".to_owned(),
            session_cwd: ".".to_owned(),
            os: "test".into(),
            arch: "test".into(),
            supervision_strategy: "test".into(),
            protocol_version: 1,
            agent_name: None,
            agent_version: None,
            started_at_unix_ms: 1,
            uptime_secs: 1,
            default_model: default_model.to_owned(),
            acp_mode: acp_mode.to_owned(),
            rpc_requests_sent: 0,
            rpc_responses_received: 0,
            rpc_notifications_received: 0,
            rpc_pending_in_flight: 0,
            active_prompt_streams: 0,
            recent_stderr: Vec::new(),
            agy_proxy: None,
            auto_update_enabled: false,
            latest_known_version: None,
            last_update_check_unix_ms: None,
            last_auto_update_unix_ms: None,
            last_auto_updated_from: None,
            auto_update_note: None,
        }
    }
}

async fn read_raw_http_body(stream: &mut tokio::net::TcpStream) -> Value {
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 4096];
    let mut expected_total: Option<usize> = None;
    loop {
        let n = stream.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if expected_total.is_none()
            && let Some(hdr_end) = buf.windows(4).position(|w| w == b"\r\n\r\n")
        {
            let body_start = hdr_end + 4;
            let hdr = String::from_utf8_lossy(&buf[..hdr_end]);
            let cl = hdr
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    if k.trim().eq_ignore_ascii_case("content-length") {
                        v.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            expected_total = Some(body_start + cl);
        }
        if let Some(tot) = expected_total
            && buf.len() >= tot
        {
            let hdr_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            return serde_json::from_slice(&buf[hdr_end + 4..tot]).unwrap_or_else(|_| json!({}));
        }
    }
    json!({})
}

fn build_test_router(
    registry: ProviderRuntimeRegistry,
    capture_dir: Option<std::path::PathBuf>,
) -> (axum::Router, Arc<TelemetryHub>) {
    let mut ctx_cfg = ContextCaptureConfig::default();
    if let Some(dir) = capture_dir {
        ctx_cfg.enabled = true;
        ctx_cfg.dir = Some(dir);
    }
    let telemetry = Arc::new(TelemetryHub::with_telemetry_config(TelemetryConfig {
        cache: CacheTelemetryConfig::default(),
        tool_calls: ToolCallLimits::default(),
        context_capture: ctx_cfg,
        pricing_path: None,
        usage_path: None,
    }));
    telemetry.set_provider_registry(Arc::new(registry));
    let limits = GatewayLimits::default();
    let pool = Arc::new(SessionPool::new(limits));
    let state = AppState {
        backend: Arc::new(DummyAcpBackend),
        pool,
        telemetry: Arc::clone(&telemetry),
        config: GatewayRuntimeConfig {
            title_model_file: None,
            spill_dir: None,
            settings_path: None,
            default_model: ModelId::fallback_primary(),
            acp_mode: AcpModeId::try_new("agent").unwrap(),
            api_key: None,
            limits,
            vision_offload: Arc::new(AtomicBool::new(false)),
            mcp_tool_bridge: Arc::new(AtomicBool::new(true)),
            mcp_server: None,
        },
        ui_dist_dir: None,
        embedded_ui: &[],
        admin_shutdown: None,
    };
    (build_router(state), telemetry)
}

#[tokio::test]
async fn native_openai_provider_runs_0rtt_search_continuation_and_unwraps_namespace_hub() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let call_num = Arc::new(AtomicUsize::new(0));
    let call_num_srv = Arc::clone(&call_num);

    tokio::spawn(async move {
        // Round 1: Model sees virtualized tools (no `mcp_lg_` prefix!) and calls `search_client_tools`.
        let (mut s1, _) = listener.accept().await.unwrap();
        let req1 = read_raw_http_body(&mut s1).await;
        call_num_srv.fetch_add(1, Ordering::SeqCst);
        let tool_names: Vec<String> = req1["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_owned))
            .collect();
        assert!(tool_names.iter().any(|n| n == "search_client_tools"));
        assert!(tool_names.iter().any(|n| n == "ns__slack"));
        assert!(!tool_names.iter().any(|n| n.starts_with("mcp_lg_")));

        let sse1 = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Searching Slack tools. \"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c_search\",\"name\":\"search_client_tools\",\"arguments\":\"{\\\"namespace\\\":\\\"slack\\\",\\\"query\\\":\\\"action_7\\\"}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":500,\"output_tokens\":20,\"input_tokens_details\":{\"cached_tokens\":400}}}}\n\n",
            "data: [DONE]\n\n"
        );
        s1.write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{sse1}").as_bytes(),
        )
        .await
        .unwrap();
        drop(s1);

        // Round 2 (automatic 0-RTT continuation on the same downstream HTTP request!):
        // Model inspects search result and invokes `ns__slack` -> gateway unwraps to `mcp__slack__action_7`!
        let (mut s2, _) = listener.accept().await.unwrap();
        let req2 = read_raw_http_body(&mut s2).await;
        call_num_srv.fetch_add(1, Ordering::SeqCst);
        let serialized_in = req2["input"].to_string();
        assert!(serialized_in.contains("mcp__slack__action_7"));
        assert!(!serialized_in.contains("mcp_lg_"));

        let sse2 = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c_hub\",\"name\":\"ns__slack\",\"arguments\":\"{\\\"action\\\":\\\"action_7\\\",\\\"args\\\":{\\\"channel\\\":\\\"#eng\\\"}}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":600,\"output_tokens\":25,\"input_tokens_details\":{\"cached_tokens\":500}}}}\n\n",
            "data: [DONE]\n\n"
        );
        s2.write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{sse2}").as_bytes(),
        )
        .await
        .unwrap();
    });

    let mut reg_cfg = ProviderRegistryConfig::default();
    let mut oai =
        ProviderProfileConfig::builtin(ProviderId::openai(), ProviderDriverKind::OpenAiResponses);
    oai.endpoint = Some(format!("http://{addr}/v1"));
    oai.auth = ApiKeySource::None;
    reg_cfg.upsert(oai);

    let (router, telemetry) =
        build_test_router(ProviderRuntimeRegistry::in_memory(reg_cfg, None), None);

    // Attach 80 tools so native 3-tier virtualization activates automatically!
    let mut tools = vec![json!({
        "type": "function",
        "name": "exec_command",
        "description": "Run shell",
        "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]}
    })];
    for i in 0..80 {
        tools.push(json!({
            "type": "function",
            "name": format!("mcp__slack__action_{i}"),
            "description": "Post message to Slack channel",
            "parameters": {"type": "object", "properties": {"channel": {"type": "string"}}, "required": ["channel"]}
        }));
    }

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/openai/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "input": "Notify #eng on Slack",
                        "tools": tools,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(
        call_num.load(Ordering::SeqCst),
        2,
        "must execute 2 upstream rounds inside 1 client HTTP request"
    );
    let outputs = body["output"].as_array().expect("output array");
    let fn_call = outputs
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("unwrapped client function_call");
    assert_eq!(fn_call["name"], "mcp__slack__action_7");
    assert_eq!(fn_call["arguments"], "{\"channel\":\"#eng\"}");

    // Verify per-provider usage attribution!
    let usage_rep = telemetry.usage_report(None);
    let oai_usage = usage_rep
        .providers
        .iter()
        .find(|p| p.provider_id == "openai")
        .expect("openai usage recorded");
    assert_eq!(oai_usage.calls, 1);
    assert_eq!(oai_usage.tokens.input, 200);
    assert_eq!(oai_usage.tokens.cached_input, 900);
}

#[tokio::test]
async fn passthrough_mode_preserves_raw_harness_prompt_and_records_three_stage_wire_summary() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let observed_instructions: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_srv = Arc::clone(&observed_instructions);

    tokio::spawn(async move {
        for _ in 0..2 {
            let (mut s, _) = listener.accept().await.unwrap();
            let req = read_raw_http_body(&mut s).await;
            let instr = req["instructions"].as_str().unwrap_or("").to_owned();
            observed_srv.lock().unwrap().push(instr);
            let sse = concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ack\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":200,\"output_tokens\":5}}}\n\n",
                "data: [DONE]\n\n"
            );
            s.write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{sse}")
                    .as_bytes(),
            )
            .await
            .unwrap();
        }
    });

    let tmp_dir = std::env::temp_dir().join(format!("lg-mp-cap-{}", uuid::Uuid::new_v4()));
    let mut reg_cfg = ProviderRegistryConfig::default();
    let mut oai =
        ProviderProfileConfig::builtin(ProviderId::openai(), ProviderDriverKind::OpenAiResponses);
    oai.endpoint = Some(format!("http://{addr}/v1"));
    oai.auth = ApiKeySource::None;
    reg_cfg.upsert(oai);

    let (router, telemetry) = build_test_router(
        ProviderRuntimeRegistry::in_memory(reg_cfg, None),
        Some(tmp_dir.clone()),
    );

    let raw_codex_builtin = format!(
        "You are Codex, based on GPT-5.\n{}",
        "Built-in verbose Codex operating manual line.\n".repeat(80)
    )
    .trim()
    .to_owned();
    let codex_tools = json!([
        {
            "type": "function",
            "name": "exec_command",
            "description": "Runs a command in a PTY",
            "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]}
        },
        {
            "type": "custom",
            "name": "apply_patch",
            "description": "Patch files"
        }
    ]);

    // 1. Optimized mode (`/p/openai/v1/responses`) distills built-in prompt.
    let r1 = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/openai/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "instructions": raw_codex_builtin,
                        "input": "hello",
                        "tools": codex_tools,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);

    // 2. Passthrough mode (`/p/openai@passthrough/v1/responses`) preserves raw built-in prompt verbatim!
    let r2 = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/openai@passthrough/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "instructions": raw_codex_builtin,
                        "input": "hello",
                        "tools": codex_tools,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);

    let captured_instrs = observed_instructions.lock().unwrap().clone();
    assert_eq!(captured_instrs.len(), 2);
    assert!(
        !captured_instrs[0].contains(&raw_codex_builtin),
        "optimized mode should distill verbose built-in Codex prompt"
    );
    assert!(
        captured_instrs[1].contains(&raw_codex_builtin),
        "passthrough mode must preserve raw built-in Codex prompt verbatim"
    );

    // Wait briefly for background context capture writer to flush.
    for _ in 0..20 {
        if telemetry.context_captures().records.len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let caps = telemetry.context_captures();
    assert_eq!(caps.records.len(), 2);
    assert!(caps.records.iter().all(|r| r.provider_id == "openai"));
    let detail = telemetry
        .context_capture_detail(&caps.records[0].task_id)
        .expect("capture io")
        .expect("capture detail present");
    let wire = detail
        .payload
        .upstream_wire
        .expect("3-stage upstream_wire summary");
    assert_eq!(wire.provider_id, "openai");
    assert!(wire.isomorphic_wire);
    assert!(!wire.spill_enabled);

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn cross_protocol_codex_to_anthropic_aligns_cache_breakpoints_and_never_retries_429() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let upstream_hits = Arc::new(AtomicUsize::new(0));
    let hits_srv = Arc::clone(&upstream_hits);

    tokio::spawn(async move {
        // 1st request: Codex `/v1/responses` -> translated to Anthropic `/v1/messages` with `cache_control`!
        let (mut s1, _) = listener.accept().await.unwrap();
        let req1 = read_raw_http_body(&mut s1).await;
        hits_srv.fetch_add(1, Ordering::SeqCst);
        let sys_arr = req1["system"].as_array().expect("anthropic system array");
        assert!(
            sys_arr.iter().any(|b| b.get("cache_control").is_some()),
            "Anthropic system block must carry ephemeral cache_control breakpoint"
        );
        let sse = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":300,\"cache_creation_input_tokens\":50}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Cross-protocol OK\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":12}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        s1.write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{sse}").as_bytes(),
        )
        .await
        .unwrap();
        drop(s1);

        // 2nd request: upstream returns 429 -> gateway must abort immediately with 429 (single upstream hit, zero failover)!
        let (mut s2, _) = listener.accept().await.unwrap();
        let _ = read_raw_http_body(&mut s2).await;
        hits_srv.fetch_add(1, Ordering::SeqCst);
        let err_json =
            "{\"error\":{\"type\":\"rate_limit_error\",\"message\":\"quota exhausted\"}}";
        s2.write_all(
            format!(
                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{err_json}",
                err_json.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    });

    let reg_cfg = ProviderRegistryConfig::default();
    let mut anth = ProviderProfileConfig::builtin(
        ProviderId::anthropic(),
        ProviderDriverKind::AnthropicMessages,
    );
    anth.endpoint = Some(format!("http://{addr}/v1"));
    anth.auth = ApiKeySource::Stored;
    let registry = ProviderRuntimeRegistry::in_memory(reg_cfg, None);
    registry
        .upsert_provider(anth, Some("sk-ant-test".to_owned()))
        .expect("upsert stored anthropic profile");

    let (router, _) = build_test_router(registry, None);

    let ok_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/anthropic/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-sonnet-4-6",
                        "stream": false,
                        "instructions": "Follow repository guidelines.",
                        "input": "Hi Claude from Codex!",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok_resp.status(), StatusCode::OK);
    let ok_bytes = to_bytes(ok_resp.into_body(), 64 * 1024).await.unwrap();
    let ok_json: Value = serde_json::from_slice(&ok_bytes).unwrap();
    assert!(ok_json.to_string().contains("Cross-protocol OK"));

    let rl_resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/anthropic/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-sonnet-4-6",
                        "stream": false,
                        "input": "Trigger 429",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rl_resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        upstream_hits.load(Ordering::SeqCst),
        2,
        "429 must never retry or fail over"
    );
}

#[tokio::test]
async fn openai_previous_response_id_restores_history_and_custom_tool_call_output() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured_bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_srv = Arc::clone(&captured_bodies);

    tokio::spawn(async move {
        let (mut s1, _) = listener.accept().await.unwrap();
        let req1 = read_raw_http_body(&mut s1).await;
        captured_srv.lock().unwrap().push(req1);
        let sse1 = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"custom_tool_call\",\"call_id\":\"call_exec_9\",\"name\":\"exec\",\"input\":\"text(21*2)\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":300,\"output_tokens\":15,\"input_tokens_details\":{\"cached_tokens\":200}}}}\n\n",
            "data: [DONE]\n\n"
        );
        s1.write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{sse1}").as_bytes(),
        )
        .await
        .unwrap();
        drop(s1);

        let (mut s2, _) = listener.accept().await.unwrap();
        let req2 = read_raw_http_body(&mut s2).await;
        captured_srv.lock().unwrap().push(req2);
        let sse2 = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Result is 42.\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":350,\"output_tokens\":8,\"input_tokens_details\":{\"cached_tokens\":300}}}}\n\n",
            "data: [DONE]\n\n"
        );
        s2.write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{sse2}").as_bytes(),
        )
        .await
        .unwrap();
        drop(s2);
    });

    let mut reg_cfg = ProviderRegistryConfig::default();
    let mut oai =
        ProviderProfileConfig::builtin(ProviderId::openai(), ProviderDriverKind::OpenAiResponses);
    oai.endpoint = Some(format!("http://{addr}/v1"));
    oai.auth = ApiKeySource::None;
    reg_cfg.upsert(oai);

    let (router, _) = build_test_router(ProviderRuntimeRegistry::in_memory(reg_cfg, None), None);

    let resp1 = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/openai/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "instructions": "Keep exact math.",
                        "input": "Compute 21 * 2 via exec.",
                        "tools": [{
                            "type": "custom",
                            "name": "exec",
                            "description": "Execute JS"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);
    let body1: Value =
        serde_json::from_slice(&to_bytes(resp1.into_body(), 64 * 1024).await.unwrap()).unwrap();
    let resp1_id = body1["id"].as_str().expect("response id").to_owned();

    let resp2 = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/openai/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "previous_response_id": resp1_id,
                        "input": [{
                            "type": "custom_tool_call_output",
                            "call_id": "call_exec_9",
                            "output": "42"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let body2: Value =
        serde_json::from_slice(&to_bytes(resp2.into_body(), 64 * 1024).await.unwrap()).unwrap();
    assert!(body2.to_string().contains("Result is 42."));

    let seen = captured_bodies.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let req2_json = &seen[1];
    assert!(
        req2_json["instructions"]
            .as_str()
            .unwrap_or("")
            .contains("Keep exact math.")
    );
    let items = req2_json["input"].as_array().expect("input array");
    assert!(
        items
            .iter()
            .any(|i| i.to_string().contains("Compute 21 * 2 via exec."))
    );
    assert!(
        items
            .iter()
            .any(|i| i["type"] == "custom_tool_call" && i["call_id"] == "call_exec_9")
    );
    assert!(
        items
            .iter()
            .any(|i| i["type"] == "custom_tool_call_output" && i["call_id"] == "call_exec_9")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn codex_app_server_provider_executes_client_tools_and_restores_continuation() {
    use std::os::unix::fs::PermissionsExt;

    let tmp_dir = std::env::temp_dir().join(format!("lg-codex-app-srv-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let script_path = tmp_dir.join("mock_codex_app_server.py");
    let log_path = tmp_dir.join("prompts.log");
    let script = format!(
        r#"#!/usr/bin/env python3
import json, sys
log_file = {log_lit}
for raw in sys.stdin:
    msg = json.loads(raw)
    method = msg.get("method", "")
    mid = msg.get("id")
    if method == "initialize":
        sys.stdout.write(json.dumps({{"jsonrpc":"2.0","id":mid,"result":{{"userAgent":"mock"}}}}) + "\n")
        sys.stdout.flush()
    elif method == "thread/start":
        dyn_tools = [t["name"] for t in (msg["params"].get("dynamicTools") or [])]
        with open(log_file, "a") as f:
            f.write("TOOLS:" + ",".join(dyn_tools) + "\n")
        sys.stdout.write(json.dumps({{"jsonrpc":"2.0","id":mid,"result":{{"thread":{{"id":"thr_mock"}}}}}}) + "\n")
        sys.stdout.flush()
    elif method == "turn/start":
        prompt_txt = msg["params"]["input"][0]["text"]
        with open(log_file, "a") as f:
            f.write("PROMPT:" + json.dumps(prompt_txt) + "\n")
        sys.stdout.write(json.dumps({{"jsonrpc":"2.0","id":mid,"result":{{"turn":{{"id":"t_mock"}}}}}}) + "\n")
        sys.stdout.flush()
        if "<tool_result" not in prompt_txt:
            call = {{"jsonrpc":"2.0","id":77,"method":"item/tool/call","params":{{"callId":"call_read_1","threadId":"thr_mock","turnId":"t_mock","tool":"read_file","arguments":{{"path":"README.md"}}}}}}
            sys.stdout.write(json.dumps(call) + "\n")
            sys.stdout.flush()
            break
        else:
            delta = {{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{{"delta":"Read README successfully."}}}}
            done = {{"jsonrpc":"2.0","method":"turn/completed","params":{{"turn":{{"status":"completed"}}}}}}
            sys.stdout.write(json.dumps(delta) + "\n" + json.dumps(done) + "\n")
            sys.stdout.flush()
            break
"#,
        log_lit = serde_json::to_string(&log_path.to_string_lossy().into_owned()).unwrap()
    );
    std::fs::write(&script_path, script).unwrap();
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut reg_cfg = ProviderRegistryConfig::default();
    let mut codex_prov = ProviderProfileConfig::builtin(
        ProviderId::try_new("codex-app-server").unwrap(),
        ProviderDriverKind::CodexAppServer,
    );
    codex_prov.command = Some(script_path);
    codex_prov.command_args = Vec::new();
    reg_cfg.upsert(codex_prov);

    let (router, _) = build_test_router(ProviderRuntimeRegistry::in_memory(reg_cfg, None), None);

    let r1 = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/codex-app-server/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "input": "Inspect README.md",
                        "tools": [{
                            "type": "function",
                            "name": "read_file",
                            "description": "Read file",
                            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let b1: Value =
        serde_json::from_slice(&to_bytes(r1.into_body(), 64 * 1024).await.unwrap()).unwrap();
    let r1_id = b1["id"].as_str().unwrap().to_owned();
    let out1 = b1["output"].as_array().unwrap();
    assert!(out1.iter().any(|i| i["type"] == "function_call"
        && i["name"] == "read_file"
        && i["call_id"] == "call_read_1"));

    let r2 = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/codex-app-server/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "previous_response_id": r1_id,
                        "input": [{
                            "type": "function_call_output",
                            "call_id": "call_read_1",
                            "output": "# Liftgate"
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
        serde_json::from_slice(&to_bytes(r2.into_body(), 64 * 1024).await.unwrap()).unwrap();
    assert!(b2.to_string().contains("Read README successfully."));

    let logged = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        logged.contains("read_file"),
        "dynamicTools must include read_file: {logged}"
    );
    assert!(
        logged.contains("<tool_call id=\\\"call_read_1\\\" name=\\\"read_file\\\">")
            && logged.contains("<tool_result call_id=\\\"call_read_1\\\""),
        "Turn 2 prompt must carry restored tool_call and tool_result history: {logged}"
    );
}

#[tokio::test]
async fn agy_passthrough_mode_disables_tool_virtualization_spill_and_billing_noise_stripping() {
    use liftgate_server::mcp_bridge::LoopbackMcpServer;
    use liftgate_types::acp::{AcpChunkContent, AcpUsage, McpServerConfig};

    struct RecordingAcpBackend {
        prompts: Mutex<Vec<String>>,
        mcp_urls: Mutex<Vec<String>>,
    }

    impl AcpBackend for RecordingAcpBackend {
        fn available_models(&self) -> Vec<AvailableModel> {
            vec![AvailableModel {
                model_id: "gemini-4-carbon-high".to_owned(),
                name: "Gemini 4 Carbon (High)".to_owned(),
            }]
        }

        async fn create_session(&self, _system_append: &str) -> Result<SessionNewResult, AcpError> {
            Ok(SessionNewResult {
                session_id: SessionId::try_new("agy_pass_sess").unwrap(),
                modes: ModesCatalog::default(),
                models: ModelsCatalog {
                    available_models: Vec::new(),
                    current_model_id: None,
                },
            })
        }

        async fn create_session_with_mcp(
            &self,
            _system_append: &str,
            mcp_servers: &[McpServerConfig],
        ) -> Result<SessionNewResult, AcpError> {
            for s in mcp_servers {
                if let Some(ref u) = s.url {
                    self.mcp_urls.lock().unwrap().push(u.clone());
                }
            }
            self.create_session(_system_append).await
        }

        async fn set_session_mode(
            &self,
            _session_id: &SessionId,
            _mode_id: &AcpModeId,
        ) -> Result<(), AcpError> {
            Ok(())
        }

        async fn set_session_model(
            &self,
            _session_id: &SessionId,
            _model_id: &ModelId,
        ) -> Result<(), AcpError> {
            Ok(())
        }

        async fn start_prompt(
            &self,
            _session_id: &SessionId,
            prompt: &[AcpPromptBlock],
            updates_tx: mpsc::Sender<SessionUpdatePayload>,
        ) -> Result<PromptTurnResult, AcpError> {
            let mut combined = String::new();
            for b in prompt {
                if let AcpPromptBlock::Text { text } = b {
                    combined.push_str(text);
                    combined.push('\n');
                }
            }
            self.prompts.lock().unwrap().push(combined);
            let _ = updates_tx
                .send(SessionUpdatePayload::AgentMessageChunk {
                    content: AcpChunkContent {
                        content_type: "text".to_owned(),
                        text: "Passthrough OK".to_owned(),
                    },
                })
                .await;
            Ok(PromptTurnResult {
                stop_reason: "end_turn".to_owned(),
                usage: AcpUsage {
                    input_tokens: 120,
                    output_tokens: 10,
                    total_tokens: 130,
                    ..AcpUsage::default()
                },
                error: None,
            })
        }

        fn abort_session_nonblocking(&self, _session_id: &SessionId) {}

        async fn close_session(&self, _session_id: &SessionId) -> Result<(), AcpError> {
            Ok(())
        }

        fn process_telemetry(&self, default_model: &str, acp_mode: &str) -> AcpProcessTelemetry {
            DummyAcpBackend.process_telemetry(default_model, acp_mode)
        }
    }

    let limits = GatewayLimits::default();
    let mcp_server = LoopbackMcpServer::bind(limits.max_body_bytes(), limits.channel_capacity())
        .await
        .unwrap();
    let spill_dir =
        std::env::temp_dir().join(format!("lg-agy-pass-spill-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&spill_dir).unwrap();

    let backend = Arc::new(RecordingAcpBackend {
        prompts: Mutex::new(Vec::new()),
        mcp_urls: Mutex::new(Vec::new()),
    });
    let telemetry = Arc::new(TelemetryHub::new());
    telemetry.set_provider_registry(Arc::new(ProviderRuntimeRegistry::default()));
    let pool = Arc::new(SessionPool::new(limits));
    let router = build_router(AppState {
        backend: Arc::clone(&backend),
        pool: Arc::clone(&pool),
        telemetry: Arc::clone(&telemetry),
        config: GatewayRuntimeConfig {
            title_model_file: None,
            spill_dir: Some(spill_dir.clone()),
            settings_path: None,
            default_model: ModelId::fallback_primary(),
            acp_mode: AcpModeId::try_new("agent").unwrap(),
            api_key: None,
            limits,
            vision_offload: Arc::new(AtomicBool::new(false)),
            mcp_tool_bridge: Arc::new(AtomicBool::new(true)),
            mcp_server: Some(mcp_server),
        },
        ui_dist_dir: None,
        embedded_ui: &[],
        admin_shutdown: None,
    });

    let mut tools = Vec::new();
    for i in 0..40 {
        tools.push(json!({
            "name": format!("mcp__svc__op_{i}"),
            "description": format!("Detailed uncompacted tool description #{i}"),
            "input_schema": {"type": "object", "properties": {"id": {"type": "string"}}}
        }));
    }

    let large_tool_out = "LINE_DATA_1234567890\n".repeat(700); // ~14.7 KB (> 9.6 KB agy spill wall)
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/agy@passthrough/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-4-carbon-high",
                        "max_tokens": 1024,
                        "stream": false,
                        "system": "x-anthropic-billing-header: cc_version=2.1.80; cch=abc12;\nYou are a Claude agent, built on Anthropic's Claude Agent SDK.",
                        "tools": tools,
                        "messages": [
                            {"role": "user", "content": "Run check"},
                            {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "mcp__svc__op_0", "input": {"id": "1"}}]},
                            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": large_tool_out}]}
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let resp_bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "unexpected status {status}: {}",
        String::from_utf8_lossy(&resp_bytes)
    );

    // 1. Billing noise & raw Claude SDK preamble must be preserved verbatim on passthrough!
    let seen_prompts = backend.prompts.lock().unwrap().clone();
    assert_eq!(seen_prompts.len(), 1);
    assert!(
        seen_prompts[0].contains("x-anthropic-billing-header: cc_version=2.1.80"),
        "passthrough must preserve billing header when strip_billing_noise is false"
    );
    assert!(
        seen_prompts[0].contains("built on Anthropic's Claude Agent SDK"),
        "passthrough must preserve raw SDK preamble"
    );

    // 2. Spill directory must remain completely empty (spill_truncation = false in passthrough)!
    let spill_entries: Vec<_> = std::fs::read_dir(&spill_dir).unwrap().collect();
    assert!(
        spill_entries.is_empty(),
        "passthrough on agy must not spill tool results to disk"
    );

    // 3. SessionPool MCP summary must report passthrough (zero virtualized hubs, 40 direct tools)!
    let pool_tel = pool.snapshot();
    assert_eq!(pool_tel.sessions.len(), 1);
    let mcp_summary = pool_tel.sessions[0].mcp.as_ref().expect("mcp summary");
    assert!(mcp_summary.is_passthrough);
    assert_eq!(mcp_summary.virtualized_hubs, 0);
    assert_eq!(
        mcp_summary.eager_tools, 40,
        "passthrough must expose all 40 client tools directly without search_client_tools/call_client_tool"
    );
    let urls = backend.mcp_urls.lock().unwrap().clone();
    assert_eq!(urls.len(), 1);
}

#[tokio::test]
async fn disabled_provider_is_rejected_and_gemini_preserves_thought_signatures_across_continuations()
 {
    // 1. Disabled provider must be rejected on both explicit `/p/<id>` and active-provider routes
    let mut disabled_cfg = ProviderRegistryConfig::default();
    let mut disabled_oai =
        ProviderProfileConfig::builtin(ProviderId::openai(), ProviderDriverKind::OpenAiResponses);
    disabled_oai.enabled = false;
    disabled_oai.auth = ApiKeySource::None;
    disabled_cfg.upsert(disabled_oai);
    disabled_cfg.active_provider = ProviderId::openai();

    let (disabled_router, _) =
        build_test_router(ProviderRuntimeRegistry::in_memory(disabled_cfg, None), None);

    let explicit_resp = disabled_router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/openai/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model": "gpt-5.4", "stream": false, "input": "Hi"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(explicit_resp.status(), StatusCode::BAD_REQUEST);
    let explicit_body: Value = serde_json::from_slice(
        &to_bytes(explicit_resp.into_body(), 64 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(explicit_body["error"]["code"], "provider_disabled");

    let active_resp = disabled_router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "max_tokens": 256,
                        "stream": false,
                        "messages": [{"role": "user", "content": "Hi"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(active_resp.status(), StatusCode::BAD_REQUEST);

    // 2. Gemini `thoughtSignature` must survive both 0-RTT local continuations AND client continuations
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen_gemini_reqs: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_srv = Arc::clone(&seen_gemini_reqs);

    tokio::spawn(async move {
        // Step 1: Gemini invokes 0-RTT `search_client_tools` with `thoughtSignature: "sig_0rtt_1"`
        let (mut s1, _) = listener.accept().await.unwrap();
        let req1 = read_raw_http_body(&mut s1).await;
        seen_srv.lock().unwrap().push(req1);
        let sse1 = concat!(
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"search_client_tools\",\"args\":{\"namespace\":\"gh\",\"query\":\"pr_0\"}},\"thoughtSignature\":\"sig_0rtt_1\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":200,\"candidatesTokenCount\":15}}\n\n",
            "data: [DONE]\n\n"
        );
        s1.write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{sse1}").as_bytes(),
        )
        .await
        .unwrap();
        drop(s1);

        // Step 2 (automatic 0-RTT continuation): Gemini receives `sig_0rtt_1` and calls client `exec_command` with `sig_client_2`
        let (mut s2, _) = listener.accept().await.unwrap();
        let req2 = read_raw_http_body(&mut s2).await;
        seen_srv.lock().unwrap().push(req2);
        let sse2 = concat!(
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"exec_command\",\"args\":{\"cmd\":\"cargo --version\"}},\"thoughtSignature\":\"sig_client_2\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":240,\"candidatesTokenCount\":18}}\n\n",
            "data: [DONE]\n\n"
        );
        s2.write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{sse2}").as_bytes(),
        )
        .await
        .unwrap();
        drop(s2);

        // Turn 2 (client `previous_response_id` continuation): Gemini receives BOTH `sig_0rtt_1` and `sig_client_2`
        let (mut s3, _) = listener.accept().await.unwrap();
        let req3 = read_raw_http_body(&mut s3).await;
        seen_srv.lock().unwrap().push(req3);
        let sse3 = concat!(
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"cargo 1.100.0 verified.\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":280,\"candidatesTokenCount\":10}}\n\n",
            "data: [DONE]\n\n"
        );
        s3.write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{sse3}").as_bytes(),
        )
        .await
        .unwrap();
        drop(s3);
    });

    let mut gem_cfg = ProviderRegistryConfig::default();
    let mut gem =
        ProviderProfileConfig::builtin(ProviderId::gemini(), ProviderDriverKind::GoogleGemini);
    gem.endpoint = Some(format!("http://{addr}"));
    gem.auth = ApiKeySource::None;
    gem_cfg.upsert(gem);

    let (gem_router, _) =
        build_test_router(ProviderRuntimeRegistry::in_memory(gem_cfg, None), None);

    let mut tools = vec![json!({
        "type": "function",
        "name": "exec_command",
        "description": "Run shell",
        "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]}
    })];
    for i in 0..50 {
        tools.push(json!({
            "type": "function",
            "name": format!("mcp__gh__pr_{i}"),
            "description": "GitHub PR action",
            "parameters": {"type": "object", "properties": {"id": {"type": "number"}}}
        }));
    }

    let r1 = gem_router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/gemini/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-3-pro",
                        "stream": false,
                        "input": "Check cargo version",
                        "tools": tools
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let b1: Value =
        serde_json::from_slice(&to_bytes(r1.into_body(), 64 * 1024).await.unwrap()).unwrap();
    let resp1_id = b1["id"].as_str().expect("resp1 id").to_owned();
    let call_id = b1["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .and_then(|i| i["call_id"].as_str())
        .expect("forwarded call_id")
        .to_owned();

    let r2 = gem_router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/gemini/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-3-pro",
                        "stream": false,
                        "previous_response_id": resp1_id,
                        "input": [{
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": "cargo 1.100.0"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);

    let reqs = seen_gemini_reqs.lock().unwrap();
    assert_eq!(reqs.len(), 3);
    let req2_str = reqs[1]["contents"].to_string();
    assert!(
        req2_str.contains("\"thoughtSignature\":\"sig_0rtt_1\""),
        "0-RTT continuation must replay thoughtSignature sig_0rtt_1: {req2_str}"
    );
    let req3_str = reqs[2]["contents"].to_string();
    assert!(
        req3_str.contains("\"thoughtSignature\":\"sig_0rtt_1\"")
            && req3_str.contains("\"thoughtSignature\":\"sig_client_2\""),
        "Client continuation must replay both thoughtSignatures without collision: {req3_str}"
    );
}

#[tokio::test]
async fn gemini_anthropic_ingress_preserves_thought_signature_across_toolu_id_rewrite() {
    let gem_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gem_addr = gem_listener.local_addr().unwrap();
    let seen_reqs: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_clone = Arc::clone(&seen_reqs);
    let step_idx = Arc::new(AtomicUsize::new(0));
    let step_clone = Arc::clone(&step_idx);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = gem_listener.accept().await else {
                break;
            };
            let seen = Arc::clone(&seen_clone);
            let n = step_clone.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let req_json = read_raw_http_body(&mut sock).await;
                seen.lock().unwrap().push(req_json);
                let sse = if n == 0 {
                    concat!(
                        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[",
                        "{\"functionCall\":{\"id\":\"3650728205117214884\",\"name\":\"get_version\",\"args\":{}},\"thoughtSignature\":\"sig_gem3_numeric_id\"}",
                        "]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":20,\"candidatesTokenCount\":8,\"totalTokenCount\":28}}\n\n"
                    )
                } else {
                    concat!(
                        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[",
                        "{\"text\":\"Version verified!\"}",
                        "]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":40,\"candidatesTokenCount\":5,\"totalTokenCount\":45}}\n\n"
                    )
                };
                let hdr = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                let _ = sock.write_all(hdr.as_bytes()).await;
                let _ = sock.write_all(sse.as_bytes()).await;
            });
        }
    });

    let mut cfg = ProviderRegistryConfig::default();
    let mut gem = ProviderProfileConfig::builtin(
        ProviderId::try_new("gemini").unwrap(),
        ProviderDriverKind::GoogleGemini,
    );
    gem.endpoint = Some(format!("http://{gem_addr}"));
    gem.auth = ApiKeySource::None;
    gem.force_direct_proxy = true;
    cfg.upsert(gem);
    let registry = ProviderRuntimeRegistry::in_memory(cfg, None);
    let (router, _) = build_test_router(registry, None);

    let r1 = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/gemini/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-3-flash-preview",
                        "max_tokens": 1024,
                        "stream": false,
                        "messages": [{"role": "user", "content": "Check version"}],
                        "tools": [{
                            "name": "get_version",
                            "description": "Get version",
                            "input_schema": {"type": "object", "properties": {}}
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let b1: Value =
        serde_json::from_slice(&to_bytes(r1.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    let tool_id = b1["content"][0]["id"].as_str().expect("toolu id");
    assert_eq!(tool_id, "toolu_3650728205117214884");

    let r2 = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/gemini/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-3-flash-preview",
                        "max_tokens": 1024,
                        "stream": false,
                        "messages": [
                            {"role": "user", "content": "Check version"},
                            {"role": "assistant", "content": [{
                                "type": "tool_use",
                                "id": tool_id,
                                "name": "get_version",
                                "input": {}
                            }]},
                            {"role": "user", "content": [{
                                "type": "tool_result",
                                "tool_use_id": tool_id,
                                "content": "0.2.24"
                            }]}
                        ],
                        "tools": [{
                            "name": "get_version",
                            "description": "Get version",
                            "input_schema": {"type": "object", "properties": {}}
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let reqs = seen_reqs.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    let turn2_body = reqs[1]["contents"].to_string();
    assert!(
        turn2_body.contains("\"thoughtSignature\":\"sig_gem3_numeric_id\""),
        "Turn 2 Gemini request must restore thoughtSignature after toolu_ rewrite: {turn2_body}"
    );
}

#[tokio::test]
async fn anthropic_extended_thinking_preserves_signed_blocks_across_continuations() {
    let anth_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let anth_addr = anth_listener.local_addr().unwrap();
    let seen_reqs: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_clone = Arc::clone(&seen_reqs);
    let call_num = Arc::new(AtomicUsize::new(0));
    let call_clone = Arc::clone(&call_num);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = anth_listener.accept().await else {
                break;
            };
            let seen = Arc::clone(&seen_clone);
            let n = call_clone.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let req_json = read_raw_http_body(&mut sock).await;
                seen.lock().unwrap().push(req_json);
                let sse = match n {
                    0 => concat!(
                        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":50,\"output_tokens\":1}}}\n\n",
                        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
                        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Need to search tools.\"}}\n\n",
                        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"EqQB_sig_step0\"}}\n\n",
                        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"EmwK_redacted_0\"}}\n\n",
                        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
                        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_0rtt_1\",\"name\":\"search_client_tools\"}}\n\n",
                        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"query\\\":\\\"ping\\\"}\"}}\n\n",
                        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
                        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":20}}\n\n",
                        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
                    ),
                    1 => concat!(
                        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":80,\"output_tokens\":1}}}\n\n",
                        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
                        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Calling client tool.\"}}\n\n",
                        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"EqQB_sig_step1\"}}\n\n",
                        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_client_1\",\"name\":\"mcp_ext__ping_0\"}}\n\n",
                        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
                        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
                        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":25}}\n\n",
                        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
                    ),
                    _ => concat!(
                        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":110,\"output_tokens\":1}}}\n\n",
                        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"All done!\"}}\n\n",
                        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":10}}\n\n",
                        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
                    ),
                };
                let hdr = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                let _ = sock.write_all(hdr.as_bytes()).await;
                let _ = sock.write_all(sse.as_bytes()).await;
            });
        }
    });

    let mut cfg = ProviderRegistryConfig::default();
    let mut anth = ProviderProfileConfig::builtin(
        ProviderId::try_new("anthropic").unwrap(),
        ProviderDriverKind::AnthropicMessages,
    );
    anth.endpoint = Some(format!("http://{anth_addr}"));
    anth.auth = ApiKeySource::None;
    anth.force_direct_proxy = true;
    cfg.upsert(anth);
    let registry = ProviderRuntimeRegistry::in_memory(cfg, None);
    let (router, _) = build_test_router(registry, None);

    let tools: Vec<Value> = (0..50)
        .map(|i| {
            json!({
                "name": format!("mcp_ext__ping_{i}"),
                "description": "Ping tool",
                "input_schema": {"type": "object", "properties": {}}
            })
        })
        .collect();

    let r1 = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/anthropic/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-sonnet-4-6",
                        "max_tokens": 32768,
                        "stream": false,
                        "thinking": {"type": "enabled", "budget_tokens": 16384},
                        "messages": [{"role": "user", "content": "Run ping_0"}],
                        "tools": tools,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let b1: Value =
        serde_json::from_slice(&to_bytes(r1.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    let content1 = b1["content"].as_array().expect("content blocks");
    assert_eq!(content1[0]["type"], "thinking");
    assert_eq!(content1[0]["signature"], "EqQB_sig_step0");
    assert_eq!(content1[1]["type"], "redacted_thinking");
    assert_eq!(content1[1]["data"], "EmwK_redacted_0");
    assert_eq!(content1[2]["type"], "thinking");
    assert_eq!(content1[2]["signature"], "EqQB_sig_step1");
    let tool_blk = content1
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("tool_use");
    let tool_id = tool_blk["id"].as_str().unwrap();
    assert_eq!(tool_id, "toolu_client_1");

    // Turn 2: replay exact content1 blocks back to /p/anthropic/v1/messages
    let r2 = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/anthropic/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-sonnet-4-6",
                        "max_tokens": 32768,
                        "stream": false,
                        "thinking": {"type": "enabled", "budget_tokens": 16384},
                        "messages": [
                            {"role": "user", "content": "Run ping_0"},
                            {"role": "assistant", "content": content1},
                            {"role": "user", "content": [{
                                "type": "tool_result",
                                "tool_use_id": tool_id,
                                "content": "pong"
                            }]}
                        ],
                        "tools": tools,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);

    let reqs = seen_reqs.lock().unwrap();
    assert_eq!(reqs.len(), 3);
    // Step 1 (local 0-RTT continuation) must keep thinking enabled and replay signed + redacted blocks!
    assert_eq!(reqs[1]["thinking"]["type"], "enabled");
    let step1_asst = &reqs[1]["messages"][1]["content"];
    assert_eq!(step1_asst[0]["type"], "thinking");
    assert_eq!(step1_asst[0]["signature"], "EqQB_sig_step0");
    assert_eq!(step1_asst[1]["type"], "redacted_thinking");
    assert_eq!(step1_asst[1]["data"], "EmwK_redacted_0");
    assert_eq!(step1_asst[2]["type"], "tool_use");

    // Step 2 (client Turn 2 continuation) must also keep thinking enabled and replay signed + redacted blocks!
    assert_eq!(reqs[2]["thinking"]["type"], "enabled");
    let step2_asst = &reqs[2]["messages"][1]["content"];
    assert_eq!(step2_asst[0]["type"], "thinking");
    assert_eq!(step2_asst[0]["signature"], "EqQB_sig_step0");
    assert_eq!(step2_asst[1]["type"], "redacted_thinking");
    assert_eq!(step2_asst[2]["type"], "thinking");
    assert_eq!(step2_asst[2]["signature"], "EqQB_sig_step1");
    assert_eq!(step2_asst[3]["type"], "tool_use");
}

#[tokio::test]
async fn streaming_http_provider_propagates_pre_sse_401_402_and_429_status_codes() {
    let err_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let err_addr = err_listener.local_addr().unwrap();
    let seq = Arc::new(AtomicUsize::new(0));
    let seq_clone = Arc::clone(&seq);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = err_listener.accept().await else {
                break;
            };
            let idx = seq_clone.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let _ = read_raw_http_body(&mut sock).await;
                let resp = match idx {
                    0 => concat!(
                        "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                        "{\"error\":{\"message\":\"Invalid API key\",\"type\":\"authentication_error\"}}"
                    ),
                    1 => concat!(
                        "HTTP/1.1 402 Payment Required\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                        "{\"error\":{\"message\":\"Quota exceeded for account\",\"type\":\"rate_limit_error\"}}"
                    ),
                    _ => concat!(
                        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                        "{\"error\":{\"message\":\"Rate limit exceeded\",\"type\":\"rate_limit_error\"}}"
                    ),
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });

    let mut cfg = ProviderRegistryConfig::default();
    let mut kiro = ProviderProfileConfig::builtin(
        ProviderId::try_new("kiro").unwrap(),
        ProviderDriverKind::AnthropicMessages,
    );
    kiro.endpoint = Some(format!("http://{err_addr}"));
    kiro.auth = ApiKeySource::None;
    kiro.force_direct_proxy = true;
    cfg.upsert(kiro);
    let registry = ProviderRuntimeRegistry::in_memory(cfg, None);
    let (router, _) = build_test_router(registry, None);

    // 1. 401 with stream: true -> HTTP 401 (NOT 200 OK SSE!)
    let r401 = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/kiro/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-sonnet-4-6",
                        "max_tokens": 256,
                        "stream": true,
                        "messages": [{"role": "user", "content": "hi"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r401.status(), StatusCode::UNAUTHORIZED);

    // 2. 402 Quota exceeded with stream: true on Anthropic ingress -> HTTP 400 invalid_request_error (halt retries)
    let r402 = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/kiro/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-sonnet-4-6",
                        "max_tokens": 256,
                        "stream": true,
                        "messages": [{"role": "user", "content": "hi"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r402.status(), StatusCode::BAD_REQUEST);
    let b402: Value =
        serde_json::from_slice(&to_bytes(r402.into_body(), 64 * 1024).await.unwrap()).unwrap();
    assert!(
        b402["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("Quota exceeded"),
        "unexpected 402 body: {b402}"
    );

    // 3. 429 with stream: true -> HTTP 429
    let r429 = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/kiro/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-sonnet-4-6",
                        "stream": true,
                        "input": "hi"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r429.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[cfg(unix)]
#[tokio::test]
async fn codex_app_server_handles_gateway_web_fetch_locally_and_forwards_images() {
    use std::os::unix::fs::PermissionsExt;

    let tmp_dir = std::env::temp_dir().join(format!("lg-codex-img-fetch-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let script_path = tmp_dir.join("mock_codex_multimodal.py");
    let log_path = tmp_dir.join("events.log");
    let script = format!(
        r#"#!/usr/bin/env python3
import json, sys
log_file = {log_lit}
it = iter(sys.stdin)
for raw in it:
    msg = json.loads(raw)
    method = msg.get("method", "")
    mid = msg.get("id")
    if method == "initialize":
        sys.stdout.write(json.dumps({{"jsonrpc":"2.0","id":mid,"result":{{"userAgent":"mock"}}}}) + "\n")
        sys.stdout.flush()
    elif method == "thread/start":
        dyn_tools = [t["name"] for t in (msg["params"].get("dynamicTools") or [])]
        with open(log_file, "a") as f:
            f.write("DYN:" + ",".join(dyn_tools) + "\n")
        sys.stdout.write(json.dumps({{"jsonrpc":"2.0","id":mid,"result":{{"thread":{{"id":"thr_img"}}}}}}) + "\n")
        sys.stdout.flush()
    elif method == "turn/start":
        inp = msg["params"]["input"]
        with open(log_file, "a") as f:
            f.write("INPUT:" + json.dumps(inp) + "\n")
        sys.stdout.write(json.dumps({{"jsonrpc":"2.0","id":mid,"result":{{"turn":{{"id":"t_img"}}}}}}) + "\n")
        sys.stdout.flush()
        call = {{"jsonrpc":"2.0","id":88,"method":"item/tool/call","params":{{"callId":"call_wf_1","threadId":"thr_img","turnId":"t_img","tool":"gateway_web_fetch","arguments":{{"url":"http://127.0.0.1:9/ssrf-blocked"}}}}}}
        sys.stdout.write(json.dumps(call) + "\n")
        sys.stdout.flush()
        reply = json.loads(next(it))
        with open(log_file, "a") as f:
            f.write("WF_REPLY:" + json.dumps(reply) + "\n")
        delta = {{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{{"delta":"Image inspected and web fetch handled locally."}}}}
        done = {{"jsonrpc":"2.0","method":"turn/completed","params":{{"turn":{{"status":"completed"}}}}}}
        sys.stdout.write(json.dumps(delta) + "\n" + json.dumps(done) + "\n")
        sys.stdout.flush()
        break
"#,
        log_lit = serde_json::to_string(&log_path.to_string_lossy().into_owned()).unwrap()
    );
    std::fs::write(&script_path, script).unwrap();
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut cfg = ProviderRegistryConfig::default();
    let mut codex_prof = ProviderProfileConfig::builtin(
        ProviderId::try_new("codex-cli").unwrap(),
        ProviderDriverKind::CodexAppServer,
    );
    codex_prof.command = Some(script_path);
    codex_prof.command_args = Vec::new();
    cfg.upsert(codex_prof);

    let (router, _) = build_test_router(ProviderRuntimeRegistry::in_memory(cfg, None), None);
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/p/codex-cli/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-5.4",
                        "max_tokens": 512,
                        "stream": false,
                        "messages": [{
                            "role": "user",
                            "content": [
                                {"type": "text", "text": "Inspect image and fetch URL"},
                                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}}
                            ]
                        }],
                        "tools": [
                            {"name": "read_file", "description": "Read", "input_schema": {"type": "object", "properties": {}}},
                            {"type": "web_fetch_20250910", "name": "web_fetch"}
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_json: Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    assert_eq!(body_json["stop_reason"], "end_turn");
    assert!(
        body_json["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("handled locally"),
        "unexpected response body: {body_json}"
    );

    let logged = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        logged.contains("gateway_web_fetch"),
        "expected gateway_web_fetch in dynamicTools: {logged}"
    );
    assert!(
        logged.contains("data:image/png;base64,iVBORw0KGgo="),
        "expected base64 data URI image in turn/start input: {logged}"
    );
    assert!(
        logged.contains("WF_REPLY:") && logged.contains("\"id\": 88"),
        "expected in-band local reply for gateway_web_fetch: {logged}"
    );
}
