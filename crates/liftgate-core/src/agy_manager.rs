//! Antigravity CLI (`agy`) lifecycle, proxy injection, login delegation, cross-platform
//! installer, and background idle auto-updater.
//!
//! Legitimacy & security invariants:
//! - Uses ONLY official public Antigravity installer manifests (`install.sh` / `install.ps1`)
//!   and documented `agy` CLI subcommands (`agy --version`, `agy models`, `agy acp --login`,
//!   `agy install`, `agy mcp`, `agy plugin`, etc.).
//! - Never inspects internal credential stores or reverse-engineers closed binaries.
//! - Enforces SHA-512 verification fail-closed before staging or swapping any `agy` binary.
//! - Default proxy policy is strictly direct (`None`); configured proxies are forcibly injected
//!   exclusively into `agy` child processes and `agy` manifest/payload downloads.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use flate2::read::GzDecoder;
use liftgate_server::{
    acp_client::{BinarySwapGuard, LiveAcpClient},
    platform::{
        CLIENT_ENV_VARS_TO_STRIP, OUTBOUND_PROXY_ENV_KEYS, canonicalize_clean,
        configure_acp_child_env, detect_system_proxy_url, merge_loopback_no_proxy,
        split_expanded_path_var,
    },
    session_pool::SessionPool,
    telemetry::TelemetryHub,
};
use liftgate_types::config::{ProxyUrl, is_proxy_disable_keyword};
use liftgate_types::error::format_error_chain;
use liftgate_types::telemetry::GatewayTelemetrySnapshot;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha512};
use tar::Archive;
use thiserror::Error;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::timeout;
use uuid::Uuid;

use crate::cli::{
    AgyAutoUpdateArgs, AgyInstallArgs, AgyLoginArgs, AgyStatusArgs, AgyUpdateArgs,
    AutoUpdateToggle, ProxyArgs, RestartArgs,
};
use crate::client::GatewayClient;
use crate::daemon;
use crate::error::{ArchiveError, CliError};
use crate::state::{
    StatePaths, now_unix_ms, resolve_effective_agy_bin, resolve_effective_agy_proxy, user_home_dir,
};
use crate::terminal::{TerminalLaunchSpec, spawn_in_system_terminal};
use crate::upgrade::{
    DownloadProgressContext, SemVer, TransferProgress, TransferStage,
    download_url_with_policy_and_progress, emit_progress, fetch_bytes_with_policy,
};

pub const DEFAULT_AGY_MANIFEST_BASE_URL: &str =
    "https://antigravity-cli-auto-updater-974169037036.us-central1.run.app";

const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_PAYLOAD_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_EXTRACTED_BINARY_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_CLI_CAPTURE_BYTES: usize = 128 * 1024;
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(15);
const PAYLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const AGY_BACKUP_SUFFIX: &str = ".old-agy";

const DEFAULT_AUTO_UPDATE_INITIAL_DELAY_SECS: u64 = 45;
const DEFAULT_AUTO_UPDATE_CHECK_INTERVAL_SECS: u64 = 30 * 60;
const DEFAULT_AUTO_UPDATE_DEFER_RETRY_SECS: u64 = 60;
const DEFAULT_IDLE_QUIET_EMPTY_POOL_MS: u64 = 15_000;
const DEFAULT_IDLE_QUIET_WARM_POOL_MS: u64 = 5 * 60 * 1_000;

/// Official Antigravity CLI per-platform release manifest.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgyManifest {
    pub version: String,
    pub url: String,
    pub sha512: String,
}

/// Authentication & reachability state of `agy` determined via official CLI/gateway probes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AgyAuthStatus {
    SignedIn {
        model_count: usize,
        source: &'static str,
    },
    NotSignedIn {
        detail: String,
    },
    NetworkError {
        detail: String,
    },
    NotInstalled,
    Skipped,
}

impl AgyAuthStatus {
    #[must_use]
    pub const fn status_slug(&self) -> &'static str {
        match self {
            Self::SignedIn { .. } => "signed_in",
            Self::NotSignedIn { .. } => "not_signed_in",
            Self::NetworkError { .. } => "network_error",
            Self::NotInstalled => "not_installed",
            Self::Skipped => "skipped",
        }
    }

    #[must_use]
    pub const fn is_signed_in(&self) -> bool {
        matches!(self, Self::SignedIn { .. })
    }
}

struct StageDirGuard {
    path: PathBuf,
}

impl StageDirGuard {
    fn create(state_paths: &StatePaths, prefix: &str) -> Result<Self, CliError> {
        let base = state_paths.ensure_ephemeral_dir()?;
        let path = base.join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&path).map_err(|source| CliError::StateIo {
            path: path.clone(),
            source,
        })?;
        Ok(Self { path })
    }
}

impl Drop for StageDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Best-effort startup sweep removing stale `.old-agy` backup binaries left by previous updates.
pub fn sweep_stale_agy_backups(state_paths: &StatePaths) {
    if let Ok(dir) = default_agy_install_dir() {
        sweep_agy_backups_in_dir(&dir);
    }
    if let Ok(settings) = state_paths.read_settings()
        && let Some(bin) = settings.agy_bin
        && let Some(parent) = bin.parent()
    {
        sweep_agy_backups_in_dir(parent);
    }
}

fn sweep_agy_backups_in_dir(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.ends_with(AGY_BACKUP_SUFFIX))
        {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Maps the current OS and CPU architecture to Google's official Antigravity manifest slug.
pub fn detect_agy_platform_slug() -> Result<&'static str, CliError> {
    detect_agy_platform_for(
        std::env::consts::OS,
        std::env::consts::ARCH,
        is_linux_musl_host(),
    )
}

fn detect_agy_platform_for(
    os: &str,
    arch: &str,
    linux_musl: bool,
) -> Result<&'static str, CliError> {
    match (os, arch) {
        ("windows", "x86_64") => Ok("windows_amd64"),
        ("windows", "aarch64") => Ok("windows_arm64"),
        ("macos", "x86_64") => Ok("darwin_amd64"),
        ("macos", "aarch64") => Ok("darwin_arm64"),
        ("android", "aarch64") => Ok("android_arm64"),
        ("android", "x86_64") => Ok("android_amd64"),
        ("linux", "x86_64") => Ok(if linux_musl {
            "linux_amd64_musl"
        } else {
            "linux_amd64"
        }),
        ("linux", "aarch64") => Ok(if linux_musl {
            "linux_arm64_musl"
        } else {
            "linux_arm64"
        }),
        _ => Err(CliError::AgyPlatformUnsupported {
            os: os.to_owned(),
            arch: arch.to_owned(),
        }),
    }
}

fn is_linux_musl_host() -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new("/lib/libc.musl-x86_64.so.1").exists()
            || Path::new("/lib/libc.musl-aarch64.so.1").exists()
            || Path::new("/etc/alpine-release").exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Returns the default per-OS installation directory matching official `install.ps1` / `install.sh`.
pub fn default_agy_install_dir() -> Result<PathBuf, CliError> {
    #[cfg(windows)]
    {
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA").filter(|s| !s.is_empty()) {
            return Ok(PathBuf::from(local_app_data).join("agy").join("bin"));
        }
    }
    Ok(user_home_dir()?.join(".local").join("bin"))
}

const fn agy_binary_filename() -> &'static str {
    if cfg!(windows) { "agy.exe" } else { "agy" }
}

fn suppress_windows_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// Queries `<agy> --version` with bounded timeout and returns the normalized version string.
pub async fn probe_agy_version(agy_path: &Path, proxy: Option<&ProxyUrl>) -> Option<String> {
    let mut cmd = Command::new(agy_path);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_acp_child_env(&mut cmd, proxy);
    suppress_windows_console(&mut cmd);

    let (status_ok, stdout_str, stderr_str) = run_command_bounded(&mut cmd, Duration::from_secs(6))
        .await
        .ok()?;
    if !status_ok {
        return None;
    }
    let combined = if stdout_str.trim().is_empty() {
        stderr_str.trim()
    } else {
        stdout_str.trim()
    };
    extract_version_token(combined)
}

fn extract_version_token(raw: &str) -> Option<String> {
    let first_line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    for token in first_line.split_whitespace() {
        let cleaned = token.trim_matches(|c: char| c == 'v' || c == 'V' || c == ',' || c == ';');
        if SemVer::parse(cleaned).is_some() {
            return Some(cleaned.to_owned());
        }
    }
    Some(first_line.to_owned())
}

/// Checks whether `agy` is signed in using either an active local gateway catalog or `agy models`.
pub async fn probe_agy_auth(
    state_paths: &StatePaths,
    agy_path: &Path,
    proxy: Option<&ProxyUrl>,
) -> AgyAuthStatus {
    if let Ok(Some(daemon)) = state_paths.read_daemon()
        && daemon::is_pid_alive(daemon.pid)
    {
        let same_bin = daemon
            .agy_bin
            .as_ref()
            .is_none_or(|b| canonicalize_clean(b) == canonicalize_clean(agy_path));
        let same_proxy = daemon.agy_proxy.as_deref() == proxy.map(ProxyUrl::as_str);
        if same_bin && same_proxy {
            let client = GatewayClient::new(daemon.listen, daemon.api_key.clone());
            if client.check_health(Duration::from_millis(500)).await
                && let Ok(models) = client.fetch_models().await
                && models.len() > 1
            {
                return AgyAuthStatus::SignedIn {
                    model_count: models.len(),
                    source: "live gateway",
                };
            }
        }
    }

    let mut cmd = Command::new(agy_path);
    cmd.arg("models")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_acp_child_env(&mut cmd, proxy);
    suppress_windows_console(&mut cmd);

    match run_command_bounded(&mut cmd, Duration::from_secs(10)).await {
        Ok((status_ok, stdout_str, stderr_str)) => {
            let model_count = count_agy_models_output(&stdout_str);
            if status_ok && model_count > 0 {
                return AgyAuthStatus::SignedIn {
                    model_count,
                    source: "agy models",
                };
            }
            classify_auth_probe_failure(&stdout_str, &stderr_str)
        }
        Err(err) => AgyAuthStatus::NetworkError {
            detail: format_error_chain(&err),
        },
    }
}

fn count_agy_models_output(stdout_text: &str) -> usize {
    stdout_text
        .lines()
        .map(str::trim)
        .filter(|line| {
            if line.is_empty() || line.starts_with("Fetching") {
                return false;
            }
            if let Some((id_part, _name_part)) = line.split_once('\t') {
                let id = id_part.trim();
                !id.is_empty() && !id.contains(' ')
            } else {
                false
            }
        })
        .count()
}

fn classify_auth_probe_failure(stdout_text: &str, stderr_text: &str) -> AgyAuthStatus {
    let combined = format!("{}\n{}", stderr_text.trim(), stdout_text.trim())
        .trim()
        .to_owned();
    let summary_line = combined
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty() && !l.starts_with("Fetching"))
        .unwrap_or("authentication required")
        .to_owned();
    let lower = combined.to_ascii_lowercase();

    let looks_like_auth = lower.contains("login")
        || lower.contains("sign in")
        || lower.contains("signin")
        || lower.contains("unauthenticated")
        || lower.contains("not authenticated")
        || lower.contains("credential")
        || lower.contains("oauth")
        || lower.contains("token expired")
        || lower.contains("401")
        || lower.contains("permission_denied");
    if looks_like_auth {
        return AgyAuthStatus::NotSignedIn {
            detail: summary_line,
        };
    }

    let looks_like_network = lower.contains("proxy")
        || lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("no such host")
        || lower.contains("dial tcp")
        || lower.contains("tls")
        || lower.contains("certificate")
        || lower.contains("unreachable")
        || lower.contains("context deadline exceeded");
    if looks_like_network {
        return AgyAuthStatus::NetworkError {
            detail: summary_line,
        };
    }

    AgyAuthStatus::NotSignedIn {
        detail: summary_line,
    }
}

#[derive(Debug, Error)]
enum CommandExecError {
    #[error("failed to spawn process")]
    Spawn(#[source] io::Error),

    #[error("failed waiting for child process")]
    Wait(#[source] io::Error),

    #[error("command timed out after {secs}s")]
    TimedOut { secs: u64 },
}

async fn run_command_bounded(
    cmd: &mut Command,
    max_wait: Duration,
) -> Result<(bool, String, String), CommandExecError> {
    let mut child = cmd.spawn().map_err(CommandExecError::Spawn)?;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let read_pair = async {
        let out_fut = read_pipe_bounded(stdout_pipe, MAX_CLI_CAPTURE_BYTES);
        let err_fut = read_pipe_bounded(stderr_pipe, MAX_CLI_CAPTURE_BYTES);
        let (out_bytes, err_bytes) = tokio::join!(out_fut, err_fut);
        let status = child.wait().await.map_err(CommandExecError::Wait)?;
        Ok::<_, CommandExecError>((
            status.success(),
            String::from_utf8_lossy(&out_bytes).into_owned(),
            String::from_utf8_lossy(&err_bytes).into_owned(),
        ))
    };

    if let Ok(res) = timeout(max_wait, read_pair).await {
        res
    } else {
        let _ = child.kill().await;
        let _ = child.wait().await;
        Err(CommandExecError::TimedOut {
            secs: max_wait.as_secs(),
        })
    }
}

fn map_agy_transfer_error(err: CliError) -> CliError {
    match err {
        CliError::UpgradeNetwork { url, source } => CliError::AgyNetwork { url, source },
        other => other,
    }
}

async fn read_pipe_bounded<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<R>,
    cap_bytes: usize,
) -> Vec<u8> {
    let Some(r) = pipe else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut limited = r.take(cap_bytes as u64);
    let _ = limited.read_to_end(&mut out).await;
    out
}

/// Fetches and validates the official Antigravity CLI release manifest for `platform_slug`.
pub async fn fetch_agy_manifest(
    manifest_base_override: Option<&str>,
    platform_slug: &str,
    proxy: Option<&ProxyUrl>,
) -> Result<AgyManifest, CliError> {
    let base = manifest_base_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_AGY_MANIFEST_BASE_URL)
        .trim_end_matches('/');
    let manifest_url = format!("{base}/manifests/{platform_slug}.json");
    let bytes = fetch_bytes_with_policy(
        &manifest_url,
        MAX_MANIFEST_BYTES,
        MANIFEST_TIMEOUT,
        proxy.map(ProxyUrl::as_str),
        true,
        false,
    )
    .await
    .map_err(map_agy_transfer_error)?;

    let parsed: AgyManifest =
        serde_json::from_slice(&bytes).map_err(|source| CliError::AgyArchive {
            asset: format!("{platform_slug}.json"),
            source: ArchiveError::MalformedManifestJson { source },
        })?;
    let clean_ver = parsed.version.trim().to_owned();
    let clean_url = parsed.url.trim().to_owned();
    let clean_hash = parsed.sha512.trim().to_ascii_lowercase();
    if clean_ver.is_empty() || clean_url.is_empty() || !is_valid_sha512_hex(&clean_hash) {
        return Err(CliError::AgyArchive {
            asset: format!("{platform_slug}.json"),
            source: ArchiveError::InvalidManifestFields,
        });
    }
    Ok(AgyManifest {
        version: clean_ver,
        url: clean_url,
        sha512: clean_hash,
    })
}

fn is_valid_sha512_hex(s: &str) -> bool {
    s.len() == 128 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn compute_file_sha512_hex(path: &Path) -> Result<String, CliError> {
    let mut file = File::open(path).map_err(|source| CliError::StateIo {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha512::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|source| CliError::StateIo {
            path: path.to_path_buf(),
            source,
        })?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            hasher.update(chunk);
        }
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(128);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

fn is_tar_gz_url(url: &str) -> bool {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let lower = without_query.to_ascii_lowercase();
    lower.ends_with(".tar.gz") || lower.ends_with(".tgz")
}

fn asset_name_from_url(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    without_query
        .rsplit('/')
        .find(|seg| !seg.is_empty())
        .unwrap_or("agy-payload")
        .to_owned()
}

fn extract_agy_from_tar_gz(archive_path: &Path, dest_bin: &Path) -> Result<(), CliError> {
    extract_agy_from_tar_gz_with_limit(archive_path, dest_bin, MAX_EXTRACTED_BINARY_BYTES)
}

fn extract_agy_from_tar_gz_with_limit(
    archive_path: &Path,
    dest_bin: &Path,
    max_extracted_bytes: u64,
) -> Result<(), CliError> {
    let asset_name = archive_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("agy.tar.gz")
        .to_owned();
    let file = File::open(archive_path).map_err(|source| CliError::StateIo {
        path: archive_path.to_path_buf(),
        source,
    })?;
    let gz = GzDecoder::new(file);
    let mut archive = Archive::new(gz);
    let entries = archive.entries().map_err(|source| CliError::AgyArchive {
        asset: asset_name.clone(),
        source: ArchiveError::CorruptTarGz { source },
    })?;

    for entry_res in entries {
        let entry = entry_res.map_err(|source| CliError::AgyArchive {
            asset: asset_name.clone(),
            source: ArchiveError::CorruptTarEntry { source },
        })?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let entry_path = entry.path().map_err(|source| CliError::AgyArchive {
            asset: asset_name.clone(),
            source: ArchiveError::InvalidTarEntryPath { source },
        })?;
        let Some(fname) = entry_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if matches!(fname, "antigravity" | "agy" | "antigravity.exe" | "agy.exe") {
            let mut out = File::create(dest_bin).map_err(|source| CliError::StateIo {
                path: dest_bin.to_path_buf(),
                source,
            })?;
            let mut bounded = entry.take(max_extracted_bytes.saturating_add(1));
            let written =
                io::copy(&mut bounded, &mut out).map_err(|source| CliError::AgyArchive {
                    asset: asset_name.clone(),
                    source: ArchiveError::Decompress { source },
                })?;
            if written > max_extracted_bytes {
                let _ = fs::remove_file(dest_bin);
                return Err(CliError::AgyArchive {
                    asset: asset_name,
                    source: ArchiveError::ExtractedBinaryTooLarge {
                        limit_mib: (max_extracted_bytes / (1024 * 1024)).max(1),
                    },
                });
            }
            out.sync_all().map_err(|source| CliError::StateIo {
                path: dest_bin.to_path_buf(),
                source,
            })?;
            return Ok(());
        }
    }

    Err(CliError::AgyArchive {
        asset: asset_name,
        source: ArchiveError::MissingExecutable {
            expected: "agy".to_owned(),
        },
    })
}

fn make_executable_and_unquarantine(bin_path: &Path) -> Result<(), CliError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(bin_path, perms).map_err(|source| CliError::StateIo {
            path: bin_path.to_path_buf(),
            source,
        })?;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(bin_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(unix))]
    {
        let _ = bin_path;
    }
    Ok(())
}

/// Downloads the official `agy` payload into `stage_dir`, verifies its SHA-512 checksum
/// fail-closed, extracts if `.tar.gz`, marks executable, and smoke-tests `--version`.
async fn download_and_verify_agy_staged(
    manifest: &AgyManifest,
    stage_dir: &Path,
    proxy: Option<&ProxyUrl>,
    progress_tx: Option<&watch::Sender<TransferProgress>>,
) -> Result<PathBuf, CliError> {
    let payload_path = stage_dir.join("agy-payload.download");
    let asset_name = asset_name_from_url(&manifest.url);
    let target_version = Some(format!("v{}", manifest.version));
    let dl_ctx = progress_tx.map(|tx| DownloadProgressContext {
        tx,
        asset_name: asset_name.clone(),
        target_version: target_version.clone(),
        known_total_bytes: None,
    });
    download_url_with_policy_and_progress(
        &manifest.url,
        &payload_path,
        MAX_PAYLOAD_BYTES,
        PAYLOAD_TIMEOUT,
        proxy.map(ProxyUrl::as_str),
        true,
        dl_ctx.as_ref(),
    )
    .await
    .map_err(map_agy_transfer_error)?;

    emit_progress(
        progress_tx,
        TransferProgress::stage(
            TransferStage::Verifying,
            format!("Verifying SHA-512 checksum of {asset_name}…"),
            Some(asset_name.clone()),
            target_version.clone(),
        ),
    );
    let actual_sha512 = compute_file_sha512_hex(&payload_path)?;
    if !actual_sha512.eq_ignore_ascii_case(&manifest.sha512) {
        let _ = fs::remove_file(&payload_path);
        return Err(CliError::AgyChecksumMismatch {
            asset: manifest.url.clone(),
            expected: manifest.sha512.clone(),
            actual: actual_sha512,
        });
    }

    let staged_bin = stage_dir.join(agy_binary_filename());
    if is_tar_gz_url(&manifest.url) {
        emit_progress(
            progress_tx,
            TransferProgress::stage(
                TransferStage::Extracting,
                format!("Extracting `agy` binary from {asset_name}…"),
                Some(asset_name.clone()),
                target_version.clone(),
            ),
        );
        extract_agy_from_tar_gz(&payload_path, &staged_bin)?;
        let _ = fs::remove_file(&payload_path);
    } else {
        fs::rename(&payload_path, &staged_bin).map_err(|source| CliError::StateIo {
            path: staged_bin.clone(),
            source,
        })?;
    }

    make_executable_and_unquarantine(&staged_bin)?;

    // Smoke-test `--version` before ever touching the destination path.
    emit_progress(
        progress_tx,
        TransferProgress::stage(
            TransferStage::Testing,
            "Running `agy --version` smoke check…",
            Some(asset_name.clone()),
            target_version,
        ),
    );
    if probe_agy_version(&staged_bin, proxy).await.is_none() {
        return Err(CliError::AgyArchive {
            asset: asset_name,
            source: ArchiveError::AgySmokeCheckFailed,
        });
    }

    Ok(staged_bin)
}

/// Atomically swaps `staged_bin` onto `target_bin`, returning a [`BinarySwapGuard`] that
/// restores the previous binary if dropped before `.commit()`.
pub fn install_staged_agy_binary(
    staged_bin: &Path,
    target_bin: &Path,
) -> io::Result<BinarySwapGuard> {
    if let Some(parent) = target_bin.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let backup_path = if target_bin.exists() {
        let file_name = target_bin
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(agy_binary_filename());
        let bak = target_bin.with_file_name(format!("{file_name}{AGY_BACKUP_SUFFIX}"));
        let _ = fs::remove_file(&bak);
        fs::rename(target_bin, &bak)?;
        Some(bak)
    } else {
        None
    };

    let guard = BinarySwapGuard::new(target_bin.to_path_buf(), backup_path);
    if fs::rename(staged_bin, target_bin).is_err() {
        fs::copy(staged_bin, target_bin)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(target_bin, fs::Permissions::from_mode(0o755));
        }
        let _ = fs::remove_file(staged_bin);
    }
    Ok(guard)
}

fn map_install_io_error(target: &Path, source: io::Error) -> CliError {
    if source.kind() == io::ErrorKind::PermissionDenied {
        CliError::UpgradePermissionDenied {
            path: target.to_path_buf(),
            source,
        }
    } else {
        CliError::StateIo {
            path: target.to_path_buf(),
            source,
        }
    }
}

// ============================================================================
// `lg proxy` & `lg agy proxy`
// ============================================================================

#[derive(Clone, Debug, Serialize)]
pub struct ProxyConnectivityProbe {
    pub ok: bool,
    pub latency_ms: u64,
    pub latest_agy_version: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProxyOutcome {
    pub updated: bool,
    pub configured_proxy: Option<ProxyUrl>,
    pub env_override: Option<String>,
    pub effective_proxy: Option<ProxyUrl>,
    pub ambient_system_proxy: Option<String>,
    pub settings_file: PathBuf,
    pub daemon_restarted: bool,
    pub connectivity_probe: Option<ProxyConnectivityProbe>,
}

#[must_use]
pub fn detect_ambient_system_proxy() -> Option<String> {
    detect_system_proxy_url()
}

pub async fn execute_proxy(
    state_paths: &StatePaths,
    args: &ProxyArgs,
) -> Result<ProxyOutcome, CliError> {
    let mut settings = state_paths.read_settings()?;
    let previous_proxy = settings.agy_proxy.clone();
    let mut mutated = false;

    if args.off {
        settings.agy_proxy = None;
        mutated = true;
    } else if args.from_system {
        let detected = detect_system_proxy_url().ok_or(CliError::NoSystemProxyDetected)?;
        settings.agy_proxy = Some(ProxyUrl::try_new(detected)?);
        mutated = true;
    } else if let Some(raw_target) = args
        .target
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if is_proxy_disable_keyword(raw_target) {
            settings.agy_proxy = None;
            mutated = true;
        } else if raw_target.eq_ignore_ascii_case("system") {
            let detected = detect_system_proxy_url().ok_or(CliError::NoSystemProxyDetected)?;
            settings.agy_proxy = Some(ProxyUrl::try_new(detected)?);
            mutated = true;
        } else {
            settings.agy_proxy = Some(ProxyUrl::try_new(raw_target)?);
            mutated = true;
        }
    }

    if mutated {
        state_paths.write_settings(&settings)?;
    }

    let env_override_raw = std::env::var("AGY_PROXY")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let effective_proxy = resolve_effective_agy_proxy(env_override_raw.as_deref(), &settings)?;

    let mut daemon_restarted = false;
    if mutated
        && !args.no_restart
        && previous_proxy != settings.agy_proxy
        && let Some(d) = state_paths.read_daemon()?
        && daemon::is_pid_alive(d.pid)
    {
        let _ = daemon::restart_daemon(state_paths, RestartArgs::default()).await?;
        daemon_restarted = true;
    }

    let connectivity_probe = if args.test {
        let slug = detect_agy_platform_slug().unwrap_or("windows_amd64");
        let started = Instant::now();
        let res = fetch_agy_manifest(None, slug, effective_proxy.as_ref()).await;
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Some(match res {
            Ok(m) => ProxyConnectivityProbe {
                ok: true,
                latency_ms: elapsed_ms,
                latest_agy_version: Some(m.version),
                error: None,
            },
            Err(err) => ProxyConnectivityProbe {
                ok: false,
                latency_ms: elapsed_ms,
                latest_agy_version: None,
                error: Some(err.display_chain()),
            },
        })
    } else {
        None
    };

    Ok(ProxyOutcome {
        updated: mutated,
        configured_proxy: settings.agy_proxy,
        env_override: env_override_raw,
        effective_proxy,
        ambient_system_proxy: detect_system_proxy_url(),
        settings_file: state_paths.settings_file(),
        daemon_restarted,
        connectivity_probe,
    })
}

// ============================================================================
// `lg agy` Structured Headless Control Plane
// ============================================================================

#[derive(Clone, Debug, Serialize)]
pub struct AgyStatusReport {
    pub installed: bool,
    pub binary_path: Option<PathBuf>,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub auth_status: AgyAuthStatus,
    pub effective_proxy: Option<ProxyUrl>,
    pub auto_update_enabled: bool,
    pub live_telemetry: Option<GatewayTelemetrySnapshot>,
}

pub async fn inspect_agy_status(
    state_paths: &StatePaths,
    explicit_agy_bin: Option<PathBuf>,
    args: &AgyStatusArgs,
) -> Result<AgyStatusReport, CliError> {
    let settings = state_paths.read_settings()?;
    let env_proxy = std::env::var("AGY_PROXY").ok();
    let effective_proxy = resolve_effective_agy_proxy(env_proxy.as_deref(), &settings)?;
    let resolved_bin = resolve_effective_agy_bin(explicit_agy_bin, &settings);

    let installed_version = match &resolved_bin {
        Some(bin_path) => probe_agy_version(bin_path, effective_proxy.as_ref()).await,
        None => None,
    };

    let latest_manifest = if args.offline {
        None
    } else if let Ok(slug) = detect_agy_platform_slug() {
        timeout(
            Duration::from_secs(5),
            fetch_agy_manifest(None, slug, effective_proxy.as_ref()),
        )
        .await
        .ok()
        .and_then(Result::ok)
    } else {
        None
    };

    let auth_status = match (&resolved_bin, args.no_auth_check) {
        (None, _) => AgyAuthStatus::NotInstalled,
        (Some(_), true) => AgyAuthStatus::Skipped,
        (Some(bin_path), false) => {
            probe_agy_auth(state_paths, bin_path, effective_proxy.as_ref()).await
        }
    };

    let daemon = state_paths.read_daemon().ok().flatten();
    let live_telemetry = if let Some(d) = &daemon
        && daemon::is_pid_alive(d.pid)
    {
        let client = GatewayClient::new(d.listen, d.api_key.clone());
        client.fetch_telemetry().await.ok()
    } else {
        None
    };

    let update_available = match (&installed_version, &latest_manifest) {
        (Some(cur), Some(m)) => match (SemVer::parse(cur), SemVer::parse(&m.version)) {
            (Some(c), Some(l)) => l > c,
            _ => m.version != *cur,
        },
        _ => false,
    };

    Ok(AgyStatusReport {
        installed: resolved_bin.is_some(),
        binary_path: resolved_bin,
        version: installed_version,
        latest_version: latest_manifest.map(|m| m.version),
        update_available,
        auth_status,
        effective_proxy,
        auto_update_enabled: settings.agy_auto_update,
        live_telemetry,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgyInstallStatus {
    Checked,
    AlreadyInstalled,
    Installed,
}

#[derive(Clone, Debug, Serialize)]
pub struct AgyInstallOutcome {
    pub status: AgyInstallStatus,
    pub platform: String,
    pub installed: bool,
    pub previous_version: Option<String>,
    pub version: String,
    pub sha512: String,
    pub binary_path: PathBuf,
    pub target_path: PathBuf,
    pub up_to_date: bool,
    pub dir_in_path: bool,
    pub daemon_restarted: bool,
}

pub async fn execute_agy_install(
    state_paths: &StatePaths,
    args: &AgyInstallArgs,
) -> Result<AgyInstallOutcome, CliError> {
    execute_agy_install_with_progress(state_paths, args, None).await
}

pub async fn execute_agy_install_with_progress(
    state_paths: &StatePaths,
    args: &AgyInstallArgs,
    progress: Option<watch::Sender<TransferProgress>>,
) -> Result<AgyInstallOutcome, CliError> {
    let progress_tx = progress.as_ref();
    let mut settings = state_paths.read_settings()?;
    let cli_proxy = args
        .proxy
        .clone()
        .or_else(|| std::env::var("AGY_PROXY").ok());
    let effective_proxy = resolve_effective_agy_proxy(cli_proxy.as_deref(), &settings)?;
    let platform_slug = detect_agy_platform_slug()?;

    let target_dir = match &args.dir {
        Some(custom) => custom.clone(),
        None => default_agy_install_dir()?,
    };
    let target_bin = target_dir.join(agy_binary_filename());
    let existing_bin = if target_bin.is_file() {
        Some(canonicalize_clean(&target_bin))
    } else {
        resolve_effective_agy_bin(None, &settings)
    };
    let current_ver = match &existing_bin {
        Some(p) => probe_agy_version(p, effective_proxy.as_ref()).await,
        None => None,
    };

    emit_progress(
        progress_tx,
        TransferProgress::checking(format!(
            "Fetching official Antigravity manifest ({platform_slug})…"
        )),
    );
    let manifest = fetch_agy_manifest(
        args.manifest_base_url.as_deref(),
        platform_slug,
        effective_proxy.as_ref(),
    )
    .await?;

    let already_up_to_date = args.dir.is_none()
        && existing_bin.is_some()
        && current_ver.as_deref().is_some_and(|cur| {
            match (SemVer::parse(cur), SemVer::parse(&manifest.version)) {
                (Some(c), Some(l)) => c >= l,
                _ => cur == manifest.version,
            }
        });

    if args.check {
        let installed = existing_bin.is_some();
        let binary_path = existing_bin.unwrap_or_else(|| target_bin.clone());
        let dir_in_path = binary_path.parent().is_some_and(|dir| {
            split_expanded_path_var()
                .into_iter()
                .any(|p| canonicalize_clean(&p) == canonicalize_clean(dir))
        });
        return Ok(AgyInstallOutcome {
            status: AgyInstallStatus::Checked,
            platform: platform_slug.to_owned(),
            installed,
            previous_version: current_ver,
            version: manifest.version,
            sha512: manifest.sha512,
            binary_path,
            target_path: target_bin,
            up_to_date: already_up_to_date,
            dir_in_path,
            daemon_restarted: false,
        });
    }

    if already_up_to_date
        && !args.force
        && let Some(existing_path) = existing_bin
    {
        let dir_in_path = existing_path.parent().is_some_and(|dir| {
            split_expanded_path_var()
                .into_iter()
                .any(|p| canonicalize_clean(&p) == canonicalize_clean(dir))
        });
        let version = current_ver.clone().unwrap_or(manifest.version);
        return Ok(AgyInstallOutcome {
            status: AgyInstallStatus::AlreadyInstalled,
            platform: platform_slug.to_owned(),
            installed: true,
            previous_version: current_ver,
            version,
            sha512: manifest.sha512,
            binary_path: existing_path,
            target_path: target_bin,
            up_to_date: true,
            dir_in_path,
            daemon_restarted: false,
        });
    }

    let install_dest = if args.dir.is_some() {
        target_bin.clone()
    } else {
        existing_bin.unwrap_or_else(|| target_bin.clone())
    };

    let stage = StageDirGuard::create(state_paths, "agy-install-stage")?;
    let staged_bin = download_and_verify_agy_staged(
        &manifest,
        &stage.path,
        effective_proxy.as_ref(),
        progress_tx,
    )
    .await?;

    emit_progress(
        progress_tx,
        TransferProgress::stage(
            TransferStage::Installing,
            format!("Installing `agy` v{} & updating gateway…", manifest.version),
            Some(asset_name_from_url(&manifest.url)),
            Some(format!("v{}", manifest.version)),
        ),
    );
    // If a managed background daemon is running on the binary we're replacing, stop it cleanly first.
    let was_daemon_running = if let Some(d) = state_paths.read_daemon()?
        && daemon::is_pid_alive(d.pid)
    {
        let _ = daemon::stop_daemon(state_paths).await?;
        true
    } else {
        false
    };

    let swap_guard = install_staged_agy_binary(&staged_bin, &install_dest)
        .map_err(|e| map_install_io_error(&install_dest, e))?;
    swap_guard.commit();
    let clean_installed = canonicalize_clean(&install_dest);

    // Run official `agy install` PATH setup (absorbing non-fatal shell profile warnings).
    if !args.skip_path {
        let mut setup_cmd = Command::new(&clean_installed);
        setup_cmd.arg("install").arg("--skip-aliases");
        if let Some(custom_dir) = &args.dir {
            setup_cmd.arg("--dir").arg(custom_dir);
        }
        setup_cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_acp_child_env(&mut setup_cmd, effective_proxy.as_ref());
        suppress_windows_console(&mut setup_cmd);
        let _ = timeout(Duration::from_secs(10), setup_cmd.status()).await;
    }

    // Persist custom `agy_bin` in `settings.json` so `lg` locates it immediately even before shell restart.
    settings.agy_bin = Some(clean_installed.clone());
    state_paths.write_settings(&settings)?;

    if was_daemon_running {
        let _ = daemon::restart_daemon(state_paths, RestartArgs::default()).await?;
    }

    let dir_in_path = clean_installed.parent().is_some_and(|dir| {
        split_expanded_path_var()
            .into_iter()
            .any(|p| canonicalize_clean(&p) == canonicalize_clean(dir))
    });
    Ok(AgyInstallOutcome {
        status: AgyInstallStatus::Installed,
        platform: platform_slug.to_owned(),
        installed: true,
        previous_version: current_ver,
        version: manifest.version,
        sha512: manifest.sha512,
        binary_path: clean_installed,
        target_path: target_bin,
        up_to_date: true,
        dir_in_path,
        daemon_restarted: was_daemon_running,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgyUpdateStatus {
    UpdateAvailable,
    UpToDate,
    Updated,
    InstalledFresh,
}

#[derive(Clone, Debug, Serialize)]
pub struct AgyUpdateOutcome {
    pub status: AgyUpdateStatus,
    pub previous_version: Option<String>,
    pub latest_version: String,
    pub update_available: bool,
    pub sha512: String,
    pub binary_path: PathBuf,
    pub daemon_restarted: bool,
}

pub async fn execute_agy_update(
    state_paths: &StatePaths,
    explicit_agy_bin: Option<PathBuf>,
    args: &AgyUpdateArgs,
) -> Result<AgyUpdateOutcome, CliError> {
    execute_agy_update_with_progress(state_paths, explicit_agy_bin, args, None).await
}

pub async fn execute_agy_update_with_progress(
    state_paths: &StatePaths,
    explicit_agy_bin: Option<PathBuf>,
    args: &AgyUpdateArgs,
    progress: Option<watch::Sender<TransferProgress>>,
) -> Result<AgyUpdateOutcome, CliError> {
    let progress_tx = progress.as_ref();
    let mut settings = state_paths.read_settings()?;
    let cli_proxy = args
        .proxy
        .clone()
        .or_else(|| std::env::var("AGY_PROXY").ok());
    let effective_proxy = resolve_effective_agy_proxy(cli_proxy.as_deref(), &settings)?;

    let Some(target_bin) = resolve_effective_agy_bin(explicit_agy_bin, &settings) else {
        if args.check {
            return Err(CliError::AgyBinaryNotFound);
        }
        let inst = execute_agy_install_with_progress(
            state_paths,
            &AgyInstallArgs {
                check: false,
                force: args.force,
                dir: None,
                proxy: args.proxy.clone(),
                skip_path: false,
                manifest_base_url: args.manifest_base_url.clone(),
            },
            progress,
        )
        .await?;
        return Ok(AgyUpdateOutcome {
            status: AgyUpdateStatus::InstalledFresh,
            previous_version: inst.previous_version,
            latest_version: inst.version,
            update_available: false,
            sha512: inst.sha512,
            binary_path: inst.binary_path,
            daemon_restarted: inst.daemon_restarted,
        });
    };

    let platform_slug = detect_agy_platform_slug()?;
    let current_ver = probe_agy_version(&target_bin, effective_proxy.as_ref()).await;
    emit_progress(
        progress_tx,
        TransferProgress::checking(format!(
            "Checking official Antigravity manifest ({platform_slug})…"
        )),
    );
    let manifest = fetch_agy_manifest(
        args.manifest_base_url.as_deref(),
        platform_slug,
        effective_proxy.as_ref(),
    )
    .await?;

    let update_available = match &current_ver {
        Some(cur) => match (SemVer::parse(cur), SemVer::parse(&manifest.version)) {
            (Some(c), Some(l)) => l > c,
            _ => *cur != manifest.version,
        },
        None => true,
    };

    if args.check || (!update_available && !args.force) {
        return Ok(AgyUpdateOutcome {
            status: if update_available {
                AgyUpdateStatus::UpdateAvailable
            } else {
                AgyUpdateStatus::UpToDate
            },
            previous_version: current_ver,
            latest_version: manifest.version,
            update_available,
            sha512: manifest.sha512,
            binary_path: target_bin,
            daemon_restarted: false,
        });
    }

    let stage = StageDirGuard::create(state_paths, "agy-update-stage")?;
    let staged_bin = download_and_verify_agy_staged(
        &manifest,
        &stage.path,
        effective_proxy.as_ref(),
        progress_tx,
    )
    .await?;

    emit_progress(
        progress_tx,
        TransferProgress::stage(
            TransferStage::Installing,
            format!(
                "Installing `agy` v{} & restarting gateway…",
                manifest.version
            ),
            Some(asset_name_from_url(&manifest.url)),
            Some(format!("v{}", manifest.version)),
        ),
    );
    let should_restart_daemon = !args.no_restart
        && state_paths
            .read_daemon()?
            .is_some_and(|d| daemon::is_pid_alive(d.pid));
    if should_restart_daemon {
        let _ = daemon::stop_daemon(state_paths).await?;
    }

    let swap_guard = install_staged_agy_binary(&staged_bin, &target_bin)
        .map_err(|e| map_install_io_error(&target_bin, e))?;
    swap_guard.commit();

    settings.agy_bin = Some(canonicalize_clean(&target_bin));
    state_paths.write_settings(&settings)?;

    if should_restart_daemon {
        let _ = daemon::restart_daemon(state_paths, RestartArgs::default()).await?;
    }

    Ok(AgyUpdateOutcome {
        status: AgyUpdateStatus::Updated,
        previous_version: current_ver,
        latest_version: manifest.version,
        update_available: false,
        sha512: manifest.sha512,
        binary_path: target_bin,
        daemon_restarted: should_restart_daemon,
    })
}

pub fn prepare_agy_terminal_spec(
    state_paths: &StatePaths,
    explicit_agy_bin: Option<PathBuf>,
    argv: &[OsString],
    proxy_override: Option<&str>,
    hold_open_on_exit: bool,
) -> Result<TerminalLaunchSpec, CliError> {
    let settings = state_paths.read_settings()?;
    let env_proxy = std::env::var("AGY_PROXY").ok();
    let effective_proxy =
        resolve_effective_agy_proxy(proxy_override.or(env_proxy.as_deref()), &settings)?;
    let agy_bin = resolve_effective_agy_bin(explicit_agy_bin, &settings)
        .ok_or(CliError::AgyBinaryNotFound)?;

    let mut env_remove: Vec<String> = CLIENT_ENV_VARS_TO_STRIP
        .iter()
        .map(|&k| k.to_owned())
        .collect();
    let merged_no_proxy = merge_loopback_no_proxy(
        std::env::var("NO_PROXY")
            .ok()
            .or_else(|| std::env::var("no_proxy").ok())
            .as_deref(),
    );
    let mut env_set = vec![
        ("NO_PROXY".to_owned(), merged_no_proxy.clone()),
        ("no_proxy".to_owned(), merged_no_proxy),
    ];
    if let Some(px) = &effective_proxy {
        for &k in OUTBOUND_PROXY_ENV_KEYS {
            env_set.push((k.to_owned(), px.as_str().to_owned()));
        }
    } else {
        for &k in OUTBOUND_PROXY_ENV_KEYS {
            env_remove.push(k.to_owned());
        }
    }

    Ok(TerminalLaunchSpec {
        title: "Liftgate · Antigravity CLI".to_owned(),
        program: agy_bin,
        args: argv.to_vec(),
        cwd: std::env::current_dir().ok(),
        env_set,
        env_remove,
        cleanup_files: Vec::new(),
        hold_open_on_exit,
    })
}

pub async fn launch_agy_login_in_terminal(
    state_paths: &StatePaths,
    explicit_agy_bin: Option<PathBuf>,
    args: &AgyLoginArgs,
) -> Result<PathBuf, CliError> {
    let settings = state_paths.read_settings()?;
    let agy_bin = match resolve_effective_agy_bin(explicit_agy_bin, &settings) {
        Some(found) => found,
        None if args.install => {
            execute_agy_install(
                state_paths,
                &AgyInstallArgs {
                    proxy: args.proxy.clone(),
                    ..AgyInstallArgs::default()
                },
            )
            .await?
            .binary_path
        }
        None => return Err(CliError::AgyBinaryNotFound),
    };

    let spec = prepare_agy_terminal_spec(
        state_paths,
        Some(agy_bin.clone()),
        &[OsString::from("acp"), OsString::from("--login")],
        args.proxy.as_deref(),
        true,
    )?;
    spawn_in_system_terminal(state_paths, &spec)?;
    Ok(agy_bin)
}

#[derive(Clone, Debug, Serialize)]
pub struct AgyCapturedOutput {
    pub exit_ok: bool,
    pub stdout: String,
    pub stderr: String,
    pub binary_path: PathBuf,
}

pub async fn run_agy_captured(
    state_paths: &StatePaths,
    explicit_agy_bin: Option<PathBuf>,
    argv: &[OsString],
    max_wait: Duration,
) -> Result<AgyCapturedOutput, CliError> {
    let settings = state_paths.read_settings()?;
    let env_proxy = std::env::var("AGY_PROXY").ok();
    let effective_proxy = resolve_effective_agy_proxy(env_proxy.as_deref(), &settings)?;
    let agy_bin = resolve_effective_agy_bin(explicit_agy_bin, &settings)
        .ok_or(CliError::AgyBinaryNotFound)?;

    let mut cmd = Command::new(&agy_bin);
    cmd.args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Ok(cwd) = std::env::current_dir() {
        cmd.current_dir(cwd);
    }
    configure_acp_child_env(&mut cmd, effective_proxy.as_ref());
    suppress_windows_console(&mut cmd);

    match run_command_bounded(&mut cmd, max_wait).await {
        Ok((exit_ok, stdout, stderr)) => Ok(AgyCapturedOutput {
            exit_ok,
            stdout,
            stderr,
            binary_path: agy_bin,
        }),
        Err(err) => Ok(AgyCapturedOutput {
            exit_ok: false,
            stdout: String::new(),
            stderr: format_error_chain(&err),
            binary_path: agy_bin,
        }),
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AgyAutoUpdateOutcome {
    pub updated: bool,
    pub agy_auto_update: bool,
    pub daemon_restarted: bool,
    pub settings_file: PathBuf,
}

pub async fn execute_agy_auto_update(
    state_paths: &StatePaths,
    args: &AgyAutoUpdateArgs,
) -> Result<AgyAutoUpdateOutcome, CliError> {
    let mut settings = state_paths.read_settings()?;
    let previous = settings.agy_auto_update;
    let mutated = match args.mode {
        AutoUpdateToggle::Status => false,
        AutoUpdateToggle::On => {
            settings.agy_auto_update = true;
            true
        }
        AutoUpdateToggle::Off => {
            settings.agy_auto_update = false;
            true
        }
    };

    if mutated {
        state_paths.write_settings(&settings)?;
    }

    let mut daemon_restarted = false;
    if mutated
        && !args.no_restart
        && previous != settings.agy_auto_update
        && let Some(d) = state_paths.read_daemon()?
        && daemon::is_pid_alive(d.pid)
    {
        let _ = daemon::restart_daemon(state_paths, RestartArgs::default()).await?;
        daemon_restarted = true;
    }

    Ok(AgyAutoUpdateOutcome {
        updated: mutated,
        agy_auto_update: settings.agy_auto_update,
        daemon_restarted,
        settings_file: state_paths.settings_file(),
    })
}

pub async fn run_agy_passthrough(
    state_paths: &StatePaths,
    explicit_agy_bin: Option<PathBuf>,
    argv: &[OsString],
) -> Result<i32, CliError> {
    let settings = state_paths.read_settings()?;
    let env_proxy = std::env::var("AGY_PROXY").ok();
    let effective_proxy = resolve_effective_agy_proxy(env_proxy.as_deref(), &settings)?;
    let agy_bin = resolve_effective_agy_bin(explicit_agy_bin, &settings)
        .ok_or(CliError::AgyBinaryNotFound)?;
    spawn_interactive_agy(&agy_bin, argv, effective_proxy.as_ref()).await
}

pub async fn spawn_interactive_agy(
    agy_bin: &Path,
    argv: &[OsString],
    proxy: Option<&ProxyUrl>,
) -> Result<i32, CliError> {
    let mut cmd = Command::new(agy_bin);
    cmd.args(argv)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Ok(cwd) = std::env::current_dir() {
        cmd.current_dir(cwd);
    }
    configure_acp_child_env(&mut cmd, proxy);

    let mut child = cmd.spawn().map_err(|source| CliError::HarnessSpawn {
        tool: "agy",
        path: agy_bin.to_path_buf(),
        source,
    })?;

    let mut sig_tasks = JoinSet::new();
    sig_tasks.spawn(async { while tokio::signal::ctrl_c().await.is_ok() {} });

    let status = child
        .wait()
        .await
        .map_err(|source| CliError::HarnessSpawn {
            tool: "agy",
            path: agy_bin.to_path_buf(),
            source,
        })?;
    Ok(status.code().unwrap_or(1))
}

// ============================================================================
// Gateway Idle `agy` Auto-Updater Background Task
// ============================================================================

#[derive(Clone, Debug)]
pub(crate) struct IdleAutoUpdateOptions {
    pub initial_delay: Duration,
    pub check_interval: Duration,
    pub defer_retry: Duration,
    pub empty_pool_cooldown_ms: u64,
    pub warm_pool_cooldown_ms: u64,
    pub manifest_base_url: Option<String>,
}

impl IdleAutoUpdateOptions {
    fn from_env() -> Self {
        Self {
            initial_delay: env_duration_secs(
                "LIFTGATE_AGY_UPDATE_INITIAL_DELAY_SECS",
                DEFAULT_AUTO_UPDATE_INITIAL_DELAY_SECS,
            ),
            check_interval: env_duration_secs(
                "LIFTGATE_AGY_UPDATE_INTERVAL_SECS",
                DEFAULT_AUTO_UPDATE_CHECK_INTERVAL_SECS,
            ),
            defer_retry: env_duration_secs(
                "LIFTGATE_AGY_UPDATE_DEFER_SECS",
                DEFAULT_AUTO_UPDATE_DEFER_RETRY_SECS,
            ),
            empty_pool_cooldown_ms: env_u64_ms(
                "LIFTGATE_AGY_IDLE_EMPTY_COOLDOWN_MS",
                DEFAULT_IDLE_QUIET_EMPTY_POOL_MS,
            ),
            warm_pool_cooldown_ms: env_u64_ms(
                "LIFTGATE_AGY_IDLE_WARM_COOLDOWN_MS",
                DEFAULT_IDLE_QUIET_WARM_POOL_MS,
            ),
            manifest_base_url: std::env::var("LIFTGATE_AGY_MANIFEST_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty()),
        }
    }
}

/// Periodically checks the official Antigravity CLI manifest in the background and hot-swaps
/// `agy` when a newer build is published AND the gateway is completely idle.
pub async fn run_idle_agy_auto_updater(
    state_paths: StatePaths,
    live_client: Arc<LiveAcpClient>,
    pool: Arc<SessionPool>,
    telemetry: Arc<TelemetryHub>,
    stop_rx: watch::Receiver<bool>,
) {
    run_idle_agy_auto_updater_with_options(
        state_paths,
        live_client,
        pool,
        telemetry,
        stop_rx,
        IdleAutoUpdateOptions::from_env(),
    )
    .await;
}

pub(crate) async fn run_idle_agy_auto_updater_with_options(
    state_paths: StatePaths,
    live_client: Arc<LiveAcpClient>,
    pool: Arc<SessionPool>,
    telemetry: Arc<TelemetryHub>,
    mut stop_rx: watch::Receiver<bool>,
    opts: IdleAutoUpdateOptions,
) {
    let Ok(platform_slug) = detect_agy_platform_slug() else {
        live_client.record_auto_update_check(
            None,
            Some("auto-update skipped: unsupported OS/arch".to_owned()),
        );
        return;
    };

    if sleep_or_stop(&mut stop_rx, opts.initial_delay).await {
        return;
    }

    let mut pending_manifest: Option<AgyManifest> = None;

    loop {
        if *stop_rx.borrow() {
            return;
        }

        let proxy = live_client.agy_proxy().cloned();
        let bin_path = live_client.binary_path().to_path_buf();

        let manifest_opt = if let Some(cached) = pending_manifest.clone() {
            Some(cached)
        } else {
            match fetch_agy_manifest(
                opts.manifest_base_url.as_deref(),
                platform_slug,
                proxy.as_ref(),
            )
            .await
            {
                Ok(m) => Some(m),
                Err(err) => {
                    tracing::debug!(error = %err, "periodic `agy` manifest check failed; will retry later");
                    live_client.record_auto_update_check(None, Some(format!("check error: {err}")));
                    telemetry.notify_change();
                    None
                }
            }
        };

        let mut next_sleep = opts.check_interval;

        if let Some(manifest) = manifest_opt {
            let current_ver = match live_client.current_agent_version() {
                Some(v) if !v.trim().is_empty() => Some(v),
                _ => probe_agy_version(&bin_path, proxy.as_ref()).await,
            };

            let newer = match &current_ver {
                Some(cur) => match (SemVer::parse(cur), SemVer::parse(&manifest.version)) {
                    (Some(c), Some(l)) => l > c,
                    _ => *cur != manifest.version,
                },
                None => false,
            };

            if !newer {
                pending_manifest = None;
                live_client.record_auto_update_check(
                    Some(manifest.version),
                    Some("up to date".to_owned()),
                );
                telemetry.notify_change();
            } else if !is_gateway_idle_for_update(&live_client, &pool, &telemetry, &opts) {
                tracing::info!(
                    current = ?current_ver,
                    target = %manifest.version,
                    "newer `agy` version detected; deferring auto-update until gateway is idle"
                );
                live_client.record_auto_update_check(
                    Some(manifest.version.clone()),
                    Some(format!(
                        "v{} ready; waiting for gateway idle window",
                        manifest.version
                    )),
                );
                telemetry.notify_change();
                pending_manifest = Some(manifest);
                next_sleep = opts.defer_retry;
            } else {
                let prev_str = current_ver.unwrap_or_else(|| "unknown".to_owned());
                match apply_idle_agy_hot_update(
                    &state_paths,
                    &live_client,
                    &pool,
                    &telemetry,
                    &prev_str,
                    &manifest,
                    &opts,
                )
                .await
                {
                    Ok(true) => {
                        tracing::info!(
                            from = %prev_str,
                            to = %manifest.version,
                            "automatically updated `agy` CLI while gateway was idle"
                        );
                        telemetry.notify_change();
                        pending_manifest = None;
                        if let Some(parent) = bin_path.parent() {
                            sweep_agy_backups_in_dir(parent);
                        }
                    }
                    Ok(false) => {
                        // Activity arrived mid-staging; retry on next idle check.
                        pending_manifest = Some(manifest);
                        next_sleep = opts.defer_retry;
                    }
                    Err(CliError::UpgradePermissionDenied { path, .. }) => {
                        tracing::warn!(
                            path = %path.display(),
                            target = %manifest.version,
                            "cannot auto-update `agy` in read-only directory; run `lg agy update` with appropriate permissions"
                        );
                        live_client.record_auto_update_check(
                            Some(manifest.version),
                            Some(format!(
                                "permission denied writing {}; run `lg agy update`",
                                path.display()
                            )),
                        );
                        telemetry.notify_change();
                        pending_manifest = None;
                    }
                    Err(err) => {
                        let detail = err.display_chain();
                        tracing::warn!(error = %detail, "idle `agy` auto-update attempt failed");
                        live_client.record_auto_update_check(
                            Some(manifest.version),
                            Some(format!("update failed: {detail}")),
                        );
                        telemetry.notify_change();
                        pending_manifest = None;
                    }
                }
            }
        }

        if sleep_or_stop(&mut stop_rx, next_sleep).await {
            return;
        }
    }
}

async fn apply_idle_agy_hot_update(
    state_paths: &StatePaths,
    live_client: &Arc<LiveAcpClient>,
    pool: &Arc<SessionPool>,
    telemetry: &Arc<TelemetryHub>,
    previous_version: &str,
    manifest: &AgyManifest,
    opts: &IdleAutoUpdateOptions,
) -> Result<bool, CliError> {
    let stage = StageDirGuard::create(state_paths, "agy-auto-stage")?;
    let proxy = live_client.agy_proxy().cloned();
    let staged_bin =
        download_and_verify_agy_staged(manifest, &stage.path, proxy.as_ref(), None).await?;

    let client_ref = Arc::clone(live_client);
    let pool_ref = Arc::clone(pool);
    let telemetry_ref = Arc::clone(telemetry);
    let opts_ref = opts.clone();
    let auto_update_record = Some((
        previous_version.to_owned(),
        manifest.version.clone(),
        Some(format!(
            "auto-updated v{previous_version} -> v{}",
            manifest.version
        )),
    ));
    let swapped = live_client
        .hot_swap_binary_when_idle(
            pool,
            auto_update_record,
            || is_gateway_idle_for_update(&client_ref, &pool_ref, &telemetry_ref, &opts_ref),
            move |target_bin| install_staged_agy_binary(&staged_bin, target_bin),
        )
        .await
        .map_err(|err| match err {
            liftgate_server::error::AcpError::SpawnProcess { path, source }
                if source.kind() == io::ErrorKind::PermissionDenied =>
            {
                CliError::UpgradePermissionDenied { path, source }
            }
            other => CliError::AcpInit { source: other },
        })?;

    Ok(swapped.is_some())
}

fn is_gateway_idle_for_update(
    live_client: &LiveAcpClient,
    pool: &SessionPool,
    telemetry: &TelemetryHub,
    opts: &IdleAutoUpdateOptions,
) -> bool {
    let (metrics, _, _, _) = telemetry.snapshot_tasks();
    if metrics.active_requests > 0 {
        return false;
    }
    if live_client.active_activity_counts() != (0, 0) {
        return false;
    }
    let (active_leases, idle_sessions) = pool.idle_and_active_counts();
    if active_leases > 0 {
        return false;
    }
    let quiet_required_ms = if idle_sessions == 0 {
        opts.empty_pool_cooldown_ms
    } else {
        opts.warm_pool_cooldown_ms
    };
    now_unix_ms().saturating_sub(pool.last_activity_unix_ms()) >= quiet_required_ms
}

fn env_duration_secs(key: &str, default_secs: u64) -> Duration {
    let secs = std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default_secs);
    Duration::from_secs(secs.max(1))
}

fn env_u64_ms(key: &str, default_ms: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default_ms)
}

async fn sleep_or_stop(stop_rx: &mut watch::Receiver<bool>, duration: Duration) -> bool {
    if *stop_rx.borrow() {
        return true;
    }
    tokio::select! {
        biased;
        _ = stop_rx.wait_for(|&stop| stop) => return true,
        () = tokio::time::sleep(duration) => {}
    }
    *stop_rx.borrow()
}

/// Quick summary of local `agy` presence and version for `lg status` / `lg` cockpit.
#[derive(Clone, Debug)]
pub struct LocalAgySummary {
    pub binary_path: Option<PathBuf>,
    pub version: Option<String>,
    pub proxy: Option<ProxyUrl>,
    pub auto_update_enabled: bool,
}

pub async fn quick_local_agy_summary(
    state_paths: &StatePaths,
    explicit_agy_bin: Option<PathBuf>,
) -> LocalAgySummary {
    let settings = state_paths.read_settings().unwrap_or_default();
    let env_proxy = std::env::var("AGY_PROXY").ok();
    let proxy = resolve_effective_agy_proxy(env_proxy.as_deref(), &settings)
        .ok()
        .flatten();
    let binary_path = resolve_effective_agy_bin(explicit_agy_bin, &settings);
    let version = match &binary_path {
        Some(p) => probe_agy_version(p, proxy.as_ref()).await,
        None => None,
    };
    LocalAgySummary {
        binary_path,
        version,
        proxy,
        auto_update_enabled: settings.agy_auto_update,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use serde_json::json;
    use tar::{Builder, Header};
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpListener;

    #[test]
    fn platform_mapping_matches_official_install_scripts() {
        assert_eq!(
            detect_agy_platform_for("windows", "x86_64", false).unwrap(),
            "windows_amd64"
        );
        assert_eq!(
            detect_agy_platform_for("windows", "aarch64", false).unwrap(),
            "windows_arm64"
        );
        assert_eq!(
            detect_agy_platform_for("macos", "aarch64", false).unwrap(),
            "darwin_arm64"
        );
        assert_eq!(
            detect_agy_platform_for("linux", "x86_64", false).unwrap(),
            "linux_amd64"
        );
        assert_eq!(
            detect_agy_platform_for("linux", "x86_64", true).unwrap(),
            "linux_amd64_musl"
        );
        assert!(detect_agy_platform_for("freebsd", "x86_64", false).is_err());
    }

    #[test]
    fn parses_agy_models_and_auth_classification_accurately() {
        let sample = "Fetching available models...\ngemini-4-carbon-high\tGemini 4 Carbon (High)\nclaude-sonnet-5-5-high\tClaude Sonnet 5.5 (High)\n";
        assert_eq!(count_agy_models_output(sample), 2);

        assert!(matches!(
            classify_auth_probe_failure("", "Error: not authenticated; please run agy acp --login"),
            AgyAuthStatus::NotSignedIn { .. }
        ));
        assert!(matches!(
            classify_auth_probe_failure(
                "",
                "Post https://googleapis.com: proxyconnect tcp: dial tcp 127.0.0.1:7890: connection refused"
            ),
            AgyAuthStatus::NetworkError { .. }
        ));
    }

    #[test]
    fn tar_gz_extractor_unpacks_antigravity_entry_and_verifies_sha512() {
        let temp = std::env::temp_dir().join(format!("lg-agy-tar-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&temp).unwrap();
        let tar_gz_path = temp.join("cli_linux_x64.tar.gz");
        let payload = b"#!/bin/sh\necho 1.5.9\n";

        {
            let f = File::create(&tar_gz_path).unwrap();
            let enc = GzEncoder::new(f, Compression::default());
            let mut builder = Builder::new(enc);
            let mut header = Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "antigravity", &payload[..])
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }

        let hash = compute_file_sha512_hex(&tar_gz_path).unwrap();
        assert!(is_valid_sha512_hex(&hash));

        let out_bin = temp.join("agy");
        extract_agy_from_tar_gz(&tar_gz_path, &out_bin).unwrap();
        assert_eq!(fs::read(&out_bin).unwrap(), payload);

        // Exceeding the extracted binary limit must fail closed and delete partial output.
        let out_over = temp.join("agy-over");
        let err = extract_agy_from_tar_gz_with_limit(&tar_gz_path, &out_over, 4).unwrap_err();
        assert!(matches!(
            err,
            CliError::AgyArchive {
                source: ArchiveError::ExtractedBinaryTooLarge { .. },
                ..
            }
        ));
        assert!(!out_over.exists());

        let _ = fs::remove_dir_all(&temp);
    }

    #[tokio::test]
    async fn mock_manifest_install_verifies_sha512_and_rejects_tampered_payload() {
        let temp = std::env::temp_dir().join(format!("lg-agy-inst-test-{}", Uuid::new_v4()));
        let state_paths = StatePaths::resolve(Some(temp.join("state"))).unwrap();
        let install_dir = temp.join("bin");
        fs::create_dir_all(&install_dir).unwrap();

        // Reuse our own compiled test binary or a valid system executable (`curl` / `cmd`)?
        // Even simpler: copy `std::env::current_exe()`? No, test binary exits 0 on `--version`? No, clap/libtest accepts `--help`, wait:
        // Let's check what executable exits 0 on `--version` on every OS: `curl` (`curl.exe --version` on Windows & `curl --version` on Unix)!
        // Or build a tiny payload to test SHA-512 mismatch deterministically first!
        let fake_bytes = b"corrupted-or-fake-payload".to_vec();
        let wrong_sha512 = "a".repeat(128);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let slug = detect_agy_platform_slug().unwrap();
        let manifest_body = serde_json::to_vec(&json!({
            "version": "9.9.9",
            "url": format!("http://{addr}/payload.bin"),
            "sha512": wrong_sha512,
        }))
        .unwrap();
        let expected_manifest_path = format!("/manifests/{slug}.json");

        let srv = tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut req_buf = vec![0u8; 2048];
                let n = stream.read(&mut req_buf).await.unwrap_or(0);
                let req_str = String::from_utf8_lossy(&req_buf[..n]);
                let body = if req_str.contains(&expected_manifest_path) {
                    manifest_body.clone()
                } else {
                    fake_bytes.clone()
                };
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            }
        });

        let err = execute_agy_install(
            &state_paths,
            &AgyInstallArgs {
                check: false,
                force: true,
                dir: Some(install_dir.clone()),
                proxy: None,
                skip_path: true,
                manifest_base_url: Some(format!("http://{addr}")),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CliError::AgyChecksumMismatch { .. }));
        assert!(!install_dir.join(agy_binary_filename()).exists());

        srv.abort();
        let _ = fs::remove_dir_all(&temp);
    }

    fn compile_mock_agy_binary(out_path: &Path, version: &str, fail_on_acp: bool) {
        let src_path = out_path.with_extension("rs");
        let fail_lit = if fail_on_acp { "true" } else { "false" };
        let code = format!(
            r#"use std::io::{{BufRead, Write}};
fn extract_id(line: &str) -> Option<u64> {{
    let rest = line.split("\"id\":").nth(1)?.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}}
fn main() {{
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {{
        println!("{version}");
        return;
    }}
    if {fail_lit} {{
        eprintln!("mock broken agy binary");
        std::process::exit(1);
    }}
    let px = std::env::var("HTTPS_PROXY").unwrap_or_else(|_| "none".to_string());
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {{
        let Ok(line) = line else {{ break }};
        let Some(id) = extract_id(&line) else {{ continue }};
        if line.contains("\"method\":\"initialize\"") {{
            let _ = writeln!(stdout, "{{{{\"jsonrpc\":\"2.0\",\"id\":{{}},\"result\":{{{{\"protocolVersion\":1,\"agentInfo\":{{{{\"name\":\"MockAgy(px={{}})\",\"version\":\"{version}\"}}}}}}}}}}}}", id, px);
            let _ = stdout.flush();
        }} else if line.contains("\"method\":\"session/new\"") {{
            let _ = writeln!(stdout, "{{{{\"jsonrpc\":\"2.0\",\"id\":{{}},\"result\":{{{{\"sessionId\":\"sess-mock\",\"models\":{{{{\"availableModels\":[{{{{\"modelId\":\"gemini-4-carbon-high\",\"name\":\"Gemini 4 Carbon\"}}}}]}}}}}}}}}}}}", id);
            let _ = stdout.flush();
        }} else {{
            let _ = writeln!(stdout, "{{{{\"jsonrpc\":\"2.0\",\"id\":{{}},\"result\":{{{{}}}}}}}}", id);
            let _ = stdout.flush();
        }}
    }}
}}
"#
        );
        fs::write(&src_path, code).unwrap();
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let status = std::process::Command::new(rustc)
            .arg(&src_path)
            .arg("-O")
            .arg("-o")
            .arg(out_path)
            .status()
            .unwrap();
        assert!(status.success(), "rustc failed to compile mock agy binary");
        let _ = fs::remove_file(&src_path);
    }

    #[tokio::test]
    async fn proxy_injection_rollback_and_idle_auto_update_hot_swap_end_to_end() {
        use liftgate_server::acp_client::LiveAcpConfig;
        use liftgate_types::config::GatewayLimits;
        use liftgate_types::telemetry::TaskProtocol;

        let temp = std::env::temp_dir().join(format!("lg-agy-hotswap-{}", Uuid::new_v4()));
        let state_paths = StatePaths::resolve(Some(temp.join("state"))).unwrap();
        let bin_dir = temp.join("bin");
        let cwd_dir = temp.join("cwd");
        let build_dir = temp.join("build");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::create_dir_all(&cwd_dir).unwrap();
        fs::create_dir_all(&build_dir).unwrap();

        let active_bin = bin_dir.join(agy_binary_filename());
        let v2_raw_bin = build_dir.join(format!("v2-{}", agy_binary_filename()));
        let broken_bin = build_dir.join(format!("broken-{}", agy_binary_filename()));
        compile_mock_agy_binary(&active_bin, "1.5.8", false);
        compile_mock_agy_binary(&v2_raw_bin, "1.5.9", false);
        compile_mock_agy_binary(&broken_bin, "1.6.0", true);

        let limits = GatewayLimits::default();

        // 1. Default policy (`agy_proxy: None`) strips all proxy env vars.
        let direct_client = LiveAcpClient::spawn_and_init(LiveAcpConfig {
            binary_path: active_bin.clone(),
            extra_args: Vec::new(),
            session_cwd: cwd_dir.clone(),
            spill_dir: None,
            limits,
            agy_proxy: None,
            auto_update_enabled: true,
        })
        .await
        .unwrap();
        let direct_tel = direct_client.process_telemetry("gemini-4-carbon-high", "auto");
        assert_eq!(direct_tel.agent_name.as_deref(), Some("MockAgy(px=none)"));
        assert_eq!(direct_tel.agent_version.as_deref(), Some("1.5.8"));
        assert_eq!(direct_tel.agy_proxy, None);
        let _ = direct_client.close().await;

        // 2. Forced proxy (`7890` -> `http://127.0.0.1:7890`) is injected into `agy acp`.
        let forced_proxy = ProxyUrl::try_new("7890").unwrap();
        let live_client = Arc::new(
            LiveAcpClient::spawn_and_init(LiveAcpConfig {
                binary_path: active_bin.clone(),
                extra_args: Vec::new(),
                session_cwd: cwd_dir.clone(),
                spill_dir: None,
                limits,
                agy_proxy: Some(forced_proxy),
                auto_update_enabled: true,
            })
            .await
            .unwrap(),
        );
        let proxied_tel = live_client.process_telemetry("gemini-4-carbon-high", "auto");
        assert_eq!(
            proxied_tel.agent_name.as_deref(),
            Some("MockAgy(px=http://127.0.0.1:7890)")
        );
        assert_eq!(
            proxied_tel.agy_proxy.as_deref(),
            Some("http://127.0.0.1:7890")
        );

        // Clear proxy on a fresh client so local loopback manifest download uses direct HTTP.
        if let Ok(owned) = Arc::try_unwrap(live_client) {
            let _ = owned.close().await;
        }
        let live_client = Arc::new(
            LiveAcpClient::spawn_and_init(LiveAcpConfig {
                binary_path: active_bin.clone(),
                extra_args: Vec::new(),
                session_cwd: cwd_dir,
                spill_dir: None,
                limits,
                agy_proxy: None,
                auto_update_enabled: true,
            })
            .await
            .unwrap(),
        );
        let pool = Arc::new(SessionPool::new(limits));
        let telemetry = Arc::new(TelemetryHub::new());

        // 3. Broken candidate binary fails ACP handshake and rolls back cleanly to v1.5.8!
        let broken_stage = broken_bin.clone();
        let swap_err = live_client
            .hot_swap_binary_when_idle(
                &pool,
                None,
                || true,
                move |target| install_staged_agy_binary(&broken_stage, target),
            )
            .await;
        assert!(swap_err.is_err());
        let after_rollback = live_client.process_telemetry("gemini-4-carbon-high", "auto");
        assert!(after_rollback.connected);
        assert_eq!(after_rollback.agent_version.as_deref(), Some("1.5.8"));

        // 4. Serve v1.5.9 artifact (raw `.exe` on Windows, `.tar.gz` with `antigravity` on Unix).
        let (artifact_name, artifact_bytes) = if cfg!(windows) {
            ("agy.exe", fs::read(&v2_raw_bin).unwrap())
        } else {
            let tgz_path = build_dir.join("cli.tar.gz");
            let v2_bytes = fs::read(&v2_raw_bin).unwrap();
            {
                let f = File::create(&tgz_path).unwrap();
                let enc = GzEncoder::new(f, Compression::default());
                let mut builder = Builder::new(enc);
                let mut header = Header::new_gnu();
                header.set_size(v2_bytes.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder
                    .append_data(&mut header, "antigravity", v2_bytes.as_slice())
                    .unwrap();
                builder.into_inner().unwrap().finish().unwrap();
            }
            ("cli.tar.gz", fs::read(&tgz_path).unwrap())
        };
        let artifact_for_hash = build_dir.join(artifact_name);
        fs::write(&artifact_for_hash, &artifact_bytes).unwrap();
        let sha512 = compute_file_sha512_hex(&artifact_for_hash).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let slug = detect_agy_platform_slug().unwrap();
        let manifest_path = format!("/manifests/{slug}.json");
        let manifest_json = serde_json::to_vec(&json!({
            "version": "1.5.9",
            "url": format!("http://{addr}/{artifact_name}"),
            "sha512": sha512,
        }))
        .unwrap();

        let srv = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut req_buf = vec![0u8; 2048];
                let n = stream.read(&mut req_buf).await.unwrap_or(0);
                let req_str = String::from_utf8_lossy(&req_buf[..n]);
                let body = if req_str.contains(&manifest_path) {
                    manifest_json.clone()
                } else {
                    artifact_bytes.clone()
                };
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(hdr.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            }
        });

        // Hold an active in-flight turn first to prove auto-update defers while busy.
        let active_turn = telemetry.begin_task(
            "task-busy-1".to_owned(),
            TaskProtocol::OpenaiResponses,
            true,
            "gemini-4-carbon-high".to_owned(),
            None,
            &[],
        );

        let (stop_tx, stop_rx) = watch::channel(false);
        let updater_client = Arc::clone(&live_client);
        let updater_pool = Arc::clone(&pool);
        let updater_tel = Arc::clone(&telemetry);
        let updater_paths = state_paths.clone();
        let updater_handle = tokio::spawn(async move {
            run_idle_agy_auto_updater_with_options(
                updater_paths,
                updater_client,
                updater_pool,
                updater_tel,
                stop_rx,
                IdleAutoUpdateOptions {
                    initial_delay: Duration::from_millis(20),
                    check_interval: Duration::from_secs(60),
                    defer_retry: Duration::from_millis(60),
                    empty_pool_cooldown_ms: 0,
                    warm_pool_cooldown_ms: 0,
                    manifest_base_url: Some(format!("http://{addr}")),
                },
            )
            .await;
        });

        // Wait until the updater detects 1.5.9 and defers because `active_turn` is in flight.
        for _ in 0..200 {
            let snap = live_client.process_telemetry("gemini-4-carbon-high", "auto");
            if snap.latest_known_version.as_deref() == Some("1.5.9") {
                assert_eq!(snap.agent_version.as_deref(), Some("1.5.8"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(
            live_client.current_agent_version().as_deref(),
            Some("1.5.8")
        );

        // Complete the turn so the gateway transitions to idle; updater should hot-swap to 1.5.9.
        active_turn.complete(
            "end_turn".to_owned(),
            liftgate_types::acp::AcpUsage::default(),
        );

        for _ in 0..250 {
            if live_client.current_agent_version().as_deref() == Some("1.5.9") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }

        let updated_tel = live_client.process_telemetry("gemini-4-carbon-high", "auto");
        assert!(updated_tel.connected);
        assert_eq!(updated_tel.agent_version.as_deref(), Some("1.5.9"));
        assert_eq!(updated_tel.last_auto_updated_from.as_deref(), Some("1.5.8"));

        let _ = stop_tx.send_replace(true);
        let _ = updater_handle.await;
        srv.abort();
        if let Ok(owned) = Arc::try_unwrap(live_client) {
            let _ = owned.close().await;
        }
        let _ = fs::remove_dir_all(&temp);
    }
}
