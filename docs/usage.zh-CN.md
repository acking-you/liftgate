# 使用指南

[English](usage.md) · 中文

启动 Liftgate，然后把 Codex CLI、Claude Code 或任意 OpenAI / Anthropic 客户端指向它。下文地址均以默认的 `http://127.0.0.1:8787` 为例。

## 准备工作

- **Antigravity CLI（`agy`）**：安装后执行一次 `agy acp --login` 完成登录。Liftgate 会在 `PATH` 和标准安装位置查找它，找不到时用 `--agy-bin <路径>` 指定。
- **Rust 1.85+**：用于编译网关。
- **Bun**（可选）：用于把监控面板构建到 `web/dist`。没有它所有 API 照常可用。

## 启动网关

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

**1. 导出模型目录**（可选，推荐），让 Codex 的 `/model` 菜单列出全部 Antigravity 模型及其上下文窗口：

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
model_context_window = 1000000
model_auto_compact_token_limit = 850000

[model_providers.agy]
name = "Liftgate"
base_url = "http://127.0.0.1:8787/v1"
wire_api = "responses"
# 若网关以 --api-key <密钥> 启动，取消下一行注释并设置环境变量 AGY_GATEWAY_API_KEY=<密钥>：
# env_key = "AGY_GATEWAY_API_KEY"
```

**3. 运行 `codex`。** 如果终端开启了 HTTP 代理，先让本地回环地址绕过代理：`export NO_PROXY="127.0.0.1,localhost,::1"`。

## Claude Code

只对当前终端生效：

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

或者持久化到 `~/.claude/settings.json`（或项目级 `.claude/settings.local.json`）：

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

网关未设置 `--api-key` 时，任意非空的 `ANTHROPIC_AUTH_TOKEN` 都能通过 Claude Code 的本地检查；设置了 `--api-key <密钥>` 时，请填对应密钥。

## 其它客户端

OpenAI Responses 客户端指向 `http://127.0.0.1:8787/v1`，Anthropic Messages 客户端指向 `http://127.0.0.1:8787`（见 [API](api.md)）。

## 监控面板

构建过 `web/dist` 后，Liftgate 在 `http://127.0.0.1:8787/` 提供监控面板：进行中与最近完成的任务（含工具调用与 Token 明细）、会话池（可手动关闭空闲会话）、模型目录以及 `agy` 最近的 stderr 日志。网关设置了 `--api-key` 时，在面板的「连接设置」中填入密钥。用 `--ui-dir <目录>` 可托管其它构建产物。

## 参数与环境变量

| 参数 | 环境变量 | 默认值 |
| --- | --- | --- |
| `--listen` | `AGY_GATEWAY_LISTEN` | `127.0.0.1:8787` |
| `--agy-bin` | `AGY_BIN` | 从 `PATH` 或标准安装位置查找 |
| `--default-model`（请求未知或通用模型名时使用） | `AGY_DEFAULT_MODEL` | `gemini-4-carbon-high` |
| `--acp-mode`（`auto`、`accept-edits`、`default`、`plan`） | `AGY_ACP_MODE` | `auto` |
| `--acp-cwd` | `AGY_ACP_CWD` | 系统临时目录下的独立空目录 |
| `--api-key`（以 `Authorization: Bearer` 或 `x-api-key` 校验） | `AGY_GATEWAY_API_KEY` | 无，不校验 |
| `--ui-dir` | `AGY_UI_DIR` | 存在时使用 `./web/dist` |
| `--max-idle-sessions` | | `64` |
| `--max-stored-responses` | | `256` |
| `--turn-timeout-secs` | | `600` |
| `--max-body-bytes` | | 12 MiB |
| `--max-line-bytes`（单行 ACP JSON-RPC） | | 16 MiB |
| `--channel-capacity`（每个流） | | `256` |

日志级别由 `RUST_LOG` 控制（默认 `info`）。`liftgate --help` 输出同样的列表。

## 常见问题

- **请求卡住或被转发到代理**：把 `127.0.0.1,localhost,::1` 加入 `NO_PROXY`。
- **`could not locate agy executable`**：传入 `--agy-bin <路径>` 或设置 `AGY_BIN`。
- **`401`**：客户端密钥与 `--api-key` 不一致。
- **额度耗尽（`Individual quota reached` / `insufficient_quota`）**：当前 Antigravity 账号或模型的配额已用完（`agy` 返回重置倒计时时会一并展示），可用 `/model` 切换模型或等待配额重置。
- **后台运行的日志**：`.agy-runtime/gateway.out.log` 与 `.agy-runtime/gateway.err.log`。
