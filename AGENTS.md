# Repository Guidelines

Rules for changing this repository.

## Mandatory Rust Engineering Standard

> **REQUIRED READING BEFORE TOUCHING ANY RUST CODE:**
> Every Rust change in this workspace **must** strictly follow [**`docs/2026-11-09-RUST.md`**](docs/2026-11-09-RUST.md).
> **Crates-First Architecture Rule:** Always prioritize extracting new subsystems, coordinators, adapters, or TUI screens into dedicated workspace crates over stuffing `mod` modules into existing crates (`liftgate-core`, `liftgate-server`, `liftgate`); fall back to an internal `mod` only when the code is genuinely too trivial to justify a crate, maximizing `rustc` pipelining, parallel builds, and incremental compile speed.
> Do not introduce `unwrap()`/`expect()` on fallible paths, raw slice indexing in long-running server code, `#[async_trait]`, bare `#[allow]`, unbounded external buffers, or non-cancel-safe `tokio::select!` arms.

## Legitimacy & Security Boundary

- Communicate with Antigravity **exclusively** through the official `agy acp --process-per-session --dangerously-skip-permissions` JSON-RPC 2.0 interface over `stdio`.
- **Never** reverse-engineer closed binaries, inspect internal credential stores, or bypass authentication gates.

## Architecture

- One-way workspace dependency graph (`resolver = "3"`, `edition = "2024"`, `rust-version = "1.100.0"`):
  `liftgate` (CLI/TUI binaries `liftgate`/`lg`) → `{liftgate-auto-update-tui, liftgate-core}` where `liftgate-auto-update-tui` owns the standalone Ratatui Auto-Update Manager UI (`rlib` compiled once in parallel with `liftgate-server`/`liftgate-core`) and `liftgate-core` owns the shared control plane (daemon lifecycle, embedded UI, `lg provider` registry CLI, config sync coordinator, async terminal/desktop harness launchers, `agy`/proxy management, doctor & self-upgrade) → `{liftgate-auto-update, liftgate-harness, liftgate-server}` where `liftgate-auto-update` owns the zero-session-gated idle fleet scheduler, `auto-update-state.json` ledger, cached binary probes & supervised harness updaters (zero HTTP/server/TUI dependencies), `liftgate-harness` owns synchronous per-harness adapters (`claude/`, `codex/`, `opencode/`, `omp/`), structure-preserving YAML/TOML/JSON editors, displaced-config ledgers & cross-platform executable/shim discovery (zero async runtime, HTTP or server dependencies), and `liftgate-server` owns HTTP/SSE ingress, provider-aware optimization & routing (`provider_runtime.rs`), ACP transport, session pool, SSRF-safe web fetch & telemetry → `{liftgate-provider-stateful, liftgate-provider-http, liftgate-tool-virtualizer}` where `liftgate-provider-stateful` owns cross-platform process-tree supervision (`job.rs`), `StdioRpcPeer`, and stateful session dialects (`codex-app-server`, `grok-acp`), `liftgate-provider-http` owns dual-mode HTTP/SSE transport (`TcpStream` + `curl --config -` over `stdin`), streaming codecs for OpenAI `/v1/responses`, Anthropic `/v1/messages` (`cache_control` breakpoint alignment) & Google Gemini `streamGenerateContent`, and `liftgate-tool-virtualizer` owns the zero-cost `HarnessToolProfile` 3-tier tool virtualizer (`AcpLoopbackMcp` & unprefixed `NativeApiTools` modes, slack-slot backfill, 0-RTT search & preflight; zero async runtime, HTTP or I/O dependencies) → `liftgate-types` (pure protocol, wire, `ProviderCapabilities` & config types; zero async runtime, HTTP or I/O dependencies) → `liftgate-codemode-js` (zero-dependency Code Mode JS template-literal healer & bare-V8 `btoa`/`atob` polyfill injector).
- `web/` is the companion real-time observability dashboard (TypeScript + Bun + Vite + React 19 + Tailwind CSS v4 + Radix Dialog). Release builds embed `web/dist` via `crates/liftgate-core/build.rs` so `liftgate` serves it at `/`.
- Cross-platform child supervision (`crates/liftgate-provider-stateful/src/job.rs`, re-exported in `crates/liftgate-server/src/job.rs`): Win32 Job Object (`KILL_ON_JOB_CLOSE`) on Windows; `process_group(0)` + `killpg` on Unix plus `prctl(PR_SET_PDEATHSIG, SIGTERM)` on Linux, backed by a chained panic hook.

## Protocol & Session Invariants

- OpenAI `/v1/responses` (`instructions` and `developer`/`system` items) and Anthropic `/v1/messages` (`system` prompts) normalize into `<developer_instructions>` blocks in `messages` (never into `_meta.antigravity.systemPrompt.append`), avoiding upstream `429` rejections on large harness prompts while keeping `SessionPool` prefix hashes deterministic across turns.
- Billing/telemetry noise (`x-anthropic-billing-header`, `cch=...`, `<fast_mode_info>`) is stripped via `session_key::strip_billing_noise` when forwarding prompts, whereas legitimate harness context signals (`<current_time_reminder>`, `<timestamp>`, `<budget:token_budget>`, `<context_window>`, `<rollout_budget>`) are preserved in forwarded prompts and normalized only inside `session_key::strip_unstable_harness_noise` for deterministic prefix hashing.
- Client tool list mutations and mid-session model/effort switches sync incrementally on warm sessions (`<client_tools_update>` + `session/set_model`), and `/compact` summarization turns match warm sessions via `compaction_lookup` before retiring the pre-compaction session.
- Never silently fail over across models on upstream `429` / capacity errors: abort stalled upstream retries immediately and return `429` (`rate_limit_error`) on the requested model.
- Session titles use the TUI-selected title model (default `gemini-4-hydrogen-low`). Other auxiliary requests use the fast model selector, which skips Gemini 3.8 Flash. Both use ephemeral sessions without evicting or mutating cached primary sessions.
- Server build/run scripts (`run-gateway.ps1`, `run-gateway.cmd`, `run-gateway.sh`) build `web/dist` (when Bun is present) and `liftgate` (`--release --locked`) before starting the gateway. Built-in CLI/GUI harness launchers (`lg cx`, `lg cc`, `lg oc`, `lg omp`, `lg cxa`, `lg cd` in `crates/liftgate-core/src/harness/` & `desktop.rs`) scope gateway credentials and live model catalogs exclusively to the launched child session and restore caller state cleanly on exit; never mutate `~/.codex/config.toml`, `~/.config/opencode/opencode.json`, `~/.omp/agent/`, or global `OPENAI_*` variables unless `lg config` is explicitly invoked.

## Verification

CI gates live in `.github/workflows/syntax-check.yml`. Run every check relevant to your change before committing:

- **Rust formatting, lints, tests & docs**:
  ```bash
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
  cargo test --workspace --all-features --locked
  RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
  ```
- **Platform-gated Rust code** (`job.rs`, `platform.rs`, `desktop.rs`): cross-check targets (`--target x86_64-unknown-linux-gnu`, `--target x86_64-apple-darwin`, `--target x86_64-pc-windows-msvc`) when editing OS-specific blocks.
- **Web dashboard** (`web/`):
  ```bash
  cd web && bun install --frozen-lockfile && bun run build
  ```

## Commits, PRs & Releases

- Keep commits short and imperative; never commit secrets, local proxy URLs, or runtime state (`.agy-runtime/`, `.claude/`, `claude-carbon.settings.json`).
- Keep `version` as the first key under `[workspace.package]` in `Cargo.toml` (read by `.github/workflows/release.yml` on `vX.Y.Z` tags) and record user-visible changes in `CHANGELOG.md`.
