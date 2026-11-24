//! Structure-preserving, indent-aware, flow-collection-capable YAML editor and validator for Oh My Pi.

use std::collections::{BTreeMap, BTreeSet};

/// Top-level YAML section (either a top-level mapping entry or leading/standalone comments).
#[derive(Clone, Debug)]
pub struct YamlTopSection {
    pub key: Option<String>,
    pub lines: Vec<String>,
}

/// Parsed YAML document preserving leading `---` and trailing `...` markers if present.
#[derive(Clone, Debug)]
pub struct YamlDocument {
    pub doc_start: Option<String>,
    pub sections: Vec<YamlTopSection>,
    pub doc_end: Option<String>,
    pub default_indent: usize,
}

/// Child entry inside a block mapping (`key: ...` plus any nested lines, or standalone comment lines).
#[derive(Clone, Debug)]
pub struct YamlSubEntry {
    pub key: Option<String>,
    pub lines: Vec<String>,
}

/// Scalar value written into 2-level Oh My Pi configuration mappings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum YamlScalar {
    Bool(bool),
    Str(String),
}

impl YamlScalar {
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Bool(true) => "true".to_owned(),
            Self::Bool(false) => "false".to_owned(),
            Self::Str(s) => yaml_quote_scalar(s),
        }
    }
}

/// Result of upserting 2-level scalar keys into a top-level mapping section.
pub struct ScalarMappingUpsert {
    pub section_was_absent: bool,
    pub previous_header_rhs: Option<String>,
    pub previous_entries: BTreeMap<String, Option<String>>,
    pub wrote_entries: BTreeMap<String, String>,
}

mod flow;
mod render;
mod validate;

pub use flow::{
    ensure_lf, line_indent, parse_yaml_flow_scalar_map, parse_yaml_flow_string_list,
    parse_yaml_mapping_key_at_indent, parse_yaml_string_sequence_section,
    split_yaml_inline_comment, split_yaml_key_and_rhs, strip_yaml_inline_comment,
    unquote_yaml_scalar,
};
use flow::{has_leading_tab_indentation, update_flow_depth};
use render::yaml_quote_key_if_needed;
pub use render::{render_json_entry_as_yaml, yaml_quote_scalar};
pub use validate::validate_yaml_structure;

/// Parses a YAML configuration document into top-level sections, rejecting tab indentation,
/// unbalanced flow brackets, or non-empty top-level flow maps that cannot be safely edited.
pub fn parse_yaml_document(content: &str) -> Result<YamlDocument, String> {
    let trimmed_bom = content.strip_prefix('\u{feff}').unwrap_or(content);
    let whole_trimmed = trimmed_bom.trim();
    if whole_trimmed.is_empty()
        || whole_trimmed == "{}"
        || whole_trimmed == "~"
        || whole_trimmed == "null"
    {
        return Ok(YamlDocument {
            doc_start: None,
            sections: Vec::new(),
            doc_end: None,
            default_indent: 2,
        });
    }
    if whole_trimmed.starts_with('{') || whole_trimmed.starts_with('[') {
        return Err("top-level YAML flow collection cannot be block-merged safely".to_owned());
    }

    let mut doc_start: Option<String> = None;
    let mut doc_end: Option<String> = None;
    let mut sections: Vec<YamlTopSection> = Vec::new();
    let mut pending_comments: Vec<String> = Vec::new();
    let mut flow_depth: i32 = 0;

    let raw_lines: Vec<&str> = trimmed_bom.split_inclusive('\n').collect();
    for (line_idx, &raw_line) in raw_lines.iter().enumerate() {
        if has_leading_tab_indentation(raw_line) {
            return Err(format!(
                "line {} uses tab indentation, which is invalid YAML",
                line_idx + 1
            ));
        }
        let code_only = strip_yaml_inline_comment(raw_line);
        let trimmed_code = code_only.trim();

        if flow_depth == 0 && sections.is_empty() && doc_start.is_none() && trimmed_code == "---" {
            doc_start = Some(ensure_lf(raw_line));
            continue;
        }
        if flow_depth == 0 && trimmed_code == "..." {
            // Any remaining lines after `...` must be blank or comments.
            if raw_lines
                .get(line_idx + 1..)
                .unwrap_or(&[])
                .iter()
                .all(|rest| strip_yaml_inline_comment(rest).trim().is_empty())
            {
                doc_end = Some(ensure_lf(raw_line));
                break;
            }
            return Err(
                "multi-document YAML (`...` followed by another document) is not supported"
                    .to_owned(),
            );
        }

        let prev_flow_depth = flow_depth;
        update_flow_depth(code_only, &mut flow_depth)?;

        if prev_flow_depth == 0
            && let Some((key, _)) = parse_yaml_mapping_key_at_indent(raw_line, 0)
        {
            if !pending_comments.is_empty() {
                sections.push(YamlTopSection {
                    key: None,
                    lines: std::mem::take(&mut pending_comments),
                });
            }
            sections.push(YamlTopSection {
                key: Some(key.to_owned()),
                lines: vec![ensure_lf(raw_line)],
            });
        } else if prev_flow_depth == 0
            && (raw_line.trim().is_empty()
                || (line_indent(raw_line) == 0 && raw_line.trim_start().starts_with('#')))
        {
            if sections.is_empty() {
                pending_comments.push(ensure_lf(raw_line));
            } else if let Some(last) = sections.last_mut() {
                last.lines.push(ensure_lf(raw_line));
            }
        } else if let Some(last) = sections.last_mut() {
            last.lines.push(ensure_lf(raw_line));
        } else {
            return Err(format!(
                "unexpected content before first top-level mapping key on line {}",
                line_idx + 1
            ));
        }
    }

    if flow_depth != 0 {
        return Err("unclosed `{` or `[` in YAML flow collection".to_owned());
    }
    if !pending_comments.is_empty() {
        sections.push(YamlTopSection {
            key: None,
            lines: pending_comments,
        });
    }

    let default_indent = detect_document_default_indent(&sections);
    Ok(YamlDocument {
        doc_start,
        sections,
        doc_end,
        default_indent,
    })
}

fn detect_document_default_indent(sections: &[YamlTopSection]) -> usize {
    for sec in sections {
        if sec.key.is_none() {
            continue;
        }
        for line in sec.lines.get(1..).unwrap_or(&[]) {
            let code = strip_yaml_inline_comment(line).trim();
            if code.is_empty() {
                continue;
            }
            let indent = line_indent(line);
            if indent >= 2 && !code.starts_with('-') {
                return indent;
            }
        }
    }
    2
}

/// Renders a [`YamlDocument`] back to normalized LF text and validates its YAML structure.
pub fn render_and_validate_yaml_document(doc: &YamlDocument) -> Result<String, String> {
    let mut out = String::new();
    if let Some(ref start) = doc.doc_start {
        out.push_str(start);
    }
    for sec in &doc.sections {
        for line in &sec.lines {
            out.push_str(line);
        }
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    if let Some(ref end) = doc.doc_end {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(end);
    }
    validate_yaml_structure(&out)?;
    Ok(out)
}

/// Parsed layout of a single top-level mapping section (`providers:`, `modelRoles:`, `tools:`, etc.).
pub struct ParsedMappingSection {
    pub section_name: String,
    pub header_comment: Option<String>,
    pub child_indent: usize,
    pub previous_header_rhs: Option<String>,
    pub children: Vec<YamlSubEntry>,
    pub trailing_blanks: Vec<String>,
}

impl ParsedMappingSection {
    #[must_use]
    pub fn format_block_header(&self) -> String {
        match &self.header_comment {
            Some(comment) => format!("{}: {comment}\n", self.section_name),
            None => format!("{}:\n", self.section_name),
        }
    }
}

/// Splits a top-level mapping section into its header, dynamically-indented children, and trailing
/// blank lines. When `allow_scalar_flow_map` is `true`, inline flow scalar maps such as
/// `tools: {commands: false, custom: 1}` are expanded into [`YamlSubEntry`] children losslessly.
pub fn parse_mapping_section(
    section_lines: &[String],
    default_indent: usize,
    allow_scalar_flow_map: bool,
) -> Result<ParsedMappingSection, String> {
    let first_line = section_lines
        .first()
        .ok_or_else(|| "empty YAML section".to_owned())?;
    let (code_head, head_comment) = split_yaml_inline_comment(first_line);
    let (sec_key, rhs_first) = split_yaml_key_and_rhs(code_head.trim())
        .ok_or_else(|| format!("invalid YAML mapping header `{}`", first_line.trim()))?;
    let section_name = sec_key.to_owned();
    let header_comment = head_comment.map(str::to_owned);

    let body_all = section_lines.get(1..).unwrap_or(&[]);
    let mut end = body_all.len();
    let mut trailing_blanks: Vec<String> = Vec::new();
    while let Some(last) = end.checked_sub(1).and_then(|i| body_all.get(i)) {
        if last.trim().is_empty() {
            trailing_blanks.push(last.clone());
            end -= 1;
        } else {
            break;
        }
    }
    trailing_blanks.reverse();
    let body = body_all.get(..end).unwrap_or(&[]);

    if matches!(rhs_first, "{}" | "null" | "~") {
        if body
            .iter()
            .any(|l| !strip_yaml_inline_comment(l).trim().is_empty())
        {
            return Err(format!(
                "section `{section_name}` combines inline `{rhs_first}` with indented block lines"
            ));
        }
        let mut children = Vec::new();
        if !body.is_empty() {
            children.push(YamlSubEntry {
                key: None,
                lines: body.to_vec(),
            });
        }
        return Ok(ParsedMappingSection {
            section_name,
            header_comment,
            child_indent: default_indent.max(2),
            previous_header_rhs: Some(rhs_first.to_owned()),
            children,
            trailing_blanks,
        });
    }

    if rhs_first.starts_with('{') {
        if !allow_scalar_flow_map {
            return Err(format!(
                "section `{section_name}` uses inline flow syntax (`{{...}}`), which cannot be block-merged safely"
            ));
        }
        // Collect possible multi-line `{ ... }` flow map.
        let mut combined_flow = String::from(rhs_first);
        for line in body {
            let part = strip_yaml_inline_comment(line).trim();
            if !part.is_empty() {
                combined_flow.push(' ');
                combined_flow.push_str(part);
            }
        }
        let pairs = parse_yaml_flow_scalar_map(&combined_flow)?;
        let child_indent = default_indent.max(2);
        let pad = " ".repeat(child_indent);
        let children = pairs
            .into_iter()
            .map(|(k, raw_v)| {
                let qk = yaml_quote_key_if_needed(&k);
                YamlSubEntry {
                    key: Some(k),
                    lines: vec![format!("{pad}{qk}: {raw_v}\n")],
                }
            })
            .collect();
        return Ok(ParsedMappingSection {
            section_name,
            header_comment,
            child_indent,
            previous_header_rhs: Some(combined_flow),
            children,
            trailing_blanks,
        });
    }

    if !rhs_first.is_empty() {
        return Err(format!(
            "section `{section_name}` has non-mapping value `{rhs_first}`"
        ));
    }

    // Detect actual child indentation from the first non-blank, non-comment body line.
    let mut detected_indent: Option<usize> = None;
    for line in body {
        let trimmed_code = strip_yaml_inline_comment(line).trim();
        if trimmed_code.is_empty() {
            continue;
        }
        let indent = line_indent(line);
        if indent == 0 {
            return Err(format!(
                "section `{section_name}` contains unindented child `{trimmed_code}`"
            ));
        }
        if trimmed_code.starts_with('-') {
            return Err(format!(
                "section `{section_name}` is a YAML sequence (`- ...`), expected a mapping"
            ));
        }
        if split_yaml_key_and_rhs(trimmed_code).is_none() {
            return Err(format!(
                "section `{section_name}` contains non-mapping line `{trimmed_code}`"
            ));
        }
        detected_indent = Some(indent);
        break;
    }

    let child_indent = detected_indent.unwrap_or_else(|| default_indent.max(2));
    let mut children: Vec<YamlSubEntry> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut child_flow_depth: i32 = 0;

    for line in body {
        let code_only = strip_yaml_inline_comment(line);
        let trimmed_code = code_only.trim();
        let prev_depth = child_flow_depth;
        update_flow_depth(code_only, &mut child_flow_depth)?;

        if trimmed_code.is_empty() {
            if let Some(last) = children.last_mut() {
                last.lines.push(line.clone());
            } else {
                pending.push(line.clone());
            }
            continue;
        }

        let indent = line_indent(line);
        if prev_depth == 0 && indent < child_indent {
            return Err(format!(
                "section `{section_name}` has inconsistent child indentation ({indent} spaces vs {child_indent} spaces)"
            ));
        }
        if prev_depth == 0 && indent == child_indent {
            if trimmed_code.starts_with('-') {
                return Err(format!(
                    "section `{section_name}` mixes mapping keys and `- ` sequence items at indent {child_indent}"
                ));
            }
            let (k, _) = split_yaml_key_and_rhs(trimmed_code).ok_or_else(|| {
                format!("invalid mapping entry `{trimmed_code}` in `{section_name}`")
            })?;
            if !pending.is_empty() {
                children.push(YamlSubEntry {
                    key: None,
                    lines: std::mem::take(&mut pending),
                });
            }
            children.push(YamlSubEntry {
                key: Some(k.to_owned()),
                lines: vec![line.clone()],
            });
        } else if let Some(last) = children.last_mut() {
            last.lines.push(line.clone());
        } else {
            pending.push(line.clone());
        }
    }
    if !pending.is_empty() {
        children.push(YamlSubEntry {
            key: None,
            lines: pending,
        });
    }

    Ok(ParsedMappingSection {
        section_name,
        header_comment,
        child_indent,
        previous_header_rhs: None,
        children,
        trailing_blanks,
    })
}

/// Upserts 2-level scalar keys (`modelRoles`, `tools`, `eval`, `mcp`), preserving existing child
/// indentation (e.g. 2-space or 4-space), sibling keys, inline comments, and flow scalar maps.
pub fn upsert_yaml_2level_scalars(
    doc: &mut YamlDocument,
    section_name: &str,
    desired: &[(&str, YamlScalar)],
) -> Result<ScalarMappingUpsert, String> {
    let mut previous_entries = BTreeMap::new();
    let mut wrote_entries = BTreeMap::new();
    for (k, v) in desired {
        wrote_entries.insert((*k).to_owned(), v.render());
    }

    if let Some(sec) = doc
        .sections
        .iter_mut()
        .find(|s| s.key.as_deref() == Some(section_name))
    {
        let parsed = parse_mapping_section(&sec.lines, doc.default_indent, true)?;
        let block_header = parsed.format_block_header();
        let pad = " ".repeat(parsed.child_indent);
        let mut rebuilt = vec![block_header];
        let mut seen = BTreeSet::new();

        for child in parsed.children {
            if let Some(ref ck) = child.key
                && let Some((_, new_val)) = desired.iter().find(|(dk, _)| dk == ck)
            {
                let first_line = child.lines.first().map(String::as_str).unwrap_or("");
                let (code_part, comment_opt) = split_yaml_inline_comment(first_line);
                let prev_rhs = split_yaml_key_and_rhs(code_part.trim())
                    .map(|(_, rhs)| rhs.to_owned())
                    .filter(|s| !s.is_empty());
                previous_entries.insert(ck.clone(), prev_rhs);
                seen.insert(ck.clone());
                let qk = yaml_quote_key_if_needed(ck);
                match comment_opt {
                    Some(comment) => {
                        rebuilt.push(format!("{pad}{qk}: {} {comment}\n", new_val.render()));
                    }
                    None => {
                        rebuilt.push(format!("{pad}{qk}: {}\n", new_val.render()));
                    }
                }
            } else {
                rebuilt.extend(child.lines);
            }
        }
        for (k, v) in desired {
            if !seen.contains(*k) {
                previous_entries.insert((*k).to_owned(), None);
                let qk = yaml_quote_key_if_needed(k);
                rebuilt.push(format!("{pad}{qk}: {}\n", v.render()));
            }
        }
        rebuilt.extend(parsed.trailing_blanks);
        sec.lines = rebuilt;
        Ok(ScalarMappingUpsert {
            section_was_absent: false,
            previous_header_rhs: parsed.previous_header_rhs,
            previous_entries,
            wrote_entries,
        })
    } else {
        if let Some(last) = doc.sections.last_mut()
            && last.lines.last().is_some_and(|l| !l.trim().is_empty())
        {
            last.lines.push("\n".to_owned());
        }
        let pad = " ".repeat(doc.default_indent.max(2));
        let mut lines = vec![format!("{section_name}:\n")];
        for (k, v) in desired {
            previous_entries.insert((*k).to_owned(), None);
            let qk = yaml_quote_key_if_needed(k);
            lines.push(format!("{pad}{qk}: {}\n", v.render()));
        }
        doc.sections.push(YamlTopSection {
            key: Some(section_name.to_owned()),
            lines,
        });
        Ok(ScalarMappingUpsert {
            section_was_absent: true,
            previous_header_rhs: None,
            previous_entries,
            wrote_entries,
        })
    }
}

/// Reverts 2-level scalar keys previously written by [`upsert_yaml_2level_scalars`], preserving
/// the section's indentation, comments, unrelated keys, and prior inline header state (`{}` / flow map).
pub fn revert_yaml_2level_scalars(
    doc: &mut YamlDocument,
    section_name: &str,
    was_absent: bool,
    previous_header_rhs: Option<&str>,
    previous: &BTreeMap<String, Option<String>>,
    wrote: &BTreeMap<String, String>,
) -> Result<bool, String> {
    if wrote.is_empty() {
        return Ok(false);
    }
    let Some(idx) = doc
        .sections
        .iter()
        .position(|s| s.key.as_deref() == Some(section_name))
    else {
        return Ok(false);
    };
    let parsed = {
        let Some(sec) = doc.sections.get(idx) else {
            return Ok(false);
        };
        parse_mapping_section(&sec.lines, doc.default_indent, true)?
    };

    let block_header = parsed.format_block_header();
    let pad = " ".repeat(parsed.child_indent);
    let mut kept: Vec<YamlSubEntry> = Vec::new();
    let mut changed = false;

    for child in parsed.children {
        if let Some(ref ck) = child.key
            && let Some(wrote_val) = wrote.get(ck)
        {
            let first_line = child.lines.first().map(String::as_str).unwrap_or("");
            let (code_part, comment_opt) = split_yaml_inline_comment(first_line);
            let current_rhs = split_yaml_key_and_rhs(code_part.trim()).map(|(_, rhs)| rhs);
            let matches = current_rhs.is_some_and(|cur| {
                cur == wrote_val.as_str()
                    || unquote_yaml_scalar(cur) == unquote_yaml_scalar(wrote_val)
            });
            if matches {
                changed = true;
                if let Some(Some(prev_rhs)) = previous.get(ck) {
                    let qk = yaml_quote_key_if_needed(ck);
                    let restored_line = match comment_opt {
                        Some(comment) => format!("{pad}{qk}: {prev_rhs} {comment}\n"),
                        None => format!("{pad}{qk}: {prev_rhs}\n"),
                    };
                    kept.push(YamlSubEntry {
                        key: Some(ck.clone()),
                        lines: vec![restored_line],
                    });
                }
                continue;
            }
        }
        kept.push(child);
    }

    let has_remaining_keys = kept.iter().any(|c| c.key.is_some());
    if !has_remaining_keys && changed {
        if was_absent {
            let _ = doc.sections.remove(idx);
        } else if let Some(prev_rhs) = previous_header_rhs
            && matches!(prev_rhs, "{}" | "null" | "~")
            && let Some(sec_after) = doc.sections.get_mut(idx)
        {
            let head = match &parsed.header_comment {
                Some(comment) => format!("{section_name}: {prev_rhs} {comment}\n"),
                None => format!("{section_name}: {prev_rhs}\n"),
            };
            let mut rebuilt = vec![head];
            rebuilt.extend(parsed.trailing_blanks);
            sec_after.lines = rebuilt;
        } else {
            let _ = doc.sections.remove(idx);
        }
    } else if changed {
        let mut rebuilt = vec![block_header];
        for c in kept {
            rebuilt.extend(c.lines);
        }
        rebuilt.extend(parsed.trailing_blanks);
        if let Some(sec_after) = doc.sections.get_mut(idx) {
            sec_after.lines = rebuilt;
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests;
