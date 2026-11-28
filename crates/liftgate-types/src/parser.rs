use std::fmt::Write as _;
use std::mem;

use serde::Deserialize;
use serde_json::value::RawValue;
use thiserror::Error;

/// Parsed tool invocation extracted from an `<agy_tool_call_{nonce}>` block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedToolCall {
    pub name: String,
    /// Canonical JSON object string for function/tool_use calls, or raw string for custom tools.
    pub arguments: String,
    /// True if payload provided raw non-JSON or string `input` for a custom tool.
    pub is_raw_input: bool,
}

/// Incremental item emitted as `agent_message_chunk` text arrives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParsedStreamItem {
    TextDelta(String),
    /// Emitted once inside `<agy_tool_call_{nonce}>` as soon as a valid tool header is recognized,
    /// before the closing `</agy_tool_call_{nonce}>` arrives.
    ToolCallStart {
        name: String,
        /// `true` when the header commits to raw/freeform string input (`"input": "` or bare
        /// `*** Begin Patch` / `// @exec:`), enabling live `ToolCallDelta` chunks.
        is_raw_input: bool,
    },
    /// Incremental decoded raw-input slice for an active `is_raw_input == true` tool call.
    /// Concatenating all `ToolCallDelta` chunks for a call equals the final `ParsedToolCall.arguments`.
    ToolCallDelta(String),
    ToolCall(ParsedToolCall),
    /// Emitted if an already-started tool call (`ToolCallStart`) is truncated or corrupted before
    /// a valid tool payload completes.
    ToolCallAborted {
        name: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParserMode {
    Text,
    /// Saw `<agy_tool_call_{nonce}` without its closing `>` yet (e.g. optional XML attributes).
    InOpenTagHeader,
    InTag,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum InTagProgress {
    #[default]
    ProbingHeader,
    WaitUntilClose,
    StartedBuffered {
        name: String,
    },
    StreamingQuotedInput {
        name: String,
        raw_cursor: usize,
        emitted_decoded_bytes: usize,
    },
    StreamingBareBody {
        name: String,
        raw_cursor: usize,
        emitted_decoded_bytes: usize,
    },
}

impl InTagProgress {
    fn started_name(&self) -> Option<&str> {
        match self {
            Self::ProbingHeader | Self::WaitUntilClose => None,
            Self::StartedBuffered { name }
            | Self::StreamingQuotedInput { name, .. }
            | Self::StreamingBareBody { name, .. } => Some(name.as_str()),
        }
    }

    fn emitted_decoded_bytes(&self) -> Option<usize> {
        match self {
            Self::StreamingQuotedInput {
                emitted_decoded_bytes,
                ..
            }
            | Self::StreamingBareBody {
                emitted_decoded_bytes,
                ..
            } => Some(*emitted_decoded_bytes),
            Self::ProbingHeader | Self::WaitUntilClose | Self::StartedBuffered { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClosingWrapper {
    InlineBacktick,
    CodeFence,
}

/// Resumable streaming parser that separates visible assistant text from
/// `<agy_tool_call_{nonce}...>...</agy_tool_call_{nonce}>` blocks without re-allocating per frame.
#[derive(Debug)]
pub struct IncrementalToolParser {
    open_prefix: String,
    open_tag: String,
    close_tag: String,
    enabled: bool,
    mode: ParserMode,
    in_tag_progress: InTagProgress,
    buffer: String,
    /// Tracks whether an odd number of single-backtick spans is currently open on the active line.
    line_single_backtick_open: bool,
    /// Markdown backtick or fence opener captured immediately before `<agy_tool_call_{nonce}>`.
    /// Discarded if the tag parses as a valid tool call; restored to text otherwise.
    pending_open_wrapper: String,
    /// Matching closing backtick or fence to strip immediately after a valid `ToolCall`.
    strip_closing_wrapper: Option<ClosingWrapper>,
}

impl IncrementalToolParser {
    #[must_use]
    pub fn new(nonce: &str, enabled: bool) -> Self {
        Self {
            open_prefix: format!("<agy_tool_call_{nonce}"),
            open_tag: format!("<agy_tool_call_{nonce}>"),
            close_tag: format!("</agy_tool_call_{nonce}>"),
            enabled,
            mode: ParserMode::Text,
            in_tag_progress: InTagProgress::ProbingHeader,
            buffer: String::new(),
            line_single_backtick_open: false,
            pending_open_wrapper: String::new(),
            strip_closing_wrapper: None,
        }
    }

    #[must_use]
    pub fn open_tag(&self) -> &str {
        &self.open_tag
    }

    #[must_use]
    pub fn close_tag(&self) -> &str {
        &self.close_tag
    }

    /// Feeds a new UTF-8 text chunk and appends any decoded items into `out`.
    pub fn feed(&mut self, chunk: &str, out: &mut Vec<ParsedStreamItem>) {
        if chunk.is_empty() {
            return;
        }
        if !self.enabled {
            out.push(ParsedStreamItem::TextDelta(chunk.to_owned()));
            return;
        }

        self.buffer.push_str(chunk);
        self.drain_complete_segments(out);
    }

    /// Flushes any buffered trailing text when the ACP turn finishes.
    pub fn finish(&mut self, out: &mut Vec<ParsedStreamItem>) {
        if self.strip_closing_wrapper.is_some() {
            let _ = self.consume_closing_wrapper(true);
        }
        if self.buffer.is_empty()
            && self.pending_open_wrapper.is_empty()
            && self.mode == ParserMode::Text
        {
            return;
        }
        let leftover = mem::take(&mut self.buffer);
        let pending_wrapper = mem::take(&mut self.pending_open_wrapper);
        match self.mode {
            ParserMode::Text => {
                if !pending_wrapper.is_empty() || !leftover.is_empty() {
                    let mut combined =
                        String::with_capacity(pending_wrapper.len() + leftover.len());
                    combined.push_str(&pending_wrapper);
                    combined.push_str(&leftover);
                    self.emit_text_delta(combined, out);
                }
            }
            ParserMode::InOpenTagHeader => {
                self.mode = ParserMode::Text;
                let mut raw = String::with_capacity(
                    pending_wrapper.len() + self.open_prefix.len() + leftover.len(),
                );
                raw.push_str(&pending_wrapper);
                raw.push_str(&self.open_prefix);
                raw.push_str(&leftover);
                self.emit_text_delta(raw, out);
            }
            ParserMode::InTag => {
                self.mode = ParserMode::Text;
                let progress = mem::take(&mut self.in_tag_progress);
                let allow_patch_rewrite = progress.emitted_decoded_bytes().is_none_or(|n| n == 0);
                if let Some(mut call) = parse_tool_call_body(&leftover) {
                    if allow_patch_rewrite && call.name == "apply_patch" {
                        call.arguments = normalize_apply_patch_text(&call.arguments);
                        call.is_raw_input = true;
                    }
                    if let Some(streamed_bytes) = progress.emitted_decoded_bytes()
                        && let Some(rem) = call.arguments.get(streamed_bytes..)
                        && !rem.is_empty()
                    {
                        out.push(ParsedStreamItem::ToolCallDelta(rem.to_owned()));
                    }
                    out.push(ParsedStreamItem::ToolCall(call));
                } else if let Some(started_name) = progress.started_name() {
                    out.push(ParsedStreamItem::ToolCallAborted {
                        name: started_name.to_owned(),
                    });
                } else {
                    let mut raw = String::with_capacity(
                        pending_wrapper.len() + self.open_tag.len() + leftover.len(),
                    );
                    raw.push_str(&pending_wrapper);
                    raw.push_str(&self.open_tag);
                    raw.push_str(&leftover);
                    self.emit_text_delta(raw, out);
                }
            }
        }
    }

    fn emit_text_delta(&mut self, text: String, out: &mut Vec<ParsedStreamItem>) {
        if text.is_empty() {
            return;
        }
        advance_line_backtick_state(&mut self.line_single_backtick_open, &text);
        out.push(ParsedStreamItem::TextDelta(text));
    }

    /// Consumes a matching closing markdown wrapper immediately following `</agy_tool_call_{nonce}>`.
    /// Returns `true` if more bytes are needed before deciding; `false` once resolved.
    fn consume_closing_wrapper(&mut self, at_eof: bool) -> bool {
        let Some(kind) = self.strip_closing_wrapper else {
            return false;
        };
        match kind {
            ClosingWrapper::InlineBacktick => {
                if self.buffer.is_empty() {
                    if at_eof {
                        self.strip_closing_wrapper = None;
                        return false;
                    }
                    return true;
                }
                if self.buffer.starts_with('`')
                    && !self.buffer.starts_with("```")
                    && let Some((_, rest)) = self.buffer.split_at_checked(1)
                {
                    let owned = rest.to_owned();
                    self.buffer.clear();
                    self.buffer.push_str(&owned);
                }
                self.strip_closing_wrapper = None;
                false
            }
            ClosingWrapper::CodeFence => {
                let ws_len = self
                    .buffer
                    .bytes()
                    .take_while(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
                    .count();
                let Some((_, after_ws)) = self.buffer.split_at_checked(ws_len) else {
                    self.strip_closing_wrapper = None;
                    return false;
                };
                if after_ws.is_empty() || (after_ws == "`" || after_ws == "``") {
                    if at_eof || ws_len > 8 {
                        self.strip_closing_wrapper = None;
                        return false;
                    }
                    return true;
                }
                if let Some(after_fence) = after_ws.strip_prefix("```") {
                    let rest = after_fence
                        .strip_prefix("\r\n")
                        .or_else(|| after_fence.strip_prefix('\n'))
                        .unwrap_or(after_fence);
                    let owned = rest.to_owned();
                    self.buffer.clear();
                    self.buffer.push_str(&owned);
                }
                self.strip_closing_wrapper = None;
                false
            }
        }
    }

    fn emit_prefix_before_tag(&mut self, before: &str, out: &mut Vec<ParsedStreamItem>) {
        self.pending_open_wrapper.clear();
        if before.is_empty() {
            return;
        }
        if let Some((clean_before, wrapper)) =
            split_trailing_code_wrapper(before, self.line_single_backtick_open)
        {
            if !clean_before.is_empty() {
                self.emit_text_delta(clean_before.to_owned(), out);
            }
            self.pending_open_wrapper.push_str(wrapper);
        } else {
            self.emit_text_delta(before.to_owned(), out);
        }
    }

    fn drain_complete_segments(&mut self, out: &mut Vec<ParsedStreamItem>) {
        loop {
            match self.mode {
                ParserMode::Text => {
                    if self.strip_closing_wrapper.is_some() && self.consume_closing_wrapper(false) {
                        break;
                    }
                    if let Some(tag_pos) = self.buffer.find(&self.open_prefix) {
                        let Some((before, rest_with_prefix)) =
                            self.buffer.split_at_checked(tag_pos)
                        else {
                            break;
                        };
                        let Some((_, after_prefix)) =
                            rest_with_prefix.split_at_checked(self.open_prefix.len())
                        else {
                            break;
                        };

                        // Verify boundary: either `>`, whitespace (attributes), or still arriving
                        let Some(next_ch) = after_prefix.chars().next() else {
                            if !before.is_empty() {
                                let before_owned = before.to_owned();
                                let keep = rest_with_prefix.to_owned();
                                self.emit_prefix_before_tag(&before_owned, out);
                                self.buffer.clear();
                                if !self.pending_open_wrapper.is_empty() {
                                    let pw = mem::take(&mut self.pending_open_wrapper);
                                    self.buffer.push_str(&pw);
                                }
                                self.buffer.push_str(&keep);
                            }
                            break;
                        };

                        if next_ch == '>' {
                            let before_owned = before.to_owned();
                            let Some((_, remainder)) = after_prefix.split_at_checked(1) else {
                                break;
                            };
                            let owned_rem = remainder.to_owned();
                            self.emit_prefix_before_tag(&before_owned, out);
                            self.buffer.clear();
                            self.buffer.push_str(&owned_rem);
                            self.mode = ParserMode::InTag;
                            self.in_tag_progress = InTagProgress::ProbingHeader;
                        } else if next_ch.is_ascii_whitespace() {
                            let before_owned = before.to_owned();
                            let owned_after = after_prefix.to_owned();
                            self.emit_prefix_before_tag(&before_owned, out);
                            self.buffer.clear();
                            self.buffer.push_str(&owned_after);
                            self.mode = ParserMode::InOpenTagHeader;
                        } else {
                            // False prefix (e.g. `<agy_tool_call_nonce_other`): emit through prefix
                            let emit_end = tag_pos + self.open_prefix.len();
                            let Some((emit_part, rem)) = self.buffer.split_at_checked(emit_end)
                            else {
                                break;
                            };
                            let emit_owned = emit_part.to_owned();
                            let owned_rem = rem.to_owned();
                            self.buffer.clear();
                            self.buffer.push_str(&owned_rem);
                            self.emit_text_delta(emit_owned, out);
                        }
                    } else {
                        let tag_hold = longest_tag_prefix_at_end(&self.buffer, &self.open_prefix);
                        let candidate_len = self.buffer.len().saturating_sub(tag_hold);
                        let wrapper_hold = self
                            .buffer
                            .get(..candidate_len)
                            .and_then(|s| {
                                split_trailing_code_wrapper(s, self.line_single_backtick_open)
                            })
                            .map_or(0, |(_, w)| w.len());
                        let emit_len = candidate_len.saturating_sub(wrapper_hold);
                        if emit_len == 0 {
                            break;
                        }
                        let Some((emit_part, keep_part)) = self.buffer.split_at_checked(emit_len)
                        else {
                            break;
                        };
                        let emit_owned = emit_part.to_owned();
                        let owned_keep = keep_part.to_owned();
                        self.buffer.clear();
                        self.buffer.push_str(&owned_keep);
                        self.emit_text_delta(emit_owned, out);
                        break;
                    }
                }
                ParserMode::InOpenTagHeader => {
                    if let Some(gt_pos) = self.buffer.find('>') {
                        let Some((_, after_gt)) = self.buffer.split_at_checked(gt_pos + 1) else {
                            break;
                        };
                        let owned_rem = after_gt.to_owned();
                        self.buffer.clear();
                        self.buffer.push_str(&owned_rem);
                        self.mode = ParserMode::InTag;
                        self.in_tag_progress = InTagProgress::ProbingHeader;
                    } else if self.buffer.len() > 256 {
                        // Malformed header without `>` — flush as literal text
                        let leftover = mem::take(&mut self.buffer);
                        let pending_wrapper = mem::take(&mut self.pending_open_wrapper);
                        let mut raw = String::with_capacity(
                            pending_wrapper.len() + self.open_prefix.len() + leftover.len(),
                        );
                        raw.push_str(&pending_wrapper);
                        raw.push_str(&self.open_prefix);
                        raw.push_str(&leftover);
                        self.mode = ParserMode::Text;
                        self.emit_text_delta(raw, out);
                    } else {
                        break;
                    }
                }
                ParserMode::InTag => {
                    if let Some(close_pos) = self.buffer.find(&self.close_tag) {
                        let Some((body, rest_with_close)) = self.buffer.split_at_checked(close_pos)
                        else {
                            break;
                        };
                        let body_owned = body.to_owned();
                        let Some((_, remainder)) =
                            rest_with_close.split_at_checked(self.close_tag.len())
                        else {
                            break;
                        };
                        let owned_rem = remainder.to_owned();
                        self.buffer.clear();
                        self.buffer.push_str(&owned_rem);
                        self.mode = ParserMode::Text;
                        let progress = mem::take(&mut self.in_tag_progress);

                        let pending_wrapper = mem::take(&mut self.pending_open_wrapper);
                        let allow_patch_rewrite =
                            progress.emitted_decoded_bytes().is_none_or(|n| n == 0);
                        if let Some(mut call) = parse_tool_call_body(&body_owned) {
                            if allow_patch_rewrite && call.name == "apply_patch" {
                                call.arguments = normalize_apply_patch_text(&call.arguments);
                                call.is_raw_input = true;
                            }
                            self.strip_closing_wrapper = classify_closing_wrapper(&pending_wrapper);
                            if let Some(streamed_bytes) = progress.emitted_decoded_bytes()
                                && let Some(rem) = call.arguments.get(streamed_bytes..)
                                && !rem.is_empty()
                            {
                                out.push(ParsedStreamItem::ToolCallDelta(rem.to_owned()));
                            }
                            out.push(ParsedStreamItem::ToolCall(call));
                        } else if let Some(started_name) = progress.started_name() {
                            out.push(ParsedStreamItem::ToolCallAborted {
                                name: started_name.to_owned(),
                            });
                        } else {
                            let mut raw = String::with_capacity(
                                pending_wrapper.len()
                                    + self.open_tag.len()
                                    + body_owned.len()
                                    + self.close_tag.len(),
                            );
                            raw.push_str(&pending_wrapper);
                            raw.push_str(&self.open_tag);
                            raw.push_str(&body_owned);
                            raw.push_str(&self.close_tag);
                            self.emit_text_delta(raw, out);
                        }
                    } else {
                        self.advance_in_tag_streaming(out);
                        break;
                    }
                }
            }
        }
    }

    fn advance_in_tag_streaming(&mut self, out: &mut Vec<ParsedStreamItem>) {
        if self.in_tag_progress == InTagProgress::ProbingHeader {
            match probe_in_tag_header(&self.buffer) {
                HeaderProbe::NeedMoreBytes => return,
                HeaderProbe::WaitUntilClose => {
                    self.in_tag_progress = InTagProgress::WaitUntilClose;
                    return;
                }
                HeaderProbe::StartBuffered { name } => {
                    out.push(ParsedStreamItem::ToolCallStart {
                        name: name.clone(),
                        is_raw_input: false,
                    });
                    self.in_tag_progress = InTagProgress::StartedBuffered { name };
                    return;
                }
                HeaderProbe::StartStreamingQuotedInput { name, raw_cursor } => {
                    out.push(ParsedStreamItem::ToolCallStart {
                        name: name.clone(),
                        is_raw_input: true,
                    });
                    self.in_tag_progress = InTagProgress::StreamingQuotedInput {
                        name,
                        raw_cursor,
                        emitted_decoded_bytes: 0,
                    };
                }
                HeaderProbe::StartStreamingBareBody { name, raw_cursor } => {
                    out.push(ParsedStreamItem::ToolCallStart {
                        name: name.clone(),
                        is_raw_input: true,
                    });
                    self.in_tag_progress = InTagProgress::StreamingBareBody {
                        name,
                        raw_cursor,
                        emitted_decoded_bytes: 0,
                    };
                }
            }
        }

        let close_hold = longest_tag_prefix_at_end(&self.buffer, &self.close_tag);
        let inspect_end = self.buffer.len().saturating_sub(close_hold);

        match &mut self.in_tag_progress {
            InTagProgress::ProbingHeader
            | InTagProgress::WaitUntilClose
            | InTagProgress::StartedBuffered { .. } => {}
            InTagProgress::StreamingQuotedInput {
                raw_cursor,
                emitted_decoded_bytes,
                ..
            } => {
                if let Some(available) = self.buffer.get(*raw_cursor..inspect_end)
                    && !available.is_empty()
                {
                    let (decoded, consumed_raw) = decode_json_string_prefix(available);
                    *raw_cursor += consumed_raw;
                    if !decoded.is_empty() {
                        *emitted_decoded_bytes += decoded.len();
                        out.push(ParsedStreamItem::ToolCallDelta(decoded));
                    }
                }
            }
            InTagProgress::StreamingBareBody {
                raw_cursor,
                emitted_decoded_bytes,
                ..
            } => {
                if let Some(inspect_slice) = self.buffer.get(..inspect_end) {
                    let trimmed_slice =
                        inspect_slice.trim_end_matches(|c: char| c.is_whitespace() || c == '`');
                    if let Some(chunk_slice) = trimmed_slice.get(*raw_cursor..)
                        && !chunk_slice.is_empty()
                    {
                        *raw_cursor = trimmed_slice.len();
                        *emitted_decoded_bytes += chunk_slice.len();
                        out.push(ParsedStreamItem::ToolCallDelta(chunk_slice.to_owned()));
                    }
                }
            }
        }
    }
}

enum HeaderProbe {
    NeedMoreBytes,
    WaitUntilClose,
    StartBuffered { name: String },
    StartStreamingQuotedInput { name: String, raw_cursor: usize },
    StartStreamingBareBody { name: String, raw_cursor: usize },
}

fn skip_ascii_ws(buf: &str, mut pos: usize) -> usize {
    if let Some(rest) = buf.get(pos..) {
        pos += rest
            .bytes()
            .take_while(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
            .count();
    }
    pos
}

fn skip_optional_opening_fence(buf: &str) -> Option<usize> {
    let pos = skip_ascii_ws(buf, 0);
    let rest = buf.get(pos..)?;
    if rest.is_empty() || rest == "`" || rest == "``" {
        return None;
    }
    if let Some(after_ticks) = rest.strip_prefix("```") {
        let nl_rel = after_ticks.find('\n')?;
        return Some(skip_ascii_ws(buf, pos + 3 + nl_rel + 1));
    }
    Some(pos)
}

fn probe_in_tag_header(buf: &str) -> HeaderProbe {
    let Some(start) = skip_optional_opening_fence(buf) else {
        return if buf.len() > 512 {
            HeaderProbe::WaitUntilClose
        } else {
            HeaderProbe::NeedMoreBytes
        };
    };
    let Some(body) = buf.get(start..) else {
        return HeaderProbe::NeedMoreBytes;
    };
    if body.is_empty() {
        return HeaderProbe::NeedMoreBytes;
    }

    if body.starts_with('{') {
        let key1_pos = skip_ascii_ws(buf, start + 1);
        let Some(key1_rest) = buf.get(key1_pos..) else {
            return HeaderProbe::NeedMoreBytes;
        };
        if key1_rest.is_empty()
            || ("\"name\"".starts_with(key1_rest) && key1_rest.len() < "\"name\"".len())
        {
            return HeaderProbe::NeedMoreBytes;
        }
        if !key1_rest.starts_with("\"name\"") {
            return HeaderProbe::WaitUntilClose;
        }

        let colon1_pos = skip_ascii_ws(buf, key1_pos + "\"name\"".len());
        let Some(colon1_rest) = buf.get(colon1_pos..) else {
            return HeaderProbe::NeedMoreBytes;
        };
        if colon1_rest.is_empty() {
            return HeaderProbe::NeedMoreBytes;
        }
        if !colon1_rest.starts_with(':') {
            return HeaderProbe::WaitUntilClose;
        }

        let name_val_pos = skip_ascii_ws(buf, colon1_pos + 1);
        let Some(name_val_rest) = buf.get(name_val_pos..) else {
            return HeaderProbe::NeedMoreBytes;
        };
        if name_val_rest.is_empty() {
            return HeaderProbe::NeedMoreBytes;
        }
        if !name_val_rest.starts_with('"') {
            return HeaderProbe::WaitUntilClose;
        }

        let Some((raw_name, after_name_slice)) = take_quoted_json_string(name_val_rest) else {
            return if name_val_rest.len() > 256 {
                HeaderProbe::WaitUntilClose
            } else {
                HeaderProbe::NeedMoreBytes
            };
        };
        let name = raw_name.trim().to_owned();
        if name.is_empty() {
            return HeaderProbe::WaitUntilClose;
        }

        let after_name_pos = buf.len() - after_name_slice.len();
        let sep_pos = skip_ascii_ws(buf, after_name_pos);
        let Some(sep_rest) = buf.get(sep_pos..) else {
            return HeaderProbe::NeedMoreBytes;
        };
        if sep_rest.is_empty() {
            return HeaderProbe::NeedMoreBytes;
        }
        if sep_rest.starts_with('}') {
            if name == "apply_patch" {
                let after_brace_pos = skip_ascii_ws(buf, sep_pos + 1);
                if let Some(after_brace) = buf.get(after_brace_pos..) {
                    if after_brace.is_empty()
                        || ("*** Begin Patch".starts_with(after_brace)
                            && after_brace.len() < "*** Begin Patch".len())
                    {
                        return HeaderProbe::NeedMoreBytes;
                    }
                    if let Some(after_hdr) = after_brace.strip_prefix("*** Begin Patch") {
                        if after_hdr.is_empty() {
                            return HeaderProbe::NeedMoreBytes;
                        }
                        if after_hdr.starts_with('\n') || after_hdr.starts_with("\r\n") {
                            return HeaderProbe::StartStreamingBareBody {
                                name,
                                raw_cursor: after_brace_pos,
                            };
                        }
                    }
                }
            }
            return HeaderProbe::StartBuffered { name };
        }
        if !sep_rest.starts_with(',') {
            return HeaderProbe::WaitUntilClose;
        }

        let key2_pos = skip_ascii_ws(buf, sep_pos + 1);
        let Some(key2_rest) = buf.get(key2_pos..) else {
            return HeaderProbe::NeedMoreBytes;
        };
        if key2_rest.is_empty()
            || ("\"input\"".starts_with(key2_rest) && key2_rest.len() < "\"input\"".len())
            || ("\"arguments\"".starts_with(key2_rest) && key2_rest.len() < "\"arguments\"".len())
        {
            return HeaderProbe::NeedMoreBytes;
        }

        if key2_rest.starts_with("\"input\"") {
            let colon2_pos = skip_ascii_ws(buf, key2_pos + "\"input\"".len());
            let Some(colon2_rest) = buf.get(colon2_pos..) else {
                return HeaderProbe::NeedMoreBytes;
            };
            if colon2_rest.is_empty() {
                return HeaderProbe::NeedMoreBytes;
            };
            if !colon2_rest.starts_with(':') {
                return HeaderProbe::WaitUntilClose;
            }
            let val2_pos = skip_ascii_ws(buf, colon2_pos + 1);
            let Some(val2_rest) = buf.get(val2_pos..) else {
                return HeaderProbe::NeedMoreBytes;
            };
            if val2_rest.is_empty() {
                return HeaderProbe::NeedMoreBytes;
            }
            if val2_rest.starts_with('"') {
                return HeaderProbe::StartStreamingQuotedInput {
                    name,
                    raw_cursor: val2_pos + 1,
                };
            }
            return HeaderProbe::StartBuffered { name };
        }

        if key2_rest.starts_with("\"arguments\"") {
            let colon2_pos = skip_ascii_ws(buf, key2_pos + "\"arguments\"".len());
            let Some(colon2_rest) = buf.get(colon2_pos..) else {
                return HeaderProbe::NeedMoreBytes;
            };
            if colon2_rest.is_empty() {
                return HeaderProbe::NeedMoreBytes;
            }
            if !colon2_rest.starts_with(':') {
                return HeaderProbe::WaitUntilClose;
            }
            let val2_pos = skip_ascii_ws(buf, colon2_pos + 1);
            let Some(val2_rest) = buf.get(val2_pos..) else {
                return HeaderProbe::NeedMoreBytes;
            };
            if val2_rest.is_empty() {
                return HeaderProbe::NeedMoreBytes;
            }
            return HeaderProbe::StartBuffered { name };
        }

        return HeaderProbe::WaitUntilClose;
    }

    if "*** Begin Patch".starts_with(body) && body.len() < "*** Begin Patch".len() {
        return HeaderProbe::NeedMoreBytes;
    }
    if let Some(after_patch_hdr) = body.strip_prefix("*** Begin Patch") {
        if after_patch_hdr.is_empty() {
            return HeaderProbe::NeedMoreBytes;
        }
        if after_patch_hdr.starts_with('\n') || after_patch_hdr.starts_with("\r\n") {
            return HeaderProbe::StartStreamingBareBody {
                name: "apply_patch".to_owned(),
                raw_cursor: start,
            };
        }
        return HeaderProbe::WaitUntilClose;
    }

    if "apply_patch".starts_with(body) && body.len() < "apply_patch".len() {
        return HeaderProbe::NeedMoreBytes;
    }
    if let Some(after_tool_label) = body.strip_prefix("apply_patch")
        && (after_tool_label.is_empty()
            || after_tool_label.starts_with('\n')
            || after_tool_label.starts_with("\r\n"))
    {
        let patch_pos = skip_ascii_ws(buf, start + "apply_patch".len());
        let Some(patch_rest) = buf.get(patch_pos..) else {
            return HeaderProbe::NeedMoreBytes;
        };
        if patch_rest.is_empty()
            || ("*** Begin Patch".starts_with(patch_rest)
                && patch_rest.len() < "*** Begin Patch".len())
        {
            return HeaderProbe::NeedMoreBytes;
        }
        if let Some(after_hdr) = patch_rest.strip_prefix("*** Begin Patch") {
            if after_hdr.is_empty() {
                return HeaderProbe::NeedMoreBytes;
            }
            if after_hdr.starts_with('\n') || after_hdr.starts_with("\r\n") {
                return HeaderProbe::StartStreamingBareBody {
                    name: "apply_patch".to_owned(),
                    raw_cursor: patch_pos,
                };
            }
        }
        return HeaderProbe::WaitUntilClose;
    }

    if "// @exec:".starts_with(body) && body.len() < "// @exec:".len() {
        return HeaderProbe::NeedMoreBytes;
    }
    if body.starts_with("// @exec:") {
        if body.contains('\n') {
            return HeaderProbe::StartStreamingBareBody {
                name: "exec".to_owned(),
                raw_cursor: start,
            };
        }
        return if body.len() > 256 {
            HeaderProbe::WaitUntilClose
        } else {
            HeaderProbe::NeedMoreBytes
        };
    }

    HeaderProbe::WaitUntilClose
}

/// Decodes a prefix of a JSON `"input": "..."` string body up to (but excluding) the first
/// unescaped `"` or any incomplete trailing `\` escape sequence, returning `(decoded, raw_bytes_consumed)`.
fn decode_json_string_prefix(available: &str) -> (String, usize) {
    let mut out = String::with_capacity(available.len());
    let mut pos = 0usize;

    while let Some(rem) = available.get(pos..) {
        let Some(ch) = rem.chars().next() else {
            break;
        };
        if ch == '"' {
            // Stop at the first unescaped quote: either it closes `"input"` or (if unescaped inner
            // quote) will be resolved once `</agy_tool_call_{nonce}>` arrives.
            break;
        }
        if ch != '\\' {
            out.push(ch);
            pos += ch.len_utf8();
            continue;
        }

        let Some(after_bs) = rem.get(1..) else {
            break;
        };
        let Some(esc) = after_bs.chars().next() else {
            // Incomplete `\` at chunk boundary — wait for next chunk.
            break;
        };
        match esc {
            '"' => {
                out.push('"');
                pos += 2;
            }
            '\\' => {
                out.push('\\');
                pos += 2;
            }
            '/' => {
                out.push('/');
                pos += 2;
            }
            'b' => {
                out.push('\u{0008}');
                pos += 2;
            }
            'f' => {
                out.push('\u{000C}');
                pos += 2;
            }
            'n' => {
                out.push('\n');
                pos += 2;
            }
            'r' => {
                out.push('\r');
                pos += 2;
            }
            't' => {
                out.push('\t');
                pos += 2;
            }
            'u' => {
                if after_bs.len() < 5 {
                    break;
                }
                if let Some(hex4) = after_bs.get(1..5)
                    && let Ok(code) = u16::from_str_radix(hex4, 16)
                {
                    if let Some(decoded) = char::from_u32(u32::from(code)) {
                        out.push(decoded);
                        pos += 6;
                    } else if (0xD800..=0xDBFF).contains(&code) {
                        if after_bs.len() < 11 {
                            break;
                        }
                        if after_bs.get(5..7) == Some("\\u")
                            && let Some(lo_hex) = after_bs.get(7..11)
                            && let Ok(lo) = u16::from_str_radix(lo_hex, 16)
                            && (0xDC00..=0xDFFF).contains(&lo)
                            && let Some(pair_ch) = char::from_u32(
                                0x10000
                                    + (((u32::from(code) - 0xD800) << 10)
                                        | (u32::from(lo) - 0xDC00)),
                            )
                        {
                            out.push(pair_ch);
                            pos += 12;
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                } else {
                    out.push_str("\\u");
                    pos += 2;
                }
            }
            other => {
                out.push('\\');
                out.push(other);
                pos += 1 + other.len_utf8();
            }
        }
    }

    (out, pos)
}

fn advance_line_backtick_state(open: &mut bool, text: &str) {
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\n' {
            *open = false;
        } else if ch == '`' {
            let mut run = 1usize;
            while chars.peek() == Some(&'`') {
                let _ = chars.next();
                run += 1;
            }
            if run == 1 {
                *open = !*open;
            }
        }
    }
}

fn classify_closing_wrapper(open_wrapper: &str) -> Option<ClosingWrapper> {
    if open_wrapper.starts_with("```") {
        Some(ClosingWrapper::CodeFence)
    } else if open_wrapper == "`" {
        Some(ClosingWrapper::InlineBacktick)
    } else {
        None
    }
}

/// Detects if `before` ends with an unclosed markdown inline backtick (`` ` ``) or fenced code
/// block opener (```` ```xml\n ````) intended to wrap an `<agy_tool_call_{nonce}>` block.
fn split_trailing_code_wrapper(before: &str, prior_line_open: bool) -> Option<(&str, &str)> {
    // 1. Check fenced code block opener on the trailing line(s): ```` ```[lang]\n ```` or ```` ``` ````
    let fence_scan_start = before.len().saturating_sub(32);
    let mut search_from = fence_scan_start;
    while search_from < before.len() && !before.is_char_boundary(search_from) {
        search_from += 1;
    }
    if let Some(tail) = before.get(search_from..)
        && let Some(rel_fence) = tail.rfind("```")
    {
        let abs_fence = search_from + rel_fence;
        let at_line_start = before
            .get(..abs_fence)
            .is_none_or(|prefix| prefix.ends_with('\n') || prefix.trim().is_empty());
        if at_line_start
            && let Some((head, fence_part)) = before.split_at_checked(abs_fence)
            && let Some(after_ticks) = fence_part.strip_prefix("```")
        {
            let valid_fence_header = if let Some((lang, ws)) = after_ticks.split_once('\n') {
                let lang_trimmed = lang.trim_end_matches('\r').trim();
                lang_trimmed.len() <= 16
                    && lang_trimmed
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                    && ws
                        .bytes()
                        .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
            } else {
                after_ticks.len() <= 16
                    && after_ticks
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b' '))
            };
            if valid_fence_header {
                return Some((head, fence_part));
            }
        }
    }

    // 2. Check single opening inline backtick right at the end (`...测试。`<agy_tool_call_...`)
    if before.ends_with('`') && !before.ends_with("``") {
        let prefix_len = before.len().saturating_sub(1);
        if let Some((head, tick)) = before.split_at_checked(prefix_len) {
            let mut open = prior_line_open;
            advance_line_backtick_state(&mut open, head);
            if !open {
                return Some((head, tick));
            }
        }
    }

    None
}

fn longest_tag_prefix_at_end(haystack: &str, tag: &str) -> usize {
    let max_len = haystack.len().min(tag.len().saturating_sub(1));
    for candidate_len in (1..=max_len).rev() {
        if let Some((prefix, _)) = tag.split_at_checked(candidate_len)
            && haystack.ends_with(prefix)
        {
            return candidate_len;
        }
    }
    0
}

#[derive(Deserialize)]
struct RawToolCallEnvelope<'a> {
    name: String,
    #[serde(default, borrow)]
    arguments: Option<&'a RawValue>,
    #[serde(default, borrow)]
    input: Option<&'a RawValue>,
}

fn strip_optional_code_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(after_open) = trimmed.strip_prefix("```") {
        let body = if let Some((_, after_lang_line)) = after_open.split_once('\n') {
            after_lang_line
        } else {
            after_open
        };
        if let Some(stripped) = body.trim_end().strip_suffix("```") {
            return stripped.trim();
        }
    }
    trimmed
}

fn parse_tool_call_body(raw_body: &str) -> Option<ParsedToolCall> {
    let cleaned = strip_optional_code_fence(raw_body);
    if let Some(call) = parse_strict_envelope(cleaned) {
        return Some(call);
    }
    let repaired = repair_llm_json(cleaned);
    if let Some(call) = parse_strict_envelope(&repaired) {
        return Some(call);
    }
    salvage_freeform_input_envelope(cleaned).or_else(|| salvage_raw_custom_tool_body(cleaned))
}

fn parse_strict_envelope(candidate: &str) -> Option<ParsedToolCall> {
    let envelope: RawToolCallEnvelope<'_> = serde_json::from_str(candidate).ok()?;
    let name = envelope.name.trim().to_owned();
    if name.is_empty() {
        return None;
    }

    if let Some(arg_raw) = envelope.arguments {
        let raw_text = arg_raw.get().trim();
        if raw_text.starts_with('"') {
            let inner_str = serde_json::from_str::<String>(raw_text).ok()?;
            let inner_trimmed = inner_str.trim();
            if inner_trimmed.starts_with('{') {
                if serde_json::from_str::<serde::de::IgnoredAny>(inner_trimmed).is_ok() {
                    if name == "apply_patch"
                        && let Some(unwrapped_patch) = extract_patch_from_json_object(inner_trimmed)
                    {
                        return Some(ParsedToolCall {
                            name,
                            arguments: unwrapped_patch,
                            is_raw_input: true,
                        });
                    }
                    return Some(ParsedToolCall {
                        name,
                        arguments: inner_trimmed.to_owned(),
                        is_raw_input: false,
                    });
                }
                let repaired_inner = repair_llm_json(inner_trimmed);
                if serde_json::from_str::<serde::de::IgnoredAny>(&repaired_inner).is_ok() {
                    if name == "apply_patch"
                        && let Some(unwrapped_patch) =
                            extract_patch_from_json_object(&repaired_inner)
                    {
                        return Some(ParsedToolCall {
                            name,
                            arguments: unwrapped_patch,
                            is_raw_input: true,
                        });
                    }
                    return Some(ParsedToolCall {
                        name,
                        arguments: repaired_inner,
                        is_raw_input: false,
                    });
                }
            }
            let arguments = if name == "apply_patch" {
                normalize_apply_patch_text(&inner_str)
            } else {
                inner_str
            };
            return Some(ParsedToolCall {
                name,
                arguments,
                is_raw_input: true,
            });
        }
        if name == "apply_patch"
            && let Some(unwrapped_patch) = extract_patch_from_json_object(raw_text)
        {
            return Some(ParsedToolCall {
                name,
                arguments: unwrapped_patch,
                is_raw_input: true,
            });
        }
        return Some(ParsedToolCall {
            name,
            arguments: raw_text.to_owned(),
            is_raw_input: false,
        });
    }

    if let Some(input_raw) = envelope.input {
        let raw_text = input_raw.get().trim();
        if raw_text.starts_with('"') {
            let inner_str = serde_json::from_str::<String>(raw_text).ok()?;
            let arguments = if name == "apply_patch" {
                normalize_apply_patch_text(&inner_str)
            } else {
                inner_str
            };
            return Some(ParsedToolCall {
                name,
                arguments,
                is_raw_input: true,
            });
        }
        if name == "apply_patch"
            && let Some(unwrapped_patch) = extract_patch_from_json_object(raw_text)
        {
            return Some(ParsedToolCall {
                name,
                arguments: unwrapped_patch,
                is_raw_input: true,
            });
        }
        return Some(ParsedToolCall {
            name,
            arguments: raw_text.to_owned(),
            is_raw_input: false,
        });
    }

    Some(ParsedToolCall {
        name,
        arguments: "{}".to_owned(),
        is_raw_input: false,
    })
}

/// Repairs common non-RFC-8259 JSON defects emitted by LLMs inside `<agy_tool_call_{nonce}>`:
/// 1. Unescaped backslashes inside string literals (`\.`, `\s`, `\d`, `\$`, `\'`, `\users`, etc.)
/// 2. Raw unescaped ASCII control characters (`\n`, `\r`, `\t`, `0x00..=0x1F`) inside strings
/// 3. Trailing commas immediately preceding `}` or `]` outside strings
fn repair_llm_json(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    let mut chars = input.char_indices().peekable();
    let mut in_string = false;

    while let Some((idx, ch)) = chars.next() {
        if in_string {
            match ch {
                '"' => {
                    in_string = false;
                    out.push('"');
                }
                '\\' => match chars.peek().copied() {
                    Some((_, esc @ ('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't'))) => {
                        let _ = chars.next();
                        out.push('\\');
                        out.push(esc);
                    }
                    Some((u_idx, 'u')) => {
                        let hex_valid = input
                            .as_bytes()
                            .get(u_idx + 1..u_idx + 5)
                            .is_some_and(|s| s.iter().all(u8::is_ascii_hexdigit));
                        if hex_valid {
                            let _ = chars.next();
                            out.push_str("\\u");
                        } else {
                            out.push_str("\\\\");
                        }
                    }
                    Some(_) | None => {
                        out.push_str("\\\\");
                    }
                },
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    let _ = write!(out, "\\u{:04x}", c as u32);
                }
                other => out.push(other),
            }
        } else {
            match ch {
                '"' => {
                    in_string = true;
                    out.push('"');
                }
                ',' => {
                    let trailing = input
                        .get(idx + 1..)
                        .map(str::trim_start)
                        .is_some_and(|rest| rest.starts_with('}') || rest.starts_with(']'));
                    if !trailing {
                        out.push(',');
                    }
                }
                other => out.push(other),
            }
        }
    }

    out
}

/// Salvages freeform string envelopes (`{"name": "apply_patch", "input": "..."}`,
/// `{"name": "exec", "input": "..."}`, or Claude Code Mode `{"name": "REPL", "arguments": {"code": "..."}}`)
/// when the model left unescaped `"` characters inside a multi-line patch or JavaScript body.
fn salvage_freeform_input_envelope(cleaned: &str) -> Option<ParsedToolCall> {
    let inner_obj = cleaned.trim().strip_prefix('{')?.strip_suffix('}')?.trim();
    let after_name_key = inner_obj
        .strip_prefix("\"name\"")?
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    let (raw_name, after_name_val) = take_quoted_json_string(after_name_key)?;
    let name = raw_name.trim().to_owned();
    if name.is_empty() {
        return None;
    }

    let after_comma = after_name_val.trim_start().strip_prefix(',')?.trim_start();
    let (is_arguments_key, after_val_key) =
        if let Some(rest) = after_comma.strip_prefix("\"input\"") {
            (false, rest.trim_start().strip_prefix(':')?.trim_start())
        } else {
            let rest = after_comma.strip_prefix("\"arguments\"")?;
            (true, rest.trim_start().strip_prefix(':')?.trim_start())
        };

    let without_trailing_comma = after_val_key
        .trim_end()
        .strip_suffix(',')
        .unwrap_or(after_val_key)
        .trim_end();

    if is_arguments_key
        && name == "REPL"
        && let Some(inner_args) = without_trailing_comma
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
    {
        let code_val = inner_args
            .trim()
            .strip_prefix("\"code\"")?
            .trim_start()
            .strip_prefix(':')?
            .trim();
        let code_without_comma = code_val.strip_suffix(',').unwrap_or(code_val).trim_end();
        let raw_code = code_without_comma.strip_prefix('"')?.strip_suffix('"')?;
        let trailing_bs = raw_code.bytes().rev().take_while(|&b| b == b'\\').count();
        if trailing_bs % 2 != 0 {
            return None;
        }
        let decoded_code = unescape_lenient_json_string(raw_code);
        let serialized = serde_json::json!({ "code": decoded_code }).to_string();
        return Some(ParsedToolCall {
            name,
            arguments: serialized,
            is_raw_input: false,
        });
    }

    let raw_body = without_trailing_comma
        .strip_prefix('"')?
        .strip_suffix('"')?;
    let trailing_backslashes = raw_body.bytes().rev().take_while(|&b| b == b'\\').count();
    if trailing_backslashes % 2 != 0 {
        return None;
    }

    let decoded = unescape_lenient_json_string(raw_body);
    let arguments = if name == "apply_patch" {
        normalize_apply_patch_text(&decoded)
    } else {
        decoded
    };
    Some(ParsedToolCall {
        name,
        arguments,
        is_raw_input: true,
    })
}

/// Salvages raw custom-tool bodies emitted directly inside `<agy_tool_call_{nonce}>` without a
/// JSON envelope (for example when a model obeys Codex Code Mode `exec`'s upstream prompt hint
/// `"Accepts raw JavaScript source text, not JSON"` literally, or writes a bare `*** Begin Patch`).
fn salvage_raw_custom_tool_body(cleaned: &str) -> Option<ParsedToolCall> {
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('{') {
        // Handle `{"name": "apply_patch"}\n*** Begin Patch...` hybrid header + bare body.
        if let Some((header_obj, rest)) = trimmed.split_once('}') {
            let candidate_obj = format!("{header_obj}}}");
            if let Ok(env) = serde_json::from_str::<RawToolCallEnvelope<'_>>(&candidate_obj)
                && env.name.trim() == "apply_patch"
                && env.arguments.is_none()
                && env.input.is_none()
            {
                let normalized = normalize_apply_patch_text(rest);
                if normalized.starts_with("*** Begin Patch") {
                    return Some(ParsedToolCall {
                        name: "apply_patch".to_owned(),
                        arguments: normalized,
                        is_raw_input: true,
                    });
                }
            }
        }
        return None;
    }
    if trimmed.starts_with("*** Begin Patch") {
        return Some(ParsedToolCall {
            name: "apply_patch".to_owned(),
            arguments: normalize_apply_patch_text(trimmed),
            is_raw_input: true,
        });
    }
    if let Some(after_label) = trimmed.strip_prefix("apply_patch") {
        let normalized = normalize_apply_patch_text(after_label);
        if normalized.starts_with("*** Begin Patch") {
            return Some(ParsedToolCall {
                name: "apply_patch".to_owned(),
                arguments: normalized,
                is_raw_input: true,
            });
        }
    }
    if looks_like_code_mode_js(trimmed) {
        return Some(ParsedToolCall {
            name: "exec".to_owned(),
            arguments: trimmed.to_owned(),
            is_raw_input: true,
        });
    }
    None
}

fn extract_patch_from_json_object(raw_json_obj: &str) -> Option<String> {
    let val = serde_json::from_str::<serde_json::Value>(raw_json_obj).ok()?;
    let map = val.as_object()?;
    for key in ["input", "patch", "diff", "content"] {
        if let Some(s) = map.get(key).and_then(serde_json::Value::as_str) {
            let normalized = normalize_apply_patch_text(s);
            if normalized.starts_with("*** Begin Patch") {
                return Some(normalized);
            }
        }
    }
    None
}

/// Normalizes an `apply_patch` payload across XML and native MCP channels:
/// 1. Strips accidental markdown code fences, JSON object/string wrappers, and CRLF line endings.
/// 2. Unescapes single-line double-escaped `\n` patches (`"*** Begin Patch\\n..."`).
/// 3. Translates standard unified git diffs (`--- a/p` + `+++ b/p`) into V4A patches when V4A
///    headers are absent, and restores omitted outer `*** Begin Patch` / `*** End Patch` markers.
/// 4. Repairs stripped blank context/add lines (`""` -> `" "` in `Update File`, `"+"` in `Add File`)
///    that would otherwise fail Codex's Lark grammar.
/// 5. Coalesces duplicate `*** Update File: <same_path>` blocks into a single file operation so
///    Codex never aborts with `invalid patch: multiple operations target <path>`.
#[must_use]
pub fn normalize_apply_patch_text(raw: &str) -> String {
    let mut text = strip_optional_code_fence(raw).trim().to_owned();
    if text.starts_with('{')
        && text.ends_with('}')
        && let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&text)
    {
        for key in ["patch", "input", "diff", "content"] {
            if let Some(s) = map.get(key).and_then(serde_json::Value::as_str) {
                text = strip_optional_code_fence(s).trim().to_owned();
                break;
            }
        }
    } else if text.starts_with('"')
        && text.ends_with('"')
        && let Ok(unquoted) = serde_json::from_str::<String>(&text)
    {
        text = strip_optional_code_fence(&unquoted).trim().to_owned();
    }

    if text.contains('\r') {
        text = text.replace("\r\n", "\n").replace('\r', "\n");
    }

    if !text.contains("*** Begin Patch\n") && text.contains("*** Begin Patch\\n") {
        text = unescape_lenient_json_string(&text);
        if text.contains('\r') {
            text = text.replace("\r\n", "\n").replace('\r', "\n");
        }
    }
    if !text.contains("\n*** End Patch")
        && let Some(stem) = text.strip_suffix("\\n*** End Patch")
    {
        text = format!("{stem}\n*** End Patch");
    }

    if !text.contains("*** Begin Patch")
        && !starts_with_v4a_file_header(&text)
        && let Some(converted) = try_convert_unified_diff_to_v4a(&text)
    {
        return coalesce_and_repair_v4a_patch(&converted);
    }

    if !text.contains("*** Begin Patch") && starts_with_v4a_file_header(&text) {
        text = format!("*** Begin Patch\n{text}");
    }

    let framed = if let Some(begin_idx) = text.find("*** Begin Patch")
        && let Some(from_begin) = text.get(begin_idx..)
    {
        if let Some(end_rel) = from_begin.rfind("*** End Patch") {
            let end_marker_end = end_rel + "*** End Patch".len();
            if let Some(after_end) = from_begin.get(end_marker_end..)
                && after_end.chars().all(|c| c.is_whitespace() || c == '`')
                && let Some(clean_patch) = from_begin.get(..end_marker_end)
            {
                clean_patch.to_owned()
            } else {
                from_begin.trim_end().to_owned()
            }
        } else if contains_v4a_file_header(from_begin) {
            format!("{}\n*** End Patch", from_begin.trim_end())
        } else {
            from_begin.trim_end().to_owned()
        }
    } else {
        return text.trim().to_owned();
    };

    coalesce_and_repair_v4a_patch(&framed)
}

pub use liftgate_codemode_js::normalize_code_mode_exec_js;

/// Structured 0-RTT structural preflight failure for V4A `apply_patch` payloads.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ApplyPatchPreflightError {
    #[error(
        "Empty `patch`: provide a non-empty V4A patch starting with '*** Begin Patch' and ending with '*** End Patch'."
    )]
    EmptyPatch,
    #[error(
        "Invalid `apply_patch` header: patch must start with '*** Begin Patch' followed by '*** Update File: <path>', '*** Add File: <path>', or '*** Delete File: <path>'."
    )]
    MissingBeginHeader,
    #[error(
        "Invalid `apply_patch` framing: patch must start with '*** Begin Patch\\n' and end with '*** End Patch'."
    )]
    InvalidFraming,
    #[error(
        "Invalid `apply_patch`: patch contains zero file operations; include at least one '*** Update File: <path>', '*** Add File: <path>', or '*** Delete File: <path>'."
    )]
    ZeroFileOperations,
    #[error("Invalid `apply_patch`: '*** Add File: {path}' has no '+<line>' content lines.")]
    EmptyAddFile { path: String },
    #[error("Invalid `apply_patch`: '*** Update File: {path}' contains no '+/-' change lines.")]
    EmptyUpdateFile { path: String },
    #[error("Invalid `apply_patch` at line {line_no}: empty path in '{header}'.")]
    EmptyFilePath {
        line_no: usize,
        header: &'static str,
    },
    #[error("Invalid `apply_patch` at line {line_no}: empty destination path in '*** Move to: '.")]
    EmptyMoveDestination { line_no: usize },
    #[error(
        "Invalid `apply_patch` syntax at line {line_no} ({preview}): expected '*** Update File: <path>', '*** Add File: <path>', '*** Delete File: <path>', or '*** End Patch'."
    )]
    UnexpectedRootLine { line_no: usize, preview: String },
    #[error(
        "Invalid `apply_patch` syntax at line {line_no} in '*** Add File: {path}' ({preview}): every added file line must start with '+'."
    )]
    InvalidAddLine {
        line_no: usize,
        path: String,
        preview: String,
    },
    #[error(
        "Invalid `apply_patch` syntax at line {line_no} in '*** Update File: {path}' ({preview}): every hunk line must start with ' ' (context), '-' (remove), '+' (add), or '@@'."
    )]
    InvalidUpdateHunkLine {
        line_no: usize,
        path: String,
        preview: String,
    },
    #[error(
        "Invalid `apply_patch` syntax at line {line_no} in '*** Update File: {path}' ({preview}): '*** End of File' may only appear at the end of the file's hunks."
    )]
    TrailingLinesAfterEof {
        line_no: usize,
        path: String,
        preview: String,
    },
}

/// Validates a normalized V4A `apply_patch` payload before forwarding an MCP call to the client,
/// returning a typed [`ApplyPatchPreflightError`] with 1-based line diagnostics so models self-heal in 0-RTT.
pub fn validate_apply_patch_preflight(patch: &str) -> Result<(), ApplyPatchPreflightError> {
    let trimmed = patch.trim();
    if trimmed.is_empty() {
        return Err(ApplyPatchPreflightError::EmptyPatch);
    }
    let Some(after_begin) = trimmed.strip_prefix("*** Begin Patch") else {
        return Err(ApplyPatchPreflightError::MissingBeginHeader);
    };
    if !after_begin.starts_with('\n') || !trimmed.ends_with("*** End Patch") {
        return Err(ApplyPatchPreflightError::InvalidFraming);
    }

    enum Section<'a> {
        Root,
        Add {
            path: &'a str,
            count: usize,
        },
        Delete,
        Update {
            path: &'a str,
            has_move: bool,
            hunk_lines: usize,
            change_lines: usize,
            saw_eof: bool,
        },
    }

    let finalize_section = |sec: &Section<'_>| -> Result<(), ApplyPatchPreflightError> {
        match *sec {
            Section::Root | Section::Delete => Ok(()),
            Section::Add { path, count } => {
                if count == 0 {
                    Err(ApplyPatchPreflightError::EmptyAddFile {
                        path: path.to_owned(),
                    })
                } else {
                    Ok(())
                }
            }
            Section::Update {
                path,
                has_move,
                change_lines,
                ..
            } => {
                if !has_move && change_lines == 0 {
                    Err(ApplyPatchPreflightError::EmptyUpdateFile {
                        path: path.to_owned(),
                    })
                } else {
                    Ok(())
                }
            }
        }
    };

    let lines: Vec<&str> = trimmed.split('\n').collect();
    let last_idx = lines.len().saturating_sub(1);
    let mut section = Section::Root;
    let mut op_count = 0usize;

    for (idx, &line) in lines.iter().enumerate() {
        let line_no = idx + 1;
        if idx == 0 {
            continue;
        }
        if idx == last_idx && line == "*** End Patch" {
            finalize_section(&section)?;
            break;
        }
        if let Some(p) = line.strip_prefix("*** Add File: ") {
            finalize_section(&section)?;
            let path = p.trim();
            if path.is_empty() {
                return Err(ApplyPatchPreflightError::EmptyFilePath {
                    line_no,
                    header: "*** Add File: ",
                });
            }
            op_count += 1;
            section = Section::Add { path, count: 0 };
            continue;
        }
        if let Some(p) = line.strip_prefix("*** Delete File: ") {
            finalize_section(&section)?;
            let path = p.trim();
            if path.is_empty() {
                return Err(ApplyPatchPreflightError::EmptyFilePath {
                    line_no,
                    header: "*** Delete File: ",
                });
            }
            op_count += 1;
            section = Section::Delete;
            continue;
        }
        if let Some(p) = line.strip_prefix("*** Update File: ") {
            finalize_section(&section)?;
            let path = p.trim();
            if path.is_empty() {
                return Err(ApplyPatchPreflightError::EmptyFilePath {
                    line_no,
                    header: "*** Update File: ",
                });
            }
            op_count += 1;
            section = Section::Update {
                path,
                has_move: false,
                hunk_lines: 0,
                change_lines: 0,
                saw_eof: false,
            };
            continue;
        }

        match &mut section {
            Section::Root | Section::Delete => {
                return Err(ApplyPatchPreflightError::UnexpectedRootLine {
                    line_no,
                    preview: preview_patch_line(line),
                });
            }
            Section::Add { path, count } => {
                if !line.starts_with('+') {
                    return Err(ApplyPatchPreflightError::InvalidAddLine {
                        line_no,
                        path: (*path).to_owned(),
                        preview: preview_patch_line(line),
                    });
                }
                *count += 1;
            }
            Section::Update {
                path,
                has_move,
                hunk_lines,
                change_lines,
                saw_eof,
            } => {
                if *saw_eof {
                    return Err(ApplyPatchPreflightError::TrailingLinesAfterEof {
                        line_no,
                        path: (*path).to_owned(),
                        preview: preview_patch_line(line),
                    });
                }
                if *hunk_lines == 0
                    && !*has_move
                    && let Some(dst) = line.strip_prefix("*** Move to: ")
                {
                    if dst.trim().is_empty() {
                        return Err(ApplyPatchPreflightError::EmptyMoveDestination { line_no });
                    }
                    *has_move = true;
                    continue;
                }
                if line == "*** End of File" {
                    if *hunk_lines == 0 {
                        return Err(ApplyPatchPreflightError::EmptyUpdateFile {
                            path: (*path).to_owned(),
                        });
                    }
                    *saw_eof = true;
                    continue;
                }
                let valid_hunk_line = line == "@@"
                    || line.starts_with("@@ ")
                    || line.starts_with('+')
                    || line.starts_with('-')
                    || line.starts_with(' ');
                if !valid_hunk_line {
                    return Err(ApplyPatchPreflightError::InvalidUpdateHunkLine {
                        line_no,
                        path: (*path).to_owned(),
                        preview: preview_patch_line(line),
                    });
                }
                *hunk_lines += 1;
                if line.starts_with('+') || line.starts_with('-') {
                    *change_lines += 1;
                }
            }
        }
    }

    if op_count == 0 {
        return Err(ApplyPatchPreflightError::ZeroFileOperations);
    }
    Ok(())
}

fn starts_with_v4a_file_header(s: &str) -> bool {
    let t = s.trim_start();
    t.starts_with("*** Update File: ")
        || t.starts_with("*** Add File: ")
        || t.starts_with("*** Delete File: ")
}

fn contains_v4a_file_header(s: &str) -> bool {
    s.contains("\n*** Update File: ")
        || s.contains("\n*** Add File: ")
        || s.contains("\n*** Delete File: ")
}

fn preview_patch_line(line: &str) -> String {
    let mut end = line.len().min(64);
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{:?}", line.get(..end).unwrap_or_default())
}

fn coalesce_and_repair_v4a_patch(framed: &str) -> String {
    if framed == "*** Begin Patch\n*** End Patch" {
        return framed.to_owned();
    }
    let Some(after_begin) = framed.strip_prefix("*** Begin Patch\n") else {
        return framed.to_owned();
    };
    let Some(inner) = after_begin.strip_suffix("\n*** End Patch") else {
        return framed.to_owned();
    };

    enum V4aOp {
        Add {
            path: String,
            lines: Vec<String>,
        },
        Delete {
            path: String,
        },
        Update {
            path: String,
            move_to: Option<String>,
            lines: Vec<String>,
        },
    }

    let mut ops: Vec<V4aOp> = Vec::new();
    for line in inner.split('\n') {
        if let Some(p) = line.strip_prefix("*** Add File: ") {
            ops.push(V4aOp::Add {
                path: p.trim().to_owned(),
                lines: Vec::new(),
            });
            continue;
        }
        if let Some(p) = line.strip_prefix("*** Delete File: ") {
            ops.push(V4aOp::Delete {
                path: p.trim().to_owned(),
            });
            continue;
        }
        if let Some(p) = line.strip_prefix("*** Update File: ") {
            ops.push(V4aOp::Update {
                path: p.trim().to_owned(),
                move_to: None,
                lines: Vec::new(),
            });
            continue;
        }

        match ops.last_mut() {
            None | Some(V4aOp::Delete { .. }) => {
                if line.trim().is_empty() {
                    continue;
                }
                return framed.to_owned();
            }
            Some(V4aOp::Add { lines, .. }) => {
                lines.push(line.to_owned());
            }
            Some(V4aOp::Update { move_to, lines, .. }) => {
                if lines.is_empty()
                    && move_to.is_none()
                    && let Some(dst) = line.strip_prefix("*** Move to: ")
                {
                    *move_to = Some(dst.trim().to_owned());
                } else {
                    lines.push(line.to_owned());
                }
            }
        }
    }

    for op in &mut ops {
        match op {
            V4aOp::Delete { .. } => {}
            V4aOp::Add { lines, .. } => {
                while lines.last().is_some_and(String::is_empty) {
                    let _ = lines.pop();
                }
                for ln in lines {
                    if ln.is_empty() {
                        ln.push('+');
                    }
                }
            }
            V4aOp::Update { lines, .. } => {
                while lines.last().is_some_and(String::is_empty) {
                    let _ = lines.pop();
                }
                for ln in lines {
                    if ln.is_empty() {
                        ln.push(' ');
                    } else if let Some(cleaned_hunk) = normalize_v4a_hunk_header(ln) {
                        *ln = cleaned_hunk;
                    }
                }
            }
        }
    }

    let mut coalesced: Vec<V4aOp> = Vec::with_capacity(ops.len());
    for op in ops {
        match op {
            V4aOp::Update {
                path,
                move_to: None,
                lines: mut extra,
            } => {
                let mut merged = false;
                for prev in coalesced.iter_mut().rev() {
                    match prev {
                        V4aOp::Delete { path: p } | V4aOp::Add { path: p, .. } if *p == path => {
                            break;
                        }
                        V4aOp::Update {
                            path: p,
                            move_to: Some(dst),
                            ..
                        } if *p == path || *dst == path => {
                            break;
                        }
                        V4aOp::Update {
                            path: p,
                            move_to: None,
                            lines: existing,
                        } if *p == path => {
                            if !extra.is_empty() {
                                merge_v4a_update_lines(existing, &mut extra);
                            }
                            merged = true;
                            break;
                        }
                        V4aOp::Delete { .. } | V4aOp::Add { .. } | V4aOp::Update { .. } => {}
                    }
                }
                if !merged {
                    coalesced.push(V4aOp::Update {
                        path,
                        move_to: None,
                        lines: extra,
                    });
                }
            }
            other => coalesced.push(other),
        }
    }

    let mut out = String::with_capacity(framed.len() + 16);
    out.push_str("*** Begin Patch\n");
    for op in coalesced {
        match op {
            V4aOp::Add { path, lines } => {
                out.push_str("*** Add File: ");
                out.push_str(&path);
                out.push('\n');
                for ln in lines {
                    out.push_str(&ln);
                    out.push('\n');
                }
            }
            V4aOp::Delete { path } => {
                out.push_str("*** Delete File: ");
                out.push_str(&path);
                out.push('\n');
            }
            V4aOp::Update {
                path,
                move_to,
                lines,
            } => {
                out.push_str("*** Update File: ");
                out.push_str(&path);
                out.push('\n');
                if let Some(dst) = move_to {
                    out.push_str("*** Move to: ");
                    out.push_str(&dst);
                    out.push('\n');
                }
                for ln in lines {
                    out.push_str(&ln);
                    out.push('\n');
                }
            }
        }
    }
    out.push_str("*** End Patch");
    out
}

fn normalize_v4a_hunk_header(line: &str) -> Option<String> {
    let after_at = line.strip_prefix("@@ ")?;
    if after_at.trim().is_empty() {
        return Some("@@".to_owned());
    }
    let after_minus = after_at.strip_prefix('-')?;
    let (old_range, rest) = after_minus.split_once(' ')?;
    if old_range.is_empty() || !old_range.bytes().all(|b| b.is_ascii_digit() || b == b',') {
        return None;
    }
    let after_plus = rest.strip_prefix('+')?;
    let (new_range, after_ranges) = after_plus.split_once(' ')?;
    if new_range.is_empty() || !new_range.bytes().all(|b| b.is_ascii_digit() || b == b',') {
        return None;
    }
    let trailing = after_ranges.strip_prefix("@@")?.trim();
    if trailing.is_empty() {
        Some("@@".to_owned())
    } else {
        Some(format!("@@ {trailing}"))
    }
}

fn merge_v4a_update_lines(existing: &mut Vec<String>, extra: &mut Vec<String>) {
    let existing_ends_eof = existing.last().is_some_and(|l| l == "*** End of File");
    let extra_ends_eof = extra.last().is_some_and(|l| l == "*** End of File");
    if existing_ends_eof && !extra_ends_eof {
        let existing_starts_hunk = existing
            .first()
            .is_some_and(|l| l == "@@" || l.starts_with("@@ "));
        if !existing_starts_hunk {
            extra.push("@@".to_owned());
        }
        extra.append(existing);
        *existing = mem::take(extra);
        return;
    }
    if existing_ends_eof && extra_ends_eof {
        let _ = existing.pop();
    }
    let starts_with_hunk = extra
        .first()
        .is_some_and(|l| l == "@@" || l.starts_with("@@ "));
    if !existing.is_empty() && !starts_with_hunk {
        existing.push("@@".to_owned());
    }
    existing.append(extra);
}

fn try_convert_unified_diff_to_v4a(text: &str) -> Option<String> {
    if !(text.starts_with("diff --git ") || text.starts_with("--- ") || text.contains("\n--- "))
        || !text.contains("\n+++ ")
    {
        return None;
    }

    let strip_diff_path = |raw: &str, prefix: &str| -> Option<String> {
        let token = raw.split('\t').next().unwrap_or(raw).trim();
        if token == "/dev/null" {
            return None;
        }
        Some(token.strip_prefix(prefix).unwrap_or(token).to_owned())
    };

    let mut out = String::from("*** Begin Patch\n");
    let mut pending_old: Option<Option<String>> = None;
    let mut current_mode: Option<&'static str> = None;
    let mut emitted_ops = 0usize;

    let mut iter = text.split('\n').peekable();
    while let Some(line) = iter.next() {
        if line.starts_with("diff --git ") {
            current_mode = None;
            pending_old = None;
            continue;
        }
        if line.starts_with("index ")
            || line.starts_with("new file mode ")
            || line.starts_with("deleted file mode ")
            || line.starts_with("similarity index ")
            || line.starts_with("rename from ")
            || line.starts_with("rename to ")
            || line.starts_with("\\ No newline at end of file")
        {
            continue;
        }
        if let Some(old_raw) = line.strip_prefix("--- ")
            && iter.peek().is_some_and(|next| next.starts_with("+++ "))
        {
            pending_old = Some(strip_diff_path(old_raw, "a/"));
            current_mode = None;
            continue;
        }
        if let Some(old_opt) = pending_old.take()
            && let Some(new_raw) = line.strip_prefix("+++ ")
        {
            let new_opt = strip_diff_path(new_raw, "b/");
            match (old_opt, new_opt) {
                (None, Some(new_p)) => {
                    out.push_str("*** Add File: ");
                    out.push_str(&new_p);
                    out.push('\n');
                    current_mode = Some("add");
                    emitted_ops += 1;
                }
                (Some(old_p), None) => {
                    out.push_str("*** Delete File: ");
                    out.push_str(&old_p);
                    out.push('\n');
                    current_mode = Some("delete");
                    emitted_ops += 1;
                }
                (Some(old_p), Some(new_p)) => {
                    out.push_str("*** Update File: ");
                    out.push_str(&old_p);
                    out.push('\n');
                    if old_p != new_p {
                        out.push_str("*** Move to: ");
                        out.push_str(&new_p);
                        out.push('\n');
                    }
                    current_mode = Some("update");
                    emitted_ops += 1;
                }
                (None, None) => return None,
            }
            continue;
        }
        if let Some(after_at) = line.strip_prefix("@@ ") {
            if current_mode == Some("update") {
                if let Some((_, ctx)) = after_at.split_once("@@") {
                    let ctx_trimmed = ctx.trim();
                    if ctx_trimmed.is_empty() {
                        out.push_str("@@\n");
                    } else {
                        out.push_str("@@ ");
                        out.push_str(ctx_trimmed);
                        out.push('\n');
                    }
                } else {
                    out.push_str("@@\n");
                }
            }
            continue;
        }
        match current_mode {
            Some("add") => {
                if line.starts_with('+') {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            Some("update") => {
                if line.starts_with('+')
                    || line.starts_with('-')
                    || line.starts_with(' ')
                    || line.is_empty()
                {
                    if line.is_empty() {
                        out.push_str(" \n");
                    } else {
                        out.push_str(line);
                        out.push('\n');
                    }
                }
            }
            Some("delete" | _) | None => {}
        }
    }

    if emitted_ops == 0 {
        return None;
    }
    out.push_str("*** End Patch");
    Some(out)
}

fn looks_like_code_mode_js(src: &str) -> bool {
    src.starts_with("// @exec:")
        || src.contains("tools.")
        || src.contains("globalThis.tools")
        || src.contains("ALL_TOOLS")
        || src.contains("yield_control(")
        || ((src.contains("await ") || src.starts_with("const ") || src.starts_with("let "))
            && (src.contains("text(")
                || src.contains("store(")
                || src.contains("load(")
                || src.contains("exit(")
                || src.contains("notify(")))
}

fn take_quoted_json_string(input: &str) -> Option<(String, &str)> {
    let body = input.strip_prefix('"')?;
    let mut escaped = false;
    for (idx, ch) in body.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            let (raw_inner, with_quote) = body.split_at_checked(idx)?;
            let (_, rest) = with_quote.split_at_checked(1)?;
            return Some((unescape_lenient_json_string(raw_inner), rest));
        }
    }
    None
}

fn unescape_lenient_json_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some((_, '"')) => out.push('"'),
            Some((_, '\\')) => out.push('\\'),
            Some((_, '/')) => out.push('/'),
            Some((_, 'b')) => out.push('\u{0008}'),
            Some((_, 'f')) => out.push('\u{000C}'),
            Some((_, 'n')) => out.push('\n'),
            Some((_, 'r')) => out.push('\r'),
            Some((_, 't')) => out.push('\t'),
            Some((u_idx, 'u')) => {
                if let Some(hex4) = raw.get(u_idx + 1..u_idx + 5)
                    && let Ok(code) = u16::from_str_radix(hex4, 16)
                {
                    if let Some(decoded) = char::from_u32(u32::from(code)) {
                        for _ in 0..4 {
                            let _ = chars.next();
                        }
                        out.push(decoded);
                    } else if (0xD800..=0xDBFF).contains(&code)
                        && raw.get(u_idx + 5..u_idx + 7) == Some("\\u")
                        && let Some(lo_hex) = raw.get(u_idx + 7..u_idx + 11)
                        && let Ok(lo) = u16::from_str_radix(lo_hex, 16)
                        && (0xDC00..=0xDFFF).contains(&lo)
                        && let Some(pair_ch) = char::from_u32(
                            0x10000
                                + (((u32::from(code) - 0xD800) << 10) | (u32::from(lo) - 0xDC00)),
                        )
                    {
                        for _ in 0..10 {
                            let _ = chars.next();
                        }
                        out.push(pair_ch);
                    } else {
                        out.push_str("\\u");
                    }
                } else {
                    out.push_str("\\u");
                }
            }
            Some((_, other)) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_chunks_across_tag_boundaries() {
        let mut parser = IncrementalToolParser::new("9f4a", true);
        let full = "Checking files...\n<agy_tool_call_9f4a>\n{\"name\":\"Bash\",\"arguments\":{\"command\":\"cargo test\"}}\n</agy_tool_call_9f4a>Done";
        let mut items = Vec::new();

        let mut cursor = full;
        while !cursor.is_empty() {
            let step = cursor.len().min(3);
            let (head, tail) = cursor
                .split_at_checked(step)
                .expect("ASCII test string splits cleanly");
            parser.feed(head, &mut items);
            cursor = tail;
        }
        parser.finish(&mut items);

        let mut text = String::new();
        let mut starts = Vec::new();
        let mut calls = Vec::new();
        for item in items {
            match item {
                ParsedStreamItem::TextDelta(delta) => text.push_str(&delta),
                ParsedStreamItem::ToolCallStart { name, is_raw_input } => {
                    starts.push((name, is_raw_input));
                }
                ParsedStreamItem::ToolCallDelta(delta) => {
                    panic!("function arguments object should not stream raw deltas: {delta}");
                }
                ParsedStreamItem::ToolCall(call) => calls.push(call),
                ParsedStreamItem::ToolCallAborted { name } => {
                    panic!("unexpected aborted tool call: {name}");
                }
            }
        }

        assert_eq!(text, "Checking files...\nDone");
        assert_eq!(starts, vec![("Bash".to_owned(), false)]);
        assert_eq!(
            calls,
            vec![ParsedToolCall {
                name: "Bash".to_owned(),
                arguments: "{\"command\":\"cargo test\"}".to_owned(),
                is_raw_input: false,
            }]
        );
    }

    #[test]
    fn parses_open_tag_with_attributes_and_parallel_calls() {
        let mut parser = IncrementalToolParser::new("abcd", true);
        let payload = "<agy_tool_call_abcd call_id=\"toolu_1\">{\"name\":\"Read\",\"arguments\":{\"file_path\":\"Cargo.toml\"}}</agy_tool_call_abcd>\n<agy_tool_call_abcd>{\"name\":\"Glob\",\"arguments\":{\"pattern\":\"**/*.rs\"}}</agy_tool_call_abcd>";
        let mut items = Vec::new();
        parser.feed(payload, &mut items);
        parser.finish(&mut items);

        let calls: Vec<ParsedToolCall> = items
            .into_iter()
            .filter_map(|it| match it {
                ParsedStreamItem::ToolCall(c) => Some(c),
                ParsedStreamItem::TextDelta(_)
                | ParsedStreamItem::ToolCallStart { .. }
                | ParsedStreamItem::ToolCallDelta(_)
                | ParsedStreamItem::ToolCallAborted { .. } => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls.first().map(|c| c.name.as_str()), Some("Read"));
        assert_eq!(calls.get(1).map(|c| c.name.as_str()), Some("Glob"));
    }

    #[test]
    fn parses_custom_tool_raw_input_and_fenced_json() {
        let mut parser = IncrementalToolParser::new("abcd", true);
        let payload = "<agy_tool_call_abcd>```json\n{\"name\":\"apply_patch\",\"input\":\"*** Begin Patch\\n*** End Patch\"}\n```</agy_tool_call_abcd>";
        let mut items = Vec::new();
        parser.feed(payload, &mut items);
        parser.finish(&mut items);

        assert_eq!(
            items,
            vec![ParsedStreamItem::ToolCall(ParsedToolCall {
                name: "apply_patch".to_owned(),
                arguments: "*** Begin Patch\n*** End Patch".to_owned(),
                is_raw_input: true,
            })]
        );
    }

    #[test]
    fn repairs_invalid_backslash_escapes_in_powershell_and_regex_args() {
        let mut parser = IncrementalToolParser::new("f032d02ecd5b", true);
        let payload = concat!(
            "我直接检索会话记录中的用户角色条目和 `bad` 关键字。\n",
            "<agy_tool_call_f032d02ecd5b>\n",
            r#"{"name": "exec_command", "arguments": {"cmd": "$f = \"$HOME\\.codex\\sessions\\rollout.jsonl\"; if ($c.text -notmatch '^# AGENTS\.md' -and $p -match '\users\demo\d+') { Write-Output \"ok\" }", "workdir": "D:\\rust_pro\\agy"}}"#,
            "\n</agy_tool_call_f032d02ecd5b>"
        );
        let mut items = Vec::new();
        parser.feed(payload, &mut items);
        parser.finish(&mut items);

        let mut text = String::new();
        let mut calls = Vec::new();
        for item in items {
            match item {
                ParsedStreamItem::TextDelta(d) => text.push_str(&d),
                ParsedStreamItem::ToolCall(c) => calls.push(c),
                ParsedStreamItem::ToolCallStart { .. }
                | ParsedStreamItem::ToolCallDelta(_)
                | ParsedStreamItem::ToolCallAborted { .. } => {}
            }
        }
        assert_eq!(
            text,
            "我直接检索会话记录中的用户角色条目和 `bad` 关键字。\n"
        );
        assert_eq!(calls.len(), 1);
        let call = calls.first().expect("parsed tool call");
        assert_eq!(call.name, "exec_command");
        assert!(!call.is_raw_input);

        let parsed_args: serde_json::Value =
            serde_json::from_str(&call.arguments).expect("repaired arguments must be valid JSON");
        let cmd = parsed_args["cmd"].as_str().expect("cmd string");
        assert!(cmd.contains(r"'^# AGENTS\.md'"));
        assert!(cmd.contains(r"\users\demo\d+"));
        assert_eq!(parsed_args["workdir"], r"D:\rust_pro\agy");
    }

    #[test]
    fn repairs_raw_newlines_in_apply_patch_and_strips_inline_backtick_wrapper_across_chunks() {
        let mut parser = IncrementalToolParser::new("f032d02ecd5b", true);
        // Exact reproduction of rollout L219/L262: inline code spans before `。`, then an unclosed
        // backtick wrapping `<agy_tool_call_...>` AND a raw unescaped LF inside `"input"`.
        let payload = concat!(
            "我现在更新 `crates/liftgate-server/src/routes.rs` 和 `tests.rs`，加入测试。",
            "`<agy_tool_call_f032d02ecd5b>\n",
            "{\"name\": \"apply_patch\", \"input\": \"*** Begin Patch\\n*** Update File: src/routes.rs\\n@@\\n+#[derive(Clone, Debug)]\n+pub struct AdminShutdownHandle {}\\n*** End Patch\"}\n",
            "</agy_tool_call_f032d02ecd5b>`"
        );

        let mut items = Vec::new();
        let mut cursor = payload;
        while !cursor.is_empty() {
            let mut step = cursor.len().min(4);
            while step < cursor.len() && !cursor.is_char_boundary(step) {
                step += 1;
            }
            let (head, tail) = cursor.split_at_checked(step).expect("valid char boundary");
            parser.feed(head, &mut items);
            cursor = tail;
        }
        parser.finish(&mut items);

        let mut text = String::new();
        let mut starts = Vec::new();
        let mut streamed_input = String::new();
        let mut delta_count = 0usize;
        let mut calls = Vec::new();
        for item in items {
            match item {
                ParsedStreamItem::TextDelta(d) => text.push_str(&d),
                ParsedStreamItem::ToolCallStart { name, is_raw_input } => {
                    starts.push((name, is_raw_input));
                }
                ParsedStreamItem::ToolCallDelta(d) => {
                    delta_count += 1;
                    streamed_input.push_str(&d);
                }
                ParsedStreamItem::ToolCall(c) => calls.push(c),
                ParsedStreamItem::ToolCallAborted { name } => {
                    panic!("unexpected aborted tool call: {name}");
                }
            }
        }
        assert_eq!(
            text,
            "我现在更新 `crates/liftgate-server/src/routes.rs` 和 `tests.rs`，加入测试。"
        );
        assert_eq!(starts, vec![("apply_patch".to_owned(), true)]);
        assert!(
            delta_count > 5,
            "expected incremental streaming deltas across 4-byte chunks, got {delta_count}"
        );
        let expected_patch = "*** Begin Patch\n*** Update File: src/routes.rs\n@@\n+#[derive(Clone, Debug)]\n+pub struct AdminShutdownHandle {}\n*** End Patch";
        assert_eq!(streamed_input, expected_patch);
        assert_eq!(
            calls,
            vec![ParsedToolCall {
                name: "apply_patch".to_owned(),
                arguments: expected_patch.to_owned(),
                is_raw_input: true,
            }]
        );
    }

    #[test]
    fn preserves_closing_backtick_before_tag_and_strips_fenced_xml_wrapper() {
        let mut parser = IncrementalToolParser::new("abcd", true);
        let payload = "Updated `routes.rs`\n```xml\n<agy_tool_call_abcd>\n{\"name\":\"Read\",\"arguments\":{\"file_path\":\"routes.rs\",}}\n</agy_tool_call_abcd>\n```";
        let mut items = Vec::new();
        parser.feed(payload, &mut items);
        parser.finish(&mut items);

        let mut text = String::new();
        let mut calls = Vec::new();
        for item in items {
            match item {
                ParsedStreamItem::TextDelta(d) => text.push_str(&d),
                ParsedStreamItem::ToolCall(c) => calls.push(c),
                ParsedStreamItem::ToolCallStart { .. }
                | ParsedStreamItem::ToolCallDelta(_)
                | ParsedStreamItem::ToolCallAborted { .. } => {}
            }
        }
        assert_eq!(text, "Updated `routes.rs`\n");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "Read");
        assert_eq!(calls[0].arguments, "{\"file_path\":\"routes.rs\"}");
    }

    #[test]
    fn salvages_apply_patch_with_unescaped_inner_quotes() {
        let mut parser = IncrementalToolParser::new("abcd", true);
        let payload = "<agy_tool_call_abcd>\n{\"name\": \"apply_patch\", \"input\": \"*** Begin Patch\\n*** Update File: src/lib.rs\\n@@\\n+pub const HEADER: &str = \"x-token\";\\n*** End Patch\"}\n</agy_tool_call_abcd>";
        let mut items = Vec::new();
        parser.feed(payload, &mut items);
        parser.finish(&mut items);

        assert_eq!(
            items,
            vec![ParsedStreamItem::ToolCall(ParsedToolCall {
                name: "apply_patch".to_owned(),
                arguments: "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n+pub const HEADER: &str = \"x-token\";\n*** End Patch".to_owned(),
                is_raw_input: true,
            })]
        );
    }

    #[test]
    fn streams_quoted_input_surrogate_pairs_escapes_and_extra_fields_across_tiny_chunks() {
        let mut parser = IncrementalToolParser::new("abcd", true);
        // Exercises: escaped quotes `\"`, invalid escape `\users`, UTF-16 surrogate pair `\uD83D\uDE00` (😀),
        // unescaped inner quotes (exercising salvage flush of remainder), and trailing JSON field `"extra": 1`
        // in strict mode.
        let strict_payload = "<agy_tool_call_abcd>{\"name\": \"apply_patch\", \"input\": \"*** Begin Patch\\n+let icon = \\\"\\uD83D\\uDE00\\\";\\n+let p = \\users\\demo;\\n*** End Patch\", \"extra\": 1}</agy_tool_call_abcd>";
        let mut items = Vec::new();
        let mut cursor = strict_payload;
        while !cursor.is_empty() {
            let mut step = 1;
            while step < cursor.len() && !cursor.is_char_boundary(step) {
                step += 1;
            }
            let (head, tail) = cursor.split_at_checked(step).expect("char boundary");
            parser.feed(head, &mut items);
            cursor = tail;
        }
        parser.finish(&mut items);

        let mut streamed = String::new();
        let mut saw_start = false;
        let mut final_call = None;
        for item in items {
            match item {
                ParsedStreamItem::TextDelta(t) => panic!("unexpected text delta: {t}"),
                ParsedStreamItem::ToolCallStart { name, is_raw_input } => {
                    assert_eq!(name, "apply_patch");
                    assert!(is_raw_input);
                    assert!(streamed.is_empty(), "ToolCallStart must precede deltas");
                    saw_start = true;
                }
                ParsedStreamItem::ToolCallDelta(d) => {
                    assert!(saw_start);
                    streamed.push_str(&d);
                }
                ParsedStreamItem::ToolCall(c) => {
                    final_call = Some(c);
                }
                ParsedStreamItem::ToolCallAborted { name } => {
                    panic!("unexpected abort: {name}");
                }
            }
        }
        let call = final_call.expect("completed tool call");
        assert_eq!(
            call.arguments,
            "*** Begin Patch\n+let icon = \"😀\";\n+let p = \\users\\demo;\n*** End Patch"
        );
        assert_eq!(streamed, call.arguments);

        // Also verify unescaped inner quotes with surrogate pairs across 2-byte chunks
        let mut parser2 = IncrementalToolParser::new("abcd", true);
        let salvaged_payload = "<agy_tool_call_abcd>{\"name\": \"apply_patch\", \"input\": \"*** Begin Patch\\n+const EMOJI: &str = \\uD83D\\uDE00 \"raw\";\\n*** End Patch\"}</agy_tool_call_abcd>";
        let mut items2 = Vec::new();
        let mut cursor2 = salvaged_payload;
        while !cursor2.is_empty() {
            let mut step = cursor2.len().min(2);
            while step < cursor2.len() && !cursor2.is_char_boundary(step) {
                step += 1;
            }
            let (head, tail) = cursor2.split_at_checked(step).expect("char boundary");
            parser2.feed(head, &mut items2);
            cursor2 = tail;
        }
        parser2.finish(&mut items2);

        let mut streamed2 = String::new();
        let mut call2 = None;
        for item in items2 {
            match item {
                ParsedStreamItem::ToolCallStart { name, is_raw_input } => {
                    assert_eq!(name, "apply_patch");
                    assert!(is_raw_input);
                }
                ParsedStreamItem::ToolCallDelta(d) => streamed2.push_str(&d),
                ParsedStreamItem::ToolCall(c) => call2 = Some(c),
                ParsedStreamItem::TextDelta(t) => panic!("unexpected text: {t}"),
                ParsedStreamItem::ToolCallAborted { name } => panic!("unexpected abort: {name}"),
            }
        }
        let call2 = call2.expect("salvaged tool call");
        assert_eq!(
            call2.arguments,
            "*** Begin Patch\n+const EMOJI: &str = 😀 \"raw\";\n*** End Patch"
        );
        assert_eq!(streamed2, call2.arguments);
    }

    #[test]
    fn streams_bare_patch_and_exec_bodies_and_aborts_truncated_calls() {
        let mut parser = IncrementalToolParser::new("abcd", true);
        let bare_patch = "<agy_tool_call_abcd>\n```diff\n*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** End Patch\n```\n</agy_tool_call_abcd>";
        let mut items = Vec::new();
        let mut cursor = bare_patch;
        while !cursor.is_empty() {
            let step = cursor.len().min(5);
            let (head, tail) = cursor.split_at_checked(step).expect("ASCII");
            parser.feed(head, &mut items);
            cursor = tail;
        }
        parser.finish(&mut items);

        let mut streamed = String::new();
        let mut call = None;
        for item in items {
            match item {
                ParsedStreamItem::ToolCallStart { name, is_raw_input } => {
                    assert_eq!(name, "apply_patch");
                    assert!(is_raw_input);
                }
                ParsedStreamItem::ToolCallDelta(d) => streamed.push_str(&d),
                ParsedStreamItem::ToolCall(c) => call = Some(c),
                ParsedStreamItem::TextDelta(t) => panic!("unexpected text: {t}"),
                ParsedStreamItem::ToolCallAborted { name } => panic!("unexpected abort: {name}"),
            }
        }
        let call = call.expect("bare patch call");
        assert_eq!(streamed, call.arguments);
        assert_eq!(
            call.arguments,
            "*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** End Patch"
        );

        // Truncated in-progress call at EOF emits ToolCallAborted instead of leaking raw XML into assistant text
        let mut trunc_parser = IncrementalToolParser::new("abcd", true);
        let mut trunc_items = Vec::new();
        trunc_parser.feed(
            "Intro\n<agy_tool_call_abcd>{\"name\": \"apply_patch\", \"input\": \"*** Begin Patch\\n+half",
            &mut trunc_items,
        );
        trunc_parser.finish(&mut trunc_items);
        assert_eq!(
            trunc_items,
            vec![
                ParsedStreamItem::TextDelta("Intro\n".to_owned()),
                ParsedStreamItem::ToolCallStart {
                    name: "apply_patch".to_owned(),
                    is_raw_input: true,
                },
                ParsedStreamItem::ToolCallDelta("*** Begin Patch\n+half".to_owned()),
                ParsedStreamItem::ToolCallAborted {
                    name: "apply_patch".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn parses_codex_code_mode_exec_envelopes_and_raw_js_fallback() {
        // 1. Standard JSON envelope with pragma, regexes, template literals, and arrow comparisons
        let mut p1 = IncrementalToolParser::new("9c2e", true);
        let js_src = "// @exec: {\"yield_time_ms\": 5000}\nconst [a, b] = await Promise.all([\n  tools.exec_command({ cmd: \"rg --files crates\" }),\n  tools.exec_command({ cmd: \"git status --short\" }),\n]);\nconst lines = String(a.output ?? \"\").split(/\\r?\\n/).filter((x) => x.length <= 80);\nstore(\"files\", lines);\ntext(`found ${lines.length} files`);";
        let env1 = format!(
            "<agy_tool_call_9c2e>\n{}\n</agy_tool_call_9c2e>",
            serde_json::json!({ "name": "exec", "input": js_src })
        );
        let mut out1 = Vec::new();
        p1.feed(&env1, &mut out1);
        p1.finish(&mut out1);
        assert_eq!(
            out1,
            vec![ParsedStreamItem::ToolCall(ParsedToolCall {
                name: "exec".to_owned(),
                arguments: js_src.to_owned(),
                is_raw_input: true,
            })]
        );

        // 2. Salvages `exec` envelope with unescaped inner quotes + raw newlines
        let mut p2 = IncrementalToolParser::new("9c2e", true);
        let env2 = "<agy_tool_call_9c2e>\n{\"name\": \"exec\", \"input\": \"const r = await tools.exec_command({ cmd: \"cargo --version\" });\ntext(r.output);\"}\n</agy_tool_call_9c2e>";
        let mut out2 = Vec::new();
        p2.feed(env2, &mut out2);
        p2.finish(&mut out2);
        assert_eq!(
            out2,
            vec![ParsedStreamItem::ToolCall(ParsedToolCall {
                name: "exec".to_owned(),
                arguments: "const r = await tools.exec_command({ cmd: \"cargo --version\" });\ntext(r.output);".to_owned(),
                is_raw_input: true,
            })]
        );

        // 3. Salvages bare JavaScript source inside `<agy_tool_call_...>` if model took `exec`
        //    `"Accepts raw JavaScript source text, not JSON"` literally
        let mut p3 = IncrementalToolParser::new("9c2e", true);
        let env3 = "<agy_tool_call_9c2e>\n// @exec: {\"yield_time_ms\": 10000}\nconst res = await tools.exec_command({ cmd: \"cargo test\" });\ntext(res.output);\n</agy_tool_call_9c2e>";
        let mut out3 = Vec::new();
        p3.feed(env3, &mut out3);
        p3.finish(&mut out3);
        assert_eq!(
            out3,
            vec![ParsedStreamItem::ToolCall(ParsedToolCall {
                name: "exec".to_owned(),
                arguments: "// @exec: {\"yield_time_ms\": 10000}\nconst res = await tools.exec_command({ cmd: \"cargo test\" });\ntext(res.output);".to_owned(),
                is_raw_input: true,
            })]
        );
    }

    #[test]
    fn salvages_claude_code_mode_repl_with_unescaped_quotes_and_bun_shell() {
        let mut parser = IncrementalToolParser::new("abcd", true);
        let payload = "<agy_tool_call_abcd>\n{\"name\": \"REPL\", \"arguments\": {\"code\": \"const out = await $`rg \"SessionPool\" crates/ -l`.text();\nconsole.log(out);\"}}\n</agy_tool_call_abcd>";
        let mut items = Vec::new();
        parser.feed(payload, &mut items);
        parser.finish(&mut items);

        assert_eq!(items.len(), 1);
        let Some(ParsedStreamItem::ToolCall(call)) = items.first() else {
            panic!("expected tool call");
        };
        assert_eq!(call.name, "REPL");
        assert!(!call.is_raw_input);
        let parsed: serde_json::Value =
            serde_json::from_str(&call.arguments).expect("valid REPL JSON arguments");
        assert_eq!(
            parsed["code"],
            "const out = await $`rg \"SessionPool\" crates/ -l`.text();\nconsole.log(out);"
        );
    }

    #[test]
    fn lone_utf16_surrogate_in_quoted_input_falls_back_to_lenient_salvage_matching_deltas() {
        let mut parser = IncrementalToolParser::new("cafe", true);
        let raw = "<agy_tool_call_cafe>{\"name\":\"apply_patch\",\"input\":\"*** Begin Patch\\n+bad \\uD800 mid \\uDC00 end\\n*** End Patch\"}</agy_tool_call_cafe>";
        let mut items = Vec::new();
        for chunk in raw.as_bytes().chunks(3) {
            let s = core::str::from_utf8(chunk).expect("ascii chunks");
            parser.feed(s, &mut items);
        }
        parser.finish(&mut items);

        let mut streamed = String::new();
        let mut final_call = None;
        for item in items {
            match item {
                ParsedStreamItem::ToolCallDelta(d) => streamed.push_str(&d),
                ParsedStreamItem::ToolCall(c) => final_call = Some(c),
                ParsedStreamItem::ToolCallStart { .. }
                | ParsedStreamItem::TextDelta(_)
                | ParsedStreamItem::ToolCallAborted { .. } => {}
            }
        }
        let call = final_call.expect("salvaged tool call");
        assert_eq!(call.name, "apply_patch");
        assert!(call.is_raw_input);
        assert_eq!(
            call.arguments,
            "*** Begin Patch\n+bad \\uD800 mid \\uDC00 end\n*** End Patch"
        );
        assert_eq!(streamed, call.arguments);
    }

    #[test]
    fn apply_patch_accepts_bare_multiline_header_prefix_and_json_object_wrappers() {
        let expected_patch = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-old(\"hi\")\n+new(\"hi\")\n*** End Patch";

        let cases = [
            format!("<agy_tool_call_cafe>\n{expected_patch}\n</agy_tool_call_cafe>"),
            format!("<agy_tool_call_cafe>apply_patch\n{expected_patch}</agy_tool_call_cafe>"),
            format!(
                "<agy_tool_call_cafe>{{\"name\": \"apply_patch\"}}\n```diff\r\n{}\r\n```</agy_tool_call_cafe>",
                expected_patch.replace('\n', "\r\n")
            ),
            "<agy_tool_call_cafe>{\"name\":\"apply_patch\",\"arguments\":{\"patch\":\"*** Begin Patch\\n*** Update File: src/lib.rs\\n@@\\n-old(\\\"hi\\\")\\n+new(\\\"hi\\\")\\n*** End Patch\"}}</agy_tool_call_cafe>".to_owned(),
        ];

        for (idx, raw) in cases.iter().enumerate() {
            let mut parser = IncrementalToolParser::new("cafe", true);
            let mut items = Vec::new();
            for chunk in raw.as_bytes().chunks(5) {
                let s = core::str::from_utf8(chunk).expect("ascii chunks");
                parser.feed(s, &mut items);
            }
            parser.finish(&mut items);

            let mut streamed = String::new();
            let mut final_call = None;
            for item in items {
                match item {
                    ParsedStreamItem::ToolCallDelta(d) => streamed.push_str(&d),
                    ParsedStreamItem::ToolCall(c) => final_call = Some(c),
                    ParsedStreamItem::ToolCallStart { .. }
                    | ParsedStreamItem::TextDelta(_)
                    | ParsedStreamItem::ToolCallAborted { .. } => {}
                }
            }
            let call = final_call.unwrap_or_else(|| panic!("case {idx} failed to parse"));
            assert_eq!(call.name, "apply_patch", "case {idx}");
            assert!(call.is_raw_input, "case {idx}");
            assert_eq!(call.arguments, expected_patch, "case {idx}");
            if !streamed.is_empty() {
                assert_eq!(
                    streamed, expected_patch,
                    "case {idx} streamed delta mismatch"
                );
            }
        }
    }

    #[test]
    fn normalize_and_validate_apply_patch_coalesces_duplicates_repairs_blank_lines_and_converts_unified_diff()
     {
        let dup_and_blank = "*** Update File: src/router.rs\n@@ -40,2 +40,2 @@\n-let tail = 1;\n+let tail = 2;\n*** End of File\n*** Update File: src/router.rs\n@@ -10,4 +10,4 @@ fn earlier()\n-let a = 1;\n+let a = 2;\n\n let keep = true;";
        let repaired = normalize_apply_patch_text(dup_and_blank);
        assert_eq!(
            repaired,
            "*** Begin Patch\n*** Update File: src/router.rs\n@@ fn earlier()\n-let a = 1;\n+let a = 2;\n \n let keep = true;\n@@\n-let tail = 1;\n+let tail = 2;\n*** End of File\n*** End Patch"
        );
        assert!(validate_apply_patch_preflight(&repaired).is_ok());

        let unified = "diff --git a/src/lib.rs b/src/lib.rs\nindex 111..222 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -10,4 +10,4 @@ pub fn run() {\n let x = 1;\n--- legacy comment\n-let y = 2;\n+let y = 3;\n";
        let v4a = normalize_apply_patch_text(unified);
        assert_eq!(
            v4a,
            "*** Begin Patch\n*** Update File: src/lib.rs\n@@ pub fn run() {\n let x = 1;\n--- legacy comment\n-let y = 2;\n+let y = 3;\n*** End Patch"
        );
        assert!(validate_apply_patch_preflight(&v4a).is_ok());

        let bad_ctx = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\nlet missing_prefix = 1;\n-old\n+new\n*** End Patch";
        let err = validate_apply_patch_preflight(bad_ctx)
            .expect_err("missing context prefix must fail preflight");
        assert!(
            matches!(
                &err,
                ApplyPatchPreflightError::InvalidUpdateHunkLine { line_no: 4, .. }
            ),
            "got err: {err:?}"
        );
        assert!(err.to_string().contains("line 4"), "got err: {err}");

        let bad_eof_mid = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-a\n+b\n*** End of File\n@@\n-c\n+d\n*** End Patch";
        assert!(matches!(
            validate_apply_patch_preflight(bad_eof_mid),
            Err(ApplyPatchPreflightError::TrailingLinesAfterEof { line_no: 7, .. })
        ));
    }

    #[test]
    fn normalize_code_mode_exec_js_heals_unescaped_backticks_in_v4a_templates_and_fences() {
        // 1. Plain JS without V4A patches stays borrowed with trailing whitespace preserved.
        let plain =
            "const r = await tools.exec_command({ cmd: `echo ${1 + 1}` });\ntext(r.output);\n";
        assert!(matches!(
            normalize_code_mode_exec_js(plain),
            std::borrow::Cow::Borrowed(_)
        ));

        // 2. Accidental outer markdown fence is stripped cleanly.
        let fenced = "```javascript\ntext('ok');\n```";
        assert_eq!(normalize_code_mode_exec_js(fenced).as_ref(), "text('ok');");

        // 3. Unescaped backticks inside a V4A template literal are escaped while `${wt}` and
        //    already-escaped `\`exec\`` remain intact, across multi-patch arrays too.
        let broken = "const patches = [\n  `*** Begin Patch\n*** Update File: ${wt}/CHANGELOG.md\n@@\n+- **Codex `exec` `JSON.parse` fix**: keep \\`escaped\\` intact\n*** End Patch`,\n  `*** Begin Patch\n*** Update File: ${wt}/README.md\n@@\n+- Run `lg cx`\n*** End Patch`\n];\nawait Promise.allSettled(patches.map(p => tools.apply_patch(p)));";
        let expected = "const patches = [\n  `*** Begin Patch\n*** Update File: ${wt}/CHANGELOG.md\n@@\n+- **Codex \\`exec\\` \\`JSON.parse\\` fix**: keep \\`escaped\\` intact\n*** End Patch`,\n  `*** Begin Patch\n*** Update File: ${wt}/README.md\n@@\n+- Run \\`lg cx\\`\n*** End Patch`\n];\nawait Promise.allSettled(patches.map(p => tools.apply_patch(p)));";
        let healed = normalize_code_mode_exec_js(broken);
        assert_eq!(healed.as_ref(), expected);
        // Idempotent when already valid.
        assert!(matches!(
            normalize_code_mode_exec_js(expected),
            std::borrow::Cow::Borrowed(_)
        ));

        // 4. Valid concatenated, array-joined, `.concat(...)`, and tagged template expressions
        //    must stay untouched (`Cow::Borrowed`).
        let valid_concat = "const p = `*** Begin Patch\\n` + `*** Update File: a.txt\\n@@\\n-old\\n+new\\n*** End Patch`;\nawait tools.apply_patch(p);";
        assert!(matches!(
            normalize_code_mode_exec_js(valid_concat),
            std::borrow::Cow::Borrowed(_)
        ));
        let valid_mid_concat = "const p = `*** Begin Patch\\n*** Update File: ` + path + `\\n@@\\n-old\\n+new\\n*** End Patch`;";
        assert!(matches!(
            normalize_code_mode_exec_js(valid_mid_concat),
            std::borrow::Cow::Borrowed(_)
        ));
        let valid_paren_mid_concat = "if (ok) {\n  const p = (`*** Begin Patch\\n*** Update File: ` + path) + `\\n@@\\n-old\\n+new\\n*** End Patch`;\n  await tools.apply_patch(p);\n}";
        assert!(matches!(
            normalize_code_mode_exec_js(valid_paren_mid_concat),
            std::borrow::Cow::Borrowed(_)
        ));
        let valid_array_join = "const p = [\n  `*** Begin Patch\\n*** Update File: a.txt`,\n  `@@\\n-old\\n+new\\n*** End Patch`,\n].join('\\n');";
        assert!(matches!(
            normalize_code_mode_exec_js(valid_array_join),
            std::borrow::Cow::Borrowed(_)
        ));
        let valid_method_concat = "const p = `*** Begin Patch\\n*** Update File: a.txt\\n`.concat(`@@\\n-old\\n+new\\n*** End Patch`);";
        assert!(matches!(
            normalize_code_mode_exec_js(valid_method_concat),
            std::borrow::Cow::Borrowed(_)
        ));
        let valid_raw_tag = "const p = String.raw`*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch`;";
        assert!(matches!(
            normalize_code_mode_exec_js(valid_raw_tag),
            std::borrow::Cow::Borrowed(_)
        ));

        // 5. Mixed script containing a valid concatenated patch, a regex literal with an
        //    unescaped backtick, a separate header template, AND a broken V4A patch heals ONLY
        //    the broken V4A patch.
        let mixed = "const clean = s.replace(/`/g, '\\\\`');\nconst hdr = `*** Begin Patch\\n`;\nconst ok = `*** Begin Patch\\n` + `*** Update File: a.txt\\n@@\\n-old\\n+new\\n*** End Patch`;\nawait tools.apply_patch(`*** Begin Patch\n*** Update File: b.md\n@@\n+- Path `/api/v1` and\n+```rust\n+fn f() {}\n+```\n*** End Patch`);";
        let mixed_expected = "const clean = s.replace(/`/g, '\\\\`');\nconst hdr = `*** Begin Patch\\n`;\nconst ok = `*** Begin Patch\\n` + `*** Update File: a.txt\\n@@\\n-old\\n+new\\n*** End Patch`;\nawait tools.apply_patch(`*** Begin Patch\n*** Update File: b.md\n@@\n+- Path \\`/api/v1\\` and\n+\\`\\`\\`rust\n+fn f() {}\n+\\`\\`\\`\n*** End Patch`);";
        assert_eq!(normalize_code_mode_exec_js(mixed).as_ref(), mixed_expected);

        // 6. Pathologically deep nested `${`${...}`}` template interpolations never recurse on
        //    the call stack and return the input unchanged (`Cow::Borrowed`) once depth exceeds
        //    `MAX_JS_TEMPLATE_NESTING`.
        let depth = 5_000_usize;
        let mut deep = String::with_capacity(depth * 5 + 64);
        deep.push_str("// *** Begin Patch\nconst x = ");
        for _ in 0..depth {
            deep.push_str("`${");
        }
        deep.push('1');
        for _ in 0..depth {
            deep.push_str("}`");
        }
        deep.push(';');
        assert!(matches!(
            normalize_code_mode_exec_js(&deep),
            std::borrow::Cow::Borrowed(_)
        ));
    }
}
