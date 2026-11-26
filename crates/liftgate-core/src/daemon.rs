use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use clap::CommandFactory as _;
use liftgate_server::platform::{canonicalize_clean, split_expanded_path_var};
use liftgate_types::config::{AcpModeId, GatewayLimits, ModelId, ProxyUrl};
use liftgate_types::telemetry::GatewayTelemetrySnapshot;
use serde::Serialize;
use uuid::Uuid;

use crate::ReportTone;
use crate::agy_manager::{
    AgyAuthStatus, probe_agy_auth, probe_agy_version, quick_local_agy_summary,
};
use crate::cli::{
    AliasArgs, Cli, DoctorArgs, ModelsArgs, RestartArgs, ServeArgs, StatusArgs, UiArgs,
    normalize_optional_api_key,
};
use crate::client::{DiscoveredModel, GatewayClient, percent_encode_query_component};
use crate::config_sync::resolve_fast_model;
use crate::error::{CliError, DaemonStopError, GatewayTransportError};
use crate::harness::{
    resolve_claude_command, resolve_codex_command, resolve_omp_command, resolve_opencode_command,
};
use crate::serve::{ENV_SHUTDOWN_TOKEN, active_embedded_ui};
use crate::state::{
    DaemonState, StatePaths, now_unix_ms, resolve_effective_agy_auto_update,
    resolve_effective_agy_bin, resolve_effective_agy_proxy,
    resolve_effective_low_cache_capture_payload, resolve_effective_low_cache_threshold_bps,
    resolve_effective_max_low_cache_records, resolve_effective_mcp_tool_bridge,
    resolve_effective_vision_offload, write_file_atomic,
};

const MAX_LOG_TAIL_READ_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug)]
pub struct ActiveGatewayTarget {
    pub client: GatewayClient,
    pub dashboard_url: String,
    pub default_model: String,
    pub api_key: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DaemonStartOutcome {
    pub daemon: DaemonState,
    pub already_running: bool,
    pub config_matches: bool,
    pub live_model_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct StopOutcome {
    pub stopped_pid: Option<u32>,
}

pub async fn start_daemon(
    state_paths: &StatePaths,
    serve: ServeArgs,
    force_restart: bool,
) -> Result<DaemonStartOutcome, CliError> {
    start_daemon_with_exe(state_paths, None, serve, force_restart).await
}

pub async fn start_daemon_with_exe(
    state_paths: &StatePaths,
    explicit_exe: Option<&Path>,
    serve: ServeArgs,
    force_restart: bool,
) -> Result<DaemonStartOutcome, CliError> {
    let settings = state_paths.read_settings()?;
    let effective_proxy = resolve_effective_agy_proxy(serve.agy_proxy.as_deref(), &settings)?;
    let effective_auto_update = resolve_effective_agy_auto_update(
        serve.agy_auto_update,
        serve.no_agy_auto_update,
        &settings,
    );
    let effective_vision_offload =
        resolve_effective_vision_offload(serve.vision_offload, serve.no_vision_offload, &settings);
    let effective_mcp_tool_bridge = resolve_effective_mcp_tool_bridge(
        serve.mcp_tool_bridge,
        serve.no_mcp_tool_bridge,
        &settings,
    );
    let effective_max_low_cache_records =
        resolve_effective_max_low_cache_records(serve.max_low_cache_records, &settings)?;
    let effective_low_cache_threshold_bps =
        resolve_effective_low_cache_threshold_bps(serve.low_cache_threshold_pct, &settings)?;
    let effective_low_cache_capture_payload = resolve_effective_low_cache_capture_payload(
        serve.low_cache_capture_payload,
        serve.no_low_cache_capture_payload,
        &settings,
    );
    let _ = ModelId::try_new(&serve.default_model)?;
    let _ = AcpModeId::try_new(&serve.acp_mode)?;
    let _ = GatewayLimits::try_new(
        serve.max_line_bytes,
        serve.max_body_bytes,
        serve.channel_capacity,
        serve.max_idle_sessions,
        serve.max_stored_responses,
        serve.turn_timeout_secs,
    )?;

    let existing_daemon = state_paths.read_daemon()?;
    if !force_restart
        && let Some(existing) = &existing_daemon
        && is_pid_alive(existing.pid)
    {
        let existing_client = GatewayClient::new(existing.listen, existing.api_key.clone());
        if existing_client
            .check_health(Duration::from_millis(800))
            .await
        {
            let requested_key = normalize_optional_api_key(serve.api_key.as_deref());
            let config_matches = existing.listen == serve.listen
                && existing.ui_listen == serve.ui_listen
                && existing.default_model == serve.default_model
                && existing.acp_mode == serve.acp_mode
                && existing.api_key == requested_key
                && existing.agy_proxy.as_deref() == effective_proxy.as_ref().map(ProxyUrl::as_str)
                && existing.agy_auto_update == effective_auto_update
                && existing.vision_offload == effective_vision_offload
                && existing.mcp_tool_bridge == effective_mcp_tool_bridge
                && existing.max_low_cache_records == effective_max_low_cache_records
                && existing.low_cache_threshold_bps == effective_low_cache_threshold_bps
                && existing.capture_low_cache_request_details
                    == effective_low_cache_capture_payload
                && existing.max_line_bytes == serve.max_line_bytes
                && existing.max_body_bytes == serve.max_body_bytes
                && existing.channel_capacity == serve.channel_capacity
                && existing.max_idle_sessions == serve.max_idle_sessions
                && existing.max_stored_responses == serve.max_stored_responses
                && existing.turn_timeout_secs == serve.turn_timeout_secs;
            let live_model_count = existing_client.fetch_models().await.map_or(0, |m| m.len());
            return Ok(DaemonStartOutcome {
                daemon: existing.clone(),
                already_running: true,
                config_matches,
                live_model_count,
            });
        }
    }

    let resolved_agy = resolve_effective_agy_bin(
        serve
            .agy_bin
            .clone()
            .or_else(|| existing_daemon.as_ref().and_then(|d| d.agy_bin.clone())),
        &settings,
    )
    .ok_or(CliError::AgyBinaryNotFound)?;

    let existing_owns_target_listen = existing_daemon
        .as_ref()
        .is_some_and(|d| is_pid_alive(d.pid) && d.listen == serve.listen);
    if !existing_owns_target_listen {
        let pre_probe = GatewayClient::new(serve.listen, serve.api_key.clone());
        if pre_probe.check_health(Duration::from_millis(500)).await {
            return Err(CliError::PortOccupiedUnmanaged { addr: serve.listen });
        }
    }

    if let Some(existing) = existing_daemon {
        if is_pid_alive(existing.pid) {
            let _ = stop_daemon(state_paths).await?;
        } else {
            state_paths.remove_daemon()?;
        }
    }

    let probe = GatewayClient::new(serve.listen, serve.api_key.clone());
    if probe.check_health(Duration::from_millis(400)).await {
        return Err(CliError::PortOccupiedUnmanaged { addr: serve.listen });
    }

    let log_out = state_paths.prepare_log_file_for_spawn()?;
    let log_err = log_out.try_clone().map_err(|source| CliError::StateIo {
        path: state_paths.log_file(),
        source,
    })?;

    let exe_path = match explicit_exe {
        Some(path) => path.to_path_buf(),
        None => std::env::current_exe().map_err(|source| CliError::DaemonSpawn { source })?,
    };
    let shutdown_token = Uuid::new_v4().to_string();
    let clean_api_key = normalize_optional_api_key(serve.api_key.as_deref());

    let mut cmd = Command::new(&exe_path);
    cmd.arg("--state-dir")
        .arg(state_paths.root())
        .arg("serve")
        .arg("--listen")
        .arg(serve.listen.to_string())
        .arg("--agy-bin")
        .arg(&resolved_agy)
        .arg("--agy-proxy")
        .arg(effective_proxy.as_ref().map_or("off", ProxyUrl::as_str))
        .arg("--agy-auto-update")
        .arg(if effective_auto_update {
            "true"
        } else {
            "false"
        })
        .arg("--vision-offload")
        .arg(if effective_vision_offload {
            "true"
        } else {
            "false"
        })
        .arg("--mcp-tool-bridge")
        .arg(if effective_mcp_tool_bridge {
            "true"
        } else {
            "false"
        })
        .arg("--max-low-cache-records")
        .arg(effective_max_low_cache_records.get().to_string())
        .arg("--low-cache-threshold-pct")
        .arg((effective_low_cache_threshold_bps.get() / 100).to_string())
        .arg("--low-cache-capture-payload")
        .arg(if effective_low_cache_capture_payload {
            "true"
        } else {
            "false"
        })
        .arg("--default-model")
        .arg(&serve.default_model)
        .arg("--acp-mode")
        .arg(&serve.acp_mode)
        .arg("--max-line-bytes")
        .arg(serve.max_line_bytes.to_string())
        .arg("--max-body-bytes")
        .arg(serve.max_body_bytes.to_string())
        .arg("--channel-capacity")
        .arg(serve.channel_capacity.to_string())
        .arg("--max-idle-sessions")
        .arg(serve.max_idle_sessions.to_string())
        .arg("--max-stored-responses")
        .arg(serve.max_stored_responses.to_string())
        .arg("--turn-timeout-secs")
        .arg(serve.turn_timeout_secs.to_string());

    if let Some(ui_addr) = serve.ui_listen {
        cmd.arg("--ui-listen").arg(ui_addr.to_string());
    }
    if let Some(cwd) = &serve.acp_cwd {
        cmd.arg("--acp-cwd").arg(cwd);
    }
    if let Some(ui_dir) = &serve.ui_dir {
        cmd.arg("--ui-dir").arg(ui_dir);
    }
    if let Some(key) = &clean_api_key {
        cmd.env("AGY_GATEWAY_API_KEY", key);
    } else {
        cmd.env_remove("AGY_GATEWAY_API_KEY");
    }
    cmd.env(ENV_SHUTDOWN_TOKEN, &shutdown_token)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err));

    configure_detached_command(&mut cmd);

    let mut child = {
        #[cfg(windows)]
        let _stdio_guard = StdioInheritGuard::suspend_current();
        cmd.spawn()
            .map_err(|source| CliError::DaemonSpawn { source })?
    };
    let pid = child.id();

    let client = GatewayClient::new(serve.listen, clean_api_key.clone());
    let mut ready = false;
    for _ in 0..50 {
        if let Ok(Some(exit_status)) = child.try_wait() {
            let log_tail = tail_file_lines(&state_paths.log_file(), 25).unwrap_or_default();
            return Err(CliError::DaemonExitedEarly {
                code: exit_status.code(),
                log_tail,
            });
        }
        if client.check_health(Duration::from_millis(500)).await {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    if !ready {
        let _ = terminate_pid_fallback(pid);
        let _ = force_kill_pid_if_needed(pid);
        return Err(CliError::DaemonHealthTimeout {
            url: format!("{}/healthz", client.base_url()),
            log_path: state_paths.log_file(),
        });
    }

    let state = DaemonState {
        pid,
        listen: serve.listen,
        ui_listen: serve.ui_listen,
        default_model: serve.default_model,
        acp_mode: serve.acp_mode,
        api_key: clean_api_key,
        agy_bin: Some(resolved_agy),
        acp_cwd: serve.acp_cwd,
        ui_dir: serve.ui_dir,
        agy_proxy: effective_proxy.as_ref().map(|p| p.as_str().to_owned()),
        agy_auto_update: effective_auto_update,
        vision_offload: effective_vision_offload,
        mcp_tool_bridge: effective_mcp_tool_bridge,
        max_low_cache_records: effective_max_low_cache_records,
        low_cache_threshold_bps: effective_low_cache_threshold_bps,
        capture_low_cache_request_details: effective_low_cache_capture_payload,
        max_line_bytes: serve.max_line_bytes,
        max_body_bytes: serve.max_body_bytes,
        channel_capacity: serve.channel_capacity,
        max_idle_sessions: serve.max_idle_sessions,
        max_stored_responses: serve.max_stored_responses,
        turn_timeout_secs: serve.turn_timeout_secs,
        shutdown_token,
        started_at_unix_ms: now_unix_ms(),
        log_file: state_paths.log_file(),
    };
    state_paths.write_daemon(&state)?;

    let live_model_count = client.fetch_models().await.map_or(0, |m| m.len());
    Ok(DaemonStartOutcome {
        daemon: state,
        already_running: false,
        config_matches: true,
        live_model_count,
    })
}

pub async fn stop_daemon(state_paths: &StatePaths) -> Result<StopOutcome, CliError> {
    let Some(daemon) = state_paths.read_daemon()? else {
        return Ok(StopOutcome { stopped_pid: None });
    };

    let pid = daemon.pid;
    if is_pid_alive(pid) {
        let client = GatewayClient::new(daemon.listen, daemon.api_key.clone());
        let mut graceful_sent = false;
        if !daemon.shutdown_token.is_empty()
            && client
                .request_shutdown(&daemon.shutdown_token)
                .await
                .is_ok()
        {
            graceful_sent = true;
        }

        if graceful_sent {
            for _ in 0..50 {
                if !is_pid_alive(pid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        if is_pid_alive(pid) && is_liftgate_process(pid) {
            terminate_pid_fallback(pid)?;
            for _ in 0..20 {
                if !is_pid_alive(pid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if is_pid_alive(pid) {
                let _ = force_kill_pid_if_needed(pid);
                for _ in 0..10 {
                    if !is_pid_alive(pid) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            if is_pid_alive(pid) {
                return Err(CliError::DaemonStopFailed {
                    pid,
                    source: DaemonStopError::StillAliveAfterSignal,
                });
            }
        }
    }

    state_paths.remove_daemon()?;
    Ok(StopOutcome {
        stopped_pid: Some(pid),
    })
}

pub fn serve_args_from_daemon_state(old: &DaemonState) -> ServeArgs {
    let defaults = ServeArgs::default();
    ServeArgs {
        listen: old.listen,
        ui_listen: old.ui_listen,
        default_model: if old.default_model.is_empty() {
            defaults.default_model
        } else {
            old.default_model.clone()
        },
        acp_mode: if old.acp_mode.is_empty() {
            defaults.acp_mode
        } else {
            old.acp_mode.clone()
        },
        api_key: old.api_key.clone(),
        agy_bin: old.agy_bin.clone(),
        acp_cwd: old.acp_cwd.clone(),
        ui_dir: old.ui_dir.clone(),
        vision_offload: Some(old.vision_offload),
        mcp_tool_bridge: Some(old.mcp_tool_bridge),
        max_low_cache_records: Some(old.max_low_cache_records.get()),
        low_cache_threshold_pct: Some(
            u8::try_from(old.low_cache_threshold_bps.get() / 100).unwrap_or(50),
        ),
        low_cache_capture_payload: Some(old.capture_low_cache_request_details),
        max_line_bytes: old.max_line_bytes,
        max_body_bytes: old.max_body_bytes,
        channel_capacity: old.channel_capacity,
        max_idle_sessions: old.max_idle_sessions,
        max_stored_responses: old.max_stored_responses,
        turn_timeout_secs: old.turn_timeout_secs,
        ..defaults
    }
}

pub async fn restart_daemon(
    state_paths: &StatePaths,
    args: RestartArgs,
) -> Result<DaemonStartOutcome, CliError> {
    let prev = state_paths.read_daemon()?;
    let mut serve = prev
        .as_ref()
        .map_or_else(ServeArgs::default, serve_args_from_daemon_state);

    if let Some(listen) = args.listen {
        serve.listen = listen;
    }
    if let Some(ui_listen) = args.ui_listen {
        serve.ui_listen = Some(ui_listen);
    }
    if let Some(model) = args.default_model {
        serve.default_model = model;
    }
    if let Some(mode) = args.acp_mode {
        serve.acp_mode = mode;
    }
    if let Some(key) = args.api_key {
        serve.api_key = Some(key);
    }
    if let Some(bin) = args.agy_bin {
        serve.agy_bin = Some(bin);
    }
    if let Some(px) = args.agy_proxy {
        serve.agy_proxy = Some(px);
    }
    if let Some(au) = args.agy_auto_update {
        serve.agy_auto_update = Some(au);
    }
    if args.no_agy_auto_update {
        serve.no_agy_auto_update = true;
    }
    if let Some(vo) = args.vision_offload {
        serve.vision_offload = Some(vo);
        serve.no_vision_offload = false;
    }
    if args.no_vision_offload {
        serve.vision_offload = None;
        serve.no_vision_offload = true;
    }
    if let Some(mb) = args.mcp_tool_bridge {
        serve.mcp_tool_bridge = Some(mb);
        serve.no_mcp_tool_bridge = false;
    }
    if args.no_mcp_tool_bridge {
        serve.mcp_tool_bridge = None;
        serve.no_mcp_tool_bridge = true;
    }
    if let Some(cap) = args.max_low_cache_records {
        serve.max_low_cache_records = Some(cap);
    }
    if let Some(pct) = args.low_cache_threshold_pct {
        serve.low_cache_threshold_pct = Some(pct);
    }
    if let Some(cp) = args.low_cache_capture_payload {
        serve.low_cache_capture_payload = Some(cp);
        serve.no_low_cache_capture_payload = false;
    }
    if args.no_low_cache_capture_payload {
        serve.low_cache_capture_payload = None;
        serve.no_low_cache_capture_payload = true;
    }

    let outcome = start_daemon(state_paths, serve, true).await?;
    if args.open {
        let url = build_authenticated_dashboard_url(
            &outcome.daemon.dashboard_base_url(),
            &outcome.daemon.connect_base_url(),
            outcome.daemon.api_key.as_deref(),
        );
        open_browser(&url)?;
    }
    Ok(outcome)
}

pub fn resolve_client_api_key(
    explicit_cli_key: Option<&str>,
    managed_daemon: Option<&DaemonState>,
    target_listen: SocketAddr,
) -> Option<String> {
    if let Some(explicit) = normalize_optional_api_key(explicit_cli_key) {
        return Some(explicit);
    }
    if let Some(d) = managed_daemon
        && d.listen == target_listen
        && is_pid_alive(d.pid)
    {
        return normalize_optional_api_key(d.api_key.as_deref());
    }
    normalize_optional_api_key(std::env::var("AGY_GATEWAY_API_KEY").ok().as_deref())
}

pub async fn ensure_gateway_ready(
    state_paths: &StatePaths,
    explicit_listen: Option<SocketAddr>,
    explicit_api_key: Option<String>,
    preferred_model: Option<&str>,
    auto_start: bool,
) -> Result<ActiveGatewayTarget, CliError> {
    let daemon = state_paths.read_daemon()?;
    if let Some(addr) = explicit_listen {
        let key = resolve_client_api_key(explicit_api_key.as_deref(), daemon.as_ref(), addr);
        let client = GatewayClient::new(addr, key.clone());
        if client.check_health(Duration::from_millis(800)).await {
            let matching_daemon = daemon
                .as_ref()
                .filter(|d| d.listen == addr && is_pid_alive(d.pid));
            let dash_url = matching_daemon.map_or_else(
                || format!("{}/", client.base_url()),
                |d| format!("{}/", d.dashboard_base_url()),
            );
            let model_fallback = matching_daemon
                .map(|d| d.default_model.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("gemini-4-carbon-high");
            return Ok(ActiveGatewayTarget {
                dashboard_url: dash_url,
                default_model: preferred_model.unwrap_or(model_fallback).to_owned(),
                api_key: key,
                client,
            });
        }
        let conflicting_managed_daemon = daemon
            .as_ref()
            .is_some_and(|d| d.listen != addr && is_pid_alive(d.pid));
        if !auto_start || conflicting_managed_daemon {
            return Err(CliError::GatewayUnreachable {
                url: format!("{}/healthz", client.base_url()),
                source: GatewayTransportError::NotRunning,
            });
        }
        let mut serve = ServeArgs {
            listen: addr,
            api_key: key,
            ..ServeArgs::default()
        };
        if let Some(m) = preferred_model.filter(|s| !s.trim().is_empty()) {
            serve.default_model = m.to_owned();
        }
        let daemon = start_daemon(state_paths, serve, false).await?.daemon;
        let client = GatewayClient::new(daemon.listen, daemon.api_key.clone());
        return Ok(ActiveGatewayTarget {
            dashboard_url: format!("{}/", daemon.dashboard_base_url()),
            default_model: daemon.default_model,
            api_key: daemon.api_key,
            client,
        });
    }

    if let Some(daemon) = daemon {
        let key = resolve_client_api_key(explicit_api_key.as_deref(), Some(&daemon), daemon.listen);
        let client = GatewayClient::new(daemon.listen, key.clone());
        if is_pid_alive(daemon.pid) && client.check_health(Duration::from_millis(800)).await {
            return Ok(ActiveGatewayTarget {
                dashboard_url: format!("{}/", daemon.dashboard_base_url()),
                default_model: if daemon.default_model.is_empty() {
                    "gemini-4-carbon-high".to_owned()
                } else {
                    daemon.default_model
                },
                api_key: key,
                client,
            });
        }
    }

    let default_serve = ServeArgs::default();
    let fallback_key = normalize_optional_api_key(explicit_api_key.as_deref())
        .or_else(|| default_serve.api_key.clone());
    let fallback_client = GatewayClient::new(default_serve.listen, fallback_key.clone());
    if fallback_client
        .check_health(Duration::from_millis(500))
        .await
    {
        return Ok(ActiveGatewayTarget {
            dashboard_url: format!("{}/", fallback_client.base_url()),
            default_model: preferred_model
                .unwrap_or(&default_serve.default_model)
                .to_owned(),
            api_key: fallback_key,
            client: fallback_client,
        });
    }

    if !auto_start {
        return Err(CliError::GatewayUnreachable {
            url: format!("{}/healthz", fallback_client.base_url()),
            source: GatewayTransportError::NotRunning,
        });
    }

    let mut serve = default_serve;
    if let Some(key) = explicit_api_key {
        serve.api_key = Some(key);
    }
    if let Some(m) = preferred_model.filter(|s| !s.trim().is_empty()) {
        serve.default_model = m.to_owned();
    }
    let daemon = start_daemon(state_paths, serve, false).await?.daemon;
    let client = GatewayClient::new(daemon.listen, daemon.api_key.clone());
    Ok(ActiveGatewayTarget {
        dashboard_url: format!("{}/", daemon.dashboard_base_url()),
        default_model: daemon.default_model,
        api_key: daemon.api_key,
        client,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayStatusMode {
    Managed,
    External,
    Offline,
}

impl GatewayStatusMode {
    #[must_use]
    pub const fn status_label(self) -> &'static str {
        match self {
            Self::Managed => "running (managed)",
            Self::External => "running (external)",
            Self::Offline => "stopped",
        }
    }

    #[must_use]
    pub const fn pill_text(self) -> &'static str {
        match self {
            Self::Managed => "ONLINE · MANAGED",
            Self::External => "ONLINE · EXTERNAL",
            Self::Offline => "OFFLINE",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct GatewayStatusReport {
    pub mode: GatewayStatusMode,
    pub healthy: bool,
    pub managed_daemon: Option<DaemonState>,
    pub pid: Option<u32>,
    pub listen: SocketAddr,
    pub gateway_url: String,
    pub dashboard_url: String,
    pub authenticated_dashboard_url: String,
    pub auth_enabled: bool,
    pub state_dir: PathBuf,
    pub log_file: PathBuf,
    pub models: Option<Vec<DiscoveredModel>>,
    pub agy_binary_path: Option<PathBuf>,
    pub agy_version: Option<String>,
    pub agy_proxy_display: Option<String>,
    pub agy_auto_update_enabled: bool,
    pub vision_offload_enabled: bool,
    pub mcp_tool_bridge_enabled: bool,
    pub telemetry: Option<GatewayTelemetrySnapshot>,
}

pub async fn inspect_gateway_status(
    state_paths: &StatePaths,
    args: &StatusArgs,
) -> Result<GatewayStatusReport, CliError> {
    let daemon = state_paths.read_daemon()?;
    let listen = args
        .listen
        .or_else(|| daemon.as_ref().map(|d| d.listen))
        .unwrap_or_else(|| ServeArgs::default().listen);
    let api_key = resolve_client_api_key(args.api_key.as_deref(), daemon.as_ref(), listen);

    let client = GatewayClient::new(listen, api_key.clone());
    let healthy = client.check_health(Duration::from_millis(800)).await;
    let managed_alive = daemon.as_ref().is_some_and(|d| is_pid_alive(d.pid));
    let telemetry = if healthy {
        client.fetch_telemetry().await.ok()
    } else {
        None
    };
    let models = if healthy {
        client.fetch_models().await.ok()
    } else {
        None
    };

    let mode = if healthy && managed_alive {
        GatewayStatusMode::Managed
    } else if healthy {
        GatewayStatusMode::External
    } else {
        GatewayStatusMode::Offline
    };

    let dashboard_url = daemon.as_ref().map_or_else(
        || format!("{}/", client.base_url()),
        |d| format!("{}/", d.dashboard_base_url()),
    );
    let authenticated_dashboard_url = build_authenticated_dashboard_url(
        dashboard_url.trim_end_matches('/'),
        client.base_url(),
        api_key.as_deref(),
    );
    let local_agy = quick_local_agy_summary(state_paths, None).await;
    let effective_version = telemetry
        .as_ref()
        .and_then(|t| t.acp_process.agent_version.clone())
        .or_else(|| local_agy.version.clone());
    let effective_proxy_display = telemetry
        .as_ref()
        .and_then(|t| t.acp_process.agy_proxy.clone())
        .or_else(|| {
            local_agy
                .proxy
                .as_ref()
                .map(|p| p.redacted_display().into_owned())
        });
    let effective_auto_update = telemetry
        .as_ref()
        .map_or(local_agy.auto_update_enabled, |t| {
            t.acp_process.auto_update_enabled
        });
    let settings = state_paths.read_settings().unwrap_or_default();
    let effective_vision_offload = telemetry.as_ref().map_or(
        daemon
            .as_ref()
            .filter(|_| managed_alive)
            .map_or(settings.vision_offload, |d| d.vision_offload),
        |t| t.vision_offload_enabled,
    );
    let effective_mcp_tool_bridge = telemetry.as_ref().map_or(
        daemon
            .as_ref()
            .filter(|_| managed_alive)
            .map_or(settings.mcp_tool_bridge, |d| d.mcp_tool_bridge),
        |t| t.mcp_tool_bridge_enabled,
    );

    let active_daemon = daemon.filter(|_| managed_alive);
    let pid = active_daemon.as_ref().map(|d| d.pid);
    Ok(GatewayStatusReport {
        mode,
        healthy,
        managed_daemon: active_daemon,
        pid,
        listen,
        gateway_url: client.base_url().to_owned(),
        dashboard_url,
        authenticated_dashboard_url,
        auth_enabled: api_key.is_some(),
        state_dir: state_paths.root().to_path_buf(),
        log_file: state_paths.log_file(),
        models,
        agy_binary_path: local_agy.binary_path,
        agy_version: effective_version,
        agy_proxy_display: effective_proxy_display,
        agy_auto_update_enabled: effective_auto_update,
        vision_offload_enabled: effective_vision_offload,
        mcp_tool_bridge_enabled: effective_mcp_tool_bridge,
        telemetry,
    })
}

pub fn clear_gateway_log(state_paths: &StatePaths) -> Result<PathBuf, CliError> {
    let log_path = state_paths.log_file();
    if log_path.exists() {
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&log_path)
            .map_err(|source| CliError::StateIo {
                path: log_path.clone(),
                source,
            })?;
    }
    Ok(log_path)
}

pub fn read_log_tail(path: &Path, max_lines: usize) -> Result<String, CliError> {
    if !path.is_file() {
        return Ok(String::new());
    }
    tail_file_lines(path, max_lines)
}

#[derive(Clone, Debug, Serialize)]
pub struct ResolvedUiTarget {
    pub authenticated_url: String,
    pub display_url: String,
}

pub async fn resolve_ui_target(
    state_paths: &StatePaths,
    args: &UiArgs,
) -> Result<ResolvedUiTarget, CliError> {
    let target = ensure_gateway_ready(
        state_paths,
        args.listen,
        args.api_key.clone(),
        None,
        !args.no_start,
    )
    .await?;

    let authenticated_url = build_authenticated_dashboard_url(
        target.dashboard_url.trim_end_matches('/'),
        target.client.base_url(),
        target.api_key.as_deref(),
    );
    Ok(ResolvedUiTarget {
        authenticated_url,
        display_url: target.dashboard_url,
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelsReport {
    pub gateway_url: String,
    pub default_model: String,
    pub fast_model: String,
    pub title_model: String,
    pub models: Vec<DiscoveredModel>,
}

/// Validates and atomically persists the session title generation model (`~/.liftgate/title-model`),
/// taking effect on the very next title request without restarting active sessions.
pub fn set_title_model(state_paths: &StatePaths, model_id: &str) -> Result<ModelId, CliError> {
    let validated = ModelId::try_new(model_id)?;
    write_file_atomic(
        &state_paths.title_model_file(),
        validated.as_str().as_bytes(),
        false,
    )?;
    Ok(validated)
}

pub async fn inspect_models(
    state_paths: &StatePaths,
    args: &ModelsArgs,
) -> Result<ModelsReport, CliError> {
    let target = ensure_gateway_ready(
        state_paths,
        args.listen,
        args.api_key.clone(),
        None,
        !args.no_start,
    )
    .await?;
    let models: Vec<DiscoveredModel> = target.client.fetch_models().await?;
    let fast_model = resolve_fast_model(None, &target.default_model, &models);
    let title_model =
        liftgate_server::title_model::title_model(Some(&state_paths.title_model_file()))
            .await
            .as_str()
            .to_owned();
    Ok(ModelsReport {
        gateway_url: target.client.base_url().to_owned(),
        default_model: target.default_model,
        fast_model,
        title_model,
        models,
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorCheckItem {
    pub name: &'static str,
    pub status: &'static str,
    pub tone: ReportTone,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub warnings: bool,
    pub agy_missing: bool,
    pub login_needed: bool,
    pub checks: Vec<DoctorCheckItem>,
}

pub async fn inspect_doctor(
    state_paths: &StatePaths,
    args: &DoctorArgs,
    bin_hint: &str,
) -> Result<DoctorReport, CliError> {
    let settings = state_paths.read_settings()?;
    let env_proxy = std::env::var("AGY_PROXY").ok();
    let effective_proxy = resolve_effective_agy_proxy(env_proxy.as_deref(), &settings)?;
    let auto_update_enabled = settings.agy_auto_update;
    let mut checks: Vec<DoctorCheckItem> = Vec::with_capacity(12);
    let mut has_error = false;
    let mut has_warn = false;
    let mut agy_missing = false;
    let mut login_needed = false;

    checks.push(DoctorCheckItem {
        name: "State Directory",
        status: "ready",
        tone: ReportTone::Ok,
        detail: state_paths.root().display().to_string(),
    });

    let resolved_agy = resolve_effective_agy_bin(args.agy_bin.clone(), &settings);
    match &resolved_agy {
        Some(agy) => {
            let ver = probe_agy_version(agy, effective_proxy.as_ref()).await;
            let detail = match ver {
                Some(v) => format!("v{v} ({})", agy.display()),
                None => agy.display().to_string(),
            };
            checks.push(DoctorCheckItem {
                name: "Antigravity CLI",
                status: "found",
                tone: ReportTone::Ok,
                detail,
            });
        }
        None => {
            has_error = true;
            agy_missing = true;
            checks.push(DoctorCheckItem {
                name: "Antigravity CLI",
                status: "missing",
                tone: ReportTone::Err,
                detail: format!(
                    "NOT FOUND — run `{bin_hint} agy install` to download & verify official agy CLI"
                ),
            });
        }
    }

    match &effective_proxy {
        Some(proxy) => checks.push(DoctorCheckItem {
            name: "AGY Proxy",
            status: "forced",
            tone: ReportTone::Info,
            detail: format!(
                "{} (forcibly injected into agy; 127.0.0.1 bypassed via NO_PROXY)",
                proxy.redacted_display()
            ),
        }),
        None => checks.push(DoctorCheckItem {
            name: "AGY Proxy",
            status: "direct",
            tone: ReportTone::Ok,
            detail: format!(
                "none (default: no proxy injected; configure via `{bin_hint} proxy <port|url>`)"
            ),
        }),
    }

    checks.push(DoctorCheckItem {
        name: "AGY Auto-Update",
        status: if auto_update_enabled {
            "enabled"
        } else {
            "disabled"
        },
        tone: if auto_update_enabled {
            ReportTone::Ok
        } else {
            ReportTone::Muted
        },
        detail: if auto_update_enabled {
            "checks official manifest & hot-swaps agy when gateway is idle".to_owned()
        } else {
            format!("disabled (enable via `{bin_hint} agy auto-update on`)")
        },
    });

    let (ui_assets, full_ui_embedded) = active_embedded_ui();
    checks.push(DoctorCheckItem {
        name: "Embedded Web UI",
        status: if full_ui_embedded {
            "bundled"
        } else {
            "fallback"
        },
        tone: ReportTone::Ok,
        detail: if full_ui_embedded {
            format!("Production React dashboard ({} assets)", ui_assets.len())
        } else {
            "Lightweight SSE telemetry dashboard".to_owned()
        },
    });

    match resolve_claude_command(None) {
        Ok(cmd) => checks.push(DoctorCheckItem {
            name: "Claude Code CLI",
            status: "found",
            tone: ReportTone::Ok,
            detail: cmd.program.display().to_string(),
        }),
        Err(_) => checks.push(DoctorCheckItem {
            name: "Claude Code CLI",
            status: "optional",
            tone: ReportTone::Muted,
            detail: "not detected in PATH (`npm i -g @anthropic-ai/claude-code`)".to_owned(),
        }),
    }
    match resolve_codex_command(None) {
        Ok(cmd) => checks.push(DoctorCheckItem {
            name: "Codex CLI",
            status: "found",
            tone: ReportTone::Ok,
            detail: cmd.program.display().to_string(),
        }),
        Err(_) => checks.push(DoctorCheckItem {
            name: "Codex CLI",
            status: "optional",
            tone: ReportTone::Muted,
            detail: "not detected in PATH (`npm i -g @openai/codex`)".to_owned(),
        }),
    }
    match resolve_opencode_command(None) {
        Ok(cmd) => checks.push(DoctorCheckItem {
            name: "OpenCode CLI",
            status: "found",
            tone: ReportTone::Ok,
            detail: cmd.program.display().to_string(),
        }),
        Err(_) => checks.push(DoctorCheckItem {
            name: "OpenCode CLI",
            status: "optional",
            tone: ReportTone::Muted,
            detail: "not detected in PATH (`npm i -g opencode-ai`)".to_owned(),
        }),
    }
    match resolve_omp_command(None) {
        Ok(cmd) => checks.push(DoctorCheckItem {
            name: "Oh My Pi (omp)",
            status: "found",
            tone: ReportTone::Ok,
            detail: cmd.program.display().to_string(),
        }),
        Err(_) => checks.push(DoctorCheckItem {
            name: "Oh My Pi (omp)",
            status: "optional",
            tone: ReportTone::Muted,
            detail: "not detected in PATH (`bun i -g @oh-my-pi/pi-coding-agent`)".to_owned(),
        }),
    }
    match crate::desktop::resolve_claude_desktop(None) {
        Ok(app) => checks.push(DoctorCheckItem {
            name: "Claude Desktop",
            status: "found",
            tone: ReportTone::Ok,
            detail: app.display_label().to_owned(),
        }),
        Err(_) => checks.push(DoctorCheckItem {
            name: "Claude Desktop",
            status: "optional",
            tone: ReportTone::Muted,
            detail: format!("not detected (`{bin_hint} cd` when installed)"),
        }),
    }
    match crate::desktop::resolve_codex_app(None) {
        Ok(app) => checks.push(DoctorCheckItem {
            name: "Codex App",
            status: "found",
            tone: ReportTone::Ok,
            detail: app.display_label().to_owned(),
        }),
        Err(_) => checks.push(DoctorCheckItem {
            name: "Codex App",
            status: "optional",
            tone: ReportTone::Muted,
            detail: format!("not detected (`{bin_hint} cxa` when installed)"),
        }),
    }

    let daemon = state_paths.read_daemon()?;
    let listen = args
        .listen
        .or_else(|| daemon.as_ref().map(|d| d.listen))
        .unwrap_or_else(|| ServeArgs::default().listen);
    let api_key = resolve_client_api_key(args.api_key.as_deref(), daemon.as_ref(), listen);
    let client = GatewayClient::new(listen, api_key);

    if client.check_health(Duration::from_millis(800)).await {
        checks.push(DoctorCheckItem {
            name: "Gateway Service",
            status: "online",
            tone: ReportTone::Ok,
            detail: format!("{}/", client.base_url()),
        });
        match client.fetch_models().await {
            Ok(models) if models.len() > 1 => {
                checks.push(DoctorCheckItem {
                    name: "ACP Auth/Models",
                    status: "ready",
                    tone: ReportTone::Ok,
                    detail: format!("{} live Antigravity models available", models.len()),
                });
            }
            Ok(_) => {
                has_warn = true;
                login_needed = true;
                checks.push(DoctorCheckItem {
                    name: "ACP Auth/Models",
                    status: "login needed",
                    tone: ReportTone::Warn,
                    detail: format!("only fallback model returned; run `{bin_hint} agy login`"),
                });
            }
            Err(err) => {
                has_error = true;
                checks.push(DoctorCheckItem {
                    name: "ACP Auth/Models",
                    status: "error",
                    tone: ReportTone::Err,
                    detail: err.display_chain(),
                });
            }
        }
    } else {
        has_warn = true;
        checks.push(DoctorCheckItem {
            name: "Gateway Service",
            status: "offline",
            tone: ReportTone::Warn,
            detail: format!(
                "not listening at {}/ (start with `{bin_hint} up`)",
                client.base_url()
            ),
        });
        if let Some(agy) = &resolved_agy {
            match probe_agy_auth(state_paths, agy, effective_proxy.as_ref()).await {
                AgyAuthStatus::SignedIn {
                    model_count,
                    source,
                } => {
                    checks.push(DoctorCheckItem {
                        name: "ACP Auth/Models",
                        status: "ready",
                        tone: ReportTone::Ok,
                        detail: format!(
                            "signed in ({model_count} live Antigravity models via {source})"
                        ),
                    });
                }
                AgyAuthStatus::NotSignedIn { detail } => {
                    has_warn = true;
                    login_needed = true;
                    checks.push(DoctorCheckItem {
                        name: "ACP Auth/Models",
                        status: "login needed",
                        tone: ReportTone::Warn,
                        detail: format!("{detail} (run `{bin_hint} agy login`)"),
                    });
                }
                AgyAuthStatus::NetworkError { detail } => {
                    has_warn = true;
                    checks.push(DoctorCheckItem {
                        name: "ACP Auth/Models",
                        status: "check failed",
                        tone: ReportTone::Warn,
                        detail: format!(
                            "{detail} (verify `{bin_hint} proxy` or run `{bin_hint} agy login`)"
                        ),
                    });
                }
                AgyAuthStatus::NotInstalled | AgyAuthStatus::Skipped => {}
            }
        }
    }

    Ok(DoctorReport {
        ok: !has_error,
        warnings: has_warn,
        agy_missing,
        login_needed,
        checks,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AliasActionStatus {
    Installed,
    AlreadyActive,
    Removed,
    NotPresent,
}

#[derive(Clone, Debug, Serialize)]
pub struct AliasOutcome {
    pub status: AliasActionStatus,
    pub stem: String,
    pub target_path: PathBuf,
    pub strategy: Option<&'static str>,
    pub dir_in_path: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct AliasStatusReport {
    pub stem: String,
    pub source_cli_exe: Option<PathBuf>,
    pub target_path: PathBuf,
    pub installed: bool,
    pub matches_source: bool,
    pub dir_in_path: bool,
}

fn resolve_cli_source_exe_for_alias() -> Result<PathBuf, CliError> {
    std::env::current_exe().map_err(|source| CliError::StateIo {
        path: PathBuf::from("current_exe"),
        source,
    })
}

pub fn inspect_alias_status(name: &str, dir: Option<&Path>) -> Result<AliasStatusReport, CliError> {
    let stem = validate_alias_stem(name)?;
    let source_cli_exe = resolve_cli_source_exe_for_alias().ok();
    let fallback_exe = std::env::current_exe().ok();
    let dest_dir = match dir {
        Some(d) => d.to_path_buf(),
        None => source_cli_exe
            .as_deref()
            .or(fallback_exe.as_deref())
            .and_then(Path::parent)
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
    };
    let target_path = alias_binary_path(&dest_dir, &stem);
    let installed = target_path.is_file();
    let matches_source = installed
        && source_cli_exe
            .as_deref()
            .is_some_and(|src| canonicalize_clean(&target_path) == canonicalize_clean(src));
    let dir_in_path = is_dir_in_path(&dest_dir);
    Ok(AliasStatusReport {
        stem,
        source_cli_exe,
        target_path,
        installed,
        matches_source,
        dir_in_path,
    })
}

pub fn execute_alias(args: &AliasArgs) -> Result<AliasOutcome, CliError> {
    let stem = validate_alias_stem(&args.name)?;
    if args.uninstall {
        let dest_dir = match &args.dir {
            Some(dir) => dir.clone(),
            None => resolve_cli_source_exe_for_alias()
                .ok()
                .or_else(|| std::env::current_exe().ok())
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .unwrap_or_else(|| PathBuf::from(".")),
        };
        let target_path = alias_binary_path(&dest_dir, &stem);
        let dummy_src = target_path.clone();
        return install_or_remove_alias(&dummy_src, &target_path, &stem, args.force, true);
    }
    let exe = resolve_cli_source_exe_for_alias()?;
    let dest_dir = match &args.dir {
        Some(dir) => dir.clone(),
        None => exe
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
    };
    let target_path = alias_binary_path(&dest_dir, &stem);
    install_or_remove_alias(&exe, &target_path, &stem, args.force, false)
}

pub fn render_completions(shell: clap_complete::Shell, bin_name: &str) -> String {
    let mut cmd = Cli::command();
    let mut buf = Vec::with_capacity(8192);
    clap_complete::generate(shell, &mut cmd, bin_name, &mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

fn validate_alias_stem(raw: &str) -> Result<String, CliError> {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_suffix(".exe")
        .or_else(|| trimmed.strip_suffix(".EXE"))
        .unwrap_or(trimmed);
    let valid = !stripped.is_empty()
        && stripped
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        return Err(CliError::InvalidAliasName {
            name: raw.to_owned(),
        });
    }
    Ok(stripped.to_owned())
}

fn alias_binary_path(dir: &Path, stem: &str) -> PathBuf {
    if cfg!(windows) {
        dir.join(format!("{stem}.exe"))
    } else {
        dir.join(stem)
    }
}

fn is_dir_in_path(dir: &Path) -> bool {
    let clean_dir = canonicalize_clean(dir);
    split_expanded_path_var()
        .into_iter()
        .any(|p| canonicalize_clean(&p) == clean_dir)
}

fn install_or_remove_alias(
    source_exe: &Path,
    target_path: &Path,
    stem: &str,
    force: bool,
    uninstall: bool,
) -> Result<AliasOutcome, CliError> {
    let clean_source = canonicalize_clean(source_exe);
    if uninstall {
        if !target_path.exists() {
            return Ok(AliasOutcome {
                status: AliasActionStatus::NotPresent,
                stem: stem.to_owned(),
                target_path: target_path.to_path_buf(),
                strategy: None,
                dir_in_path: target_path.parent().is_some_and(is_dir_in_path),
            });
        }
        fs::remove_file(target_path).map_err(|source| CliError::StateIo {
            path: target_path.to_path_buf(),
            source,
        })?;
        return Ok(AliasOutcome {
            status: AliasActionStatus::Removed,
            stem: stem.to_owned(),
            target_path: target_path.to_path_buf(),
            strategy: None,
            dir_in_path: target_path.parent().is_some_and(is_dir_in_path),
        });
    }

    if let Some(parent) = target_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| CliError::StateIo {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    if target_path.exists() {
        let clean_target = canonicalize_clean(target_path);
        if clean_target == clean_source {
            let dir_in_path = clean_target.parent().is_some_and(is_dir_in_path);
            return Ok(AliasOutcome {
                status: AliasActionStatus::AlreadyActive,
                stem: stem.to_owned(),
                target_path: clean_target,
                strategy: None,
                dir_in_path,
            });
        }
        if !force {
            return Err(CliError::AliasTargetExists {
                path: target_path.to_path_buf(),
            });
        }
        fs::remove_file(target_path).map_err(|source| CliError::StateIo {
            path: target_path.to_path_buf(),
            source,
        })?;
    }

    let strategy = link_or_copy_executable(&clean_source, target_path)?;
    let resolved_target = canonicalize_clean(target_path);
    let dir_in_path = resolved_target.parent().is_some_and(is_dir_in_path);
    Ok(AliasOutcome {
        status: AliasActionStatus::Installed,
        stem: stem.to_owned(),
        target_path: resolved_target,
        strategy: Some(strategy),
        dir_in_path,
    })
}

fn link_or_copy_executable(
    source_exe: &Path,
    target_path: &Path,
) -> Result<&'static str, CliError> {
    #[cfg(unix)]
    {
        if std::os::unix::fs::symlink(source_exe, target_path).is_ok() {
            return Ok("symbolic link");
        }
    }
    if fs::hard_link(source_exe, target_path).is_ok() {
        return Ok("hard link (zero extra disk space)");
    }
    fs::copy(source_exe, target_path).map_err(|source| CliError::StateIo {
        path: target_path.to_path_buf(),
        source,
    })?;
    Ok("standalone binary copy")
}

pub fn build_authenticated_dashboard_url(
    dashboard_base_url: &str,
    gateway_base_url: &str,
    api_key: Option<&str>,
) -> String {
    let clean_dash = dashboard_base_url.trim_end_matches('/');
    let clean_gw = gateway_base_url.trim_end_matches('/');
    let mut fragment_parts: Vec<String> = Vec::new();
    if !clean_dash.eq_ignore_ascii_case(clean_gw) {
        fragment_parts.push(format!(
            "base_url={}",
            percent_encode_query_component(clean_gw)
        ));
    }
    if let Some(key) = api_key.map(str::trim).filter(|k| !k.is_empty()) {
        fragment_parts.push(format!("api_key={}", percent_encode_query_component(key)));
    }
    if fragment_parts.is_empty() {
        format!("{clean_dash}/")
    } else {
        format!("{clean_dash}/#{}", fragment_parts.join("&"))
    }
}

pub fn open_browser(url: &str) -> Result<(), CliError> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
        const OPEN_VERB: &[u16] = &[b'o' as u16, b'p' as u16, b'e' as u16, b'n' as u16, 0];
        let mut wide_url: Vec<u16> = url.encode_utf16().collect();
        wide_url.push(0);
        // SAFETY: `OPEN_VERB` and `wide_url` are valid null-terminated UTF-16 buffers outliving the call.
        let rc = unsafe {
            ShellExecuteW(
                core::ptr::null_mut(),
                OPEN_VERB.as_ptr(),
                wide_url.as_ptr(),
                core::ptr::null(),
                core::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        if (rc as isize) <= 32 {
            return Err(CliError::BrowserOpen {
                url: url.to_owned(),
                source: io::Error::last_os_error(),
            });
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| CliError::BrowserOpen {
                url: url.to_owned(),
                source,
            })?;
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| CliError::BrowserOpen {
                url: url.to_owned(),
                source,
            })?;
        Ok(())
    }
}

pub fn open_path_in_file_manager(path: &Path) -> Result<(), CliError> {
    let target = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().unwrap_or(path).to_path_buf()
    };
    #[cfg(windows)]
    {
        Command::new("explorer.exe")
            .arg(&target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| CliError::BrowserOpen {
                url: target.display().to_string(),
                source,
            })?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(&target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| CliError::BrowserOpen {
                url: target.display().to_string(),
                source,
            })?;
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(&target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| CliError::BrowserOpen {
                url: target.display().to_string(),
                source,
            })?;
        Ok(())
    }
}

pub(crate) fn tail_file_lines(path: &Path, max_lines: usize) -> Result<String, CliError> {
    if max_lines == 0 {
        return Ok(String::new());
    }
    let mut file = File::open(path).map_err(|source| CliError::StateIo {
        path: path.to_path_buf(),
        source,
    })?;
    let len = file
        .metadata()
        .map_err(|source| CliError::StateIo {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    let start = len.saturating_sub(MAX_LOG_TAIL_READ_BYTES);
    file.seek(SeekFrom::Start(start))
        .map_err(|source| CliError::StateIo {
            path: path.to_path_buf(),
            source,
        })?;
    let mut raw = Vec::new();
    file.take(MAX_LOG_TAIL_READ_BYTES)
        .read_to_end(&mut raw)
        .map_err(|source| CliError::StateIo {
            path: path.to_path_buf(),
            source,
        })?;
    let text = String::from_utf8_lossy(&raw);
    let all_lines: Vec<&str> = text.lines().collect();
    let skip = all_lines.len().saturating_sub(max_lines);
    let slice = all_lines.get(skip..).unwrap_or(&all_lines);
    Ok(slice.join("\n"))
}

pub(crate) fn configure_detached_command(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
        // Avoid combining `DETACHED_PROCESS` with `CREATE_NO_WINDOW`, which conflicts in Win32
        // `CreateProcessW` and prevents stdio pipe handle isolation when `lg start` is piped.
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid()` is an async-signal-safe POSIX syscall with zero heap allocations
        // that detaches the child daemon into a new session and process group without a TTY.
        unsafe {
            cmd.pre_exec(|| {
                let _ = libc::setsid();
                Ok(())
            });
        }
    }
}

/// Temporarily clears `HANDLE_FLAG_INHERIT` on the parent CLI's stdio handles around Windows
/// background daemon spawning (`CreateProcessW` passes `bInheritHandles = TRUE` without a narrow
/// handle list), preventing piped callers (`lg start | Out-Null`, `$out = lg start`, CI runners)
/// from hanging on inherited stdout/stderr pipe ends.
#[cfg(windows)]
pub(crate) struct StdioInheritGuard {
    saved: Vec<(isize, u32)>,
}

#[cfg(windows)]
impl StdioInheritGuard {
    pub(crate) fn suspend_current() -> Self {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{
            GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
            SetHandleInformation,
        };

        let raw_handles = [
            std::io::stdin().as_raw_handle() as isize,
            std::io::stdout().as_raw_handle() as isize,
            std::io::stderr().as_raw_handle() as isize,
        ];
        let mut saved = Vec::with_capacity(3);
        for raw in raw_handles {
            if raw == 0 || (raw as HANDLE) == INVALID_HANDLE_VALUE {
                continue;
            }
            let mut flags: u32 = 0;
            // SAFETY: `raw` is a live process stdio handle obtained via `AsRawHandle`;
            // `flags` points to stack storage and we only toggle `HANDLE_FLAG_INHERIT`.
            unsafe {
                if GetHandleInformation(raw as HANDLE, &raw mut flags) != 0
                    && (flags & HANDLE_FLAG_INHERIT) != 0
                    && SetHandleInformation(raw as HANDLE, HANDLE_FLAG_INHERIT, 0) != 0
                {
                    saved.push((raw, flags));
                }
            }
        }
        Self { saved }
    }
}

#[cfg(windows)]
impl Drop for StdioInheritGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
        for &(raw, flags) in &self.saved {
            // SAFETY: Restores the original `HANDLE_FLAG_INHERIT` bit recorded in `suspend_current`.
            unsafe {
                let _ = SetHandleInformation(
                    raw as HANDLE,
                    HANDLE_FLAG_INHERIT,
                    flags & HANDLE_FLAG_INHERIT,
                );
            }
        }
    }
}

#[must_use]
pub fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, WAIT_TIMEOUT},
            System::Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
                WaitForSingleObject,
            },
        };
        // SAFETY: Read-only Win32 kernel handle probe; handle is closed immediately before return.
        unsafe {
            let handle = OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            );
            if handle.is_null() {
                return false;
            }
            let wait_rc = WaitForSingleObject(handle, 0);
            let _ = CloseHandle(handle);
            wait_rc == WAIT_TIMEOUT
        }
    }

    #[cfg(unix)]
    {
        let Ok(raw_pid) = i32::try_from(pid) else {
            return false;
        };
        if raw_pid <= 0 {
            return false;
        }
        // SAFETY: `kill(pid, 0)` sends no signal and checks process existence in the kernel.
        let rc = unsafe { libc::kill(raw_pid, 0) };
        rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

fn is_liftgate_process(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
            },
        };
        // SAFETY: Queries executable path into stack UTF-16 buffer and closes handle immediately.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut buf = [0u16; 1024];
            let mut size = u32::try_from(buf.len()).unwrap_or(1024);
            let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &raw mut size);
            let _ = CloseHandle(handle);
            if ok == 0 {
                return false;
            }
            let slice = buf.get(..size as usize).unwrap_or(&[]);
            let path_str = String::from_utf16_lossy(slice);
            is_recognized_gateway_exe_path(&path_str)
        }
    }

    #[cfg(unix)]
    {
        #[cfg(target_os = "linux")]
        if let Ok(exe) = fs::read_link(format!("/proc/{pid}/exe")) {
            return is_recognized_gateway_exe_path(&exe.to_string_lossy());
        }
        Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .stdin(Stdio::null())
            .output()
            .ok()
            .is_some_and(|out| {
                is_recognized_gateway_exe_path(&String::from_utf8_lossy(&out.stdout))
            })
    }
}

fn is_recognized_gateway_exe_path(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    let normalized = trimmed.replace('\\', "/");
    let file_name = normalized
        .rsplit('/')
        .next()
        .unwrap_or(normalized.as_str())
        .to_ascii_lowercase();
    let stem = file_name.strip_suffix(".exe").unwrap_or(&file_name);
    matches!(stem, "liftgate" | "lg") || stem.starts_with("liftgate-") || stem.starts_with("lg-")
}

fn terminate_pid_fallback(pid: u32) -> Result<(), CliError> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess},
        };
        // SAFETY: Opens `pid` with `PROCESS_TERMINATE`, requests termination, and closes handle.
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if handle.is_null() {
                return Err(CliError::DaemonStopFailed {
                    pid,
                    source: DaemonStopError::Os(io::Error::last_os_error()),
                });
            }
            let ok = TerminateProcess(handle, 1);
            let err = io::Error::last_os_error();
            let _ = CloseHandle(handle);
            if ok == 0 && is_pid_alive(pid) {
                return Err(CliError::DaemonStopFailed {
                    pid,
                    source: DaemonStopError::Os(err),
                });
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    {
        let raw_pid = i32::try_from(pid).map_err(|_| CliError::DaemonStopFailed {
            pid,
            source: DaemonStopError::PidOutOfRange { pid },
        })?;
        // SAFETY: Sends SIGTERM to verified `liftgate` process.
        let rc = unsafe { libc::kill(raw_pid, libc::SIGTERM) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                return Err(CliError::DaemonStopFailed {
                    pid,
                    source: DaemonStopError::Os(err),
                });
            }
        }
        Ok(())
    }
}

fn force_kill_pid_if_needed(pid: u32) -> Result<(), CliError> {
    #[cfg(windows)]
    {
        terminate_pid_fallback(pid)
    }

    #[cfg(unix)]
    {
        if let Ok(raw_pid) = i32::try_from(pid)
            && raw_pid > 0
        {
            // SAFETY: Escalates to SIGKILL on verified `liftgate` PID after async grace period.
            unsafe {
                let _ = libc::kill(raw_pid, libc::SIGKILL);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_pid_is_detected_alive() {
        assert!(is_pid_alive(std::process::id()));
    }

    #[test]
    fn tail_file_lines_returns_requested_suffix() {
        let tmp = std::env::temp_dir().join(format!("liftgate-tail-{}.log", Uuid::new_v4()));
        fs::write(&tmp, "line1\nline2\nline3\nline4\n").unwrap();
        let out = tail_file_lines(&tmp, 2).unwrap();
        assert_eq!(out, "line3\nline4");
        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn managed_daemon_api_key_takes_precedence_over_ambient_env_unless_explicitly_overridden() {
        let listen: SocketAddr = "127.0.0.1:18789".parse().unwrap();
        let daemon = DaemonState {
            pid: std::process::id(),
            listen,
            ui_listen: None,
            default_model: "gemini-4-carbon-high".to_owned(),
            acp_mode: "auto".to_owned(),
            api_key: Some("daemon-secret".to_owned()),
            agy_bin: None,
            acp_cwd: None,
            ui_dir: None,
            agy_proxy: None,
            agy_auto_update: true,
            vision_offload: false,
            mcp_tool_bridge: true,
            max_low_cache_records: liftgate_types::config::LowCacheRecordLimit::default(),
            low_cache_threshold_bps: liftgate_types::config::CacheHitThresholdBps::default(),
            capture_low_cache_request_details: true,
            max_line_bytes: liftgate_types::config::DEFAULT_MAX_LINE_BYTES,
            max_body_bytes: liftgate_types::config::DEFAULT_MAX_BODY_BYTES,
            channel_capacity: liftgate_types::config::DEFAULT_CHANNEL_CAPACITY,
            max_idle_sessions: liftgate_types::config::DEFAULT_MAX_IDLE_SESSIONS,
            max_stored_responses: liftgate_types::config::DEFAULT_MAX_STORED_RESPONSES,
            turn_timeout_secs: liftgate_types::config::DEFAULT_TURN_TIMEOUT_SECS,
            shutdown_token: "tok".to_owned(),
            started_at_unix_ms: 1,
            log_file: Path::new("gateway.log").to_path_buf(),
        };
        assert_eq!(
            resolve_client_api_key(None, Some(&daemon), listen).as_deref(),
            Some("daemon-secret")
        );
        assert_eq!(
            resolve_client_api_key(Some("cli-override"), Some(&daemon), listen).as_deref(),
            Some("cli-override")
        );
        assert_eq!(
            resolve_client_api_key(Some("local-agy"), None, listen),
            None
        );
    }

    #[test]
    fn serve_args_from_daemon_state_preserves_custom_paths_and_limits() {
        let daemon = DaemonState {
            pid: 99,
            listen: "127.0.0.1:18790".parse().unwrap(),
            ui_listen: Some("127.0.0.1:18791".parse().unwrap()),
            default_model: "claude-opus-4-6-thinking".to_owned(),
            acp_mode: "accept-edits".to_owned(),
            api_key: Some("k".to_owned()),
            agy_bin: Some(PathBuf::from("/opt/custom/agy")),
            acp_cwd: Some(PathBuf::from("/tmp/acp-work")),
            ui_dir: Some(PathBuf::from("/srv/ui")),
            agy_proxy: None,
            agy_auto_update: false,
            vision_offload: false,
            mcp_tool_bridge: false,
            max_low_cache_records: liftgate_types::config::LowCacheRecordLimit::try_new(250)
                .unwrap(),
            low_cache_threshold_bps:
                liftgate_types::config::CacheHitThresholdBps::try_from_percent(40).unwrap(),
            capture_low_cache_request_details: false,
            max_line_bytes: 67_108_864,
            max_body_bytes: 33_554_432,
            channel_capacity: 512,
            max_idle_sessions: 32,
            max_stored_responses: 128,
            turn_timeout_secs: 3600,
            shutdown_token: "tok".to_owned(),
            started_at_unix_ms: 1,
            log_file: PathBuf::from("gateway.log"),
        };
        let restored = serve_args_from_daemon_state(&daemon);
        assert_eq!(restored.agy_bin, Some(PathBuf::from("/opt/custom/agy")));
        assert_eq!(restored.acp_cwd, Some(PathBuf::from("/tmp/acp-work")));
        assert_eq!(restored.ui_dir, Some(PathBuf::from("/srv/ui")));
        assert_eq!(restored.vision_offload, Some(false));
        assert_eq!(restored.mcp_tool_bridge, Some(false));
        assert_eq!(restored.max_low_cache_records, Some(250));
        assert_eq!(restored.low_cache_threshold_pct, Some(40));
        assert_eq!(restored.low_cache_capture_payload, Some(false));
        assert_eq!(restored.max_line_bytes, 67_108_864);
        assert_eq!(restored.max_idle_sessions, 32);
        assert_eq!(restored.turn_timeout_secs, 3600);
    }

    #[test]
    fn recognizes_both_liftgate_and_lg_process_image_names() {
        assert!(is_recognized_gateway_exe_path(
            r"C:\Users\dev\.cargo\bin\liftgate.exe"
        ));
        assert!(is_recognized_gateway_exe_path(
            r"C:\Users\dev\.cargo\bin\lg.exe"
        ));
        assert!(is_recognized_gateway_exe_path("/usr/local/bin/liftgate"));
        assert!(is_recognized_gateway_exe_path("/usr/local/bin/lg"));
        assert!(!is_recognized_gateway_exe_path("/usr/bin/python3"));
        assert!(!is_recognized_gateway_exe_path(r"C:\liftgate\other.exe"));
    }

    #[test]
    fn alias_lifecycle_validates_installs_overwrites_and_uninstalls() {
        assert!(validate_alias_stem("lg").is_ok());
        assert_eq!(validate_alias_stem("lg.exe").unwrap(), "lg");
        assert!(validate_alias_stem("../bad").is_err());
        assert!(validate_alias_stem("bad/name").is_err());

        let dir = std::env::temp_dir().join(format!("liftgate-alias-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let fake_src = alias_binary_path(&dir, "liftgate");
        fs::write(&fake_src, b"fake-binary-bytes").unwrap();
        let target = alias_binary_path(&dir, "lg");

        install_or_remove_alias(&fake_src, &target, "lg", false, false).unwrap();
        assert!(target.is_file());

        let other_src = alias_binary_path(&dir, "other");
        fs::write(&other_src, b"different-bytes").unwrap();
        assert!(matches!(
            install_or_remove_alias(&other_src, &target, "lg", false, false),
            Err(CliError::AliasTargetExists { .. })
        ));
        install_or_remove_alias(&other_src, &target, "lg", true, false).unwrap();

        install_or_remove_alias(&other_src, &target, "lg", false, true).unwrap();
        assert!(!target.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reuses_healthy_daemon_before_requiring_discoverable_agy_binary() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::TcpListener;

        let temp = std::env::temp_dir().join(format!("liftgate-reuse-daemon-{}", Uuid::new_v4()));
        let state_paths = StatePaths::resolve(Some(temp.clone())).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            for _ in 0..4 {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut req_buf = [0u8; 1024];
                let n = stream.read(&mut req_buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&req_buf[..n]);
                let body = if req.starts_with("GET /v1/models ") {
                    r#"{"object":"list","data":[{"id":"claude-opus-4-6-thinking"}]}"#
                } else {
                    r#"{"status":"ok"}"#
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            }
        });

        let daemon = DaemonState {
            pid: std::process::id(),
            listen: addr,
            ui_listen: None,
            default_model: "claude-opus-4-6-thinking".to_owned(),
            acp_mode: "agent".to_owned(),
            api_key: None,
            agy_bin: Some(PathBuf::from("/custom/non-path/agy")),
            acp_cwd: None,
            ui_dir: None,
            agy_proxy: None,
            agy_auto_update: true,
            vision_offload: false,
            mcp_tool_bridge: true,
            max_low_cache_records: liftgate_types::config::LowCacheRecordLimit::default(),
            low_cache_threshold_bps: liftgate_types::config::CacheHitThresholdBps::default(),
            capture_low_cache_request_details: true,
            max_line_bytes: 33_554_432,
            max_body_bytes: 16_777_216,
            channel_capacity: 256,
            max_idle_sessions: 16,
            max_stored_responses: 64,
            turn_timeout_secs: 1800,
            shutdown_token: "tok".to_owned(),
            started_at_unix_ms: 1,
            log_file: state_paths.log_file(),
        };
        state_paths.write_daemon(&daemon).unwrap();

        let serve = ServeArgs {
            listen: addr,
            default_model: "claude-opus-4-6-thinking".to_owned(),
            acp_mode: "agent".to_owned(),
            max_line_bytes: 33_554_432,
            max_body_bytes: 16_777_216,
            channel_capacity: 256,
            max_idle_sessions: 16,
            max_stored_responses: 64,
            turn_timeout_secs: 1800,
            agy_bin: Some(temp.join("missing-agy-binary")),
            ..ServeArgs::default()
        };
        let outcome = start_daemon_with_exe(&state_paths, None, serve.clone(), false)
            .await
            .unwrap();
        assert!(outcome.already_running);
        assert!(outcome.config_matches);
        assert_eq!(outcome.daemon.listen, addr);
        assert_eq!(outcome.live_model_count, 1);

        let mut mismatched_serve = serve;
        mismatched_serve.no_mcp_tool_bridge = true;
        let mismatch_outcome = start_daemon_with_exe(&state_paths, None, mismatched_serve, false)
            .await
            .unwrap();
        assert!(mismatch_outcome.already_running);
        assert!(
            !mismatch_outcome.config_matches,
            "toggling --no-mcp-tool-bridge must be detected by config_matches"
        );

        server_task.abort();
        let _ = fs::remove_dir_all(&temp);
    }
}
