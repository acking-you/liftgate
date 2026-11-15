#[path = "cli.rs"]
pub(crate) mod cli;
#[path = "client.rs"]
pub(crate) mod client;
#[path = "config_sync.rs"]
pub(crate) mod config_sync;
#[path = "daemon.rs"]
pub(crate) mod daemon;
#[path = "error.rs"]
pub(crate) mod error;
#[path = "harness.rs"]
pub(crate) mod harness;
#[path = "serve.rs"]
pub(crate) mod serve;
#[path = "state.rs"]
pub(crate) mod state;
#[path = "tui.rs"]
pub(crate) mod tui;
#[path = "ui.rs"]
pub(crate) mod ui;
#[path = "upgrade.rs"]
pub(crate) mod upgrade;

use std::ffi::OsString;
use std::io::{IsTerminal as _, stdin, stdout};
use std::process::ExitCode;

use clap::{CommandFactory as _, FromArgMatches as _};
use liftgate_server::job::install_panic_cleanup_hook;
use tracing_subscriber::EnvFilter;

use crate::cli::{
    ClaudeArgs, Cli, CodexArgs, Commands, ConfigArgs, DoctorArgs, ModelsArgs, RestartArgs,
    ServeArgs, StartArgs, TopArgs, UiArgs, UpgradeArgs, invoked_bin_name, normalize_harness_args,
};
use crate::error::CliError;
use crate::state::StatePaths;
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

    match cli.command {
        None if bare_interactive_invocation => {
            let Some(action) = tui::run_cockpit(&state_paths).await? else {
                return Ok(0);
            };
            dispatch_cockpit_action(&state_paths, action).await
        }
        None => {
            serve::run_serve(cli.serve).await?;
            Ok(0)
        }
        Some(Commands::Top(args)) => {
            tui::run_top(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Serve(args)) => {
            serve::run_serve(args).await?;
            Ok(0)
        }
        Some(Commands::Start(args)) => {
            daemon::run_start(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Stop) => {
            let _ = daemon::stop_daemon(&state_paths, false).await?;
            Ok(0)
        }
        Some(Commands::Restart(args)) => {
            daemon::run_restart(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Status(args)) => {
            daemon::run_status(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Logs(args)) => {
            daemon::run_logs(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Ui(args)) => {
            daemon::run_ui(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Models(args)) => {
            daemon::run_models(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Claude(args)) => harness::run_claude(&state_paths, args).await,
        Some(Commands::Codex(args)) => harness::run_codex(&state_paths, args).await,
        Some(Commands::Config(args)) => {
            config_sync::run_config(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Doctor(args)) => {
            daemon::run_doctor(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Upgrade(args)) => {
            upgrade::run_upgrade(&state_paths, args).await?;
            Ok(0)
        }
        Some(Commands::Alias(args)) => {
            daemon::run_alias(args)?;
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

async fn dispatch_cockpit_action(
    state_paths: &StatePaths,
    action: CockpitAction,
) -> Result<i32, CliError> {
    match action {
        CockpitAction::LaunchCodex => {
            harness::run_codex(state_paths, CodexArgs::default_interactive()).await
        }
        CockpitAction::LaunchClaude => {
            harness::run_claude(state_paths, ClaudeArgs::default_interactive()).await
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
            daemon::run_models(
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
            daemon::run_ui(
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
                daemon::run_restart(state_paths, RestartArgs::default()).await?;
            } else {
                daemon::run_start(
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
        CockpitAction::SyncConfig => {
            config_sync::run_config(state_paths, ConfigArgs::default_local()).await?;
            Ok(0)
        }
        CockpitAction::Doctor => {
            daemon::run_doctor(
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
            upgrade::run_upgrade(
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
            let _ = daemon::stop_daemon(state_paths, false).await?;
            Ok(0)
        }
    }
}
