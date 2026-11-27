# 使用指南

[English](usage.md) · 中文

启动 Liftgate，然后把 Codex CLI、Claude Code、OpenCode 或任意 OpenAI / Anthropic 客户端指向它。下文地址均以默认的 `http://127.0.0.1:8787` 为例；若要在远端服务器运行网关并从本机连接 `cx`、`omp`、`cc` 或 `oc`（支持 `lg remote` 一键接入或纯手动复制配置），详见[远端网关部署与客户端接入指南](remote.zh-CN.md)。

## 准备工作

- **全局安装 `lg` / `liftgate`**：
  ```bash
  # macOS / Linux / FreeBSD
  curl -fsSL https://raw.githubusercontent.com/acking-you/liftgate/main/scripts/install.sh | sh
  ```
  ```powershell
  # Windows (PowerShell)
  irm https://raw.githubusercontent.com/acking-you/liftgate/main/scripts/install.ps1 | iex
  ```
  也可运行 `cargo binstall liftgate`、从 [最新 CLI Release](https://github.com/acking-you/liftgate/releases/latest) 手动下载归档，或使用 **Rust 1.100.0** 从源码全局安装：
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
lg remote 10.0.0.8:8787         # 持久指向远端 Liftgate 网关（支持 `--api-key`；`lg remote local` 切回本地）
lg top                          # 打开全屏实时 TUI 监控仪表盘（别名：tui、dash、monitor）
lg up                           # 后台启动网关与内嵌 Web 监控面板（全称：start）
lg up --open                    # 后台启动并自动在浏览器打开监控面板
lg st                           # 查看健康状态、PID、会话池、Token 统计与进行中轮次（全称：status，支持 --json）
lg l -n 100 -f                  # 查看并实时跟随后台日志（全称：logs，加 --acp 查看 agy stderr）
lg web                          # 打开 Web 监控面板（全称：ui，加 --print 仅输出地址）
lg m -i                         # 交互式搜索并热切换默认模型（全称：models --pick）
lg doc                          # 自检 agy、claude、codex、opencode、代理、内嵌面板与登录模型就绪状态（全称：doctor）
lg r                            # 重启后台网关（全称：restart）
lg down                         # 优雅停止后台网关并清理 ACP 子进程树（全称：stop）
lg upgrade                      # 检查 GitHub Releases（-c / --check）并自动校验升级 lg 与 liftgate（别名：update、self-update）
lg alias --name lg              # 在 PATH 中安装或移除（--uninstall）短命令别名
lg completions powershell       # 生成 Shell 自动补全脚本（bash、zsh、fish、powershell、elvish）
liftgate serve                  # 前台运行网关
```

在 `lg tui` 中按 `3` 进入 Models，选中模型后按 `t` 保存为标题生成模型（默认 `gemini-4-hydrogen-low`）；按 `b` 或 `v` 可分别实时热切换「混合原生 MCP 工具挂起桥接」与「`>=300k` 视觉子会话卸载」。

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

## 零副作用临时启动（`lg cx` / `lg cc` / `lg oc` / `lg omp` / `lg cxa` / `lg cd`）

跨 Windows、macOS、Linux 在**任意目录**下零污染启动 Codex CLI、Claude Code、**OpenCode**、**Oh My Pi（`omp`）**、**Codex App** 或 **Claude Desktop**，退出后**完全 0 残留**（不改动当前项目、`~/.claude/settings.json`、`~/.codex/config.toml`、`~/.config/opencode/opencode.json`、`~/.omp/agent/`、官方桌面端配置或父终端环境）：

```bash
cd /path/to/your/project
liftgate codex                              # 在当前目录启动交互式 Codex CLI（默认开启 Code Mode：`exec` + `wait`）
liftgate codex --code-mode-only             # 启动纯 Code Mode（仅暴露 `exec` + `wait`）的 Codex CLI
liftgate codex --no-code-mode               # 当前会话关闭 Code Mode，恢复直连工具模式（`direct`）
liftgate claude                             # 在当前目录启动交互式 Claude Code（默认注入 `CLAUDE_CODE_EXPERIMENTAL_CODE_MODE=1`）
liftgate opencode                           # 在当前目录启动交互式 OpenCode TUI（短别名 `lg oc`；默认开启 `@opencode/codemode`）
liftgate opencode --full-code-mode          # 开启 OpenCode 全量 Code Mode（注入 `OPENCODE_FLAGS=full_codemode`）
liftgate opencode --no-code-mode            # 关闭 Code Mode，由 Liftgate `OpenCodeToolProfile` 虚拟化直连全部工具
liftgate omp                                # 在当前目录启动交互式 Oh My Pi TUI（别名 `lg oh-my-pi`；开启双栈 `eval` + `tools.commands`）
liftgate omp --full-code-mode               # 将 MCP Server 直接挂载为 `eval` 全局命名空间（`mcp.evalNamespaces: true`）
liftgate omp --no-code-mode                 # 关闭 `eval`（`PI_PY=0 PI_JS=0`），切换为直连工具调用
liftgate codex exec "运行 cargo test"        # 原生子命令与参数直接透传
liftgate claude --model gemini-4-hydrogen-low -p "总结最近一次提交"
liftgate opencode run "运行 cargo test"      # 非交互式执行单次 OpenCode 任务（支持 `lg oc run -c "..."`）
liftgate omp -p "运行 cargo test"            # 非交互式执行单次 Oh My Pi 打印模式任务

# 原生会话查看、恢复、分叉、Git Worktree 与后台任务管理
lg cx resume                                # 打开 Codex 交互式会话列表（加 `--all` 跨目录查看；兼容别名 `lg cx -r`）
lg cx resume --last -m gemini-3.8-pro-high  # 恢复最近一次会话并切换模型（Liftgate 参数写在前后任意位置均可识别）
lg cx fork --last                           # 基于最近一次 Codex 会话分叉新线程
lg cx agents                                # 打开 Codex Agent 控制中心（`archive`、`unarchive`、`delete`、`queue` 等均原生可用）
lg cx review --uncommitted                  # 通过 Liftgate 执行非交互式代码审查
lg cc -r                                    # 打开 Claude 交互式会话选择器（兼容 `lg cc resume`；按 `Ctrl+A` 查看全部项目）
lg cc -c                                    # 继续当前目录最近一次 Claude 会话（兼容 `lg cc continue`、`lg cc resume --last`）
lg cc fork --last                           # 分叉最近一次 Claude 会话（自动映射 `--continue --fork-session`）
lg cc -w feat-auth                          # 在独立 Git Worktree 中启动 Claude Code
lg cc --bg "重构解析器"                      # 启动后台会话（`lg cc agents`、`attach`、`logs`、`stop`、`rm` 等均原生可用）
lg oc -c                                    # 继续最近一次 OpenCode 会话（兼容 `lg oc continue`、`lg oc resume`）
lg oc -s ses_42                             # 继续或创建指定 ID 的 OpenCode 会话（兼容 `lg oc resume ses_42`）
lg oc models                                # 查看已配置的 Provider 与实时 Antigravity 模型列表
lg oc session list                          # 查看当前项目会话列表（支持 `delete`、`export`、`import`、`web`、`serve`、`acp`）
lg omp -c                                   # 继续最近一次 Oh My Pi 会话（兼容 `lg omp continue`、`lg omp resume --last`）
lg omp -r                                   # 打开 Oh My Pi 交互式会话选择器（兼容 `lg omp resume`、`lg omp resume <ID>`）
lg omp --fork <ID>                          # 基于已有会话分叉新分支
lg omp models agy                           # 查看实时 Antigravity 模型目录（`commit`、`session`、`acp` 等均原生可用）

# 全平台桌面客户端（适配 Windows MSIX/Squirrel/独立安装、macOS .app、Linux .deb/Flatpak/AppImage）
lg codex-app                                # 短别名 `lg cxa` 或 `lg cx --app`（隔离 CODEX_HOME + --user-data-dir）
lg codex-app --ephemeral                    # 使用一次性 Electron Profile，关闭窗口即自动清除
lg claude-desktop                           # 短别名 `lg cd` 或 `lg cc --desktop`（3P 网关会话注入，退出自动无损还原）
lg claude-desktop --restart-app             # 若 Claude Desktop 已在运行，优雅重启并切入网关
lg claude-desktop --restore                 # 手动立即执行回滚日志恢复（断电/强杀亦会在下次启动自动恢复）
```

- 若后台网关尚未启动，`liftgate codex`、`liftgate claude`、`liftgate opencode`、`liftgate omp` 及桌面端命令会自动在后台拉起它。
- **OpenCode（`lg oc` / `lg opencode`）**：通过纯内存环境变量 `OPENCODE_CONFIG_CONTENT` 无损合并注入 `provider.agy` 与实时模型目录（不写入任何项目或全局文件），并在检测到 OpenCode v2（`>= 2.0.0`）时自动附加 `--standalone` 启动私有隔离服务，避免复用到未携带网关配置的后台守护进程。
- **Oh My Pi（`lg omp` / `lg oh-my-pi`）**：在 `~/.liftgate/ephemeral/` 生成一次性 TypeScript Provider 扩展（`pi.registerProvider("agy", ...)`）与 YAML Overlay 并通过 `PI_CONFIG_FILES` 注入（自动继承用户已配置的 `extensions` 列表），全程不触碰 `~/.omp/agent/` 与项目 `.omp/`，进程退出立即清理。
- 注入的环境变量与临时配置目录（位于 `~/.liftgate/ephemeral/`）仅对子进程生效，进程退出时立即自动删除。
- **Codex App（`lg cxa`）**：在 `~/.liftgate/desktop/sessions/` 下合成独立物理 `CODEX_HOME`（只读合并 `~/.codex/config.toml`，强制设置 `cli_auth_credentials_store = "file"`、`forced_login_method = "api"` 并生成独立 API Key 模式 `auth.json`，绝不混入个人 ChatGPT OAuth 登录态、也绝不触碰 `~/.codex`），同时通过 `--user-data-dir`（`~/.liftgate/desktop/codex-app-isolated`）隔离桌面端数据目录；Windows Microsoft Store MSIX 包通过无窗口容器内启动器（`conhost --headless` + `Invoke-CommandInDesktopPackage -PreventBreakaway`）跨 COM 边界完整注入 `CODEX_HOME`，且不篡改 `HOME` / `USERPROFILE` / `APPDATA`，确保内置终端中的 `git`、`cargo`、`ssh` 完全正常。
- **Claude Desktop（`lg cd`）**：修改前先写入原子回滚日志（`~/.liftgate/desktop-journals/`），覆盖标准目录、Windows MSIX `LocalCache` 与 Linux Flatpak 沙箱目录注入临时第三方推理配置（`deploymentMode: "3p"`，指向本地 `/claude-desktop` 路由），后台守护进程监听窗口关闭后自动精准还原原始 `deploymentMode` 与 Profile 列表，同时完整保留会话期间新增的 `mcpServers` 或偏好设置。
- 默认开启全自动免确认模式（Codex 传 `--dangerously-bypass-approvals-and-sandbox`，Claude Code 传 `--dangerously-skip-permissions`）；加 `--no-bypass`（或显式传入权限/沙箱参数）即可保留常规权限确认。
- 原生子命令与选项无需额外写 `--`：本地管理/查询类子命令（如 `lg cx queue|archive|delete|mcp|plugin|features|-V`、`lg cx resume --help`、`lg cc logs|attach|stop|rm|mcp|plugin|purge|-v`、`lg cc agents --json`）会保持原生子命令位置并直接离线秒开（无需拉起网关）；Liftgate 自身参数（`-m`、`--no-bypass`、`--no-code-mode`、`--code-mode-only` 等）放在原生子命令前或后均可识别。如需完全禁用别名转换并原样透传参数（如查看完整原生帮助 `lg cx -- --help` / `lg cc -- --help`），显式加上 `--` 即可。

## 上下文窗口与单轮预算

推荐通过 `lg cx` 或 `lg cc` 读取运行中网关的上下文策略。Liftgate 采用纯单轮 `session/prompt` 配合热 ACP 会话复用，由客户端提前触发压缩，不插入任何后台重放或合成预灌回合：

| 模型系列 | 声明窗口 | 自动压缩阈值 |
| --- | --- | --- |
| Gemini | 500,000 | 425,000 |
| Claude | 200,000 | 170,000 |
| GPT-OSS | 128,000 | 110,000 |

实测上游 `agy acp` 在 Gemini 会话达到 `524,288` token（`1,048,576` `usage_update` 窗口的 `50%`）时会立即在轮末触发内部自动摘要压缩；将客户端窗口设为 `500,000 / 425,000` 可在上游清栈前稳妥在热会话上完成客户端 `/compact`。Codex 按模型目录读取配置；Claude Code 使用逐模型 `modelSettings.autoCompactWindow`（需 v2.1.288 或更新版本）。多个网关模型映射到同一个 Claude canonical 名称时取较小阈值，因此可能早于表中数值压缩。参考[Claude 官方上下文配置](https://code.claude.com/docs/en/model-config#context-window-and-auto-compaction)。

每次 ACP `session/prompt` 有 `176,000` 字节的单轮文本预算（留有安全余量，低于上游单轮 `191,579` 字节的截断边界）。当任何工具调用输出、单轮输入、系统指令、工具 Schema、冷历史归档或 `web_fetch` 网页内容发生截断或省略时，Liftgate 都会先将 100% 原始无损内容原子落盘到 `~/.liftgate/spills/`，再按水位线分配算法仅对超长块做按行对齐的首尾保留中间截断（保留约 55% 开头与 45% 结尾），并在省略提示中精确附带本地文件路径与省略行号范围以便客户端工具按需读取，同时保持同轮其它小工具输出完整无损、不影响后续轮次的 `SessionPool` 热会话前缀命中。真正发生上下文窗口溢出时，网关会返回客户端标准可自动触发压缩的错误格式（Codex 为 `context_length_exceeded`，Claude Code 为 `prompt is too long: N tokens > M maximum`）。客户端 `/compact` 会优先复用匹配的热会话完成总结，再释放旧会话。

## 混合本地回环 MCP 工具挂起桥接

默认情况下（`--mcp-tool-bridge`、`AGY_MCP_TOOL_BRIDGE=true` 或 `~/.liftgate/settings.json` 中的 `mcp_tool_bridge: true`），Liftgate 会在本地仅绑定 `127.0.0.1` 的受鉴权回环 HTTP MCP 服务（每会话独立 256-bit 随机令牌），以严格契合 `agy acp` 50 工具 Eager 上限的安全预算（`<= 24` 个原生 MCP 槽位）挂载客户端工具，确保 `exec`、`exec_command`、`Bash` 等大体积核心编程工具 100% 保留在 Eager 原生声明中：

- **单次 `session/prompt` 挂起原生续接**：模型调用桥接工具时，网关挂起当前 `session/prompt` 回环调用并立即向客户端流式返回标准 OpenAI `function_call` / `custom_tool_call` 或 Anthropic `tool_use`；下一轮客户端携带工具结果返回时（无论使用有状态 `previous_response_id` 还是无状态全量 `input` / `messages` 数组），直接唤醒挂起通道继续推理，使多步工具链始终驻留在同一 `session/prompt` 内，稳定保持 ~93%+ 的上游 Prompt KV 缓存命中率并彻底消除跨轮副本分片掉缓存与 XML 幻觉。
- **自适应命名空间聚合 Hub 与 0-RTT 内存检索（`search_client_tools`）**：轻量会话（`<= 20` 个工具且总定义体积 `<= 22 KiB`）保持 1:1 全量直通；当挂载大规模连接器（如 128+ 个 GitHub / TUI / 多智能体工具）时，核心编程工具（`exec`、`wait`、`REPL`、`exec_command`、`write_stdin`、`Bash`、`Read`、`Edit` 等）保持 1:1 原生直通，其余同源工具族自动折叠为 `mcp_lg_ns__<namespace>` 聚合入口 + `mcp_lg_call_client_tool` 通用分发器，并配合网关内存级 `< 0.1ms` 零客户端往返检索 `mcp_lg_search_client_tools` 与必填参数预检自愈（详见 [MCP 工具虚拟化设计](mcp-tool-virtualization.md)）。
- **XML 混合共存、动态工具热同步与无损退栈**：自由文本 `Custom` 工具（如多行裸 `apply_patch` 补丁）在同一会话内走无转义 `<agy_tool_call_...>` XML 通道协同工作；会话中途动态新增连接器工具时实时热更新内存路由表，避免注入大体积 XML Schema；若用户在工具等待期间按 `Esc` 打断、发送新提示词、切换模型或触发 `/compact`，网关会先通过 `session/cancel` 无损退栈并继续复用原热会话。
- **零重启实时热切换**：启动时可传 `--no-mcp-tool-bridge`（`AGY_MCP_TOOL_BRIDGE=false`）关闭，运行中也可随时在 `lg top` 按 `b`、在 Web 监控面板「ACP 会话池与监管」页点击开关或调用 `POST /api/telemetry/mcp-tool-bridge` 在线热切换。

## 快捷生成本地或全局配置文件（`liftgate config`）

如果你希望直接输入裸命令 `claude` 或 `codex` 使用网关，可用 `liftgate config`（别名 `liftgate setup`）一键生成或无损合并本地/全局配置文件：

```bash
liftgate config                     # 在当前项目无损同步 .claude、.codex、opencode.json 与 .omp
liftgate config claude --global     # 无损合并到全局 ~/.claude/settings.json
liftgate config codex --global      # 无损合并 [model_providers.agy] 与模型目录到全局 ~/.codex/
liftgate config opencode --global   # 无损合并 provider.agy 到全局 ~/.config/opencode/opencode.json（短别名 `oc`）
liftgate config omp --global        # 无损合并 providers.agy 与 modelRoles 到全局 ~/.omp/agent/{models.yml,config.yml}
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
export ANTHROPIC_SMALL_FAST_MODEL="gemini-4-hydrogen-low"
export CLAUDE_CODE_AUTO_COMPACT_WINDOW="110000"
export NO_PROXY="127.0.0.1,localhost,::1"
claude
```

```powershell
$env:ANTHROPIC_BASE_URL = "http://127.0.0.1:8787"
$env:ANTHROPIC_AUTH_TOKEN = "local-agy"
$env:ANTHROPIC_MODEL = "gemini-4-carbon-high"
$env:ANTHROPIC_SMALL_FAST_MODEL = "gemini-4-hydrogen-low"
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
    "ANTHROPIC_SMALL_FAST_MODEL": "gemini-4-hydrogen-low",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "110000",
    "NO_PROXY": "127.0.0.1,localhost,::1"
  }
}
```

网关未设置 `--api-key` 时，任意非空的 `ANTHROPIC_AUTH_TOKEN` 都能通过 Claude Code 的本地检查；设置了 `--api-key <密钥>` 时，请填对应密钥。

> **关于 Claude Code Mode（`REPL`）的说明**：Liftgate 的 `/v1/messages` 协议层已完整适配 `REPL` 沙箱工具定义保留与引号容错解析、`claude-fable-*` 模型族映射、`container` / `allowed_callers` / `caller` 字段以及 Auto Mode 代码审查分类器分流。但需要注意：Claude Code 官方客户端（`v2.1.323+`）目前在本地硬性限制了设置 `ANTHROPIC_BASE_URL` 或 `ANTHROPIC_AUTH_TOKEN` 时不启用客户端 Code Mode（详见[官方文档](https://code.claude.com/docs/en/code-mode)）；待上游放开自定义网关限制后即可直接无缝使用。

## OpenCode

推荐使用 `lg oc`（`liftgate opencode`）零副作用自动配置（同时兼容 OpenCode v1.x 与 OpenCode v2.x），或执行 `lg config opencode --global` 无损写入全局 `~/.config/opencode/opencode.json`（自动识别 `OPENCODE_CONFIG_DIR` 与 `XDG_CONFIG_HOME`）。

若需手工配置，编辑 `~/.config/opencode/opencode.json`（或项目根目录 `./opencode.json`）：

```json
{
  "$schema": "https://opencode.ai/config.json",
  "model": "agy/gemini-4-carbon-high",
  "small_model": "agy/gemini-4-hydrogen-low",
  "experimental": {
    "full_codemode": true
  },
  "provider": {
    "agy": {
      "npm": "@ai-sdk/anthropic",
      "name": "Antigravity (Liftgate)",
      "options": {
        "baseURL": "http://127.0.0.1:8787/v1",
        "apiKey": "local-agy",
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

- **Code Mode（`@opencode/codemode`）**：`lg oc` 与 `lg config opencode` 默认开启 OpenCode v2 全量 Code Mode（`experimental.full_codemode: true` / `OPENCODE_FLAGS=full_codemode`，OpenCode v1.x 则自动注入 `OPENCODE_EXPERIMENTAL_CODE_MODE=true`），将内置文件读写/编辑/终端命令、`Bun.file` / `Bun.write` / `Bun.Glob` 及外部 MCP 与命名空间工具统一纳入 `execute` 编排；加 `lg oc --mcp-code-mode` 可将内置文件/终端工具保留为直连调用（仅对 MCP 与命名空间工具使用 `execute`），加 `lg oc --no-code-mode` 则将全部工具切换为直连调用（由 Liftgate 零开销 `OpenCodeToolProfile` 自动折叠至 `<= 24` 个 Eager 槽位）。
- **上下文校准与补丁预检自愈**：`limit.context = 500000` 配合 OpenCode 默认 `15%` 余量恰好在 `425,000` token 稳妥触发自动压缩（避开上游 `524,288` 内部压缩线）；OpenCode 的 `patch`（`{ "patchText": "..." }`）享受网关侧 0-RTT Lark V4A 语法预检自愈与同文件自动合并，会话标题与摘要自动分流至轻量临时会话。

## Oh My Pi（`omp`）

推荐使用 `lg omp`（`liftgate omp`，别名 `lg oh-my-pi`）零副作用自动配置，或执行 `lg config omp --global` 无损写入全局 `~/.omp/agent/models.yml` 与 `~/.omp/agent/config.yml`（自动识别 `OMP_PROFILE` / `PI_PROFILE`、`PI_CONFIG_DIR`、`PI_CODING_AGENT_DIR` 与 `$XDG_DATA_HOME/omp`）。在项目目录执行 `lg config omp` 则生成 `.omp/liftgate-agy-provider.ts` 并无损合并 `.omp/config.yml`，完全不触碰全局配置。

若需手工配置全局文件，可在 `~/.omp/agent/models.yml` 添加 `providers.agy` 并在 `~/.omp/agent/config.yml` 配置 `modelRoles`：

```yaml
# ~/.omp/agent/models.yml
providers:
  agy:
    baseUrl: http://127.0.0.1:8787/v1
    apiKey: local-agy
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
```

- **双栈 Code Mode（`eval` + `wait` + `tools.commands`）**：自动写入 `inlineToolDescriptors: "off"` 以保留 Gemini 模型的工具描述，`OmpToolProfile` 将常驻 Python（`py`）与 Bun JavaScript（`js`）双内核 `eval` 及后台作业屏障 `wait` 固定在 Tier 0（`64 KiB` 描述预算完整保留 `<prelude>` / `<browser>` / `<computer>` / `<dsm>` 帮助并附带 `<eval_session_tools>` 签名），将从属检索/命令工具（`read`、`bash`、`grep`、`glob`、`lsp` 等）收敛进 `mcp_lg_eval` 并引导模型通过无前缀 `await tool.<name>(args)` 调用；加 `lg omp --full-code-mode` 开启 `mcp.evalNamespaces: true`，加 `lg omp --no-code-mode` 关闭 `eval` 切换为全量直连调用。
- **`hashline` 安全编辑与单下划线 MCP 虚拟化**：完整保留 `omp` 默认 `edit` 工具的 `hashline`（`[path#TAG]` + `PUT N.=M:`，含 JSON `{ input }` 与自定义 Lark 语法两种形态）原样透传而不被误判为 V4A 补丁；`apply_patch` 享受 0-RTT V4A 自愈校验，大规模 `mcp__<server>_<tool>`、`memory` 与 `session_state` 工具自动折叠至 `mcp_lg_ns__*` 路由中心。

## 其它客户端

OpenAI Responses 客户端指向 `http://127.0.0.1:8787/v1`，Anthropic Messages 客户端指向 `http://127.0.0.1:8787`（见 [API](api.md)）。

## 监控面板

Liftgate 直接在 `http://127.0.0.1:8787/` 提供实时监控面板（Release 二进制已在编译期内置完整面板，也可直接执行 `liftgate ui` 打开）：展示进行中与最近完成的任务（含 Token 与缓存命中率）、每一次工具调用的完整输入输出（CodeMode 脚本高亮、补丁以 Diff 展示，网关本地 0-RTT 应答与预检拒绝也会记录）、各会话的工具目录（客户端工具如何虚拟化挂载到 `lg`、客户端与 Wire 定义对照、上下文构成）、可选的完整上下文捕获、会话池（可手动释放空闲或 MCP 挂起会话，并在线热切换 MCP 原生工具桥接与 `>=300k` 视觉卸载）、实时模型目录以及 `agy` 最近的 stderr 日志。启用了 `--api-key` 时，`liftgate ui` 会自动携带密钥完成连接；也可通过 `--ui-listen <地址>` 额外监听独立前端端口，或用 `--ui-dir <目录>` 覆盖静态资源目录。

## 参数与环境变量

| 参数 | 环境变量 | 默认值 |
| --- | --- | --- |
| `--state-dir` | `LIFTGATE_HOME` | `~/.liftgate` |
| `--listen` | `AGY_GATEWAY_LISTEN` | `127.0.0.1:8787` |
| `--ui-listen` | `AGY_UI_LISTEN` | 无（直接由 `--listen` 同端口提供） |
| `--agy-bin` | `AGY_BIN` | 从 `PATH` 或标准安装位置查找 |
| `--agy-proxy`（`<端口\|URL>` 或 `off`） | `AGY_PROXY` | 无（直连；或读取 `~/.liftgate/settings.json` 中的 `agy_proxy`） |
| `--agy-auto-update` / `--no-agy-auto-update` | `AGY_AUTO_UPDATE` | `true`（或读取 `~/.liftgate/settings.json` 中的 `agy_auto_update`） |
| `--mcp-tool-bridge` / `--no-mcp-tool-bridge` | `AGY_MCP_TOOL_BRIDGE` | `true`（或读取 `~/.liftgate/settings.json` 中的 `mcp_tool_bridge`，亦可通过 `POST /api/telemetry/mcp-tool-bridge`、Web 面板与 `lg top` 按 `b` 实时切换） |
| `--vision-offload` / `--no-vision-offload` | `AGY_VISION_OFFLOAD` | `false`（或读取 `~/.liftgate/settings.json` 中的 `vision_offload`，可选 `>=300k` 视觉子会话卸载，亦可在 Web 面板与 `lg top` 按 `v` 实时切换） |
| `--max-low-cache-records` | `AGY_MAX_LOW_CACHE_RECORDS` | `1000`（`0..=50000`；原子持久化至 `~/.liftgate/cache-telemetry.json`，可在 Web 面板实时调整） |
| `--low-cache-threshold-pct` | `AGY_LOW_CACHE_THRESHOLD_PCT` | `50`（`0..=100%`；低于该命中率阈值的请求自动持久化归档） |
| `--low-cache-capture-payload` / `--no-low-cache-capture-payload` | `AGY_LOW_CACHE_CAPTURE_PAYLOAD` | `true`（低命中时在 `~/.liftgate/low-cache-payloads/` 落盘完整请求内容与消息明细，可在 Web 面板实时开关） |
| `--capture-full-context` / `--no-capture-full-context` | `AGY_CAPTURE_FULL_CONTEXT` | `false`（或读取 `~/.liftgate/settings.json` 中的 `capture_full_context`；开启后每一轮的完整上下文——消息历史、思考、发往 ACP 的 Prompt 与输出——落盘到 `~/.liftgate/context-captures/`，可在 Web 面板实时开关） |
| `--max-context-captures` | `AGY_MAX_CONTEXT_CAPTURES` | `200`（`1..=10000`；超出后先删除最旧的捕获） |
| `--max-context-capture-mib` | `AGY_MAX_CONTEXT_CAPTURE_MIB` | `512`（`16..=65536` MiB，完整上下文捕获的磁盘上限） |
| `--default-model`（请求未知或通用模型名时使用） | `AGY_DEFAULT_MODEL` | `gemini-4-carbon-high` |
| `--acp-mode`（`auto`、`accept-edits`、`default`、`plan`） | `AGY_ACP_MODE` | `auto` |
| `--acp-cwd` | `AGY_ACP_CWD` | 系统临时目录下的独立空目录 |
| `--api-key`（以 `Authorization: Bearer` 或 `x-api-key` 校验） | `AGY_GATEWAY_API_KEY` | 无，不校验 |
| `--ui-dir` | `AGY_UI_DIR` | 编译期内嵌的 `web/dist` |
| `--max-idle-sessions` | | `64` |
| `--max-stored-responses` | | `256` |
| `--turn-timeout-secs` | 网关单轮请求的总时限，单位为秒 | `1800` |
| `--max-body-bytes` | | 32 MiB |
| `--max-line-bytes`（单行 ACP JSON-RPC） | | 48 MiB |
| `--channel-capacity`（每个流） | | `256` |

日志级别由 `RUST_LOG` 控制（默认 `info`）。`liftgate --help` 输出同样的列表。

## 常见问题

- **请求卡住或被转发到代理**：把 `127.0.0.1,localhost,::1` 加入 `NO_PROXY`；若 `agy` 访问 Google Antigravity 需要走代理，运行 `lg proxy 7890`（或 `lg proxy <URL>`）。
- **`could not locate agy executable`**：直接运行 `lg agy install` 自动安装，或传入 `--agy-bin <路径>` / 设置 `AGY_BIN`。
- **`401`**：客户端密钥与 `--api-key` 不一致。
- **额度耗尽（`Individual quota reached` / `insufficient_quota`）**：当前 Antigravity 账号或模型的配额已用完（`agy` 返回重置倒计时时会一并展示），可用 `/model` 切换模型或等待配额重置。
- **后台运行的日志**：执行 `liftgate logs -f` 查看 `~/.liftgate/gateway.log`（使用 `./run-gateway` 脚本时位于 `.agy-runtime/gateway.{out,err}.log`）。
