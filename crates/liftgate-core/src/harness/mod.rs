//! Async child-process and terminal-window launchers for supported coding harnesses
//! (`lg cc`, `lg cx`, `lg oc`, `lg omp`), delegating synchronous command discovery,
//! config generation, and CLI flag classification to `liftgate-harness`.

mod claude;
mod codex;
mod omp;
mod opencode;

use std::path::Path;
use std::process::Stdio;

use liftgate_types::config::DEFAULT_TURN_TIMEOUT_SECS;
use tokio::process::Command;
use tokio::task::JoinSet;

use crate::error::CliError;
use crate::state::StatePaths;
use crate::terminal::TerminalLaunchSpec;

pub use claude::{prepare_claude_terminal_spec, run_claude};
pub use codex::{prepare_codex_terminal_spec, run_codex};
pub use liftgate_harness::ResolvedCommand;
pub(crate) use liftgate_harness::codex::toml_inline_string;
pub use omp::{prepare_omp_terminal_spec, run_omp};
pub use opencode::{prepare_opencode_terminal_spec, run_opencode};

pub(super) const DEFAULT_REQUEST_TIMEOUT_MS: u64 = DEFAULT_TURN_TIMEOUT_SECS * 1_000;

pub fn resolve_claude_command(explicit: Option<&Path>) -> Result<ResolvedCommand, CliError> {
    Ok(liftgate_harness::claude::resolve_claude_command(explicit)?)
}

pub fn resolve_codex_command(explicit: Option<&Path>) -> Result<ResolvedCommand, CliError> {
    Ok(liftgate_harness::codex::resolve_codex_command(explicit)?)
}

pub fn resolve_opencode_command(explicit: Option<&Path>) -> Result<ResolvedCommand, CliError> {
    Ok(liftgate_harness::opencode::resolve_opencode_command(
        explicit,
    )?)
}

pub fn resolve_omp_command(explicit: Option<&Path>) -> Result<ResolvedCommand, CliError> {
    Ok(liftgate_harness::omp::resolve_omp_command(explicit)?)
}

pub(super) fn command_from_terminal_spec(spec: &TerminalLaunchSpec) -> Command {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args);
    if let Some(ref cwd) = spec.cwd {
        cmd.current_dir(cwd);
    }
    for key in &spec.env_remove {
        cmd.env_remove(key);
    }
    for (k, v) in &spec.env_set {
        cmd.env(k, v);
    }
    cmd
}

pub(super) async fn spawn_and_wait_interactive(
    state_paths: &StatePaths,
    tool: &'static str,
    program_path: &Path,
    mut cmd: Command,
) -> Result<i32, CliError> {
    let _active_session_guard = state_paths.register_active_client_session(tool);
    if let Ok(cwd) = std::env::current_dir() {
        cmd.current_dir(cwd);
    }
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let mut child = cmd.spawn().map_err(|source| CliError::HarnessSpawn {
        tool,
        path: program_path.to_path_buf(),
        source,
    })?;

    let mut sig_tasks = JoinSet::new();
    sig_tasks.spawn(async { while tokio::signal::ctrl_c().await.is_ok() {} });

    let status = child
        .wait()
        .await
        .map_err(|source| CliError::HarnessSpawn {
            tool,
            path: program_path.to_path_buf(),
            source,
        })?;

    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests;
