# Usage guide

English · [中文](usage.zh-CN.md)

Run Liftgate, then point Codex CLI, Claude Code or any OpenAI / Anthropic
client at it. Every address below assumes the default
`http://127.0.0.1:8787`.

## Before you start

- **Liftgate (`lg` / `liftgate`)**, installed in one command:
  ```bash
  # macOS / Linux / FreeBSD
  curl -fsSL https://raw.githubusercontent.com/acking-you/liftgate/main/scripts/install.sh | sh
  ```
  ```powershell
  # Windows (PowerShell)
  irm https://raw.githubusercontent.com/acking-you/liftgate/main/scripts/install.ps1 | iex
  ```
  You can also install via `cargo binstall liftgate`, download an archive from
  [GitHub Releases](https://github.com/acking-you/liftgate/releases/latest),
  or build from source with **Rust 1.100.0**:
  ```bash
  cargo install --path crates/liftgate --locked
  ```
  When **Bun** is installed at build time, `build.rs` compiles `web/` and embeds
  the production React dashboard directly into the single `liftgate` binary (no
  external `web/dist` folder needed at runtime). Without Bun, every API still
  works and `/` serves a lightweight built-in SSE telemetry page.
- **Antigravity CLI (`agy`)**: if `agy` is not on your machine yet, run
  `lg agy install` to download and SHA-512-verify the official platform binary,
  then run `lg agy login` once to sign in (`agy acp --login`). Liftgate finds
  `agy` on `PATH`, in standard install locations, or in `~/.liftgate/settings.json`
  (override with `--agy-bin <path>` or `AGY_BIN`).

## Manage the gateway service (`lg` / `liftgate` CLI)

Every archive and `cargo install` ships both `liftgate` and its short alias
`lg` (you can also install custom aliases via `liftgate alias --name <cmd>`).
Manage the background gateway, live TUI, and embedded Web UI from any directory
(state and logs live in `~/.liftgate`, configurable via `--state-dir` or
`LIFTGATE_HOME`):

```bash
lg                              # open interactive command center (on a TTY)
lg agy                          # inspect agy binary, version, login status, proxy & auto-update
lg agy install                  # download, SHA-512-verify & install official agy CLI
lg agy login                    # interactive Google login via official `agy acp --login`
lg proxy 7890                   # force outbound proxy into agy (`lg proxy off` restores direct default)
lg top                          # full-screen live TUI dashboard (aliases: tui, dash, monitor)
lg up                           # start background gateway + embedded Web UI (full: start)
lg up --open                    # start and open http://127.0.0.1:8787/ in your browser
lg st                           # live health, PID, session pool, tokens & active turns (full: status, --json)
lg l -n 100 -f                  # tail & follow ~/.liftgate/gateway.log (full: logs, --acp includes agy stderr)
lg web                          # open the browser dashboard (full: ui, --print prints URL)
lg m -i                         # interactively filter & switch default model (full: models --pick)
lg doc                          # check agy, claude, codex, proxy, embedded UI & auth readiness (full: doctor)
lg r                            # restart preserving (or overriding) settings (full: restart)
lg down                         # gracefully stop the background service and ACP process tree (full: stop)
lg upgrade                      # check GitHub Releases (`-c` / `--check`) and self-upgrade `lg` + `liftgate` (aliases: update, self-update)
lg alias --name lg              # install or remove (`--uninstall`) short executable alias on PATH
lg completions zsh              # generate shell completions (bash, zsh, fish, powershell, elvish)
liftgate serve                  # run in the foreground
```

In `lg tui`, press `3` to open Models, select a model, and press `t` to save it for future titles. The default is `gemini-4-hydrogen-low`.

## Antigravity CLI (`lg agy`) & Proxy Management (`lg proxy`)

Liftgate manages the official `agy` CLI lifecycle, outbound proxy injection,
and background idle updates directly from `lg`:

```bash
# 1. Inspect agy presence, version, upstream release, login state, proxy & auto-update
lg agy                                  # or `lg agy --json`, `lg agy status --offline`

# 2. Install or manually update agy across Windows, macOS, Linux (glibc/musl) & Android
lg agy install                          # SHA-512 verified from official Antigravity manifest
lg agy update                           # check & update agy now (restarts running daemon)

# 3. Configure forced proxy for agy (DEFAULT: no proxy / direct connection)
lg proxy                                # show current proxy policy (alias: `lg px`)
lg proxy 7890                           # shorthand -> http://127.0.0.1:7890
lg proxy socks5h://127.0.0.1:1080       # HTTP, HTTPS, SOCKS5 and SOCKS5H supported
lg proxy system                         # import OS / environment proxy once
lg proxy test                           # test connectivity to official Antigravity manifest
lg proxy off                            # clear forced proxy (back to default direct mode)

# 4. Toggle background idle auto-update of agy (DEFAULT: enabled)
lg agy auto-update                      # show current policy & last auto-update telemetry
lg agy auto-update off                  # disable background idle auto-update
lg agy auto-update on                   # re-enable background idle auto-update

# 5. Delegate official agy operations (with forced proxy applied automatically)
lg agy login                            # runs `agy acp --login` (--install installs agy first if missing)
lg agy models                           # runs `agy models`
lg agy mcp -- --help                    # forwards to `agy mcp ...` (any subcommand works)
```

- **Default direct mode**: out of the box, Liftgate strips ambient
  `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` variables and does **not** inject
  any OS proxy into `agy`. Only when you configure a proxy via `lg proxy`
  (persisted in `~/.liftgate/settings.json`) or `--agy-proxy` / `AGY_PROXY`
  does Liftgate forcibly inject `HTTP_PROXY`, `HTTPS_PROXY`, and `ALL_PROXY`
  (with loopback `127.0.0.1,localhost,::1` kept in `NO_PROXY`).
- **Safe idle auto-update**: while the gateway is running and
  `agy_auto_update` is enabled (`true` by default), Liftgate periodically
  checks the official Antigravity manifest. When a newer version is available,
  it waits until **zero** HTTP turns or ACP streams are active, drains pooled
  idle sessions, verifies the SHA-512 digest, hot-swaps the `agy` binary
  (rolling back automatically if the new binary fails ACP handshake), and
  resumes serving without dropping configuration.

## Zero-side-effect launch (`lg cx` / `lg cc` / `lg cxa` / `lg cd`)

Run Codex CLI, Claude Code, **Codex App**, or **Claude Desktop** across Windows,
macOS, and Linux without polluting project files, `~/.claude/settings.json`,
`~/.codex/config.toml`, native desktop profiles, or your caller shell:

```bash
cd /path/to/your/project
liftgate codex                              # interactive Codex CLI with Code Mode (`exec` + `wait`) enabled by default
liftgate codex --code-mode-only             # strict Code Mode (`exec` + `wait` JavaScript orchestration only)
liftgate codex --no-code-mode               # disable Code Mode (`tool_mode = "direct"`) for this session
liftgate claude                             # interactive Claude Code (`CLAUDE_CODE_EXPERIMENTAL_CODE_MODE=1` by default)
liftgate codex exec "run cargo test"        # native flags & subcommands forward directly
liftgate claude --model gemini-4-hydrogen-low -p "summarize recent commits"

# Native session browsing, resumption, forking, worktrees & background agents
lg cx resume                                # interactive Codex session picker (`--all` across projects; alias: `lg cx -r`)
lg cx resume --last -m gemini-3.8-pro-high  # continue latest session & switch model (Liftgate flags work in any position)
lg cx fork --last                           # fork most recent Codex session into a new thread
lg cx agents                                # open Codex Agent Command Center (`archive`, `unarchive`, `delete`, `queue` also work)
lg cx review --uncommitted                  # run a non-interactive Codex code review via Liftgate
lg cc -r                                    # interactive Claude session picker (alias: `lg cc resume`; `Ctrl+A` shows all projects)
lg cc -c                                    # continue most recent Claude session (aliases: `lg cc continue`, `lg cc resume --last`)
lg cc fork --last                           # fork most recent Claude session (`--continue --fork-session`)
lg cc -w feat-auth                          # start Claude Code in an isolated git worktree
lg cc --bg "refactor parser"                # background session (`lg cc agents`, `attach`, `logs`, `stop`, `rm` all work)

# Desktop GUI apps (Windows MSIX/Squirrel/standalone, macOS .app bundles, Linux .deb/Flatpak/AppImage)
lg codex-app                                # alias: `lg cxa` or `lg cx --app` (isolated CODEX_HOME + --user-data-dir)
lg codex-app --ephemeral                    # use a disposable Electron profile deleted when the window closes
lg claude-desktop                           # alias: `lg cd` or `lg cc --desktop` (scoped 3P profile, auto-restored on exit)
lg claude-desktop --restart-app             # gracefully restart a running Claude Desktop instance into Liftgate
lg claude-desktop --restore                 # manually replay rollback journals if needed
```

- If the background gateway is not running yet, `liftgate codex` and
  `liftgate claude` (and their desktop counterparts) start it automatically.
- Scoped environment variables and temporary settings/catalog files (under
  `~/.liftgate/ephemeral/`) apply **only** to the spawned child process and are
  deleted immediately when the CLI exits.
- **Codex App (`lg cxa`)** synthesizes a physical session `CODEX_HOME` (merging
  `~/.codex/config.toml` read-only with `cli_auth_credentials_store = "file"`,
  `forced_login_method = "api"`, and a dedicated API-key `auth.json` so ChatGPT
  OAuth tokens never leak, without ever modifying `~/.codex`) and passes
  `--user-data-dir` under `~/.liftgate/desktop/codex-app-isolated` (activating
  MSIX packages on Windows via headless in-container `Invoke-CommandInDesktopPackage`
  so `CODEX_HOME` survives the COM boundary while preserving `HOME` /
  `USERPROFILE` / `APPDATA` for child `git`, `cargo`, and `ssh` shells).
- **Claude Desktop (`lg cd`)** snapshots existing `Claude` / `Claude-3p` state
  into an atomic rollback journal (`~/.liftgate/desktop-journals/`), activates a
  scoped Third-Party Inference (`deploymentMode: "3p"`) profile pointed at
  `/claude-desktop` (including Windows MSIX `LocalCache` and Linux Flatpak
  paths), watches the GUI process in the background, and surgically restores the
  original `deploymentMode` and profile library when Claude Desktop closes
  (preserving any `mcpServers` or preferences edited during the session).
- By default both launch in autonomous mode (`--dangerously-bypass-approvals-and-sandbox`
  for Codex, `--dangerously-skip-permissions` for Claude Code); pass
  `--no-bypass` (or explicit permission/sandbox flags) to keep standard prompts.
- Native subcommands and flags work without needing `--`: local inspection/utility
  commands (`lg cx queue|archive|delete|mcp|plugin|features|-V`, `lg cx resume --help`,
  `lg cc logs|attach|stop|rm|mcp|plugin|purge|-v`, `lg cc agents --json`) run
  directly without booting the gateway, while Liftgate flags (`-m`, `--no-bypass`,
  `--no-code-mode`, `--code-mode-only`, etc.) can appear before or after native
  arguments. Pass `--` (`lg cx -- --help`, `lg cc -- --help`) whenever you want
  verbatim raw argument forwarding.

## Context windows

Use `lg cx` or `lg cc` to load the running gateway's context policy. Liftgate
uses warm ACP sessions, single-turn `session/prompt` calls, and client-side
compaction with zero background replay turns:

| Model family | Declared window | Auto-compact threshold |
| --- | --- | --- |
| Gemini | 500,000 | 425,000 |
| Claude | 200,000 | 170,000 |
| GPT-OSS | 128,000 | 110,000 |

Upstream `agy acp` enforces an internal post-turn auto-compaction threshold at
`524,288` tokens (`50%` of the `1,048,576`-token `usage_update` window) on
Gemini sessions; advertising `500,000 / 425,000` leaves safe output headroom so
client `/compact` runs on the warm session before upstream compaction fires.
Codex reads the policy per model from its catalog. Claude Code uses
`modelSettings.autoCompactWindow` (requires v2.1.288 or later); when several
gateway models share a canonical Claude name, the smaller threshold wins.
That can compact sooner than the table. See the official
[Claude context settings](https://code.claude.com/docs/en/model-config#context-window-and-auto-compaction).

Every ACP `session/prompt` has a `176,000`-byte text budget (safely below
Antigravity's `191,579`-byte single-turn truncation boundary). When an active turn or tool
result exceeds that single-turn budget, Liftgate automatically middle-truncates
only the oversized block(s) on clean line boundaries (preserving ~55% of the
beginning and ~45% of the end), atomically saves 100% of the lossless original
payload to `~/.liftgate/spills/`, and cites the exact local file path and
omitted line range in the marker so client agents can read any needed middle
lines on demand—while keeping smaller sibling tool results untouched and
preserving warm `SessionPool` reuse on subsequent turns. Cold replays, compacted
tool catalogs, and oversized `web_fetch` pages follow the same lossless spill
policy, and genuine context-window overflows emit standard
auto-compactable errors (`context_length_exceeded` for Codex and `prompt is too
long: N tokens > M maximum` for Claude Code). Client `/compact` reuses a
matching warm session before retiring it.

## Configure local or global client files (`liftgate config`)

If you prefer running bare `claude` or `codex` directly, let `liftgate config`
(alias `liftgate setup`) generate or non-destructively merge the live model
catalog and provider config for you:

```bash
liftgate config                     # write .claude/settings.local.json + .codex/{config.toml,agy-models.json}
liftgate config claude --global     # merge into ~/.claude/settings.json
liftgate config codex --global      # merge [model_providers.agy] + catalog into ~/.codex/
liftgate config --print             # preview generated config without writing files
liftgate config --uninstall         # remove only Liftgate-managed keys (--global for ~/)
```

Existing unrelated user settings (`mcpServers`, `hooks`, `[mcp_servers.*]`,
`[projects.*]`, custom `env` entries) are preserved, and a one-time
`*.liftgate.bak` backup is created before updating an existing file.

## Source-checkout scripts

`run-gateway` builds `web/dist` when Bun is present and the bundle is missing,
builds `liftgate` with `cargo build --release --locked`, then starts it.

```bash
./run-gateway.sh                  # foreground, Ctrl+C to stop
./run-gateway.sh --daemon         # background; PID and logs in .agy-runtime/
./run-gateway.sh --stop           # stop the background gateway
./run-gateway.sh --listen 127.0.0.1:8787 --default-model gemini-4-carbon-high --api-key local-agy
```

```powershell
.\run-gateway.cmd                 # CMD or PowerShell
.\run-gateway.cmd -Daemon
.\run-gateway.cmd -Stop
```

| Script option | Effect |
| --- | --- |
| `--daemon` · `-Daemon` | run in the background and wait for `/healthz` |
| `--stop` · `-Stop` | stop the background gateway |
| `--skip-build` · `-SkipBuild` | start the existing release binary without building |
| `--rebuild-web` · `-RebuildWeb` | rebuild `web/dist` even when it exists |
| `-h` · `--help` | show script help |

Any other argument goes to `liftgate` unchanged ([flags](#flags-and-environment)).
Check it is up with `curl http://127.0.0.1:8787/healthz`, and list your models
with `curl http://127.0.0.1:8787/v1/models`.

## Codex CLI

Prefer `lg cx` for automatic configuration. For manual setup, **1. export the
model catalog** so `/model` has the correct per-model window and compaction policy:

```bash
mkdir -p ~/.codex
curl -fsS "http://127.0.0.1:8787/v1/codex-models?default_model=gemini-4-carbon-high" -o ~/.codex/agy-models.json
```

```powershell
New-Item -ItemType Directory -Force -Path "$HOME\.codex" | Out-Null
Invoke-WebRequest "http://127.0.0.1:8787/v1/codex-models?default_model=gemini-4-carbon-high" -OutFile "$HOME\.codex\agy-models.json"
```

**2. Add the provider** to `~/.codex/config.toml`:

```toml
model = "gemini-4-carbon-high"
model_provider = "agy"
# Absolute path from step 1; on Windows use forward slashes, e.g. "C:/Users/you/.codex/agy-models.json"
model_catalog_json = "/Users/you/.codex/agy-models.json"
web_search = "live"

[model_providers.agy]
name = "Liftgate"
base_url = "http://127.0.0.1:8787/v1"
wire_api = "responses"
# If the gateway runs with --api-key <key>, uncomment and export AGY_GATEWAY_API_KEY=<key>:
# env_key = "AGY_GATEWAY_API_KEY"
```

**3. Run `codex`.** Behind an HTTP proxy, keep loopback off it first:
`export NO_PROXY="127.0.0.1,localhost,::1"`.

Remove old blanket `model_context_window` / `model_auto_compact_token_limit`
overrides when migrating an existing setup, or rerun `lg config codex --global`
to update the managed keys. The new catalog's `max_context_window` also caps
old global overrides in current Codex versions; older clients may not honor
that cap, so clearing stale overrides is still recommended. Keep the exported
catalog synchronized with the gateway's current mode.

**Code Mode (`exec` + `wait`)**: Liftgate supports Codex Code Mode end-to-end
(preserving the full generated `declare const tools` TypeScript declarations
inside `exec`, spilling large MCP catalogs past the 16 KB `systemPrompt.append`
cap without truncation, streaming `response.custom_tool_call_input.delta` with
`call_id`, handling yielded cells via `wait`, and routing recursive Guardian
security reviews to fast Flash ephemerally). Launch with `lg cx --code-mode`
(hybrid) or `lg cx --code-mode-only` (strict), or pass `-c features.code_mode=true`
(and optional `?tool_mode=code_mode` when exporting `/v1/codex-models`).

## Claude Code

Prefer `lg cc` for model-specific settings. This manual example uses a
conservative 110k auto-compact window for every model. Set the variables for one shell:

```bash
export ANTHROPIC_BASE_URL="http://127.0.0.1:8787"
export ANTHROPIC_AUTH_TOKEN="local-agy"
export ANTHROPIC_MODEL="gemini-4-carbon-high"
export ANTHROPIC_SMALL_FAST_MODEL="gemini-4-hydrogen-low"
export CLAUDE_CODE_AUTO_COMPACT_WINDOW="110000"
export NO_PROXY="127.0.0.1,localhost,::1"
claude
```

```powershell
$env:ANTHROPIC_BASE_URL = "http://127.0.0.1:8787"
$env:ANTHROPIC_AUTH_TOKEN = "local-agy"
$env:ANTHROPIC_MODEL = "gemini-4-carbon-high"
$env:ANTHROPIC_SMALL_FAST_MODEL = "gemini-4-hydrogen-low"
$env:CLAUDE_CODE_AUTO_COMPACT_WINDOW = "110000"
$env:NO_PROXY = "127.0.0.1,localhost,::1"
claude
```

Or keep them in `~/.claude/settings.json` (or a project's
`.claude/settings.local.json`):

```json
{
  "model": "gemini-4-carbon-high",
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:8787",
    "ANTHROPIC_AUTH_TOKEN": "local-agy",
    "ANTHROPIC_MODEL": "gemini-4-carbon-high",
    "ANTHROPIC_SMALL_FAST_MODEL": "gemini-4-hydrogen-low",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "110000",
    "NO_PROXY": "127.0.0.1,localhost,::1"
  }
}
```

Without `--api-key`, any non-empty `ANTHROPIC_AUTH_TOKEN` passes Claude Code's
local check; with `--api-key <key>`, set it to `<key>`.

> **Note on Claude Code Mode (`REPL`)**: Liftgate's `/v1/messages` adapter
> supports `REPL` sandbox schemas, `claude-fable-*` family resolution,
> `container` / `allowed_callers` / `caller` fields, and Auto Mode code-review
> classifiers. However, upstream Claude Code (`v2.1.323+`) currently disables
> client-side Code Mode whenever `ANTHROPIC_BASE_URL` or `ANTHROPIC_AUTH_TOKEN`
> is set ([official docs](https://code.claude.com/docs/en/code-mode)). Once
> Anthropic lifts that client-side restriction for custom gateways, Code Mode
> works over Liftgate automatically.

## Other clients

Point an OpenAI Responses client at `http://127.0.0.1:8787/v1` and an Anthropic
Messages client at `http://127.0.0.1:8787` ([API](api.md)).

## Dashboard

Liftgate serves the observability dashboard directly at `http://127.0.0.1:8787/`
(embedded inside release binaries at compile time, or opened via `liftgate ui`):
in-flight and recent turns with their tool calls and token counts, the session
pool (idle sessions can be closed by hand), the model catalog and the last lines
of `agy` stderr. With `--api-key`, `liftgate ui` passes the key automatically
(or enter it in the dashboard's connection settings). Serve an extra dedicated
UI port with `--ui-listen <addr>` or override assets on disk with `--ui-dir <dir>`.

## Flags and environment

| Flag | Environment | Default |
| --- | --- | --- |
| `--state-dir` | `LIFTGATE_HOME` | `~/.liftgate` |
| `--listen` | `AGY_GATEWAY_LISTEN` | `127.0.0.1:8787` |
| `--ui-listen` | `AGY_UI_LISTEN` | none (served on `--listen`) |
| `--agy-bin` | `AGY_BIN` | found on `PATH` or in standard install paths |
| `--agy-proxy` (`<port\|url>` or `off`) | `AGY_PROXY` | none (direct; or `agy_proxy` in `~/.liftgate/settings.json`) |
| `--agy-auto-update` / `--no-agy-auto-update` | `AGY_AUTO_UPDATE` | `true` (or `agy_auto_update` in `~/.liftgate/settings.json`) |
| `--vision-offload` / `--no-vision-offload` | `AGY_VISION_OFFLOAD` | `true` (or `vision_offload` in `~/.liftgate/settings.json`; also live-toggleable in Web UI & `lg top`) |
| `--max-low-cache-records` | `AGY_MAX_LOW_CACHE_RECORDS` | `1000` (`0..=50000`; persisted in `~/.liftgate/cache-telemetry.json`, live-configurable in Web UI) |
| `--low-cache-threshold-pct` | `AGY_LOW_CACHE_THRESHOLD_PCT` | `50` (`0..=100%`; turns below threshold are persisted as low-cache-hit records) |
| `--low-cache-capture-payload` / `--no-low-cache-capture-payload` | `AGY_LOW_CACHE_CAPTURE_PAYLOAD` | `true` (persists full low-cache request payloads under `~/.liftgate/low-cache-payloads/`, live-toggleable in Web UI) |
| `--default-model` (used for unknown or generic model names) | `AGY_DEFAULT_MODEL` | `gemini-4-carbon-high` |
| `--acp-mode` (`auto`, `accept-edits`, `default`, `plan`) | `AGY_ACP_MODE` | `auto` |
| `--acp-cwd` | `AGY_ACP_CWD` | an isolated empty directory under the system temp dir |
| `--api-key` (required as `Authorization: Bearer` or `x-api-key`) | `AGY_GATEWAY_API_KEY` | none: no check |
| `--ui-dir` | `AGY_UI_DIR` | embedded `web/dist` bundle |
| `--max-idle-sessions` | | `64` |
| `--max-stored-responses` | | `256` |
| `--turn-timeout-secs` | Overall gateway deadline per turn, in seconds | `1800` |
| `--max-body-bytes` | | 32 MiB |
| `--max-line-bytes` (one ACP JSON-RPC line) | | 48 MiB |
| `--channel-capacity` (per stream) | | `256` |

Logs follow `RUST_LOG` (default `info`). `liftgate --help` prints the same list.

## Troubleshooting

- **Requests hang or go to a proxy** — add `127.0.0.1,localhost,::1` to
  `NO_PROXY`. If `agy` itself needs an outbound proxy to reach Google
  Antigravity, run `lg proxy 7890` (or `lg proxy <url>`).
- **`could not locate agy executable`** — run `lg agy install`, or pass
  `--agy-bin <path>` / set `AGY_BIN`.
- **`401`** — the client's key does not match `--api-key`.
- **Quota exhausted (`Individual quota reached` / `insufficient_quota`)** — your
  Antigravity account or model quota is exhausted (the error includes the reset
  countdown when `agy` reports one); switch models with `/model` or wait for the
  reset window.
- **Logs from a background run** — run `liftgate logs -f` (`~/.liftgate/gateway.log`),
  or check `.agy-runtime/gateway.{out,err}.log` when using `./run-gateway`.
