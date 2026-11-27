# API

Liftgate answers the OpenAI Responses and Anthropic Messages APIs on one
address (default `http://127.0.0.1:8787`). Every `/v1/...` route also answers
without the `/v1` prefix.

| Route | Purpose |
| --- | --- |
| `POST /v1/responses` | OpenAI Responses, streamed (SSE) or not |
| `GET /v1/responses/{id}` | a completed response, kept in memory (last 256 by default) |
| `POST /v1/messages` | Anthropic Messages, streamed (SSE) or not |
| `POST /v1/messages/count_tokens` | Anthropic token count (a local estimate) |
| `GET /v1/models`, `GET /v1/models/{id}` | the models your Antigravity account can use |
| `GET /v1/codex-models?default_model=<id>` | the same models as a Codex `model_catalog_json` |
| `GET /claude-desktop/v1/models`, `POST /claude-desktop/v1/messages` | Claude Desktop 3P gateway namespace (encodes ACP models into Claude-safe `claude-*-lg--<id>` route IDs and unwraps them automatically) |
| `GET /healthz` | liveness |
| `GET /api/telemetry/snapshot`, `GET /api/telemetry/events` | dashboard data (snapshot and SSE stream) |
| `GET /api/telemetry/low-cache-hits`, `GET /api/telemetry/low-cache-hits/{task_id}` | persisted low-cache-hit audit records and full captured request payloads |
| `POST /api/telemetry/low-cache-hits/clear`, `POST /api/telemetry/cache-config` | clear low-cache records or update retention cap, threshold, and payload capture live |
| `POST /api/telemetry/vision-offload` | live toggle for optional `>=300k` ephemeral vision sub-session offload (`false` by default) |
| `POST /api/telemetry/mcp-tool-bridge` | live toggle for the hybrid loopback MCP tool bridge |
| `POST /api/telemetry/sessions/{id}/close` | close an idle or MCP-suspended pooled session |
| `GET /api/telemetry/tool-calls[?since=<revision>]`, `GET /api/telemetry/tool-calls/{call_id}` | per-call tool I/O from a bounded in-memory ring buffer (route, status, timing, sizes, wire vs. forwarded arguments, output blocks); `since` returns only records changed after that revision |
| `GET /api/telemetry/sessions/{id}/tool-catalog` | how a pooled MCP session's client tools were virtualized: tier per tool, client vs. wire definitions, wire tools, bridge rules, and the latest context composition |
| `GET /api/telemetry/context-captures`, `GET /api/telemetry/context-captures/{task_id}` | opt-in full-context captures (index, newest first) and one captured turn |
| `POST /api/telemetry/context-capture`, `POST /api/telemetry/context-captures/clear` | toggle full-context capture and set its quota (`{"enabled", "max_records", "max_total_mib"}`), or delete every capture |
| `POST /api/admin/shutdown` | graceful shutdown for managed background daemons (requires `x-liftgate-shutdown-token`) |

## Authentication

Started without `--api-key`, Liftgate checks nothing; bind it to loopback (the
default). With `--api-key <key>`, send `Authorization: Bearer <key>` or
`x-api-key: <key>`; the telemetry routes also accept `?api_key=<key>`.

## OpenAI Responses

Base URL `http://127.0.0.1:8787/v1`.

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8787/v1", api_key="local-agy")
reply = client.responses.create(model="gemini-4-carbon-high", input="Hello")
print(reply.output_text)
```

- Tools: `function`, `custom` and `namespace` tools, plus Codex `local_shell`
  and `apply_patch`; the client runs them and sends the results back.
- Built in: `web_search`, and server-side page fetching for the `open_page`
  action.
- Reasoning: the model's thoughts stream as reasoning summary events. When a
  client asks for `reasoning.encrypted_content`, it gets a fixed placeholder;
  the thoughts themselves are only in the summary.
- `instructions` and `developer` / `system` items become
  `<developer_instructions>` blocks in the prompt
  ([why](architecture.md#translate)).

## Anthropic Messages

Base URL `http://127.0.0.1:8787` (the SDK adds `/v1/messages`).

```python
import anthropic

client = anthropic.Anthropic(base_url="http://127.0.0.1:8787", api_key="local-agy")
reply = client.messages.create(
    model="gemini-4-carbon-high",
    max_tokens=1024,
    messages=[{"role": "user", "content": "Hello"}],
)
print(reply.content[0].text)
```

- Extended thinking, parallel `tool_use` / `tool_result` and image input.
- Built in: `web_search_20250305` and `web_fetch_20250910`; fetches refuse
  private and loopback addresses.
- Claude Code titles use the configured title model. Classifiers and recaps use
  the fast model selector ([model routing](architecture.md#model-routing)).

## Models

Use any ID from `GET /v1/models`, such as `gemini-4-carbon-high` or
`gemini-3.8-flash-low`. An unknown or generic name (for example a client's
built-in default) maps to `--default-model`. The catalog reports a
500,000-token context window (`auto_compact_token_limit = 425,000`) for
`gemini-*` models, 200,000 / 170,000 for Claude models, and 128,000 / 110,000
for `gpt-oss*` models.
