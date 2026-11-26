//! Deterministic conversation prefix hashing, noise normalization, and ACP prompt/system builders.

mod codemode;
mod prompt_plan;
mod spill;
mod system_append;
#[cfg(test)]
mod tests;

use sha2::{Digest, Sha256};

use crate::canonical::{CanonicalMessage, CanonicalPart, CanonicalRole, CanonicalToolSpec};
use crate::config::ModelId;

pub use codemode::{
    McpOrchestratorContract, compact_mcp_tool_description, is_code_mode_custom_exec,
    is_code_mode_orchestrator_tool, is_omp_eval_orchestrator_tool,
    is_opencode_execute_orchestrator_tool, is_opencode_full_code_mode_exec,
    split_code_mode_ts_declaration,
};
pub use prompt_plan::{
    MAX_ACP_PROMPT_TEXT_BYTES, build_acp_prompt_blocks, build_acp_prompt_blocks_with_spill,
    build_acp_prompt_plan, build_acp_prompt_plan_with_spill,
};
#[cfg(any(test, feature = "test-support"))]
pub use spill::MemorySpillSink;
pub use spill::{
    NoopSpillSink, TruncationSpillSink, format_content_addressed_spill_filename,
    scrub_terminal_control_noise, truncate_head_tail_lines, truncate_head_tail_lines_with_spill,
};
pub use system_append::{
    INTERNAL_CODEX_LOCAL_SHELL_TOOL, INTERNAL_CODEX_TOOL_SEARCH_TOOL,
    INTERNAL_GATEWAY_WEB_FETCH_TOOL, MAX_SYSTEM_APPEND_BYTES, McpBridgePromptSpec,
    WebToolBridgeConfig, build_client_tools_delta_xml, build_client_tools_delta_xml_with_spill,
    build_compaction_directive_xml, build_compaction_directive_xml_with_spill,
    build_mcp_virtualized_tools_delta_xml_with_spill, build_session_system_append,
    build_session_system_append_for_mcp_with_spill_sink, build_session_system_append_with_sink,
    build_session_system_append_with_spill, build_session_system_append_with_spill_sink,
};

pub type ConversationHash = [u8; 32];

/// Formats a 32-byte [`ConversationHash`] as a lowercase 64-character hex string for telemetry.
#[must_use]
pub fn format_conversation_hash_hex(hash: &ConversationHash) -> String {
    let mut out = String::with_capacity(64);
    for &byte in hash {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

/// incrementally on warm sessions via `<client_tools_update>` so mode switches, `ToolSearch`,
/// lazy app namespaces, and `/compact` never invalidate warm KV caches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRoutingPlan {
    pub prefix_lookup: Option<(ConversationHash, usize)>,
    /// Secondary prefix lookup when a prior in-flight turn on a warm session was cancelled
    /// (via `session/cancel`) after already delivering `messages[..len - 1]` to `agy acp`,
    /// and the client immediately follows up with an appended `User` steering message.
    pub cancelled_turn_lookup: Option<(ConversationHash, usize)>,
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

    let cancelled_turn_lookup = {
        let after_assistant = last_assistant_idx.map_or(0, |idx| idx + 1);
        let trailing_substantive: Vec<usize> = (after_assistant..messages.len())
            .filter(|&i| {
                messages.get(i).is_some_and(|m| {
                    m.role == CanonicalRole::User && message_has_stable_hash_content(m, true)
                })
            })
            .collect();
        if let Some(&prev_user_idx) =
            trailing_substantive.get(trailing_substantive.len().wrapping_sub(2))
        {
            let split_at = prev_user_idx + 1;
            let already_covered = prefix_lookup.is_some_and(|(_, p_split)| p_split == split_at);
            if !already_covered && let Some(prefix_slice) = messages.get(..split_at) {
                Some((
                    hash_conversation_state(system, tools, web_cfg, model, prefix_slice),
                    split_at,
                ))
            } else {
                None
            }
        } else {
            None
        }
    };

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
        cancelled_turn_lookup,
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

pub(super) fn is_developer_instructions_wrapper(text: &str) -> bool {
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
/// (such as OpenAI Codex's `CONTEXT CHECKPOINT COMPACTION`, Claude Code's `/compact` prompt,
/// OpenCode's session compaction summary prompt, or Oh My Pi's structured handoff summary).
#[must_use]
pub fn is_compaction_instructions_prompt(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("context checkpoint compaction")
        || lower.contains("handoff summary for another llm")
        || (lower.contains("compaction") && lower.contains("another language model"))
        || lower.contains("summarize user–ai coding-assistant conversations")
        || lower.contains("summarize user-ai coding-assistant conversations")
        || (lower.contains("complete history to summarize")
            && lower.contains("replaces every message"))
        || lower.contains("write a handoff document for another instance of yourself")
        || (lower.contains("turn prefix too large")
            && lower.contains("summarize prefix for retained suffix"))
        || lower
            .contains("structured summary of the conversation branch for context when returning")
        || lower.contains("create a detailed summary of the conversation so far")
        || lower.contains("tasked with summarizing conversations")
        || lower.contains("provide a detailed prompt for continuing our conversation above")
        || (lower.contains("summarize only what the user and the assistant said and did")
            && lower.contains("## work state"))
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
        sort_json_object_keys_recursive(&mut val);
        if let Ok(compact) = serde_json::to_string(&val) {
            return compact;
        }
    }
    raw.trim().to_owned()
}

fn sort_json_object_keys_recursive(val: &mut serde_json::Value) {
    match val {
        serde_json::Value::Object(map) => {
            let mut sorted: std::collections::BTreeMap<String, serde_json::Value> =
                std::mem::take(map).into_iter().collect();
            for child in sorted.values_mut() {
                sort_json_object_keys_recursive(child);
            }
            for (k, v) in sorted {
                map.insert(k, v);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                sort_json_object_keys_recursive(item);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
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
    let s9 = strip_tag_blocks(&s8, "<current_date>", "</current_date>");
    let without_volatile_tags = strip_tag_blocks(&s9, "<timestamp>", "</timestamp>");
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

pub(super) fn strip_tag_blocks(input: &str, open_tag: &str, close_tag: &str) -> String {
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
