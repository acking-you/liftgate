# Usage guide

English · [中文](usage.zh-CN.md)

Run Liftgate, then point Codex CLI, Claude Code or any OpenAI / Anthropic
client at it. Every address below assumes the default
`http://127.0.0.1:8787`.

## Before you start

- **Antigravity CLI (`agy`)**, installed and signed in once with
  `agy acp --login`. Liftgate finds it on `PATH` and in the standard install
  locations; otherwise pass `--agy-bin <path>`.
- **Rust 1.85+** to build the gateway.
- **Bun** (optional) to build the dashboard into `web/dist`. Without it every
  API still works.

## Run the gateway

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

## Launchers

`start-agy` starts the gateway if it is not running, syncs the live model list
and opens Claude Code or Codex already pointed at it. Its settings apply only
to the process it launches; when that exits, your shell is as it was.

| | Linux / macOS | Windows |
| --- | --- | --- |
| Claude Code | `./start-agy.sh` | `.\start-agy.cmd` |
| Codex CLI | `./start-agy.sh --codex` | `.\start-agy.cmd -Codex` |
| Standard permissions | `./start-agy.sh --no-bypass` | `.\start-agy.cmd -NoBypass` |
| Services only | `./start-agy.sh --services-only` | `.\start-agy.cmd -BackgroundOnly` |
| Stop services | `./start-agy.sh --stop` | `.\start-agy.cmd -StopServices` |

- By default `start-agy` passes `--dangerously-skip-permissions` to Claude Code
  and `--dangerously-bypass-approvals-and-sandbox` to Codex; pass
  `--no-bypass` / `-NoBypass` (or your own `--permission-mode` / `--sandbox`
  flag) to keep standard prompts.
- Remaining arguments pass through to `claude` or `codex`
  (`./start-agy.sh -p "hello"`).
- Change the defaults with `--gateway-model` (`-GatewayModel`), `--fast-model`
  (`-FastModel`), `--host` (`-ListenHost`), `--port` (`-ListenPort`),
  `--ui-port` (`-UiPort`) and `--api-key` (`-ApiKey`), or with `AGY_MODEL`,
  `AGY_FAST_MODEL`, `AGY_HOST`, `AGY_PORT`, `AGY_WEB_PORT` and `AGY_API_KEY`.
- With Bun installed it also starts the dashboard dev server on port `5173`.
- It writes only inside the repository: `.agy-runtime/` (logs, PIDs, the Codex
  catalog), `claude-carbon.settings.json` and `.claude/settings.local.json`.
- On Windows, `-InstallEnv` opts in to saving the variables to your user
  profile so any new terminal's `claude` uses Liftgate; `-UninstallEnv` removes
  them.

## Codex CLI

**1. Export the model catalog** (optional, recommended) so `/model` in Codex
lists every Antigravity model with its context window:

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
model_context_window = 1000000
model_auto_compact_token_limit = 850000

[model_providers.agy]
name = "Liftgate"
base_url = "http://127.0.0.1:8787/v1"
wire_api = "responses"
# If the gateway runs with --api-key <key>, uncomment and export AGY_GATEWAY_API_KEY=<key>:
# env_key = "AGY_GATEWAY_API_KEY"
```

**3. Run `codex`.** Behind an HTTP proxy, keep loopback off it first:
`export NO_PROXY="127.0.0.1,localhost,::1"`.

## Claude Code

Set the variables for one shell:

```bash
export ANTHROPIC_BASE_URL="http://127.0.0.1:8787"
export ANTHROPIC_AUTH_TOKEN="local-agy"
export ANTHROPIC_MODEL="gemini-4-carbon-high"
export ANTHROPIC_SMALL_FAST_MODEL="gemini-3.8-flash-low"
export CLAUDE_CODE_MAX_CONTEXT_TOKENS="1000000"
export NO_PROXY="127.0.0.1,localhost,::1"
claude
```

```powershell
$env:ANTHROPIC_BASE_URL = "http://127.0.0.1:8787"
$env:ANTHROPIC_AUTH_TOKEN = "local-agy"
$env:ANTHROPIC_MODEL = "gemini-4-carbon-high"
$env:ANTHROPIC_SMALL_FAST_MODEL = "gemini-3.8-flash-low"
$env:CLAUDE_CODE_MAX_CONTEXT_TOKENS = "1000000"
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
    "ANTHROPIC_SMALL_FAST_MODEL": "gemini-3.8-flash-low",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "1000000",
    "NO_PROXY": "127.0.0.1,localhost,::1"
  }
}
```

Without `--api-key`, any non-empty `ANTHROPIC_AUTH_TOKEN` passes Claude Code's
local check; with `--api-key <key>`, set it to `<key>`.

## Other clients

Point an OpenAI Responses client at `http://127.0.0.1:8787/v1` and an Anthropic
Messages client at `http://127.0.0.1:8787` ([API](api.md)).

## Dashboard

When `web/dist` is built, Liftgate serves the dashboard at
`http://127.0.0.1:8787/`: in-flight and recent turns with their tool calls and
token counts, the session pool (idle sessions can be closed by hand), the model
catalog and the last lines of `agy` stderr. With `--api-key`, enter the key in
the dashboard's connection settings. Serve another build with `--ui-dir <dir>`.

## Flags and environment

| Flag | Environment | Default |
| --- | --- | --- |
| `--listen` | `AGY_GATEWAY_LISTEN` | `127.0.0.1:8787` |
| `--agy-bin` | `AGY_BIN` | found on `PATH` or in standard install paths |
| `--default-model` (used for unknown or generic model names) | `AGY_DEFAULT_MODEL` | `gemini-4-carbon-high` |
| `--acp-mode` (`auto`, `accept-edits`, `default`, `plan`) | `AGY_ACP_MODE` | `auto` |
| `--acp-cwd` | `AGY_ACP_CWD` | an isolated empty directory under the system temp dir |
| `--api-key` (required as `Authorization: Bearer` or `x-api-key`) | `AGY_GATEWAY_API_KEY` | none: no check |
| `--ui-dir` | `AGY_UI_DIR` | `./web/dist` when present |
| `--max-idle-sessions` | | `64` |
| `--max-stored-responses` | | `256` |
| `--turn-timeout-secs` | | `600` |
| `--max-body-bytes` | | 12 MiB |
| `--max-line-bytes` (one ACP JSON-RPC line) | | 16 MiB |
| `--channel-capacity` (per stream) | | `256` |

Logs follow `RUST_LOG` (default `info`). `liftgate --help` prints the same list.

## Troubleshooting

- **Requests hang or go to a proxy** — add `127.0.0.1,localhost,::1` to
  `NO_PROXY`.
- **`could not locate agy executable`** — pass `--agy-bin <path>` or set
  `AGY_BIN`.
- **`401`** — the client's key does not match `--api-key`.
- **Quota exhausted (`Individual quota reached` / `insufficient_quota`)** — your
  Antigravity account or model quota is exhausted (the error includes the reset
  countdown when `agy` reports one); switch models with `/model` or wait for the
  reset window.
- **Logs from a background run** — `.agy-runtime/gateway.out.log` and
  `.agy-runtime/gateway.err.log`.
