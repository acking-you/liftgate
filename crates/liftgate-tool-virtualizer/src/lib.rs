//! Zero-cost, trait-driven 3-tier MCP tool virtualization for `agy acp`.
//!
//! This crate contains zero async runtime, HTTP, or OS I/O dependencies. Each coding harness
//! implements [`HarnessToolProfile`] as a zero-sized unit struct (`Copy + Default + 'static`),
//! monomorphized into [`plan_virtualization`] to keep `lg` loopback MCP tool exposure strictly
//! within `agy acp`'s 50-tool Eager ceiling while providing 0-RTT in-memory schema search and
//! required-parameter preflight self-healing.

pub mod budget;
pub mod planner;
pub mod profile;
pub mod profiles;
pub mod router;
pub mod schema;

pub use budget::VirtualizerBudget;
pub use planner::{
    McpSessionCatalog, partition_tools_for_harness, partition_tools_for_mcp,
    partition_tools_for_mcp_session, partition_tools_for_mcp_with_hint, plan_virtualization,
    plan_virtualization_with_options,
};
pub use profile::{
    DetectedHarness, HarnessToolProfile, ProtocolHint, ToolPlacement, V4aPatchContract,
    default_apply_patch_v4a_contract, detect_harness, is_opencode_v4a_patch_spec,
};
pub use profiles::{
    ClaudeCodeToolProfile, CodexToolProfile, GenericToolProfile, OpenCodeToolProfile,
};
pub use router::{LocalReplyKind, McpRoutingTable, VirtualizedCallDecision};
pub use schema::{
    McpBridgedTool, McpWireTool, apply_patch_input_schema, code_mode_exec_input_schema,
    coerce_stringified_container_fields, estimate_wire_tool_bytes, extract_custom_tool_input,
    freeform_custom_input_schema, gateway_web_fetch_input_schema, normalize_apply_patch_text,
    normalize_mcp_object_schema, sanitize_mcp_tool_name, strip_call_prefix,
    validate_apply_patch_preflight,
};

/// Default hard ceiling on MCP tools exposed per session on `lg`.
pub const MAX_NATIVE_MCP_TOOLS: usize = VirtualizerBudget::DEFAULT_AGY_ACP.max_eager_tools;

/// Gateway-local 0-RTT search tool mounted on `lg` (`mcp_lg_search_client_tools`).
pub const SEARCH_CLIENT_TOOLS_MCP_NAME: &str = "search_client_tools";

/// Universal dispatcher MCP tool mounted on `lg` (`mcp_lg_call_client_tool`).
pub const CALL_CLIENT_TOOL_MCP_NAME: &str = "call_client_tool";

/// Server-managed URL fetch MCP tool mounted on `lg` (`mcp_lg_gateway_web_fetch`).
pub const GATEWAY_WEB_FETCH_MCP_NAME: &str = "gateway_web_fetch";

/// Prefix for namespace action hub MCP tools (`mcp_lg_ns__<namespace>`).
pub const NAMESPACE_HUB_MCP_PREFIX: &str = "ns__";

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::{HashMap, HashSet};

    use liftgate_types::{
        canonical::{CanonicalToolKind, CanonicalToolSpec},
        session_key::NoopSpillSink,
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
                    "Run JavaScript in Code Mode. globalThis.tools exposes nested tools."
                        .to_owned(),
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
            | VirtualizedCallDecision::GatewayWebFetch { .. } => {
                panic!("malformed apply_patch must be caught in 0-RTT preflight")
            }
        }

        match cat.routing_table.resolve_call(
            "apply_patch",
            &json!({"patch": "```diff\n*** Update File: src/lib.rs\n@@\n-a\n+b\n\n ctx\n*** Update File: src/lib.rs\n@@\n-c\n+d\n```"}),
            &tools,
        ) {
            VirtualizedCallDecision::ForwardToClient { client_name, arguments, is_custom } => {
                assert_eq!(client_name, "apply_patch");
                assert!(is_custom);
                assert_eq!(
                    arguments,
                    "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-a\n+b\n \n ctx\n@@\n-c\n+d\n*** End Patch"
                );
            }
            VirtualizedCallDecision::LocalReply { text, .. } => {
                panic!("expected coalesced ForwardToClient, got LocalReply: {text}")
            }
            VirtualizedCallDecision::GatewayWebFetch { .. } => {
                panic!("unexpected GatewayWebFetch")
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
                .any(|(h, ns, cnt)| h == "ns__multi_agent_v1"
                    && ns == "multi_agent_v1"
                    && *cnt == 4)
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
            | VirtualizedCallDecision::GatewayWebFetch { .. } => {
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
            } => {
                assert_eq!(client_name, "TaskCreate");
                assert!(!is_custom);
                assert_eq!(arguments, r#"{"id":"t-1"}"#);
            }
            VirtualizedCallDecision::LocalReply { .. }
            | VirtualizedCallDecision::GatewayWebFetch { .. } => {
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
            | VirtualizedCallDecision::GatewayWebFetch { .. } => {
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

            fn classify_tool<'a>(
                self,
                spec: &'a CanonicalToolSpec,
                _fallback_stem_counts: &HashMap<String, usize>,
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
        for i in 0..24 {
            tools.push(fn_tool(
                &format!("lsp_op_{i:02}"),
                "LSP operation.",
                json!({"type": "object", "properties": {"uri": {"type": "string"}}, "required": ["uri"]}),
            ));
        }

        let cat = plan_virtualization(ToyHarnessProfile, &tools, &mut NoopSpillSink);
        assert_eq!(cat.profile_id, "toy-harness");
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
            | VirtualizedCallDecision::GatewayWebFetch { .. } => {
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
    fn zero_tool_session_with_gateway_web_fetch_hot_syncs_dynamic_tools_without_recreating_session()
    {
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
                url: "https://example.com/docs".to_owned()
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
            }
        );
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

        let wire_names: HashSet<&str> =
            catalog.wire_tools.iter().map(|w| w.name.as_str()).collect();
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
        for idx in 0..30 {
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
            }
        );

        let virt_cat =
            partition_tools_for_mcp_with_hint(ProtocolHint::Unknown, &fn_tools, &mut spill);
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
        let oc_cat = partition_tools_for_harness(
            DetectedHarness::OpenCode,
            &oc_with_custom_patch,
            &mut spill,
        );
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
            }
        );
    }
}
