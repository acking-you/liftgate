# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

## [0.1.0] - 2026-11-14

### Added
- Cross-platform (`Windows`, `Linux`, `macOS`) Rust API gateway (`agy-acp-gateway`) supervising `agy acp --process-per-session --dangerously-skip-permissions` over `stdio` JSON-RPC 2.0.
- Full OpenAI Responses API (`POST /v1/responses`, `GET /v1/responses/{id}`) with streaming SSE, `function` / `custom` / `namespace` tools, Codex `local_shell` & `apply_patch`, `reasoning.encrypted_content`, native `web_search`, and server-managed `open_page` URL fetching.
- Full Anthropic Messages API (`POST /v1/messages`, `POST /v1/messages/count_tokens`) with Extended Thinking, parallel `tool_use` / `tool_result`, native `web_search_20250305` + SSRF-safe `web_fetch_20250910`, volatile header stripping (`cch`, `<total_tokens>`), and automatic Flash routing for Auto Mode & auxiliary requests.
- Unified model discovery (`GET /v1/models`, `GET /v1/models/{model_id}`) and Codex catalog endpoint (`GET /v1/codex-models`) for replacing harness `/model` pickers with the live Antigravity model catalog.
- Bounded LRU `SessionPool` with canonical prefix-hash session reuse and `0ms` upstream `429` capacity failover.
- Real-time observability dashboard (`web/`, TypeScript + Bun + Vite + React 19 + shadcn/ui) and scoped launchers (`start-agy.ps1`, `start-agy.cmd`, `start-agy.sh`).
