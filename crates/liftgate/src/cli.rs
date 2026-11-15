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
  lg cx resume            Browse & resume Codex sessions (`--last`, `--all`, `<ID>`)
  lg cxa                  Launch Codex App GUI via Liftgate (0 pollution on exit)
  lg cc                   Launch Claude Code in current dir (0 side effects on exit)
  lg cc -r                Browse & resume Claude sessions (`lg cc resume`, `lg cc -c`)
  lg cd                   Launch Claude Desktop GUI via Liftgate (auto-restores on exit)
  lg top                  Open real-time terminal observability dashboard (TUI)
  lg up --open            Start background gateway and open Web UI
  lg m -i                 Interactively pick & switch default Antigravity model
  lg st                   Inspect gateway health, session pool & token metrics
  lg upgrade              Check GitHub Releases & self-upgrade `lg` + `liftgate`";

const CLAUDE_AFTER_HELP: &str = "\
Native Claude Code Commands & Session Examples:
  lg cc                           Start an interactive session in current directory
  lg cc -r / lg cc resume         Open interactive session picker (`Ctrl+A` for all projects)
  lg cc -c / lg cc continue       Continue most recent session (`lg cc resume --last`)
  lg cc resume <ID_OR_NAME>       Resume a specific session (alias for `--resume <ID>`)
  lg cc fork [--last | <ID>]      Fork a previous session (`--resume/--continue --fork-session`)
  lg cc -p \"summarize changes\"    Print non-interactive response and exit
  lg cc -w [name]                 Start session in an isolated git worktree
  lg cc --bg \"refactor module\"    Start a background session
  lg cc agents | attach | logs | stop | respawn | rm ...
  lg cc mcp | plugin | doctor | auth | auto-mode | purge ...
  lg cc -- --help                 Show full native `claude --help`";

const CODEX_AFTER_HELP: &str = "\
Native Codex CLI Commands & Session Examples:
  lg cx                           Start an interactive session in current directory
  lg cx resume                    Open interactive session picker (alias: `-r`, `--resume`)
  lg cx resume --last             Continue most recent session (alias: `--continue`)
  lg cx resume --all              Browse & resume sessions across all directories
  lg cx resume <ID_OR_NAME>       Resume a specific session (optional trailing prompt)
  lg cx fork [--last | <ID>]      Fork a previous session into a new thread
  lg cx agents                    Open Agent Command Center to browse & switch sessions
  lg cx exec \"run cargo test\"     Run non-interactively (`lg cx exec resume --last \"...\"`)
  lg cx review --uncommitted      Run a non-interactive code review via Liftgate
  lg cx archive | unarchive | delete <SESSION>
  lg cx mcp | plugin | features | doctor | queue ...
  lg cx -- --help                 Show full native `codex --help`";

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
    about = "Antigravity ACP local API gateway, live TUI & web dashboard, and Claude / Codex CLI & Desktop manager (short alias: `lg`)",
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
    #[command(visible_alias = "cc", after_help = CLAUDE_AFTER_HELP)]
    Claude(ClaudeArgs),

    /// Launch Claude Desktop GUI routed through Liftgate with crash-safe auto-restoring 3P session config.
    #[command(visible_aliases = ["cd", "claude-app"])]
    ClaudeDesktop(ClaudeDesktopArgs),

    /// Launch Codex CLI in the current directory via Liftgate with zero project/global file side effects.
    #[command(visible_alias = "cx", alias = "x", after_help = CODEX_AFTER_HELP)]
    Codex(CodexArgs),

    /// Launch Codex App GUI routed through Liftgate with an isolated ephemeral `CODEX_HOME` (zero pollution).
    #[command(visible_aliases = ["cxa", "codex-desktop"])]
    CodexApp(CodexAppArgs),

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

    /// Internal detached reaper that cleans up or restores desktop session state once the GUI app exits.
    #[command(name = "__desktop-watch", hide = true)]
    InternalDesktopWatch(DesktopWatchArgs),
}

#[derive(Clone, Debug, Args)]
pub struct DesktopWatchArgs {
    /// Path to the desktop session journal file.
    #[arg(long)]
    pub journal: PathBuf,
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

    /// Disable Claude Code's experimental Code Mode (`CLAUDE_CODE_EXPERIMENTAL_CODE_MODE=0`) for this session (enabled by default).
    #[arg(long)]
    pub no_code_mode: bool,

    /// Launch Claude Desktop GUI instead of Claude Code CLI.
    #[arg(long, visible_alias = "app")]
    pub desktop: bool,

    /// Restart Claude Desktop automatically if it is already running (used with `--desktop`).
    #[arg(long)]
    pub restart_app: bool,

    /// Wait in the foreground until Claude Desktop exits instead of detaching the cleanup watcher (used with `--desktop`).
    #[arg(short = 'w', long)]
    pub wait: bool,

    /// Explicit path to `claude` executable.
    #[arg(long, env = "CLAUDE_BIN")]
    pub claude_bin: Option<PathBuf>,

    /// Explicit path to Claude Desktop executable or `.app` bundle (used with `--desktop`).
    #[arg(long, env = "CLAUDE_DESKTOP_BIN")]
    pub claude_desktop_bin: Option<PathBuf>,

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
            no_code_mode: false,
            desktop: false,
            restart_app: false,
            wait: false,
            claude_bin: std::env::var_os("CLAUDE_BIN")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            claude_desktop_bin: std::env::var_os("CLAUDE_DESKTOP_BIN")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            passthrough_args: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct ClaudeDesktopArgs {
    /// Target gateway socket address (auto-starts background gateway if offline).
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Optional gateway API key.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Primary model ID selected by default in Claude Desktop.
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// Gracefully restart Claude Desktop if an instance is already running.
    #[arg(long)]
    pub restart_app: bool,

    /// Immediately restore any active or interrupted Liftgate 3P session back to original Claude Desktop state without launching.
    #[arg(long)]
    pub restore: bool,

    /// Wait in the foreground until Claude Desktop closes instead of spawning a detached cleanup watcher.
    #[arg(short = 'w', long)]
    pub wait: bool,

    /// Explicit path to Claude Desktop executable or `.app` bundle.
    #[arg(long, env = "CLAUDE_DESKTOP_BIN")]
    pub claude_desktop_bin: Option<PathBuf>,

    /// Extra arguments forwarded to Claude Desktop.
    #[arg(last = true, value_name = "CLAUDE_DESKTOP_ARGS")]
    pub passthrough_args: Vec<String>,
}

impl ClaudeDesktopArgs {
    #[must_use]
    pub fn default_interactive() -> Self {
        Self {
            listen: None,
            api_key: None,
            model: None,
            restart_app: false,
            restore: false,
            wait: false,
            claude_desktop_bin: std::env::var_os("CLAUDE_DESKTOP_BIN")
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

    /// Explicitly select hybrid Codex Code Mode (`exec` + `wait` JavaScript orchestration alongside direct tools; enabled by default).
    #[arg(long, conflicts_with_all = ["code_mode_only", "no_code_mode"])]
    pub code_mode: bool,

    /// Enable strict Codex Code Mode Only (`exec` + `wait` JavaScript tool orchestration exclusively) for this session.
    #[arg(long, conflicts_with_all = ["code_mode", "no_code_mode"])]
    pub code_mode_only: bool,

    /// Disable Codex Code Mode (`tool_mode = "direct"`, `features.code_mode = false`) for this session.
    #[arg(long, conflicts_with_all = ["code_mode", "code_mode_only"])]
    pub no_code_mode: bool,

    /// Launch Codex App GUI instead of Codex CLI.
    #[arg(long, visible_alias = "desktop")]
    pub app: bool,

    /// Gracefully restart Codex App if an instance is already running (used with `--app`).
    #[arg(long)]
    pub restart_app: bool,

    /// Use a disposable Electron `--user-data-dir` deleted on exit instead of `~/.liftgate/desktop/codex-app` (used with `--app`).
    #[arg(long)]
    pub ephemeral: bool,

    /// Wait in the foreground until Codex App closes instead of spawning a detached cleanup watcher (used with `--app`).
    #[arg(short = 'w', long)]
    pub wait: bool,

    /// Explicit path to `codex` executable.
    #[arg(long, env = "CODEX_BIN")]
    pub codex_bin: Option<PathBuf>,

    /// Explicit path to Codex App executable or `.app` bundle (used with `--app`).
    #[arg(long, env = "CODEX_APP_BIN")]
    pub codex_app_bin: Option<PathBuf>,

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
            code_mode: false,
            code_mode_only: false,
            no_code_mode: false,
            app: false,
            restart_app: false,
            ephemeral: false,
            wait: false,
            codex_bin: std::env::var_os("CODEX_BIN")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            codex_app_bin: std::env::var_os("CODEX_APP_BIN")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            passthrough_args: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct CodexAppArgs {
    /// Target gateway socket address (auto-starts background gateway if offline).
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Optional gateway API key.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Primary model ID configured as default in Codex App.
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// Keep Codex App's standard approval/sandbox prompts instead of `approval_policy = "never"`.
    #[arg(long)]
    pub no_bypass: bool,

    /// Explicitly select hybrid Codex Code Mode (`exec` + `wait` JavaScript orchestration alongside direct tools; enabled by default).
    #[arg(long, conflicts_with_all = ["code_mode_only", "no_code_mode"])]
    pub code_mode: bool,

    /// Enable strict Codex Code Mode Only (`exec` + `wait` JavaScript tool orchestration exclusively) for this session.
    #[arg(long, conflicts_with_all = ["code_mode", "no_code_mode"])]
    pub code_mode_only: bool,

    /// Disable Codex Code Mode (`tool_mode = "direct"`, `features.code_mode = false`) for this session.
    #[arg(long, conflicts_with_all = ["code_mode", "code_mode_only"])]
    pub no_code_mode: bool,

    /// Gracefully restart Codex App if an instance is already running.
    #[arg(long)]
    pub restart_app: bool,

    /// Use a disposable Electron `--user-data-dir` wiped on exit instead of `~/.liftgate/desktop/codex-app`.
    #[arg(long)]
    pub ephemeral: bool,

    /// Wait in the foreground until Codex App closes instead of spawning a detached cleanup watcher.
    #[arg(short = 'w', long)]
    pub wait: bool,

    /// Explicit path to Codex App executable or `.app` bundle.
    #[arg(long, env = "CODEX_APP_BIN")]
    pub codex_app_bin: Option<PathBuf>,

    /// Optional explicit `codex` CLI binary injected via `CODEX_CLI_PATH` (uses Codex App's bundled binary by default).
    #[arg(long)]
    pub codex_bin: Option<PathBuf>,

    /// Optional workspace path or extra arguments forwarded to Codex App.
    #[arg(last = true, value_name = "CODEX_APP_ARGS")]
    pub passthrough_args: Vec<String>,
}

impl CodexAppArgs {
    #[must_use]
    pub fn default_interactive() -> Self {
        Self {
            listen: None,
            api_key: None,
            model: None,
            no_bypass: false,
            code_mode: false,
            code_mode_only: false,
            no_code_mode: false,
            restart_app: false,
            ephemeral: false,
            wait: false,
            codex_app_bin: std::env::var_os("CODEX_APP_BIN")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            codex_bin: None,
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

    /// Disable default Code Mode in generated Claude Code and Codex CLI configurations.
    #[arg(long)]
    pub no_code_mode: bool,

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
            no_code_mode: false,
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
    let Some(kind) = HarnessSubcommand::from_name(sub_name) else {
        return args;
    };
    let Some(rest) = args.get(sub_idx + 1..) else {
        return args;
    };
    if rest.is_empty() {
        return args;
    }

    let dd_pos = rest.iter().position(|t| t == "--");
    let pre_dd = match dd_pos {
        Some(pos) => rest.get(..pos).unwrap_or_default(),
        None => rest,
    };
    let post_dd = dd_pos.and_then(|pos| rest.get(pos..));

    let is_desktop_mode = detect_desktop_mode(kind, pre_dd);
    let value_flags = harness_value_flags(kind);
    let bool_flags = harness_bool_flags(kind, is_desktop_mode);

    // Caller explicitly separated Liftgate options from raw child arguments (`lg cx [-m ...] -- ...`).
    if dd_pos.is_some() && all_tokens_are_liftgate_flags(pre_dd, value_flags, bool_flags) {
        return args;
    }

    let native_value_flags = native_single_value_flags(kind);
    let mut lg_args = Vec::new();
    let mut child_args = Vec::new();
    let mut help_positions: Vec<(usize, OsString)> = Vec::new();

    let mut idx = 0usize;
    while let Some(tok) = pre_dd.get(idx) {
        let Some(s) = tok.to_str() else {
            child_args.push(tok.clone());
            idx += 1;
            continue;
        };
        if matches!(s, "-h" | "--help") {
            help_positions.push((child_args.len(), tok.clone()));
            idx += 1;
            continue;
        }
        if bool_flags.contains(&s) {
            lg_args.push(tok.clone());
            idx += 1;
            continue;
        }
        if value_flags.contains(&s) {
            lg_args.push(tok.clone());
            if let Some(val) = pre_dd.get(idx + 1) {
                lg_args.push(val.clone());
                idx += 2;
            } else {
                idx += 1;
            }
            continue;
        }
        if value_flags
            .iter()
            .any(|vf| s.strip_prefix(vf).is_some_and(|rest| rest.starts_with('=')))
        {
            lg_args.push(tok.clone());
            idx += 1;
            continue;
        }
        if native_value_flags.contains(&s) {
            child_args.push(tok.clone());
            if let Some(val) = pre_dd.get(idx + 1) {
                child_args.push(val.clone());
                idx += 2;
            } else {
                idx += 1;
            }
            continue;
        }
        child_args.push(tok.clone());
        idx += 1;
    }

    if let Some(tail) = post_dd {
        child_args.extend_from_slice(tail);
    }

    if child_args.is_empty() {
        for (_, flag) in help_positions {
            lg_args.push(flag);
        }
    } else {
        for (offset, (pos, flag)) in help_positions.into_iter().enumerate() {
            let insert_at = (pos + offset).min(child_args.len());
            child_args.insert(insert_at, flag);
        }
    }

    if !is_desktop_mode {
        match kind {
            HarnessSubcommand::Claude => rewrite_claude_session_aliases(&mut child_args),
            HarnessSubcommand::Codex => rewrite_codex_session_aliases(&mut child_args),
            HarnessSubcommand::ClaudeDesktop | HarnessSubcommand::CodexApp => {}
        }
    }

    let mut out = Vec::with_capacity(sub_idx + 2 + lg_args.len() + child_args.len());
    if let Some(prefix) = args.get(..=sub_idx) {
        out.extend_from_slice(prefix);
    }
    out.extend(lg_args);
    if !child_args.is_empty() {
        out.push(OsString::from("--"));
        out.extend(child_args);
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HarnessSubcommand {
    Claude,
    ClaudeDesktop,
    Codex,
    CodexApp,
}

impl HarnessSubcommand {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "claude" | "cc" => Some(Self::Claude),
            "claude-desktop" | "cd" | "claude-app" => Some(Self::ClaudeDesktop),
            "codex" | "cx" | "x" => Some(Self::Codex),
            "codex-app" | "cxa" | "codex-desktop" => Some(Self::CodexApp),
            _ => None,
        }
    }
}

fn detect_desktop_mode(kind: HarnessSubcommand, pre_dd: &[OsString]) -> bool {
    match kind {
        HarnessSubcommand::ClaudeDesktop | HarnessSubcommand::CodexApp => true,
        HarnessSubcommand::Codex => pre_dd
            .iter()
            .any(|t| matches!(t.to_str(), Some("--app" | "--desktop"))),
        HarnessSubcommand::Claude => {
            let has_app = pre_dd.iter().any(|t| t.to_str() == Some("--app"));
            let has_desktop = pre_dd.iter().any(|t| t.to_str() == Some("--desktop"));
            let has_lg_desktop_only = pre_dd.iter().any(|t| {
                t.to_str().is_some_and(|s| {
                    matches!(s, "--restart-app" | "--wait" | "--claude-desktop-bin")
                        || s.starts_with("--claude-desktop-bin=")
                })
            });
            let has_session_handoff = pre_dd.iter().any(|t| {
                t.to_str().is_some_and(|s| {
                    matches!(
                        s,
                        "-c" | "--continue" | "-r" | "--resume" | "resume" | "continue"
                    ) || s.starts_with("--resume=")
                })
            });
            has_app || (has_desktop && (has_lg_desktop_only || !has_session_handoff))
        }
    }
}

fn harness_value_flags(kind: HarnessSubcommand) -> &'static [&'static str] {
    match kind {
        HarnessSubcommand::Claude => &[
            "--state-dir",
            "--listen",
            "--api-key",
            "-m",
            "--model",
            "--fast-model",
            "--claude-bin",
            "--claude-desktop-bin",
        ],
        HarnessSubcommand::ClaudeDesktop => &[
            "--state-dir",
            "--listen",
            "--api-key",
            "-m",
            "--model",
            "--claude-desktop-bin",
        ],
        HarnessSubcommand::Codex | HarnessSubcommand::CodexApp => &[
            "--state-dir",
            "--listen",
            "--api-key",
            "-m",
            "--model",
            "--codex-bin",
            "--codex-app-bin",
        ],
    }
}

fn harness_bool_flags(kind: HarnessSubcommand, is_desktop_mode: bool) -> &'static [&'static str] {
    match kind {
        HarnessSubcommand::Codex => {
            if is_desktop_mode {
                &[
                    "--no-bypass",
                    "--code-mode",
                    "--code-mode-only",
                    "--no-code-mode",
                    "--app",
                    "--desktop",
                    "--restart-app",
                    "--ephemeral",
                    "-w",
                    "--wait",
                ]
            } else {
                &[
                    "--no-bypass",
                    "--code-mode",
                    "--code-mode-only",
                    "--no-code-mode",
                ]
            }
        }
        HarnessSubcommand::CodexApp => &[
            "--no-bypass",
            "--code-mode",
            "--code-mode-only",
            "--no-code-mode",
            "--restart-app",
            "--ephemeral",
            "-w",
            "--wait",
        ],
        HarnessSubcommand::Claude => {
            if is_desktop_mode {
                &[
                    "--no-bypass",
                    "--no-code-mode",
                    "--desktop",
                    "--app",
                    "--restart-app",
                    "-w",
                    "--wait",
                ]
            } else {
                &["--no-bypass", "--no-code-mode"]
            }
        }
        HarnessSubcommand::ClaudeDesktop => &["--restart-app", "--restore", "-w", "--wait"],
    }
}

fn native_single_value_flags(kind: HarnessSubcommand) -> &'static [&'static str] {
    match kind {
        HarnessSubcommand::Codex => &[
            "-c",
            "--config",
            "--enable",
            "--disable",
            "--remote",
            "--remote-auth-token-env",
            "-i",
            "--image",
            "--local-provider",
            "-p",
            "--profile",
            "-s",
            "--sandbox",
            "-C",
            "--cd",
            "--add-dir",
            "-a",
            "--ask-for-approval",
            "-o",
            "--output-last-message",
            "--output-schema",
            "--thread",
            "--message",
            "--base",
            "--commit",
            "--title",
            "--thread-source",
            "--cyber-access-program",
        ],
        HarnessSubcommand::Claude => &[
            "--agent",
            "--agents",
            "--append-system-prompt",
            "--autocompact",
            "--betas",
            "--debug-file",
            "--effort",
            "--environment",
            "--fallback-model",
            "--input-format",
            "--json-schema",
            "--max-budget-usd",
            "--name",
            "-n",
            "--output-format",
            "--permission-mode",
            "--permission-prompts",
            "--plugin-dir",
            "--plugin-url",
            "--remote-control-session-name-prefix",
            "--session-id",
            "--setting-sources",
            "--settings",
            "--system-prompt",
            "--system-prompt-snapshot",
        ],
        HarnessSubcommand::ClaudeDesktop | HarnessSubcommand::CodexApp => &[],
    }
}

fn all_tokens_are_liftgate_flags(
    tokens: &[OsString],
    value_flags: &[&str],
    bool_flags: &[&str],
) -> bool {
    let mut idx = 0usize;
    while let Some(tok) = tokens.get(idx) {
        let Some(s) = tok.to_str() else {
            return false;
        };
        if matches!(s, "-h" | "--help") || bool_flags.contains(&s) {
            idx += 1;
            continue;
        }
        if value_flags.contains(&s) {
            if tokens.get(idx + 1).is_none() {
                return false;
            }
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
        return false;
    }
    true
}

fn rewrite_claude_session_aliases(child_args: &mut Vec<OsString>) {
    let is_alias = matches!(
        child_args.first().and_then(|s| s.to_str()),
        Some("resume" | "continue" | "fork")
    );
    if !is_alias {
        return;
    }
    let tail = child_args.split_off(1);
    let first_owned = child_args.pop().unwrap_or_default();
    let Some(first) = first_owned.to_str() else {
        return;
    };
    let second = tail.first().and_then(|s| s.to_str());
    match first {
        "resume" => match second {
            Some("-h" | "--help") => {
                child_args.extend(tail);
            }
            Some("--last") => {
                child_args.push(OsString::from("--continue"));
                if let Some(after_last) = tail.get(1..) {
                    child_args.extend_from_slice(after_last);
                }
            }
            _ => {
                child_args.push(OsString::from("--resume"));
                child_args.extend(tail);
            }
        },
        "continue" => {
            if !matches!(second, Some("-h" | "--help")) {
                child_args.push(OsString::from("--continue"));
            }
            child_args.extend(tail);
        }
        "fork" => match second {
            Some("-h" | "--help") => {
                child_args.extend(tail);
            }
            Some("--last") => {
                child_args.push(OsString::from("--continue"));
                child_args.push(OsString::from("--fork-session"));
                if let Some(after_last) = tail.get(1..) {
                    child_args.extend_from_slice(after_last);
                }
            }
            Some(target) if !target.starts_with('-') => {
                child_args.push(OsString::from("--resume"));
                if let Some(first_tail) = tail.first() {
                    child_args.push(first_tail.clone());
                }
                child_args.push(OsString::from("--fork-session"));
                if let Some(after_target) = tail.get(1..) {
                    child_args.extend_from_slice(after_target);
                }
            }
            _ => {
                child_args.push(OsString::from("--resume"));
                child_args.push(OsString::from("--fork-session"));
                child_args.extend(tail);
            }
        },
        _ => {}
    }
}

fn rewrite_codex_session_aliases(child_args: &mut Vec<OsString>) {
    const ROOT_VALUE_OPTS: &[&str] = &[
        "-c",
        "--config",
        "--enable",
        "--disable",
        "--remote",
        "--remote-auth-token-env",
        "-i",
        "--image",
        "--local-provider",
        "-p",
        "--profile",
        "-s",
        "--sandbox",
        "-C",
        "--cd",
        "--add-dir",
        "-a",
        "--ask-for-approval",
    ];

    let mut idx = 0usize;
    while let Some(tok) = child_args.get(idx) {
        let Some(s) = tok.to_str() else {
            return;
        };
        if s == "--" {
            return;
        }
        if matches!(s, "-r" | "--resume") {
            if let Some(slot) = child_args.get_mut(idx) {
                *slot = OsString::from("resume");
            }
            return;
        }
        if let Some(target) = s
            .strip_prefix("--resume=")
            .or_else(|| s.strip_prefix("-r="))
        {
            let target_owned = target.to_owned();
            if let Some(slot) = child_args.get_mut(idx) {
                *slot = OsString::from("resume");
            }
            if !target_owned.is_empty() {
                child_args.insert(idx + 1, OsString::from(target_owned));
            }
            return;
        }
        if s == "--continue" {
            if let Some(slot) = child_args.get_mut(idx) {
                *slot = OsString::from("resume");
            }
            child_args.insert(idx + 1, OsString::from("--last"));
            return;
        }
        if !s.starts_with('-') {
            return;
        }
        if ROOT_VALUE_OPTS.contains(&s) {
            idx += 2;
            continue;
        }
        idx += 1;
    }
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

        let short_claude = parse_strs(&["lg", "cc", "--no-code-mode", "-p", "quick check"]);
        match short_claude.command {
            Some(Commands::Claude(c)) => {
                assert!(c.no_code_mode);
                assert_eq!(c.passthrough_args, vec!["-p", "quick check"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let short_codex = parse_strs(&["lg", "cx", "exec", "cargo test"]);
        match short_codex.command {
            Some(Commands::Codex(c)) => {
                assert!(!c.code_mode && !c.code_mode_only && !c.no_code_mode);
                assert_eq!(c.passthrough_args, vec!["exec", "cargo test"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let no_code_mode_codex = parse_strs(&["lg", "cx", "--no-code-mode", "exec", "cargo test"]);
        match no_code_mode_codex.command {
            Some(Commands::Codex(c)) => {
                assert!(c.no_code_mode && !c.code_mode && !c.code_mode_only);
                assert_eq!(c.passthrough_args, vec!["exec", "cargo test"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let code_mode_codex = parse_strs(&["lg", "cx", "--code-mode", "exec", "cargo check"]);
        match code_mode_codex.command {
            Some(Commands::Codex(c)) => {
                assert!(c.code_mode && !c.code_mode_only);
                assert_eq!(c.passthrough_args, vec!["exec", "cargo check"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let code_mode_only_codex = parse_strs(&[
            "lg",
            "cx",
            "--code-mode-only",
            "-m",
            "gemini-4-carbon-high",
            "inspect workspace",
        ]);
        match code_mode_only_codex.command {
            Some(Commands::Codex(c)) => {
                assert!(c.code_mode_only && !c.code_mode);
                assert_eq!(c.model.as_deref(), Some("gemini-4-carbon-high"));
                assert_eq!(c.passthrough_args, vec!["inspect workspace"]);
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

        let cxa_cli = parse_strs(&["lg", "cxa", "-m", "gemini-4-carbon-high", "."]);
        match cxa_cli.command {
            Some(Commands::CodexApp(c)) => {
                assert_eq!(c.model.as_deref(), Some("gemini-4-carbon-high"));
                assert_eq!(c.passthrough_args, vec!["."]);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cd_cli = parse_strs(&["lg", "cd", "--restart-app", "-w"]);
        assert!(matches!(
            cd_cli.command,
            Some(Commands::ClaudeDesktop(c)) if c.restart_app && c.wait
        ));

        let cx_app = parse_strs(&["lg", "cx", "--app", "--ephemeral"]);
        assert!(matches!(
            cx_app.command,
            Some(Commands::Codex(c)) if c.app && c.ephemeral
        ));

        let cc_desktop = parse_strs(&["lg", "cc", "--desktop", "--restart-app"]);
        assert!(matches!(
            cc_desktop.command,
            Some(Commands::Claude(c)) if c.desktop && c.restart_app
        ));
    }

    #[test]
    fn native_session_commands_mixed_flags_and_conflicting_options_route_cleanly() {
        // Position-independent Liftgate flags after native subcommands
        let cx_resume = parse_strs(&["lg", "cx", "resume", "--last", "-m", "gemini-3.8-pro-high"]);
        match cx_resume.command {
            Some(Commands::Codex(c)) => {
                assert_eq!(c.model.as_deref(), Some("gemini-3.8-pro-high"));
                assert_eq!(c.passthrough_args, vec!["resume", "--last"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // `--ephemeral` without `--app` forwards to native `codex --ephemeral`
        let cx_ephemeral = parse_strs(&["lg", "cx", "--ephemeral", "exec", "cargo check"]);
        match cx_ephemeral.command {
            Some(Commands::Codex(c)) => {
                assert!(!c.app && !c.ephemeral);
                assert_eq!(
                    c.passthrough_args,
                    vec!["--ephemeral", "exec", "cargo check"]
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Child `--` inside `codex sandbox` does not confuse top-level clap parsing
        let cx_sandbox = parse_strs(&[
            "lg", "cx", "sandbox", "windows", "--", "cmd", "/c", "echo", "ok",
        ]);
        match cx_sandbox.command {
            Some(Commands::Codex(c)) => {
                assert_eq!(
                    c.passthrough_args,
                    vec!["sandbox", "windows", "--", "cmd", "/c", "echo", "ok"]
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Subcommand `--help` forwards to child instead of intercepting as Liftgate help
        let norm_sub_help =
            normalize_harness_args(["lg", "cx", "resume", "--help"].map(OsString::from));
        assert_eq!(norm_sub_help, vec!["lg", "cx", "--", "resume", "--help"]);

        // Codex `-r` / `--resume` / `--continue` ergonomic aliases (preserving `-c` as `--config`)
        let cx_short_r = parse_strs(&["lg", "cx", "-r", "--all"]);
        assert!(matches!(
            cx_short_r.command,
            Some(Commands::Codex(c)) if c.passthrough_args == vec!["resume", "--all"]
        ));
        let cx_cont = parse_strs(&["lg", "cx", "--continue"]);
        assert!(matches!(
            cx_cont.command,
            Some(Commands::Codex(c)) if c.passthrough_args == vec!["resume", "--last"]
        ));
        let cx_cfg = parse_strs(&["lg", "cx", "-c", "model_reasoning_effort='low'", "resume"]);
        assert!(matches!(
            cx_cfg.command,
            Some(Commands::Codex(c)) if c.passthrough_args == vec!["-c", "model_reasoning_effort='low'", "resume"]
        ));

        // Claude `-w` without `--desktop` forwards to native `claude --worktree`, while `--desktop -w` sets `wait`
        let cc_worktree = parse_strs(&["lg", "cc", "-w", "feat-branch"]);
        match cc_worktree.command {
            Some(Commands::Claude(c)) => {
                assert!(!c.desktop && !c.wait);
                assert_eq!(c.passthrough_args, vec!["-w", "feat-branch"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        let cc_desktop_wait = parse_strs(&["lg", "cc", "--desktop", "-w"]);
        assert!(matches!(
            cc_desktop_wait.command,
            Some(Commands::Claude(c)) if c.desktop && c.wait
        ));

        // Claude `resume` / `continue` / `fork` aliases + trailing `-m` hoisting
        let cc_resume = parse_strs(&[
            "lg",
            "cc",
            "resume",
            "abc-123",
            "-m",
            "gemini-3.8-flash-low",
        ]);
        match cc_resume.command {
            Some(Commands::Claude(c)) => {
                assert_eq!(c.model.as_deref(), Some("gemini-3.8-flash-low"));
                assert_eq!(c.passthrough_args, vec!["--resume", "abc-123"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        let cc_resume_last = parse_strs(&["lg", "cc", "resume", "--last"]);
        assert!(matches!(
            cc_resume_last.command,
            Some(Commands::Claude(c)) if c.passthrough_args == vec!["--continue"]
        ));
        let cc_fork = parse_strs(&["lg", "cc", "fork", "sess-42"]);
        assert!(matches!(
            cc_fork.command,
            Some(Commands::Claude(c)) if c.passthrough_args == vec!["--resume", "sess-42", "--fork-session"]
        ));

        // Native `claude --continue --desktop` session handoff passes through instead of launching raw 3P desktop
        let cc_handoff = parse_strs(&["lg", "cc", "-c", "--desktop"]);
        match cc_handoff.command {
            Some(Commands::Claude(c)) => {
                assert!(!c.desktop);
                assert_eq!(c.passthrough_args, vec!["-c", "--desktop"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
