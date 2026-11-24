//! YAML inline comment splitting, scalar unquoting, and flow collection parsers.

#[must_use]
pub fn line_indent(line: &str) -> usize {
    line.bytes().take_while(|b| *b == b' ').count()
}

pub(crate) fn has_leading_tab_indentation(line: &str) -> bool {
    line.bytes()
        .take_while(|b| matches!(*b, b' ' | b'\t'))
        .any(|b| b == b'\t')
}

/// Splits `s` into `(code_part, optional_comment_with_hash)` respecting `'...'` and `"..."` quotes.
#[must_use]
pub fn split_yaml_inline_comment(s: &str) -> (&str, Option<&str>) {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut prev_space = true;
    let mut chars = s.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if in_double {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_double = false;
            }
            prev_space = false;
            continue;
        }
        if in_single {
            if ch == '\'' {
                if chars.peek().is_some_and(|(_, next)| *next == '\'') {
                    let _ = chars.next();
                } else {
                    in_single = false;
                }
            }
            prev_space = false;
            continue;
        }
        match ch {
            '\'' => {
                in_single = true;
                prev_space = false;
            }
            '"' => {
                in_double = true;
                prev_space = false;
            }
            '#' if prev_space => {
                if let Some((before, comment)) = s.split_at_checked(idx) {
                    return (
                        before.trim_end(),
                        Some(comment.trim_end_matches(['\r', '\n'])),
                    );
                }
            }
            c => {
                prev_space = c.is_whitespace();
            }
        }
    }
    (s.trim_end_matches(['\r', '\n']), None)
}

#[must_use]
pub fn strip_yaml_inline_comment(s: &str) -> &str {
    split_yaml_inline_comment(s).0
}

/// Parses `<indent><key>: <rhs>` at an exact leading space count, returning `(unquoted_key, trimmed_rhs)`.
#[must_use]
pub fn parse_yaml_mapping_key_at_indent(
    line: &str,
    expected_indent: usize,
) -> Option<(&str, &str)> {
    if line_indent(line) != expected_indent {
        return None;
    }
    let trimmed = strip_yaml_inline_comment(line).trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
        return None;
    }
    split_yaml_key_and_rhs(trimmed)
}

/// Splits a trimmed mapping line into `(unquoted_key, trimmed_rhs)` where `:` is followed by
/// whitespace or end-of-line (preventing false splits on URLs or timestamps).
pub fn split_yaml_key_and_rhs(trimmed: &str) -> Option<(&str, &str)> {
    if let Some(rest) = trimmed.strip_prefix('"') {
        let mut escaped = false;
        for (idx, ch) in rest.char_indices() {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                let key = rest.get(..idx)?;
                let after_quote = rest.get(idx + 1..)?.trim_start();
                let rhs = after_quote.strip_prefix(':')?;
                if !rhs.is_empty() && !rhs.starts_with(char::is_whitespace) {
                    return None;
                }
                return (!key.is_empty()).then_some((key, rhs.trim()));
            }
        }
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix('\'') {
        let mut chars = rest.char_indices().peekable();
        while let Some((idx, ch)) = chars.next() {
            if ch == '\'' {
                if chars.peek().is_some_and(|(_, next)| *next == '\'') {
                    let _ = chars.next();
                } else {
                    let key = rest.get(..idx)?;
                    let after_quote = rest.get(idx + 1..)?.trim_start();
                    let rhs = after_quote.strip_prefix(':')?;
                    if !rhs.is_empty() && !rhs.starts_with(char::is_whitespace) {
                        return None;
                    }
                    return (!key.is_empty()).then_some((key, rhs.trim()));
                }
            }
        }
        return None;
    }
    for (idx, ch) in trimmed.char_indices() {
        if ch == ':' {
            let after = trimmed.get(idx + 1..)?;
            if after.is_empty() || after.starts_with(char::is_whitespace) {
                let raw_key = trimmed.get(..idx)?.trim();
                if raw_key.is_empty() || raw_key.contains(['{', '}', '[', ']']) {
                    return None;
                }
                return Some((raw_key, after.trim()));
            }
        }
    }
    None
}

#[must_use]
pub fn unquote_yaml_scalar(raw: &str) -> String {
    let trimmed = strip_yaml_inline_comment(raw).trim();
    if trimmed.starts_with('"')
        && trimmed.ends_with('"')
        && trimmed.len() >= 2
        && let Ok(s) = serde_json::from_str::<String>(trimmed)
    {
        return s;
    }
    if let Some(inner) = trimmed
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
    {
        return inner.replace("''", "'");
    }
    trimmed.to_owned()
}

#[must_use]
pub fn ensure_lf(line: &str) -> String {
    let clean = line.trim_end_matches(['\r', '\n']);
    let mut s = String::with_capacity(clean.len() + 1);
    s.push_str(clean);
    s.push('\n');
    s
}

pub(crate) fn update_flow_depth(line_no_comment: &str, depth: &mut i32) -> Result<(), String> {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut chars = line_no_comment.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_double {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_double = false;
            }
            continue;
        }
        if in_single {
            if ch == '\'' {
                if chars.peek() == Some(&'\'') {
                    let _ = chars.next();
                } else {
                    in_single = false;
                }
            }
            continue;
        }
        match ch {
            '\'' => in_single = true,
            '"' => in_double = true,
            '{' | '[' => *depth += 1,
            '}' | ']' => {
                *depth -= 1;
                if *depth < 0 {
                    return Err("unmatched closing `}` or `]` in YAML flow collection".to_owned());
                }
            }
            _ => {}
        }
    }
    if in_single || in_double {
        return Err("unterminated quoted string in YAML line".to_owned());
    }
    Ok(())
}

/// Parses an inline YAML scalar flow map (`{commands: false, mode: 'strict'}`).
/// Rejects nested `{` / `[` collections or malformed entries.
pub fn parse_yaml_flow_scalar_map(flow_text: &str) -> Result<Vec<(String, String)>, String> {
    let trimmed = flow_text.trim();
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(|| format!("malformed YAML flow mapping `{trimmed}`"))?
        .trim();
    if inner.is_empty() {
        return Ok(Vec::new());
    }
    let items = split_flow_comma_items(inner)?;
    let mut pairs = Vec::with_capacity(items.len());
    for item in items {
        let (k, rhs) = split_yaml_key_and_rhs(item)
            .ok_or_else(|| format!("invalid entry `{item}` inside YAML flow mapping"))?;
        if rhs.is_empty() {
            return Err(format!(
                "missing scalar value for `{k}` inside YAML flow mapping"
            ));
        }
        if contains_unquoted_flow_brackets(rhs) {
            return Err(format!(
                "nested flow collection in `{k}: {rhs}` cannot be flattened as a scalar"
            ));
        }
        pairs.push((k.to_owned(), rhs.to_owned()));
    }
    Ok(pairs)
}

/// Parses a YAML `extensions:` section supporting both block `- item` sequences (at any consistent
/// indentation `>= 0`) and single-line or multi-line YAML flow lists (`[./.omp/ext.ts, './b.ts']`).
/// Returns `(items, sequence_item_indent)`.
pub fn parse_yaml_string_sequence_section(
    section_lines: &[String],
    default_indent: usize,
) -> Result<(Vec<String>, usize), String> {
    let first_line = section_lines
        .first()
        .ok_or_else(|| "empty YAML sequence section".to_owned())?;
    let code_head = strip_yaml_inline_comment(first_line).trim();
    let (sec_key, rhs_first) = split_yaml_key_and_rhs(code_head)
        .ok_or_else(|| format!("invalid YAML sequence header `{code_head}`"))?;

    let body = section_lines.get(1..).unwrap_or(&[]);
    if matches!(rhs_first, "[]" | "null" | "~") {
        if body
            .iter()
            .any(|l| !strip_yaml_inline_comment(l).trim().is_empty())
        {
            return Err(format!(
                "section `{sec_key}` combines inline `{rhs_first}` with block lines"
            ));
        }
        return Ok((Vec::new(), default_indent.max(2)));
    }

    if rhs_first.starts_with('[') {
        let mut combined = String::from(rhs_first);
        for line in body {
            let part = strip_yaml_inline_comment(line).trim();
            if !part.is_empty() {
                combined.push(' ');
                combined.push_str(part);
            }
        }
        let list = parse_yaml_flow_string_list(&combined)?;
        return Ok((list, default_indent.max(2)));
    }

    if !rhs_first.is_empty() {
        return Err(format!(
            "section `{sec_key}` has scalar value `{rhs_first}`, expected a YAML sequence"
        ));
    }

    let mut items = Vec::new();
    let mut seq_indent: Option<usize> = None;
    for line in body {
        let trimmed = strip_yaml_inline_comment(line).trim();
        if trimmed.is_empty() {
            continue;
        }
        let indent = line_indent(line);
        if let Some(expected) = seq_indent {
            if indent != expected {
                return Err(format!(
                    "section `{sec_key}` has inconsistent sequence item indentation ({indent} vs {expected})"
                ));
            }
        } else {
            seq_indent = Some(indent);
        }
        let rest = trimmed.strip_prefix('-').ok_or_else(|| {
            format!("section `{sec_key}` contains non-sequence entry `{trimmed}`")
        })?;
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return Err(format!(
                "section `{sec_key}` contains invalid sequence entry `{trimmed}`"
            ));
        }
        let item_raw = rest.trim();
        if item_raw.is_empty()
            || contains_unquoted_flow_brackets(item_raw)
            || split_yaml_key_and_rhs(item_raw).is_some()
        {
            return Err(format!(
                "section `{sec_key}` contains non-string sequence item `{trimmed}`"
            ));
        }
        items.push(unquote_yaml_scalar(item_raw));
    }

    Ok((items, seq_indent.unwrap_or_else(|| default_indent.max(2))))
}

/// Parses a YAML flow list of strings (`[./.omp/user-ext.ts, './a.ts', "./b.ts"]`).
pub fn parse_yaml_flow_string_list(flow_text: &str) -> Result<Vec<String>, String> {
    let trimmed = flow_text.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| format!("malformed YAML flow sequence `{trimmed}`"))?
        .trim();
    if inner.is_empty() {
        return Ok(Vec::new());
    }
    let raw_items = split_flow_comma_items(inner)?;
    let mut out = Vec::with_capacity(raw_items.len());
    for raw in raw_items {
        if contains_unquoted_flow_brackets(raw) {
            return Err(format!(
                "nested flow collection `{raw}` is not a valid extension path string"
            ));
        }
        let val = unquote_yaml_scalar(raw);
        if val.is_empty() {
            return Err("empty scalar inside YAML flow sequence".to_owned());
        }
        out.push(val);
    }
    Ok(out)
}

fn split_flow_comma_items(inner: &str) -> Result<Vec<&str>, String> {
    let mut items = Vec::new();
    let mut start = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut depth: i32 = 0;
    let mut chars = inner.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        if in_double {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_double = false;
            }
            continue;
        }
        if in_single {
            if ch == '\'' {
                if chars.peek().is_some_and(|(_, next)| *next == '\'') {
                    let _ = chars.next();
                } else {
                    in_single = false;
                }
            }
            continue;
        }
        match ch {
            '\'' => in_single = true,
            '"' => in_double = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth < 0 {
                    return Err("unmatched closing bracket inside YAML flow collection".to_owned());
                }
            }
            ',' if depth == 0 => {
                let seg = inner.get(start..idx).unwrap_or("").trim();
                if seg.is_empty() {
                    return Err("empty comma-separated element in YAML flow collection".to_owned());
                }
                items.push(seg);
                start = idx + 1;
            }
            _ => {}
        }
    }
    if in_single || in_double || depth != 0 {
        return Err("unterminated quote or bracket in YAML flow collection".to_owned());
    }
    let tail = inner.get(start..).unwrap_or("").trim();
    if !tail.is_empty() {
        items.push(tail);
    }
    Ok(items)
}

fn contains_unquoted_flow_brackets(raw: &str) -> bool {
    let trimmed = raw.trim();
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        return false;
    }
    trimmed.contains(['{', '}', '[', ']'])
}
