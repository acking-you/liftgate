use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use clap::builder::{Styles, styling};
use clap::{Args, CommandFactory as _, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use liftgate_types::config::{
    DEFAULT_CHANNEL_CAPACITY, DEFAULT_MAX_BODY_BYTES, DEFAULT_MAX_IDLE_SESSIONS,
    DEFAULT_MAX_LINE_BYTES, DEFAULT_MAX_STORED_RESPONSES, DEFAULT_TURN_TIMEOUT_SECS,
};

/// Sentinel placeholder token injected into client harnesses when the local gateway runs
/// without API key authentication.
pub const PLACEHOLDER_API_KEY: &str = "local-agy";

/// Clap v4 color theme used by `--help` and CLI parse errors.
#[must_use]
pub fn clap_styles() -> Styles {
    Styles::styled()
        .header(styling::AnsiColor::Cyan.on_default().bold())
        .usage(styling::AnsiColor::Cyan.on_default().bold())
        .literal(styling::AnsiColor::Green.on_default().bold())
        .placeholder(styling::AnsiColor::Cyan.on_default())
        .valid(styling::AnsiColor::Yellow.on_default())
        .error(styling::AnsiColor::Red.on_default().bold())
        .invalid(styling::AnsiColor::Yellow.on_default().bold())
}

/// Generates shell completion script text for `shell` and executable `bin_name`.
#[must_use]
pub fn render_completions(shell: Shell, bin_name: &str) -> String {
    let mut cmd = Cli::command();
    let mut buf = Vec::with_capacity(4096);
    clap_complete::generate(shell, &mut cmd, bin_name, &mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

mod harness_args;
mod help;
mod normalize;

pub use harness_args::*;
use help::{
    AFTER_HELP_TEXT, CLAUDE_AFTER_HELP, CODEX_AFTER_HELP, OMP_AFTER_HELP, OPENCODE_AFTER_HELP,
};
pub use normalize::normalize_harness_args;

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
    styles = clap_styles(),
    about = "Antigravity ACP local API gateway, live TUI & web dashboard, and Claude / Codex / OpenCode / Oh My Pi manager (short alias: `lg`)",
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

    /// Launch OpenCode in the current directory via Liftgate with zero project/global file side effects.
    #[command(visible_alias = "oc", after_help = OPENCODE_AFTER_HELP)]
    Opencode(OpencodeArgs),

    /// Launch Oh My Pi (`omp`) in the current directory via Liftgate with zero project/global file side effects.
    #[command(visible_alias = "oh-my-pi", after_help = OMP_AFTER_HELP)]
    Omp(OmpArgs),

    /// Generate, update, or uninstall project-local or global configuration files for Claude Code, Codex CLI, OpenCode, and/or Oh My Pi (`omp`).
    #[command(visible_aliases = ["setup", "cfg"])]
    Config(ConfigArgs),

    /// Diagnose `agy`, `claude`, `codex`, `opencode`, `omp`, embedded Web UI, and live gateway readiness.
    #[command(visible_alias = "doc")]
    Doctor(DoctorArgs),

    /// Inspect, install, sign in, update, or run Antigravity CLI (`agy`) with Liftgate's proxy & auto-update policy.
    Agy(AgyArgs),

    /// Manage multi-provider upstream endpoints, API credentials, active provider, and optimization modes (`agy`, `openai`, `anthropic`, `gemini`, `codex-app-server`, `grok-acp`, custom).
    #[command(visible_aliases = ["pv", "providers"])]
    Provider(ProviderArgs),

    /// View, set (`lg proxy 7890`), or clear (`lg proxy off`) the outbound proxy forced into `agy` (default: none).
    #[command(visible_alias = "px")]
    Proxy(ProxyArgs),

    /// View, set (`lg remote 192.168.1.50:8787`), or clear (`lg remote local`) a remote Liftgate gateway target.
    #[command(visible_alias = "endpoint")]
    Remote(RemoteArgs),

    /// Check GitHub Releases and automatically upgrade `lg` and `liftgate` with SHA-256 verification.
    #[command(visible_aliases = ["update", "self-update", "upg"])]
    Upgrade(UpgradeArgs),

    /// Manage periodic idle auto-updates for `lg`, `agy`, `codex`, `claude`, `opencode`, and `omp` (opens TUI by default).
    #[command(visible_aliases = ["autoupdate", "au"])]
    AutoUpdate(AutoUpdateArgs),

    /// Install or remove a short CLI executable alias (`lg` by default) on your PATH.
    Alias(AliasArgs),

    /// Generate shell completion scripts for `lg` or `liftgate`.
    Completions(CompletionsArgs),

    /// Run the HTTP gateway and Web UI in the foreground.
    Serve(ServeArgs),

    /// Internal detached reaper that cleans up or restores desktop session state once the GUI app exits.
    #[command(name = "__desktop-watch", hide = true)]
    InternalDesktopWatch(DesktopWatchArgs),

    /// Internal Windows MSIX in-container launcher that injects environment variables across the
    /// out-of-process `IDesktopAppXActivator` COM boundary.
    #[command(name = "__desktop-msix-launch", hide = true)]
    InternalDesktopMsixLaunch(DesktopMsixLaunchArgs),

    /// Internal detached helper that restarts the managed background daemon onto a newly installed binary
    /// once the previous daemon process has drained and closed its listeners.
    #[command(name = "__daemon-respawn", hide = true)]
    InternalDaemonRespawn(DaemonRespawnArgs),
}

#[derive(Clone, Copy, Debug, Args)]
pub struct DaemonRespawnArgs {
    /// PID of the retiring daemon process to wait on before rebinding listeners.
    #[arg(long)]
    pub old_pid: u32,
}

#[derive(Clone, Debug, Args)]
pub struct DesktopWatchArgs {
    /// Path to the desktop session journal file.
    #[arg(long)]
    pub journal: PathBuf,
}

#[derive(Clone, Debug, Args)]
pub struct DesktopMsixLaunchArgs {
    /// Path to the ephemeral owner-only MSIX launch specification JSON file.
    #[arg(long)]
    pub spec: PathBuf,
}

#[derive(Clone, Debug, Args)]
pub struct TopArgs {
    /// Target gateway socket address override.
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Target remote `liftgate` endpoint override (`host:port` or `http://host:port`).
    #[arg(long, conflicts_with = "listen")]
    pub remote: Option<String>,

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

    /// Override background idle self-updating of `lg` & `liftgate` (`true` or `false`; enabled by default).
    #[arg(long, env = "LG_AUTO_UPDATE")]
    pub lg_auto_update: Option<bool>,

    /// Disable background idle self-updating of `lg` & `liftgate` for this server run.
    #[arg(long, conflicts_with = "lg_auto_update")]
    pub no_lg_auto_update: bool,

    /// Offload incoming images on `>=300k`-token warm sessions to an ephemeral sub-session (`false` by default).
    #[arg(long, env = "AGY_VISION_OFFLOAD")]
    pub vision_offload: Option<bool>,

    /// Disable `>=300k`-token ephemeral vision sub-session offload for this server run.
    #[arg(long, conflicts_with = "vision_offload")]
    pub no_vision_offload: bool,

    /// Expose client tools over a loopback HTTP MCP bridge so multi-step tool cascades stay inside a single `session/prompt` (`true` by default).
    #[arg(long, env = "AGY_MCP_TOOL_BRIDGE")]
    pub mcp_tool_bridge: Option<bool>,

    /// Disable the Hybrid Loopback MCP Tool Bridge and route all client tools exclusively via XML.
    #[arg(long, conflicts_with = "mcp_tool_bridge")]
    pub no_mcp_tool_bridge: bool,

    /// Maximum persisted low-cache-hit diagnostic records (`1000` by default; `0` disables recording).
    #[arg(long, env = "AGY_MAX_LOW_CACHE_RECORDS")]
    pub max_low_cache_records: Option<usize>,

    /// Percentage threshold (`0..=100`, `50` by default) below which a turn's prompt cache hit rate is persisted.
    #[arg(long, env = "AGY_LOW_CACHE_THRESHOLD_PCT")]
    pub low_cache_threshold_pct: Option<u8>,

    /// Persist full request payload details & message breakdown for low-cache turns (`true` by default).
    #[arg(long, env = "AGY_LOW_CACHE_CAPTURE_PAYLOAD")]
    pub low_cache_capture_payload: Option<bool>,

    /// Disable full request payload capture for low-cache turns while keeping lightweight summary counters.
    #[arg(long, conflicts_with = "low_cache_capture_payload")]
    pub no_low_cache_capture_payload: bool,

    /// Capture the complete context (messages, reasoning, ACP prompt, output) of every turn to
    /// `~/.liftgate/context-captures` (`false` by default; toggle live from the dashboard).
    #[arg(long, env = "AGY_CAPTURE_FULL_CONTEXT")]
    pub capture_full_context: Option<bool>,

    /// Disable full-context capture for this server run.
    #[arg(long, conflicts_with = "capture_full_context")]
    pub no_capture_full_context: bool,

    /// Maximum retained full-context captures before the oldest is deleted (`200` by default).
    #[arg(long, env = "AGY_MAX_CONTEXT_CAPTURES")]
    pub max_context_captures: Option<usize>,

    /// Disk quota in MiB for full-context captures (`512` by default, `16..=65536`).
    #[arg(long, env = "AGY_MAX_CONTEXT_CAPTURE_MIB")]
    pub max_context_capture_mib: Option<usize>,

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
        let lg_auto_update = std::env::var("LG_AUTO_UPDATE").ok().and_then(|s| {
            match s.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "on" | "yes" => Some(true),
                "0" | "false" | "off" | "no" => Some(false),
                _ => None,
            }
        });
        let vision_offload = std::env::var("AGY_VISION_OFFLOAD").ok().and_then(|s| {
            match s.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "on" | "yes" => Some(true),
                "0" | "false" | "off" | "no" => Some(false),
                _ => None,
            }
        });
        let mcp_tool_bridge = std::env::var("AGY_MCP_TOOL_BRIDGE").ok().and_then(|s| {
            match s.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "on" | "yes" => Some(true),
                "0" | "false" | "off" | "no" => Some(false),
                _ => None,
            }
        });
        let max_low_cache_records = std::env::var("AGY_MAX_LOW_CACHE_RECORDS")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok());
        let low_cache_threshold_pct = std::env::var("AGY_LOW_CACHE_THRESHOLD_PCT")
            .ok()
            .and_then(|s| s.trim().parse::<u8>().ok());
        let low_cache_capture_payload = std::env::var("AGY_LOW_CACHE_CAPTURE_PAYLOAD")
            .ok()
            .and_then(|s| match s.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "on" | "yes" => Some(true),
                "0" | "false" | "off" | "no" => Some(false),
                _ => None,
            });
        let capture_full_context = std::env::var("AGY_CAPTURE_FULL_CONTEXT")
            .ok()
            .and_then(|s| match s.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "on" | "yes" => Some(true),
                "0" | "false" | "off" | "no" => Some(false),
                _ => None,
            });
        let max_context_captures = std::env::var("AGY_MAX_CONTEXT_CAPTURES")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok());
        let max_context_capture_mib = std::env::var("AGY_MAX_CONTEXT_CAPTURE_MIB")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok());

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
            lg_auto_update,
            no_lg_auto_update: false,
            vision_offload,
            no_vision_offload: false,
            mcp_tool_bridge,
            no_mcp_tool_bridge: false,
            max_low_cache_records,
            low_cache_threshold_pct,
            low_cache_capture_payload,
            no_low_cache_capture_payload: false,
            capture_full_context,
            no_capture_full_context: false,
            max_context_captures,
            max_context_capture_mib,
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

    /// Override background idle `lg` self-update (`true` or `false`).
    #[arg(long)]
    pub lg_auto_update: Option<bool>,

    /// Disable background idle `lg` self-update.
    #[arg(long, conflicts_with = "lg_auto_update")]
    pub no_lg_auto_update: bool,

    /// Override `>=300k`-token ephemeral vision sub-session offload (`true` or `false`).
    #[arg(long)]
    pub vision_offload: Option<bool>,

    /// Disable `>=300k`-token ephemeral vision sub-session offload.
    #[arg(long, conflicts_with = "vision_offload")]
    pub no_vision_offload: bool,

    /// Override Hybrid Loopback MCP Tool Bridge (`true` or `false`).
    #[arg(long)]
    pub mcp_tool_bridge: Option<bool>,

    /// Disable Hybrid Loopback MCP Tool Bridge.
    #[arg(long, conflicts_with = "mcp_tool_bridge")]
    pub no_mcp_tool_bridge: bool,

    /// Override maximum persisted low-cache-hit diagnostic records (`1000` by default).
    #[arg(long)]
    pub max_low_cache_records: Option<usize>,

    /// Override percentage threshold (`0..=100`, `50` by default) for recording low cache hits.
    #[arg(long)]
    pub low_cache_threshold_pct: Option<u8>,

    /// Override full request payload capture on low-cache turns (`true` or `false`, `true` by default).
    #[arg(long)]
    pub low_cache_capture_payload: Option<bool>,

    /// Disable full request payload capture on low-cache turns.
    #[arg(long, conflicts_with = "low_cache_capture_payload")]
    pub no_low_cache_capture_payload: bool,

    /// Override full-context capture of every turn (`true` or `false`, `false` by default).
    #[arg(long)]
    pub capture_full_context: Option<bool>,

    /// Disable full-context capture.
    #[arg(long, conflicts_with = "capture_full_context")]
    pub no_capture_full_context: bool,

    /// Override the retained full-context capture count (`200` by default).
    #[arg(long)]
    pub max_context_captures: Option<usize>,

    /// Override the full-context capture disk quota in MiB (`512` by default).
    #[arg(long)]
    pub max_context_capture_mib: Option<usize>,

    /// Open the Web observability dashboard in the default browser after restarting.
    #[arg(long)]
    pub open: bool,
}

#[derive(Clone, Debug, Default, Args)]
pub struct StatusArgs {
    /// Target gateway socket address override.
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Target remote `liftgate` endpoint override (`host:port` or `http://host:port`).
    #[arg(long, conflicts_with = "listen")]
    pub remote: Option<String>,

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

    /// Target remote `liftgate` endpoint override when fetching `--acp` stderr diagnostics.
    #[arg(long)]
    pub remote: Option<String>,

    /// Optional API key when fetching `--acp` stderr diagnostics.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Truncate the managed gateway log file.
    #[arg(long)]
    pub clear: bool,

    /// Print the resolved gateway log file path and exit.
    #[arg(long)]
    pub path: bool,
}

#[derive(Clone, Debug, Default, Args)]
pub struct UiArgs {
    /// Target gateway socket address override.
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Target remote `liftgate` endpoint override (`host:port` or `http://host:port`).
    #[arg(long, conflicts_with = "listen")]
    pub remote: Option<String>,

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

#[derive(Clone, Debug, Default, Args)]
pub struct ModelsArgs {
    /// Target gateway socket address override.
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Target remote `liftgate` endpoint override (`host:port` or `http://host:port`).
    #[arg(long, conflicts_with = "listen")]
    pub remote: Option<String>,

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

#[derive(Clone, Debug, Default, Args)]
pub struct DoctorArgs {
    /// Optional explicit path to `agy` executable.
    #[arg(long, env = "AGY_BIN")]
    pub agy_bin: Option<PathBuf>,

    /// Target gateway socket address override.
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Target remote `liftgate` endpoint override (`host:port` or `http://host:port`).
    #[arg(long, conflicts_with = "listen")]
    pub remote: Option<String>,

    /// Optional API key for querying live gateway status.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Emit structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Default, Args)]
pub struct RemoteArgs {
    /// Remote `liftgate` endpoint (`192.168.1.50:8787`, `http://devbox.lan:8787`, `[fd00::1]:8787`),
    /// `status` (inspect current target), or `local` / `off` / `clear` (switch back to local daemon).
    pub target: Option<String>,

    /// Switch back to local managed daemon mode (`target_mode = "local"`).
    #[arg(long, visible_aliases = ["local", "clear", "unset", "disable"])]
    pub off: bool,

    /// Optional bearer API key saved alongside the remote endpoint (`--clear-key` to remove).
    #[arg(long)]
    pub api_key: Option<String>,

    /// Clear any saved `remote_api_key` in `~/.liftgate/settings.json`.
    #[arg(long, conflicts_with = "api_key")]
    pub clear_key: bool,

    /// Save the remote endpoint without probing `/healthz` first.
    #[arg(long)]
    pub no_verify: bool,

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

#[derive(Clone, Debug, Args)]
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

#[derive(Clone, Debug, Default, Args)]
pub struct AutoUpdateArgs {
    #[command(subcommand)]
    pub action: Option<AutoUpdateCommand>,

    /// Emit structured JSON status when run non-interactively.
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Subcommand)]
pub enum AutoUpdateCommand {
    /// Open the dedicated interactive Auto-Update Manager TUI.
    #[command(visible_aliases = ["ui", "manage"])]
    Tui,

    /// Inspect auto-update toggles, check interval, installed versions, and update history.
    #[command(visible_alias = "st")]
    Status(AutoUpdateStatusArgs),

    /// Enable idle auto-update for one or more targets (`lg`, `agy`, `codex`, `claude`, `opencode`, `omp`, or `--all`).
    #[command(visible_alias = "on")]
    Enable(AutoUpdateTargetsArgs),

    /// Disable idle auto-update for one or more targets (`lg`, `agy`, `codex`, `claude`, `opencode`, `omp`, or `--all`).
    #[command(visible_alias = "off")]
    Disable(AutoUpdateTargetsArgs),

    /// Configure the periodic idle check interval in minutes (`5..=1440`, `30` by default).
    Interval(AutoUpdateIntervalArgs),

    /// Reset all auto-update switches to defaults (`lg` & `agy` enabled; harnesses disabled).
    Reset(AutoUpdateStatusArgs),

    /// Check (or check & install) updates right now for specified or enabled targets.
    #[command(visible_aliases = ["now", "check-now"])]
    Run(AutoUpdateRunArgs),
}

#[derive(Clone, Copy, Debug, Default, Args)]
pub struct AutoUpdateStatusArgs {
    /// Emit structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Default, Args)]
pub struct AutoUpdateTargetsArgs {
    /// Target names (`lg`, `agy`, `codex`, `claude`, `opencode`, `omp`, or `all`).
    pub targets: Vec<String>,

    /// Apply to all 6 managed targets (`lg`, `agy`, `codex`, `claude`, `opencode`, `omp`).
    #[arg(long)]
    pub all: bool,

    /// Emit structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Copy, Debug, Args)]
pub struct AutoUpdateIntervalArgs {
    /// Periodic idle check interval in minutes (`5..=1440`).
    pub minutes: u64,

    /// Emit structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Default, Args)]
pub struct AutoUpdateRunArgs {
    /// Optional target names (`lg`, `agy`, `codex`, `claude`, `opencode`, `omp`). Defaults to all enabled targets.
    pub targets: Vec<String>,

    /// Check/update every supported target regardless of its saved toggle.
    #[arg(long)]
    pub all: bool,

    /// Only check versions without installing updates where supported.
    #[arg(short = 'c', long)]
    pub check: bool,

    /// Emit structured JSON output.
    #[arg(long)]
    pub json: bool,
}

impl Default for UpgradeArgs {
    fn default() -> Self {
        let repo = std::env::var("LIFTGATE_UPDATE_REPO")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "acking-you/liftgate".to_owned());
        let base_url = std::env::var("LIFTGATE_UPDATE_BASE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty());
        Self {
            check: false,
            version: None,
            force: false,
            target: None,
            repo,
            proxy: None,
            no_restart: false,
            json: false,
            base_url,
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct ProviderArgs {
    #[command(subcommand)]
    pub action: Option<ProviderCommand>,

    /// Emit machine-readable JSON when listing providers (`lg provider --json`).
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Subcommand)]
pub enum ProviderCommand {
    /// List all built-in and custom providers, auth status, default models, and active state.
    #[command(visible_alias = "ls")]
    List(ProviderListArgs),

    /// Register or update a provider profile and optional API key (`--api-key-env` or `--api-key-stdin`).
    #[command(visible_aliases = ["set", "upsert"])]
    Add(Box<ProviderAddArgs>),

    /// Remove a custom provider (or reset a built-in provider to factory defaults) and clear stored credentials.
    #[command(visible_aliases = ["rm", "delete"])]
    Remove(ProviderRemoveArgs),

    /// Switch the global default active provider used when `-p` / `--provider` is omitted.
    #[command(visible_alias = "select")]
    Use(ProviderUseArgs),

    /// Test upstream connectivity, binary presence, or `/models` reachability for a provider.
    Test(ProviderTestArgs),

    /// List configured/discovered models and context windows for a provider.
    Models(ProviderModelsArgs),

    /// View or switch the gateway context/tool optimization mode (`optimized` vs `passthrough` / custom flags).
    #[command(visible_alias = "optimization")]
    Opt(ProviderOptArgs),
}

#[derive(Clone, Debug, Args)]
pub struct ProviderListArgs {
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct ProviderAddArgs {
    /// Provider ID slug (e.g. `openai`, `anthropic`, `gemini`, `codex-app-server`, `grok-acp`, or custom `my-vllm`).
    pub id: String,

    /// Protocol driver (`agy-acp`, `openai`, `anthropic`, `gemini`, `codex-app-server`, `grok-acp`); auto-inferred for built-in IDs.
    #[arg(long)]
    pub driver: Option<String>,

    /// Friendly display name.
    #[arg(long)]
    pub name: Option<String>,

    /// Upstream base URL override for HTTP Platform API providers (e.g. `https://api.openai.com/v1`).
    #[arg(long)]
    pub endpoint: Option<String>,

    /// Default primary model ID for this provider.
    #[arg(short = 'm', long)]
    pub default_model: Option<String>,

    /// Fast/auxiliary model ID override for this provider.
    #[arg(long)]
    pub fast_model: Option<String>,

    /// Session title model ID override for this provider.
    #[arg(long)]
    pub title_model: Option<String>,

    /// Environment variable name supplying the API key (e.g. `OPENAI_API_KEY`).
    #[arg(long, conflicts_with_all = ["api_key_stdin", "clear_auth"])]
    pub api_key_env: Option<String>,

    /// Read the API secret from standard input and store it in owner-only (`0600`) `~/.liftgate/credentials.json` (never argv).
    #[arg(long, conflicts_with_all = ["api_key_env", "clear_auth"])]
    pub api_key_stdin: bool,

    /// Configure unauthenticated requests (`ApiKeySource::None`) and remove any stored secret.
    #[arg(long, conflicts_with_all = ["api_key_env", "api_key_stdin"])]
    pub clear_auth: bool,

    /// Per-provider outbound proxy URL (`http://...`, `socks5://...`, port `7890`, or `direct`/`off`).
    #[arg(long)]
    pub proxy: Option<String>,

    /// Executable path override for stateful session providers (`agy`, `codex`, `grok`).
    #[arg(long)]
    pub bin: Option<PathBuf>,

    /// Per-provider optimization mode override (`optimized`, `passthrough`, or `inherit`).
    #[arg(long)]
    pub opt: Option<String>,

    /// Immediately make this provider the global active default.
    #[arg(long)]
    pub activate: bool,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct ProviderRemoveArgs {
    /// Provider ID to remove or reset.
    pub id: String,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct ProviderUseArgs {
    /// Provider ID to set as global active default.
    pub id: String,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct ProviderTestArgs {
    /// Provider ID to test (defaults to the active provider).
    pub id: Option<String>,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct ProviderModelsArgs {
    /// Provider ID whose model catalog to inspect (defaults to the active provider).
    pub id: Option<String>,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct ProviderOptArgs {
    /// Target mode (`optimized` or `passthrough`). Omit to inspect current policy or toggle granular flags.
    pub mode: Option<String>,

    #[arg(long)]
    pub prompt_distillation: Option<bool>,
    #[arg(long)]
    pub tool_virtualization: Option<bool>,
    #[arg(long)]
    pub code_mode: Option<bool>,
    #[arg(long)]
    pub strip_billing: Option<bool>,
    #[arg(long)]
    pub spill: Option<bool>,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
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

#[cfg(test)]
mod tests;
