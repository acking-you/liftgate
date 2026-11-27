# Remote gateway guide

English · [中文](remote.zh-CN.md)

Run Liftgate on a remote Linux, macOS, or Windows host (over Tailscale,
WireGuard, an SSH tunnel, a private LAN, or an HTTPS reverse proxy) and connect
Codex CLI (`cx`), Oh My Pi (`omp`), Claude Code (`cc`), or OpenCode (`oc`) from
your local machine.

No local hooks, binary patches, or companion proxy daemons are needed on the
client machine.

## How cross-host execution works

Liftgate separates remote model inference from local workspace execution over
standard `/v1/responses` and `/v1/messages` HTTP + SSE streams:

- **Tools stay on your local machine**: remote `agy acp` processes run inside an
  isolated empty directory with host tools disabled. When the model calls shell,
  file-edit, or image-inspection tools (`exec_command`, `apply_patch`, `exec`,
  `eval`, `Bash`, `Edit`, `execute`, `view_image`), Liftgate suspends the live
  `session/prompt` turn on its remote loopback MCP bridge (`127.0.0.1:0`),
  streams standard tool-call events back to your local CLI, and resumes the
  same ACP prompt as soon as your local CLI posts the tool output.
- **Context optimizations run automatically on the remote gateway**:
  - **Warm `SessionPool` & KV prefix caching**: billing headers and volatile
    reminders are normalized so multi-turn chats, `Ctrl+C` steering interrupts,
    and client `/compact` turns reuse warm remote ACP sessions instead of
    re-uploading the full history.
  - **3-tier MCP tool virtualization**: core coding tools stay 1:1 in Eager
    slots while large MCP/plugin packs fold into `mcp_lg_ns__*` hubs backed by
    0-RTT in-memory `mcp_lg_search_client_tools` on the remote gateway.
  - **Lossless truncation spills across hosts**: oversized tool outputs and web
    fetches are middle-truncated on clean line boundaries while the full payload
    is saved to `~/.liftgate/spills/` on the remote host. Because the model
    reads slices and searches via the gateway-local `mcp_lg_read_context_spill`
    tool in 0-RTT, local clients never need filesystem access to the remote
    spill directory.
  - **Server-side web fetch & Flash fast lanes**: `mcp_lg_gateway_web_fetch`
    runs on the remote gateway, and session titles, summaries, and classifier
    sub-requests route to ephemeral Flash sessions without evicting your active
    coding session.

## 1. Start Liftgate on the remote server

Install `lg` and sign in to `agy` once on the remote host:

```bash
# Install lg + liftgate
curl -fsSL https://raw.githubusercontent.com/acking-you/liftgate/main/scripts/install.sh | sh

# Install official agy CLI and sign in
lg agy install
lg agy login

# Optional: if the remote host needs an outbound proxy to reach Google Antigravity
# lg proxy http://127.0.0.1:7890
```

By default `liftgate` binds to `127.0.0.1:8787`. On a remote server, bind
`--listen` to a private Tailscale/LAN interface (recommended) or `0.0.0.0:8787`,
and require `--api-key` (which protects `/v1/*`, `/claude-desktop/*`, and
`/api/telemetry/*`):

```bash
# Background daemon (recommended: bind to your Tailscale/WireGuard/LAN IP or 0.0.0.0)
lg up --listen 0.0.0.0:8787 --api-key "<YOUR_API_KEY>"

# Or run in the foreground (e.g. inside systemd, tmux, or Docker)
liftgate serve --listen 0.0.0.0:8787 --api-key "<YOUR_API_KEY>"
```

Verify from your local machine (replace `100.64.0.10:8787` below with your
remote host and port):

```bash
curl -fsS http://100.64.0.10:8787/healthz
curl -fsS -H "Authorization: Bearer <YOUR_API_KEY>" http://100.64.0.10:8787/v1/models
```

## 2. Option A: Simplified local setup with `lg` (recommended)

Install **only** the `lg` binary on your local machine — **no local `agy`
binary, no Google login, and no local gateway daemon are needed**. `lg` fetches
the live model catalog from the remote server, calibrates per-model context and
compaction limits, enables Code Mode, and adds the remote host to `NO_PROXY`
automatically.

### Zero-pollution launchers (`lg remote` + `lg cx` / `omp` / `cc` / `oc`)

Save the remote target once in `~/.liftgate/settings.json`:

```bash
# Save remote endpoint and API key (verifies /healthz automatically)
lg remote http://100.64.0.10:8787 --api-key "<YOUR_API_KEY>"

# Launch any coding agent in your local repo against the remote gateway
lg cx        # OpenAI Codex CLI (lg cxa for Codex App GUI)
lg omp       # Oh My Pi
lg cc        # Claude Code (lg cd for Claude Desktop GUI)
lg oc        # OpenCode

# Inspect remote status, switch default models, or open the live TUI remotely
lg st
lg m -i
lg top
```

Useful one-off overrides and reset commands:

```bash
# One-off remote session without changing saved settings
lg cx --remote 100.64.0.10:8787 --api-key "<YOUR_API_KEY>"

# Or via environment variables
export AGY_REMOTE="http://100.64.0.10:8787"
export AGY_GATEWAY_API_KEY="<YOUR_API_KEY>"
lg omp

# Switch back to a local managed daemon anytime
lg remote local
```

### Sync persistent config files (`lg config`)

If you prefer typing bare `codex`, `omp`, `claude`, or `opencode` directly in
any terminal, let `lg config` non-destructively merge the remote provider and
live model catalog into your global (or project-local) config files:

```bash
# Export AGY_GATEWAY_API_KEY in your shell profile (~/.zshrc, ~/.bashrc, or PowerShell $PROFILE)
# so Codex CLI can read it via env_key = "AGY_GATEWAY_API_KEY":
export AGY_GATEWAY_API_KEY="<YOUR_API_KEY>"

# Sync all four harnesses globally against the saved `lg remote` target (or pass --remote)
lg config --global --remote http://100.64.0.10:8787 --api-key "<YOUR_API_KEY>"

# Or sync individual harnesses:
lg config codex --global
lg config omp --global
lg config claude --global
lg config opencode --global
```

## 3. Option B: Manual copy-paste setup (no local `lg` required)

If you do not want to install `lg` on your local machine, copy the configuration
for your harness below. Replace:

- `http://100.64.0.10:8787` (or `https://lg.example.com` behind an HTTPS reverse proxy) with your remote gateway URL
- `100.64.0.10` in `NO_PROXY` with your remote IP or domain so local HTTP proxies do not intercept gateway traffic
- `<YOUR_API_KEY>` with your gateway `--api-key` (or `local-agy` if running on a trusted network without `--api-key`)

---

### Codex CLI (`cx` / `codex`)

Native `codex` does not fetch `/v1/codex-models` on its own. Exporting the
catalog file once is essential: it unlocks `/model` picker entries, `500k / 425k`
Gemini context & auto-compaction limits, `80 KB` per-tool output budgets, Code
Mode (`exec` + `wait`), and Liftgate's calibrated `base_instructions`.

**1. Export the live model catalog from the remote gateway:**

```bash
# macOS / Linux
mkdir -p ~/.codex
curl -fsSL -H "Authorization: Bearer <YOUR_API_KEY>" \
  "http://100.64.0.10:8787/v1/codex-models?default_model=gemini-4-carbon-high&tool_mode=code_mode" \
  -o ~/.codex/agy-models.json
```

```powershell
# Windows (PowerShell)
New-Item -ItemType Directory -Force -Path "$HOME\.codex" | Out-Null
Invoke-WebRequest `
  -Headers @{ Authorization = "Bearer <YOUR_API_KEY>" } `
  -Uri "http://100.64.0.10:8787/v1/codex-models?default_model=gemini-4-carbon-high&tool_mode=code_mode" `
  -OutFile "$HOME\.codex\agy-models.json"
```

**2. Edit `~/.codex/config.toml` (`%USERPROFILE%\.codex\config.toml` on Windows):**

```toml
model = "gemini-4-carbon-high"
model_provider = "agy"
# Must be an absolute path; use forward slashes on Windows (e.g. "C:/Users/you/.codex/agy-models.json")
model_catalog_json = "/Users/you/.codex/agy-models.json"
approval_policy = "never"
sandbox_mode = "danger-full-access"
web_search = "live"
suppress_unstable_features_warning = true

[features]
code_mode = true

[model_providers.agy]
name = "Liftgate"
base_url = "http://100.64.0.10:8787/v1"
env_key = "AGY_GATEWAY_API_KEY"
wire_api = "responses"
requires_openai_auth = false
stream_idle_timeout_ms = 1800000
```

> Remove `approval_policy` and `sandbox_mode` above if you prefer standard
> confirmation prompts.

**3. Export your API key and `NO_PROXY`, then launch `codex`:**

```bash
export AGY_GATEWAY_API_KEY="<YOUR_API_KEY>"
export NO_PROXY="${NO_PROXY:+$NO_PROXY,}127.0.0.1,localhost,::1,100.64.0.10"
codex
```

---

### Oh My Pi (`omp`)

**1. Create or merge `~/.omp/agent/models.yml`:**

```yaml
providers:
  agy:
    baseUrl: http://100.64.0.10:8787/v1
    apiKey: "<YOUR_API_KEY>"
    api: openai-responses
    authHeader: true
    compat:
      supportsStore: false
      statefulResponses: false
      supportsDeveloperRole: true
      supportsReasoningEffort: true
      supportsReasoningSummary: true
      supportsToolChoice: true
      streamIdleTimeoutMs: 1800000
    models:
      - id: gemini-4-carbon-high
        name: "Gemini 4 Carbon (High)"
        api: openai-responses
        reasoning: true
        thinking:
          mode: effort
          efforts: [low, medium, high]
          defaultLevel: high
        input: [text, image]
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }
        contextWindow: 500000
        maxTokens: 64000
        preferWebsockets: false
      - id: gemini-4-hydrogen-low
        name: "Gemini 4 Hydrogen (Low)"
        api: openai-responses
        reasoning: true
        thinking:
          mode: effort
          efforts: [low, medium, high]
          defaultLevel: low
        input: [text, image]
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }
        contextWindow: 500000
        maxTokens: 64000
        preferWebsockets: false
```

**2. Create or merge `~/.omp/agent/config.yml`:**

```yaml
inlineToolDescriptors: "off"
tools:
  approvalMode: yolo
  commands: true
eval:
  py: true
  js: true
mcp:
  evalNamespaces: true
modelRoles:
  default: agy/gemini-4-carbon-high
  slow: agy/gemini-4-carbon-high
  plan: agy/gemini-4-carbon-high
  task: agy/gemini-4-carbon-high
  vision: agy/gemini-4-carbon-high
  smol: agy/gemini-4-hydrogen-low
  commit: agy/gemini-4-hydrogen-low
  tiny: agy/gemini-4-hydrogen-low
  memory: agy/gemini-4-hydrogen-low
  widget: agy/gemini-4-hydrogen-low
```

> Change `tools.approvalMode` to `always-ask` if you want confirmation prompts.

**3. Bypass local proxies for the remote host and launch `omp`:**

```bash
export NO_PROXY="${NO_PROXY:+$NO_PROXY,}127.0.0.1,localhost,::1,100.64.0.10"
omp
```

---

### Claude Code (`cc` / `claude`)

**Option 1 — Persistent `~/.claude/settings.json` (or `.claude/settings.local.json`) with calibrated `425k` Gemini compaction:**

```json
{
  "model": "gemini-4-carbon-high",
  "skipDangerousModePermissionPrompt": true,
  "permissions": {
    "defaultMode": "bypassPermissions"
  },
  "autoCompactWindow": 110000,
  "modelOverrides": {
    "claude-fable-5-2": "gemini-4-carbon-high",
    "claude-opus-5-5": "gemini-4-carbon-high",
    "claude-sonnet-5-5": "gemini-4-carbon-high",
    "claude-haiku-5-5": "gemini-4-hydrogen-low",
    "claude-opus-4-6": "gemini-4-carbon-high",
    "claude-sonnet-4-6": "gemini-4-carbon-high",
    "claude-haiku-4-5-20251001": "gemini-4-hydrogen-low"
  },
  "modelSettings": {
    "gemini-4-carbon-high": { "autoCompactWindow": 425000 },
    "gemini-4-hydrogen-low": { "autoCompactWindow": 425000 },
    "claude-fable-5-2": { "autoCompactWindow": 425000 },
    "claude-opus-5-5": { "autoCompactWindow": 425000 },
    "claude-sonnet-5-5": { "autoCompactWindow": 425000 },
    "claude-haiku-5-5": { "autoCompactWindow": 425000 },
    "claude-opus-4-6": { "autoCompactWindow": 425000 },
    "claude-sonnet-4-6": { "autoCompactWindow": 425000 },
    "claude-haiku-4-5-20251001": { "autoCompactWindow": 425000 }
  },
  "env": {
    "ANTHROPIC_BASE_URL": "http://100.64.0.10:8787",
    "ANTHROPIC_AUTH_TOKEN": "<YOUR_API_KEY>",
    "ANTHROPIC_API_KEY": "",
    "ANTHROPIC_MODEL": "gemini-4-carbon-high",
    "ANTHROPIC_DEFAULT_FABLE_MODEL": "gemini-4-carbon-high",
    "ANTHROPIC_DEFAULT_OPUS_MODEL": "gemini-4-carbon-high",
    "ANTHROPIC_DEFAULT_SONNET_MODEL": "gemini-4-carbon-high",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "gemini-4-hydrogen-low",
    "ANTHROPIC_SMALL_FAST_MODEL": "gemini-4-hydrogen-low",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    "CLAUDE_CODE_AUTO_MODE_SERVER": "0",
    "NO_PROXY": "127.0.0.1,localhost,::1,100.64.0.10"
  }
}
```

> Remove `skipDangerousModePermissionPrompt` and `permissions` if you want
> standard permission prompts. If you switch `model` to an Anthropic `claude-*`
> model on the gateway, set its `autoCompactWindow` to `170000`.

**Option 2 — Quick single-shell environment variables:**

```bash
export ANTHROPIC_BASE_URL="http://100.64.0.10:8787"
export ANTHROPIC_AUTH_TOKEN="<YOUR_API_KEY>"
export ANTHROPIC_API_KEY=""
export ANTHROPIC_MODEL="gemini-4-carbon-high"
export ANTHROPIC_SMALL_FAST_MODEL="gemini-4-hydrogen-low"
export CLAUDE_CODE_AUTO_COMPACT_WINDOW="425000"
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1"
export NO_PROXY="127.0.0.1,localhost,::1,100.64.0.10"
claude
```

---

### OpenCode (`oc` / `opencode`)

**1. Create or merge `~/.config/opencode/opencode.json` (or `./opencode.json` in your project):**

```json
{
  "$schema": "https://opencode.ai/config.json",
  "model": "agy/gemini-4-carbon-high",
  "small_model": "agy/gemini-4-hydrogen-low",
  "permission": {
    "*": "allow"
  },
  "experimental": {
    "full_codemode": true
  },
  "compaction": {
    "auto": true,
    "background": false
  },
  "provider": {
    "agy": {
      "npm": "@ai-sdk/anthropic",
      "name": "Antigravity (Liftgate)",
      "options": {
        "baseURL": "http://100.64.0.10:8787/v1",
        "apiKey": "<YOUR_API_KEY>",
        "timeout": 1800000
      },
      "models": {
        "gemini-4-carbon-high": {
          "name": "Gemini 4 Carbon (High)",
          "tool_call": true,
          "limit": { "context": 500000, "output": 64000 },
          "modalities": { "input": ["text", "image"], "output": ["text"] }
        },
        "gemini-4-hydrogen-low": {
          "name": "Gemini 4 Hydrogen (Low)",
          "tool_call": true,
          "limit": { "context": 500000, "output": 64000 },
          "modalities": { "input": ["text", "image"], "output": ["text"] }
        }
      }
    }
  }
}
```

> Remove `"permission": { "*": "allow" }` if you prefer interactive permission
> prompts.

**2. Bypass local proxies (and set `OPENCODE_EXPERIMENTAL_CODE_MODE=true` if still on OpenCode v1.x) and launch `opencode`:**

```bash
export NO_PROXY="${NO_PROXY:+$NO_PROXY,}127.0.0.1,localhost,::1,100.64.0.10"
opencode
```

## Operational rules & limits

- **Keep `--mcp-tool-bridge` enabled on the remote gateway** (the default).
  Passing `--no-mcp-tool-bridge` falls back to XML-only tool routing and removes
  `mcp_lg_read_context_spill`, which prevents remote sessions from reading
  truncated spill files stored in the remote host's `~/.liftgate/spills/`.
- **`lg remote` / `--remote` uses plain HTTP (`http://<host>:<port>`)**:
  Liftgate's built-in CLI control client speaks HTTP/1.1 directly over TCP
  (supporting IPv4, bracketed IPv6 like `[fd00::1]:8787`, and Tailscale/DNS
  hostnames without TLS or custom path prefixes). Use it over Tailscale,
  WireGuard, SSH local port forwarding (`ssh -L 8787:127.0.0.1:8787 user@host`),
  or a trusted LAN.
- **HTTPS reverse proxies (Nginx, Caddy, Cloudflare Tunnel)**: if you terminate
  TLS in front of the remote gateway, configure your local harnesses directly
  using **Option B (manual setup)** above with `https://your-domain.example/v1`
  (or `https://your-domain.example` for Claude Code). Ensure your reverse proxy:
  - Disables SSE response buffering (`proxy_buffering off;` in Nginx)
  - Allows `>= 32 MiB` request bodies (`client_max_body_size 32m;`)
  - Sets upstream read/send timeouts to at least `1800s` (`proxy_read_timeout 1800s;`)
- **Refreshing manual model catalogs**: `lg cx` / `lg omp` / `lg cc` / `lg oc`
  query live models on startup, whereas manual config files (`agy-models.json`,
  `models.yml`, `opencode.json`) store a point-in-time catalog snapshot. Re-run
  the `curl /v1/codex-models` command (or `lg config --global`) after `agy`
  introduces new upstream models.
