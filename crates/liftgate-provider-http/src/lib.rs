//! Stateless native Platform HTTP/SSE client and streaming codecs for OpenAI, Anthropic, and Google Gemini.

pub mod codecs;
pub mod error;
pub mod sse;
pub mod transport;

pub use codecs::{
    NativeDriverInput, NativeStreamEvent, NativeWireTool, PreparedWireMetrics,
    build_driver_http_request, execute_driver_sse_step, probe_http_provider,
};
pub use error::PlatformApiError;
pub use transport::{DefaultHttpTransport, HttpJsonResponse, HttpRequestSpec, HttpSseTransport};

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use liftgate_types::{
        canonical::{CanonicalMessage, CanonicalPart, CanonicalRole},
        provider::ProviderDriverKind,
    };
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    use crate::{
        DefaultHttpTransport, NativeDriverInput, NativeStreamEvent, NativeWireTool,
        PlatformApiError, execute_driver_sse_step,
    };

    async fn read_full_http_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::with_capacity(4096);
        let mut tmp = [0u8; 2048];
        let mut expected_total: Option<usize> = None;
        loop {
            let n = stream.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            if let Some(slice) = tmp.get(..n) {
                buf.extend_from_slice(slice);
            }
            if expected_total.is_none()
                && let Some(hdr_end) = buf.windows(4).position(|w| w == b"\r\n\r\n")
            {
                let body_start = hdr_end + 4;
                let hdr_text = String::from_utf8_lossy(buf.get(..hdr_end).unwrap_or(&[]));
                let cl = hdr_text
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
            if let Some(total) = expected_total
                && buf.len() >= total
            {
                break;
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[tokio::test]
    async fn openai_responses_driver_streams_text_parallel_tools_and_cached_tokens() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let raw_req = read_full_http_request(&mut sock).await;
            assert!(raw_req.starts_with("POST /v1/responses HTTP/1.1\r\n"));
            assert!(raw_req.contains("Authorization: Bearer sk-test-oai\r\n"));

            let sse_body = concat!(
                "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"Plan\"}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Checking files.\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"custom_tool_call\",\"call_id\":\"call_2\",\"name\":\"exec\",\"input\":\"text(42)\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1000,\"output_tokens\":80,\"output_tokens_details\":{\"reasoning_tokens\":20},\"input_tokens_details\":{\"cached_tokens\":768}}}}\n\n",
                "data: [DONE]\n\n"
            );
            let resp =
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{sse_body}");
            sock.write_all(resp.as_bytes()).await.unwrap();
        });

        let input = NativeDriverInput {
            provider_id: "openai".to_owned(),
            driver: ProviderDriverKind::OpenAiResponses,
            endpoint: format!("http://{addr}/v1"),
            api_key: Some("sk-test-oai".to_owned()),
            proxy: None,
            force_direct_proxy: true,
            model: "gpt-5.4".to_owned(),
            effort: None,
            max_output_tokens: None,
            system_blocks: vec!["You are helpful.".to_owned()],
            messages: vec![CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text("Inspect repo".to_owned())],
            }],
            tools: vec![
                NativeWireTool {
                    name: "read_file".to_owned(),
                    description: Some("Read".to_owned()),
                    input_schema: json!({"type":"object"}),
                    is_custom_freeform: false,
                    custom_format: None,
                },
                NativeWireTool {
                    name: "exec".to_owned(),
                    description: Some("Exec JS".to_owned()),
                    input_schema: json!({"type":"object"}),
                    is_custom_freeform: true,
                    custom_format: None,
                },
            ],
            enable_native_web_search: false,
            thought_signatures: std::collections::HashMap::new(),
            prompt_cache_key: Some("sess_abc".to_owned()),
            align_cache_breakpoints: true,
            connect_timeout: Duration::from_secs(5),
            total_timeout: Duration::from_secs(10),
        };

        let (tx, mut rx) = mpsc::channel(32);
        let metrics = execute_driver_sse_step(&DefaultHttpTransport, &input, &tx)
            .await
            .expect("step ok");
        drop(tx);
        assert!(metrics.wire_body_bytes > 50);

        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev.expect("event ok"));
        }
        assert!(events.contains(&NativeStreamEvent::ReasoningDelta("Plan".to_owned())));
        assert!(events.contains(&NativeStreamEvent::TextDelta("Checking files.".to_owned())));
        assert!(events.iter().any(|e| matches!(e, NativeStreamEvent::ToolCall { name, is_custom: false, .. } if name == "read_file")));
        assert!(events.iter().any(|e| matches!(e, NativeStreamEvent::ToolCall { name, is_custom: true, .. } if name == "exec")));
        assert!(events.iter().any(|e| matches!(
            e,
            NativeStreamEvent::Usage(u)
                if u.input_tokens == 232
                    && u.cached_read_tokens == 768
                    && u.total_prompt_tokens() == 1000
        )));
    }

    #[tokio::test]
    async fn anthropic_driver_aligns_cache_breakpoints_and_parses_cache_metrics() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let raw_req = read_full_http_request(&mut sock).await;
            assert!(raw_req.contains("x-api-key: sk-ant-test\r\n"));
            let body_str = raw_req.split("\r\n\r\n").nth(1).unwrap_or("{}");
            let body_json: Value = serde_json::from_str(body_str).expect("json body");
            assert_eq!(body_json["system"][0]["cache_control"]["type"], "ephemeral");
            assert_eq!(body_json["tools"][0]["cache_control"]["type"], "ephemeral");
            assert_eq!(
                body_json["messages"][0]["content"][0]["cache_control"]["type"],
                "ephemeral"
            );

            let sse_body = concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":50,\"cache_creation_input_tokens\":100,\"cache_read_input_tokens\":850,\"output_tokens\":1}}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Done!\"}}\n\n",
                "event: message_delta\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":12}}\n\n",
                "event: message_stop\n",
                "data: {\"type\":\"message_stop\"}\n\n"
            );
            let resp =
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{sse_body}");
            sock.write_all(resp.as_bytes()).await.unwrap();
        });

        let input = NativeDriverInput {
            provider_id: "anthropic".to_owned(),
            driver: ProviderDriverKind::AnthropicMessages,
            endpoint: format!("http://{addr}"),
            api_key: Some("sk-ant-test".to_owned()),
            proxy: None,
            force_direct_proxy: true,
            model: "claude-sonnet-4-6".to_owned(),
            effort: None,
            max_output_tokens: Some(4096),
            system_blocks: vec!["System prompt".to_owned()],
            messages: vec![CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text("Hello".to_owned())],
            }],
            tools: vec![NativeWireTool {
                name: "Bash".to_owned(),
                description: Some("Run command".to_owned()),
                input_schema: json!({"type":"object","properties":{"command":{"type":"string"}}}),
                is_custom_freeform: false,
                custom_format: None,
            }],
            enable_native_web_search: false,
            thought_signatures: std::collections::HashMap::new(),
            prompt_cache_key: None,
            align_cache_breakpoints: true,
            connect_timeout: Duration::from_secs(5),
            total_timeout: Duration::from_secs(10),
        };

        let (tx, mut rx) = mpsc::channel(32);
        let metrics = execute_driver_sse_step(&DefaultHttpTransport, &input, &tx)
            .await
            .expect("step ok");
        drop(tx);
        assert_eq!(metrics.cache_breakpoints_aligned, 3);

        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev.expect("event ok"));
        }
        assert!(events.contains(&NativeStreamEvent::TextDelta("Done!".to_owned())));
        assert!(events.iter().any(|e| matches!(
            e,
            NativeStreamEvent::Usage(u)
                if u.input_tokens == 50
                    && u.cached_read_tokens == 850
                    && u.cached_write_tokens == 100
                    && u.total_prompt_tokens() == 1000
                    && u.output_tokens == 12
        )));
    }

    #[tokio::test]
    async fn gemini_driver_sends_header_key_and_rejects_429_without_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let raw_req = read_full_http_request(&mut sock).await;
            assert!(raw_req.starts_with(
                "POST /v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse HTTP/1.1\r\n"
            ));
            assert!(raw_req.contains("x-goog-api-key: gem-secret\r\n"));
            assert!(!raw_req.lines().next().unwrap_or("").contains("gem-secret"));

            let resp = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 3\r\nContent-Type: application/json\r\n\r\n{\"error\":{\"message\":\"Resource exhausted\"}}";
            sock.write_all(resp.as_bytes()).await.unwrap();
        });

        let input = NativeDriverInput {
            provider_id: "gemini".to_owned(),
            driver: ProviderDriverKind::GoogleGemini,
            endpoint: format!("http://{addr}"),
            api_key: Some("gem-secret".to_owned()),
            proxy: None,
            force_direct_proxy: true,
            model: "gemini-2.5-pro".to_owned(),
            effort: None,
            max_output_tokens: None,
            system_blocks: Vec::new(),
            messages: vec![CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text("Ping".to_owned())],
            }],
            tools: Vec::new(),
            enable_native_web_search: false,
            thought_signatures: std::collections::HashMap::new(),
            prompt_cache_key: None,
            align_cache_breakpoints: false,
            connect_timeout: Duration::from_secs(5),
            total_timeout: Duration::from_secs(10),
        };

        let (tx, _rx) = mpsc::channel(8);
        let err = execute_driver_sse_step(&DefaultHttpTransport, &input, &tx)
            .await
            .expect_err("must fail with 429");
        assert!(!err.is_retryable());
        match err {
            PlatformApiError::RateLimited {
                status,
                retry_after_ms,
                message,
            } => {
                assert_eq!(status, 429);
                assert_eq!(retry_after_ms, Some(3_000));
                assert_eq!(message, "Resource exhausted");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn openai_request_serializes_custom_tool_call_output_for_custom_tools() {
        let input = NativeDriverInput {
            provider_id: "openai".to_owned(),
            driver: ProviderDriverKind::OpenAiResponses,
            endpoint: "http://127.0.0.1:9999/v1".to_owned(),
            api_key: None,
            proxy: None,
            force_direct_proxy: true,
            model: "gpt-5.4".to_owned(),
            effort: None,
            max_output_tokens: None,
            system_blocks: Vec::new(),
            messages: vec![
                CanonicalMessage {
                    role: CanonicalRole::User,
                    parts: vec![CanonicalPart::Text("Run script and read file".to_owned())],
                },
                CanonicalMessage {
                    role: CanonicalRole::Assistant,
                    parts: vec![
                        CanonicalPart::ToolCall {
                            call_id: "call_exec_1".to_owned(),
                            name: "exec".to_owned(),
                            arguments: "text(1 + 1)".to_owned(),
                            is_custom: true,
                        },
                        CanonicalPart::ToolCall {
                            call_id: "call_fn_2".to_owned(),
                            name: "read_file".to_owned(),
                            arguments: "{\"path\":\"Cargo.toml\"}".to_owned(),
                            is_custom: false,
                        },
                    ],
                },
                CanonicalMessage {
                    role: CanonicalRole::User,
                    parts: vec![
                        CanonicalPart::ToolResult {
                            call_id: "call_exec_1".to_owned(),
                            output: "2".to_owned(),
                            is_error: false,
                        },
                        CanonicalPart::ToolResult {
                            call_id: "call_fn_2".to_owned(),
                            output: "[workspace]".to_owned(),
                            is_error: false,
                        },
                    ],
                },
            ],
            tools: vec![
                NativeWireTool {
                    name: "exec".to_owned(),
                    description: Some("Exec".to_owned()),
                    input_schema: json!({"type":"object"}),
                    is_custom_freeform: true,
                    custom_format: None,
                },
                NativeWireTool {
                    name: "read_file".to_owned(),
                    description: Some("Read".to_owned()),
                    input_schema: json!({"type":"object"}),
                    is_custom_freeform: false,
                    custom_format: None,
                },
            ],
            enable_native_web_search: false,
            thought_signatures: std::collections::HashMap::new(),
            prompt_cache_key: None,
            align_cache_breakpoints: false,
            connect_timeout: Duration::from_secs(5),
            total_timeout: Duration::from_secs(10),
        };

        let (spec, _) = crate::build_driver_http_request(&input).expect("build request");
        let body: Value =
            serde_json::from_slice(spec.body.as_deref().expect("body")).expect("json");
        let arr = body["input"].as_array().expect("input array");
        let exec_out = arr
            .iter()
            .find(|i| i["call_id"] == "call_exec_1" && i["output"].is_string())
            .expect("custom output item");
        assert_eq!(exec_out["type"], "custom_tool_call_output");
        let fn_out = arr
            .iter()
            .find(|i| i["call_id"] == "call_fn_2" && i["output"].is_string())
            .expect("function output item");
        assert_eq!(fn_out["type"], "function_call_output");
    }

    #[tokio::test]
    async fn premature_sse_eof_without_terminal_event_fails_and_drops_partial_tool_call() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let _ = read_full_http_request(&mut sock).await;
            let truncated_sse = concat!(
                "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_1\",\"call_id\":\"call_trunc\",\"type\":\"function_call\",\"name\":\"rm_something\"}}\n\n",
                "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"cmd\\\":\\\"partial\"}\n\n"
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{truncated_sse}"
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        });

        let input = NativeDriverInput {
            provider_id: "openai".to_owned(),
            driver: ProviderDriverKind::OpenAiResponses,
            endpoint: format!("http://{addr}/v1"),
            api_key: None,
            proxy: None,
            force_direct_proxy: true,
            model: "gpt-5.4".to_owned(),
            effort: None,
            max_output_tokens: None,
            system_blocks: Vec::new(),
            messages: vec![CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text("Hi".to_owned())],
            }],
            tools: Vec::new(),
            enable_native_web_search: false,
            thought_signatures: std::collections::HashMap::new(),
            prompt_cache_key: None,
            align_cache_breakpoints: false,
            connect_timeout: Duration::from_secs(2),
            total_timeout: Duration::from_secs(5),
        };

        let (tx, mut rx) = mpsc::channel(16);
        let res = execute_driver_sse_step(&DefaultHttpTransport, &input, &tx).await;
        drop(tx);
        assert!(matches!(res, Err(PlatformApiError::Protocol { .. })));
        while let Some(ev) = rx.recv().await {
            assert!(
                !matches!(
                    ev,
                    Ok(NativeStreamEvent::ToolCall { .. } | NativeStreamEvent::Completed { .. })
                ),
                "must never emit unfinished tool call or synthetic Completed on premature EOF"
            );
        }
    }

    #[tokio::test]
    async fn oversized_chunk_header_is_rejected_without_allocation_and_stalled_body_times_out() {
        // 1. Oversized chunk header (`ffffffff` = 4 GiB) must fail cleanly with Protocol error
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let _ = read_full_http_request(&mut sock).await;
            let resp = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/event-stream\r\n\r\nffffffff\r\n";
            sock.write_all(resp.as_bytes()).await.unwrap();
        });

        let mut input = NativeDriverInput {
            provider_id: "openai".to_owned(),
            driver: ProviderDriverKind::OpenAiResponses,
            endpoint: format!("http://{addr}/v1"),
            api_key: None,
            proxy: None,
            force_direct_proxy: true,
            model: "gpt-5.4".to_owned(),
            effort: None,
            max_output_tokens: None,
            system_blocks: Vec::new(),
            messages: vec![CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text("Hi".to_owned())],
            }],
            tools: Vec::new(),
            enable_native_web_search: false,
            thought_signatures: std::collections::HashMap::new(),
            prompt_cache_key: None,
            align_cache_breakpoints: false,
            connect_timeout: Duration::from_secs(2),
            total_timeout: Duration::from_millis(180),
        };

        let (tx, _rx) = mpsc::channel(16);
        let res = execute_driver_sse_step(&DefaultHttpTransport, &input, &tx).await;
        assert!(matches!(res, Err(PlatformApiError::Protocol { .. })));

        // 2. Stalled SSE body after 200 OK headers must enforce total_timeout
        let stall_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let stall_addr = stall_listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut sock, _) = stall_listener.accept().await.expect("accept");
            let _ = read_full_http_request(&mut sock).await;
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        input.endpoint = format!("http://{stall_addr}/v1");
        let (tx2, _rx2) = mpsc::channel(16);
        let stall_res = execute_driver_sse_step(&DefaultHttpTransport, &input, &tx2).await;
        assert!(
            matches!(stall_res, Err(PlatformApiError::Timeout { .. })),
            "expected Timeout on stalled stream body, got {stall_res:?}"
        );

        // 3. Proxy `HTTP/1.1 200 Connection established` ahead of upstream 429 must surface 429
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let proxy_addr = proxy_listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut sock, _) = proxy_listener.accept().await.expect("accept");
            let _ = read_full_http_request(&mut sock).await;
            let raw = concat!(
                "HTTP/1.1 200 Connection established\r\nProxy-Agent: test\r\n\r\n",
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 2\r\nContent-Type: application/json\r\n\r\n{\"error\":{\"message\":\"Quota hit\"}}"
            );
            sock.write_all(raw.as_bytes()).await.unwrap();
        });
        input.endpoint = format!("http://{proxy_addr}/v1");
        input.total_timeout = Duration::from_secs(3);
        let (tx3, _rx3) = mpsc::channel(16);
        let proxy_res = execute_driver_sse_step(&DefaultHttpTransport, &input, &tx3).await;
        assert!(
            matches!(
                proxy_res,
                Err(PlatformApiError::RateLimited { status: 429, .. })
            ),
            "expected upstream 429 past CONNECT headers, got {proxy_res:?}"
        );
    }

    #[test]
    fn sse_decoder_rejects_accumulated_multiline_frame_overflow_and_codecs_preserve_web_search_and_gemini_signatures()
     {
        // 1. Accumulated `data:` lines without blank separator must fail past MAX_SSE_FRAME_BYTES
        let mut dec = crate::sse::SseDecoder::new();
        let one_mib = format!("data: {}", "x".repeat(1024 * 1024));
        let mut overflowed = false;
        for _ in 0..10 {
            if dec.try_push_line(&one_mib).is_err() {
                overflowed = true;
                break;
            }
        }
        assert!(
            overflowed,
            "SseDecoder must reject multi-line frames exceeding MAX_SSE_FRAME_BYTES"
        );

        // 2. Native Anthropic request must forward `web_search_20250305` when `enable_native_web_search` is true
        let anth_in = NativeDriverInput {
            provider_id: "anthropic".to_owned(),
            driver: ProviderDriverKind::AnthropicMessages,
            endpoint: "https://api.anthropic.com".to_owned(),
            api_key: Some("sk-ant-test".to_owned()),
            proxy: None,
            force_direct_proxy: true,
            model: "claude-sonnet-4-6".to_owned(),
            effort: None,
            max_output_tokens: Some(2048),
            system_blocks: Vec::new(),
            messages: vec![CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text("Search latest Rust release".to_owned())],
            }],
            tools: Vec::new(),
            enable_native_web_search: true,
            thought_signatures: std::collections::HashMap::new(),
            prompt_cache_key: None,
            align_cache_breakpoints: true,
            connect_timeout: Duration::from_secs(5),
            total_timeout: Duration::from_secs(10),
        };
        let (anth_spec, _) = crate::build_driver_http_request(&anth_in).expect("anthropic build");
        let anth_json: Value =
            serde_json::from_slice(anth_spec.body.as_deref().expect("body")).expect("json");
        let anth_tools = anth_json["tools"].as_array().expect("tools array");
        assert!(
            anth_tools
                .iter()
                .any(|t| t["type"] == "web_search_20250305" && t["name"] == "web_search"),
            "expected web_search_20250305 in Anthropic tools: {anth_tools:?}"
        );

        // 3. Gemini request must restore `thoughtSignature` alongside `functionCall`
        let mut sigs = std::collections::HashMap::new();
        sigs.insert(
            "call_gem_99_1".to_owned(),
            "sig_gemini3_opaque_xyz".to_owned(),
        );
        let gem_in = NativeDriverInput {
            provider_id: "gemini".to_owned(),
            driver: ProviderDriverKind::GoogleGemini,
            endpoint: "https://generativelanguage.googleapis.com".to_owned(),
            api_key: Some("AIza-test".to_owned()),
            proxy: None,
            force_direct_proxy: true,
            model: "gemini-3-pro".to_owned(),
            effort: None,
            max_output_tokens: None,
            system_blocks: Vec::new(),
            messages: vec![
                CanonicalMessage {
                    role: CanonicalRole::User,
                    parts: vec![CanonicalPart::Text("Run check".to_owned())],
                },
                CanonicalMessage {
                    role: CanonicalRole::Assistant,
                    parts: vec![CanonicalPart::ToolCall {
                        call_id: "call_gem_99_1".to_owned(),
                        name: "read_file".to_owned(),
                        arguments: "{\"path\":\"Cargo.toml\"}".to_owned(),
                        is_custom: false,
                    }],
                },
                CanonicalMessage {
                    role: CanonicalRole::User,
                    parts: vec![CanonicalPart::ToolResult {
                        call_id: "call_gem_99_1".to_owned(),
                        output: "[workspace]".to_owned(),
                        is_error: false,
                    }],
                },
            ],
            tools: Vec::new(),
            enable_native_web_search: false,
            thought_signatures: sigs,
            prompt_cache_key: None,
            align_cache_breakpoints: false,
            connect_timeout: Duration::from_secs(5),
            total_timeout: Duration::from_secs(10),
        };
        let (gem_spec, _) = crate::build_driver_http_request(&gem_in).expect("gemini build");
        let gem_json: Value =
            serde_json::from_slice(gem_spec.body.as_deref().expect("body")).expect("json");
        let model_part = &gem_json["contents"][1]["parts"][0];
        assert_eq!(model_part["functionCall"]["name"], "read_file");
        assert_eq!(model_part["thoughtSignature"], "sig_gemini3_opaque_xyz");
    }
}
