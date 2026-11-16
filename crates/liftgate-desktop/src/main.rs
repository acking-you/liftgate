#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod i18n;
mod theme;
mod views;

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use gpui_kit::*;
use liftgate_core::{
    agy_manager,
    cli::{DesktopWatchArgs, ServeArgs, clap_styles},
    desktop, serve,
    state::StatePaths,
    upgrade,
};
use tracing_subscriber::EnvFilter;

use crate::app::DesktopApp;

#[derive(Debug, Parser)]
#[command(
    name = "liftgate-desktop",
    version,
    styles = clap_styles(),
    about = "Standalone GPUI desktop control center for Liftgate (Antigravity ACP local API gateway)",
    long_about = None,
)]
struct DesktopCli {
    /// Override Liftgate runtime state directory (defaults to `~/.liftgate` or `LIFTGATE_STATE_DIR`).
    #[arg(long, global = true, env = "LIFTGATE_STATE_DIR", value_name = "DIR")]
    state_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<DesktopCommand>,
}

#[derive(Debug, Subcommand)]
enum DesktopCommand {
    /// Run the HTTP/SSE + ACP gateway server in the foreground (used by managed daemon supervisor).
    #[command(hide = true)]
    Serve(Box<ServeArgs>),

    /// Internal detached rollback watcher for Codex App / Claude Desktop sessions.
    #[command(name = "__desktop-watch", hide = true)]
    InternalDesktopWatch(DesktopWatchArgs),
}

fn main() -> ExitCode {
    serve::install_panic_cleanup_hook();
    upgrade::sweep_stale_backups();

    let cli = match DesktopCli::try_parse() {
        Ok(parsed) => parsed,
        Err(err) => {
            let _ = err.print();
            return ExitCode::from(u8::try_from(err.exit_code()).unwrap_or(2));
        }
    };

    let is_headless_serve = matches!(cli.command, Some(DesktopCommand::Serve(_)));
    let default_level = if is_headless_serve { "info" } else { "warn" };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();

    let state_paths = match StatePaths::resolve(cli.state_dir) {
        Ok(paths) => paths,
        Err(err) => {
            let _ = writeln!(std::io::stderr(), "liftgate-desktop: {err}");
            return ExitCode::FAILURE;
        }
    };
    agy_manager::sweep_stale_agy_backups(&state_paths);
    if !matches!(cli.command, Some(DesktopCommand::InternalDesktopWatch(_))) {
        desktop::sweep_stale_desktop_sessions(&state_paths);
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            let _ = writeln!(
                std::io::stderr(),
                "liftgate-desktop: failed to initialize tokio runtime: {err}"
            );
            return ExitCode::FAILURE;
        }
    };

    match cli.command {
        Some(DesktopCommand::Serve(args)) => {
            if let Err(err) = runtime.block_on(serve::run_serve(&state_paths, *args)) {
                tracing::error!(error = %err, "gateway server exited with error");
                return ExitCode::FAILURE;
            }
            return ExitCode::SUCCESS;
        }
        Some(DesktopCommand::InternalDesktopWatch(args)) => {
            if let Err(err) = runtime.block_on(desktop::run_desktop_watch(args)) {
                tracing::error!(error = %err, "desktop session watcher exited with error");
                return ExitCode::FAILURE;
            }
            return ExitCode::SUCCESS;
        }
        None => {}
    }

    let current_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("liftgate-desktop"));
    let prefs = state_paths.read_desktop_prefs().unwrap_or_default();
    let init_dark = prefs.theme.is_dark();
    let rt_handle = runtime.handle().clone();

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            theme::apply_staticflow_theme(init_dark, cx);

            let bounds = Bounds::centered(None, size(px(1320.0), px(860.0)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from(
                        "Liftgate · Antigravity Desktop Control Center",
                    )),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(1020.0), px(680.0))),
                ..Default::default()
            };

            let app_rt = rt_handle.clone();
            let app_paths = state_paths.clone();
            let app_exe = current_exe.clone();
            let app_prefs = prefs.clone();
            if let Err(err) = gpui_kit::open_window(options, cx, move |window, cx| {
                cx.new(|cx| DesktopApp::new(app_rt, app_paths, app_exe, app_prefs, window, cx))
            }) {
                tracing::error!(error = %err, "failed to open Liftgate GPUI window");
                cx.quit();
                return;
            }
            cx.activate(true);
        });

    ExitCode::SUCCESS
}
