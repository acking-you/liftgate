use super::prompt_plan::{render_smart_bounded_active_slice, total_text_block_bytes};
use super::*;
use crate::acp::AcpPromptBlock;
use crate::canonical::{CanonicalToolKind, CanonicalToolSpec};

#[test]
fn claude_code_changing_cch_and_total_tokens_preserves_prefix_hash_reuse() {
    let model = ModelId::try_new("gemini-4-carbon-high").expect("valid model");
    let web_cfg = WebToolBridgeConfig::default();
    let tools = vec![CanonicalToolSpec {
        name: "WebSearch".to_owned(),
        description: Some("Search the web".to_owned()),
        kind: CanonicalToolKind::Function {
            parameters_json:
                "{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\"}}}".to_owned(),
        },
    }];

    let sys_t1 = "x-anthropic-billing-header: cc_version=2.1.296.fd5; cc_entrypoint=cli; cch=8a3f1;\n\
                      attribution: cch=8a3f1; mode=cli\n\
                      You are an agent.\n<total_tokens>15000000 tokens left</total_tokens>";
    let msg_u1_t1 = CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![
                CanonicalPart::Text(
                    "<system-reminder>\n<total_tokens>15000000 tokens left</total_tokens>\n</system-reminder>".to_owned(),
                ),
                CanonicalPart::Text("Search Rust 2024".to_owned()),
            ],
        };
    let assistant_t1 = CanonicalMessage {
        role: CanonicalRole::Assistant,
        parts: vec![CanonicalPart::ToolCall {
            call_id: "call_1".to_owned(),
            name: "WebSearch".to_owned(),
            arguments: "{\"query\":\"Rust 2024\"}".to_owned(),
            is_custom: false,
        }],
    };

    let post_hash_t1 = hash_conversation_with_reply(
        Some(sys_t1),
        &tools,
        web_cfg,
        &model,
        std::slice::from_ref(&msg_u1_t1),
        &assistant_t1,
    );

    let sys_t2 = "x-anthropic-billing-header: cc_version=2.1.296.fd5; cc_entrypoint=cli; cch=99e0c;\n\
                      attribution: cch=99e0c; mode=cli\n\
                      You are an agent.\n<total_tokens>14999820 tokens left</total_tokens>";
    let msg_u1_t2 = CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![
                CanonicalPart::Text(
                    "<system-reminder>\n<total_tokens>14999820 tokens left</total_tokens>\n</system-reminder>".to_owned(),
                ),
                CanonicalPart::Text("Search Rust 2024".to_owned()),
            ],
        };
    let msg_u2_t2 = CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::ToolResult {
                call_id: "toolu_1".to_owned(),
                output: "Results...\n<system-reminder>\n<total_tokens>14999820 tokens left</total_tokens>\n</system-reminder>".to_owned(),
                is_error: false,
            }],
        };

    let plan_t2 = compute_routing_plan(
        Some(sys_t2),
        &tools,
        web_cfg,
        &model,
        &[msg_u1_t2, assistant_t1, msg_u2_t2],
    );
    assert_eq!(plan_t2.prefix_lookup, Some((post_hash_t1, 2)));
}

#[test]
fn compacts_massive_client_tool_catalog_without_losing_schema_fields() {
    let verbose_desc = format!(
        "Executes bash command.\n<example>\n{}\n</example>\n<commentary>\nverbose\n</commentary>",
        "x".repeat(4000)
    );
    let verbose_schema = format!(
        "{{\"$schema\":\"https://json-schema.org/draft/2020-12/schema\",\"type\":\"object\",\"properties\":{{\"command\":{{\"type\":\"string\",\"description\":\"{}\"}}}},\"required\":[\"command\"],\"additionalProperties\":false}}",
        "y".repeat(500)
    );
    let tools = vec![CanonicalToolSpec {
        name: "Bash".to_owned(),
        description: Some(verbose_desc),
        kind: CanonicalToolKind::Function {
            parameters_json: verbose_schema,
        },
    }];

    let out = build_session_system_append(
        Some("Helpful assistant"),
        &tools,
        WebToolBridgeConfig::default(),
        "1a2b",
    );
    assert!(
        out.len() < 2900,
        "unexpectedly large append len={}",
        out.len()
    );
    assert!(!out.contains("$schema"));
    assert!(!out.contains("additionalProperties"));
    assert!(out.contains("\"required\":[\"command\"]"));
    assert!(out.contains("\"command\""));
}

#[test]
fn cold_history_replay_bounds_megabyte_transcript_and_preserves_late_user_instructions() {
    let mut messages = Vec::new();

    // 1. Initial developer + AGENTS.md + Task 1 user prompt
    messages.push(CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![
            CanonicalPart::Text(
                "<developer_instructions>\nFollow workspace Rust rules.\n</developer_instructions>"
                    .to_owned(),
            ),
            CanonicalPart::Text("Task 1: build the `liftgate` management CLI.".to_owned()),
        ],
    });

    // 2. 120 bulky tool round-trips (~1.8 MB total) from Task 1
    for step in 0..120 {
        messages.push(CanonicalMessage {
            role: CanonicalRole::Assistant,
            parts: vec![CanonicalPart::ToolCall {
                call_id: format!("call_old_{step}"),
                name: "exec_command".to_owned(),
                arguments: format!("{{\"cmd\":\"Get-Content file_{step}.rs\"}}"),
                is_custom: false,
            }],
        });
        messages.push(CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::ToolResult {
                call_id: format!("call_old_{step}"),
                output: format!(
                    "// file_{step}.rs header\n{}\n// file_{step}.rs EOF",
                    "x".repeat(15_000)
                ),
                is_error: false,
            }],
        });
    }

    // 3. Assistant completion summary for Task 1
    messages.push(CanonicalMessage {
        role: CanonicalRole::Assistant,
        parts: vec![CanonicalPart::Text(
            "Task 1 is complete: `liftgate` CLI built and all tests passed.".to_owned(),
        )],
    });

    // 4. Late user message (reproducing L1170 "突然失忆" scenario after 1.8 MB of history)
    let critical_user_msg =
        "目前我在使用codex的过程中遇到了两个bad case：请重点修复工具调用解析与会话失忆问题！";
    messages.push(CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![CanonicalPart::Text(critical_user_msg.to_owned())],
    });

    // 5. 20 tool steps already executed under Task 2
    for step in 0..20 {
        messages.push(CanonicalMessage {
            role: CanonicalRole::Assistant,
            parts: vec![CanonicalPart::ToolCall {
                call_id: format!("call_new_{step}"),
                name: "exec_command".to_owned(),
                arguments: format!("{{\"cmd\":\"cargo test step_{step}\"}}"),
                is_custom: false,
            }],
        });
        messages.push(CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::ToolResult {
                call_id: format!("call_new_{step}"),
                output: format!(
                    "running 12 tests for step_{step} ... ok\n{}",
                    "y".repeat(6_000)
                ),
                is_error: false,
            }],
        });
    }

    let blocks = build_acp_prompt_blocks(&messages, "f032d02ecd5b", true);
    let joined: String = blocks
        .iter()
        .filter_map(|b| match b {
            AcpPromptBlock::Text { text } => Some(text.as_str()),
            AcpPromptBlock::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        joined.len() <= MAX_ACP_PROMPT_TEXT_BYTES,
        "cold replay prompt len {} exceeded safe ceiling {}",
        joined.len(),
        MAX_ACP_PROMPT_TEXT_BYTES
    );
    assert!(joined.contains("<developer_instructions>\nFollow workspace Rust rules."));
    assert!(joined.contains("Task 1: build the `liftgate` management CLI."));
    assert!(joined.contains("Task 1 is complete: `liftgate` CLI built and all tests passed."));
    assert!(joined.contains(critical_user_msg));
    assert!(joined.contains("### Current Caller Input"));
    assert!(joined.contains("[Client Tool Result id=call_new_19 status=ok]:"));
}

#[test]
fn preserves_timestamps_and_token_budgets_in_prompts_while_keeping_prefix_hash_stable() {
    let model = ModelId::try_new("gemini-4-carbon-high").expect("valid model");
    let web_cfg = WebToolBridgeConfig::default();
    let sys = "x-anthropic-billing-header: cc_version=2.1.300; cch=abc12;\nYou are a helpful coding assistant.";

    let msg_u1_t1 = CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![
                CanonicalPart::Text(
                    "<current_time_reminder>The current local time is: 2026-11-15T10:00:00+08:00.</current_time_reminder>".to_owned(),
                ),
                CanonicalPart::Text(
                    "<system-reminder>\n<budget:token_budget>200000</budget:token_budget>\n</system-reminder>".to_owned(),
                ),
                CanonicalPart::Text("Check current build status.".to_owned()),
            ],
        };
    let rendered_t1 =
        build_acp_prompt_blocks(std::slice::from_ref(&msg_u1_t1), "deadbeef1234", true);
    let rendered_text = match rendered_t1.first() {
        Some(AcpPromptBlock::Text { text }) => text.as_str(),
        _ => panic!("expected text block"),
    };
    assert!(rendered_text.contains("<current_time_reminder>The current local time is: 2026-11-15T10:00:00+08:00.</current_time_reminder>"));
    assert!(rendered_text.contains("<budget:token_budget>200000</budget:token_budget>"));

    let assistant_t1 = CanonicalMessage {
        role: CanonicalRole::Assistant,
        parts: vec![CanonicalPart::Text("Build is green.".to_owned())],
    };
    let post_hash = hash_conversation_with_reply(
        Some(sys),
        &[],
        web_cfg,
        &model,
        std::slice::from_ref(&msg_u1_t1),
        &assistant_t1,
    );

    // Next turn: harness removes old `<current_time_reminder>` from Turn 1, mutates
    // `<budget:token_budget>`, and inserts a pure volatile `<context_window>` message.
    let msg_u1_t2 = CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![
                CanonicalPart::Text(
                    "<system-reminder>\n<budget:token_budget>182400</budget:token_budget>\n</system-reminder>".to_owned(),
                ),
                CanonicalPart::Text("Check current build status.".to_owned()),
            ],
        };
    let volatile_only_msg = CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![CanonicalPart::Text(
            "<context_window>used=17600 limit=1048576</context_window>".to_owned(),
        )],
    };
    let msg_u2_t2 = CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![CanonicalPart::Text("Now run clippy.".to_owned())],
    };
    let plan = compute_routing_plan(
        Some(
            "x-anthropic-billing-header: cc_version=2.1.300; cch=99999;\nYou are a helpful coding assistant.",
        ),
        &[],
        web_cfg,
        &model,
        &[msg_u1_t2, volatile_only_msg, assistant_t1, msg_u2_t2],
    );
    assert_eq!(plan.prefix_lookup, Some((post_hash, 3)));
}

#[test]
fn tool_mutations_and_effort_switches_preserve_warm_prefix_hash_and_emit_tools_delta() {
    let model_high = ModelId::try_new("gemini-4-carbon-high").expect("valid model");
    let model_xhigh = ModelId::try_new("gemini-4-carbon-xhigh").expect("valid model");
    let web_cfg = WebToolBridgeConfig::default();
    let bash_tool = CanonicalToolSpec {
        name: "Bash".to_owned(),
        description: Some("Run shell".to_owned()),
        kind: CanonicalToolKind::Function {
            parameters_json: "{\"type\":\"object\"}".to_owned(),
        },
    };
    let edit_tool = CanonicalToolSpec {
        name: "Edit".to_owned(),
        description: Some("Edit file".to_owned()),
        kind: CanonicalToolKind::Function {
            parameters_json: "{\"type\":\"object\"}".to_owned(),
        },
    };

    let u1 = CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![CanonicalPart::Text("Plan the fix.".to_owned())],
    };
    let a1 = CanonicalMessage {
        role: CanonicalRole::Assistant,
        parts: vec![CanonicalPart::Text("Here is the plan.".to_owned())],
    };
    let post_hash = hash_conversation_with_reply(
        Some("Agent system"),
        std::slice::from_ref(&bash_tool),
        web_cfg,
        &model_high,
        std::slice::from_ref(&u1),
        &a1,
    );

    let u2 = CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![CanonicalPart::Text("Execute the plan.".to_owned())],
    };
    let next_tools = vec![bash_tool.clone(), edit_tool];
    let plan = compute_routing_plan(
        Some("Agent system"),
        &next_tools,
        web_cfg,
        &model_xhigh,
        &[u1, a1, u2],
    );
    assert_eq!(plan.prefix_lookup, Some((post_hash, 2)));

    let delta = build_client_tools_delta_xml(
        std::slice::from_ref(&bash_tool),
        &next_tools,
        "1a2b3c4d5e6f",
    )
    .expect("expected tool delta");
    assert!(delta.contains("<client_tools_update>"));
    assert!(delta.contains("name: Edit"));
    assert!(!delta.contains("name: Bash"));
}

#[test]
fn compaction_summarization_turn_matches_warm_dialogue_hash_for_codex_and_claude() {
    let model_xhigh = ModelId::try_new("gemini-4-carbon-xhigh").expect("valid model");
    let model_high = ModelId::try_new("gemini-4-carbon-high").expect("valid model");
    let web_cfg = WebToolBridgeConfig::default();

    let u1 = CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![
            CanonicalPart::Text(
                "<developer_instructions>\nCodex coding rules\n</developer_instructions>"
                    .to_owned(),
            ),
            CanonicalPart::Text("Implement feature X.".to_owned()),
        ],
    };
    let a1 = CanonicalMessage {
        role: CanonicalRole::Assistant,
        parts: vec![CanonicalPart::Text("Implemented feature X.".to_owned())],
    };
    let dialogue_hash = hash_dialogue_with_reply(&model_xhigh, std::slice::from_ref(&u1), &a1);

    // 1. Codex `/compact`: drops `<developer_instructions>`, clears `tools`, appends
    //    `CONTEXT CHECKPOINT COMPACTION` + deferred compaction `instructions`.
    let codex_u1 = CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![CanonicalPart::Text("Implement feature X.".to_owned())],
    };
    let codex_compact_prompt = CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![CanonicalPart::Text(
            "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary."
                .to_owned(),
        )],
    };
    let codex_plan = compute_routing_plan(
        None,
        &[],
        web_cfg,
        &model_high,
        &[codex_u1.clone(), a1.clone(), codex_compact_prompt],
    );
    assert_eq!(codex_plan.compaction_lookup, Some((dialogue_hash, 2)));

    // 2. Claude Code `/compact`: replaces `system` with summarization system prompt and clears `tools`.
    let cc_compact_prompt = CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![CanonicalPart::Text(
            "Your task is to create a detailed summary of the conversation so far.".to_owned(),
        )],
    };
    let cc_plan = compute_routing_plan(
        Some("You are a helpful AI assistant tasked with summarizing conversations."),
        &[],
        web_cfg,
        &model_high,
        &[codex_u1, a1, cc_compact_prompt],
    );
    assert_eq!(cc_plan.compaction_lookup, Some((dialogue_hash, 2)));
}

#[test]
fn bounded_single_turn_plan_archives_cold_history_preserves_active_turn_and_escapes_slash() {
    let mut messages = Vec::new();
    messages.push(CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![CanonicalPart::Text(format!(
            "EARLY_NEEDLE_ALPHA_001\n{}",
            "a".repeat(95_000)
        ))],
    });
    messages.push(CanonicalMessage {
        role: CanonicalRole::Assistant,
        parts: vec![CanonicalPart::Text(format!(
            "MID_NEEDLE_BETA_002\n{}",
            "b".repeat(115_000)
        ))],
    });
    messages.push(CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![CanonicalPart::Text(format!(
            "MID_NEEDLE_GAMMA_003\n{}",
            "c".repeat(115_000)
        ))],
    });
    messages.push(CanonicalMessage {
        role: CanonicalRole::Assistant,
        parts: vec![CanonicalPart::Text(format!(
            "LATE_NEEDLE_DELTA_004\n{}",
            "d".repeat(85_000)
        ))],
    });
    messages.push(CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![CanonicalPart::Text(
            "/compact summarize and verify FINAL_NEEDLE_OMEGA_005".to_owned(),
        )],
    });

    let safe = build_acp_prompt_plan(
        &messages,
        "abcd1234ef56",
        true,
        Some("<client_tools_update>new tool</client_tools_update>"),
    )
    .unwrap();
    assert!(total_text_block_bytes(&safe) <= MAX_ACP_PROMPT_TEXT_BYTES);
    let safe_text = safe
        .iter()
        .filter_map(|b| match b {
            AcpPromptBlock::Text { text } => Some(text.as_str()),
            AcpPromptBlock::Image { .. } => None,
        })
        .collect::<String>();
    assert!(safe_text.starts_with("<client_tools_update>"));
    assert!(safe_text.contains("FINAL_NEEDLE_OMEGA_005"));
    assert!(
        build_acp_prompt_plan(
            &messages,
            "nonce",
            true,
            Some(&"x".repeat(MAX_ACP_PROMPT_TEXT_BYTES)),
        )
        .is_err()
    );
    let oversized = [CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![CanonicalPart::Text(format!(
            "MEGA_HEAD_MARKER\n{}\nMEGA_TAIL_MARKER",
            "x".repeat(MAX_ACP_PROMPT_TEXT_BYTES * 3)
        ))],
    }];
    let bounded_mega = build_acp_prompt_plan(&oversized, "nonce", false, None)
        .expect("oversized single turn should auto-truncate middle instead of failing");
    let mega_text: String = bounded_mega
        .iter()
        .filter_map(|b| match b {
            AcpPromptBlock::Text { text } => Some(text.as_str()),
            AcpPromptBlock::Image { .. } => None,
        })
        .collect();
    assert!(mega_text.contains("MEGA_HEAD_MARKER"));
    assert!(mega_text.contains("MEGA_TAIL_MARKER"));
    assert!(mega_text.contains("[Gateway truncated input text:"));

    // Direct warm-delta prompt starting with `/compact` must escape the leading slash.
    let direct_slash = build_acp_prompt_plan(
        &[CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::Text("/compact focus on auth".to_owned())],
        }],
        "abcd1234ef56",
        false,
        None,
    )
    .unwrap();
    match direct_slash.first() {
        Some(AcpPromptBlock::Text { text }) => {
            assert!(!text.starts_with('/'));
            assert_eq!(text, "\u{200b}/compact focus on auth");
        }
        _ => panic!("expected text block"),
    }
}

#[test]
fn spills_massive_mcp_tool_catalog_past_16kb_cap_and_slims_only_developer_instructions() {
    let many_tools: Vec<CanonicalToolSpec> = (0..60)
            .map(|i| CanonicalToolSpec {
                name: format!("mcp__opencode_server__tool_{i:02}"),
                description: Some(format!(
                    "Detailed OpenCode MCP tool {i} description: {}",
                    "capability ".repeat(35)
                )),
                kind: CanonicalToolKind::Function {
                    parameters_json:
                        r#"{"type":"object","properties":{"path":{"type":"string"},"recursive":{"type":"boolean"}},"required":["path"]}"#
                            .to_owned(),
                },
            })
            .collect();

    let (append, spill) = build_session_system_append_with_spill(
        None,
        &many_tools,
        WebToolBridgeConfig::default(),
        "abcd12",
    );
    assert!(
        append.len() <= MAX_SYSTEM_APPEND_BYTES,
        "systemPrompt.append len {} exceeded cap {}",
        append.len(),
        MAX_SYSTEM_APPEND_BYTES
    );
    let spill_xml = spill.expect("60 verbose MCP tools must spill excess schemas into Turn 1");
    assert!(spill_xml.starts_with("<client_tools_update>"));
    assert!(spill_xml.contains("mcp__opencode_server__tool_59"));
    assert!(append.contains("mcp__opencode_server__tool_00"));

    // Giant 150 KB `<developer_instructions>` + 35 KB user prompt (> 176 KB total) must slim
    // ONLY `<developer_instructions>` while preserving 100% of the user prompt losslessly.
    let user_prompt = format!("USER_START_MARKER\n{}\nUSER_END_MARKER", "u".repeat(35_000));
    let giant_dev = format!(
        "<developer_instructions>\nHEAD_RULE\n{}\nTAIL_RULE\n</developer_instructions>",
        "r".repeat(150_000)
    );
    let plan = build_acp_prompt_plan(
        &[
            CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text(giant_dev)],
            },
            CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text(user_prompt.clone())],
            },
        ],
        "abcd12",
        true,
        None,
    )
    .expect("giant developer_instructions should slim instead of failing Turn 1");
    let rendered = match plan.first() {
        Some(AcpPromptBlock::Text { text }) => text,
        _ => panic!("expected text block"),
    };
    assert!(rendered.len() <= MAX_ACP_PROMPT_TEXT_BYTES);
    assert!(
        rendered.contains(&user_prompt),
        "caller user prompt must remain 100% intact and uncompacted"
    );
    assert!(rendered.contains("HEAD_RULE"));
    assert!(rendered.contains("TAIL_RULE"));

    // An oversized raw user log paste (> 176 KB) must also auto-truncate its middle cleanly
    // (preserving beginning + end) rather than throwing a hard `PromptTooLarge` error.
    let oversized_user = CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![CanonicalPart::Text(format!(
            "PASTE_HEAD_LINE\n{}\nPASTE_TAIL_LINE",
            "log_entry_1234567890\n".repeat(12_000)
        ))],
    };
    let bounded_user_plan = build_acp_prompt_plan(&[oversized_user], "abcd12", true, None)
        .expect("oversized user paste must middle-truncate instead of failing");
    let bounded_user_text = match bounded_user_plan.first() {
        Some(AcpPromptBlock::Text { text }) => text,
        _ => panic!("expected text block"),
    };
    assert!(bounded_user_text.len() <= MAX_ACP_PROMPT_TEXT_BYTES);
    assert!(bounded_user_text.contains("PASTE_HEAD_LINE"));
    assert!(bounded_user_text.contains("PASTE_TAIL_LINE"));
    assert!(bounded_user_text.contains("[Gateway truncated input text:"));
}

#[test]
fn waterfill_head_tail_truncates_195kb_tool_output_preserving_small_siblings_and_session_hash() {
    let model = ModelId::try_new("gemini-4-carbon-high").expect("valid model");
    let web_cfg = WebToolBridgeConfig::default();

    let mut giant_cmd_output = String::with_capacity(200_000);
    giant_cmd_output.push_str("CMD_START: cargo test --workspace\n");
    for line_no in 2..=3_200 {
        giant_cmd_output.push_str(&format!(
                "line_{line_no:04}: compiler diagnostic output详情 payload_segment_abcdefghijklmnopqrstuvwxyz\n"
            ));
    }
    giant_cmd_output.push_str("CMD_END: test result: FAILED. 142 passed; 2 failed;\n");
    assert!(
        giant_cmd_output.len() > 195_000,
        "expected >195 KB tool result, got {}",
        giant_cmd_output.len()
    );

    let small_sibling_a = "SMALL_TOOL_A_EXACT_CONTENT_100_PERCENT_INTACT";
    let small_sibling_b = "SMALL_TOOL_B_EXACT_CONTENT_100_PERCENT_INTACT";

    let turn_delta = CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![
            CanonicalPart::ToolResult {
                call_id: "call_small_a".to_owned(),
                output: small_sibling_a.to_owned(),
                is_error: false,
            },
            CanonicalPart::ToolResult {
                call_id: "call_giant".to_owned(),
                output: giant_cmd_output.clone(),
                is_error: true,
            },
            CanonicalPart::ToolResult {
                call_id: "call_small_b".to_owned(),
                output: small_sibling_b.to_owned(),
                is_error: false,
            },
        ],
    };

    let plan = build_acp_prompt_plan(std::slice::from_ref(&turn_delta), "abcd12", false, None)
        .expect("195 KB tool output must truncate middle cleanly without erroring");

    let rendered = match plan.first() {
        Some(AcpPromptBlock::Text { text }) => text,
        _ => panic!("expected single text block"),
    };
    assert!(
        rendered.len() <= MAX_ACP_PROMPT_TEXT_BYTES,
        "rendered len {} exceeded {}",
        rendered.len(),
        MAX_ACP_PROMPT_TEXT_BYTES
    );
    // Water-filling must preserve >95% of the 176 KB budget instead of dividing 176 KB / 3!
    assert!(
        rendered.len() > 168_000,
        "water-filling should utilize nearly full 176 KB budget, got {}",
        rendered.len()
    );
    assert!(rendered.contains(small_sibling_a));
    assert!(rendered.contains(small_sibling_b));
    assert!(rendered.contains("CMD_START: cargo test --workspace"));
    assert!(rendered.contains("CMD_END: test result: FAILED. 142 passed; 2 failed;"));
    assert!(rendered.contains("[Gateway truncated tool output:"));
    assert!(rendered.contains("lines "));

    // Verify next-turn warm `SessionPool` prefix hash still hits 100% when client resends
    // the original untruncated 195 KB `ToolResult` in `messages`!
    let u1 = CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![CanonicalPart::Text("Run workspace tests".to_owned())],
    };
    let a1 = CanonicalMessage {
        role: CanonicalRole::Assistant,
        parts: vec![CanonicalPart::ToolCall {
            call_id: "call_giant".to_owned(),
            name: "Bash".to_owned(),
            arguments: "{\"command\":\"cargo test\"}".to_owned(),
            is_custom: false,
        }],
    };
    let a2 = CanonicalMessage {
        role: CanonicalRole::Assistant,
        parts: vec![CanonicalPart::Text(
            "I see 2 failures at the end of the truncated test output.".to_owned(),
        )],
    };
    let committed_hash = hash_conversation_with_reply(
        Some("You are a coding agent."),
        &[],
        web_cfg,
        &model,
        &[u1.clone(), a1.clone(), turn_delta.clone()],
        &a2,
    );
    let u3 = CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![CanonicalPart::Text("Fix those 2 failures.".to_owned())],
    };
    let next_routing = compute_routing_plan(
        Some("You are a coding agent."),
        &[],
        web_cfg,
        &model,
        &[u1, a1, turn_delta, a2, u3],
    );
    assert_eq!(next_routing.prefix_lookup, Some((committed_hash, 4)));
}

#[test]
fn preserves_full_code_mode_exec_and_repl_tool_declarations_inline_and_on_spill() {
    let mut ts_decls = String::from(
        "Run JavaScript code to orchestrate tool calls. Accepts raw JavaScript source text, not JSON, quoted strings, or markdown code fences.\n\
             Runtime helpers: `exit()`, `text(...)`, `image(...)`, `store(k, v)`, `load(k)`, `yield_control()`, `ALL_TOOLS`.\n\
             Tool TypeScript declarations:\n```ts\ndeclare const tools: {\n  apply_patch(args: string): Promise<unknown>;\n  exec_command(args: { cmd: string; workdir?: string }): Promise<{ output: string }>;\n",
    );
    for idx in 0..8 {
        ts_decls.push_str(&format!(
                "  mcp__workspace__tool_{idx:02}(args: {{ query: string; limit?: number }}): Promise<{{ items: string[] }}>;\n"
            ));
    }
    ts_decls.push_str("};\n```");
    assert!(ts_decls.len() > 1_200 && ts_decls.len() < 10_000);

    let inline_tools = vec![
            CanonicalToolSpec {
                name: "exec".to_owned(),
                description: Some(ts_decls.clone()),
                kind: CanonicalToolKind::Custom {
                    format_json: Some(
                        r#"{"type":"grammar","syntax":"lark","definition":"start: /[\\s\\S]+/"}"#
                            .to_owned(),
                    ),
                },
            },
            CanonicalToolSpec {
                name: "wait".to_owned(),
                description: Some("Wait on a yielded `exec` cell ID or terminate it.".to_owned()),
                kind: CanonicalToolKind::Function {
                    parameters_json:
                        r#"{"type":"object","properties":{"cell_id":{"type":"string"},"terminate":{"type":"boolean"}},"required":["cell_id"]}"#
                            .to_owned(),
                },
            },
        ];

    let (inline_append, inline_spill) = build_session_system_append_with_spill(
        None,
        &inline_tools,
        WebToolBridgeConfig::default(),
        "9c2e",
    );
    assert!(inline_spill.is_none());
    assert!(inline_append.len() <= MAX_SYSTEM_APPEND_BYTES);
    assert!(
        inline_append.contains("mcp__workspace__tool_07"),
        "Code Mode `exec` TypeScript declarations must NOT be truncated at 600 chars"
    );
    assert!(inline_append.contains("{\"name\": \"exec\", \"input\":"));

    // Now build a >18 KB Code Mode `exec` tool (45 nested MCP tools) and verify it keeps
    // `systemPrompt.append <= 16 KB` while spilling 100% of `declare const tools` into Turn 1!
    let mut big_decls = ts_decls;
    big_decls.truncate(big_decls.len().saturating_sub(6));
    for idx in 8..180 {
        big_decls.push_str(&format!(
                "  mcp__enterprise__connector_action_{idx:03}(args: {{ resource_id: string; payload: Record<string, unknown> }}): Promise<{{ ok: boolean }}>;\n"
            ));
    }
    big_decls.push_str("};\n```");
    assert!(big_decls.len() > 18_000);

    let big_tools = vec![
        CanonicalToolSpec {
            name: "exec".to_owned(),
            description: Some(big_decls),
            kind: CanonicalToolKind::Custom { format_json: None },
        },
        inline_tools[1].clone(),
    ];

    let (spilled_append, spilled_xml_opt) = build_session_system_append_with_spill(
        None,
        &big_tools,
        WebToolBridgeConfig::default(),
        "9c2e",
    );
    assert!(spilled_append.len() <= MAX_SYSTEM_APPEND_BYTES);
    assert!(spilled_append.contains("{\"name\": \"exec\", \"input\":"));
    let spilled_xml = spilled_xml_opt.expect("18 KB+ `exec` tool must spill into Turn 1");
    assert!(spilled_xml.contains("mcp__enterprise__connector_action_008"));
    assert!(spilled_xml.contains("mcp__enterprise__connector_action_179"));
    assert!(spilled_xml.contains("- name: wait"));

    // Claude Code Mode `REPL` tool (>1.5 KB Bun sandbox helper docs) is also preserved intact.
    let repl_desc = format!(
        "Execute JavaScript inside the persistent Bun Code Mode sandbox.\n{}\nBuilt-in helpers: `$`, `edit(path, old, new)`, `create(path, content)`, `view(path)`.",
        "// sandbox capability line\n".repeat(45)
    );
    let repl_tools = vec![CanonicalToolSpec {
        name: "REPL".to_owned(),
        description: Some(repl_desc),
        kind: CanonicalToolKind::Function {
            parameters_json:
                r#"{"type":"object","properties":{"code":{"type":"string"}},"required":["code"]}"#
                    .to_owned(),
        },
    }];
    let (repl_append, repl_spill) = build_session_system_append_with_spill(
        None,
        &repl_tools,
        WebToolBridgeConfig::default(),
        "9c2e",
    );
    assert!(repl_spill.is_none());
    assert!(repl_append.contains("`edit(path, old, new)`, `create(path, content)`, `view(path)`"));
}

#[test]
fn every_truncation_and_omission_path_persists_lossless_file_and_cites_path() {
    let mut sink = MemorySpillSink::new("/tmp/liftgate-spills");

    // 1. Oversized tool output + developer instructions with <example> blocks
    let dev_body = format!(
        "CORE_RULE_HEAD\n<example>\n{}\n</example>\nCORE_RULE_TAIL",
        "example_line_payload\n".repeat(3_000)
    );
    let huge_tool_out: String = (1..=3_500)
        .map(|i| format!("OUT_LINE_{i:04}: {}\n", "a".repeat(48)))
        .collect();
    let active_msg = CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![
            CanonicalPart::Text(format!(
                "<developer_instructions>\n{dev_body}\n</developer_instructions>"
            )),
            CanonicalPart::ToolResult {
                call_id: "call_big".to_owned(),
                output: huge_tool_out.clone(),
                is_error: false,
            },
        ],
    };
    let plan = build_acp_prompt_plan_with_spill(
        std::slice::from_ref(&active_msg),
        "abcd12",
        false,
        None,
        &mut sink,
    )
    .expect("bounded prompt plan");
    let text = match plan.first() {
        Some(AcpPromptBlock::Text { text }) => text,
        _ => panic!("expected text block"),
    };
    assert!(text.len() <= MAX_ACP_PROMPT_TEXT_BYTES);
    assert!(text.contains("OUT_LINE_0001:"));
    assert!(text.contains("OUT_LINE_3500:"));

    let expected_tool_file = format!(
        "/tmp/liftgate-spills/{}",
        format_content_addressed_spill_filename("tool output", huge_tool_out.trim())
    );
    let expected_dev_file = format!(
        "/tmp/liftgate-spills/{}",
        format_content_addressed_spill_filename("developer instructions", &dev_body)
    );
    assert!(
        text.contains(&expected_tool_file),
        "truncated tool output must cite exact local spill file `{expected_tool_file}`"
    );
    assert!(
        text.contains(&expected_dev_file),
        "stripped developer instructions must cite exact local spill file `{expected_dev_file}`"
    );
    assert_eq!(
        sink.files().get(&expected_tool_file).map(String::as_str),
        Some(huge_tool_out.trim())
    );
    assert_eq!(
        sink.files().get(&expected_dev_file).map(String::as_str),
        Some(dev_body.as_str())
    );

    // 2. Cold history replay: omitted turns + kept truncated archival tool outputs
    let mut history_msgs = vec![CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![CanonicalPart::Text("Initial goal".to_owned())],
    }];
    for idx in 0..260 {
        history_msgs.push(CanonicalMessage {
            role: CanonicalRole::Assistant,
            parts: vec![CanonicalPart::ToolCall {
                call_id: format!("c_{idx}"),
                name: "Bash".to_owned(),
                arguments: format!("{{\"cmd\":\"step_{idx}\"}}"),
                is_custom: false,
            }],
        });
        history_msgs.push(CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::ToolResult {
                call_id: format!("c_{idx}"),
                output: format!("STEP_{idx}_HEAD\n{}\nSTEP_{idx}_TAIL", "z".repeat(5_000)),
                is_error: false,
            }],
        });
    }
    let cold_plan =
        build_acp_prompt_plan_with_spill(&history_msgs, "abcd12", true, None, &mut sink)
            .expect("cold history plan");
    let cold_text = match cold_plan.first() {
        Some(AcpPromptBlock::Text { text }) => text,
        _ => panic!("expected text block"),
    };
    assert!(cold_text.len() <= MAX_ACP_PROMPT_TEXT_BYTES);
    assert!(cold_text.contains(
        "full uncompacted turns saved locally at `/tmp/liftgate-spills/omitted-archival-turns-"
    ));
    assert!(
        cold_text.contains("full content saved at `/tmp/liftgate-spills/archival-tool-output-")
    );

    // 3. Compacted client tool schemas & descriptions persist full JSON catalog
    let verbose_tool = CanonicalToolSpec {
        name: "QueryDb".to_owned(),
        description: Some(format!("Run SQL query.\n{}", "detail ".repeat(200))),
        kind: CanonicalToolKind::Function {
            parameters_json: format!(
                "{{\"type\":\"object\",\"properties\":{{\"sql\":{{\"type\":\"string\",\"description\":\"{}\"}}}}}}",
                "column docs ".repeat(40)
            ),
        },
    };
    let (sys_append, _) = build_session_system_append_with_spill_sink(
        None,
        std::slice::from_ref(&verbose_tool),
        WebToolBridgeConfig::default(),
        "99aa",
        &mut sink,
    );
    assert!(sys_append.contains("/tmp/liftgate-spills/client-tools-catalog-"));
    let catalog_entry = sink
        .files()
        .iter()
        .find(|(k, _)| k.contains("client-tools-catalog-"))
        .expect("expected spilled client tools catalog json");
    assert!(catalog_entry.1.contains(&"detail ".repeat(200)));
    assert!(catalog_entry.1.contains(&"column docs ".repeat(40)));
}

#[test]
fn scrub_terminal_control_noise_handles_ansi_progress_bars_and_conpty_crlf() {
    let clean = "line one\r\nline two\nplain utf8 中文 🚀";
    assert!(matches!(
        scrub_terminal_control_noise(clean),
        std::borrow::Cow::Borrowed(_)
    ));

    let noisy = "\x1b[32mDownloading\x1b[0m 10%...\r\x1b[2KDownloading 65%...\r\x1b[32mFinished\x1b[0m in 1.2s\r\r\n\x1b]0;term-title\x07ab\x08c trailing\r";
    let scrubbed = scrub_terminal_control_noise(noisy);
    assert_eq!(scrubbed, "Finished in 1.2s\nac trailing");
}

#[test]
fn truncated_tool_output_rescues_buried_root_cause_diagnostics_and_spills_raw_original() {
    let mut sink = MemorySpillSink::new("/tmp/liftgate-spills");
    let mut lines = Vec::new();
    for idx in 1..=300 {
        lines.push(format!(
            "{{\"level\":30,\"msg\":\"info startup trace line {idx} padding {}\"}}",
            "a".repeat(60)
        ));
    }
    lines.push("error[E0308]: mismatched types in crates/app/src/lib.rs:42:9".to_owned());
    lines.push("  expected `Result<Config, AppError>`, found `Option<Config>`".to_owned());
    lines.push("Caused by: invalid downstream schema binding".to_owned());
    for idx in 304..=620 {
        lines.push(format!(
            "compiling unrelated downstream crate step {idx} {}",
            "b".repeat(60)
        ));
    }
    let raw_log = format!("\x1b[31m{}\x1b[0m", lines.join("\n"));

    let msg = CanonicalMessage {
        role: CanonicalRole::User,
        parts: vec![CanonicalPart::ToolResult {
            call_id: "call_build".to_owned(),
            output: raw_log.clone(),
            is_error: true,
        }],
    };
    let blocks =
        render_smart_bounded_active_slice(std::slice::from_ref(&msg), "1122", 12_000, &mut sink);
    let rendered = match blocks.first() {
        Some(AcpPromptBlock::Text { text }) => text,
        _ => panic!("expected text block"),
    };
    assert!(rendered.len() <= 12_000);
    assert!(!rendered.contains("\x1b[31m"));
    let (_, after_diag_open) = rendered
        .split_once("[Diagnostics extracted from omitted middle:]\n")
        .expect("expected extracted diagnostics header");
    let (diag_section, _) = after_diag_open
        .split_once("[End extracted diagnostics]")
        .expect("expected extracted diagnostics footer");
    assert!(diag_section.contains("error[E0308]: mismatched types"));
    assert!(diag_section.contains("Caused by: invalid downstream schema binding"));
    assert!(!diag_section.contains("\"level\":30"));

    let expected_spill = format!(
        "/tmp/liftgate-spills/{}",
        format_content_addressed_spill_filename("tool output", raw_log.trim())
    );
    assert_eq!(
        sink.files().get(&expected_spill).map(String::as_str),
        Some(raw_log.trim()),
        "spill store must retain 100% raw un-scrubbed ANSI content"
    );
}

#[test]
fn bridge_rules_forbid_host_plugin_mcp_tools_and_guide_parallel_batching() {
    let tool = CanonicalToolSpec {
        name: "Read".to_owned(),
        description: Some("Read file".to_owned()),
        kind: CanonicalToolKind::Function {
            parameters_json:
                "{\"type\":\"object\",\"properties\":{\"file_path\":{\"type\":\"string\"}}}"
                    .to_owned(),
        },
    };
    let append = build_session_system_append(
        None,
        std::slice::from_ref(&tool),
        WebToolBridgeConfig::default(),
        "abcd",
    );
    assert!(append.contains("call_mcp_tool"));
    assert!(append.contains("mcp_*"));
    assert!(append.contains("Batch independent read-only searches"));
}

#[test]
fn mcp_system_append_and_tools_delta_are_xml_free_and_nonce_deterministic() {
    let bridged = vec![
        ("exec".to_owned(), "exec".to_owned()),
        ("apply_patch".to_owned(), "apply_patch".to_owned()),
    ];
    let hubs = vec![("ns__github".to_owned(), "github".to_owned(), 42usize)];
    let spec = McpBridgePromptSpec {
        bridged_tools: &bridged,
        virtualized_hubs: &hubs,
        has_virtualized_dispatch: true,
        has_gateway_web_fetch: true,
        has_context_spill_reader: false,
        orchestrator: None,
    };
    let (append_a, spill_a) = build_session_system_append_for_mcp_with_spill_sink(
        Some("Caller instructions."),
        spec,
        WebToolBridgeConfig {
            enable_native_web_search: true,
            force_native_web_search: false,
            enable_native_web_fetch: true,
            force_native_web_fetch: false,
        },
        "nonce_alpha_111",
        &mut NoopSpillSink,
    );
    let (append_b, spill_b) = build_session_system_append_for_mcp_with_spill_sink(
        Some("Caller instructions."),
        spec,
        WebToolBridgeConfig {
            enable_native_web_search: true,
            force_native_web_search: false,
            enable_native_web_fetch: true,
            force_native_web_fetch: false,
        },
        "nonce_bravo_999",
        &mut NoopSpillSink,
    );
    assert_eq!(
        append_a, append_b,
        "MCP system prompt must be 100% nonce-free for cross-session prefix cache hits"
    );
    assert_eq!(spill_a, spill_b);
    assert!(!append_a.contains("<agy_tool_call_"));
    assert!(!append_a.contains("<client_tools>"));
    assert!(append_a.contains("mcp_lg_apply_patch"));
    assert!(append_a.contains("mcp_lg_gateway_web_fetch"));

    let next_tools = vec![CanonicalToolSpec {
        name: "sql_lark".to_owned(),
        description: Some("Lark grammar tool".to_owned()),
        kind: CanonicalToolKind::Custom { format_json: None },
    }];
    let delta = build_mcp_virtualized_tools_delta_xml_with_spill(
        &[],
        &next_tools,
        "nonce_alpha_111",
        &mut NoopSpillSink,
    )
    .expect("non-empty delta");
    assert!(!delta.contains("<agy_tool_call_"));
    assert!(delta.contains("sql_lark"));
}

#[test]
fn omp_eval_codemode_injects_unprefixed_bridge_rule_and_matches_compaction_prompts() {
    assert!(is_compaction_instructions_prompt(
        "You summarize user–AI coding-assistant conversations into structured handoffs."
    ));
    assert!(is_compaction_instructions_prompt(
        "Write a handoff document for another instance of yourself to continue seamlessly."
    ));

    let eval_spec = CanonicalToolSpec {
            name: "eval".to_owned(),
            description: Some("Persistent kernels (`py` / `js`).\n<prelude>await tool.read()</prelude>".to_owned()),
            kind: CanonicalToolKind::Function {
                parameters_json: r#"{"type":"object","properties":{"language":{"type":"string"},"code":{"type":"string"}}}"#.to_owned(),
            },
        };
    assert!(is_omp_eval_orchestrator_tool(&eval_spec));
    assert!(is_code_mode_orchestrator_tool(&eval_spec));

    let bridged = vec![
        ("eval".to_owned(), "eval".to_owned()),
        ("bash".to_owned(), "bash".to_owned()),
        ("edit".to_owned(), "edit".to_owned()),
    ];
    let hubs = vec![("ns__github".to_owned(), "github".to_owned(), 12usize)];
    let mcp_spec = McpBridgePromptSpec {
        bridged_tools: &bridged,
        virtualized_hubs: &hubs,
        has_virtualized_dispatch: true,
        has_gateway_web_fetch: true,
        has_context_spill_reader: false,
        orchestrator: Some(McpOrchestratorContract::OmpPolyglotEval {
            mcp_name: "eval",
            elided_subordinate_tools: true,
        }),
    };
    let mut spill = NoopSpillSink;
    let (append, _) = build_session_system_append_for_mcp_with_spill_sink(
        Some("You are Oh My Pi."),
        mcp_spec,
        WebToolBridgeConfig::default(),
        "nonce_omp_test",
        &mut spill,
    );
    assert!(append.contains("Polyglot Code Mode (`mcp_lg_eval`)"));
    assert!(append.contains(
        "call `tool.<name>(args)` and MCP eval globals by their unprefixed client names"
    ));
    assert!(append.contains("folded into `mcp_lg_eval`"));
    assert!(!append.contains(
        "Batch independent read-only searches, reads, or checks as parallel `mcp_lg_*` calls"
    ));

    let empty_desc_eval = CanonicalToolSpec {
        name: "eval".to_owned(),
        description: Some(String::new()),
        kind: CanonicalToolKind::Function {
            parameters_json: r#"{"type":"object","properties":{"language":{"type":"string"},"code":{"type":"string"}}}"#.to_owned(),
        },
    };
    let fallback_desc = compact_mcp_tool_description(&empty_desc_eval, 8_000, &mut spill)
        .expect("empty-description OMP eval must synthesize fallback Code Mode docs");
    assert!(fallback_desc.contains("await tool.<name>(args)"));

    let omp_skill_aux = crate::canonical::detect_auxiliary_task(
        None,
        &[CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::Text(
                "Compress into one routing hint of at most 12 words: WHEN to use; WHEN NOT only if it prevents confusion.\n\nSkill: agents".to_owned(),
            )],
        }],
        &[],
        false,
        false,
    );
    assert_eq!(
        omp_skill_aux,
        Some(crate::canonical::AuxiliaryTaskKind::AwayRecapOrSuggestion)
    );
}

#[test]
fn generic_eval_tool_does_not_receive_omp_polyglot_codemode_instructions() {
    let generic_fn_eval = CanonicalToolSpec {
        name: "eval".to_owned(),
        description: Some(
            "Evaluate a mathematical expression and return its numeric value.".to_owned(),
        ),
        kind: CanonicalToolKind::Function {
            parameters_json: r#"{"type":"object","properties":{"expression":{"type":"string"}}}"#
                .to_owned(),
        },
    };
    let generic_custom_eval = CanonicalToolSpec {
        name: "eval".to_owned(),
        description: Some("Evaluate an arithmetic expression.".to_owned()),
        kind: CanonicalToolKind::Custom { format_json: None },
    };
    assert!(!is_omp_eval_orchestrator_tool(&generic_fn_eval));
    assert!(!is_code_mode_orchestrator_tool(&generic_fn_eval));
    assert!(!is_omp_eval_orchestrator_tool(&generic_custom_eval));
    assert!(!is_code_mode_orchestrator_tool(&generic_custom_eval));

    let bridged = vec![
        ("eval".to_owned(), "eval".to_owned()),
        ("read_file".to_owned(), "read_file".to_owned()),
    ];
    let mcp_spec = McpBridgePromptSpec {
        bridged_tools: &bridged,
        virtualized_hubs: &[],
        has_virtualized_dispatch: false,
        has_gateway_web_fetch: false,
        has_context_spill_reader: false,
        orchestrator: None,
    };
    let mut spill = NoopSpillSink;
    let (append, _) = build_session_system_append_for_mcp_with_spill_sink(
        Some("You are a helpful math assistant."),
        mcp_spec,
        WebToolBridgeConfig::default(),
        "nonce_generic_eval",
        &mut spill,
    );
    assert!(!append.contains("Polyglot Code Mode"));
    assert!(!append.contains("tool.<name>(args)"));
}

#[test]
fn opencode_execute_contract_preserves_v1_catalog_and_classifies_summary_auxiliary() {
    let v1_catalog_desc = format!(
        "Run a confined orchestration script with access to connected MCP tools.\n\n## Rules\n- Only Code Mode tools listed here or returned by `tools.$codemode.search` and internal runtime tools are available.\n\n## Available tools\n{}",
        "- github.list_prs({ repo: string }): Promise<unknown>\n".repeat(60)
    );
    assert!(v1_catalog_desc.len() > 2_500);
    let v1_exec = CanonicalToolSpec {
        name: "execute".to_owned(),
        description: Some(v1_catalog_desc.clone()),
        kind: CanonicalToolKind::Function {
            parameters_json:
                r#"{"type":"object","properties":{"code":{"type":"string"}},"required":["code"]}"#
                    .to_owned(),
        },
    };
    assert!(is_opencode_execute_orchestrator_tool(&v1_exec));
    assert!(!is_opencode_full_code_mode_exec(&v1_exec));
    assert!(is_code_mode_orchestrator_tool(&v1_exec));

    let mut spill = NoopSpillSink;
    let full_kept = compact_mcp_tool_description(&v1_exec, 36_000, &mut spill).expect("kept");
    assert!(!full_kept.contains("omitted"));
    assert_eq!(full_kept.matches("github.list_prs").count(), 60);

    // Unrelated SQL `execute(query)` tool is NOT treated as an OpenCode Code Mode orchestrator.
    let sql_exec = CanonicalToolSpec {
        name: "execute".to_owned(),
        description: Some("Execute parameterized SQL statement.".to_owned()),
        kind: CanonicalToolKind::Function {
            parameters_json:
                r#"{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}"#
                    .to_owned(),
        },
    };
    assert!(!is_opencode_execute_orchestrator_tool(&sql_exec));
    assert!(!is_code_mode_orchestrator_tool(&sql_exec));

    // Full Code Mode prompt rules steer workspace operations into `mcp_lg_execute`.
    let bridged = vec![
        ("execute".to_owned(), "execute".to_owned()),
        ("subagent".to_owned(), "subagent".to_owned()),
        ("question".to_owned(), "question".to_owned()),
    ];
    let mcp_spec = McpBridgePromptSpec {
        bridged_tools: &bridged,
        virtualized_hubs: &[],
        has_virtualized_dispatch: true,
        has_gateway_web_fetch: false,
        has_context_spill_reader: false,
        orchestrator: Some(McpOrchestratorContract::OpenCodeExecute {
            mcp_name: "execute",
            full_code_mode: true,
        }),
    };
    let (append, _) = build_session_system_append_for_mcp_with_spill_sink(
        Some("You are an AI agent running in OpenCode."),
        mcp_spec,
        WebToolBridgeConfig::default(),
        "nonce_oc_exec",
        &mut spill,
    );
    assert!(append.contains("OpenCode Code Mode (`mcp_lg_execute`)"));
    assert!(append.contains("Bun.file"));
    assert!(append.contains("do NOT call `mcp_lg_call_client_tool`"));

    // OpenCode hidden `summary` agent (`conv_len > 2`, empty `client_tools`) routes to auxiliary.
    let multi_turn_msgs = vec![
        CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::Text("Fix calc.rs".to_owned())],
        },
        CanonicalMessage {
            role: CanonicalRole::Assistant,
            parts: vec![CanonicalPart::Text("Fixed mul().".to_owned())],
        },
        CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::Text("Also run tests".to_owned())],
        },
    ];
    let summary_aux = crate::canonical::detect_auxiliary_task(
        Some(
            "Summarize what was done in this conversation. Write like a pull request description.\n\nRules:\n- 2-3 sentences max",
        ),
        &multi_turn_msgs,
        &[],
        false,
        false,
    );
    assert_eq!(
        summary_aux,
        Some(crate::canonical::AuxiliaryTaskKind::AwayRecapOrSuggestion)
    );
}

#[test]
fn active_prompt_waterfill_never_truncates_assistant_output_or_real_tool_call_arguments() {
    use crate::acp::AcpPromptBlock;

    let large_patch = format!(
        "*** Begin Patch\n*** Update File: src/engine.rs\n@@\n{}\n*** End Patch",
        (0..900)
            .map(|i| format!(
                "-    let old_val_{i} = {i};\n+    let new_val_{i} = {};",
                i + 1
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        large_patch.len() > 35_000,
        "patch payload must exceed old 2KB/waterfill truncation thresholds"
    );
    let assistant_prose = "Applying verified single-file patch to `src/engine.rs`.".repeat(40);
    let huge_tool_output = (1..=3_500)
        .map(|ln| {
            format!("line {ln:04}: compiler output diagnostic context padding.................")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(huge_tool_output.len() > 200_000);

    let messages = [
        CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::Text("Refactor src/engine.rs".to_owned())],
        },
        CanonicalMessage {
            role: CanonicalRole::Assistant,
            parts: vec![
                CanonicalPart::Text(assistant_prose.clone()),
                CanonicalPart::ToolCall {
                    call_id: "call_patch_1".to_owned(),
                    name: "apply_patch".to_owned(),
                    arguments: large_patch.clone(),
                    is_custom: true,
                },
            ],
        },
        CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::ToolResult {
                call_id: "call_patch_1".to_owned(),
                output: huge_tool_output,
                is_error: false,
            }],
        },
    ];

    let mut spill = MemorySpillSink::default();
    // Delta from index 1 simulates an active slice carrying both the large ToolCall and 200KB ToolResult.
    let blocks = build_acp_prompt_plan_with_spill(
        messages.get(1..).expect("active slice"),
        "nonce_notrunc",
        false,
        None,
        &mut spill,
    )
    .expect("prompt plan");
    let joined: String = blocks
        .iter()
        .filter_map(|b| match b {
            AcpPromptBlock::Text { text } => Some(text.as_str()),
            AcpPromptBlock::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    let escaped_patch = serde_json::to_string(&large_patch).expect("json-escaped custom patch");
    assert!(
        joined.contains(&escaped_patch),
        "real ToolCall arguments must never be truncated in active prompt planning"
    );
    assert!(
        joined.contains(&assistant_prose),
        "assistant output text must never be truncated in active prompt planning"
    );
    assert!(!joined.contains("_truncated_arguments_preview"));
    assert!(!joined.contains("Gateway truncated tool call arguments"));
    assert!(
        joined.contains("mcp_lg_read_context_spill"),
        "oversized ToolResult should spill cleanly with 0-RTT mcp_lg_read_context_spill notice"
    );
    assert!(!spill.files().is_empty());
}
