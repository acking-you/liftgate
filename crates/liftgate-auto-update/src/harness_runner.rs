//! Supervised non-interactive version probing and update runners for supported coding harnesses.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

#[cfg(not(windows))]
use liftgate_harness::fs_util::is_executable_file;
use liftgate_harness::{
    ResolvedCommand,
    claude::resolve_claude_command,
    codex::resolve_codex_command,
    fs_util::{canonicalize_clean, split_expanded_path_var},
    omp::resolve_omp_command,
    opencode::resolve_opencode_command,
};
use liftgate_types::{config::AutoUpdateTarget, telemetry::AutoUpdateLastOutcome};
use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::time::timeout;

use crate::{ProbedTargetInfo, SingleTargetRunResult};

const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(12);
const HARNESS_UPDATE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_CHILD_CAPTURE_BYTES: u64 = 128 * 1024;

/// Pluggable OS child process-group / Job Object supervisor injected by the host runtime.
pub trait ChildGroupSupervisor: Send + Sync {
    /// Applies pre-spawn process isolation settings (`process_group(0)` / Win32 creation flags).
    fn configure_command(&self, cmd: &mut Command);

    /// Attaches an RAII process-group / Win32 Job Object guard kept alive until the child exits.
    fn attach_guard(&self, child: &Child) -> Option<Box<dyn Send + Sync>>;
}

/// Safe fallback child supervisor using `kill_on_drop(true)` and non-interactive git env flags.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultChildSupervisor;

impl ChildGroupSupervisor for DefaultChildSupervisor {
    fn configure_command(&self, cmd: &mut Command) {
        cmd.kill_on_drop(true)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat");
    }

    fn attach_guard(&self, _child: &Child) -> Option<Box<dyn Send + Sync>> {
        None
    }
}

/// Resolves a third-party coding harness executable using `liftgate-harness`'s discovery rules.
#[must_use]
pub fn resolve_harness_for_target(target: AutoUpdateTarget) -> Option<ResolvedCommand> {
    match target {
        AutoUpdateTarget::Codex => resolve_codex_command(None).ok(),
        AutoUpdateTarget::Claude => resolve_claude_command(None).ok(),
        AutoUpdateTarget::Opencode => resolve_opencode_command(None).ok(),
        AutoUpdateTarget::Omp => resolve_omp_command(None).ok(),
        AutoUpdateTarget::Liftgate | AutoUpdateTarget::Agy => None,
    }
}

/// Probes `<harness> --version` for `target`.
pub async fn probe_harness_info(
    target: AutoUpdateTarget,
    supervisor: &dyn ChildGroupSupervisor,
) -> ProbedTargetInfo {
    let Some(resolved) = resolve_harness_for_target(target) else {
        return ProbedTargetInfo::default();
    };
    let ver = probe_resolved_command_version(&resolved, supervisor).await;
    ProbedTargetInfo {
        binary_path: Some(resolved.program.display().to_string()),
        installed_version: ver,
    }
}

/// Executes a single non-interactive check or upgrade pass for a coding harness (`codex`, `claude`, `opencode`, `omp`).
pub async fn run_harness_target_pass(
    target: AutoUpdateTarget,
    check_only: bool,
    supervisor: &dyn ChildGroupSupervisor,
    stop_rx: &watch::Receiver<bool>,
) -> SingleTargetRunResult {
    let Some(resolved) = resolve_harness_for_target(target) else {
        return SingleTargetRunResult {
            target,
            status: AutoUpdateLastOutcome::NotInstalled,
            previous_version: None,
            current_version: None,
            latest_version: None,
            note: "not installed in PATH".to_owned(),
        };
    };

    let before_version = probe_resolved_command_version(&resolved, supervisor).await;
    if check_only && target != AutoUpdateTarget::Omp {
        let note = match &before_version {
            Some(v) => {
                format!("installed v{v} (run without `--check` to invoke `{target}` updater)")
            }
            None => format!("installed at {}", resolved.program.display()),
        };
        return SingleTargetRunResult {
            target,
            status: AutoUpdateLastOutcome::UpToDate,
            previous_version: before_version.clone(),
            current_version: before_version.clone(),
            latest_version: before_version,
            note,
        };
    }

    let primary_args: &[&str] = match target {
        AutoUpdateTarget::Codex => &["update"],
        AutoUpdateTarget::Claude => &["update"],
        AutoUpdateTarget::Opencode => &["upgrade"],
        AutoUpdateTarget::Omp => {
            if check_only {
                &["update", "--check"]
            } else {
                &["update"]
            }
        }
        AutoUpdateTarget::Liftgate | AutoUpdateTarget::Agy => &[],
    };

    let mut exec_res = run_supervised_resolved_command(
        &resolved,
        primary_args,
        HARNESS_UPDATE_TIMEOUT,
        supervisor,
        stop_rx,
    )
    .await;

    if exec_res.is_err()
        && !check_only
        && let Some((fallback_cmd, fallback_args)) =
            detect_package_manager_fallback(target, &resolved)
    {
        let fb_resolved = ResolvedCommand {
            program: fallback_cmd,
            prefix_args: Vec::new(),
        };
        let fb_refs: Vec<&str> = fallback_args.iter().map(String::as_str).collect();
        exec_res = run_supervised_resolved_command(
            &fb_resolved,
            &fb_refs,
            HARNESS_UPDATE_TIMEOUT,
            supervisor,
            stop_rx,
        )
        .await;
    }

    match exec_res {
        Ok(output) => {
            let after_version = probe_resolved_command_version(&resolved, supervisor)
                .await
                .or_else(|| before_version.clone());
            if check_only && target == AutoUpdateTarget::Omp {
                let lower = output.to_ascii_lowercase();
                let has_update = lower.contains("update available")
                    || (lower.contains("available") && !lower.contains("up to date"));
                return SingleTargetRunResult {
                    target,
                    status: if has_update {
                        AutoUpdateLastOutcome::UpdateAvailable
                    } else {
                        AutoUpdateLastOutcome::UpToDate
                    },
                    previous_version: before_version,
                    current_version: after_version.clone(),
                    latest_version: after_version,
                    note: summarize_child_output(&output, "checked"),
                };
            }

            let changed = match (&before_version, &after_version) {
                (Some(b), Some(a)) => b != a,
                _ => false,
            };
            if changed {
                let from_v = before_version
                    .clone()
                    .unwrap_or_else(|| "unknown".to_owned());
                let to_v = after_version
                    .clone()
                    .unwrap_or_else(|| "unknown".to_owned());
                tracing::info!(
                    harness = %target,
                    from = %from_v,
                    to = %to_v,
                    "automatically updated coding harness while gateway was idle"
                );
                SingleTargetRunResult {
                    target,
                    status: AutoUpdateLastOutcome::Updated,
                    previous_version: before_version,
                    current_version: after_version.clone(),
                    latest_version: after_version,
                    note: format!("auto-updated v{from_v} -> v{to_v}"),
                }
            } else {
                SingleTargetRunResult {
                    target,
                    status: AutoUpdateLastOutcome::UpToDate,
                    previous_version: before_version,
                    current_version: after_version.clone(),
                    latest_version: after_version,
                    note: "up to date".to_owned(),
                }
            }
        }
        Err(detail) => {
            tracing::warn!(harness = %target, error = %detail, "harness auto-update command failed");
            SingleTargetRunResult {
                target,
                status: AutoUpdateLastOutcome::Failed,
                previous_version: before_version.clone(),
                current_version: before_version,
                latest_version: None,
                note: detail,
            }
        }
    }
}

fn detect_package_manager_fallback(
    target: AutoUpdateTarget,
    resolved: &ResolvedCommand,
) -> Option<(PathBuf, Vec<String>)> {
    let canon = canonicalize_clean(&resolved.program);
    let path_str = canon.to_string_lossy();
    let is_homebrew = path_str.contains("/homebrew/")
        || path_str.contains("/Cellar/")
        || path_str.contains("/Caskroom/")
        || path_str.contains("/linuxbrew/");

    if is_homebrew && let Some(brew) = find_tool_in_path("brew") {
        let pkg = match target {
            AutoUpdateTarget::Codex => Some("codex"),
            AutoUpdateTarget::Claude => Some("claude-code"),
            AutoUpdateTarget::Opencode => Some("opencode"),
            AutoUpdateTarget::Omp | AutoUpdateTarget::Liftgate | AutoUpdateTarget::Agy => None,
        }?;
        return Some((brew, vec!["upgrade".to_owned(), pkg.to_owned()]));
    }

    if target == AutoUpdateTarget::Codex
        && let Some(npm) = find_tool_in_path("npm")
    {
        return Some((
            npm,
            vec![
                "install".to_owned(),
                "-g".to_owned(),
                "@openai/codex@latest".to_owned(),
            ],
        ));
    }
    None
}

fn find_tool_in_path(name: &str) -> Option<PathBuf> {
    for dir in split_expanded_path_var() {
        #[cfg(windows)]
        {
            for ext in ["cmd", "exe", "bat"] {
                let candidate = dir.join(format!("{name}.{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        #[cfg(not(windows))]
        {
            let candidate = dir.join(name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Runs a `ResolvedCommand` non-interactively with bounded stdout/stderr capture and timeout.
pub async fn run_supervised_resolved_command(
    resolved: &ResolvedCommand,
    extra_args: &[&str],
    max_duration: Duration,
    supervisor: &dyn ChildGroupSupervisor,
    stop_rx: &watch::Receiver<bool>,
) -> Result<String, String> {
    let mut cmd = Command::new(&resolved.program);
    cmd.args(&resolved.prefix_args)
        .args(extra_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CI", "1")
        .env("NONINTERACTIVE", "1")
        .env("NPM_CONFIG_YES", "true")
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .env("HOMEBREW_NO_INSTALL_CLEANUP", "1")
        .env("NO_COLOR", "1");
    supervisor.configure_command(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn `{}`: {e}", resolved.program.display()))?;
    let _group_guard = supervisor.attach_guard(&child);

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let read_out = async move {
        let mut buf = Vec::new();
        if let Some(out) = stdout_pipe {
            let _ = out
                .take(MAX_CHILD_CAPTURE_BYTES)
                .read_to_end(&mut buf)
                .await;
        }
        buf
    };
    let read_err = async move {
        let mut buf = Vec::new();
        if let Some(err) = stderr_pipe {
            let _ = err
                .take(MAX_CHILD_CAPTURE_BYTES)
                .read_to_end(&mut buf)
                .await;
        }
        buf
    };

    let mut stop_clone = stop_rx.clone();
    let wait_fut = async {
        let (out_bytes, err_bytes, wait_res) = tokio::join!(read_out, read_err, child.wait());
        (out_bytes, err_bytes, wait_res)
    };

    let outcome = tokio::select! {
        biased;
        Ok(_) = stop_clone.wait_for(|&stop| stop) => {
            return Err("aborted due to gateway shutdown".to_owned());
        }
        timed = timeout(max_duration, wait_fut) => timed,
    };

    let Ok((out_bytes, err_bytes, wait_res)) = outcome else {
        return Err(format!("timed out after {}s", max_duration.as_secs()));
    };
    let status = wait_res.map_err(|e| format!("wait failed: {e}"))?;
    let combined = merge_captured_output(&out_bytes, &err_bytes);
    if status.success() {
        Ok(combined)
    } else {
        let detail = summarize_child_output(
            &combined,
            &format!("exited with status {}", status.code().unwrap_or(-1)),
        );
        Err(detail)
    }
}

/// Probes `<resolved> --version` and extracts a clean SemVer-like token.
pub async fn probe_resolved_command_version(
    resolved: &ResolvedCommand,
    supervisor: &dyn ChildGroupSupervisor,
) -> Option<String> {
    let (_stop_tx, stop_rx) = watch::channel(false);
    let output = run_supervised_resolved_command(
        resolved,
        &["--version"],
        VERSION_PROBE_TIMEOUT,
        supervisor,
        &stop_rx,
    )
    .await
    .ok()?;
    extract_clean_version_token(&output)
}

/// Extracts a clean SemVer-like version token (e.g. `0.171.0`, `2.1.326`, `19.2.1`) from `--version` output.
#[must_use]
pub fn extract_clean_version_token(raw: &str) -> Option<String> {
    let first_line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    for token in first_line.split(|c: char| c.is_whitespace() || c == '/' || c == '(' || c == ')') {
        let clean_punct = token.trim_matches(|c: char| c == ',' || c == ';');
        let trimmed = clean_punct
            .strip_prefix('v')
            .or_else(|| clean_punct.strip_prefix('V'))
            .unwrap_or(clean_punct);
        if trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) && trimmed.contains('.') {
            return Some(trimmed.to_owned());
        }
    }
    Some(first_line.chars().take(48).collect())
}

fn merge_captured_output(stdout: &[u8], stderr: &[u8]) -> String {
    let out_s = String::from_utf8_lossy(stdout);
    let err_s = String::from_utf8_lossy(stderr);
    match (out_s.trim().is_empty(), err_s.trim().is_empty()) {
        (false, true) => out_s.trim().to_owned(),
        (true, false) => err_s.trim().to_owned(),
        (false, false) => format!("{}\n{}", out_s.trim(), err_s.trim()),
        (true, true) => String::new(),
    }
}

fn summarize_child_output(raw: &str, fallback: &str) -> String {
    let line = raw
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(fallback);
    line.chars().take(140).collect()
}
