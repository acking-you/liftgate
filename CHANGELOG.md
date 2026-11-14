# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added
- Turned `liftgate` into a full cross-platform management CLI (`start` / `stop` / `restart` / `status` / `logs` / `ui` / `models` / `doctor` / `config` / `claude` / `codex` / `serve`) while keeping bare `liftgate` foreground server flags 100% backwards-compatible.
- Embedded the companion React observability dashboard (`web/dist`) directly into `liftgate` at compile time (`crates/liftgate/build.rs`), with an SSE live-telemetry fallback page when built without Bun, optional `--ui-listen` dedicated port binding, and automatic `#api_key` / `#base_url` bootstrap handoff via `liftgate ui`.
- Added `liftgate claude` and `liftgate codex` zero-side-effect launchers that auto-start the background gateway if needed, inject process-scoped environment variables and auto-cleaned ephemeral settings/catalogs (`~/.liftgate/ephemeral/`), forward native arguments directly, and leave project directories, `~/.claude`, `~/.codex`, and the caller shell completely untouched on exit.
- Added `liftgate config` (`all` / `claude` / `codex`, `--global`, `--dir`, `--print`, `--uninstall`) to non-destructively generate, merge, back up (`*.liftgate.bak`), or cleanly remove local/global Claude Code and Codex CLI configurations.

### Fixed
- Surfaced upstream Antigravity quota exhaustion (`⚠ Individual quota reached...` and quota-related ACP errors) directly to clients as non-retryable Anthropic `400 invalid_request_error` / SSE `event: error` and OpenAI `429 insufficient_quota` / SSE `response.failed` instead of returning blank `end_turn` replies or poisoning pooled sessions.
- Guarded against silent zero-output upstream turns by evicting the affected session and returning an explicit upstream error rather than fabricating an empty assistant message.
- Short-circuited `agy acp` internal `Malformed function call` retries immediately when a valid XML client tool call or `web_fetch` block has already been captured, and tightened `<api_gateway_bridge_rules>` against empty native function calls and post-tool trailing text.
- Fixed Windows `start-agy` / `run-gateway` launchers (`start-agy.ps1`, `start-agy.cmd`, `run-gateway.ps1`, `run-gateway.cmd`): resolved `Resolve-NodeExecutable` PowerShell `(if ...)` command-lookup crash on `-Codex`, preserved direct console TTY handles (`isTTY`) when launching interactive Claude Code / Codex TUIs instead of piping stdout through function return assignments, guarded candidate `Test-Path` probes under `$ErrorActionPreference = "Stop"`, expanded `%VAR%` `PATH` entries, and bypassed system proxies on loopback requests.

### Changed
- Renamed the project to **Liftgate**: the binary is now `liftgate` (release archives `liftgate-<target>`) and the crates are `liftgate`, `liftgate-server` and `liftgate-types`. `AGY_*` environment variables, the Codex provider id `agy` and the `run-gateway` / `start-agy` scripts are unchanged.
- Rewrote the READMEs (Chinese moved to `README.zh-CN.md`) around a new architecture diagram, and moved setup details into `docs/` (usage guide in English and Chinese, API reference, architecture).

## [0.1.0] - 2026-11-14

### Added
- Cross-platform (`Windows`, `Linux`, `macOS`) Rust API gateway (`agy-acp-gateway`) supervising `agy acp --process-per-session --dangerously-skip-permissions` over `stdio` JSON-RPC 2.0.
- Full OpenAI Responses API (`POST /v1/responses`, `GET /v1/responses/{id}`) with streaming SSE, `function` / `custom` / `namespace` tools, Codex `local_shell` & `apply_patch`, `reasoning.encrypted_content`, native `web_search`, and server-managed `open_page` URL fetching.
- Full Anthropic Messages API (`POST /v1/messages`, `POST /v1/messages/count_tokens`) with Extended Thinking, parallel `tool_use` / `tool_result`, native `web_search_20250305` + SSRF-safe `web_fetch_20250910`, volatile header stripping (`cch`, `<total_tokens>`), and automatic Flash routing for Auto Mode & auxiliary requests.
- Unified model discovery (`GET /v1/models`, `GET /v1/models/{model_id}`) and Codex catalog endpoint (`GET /v1/codex-models`) for replacing harness `/model` pickers with the live Antigravity model catalog.
- Bounded LRU `SessionPool` with canonical prefix-hash session reuse and `0ms` upstream `429` capacity failover.
- Real-time observability dashboard (`web/`, TypeScript + Bun + Vite + React 19 + shadcn/ui) and scoped launchers (`start-agy.ps1`, `start-agy.cmd`, `start-agy.sh`).
