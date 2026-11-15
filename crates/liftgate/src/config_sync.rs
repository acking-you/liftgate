use std::fs;
use std::path::{Path, PathBuf};

use liftgate_server::platform::{canonicalize_clean, merge_loopback_no_proxy};
use liftgate_types::{
    config::ModelId,
    model::{context_limits, select_fastest_gemini_model},
    openai::build_codex_model_catalog,
};
use serde_json::{Map, Value, json};

use crate::cli::{
    ConfigArgs, ConfigTarget, PLACEHOLDER_API_KEY, ServeArgs, invoked_bin_name,
    normalize_optional_api_key,
};
use crate::client::DiscoveredModel;
use crate::daemon::{ensure_gateway_ready, resolve_client_api_key};
use crate::error::CliError;
use crate::state::{StatePaths, format_connect_url, user_home_dir, write_file_atomic};
use crate::ui::{self, Tone};

const MANAGED_CLAUDE_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "CLAUDE_CODE_AUTO_MODE_SERVER",
    "NO_PROXY",
    "HTTP_PROXY",
    "HTTPS_PROXY",
];

const MANAGED_CODEX_ROOT_KEYS: &[&str] = &[
    "model",
    "model_provider",
    "model_catalog_json",
    "approval_policy",
    "sandbox_mode",
    "web_search",
    "model_context_window",
    "model_auto_compact_token_limit",
];

#[derive(Clone, Debug)]
pub struct HarnessSyncParams {
    pub gateway_base_url: String,
    pub api_key: String,
    pub primary_model: String,
    pub fast_model: String,
    pub no_bypass: bool,
    pub long_context: bool,
    pub discovered_models: Vec<DiscoveredModel>,
}

/// Resolves the optimal fast/subagent model (`gemini-*-flash-low`) from the discovered live
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
    let fallback = ModelId::try_new(primary_model)
        .or_else(|_| ModelId::try_new("gemini-3.8-flash-low"))
        .expect("compile-time fallback model id is non-empty");
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
        env_map.insert("NO_PROXY".to_owned(), Value::String(no_proxy));
        env_map.remove("HTTP_PROXY");
        env_map.remove("HTTPS_PROXY");
    }

    existing.insert(
        "modelOverrides".to_owned(),
        json!({
            "claude-fable-5-2": params.primary_model,
            "claude-opus-5-5": params.primary_model,
            "claude-sonnet-5-5": params.primary_model,
            "claude-haiku-5-5": params.fast_model,
            "claude-opus-4-6": params.primary_model,
            "claude-sonnet-4-6": params.primary_model,
            "claude-haiku-4-5-20251001": params.fast_model,
        }),
    );

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

    let options: Vec<Value> = picker_models
        .iter()
        .map(|m| {
            let lower = m.id.to_ascii_lowercase();
            let behaves_as = if lower.starts_with("gpt-oss") {
                "claude-haiku-4-5-20251001"
            } else if lower.starts_with("gemini-") {
                if lower.contains("flash-low") {
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
            let ctx_k = format!("{}k ctx", m.context_limits(params.long_context).0 / 1_000);
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
    let resolve = |id: &str| {
        params
            .discovered_models
            .iter()
            .find(|m| m.id == id)
            .map_or_else(
                || context_limits(id, params.long_context).1,
                |m| m.context_limits(params.long_context).1,
            )
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
    existing.insert("autoCompactWindow".to_owned(), json!(110_000));
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
    if !ids.is_empty()
        && let Some(Value::Object(overrides)) = existing.get("modelOverrides")
    {
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
    if existing.get("autoCompactWindow").and_then(Value::as_u64) == Some(110_000) {
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

/// Builds fallback Codex model catalog bytes offline when the gateway is not queried.
pub fn build_offline_codex_catalog_bytes(
    discovered: &[DiscoveredModel],
    primary_model: &str,
    long_context: bool,
) -> Result<Vec<u8>, CliError> {
    let available: Vec<_> = discovered
        .iter()
        .map(DiscoveredModel::to_available_model)
        .collect();
    let mut catalog = build_codex_model_catalog(&available, primary_model, long_context);
    for entry in &mut catalog.models {
        if let Some(model) = discovered.iter().find(|m| m.id == entry.slug) {
            let (window, compact) = model.context_limits(long_context);
            entry.context_window = i64::try_from(window).unwrap_or(i64::MAX);
            entry.max_context_window = entry.context_window;
            entry.auto_compact_token_limit = i64::try_from(compact).unwrap_or(i64::MAX);
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

/// Non-destructively merges Liftgate's Codex provider (`[model_providers.agy]`) and root settings
/// into an existing `config.toml` string, keeping top-level keys before the first `[table]` header
/// and preserving all unrelated `[mcp_servers.*]`, `[projects.*]`, and `[profiles.*]` sections.
#[must_use]
pub fn merge_codex_config_toml(
    existing: &str,
    gateway_base_url: &str,
    primary_model: &str,
    catalog_path: &Path,
    require_env_key: bool,
    no_bypass: bool,
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

    let stripped_tables = remove_toml_section(tables_part, "model_providers.agy");
    let trimmed_tables = stripped_tables.trim();
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
    out
}

/// Strips Liftgate's `[model_providers.agy]` section and managed root keys from `config.toml`.
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

    let stripped_tables = remove_toml_section(tables_part, "model_providers.agy");
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

pub async fn run_config(state_paths: &StatePaths, args: ConfigArgs) -> Result<(), CliError> {
    let want_claude = matches!(args.target, ConfigTarget::All | ConfigTarget::Claude);
    let want_codex = matches!(args.target, ConfigTarget::All | ConfigTarget::Codex);

    if args.uninstall {
        if want_claude {
            let claude_path = resolve_claude_config_path(args.global, args.dir.as_deref())?;
            if uninstall_claude_config(&claude_path)? {
                ui::headline(
                    Tone::Ok,
                    &format!("Removed Liftgate settings from {}", claude_path.display()),
                );
            } else {
                ui::headline(
                    Tone::Muted,
                    &format!("No Liftgate settings found at {}", claude_path.display()),
                );
            }
        }
        if want_codex {
            let (codex_cfg, codex_cat) = resolve_codex_paths(args.global, args.dir.as_deref())?;
            if uninstall_codex_config(&codex_cfg, &codex_cat)? {
                ui::headline(
                    Tone::Ok,
                    &format!("Removed Liftgate provider from {}", codex_cfg.display()),
                );
            } else {
                ui::headline(
                    Tone::Muted,
                    &format!("No Liftgate provider found at {}", codex_cfg.display()),
                );
            }
        }
        return Ok(());
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
    let long_context = if live_target.is_some() {
        // Live discovery below carries the running gateway's policy. Never let saved preferences
        // turn a missing/old live policy into an experimental one.
        false
    } else {
        default_serve
            .long_context
            .unwrap_or(state_paths.read_settings()?.long_context)
    };
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
        .map(str::trim)
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
        args.fast_model.as_deref(),
        &primary_model,
        &discovered_models,
    );

    if want_claude {
        let claude_path = resolve_claude_config_path(args.global, args.dir.as_deref())?;
        let sync_params = HarnessSyncParams {
            gateway_base_url: gateway_base_url.clone(),
            api_key: claude_auth_token,
            primary_model: primary_model.clone(),
            fast_model: fast_model.clone(),
            no_bypass: args.no_bypass,
            long_context,
            discovered_models: discovered_models.clone(),
        };
        let rendered = write_or_update_claude_config(&claude_path, &sync_params, args.print)?;
        if args.print {
            println!("# --- {} ---", claude_path.display());
            print!("{rendered}");
        } else {
            ui::headline(Tone::Ok, "Updated Claude Code configuration");
            ui::kv_accent("Settings File", &claude_path.display().to_string());
            ui::kv(
                "Model Routing",
                &format!("primary={primary_model} · fast={fast_model}"),
            );
        }
    }

    if want_codex {
        let (codex_cfg, codex_cat) = resolve_codex_paths(args.global, args.dir.as_deref())?;
        let catalog_bytes = if let Some(t) = &live_target {
            match t.client.fetch_codex_catalog_json(&primary_model).await {
                Ok(bytes) => bytes,
                Err(_) => build_offline_codex_catalog_bytes(
                    &discovered_models,
                    &primary_model,
                    long_context,
                )?,
            }
        } else {
            build_offline_codex_catalog_bytes(&discovered_models, &primary_model, long_context)?
        };

        let rendered = write_or_update_codex_config(&CodexSyncRequest {
            config_path: &codex_cfg,
            catalog_path: &codex_cat,
            catalog_bytes: &catalog_bytes,
            gateway_base_url: &gateway_base_url,
            primary_model: &primary_model,
            require_env_key,
            no_bypass: args.no_bypass,
            print_only: args.print,
        })?;
        if args.print {
            println!("# --- {} ---", codex_cfg.display());
            print!("{rendered}");
        } else {
            let bin = invoked_bin_name();
            ui::headline(Tone::Ok, "Updated Codex CLI configuration");
            ui::kv_accent("Config File", &codex_cfg.display().to_string());
            ui::kv("Model Catalog", &codex_cat.display().to_string());
            ui::kv("Default Model", &primary_model);
            if !args.global {
                ui::note(
                    Tone::Info,
                    &format!(
                        "Codex CLI skips untrusted project `.codex/config.toml` until trusted; pass `--global` for `~/.codex/config.toml` or run `{bin} cx` for zero-config launch."
                    ),
                );
            }
            if require_env_key {
                ui::note(
                    Tone::Warn,
                    &format!(
                        "Gateway API key auth is active; export `AGY_GATEWAY_API_KEY` for bare `codex`, or run `{bin} cx` directly."
                    ),
                );
            }
        }
    }

    Ok(())
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

fn remove_toml_section(tables_part: &str, target_section: &str) -> String {
    let mut out = String::with_capacity(tables_part.len());
    let mut skipping = false;
    for line in tables_part.split_inclusive('\n') {
        if let Some(sec_name) = is_toml_section_header(line) {
            skipping = sec_name == target_section;
        }
        if !skipping {
            out.push_str(line);
        }
    }
    out
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
            long_context: false,
            discovered_models: vec![DiscoveredModel {
                id: "gemini-4-carbon-high".to_owned(),
                display_name: "Gemini 4 Carbon (High)".to_owned(),
                context_window: 280_000,
                auto_compact_token_limit: 245_000,
            }],
        };

        merged_claude_settings_into(&mut existing, &params);
        assert_eq!(existing["alwaysThinkingEnabled"], true);
        assert_eq!(existing["env"]["CUSTOM_USER_VAR"], "keep-me");
        assert_eq!(
            existing["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:8787"
        );
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
            245000
        );
        let merged_once = existing.clone();
        merged_claude_settings_into(&mut existing, &params);
        assert_eq!(existing, merged_once);

        let mut standard_perms = params.clone();
        standard_perms.no_bypass = true;
        merged_claude_settings_into(&mut existing, &standard_perms);
        assert!(existing.get("skipDangerousModePermissionPrompt").is_none());
        assert!(existing.get("permissions").is_none());

        strip_claude_settings(&mut existing);
        assert_eq!(existing["alwaysThinkingEnabled"], true);
        assert_eq!(existing["env"]["CUSTOM_USER_VAR"], "keep-me");
        assert!(existing["env"].get("ANTHROPIC_BASE_URL").is_none());
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
    fn context_policy_tracks_live_models_and_limits_shared_claude_aliases() {
        let models: Vec<DiscoveredModel> = serde_json::from_value(json!([
            {"id": "gemini-4-carbon-high", "context_window": 1000000, "auto_compact_token_limit": 850000},
            {"id": "gemini-3.8-flash-low", "context_window": 1000000, "auto_compact_token_limit": 850000},
            {"id": "gpt-oss-120b", "context_window": 128000, "auto_compact_token_limit": 110000},
            {"id": "claude-opus-5-5", "context_window": 200000, "auto_compact_token_limit": 170000}
        ])).unwrap();
        let params = HarnessSyncParams {
            gateway_base_url: "http://127.0.0.1:8787".to_owned(),
            api_key: "local-agy".to_owned(),
            primary_model: "gemini-4-carbon-high".to_owned(),
            fast_model: "gemini-3.8-flash-low".to_owned(),
            no_bypass: true,
            long_context: false,
            discovered_models: models,
        };
        let settings = build_claude_settings_map(&params);
        assert_eq!(settings["autoCompactWindow"], 110000);
        assert_eq!(
            settings["modelSettings"]["gemini-4-carbon-high"]["autoCompactWindow"],
            850000
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
            &build_offline_codex_catalog_bytes(
                &params.discovered_models,
                &params.primary_model,
                false,
            )
            .unwrap(),
        )
        .unwrap();
        let catalog_models = catalog["models"].as_array().unwrap();
        let gemini = catalog_models
            .iter()
            .find(|m| m["slug"] == params.primary_model)
            .unwrap();
        assert_eq!(gemini["context_window"], 1000000);
        assert_eq!(gemini["auto_compact_token_limit"], 850000);
        let legacy: DiscoveredModel = serde_json::from_value(json!({
            "id": "gemini-4-carbon-high", "context_window": 1000000
        }))
        .unwrap();
        assert_eq!(legacy.context_limits(false), (280000, 245000));
    }

    #[test]
    fn codex_toml_merge_is_idempotent_keeps_root_before_tables_and_uninstalls_cleanly() {
        let original = "# User header\ncustom_flag = true\nmodel_context_window = 1000000\nmodel_auto_compact_token_limit = 850000\n\n[projects.\"D:\\\\repo\"]\ntrust_level = \"trusted\"\n\n[mcp_servers.demo]\ncommand = \"npx\"\n";
        let catalog = Path::new(r"C:\Users\alice\.codex\agy-models.json");

        let merged_once = merge_codex_config_toml(
            original,
            "http://127.0.0.1:8787",
            "gemini-4-carbon-high",
            catalog,
            true,
            false,
        );
        let merged_twice = merge_codex_config_toml(
            &merged_once,
            "http://127.0.0.1:8787",
            "gemini-4-carbon-high",
            catalog,
            true,
            false,
        );
        assert_eq!(merged_once, merged_twice);

        // Root keys MUST precede the first `[` table header.
        let first_table_pos = merged_twice.find("\n[").unwrap();
        let model_pos = merged_twice
            .find("model = \"gemini-4-carbon-high\"")
            .unwrap();
        assert!(model_pos < first_table_pos);
        assert!(merged_twice.contains("C:/Users/alice/.codex/agy-models.json"));
        assert!(merged_twice.contains("[mcp_servers.demo]"));
        assert!(merged_twice.contains("custom_flag = true"));
        assert!(!merged_twice.contains("model_context_window"));
        assert!(!merged_twice.contains("model_auto_compact_token_limit"));

        let stripped = strip_codex_config_toml(&merged_twice);
        assert!(!stripped.contains("[model_providers.agy]"));
        assert!(!stripped.contains("model_provider"));
        assert!(stripped.contains("custom_flag = true"));
        assert!(stripped.contains("[mcp_servers.demo]"));
    }

    #[test]
    fn relative_project_dir_resolves_to_absolute_codex_catalog_path() {
        let (_, catalog) = resolve_codex_paths(false, Some(Path::new("."))).unwrap();
        assert!(catalog.is_absolute());
    }
}
