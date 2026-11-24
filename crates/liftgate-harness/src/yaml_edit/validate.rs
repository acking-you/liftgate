//! Lightweight structural validator for rendered YAML mappings and sequences.

use super::flow::{
    has_leading_tab_indentation, line_indent, split_yaml_key_and_rhs, strip_yaml_inline_comment,
    update_flow_depth,
};

/// Validates that a YAML document has consistent block indentation, balanced quotes/flow brackets,
/// no tab indentation, and never nests block lines beneath a scalar or inline flow collection.
pub fn validate_yaml_structure(yaml: &str) -> Result<(), String> {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum BlockKind {
        Mapping,
        Sequence,
    }

    let trimmed_bom = yaml.strip_prefix('\u{feff}').unwrap_or(yaml);
    let mut stack: Vec<(usize, BlockKind)> = Vec::new();
    let mut allow_nested_block = true;
    let mut flow_depth: i32 = 0;
    let mut block_scalar_parent_indent: Option<usize> = None;

    for (idx, raw_line) in trimmed_bom.split_inclusive('\n').enumerate() {
        let line_no = idx + 1;
        if has_leading_tab_indentation(raw_line) {
            return Err(format!("line {line_no}: tab character in YAML indentation"));
        }
        if let Some(parent_indent) = block_scalar_parent_indent {
            if raw_line.trim().is_empty() {
                continue;
            }
            if line_indent(raw_line) > parent_indent {
                continue;
            }
            block_scalar_parent_indent = None;
        }

        let code_only = strip_yaml_inline_comment(raw_line);
        let trimmed = code_only.trim();
        if trimmed.is_empty() {
            continue;
        }
        if flow_depth == 0 && matches!(trimmed, "---" | "...") {
            stack.clear();
            allow_nested_block = true;
            continue;
        }

        let prev_flow_depth = flow_depth;
        update_flow_depth(code_only, &mut flow_depth)
            .map_err(|e| format!("line {line_no}: {e}"))?;
        if prev_flow_depth > 0 {
            continue;
        }

        let indent = line_indent(raw_line);
        let is_seq_item = trimmed == "-" || trimmed.starts_with("- ");

        let mut did_pop = false;
        while let Some(&(top_indent, top_kind)) = stack.last() {
            if indent < top_indent {
                let _ = stack.pop();
                allow_nested_block = true;
                did_pop = true;
            } else if indent == top_indent && top_kind == BlockKind::Mapping && is_seq_item {
                // YAML allows indentless sequence items (`key:\n- item`) at the same indent as `key:`.
                break;
            } else {
                break;
            }
        }
        if did_pop && stack.last().map(|&(ti, _)| ti) != Some(indent) {
            return Err(format!(
                "line {line_no}: inconsistent dedent to {indent} spaces on `{trimmed}`"
            ));
        }

        if let Some(&(top_indent, top_kind)) = stack.last() {
            if indent > top_indent && !allow_nested_block {
                return Err(format!(
                    "line {line_no}: unexpected indented block line `{trimmed}` beneath a scalar or inline flow value"
                ));
            }
            if indent == top_indent {
                match (top_kind, is_seq_item) {
                    (BlockKind::Mapping, true) => {
                        // Indentless sequence directly beneath a mapping key with empty RHS.
                        if !allow_nested_block {
                            return Err(format!(
                                "line {line_no}: unexpected `- ` sequence item beneath scalar mapping entry"
                            ));
                        }
                        stack.push((indent, BlockKind::Sequence));
                    }
                    (BlockKind::Sequence, false) => {
                        return Err(format!(
                            "line {line_no}: mapping key `{trimmed}` at sequence indentation {indent}"
                        ));
                    }
                    _ => {}
                }
            } else if indent > top_indent {
                let kind = if is_seq_item {
                    BlockKind::Sequence
                } else {
                    BlockKind::Mapping
                };
                stack.push((indent, kind));
            }
        } else {
            let kind = if is_seq_item {
                BlockKind::Sequence
            } else {
                BlockKind::Mapping
            };
            stack.push((indent, kind));
        }

        if is_seq_item {
            let after_dash = trimmed.strip_prefix('-').unwrap_or("").trim();
            if after_dash.is_empty() {
                allow_nested_block = true;
            } else if let Some((_, rhs)) = split_yaml_key_and_rhs(after_dash) {
                // Inline mapping inside a sequence item (`- id: foo` followed by `  name: bar` at `indent + 2`).
                stack.push((indent + 2, BlockKind::Mapping));
                if rhs.starts_with('|') || rhs.starts_with('>') {
                    block_scalar_parent_indent = Some(indent + 2);
                    allow_nested_block = false;
                } else {
                    allow_nested_block = rhs.is_empty();
                }
            } else {
                allow_nested_block = false;
            }
        } else if let Some((_, rhs)) = split_yaml_key_and_rhs(trimmed) {
            if rhs.starts_with('|') || rhs.starts_with('>') {
                block_scalar_parent_indent = Some(indent);
                allow_nested_block = false;
            } else {
                allow_nested_block = rhs.is_empty();
            }
        } else {
            return Err(format!(
                "line {line_no}: expected YAML mapping `key: value` or `- item`, found `{trimmed}`"
            ));
        }
    }

    if flow_depth != 0 {
        return Err("unclosed `{` or `[` at end of YAML document".to_owned());
    }
    Ok(())
}
