use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use liftgate_server::platform::{canonicalize_clean, is_executable_file, resolve_agy_binary};
use liftgate_types::config::{ProxyUrl, is_proxy_disable_keyword};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::CliError;

const MAX_LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

const fn default_true() -> bool {
    true
}

/// Resolved filesystem locations for Liftgate's global management state (`~/.liftgate` by default).
#[derive(Clone, Debug)]
pub struct StatePaths {
    root: PathBuf,
}

impl StatePaths {
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self, CliError> {
        let raw_root = if let Some(dir) = explicit {
            dir
        } else {
            user_home_dir()?.join(".liftgate")
        };
        Ok(Self { root: raw_root })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn daemon_file(&self) -> PathBuf {
        self.root.join("daemon.json")
    }

    #[must_use]
    pub fn settings_file(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    #[must_use]
    pub fn log_file(&self) -> PathBuf {
        self.root.join("gateway.log")
    }

    #[must_use]
    pub fn rotated_log_file(&self) -> PathBuf {
        self.root.join("gateway.log.1")
    }

    #[must_use]
    pub fn ephemeral_dir(&self) -> PathBuf {
        self.root.join("ephemeral")
    }

    #[must_use]
    pub fn desktop_dir(&self) -> PathBuf {
        self.root.join("desktop")
    }

    #[must_use]
    pub fn desktop_journals_dir(&self) -> PathBuf {
        self.root.join("desktop-journals")
    }

    pub fn ensure_root_dir(&self) -> Result<(), CliError> {
        ensure_private_dir(&self.root)
    }

    pub fn ensure_ephemeral_dir(&self) -> Result<PathBuf, CliError> {
        let dir = self.ephemeral_dir();
        ensure_private_dir(&dir)?;
        Ok(canonicalize_clean(&dir))
    }

    pub fn ensure_desktop_journals_dir(&self) -> Result<PathBuf, CliError> {
        let dir = self.desktop_journals_dir();
        ensure_private_dir(&dir)?;
        Ok(canonicalize_clean(&dir))
    }

    pub fn ensure_desktop_sessions_dir(&self) -> Result<PathBuf, CliError> {
        let dir = self.desktop_dir().join("sessions");
        ensure_private_dir(&dir)?;
        Ok(canonicalize_clean(&dir))
    }

    pub fn prepare_log_file_for_spawn(&self) -> Result<File, CliError> {
        self.ensure_root_dir()?;
        let log_path = self.log_file();
        if let Ok(meta) = fs::metadata(&log_path)
            && meta.len() >= MAX_LOG_ROTATE_BYTES
        {
            let _ = fs::rename(&log_path, self.rotated_log_file());
        }

        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(&log_path).map_err(|source| CliError::StateIo {
            path: log_path,
            source,
        })
    }

    pub fn read_daemon(&self) -> Result<Option<DaemonState>, CliError> {
        let path = self.daemon_file();
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(CliError::StateIo { path, source }),
        };
        let parsed = serde_json::from_slice(&bytes)
            .map_err(|source| CliError::StateJson { path, source })?;
        Ok(Some(parsed))
    }

    pub fn write_daemon(&self, state: &DaemonState) -> Result<(), CliError> {
        self.ensure_root_dir()?;
        let path = self.daemon_file();
        let mut json = serde_json::to_vec_pretty(state).map_err(|source| CliError::StateJson {
            path: path.clone(),
            source,
        })?;
        json.push(b'\n');
        write_file_atomic(&path, &json, true)
    }

    pub fn remove_daemon(&self) -> Result<(), CliError> {
        let path = self.daemon_file();
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(CliError::StateIo { path, source }),
        }
    }

    pub fn read_settings(&self) -> Result<LiftgateSettings, CliError> {
        let path = self.settings_file();
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LiftgateSettings::default());
            }
            Err(source) => return Err(CliError::StateIo { path, source }),
        };
        let parsed = serde_json::from_slice(&bytes)
            .map_err(|source| CliError::StateJson { path, source })?;
        Ok(parsed)
    }

    pub fn write_settings(&self, settings: &LiftgateSettings) -> Result<(), CliError> {
        self.ensure_root_dir()?;
        let path = self.settings_file();
        let mut json =
            serde_json::to_vec_pretty(settings).map_err(|source| CliError::StateJson {
                path: path.clone(),
                source,
            })?;
        json.push(b'\n');
        write_file_atomic(&path, &json, true)
    }
}

/// Persistent user-level preferences stored at `~/.liftgate/settings.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiftgateSettings {
    /// Forced outbound proxy URL injected into `agy` subprocesses (`None` = no proxy by default).
    #[serde(default)]
    pub agy_proxy: Option<ProxyUrl>,
    /// Automatically check and update `agy` CLI when the gateway is idle (`true` by default).
    #[serde(default = "default_true")]
    pub agy_auto_update: bool,
    /// Experimental multi-turn hydration; disabled unless explicitly selected.
    #[serde(default)]
    pub long_context: bool,
    /// Custom `agy` binary path recorded when `lg agy install --dir <path>` installs outside `PATH`.
    #[serde(default)]
    pub agy_bin: Option<PathBuf>,
    /// Forward-compatible unknown keys preserved across versions.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Default for LiftgateSettings {
    fn default() -> Self {
        Self {
            agy_proxy: None,
            agy_auto_update: true,
            long_context: false,
            agy_bin: None,
            extra: serde_json::Map::new(),
        }
    }
}

/// Resolves the effective outbound `agy` proxy (`CLI/env override` -> `settings.json` -> `None`).
pub fn resolve_effective_agy_proxy(
    cli_override: Option<&str>,
    settings: &LiftgateSettings,
) -> Result<Option<ProxyUrl>, CliError> {
    if let Some(raw) = cli_override.map(str::trim).filter(|s| !s.is_empty()) {
        if is_proxy_disable_keyword(raw) {
            return Ok(None);
        }
        return Ok(Some(ProxyUrl::try_new(raw)?));
    }
    Ok(settings.agy_proxy.clone())
}

/// Resolves the effective background idle `agy` auto-update toggle (`true` by default).
#[must_use]
pub const fn resolve_effective_agy_auto_update(
    cli_override: Option<bool>,
    cli_disable_flag: bool,
    settings: &LiftgateSettings,
) -> bool {
    if cli_disable_flag {
        false
    } else if let Some(explicit) = cli_override {
        explicit
    } else {
        settings.agy_auto_update
    }
}

/// Resolves the `agy` executable path, consulting saved `settings.agy_bin` when not yet on `PATH`.
#[must_use]
pub fn resolve_effective_agy_bin(
    explicit: Option<PathBuf>,
    settings: &LiftgateSettings,
) -> Option<PathBuf> {
    if explicit.is_some() {
        return resolve_agy_binary(explicit);
    }
    resolve_agy_binary(None).or_else(|| {
        settings
            .agy_bin
            .as_ref()
            .filter(|p| is_executable_file(p))
            .map(|p| canonicalize_clean(p))
    })
}

/// Persisted metadata describing a background `liftgate` daemon managed via `~/.liftgate/daemon.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonState {
    pub pid: u32,
    pub listen: SocketAddr,
    #[serde(default)]
    pub ui_listen: Option<SocketAddr>,
    #[serde(default)]
    pub default_model: String,
    #[serde(default)]
    pub acp_mode: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub agy_bin: Option<PathBuf>,
    #[serde(default)]
    pub acp_cwd: Option<PathBuf>,
    #[serde(default)]
    pub ui_dir: Option<PathBuf>,
    #[serde(default)]
    pub agy_proxy: Option<String>,
    #[serde(default = "default_true")]
    pub agy_auto_update: bool,
    /// Experimental multi-turn hydration; disabled unless explicitly selected.
    #[serde(default)]
    pub long_context: bool,
    #[serde(default)]
    pub shutdown_token: String,
    #[serde(default)]
    pub started_at_unix_ms: u64,
    #[serde(default)]
    pub log_file: PathBuf,
}

impl DaemonState {
    #[must_use]
    pub fn connect_base_url(&self) -> String {
        format_connect_url(self.listen)
    }

    #[must_use]
    pub fn dashboard_base_url(&self) -> String {
        self.ui_listen
            .map_or_else(|| self.connect_base_url(), format_connect_url)
    }
}

/// Formats a local `http://host:port` connection URL, rewriting wildcard bind addresses
/// (`0.0.0.0`, `[::]`) to loopback so local clients and browsers can connect directly.
#[must_use]
pub fn format_connect_url(addr: SocketAddr) -> String {
    match addr {
        SocketAddr::V4(v4) if v4.ip().is_unspecified() => {
            format!("http://127.0.0.1:{}", v4.port())
        }
        SocketAddr::V6(v6) if v6.ip().is_unspecified() => {
            format!("http://[::1]:{}", v6.port())
        }
        other => format!("http://{other}"),
    }
}

/// Returns the current user's home directory across Windows, macOS, and Linux.
pub fn user_home_dir() -> Result<PathBuf, CliError> {
    #[cfg(windows)]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE").filter(|s| !s.is_empty()) {
            return Ok(PathBuf::from(profile));
        }
    }
    if let Some(home) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    Err(CliError::NoHomeDirectory)
}

fn ensure_private_dir(dir: &Path) -> Result<(), CliError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(dir).map_err(|source| CliError::StateIo {
            path: dir.to_path_buf(),
            source,
        })
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir).map_err(|source| CliError::StateIo {
            path: dir.to_path_buf(),
            source,
        })
    }
}

struct UnlinkOnDrop<'a> {
    path: &'a Path,
    armed: bool,
}

impl Drop for UnlinkOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(self.path);
        }
    }
}

/// Atomically writes `bytes` to `target` via a sibling temporary file, `sync_all`, and `rename`.
pub fn write_file_atomic(target: &Path, bytes: &[u8], secret_mode: bool) -> Result<(), CliError> {
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| CliError::StateIo {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("state");
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}", Uuid::new_v4().simple()));

    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if secret_mode {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = secret_mode;

    let mut guard = UnlinkOnDrop {
        path: &tmp_path,
        armed: true,
    };

    let mut file = opts.open(&tmp_path).map_err(|source| CliError::StateIo {
        path: tmp_path.clone(),
        source,
    })?;
    file.write_all(bytes).map_err(|source| CliError::StateIo {
        path: tmp_path.clone(),
        source,
    })?;
    file.sync_all().map_err(|source| CliError::StateIo {
        path: tmp_path.clone(),
        source,
    })?;
    drop(file);

    fs::rename(&tmp_path, target).map_err(|source| CliError::StateIo {
        path: target.to_path_buf(),
        source,
    })?;
    guard.armed = false;
    Ok(())
}

/// Synchronous `Drop` guard that writes a temporary config/catalog file under `~/.liftgate/ephemeral`
/// and unlinks it automatically when the launched child CLI exits.
#[derive(Debug)]
pub struct EphemeralFileGuard {
    path: PathBuf,
}

impl EphemeralFileGuard {
    pub fn create(
        state_paths: &StatePaths,
        prefix: &str,
        extension: &str,
        contents: &[u8],
    ) -> Result<Self, CliError> {
        let dir = state_paths.ensure_ephemeral_dir()?;
        sweep_dead_ephemeral_files(&dir);
        let filename = format!(
            "{prefix}-{}-{}.{extension}",
            std::process::id(),
            Uuid::new_v4().simple()
        );
        let path = dir.join(filename);
        write_file_atomic(&path, contents, true)?;
        Ok(Self { path })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for EphemeralFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn sweep_dead_ephemeral_files(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let mut parts = stem.rsplitn(3, '-');
        let _uuid_part = parts.next();
        let Some(pid_part) = parts.next() else {
            continue;
        };
        if let Ok(pid) = pid_part.parse::<u32>()
            && !crate::daemon::is_pid_alive(pid)
        {
            if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_dir()) {
                let _ = fs::remove_dir_all(&path);
            } else {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

#[must_use]
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ephemeral_guard_removes_file_on_drop_leaving_zero_artifacts() {
        let temp_root =
            std::env::temp_dir().join(format!("liftgate-state-test-{}", Uuid::new_v4()));
        let paths = StatePaths::resolve(Some(temp_root.clone())).unwrap();
        let dead_orphan = paths
            .ensure_ephemeral_dir()
            .unwrap()
            .join("claude-99999999-deadbeef.json");
        fs::write(&dead_orphan, b"{}").unwrap();
        assert!(dead_orphan.is_file());

        let created_path = {
            let guard = EphemeralFileGuard::create(&paths, "claude", "json", b"{}").unwrap();
            assert!(guard.path().is_file());
            assert!(!dead_orphan.exists());
            guard.path().to_path_buf()
        };
        assert!(!created_path.exists());

        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn atomic_daemon_state_roundtrip() {
        let temp_root =
            std::env::temp_dir().join(format!("liftgate-daemon-test-{}", Uuid::new_v4()));
        let paths = StatePaths::resolve(Some(temp_root.clone())).unwrap();

        let sample = DaemonState {
            pid: 4242,
            listen: "127.0.0.1:8787".parse().unwrap(),
            ui_listen: Some("127.0.0.1:5173".parse().unwrap()),
            default_model: "gemini-4-carbon-high".to_owned(),
            acp_mode: "auto".to_owned(),
            api_key: Some("secret".to_owned()),
            agy_bin: None,
            acp_cwd: None,
            ui_dir: None,
            agy_proxy: None,
            agy_auto_update: true,
            long_context: false,
            shutdown_token: "tok-123".to_owned(),
            started_at_unix_ms: 1000,
            log_file: paths.log_file(),
        };
        paths.write_daemon(&sample).unwrap();
        let loaded = paths.read_daemon().unwrap().unwrap();
        assert_eq!(loaded.pid, 4242);
        assert_eq!(loaded.dashboard_base_url(), "http://127.0.0.1:5173");
        assert!(loaded.agy_auto_update);
        assert!(loaded.agy_proxy.is_none());

        paths.remove_daemon().unwrap();
        assert!(paths.read_daemon().unwrap().is_none());

        let defaults = paths.read_settings().unwrap();
        assert!(defaults.agy_proxy.is_none());
        assert!(defaults.agy_auto_update);

        let mut updated = defaults;
        updated.agy_proxy = Some(ProxyUrl::try_new("7890").unwrap());
        updated.agy_auto_update = false;
        paths.write_settings(&updated).unwrap();
        let reloaded = paths.read_settings().unwrap();
        assert_eq!(
            reloaded.agy_proxy.as_ref().map(ProxyUrl::as_str),
            Some("http://127.0.0.1:7890")
        );
        assert!(!reloaded.agy_auto_update);
        let _ = fs::remove_dir_all(&temp_root);
    }
}
