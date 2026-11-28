use std::borrow::Cow;
use std::collections::HashSet;

use liftgate_types::{
    canonical::{CanonicalToolKind, CanonicalToolSpec},
    session_key::NoopSpillSink,
    telemetry::{CatalogToolTier, CatalogWireRole},
};
use serde_json::{Value, json};

use super::*;

fn fn_tool(name: &str, desc: &str, params: Value) -> CanonicalToolSpec {
    CanonicalToolSpec {
        name: name.to_owned(),
        description: Some(desc.to_owned()),
        kind: CanonicalToolKind::Function {
            parameters_json: params.to_string(),
        },
    }
}

#[test]
fn sanitizes_names_bijectively_and_preserves_standard_identifiers() {
    let mut used = HashSet::new();
    assert_eq!(sanitize_mcp_tool_name("exec", &mut used), "exec");
    assert_eq!(sanitize_mcp_tool_name("wait", &mut used), "wait");
    assert_eq!(
        sanitize_mcp_tool_name("mcp__node_repl.js", &mut used),
        "mcp__node_repl__js"
    );
    assert_eq!(
        sanitize_mcp_tool_name("__codex_local_shell", &mut used),
        "lg_internal_codex_local_shell"
    );
    assert_eq!(sanitize_mcp_tool_name("exec", &mut used), "exec_2");
}

#[test]
fn codex_profile_virtualizes_128_tools_preserving_eager_exec_and_dehydrating_ts_footers() {
    let ts_footer = "\n\nexec tool declaration:\n```ts\ndeclare const tools: { sample(args: { id: number }): Promise<unknown>; };\n```";
    let mut tools = vec![
        CanonicalToolSpec {
            name: "exec".to_owned(),
            description: Some(
                "Run JavaScript in Code Mode. globalThis.tools exposes nested tools.".to_owned(),
            ),
            kind: CanonicalToolKind::Custom { format_json: None },
        },
        fn_tool(
            "wait",
            &format!("Wait on an async exec cell.{ts_footer}"),
            json!({"type": "object", "properties": {"cell_id": {"type": "string"}}, "required": ["cell_id"]}),
        ),
        fn_tool(
            "exec_command",
            &format!("Run a shell command in PTY.{ts_footer}"),
            json!({"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]}),
        ),
        fn_tool(
            "write_stdin",
            &format!("Write stdin to PTY.{ts_footer}"),
            json!({"type": "object", "properties": {"session_id": {"type": "integer"}}, "required": ["session_id"]}),
        ),
        fn_tool(
            "update_plan",
            &format!("Update active coding plan.{ts_footer}"),
            json!({"type": "object", "properties": {"plan": {"type": "array"}}, "required": ["plan"]}),
        ),
        CanonicalToolSpec {
            name: "apply_patch".to_owned(),
            description: Some(format!("Freeform patch grammar.{ts_footer}")),
            kind: CanonicalToolKind::Custom { format_json: None },
        },
        CanonicalToolSpec {
            name: "sql_lark".to_owned(),
            description: Some("Arbitrary custom Lark grammar.".to_owned()),
            kind: CanonicalToolKind::Custom { format_json: None },
        },
    ];
    for name in ["spawn_agent", "send_input", "wait_agent", "close_agent"] {
        tools.push(fn_tool(
            name,
            &format!("Multi-agent op {name}.{ts_footer}"),
            json!({"type": "object", "properties": {"id": {"type": "string"}}}),
        ));
    }
    for i in 0..80 {
        tools.push(fn_tool(
            &format!("mcp__codex_apps__github._action_{i:02}"),
            &format!("GitHub connector action #{i}.{ts_footer}"),
            json!({
                "type": "object",
                "properties": {
                    "repository_full_name": {"type": "string"},
                    "pr_number": {"type": "integer"}
                },
                "required": ["repository_full_name", "pr_number"]
            }),
        ));
    }
    for i in 0..38 {
        tools.push(fn_tool(
            &format!("mcp__codex_tui.op_{i:02}"),
            &format!("Codex TUI operation #{i}.{ts_footer}"),
            json!({"type": "object", "properties": {"id": {"type": "string"}}}),
        ));
    }
    assert_eq!(tools.len(), 129);
    assert_eq!(
        detect_harness(ProtocolHint::Unknown, &tools),
        DetectedHarness::Codex
    );

    let cat = partition_tools_for_mcp(&tools, &mut NoopSpillSink);
    assert_eq!(cat.profile_id, "codex");
    assert!(cat.has_virtualized_dispatch);
    assert!(cat.wire_tools.len() <= MAX_NATIVE_MCP_TOOLS);
    // Arbitrary custom grammar tool `sql_lark` is also bridged over MCP via `call_client_tool` (`{input: string}`)!
    let lark_res = cat.routing_table.resolve_call(
        CALL_CLIENT_TOOL_MCP_NAME,
        &json!({ "name": "sql_lark", "arguments": { "input": "SELECT 1;" } }),
        &tools,
    );
    assert_eq!(
        lark_res,
        VirtualizedCallDecision::ForwardToClient {
            client_name: "sql_lark".to_owned(),
            arguments: "SELECT 1;".to_owned(),
            is_custom: true,
            patch_repaired: false,
        }
    );

    for must_direct in [
        "exec",
        "wait",
        "exec_command",
        "write_stdin",
        "apply_patch",
        "update_plan",
    ] {
        assert!(
            cat.by_mcp_name.contains_key(must_direct),
            "expected Codex core tool `{must_direct}` in 1:1 Direct Eager MCP"
        );
    }
    assert!(cat.by_mcp_name["apply_patch"].is_custom);

    match cat.routing_table.resolve_call(
            "apply_patch",
            &json!({"patch": "*** Begin Patch\n*** Update File: src/lib.rs\n@@\nbad_context_line\n-old\n+new\n*** End Patch"}),
            &tools,
        ) {
            VirtualizedCallDecision::LocalReply { is_error, kind, text } => {
                assert!(is_error);
                assert_eq!(kind, LocalReplyKind::PreflightError);
                assert!(text.contains("line 4"), "got: {text}");
            }
            VirtualizedCallDecision::ForwardToClient { .. }
            | VirtualizedCallDecision::GatewayWebFetch { .. }
            | VirtualizedCallDecision::ReadContextSpill { .. } => {
                panic!("malformed apply_patch must be caught in 0-RTT preflight")
            }
        }

    match cat.routing_table.resolve_call(
            "apply_patch",
            &json!({"patch": "```diff\n*** Update File: src/lib.rs\n@@\n-a\n+b\n\n ctx\n*** Update File: src/lib.rs\n@@\n-c\n+d\n```"}),
            &tools,
        ) {
            VirtualizedCallDecision::ForwardToClient {
                client_name,
                arguments,
                is_custom,
                patch_repaired,
            } => {
                assert_eq!(client_name, "apply_patch");
                assert!(is_custom);
                assert!(patch_repaired, "fenced, split patch is reported as repaired");
                assert_eq!(
                    arguments,
                    "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-a\n+b\n \n ctx\n@@\n-c\n+d\n*** End Patch"
                );
            }
            VirtualizedCallDecision::LocalReply { text, .. } => {
                panic!("expected coalesced ForwardToClient, got LocalReply: {text}")
            }
            VirtualizedCallDecision::GatewayWebFetch { .. }
            | VirtualizedCallDecision::ReadContextSpill { .. } => {
                panic!("unexpected GatewayWebFetch or ReadContextSpill")
            }
        }
    let exec_cmd_wire = cat
        .wire_tools
        .iter()
        .find(|w| w.name == "exec_command")
        .expect("exec_command wire tool");
    assert!(
        !exec_cmd_wire
            .description
            .as_deref()
            .unwrap_or("")
            .contains("declare const tools"),
        "wire description must strip duplicate Code Mode TS trailer"
    );
    assert!(
        cat.virtualized_hubs
            .iter()
            .any(|(h, ns, cnt)| h == "ns__github" && ns == "github" && *cnt == 80)
    );
    assert!(
        cat.virtualized_hubs
            .iter()
            .any(|(h, ns, cnt)| h == "ns__multi_agent_v1" && ns == "multi_agent_v1" && *cnt == 4)
    );

    match cat.routing_table.resolve_call(
        SEARCH_CLIENT_TOOLS_MCP_NAME,
        &json!({"namespace": "github", "query": "action_42"}),
        &tools,
    ) {
        VirtualizedCallDecision::LocalReply {
            text,
            is_error,
            kind,
        } => {
            assert!(!is_error);
            assert_eq!(kind, LocalReplyKind::Search);
            assert!(text.contains("mcp__codex_apps__github._action_42"));
            assert!(text.contains("declare const tools:"));
        }
        VirtualizedCallDecision::ForwardToClient { .. }
        | VirtualizedCallDecision::GatewayWebFetch { .. }
        | VirtualizedCallDecision::ReadContextSpill { .. } => {
            panic!("search_client_tools must resolve locally in 0-RTT")
        }
    }
}

#[test]
fn claude_code_profile_groups_pascal_families_dedupes_plugins_and_backfills_standalones() {
    let mut tools = Vec::new();
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
        "NotebookEdit",
        "ToolSearch",
    ] {
        tools.push(fn_tool(
            core,
            &format!("Claude Code core tool {core}."),
            json!({"type": "object", "properties": {"input": {"type": "string"}}}),
        ));
    }
    for family_tool in [
        "TodoWrite",
        "TaskCreate",
        "TaskUpdate",
        "TaskGet",
        "TaskList",
        "TaskOutput",
        "TaskStop",
        "EnterPlanMode",
        "ExitPlanMode",
        "EnterWorktree",
        "ExitWorktree",
        "TeamCreate",
        "TeamDelete",
        "SendMessage",
        "CronCreate",
        "ScheduleWakeup",
        "Monitor",
        "ListMcpResourcesTool",
        "ReadMcpResourceTool",
    ] {
        tools.push(fn_tool(
            family_tool,
            &format!("Claude Code built-in {family_tool}."),
            json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}),
        ));
    }
    for standalone in ["AskUserQuestion", "Skill", "SlashCommand"] {
        tools.push(fn_tool(
                standalone,
                &format!("Claude Code standalone {standalone}."),
                json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}),
            ));
    }
    for i in 0..24 {
        tools.push(fn_tool(
                &format!("mcp__plugin_playwright_playwright__browser_op_{i:02}"),
                &format!("Playwright browser op #{i}."),
                json!({"type": "object", "properties": {"selector": {"type": "string"}}, "required": ["selector"]}),
            ));
    }
    for i in 0..30 {
        tools.push(fn_tool(
                &format!("mcp__claude_ai_Linear__issue_op_{i:02}"),
                &format!("Linear issue op #{i}."),
                json!({"type": "object", "properties": {"issue_id": {"type": "string"}}, "required": ["issue_id"]}),
            ));
    }
    assert_eq!(tools.len(), 88);
    assert_eq!(
        detect_harness(ProtocolHint::Unknown, &tools),
        DetectedHarness::ClaudeCode
    );

    let cat = partition_tools_for_mcp(&tools, &mut NoopSpillSink);
    assert_eq!(cat.profile_id, "claude-code");
    assert!(cat.wire_tools.len() <= MAX_NATIVE_MCP_TOOLS);

    for core in [
        "REPL", "Bash", "Read", "Edit", "Write", "Grep", "Glob", "Agent", "LSP",
    ] {
        assert!(
            cat.by_mcp_name.contains_key(core),
            "expected Claude Code `{core}` to be 1:1 Direct Eager"
        );
    }
    assert!(
        cat.by_mcp_name.contains_key("AskUserQuestion"),
        "expected Slack-Slot Backfill to promote `AskUserQuestion` to 1:1 Direct Eager"
    );
    assert!(
        cat.by_mcp_name.contains_key("Skill"),
        "expected Slack-Slot Backfill to promote `Skill` to 1:1 Direct Eager"
    );

    for (expected_hub, expected_ns, expected_count) in [
        ("ns__tasks", "tasks", 7usize),
        ("ns__workflow", "workflow", 4),
        ("ns__teams", "teams", 3),
        ("ns__schedule", "schedule", 3),
        ("ns__mcp_resources", "mcp_resources", 2),
        ("ns__playwright", "playwright", 24),
        ("ns__linear", "linear", 30),
    ] {
        assert!(
            cat.virtualized_hubs.iter().any(|(h, ns, c)| {
                h == expected_hub && ns == expected_ns && *c == expected_count
            }),
            "missing expected Claude Code hub ({expected_hub}, {expected_ns}, {expected_count}); got {:?}",
            cat.virtualized_hubs
        );
    }

    let tasks_wire = cat
        .wire_tools
        .iter()
        .find(|w| w.name == "ns__tasks")
        .expect("ns__tasks wire tool");
    assert!(tasks_wire.input_schema["properties"]["action"]["enum"].is_array());

    match cat.routing_table.resolve_call(
        "ns__tasks",
        &json!({"action": "TaskCreate", "args": {"id": "t-1"}, "toolSummary": "Create task"}),
        &tools,
    ) {
        VirtualizedCallDecision::ForwardToClient {
            client_name,
            arguments,
            is_custom,
            ..
        } => {
            assert_eq!(client_name, "TaskCreate");
            assert!(!is_custom);
            assert_eq!(arguments, r#"{"id":"t-1"}"#);
        }
        VirtualizedCallDecision::LocalReply { .. }
        | VirtualizedCallDecision::GatewayWebFetch { .. }
        | VirtualizedCallDecision::ReadContextSpill { .. } => {
            panic!("valid TaskCreate hub call must forward to client")
        }
    }

    match cat.routing_table.resolve_call(
        "ns__playwright",
        &json!({"action": "browser_op_05", "args": {"selector": "#submit"}}),
        &tools,
    ) {
        VirtualizedCallDecision::ForwardToClient {
            client_name,
            arguments,
            ..
        } => {
            assert_eq!(
                client_name,
                "mcp__plugin_playwright_playwright__browser_op_05"
            );
            assert_eq!(arguments, r##"{"selector":"#submit"}"##);
        }
        VirtualizedCallDecision::LocalReply { .. }
        | VirtualizedCallDecision::GatewayWebFetch { .. }
        | VirtualizedCallDecision::ReadContextSpill { .. } => {
            panic!("valid playwright plugin hub call must forward to client")
        }
    }
}

#[test]
fn custom_zst_harness_profile_monomorphizes_and_preserves_profile_across_warm_rebuild() {
    #[derive(Clone, Copy, Debug, Default)]
    struct ToyHarnessProfile;

    impl HarnessToolProfile for ToyHarnessProfile {
        const ID: &'static str = "toy-harness";
        type Prepass = ();

        fn build_prepass(self, _tools: &[CanonicalToolSpec]) {}

        fn classify_tool<'a>(
            self,
            spec: &'a CanonicalToolSpec,
            (): &Self::Prepass,
        ) -> ToolPlacement<'a> {
            if spec.name == "kernel_eval" {
                ToolPlacement::CoreDirect { rank: 0 }
            } else if let Some(rest) = spec.name.strip_prefix("lsp_") {
                ToolPlacement::Namespaced {
                    namespace: Cow::Borrowed("lsp"),
                    action: Cow::Borrowed(rest),
                }
            } else {
                ToolPlacement::Standalone { backfill_rank: 5 }
            }
        }
    }

    let mut tools = vec![fn_tool(
        "kernel_eval",
        "Run persistent kernel.",
        json!({"type": "object", "properties": {"code": {"type": "string"}}}),
    )];
    for i in 0..40 {
        tools.push(fn_tool(
                &format!("lsp_op_{i:02}"),
                "LSP operation.",
                json!({"type": "object", "properties": {"uri": {"type": "string"}}, "required": ["uri"]}),
            ));
    }

    let cat = plan_virtualization(ToyHarnessProfile, &tools, &mut NoopSpillSink);
    assert_eq!(cat.profile_id, "toy-harness");
    assert!(!cat.is_passthrough);
    assert!(cat.by_mcp_name.contains_key("kernel_eval"));
    assert!(cat.virtualized_hubs.iter().any(|(h, _, _)| h == "ns__lsp"));

    tools.push(fn_tool(
        "lsp_op_99",
        "New mid-session LSP operation.",
        json!({"type": "object", "properties": {"uri": {"type": "string"}}, "required": ["uri"]}),
    ));
    let rebuilt = cat
        .routing_table
        .rebuild_if_changed(&tools)
        .expect("must detect dynamic tool change");
    assert_eq!(rebuilt.profile_id(), "toy-harness");

    match rebuilt.resolve_call(
        "ns__lsp",
        &json!({"action": "op_99", "args": {"uri": "file:///src/lib.rs"}}),
        &tools,
    ) {
        VirtualizedCallDecision::ForwardToClient { client_name, .. } => {
            assert_eq!(client_name, "lsp_op_99");
        }
        VirtualizedCallDecision::LocalReply { .. }
        | VirtualizedCallDecision::GatewayWebFetch { .. }
        | VirtualizedCallDecision::ReadContextSpill { .. } => {
            panic!("dynamically synced tool must route through custom profile's `ns__lsp` hub")
        }
    }
}

#[test]
fn extract_custom_tool_input_ignores_tool_summary_and_null() {
    assert_eq!(
        extract_custom_tool_input(
            &json!({"script": "console.log(42);", "toolSummary": "Execute JS"}),
            None
        ),
        "console.log(42);"
    );
    assert_eq!(
        extract_custom_tool_input(&json!({"toolSummary": "Execute JS"}), None),
        r#"{"toolSummary":"Execute JS"}"#
    );
    assert_eq!(extract_custom_tool_input(&Value::Null, None), "");
}

#[test]
fn zero_tool_session_with_gateway_web_fetch_hot_syncs_dynamic_tools_without_recreating_session() {
    let cat = partition_tools_for_mcp_session(
        ProtocolHint::OpenAiResponses,
        &[],
        true,
        &mut NoopSpillSink,
    );
    assert!(cat.has_virtualized_dispatch);
    assert!(cat.has_gateway_web_fetch);
    assert_eq!(cat.wire_tools.len(), 3);
    assert_eq!(
        cat.routing_table.resolve_call(
            GATEWAY_WEB_FETCH_MCP_NAME,
            &json!({ "url": "https://example.com/docs" }),
            &[],
        ),
        VirtualizedCallDecision::GatewayWebFetch {
            url: "https://example.com/docs".to_owned(),
            pattern: None,
        }
    );
    assert_eq!(
        cat.routing_table.resolve_call(
            GATEWAY_WEB_FETCH_MCP_NAME,
            &json!({ "url": "https://example.com/docs", "pattern": "rate limit" }),
            &[],
        ),
        VirtualizedCallDecision::GatewayWebFetch {
            url: "https://example.com/docs".to_owned(),
            pattern: Some("rate limit".to_owned()),
        }
    );

    let hot_tools = vec![
        CanonicalToolSpec {
            name: "apply_patch".to_owned(),
            description: Some("Apply V4A patch.".to_owned()),
            kind: CanonicalToolKind::Custom { format_json: None },
        },
        fn_tool(
            "exec_command",
            "Run shell command.",
            json!({"type":"object","properties":{"cmd":{"type":"string"}},"required":["cmd"]}),
        ),
    ];
    let rebuilt = cat
        .routing_table
        .rebuild_if_changed(&hot_tools)
        .expect("must detect hot-loaded tools");
    assert_eq!(
            rebuilt.resolve_call(
                CALL_CLIENT_TOOL_MCP_NAME,
                &json!({ "name": "apply_patch", "arguments": { "patch": "*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n*** End Patch" } }),
                &hot_tools,
            ),
            VirtualizedCallDecision::ForwardToClient {
                client_name: "apply_patch".to_owned(),
                arguments: "*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n*** End Patch".to_owned(),
                is_custom: true,
                patch_repaired: false,
            },
            "a valid patch behind the dispatcher envelope is not a repair"
        );
    match rebuilt.resolve_call(
        CALL_CLIENT_TOOL_MCP_NAME,
        &json!({ "name": "apply_patch", "arguments": { "patch": "```diff\n*** Update File: a.rs\n@@\n-x\n+y\n```" } }),
        &hot_tools,
    ) {
        VirtualizedCallDecision::ForwardToClient {
            arguments,
            patch_repaired,
            ..
        } => {
            assert!(patch_repaired, "a fenced patch behind the dispatcher is repaired");
            assert!(arguments.starts_with("*** Begin Patch"), "got: {arguments}");
        }
        other => panic!("expected the repaired patch to be forwarded, got {other:?}"),
    }
}

#[test]
fn opencode_profile_virtualizes_namespaces_mcps_backfills_and_heals_patchtext() {
    let mut tools = vec![
        fn_tool(
            "execute",
            "Run JavaScript in a confined Code Mode runtime to script tool calls and HTTP requests.",
            json!({
                "type": "object",
                "properties": { "code": { "type": "string" } },
                "required": ["code"]
            }),
        ),
        fn_tool(
            "shell",
            "Execute shell command",
            json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}),
        ),
        fn_tool(
            "read",
            "Read file",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        ),
        fn_tool(
            "edit",
            "Edit file",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        ),
        fn_tool(
            "write",
            "Write file",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        ),
        fn_tool(
            "patch",
            "Apply V4A patch",
            json!({"type":"object","properties":{"patchText":{"type":"string"}},"required":["patchText"]}),
        ),
        fn_tool(
            "grep",
            "Search file contents",
            json!({"type":"object","properties":{"pattern":{"type":"string"}},"required":["pattern"]}),
        ),
        fn_tool(
            "glob",
            "Find files",
            json!({"type":"object","properties":{"pattern":{"type":"string"}},"required":["pattern"]}),
        ),
        fn_tool(
            "subagent",
            "Spawn subagent",
            json!({"type":"object","properties":{"prompt":{"type":"string"}},"required":["prompt"]}),
        ),
        fn_tool(
            "question",
            "Ask user question",
            json!({"type":"object","properties":{"questions":{"type":"array"}},"required":["questions"]}),
        ),
        fn_tool(
            "skill",
            "Load skill",
            json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}),
        ),
        fn_tool(
            "webfetch",
            "Fetch URL",
            json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}),
        ),
        fn_tool(
            "opencode_session_search",
            "Search sessions",
            json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}),
        ),
        fn_tool(
            "opencode_session_read",
            "Read session",
            json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}),
        ),
        fn_tool(
            "opencode_mcp_resource_list",
            "List MCP resources",
            json!({"type":"object","properties":{}}),
        ),
        fn_tool(
            "artifact_create",
            "Create artifact",
            json!({"type":"object","properties":{"title":{"type":"string"}},"required":["title"]}),
        ),
        fn_tool(
            "automation_create",
            "Create automation",
            json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}),
        ),
        fn_tool(
            "automation_list",
            "List automations",
            json!({"type":"object","properties":{}}),
        ),
    ];
    for idx in 0..32 {
        tools.push(fn_tool(
                &format!("playwright_browser_op_{idx:02}"),
                "Playwright browser operation",
                json!({"type":"object","properties":{"selector":{"type":"string"}},"required":["selector"]}),
            ));
        tools.push(fn_tool(
            &format!("github_repo_op_{idx:02}"),
            "GitHub repository operation",
            json!({"type":"object","properties":{"repo":{"type":"string"}},"required":["repo"]}),
        ));
    }

    assert_eq!(
        detect_harness(ProtocolHint::AnthropicMessages, &tools),
        DetectedHarness::OpenCode
    );
    let mut spill = NoopSpillSink;
    let catalog =
        partition_tools_for_mcp_with_hint(ProtocolHint::AnthropicMessages, &tools, &mut spill);
    assert_eq!(catalog.profile_id, "opencode");
    assert!(catalog.has_virtualized_dispatch);
    assert!(catalog.wire_tools.len() <= VirtualizerBudget::OPENCODE_AGY_ACP.max_eager_tools);

    let wire_names: HashSet<&str> = catalog.wire_tools.iter().map(|w| w.name.as_str()).collect();
    for expected in [
        "execute",
        "shell",
        "read",
        "edit",
        "write",
        "patch",
        "grep",
        "glob",
        "subagent",
        "question",
        "skill",
        "ns__session",
        "ns__opencode",
        "ns__automation",
        "ns__playwright",
        "ns__github",
        SEARCH_CLIENT_TOOLS_MCP_NAME,
        CALL_CLIENT_TOOL_MCP_NAME,
    ] {
        assert!(
            wire_names.contains(expected),
            "expected `{expected}` in OpenCode Eager wire tools, got: {wire_names:?}"
        );
    }

    // Verify OpenCode `patch` (`{ "patchText": "..." }`) auto-heals duplicate file blocks & blank lines
    let unhealed_patch = "*** Begin Patch\n*** Update File: src/lib.rs\n@@ -1,3 +1,3 @@\n fn a() {\n\n-    old();\n+    new();\n }\n*** Update File: src/lib.rs\n@@\n-fn b() {}\n+fn b_v2() {}\n*** End Patch";
    match catalog.routing_table.resolve_call(
        "patch",
        &json!({ "patchText": unhealed_patch, "toolSummary": "Patch lib.rs" }),
        &tools,
    ) {
        VirtualizedCallDecision::ForwardToClient {
            client_name,
            arguments,
            is_custom,
            ..
        } => {
            assert_eq!(client_name, "patch");
            assert!(!is_custom);
            let parsed: Value = serde_json::from_str(&arguments).expect("valid JSON args");
            let healed = parsed["patchText"].as_str().expect("patchText string");
            assert_eq!(healed.matches("*** Update File: src/lib.rs").count(), 1);
            assert!(!healed.contains("@@ -1,3 +1,3 @@"));
            assert!(parsed.get("toolSummary").is_none());
        }
        other => panic!("unexpected decision: {other:?}"),
    }

    // Malformed `patchText` missing hunk line prefix fails 0-RTT preflight in-memory
    match catalog.routing_table.resolve_call(
            "patch",
            &json!({ "patchText": "*** Begin Patch\n*** Update File: src/lib.rs\n@@\noops_missing_prefix()\n*** End Patch" }),
            &tools,
        ) {
            VirtualizedCallDecision::LocalReply {
                is_error: true,
                kind: LocalReplyKind::PreflightError,
                ..
            } => {}
            other => panic!("expected 0-RTT preflight error on bad patchText, got: {other:?}"),
        }

    // `<server>_<tool>` hub dispatches back to exact OpenCode `playwright_browser_op_04`
    assert_eq!(
        catalog.routing_table.resolve_call(
            "ns__playwright",
            &json!({ "action": "browser_op_04", "args": { "selector": "#login" } }),
            &tools,
        ),
        VirtualizedCallDecision::ForwardToClient {
            client_name: "playwright_browser_op_04".to_owned(),
            arguments: "{\"selector\":\"#login\"}".to_owned(),
            is_custom: false,
            patch_repaired: false,
        }
    );
}

#[test]
fn generic_and_non_opencode_patch_tools_forward_unified_diffs_verbatim() {
    let unified_diff = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,3 @@\n fn main() {\n-    println!(\"old\");\n+    println!(\"new\");\n }\n";

    // 1. Generic Function `patch` tool with `{ "patch": "<unified diff>" }` (direct + virtualized)
    let mut fn_tools = vec![fn_tool(
        "patch",
        "Apply a standard unified diff patch to workspace files.",
        json!({
            "type": "object",
            "properties": { "patch": { "type": "string" } },
            "required": ["patch"]
        }),
    )];
    for idx in 0..40 {
        fn_tools.push(fn_tool(
            &format!("extra_tool_{idx:02}"),
            "Extra tool",
            json!({"type": "object", "properties": {"x": {"type": "string"}}}),
        ));
    }

    assert_eq!(
        detect_harness(ProtocolHint::OpenAiResponses, &fn_tools[..1]),
        DetectedHarness::Codex
    );
    assert_eq!(
        detect_harness(ProtocolHint::AnthropicMessages, &fn_tools[..1]),
        DetectedHarness::ClaudeCode
    );
    assert_eq!(
        detect_harness(ProtocolHint::Unknown, &fn_tools[..1]),
        DetectedHarness::Generic
    );

    let mut spill = NoopSpillSink;
    let direct_cat =
        partition_tools_for_mcp_with_hint(ProtocolHint::Unknown, &fn_tools[..1], &mut spill);
    assert_eq!(direct_cat.profile_id, "generic");
    let expected_json = json!({ "patch": unified_diff }).to_string();
    assert_eq!(
        direct_cat.routing_table.resolve_call(
            "patch",
            &json!({ "patch": unified_diff }),
            &fn_tools[..1],
        ),
        VirtualizedCallDecision::ForwardToClient {
            client_name: "patch".to_owned(),
            arguments: expected_json.clone(),
            is_custom: false,
            patch_repaired: false,
        }
    );

    let virt_cat = partition_tools_for_mcp_with_hint(ProtocolHint::Unknown, &fn_tools, &mut spill);
    assert_eq!(
        virt_cat.routing_table.resolve_call(
            CALL_CLIENT_TOOL_MCP_NAME,
            &json!({ "name": "patch", "arguments": { "patch": unified_diff } }),
            &fn_tools,
        ),
        VirtualizedCallDecision::ForwardToClient {
            client_name: "patch".to_owned(),
            arguments: expected_json.clone(),
            is_custom: false,
            patch_repaired: false,
        }
    );

    // 2. Generic Custom freeform `patch` tool with unified diff payload
    let custom_patch_tools = vec![CanonicalToolSpec {
        name: "patch".to_owned(),
        description: Some("Apply patch text directly".to_owned()),
        kind: CanonicalToolKind::Custom { format_json: None },
    }];
    let custom_cat = partition_tools_for_mcp_with_hint(
        ProtocolHint::OpenAiResponses,
        &custom_patch_tools,
        &mut spill,
    );
    assert_eq!(custom_cat.profile_id, "codex");
    assert_eq!(
        custom_cat.routing_table.resolve_call(
            "patch",
            &json!({ "input": unified_diff }),
            &custom_patch_tools,
        ),
        VirtualizedCallDecision::ForwardToClient {
            client_name: "patch".to_owned(),
            arguments: unified_diff.to_owned(),
            is_custom: true,
            patch_repaired: false,
        }
    );

    // 3. Even inside an OpenCode session, a non-V4A MCP/custom `patch` tool without `patchText`
    // or `*** Begin Patch` forwards unified diffs verbatim instead of rewriting to V4A!
    let oc_with_custom_patch = vec![
        fn_tool(
            "execute",
            "Run JavaScript in a confined Code Mode runtime.",
            json!({"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"]}),
        ),
        fn_tool(
            "patch",
            "Apply unified diff via custom plugin",
            json!({"type": "object", "properties": {"patch": {"type": "string"}}, "required": ["patch"]}),
        ),
    ];
    let oc_cat =
        partition_tools_for_harness(DetectedHarness::OpenCode, &oc_with_custom_patch, &mut spill);
    assert_eq!(
        oc_cat.routing_table.resolve_call(
            "patch",
            &json!({ "patch": unified_diff }),
            &oc_with_custom_patch,
        ),
        VirtualizedCallDecision::ForwardToClient {
            client_name: "patch".to_owned(),
            arguments: expected_json,
            is_custom: false,
            patch_repaired: false,
        }
    );
}

#[test]
fn omp_default_16_builtin_tools_elide_subordinates_into_eval_with_full_prelude() {
    let mut spill = NoopSpillSink;
    let eval_prelude = format!(
        "Run persistent Python (`py`) or JavaScript (`js`) cells; top-level state persists.\n<prelude>\nawait tool.read({{ path: 'src/lib.rs' }});\n</prelude>\n<browser>\n{}</browser>",
        "page.goto('https://example.com');\n".repeat(320)
    );
    assert!(eval_prelude.len() > 10_000);

    let mut tools = vec![
        fn_tool(
            "eval",
            &eval_prelude,
            json!({
                "type": "object",
                "properties": {
                    "language": { "type": "string", "enum": ["py", "js"] },
                    "code": { "type": "string" }
                },
                "required": ["language", "code"]
            }),
        ),
        fn_tool(
            "wait",
            "Wait for background jobs",
            json!({ "type": "object", "properties": { "jobs": { "type": "array" } } }),
        ),
        fn_tool(
            "edit",
            "Edit files in place using hashline envelopes (`*** Begin Patch` + `[path#TAG]`).",
            json!({
                "type": "object",
                "properties": { "input": { "type": "string" } },
                "required": ["input"]
            }),
        ),
    ];
    for name in [
        "read",
        "write",
        "bash",
        "grep",
        "glob",
        "find",
        "task",
        "todo",
        "ask",
        "web_search",
        "lsp",
        "ast_edit",
        "debug",
    ] {
        tools.push(fn_tool(
            name,
            &format!("OMP built-in {name} tool"),
            json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        ));
    }
    assert_eq!(tools.len(), 16);

    assert_eq!(
        detect_harness(ProtocolHint::OpenAiResponses, &tools),
        DetectedHarness::Omp
    );
    let cat =
        partition_tools_for_mcp_session(ProtocolHint::OpenAiResponses, &tools, true, &mut spill);
    assert!(
        cat.virtualized_hubs.is_empty(),
        "16 default omp tools must bypass hub folding"
    );
    let bridged_names: Vec<&str> = cat.bridged_pairs.iter().map(|(c, _)| c.as_str()).collect();
    assert_eq!(
        bridged_names,
        vec!["eval", "wait", "edit", "write", "task", "ask", "ast_edit"],
        "when OMP `eval` Code Mode is active, only orchestrators and direct mutation/interactive tools mount top-level"
    );
    let wire_eval = cat
        .wire_tools
        .iter()
        .find(|t| t.name == "eval")
        .expect("eval mounted 1:1");
    let wire_eval_desc = wire_eval.description.as_deref().unwrap_or_default();
    assert!(
        wire_eval_desc.starts_with(eval_prelude.trim()),
        "64 KiB orchestrator budget must preserve <prelude> and <browser> docs intact"
    );
    assert!(
        wire_eval_desc.contains("<eval_session_tools>")
            && wire_eval_desc.contains("`tool.read`")
            && wire_eval_desc.contains("`tool.bash`")
            && wire_eval_desc.contains("`tool.grep`"),
        "wire eval description must append folded session tool signatures"
    );
    let search_out = cat.routing_table.resolve_call(
        "search_client_tools",
        &json!({ "names": ["grep"] }),
        &tools,
    );
    match search_out {
        VirtualizedCallDecision::LocalReply { text, is_error, .. } => {
            assert!(!is_error);
            assert!(
                text.contains("`mcp_lg_eval` cell via `await tool.grep({ ... })`"),
                "search_client_tools must guide folded tools into `mcp_lg_eval`, got:\n{text}"
            );
        }
        other => panic!("expected LocalReply from search_client_tools, got {other:?}"),
    }
}

#[test]
fn omp_large_toolset_virtualizes_mcp_and_memory_while_preserving_hashline_edit() {
    let mut spill = NoopSpillSink;
    let mut tools = vec![
        fn_tool(
            "eval",
            "Persistent kernels (`py` / `js`); call `await tool.bash(...)`.\n<prelude>helpers</prelude>",
            json!({
                "type": "object",
                "properties": {
                    "language": { "type": "string", "enum": ["py", "js"] },
                    "code": { "type": "string" }
                },
                "required": ["language", "code"]
            }),
        ),
        fn_tool(
            "wait",
            "Wait for background jobs",
            json!({"type": "object"}),
        ),
        fn_tool("bash", "Execute shell commands", json!({"type": "object"})),
        fn_tool(
            "read",
            "Read files and omp:// URLs",
            json!({"type": "object"}),
        ),
        fn_tool("write", "Write files", json!({"type": "object"})),
        fn_tool(
            "edit",
            "Hashline edit (`*** Begin Patch` + `[path#TAG]` + `PUT`)",
            json!({
                "type": "object",
                "properties": { "input": { "type": "string" } },
                "required": ["input"]
            }),
        ),
        fn_tool(
            "apply_patch",
            "OpenAI V4A `*** Begin Patch` / `*** Update File:` patch tool",
            json!({
                "type": "object",
                "properties": { "input": { "type": "string" } },
                "required": ["input"]
            }),
        ),
        fn_tool("grep", "Regex search", json!({"type": "object"})),
        fn_tool("glob", "Glob search", json!({"type": "object"})),
        fn_tool("task", "Subagents", json!({"type": "object"})),
        fn_tool("lsp", "LSP intelligence", json!({"type": "object"})),
        fn_tool("todo", "Todo tracker", json!({"type": "object"})),
        fn_tool("ask", "Ask user", json!({"type": "object"})),
    ];
    for mem in ["retain", "recall", "reflect", "memory_edit", "learn"] {
        tools.push(fn_tool(
            mem,
            "Long-term memory op",
            json!({"type": "object"}),
        ));
    }
    for st in ["checkpoint", "rewind", "context_notes", "new_context"] {
        tools.push(fn_tool(st, "Session state op", json!({"type": "object"})));
    }
    for irc in ["irc_send", "irc_join", "irc_history"] {
        tools.push(fn_tool(irc, "IRC collab op", json!({"type": "object"})));
    }
    for idx in 0..20 {
        tools.push(fn_tool(
            &format!("mcp__github_issue_op_{idx:02}"),
            "GitHub MCP op",
            json!({
                "type": "object",
                "properties": { "repo": { "type": "string" } },
                "required": ["repo"]
            }),
        ));
        tools.push(fn_tool(
            &format!("mcp__google_drive_file_op_{idx:02}"),
            "Google Drive MCP op",
            json!({
                "type": "object",
                "properties": { "fileId": { "type": "string" } },
                "required": ["fileId"]
            }),
        ));
    }

    let cat =
        partition_tools_for_mcp_session(ProtocolHint::OpenAiResponses, &tools, true, &mut spill);
    assert!(cat.has_virtualized_dispatch);
    assert!(cat.wire_tools.len() <= VirtualizerBudget::OMP_AGY_ACP.max_eager_tools);

    let wire_names: Vec<&str> = cat.wire_tools.iter().map(|t| t.name.as_str()).collect();
    for direct in [
        "eval",
        "wait",
        "edit",
        "apply_patch",
        "write",
        "task",
        "ask",
    ] {
        assert!(
            wire_names.contains(&direct),
            "expected `{direct}` to remain Tier 0/1 Direct Eager, got {wire_names:?}"
        );
    }
    for elided in ["bash", "read", "grep", "glob", "lsp", "todo"] {
        assert!(
            !wire_names.contains(&elided),
            "expected `{elided}` to be folded into `eval` when OMP Code Mode is active, got {wire_names:?}"
        );
    }
    let eval_wire = cat
        .wire_tools
        .iter()
        .find(|t| t.name == "eval")
        .expect("eval wire tool");
    let eval_desc = eval_wire.description.as_deref().unwrap_or_default();
    assert!(
        eval_desc.contains("<eval_session_tools>")
            && eval_desc.contains("`tool.read`")
            && eval_desc.contains("`tool.bash`"),
        "eval wire description must summarize folded session tools:\n{eval_desc}"
    );
    for hub in [
        "ns__memory",
        "ns__session_state",
        "ns__irc",
        "ns__github",
        "ns__google_drive",
    ] {
        assert!(
            wire_names.contains(&hub),
            "expected hub `{hub}` in wire_tools, got {wire_names:?}"
        );
    }

    // 1. Hashline `edit` MUST pass through untouched without V4A preflight rejection!
    let hashline_payload =
        "*** Begin Patch\n[src/main.rs#A1B2]\nPUT 12.=14:\n    println!(\"ok\");\n*** End Patch";
    assert_eq!(
        cat.routing_table.resolve_call(
            "edit",
            &json!({ "input": hashline_payload, "toolSummary": "Edit main.rs" }),
            &tools,
        ),
        VirtualizedCallDecision::ForwardToClient {
            client_name: "edit".to_owned(),
            arguments: json!({ "input": hashline_payload }).to_string(),
            is_custom: false,
            patch_repaired: false,
        }
    );

    // 2. Custom Lark-grammar `edit` also unwraps `{ input }` verbatim without V4A rejection!
    let custom_edit_tools = vec![
        tools[0].clone(),
        CanonicalToolSpec {
            name: "edit".to_owned(),
            description: Some("Custom Lark hashline grammar".to_owned()),
            kind: CanonicalToolKind::Custom {
                format_json: Some(r#"{"type":"grammar","syntax":"lark"}"#.to_owned()),
            },
        },
    ];
    let custom_cat =
        partition_tools_for_harness(DetectedHarness::Omp, &custom_edit_tools, &mut spill);
    assert_eq!(
        custom_cat.routing_table.resolve_call(
            "edit",
            &json!({ "input": hashline_payload }),
            &custom_edit_tools,
        ),
        VirtualizedCallDecision::ForwardToClient {
            client_name: "edit".to_owned(),
            arguments: hashline_payload.to_owned(),
            is_custom: true,
            patch_repaired: false,
        }
    );

    // 3. `apply_patch` (`input` field) DOES heal V4A duplicate file sections & reject bad hunks!
    let bad_v4a =
        "*** Begin Patch\n*** Update File: src/lib.rs\n@@\nbad_unprefixed_line\n*** End Patch";
    assert!(matches!(
        cat.routing_table
            .resolve_call("apply_patch", &json!({ "input": bad_v4a }), &tools),
        VirtualizedCallDecision::LocalReply {
            is_error: true,
            kind: LocalReplyKind::PreflightError,
            ..
        }
    ));

    // 4. Two-segment single-underscore MCP server (`mcp__google_drive_file_op_03`) routes cleanly!
    assert_eq!(
        cat.routing_table.resolve_call(
            "ns__google_drive",
            &json!({
                "action": "file_op_03",
                "args": { "fileId": "doc-77" }
            }),
            &tools,
        ),
        VirtualizedCallDecision::ForwardToClient {
            client_name: "mcp__google_drive_file_op_03".to_owned(),
            arguments: json!({ "fileId": "doc-77" }).to_string(),
            is_custom: false,
            patch_repaired: false,
        }
    );
    assert_eq!(
        cat.orchestrator_contract(),
        Some(
            liftgate_types::session_key::McpOrchestratorContract::OmpPolyglotEval {
                mcp_name: "eval",
                elided_subordinate_tools: true,
            }
        )
    );

    // 5. When `eval` is disabled (`lg omp --no-code-mode`), subordinate tools remain direct
    //    1:1 Eager MCP tools and OMP detection still succeeds even with empty descriptions!
    let no_code_mode_tools = vec![
        fn_tool(
            "bash",
            "",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "pty": { "type": "boolean" },
                    "ready": { "type": "object" }
                },
                "required": ["command"]
            }),
        ),
        fn_tool(
            "read",
            "",
            json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        ),
        fn_tool(
            "grep",
            "",
            json!({ "type": "object", "properties": { "pattern": { "type": "string" } } }),
        ),
        fn_tool(
            "glob",
            "",
            json!({ "type": "object", "properties": { "pattern": { "type": "string" } } }),
        ),
        fn_tool(
            "lsp",
            "",
            json!({ "type": "object", "properties": { "action": { "type": "string" } } }),
        ),
        fn_tool(
            "todo",
            "",
            json!({
                "type": "object",
                "properties": { "op": { "type": "string" }, "items": { "type": "array" } }
            }),
        ),
    ];
    assert_eq!(
        detect_harness(ProtocolHint::OpenAiResponses, &no_code_mode_tools),
        DetectedHarness::Omp
    );
    let no_cm_cat = partition_tools_for_mcp_session(
        ProtocolHint::OpenAiResponses,
        &no_code_mode_tools,
        false,
        &mut spill,
    );
    let no_cm_names: Vec<&str> = no_cm_cat
        .wire_tools
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    for direct in ["bash", "read", "grep", "glob", "lsp", "todo"] {
        assert!(
            no_cm_names.contains(&direct),
            "expected `{direct}` to stay direct when `eval` is disabled, got {no_cm_names:?}"
        );
    }
    assert_eq!(no_cm_cat.orchestrator_contract(), None);
}

#[test]
fn codex_with_single_underscore_mcp_integrations_stays_codex_and_generic_eval_has_no_omp_contract()
{
    let mut spill = NoopSpillSink;
    let mut tools = vec![
        fn_tool(
            "exec_command",
            "Run a shell command in a PTY",
            json!({ "type": "object", "properties": { "cmd": { "type": "string" } } }),
        ),
        fn_tool(
            "write_stdin",
            "Write bytes to an existing PTY session",
            json!({ "type": "object", "properties": { "session_id": { "type": "integer" } } }),
        ),
        fn_tool(
            "eval",
            "Evaluate a simple math expression",
            json!({ "type": "object", "properties": { "expression": { "type": "string" } } }),
        ),
    ];
    for op in [
        "issue_list",
        "issue_get",
        "issue_create",
        "issue_update",
        "issue_close",
        "pr_list",
    ] {
        tools.push(fn_tool(
            &format!("mcp__github_{op}"),
            &format!("GitHub MCP {op}"),
            json!({ "type": "object", "properties": { "repo": { "type": "string" } } }),
        ));
    }

    assert_eq!(
        detect_harness(ProtocolHint::OpenAiResponses, &tools),
        DetectedHarness::Codex,
        "single-underscore MCP integrations must never flip a Codex session into OMP"
    );
    let cat =
        partition_tools_for_mcp_session(ProtocolHint::OpenAiResponses, &tools, true, &mut spill);
    assert_eq!(cat.profile_id, "codex");
    assert_eq!(
        cat.orchestrator_contract(),
        None,
        "generic `eval(expression)` must not emit an OMP polyglot eval prompt contract"
    );
}

#[test]
fn opencode_profile_emits_full_and_partial_execute_orchestrator_contracts() {
    use liftgate_types::session_key::McpOrchestratorContract;

    let mut spill = NoopSpillSink;

    // 1. OpenCode v2 `full_codemode` (`question`, `skill`, `subagent`, `execute` with `timeout` & `background`)
    let full_tools = vec![
        fn_tool(
            "question",
            "Ask user questions",
            json!({ "type": "object", "properties": { "questions": { "type": "array" } } }),
        ),
        fn_tool(
            "skill",
            "Load skill",
            json!({ "type": "object", "properties": { "id": { "type": "string" } } }),
        ),
        fn_tool(
            "subagent",
            "Spawn child subagent",
            json!({ "type": "object", "properties": { "prompt": { "type": "string" } } }),
        ),
        fn_tool(
            "execute",
            "Run JavaScript in a confined Code Mode runtime to work with files and data (`Bun.file`, `Bun.write`, `Bun.Glob`).",
            json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string" },
                    "timeout": { "type": "integer" },
                    "background": { "type": "boolean" }
                },
                "required": ["code"]
            }),
        ),
    ];
    assert_eq!(
        detect_harness(ProtocolHint::AnthropicMessages, &full_tools),
        DetectedHarness::OpenCode
    );
    let full_cat = partition_tools_for_mcp_session(
        ProtocolHint::AnthropicMessages,
        &full_tools,
        true,
        &mut spill,
    );
    assert_eq!(full_cat.profile_id, "opencode");
    assert_eq!(
        full_cat.orchestrator_contract(),
        Some(McpOrchestratorContract::OpenCodeExecute {
            mcp_name: "execute",
            full_code_mode: true,
        })
    );

    // 2. Partial / MCP-only Code Mode (`execute` with `{ code }` alongside direct `shell` & `read`)
    let partial_tools = vec![
        fn_tool(
            "shell",
            "Run shell command",
            json!({ "type": "object", "properties": { "command": { "type": "string" } } }),
        ),
        fn_tool(
            "read",
            "Read file",
            json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        ),
        fn_tool(
            "execute",
            "Run JavaScript in a confined Code Mode runtime to script tool calls.",
            json!({
                "type": "object",
                "properties": { "code": { "type": "string" } },
                "required": ["code"]
            }),
        ),
    ];
    let partial_cat = partition_tools_for_mcp_session(
        ProtocolHint::AnthropicMessages,
        &partial_tools,
        true,
        &mut spill,
    );
    assert_eq!(
        partial_cat.orchestrator_contract(),
        Some(McpOrchestratorContract::OpenCodeExecute {
            mcp_name: "execute",
            full_code_mode: false,
        })
    );
}

#[test]
fn omp_eval_typed_signatures_intent_backfill_direct_preflight_and_spill_reader() {
    let mut spill = NoopSpillSink;
    let tools = vec![
        fn_tool(
            "eval",
            "Persistent kernels (`py` / `js`); call `await tool.read(...)`.\n<prelude>helpers</prelude>",
            json!({
                "type": "object",
                "properties": {
                    "i": { "type": "string" },
                    "language": { "type": "string", "enum": ["py", "js"] },
                    "code": { "type": "string" }
                },
                "required": ["i", "language", "code"]
            }),
        ),
        fn_tool(
            "edit",
            "Hashline edit (`[FILENAME#TAG]`).\nexec tool declaration:\n```ts\ndeclare function edit(args: { i: string; path: string; edits: EditOp[] }): Promise<string>;\n```",
            json!({
                "type": "object",
                "properties": {
                    "i": { "type": "string" },
                    "path": { "type": "string" },
                    "edits": { "type": "array", "items": { "type": "object" } },
                    "rename": { "type": ["string", "null"] }
                },
                "required": ["i", "path", "edits", "rename"]
            }),
        ),
        fn_tool(
            "todo",
            "Manage structured task list",
            json!({
                "type": "object",
                "properties": {
                    "i": { "type": "string" },
                    "op": { "type": "string", "enum": ["write", "update", "list"] },
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string" },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                            },
                            "required": ["content"]
                        }
                    }
                },
                "required": ["i", "op"]
            }),
        ),
        fn_tool(
            "read",
            "Read file with hashline tags",
            json!({
                "type": "object",
                "properties": {
                    "i": { "type": "string" },
                    "path": { "type": "string" },
                    "limit": { "type": "integer" }
                },
                "required": ["i", "path"]
            }),
        ),
    ];

    let cat = partition_tools_for_mcp_session_with_options(
        ProtocolHint::OpenAiResponses,
        &tools,
        McpSessionOptions {
            enable_gateway_web_fetch: true,
            enable_context_spill_reader: true,
        },
        &mut spill,
    );
    assert_eq!(cat.profile_id, "omp");
    assert!(cat.has_context_spill_reader);
    assert!(
        cat.wire_tools
            .iter()
            .any(|w| w.name == READ_CONTEXT_SPILL_MCP_NAME)
    );

    // 1. `<eval_session_tools>` includes compact TypeScript types & union enums for folded `todo` and `read`
    let eval_desc = cat
        .wire_tools
        .iter()
        .find(|w| w.name == "eval")
        .and_then(|w| w.description.as_deref())
        .unwrap_or_default();
    assert!(
        eval_desc.contains("`tool.todo`({ i: string, op: \"write\" | \"update\" | \"list\"")
            && eval_desc.contains("items?: { content: string, status?: \"pending\" | \"in_progress\" | \"completed\" }[]"),
        "folded todo signature must render TypeScript types and enums:\n{eval_desc}"
    );
    assert!(
        eval_desc.contains("`tool.read`({ i: string, path: string, limit?: number })"),
        "folded read signature must render TypeScript types:\n{eval_desc}"
    );

    // 2. Direct bridged `edit` auto-backfills `"i"` from `"toolSummary"`, strips `"toolSummary"`,
    //    and permits omitted/null OpenAI-strict nullable `rename: ["string", "null"]`!
    match cat.routing_table.resolve_call(
        "edit",
        &json!({
            "path": "src/main.rs",
            "edits": [{ "op": "put", "loc": "1.=1", "content": "fn main() {}" }],
            "toolSummary": "Update main.rs entry point"
        }),
        &tools,
    ) {
        VirtualizedCallDecision::ForwardToClient {
            client_name,
            arguments,
            is_custom,
            ..
        } => {
            assert_eq!(client_name, "edit");
            assert!(!is_custom);
            let parsed: Value = serde_json::from_str(&arguments).unwrap();
            assert_eq!(parsed["i"], "Update main.rs entry point");
            assert!(parsed.get("toolSummary").is_none());
            assert_eq!(parsed["path"], "src/main.rs");
        }
        other => panic!("expected ForwardToClient with backfilled `i`, got {other:?}"),
    }

    // 3. Direct bridged `edit` missing non-nullable required `edits` fails in 0-RTT with schema!
    match cat.routing_table.resolve_call(
        "edit",
        &json!({
            "path": "src/main.rs",
            "toolSummary": "Incomplete edit call"
        }),
        &tools,
    ) {
        VirtualizedCallDecision::LocalReply {
            text,
            is_error: true,
            kind: LocalReplyKind::PreflightError,
        } => {
            assert!(text.contains("Missing required parameter(s) [edits] for `edit`"));
        }
        other => panic!("expected 0-RTT PreflightError for missing `edits`, got {other:?}"),
    }

    // 4. `search_client_tools` surfaces `codeModeTs:` in OMP `eval` sessions
    match cat.routing_table.resolve_call(
        SEARCH_CLIENT_TOOLS_MCP_NAME,
        &json!({ "names": ["edit"] }),
        &tools,
    ) {
        VirtualizedCallDecision::LocalReply {
            text,
            is_error: false,
            kind: LocalReplyKind::Search,
        } => {
            assert!(
                text.contains("codeModeTs:") && text.contains("declare function edit"),
                "OMP search_client_tools must include codeModeTs when eval is active:\n{text}"
            );
        }
        other => panic!("expected Search reply, got {other:?}"),
    }

    // 5. `read_context_spill` resolves to 0-RTT `VirtualizedCallDecision::ReadContextSpill` (including `byte_offset`)
    assert_eq!(
        cat.routing_table.resolve_call(
            READ_CONTEXT_SPILL_MCP_NAME,
            &json!({
                "path": "/Users/dev/.liftgate/spills/tool_result_exec-abcd1234.txt",
                "start_line": 120,
                "byte_offset": 8750,
                "max_lines": 80,
                "query": "planner"
            }),
            &tools,
        ),
        VirtualizedCallDecision::ReadContextSpill {
            path: "/Users/dev/.liftgate/spills/tool_result_exec-abcd1234.txt".to_owned(),
            start_line: 120,
            byte_offset: 8750,
            max_lines: 80,
            query: Some("planner".to_owned()),
        }
    );

    // 6. Warm direct-tool schema mutation refreshes `direct_by_mcp` preflight metadata!
    let mut updated_tools = tools.clone();
    updated_tools[1] = fn_tool(
        "edit",
        "Hashline edit with optional `edits` on warm schema update",
        json!({
            "type": "object",
            "properties": {
                "i": { "type": "string" },
                "path": { "type": "string" },
                "edits": { "type": "array", "items": { "type": "object" } }
            },
            "required": ["i", "path"]
        }),
    );
    let rebuilt = cat
        .routing_table
        .rebuild_if_changed(&updated_tools)
        .expect("must detect warm direct-tool schema update");
    match rebuilt.resolve_call(
        "edit",
        &json!({
            "path": "src/main.rs",
            "toolSummary": "Schema updated so `edits` is now optional"
        }),
        &updated_tools,
    ) {
        VirtualizedCallDecision::ForwardToClient {
            client_name,
            arguments,
            is_custom: false,
            ..
        } => {
            assert_eq!(client_name, "edit");
            let parsed: Value = serde_json::from_str(&arguments).unwrap();
            assert_eq!(parsed["path"], "src/main.rs");
            assert_eq!(parsed["i"], "Schema updated so `edits` is now optional");
        }
        other => panic!(
            "warm direct-tool schema update must refresh required-field preflight, got {other:?}"
        ),
    }

    // 7. Unconstrained `{}` or `$ref` required properties permit explicit `null` values (`{"payload": null}`)!
    let unconstrained_tools = vec![fn_tool(
        "emit_event",
        "Emit structured event with unconstrained JSON payload",
        json!({
            "type": "object",
            "properties": {
                "payload": {},
                "meta": { "$ref": "#/$defs/Meta" }
            },
            "required": ["payload", "meta"]
        }),
    )];
    let unconst_cat =
        partition_tools_for_mcp_with_hint(ProtocolHint::Unknown, &unconstrained_tools, &mut spill);
    assert_eq!(
        unconst_cat.routing_table.resolve_call(
            "emit_event",
            &json!({ "payload": null, "meta": null }),
            &unconstrained_tools,
        ),
        VirtualizedCallDecision::ForwardToClient {
            client_name: "emit_event".to_owned(),
            arguments: r#"{"payload":null,"meta":null}"#.to_owned(),
            is_custom: false,
            patch_repaired: false,
        }
    );
    assert!(matches!(
        unconst_cat
            .routing_table
            .resolve_call("emit_event", &json!({}), &unconstrained_tools,),
        VirtualizedCallDecision::LocalReply {
            is_error: true,
            kind: LocalReplyKind::PreflightError,
            ..
        }
    ));
}

#[test]
fn calibrated_budget_enables_30_tool_direct_passthrough_and_codex_claude_contracts() {
    use liftgate_types::session_key::McpOrchestratorContract;

    let mut spill = NoopSpillSink;

    // 1. Medium 30-tool Codex session (`<= 36` tools, `<= 28 KiB`) stays 100% 1:1 passthrough (`0` hubs)
    let mut medium_codex = vec![
        CanonicalToolSpec {
            name: "exec".to_owned(),
            description: Some("Run JavaScript in Code Mode (`declare const tools`).".to_owned()),
            kind: CanonicalToolKind::Custom { format_json: None },
        },
        fn_tool(
            "wait",
            "Wait on an exec cell.",
            json!({"type": "object", "properties": {"cell_id": {"type": "string"}}, "required": ["cell_id"]}),
        ),
        fn_tool(
            "exec_command",
            "Execute PTY shell command.",
            json!({"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]}),
        ),
        CanonicalToolSpec {
            name: "apply_patch".to_owned(),
            description: Some("Apply V4A patch.".to_owned()),
            kind: CanonicalToolKind::Custom { format_json: None },
        },
    ];
    for idx in 0..26 {
        medium_codex.push(fn_tool(
            &format!("mcp__linear__op_{idx:02}"),
            "Medium MCP connector tool.",
            json!({"type": "object", "properties": {"id": {"type": "string"}}}),
        ));
    }
    assert_eq!(medium_codex.len(), 30);

    let codex_cat = partition_tools_for_mcp_session_with_options(
        ProtocolHint::OpenAiResponses,
        &medium_codex,
        McpSessionOptions {
            enable_gateway_web_fetch: true,
            enable_context_spill_reader: true,
        },
        &mut spill,
    );
    assert_eq!(codex_cat.profile_id, "codex");
    assert!(
        codex_cat.is_passthrough,
        "30-tool session with <= 28 KiB schemas must use 1:1 Direct Passthrough"
    );
    assert!(codex_cat.virtualized_hubs.is_empty());
    assert_eq!(
        codex_cat.orchestrator_contract(),
        Some(McpOrchestratorContract::CodexExec {
            mcp_name: "exec",
            has_apply_patch: true,
            has_update_plan: false,
        })
    );
    assert!(codex_cat.wire_tool_bytes <= VirtualizerBudget::CODEX_AGY_ACP.max_eager_wire_bytes);

    let mut codex_with_plan = medium_codex.clone();
    codex_with_plan.pop();
    codex_with_plan.push(fn_tool(
        "update_plan",
        "Updates the task plan.",
        json!({"type": "object", "properties": {"plan": {"type": "array"}}}),
    ));
    let codex_plan_cat = partition_tools_for_mcp_session_with_options(
        ProtocolHint::OpenAiResponses,
        &codex_with_plan,
        McpSessionOptions {
            enable_gateway_web_fetch: true,
            enable_context_spill_reader: true,
        },
        &mut spill,
    );
    assert_eq!(
        codex_plan_cat.orchestrator_contract(),
        Some(McpOrchestratorContract::CodexExec {
            mcp_name: "exec",
            has_apply_patch: true,
            has_update_plan: true,
        })
    );

    // 2. Standard Claude Code session (no `REPL`, with `Bash` + 25 built-in/plugin tools = 28 tools)
    //    achieves 1:1 Direct Passthrough AND emits `ClaudeBashFirst { bash_mcp_name: "Bash" }`!
    let mut claude_std = Vec::new();
    for core in [
        "Bash", "Read", "Edit", "Write", "Grep", "Glob", "Agent", "LSP",
    ] {
        claude_std.push(fn_tool(
            core,
            &format!("Claude tool {core}."),
            json!({"type": "object", "properties": {"input": {"type": "string"}}}),
        ));
    }
    for idx in 0..20 {
        claude_std.push(fn_tool(
            &format!("mcp__plugin_git__op_{idx:02}"),
            "Git MCP tool.",
            json!({"type": "object", "properties": {"repo": {"type": "string"}}}),
        ));
    }
    let claude_cat = partition_tools_for_mcp_session(
        ProtocolHint::AnthropicMessages,
        &claude_std,
        false,
        &mut spill,
    );
    assert_eq!(claude_cat.profile_id, "claude-code");
    assert!(claude_cat.is_passthrough);
    assert_eq!(
        claude_cat.orchestrator_contract(),
        Some(McpOrchestratorContract::ClaudeBashFirst {
            bash_mcp_name: "Bash",
        })
    );

    // 3. When Claude Code `REPL` IS present (even after `Bash` in declaration order!),
    //    `ClaudeBashFirst` is suppressed so `REPL` Code Mode is never shadowed.
    let mut claude_with_repl = claude_std;
    claude_with_repl.push(fn_tool(
        "REPL",
        "Persistent Code Mode REPL.",
        json!({"type": "object", "properties": {"code": {"type": "string"}}}),
    ));
    let claude_repl_cat = partition_tools_for_mcp_session(
        ProtocolHint::AnthropicMessages,
        &claude_with_repl,
        false,
        &mut spill,
    );
    assert_eq!(claude_repl_cat.orchestrator_contract(), None);
}

#[test]
fn describe_explains_tier_wire_tool_and_byte_attribution_for_every_client_tool() {
    let ts_footer = "\n\nexec tool declaration:\n```ts\ndeclare const tools: { gh(args: { id: number }): Promise<unknown>; };\n```";
    let mut tools = vec![
        CanonicalToolSpec {
            name: "exec".to_owned(),
            description: Some(
                "Run JavaScript in Code Mode. globalThis.tools exposes nested tools.".to_owned(),
            ),
            kind: CanonicalToolKind::Custom { format_json: None },
        },
        fn_tool(
            "exec_command",
            "Run a shell command in PTY.",
            json!({"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]}),
        ),
        CanonicalToolSpec {
            name: "apply_patch".to_owned(),
            description: Some("Freeform patch grammar.".to_owned()),
            kind: CanonicalToolKind::Custom { format_json: None },
        },
    ];
    for i in 0..50 {
        tools.push(fn_tool(
            &format!("mcp__codex_apps__github._action_{i:02}"),
            &format!("GitHub connector action #{i}.{ts_footer}"),
            json!({"type": "object", "properties": {"pr_number": {"type": "integer"}}, "required": ["pr_number"]}),
        ));
    }

    let cat = partition_tools_for_mcp(&tools, &mut NoopSpillSink);
    assert!(!cat.is_passthrough, "53 tools must virtualize");
    let described = cat.routing_table.describe(&cat);

    assert_eq!(described.tools.len(), tools.len());
    assert_eq!(described.search_index_entries, tools.len());
    assert_eq!(described.wire_tools.len(), cat.wire_tools.len());
    let by_name = |name: &str| {
        described
            .tools
            .iter()
            .find(|t| t.client_name == name)
            .unwrap_or_else(|| panic!("missing `{name}`"))
    };

    let exec = by_name("exec");
    assert_eq!(exec.tier, CatalogToolTier::Orchestrator);
    assert_eq!(exec.wire_name.as_deref(), Some("exec"));
    assert_eq!(exec.client_kind, "custom");
    assert!(exec.wire_bytes > 0 && exec.wire_description.is_some());

    let shell = by_name("exec_command");
    assert_eq!(shell.tier, CatalogToolTier::Direct);
    assert!(
        shell
            .wire_schema
            .as_deref()
            .is_some_and(|s| s.contains("cmd"))
    );

    let action = by_name("mcp__codex_apps__github._action_07");
    assert_eq!(action.tier, CatalogToolTier::NamespaceHubAction);
    assert_eq!(action.wire_name.as_deref(), Some("ns__github"));
    assert!(action.wire_description.is_none());
    assert!(
        action
            .ts_signature
            .as_deref()
            .is_some_and(|ts| ts.contains("declare const tools")),
        "Codex TypeScript declarations stay in the 0-RTT search index"
    );
    assert!(action.client_bytes > action.wire_bytes);

    let hub = described
        .wire_tools
        .iter()
        .find(|w| w.wire_name == "ns__github")
        .expect("github hub");
    assert_eq!(hub.role, CatalogWireRole::NamespaceHub);
    assert_eq!(hub.folded_actions.len(), 50);
    let per_action: usize = described
        .tools
        .iter()
        .filter(|t| t.wire_name.as_deref() == Some("ns__github"))
        .map(|t| t.wire_bytes)
        .sum();
    assert!(per_action <= hub.bytes && per_action + 50 >= hub.bytes);

    for gateway in [SEARCH_CLIENT_TOOLS_MCP_NAME, CALL_CLIENT_TOOL_MCP_NAME] {
        let wire = described
            .wire_tools
            .iter()
            .find(|w| w.wire_name == gateway)
            .expect("gateway tool mounted");
        assert_eq!(wire.role, CatalogWireRole::Gateway);
    }
    let wire_total: usize = described.wire_tools.iter().map(|w| w.bytes).sum();
    assert_eq!(wire_total, cat.wire_tool_bytes);
    assert!(described.client_tools_bytes > wire_total);
}

#[test]
fn describe_attributes_omp_folded_tools_to_the_eval_orchestrator() {
    let mut tools = vec![
        fn_tool(
            "eval",
            "Run persistent Python (`py`) or JavaScript (`js`) cells; top-level state persists.\n<prelude>\nawait tool.read({ path: 'src/lib.rs' });\n</prelude>",
            json!({
                "type": "object",
                "properties": {
                    "language": { "type": "string", "enum": ["py", "js"] },
                    "code": { "type": "string" }
                },
                "required": ["language", "code"]
            }),
        ),
        fn_tool(
            "wait",
            "Wait for background jobs",
            json!({ "type": "object", "properties": { "jobs": { "type": "array" } } }),
        ),
    ];
    for name in ["read", "bash", "grep", "glob", "write", "task"] {
        tools.push(fn_tool(
            name,
            &format!("OMP built-in {name} tool"),
            json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        ));
    }
    let cat = partition_tools_for_mcp_session(
        ProtocolHint::OpenAiResponses,
        &tools,
        false,
        &mut NoopSpillSink,
    );
    let described = cat.routing_table.describe(&cat);

    let read = described
        .tools
        .iter()
        .find(|t| t.client_name == "read")
        .expect("read");
    assert_eq!(read.tier, CatalogToolTier::FoldedIntoOrchestrator);
    assert_eq!(read.wire_name.as_deref(), Some("eval"));
    assert_eq!(read.wire_bytes, 0);

    let eval = described
        .wire_tools
        .iter()
        .find(|w| w.wire_name == "eval")
        .expect("eval mounted");
    assert_eq!(eval.role, CatalogWireRole::Orchestrator);
    assert!(eval.folded_actions.iter().any(|a| a == "read"));
    assert!(eval.folded_actions.iter().any(|a| a == "bash"));
    let wait = described
        .wire_tools
        .iter()
        .find(|w| w.wire_name == "wait")
        .expect("wait mounted");
    assert!(
        wait.folded_actions.is_empty(),
        "the wait barrier is not the script orchestrator"
    );
}
