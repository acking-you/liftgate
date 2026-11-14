use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Resolves the `agy` CLI executable across Windows, Linux, and macOS.
///
/// Discovery priority:
/// 1. Explicit `--agy-bin` / `AGY_BIN` path (if provided; returns `None` if invalid rather than
///    silently falling back to a different binary).
/// 2. Standard `PATH` lookup (`;` on Windows, `:` on Linux/macOS), checking executable bits on Unix
///    and standard executable extensions on Windows.
/// 3. Well-known per-OS installation directories (so `systemd` on Linux, `launchd` on macOS, and
///    GUI/service launchers on Windows work even with a minimal `PATH`).
#[must_use]
pub fn resolve_agy_binary(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return is_executable_file(&path).then(|| canonicalize_clean(&path));
    }

    if let Some(path_var) = std::env::var_os("PATH")
        && let Some(found) = search_path_entries(std::env::split_paths(&path_var))
    {
        return Some(found);
    }

    for candidate in platform_default_candidates() {
        if is_executable_file(&candidate) {
            return Some(canonicalize_clean(&candidate));
        }
    }

    None
}

fn search_path_entries(dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    for dir in dirs {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for bin_name in executable_names() {
            let candidate = dir.join(bin_name);
            if is_executable_file(&candidate) {
                return Some(canonicalize_clean(&candidate));
            }
        }
    }
    None
}

#[cfg(windows)]
fn executable_names() -> &'static [&'static str] {
    &["agy.exe", "agy.cmd", "agy.bat"]
}

#[cfg(not(windows))]
fn executable_names() -> &'static [&'static str] {
    &["agy"]
}

/// Checks whether `path` is a regular file (or symlink to one) that is executable by the OS.
#[must_use]
pub fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && (meta.permissions().mode() & 0o111 != 0)
    }

    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

fn platform_default_candidates() -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(8);

    #[cfg(windows)]
    {
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            let base = PathBuf::from(local_app_data);
            out.push(base.join("agy").join("bin").join("agy.exe"));
            out.push(
                base.join("Programs")
                    .join("Antigravity")
                    .join("bin")
                    .join("agy.exe"),
            );
            out.push(
                base.join("Programs")
                    .join("Antigravity")
                    .join("bin")
                    .join("agy.cmd"),
            );
        }
        if let Some(user_profile) = std::env::var_os("USERPROFILE") {
            let home = PathBuf::from(user_profile);
            out.push(home.join(".antigravity").join("bin").join("agy.exe"));
            out.push(home.join(".local").join("bin").join("agy.exe"));
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(home_os) = std::env::var_os("HOME") {
            let home = PathBuf::from(home_os);
            out.push(home.join(".local/bin/agy"));
            out.push(home.join(".antigravity/bin/agy"));
            out.push(home.join("Applications/Antigravity.app/Contents/Resources/app/bin/agy"));
        }
        out.push(PathBuf::from("/opt/homebrew/bin/agy"));
        out.push(PathBuf::from("/usr/local/bin/agy"));
        out.push(PathBuf::from(
            "/Applications/Antigravity.app/Contents/Resources/app/bin/agy",
        ));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(home_os) = std::env::var_os("HOME") {
            let home = PathBuf::from(home_os);
            out.push(home.join(".local/bin/agy"));
            out.push(home.join(".antigravity/bin/agy"));
        }
        out.push(PathBuf::from("/usr/local/bin/agy"));
        out.push(PathBuf::from("/usr/bin/agy"));
        out.push(PathBuf::from("/opt/antigravity/bin/agy"));
        out.push(PathBuf::from("/usr/share/antigravity/bin/agy"));
        out.push(PathBuf::from("/snap/bin/agy"));
    }

    out
}

/// Prepares an isolated working directory for ACP sessions across Windows, Linux, and macOS.
///
/// On multi-user Unix systems (Linux & macOS), `/tmp` is shared across UIDs; the default directory
/// is therefore scoped by effective UID (`agy-acp-gateway-cwd-<uid>`) and created with mode `0o700`
/// to prevent cross-user permission collisions.
pub fn prepare_isolated_session_cwd(
    explicit: Option<PathBuf>,
) -> Result<(PathBuf, PathBuf), (PathBuf, io::Error)> {
    let target = explicit.unwrap_or_else(default_isolated_cwd_path);

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(&target)
            .map_err(|err| (target.clone(), err))?;
    }

    #[cfg(not(unix))]
    {
        fs::create_dir_all(&target).map_err(|err| (target.clone(), err))?;
    }

    let canonical = canonicalize_clean(&target);
    Ok((target, canonical))
}

fn default_isolated_cwd_path() -> PathBuf {
    #[cfg(unix)]
    {
        // SAFETY: `geteuid` is thread-safe, infallible, and takes no pointers.
        let uid = unsafe { libc::geteuid() };
        std::env::temp_dir().join(format!("agy-acp-gateway-cwd-{uid}"))
    }

    #[cfg(not(unix))]
    {
        std::env::temp_dir().join("agy-acp-gateway-empty-cwd")
    }
}

/// Canonicalizes `path` while avoiding Windows `\\?\` verbatim prefixes that some child CLIs mishandle.
#[must_use]
pub fn canonicalize_clean(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Scrubs gateway-client variables from the `agy acp` subprocess environment, ensures
/// loopback addresses bypass proxies, and on Windows propagates the active WinINet system
/// proxy into `HTTP_PROXY`/`HTTPS_PROXY` when Go's `http.ProxyFromEnvironment` needs it.
pub fn configure_acp_child_env(cmd: &mut tokio::process::Command) {
    const CLIENT_ENV_VARS_TO_STRIP: &[&str] = &[
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_MODEL",
        "ANTHROPIC_SMALL_FAST_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "OPENAI_BASE_URL",
        "OPENAI_API_KEY",
        "AGY_API_KEY",
        "AGY_GATEWAY_API_KEY",
    ];
    for key in CLIENT_ENV_VARS_TO_STRIP {
        cmd.env_remove(key);
    }

    let merged_no_proxy = merge_loopback_no_proxy(
        std::env::var("NO_PROXY")
            .ok()
            .or_else(|| std::env::var("no_proxy").ok())
            .as_deref(),
    );
    cmd.env("NO_PROXY", &merged_no_proxy);
    cmd.env("no_proxy", &merged_no_proxy);

    #[cfg(windows)]
    {
        let has_proxy_env = ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"]
            .iter()
            .any(|k| {
                std::env::var_os(k)
                    .and_then(|v| v.into_string().ok())
                    .is_some_and(|s| !s.trim().is_empty())
            });
        if !has_proxy_env && let Some(proxy_url) = detect_windows_system_proxy() {
            tracing::info!(
                proxy = %proxy_url,
                "propagating Windows system proxy to `agy acp` subprocess"
            );
            cmd.env("HTTP_PROXY", &proxy_url);
            cmd.env("HTTPS_PROXY", &proxy_url);
            cmd.env("http_proxy", &proxy_url);
            cmd.env("https_proxy", &proxy_url);
        }
    }
}

fn merge_loopback_no_proxy(existing: Option<&str>) -> String {
    let mut entries: Vec<String> = existing
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    for required in ["127.0.0.1", "localhost", "::1"] {
        if !entries.iter().any(|e| e.eq_ignore_ascii_case(required)) {
            entries.push(required.to_owned());
        }
    }
    entries.join(",")
}

#[cfg(windows)]
fn detect_windows_system_proxy() -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const REG_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";

    let enable_out = std::process::Command::new("reg")
        .args(["query", REG_KEY, "/v", "ProxyEnable"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !enable_out.status.success() {
        return None;
    }
    let server_out = std::process::Command::new("reg")
        .args(["query", REG_KEY, "/v", "ProxyServer"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !server_out.status.success() {
        return None;
    }

    let enable_str = String::from_utf8_lossy(&enable_out.stdout);
    let server_str = String::from_utf8_lossy(&server_out.stdout);
    parse_windows_registry_proxy(&enable_str, &server_str)
}

#[must_use]
pub fn parse_windows_registry_proxy(enable_stdout: &str, server_stdout: &str) -> Option<String> {
    let enabled = enable_stdout.lines().any(|line| {
        line.contains("ProxyEnable")
            && line
                .split_whitespace()
                .last()
                .is_some_and(|v| v.eq_ignore_ascii_case("0x1") || v == "1")
    });
    if !enabled {
        return None;
    }

    let raw_server = server_stdout.lines().find_map(|line| {
        if !line.contains("ProxyServer") {
            return None;
        }
        line.split_whitespace().last()
    })?;

    let chosen = if raw_server.contains(';') || raw_server.contains('=') {
        let mut https_target = None;
        let mut http_target = None;
        let mut fallback_target = None;
        for part in raw_server
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Some((scheme, addr)) = part.split_once('=') {
                match scheme.trim().to_ascii_lowercase().as_str() {
                    "https" => https_target = Some(addr.trim()),
                    "http" => http_target = Some(addr.trim()),
                    _ => {
                        if fallback_target.is_none() {
                            fallback_target = Some(addr.trim());
                        }
                    }
                }
            } else if fallback_target.is_none() {
                fallback_target = Some(part);
            }
        }
        https_target.or(http_target).or(fallback_target)?
    } else {
        raw_server.trim()
    };

    if chosen.is_empty() {
        return None;
    }
    if chosen.contains("://") {
        Some(chosen.to_owned())
    } else {
        Some(format!("http://{chosen}"))
    }
}

/// Waits for an OS termination signal across Windows, Linux, and macOS so the HTTP server can
/// drain in-flight requests and cleanly tear down the `agy acp` process tree.
pub async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigint = signal(SignalKind::interrupt()).ok();
        let mut sigterm = signal(SignalKind::terminate()).ok();
        let mut sighup = signal(SignalKind::hangup()).ok();

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received ctrl-c, initiating graceful shutdown");
            }
            _ = recv_opt_unix_signal(&mut sigint), if sigint.is_some() => {
                tracing::info!("received SIGINT, initiating graceful shutdown");
            }
            _ = recv_opt_unix_signal(&mut sigterm), if sigterm.is_some() => {
                tracing::info!("received SIGTERM, initiating graceful shutdown");
            }
            _ = recv_opt_unix_signal(&mut sighup), if sighup.is_some() => {
                tracing::info!("received SIGHUP, initiating graceful shutdown");
            }
        }
    }

    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_shutdown};

        let mut on_break = ctrl_break().ok();
        let mut on_close = ctrl_close().ok();
        let mut on_shutdown = ctrl_shutdown().ok();

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received CTRL_C_EVENT, initiating graceful shutdown");
            }
            _ = recv_opt_win_break(&mut on_break), if on_break.is_some() => {
                tracing::info!("received CTRL_BREAK_EVENT, initiating graceful shutdown");
            }
            _ = recv_opt_win_close(&mut on_close), if on_close.is_some() => {
                tracing::info!("received CTRL_CLOSE_EVENT, initiating graceful shutdown");
            }
            _ = recv_opt_win_shutdown(&mut on_shutdown), if on_shutdown.is_some() => {
                tracing::info!("received CTRL_SHUTDOWN_EVENT, initiating graceful shutdown");
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(unix)]
async fn recv_opt_unix_signal(sig: &mut Option<tokio::signal::unix::Signal>) {
    if let Some(s) = sig {
        let _ = s.recv().await;
    }
}

#[cfg(windows)]
async fn recv_opt_win_break(sig: &mut Option<tokio::signal::windows::CtrlBreak>) {
    if let Some(s) = sig {
        let _ = s.recv().await;
    }
}

#[cfg(windows)]
async fn recv_opt_win_close(sig: &mut Option<tokio::signal::windows::CtrlClose>) {
    if let Some(s) = sig {
        let _ = s.recv().await;
    }
}

#[cfg(windows)]
async fn recv_opt_win_shutdown(sig: &mut Option<tokio::signal::windows::CtrlShutdown>) {
    if let Some(s) = sig {
        let _ = s.recv().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepares_isolated_cwd_cleanly() {
        let custom = std::env::temp_dir().join("agy-acp-gateway-platform-test");
        let (_, canonical) = prepare_isolated_session_cwd(Some(custom.clone())).unwrap();
        assert!(canonical.is_dir());
        let _ = fs::remove_dir(&custom);
    }

    #[test]
    fn rejects_missing_explicit_binary() {
        let missing = std::env::temp_dir().join("nonexistent-agy-binary-xyz-999");
        assert!(resolve_agy_binary(Some(missing)).is_none());
    }

    #[test]
    fn parses_windows_registry_proxy_variants() {
        let enabled_out = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings\r\n    ProxyEnable    REG_DWORD    0x1\r\n";
        let disabled_out = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings\r\n    ProxyEnable    REG_DWORD    0x0\r\n";
        let simple_srv = "    ProxyServer    REG_SZ    127.0.0.1:10811\r\n";
        let multi_srv = "    ProxyServer    REG_SZ    http=127.0.0.1:8080;https=127.0.0.1:8443\r\n";

        assert_eq!(
            parse_windows_registry_proxy(enabled_out, simple_srv).as_deref(),
            Some("http://127.0.0.1:10811")
        );
        assert_eq!(
            parse_windows_registry_proxy(enabled_out, multi_srv).as_deref(),
            Some("http://127.0.0.1:8443")
        );
        assert!(parse_windows_registry_proxy(disabled_out, simple_srv).is_none());
    }
}
