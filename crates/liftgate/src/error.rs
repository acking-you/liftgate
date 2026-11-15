use std::net::SocketAddr;
use std::path::PathBuf;

use liftgate_server::error::AcpError;
use liftgate_types::error::ValidationError;
use thiserror::Error;

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

    #[error("failed to install or update Antigravity CLI (`agy`): {detail}")]
    AgyInstallFailed { detail: String },

    #[error(
        "no system or environment proxy was detected; pass an explicit address such as `lg proxy 7890` or `lg proxy http://127.0.0.1:7890`"
    )]
    NoSystemProxyDetected,

    #[error("failed to prepare isolated ACP session directory `{}`", path.display())]
    PrepareCwd {
        path: PathBuf,
        #[source]
        source: std::io::Error,
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
        source: std::io::Error,
    },

    #[error("gateway is not running or unreachable at `{url}`")]
    GatewayUnreachable {
        url: String,
        #[source]
        source: std::io::Error,
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

    #[error("invalid upgrade parameter: {reason}")]
    UpgradeInvalidInput { reason: String },

    #[error("failed to query or download release from `{url}`: {detail}")]
    UpgradeNetwork { url: String, detail: String },

    #[error("release `{tag}` has no prebuilt archive for target `{target}`")]
    UpgradeUnsupportedTarget { tag: String, target: String },

    #[error("SHA-256 verification failed for `{asset}` (expected `{expected}`, got `{actual}`)")]
    UpgradeChecksumMismatch {
        asset: String,
        expected: String,
        actual: String,
    },

    #[error("failed to verify or unpack release archive `{asset}`: {detail}")]
    UpgradeArchive { asset: String, detail: String },

    #[error("permission denied replacing `{}`", path.display())]
    UpgradePermissionDenied {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
