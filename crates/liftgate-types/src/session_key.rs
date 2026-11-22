use sha2::{Digest, Sha256};

use crate::acp::AcpPromptBlock;
use crate::canonical::{
    CanonicalMessage, CanonicalPart, CanonicalRole, CanonicalToolKind, CanonicalToolSpec,
};
use crate::config::ModelId;
use crate::error::ProtocolError;

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

/// Pluggable sink that persists the complete, un-truncated UTF-8 payload to a local file
/// before any head+tail truncation, schema compaction, or archival turn omission is applied.
///
/// Implementations return the normalized local filesystem path so inline omission markers
/// can tell client agents (`Read`, `Grep`, `rg`, shell tools) exactly where the lossless
/// content is stored on disk.
pub trait TruncationSpillSink {
    fn spill_truncated_content(&mut self, kind_label: &str, full_content: &str) -> Option<String>;
}

/// No-op [`TruncationSpillSink`] used when local spill persistence is not configured.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopSpillSink;

impl TruncationSpillSink for NoopSpillSink {
    fn spill_truncated_content(
        &mut self,
        _kind_label: &str,
        _full_content: &str,
    ) -> Option<String> {
        None
    }
}

impl<S: TruncationSpillSink + ?Sized> TruncationSpillSink for &mut S {
    fn spill_truncated_content(&mut self, kind_label: &str, full_content: &str) -> Option<String> {
        (**self).spill_truncated_content(kind_label, full_content)
    }
}

impl<S: TruncationSpillSink> TruncationSpillSink for Option<S> {
    fn spill_truncated_content(&mut self, kind_label: &str, full_content: &str) -> Option<String> {
        self.as_mut()
            .and_then(|inner| inner.spill_truncated_content(kind_label, full_content))
    }
}

/// Formats a deterministic, content-addressed spill filename (`<slug>-<sha256_16>.<ext>`)
/// so identical payloads across turns or retries deduplicate cleanly and preserve prompt
/// determinism for upstream KV prefix caching.
#[must_use]
pub fn format_content_addressed_spill_filename(kind_label: &str, full_content: &str) -> String {
    let mut slug = String::with_capacity(kind_label.len().min(40));
    let mut prev_dash = false;
    for ch in kind_label.chars() {
        if slug.len() >= 40 {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !slug.is_empty() && !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    while slug.ends_with('-') {
        let _ = slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("payload");
    }

    let digest = Sha256::digest(full_content.as_bytes());
    let mut hash_hex = String::with_capacity(16);
    for &byte in digest.get(..8).unwrap_or(&[]) {
        use std::fmt::Write as _;
        let _ = write!(&mut hash_hex, "{byte:02x}");
    }

    let trimmed = full_content.trim();
    let ext = if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && serde_json::from_str::<serde::de::IgnoredAny>(trimmed).is_ok()
    {
        "json"
    } else {
        "txt"
    };

    format!("{slug}-{hash_hex}.{ext}")
}

/// In-memory [`TruncationSpillSink`] for deterministic unit and integration assertions.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug, Default)]
pub struct MemorySpillSink {
    base_dir: String,
    files: std::collections::BTreeMap<String, String>,
}

#[cfg(any(test, feature = "test-support"))]
impl MemorySpillSink {
    #[must_use]
    pub fn new(base_dir: impl Into<String>) -> Self {
        let mut dir = base_dir.into().replace('\\', "/");
        while dir.ends_with('/') {
            let _ = dir.pop();
        }
        Self {
            base_dir: dir,
            files: std::collections::BTreeMap::new(),
        }
    }

    #[must_use]
    pub const fn files(&self) -> &std::collections::BTreeMap<String, String> {
        &self.files
    }
}

#[cfg(any(test, feature = "test-support"))]
impl TruncationSpillSink for MemorySpillSink {
    fn spill_truncated_content(&mut self, kind_label: &str, full_content: &str) -> Option<String> {
        let file_name = format_content_addressed_spill_filename(kind_label, full_content);
        let full_path = format!("{}/{file_name}", self.base_dir);
        self.files
            .insert(full_path.clone(), full_content.to_owned());
        Some(full_path)
    }
}

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

fn compact_middle_chars(input: &str, max_chars: usize) -> (String, bool) {
    let total_chars = input.chars().count();
    if total_chars <= max_chars {
        return (input.to_owned(), false);
    }
    const MARKER: &str = " ... ";
    let marker_chars = 5usize;
    if max_chars <= marker_chars + 4 {
        let head: String = input.chars().take(max_chars.saturating_sub(3)).collect();
        return (format!("{head}..."), true);
    }
    let usable = max_chars.saturating_sub(marker_chars);
    let head_chars = (usable * 3) / 5;
    let tail_chars = usable.saturating_sub(head_chars);
    let head: String = input.chars().take(head_chars).collect();
    let tail_skip = total_chars.saturating_sub(tail_chars);
    let tail: String = input.chars().skip(tail_skip).collect();
    (
        format!("{}{MARKER}{}", head.trim_end(), tail.trim_start()),
        true,
    )
}

const MAX_CODE_MODE_TOOL_DESC_BYTES: usize = 36_000;
const MAX_SLIM_CODE_MODE_TOOL_DESC_BYTES: usize = 24_000;

/// Returns `true` when `tool` is a Codex Code Mode custom JavaScript `exec` tool
/// (`CanonicalToolKind::Custom` named `"exec"` or `"*.exec"`).
#[must_use]
pub fn is_code_mode_custom_exec(tool: &CanonicalToolSpec) -> bool {
    matches!(tool.kind, CanonicalToolKind::Custom { .. })
        && (tool.name == "exec" || tool.name.ends_with(".exec"))
}

/// Returns `true` when `tool` is a Code Mode orchestrator (`exec` custom tool, `REPL`, or TypeScript
/// `declare const tools` runtime wrapper).
#[must_use]
pub fn is_code_mode_orchestrator_tool(tool: &CanonicalToolSpec) -> bool {
    if tool.name == "REPL" {
        return true;
    }
    if is_code_mode_custom_exec(tool) {
        return true;
    }
    tool.description.as_deref().is_some_and(|desc| {
        desc.contains("declare const tools")
            || desc.contains("globalThis.tools")
            || desc.contains("Tool TypeScript declarations:")
            || desc.contains("// @exec:")
    })
}

/// Compacts a tool description for native loopback MCP `tools/list` while preserving full
/// Code Mode TypeScript `declare const tools` declarations up to `max_bytes` (spilling to disk if larger).
#[must_use]
pub fn compact_mcp_tool_description<S: TruncationSpillSink>(
    tool: &CanonicalToolSpec,
    max_bytes: usize,
    spill: &mut S,
) -> Option<String> {
    let raw_desc = tool.description.as_deref()?.trim();
    if raw_desc.is_empty() {
        return None;
    }
    if is_code_mode_orchestrator_tool(tool) {
        let (desc, _) = compact_code_mode_tool_description(raw_desc, max_bytes, spill);
        (!desc.is_empty()).then_some(desc)
    } else {
        let collapsed = collapse_tool_description(raw_desc);
        let bounded = if collapsed.len() > max_bytes {
            truncate_head_tail_lines_from_source(
                &collapsed,
                raw_desc,
                None,
                max_bytes,
                "MCP tool description",
                spill,
            )
        } else {
            collapsed
        };
        (!bounded.is_empty()).then_some(bounded)
    }
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

fn had_tool_boilerplate_tags(raw_desc: &str) -> bool {
    raw_desc.contains("<example>")
        || raw_desc.contains("<examples>")
        || raw_desc.contains("<commentary>")
}

fn compact_tool_description(raw_desc: &str) -> (String, bool) {
    let collapsed = collapse_tool_description(raw_desc);
    let (compacted, truncated) = compact_middle_chars(&collapsed, 600);
    (compacted, truncated || had_tool_boilerplate_tags(raw_desc))
}

fn compact_code_mode_tool_description<S: TruncationSpillSink>(
    raw_desc: &str,
    max_bytes: usize,
    spill: &mut S,
) -> (String, bool) {
    let collapsed = collapse_tool_description(raw_desc);
    let stripped_tags = had_tool_boilerplate_tags(raw_desc);
    if collapsed.len() <= max_bytes {
        if stripped_tags {
            if let Some(path) =
                spill.spill_truncated_content("Code Mode tool declarations", raw_desc)
            {
                let with_note = format!(
                    "{collapsed}\n  [Full unstripped Code Mode tool description saved locally at `{path}`.]"
                );
                if with_note.len() <= max_bytes {
                    return (with_note, true);
                }
            }
            (collapsed, true)
        } else {
            (collapsed, false)
        }
    } else {
        (
            truncate_head_tail_lines_from_source(
                &collapsed,
                raw_desc,
                None,
                max_bytes,
                "Code Mode tool declarations",
                spill,
            ),
            true,
        )
    }
}

fn compact_json_schema(raw_schema: &str) -> (String, bool) {
    let Ok(mut val) = serde_json::from_str::<serde_json::Value>(raw_schema) else {
        return (raw_schema.trim().to_owned(), false);
    };
    let mut truncated_any = false;
    slim_schema_value(&mut val, &mut truncated_any);
    let serialized = serde_json::to_string(&val).unwrap_or_else(|_| raw_schema.trim().to_owned());
    (serialized, truncated_any)
}

fn slim_schema_value(val: &mut serde_json::Value, truncated_any: &mut bool) {
    match val {
        serde_json::Value::Object(map) => {
            let _ = map.remove("$schema");
            let _ = map.remove("additionalProperties");
            if let Some(serde_json::Value::String(desc)) = map.get_mut("description")
                && desc.chars().count() > 140
            {
                let (short, _) = compact_middle_chars(desc, 140);
                *desc = short;
                *truncated_any = true;
            }
            for child in map.values_mut() {
                slim_schema_value(child, truncated_any);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                slim_schema_value(item, truncated_any);
            }
        }
        _ => {}
    }
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
    /// Freeform `Custom` tools (e.g. `apply_patch`) or overflow `#65+` tools invoked via XML.
    pub xml_tools: &'a [CanonicalToolSpec],
}

/// Builds `_meta.antigravity.systemPrompt.append` for sessions using the Hybrid Loopback MCP Tool
/// Bridge + XML Coexistence architecture.
///
/// When `mcp_spec.bridged_tools` is empty, delegates directly to
/// [`build_session_system_append_with_spill_sink`] so non-MCP turns remain 100% byte-identical.
#[must_use]
pub fn build_session_system_append_for_mcp_with_spill_sink<S: TruncationSpillSink>(
    caller_system: Option<&str>,
    mcp_spec: McpBridgePromptSpec<'_>,
    web_cfg: WebToolBridgeConfig,
    tool_nonce: &str,
    spill: &mut S,
) -> (String, Option<String>) {
    if mcp_spec.bridged_tools.is_empty() {
        return build_session_system_append_with_spill_sink(
            caller_system,
            mcp_spec.xml_tools,
            web_cfg,
            tool_nonce,
            spill,
        );
    }

    let open_tag = format!("<agy_tool_call_{tool_nonce}>");
    let close_tag = format!("</agy_tool_call_{tool_nonce}>");
    let xml_tools = mcp_spec.xml_tools;
    let has_xml_custom = xml_tools
        .iter()
        .any(|t| matches!(t.kind, CanonicalToolKind::Custom { .. }));

    let mut header_rules = String::with_capacity(2048);
    header_rules.push_str("<api_gateway_bridge_rules>\n");
    header_rules.push_str(
        "You are serving an external API harness via a structured protocol gateway.\n\
         - NEVER invoke internal host tools (`read_file`, `write_file`, `edit_file`, `run_command`, \
         `manage_task`, `ask_question`, `invoke_subagent`, `define_subagent`, `manage_subagents`, \
         `run_workflow`, `swarm`, `schedule`, `send_message`, `call_mcp_tool`) or any `mcp_*` tool \
         outside `mcp_lg_*`.\n\
         - Ignore internal ACP host folders (`liftgate-*`) and any host plugin rules/snapshots.\n\
         - Invoke the caller's native `mcp_lg_*` client tools directly as native tool calls (they \
         execute in the caller's real workspace from `<developer_instructions>` and `<caller_system_instructions>`). \
         Batch independent read-only searches, reads, or checks as parallel `mcp_lg_*` calls in a single turn.\n\
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
    } else if xml_tools
        .iter()
        .any(|t| t.name.eq_ignore_ascii_case("WebSearch"))
    {
        header_rules.push_str(&format!(
            "- When web search is needed, invoke the client tool `WebSearch` via `{open_tag}` XML \
             below (do NOT invoke internal `search_web` directly when client `WebSearch` is provided).\n"
        ));
    } else {
        header_rules.push_str(
            "- You MAY still use built-in `search_web` if the user explicitly asks to search the live web \
             and no client web-search tool is provided.\n",
        );
    }

    if !xml_tools.is_empty() || web_cfg.enable_native_web_fetch {
        header_rules.push_str(&format!(
            "- Coexisting XML-only `<client_tools>` below (such as freeform `apply_patch` or overflow tools) \
             are NOT native function calls: invoke those XML-only tools ONLY by writing `{open_tag}...{close_tag}` \
             blocks in plain assistant text (never wrap in code fences, and end your turn immediately after \
             `{close_tag}` with zero trailing text or native function calls in that step):\n\
             {open_tag}\n\
             {{\"name\": \"<exact_tool_name>\", \"arguments\": {{ ... }}}}\n\
             {close_tag}\n"
        ));
        if has_xml_custom {
            header_rules.push_str(
                "  (For `apply_patch`, emit raw multi-line `*** Begin Patch ... *** End Patch` directly inside \
                 the XML block WITHOUT JSON-escaping, keeping `@@` hunks surgical with 1-3 exact context lines.)\n",
            );
        }
        if web_cfg.enable_native_web_fetch {
            if web_cfg.force_native_web_fetch {
                header_rules.push_str(&format!(
                    "- MANDATORY SERVER WEB FETCH: Before answering, fetch the requested web URL by emitting \
                     `{open_tag}\n{{\"name\": \"{INTERNAL_GATEWAY_WEB_FETCH_TOOL}\", \"arguments\": {{\"url\": \"https://...\"}}}}\n{close_tag}` \
                     and stop; the gateway will immediately fetch the live page and deliver `<gateway_web_fetch_result>`.\n"
                ));
            } else {
                header_rules.push_str(&format!(
                    "- Server-managed URL fetching IS enabled: to read the full content of any `http://` or `https://` URL, \
                     emit `{open_tag}\n{{\"name\": \"{INTERNAL_GATEWAY_WEB_FETCH_TOOL}\", \"arguments\": {{\"url\": \"https://...\"}}}}\n{close_tag}` \
                     and stop; the gateway will fetch the URL server-side and return `<gateway_web_fetch_result>` in the same turn.\n"
                ));
            }
        }
    } else {
        header_rules.push_str(&format!(
            "- If later turns dynamically add XML-only tools via `<client_tools_update>`, invoke those \
             newly added XML tools via `{open_tag}\n{{\"name\": \"<exact_tool_name>\", \"arguments\": {{ ... }}}}\n{close_tag}` \
             in plain text and stop immediately after `{close_tag}`.\n"
        ));
    }

    let close_tag_rules = "</api_gateway_bridge_rules>\n";
    let close_with_tools = "</client_tools>\n</api_gateway_bridge_rules>\n";
    let mut bounded = header_rules;
    let mut inline_count = 0usize;
    let mut inline_compacted = false;
    let mut tool_buf = String::new();
    let catalog_note_reserve = 220usize;

    for tool in xml_tools {
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
        if bounded.len() + open_cost + tool_buf.len() + note_cost + close_with_tools.len()
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
        if inline_compacted && let Some(inline_slice) = xml_tools.get(..inline_count) {
            let inline_refs: Vec<&CanonicalToolSpec> = inline_slice.iter().collect();
            if let Some(catalog_path) = spill.spill_truncated_content(
                "client tools catalog",
                &serialize_full_client_tools_catalog(&inline_refs),
            ) {
                let note = format!(
                    "  # Full lossless inline tool schemas saved locally at `{catalog_path}`.\n"
                );
                if bounded.len() + note.len() + close_with_tools.len() <= MAX_SYSTEM_APPEND_BYTES {
                    bounded.push_str(&note);
                }
            }
        }
        bounded.push_str(close_with_tools);
    } else {
        bounded.push_str(close_tag_rules);
    }

    let mut combined_spill_xml = String::new();
    if let Some(raw_sys) = caller_system {
        let compacted_sys = compact_caller_system(raw_sys, spill);
        if !compacted_sys.is_empty() {
            let sys_wrapper_len =
                "\n<caller_system_instructions>\n\n</caller_system_instructions>\n".len();
            if xml_tools.get(inline_count..).is_none_or(<[_]>::is_empty)
                && bounded.len() + sys_wrapper_len + compacted_sys.len() <= MAX_SYSTEM_APPEND_BYTES
            {
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

    if let Some(spilled) = xml_tools.get(inline_count..)
        && !spilled.is_empty()
    {
        let spilled_refs: Vec<&CanonicalToolSpec> = spilled.iter().collect();
        combined_spill_xml.push_str(
            "<client_tools_update>\nAdditional active XML `<client_tools>` for this session:\n",
        );
        render_bounded_tools_list(
            &spilled_refs,
            MAX_TOOL_DIRECTIVE_BYTES,
            &mut combined_spill_xml,
            spill,
        );
        combined_spill_xml.push_str("</client_tools_update>\n");
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

/// Maximum total UTF-8 text bytes emitted in a single `session/prompt` request.
///
/// Upstream `agy acp` concatenates text blocks in `session/prompt` and truncates at `191,579` bytes
/// (`<truncated N bytes>`). Keeping each ACP turn at or below `176,000` bytes provides >15.5 KB of
/// safety headroom while preserving line-aligned head and tail sections when oversized tool outputs
/// or logs arrive.
pub const MAX_ACP_PROMPT_TEXT_BYTES: usize = 176_000;

/// Progressive schema-compaction ceiling for spilled or incremental `<client_tools_update>` blocks.
const MAX_TOOL_DIRECTIVE_BYTES: usize = 48_000;

const MAX_ACTIVE_WITH_HISTORY_TEXT_BYTES: usize = 76_000;
const MAX_HISTORY_RESERVE_BYTES: usize = 48_000;
const RECENT_TAIL_HISTORY_MESSAGES: usize = 28;

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
        MAX_ACP_PROMPT_TEXT_BYTES > MAX_HISTORY_RESERVE_BYTES * 3,
        "history reserve on cold replays must never starve the active turn below two-thirds of the turn budget"
    );
};

const ARCHIVE_OPEN_HEADER: &str =
    "--- Prior Conversation Context (Read-Only Archive; Do NOT echo these headers) ---\n";
const ARCHIVE_CLOSE_HEADER: &str = "--- End Prior Conversation Context ---\n\n### Current Caller Input (Respond ONLY for the current assistant step)\n";

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
    build_acp_prompt_blocks_with_spill(
        messages,
        tool_nonce,
        is_full_history_replay,
        &mut NoopSpillSink,
    )
}

/// Like [`build_acp_prompt_blocks`], persisting any truncated payloads or omitted archival
/// turns through `spill`.
#[must_use]
pub fn build_acp_prompt_blocks_with_spill<S: TruncationSpillSink>(
    messages: &[CanonicalMessage],
    tool_nonce: &str,
    is_full_history_replay: bool,
    spill: &mut S,
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
        spill,
    ))
}

/// Builds a single-turn bounded ACP prompt (`<= MAX_ACP_PROMPT_TEXT_BYTES`), automatically
/// middle-truncating oversized tool results or pasted logs on clean line boundaries so agentic
/// loops never stall on large command/file outputs while warm sessions accumulate naturally.
pub fn build_acp_prompt_plan(
    messages: &[CanonicalMessage],
    tool_nonce: &str,
    is_full_history_replay: bool,
    leading_directive_xml: Option<&str>,
) -> Result<Vec<AcpPromptBlock>, ProtocolError> {
    build_acp_prompt_plan_with_spill(
        messages,
        tool_nonce,
        is_full_history_replay,
        leading_directive_xml,
        &mut NoopSpillSink,
    )
}

/// Like [`build_acp_prompt_plan`], persisting full original payloads to local files via `spill`
/// and embedding exact file locations inside every head+tail or archival omission marker.
pub fn build_acp_prompt_plan_with_spill<S: TruncationSpillSink>(
    messages: &[CanonicalMessage],
    tool_nonce: &str,
    is_full_history_replay: bool,
    leading_directive_xml: Option<&str>,
    spill: &mut S,
) -> Result<Vec<AcpPromptBlock>, ProtocolError> {
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
        return Ok(sanitize_slash_command_blocks(vec![AcpPromptBlock::Text {
            text,
        }]));
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

    let budget = MAX_ACP_PROMPT_TEXT_BYTES.saturating_sub(directive_bytes);
    let mut blocks = if history_slice.is_empty() {
        render_smart_bounded_active_slice(active_slice, tool_nonce, budget, spill)
    } else {
        let uncompacted = render_uncompacted_prompt_blocks(history_slice, active_slice, tool_nonce);
        if total_text_block_bytes(&uncompacted) <= budget {
            uncompacted
        } else {
            let hist_reserve = estimate_raw_prompt_bytes(history_slice, &[])
                .min(budget / 3)
                .min(MAX_HISTORY_RESERVE_BYTES);
            let active_budget = budget.saturating_sub(hist_reserve);
            let active =
                render_smart_bounded_active_slice(active_slice, tool_nonce, active_budget, spill);
            render_history_around_active(history_slice, active, tool_nonce, budget, spill)
        }
    };
    prepend_directive_to_blocks(&mut blocks, leading_directive_xml);
    let mut blocks = sanitize_slash_command_blocks(blocks);
    clamp_blocks_to_budget(&mut blocks, MAX_ACP_PROMPT_TEXT_BYTES, spill);
    let bytes = total_text_block_bytes(&blocks);
    if bytes > MAX_ACP_PROMPT_TEXT_BYTES {
        return Err(ProtocolError::PromptTooLarge { bytes });
    }
    Ok(blocks)
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
                &mut NoopSpillSink,
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
            &mut NoopSpillSink,
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

fn render_bounded_prompt_blocks<S: TruncationSpillSink>(
    history_slice: &[CanonicalMessage],
    active_slice: &[CanonicalMessage],
    tool_nonce: &str,
    max_bytes: usize,
    spill: &mut S,
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
    let active_blocks =
        render_smart_bounded_active_slice(active_slice, tool_nonce, active_budget, spill);
    render_history_around_active(history_slice, active_blocks, tool_nonce, max_bytes, spill)
}

fn render_history_around_active<S: TruncationSpillSink>(
    history_slice: &[CanonicalMessage],
    active_blocks: Vec<AcpPromptBlock>,
    tool_nonce: &str,
    max_bytes: usize,
    spill: &mut S,
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

    let history_body =
        build_compacted_history_body(history_slice, tool_nonce, history_budget, spill);

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

fn clamp_blocks_to_budget<S: TruncationSpillSink>(
    blocks: &mut [AcpPromptBlock],
    max_bytes: usize,
    spill: &mut S,
) {
    for _ in 0..4 {
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
        let Some((idx, cur_len)) = largest_idx else {
            return;
        };
        if cur_len == 0 {
            return;
        }
        if let Some(AcpPromptBlock::Text { text }) = blocks.get_mut(idx) {
            let target = cur_len.saturating_sub(excess.saturating_add(48));
            *text = truncate_head_tail_lines_with_spill(text, target, "prompt content", spill);
        }
    }
}

fn unwrap_developer_instructions_inner(raw: &str) -> &str {
    raw.trim()
        .strip_prefix("<developer_instructions>")
        .and_then(|s| s.strip_suffix("</developer_instructions>"))
        .unwrap_or(raw)
        .trim()
}

/// Renders `active_slice` losslessly whenever it fits within `budget`, and otherwise applies a
/// prioritized, water-filling head+tail compaction pipeline:
/// 1. Strip `<examples>` / `<commentary>` boilerplate inside `<developer_instructions>`.
/// 2. Water-fill middle-truncate oversized `ToolResult` outputs while keeping small tool outputs,
///    caller prompts, and `<developer_instructions>` 100% lossless.
/// 3. Slim oversized `<developer_instructions>` blocks while preserving caller prompts intact.
/// 4. Universal water-fill middle-truncation across all oversized variable parts (pasted logs,
///    attachments, or giant tool payloads) so single-turn overflow never bricks the agent loop.
fn render_smart_bounded_active_slice<S: TruncationSpillSink>(
    active_slice: &[CanonicalMessage],
    tool_nonce: &str,
    budget: usize,
    spill: &mut S,
) -> Vec<AcpPromptBlock> {
    let uncompacted = render_uncompacted_prompt_blocks(&[], active_slice, tool_nonce);
    if total_text_block_bytes(&uncompacted) <= budget {
        return uncompacted;
    }

    // Stage 1: Strip `<examples>`, `<example>`, and `<commentary>` inside `<developer_instructions>`.
    let mut work: Vec<CanonicalMessage> = active_slice.to_vec();
    let mut dev_part_count = 0usize;
    let mut dev_originals: std::collections::BTreeMap<(usize, usize), (String, Option<String>)> =
        std::collections::BTreeMap::new();
    for (msg_idx, msg) in work.iter_mut().enumerate() {
        for (part_idx, part) in msg.parts.iter_mut().enumerate() {
            if let CanonicalPart::Text(text) = part
                && is_developer_instructions_wrapper(text)
            {
                dev_part_count += 1;
                let orig_inner = unwrap_developer_instructions_inner(text).to_owned();
                let s1 = strip_tag_blocks(&orig_inner, "<examples>", "</examples>");
                let s2 = strip_tag_blocks(&s1, "<example>", "</example>");
                let stripped_inner = strip_tag_blocks(&s2, "<commentary>", "</commentary>")
                    .trim()
                    .to_owned();
                let pre_spilled = if stripped_inner.len() < orig_inner.len() {
                    let path = spill.spill_truncated_content("developer instructions", &orig_inner);
                    if let Some(ref p) = path {
                        *text = format!(
                            "<developer_instructions>\n{stripped_inner}\n[Gateway omitted inline <example>/<commentary> blocks to fit single-turn limit; full original developer instructions saved locally at `{p}`.]\n</developer_instructions>"
                        );
                    } else {
                        *text = format!(
                            "<developer_instructions>\n{stripped_inner}\n</developer_instructions>"
                        );
                    }
                    path
                } else {
                    None
                };
                dev_originals.insert((msg_idx, part_idx), (orig_inner, pre_spilled));
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
                let scrubbed = scrub_terminal_control_noise(cleaned.trim());
                result_lens.push(scrubbed.trim().len());
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
                        let raw_trimmed = cleaned.trim();
                        let scrubbed = scrub_terminal_control_noise(raw_trimmed);
                        let trimmed = scrubbed.trim();
                        if trimmed.len() > cap {
                            *output = truncate_head_tail_lines_from_source(
                                trimmed,
                                raw_trimmed,
                                None,
                                cap,
                                "tool output",
                                spill,
                            );
                        } else if matches!(scrubbed, std::borrow::Cow::Owned(_)) {
                            *output = trimmed.to_owned();
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
                for (msg_idx, msg) in stage3_msgs.iter_mut().enumerate() {
                    for (part_idx, part) in msg.parts.iter_mut().enumerate() {
                        if let CanonicalPart::Text(text) = part
                            && is_developer_instructions_wrapper(text)
                        {
                            let inner = unwrap_developer_instructions_inner(text);
                            let orig_entry = dev_originals.get(&(msg_idx, part_idx));
                            let full_src = orig_entry.map_or(inner, |(orig, _)| orig.as_str());
                            let pre_spilled = orig_entry.and_then(|(_, p)| p.as_deref());
                            let body_budget = per_dev.saturating_sub(64);
                            let bounded = if inner.len() > body_budget {
                                truncate_head_tail_lines_from_source(
                                    inner,
                                    full_src,
                                    pre_spilled,
                                    body_budget,
                                    "developer instructions",
                                    spill,
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
            }
        }
    }

    // Stage 4: Universal water-filling starting cleanly from `active_slice` (so no `ToolResult`
    // or `<developer_instructions>` is ever double-truncated across stages).
    let mut stage4_work: Vec<CanonicalMessage> = active_slice.to_vec();
    if let Some(dev_count) = std::num::NonZeroUsize::new(dev_part_count) {
        let dev_share = (budget / 5).max(2_048) / dev_count;
        for (msg_idx, msg) in stage4_work.iter_mut().enumerate() {
            for (part_idx, part) in msg.parts.iter_mut().enumerate() {
                if let CanonicalPart::Text(text) = part
                    && is_developer_instructions_wrapper(text)
                    && text.len() > dev_share
                {
                    let inner = unwrap_developer_instructions_inner(text);
                    let orig_entry = dev_originals.get(&(msg_idx, part_idx));
                    let full_src = orig_entry.map_or(inner, |(orig, _)| orig.as_str());
                    let pre_spilled = orig_entry.and_then(|(_, p)| p.as_deref());
                    let bounded = truncate_head_tail_lines_from_source(
                        inner,
                        full_src,
                        pre_spilled,
                        dev_share.saturating_sub(64),
                        "developer instructions",
                        spill,
                    );
                    *text =
                        format!("<developer_instructions>\n{bounded}\n</developer_instructions>");
                }
            }
        }
    }

    let mut variable_lens = Vec::new();
    let mut fixed_overhead = 128usize;
    for msg in &stage4_work {
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
                    let scrubbed = scrub_terminal_control_noise(cleaned.trim());
                    variable_lens.push(scrubbed.trim().len());
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
    for (msg_idx, msg) in stage4_work.iter_mut().enumerate() {
        for (part_idx, part) in msg.parts.iter_mut().enumerate() {
            match part {
                CanonicalPart::Text(text) => {
                    let cleaned = strip_billing_noise(text);
                    let trimmed = cleaned.trim();
                    if trimmed.len() > universal_cap {
                        if is_developer_instructions_wrapper(trimmed) {
                            let inner = unwrap_developer_instructions_inner(trimmed);
                            let orig_entry = dev_originals.get(&(msg_idx, part_idx));
                            let full_src = orig_entry.map_or(inner, |(orig, _)| orig.as_str());
                            let pre_spilled = orig_entry.and_then(|(_, p)| p.as_deref());
                            let bounded = truncate_head_tail_lines_from_source(
                                inner,
                                full_src,
                                pre_spilled,
                                universal_cap.saturating_sub(64),
                                "developer instructions",
                                spill,
                            );
                            *text = format!(
                                "<developer_instructions>\n{bounded}\n</developer_instructions>"
                            );
                        } else {
                            *text = truncate_head_tail_lines_with_spill(
                                trimmed,
                                universal_cap,
                                "input text",
                                spill,
                            );
                        }
                    }
                }
                CanonicalPart::ToolResult { output, .. } => {
                    let cleaned = strip_billing_noise(output);
                    let raw_trimmed = cleaned.trim();
                    let scrubbed = scrub_terminal_control_noise(raw_trimmed);
                    let trimmed = scrubbed.trim();
                    if trimmed.len() > universal_cap {
                        *output = truncate_head_tail_lines_from_source(
                            trimmed,
                            raw_trimmed,
                            None,
                            universal_cap,
                            "tool output",
                            spill,
                        );
                    } else if matches!(scrubbed, std::borrow::Cow::Owned(_)) {
                        *output = trimmed.to_owned();
                    }
                }
                CanonicalPart::ToolCall {
                    arguments,
                    is_custom,
                    ..
                } => {
                    if arguments.len() > universal_cap {
                        let preview = truncate_head_tail_lines_with_spill(
                            arguments,
                            universal_cap.saturating_sub(96),
                            "tool call arguments",
                            spill,
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

    let mut blocks = render_uncompacted_prompt_blocks(&[], &stage4_work, tool_nonce);
    clamp_blocks_to_budget(&mut blocks, budget, spill);
    blocks
}

struct CompactedHistoryEntry {
    priority: u8,
    keep: bool,
    limits: PartRenderLimits,
    text: String,
}

fn render_uncompacted_archival_slice(messages: &[CanonicalMessage], tool_nonce: &str) -> String {
    let mut out = String::new();
    let mut ignored_blocks = Vec::new();
    let unlimited_archival = PartRenderLimits {
        max_text_bytes: usize::MAX,
        max_tool_call_bytes: usize::MAX,
        max_tool_result_bytes: usize::MAX,
        emit_images: false,
    };
    for msg in messages {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        let role_header = match msg.role {
            CanonicalRole::User => "### Prior User Turn\n",
            CanonicalRole::Assistant => "### Prior Assistant Turn\n",
        };
        out.push_str(role_header);
        render_parts_into(
            msg,
            tool_nonce,
            true,
            unlimited_archival,
            &mut out,
            &mut ignored_blocks,
            &mut NoopSpillSink,
        );
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn render_single_archival_entry<S: TruncationSpillSink>(
    msg: &CanonicalMessage,
    tool_nonce: &str,
    limits: PartRenderLimits,
    spill: &mut S,
) -> String {
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
        spill,
    );
    if !entry_text.ends_with('\n') {
        entry_text.push('\n');
    }
    entry_text
}

fn build_compacted_history_body<S: TruncationSpillSink>(
    history_slice: &[CanonicalMessage],
    tool_nonce: &str,
    budget: usize,
    spill: &mut S,
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
                    max_tool_call_bytes: 360,
                    max_tool_result_bytes: 440,
                    emit_images: false,
                },
            )
        } else {
            (
                0,
                PartRenderLimits {
                    max_text_bytes: 600,
                    max_tool_call_bytes: 320,
                    max_tool_result_bytes: 380,
                    emit_images: false,
                },
            )
        };

        // Phase 1 sizes entries with `NoopSpillSink` so entries dropped in the priority loop
        // do not create individual spill files before being grouped into an omitted run.
        let entry_text = render_single_archival_entry(msg, tool_nonce, limits, &mut NoopSpillSink);

        entries.push(CompactedHistoryEntry {
            priority,
            keep: true,
            limits,
            text: entry_text,
        });
    }

    let mut current_bytes: usize = entries.iter().map(|e| e.text.len()).sum();
    let omission_marker_reserve = 512usize;
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

    // Phase 2: Re-render kept entries with `spill` active so every truncated kept item saves
    // its full lossless content to disk and embeds the local file path.
    for (idx, entry) in entries.iter_mut().enumerate() {
        if !entry.keep {
            continue;
        }
        if let Some(msg) = history_slice.get(idx) {
            let old_len = entry.text.len();
            entry.text = render_single_archival_entry(msg, tool_nonce, entry.limits, spill);
            current_bytes = current_bytes
                .saturating_sub(old_len)
                .saturating_add(entry.text.len());
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
                let pre_spilled = history_slice.get(idx).and_then(|msg| {
                    let full_turn =
                        render_uncompacted_archival_slice(std::slice::from_ref(msg), tool_nonce);
                    spill.spill_truncated_content("archival turn", &full_turn)
                });
                entry.text = truncate_middle_utf8_bytes_pre_spilled(
                    &entry.text,
                    2_400,
                    13,
                    20,
                    pre_spilled.as_deref(),
                );
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
    let mut omitted_start: Option<usize> = None;
    for (idx, entry) in entries.iter().enumerate() {
        if !entry.keep {
            if omitted_start.is_none() {
                omitted_start = Some(idx);
            }
            continue;
        }
        if let Some(run_start) = omitted_start.take() {
            let omitted_run = idx.saturating_sub(run_start);
            if !assembled.is_empty() && !assembled.ends_with('\n') {
                assembled.push('\n');
            }
            append_omitted_archival_run_notice(
                &mut assembled,
                history_slice.get(run_start..idx).unwrap_or(&[]),
                omitted_run,
                tool_nonce,
                spill,
            );
        }
        assembled.push_str(&entry.text);
    }
    if let Some(run_start) = omitted_start.take() {
        let omitted_run = entries.len().saturating_sub(run_start);
        append_omitted_archival_run_notice(
            &mut assembled,
            history_slice.get(run_start..).unwrap_or(&[]),
            omitted_run,
            tool_nonce,
            spill,
        );
    }

    if assembled.len() > budget {
        let full_history = render_uncompacted_archival_slice(history_slice, tool_nonce);
        let spilled_hist =
            spill.spill_truncated_content("archival conversation history", &full_history);
        // Favor the tail (70%) so `last_user_idx` and the newest turns stay completely intact.
        truncate_middle_utf8_bytes_pre_spilled(&assembled, budget, 3, 10, spilled_hist.as_deref())
    } else {
        assembled
    }
}

fn append_omitted_archival_run_notice<S: TruncationSpillSink>(
    assembled: &mut String,
    omitted_slice: &[CanonicalMessage],
    omitted_run: usize,
    tool_nonce: &str,
    spill: &mut S,
) {
    let full_turns = render_uncompacted_archival_slice(omitted_slice, tool_nonce);
    if let Some(path) = spill.spill_truncated_content("omitted archival turns", &full_turns) {
        assembled.push_str(&format!(
            "[... omitted {omitted_run} earlier archival turns to fit context window; full uncompacted turns saved locally at `{path}` ...]\n"
        ));
    } else {
        assembled.push_str(&format!(
            "[... omitted {omitted_run} earlier archival turns to fit context window ...]\n"
        ));
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
    truncate_head_tail_lines_with_spill(input, max_bytes, kind_label, &mut NoopSpillSink)
}

/// Like [`truncate_head_tail_lines`], persisting the full un-truncated `input` to a local file
/// via `spill` and referencing its path directly inside the middle omission notice.
#[must_use]
pub fn truncate_head_tail_lines_with_spill<S: TruncationSpillSink>(
    input: &str,
    max_bytes: usize,
    kind_label: &str,
    spill: &mut S,
) -> String {
    truncate_head_tail_lines_from_source(input, input, None, max_bytes, kind_label, spill)
}

/// Strips terminal ANSI escape sequences (`CSI`, `OSC`, `DCS`, 2-byte `ESC`), backspace
/// overstrikes, and line-local carriage-return (`\r`) progress-bar overwrites while preserving
/// `\r\n` / `\r\r\n` line endings and UTF-8 integrity. Returns `Cow::Borrowed` with zero
/// allocation when no terminal control noise is present.
#[must_use]
pub fn scrub_terminal_control_noise(input: &str) -> std::borrow::Cow<'_, str> {
    if !has_terminal_control_noise(input) {
        return std::borrow::Cow::Borrowed(input);
    }

    let mut out = String::with_capacity(input.len());
    let mut line_start = 0usize;
    let mut pending_cr = false;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.peek().copied() {
                Some('[') => {
                    let _ = chars.next();
                    let mut csi_param = String::new();
                    while let Some(&b) = chars.peek() {
                        if ('\x20'..='\x3f').contains(&b) {
                            if csi_param.len() < 8 {
                                csi_param.push(b);
                            }
                            let _ = chars.next();
                        } else {
                            break;
                        }
                    }
                    if let Some(&final_ch) = chars.peek()
                        && ('\x40'..='\x7e').contains(&final_ch)
                    {
                        let _ = chars.next();
                        if final_ch == 'K'
                            && (csi_param == "2"
                                || (pending_cr && (csi_param.is_empty() || csi_param == "0")))
                        {
                            out.truncate(line_start);
                            pending_cr = false;
                        }
                    }
                }
                Some(']') => {
                    let _ = chars.next();
                    while let Some(osc_ch) = chars.next() {
                        if osc_ch == '\x07' {
                            break;
                        }
                        if osc_ch == '\x1b' && chars.peek() == Some(&'\\') {
                            let _ = chars.next();
                            break;
                        }
                    }
                }
                Some('P' | 'X' | '^' | '_') => {
                    let _ = chars.next();
                    while let Some(st_ch) = chars.next() {
                        if st_ch == '\x1b' && chars.peek() == Some(&'\\') {
                            let _ = chars.next();
                            break;
                        }
                    }
                }
                Some('(' | ')') => {
                    let _ = chars.next();
                    let _ = chars.next();
                }
                Some(esc_next) if ('\x40'..='\x5f').contains(&esc_next) => {
                    let _ = chars.next();
                }
                _ => {}
            },
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    let _ = chars.next();
                    pending_cr = false;
                    out.push('\n');
                    line_start = out.len();
                } else {
                    pending_cr = true;
                }
            }
            '\n' => {
                pending_cr = false;
                out.push('\n');
                line_start = out.len();
            }
            '\x08' => {
                if !pending_cr && out.len() > line_start {
                    let _ = out.pop();
                }
            }
            other => {
                if pending_cr {
                    out.truncate(line_start);
                    pending_cr = false;
                }
                out.push(other);
            }
        }
    }

    std::borrow::Cow::Owned(out)
}

fn has_terminal_control_noise(input: &str) -> bool {
    let mut iter = input.as_bytes().iter().copied().peekable();
    while let Some(b) = iter.next() {
        if b == 0x1b || b == 0x08 {
            return true;
        }
        if b == b'\r' && iter.peek().copied() != Some(b'\n') {
            return true;
        }
    }
    false
}

fn contains_ascii_case_insensitive(haystack: &str, needle_lower: &str) -> bool {
    let n_bytes = needle_lower.as_bytes();
    if n_bytes.is_empty() {
        return true;
    }
    let h_bytes = haystack.as_bytes();
    if h_bytes.len() < n_bytes.len() {
        return false;
    }
    h_bytes
        .windows(n_bytes.len())
        .any(|win| win.eq_ignore_ascii_case(n_bytes))
}

fn is_diagnostic_noise_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }
    if contains_ascii_case_insensitive(trimmed, "[error] to see the full stack trace")
        || contains_ascii_case_insensitive(trimmed, "[error] re-run maven")
        || contains_ascii_case_insensitive(trimmed, "[error] for more information")
        || contains_ascii_case_insensitive(trimmed, "-> [help 1]")
    {
        return true;
    }
    let is_json_info = trimmed.starts_with('{')
        && (contains_ascii_case_insensitive(trimmed, "\"level\":10")
            || contains_ascii_case_insensitive(trimmed, "\"level\":20")
            || contains_ascii_case_insensitive(trimmed, "\"level\":30")
            || contains_ascii_case_insensitive(trimmed, "\"level\":\"info\"")
            || contains_ascii_case_insensitive(trimmed, "\"level\":\"debug\"")
            || contains_ascii_case_insensitive(trimmed, "\"level\":\"trace\""));
    if is_json_info {
        return !contains_ascii_case_insensitive(trimmed, "error")
            && !contains_ascii_case_insensitive(trimmed, "exception")
            && !contains_ascii_case_insensitive(trimmed, "fatal");
    }
    false
}

fn classify_diagnostic_context_lines(line: &str) -> Option<usize> {
    if is_diagnostic_noise_line(line) {
        return None;
    }
    let trimmed = line.trim();
    const ROOT_CAUSE_MARKERS: &[&str] = &[
        "caused by:",
        "panicked at",
        "application failed to start",
        "compilation error",
        "traceback (most recent call last)",
        "failures:",
        "failed tests",
    ];
    if ROOT_CAUSE_MARKERS
        .iter()
        .any(|m| contains_ascii_case_insensitive(trimmed, m))
    {
        return Some(5);
    }

    const ERROR_MARKERS: &[&str] = &[
        "error:",
        "error[",
        "fatal:",
        "fatal error:",
        "npm err!",
        "--- fail:",
        "[  failed  ]",
        "build failed",
        "compilation failed",
        "undefined reference to",
        "assertion failed",
        "assertionerror",
        "syntaxerror:",
        "typeerror:",
        "referenceerror:",
    ];
    if ERROR_MARKERS
        .iter()
        .any(|m| contains_ascii_case_insensitive(trimmed, m))
    {
        return Some(3);
    }
    if (contains_ascii_case_insensitive(trimmed, "test ")
        && trimmed
            .get(trimmed.len().saturating_sub(10)..)
            .is_some_and(|tail| contains_ascii_case_insensitive(tail, "... failed")))
        || trimmed
            .get(..7)
            .is_some_and(|head| head.eq_ignore_ascii_case("failed "))
    {
        return Some(3);
    }
    None
}

/// Scans an omitted middle slice of a tool output for compiler/test/runtime failure root causes,
/// returning a bounded line-numbered excerpt when high-signal diagnostics would otherwise be lost.
fn extract_omitted_diagnostic_excerpt(
    omitted_slice: &str,
    first_omitted_line: usize,
    max_excerpt_bytes: usize,
) -> Option<String> {
    if max_excerpt_bytes < 160 {
        return None;
    }
    let lines: Vec<&str> = omitted_slice.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let mut keep = vec![false; lines.len()];
    let mut matched_any = false;
    for (idx, &line) in lines.iter().enumerate() {
        if let Some(after) = classify_diagnostic_context_lines(line) {
            matched_any = true;
            let from = idx.saturating_sub(1);
            let to = (idx + after).min(lines.len().saturating_sub(1));
            for slot in keep.get_mut(from..=to).into_iter().flatten() {
                *slot = true;
            }
        }
    }
    if !matched_any {
        return None;
    }

    let mut out = String::with_capacity(max_excerpt_bytes.min(1_024));
    let mut prev_idx: Option<usize> = None;
    for (idx, (&is_kept, &raw_line)) in keep.iter().zip(lines.iter()).enumerate() {
        if !is_kept || is_diagnostic_noise_line(raw_line) {
            continue;
        }
        if let Some(prev) = prev_idx
            && idx > prev + 1
        {
            let gap = "  ...\n";
            if out.len() + gap.len() > max_excerpt_bytes {
                break;
            }
            out.push_str(gap);
        }
        let line_no = first_omitted_line + idx;
        let trimmed = raw_line.trim_end();
        let clamped = if trimmed.len() > 260 {
            let cut = floor_utf8_boundary(trimmed, 257);
            format!("{}...", trimmed.get(..cut).unwrap_or(""))
        } else {
            trimmed.to_owned()
        };
        let formatted = format!("  L{line_no}: {clamped}\n");
        if out.len() + formatted.len() > max_excerpt_bytes {
            break;
        }
        out.push_str(&formatted);
        prev_idx = Some(idx);
    }

    if out.is_empty() { None } else { Some(out) }
}

fn slice_head_tail_on_lines(input: &str, usable: usize) -> (&str, &str, &str) {
    let total_bytes = input.len();
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
    (head, omitted_slice, tail)
}

fn truncate_head_tail_lines_from_source<S: TruncationSpillSink>(
    input: &str,
    full_source_for_spill: &str,
    pre_spilled_path: Option<&str>,
    max_bytes: usize,
    kind_label: &str,
    spill: &mut S,
) -> String {
    if input.len() <= max_bytes {
        return input.to_owned();
    }
    let spilled_path = pre_spilled_path
        .map(str::to_owned)
        .or_else(|| spill.spill_truncated_content(kind_label, full_source_for_spill));

    let path_extra = spilled_path.as_deref().map_or(0, |p| p.len() * 2 + 160);
    let marker_reserve = 380usize.saturating_add(path_extra);
    if max_bytes <= marker_reserve.saturating_add(64) {
        return truncate_middle_utf8_bytes_pre_spilled(
            input,
            max_bytes,
            11,
            20,
            spilled_path.as_deref(),
        );
    }

    let total_bytes = input.len();
    let usable = max_bytes.saturating_sub(marker_reserve);
    let (mut head, mut omitted_slice, mut tail) = slice_head_tail_on_lines(input, usable);

    // For oversized tool outputs, rescue buried compiler/test failure lines from the omitted
    // middle so the model sees root-cause diagnostics inline without an extra file-read turn.
    let mut diag_excerpt: Option<String> = None;
    if matches!(kind_label, "tool output" | "archival tool output")
        && usable >= 2_400
        && omitted_slice.contains('\n')
    {
        let probe_first_line = if head.is_empty() {
            1
        } else {
            head.bytes().filter(|&b| b == b'\n').count() + 2
        };
        let diag_cap = (usable / 5).min(4_096);
        if extract_omitted_diagnostic_excerpt(omitted_slice, probe_first_line, diag_cap).is_some() {
            let shrunk_usable = usable.saturating_sub(diag_cap.saturating_add(96));
            let (h2, om2, t2) = slice_head_tail_on_lines(input, shrunk_usable);
            let first_line_2 = if h2.is_empty() {
                1
            } else {
                h2.bytes().filter(|&b| b == b'\n').count() + 2
            };
            if let Some(excerpt) = extract_omitted_diagnostic_excerpt(om2, first_line_2, diag_cap) {
                head = h2;
                omitted_slice = om2;
                tail = t2;
                diag_excerpt = Some(excerpt);
            }
        }
    }

    let omitted_bytes = omitted_slice.len();

    let mut notice = if omitted_slice.contains('\n') {
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
        if let Some(path) = &spilled_path {
            format!(
                "\n... [Gateway truncated {kind_label}: {total_bytes} bytes ({total_lines} lines) total; omitted middle {omitted_bytes} bytes ({omitted_lines} lines, lines {first_omitted}..={last_omitted}) to fit single-turn limit. Full lossless content saved locally at `{path}` — beginning and end are preserved intact; inspect `{path}` (e.g. lines {first_omitted}..={last_omitted}) if the omitted middle section is needed.] ...\n"
            )
        } else {
            format!(
                "\n... [Gateway truncated {kind_label}: {total_bytes} bytes ({total_lines} lines) total; omitted middle {omitted_bytes} bytes ({omitted_lines} lines, lines {first_omitted}..={last_omitted}) to fit single-turn limit. Beginning and end are preserved intact; query that specific line range or filter output if the middle section is needed.] ...\n"
            )
        }
    } else if let Some(path) = &spilled_path {
        format!(
            "\n... [Gateway truncated {kind_label}: {total_bytes} bytes total; omitted middle {omitted_bytes} bytes to fit single-turn limit. Full lossless content saved locally at `{path}` — beginning and end are preserved intact; inspect `{path}` if the omitted middle section is needed.] ...\n"
        )
    } else {
        format!(
            "\n... [Gateway truncated {kind_label}: {total_bytes} bytes total; omitted middle {omitted_bytes} bytes to fit single-turn limit. Beginning and end are preserved intact.] ...\n"
        )
    };
    if let Some(excerpt) = diag_excerpt {
        notice.push_str("[Diagnostics extracted from omitted middle:]\n");
        notice.push_str(&excerpt);
        notice.push_str("[End extracted diagnostics]\n");
    }

    let mut out = String::with_capacity(head.len() + notice.len() + tail.len());
    out.push_str(head);
    out.push_str(&notice);
    out.push_str(tail);
    if out.len() > max_bytes {
        truncate_middle_utf8_bytes_pre_spilled(input, max_bytes, 11, 20, spilled_path.as_deref())
    } else {
        out
    }
}

fn truncate_middle_utf8_bytes_with_spill<S: TruncationSpillSink>(
    input: &str,
    max_bytes: usize,
    head_num: usize,
    denom: usize,
    kind_label: &str,
    spill: &mut S,
) -> String {
    if input.len() <= max_bytes {
        return input.to_owned();
    }
    let spilled = spill.spill_truncated_content(kind_label, input);
    truncate_middle_utf8_bytes_pre_spilled(input, max_bytes, head_num, denom, spilled.as_deref())
}

fn truncate_middle_utf8_bytes_pre_spilled(
    input: &str,
    max_bytes: usize,
    head_num: usize,
    denom: usize,
    spilled_path: Option<&str>,
) -> String {
    if input.len() <= max_bytes {
        return input.to_owned();
    }

    if let Some(path) = spilled_path {
        let full_marker_overhead = path.len().saturating_add(74);
        if max_bytes >= full_marker_overhead.saturating_add(16) {
            let usable = max_bytes.saturating_sub(full_marker_overhead);
            let safe_denom = denom.max(1);
            let head_budget = usable.saturating_mul(head_num.min(safe_denom)) / safe_denom;
            let tail_budget = usable.saturating_sub(head_budget);

            let head_end = floor_utf8_boundary(input, head_budget);
            let tail_start =
                ceil_utf8_boundary(input, input.len().saturating_sub(tail_budget)).max(head_end);
            let head = input.get(..head_end).unwrap_or("").trim_end();
            let tail = input.get(tail_start..).unwrap_or("").trim_start();
            let omitted_bytes = tail_start.saturating_sub(head_end);
            let marker = format!(
                "\n...[omitted {omitted_bytes} bytes; full content saved at `{path}`]...\n"
            );
            let mut out = String::with_capacity(head.len() + marker.len() + tail.len());
            out.push_str(head);
            out.push_str(&marker);
            out.push_str(tail);
            return out;
        }
        let compact_notice = format!("...[omitted; full: `{path}`]");
        if max_bytes >= compact_notice.len() {
            let head_end =
                floor_utf8_boundary(input, max_bytes.saturating_sub(compact_notice.len()));
            let head = input.get(..head_end).unwrap_or("").trim_end();
            return format!("{head}{compact_notice}");
        }
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

fn render_parts_into<S: TruncationSpillSink>(
    msg: &CanonicalMessage,
    tool_nonce: &str,
    is_archival: bool,
    limits: PartRenderLimits,
    text_acc: &mut String,
    blocks: &mut Vec<AcpPromptBlock>,
    spill: &mut S,
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
                        let compacted = truncate_middle_utf8_bytes_with_spill(
                            &cleaned,
                            limits.max_text_bytes,
                            13,
                            20,
                            "archival message text",
                            spill,
                        );
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
                        truncate_middle_utf8_bytes_with_spill(
                            trimmed_args,
                            limits.max_tool_call_bytes,
                            13,
                            20,
                            "archival tool call arguments",
                            spill,
                        )
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
                let raw_trimmed = cleaned_output.trim();
                let scrubbed = scrub_terminal_control_noise(raw_trimmed);
                let trimmed_output = scrubbed.trim();
                let rendered_output = if trimmed_output.len() > limits.max_tool_result_bytes {
                    let spilled =
                        spill.spill_truncated_content("archival tool output", raw_trimmed);
                    truncate_middle_utf8_bytes_pre_spilled(
                        trimmed_output,
                        limits.max_tool_result_bytes,
                        13,
                        20,
                        spilled.as_deref(),
                    )
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
        assert!(
            repl_append.contains("`edit(path, old, new)`, `create(path, content)`, `view(path)`")
        );
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
        let blocks = render_smart_bounded_active_slice(
            std::slice::from_ref(&msg),
            "1122",
            12_000,
            &mut sink,
        );
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
}
