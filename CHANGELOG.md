# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added
- **Antigravity CLI (`agy`) lifecycle, login & passthrough commands (`lg agy`)**:
  - `lg agy` (`lg agy status`, `--json`, `--offline`) inspects `agy` installation path, local vs. latest upstream version, Google account login status (via live gateway catalog or official `agy models` probe), active proxy policy, and idle auto-update status.
  - `lg agy install` and `lg agy update` download official platform releases (`windows_amd64`, `windows_arm64`, `darwin_arm64`, `darwin_amd64`, `linux_amd64`, `linux_arm64`, `linux_*_musl`, `android_*`), enforce fail-closed SHA-512 verification against the official Antigravity manifest, extract `antigravity` / `agy` archives in safe Rust, smoke-test `--version`, and register user `PATH` via `agy install --skip-aliases`.
  - `lg agy login` delegates interactive authentication to `agy acp --login` (`--install` auto-installs `agy` first if missing) and restarts any active managed background gateway afterward; `lg agy models`, `lg agy mcp`, `lg agy plugin`, `lg agy changelog`, `lg agy run`, and external subcommand passthrough run official `agy` commands with Liftgate's proxy policy applied.
  - Added `[i] Antigravity CLI (agy)` and `[l] Antigravity Sign-In` quick actions and live `AGY CLI` / `Proxy` / `Idle Auto-Update` status indicators to the `lg` interactive cockpit, `lg status`, `lg doctor`, `lg top`, and the embedded React Web dashboard.
- **Explicit forced `agy` proxy management (`lg proxy` / `lg px`)**:
  - Defaults strictly to **no proxy** (`direct` mode, stripping ambient `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` instead of implicitly inheriting OS registry proxies).
  - Supports shorthand port (`lg proxy 7890` -> `http://127.0.0.1:7890`), `host:port`, `http://`, `https://`, `socks5://`, `socks5h://`, opt-in OS proxy import (`lg proxy system`), connectivity testing (`lg proxy test`), and instant disable (`lg proxy off`), persisting in `~/.liftgate/settings.json` and forcibly injecting the proxy into `agy` on startup (also overridable via `--agy-proxy` / `AGY_PROXY`).
- **Configurable background idle auto-update for `agy` (`lg agy auto-update`)**:
  - Enabled by default (`true`) and toggleable via `lg agy auto-update [on|off|status]` or `--agy-auto-update` / `--no-agy-auto-update` / `AGY_AUTO_UPDATE`.
  - Periodically checks the official Antigravity manifest while the gateway is running, defers updates whenever HTTP turns or ACP streams are active, and hot-swaps `agy` during idle windows with SHA-512 verification, Windows `.exe` lock release, and automatic rollback if the new binary fails ACP handshake.

## [0.1.1] - 2026-11-15

### Added
- **In-place CLI self-upgrade (`lg upgrade` / `liftgate upgrade`)**:
  - Queries GitHub Releases (`--check` / `-c`, `--version <tag>`, `--force`, `--proxy <url>`, `--json`, `--no-restart`) with automatic redirect fallback when unauthenticated `api.github.com` requests hit HTTP 403/429 rate limits, and passes any `GITHUB_TOKEN` / `GH_TOKEN` via `stdin` rather than command-line arguments.
  - Verifies release archives fail-closed against `SHA256SUMS` (and GitHub's native `sha256:` asset digest), extracts `.tar.gz` / `.zip` bundles in pure safe Rust with per-file size caps, smoke-checks `--version`, atomically updates both `lg` and `liftgate` (with rollback and Windows running-`.exe` backup sweeping), and seamlessly restarts any active managed background gateway daemon.
  - Added `[p] Check & Upgrade CLI` quick action to the bare `lg` interactive command center.

## [0.1.0] - 2026-11-15

### Added
- **Short `lg` CLI alias & ergonomic subcommands**: shipped both `lg` and `liftgate` binaries out of the box, added `lg alias` (`--name`, `--dir`, `--force`, `--uninstall`) for installing custom short aliases onto `PATH`, `lg completions <shell>` for Bash/Zsh/Fish/PowerShell/Elvish tab completion, and short subcommand aliases (`up`, `down`, `r`, `st`, `l`, `web`, `m`, `cc`, `cx`, `cfg`, `doc`).
- **Interactive Terminal UI (`ratatui`) & visual CLI design**:
  - Bare `lg` (in an interactive terminal) opens a quick-action **Command Center** cockpit showing live gateway health, session pool, and single-key launchers.
  - `lg top` (`lg tui` / `lg dash`) launches a 4-tab real-time terminal observability dashboard (*Turns & Inspector*, *Session Pool* with `x` idle eviction, *Models* with `Enter` live default-model switching, and *Live Logs* toggling `agy` stderr vs `gateway.log`).
  - `lg models --pick` (`lg m -i`) provides an interactive live-filtered fuzzy model selector that hot-switches the managed gateway default model.
  - Upgraded `status`, `models`, `doctor` (`--json` supported), `start`/`stop`, `logs`, and error diagnostics with `NO_COLOR`-aware semantic styling, rounded UTF-8 tables (`comfy-table`), non-blocking Braille progress spinners, and actionable recovery hints.
- **Full cross-platform CLI & daemon lifecycle**: `start` / `stop` / `restart` / `status` / `logs` / `ui` / `models` / `doctor` / `config` / `claude` / `codex` / `serve`, keeping `liftgate --listen ...` foreground flags 100% backwards-compatible.
- Embedded the companion React observability dashboard (`web/dist`) directly into `liftgate` at compile time (`crates/liftgate/build.rs`), with an SSE live-telemetry fallback page when built without Bun, optional `--ui-listen` dedicated port binding, and automatic `#api_key` / `#base_url` bootstrap handoff via `liftgate ui`.
- Added `lg claude` (`lg cc`) and `lg codex` (`lg cx`) zero-side-effect launchers that auto-start the background gateway if needed, inject process-scoped environment variables and auto-cleaned ephemeral settings/catalogs (`~/.liftgate/ephemeral/`), forward native arguments directly, and leave project directories, `~/.claude`, `~/.codex`, and the caller shell completely untouched on exit.
- Added `lg config` (`lg cfg`: `all` / `claude` / `codex`, `--global`, `--dir`, `--print`, `--uninstall`) to non-destructively generate, merge, back up (`*.liftgate.bak`), or cleanly remove local/global Claude Code and Codex CLI configurations.
- **17-target multi-platform release pipeline**: automated GitHub Actions builds and `SHA256SUMS` verification across Linux static `musl` (`x86_64`, `aarch64`, `armv7hf`, `i686`, `riscv64gc`), Linux `glibc` (`x86_64`, `aarch64`, `armv7hf`, `riscv64gc`, `powerpc64le`, `s390x`), FreeBSD (`x86_64`), macOS (`aarch64` Apple Silicon & `x86_64` Intel), and Windows MSVC (`x86_64`, `aarch64`, `i686`), plus `cargo-binstall` metadata.
- **Core ACP Gateway**:
  - Cross-platform (`Windows`, `Linux`, `macOS`, `BSD`) Rust API gateway supervising `agy acp --process-per-session --dangerously-skip-permissions` over `stdio` JSON-RPC 2.0.
  - Full OpenAI Responses API (`POST /v1/responses`, `GET /v1/responses/{id}`) with streaming SSE, `function` / `custom` / `namespace` tools, Codex `local_shell` & `apply_patch`, `reasoning.encrypted_content`, native `web_search`, and server-managed `open_page` URL fetching.
  - Full Anthropic Messages API (`POST /v1/messages`, `POST /v1/messages/count_tokens`) with Extended Thinking, parallel `tool_use` / `tool_result`, native `web_search_20250305` + SSRF-safe `web_fetch_20250910`, volatile header stripping (`cch`, `<total_tokens>`), and automatic Flash routing for Auto Mode & auxiliary requests.
  - Unified model discovery (`GET /v1/models`, `GET /v1/models/{model_id}`) and Codex catalog endpoint (`GET /v1/codex-models`), plus bounded LRU `SessionPool` with canonical prefix-hash reuse and `0ms` upstream `429` capacity failover.

### Fixed
- Repaired malformed LLM JSON inside `<agy_tool_call_{nonce}>` blocks (`crates/liftgate-types/src/parser.rs`) so unescaped regex/Windows backslashes (`\.`, `\s`, `\d`, `\users`), raw control characters (`\n`, `\r`, `\t`) inside multi-line `apply_patch` diffs, trailing commas, unescaped inner quotes in freeform `"input"` envelopes, and accidental inline/fenced markdown backtick wrappers are cleanly decoded into structured tool calls instead of leaking raw XML/JSON into assistant text; also coerced mismatched `input`/`arguments` shapes between `function` and `custom` tools across both `/v1/responses` and `/v1/messages`.
- Eliminated mid-session conversation amnesia on cold ACP session replays (`crates/liftgate-types/src/session_key.rs`): bounded `build_acp_prompt_blocks` to `180 KB` (safely below upstream `agy acp`'s ~250 KB tail-truncation cliff) using a 4-tier priority compactor that pins `<developer_instructions>`, every conversational user turn, prior turn-completion summaries, recent working steps, and `### Current Caller Input` while compacting/pruning bulky archival tool outputs.
- Surfaced upstream Antigravity quota exhaustion (`⚠ Individual quota reached...` and quota-related ACP errors) directly to clients as non-retryable Anthropic `400 invalid_request_error` / SSE `event: error` and OpenAI `429 insufficient_quota` / SSE `response.failed` instead of returning blank `end_turn` replies or poisoning pooled sessions.
- Guarded against silent zero-output upstream turns by evicting the affected session and returning an explicit upstream error rather than fabricating an empty assistant message.
- Short-circuited `agy acp` internal `Malformed function call` retries immediately when a valid XML client tool call or `web_fetch` block has already been captured, and tightened `<api_gateway_bridge_rules>` against empty native function calls and post-tool trailing text.
- Fixed Windows `start-agy` / `run-gateway` launchers (`start-agy.ps1`, `start-agy.cmd`, `run-gateway.ps1`, `run-gateway.cmd`): resolved `Resolve-NodeExecutable` PowerShell `(if ...)` command-lookup crash on `-Codex`, preserved direct console TTY handles (`isTTY`) when launching interactive Claude Code / Codex TUIs instead of piping stdout through function return assignments, guarded candidate `Test-Path` probes under `$ErrorActionPreference = "Stop"`, expanded `%VAR%` `PATH` entries, and bypassed system proxies on loopback requests.
