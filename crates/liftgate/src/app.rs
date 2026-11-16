#[path = "render.rs"]
pub(crate) mod render;
#[path = "tui.rs"]
pub(crate) mod tui;
#[path = "ui.rs"]
pub(crate) mod ui;

use std::ffi::OsString;
use std::io::{IsTerminal as _, stdin, stdout};
use std::process::ExitCode;

use clap::{CommandFactory as _, FromArgMatches as _};
use liftgate_core::{
    agy_manager,
    cli::{
        AgyArgs, AgyCommand, AgyInstallArgs, AgyLoginArgs, AgyStatusArgs, ClaudeArgs,
        ClaudeDesktopArgs, Cli, CodexAppArgs, CodexArgs, Commands, ConfigArgs, DoctorArgs,
        ModelsArgs, RestartArgs, ServeArgs, StartArgs, TopArgs, UiArgs, UpgradeArgs,
        invoked_bin_name, normalize_harness_args,
    },
    daemon, desktop,
    error::CliError,
    serve::{self, ServeReadyInfo},
    state::{StatePaths, resolve_effective_agy_bin},
    upgrade,
};
use liftgate_server::job::install_panic_cleanup_hook;
use tracing_subscriber::EnvFilter;

use crate::tui::CockpitAction;

pub async fn entry() -> ExitCode {
    install_panic_cleanup_hook();
    upgrade::sweep_stale_backups();

    let raw_args: Vec<OsString> = std::env::args_os().collect();
    let bin_name = invoked_bin_name();
    let bare_interactive_invocation =
        raw_args.len() <= 1 && stdin().is_terminal() && stdout().is_terminal();

    let normalized_args = normalize_harness_args(raw_args);
    let mut cmd = Cli::command().bin_name(&bin_name);
    let matches = match cmd.try_get_matches_from_mut(normalized_args) {
        Ok(m) => m,
        Err(err) => {
            let _ = err.print();
            return ExitCode::from(u8::try_from(err.exit_code()).unwrap_or(2));
        }
    };
    let parsed = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(err) => {
            let _ = err.print();
            return ExitCode::from(u8::try_from(err.exit_code()).unwrap_or(2));
        }
    };

    let is_foreground_server = matches!(parsed.command, Some(Commands::Serve(_)))
        || (parsed.command.is_none() && !bare_interactive_invocation);
    let default_level = if is_foreground_server { "info" } else { "warn" };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(ui::use_color_stderr())
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    match dispatch(parsed, bare_interactive_invocation, &bin_name).await {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(err) => {
            if is_foreground_server {
                tracing::error!(error = %err, "gateway exited with error");
                let mut source = std::error::Error::source(&err);
                while let Some(cause) = source {
                    tracing::error!(cause = %cause, "caused by");
                    source = cause.source();
                }
            } else {
                ui::print_cli_error(&err, &bin_name);
            }
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(
    cli: Cli,
    bare_interactive_invocation: bool,
    bin_name: &str,
) -> Result<i32, CliError> {
    let state_paths = StatePaths::resolve(cli.state_dir)?;
    agy_manager::sweep_stale_agy_backups(&state_paths);
    if !matches!(
        cli.command,
        Some(Commands::InternalDesktopWatch(_) | Commands::InternalDesktopMsixLaunch(_))
    ) {
        desktop::sweep_stale_desktop_sessions(&state_paths);
    }

    match cli.command {
        None if bare_interactive_invocation => {
            let Some(action) = tui::run_cockpit(&state_paths).await? else {
                return Ok(0);
            };
            dispatch_cockpit_action(&state_paths, action).await
        }
        None => {
            run_foreground_serve(&state_paths, cli.serve).await?;
            Ok(0)
        }
        Some(Commands::Top(args)) => {
            tui::run_top(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Serve(args)) => {
            run_foreground_serve(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Start(args)) => {
            render::run_start(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Stop) => {
            render::run_stop(&state_paths).await?;
            Ok(0)
        }
        Some(Commands::Restart(args)) => {
            render::run_restart(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Status(args)) => {
            render::run_status(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Logs(args)) => {
            render::run_logs(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Ui(args)) => {
            render::run_ui(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Models(args)) => {
            render::run_models(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Claude(args)) => render::run_claude_cli(&state_paths, args).await,
        Some(Commands::ClaudeDesktop(args)) => {
            render::run_claude_desktop_cli(&state_paths, args).await
        }
        Some(Commands::Codex(args)) => render::run_codex_cli(&state_paths, args).await,
        Some(Commands::CodexApp(args)) => render::run_codex_app_cli(&state_paths, args).await,
        Some(Commands::InternalDesktopWatch(args)) => {
            desktop::run_desktop_watch(args).await?;
            Ok(0)
        }
        Some(Commands::InternalDesktopMsixLaunch(args)) => {
            desktop::run_desktop_msix_launch(args)?;
            Ok(0)
        }
        Some(Commands::Config(args)) => {
            render::run_config(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Doctor(args)) => {
            render::run_doctor(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Upgrade(args)) => {
            render::run_upgrade(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Agy(args)) => render::run_agy(&state_paths, args).await,
        Some(Commands::Proxy(args)) => {
            render::run_proxy(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Alias(args)) => {
            render::run_alias(args)?;
            Ok(0)
        }
        Some(Commands::Completions(args)) => {
            let target_bin = args.bin.unwrap_or_else(|| bin_name.to_owned());
            let mut cmd = Cli::command();
            clap_complete::generate(args.shell, &mut cmd, target_bin, &mut stdout());
            Ok(0)
        }
    }
}

async fn run_foreground_serve(state_paths: &StatePaths, args: ServeArgs) -> Result<(), CliError> {
    let banner_cb = |info: &ServeReadyInfo| {
        ui::print_serve_ready_banner(
            &info.listen_url,
            &info.dashboard_url,
            &info.default_model,
            &info.acp_mode,
            info.auth_enabled,
            info.full_ui_embedded,
            info.agy_proxy_display.as_deref(),
            info.agy_auto_update,
        );
    };
    serve::run_serve_with_hook(state_paths, args, Some(&banner_cb)).await
}

async fn dispatch_cockpit_action(
    state_paths: &StatePaths,
    action: CockpitAction,
) -> Result<i32, CliError> {
    match action {
        CockpitAction::LaunchCodex => {
            render::run_codex_cli(state_paths, CodexArgs::default_interactive()).await
        }
        CockpitAction::LaunchClaude => {
            render::run_claude_cli(state_paths, ClaudeArgs::default_interactive()).await
        }
        CockpitAction::LaunchCodexApp => {
            render::run_codex_app_cli(state_paths, CodexAppArgs::default_interactive()).await
        }
        CockpitAction::LaunchClaudeDesktop => {
            render::run_claude_desktop_cli(state_paths, ClaudeDesktopArgs::default_interactive())
                .await
        }
        CockpitAction::OpenTop => {
            tui::run_top(
                state_paths,
                TopArgs {
                    listen: None,
                    api_key: None,
                    interval_ms: 1_000,
                    auto_start: false,
                },
            )
            .await?;
            Ok(0)
        }
        CockpitAction::PickModel => {
            render::run_models(
                state_paths,
                ModelsArgs {
                    listen: None,
                    api_key: None,
                    pick: true,
                    json: false,
                    no_start: false,
                },
            )
            .await?;
            Ok(0)
        }
        CockpitAction::OpenWebUi => {
            render::run_ui(
                state_paths,
                UiArgs {
                    listen: None,
                    api_key: None,
                    print: false,
                    no_start: false,
                },
            )
            .await?;
            Ok(0)
        }
        CockpitAction::StartOrRestart => {
            let managed_running = state_paths
                .read_daemon()?
                .is_some_and(|d| daemon::is_pid_alive(d.pid));
            if managed_running {
                render::run_restart(state_paths, RestartArgs::default()).await?;
            } else {
                render::run_start(
                    state_paths,
                    StartArgs {
                        serve: ServeArgs::default(),
                        force: false,
                        open: false,
                    },
                )
                .await?;
            }
            Ok(0)
        }
        CockpitAction::AgyManage => {
            let settings = state_paths.read_settings()?;
            if resolve_effective_agy_bin(None, &settings).is_none() {
                let _ =
                    render::run_agy_install(state_paths, AgyInstallArgs::default(), false).await?;
                Ok(0)
            } else {
                render::run_agy(
                    state_paths,
                    AgyArgs {
                        agy_bin: None,
                        json: false,
                        action: Some(AgyCommand::Status(AgyStatusArgs::default())),
                    },
                )
                .await
            }
        }
        CockpitAction::AgyLogin => {
            render::run_agy_login(
                state_paths,
                None,
                AgyLoginArgs {
                    install: true,
                    proxy: None,
                    no_restart: false,
                },
            )
            .await
        }
        CockpitAction::SyncConfig => {
            render::run_config(state_paths, ConfigArgs::default_local()).await?;
            Ok(0)
        }
        CockpitAction::Doctor => {
            render::run_doctor(
                state_paths,
                DoctorArgs {
                    agy_bin: None,
                    listen: None,
                    api_key: None,
                    json: false,
                },
            )
            .await?;
            Ok(0)
        }
        CockpitAction::Upgrade => {
            render::run_upgrade(
                state_paths,
                UpgradeArgs {
                    repo: "acking-you/liftgate".to_owned(),
                    ..UpgradeArgs::default()
                },
            )
            .await?;
            Ok(0)
        }
        CockpitAction::StopGateway => {
            render::run_stop(state_paths).await?;
            Ok(0)
        }
    }
}
