<h1 align="center">Liftgate</h1>

<p align="center">
  <strong>Lift</strong>&nbsp;+&nbsp;<strong>gate</strong>way · <em>Lift Antigravity into every agent.</em>
</p>

<p align="center">
  <a href="CHANGELOG.md"><img alt="status: work in progress" src="https://img.shields.io/badge/status-WIP-626C7E"></a>
  <a href="https://github.com/acking-you/antigravity-gateway/actions/workflows/syntax-check.yml"><img alt="CI" src="https://github.com/acking-you/antigravity-gateway/actions/workflows/syntax-check.yml/badge.svg"></a>
  <a href="https://www.rust-lang.org"><img alt="rust 1.85+" src="https://img.shields.io/badge/rust-1.85%2B-355ACF.svg"></a>
  <a href="#license"><img alt="license" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-355ACF.svg"></a>
</p>

<p align="center">
  <strong>English</strong> · <a href="README.zh-CN.md">中文</a>
</p>

> [!WARNING]
> Liftgate is pre-1.0: flags, endpoints and defaults can change without
> notice. Bug reports and feedback are welcome.

Liftgate puts Google Antigravity behind the OpenAI and Anthropic APIs. Keep
using Codex CLI, Claude Code or your own SDK code the way they ship; Liftgate
answers them on your machine and runs every turn through the official `agy`
CLI.

## Works with

<table>
  <tr>
    <td align="center" width="25%"><a href="docs/usage.md#codex-cli"><img src="assets/readme/openai.svg" width="36" alt=""><br><b>Codex CLI</b></a><br><sub>OpenAI Responses</sub></td>
    <td align="center" width="25%"><a href="docs/usage.md#claude-code"><img src="assets/readme/claude.svg" width="36" alt=""><br><b>Claude Code</b></a><br><sub>Anthropic Messages</sub></td>
    <td align="center" width="25%"><a href="docs/api.md#openai-responses"><img src="assets/readme/openai.svg" width="36" alt=""><br><b>OpenAI SDKs</b></a><br><sub><code>/v1/responses</code></sub></td>
    <td align="center" width="25%"><a href="docs/api.md#anthropic-messages"><img src="assets/readme/anthropic.svg" width="36" alt=""><br><b>Anthropic SDKs</b></a><br><sub><code>/v1/messages</code></sub></td>
  </tr>
</table>

Any other harness that speaks either API connects the same way
([API](docs/api.md)).

## What it does

- **Official interface only** — talks to Antigravity only through `agy acp`,
  the Agent Client Protocol the CLI ships for editors. Login, tokens, quota and
  TLS stay inside `agy` ([why ACP](docs/architecture.md#why-acp)).
- **Two standard APIs** — OpenAI Responses and Anthropic Messages, streamed,
  with tool calls, thinking, images and token counting ([API](docs/api.md)).
- **Your agent keeps the tools** — ACP sessions run in an empty directory with
  no MCP servers, so file edits and shell commands stay in Codex or Claude
  Code; web search and an SSRF-safe web fetch run in the gateway
  ([inference-only sessions](docs/architecture.md#inference-only-sessions)).
- **Warm sessions** — each request's history is matched to a pooled ACP
  session and only the new turn is sent ([session reuse](docs/architecture.md#session-reuse)).
- **Live model catalog** — `/v1/models` and a Codex catalog list every model
  your account can use, so `/model` pickers show Antigravity models
  ([Codex setup](docs/usage.md#codex-cli)).
- **Failover and fast lanes** — on an upstream `429` a turn moves to a backup
  model at once; titles, recaps and Auto Mode classifiers go to the fastest
  Flash model without touching your main session ([model routing](docs/architecture.md#model-routing)).
- **Live dashboard** — in-flight turns, the session pool, token usage, models
  and `agy` logs at `http://127.0.0.1:8787/` ([dashboard](docs/usage.md#dashboard)).

## How it works

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/readme/how-it-works-dark.svg">
  <img src="assets/readme/how-it-works-light.svg" width="100%" alt="Codex CLI, Claude Code and SDKs send stateless HTTP requests to Liftgate, which translates them, reuses a warm session, streams events back and fails over on 429; it talks JSON-RPC over stdio to one agy acp process per session, and only agy holds the credentials for Google Antigravity.">
</picture>

Standard LLM APIs are stateless: every request carries the whole
conversation. ACP is a stateful editor session. Liftgate keeps one
`agy acp` process per session, forwards only what is new, and streams ACP
updates back as OpenAI or Anthropic events. See [architecture](docs/architecture.md).

## Get started

You need the Antigravity CLI (`agy`), Rust 1.85+ and, for the dashboard,
[Bun](https://bun.sh) (optional).

```bash
agy acp --login     # once, to sign in to Antigravity
./run-gateway.sh    # build and serve http://127.0.0.1:8787 (Ctrl+C to stop)
./start-agy.sh      # in another terminal: Claude Code through Liftgate (--codex for Codex)
```

On Windows use `.\run-gateway.cmd` and `.\start-agy.cmd` (`-Codex`).
`start-agy` keeps its settings to the agent it launches: `~/.codex/config.toml`
and your shell stay untouched. Background mode, every flag and manual Codex or
Claude Code setup: [usage guide](docs/usage.md).

## Documentation

| | |
| --- | --- |
| **Use** | [Usage guide](docs/usage.md) · [Codex CLI](docs/usage.md#codex-cli) · [Claude Code](docs/usage.md#claude-code) · [Dashboard](docs/usage.md#dashboard) · [Flags](docs/usage.md#flags-and-environment) |
| **Reference** | [API](docs/api.md) · [Architecture](docs/architecture.md) |
| **Project** | [Changelog](CHANGELOG.md) · [Rust standard](docs/2026-11-09-RUST.md) · [AGENTS.md](AGENTS.md) |

All pages: [docs index](docs/README.md).

## Contributing

Build, lint and test commands, architecture rules and the Rust standard every
change follows are in [AGENTS.md](AGENTS.md); note user-visible changes in
[CHANGELOG.md](CHANGELOG.md).

## License

Either [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your
option. The client marks in `assets/readme/` come from Simple Icons (CC0) and
belong to their owners ([notice](assets/readme/NOTICE.txt)).
