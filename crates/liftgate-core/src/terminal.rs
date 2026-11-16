//! Cross-platform native terminal emulator launcher for interactive CLI tools.
//!
//! Allows standalone GUI applications (`liftgate-desktop`) to launch interactive sessions
//! (`claude`, `codex`, `agy acp --login`, `agy <args>`) inside an isolated native terminal
//! window with process-scoped environment variables, without polluting global user config.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[cfg(not(target_os = "macos"))]
use liftgate_server::platform::{is_executable_file, split_expanded_path_var};

use crate::error::CliError;
#[cfg(not(windows))]
use crate::state::StatePaths;

/// Self-contained specification for an interactive terminal command.
#[derive(Clone, Debug)]
pub struct TerminalLaunchSpec {
    pub title: String,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env_set: Vec<(String, String)>,
    pub env_remove: Vec<String>,
    pub cleanup_files: Vec<PathBuf>,
    pub hold_open_on_exit: bool,
}

impl TerminalLaunchSpec {
    /// Renders an equivalent copy-pasteable shell snippet for UI preview.
    #[must_use]
    pub fn preview_command_line(&self) -> String {
        let mut parts = Vec::new();
        if let Some(cwd) = &self.cwd {
            parts.push(format!("cd {}", quote_preview(&cwd.display().to_string())));
        }
        let mut cmd_tokens = Vec::with_capacity(1 + self.args.len());
        cmd_tokens.push(quote_preview(&self.program.display().to_string()));
        for arg in &self.args {
            cmd_tokens.push(quote_preview(&arg.to_string_lossy()));
        }
        parts.push(cmd_tokens.join(" "));
        parts.join(" && ")
    }
}

fn quote_preview(s: &str) -> String {
    if s.is_empty() {
        return "''".to_owned();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | '+'))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Spawns `spec` in a new visible system terminal window (`wt.exe` / PowerShell console on
/// Windows, `Terminal.app` on macOS, or standard desktop terminal emulator on Linux).
pub fn spawn_in_system_terminal(
    #[cfg_attr(
        windows,
        expect(
            unused_variables,
            reason = "Windows passes env directly via CreateProcess"
        )
    )]
    state_paths: &crate::state::StatePaths,
    spec: &TerminalLaunchSpec,
) -> Result<(), CliError> {
    #[cfg(windows)]
    {
        spawn_windows_terminal(spec)
    }

    #[cfg(target_os = "macos")]
    {
        spawn_macos_terminal(state_paths, spec)
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        spawn_linux_terminal(state_paths, spec)
    }
}

#[cfg(windows)]
fn spawn_windows_terminal(spec: &TerminalLaunchSpec) -> Result<(), CliError> {
    use std::os::windows::process::CommandExt as _;
    use windows_sys::Win32::System::Threading::CREATE_NEW_CONSOLE;

    let apply_env_and_cwd = |cmd: &mut Command| {
        if let Some(cwd) = &spec.cwd
            && cwd.is_dir()
        {
            cmd.current_dir(cwd);
        }
        for k in &spec.env_remove {
            cmd.env_remove(k);
        }
        for (k, v) in &spec.env_set {
            cmd.env(k, v);
        }
    };

    // Prefer Windows Terminal (`wt.exe`) when available for full Unicode/TrueColor TUI rendering.
    if let Some(wt_exe) = find_in_path_windows("wt.exe") {
        let mut cmd = Command::new(&wt_exe);
        if let Some(cwd) = &spec.cwd
            && cwd.is_dir()
        {
            cmd.arg("-d").arg(cwd);
        }
        cmd.arg("--title").arg(&spec.title);
        if spec.hold_open_on_exit || !spec.cleanup_files.is_empty() {
            let shell = find_in_path_windows("pwsh.exe")
                .or_else(|| find_in_path_windows("powershell.exe"))
                .unwrap_or_else(|| PathBuf::from("powershell.exe"));
            cmd.arg(shell)
                .arg("-NoLogo")
                .args(if spec.hold_open_on_exit {
                    &["-NoExit"][..]
                } else {
                    &[][..]
                })
                .arg("-Command")
                .arg(build_powershell_invocation(spec));
        } else {
            cmd.arg("--").arg(&spec.program).args(&spec.args);
        }
        apply_env_and_cwd(&mut cmd);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if cmd.spawn().is_ok() {
            return Ok(());
        }
    }

    // Fallback: allocate a fresh console window directly (`CREATE_NEW_CONSOLE`).
    if spec.hold_open_on_exit || !spec.cleanup_files.is_empty() {
        let shell = find_in_path_windows("pwsh.exe")
            .or_else(|| find_in_path_windows("powershell.exe"))
            .unwrap_or_else(|| PathBuf::from("powershell.exe"));
        let mut cmd = Command::new(&shell);
        cmd.arg("-NoLogo")
            .args(if spec.hold_open_on_exit {
                &["-NoExit"][..]
            } else {
                &[][..]
            })
            .arg("-Command")
            .arg(build_powershell_invocation(spec));
        apply_env_and_cwd(&mut cmd);
        cmd.creation_flags(CREATE_NEW_CONSOLE);
        cmd.spawn().map_err(|source| CliError::HarnessSpawn {
            tool: "terminal",
            path: shell,
            source,
        })?;
        return Ok(());
    }

    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args);
    apply_env_and_cwd(&mut cmd);
    cmd.creation_flags(CREATE_NEW_CONSOLE);
    cmd.spawn().map_err(|source| CliError::HarnessSpawn {
        tool: "terminal",
        path: spec.program.clone(),
        source,
    })?;
    Ok(())
}

#[cfg(windows)]
fn build_powershell_invocation(spec: &TerminalLaunchSpec) -> String {
    let escape_ps = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let mut tokens = Vec::with_capacity(1 + spec.args.len());
    tokens.push(format!(
        "& {}",
        escape_ps(&spec.program.display().to_string())
    ));
    for arg in &spec.args {
        tokens.push(escape_ps(&arg.to_string_lossy()));
    }
    let invoke_expr = tokens.join(" ");
    if spec.cleanup_files.is_empty() {
        invoke_expr
    } else {
        let mut cleanups = Vec::with_capacity(spec.cleanup_files.len());
        for f in &spec.cleanup_files {
            cleanups.push(format!(
                "Remove-Item -LiteralPath {} -Force -ErrorAction SilentlyContinue",
                escape_ps(&f.display().to_string())
            ));
        }
        format!(
            "try {{ {invoke_expr} }} finally {{ {} }}",
            cleanups.join("; ")
        )
    }
}

#[cfg(windows)]
fn find_in_path_windows(exe_name: &str) -> Option<PathBuf> {
    for dir in split_expanded_path_var() {
        let candidate = dir.join(exe_name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(not(windows))]
fn write_ephemeral_launcher_script(
    state_paths: &StatePaths,
    spec: &TerminalLaunchSpec,
) -> Result<PathBuf, CliError> {
    use std::fmt::Write as _;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    let dir = state_paths.ensure_ephemeral_dir()?;
    let script_path = dir.join(format!("term-launch-{}.sh", uuid::Uuid::new_v4().simple()));

    let sh_quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let mut body = String::from("#!/usr/bin/env sh\nrm -f \"$0\"\n");
    for k in &spec.env_remove {
        let _ = writeln!(body, "unset {k}");
    }
    for (k, v) in &spec.env_set {
        let _ = writeln!(body, "export {k}={}", sh_quote(v));
    }
    if let Some(cwd) = &spec.cwd
        && cwd.is_dir()
    {
        let _ = writeln!(
            body,
            "cd {} || exit 1",
            sh_quote(&cwd.display().to_string())
        );
    }
    let _ = write!(body, "{}", sh_quote(&spec.program.display().to_string()));
    for arg in &spec.args {
        let _ = write!(body, " {}", sh_quote(&arg.to_string_lossy()));
    }
    body.push('\n');
    for f in &spec.cleanup_files {
        let _ = writeln!(body, "rm -f {}", sh_quote(&f.display().to_string()));
    }
    if spec.hold_open_on_exit {
        body.push_str(
            "printf '\\n[Liftgate session finished — press Enter to close] '\nread -r _\n",
        );
    }

    crate::state::write_file_atomic(&script_path, body.as_bytes(), true)?;
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700)).map_err(|source| {
        CliError::StateIo {
            path: script_path.clone(),
            source,
        }
    })?;
    Ok(script_path)
}

#[cfg(target_os = "macos")]
fn spawn_macos_terminal(
    state_paths: &StatePaths,
    spec: &TerminalLaunchSpec,
) -> Result<(), CliError> {
    let script = write_ephemeral_launcher_script(state_paths, spec)?;
    // `.command` / executable script opened with `Terminal.app` launches a dedicated window
    // without requiring Automation AppleEvents TCC prompts.
    let mut cmd = Command::new("open");
    cmd.args(["-a", "Terminal"])
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn().map_err(|source| CliError::HarnessSpawn {
        tool: "Terminal.app",
        path: PathBuf::from("/System/Applications/Utilities/Terminal.app"),
        source,
    })?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn spawn_linux_terminal(
    state_paths: &StatePaths,
    spec: &TerminalLaunchSpec,
) -> Result<(), CliError> {
    let script = write_ephemeral_launcher_script(state_paths, spec)?;
    let candidates: &[(&str, &[&str])] = &[
        ("x-terminal-emulator", &["-e"]),
        ("gnome-terminal", &["--"]),
        ("konsole", &["-e"]),
        ("kitty", &[]),
        ("alacritty", &["-e"]),
        ("wezterm", &["start", "--"]),
        ("xfce4-terminal", &["-x"]),
        ("xterm", &["-e"]),
    ];

    if let Ok(custom_term) = std::env::var("TERMINAL")
        && !custom_term.trim().is_empty()
    {
        let mut cmd = Command::new(custom_term.trim());
        cmd.arg("-e")
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if cmd.spawn().is_ok() {
            return Ok(());
        }
    }

    for (bin, prefix_args) in candidates {
        if find_in_path_unix(bin).is_some() {
            let mut cmd = Command::new(bin);
            cmd.args(*prefix_args)
                .arg(&script)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if cmd.spawn().is_ok() {
                return Ok(());
            }
        }
    }

    let _ = std::fs::remove_file(&script);
    Err(CliError::HarnessNotFound {
        tool: "terminal emulator",
        hint: "install a desktop terminal (e.g. kitty, alacritty, gnome-terminal, konsole, xterm) or set $TERMINAL",
    })
}

#[cfg(all(unix, not(target_os = "macos")))]
fn find_in_path_unix(name: &str) -> Option<PathBuf> {
    for dir in split_expanded_path_var() {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_command_line_quotes_paths_with_spaces_cleanly() {
        let spec = TerminalLaunchSpec {
            title: "Liftgate Claude".to_owned(),
            program: PathBuf::from("/usr/local/bin/claude"),
            args: vec![
                OsString::from("--model"),
                OsString::from("gemini-4-carbon-high"),
                OsString::from("hello world"),
            ],
            cwd: Some(PathBuf::from("/tmp/my project")),
            env_set: Vec::new(),
            env_remove: Vec::new(),
            cleanup_files: Vec::new(),
            hold_open_on_exit: false,
        };
        assert_eq!(
            spec.preview_command_line(),
            "cd '/tmp/my project' && /usr/local/bin/claude --model gemini-4-carbon-high 'hello world'"
        );
    }
}
