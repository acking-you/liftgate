use std::path::{Path, PathBuf};
use std::process::Stdio;

use liftgate_server::platform::{
    canonicalize_clean, is_executable_file, merge_loopback_no_proxy, split_expanded_path_var,
};
use serde_json::Value;
use tokio::process::Command;
use tokio::task::JoinSet;

use crate::cli::{ClaudeArgs, CodexArgs, PLACEHOLDER_API_KEY, normalize_optional_api_key};
use crate::config_sync::{
    HarnessSyncParams, build_claude_settings_map, build_offline_codex_catalog_bytes,
    resolve_fast_model,
};
use crate::daemon::ensure_gateway_ready;
use crate::error::CliError;
use crate::state::{EphemeralFileGuard, StatePaths, user_home_dir};

#[derive(Clone, Debug)]
pub struct ResolvedCommand {
    pub program: PathBuf,
    pub prefix_args: Vec<PathBuf>,
}

pub async fn run_claude(state_paths: &StatePaths, args: ClaudeArgs) -> Result<i32, CliError> {
    let resolved_cmd = resolve_claude_command(args.claude_bin.as_deref())?;
    let target = ensure_gateway_ready(
        state_paths,
        args.listen,
        args.api_key.clone(),
        args.model.as_deref(),
        true,
    )
    .await?;

    let primary_model = args
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&target.default_model)
        .to_owned();
    let auth_token = normalize_optional_api_key(target.api_key.as_deref())
        .unwrap_or_else(|| PLACEHOLDER_API_KEY.to_owned());

    let discovered = target.client.fetch_models().await.unwrap_or_default();

    let fast_model = resolve_fast_model(args.fast_model.as_deref(), &primary_model, &discovered);

    let sync_params = HarnessSyncParams {
        gateway_base_url: target.client.base_url().to_owned(),
        api_key: auth_token.clone(),
        primary_model: primary_model.clone(),
        fast_model: fast_model.clone(),
        no_bypass: args.no_bypass,
        long_context: false,
        discovered_models: discovered,
    };
    let settings_map = build_claude_settings_map(&sync_params);
    let settings_bytes =
        serde_json::to_vec_pretty(&Value::Object(settings_map)).map_err(|source| {
            CliError::StateJson {
                path: PathBuf::from("ephemeral-claude-settings.json"),
                source,
            }
        })?;

    let ephemeral_settings =
        EphemeralFileGuard::create(state_paths, "claude-settings", "json", &settings_bytes)?;

    let has_explicit_model = args
        .passthrough_args
        .iter()
        .any(|a| a == "--model" || a == "-m" || a.starts_with("--model=") || a.starts_with("-m="));
    let has_explicit_perm = args.passthrough_args.iter().any(|a| {
        matches!(
            a.as_str(),
            "--dangerously-skip-permissions"
                | "--allow-dangerously-skip-permissions"
                | "--permission-mode"
                | "--safe-mode"
                | "--restricted"
        ) || a.starts_with("--permission-mode=")
    });

    let mut cmd = build_base_child_command(&resolved_cmd);
    if !args.no_bypass && !has_explicit_perm {
        cmd.arg("--dangerously-skip-permissions");
    }
    cmd.arg("--settings").arg(ephemeral_settings.path());
    if !has_explicit_model {
        cmd.arg("--model").arg(&primary_model);
    }
    cmd.args(&args.passthrough_args);

    let no_proxy = merge_loopback_no_proxy(
        std::env::var("NO_PROXY")
            .ok()
            .or_else(|| std::env::var("no_proxy").ok())
            .as_deref(),
    );
    cmd.env_remove("ANTHROPIC_API_KEY")
        .env_remove("AGY_API_KEY")
        .env("ANTHROPIC_BASE_URL", target.client.base_url())
        .env("ANTHROPIC_AUTH_TOKEN", &auth_token)
        .env("ANTHROPIC_MODEL", &primary_model)
        .env("ANTHROPIC_DEFAULT_OPUS_MODEL", &primary_model)
        .env("ANTHROPIC_DEFAULT_SONNET_MODEL", &primary_model)
        .env("ANTHROPIC_DEFAULT_HAIKU_MODEL", &fast_model)
        .env("ANTHROPIC_SMALL_FAST_MODEL", &fast_model)
        .env_remove("CLAUDE_CODE_MAX_CONTEXT_TOKENS")
        .env_remove("CLAUDE_CODE_AUTO_COMPACT_WINDOW")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("CLAUDE_CODE_AUTO_MODE_SERVER", "0")
        .env("NO_PROXY", &no_proxy)
        .env("no_proxy", &no_proxy);

    let exit_code = spawn_and_wait_interactive("claude", &resolved_cmd.program, cmd).await?;
    drop(ephemeral_settings);
    Ok(exit_code)
}

pub async fn run_codex(state_paths: &StatePaths, args: CodexArgs) -> Result<i32, CliError> {
    let resolved_cmd = resolve_codex_command(args.codex_bin.as_deref())?;
    let target = ensure_gateway_ready(
        state_paths,
        args.listen,
        args.api_key.clone(),
        args.model.as_deref(),
        true,
    )
    .await?;

    let primary_model = args
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&target.default_model)
        .to_owned();
    let normalized_key = normalize_optional_api_key(target.api_key.as_deref());
    let require_env_key = normalized_key.is_some();
    let auth_token = normalized_key.unwrap_or_else(|| PLACEHOLDER_API_KEY.to_owned());

    let catalog_bytes = match target.client.fetch_codex_catalog_json(&primary_model).await {
        Ok(bytes) => bytes,
        Err(_) => {
            let discovered = target.client.fetch_models().await.unwrap_or_default();
            build_offline_codex_catalog_bytes(&discovered, &primary_model, false)?
        }
    };

    let ephemeral_catalog =
        EphemeralFileGuard::create(state_paths, "codex-catalog", "json", &catalog_bytes)?;
    let catalog_slash = ephemeral_catalog
        .path()
        .to_string_lossy()
        .replace('\\', "/");

    let has_explicit_model = args
        .passthrough_args
        .iter()
        .any(|a| a == "-m" || a == "--model" || a.starts_with("-m=") || a.starts_with("--model="));
    let has_explicit_approval = args.passthrough_args.iter().any(|a| {
        matches!(
            a.as_str(),
            "--dangerously-bypass-approvals-and-sandbox"
                | "--yolo"
                | "-s"
                | "--sandbox"
                | "-a"
                | "--ask-for-approval"
                | "--approve-for-me"
                | "--full-auto"
        ) || a.starts_with("-s=")
            || a.starts_with("--sandbox=")
            || a.starts_with("-a=")
            || a.starts_with("--ask-for-approval=")
    });

    let mut cmd = build_base_child_command(&resolved_cmd);
    if !args.no_bypass && !has_explicit_approval {
        cmd.arg("--dangerously-bypass-approvals-and-sandbox");
    }
    let toml_primary = toml_inline_string(&primary_model);
    let toml_catalog = toml_inline_string(&catalog_slash);
    let toml_base_v1 = toml_inline_string(&format!("{}/v1", target.client.base_url()));
    cmd.arg("-c").arg("model_provider='agy'");
    if !has_explicit_model {
        cmd.arg("-c").arg(format!("model={toml_primary}"));
    }
    cmd.arg("-c")
        .arg(format!("model_catalog_json={toml_catalog}"))
        .arg("-c")
        .arg("web_search='live'")
        .arg("-c")
        .arg("model_providers.agy.name='Liftgate'")
        .arg("-c")
        .arg(format!("model_providers.agy.base_url={toml_base_v1}"));
    if require_env_key {
        cmd.arg("-c")
            .arg("model_providers.agy.env_key='AGY_GATEWAY_API_KEY'");
    }
    cmd.arg("-c")
        .arg("model_providers.agy.wire_api='responses'");
    cmd.args(&args.passthrough_args);

    let no_proxy = merge_loopback_no_proxy(
        std::env::var("NO_PROXY")
            .ok()
            .or_else(|| std::env::var("no_proxy").ok())
            .as_deref(),
    );
    cmd.env_remove("AGY_API_KEY")
        .env("AGY_GATEWAY_API_KEY", &auth_token)
        .env("NO_PROXY", &no_proxy)
        .env("no_proxy", &no_proxy);

    let exit_code = spawn_and_wait_interactive("codex", &resolved_cmd.program, cmd).await?;
    drop(ephemeral_catalog);
    Ok(exit_code)
}

fn toml_inline_string(val: &str) -> String {
    if !val.contains('\'') && !val.chars().any(char::is_control) {
        format!("'{val}'")
    } else {
        serde_json::to_string(val).unwrap_or_else(|_| format!("{val:?}"))
    }
}

fn build_base_child_command(resolved: &ResolvedCommand) -> Command {
    let mut cmd = Command::new(&resolved.program);
    for prefix in &resolved.prefix_args {
        cmd.arg(prefix);
    }
    cmd
}

async fn spawn_and_wait_interactive(
    tool: &'static str,
    program_path: &Path,
    mut cmd: Command,
) -> Result<i32, CliError> {
    if let Ok(cwd) = std::env::current_dir() {
        cmd.current_dir(cwd);
    }
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let mut child = cmd.spawn().map_err(|source| CliError::HarnessSpawn {
        tool,
        path: program_path.to_path_buf(),
        source,
    })?;

    let mut sig_tasks = JoinSet::new();
    sig_tasks.spawn(async { while tokio::signal::ctrl_c().await.is_ok() {} });

    let status = child
        .wait()
        .await
        .map_err(|source| CliError::HarnessSpawn {
            tool,
            path: program_path.to_path_buf(),
            source,
        })?;

    Ok(status.code().unwrap_or(1))
}

pub fn resolve_claude_command(explicit: Option<&Path>) -> Result<ResolvedCommand, CliError> {
    if let Some(p) = explicit {
        if is_executable_file(p) {
            return Ok(ResolvedCommand {
                program: canonicalize_clean(p),
                prefix_args: Vec::new(),
            });
        }
        return Err(CliError::HarnessNotFound {
            tool: "claude",
            hint: "explicit `--claude-bin` path does not exist or is not executable",
        });
    }

    let path_dirs = split_expanded_path_var();

    #[cfg(windows)]
    {
        if let Some(exe) = find_in_dirs(&path_dirs, &["claude.exe"]) {
            return Ok(ResolvedCommand {
                program: exe,
                prefix_args: Vec::new(),
            });
        }
        for cand in windows_claude_exe_candidates() {
            if is_executable_file(&cand) {
                return Ok(ResolvedCommand {
                    program: canonicalize_clean(&cand),
                    prefix_args: Vec::new(),
                });
            }
        }
        if let Some(node_cmd) = resolve_windows_npm_script(
            &path_dirs,
            &["node_modules", "@anthropic-ai", "claude-code", "cli.js"],
        ) {
            return Ok(node_cmd);
        }
        if let Some(shim) = find_in_dirs(&path_dirs, &["claude.cmd", "claude.bat"]) {
            return Ok(ResolvedCommand {
                program: shim,
                prefix_args: Vec::new(),
            });
        }
    }

    #[cfg(not(windows))]
    {
        if let Some(exe) = find_in_dirs(&path_dirs, &["claude"]) {
            return Ok(ResolvedCommand {
                program: exe,
                prefix_args: Vec::new(),
            });
        }
        for cand in unix_extra_bin_candidates("claude") {
            if is_executable_file(&cand) {
                return Ok(ResolvedCommand {
                    program: canonicalize_clean(&cand),
                    prefix_args: Vec::new(),
                });
            }
        }
    }

    Err(CliError::HarnessNotFound {
        tool: "claude",
        hint: "install Claude Code (`npm i -g @anthropic-ai/claude-code`) or pass `--claude-bin <path>`",
    })
}

pub fn resolve_codex_command(explicit: Option<&Path>) -> Result<ResolvedCommand, CliError> {
    if let Some(p) = explicit {
        if is_executable_file(p) {
            return Ok(ResolvedCommand {
                program: canonicalize_clean(p),
                prefix_args: Vec::new(),
            });
        }
        return Err(CliError::HarnessNotFound {
            tool: "codex",
            hint: "explicit `--codex-bin` path does not exist or is not executable",
        });
    }

    let path_dirs = split_expanded_path_var();

    #[cfg(windows)]
    {
        if let Some(exe) = find_in_dirs(&path_dirs, &["codex.exe"]) {
            return Ok(ResolvedCommand {
                program: exe,
                prefix_args: Vec::new(),
            });
        }
        for cand in windows_codex_exe_candidates() {
            if is_executable_file(&cand) {
                return Ok(ResolvedCommand {
                    program: canonicalize_clean(&cand),
                    prefix_args: Vec::new(),
                });
            }
        }
        if let Some(node_cmd) = resolve_windows_npm_script(
            &path_dirs,
            &["node_modules", "@openai", "codex", "bin", "codex.js"],
        ) {
            return Ok(node_cmd);
        }
        if let Some(shim) = find_in_dirs(&path_dirs, &["codex.cmd", "codex.bat"]) {
            return Ok(ResolvedCommand {
                program: shim,
                prefix_args: Vec::new(),
            });
        }
    }

    #[cfg(not(windows))]
    {
        if let Some(exe) = find_in_dirs(&path_dirs, &["codex"]) {
            return Ok(ResolvedCommand {
                program: exe,
                prefix_args: Vec::new(),
            });
        }
        for cand in unix_extra_bin_candidates("codex") {
            if is_executable_file(&cand) {
                return Ok(ResolvedCommand {
                    program: canonicalize_clean(&cand),
                    prefix_args: Vec::new(),
                });
            }
        }
    }

    Err(CliError::HarnessNotFound {
        tool: "codex",
        hint: "install OpenAI Codex CLI (`npm i -g @openai/codex`) or pass `--codex-bin <path>`",
    })
}

fn find_in_dirs(dirs: &[PathBuf], names: &[&str]) -> Option<PathBuf> {
    for dir in dirs {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in names {
            let cand = dir.join(name);
            if is_executable_file(&cand) {
                return Some(canonicalize_clean(&cand));
            }
        }
    }
    None
}

#[cfg(not(windows))]
fn unix_extra_bin_candidates(bin_name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(home) = user_home_dir() {
        out.push(home.join(".local").join("bin").join(bin_name));
        out.push(home.join(".bun").join("bin").join(bin_name));
        out.push(home.join(".cargo").join("bin").join(bin_name));
    }
    out.push(PathBuf::from("/opt/homebrew/bin").join(bin_name));
    out.push(PathBuf::from("/usr/local/bin").join(bin_name));
    out
}

#[cfg(windows)]
fn windows_claude_exe_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(home) = user_home_dir() {
        out.push(home.join(".local").join("bin").join("claude.exe"));
        out.push(home.join(".bun").join("bin").join("claude.exe"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        out.push(
            PathBuf::from(local)
                .join("Programs")
                .join("claude")
                .join("claude.exe"),
        );
    }
    out
}

#[cfg(windows)]
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

#[cfg(windows)]
fn resolve_windows_npm_script(
    path_dirs: &[PathBuf],
    rel_segments: &[&str],
) -> Option<ResolvedCommand> {
    let mut candidate_roots: Vec<PathBuf> = path_dirs.to_vec();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        candidate_roots.push(PathBuf::from(appdata).join("npm"));
    }
    if let Some(nvm) = std::env::var_os("NVM_SYMLINK") {
        candidate_roots.push(PathBuf::from(nvm));
    }
    candidate_roots.push(PathBuf::from(r"C:\nvm4w\nodejs"));

    for root in &candidate_roots {
        let mut script_path = root.clone();
        for seg in rel_segments {
            script_path.push(seg);
        }
        if script_path.is_file()
            && let Some(node_exe) = resolve_windows_node_exe(Some(root), path_dirs)
        {
            return Some(ResolvedCommand {
                program: node_exe,
                prefix_args: vec![canonicalize_clean(&script_path)],
            });
        }
    }
    None
}

#[cfg(windows)]
fn resolve_windows_node_exe(
    preferred_dir: Option<&Path>,
    path_dirs: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(dir) = preferred_dir {
        let local = dir.join("node.exe");
        if is_executable_file(&local) {
            return Some(canonicalize_clean(&local));
        }
    }
    if let Some(found) = find_in_dirs(path_dirs, &["node.exe"]) {
        return Some(found);
    }
    let mut fallbacks = Vec::new();
    if let Some(nvm) = std::env::var_os("NVM_SYMLINK") {
        fallbacks.push(PathBuf::from(nvm).join("node.exe"));
    }
    fallbacks.push(PathBuf::from(r"C:\nvm4w\nodejs\node.exe"));
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        fallbacks.push(PathBuf::from(pf).join("nodejs").join("node.exe"));
    }
    if let Some(volta) = std::env::var_os("VOLTA_HOME") {
        fallbacks.push(PathBuf::from(volta).join("bin").join("node.exe"));
    }
    for cand in fallbacks {
        if is_executable_file(&cand) {
            return Some(canonicalize_clean(&cand));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_inline_string_uses_single_quotes_for_plain_values() {
        assert_eq!(
            toml_inline_string("gemini-4-carbon-high"),
            "'gemini-4-carbon-high'"
        );
        assert_eq!(
            toml_inline_string("C:/Users/alice/.liftgate/ephemeral/cat.json"),
            "'C:/Users/alice/.liftgate/ephemeral/cat.json'"
        );
    }

    #[test]
    fn toml_inline_string_escapes_embedded_single_quotes_as_json_basic_string() {
        assert_eq!(
            toml_inline_string("C:/Users/O'Connor/cat.json"),
            "\"C:/Users/O'Connor/cat.json\""
        );
    }

    #[test]
    fn resolve_explicit_nonexistent_binary_returns_not_found() {
        let missing = PathBuf::from("__nonexistent_liftgate_test_binary__.exe");
        assert!(matches!(
            resolve_claude_command(Some(&missing)),
            Err(CliError::HarnessNotFound { tool: "claude", .. })
        ));
        assert!(matches!(
            resolve_codex_command(Some(&missing)),
            Err(CliError::HarnessNotFound { tool: "codex", .. })
        ));
    }
}
