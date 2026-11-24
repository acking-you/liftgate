# Architecture

Liftgate turns stateless HTTP APIs into stateful ACP sessions. Clients send
OpenAI Responses or Anthropic Messages requests; Liftgate runs each turn
through the official `agy acp` CLI and streams the answer back in the format
the client asked for. Contributor rules for this design are in
[AGENTS.md](../AGENTS.md).

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="../assets/readme/how-it-works-dark.svg">
  <img src="../assets/readme/how-it-works-light.svg" width="100%" alt="Clients send stateless HTTP to Liftgate, which translates, reuses warm sessions, streams events back, and talks JSON-RPC over stdio to one agy acp process per session; only agy holds the credentials for Google Antigravity.">
</picture>

## Why ACP

Many proxies scrape local OAuth tokens, patch closed binaries or spoof private
headers. Those break easily, can leak credentials and may violate the terms of
service. Liftgate does none of that:

- It talks to Antigravity only through `agy acp`, the JSON-RPC 2.0 over stdio
  interface the CLI ships for editors and IDEs.
- Login (`agy acp --login`), token refresh, quota and upstream TLS stay inside
  the `agy` process. Liftgate never reads credentials.
- No binary patching, no injected certificates, no private endpoints.

## The request path

### Translate

Each request is normalized into one ACP `session/prompt`: instructions,
multi-turn history, images and the client's tool definitions. OpenAI
`instructions` / `developer` / `system` items and Anthropic `system` prompts go
into the prompt as `<developer_instructions>` blocks rather than ACP's
system-prompt extension, which avoids upstream `429`s on large harness prompts
and keeps session hashes stable. Billing headers (`cch=...`) are stripped from prompts, while volatile
reminders (timestamps, token budgets, context-window counters) are preserved for
the model and normalized only during session hashing. Tool outputs scrub ANSI
escape sequences and carriage-return progress-bar overwrites on the wire while
persisting raw unmodified bytes on spill, and schema-aware argument coercion
unwraps JSON-stringified `array` / `object` tool fields and `null` envelopes.
Multimodal images inside both user turns and tool outputs (`view_image`, `Read`)
become native ACP image blocks on fresh or `<150k`-token sessions, and
automatically offload to a zero-history ephemeral ACP sub-session on warm
`>=150k`-token coding sessions so upstream prompt caches are never invalidated
by mid-session screenshots.

### Inference-only sessions

Each ACP session runs in its own `agy acp --process-per-session --include-default-customizations=false`
process, inside an isolated empty directory, with `mcpServers: []`, non-interactive
git/updater environment isolation (`GIT_TERMINAL_PROMPT=0`, `GIT_PAGER=cat`,
`AGY_FAST_AUTO_UPDATE=off`, `NO_UPDATE_NOTIFIER=1`), and explicit bridge rules
prohibiting internal host or plugin MCP tools (`call_mcp_tool`, `mcp_*`). `agy`
therefore has nothing to edit or execute: it only reasons. Tool calls go back to the
client, so Codex or Claude Code keeps running file edits and shell commands
under its own permissions. Liftgate itself serves `web_search` and an
SSRF-safe `web_fetch` that refuses private, loopback and link-local addresses.

### Session reuse

HTTP clients resend the whole conversation every turn. Liftgate hashes the
normalized message prefix and looks it up in `SessionPool`, a bounded LRU of
warm ACP sessions (`--max-idle-sessions`, default 64, with KV-cache warmth
telemetry across `<60s` hot / `1–5m` warm / `>=5m` cold tiers and a 45-minute
idle worker reaper). On a hit it sends only
the new turn or tool results (plus an incremental `<client_tools_update>` if
tools changed, or `session/set_model` if effort/model changed). Local `/compact`
summarization turns match the warm coding session via a dialogue hash and retire
it cleanly afterward.

Liftgate sends exactly one ACP prompt per client step, with a `176,000`-byte
text budget below Antigravity's `191,579`-byte single-turn `session/prompt`
truncation boundary and zero synthetic replay turns. Whenever any payload
(tool output, pasted log, tool call arguments, developer/system instructions,
client tool schemas, cold-history turns, or `web_fetch` page) requires
truncation or omission, `SpillStore` atomically writes 100% of the lossless
original content to a content-addressed file under `~/.liftgate/spills/`
before applying water-filling Head+Tail line truncation (~55% head, ~45% tail,
plus inline extraction of buried compiler/test failure diagnostics from omitted
middle sections on tool outputs)
and embeds the exact local file path and omitted line range in the notice,
while leaving smaller sibling tool results untouched and keeping the canonical
`SessionPool` prefix hash intact for subsequent warm turns.

When a genuine context-window overflow occurs, Liftgate returns the
standard reactive-compaction wire format expected by each client (`code:
"context_length_exceeded"` / `param: "input"` on OpenAI `/v1/responses` and
`prompt is too long: N tokens > M maximum` on Anthropic `/v1/messages`, across
both HTTP `400` and SSE failure frames).

Model metadata and generated client settings use empirically verified context
and auto-compact limits: Gemini `500k / 425k`, Claude `200k / 170k`, and GPT-OSS
`128k / 110k`. Codex uses per-model catalog entries, and Claude Code uses
per-model `autoCompactWindow` settings. Canonical Claude aliases shared by
several gateway models take the smallest limit. Upstream `agy acp` enforces an
internal post-turn auto-compaction cliff at `524,288` tokens (`50%` of the
`1,048,576`-token `usage_update` window) on Gemini sessions; keeping the
client window at `500k / 425k` triggers client `/compact` cleanly on the warm
session before upstream compaction wipes context. If `UsageUpdate` still
detects post-turn upstream compaction, Liftgate delivers the current response
and retires the session afterward. See [context configuration](usage.md#context-windows).

### Streaming

ACP `session/update` notifications (`agent_thought_chunk`,
`agent_message_chunk`, tool calls) become OpenAI Responses or Anthropic
Messages SSE events frame by frame. `agy` retry banners are filtered out of the
text.

### Model routing

- **Transparent rate-limit errors (no silent downgrade).** When `agy` reports
  an upstream `429 RESOURCE_EXHAUSTED` or quota error, Liftgate aborts `agy`'s
  retry wait at once and returns HTTP `429` (`rate_limit_error`) / SSE failure
  directly on the exact model you requested, never silently switching models.
- **Background requests.** Titles and topics use `gemini-4-hydrogen-low` by default.
  In `lg tui`, press `3` for Models, highlight a model, and press `t` to use it
  for subsequent titles. Classifiers, web search sub-requests, and away recaps
  use the fast model selector, which skips Gemini 3.8 Flash. All run in short-lived sessions.
- **Defaults.** An unknown or generic model name maps to `--default-model`.

## Process supervision

Liftgate owns every `agy` child it starts. On Windows they run in a Job Object
with `KILL_ON_JOB_CLOSE`; on Unix in their own process group, killed with
`killpg`, plus `PR_SET_PDEATHSIG` on Linux. A panic hook cleans up too, so
closing or crashing the gateway never leaves `agy` processes behind.

## Workspace

| Path | Owns |
| --- | --- |
| `crates/liftgate-types` | protocol, wire and config types: no async runtime, HTTP or I/O |
| `crates/liftgate-tool-virtualizer` | zero-cost, trait-driven (`HarnessToolProfile`) 3-tier MCP tool virtualizer, `CodexToolProfile`, `ClaudeCodeToolProfile`, `OpenCodeToolProfile` & `OmpToolProfile` adapters, slack-slot backfill, 0-RTT search index, and preflight validator: no async runtime, HTTP or I/O |
| `crates/liftgate-harness` | synchronous per-harness client adapters (`claude`, `codex`, `opencode`, `omp`), structure-preserving YAML/TOML/JSON surgical editors, displaced-config ledger, CLI flag/session-alias normalizers, and cross-platform executable & Windows shim discovery: no async runtime, HTTP or server dependencies |
| `crates/liftgate-server` | HTTP/SSE routes, ACP transport, `SessionPool`, process supervision, `web_fetch`, telemetry |
| `crates/liftgate-core` | shared control-plane library: daemon lifecycle, embedded `web/dist` assets, HTTP/SSE client, `agy` & proxy manager, zero-side-effect CLI/GUI launchers, system terminal spawner, non-destructive `config` sync, and verified self-upgrade |
| `crates/liftgate` | the `liftgate` / `lg` CLI and interactive `ratatui` TUI binaries |
| `web/` | the dashboard (Bun, Vite, React 19, Tailwind CSS v4, shadcn/ui), embedded into release builds via `crates/liftgate-core/build.rs` |
| `run-gateway.{sh,ps1,cmd}` | build and run the gateway |

Dependencies point one way: `liftgate` → `liftgate-core` → `{liftgate-harness, liftgate-server}` → `liftgate-tool-virtualizer` → `liftgate-types` (`liftgate-harness` depends directly on `liftgate-types` and compiles in parallel with `liftgate-server`).
