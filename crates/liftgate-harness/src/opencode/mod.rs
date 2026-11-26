//! OpenCode (`opencode`) persistent JSON/JSONC schema, displaced-provider ledger, and binary resolver.

pub mod invocation;
pub use invocation::*;

use std::fs;
use std::path::{Path, PathBuf};

use liftgate_types::{config::DEFAULT_TURN_TIMEOUT_SECS, model::context_limits};
use serde_json::{Map, Value, json};

use crate::error::HarnessError;
use crate::fs_util::{
    backup_once_if_exists, remove_key_preserving_order, resolve_base_dir, strip_jsonc_to_json,
    user_home_dir, write_file_atomic,
};
use crate::ledger::{
    OpenCodeManagedRecord, ledger_key_for_path, read_config_sync_ledger, write_config_sync_ledger,
};
use crate::resolve::{HarnessLookupSpec, ResolvedCommand, resolve_harness_command};
use crate::{HarnessSyncParams, strip_agy_provider_prefix};

/// Provider ID registered in `opencode.json`.
pub const OPENCODE_PROVIDER_ID: &str = "agy";
const OPENCODE_MANAGED_PROVIDER_NAME: &str = "Antigravity (Liftgate)";
const OPENCODE_MANAGED_PROVIDER_NPM: &str = "@ai-sdk/anthropic";
const OPENCODE_SCHEMA_URL: &str = "https://opencode.ai/config.json";
const OPENCODE_DEFAULT_MAX_OUTPUT_TOKENS: u64 = 64_000;

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
        let _ = remove_key_preserving_order(v2_providers, OPENCODE_PROVIDER_ID);
        if v2_providers.is_empty() {
            let _ = remove_key_preserving_order(existing, "providers");
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
        "npm": OPENCODE_MANAGED_PROVIDER_NPM,
        "name": OPENCODE_MANAGED_PROVIDER_NAME,
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
    apply_opencode_code_mode_policy(existing, params.no_code_mode);

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
            "input": ["text", "image"],
            "output": ["text"],
        },
    })
}

fn apply_opencode_permission_policy(
    existing: &mut Map<String, Value>,
    no_bypass: bool,
    no_code_mode: bool,
) {
    let override_wildcard = !no_bypass;
    let override_execute = no_code_mode;
    if !override_wildcard && !override_execute {
        return;
    }

    let had_permission_key = existing.contains_key("permission");
    let prev_val = existing.get_mut("permission").map(std::mem::take);
    let prev_perms = match prev_val {
        Some(Value::Object(m)) => m,
        Some(Value::String(s)) if !s.is_empty() && !override_wildcard => {
            let mut m = Map::new();
            m.insert("*".to_owned(), Value::String(s));
            m
        }
        _ => Map::new(),
    };

    // OpenCode evaluates `permission` entries in last-match-wins insertion order:
    // 1. `"*": "allow"` baseline when `!no_bypass`
    // 2. All non-overridden caller rules in their exact original order (preserving string/object
    //    `permission["*"]` when `no_bypass`, and pre-existing `execute` when `!no_code_mode`)
    // 3. `"execute": "deny"` last when `no_code_mode`
    let mut ordered = Map::new();
    if override_wildcard {
        ordered.insert("*".to_owned(), Value::String("allow".to_owned()));
    }
    for (k, v) in prev_perms {
        if override_wildcard && k == "*" {
            continue;
        }
        if override_execute && k == "execute" {
            continue;
        }
        ordered.insert(k, v);
    }
    if override_execute {
        ordered.insert("execute".to_owned(), Value::String("deny".to_owned()));
    }
    if ordered.is_empty() {
        if had_permission_key {
            let _ = remove_key_preserving_order(existing, "permission");
        }
    } else if let Some(slot) = existing.get_mut("permission") {
        *slot = Value::Object(ordered);
    } else {
        existing.insert("permission".to_owned(), Value::Object(ordered));
    }
}

/// Configures `experimental.full_codemode` in OpenCode's JSON config.
///
/// OpenCode v2 release builds default `full_codemode` to `false` unless explicitly enabled via
/// `experimental.full_codemode: true` or `OPENCODE_FLAGS=full_codemode`, leaving `read`, `shell`,
/// `edit`, `write`, `glob`, and `grep` outside `execute`. Setting `experimental.full_codemode`
/// to `true` by default (unless `no_code_mode` is set or the caller explicitly configured
/// `"full_codemode": false` for partial MCP-only Code Mode) activates full Code Mode across both
/// `lg oc` and `lg config opencode` while remaining ignored safely by OpenCode v1.x.
fn apply_opencode_code_mode_policy(existing: &mut Map<String, Value>, no_code_mode: bool) {
    if !no_code_mode
        && existing
            .get("experimental")
            .and_then(Value::as_object)
            .and_then(|exp| exp.get("full_codemode"))
            .and_then(Value::as_bool)
            == Some(false)
    {
        return;
    }
    let exp_entry = existing
        .entry("experimental".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !exp_entry.is_object() {
        *exp_entry = Value::Object(Map::new());
    }
    if let Value::Object(exp_map) = exp_entry {
        exp_map.insert("full_codemode".to_owned(), Value::Bool(!no_code_mode));
    }
}

/// Conservatively strips unambiguous Liftgate-owned OpenCode provider entries from `existing`
/// when no installation ledger is available.
///
/// Never mutates maps that do not carry Liftgate's `"Antigravity (Liftgate)"` provider
/// signature, and never deletes `permission` rules without an installation ledger proving Liftgate
/// wrote them.
pub fn strip_opencode_config(existing: &mut Map<String, Value>) {
    let _ = strip_opencode_config_conservative(existing);
}

fn strip_opencode_config_conservative(existing: &mut Map<String, Value>) -> bool {
    if !has_liftgate_managed_opencode_provider(existing) {
        return false;
    }
    let prefix = format!("{OPENCODE_PROVIDER_ID}/");
    for key in ["model", "small_model"] {
        let managed = existing
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|v| v.starts_with(&prefix));
        if managed {
            let _ = remove_key_preserving_order(existing, key);
        }
    }
    if let Some(Value::Object(prov_map)) = existing.get_mut("provider") {
        if prov_map
            .get(OPENCODE_PROVIDER_ID)
            .is_some_and(is_liftgate_managed_opencode_provider)
        {
            let _ = remove_key_preserving_order(prov_map, OPENCODE_PROVIDER_ID);
        }
        if prov_map.is_empty() {
            let _ = remove_key_preserving_order(existing, "provider");
        }
    }
    if existing.len() == 1
        && existing.get("$schema").and_then(Value::as_str) == Some(OPENCODE_SCHEMA_URL)
    {
        let _ = remove_key_preserving_order(existing, "$schema");
    }
    true
}

#[must_use]
fn is_liftgate_managed_opencode_provider(val: &Value) -> bool {
    val.as_object().is_some_and(|obj| {
        obj.get("name").and_then(Value::as_str) == Some(OPENCODE_MANAGED_PROVIDER_NAME)
            && obj.get("npm").and_then(Value::as_str) == Some(OPENCODE_MANAGED_PROVIDER_NPM)
            && obj
                .get("options")
                .and_then(Value::as_object)
                .and_then(|o| o.get("baseURL"))
                .is_some_and(Value::is_string)
    })
}

#[must_use]
pub fn has_liftgate_managed_opencode_provider(map: &Map<String, Value>) -> bool {
    map.get("provider")
        .and_then(Value::as_object)
        .and_then(|p| p.get(OPENCODE_PROVIDER_ID))
        .is_some_and(is_liftgate_managed_opencode_provider)
}

fn capture_opencode_managed_record(
    baseline: &Map<String, Value>,
    merged: &Map<String, Value>,
    existed_before_sync: bool,
    params: &HarnessSyncParams,
) -> OpenCodeManagedRecord {
    let wrote_schema = !baseline.contains_key("$schema")
        && merged.get("$schema").and_then(Value::as_str) == Some(OPENCODE_SCHEMA_URL);
    let wrote_model = merged
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let wrote_small_model = merged
        .get("small_model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let wrote_provider_agy = merged
        .get("provider")
        .and_then(Value::as_object)
        .and_then(|p| p.get(OPENCODE_PROVIDER_ID))
        .cloned()
        .unwrap_or(Value::Null);
    let previous_provider_agy = baseline
        .get("provider")
        .and_then(Value::as_object)
        .and_then(|p| p.get(OPENCODE_PROVIDER_ID))
        .cloned();
    let removed_v2_providers_agy = baseline
        .get("providers")
        .and_then(Value::as_object)
        .and_then(|p| p.get(OPENCODE_PROVIDER_ID))
        .cloned();

    let permission_was_absent = !baseline.contains_key("permission");
    let permission_was_shorthand = baseline
        .get("permission")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let previous_permission_wildcard = match baseline.get("permission") {
        Some(Value::Object(m)) => m.get("*").cloned(),
        Some(Value::String(s)) if !s.is_empty() => Some(Value::String(s.clone())),
        _ => None,
    };
    let previous_permission_execute = baseline
        .get("permission")
        .and_then(Value::as_object)
        .and_then(|m| m.get("execute"))
        .cloned();

    let wrote_permission_wildcard = (!params.no_bypass).then(|| Value::String("allow".to_owned()));
    let wrote_permission_execute = params
        .no_code_mode
        .then(|| Value::String("deny".to_owned()));
    let experimental_was_absent = !baseline.contains_key("experimental");
    let previous_experimental_full_codemode = baseline
        .get("experimental")
        .and_then(Value::as_object)
        .and_then(|m| m.get("full_codemode"))
        .cloned();
    let wrote_experimental_full_codemode = merged
        .get("experimental")
        .and_then(Value::as_object)
        .and_then(|m| m.get("full_codemode"))
        .cloned();

    OpenCodeManagedRecord {
        existed_before_sync,
        wrote_schema,
        previous_model: baseline.get("model").cloned(),
        wrote_model,
        previous_small_model: baseline.get("small_model").cloned(),
        wrote_small_model,
        wrote_provider_agy,
        previous_provider_agy,
        removed_v2_providers_agy,
        permission_was_absent,
        permission_was_shorthand,
        previous_permission_wildcard,
        wrote_permission_wildcard,
        previous_permission_execute,
        wrote_permission_execute,
        experimental_was_absent,
        previous_experimental_full_codemode,
        wrote_experimental_full_codemode,
    }
}

fn revert_opencode_managed_record(
    map: &mut Map<String, Value>,
    rec: &OpenCodeManagedRecord,
) -> bool {
    let snapshot_before = Value::Object(map.clone());

    if map.get("model").and_then(Value::as_str) == Some(rec.wrote_model.as_str()) {
        if let Some(ref prev) = rec.previous_model {
            map.insert("model".to_owned(), prev.clone());
        } else {
            let _ = remove_key_preserving_order(map, "model");
        }
    }
    if map.get("small_model").and_then(Value::as_str) == Some(rec.wrote_small_model.as_str()) {
        if let Some(ref prev) = rec.previous_small_model {
            map.insert("small_model".to_owned(), prev.clone());
        } else {
            let _ = remove_key_preserving_order(map, "small_model");
        }
    }
    if let Some(Value::Object(prov_map)) = map.get_mut("provider")
        && prov_map.get(OPENCODE_PROVIDER_ID) == Some(&rec.wrote_provider_agy)
    {
        if let Some(previous) = &rec.previous_provider_agy {
            prov_map.insert(OPENCODE_PROVIDER_ID.to_owned(), previous.clone());
        } else {
            let _ = remove_key_preserving_order(prov_map, OPENCODE_PROVIDER_ID);
            if prov_map.is_empty() {
                let _ = remove_key_preserving_order(map, "provider");
            }
        }
    }
    if let Some(ref prev_v2_agy) = rec.removed_v2_providers_agy {
        let providers_slot = map
            .entry("providers".to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(v2_map) = providers_slot
            && !v2_map.contains_key(OPENCODE_PROVIDER_ID)
        {
            v2_map.insert(OPENCODE_PROVIDER_ID.to_owned(), prev_v2_agy.clone());
        }
    }

    if rec.wrote_permission_wildcard.is_some() || rec.wrote_permission_execute.is_some() {
        let mut remove_permission_key = false;
        let mut restore_shorthand: Option<String> = None;
        if let Some(Value::Object(perm_map)) = map.get_mut("permission") {
            if let Some(ref wrote_wc) = rec.wrote_permission_wildcard
                && perm_map.get("*") == Some(wrote_wc)
            {
                if let Some(ref prev_wc) = rec.previous_permission_wildcard {
                    perm_map.insert("*".to_owned(), prev_wc.clone());
                } else {
                    let _ = remove_key_preserving_order(perm_map, "*");
                }
            }
            if let Some(ref wrote_ex) = rec.wrote_permission_execute
                && perm_map.get("execute") == Some(wrote_ex)
            {
                if let Some(ref prev_ex) = rec.previous_permission_execute {
                    perm_map.insert("execute".to_owned(), prev_ex.clone());
                } else {
                    let _ = remove_key_preserving_order(perm_map, "execute");
                }
            }
            if let Some(ref s) = rec.permission_was_shorthand
                && perm_map.len() == 1
                && perm_map.get("*").and_then(Value::as_str) == Some(s.as_str())
            {
                restore_shorthand = Some(s.clone());
            } else if perm_map.is_empty() && rec.permission_was_absent {
                remove_permission_key = true;
            }
        }
        if let Some(shorthand) = restore_shorthand {
            map.insert("permission".to_owned(), Value::String(shorthand));
        } else if remove_permission_key {
            let _ = remove_key_preserving_order(map, "permission");
        }
    }

    if let Some(ref wrote_fc) = rec.wrote_experimental_full_codemode {
        let mut remove_experimental_key = false;
        if let Some(Value::Object(exp_map)) = map.get_mut("experimental") {
            if exp_map.get("full_codemode") == Some(wrote_fc) {
                if let Some(ref prev_fc) = rec.previous_experimental_full_codemode {
                    exp_map.insert("full_codemode".to_owned(), prev_fc.clone());
                } else {
                    let _ = remove_key_preserving_order(exp_map, "full_codemode");
                }
            }
            if exp_map.is_empty() && rec.experimental_was_absent {
                remove_experimental_key = true;
            }
        }
        if remove_experimental_key {
            let _ = remove_key_preserving_order(map, "experimental");
        }
    }

    if rec.wrote_schema
        && map.len() == 1
        && map.get("$schema").and_then(Value::as_str) == Some(OPENCODE_SCHEMA_URL)
    {
        let _ = remove_key_preserving_order(map, "$schema");
    }

    Value::Object(map.clone()) != snapshot_before
}

pub fn resolve_opencode_config_path(
    global: bool,
    project_dir: Option<&Path>,
) -> Result<PathBuf, HarnessError> {
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
) -> Result<String, HarnessError> {
    write_or_update_opencode_config_with_ledger(None, target_path, params, print_only)
}

pub fn write_or_update_opencode_config_with_ledger(
    ledger_path: Option<&Path>,
    target_path: &Path,
    params: &HarnessSyncParams,
    print_only: bool,
) -> Result<String, HarnessError> {
    let file_existed_now = target_path.is_file();
    let mut map = read_existing_opencode_json_object(target_path)?;

    let mut ledger = match ledger_path {
        Some(lp) => Some((lp, read_config_sync_ledger(lp)?)),
        None => None,
    };
    let ledger_key = ledger_key_for_path(target_path);

    let existed_before_sync = if let Some((_, ref lg)) = ledger
        && let Some(prior_rec) = lg.opencode.get(&ledger_key)
        && file_existed_now
    {
        let _ = revert_opencode_managed_record(&mut map, prior_rec);
        prior_rec.existed_before_sync
    } else {
        file_existed_now
    };

    let baseline_before_merge = map.clone();
    merged_opencode_config_into(&mut map, params, false);
    let mut rendered =
        serde_json::to_string_pretty(&Value::Object(map.clone())).map_err(|source| {
            HarnessError::Json {
                path: target_path.to_path_buf(),
                source,
            }
        })?;
    rendered.push('\n');
    if !print_only {
        backup_once_if_exists(target_path)?;
        write_file_atomic(target_path, rendered.as_bytes(), true)?;
        if let Some((sp, ref mut lg)) = ledger {
            let rec = capture_opencode_managed_record(
                &baseline_before_merge,
                &map,
                existed_before_sync,
                params,
            );
            lg.opencode.insert(ledger_key, rec);
            write_config_sync_ledger(sp, lg)?;
        }
    }
    Ok(rendered)
}

pub fn uninstall_opencode_config(target_path: &Path) -> Result<bool, HarnessError> {
    uninstall_opencode_config_with_ledger(None, target_path)
}

pub fn uninstall_opencode_config_with_ledger(
    ledger_path: Option<&Path>,
    target_path: &Path,
) -> Result<bool, HarnessError> {
    let ledger_key = ledger_key_for_path(target_path);
    if !target_path.exists() {
        if let Some(lp) = ledger_path {
            let mut lg = read_config_sync_ledger(lp)?;
            if lg.opencode.remove(&ledger_key).is_some() {
                write_config_sync_ledger(lp, &lg)?;
            }
        }
        return Ok(false);
    }

    if let Some(lp) = ledger_path {
        let mut lg = read_config_sync_ledger(lp)?;
        if let Some(rec) = lg.opencode.remove(&ledger_key) {
            let mut map = read_existing_opencode_json_object(target_path)?;
            let changed = revert_opencode_managed_record(&mut map, &rec);
            write_config_sync_ledger(lp, &lg)?;
            if !changed {
                return Ok(false);
            }
            if map.is_empty() && !rec.existed_before_sync {
                fs::remove_file(target_path).map_err(|source| HarnessError::Io {
                    path: target_path.to_path_buf(),
                    source,
                })?;
                if let Some(parent) = target_path.parent() {
                    let _ = fs::remove_dir(parent);
                }
            } else {
                let mut rendered =
                    serde_json::to_string_pretty(&Value::Object(map)).map_err(|source| {
                        HarnessError::Json {
                            path: target_path.to_path_buf(),
                            source,
                        }
                    })?;
                rendered.push('\n');
                write_file_atomic(target_path, rendered.as_bytes(), true)?;
            }
            return Ok(true);
        }
    }

    // No installation record exists: never touch or reformat files not provably written by Liftgate.
    let Ok(mut map) = read_existing_opencode_json_object(target_path) else {
        return Ok(false);
    };
    if !strip_opencode_config_conservative(&mut map) {
        return Ok(false);
    }
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

/// Parses OpenCode's `OPENCODE_CONFIG_CONTENT` environment variable as JSONC (allowing `//` line
/// comments, `/* ... */` block comments, and trailing commas), failing closed on syntax errors or
/// non-object roots instead of silently discarding the caller's existing configuration.
pub fn parse_opencode_env_config_content(
    raw: Option<&str>,
) -> Result<Map<String, Value>, HarnessError> {
    let Some(raw) = raw else {
        return Ok(Map::new());
    };
    let cleaned = strip_jsonc_to_json(raw);
    if cleaned.trim().is_empty() {
        return Ok(Map::new());
    }
    let val: Value =
        serde_json::from_str(&cleaned).map_err(|source| HarnessError::InvalidEnvConfigJson {
            var: "OPENCODE_CONFIG_CONTENT",
            source,
        })?;
    match val {
        Value::Object(map) => Ok(map),
        _ => Err(HarnessError::EnvConfigNotObject {
            var: "OPENCODE_CONFIG_CONTENT",
        }),
    }
}

pub fn read_existing_opencode_json_object(path: &Path) -> Result<Map<String, Value>, HarnessError> {
    let raw = match fs::read(path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(source) => {
            return Err(HarnessError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let text = String::from_utf8(raw).map_err(|source| HarnessError::Utf8 {
        path: path.to_path_buf(),
        source,
    })?;
    let cleaned = strip_jsonc_to_json(&text);
    if cleaned.trim().is_empty() {
        return Ok(Map::new());
    }
    let val: Value = serde_json::from_str(&cleaned).map_err(|source| HarnessError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    match val {
        Value::Object(map) => Ok(map),
        _ => Err(HarnessError::ConfigRootNotObject {
            path: path.to_path_buf(),
        }),
    }
}

/// Resolves the `opencode` CLI executable using the shared cross-platform harness resolver.
pub fn resolve_opencode_command(explicit: Option<&Path>) -> Result<ResolvedCommand, HarnessError> {
    let spec = HarnessLookupSpec {
        tool: "opencode",
        explicit_invalid_hint: "explicit `--opencode-bin` path does not exist or is not executable",
        missing_install_hint: "install OpenCode (`npm i -g opencode-ai` or `curl -fsSL https://opencode.ai/install | bash`) or pass `--opencode-bin <path>`",
        unix_bin_name: "opencode",
        unix_home_rel_paths: &[&[".opencode", "bin", "opencode"]],
        windows_exe_name: "opencode.exe",
        windows_extra_exe_candidates: windows_opencode_exe_candidates(),
        windows_npm_script_rel: Some(&["node_modules", "opencode-ai", "bin", "opencode"]),
        windows_cmd_shims: &["opencode.cmd", "opencode.bat"],
        windows_ps1_shims: &["opencode.ps1"],
    };
    resolve_harness_command(explicit, &spec)
}

fn windows_opencode_exe_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(home) = user_home_dir() {
        out.push(home.join(".opencode").join("bin").join("opencode.exe"));
        out.push(home.join(".bun").join("bin").join("opencode.exe"));
        out.push(home.join(".local").join("bin").join("opencode.exe"));
        out.push(home.join("scoop").join("shims").join("opencode.exe"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let local_path = PathBuf::from(local);
        out.push(
            local_path
                .join("Microsoft")
                .join("WinGet")
                .join("Links")
                .join("opencode.exe"),
        );
        out.push(
            local_path
                .join("Programs")
                .join("opencode")
                .join("opencode.exe"),
        );
    }
    out
}
