# Liftgate docs

Each page covers one topic: what it does, its limits and the rules a change
must keep. Start with the [usage guide](usage.md); contributor rules are in
[AGENTS.md](../AGENTS.md).

## Use

| Page | Covers |
| --- | --- |
| [Usage guide](usage.md) · [中文](usage.zh-CN.md) | running the gateway, launchers, Codex CLI and Claude Code setup, dashboard, flags, troubleshooting |
| [Remote gateway](remote.md) · [中文](remote.zh-CN.md) | running Liftgate on a remote server and connecting local `cx`, `omp`, `cc`, and `oc` via `lg remote` or copy-paste manual configs |
| [API](api.md) | routes, authentication, OpenAI Responses and Anthropic Messages support, models |

## Design

| Page | Covers |
| --- | --- |
| [Architecture](architecture.md) | why ACP, the request path, session reuse, model routing, process supervision, workspace layout |
| [MCP tool virtualization](mcp-tool-virtualization.md) | `agy acp` Eager tool budget adaptation, namespace action hubs, 0-RTT gateway-local `search_client_tools`, preflight self-healing |
| [`agy acp` 1.5.18 verification & elastic I/O plan](agy-acp-optimization-plan.md) | empirical `agy acp 1.5.18` protocol benchmarks, budget recalibration, and Bash-first / orchestrator-first read-write design |
| [Harness readability evolution (中文)](harness-readability-evolution.zh-CN.md) | four-stage evolution of Codex CLI work readability, comparisons with raw/Stage-1 harnesses (`Antigravity`) and Claude Code, and Liftgate's 0-RTT commentary + plan architecture |

## Project

| Page | Covers |
| --- | --- |
| [Rust engineering standard](2026-11-09-RUST.md) | the rules every Rust change follows |
| [Changelog](../CHANGELOG.md) | notable changes per release |
