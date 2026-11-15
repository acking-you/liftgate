use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use liftgate_types::config::{
    DEFAULT_CHANNEL_CAPACITY, DEFAULT_MAX_BODY_BYTES, DEFAULT_MAX_IDLE_SESSIONS,
    DEFAULT_MAX_LINE_BYTES, DEFAULT_MAX_STORED_RESPONSES, DEFAULT_TURN_TIMEOUT_SECS,
};

/// Sentinel placeholder token injected into client harnesses when the local gateway runs
/// without API key authentication.
pub const PLACEHOLDER_API_KEY: &str = "local-agy";

const AFTER_HELP_TEXT: &str = "\
Quick Examples (both `lg` and `liftgate` work identically):
  lg                      Open interactive command center (when run in a terminal)
  lg agy                  Inspect `agy` version, login status, proxy & auto-update
  lg agy install          Download & install Antigravity CLI (`agy`) automatically
  lg agy login            Sign in to Antigravity interactively (with proxy applied)
  lg proxy 7890           Force `http://127.0.0.1:7890` into `agy` (`lg proxy off` to clear)
  lg cx                   Launch Codex CLI in current dir (0 side effects on exit)
  lg cc                   Launch Claude Code in current dir (0 side effects on exit)
  lg top                  Open real-time terminal observability dashboard (TUI)
  lg up --open            Start background gateway and open Web UI
  lg m -i                 Interactively pick & switch default Antigravity model
  lg st                   Inspect gateway health, session pool & token metrics
  lg upgrade              Check GitHub Releases & self-upgrade `lg` + `liftgate`";

/// Returns the short command name (`lg`, `liftgate`, or custom alias) used to invoke this process.
#[must_use]
pub fn invoked_bin_name() -> String {
    std::env::args_os()
        .next()
        .as_deref()
        .and_then(|raw| Path::new(raw).file_stem())
        .and_then(|stem| stem.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("lg")
        .to_owned()
}

/// Normalizes an optional API key, treating empty strings and [`PLACEHOLDER_API_KEY`] as
/// unauthenticated (`None`).
#[must_use]
pub fn normalize_optional_api_key(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case(PLACEHOLDER_API_KEY))
        .map(str::to_owned)
}

#[derive(Debug, Parser)]
#[command(
    name = "liftgate",
    version,
    styles = crate::ui::clap_styles(),
    about = "Antigravity ACP local API gateway, live TUI & web dashboard, and Claude Code / Codex CLI manager (short alias: `lg`)",
    after_help = AFTER_HELP_TEXT
)]
pub struct Cli {
    /// Directory storing global daemon state, logs, and ephemeral files (`~/.liftgate` by default).
    #[arg(long, global = true, env = "LIFTGATE_HOME")]
    pub state_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Foreground server options when `liftgate` is invoked without a subcommand.
    #[command(flatten)]
    pub serve: ServeArgs,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Launch the interactive real-time terminal observability dashboard (TUI).
    #[command(visible_aliases = ["tui", "dash", "monitor"])]
    Top(TopArgs),

    /// Start the gateway and embedded Web UI in the background (`~/.liftgate`).
    #[command(visible_alias = "up")]
    Start(StartArgs),

    /// Gracefully stop the managed background gateway service.
    #[command(visible_alias = "down")]
    Stop,

    /// Restart the managed background gateway service, preserving or overriding settings.
    #[command(visible_alias = "r")]
    Restart(RestartArgs),

    /// Show live gateway health, PID, session pool, token metrics, and discovered model count.
    #[command(visible_alias = "st", alias = "s")]
    Status(StatusArgs),

    /// Tail or follow (`-f`) the background gateway service log.
    #[command(visible_alias = "l")]
    Logs(LogsArgs),

    /// Open (or print) the live Web observability dashboard URL.
    #[command(visible_aliases = ["web", "dashboard", "open"])]
    Ui(UiArgs),

    /// List or interactively switch (`-i`) live models discovered from `agy acp`.
    #[command(visible_aliases = ["m", "ls"])]
    Models(ModelsArgs),

    /// Launch Claude Code in the current directory via Liftgate with zero project/global file side effects.
    #[command(visible_alias = "cc")]
    Claude(ClaudeArgs),

    /// Launch Codex CLI in the current directory via Liftgate with zero project/global file side effects.
    #[command(visible_alias = "cx", alias = "x")]
    Codex(CodexArgs),

    /// Generate, update, or uninstall project-local or global configuration files for Claude Code and/or Codex CLI.
    #[command(visible_aliases = ["setup", "cfg"])]
    Config(ConfigArgs),

    /// Diagnose `agy`, `claude`, `codex`, embedded Web UI, and live gateway readiness.
    #[command(visible_alias = "doc")]
    Doctor(DoctorArgs),

    /// Inspect, install, sign in, update, or run Antigravity CLI (`agy`) with Liftgate's proxy & auto-update policy.
    Agy(AgyArgs),

    /// View, set (`lg proxy 7890`), or clear (`lg proxy off`) the outbound proxy forced into `agy` (default: none).
    #[command(visible_alias = "px")]
    Proxy(ProxyArgs),

    /// Check GitHub Releases and automatically upgrade `lg` and `liftgate` with SHA-256 verification.
    #[command(visible_aliases = ["update", "self-update", "upg"])]
    Upgrade(UpgradeArgs),

    /// Install or remove a short CLI executable alias (`lg` by default) on your PATH.
    Alias(AliasArgs),

    /// Generate shell completion scripts for `lg` or `liftgate`.
    Completions(CompletionsArgs),

    /// Run the HTTP gateway and Web UI in the foreground.
    Serve(ServeArgs),
}

#[derive(Clone, Debug, Args)]
pub struct TopArgs {
    /// Target gateway socket address override.
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Optional API key when inspecting an authenticated gateway.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Telemetry polling interval in milliseconds (minimum 250ms).
    #[arg(long, default_value_t = 1000)]
    pub interval_ms: u64,

    /// Auto-start the background gateway if it is currently offline.
    #[arg(long)]
    pub auto_start: bool,
}

#[derive(Clone, Debug, Args)]
pub struct ServeArgs {
    /// Local socket address to bind the HTTP gateway and embedded Web UI on.
    #[arg(long, env = "AGY_GATEWAY_LISTEN", default_value = "127.0.0.1:8787")]
    pub listen: SocketAddr,

    /// Optional extra socket address dedicated to serving the Web observability UI.
    #[arg(long, env = "AGY_UI_LISTEN")]
    pub ui_listen: Option<SocketAddr>,

    /// Path to `agy` (`agy.exe` on Windows). Auto-detected from `PATH` and standard install locations if omitted.
    #[arg(long, env = "AGY_BIN")]
    pub agy_bin: Option<PathBuf>,

    /// Default ACP model identifier when caller requests an unknown/generic model name.
    #[arg(
        long,
        env = "AGY_DEFAULT_MODEL",
        default_value = "gemini-4-carbon-high"
    )]
    pub default_model: String,

    /// Default ACP session mode (`auto`, `accept-edits`, `default`, `plan`).
    #[arg(long, env = "AGY_ACP_MODE", default_value = "auto")]
    pub acp_mode: String,

    /// Working directory assigned to ACP sessions (defaults to an isolated empty directory under system temp).
    #[arg(long, env = "AGY_ACP_CWD")]
    pub acp_cwd: Option<PathBuf>,

    /// Optional local API key required via `Authorization: Bearer <key>` or `x-api-key: <key>`.
    #[arg(long, env = "AGY_GATEWAY_API_KEY")]
    pub api_key: Option<String>,

    /// Optional directory overriding the embedded Web UI static bundle.
    #[arg(long, env = "AGY_UI_DIR")]
    pub ui_dir: Option<PathBuf>,

    /// Outbound proxy URL forced into `agy` (`http://...`, `socks5://...`, port `7890`, or `off`).
    /// Defaults to `~/.liftgate/settings.json` (`none` / direct connection by default).
    #[arg(long, env = "AGY_PROXY")]
    pub agy_proxy: Option<String>,

    /// Override background idle auto-updating of `agy` CLI (`true` or `false`; enabled by default).
    #[arg(long, env = "AGY_AUTO_UPDATE")]
    pub agy_auto_update: Option<bool>,

    /// Disable background idle auto-updating of `agy` CLI for this server run.
    #[arg(long, conflicts_with = "agy_auto_update")]
    pub no_agy_auto_update: bool,

    /// Opt into experimental Flash hydration; upstream compaction still applies.
    #[arg(long, env = "AGY_LONG_CONTEXT", num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    pub long_context: Option<bool>,

    /// Maximum ACP stdout JSON-RPC line size in bytes.
    #[arg(long, default_value_t = DEFAULT_MAX_LINE_BYTES)]
    pub max_line_bytes: usize,

    /// Maximum HTTP request body size in bytes.
    #[arg(long, default_value_t = DEFAULT_MAX_BODY_BYTES)]
    pub max_body_bytes: usize,

    /// Internal bounded channel capacity per stream.
    #[arg(long, default_value_t = DEFAULT_CHANNEL_CAPACITY)]
    pub channel_capacity: usize,

    /// Maximum idle multi-turn ACP sessions retained in LRU cache.
    #[arg(long, default_value_t = DEFAULT_MAX_IDLE_SESSIONS)]
    pub max_idle_sessions: usize,

    /// Maximum completed OpenAI `ResponseObject` records retained in memory.
    #[arg(long, default_value_t = DEFAULT_MAX_STORED_RESPONSES)]
    pub max_stored_responses: usize,

    /// Per-turn overall timeout in seconds.
    #[arg(long, default_value_t = DEFAULT_TURN_TIMEOUT_SECS)]
    pub turn_timeout_secs: u64,
}

impl Default for ServeArgs {
    fn default() -> Self {
        let listen = std::env::var("AGY_GATEWAY_LISTEN")
            .ok()
            .and_then(|s| s.parse::<SocketAddr>().ok())
            .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 8787)));
        let ui_listen = std::env::var("AGY_UI_LISTEN")
            .ok()
            .and_then(|s| s.parse::<SocketAddr>().ok());
        let agy_bin = std::env::var_os("AGY_BIN")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let default_model = std::env::var("AGY_DEFAULT_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "gemini-4-carbon-high".to_owned());
        let acp_mode = std::env::var("AGY_ACP_MODE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "auto".to_owned());
        let acp_cwd = std::env::var_os("AGY_ACP_CWD")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let api_key =
            normalize_optional_api_key(std::env::var("AGY_GATEWAY_API_KEY").ok().as_deref());
        let ui_dir = std::env::var_os("AGY_UI_DIR")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let agy_proxy = std::env::var("AGY_PROXY")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        let agy_auto_update = std::env::var("AGY_AUTO_UPDATE").ok().and_then(|s| {
            match s.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "on" | "yes" => Some(true),
                "0" | "false" | "off" | "no" => Some(false),
                _ => None,
            }
        });

        Self {
            listen,
            ui_listen,
            agy_bin,
            default_model,
            acp_mode,
            acp_cwd,
            api_key,
            ui_dir,
            agy_proxy,
            agy_auto_update,
            no_agy_auto_update: false,
            long_context: std::env::var("AGY_LONG_CONTEXT")
                .ok()
                .and_then(|s| s.parse().ok()),
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            max_idle_sessions: DEFAULT_MAX_IDLE_SESSIONS,
            max_stored_responses: DEFAULT_MAX_STORED_RESPONSES,
            turn_timeout_secs: DEFAULT_TURN_TIMEOUT_SECS,
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct StartArgs {
    #[command(flatten)]
    pub serve: ServeArgs,

    /// Force a restart if a managed background gateway is already running.
    #[arg(long)]
    pub force: bool,

    /// Open the Web observability dashboard in the default browser once ready.
    #[arg(long)]
    pub open: bool,
}

#[derive(Clone, Debug, Default, Args)]
pub struct RestartArgs {
    /// Override bind socket address (`127.0.0.1:8787` by default).
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Override extra Web UI socket address.
    #[arg(long)]
    pub ui_listen: Option<SocketAddr>,

    /// Override default model ID.
    #[arg(long)]
    pub default_model: Option<String>,

    /// Override ACP mode (`auto`, `accept-edits`, `default`, `plan`).
    #[arg(long)]
    pub acp_mode: Option<String>,

    /// Override API key.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Override `agy` executable path.
    #[arg(long)]
    pub agy_bin: Option<PathBuf>,

    /// Override outbound proxy forced into `agy` (or `off` to disable).
    #[arg(long)]
    pub agy_proxy: Option<String>,

    /// Override background idle `agy` auto-update (`true` or `false`).
    #[arg(long)]
    pub agy_auto_update: Option<bool>,

    /// Disable background idle `agy` auto-update.
    #[arg(long, conflicts_with = "agy_auto_update")]
    pub no_agy_auto_update: bool,

    /// Opt into experimental Flash hydration; upstream compaction still applies.
    #[arg(long, env = "AGY_LONG_CONTEXT", num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    pub long_context: Option<bool>,

    /// Open the Web observability dashboard in the default browser after restarting.
    #[arg(long)]
    pub open: bool,
}

#[derive(Clone, Debug, Args)]
pub struct StatusArgs {
    /// Target gateway socket address override.
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Optional API key when inspecting an externally started authenticated gateway.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Emit structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct LogsArgs {
    /// Number of trailing log lines to print.
    #[arg(short = 'n', long, default_value_t = 80)]
    pub lines: usize,

    /// Follow newly written log output in real time until interrupted.
    #[arg(short = 'f', long)]
    pub follow: bool,

    /// Include recent `agy acp` stderr diagnostics from live telemetry.
    #[arg(long)]
    pub acp: bool,

    /// Truncate the managed gateway log file.
    #[arg(long)]
    pub clear: bool,

    /// Print the resolved gateway log file path and exit.
    #[arg(long)]
    pub path: bool,
}

#[derive(Clone, Debug, Args)]
pub struct UiArgs {
    /// Target gateway socket address override.
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Optional API key passed via `#api_key=...` URL fragment (automatically stripped by UI).
    #[arg(long)]
    pub api_key: Option<String>,

    /// Print the dashboard URL to stdout without opening a browser.
    #[arg(long)]
    pub print: bool,

    /// Do not auto-start the background gateway if it is currently offline.
    #[arg(long)]
    pub no_start: bool,
}

#[derive(Clone, Debug, Args)]
pub struct ModelsArgs {
    /// Target gateway socket address override.
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Optional API key for `/v1/models`.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Interactively filter, pick, and switch the active default model.
    #[arg(short = 'i', long)]
    pub pick: bool,

    /// Emit JSON output.
    #[arg(long)]
    pub json: bool,

    /// Do not auto-start the background gateway if it is currently offline.
    #[arg(long)]
    pub no_start: bool,
}

#[derive(Clone, Debug, Args)]
pub struct DoctorArgs {
    /// Optional explicit path to `agy` executable.
    #[arg(long, env = "AGY_BIN")]
    pub agy_bin: Option<PathBuf>,

    /// Target gateway socket address override.
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Optional API key for querying live gateway status.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Emit structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Default, Args)]
pub struct ProxyArgs {
    /// Proxy URL (`http://127.0.0.1:7890`, `socks5://127.0.0.1:1080`), port shorthand (`7890`),
    /// `system` (import OS/env proxy), or `off` / `clear` / `none` (disable proxy).
    pub target: Option<String>,

    /// Clear the configured proxy so `agy` connects directly (default behavior).
    #[arg(long, visible_aliases = ["clear", "unset", "disable"])]
    pub off: bool,

    /// Detect and save the current system or shell proxy for `agy`.
    #[arg(long)]
    pub from_system: bool,

    /// Probe connectivity to the official Antigravity release endpoint using the effective proxy.
    #[arg(long)]
    pub test: bool,

    /// Do not automatically restart a running managed background gateway after updating proxy settings.
    #[arg(long)]
    pub no_restart: bool,

    /// Emit structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Default, Args)]
pub struct AgyArgs {
    /// Optional explicit path to `agy` executable.
    #[arg(long, global = true, env = "AGY_BIN")]
    pub agy_bin: Option<PathBuf>,

    /// Emit structured JSON output.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub action: Option<AgyCommand>,
}

#[derive(Clone, Debug, Subcommand)]
pub enum AgyCommand {
    /// Inspect `agy` presence, version, latest release, login status, forced proxy & auto-update policy.
    #[command(visible_aliases = ["st", "info", "check"])]
    Status(AgyStatusArgs),

    /// Download, SHA-512 verify, and install the official Antigravity CLI (`agy`) for this platform.
    #[command(visible_aliases = ["get", "download"])]
    Install(AgyInstallArgs),

    /// Check (`-c`) or update the installed `agy` CLI to the latest official release.
    #[command(visible_aliases = ["upgrade", "up", "upg"])]
    Update(AgyUpdateArgs),

    /// Sign in to Antigravity interactively (`agy acp --login`) with Liftgate's proxy configuration applied.
    #[command(visible_aliases = ["auth", "signin"])]
    Login(AgyLoginArgs),

    /// View or toggle background idle auto-updating of `agy` CLI (`on` by default, `off` to disable).
    #[command(name = "auto-update", visible_aliases = ["autoupdate", "auto"])]
    AutoUpdate(AgyAutoUpdateArgs),

    /// View, set, or clear the forced outbound proxy injected into `agy` (`none` by default).
    #[command(visible_alias = "px")]
    Proxy(ProxyArgs),

    /// List account models directly via `agy models` with Liftgate's proxy configuration applied.
    #[command(visible_aliases = ["m", "ls"])]
    Models,

    /// Forward `mcp` subcommands (`list`, `add`, `remove`, `enable`, `disable`) to `agy` with proxy applied.
    Mcp(AgyForwardArgs),

    /// Forward `plugin` subcommands (`list`, `install`, `uninstall`, `enable`, `disable`) to `agy` with proxy applied.
    #[command(visible_alias = "plugins")]
    Plugin(AgyForwardArgs),

    /// Show Antigravity CLI release notes via `agy changelog` with proxy applied.
    Changelog,

    /// Run `agy` interactively or pass arbitrary flags/prompts with Liftgate's proxy configuration applied.
    #[command(visible_alias = "exec")]
    Run(AgyForwardArgs),

    /// Forward any other subcommand directly to `agy` with Liftgate's proxy configuration applied.
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

#[derive(Clone, Debug, Default, Args)]
pub struct AgyStatusArgs {
    /// Skip querying the remote Antigravity manifest for the latest available version.
    #[arg(long)]
    pub offline: bool,

    /// Skip probing `agy models` when the local gateway is offline.
    #[arg(long)]
    pub no_auth_check: bool,
}

#[derive(Clone, Debug, Default, Args)]
pub struct AgyInstallArgs {
    /// Only check the latest official `agy` release for this platform without installing.
    #[arg(short = 'c', long)]
    pub check: bool,

    /// Reinstall/overwrite even if `agy` is already on the latest version.
    #[arg(short = 'f', long)]
    pub force: bool,

    /// Custom installation directory (defaults to `%LOCALAPPDATA%\agy\bin` on Windows, `~/.local/bin` on Unix).
    #[arg(short = 'd', long)]
    pub dir: Option<PathBuf>,

    /// One-off proxy override for downloading the `agy` manifest and binary.
    #[arg(long)]
    pub proxy: Option<String>,

    /// Skip running `agy install --skip-aliases` shell PATH setup after placing the binary.
    #[arg(long)]
    pub skip_path: bool,

    /// Internal/mirror manifest base URL override for self-hosted mirrors and integration tests.
    #[arg(long, hide = true, env = "LIFTGATE_AGY_MANIFEST_BASE_URL")]
    pub manifest_base_url: Option<String>,
}

#[derive(Clone, Debug, Default, Args)]
pub struct AgyUpdateArgs {
    /// Only check whether a newer official `agy` build is available without updating.
    #[arg(short = 'c', long)]
    pub check: bool,

    /// Re-download and reinstall even if `agy` is already on the latest version.
    #[arg(short = 'f', long)]
    pub force: bool,

    /// One-off proxy override for checking and downloading the update.
    #[arg(long)]
    pub proxy: Option<String>,

    /// Do not automatically restart a running managed background gateway after updating `agy`.
    #[arg(long)]
    pub no_restart: bool,

    /// Internal/mirror manifest base URL override for self-hosted mirrors and integration tests.
    #[arg(long, hide = true, env = "LIFTGATE_AGY_MANIFEST_BASE_URL")]
    pub manifest_base_url: Option<String>,
}

#[derive(Clone, Debug, Default, Args)]
pub struct AgyLoginArgs {
    /// Automatically download and install `agy` first if it is missing on this machine.
    #[arg(long)]
    pub install: bool,

    /// One-off proxy override injected into `agy acp --login`.
    #[arg(long)]
    pub proxy: Option<String>,

    /// Do not restart a running managed background gateway after signing in.
    #[arg(long)]
    pub no_restart: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum AutoUpdateToggle {
    /// Print current background idle auto-update setting and live daemon status.
    #[default]
    #[value(aliases = ["st", "show"])]
    Status,
    /// Enable background idle auto-updating of `agy` CLI (default).
    #[value(aliases = ["true", "1", "enable", "enabled"])]
    On,
    /// Disable background idle auto-updating of `agy` CLI.
    #[value(aliases = ["false", "0", "disable", "disabled"])]
    Off,
}

#[derive(Clone, Debug, Default, Args)]
pub struct AgyAutoUpdateArgs {
    /// Desired state (`on`, `off`, or `status`). Omit to inspect current status.
    #[arg(value_enum, default_value_t = AutoUpdateToggle::Status)]
    pub mode: AutoUpdateToggle,

    /// Do not automatically restart a running managed background gateway after changing the setting.
    #[arg(long)]
    pub no_restart: bool,
}

#[derive(Clone, Debug, Default, Args)]
pub struct AgyForwardArgs {
    /// Arguments forwarded verbatim to `agy`.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "AGY_ARGS"
    )]
    pub args: Vec<OsString>,
}

#[derive(Clone, Debug, Args)]
pub struct AliasArgs {
    /// Short executable name to install (without `.exe`).
    #[arg(short = 'n', long, default_value = "lg")]
    pub name: String,

    /// Destination directory on PATH (defaults to the directory containing the current binary).
    #[arg(long)]
    pub dir: Option<PathBuf>,

    /// Replace an existing executable at the destination path.
    #[arg(short = 'f', long)]
    pub force: bool,

    /// Remove the installed alias binary.
    #[arg(long, visible_alias = "remove")]
    pub uninstall: bool,
}

#[derive(Clone, Debug, Default, Args)]
pub struct UpgradeArgs {
    /// Only check whether a newer release is available without downloading or modifying binaries.
    #[arg(short = 'c', long)]
    pub check: bool,

    /// Target a specific release version/tag (e.g. `v0.1.0` or `0.1.0`) instead of `latest`.
    #[arg(long, visible_alias = "tag")]
    pub version: Option<String>,

    /// Re-download and reinstall even if already on (or ahead of) the target release.
    #[arg(short = 'f', long)]
    pub force: bool,

    /// Override platform target triple (defaults to this binary's compile-time target).
    #[arg(long)]
    pub target: Option<String>,

    /// GitHub `owner/repo` slug hosting official releases.
    #[arg(
        long,
        env = "LIFTGATE_UPDATE_REPO",
        default_value = "acking-you/liftgate"
    )]
    pub repo: String,

    /// Optional HTTP/HTTPS/SOCKS5 proxy URL for GitHub API and asset downloads.
    #[arg(long)]
    pub proxy: Option<String>,

    /// Do not automatically restart a running managed background gateway daemon after upgrading.
    #[arg(long)]
    pub no_restart: bool,

    /// Output structured JSON for scripting and automation.
    #[arg(long)]
    pub json: bool,

    /// Internal/mirror base URL override (used for self-hosted mirrors and integration tests).
    #[arg(long, hide = true, env = "LIFTGATE_UPDATE_BASE_URL")]
    pub base_url: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct CompletionsArgs {
    /// Target shell (`bash`, `zsh`, `fish`, `powershell`, `elvish`).
    #[arg(value_enum)]
    pub shell: Shell,

    /// Binary name to bind completions to (defaults to invoked command name, e.g. `lg` or `liftgate`).
    #[arg(long)]
    pub bin: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct ClaudeArgs {
    /// Target gateway socket address (auto-starts background gateway if offline).
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Optional gateway API key.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Primary model ID override for this session.
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// Fast/subagent model override (`ANTHROPIC_SMALL_FAST_MODEL` / `ANTHROPIC_DEFAULT_HAIKU_MODEL`).
    #[arg(long)]
    pub fast_model: Option<String>,

    /// Keep Claude Code's normal permission prompts instead of `--dangerously-skip-permissions`.
    #[arg(long)]
    pub no_bypass: bool,

    /// Explicit path to `claude` executable.
    #[arg(long, env = "CLAUDE_BIN")]
    pub claude_bin: Option<PathBuf>,

    /// Arguments forwarded verbatim to `claude` (works with or without `--`).
    #[arg(last = true, value_name = "CLAUDE_ARGS")]
    pub passthrough_args: Vec<String>,
}

impl ClaudeArgs {
    #[must_use]
    pub fn default_interactive() -> Self {
        Self {
            listen: None,
            api_key: None,
            model: None,
            fast_model: None,
            no_bypass: false,
            claude_bin: std::env::var_os("CLAUDE_BIN")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            passthrough_args: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct CodexArgs {
    /// Target gateway socket address (auto-starts background gateway if offline).
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Optional gateway API key.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Primary model ID override for this session.
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// Keep Codex CLI's normal approval/sandbox prompts instead of `--dangerously-bypass-approvals-and-sandbox`.
    #[arg(long)]
    pub no_bypass: bool,

    /// Explicit path to `codex` executable.
    #[arg(long, env = "CODEX_BIN")]
    pub codex_bin: Option<PathBuf>,

    /// Arguments forwarded verbatim to `codex` (works with or without `--`).
    #[arg(last = true, value_name = "CODEX_ARGS")]
    pub passthrough_args: Vec<String>,
}

impl CodexArgs {
    #[must_use]
    pub fn default_interactive() -> Self {
        Self {
            listen: None,
            api_key: None,
            model: None,
            no_bypass: false,
            codex_bin: std::env::var_os("CODEX_BIN")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            passthrough_args: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum ConfigTarget {
    /// Configure both Claude Code and Codex CLI.
    #[default]
    All,
    /// Configure Claude Code (`.claude/settings.local.json` or `~/.claude/settings.json`).
    Claude,
    /// Configure Codex CLI (`.codex/config.toml` + `agy-models.json` or `~/.codex/...`).
    Codex,
}

#[derive(Clone, Debug, Args)]
pub struct ConfigArgs {
    /// Target CLI harness to configure (`all`, `claude`, or `codex`).
    #[arg(value_enum, default_value_t = ConfigTarget::All)]
    pub target: ConfigTarget,

    /// Write to user-global config (`~/.claude/settings.json`, `~/.codex/config.toml`) instead of current project.
    #[arg(short = 'g', long)]
    pub global: bool,

    /// Target project directory when `--global` is not set (defaults to current working directory).
    #[arg(long)]
    pub dir: Option<PathBuf>,

    /// Target gateway socket address.
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Optional gateway API key.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Primary model ID to configure.
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// Fast/subagent model override for Claude Code.
    #[arg(long)]
    pub fast_model: Option<String>,

    /// Preserve standard permission/sandbox prompts instead of auto-bypassing.
    #[arg(long)]
    pub no_bypass: bool,

    /// Do not start or query the live gateway; synthesize config from fallback catalog if offline.
    #[arg(long)]
    pub offline: bool,

    /// Print generated configuration to stdout without writing any files.
    #[arg(long)]
    pub print: bool,

    /// Cleanly remove Liftgate-managed settings/provider while preserving unrelated user config.
    #[arg(long, visible_alias = "remove")]
    pub uninstall: bool,
}

impl ConfigArgs {
    #[must_use]
    pub fn default_local() -> Self {
        Self {
            target: ConfigTarget::All,
            global: false,
            dir: None,
            listen: None,
            api_key: None,
            model: None,
            fast_model: None,
            no_bypass: false,
            offline: false,
            print: false,
            uninstall: false,
        }
    }
}

/// Normalizes `liftgate claude ...` and `liftgate codex ...` argv sequences so callers can pass
/// native `claude` / `codex` flags or prompts directly (e.g. `liftgate claude -p "hi"` or
/// `liftgate codex exec "fix tests"`) without needing an explicit `--` separator.
#[must_use]
pub fn normalize_harness_args(raw_args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let args: Vec<OsString> = raw_args.into_iter().collect();
    let Some(sub_idx) = find_subcommand_index(&args) else {
        return args;
    };
    let Some(sub_name) = args.get(sub_idx).and_then(|s| s.to_str()) else {
        return args;
    };
    let is_claude = matches!(sub_name, "claude" | "cc");
    let is_codex = matches!(sub_name, "codex" | "cx" | "x");
    if !is_claude && !is_codex {
        return args;
    }

    if args
        .get(sub_idx + 1..)
        .is_some_and(|rest| rest.iter().any(|t| t == "--"))
    {
        return args;
    }

    let value_flags: &[&str] = if is_claude {
        &[
            "--state-dir",
            "--listen",
            "--api-key",
            "-m",
            "--model",
            "--fast-model",
            "--claude-bin",
        ]
    } else {
        &[
            "--state-dir",
            "--listen",
            "--api-key",
            "-m",
            "--model",
            "--codex-bin",
        ]
    };
    let bool_flags: &[&str] = &["--no-bypass", "-h", "--help"];

    let mut idx = sub_idx + 1;
    while let Some(tok) = args.get(idx) {
        let Some(s) = tok.to_str() else {
            break;
        };
        if bool_flags.contains(&s) {
            idx += 1;
            continue;
        }
        if value_flags.contains(&s) {
            idx += 2;
            continue;
        }
        if value_flags
            .iter()
            .any(|vf| s.strip_prefix(vf).is_some_and(|rest| rest.starts_with('=')))
        {
            idx += 1;
            continue;
        }
        break;
    }

    if idx >= args.len() {
        return args;
    }

    let mut out = Vec::with_capacity(args.len() + 1);
    if let Some(prefix) = args.get(..idx) {
        out.extend_from_slice(prefix);
    }
    out.push(OsString::from("--"));
    if let Some(suffix) = args.get(idx..) {
        out.extend_from_slice(suffix);
    }
    out
}

fn find_subcommand_index(args: &[OsString]) -> Option<usize> {
    const TOP_VALUE_FLAGS: &[&str] = &[
        "--state-dir",
        "--listen",
        "--ui-listen",
        "--agy-bin",
        "--default-model",
        "--acp-mode",
        "--acp-cwd",
        "--api-key",
        "--ui-dir",
        "--agy-proxy",
        "--agy-auto-update",
        "--max-line-bytes",
        "--max-body-bytes",
        "--channel-capacity",
        "--max-idle-sessions",
        "--max-stored-responses",
        "--turn-timeout-secs",
    ];

    let mut idx = 1usize;
    while let Some(tok) = args.get(idx) {
        let s = tok.to_str()?;
        if s == "--" {
            return None;
        }
        if !s.starts_with('-') {
            return Some(idx);
        }
        if TOP_VALUE_FLAGS.contains(&s) {
            idx += 2;
            continue;
        }
        idx += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(items: &[&str]) -> Cli {
        let normalized = normalize_harness_args(items.iter().map(OsString::from));
        Cli::try_parse_from(normalized).expect("CLI arguments should parse")
    }

    #[test]
    fn bare_invocation_defaults_to_foreground_serve_compat_mode() {
        let cli = parse_strs(&[
            "liftgate",
            "--listen",
            "127.0.0.1:9123",
            "--default-model",
            "gemini-3.8-pro-high",
        ]);
        assert!(cli.command.is_none());
        assert_eq!(cli.serve.listen.port(), 9123);
        assert_eq!(cli.serve.default_model, "gemini-3.8-pro-high");
        let start = parse_strs(&["lg", "start", "--long-context"]);
        assert!(
            matches!(start.command, Some(Commands::Start(s)) if s.serve.long_context == Some(true))
        );
        let restart = parse_strs(&["lg", "restart", "--long-context=false"]);
        assert!(
            matches!(restart.command, Some(Commands::Restart(s)) if s.long_context == Some(false))
        );
    }

    #[test]
    fn claude_and_codex_forward_native_flags_without_requiring_double_dash() {
        let cli = parse_strs(&[
            "liftgate",
            "claude",
            "-m",
            "gemini-3.8-pro-high",
            "-p",
            "summarize repo",
        ]);
        match cli.command {
            Some(Commands::Claude(c)) => {
                assert_eq!(c.model.as_deref(), Some("gemini-3.8-pro-high"));
                assert_eq!(c.passthrough_args, vec!["-p", "summarize repo"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let codex_cli = parse_strs(&["liftgate", "codex", "exec", "cargo check"]);
        match codex_cli.command {
            Some(Commands::Codex(c)) => {
                assert_eq!(c.passthrough_args, vec!["exec", "cargo check"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let short_claude = parse_strs(&["lg", "cc", "-p", "quick check"]);
        match short_claude.command {
            Some(Commands::Claude(c)) => {
                assert_eq!(c.passthrough_args, vec!["-p", "quick check"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let short_codex = parse_strs(&["lg", "cx", "exec", "cargo test"]);
        match short_codex.command {
            Some(Commands::Codex(c)) => {
                assert_eq!(c.passthrough_args, vec!["exec", "cargo test"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn harness_help_stays_with_liftgate_unless_after_double_dash() {
        let norm_own = normalize_harness_args(["liftgate", "claude", "--help"].map(OsString::from));
        assert_eq!(norm_own, vec!["liftgate", "claude", "--help"]);

        let norm_child =
            normalize_harness_args(["liftgate", "claude", "--", "--help"].map(OsString::from));
        assert_eq!(norm_child, vec!["liftgate", "claude", "--", "--help"]);
    }

    #[test]
    fn config_subcommand_defaults_to_all_targets() {
        let cli = parse_strs(&["liftgate", "config", "--global"]);
        match cli.command {
            Some(Commands::Config(cfg)) => {
                assert_eq!(cfg.target, ConfigTarget::All);
                assert!(cfg.global);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn short_subcommand_aliases_and_new_commands_parse_cleanly() {
        let top_cli = parse_strs(&["lg", "tui", "--auto-start"]);
        assert!(matches!(top_cli.command, Some(Commands::Top(t)) if t.auto_start));

        let pick_cli = parse_strs(&["lg", "m", "-i"]);
        assert!(matches!(pick_cli.command, Some(Commands::Models(m)) if m.pick));

        let alias_cli = parse_strs(&["liftgate", "alias", "--name", "lg", "--force"]);
        assert!(matches!(alias_cli.command, Some(Commands::Alias(a)) if a.name == "lg" && a.force));

        let comp_cli = parse_strs(&["lg", "completions", "powershell"]);
        assert!(matches!(
            comp_cli.command,
            Some(Commands::Completions(c)) if c.shell == Shell::PowerShell
        ));

        let upg_cli = parse_strs(&["lg", "upgrade", "--check", "--json"]);
        assert!(matches!(
            upg_cli.command,
            Some(Commands::Upgrade(u)) if u.check && u.json
        ));

        let upd_alias = parse_strs(&["liftgate", "update", "--version", "v0.1.0"]);
        assert!(matches!(
            upd_alias.command,
            Some(Commands::Upgrade(u)) if u.version.as_deref() == Some("v0.1.0")
        ));

        let proxy_set = parse_strs(&["lg", "proxy", "7890"]);
        assert!(matches!(
            proxy_set.command,
            Some(Commands::Proxy(p)) if p.target.as_deref() == Some("7890")
        ));

        let proxy_off = parse_strs(&["lg", "px", "off"]);
        assert!(matches!(
            proxy_off.command,
            Some(Commands::Proxy(p)) if p.target.as_deref() == Some("off")
        ));

        let agy_bare = parse_strs(&["lg", "agy"]);
        assert!(matches!(
            agy_bare.command,
            Some(Commands::Agy(a)) if a.action.is_none()
        ));

        let agy_install = parse_strs(&["lg", "agy", "install", "--force"]);
        assert!(matches!(
            agy_install.command,
            Some(Commands::Agy(AgyArgs { action: Some(AgyCommand::Install(i)), .. })) if i.force
        ));

        let agy_login = parse_strs(&["lg", "agy", "login", "--install"]);
        assert!(matches!(
            agy_login.command,
            Some(Commands::Agy(AgyArgs { action: Some(AgyCommand::Login(l)), .. })) if l.install
        ));

        let agy_auto_off = parse_strs(&["lg", "agy", "auto-update", "off"]);
        assert!(matches!(
            agy_auto_off.command,
            Some(Commands::Agy(AgyArgs { action: Some(AgyCommand::AutoUpdate(a)), .. })) if a.mode == AutoUpdateToggle::Off
        ));

        let agy_mcp = parse_strs(&["lg", "agy", "mcp", "list"]);
        assert!(matches!(
            agy_mcp.command,
            Some(Commands::Agy(AgyArgs { action: Some(AgyCommand::Mcp(f)), .. })) if f.args == vec![OsString::from("list")]
        ));

        let agy_ext = parse_strs(&["lg", "agy", "agents"]);
        assert!(matches!(
            agy_ext.command,
            Some(Commands::Agy(AgyArgs { action: Some(AgyCommand::External(v)), .. })) if v == vec![OsString::from("agents")]
        ));
    }
}
