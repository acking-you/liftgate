use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use liftgate_server::platform::{resolve_agy_binary, wait_for_shutdown_signal};
use liftgate_types::config::{AcpModeId, GatewayLimits, ModelId};
use serde_json::json;
use uuid::Uuid;

use crate::cli::{
    DoctorArgs, LogsArgs, ModelsArgs, RestartArgs, ServeArgs, StartArgs, StatusArgs, UiArgs,
    normalize_optional_api_key,
};
use crate::client::{DiscoveredModel, GatewayClient, percent_encode_query_component};
use crate::config_sync::resolve_fast_model;
use crate::error::CliError;
use crate::harness::{resolve_claude_command, resolve_codex_command};
use crate::serve::{ENV_SHUTDOWN_TOKEN, active_embedded_ui};
use crate::state::{DaemonState, StatePaths, format_connect_url, now_unix_ms};

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
                    && existing.api_key == requested_key;
                if !quiet_banner {
                    println!(
                        "[OK] Liftgate gateway is already running (PID {}, API & Web UI: {}/)",
                        existing.pid,
                        existing.dashboard_base_url()
                    );
                    if !config_matches {
                        println!(
                            "  [Note] Active daemon uses listen={} model={}; run `liftgate restart ...` or `liftgate start --force ...` to apply new settings.",
                            existing.listen, existing.default_model
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
    let resolved_agy =
        resolve_agy_binary(serve.agy_bin.clone()).ok_or(CliError::AgyBinaryNotFound)?;

    let log_out = state_paths.prepare_log_file_for_spawn()?;
    let log_err = log_out.try_clone().map_err(|source| CliError::StateIo {
        path: state_paths.log_file(),
        source,
    })?;

    let exe_path = std::env::current_exe().map_err(|source| CliError::DaemonSpawn { source })?;
    let shutdown_token = Uuid::new_v4().to_string();
    let clean_api_key = normalize_optional_api_key(serve.api_key.as_deref());

    let mut cmd = Command::new(&exe_path);
    cmd.arg("serve")
        .arg("--listen")
        .arg(serve.listen.to_string())
        .arg("--agy-bin")
        .arg(&resolved_agy)
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
        shutdown_token,
        started_at_unix_ms: now_unix_ms(),
        log_file: state_paths.log_file(),
    };
    state_paths.write_daemon(&state)?;

    if !quiet_banner {
        let model_count = client.fetch_models().await.map_or(0, |m| m.len());
        println!("[OK] Liftgate background service started (PID {pid})");
        println!("  Gateway API  : {}/", state.connect_base_url());
        println!("  Web Dashboard: {}/", state.dashboard_base_url());
        println!(
            "  Default Model: {} (mode: {}, live models: {model_count})",
            state.default_model, state.acp_mode
        );
        println!("  Service Logs : {}", state.log_file.display());
        if model_count <= 1 {
            println!(
                "  [Hint] Only fallback model detected; run `agy acp --login` if you have not signed in yet."
            );
        }
    }

    Ok(state)
}

pub async fn stop_daemon(state_paths: &StatePaths, quiet: bool) -> Result<bool, CliError> {
    let Some(daemon) = state_paths.read_daemon()? else {
        if !quiet {
            println!("[INFO] No managed background gateway is currently running.");
        }
        return Ok(false);
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
                    source: io::Error::new(
                        io::ErrorKind::TimedOut,
                        "process remained alive after termination signal",
                    ),
                });
            }
        }
    }

    state_paths.remove_daemon()?;
    if !quiet {
        println!("[OK] Stopped Liftgate background service (PID {pid}).");
    }
    Ok(true)
}

pub async fn run_restart(state_paths: &StatePaths, args: RestartArgs) -> Result<(), CliError> {
    let prev = state_paths.read_daemon()?;
    let _ = stop_daemon(state_paths, false).await?;

    let mut serve = ServeArgs::default();
    if let Some(old) = prev {
        serve.listen = old.listen;
        serve.ui_listen = old.ui_listen;
        if !old.default_model.is_empty() {
            serve.default_model = old.default_model;
        }
        if !old.acp_mode.is_empty() {
            serve.acp_mode = old.acp_mode;
        }
        serve.api_key = old.api_key;
        serve.agy_bin = old.agy_bin;
        serve.acp_cwd = old.acp_cwd;
        serve.ui_dir = old.ui_dir;
    }

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
        eprintln!(
            "[liftgate] Starting background gateway on {} ...",
            format_connect_url(addr)
        );
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
    eprintln!(
        "[liftgate] Starting background gateway on {} ...",
        format_connect_url(serve.listen)
    );
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
            "telemetry": telemetry,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_owned())
        );
        return Ok(());
    }

    println!("Liftgate Service Status: {state_label}");
    if let Some(d) = &daemon
        && managed_alive
    {
        println!("  PID           : {}", d.pid);
    }
    println!("  Gateway API   : {}/", client.base_url());
    println!("  Web Dashboard : {dashboard_url}");
    println!(
        "  API Key Auth  : {}",
        if api_key.is_some() {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("  Log File      : {}", state_paths.log_file().display());

    if let Some(snap) = telemetry {
        println!(
            "  Uptime & ACP  : {}s · default={} · mode={} · agy_pid={:?}",
            snap.acp_process.uptime_secs,
            snap.acp_process.default_model,
            snap.acp_process.acp_mode,
            snap.acp_process.pid
        );
        println!(
            "  Session Pool  : {} active / {} idle (reused {}, created {})",
            snap.session_pool.active_leased_sessions,
            snap.session_pool.idle_cached_sessions,
            snap.session_pool.total_sessions_reused,
            snap.session_pool.total_sessions_created
        );
        println!(
            "  Requests      : {} total ({} completed, {} failed, {} active) · tokens in={} out={}",
            snap.metrics.total_requests,
            snap.metrics.completed_requests,
            snap.metrics.failed_requests,
            snap.metrics.active_requests,
            snap.metrics.total_input_tokens,
            snap.metrics.total_output_tokens
        );
    }
    if let Some(list) = models {
        println!("  Live Models   : {} discovered", list.len());
    }
    if !healthy {
        println!("  [Hint] Start the background gateway with: liftgate start");
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
        println!("[OK] Cleared gateway log at {}", log_path.display());
        return Ok(());
    }

    if log_path.is_file() {
        let tail = tail_file_lines(&log_path, args.lines)?;
        if !tail.is_empty() {
            print!("{tail}");
            if !tail.ends_with('\n') {
                println!();
            }
        }
    } else if !args.follow && !args.acp {
        println!("[INFO] No log file yet at {}", log_path.display());
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
            println!("--- Recent `agy acp` stderr ---");
            for item in &snap.acp_process.recent_stderr {
                println!("[{}] {}", item.timestamp_unix_ms, item.line);
            }
        }
    }

    if !args.follow {
        return Ok(());
    }

    let mut offset = fs::metadata(&log_path).map_or(0, |m| m.len());
    let mut buf = vec![0u8; 8192];
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
                                    print!("{}", String::from_utf8_lossy(chunk));
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        }
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
    println!("Opening Liftgate Web Dashboard: {}", target.dashboard_url);
    open_browser(&url)
}

pub async fn run_models(state_paths: &StatePaths, args: ModelsArgs) -> Result<(), CliError> {
    let target =
        ensure_gateway_ready(state_paths, args.listen, args.api_key, None, !args.no_start).await?;
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

    println!(
        "Available Antigravity Models ({} total · primary={} · fast={}):",
        models.len(),
        target.default_model,
        fast_model
    );
    for m in &models {
        let ctx_k = if m.context_window > 0 {
            format!("{}k", m.context_window / 1_000)
        } else {
            "1000k".to_owned()
        };
        let mut tags: Vec<&str> = Vec::new();
        if m.id == target.default_model {
            tags.push("default");
        }
        if m.id == fast_model {
            tags.push("fast");
        }
        let tag_str = if tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", tags.join(", "))
        };
        println!("  - {:<30} {:>6}  {}{tag_str}", m.id, ctx_k, m.display_name);
    }
    Ok(())
}

pub async fn run_doctor(state_paths: &StatePaths, args: DoctorArgs) -> Result<(), CliError> {
    println!("Liftgate Environment Doctor");
    println!("  State Directory : {}", state_paths.root().display());

    match resolve_agy_binary(args.agy_bin) {
        Some(bin) => println!("  [OK] agy CLI    : {}", bin.display()),
        None => println!(
            "  [!!] agy CLI    : NOT FOUND (install Antigravity CLI and run `agy acp --login`)"
        ),
    }

    let (_, full_ui_embedded) = active_embedded_ui();
    println!(
        "  [OK] Web UI     : {}",
        if full_ui_embedded {
            "embedded production React dashboard"
        } else {
            "embedded lightweight SSE telemetry dashboard"
        }
    );

    match resolve_claude_command(None) {
        Ok(cmd) => println!("  [OK] claude CLI : {}", cmd.program.display()),
        Err(_) => println!("  [--] claude CLI : not detected in PATH"),
    }
    match resolve_codex_command(None) {
        Ok(cmd) => println!("  [OK] codex CLI  : {}", cmd.program.display()),
        Err(_) => println!("  [--] codex CLI  : not detected in PATH"),
    }

    let daemon = state_paths.read_daemon()?;
    let listen = args
        .listen
        .or_else(|| daemon.as_ref().map(|d| d.listen))
        .unwrap_or_else(|| ServeArgs::default().listen);
    let api_key = resolve_client_api_key(args.api_key.as_deref(), daemon.as_ref(), listen);
    let client = GatewayClient::new(listen, api_key);

    if client.check_health(Duration::from_millis(800)).await {
        println!("  [OK] Gateway    : online at {}/", client.base_url());
        match client.fetch_models().await {
            Ok(models) if models.len() > 1 => {
                println!(
                    "  [OK] Auth/Models: {} live Antigravity models ready",
                    models.len()
                );
            }
            Ok(_) => {
                println!(
                    "  [!!] Auth/Models: only fallback model returned; run `agy acp --login` to sign in"
                );
            }
            Err(err) => {
                println!("  [!!] Auth/Models: {err}");
            }
        }
    } else {
        println!(
            "  [--] Gateway    : offline at {}/ (start with `liftgate start`)",
            client.base_url()
        );
    }

    Ok(())
}

fn build_authenticated_dashboard_url(
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

fn open_browser(url: &str) -> Result<(), CliError> {
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

fn tail_file_lines(path: &Path, max_lines: usize) -> Result<String, CliError> {
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
            let path_str = String::from_utf16_lossy(slice).to_ascii_lowercase();
            path_str.contains("liftgate")
        }
    }

    #[cfg(unix)]
    {
        #[cfg(target_os = "linux")]
        if let Ok(exe) = fs::read_link(format!("/proc/{pid}/exe")) {
            return exe
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains("liftgate");
        }
        Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .stdin(Stdio::null())
            .output()
            .ok()
            .is_some_and(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .to_ascii_lowercase()
                    .contains("liftgate")
            })
    }
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
}
