<p align="center">
  <img src="packaging/icons/app_icon.svg" width="76" alt="Liftgate icon">
</p>

<h1 align="center">Liftgate</h1>

<p align="center">
  <strong>Lift</strong>&nbsp;+&nbsp;<strong>gate</strong>way · <em>Lift Antigravity into every agent.</em>
</p>

<p align="center">
  <a href="https://github.com/acking-you/liftgate/releases/latest"><img alt="CLI release" src="https://img.shields.io/github/v/release/acking-you/liftgate?color=355ACF&label=CLI"></a>
  <a href="https://github.com/acking-you/liftgate/releases/tag/v0.2.3-ui"><img alt="Desktop release" src="https://img.shields.io/badge/Desktop-v0.2.3-D97757"></a>
  <a href="https://github.com/acking-you/liftgate/actions/workflows/syntax-check.yml"><img alt="CI" src="https://github.com/acking-you/liftgate/actions/workflows/syntax-check.yml/badge.svg"></a>
  <a href="https://www.rust-lang.org"><img alt="rust 1.100.0" src="https://img.shields.io/badge/rust-1.100.0-355ACF.svg"></a>
  <a href="#license"><img alt="license" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-355ACF.svg"></a>
</p>

<p align="center">
  <strong>English</strong> · <a href="README.zh-CN.md">中文</a>
</p>

> [!WARNING]
> Liftgate is pre-1.0: flags, endpoints and defaults can change without
> notice. Bug reports and feedback are welcome.

Liftgate puts Google Antigravity behind standard OpenAI Responses and Anthropic
Messages APIs on your machine. Keep using Codex CLI, Codex App, Claude Code,
Claude Desktop, or your own SDK code unchanged — Liftgate answers them locally
and runs every turn exclusively through the official `agy acp` interface.

Run it from the terminal with **`lg`** / **`liftgate`** (CLI + full-screen TUI),
or install [**Liftgate Desktop**](#liftgate-desktop) — a standalone native GPUI
control center for macOS, Windows, and Linux.

## Works with

<table>
  <tr>
    <td align="center" width="25%"><a href="docs/usage.md#codex-cli"><img src="assets/readme/openai.svg" width="36" alt=""><br><b>Codex CLI &amp; App</b></a><br><sub>OpenAI Responses</sub></td>
    <td align="center" width="25%"><a href="docs/usage.md#claude-code"><img src="assets/readme/claude.svg" width="36" alt=""><br><b>Claude Code &amp; Desktop</b></a><br><sub>Anthropic Messages</sub></td>
    <td align="center" width="25%"><a href="docs/api.md#openai-responses"><img src="assets/readme/openai.svg" width="36" alt=""><br><b>OpenAI SDKs</b></a><br><sub><code>/v1/responses</code></sub></td>
    <td align="center" width="25%"><a href="docs/api.md#anthropic-messages"><img src="assets/readme/anthropic.svg" width="36" alt=""><br><b>Anthropic SDKs</b></a><br><sub><code>/v1/messages</code></sub></td>
  </tr>
</table>

Any other harness that speaks either API connects the same way
([API](docs/api.md)).

## Install

### CLI (`lg` + `liftgate`)

Install prebuilt `lg` and `liftgate` binaries (with the Web dashboard embedded
and `SHA256SUMS` verified fail-closed) in one command:

**macOS / Linux / FreeBSD**

```bash
curl -fsSL https://raw.githubusercontent.com/acking-you/liftgate/main/scripts/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://raw.githubusercontent.com/acking-you/liftgate/main/scripts/install.ps1 | iex
```

`LIFTGATE_VERSION=v0.2.2` pins a tag, `LIFTGATE_BIN_DIR` sets the install
`LIFTGATE_VERSION=v0.2.3` pins a tag, `LIFTGATE_BIN_DIR` sets the install
directory, and `LIFTGATE_NO_MODIFY_PATH=1` skips updating `PATH`. Once
installed, run `lg upgrade` anytime to update in place.

<details>
<summary>Other CLI install methods (<code>cargo binstall</code>, GitHub Releases, source build)</summary>

```bash
# Prebuilt archive via cargo-binstall
cargo binstall liftgate

# Build from a local checkout (Rust 1.100.0; embeds Web UI automatically when Bun is installed)
cargo install --path crates/liftgate --locked
```

Prebuilt 17-target archives (static `musl` Linux, `glibc` Linux, macOS
ARM64/Intel, Windows x64/ARM64/x86, FreeBSD) are also available directly on the
[latest CLI release](https://github.com/acking-you/liftgate/releases/latest).

</details>

### Liftgate Desktop

**Liftgate Desktop** (`liftgate-desktop`) is a standalone native GPUI desktop
application built on `gpui-kit` — no Electron/WebView runtime and no separate
CLI install required (the headless gateway daemon and React web dashboard are
compiled right in).

- **Warm Editorial Light & Dark themes + `中文` / `EN` bilingual UI** — follows
  your OS appearance and system language automatically on first launch, with
  instant live switching in the sidebar.
- **100% CLI parity across 6 workspaces**:
  1. **Overview** — Start, stop, or losslessly restart the gateway daemon,
     configure listen address & capacity limits, and inspect live health.
  2. **Telemetry** — Real-time SSE request stream, token throughput, warm ACP
     session pool inspector, and one-click session eviction.
  3. **Launchers** — Zero-pollution one-click launch (and reusable command copy)
     for **Codex CLI**, **Claude Code**, **Codex App**, and **Claude Desktop**,
     plus `~/.codex` / `~/.claude` config sync.
  4. **Models** — Searchable live Antigravity model catalog with instant
     hot-switching for both default and session-title models.
  5. **`agy` & Proxy** — Official `agy` SHA-512 install/update, interactive
     Google sign-in, outbound HTTP/SOCKS5 proxy enforcement & connectivity
     probe, and idle background auto-update toggle.
  6. **System** — Live gateway & `agy` stderr logs, environment doctor, `lg`
     `PATH` alias & shell completions installer, and release upgrade checker.

**Download the latest Desktop release — [Liftgate Desktop `v0.2.3`](https://github.com/acking-you/liftgate/releases/tag/v0.2.3-ui)**
([all Desktop releases](https://github.com/acking-you/liftgate/releases?q=Liftgate+Desktop) ·
[`SHA256SUMS`](https://github.com/acking-you/liftgate/releases/download/v0.2.3-ui/SHA256SUMS) ·
[`release-manifest.json`](https://github.com/acking-you/liftgate/releases/download/v0.2.3-ui/release-manifest.json)):

| Platform | Installer (Recommended) | Portable Bundle |
| --- | --- | --- |
| **macOS (Apple Silicon · M1–M4)** | [`liftgate-desktop-macos-arm64.dmg`](https://github.com/acking-you/liftgate/releases/download/v0.2.3-ui/liftgate-desktop-macos-arm64.dmg) | [`.zip`](https://github.com/acking-you/liftgate/releases/download/v0.2.3-ui/liftgate-desktop-macos-arm64.zip) |
| **macOS (Intel · x86_64)** | [`liftgate-desktop-macos-x86_64.dmg`](https://github.com/acking-you/liftgate/releases/download/v0.2.3-ui/liftgate-desktop-macos-x86_64.dmg) | [`.zip`](https://github.com/acking-you/liftgate/releases/download/v0.2.3-ui/liftgate-desktop-macos-x86_64.zip) |
| **Windows (x64)** | [`liftgate-desktop-windows-x86_64-setup.exe`](https://github.com/acking-you/liftgate/releases/download/v0.2.3-ui/liftgate-desktop-windows-x86_64-setup.exe) | [`.zip`](https://github.com/acking-you/liftgate/releases/download/v0.2.3-ui/liftgate-desktop-windows-x86_64.zip) |
| **Windows (ARM64)** | [`liftgate-desktop-windows-arm64-setup.exe`](https://github.com/acking-you/liftgate/releases/download/v0.2.3-ui/liftgate-desktop-windows-arm64-setup.exe) | [`.zip`](https://github.com/acking-you/liftgate/releases/download/v0.2.3-ui/liftgate-desktop-windows-arm64.zip) |
| **Linux (x86_64 · glibc 2.35+)** | [`.AppImage`](https://github.com/acking-you/liftgate/releases/download/v0.2.3-ui/liftgate-desktop-linux-x86_64.AppImage) · [`.deb` (amd64)](https://github.com/acking-you/liftgate/releases/download/v0.2.3-ui/liftgate-desktop-linux-amd64.deb) | [`.tar.gz`](https://github.com/acking-you/liftgate/releases/download/v0.2.3-ui/liftgate-desktop-linux-x86_64.tar.gz) |

> [!NOTE]
> macOS builds are currently ad-hoc signed; if Gatekeeper blocks first launch,
> run `xattr -cr /Applications/Liftgate.app` once after dragging `Liftgate.app`
> into `/Applications`. You can also build from source via
> `cargo install --path crates/liftgate-desktop --locked`.

## Quick start

Once **`lg`** (identical to `liftgate`) is installed — or directly from the
**`agy` & Proxy** and **Launchers** tabs inside **Liftgate Desktop** — install
the official `agy` CLI if needed, sign in, and launch your agent:

```bash
# 1. One-time Antigravity CLI setup & sign-in
lg agy install                          # download & SHA-512-verify official `agy` CLI
lg proxy 7890                           # optional: force proxy into `agy` (`lg proxy off` restores direct default)
lg agy login                            # sign in to Google Antigravity interactively via `agy acp --login`

# 2. Launch agents with zero config pollution (auto-starts background gateway)
lg                                      # interactive command center in your terminal
lg cx                                   # launch Codex CLI in current dir (0 side effects on exit)
lg cx resume                            # browse & resume Codex sessions (`--last`, `--all`, `fork`, `agents`)
lg cxa                                  # launch Codex App across Win/macOS/Linux (isolated profile)
lg cc                                   # launch Claude Code in current dir (0 side effects on exit)
lg cc -r                                # browse & resume Claude sessions (`lg cc resume`, `-c`, `fork`, `agents`)
lg cd                                   # launch Claude Desktop across Win/macOS/Linux (auto-restores on exit)
```

Monitor live traffic, hot-switch models, or sync persistent configs from anywhere:

```bash
lg agy                   # inspect `agy` version, login status, forced proxy & idle auto-update
lg top                   # full-screen real-time terminal dashboard (turns, pool, models, logs)
lg m -i                  # interactively filter & hot-switch default Antigravity model
lg up --open             # start background gateway + open embedded Web dashboard
lg st                    # health, PID, session pool, token metrics & active turns
lg cfg -g                # losslessly merge ~/.claude & ~/.codex configs (--uninstall to remove)
lg upgrade               # check GitHub Releases (--check) & self-upgrade `lg` + `liftgate`
lg down                  # gracefully stop the background gateway
```

Running straight from a source checkout via `./run-gateway.{sh,cmd}` works as
well. Full command reference and SDK examples:
[usage guide](docs/usage.md).

## What it does

- **Official interface only** — talks to Antigravity only through `agy acp`,
  the Agent Client Protocol the CLI ships for editors. Login, tokens, quota and
  TLS stay inside `agy` ([why ACP](docs/architecture.md#why-acp)).
- **Built-in `agy` setup, proxy & idle auto-update** — install `agy` with
  `lg agy install` (SHA-512 verified), sign in with `lg agy login`, optionally
  force an outbound proxy into `agy` with `lg proxy <port|url>` (default:
  direct, no proxy), and keep `agy` automatically updated while the gateway is
  idle (`lg agy auto-update`).
- **Two standard APIs** — OpenAI Responses and Anthropic Messages, streamed,
  with tool calls, thinking, images and token counting ([API](docs/api.md)).
- **Your agent keeps the tools** — ACP sessions run in an empty directory with
  no MCP servers, so file edits and shell commands stay in Codex or Claude
  Code; web search and an SSRF-safe web fetch run in the gateway
  ([inference-only sessions](docs/architecture.md#inference-only-sessions)).
- **Warm sessions** — each request's history is matched to a pooled ACP
  session and only the new turn is sent ([session reuse](docs/architecture.md#session-reuse)).
- **Verified context windows** — Codex and Claude Code compact cleanly within
  empirically verified ACP session windows (`500k / 425k` for Gemini, safely
  below Antigravity's `524,288`-token internal compaction threshold) with
  single-turn prompts and zero synthetic replay turns ([context policy](docs/usage.md#context-windows)).
- **Live model catalog** — `/v1/models` and a Codex catalog list every model
  your account can use, so `/model` pickers show Antigravity models
  ([Codex setup](docs/usage.md#codex-cli)).
- **No silent downgrade & Flash fast lanes** — upstream `429` rate limits surface
  immediately on the exact model you asked for without silent fallback, while
  titles, recaps and Auto Mode classifiers use the fastest
  Flash model without touching your main session ([model routing](docs/architecture.md#model-routing)).
- **Web & terminal dashboards** — inspect live turns, evict pooled sessions,
  switch models and tail `agy` logs either in the browser
  (`http://127.0.0.1:8787/`) or right inside your terminal with `lg top`
  ([dashboard](docs/usage.md#dashboard)).
- **Standalone GPUI desktop control center** — native `liftgate-desktop` app
  for macOS, Windows, and Linux with Warm Editorial Light/Dark themes and
  `中文` / `EN` UI ([download](#liftgate-desktop)).

## How it works

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/readme/how-it-works-dark.svg">
  <img src="assets/readme/how-it-works-light.svg" width="100%" alt="Codex CLI, Claude Code and SDKs send stateless HTTP requests to Liftgate, which translates them, reuses a warm session, and streams events back; it talks JSON-RPC over stdio to one agy acp process per session, and only agy holds the credentials for Google Antigravity.">
</picture>

Standard LLM APIs are stateless: every request carries the whole
conversation. ACP is a stateful editor session. Liftgate keeps one
`agy acp` process per session, forwards only what is new, and streams ACP
updates back as OpenAI or Anthropic events. See [architecture](docs/architecture.md).

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
