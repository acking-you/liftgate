//! Single-source Oh My Pi (`omp`) `providers.agy` data model shared across JSON/TS and `models.yml`.

use std::collections::HashSet;
use std::path::PathBuf;

use liftgate_types::{config::DEFAULT_TURN_TIMEOUT_SECS, model::context_limits};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::HarnessSyncParams;
use crate::error::HarnessError;
use crate::omp::{
    OMP_DEFAULT_MAX_OUTPUT_TOKENS, OMP_LOCAL_EXTENSION_FILENAME, OMP_MANAGED_EXTENSION_SENTINEL,
    OMP_PROVIDER_ID,
};
use crate::strip_agy_provider_prefix;
use crate::yaml_edit::render_json_entry_as_yaml;

const OMP_EXTENSION_EXPORT_HEAD: &str = "export default function liftgateAgyProvider(pi: { registerProvider(name: string, config: Record<string, unknown>): void }): void {";

/// Canonical `providers.agy` configuration matching `@oh-my-pi/pi-coding-agent`'s
/// `ModelsConfigSchema` and `pi.registerProvider("agy", ...)`.
///
/// Both [`build_omp_provider_json`] (used for TypeScript extensions) and
/// [`render_omp_provider_agy_yaml_block_indented`] (used for `~/.omp/agent/models.yml`) derive
/// directly from this single struct so JSON and YAML schemas can never diverge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OmpProviderConfig {
    pub base_url: String,
    pub api_key: String,
    pub api: String,
    pub auth_header: bool,
    pub compat: OmpCompatConfig,
    pub models: Vec<OmpModelConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OmpCompatConfig {
    pub supports_store: bool,
    pub stateful_responses: bool,
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub supports_reasoning_summary: bool,
    pub supports_tool_choice: bool,
    pub stream_idle_timeout_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OmpModelConfig {
    pub id: String,
    pub name: String,
    pub api: String,
    pub reasoning: bool,
    pub thinking: OmpThinkingConfig,
    pub input: Vec<String>,
    pub cost: OmpCostConfig,
    pub context_window: u64,
    pub max_tokens: u64,
    pub prefer_websockets: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OmpThinkingConfig {
    pub mode: String,
    pub efforts: Vec<String>,
    pub default_level: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OmpCostConfig {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

pub(crate) fn omp_model_supports_images(model_id: &str) -> bool {
    !model_id.trim().to_ascii_lowercase().starts_with("gpt-oss")
}

fn omp_default_effort_for_model(model_id: &str) -> &'static str {
    let lower = model_id.trim().to_ascii_lowercase();
    if lower.ends_with("-low")
        || lower.contains("flash-low")
        || lower.contains("hydrogen-low")
        || lower.contains("neon-low")
        || lower.contains("haiku")
    {
        "low"
    } else if lower.ends_with("-medium") {
        "medium"
    } else {
        "high"
    }
}

pub(crate) fn build_omp_provider_model(
    params: &HarnessSyncParams,
    api_key_override: Option<&str>,
) -> OmpProviderConfig {
    let base_v1_url = format!("{}/v1", params.gateway_base_url.trim_end_matches('/'));
    let timeout_ms = DEFAULT_TURN_TIMEOUT_SECS.saturating_mul(1_000);
    let api_key = api_key_override.unwrap_or(&params.api_key).to_owned();
    let primary = strip_agy_provider_prefix(&params.primary_model);
    let fast = strip_agy_provider_prefix(&params.fast_model);

    let mut seen: HashSet<String> = HashSet::new();
    let mut models: Vec<OmpModelConfig> = Vec::new();
    let mut push_model = |raw_id: &str, display_opt: Option<&str>, ctx_override: Option<u64>| {
        let id = raw_id.trim();
        if id.is_empty() || !seen.insert(id.to_owned()) {
            return;
        }
        let context_window = ctx_override.unwrap_or_else(|| context_limits(id).0);
        let name = display_opt
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(|| id.to_owned(), str::to_owned);
        let input = if omp_model_supports_images(id) {
            vec!["text".to_owned(), "image".to_owned()]
        } else {
            vec!["text".to_owned()]
        };
        models.push(OmpModelConfig {
            id: id.to_owned(),
            name,
            api: "openai-responses".to_owned(),
            reasoning: true,
            thinking: OmpThinkingConfig {
                mode: "effort".to_owned(),
                efforts: vec!["low".to_owned(), "medium".to_owned(), "high".to_owned()],
                default_level: omp_default_effort_for_model(id).to_owned(),
            },
            input,
            cost: OmpCostConfig {
                input: 0,
                output: 0,
                cache_read: 0,
                cache_write: 0,
            },
            context_window,
            max_tokens: OMP_DEFAULT_MAX_OUTPUT_TOKENS,
            prefer_websockets: false,
        });
    };

    for dm in &params.discovered_models {
        let (ctx, _) = dm.context_limits();
        push_model(&dm.id, Some(&dm.display_name), Some(ctx));
    }
    push_model(primary, None, None);
    push_model(fast, None, None);

    OmpProviderConfig {
        base_url: base_v1_url,
        api_key,
        api: "openai-responses".to_owned(),
        auth_header: true,
        compat: OmpCompatConfig {
            supports_store: false,
            stateful_responses: false,
            supports_developer_role: true,
            supports_reasoning_effort: true,
            supports_reasoning_summary: true,
            supports_tool_choice: true,
            stream_idle_timeout_ms: timeout_ms,
        },
        models,
    }
}

/// Builds the JSON representation of Oh My Pi's `ProviderConfig` (`providers.agy`).
#[must_use]
pub fn build_omp_provider_json(
    params: &HarnessSyncParams,
    api_key_override: Option<&str>,
) -> Value {
    let model = build_omp_provider_model(params, api_key_override);
    serde_json::to_value(model).unwrap_or(Value::Null)
}

/// Builds a self-contained Oh My Pi TypeScript extension registering the `agy` provider via
/// `pi.registerProvider("agy", ...)` (used by `lg omp` ephemeral launch and project `.omp/`).
pub fn build_omp_provider_extension_ts(
    params: &HarnessSyncParams,
    api_key_override: Option<&str>,
) -> Result<String, HarnessError> {
    let provider_val = build_omp_provider_json(params, api_key_override);
    let pretty_json =
        serde_json::to_string_pretty(&provider_val).map_err(|source| HarnessError::Json {
            path: PathBuf::from(OMP_LOCAL_EXTENSION_FILENAME),
            source,
        })?;
    Ok(format!(
        "{OMP_MANAGED_EXTENSION_SENTINEL}\n{OMP_EXTENSION_EXPORT_HEAD}\n  pi.registerProvider({OMP_PROVIDER_ID:?}, {pretty_json});\n}}\n"
    ))
}

/// Renders `providers.agy` as a 2-space indented YAML block (`  agy:\n    ...`).
#[must_use]
pub fn render_omp_provider_agy_yaml_block(
    params: &HarnessSyncParams,
    api_key_override: Option<&str>,
) -> String {
    render_omp_provider_agy_yaml_block_indented(params, api_key_override, 2, 2)
}

/// Renders `providers.agy` at arbitrary `key_indent` / `step_indent` (e.g. 4-space `models.yml`)
/// directly from `OmpProviderConfig`'s serialized JSON value so JSON and YAML schemas stay identical.
#[must_use]
pub fn render_omp_provider_agy_yaml_block_indented(
    params: &HarnessSyncParams,
    api_key_override: Option<&str>,
    key_indent: usize,
    step_indent: usize,
) -> String {
    let val = build_omp_provider_json(params, api_key_override);
    render_json_entry_as_yaml(OMP_PROVIDER_ID, &val, key_indent, step_indent)
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use crate::DiscoveredModel;
    use crate::yaml_edit::{
        line_indent, split_yaml_key_and_rhs, strip_yaml_inline_comment, unquote_yaml_scalar,
        validate_yaml_structure,
    };

    use super::*;

    /// Minimal recursive block-YAML-to-JSON parser for round-trip provider schema verification.
    fn parse_block_yaml_lines_to_value(
        lines: &[&str],
        pos: &mut usize,
        min_indent: usize,
    ) -> Value {
        while *pos < lines.len() && strip_yaml_inline_comment(lines[*pos]).trim().is_empty() {
            *pos += 1;
        }
        let Some(&first) = lines.get(*pos) else {
            return Value::Null;
        };
        let indent = line_indent(first);
        if indent < min_indent {
            return Value::Null;
        }
        let first_trim = strip_yaml_inline_comment(first).trim();
        if first_trim.starts_with("- ") || first_trim == "-" {
            let mut arr = Vec::new();
            while let Some(&line) = lines.get(*pos) {
                let code = strip_yaml_inline_comment(line);
                let trimmed = code.trim();
                if trimmed.is_empty() {
                    *pos += 1;
                    continue;
                }
                let cur_indent = line_indent(line);
                if cur_indent < indent {
                    break;
                }
                if cur_indent != indent || !(trimmed == "-" || trimmed.starts_with("- ")) {
                    break;
                }
                let rest = trimmed.strip_prefix('-').unwrap_or("").trim();
                *pos += 1;
                if let Some((k, rhs)) = split_yaml_key_and_rhs(rest) {
                    let mut map = Map::new();
                    let v0 = if rhs.is_empty() {
                        parse_block_yaml_lines_to_value(lines, pos, indent + 3)
                    } else {
                        parse_yaml_scalar_value(rhs)
                    };
                    map.insert(k.to_owned(), v0);
                    if let Value::Object(more) =
                        parse_block_yaml_lines_to_value(lines, pos, indent + 2)
                    {
                        for (mk, mv) in more {
                            map.insert(mk, mv);
                        }
                    }
                    arr.push(Value::Object(map));
                } else {
                    arr.push(parse_yaml_scalar_value(rest));
                }
            }
            Value::Array(arr)
        } else {
            let mut map = Map::new();
            while let Some(&line) = lines.get(*pos) {
                let code = strip_yaml_inline_comment(line);
                let trimmed = code.trim();
                if trimmed.is_empty() {
                    *pos += 1;
                    continue;
                }
                let cur_indent = line_indent(line);
                if cur_indent < indent {
                    break;
                }
                if cur_indent != indent {
                    break;
                }
                let Some((k, rhs)) = split_yaml_key_and_rhs(trimmed) else {
                    break;
                };
                *pos += 1;
                let val = if rhs.is_empty() {
                    parse_block_yaml_lines_to_value(lines, pos, indent + 1)
                } else {
                    parse_yaml_scalar_value(rhs)
                };
                map.insert(k.to_owned(), val);
            }
            Value::Object(map)
        }
    }

    fn parse_yaml_scalar_value(rhs: &str) -> Value {
        match rhs {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            "null" | "~" => Value::Null,
            "[]" => Value::Array(Vec::new()),
            "{}" => Value::Object(Map::new()),
            _ => {
                if !rhs.starts_with('"')
                    && !rhs.starts_with('\'')
                    && let Ok(n) = rhs.parse::<u64>()
                {
                    Value::Number(n.into())
                } else {
                    Value::String(unquote_yaml_scalar(rhs))
                }
            }
        }
    }

    #[test]
    fn json_and_yaml_provider_renderers_produce_identical_deserialized_provider_config() {
        let params = HarnessSyncParams {
            gateway_base_url: "http://127.0.0.1:8787".to_owned(),
            api_key: "AGY_OMP_API_KEY".to_owned(),
            primary_model: "gemini-4-carbon-high".to_owned(),
            fast_model: "gemini-4-hydrogen-low".to_owned(),
            no_bypass: false,
            no_code_mode: false,
            discovered_models: vec![
                DiscoveredModel {
                    id: "gemini-4-carbon-high".to_owned(),
                    display_name: "Gemini 4 Carbon (High)".to_owned(),
                    context_window: 500_000,
                    auto_compact_token_limit: 425_000,
                },
                DiscoveredModel {
                    id: "gemini-4-hydrogen-low".to_owned(),
                    display_name: "Gemini 4 Hydrogen (Low)".to_owned(),
                    context_window: 500_000,
                    auto_compact_token_limit: 425_000,
                },
            ],
        };

        let expected = build_omp_provider_model(&params, None);
        let from_json: OmpProviderConfig =
            serde_json::from_value(build_omp_provider_json(&params, None)).unwrap();
        assert_eq!(from_json, expected);

        for (key_indent, step_indent) in [(2usize, 2usize), (4usize, 4usize)] {
            let yaml_block =
                render_omp_provider_agy_yaml_block_indented(&params, None, key_indent, step_indent);
            let full_doc = format!("providers:\n{yaml_block}");
            validate_yaml_structure(&full_doc).unwrap();

            let lines: Vec<&str> = full_doc.lines().collect();
            let mut pos = 0usize;
            let parsed_root = parse_block_yaml_lines_to_value(&lines, &mut pos, 0);
            let agy_val = parsed_root
                .get("providers")
                .and_then(|p| p.get(OMP_PROVIDER_ID))
                .cloned()
                .expect("parsed YAML must contain providers.agy");
            let from_yaml: OmpProviderConfig = serde_json::from_value(agy_val).unwrap();
            assert_eq!(
                from_yaml, from_json,
                "YAML provider at indent ({key_indent}, {step_indent}) must match JSON provider schema"
            );
        }
    }
}
