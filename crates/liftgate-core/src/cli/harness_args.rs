//! Clap argument definitions for `claude`, `codex`, `opencode`, `omp`, desktop apps, and `config`.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, ValueEnum};

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

    /// Use a disposable Electron `--user-data-dir` deleted on exit instead of `~/.liftgate/desktop/codex-app-isolated` (used with `--app`).
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

    /// Use a disposable Electron `--user-data-dir` wiped on exit instead of `~/.liftgate/desktop/codex-app-isolated`.
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
    /// Configure Claude Code, Codex CLI, OpenCode, and Oh My Pi (`omp`).
    #[default]
    All,
    /// Configure Claude Code (`.claude/settings.local.json` or `~/.claude/settings.json`).
    Claude,
    /// Configure Codex CLI (`.codex/config.toml` + `agy-models.json` or `~/.codex/...`).
    Codex,
    /// Configure OpenCode (`./opencode.json` or `~/.config/opencode/opencode.json`).
    #[value(alias = "oc")]
    Opencode,
    /// Configure Oh My Pi (`.omp/config.yml` + `liftgate-agy-provider.ts` or `~/.omp/agent/{models.yml,config.yml}`).
    #[value(alias = "oh-my-pi")]
    Omp,
}

#[derive(Clone, Debug, Args)]
pub struct OpencodeArgs {
    /// Target gateway socket address (auto-starts background gateway if offline).
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Optional gateway API key.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Primary Antigravity model ID override for this session.
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// Fast/title model override (`small_model` / `agents.title.model`).
    #[arg(long)]
    pub fast_model: Option<String>,

    /// Keep OpenCode's standard permission prompts instead of auto-allowing tool permissions.
    #[arg(long)]
    pub no_bypass: bool,

    /// Force OpenCode full Code Mode (`full_codemode`: built-in file/shell tools + `Bun.file`/`Bun.write` inside `execute`; enabled by default on OpenCode v2).
    #[arg(
        long,
        visible_alias = "code-mode-only",
        conflicts_with_all = ["mcp_code_mode", "no_code_mode"]
    )]
    pub full_code_mode: bool,

    /// Use OpenCode v2's partial MCP/namespace-only Code Mode (`OPENCODE_FLAGS=-full_codemode`) while keeping built-in file and shell tools direct.
    #[arg(long, conflicts_with_all = ["full_code_mode", "no_code_mode"])]
    pub mcp_code_mode: bool,

    /// Disable OpenCode Code Mode (`execute: deny`) so all MCP and namespace tools run as direct tool calls.
    #[arg(long, conflicts_with_all = ["full_code_mode", "mcp_code_mode"])]
    pub no_code_mode: bool,

    /// Explicit path to `opencode` executable.
    #[arg(long, env = "OPENCODE_BIN")]
    pub opencode_bin: Option<PathBuf>,

    /// Arguments forwarded verbatim to `opencode` (works with or without `--`).
    #[arg(last = true, value_name = "OPENCODE_ARGS")]
    pub passthrough_args: Vec<String>,
}

impl OpencodeArgs {
    #[must_use]
    pub fn default_interactive() -> Self {
        Self {
            listen: None,
            api_key: None,
            model: None,
            fast_model: None,
            no_bypass: false,
            full_code_mode: false,
            mcp_code_mode: false,
            no_code_mode: false,
            opencode_bin: std::env::var_os("OPENCODE_BIN")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            passthrough_args: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct OmpArgs {
    /// Target gateway socket address (auto-starts background gateway if offline).
    #[arg(long)]
    pub listen: Option<SocketAddr>,

    /// Optional gateway API key.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Primary Antigravity model ID override for this session (`modelRoles.default`).
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// Fast/auxiliary model override (`modelRoles.smol`, `commit`, `tiny`, `memory`, `widget`).
    #[arg(long)]
    pub fast_model: Option<String>,

    /// Require interactive confirmation for write/exec tools (`tools.approvalMode: always-ask`) instead of `yolo`.
    #[arg(long)]
    pub no_bypass: bool,

    /// Mount MCP servers directly as global namespaces inside `eval` (`mcp.evalNamespaces: true`).
    #[arg(
        long,
        visible_alias = "code-mode-only",
        conflicts_with = "no_code_mode"
    )]
    pub full_code_mode: bool,

    /// Disable Oh My Pi's `eval` Python/JS Code Mode (`PI_PY=0`, `PI_JS=0`) so all tools run as direct calls.
    #[arg(long, conflicts_with = "full_code_mode")]
    pub no_code_mode: bool,

    /// Explicit path to `omp` executable.
    #[arg(long, env = "OMP_BIN")]
    pub omp_bin: Option<PathBuf>,

    /// Arguments forwarded verbatim to `omp` (works with or without `--`).
    #[arg(last = true, value_name = "OMP_ARGS")]
    pub passthrough_args: Vec<String>,
}

impl OmpArgs {
    #[must_use]
    pub fn default_interactive() -> Self {
        Self {
            listen: None,
            api_key: None,
            model: None,
            fast_model: None,
            no_bypass: false,
            full_code_mode: false,
            no_code_mode: false,
            omp_bin: std::env::var_os("OMP_BIN")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            passthrough_args: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct ConfigArgs {
    /// Target CLI harness to configure (`all`, `claude`, `codex`, `opencode`, or `omp`).
    #[arg(value_enum, default_value_t = ConfigTarget::All)]
    pub target: ConfigTarget,

    /// Write to user-global config (`~/.claude/settings.json`, `~/.codex/config.toml`, `~/.config/opencode/opencode.json`, `~/.omp/agent/...`) instead of current project.
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
