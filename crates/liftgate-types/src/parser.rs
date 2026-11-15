use std::fmt::Write as _;
use std::mem;

use serde::Deserialize;
use serde_json::value::RawValue;

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
    ToolCall(ParsedToolCall),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParserMode {
    Text,
    /// Saw `<agy_tool_call_{nonce}` without its closing `>` yet (e.g. optional XML attributes).
    InOpenTagHeader,
    InTag,
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
                if let Some(call) = parse_tool_call_body(&leftover) {
                    out.push(ParsedStreamItem::ToolCall(call));
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

                        let pending_wrapper = mem::take(&mut self.pending_open_wrapper);
                        if let Some(call) = parse_tool_call_body(&body_owned) {
                            self.strip_closing_wrapper = classify_closing_wrapper(&pending_wrapper);
                            out.push(ParsedStreamItem::ToolCall(call));
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
                        break;
                    }
                }
            }
        }
    }
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
        if raw_text.starts_with('"')
            && let Ok(inner_str) = serde_json::from_str::<String>(raw_text)
        {
            let inner_trimmed = inner_str.trim();
            if inner_trimmed.starts_with('{') {
                if serde_json::from_str::<serde::de::IgnoredAny>(inner_trimmed).is_ok() {
                    return Some(ParsedToolCall {
                        name,
                        arguments: inner_trimmed.to_owned(),
                        is_raw_input: false,
                    });
                }
                let repaired_inner = repair_llm_json(inner_trimmed);
                if serde_json::from_str::<serde::de::IgnoredAny>(&repaired_inner).is_ok() {
                    return Some(ParsedToolCall {
                        name,
                        arguments: repaired_inner,
                        is_raw_input: false,
                    });
                }
            }
            return Some(ParsedToolCall {
                name,
                arguments: inner_str,
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
        if raw_text.starts_with('"')
            && let Ok(inner_str) = serde_json::from_str::<String>(raw_text)
        {
            return Some(ParsedToolCall {
                name,
                arguments: inner_str,
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

    Some(ParsedToolCall {
        name,
        arguments: unescape_lenient_json_string(raw_body),
        is_raw_input: true,
    })
}

/// Salvages raw custom-tool bodies emitted directly inside `<agy_tool_call_{nonce}>` without a
/// JSON envelope (for example when a model obeys Codex Code Mode `exec`'s upstream prompt hint
/// `"Accepts raw JavaScript source text, not JSON"` literally, or writes a bare `*** Begin Patch`).
fn salvage_raw_custom_tool_body(cleaned: &str) -> Option<ParsedToolCall> {
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || trimmed.starts_with('{') {
        return None;
    }
    if trimmed.starts_with("*** Begin Patch") {
        return Some(ParsedToolCall {
            name: "apply_patch".to_owned(),
            arguments: trimmed.to_owned(),
            is_raw_input: true,
        });
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
                    && let Some(decoded) = char::from_u32(u32::from(code))
                {
                    for _ in 0..4 {
                        let _ = chars.next();
                    }
                    out.push(decoded);
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
        let mut calls = Vec::new();
        for item in items {
            match item {
                ParsedStreamItem::TextDelta(delta) => text.push_str(&delta),
                ParsedStreamItem::ToolCall(call) => calls.push(call),
            }
        }

        assert_eq!(text, "Checking files...\nDone");
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
                ParsedStreamItem::TextDelta(_) => None,
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
        let mut calls = Vec::new();
        for item in items {
            match item {
                ParsedStreamItem::TextDelta(d) => text.push_str(&d),
                ParsedStreamItem::ToolCall(c) => calls.push(c),
            }
        }
        assert_eq!(
            text,
            "我现在更新 `crates/liftgate-server/src/routes.rs` 和 `tests.rs`，加入测试。"
        );
        assert_eq!(
            calls,
            vec![ParsedToolCall {
                name: "apply_patch".to_owned(),
                arguments: "*** Begin Patch\n*** Update File: src/routes.rs\n@@\n+#[derive(Clone, Debug)]\n+pub struct AdminShutdownHandle {}\n*** End Patch".to_owned(),
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
}
