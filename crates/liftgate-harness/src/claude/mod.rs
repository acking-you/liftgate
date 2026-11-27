//! Claude Code (`claude`) persistent settings schema, context configuration, and binary resolver.

use std::fs;
use std::path::{Path, PathBuf};

use liftgate_types::{
    config::ModelId,
    model::{context_limits, resolve_model_id},
};
use serde_json::{Map, Value, json};

use crate::{DiscoveredModel, HarnessSyncParams};
pub mod invocation;
use crate::error::HarnessError;
use crate::fs_util::{
    backup_once_if_exists, merge_endpoint_no_proxy, read_existing_json_object, resolve_base_dir,
    user_home_dir, write_file_atomic,
};
use crate::resolve::{HarnessLookupSpec, ResolvedCommand, resolve_harness_command};
pub use invocation::*;

const MANAGED_CLAUDE_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "CLAUDE_CODE_AUTO_MODE_SERVER",
    "CLAUDE_CODE_EXPERIMENTAL_CODE_MODE",
    "NO_PROXY",
    "HTTP_PROXY",
    "HTTPS_PROXY",
];

/// Global Claude Code `autoCompactWindow` written for models absent from `modelSettings`.
const CLAUDE_FALLBACK_AUTO_COMPACT_WINDOW: u64 = 110_000;

pub fn build_claude_settings_map(params: &HarnessSyncParams) -> Map<String, Value> {
    let mut out = Map::new();
    merged_claude_settings_into(&mut out, params);
    out
}

/// Non-destructively merges Liftgate's Claude Code configuration into `existing`, preserving any
/// unrelated user settings (`hooks`, `mcpServers`, `enabledPlugins`, custom `env` entries).
pub fn merged_claude_settings_into(existing: &mut Map<String, Value>, params: &HarnessSyncParams) {
    strip_managed_claude_context(existing);
    existing.insert(
        "model".to_owned(),
        Value::String(params.primary_model.clone()),
    );
    if !params.no_bypass {
        existing.insert(
            "skipDangerousModePermissionPrompt".to_owned(),
            Value::Bool(true),
        );
        let perms = existing
            .entry("permissions".to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if !perms.is_object() {
            *perms = Value::Object(Map::new());
        }
        if let Value::Object(perm_map) = perms {
            perm_map.insert(
                "defaultMode".to_owned(),
                Value::String("bypassPermissions".to_owned()),
            );
        }
    } else {
        existing.remove("skipDangerousModePermissionPrompt");
        if let Some(Value::Object(perm_map)) = existing.get_mut("permissions") {
            if perm_map.get("defaultMode").and_then(Value::as_str) == Some("bypassPermissions") {
                perm_map.remove("defaultMode");
            }
            if perm_map.is_empty() {
                existing.remove("permissions");
            }
        }
    }

    let no_proxy = merge_endpoint_no_proxy(
        std::env::var("NO_PROXY")
            .ok()
            .or_else(|| std::env::var("no_proxy").ok())
            .as_deref(),
        Some(&params.gateway_base_url),
    );

    let env_entry = existing
        .entry("env".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !env_entry.is_object() {
        *env_entry = Value::Object(Map::new());
    }
    if let Value::Object(env_map) = env_entry {
        env_map.insert(
            "ANTHROPIC_BASE_URL".to_owned(),
            Value::String(params.gateway_base_url.clone()),
        );
        env_map.insert(
            "ANTHROPIC_AUTH_TOKEN".to_owned(),
            Value::String(params.api_key.clone()),
        );
        env_map.insert("ANTHROPIC_API_KEY".to_owned(), Value::String(String::new()));
        env_map.insert(
            "ANTHROPIC_MODEL".to_owned(),
            Value::String(params.primary_model.clone()),
        );
        env_map.insert(
            "ANTHROPIC_DEFAULT_FABLE_MODEL".to_owned(),
            Value::String(params.primary_model.clone()),
        );
        env_map.insert(
            "ANTHROPIC_DEFAULT_OPUS_MODEL".to_owned(),
            Value::String(params.primary_model.clone()),
        );
        env_map.insert(
            "ANTHROPIC_DEFAULT_SONNET_MODEL".to_owned(),
            Value::String(params.primary_model.clone()),
        );
        env_map.insert(
            "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_owned(),
            Value::String(params.fast_model.clone()),
        );
        env_map.insert(
            "ANTHROPIC_SMALL_FAST_MODEL".to_owned(),
            Value::String(params.fast_model.clone()),
        );
        // A process-wide override defeats per-model compaction on switches and subagents.
        env_map.remove("CLAUDE_CODE_MAX_CONTEXT_TOKENS");
        env_map.remove("CLAUDE_CODE_AUTO_COMPACT_WINDOW");
        env_map.insert(
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_owned(),
            Value::String("1".to_owned()),
        );
        env_map.insert(
            "CLAUDE_CODE_AUTO_MODE_SERVER".to_owned(),
            Value::String("0".to_owned()),
        );
        env_map.insert(
            "CLAUDE_CODE_EXPERIMENTAL_CODE_MODE".to_owned(),
            Value::String(if params.no_code_mode { "0" } else { "1" }.to_owned()),
        );
        env_map.insert("NO_PROXY".to_owned(), Value::String(no_proxy));
        env_map.remove("HTTP_PROXY");
        env_map.remove("HTTPS_PROXY");
    }

    let picker_models: Vec<DiscoveredModel> = if params.discovered_models.is_empty() {
        vec![DiscoveredModel {
            id: params.primary_model.clone(),
            display_name: params.primary_model.clone(),
            context_window: 0,
            auto_compact_token_limit: 0,
        }]
    } else {
        params.discovered_models.clone()
    };

    // Clear prior Liftgate-managed `modelSettings[*].autoCompactWindow` entries before replacing
    // `modelOverrides` and `modelPicker` so retired models or old aliases never linger as orphans.
    strip_managed_claude_context(existing);

    let mut model_overrides = Map::new();
    for (canonical, target) in [
        ("claude-fable-5-2", &params.primary_model),
        ("claude-opus-5-5", &params.primary_model),
        ("claude-sonnet-5-5", &params.primary_model),
        ("claude-haiku-5-5", &params.fast_model),
        ("claude-opus-4-6", &params.primary_model),
        ("claude-sonnet-4-6", &params.primary_model),
        ("claude-haiku-4-5-20251001", &params.fast_model),
    ] {
        let collides_with_distinct_picker_model = picker_models
            .iter()
            .any(|m| m.id.eq_ignore_ascii_case(canonical) && !m.id.eq_ignore_ascii_case(target));
        if !collides_with_distinct_picker_model {
            model_overrides.insert(canonical.to_owned(), Value::String(target.clone()));
        }
    }
    existing.insert("modelOverrides".to_owned(), Value::Object(model_overrides));

    let options: Vec<Value> = picker_models
        .iter()
        .map(|m| {
            let lower = m.id.to_ascii_lowercase();
            let behaves_as = if lower.starts_with("gpt-oss") {
                "claude-haiku-4-5-20251001"
            } else if lower.starts_with("gemini-") {
                if lower.contains("flash-low")
                    || lower.contains("hydrogen-low")
                    || lower.contains("neon-low")
                {
                    "claude-haiku-5-5"
                } else if lower.ends_with("-medium") {
                    "claude-sonnet-5-5"
                } else {
                    "claude-opus-5-5"
                }
            } else if lower.contains("sonnet") || lower.ends_with("-medium") {
                "claude-sonnet-4-6"
            } else {
                "claude-opus-4-6"
            };
            let ctx_k = format!("{}k ctx", m.context_limits().0 / 1_000);
            let label = if m.display_name.is_empty() {
                m.id.clone()
            } else {
                m.display_name.clone()
            };
            json!({
                "model": m.id,
                "label": label,
                "description": format!("{ctx_k} \u{00b7} Antigravity ACP ({})", m.id),
                "behavesAs": behaves_as,
            })
        })
        .collect();

    existing.insert(
        "modelPicker".to_owned(),
        json!({
            "replaceBuiltInOptions": true,
            "options": options,
        }),
    );
    merge_claude_context_settings(existing, params);
}

/// Claude resolves modelSettings through canonical model names. Cover both exact gateway IDs
/// and every canonical alias, taking the smallest window when several IDs share a capability map.
fn merge_claude_context_settings(existing: &mut Map<String, Value>, params: &HarnessSyncParams) {
    let mut windows = std::collections::BTreeMap::<String, u64>::new();
    let available: Vec<_> = params
        .discovered_models
        .iter()
        .map(DiscoveredModel::to_available_model)
        .collect();
    let fallback_model =
        ModelId::try_new(&params.primary_model).unwrap_or_else(|_| ModelId::fallback_primary());
    let resolve = |id: &str| {
        if let Some(exact) = params
            .discovered_models
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(id))
        {
            return exact.context_limits().1;
        }
        if !available.is_empty() {
            let mapped = resolve_model_id(Some(id), None, &available, &fallback_model);
            if let Some(hit) = params
                .discovered_models
                .iter()
                .find(|m| m.id.eq_ignore_ascii_case(mapped.as_str()))
            {
                return hit.context_limits().1;
            }
        }
        context_limits(id).1
    };
    let mut add = |id: &str, limit: u64| {
        windows
            .entry(id.to_owned())
            .and_modify(|old| *old = (*old).min(limit))
            .or_insert(limit);
    };
    for id in [&params.primary_model, &params.fast_model] {
        add(id, resolve(id));
    }
    if let Some(Value::Array(options)) = existing.get("modelPicker").and_then(|v| v.get("options"))
    {
        for option in options {
            if let Some(id) = option.get("model").and_then(Value::as_str) {
                let limit = resolve(id);
                add(id, limit);
                if let Some(canonical) = option.get("behavesAs").and_then(Value::as_str) {
                    add(canonical, limit);
                }
            }
        }
    }
    if let Some(Value::Object(overrides)) = existing.get("modelOverrides") {
        for (canonical, id) in overrides {
            if let Some(id) = id.as_str() {
                add(canonical, resolve(id));
            }
        }
    }
    // Applies to unlisted models; known models override it in the same settings scope.
    // Never overwrite a user-chosen global value: it only acts as a fallback anyway.
    existing
        .entry("autoCompactWindow")
        .or_insert_with(|| json!(CLAUDE_FALLBACK_AUTO_COMPACT_WINDOW));
    let settings = existing.entry("modelSettings").or_insert_with(|| json!({}));
    if !settings.is_object() {
        *settings = json!({});
    }
    if let Value::Object(settings) = settings {
        for (id, limit) in windows {
            let model = settings.entry(id).or_insert_with(|| json!({}));
            if !model.is_object() {
                *model = json!({});
            }
            if let Value::Object(model) = model {
                model.insert("autoCompactWindow".to_owned(), json!(limit));
            }
        }
    }
}

/// Remove only the context fields associated with our saved picker and routing map.
fn strip_managed_claude_context(existing: &mut Map<String, Value>) {
    let mut ids = Vec::new();
    if let Some(Value::Array(options)) = existing.get("modelPicker").and_then(|v| v.get("options"))
    {
        for option in options {
            if !option
                .get("description")
                .and_then(Value::as_str)
                .is_some_and(|s| s.contains("Antigravity ACP"))
            {
                continue;
            }
            for key in ["model", "behavesAs"] {
                if let Some(id) = option.get(key).and_then(Value::as_str) {
                    ids.push(id.to_owned());
                }
            }
        }
    }
    // Without our picker there is nothing we wrote, so leave every context field untouched.
    let has_managed_picker = !ids.is_empty();
    if has_managed_picker && let Some(Value::Object(overrides)) = existing.get("modelOverrides") {
        for (canonical, id) in overrides {
            ids.push(canonical.clone());
            if let Some(id) = id.as_str() {
                ids.push(id.to_owned());
            }
        }
    }
    if let Some(Value::Object(settings)) = existing.get_mut("modelSettings") {
        for id in ids {
            if let Some(Value::Object(model)) = settings.get_mut(&id) {
                model.remove("autoCompactWindow");
                if model.is_empty() {
                    settings.remove(&id);
                }
            }
        }
        if settings.is_empty() {
            existing.remove("modelSettings");
        }
    }
    if has_managed_picker
        && existing.get("autoCompactWindow").and_then(Value::as_u64)
            == Some(CLAUDE_FALLBACK_AUTO_COMPACT_WINDOW)
    {
        existing.remove("autoCompactWindow");
    }
}

/// Removes Liftgate-managed keys from a Claude Code settings JSON map.
pub fn strip_claude_settings(existing: &mut Map<String, Value>) {
    strip_managed_claude_context(existing);
    existing.remove("modelOverrides");
    existing.remove("modelPicker");
    existing.remove("skipDangerousModePermissionPrompt");
    if let Some(Value::Object(perms)) = existing.get_mut("permissions") {
        if perms.get("defaultMode").and_then(Value::as_str) == Some("bypassPermissions") {
            perms.remove("defaultMode");
        }
        if perms.is_empty() {
            existing.remove("permissions");
        }
    }
    if let Some(Value::Object(env_map)) = existing.get_mut("env") {
        for &key in MANAGED_CLAUDE_ENV_KEYS {
            env_map.remove(key);
        }
        if env_map.is_empty() {
            existing.remove("env");
        }
    }
    existing.remove("model");
}

pub fn resolve_claude_config_path(
    global: bool,
    project_dir: Option<&Path>,
) -> Result<PathBuf, HarnessError> {
    if global {
        Ok(user_home_dir()?.join(".claude").join("settings.json"))
    } else {
        let base = resolve_base_dir(project_dir)?;
        Ok(base.join(".claude").join("settings.local.json"))
    }
}

pub fn write_or_update_claude_config(
    target_path: &Path,
    params: &HarnessSyncParams,
    print_only: bool,
) -> Result<String, HarnessError> {
    let mut map = read_existing_json_object(target_path)?;
    merged_claude_settings_into(&mut map, params);
    let mut rendered =
        serde_json::to_string_pretty(&Value::Object(map)).map_err(|source| HarnessError::Json {
            path: target_path.to_path_buf(),
            source,
        })?;
    rendered.push('\n');
    if !print_only {
        backup_once_if_exists(target_path)?;
        write_file_atomic(target_path, rendered.as_bytes(), true)?;
    }
    Ok(rendered)
}

pub fn uninstall_claude_config(target_path: &Path) -> Result<bool, HarnessError> {
    if !target_path.exists() {
        return Ok(false);
    }
    let mut map = read_existing_json_object(target_path)?;
    strip_claude_settings(&mut map);
    if map.is_empty() {
        fs::remove_file(target_path).map_err(|source| HarnessError::Io {
            path: target_path.to_path_buf(),
            source,
        })?;
        if let Some(parent) = target_path.parent() {
            let _ = fs::remove_dir(parent);
        }
    } else {
        let mut rendered = serde_json::to_string_pretty(&Value::Object(map)).map_err(|source| {
            HarnessError::Json {
                path: target_path.to_path_buf(),
                source,
            }
        })?;
        rendered.push('\n');
        write_file_atomic(target_path, rendered.as_bytes(), true)?;
    }
    Ok(true)
}

/// Resolves the `claude` CLI executable using the shared cross-platform harness resolver.
pub fn resolve_claude_command(explicit: Option<&Path>) -> Result<ResolvedCommand, HarnessError> {
    let spec = HarnessLookupSpec {
        tool: "claude",
        explicit_invalid_hint: "explicit `--claude-bin` path does not exist or is not executable",
        missing_install_hint: "install Claude Code (`npm i -g @anthropic-ai/claude-code`) or pass `--claude-bin <path>`",
        unix_bin_name: "claude",
        unix_home_rel_paths: &[
            &[".local", "bin", "claude"],
            &[".claude", "local", "claude"],
        ],
        windows_exe_name: "claude.exe",
        windows_extra_exe_candidates: windows_claude_exe_candidates(),
        windows_npm_script_rel: Some(&["node_modules", "@anthropic-ai", "claude-code", "cli.js"]),
        windows_cmd_shims: &["claude.cmd", "claude.bat"],
        windows_ps1_shims: &["claude.ps1"],
    };
    resolve_harness_command(explicit, &spec)
}

fn windows_claude_exe_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(home) = user_home_dir() {
        out.push(home.join(".local").join("bin").join("claude.exe"));
        out.push(home.join(".claude").join("local").join("claude.exe"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let local_path = PathBuf::from(local);
        out.push(
            local_path
                .join("Programs")
                .join("claude")
                .join("claude.exe"),
        );
        out.push(local_path.join("AnthropicClaude").join("claude.exe"));
    }
    out
}
