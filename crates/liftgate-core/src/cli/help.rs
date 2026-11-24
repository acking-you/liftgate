//! Extended `--help` quick-example banners for `liftgate` and harness subcommands.

pub(super) const AFTER_HELP_TEXT: &str = "\
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
  lg oc                   Launch OpenCode in current dir (0 side effects on exit)
  lg oc -c                Continue most recent OpenCode session (`lg oc -s <ID>`, `lg oc run`)
  lg omp                  Launch Oh My Pi (`omp`) in current dir (0 side effects on exit)
  lg omp -c               Continue latest `omp` session (`lg omp -r`, `--full-code-mode`, `models`)
  lg top                  Open real-time terminal observability dashboard (TUI)
  lg up --open            Start background gateway and open Web UI
  lg m -i                 Interactively pick & switch default Antigravity model
  lg st                   Inspect gateway health, session pool & token metrics
  lg upgrade              Check GitHub Releases & self-upgrade `lg` + `liftgate`";

pub(super) const CLAUDE_AFTER_HELP: &str = "\
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

pub(super) const CODEX_AFTER_HELP: &str = "\
Native Codex CLI Commands & Session Examples:
  lg cx                           Start an interactive session in current directory
  lg cx resume                    Open interactive session picker (alias: `-r`, `--resume`)
  lg cx resume --last             Continue most recent session (alias: `--continue`)
  lg cx resume --all              Browse & resume sessions across all directories
  lg cx resume <ID_OR_NAME>       Resume a specific session (optional trailing prompt)
  lg cx fork [--last | <ID>]      Fork a previous session into a new thread
  lg cx agents                    Open Agent Command Center to browse & switch sessions
  lg cx update                    Run the native Codex updater from a regular shell
  lg cx exec \"run cargo test\"     Run non-interactively (`lg cx exec resume --last \"...\"`)
  lg cx review --uncommitted      Run a non-interactive code review via Liftgate
  lg cx archive | unarchive | delete <SESSION>
  lg cx mcp | plugin | features | doctor | queue ...
  lg cx -- --help                 Show full native `codex --help`";

pub(super) const OPENCODE_AFTER_HELP: &str = "\
Native OpenCode Commands & Session Examples:
  lg oc                           Start an interactive OpenCode TUI session in current directory
  lg oc -c / lg oc continue       Continue most recent session (alias: `lg oc resume`)
  lg oc -s <ID> / lg oc resume <ID> Continue or create a specific session by ID
  lg oc --full-code-mode          Enable full Code Mode (`OPENCODE_FLAGS=full_codemode`)
  lg oc run \"run cargo test\"      Run a non-interactive prompt (`lg oc run -c \"...\"`)
  lg oc models                    List configured providers and live Antigravity models
  lg oc session list              List recent project sessions (`delete`, `export`, `import`)
  lg oc web | serve | acp         Start OpenCode Web UI, headless HTTP server, or ACP server
  lg oc mcp | plugin | auth | upgrade ...
  lg oc -- --help                 Show full native `opencode --help`";

pub(super) const OMP_AFTER_HELP: &str = "\
Native Oh My Pi (`omp`) Commands & Session Examples:
  lg omp                          Start an interactive Oh My Pi TUI session in current directory
  lg omp -c / lg omp continue     Continue most recent session (`lg omp resume --last`)
  lg omp -r / lg omp resume       Open interactive session picker (`lg omp resume <ID>`)
  lg omp --fork <ID>              Fork an existing session into a new branch
  lg omp --full-code-mode         Mount MCP servers directly into `eval` globals (`mcp.evalNamespaces`)
  lg omp --no-code-mode           Disable `eval` (`PI_PY=0 PI_JS=0`) and run direct tool calls
  lg omp -p \"run cargo test\"      Run a non-interactive print prompt and exit
  lg omp models [agy]             List configured providers and live Antigravity models
  lg omp commit | session | acp   AI git commit, session manager, or ACP server
  lg omp config | plugin | stats | update ...
  lg omp -- --help                Show full native `omp --help`";
