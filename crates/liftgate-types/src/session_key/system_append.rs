//! `_meta.antigravity.systemPrompt.append` builders and incremental `<client_tools_update>` deltas.

use crate::canonical::{CanonicalToolKind, CanonicalToolSpec};

use super::codemode::{
    MAX_CODE_MODE_TOOL_DESC_BYTES, MAX_SLIM_CODE_MODE_TOOL_DESC_BYTES, McpOrchestratorContract,
    collapse_tool_description, compact_code_mode_tool_description, compact_json_schema,
    compact_tool_description, is_code_mode_orchestrator_tool,
};
use super::prompt_plan::MAX_TOOL_DIRECTIVE_BYTES;
use super::spill::{
    NoopSpillSink, TruncationSpillSink, compact_middle_chars, truncate_head_tail_lines_with_spill,
};
use super::{strip_billing_noise, strip_tag_blocks};

/// Internal tool name used when server-side `web_fetch` (`web_fetch_20250910` / `web_fetch`)
/// is requested on `/v1/messages` or `/v1/responses`.
pub const INTERNAL_GATEWAY_WEB_FETCH_TOOL: &str = "__gateway_web_fetch";

/// Internal tool name used when OpenAI Codex declares built-in `{"type": "local_shell"}`.
pub const INTERNAL_CODEX_LOCAL_SHELL_TOOL: &str = "__codex_local_shell";

/// Canonical tool name used when OpenAI Codex declares built-in `{"type": "tool_search"}`.
pub const INTERNAL_CODEX_TOOL_SEARCH_TOOL: &str = "tool_search";

/// Server-executed web-tool mode flags passed into `build_session_system_append`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "groups the four orthogonal server-side web-search and web-fetch flags"
)]
pub struct WebToolBridgeConfig {
    pub enable_native_web_search: bool,
    pub force_native_web_search: bool,
    pub enable_native_web_fetch: bool,
    pub force_native_web_fetch: bool,
}

fn serialize_full_client_tools_catalog(tools: &[&CanonicalToolSpec]) -> String {
    let items: Vec<serde_json::Value> = tools
        .iter()
        .map(|tool| match &tool.kind {
            CanonicalToolKind::Function { parameters_json } => {
                let params = serde_json::from_str::<serde_json::Value>(parameters_json)
                    .unwrap_or_else(|_| serde_json::Value::String(parameters_json.clone()));
                serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "invocation": "xml_json_arguments",
                    "parameters": params,
                })
            }
            CanonicalToolKind::Custom { format_json } => {
                let fmt = format_json.as_deref().map(|raw| {
                    serde_json::from_str::<serde_json::Value>(raw)
                        .unwrap_or_else(|_| serde_json::Value::String(raw.to_owned()))
                });
                serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "invocation": "xml_freeform_input",
                    "format": fmt,
                })
            }
        })
        .collect();
    serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".to_owned())
}

fn compact_caller_system<S: TruncationSpillSink>(raw_sys: &str, spill: &mut S) -> String {
    let cleaned = strip_billing_noise(raw_sys);
    let trimmed = cleaned.trim();
    if trimmed.len() <= 8_000 {
        return trimmed.to_owned();
    }
    let without_examples = strip_tag_blocks(trimmed, "<examples>", "</examples>");
    let without_single = strip_tag_blocks(&without_examples, "<example>", "</example>");
    let compacted = without_single.trim();
    if compacted.len() < trimmed.len()
        && let Some(path) = spill.spill_truncated_content("caller system instructions", trimmed)
    {
        return format!(
            "{compacted}\n[Gateway omitted inline <example> blocks from caller system instructions; full original instructions saved locally at `{path}`.]"
        );
    }
    compacted.to_owned()
}

/// Native loopback MCP bridge tool partition passed when constructing `_meta.antigravity.systemPrompt.append`.
#[derive(Clone, Copy, Debug, Default)]
pub struct McpBridgePromptSpec<'a> {
    /// `(client_tool_name, mcp_tool_name)` pairs exposed natively on the `lg` loopback MCP server.
    pub bridged_tools: &'a [(String, String)],
    /// `(hub_mcp_name, namespace_label, action_count)` summaries when adaptive namespace
    /// virtualization (`mcp_lg_ns__*`) is active on the `lg` loopback MCP server.
    pub virtualized_hubs: &'a [(String, String, usize)],
    /// Whether `mcp_lg_search_client_tools` and `mcp_lg_call_client_tool` are mounted on `lg`.
    pub has_virtualized_dispatch: bool,
    /// Whether server-managed `mcp_lg_gateway_web_fetch` is mounted on `lg`.
    pub has_gateway_web_fetch: bool,
    /// Active Code Mode orchestrator contract supplied by the selected harness tool profile.
    pub orchestrator: Option<McpOrchestratorContract<'a>>,
}

/// Builds a 100% pure-MCP, nonce-free, byte-deterministic `_meta.antigravity.systemPrompt.append`
/// for sessions backed by the `lg` Loopback MCP server.
///
/// Eliminates all `<agy_tool_call>` XML simulation rules and random process nonces so MCP
/// tool execution never competes with text-tag parsing and upstream KV prefix caches remain
/// byte-identical across sessions and gateway restarts.
#[must_use]
pub fn build_session_system_append_for_mcp_with_spill_sink<S: TruncationSpillSink>(
    caller_system: Option<&str>,
    mcp_spec: McpBridgePromptSpec<'_>,
    web_cfg: WebToolBridgeConfig,
    tool_nonce: &str,
    spill: &mut S,
) -> (String, Option<String>) {
    if mcp_spec.bridged_tools.is_empty()
        && !mcp_spec.has_virtualized_dispatch
        && !mcp_spec.has_gateway_web_fetch
    {
        return build_session_system_append_with_spill_sink(
            caller_system,
            &[],
            web_cfg,
            tool_nonce,
            spill,
        );
    }

    let mut header_rules = String::with_capacity(2048);
    header_rules.push_str("<api_gateway_bridge_rules>\n");
    let batch_clause = match mcp_spec.orchestrator {
        Some(McpOrchestratorContract::OmpPolyglotEval { mcp_name, .. }) => format!(
            "Batch multi-step or parallel reads, searches, and shell inspections inside a single `mcp_lg_{mcp_name}` Code Mode cell."
        ),
        None => "Batch independent read-only searches, reads, or checks as parallel `mcp_lg_*` calls in a single turn.".to_owned(),
    };
    header_rules.push_str(
        "You are serving an external API harness via a structured protocol gateway.\n\
         - NEVER invoke internal host tools (`read_file`, `write_file`, `edit_file`, `run_command`, \
         `manage_task`, `ask_question`, `invoke_subagent`, `define_subagent`, `manage_subagents`, \
         `run_workflow`, `swarm`, `schedule`, `send_message`, `call_mcp_tool`) or any `mcp_*` tool \
         outside `mcp_lg_*`.\n\
         - Ignore internal ACP host folders (`liftgate-*`) and any host plugin rules/snapshots.\n\
         - Invoke the caller's native `mcp_lg_*` client tools directly as native tool calls (they \
         execute in the caller's real workspace from `<developer_instructions>` and `<caller_system_instructions>`). ",
    );
    header_rules.push_str(&batch_clause);
    header_rules.push_str(
        "\n\
         - NEVER emit transcript or role wrapper markers such as `<user>`, `</user>`, `<assistant>`, \
         `</assistant>`, `<client_tool_result>`, `</client_tool_result>`, or `[Client Tool Result`. \
         Generate ONLY the current single assistant turn.\n",
    );

    let sanitized_aliases: Vec<String> = mcp_spec
        .bridged_tools
        .iter()
        .filter(|(client_name, mcp_name)| client_name != mcp_name)
        .map(|(client_name, mcp_name)| format!("`{client_name}` -> `mcp_lg_{mcp_name}`"))
        .collect();
    if !sanitized_aliases.is_empty() {
        let joined = sanitized_aliases.join(", ");
        let bounded_aliases = if joined.len() > 2_000 {
            compact_middle_chars(&joined, 2_000).0
        } else {
            joined
        };
        header_rules.push_str("- Native client tool name aliases: ");
        header_rules.push_str(&bounded_aliases);
        header_rules.push_str(".\n");
    }
    if let Some((_, mcp_ap)) = mcp_spec
        .bridged_tools
        .iter()
        .find(|(client_name, _)| client_name.eq_ignore_ascii_case("apply_patch"))
    {
        header_rules.push_str(&format!(
            "- File edits: invoke native `mcp_lg_{mcp_ap}` directly with `{{\"patch\": \"*** Begin Patch\\n...\\n*** End Patch\"}}` (prefer native `mcp_lg_{mcp_ap}` over embedding multi-line patches inside `exec` JavaScript template literals; keep `@@` hunks surgical with 1-3 exact context lines and merge multiple edits to the same file under one `*** Update File:` header).\n"
        ));
    }
    match mcp_spec.orchestrator {
        Some(McpOrchestratorContract::OmpPolyglotEval {
            mcp_name,
            elided_subordinate_tools,
        }) => {
            header_rules.push_str(&format!(
                "- Polyglot Code Mode (`mcp_lg_{mcp_name}`): invoke native `mcp_lg_{mcp_name}` as your primary workspace inspection, search, and shell execution tool in persistent Python (`py`) / JavaScript (`js`) cells (e.g. `await Promise.all([tool.read(...), tool.grep(...), sh(...)])` in `js`, or `await tool.<name>(args)` / `sh(...)` in `py`); inside `eval` scripts (and `bash` tool-commands), call `tool.<name>(args)` and MCP eval globals by their unprefixed client names (never `mcp_lg_*` inside `eval` code)."
            ));
            if elided_subordinate_tools {
                header_rules.push_str(&format!(
                    " Subordinate read/search/shell tools (`read`, `bash`, `glob`, `grep`, `find`, `lsp`, `todo`, `web_search`, etc.) are folded into `mcp_lg_{mcp_name}` rather than exposed as separate `mcp_lg_*` tools (do NOT invoke them via `mcp_lg_call_client_tool`); before editing an existing file with native `mcp_lg_edit`, read it via `await tool.read({{\"path\": \"<file>:<start>-<end>\"}})` inside `mcp_lg_{mcp_name}` to obtain its `[FILENAME#TAG]` hashline anchors, then invoke native `mcp_lg_edit` / `mcp_lg_write` directly for file mutations."
                ));
            }
            header_rules.push('\n');
        }
        None => {}
    }

    if mcp_spec.has_virtualized_dispatch {
        if !mcp_spec.virtualized_hubs.is_empty() {
            let hub_list: Vec<String> = mcp_spec
                .virtualized_hubs
                .iter()
                .map(|(mcp_name, ns_label, count)| {
                    format!("`mcp_lg_{mcp_name}` ({ns_label}: {count} actions)")
                })
                .collect();
            let joined_hubs = hub_list.join(", ");
            let bounded_hubs = if joined_hubs.len() > 1_200 {
                compact_middle_chars(&joined_hubs, 1_200).0
            } else {
                joined_hubs
            };
            header_rules.push_str("- Virtualized client tool hubs: ");
            header_rules.push_str(&bounded_hubs);
            header_rules.push_str(".\n");
        }
        header_rules.push_str(
            "- Virtualized tool discovery & dispatch: grouped client tool families are invoked via \
             `mcp_lg_ns__<namespace>` (`{\"action\": \"<action_name>\", \"args\": { ... }}`) and \
             standalone or dynamically added tools via `mcp_lg_call_client_tool` \
             (`{\"name\": \"<exact_tool_name>\", \"arguments\": { ... }}`). Call \
             `mcp_lg_search_client_tools` FIRST (zero-round-trip local lookup) whenever you need \
             exact action names, required parameter schemas, or Code Mode `tools.*` TypeScript \
             declarations before calling an unfamiliar hub action or any tool added mid-session via `<client_tools_update>`.\n",
        );
    }

    if web_cfg.force_native_web_search {
        header_rules.push_str(
            "- MANDATORY WEB SEARCH: Immediately invoke your built-in `search_web` native tool for the \
             user's query before answering. After `search_web` completes, write a concise, accurate \
             summary of the findings followed by a `Sources:` list of markdown hyperlinks `[Title](https://...)` \
             for every relevant URL.\n",
        );
    } else if web_cfg.enable_native_web_search {
        header_rules.push_str(
            "- Built-in `search_web` IS enabled: invoke your built-in `search_web` native tool whenever \
             live web search information is needed, and cite every source URL as `[Title](https://...)`.\n",
        );
    } else if let Some((_, mcp_ws)) = mcp_spec
        .bridged_tools
        .iter()
        .find(|(client_name, _)| client_name.eq_ignore_ascii_case("WebSearch"))
    {
        header_rules.push_str(&format!(
            "- When web search is needed, invoke the native client tool `mcp_lg_{mcp_ws}` \
             (do NOT invoke internal `search_web` directly when client `WebSearch` is provided).\n"
        ));
    } else {
        header_rules.push_str(
            "- You MAY still use built-in `search_web` if the user explicitly asks to search the live web \
             and no client web-search tool is provided.\n",
        );
    }

    if mcp_spec.has_gateway_web_fetch || web_cfg.enable_native_web_fetch {
        if web_cfg.force_native_web_fetch {
            header_rules.push_str(
                "- MANDATORY SERVER WEB FETCH: Before answering, fetch the requested web URL by invoking \
                 native `mcp_lg_gateway_web_fetch` (`{\"url\": \"https://...\"}`) directly.\n",
            );
        } else {
            header_rules.push_str(
                "- Server-managed URL fetching IS enabled: to read the full content of any `http://` or `https://` URL, \
                 invoke native `mcp_lg_gateway_web_fetch` (`{\"url\": \"https://...\"}`) directly.\n",
            );
        }
    }

    let mut bounded = header_rules;
    bounded.push_str("</api_gateway_bridge_rules>\n");

    let mut combined_spill_xml = String::new();
    if let Some(raw_sys) = caller_system {
        let compacted_sys = compact_caller_system(raw_sys, spill);
        if !compacted_sys.is_empty() {
            let sys_wrapper_len =
                "\n<caller_system_instructions>\n\n</caller_system_instructions>\n".len();
            if bounded.len() + sys_wrapper_len + compacted_sys.len() <= MAX_SYSTEM_APPEND_BYTES {
                bounded.push_str("\n<caller_system_instructions>\n");
                bounded.push_str(&compacted_sys);
                bounded.push_str("\n</caller_system_instructions>\n");
            } else {
                let bounded_sys = if compacted_sys.len() > 24_000 {
                    truncate_head_tail_lines_with_spill(
                        &compacted_sys,
                        24_000,
                        "caller system instructions",
                        spill,
                    )
                } else {
                    compacted_sys
                };
                combined_spill_xml.push_str("<caller_system_instructions>\n");
                combined_spill_xml.push_str(&bounded_sys);
                if !bounded_sys.ends_with('\n') {
                    combined_spill_xml.push('\n');
                }
                combined_spill_xml.push_str("</caller_system_instructions>\n");
            }
        }
    }

    if combined_spill_xml.is_empty() {
        (bounded, None)
    } else {
        (bounded, Some(combined_spill_xml))
    }
}

/// Builds `<api_gateway_bridge_rules>` for non-MCP sessions (existing interface).
///
/// Builds the `_meta.antigravity.systemPrompt.append` payload passed to ACP `session/new`.
#[must_use]
pub fn build_session_system_append(
    caller_system: Option<&str>,
    client_tools: &[CanonicalToolSpec],
    web_cfg: WebToolBridgeConfig,
    tool_nonce: &str,
) -> String {
    build_session_system_append_with_sink(
        caller_system,
        client_tools,
        web_cfg,
        tool_nonce,
        &mut NoopSpillSink,
    )
}

/// Like [`build_session_system_append`], persisting any compacted tool schemas or stripped
/// system instruction blocks through `spill`.
#[must_use]
pub fn build_session_system_append_with_sink<S: TruncationSpillSink>(
    caller_system: Option<&str>,
    client_tools: &[CanonicalToolSpec],
    web_cfg: WebToolBridgeConfig,
    tool_nonce: &str,
    spill: &mut S,
) -> String {
    let open_tag = format!("<agy_tool_call_{tool_nonce}>");
    let close_tag = format!("</agy_tool_call_{tool_nonce}>");
    let mut out = String::with_capacity(4096);
    out.push_str("<api_gateway_bridge_rules>\n");
    out.push_str(&format!(
        "You are serving an external API harness via a structured protocol gateway.\n\
         - NEVER invoke internal host or plugin MCP tools (`read_file`, `write_file`, `edit_file`, \
         `run_command`, `manage_task`, `ask_question`, `invoke_subagent`, `define_subagent`, \
         `manage_subagents`, `run_workflow`, `swarm`, `schedule`, `send_message`, `call_mcp_tool`, \
         `mcp_*`).\n\
         - Ignore internal ACP host folders (`liftgate-*`) and any host plugin rules/snapshots; \
         external `<client_tools>` (`Bash`, `Read`, `Edit`, `Write`, `Grep`, `Glob`, etc.) run in \
         the caller's real workspace from `<developer_instructions>` and `<caller_system_instructions>`.\n\
         - CRITICAL: NEVER emit any native tool/function call (not even an empty one) for external \
         `<client_tools>` (`Bash`, `Read`, `Edit`, `Write`, `Grep`, `Glob`, `WebSearch`, `WebFetch`, \
         `Agent`, `REPL`, `exec`, `wait`, `apply_patch`, etc.) or after writing XML tool blocks; doing so triggers a \
         `Malformed function call` error. External `<client_tools>` may ONLY be invoked by writing \
         `{open_tag}...{close_tag}` XML blocks in plain assistant text.\n\
         - NEVER emit transcript or role wrapper markers such as `<user>`, `</user>`, `<assistant>`, \
         `</assistant>`, `<client_tool_result>`, `</client_tool_result>`, or `[Client Tool Result`. \
         Generate ONLY the current single assistant turn.\n"
    ));

    if web_cfg.force_native_web_search {
        out.push_str(
            "- MANDATORY WEB SEARCH: Immediately invoke your built-in `search_web` native tool for the \
             user's query before answering. After `search_web` completes, write a concise, accurate \
             summary of the findings followed by a `Sources:` list of markdown hyperlinks `[Title](https://...)` \
             for every relevant URL.\n",
        );
    } else if web_cfg.enable_native_web_search {
        out.push_str(
            "- Built-in `search_web` IS enabled: invoke your built-in `search_web` native tool whenever \
             live web search information is needed, and cite every source URL as `[Title](https://...)`.\n",
        );
    } else if client_tools
        .iter()
        .any(|t| t.name.eq_ignore_ascii_case("WebSearch"))
    {
        out.push_str(&format!(
            "- When web search is needed, invoke the client tool `WebSearch` via `{open_tag}` XML \
             below (do NOT invoke internal `search_web` directly when client `WebSearch` is provided).\n"
        ));
    } else {
        out.push_str(
            "- You MAY still use built-in `search_web` if the user explicitly asks to search the live web \
             and no client web-search tool is provided.\n",
        );
    }

    let has_custom_tools = client_tools
        .iter()
        .any(|t| matches!(t.kind, CanonicalToolKind::Custom { .. }));
    let need_xml_protocol = !client_tools.is_empty() || web_cfg.enable_native_web_fetch;
    if need_xml_protocol {
        append_xml_invocation_bridge_rules(
            &mut out,
            &open_tag,
            &close_tag,
            web_cfg,
            has_custom_tools,
        );

        if !client_tools.is_empty() {
            out.push_str("<client_tools>\n");
            let mut any_tool_compacted = false;
            for tool in client_tools {
                if render_compact_client_tool(tool, &mut out, spill) {
                    any_tool_compacted = true;
                }
            }
            if any_tool_compacted {
                let refs: Vec<&CanonicalToolSpec> = client_tools.iter().collect();
                if let Some(catalog_path) = spill.spill_truncated_content(
                    "client tools catalog",
                    &serialize_full_client_tools_catalog(&refs),
                ) {
                    out.push_str(&format!(
                        "  # Full lossless client tool descriptions & JSON schemas saved locally at `{catalog_path}`.\n"
                    ));
                }
            }
            out.push_str("</client_tools>\n");
        }
    }
    out.push_str("</api_gateway_bridge_rules>\n");

    if let Some(sys) = caller_system {
        let compacted_sys = compact_caller_system(sys, spill);
        if !compacted_sys.is_empty() {
            out.push_str("\n<caller_system_instructions>\n");
            out.push_str(&compacted_sys);
            out.push_str("\n</caller_system_instructions>\n");
        }
    }

    out
}

fn append_xml_invocation_bridge_rules(
    out: &mut String,
    open_tag: &str,
    close_tag: &str,
    web_cfg: WebToolBridgeConfig,
    has_custom_tools: bool,
) {
    out.push_str(
        "- Batch independent read-only searches, ranged reads, or checks as parallel XML blocks in ONE turn \
         (avoid serial single-tool turns), and never retry identical rejected arguments.\n\
         - To invoke XML-bridged tools below, optionally write a brief one-sentence status note before the \
         first XML block, emit one XML block per call in this EXACT format (no XML attributes, \
         NEVER wrap in backticks/code fences, JSON-escape `\\`, `\"`, `\\n` inside strings), \
         and END YOUR TURN IMMEDIATELY with ZERO trailing text or native function calls after the \
         final closing tag — NEVER fabricate tool outputs before `[Client Tool Result ...]`:\n",
    );
    out.push_str(open_tag);
    out.push('\n');
    out.push_str("{\"name\": \"<exact_tool_name>\", \"arguments\": { ... }}\n");
    out.push_str(close_tag);
    if has_custom_tools {
        out.push_str(
            "\n  (For `apply_patch`, emit the raw multi-line patch directly inside the XML block \
             WITHOUT JSON-escaping — starting with `*** Begin Patch` and ending with `*** End Patch`, \
             keeping `@@` hunks surgical with 1-3 exact unchanged context lines per edit, splitting \
             distant edits into separate `@@` hunks, and re-reading exact target lines first if prior \
             output showed `[...omitted ...]`. For Code Mode `exec`, pass `{\"name\": \"exec\", \"input\": \
             \"<JSON-escaped JS source>\"}` or start a bare JS block with `// @exec:`.)\n",
        );
    } else {
        out.push_str(
            "\n  (For `apply_patch`, emit raw multi-line `*** Begin Patch ... *** End Patch` directly \
             inside the XML block with 1-3 exact context lines per `@@` hunk, or pass `\"input\"`.)\n",
        );
    }

    if web_cfg.enable_native_web_fetch {
        if web_cfg.force_native_web_fetch {
            out.push_str(&format!(
                "- MANDATORY SERVER WEB FETCH: Before answering, fetch the requested web URL by emitting \
                 `{open_tag}\n{{\"name\": \"{INTERNAL_GATEWAY_WEB_FETCH_TOOL}\", \"arguments\": {{\"url\": \"https://...\"}}}}\n{close_tag}` \
                 and stop; the gateway will immediately fetch the live page and deliver `<gateway_web_fetch_result>`.\n"
            ));
        } else {
            out.push_str(&format!(
                "- Server-managed URL fetching IS enabled: to read the full content of any `http://` or `https://` URL, \
                 emit `{open_tag}\n{{\"name\": \"{INTERNAL_GATEWAY_WEB_FETCH_TOOL}\", \"arguments\": {{\"url\": \"https://...\"}}}}\n{close_tag}` \
                 and stop; the gateway will fetch the URL server-side and return `<gateway_web_fetch_result>` in the same turn.\n"
            ));
        }
    }
}

/// Maximum UTF-8 byte budget for `_meta.antigravity.systemPrompt.append` on `session/new`.
///
/// Upstream `agy acp` rejects oversized `systemPrompt.append` payloads with `429 RESOURCE_EXHAUSTED`.
/// Capping `systemPrompt.append` at `16,000` bytes and spilling any excess `<client_tools>`
/// schemas into the initial `session/prompt` directive protects tool-heavy harnesses (such as
/// OpenCode or multi-MCP setups) without dropping a single tool definition.
pub const MAX_SYSTEM_APPEND_BYTES: usize = 16_000;

/// Builds `_meta.antigravity.systemPrompt.append` bounded to `MAX_SYSTEM_APPEND_BYTES`, returning
/// `(system_append, overflow_tools_directive_xml)` when a large `<client_tools>` catalog spills
/// into the initial `session/prompt` turn.
#[must_use]
pub fn build_session_system_append_with_spill(
    caller_system: Option<&str>,
    client_tools: &[CanonicalToolSpec],
    web_cfg: WebToolBridgeConfig,
    tool_nonce: &str,
) -> (String, Option<String>) {
    build_session_system_append_with_spill_sink(
        caller_system,
        client_tools,
        web_cfg,
        tool_nonce,
        &mut NoopSpillSink,
    )
}

/// Like [`build_session_system_append_with_spill`], persisting any compacted or spilled tool
/// schemas and caller system instructions to local files via `spill`.
#[must_use]
pub fn build_session_system_append_with_spill_sink<S: TruncationSpillSink>(
    caller_system: Option<&str>,
    client_tools: &[CanonicalToolSpec],
    web_cfg: WebToolBridgeConfig,
    tool_nonce: &str,
    spill: &mut S,
) -> (String, Option<String>) {
    let full = build_session_system_append_with_sink(
        caller_system,
        client_tools,
        web_cfg,
        tool_nonce,
        spill,
    );
    if full.len() <= MAX_SYSTEM_APPEND_BYTES {
        return (full, None);
    }
    if client_tools.is_empty() {
        if let Some(raw_sys) = caller_system {
            let base = build_session_system_append_with_sink(None, &[], web_cfg, tool_nonce, spill);
            let cleaned_sys = strip_billing_noise(raw_sys);
            let trimmed_sys = cleaned_sys.trim();
            let wrapper_overhead =
                "\n<caller_system_instructions>\n\n</caller_system_instructions>\n".len();
            let avail = MAX_SYSTEM_APPEND_BYTES.saturating_sub(base.len() + wrapper_overhead);
            if avail >= 512 && !trimmed_sys.is_empty() {
                let bounded_sys = truncate_head_tail_lines_with_spill(
                    trimmed_sys,
                    avail,
                    "caller system instructions",
                    spill,
                );
                if base.len() + wrapper_overhead + bounded_sys.len() <= MAX_SYSTEM_APPEND_BYTES {
                    let mut out = base;
                    out.push_str("\n<caller_system_instructions>\n");
                    out.push_str(&bounded_sys);
                    out.push_str("\n</caller_system_instructions>\n");
                    return (out, None);
                }
            }
            if !trimmed_sys.is_empty() {
                let bounded_spill = truncate_head_tail_lines_with_spill(
                    trimmed_sys,
                    24_000,
                    "caller system instructions",
                    spill,
                );
                let spill_xml = format!(
                    "<caller_system_instructions>\n{bounded_spill}\n</caller_system_instructions>\n"
                );
                return (base, Some(spill_xml));
            }
            return (base, None);
        }
        return (full, None);
    }

    let base_web_cfg = WebToolBridgeConfig {
        enable_native_web_fetch: false,
        force_native_web_fetch: false,
        ..web_cfg
    };
    let mut header =
        build_session_system_append_with_sink(None, &[], base_web_cfg, tool_nonce, spill);
    if let Some(base_prefix) = header.strip_suffix("</api_gateway_bridge_rules>\n") {
        let mut bounded = String::with_capacity(MAX_SYSTEM_APPEND_BYTES);
        bounded.push_str(base_prefix);
        let open_tag = format!("<agy_tool_call_{tool_nonce}>");
        let close_tag = format!("</agy_tool_call_{tool_nonce}>");
        let has_custom_tools = client_tools
            .iter()
            .any(|t| matches!(t.kind, CanonicalToolKind::Custom { .. }));
        append_xml_invocation_bridge_rules(
            &mut bounded,
            &open_tag,
            &close_tag,
            web_cfg,
            has_custom_tools,
        );

        let close_rules = "</client_tools>\n</api_gateway_bridge_rules>\n";
        let mut inline_count = 0usize;
        let mut inline_compacted = false;
        let mut tool_buf = String::new();
        let catalog_note_reserve = 220usize;
        for tool in client_tools {
            tool_buf.clear();
            let tool_was_compacted = render_compact_client_tool(tool, &mut tool_buf, spill);
            let open_cost = if inline_count == 0 {
                "<client_tools>\n".len()
            } else {
                0
            };
            let note_cost = if inline_compacted || tool_was_compacted {
                catalog_note_reserve
            } else {
                0
            };
            if bounded.len() + open_cost + tool_buf.len() + note_cost + close_rules.len()
                <= MAX_SYSTEM_APPEND_BYTES
            {
                if inline_count == 0 {
                    bounded.push_str("<client_tools>\n");
                }
                bounded.push_str(&tool_buf);
                inline_count += 1;
                inline_compacted |= tool_was_compacted;
            } else {
                break;
            }
        }
        if inline_count > 0 {
            if inline_compacted && let Some(inline_slice) = client_tools.get(..inline_count) {
                let inline_refs: Vec<&CanonicalToolSpec> = inline_slice.iter().collect();
                if let Some(catalog_path) = spill.spill_truncated_content(
                    "client tools catalog",
                    &serialize_full_client_tools_catalog(&inline_refs),
                ) {
                    let note = format!(
                        "  # Full lossless inline tool schemas saved locally at `{catalog_path}`.\n"
                    );
                    if bounded.len() + note.len() + close_rules.len() <= MAX_SYSTEM_APPEND_BYTES {
                        bounded.push_str(&note);
                    }
                }
            }
            bounded.push_str(close_rules);
        } else {
            bounded.push_str("</api_gateway_bridge_rules>\n");
        }
        header = bounded;

        let mut combined_spill_xml = String::new();
        if let Some(raw_sys) = caller_system {
            let compacted_sys = compact_caller_system(raw_sys, spill);
            if !compacted_sys.is_empty() {
                let bounded_sys = if compacted_sys.len() > 24_000 {
                    truncate_head_tail_lines_with_spill(
                        &compacted_sys,
                        24_000,
                        "caller system instructions",
                        spill,
                    )
                } else {
                    compacted_sys
                };
                combined_spill_xml.push_str("<caller_system_instructions>\n");
                combined_spill_xml.push_str(&bounded_sys);
                if !bounded_sys.ends_with('\n') {
                    combined_spill_xml.push('\n');
                }
                combined_spill_xml.push_str("</caller_system_instructions>\n");
            }
        }
        if let Some(spilled) = client_tools.get(inline_count..)
            && !spilled.is_empty()
        {
            let spilled_refs: Vec<&CanonicalToolSpec> = spilled.iter().collect();
            combined_spill_xml.push_str(
                "<client_tools_update>\nAdditional active `<client_tools>` for this session:\n",
            );
            render_bounded_tools_list(
                &spilled_refs,
                MAX_TOOL_DIRECTIVE_BYTES,
                &mut combined_spill_xml,
                spill,
            );
            combined_spill_xml.push_str("</client_tools_update>\n");
        }
        if !combined_spill_xml.is_empty() {
            return (header, Some(combined_spill_xml));
        }
    }

    (header, None)
}

fn render_compact_client_tool<S: TruncationSpillSink>(
    tool: &CanonicalToolSpec,
    out: &mut String,
    spill: &mut S,
) -> bool {
    let mut was_compacted = false;
    out.push_str("- name: ");
    out.push_str(&tool.name);
    out.push('\n');
    if let Some(desc) = &tool.description {
        let (compacted_desc, desc_compacted) = if is_code_mode_orchestrator_tool(tool) {
            compact_code_mode_tool_description(desc, MAX_CODE_MODE_TOOL_DESC_BYTES, spill)
        } else {
            compact_tool_description(desc)
        };
        was_compacted |= desc_compacted;
        if !compacted_desc.is_empty() {
            out.push_str("  description: ");
            out.push_str(&compacted_desc);
            out.push('\n');
        }
    }
    match &tool.kind {
        CanonicalToolKind::Function { parameters_json } => {
            out.push_str("  invocation: xml_json_arguments\n  args_schema: ");
            let (schema_str, schema_compacted) = compact_json_schema(parameters_json);
            was_compacted |= schema_compacted;
            out.push_str(&schema_str);
            out.push('\n');
        }
        CanonicalToolKind::Custom { format_json } => {
            out.push_str("  invocation: xml_freeform_input\n");
            if let Some(fmt) = format_json {
                out.push_str("  format: ");
                out.push_str(fmt);
                out.push('\n');
            }
        }
    }
    was_compacted
}

fn render_slim_client_tool<S: TruncationSpillSink>(
    tool: &CanonicalToolSpec,
    out: &mut String,
    spill: &mut S,
) {
    out.push_str("- name: ");
    out.push_str(&tool.name);
    out.push('\n');
    if let Some(desc) = &tool.description {
        let short_desc = if is_code_mode_orchestrator_tool(tool) {
            compact_code_mode_tool_description(desc, MAX_SLIM_CODE_MODE_TOOL_DESC_BYTES, spill).0
        } else {
            let collapsed = collapse_tool_description(desc);
            compact_middle_chars(&collapsed, 120).0
        };
        if !short_desc.is_empty() {
            out.push_str("  description: ");
            out.push_str(&short_desc);
            out.push('\n');
        }
    }
    match &tool.kind {
        CanonicalToolKind::Function { parameters_json } => {
            out.push_str("  invocation: xml_json_arguments\n  args_schema: ");
            out.push_str(&strip_schema_descriptions(parameters_json));
            out.push('\n');
        }
        CanonicalToolKind::Custom { format_json } => {
            out.push_str("  invocation: xml_freeform_input\n");
            if let Some(fmt) = format_json {
                out.push_str("  format: ");
                out.push_str(&compact_middle_chars(fmt.trim(), 160).0);
                out.push('\n');
            }
        }
    }
}

fn render_minimal_client_tool(tool: &CanonicalToolSpec, out: &mut String) {
    out.push_str("- name: ");
    out.push_str(&tool.name);
    match &tool.kind {
        CanonicalToolKind::Function { parameters_json } => {
            out.push_str(" (json_args");
            if let Ok(serde_json::Value::Object(map)) =
                serde_json::from_str::<serde_json::Value>(parameters_json)
                && let Some(serde_json::Value::Object(props)) = map.get("properties")
                && !props.is_empty()
            {
                let keys: Vec<&str> = props.keys().take(24).map(String::as_str).collect();
                out.push_str(": ");
                out.push_str(&keys.join(", "));
            }
            out.push_str(")\n");
        }
        CanonicalToolKind::Custom { .. } => {
            out.push_str(" (freeform_input)\n");
        }
    }
}

fn strip_schema_descriptions(raw_schema: &str) -> String {
    let Ok(mut val) = serde_json::from_str::<serde_json::Value>(raw_schema) else {
        return compact_middle_chars(raw_schema.trim(), 240).0;
    };
    strip_schema_metadata_value(&mut val);
    serde_json::to_string(&val).unwrap_or_else(|_| compact_middle_chars(raw_schema.trim(), 240).0)
}

fn strip_schema_metadata_value(val: &mut serde_json::Value) {
    match val {
        serde_json::Value::Object(map) => {
            let _ = map.remove("$schema");
            let _ = map.remove("additionalProperties");
            let _ = map.remove("description");
            let _ = map.remove("title");
            let _ = map.remove("examples");
            let _ = map.remove("default");
            for child in map.values_mut() {
                strip_schema_metadata_value(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                strip_schema_metadata_value(item);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

fn render_bounded_tools_list<S: TruncationSpillSink>(
    tools: &[&CanonicalToolSpec],
    max_body_bytes: usize,
    out: &mut String,
    spill: &mut S,
) {
    let note_reserve = 220usize;
    let mut standard = String::new();
    let mut standard_compacted = false;
    for tool in tools {
        standard_compacted |= render_compact_client_tool(tool, &mut standard, spill);
    }
    if standard
        .len()
        .saturating_add(if standard_compacted { note_reserve } else { 0 })
        <= max_body_bytes
    {
        out.push_str(&standard);
        if standard_compacted
            && let Some(catalog_path) = spill.spill_truncated_content(
                "client tools catalog",
                &serialize_full_client_tools_catalog(tools),
            )
        {
            out.push_str(&format!(
                "  # Full lossless client tool descriptions & JSON schemas saved locally at `{catalog_path}`.\n"
            ));
        }
        return;
    }

    let catalog_path = spill.spill_truncated_content(
        "client tools catalog",
        &serialize_full_client_tools_catalog(tools),
    );

    let mut slim = String::new();
    for tool in tools {
        render_slim_client_tool(tool, &mut slim, spill);
    }
    if slim.len().saturating_add(note_reserve) <= max_body_bytes {
        out.push_str(&slim);
        if let Some(path) = &catalog_path {
            out.push_str(&format!(
                "  # Tool schemas compacted to fit prompt budget; full lossless tool catalog saved locally at `{path}`.\n"
            ));
        }
        return;
    }

    let mut minimal = String::new();
    let mut entry_buf = String::new();
    let mut omitted = 0usize;
    let tail_reserve = catalog_path.as_deref().map_or(112, |p| p.len() + 180);
    for tool in tools {
        entry_buf.clear();
        render_minimal_client_tool(tool, &mut entry_buf);
        if minimal.len() + entry_buf.len() + tail_reserve <= max_body_bytes {
            minimal.push_str(&entry_buf);
        } else {
            omitted += 1;
        }
    }
    if omitted > 0 {
        if let Some(path) = &catalog_path {
            minimal.push_str(&format!(
                "  ... [{omitted} additional client tools omitted and schemas compacted to fit single-turn directive budget; full lossless tool catalog saved locally at `{path}`]\n"
            ));
        } else {
            minimal.push_str(&format!(
                "  ... [{omitted} additional client tools omitted to stay within single-turn directive budget]\n"
            ));
        }
    } else if let Some(path) = &catalog_path {
        minimal.push_str(&format!(
            "  # Tool descriptions & schemas compacted to fit single-turn directive budget; full lossless tool catalog saved locally at `{path}`.\n"
        ));
    }
    out.push_str(&minimal);
}

/// Builds an incremental `<client_tools_update>` block when a warm session's active tool list
/// changes across turns (e.g. Plan <-> Default mode, Claude Code `ToolSearch`, Codex lazy app
/// connector activation, or `/compact` disabling tools for a summarization turn).
#[must_use]
pub fn build_client_tools_delta_xml(
    prev_tools: &[CanonicalToolSpec],
    next_tools: &[CanonicalToolSpec],
    tool_nonce: &str,
) -> Option<String> {
    build_client_tools_delta_xml_with_spill(prev_tools, next_tools, tool_nonce, &mut NoopSpillSink)
}

/// Like [`build_client_tools_delta_xml`], persisting full tool catalogs or long removed-tool
/// lists to local files via `spill` whenever compaction or truncation occurs.
#[must_use]
pub fn build_client_tools_delta_xml_with_spill<S: TruncationSpillSink>(
    prev_tools: &[CanonicalToolSpec],
    next_tools: &[CanonicalToolSpec],
    tool_nonce: &str,
    spill: &mut S,
) -> Option<String> {
    if prev_tools == next_tools {
        return None;
    }
    let mut out = String::with_capacity(512);
    out.push_str("<client_tools_update>\n");
    if next_tools.is_empty() {
        out.push_str(&format!(
            "External `<client_tools>` are disabled for this turn; respond in plain text without emitting any `<agy_tool_call_{tool_nonce}>` XML blocks or invoking any `mcp_lg_*` tool calls.\n"
        ));
        out.push_str("</client_tools_update>\n");
        return Some(out);
    }

    if prev_tools.is_empty() {
        let open_tag = format!("<agy_tool_call_{tool_nonce}>");
        let close_tag = format!("</agy_tool_call_{tool_nonce}>");
        out.push_str(&format!(
            "External `<client_tools>` are now enabled. Invoke them ONLY via `{open_tag}\n{{\"name\": \"<exact_tool_name>\", \"arguments\": {{ ... }}}}\n{close_tag}` (or `\"input\"` for custom freeform tools) and stop immediately after the closing tag:\n"
        ));
    }

    let added_or_changed: Vec<&CanonicalToolSpec> = next_tools
        .iter()
        .filter(|nt| !prev_tools.iter().any(|pt| *pt == **nt))
        .collect();
    let removed_names: Vec<&str> = prev_tools
        .iter()
        .filter(|pt| !next_tools.iter().any(|nt| nt.name == pt.name))
        .map(|pt| pt.name.as_str())
        .collect();

    if !removed_names.is_empty() {
        out.push_str("Removed client tools (no longer available): ");
        let joined = removed_names.join(", ");
        if joined.len() > 4_000 {
            out.push_str(&truncate_head_tail_lines_with_spill(
                &joined,
                4_000,
                "removed client tools",
                spill,
            ));
        } else {
            out.push_str(&joined);
        }
        out.push('\n');
    }
    if !added_or_changed.is_empty() {
        out.push_str("Added/updated client tools:\n");
        let remaining_budget = MAX_TOOL_DIRECTIVE_BYTES.saturating_sub(out.len());
        render_bounded_tools_list(
            &added_or_changed,
            remaining_budget.max(4_096),
            &mut out,
            spill,
        );
    }
    out.push_str("</client_tools_update>\n");
    Some(out)
}

/// Builds a compact `<client_tools_update>` block for warm MCP sessions where virtualized
/// dispatch (`mcp_lg_search_client_tools` + `mcp_lg_call_client_tool`) is active, avoiding
/// multi-kilobyte JSON schema dumps when standard `Function` tools are added or updated mid-session.
#[must_use]
pub fn build_mcp_virtualized_tools_delta_xml_with_spill<S: TruncationSpillSink>(
    prev_tools: &[CanonicalToolSpec],
    next_tools: &[CanonicalToolSpec],
    _tool_nonce: &str,
    spill: &mut S,
) -> Option<String> {
    if prev_tools == next_tools {
        return None;
    }
    let mut out = String::with_capacity(512);
    out.push_str("<client_tools_update>\n");
    if next_tools.is_empty() {
        out.push_str(
            "External client tools are disabled for this turn; respond in plain text without invoking any `mcp_lg_*` client tools.\n",
        );
        out.push_str("</client_tools_update>\n");
        return Some(out);
    }

    let removed_names: Vec<&str> = prev_tools
        .iter()
        .filter(|pt| !next_tools.iter().any(|nt| nt.name == pt.name))
        .map(|pt| pt.name.as_str())
        .collect();
    if !removed_names.is_empty() {
        out.push_str("Removed client tools (no longer available): ");
        let joined = removed_names.join(", ");
        if joined.len() > 2_000 {
            out.push_str(&truncate_head_tail_lines_with_spill(
                &joined,
                2_000,
                "removed client tools",
                spill,
            ));
        } else {
            out.push_str(&joined);
        }
        out.push('\n');
    }

    let mut added_mcp_names: Vec<&str> = Vec::new();
    for nt in next_tools {
        if prev_tools.iter().any(|pt| pt == nt) {
            continue;
        }
        added_mcp_names.push(nt.name.as_str());
    }

    if !added_mcp_names.is_empty() {
        out.push_str(
            "Added/updated native client tools (inspect schemas via `mcp_lg_search_client_tools` and invoke via `mcp_lg_call_client_tool` or matching `mcp_lg_ns__*` hub): ",
        );
        let joined = added_mcp_names.join(", ");
        if joined.len() > 2_000 {
            out.push_str(&truncate_head_tail_lines_with_spill(
                &joined,
                2_000,
                "added native client tools",
                spill,
            ));
        } else {
            out.push_str(&joined);
        }
        out.push('\n');
    }

    out.push_str("</client_tools_update>\n");
    Some(out)
}

/// Combines optional compaction system instructions (e.g. Claude Code `/compact` system prompt)
/// and optional `<client_tools_update>` into a single leading directive block when reusing a
/// warm pre-compaction ACP session.
#[must_use]
pub fn build_compaction_directive_xml(
    compaction_system: Option<&str>,
    tools_delta_xml: Option<&str>,
) -> Option<String> {
    build_compaction_directive_xml_with_spill(
        compaction_system,
        tools_delta_xml,
        &mut NoopSpillSink,
    )
}

/// Like [`build_compaction_directive_xml`], persisting oversized compaction instructions to
/// disk via `spill` before head+tail truncation.
#[must_use]
pub fn build_compaction_directive_xml_with_spill<S: TruncationSpillSink>(
    compaction_system: Option<&str>,
    tools_delta_xml: Option<&str>,
    spill: &mut S,
) -> Option<String> {
    let cleaned_sys = compaction_system
        .map(strip_billing_noise)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let tools_xml = tools_delta_xml.filter(|s| !s.trim().is_empty());
    if cleaned_sys.is_none() && tools_xml.is_none() {
        return None;
    }
    let mut out = String::with_capacity(512);
    if let Some(sys) = cleaned_sys {
        let bounded_sys = if sys.len() > 24_000 {
            truncate_head_tail_lines_with_spill(&sys, 24_000, "compaction instructions", spill)
        } else {
            sys
        };
        out.push_str("<caller_compaction_instructions>\n");
        out.push_str(&bounded_sys);
        if !bounded_sys.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("</caller_compaction_instructions>\n");
    }
    if let Some(t_xml) = tools_xml {
        out.push_str(t_xml);
        if !t_xml.ends_with('\n') {
            out.push('\n');
        }
    }
    Some(out)
}
