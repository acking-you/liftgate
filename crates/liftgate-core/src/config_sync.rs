use std::fs;
use std::path::{Path, PathBuf};

use liftgate_server::platform::{canonicalize_clean, merge_loopback_no_proxy};
use liftgate_types::{
    config::{DEFAULT_TURN_TIMEOUT_SECS, ModelId},
    model::{context_limits, resolve_model_id, select_fastest_gemini_model},
    openai::{build_codex_model_catalog_with_tool_mode, codex_truncation_limit_bytes},
};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::cli::{
    ConfigArgs, ConfigTarget, PLACEHOLDER_API_KEY, ServeArgs, normalize_optional_api_key,
};
use crate::client::DiscoveredModel;
use crate::daemon::{ensure_gateway_ready, resolve_client_api_key};
use crate::error::CliError;
use crate::state::{StatePaths, format_connect_url, user_home_dir, write_file_atomic};

/// Structured outcome for a single harness target (`claude` or `codex`) during `config` sync.
#[derive(Clone, Debug, Serialize)]
pub struct ConfigSyncTargetResult {
    pub target: &'static str,
    pub action: &'static str,
    pub config_path: PathBuf,
    pub catalog_path: Option<PathBuf>,
    pub primary_model: Option<String>,
    pub fast_model: Option<String>,
    pub rendered_preview: Option<String>,
    pub project_untrusted_codex_note: bool,
    pub require_env_key_warn: bool,
}

/// Complete structured outcome returned by [`execute_config_sync`].
#[derive(Clone, Debug, Serialize)]
pub struct ConfigSyncOutcome {
    pub global: bool,
    pub results: Vec<ConfigSyncTargetResult>,
}

/// Live inspection snapshot of persistent Claude Code and Codex CLI configuration files.
#[derive(Clone, Debug, Serialize)]
pub struct ConfigSyncInspection {
    pub global: bool,
    pub claude_path: PathBuf,
    pub claude_exists: bool,
    pub claude_managed: bool,
    pub claude_model: Option<String>,
    pub claude_base_url: Option<String>,
    pub codex_path: PathBuf,
    pub codex_catalog_path: PathBuf,
    pub codex_exists: bool,
    pub codex_managed: bool,
    pub codex_model: Option<String>,
    pub opencode_path: PathBuf,
    pub opencode_exists: bool,
    pub opencode_managed: bool,
    pub opencode_model: Option<String>,
}

pub const OPENCODE_PROVIDER_ID: &str = "agy";
const OPENCODE_SCHEMA_URL: &str = "https://opencode.ai/config.json";
const OPENCODE_DEFAULT_MAX_OUTPUT_TOKENS: u64 = 64_000;

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

const MANAGED_CODEX_ROOT_KEYS: &[&str] = &[
    "model",
    "model_provider",
    "model_catalog_json",
    "approval_policy",
    "sandbox_mode",
    "web_search",
    "suppress_unstable_features_warning",
    "model_context_window",
    "model_auto_compact_token_limit",
];

/// Additional top-level keys stripped when synthesizing an isolated Codex App `CODEX_HOME` so an
/// existing ChatGPT subscription login, OS keyring credential store, startup update check, or named
/// profile in `~/.codex/config.toml` cannot override Liftgate's local provider routing or mutate
/// shared MSIX runtime state.
const ISOLATED_CODEX_APP_EXTRA_ROOT_KEYS: &[&str] = &[
    "cli_auth_credentials_store",
    "forced_login_method",
    "forced_chatgpt_workspace_id",
    "check_for_update_on_startup",
    "openai_base_url",
    "chatgpt_base_url",
    "profile",
    "review_model",
];

/// `[features]` toggles forced to `false` inside an isolated `lg cxa` session so Codex App never
/// initiates ChatGPT OAuth remote-control tunnels, downloads 500MB+ unauthenticated primary
/// runtime MSIX updates over an existing signed-in runtime, or stalls startup for 30–43s waiting
/// on ChatGPT-authenticated remote plugin catalogs.
const ISOLATED_DESKTOP_DISABLED_FEATURES: &[&str] = &[
    "remote_control",
    "in_app_updates",
    "remote_plugin",
    "recommended_plugins",
];

#[derive(Clone, Debug)]
pub struct HarnessSyncParams {
    pub gateway_base_url: String,
    pub api_key: String,
    pub primary_model: String,
    pub fast_model: String,
    pub no_bypass: bool,
    pub no_code_mode: bool,
    pub discovered_models: Vec<DiscoveredModel>,
}

/// Strips a leading `agy/` provider prefix if the caller supplied `agy/<model>` instead of `<model>`.
#[must_use]
pub fn strip_agy_provider_prefix(model: &str) -> &str {
    let trimmed = model.trim();
    trimmed
        .strip_prefix("agy/")
        .filter(|rest| !rest.is_empty())
        .unwrap_or(trimmed)
}

/// Resolves the fast/subagent model from the discovered live
/// catalog unless an explicit `--fast-model` override was supplied.
#[must_use]
pub fn resolve_fast_model(
    explicit_fast: Option<&str>,
    primary_model: &str,
    discovered: &[DiscoveredModel],
) -> String {
    if let Some(exp) = explicit_fast.map(str::trim).filter(|s| !s.is_empty()) {
        return exp.to_owned();
    }
    let available: Vec<_> = discovered
        .iter()
        .map(DiscoveredModel::to_available_model)
        .collect();
    let fallback = ModelId::try_new(primary_model).unwrap_or_else(|_| ModelId::fallback_fast());
    select_fastest_gemini_model(&available, &fallback)
        .as_str()
        .to_owned()
}

/// Builds the complete Claude Code settings JSON object (used both for zero-side-effect ephemeral
/// `--settings` files and for non-destructive merging into `.claude/settings.local.json`).
#[must_use]
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

    let no_proxy = merge_loopback_no_proxy(
        std::env::var("NO_PROXY")
            .ok()
            .or_else(|| std::env::var("no_proxy").ok())
            .as_deref(),
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
) -> Result<PathBuf, CliError> {
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
) -> Result<String, CliError> {
    let mut map = read_existing_json_object(target_path)?;
    merged_claude_settings_into(&mut map, params);
    let mut rendered = serde_json::to_string_pretty(&Value::Object(map)).map_err(|source| {
        CliError::StateJson {
            path: target_path.to_path_buf(),
            source,
        }
    })?;
    rendered.push('\n');
    if !print_only {
        backup_once_if_exists(target_path)?;
        write_file_atomic(target_path, rendered.as_bytes(), true)?;
    }
    Ok(rendered)
}

pub fn uninstall_claude_config(target_path: &Path) -> Result<bool, CliError> {
    if !target_path.exists() {
        return Ok(false);
    }
    let mut map = read_existing_json_object(target_path)?;
    strip_claude_settings(&mut map);
    if map.is_empty() {
        fs::remove_file(target_path).map_err(|source| CliError::StateIo {
            path: target_path.to_path_buf(),
            source,
        })?;
        if let Some(parent) = target_path.parent() {
            let _ = fs::remove_dir(parent);
        }
    } else {
        let mut rendered = serde_json::to_string_pretty(&Value::Object(map)).map_err(|source| {
            CliError::StateJson {
                path: target_path.to_path_buf(),
                source,
            }
        })?;
        rendered.push('\n');
        write_file_atomic(target_path, rendered.as_bytes(), true)?;
    }
    Ok(true)
}

/// Builds a complete OpenCode JSON configuration object compatible with both OpenCode v1.x and
/// OpenCode v2.x (`ConfigNormalize` zero-diagnostic subset; used for `OPENCODE_CONFIG_CONTENT`
/// and non-destructive `opencode.json` sync).
#[must_use]
pub fn build_opencode_config_map(
    params: &HarnessSyncParams,
    v2_runtime: bool,
) -> Map<String, Value> {
    let mut out = Map::new();
    merged_opencode_config_into(&mut out, params, v2_runtime);
    out
}

/// Non-destructively merges Liftgate's `agy` provider, calibrated context limits, default model
/// selection, and permission / Code Mode policy into an existing OpenCode JSON object.
pub fn merged_opencode_config_into(
    existing: &mut Map<String, Value>,
    params: &HarnessSyncParams,
    v2_runtime: bool,
) {
    if !existing.contains_key("$schema") {
        existing.insert(
            "$schema".to_owned(),
            Value::String(OPENCODE_SCHEMA_URL.to_owned()),
        );
    }

    let primary = strip_agy_provider_prefix(&params.primary_model);
    let fast = strip_agy_provider_prefix(&params.fast_model);
    existing.insert(
        "model".to_owned(),
        Value::String(format!("{OPENCODE_PROVIDER_ID}/{primary}")),
    );
    existing.insert(
        "small_model".to_owned(),
        Value::String(format!("{OPENCODE_PROVIDER_ID}/{fast}")),
    );

    // Prevent v2 `ConfigNormalize` duplicate provider diagnostics if `providers.agy` also exists.
    if let Some(Value::Object(v2_providers)) = existing.get_mut("providers") {
        v2_providers.remove(OPENCODE_PROVIDER_ID);
        if v2_providers.is_empty() {
            existing.remove("providers");
        }
    }

    let mut models_map = Map::new();
    for dm in &params.discovered_models {
        let (window, _) = dm.context_limits();
        let display_name = if dm.display_name.is_empty() {
            dm.id.as_str()
        } else {
            dm.display_name.as_str()
        };
        models_map.insert(dm.id.clone(), opencode_model_entry(display_name, window));
    }
    for required_id in [primary, fast] {
        if !models_map.contains_key(required_id) {
            let (window, _) = context_limits(required_id);
            models_map.insert(
                required_id.to_owned(),
                opencode_model_entry(required_id, window),
            );
        }
    }

    let base_v1_url = format!("{}/v1", params.gateway_base_url.trim_end_matches('/'));
    let timeout_ms = DEFAULT_TURN_TIMEOUT_SECS.saturating_mul(1_000);
    let agy_provider = json!({
        "npm": "@ai-sdk/anthropic",
        "name": "Antigravity (Liftgate)",
        "options": {
            "baseURL": base_v1_url,
            "apiKey": params.api_key,
            "timeout": timeout_ms,
        },
        "models": Value::Object(models_map),
    });

    let provider_entry = existing
        .entry("provider".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !provider_entry.is_object() {
        *provider_entry = Value::Object(Map::new());
    }
    if let Value::Object(provider_map) = provider_entry {
        provider_map.insert(OPENCODE_PROVIDER_ID.to_owned(), agy_provider);
    }

    apply_opencode_permission_policy(existing, params.no_bypass, params.no_code_mode);

    if v2_runtime {
        let compaction_entry = existing
            .entry("compaction".to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if !compaction_entry.is_object() {
            *compaction_entry = Value::Object(Map::new());
        }
        if let Value::Object(comp_map) = compaction_entry {
            comp_map
                .entry("auto".to_owned())
                .or_insert(Value::Bool(true));
            comp_map.insert("background".to_owned(), Value::Bool(false));
        }
    }
}

fn opencode_model_entry(display_name: &str, context_window: u64) -> Value {
    json!({
        "name": display_name,
        "tool_call": true,
        "limit": {
            "context": context_window,
            "output": OPENCODE_DEFAULT_MAX_OUTPUT_TOKENS,
        },
        "modalities": {
            "input": ["text", "image", "pdf"],
            "output": ["text"],
        },
    })
}

fn apply_opencode_permission_policy(
    existing: &mut Map<String, Value>,
    no_bypass: bool,
    no_code_mode: bool,
) {
    if no_bypass && !no_code_mode {
        if let Some(Value::Object(perm_map)) = existing.get_mut("permission") {
            if perm_map.get("*").and_then(Value::as_str) == Some("allow") {
                perm_map.remove("*");
            }
            if perm_map.get("execute").and_then(Value::as_str) == Some("deny") {
                perm_map.remove("execute");
            }
            if perm_map.is_empty() {
                existing.remove("permission");
            }
        }
        return;
    }

    let next_perms = match existing.remove("permission") {
        Some(Value::Object(mut m)) => {
            m.remove("*");
            m.remove("execute");
            m
        }
        Some(Value::String(s)) if !s.is_empty() && no_bypass => {
            let mut m = Map::new();
            m.insert("*".to_owned(), Value::String(s));
            m
        }
        _ => Map::new(),
    };

    // OpenCode evaluates permission entries in last-match-wins order: insert `"*"` first, then
    // any user-preserved rules, and finally `"execute": "deny"` when `--no-code-mode` is active.
    let mut ordered = Map::new();
    if !no_bypass {
        ordered.insert("*".to_owned(), Value::String("allow".to_owned()));
    }
    for (k, v) in next_perms {
        ordered.insert(k, v);
    }
    if no_code_mode {
        ordered.insert("execute".to_owned(), Value::String("deny".to_owned()));
    }
    if !ordered.is_empty() {
        existing.insert("permission".to_owned(), Value::Object(ordered));
    }
}

pub fn strip_opencode_config(existing: &mut Map<String, Value>) {
    let prefix = format!("{OPENCODE_PROVIDER_ID}/");
    for key in ["model", "small_model"] {
        let managed = existing
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|v| v.starts_with(&prefix));
        if managed {
            existing.remove(key);
        }
    }
    for prov_key in ["provider", "providers"] {
        if let Some(Value::Object(prov_map)) = existing.get_mut(prov_key) {
            prov_map.remove(OPENCODE_PROVIDER_ID);
            if prov_map.is_empty() {
                existing.remove(prov_key);
            }
        }
    }
    if let Some(Value::Object(perm_map)) = existing.get_mut("permission") {
        if perm_map.get("*").and_then(Value::as_str) == Some("allow") {
            perm_map.remove("*");
        }
        if perm_map.get("execute").and_then(Value::as_str) == Some("deny") {
            perm_map.remove("execute");
        }
        if perm_map.is_empty() {
            existing.remove("permission");
        }
    }
    if existing.len() == 1
        && existing.get("$schema").and_then(Value::as_str) == Some(OPENCODE_SCHEMA_URL)
    {
        existing.remove("$schema");
    }
}

pub fn resolve_opencode_config_path(
    global: bool,
    project_dir: Option<&Path>,
) -> Result<PathBuf, CliError> {
    if global {
        if let Some(dir) = std::env::var_os("OPENCODE_CONFIG_DIR").filter(|s| !s.is_empty()) {
            return Ok(PathBuf::from(dir).join("opencode.json"));
        }
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
            return Ok(PathBuf::from(xdg).join("opencode").join("opencode.json"));
        }
        Ok(user_home_dir()?
            .join(".config")
            .join("opencode")
            .join("opencode.json"))
    } else {
        let base = resolve_base_dir(project_dir)?;
        Ok(base.join("opencode.json"))
    }
}

pub fn write_or_update_opencode_config(
    target_path: &Path,
    params: &HarnessSyncParams,
    print_only: bool,
) -> Result<String, CliError> {
    let mut map = read_existing_json_object(target_path)?;
    merged_opencode_config_into(&mut map, params, false);
    let mut rendered = serde_json::to_string_pretty(&Value::Object(map)).map_err(|source| {
        CliError::StateJson {
            path: target_path.to_path_buf(),
            source,
        }
    })?;
    rendered.push('\n');
    if !print_only {
        backup_once_if_exists(target_path)?;
        write_file_atomic(target_path, rendered.as_bytes(), true)?;
    }
    Ok(rendered)
}

pub fn uninstall_opencode_config(target_path: &Path) -> Result<bool, CliError> {
    if !target_path.exists() {
        return Ok(false);
    }
    let mut map = read_existing_json_object(target_path)?;
    strip_opencode_config(&mut map);
    if map.is_empty() {
        fs::remove_file(target_path).map_err(|source| CliError::StateIo {
            path: target_path.to_path_buf(),
            source,
        })?;
        if let Some(parent) = target_path.parent() {
            let _ = fs::remove_dir(parent);
        }
    } else {
        let mut rendered = serde_json::to_string_pretty(&Value::Object(map)).map_err(|source| {
            CliError::StateJson {
                path: target_path.to_path_buf(),
                source,
            }
        })?;
        rendered.push('\n');
        write_file_atomic(target_path, rendered.as_bytes(), true)?;
    }
    Ok(true)
}

/// Builds fallback Codex model catalog bytes when the gateway is not queried, with an optional
/// `tool_mode` override (`None` defaults to `"code_mode"`).
pub fn build_offline_codex_catalog_bytes_with_mode(
    discovered: &[DiscoveredModel],
    primary_model: &str,
    tool_mode: Option<&str>,
) -> Result<Vec<u8>, CliError> {
    let available: Vec<_> = discovered
        .iter()
        .map(DiscoveredModel::to_available_model)
        .collect();
    let mut catalog =
        build_codex_model_catalog_with_tool_mode(&available, primary_model, tool_mode);
    for entry in &mut catalog.models {
        if let Some(model) = discovered.iter().find(|m| m.id == entry.slug) {
            let (window, compact) = model.context_limits();
            entry.context_window = i64::try_from(window).unwrap_or(i64::MAX);
            entry.max_context_window = entry.context_window;
            entry.auto_compact_token_limit = i64::try_from(compact).unwrap_or(i64::MAX);
            entry.truncation_policy.limit = codex_truncation_limit_bytes(window);
            entry.description =
                format!("{}k ctx · Antigravity ACP ({})", window / 1_000, entry.slug);
        }
    }
    let mut bytes = serde_json::to_vec_pretty(&catalog).map_err(|source| CliError::StateJson {
        path: PathBuf::from("codex-models.json"),
        source,
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn resolve_codex_paths(
    global: bool,
    project_dir: Option<&Path>,
) -> Result<(PathBuf, PathBuf), CliError> {
    if global {
        let dir = user_home_dir()?.join(".codex");
        Ok((dir.join("config.toml"), dir.join("agy-models.json")))
    } else {
        let dir = resolve_base_dir(project_dir)?.join(".codex");
        Ok((dir.join("config.toml"), dir.join("agy-models.json")))
    }
}

/// Non-destructively merges Liftgate's Codex provider (`[model_providers.agy]`), `[features]`
/// Code Mode toggle, and root settings into an existing `config.toml` string, keeping top-level
/// keys before the first `[table]` header and preserving all unrelated `[mcp_servers.*]`,
/// `[projects.*]`, `[profiles.*]`, and user `[features]` keys.
#[must_use]
pub fn merge_codex_config_toml(
    existing: &str,
    gateway_base_url: &str,
    primary_model: &str,
    catalog_path: &Path,
    require_env_key: bool,
    no_bypass: bool,
    no_code_mode: bool,
) -> String {
    merge_codex_config_toml_with_mode(
        existing,
        gateway_base_url,
        primary_model,
        catalog_path,
        require_env_key,
        no_bypass,
        no_code_mode,
        false,
    )
}

/// Non-destructively merges Liftgate's Codex provider (`[model_providers.agy]`), `[features]`
/// Code Mode / Code Mode Only flags, and root settings into an existing `config.toml` string.
#[expect(
    clippy::too_many_arguments,
    reason = "config synthesis takes explicit orthogonal options without allocating an extra builder"
)]
#[must_use]
pub fn merge_codex_config_toml_with_mode(
    existing: &str,
    gateway_base_url: &str,
    primary_model: &str,
    catalog_path: &Path,
    require_env_key: bool,
    no_bypass: bool,
    no_code_mode: bool,
    code_mode_only: bool,
) -> String {
    let catalog_slash = catalog_path.to_string_lossy().replace('\\', "/");
    let (root_part, tables_part) = split_toml_root_and_tables(existing);

    let mut kept_root_lines: Vec<&str> = Vec::new();
    for line in root_part.lines() {
        if let Some(key) = parse_toml_assignment_key(line)
            && MANAGED_CODEX_ROOT_KEYS.contains(&key)
        {
            continue;
        }
        kept_root_lines.push(line);
    }
    while kept_root_lines.last().is_some_and(|l| l.trim().is_empty()) {
        kept_root_lines.pop();
    }

    let mut out = String::new();
    if !kept_root_lines.is_empty() {
        out.push_str(&kept_root_lines.join("\n"));
        out.push('\n');
    }

    out.push_str(&format!("model = {primary_model:?}\n"));
    out.push_str("model_provider = \"agy\"\n");
    out.push_str(&format!("model_catalog_json = {catalog_slash:?}\n"));
    if !no_bypass {
        out.push_str("approval_policy = \"never\"\n");
        out.push_str("sandbox_mode = \"danger-full-access\"\n");
    }
    out.push_str("web_search = \"live\"\n");
    if !no_code_mode {
        out.push_str("suppress_unstable_features_warning = true\n");
    }

    let merged_tables = rewrite_codex_tables(tables_part, !no_code_mode, code_mode_only, false);
    let trimmed_tables = merged_tables.trim();
    if !trimmed_tables.is_empty() {
        out.push('\n');
        out.push_str(trimmed_tables);
        out.push('\n');
    }

    let base_v1 = format!("{}/v1", gateway_base_url.trim_end_matches('/'));
    out.push_str("\n[model_providers.agy]\n");
    out.push_str("name = \"Liftgate\"\n");
    out.push_str(&format!("base_url = {base_v1:?}\n"));
    if require_env_key {
        out.push_str("env_key = \"AGY_GATEWAY_API_KEY\"\n");
    }
    out.push_str("wire_api = \"responses\"\n");
    out.push_str("requires_openai_auth = false\n");
    out
}

/// Synthesizes an isolated `config.toml` for **Codex App** (`lg cxa`) from a read-only copy of
/// `~/.codex/config.toml`, forcing file-only API-key credentials (`cli_auth_credentials_store = "file"`,
/// `forced_login_method = "api"`), disabling ChatGPT cloud `remote_control`, and embedding
/// `requires_openai_auth = false` + `experimental_bearer_token` under `[model_providers.agy]` so
/// the desktop `app-server` never falls back to the user's ChatGPT OAuth session or OS keyring.
#[expect(
    clippy::too_many_arguments,
    reason = "isolated desktop config synthesis accepts explicit session parameters directly"
)]
#[must_use]
pub fn merge_isolated_codex_app_config_toml(
    existing: &str,
    gateway_base_url: &str,
    primary_model: &str,
    catalog_path: &Path,
    auth_token: &str,
    no_bypass: bool,
    no_code_mode: bool,
    code_mode_only: bool,
) -> String {
    let catalog_slash = catalog_path.to_string_lossy().replace('\\', "/");
    let (root_part, tables_part) = split_toml_root_and_tables(existing);

    let mut kept_root_lines: Vec<&str> = Vec::new();
    for line in root_part.lines() {
        if let Some(key) = parse_toml_assignment_key(line)
            && (MANAGED_CODEX_ROOT_KEYS.contains(&key)
                || ISOLATED_CODEX_APP_EXTRA_ROOT_KEYS.contains(&key))
        {
            continue;
        }
        kept_root_lines.push(line);
    }
    while kept_root_lines.last().is_some_and(|l| l.trim().is_empty()) {
        kept_root_lines.pop();
    }

    let mut out = String::new();
    if !kept_root_lines.is_empty() {
        out.push_str(&kept_root_lines.join("\n"));
        out.push('\n');
    }

    out.push_str(&format!("model = {primary_model:?}\n"));
    out.push_str("model_provider = \"agy\"\n");
    out.push_str(&format!("model_catalog_json = {catalog_slash:?}\n"));
    out.push_str("cli_auth_credentials_store = \"file\"\n");
    out.push_str("forced_login_method = \"api\"\n");
    out.push_str("check_for_update_on_startup = false\n");
    if !no_bypass {
        out.push_str("approval_policy = \"never\"\n");
        out.push_str("sandbox_mode = \"danger-full-access\"\n");
    }
    out.push_str("web_search = \"live\"\n");
    if !no_code_mode {
        out.push_str("suppress_unstable_features_warning = true\n");
    }

    let merged_tables = rewrite_codex_tables(tables_part, !no_code_mode, code_mode_only, true);
    let trimmed_tables = merged_tables.trim();
    if !trimmed_tables.is_empty() {
        out.push('\n');
        out.push_str(trimmed_tables);
        out.push('\n');
    }

    let base_v1 = format!("{}/v1", gateway_base_url.trim_end_matches('/'));
    out.push_str("\n[model_providers.agy]\n");
    out.push_str("name = \"Liftgate\"\n");
    out.push_str(&format!("base_url = {base_v1:?}\n"));
    out.push_str("wire_api = \"responses\"\n");
    out.push_str("requires_openai_auth = false\n");
    out.push_str(&format!("experimental_bearer_token = {auth_token:?}\n"));
    out
}

/// Strips Liftgate's `[model_providers.agy]` section, managed `[features]` `code_mode` key, and
/// managed root keys from `config.toml`.
#[must_use]
pub fn strip_codex_config_toml(existing: &str) -> String {
    let (root_part, tables_part) = split_toml_root_and_tables(existing);
    let mut kept_root: Vec<&str> = Vec::new();
    for line in root_part.lines() {
        if let Some(key) = parse_toml_assignment_key(line)
            && MANAGED_CODEX_ROOT_KEYS.contains(&key)
        {
            continue;
        }
        kept_root.push(line);
    }
    while kept_root.last().is_some_and(|l| l.trim().is_empty()) {
        kept_root.pop();
    }

    let stripped_tables = rewrite_codex_tables(tables_part, false, false, false);
    let trimmed_tables = stripped_tables.trim();

    let mut out = String::new();
    if !kept_root.is_empty() {
        out.push_str(&kept_root.join("\n"));
        out.push('\n');
    }
    if !trimmed_tables.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(trimmed_tables);
        out.push('\n');
    }
    out
}

pub struct CodexSyncRequest<'a> {
    pub config_path: &'a Path,
    pub catalog_path: &'a Path,
    pub catalog_bytes: &'a [u8],
    pub gateway_base_url: &'a str,
    pub primary_model: &'a str,
    pub require_env_key: bool,
    pub no_bypass: bool,
    pub no_code_mode: bool,
    pub print_only: bool,
}

pub fn write_or_update_codex_config(req: &CodexSyncRequest<'_>) -> Result<String, CliError> {
    let existing = match fs::read_to_string(req.config_path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(CliError::StateIo {
                path: req.config_path.to_path_buf(),
                source,
            });
        }
    };
    let merged = merge_codex_config_toml(
        &existing,
        req.gateway_base_url,
        req.primary_model,
        req.catalog_path,
        req.require_env_key,
        req.no_bypass,
        req.no_code_mode,
    );
    if !req.print_only {
        write_file_atomic(req.catalog_path, req.catalog_bytes, false)?;
        backup_once_if_exists(req.config_path)?;
        write_file_atomic(req.config_path, merged.as_bytes(), false)?;
    }
    Ok(merged)
}

pub fn uninstall_codex_config(config_path: &Path, catalog_path: &Path) -> Result<bool, CliError> {
    let mut touched = false;
    if catalog_path.exists() {
        fs::remove_file(catalog_path).map_err(|source| CliError::StateIo {
            path: catalog_path.to_path_buf(),
            source,
        })?;
        touched = true;
    }
    if config_path.exists() {
        let existing = fs::read_to_string(config_path).map_err(|source| CliError::StateIo {
            path: config_path.to_path_buf(),
            source,
        })?;
        let stripped = strip_codex_config_toml(&existing);
        if stripped.trim().is_empty() {
            fs::remove_file(config_path).map_err(|source| CliError::StateIo {
                path: config_path.to_path_buf(),
                source,
            })?;
            if let Some(parent) = config_path.parent() {
                let _ = fs::remove_dir(parent);
            }
        } else {
            write_file_atomic(config_path, stripped.as_bytes(), false)?;
        }
        touched = true;
    } else if touched && let Some(parent) = catalog_path.parent() {
        let _ = fs::remove_dir(parent);
    }
    Ok(touched)
}

pub fn inspect_config_sync(
    global: bool,
    project_dir: Option<&Path>,
) -> Result<ConfigSyncInspection, CliError> {
    let claude_path = resolve_claude_config_path(global, project_dir)?;
    let claude_exists = claude_path.is_file();
    let claude_obj = if claude_exists {
        read_existing_json_object(&claude_path).unwrap_or_default()
    } else {
        Map::new()
    };
    let claude_base_url = claude_obj
        .get("env")
        .and_then(Value::as_object)
        .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let claude_managed = claude_base_url.is_some() || claude_obj.contains_key("modelPicker");
    let claude_model = claude_obj
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let (codex_path, codex_catalog_path) = resolve_codex_paths(global, project_dir)?;
    let codex_exists = codex_path.is_file();
    let codex_text = if codex_exists {
        fs::read_to_string(&codex_path).unwrap_or_default()
    } else {
        String::new()
    };
    let codex_managed = codex_text.contains("[model_providers.agy]")
        || codex_text.contains("model_provider = \"agy\"");
    let codex_model = codex_text.lines().find_map(|line| {
        let key = parse_toml_assignment_key(line)?;
        if key != "model" {
            return None;
        }
        let (_, rhs) = line.split_once('=')?;
        let val = rhs
            .split('#')
            .next()
            .unwrap_or(rhs)
            .trim()
            .trim_matches('"')
            .trim_matches('\'');
        (!val.is_empty()).then(|| val.to_owned())
    });

    let opencode_path = resolve_opencode_config_path(global, project_dir)?;
    let opencode_exists = opencode_path.is_file();
    let opencode_obj = if opencode_exists {
        read_existing_json_object(&opencode_path).unwrap_or_default()
    } else {
        Map::new()
    };
    let opencode_managed = opencode_obj
        .get("provider")
        .and_then(Value::as_object)
        .is_some_and(|p| p.contains_key(OPENCODE_PROVIDER_ID))
        || opencode_obj
            .get("providers")
            .and_then(Value::as_object)
            .is_some_and(|p| p.contains_key(OPENCODE_PROVIDER_ID));
    let opencode_model = opencode_obj
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);

    Ok(ConfigSyncInspection {
        global,
        claude_path,
        claude_exists,
        claude_managed,
        claude_model,
        claude_base_url,
        codex_path,
        codex_catalog_path,
        codex_exists,
        codex_managed,
        codex_model,
        opencode_path,
        opencode_exists,
        opencode_managed,
        opencode_model,
    })
}

pub async fn execute_config_sync(
    state_paths: &StatePaths,
    args: &ConfigArgs,
) -> Result<ConfigSyncOutcome, CliError> {
    let want_claude = matches!(args.target, ConfigTarget::All | ConfigTarget::Claude);
    let want_codex = matches!(args.target, ConfigTarget::All | ConfigTarget::Codex);
    let want_opencode = matches!(args.target, ConfigTarget::All | ConfigTarget::Opencode);
    let mut results = Vec::with_capacity(3);

    if args.uninstall {
        if want_claude {
            let claude_path = resolve_claude_config_path(args.global, args.dir.as_deref())?;
            let removed = uninstall_claude_config(&claude_path)?;
            results.push(ConfigSyncTargetResult {
                target: "claude",
                action: if removed { "removed" } else { "not_present" },
                config_path: claude_path,
                catalog_path: None,
                primary_model: None,
                fast_model: None,
                rendered_preview: None,
                project_untrusted_codex_note: false,
                require_env_key_warn: false,
            });
        }
        if want_codex {
            let (codex_cfg, codex_cat) = resolve_codex_paths(args.global, args.dir.as_deref())?;
            let removed = uninstall_codex_config(&codex_cfg, &codex_cat)?;
            results.push(ConfigSyncTargetResult {
                target: "codex",
                action: if removed { "removed" } else { "not_present" },
                config_path: codex_cfg,
                catalog_path: Some(codex_cat),
                primary_model: None,
                fast_model: None,
                rendered_preview: None,
                project_untrusted_codex_note: false,
                require_env_key_warn: false,
            });
        }
        if want_opencode {
            let opencode_path = resolve_opencode_config_path(args.global, args.dir.as_deref())?;
            let removed = uninstall_opencode_config(&opencode_path)?;
            results.push(ConfigSyncTargetResult {
                target: "opencode",
                action: if removed { "removed" } else { "not_present" },
                config_path: opencode_path,
                catalog_path: None,
                primary_model: None,
                fast_model: None,
                rendered_preview: None,
                project_untrusted_codex_note: false,
                require_env_key_warn: false,
            });
        }
        return Ok(ConfigSyncOutcome {
            global: args.global,
            results,
        });
    }

    let live_target = if args.offline {
        None
    } else {
        match ensure_gateway_ready(
            state_paths,
            args.listen,
            args.api_key.clone(),
            args.model.as_deref(),
            !args.print,
        )
        .await
        {
            Ok(target) => Some(target),
            Err(_) if args.print => None,
            Err(err) => return Err(err),
        }
    };

    let daemon = state_paths.read_daemon()?;
    let default_serve = ServeArgs::default();
    let resolved_listen = args
        .listen
        .or_else(|| daemon.as_ref().map(|d| d.listen))
        .unwrap_or(default_serve.listen);

    let gateway_base_url = if let Some(t) = &live_target {
        t.client.base_url().to_owned()
    } else {
        format_connect_url(resolved_listen)
    };

    let configured_api_key = if let Some(t) = &live_target {
        normalize_optional_api_key(t.api_key.as_deref())
    } else {
        resolve_client_api_key(args.api_key.as_deref(), daemon.as_ref(), resolved_listen)
    };
    let require_env_key = configured_api_key.is_some();
    let claude_auth_token = configured_api_key.unwrap_or_else(|| PLACEHOLDER_API_KEY.to_owned());

    let primary_model = args
        .model
        .as_deref()
        .map(strip_agy_provider_prefix)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| live_target.as_ref().map(|t| t.default_model.clone()))
        .or_else(|| daemon.as_ref().map(|d| d.default_model.clone()))
        .unwrap_or(default_serve.default_model);

    let discovered_models = if let Some(t) = &live_target {
        t.client.fetch_models().await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let fast_model = resolve_fast_model(
        args.fast_model.as_deref().map(strip_agy_provider_prefix),
        &primary_model,
        &discovered_models,
    );

    if want_claude {
        let claude_path = resolve_claude_config_path(args.global, args.dir.as_deref())?;
        let sync_params = HarnessSyncParams {
            gateway_base_url: gateway_base_url.clone(),
            api_key: claude_auth_token.clone(),
            primary_model: primary_model.clone(),
            fast_model: fast_model.clone(),
            no_bypass: args.no_bypass,
            no_code_mode: args.no_code_mode,
            discovered_models: discovered_models.clone(),
        };
        let rendered = write_or_update_claude_config(&claude_path, &sync_params, args.print)?;
        results.push(ConfigSyncTargetResult {
            target: "claude",
            action: if args.print { "printed" } else { "synced" },
            config_path: claude_path,
            catalog_path: None,
            primary_model: Some(primary_model.clone()),
            fast_model: Some(fast_model.clone()),
            rendered_preview: Some(rendered),
            project_untrusted_codex_note: false,
            require_env_key_warn: false,
        });
    }

    if want_codex {
        let (codex_cfg, codex_cat) = resolve_codex_paths(args.global, args.dir.as_deref())?;
        let requested_tool_mode = if args.no_code_mode {
            Some("direct")
        } else {
            Some("code_mode")
        };
        let catalog_bytes = if let Some(t) = &live_target {
            match t
                .client
                .fetch_codex_catalog_json_with_mode(&primary_model, requested_tool_mode)
                .await
            {
                Ok(bytes) => bytes,
                Err(_) => build_offline_codex_catalog_bytes_with_mode(
                    &discovered_models,
                    &primary_model,
                    requested_tool_mode,
                )?,
            }
        } else {
            build_offline_codex_catalog_bytes_with_mode(
                &discovered_models,
                &primary_model,
                requested_tool_mode,
            )?
        };

        let rendered = write_or_update_codex_config(&CodexSyncRequest {
            config_path: &codex_cfg,
            catalog_path: &codex_cat,
            catalog_bytes: &catalog_bytes,
            gateway_base_url: &gateway_base_url,
            primary_model: &primary_model,
            require_env_key,
            no_bypass: args.no_bypass,
            no_code_mode: args.no_code_mode,
            print_only: args.print,
        })?;
        results.push(ConfigSyncTargetResult {
            target: "codex",
            action: if args.print { "printed" } else { "synced" },
            config_path: codex_cfg,
            catalog_path: Some(codex_cat),
            primary_model: Some(primary_model.clone()),
            fast_model: Some(fast_model.clone()),
            rendered_preview: Some(rendered),
            project_untrusted_codex_note: !args.global,
            require_env_key_warn: require_env_key,
        });
    }

    if want_opencode {
        let opencode_path = resolve_opencode_config_path(args.global, args.dir.as_deref())?;
        let params = HarnessSyncParams {
            gateway_base_url: gateway_base_url.clone(),
            api_key: claude_auth_token,
            primary_model: primary_model.clone(),
            fast_model: fast_model.clone(),
            no_bypass: args.no_bypass,
            no_code_mode: args.no_code_mode,
            discovered_models: discovered_models.clone(),
        };
        let rendered = write_or_update_opencode_config(&opencode_path, &params, args.print)?;
        results.push(ConfigSyncTargetResult {
            target: "opencode",
            action: if args.print { "printed" } else { "synced" },
            config_path: opencode_path,
            catalog_path: None,
            primary_model: Some(primary_model),
            fast_model: Some(fast_model),
            rendered_preview: Some(rendered),
            project_untrusted_codex_note: false,
            require_env_key_warn: false,
        });
    }

    Ok(ConfigSyncOutcome {
        global: args.global,
        results,
    })
}

fn resolve_base_dir(explicit: Option<&Path>) -> Result<PathBuf, CliError> {
    let raw = match explicit {
        Some(dir) => std::path::absolute(dir).map_err(|source| CliError::StateIo {
            path: dir.to_path_buf(),
            source,
        })?,
        None => std::env::current_dir().map_err(|source| CliError::StateIo {
            path: PathBuf::from("."),
            source,
        })?,
    };
    Ok(canonicalize_clean(&raw))
}

fn read_existing_json_object(path: &Path) -> Result<Map<String, Value>, CliError> {
    let raw = match fs::read(path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(source) => {
            return Err(CliError::StateIo {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if raw.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    let val: Value = serde_json::from_slice(&raw).map_err(|source| CliError::StateJson {
        path: path.to_path_buf(),
        source,
    })?;
    match val {
        Value::Object(map) => Ok(map),
        _ => Ok(Map::new()),
    }
}

fn backup_once_if_exists(path: &Path) -> Result<(), CliError> {
    if !path.is_file() {
        return Ok(());
    }
    let mut bak_os = path.as_os_str().to_os_string();
    bak_os.push(".liftgate.bak");
    let bak_path = PathBuf::from(bak_os);
    if !bak_path.exists() {
        let bytes = fs::read(path).map_err(|source| CliError::StateIo {
            path: path.to_path_buf(),
            source,
        })?;
        write_file_atomic(&bak_path, &bytes, true)?;
    }
    Ok(())
}

fn split_toml_root_and_tables(content: &str) -> (&str, &str) {
    let mut byte_idx = 0usize;
    for line in content.split_inclusive('\n') {
        if is_toml_section_header(line).is_some() {
            return content.split_at(byte_idx);
        }
        byte_idx += line.len();
    }
    (content, "")
}

fn is_toml_section_header(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.starts_with('#') || !trimmed.starts_with('[') {
        return None;
    }
    let without_comment = trimmed.split('#').next().unwrap_or(trimmed).trim();
    if let Some(inner) = without_comment
        .strip_prefix("[[")
        .and_then(|s| s.strip_suffix("]]"))
    {
        Some(inner.trim())
    } else {
        without_comment
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .map(str::trim)
    }
}

fn parse_toml_assignment_key(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
        return None;
    }
    let (lhs, _) = trimmed.split_once('=')?;
    let key = lhs.trim().trim_matches('"').trim_matches('\'');
    (!key.is_empty()).then_some(key)
}

fn is_ephemeral_bundled_marketplace_table(sec_name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "marketplaces.openai-bundled",
        "marketplaces.\"openai-bundled\"",
        "marketplaces.'openai-bundled'",
    ];
    PREFIXES.iter().any(|prefix| {
        sec_name == *prefix
            || sec_name
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

fn rewrite_codex_tables(
    tables_part: &str,
    enable_code_mode: bool,
    enable_code_mode_only: bool,
    isolate_desktop_features: bool,
) -> String {
    let mut sections: Vec<(String, String, Vec<&str>)> = Vec::new();
    for line in tables_part.split_inclusive('\n') {
        if let Some(sec_name) = is_toml_section_header(line) {
            sections.push((sec_name.to_owned(), line.trim_end().to_owned(), Vec::new()));
        } else if let Some((_, _, body)) = sections.last_mut() {
            body.push(line);
        }
    }

    let mut rendered_sections: Vec<String> = Vec::new();
    let mut saw_features = false;

    for (sec_name, header, body) in sections {
        if sec_name == "model_providers.agy" || sec_name.starts_with("model_providers.agy.") {
            continue;
        }
        // `~/.codex/config.toml` records `[marketplaces.openai-bundled]` pointing into
        // `~/.codex/.tmp/bundled-marketplaces/openai-bundled`; copying that path into an
        // ephemeral `CODEX_HOME` makes Codex App reject its own ephemeral bundled marketplace
        // (`marketplace 'openai-bundled' is already added from a different source`).
        if isolate_desktop_features && is_ephemeral_bundled_marketplace_table(&sec_name) {
            continue;
        }
        if sec_name == "features" {
            saw_features = true;
            let mut kept: Vec<&str> = body
                .into_iter()
                .filter(|l| {
                    let key = parse_toml_assignment_key(l);
                    !matches!(key, Some("code_mode" | "code_mode_only"))
                        && !(isolate_desktop_features
                            && key.is_some_and(|k| ISOLATED_DESKTOP_DISABLED_FEATURES.contains(&k)))
                })
                .collect();
            while kept.last().is_some_and(|l| l.trim().is_empty()) {
                kept.pop();
            }
            let mut managed_feature_lines: Vec<String> = Vec::new();
            if enable_code_mode {
                managed_feature_lines.push("code_mode = true".to_owned());
                if enable_code_mode_only {
                    managed_feature_lines.push("code_mode_only = true".to_owned());
                }
            } else if isolate_desktop_features {
                managed_feature_lines.push("code_mode = false".to_owned());
                managed_feature_lines.push("code_mode_only = false".to_owned());
            }
            if isolate_desktop_features {
                for feat in ISOLATED_DESKTOP_DISABLED_FEATURES {
                    managed_feature_lines.push(format!("{feat} = false"));
                }
            }

            if !managed_feature_lines.is_empty() || kept.iter().any(|l| !l.trim().is_empty()) {
                let mut block = String::new();
                block.push_str(&header);
                for fl in &managed_feature_lines {
                    block.push('\n');
                    block.push_str(fl);
                }
                if !kept.is_empty() {
                    block.push('\n');
                    for l in kept {
                        block.push_str(l);
                    }
                }
                rendered_sections.push(block.trim_end().to_owned());
            }
            continue;
        }

        let mut block = String::new();
        block.push_str(&header);
        block.push('\n');
        for l in body {
            block.push_str(l);
        }
        rendered_sections.push(block.trim_end().to_owned());
    }

    if !saw_features && (enable_code_mode || isolate_desktop_features) {
        let mut block = String::from("[features]");
        if enable_code_mode {
            block.push_str("\ncode_mode = true");
            if enable_code_mode_only {
                block.push_str("\ncode_mode_only = true");
            }
        } else if isolate_desktop_features {
            block.push_str("\ncode_mode = false\ncode_mode_only = false");
        }
        if isolate_desktop_features {
            for feat in ISOLATED_DESKTOP_DISABLED_FEATURES {
                block.push_str(&format!("\n{feat} = false"));
            }
        }
        rendered_sections.push(block);
    }

    rendered_sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_settings_merge_and_uninstall_preserve_unrelated_user_keys() {
        let mut existing: Map<String, Value> = serde_json::from_value(json!({
            "alwaysThinkingEnabled": true,
            "modelSettings": {
                "gemini-4-carbon-high": { "effortLevel": "high" },
                "unrelated-model": { "autoCompactWindow": 450000, "effortLevel": "low" }
            },
            "env": {
                "CUSTOM_USER_VAR": "keep-me",
                "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "1000000",
                "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "1000000"
            }
        }))
        .unwrap();

        let params = HarnessSyncParams {
            gateway_base_url: "http://127.0.0.1:8787".to_owned(),
            api_key: "local-agy".to_owned(),
            primary_model: "gemini-4-carbon-high".to_owned(),
            fast_model: "gemini-3.8-flash-low".to_owned(),
            no_bypass: false,
            no_code_mode: false,
            discovered_models: vec![DiscoveredModel {
                id: "gemini-4-carbon-high".to_owned(),
                display_name: "Gemini 4 Carbon (High)".to_owned(),
                context_window: 500_000,
                auto_compact_token_limit: 425_000,
            }],
        };

        merged_claude_settings_into(&mut existing, &params);
        assert_eq!(existing["alwaysThinkingEnabled"], true);
        assert_eq!(existing["env"]["CUSTOM_USER_VAR"], "keep-me");
        assert_eq!(
            existing["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:8787"
        );
        assert_eq!(existing["env"]["CLAUDE_CODE_EXPERIMENTAL_CODE_MODE"], "1");
        assert_eq!(existing["model"], "gemini-4-carbon-high");
        assert_eq!(existing["skipDangerousModePermissionPrompt"], true);
        assert!(
            existing["env"]
                .get("CLAUDE_CODE_MAX_CONTEXT_TOKENS")
                .is_none()
        );
        assert!(
            existing["env"]
                .get("CLAUDE_CODE_AUTO_COMPACT_WINDOW")
                .is_none()
        );
        assert_eq!(
            existing["modelSettings"]["gemini-4-carbon-high"]["autoCompactWindow"],
            425000
        );
        let merged_once = existing.clone();
        merged_claude_settings_into(&mut existing, &params);
        assert_eq!(existing, merged_once);

        let mut standard_perms = params.clone();
        standard_perms.no_bypass = true;
        standard_perms.no_code_mode = true;
        merged_claude_settings_into(&mut existing, &standard_perms);
        assert!(existing.get("skipDangerousModePermissionPrompt").is_none());
        assert!(existing.get("permissions").is_none());
        assert_eq!(existing["env"]["CLAUDE_CODE_EXPERIMENTAL_CODE_MODE"], "0");

        strip_claude_settings(&mut existing);
        assert_eq!(existing["alwaysThinkingEnabled"], true);
        assert_eq!(existing["env"]["CUSTOM_USER_VAR"], "keep-me");
        assert!(existing["env"].get("ANTHROPIC_BASE_URL").is_none());
        assert!(
            existing["env"]
                .get("CLAUDE_CODE_EXPERIMENTAL_CODE_MODE")
                .is_none()
        );
        assert!(existing.get("modelOverrides").is_none());
        assert_eq!(
            existing["modelSettings"]["gemini-4-carbon-high"]["effortLevel"],
            "high"
        );
        assert!(
            existing["modelSettings"]["gemini-4-carbon-high"]
                .get("autoCompactWindow")
                .is_none()
        );
        assert_eq!(
            existing["modelSettings"]["unrelated-model"]["autoCompactWindow"],
            450000
        );
        assert!(existing.get("autoCompactWindow").is_none());
    }

    #[test]
    fn claude_global_auto_compact_window_is_only_managed_when_liftgate_wrote_it() {
        let params = HarnessSyncParams {
            gateway_base_url: "http://127.0.0.1:8787".to_owned(),
            api_key: "local-agy".to_owned(),
            primary_model: "gemini-4-carbon-high".to_owned(),
            fast_model: "gemini-3.8-flash-low".to_owned(),
            no_bypass: true,
            no_code_mode: false,
            discovered_models: Vec::new(),
        };

        // A user-chosen global window survives both merge and uninstall.
        let mut custom: Map<String, Value> =
            serde_json::from_value(json!({ "autoCompactWindow": 300000 })).unwrap();
        merged_claude_settings_into(&mut custom, &params);
        assert_eq!(custom["autoCompactWindow"], 300000);
        strip_claude_settings(&mut custom);
        assert_eq!(custom["autoCompactWindow"], 300000);

        // Without a Liftgate picker, a value equal to our fallback is still the user's.
        let mut unmanaged: Map<String, Value> =
            serde_json::from_value(json!({ "autoCompactWindow": 110000 })).unwrap();
        strip_claude_settings(&mut unmanaged);
        assert_eq!(unmanaged["autoCompactWindow"], 110000);

        // Our own fallback is written once and removed cleanly.
        let mut managed = Map::new();
        merged_claude_settings_into(&mut managed, &params);
        assert_eq!(managed["autoCompactWindow"], 110000);
        let merged_once = managed.clone();
        merged_claude_settings_into(&mut managed, &params);
        assert_eq!(managed, merged_once);
        strip_claude_settings(&mut managed);
        assert!(managed.get("autoCompactWindow").is_none());
    }

    #[test]
    fn context_policy_tracks_live_models_and_limits_shared_claude_aliases() {
        let models: Vec<DiscoveredModel> = serde_json::from_value(json!([
            {"id": "gemini-4-carbon-high", "context_window": 500000, "auto_compact_token_limit": 425000},
            {"id": "gemini-3.8-flash-low", "context_window": 500000, "auto_compact_token_limit": 425000},
            {"id": "gpt-oss-120b", "context_window": 128000, "auto_compact_token_limit": 110000},
            {"id": "claude-opus-5-5", "context_window": 200000, "auto_compact_token_limit": 170000}
        ])).unwrap();
        let params = HarnessSyncParams {
            gateway_base_url: "http://127.0.0.1:8787".to_owned(),
            api_key: "local-agy".to_owned(),
            primary_model: "gemini-4-carbon-high".to_owned(),
            fast_model: "gemini-3.8-flash-low".to_owned(),
            no_bypass: true,
            no_code_mode: false,
            discovered_models: models,
        };
        let settings = build_claude_settings_map(&params);
        assert_eq!(settings["autoCompactWindow"], 110000);
        assert_eq!(
            settings["modelSettings"]["gemini-4-carbon-high"]["autoCompactWindow"],
            425000
        );
        // The shared canonical ID must honor the smaller real Claude deployment.
        assert_eq!(
            settings["modelSettings"]["claude-opus-5-5"]["autoCompactWindow"],
            170000
        );
        assert_eq!(
            settings["modelSettings"]["claude-haiku-4-5-20251001"]["autoCompactWindow"],
            110000
        );
        assert_eq!(
            settings["modelSettings"]["gpt-oss-120b"]["autoCompactWindow"],
            110000
        );
        let catalog: Value = serde_json::from_slice(
            &build_offline_codex_catalog_bytes_with_mode(
                &params.discovered_models,
                &params.primary_model,
                None,
            )
            .unwrap(),
        )
        .unwrap();
        let catalog_models = catalog["models"].as_array().unwrap();
        let gemini = catalog_models
            .iter()
            .find(|m| m["slug"] == params.primary_model)
            .unwrap();
        assert_eq!(gemini["context_window"], 500000);
        assert_eq!(gemini["auto_compact_token_limit"], 425000);
        assert_eq!(gemini["truncation_policy"]["limit"], 80000);
        assert_eq!(gemini["tool_mode"], "code_mode");
        let gpt_oss = catalog_models
            .iter()
            .find(|m| m["slug"] == "gpt-oss-120b")
            .unwrap();
        assert_eq!(gpt_oss["truncation_policy"]["limit"], 40000);
        let legacy: DiscoveredModel = serde_json::from_value(json!({
            "id": "gemini-4-carbon-high", "context_window": 1000000
        }))
        .unwrap();
        assert_eq!(legacy.context_limits(), (500000, 425000));

        // Standard 500k live catalog containing both Gemini 4 Carbon (500k / 425k) and real
        // `claude-sonnet-4-6` (200k / 170k) keeps 425k on Gemini + `claude-opus-5-5` without
        // hijacking `claude-sonnet-4-6` in `modelOverrides`.
        let live_500k_models: Vec<DiscoveredModel> = serde_json::from_value(json!([
            {"id": "gemini-4-carbon-high", "context_window": 500000, "auto_compact_token_limit": 425000},
            {"id": "gemini-4-hydrogen-low", "context_window": 500000, "auto_compact_token_limit": 425000},
            {"id": "claude-sonnet-4-6", "context_window": 200000, "auto_compact_token_limit": 170000}
        ]))
        .unwrap();
        let live_500k_params = HarnessSyncParams {
            gateway_base_url: "http://127.0.0.1:8787".to_owned(),
            api_key: "local-agy".to_owned(),
            primary_model: "gemini-4-carbon-high".to_owned(),
            fast_model: "gemini-4-hydrogen-low".to_owned(),
            no_bypass: true,
            no_code_mode: false,
            discovered_models: live_500k_models,
        };
        let live_500k_settings = build_claude_settings_map(&live_500k_params);
        assert_eq!(
            live_500k_settings["modelSettings"]["gemini-4-carbon-high"]["autoCompactWindow"],
            425000
        );
        assert_eq!(
            live_500k_settings["modelSettings"]["claude-opus-5-5"]["autoCompactWindow"],
            425000
        );
        assert_eq!(
            live_500k_settings["modelSettings"]["claude-sonnet-4-6"]["autoCompactWindow"],
            170000
        );
        assert!(
            live_500k_settings["modelOverrides"]
                .get("claude-sonnet-4-6")
                .is_none()
        );
        assert_eq!(
            live_500k_settings["modelOverrides"]["claude-opus-5-5"],
            "gemini-4-carbon-high"
        );

        // Re-syncing over an older 280k/245k settings map cleans up retired model aliases while
        // preserving unrelated user-configured `modelSettings` entries.
        let mut stale_v020: Map<String, Value> = serde_json::from_value(json!({
            "modelOverrides": {
                "claude-opus-5-5": "gemini-4-argon",
                "claude-sonnet-4-6": "gemini-4-argon"
            },
            "modelPicker": {
                "replaceBuiltInOptions": true,
                "options": [{
                    "model": "gemini-4-argon-high",
                    "label": "Gemini 4 Argon (High)",
                    "description": "280k ctx · Antigravity ACP (gemini-4-argon-high)",
                    "behavesAs": "claude-opus-5-5"
                }]
            },
            "modelSettings": {
                "gemini-4-argon": { "autoCompactWindow": 245000 },
                "gemini-4-argon-high": { "autoCompactWindow": 245000 },
                "claude-sonnet-4-6": { "autoCompactWindow": 245000 },
                "unrelated-model": { "autoCompactWindow": 450000, "effortLevel": "low" }
            }
        }))
        .unwrap();
        merged_claude_settings_into(&mut stale_v020, &live_500k_params);
        assert!(stale_v020["modelSettings"].get("gemini-4-argon").is_none());
        assert!(
            stale_v020["modelSettings"]
                .get("gemini-4-argon-high")
                .is_none()
        );
        assert_eq!(
            stale_v020["modelSettings"]["claude-sonnet-4-6"]["autoCompactWindow"],
            170000
        );
        assert_eq!(
            stale_v020["modelSettings"]["gemini-4-carbon-high"]["autoCompactWindow"],
            425000
        );
        assert_eq!(
            stale_v020["modelSettings"]["unrelated-model"]["autoCompactWindow"],
            450000
        );

        // Alias primary models (`gemini-4-neon`) resolve against the live catalog entries.
        let alias_models: Vec<DiscoveredModel> = serde_json::from_value(json!([
            {"id": "gemini-4-neon-high", "context_window": 500000, "auto_compact_token_limit": 425000},
            {"id": "gemini-3.8-flash-low", "context_window": 500000, "auto_compact_token_limit": 425000}
        ]))
        .unwrap();
        let alias_params = HarnessSyncParams {
            gateway_base_url: "http://127.0.0.1:8787".to_owned(),
            api_key: "local-agy".to_owned(),
            primary_model: "gemini-4-neon".to_owned(),
            fast_model: "gemini-3.8-flash-low".to_owned(),
            no_bypass: true,
            no_code_mode: false,
            discovered_models: alias_models,
        };
        let alias_settings = build_claude_settings_map(&alias_params);
        assert_eq!(
            alias_settings["modelSettings"]["gemini-4-neon"]["autoCompactWindow"],
            425000
        );
        assert_eq!(
            alias_settings["modelSettings"]["claude-opus-5-5"]["autoCompactWindow"],
            425000
        );
    }

    #[test]
    fn codex_toml_merge_is_idempotent_keeps_root_before_tables_and_uninstalls_cleanly() {
        let original = "# User header\ncustom_flag = true\nmodel_context_window = 1000000\nmodel_auto_compact_token_limit = 850000\n\n[projects.\"D:\\\\repo\"]\ntrust_level = \"trusted\"\n\n[features]\nfast_mode = true\n\n[mcp_servers.demo]\ncommand = \"npx\"\n";
        let catalog = Path::new(r"C:\Users\alice\.codex\agy-models.json");

        let merged_once = merge_codex_config_toml(
            original,
            "http://127.0.0.1:8787",
            "gemini-4-carbon-high",
            catalog,
            true,
            false,
            false,
        );
        let merged_twice = merge_codex_config_toml(
            &merged_once,
            "http://127.0.0.1:8787",
            "gemini-4-carbon-high",
            catalog,
            true,
            false,
            false,
        );
        assert_eq!(merged_once, merged_twice);

        // Root keys MUST precede the first `[` table header.
        let first_table_pos = merged_twice.find("\n[").unwrap();
        let model_pos = merged_twice
            .find("model = \"gemini-4-carbon-high\"")
            .unwrap();
        assert!(model_pos < first_table_pos);
        assert!(merged_twice.contains("suppress_unstable_features_warning = true"));
        assert!(merged_twice.contains("code_mode = true"));
        assert!(merged_twice.contains("fast_mode = true"));
        assert!(merged_twice.contains("C:/Users/alice/.codex/agy-models.json"));
        assert!(merged_twice.contains("[mcp_servers.demo]"));
        assert!(merged_twice.contains("custom_flag = true"));
        assert!(!merged_twice.contains("model_context_window"));
        assert!(!merged_twice.contains("model_auto_compact_token_limit"));

        let stripped = strip_codex_config_toml(&merged_twice);
        assert!(!stripped.contains("[model_providers.agy]"));
        assert!(!stripped.contains("model_provider"));
        assert!(!stripped.contains("suppress_unstable_features_warning"));
        assert!(!stripped.contains("code_mode"));
        assert!(stripped.contains("fast_mode = true"));
        assert!(stripped.contains("custom_flag = true"));
        assert!(stripped.contains("[mcp_servers.demo]"));
    }

    #[test]
    fn relative_project_dir_resolves_to_absolute_codex_catalog_path() {
        let (_, catalog) = resolve_codex_paths(false, Some(Path::new("."))).unwrap();
        assert!(catalog.is_absolute());
    }

    #[test]
    fn isolated_codex_app_toml_forces_file_apikey_auth_and_strips_conflicting_user_auth_keys() {
        let user_toml = "\
# User settings
custom_flag = true
cli_auth_credentials_store = \"keyring\"
forced_login_method = \"chatgpt\"
forced_chatgpt_workspace_id = \"ws-123\"
check_for_update_on_startup = true
openai_base_url = \"https://example.invalid/v1\"
profile = \"work\"

[features]
remote_control = true
in_app_updates = true
remote_plugin = true
recommended_plugins = true
fast_mode = true

[marketplaces.openai-bundled]
source = \"C:\\\\Users\\\\alice\\\\.codex\\\\.tmp\\\\bundled-marketplaces\\\\openai-bundled\"

[marketplaces.openai-primary-runtime]
source = \"C:\\\\Users\\\\alice\\\\.cache\\\\codex-runtimes\\\\codex-primary-runtime\"

[model_providers.agy.auth]
command = \"echo bad\"

[mcp_servers.demo]
command = \"npx\"
";
        let catalog = Path::new(r"C:\Users\alice\.liftgate\desktop\sessions\s1\agy-models.json");
        let rendered = merge_isolated_codex_app_config_toml(
            user_toml,
            "http://127.0.0.1:8787",
            "gemini-4-carbon-high",
            catalog,
            "local-agy",
            false,
            false,
            false,
        );
        let rendered_again = merge_isolated_codex_app_config_toml(
            &rendered,
            "http://127.0.0.1:8787",
            "gemini-4-carbon-high",
            catalog,
            "local-agy",
            false,
            false,
            false,
        );
        assert_eq!(rendered, rendered_again);
        assert!(rendered.contains("cli_auth_credentials_store = \"file\""));
        assert!(rendered.contains("forced_login_method = \"api\""));
        assert!(rendered.contains("check_for_update_on_startup = false"));
        assert!(rendered.contains("requires_openai_auth = false"));
        assert!(rendered.contains("experimental_bearer_token = \"local-agy\""));
        assert!(rendered.contains("remote_control = false"));
        assert!(rendered.contains("in_app_updates = false"));
        assert!(rendered.contains("remote_plugin = false"));
        assert!(rendered.contains("recommended_plugins = false"));
        assert!(rendered.contains("fast_mode = true"));
        assert!(rendered.contains("[marketplaces.openai-primary-runtime]"));
        assert!(rendered.contains("[mcp_servers.demo]"));
        assert!(!rendered.contains("keyring"));
        assert!(!rendered.contains("forced_chatgpt_workspace_id"));
        assert!(!rendered.contains("check_for_update_on_startup = true"));
        assert!(!rendered.contains("openai_base_url"));
        assert!(!rendered.contains("profile ="));
        assert!(!rendered.contains("[marketplaces.openai-bundled]"));
        assert!(!rendered.contains("[model_providers.agy.auth]"));
        assert!(!rendered.contains("remote_control = true"));
        assert!(!rendered.contains("in_app_updates = true"));
        assert!(!rendered.contains("remote_plugin = true"));
        assert!(!rendered.contains("recommended_plugins = true"));
    }

    #[test]
    fn opencode_config_merge_and_uninstall_preserve_unrelated_user_fields() {
        let params = HarnessSyncParams {
            gateway_base_url: "http://127.0.0.1:8787".to_owned(),
            api_key: "local-agy".to_owned(),
            primary_model: "agy/gemini-4-carbon-high".to_owned(),
            fast_model: "gemini-4-hydrogen-low".to_owned(),
            no_bypass: false,
            no_code_mode: true,
            discovered_models: vec![DiscoveredModel {
                id: "gemini-4-carbon-high".to_owned(),
                display_name: "Gemini 4 Carbon (High)".to_owned(),
                context_window: 500_000,
                auto_compact_token_limit: 425_000,
            }],
        };

        let mut existing: Map<String, Value> = serde_json::from_value(json!({
            "theme": "tokyonight",
            "mcp": {
                "playwright": { "type": "local", "command": ["npx", "@playwright/mcp"] }
            },
            "providers": {
                "agy": { "name": "Stale v2 entry" },
                "openrouter": { "name": "OpenRouter" }
            },
            "permission": {
                "doom_loop": "ask"
            }
        }))
        .expect("valid json object");

        merged_opencode_config_into(&mut existing, &params, true);
        assert_eq!(
            existing.get("model").and_then(Value::as_str),
            Some("agy/gemini-4-carbon-high")
        );
        assert_eq!(
            existing.get("small_model").and_then(Value::as_str),
            Some("agy/gemini-4-hydrogen-low")
        );
        assert_eq!(
            existing.get("theme").and_then(Value::as_str),
            Some("tokyonight")
        );
        assert!(existing.get("mcp").is_some());
        // Stale `providers.agy` removed while `providers.openrouter` preserved
        assert!(existing["providers"].get("agy").is_none());
        assert!(existing["providers"].get("openrouter").is_some());
        assert_eq!(
            existing["provider"]["agy"]["npm"].as_str(),
            Some("@ai-sdk/anthropic")
        );
        assert_eq!(
            existing["provider"]["agy"]["options"]["baseURL"].as_str(),
            Some("http://127.0.0.1:8787/v1")
        );
        assert_eq!(
            existing["provider"]["agy"]["models"]["gemini-4-carbon-high"]["limit"]["context"]
                .as_u64(),
            Some(500_000)
        );
        // Last-match-wins permission ordering: `*` -> `doom_loop` -> `execute: deny`
        let perm_keys: Vec<&str> = existing["permission"]
            .as_object()
            .expect("permission object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(perm_keys, vec!["*", "doom_loop", "execute"]);
        assert_eq!(existing["compaction"]["background"].as_bool(), Some(false));

        // Uninstall removes `agy` provider & managed keys while keeping `theme`, `mcp`, `openrouter`, `doom_loop`
        existing.remove("compaction");
        strip_opencode_config(&mut existing);
        assert!(existing.get("model").is_none());
        assert!(existing.get("small_model").is_none());
        assert!(existing.get("provider").is_none());
        assert!(existing["providers"].get("openrouter").is_some());
        assert_eq!(existing["permission"]["doom_loop"].as_str(), Some("ask"));
        assert!(existing["permission"].get("*").is_none());
        assert!(existing["permission"].get("execute").is_none());
    }
}
