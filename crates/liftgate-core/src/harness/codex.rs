//! Interactive and terminal-window launcher for OpenAI Codex CLI (`lg cx`).

use std::ffi::OsString;
use std::path::PathBuf;

use liftgate_harness::args_util::{extract_explicit_model_arg, slice_before_double_dash};
use liftgate_harness::codex::{
    CODEX_STREAM_IDLE_TIMEOUT_KEY, CODEX_UPDATE_PLAN_ENABLED_KEY, CodexInvocationKind,
    classify_codex_invocation, has_explicit_codex_config_key,
    has_explicit_codex_update_plan_config, toml_inline_string,
};
use liftgate_harness::merge_endpoint_no_proxy;

use crate::cli::{CodexAppArgs, CodexArgs, PLACEHOLDER_API_KEY, normalize_optional_api_key};
use crate::config_sync::build_offline_codex_catalog_bytes_with_mode;
use crate::daemon::ensure_gateway_ready_for_provider_target;
use crate::error::CliError;
use crate::state::{EphemeralFileGuard, StatePaths};
use crate::terminal::TerminalLaunchSpec;

use super::{
    DEFAULT_REQUEST_TIMEOUT_MS, command_from_terminal_spec, resolve_codex_command,
    spawn_and_wait_interactive,
};

pub async fn run_codex(state_paths: &StatePaths, args: CodexArgs) -> Result<i32, CliError> {
    if args.app {
        let _ = crate::desktop::launch_codex_app(
            state_paths,
            CodexAppArgs {
                listen: args.listen,
                remote: args.remote,
                api_key: args.api_key,
                provider: args.provider,
                opt: args.opt,
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
    let exit_code = spawn_and_wait_interactive(state_paths, "codex", &spec.program, cmd).await?;
    drop(ephemeral_guard);
    Ok(exit_code)
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

pub(super) async fn build_codex_invocation(
    state_paths: &StatePaths,
    args: &CodexArgs,
    cwd: Option<PathBuf>,
    hold_open: bool,
) -> Result<(TerminalLaunchSpec, Option<EphemeralFileGuard>), CliError> {
    let resolved_cmd = resolve_codex_command(args.codex_bin.as_deref())?;
    let invocation = classify_codex_invocation(&args.passthrough_args);

    let existing_no_proxy = std::env::var("NO_PROXY")
        .ok()
        .or_else(|| std::env::var("no_proxy").ok());

    if matches!(
        invocation,
        CodexInvocationKind::DirectPassthrough | CodexInvocationKind::NativeUpdate
    ) {
        let no_proxy = merge_endpoint_no_proxy(existing_no_proxy.as_deref(), None);
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

    let target = ensure_gateway_ready_for_provider_target(
        state_paths,
        args.listen,
        args.remote.as_deref(),
        args.api_key.clone(),
        requested_model,
        args.provider.as_deref(),
        args.opt.as_deref(),
        true,
    )
    .await?;

    let scoped_base_url = target.scoped_base_url();
    let no_proxy =
        merge_endpoint_no_proxy(existing_no_proxy.as_deref(), Some(target.client.base_url()));

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
        .fetch_codex_catalog_json_for_route(
            target.route_spec.as_deref(),
            &primary_model,
            requested_tool_mode,
        )
        .await
    {
        Ok(bytes) => bytes,
        Err(_) => {
            let discovered = target
                .client
                .fetch_models_for_route(target.route_spec.as_deref())
                .await
                .unwrap_or_default();
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
    let toml_base_v1 = toml_inline_string(&format!("{scoped_base_url}/v1"));
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
    if !has_explicit_codex_update_plan_config(&args.passthrough_args) {
        push_cfg(
            &mut full_args,
            format!("{CODEX_UPDATE_PLAN_ENABLED_KEY}=true"),
        );
    }
    for remote_feature in [
        "features.apps",
        "features.remote_plugin",
        "features.recommended_plugins",
    ] {
        if !has_explicit_codex_config_key(&args.passthrough_args, remote_feature) {
            push_cfg(&mut full_args, format!("{remote_feature}=false"));
        }
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
