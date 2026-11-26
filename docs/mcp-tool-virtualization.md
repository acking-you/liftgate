# Adaptive MCP Tool Virtualization for `agy acp`

This document specifies how Liftgate virtualizes large caller tool catalogs onto the loopback MCP server (`lg`) so Antigravity ACP (`agy acp`) never evicts high-value coding tools (`exec`, `exec_command`, `Bash`, `spawn_agent`) while cutting cold-start schema overhead by ~85%+.

## 1. Background & Empirical `agy acp` Constraints

Liftgate bridges caller-declared tools (from Codex App, Codex CLI, Claude Code, OpenCode, Oh My Pi (`omp`), and API clients) into `agy acp` over an authenticated loopback Streamable HTTP MCP server (`http://127.0.0.1:<port>/mcp/<token>`, server name `lg`), suspending and resuming in-flight `session/prompt` turns in-place (`mcp-cont`) with ~90%–96% KV-cache retention.

Live production telemetry on 128-tool Codex App sessions (`exec` + `wait` + `exec_command` + `node_repl` + `codex_tui` + `multi_agent_v1` + 79 GitHub connector tools = `183.7 KB` of tool definitions) exposed three compounding bottlenecks when `lg` naively exposed up to 64 tools:

1. **Upstream `50`-Tool Eager Ceiling & Size-Sorted Eviction**:
   `agy acp` enforces a hard budget of **50 Eager MCP tools** across *all* configured MCP servers (host plugins + `lg`). When total MCP tools exceed 50, `agy acp` packs tools **ascending by serialized tool size (`schema_bytes`)** and demotes the largest tools out of the Eager native declaration set:
   - In a 128-tool session (`1` host plugin tool + `64` `lg` tools = `65` MCP tools), the 49 smallest `lg` tools (`schema_bytes <= 1,663 B`, `48.6 KB` total) stayed Eager while all **15 tools `>= 1,669 B`** were evicted — including `exec` (`3,663 B`), `exec_command` (`3,202 B`), `write_stdin` (`1,859 B`), `multi_agent_v1.send_input` (`2,756 B`), and `multi_agent_v1.spawn_agent` (`9,296 B`).
   - Because `<api_gateway_bridge_rules>` forbids host-internal `call_mcp_tool` for isolation and Liftgate assumed all 64 MCP tools were already mounted natively, those 15 evicted tools became completely unreachable to the model.
2. **Code Mode Child Footer Inflation**:
   When Codex runs with `features.code_mode_host=true`, it appends an `exec tool declaration:\n```ts\ndeclare const tools: ...` block to nearly every nested tool description. Matching `desc.contains("declare const tools")` blindly promoted 115+ ordinary tools to `tier = 0` orchestrator status and bypassed standard description compaction, doubling `lg` wire schema size to `94.4 KB`.
3. **Overflow XML Token Churn**:
   Spilling tools `#65..#128` into `<client_tools>` and `<client_tools_update>` injected `~21.4 KB` (~5k+ tokens) of redundant schema prose on cold sessions and mid-session connector updates.

## 2. Design Goals

- **Zero Eager Eviction**: Strictly bound `lg` `tools/list` to **`<= 24` tools** (leaving 26+ Eager slots for host MCP plugins) and compact wire descriptions so `exec`, `exec_command`, and every mounted `lg` tool remain **100% Eager** in `agy acp`.
- **Zero Overhead on Small Sessions**: Sessions with `<= 20` eligible tools and `<= 22 KB` wire schema bypass virtualization completely and expose every tool 1:1.
- **100% Client Protocol Transparency**: Callers still send flat `tools` arrays on `/v1/responses` or `/v1/messages` and still receive standard `function_call` / `custom_tool_call` / `tool_use` items with exact original tool names (`mcp__codex_apps__github._fetch_pr`, `exec`, etc.).
- **Zero Client Round-Trips for Tool Discovery**: Resolve `search_client_tools` and schema preflight errors **in-memory inside Liftgate's loopback MCP driver (`< 0.1 ms`)** without ending or suspending the caller's HTTP stream.

## 3. Three-Tier Adaptive Tool Virtualization Architecture

When a session's eligible MCP tools exceed the direct passthrough budget (`> 20` tools or `> 22 KB` wire bytes), `partition_tools_for_mcp` partitions tools into three native MCP layers plus freeform XML coexistence:

```text
Caller Tool Catalog (e.g. 128 tools, 183.7 KB)
  │
  ├── Layer 1: Direct Native MCP Tools (1:1, never collapsed, <= 12 core coding tools)
  │     • Code Mode & REPL orchestrators: exec ({code}), wait, REPL
  │     • Core workspace primitives: exec_command, write_stdin, Bash, Read, Edit, Write,
  │       MultiEdit, Glob, Grep, LS, NotebookEdit, view_image, Agent, Task, TodoWrite,
  │       WebSearch, WebFetch, ToolSearch / tool_search
  │     • Freeform non-exec grammar tools (e.g. apply_patch) remain raw XML blocks so
  │       multi-hunk patches never suffer JSON string-escaping corruption.
  │
  ├── Layer 2: Namespace Action Hubs (grouped by connector/subsystem prefix)
  │     • mcp_lg_ns__github          (79 actions -> ~550 B single MCP tool)
  │     • mcp_lg_ns__codex_tui       (12 actions -> ~550 B with action enum)
  │     • mcp_lg_ns__multi_agent_v1  (5 actions, absorbing 9.3 KB spawn_agent -> ~500 B)
  │     • mcp_lg_ns__node_repl / cua_repl / mcp_resources / user_input / clock / goals ...
  │     • mcp_lg_call_client_tool    (universal dispatcher for standalone & mid-session tools)
  │
  └── Layer 3: Gateway-Local 0-RTT Discovery Tool
        • mcp_lg_search_client_tools (in-memory lexical + exact-name schema & TS lookup)
```

### Zero-Cost `HarnessToolProfile` Trait & Static Dispatch (`crates/liftgate-tool-virtualizer`)

All virtualization planning, harness adaptation, 0-RTT lexical search, and schema preflight validation live in the standalone, zero-I/O crate **`crates/liftgate-tool-virtualizer`** (`liftgate_tool_virtualizer` → `liftgate_types`, zero `tokio`/`axum` dependencies). Each coding harness is modeled as a zero-sized unit struct (`size_of::<P>() == 0`) implementing `HarnessToolProfile: Copy + Default + Send + Sync + 'static`, monomorphized directly into `plan_virtualization<P, S>`:

- **`CodexToolProfile` (`"codex"`)**: Prioritizes Code Mode (`exec` + `wait`) at Tier 0 and PTY/workspace primitives (`exec_command`, `write_stdin`, `local_shell`/`shell`, `view_image`, `update_plan`, `tool_search`) at Tier 1; strips `mcp__codex_apps__` cleanly (`ns__github`, `ns__linear`); unifies dotted (`multi_agent_v1.*`, `clock.*`) and top-level un-namespaced Codex families (`spawn_agent`..`close_agent`, `request_user_input*`, `get_goal*`, `js_repl*`); and dehydrates trailing `exec tool declaration:` TypeScript footers out of wire `tools/list` while preserving them in 0-RTT search.
  - Freeform `apply_patch` (`CanonicalToolKind::Custom`) is bridged natively at Tier 1 (`mcp_lg_apply_patch` with `custom_unwrap_key: Some("patch")`), eliminating `<client_tools>` XML split-brain and JS template-literal escaping hazards inside `exec`, while auto-coalescing duplicate `*** Update File:` targets, repairing stripped Lark blank context lines (`""` -> `" "`), translating unified git diffs, and validating hunk syntax in 0-RTT preflight.
- **`ClaudeCodeToolProfile` (`"claude-code"`)**: Prioritizes `REPL` (Tier 0) and `Bash`, `Read`, `Edit`, `Write`, `MultiEdit`, `Grep`, `Glob`, `LS`, `Agent`/`Task`, `LSP`, `NotebookEdit`, `ToolSearch` (Tier 1); groups underscore-free PascalCase built-in families into inline-`enum` hubs (`ns__tasks`, `ns__workflow`, `ns__teams`, `ns__schedule`, `ns__mcp_resources`); deduplicates repeated plugin server slugs (`mcp__plugin_playwright_playwright__*` → `ns__playwright`) and `mcp__claude_ai_*` cloud connectors (`ns__linear`); and ranks `AskUserQuestion`, `Skill`, `SlashCommand`, and `Config` first for **Slack-Slot Backfill** into 1:1 Direct Eager slots.
- **`OpenCodeToolProfile` (`"opencode"`)**: Prioritizes `@opencode/codemode` / `@opencode-ai/codemode` `execute` at Tier 0 (emitting `McpOrchestratorContract::OpenCodeExecute` for both v2 `full_codemode` `FullInput` with `Bun.file`/`tools.*` and partial/v1 MCP catalog mode while preserving `36 KiB` inline catalog descriptions) and lowercase core coding primitives (`shell`/`bash`, `read`, `edit`/`multiedit`/`patch`/`apply_patch`, `write`, `grep`/`glob`/`ls`/`list`, `subagent`/`task`/`batch`, `lsp`/`codesearch`) at Tier 1; groups built-in OpenCode families (`ns__session`, `ns__opencode`, `ns__automation`, `ns__machine`, `ns__todo`) and `<server>_<tool>` MCP registrations (`ns__playwright`, `ns__github`) at Tier 2; prioritizes `question`, `skill`, `websearch`, and `webfetch` for **Slack-Slot Backfill**; and applies 0-RTT Lark V4A preflight self-healing + duplicate-file coalescing to OpenCode's `patch` (`{ "patchText": "..." }`).
- **`OmpToolProfile` (`"omp"`)**: Tailored to Oh My Pi (`@oh-my-pi/pi-coding-agent`) with `VirtualizerBudget::OMP_AGY_ACP` (`<= 21` Direct Eager passthrough / `30 KiB` non-orchestrator / `64 KiB` orchestrator budget); pins polyglot `eval` (`py` Python + `js` Bun persistent kernels with `<prelude>` / `<browser>` / `<computer>` / `<dsm>` docs and `mcp.evalNamespaces`) and `wait` at Tier 0; when `eval` Code Mode is active, elides subordinate built-in tools (`read`, `bash`, `grep`, `glob`, `find`, `lsp`, `todo`, `web_search`, etc.) into `mcp_lg_eval` (`ToolPlacement::FoldedIntoOrchestrator`, appending dehydrated `<eval_session_tools>` `tool.<name>(args)` signatures onto `mcp_lg_eval`) while keeping direct mutation and interactive tools (`edit`/`apply_patch`/`ast_edit`, `write`, `task`/`yield`/`resolve`/`reject`/`propose`, `ask`) mounted top-level (or passing all built-ins through 1:1 under `--no-code-mode`); folds single-underscore `mcp__<server>_<tool>` (including two-token server names like `google_drive`), `memory` (`retain`..`learn`), `session_state` (`checkpoint`..`new_context`), and `irc_*` into `ns__*` hubs; and leaves `omp`'s `hashline` `edit` (`[path#TAG]` + `PUT`) untouched while V4A-healing `apply_patch` (`input`).
- **`GenericToolProfile` (`"generic"`)**: Fallback profile supporting `mcp__<server>__<tool>`, `mcp__<server>.<tool>`, repeated single-underscore `mcp__<server>_<tool>` servers, dotted namespaces, and case-insensitive core coding primitives.
- **Closed-set static dispatch & warm rebuild**: `DetectedHarness` (`Codex` | `ClaudeCode` | `OpenCode` | `Omp` | `Generic`) dispatches exhaustively without `dyn` trait objects, while `McpRoutingTable` stores a monomorphized `fn` pointer (`build_routing_table_for_profile::<P>`) so dynamic warm-turn tool updates re-index under the exact same zero-cost profile `P`.

### End-to-End Execution Flow

```text
Gemini (agy acp)                 Liftgate (Loopback MCP + Driver)              Codex / Claude Code
       │                                        │                                       │
       │ 1. POST /mcp/{tok}                     │                                       │
       │    search_client_tools(ns="github",    │                                       │
       │                        query="review") │                                       │
       │───────────────────────────────────────►│                                       │
       │                                        │── Queries in-memory routing snapshot  │ (0 client round-trips;
       │ 2. Exact JSON inputSchema + TS sig     │   (<0.1ms), replies on reply_tx,      │  HTTP SSE stream stays open)
       │◄───────────────────────────────────────│   continues 'substep_loop             │
       │                                        │                                       │
       │ 3. POST /mcp/{tok}                     │                                       │
       │    ns__github(action="_add_review_to_pr", args={...})                          │
       │───────────────────────────────────────►│                                       │
       │                                        │── Preflight checks required fields,   │
       │        (parked in mcp-wait)            │   unwraps to canonical client tool ──►│ 4. SSE: function_call
       │                                        │                                       │    name="mcp__codex_apps__github._add_review_to_pr"
       │                                        │◄──────────────────────────────────────│ 5. Next request (mcp-cont)
       │ 6. oneshot delivers CallToolResult     │                                       │    function_call_output
       │◄───────────────────────────────────────│                                       │
```

## 4. Component Specifications

### 4.1 Code Mode Orchestrator Detection & TS Footer Splitting (`liftgate-types::session_key`)

- `split_code_mode_ts_declaration(raw_desc)` separates a tool's primary prose from any trailing `\nexec tool declaration:\n```ts...` block injected by Codex `code_mode_host`.
- `is_code_mode_orchestrator_tool` inspects **only** primary prose (plus `exec` / `*.exec` / `REPL` tool names), preventing ordinary nested tools from being misclassified as `tier = 0` orchestrators.
- Non-orchestrator MCP `wire_tools` descriptions strip the duplicate trailing TypeScript block (since `wire_tools` already carry structured JSON `inputSchema`), while the extracted TypeScript signature is retained in Liftgate's in-memory search index so `search_client_tools` can return it when the model writes multi-tool Code Mode `exec` scripts.

### 4.2 Deterministic Namespace Clustering & Hub Schemas (`liftgate-server::mcp_bridge`)

Non-core `Function` tools are grouped deterministically:
1. `mcp__codex_apps__<connector>.<action>` / `mcp__claude_ai_<connector>__<action>` / `mcp__<server>__<action>` / `mcp__<server>.<action>` → namespace `<connector>` / `<server>`, short action `<action>`.
2. Dot-namespaced subsystems `<subsystem>.<action>` → namespace `<subsystem>`, short action `<action>`.
3. Cohesive built-in families (`*_mcp_resource*` → `mcp_resources`, `request_user_input*` / `send_message_to_user*` → `user_input`, `*_goal` → `goals`) and shared `<prefix>_*` families (`>= 3` tools).
4. **Small vs Large Hub Wire Schema**:
   - Small families (`<= 12` actions) embed `"enum": [...]` on `action` so self-explanatory calls execute in a single step without prior search.
   - Large families (`> 12` actions, e.g. `github` with 79 actions) omit `enum`, summarize action categories, and direct unfamiliar calls through `search_client_tools`.
   - If distinct namespaces exceed remaining MCP slots (`MAX_NATIVE_MCP_TOOLS = 24` minus core tools, `search_client_tools`, and `call_client_tool`), lower-count long-tail namespaces fold cleanly into `call_client_tool`.

### 4.3 Gateway-Local `search_client_tools` & Mixed Parallel Batches

- `search_client_tools` supports `query` (multi-token lexical ranking across canonical name, action, namespace, parameter names, and description), `namespace` filtering, `names` exact schema lookup, and `limit` (`1..=20`, default `8`).
- Inside `run_mcp_session_driver`'s `'substep_loop`:
  - **Pure local step** (only `search_client_tools` / preflight / disabled rejections): Liftgate answers every `oneshot::Sender<McpToolCallReply>` immediately and executes `continue 'substep_loop` with zero client SSE output.
  - **Mixed parallel batch** (e.g. `[search_client_tools(...), exec_command(...)]` in one model step): `search_client_tools` resolves its `reply_tx` immediately while `exec_command` suspends normally for client execution; when `mcp-cont` resumes `exec_command`, `agy acp` receives both HTTP responses and advances the turn cleanly.

### 4.4 Hub Unwrapping & Schema Preflight Self-Healing

When resolving `ns__<namespace>` or `call_client_tool`:
- Accepts flexible argument layouts (`{"action": "...", "args": {...}}`, `"arguments"` / `"params"` aliases, stringified JSON objects, or flattened top-level fields) and both short `action` names (`_fetch_pr` / `fetch_pr`) and full canonical tool names.
- **Unknown action self-healing**: Returns `is_error: true` locally inside `'substep_loop` with the closest available action names in that namespace instead of forwarding an invalid call to the client.
- **Required-parameter preflight**: Applies `coerce_stringified_container_fields` and checks top-level `required` keys against the target tool's JSON Schema (when the schema has no root-level `oneOf`/`anyOf` polymorphism). Missing required keys return `is_error: true` locally together with the target tool's exact `inputSchema`, allowing Gemini to self-correct within the same turn (bounded by `MAX_CONSECUTIVE_PREFLIGHT_REJECTIONS` to prevent runaway loops).

### 4.5 Warm Session Dynamic Tool Sync (`RwLock` Routing Table)

- `McpSessionHandle` separates immutable `wire_tools` (`session/new`) from a live `RwLock<Arc<McpRoutingTable>>` updated at the start of every warm or resumed turn (`sync_client_tools`).
- Mid-session tool additions (such as lazy connector activation or Plan ↔ Default mode transitions) become immediately searchable via `search_client_tools` and invokable via `call_client_tool` / existing `ns__*` hubs, while `<client_tools_update>` emits only a compact name delta instead of multi-kilobyte JSON schemas.

## 5. Impact Summary (128-Tool Codex App Benchmark)

| Metric | Before (`v0.2.7`) | After (Adaptive Virtualization) |
| --- | ---: | ---: |
| Client-declared tools | `128` (`183.7 KB`) | `128` (`183.7 KB`) |
| `lg` `tools/list` count | `64` (exceeded `agy` 50-tool Eager limit) | `16` (`34` Eager slots of headroom) |
| `lg` `tools/list` wire bytes | `94.4 KB` | `~12.0 KB` (`-87%`) |
| Cold XML overflow (`<client_tools>` + spill) | `64` tools (`~21.4 KB`) | **`0` tools (`0 KB`, `-100%` — 100% pure native MCP)** |
| `exec` / `exec_command` / `spawn_agent` availability | Evicted by `agy` size sort | **100% Eager / Natively Reachable** |
