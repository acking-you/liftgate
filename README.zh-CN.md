<h1 align="center">Liftgate</h1>

<p align="center">
  <strong>Lift</strong>&nbsp;+&nbsp;<strong>gate</strong>way · <em>把 Antigravity 托举进每一个智能体。</em>
</p>

<p align="center">
  <a href="CHANGELOG.md"><img alt="status: work in progress" src="https://img.shields.io/badge/status-WIP-626C7E"></a>
  <a href="https://github.com/acking-you/antigravity-gateway/actions/workflows/syntax-check.yml"><img alt="CI" src="https://github.com/acking-you/antigravity-gateway/actions/workflows/syntax-check.yml/badge.svg"></a>
  <a href="https://www.rust-lang.org"><img alt="rust 1.85+" src="https://img.shields.io/badge/rust-1.85%2B-355ACF.svg"></a>
  <a href="#许可证"><img alt="license" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-355ACF.svg"></a>
</p>

<p align="center">
  <a href="README.md">English</a> · <strong>中文</strong>
</p>

> [!WARNING]
> Liftgate 尚未发布 1.0：参数、接口和默认值都可能不经通知而变化。欢迎反馈缺陷和意见。

Liftgate 把 Google Antigravity 接到 OpenAI 与 Anthropic 的标准 API 后面。Codex CLI、Claude Code 或你自己的 SDK 代码照原样使用；Liftgate 在本机应答它们，每一轮对话都交给官方 `agy` CLI 执行。

## 支持的客户端

<table>
  <tr>
    <td align="center" width="25%"><a href="docs/usage.zh-CN.md#codex-cli"><img src="assets/readme/openai.svg" width="36" alt=""><br><b>Codex CLI</b></a><br><sub>OpenAI Responses</sub></td>
    <td align="center" width="25%"><a href="docs/usage.zh-CN.md#claude-code"><img src="assets/readme/claude.svg" width="36" alt=""><br><b>Claude Code</b></a><br><sub>Anthropic Messages</sub></td>
    <td align="center" width="25%"><a href="docs/api.md#openai-responses"><img src="assets/readme/openai.svg" width="36" alt=""><br><b>OpenAI SDK</b></a><br><sub><code>/v1/responses</code></sub></td>
    <td align="center" width="25%"><a href="docs/api.md#anthropic-messages"><img src="assets/readme/anthropic.svg" width="36" alt=""><br><b>Anthropic SDK</b></a><br><sub><code>/v1/messages</code></sub></td>
  </tr>
</table>

其它讲这两种 API 的 Harness 用同样方式接入（[API](docs/api.md)）。

## 能做什么

- **只走官方接口**：只通过 `agy acp`（CLI 为编辑器提供的 Agent Client Protocol）与 Antigravity 通信，登录、Token、配额与 TLS 都留在 `agy` 内部（[为什么是 ACP](docs/architecture.md#why-acp)）。
- **两套标准 API**：OpenAI Responses 与 Anthropic Messages，支持流式输出、工具调用、思考过程、图片与 Token 计数（[API](docs/api.md)）。
- **工具留在你的智能体里**：ACP 会话运行在空目录且不挂任何 MCP 服务，文件编辑与命令执行仍由 Codex 或 Claude Code 完成；联网搜索与防 SSRF 的网页抓取由网关处理（[纯推理会话](docs/architecture.md#inference-only-sessions)）。
- **会话预热复用**：按每次请求的历史匹配池中已预热的 ACP 会话，只发送本轮新增内容（[会话复用](docs/architecture.md#session-reuse)）。
- **实时模型目录**：`/v1/models` 与 Codex 模型目录列出账号下全部可用模型，`/model` 菜单直接显示 Antigravity 模型（[接入 Codex](docs/usage.zh-CN.md#codex-cli)）。
- **容灾与快车道**：上游返回 `429` 时立即切换备用模型；标题、摘要与 Auto Mode 分类等辅助请求走最快的 Flash 模型，不打扰主会话（[模型路由](docs/architecture.md#model-routing)）。
- **实时监控面板**：在 `http://127.0.0.1:8787/` 查看进行中的任务、会话池、Token 用量、模型与 `agy` 日志（[监控面板](docs/usage.zh-CN.md#监控面板)）。

## 工作原理

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/readme/how-it-works-dark.zh-CN.svg">
  <img src="assets/readme/how-it-works-light.zh-CN.svg" width="100%" alt="Codex CLI、Claude Code 与 SDK 以无状态 HTTP 请求访问 Liftgate；Liftgate 负责转换、复用预热会话、流式回传并在 429 时容灾，经 stdio 上的 JSON-RPC 与每会话一个的 agy acp 进程通信，只有 agy 持有访问 Google Antigravity 的凭据。">
</picture>

标准 LLM API 是无状态的：每次请求都携带完整对话。ACP 则是有状态的编辑器会话。Liftgate 为每个会话保持一个 `agy acp` 进程，只转发新增内容，再把 ACP 更新流式转换成 OpenAI 或 Anthropic 事件。详见[架构](docs/architecture.md)。

## 快速开始

使用 Antigravity CLI（`agy`）完成一次登录，全局安装单个 `liftgate` 二进制（预编译发行版或 `cargo install`），即可通过同一命令完成全部管理与启动：

```bash
agy acp --login                         # 首次使用时登录 Antigravity
cargo install --path crates/liftgate    # 若本机装有 Bun 会自动编译并内嵌 Web 监控面板

liftgate start --open                   # 后台启动网关服务并打开监控面板（http://127.0.0.1:8787/）
liftgate codex                          # 在当前目录直接启动 Codex CLI（退出后 0 副作用）
liftgate claude                         # 或在当前目录直接启动 Claude Code（退出后 0 副作用）
```

在任意目录下管理后台服务、查看日志，或生成/卸载项目级与全局客户端配置：

```bash
liftgate status          # 查看健康状态、PID、会话池、Token 用量与模型数
liftgate logs -f         # 实时追踪后台网关日志
liftgate ui              # 在浏览器打开内嵌 Web 监控面板
liftgate config          # 生成/合并 .claude/ 与 .codex/ 配置（--global 写全局，--uninstall 卸载）
liftgate stop            # 优雅停止后台网关服务
```

在源码仓库中继续使用 `./run-gateway.{sh,cmd}` 与 `./start-agy.{sh,cmd}` 同样完全兼容。完整子命令说明、参数列表与 SDK 接入方式见[使用指南](docs/usage.zh-CN.md)。

## 文档

| | |
| --- | --- |
| **使用** | [使用指南](docs/usage.zh-CN.md) · [Codex CLI](docs/usage.zh-CN.md#codex-cli) · [Claude Code](docs/usage.zh-CN.md#claude-code) · [监控面板](docs/usage.zh-CN.md#监控面板) · [参数](docs/usage.zh-CN.md#参数与环境变量) |
| **参考** | [API](docs/api.md) · [架构](docs/architecture.md) |
| **项目** | [更新日志](CHANGELOG.md) · [Rust 规范](docs/2026-11-09-RUST.md) · [AGENTS.md](AGENTS.md) |

全部文档：[文档索引](docs/README.md)（除使用指南外多为英文）。

## 参与贡献

构建、检查与测试命令、架构约束以及每次改动都要遵循的 Rust 规范见 [AGENTS.md](AGENTS.md)；用户可见的改动请记录到 [CHANGELOG.md](CHANGELOG.md)。

## 许可证

[Apache License 2.0](LICENSE-APACHE) 或 [MIT](LICENSE-MIT) 双协议，任选其一。`assets/readme/` 中的客户端图标来自 Simple Icons（CC0），商标归各自所有者（[说明](assets/readme/NOTICE.txt)）。
