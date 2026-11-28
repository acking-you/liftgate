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
    assert_eq!(oai_usage.tokens.input, 1100);
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
