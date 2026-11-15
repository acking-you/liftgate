use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use comfy_table::{Attribute, Cell, Color as TableColor};
use liftgate_server::platform::{
    canonicalize_clean, split_expanded_path_var, wait_for_shutdown_signal,
};
use liftgate_types::config::{AcpModeId, GatewayLimits, ModelId, ProxyUrl};
use liftgate_types::telemetry::{TaskPhase, TaskProtocol};
use serde_json::json;
use uuid::Uuid;

use crate::agy_manager::{
    AgyAuthStatus, probe_agy_auth, probe_agy_version, quick_local_agy_summary,
};
use crate::cli::{
    AliasArgs, DoctorArgs, LogsArgs, ModelsArgs, RestartArgs, ServeArgs, StartArgs, StatusArgs,
    UiArgs, invoked_bin_name, normalize_optional_api_key,
};
use crate::client::{DiscoveredModel, GatewayClient, percent_encode_query_component};
use crate::config_sync::resolve_fast_model;
use crate::error::CliError;
use crate::harness::{resolve_claude_command, resolve_codex_command};
use crate::serve::{ENV_SHUTDOWN_TOKEN, active_embedded_ui};
use crate::state::{
    DaemonState, StatePaths, format_connect_url, now_unix_ms, resolve_effective_agy_auto_update,
    resolve_effective_agy_bin, resolve_effective_agy_proxy,
};
use crate::ui::{self, Spinner, Tone};

const MAX_LOG_TAIL_READ_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug)]
pub struct ActiveGatewayTarget {
    pub client: GatewayClient,
    pub dashboard_url: String,
    pub default_model: String,
    pub api_key: Option<String>,
}

pub async fn run_start(state_paths: &StatePaths, args: StartArgs) -> Result<(), CliError> {
    let daemon = start_daemon_inner(state_paths, args.serve, args.force, false).await?;
    if args.open {
        let url = build_authenticated_dashboard_url(
            &daemon.dashboard_base_url(),
            &daemon.connect_base_url(),
            daemon.api_key.as_deref(),
        );
        open_browser(&url)?;
    }
    Ok(())
}

pub async fn start_daemon_inner(
    state_paths: &StatePaths,
    serve: ServeArgs,
    force_restart: bool,
    quiet_banner: bool,
) -> Result<DaemonState, CliError> {
    start_daemon_with_exe(state_paths, None, serve, force_restart, quiet_banner).await
}

pub async fn start_daemon_with_exe(
    state_paths: &StatePaths,
    explicit_exe: Option<&Path>,
    serve: ServeArgs,
    force_restart: bool,
    quiet_banner: bool,
) -> Result<DaemonState, CliError> {
    let settings = state_paths.read_settings()?;
    let long_context = serve.long_context.unwrap_or(settings.long_context);
    let effective_proxy = resolve_effective_agy_proxy(serve.agy_proxy.as_deref(), &settings)?;
    let effective_auto_update = resolve_effective_agy_auto_update(
        serve.agy_auto_update,
        serve.no_agy_auto_update,
        &settings,
    );

    if let Some(existing) = state_paths.read_daemon()? {
        let existing_client = GatewayClient::new(existing.listen, existing.api_key.clone());
        if is_pid_alive(existing.pid)
            && existing_client
                .check_health(Duration::from_millis(800))
                .await
        {
            if force_restart {
                let _ = stop_daemon(state_paths, true).await?;
            } else {
                let requested_key = normalize_optional_api_key(serve.api_key.as_deref());
                let config_matches = existing.listen == serve.listen
                    && existing.ui_listen == serve.ui_listen
                    && existing.default_model == serve.default_model
                    && existing.acp_mode == serve.acp_mode
                    && existing.api_key == requested_key
                    && existing.agy_proxy.as_deref()
                        == effective_proxy.as_ref().map(ProxyUrl::as_str)
                    && existing.agy_auto_update == effective_auto_update
                    && existing.long_context == long_context;
                if !quiet_banner {
                    let bin = invoked_bin_name();
                    ui::headline(
                        Tone::Ok,
                        &format!(
                            "Liftgate background service is already online (PID {})",
                            existing.pid
                        ),
                    );
                    ui::kv_accent("Gateway API", &format!("{}/", existing.connect_base_url()));
                    ui::kv_accent(
                        "Web Dashboard",
                        &format!("{}/", existing.dashboard_base_url()),
                    );
                    ui::kv(
                        "Default Model",
                        &format!("{} (mode: {})", existing.default_model, existing.acp_mode),
                    );
                    let existing_proxy_label = existing
                        .agy_proxy
                        .as_deref()
                        .and_then(|s| ProxyUrl::try_new(s).ok())
                        .map(|p| p.redacted_display().into_owned())
                        .or_else(|| existing.agy_proxy.clone())
                        .unwrap_or_else(|| "none".to_owned());
                    ui::kv(
                        "AGY Policy",
                        &format!(
                            "proxy={existing_proxy_label} · idle-auto-update={}",
                            if existing.agy_auto_update {
                                "on"
                            } else {
                                "off"
                            }
                        ),
                    );
                    if !config_matches {
                        ui::note(
                            Tone::Warn,
                            &format!(
                                "Active daemon uses listen={} model={}; run `{bin} restart` or `{bin} up --force` to apply new flags.",
                                existing.listen, existing.default_model
                            ),
                        );
                    }
                }
                return Ok(existing);
            }
        } else {
            state_paths.remove_daemon()?;
        }
    }

    let probe = GatewayClient::new(serve.listen, serve.api_key.clone());
    if probe.check_health(Duration::from_millis(400)).await {
        return Err(CliError::PortOccupiedUnmanaged { addr: serve.listen });
    }

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
    let resolved_agy = resolve_effective_agy_bin(serve.agy_bin.clone(), &settings)
        .ok_or(CliError::AgyBinaryNotFound)?;

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

    let spinner = Spinner::start(format!(
        "Starting Liftgate background gateway on {} (initializing agy acp)…",
        format_connect_url(serve.listen)
    ));

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
        .arg(format!("--long-context={long_context}"))
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

    let mut child = cmd
        .spawn()
        .map_err(|source| CliError::DaemonSpawn { source })?;
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
        spinner.finish_and_clear().await;
        let _ = terminate_pid_fallback(pid);
        let _ = force_kill_pid_if_needed(pid);
        return Err(CliError::DaemonHealthTimeout {
            url: format!("{}/healthz", client.base_url()),
            log_path: state_paths.log_file(),
        });
    }
    spinner.finish_and_clear().await;

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
        long_context,
        shutdown_token,
        started_at_unix_ms: now_unix_ms(),
        log_file: state_paths.log_file(),
    };
    state_paths.write_daemon(&state)?;

    if !quiet_banner {
        let bin = invoked_bin_name();
        let model_count = client.fetch_models().await.map_or(0, |m| m.len());
        ui::headline(
            Tone::Ok,
            &format!("Liftgate background service started (PID {pid})"),
        );
        ui::kv_accent("Gateway API", &format!("{}/", state.connect_base_url()));
        ui::kv_accent("Web Dashboard", &format!("{}/", state.dashboard_base_url()));
        ui::kv(
            "Default Model",
            &format!(
                "{}  (mode: {}, {model_count} live models)",
                state.default_model, state.acp_mode
            ),
        );
        ui::kv(
            "AGY Policy",
            &format!(
                "proxy={} · idle-auto-update={}",
                effective_proxy
                    .as_ref()
                    .map(ProxyUrl::redacted_display)
                    .as_deref()
                    .unwrap_or("none"),
                if effective_auto_update { "on" } else { "off" }
            ),
        );
        ui::kv("Service Logs", &state.log_file.display().to_string());
        if model_count <= 1 {
            ui::note(
                Tone::Warn,
                &format!(
                    "Only fallback model detected; run `{bin} agy login` if you have not signed in yet."
                ),
            );
        }
        ui::cmd_hint("Live terminal dashboard", &format!("{bin} top"));
    }

    Ok(state)
}

pub async fn stop_daemon(state_paths: &StatePaths, quiet: bool) -> Result<bool, CliError> {
    let Some(daemon) = state_paths.read_daemon()? else {
        if !quiet {
            ui::headline(
                Tone::Muted,
                "No managed background gateway is currently running",
            );
        }
        return Ok(false);
    };

    let pid = daemon.pid;
    let spinner = (!quiet)
        .then(|| Spinner::start(format!("Draining & stopping Liftgate daemon (PID {pid})…")));
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
                if let Some(sp) = spinner {
                    sp.finish_and_clear().await;
                }
                return Err(CliError::DaemonStopFailed {
                    pid,
                    source: io::Error::new(
                        io::ErrorKind::TimedOut,
                        "process remained alive after termination signal",
                    ),
                });
            }
        }
    }

    if let Some(sp) = spinner {
        sp.finish_and_clear().await;
    }
    state_paths.remove_daemon()?;
    if !quiet {
        ui::headline(
            Tone::Ok,
            &format!("Stopped Liftgate background service (PID {pid})"),
        );
    }
    Ok(true)
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
        long_context: Some(old.long_context),
        api_key: old.api_key.clone(),
        agy_bin: old.agy_bin.clone(),
        acp_cwd: old.acp_cwd.clone(),
        ui_dir: old.ui_dir.clone(),
        ..defaults
    }
}

pub async fn run_restart(state_paths: &StatePaths, args: RestartArgs) -> Result<(), CliError> {
    let prev = state_paths.read_daemon()?;
    let _ = stop_daemon(state_paths, false).await?;

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
    if let Some(long_context) = args.long_context {
        serve.long_context = Some(long_context);
    }
    if args.no_agy_auto_update {
        serve.no_agy_auto_update = true;
    }

    let daemon = start_daemon_inner(state_paths, serve, false, false).await?;
    if args.open {
        let url = build_authenticated_dashboard_url(
            &daemon.dashboard_base_url(),
            &daemon.connect_base_url(),
            daemon.api_key.as_deref(),
        );
        open_browser(&url)?;
    }
    Ok(())
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
                source: io::Error::new(io::ErrorKind::ConnectionRefused, "gateway is not running"),
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
        let daemon = start_daemon_inner(state_paths, serve, false, true).await?;
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
            source: io::Error::new(io::ErrorKind::ConnectionRefused, "gateway is not running"),
        });
    }

    let mut serve = default_serve;
    if let Some(key) = explicit_api_key {
        serve.api_key = Some(key);
    }
    if let Some(m) = preferred_model.filter(|s| !s.trim().is_empty()) {
        serve.default_model = m.to_owned();
    }
    let daemon = start_daemon_inner(state_paths, serve, false, true).await?;
    let client = GatewayClient::new(daemon.listen, daemon.api_key.clone());
    Ok(ActiveGatewayTarget {
        dashboard_url: format!("{}/", daemon.dashboard_base_url()),
        default_model: daemon.default_model,
        api_key: daemon.api_key,
        client,
    })
}

pub async fn run_status(state_paths: &StatePaths, args: StatusArgs) -> Result<(), CliError> {
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

    let state_label = if healthy && managed_alive {
        "running (managed)"
    } else if healthy {
        "running (external)"
    } else {
        "stopped"
    };

    let dashboard_url = daemon.as_ref().map_or_else(
        || format!("{}/", client.base_url()),
        |d| format!("{}/", d.dashboard_base_url()),
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

    if args.json {
        let payload = json!({
            "status": state_label,
            "healthy": healthy,
            "pid": daemon.as_ref().filter(|_| managed_alive).map(|d| d.pid),
            "gateway_url": client.base_url(),
            "dashboard_url": dashboard_url,
            "auth_enabled": api_key.is_some(),
            "state_dir": state_paths.root(),
            "log_file": state_paths.log_file(),
            "models_count": models.as_ref().map(Vec::len),
            "agy": {
                "installed": local_agy.binary_path.is_some(),
                "binary_path": local_agy.binary_path,
                "version": effective_version,
                "proxy": effective_proxy_display,
                "auto_update_enabled": effective_auto_update,
                "latest_known_version": telemetry.as_ref().and_then(|t| t.acp_process.latest_known_version.clone()),
                "auto_update_note": telemetry.as_ref().and_then(|t| t.acp_process.auto_update_note.clone()),
            },
            "telemetry": telemetry,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_owned())
        );
        return Ok(());
    }

    let (pill_tone, pill_text) = if healthy && managed_alive {
        (Tone::Ok, "ONLINE · MANAGED")
    } else if healthy {
        (Tone::Info, "ONLINE · EXTERNAL")
    } else {
        (Tone::Err, "OFFLINE")
    };
    ui::banner(
        "Liftgate Gateway",
        Some((pill_tone, pill_text)),
        Some(&format!("v{}", env!("CARGO_PKG_VERSION"))),
    );

    if let Some(d) = &daemon
        && managed_alive
    {
        ui::kv("Service PID", &d.pid.to_string());
    }
    ui::kv_accent("Gateway API", &format!("{}/", client.base_url()));
    ui::kv_accent("Web Dashboard", &dashboard_url);
    ui::kv(
        "Authentication",
        if api_key.is_some() {
            "API key required (Bearer / x-api-key)"
        } else {
            "open loopback (no API key required)"
        },
    );
    let bin = invoked_bin_name();
    match (&local_agy.binary_path, &effective_version) {
        (Some(path), Some(ver)) => {
            ui::kv("Antigravity CLI", &format!("v{ver}  ({})", path.display()));
        }
        (Some(path), None) => {
            ui::kv("Antigravity CLI", &path.display().to_string());
        }
        (None, _) => {
            ui::kv(
                "Antigravity CLI",
                &format!("missing — run `{bin} agy install` to install"),
            );
        }
    }
    ui::kv(
        "AGY Proxy",
        &effective_proxy_display
            .as_deref()
            .map_or_else(|| "none (direct)".to_owned(), |p| format!("forced -> {p}")),
    );
    let auto_update_suffix = telemetry
        .as_ref()
        .and_then(|t| t.acp_process.auto_update_note.as_deref())
        .map_or_else(String::new, |note| format!("  ({note})"));
    ui::kv(
        "AGY Auto-Update",
        &format!(
            "{}{auto_update_suffix}",
            if effective_auto_update {
                "enabled (idle hot-swap)"
            } else {
                "disabled"
            }
        ),
    );
    ui::kv("Service Log", &state_paths.log_file().display().to_string());

    if let Some(snap) = telemetry {
        println!();
        let mut table = ui::new_table(&["Subsystem", "Status", "Telemetry Summary"]);

        let acp_tone = if snap.acp_process.connected {
            Tone::Ok
        } else {
            Tone::Err
        };
        let agy_pid = snap
            .acp_process
            .pid
            .map_or_else(|| "—".to_owned(), |p| p.to_string());
        let acp_detail = format!(
            "up {} · model={} · mode={} · agy_pid={agy_pid}",
            ui::format_duration_secs(snap.acp_process.uptime_secs),
            snap.acp_process.default_model,
            snap.acp_process.acp_mode,
        );
        table.add_row(vec![
            ui::accent_cell("ACP Engine"),
            ui::status_cell(
                if snap.acp_process.connected {
                    "connected"
                } else {
                    "disconnected"
                },
                acp_tone,
            ),
            Cell::new(acp_detail),
        ]);

        let pool_bar = ui::progress_bar(
            snap.session_pool.idle_cached_sessions,
            snap.session_pool.max_idle_sessions,
            8,
        );
        let pool_detail = format!(
            "{pool_bar}  {} active · {}/{} idle  (reused {}, created {}, evicted {})",
            snap.session_pool.active_leased_sessions,
            snap.session_pool.idle_cached_sessions,
            snap.session_pool.max_idle_sessions,
            snap.session_pool.total_sessions_reused,
            snap.session_pool.total_sessions_created,
            snap.session_pool.total_sessions_closed_or_evicted,
        );
        table.add_row(vec![
            ui::accent_cell("Session Pool"),
            ui::status_cell("ready", Tone::Ok),
            Cell::new(pool_detail),
        ]);

        let traffic_tone =
            if snap.metrics.failed_requests > 0 && snap.metrics.completed_requests == 0 {
                Tone::Warn
            } else {
                Tone::Ok
            };
        let traffic_detail = format!(
            "{} total ({} ok, {} err, {} in-flight) · tokens in {} / out {} / think {}",
            snap.metrics.total_requests,
            snap.metrics.completed_requests,
            snap.metrics.failed_requests,
            snap.metrics.active_requests,
            ui::format_tokens(snap.metrics.total_input_tokens),
            ui::format_tokens(snap.metrics.total_output_tokens),
            ui::format_tokens(snap.metrics.total_thought_tokens),
        );
        table.add_row(vec![
            ui::accent_cell("API Traffic"),
            ui::status_cell(
                &format!("{} active", snap.metrics.active_requests),
                traffic_tone,
            ),
            Cell::new(traffic_detail),
        ]);

        if let Some(list) = &models {
            let model_tone = if list.len() > 1 { Tone::Ok } else { Tone::Warn };
            table.add_row(vec![
                ui::accent_cell("Model Catalog"),
                ui::status_cell(&format!("{} live", list.len()), model_tone),
                Cell::new(if list.len() > 1 {
                    "Antigravity live catalog synchronized".to_owned()
                } else {
                    format!("Fallback catalog only (run `{bin} agy login` to authenticate)")
                }),
            ]);
        }

        println!("{table}");

        if !snap.active_tasks.is_empty() {
            ui::section("Active In-Flight Turns");
            let mut turns = ui::new_table(&["Turn", "Surface", "Model", "Phase", "Elapsed"]);
            for t in &snap.active_tasks {
                let surface = match t.protocol {
                    TaskProtocol::OpenaiResponses => "OpenAI",
                    TaskProtocol::AnthropicMessages => "Anthropic",
                };
                let phase = match t.phase {
                    TaskPhase::InitializingSession => "init",
                    TaskPhase::Thinking => "thinking",
                    TaskPhase::SearchingWeb => "web-search",
                    TaskPhase::FetchingWeb => "web-fetch",
                    TaskPhase::StreamingText => "streaming",
                    TaskPhase::CallingClientTool => "tool-call",
                    TaskPhase::Completed => "done",
                    TaskPhase::Failed => "failed",
                    TaskPhase::Cancelled => "cancelled",
                };
                let short_id: String = t.task_id.chars().take(12).collect();
                turns.add_row(vec![
                    ui::dim_cell(short_id),
                    Cell::new(surface),
                    ui::accent_cell(&t.resolved_model),
                    ui::status_cell(phase, Tone::Info),
                    ui::right_cell(ui::format_latency_ms(t.elapsed_ms)),
                ]);
            }
            println!("{turns}");
        }
    } else if let Some(list) = models {
        ui::kv("Live Models", &format!("{} discovered", list.len()));
    }

    if local_agy.binary_path.is_none() {
        println!();
        ui::cmd_hint("Install Antigravity CLI", &format!("{bin} agy install"));
        return Ok(());
    }
    if !healthy {
        println!();
        ui::cmd_hint("Start background service", &format!("{bin} up"));
        ui::cmd_hint("Open interactive command center", &bin);
    } else {
        println!();
        ui::cmd_hint("Open real-time TUI monitor", &format!("{bin} top"));
    }
    Ok(())
}

pub async fn run_logs(state_paths: &StatePaths, args: LogsArgs) -> Result<(), CliError> {
    let log_path = state_paths.log_file();
    if args.path {
        println!("{}", log_path.display());
        return Ok(());
    }
    if args.clear {
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
        ui::headline(
            Tone::Ok,
            &format!("Cleared gateway log at {}", log_path.display()),
        );
        return Ok(());
    }

    if log_path.is_file() {
        let tail = tail_file_lines(&log_path, args.lines)?;
        if !tail.is_empty() {
            for line in tail.lines() {
                ui::print_log_line(line);
            }
        }
    } else if !args.follow && !args.acp {
        ui::headline(
            Tone::Muted,
            &format!("No log file yet at {}", log_path.display()),
        );
        return Ok(());
    }

    if args.acp {
        let daemon = state_paths.read_daemon().ok().flatten();
        let listen = daemon
            .as_ref()
            .map_or_else(|| ServeArgs::default().listen, |d| d.listen);
        let api_key = resolve_client_api_key(None, daemon.as_ref(), listen);
        let client = GatewayClient::new(listen, api_key);
        if let Ok(snap) = client.fetch_telemetry().await
            && !snap.acp_process.recent_stderr.is_empty()
        {
            ui::section("Recent `agy acp` stderr");
            for item in &snap.acp_process.recent_stderr {
                let ts = ui::format_utc_hms(item.timestamp_unix_ms);
                ui::print_log_line(&format!("{ts}  {}", item.line));
            }
        }
    }

    if !args.follow {
        return Ok(());
    }

    let mut offset = fs::metadata(&log_path).map_or(0, |m| m.len());
    let mut buf = vec![0u8; 8192];
    let mut pending_line = String::new();
    let mut shutdown = std::pin::pin!(wait_for_shutdown_signal());

    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            () = tokio::time::sleep(Duration::from_millis(250)) => {
                let Ok(meta) = fs::metadata(&log_path) else {
                    continue;
                };
                if meta.len() < offset {
                    offset = 0;
                    pending_line.clear();
                }
                if meta.len() > offset
                    && let Ok(mut file) = File::open(&log_path)
                    && file.seek(SeekFrom::Start(offset)).is_ok()
                {
                    loop {
                        match file.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                offset += n as u64;
                                if let Some(chunk) = buf.get(..n) {
                                    pending_line.push_str(&String::from_utf8_lossy(chunk));
                                    while let Some(nl_pos) = pending_line.find('\n') {
                                        let rest = pending_line.split_off(nl_pos + 1);
                                        let line = pending_line.trim_end_matches(&['\r', '\n'][..]);
                                        ui::print_log_line(line);
                                        pending_line = rest;
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        }
    }
    if !pending_line.is_empty() {
        ui::print_log_line(&pending_line);
    }
    Ok(())
}

pub async fn run_ui(state_paths: &StatePaths, args: UiArgs) -> Result<(), CliError> {
    let target =
        ensure_gateway_ready(state_paths, args.listen, args.api_key, None, !args.no_start).await?;

    let url = build_authenticated_dashboard_url(
        target.dashboard_url.trim_end_matches('/'),
        target.client.base_url(),
        target.api_key.as_deref(),
    );
    if args.print {
        println!("{url}");
        return Ok(());
    }
    ui::headline(Tone::Ok, "Opening Liftgate Web Dashboard in browser");
    ui::kv_accent("Dashboard URL", &target.dashboard_url);
    open_browser(&url)
}

pub async fn run_models(state_paths: &StatePaths, args: ModelsArgs) -> Result<(), CliError> {
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

    if args.json {
        let payload = json!({
            "default_model": target.default_model,
            "fast_model": fast_model,
            "count": models.len(),
            "models": models.iter().map(|m| json!({
                "id": m.id,
                "display_name": m.display_name,
                "context_window": m.context_window,
            })).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_owned())
        );
        return Ok(());
    }

    if args.pick {
        let chosen =
            crate::tui::run_model_picker(&models, &target.default_model, &fast_model).await?;
        let Some(selected) = chosen else {
            return Ok(());
        };
        let bin = invoked_bin_name();
        if selected == target.default_model {
            ui::headline(
                Tone::Info,
                &format!("Default model is already `{selected}`"),
            );
            return Ok(());
        }
        let managed_daemon = state_paths
            .read_daemon()?
            .filter(|d| is_pid_alive(d.pid) && args.listen.is_none_or(|l| l == d.listen));
        if managed_daemon.is_some() {
            ui::headline(
                Tone::Action,
                &format!("Switching managed gateway default model -> `{selected}`"),
            );
            run_restart(
                state_paths,
                RestartArgs {
                    listen: args.listen,
                    default_model: Some(selected),
                    api_key: args.api_key,
                    ..RestartArgs::default()
                },
            )
            .await?;
        } else {
            ui::headline(Tone::Info, &format!("Selected model `{selected}`"));
            ui::cmd_hint(
                "Launch Codex with this model",
                &format!("{bin} cx -m {selected}"),
            );
            ui::cmd_hint(
                "Launch Claude with this model",
                &format!("{bin} cc -m {selected}"),
            );
        }
        return Ok(());
    }

    let count_badge = format!("{} models", models.len());
    let sub = format!("default={} · fast={fast_model}", target.default_model);
    ui::banner(
        "Antigravity Model Catalog",
        Some((Tone::Ok, &count_badge)),
        Some(&sub),
    );
    println!();

    let mut table = ui::new_table(&["Model ID", "Display Name", "Context", "Role"]);
    for m in &models {
        let ctx_k = if m.context_window > 0 {
            format!("{}k", m.context_window / 1_000)
        } else {
            "1000k".to_owned()
        };
        let is_default = m.id == target.default_model;
        let is_fast = m.id == fast_model;
        let id_cell = if is_default && ui::use_color() {
            Cell::new(&m.id)
                .fg(TableColor::Green)
                .add_attribute(Attribute::Bold)
        } else if is_fast && ui::use_color() {
            Cell::new(&m.id).fg(TableColor::Cyan)
        } else {
            Cell::new(&m.id)
        };
        let role_cell = match (is_default, is_fast) {
            (true, true) => ui::status_cell("default · fast", Tone::Ok),
            (true, false) => ui::status_cell("default", Tone::Ok),
            (false, true) => ui::status_cell("fast", Tone::Info),
            (false, false) => ui::dim_cell("—"),
        };
        let display = if m.display_name.is_empty() {
            m.id.as_str()
        } else {
            m.display_name.as_str()
        };
        table.add_row(vec![
            id_cell,
            Cell::new(display),
            ui::right_cell(ctx_k),
            role_cell,
        ]);
    }
    println!("{table}");
    println!();
    let bin = invoked_bin_name();
    ui::cmd_hint(
        "Interactively filter & switch default model",
        &format!("{bin} m -i"),
    );
    Ok(())
}

pub async fn run_doctor(state_paths: &StatePaths, args: DoctorArgs) -> Result<(), CliError> {
    struct CheckRow {
        name: &'static str,
        status: &'static str,
        tone: Tone,
        detail: String,
    }

    let bin = invoked_bin_name();
    let settings = state_paths.read_settings()?;
    let env_proxy = std::env::var("AGY_PROXY").ok();
    let effective_proxy = resolve_effective_agy_proxy(env_proxy.as_deref(), &settings)?;
    let auto_update_enabled = settings.agy_auto_update;
    let mut checks: Vec<CheckRow> = Vec::with_capacity(9);
    let mut has_error = false;
    let mut has_warn = false;
    let mut agy_missing = false;
    let mut login_needed = false;

    checks.push(CheckRow {
        name: "State Directory",
        status: "ready",
        tone: Tone::Ok,
        detail: state_paths.root().display().to_string(),
    });

    let resolved_agy = resolve_effective_agy_bin(args.agy_bin, &settings);
    match &resolved_agy {
        Some(agy) => {
            let ver = probe_agy_version(agy, effective_proxy.as_ref()).await;
            let detail = match ver {
                Some(v) => format!("v{v} ({})", agy.display()),
                None => agy.display().to_string(),
            };
            checks.push(CheckRow {
                name: "Antigravity CLI",
                status: "found",
                tone: Tone::Ok,
                detail,
            });
        }
        None => {
            has_error = true;
            agy_missing = true;
            checks.push(CheckRow {
                name: "Antigravity CLI",
                status: "missing",
                tone: Tone::Err,
                detail: format!(
                    "NOT FOUND — run `{bin} agy install` to download & verify official agy CLI"
                ),
            });
        }
    }

    match &effective_proxy {
        Some(proxy) => checks.push(CheckRow {
            name: "AGY Proxy",
            status: "forced",
            tone: Tone::Info,
            detail: format!(
                "{} (forcibly injected into agy; 127.0.0.1 bypassed via NO_PROXY)",
                proxy.redacted_display()
            ),
        }),
        None => checks.push(CheckRow {
            name: "AGY Proxy",
            status: "direct",
            tone: Tone::Ok,
            detail: format!(
                "none (default: no proxy injected; configure via `{bin} proxy <port|url>`)"
            ),
        }),
    }

    checks.push(CheckRow {
        name: "AGY Auto-Update",
        status: if auto_update_enabled {
            "enabled"
        } else {
            "disabled"
        },
        tone: if auto_update_enabled {
            Tone::Ok
        } else {
            Tone::Muted
        },
        detail: if auto_update_enabled {
            "checks official manifest & hot-swaps agy when gateway is idle".to_owned()
        } else {
            format!("disabled (enable via `{bin} agy auto-update on`)")
        },
    });

    let (ui_assets, full_ui_embedded) = active_embedded_ui();
    checks.push(CheckRow {
        name: "Embedded Web UI",
        status: if full_ui_embedded {
            "bundled"
        } else {
            "fallback"
        },
        tone: Tone::Ok,
        detail: if full_ui_embedded {
            format!("Production React dashboard ({} assets)", ui_assets.len())
        } else {
            "Lightweight SSE telemetry dashboard".to_owned()
        },
    });

    match resolve_claude_command(None) {
        Ok(cmd) => checks.push(CheckRow {
            name: "Claude Code CLI",
            status: "found",
            tone: Tone::Ok,
            detail: cmd.program.display().to_string(),
        }),
        Err(_) => checks.push(CheckRow {
            name: "Claude Code CLI",
            status: "optional",
            tone: Tone::Muted,
            detail: "not detected in PATH (`npm i -g @anthropic-ai/claude-code`)".to_owned(),
        }),
    }
    match resolve_codex_command(None) {
        Ok(cmd) => checks.push(CheckRow {
            name: "Codex CLI",
            status: "found",
            tone: Tone::Ok,
            detail: cmd.program.display().to_string(),
        }),
        Err(_) => checks.push(CheckRow {
            name: "Codex CLI",
            status: "optional",
            tone: Tone::Muted,
            detail: "not detected in PATH (`npm i -g @openai/codex`)".to_owned(),
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
        checks.push(CheckRow {
            name: "Gateway Service",
            status: "online",
            tone: Tone::Ok,
            detail: format!("{}/", client.base_url()),
        });
        match client.fetch_models().await {
            Ok(models) if models.len() > 1 => {
                checks.push(CheckRow {
                    name: "ACP Auth/Models",
                    status: "ready",
                    tone: Tone::Ok,
                    detail: format!("{} live Antigravity models available", models.len()),
                });
            }
            Ok(_) => {
                has_warn = true;
                login_needed = true;
                checks.push(CheckRow {
                    name: "ACP Auth/Models",
                    status: "login needed",
                    tone: Tone::Warn,
                    detail: format!("only fallback model returned; run `{bin} agy login`"),
                });
            }
            Err(err) => {
                has_error = true;
                checks.push(CheckRow {
                    name: "ACP Auth/Models",
                    status: "error",
                    tone: Tone::Err,
                    detail: err.to_string(),
                });
            }
        }
    } else {
        has_warn = true;
        checks.push(CheckRow {
            name: "Gateway Service",
            status: "offline",
            tone: Tone::Warn,
            detail: format!(
                "not listening at {}/ (start with `{bin} up`)",
                client.base_url()
            ),
        });
        if let Some(agy) = &resolved_agy {
            match probe_agy_auth(state_paths, agy, effective_proxy.as_ref()).await {
                AgyAuthStatus::SignedIn {
                    model_count,
                    source,
                } => {
                    checks.push(CheckRow {
                        name: "ACP Auth/Models",
                        status: "ready",
                        tone: Tone::Ok,
                        detail: format!(
                            "signed in ({model_count} live Antigravity models via {source})"
                        ),
                    });
                }
                AgyAuthStatus::NotSignedIn { detail } => {
                    has_warn = true;
                    login_needed = true;
                    checks.push(CheckRow {
                        name: "ACP Auth/Models",
                        status: "login needed",
                        tone: Tone::Warn,
                        detail: format!("{detail} (run `{bin} agy login`)"),
                    });
                }
                AgyAuthStatus::NetworkError { detail } => {
                    has_warn = true;
                    checks.push(CheckRow {
                        name: "ACP Auth/Models",
                        status: "check failed",
                        tone: Tone::Warn,
                        detail: format!("{detail} (verify `{bin} proxy` or run `{bin} agy login`)"),
                    });
                }
                AgyAuthStatus::NotInstalled | AgyAuthStatus::Skipped => {}
            }
        }
    }

    if args.json {
        let payload = json!({
            "ok": !has_error,
            "warnings": has_warn,
            "checks": checks.iter().map(|c| json!({
                "name": c.name,
                "status": c.status,
                "detail": c.detail,
            })).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_owned())
        );
        return Ok(());
    }

    let summary_pill = if has_error {
        (Tone::Err, "ACTION REQUIRED")
    } else if has_warn {
        (Tone::Warn, "READY WITH NOTES")
    } else {
        (Tone::Ok, "ALL SYSTEMS READY")
    };
    ui::banner(
        "Liftgate Environment Doctor",
        Some(summary_pill),
        Some(&format!("v{}", env!("CARGO_PKG_VERSION"))),
    );
    println!();

    let mut table = ui::new_table(&["Component", "Status", "Details"]);
    for row in &checks {
        table.add_row(vec![
            ui::accent_cell(row.name),
            ui::status_cell(row.status, row.tone),
            Cell::new(&row.detail),
        ]);
    }
    println!("{table}");

    if agy_missing {
        println!();
        ui::cmd_hint(
            "Download & install Antigravity CLI",
            &format!("{bin} agy install"),
        );
        ui::cmd_hint(
            "Configure proxy first (if needed)",
            &format!("{bin} proxy 7890"),
        );
    } else if login_needed {
        println!();
        ui::cmd_hint(
            "Sign in to Antigravity interactively",
            &format!("{bin} agy login"),
        );
    } else if has_warn && !has_error {
        println!();
        ui::cmd_hint("Start background gateway", &format!("{bin} up"));
    }

    Ok(())
}

pub fn run_alias(args: AliasArgs) -> Result<(), CliError> {
    let exe = std::env::current_exe().map_err(|source| CliError::StateIo {
        path: PathBuf::from("current_exe"),
        source,
    })?;
    let stem = validate_alias_stem(&args.name)?;
    let dest_dir = match &args.dir {
        Some(dir) => dir.clone(),
        None => exe
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
    };
    let target_path = alias_binary_path(&dest_dir, &stem);
    install_or_remove_alias(&exe, &target_path, &stem, args.force, args.uninstall)
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

fn install_or_remove_alias(
    source_exe: &Path,
    target_path: &Path,
    stem: &str,
    force: bool,
    uninstall: bool,
) -> Result<(), CliError> {
    let clean_source = canonicalize_clean(source_exe);
    if uninstall {
        if !target_path.exists() {
            ui::headline(
                Tone::Muted,
                &format!("Alias `{stem}` is not present at {}", target_path.display()),
            );
            return Ok(());
        }
        fs::remove_file(target_path).map_err(|source| CliError::StateIo {
            path: target_path.to_path_buf(),
            source,
        })?;
        ui::headline(
            Tone::Ok,
            &format!("Removed CLI alias `{stem}` ({})", target_path.display()),
        );
        return Ok(());
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
            ui::headline(Tone::Ok, &format!("CLI alias `{stem}` is already active"));
            ui::kv_accent("Executable", &clean_target.display().to_string());
            return Ok(());
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
    ui::headline(Tone::Ok, &format!("Installed short CLI alias `{stem}`"));
    ui::kv_accent("Alias Binary", &resolved_target.display().to_string());
    ui::kv("Link Strategy", strategy);

    if let Some(dir) = resolved_target.parent() {
        let clean_dir = canonicalize_clean(dir);
        let in_path = split_expanded_path_var()
            .into_iter()
            .any(|p| canonicalize_clean(&p) == clean_dir);
        if !in_path {
            ui::note(
                Tone::Warn,
                &format!(
                    "Directory `{}` is not currently in your PATH; add it or pass `--dir <dir-on-PATH>`",
                    clean_dir.display()
                ),
            );
        }
    }
    ui::cmd_hint("Try your short alias", &format!("{stem} st"));
    Ok(())
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

pub(crate) fn build_authenticated_dashboard_url(
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

pub(crate) fn open_browser(url: &str) -> Result<(), CliError> {
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

fn configure_detached_command(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{
            CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
        };
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW);
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
                    source: io::Error::last_os_error(),
                });
            }
            let ok = TerminateProcess(handle, 1);
            let err = io::Error::last_os_error();
            let _ = CloseHandle(handle);
            if ok == 0 && is_pid_alive(pid) {
                return Err(CliError::DaemonStopFailed { pid, source: err });
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    {
        let raw_pid = i32::try_from(pid).map_err(|_| CliError::DaemonStopFailed {
            pid,
            source: io::Error::new(io::ErrorKind::InvalidInput, "PID exceeds i32"),
        })?;
        // SAFETY: Sends SIGTERM to verified `liftgate` process.
        let rc = unsafe { libc::kill(raw_pid, libc::SIGTERM) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                return Err(CliError::DaemonStopFailed { pid, source: err });
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
            long_context: false,
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
}
