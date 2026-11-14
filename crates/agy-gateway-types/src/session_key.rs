use sha2::{Digest, Sha256};

use crate::acp::AcpPromptBlock;
use crate::canonical::{
    CanonicalMessage, CanonicalPart, CanonicalRole, CanonicalToolKind, CanonicalToolSpec,
};
use crate::config::ModelId;

pub type ConversationHash = [u8; 32];

/// Internal tool name used when server-side `web_fetch` (`web_fetch_20250910` / `web_fetch`)
/// is requested on `/v1/messages` or `/v1/responses`.
pub const INTERNAL_GATEWAY_WEB_FETCH_TOOL: &str = "__gateway_web_fetch";

/// Internal tool name used when OpenAI Codex declares built-in `{"type": "local_shell"}`.
pub const INTERNAL_CODEX_LOCAL_SHELL_TOOL: &str = "__codex_local_shell";

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

/// Computes deterministic prefix & full hashes for multi-turn ACP session continuation.
///
/// If the request ends with one or more `User` messages following an `Assistant` message,
/// `prefix_lookup` holds `(prefix_hash, delta_start_index)` where `prefix_hash` hashes
/// `(system, tools, web_flags, model, messages[..delta_start_index])`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRoutingPlan {
    pub prefix_lookup: Option<(ConversationHash, usize)>,
    pub base_state_hash_before_reply: ConversationHash,
}

#[must_use]
pub fn compute_routing_plan(
    system: Option<&str>,
    tools: &[CanonicalToolSpec],
    web_cfg: WebToolBridgeConfig,
    model: &ModelId,
    messages: &[CanonicalMessage],
) -> SessionRoutingPlan {
    let last_assistant_idx = messages
        .iter()
        .rposition(|m| m.role == CanonicalRole::Assistant);

    let prefix_lookup = last_assistant_idx.and_then(|idx| {
        let split_at = idx + 1;
        if split_at < messages.len() {
            let prefix_slice = messages.get(..split_at)?;
            let prefix_hash = hash_conversation_state(system, tools, web_cfg, model, prefix_slice);
            Some((prefix_hash, split_at))
        } else {
            None
        }
    });

    let base_state_hash_before_reply =
        hash_conversation_state(system, tools, web_cfg, model, messages);

    SessionRoutingPlan {
        prefix_lookup,
        base_state_hash_before_reply,
    }
}

/// Extends an existing conversation hash with the newly generated assistant message so the
/// idle session pool can index the updated state for the next turn.
#[must_use]
pub fn hash_conversation_with_reply(
    system: Option<&str>,
    tools: &[CanonicalToolSpec],
    web_cfg: WebToolBridgeConfig,
    model: &ModelId,
    prior_messages: &[CanonicalMessage],
    assistant_reply: &CanonicalMessage,
) -> ConversationHash {
    let mut hasher = init_base_hasher(system, tools, web_cfg, model);
    for msg in prior_messages {
        feed_message_hash(&mut hasher, msg);
    }
    feed_message_hash(&mut hasher, assistant_reply);
    hasher.finalize().into()
}

#[must_use]
pub fn hash_conversation_state(
    system: Option<&str>,
    tools: &[CanonicalToolSpec],
    web_cfg: WebToolBridgeConfig,
    model: &ModelId,
    messages: &[CanonicalMessage],
) -> ConversationHash {
    let mut hasher = init_base_hasher(system, tools, web_cfg, model);
    for msg in messages {
        feed_message_hash(&mut hasher, msg);
    }
    hasher.finalize().into()
}

fn init_base_hasher(
    system: Option<&str>,
    tools: &[CanonicalToolSpec],
    web_cfg: WebToolBridgeConfig,
    model: &ModelId,
) -> Sha256 {
    let mut hasher = Sha256::new();
    hasher.update(b"agy-acp-gateway-v2\0");
    hasher.update(model.as_str().as_bytes());
    hasher.update([
        u8::from(web_cfg.enable_native_web_search),
        u8::from(web_cfg.enable_native_web_fetch),
    ]);
    hasher.update(b"\0sys\0");
    if let Some(sys) = system {
        let cleaned = strip_unstable_harness_noise(sys);
        hasher.update(cleaned.trim().as_bytes());
    }
    hasher.update(b"\0tools\0");
    for tool in tools {
        hasher.update(tool.name.as_bytes());
        hasher.update(b"\x1f");
        match &tool.kind {
            CanonicalToolKind::Function { parameters_json } => {
                hasher.update(b"fn:");
                hasher.update(parameters_json.as_bytes());
            }
            CanonicalToolKind::Custom { format_json } => {
                hasher.update(b"custom:");
                if let Some(fmt) = format_json {
                    hasher.update(fmt.as_bytes());
                }
            }
        }
        hasher.update(b"\x1e");
    }
    hasher
}

fn feed_message_hash(hasher: &mut Sha256, msg: &CanonicalMessage) {
    match msg.role {
        CanonicalRole::User => hasher.update(b"\0role:user\0"),
        CanonicalRole::Assistant => hasher.update(b"\0role:assistant\0"),
    }
    for part in &msg.parts {
        match part {
            CanonicalPart::Text(text) => {
                let cleaned = strip_unstable_harness_noise(text);
                let trimmed = cleaned.trim();
                if !trimmed.is_empty() {
                    hasher.update(b"txt:");
                    hasher.update(trimmed.as_bytes());
                    hasher.update(b"\x1d");
                }
            }
            CanonicalPart::ImageBase64 { media_type, data } => {
                hasher.update(b"img:");
                hasher.update(media_type.as_bytes());
                hasher.update(b":");
                hasher.update(data.as_bytes());
                hasher.update(b"\x1d");
            }
            CanonicalPart::Reasoning(_) => {
                // Intentionally omitted from cross-turn match key because some clients drop
                // reasoning summaries when resending history.
            }
            CanonicalPart::ToolCall {
                call_id: _,
                name,
                arguments,
                is_custom,
            } => {
                hasher.update(if *is_custom { b"ctool:" } else { b"ftool:" });
                hasher.update(name.as_bytes());
                hasher.update(b":");
                hasher.update(canonicalize_tool_call_for_hash(name, arguments).as_bytes());
                hasher.update(b"\x1d");
            }
            CanonicalPart::ToolResult {
                call_id: _,
                output,
                is_error,
            } => {
                let cleaned = strip_unstable_harness_noise(output);
                hasher.update(if *is_error { b"terr:" } else { b"tres:" });
                hasher.update(cleaned.trim().as_bytes());
                hasher.update(b"\x1d");
            }
        }
    }
}

fn canonicalize_tool_call_for_hash(name: &str, raw: &str) -> String {
    if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(raw) {
        if name == INTERNAL_CODEX_LOCAL_SHELL_TOOL
            && let serde_json::Value::Object(map) = &mut val
        {
            let _ = map.remove("type");
            map.retain(|k, v| {
                !(v.is_null() || k == "env" && v.as_object().is_some_and(serde_json::Map::is_empty))
            });
        }
        if let Ok(compact) = serde_json::to_string(&val) {
            return compact;
        }
    }
    raw.trim().to_owned()
}

/// Strips volatile per-request attribution prefixes and token/timestamp counters injected by
/// Claude Code (`x-anthropic-billing-header`, `cch=...`, `<total_tokens>`, `<budget:token_budget>`)
/// and OpenAI Codex (`<current_time>`, `<timestamp>`) so both backend Gemini KV prefix caching
/// and `SessionPool` stateful prefix-hash matching achieve maximal hit rates.
#[must_use]
pub fn strip_unstable_harness_noise(input: &str) -> String {
    let line_filtered = strip_billing_and_cch_tokens(input);
    let s1 = strip_tag_blocks(&line_filtered, "<total_tokens>", "</total_tokens>");
    let s2 = strip_tag_blocks(&s1, "<budget:token_budget>", "</budget:token_budget>");
    let s3 = strip_tag_blocks(&s2, "<fast_mode_info>", "</fast_mode_info>");
    let s4 = strip_tag_blocks(&s3, "<current_time>", "</current_time>");
    let without_volatile_tags = strip_tag_blocks(&s4, "<timestamp>", "</timestamp>");

    if !without_volatile_tags.contains("<system-reminder>") {
        return without_volatile_tags;
    }

    // Collapse `<system-reminder>` blocks that became whitespace-only after removing volatile tags
    let mut out = String::with_capacity(without_volatile_tags.len());
    let mut rest = without_volatile_tags.as_str();
    let open = "<system-reminder>";
    let close = "</system-reminder>";
    while let Some(start) = rest.find(open) {
        let Some((before, after_open_pos)) = rest.split_at_checked(start) else {
            break;
        };
        out.push_str(before);
        let Some((_, inside_and_after)) = after_open_pos.split_at_checked(open.len()) else {
            rest = "";
            break;
        };
        if let Some(end) = inside_and_after.find(close) {
            let Some((inside, with_close)) = inside_and_after.split_at_checked(end) else {
                rest = "";
                break;
            };
            let Some((_, after_close)) = with_close.split_at_checked(close.len()) else {
                rest = "";
                break;
            };
            if !inside.trim().is_empty() {
                out.push_str(open);
                out.push_str(inside);
                out.push_str(close);
            }
            rest = after_close;
        } else {
            out.push_str(after_open_pos);
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

fn strip_billing_and_cch_tokens(input: &str) -> String {
    let lower_probe_needed = input.contains("x-anthropic-billing-header")
        || input.contains("X-Anthropic-Billing-Header")
        || input.contains("cch=");
    if !lower_probe_needed {
        return input.to_owned();
    }

    let mut out = String::with_capacity(input.len());
    let mut first = true;
    for line in input.lines() {
        let trimmed_start = line.trim_start();
        if trimmed_start
            .get(..27)
            .is_some_and(|p| p.eq_ignore_ascii_case("x-anthropic-billing-header:"))
        {
            continue;
        }
        if !first {
            out.push('\n');
        }
        first = false;
        if line.contains("cch=") {
            out.push_str(&strip_inline_cch(line));
        } else {
            out.push_str(line);
        }
    }
    out
}

fn strip_inline_cch(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(pos) = rest.find("cch=") {
        let Some((before, from_cch)) = rest.split_at_checked(pos) else {
            break;
        };
        // Only strip when `cch=` is a standalone token boundary (start of string or preceded by whitespace/`;`/`,`)
        let valid_boundary = before
            .chars()
            .next_back()
            .is_none_or(|ch| ch.is_whitespace() || matches!(ch, ';' | ',' | '(' | '[' | '|'));
        let Some((_, after_eq)) = from_cch.split_at_checked(4) else {
            break;
        };
        let hex_len = after_eq.bytes().take_while(u8::is_ascii_hexdigit).count();
        if valid_boundary && (4..=16).contains(&hex_len) {
            out.push_str(before.trim_end_matches(' '));
            let Some((_, mut after_hex)) = after_eq.split_at_checked(hex_len) else {
                rest = "";
                break;
            };
            if let Some(stripped_semi) = after_hex.strip_prefix(';') {
                after_hex = stripped_semi.strip_prefix(' ').unwrap_or(stripped_semi);
            }
            rest = after_hex;
        } else {
            out.push_str(before);
            out.push_str("cch=");
            rest = after_eq;
        }
    }
    out.push_str(rest);
    out
}

fn strip_tag_blocks(input: &str, open_tag: &str, close_tag: &str) -> String {
    if !input.contains(open_tag) {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find(open_tag) {
        let Some((before, after_start)) = rest.split_at_checked(start) else {
            break;
        };
        out.push_str(before);
        let Some((_, inside_and_after)) = after_start.split_at_checked(open_tag.len()) else {
            rest = "";
            break;
        };
        if let Some(end) = inside_and_after.find(close_tag) {
            let Some((_, after_close)) = inside_and_after.split_at_checked(end + close_tag.len())
            else {
                rest = "";
                break;
            };
            rest = after_close;
        } else {
            out.push_str(after_start);
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

fn truncate_utf8_chars(input: &str, max_chars: usize) -> String {
    let Some((byte_idx, _)) = input.char_indices().nth(max_chars) else {
        return input.to_owned();
    };
    let Some((head, _)) = input.split_at_checked(byte_idx) else {
        return input.to_owned();
    };
    let mut truncated = head.trim_end().to_owned();
    truncated.push_str("...");
    truncated
}

fn compact_tool_description(raw_desc: &str) -> String {
    let s1 = strip_tag_blocks(raw_desc, "<example>", "</example>");
    let s2 = strip_tag_blocks(&s1, "<examples>", "</examples>");
    let s3 = strip_tag_blocks(&s2, "<commentary>", "</commentary>");
    let mut collapsed = String::with_capacity(s3.len().min(700));
    let mut prev_blank = false;
    for line in s3.lines() {
        let trimmed_end = line.trim_end();
        if trimmed_end.trim().is_empty() {
            if !prev_blank && !collapsed.is_empty() {
                collapsed.push('\n');
                prev_blank = true;
            }
        } else {
            if !collapsed.is_empty() && !collapsed.ends_with('\n') {
                collapsed.push('\n');
            }
            collapsed.push_str(trimmed_end);
            prev_blank = false;
        }
    }
    truncate_utf8_chars(collapsed.trim(), 600)
}

fn compact_json_schema(raw_schema: &str) -> String {
    let Ok(mut val) = serde_json::from_str::<serde_json::Value>(raw_schema) else {
        return raw_schema.trim().to_owned();
    };
    slim_schema_value(&mut val);
    serde_json::to_string(&val).unwrap_or_else(|_| raw_schema.trim().to_owned())
}

fn slim_schema_value(val: &mut serde_json::Value) {
    match val {
        serde_json::Value::Object(map) => {
            let _ = map.remove("$schema");
            let _ = map.remove("additionalProperties");
            if let Some(serde_json::Value::String(desc)) = map.get_mut("description")
                && desc.chars().count() > 140
            {
                *desc = truncate_utf8_chars(desc, 140);
            }
            for child in map.values_mut() {
                slim_schema_value(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                slim_schema_value(item);
            }
        }
        _ => {}
    }
}

fn compact_caller_system(raw_sys: &str) -> String {
    let cleaned = strip_unstable_harness_noise(raw_sys);
    if cleaned.len() <= 8_000 {
        return cleaned.trim().to_owned();
    }
    let without_examples = strip_tag_blocks(&cleaned, "<examples>", "</examples>");
    let without_single = strip_tag_blocks(&without_examples, "<example>", "</example>");
    without_single.trim().to_owned()
}

/// Builds the `_meta.antigravity.systemPrompt.append` payload passed to ACP `session/new`.
#[must_use]
pub fn build_session_system_append(
    caller_system: Option<&str>,
    client_tools: &[CanonicalToolSpec],
    web_cfg: WebToolBridgeConfig,
    tool_nonce: &str,
) -> String {
    let open_tag = format!("<agy_tool_call_{tool_nonce}>");
    let close_tag = format!("</agy_tool_call_{tool_nonce}>");
    let mut out = String::with_capacity(4096);
    out.push_str("<api_gateway_bridge_rules>\n");
    out.push_str(&format!(
        "You are serving an external API harness via a structured protocol gateway.\n\
         - NEVER invoke your internal host workspace tools (`read_file`, `write_file`, `edit_file`, \
         `run_command`, `manage_task`, `ask_question`, `invoke_subagent`, `define_subagent`, \
         `manage_subagents`, `run_workflow`, `swarm`, `schedule`, `send_message`).\n\
         - Ignore any internal ACP host directory such as `agy-acp-gateway-empty-cwd`; it is only an \
         inert gateway sandbox folder. External `<client_tools>` (`Bash`, `Read`, `Edit`, `Write`, \
         `Grep`, `Glob`, etc.) execute directly in the caller's actual workspace directory described \
         in `<caller_system_instructions>` and caller environment blocks.\n\
         - CRITICAL: NEVER attempt to call external `<client_tools>` names (such as `Bash`, `Read`, \
         `Edit`, `Write`, `Grep`, `Glob`, `WebSearch`, `WebFetch`, `Agent`, `apply_patch`, etc.) \
         through your native function-calling tool channel! They are NOT native function declarations; \
         calling them natively causes a malformed function-call error. External `<client_tools>` may \
         ONLY be invoked by writing `{open_tag}...{close_tag}` XML blocks in plain assistant text.\n\
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

    let need_xml_protocol = !client_tools.is_empty() || web_cfg.enable_native_web_fetch;
    if need_xml_protocol {
        out.push_str(
            "- Whenever you want to invoke one or more XML-bridged tools below (including parallel calls \
             in a single turn), optionally write a brief one-sentence status note first, then emit one XML \
             block per tool call in this EXACT format (no XML attributes on the tag), and IMMEDIATELY \
             STOP generating output right after the closing tag — NEVER guess, fabricate, or simulate \
             tool outputs or future turns before the caller returns `[Client Tool Result ...]`:\n",
        );
        out.push_str(&open_tag);
        out.push('\n');
        out.push_str("{\"name\": \"<exact_tool_name>\", \"arguments\": { ... }}\n");
        out.push_str(&close_tag);
        out.push_str(
            "\n  (For freeform string tools like `apply_patch`, pass a JSON string in `\"input\"`: \
             `{\"name\": \"apply_patch\", \"input\": \"...\"}`.)\n",
        );

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

        if !client_tools.is_empty() {
            out.push_str("<client_tools>\n");
            for tool in client_tools {
                out.push_str("- name: ");
                out.push_str(&tool.name);
                out.push('\n');
                if let Some(desc) = &tool.description {
                    let compacted_desc = compact_tool_description(desc);
                    if !compacted_desc.is_empty() {
                        out.push_str("  description: ");
                        out.push_str(&compacted_desc);
                        out.push('\n');
                    }
                }
                match &tool.kind {
                    CanonicalToolKind::Function { parameters_json } => {
                        out.push_str("  invocation: xml_json_arguments\n  args_schema: ");
                        out.push_str(&compact_json_schema(parameters_json));
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
            }
            out.push_str("</client_tools>\n");
        }
    }
    out.push_str("</api_gateway_bridge_rules>\n");

    if let Some(sys) = caller_system {
        let compacted_sys = compact_caller_system(sys);
        if !compacted_sys.is_empty() {
            out.push_str("\n<caller_system_instructions>\n");
            out.push_str(&compacted_sys);
            out.push_str("\n</caller_system_instructions>\n");
        }
    }

    out
}

/// Converts a slice of `CanonicalMessage` turns into ACP `AcpPromptBlock`s.
///
/// When `is_full_history_replay` is true and there are prior turns before the latest turn,
/// earlier turns are formatted inside `<conversation_history>` so the model has full context.
#[must_use]
pub fn build_acp_prompt_blocks(
    messages: &[CanonicalMessage],
    tool_nonce: &str,
    is_full_history_replay: bool,
) -> Vec<AcpPromptBlock> {
    let mut blocks = Vec::new();
    if messages.is_empty() {
        return blocks;
    }

    let split_history_at = if is_full_history_replay && messages.len() > 1 {
        messages
            .iter()
            .rposition(|m| m.role == CanonicalRole::Assistant)
            .map_or(0, |idx| idx + 1)
    } else {
        0
    };

    let (history_slice, active_slice) = messages
        .split_at_checked(split_history_at)
        .unwrap_or((&[], messages));

    let mut text_acc = String::new();
    if !history_slice.is_empty() {
        text_acc.push_str(
            "--- Prior Conversation Context (Read-Only Archive; Do NOT echo these headers) ---\n",
        );
        for msg in history_slice {
            let role_header = match msg.role {
                CanonicalRole::User => "### Prior User Turn\n",
                CanonicalRole::Assistant => "### Prior Assistant Turn\n",
            };
            if !text_acc.ends_with('\n') {
                text_acc.push('\n');
            }
            text_acc.push_str(role_header);
            render_parts_into(msg, tool_nonce, true, &mut text_acc, &mut blocks);
            if !text_acc.ends_with('\n') {
                text_acc.push('\n');
            }
        }
        text_acc.push_str(
            "--- End Prior Conversation Context ---\n\n### Current Caller Input (Respond ONLY for the current assistant step)\n",
        );
    }

    for (idx, msg) in active_slice.iter().enumerate() {
        if idx > 0 && !text_acc.is_empty() && !text_acc.ends_with('\n') {
            text_acc.push('\n');
        }
        render_parts_into(msg, tool_nonce, false, &mut text_acc, &mut blocks);
    }

    if !text_acc.is_empty() {
        blocks.push(AcpPromptBlock::Text { text: text_acc });
    }

    if blocks.is_empty() {
        blocks.push(AcpPromptBlock::Text {
            text: " ".to_owned(),
        });
    }

    blocks
}

fn render_parts_into(
    msg: &CanonicalMessage,
    tool_nonce: &str,
    is_archival: bool,
    text_acc: &mut String,
    blocks: &mut Vec<AcpPromptBlock>,
) {
    for part in &msg.parts {
        match part {
            CanonicalPart::Text(text) => {
                let cleaned = strip_unstable_harness_noise(text);
                if !cleaned.trim().is_empty() {
                    if !text_acc.is_empty() && !text_acc.ends_with('\n') {
                        text_acc.push('\n');
                    }
                    text_acc.push_str(&cleaned);
                }
            }
            CanonicalPart::ImageBase64 { media_type, data } => {
                if !text_acc.is_empty() {
                    blocks.push(AcpPromptBlock::Text {
                        text: std::mem::take(text_acc),
                    });
                }
                blocks.push(AcpPromptBlock::Image {
                    data: data.clone(),
                    mime_type: media_type.clone(),
                });
            }
            CanonicalPart::Reasoning(_) => {}
            CanonicalPart::ToolCall {
                call_id,
                name,
                arguments,
                is_custom,
            } => {
                if !text_acc.is_empty() && !text_acc.ends_with('\n') {
                    text_acc.push('\n');
                }
                if is_archival {
                    // Render historical tool invocations without live `<agy_tool_call_...>` XML tags
                    // so multi-turn cold replays never prime the model to continue generating fake
                    // tool results after its own closing `</agy_tool_call_...>` tag.
                    text_acc.push_str(&format!(
                        "[Completed Tool Invocation id={call_id} name={name}]: {}\n",
                        arguments.trim()
                    ));
                } else {
                    let field_key = if *is_custom { "input" } else { "arguments" };
                    let arg_repr = if *is_custom {
                        serde_json::to_string(arguments).unwrap_or_else(|_| "\"\"".to_owned())
                    } else {
                        arguments.clone()
                    };
                    text_acc.push_str(&format!(
                        "<agy_tool_call_{tool_nonce}>\n\
                         {{\"name\": {}, \"{field_key}\": {arg_repr}}}\n\
                         </agy_tool_call_{tool_nonce}>\n",
                        serde_json::to_string(name).unwrap_or_else(|_| "\"tool\"".to_owned())
                    ));
                }
            }
            CanonicalPart::ToolResult {
                call_id,
                output,
                is_error,
            } => {
                if !text_acc.is_empty() && !text_acc.ends_with('\n') {
                    text_acc.push('\n');
                }
                let cleaned_output = strip_unstable_harness_noise(output);
                text_acc.push_str(&format!(
                    "[Client Tool Result id={call_id} status={}]:\n{}\n[End Client Tool Result]\n",
                    if *is_error { "error" } else { "ok" },
                    cleaned_output.trim()
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_changing_cch_and_total_tokens_preserves_prefix_hash_reuse() {
        let model = ModelId::try_new("gemini-4-carbon-high").expect("valid model");
        let web_cfg = WebToolBridgeConfig::default();
        let tools = vec![CanonicalToolSpec {
            name: "WebSearch".to_owned(),
            description: Some("Search the web".to_owned()),
            kind: CanonicalToolKind::Function {
                parameters_json:
                    "{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\"}}}"
                        .to_owned(),
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
}
