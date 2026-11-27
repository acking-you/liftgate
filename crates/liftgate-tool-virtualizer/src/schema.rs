//! Pure JSON Schema normalization, byte estimation, identifier sanitization, and container coercion.

use std::borrow::Cow;
use std::collections::HashSet;

pub use liftgate_types::parser::{
    ApplyPatchPreflightError, normalize_apply_patch_text, validate_apply_patch_preflight,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::profile::V4aPatchContract;

/// Wire tool descriptor serialized in MCP `tools/list` responses.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpWireTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// Mapping from an exposed MCP tool name back to the original client tool contract.
#[derive(Clone, Debug)]
pub struct McpBridgedTool {
    pub mcp_name: String,
    pub client_name: String,
    pub is_custom: bool,
    pub unwrap_custom_key: Option<&'static str>,
    pub v4a_patch: Option<V4aPatchContract>,
    pub parameters_json: Option<String>,
    pub non_nullable_required_fields: Vec<String>,
    pub present_required_fields: Vec<String>,
    pub declares_tool_summary: bool,
    pub requires_string_intent_i: bool,
    pub has_root_combinator: bool,
}

/// Measures serialized JSON wire size of `(name, description, schema)` without allocating a `String`.
#[must_use]
pub fn estimate_wire_tool_bytes(name: &str, desc: Option<&str>, schema: &Value) -> usize {
    struct ByteCounter(usize);
    impl std::io::Write for ByteCounter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(buf.len());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = ByteCounter(0);
    let _ = serde_json::to_writer(&mut counter, schema);
    name.len()
        .saturating_add(desc.map_or(0, str::len))
        .saturating_add(counter.0)
}

/// Input JSON Schema synthesized for freeform Code Mode `exec` tools exposed over JSON MCP.
#[must_use]
pub fn code_mode_exec_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "code": {
                "type": "string",
                "description": "Raw JavaScript source code to execute in the Code Mode runtime (do not wrap in markdown code fences)."
            }
        },
        "required": ["code"]
    })
}

/// Input JSON Schema synthesized for freeform `apply_patch` tools exposed natively over JSON MCP.
#[must_use]
pub fn apply_patch_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "patch": {
                "type": "string",
                "description": "Raw V4A patch text starting with '*** Begin Patch' and ending with '*** End Patch'. Keep '@@' hunks surgical (1-3 exact context lines) and combine multiple edits to the same file under a single '*** Update File: <path>' header."
            }
        },
        "required": ["patch"]
    })
}

/// Input JSON Schema synthesized for arbitrary freeform `Custom` client tools exposed over JSON MCP.
#[must_use]
pub fn freeform_custom_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "input": {
                "type": "string",
                "description": "Raw freeform text input for this custom tool (do not JSON-escape or wrap in markdown code fences)."
            }
        },
        "required": ["input"]
    })
}

/// Input JSON Schema for the gateway-managed server-side `gateway_web_fetch` MCP tool.
#[must_use]
pub fn gateway_web_fetch_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "url": {
                "type": "string",
                "description": "Full http:// or https:// URL to fetch server-side."
            },
            "pattern": {
                "type": "string",
                "description": "Optional case-insensitive substring or keyword to scan for within the fetched page (hosted `find_in_page`)."
            }
        },
        "required": ["url"]
    })
}

/// Input JSON Schema for the gateway-local 0-RTT `read_context_spill` MCP tool.
#[must_use]
pub fn read_context_spill_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Spill file path or `<slug>-<hash>.txt` / `<slug>-<hash>.json` filename referenced in a truncation notice."
            },
            "start_line": {
                "type": "integer",
                "description": "1-based starting line number (default 1)."
            },
            "byte_offset": {
                "type": "integer",
                "description": "0-based byte offset within `start_line` to resume reading an overlong single line (default 0; e.g. minified JSON)."
            },
            "max_lines": {
                "type": "integer",
                "description": "Maximum lines to return (1..200, default 120)."
            },
            "query": {
                "type": "string",
                "description": "Optional case-insensitive substring filter returning matching lines with line numbers."
            }
        },
        "required": ["path"]
    })
}

/// Normalizes a caller tool's JSON Schema string into a valid root `{"type": "object", ...}` MCP schema.
///
/// Always returns a valid object schema so 100% of `Function` tools are bridged natively over MCP
/// without ever falling back to XML text simulation.
#[must_use]
pub fn normalize_mcp_object_schema(parameters_json: &str) -> Value {
    let trimmed = parameters_json.trim();
    if trimmed.is_empty() {
        return json!({
            "type": "object",
            "properties": {}
        });
    }
    let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(trimmed) else {
        return json!({
            "type": "object",
            "properties": {}
        });
    };
    let _ = map.remove("$schema");

    match map.get("type").and_then(Value::as_str) {
        Some("object") => {
            if !map.contains_key("properties") {
                map.insert(
                    "properties".to_owned(),
                    Value::Object(serde_json::Map::new()),
                );
            }
            Value::Object(map)
        }
        Some(_) => json!({
            "type": "object",
            "properties": {
                "value": Value::Object(map)
            }
        }),
        None => {
            map.insert("type".to_owned(), Value::String("object".to_owned()));
            if !map.contains_key("properties") {
                map.insert(
                    "properties".to_owned(),
                    Value::Object(serde_json::Map::new()),
                );
            }
            Value::Object(map)
        }
    }
}

/// Pre-extracted structural metadata from a tool's JSON Schema for 0-RTT preflight and search indexing.
#[derive(Clone, Debug, Default)]
pub struct ExtractedSchemaMeta {
    pub required_fields: Vec<String>,
    /// Required fields whose schema rejects `null` (must be both present AND non-null).
    pub non_nullable_required_fields: Vec<String>,
    /// Required fields with unconstrained schemas (e.g. `{}`) where `null` is valid when present,
    /// while complete key omission (`!map.contains_key`) is still missing.
    pub present_required_fields: Vec<String>,
    pub declares_tool_summary: bool,
    pub requires_string_intent_i: bool,
    pub has_root_combinator: bool,
    pub prop_names_joined: String,
}

fn schema_explicitly_declares_null_or_ref(prop_schema: &Value) -> bool {
    let Value::Object(obj) = prop_schema else {
        return false;
    };
    if obj.contains_key("$ref") || obj.contains_key("$dynamicRef") {
        return true;
    }
    if let Some(ty) = obj.get("type") {
        match ty {
            Value::String(s) if s == "null" => return true,
            Value::Array(arr) if arr.iter().any(|v| v.as_str() == Some("null")) => return true,
            _ => {}
        }
    }
    if let Some(const_val) = obj.get("const")
        && const_val.is_null()
    {
        return true;
    }
    if let Some(Value::Array(enum_vals)) = obj.get("enum")
        && enum_vals.iter().any(Value::is_null)
    {
        return true;
    }
    for union_key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(branches)) = obj.get(union_key)
            && branches.iter().any(schema_explicitly_declares_null_or_ref)
        {
            return true;
        }
    }
    false
}

fn schema_property_allows_null(prop_schema: &Value) -> bool {
    match prop_schema {
        Value::Bool(allow_all) => *allow_all,
        Value::Object(obj) => {
            if obj.contains_key("$ref") || obj.contains_key("$dynamicRef") {
                return true;
            }
            if let Some(not_schema) = obj.get("not")
                && (not_schema.get("type").and_then(Value::as_str) == Some("null")
                    || not_schema.get("const").is_some_and(Value::is_null))
            {
                return false;
            }
            if let Some(const_val) = obj.get("const")
                && !const_val.is_null()
            {
                return false;
            }
            if let Some(Value::Array(enum_vals)) = obj.get("enum")
                && !enum_vals.iter().any(Value::is_null)
            {
                return false;
            }
            if let Some(ty) = obj.get("type") {
                let allows_null = match ty {
                    Value::String(s) => s == "null",
                    Value::Array(arr) => arr.iter().any(|v| v.as_str() == Some("null")),
                    _ => false,
                };
                if !allows_null {
                    return false;
                }
            }
            if let Some(Value::Array(branches)) = obj.get("anyOf")
                && !branches.iter().any(schema_property_allows_null)
            {
                return false;
            }
            if let Some(Value::Array(branches)) = obj.get("oneOf")
                && !branches.iter().any(schema_property_allows_null)
            {
                return false;
            }
            if let Some(Value::Array(branches)) = obj.get("allOf")
                && !branches.iter().all(schema_property_allows_null)
            {
                return false;
            }
            true
        }
        _ => false,
    }
}

fn schema_property_is_string(prop_schema: &Value) -> bool {
    match prop_schema.get("type") {
        Some(Value::String(s)) => s == "string",
        Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some("string")),
        _ => false,
    }
}

/// Extracts [`ExtractedSchemaMeta`] from a raw JSON Schema string for preflight validation and lexical indexing.
#[must_use]
pub fn extract_schema_metadata(parameters_json: Option<&str>) -> ExtractedSchemaMeta {
    let Some(raw) = parameters_json else {
        return ExtractedSchemaMeta::default();
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(raw) else {
        return ExtractedSchemaMeta::default();
    };
    let required_fields = map
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let has_root_combinator =
        map.contains_key("oneOf") || map.contains_key("anyOf") || map.contains_key("allOf");
    let props_opt = map.get("properties").and_then(Value::as_object);
    let mut non_nullable_required_fields = Vec::new();
    let mut present_required_fields = Vec::new();
    for req_key in &required_fields {
        match props_opt.and_then(|props| props.get(req_key.as_str())) {
            Some(prop_schema) if !schema_property_allows_null(prop_schema) => {
                non_nullable_required_fields.push(req_key.clone());
            }
            Some(prop_schema) if !schema_explicitly_declares_null_or_ref(prop_schema) => {
                present_required_fields.push(req_key.clone());
            }
            None => {
                present_required_fields.push(req_key.clone());
            }
            Some(_) => {}
        }
    }
    let requires_string_intent_i = required_fields.iter().any(|r| r == "i")
        && props_opt
            .and_then(|props| props.get("i"))
            .is_some_and(|prop_i| {
                schema_property_is_string(prop_i) && !schema_property_allows_null(prop_i)
            });

    let mut declares_tool_summary = false;
    let mut prop_names = Vec::new();
    if let Some(props) = props_opt {
        for k in props.keys() {
            if k == "toolSummary" {
                declares_tool_summary = true;
            }
            prop_names.push(k.as_str());
        }
    }
    ExtractedSchemaMeta {
        required_fields,
        non_nullable_required_fields,
        present_required_fields,
        declares_tool_summary,
        requires_string_intent_i,
        has_root_combinator,
        prop_names_joined: prop_names.join(" "),
    }
}

/// Sanitizes an arbitrary client tool identifier into a unique MCP tool name (`[a-zA-Z_][a-zA-Z0-9_-]{0,51}`).
#[must_use]
pub fn sanitize_mcp_tool_name(raw_name: &str, used: &mut HashSet<String>) -> String {
    let base_raw = if let Some(after_dunder) = raw_name.strip_prefix("__") {
        format!("lg_internal_{after_dunder}")
    } else {
        raw_name.to_owned()
    };

    let mut clean = String::with_capacity(base_raw.len().min(52));
    for ch in base_raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            clean.push(ch);
        } else if ch == '.' {
            clean.push_str("__");
        } else {
            clean.push('_');
        }
    }
    if clean.is_empty()
        || clean
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphabetic() || c == '_'))
    {
        clean.insert_str(0, "tool_");
    }
    if clean.len() > 52
        && let Some(cut) = clean.get(..52)
    {
        clean = cut.to_owned();
    }

    if used.insert(clean.clone()) {
        return clean;
    }

    let stem = if clean.len() > 46 {
        clean.get(..46).unwrap_or(&clean).to_owned()
    } else {
        clean
    };
    let mut suffix = 2u32;
    loop {
        let candidate = format!("{stem}_{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        suffix = suffix.saturating_add(1);
    }
}

/// Extracts a custom freeform string (such as Code Mode `exec` raw JavaScript) from MCP `arguments`.
#[must_use]
pub fn extract_custom_tool_input(arguments: &Value, preferred_key: Option<&str>) -> String {
    if let Some(key) = preferred_key
        && let Some(s) = arguments.get(key).and_then(Value::as_str)
    {
        return s.to_owned();
    }
    if let Some(s) = arguments.as_str() {
        return s.to_owned();
    }
    if let Value::Object(map) = arguments {
        for fallback in ["code", "patch", "input", "diff", "content"] {
            if let Some(s) = map.get(fallback).and_then(Value::as_str) {
                return s.to_owned();
            }
        }
        let mut non_summary_iter = map.iter().filter(|(k, _)| k.as_str() != "toolSummary");
        if let Some((_, val)) = non_summary_iter.next()
            && non_summary_iter.next().is_none()
            && let Some(s) = val.as_str()
        {
            return s.to_owned();
        }
    }
    if arguments.is_null() {
        return String::new();
    }
    serde_json::to_string(arguments).unwrap_or_default()
}

/// Normalizes OpenAI (`call_<nonce>`) and Anthropic (`toolu_<nonce>`) tool call identifiers to
/// their shared `<nonce>` suffix so suspended MCP continuations match seamlessly across protocols.
#[must_use]
pub fn strip_call_prefix(raw_id: &str) -> &str {
    let trimmed = raw_id.trim();
    trimmed
        .strip_prefix("call_")
        .or_else(|| trimmed.strip_prefix("toolu_"))
        .unwrap_or(trimmed)
}

/// Truncates `s` on a valid UTF-8 boundary at or below `max_bytes`, returning `Cow::Borrowed` when unchanged.
#[must_use]
pub fn truncate_utf8_prefix(s: &str, max_bytes: usize) -> Cow<'_, str> {
    if s.len() <= max_bytes {
        return Cow::Borrowed(s);
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!(
        "{}...[truncated]",
        s.get(..end).unwrap_or_default()
    ))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExpectedJsonContainer {
    Array,
    Object,
}

fn schema_declares_container(prop_schema: &Value) -> Option<ExpectedJsonContainer> {
    if let Some(ty) = prop_schema.get("type") {
        match ty {
            Value::String(s) => match s.as_str() {
                "array" => return Some(ExpectedJsonContainer::Array),
                "object" => return Some(ExpectedJsonContainer::Object),
                _ => {}
            },
            Value::Array(arr) => {
                let has_string = arr.iter().any(|v| v.as_str() == Some("string"));
                if !has_string {
                    if arr.iter().any(|v| v.as_str() == Some("array")) {
                        return Some(ExpectedJsonContainer::Array);
                    }
                    if arr.iter().any(|v| v.as_str() == Some("object")) {
                        return Some(ExpectedJsonContainer::Object);
                    }
                }
            }
            _ => {}
        }
    }
    for union_key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(branches)) = prop_schema.get(union_key) {
            let allows_string = branches
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("string"));
            if !allows_string {
                for branch in branches {
                    if let Some(kind) = schema_declares_container(branch) {
                        return Some(kind);
                    }
                }
            }
        }
    }
    None
}

/// Unwraps JSON-stringified `array` or `object` fields (e.g. `"edits": "[{\"path\":\"a.rs\"}]"`
/// emitted on nested tool calls) ONLY when the client tool's JSON Schema explicitly declares that
/// property as an `array` or `object` (and not a `string`).
pub fn coerce_stringified_container_fields(
    map: &mut serde_json::Map<String, Value>,
    parameters_json: &str,
) -> bool {
    let has_candidate = map.values().any(|v| {
        v.as_str().is_some_and(|s| {
            let t = s.trim();
            (t.starts_with('[') && t.ends_with(']')) || (t.starts_with('{') && t.ends_with('}'))
        })
    });
    if !has_candidate {
        return false;
    }

    let Ok(schema) = serde_json::from_str::<Value>(parameters_json) else {
        return false;
    };
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return false;
    };

    let mut mutated = false;
    for (prop_name, prop_schema) in props {
        let Some(expected) = schema_declares_container(prop_schema) else {
            continue;
        };
        let Some(slot) = map.get_mut(prop_name) else {
            continue;
        };
        let Some(raw_str) = slot.as_str() else {
            continue;
        };
        let trimmed = raw_str.trim();
        match expected {
            ExpectedJsonContainer::Array if trimmed.starts_with('[') && trimmed.ends_with(']') => {
                if let Ok(parsed @ Value::Array(_)) = serde_json::from_str::<Value>(trimmed) {
                    *slot = parsed;
                    mutated = true;
                }
            }
            ExpectedJsonContainer::Object if trimmed.starts_with('{') && trimmed.ends_with('}') => {
                if let Ok(parsed @ Value::Object(_)) = serde_json::from_str::<Value>(trimmed) {
                    *slot = parsed;
                    mutated = true;
                }
            }
            _ => {}
        }
    }
    mutated
}
