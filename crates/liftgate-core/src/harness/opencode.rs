//! Interactive and terminal-window launcher for OpenCode (`lg oc`).

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use liftgate_harness::opencode::{
    OpenCodeInvocationKind, classify_opencode_invocation, has_negated_opencode_flag,
    merge_opencode_flag, parse_opencode_major_version, strip_opencode_flag,
};
use liftgate_server::platform::merge_loopback_no_proxy;
use serde_json::{Map, Value};
use tokio::process::Command;

use crate::cli::{OpencodeArgs, PLACEHOLDER_API_KEY, normalize_optional_api_key};
use crate::config_sync::{
    HarnessSyncParams, merged_opencode_config_into, parse_opencode_env_config_content,
    resolve_fast_model, strip_agy_provider_prefix,
};
use crate::daemon::ensure_gateway_ready;
use crate::error::CliError;
use crate::state::StatePaths;
use crate::terminal::TerminalLaunchSpec;

use super::{
    ResolvedCommand, command_from_terminal_spec, resolve_opencode_command,
    spawn_and_wait_interactive,
};

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

pub(super) async fn build_opencode_invocation(
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
    let existing_flags = std::env::var("OPENCODE_FLAGS").ok();
    let env_negates_full_cm = has_negated_opencode_flag(existing_flags.as_deref(), "full_codemode");
    let cfg_negates_full_cm = cfg_map
        .get("experimental")
        .and_then(Value::as_object)
        .and_then(|exp| exp.get("full_codemode"))
        .and_then(Value::as_bool)
        == Some(false);

    merged_opencode_config_into(&mut cfg_map, &sync_params, v2_runtime);
    if args.full_code_mode {
        set_experimental_full_codemode(&mut cfg_map, true);
    } else if args.mcp_code_mode || (!args.no_code_mode && env_negates_full_cm) {
        set_experimental_full_codemode(&mut cfg_map, false);
    }
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
    apply_opencode_runtime_env_flags(
        &mut env_set,
        args,
        v2_runtime,
        existing_flags.as_deref(),
        env_negates_full_cm || cfg_negates_full_cm,
        std::env::var_os("OPENCODE_EXPERIMENTAL_CODE_MODE").is_some(),
    );

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

fn set_experimental_full_codemode(cfg_map: &mut Map<String, Value>, enabled: bool) {
    let exp_entry = cfg_map
        .entry("experimental".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !exp_entry.is_object() {
        *exp_entry = Value::Object(Map::new());
    }
    if let Value::Object(exp_map) = exp_entry {
        exp_map.insert("full_codemode".to_owned(), Value::Bool(enabled));
    }
}

pub(super) fn apply_opencode_runtime_env_flags(
    env_set: &mut Vec<(String, String)>,
    args: &OpencodeArgs,
    v2_runtime: bool,
    existing_opencode_flags: Option<&str>,
    caller_opted_out_full_cm: bool,
    has_v1_code_mode_env: bool,
) {
    if args.no_code_mode || args.mcp_code_mode {
        if v2_runtime || existing_opencode_flags.is_some() {
            let flags = strip_opencode_flag(existing_opencode_flags, "full_codemode");
            env_set.push(("OPENCODE_FLAGS".to_owned(), flags));
        }
        if !v2_runtime && args.no_code_mode {
            env_set.push((
                "OPENCODE_EXPERIMENTAL_CODE_MODE".to_owned(),
                "false".to_owned(),
            ));
        }
        return;
    }

    if args.full_code_mode || (v2_runtime && !caller_opted_out_full_cm) {
        let flags = merge_opencode_flag(existing_opencode_flags, "full_codemode");
        env_set.push(("OPENCODE_FLAGS".to_owned(), flags));
    }
    if !v2_runtime && !has_v1_code_mode_env {
        env_set.push((
            "OPENCODE_EXPERIMENTAL_CODE_MODE".to_owned(),
            "true".to_owned(),
        ));
    }
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
