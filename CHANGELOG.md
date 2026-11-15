# Changelog

All notable changes to this project will be documented in this file.

## [0.1.6] - 2026-11-15

### Fixed
- **Prevented multi-subcall `cached_read_tokens` inflation from prematurely triggering Codex / Claude Code auto-compaction**: when `agy acp` executes multiple internal `search_web` or retry sub-steps inside a single `session/prompt` RPC (multiplying a ~`100k` cached prefix into `~650k–720k` cumulative billable tokens), `turn_runner` now reconciles client-reported `usage` downward as well as upward against `UsageUpdate` single-window context occupancy (with a per-turn subcall-ceiling fallback when `UsageUpdate` is absent and full occupancy tracking across warm `SessionPool` commits), while preserving cumulative billable token totals in Liftgate telemetry.

## [0.1.5] - 2026-11-15

### Fixed
- **Automatic Head+Tail middle-truncation for oversized tool outputs & single-turn inputs**: replaced the hard `prompt is too long for the ACP single-turn limit` `400` rejection with a `176,000`-byte water-filling Head+Tail line-boundary truncator (`truncate_head_tail_lines` + `render_smart_bounded_active_slice`). Large command/file outputs (`~195 KB+`) now keep ~55% opening lines and ~45% closing lines intact with an explicit `[Gateway truncated ...: omitted M bytes across lines X..=Y ...]` notice, leave small sibling tool results 100% untouched, and preserve warm `SessionPool` prefix-hash hits on subsequent turns without interrupting agentic loops.
- **Standard reactive auto-compaction error wire contracts for OpenAI Codex & Claude Code**: mapped context-window overflows (`HydrationCompacted`, upstream ACP context-length errors, and directive overflows) across both HTTP JSON and SSE (`response.failed` / `event: error`) to OpenAI's `code: "context_length_exceeded"`, `param: "input"`, `"Your input exceeds the context window of this model. Please adjust your input and try again."` and Anthropic's `type: "invalid_request_error"`, `"prompt is too long: {used} tokens > {max} maximum"`, enabling automatic client-side reactive compaction.
- **Bounded tool-schema delta & `web_fetch` continuation payloads**: added progressive 3-tier schema slimming for large `<client_tools_update>` directives (`<= 48 KB`), bounded `/compact` directives, and Head+Tail clamping on oversized server-side `web_fetch` continuations.

## [0.1.4] - 2026-11-15

### Fixed
- Unify harness-agnostic system prompt handling across the canonical engine (`NormalizedTurnRequest::materialize_system_instructions`), OpenAI `/v1/responses`, and Anthropic `/v1/messages` (including `role: "system" | "developer"` message entries): caller system prompts always normalize into `<developer_instructions>` inside `messages` instead of `_meta.antigravity.systemPrompt.append`, eliminating false upstream `429 RESOURCE_EXHAUSTED` rejections across Claude Code, Codex CLI, OpenCode, and third-party harnesses while preserving warm `SessionPool` prefix reuse, `/compact` dialogue matching, and Auto Mode classification.
- Cap `_meta.antigravity.systemPrompt.append` at `16 KB` (`MAX_SYSTEM_APPEND_BYTES`), automatically spilling excess `<client_tools>` schemas (e.g. OpenCode or multi-MCP setups) into Turn 1's `<client_tools_update>` directive, and slim oversized `<developer_instructions>` blocks on single-turn prompts without ever truncating caller user inputs or tool outputs.
- Align `lg cc` ephemeral `effortLevel` with the selected model suffix (`-xhigh`/`-high`/`-thinking` -> `high`, `-medium` -> `medium`, `-low` -> `low`) unless `--effort` or `--model` is passed explicitly, preventing Claude Code's default `medium` effort from silently downgrading `-high` models.
- Clear `HANDLE_FLAG_INHERIT` on parent stdio handles while spawning the Windows background daemon and avoid combining mutually exclusive `DETACHED_PROCESS | CREATE_NO_WINDOW` flags so piped invocations (`lg start | Out-Null`, CI scripts) return immediately without holding parent pipes open.

### Changed
- Propagate live `--long-context` policy across `lg cc`, `lg cx`, `lg cfg`, `lg models`, `lg status`, and `lg top` without promoting legacy gateways, and resolve caller model aliases against the discovered catalog when computing per-model Claude Code `autoCompactWindow` thresholds.
- Resolve `opus`, `sonnet`, and `orchestrator` family aliases dynamically against the newest matching entries in the live ACP catalog (including `claude-opus-4-6-thinking`, `claude-sonnet-4-6`, and `gemini-pro-orchestrator-argon`).
- Preserve user-authored top-level Claude Code `autoCompactWindow` values during `lg cfg` merge and `--uninstall`, removing the `110000` fallback only when Liftgate's managed picker is present.
- Avoid duplicate active-turn rendering in single-turn prompt assembly and add compile-time context-window and chunk-size invariants.

## [0.1.3] - 2026-11-15

### Changed
- Default to one ACP prompt per client step with no Flash hydration or synthetic acknowledgement turns. Oversized cold histories retain bounded, marked archival context; oversized current input or tool updates return an explicit error instead of silently truncating the current request.
- Advertise transport-aware windows and compaction thresholds: Gemini 280k/245k, Claude 200k/170k, GPT-OSS 128k/110k. Codex catalogs now include a maximum window cap, and launchers no longer force every model to 1M. Claude uses per-model auto-compaction settings with conservative handling of shared canonical aliases.
- Make multi-turn Flash hydration and Gemini's advertised 1M/850k window experimental and opt-in via `--long-context[=true|false]`, `AGY_LONG_CONTEXT`, or `settings.json`'s `long_context`. Preserve the selected mode through managed restarts. This does **not** bypass upstream automatic compaction; the v0.1.2 description overstated that capability.

### Fixed
- Return non-retryable context errors when experimental hydration triggers upstream compaction or exceeds the seed-turn capacity, preventing repeated replay and silent loss of middle segments.
- Include cache reads/writes in OpenAI input usage and stop double-counting reasoning output. Report final context occupancy separately from cumulative internal call consumption, retaining pre-compaction occupancy so clients do not miss their compaction threshold.
- Keep warm `/compact` reuse, tool/model synchronization, multimodal forwarding, and direct requested-model quota/capacity errors intact.

## [0.1.2] - 2026-11-15

### Added
- **Lossless multi-chunk Flash context hydration & upstream compaction recovery**:
  - Bypasses upstream `agy acp`'s `191,578`-byte single-turn truncation cliff and ~`300k–320k` post-turn background compaction limit by streaming conversations above `160 KB` (up to `~900 KB` / `~300k–430k` tokens) losslessly across `<= 148 KB` hydration turns on `gemini-*-flash-low` before switching the same ACP session to the requested target model.
  - Monitors ACP `UsageUpdate` frames (`used` vs. `size`) to detect post-turn backend compaction drops (`>= 64k` peak dropping by `> 50%`), delivering the current turn's uncompacted response and retiring the compacted worker afterward so subsequent turns rehydrate cleanly.
  - Launches `agy acp` with `--include-default-customizations=false` by default (unless overridden via `--acp-arg`), stripping ~`4.5k` tokens of Antigravity IDE default skills/subagents/slash-commands per session while keeping built-in `search_web` enabled.
- **Warm `/compact` session reuse & tool/effort-invariant KV cache continuity**:
  - Added pure dialogue-hash matching (`compaction_lookup`) so Codex CLI (`CONTEXT CHECKPOINT COMPACTION`) and Claude Code (`/compact`) summarization turns reuse the already-warm coding ACP session instead of cold-replaying history, then retire the pre-compaction worker cleanly.
  - Decoupled client tool definitions and mid-session model/effort switches from `SessionPool` prefix hashes, syncing tool mutations incrementally via `<client_tools_update>` so Plan/Act mode switches, `ToolSearch`, lazy `mcp__codex_apps__*` connectors, and `/effort` changes never invalidate warm sessions.
- **Multimodal tool results & Codex async user-input capabilities**:
  - Extracted nested `input_image` / base64 `image` payloads inside OpenAI `/v1/responses` `function_call_output` / `custom_tool_call_output` items (`view_image`, Playwright/MCP screenshots) and Anthropic `/v1/messages` `tool_result` blocks into native `AcpPromptBlock::Image` frames instead of serializing raw base64 strings.
  - Advertised `request_user_input_async`, `send_user_message_async`, `send_message_to_user_async`, and `clock` in `experimental_supported_tools` (plus `supports_image_detail_original = true` and `web_search_tool_type = "text_and_image"`) on `GET /v1/codex-models` and generated Codex catalogs.
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

### Changed
- **Removed silent cross-model `429` failover**: upstream `429 RESOURCE_EXHAUSTED` / `MODEL_CAPACITY_EXHAUSTED` errors now abort in `0ms` and return directly to the caller as HTTP `429` (`rate_limit_error` / `rate_limit_exceeded`) or SSE `response.failed` / `event: error` on the exact requested model without silently downgrading or switching models.

### Fixed
- **Preserved timestamps & token-budget reminders without breaking session prefix hashes**: split billing-header scrubbing (`strip_billing_noise`: strips only `x-anthropic-billing-header`, `cch=...`, and `<fast_mode_info>`) from hash normalization (`strip_unstable_harness_noise`), ensuring `<current_time_reminder>`, `<current_time>`, `<timestamp>`, `<budget:token_budget>`, `<context_window>`, and `<rollout_budget>` reach the model intact while volatile reminder mutations never cause `SessionPool` misses.
- **Eliminated slash-command interception & compaction prompt misordering**: escaped leading `/` characters with `\u{200b}` in outbound ACP text blocks so user `/compact` or `/review` prompts are never intercepted by `agy acp`'s internal slash-command router, and placed Codex compaction `instructions` at the tail of `messages` rather than ahead of the transcript.

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
