//! Bounded ACP prompt block planning, waterfill budgeting, and archival history compaction.

use crate::acp::AcpPromptBlock;
use crate::canonical::{CanonicalMessage, CanonicalPart, CanonicalRole};
use crate::error::ProtocolError;

use super::spill::{
    NoopSpillSink, TruncationSpillSink, scrub_terminal_control_noise,
    truncate_head_tail_lines_from_source, truncate_head_tail_lines_with_spill,
    truncate_middle_utf8_bytes_pre_spilled, truncate_middle_utf8_bytes_with_spill,
};
use super::system_append::MAX_SYSTEM_APPEND_BYTES;
use super::{
    is_developer_instructions_wrapper, strip_billing_noise, strip_tag_blocks,
    strip_unstable_harness_noise,
};

/// Maximum UTF-8 text budget across all blocks of a single ACP `session/prompt` RPC.
///
/// Empirical canary probes on `agy acp 1.5.18` proved that `session/prompt` concatenates all
/// `Text` blocks and enforces a hard **tail-truncation** cliff at `191,579` bytes (`<truncated N bytes>`),
/// which would silently sever the trailing user prompt on cold replays. Keeping our middle-waterfill
/// budget at `176,000` bytes leaves `~15.5 KB` of safety margin while preserving both head and tail.
pub const MAX_ACP_PROMPT_TEXT_BYTES: usize = 176_000;

/// Progressive schema-compaction ceiling for spilled or incremental `<client_tools_update>` blocks.
pub(super) const MAX_TOOL_DIRECTIVE_BYTES: usize = 48_000;

const MAX_ACTIVE_WITH_HISTORY_TEXT_BYTES: usize = 76_000;
const MAX_HISTORY_RESERVE_BYTES: usize = 48_000;
const RECENT_TAIL_HISTORY_MESSAGES: usize = 28;

const _: () = {
    assert!(
        MAX_SYSTEM_APPEND_BYTES * 3 < MAX_ACP_PROMPT_TEXT_BYTES,
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

#[derive(Clone, Copy)]
struct PartRenderMode {
    is_archival: bool,
    strip_billing: bool,
}

impl PartRenderLimits {
    const UNBOUNDED: Self = Self {
        max_text_bytes: usize::MAX,
        max_tool_call_bytes: usize::MAX,
        max_tool_result_bytes: usize::MAX,
        emit_images: true,
    };
}

fn maybe_strip_billing(input: &str, strip_billing: bool) -> std::borrow::Cow<'_, str> {
    if strip_billing {
        std::borrow::Cow::Owned(strip_billing_noise(input))
    } else {
        std::borrow::Cow::Borrowed(input)
    }
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
        let uncompacted =
            render_uncompacted_prompt_blocks(history_slice, active_slice, tool_nonce, true);
        if total_text_block_bytes(&uncompacted) <= MAX_ACP_PROMPT_TEXT_BYTES {
            return sanitize_slash_command_blocks(uncompacted);
        }
    }

    sanitize_slash_command_blocks(render_bounded_prompt_blocks(
        history_slice,
        active_slice,
        tool_nonce,
        MAX_ACP_PROMPT_TEXT_BYTES,
        true,
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
    build_acp_prompt_plan_with_options(
        messages,
        tool_nonce,
        is_full_history_replay,
        leading_directive_xml,
        true,
        spill,
    )
}

/// Like [`build_acp_prompt_plan_with_spill`], controlling whether non-semantic billing noise
/// is stripped (`strip_billing = true`) or preserved verbatim (`strip_billing = false`).
pub fn build_acp_prompt_plan_with_options<S: TruncationSpillSink>(
    messages: &[CanonicalMessage],
    tool_nonce: &str,
    is_full_history_replay: bool,
    leading_directive_xml: Option<&str>,
    strip_billing: bool,
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
        render_smart_bounded_active_slice(active_slice, tool_nonce, budget, strip_billing, spill)
    } else {
        let uncompacted = render_uncompacted_prompt_blocks(
            history_slice,
            active_slice,
            tool_nonce,
            strip_billing,
        );
        if total_text_block_bytes(&uncompacted) <= budget {
            uncompacted
        } else {
            let hist_reserve = estimate_raw_prompt_bytes(history_slice, &[])
                .min(budget / 3)
                .min(MAX_HISTORY_RESERVE_BYTES);
            let active_budget = budget.saturating_sub(hist_reserve);
            let active = render_smart_bounded_active_slice(
                active_slice,
                tool_nonce,
                active_budget,
                strip_billing,
                spill,
            );
            render_history_around_active(
                history_slice,
                active,
                tool_nonce,
                budget,
                strip_billing,
                spill,
            )
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
    strip_billing: bool,
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
                PartRenderMode {
                    is_archival: true,
                    strip_billing,
                },
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
            PartRenderMode {
                is_archival: false,
                strip_billing,
            },
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
    strip_billing: bool,
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
    let active_blocks = render_smart_bounded_active_slice(
        active_slice,
        tool_nonce,
        active_budget,
        strip_billing,
        spill,
    );
    render_history_around_active(
        history_slice,
        active_blocks,
        tool_nonce,
        max_bytes,
        strip_billing,
        spill,
    )
}

fn render_history_around_active<S: TruncationSpillSink>(
    history_slice: &[CanonicalMessage],
    active_blocks: Vec<AcpPromptBlock>,
    tool_nonce: &str,
    max_bytes: usize,
    strip_billing: bool,
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

    let history_body = build_compacted_history_body(
        history_slice,
        tool_nonce,
        history_budget,
        strip_billing,
        spill,
    );

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
pub(super) fn render_smart_bounded_active_slice<S: TruncationSpillSink>(
    active_slice: &[CanonicalMessage],
    tool_nonce: &str,
    budget: usize,
    strip_billing: bool,
    spill: &mut S,
) -> Vec<AcpPromptBlock> {
    let uncompacted =
        render_uncompacted_prompt_blocks(&[], active_slice, tool_nonce, strip_billing);
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
    let stage1_blocks = render_uncompacted_prompt_blocks(&[], &work, tool_nonce, strip_billing);
    if total_text_block_bytes(&stage1_blocks) <= budget {
        return stage1_blocks;
    }

    // Stage 2: Water-fill middle-truncate ONLY oversized `ToolResult` outputs if non-result parts fit.
    let mut result_lens = Vec::new();
    let mut empty_results_probe = work.clone();
    for msg in &mut empty_results_probe {
        for part in &mut msg.parts {
            if let CanonicalPart::ToolResult { output, .. } = part {
                let cleaned = maybe_strip_billing(output, strip_billing);
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
            strip_billing,
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
                        let cleaned = maybe_strip_billing(output, strip_billing);
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
            let stage2_blocks =
                render_uncompacted_prompt_blocks(&[], &stage2_msgs, tool_nonce, strip_billing);
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
            strip_billing,
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
                let stage3_blocks =
                    render_uncompacted_prompt_blocks(&[], &stage3_msgs, tool_nonce, strip_billing);
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
        let is_assistant = msg.role == CanonicalRole::Assistant;
        for part in &msg.parts {
            match part {
                CanonicalPart::Text(t) => {
                    let cleaned = maybe_strip_billing(t, strip_billing);
                    let trimmed = cleaned.trim();
                    if !trimmed.is_empty() {
                        if is_assistant {
                            fixed_overhead = fixed_overhead.saturating_add(trimmed.len() + 16);
                        } else {
                            variable_lens.push(trimmed.len());
                            fixed_overhead = fixed_overhead.saturating_add(16);
                        }
                    }
                }
                CanonicalPart::ToolResult {
                    call_id, output, ..
                } => {
                    let cleaned = maybe_strip_billing(output, strip_billing);
                    let scrubbed = scrub_terminal_control_noise(cleaned.trim());
                    variable_lens.push(scrubbed.trim().len());
                    fixed_overhead = fixed_overhead.saturating_add(call_id.len() + 80);
                }
                CanonicalPart::ToolCall {
                    call_id,
                    name,
                    arguments,
                    is_custom,
                } => {
                    // Never waterfill-truncate real assistant tool calls (`ToolCall` outputs):
                    // reserve their full JSON-encoded wire length in `fixed_overhead` so caller
                    // inputs/results absorb compaction without corrupting tool-call payloads.
                    let encoded_args_len = if *is_custom {
                        serde_json::to_string(arguments).map_or(arguments.len(), |s| s.len())
                    } else {
                        arguments.len()
                    };
                    fixed_overhead = fixed_overhead.saturating_add(
                        call_id.len() + name.len() + encoded_args_len + tool_nonce.len() * 2 + 96,
                    );
                }
                CanonicalPart::ImageBase64 { .. } | CanonicalPart::Reasoning(_) => {}
            }
        }
    }

    let available_var = budget.saturating_sub(fixed_overhead);
    let universal_cap = compute_waterfill_cap(&variable_lens, available_var).max(512);
    for (msg_idx, msg) in stage4_work.iter_mut().enumerate() {
        let is_assistant = msg.role == CanonicalRole::Assistant;
        for (part_idx, part) in msg.parts.iter_mut().enumerate() {
            match part {
                CanonicalPart::Text(text) => {
                    if is_assistant {
                        continue;
                    }
                    let cleaned = maybe_strip_billing(text, strip_billing);
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
                    let cleaned = maybe_strip_billing(output, strip_billing);
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
                CanonicalPart::ToolCall { .. }
                | CanonicalPart::ImageBase64 { .. }
                | CanonicalPart::Reasoning(_) => {}
            }
        }
    }

    let mut blocks = render_uncompacted_prompt_blocks(&[], &stage4_work, tool_nonce, strip_billing);
    clamp_blocks_to_budget(&mut blocks, budget, spill);
    blocks
}

struct CompactedHistoryEntry {
    priority: u8,
    keep: bool,
    limits: PartRenderLimits,
    text: String,
}

fn render_uncompacted_archival_slice(
    messages: &[CanonicalMessage],
    tool_nonce: &str,
    strip_billing: bool,
) -> String {
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
            PartRenderMode {
                is_archival: true,
                strip_billing,
            },
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
    strip_billing: bool,
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
        PartRenderMode {
            is_archival: true,
            strip_billing,
        },
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
    strip_billing: bool,
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
        } else if idx >= history_slice.len().saturating_sub(4) && budget >= 48_000 {
            (
                2,
                PartRenderLimits {
                    max_text_bytes: 12_000,
                    max_tool_call_bytes: 24_000,
                    max_tool_result_bytes: 6_000,
                    emit_images: false,
                },
            )
        } else if idx >= recent_tail_start {
            (
                2,
                PartRenderLimits {
                    max_text_bytes: 4_000,
                    max_tool_call_bytes: 6_000,
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
        let entry_text = render_single_archival_entry(
            msg,
            tool_nonce,
            limits,
            strip_billing,
            &mut NoopSpillSink,
        );

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
            entry.text =
                render_single_archival_entry(msg, tool_nonce, entry.limits, strip_billing, spill);
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
                    let full_turn = render_uncompacted_archival_slice(
                        std::slice::from_ref(msg),
                        tool_nonce,
                        strip_billing,
                    );
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
                strip_billing,
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
            strip_billing,
            spill,
        );
    }

    if assembled.len() > budget {
        let full_history =
            render_uncompacted_archival_slice(history_slice, tool_nonce, strip_billing);
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
    strip_billing: bool,
    spill: &mut S,
) {
    let full_turns = render_uncompacted_archival_slice(omitted_slice, tool_nonce, strip_billing);
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

pub(super) fn total_text_block_bytes(blocks: &[AcpPromptBlock]) -> usize {
    blocks
        .iter()
        .map(|b| match b {
            AcpPromptBlock::Text { text } => text.len(),
            AcpPromptBlock::Image { .. } => 0,
        })
        .sum()
}

fn render_parts_into<S: TruncationSpillSink>(
    msg: &CanonicalMessage,
    tool_nonce: &str,
    mode: PartRenderMode,
    limits: PartRenderLimits,
    text_acc: &mut String,
    blocks: &mut Vec<AcpPromptBlock>,
    spill: &mut S,
) {
    for part in &msg.parts {
        match part {
            CanonicalPart::Text(text) => {
                let cleaned = maybe_strip_billing(text, mode.strip_billing);
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
                if mode.is_archival {
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
                let cleaned_output = maybe_strip_billing(output, mode.strip_billing);
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
