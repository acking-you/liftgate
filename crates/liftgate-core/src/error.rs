use std::net::SocketAddr;
use std::path::PathBuf;

use liftgate_server::{error::AcpError, platform::SessionCwdError};
use liftgate_types::error::{ValidationError, format_error_chain};
use thiserror::Error;

/// Structured validation failure for self-upgrade CLI arguments.
#[derive(Debug, Error)]
pub enum UpgradeInputError {
    #[error("refusing non-HTTPS download URL `{url}`")]
    NonHttpsUrl { url: String },

    #[error("repository `{repo}` must be in `owner/name` format")]
    InvalidRepoFormat { repo: String },

    #[error("invalid GitHub repository slug `{repo}`")]
    InvalidRepoSlug { repo: String },

    #[error("invalid release version or tag `{tag}`")]
    InvalidReleaseTag { tag: String },

    #[error("invalid target triple `{target}`")]
    InvalidTargetTriple { target: String },
}

/// Structured network or external download-tool failure.
#[derive(Debug, Error)]
pub enum TransferError {
    #[error("upstream returned HTTP {status}")]
    HttpStatus { status: u16 },

    #[error("transfer timed out")]
    TimedOut,

    #[error("failed to spawn `{tool}`")]
    SpawnTool {
        tool: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("`{tool}` stdout pipe was unavailable")]
    MissingStdoutPipe { tool: &'static str },

    #[error("I/O error communicating with `{tool}`")]
    ToolIo {
        tool: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("`{tool}` exited with {status}")]
    ToolExitStatus {
        tool: &'static str,
        status: std::process::ExitStatus,
    },

    #[error("`{tool}` failed: {stderr}")]
    ToolStderr { tool: &'static str, stderr: String },

    #[error("no HTTPS download utility (`curl`, `wget`, or `fetch`) was found on PATH")]
    NoHttpsDownloadTool,

    #[error("payload exceeded {limit_mib} MiB safety limit")]
    PayloadTooLarge { limit_mib: u64 },

    #[error("invalid JSON response payload")]
    InvalidJson {
        #[from]
        source: serde_json::Error,
    },

    #[error("could not resolve latest release tag from GitHub redirect")]
    MissingReleaseRedirectTag,

    #[error("loopback HTTP {op} failed")]
    LoopbackIo {
        op: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("malformed HTTP response header")]
    MalformedHttpHeader,

    #[error("HTTP response header exceeded safety limit")]
    HttpHeaderTooLarge,
}

/// Structured archive, manifest, extraction, or smoke-check failure.
#[derive(Debug, Error)]
pub enum ArchiveError {
    #[error("`SHA256SUMS` for `{tag}` did not contain an entry for `{asset}`")]
    MissingChecksumEntry { tag: String, asset: String },

    #[error("malformed release manifest JSON")]
    MalformedManifestJson {
        #[from]
        source: serde_json::Error,
    },

    #[error("manifest is missing valid version, URL, or 128-character SHA-512 hex digest")]
    InvalidManifestFields,

    #[error("corrupt ZIP archive")]
    CorruptZip {
        #[source]
        source: zip::result::ZipError,
    },

    #[error("failed reading ZIP entry #{index}")]
    ZipEntry {
        index: usize,
        #[source]
        source: zip::result::ZipError,
    },

    #[error("corrupt tar.gz archive")]
    CorruptTarGz {
        #[source]
        source: std::io::Error,
    },

    #[error("corrupt tar entry")]
    CorruptTarEntry {
        #[source]
        source: std::io::Error,
    },

    #[error("invalid tar entry path")]
    InvalidTarEntryPath {
        #[source]
        source: std::io::Error,
    },

    #[error("decompression failed")]
    Decompress {
        #[source]
        source: std::io::Error,
    },

    #[error("extracted binary exceeded {limit_mib} MiB safety limit")]
    ExtractedBinaryTooLarge { limit_mib: u64 },

    #[error("archive did not contain expected executable `{expected}`")]
    MissingExecutable { expected: String },

    #[error("release archive for this target does not bundle `liftgate-desktop`")]
    MissingDesktopBundle,

    #[error("staged binary `--version` smoke check timed out")]
    SmokeCheckTimeout,

    #[error("staged binary failed to execute on this host")]
    SmokeCheckExec {
        #[source]
        source: std::io::Error,
    },

    #[error("staged binary `--version` exited with {status}: {stderr}")]
    SmokeCheckFailed {
        status: std::process::ExitStatus,
        stderr: String,
    },

    #[error("downloaded `agy` binary failed `--version` smoke check")]
    AgySmokeCheckFailed,
}

/// Structured local HTTP/SSE transport failure when talking to a Liftgate daemon.
#[derive(Debug, Error)]
pub enum GatewayTransportError {
    #[error("TCP socket I/O error")]
    Io(#[from] std::io::Error),

    #[error("gateway is not running")]
    NotRunning,

    #[error("request timed out")]
    TimedOut,

    #[error("incomplete HTTP response headers")]
    IncompleteHeaders,

    #[error("HTTP response headers are not valid UTF-8")]
    NonUtf8Headers,

    #[error("missing HTTP status line")]
    MissingStatusLine,

    #[error("invalid HTTP status code")]
    InvalidStatusCode,

    #[error("invalid HTTP chunk header")]
    InvalidChunkHeader,

    #[error("invalid HTTP chunk length")]
    InvalidChunkLength,

    #[error("truncated HTTP chunked response body")]
    TruncatedChunk,

    #[error("SSE stream buffer exceeded {max_bytes} bytes")]
    SseBufferExceeded { max_bytes: usize },
}

/// Structured failure when terminating a managed background daemon.
#[derive(Debug, Error)]
pub enum DaemonStopError {
    #[error("OS process termination failed")]
    Os(#[from] std::io::Error),

    #[error("process remained alive after termination signal")]
    StillAliveAfterSignal,

    #[error("PID {pid} exceeds platform `pid_t` range")]
    PidOutOfRange { pid: u32 },
}

/// Structured failure when launching a desktop GUI harness or helper process.
#[derive(Debug, Error)]
pub enum DesktopLaunchError {
    #[error("failed to locate current `liftgate` executable")]
    LocateCurrentExe(#[source] std::io::Error),

    #[error("failed to spawn `{}`", executable.display())]
    Spawn {
        executable: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to invoke `{}` for package `{package_family_name}`", executable.display())]
    MsixInvoke {
        executable: PathBuf,
        package_family_name: String,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "`Invoke-CommandInDesktopPackage` (`{package_family_name}!{app_id}`) exited with {status}"
    )]
    MsixExitStatus {
        package_family_name: String,
        app_id: String,
        status: std::process::ExitStatus,
    },

    #[error("{detail}")]
    MsixReported { detail: String },
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("invalid gateway configuration")]
    Validation {
        #[from]
        source: ValidationError,
    },

    #[error("Antigravity CLI (`agy`) is not installed or not found in PATH")]
    AgyBinaryNotFound,

    #[error(
        "official Antigravity CLI does not currently publish prebuilt binaries for `{os}/{arch}` (supported: 64-bit Windows, macOS, Linux, and Android)"
    )]
    AgyPlatformUnsupported { os: String, arch: String },

    #[error(
        "SHA-512 verification failed for Antigravity CLI payload `{asset}` (expected `{expected}`, got `{actual}`)"
    )]
    AgyChecksumMismatch {
        asset: String,
        expected: String,
        actual: String,
    },

    #[error("failed to query or download Antigravity CLI (`agy`) from `{url}`")]
    AgyNetwork {
        url: String,
        #[source]
        source: TransferError,
    },

    #[error("failed to verify or unpack Antigravity CLI payload `{asset}`")]
    AgyArchive {
        asset: String,
        #[source]
        source: ArchiveError,
    },

    #[error("Antigravity upstream connectivity test failed: {detail}")]
    AgyProxyProbeFailed { detail: String },

    #[error(
        "no system or environment proxy was detected; pass an explicit address such as `lg proxy 7890` or `lg proxy http://127.0.0.1:7890`"
    )]
    NoSystemProxyDetected,

    #[error("failed to prepare isolated ACP session directory")]
    PrepareCwd {
        #[from]
        source: SessionCwdError,
    },

    #[error("failed to initialize `agy acp` subprocess")]
    AcpInit {
        #[from]
        source: AcpError,
    },

    #[error("failed to bind HTTP listener on `{addr}`")]
    BindListener {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    #[error("HTTP server runtime error")]
    Serve {
        #[source]
        source: std::io::Error,
    },

    #[error(
        "could not determine user home directory; set `LIFTGATE_HOME` or `HOME` / `USERPROFILE`"
    )]
    NoHomeDirectory,

    #[error("filesystem operation failed at `{}`", path.display())]
    StateIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid JSON file at `{}`", path.display())]
    StateJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("configuration file `{}` exceeds {limit} byte safety limit", path.display())]
    ConfigFileTooLarge { path: PathBuf, limit: u64 },

    #[error("file `{}` is not valid UTF-8", path.display())]
    StateUtf8 {
        path: PathBuf,
        #[source]
        source: std::string::FromUtf8Error,
    },

    #[error(
        "another process is already listening on `{addr}` (not managed by this state directory); pass `--listen <addr>` or stop that process first"
    )]
    PortOccupiedUnmanaged { addr: SocketAddr },

    #[error("failed to spawn background `liftgate` daemon")]
    DaemonSpawn {
        #[source]
        source: std::io::Error,
    },

    #[error("background gateway exited unexpectedly (exit code: {code:?})\n{log_tail}")]
    DaemonExitedEarly { code: Option<i32>, log_tail: String },

    #[error("timed out waiting for gateway health check at `{url}` (inspect logs at `{}`)", log_path.display())]
    DaemonHealthTimeout { url: String, log_path: PathBuf },

    #[error("failed to stop background gateway (PID {pid})")]
    DaemonStopFailed {
        pid: u32,
        #[source]
        source: DaemonStopError,
    },

    #[error("gateway is not running or unreachable at `{url}`")]
    GatewayUnreachable {
        url: String,
        #[source]
        source: GatewayTransportError,
    },

    #[error("gateway request `{url}` failed with HTTP {status}: {body}")]
    GatewayHttpStatus {
        url: String,
        status: u16,
        body: String,
    },

    #[error("failed to decode JSON response from `{url}`")]
    GatewayBadJson {
        url: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("`{tool}` CLI executable was not found ({hint})")]
    HarnessNotFound {
        tool: &'static str,
        hint: &'static str,
    },

    #[error("failed to run `{tool}` at `{}`", path.display())]
    HarnessSpawn {
        tool: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("`{app}` desktop application was not found ({hint})")]
    DesktopAppNotFound {
        app: &'static str,
        hint: &'static str,
    },

    #[error(
        "`{app}` is already running (PID {pids}); close it first or pass `--restart-app` so Liftgate can inject session config cleanly"
    )]
    DesktopAppAlreadyRunning { app: &'static str, pids: String },

    #[error("failed to launch `{app}`")]
    DesktopAppLaunchFailed {
        app: &'static str,
        #[source]
        source: DesktopLaunchError,
    },

    #[error("failed to open browser for `{url}`")]
    BrowserOpen {
        url: String,
        #[source]
        source: std::io::Error,
    },

    #[error("`{feature}` requires an interactive terminal (TTY)")]
    NotInteractiveTerminal { feature: &'static str },

    #[error("interactive terminal UI error")]
    TuiIo {
        #[source]
        source: std::io::Error,
    },

    #[error("invalid alias name `{name}` (must be a plain executable stem such as `lg`)")]
    InvalidAliasName { name: String },

    #[error(
        "alias target `{}` already exists; pass `--force` to replace it or `--uninstall` to remove it",
        path.display()
    )]
    AliasTargetExists { path: PathBuf },

    #[error("invalid upgrade parameter")]
    UpgradeInvalidInput {
        #[from]
        source: UpgradeInputError,
    },

    #[error("failed to query or download release from `{url}`")]
    UpgradeNetwork {
        url: String,
        #[source]
        source: TransferError,
    },

    #[error("release `{tag}` has no prebuilt archive for target `{target}`")]
    UpgradeUnsupportedTarget { tag: String, target: String },

    #[error("SHA-256 verification failed for `{asset}` (expected `{expected}`, got `{actual}`)")]
    UpgradeChecksumMismatch {
        asset: String,
        expected: String,
        actual: String,
    },

    #[error("failed to verify or unpack release archive `{asset}`")]
    UpgradeArchive {
        asset: String,
        #[source]
        source: ArchiveError,
    },

    #[error("permission denied replacing `{}`", path.display())]
    UpgradePermissionDenied {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "in-place binary self-upgrade is not supported when running inside {bundle_kind} (`{}`); download the latest desktop package from `{release_url}` instead",
        exe_path.display()
    )]
    UpgradePackagedBundle {
        bundle_kind: &'static str,
        exe_path: PathBuf,
        release_url: String,
    },
}

impl CliError {
    /// Formats this error together with its full [`std::error::Error::source`] chain.
    #[must_use]
    pub fn display_chain(&self) -> String {
        format_error_chain(self)
    }
}

/// Returns an actionable recovery hint for `err` using `bin` as the CLI command name.
#[must_use]
pub fn recovery_hint_for_error(err: &CliError, bin: &str) -> Option<String> {
    match err {
        CliError::AgyBinaryNotFound => Some(format!(
            "Run `{bin} agy install` to download & install Antigravity CLI automatically, then `{bin} agy login` to sign in."
        )),
        CliError::NoSystemProxyDetected => Some(format!(
            "Set an explicit proxy with `{bin} proxy 7890` (or `{bin} proxy socks5://127.0.0.1:1080`), or clear with `{bin} proxy off`."
        )),
        CliError::AgyArchive { .. } => Some(format!(
            "Verify the downloaded archive or update Liftgate with `{bin} upgrade`, then re-run `{bin} agy update --force`."
        )),
        CliError::AgyNetwork { .. } | CliError::AgyProxyProbeFailed { .. } => Some(format!(
            "If your network requires a proxy to reach Google Cloud Storage, configure `{bin} proxy <port_or_url>` first and re-run `{bin} agy install`."
        )),
        CliError::GatewayUnreachable { .. } => Some(format!(
            "Start the background gateway with `{bin} up` (or `{bin} start`), or check `{bin} status`."
        )),
        CliError::PortOccupiedUnmanaged { .. } => Some(format!(
            "Run `{bin} status` to inspect the active listener, or bind another port with `--listen 127.0.0.1:8788`."
        )),
        CliError::DaemonExitedEarly { .. } | CliError::DaemonHealthTimeout { .. } => Some(format!(
            "Inspect `{bin} logs -n 80` and `{bin} doctor` for startup diagnostics."
        )),
        CliError::GatewayHttpStatus { status: 401, .. } => Some(format!(
            "Pass `--api-key <key>` or export `AGY_GATEWAY_API_KEY` matching the running gateway (`{bin} status`)."
        )),
        CliError::UpgradeNetwork { .. } => Some(format!(
            "Check network connectivity, pass `--proxy <url>` (or set `HTTPS_PROXY`), or set `GITHUB_TOKEN` if rate-limited (`{bin} upgrade --check`)."
        )),
        CliError::UpgradePermissionDenied { .. } => Some(
            "Re-run with elevated permissions (e.g. `sudo lg upgrade`) or install into a user-writable directory such as `~/.cargo/bin`."
                .to_owned(),
        ),
        CliError::UpgradePackagedBundle { release_url, .. } => Some(format!(
            "Open `{release_url}` to download the updated desktop package (`.AppImage`, `.dmg`, or `-setup.exe`); `--check` remains supported."
        )),
        CliError::UpgradeUnsupportedTarget { .. } => Some(
            "Install from source with `cargo install --git https://github.com/acking-you/liftgate liftgate` or specify `--target <triple>`."
                .to_owned(),
        ),
        CliError::DesktopAppAlreadyRunning { .. } => Some(
            "Close the running desktop app window first, or re-run with `--restart-app` to restart it automatically with Liftgate configuration."
                .to_owned(),
        ),
        CliError::DesktopAppNotFound { .. } => Some(format!(
            "Run `{bin} doctor` to inspect detected desktop installations, or specify an explicit path via `--codex-app-bin` / `--claude-desktop-bin`."
        )),
        _ => None,
    }
}
