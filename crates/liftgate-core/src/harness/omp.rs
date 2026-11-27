//! Interactive and terminal-window launcher for Oh My Pi (`lg omp`).

use std::ffi::OsString;
use std::path::PathBuf;

use liftgate_harness::merge_endpoint_no_proxy;
use liftgate_harness::omp::{
    OmpInvocationKind, classify_omp_invocation, collect_existing_omp_extensions,
    merge_omp_config_files_env,
};

use crate::cli::{OmpArgs, PLACEHOLDER_API_KEY, normalize_optional_api_key};
use crate::config_sync::{
    HarnessSyncParams, OMP_EPHEMERAL_API_KEY_ENV, OmpCodeModePolicy,
    build_omp_ephemeral_overlay_yaml, build_omp_provider_extension_ts, resolve_fast_model,
    strip_agy_provider_prefix,
};
use crate::daemon::ensure_gateway_ready_for_target;
use crate::error::CliError;
use crate::state::{EphemeralFileGuard, StatePaths};
use crate::terminal::TerminalLaunchSpec;

use super::{command_from_terminal_spec, resolve_omp_command, spawn_and_wait_interactive};

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

pub(super) async fn build_omp_invocation(
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

    let target = ensure_gateway_ready_for_target(
        state_paths,
        args.listen,
        args.remote.as_deref(),
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
    let no_proxy = merge_endpoint_no_proxy(
        std::env::var("NO_PROXY")
            .ok()
            .or_else(|| std::env::var("no_proxy").ok())
            .as_deref(),
        Some(target.client.base_url()),
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
