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
        StatefulDialect, StatefulStreamEvent, StatefulTurnInput, run_turn_on_peer,
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
            cwd: PathBuf::from("."),
            mcp_servers: Vec::new(),
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
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StatefulStreamEvent::Usage(u) if u.cached_read_tokens == 80))
        );
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
            cwd: PathBuf::from("."),
            mcp_servers: Vec::new(),
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
}
