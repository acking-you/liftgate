//! Cross-platform native terminal emulator launcher for interactive CLI tools.
//!
//! Allows standalone GUI applications (`liftgate-desktop`) to launch interactive sessions
//! (`claude`, `codex`, `agy acp --login`, `agy <args>`) inside an isolated native terminal
//! window with process-scoped environment variables, without polluting global user config.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[cfg(not(target_os = "macos"))]
use liftgate_server::platform::{is_executable_file, split_expanded_path_var};

use crate::error::CliError;
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
        let has_env = !self.env_remove.is_empty() || !self.env_set.is_empty();
        let mut cmd_tokens = Vec::with_capacity(
            1 + self.args.len() + self.env_remove.len() * 2 + self.env_set.len(),
        );
        if has_env {
            cmd_tokens.push("env".to_owned());
            for key in &self.env_remove {
                cmd_tokens.push("-u".to_owned());
                cmd_tokens.push(quote_preview(key));
            }
            for (key, val) in &self.env_set {
                let display_val = if is_sensitive_env_var(key, val) {
                    "<redacted>"
                } else {
                    val.as_str()
                };
                cmd_tokens.push(format!("{key}={}", quote_preview(display_val)));
            }
        }
        cmd_tokens.push(quote_preview(&self.program.display().to_string()));
        for arg in &self.args {
            cmd_tokens.push(quote_preview(&arg.to_string_lossy()));
        }
        parts.push(cmd_tokens.join(" "));
        parts.join(" && ")
    }
}

fn is_sensitive_env_var(key: &str, value: &str) -> bool {
    !value.is_empty()
        && value != crate::cli::PLACEHOLDER_API_KEY
        && (key.eq_ignore_ascii_case("AGY_GATEWAY_API_KEY")
            || key.eq_ignore_ascii_case("ANTHROPIC_AUTH_TOKEN")
            || key.eq_ignore_ascii_case("ANTHROPIC_API_KEY")
            || key.eq_ignore_ascii_case("OPENAI_API_KEY"))
}

fn quote_preview(s: &str) -> String {
    if s.is_empty() {
        return "''".to_owned();
    }
    if s.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | '+' | ',')
    }) {
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
    state_paths: &StatePaths,
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

/// Materializes a self-contained, multi-run launcher script under `~/.liftgate/launchers/`
/// (preserving referenced `--settings` / `model_catalog_json` assets and scoped environment
/// overrides while removing transient `ephemeral` files) and returns a ready-to-paste command.
pub fn prepare_reusable_launcher_command(
    state_paths: &StatePaths,
    slug: &str,
    spec: &TerminalLaunchSpec,
) -> Result<String, CliError> {
    let launchers_dir = state_paths.ensure_launchers_dir()?;
    let clean_slug: String = slug
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let safe_slug = if clean_slug.is_empty() {
        "harness"
    } else {
        clean_slug.as_str()
    };

    let mut path_rewrites: Vec<(String, String)> = Vec::with_capacity(spec.cleanup_files.len() * 3);
    for (idx, temp_file) in spec.cleanup_files.iter().enumerate() {
        let ext = temp_file
            .extension()
            .and_then(|e| e.to_str())
            .filter(|e| !e.is_empty())
            .unwrap_or("json");
        let stable_file = launchers_dir.join(format!("{safe_slug}-config-{idx}.{ext}"));
        let payload = std::fs::read(temp_file).map_err(|source| CliError::StateIo {
            path: temp_file.clone(),
            source,
        })?;
        crate::state::write_file_atomic(&stable_file, &payload, true)?;
        let _ = std::fs::remove_file(temp_file);

        let old_native = temp_file.display().to_string();
        let new_native = stable_file.display().to_string();
        let old_slash = temp_file.to_string_lossy().replace('\\', "/");
        let new_slash = stable_file.to_string_lossy().replace('\\', "/");
        let old_toml = crate::harness::toml_inline_string(&old_slash);
        let new_toml = crate::harness::toml_inline_string(&new_slash);
        if old_toml != old_slash {
            path_rewrites.push((old_toml, new_toml));
        }
        path_rewrites.push((old_native.clone(), new_native));
        if old_slash != old_native {
            path_rewrites.push((old_slash, new_slash));
        }
    }

    let rewritten_args: Vec<String> = spec
        .args
        .iter()
        .map(|raw| {
            let mut arg = raw.to_string_lossy().into_owned();
            for (from, to) in &path_rewrites {
                if !from.is_empty() && arg.contains(from) {
                    arg = arg.replace(from, to);
                }
            }
            arg
        })
        .collect();

    #[cfg(windows)]
    {
        let script_path = launchers_dir.join(format!("{safe_slug}-launch.ps1"));
        let escape_ps = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let mut body = String::from("$ErrorActionPreference = 'Stop'\n$__lgSavedEnv = @{}\n");
        let mut touched_keys: Vec<&str> = Vec::new();
        for k in &spec.env_remove {
            if !touched_keys.contains(&k.as_str()) {
                touched_keys.push(k.as_str());
            }
        }
        for (k, _) in &spec.env_set {
            if !touched_keys.contains(&k.as_str()) {
                touched_keys.push(k.as_str());
            }
        }
        for key in &touched_keys {
            let qk = escape_ps(key);
            let _ = writeln!(
                body,
                "$__lgSavedEnv[{qk}] = [Environment]::GetEnvironmentVariable({qk}, 'Process')"
            );
        }
        body.push_str("$__lgPushedLocation = $false\ntry {\n");
        for k in &spec.env_remove {
            let _ = writeln!(
                body,
                "  [Environment]::SetEnvironmentVariable({}, $null, 'Process')",
                escape_ps(k)
            );
        }
        for (k, v) in &spec.env_set {
            let _ = writeln!(
                body,
                "  [Environment]::SetEnvironmentVariable({}, {}, 'Process')",
                escape_ps(k),
                escape_ps(v)
            );
        }
        if let Some(cwd) = &spec.cwd {
            let _ = writeln!(
                body,
                "  Push-Location -LiteralPath {}\n  $__lgPushedLocation = $true",
                escape_ps(&cwd.display().to_string())
            );
        }
        let _ = write!(
            body,
            "  & {}",
            escape_ps(&spec.program.display().to_string())
        );
        for arg in &rewritten_args {
            let _ = write!(body, " {}", escape_ps(arg));
        }
        body.push_str(" @args\n} finally {\n  if ($__lgPushedLocation) { Pop-Location }\n");
        body.push_str("  foreach ($__k in $__lgSavedEnv.Keys) { [Environment]::SetEnvironmentVariable($__k, $__lgSavedEnv[$__k], 'Process') }\n}\n");

        crate::state::write_file_atomic(&script_path, body.as_bytes(), true)?;
        let shell_bin = if find_in_path_windows("pwsh.exe").is_some() {
            "pwsh"
        } else {
            "powershell"
        };
        let raw_script = script_path.display().to_string();
        let file_arg = if raw_script.contains('$') || raw_script.contains('`') {
            format!("'{}'", raw_script.replace('\'', "''"))
        } else {
            format!("\"{}\"", raw_script.replace('"', "\\\""))
        };
        Ok(format!(
            "{shell_bin} -NoLogo -NoProfile -ExecutionPolicy Bypass -File {file_arg}"
        ))
    }

    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let script_path = launchers_dir.join(format!("{safe_slug}-launch.sh"));
        let sh_quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
        let mut body = String::from("#!/usr/bin/env sh\nset -e\n");
        for k in &spec.env_remove {
            let _ = writeln!(body, "unset {k}");
        }
        for (k, v) in &spec.env_set {
            let _ = writeln!(body, "export {k}={}", sh_quote(v));
        }
        if let Some(cwd) = &spec.cwd {
            let _ = writeln!(body, "cd {}", sh_quote(&cwd.display().to_string()));
        }
        let _ = write!(
            body,
            "exec {}",
            sh_quote(&spec.program.display().to_string())
        );
        for arg in &rewritten_args {
            let _ = write!(body, " {}", sh_quote(arg));
        }
        body.push_str(" \"$@\"\n");

        crate::state::write_file_atomic(&script_path, body.as_bytes(), true)?;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700)).map_err(
            |source| CliError::StateIo {
                path: script_path.clone(),
                source,
            },
        )?;
        Ok(format!(
            "sh {}",
            quote_preview(&script_path.display().to_string())
        ))
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

    #[test]
    fn preview_and_reusable_launcher_preserve_env_and_referenced_configs() {
        let tmp_root =
            std::env::temp_dir().join(format!("liftgate-term-test-{}", uuid::Uuid::new_v4()));
        let paths = StatePaths::resolve(Some(tmp_root.clone())).unwrap();
        let eph_dir = paths.ensure_ephemeral_dir().unwrap();
        let eph_catalog = eph_dir.join("codex-catalog-1234-abcd.json");
        std::fs::write(&eph_catalog, br#"{"models":[]}"#).unwrap();

        let catalog_slash = eph_catalog.to_string_lossy().replace('\\', "/");
        let spec = TerminalLaunchSpec {
            title: "Liftgate Codex".to_owned(),
            program: PathBuf::from("codex"),
            args: vec![
                OsString::from("-c"),
                OsString::from(format!("model_catalog_json=\"{catalog_slash}\"")),
            ],
            cwd: Some(tmp_root.clone()),
            env_set: vec![
                ("AGY_GATEWAY_API_KEY".to_owned(), "secret-token".to_owned()),
                ("NO_PROXY".to_owned(), "127.0.0.1,localhost".to_owned()),
            ],
            env_remove: vec!["OPENAI_BASE_URL".to_owned()],
            cleanup_files: vec![eph_catalog.clone()],
            hold_open_on_exit: false,
        };

        let preview = spec.preview_command_line();
        assert!(preview.contains("env -u OPENAI_BASE_URL"));
        assert!(preview.contains("AGY_GATEWAY_API_KEY='<redacted>'"));
        assert!(preview.contains("NO_PROXY=127.0.0.1,localhost"));
        assert!(!preview.contains("secret-token"));

        let cmd = prepare_reusable_launcher_command(&paths, "codex", &spec).unwrap();
        assert!(
            !eph_catalog.exists(),
            "ephemeral input should be cleaned up"
        );

        let stable_cfg = paths.launchers_dir().join("codex-config-0.json");
        assert_eq!(std::fs::read(&stable_cfg).unwrap(), br#"{"models":[]}"#);

        let script_name = if cfg!(windows) {
            "codex-launch.ps1"
        } else {
            "codex-launch.sh"
        };
        let script_path = paths.launchers_dir().join(script_name);
        assert!(cmd.contains(script_name));
        let script_body = std::fs::read_to_string(&script_path).unwrap();
        assert!(script_body.contains("codex-config-0.json"));
        assert!(!script_body.contains("codex-catalog-1234-abcd.json"));
        assert!(script_body.contains("secret-token"));
        assert!(script_body.contains("OPENAI_BASE_URL"));

        let _ = std::fs::remove_dir_all(&tmp_root);
    }
}
