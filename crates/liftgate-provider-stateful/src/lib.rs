//! Shared stateful JSON-RPC 2.0 stdio process supervision and dialect adapters.

pub mod adapters;
pub mod job;
pub mod stdio_rpc;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::mpsc;

    use crate::adapters::{
        DynamicToolDisposition, StatefulClientToolSpec, StatefulDialect, StatefulStreamEvent,
        StatefulTurnInput, run_turn_on_peer, run_turn_on_peer_with_handler,
    };
    use crate::stdio_rpc::StdioRpcPeer;

    #[tokio::test]
    async fn codex_app_server_dialect_streams_reasoning_text_and_usage() {
        let (client_read, mut server_write) = tokio::io::duplex(16 * 1024);
        let (server_read, client_write) = tokio::io::duplex(16 * 1024);

        tokio::spawn(async move {
            let mut lines = BufReader::new(server_read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let req: Value = serde_json::from_str(&line).expect("valid rpc");
                let method = req.get("method").and_then(Value::as_str).unwrap_or("");
                let id = req.get("id").cloned();
                match method {
                    "initialize" => {
                        let resp =
                            json!({"jsonrpc": "2.0", "id": id, "result": {"userAgent": "codex"}});
                        server_write
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                    "thread/start" => {
                        let resp = json!({"jsonrpc": "2.0", "id": id, "result": {"thread": {"id": "thr_1"}}});
                        server_write
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                    "turn/start" => {
                        let resp =
                            json!({"jsonrpc": "2.0", "id": id, "result": {"turn": {"id": "t_1"}}});
                        server_write
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .unwrap();
                        for note in [
                            json!({"jsonrpc": "2.0", "method": "item/reasoning/summaryTextDelta", "params": {"delta": "thinking..."}}),
                            json!({"jsonrpc": "2.0", "method": "item/agentMessage/delta", "params": {"delta": "Hello from app-server"}}),
                            json!({"jsonrpc": "2.0", "method": "turn/tokenUsage/updated", "params": {"tokenUsage": {"last": {"inputTokens": 100, "outputTokens": 25, "cachedInputTokens": 80}}}}),
                            json!({"jsonrpc": "2.0", "method": "turn/completed", "params": {"turn": {"status": "completed"}}}),
                        ] {
                            server_write
                                .write_all(format!("{note}\n").as_bytes())
                                .await
                                .unwrap();
                        }
                        break;
                    }
                    _ => {}
                }
            }
        });

        let peer = StdioRpcPeer::from_io(client_read, client_write, 1024 * 1024, None);
        let (tx, mut rx) = mpsc::channel(32);
        let input = StatefulTurnInput {
            dialect: StatefulDialect::CodexAppServer,
            model: "gpt-5.3-codex".to_owned(),
            effort: None,
            system_instructions: Some("Be concise.".to_owned()),
            prompt_text: "hi".to_owned(),
            images: Vec::new(),
            cwd: PathBuf::from("."),
            mcp_servers: Vec::new(),
            dynamic_tools: Vec::new(),
        };
        run_turn_on_peer(&peer, &input, &tx).await.expect("turn ok");
        drop(tx);

        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev.expect("ok event"));
        }
        assert!(events.contains(&StatefulStreamEvent::ReasoningDelta(
            "thinking...".to_owned()
        )));
        assert!(events.contains(&StatefulStreamEvent::TextDelta(
            "Hello from app-server".to_owned()
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            StatefulStreamEvent::Usage(u)
                if u.input_tokens == 20
                    && u.cached_read_tokens == 80
                    && u.total_prompt_tokens() == 100
        )));
        assert!(events.contains(&StatefulStreamEvent::Completed {
            stop_reason: "end_turn".to_owned()
        }));
    }

    #[tokio::test]
    async fn standard_acp_dialect_streams_updates_and_usage() {
        let (client_read, mut server_write) = tokio::io::duplex(16 * 1024);
        let (server_read, client_write) = tokio::io::duplex(16 * 1024);

        tokio::spawn(async move {
            let mut lines = BufReader::new(server_read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let req: Value = serde_json::from_str(&line).expect("valid rpc");
                let method = req.get("method").and_then(Value::as_str).unwrap_or("");
                let id = req.get("id").cloned();
                match method {
                    "initialize" => {
                        let resp =
                            json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": 1}});
                        server_write
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                    "session/new" => {
                        let resp =
                            json!({"jsonrpc": "2.0", "id": id, "result": {"sessionId": "grok_s1"}});
                        server_write
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                    "session/set_model" => {
                        let resp = json!({"jsonrpc": "2.0", "id": id, "result": {}});
                        server_write
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                    "session/prompt" => {
                        let note = json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": "grok_s1",
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": { "type": "text", "text": "Hello from grok acp" }
                                }
                            }
                        });
                        server_write
                            .write_all(format!("{note}\n").as_bytes())
                            .await
                            .unwrap();
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "stopReason": "end_turn",
                                "usage": { "inputTokens": 40, "outputTokens": 10 }
                            }
                        });
                        server_write
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .unwrap();
                        break;
                    }
                    _ => {}
                }
            }
        });

        let peer = StdioRpcPeer::from_io(client_read, client_write, 1024 * 1024, None);
        let (tx, mut rx) = mpsc::channel(32);
        let input = StatefulTurnInput {
            dialect: StatefulDialect::GrokAcp,
            model: "grok-4".to_owned(),
            effort: None,
            system_instructions: None,
            prompt_text: "ping".to_owned(),
            images: Vec::new(),
            cwd: PathBuf::from("."),
            mcp_servers: Vec::new(),
            dynamic_tools: Vec::new(),
        };
        run_turn_on_peer(&peer, &input, &tx).await.expect("turn ok");
        drop(tx);

        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev.expect("ok event"));
        }
        assert!(events.contains(&StatefulStreamEvent::TextDelta(
            "Hello from grok acp".to_owned()
        )));
        assert!(events.contains(&StatefulStreamEvent::Completed {
            stop_reason: "end_turn".to_owned()
        }));
    }

    #[tokio::test]
    async fn codex_app_server_forwards_dynamic_tools_handles_zero_rtt_and_suspends_client_calls() {
        let (client_read, mut server_write) = tokio::io::duplex(16 * 1024);
        let (server_read, client_write) = tokio::io::duplex(16 * 1024);

        tokio::spawn(async move {
            let mut lines = BufReader::new(server_read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let req: Value = serde_json::from_str(&line).expect("valid rpc");
                let method = req.get("method").and_then(Value::as_str).unwrap_or("");
                let id = req.get("id").cloned();
                match method {
                    "initialize" => {
                        assert_eq!(req["params"]["capabilities"]["experimentalApi"], true);
                        let resp =
                            json!({"jsonrpc": "2.0", "id": id, "result": {"userAgent": "codex"}});
                        server_write
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                    "thread/start" => {
                        assert_eq!(
                            req["params"]["dynamicTools"][0]["name"],
                            "search_client_tools"
                        );
                        assert_eq!(req["params"]["dynamicTools"][1]["name"], "read_file");
                        let resp = json!({"jsonrpc": "2.0", "id": id, "result": {"thread": {"id": "thr_tools"}}});
                        server_write
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                    "turn/start" => {
                        let resp = json!({"jsonrpc": "2.0", "id": id, "result": {"turn": {"id": "t_tools"}}});
                        server_write
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .unwrap();
                        let call1 = json!({
                            "jsonrpc": "2.0",
                            "id": 9001,
                            "method": "item/tool/call",
                            "params": {
                                "callId": "call_s1",
                                "threadId": "thr_tools",
                                "turnId": "t_tools",
                                "tool": "search_client_tools",
                                "arguments": {"query": "read"}
                            }
                        });
                        server_write
                            .write_all(format!("{call1}\n").as_bytes())
                            .await
                            .unwrap();
                        let reply_line = lines.next_line().await.unwrap().unwrap();
                        let reply: Value = serde_json::from_str(&reply_line).unwrap();
                        assert_eq!(reply["id"], 9001);
                        assert_eq!(reply["result"]["success"], true);
                        assert!(
                            reply["result"]["contentItems"][0]["text"]
                                .as_str()
                                .unwrap_or("")
                                .contains("read_file")
                        );

                        let call2 = json!({
                            "jsonrpc": "2.0",
                            "id": 9002,
                            "method": "item/tool/call",
                            "params": {
                                "callId": "call_rf_1",
                                "threadId": "thr_tools",
                                "turnId": "t_tools",
                                "tool": "read_file",
                                "arguments": {"path": "Cargo.toml"}
                            }
                        });
                        server_write
                            .write_all(format!("{call2}\n").as_bytes())
                            .await
                            .unwrap();
                        break;
                    }
                    _ => {}
                }
            }
        });

        let peer = StdioRpcPeer::from_io(client_read, client_write, 1024 * 1024, None);
        let (tx, mut rx) = mpsc::channel(32);
        let input = StatefulTurnInput {
            dialect: StatefulDialect::CodexAppServer,
            model: "gpt-5.4".to_owned(),
            effort: None,
            system_instructions: None,
            prompt_text: "read Cargo.toml".to_owned(),
            images: Vec::new(),
            cwd: PathBuf::from("."),
            mcp_servers: Vec::new(),
            dynamic_tools: vec![
                StatefulClientToolSpec {
                    name: "search_client_tools".to_owned(),
                    description: "Search".to_owned(),
                    input_schema: json!({"type":"object"}),
                    is_custom_freeform: false,
                },
                StatefulClientToolSpec {
                    name: "read_file".to_owned(),
                    description: "Read".to_owned(),
                    input_schema: json!({"type":"object"}),
                    is_custom_freeform: false,
                },
            ],
        };

        run_turn_on_peer_with_handler(&peer, &input, &tx, |tool, _args| async move {
            if tool == "search_client_tools" {
                DynamicToolDisposition::LocalReply {
                    text: "found read_file".to_owned(),
                    is_error: false,
                }
            } else {
                DynamicToolDisposition::SuspendToClient
            }
        })
        .await
        .expect("turn ok");
        drop(tx);

        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev.expect("ok event"));
        }
        assert!(events.contains(&StatefulStreamEvent::ToolCall {
            id: "call_rf_1".to_owned(),
            name: "read_file".to_owned(),
            arguments_json: "{\"path\":\"Cargo.toml\"}".to_owned(),
            is_custom: false,
        }));
        assert!(events.contains(&StatefulStreamEvent::Completed {
            stop_reason: "tool_use".to_owned()
        }));
    }

    #[tokio::test]
    async fn grok_acp_set_model_failure_propagates_without_running_prompt() {
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        let (client_read, client_write) = tokio::io::split(client_stream);
        let (server_read, mut server_write) = tokio::io::split(server_stream);
        let saw_prompt = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saw_prompt_srv = std::sync::Arc::clone(&saw_prompt);

        tokio::spawn(async move {
            let mut lines = BufReader::new(server_read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let req: Value = serde_json::from_str(&line).expect("valid rpc");
                let id = req.get("id").cloned().unwrap_or(Value::Null);
                match req.get("method").and_then(Value::as_str).unwrap_or("") {
                    "initialize" => {
                        let resp =
                            json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": 1}});
                        server_write
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                    "session/new" => {
                        let resp =
                            json!({"jsonrpc": "2.0", "id": id, "result": {"sessionId": "grok_rl"}});
                        server_write
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                    "session/set_model" => {
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": 429, "message": "429 Too Many Requests: model capacity exhausted" }
                        });
                        server_write
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .unwrap();
                        break;
                    }
                    "session/prompt" => {
                        saw_prompt_srv.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                    _ => {}
                }
            }
        });

        let peer = StdioRpcPeer::from_io(client_read, client_write, 1024 * 1024, None);
        let (tx, _rx) = mpsc::channel(16);
        let res = run_turn_on_peer(
            &peer,
            &StatefulTurnInput {
                dialect: StatefulDialect::GrokAcp,
                model: "grok-4.20".to_owned(),
                effort: None,
                cwd: PathBuf::from("."),
                system_instructions: None,
                prompt_text: "Hello".to_owned(),
                images: Vec::new(),
                mcp_servers: Vec::new(),
                dynamic_tools: Vec::new(),
            },
            &tx,
        )
        .await;
        assert!(
            matches!(
                res,
                Err(crate::stdio_rpc::StatefulRpcError::RateLimited { .. })
            ),
            "expected RateLimited from session/set_model, got {res:?}"
        );
        assert!(
            !saw_prompt.load(std::sync::atomic::Ordering::SeqCst),
            "must never dispatch session/prompt after session/set_model fails"
        );
    }

    #[tokio::test]
    async fn stateful_dialects_serialize_multimodal_images() {
        // 1. Codex app-server image serialization (`data:<mime>;base64,<data>`)
        let (c_read, mut s_write) = tokio::io::duplex(16 * 1024);
        let (s_read, c_write) = tokio::io::duplex(16 * 1024);
        let seen_codex_input = std::sync::Arc::new(std::sync::Mutex::new(Value::Null));
        let seen_codex_clone = std::sync::Arc::clone(&seen_codex_input);
        tokio::spawn(async move {
            let mut lines = BufReader::new(s_read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let req: Value = serde_json::from_str(&line).unwrap();
                let id = req.get("id").cloned();
                match req.get("method").and_then(Value::as_str).unwrap_or("") {
                    "initialize" => {
                        let r = json!({"jsonrpc":"2.0","id":id,"result":{"userAgent":"codex"}});
                        s_write
                            .write_all(format!("{r}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                    "thread/start" => {
                        let r = json!({"jsonrpc":"2.0","id":id,"result":{"thread":{"id":"t1"}}});
                        s_write
                            .write_all(format!("{r}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                    "turn/start" => {
                        *seen_codex_clone.lock().unwrap() = req["params"]["input"].clone();
                        let r = json!({"jsonrpc":"2.0","id":id,"result":{"turn":{"id":"u1"}}});
                        s_write
                            .write_all(format!("{r}\n").as_bytes())
                            .await
                            .unwrap();
                        let done = json!({"jsonrpc":"2.0","method":"turn/completed","params":{"turn":{"status":"completed"}}});
                        s_write
                            .write_all(format!("{done}\n").as_bytes())
                            .await
                            .unwrap();
                        break;
                    }
                    _ => {}
                }
            }
        });
        let peer = StdioRpcPeer::from_io(c_read, c_write, 1024 * 1024, None);
        let (tx, _rx) = mpsc::channel(16);
        run_turn_on_peer(
            &peer,
            &StatefulTurnInput {
                dialect: StatefulDialect::CodexAppServer,
                model: "gpt-5.4".to_owned(),
                effort: None,
                system_instructions: None,
                prompt_text: "Describe image".to_owned(),
                images: vec![crate::adapters::StatefulInputImage {
                    media_type: "image/png".to_owned(),
                    data: "iVBORw0KGgo=".to_owned(),
                }],
                cwd: PathBuf::from("."),
                mcp_servers: Vec::new(),
                dynamic_tools: Vec::new(),
            },
            &tx,
        )
        .await
        .expect("codex multimodal turn");
        let codex_items = seen_codex_input.lock().unwrap().clone();
        assert_eq!(codex_items[1]["type"], "image");
        assert_eq!(codex_items[1]["url"], "data:image/png;base64,iVBORw0KGgo=");

        // 2. Grok ACP image serialization (`{"type":"image","data":...,"mimeType":...}`)
        let (c_read2, mut s_write2) = tokio::io::duplex(16 * 1024);
        let (s_read2, c_write2) = tokio::io::duplex(16 * 1024);
        let seen_acp_prompt = std::sync::Arc::new(std::sync::Mutex::new(Value::Null));
        let seen_acp_clone = std::sync::Arc::clone(&seen_acp_prompt);
        tokio::spawn(async move {
            let mut lines = BufReader::new(s_read2).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let req: Value = serde_json::from_str(&line).unwrap();
                let id = req.get("id").cloned();
                match req.get("method").and_then(Value::as_str).unwrap_or("") {
                    "initialize" => {
                        let r = json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1}});
                        s_write2
                            .write_all(format!("{r}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                    "session/new" => {
                        let r = json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"s1"}});
                        s_write2
                            .write_all(format!("{r}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                    "session/set_model" => {
                        let r = json!({"jsonrpc":"2.0","id":id,"result":{}});
                        s_write2
                            .write_all(format!("{r}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                    "session/prompt" => {
                        *seen_acp_clone.lock().unwrap() = req["params"]["prompt"].clone();
                        let r = json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}});
                        s_write2
                            .write_all(format!("{r}\n").as_bytes())
                            .await
                            .unwrap();
                        break;
                    }
                    _ => {}
                }
            }
        });
        let peer2 = StdioRpcPeer::from_io(c_read2, c_write2, 1024 * 1024, None);
        let (tx2, _rx2) = mpsc::channel(16);
        run_turn_on_peer(
            &peer2,
            &StatefulTurnInput {
                dialect: StatefulDialect::GrokAcp,
                model: "grok-4".to_owned(),
                effort: None,
                system_instructions: None,
                prompt_text: "Describe image".to_owned(),
                images: vec![crate::adapters::StatefulInputImage {
                    media_type: "image/png".to_owned(),
                    data: "iVBORw0KGgo=".to_owned(),
                }],
                cwd: PathBuf::from("."),
                mcp_servers: Vec::new(),
                dynamic_tools: Vec::new(),
            },
            &tx2,
        )
        .await
        .expect("grok acp multimodal turn");
        let acp_blocks = seen_acp_prompt.lock().unwrap().clone();
        assert_eq!(acp_blocks[1]["type"], "image");
        assert_eq!(acp_blocks[1]["mimeType"], "image/png");
        assert_eq!(acp_blocks[1]["data"], "iVBORw0KGgo=");
    }
}
