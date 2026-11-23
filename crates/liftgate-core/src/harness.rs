use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use liftgate_server::platform::{
    canonicalize_clean, is_executable_file, merge_loopback_no_proxy, split_expanded_path_var,
};
use liftgate_types::config::DEFAULT_TURN_TIMEOUT_SECS;
use serde_json::Value;
use tokio::process::Command;
use tokio::task::JoinSet;

use crate::cli::{
    ClaudeArgs, ClaudeDesktopArgs, CodexAppArgs, CodexArgs, OmpArgs, OpencodeArgs,
    PLACEHOLDER_API_KEY, normalize_optional_api_key,
};
use crate::config_sync::{
    HarnessSyncParams, OMP_EPHEMERAL_API_KEY_ENV, OmpCodeModePolicy, build_claude_settings_map,
    build_offline_codex_catalog_bytes_with_mode, build_omp_ephemeral_overlay_yaml,
    build_omp_provider_extension_ts, extract_omp_yaml_extensions, merged_opencode_config_into,
    parse_opencode_env_config_content, resolve_fast_model, resolve_omp_paths,
    strip_agy_provider_prefix,
};
use crate::daemon::ensure_gateway_ready;
use crate::error::CliError;
use crate::state::{EphemeralFileGuard, StatePaths, user_home_dir};
use crate::terminal::TerminalLaunchSpec;

const DEFAULT_REQUEST_TIMEOUT_MS: u64 = DEFAULT_TURN_TIMEOUT_SECS * 1_000;
const CODEX_STREAM_IDLE_TIMEOUT_KEY: &str = "model_providers.agy.stream_idle_timeout_ms";

#[derive(Clone, Debug)]
pub struct ResolvedCommand {
    pub program: PathBuf,
    pub prefix_args: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClaudeInvocationKind {
    /// Local utility, background job control, help, or version invocation that must run with the
    /// native subcommand in `argv[1]` and without starting the gateway.
    DirectPassthrough,
    /// Subcommand requiring `argv[1]` intact (`respawn`, `doctor`, `ultrareview`) while inheriting
    /// Liftgate's `ANTHROPIC_*` environment variables.
    GatewayEnvSubcommand,
    /// Interactive `claude agents` TUI: keeps `agents` in `argv[1]` while passing Liftgate's
    /// ephemeral `--settings`, `--model`, permissions, and `ANTHROPIC_*` env for dispatched sessions.
    AgentsView,
    /// Interactive, `--print`, `--resume`, or `--continue` coding session.
    AgentSession,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexInvocationKind {
    /// Local/session-store/IPC/help/version invocation (`queue`, `archive`, `delete`, `mcp`,
    /// `plugin`, `features`, `completion`, `sandbox`, `--help`, `-V`, etc.) executed directly.
    DirectPassthrough,
    /// Run the native updater with the caller's environment unchanged.
    NativeUpdate,
    /// Gateway-backed subcommand (`review`, `doctor`, `debug`, `app-server`, etc.)
    /// configured solely via `-c` overrides without `--dangerously-bypass-approvals-and-sandbox`.
    GatewayConfigOnly,
    /// Interactive agents overview, with the same config flags as other gateway subcommands.
    AgentsView,
    /// Start, resume, or fork an interactive session.
    InteractiveSession,
    /// Run an agent session non-interactively.
    ExecSession,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenCodeInvocationKind {
    /// Local/credential/plugin/MCP/upgrade/help/version invocation executed directly without
    /// starting the gateway or injecting `OPENCODE_CONFIG_CONTENT`.
    DirectPassthrough,
    /// Interactive TUI (`opencode`, `-c`, `-s <ID>`), `run`, `models`, `session`, or server
    /// subcommands (`web`, `serve`, `acp`, `attach`) routed through Liftgate.
    GatewaySession {
        /// Whether OpenCode v2 (`>= 2.0.0`) accepts `--standalone` on this command so a private
        /// server reads `OPENCODE_CONFIG_CONTENT` instead of attaching to an older background daemon.
        supports_v2_standalone: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OmpInvocationKind {
    /// Local/credential/plugin/stats/update/help/version invocation executed directly without
    /// starting the gateway or injecting `PI_CONFIG_FILES`.
    DirectPassthrough,
    /// Interactive TUI (`omp`, `-c`, `-r`, `--fork`), `-p`, `models`, `commit`, `session`,
    /// `acp`, or `stream` routed through Liftgate via ephemeral `PI_CONFIG_FILES`.
    GatewaySession,
}

pub async fn run_claude(state_paths: &StatePaths, args: ClaudeArgs) -> Result<i32, CliError> {
    if args.desktop {
        let _ = crate::desktop::launch_claude_desktop(
            state_paths,
            ClaudeDesktopArgs {
                listen: args.listen,
                api_key: args.api_key,
                model: args.model,
                restart_app: args.restart_app,
                restore: false,
                wait: args.wait,
                claude_desktop_bin: args.claude_desktop_bin,
                passthrough_args: args.passthrough_args,
            },
        )
        .await?;
        return Ok(0);
    }
    let (spec, ephemeral_guard) = build_claude_invocation(state_paths, &args, None, false).await?;
    let cmd = command_from_terminal_spec(&spec);
    let exit_code = spawn_and_wait_interactive("claude", &spec.program, cmd).await?;
    drop(ephemeral_guard);
    Ok(exit_code)
}

/// Prepares an isolated [`TerminalLaunchSpec`] for launching Claude Code CLI in a native terminal
/// window with zero global config side effects.
pub async fn prepare_claude_terminal_spec(
    state_paths: &StatePaths,
    args: &ClaudeArgs,
    cwd: Option<PathBuf>,
    hold_open: bool,
) -> Result<TerminalLaunchSpec, CliError> {
    let (mut spec, guard) = build_claude_invocation(state_paths, args, cwd, hold_open).await?;
    if let Some(g) = guard {
        spec.cleanup_files.push(g.disarm());
    }
    Ok(spec)
}

async fn build_claude_invocation(
    state_paths: &StatePaths,
    args: &ClaudeArgs,
    cwd: Option<PathBuf>,
    hold_open: bool,
) -> Result<(TerminalLaunchSpec, Option<EphemeralFileGuard>), CliError> {
    let resolved_cmd = resolve_claude_command(args.claude_bin.as_deref())?;
    let invocation = classify_claude_invocation(&args.passthrough_args);

    let no_proxy = merge_loopback_no_proxy(
        std::env::var("NO_PROXY")
            .ok()
            .or_else(|| std::env::var("no_proxy").ok())
            .as_deref(),
    );

    if invocation == ClaudeInvocationKind::DirectPassthrough {
        let mut full_args: Vec<OsString> = resolved_cmd
            .prefix_args
            .iter()
            .map(OsString::from)
            .collect();
        full_args.extend(args.passthrough_args.iter().map(OsString::from));
        return Ok((
            TerminalLaunchSpec {
                title: "Liftgate · Claude Code".to_owned(),
                program: resolved_cmd.program,
                args: full_args,
                cwd,
                env_set: vec![
                    ("NO_PROXY".to_owned(), no_proxy.clone()),
                    ("no_proxy".to_owned(), no_proxy),
                ],
                env_remove: Vec::new(),
                cleanup_files: Vec::new(),
                hold_open_on_exit: hold_open,
            },
            None,
        ));
    }

    let explicit_passthrough_model = extract_explicit_model_arg(&args.passthrough_args);
    let requested_model = args
        .model
        .as_deref()
        .or(explicit_passthrough_model)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let target = ensure_gateway_ready(
        state_paths,
        args.listen,
        args.api_key.clone(),
        requested_model,
        true,
    )
    .await?;

    let primary_model = requested_model.unwrap_or(&target.default_model).to_owned();
    let auth_token = normalize_optional_api_key(target.api_key.as_deref())
        .unwrap_or_else(|| PLACEHOLDER_API_KEY.to_owned());

    let discovered = target.client.fetch_models().await.unwrap_or_default();

    let fast_model = resolve_fast_model(args.fast_model.as_deref(), &primary_model, &discovered);
    let (env_set, env_remove) = build_claude_gateway_env_pairs(
        target.client.base_url(),
        &auth_token,
        &primary_model,
        &fast_model,
        args.no_code_mode,
        &no_proxy,
    );

    if matches!(invocation, ClaudeInvocationKind::GatewayEnvSubcommand) {
        let mut full_args: Vec<OsString> = resolved_cmd
            .prefix_args
            .iter()
            .map(OsString::from)
            .collect();
        full_args.extend(args.passthrough_args.iter().map(OsString::from));
        return Ok((
            TerminalLaunchSpec {
                title: format!("Liftgate · Claude Code ({primary_model})"),
                program: resolved_cmd.program,
                args: full_args,
                cwd,
                env_set,
                env_remove,
                cleanup_files: Vec::new(),
                hold_open_on_exit: hold_open,
            },
            None,
        ));
    }

    let sync_params = HarnessSyncParams {
        gateway_base_url: target.client.base_url().to_owned(),
        api_key: auth_token.clone(),
        primary_model: primary_model.clone(),
        fast_model: fast_model.clone(),
        no_bypass: args.no_bypass,
        no_code_mode: args.no_code_mode,
        discovered_models: discovered,
    };
    let has_explicit_model = explicit_passthrough_model.is_some();
    let has_explicit_effort = args
        .passthrough_args
        .iter()
        .any(|a| a == "--effort" || a.starts_with("--effort="));

    let mut settings_map = build_claude_settings_map(&sync_params);
    if !has_explicit_effort
        && !has_explicit_model
        && let Some(effort_level) = default_claude_effort_level(&primary_model)
    {
        settings_map.insert(
            "effortLevel".to_owned(),
            Value::String(effort_level.to_owned()),
        );
    }
    let settings_bytes =
        serde_json::to_vec_pretty(&Value::Object(settings_map)).map_err(|source| {
            CliError::StateJson {
                path: PathBuf::from("ephemeral-claude-settings.json"),
                source,
            }
        })?;

    let ephemeral_settings =
        EphemeralFileGuard::create(state_paths, "claude-settings", "json", &settings_bytes)?;

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

    let mut full_args: Vec<OsString> = resolved_cmd
        .prefix_args
        .iter()
        .map(OsString::from)
        .collect();
    match invocation {
        ClaudeInvocationKind::AgentsView => {
            full_args.push(OsString::from("agents"));
            if !args.no_bypass && !has_explicit_perm {
                full_args.push(OsString::from("--dangerously-skip-permissions"));
            }
            full_args.push(OsString::from("--settings"));
            full_args.push(ephemeral_settings.path().as_os_str().to_os_string());
            if !has_explicit_model {
                full_args.push(OsString::from("--model"));
                full_args.push(OsString::from(&primary_model));
            }
            if let Some(rest) = args.passthrough_args.get(1..) {
                full_args.extend(rest.iter().map(OsString::from));
            }
        }
        ClaudeInvocationKind::AgentSession => {
            if !args.no_bypass && !has_explicit_perm {
                full_args.push(OsString::from("--dangerously-skip-permissions"));
            }
            full_args.push(OsString::from("--settings"));
            full_args.push(ephemeral_settings.path().as_os_str().to_os_string());
            if !has_explicit_model {
                full_args.push(OsString::from("--model"));
                full_args.push(OsString::from(&primary_model));
            }
            full_args.extend(args.passthrough_args.iter().map(OsString::from));
        }
        ClaudeInvocationKind::DirectPassthrough | ClaudeInvocationKind::GatewayEnvSubcommand => {}
    }

    Ok((
        TerminalLaunchSpec {
            title: format!("Liftgate · Claude Code ({primary_model})"),
            program: resolved_cmd.program,
            args: full_args,
            cwd,
            env_set,
            env_remove,
            cleanup_files: Vec::new(),
            hold_open_on_exit: hold_open,
        },
        Some(ephemeral_settings),
    ))
}

pub async fn run_codex(state_paths: &StatePaths, args: CodexArgs) -> Result<i32, CliError> {
    if args.app {
        let _ = crate::desktop::launch_codex_app(
            state_paths,
            CodexAppArgs {
                listen: args.listen,
                api_key: args.api_key,
                model: args.model,
                no_bypass: args.no_bypass,
                code_mode: args.code_mode,
                code_mode_only: args.code_mode_only,
                no_code_mode: args.no_code_mode,
                restart_app: args.restart_app,
                ephemeral: args.ephemeral,
                wait: args.wait,
                codex_app_bin: args.codex_app_bin,
                codex_bin: None,
                passthrough_args: args.passthrough_args,
            },
        )
        .await?;
        return Ok(0);
    }
    let (spec, ephemeral_guard) = build_codex_invocation(state_paths, &args, None, false).await?;
    let cmd = command_from_terminal_spec(&spec);
    let exit_code = spawn_and_wait_interactive("codex", &spec.program, cmd).await?;
    drop(ephemeral_guard);
    Ok(exit_code)
}

pub async fn run_opencode(state_paths: &StatePaths, args: OpencodeArgs) -> Result<i32, CliError> {
    let spec = build_opencode_invocation(state_paths, &args, None, false).await?;
    let cmd = command_from_terminal_spec(&spec);
    spawn_and_wait_interactive("opencode", &spec.program, cmd).await
}

/// Prepares an isolated [`TerminalLaunchSpec`] for launching OpenCode in a native terminal
/// window with zero project/global config file side effects.
pub async fn prepare_opencode_terminal_spec(
    state_paths: &StatePaths,
    args: &OpencodeArgs,
    cwd: Option<PathBuf>,
    hold_open: bool,
) -> Result<TerminalLaunchSpec, CliError> {
    build_opencode_invocation(state_paths, args, cwd, hold_open).await
}

async fn build_opencode_invocation(
    state_paths: &StatePaths,
    args: &OpencodeArgs,
    cwd: Option<PathBuf>,
    hold_open: bool,
) -> Result<TerminalLaunchSpec, CliError> {
    let resolved_cmd = resolve_opencode_command(args.opencode_bin.as_deref())?;
    let invocation = classify_opencode_invocation(&args.passthrough_args);

    if let OpenCodeInvocationKind::DirectPassthrough = invocation {
        let mut full_args: Vec<OsString> = resolved_cmd
            .prefix_args
            .iter()
            .map(OsString::from)
            .collect();
        full_args.extend(args.passthrough_args.iter().map(OsString::from));
        return Ok(TerminalLaunchSpec {
            title: "Liftgate · OpenCode".to_owned(),
            program: resolved_cmd.program,
            args: full_args,
            cwd,
            env_set: Vec::new(),
            env_remove: Vec::new(),
            cleanup_files: Vec::new(),
            hold_open_on_exit: hold_open,
        });
    }

    let OpenCodeInvocationKind::GatewaySession {
        supports_v2_standalone,
    } = invocation
    else {
        unreachable!("DirectPassthrough handled above");
    };

    let requested_model = args
        .model
        .as_deref()
        .map(strip_agy_provider_prefix)
        .filter(|s| !s.is_empty());

    let (target, v2_runtime) = tokio::join!(
        ensure_gateway_ready(
            state_paths,
            args.listen,
            args.api_key.clone(),
            requested_model,
            true,
        ),
        detect_opencode_v2(&resolved_cmd),
    );
    let target = target?;

    let primary_model = requested_model
        .map(str::to_owned)
        .unwrap_or_else(|| target.default_model.clone());
    let discovered_models = target.client.fetch_models().await.unwrap_or_default();
    let fast_model = resolve_fast_model(
        args.fast_model.as_deref().map(strip_agy_provider_prefix),
        &primary_model,
        &discovered_models,
    );
    let auth_token = normalize_optional_api_key(target.api_key.as_deref())
        .unwrap_or_else(|| PLACEHOLDER_API_KEY.to_owned());

    let sync_params = HarnessSyncParams {
        gateway_base_url: target.client.base_url().to_owned(),
        api_key: auth_token,
        primary_model: primary_model.clone(),
        fast_model,
        no_bypass: args.no_bypass,
        no_code_mode: args.no_code_mode,
        discovered_models,
    };

    let raw_env_config = std::env::var("OPENCODE_CONFIG_CONTENT").ok();
    let mut cfg_map = parse_opencode_env_config_content(raw_env_config.as_deref())?;
    merged_opencode_config_into(&mut cfg_map, &sync_params, v2_runtime);
    let config_content =
        serde_json::to_string(&Value::Object(cfg_map)).map_err(|source| CliError::StateJson {
            path: PathBuf::from("OPENCODE_CONFIG_CONTENT"),
            source,
        })?;

    let no_proxy = merge_loopback_no_proxy(
        std::env::var("NO_PROXY")
            .ok()
            .or_else(|| std::env::var("no_proxy").ok())
            .as_deref(),
    );
    let mut env_set = vec![
        ("OPENCODE_CONFIG_CONTENT".to_owned(), config_content),
        ("NO_PROXY".to_owned(), no_proxy.clone()),
        ("no_proxy".to_owned(), no_proxy),
    ];
    if args.full_code_mode {
        let flags = merge_opencode_flag(
            std::env::var("OPENCODE_FLAGS").ok().as_deref(),
            "full_codemode",
        );
        env_set.push(("OPENCODE_FLAGS".to_owned(), flags));
    } else if args.no_code_mode
        && let Ok(existing_flags) = std::env::var("OPENCODE_FLAGS")
        && existing_flags
            .split(',')
            .any(|item| item.trim() == "full_codemode")
    {
        let flags = strip_opencode_flag(Some(existing_flags.as_str()), "full_codemode");
        env_set.push(("OPENCODE_FLAGS".to_owned(), flags));
    }

    let has_server_flag = args
        .passthrough_args
        .iter()
        .any(|a| matches!(a.as_str(), "--standalone" | "--server") || a.starts_with("--server="));
    let mut full_args: Vec<OsString> = resolved_cmd
        .prefix_args
        .iter()
        .map(OsString::from)
        .collect();
    if v2_runtime && supports_v2_standalone && !has_server_flag {
        full_args.push(OsString::from("--standalone"));
    }
    full_args.extend(args.passthrough_args.iter().map(OsString::from));

    Ok(TerminalLaunchSpec {
        title: format!("Liftgate · OpenCode ({primary_model})"),
        program: resolved_cmd.program,
        args: full_args,
        cwd,
        env_set,
        env_remove: Vec::new(),
        cleanup_files: Vec::new(),
        hold_open_on_exit: hold_open,
    })
}

fn classify_opencode_invocation(passthrough_args: &[String]) -> OpenCodeInvocationKind {
    if passthrough_args
        .iter()
        .any(|a| matches!(a.as_str(), "-h" | "--help" | "-V" | "-v" | "--version"))
    {
        return OpenCodeInvocationKind::DirectPassthrough;
    }
    let first_non_flag = passthrough_args.first().filter(|a| !a.starts_with('-'));
    match first_non_flag.map(String::as_str) {
        Some(
            "upgrade" | "update" | "uninstall" | "auth" | "login" | "logout" | "mcp" | "plugin"
            | "completion" | "stats" | "export" | "import" | "debug" | "service",
        ) => OpenCodeInvocationKind::DirectPassthrough,
        Some("web" | "serve" | "acp" | "attach") => OpenCodeInvocationKind::GatewaySession {
            supports_v2_standalone: false,
        },
        Some("run" | "models" | "session") | Some(_) | None => {
            OpenCodeInvocationKind::GatewaySession {
                supports_v2_standalone: true,
            }
        }
    }
}

fn merge_opencode_flag(existing: Option<&str>, flag: &str) -> String {
    let mut items: Vec<&str> = existing
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != format!("-{flag}"))
        .collect();
    if !items.contains(&flag) {
        items.push(flag);
    }
    items.join(",")
}

fn strip_opencode_flag(existing: Option<&str>, flag: &str) -> String {
    let neg = format!("-{flag}");
    let mut items: Vec<&str> = existing
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != flag)
        .collect();
    if !items.iter().any(|s| *s == neg) {
        items.push(&neg);
    }
    items.join(",")
}

async fn detect_opencode_v2(resolved: &ResolvedCommand) -> bool {
    let mut cmd = Command::new(&resolved.program);
    cmd.args(&resolved.prefix_args)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let Ok(Ok(out)) = tokio::time::timeout(Duration::from_millis(600), cmd.output()).await else {
        return false;
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    parse_opencode_major_version(&stdout)
        .or_else(|| parse_opencode_major_version(&stderr))
        .is_some_and(|major| major >= 2)
}

fn parse_opencode_major_version(text: &str) -> Option<u32> {
    for token in text.split_whitespace() {
        let trimmed = token.trim_start_matches('v');
        let Some((major_str, rest)) = trimmed.split_once('.') else {
            continue;
        };
        if rest.chars().next().is_some_and(|c| c.is_ascii_digit())
            && let Ok(major) = major_str.parse::<u32>()
        {
            return Some(major);
        }
    }
    None
}

pub async fn run_omp(state_paths: &StatePaths, args: OmpArgs) -> Result<i32, CliError> {
    let (spec, ephemeral_guards) = build_omp_invocation(state_paths, &args, None, false).await?;
    let cmd = command_from_terminal_spec(&spec);
    let exit_code = spawn_and_wait_interactive("omp", &spec.program, cmd).await?;
    drop(ephemeral_guards);
    Ok(exit_code)
}

/// Prepares an isolated [`TerminalLaunchSpec`] for launching Oh My Pi (`omp`) in a native terminal
/// window with zero project or `~/.omp/agent` side effects.
pub async fn prepare_omp_terminal_spec(
    state_paths: &StatePaths,
    args: &OmpArgs,
    cwd: Option<PathBuf>,
    hold_open: bool,
) -> Result<TerminalLaunchSpec, CliError> {
    let (mut spec, guards) = build_omp_invocation(state_paths, args, cwd, hold_open).await?;
    for guard in guards {
        spec.cleanup_files.push(guard.disarm());
    }
    Ok(spec)
}

async fn build_omp_invocation(
    state_paths: &StatePaths,
    args: &OmpArgs,
    cwd: Option<PathBuf>,
    hold_open: bool,
) -> Result<(TerminalLaunchSpec, Vec<EphemeralFileGuard>), CliError> {
    let resolved_cmd = resolve_omp_command(args.omp_bin.as_deref())?;
    let invocation = classify_omp_invocation(&args.passthrough_args);

    if matches!(invocation, OmpInvocationKind::DirectPassthrough) {
        let mut full_args: Vec<OsString> = resolved_cmd
            .prefix_args
            .iter()
            .map(OsString::from)
            .collect();
        full_args.extend(args.passthrough_args.iter().map(OsString::from));
        return Ok((
            TerminalLaunchSpec {
                title: "Liftgate · Oh My Pi (omp)".to_owned(),
                program: resolved_cmd.program,
                args: full_args,
                cwd,
                env_set: Vec::new(),
                env_remove: Vec::new(),
                cleanup_files: Vec::new(),
                hold_open_on_exit: hold_open,
            },
            Vec::new(),
        ));
    }

    let requested_model = args
        .model
        .as_deref()
        .map(strip_agy_provider_prefix)
        .filter(|s| !s.is_empty());

    let target = ensure_gateway_ready(
        state_paths,
        args.listen,
        args.api_key.clone(),
        requested_model,
        true,
    )
    .await?;

    let primary_model = requested_model
        .map(str::to_owned)
        .unwrap_or_else(|| target.default_model.clone());
    let discovered_models = target.client.fetch_models().await.unwrap_or_default();
    let fast_model = resolve_fast_model(
        args.fast_model.as_deref().map(strip_agy_provider_prefix),
        &primary_model,
        &discovered_models,
    );
    let auth_token = normalize_optional_api_key(target.api_key.as_deref())
        .unwrap_or_else(|| PLACEHOLDER_API_KEY.to_owned());

    let sync_params = HarnessSyncParams {
        gateway_base_url: target.client.base_url().to_owned(),
        api_key: auth_token.clone(),
        primary_model: primary_model.clone(),
        fast_model,
        no_bypass: args.no_bypass,
        no_code_mode: args.no_code_mode,
        discovered_models,
    };

    let code_mode = if args.no_code_mode {
        OmpCodeModePolicy::Disabled
    } else if args.full_code_mode {
        OmpCodeModePolicy::FullEvalNamespaces
    } else {
        OmpCodeModePolicy::Standard
    };

    let inherited_extensions = collect_existing_omp_extensions(cwd.as_deref());
    let provider_ts =
        build_omp_provider_extension_ts(&sync_params, Some(OMP_EPHEMERAL_API_KEY_ENV))?;
    let provider_guard =
        EphemeralFileGuard::create(state_paths, "omp-provider", "ts", provider_ts.as_bytes())?;

    let overlay_yaml = build_omp_ephemeral_overlay_yaml(
        &sync_params,
        code_mode,
        provider_guard.path(),
        &inherited_extensions,
    );
    let overlay_guard =
        EphemeralFileGuard::create(state_paths, "omp-overlay", "yml", overlay_yaml.as_bytes())?;

    let pi_config_files = merge_omp_config_files_env(
        std::env::var_os("PI_CONFIG_FILES").as_deref(),
        overlay_guard.path(),
    );
    let no_proxy = merge_loopback_no_proxy(
        std::env::var("NO_PROXY")
            .ok()
            .or_else(|| std::env::var("no_proxy").ok())
            .as_deref(),
    );

    let mut env_set = vec![
        (OMP_EPHEMERAL_API_KEY_ENV.to_owned(), auth_token),
        ("PI_CONFIG_FILES".to_owned(), pi_config_files),
        ("NO_PROXY".to_owned(), no_proxy.clone()),
        ("no_proxy".to_owned(), no_proxy),
    ];
    let mut env_remove = Vec::new();
    if args.no_code_mode {
        env_set.push(("PI_PY".to_owned(), "0".to_owned()));
        env_set.push(("PI_JS".to_owned(), "0".to_owned()));
    } else {
        env_remove.push("PI_PY".to_owned());
        env_remove.push("PI_JS".to_owned());
    }

    let mut full_args: Vec<OsString> = resolved_cmd
        .prefix_args
        .iter()
        .map(OsString::from)
        .collect();
    full_args.extend(args.passthrough_args.iter().map(OsString::from));

    Ok((
        TerminalLaunchSpec {
            title: format!("Liftgate · Oh My Pi ({primary_model})"),
            program: resolved_cmd.program,
            args: full_args,
            cwd,
            env_set,
            env_remove,
            cleanup_files: Vec::new(),
            hold_open_on_exit: hold_open,
        },
        vec![provider_guard, overlay_guard],
    ))
}

fn classify_omp_invocation(passthrough_args: &[String]) -> OmpInvocationKind {
    if passthrough_args.iter().any(|a| {
        matches!(
            a.as_str(),
            "-h" | "--help" | "-v" | "--version" | "--license"
        )
    }) {
        return OmpInvocationKind::DirectPassthrough;
    }
    let first_non_flag = passthrough_args.first().filter(|a| !a.starts_with('-'));
    match first_non_flag.map(String::as_str) {
        Some(
            "help" | "auth" | "auth-broker" | "auth-gateway" | "login" | "plugin" | "plugins"
            | "install" | "update" | "completions" | "__complete" | "stats" | "toks" | "token"
            | "usage" | "config" | "gc" | "cleanse" | "anonymize" | "ps" | "ssh" | "worktree"
            | "wt" | "skill" | "skills" | "tiny-models",
        ) => OmpInvocationKind::DirectPassthrough,
        Some(_) | None => OmpInvocationKind::GatewaySession,
    }
}

fn collect_existing_omp_extensions(cwd: Option<&Path>) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(global_paths) = resolve_omp_paths(true, cwd)
        && let Ok(text) = std::fs::read_to_string(&global_paths.config_path)
    {
        out.extend(extract_omp_yaml_extensions(&text));
    }
    if let Ok(local_paths) = resolve_omp_paths(false, cwd)
        && let Ok(text) = std::fs::read_to_string(&local_paths.config_path)
    {
        for item in extract_omp_yaml_extensions(&text) {
            if !out.iter().any(|existing| existing == &item) {
                out.push(item);
            }
        }
    }
    out
}

fn merge_omp_config_files_env(existing: Option<&std::ffi::OsStr>, overlay_path: &Path) -> String {
    let sep = if cfg!(windows) { ';' } else { ':' };
    let mut parts: Vec<String> = existing
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
        .split(sep)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    parts.push(overlay_path.display().to_string());
    parts.join(&sep.to_string())
}

/// Prepares an isolated [`TerminalLaunchSpec`] for launching Codex CLI in a native terminal
/// window with zero global config side effects.
pub async fn prepare_codex_terminal_spec(
    state_paths: &StatePaths,
    args: &CodexArgs,
    cwd: Option<PathBuf>,
    hold_open: bool,
) -> Result<TerminalLaunchSpec, CliError> {
    let (mut spec, guard) = build_codex_invocation(state_paths, args, cwd, hold_open).await?;
    if let Some(g) = guard {
        spec.cleanup_files.push(g.disarm());
    }
    Ok(spec)
}

async fn build_codex_invocation(
    state_paths: &StatePaths,
    args: &CodexArgs,
    cwd: Option<PathBuf>,
    hold_open: bool,
) -> Result<(TerminalLaunchSpec, Option<EphemeralFileGuard>), CliError> {
    let resolved_cmd = resolve_codex_command(args.codex_bin.as_deref())?;
    let invocation = classify_codex_invocation(&args.passthrough_args);

    let no_proxy = merge_loopback_no_proxy(
        std::env::var("NO_PROXY")
            .ok()
            .or_else(|| std::env::var("no_proxy").ok())
            .as_deref(),
    );

    if matches!(
        invocation,
        CodexInvocationKind::DirectPassthrough | CodexInvocationKind::NativeUpdate
    ) {
        let mut full_args: Vec<OsString> = resolved_cmd
            .prefix_args
            .iter()
            .map(OsString::from)
            .collect();
        full_args.extend(args.passthrough_args.iter().map(OsString::from));
        return Ok((
            TerminalLaunchSpec {
                title: "Liftgate · Codex CLI".to_owned(),
                program: resolved_cmd.program,
                args: full_args,
                cwd,
                env_set: if invocation == CodexInvocationKind::NativeUpdate {
                    Vec::new()
                } else {
                    vec![
                        ("NO_PROXY".to_owned(), no_proxy.clone()),
                        ("no_proxy".to_owned(), no_proxy),
                    ]
                },
                env_remove: Vec::new(),
                cleanup_files: Vec::new(),
                hold_open_on_exit: hold_open,
            },
            None,
        ));
    }

    let explicit_passthrough_model = extract_explicit_model_arg(&args.passthrough_args);
    let requested_model = args
        .model
        .as_deref()
        .or(explicit_passthrough_model)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let target = ensure_gateway_ready(
        state_paths,
        args.listen,
        args.api_key.clone(),
        requested_model,
        true,
    )
    .await?;

    let primary_model = requested_model.unwrap_or(&target.default_model).to_owned();
    let normalized_key = normalize_optional_api_key(target.api_key.as_deref());
    let require_env_key = normalized_key.is_some();
    let auth_token = normalized_key.unwrap_or_else(|| PLACEHOLDER_API_KEY.to_owned());

    let requested_tool_mode = if args.no_code_mode {
        Some("direct")
    } else if args.code_mode_only {
        Some("code_mode_only")
    } else {
        Some("code_mode")
    };

    let catalog_bytes = match target
        .client
        .fetch_codex_catalog_json_with_mode(&primary_model, requested_tool_mode)
        .await
    {
        Ok(bytes) => bytes,
        Err(_) => {
            let discovered = target.client.fetch_models().await.unwrap_or_default();
            build_offline_codex_catalog_bytes_with_mode(
                &discovered,
                &primary_model,
                requested_tool_mode,
            )?
        }
    };

    let ephemeral_catalog =
        EphemeralFileGuard::create(state_paths, "codex-catalog", "json", &catalog_bytes)?;
    let catalog_slash = ephemeral_catalog
        .path()
        .to_string_lossy()
        .replace('\\', "/");

    let has_explicit_model = explicit_passthrough_model.is_some()
        || has_explicit_codex_config_key(&args.passthrough_args, "model");
    let has_explicit_approval =
        args.passthrough_args.iter().any(|a| {
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
        }) || has_explicit_codex_config_key(&args.passthrough_args, "approval_policy")
            || has_explicit_codex_config_key(&args.passthrough_args, "sandbox_mode");

    let mut full_args: Vec<OsString> = resolved_cmd
        .prefix_args
        .iter()
        .map(OsString::from)
        .collect();
    let push_cfg = |out: &mut Vec<OsString>, expr: String| {
        out.push(OsString::from("-c"));
        out.push(OsString::from(expr));
    };
    match invocation {
        CodexInvocationKind::InteractiveSession | CodexInvocationKind::ExecSession => {
            if !args.no_bypass && !has_explicit_approval {
                full_args.push(OsString::from("--dangerously-bypass-approvals-and-sandbox"));
            }
        }
        CodexInvocationKind::AgentsView | CodexInvocationKind::GatewayConfigOnly => {
            if !args.no_bypass && !has_explicit_approval {
                push_cfg(&mut full_args, "approval_policy='never'".to_owned());
                push_cfg(
                    &mut full_args,
                    "sandbox_mode='danger-full-access'".to_owned(),
                );
            }
        }
        CodexInvocationKind::DirectPassthrough | CodexInvocationKind::NativeUpdate => {}
    }
    let toml_primary = toml_inline_string(&primary_model);
    let toml_catalog = toml_inline_string(&catalog_slash);
    let toml_base_v1 = toml_inline_string(&format!("{}/v1", target.client.base_url()));
    push_cfg(&mut full_args, "model_provider='agy'".to_owned());
    if !has_explicit_model {
        push_cfg(&mut full_args, format!("model={toml_primary}"));
    }
    push_cfg(&mut full_args, format!("model_catalog_json={toml_catalog}"));
    push_cfg(&mut full_args, "web_search='live'".to_owned());
    push_cfg(
        &mut full_args,
        "model_providers.agy.name='Liftgate'".to_owned(),
    );
    push_cfg(
        &mut full_args,
        format!("model_providers.agy.base_url={toml_base_v1}"),
    );
    if require_env_key {
        push_cfg(
            &mut full_args,
            "model_providers.agy.env_key='AGY_GATEWAY_API_KEY'".to_owned(),
        );
    }
    push_cfg(
        &mut full_args,
        "model_providers.agy.wire_api='responses'".to_owned(),
    );
    push_cfg(
        &mut full_args,
        "model_providers.agy.requires_openai_auth=false".to_owned(),
    );
    if !has_explicit_codex_config_key(&args.passthrough_args, CODEX_STREAM_IDLE_TIMEOUT_KEY) {
        push_cfg(
            &mut full_args,
            format!("{CODEX_STREAM_IDLE_TIMEOUT_KEY}={DEFAULT_REQUEST_TIMEOUT_MS}"),
        );
    }
    if args.no_code_mode {
        push_cfg(&mut full_args, "features.code_mode=false".to_owned());
        push_cfg(&mut full_args, "features.code_mode_only=false".to_owned());
    } else {
        push_cfg(
            &mut full_args,
            "suppress_unstable_features_warning=true".to_owned(),
        );
        push_cfg(&mut full_args, "features.code_mode=true".to_owned());
        if args.code_mode_only {
            push_cfg(&mut full_args, "features.code_mode_only=true".to_owned());
        }
    }
    if matches!(
        invocation,
        CodexInvocationKind::InteractiveSession | CodexInvocationKind::AgentsView
    ) {
        let (options, positional_tail) = args
            .passthrough_args
            .split_at(slice_before_double_dash(&args.passthrough_args).len());
        full_args.extend(options.iter().map(OsString::from));
        push_cfg(
            &mut full_args,
            "check_for_update_on_startup=false".to_owned(),
        );
        full_args.extend(positional_tail.iter().map(OsString::from));
    } else {
        full_args.extend(args.passthrough_args.iter().map(OsString::from));
    }

    Ok((
        TerminalLaunchSpec {
            title: format!("Liftgate · Codex CLI ({primary_model})"),
            program: resolved_cmd.program,
            args: full_args,
            cwd,
            env_set: vec![
                ("AGY_GATEWAY_API_KEY".to_owned(), auth_token),
                ("NO_PROXY".to_owned(), no_proxy.clone()),
                ("no_proxy".to_owned(), no_proxy),
            ],
            env_remove: vec!["AGY_API_KEY".to_owned()],
            cleanup_files: Vec::new(),
            hold_open_on_exit: hold_open,
        },
        Some(ephemeral_catalog),
    ))
}

fn build_claude_gateway_env_pairs(
    base_url: &str,
    auth_token: &str,
    primary_model: &str,
    fast_model: &str,
    no_code_mode: bool,
    no_proxy: &str,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut env_set = vec![
        ("ANTHROPIC_BASE_URL".to_owned(), base_url.to_owned()),
        ("ANTHROPIC_AUTH_TOKEN".to_owned(), auth_token.to_owned()),
        ("ANTHROPIC_MODEL".to_owned(), primary_model.to_owned()),
        (
            "ANTHROPIC_DEFAULT_FABLE_MODEL".to_owned(),
            primary_model.to_owned(),
        ),
        (
            "ANTHROPIC_DEFAULT_OPUS_MODEL".to_owned(),
            primary_model.to_owned(),
        ),
        (
            "ANTHROPIC_DEFAULT_SONNET_MODEL".to_owned(),
            primary_model.to_owned(),
        ),
        (
            "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_owned(),
            fast_model.to_owned(),
        ),
        (
            "ANTHROPIC_SMALL_FAST_MODEL".to_owned(),
            fast_model.to_owned(),
        ),
        (
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_owned(),
            "1".to_owned(),
        ),
        ("CLAUDE_CODE_AUTO_MODE_SERVER".to_owned(), "0".to_owned()),
        (
            "CLAUDE_CODE_EXPERIMENTAL_CODE_MODE".to_owned(),
            if no_code_mode { "0" } else { "1" }.to_owned(),
        ),
        ("NO_PROXY".to_owned(), no_proxy.to_owned()),
        ("no_proxy".to_owned(), no_proxy.to_owned()),
    ];
    for key in ["API_TIMEOUT_MS", "CLAUDE_STREAM_IDLE_TIMEOUT_MS"] {
        if std::env::var_os(key).is_none() {
            env_set.push((key.to_owned(), DEFAULT_REQUEST_TIMEOUT_MS.to_string()));
        }
    }
    let env_remove = vec![
        "ANTHROPIC_API_KEY".to_owned(),
        "AGY_API_KEY".to_owned(),
        "CLAUDE_CODE_MAX_CONTEXT_TOKENS".to_owned(),
        "CLAUDE_CODE_AUTO_COMPACT_WINDOW".to_owned(),
    ];
    (env_set, env_remove)
}

fn command_from_terminal_spec(spec: &TerminalLaunchSpec) -> Command {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args);
    for k in &spec.env_remove {
        cmd.env_remove(k);
    }
    for (k, v) in &spec.env_set {
        cmd.env(k, v);
    }
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    cmd
}

fn classify_claude_invocation(passthrough_args: &[String]) -> ClaudeInvocationKind {
    let pre_dd = slice_before_double_dash(passthrough_args);
    if pre_dd.iter().any(|a| matches!(a.as_str(), "-h" | "--help")) {
        return ClaudeInvocationKind::DirectPassthrough;
    }

    if let Some(first) = pre_dd.first().map(String::as_str) {
        match first {
            "agents" => {
                if pre_dd.iter().any(|a| a == "--json") {
                    return ClaudeInvocationKind::DirectPassthrough;
                }
                return ClaudeInvocationKind::AgentsView;
            }
            "help" | "-v" | "--version" | "attach" | "logs" | "stop" | "kill" | "rm" | "mcp"
            | "plugin" | "plugins" | "auth" | "auto-mode" | "import" | "install" | "purge"
            | "setup-token" | "update" | "upgrade" | "gateway" => {
                return ClaudeInvocationKind::DirectPassthrough;
            }
            "respawn" | "doctor" | "ultrareview" => {
                return ClaudeInvocationKind::GatewayEnvSubcommand;
            }
            _ => {}
        }
    }

    if pre_dd
        .iter()
        .any(|a| matches!(a.as_str(), "-v" | "--version"))
    {
        return ClaudeInvocationKind::DirectPassthrough;
    }

    ClaudeInvocationKind::AgentSession
}

fn classify_codex_invocation(passthrough_args: &[String]) -> CodexInvocationKind {
    let pre_dd = slice_before_double_dash(passthrough_args);
    let subcommand = find_codex_root_subcommand(pre_dd);
    if matches!(subcommand, Some(("update", _))) {
        return CodexInvocationKind::NativeUpdate;
    }
    if pre_dd
        .iter()
        .any(|a| matches!(a.as_str(), "-h" | "--help" | "-V" | "--version"))
    {
        return CodexInvocationKind::DirectPassthrough;
    }

    let Some((sub, sub_idx)) = subcommand else {
        return CodexInvocationKind::InteractiveSession;
    };

    match sub {
        "help" | "completion" | "queue" | "archive" | "unarchive" | "delete" | "mcp" | "plugin"
        | "features" | "login" | "logout" | "apply" | "a" | "cloud" | "sandbox"
        | "migrate-rollouts" | "migrate-nfs-db" | "app" => CodexInvocationKind::DirectPassthrough,
        "app-server" => {
            if matches!(
                pre_dd.get(sub_idx + 1).map(String::as_str),
                Some("daemon" | "proxy" | "generate-ts" | "generate-json-schema" | "help")
            ) {
                CodexInvocationKind::DirectPassthrough
            } else {
                CodexInvocationKind::GatewayConfigOnly
            }
        }
        "agents" => CodexInvocationKind::AgentsView,
        "review" | "doctor" | "debug" | "remote-control" | "exec-server" => {
            CodexInvocationKind::GatewayConfigOnly
        }
        "exec" | "e" => {
            if pre_dd.get(sub_idx + 1).map(String::as_str) == Some("help") {
                CodexInvocationKind::DirectPassthrough
            } else {
                CodexInvocationKind::ExecSession
            }
        }
        _ => CodexInvocationKind::InteractiveSession,
    }
}

fn find_codex_root_subcommand(pre_dd: &[String]) -> Option<(&str, usize)> {
    const ROOT_VALUE_OPTS: &[&str] = &[
        "-c",
        "--config",
        "--enable",
        "--disable",
        "--remote",
        "--remote-auth-token-env",
        "-i",
        "--image",
        "-m",
        "--model",
        "--local-provider",
        "-p",
        "--profile",
        "-s",
        "--sandbox",
        "-C",
        "--cd",
        "--add-dir",
        "-a",
        "--ask-for-approval",
    ];
    const KNOWN_SUBCOMMANDS: &[&str] = &[
        "agents",
        "exec",
        "e",
        "review",
        "login",
        "logout",
        "mcp",
        "plugin",
        "app-server",
        "remote-control",
        "app",
        "completion",
        "update",
        "doctor",
        "sandbox",
        "debug",
        "apply",
        "a",
        "resume",
        "queue",
        "archive",
        "delete",
        "migrate-rollouts",
        "migrate-nfs-db",
        "unarchive",
        "fork",
        "cloud",
        "exec-server",
        "features",
        "help",
    ];

    let mut idx = 0usize;
    while let Some(tok) = pre_dd.get(idx) {
        let s = tok.as_str();
        if !s.starts_with('-') {
            return KNOWN_SUBCOMMANDS.contains(&s).then_some((s, idx));
        }
        if ROOT_VALUE_OPTS.contains(&s) {
            idx += 2;
            continue;
        }
        idx += 1;
    }
    None
}

fn slice_before_double_dash(args: &[String]) -> &[String] {
    match args.iter().position(|a| a == "--") {
        Some(pos) => args.get(..pos).unwrap_or_default(),
        None => args,
    }
}

fn extract_explicit_model_arg(args: &[String]) -> Option<&str> {
    let pre_dd = slice_before_double_dash(args);
    let mut idx = 0usize;
    while let Some(tok) = pre_dd.get(idx) {
        let s = tok.as_str();
        if matches!(s, "-m" | "--model") {
            return pre_dd
                .get(idx + 1)
                .map(String::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty());
        }
        if let Some(val) = s.strip_prefix("--model=").or_else(|| s.strip_prefix("-m=")) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
        idx += 1;
    }
    None
}

fn has_explicit_codex_config_key(args: &[String], key: &str) -> bool {
    let pre_dd = slice_before_double_dash(args);
    let mut idx = 0usize;
    while let Some(tok) = pre_dd.get(idx) {
        let s = tok.as_str();
        let expr = if matches!(s, "-c" | "--config") {
            idx += 2;
            pre_dd.get(idx - 1).map(String::as_str)
        } else if let Some(rest) = s
            .strip_prefix("--config=")
            .or_else(|| s.strip_prefix("-c="))
        {
            idx += 1;
            Some(rest)
        } else {
            idx += 1;
            None
        };
        if let Some(kv) = expr
            && let Some((lhs, _)) = kv.split_once('=')
            && lhs.trim() == key
        {
            return true;
        }
    }
    false
}

fn default_claude_effort_level(model_id: &str) -> Option<&'static str> {
    let lower = model_id.trim().to_ascii_lowercase();
    if lower.ends_with("-xhigh") {
        Some("xhigh")
    } else if lower.ends_with("-high") || lower.ends_with("-thinking") {
        Some("high")
    } else if lower.ends_with("-medium") {
        Some("medium")
    } else if lower.ends_with("-low") {
        Some("low")
    } else {
        None
    }
}

pub(crate) fn toml_inline_string(val: &str) -> String {
    if !val.contains('\'') && !val.chars().any(char::is_control) {
        format!("'{val}'")
    } else {
        serde_json::to_string(val).unwrap_or_else(|_| format!("{val:?}"))
    }
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

pub fn resolve_opencode_command(explicit: Option<&Path>) -> Result<ResolvedCommand, CliError> {
    if let Some(p) = explicit {
        if is_executable_file(p) {
            return Ok(ResolvedCommand {
                program: canonicalize_clean(p),
                prefix_args: Vec::new(),
            });
        }
        return Err(CliError::HarnessNotFound {
            tool: "opencode",
            hint: "explicit `--opencode-bin` path does not exist or is not executable",
        });
    }

    let path_dirs = split_expanded_path_var();

    #[cfg(windows)]
    {
        if let Some(exe) = find_in_dirs(&path_dirs, &["opencode.exe"]) {
            return Ok(ResolvedCommand {
                program: exe,
                prefix_args: Vec::new(),
            });
        }
        for cand in windows_opencode_exe_candidates() {
            if is_executable_file(&cand) {
                return Ok(ResolvedCommand {
                    program: canonicalize_clean(&cand),
                    prefix_args: Vec::new(),
                });
            }
        }
        if let Some(node_cmd) = resolve_windows_npm_script(
            &path_dirs,
            &["node_modules", "opencode-ai", "bin", "opencode"],
        ) {
            return Ok(node_cmd);
        }
        if let Some(shim) = find_in_dirs(&path_dirs, &["opencode.cmd", "opencode.bat"]) {
            return Ok(ResolvedCommand {
                program: PathBuf::from(
                    std::env::var_os("ComSpec").unwrap_or_else(|| OsString::from("cmd.exe")),
                ),
                prefix_args: vec![PathBuf::from("/d"), PathBuf::from("/c"), shim],
            });
        }
        if let Some(ps1) = find_in_dirs(&path_dirs, &["opencode.ps1"]) {
            return Ok(ResolvedCommand {
                program: PathBuf::from("powershell.exe"),
                prefix_args: vec![
                    PathBuf::from("-NoProfile"),
                    PathBuf::from("-ExecutionPolicy"),
                    PathBuf::from("Bypass"),
                    PathBuf::from("-File"),
                    ps1,
                ],
            });
        }
    }

    #[cfg(not(windows))]
    {
        if let Some(bin) = find_in_dirs(&path_dirs, &["opencode"]) {
            return Ok(ResolvedCommand {
                program: bin,
                prefix_args: Vec::new(),
            });
        }
        if let Ok(home) = user_home_dir() {
            let oc_home_bin = home.join(".opencode").join("bin").join("opencode");
            if is_executable_file(&oc_home_bin) {
                return Ok(ResolvedCommand {
                    program: canonicalize_clean(&oc_home_bin),
                    prefix_args: Vec::new(),
                });
            }
        }
        for cand in unix_extra_bin_candidates("opencode") {
            if is_executable_file(&cand) {
                return Ok(ResolvedCommand {
                    program: canonicalize_clean(&cand),
                    prefix_args: Vec::new(),
                });
            }
        }
    }

    Err(CliError::HarnessNotFound {
        tool: "opencode",
        hint: "install OpenCode (`npm i -g opencode-ai` or `curl -fsSL https://opencode.ai/install | bash`) or pass `--opencode-bin <path>`",
    })
}

pub fn resolve_omp_command(explicit: Option<&Path>) -> Result<ResolvedCommand, CliError> {
    if let Some(p) = explicit {
        if is_executable_file(p) {
            return Ok(ResolvedCommand {
                program: canonicalize_clean(p),
                prefix_args: Vec::new(),
            });
        }
        return Err(CliError::HarnessNotFound {
            tool: "omp",
            hint: "explicit `--omp-bin` path does not exist or is not executable",
        });
    }

    let path_dirs = split_expanded_path_var();

    #[cfg(windows)]
    {
        if let Some(exe) = find_in_dirs(&path_dirs, &["omp.exe"]) {
            return Ok(ResolvedCommand {
                program: exe,
                prefix_args: Vec::new(),
            });
        }
        for cand in windows_omp_exe_candidates() {
            if is_executable_file(&cand) {
                return Ok(ResolvedCommand {
                    program: canonicalize_clean(&cand),
                    prefix_args: Vec::new(),
                });
            }
        }
        if let Some(shim) = find_in_dirs(&path_dirs, &["omp.cmd", "omp.bat"]) {
            return Ok(ResolvedCommand {
                program: PathBuf::from(
                    std::env::var_os("ComSpec").unwrap_or_else(|| OsString::from("cmd.exe")),
                ),
                prefix_args: vec![PathBuf::from("/d"), PathBuf::from("/c"), shim],
            });
        }
        if let Some(ps1) = find_in_dirs(&path_dirs, &["omp.ps1"]) {
            return Ok(ResolvedCommand {
                program: PathBuf::from("powershell.exe"),
                prefix_args: vec![
                    PathBuf::from("-NoProfile"),
                    PathBuf::from("-ExecutionPolicy"),
                    PathBuf::from("Bypass"),
                    PathBuf::from("-File"),
                    ps1,
                ],
            });
        }
    }

    #[cfg(not(windows))]
    {
        if let Some(bin) = find_in_dirs(&path_dirs, &["omp"]) {
            return Ok(ResolvedCommand {
                program: bin,
                prefix_args: Vec::new(),
            });
        }
        if let Ok(home) = user_home_dir() {
            let omp_home_bin = home.join(".omp").join("bin").join("omp");
            if is_executable_file(&omp_home_bin) {
                return Ok(ResolvedCommand {
                    program: canonicalize_clean(&omp_home_bin),
                    prefix_args: Vec::new(),
                });
            }
        }
        for cand in unix_extra_bin_candidates("omp") {
            if is_executable_file(&cand) {
                return Ok(ResolvedCommand {
                    program: canonicalize_clean(&cand),
                    prefix_args: Vec::new(),
                });
            }
        }
    }

    Err(CliError::HarnessNotFound {
        tool: "omp",
        hint: "install Oh My Pi (`bun install -g @oh-my-pi/pi-coding-agent` or `curl -fsSL https://omp.sh/install | sh`) or pass `--omp-bin <path>`",
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
fn windows_opencode_exe_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(home) = user_home_dir() {
        out.push(home.join(".opencode").join("bin").join("opencode.exe"));
        out.push(home.join(".local").join("bin").join("opencode.exe"));
        out.push(home.join(".bun").join("bin").join("opencode.exe"));
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

#[cfg(windows)]
fn windows_omp_exe_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(home) = user_home_dir() {
        out.push(home.join(".omp").join("bin").join("omp.exe"));
        out.push(home.join(".bun").join("bin").join("omp.exe"));
        out.push(home.join(".local").join("bin").join("omp.exe"));
        out.push(home.join("scoop").join("shims").join("omp.exe"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let local_path = PathBuf::from(local);
        out.push(
            local_path
                .join("Microsoft")
                .join("WinGet")
                .join("Links")
                .join("omp.exe"),
        );
        out.push(local_path.join("Programs").join("omp").join("omp.exe"));
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

    #[test]
    fn infers_default_claude_effort_level_from_model_suffix() {
        assert_eq!(
            default_claude_effort_level("gemini-4-neon-xhigh"),
            Some("xhigh")
        );
        assert_eq!(
            default_claude_effort_level("gemini-4-carbon-high"),
            Some("high")
        );
        assert_eq!(
            default_claude_effort_level("claude-opus-4-6-thinking"),
            Some("high")
        );
        assert_eq!(
            default_claude_effort_level("gemini-4-argon-medium"),
            Some("medium")
        );
        assert_eq!(
            default_claude_effort_level("gemini-3.8-flash-low"),
            Some("low")
        );
        assert_eq!(default_claude_effort_level("claude-sonnet-4-6"), None);
    }

    #[test]
    fn classifies_claude_invocations_across_sessions_jobs_and_utilities() {
        let strs =
            |items: &[&str]| -> Vec<String> { items.iter().map(|s| (*s).to_owned()).collect() };

        assert_eq!(
            classify_claude_invocation(&strs(&[])),
            ClaudeInvocationKind::AgentSession
        );
        assert_eq!(
            classify_claude_invocation(&strs(&["--resume", "abc"])),
            ClaudeInvocationKind::AgentSession
        );
        assert_eq!(
            classify_claude_invocation(&strs(&["-c", "-p", "continue work"])),
            ClaudeInvocationKind::AgentSession
        );
        assert_eq!(
            classify_claude_invocation(&strs(&["fix failing tests"])),
            ClaudeInvocationKind::AgentSession
        );

        for sub in [
            "logs", "attach", "stop", "kill", "rm", "mcp", "plugin", "purge",
        ] {
            assert_eq!(
                classify_claude_invocation(&strs(&[sub, "target"])),
                ClaudeInvocationKind::DirectPassthrough,
                "expected DirectPassthrough for claude {sub}"
            );
        }
        assert_eq!(
            classify_claude_invocation(&strs(&["agents", "--json"])),
            ClaudeInvocationKind::DirectPassthrough
        );
        assert_eq!(
            classify_claude_invocation(&strs(&["agents"])),
            ClaudeInvocationKind::AgentsView
        );
        assert_eq!(
            classify_claude_invocation(&strs(&["--version"])),
            ClaudeInvocationKind::DirectPassthrough
        );
        assert_eq!(
            classify_claude_invocation(&strs(&["--resume", "--help"])),
            ClaudeInvocationKind::DirectPassthrough
        );
        for sub in ["respawn", "doctor", "ultrareview"] {
            assert_eq!(
                classify_claude_invocation(&strs(&[sub])),
                ClaudeInvocationKind::GatewayEnvSubcommand,
                "expected GatewayEnvSubcommand for claude {sub}"
            );
        }
    }

    #[test]
    fn classifies_codex_invocations_and_root_options_accurately() {
        let strs =
            |items: &[&str]| -> Vec<String> { items.iter().map(|s| (*s).to_owned()).collect() };

        assert_eq!(
            classify_codex_invocation(&strs(&[])),
            CodexInvocationKind::InteractiveSession
        );
        assert_eq!(
            classify_codex_invocation(&strs(&["explain architecture"])),
            CodexInvocationKind::InteractiveSession
        );
        assert_eq!(
            classify_codex_invocation(&strs(&["resume", "--last"])),
            CodexInvocationKind::InteractiveSession
        );
        assert_eq!(
            classify_codex_invocation(&strs(&["-C", "../other", "resume", "--all"])),
            CodexInvocationKind::InteractiveSession
        );
        assert_eq!(
            classify_codex_invocation(&strs(&["fork", "--last"])),
            CodexInvocationKind::InteractiveSession
        );
        assert_eq!(
            classify_codex_invocation(&strs(&["exec", "resume", "--last", "test"])),
            CodexInvocationKind::ExecSession
        );

        assert_eq!(
            classify_codex_invocation(&strs(&["agents"])),
            CodexInvocationKind::AgentsView
        );
        for sub in ["review", "doctor", "debug"] {
            assert_eq!(
                classify_codex_invocation(&strs(&[sub])),
                CodexInvocationKind::GatewayConfigOnly,
                "expected GatewayConfigOnly for codex {sub}"
            );
        }

        for sub in [
            "queue",
            "archive",
            "unarchive",
            "delete",
            "mcp",
            "plugin",
            "features",
            "completion",
            "sandbox",
            "login",
            "logout",
        ] {
            assert_eq!(
                classify_codex_invocation(&strs(&[sub])),
                CodexInvocationKind::DirectPassthrough,
                "expected DirectPassthrough for codex {sub}"
            );
        }
        assert_eq!(
            classify_codex_invocation(&strs(&["app-server", "daemon", "version"])),
            CodexInvocationKind::DirectPassthrough
        );
        assert_eq!(
            classify_codex_invocation(&strs(&["app-server", "generate-ts"])),
            CodexInvocationKind::DirectPassthrough
        );
        assert_eq!(
            classify_codex_invocation(&strs(&["app-server"])),
            CodexInvocationKind::GatewayConfigOnly
        );
        assert_eq!(
            classify_codex_invocation(&strs(&["exec", "help"])),
            CodexInvocationKind::DirectPassthrough
        );
        assert_eq!(
            classify_codex_invocation(&strs(&["resume", "--help"])),
            CodexInvocationKind::DirectPassthrough
        );
        assert_eq!(
            classify_codex_invocation(&strs(&["-V"])),
            CodexInvocationKind::DirectPassthrough
        );

        assert_eq!(
            extract_explicit_model_arg(&strs(&["resume", "-m", "gemini-3.8-pro-high"])),
            Some("gemini-3.8-pro-high")
        );
        assert!(has_explicit_codex_config_key(
            &strs(&["-c", "model = 'gemini-3.8-flash-low'", "resume"]),
            "model"
        ));
    }

    #[tokio::test]
    async fn codex_terminal_session_disables_updates_after_passthrough_overrides()
    -> Result<(), Box<dyn std::error::Error>> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::TcpListener;

        let state_paths = StatePaths::resolve(Some(
            std::env::temp_dir().join(format!("liftgate-codex-{}", uuid::Uuid::new_v4())),
        ))?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let args = CodexArgs {
            listen: Some(listener.local_addr()?),
            api_key: Some("test-gateway-key".into()),
            codex_bin: Some(std::env::current_exe()?),
            passthrough_args: [
                "resume",
                "-c",
                "check_for_update_on_startup=true",
                "--",
                "-prompt",
            ]
            .map(str::to_owned)
            .to_vec(),
            ..CodexArgs::default_interactive()
        };
        let serve_gateway = async {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await?;
                let mut request = [0; 4096];
                let _ = stream.read(&mut request).await?;
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .await?;
            }
            Ok::<_, std::io::Error>(())
        };
        let (spec, served) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(
                prepare_codex_terminal_spec(&state_paths, &args, None, false),
                serve_gateway,
            )
        })
        .await?;
        let spec = spec?;
        served?;
        std::fs::remove_dir_all(state_paths.root())?;

        let command = command_from_terminal_spec(&spec);
        let command = command.as_std();
        let argv = command.get_args().collect::<Vec<_>>();
        assert!(
            argv.ends_with(
                &[
                    "resume",
                    "-c",
                    "check_for_update_on_startup=true",
                    "-c",
                    "check_for_update_on_startup=false",
                    "--",
                    "-prompt"
                ]
                .map(std::ffi::OsStr::new)
            )
        );
        assert!(command.get_envs().any(|(key, value)| {
            key == "AGY_GATEWAY_API_KEY" && value == Some(std::ffi::OsStr::new("test-gateway-key"))
        }));
        Ok(())
    }

    #[tokio::test]
    async fn explicit_codex_update_uses_the_native_environment()
    -> Result<(), Box<dyn std::error::Error>> {
        let state_paths =
            StatePaths::resolve(Some(PathBuf::from("unused-codex-update-test-state")))?;
        let args = CodexArgs {
            codex_bin: Some(std::env::current_exe()?),
            passthrough_args: vec!["update".to_owned()],
            ..CodexArgs::default_interactive()
        };
        let (spec, catalog) = build_codex_invocation(&state_paths, &args, None, false).await?;
        let command = command_from_terminal_spec(&spec);
        assert_eq!(
            command.as_std().get_args().collect::<Vec<_>>(),
            ["update"].map(std::ffi::OsStr::new)
        );
        assert_eq!(command.as_std().get_envs().count(), 0);
        assert!(catalog.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn opencode_direct_passthrough_subcommands_do_not_start_gateway()
    -> Result<(), Box<dyn std::error::Error>> {
        let state_paths =
            StatePaths::resolve(Some(PathBuf::from("unused-opencode-passthrough-state")))?;
        let args = OpencodeArgs {
            opencode_bin: Some(std::env::current_exe()?),
            passthrough_args: vec!["mcp".to_owned(), "list".to_owned()],
            ..OpencodeArgs::default_interactive()
        };
        let spec = build_opencode_invocation(&state_paths, &args, None, false).await?;
        assert_eq!(
            spec.args,
            vec![OsString::from("mcp"), OsString::from("list")]
        );
        assert!(spec.env_set.is_empty());
        Ok(())
    }

    #[test]
    fn parses_opencode_major_versions_and_merges_flags() {
        assert_eq!(parse_opencode_major_version("2.3.2\n"), Some(2));
        assert_eq!(parse_opencode_major_version("opencode v1.4.12"), Some(1));
        assert_eq!(parse_opencode_major_version("dev-snapshot"), None);
        assert_eq!(merge_opencode_flag(None, "full_codemode"), "full_codemode");
        assert_eq!(
            merge_opencode_flag(Some("sqlite_fts,-full_codemode"), "full_codemode"),
            "sqlite_fts,full_codemode"
        );
    }

    #[tokio::test]
    async fn omp_direct_passthrough_subcommands_do_not_start_gateway()
    -> Result<(), Box<dyn std::error::Error>> {
        let state_paths = StatePaths::resolve(Some(PathBuf::from("unused-omp-passthrough-state")))?;
        for sub_args in [
            vec!["stats".to_owned()],
            vec!["plugin".to_owned(), "list".to_owned()],
            vec!["--version".to_owned()],
        ] {
            let args = OmpArgs {
                omp_bin: Some(std::env::current_exe()?),
                passthrough_args: sub_args.clone(),
                ..OmpArgs::default_interactive()
            };
            let (spec, guards) = build_omp_invocation(&state_paths, &args, None, false).await?;
            assert!(guards.is_empty());
            assert_eq!(
                spec.args,
                sub_args.into_iter().map(OsString::from).collect::<Vec<_>>()
            );
            assert!(spec.env_set.is_empty());
        }
        Ok(())
    }

    #[test]
    fn merges_pi_config_files_preserving_existing_entries() {
        let sep = if cfg!(windows) { ';' } else { ':' };
        let overlay = Path::new("omp-overlay-test.yml");
        assert_eq!(
            merge_omp_config_files_env(None, overlay),
            "omp-overlay-test.yml"
        );
        let existing = OsString::from(format!("first.yml{sep}second.yml"));
        assert_eq!(
            merge_omp_config_files_env(Some(existing.as_os_str()), overlay),
            format!("first.yml{sep}second.yml{sep}omp-overlay-test.yml")
        );
    }
}
