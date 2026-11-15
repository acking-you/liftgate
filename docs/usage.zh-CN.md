# 使用指南

[English](usage.md) · 中文

启动 Liftgate，然后把 Codex CLI、Claude Code 或任意 OpenAI / Anthropic 客户端指向它。下文地址均以默认的 `http://127.0.0.1:8787` 为例。

## 准备工作

- **全局安装 `lg` / `liftgate`**：从 [GitHub Releases](https://github.com/acking-you/liftgate/releases) 下载对应平台发行包放入 `PATH`（或运行 `cargo binstall liftgate`），亦可使用 **Rust 1.100.0** 从源码全局安装：
  ```bash
  cargo install --path crates/liftgate --locked
  ```
  编译 Release 时若本机装有 **Bun**，`build.rs` 会自动构建 `web/` 并将完整 React 监控面板直接内嵌进 `liftgate` 单二进制中（运行时无需外部 `web/dist` 目录）；未安装 Bun 时所有 API 照常可用，且 `/` 仍提供内置的轻量级实时遥测页面。
- **Antigravity CLI（`agy`）**：若本机尚未安装 `agy`，直接执行 `lg agy install` 即可自动下载官方包并完成 SHA-512 校验安装，再执行一次 `lg agy login` 完成登录（底层调用官方 `agy acp --login`）。Liftgate 会在 `PATH`、标准安装目录及 `~/.liftgate/settings.json` 中自动定位它，也可通过 `--agy-bin <路径>` 或 `AGY_BIN` 指定。

## 全局服务管理（`lg` / `liftgate` CLI）

发行包与 `cargo install` 默认同时安装完整命令 `liftgate` 与短别名 `lg`（也可通过 `liftgate alias --name <名称>` 安装自定义短别名）。可在任意目录下使用全屏 TUI、启停后台网关与查看状态（状态与日志默认保存在 `~/.liftgate`，可通过 `--state-dir` 或 `LIFTGATE_HOME` 指定）：

```bash
lg                              # 在交互式终端打开快捷控制中心
lg agy                          # 查看 agy 是否存在、版本、登录状态、强制代理与空闲自动更新设置
lg agy install                  # 跨平台下载官方 agy 并完成 SHA-512 校验与安装
lg agy login                    # 调用官方 `agy acp --login` 交互式登录 Google 账号
lg proxy 7890                   # 为 agy 强制设置代理（默认不设任何代理，`lg proxy off` 恢复直连）
lg top                          # 打开全屏实时 TUI 监控仪表盘（别名：tui、dash、monitor）
lg up                           # 后台启动网关与内嵌 Web 监控面板（全称：start）
lg up --open                    # 后台启动并自动在浏览器打开监控面板
lg st                           # 查看健康状态、PID、会话池、Token 统计与进行中轮次（全称：status，支持 --json）
lg l -n 100 -f                  # 查看并实时跟随后台日志（全称：logs，加 --acp 查看 agy stderr）
lg web                          # 打开 Web 监控面板（全称：ui，加 --print 仅输出地址）
lg m -i                         # 交互式搜索并热切换默认模型（全称：models --pick）
lg doc                          # 自检 agy、claude、codex、代理、内嵌面板与登录模型就绪状态（全称：doctor）
lg r                            # 重启后台网关（全称：restart）
lg down                         # 优雅停止后台网关并清理 ACP 子进程树（全称：stop）
lg upgrade                      # 检查 GitHub Releases（-c / --check）并自动校验升级 lg 与 liftgate（别名：update、self-update）
lg alias --name lg              # 在 PATH 中安装或移除（--uninstall）短命令别名
lg completions powershell       # 生成 Shell 自动补全脚本（bash、zsh、fish、powershell、elvish）
liftgate serve                  # 前台运行网关
```

## Antigravity CLI 管理（`lg agy`）与代理配置（`lg proxy`）

无需手动查找安装包、拼环境变量或单独维护更新，直接通过 `lg` 即可完成 `agy` 的全生命周期管理：

```bash
# 1. 查看 agy 是否已安装、当前版本、上游最新版本、Google 登录状态、代理与自动更新开关
lg agy                                  # 支持 `--json`、`lg agy status --offline`

# 2. 跨平台安装或手动立即更新 agy（支持 Windows、macOS、Linux glibc/musl、Android）
lg agy install                          # 从官方 Antigravity 清单下载并做 SHA-512 强校验
lg agy update                           # 立即更新到最新版（若后台网关正在运行会自动热重启）

# 3. 便捷设置强制注入给 agy 的网络代理（默认：不设置任何代理，直连）
lg proxy                                # 查看当前代理状态（短别名：`lg px`）
lg proxy 7890                           # 端口简写 -> 自动规范化为 http://127.0.0.1:7890
lg proxy socks5h://127.0.0.1:1080       # 支持 http://、https://、socks5://、socks5h:// 与 host:port
lg proxy system                         # 显式读取并导入一次当前系统/环境变量代理
lg proxy test                           # 测试通过当前代理策略访问官方 Antigravity 更新源
lg proxy off                            # 关闭代理，恢复默认的无代理直连模式

# 4. 配置网关运行期间的 agy 空闲自动更新（默认：开启）
lg agy auto-update                      # 查看当前自动更新开关与最近一次自动更新记录
lg agy auto-update off                  # 关闭后台空闲自动更新
lg agy auto-update on                   # 开启后台空闲自动更新（默认）

# 5. 通过 lg 直接执行官方 agy CLI 操作（自动套用已配置的强制代理）
lg agy login                            # 执行 `agy acp --login`（加 `--install` 可在缺失时先自动安装）
lg agy models                           # 执行 `agy models` 查看账号已解锁模型
lg agy mcp -- --help                    # 透传任意官方子命令（`mcp`、`plugin`、`changelog`、`run` 等）
```

- **默认零代理直连**：Liftgate 默认**不设置任何代理**，并在拉起 `agy` 时清理父进程残留的 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`（也不会自动注入系统注册表代理）。只有当你通过 `lg proxy`（持久化于 `~/.liftgate/settings.json`）或 `--agy-proxy` / `AGY_PROXY` 显式配置后，Liftgate 才会在每次启动 `agy` 时强制注入代理环境变量，并自动将 `127.0.0.1,localhost,::1` 写入 `NO_PROXY`。
- **空闲无感自动热更新**：网关运行时默认开启 `agy` 自动更新（`true`）。后台任务会定期检查官方 Antigravity 清单；发现新版本时**绝不打断进行中的请求**，而是等待网关完全空闲（0 活跃请求、0 活跃流且会话池静置），排空缓存空闲会话并优雅释放旧 `agy` 进程文件锁（含 Windows `.exe` 锁），完成 SHA-512 校验替换后立即拉起新进程——若新版本握手异常还会自动回滚旧二进制。

## 零副作用临时启动（`liftgate codex` / `liftgate claude`）

在**任意项目目录**下直接通过 Liftgate 注入的进程级环境启动 Codex CLI 或 Claude Code，退出后**完全 0 副作用**（不在当前项目生成任何文件，也不改动 `~/.claude/settings.json`、`~/.codex/config.toml` 或父终端环境变量）：

```bash
cd /path/to/your/project
liftgate codex                              # 在当前目录启动交互式 Codex CLI（默认开启 Code Mode：`exec` + `wait`）
liftgate codex --code-mode-only             # 启动纯 Code Mode（仅暴露 `exec` + `wait`）的 Codex CLI
liftgate codex --no-code-mode               # 当前会话关闭 Code Mode，恢复直连工具模式（`direct`）
liftgate claude                             # 在当前目录启动交互式 Claude Code（默认注入 `CLAUDE_CODE_EXPERIMENTAL_CODE_MODE=1`）
liftgate codex exec "运行 cargo test"        # 原生子命令与参数直接透传
liftgate claude --model gemini-3.8-flash-low -p "总结最近一次提交"
```

- 若后台网关尚未启动，`liftgate codex` 与 `liftgate claude` 会自动在后台拉起它。
- 注入的环境变量与临时配置目录（位于 `~/.liftgate/ephemeral/`）仅对子进程生效，进程退出时立即自动删除。
- 默认开启全自动免确认模式（Codex 传 `--dangerously-bypass-approvals-and-sandbox`，Claude Code 传 `--dangerously-skip-permissions`）；加 `--no-bypass`（或显式传入权限/沙箱参数）即可保留常规权限确认。

## 上下文窗口与超长上下文选项

推荐通过 `lg cx` 或 `lg cc` 读取运行中网关的上下文策略。默认复用热 ACP 会话，由客户端提前压缩，不插入 Flash 预灌回合：

| 模型系列 | 声明窗口 | 自动压缩阈值 |
| --- | --- | --- |
| Gemini | 280,000 | 245,000 |
| Claude | 200,000 | 170,000 |
| GPT-OSS | 128,000 | 110,000 |

这是网关的运行窗口，并非模型宣传的原生容量，目的是为实测约 300k–320k tokens 的上游压缩行为留出余量。Codex 按模型目录读取配置；Claude Code 使用逐模型 `modelSettings.autoCompactWindow`（需 v2.1.288 或更新版本）。多个网关模型映射到同一个 Claude canonical 名称时取较小阈值，因此可能早于表中数值压缩。参考[Claude 官方上下文配置](https://code.claude.com/docs/en/model-config#context-window-and-auto-compaction)。

每次 ACP 请求有 `176,000` 字节的单轮文本预算。当某次工具调用输出或单轮输入超出预算时，Liftgate 会按水位线分配算法仅对超长块做按行对齐的首尾保留中间截断（保留约 55% 开头与 45% 结尾，并插入明确省略行号范围的提示标记），保持同轮其它小工具输出完整无损，且不影响后续轮次的 `SessionPool` 热会话前缀命中。冷启动回放会在必要时带标记缩减旧历史；真正发生上下文窗口溢出时，网关会返回客户端标准可自动触发压缩的错误格式（Codex 为 `context_length_exceeded`，Claude Code 为 `prompt is too long: N tokens > M maximum`）。客户端 `/compact` 会优先复用匹配的热会话完成总结，再释放旧会话。

超长上下文是需要显式开启的实验功能：

```bash
lg start --long-context          # 网关尚未运行时开启
lg restart --long-context        # 已运行的托管网关切换到实验模式
lg restart --long-context=false  # 恢复稳妥默认模式
```

也可在启动网关前设置 `AGY_LONG_CONTEXT=true`，或在 `~/.liftgate/settings.json` 添加 `"long_context": true`，用于 `lg stop` 后的下一次 `lg start`。裸 `lg restart` 会保留当前守护进程的模式；已有服务应明确执行 `lg restart --long-context` 或 `lg restart --long-context=false` 切换。随后重新打开客户端；使用持久配置文件时，再执行 `lg config` 更新文件。

实验模式为 Gemini 声明 `1M / 850k`，通过有数量和大小上限的 Flash 回合预灌较长历史，会增加调用并产生合成确认消息。它**不能绕过上游自动压缩，也不保证保留 1M 对话**。超过分片容量，或在预灌过程中检测到上游压缩时，返回不可重试的 `400`，避免掉段或重复重放。Claude 与 GPT-OSS 在此模式下仍保持较小窗口。

## 快捷生成本地或全局配置文件（`liftgate config`）

如果你希望直接输入裸命令 `claude` 或 `codex` 使用网关，可用 `liftgate config`（别名 `liftgate setup`）一键生成或无损合并本地/全局配置文件：

```bash
liftgate config                     # 在当前项目生成/合并 .claude/settings.local.json 与 .codex/{config.toml,agy-models.json}
liftgate config claude --global     # 无损合并到全局 ~/.claude/settings.json
liftgate config codex --global      # 无损合并 [model_providers.agy] 与模型目录到全局 ~/.codex/
liftgate config --print             # 仅在终端预览将生成的配置内容，不写盘
liftgate config --uninstall         # 精准清理 Liftgate 写入的配置项（加 --global 清理全局）
```

合并时会完整保留用户原有的无关配置（如 `mcpServers`、`hooks`、`[mcp_servers.*]`、`[projects.*]`、自定义环境变量等），并在首次修改已有文件前自动备份为 `*.liftgate.bak`。

## 源码目录脚本启动

`run-gateway` 会在装有 Bun 且尚未构建时先构建 `web/dist`，再用 `cargo build --release --locked` 编译 `liftgate` 并启动。

```bash
./run-gateway.sh                  # 前台运行，Ctrl+C 停止
./run-gateway.sh --daemon         # 后台运行；PID 与日志在 .agy-runtime/
./run-gateway.sh --stop           # 停止后台网关
./run-gateway.sh --listen 127.0.0.1:8787 --default-model gemini-4-carbon-high --api-key local-agy
```

```powershell
.\run-gateway.cmd                 # CMD 或 PowerShell 均可
.\run-gateway.cmd -Daemon
.\run-gateway.cmd -Stop
```

| 脚本选项 | 作用 |
| --- | --- |
| `--daemon` · `-Daemon` | 后台运行，并等待 `/healthz` 就绪 |
| `--stop` · `-Stop` | 停止后台网关 |
| `--skip-build` · `-SkipBuild` | 不重新编译，直接启动现有 Release 二进制 |
| `--rebuild-web` · `-RebuildWeb` | 即使 `web/dist` 已存在也重新构建 |
| `-h` · `--help` | 查看脚本帮助 |

其它参数原样传给 `liftgate`（见[参数与环境变量](#参数与环境变量)）。用 `curl http://127.0.0.1:8787/healthz` 确认已启动，用 `curl http://127.0.0.1:8787/v1/models` 查看可用模型。

## 一键启动器

`start-agy` 会在网关未运行时自动启动它，同步实时模型列表，再打开已指向网关的 Claude Code 或 Codex。配置只作用于它启动的进程，退出后终端环境保持原样。

| | Linux / macOS | Windows |
| --- | --- | --- |
| Claude Code | `./start-agy.sh` | `.\start-agy.cmd` |
| Codex CLI | `./start-agy.sh --codex` | `.\start-agy.cmd -Codex` |
| 标准权限确认模式 | `./start-agy.sh --no-bypass` | `.\start-agy.cmd -NoBypass` |
| 只启动服务 | `./start-agy.sh --services-only` | `.\start-agy.cmd -BackgroundOnly` |
| 停止服务 | `./start-agy.sh --stop` | `.\start-agy.cmd -StopServices` |

- 默认以全自动免确认模式启动 Claude Code（`--dangerously-skip-permissions`）与 Codex（`--dangerously-bypass-approvals-and-sandbox`）；加 `--no-bypass` / `-NoBypass`（或显式传入 `--permission-mode` / `--sandbox`）即可保留常规权限确认。
- 其余参数原样传给 `claude` 或 `codex`（如 `./start-agy.sh -p "你好"`）。
- 修改默认值：可用参数 `--gateway-model`（`-GatewayModel`）、`--fast-model`（`-FastModel`）、`--host`（`-ListenHost`）、`--port`（`-ListenPort`）、`--ui-port`（`-UiPort`）、`--api-key`（`-ApiKey`），或环境变量 `AGY_MODEL`、`AGY_FAST_MODEL`、`AGY_HOST`、`AGY_PORT`、`AGY_WEB_PORT`、`AGY_API_KEY`。
- 装有 Bun 时还会在 `5173` 端口启动监控面板的开发服务器。
- 只在仓库目录内写文件：`.agy-runtime/`（日志、PID、Codex 模型目录）、`claude-carbon.settings.json` 与 `.claude/settings.local.json`。
- Windows 上可显式加 `-InstallEnv` 把变量写入用户环境，之后新开终端里的 `claude` 都走 Liftgate；`-UninstallEnv` 清除它们。

## Codex CLI

推荐使用 `lg cx` 自动配置。手动接入时，**1. 导出模型目录**，让 Codex 的 `/model` 获取正确的逐模型窗口与压缩策略：

```bash
mkdir -p ~/.codex
curl -fsS "http://127.0.0.1:8787/v1/codex-models?default_model=gemini-4-carbon-high" -o ~/.codex/agy-models.json
```

```powershell
New-Item -ItemType Directory -Force -Path "$HOME\.codex" | Out-Null
Invoke-WebRequest "http://127.0.0.1:8787/v1/codex-models?default_model=gemini-4-carbon-high" -OutFile "$HOME\.codex\agy-models.json"
```

**2. 添加 Provider**，编辑 `~/.codex/config.toml`（Windows 为 `%USERPROFILE%\.codex\config.toml`）：

```toml
model = "gemini-4-carbon-high"
model_provider = "agy"
# 第一步导出的绝对路径；Windows 请用正斜杠，如 "C:/Users/你的用户名/.codex/agy-models.json"
model_catalog_json = "/Users/你的用户名/.codex/agy-models.json"
web_search = "live"

[model_providers.agy]
name = "Liftgate"
base_url = "http://127.0.0.1:8787/v1"
wire_api = "responses"
# 若网关以 --api-key <密钥> 启动，取消下一行注释并设置环境变量 AGY_GATEWAY_API_KEY=<密钥>：
# env_key = "AGY_GATEWAY_API_KEY"
```

**3. 运行 `codex`。** 如果终端开启了 HTTP 代理，先让本地回环地址绕过代理：`export NO_PROXY="127.0.0.1,localhost,::1"`。

迁移已有配置时，删除旧的统一 `model_context_window` / `model_auto_compact_token_limit` 覆盖值，或重新执行 `lg config codex --global` 更新托管配置项。新版模型目录的 `max_context_window` 也会在当前 Codex 版本中限制旧全局覆盖；旧客户端可能不支持此上限，仍建议清理残留覆盖。导出的模型目录应与网关当前模式保持一致。

**Code Mode（`exec` + `wait`）**：Liftgate 已完整适配最新 Codex Code Mode（完整保留 `exec` 内嵌的 `declare const tools` TypeScript 类型声明，超过 16 KB `systemPrompt.append` 上限时无损溢出至首轮 `<client_tools_update>`，流式下发带 `call_id` 的 `response.custom_tool_call_input.delta`，支持 `wait` 异步单元轮询、图文/音频输出、`tool_search`、`configuration_update` 热切换推理强度，并将 Guardian 嵌套安全审查分流至极速临时 Flash 会话）。可直接用 `lg cx --code-mode`（混合模式）或 `lg cx --code-mode-only`（纯 Code Mode）启动，也可手动传 `-c features.code_mode=true`（导出 `/v1/codex-models` 时还支持可选参数 `?tool_mode=code_mode`）。

## Claude Code

推荐使用 `lg cc` 获取逐模型设置。下列手工示例对所有模型采用保守的 110k 自动压缩窗口，仅作用于当前终端：

```bash
export ANTHROPIC_BASE_URL="http://127.0.0.1:8787"
export ANTHROPIC_AUTH_TOKEN="local-agy"
export ANTHROPIC_MODEL="gemini-4-carbon-high"
export ANTHROPIC_SMALL_FAST_MODEL="gemini-3.8-flash-low"
export CLAUDE_CODE_AUTO_COMPACT_WINDOW="110000"
export NO_PROXY="127.0.0.1,localhost,::1"
claude
```

```powershell
$env:ANTHROPIC_BASE_URL = "http://127.0.0.1:8787"
$env:ANTHROPIC_AUTH_TOKEN = "local-agy"
$env:ANTHROPIC_MODEL = "gemini-4-carbon-high"
$env:ANTHROPIC_SMALL_FAST_MODEL = "gemini-3.8-flash-low"
$env:CLAUDE_CODE_AUTO_COMPACT_WINDOW = "110000"
$env:NO_PROXY = "127.0.0.1,localhost,::1"
claude
```

或者持久化到 `~/.claude/settings.json`（或项目级 `.claude/settings.local.json`）：

```json
{
  "model": "gemini-4-carbon-high",
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:8787",
    "ANTHROPIC_AUTH_TOKEN": "local-agy",
    "ANTHROPIC_MODEL": "gemini-4-carbon-high",
    "ANTHROPIC_SMALL_FAST_MODEL": "gemini-3.8-flash-low",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "110000",
    "NO_PROXY": "127.0.0.1,localhost,::1"
  }
}
```

网关未设置 `--api-key` 时，任意非空的 `ANTHROPIC_AUTH_TOKEN` 都能通过 Claude Code 的本地检查；设置了 `--api-key <密钥>` 时，请填对应密钥。

> **关于 Claude Code Mode（`REPL`）的说明**：Liftgate 的 `/v1/messages` 协议层已完整适配 `REPL` 沙箱工具定义保留与引号容错解析、`claude-fable-*` 模型族映射、`container` / `allowed_callers` / `caller` 字段以及 Auto Mode 代码审查分类器分流。但需要注意：Claude Code 官方客户端（`v2.1.323+`）目前在本地硬性限制了设置 `ANTHROPIC_BASE_URL` 或 `ANTHROPIC_AUTH_TOKEN` 时不启用客户端 Code Mode（详见[官方文档](https://code.claude.com/docs/en/code-mode)）；待上游放开自定义网关限制后即可直接无缝使用。

## 其它客户端

OpenAI Responses 客户端指向 `http://127.0.0.1:8787/v1`，Anthropic Messages 客户端指向 `http://127.0.0.1:8787`（见 [API](api.md)）。

## 监控面板

Liftgate 直接在 `http://127.0.0.1:8787/` 提供实时监控面板（Release 二进制已在编译期内置完整面板，也可直接执行 `liftgate ui` 打开）：展示进行中与最近完成的任务（含工具调用与 Token 明细）、会话池（可手动关闭空闲会话）、实时模型目录以及 `agy` 最近的 stderr 日志。启用了 `--api-key` 时，`liftgate ui` 会自动携带密钥完成连接；也可通过 `--ui-listen <地址>` 额外监听独立前端端口，或用 `--ui-dir <目录>` 覆盖静态资源目录。

## 参数与环境变量

| 参数 | 环境变量 | 默认值 |
| --- | --- | --- |
| `--state-dir` | `LIFTGATE_HOME` | `~/.liftgate` |
| `--listen` | `AGY_GATEWAY_LISTEN` | `127.0.0.1:8787` |
| `--ui-listen` | `AGY_UI_LISTEN` | 无（直接由 `--listen` 同端口提供） |
| `--agy-bin` | `AGY_BIN` | 从 `PATH` 或标准安装位置查找 |
| `--agy-proxy`（`<端口\|URL>` 或 `off`） | `AGY_PROXY` | 无（直连；或读取 `~/.liftgate/settings.json` 中的 `agy_proxy`） |
| `--agy-auto-update` / `--no-agy-auto-update` | `AGY_AUTO_UPDATE` | `true`（或读取 `~/.liftgate/settings.json` 中的 `agy_auto_update`） |
| `--long-context[=true\|false]` | `AGY_LONG_CONTEXT` | `false`（或读取 `~/.liftgate/settings.json` 中的 `long_context`） |
| `--default-model`（请求未知或通用模型名时使用） | `AGY_DEFAULT_MODEL` | `gemini-4-carbon-high` |
| `--acp-mode`（`auto`、`accept-edits`、`default`、`plan`） | `AGY_ACP_MODE` | `auto` |
| `--acp-cwd` | `AGY_ACP_CWD` | 系统临时目录下的独立空目录 |
| `--api-key`（以 `Authorization: Bearer` 或 `x-api-key` 校验） | `AGY_GATEWAY_API_KEY` | 无，不校验 |
| `--ui-dir` | `AGY_UI_DIR` | 编译期内嵌的 `web/dist` |
| `--max-idle-sessions` | | `64` |
| `--max-stored-responses` | | `256` |
| `--turn-timeout-secs` | | `600` |
| `--max-body-bytes` | | 12 MiB |
| `--max-line-bytes`（单行 ACP JSON-RPC） | | 16 MiB |
| `--channel-capacity`（每个流） | | `256` |

日志级别由 `RUST_LOG` 控制（默认 `info`）。`liftgate --help` 输出同样的列表。

## 常见问题

- **请求卡住或被转发到代理**：把 `127.0.0.1,localhost,::1` 加入 `NO_PROXY`；若 `agy` 访问 Google Antigravity 需要走代理，运行 `lg proxy 7890`（或 `lg proxy <URL>`）。
- **`could not locate agy executable`**：直接运行 `lg agy install` 自动安装，或传入 `--agy-bin <路径>` / 设置 `AGY_BIN`。
- **`401`**：客户端密钥与 `--api-key` 不一致。
- **额度耗尽（`Individual quota reached` / `insufficient_quota`）**：当前 Antigravity 账号或模型的配额已用完（`agy` 返回重置倒计时时会一并展示），可用 `/model` 切换模型或等待配额重置。
- **后台运行的日志**：执行 `liftgate logs -f` 查看 `~/.liftgate/gateway.log`（使用 `./run-gateway` 脚本时位于 `.agy-runtime/gateway.{out,err}.log`）。
