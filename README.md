# antigravity-gateway

[![Syntax Check](https://github.com/acking-you/antigravity-gateway/actions/workflows/syntax-check.yml/badge.svg)](https://github.com/acking-you/antigravity-gateway/actions/workflows/syntax-check.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Run Google Antigravity legitimately as a standard OpenAI & Anthropic API underneath any agent harness.**

`antigravity-gateway` (`agy-acp-gateway`) is a high-performance, cross-platform (**Windows**, **Linux**, **macOS**) local Rust API gateway that supervises the official Antigravity CLI (`agy acp --process-per-session --dangerously-skip-permissions`) over standard Agent Client Protocol (`stdio` JSON-RPC 2.0) and exposes industry-standard LLM endpoints for **Claude Code**, **OpenAI Codex CLI**, **OpenCode**, **Cursor**, and any OpenAI or Anthropic SDK client — with zero binary patching, zero reverse-engineering, and zero internal credential scraping.

---

## Key Capabilities

- **Dual-Protocol Standard API Surface**:
  - **OpenAI Responses API (`POST /v1/responses`, `GET /v1/responses/{id}`)**: Full streaming SSE & non-streaming responses, stateful `previous_response_id` + stateless prefix-hash session continuation, `function` / `custom` / `namespace` tools (including on-demand deferred loading for massive remote MCP connector bundles), Codex built-in `local_shell` & freeform `apply_patch`, `reasoning.encrypted_content`, native `web_search_call` (`search` + server-managed `open_page` URL fetching), and `url_citation` annotations.
  - **Anthropic Messages API (`POST /v1/messages`, `POST /v1/messages/count_tokens`)**: Full streaming SSE & non-streaming responses, Extended Thinking (`thinking_delta` + `signature_delta` / `omitted`), parallel `tool_use` / `tool_result` loops, native `server_tool_use` (`web_search_20250305` + SSRF-safe server-side `web_fetch_20250910`), dynamic `cch` / `<total_tokens>` cache-busting header normalization, and sub-second routing of Auto Mode classifiers & background utility calls to the fastest `gemini-*-flash-low` model.
- **Full Live Model Discovery & Harness Picker Replacement**:
  - `GET /v1/models` & `GET /v1/models/{model_id}`: OpenAI & Anthropic compatible live model catalog with context-window metadata (`1M` for Gemini 4 / 3.8, `200k` for Claude 5.5).
  - `GET /v1/codex-models`: Generates a complete `model_catalog_json` specification from live `agy acp` discovery, replacing Codex CLI's built-in `/model` picker with your full Antigravity model catalog and eliminating unknown-model fallback warnings.
- **Stateful Multi-Session Pool & Instant 429 Failover**:
  - Bounded LRU `SessionPool` reuses warm `--process-per-session` ACP sessions across multi-turn tool loops by matching canonical conversation prefix hashes and forwarding only incremental deltas.
  - Real-time stream banner inspection detects upstream `429 RESOURCE_EXHAUSTED` / `MODEL_CAPACITY_EXHAUSTED` in `0ms` (before timeout) and transparently fails over to a healthy backup model while pinning subsequent continuation turns.
- **OS-Level Process-Tree Supervision**:
  - **Windows**: Win32 Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` + `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW`).
  - **Linux & macOS**: Dedicated process groups (`process_group(0)` + `killpg`), plus kernel-enforced `prctl(PR_SET_PDEATHSIG, SIGTERM)` on Linux and chained panic hooks across all platforms so orphaned `agy` child trees never leak.
- **Real-Time Companion Telemetry Dashboard (`web/`)**:
  - Built with **TypeScript + Bun + Vite + React 19 + Tailwind CSS v4 + shadcn/ui**, streaming live ACP process diagnostics, active/recent turn timelines, token/thought/cache metrics, warm session pool controls, and an interactive protocol probe via `/api/telemetry/snapshot` and `/api/telemetry/events` (SSE).

---

## Prerequisites

1. **Antigravity CLI (`agy`)** installed and signed in (`agy acp --login`).
2. **Rust `1.85+`** (Edition 2024) if building from source.
3. *(Optional)* **Bun** if running or rebuilding the companion web dashboard (`web/`), plus **Claude Code** (`claude`) and/or **OpenAI Codex CLI** (`codex`).

---

## One-Command Quickstart

The included launcher scripts (`start-agy.cmd` / `start-agy.ps1` on Windows, `start-agy.sh` on Linux & macOS) automatically:
1. Build (if needed) and start `agy-acp-gateway` in the background (`http://127.0.0.1:8787`);
2. Start the companion telemetry frontend (`http://127.0.0.1:5173`, with production static hosting also embedded at `http://127.0.0.1:8787/`);
3. Query `/v1/models` and `/v1/codex-models` to sync **every live Antigravity model** into both Claude Code's `modelPicker` and Codex CLI's `model_catalog_json`;
4. Launch `claude` or `codex` with process-scoped configuration — **leaving your global `~/.codex/config.toml` and `OPENAI_*` environment variables completely untouched after exit**.

### Windows (`CMD` or `PowerShell`)

```powershell
# Launch gateway + telemetry UI + interactive Claude Code (default: gemini-4-carbon-high)
.\start-agy.cmd

# Launch OpenAI Codex CLI with full Antigravity /model catalog & live web search/fetch
.\start-agy.cmd -Codex

# Pass one-shot prompts or flags directly through to Claude Code or Codex
.\start-agy.cmd -p "Summarize the workspace architecture"
.\start-agy.cmd -Codex exec "Inspect Cargo.toml and list the workspace crates"

# Start background services only (without entering an interactive CLI)
.\start-agy.cmd -BackgroundOnly

# Stop background gateway and frontend services
.\start-agy.cmd -StopServices
```

### Linux & macOS (`bash` / `zsh`)

```bash
chmod +x ./start-agy.sh

# Launch gateway + telemetry UI + interactive Claude Code
./start-agy.sh

# Launch OpenAI Codex CLI with full Antigravity /model catalog & live web search/fetch
./start-agy.sh --codex

# Start background services only
./start-agy.sh --services-only

# Stop background services
./start-agy.sh --stop
```

---

## Manual Harness & SDK Integration

Start the standalone gateway binary directly:

```bash
cargo run --release -p agy-acp-gateway -- \
  --listen 127.0.0.1:8787 \
  --default-model gemini-4-carbon-high \
  --acp-mode auto \
  --api-key local-agy
```

### 1. OpenAI Codex CLI (`~/.codex/config.toml`)

```toml
model = "gemini-4-carbon-high"
model_provider = "agy"
model_catalog_json = "/absolute/path/to/.agy-runtime/codex-model-catalog.json"
web_search = "live"

[model_providers.agy]
name = "Antigravity ACP Gateway"
base_url = "http://127.0.0.1:8787/v1"
env_key = "AGY_GATEWAY_API_KEY"
wire_api = "responses"
```

### 2. Claude Code

```bash
export ANTHROPIC_BASE_URL="http://127.0.0.1:8787"
export ANTHROPIC_AUTH_TOKEN="local-agy"
export ANTHROPIC_MODEL="gemini-4-carbon-high"
export ANTHROPIC_SMALL_FAST_MODEL="gemini-3.8-flash-low"
claude
```

### 3. Standard OpenAI / Anthropic SDKs

Point any OpenAI Responses SDK (`base_url="http://127.0.0.1:8787/v1"`) or Anthropic Messages SDK (`base_url="http://127.0.0.1:8787"`) at the local gateway and pass any model ID returned by `GET http://127.0.0.1:8787/v1/models` (such as `gemini-4-carbon-high`, `gemini-4-argon-high`, `gemini-4-hydrogen-xhigh`, `claude-opus-5-5-high`, `claude-sonnet-5-5-high`, or `gemini-3.8-flash-low`).

---

## Repository Layout

```text
.
├── crates/
│   ├── agy-gateway-types/    # Pure protocol, wire & config types (zero async/HTTP/I/O deps)
│   ├── agy-gateway-server/   # Axum router, ACP stdio supervisor, SessionPool, SSRF-safe web_fetch, SSE telemetry
│   └── agy-acp-gateway/      # Thin CLI binary crate & static UI hosting entrypoint
├── web/                      # Real-time telemetry & probe console (TypeScript + Bun + Vite + shadcn/ui)
├── docs/
│   └── 2026-11-09-RUST.md    # Workspace Rust engineering standard
├── start-agy.ps1             # Cross-shell PowerShell full-stack launcher
├── start-agy.cmd             # Windows CMD wrapper (bypasses PowerShell ExecutionPolicy blocks)
└── start-agy.sh              # Linux & macOS POSIX/Bash full-stack launcher
```

---

## Development & Verification

See [`AGENTS.md`](AGENTS.md) and [`docs/2026-11-09-RUST.md`](docs/2026-11-09-RUST.md) for architecture invariants and Rust engineering rules.

```bash
# Format, strict Clippy lints, unit/integration suite, and rustdoc verification
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

# Build production web console bundle
cd web && bun install --frozen-lockfile && bun run build
```

---

## License

Licensed under either of

- Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([`LICENSE-MIT`](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
