//! Code Mode orchestrator contracts, TypeScript declaration dehydration, and tool schema compaction.

use crate::canonical::{CanonicalToolKind, CanonicalToolSpec};

use super::spill::{
    TruncationSpillSink, compact_middle_chars, truncate_head_tail_lines_from_source,
};
use super::strip_tag_blocks;

/// Explicit Code Mode orchestration contract discovered by the active harness tool profile and
/// forwarded into `_meta.antigravity.systemPrompt.append` without name-based guessing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpOrchestratorContract<'a> {
    /// Oh My Pi (`omp`) persistent polyglot `eval` (`py` / `js` kernels calling `tool.<name>(args)`
    /// and `mcp.evalNamespaces` globals by unprefixed client names).
    OmpPolyglotEval {
        /// Sanitized MCP tool name mounted on `lg` (e.g. `"eval"` -> `mcp_lg_eval`).
        mcp_name: &'a str,
    },
}

pub(super) const MAX_CODE_MODE_TOOL_DESC_BYTES: usize = 36_000;
pub(super) const MAX_SLIM_CODE_MODE_TOOL_DESC_BYTES: usize = 24_000;

/// Returns `true` when `tool` is a Codex Code Mode custom JavaScript `exec` tool
/// (`CanonicalToolKind::Custom` named `"exec"` or `"*.exec"`).
#[must_use]
pub fn is_code_mode_custom_exec(tool: &CanonicalToolSpec) -> bool {
    matches!(tool.kind, CanonicalToolKind::Custom { .. })
        && (matches!(tool.name.as_str(), "exec" | "execute") || tool.name.ends_with(".exec"))
}

/// Returns `true` when `tool` is Oh My Pi (`omp`)'s persistent polyglot `eval` Code Mode
/// orchestrator (`py` / `js` kernels with `tool.<name>(args)`, `mcp.evalNamespaces`, and preludes).
///
/// Requires explicit OMP orchestration evidence in either the description or the `{ language, code }`
/// schema so generic `eval` tools (e.g. expression calculators) never receive OMP kernel rules.
#[must_use]
pub fn is_omp_eval_orchestrator_tool(tool: &CanonicalToolSpec) -> bool {
    if tool.name != "eval" {
        return false;
    }
    let has_omp_desc = tool.description.as_deref().is_some_and(|desc| {
        desc.contains("<prelude>")
            || desc.contains("await tool.")
            || desc.contains("persistent kernels")
            || desc.contains("top-level state persists")
    });
    if has_omp_desc {
        return true;
    }
    match &tool.kind {
        CanonicalToolKind::Custom { format_json } => format_json.as_deref().is_some_and(|fmt| {
            fmt.contains("<prelude>")
                || fmt.contains("persistent kernels")
                || (fmt.contains("language") && fmt.contains("code"))
        }),
        CanonicalToolKind::Function { parameters_json } => {
            parameters_json.contains("\"code\"")
                && parameters_json.contains("\"language\"")
                && serde_json::from_str::<serde_json::Value>(parameters_json)
                    .ok()
                    .and_then(|val| {
                        val.get("properties")
                            .and_then(serde_json::Value::as_object)
                            .map(|props| {
                                props.contains_key("code") && props.contains_key("language")
                            })
                    })
                    .unwrap_or(false)
        }
    }
}

/// Splits a tool description into its primary prose and an optional trailing Codex Code Mode
/// TypeScript signature footer (`exec tool declaration:\n```ts\ndeclare const tools: ...`).
#[must_use]
pub fn split_code_mode_ts_declaration(raw_desc: &str) -> (&str, Option<&str>) {
    let trimmed = raw_desc.trim();
    if trimmed.starts_with("exec tool declaration:") {
        return ("", Some(trimmed));
    }
    if let Some(idx) = trimmed.find("\nexec tool declaration:")
        && let Some((prose, footer)) = trimmed.split_at_checked(idx)
    {
        return (prose.trim_end(), Some(footer.trim()));
    }
    (trimmed, None)
}

/// Returns `true` when `tool` is a Code Mode orchestrator (`exec` custom tool, `REPL`, or TypeScript
/// `declare const tools` runtime wrapper).
///
/// Ordinary nested tools whose descriptions merely carry a trailing `exec tool declaration:`
/// signature block from Codex `code_mode_host` are excluded so they are not misclassified as
/// top-tier orchestrators.
#[must_use]
pub fn is_code_mode_orchestrator_tool(tool: &CanonicalToolSpec) -> bool {
    if tool.name == "REPL" {
        return true;
    }
    if is_code_mode_custom_exec(tool) || is_omp_eval_orchestrator_tool(tool) {
        return true;
    }
    if tool.name == "execute"
        && tool.description.as_deref().is_some_and(|desc| {
            desc.contains("Code Mode runtime") || desc.contains("Code Mode catalog")
        })
    {
        return true;
    }
    tool.description.as_deref().is_some_and(|desc| {
        let (prose, _) = split_code_mode_ts_declaration(desc);
        prose.contains("declare const tools")
            || prose.contains("globalThis.tools")
            || prose.contains("Tool TypeScript declarations:")
            || prose.contains("// @exec:")
    })
}

/// Compacts a tool description for native loopback MCP `tools/list` while preserving full
/// Code Mode TypeScript `declare const tools` declarations up to `max_bytes` (spilling to disk if larger).
///
/// For non-orchestrator tools, redundant trailing `exec tool declaration:` TypeScript blocks are
/// stripped from wire `tools/list` prose because the MCP wire entry already carries `inputSchema`
/// (and the extracted TypeScript signature remains queryable via `search_client_tools`).
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
        let (prose, _) = split_code_mode_ts_declaration(raw_desc);
        let source_desc = if prose.is_empty() { raw_desc } else { prose };
        let collapsed = collapse_tool_description(source_desc);
        let bounded = if collapsed.len() > max_bytes {
            truncate_head_tail_lines_from_source(
                &collapsed,
                source_desc,
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

pub(super) fn collapse_tool_description(raw_desc: &str) -> String {
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

pub(super) fn had_tool_boilerplate_tags(raw_desc: &str) -> bool {
    raw_desc.contains("<example>")
        || raw_desc.contains("<examples>")
        || raw_desc.contains("<commentary>")
}

pub(super) fn compact_tool_description(raw_desc: &str) -> (String, bool) {
    let collapsed = collapse_tool_description(raw_desc);
    let (compacted, truncated) = compact_middle_chars(&collapsed, 600);
    (compacted, truncated || had_tool_boilerplate_tags(raw_desc))
}

pub(super) fn compact_code_mode_tool_description<S: TruncationSpillSink>(
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

pub(super) fn compact_json_schema(raw_schema: &str) -> (String, bool) {
    let Ok(mut val) = serde_json::from_str::<serde_json::Value>(raw_schema) else {
        return (raw_schema.trim().to_owned(), false);
    };
    let mut truncated_any = false;
    slim_schema_value(&mut val, &mut truncated_any);
    let serialized = serde_json::to_string(&val).unwrap_or_else(|_| raw_schema.trim().to_owned());
    (serialized, truncated_any)
}

pub(super) fn slim_schema_value(val: &mut serde_json::Value, truncated_any: &mut bool) {
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
