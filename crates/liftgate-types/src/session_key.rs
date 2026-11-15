use sha2::{Digest, Sha256};

use crate::acp::AcpPromptBlock;
use crate::canonical::{
    CanonicalMessage, CanonicalPart, CanonicalRole, CanonicalToolKind, CanonicalToolSpec,
};
use crate::config::ModelId;
use crate::error::ProtocolError;

pub type ConversationHash = [u8; 32];

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

/// Computes deterministic prefix & full hashes for multi-turn ACP session continuation.
///
/// If the request ends with one or more `User` messages following an `Assistant` message,
/// `prefix_lookup` holds `(prefix_hash, delta_start_index)` where `prefix_hash` hashes
/// `(system, web_flags, model, messages[..delta_start_index])`. Client tools are synced
/// incrementally on warm sessions via `<client_tools_update>` so mode switches, `ToolSearch`,
/// lazy app namespaces, and `/compact` never invalidate warm KV caches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRoutingPlan {
    pub prefix_lookup: Option<(ConversationHash, usize)>,
    pub compaction_lookup: Option<(ConversationHash, usize)>,
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

    let compaction_lookup = if is_compaction_summarization_turn(system, messages, tools) {
        last_assistant_idx.and_then(|idx| {
            let split_at = idx + 1;
            if split_at < messages.len() {
                let prefix_slice = messages.get(..split_at)?;
                Some((hash_dialogue_state(model, prefix_slice), split_at))
            } else {
                None
            }
        })
    } else {
        None
    };

    let base_state_hash_before_reply =
        hash_conversation_state(system, tools, web_cfg, model, messages);

    SessionRoutingPlan {
        prefix_lookup,
        compaction_lookup,
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
    _tools: &[CanonicalToolSpec],
    web_cfg: WebToolBridgeConfig,
    model: &ModelId,
    messages: &[CanonicalMessage],
) -> ConversationHash {
    let mut hasher = init_base_hasher(system, _tools, web_cfg, model);
    for msg in messages {
        feed_message_hash(&mut hasher, msg);
    }
    hasher.finalize().into()
}

/// Computes a pure dialogue hash (`(model, non-developer-instruction parts)`) so harness
/// `/compact` summarization requests (which swap `system`/`instructions` and clear `tools`)
/// can still hit the warm pre-compaction ACP session with zero cold re-upload.
#[must_use]
pub fn hash_dialogue_state(_model: &ModelId, messages: &[CanonicalMessage]) -> ConversationHash {
    let mut hasher = init_dialogue_hasher();
    for msg in messages {
        feed_dialogue_message_hash(&mut hasher, msg);
    }
    hasher.finalize().into()
}

#[must_use]
pub fn hash_dialogue_with_reply(
    _model: &ModelId,
    prior_messages: &[CanonicalMessage],
    assistant_reply: &CanonicalMessage,
) -> ConversationHash {
    let mut hasher = init_dialogue_hasher();
    for msg in prior_messages {
        feed_dialogue_message_hash(&mut hasher, msg);
    }
    feed_dialogue_message_hash(&mut hasher, assistant_reply);
    hasher.finalize().into()
}

fn init_dialogue_hasher() -> Sha256 {
    let mut hasher = Sha256::new();
    hasher.update(b"agy-acp-dialogue-v1\0");
    hasher
}

fn init_base_hasher(
    system: Option<&str>,
    _tools: &[CanonicalToolSpec],
    web_cfg: WebToolBridgeConfig,
    _model: &ModelId,
) -> Sha256 {
    let mut hasher = Sha256::new();
    hasher.update(b"agy-acp-gateway-v3\0");
    hasher.update([
        u8::from(web_cfg.enable_native_web_search),
        u8::from(web_cfg.enable_native_web_fetch),
    ]);
    hasher.update(b"\0sys\0");
    if let Some(sys) = system {
        let cleaned = strip_unstable_harness_noise(sys);
        hasher.update(cleaned.trim().as_bytes());
    }
    hasher
}

fn feed_message_hash(hasher: &mut Sha256, msg: &CanonicalMessage) {
    if !message_has_stable_hash_content(msg, false) {
        return;
    }
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

fn feed_dialogue_message_hash(hasher: &mut Sha256, msg: &CanonicalMessage) {
    if !message_has_stable_hash_content(msg, true) {
        return;
    }
    match msg.role {
        CanonicalRole::User => hasher.update(b"\0role:user\0"),
        CanonicalRole::Assistant => hasher.update(b"\0role:assistant\0"),
    }
    for part in &msg.parts {
        match part {
            CanonicalPart::Text(text) => {
                if is_developer_instructions_wrapper(text) {
                    continue;
                }
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
            CanonicalPart::Reasoning(_) => {}
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

fn message_has_stable_hash_content(msg: &CanonicalMessage, skip_developer_wrappers: bool) -> bool {
    for part in &msg.parts {
        match part {
            CanonicalPart::Text(text) => {
                if skip_developer_wrappers && is_developer_instructions_wrapper(text) {
                    continue;
                }
                if !strip_unstable_harness_noise(text).trim().is_empty() {
                    return true;
                }
            }
            CanonicalPart::ImageBase64 { .. }
            | CanonicalPart::ToolCall { .. }
            | CanonicalPart::ToolResult { .. } => return true,
            CanonicalPart::Reasoning(_) => {}
        }
    }
    false
}

fn is_developer_instructions_wrapper(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with("<developer_instructions>")
        && trimmed.ends_with("</developer_instructions>")
}

/// Cleans non-semantic billing noise from a caller `system` / `developer` instruction string
/// and wraps it in `<developer_instructions>` (idempotently, never double-wrapping).
#[must_use]
pub fn wrap_developer_instructions_block(raw_text: &str) -> Option<String> {
    let cleaned = strip_billing_noise(raw_text);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    if is_developer_instructions_wrapper(trimmed) {
        Some(trimmed.to_owned())
    } else {
        Some(format!(
            "<developer_instructions>\n{trimmed}\n</developer_instructions>"
        ))
    }
}

/// Protocol-agnostic normalizer that materializes top-level caller `system` / `instructions`
/// into `<developer_instructions>` blocks inside `messages` instead of
/// `_meta.antigravity.systemPrompt.append`, keeping every harness (Claude Code, Codex CLI,
/// OpenCode, etc.) immune to upstream `systemPrompt.append` `429` rejections while preserving
/// warm `SessionPool` prefix and `/compact` dialogue hashes.
pub fn materialize_system_prompt_into_messages(
    raw_system: Option<&str>,
    has_previous_response_anchor: bool,
    client_tools: &[CanonicalToolSpec],
    messages: &mut Vec<CanonicalMessage>,
) {
    if has_previous_response_anchor {
        return;
    }
    let Some(sys_raw) = raw_system else {
        return;
    };
    let Some(wrapped) = wrap_developer_instructions_block(sys_raw) else {
        return;
    };
    if client_tools.is_empty() && is_compaction_instructions_prompt(&wrapped) {
        if let Some(last_user) = messages
            .last_mut()
            .filter(|m| m.role == CanonicalRole::User)
        {
            last_user.parts.push(CanonicalPart::Text(wrapped));
        } else {
            messages.push(CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text(wrapped)],
            });
        }
    } else {
        messages.insert(
            0,
            CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text(wrapped)],
            },
        );
    }
}

/// Returns `true` when an `instructions` / `system` string is a harness compaction prompt
/// (such as OpenAI Codex's `CONTEXT CHECKPOINT COMPACTION` or Claude Code's `/compact` prompt).
#[must_use]
pub fn is_compaction_instructions_prompt(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("context checkpoint compaction")
        || lower.contains("handoff summary for another llm")
        || (lower.contains("compaction") && lower.contains("another language model"))
        || lower.contains("create a detailed summary of the conversation so far")
        || lower.contains("tasked with summarizing conversations")
}

/// Detects whether an incoming turn is a local context-compaction summarization request
/// so `SessionPool` can reuse the warm pre-compaction session and retire it afterward.
#[must_use]
pub fn is_compaction_summarization_turn(
    system: Option<&str>,
    messages: &[CanonicalMessage],
    tools: &[CanonicalToolSpec],
) -> bool {
    if !tools.is_empty() {
        return false;
    }
    if system.is_some_and(is_compaction_instructions_prompt) {
        return true;
    }
    let Some(last_user) = messages
        .iter()
        .rev()
        .find(|m| m.role == CanonicalRole::User)
    else {
        return false;
    };
    for part in &last_user.parts {
        if let CanonicalPart::Text(t) = part {
            let lower = t.to_ascii_lowercase();
            if is_compaction_instructions_prompt(t)
                || lower.contains("create a detailed summary of the conversation so far")
                || lower.contains("summarize the conversation so far")
                || (lower.contains("handoff summary") && lower.contains("progress"))
            {
                return true;
            }
        }
    }
    false
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

/// Strips ONLY non-semantic billing/attribution noise (`x-anthropic-billing-header`, `cch=...`,
/// `<fast_mode_info>`) while keeping runtime context signals (`<current_time>`,
/// `<current_time_reminder>`, `<timestamp>`, `<budget:token_budget>`, `<total_tokens>`,
/// `<context_window>`, `<rollout_budget>`) completely intact for the model.
#[must_use]
pub fn strip_billing_noise(input: &str) -> String {
    let line_filtered = strip_billing_and_cch_tokens(input);
    let without_fast_mode =
        strip_tag_blocks(&line_filtered, "<fast_mode_info>", "</fast_mode_info>");
    collapse_empty_wrapper_blocks(
        &without_fast_mode,
        "<system-reminder>",
        "</system-reminder>",
    )
}

/// Strips volatile per-request attribution prefixes and token/timestamp counters injected by
/// Claude Code (`x-anthropic-billing-header`, `cch=...`, `<total_tokens>`, `<budget:token_budget>`)
/// and OpenAI Codex (`<current_time>`, `<current_time_reminder>`, `<timestamp>`, `<context_window>`,
/// `<rollout_budget>`) when computing deterministic `SessionPool` prefix hashes.
#[must_use]
pub fn strip_unstable_harness_noise(input: &str) -> String {
    let base = strip_billing_noise(input);
    let s1 = strip_tag_blocks(&base, "<total_tokens>", "</total_tokens>");
    let s2 = strip_tag_blocks(&s1, "<budget:token_budget>", "</budget:token_budget>");
    let s3 = strip_tag_blocks(&s2, "<context_window>", "</context_window>");
    let s4 = strip_tag_blocks(
        &s3,
        "<context_window_reminder>",
        "</context_window_reminder>",
    );
    let s5 = strip_tag_blocks(&s4, "<rollout_budget>", "</rollout_budget>");
    let s6 = strip_tag_blocks(&s5, "<current_time>", "</current_time>");
    let s7 = strip_tag_blocks(&s6, "<current_time_reminder>", "</current_time_reminder>");
    let s8 = strip_tag_blocks(
        &s7,
        "<current_time_unavailable>",
        "</current_time_unavailable>",
    );
    let without_volatile_tags = strip_tag_blocks(&s8, "<timestamp>", "</timestamp>");
    let collapsed_rem = collapse_empty_wrapper_blocks(
        &without_volatile_tags,
        "<system-reminder>",
        "</system-reminder>",
    );
    collapse_empty_wrapper_blocks(
        &collapsed_rem,
        "<developer_instructions>",
        "</developer_instructions>",
    )
}

fn collapse_empty_wrapper_blocks(input: &str, open: &str, close: &str) -> String {
    if !input.contains(open) {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
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

const MAX_CODE_MODE_TOOL_DESC_BYTES: usize = 36_000;
const MAX_SLIM_CODE_MODE_TOOL_DESC_BYTES: usize = 24_000;

fn is_code_mode_orchestrator_tool(tool: &CanonicalToolSpec) -> bool {
    if tool.name == "REPL" {
        return true;
    }
    if tool.name == "exec" && matches!(tool.kind, CanonicalToolKind::Custom { .. }) {
        return true;
    }
    tool.description.as_deref().is_some_and(|desc| {
        desc.contains("declare const tools")
            || desc.contains("globalThis.tools")
            || desc.contains("Tool TypeScript declarations:")
            || desc.contains("// @exec:")
    })
}

fn collapse_tool_description(raw_desc: &str) -> String {
    let s1 = strip_tag_blocks(raw_desc, "<example>", "</example>");
    let s2 = strip_tag_blocks(&s1, "<examples>", "</examples>");
    let s3 = strip_tag_blocks(&s2, "<commentary>", "</commentary>");
    let mut collapsed = String::with_capacity(s3.len());
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
    collapsed.trim().to_owned()
}

fn compact_tool_description(raw_desc: &str) -> String {
    let collapsed = collapse_tool_description(raw_desc);
    truncate_utf8_chars(&collapsed, 600)
}

fn compact_code_mode_tool_description(raw_desc: &str, max_bytes: usize) -> String {
    let collapsed = collapse_tool_description(raw_desc);
    if collapsed.len() <= max_bytes {
        collapsed
    } else {
        truncate_head_tail_lines(&collapsed, max_bytes, "Code Mode tool declarations")
    }
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
    let cleaned = strip_billing_noise(raw_sys);
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
         - Ignore any internal ACP host directory such as `liftgate-empty-cwd`; it is only an \
         inert gateway sandbox folder. External `<client_tools>` (`Bash`, `Read`, `Edit`, `Write`, \
         `Grep`, `Glob`, etc.) execute directly in the caller's actual workspace directory described \
         in `<developer_instructions>`, `<caller_system_instructions>`, and caller environment blocks.\n\
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
            for tool in client_tools {
                render_compact_client_tool(tool, &mut out);
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

fn append_xml_invocation_bridge_rules(
    out: &mut String,
    open_tag: &str,
    close_tag: &str,
    web_cfg: WebToolBridgeConfig,
    has_custom_tools: bool,
) {
    out.push_str(
        "- Whenever you want to invoke one or more XML-bridged tools below (including parallel calls \
         in a single turn), optionally write a brief one-sentence status note before the first XML \
         block, then emit one XML block per tool call in this EXACT format (no XML attributes, \
         NEVER wrap the tag in backticks/code fences, and JSON-escape all `\\`, `\"`, and `\\n` inside strings), \
         and END YOUR TURN IMMEDIATELY with ZERO trailing text, explanation, or native \
         function calls after the final closing tag — NEVER guess, fabricate, or simulate tool \
         outputs or future turns before the caller returns `[Client Tool Result ...]`:\n",
    );
    out.push_str(open_tag);
    out.push('\n');
    out.push_str("{\"name\": \"<exact_tool_name>\", \"arguments\": { ... }}\n");
    out.push_str(close_tag);
    if has_custom_tools {
        out.push_str(
            "\n  (For freeform custom tools like `apply_patch` or Code Mode `exec` — even when a tool's \
             description says \"Accepts raw JavaScript source text, not JSON\" — you MUST still wrap the \
             call in a JSON `\"input\"` string inside the XML block: \
             `{\"name\": \"exec\", \"input\": \"<JSON-escaped JS source>\"}` or \
             `{\"name\": \"apply_patch\", \"input\": \"*** Begin Patch\\n...\"}`.)\n",
        );
    } else {
        out.push_str(
            "\n  (For freeform string tools like `apply_patch`, pass a JSON string in `\"input\"`: \
             `{\"name\": \"apply_patch\", \"input\": \"...\"}`.)\n",
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
    let full = build_session_system_append(caller_system, client_tools, web_cfg, tool_nonce);
    if full.len() <= MAX_SYSTEM_APPEND_BYTES || client_tools.is_empty() {
        return (full, None);
    }

    let base_web_cfg = WebToolBridgeConfig {
        enable_native_web_fetch: false,
        force_native_web_fetch: false,
        ..web_cfg
    };
    let mut header = build_session_system_append(None, &[], base_web_cfg, tool_nonce);
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
        let mut tool_buf = String::new();
        for tool in client_tools {
            tool_buf.clear();
            render_compact_client_tool(tool, &mut tool_buf);
            let open_cost = if inline_count == 0 {
                "<client_tools>\n".len()
            } else {
                0
            };
            if bounded.len() + open_cost + tool_buf.len() + close_rules.len()
                <= MAX_SYSTEM_APPEND_BYTES
            {
                if inline_count == 0 {
                    bounded.push_str("<client_tools>\n");
                }
                bounded.push_str(&tool_buf);
                inline_count += 1;
            } else {
                break;
            }
        }
        if inline_count > 0 {
            bounded.push_str(close_rules);
        } else {
            bounded.push_str("</api_gateway_bridge_rules>\n");
        }
        header = bounded;

        if let Some(spilled) = client_tools.get(inline_count..)
            && !spilled.is_empty()
        {
            let spilled_refs: Vec<&CanonicalToolSpec> = spilled.iter().collect();
            let mut spill_xml = String::with_capacity(4096);
            spill_xml.push_str(
                "<client_tools_update>\nAdditional active `<client_tools>` for this session:\n",
            );
            render_bounded_tools_list(&spilled_refs, MAX_TOOL_DIRECTIVE_BYTES, &mut spill_xml);
            spill_xml.push_str("</client_tools_update>\n");
            return (header, Some(spill_xml));
        }
    }

    (header, None)
}

fn render_compact_client_tool(tool: &CanonicalToolSpec, out: &mut String) {
    out.push_str("- name: ");
    out.push_str(&tool.name);
    out.push('\n');
    if let Some(desc) = &tool.description {
        let compacted_desc = if is_code_mode_orchestrator_tool(tool) {
            compact_code_mode_tool_description(desc, MAX_CODE_MODE_TOOL_DESC_BYTES)
        } else {
            compact_tool_description(desc)
        };
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

fn render_slim_client_tool(tool: &CanonicalToolSpec, out: &mut String) {
    out.push_str("- name: ");
    out.push_str(&tool.name);
    out.push('\n');
    if let Some(desc) = &tool.description {
        let short_desc = if is_code_mode_orchestrator_tool(tool) {
            compact_code_mode_tool_description(desc, MAX_SLIM_CODE_MODE_TOOL_DESC_BYTES)
        } else {
            truncate_utf8_chars(&compact_tool_description(desc), 120)
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
                out.push_str(&truncate_utf8_chars(fmt.trim(), 160));
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
        return truncate_utf8_chars(raw_schema.trim(), 240);
    };
    strip_schema_metadata_value(&mut val);
    serde_json::to_string(&val).unwrap_or_else(|_| truncate_utf8_chars(raw_schema.trim(), 240))
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

fn render_bounded_tools_list(
    tools: &[&CanonicalToolSpec],
    max_body_bytes: usize,
    out: &mut String,
) {
    let mut standard = String::new();
    for tool in tools {
        render_compact_client_tool(tool, &mut standard);
    }
    if standard.len() <= max_body_bytes {
        out.push_str(&standard);
        return;
    }

    let mut slim = String::new();
    for tool in tools {
        render_slim_client_tool(tool, &mut slim);
    }
    if slim.len() <= max_body_bytes {
        out.push_str(&slim);
        return;
    }

    let mut minimal = String::new();
    let mut entry_buf = String::new();
    let mut omitted = 0usize;
    for tool in tools {
        entry_buf.clear();
        render_minimal_client_tool(tool, &mut entry_buf);
        if minimal.len() + entry_buf.len() + 96 <= max_body_bytes {
            minimal.push_str(&entry_buf);
        } else {
            omitted += 1;
        }
    }
    if omitted > 0 {
        minimal.push_str(&format!(
            "  ... [{omitted} additional client tools omitted to stay within single-turn directive budget]\n"
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
    if prev_tools == next_tools {
        return None;
    }
    let mut out = String::with_capacity(512);
    out.push_str("<client_tools_update>\n");
    if next_tools.is_empty() {
        out.push_str(&format!(
            "External `<client_tools>` are disabled for this turn; respond in plain text without emitting any `<agy_tool_call_{tool_nonce}>` XML blocks.\n"
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
        out.push_str(&truncate_utf8_chars(&joined, 4_000));
        out.push('\n');
    }
    if !added_or_changed.is_empty() {
        out.push_str("Added/updated client tools:\n");
        let remaining_budget = MAX_TOOL_DIRECTIVE_BYTES.saturating_sub(out.len());
        render_bounded_tools_list(&added_or_changed, remaining_budget.max(4_096), &mut out);
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
            truncate_head_tail_lines(&sys, 24_000, "compaction instructions")
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

/// Maximum total UTF-8 text bytes emitted in a single `session/prompt` request.
///
/// Upstream `agy acp` truncates single-turn text payloads at `191,578` bytes (`<truncated N bytes>`).
/// Keeping each ACP turn at or below `176,000` bytes provides >15.5 KB of safety headroom while
/// automatically preserving generous head and tail sections when large tool outputs or logs arrive.
pub const MAX_ACP_PROMPT_TEXT_BYTES: usize = 176_000;

/// Progressive schema-compaction ceiling for spilled or incremental `<client_tools_update>` blocks.
const MAX_TOOL_DIRECTIVE_BYTES: usize = 48_000;

/// Maximum number of fast Flash seed chunks before the final target-model turn (`5 * 156 KB + 176 KB ≈ 956 KB`).
pub const MAX_HYDRATION_SEED_CHUNKS: usize = 5;

/// Maximum text capacity before framing; fragmented blocks can reach the turn cap sooner.
pub const MAX_LOSSLESS_HYDRATION_BYTES: usize =
    (MAX_HYDRATION_SEED_CHUNKS + 1) * SEED_CHUNK_TARGET_TEXT_BYTES;

const SEED_CHUNK_TARGET_TEXT_BYTES: usize = 156_000;

const MAX_ACTIVE_WITH_HISTORY_TEXT_BYTES: usize = 76_000;
const MAX_HISTORY_RESERVE_BYTES: usize = 48_000;
const RECENT_TAIL_HISTORY_MESSAGES: usize = 14;

const _: () = {
    assert!(
        MAX_SYSTEM_APPEND_BYTES * 4 < MAX_ACP_PROMPT_TEXT_BYTES,
        "systemPrompt.append cap must stay well below a single ACP prompt turn budget"
    );
    assert!(
        MAX_TOOL_DIRECTIVE_BYTES * 2 < MAX_ACP_PROMPT_TEXT_BYTES,
        "client_tools_update budget must leave the majority of a turn for conversation and tool outputs"
    );
    assert!(
        MAX_ACP_PROMPT_TEXT_BYTES > SEED_CHUNK_TARGET_TEXT_BYTES + 1_024,
        "hydration seed chunks plus footer framing must stay below the single-turn ACP ceiling"
    );
    assert!(
        SEED_CHUNK_TARGET_TEXT_BYTES > MAX_ACTIVE_WITH_HISTORY_TEXT_BYTES,
        "seed chunk budget must exceed the active-slice share of a bounded cold replay"
    );
    assert!(
        MAX_ACP_PROMPT_TEXT_BYTES > MAX_HISTORY_RESERVE_BYTES * 3,
        "history reserve on cold replays must never starve the active turn below two-thirds of the turn budget"
    );
};

const ARCHIVE_OPEN_HEADER: &str =
    "--- Prior Conversation Context (Read-Only Archive; Do NOT echo these headers) ---\n";
const ARCHIVE_CLOSE_HEADER: &str = "--- End Prior Conversation Context ---\n\n### Current Caller Input (Respond ONLY for the current assistant step)\n";

/// Multi-step ACP prompt execution plan.
///
/// When `seed_chunks` is empty, `final_blocks` fits in a single `session/prompt` turn.
/// When `seed_chunks` is non-empty, `turn_runner` seeds each chunk (`<= 148 KB`) on the fastest
/// Flash model in the same ACP session before switching to the requested model for `final_blocks`,
/// avoiding single-turn truncation. This cannot disable upstream post-turn compaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpPromptPlan {
    pub seed_chunks: Vec<Vec<AcpPromptBlock>>,
    pub final_blocks: Vec<AcpPromptBlock>,
}

#[derive(Clone, Copy)]
struct PartRenderLimits {
    max_text_bytes: usize,
    max_tool_call_bytes: usize,
    max_tool_result_bytes: usize,
    emit_images: bool,
}

impl PartRenderLimits {
    const UNBOUNDED: Self = Self {
        max_text_bytes: usize::MAX,
        max_tool_call_bytes: usize::MAX,
        max_tool_result_bytes: usize::MAX,
        emit_images: true,
    };
}

/// Converts a slice of `CanonicalMessage` turns into ACP `AcpPromptBlock`s.
///
/// When `is_full_history_replay` is true and there are prior turns before the latest turn,
/// earlier turns are formatted inside a bounded read-only archive that preserves user instructions
/// and recent context without exceeding upstream `agy acp`'s per-prompt payload ceiling.
#[must_use]
pub fn build_acp_prompt_blocks(
    messages: &[CanonicalMessage],
    tool_nonce: &str,
    is_full_history_replay: bool,
) -> Vec<AcpPromptBlock> {
    if messages.is_empty() {
        return Vec::new();
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

    if estimate_raw_prompt_bytes(history_slice, active_slice) <= MAX_ACP_PROMPT_TEXT_BYTES {
        let uncompacted = render_uncompacted_prompt_blocks(history_slice, active_slice, tool_nonce);
        if total_text_block_bytes(&uncompacted) <= MAX_ACP_PROMPT_TEXT_BYTES {
            return sanitize_slash_command_blocks(uncompacted);
        }
    }

    sanitize_slash_command_blocks(render_bounded_prompt_blocks(
        history_slice,
        active_slice,
        tool_nonce,
        MAX_ACP_PROMPT_TEXT_BYTES,
    ))
}

/// Builds a single-turn bounded prompt by default, automatically middle-truncating oversized
/// tool results or pasted logs (preserving clean line-aligned head and tail sections) so agentic
/// loops never stall on large command/file outputs. Experimental long-context mode may seed
/// larger transcripts across multiple Flash turns; upstream compaction still applies.
pub fn build_acp_prompt_plan(
    messages: &[CanonicalMessage],
    tool_nonce: &str,
    is_full_history_replay: bool,
    leading_directive_xml: Option<&str>,
    long_context: bool,
) -> Result<AcpPromptPlan, ProtocolError> {
    // Reserve framing and slash-escaping bytes as well as the complete tool/directive update.
    let directive_bytes = leading_directive_xml
        .map_or(0, str::len)
        .saturating_add(128);
    if directive_bytes.saturating_add(2_048) > MAX_ACP_PROMPT_TEXT_BYTES {
        return Err(ProtocolError::PromptDirectivesTooLarge {
            bytes: directive_bytes,
        });
    }
    if messages.is_empty() {
        let text = leading_directive_xml
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(" ")
            .to_owned();
        return Ok(AcpPromptPlan {
            seed_chunks: Vec::new(),
            final_blocks: sanitize_slash_command_blocks(vec![AcpPromptBlock::Text { text }]),
        });
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

    if !long_context {
        let budget = MAX_ACP_PROMPT_TEXT_BYTES.saturating_sub(directive_bytes);
        let mut blocks = if history_slice.is_empty() {
            render_smart_bounded_active_slice(active_slice, tool_nonce, budget)
        } else {
            let uncompacted =
                render_uncompacted_prompt_blocks(history_slice, active_slice, tool_nonce);
            if total_text_block_bytes(&uncompacted) <= budget {
                uncompacted
            } else {
                let hist_reserve = estimate_raw_prompt_bytes(history_slice, &[])
                    .min(budget / 3)
                    .min(MAX_HISTORY_RESERVE_BYTES);
                let active_budget = budget.saturating_sub(hist_reserve);
                let active =
                    render_smart_bounded_active_slice(active_slice, tool_nonce, active_budget);
                render_history_around_active(history_slice, active, tool_nonce, budget)
            }
        };
        prepend_directive_to_blocks(&mut blocks, leading_directive_xml);
        let mut blocks = sanitize_slash_command_blocks(blocks);
        clamp_blocks_to_budget(&mut blocks, MAX_ACP_PROMPT_TEXT_BYTES);
        let bytes = total_text_block_bytes(&blocks);
        if bytes > MAX_ACP_PROMPT_TEXT_BYTES {
            return Err(ProtocolError::PromptTooLarge { bytes });
        }
        return Ok(AcpPromptPlan {
            seed_chunks: Vec::new(),
            final_blocks: blocks,
        });
    }

    let raw_est =
        estimate_raw_prompt_bytes(history_slice, active_slice).saturating_add(directive_bytes);

    if raw_est <= MAX_LOSSLESS_HYDRATION_BYTES {
        let mut uncompacted =
            render_uncompacted_prompt_blocks(history_slice, active_slice, tool_nonce);
        prepend_directive_to_blocks(&mut uncompacted, leading_directive_xml);
        let total_bytes = total_text_block_bytes(&uncompacted);
        if total_bytes <= MAX_ACP_PROMPT_TEXT_BYTES {
            return Ok(AcpPromptPlan {
                seed_chunks: Vec::new(),
                final_blocks: sanitize_slash_command_blocks(uncompacted),
            });
        }
        if total_bytes <= MAX_LOSSLESS_HYDRATION_BYTES {
            return split_blocks_into_hydration_plan(uncompacted);
        }
    }

    // Ultra-large payload exceeding lossless hydration capacity: smartly middle-truncate the
    // active turn to one seed-chunk budget and compact historical turns into the seed budget.
    let active_hydration_budget = SEED_CHUNK_TARGET_TEXT_BYTES.saturating_sub(directive_bytes);
    let mut active_blocks = render_smart_bounded_active_slice(
        active_slice,
        tool_nonce,
        active_hydration_budget.max(8_192),
    );
    if history_slice.is_empty() {
        prepend_directive_to_blocks(&mut active_blocks, leading_directive_xml);
        return split_blocks_into_hydration_plan(active_blocks);
    }

    let history_budget = MAX_HYDRATION_SEED_CHUNKS * (SEED_CHUNK_TARGET_TEXT_BYTES - 8_000);
    let history_body = build_compacted_history_body(history_slice, tool_nonce, history_budget);
    let mut archive_text = String::with_capacity(
        directive_bytes
            + ARCHIVE_OPEN_HEADER.len()
            + history_body.len()
            + ARCHIVE_CLOSE_HEADER.len(),
    );
    if let Some(dir) = leading_directive_xml.filter(|s| !s.trim().is_empty()) {
        archive_text.push_str(dir);
        if !archive_text.ends_with('\n') {
            archive_text.push('\n');
        }
    }
    archive_text.push_str(ARCHIVE_OPEN_HEADER);
    archive_text.push_str(&history_body);
    if !archive_text.ends_with('\n') {
        archive_text.push('\n');
    }
    archive_text.push_str(ARCHIVE_CLOSE_HEADER);

    let mut combined = Vec::with_capacity(active_blocks.len() + 1);
    let mut active_iter = active_blocks.into_iter();
    match active_iter.next() {
        Some(AcpPromptBlock::Text { text }) => {
            archive_text.push_str(&text);
            combined.push(AcpPromptBlock::Text { text: archive_text });
        }
        Some(other) => {
            combined.push(AcpPromptBlock::Text { text: archive_text });
            combined.push(other);
        }
        None => {
            combined.push(AcpPromptBlock::Text { text: archive_text });
        }
    }
    combined.extend(active_iter);
    split_blocks_into_hydration_plan(combined)
}

fn prepend_directive_to_blocks(blocks: &mut Vec<AcpPromptBlock>, directive: Option<&str>) {
    let Some(dir) = directive.filter(|s| !s.trim().is_empty()) else {
        return;
    };
    match blocks.first_mut() {
        Some(AcpPromptBlock::Text { text }) => {
            let mut merged = String::with_capacity(dir.len() + 1 + text.len());
            merged.push_str(dir);
            if !merged.ends_with('\n') {
                merged.push('\n');
            }
            merged.push_str(text);
            *text = merged;
        }
        _ => {
            blocks.insert(
                0,
                AcpPromptBlock::Text {
                    text: dir.to_owned(),
                },
            );
        }
    }
}

/// Splits a sequence of `AcpPromptBlock`s into `<= MAX_HYDRATION_SEED_CHUNKS` seed turns
/// (`<= SEED_CHUNK_TARGET_TEXT_BYTES` each) plus a `final_blocks` turn (`<= MAX_ACP_PROMPT_TEXT_BYTES`).
fn split_blocks_into_hydration_plan(
    blocks: Vec<AcpPromptBlock>,
) -> Result<AcpPromptPlan, ProtocolError> {
    let original_bytes = total_text_block_bytes(&blocks);
    let mut flat_segments: Vec<AcpPromptBlock> = Vec::new();
    for block in blocks {
        match block {
            AcpPromptBlock::Image { .. } => flat_segments.push(block),
            AcpPromptBlock::Text { text } => {
                if text.len() <= SEED_CHUNK_TARGET_TEXT_BYTES {
                    flat_segments.push(AcpPromptBlock::Text { text });
                } else {
                    let mut rest = text.as_str();
                    while !rest.is_empty() {
                        if rest.len() <= SEED_CHUNK_TARGET_TEXT_BYTES {
                            flat_segments.push(AcpPromptBlock::Text {
                                text: rest.to_owned(),
                            });
                            break;
                        }
                        let cut = find_clean_line_split(rest, SEED_CHUNK_TARGET_TEXT_BYTES);
                        let (chunk_str, after) = rest.split_at_checked(cut).unwrap_or((rest, ""));
                        flat_segments.push(AcpPromptBlock::Text {
                            text: chunk_str.to_owned(),
                        });
                        rest = after;
                    }
                }
            }
        }
    }

    let mut raw_turns: Vec<Vec<AcpPromptBlock>> = Vec::new();
    let mut cur_turn: Vec<AcpPromptBlock> = Vec::new();
    let mut cur_bytes = 0usize;

    for seg in flat_segments {
        let seg_bytes = match &seg {
            AcpPromptBlock::Text { text } => text.len(),
            AcpPromptBlock::Image { .. } => 0,
        };
        if !cur_turn.is_empty()
            && cur_bytes.saturating_add(seg_bytes) > SEED_CHUNK_TARGET_TEXT_BYTES
        {
            raw_turns.push(std::mem::take(&mut cur_turn));
            cur_bytes = 0;
        }
        cur_bytes = cur_bytes.saturating_add(seg_bytes);
        cur_turn.push(seg);
    }
    if !cur_turn.is_empty() {
        raw_turns.push(cur_turn);
    }

    if raw_turns.is_empty() {
        return Ok(AcpPromptPlan {
            seed_chunks: Vec::new(),
            final_blocks: vec![AcpPromptBlock::Text {
                text: " ".to_owned(),
            }],
        });
    }

    let mut final_blocks = raw_turns.pop().unwrap_or_default();
    // Ensure `final_blocks` is non-empty and ends with text context if it only held an image.
    if final_blocks.is_empty() {
        final_blocks.push(AcpPromptBlock::Text {
            text: "Continue with the current caller request above.".to_owned(),
        });
    } else if !final_blocks
        .iter()
        .any(|b| matches!(b, AcpPromptBlock::Text { text } if !text.trim().is_empty()))
    {
        final_blocks.push(AcpPromptBlock::Text {
            text: "Inspect the attached image(s) and fulfill the current caller request above."
                .to_owned(),
        });
    }

    if raw_turns.len() > MAX_HYDRATION_SEED_CHUNKS {
        return Err(ProtocolError::PromptTooLarge {
            bytes: original_bytes,
        });
    }

    let total_seeds = raw_turns.len();
    let seed_chunks = raw_turns
        .into_iter()
        .enumerate()
        .map(|(idx, mut turn_blocks)| {
            let footer = format!(
                "\n\n[Context Hydration Segment {}/{total_seeds} — Store the exact context above in session memory. Do NOT invoke any tools yet; reply with ONLY: OK]",
                idx + 1
            );
            match turn_blocks.last_mut() {
                Some(AcpPromptBlock::Text { text }) => text.push_str(&footer),
                _ => turn_blocks.push(AcpPromptBlock::Text { text: footer }),
            }
            sanitize_slash_command_blocks(turn_blocks)
        })
        .collect();

    Ok(AcpPromptPlan {
        seed_chunks,
        final_blocks: sanitize_slash_command_blocks(final_blocks),
    })
}

fn find_clean_line_split(text: &str, max_bytes: usize) -> usize {
    let safe_end = floor_utf8_boundary(text, max_bytes);
    let candidate = text.get(..safe_end).unwrap_or("");
    // Prefer splitting at a newline within the last 16 KB so lines/tags stay intact.
    let min_newline_pos = safe_end.saturating_sub(16_384);
    if let Some(rel_nl) = candidate
        .get(min_newline_pos..)
        .and_then(|tail| tail.rfind('\n'))
    {
        let pos = min_newline_pos + rel_nl + 1;
        if pos > 0 {
            return pos;
        }
    }
    safe_end.max(1)
}

fn sanitize_slash_command_blocks(mut blocks: Vec<AcpPromptBlock>) -> Vec<AcpPromptBlock> {
    for block in &mut blocks {
        if let AcpPromptBlock::Text { text } = block
            && text.trim_start().starts_with('/')
        {
            text.insert(0, '\u{200b}');
        }
    }
    blocks
}

fn render_uncompacted_prompt_blocks(
    history_slice: &[CanonicalMessage],
    active_slice: &[CanonicalMessage],
    tool_nonce: &str,
) -> Vec<AcpPromptBlock> {
    let mut blocks = Vec::new();
    let mut text_acc = String::new();
    if !history_slice.is_empty() {
        text_acc.push_str(ARCHIVE_OPEN_HEADER);
        for msg in history_slice {
            let role_header = match msg.role {
                CanonicalRole::User => "### Prior User Turn\n",
                CanonicalRole::Assistant => "### Prior Assistant Turn\n",
            };
            if !text_acc.ends_with('\n') {
                text_acc.push('\n');
            }
            text_acc.push_str(role_header);
            render_parts_into(
                msg,
                tool_nonce,
                true,
                PartRenderLimits::UNBOUNDED,
                &mut text_acc,
                &mut blocks,
            );
            if !text_acc.ends_with('\n') {
                text_acc.push('\n');
            }
        }
        text_acc.push_str(ARCHIVE_CLOSE_HEADER);
    }

    for (idx, msg) in active_slice.iter().enumerate() {
        if idx > 0 && !text_acc.is_empty() && !text_acc.ends_with('\n') {
            text_acc.push('\n');
        }
        render_parts_into(
            msg,
            tool_nonce,
            false,
            PartRenderLimits::UNBOUNDED,
            &mut text_acc,
            &mut blocks,
        );
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

fn render_bounded_prompt_blocks(
    history_slice: &[CanonicalMessage],
    active_slice: &[CanonicalMessage],
    tool_nonce: &str,
    max_bytes: usize,
) -> Vec<AcpPromptBlock> {
    let active_budget = if history_slice.is_empty() {
        max_bytes
    } else {
        let hist_reserve = estimate_raw_prompt_bytes(history_slice, &[])
            .min(max_bytes / 3)
            .min(MAX_HISTORY_RESERVE_BYTES);
        max_bytes
            .saturating_sub(hist_reserve)
            .max(MAX_ACTIVE_WITH_HISTORY_TEXT_BYTES.min(max_bytes / 2))
    };
    let active_blocks = render_smart_bounded_active_slice(active_slice, tool_nonce, active_budget);
    render_history_around_active(history_slice, active_blocks, tool_nonce, max_bytes)
}

fn render_history_around_active(
    history_slice: &[CanonicalMessage],
    active_blocks: Vec<AcpPromptBlock>,
    tool_nonce: &str,
    max_bytes: usize,
) -> Vec<AcpPromptBlock> {
    let active_bytes = total_text_block_bytes(&active_blocks);

    if history_slice.is_empty() {
        if active_blocks.is_empty() {
            return vec![AcpPromptBlock::Text {
                text: " ".to_owned(),
            }];
        }
        return active_blocks;
    }

    let framing_bytes = ARCHIVE_OPEN_HEADER.len() + ARCHIVE_CLOSE_HEADER.len() + 64;
    let history_budget = max_bytes
        .saturating_sub(active_bytes)
        .saturating_sub(framing_bytes);

    let history_body = build_compacted_history_body(history_slice, tool_nonce, history_budget);

    let mut archive_prefix = String::with_capacity(
        ARCHIVE_OPEN_HEADER.len() + history_body.len() + ARCHIVE_CLOSE_HEADER.len(),
    );
    archive_prefix.push_str(ARCHIVE_OPEN_HEADER);
    archive_prefix.push_str(&history_body);
    if !archive_prefix.ends_with('\n') {
        archive_prefix.push('\n');
    }
    archive_prefix.push_str(ARCHIVE_CLOSE_HEADER);

    let mut combined = Vec::with_capacity(active_blocks.len() + 1);
    let mut active_iter = active_blocks.into_iter();
    match active_iter.next() {
        Some(AcpPromptBlock::Text { text }) => {
            archive_prefix.push_str(&text);
            combined.push(AcpPromptBlock::Text {
                text: archive_prefix,
            });
        }
        Some(other) => {
            combined.push(AcpPromptBlock::Text {
                text: archive_prefix,
            });
            combined.push(other);
        }
        None => {
            combined.push(AcpPromptBlock::Text {
                text: archive_prefix,
            });
        }
    }
    combined.extend(active_iter);
    combined
}

/// Computes an exact `O(N log N)` water-filling per-item byte cap: items whose length already
/// fits below the fair share keep 100% of their content untouched, redistributing all unused
/// budget slack exclusively to the oversized items that caused the overflow.
fn compute_waterfill_cap(lengths: &[usize], total_budget: usize) -> usize {
    if lengths.is_empty() {
        return total_budget;
    }
    let mut sorted = lengths.to_vec();
    sorted.sort_unstable();
    let mut rem_budget = total_budget;
    let mut rem_count = sorted.len();
    for len in sorted {
        let share = rem_budget / rem_count.max(1);
        if len <= share {
            rem_budget = rem_budget.saturating_sub(len);
            rem_count = rem_count.saturating_sub(1);
        } else {
            return share;
        }
    }
    usize::MAX
}

fn clamp_blocks_to_budget(blocks: &mut [AcpPromptBlock], max_bytes: usize) {
    let total = total_text_block_bytes(blocks);
    if total <= max_bytes {
        return;
    }
    let excess = total.saturating_sub(max_bytes);
    let mut largest_idx: Option<(usize, usize)> = None;
    for (idx, block) in blocks.iter().enumerate() {
        if let AcpPromptBlock::Text { text } = block
            && largest_idx.is_none_or(|(_, best_len)| text.len() > best_len)
        {
            largest_idx = Some((idx, text.len()));
        }
    }
    if let Some((idx, cur_len)) = largest_idx
        && let Some(AcpPromptBlock::Text { text }) = blocks.get_mut(idx)
    {
        let target = cur_len.saturating_sub(excess.saturating_add(32));
        *text = truncate_head_tail_lines(text, target, "prompt content");
    }
}

/// Renders `active_slice` losslessly whenever it fits within `budget`, and otherwise applies a
/// prioritized, water-filling head+tail compaction pipeline:
/// 1. Strip `<examples>` / `<commentary>` boilerplate inside `<developer_instructions>`.
/// 2. Water-fill middle-truncate oversized `ToolResult` outputs while keeping small tool outputs,
///    caller prompts, and `<developer_instructions>` 100% lossless.
/// 3. Slim oversized `<developer_instructions>` blocks while preserving caller prompts intact.
/// 4. Universal water-fill middle-truncation across all oversized variable parts (pasted logs,
///    attachments, or giant tool payloads) so single-turn overflow never bricks the agent loop.
fn render_smart_bounded_active_slice(
    active_slice: &[CanonicalMessage],
    tool_nonce: &str,
    budget: usize,
) -> Vec<AcpPromptBlock> {
    let uncompacted = render_uncompacted_prompt_blocks(&[], active_slice, tool_nonce);
    if total_text_block_bytes(&uncompacted) <= budget {
        return uncompacted;
    }

    // Stage 1: Strip `<examples>`, `<example>`, and `<commentary>` inside `<developer_instructions>`.
    let mut work: Vec<CanonicalMessage> = active_slice.to_vec();
    let mut dev_part_count = 0usize;
    for msg in &mut work {
        for part in &mut msg.parts {
            if let CanonicalPart::Text(text) = part
                && is_developer_instructions_wrapper(text)
            {
                dev_part_count += 1;
                let s1 = strip_tag_blocks(text, "<examples>", "</examples>");
                let s2 = strip_tag_blocks(&s1, "<example>", "</example>");
                *text = strip_tag_blocks(&s2, "<commentary>", "</commentary>");
            }
        }
    }
    let stage1_blocks = render_uncompacted_prompt_blocks(&[], &work, tool_nonce);
    if total_text_block_bytes(&stage1_blocks) <= budget {
        return stage1_blocks;
    }

    // Stage 2: Water-fill middle-truncate ONLY oversized `ToolResult` outputs if non-result parts fit.
    let mut result_lens = Vec::new();
    let mut empty_results_probe = work.clone();
    for msg in &mut empty_results_probe {
        for part in &mut msg.parts {
            if let CanonicalPart::ToolResult { output, .. } = part {
                let cleaned = strip_billing_noise(output);
                result_lens.push(cleaned.trim().len());
                output.clear();
            }
        }
    }
    if !result_lens.is_empty() {
        let non_result_bytes = total_text_block_bytes(&render_uncompacted_prompt_blocks(
            &[],
            &empty_results_probe,
            tool_nonce,
        ));
        let safety_pad = result_lens.len().saturating_mul(16).saturating_add(128);
        let available_for_results =
            budget.saturating_sub(non_result_bytes.saturating_add(safety_pad));
        let min_viable_results = result_lens.len().saturating_mul(1_024).min(16_384);
        if available_for_results >= min_viable_results {
            let cap = compute_waterfill_cap(&result_lens, available_for_results);
            let mut stage2_msgs = work.clone();
            for msg in &mut stage2_msgs {
                for part in &mut msg.parts {
                    if let CanonicalPart::ToolResult { output, .. } = part {
                        let cleaned = strip_billing_noise(output);
                        let trimmed = cleaned.trim();
                        if trimmed.len() > cap {
                            *output = truncate_head_tail_lines(trimmed, cap, "tool output");
                        }
                    }
                }
            }
            let stage2_blocks = render_uncompacted_prompt_blocks(&[], &stage2_msgs, tool_nonce);
            if total_text_block_bytes(&stage2_blocks) <= budget {
                return stage2_blocks;
            }
            work = stage2_msgs;
        }
    }

    // Stage 3: Slim `<developer_instructions>` while preserving caller prompts and remaining parts intact.
    if let Some(dev_count) = std::num::NonZeroUsize::new(dev_part_count) {
        let mut non_dev_probe = work.clone();
        for msg in &mut non_dev_probe {
            msg.parts.retain(
                |p| !matches!(p, CanonicalPart::Text(t) if is_developer_instructions_wrapper(t)),
            );
        }
        let non_dev_bytes = total_text_block_bytes(&render_uncompacted_prompt_blocks(
            &[],
            &non_dev_probe,
            tool_nonce,
        ));
        if non_dev_bytes.saturating_add(512) < budget {
            let available_dev = budget.saturating_sub(non_dev_bytes.saturating_add(256));
            let per_dev = available_dev / dev_count;
            if per_dev >= 512 {
                let mut stage3_msgs = work.clone();
                for msg in &mut stage3_msgs {
                    for part in &mut msg.parts {
                        if let CanonicalPart::Text(text) = part
                            && is_developer_instructions_wrapper(text)
                        {
                            let inner = text
                                .trim()
                                .strip_prefix("<developer_instructions>")
                                .and_then(|s| s.strip_suffix("</developer_instructions>"))
                                .unwrap_or(text.as_str())
                                .trim();
                            let body_budget = per_dev.saturating_sub(64);
                            let bounded = if inner.len() > body_budget {
                                truncate_head_tail_lines(
                                    inner,
                                    body_budget,
                                    "developer instructions",
                                )
                            } else {
                                inner.to_owned()
                            };
                            *text = format!(
                                "<developer_instructions>\n{bounded}\n</developer_instructions>"
                            );
                        }
                    }
                }
                let stage3_blocks = render_uncompacted_prompt_blocks(&[], &stage3_msgs, tool_nonce);
                if total_text_block_bytes(&stage3_blocks) <= budget {
                    return stage3_blocks;
                }
                work = stage3_msgs;
            }
        } else {
            // Non-dev parts ALSO overflow `budget`: cap `<developer_instructions>` to at most 20%
            // of `budget` first so Stage 4 preserves up to 80% for caller prompt / tool payloads.
            let dev_share = (budget / 5).max(2_048) / dev_count;
            for msg in &mut work {
                for part in &mut msg.parts {
                    if let CanonicalPart::Text(text) = part
                        && is_developer_instructions_wrapper(text)
                        && text.len() > dev_share
                    {
                        let inner = text
                            .trim()
                            .strip_prefix("<developer_instructions>")
                            .and_then(|s| s.strip_suffix("</developer_instructions>"))
                            .unwrap_or(text.as_str())
                            .trim();
                        let bounded = truncate_head_tail_lines(
                            inner,
                            dev_share.saturating_sub(64),
                            "developer instructions",
                        );
                        *text = format!(
                            "<developer_instructions>\n{bounded}\n</developer_instructions>"
                        );
                    }
                }
            }
        }
    }

    // Stage 4: Universal water-filling across all variable text/tool-result/tool-call payloads.
    let mut variable_lens = Vec::new();
    let mut fixed_overhead = 128usize;
    for msg in &work {
        fixed_overhead = fixed_overhead.saturating_add(16);
        for part in &msg.parts {
            match part {
                CanonicalPart::Text(t) => {
                    let cleaned = strip_billing_noise(t);
                    let trimmed = cleaned.trim();
                    if !trimmed.is_empty() {
                        variable_lens.push(trimmed.len());
                        fixed_overhead = fixed_overhead.saturating_add(16);
                    }
                }
                CanonicalPart::ToolResult {
                    call_id, output, ..
                } => {
                    let cleaned = strip_billing_noise(output);
                    variable_lens.push(cleaned.trim().len());
                    fixed_overhead = fixed_overhead.saturating_add(call_id.len() + 80);
                }
                CanonicalPart::ToolCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } => {
                    variable_lens.push(arguments.len());
                    fixed_overhead = fixed_overhead
                        .saturating_add(call_id.len() + name.len() + tool_nonce.len() * 2 + 96);
                }
                CanonicalPart::ImageBase64 { .. } | CanonicalPart::Reasoning(_) => {}
            }
        }
    }

    let available_var = budget.saturating_sub(fixed_overhead);
    let universal_cap = compute_waterfill_cap(&variable_lens, available_var).max(512);
    for msg in &mut work {
        for part in &mut msg.parts {
            match part {
                CanonicalPart::Text(text) => {
                    let cleaned = strip_billing_noise(text);
                    let trimmed = cleaned.trim();
                    if trimmed.len() > universal_cap {
                        if is_developer_instructions_wrapper(trimmed) {
                            let inner = trimmed
                                .strip_prefix("<developer_instructions>")
                                .and_then(|s| s.strip_suffix("</developer_instructions>"))
                                .unwrap_or(trimmed)
                                .trim();
                            let bounded = truncate_head_tail_lines(
                                inner,
                                universal_cap.saturating_sub(64),
                                "developer instructions",
                            );
                            *text = format!(
                                "<developer_instructions>\n{bounded}\n</developer_instructions>"
                            );
                        } else {
                            *text = truncate_head_tail_lines(trimmed, universal_cap, "input text");
                        }
                    }
                }
                CanonicalPart::ToolResult { output, .. } => {
                    let cleaned = strip_billing_noise(output);
                    let trimmed = cleaned.trim();
                    if trimmed.len() > universal_cap {
                        *output = truncate_head_tail_lines(trimmed, universal_cap, "tool output");
                    }
                }
                CanonicalPart::ToolCall {
                    arguments,
                    is_custom,
                    ..
                } => {
                    if arguments.len() > universal_cap {
                        let preview = truncate_head_tail_lines(
                            arguments,
                            universal_cap.saturating_sub(96),
                            "tool call arguments",
                        );
                        if *is_custom {
                            *arguments = preview;
                        } else {
                            *arguments = serde_json::json!({
                                "_truncated_arguments_preview": preview
                            })
                            .to_string();
                        }
                    }
                }
                CanonicalPart::ImageBase64 { .. } | CanonicalPart::Reasoning(_) => {}
            }
        }
    }

    let mut blocks = render_uncompacted_prompt_blocks(&[], &work, tool_nonce);
    clamp_blocks_to_budget(&mut blocks, budget);
    blocks
}

struct CompactedHistoryEntry {
    priority: u8,
    keep: bool,
    text: String,
}

fn build_compacted_history_body(
    history_slice: &[CanonicalMessage],
    tool_nonce: &str,
    budget: usize,
) -> String {
    if history_slice.is_empty() || budget == 0 {
        return String::new();
    }

    let conversational_user_flags: Vec<bool> = history_slice
        .iter()
        .map(is_conversational_user_message)
        .collect();
    let last_user_idx = conversational_user_flags
        .iter()
        .rposition(|&flag| flag)
        .unwrap_or(0);
    let recent_tail_start = history_slice
        .len()
        .saturating_sub(RECENT_TAIL_HISTORY_MESSAGES);

    let mut entries = Vec::with_capacity(history_slice.len());
    for (idx, msg) in history_slice.iter().enumerate() {
        let is_conv_user = conversational_user_flags.get(idx).copied().unwrap_or(false);
        let next_is_conv_user = conversational_user_flags
            .get(idx + 1)
            .copied()
            .unwrap_or(false);
        let is_assistant_summary = msg.role == CanonicalRole::Assistant
            && msg
                .parts
                .iter()
                .any(|p| matches!(p, CanonicalPart::Text(t) if !t.trim().is_empty()))
            && (!msg
                .parts
                .iter()
                .any(|p| matches!(p, CanonicalPart::ToolCall { .. }))
                || next_is_conv_user);

        let (priority, limits) = if idx == 0 {
            (
                3,
                PartRenderLimits {
                    max_text_bytes: 26_000,
                    max_tool_call_bytes: 600,
                    max_tool_result_bytes: 1_200,
                    emit_images: false,
                },
            )
        } else if is_conv_user {
            let max_user_txt = if idx == last_user_idx { 24_000 } else { 14_000 };
            (
                3,
                PartRenderLimits {
                    max_text_bytes: max_user_txt,
                    max_tool_call_bytes: 600,
                    max_tool_result_bytes: 1_200,
                    emit_images: false,
                },
            )
        } else if idx >= recent_tail_start {
            (
                2,
                PartRenderLimits {
                    max_text_bytes: 4_000,
                    max_tool_call_bytes: 2_000,
                    max_tool_result_bytes: 3_200,
                    emit_images: false,
                },
            )
        } else if is_assistant_summary {
            (
                2,
                PartRenderLimits {
                    max_text_bytes: 3_200,
                    max_tool_call_bytes: 400,
                    max_tool_result_bytes: 400,
                    emit_images: false,
                },
            )
        } else if idx > last_user_idx {
            (
                1,
                PartRenderLimits {
                    max_text_bytes: 900,
                    max_tool_call_bytes: 320,
                    max_tool_result_bytes: 400,
                    emit_images: false,
                },
            )
        } else {
            (
                0,
                PartRenderLimits {
                    max_text_bytes: 600,
                    max_tool_call_bytes: 220,
                    max_tool_result_bytes: 280,
                    emit_images: false,
                },
            )
        };

        let mut entry_text = String::new();
        let role_header = match msg.role {
            CanonicalRole::User => "### Prior User Turn\n",
            CanonicalRole::Assistant => "### Prior Assistant Turn\n",
        };
        entry_text.push_str(role_header);
        let mut ignored_blocks = Vec::new();
        render_parts_into(
            msg,
            tool_nonce,
            true,
            limits,
            &mut entry_text,
            &mut ignored_blocks,
        );
        if !entry_text.ends_with('\n') {
            entry_text.push('\n');
        }

        entries.push(CompactedHistoryEntry {
            priority,
            keep: true,
            text: entry_text,
        });
    }

    let mut current_bytes: usize = entries.iter().map(|e| e.text.len()).sum();
    let omission_marker_reserve = 256usize;
    let target_bytes = budget.saturating_sub(omission_marker_reserve);

    // Drop lowest-priority older entries first (`0` -> `1` -> `2` outside immediate tail -> middle `3`)
    for drop_prio in 0..=2u8 {
        if current_bytes <= target_bytes {
            break;
        }
        let protect_tail_from = history_slice.len().saturating_sub(6);
        for (idx, entry) in entries.iter_mut().enumerate() {
            if current_bytes <= target_bytes {
                break;
            }
            if !entry.keep || entry.priority != drop_prio {
                continue;
            }
            if drop_prio == 2 && idx >= protect_tail_from {
                continue;
            }
            entry.keep = false;
            current_bytes = current_bytes.saturating_sub(entry.text.len());
        }
    }

    // Extreme fallback: if dozens of large user pastes still exceed `target_bytes`, shrink older
    // `priority == 3` entries between `0` and `last_user_idx` before ever dropping any.
    if current_bytes > target_bytes {
        for (idx, entry) in entries.iter_mut().enumerate() {
            if current_bytes <= target_bytes {
                break;
            }
            if !entry.keep || idx == 0 || idx == last_user_idx {
                continue;
            }
            if entry.text.len() > 2_400 {
                let old_len = entry.text.len();
                entry.text = truncate_middle_utf8_bytes(&entry.text, 2_400, 13, 20);
                if !entry.text.ends_with('\n') {
                    entry.text.push('\n');
                }
                current_bytes = current_bytes
                    .saturating_sub(old_len)
                    .saturating_add(entry.text.len());
            }
        }
    }

    let mut assembled = String::with_capacity(current_bytes.min(budget) + 128);
    let mut omitted_run = 0usize;
    for entry in &entries {
        if !entry.keep {
            omitted_run += 1;
            continue;
        }
        if omitted_run > 0 {
            if !assembled.is_empty() && !assembled.ends_with('\n') {
                assembled.push('\n');
            }
            assembled.push_str(&format!(
                "[... omitted {omitted_run} earlier archival turns to fit context window ...]\n"
            ));
            omitted_run = 0;
        }
        assembled.push_str(&entry.text);
    }
    if omitted_run > 0 {
        assembled.push_str(&format!(
            "[... omitted {omitted_run} earlier archival turns to fit context window ...]\n"
        ));
    }

    if assembled.len() > budget {
        // Favor the tail (70%) so `last_user_idx` and the newest turns stay completely intact.
        truncate_middle_utf8_bytes(&assembled, budget, 3, 10)
    } else {
        assembled
    }
}

fn is_conversational_user_message(msg: &CanonicalMessage) -> bool {
    if msg.role != CanonicalRole::User {
        return false;
    }
    for part in &msg.parts {
        match part {
            CanonicalPart::ImageBase64 { .. } => return true,
            CanonicalPart::Text(txt) => {
                let cleaned = strip_unstable_harness_noise(txt);
                let trimmed = cleaned.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if (trimmed.starts_with("<developer_instructions>")
                    && trimmed.ends_with("</developer_instructions>"))
                    || (trimmed.starts_with("<environment_context>")
                        && trimmed.ends_with("</environment_context>"))
                    || (trimmed.starts_with("<system-reminder>")
                        && trimmed.ends_with("</system-reminder>"))
                {
                    continue;
                }
                return true;
            }
            CanonicalPart::Reasoning(_)
            | CanonicalPart::ToolCall { .. }
            | CanonicalPart::ToolResult { .. } => {}
        }
    }
    false
}

fn estimate_raw_prompt_bytes(
    history_slice: &[CanonicalMessage],
    active_slice: &[CanonicalMessage],
) -> usize {
    let mut total: usize = if history_slice.is_empty() { 0 } else { 256 };
    for msg in history_slice.iter().chain(active_slice.iter()) {
        total = total.saturating_add(32);
        for part in &msg.parts {
            let part_len = match part {
                CanonicalPart::Text(t) => t.len(),
                CanonicalPart::ToolCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } => call_id.len() + name.len() + arguments.len() + 96,
                CanonicalPart::ToolResult {
                    call_id, output, ..
                } => call_id.len() + output.len() + 80,
                CanonicalPart::ImageBase64 { media_type, .. } => media_type.len() + 48,
                CanonicalPart::Reasoning(_) => 0,
            };
            total = total.saturating_add(part_len);
        }
    }
    total
}

fn total_text_block_bytes(blocks: &[AcpPromptBlock]) -> usize {
    blocks
        .iter()
        .map(|b| match b {
            AcpPromptBlock::Text { text } => text.len(),
            AcpPromptBlock::Image { .. } => 0,
        })
        .sum()
}

fn floor_utf8_boundary(s: &str, max_byte: usize) -> usize {
    let mut idx = max_byte.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_utf8_boundary(s: &str, min_byte: usize) -> usize {
    let mut idx = min_byte.min(s.len());
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

fn count_logical_lines(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        s.bytes().filter(|&b| b == b'\n').count() + usize::from(!s.ends_with('\n'))
    }
}

/// Truncates an oversized UTF-8 payload in the middle while preserving a generous head (~55%)
/// and tail (~45%) snapped cleanly to line boundaries, inserting a structured diagnostic marker
/// with exact byte and 1-based line ranges so coding agents understand that the middle was
/// omitted and can inspect a targeted range if needed.
#[must_use]
pub fn truncate_head_tail_lines(input: &str, max_bytes: usize, kind_label: &str) -> String {
    if input.len() <= max_bytes {
        return input.to_owned();
    }
    if max_bytes <= 380 {
        return truncate_middle_utf8_bytes(input, max_bytes, 11, 20);
    }

    let total_bytes = input.len();
    let marker_reserve = 360usize;
    let usable = max_bytes.saturating_sub(marker_reserve);
    let head_budget = usable.saturating_mul(11) / 20;
    let tail_budget = usable.saturating_sub(head_budget);

    let raw_head_end = floor_utf8_boundary(input, head_budget);
    let min_head_snap = (head_budget * 4) / 5;
    let head_end = input
        .get(min_head_snap..raw_head_end)
        .and_then(|win| win.rfind('\n').map(|pos| min_head_snap + pos))
        .unwrap_or(raw_head_end);

    let raw_tail_start =
        ceil_utf8_boundary(input, total_bytes.saturating_sub(tail_budget)).max(head_end);
    let max_tail_snap = total_bytes
        .saturating_sub((tail_budget * 4) / 5)
        .max(raw_tail_start);
    let tail_start = input
        .get(raw_tail_start..max_tail_snap)
        .and_then(|win| win.find('\n').map(|pos| raw_tail_start + pos + 1))
        .unwrap_or(raw_tail_start)
        .max(head_end);

    let head = input
        .get(..head_end)
        .unwrap_or("")
        .trim_end_matches(['\r', '\n']);
    let tail = input
        .get(tail_start..)
        .unwrap_or("")
        .trim_start_matches(['\r', '\n']);
    let omitted_end = total_bytes.saturating_sub(tail.len()).max(head.len());
    let omitted_slice = input.get(head.len()..omitted_end).unwrap_or("");
    let omitted_bytes = omitted_slice.len();

    let notice = if omitted_slice.contains('\n') {
        let total_lines = count_logical_lines(input);
        let head_lines = if head.is_empty() {
            0
        } else {
            head.bytes().filter(|&b| b == b'\n').count() + 1
        };
        let tail_lines = count_logical_lines(tail);
        let first_omitted = head_lines + 1;
        let last_omitted = total_lines.saturating_sub(tail_lines).max(first_omitted);
        let omitted_lines = last_omitted.saturating_sub(first_omitted) + 1;
        format!(
            "\n... [Gateway truncated {kind_label}: {total_bytes} bytes ({total_lines} lines) total; omitted middle {omitted_bytes} bytes ({omitted_lines} lines, lines {first_omitted}..={last_omitted}) to fit single-turn limit. Beginning and end are preserved intact; query that specific line range or filter output if the middle section is needed.] ...\n"
        )
    } else {
        format!(
            "\n... [Gateway truncated {kind_label}: {total_bytes} bytes total; omitted middle {omitted_bytes} bytes to fit single-turn limit. Beginning and end are preserved intact.] ...\n"
        )
    };

    let mut out = String::with_capacity(head.len() + notice.len() + tail.len());
    out.push_str(head);
    out.push_str(&notice);
    out.push_str(tail);
    if out.len() > max_bytes {
        truncate_middle_utf8_bytes(&out, max_bytes, 11, 20)
    } else {
        out
    }
}

fn truncate_middle_utf8_bytes(
    input: &str,
    max_bytes: usize,
    head_num: usize,
    denom: usize,
) -> String {
    if input.len() <= max_bytes {
        return input.to_owned();
    }
    if max_bytes <= 80 {
        let head_end = floor_utf8_boundary(input, max_bytes.saturating_sub(3));
        let head = input.get(..head_end).unwrap_or("");
        return format!("{head}...");
    }

    let marker_overhead = 56usize;
    let usable = max_bytes.saturating_sub(marker_overhead);
    let safe_denom = denom.max(1);
    let head_budget = usable.saturating_mul(head_num.min(safe_denom)) / safe_denom;
    let tail_budget = usable.saturating_sub(head_budget);

    let head_end = floor_utf8_boundary(input, head_budget);
    let tail_start =
        ceil_utf8_boundary(input, input.len().saturating_sub(tail_budget)).max(head_end);

    let head = input.get(..head_end).unwrap_or("").trim_end();
    let tail = input.get(tail_start..).unwrap_or("").trim_start();
    let omitted_bytes = tail_start.saturating_sub(head_end);

    let mut out = String::with_capacity(head.len() + tail.len() + marker_overhead);
    out.push_str(head);
    out.push_str(&format!("\n...[omitted {omitted_bytes} bytes]...\n"));
    out.push_str(tail);
    out
}

fn render_parts_into(
    msg: &CanonicalMessage,
    tool_nonce: &str,
    is_archival: bool,
    limits: PartRenderLimits,
    text_acc: &mut String,
    blocks: &mut Vec<AcpPromptBlock>,
) {
    for part in &msg.parts {
        match part {
            CanonicalPart::Text(text) => {
                let cleaned = strip_billing_noise(text);
                if !cleaned.trim().is_empty() {
                    if !text_acc.is_empty() && !text_acc.ends_with('\n') {
                        text_acc.push('\n');
                    }
                    if cleaned.len() > limits.max_text_bytes {
                        let compacted =
                            truncate_middle_utf8_bytes(&cleaned, limits.max_text_bytes, 13, 20);
                        text_acc.push_str(&compacted);
                    } else {
                        text_acc.push_str(&cleaned);
                    }
                }
            }
            CanonicalPart::ImageBase64 { media_type, data } => {
                if limits.emit_images {
                    if !text_acc.is_empty() {
                        blocks.push(AcpPromptBlock::Text {
                            text: std::mem::take(text_acc),
                        });
                    }
                    blocks.push(AcpPromptBlock::Image {
                        data: data.clone(),
                        mime_type: media_type.clone(),
                    });
                } else {
                    if !text_acc.is_empty() && !text_acc.ends_with('\n') {
                        text_acc.push('\n');
                    }
                    text_acc.push_str(&format!("[Prior Image media_type={media_type}]\n"));
                }
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
                    let trimmed_args = arguments.trim();
                    let arg_summary = if trimmed_args.len() > limits.max_tool_call_bytes {
                        truncate_middle_utf8_bytes(trimmed_args, limits.max_tool_call_bytes, 13, 20)
                    } else {
                        trimmed_args.to_owned()
                    };
                    text_acc.push_str(&format!(
                        "[Completed Tool Invocation id={call_id} name={name}]: {arg_summary}\n"
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
                let cleaned_output = strip_billing_noise(output);
                let trimmed_output = cleaned_output.trim();
                let rendered_output = if trimmed_output.len() > limits.max_tool_result_bytes {
                    truncate_middle_utf8_bytes(trimmed_output, limits.max_tool_result_bytes, 13, 20)
                } else {
                    trimmed_output.to_owned()
                };
                text_acc.push_str(&format!(
                    "[Client Tool Result id={call_id} status={}]:\n{rendered_output}\n[End Client Tool Result]\n",
                    if *is_error { "error" } else { "ok" },
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
    fn lossless_hydration_plan_splits_420kb_transcript_without_truncation_and_escapes_slash() {
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

        let plan = build_acp_prompt_plan(&messages, "abcd1234ef56", true, None, true).unwrap();
        let safe = build_acp_prompt_plan(
            &messages,
            "abcd1234ef56",
            true,
            Some("<client_tools_update>new tool</client_tools_update>"),
            false,
        )
        .unwrap();
        assert!(safe.seed_chunks.is_empty());
        assert!(total_text_block_bytes(&safe.final_blocks) <= MAX_ACP_PROMPT_TEXT_BYTES);
        let safe_text = safe
            .final_blocks
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
                false
            )
            .is_err()
        );
        let oversized = [CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::Text(format!(
                "MEGA_HEAD_MARKER\n{}\nMEGA_TAIL_MARKER",
                "x".repeat(980_000)
            ))],
        }];
        let bounded_mega = build_acp_prompt_plan(&oversized, "nonce", false, None, true)
            .expect("oversized long-context turn should auto-truncate middle instead of failing");
        let mega_text: String = bounded_mega
            .final_blocks
            .iter()
            .filter_map(|b| match b {
                AcpPromptBlock::Text { text } => Some(text.as_str()),
                AcpPromptBlock::Image { .. } => None,
            })
            .collect();
        assert!(mega_text.contains("MEGA_HEAD_MARKER"));
        assert!(mega_text.contains("MEGA_TAIL_MARKER"));
        assert!(mega_text.contains("[Gateway truncated input text:"));
        assert!(
            !plan.seed_chunks.is_empty() && plan.seed_chunks.len() <= MAX_HYDRATION_SEED_CHUNKS,
            "expected 1..={} seed chunks, got {}",
            MAX_HYDRATION_SEED_CHUNKS,
            plan.seed_chunks.len()
        );
        for (idx, chunk) in plan.seed_chunks.iter().enumerate() {
            let bytes = total_text_block_bytes(chunk);
            assert!(
                bytes <= MAX_ACP_PROMPT_TEXT_BYTES,
                "seed chunk {idx} len {bytes} exceeded {MAX_ACP_PROMPT_TEXT_BYTES}"
            );
        }
        assert!(total_text_block_bytes(&plan.final_blocks) <= MAX_ACP_PROMPT_TEXT_BYTES);

        let mut combined = String::new();
        for chunk in &plan.seed_chunks {
            for block in chunk {
                if let AcpPromptBlock::Text { text } = block {
                    combined.push_str(text);
                }
            }
        }
        for block in &plan.final_blocks {
            if let AcpPromptBlock::Text { text } = block {
                assert!(
                    !text.starts_with('/'),
                    "leading slash must be escaped so `agy acp` does not intercept slash commands"
                );
                combined.push_str(text);
            }
        }
        assert!(!combined.contains("...[omitted"));
        assert!(!combined.contains("[... omitted"));
        assert!(!combined.contains("[Gateway truncated"));
        assert!(combined.contains("EARLY_NEEDLE_ALPHA_001"));
        assert!(combined.contains("MID_NEEDLE_BETA_002"));
        assert!(combined.contains("MID_NEEDLE_GAMMA_003"));
        assert!(combined.contains("LATE_NEEDLE_DELTA_004"));
        assert!(combined.contains("FINAL_NEEDLE_OMEGA_005"));

        // Direct warm-delta prompt starting with `/compact` must escape the leading slash.
        let direct_slash = build_acp_prompt_plan(
            &[CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text("/compact focus on auth".to_owned())],
            }],
            "abcd1234ef56",
            false,
            None,
            false,
        )
        .unwrap();
        assert!(direct_slash.seed_chunks.is_empty());
        match direct_slash.final_blocks.first() {
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
            false,
        )
        .expect("giant developer_instructions should slim instead of failing Turn 1");
        let rendered = match plan.final_blocks.first() {
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
        let bounded_user_plan =
            build_acp_prompt_plan(&[oversized_user], "abcd12", true, None, false)
                .expect("oversized user paste must middle-truncate instead of failing");
        let bounded_user_text = match bounded_user_plan.final_blocks.first() {
            Some(AcpPromptBlock::Text { text }) => text,
            _ => panic!("expected text block"),
        };
        assert!(bounded_user_text.len() <= MAX_ACP_PROMPT_TEXT_BYTES);
        assert!(bounded_user_text.contains("PASTE_HEAD_LINE"));
        assert!(bounded_user_text.contains("PASTE_TAIL_LINE"));
        assert!(bounded_user_text.contains("[Gateway truncated input text:"));
    }

    #[test]
    fn waterfill_head_tail_truncates_195kb_tool_output_preserving_small_siblings_and_session_hash()
    {
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

        let plan = build_acp_prompt_plan(
            std::slice::from_ref(&turn_delta),
            "abcd12",
            false,
            None,
            false,
        )
        .expect("195 KB tool output must truncate middle cleanly without erroring");

        assert!(plan.seed_chunks.is_empty());
        let rendered = match plan.final_blocks.first() {
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
        assert!(
            repl_append.contains("`edit(path, old, new)`, `create(path, content)`, `view(path)`")
        );
    }
}
