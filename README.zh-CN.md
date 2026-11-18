<p align="center">
  <img src="packaging/icons/app_icon.svg" width="76" alt="Liftgate icon">
</p>

<h1 align="center">Liftgate</h1>

<p align="center">
  <strong>Lift</strong>&nbsp;+&nbsp;<strong>gate</strong>way · <em>把 Antigravity 托举进每一个智能体。</em>
</p>

<p align="center">
  <a href="https://github.com/acking-you/liftgate/releases/latest"><img alt="CLI release" src="https://img.shields.io/github/v/release/acking-you/liftgate?color=355ACF&label=CLI"></a>
  <a href="https://github.com/acking-you/liftgate/releases/tag/v0.2.1-ui"><img alt="Desktop release" src="https://img.shields.io/badge/Desktop-v0.2.1-D97757"></a>
  <a href="https://github.com/acking-you/liftgate/actions/workflows/syntax-check.yml"><img alt="CI" src="https://github.com/acking-you/liftgate/actions/workflows/syntax-check.yml/badge.svg"></a>
  <a href="https://www.rust-lang.org"><img alt="rust 1.100.0" src="https://img.shields.io/badge/rust-1.100.0-355ACF.svg"></a>
  <a href="#许可证"><img alt="license" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-355ACF.svg"></a>
</p>

<p align="center">
  <a href="README.md">English</a> · <strong>中文</strong>
</p>

> [!WARNING]
> Liftgate 尚未发布 1.0：参数、接口和默认值都可能不经通知而变化。欢迎反馈缺陷和意见。

Liftgate 在本机把 Google Antigravity 接到标准 OpenAI Responses 与 Anthropic Messages API 后面。Codex CLI、Codex App、Claude Code、Claude Desktop 或你自己的 SDK 代码无需任何改动，每一轮对话都严格通过官方 `agy acp` 协议在本地转发与流式应答。

既可以在终端使用 **`lg`** / **`liftgate`**（CLI 与全屏实时 TUI），也可以直接安装跨平台原生 GPUI 桌面应用 [**Liftgate Desktop**](#liftgate-desktop)（macOS / Windows / Linux）。

## 支持的客户端

<table>
  <tr>
    <td align="center" width="25%"><a href="docs/usage.zh-CN.md#codex-cli"><img src="assets/readme/openai.svg" width="36" alt=""><br><b>Codex CLI / App</b></a><br><sub>OpenAI Responses</sub></td>
    <td align="center" width="25%"><a href="docs/usage.zh-CN.md#claude-code"><img src="assets/readme/claude.svg" width="36" alt=""><br><b>Claude Code / Desktop</b></a><br><sub>Anthropic Messages</sub></td>
    <td align="center" width="25%"><a href="docs/api.md#openai-responses"><img src="assets/readme/openai.svg" width="36" alt=""><br><b>OpenAI SDK</b></a><br><sub><code>/v1/responses</code></sub></td>
    <td align="center" width="25%"><a href="docs/api.md#anthropic-messages"><img src="assets/readme/anthropic.svg" width="36" alt=""><br><b>Anthropic SDK</b></a><br><sub><code>/v1/messages</code></sub></td>
  </tr>
</table>

其它讲这两种 API 的 Harness 用同样方式接入（[API](docs/api.md)）。

## 安装与下载

### CLI 命令行（`lg` + `liftgate` 一键安装）

一条命令自动识别系统与 CPU 架构，从[最新 CLI Release](https://github.com/acking-you/liftgate/releases/latest) 下载预编译发行包（已内嵌 Web 监控面板）、校验 `SHA256SUMS`、原子安装 `lg` 与 `liftgate` 并写入 `PATH`：

**macOS / Linux / FreeBSD**

```bash
curl -fsSL https://raw.githubusercontent.com/acking-you/liftgate/main/scripts/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://raw.githubusercontent.com/acking-you/liftgate/main/scripts/install.ps1 | iex
```

支持环境变量自定义：`LIFTGATE_VERSION=v0.2.1` 指定版本标签、`LIFTGATE_BIN_DIR` 指定安装目录、`LIFTGATE_NO_MODIFY_PATH=1` 跳过修改 `PATH`。安装后随时运行 `lg upgrade` 即可原地校验升级。

<details>
<summary>其它 CLI 安装方式（<code>cargo binstall</code>、手动下载、源码编译）</summary>

```bash
# 通过 cargo-binstall 直接拉取预编译二进制
cargo binstall liftgate

# 从本地源码编译安装（Rust 1.100.0；装有 Bun 时自动编译并内嵌 Web 面板）
cargo install --path crates/liftgate --locked
```

也可直接在 [最新 CLI Release](https://github.com/acking-you/liftgate/releases/latest) 下载覆盖 17 个平台目标的预编译压缩包（Linux 静态 `musl` / `glibc`、macOS ARM64/Intel、Windows x64/ARM64/x86、FreeBSD）。

</details>

### Liftgate Desktop

**Liftgate Desktop**（`liftgate-desktop`）是基于 `gpui-kit` 构建的原生跨平台 GPUI 桌面控制中心——无 Electron/WebView 运行时开销，也无需单独安装外部 CLI（内置无头网关守护模式与内嵌 React Web 面板）。

- **Warm Editorial 暖调亮/暗双色主题 + 中英双语界面**：首次启动默认自动跟随当前系统深浅色外观与系统语言，侧边栏支持一键实时切换。
- **6 大工作区 100% 覆盖 CLI 全部能力**：
  1. **概览（Overview）** — 一键启停与无损重启本地网关、修改监听端口与并发容量限制、实时查看健康与运行状态。
  2. **遥测（Telemetry）** — SSE 实时请求流、Token 吞吐统计、预热 ACP 会话池检视与单会话一键驱逐。
  3. **启动器（Launchers）** — 零副作用一键启动（或复制可复用独立命令）**Codex CLI**、**Claude Code**、**Codex App** 与 **Claude Desktop**，以及项目级/全局 `~/.codex` 与 `~/.claude` 配置同步。
  4. **模型（Models）** — 实时检索账号可用 Antigravity 模型目录，一键热切换默认模型与会话标题模型。
  5. **`agy` 与代理（`agy` & Proxy）** — 官方 `agy` SHA-512 下载安装与更新、Google 交互式登录、HTTP/SOCKS5 代理注入与连通性探测、后台空闲自动热更新开关。
  6. **系统（System）** — 网关与 `agy` 实时日志流、环境全项自检（Doctor）、`lg` 命令别名与 Shell 补全安装、版本升级检测。

**下载最新桌面端版本 — [Liftgate Desktop `v0.2.1`](https://github.com/acking-you/liftgate/releases/tag/v0.2.1-ui)**
（[全部桌面端版本](https://github.com/acking-you/liftgate/releases?q=Liftgate+Desktop) ·
[`SHA256SUMS`](https://github.com/acking-you/liftgate/releases/download/v0.2.1-ui/SHA256SUMS) ·
[`release-manifest.json`](https://github.com/acking-you/liftgate/releases/download/v0.2.1-ui/release-manifest.json)）：

| 平台 | 安装包（推荐） | 免安装便携包 |
| --- | --- | --- |
| **macOS（Apple Silicon · M1–M4）** | [`liftgate-desktop-macos-arm64.dmg`](https://github.com/acking-you/liftgate/releases/download/v0.2.1-ui/liftgate-desktop-macos-arm64.dmg) | [`.zip`](https://github.com/acking-you/liftgate/releases/download/v0.2.1-ui/liftgate-desktop-macos-arm64.zip) |
| **macOS（Intel · x86_64）** | [`liftgate-desktop-macos-x86_64.dmg`](https://github.com/acking-you/liftgate/releases/download/v0.2.1-ui/liftgate-desktop-macos-x86_64.dmg) | [`.zip`](https://github.com/acking-you/liftgate/releases/download/v0.2.1-ui/liftgate-desktop-macos-x86_64.zip) |
| **Windows（x64）** | [`liftgate-desktop-windows-x86_64-setup.exe`](https://github.com/acking-you/liftgate/releases/download/v0.2.1-ui/liftgate-desktop-windows-x86_64-setup.exe) | [`.zip`](https://github.com/acking-you/liftgate/releases/download/v0.2.1-ui/liftgate-desktop-windows-x86_64.zip) |
| **Windows（ARM64）** | [`liftgate-desktop-windows-arm64-setup.exe`](https://github.com/acking-you/liftgate/releases/download/v0.2.1-ui/liftgate-desktop-windows-arm64-setup.exe) | [`.zip`](https://github.com/acking-you/liftgate/releases/download/v0.2.1-ui/liftgate-desktop-windows-arm64.zip) |
| **Linux（x86_64 · glibc 2.35+）** | [`.AppImage`](https://github.com/acking-you/liftgate/releases/download/v0.2.1-ui/liftgate-desktop-linux-x86_64.AppImage) · [`.deb` (amd64)](https://github.com/acking-you/liftgate/releases/download/v0.2.1-ui/liftgate-desktop-linux-amd64.deb) | [`.tar.gz`](https://github.com/acking-you/liftgate/releases/download/v0.2.1-ui/liftgate-desktop-linux-x86_64.tar.gz) |

> [!NOTE]
> 当前 macOS 安装包采用 ad-hoc 签名；首次拖入 `/Applications` 打开若遇到 Gatekeeper 拦截，执行一次 `xattr -cr /Applications/Liftgate.app` 即可。也可通过源码安装：`cargo install --path crates/liftgate-desktop --locked`。

## 快速开始

安装好 **`lg`**（与 `liftgate` 完全等价）后——或直接在 **Liftgate Desktop** 的 **`agy` 与代理** 与 **启动器** 页签中点击操作——即可完成 `agy` 安装、登录并零污染拉起智能体：

```bash
# 1. 首次准备官方 Antigravity CLI 并登录
lg agy install                          # 若本机尚未安装 `agy`，自动下载官方包并做 SHA-512 强校验安装
lg proxy 7890                           # 可选：为 `agy` 强制指定代理（默认不设任何代理，`lg proxy off` 关闭）
lg agy login                            # 通过官方 `agy acp --login` 交互式登录 Google Antigravity

# 2. 零配置污染启动智能体（网关未运行时自动在后台拉起）
lg                                      # 在终端打开交互式快捷控制中心
lg cx                                   # 在当前目录直接启动 Codex CLI（退出后 0 副作用）
lg cx resume                            # 浏览并恢复 Codex 会话（支持 `--last`、`--all`、`fork`、`agents`）
lg cxa                                  # 跨 Win/macOS/Linux 启动 Codex App（独立隔离 Profile）
lg cc                                   # 在当前目录直接启动 Claude Code（退出后 0 副作用）
lg cc -r                                # 浏览并恢复 Claude 会话（支持 `lg cc resume`、`-c`、`fork`、`agents`）
lg cd                                   # 跨 Win/macOS/Linux 启动 Claude Desktop（退出自动无损还原）
```

在任意目录下监控实时流量、热切换模型，或同步全局/项目级客户端配置：

```bash
lg agy                   # 查看 `agy` 版本、登录状态、强制代理与空闲自动更新策略
lg top                   # 打开全屏实时终端仪表盘（实时请求、会话池管理、模型切换、日志）
lg m -i                  # 交互式搜索并热切换默认 Antigravity 模型
lg up --open             # 后台启动网关服务并打开内嵌 Web 监控面板
lg st                    # 查看健康状态、PID、会话池、Token 统计与进行中轮次
lg cfg -g                # 无损合并 ~/.claude 与 ~/.codex 全局配置（--uninstall 卸载）
lg upgrade               # 检查 GitHub Releases（--check）并自动校验升级 `lg` + `liftgate`
lg down                  # 优雅停止后台网关服务
```

在源码仓库中继续使用 `./run-gateway.{sh,cmd}` 启动网关同样完全兼容。完整子命令说明、参数列表与 SDK 接入方式见[使用指南](docs/usage.zh-CN.md)。

## 能做什么

- **只走官方接口**：只通过 `agy acp`（CLI 为编辑器提供的 Agent Client Protocol）与 Antigravity 通信，登录、Token、配额与 TLS 都留在 `agy` 内部（[为什么是 ACP](docs/architecture.md#why-acp)）。
- **内置 `agy` 一键安装、代理注入与空闲自动更新**：缺失 `agy` 时用 `lg agy install` 跨平台自动下载并做 SHA-512 校验，用 `lg agy login` 完成交互登录；默认**不设置任何代理**，需要时一条 `lg proxy <端口|URL>` 即可在 `agy` 启动时强制注入代理；网关运行期间默认在空闲时自动检测并无感热更新 `agy`（可通过 `lg agy auto-update` 随时开关）。
- **两套标准 API**：OpenAI Responses 与 Anthropic Messages，支持流式输出、工具调用、思考过程、图片与 Token 计数（[API](docs/api.md)）。
- **工具留在你的智能体里**：ACP 会话运行在空目录且不挂任何 MCP 服务，文件编辑与命令执行仍由 Codex 或 Claude Code 完成；联网搜索与防 SSRF 的网页抓取由网关处理（[纯推理会话](docs/architecture.md#inference-only-sessions)）。
- **会话预热复用**：按每次请求的历史匹配池中已预热的 ACP 会话，只发送本轮新增内容（[会话复用](docs/architecture.md#session-reuse)）。
- **稳妥的上下文默认配置**：Codex 与 Claude Code 按模型窗口提前压缩；Flash 历史分块预灌仅作为显式实验选项，不能绕过上游自动压缩（[上下文策略](docs/usage.zh-CN.md#上下文窗口与超长上下文选项)）。
- **实时模型目录**：`/v1/models` 与 Codex 模型目录列出账号下全部可用模型，`/model` 菜单直接显示 Antigravity 模型（[接入 Codex](docs/usage.zh-CN.md#codex-cli)）。
- **拒绝静默降级与 Flash 快车道**：上游 `429` 限流立即返回，绝不暗中偷换模型；标题、摘要与 Auto Mode 分类走独立的 Flash 会话，不打扰主会话（[模型路由](docs/architecture.md#model-routing)）。
- **Web 与终端双监控面板**：既可在浏览器 `http://127.0.0.1:8787/` 打开内嵌 React 面板，也可直接在终端运行 `lg top` 全屏实时查看对话流、驱逐空闲会话、热切换模型与追踪日志（[监控面板](docs/usage.zh-CN.md#监控面板)）。
- **原生 GPUI 独立桌面端**：跨平台 `liftgate-desktop` 桌面控制中心，采用 Warm Editorial 亮/暗双色主题与中英双语界面（[前往下载](#liftgate-desktop)）。

## 工作原理

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/readme/how-it-works-dark.zh-CN.svg">
  <img src="assets/readme/how-it-works-light.zh-CN.svg" width="100%" alt="Codex CLI、Claude Code 与 SDK 以无状态 HTTP 请求访问 Liftgate；Liftgate 负责转换、复用预热会话并流式回传，经 stdio 上的 JSON-RPC 与每会话一个的 agy acp 进程通信，只有 agy 持有访问 Google Antigravity 的凭据。">
</picture>

标准 LLM API 是无状态的：每次请求都携带完整对话。ACP 则是有状态的编辑器会话。Liftgate 为每个会话保持一个 `agy acp` 进程，只转发新增内容，再把 ACP 更新流式转换成 OpenAI 或 Anthropic 事件。详见[架构](docs/architecture.md)。

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
