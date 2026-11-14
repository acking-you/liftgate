mod cli;
mod client;
mod config_sync;
mod daemon;
mod error;
mod harness;
mod serve;
mod state;

use std::process::ExitCode;

use clap::Parser as _;
use liftgate_server::job::install_panic_cleanup_hook;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Commands, normalize_harness_args};
use crate::error::CliError;
use crate::state::StatePaths;

#[tokio::main]
async fn main() -> ExitCode {
    install_panic_cleanup_hook();

    let normalized_args = normalize_harness_args(std::env::args_os());
    let parsed = Cli::parse_from(normalized_args);

    let is_foreground_server = matches!(parsed.command, None | Some(Commands::Serve(_)));
    let default_level = if is_foreground_server { "info" } else { "warn" };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    match dispatch(parsed).await {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(err) => {
            if is_foreground_server {
                tracing::error!(error = %err, "gateway exited with error");
            } else {
                eprintln!("[ERROR] {err}");
            }
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                if is_foreground_server {
                    tracing::error!(cause = %cause, "caused by");
                } else {
                    eprintln!("  caused by: {cause}");
                }
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(cli: Cli) -> Result<i32, CliError> {
    let state_paths = StatePaths::resolve(cli.state_dir)?;

    match cli.command {
        None => {
            serve::run_serve(cli.serve).await?;
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
    }
}
