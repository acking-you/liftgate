//! OpenAI Codex CLI & Codex App `config.toml` surgical editor, catalog builder, and binary resolver.

use std::fs;
use std::path::{Path, PathBuf};

use liftgate_types::openai::{
    build_codex_model_catalog_with_tool_mode, codex_truncation_limit_bytes,
};

use crate::DiscoveredModel;
pub mod invocation;
use crate::error::HarnessError;
use crate::fs_util::{backup_once_if_exists, resolve_base_dir, user_home_dir, write_file_atomic};
use crate::resolve::{HarnessLookupSpec, ResolvedCommand, resolve_harness_command};
pub use invocation::*;

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

const ISOLATED_DESKTOP_DISABLED_FEATURES: &[&str] = &[
    "apps",
    "remote_control",
    "in_app_updates",
    "remote_plugin",
    "recommended_plugins",
];

/// Builds fallback Codex model catalog bytes when the gateway is not queried, with an optional
/// `tool_mode` override (`None` defaults to `"code_mode"`).
pub fn build_offline_codex_catalog_bytes_with_mode(
    discovered: &[DiscoveredModel],
    primary_model: &str,
    tool_mode: Option<&str>,
) -> Result<Vec<u8>, HarnessError> {
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
    let mut bytes = serde_json::to_vec_pretty(&catalog).map_err(|source| HarnessError::Json {
        path: PathBuf::from("codex-models.json"),
        source,
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn resolve_codex_paths(
    global: bool,
    project_dir: Option<&Path>,
) -> Result<(PathBuf, PathBuf), HarnessError> {
    if global {
        let dir = user_home_dir()?.join(".codex");
        Ok((dir.join("config.toml"), dir.join("agy-models.json")))
    } else {
        let dir = resolve_base_dir(project_dir)?.join(".codex");
        Ok((dir.join("config.toml"), dir.join("agy-models.json")))
    }
}

/// Non-destructively merges Liftgate's Codex provider (`[model_providers.agy]`), `[features]`
/// Code Mode toggle, `[tools.update_plan]`, and root settings into an existing `config.toml` string, keeping top-level
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

    let merged_tables = rewrite_codex_tables(
        tables_part,
        CodexTableRewriteOpts {
            enable_code_mode: !no_code_mode,
            enable_code_mode_only: code_mode_only,
            isolate_desktop_features: false,
            manage_update_plan: true,
        },
    );
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

    let merged_tables = rewrite_codex_tables(
        tables_part,
        CodexTableRewriteOpts {
            enable_code_mode: !no_code_mode,
            enable_code_mode_only: code_mode_only,
            isolate_desktop_features: true,
            manage_update_plan: true,
        },
    );
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

/// Strips Liftgate's `[model_providers.agy]` section, managed `[features]` `code_mode` key,
/// managed `[tools.update_plan]` default, and managed root keys from `config.toml`.
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

    let stripped_tables = rewrite_codex_tables(
        tables_part,
        CodexTableRewriteOpts {
            enable_code_mode: false,
            enable_code_mode_only: false,
            isolate_desktop_features: false,
            manage_update_plan: false,
        },
    );
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

pub fn write_or_update_codex_config(req: &CodexSyncRequest<'_>) -> Result<String, HarnessError> {
    let existing = match fs::read_to_string(req.config_path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(HarnessError::Io {
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

pub fn uninstall_codex_config(
    config_path: &Path,
    catalog_path: &Path,
) -> Result<bool, HarnessError> {
    let mut touched = false;
    if catalog_path.exists() {
        fs::remove_file(catalog_path).map_err(|source| HarnessError::Io {
            path: catalog_path.to_path_buf(),
            source,
        })?;
        touched = true;
    }
    if config_path.exists() {
        let existing = fs::read_to_string(config_path).map_err(|source| HarnessError::Io {
            path: config_path.to_path_buf(),
            source,
        })?;
        let stripped = strip_codex_config_toml(&existing);
        if stripped.trim().is_empty() {
            fs::remove_file(config_path).map_err(|source| HarnessError::Io {
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

pub fn parse_toml_assignment_key(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
        return None;
    }
    let (lhs, _) = trimmed.split_once('=')?;
    let key = lhs.trim().trim_matches('"').trim_matches('\'');
    (!key.is_empty()).then_some(key)
}

fn parse_toml_bool_assignment(line: &str) -> Option<(&str, bool)> {
    let key = parse_toml_assignment_key(line)?;
    let (_, rhs) = line.split_once('=')?;
    let val = rhs.split('#').next().unwrap_or(rhs).trim();
    match val {
        "true" => Some((key, true)),
        "false" => Some((key, false)),
        _ => None,
    }
}

fn is_tools_update_plan_section(sec_name: &str) -> bool {
    matches!(
        sec_name,
        "tools.update_plan" | "tools.\"update_plan\"" | "tools.'update_plan'"
    )
}

#[derive(Clone, Copy)]
struct CodexTableRewriteOpts {
    enable_code_mode: bool,
    enable_code_mode_only: bool,
    isolate_desktop_features: bool,
    manage_update_plan: bool,
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

fn rewrite_codex_tables(tables_part: &str, opts: CodexTableRewriteOpts) -> String {
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
    let mut saw_update_plan = false;

    for (sec_name, header, body) in sections {
        if sec_name == "model_providers.agy" || sec_name.starts_with("model_providers.agy.") {
            continue;
        }
        // `~/.codex/config.toml` records `[marketplaces.openai-bundled]` pointing into
        // `~/.codex/.tmp/bundled-marketplaces/openai-bundled`; copying that path into an
        // ephemeral `CODEX_HOME` makes Codex App reject its own ephemeral bundled marketplace
        // (`marketplace 'openai-bundled' is already added from a different source`).
        if opts.isolate_desktop_features && is_ephemeral_bundled_marketplace_table(&sec_name) {
            continue;
        }
        if sec_name == "features" {
            saw_features = true;
            let mut kept: Vec<&str> = body
                .into_iter()
                .filter(|l| {
                    let key = parse_toml_assignment_key(l);
                    !matches!(key, Some("code_mode" | "code_mode_only"))
                        && !(opts.isolate_desktop_features
                            && key.is_some_and(|k| ISOLATED_DESKTOP_DISABLED_FEATURES.contains(&k)))
                })
                .collect();
            while kept.last().is_some_and(|l| l.trim().is_empty()) {
                kept.pop();
            }
            let mut managed_feature_lines: Vec<String> = Vec::new();
            if opts.enable_code_mode {
                managed_feature_lines.push("code_mode = true".to_owned());
                if opts.enable_code_mode_only {
                    managed_feature_lines.push("code_mode_only = true".to_owned());
                }
            } else if opts.isolate_desktop_features {
                managed_feature_lines.push("code_mode = false".to_owned());
                managed_feature_lines.push("code_mode_only = false".to_owned());
            }
            if opts.isolate_desktop_features {
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
        if is_tools_update_plan_section(&sec_name) {
            saw_update_plan = true;
            let explicitly_disabled = body
                .iter()
                .any(|l| parse_toml_bool_assignment(l) == Some(("enabled", false)));
            if opts.manage_update_plan {
                if explicitly_disabled {
                    let mut block = String::new();
                    block.push_str(&header);
                    block.push('\n');
                    for l in body {
                        block.push_str(l);
                    }
                    rendered_sections.push(block.trim_end().to_owned());
                } else {
                    let mut kept: Vec<&str> = body
                        .into_iter()
                        .filter(|l| parse_toml_assignment_key(l) != Some("enabled"))
                        .collect();
                    while kept.last().is_some_and(|l| l.trim().is_empty()) {
                        kept.pop();
                    }
                    let mut block = String::new();
                    block.push_str(&header);
                    block.push_str("\nenabled = true");
                    if !kept.is_empty() {
                        block.push('\n');
                        for l in kept {
                            block.push_str(l);
                        }
                    }
                    rendered_sections.push(block.trim_end().to_owned());
                }
            } else {
                let mut kept: Vec<&str> = body
                    .into_iter()
                    .filter(|l| parse_toml_bool_assignment(l) != Some(("enabled", true)))
                    .collect();
                while kept.last().is_some_and(|l| l.trim().is_empty()) {
                    kept.pop();
                }
                if kept.iter().any(|l| !l.trim().is_empty()) {
                    let mut block = String::new();
                    block.push_str(&header);
                    block.push('\n');
                    for l in kept {
                        block.push_str(l);
                    }
                    rendered_sections.push(block.trim_end().to_owned());
                }
            }
            continue;
        }
        if sec_name == "tools"
            && body.iter().any(|l| {
                parse_toml_assignment_key(l)
                    .is_some_and(|k| k == "update_plan" || k.starts_with("update_plan."))
            })
        {
            saw_update_plan = true;
        }

        let mut block = String::new();
        block.push_str(&header);
        block.push('\n');
        for l in body {
            block.push_str(l);
        }
        rendered_sections.push(block.trim_end().to_owned());
    }

    if !saw_features && (opts.enable_code_mode || opts.isolate_desktop_features) {
        let mut block = String::from("[features]");
        if opts.enable_code_mode {
            block.push_str("\ncode_mode = true");
            if opts.enable_code_mode_only {
                block.push_str("\ncode_mode_only = true");
            }
        } else if opts.isolate_desktop_features {
            block.push_str("\ncode_mode = false\ncode_mode_only = false");
        }
        if opts.isolate_desktop_features {
            for feat in ISOLATED_DESKTOP_DISABLED_FEATURES {
                block.push_str(&format!("\n{feat} = false"));
            }
        }
        rendered_sections.push(block);
    }
    if !saw_update_plan && opts.manage_update_plan {
        rendered_sections.push("[tools.update_plan]\nenabled = true".to_owned());
    }

    rendered_sections.join("\n\n")
}

/// Resolves the `codex` CLI executable using the shared cross-platform harness resolver.
pub fn resolve_codex_command(explicit: Option<&Path>) -> Result<ResolvedCommand, HarnessError> {
    let spec = HarnessLookupSpec {
        tool: "codex",
        explicit_invalid_hint: "explicit `--codex-bin` path does not exist or is not executable",
        missing_install_hint: "install OpenAI Codex (`npm i -g @openai/codex`) or pass `--codex-bin <path>`",
        unix_bin_name: "codex",
        unix_home_rel_paths: &[],
        windows_exe_name: "codex.exe",
        windows_extra_exe_candidates: windows_codex_exe_candidates(),
        windows_npm_script_rel: Some(&["node_modules", "@openai", "codex", "bin", "codex.js"]),
        windows_cmd_shims: &["codex.cmd", "codex.bat"],
        windows_ps1_shims: &["codex.ps1"],
    };
    resolve_harness_command(explicit, &spec)
}

fn windows_codex_exe_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(home) = user_home_dir() {
        out.push(home.join(".bun").join("bin").join("codex.exe"));
        out.push(home.join(".cargo").join("bin").join("codex.exe"));
        out.push(home.join(".local").join("bin").join("codex.exe"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        out.push(
            PathBuf::from(local)
                .join("Microsoft")
                .join("WinGet")
                .join("Links")
                .join("codex.exe"),
        );
    }
    out
}

/// Extracts top-level `model = "..."` from a Codex `config.toml` string when present.
#[must_use]
pub fn inspect_codex_model(codex_text: &str) -> Option<String> {
    codex_text.lines().find_map(|line| {
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
    })
}
