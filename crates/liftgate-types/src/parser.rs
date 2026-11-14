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
        if self.buffer.is_empty() && self.mode == ParserMode::Text {
            return;
        }
        let leftover = mem::take(&mut self.buffer);
        match self.mode {
            ParserMode::Text => {
                if !leftover.is_empty() {
                    out.push(ParsedStreamItem::TextDelta(leftover));
                }
            }
            ParserMode::InOpenTagHeader => {
                self.mode = ParserMode::Text;
                let mut raw = String::with_capacity(self.open_prefix.len() + leftover.len());
                raw.push_str(&self.open_prefix);
                raw.push_str(&leftover);
                out.push(ParsedStreamItem::TextDelta(raw));
            }
            ParserMode::InTag => {
                self.mode = ParserMode::Text;
                if let Some(call) = parse_tool_call_body(&leftover) {
                    out.push(ParsedStreamItem::ToolCall(call));
                } else {
                    let mut raw = String::with_capacity(self.open_tag.len() + leftover.len());
                    raw.push_str(&self.open_tag);
                    raw.push_str(&leftover);
                    out.push(ParsedStreamItem::TextDelta(raw));
                }
            }
        }
    }

    fn drain_complete_segments(&mut self, out: &mut Vec<ParsedStreamItem>) {
        loop {
            match self.mode {
                ParserMode::Text => {
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
                                out.push(ParsedStreamItem::TextDelta(before.to_owned()));
                                let keep = rest_with_prefix.to_owned();
                                self.buffer.clear();
                                self.buffer.push_str(&keep);
                            }
                            break;
                        };

                        if next_ch == '>' {
                            if !before.is_empty() {
                                out.push(ParsedStreamItem::TextDelta(before.to_owned()));
                            }
                            let Some((_, remainder)) = after_prefix.split_at_checked(1) else {
                                break;
                            };
                            let owned_rem = remainder.to_owned();
                            self.buffer.clear();
                            self.buffer.push_str(&owned_rem);
                            self.mode = ParserMode::InTag;
                        } else if next_ch.is_ascii_whitespace() {
                            if !before.is_empty() {
                                out.push(ParsedStreamItem::TextDelta(before.to_owned()));
                            }
                            let owned_after = after_prefix.to_owned();
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
                            out.push(ParsedStreamItem::TextDelta(emit_part.to_owned()));
                            let owned_rem = rem.to_owned();
                            self.buffer.clear();
                            self.buffer.push_str(&owned_rem);
                        }
                    } else {
                        let holdback = longest_tag_prefix_at_end(&self.buffer, &self.open_prefix);
                        let emit_len = self.buffer.len().saturating_sub(holdback);
                        if emit_len == 0 {
                            break;
                        }
                        let Some((emit_part, keep_part)) = self.buffer.split_at_checked(emit_len)
                        else {
                            break;
                        };
                        out.push(ParsedStreamItem::TextDelta(emit_part.to_owned()));
                        let owned_keep = keep_part.to_owned();
                        self.buffer.clear();
                        self.buffer.push_str(&owned_keep);
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
                        let mut raw =
                            String::with_capacity(self.open_prefix.len() + leftover.len());
                        raw.push_str(&self.open_prefix);
                        raw.push_str(&leftover);
                        out.push(ParsedStreamItem::TextDelta(raw));
                        self.mode = ParserMode::Text;
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

                        if let Some(call) = parse_tool_call_body(&body_owned) {
                            out.push(ParsedStreamItem::ToolCall(call));
                        } else {
                            let mut raw = String::with_capacity(
                                self.open_tag.len() + body_owned.len() + self.close_tag.len(),
                            );
                            raw.push_str(&self.open_tag);
                            raw.push_str(&body_owned);
                            raw.push_str(&self.close_tag);
                            out.push(ParsedStreamItem::TextDelta(raw));
                        }
                    } else {
                        break;
                    }
                }
            }
        }
    }
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
    let envelope: RawToolCallEnvelope<'_> = serde_json::from_str(cleaned).ok()?;
    let name = envelope.name.trim().to_owned();
    if name.is_empty() {
        return None;
    }

    if let Some(arg_raw) = envelope.arguments {
        let raw_text = arg_raw.get().trim();
        if raw_text.starts_with('"') {
            if let Ok(inner_str) = serde_json::from_str::<String>(raw_text) {
                let inner_trimmed = inner_str.trim();
                if inner_trimmed.starts_with('{')
                    && serde_json::from_str::<serde::de::IgnoredAny>(inner_trimmed).is_ok()
                {
                    return Some(ParsedToolCall {
                        name,
                        arguments: inner_trimmed.to_owned(),
                        is_raw_input: false,
                    });
                }
                return Some(ParsedToolCall {
                    name,
                    arguments: inner_str,
                    is_raw_input: true,
                });
            }
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
}
