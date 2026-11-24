//! Deterministic YAML scalar quoting and `serde_json::Value` block rendering.

use serde_json::Value;

/// Quotes a YAML scalar when needed while keeping clean identifiers/paths unquoted.
#[must_use]
pub fn yaml_quote_scalar(s: &str) -> String {
    if !s.is_empty()
        && !matches!(
            s.to_ascii_lowercase().as_str(),
            "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "~"
        )
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.'))
        && !s.starts_with('.')
        && !s.starts_with('-')
        && s.parse::<f64>().is_err()
    {
        return s.to_owned();
    }
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{}\"", s.replace('"', "\\\"")))
}

/// Renders any [`serde_json::Value`] as a block-formatted YAML entry `<indent><key>: ...`,
/// using `step_indent` for nested mappings and `list_indent + 2` for object fields inside `- ` items.
#[must_use]
pub fn render_json_entry_as_yaml(
    key: &str,
    value: &Value,
    key_indent: usize,
    step_indent: usize,
) -> String {
    let step = step_indent.max(2);
    let mut out = String::new();
    write_yaml_key_value(&mut out, key, value, key_indent, step);
    out
}

fn write_yaml_key_value(out: &mut String, key: &str, value: &Value, indent: usize, step: usize) {
    let pad = " ".repeat(indent);
    let quoted_key = yaml_quote_key_if_needed(key);
    match value {
        Value::Null => {
            out.push_str(&format!("{pad}{quoted_key}: null\n"));
        }
        Value::Bool(b) => {
            out.push_str(&format!("{pad}{quoted_key}: {b}\n"));
        }
        Value::Number(n) => {
            out.push_str(&format!("{pad}{quoted_key}: {n}\n"));
        }
        Value::String(s) => {
            out.push_str(&format!("{pad}{quoted_key}: {}\n", yaml_quote_scalar(s)));
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str(&format!("{pad}{quoted_key}: {{}}\n"));
            } else {
                out.push_str(&format!("{pad}{quoted_key}:\n"));
                for (ck, cv) in map {
                    write_yaml_key_value(out, ck, cv, indent + step, step);
                }
            }
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                out.push_str(&format!("{pad}{quoted_key}: []\n"));
            } else {
                out.push_str(&format!("{pad}{quoted_key}:\n"));
                let item_indent = indent + step;
                let item_pad = " ".repeat(item_indent);
                for elem in arr {
                    match elem {
                        Value::Null => out.push_str(&format!("{item_pad}- null\n")),
                        Value::Bool(b) => out.push_str(&format!("{item_pad}- {b}\n")),
                        Value::Number(n) => out.push_str(&format!("{item_pad}- {n}\n")),
                        Value::String(s) => {
                            out.push_str(&format!("{item_pad}- {}\n", yaml_quote_scalar(s)));
                        }
                        Value::Object(obj) if !obj.is_empty() => {
                            let mut iter = obj.iter();
                            if let Some((first_k, first_v)) = iter.next() {
                                let qk = yaml_quote_key_if_needed(first_k);
                                match first_v {
                                    Value::Null => {
                                        out.push_str(&format!("{item_pad}- {qk}: null\n"));
                                    }
                                    Value::Bool(b) => {
                                        out.push_str(&format!("{item_pad}- {qk}: {b}\n"));
                                    }
                                    Value::Number(n) => {
                                        out.push_str(&format!("{item_pad}- {qk}: {n}\n"));
                                    }
                                    Value::String(s) => {
                                        out.push_str(&format!(
                                            "{item_pad}- {qk}: {}\n",
                                            yaml_quote_scalar(s)
                                        ));
                                    }
                                    Value::Object(_) | Value::Array(_) => {
                                        out.push_str(&format!("{item_pad}-\n"));
                                        write_yaml_key_value(
                                            out,
                                            first_k,
                                            first_v,
                                            item_indent + 2,
                                            step,
                                        );
                                    }
                                }
                                for (rest_k, rest_v) in iter {
                                    write_yaml_key_value(
                                        out,
                                        rest_k,
                                        rest_v,
                                        item_indent + 2,
                                        step,
                                    );
                                }
                            }
                        }
                        Value::Object(_) => out.push_str(&format!("{item_pad}- {{}}\n")),
                        Value::Array(_) => {
                            let compact =
                                serde_json::to_string(elem).unwrap_or_else(|_| "[]".to_owned());
                            out.push_str(&format!("{item_pad}- {compact}\n"));
                        }
                    }
                }
            }
        }
    }
}

pub(crate) fn yaml_quote_key_if_needed(key: &str) -> String {
    if !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !key.starts_with('-')
    {
        key.to_owned()
    } else {
        serde_json::to_string(key).unwrap_or_else(|_| format!("\"{key}\""))
    }
}
