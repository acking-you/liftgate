# Repository Guidelines

Rules for changing this repository.

## Mandatory Rust Engineering Standard

> **REQUIRED READING BEFORE TOUCHING ANY RUST CODE:**
> Every Rust change in this workspace **must** strictly follow [**`docs/2026-11-09-RUST.md`**](docs/2026-11-09-RUST.md) (mirrored from [`acking-you/agent-kit` — `2026-11-09-RUST.md`](https://github.com/acking-you/agent-kit/blob/main/prompts/language-guides/rust/2026-11-09-RUST.md)).
> Do not introduce `unwrap()`/`expect()` on fallible paths, raw slice indexing in long-running server code, `#[async_trait]`, bare `#[allow]`, unbounded external buffers, or non-cancel-safe `tokio::select!` arms.

## Legitimacy & Security Boundary

- Communicate with Antigravity **exclusively** through the official `agy acp --process-per-session --dangerously-skip-permissions` JSON-RPC 2.0 interface over `stdio`.
- **Never** reverse-engineer closed binaries, inspect internal credential stores, or bypass authentication gates.

## Architecture

- One-way workspace dependency graph (`resolver = "3"`, `edition = "2024"`, `rust-version = "1.100.0"`):
  `{liftgate, liftgate-desktop}` (CLI/TUI binaries `liftgate`/`lg` and standalone GPUI desktop client `liftgate-desktop`) → `liftgate-core` (shared control plane: daemon lifecycle, embedded UI, config sync, terminal/desktop harness launchers, `agy`/proxy management, doctor & self-upgrade) → `liftgate-server` (HTTP/SSE, ACP transport, session pool, process-tree supervision, SSRF-safe web fetch, telemetry) → `liftgate-types` (pure protocol, wire & config types; zero async runtime, HTTP or I/O dependencies).
- `web/` is the companion real-time observability dashboard (TypeScript + Bun + Vite + React 19 + Tailwind CSS v4 + shadcn/ui). Release builds embed `web/dist` via `crates/liftgate-core/build.rs` so both `liftgate` and `liftgate-desktop` serve it at `/`.
- Cross-platform child supervision (`crates/liftgate-server/src/job.rs`): Win32 Job Object (`KILL_ON_JOB_CLOSE`) on Windows; `process_group(0)` + `killpg` on Unix plus `prctl(PR_SET_PDEATHSIG, SIGTERM)` on Linux, backed by a chained panic hook.

## Protocol & Session Invariants

- OpenAI `/v1/responses` (`instructions` and `developer`/`system` items) and Anthropic `/v1/messages` (`system` prompts) normalize into `<developer_instructions>` blocks in `messages` (never into `_meta.antigravity.systemPrompt.append`), avoiding upstream `429` rejections on large harness prompts while keeping `SessionPool` prefix hashes deterministic across turns.
- Billing/telemetry noise (`x-anthropic-billing-header`, `cch=...`, `<fast_mode_info>`) is stripped via `session_key::strip_billing_noise` when forwarding prompts, whereas legitimate harness context signals (`<current_time_reminder>`, `<timestamp>`, `<budget:token_budget>`, `<context_window>`, `<rollout_budget>`) are preserved in forwarded prompts and normalized only inside `session_key::strip_unstable_harness_noise` for deterministic prefix hashing.
- Client tool list mutations and mid-session model/effort switches sync incrementally on warm sessions (`<client_tools_update>` + `session/set_model`), and `/compact` summarization turns match warm sessions via `compaction_lookup` before retiring the pre-compaction session.
- Never silently fail over across models on upstream `429` / capacity errors: abort stalled upstream retries immediately and return `429` (`rate_limit_error`) on the requested model.
- Session titles use the TUI-selected title model (default `gemini-4-hydrogen-low`). Other auxiliary requests use the fast model selector, which skips Gemini 3.8 Flash. Both use ephemeral sessions without evicting or mutating cached primary sessions.
- Server build/run scripts (`run-gateway.ps1`, `run-gateway.cmd`, `run-gateway.sh`) build `web/dist` (when Bun is present) and `liftgate` (`--release --locked`) before starting the gateway. Harness wrapper scripts (`start-agy.ps1`, `start-agy.cmd`, `start-agy.sh`) scope `AGY_GATEWAY_API_KEY` and `-c model_catalog_json=...` to the launched child process and restore the caller shell on exit; never mutate `~/.codex/config.toml` or global `OPENAI_*` variables.

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
