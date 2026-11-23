//! Terminal rendering, progress spinners, UTF-8 tables, and `--json` output for `liftgate` CLI.
//! All business logic, HTTP/SSE I/O, config mutations, and process management delegate to `liftgate_core`.

use std::ffi::OsString;
use std::fs::File;
use std::io::{
    BufRead as _, BufReader, IsTerminal as _, Seek as _, SeekFrom, Write as _, stdin, stdout,
};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use comfy_table::Cell;
use liftgate_core::{
    agy_manager::{
        AgyAuthStatus, AgyInstallOutcome, AgyInstallStatus, AgyUpdateOutcome, AgyUpdateStatus,
        execute_agy_auto_update, execute_agy_install_with_progress,
        execute_agy_update_with_progress, execute_proxy, inspect_agy_status, probe_agy_auth,
        run_agy_passthrough, spawn_interactive_agy,
    },
    cli::{
        AgyArgs, AgyCommand, AgyInstallArgs, AgyLoginArgs, AgyStatusArgs, AgyUpdateArgs, AliasArgs,
        ClaudeArgs, ClaudeDesktopArgs, CodexAppArgs, CodexArgs, ConfigArgs, DoctorArgs, LogsArgs,
        ModelsArgs, OmpArgs, OpencodeArgs, ProxyArgs, RestartArgs, ServeArgs, StartArgs,
        StatusArgs, UiArgs, UpgradeArgs, invoked_bin_name,
    },
    client::GatewayClient,
    config_sync::execute_config_sync,
    daemon::{
        AliasActionStatus, GatewayStatusMode, build_authenticated_dashboard_url, clear_gateway_log,
        execute_alias, inspect_doctor, inspect_gateway_status, inspect_models, is_pid_alive,
        open_browser, read_log_tail, resolve_client_api_key, resolve_ui_target, restart_daemon,
        start_daemon, stop_daemon,
    },
    desktop::{DesktopLaunchOutcome, launch_claude_desktop, launch_codex_app},
    error::CliError,
    harness,
    state::{DaemonState, StatePaths, resolve_effective_agy_bin, resolve_effective_agy_proxy},
    upgrade::{TransferProgress, execute_upgrade_with_progress},
};
use liftgate_types::telemetry::{TaskPhase, TaskProtocol};
use serde_json::json;
use tokio::sync::watch;

use crate::ui::{self, Spinner, Tone, TransferProgressDisplay};

pub async fn run_start(state_paths: &StatePaths, args: StartArgs) -> Result<(), CliError> {
    let sp = Spinner::start(format!(
        "Starting Liftgate background daemon on {}…",
        args.serve.listen
    ));
    let out_res = start_daemon(state_paths, args.serve, args.force).await;
    sp.finish_and_clear().await;
    let out = out_res?;

    if out.already_running {
        if out.config_matches {
            ui::headline(
                Tone::Ok,
                &format!(
                    "Liftgate daemon is already running (PID {})",
                    out.daemon.pid
                ),
            );
        } else {
            ui::headline(
                Tone::Warn,
                &format!(
                    "Liftgate daemon is already running (PID {}); pass `--force` or `restart` to apply new flags",
                    out.daemon.pid
                ),
            );
        }
    } else {
        ui::headline(
            Tone::Ok,
            &format!(
                "Liftgate background daemon started (PID {})",
                out.daemon.pid
            ),
        );
    }
    print_daemon_summary(&out.daemon, out.live_model_count);

    if args.open {
        let url = build_authenticated_dashboard_url(
            &out.daemon.dashboard_base_url(),
            &out.daemon.connect_base_url(),
            out.daemon.api_key.as_deref(),
        );
        open_browser(&url)?;
    }
    Ok(())
}

pub async fn run_stop(state_paths: &StatePaths) -> Result<(), CliError> {
    let sp = Spinner::start("Stopping Liftgate background daemon…");
    let out_res = stop_daemon(state_paths).await;
    sp.finish_and_clear().await;
    let out = out_res?;
    match out.stopped_pid {
        Some(pid) => ui::headline(
            Tone::Ok,
            &format!("Stopped Liftgate background daemon (PID {pid})"),
        ),
        None => ui::headline(
            Tone::Muted,
            "No managed Liftgate background daemon is running",
        ),
    }
    Ok(())
}

pub async fn run_restart(state_paths: &StatePaths, args: RestartArgs) -> Result<(), CliError> {
    let sp = Spinner::start("Restarting Liftgate background daemon…");
    let out_res = restart_daemon(state_paths, args).await;
    sp.finish_and_clear().await;
    let out = out_res?;
    ui::headline(
        Tone::Ok,
        &format!(
            "Restarted Liftgate background daemon (PID {})",
            out.daemon.pid
        ),
    );
    print_daemon_summary(&out.daemon, out.live_model_count);
    Ok(())
}

fn print_daemon_summary(d: &DaemonState, live_model_count: usize) {
    ui::kv_accent("Gateway API", &format!("{}/", d.connect_base_url()));
    ui::kv_accent("Web Dashboard", &format!("{}/", d.dashboard_base_url()));
    ui::kv(
        "Default Model",
        &format!("{} (mode: {})", d.default_model, d.acp_mode),
    );
    if live_model_count > 0 {
        ui::kv("Live Models", &format!("{live_model_count} synchronized"));
    }
}

pub async fn run_status(state_paths: &StatePaths, args: StatusArgs) -> Result<(), CliError> {
    let sp = (!args.json).then(|| Spinner::start("Inspecting Liftgate gateway status…"));
    let report_res = inspect_gateway_status(state_paths, &args).await;
    if let Some(s) = sp {
        s.finish_and_clear().await;
    }
    let report = report_res?;

    if args.json {
        let payload = json!({
            "status": report.mode.status_label(),
            "healthy": report.healthy,
            "pid": report.pid,
            "gateway_url": report.gateway_url,
            "dashboard_url": report.dashboard_url,
            "auth_enabled": report.auth_enabled,
            "state_dir": report.state_dir,
            "log_file": report.log_file,
            "models_count": report.models.as_ref().map(Vec::len),
            "agy": {
                "installed": report.agy_binary_path.is_some(),
                "binary_path": report.agy_binary_path,
                "version": report.agy_version,
                "proxy": report.agy_proxy_display,
                "auto_update_enabled": report.agy_auto_update_enabled,
                "vision_offload_enabled": report.vision_offload_enabled,
                "mcp_tool_bridge_enabled": report.mcp_tool_bridge_enabled,
                "latest_known_version": report.telemetry.as_ref().and_then(|t| t.acp_process.latest_known_version.clone()),
                "auto_update_note": report.telemetry.as_ref().and_then(|t| t.acp_process.auto_update_note.clone()),
            },
            "telemetry": report.telemetry,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_owned())
        );
        return Ok(());
    }

    let pill_tone = match report.mode {
        GatewayStatusMode::Managed => Tone::Ok,
        GatewayStatusMode::External => Tone::Info,
        GatewayStatusMode::Offline => Tone::Err,
    };
    ui::banner(
        "Liftgate Gateway",
        Some((pill_tone, report.mode.pill_text())),
        Some(&format!("v{}", env!("CARGO_PKG_VERSION"))),
    );

    if let Some(pid) = report.pid {
        ui::kv("Service PID", &pid.to_string());
    }
    ui::kv_accent("Gateway API", &format!("{}/", report.gateway_url));
    ui::kv_accent("Web Dashboard", &report.dashboard_url);
    ui::kv(
        "Authentication",
        if report.auth_enabled {
            "API key required (Bearer / x-api-key)"
        } else {
            "open loopback (no API key required)"
        },
    );
    let bin = invoked_bin_name();
    match (&report.agy_binary_path, &report.agy_version) {
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
        &report
            .agy_proxy_display
            .as_deref()
            .map_or_else(|| "none (direct)".to_owned(), |p| format!("forced -> {p}")),
    );
    let auto_update_suffix = report
        .telemetry
        .as_ref()
        .and_then(|t| t.acp_process.auto_update_note.as_deref())
        .map_or_else(String::new, |note| format!("  ({note})"));
    ui::kv(
        "AGY Auto-Update",
        &format!(
            "{}{auto_update_suffix}",
            if report.agy_auto_update_enabled {
                "enabled (idle hot-swap)"
            } else {
                "disabled"
            }
        ),
    );
    ui::kv(
        "Vision Offload",
        if report.vision_offload_enabled {
            "enabled (>=150k ephemeral sub-session)"
        } else {
            "disabled"
        },
    );
    ui::kv(
        "MCP Tool Bridge",
        if report.mcp_tool_bridge_enabled {
            "enabled (loopback MCP suspension + XML coexistence)"
        } else {
            "disabled (XML-only client tools)"
        },
    );
    ui::kv(
        "Context Policy",
        "native single-turn (500k Gemini / 200k Claude / 128k GPT-OSS)",
    );
    ui::kv("Service Log", &report.log_file.display().to_string());

    if let Some(snap) = report.telemetry {
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

        let cached_total = snap
            .session_pool
            .idle_cached_sessions
            .saturating_add(snap.session_pool.suspended_mcp_sessions);
        let pool_bar = ui::progress_bar(cached_total, snap.session_pool.max_idle_sessions, 8);
        let mut hot_idle = 0usize;
        let mut warm_idle = 0usize;
        let mut cold_idle = 0usize;
        for sess in &snap.session_pool.sessions {
            match sess.cache_warmth {
                liftgate_types::telemetry::PromptCacheWarmth::Hot => hot_idle += 1,
                liftgate_types::telemetry::PromptCacheWarmth::Warm => warm_idle += 1,
                liftgate_types::telemetry::PromptCacheWarmth::Cold => cold_idle += 1,
                liftgate_types::telemetry::PromptCacheWarmth::Active
                | liftgate_types::telemetry::PromptCacheWarmth::Unknown => {}
            }
        }
        let cache_breakdown = if cached_total > 0 {
            format!(" [kv: {hot_idle} hot, {warm_idle} warm, {cold_idle} cold]")
        } else {
            String::new()
        };
        let pool_detail = format!(
            "{pool_bar}  {} active · {} mcp-wait · {}/{} idle{cache_breakdown}  (reused {}, mcp-cont {}, created {}, evicted {})",
            snap.session_pool.active_leased_sessions,
            snap.session_pool.suspended_mcp_sessions,
            snap.session_pool.idle_cached_sessions,
            snap.session_pool.max_idle_sessions,
            snap.session_pool.total_sessions_reused,
            snap.session_pool.mcp_continuations,
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

        let ct = &snap.cache_telemetry;
        let (cache_status_label, cache_tone) = match ct.total_cache_hit_rate_bps {
            Some(bps) => {
                let tone = if bps >= ct.low_cache_threshold_bps {
                    Tone::Ok
                } else {
                    Tone::Warn
                };
                (format!("{}.{:02}% hit", bps / 100, bps % 100), tone)
            }
            None => ("no turns".to_owned(), Tone::Muted),
        };
        let warm_str = match ct.warm_cache_hit_rate_bps {
            Some(bps) => format!("{}.{:02}%", bps / 100, bps % 100),
            None => "—".to_owned(),
        };
        let cache_detail = format!(
            "warm {warm_str} · cached {} / prompt {} · low-hit {}/{} (<{}%, payload:{})",
            ui::format_tokens(ct.total_cached_read_tokens),
            ui::format_tokens(ct.total_prompt_tokens),
            ct.stored_low_cache_count,
            ct.max_low_cache_records,
            ct.low_cache_threshold_bps / 100,
            if ct.capture_request_details {
                "on"
            } else {
                "off"
            },
        );
        table.add_row(vec![
            ui::accent_cell("Prompt Cache"),
            ui::status_cell(&cache_status_label, cache_tone),
            Cell::new(cache_detail),
        ]);

        if let Some(list) = &report.models {
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
                    TaskPhase::CallingClientTool => "client-tool",
                    TaskPhase::Completed => "done",
                    TaskPhase::Failed => "failed",
                    TaskPhase::Cancelled => "cancelled",
                };
                turns.add_row(vec![
                    ui::dim_cell(&t.task_id),
                    Cell::new(surface),
                    ui::accent_cell(&t.resolved_model),
                    Cell::new(phase),
                    ui::right_cell(ui::format_latency_ms(t.elapsed_ms)),
                ]);
            }
            println!("{turns}");
        }
    } else if report.mode == GatewayStatusMode::Offline {
        println!();
        ui::cmd_hint("Start background gateway", &format!("{bin} up"));
        ui::cmd_hint("Open interactive TUI", &bin);
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
        let cleared = clear_gateway_log(state_paths)?;
        ui::headline(
            Tone::Ok,
            &format!("Cleared gateway log at {}", cleared.display()),
        );
        return Ok(());
    }

    if log_path.is_file() {
        let tail = read_log_tail(&log_path, args.lines)?;
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
        let active_daemon = daemon.as_ref().filter(|d| is_pid_alive(d.pid));
        let mut candidate_addrs = Vec::with_capacity(2);
        if let Some(d) = active_daemon {
            candidate_addrs.push(d.listen);
        } else {
            if let Some(d) = daemon.as_ref() {
                candidate_addrs.push(d.listen);
            }
            let default_listen = ServeArgs::default().listen;
            if !candidate_addrs.contains(&default_listen) {
                candidate_addrs.push(default_listen);
            }
        }

        for listen in candidate_addrs {
            let key = resolve_client_api_key(None, active_daemon, listen);
            let client = GatewayClient::new(listen, key);
            if let Ok(snap) = client.fetch_telemetry().await {
                if !snap.acp_process.recent_stderr.is_empty() {
                    ui::section("`agy acp` Recent Stderr");
                    for entry in &snap.acp_process.recent_stderr {
                        ui::print_log_line(&format!(
                            "{}  {}",
                            ui::format_utc_hms(entry.timestamp_unix_ms),
                            entry.line
                        ));
                    }
                }
                break;
            }
        }
    }

    if !args.follow {
        return Ok(());
    }

    let mut offset = std::fs::metadata(&log_path).map_or(0, |m| m.len());
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        tokio::select! {
            biased;
            _ = &mut ctrl_c => break,
            () = tokio::time::sleep(Duration::from_millis(250)) => {
                if let Ok(meta) = std::fs::metadata(&log_path) {
                    if meta.len() < offset {
                        offset = 0;
                    }
                    if meta.len() > offset
                        && let Ok(mut file) = File::open(&log_path)
                        && file.seek(SeekFrom::Start(offset)).is_ok()
                    {
                        let mut reader = BufReader::new(file);
                        let mut line = String::new();
                        while let Ok(n) = reader.read_line(&mut line) {
                            if n == 0 {
                                break;
                            }
                            offset = offset.saturating_add(u64::try_from(n).unwrap_or(0));
                            ui::print_log_line(line.trim_end_matches(&['\r', '\n'][..]));
                            line.clear();
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

pub async fn run_ui(state_paths: &StatePaths, args: UiArgs) -> Result<(), CliError> {
    let resolved = resolve_ui_target(state_paths, &args).await?;
    if args.print {
        println!("{}", resolved.authenticated_url);
        return Ok(());
    }
    ui::headline(Tone::Ok, "Opening Liftgate Web Dashboard in browser");
    ui::kv_accent("Dashboard URL", &resolved.display_url);
    open_browser(&resolved.authenticated_url)
}

pub async fn run_models(state_paths: &StatePaths, args: ModelsArgs) -> Result<(), CliError> {
    let sp = (!args.json && !args.pick)
        .then(|| Spinner::start("Fetching live Antigravity model catalog…"));
    let report_res = inspect_models(state_paths, &args).await;
    if let Some(s) = sp {
        s.finish_and_clear().await;
    }
    let report = report_res?;

    if args.json {
        let payload = json!({
            "default_model": report.default_model,
            "fast_model": report.fast_model,
            "title_model": report.title_model,
            "count": report.models.len(),
            "models": report.models.iter().map(|m| {
                let (context_window, auto_compact_token_limit) = m.context_limits();
                json!({
                    "id": m.id,
                    "display_name": m.display_name,
                    "context_window": context_window,
                    "auto_compact_token_limit": auto_compact_token_limit,
                })
            }).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_owned())
        );
        return Ok(());
    }

    if args.pick {
        let chosen =
            crate::tui::run_model_picker(&report.models, &report.default_model, &report.fast_model)
                .await?;
        let Some(selected) = chosen else {
            return Ok(());
        };
        let bin = invoked_bin_name();
        if selected == report.default_model {
            ui::headline(
                Tone::Info,
                &format!("Default model is already `{selected}`"),
            );
            return Ok(());
        }
        let managed_running = state_paths
            .read_daemon()?
            .is_some_and(|d| is_pid_alive(d.pid));
        if managed_running {
            run_restart(
                state_paths,
                RestartArgs {
                    default_model: Some(selected.clone()),
                    ..RestartArgs::default()
                },
            )
            .await?;
        } else {
            ui::headline(Tone::Ok, &format!("Selected model `{selected}`"));
            ui::cmd_hint(
                "Start gateway with this model",
                &format!("{bin} up --default-model {selected}"),
            );
        }
        return Ok(());
    }

    ui::banner(
        "Antigravity Model Catalog",
        Some((Tone::Ok, &format!("{} MODELS", report.models.len()))),
        Some(&report.gateway_url),
    );
    ui::kv_accent("Default Model", &report.default_model);
    ui::kv("Fast Subagent", &report.fast_model);
    println!();

    let mut table = ui::new_table(&["Model ID", "Display Name", "Context", "Compact At", "Role"]);
    for m in &report.models {
        let (ctx_win, compact_at) = m.context_limits();
        let mut roles = Vec::new();
        if m.id == report.default_model {
            roles.push("default");
        }
        if m.id == report.fast_model {
            roles.push("fast");
        }
        let role_str = if roles.is_empty() {
            "—".to_owned()
        } else {
            roles.join(", ")
        };
        table.add_row(vec![
            ui::accent_cell(&m.id),
            Cell::new(&m.display_name),
            ui::right_cell(ui::format_tokens(ctx_win)),
            ui::right_cell(ui::format_tokens(compact_at)),
            if roles.is_empty() {
                ui::dim_cell(role_str)
            } else {
                ui::status_cell(&role_str, Tone::Ok)
            },
        ]);
    }
    println!("{table}");
    Ok(())
}

pub async fn run_claude_cli(state_paths: &StatePaths, args: ClaudeArgs) -> Result<i32, CliError> {
    if args.desktop {
        return run_claude_desktop_cli(
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
        .await;
    }
    harness::run_claude(state_paths, args).await
}

pub async fn run_codex_cli(state_paths: &StatePaths, args: CodexArgs) -> Result<i32, CliError> {
    if args.app {
        return run_codex_app_cli(
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
        .await;
    }
    harness::run_codex(state_paths, args).await
}

pub async fn run_opencode_cli(
    state_paths: &StatePaths,
    args: OpencodeArgs,
) -> Result<i32, CliError> {
    harness::run_opencode(state_paths, args).await
}

pub async fn run_omp_cli(state_paths: &StatePaths, args: OmpArgs) -> Result<i32, CliError> {
    harness::run_omp(state_paths, args).await
}

pub async fn run_claude_desktop_cli(
    state_paths: &StatePaths,
    args: ClaudeDesktopArgs,
) -> Result<i32, CliError> {
    let sp = Spinner::start("Preparing isolated Claude Desktop session…");
    let out_res = launch_claude_desktop(state_paths, args).await;
    sp.finish_and_clear().await;
    let out = out_res?;
    print_desktop_launch_outcome(&out);
    Ok(0)
}

pub async fn run_codex_app_cli(
    state_paths: &StatePaths,
    args: CodexAppArgs,
) -> Result<i32, CliError> {
    let sp = Spinner::start("Preparing isolated Codex App session…");
    let out_res = launch_codex_app(state_paths, args).await;
    sp.finish_and_clear().await;
    let out = out_res?;
    print_desktop_launch_outcome(&out);
    Ok(0)
}

fn print_desktop_launch_outcome(out: &DesktopLaunchOutcome) {
    let tone = if out.restored_sessions == Some(0) {
        Tone::Muted
    } else {
        Tone::Ok
    };
    ui::headline(tone, &out.headline);
    for (k, v) in &out.rows {
        if v.starts_with("http://") || v.starts_with("https://") {
            ui::kv_accent(k, v);
        } else {
            ui::kv(k, v);
        }
    }
    if out.watcher_fallback_warn {
        ui::note(
            Tone::Warn,
            "Background cleanup watcher could not detach; run `--restore` after closing if needed.",
        );
    }
}

pub async fn run_config(state_paths: &StatePaths, args: ConfigArgs) -> Result<(), CliError> {
    let print_mode = args.print;
    let sp = (!print_mode && !args.uninstall)
        .then(|| Spinner::start("Synchronizing persistent harness configuration…"));
    let out_res = execute_config_sync(state_paths, &args).await;
    if let Some(s) = sp {
        s.finish_and_clear().await;
    }
    let out = out_res?;

    for item in &out.results {
        match item.action {
            "printed" => {
                println!("# --- {} ---", item.config_path.display());
                if let Some(preview) = &item.rendered_preview {
                    print!("{preview}");
                    if !preview.ends_with('\n') {
                        println!();
                    }
                }
            }
            "removed" => {
                ui::headline(
                    Tone::Ok,
                    &format!(
                        "Removed Liftgate {} configuration from {}",
                        item.target,
                        item.config_path.display()
                    ),
                );
            }
            "noop" => {
                ui::headline(
                    Tone::Muted,
                    &format!(
                        "No Liftgate {} settings found at {}",
                        item.target,
                        item.config_path.display()
                    ),
                );
            }
            _ => {
                let label = match item.target {
                    "claude" => "Updated Claude Code configuration",
                    "codex" => "Updated Codex CLI configuration",
                    "opencode" => "Updated OpenCode configuration",
                    "omp" => "Updated Oh My Pi (omp) configuration",
                    _ => "Updated harness configuration",
                };
                ui::headline(Tone::Ok, label);
                ui::kv_accent("Config File", &item.config_path.display().to_string());
                if let Some(cat) = &item.catalog_path {
                    let aux_label = if item.target == "omp" {
                        "Provider File"
                    } else {
                        "Model Catalog"
                    };
                    ui::kv(aux_label, &cat.display().to_string());
                }
                if let (Some(p), Some(f)) = (&item.primary_model, &item.fast_model) {
                    ui::kv("Model Routing", &format!("primary={p} · fast={f}"));
                }
                if item.project_untrusted_codex_note {
                    ui::note(
                        Tone::Info,
                        "Tip: project `.codex/config.toml` applies once the directory is trusted in Codex (`lg config --global` syncs user-wide).",
                    );
                }
                if item.require_env_key_warn {
                    ui::note(
                        Tone::Warn,
                        "Gateway requires an API key: export `AGY_GATEWAY_API_KEY` in shells invoking `codex` directly.",
                    );
                }
            }
        }
    }
    Ok(())
}

pub async fn run_doctor(state_paths: &StatePaths, args: DoctorArgs) -> Result<(), CliError> {
    let bin = invoked_bin_name();
    let sp = (!args.json).then(|| Spinner::start("Running Liftgate environment diagnostics…"));
    let report_res = inspect_doctor(state_paths, &args, &bin).await;
    if let Some(s) = sp {
        s.finish_and_clear().await;
    }
    let report = report_res?;

    if args.json {
        let payload = json!({
            "ok": report.ok,
            "warnings": report.warnings,
            "checks": report.checks.iter().map(|c| json!({
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

    let summary_pill = if !report.ok {
        (Tone::Err, "ACTION REQUIRED")
    } else if report.warnings {
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
    for row in &report.checks {
        table.add_row(vec![
            ui::accent_cell(row.name),
            ui::status_cell(row.status, Tone::from(row.tone)),
            Cell::new(&row.detail),
        ]);
    }
    println!("{table}");

    if report.agy_missing {
        println!();
        ui::cmd_hint("Install Antigravity CLI", &format!("{bin} agy install"));
    } else if report.login_needed {
        println!();
        ui::cmd_hint("Sign in with Google Account", &format!("{bin} agy login"));
    }
    Ok(())
}

pub async fn run_upgrade(state_paths: &StatePaths, args: UpgradeArgs) -> Result<(), CliError> {
    let current_exe = std::env::current_exe().map_err(|source| CliError::StateIo {
        path: PathBuf::from("current_exe"),
        source,
    })?;
    let msg = if args.check {
        format!("Checking GitHub Releases ({})…", args.repo)
    } else {
        format!("Preparing release upgrade from {}…", args.repo)
    };
    let started = Instant::now();
    let (prog_tx, progress_display) = if args.json {
        (None, None)
    } else {
        let (tx, rx) = watch::channel(TransferProgress::checking(msg));
        (Some(tx), Some(TransferProgressDisplay::start(rx)))
    };
    let out_res = execute_upgrade_with_progress(state_paths, &args, &current_exe, prog_tx).await;
    let stats = match progress_display {
        Some(display) => display.finish_and_clear().await,
        None => ui::TransferDisplayStats::default(),
    };
    let out = out_res?;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&out).unwrap_or_else(|_| "{}".to_owned())
        );
        return Ok(());
    }

    match out.status {
        "update_available" => {
            ui::headline(
                Tone::Warn,
                &format!(
                    "Update available: v{} -> v{} ({})",
                    out.current_version, out.target_version, out.tag
                ),
            );
            ui::cmd_hint(
                "Install verified release now",
                &format!("{} upgrade", invoked_bin_name()),
            );
        }
        "up_to_date" => {
            ui::headline(
                Tone::Ok,
                &format!(
                    "Liftgate v{} is already up to date ({})",
                    out.current_version, out.target_triple
                ),
            );
        }
        "ahead_of_release" => {
            ui::headline(
                Tone::Info,
                &format!(
                    "Local build v{} is newer than latest release {}",
                    out.current_version, out.tag
                ),
            );
        }
        "upgraded" => {
            ui::headline(
                Tone::Ok,
                &format!(
                    "Upgraded Liftgate v{} -> v{} ({})",
                    out.current_version, out.target_version, out.target_triple
                ),
            );
            let transferred = out
                .archive_size_bytes
                .filter(|&b| b > 0)
                .or((stats.downloaded_bytes > 0).then_some(stats.downloaded_bytes));
            if let Some(size) = transferred {
                let avg_rate = size as f64 / started.elapsed().as_secs_f64().max(0.001);
                ui::kv(
                    "Transfer",
                    &format!(
                        "{} in {} (avg {})",
                        ui::format_bytes(size),
                        ui::format_latency_ms(elapsed_ms),
                        ui::format_transfer_rate(avg_rate),
                    ),
                );
            }
            for bin_path in &out.installed_binaries {
                ui::kv_accent("Installed", bin_path);
            }
            if let Some(sha) = &out.sha256 {
                ui::kv("SHA-256", sha);
            }
            if out.daemon_restarted {
                ui::note(
                    Tone::Ok,
                    "Managed background gateway was restarted onto the new version automatically.",
                );
            }
        }
        _ => {
            ui::headline(Tone::Info, &format!("Release status: {}", out.status));
        }
    }
    ui::kv_accent("Release URL", &out.release_url);
    Ok(())
}

pub async fn run_proxy(state_paths: &StatePaths, args: ProxyArgs) -> Result<(), CliError> {
    let sp = (args.test && !args.json)
        .then(|| Spinner::start("Testing Antigravity upstream connectivity…"));
    let out_res = execute_proxy(state_paths, &args).await;
    if let Some(s) = sp {
        s.finish_and_clear().await;
    }
    let out = out_res?;

    if args.json {
        let probe_json = out.connectivity_probe.as_ref().map(|p| {
            if p.ok {
                json!({ "ok": true, "latency_ms": p.latency_ms, "latest_agy_version": p.latest_agy_version })
            } else {
                json!({ "ok": false, "latency_ms": p.latency_ms, "error": p.error })
            }
        });
        let payload = json!({
            "updated": out.updated,
            "configured_proxy": out.configured_proxy.as_ref().map(|p| p.redacted_display()),
            "env_override": out.env_override,
            "effective_proxy": out.effective_proxy.as_ref().map(|p| p.redacted_display()),
            "default_mode": out.effective_proxy.is_none(),
            "daemon_restarted": out.daemon_restarted,
            "connectivity_test": probe_json,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_owned())
        );
        if let Some(probe) = &out.connectivity_probe
            && !probe.ok
        {
            return Err(CliError::AgyProxyProbeFailed {
                detail: probe
                    .error
                    .clone()
                    .unwrap_or_else(|| "upstream connectivity test failed".to_owned()),
            });
        }
        return Ok(());
    }

    let bin = invoked_bin_name();
    if out.updated {
        match &out.configured_proxy {
            Some(p) => ui::headline(
                Tone::Ok,
                &format!("Configured forced `agy` proxy -> {}", p.redacted_display()),
            ),
            None => ui::headline(
                Tone::Ok,
                "Cleared `agy` proxy (restored default: direct connection, no proxy injected)",
            ),
        }
    } else {
        let badge = if out.effective_proxy.is_some() {
            (Tone::Info, "FORCED PROXY")
        } else {
            (Tone::Muted, "DIRECT · NO PROXY")
        };
        ui::banner("Antigravity Proxy Policy", Some(badge), None);
    }

    ui::kv_accent(
        "Saved Setting",
        &out.configured_proxy.as_ref().map_or_else(
            || "none (default · no proxy injected into `agy`)".to_owned(),
            |p| p.redacted_display().into_owned(),
        ),
    );
    if let Some(env_val) = &out.env_override {
        ui::kv("AGY_PROXY Env", env_val);
    }
    ui::kv("Settings File", &out.settings_file.display().to_string());
    if out.daemon_restarted {
        ui::note(
            Tone::Ok,
            "Restarted background gateway onto updated proxy policy.",
        );
    }

    if let Some(probe) = out.connectivity_probe {
        if probe.ok {
            ui::headline(
                Tone::Ok,
                &format!(
                    "Upstream reachable in {} (latest `agy` v{})",
                    ui::format_latency_ms(probe.latency_ms),
                    probe.latest_agy_version.as_deref().unwrap_or("unknown")
                ),
            );
        } else {
            return Err(CliError::AgyProxyProbeFailed {
                detail: probe
                    .error
                    .unwrap_or_else(|| "upstream connectivity test failed".to_owned()),
            });
        }
    }

    if !out.updated {
        println!();
        ui::cmd_hint("Set local port shorthand", &format!("{bin} proxy 7890"));
        ui::cmd_hint(
            "Set explicit HTTP/SOCKS5 proxy",
            &format!("{bin} proxy socks5://127.0.0.1:1080"),
        );
        ui::cmd_hint("Disable proxy (default)", &format!("{bin} proxy off"));
    }
    Ok(())
}

pub async fn run_agy(state_paths: &StatePaths, args: AgyArgs) -> Result<i32, CliError> {
    match args.action {
        None => {
            run_agy_status(
                state_paths,
                args.agy_bin,
                AgyStatusArgs::default(),
                args.json,
            )
            .await?;
            Ok(0)
        }
        Some(AgyCommand::Status(st_args)) => {
            run_agy_status(state_paths, args.agy_bin, st_args, args.json).await?;
            Ok(0)
        }
        Some(AgyCommand::Install(inst_args)) => {
            let _ = run_agy_install(state_paths, inst_args, args.json).await?;
            Ok(0)
        }
        Some(AgyCommand::Update(upd_args)) => {
            let _ = run_agy_update(state_paths, args.agy_bin, upd_args, args.json).await?;
            Ok(0)
        }
        Some(AgyCommand::Login(login_args)) => {
            run_agy_login(state_paths, args.agy_bin, login_args).await
        }
        Some(AgyCommand::AutoUpdate(auto_args)) => {
            let out = execute_agy_auto_update(state_paths, &auto_args).await?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&out).unwrap_or_else(|_| "{}".to_owned())
                );
            } else {
                let state_word = if out.agy_auto_update {
                    "enabled"
                } else {
                    "disabled"
                };
                ui::headline(Tone::Ok, &format!("Idle `agy` auto-update is {state_word}"));
                ui::kv("Settings File", &out.settings_file.display().to_string());
                if out.daemon_restarted {
                    ui::note(Tone::Ok, "Restarted background gateway onto new policy.");
                }
            }
            Ok(0)
        }
        Some(AgyCommand::Proxy(mut proxy_args)) => {
            if args.json {
                proxy_args.json = true;
            }
            run_proxy(state_paths, proxy_args).await?;
            Ok(0)
        }
        Some(AgyCommand::Models) => {
            run_agy_passthrough(state_paths, args.agy_bin, &[OsString::from("models")]).await
        }
        Some(AgyCommand::Mcp(fwd)) => {
            let mut argv = Vec::with_capacity(fwd.args.len() + 1);
            argv.push(OsString::from("mcp"));
            argv.extend(fwd.args);
            run_agy_passthrough(state_paths, args.agy_bin, &argv).await
        }
        Some(AgyCommand::Plugin(fwd)) => {
            let mut argv = Vec::with_capacity(fwd.args.len() + 1);
            argv.push(OsString::from("plugin"));
            argv.extend(fwd.args);
            run_agy_passthrough(state_paths, args.agy_bin, &argv).await
        }
        Some(AgyCommand::Changelog) => {
            run_agy_passthrough(state_paths, args.agy_bin, &[OsString::from("changelog")]).await
        }
        Some(AgyCommand::Run(fwd)) => {
            run_agy_passthrough(state_paths, args.agy_bin, &fwd.args).await
        }
        Some(AgyCommand::External(ext)) => {
            run_agy_passthrough(state_paths, args.agy_bin, &ext).await
        }
    }
}

async fn run_agy_status(
    state_paths: &StatePaths,
    explicit_agy_bin: Option<PathBuf>,
    args: AgyStatusArgs,
    json_out: bool,
) -> Result<(), CliError> {
    let sp = (!json_out).then(|| Spinner::start("Inspecting Antigravity CLI (`agy`) environment…"));
    let report_res = inspect_agy_status(state_paths, explicit_agy_bin, &args).await;
    if let Some(s) = sp {
        s.finish_and_clear().await;
    }
    let report = report_res?;

    if json_out {
        let auth_detail = match &report.auth_status {
            AgyAuthStatus::SignedIn {
                model_count,
                source,
            } => json!({ "signed_in": true, "model_count": model_count, "source": source }),
            AgyAuthStatus::NotSignedIn { detail } => {
                json!({ "signed_in": false, "detail": detail })
            }
            AgyAuthStatus::NetworkError { detail } => {
                json!({ "signed_in": false, "network_error": true, "detail": detail })
            }
            AgyAuthStatus::NotInstalled => json!({ "signed_in": false }),
            AgyAuthStatus::Skipped => json!({ "skipped": true }),
        };
        let payload = json!({
            "installed": report.installed,
            "binary_path": report.binary_path,
            "version": report.version,
            "latest_version": report.latest_version,
            "update_available": report.update_available,
            "auth_status": report.auth_status.status_slug(),
            "auth": auth_detail,
            "proxy": report.effective_proxy.as_ref().map(|p| p.redacted_display()),
            "auto_update_enabled": report.auto_update_enabled,
            "daemon_auto_update": report.live_telemetry.as_ref().map(|t| {
                json!({
                    "enabled": t.acp_process.auto_update_enabled,
                    "last_check_unix_ms": t.acp_process.last_update_check_unix_ms,
                    "last_auto_update_unix_ms": t.acp_process.last_auto_update_unix_ms,
                    "last_auto_updated_from": t.acp_process.last_auto_updated_from,
                    "note": t.acp_process.auto_update_note,
                })
            }),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_owned())
        );
        return Ok(());
    }

    let bin = invoked_bin_name();
    let pill = match (&report.binary_path, &report.auth_status) {
        (None, _) => (Tone::Err, "NOT INSTALLED"),
        (Some(_), AgyAuthStatus::SignedIn { .. }) => (Tone::Ok, "READY · SIGNED IN"),
        (Some(_), AgyAuthStatus::NotSignedIn { .. }) => (Tone::Warn, "LOGIN REQUIRED"),
        (Some(_), AgyAuthStatus::NetworkError { .. }) => (Tone::Warn, "NETWORK / PROXY ISSUE"),
        (Some(_), AgyAuthStatus::NotInstalled | AgyAuthStatus::Skipped) => {
            (Tone::Info, "INSTALLED")
        }
    };
    let subtitle = report.version.as_deref().map(|v| format!("agy v{v}"));
    ui::banner("Antigravity CLI (`agy`)", Some(pill), subtitle.as_deref());
    println!();

    let mut table = ui::new_table(&["Item", "Status", "Details"]);
    match &report.binary_path {
        Some(path) => table.add_row(vec![
            ui::accent_cell("Executable"),
            ui::status_cell("installed", Tone::Ok),
            Cell::new(path.display().to_string()),
        ]),
        None => table.add_row(vec![
            ui::accent_cell("Executable"),
            ui::status_cell("missing", Tone::Err),
            Cell::new(format!(
                "not found in PATH — run `{bin} agy install` to install automatically"
            )),
        ]),
    };

    let (ver_status, ver_tone, ver_detail) = match (&report.version, &report.latest_version) {
        (Some(cur), Some(lat)) if report.update_available => (
            "outdated",
            Tone::Warn,
            format!("v{cur} -> v{lat} available (run `{bin} agy update`)"),
        ),
        (Some(cur), Some(_)) => (
            "up to date",
            Tone::Ok,
            format!("v{cur} (latest official build)"),
        ),
        (Some(cur), None) => ("installed", Tone::Ok, format!("v{cur}")),
        (None, Some(lat)) => (
            "not installed",
            Tone::Warn,
            format!("latest upstream build is v{lat}"),
        ),
        (None, None) => ("unknown", Tone::Muted, "—".to_owned()),
    };
    table.add_row(vec![
        ui::accent_cell("CLI Version"),
        ui::status_cell(ver_status, ver_tone),
        Cell::new(ver_detail),
    ]);

    let (auth_label, auth_tone, auth_detail) = match &report.auth_status {
        AgyAuthStatus::SignedIn {
            model_count,
            source,
        } => (
            "signed in",
            Tone::Ok,
            format!("{model_count} account models unlocked (verified via {source})"),
        ),
        AgyAuthStatus::NotSignedIn { detail } => (
            "login needed",
            Tone::Warn,
            format!("{detail} — run `{bin} agy login`"),
        ),
        AgyAuthStatus::NetworkError { detail } => (
            "unreachable",
            Tone::Err,
            format!("{detail} — check `{bin} proxy`"),
        ),
        AgyAuthStatus::NotInstalled => {
            ("unavailable", Tone::Muted, "install `agy` first".to_owned())
        }
        AgyAuthStatus::Skipped => ("skipped", Tone::Muted, "--no-auth-check".to_owned()),
    };
    table.add_row(vec![
        ui::accent_cell("Google Account"),
        ui::status_cell(auth_label, auth_tone),
        Cell::new(auth_detail),
    ]);

    match &report.effective_proxy {
        Some(proxy) => table.add_row(vec![
            ui::accent_cell("Forced Proxy"),
            ui::status_cell("active", Tone::Info),
            Cell::new(format!(
                "{}  (disable with `{bin} proxy off`)",
                proxy.redacted_display()
            )),
        ]),
        None => table.add_row(vec![
            ui::accent_cell("Forced Proxy"),
            ui::status_cell("none", Tone::Ok),
            Cell::new(format!(
                "direct connection by default (set with `{bin} proxy 7890`)"
            )),
        ]),
    };

    let mut auto_detail = if report.auto_update_enabled {
        format!(
            "checks & updates `agy` automatically while gateway is idle (`{bin} agy auto-update off` to disable)"
        )
    } else {
        format!("disabled (`{bin} agy auto-update on` to re-enable)")
    };
    if let Some(snap) = &report.live_telemetry
        && let Some(prev) = &snap.acp_process.last_auto_updated_from
    {
        let cur = snap
            .acp_process
            .agent_version
            .as_deref()
            .unwrap_or("latest");
        auto_detail = format!("{auto_detail} · last updated v{prev} -> v{cur}");
    }
    table.add_row(vec![
        ui::accent_cell("Idle Auto-Update"),
        ui::status_cell(
            if report.auto_update_enabled {
                "enabled"
            } else {
                "disabled"
            },
            if report.auto_update_enabled {
                Tone::Ok
            } else {
                Tone::Muted
            },
        ),
        Cell::new(auto_detail),
    ]);

    println!("{table}");
    println!();

    if report.binary_path.is_none() {
        ui::cmd_hint(
            "Download & install `agy` now",
            &format!("{bin} agy install"),
        );
    } else if !report.auth_status.is_signed_in()
        && !matches!(report.auth_status, AgyAuthStatus::Skipped)
    {
        ui::cmd_hint("Sign in with Google account", &format!("{bin} agy login"));
    } else if report.update_available {
        ui::cmd_hint(
            "Update `agy` to latest version",
            &format!("{bin} agy update"),
        );
    }
    Ok(())
}

pub async fn run_agy_install(
    state_paths: &StatePaths,
    args: AgyInstallArgs,
    json_out: bool,
) -> Result<AgyInstallOutcome, CliError> {
    let msg = if args.check {
        "Querying official Antigravity CLI manifest…"
    } else {
        "Preparing Antigravity CLI (`agy`) download…"
    };
    let started = Instant::now();
    let (prog_tx, progress_display) = if json_out {
        (None, None)
    } else {
        let (tx, rx) = watch::channel(TransferProgress::checking(msg));
        (Some(tx), Some(TransferProgressDisplay::start(rx)))
    };
    let out_res = execute_agy_install_with_progress(state_paths, &args, prog_tx).await;
    let stats = match progress_display {
        Some(display) => display.finish_and_clear().await,
        None => ui::TransferDisplayStats::default(),
    };
    let out = out_res?;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&out).unwrap_or_else(|_| "{}".to_owned())
        );
        return Ok(out);
    }

    match out.status {
        AgyInstallStatus::Checked => {
            ui::headline(
                Tone::Info,
                &format!(
                    "Official `agy` latest version: v{} ({})",
                    out.version, out.platform
                ),
            );
        }
        AgyInstallStatus::AlreadyInstalled => {
            ui::headline(
                Tone::Ok,
                &format!(
                    "Antigravity CLI v{} is already installed and up to date",
                    out.version
                ),
            );
        }
        AgyInstallStatus::Installed => {
            ui::headline(
                Tone::Ok,
                &format!(
                    "Installed official Antigravity CLI v{} (SHA-512 verified)",
                    out.version
                ),
            );
            if stats.downloaded_bytes > 0 {
                let avg_rate =
                    stats.downloaded_bytes as f64 / started.elapsed().as_secs_f64().max(0.001);
                ui::kv(
                    "Transfer",
                    &format!(
                        "{} in {} (avg {})",
                        ui::format_bytes(stats.downloaded_bytes),
                        ui::format_latency_ms(elapsed_ms),
                        ui::format_transfer_rate(avg_rate),
                    ),
                );
            } else {
                ui::kv("Completed In", &ui::format_latency_ms(elapsed_ms));
            }
        }
    }
    ui::kv_accent("Binary Path", &out.binary_path.display().to_string());
    if !out.dir_in_path {
        ui::note(
            Tone::Info,
            "Directory is outside PATH; Liftgate recorded `agy_bin` in `~/.liftgate/settings.json`.",
        );
    }
    if out.daemon_restarted {
        ui::note(
            Tone::Ok,
            "Restarted background gateway onto newly installed `agy`.",
        );
    }
    Ok(out)
}

async fn run_agy_update(
    state_paths: &StatePaths,
    explicit_agy_bin: Option<PathBuf>,
    args: AgyUpdateArgs,
    json_out: bool,
) -> Result<AgyUpdateOutcome, CliError> {
    let msg = if args.check {
        "Checking official Antigravity CLI release manifest…"
    } else {
        "Preparing Antigravity CLI (`agy`) update…"
    };
    let started = Instant::now();
    let (prog_tx, progress_display) = if json_out {
        (None, None)
    } else {
        let (tx, rx) = watch::channel(TransferProgress::checking(msg));
        (Some(tx), Some(TransferProgressDisplay::start(rx)))
    };
    let out_res =
        execute_agy_update_with_progress(state_paths, explicit_agy_bin, &args, prog_tx).await;
    let stats = match progress_display {
        Some(display) => display.finish_and_clear().await,
        None => ui::TransferDisplayStats::default(),
    };
    let out = out_res?;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&out).unwrap_or_else(|_| "{}".to_owned())
        );
        return Ok(out);
    }

    match out.status {
        AgyUpdateStatus::UpdateAvailable => {
            ui::headline(
                Tone::Warn,
                &format!(
                    "Update available: v{} -> v{}",
                    out.previous_version.as_deref().unwrap_or("unknown"),
                    out.latest_version
                ),
            );
        }
        AgyUpdateStatus::UpToDate => {
            ui::headline(
                Tone::Ok,
                &format!("`agy` v{} is already up to date", out.latest_version),
            );
        }
        AgyUpdateStatus::Updated | AgyUpdateStatus::InstalledFresh => {
            ui::headline(
                Tone::Ok,
                &format!(
                    "Updated `agy` to v{} (SHA-512 verified)",
                    out.latest_version
                ),
            );
            if stats.downloaded_bytes > 0 {
                let avg_rate =
                    stats.downloaded_bytes as f64 / started.elapsed().as_secs_f64().max(0.001);
                ui::kv(
                    "Transfer",
                    &format!(
                        "{} in {} (avg {})",
                        ui::format_bytes(stats.downloaded_bytes),
                        ui::format_latency_ms(elapsed_ms),
                        ui::format_transfer_rate(avg_rate),
                    ),
                );
            } else {
                ui::kv("Completed In", &ui::format_latency_ms(elapsed_ms));
            }
        }
    }
    ui::kv_accent("Binary Path", &out.binary_path.display().to_string());
    if out.daemon_restarted {
        ui::note(Tone::Ok, "Restarted background gateway onto updated `agy`.");
    }
    Ok(out)
}

pub async fn run_agy_login(
    state_paths: &StatePaths,
    explicit_agy_bin: Option<PathBuf>,
    args: AgyLoginArgs,
) -> Result<i32, CliError> {
    let settings = state_paths.read_settings()?;
    let cli_proxy = args
        .proxy
        .clone()
        .or_else(|| std::env::var("AGY_PROXY").ok());
    let effective_proxy = resolve_effective_agy_proxy(cli_proxy.as_deref(), &settings)?;

    let agy_bin = match resolve_effective_agy_bin(explicit_agy_bin, &settings) {
        Some(found) => found,
        None => {
            if args.install || prompt_install_missing_agy()? {
                run_agy_install(
                    state_paths,
                    AgyInstallArgs {
                        proxy: args.proxy.clone(),
                        ..AgyInstallArgs::default()
                    },
                    false,
                )
                .await?
                .binary_path
            } else {
                return Err(CliError::AgyBinaryNotFound);
            }
        }
    };

    ui::headline(
        Tone::Action,
        "Launching official Antigravity Google Sign-In (`agy acp --login`)…",
    );
    if let Some(p) = &effective_proxy {
        ui::kv("Forced Proxy", &p.redacted_display());
    }

    let exit_code = spawn_interactive_agy(
        &agy_bin,
        &[OsString::from("acp"), OsString::from("--login")],
        effective_proxy.as_ref(),
    )
    .await?;

    if exit_code == 0 {
        let auth = probe_agy_auth(state_paths, &agy_bin, effective_proxy.as_ref()).await;
        if let AgyAuthStatus::SignedIn { model_count, .. } = auth {
            ui::headline(
                Tone::Ok,
                &format!("Signed in successfully ({model_count} Antigravity models available)"),
            );
            if !args.no_restart
                && let Some(d) = state_paths.read_daemon()?
                && is_pid_alive(d.pid)
            {
                let _ = restart_daemon(state_paths, RestartArgs::default()).await?;
                ui::note(
                    Tone::Ok,
                    "Restarted background gateway to refresh authenticated model catalog.",
                );
            }
        }
    }
    Ok(exit_code)
}

fn prompt_install_missing_agy() -> Result<bool, CliError> {
    if !stdin().is_terminal() || !stdout().is_terminal() {
        return Ok(false);
    }
    print!("`agy` is not installed. Download & verify official Antigravity CLI now? [Y/n] ");
    let _ = stdout().flush();
    let mut line = String::new();
    let _ = stdin().read_line(&mut line);
    let ans = line.trim();
    Ok(ans.is_empty() || ans.eq_ignore_ascii_case("y") || ans.eq_ignore_ascii_case("yes"))
}

pub fn run_alias(args: AliasArgs) -> Result<(), CliError> {
    let out = execute_alias(&args)?;
    match out.status {
        AliasActionStatus::Installed => {
            ui::headline(
                Tone::Ok,
                &format!(
                    "Installed `{}` command alias -> {}",
                    out.stem,
                    out.target_path.display()
                ),
            );
            if let Some(strategy) = out.strategy {
                ui::kv("Link Strategy", strategy);
            }
            if !out.dir_in_path
                && let Some(parent) = out.target_path.parent()
            {
                ui::note(
                    Tone::Warn,
                    &format!(
                        "Add `{}` to PATH to invoke `{}` from any shell.",
                        parent.display(),
                        out.stem
                    ),
                );
            }
        }
        AliasActionStatus::AlreadyActive => {
            ui::headline(
                Tone::Ok,
                &format!(
                    "Alias `{}` already points to this executable ({})",
                    out.stem,
                    out.target_path.display()
                ),
            );
        }
        AliasActionStatus::Removed => {
            ui::headline(
                Tone::Ok,
                &format!(
                    "Removed `{}` alias at {}",
                    out.stem,
                    out.target_path.display()
                ),
            );
        }
        AliasActionStatus::NotPresent => {
            ui::headline(
                Tone::Muted,
                &format!(
                    "No `{}` alias found at {}",
                    out.stem,
                    out.target_path.display()
                ),
            );
        }
    }
    Ok(())
}
