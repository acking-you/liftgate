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
  lg cx                   Launch Codex CLI in current dir (0 side effects on exit)
  lg cc                   Launch Claude Code in current dir (0 side effects on exit)
  lg top                  Open real-time terminal observability dashboard (TUI)
  lg up --open            Start background gateway and open Web UI
  lg m -i                 Interactively pick & switch default Antigravity model
  lg st                   Inspect gateway health, session pool & token metrics
  lg cfg -g               Configure ~/.claude & ~/.codex for bare `claude`/`codex`";

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

        Self {
            listen,
            ui_listen,
            agy_bin,
            default_model,
            acp_mode,
            acp_cwd,
            api_key,
            ui_dir,
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
    }
}
