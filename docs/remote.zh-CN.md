# 远端网关部署与客户端接入指南

[English](remote.md) · 中文

你可以将 Liftgate 部署在远端 Linux、macOS 或 Windows 服务器上（通过 Tailscale、WireGuard、SSH 端口转发、内网直连或 HTTPS 反向代理访问），然后从本机直接连接 Codex CLI（`cx`）、Oh My Pi（`omp`）、Claude Code（`cc`）或 OpenCode（`oc`）。

本机客户端**不需要打补丁、不需要挂载任何二进制/进程 Hook，也不需要在本地额外部署代理守护进程**。

## 跨机运行与上下文优化原理

Liftgate 通过标准 `/v1/responses` 与 `/v1/messages` HTTP + SSE 协议将“远端模型推理”与“本地工作区执行”彻底解耦：

- **工具执行完全留在你的本机**：远端 `agy acp` 子进程运行在隔离的空临时目录中，且默认禁用远端宿主机自带的文件/终端工具。当模型调用命令、改文件或查看本地图片（`exec_command`、`apply_patch`、`exec`、`eval`、`Bash`、`Edit`、`execute`、`view_image`）时，远端网关会在回环 MCP 桥接器（`127.0.0.1:0`）上原位挂起当前 `session/prompt`，通过 SSE 将标准工具调用流式推回本机 CLI；本机在本地仓库执行完毕并回传输出后，远端直接唤醒同一个 ACP 会话继续推理。
- **各项 Context 与调用优化在远端自动闭环**：
  - **热会话池（`SessionPool`）与 KV 前缀缓存**：自动剥离计费头噪声并归一化动态时间戳/窗口提示块，多轮对话、`Ctrl+C` 中断后追问以及客户端 `/compact` 压缩均可直接命中远端已预热的 ACP 会话，无需全量冷重传。
  - **3 层客户端工具虚拟化**：核心编码工具 1:1 保留在 Eager 槽位，海量 MCP / 插件工具自动折叠进 `mcp_lg_ns__*` 命名空间聚合器，并在远端网关内存内 0-RTT 响应 `mcp_lg_search_client_tools` 按需检索。
  - **超长输出无损截断与跨机 0-RTT Spill 读取**：超长命令输出与网页内容按头尾行截断，完整原文原子落盘到远端服务器的 `~/.liftgate/spills/`；模型通过网关内置的 `mcp_lg_read_context_spill` 工具在远端内部 0-RTT 按行号、字节偏移或关键词读取，本机无需访问远端磁盘。
  - **服务端网页抓取与 Flash 极速分流**：`mcp_lg_gateway_web_fetch` 在远端网关直接执行；会话起标题、摘要生成与分类器子请求自动路由至独立的临时 Flash 会话，绝不驱逐主编码热会话。

## 1. 在远端服务器启动 Liftgate

首次在远端机器安装 `lg` 并完成官方 `agy` 登录：

```bash
# 安装 lg 与 liftgate
curl -fsSL https://raw.githubusercontent.com/acking-you/liftgate/main/scripts/install.sh | sh

# 安装官方 agy CLI 并完成 Google 登录
lg agy install
lg agy login

# 可选：若远端机器访问 Google Antigravity 需要走出口代理
# lg proxy http://127.0.0.1:7890
```

`liftgate` 默认仅监听本地回环地址 `127.0.0.1:8787`。在远端服务器上，请通过 `--listen` 绑定到 Tailscale/WireGuard/内网网卡 IP（推荐）或 `0.0.0.0:8787`，并务必开启 `--api-key`（统一保护 `/v1/*`、`/claude-desktop/*` 与 `/api/telemetry/*`）：

```bash
# 后台守护模式启动（推荐绑定 Tailscale/内网 IP 或 0.0.0.0）
lg up --listen 0.0.0.0:8787 --api-key "<YOUR_API_KEY>"

# 或前台运行（适合 systemd、tmux 或容器编排）
liftgate serve --listen 0.0.0.0:8787 --api-key "<YOUR_API_KEY>"
```

在本机验证远端连通性（下文均以 `100.64.0.10:8787` 代表你的远端地址与端口）：

```bash
curl -fsS http://100.64.0.10:8787/healthz
curl -fsS -H "Authorization: Bearer <YOUR_API_KEY>" http://100.64.0.10:8787/v1/models
```

## 2. 方案 A：在本机用 `lg` 一键简化接入（推荐）

你只需在本机安装单文件 `lg` 二进制——**本机无需安装 `agy`、无需登录账号，也不会启动本地网关守护进程**。`lg` 会自动向远端拉取实时模型目录、校准逐模型上下文窗口与自动压缩阈值、开启 Code Mode，并自动将远端地址并入子进程的 `NO_PROXY`。

### 零配置污染直连启动（`lg remote` + `lg cx` / `omp` / `cc` / `oc`）

将远端网关地址与密钥保存至本机 `~/.liftgate/settings.json`：

```bash
# 保存远端网关目标与密钥（自动校验 /healthz 连通性）
lg remote http://100.64.0.10:8787 --api-key "<YOUR_API_KEY>"

# 在本地任意代码仓库直接拉起智能体连接远端网关
lg cx        # OpenAI Codex CLI（桌面端 GUI 可用 lg cxa）
lg omp       # Oh My Pi
lg cc        # Claude Code（桌面端 GUI 可用 lg cd）
lg oc        # OpenCode

# 远程查看网关健康状态、切换默认模型或打开实时全屏 TUI 面板
lg st
lg m -i
lg top
```

单次临时指定或切回本地网关：

```bash
# 单次临时连接远端，不修改已保存设置
lg cx --remote 100.64.0.10:8787 --api-key "<YOUR_API_KEY>"

# 或通过环境变量指定
export AGY_REMOTE="http://100.64.0.10:8787"
export AGY_GATEWAY_API_KEY="<YOUR_API_KEY>"
lg omp

# 随时切回本机托管网关模式
lg remote local
```

### 一键持久化生成客户端配置文件（`lg config`）

如果你习惯在终端直接敲原生的 `codex`、`omp`、`claude` 或 `opencode`，可用 `lg config` 将远端网关配置与实时模型目录无损合并进本机全局（或项目级）配置文件：

```bash
# 将 AGY_GATEWAY_API_KEY 写入你的 ~/.zshrc、~/.bashrc 或 PowerShell $PROFILE，
# 以便 Codex CLI 通过 env_key = "AGY_GATEWAY_API_KEY" 读取密钥：
export AGY_GATEWAY_API_KEY="<YOUR_API_KEY>"

# 一键同步全部四个客户端的全局配置（若已执行过 lg remote 可省略 --remote 与 --api-key）
lg config --global --remote http://100.64.0.10:8787 --api-key "<YOUR_API_KEY>"

# 也可只同步指定客户端：
lg config codex --global
lg config omp --global
lg config claude --global
lg config opencode --global
```

## 3. 方案 B：纯手动配置（本机不装 `lg`，一键复制即用）

如果本机不想安装 `lg`，可直接复制下方对应客户端的配置片段。请将示例中的占位符替换为你的实际值：

- `http://100.64.0.10:8787`（挂了 HTTPS 反代则为 `https://lg.example.com`）：你的远端网关地址
- `NO_PROXY` 中的 `100.64.0.10`：你的远端 IP 或域名，避免本机系统代理拦截网关请求
- `<YOUR_API_KEY>`：远端 `--api-key` 设置的密钥（若远端在内网未开启 `--api-key`，保留非空占位符 `local-agy` 即可）

---

### Codex CLI（`cx` / `codex`）

原生 `codex` 不会自动拉取远端的 `/v1/codex-models`。**务必先导出一次模型目录文件**：它负责向 Codex 提供 `/model` 实时模型列表、`500k / 425k` 上下文与自动压缩水位线、`80 KB` 单工具输出预算、Code Mode（`exec` + `wait`）以及针对网关调优过的 `base_instructions`。

**1. 从远端网关导出模型目录文件：**

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

**2. 编辑 `~/.codex/config.toml`（Windows 为 `%USERPROFILE%\.codex\config.toml`）：**

```toml
model = "gemini-4-carbon-high"
model_provider = "agy"
# 必须填第 1 步导出的绝对路径；Windows 请使用正斜杠，如 "C:/Users/你的用户名/.codex/agy-models.json"
model_catalog_json = "/Users/你的用户名/.codex/agy-models.json"
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

> 若希望保留命令审批与本地沙箱提示，删去上方的 `approval_policy` 与 `sandbox_mode` 两行即可。

**3. 在终端设置环境变量并启动 `codex`：**

```bash
export AGY_GATEWAY_API_KEY="<YOUR_API_KEY>"
export NO_PROXY="${NO_PROXY:+$NO_PROXY,}127.0.0.1,localhost,::1,100.64.0.10"
codex
```

---

### Oh My Pi（`omp`）

**1. 创建或合并 `~/.omp/agent/models.yml`：**

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

**2. 创建或合并 `~/.omp/agent/config.yml`：**

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

> 若希望保留每次工具执行前的确认提示，将 `tools.approvalMode` 改为 `always-ask` 即可。

**3. 让远端地址绕过本机代理并启动 `omp`：**

```bash
export NO_PROXY="${NO_PROXY:+$NO_PROXY,}127.0.0.1,localhost,::1,100.64.0.10"
omp
```

---

### Claude Code（`cc` / `claude`）

**方式 1 —— 持久化写入 `~/.claude/settings.json`（或项目 `.claude/settings.local.json`），带 `425k` Gemini 自动压缩窗口校准：**

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

> 若希望保留标准权限确认提示，删去 `skipDangerousModePermissionPrompt` 与 `permissions` 即可；若将主模型切换为网关上的 `claude-*` 系列模型，建议把对应模型的 `autoCompactWindow` 调整为 `170000`。

**方式 2 —— 当前终端一次性环境变量启动：**

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

### OpenCode（`oc` / `opencode`）

**1. 创建或合并 `~/.config/opencode/opencode.json`（或项目根目录 `./opencode.json`）：**

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

> 若希望保留交互式权限确认提示，删去 `"permission": { "*": "allow" }` 即可。

**2. 让远端地址绕过本机代理并启动 `opencode`（若仍在使用 OpenCode v1.x，可额外导出 `OPENCODE_EXPERIMENTAL_CODE_MODE=true`）：**

```bash
export NO_PROXY="${NO_PROXY:+$NO_PROXY,}127.0.0.1,localhost,::1,100.64.0.10"
opencode
```

## 注意事项与已知边界

- **远端网关切勿传入 `--no-mcp-tool-bridge`**（保持默认开启）：关闭 MCP 桥接退回纯 XML 模式后，远端将不再挂载 `mcp_lg_read_context_spill`，落盘在远端 `~/.liftgate/spills/` 的超长截断全文将无法被跨机客户端读取。
- **`lg remote` / `--remote` 目前仅支持明文 `http://<host>:<port>`**：`lg` 内置的控制面客户端使用无 TLS 依赖的裸 TCP HTTP/1.1（支持 IPv4、带方括号的 IPv6 如 `[fd00::1]:8787` 以及 Tailscale/DNS 域名，不支持 `https://` 或自定义 URL 子路径），适合在 Tailscale、WireGuard、SSH 本地端口转发（`ssh -L 8787:127.0.0.1:8787 user@host`）或受信任内网中使用。
- **挂公网 HTTPS 反向代理（Nginx / Caddy / Cloudflare Tunnel）**：若在远端网关前加了 TLS 反向代理，请直接采用上方**方案 B（纯手动配置）**，将各客户端的 `base_url` / `baseUrl` / `baseURL` 指向 `https://你的域名/v1`（Claude Code 的 `ANTHROPIC_BASE_URL` 指向 `https://你的域名`），并在反代侧确保：
  - 关闭流式响应缓冲（如 Nginx `proxy_buffering off;`）
  - 放宽请求体大小上限至 `>= 32 MiB`（`client_max_body_size 32m;`）
  - 将上游读写超时调至 `>= 1800s`（`proxy_read_timeout 1800s;`）
- **手动配置的模型目录刷新**：`lg cx` / `lg omp` / `lg cc` / `lg oc` 每次启动都会向远端拉取最新模型列表，而手动导出的 `agy-models.json`、`models.yml`、`opencode.json` 是静态快照；当远端 `agy` 更新增加新模型后，重新执行一次 `curl /v1/codex-models`（或 `lg config --global`）即可同步。
