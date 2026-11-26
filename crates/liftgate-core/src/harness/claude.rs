//! Interactive and terminal-window launcher for Claude Code (`lg cc`).

use std::ffi::OsString;
use std::path::PathBuf;

use liftgate_harness::args_util::extract_explicit_model_arg;
use liftgate_harness::claude::{
    ClaudeInvocationKind, build_claude_gateway_env_pairs, classify_claude_invocation,
    default_claude_effort_level,
};
use liftgate_harness::merge_endpoint_no_proxy;
use serde_json::Value;

use crate::cli::{ClaudeArgs, ClaudeDesktopArgs, PLACEHOLDER_API_KEY, normalize_optional_api_key};
use crate::config_sync::{HarnessSyncParams, build_claude_settings_map, resolve_fast_model};
use crate::daemon::ensure_gateway_ready_for_target;
use crate::error::CliError;
use crate::state::{EphemeralFileGuard, StatePaths};
use crate::terminal::TerminalLaunchSpec;

use super::{command_from_terminal_spec, resolve_claude_command, spawn_and_wait_interactive};

pub async fn run_claude(state_paths: &StatePaths, args: ClaudeArgs) -> Result<i32, CliError> {
    if args.desktop {
        let _ = crate::desktop::launch_claude_desktop(
            state_paths,
            ClaudeDesktopArgs {
                listen: args.listen,
                remote: args.remote,
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

pub(super) async fn build_claude_invocation(
    state_paths: &StatePaths,
    args: &ClaudeArgs,
    cwd: Option<PathBuf>,
    hold_open: bool,
) -> Result<(TerminalLaunchSpec, Option<EphemeralFileGuard>), CliError> {
    let resolved_cmd = resolve_claude_command(args.claude_bin.as_deref())?;
    let invocation = classify_claude_invocation(&args.passthrough_args);

    let existing_no_proxy = std::env::var("NO_PROXY")
        .ok()
        .or_else(|| std::env::var("no_proxy").ok());

    if invocation == ClaudeInvocationKind::DirectPassthrough {
        let no_proxy = merge_endpoint_no_proxy(existing_no_proxy.as_deref(), None);
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

    let target = ensure_gateway_ready_for_target(
        state_paths,
        args.listen,
        args.remote.as_deref(),
        args.api_key.clone(),
        requested_model,
        true,
    )
    .await?;

    let no_proxy =
        merge_endpoint_no_proxy(existing_no_proxy.as_deref(), Some(target.client.base_url()));

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
