# `agy acp 1.5.18` 实测验证与弹性读写（Elastic I/O）优化方案

- **实测日期**：2026-11-27
- **被测上游版本**：`Antigravity ACP Server 1.5.18`（官方 `agy acp --process-per-session --dangerously-skip-permissions` JSON-RPC 2.0 `stdio` 接口）
- **覆盖范围**：`liftgate-types`、`liftgate-tool-virtualizer`、`liftgate-server` 及四端适配器（Codex `cx`、Claude Code `cc`、OpenCode `oc`、Oh My Pi `omp`）

---

## 1. 核心结论速览

通过构建纯标准协议的本地回环 HTTP/1.1 MCP 探针服与加密随机金丝雀（Canary）脚本，对当前本机安装的 **`agy acp 1.5.18`** 进行了黑盒实测。结果打破了代码中两项旧假设，同时证实了两项关键硬悬崖仍然存在，并揭示了一条此前未被显式建模的**核心延迟定律**：

| 验证维度 | Liftgate 现有假设 / 常量 | `agy acp 1.5.18` 实测真相 | 策略调整结论 |
| --- | --- | --- | --- |
| **单次 MCP 工具返回截断** | `~10,240 B` 中间截断（当前卡在 `9,600 B` 落盘 Spill） | **依然严格存在**：精确触发阈值为 `raw_bytes + 78B(时间戳头) > 10,240 B (10 KiB)`；触发后仅保留首尾共 `9,894 B`；多块 `content: [Text, ...]` 会先拼接再按同一 `10 KiB` 截断，**无法绕过** | **必须保留** `MAX_INLINE_MCP_TOOL_RESULT_WITH_SPILL_BYTES = 9_600` 与 0-RTT `mcp_lg_read_context_spill`；同时必须在提示词中引入**预算感知切片（`< 8 KB` 摘要返回）**以降低被动溢出率 |
| **同轮多 MCP 工具并发调度** | 假设模型同轮发出多个 `mcp_lg_*` 可并发执行；提示词引导“发并行 `mcp_lg_*` / 逐文件拆发并行 `apply_patch`” | **完全串行 + 单次固定 ~0.82s 内耗**：即使模型在**同一 LLM Step** 内同时输出 3 个工具调用，且工具声明 `annotations.readOnlyHint = true`、HTTP 开启 Keep-Alive 且服务端 `0.2 ms` 瞬回，`agy acp` 仍严格按 `await call_1 -> await call_2 -> await call_3` 串行发起 HTTP 请求，每次调用间隔固定 **`810~840 ms`**（叠加网关 HTTP 挂起唤醒后每次调用实耗 **`1.2~1.6 s`**） | **彻底重构编排提示词**：废除 `cx`/`cc` 上的“同轮拆发多个并行 `mcp_lg_*`”与“每个文件单独一次 `apply_patch`”规则，全面转向 **Code Mode 单单元聚合（`exec`/`execute`/`eval`）+ Bash-First 复合命令 + 多文件原子 Patch** |
| **MCP `Eager` 工具数量上限** | 认为存在 **硬性 `50` 个工具数量上限**，因此严卡 `max_eager_tools = 24`、`direct_passthrough_max_tools = 20` | **`50` 数量上限已不存在，实为 `~55–58 KiB` 字节预算上限**：注册 `75` 个、`120` 个工具（总重 `~48 KB`）时全部达成 **`120 Eager / 0 Lazy`**；注册 `160~220` 个工具超预算时仍按定义体积从小到大贪心装箱至 `~55–58 KiB`，且 **`1.5.18` 对溢出工具直接静默丢弃（`lazy: 0`）而非降级为 `Lazy`** | **放宽数量阈值、严守字节红线**：将 `max_eager_tools` 放宽至 `40`、`direct_passthrough_max_tools` 放宽至 `36`（让绝大多数中等工具集会话 100% 直通免 Hub 跳转），同时将 `max_eager_wire_bytes` 严控在 `34 KiB` 以确保体积最大的编排器永不被静默丢弃 |
| **`systemPrompt.append` 容量** | 认为超过 `16 KB` 易触发上游 `429`，严卡 `MAX_SYSTEM_APPEND_BYTES = 16_000`、`compact_caller_system = 8_000` | **大幅放宽**：实测注入 **`39.8 KB`、`96.2 KB`、`160.3 KB`** 均一次通过，零 `429`、首尾金丝雀 100% 可读 | **上调静态追加上限，保留消息内指令不变式**：`MAX_SYSTEM_APPEND_BYTES` 放宽至 `48_000`（`compact_caller_system` 放宽至 `24_000`），避免无谓剥离 `<example>`；但动态 `<developer_instructions>` **继续保留在 `messages` 中**，防止中途切换 Plan/Default 模式击穿 `SessionPool` 热会话复用 |
| **`session/prompt` 单轮长度** | 卡在 `MAX_ACP_PROMPT_TEXT_BYTES = 176_000` 做中间水位线截断 | **存在极危险的尾部硬截断悬崖（精确到 `191,579 B`）**：无论传单个 `Text` 块（`217 KB`）还是拆成 2 个 `Text` 块（`2 × 122 KB`），`agy acp` 都会先拼接再从 **`191,579 B` 处直接砍掉尾部**（`<truncated N bytes>`），导致冷重放末尾的最新用户提问丢失 | **严格保留** `MAX_ACP_PROMPT_TEXT_BYTES = 176_000` 中间水位线裁剪与 Spill 机制（距 `191,579 B` 尾部断头台留有 `~15.5 KB` 安全裕量） |

---

## 2. `agy acp 1.5.18` 实测数据与证据链

> **合规说明**：所有测试均在临时空目录中通过官方 `agy acp --process-per-session --dangerously-skip-permissions` 的 JSON-RPC 2.0 `stdio` 接口驱动，挂载本地 `127.0.0.1:0` 测试 MCP 服完成黑盒观测，未读取任何宿主机私有目录或二进制内部状态。

### 2.1 实验一：MCP 工具返回截断阈值（单块与多块 `content`）

每次调用生成固定行宽并在 `0% / 25% / 50% / 75% / 100%` 偏移埋入当次随机生成的 6 位十六进制金丝雀令牌，由模型读取工具输出后原样回传可见金丝雀与截断提示文本：

| 返回形态与原始字节数 | `agy acp` 注入时间戳头后总长 | 是否触发截断 | `agy acp` 插入的精确截断标记 | 实际保留字节数 |
| --- | --- | --- | --- | --- |
| 单 `Text`：`9,600 B` | `9,678 B` | 否 | 无（全部 5 个金丝雀完整命中） | `9,678 B` |
| 单 `Text`：`9,950 B` | `10,028 B` | 否 | 无（中心金丝雀 `4F556A` 完整命中） | `10,028 B` |
| 单 `Text`：`10,150 B` | `10,228 B` | 否 | 无（距离 `10,240 B` 仅差 `12 B`，完整无损） | `10,228 B` |
| 单 `Text`：`10,175 B` | `10,253 B` | **是** | `<truncated 359 of 10253 bytes from the middle of the output>` | **`9,894 B`** |
| 单 `Text`：`12,000 B` | `12,078 B` | **是** | `<truncated 2184 of 12078 bytes from the middle of the output>` | **`9,894 B`** |
| 单 `Text`：`48,000 B` | `48,078 B` | **是** | `<truncated 38184 of 48078 bytes from the middle of the output>` | **`9,894 B`** |
| 单 `Text`：`96,000 B` | `96,078 B` | **是** | `<truncated 86184 of 96078 bytes from the middle of the output>` | **`9,894 B`** |
| 多 `Text` 数组：`8 × 8,000 B` | `63,477 B` | **是** | `<truncated 53583 of 63477 bytes from the middle of the output>` | **`9,894 B`** |

**推导出的精确公式**：
1. `agy acp` 会在每次 MCP 工具返回头部固定拼接两行时间戳（`Created At: ...\nCompleted At: ...\n`，共 `78` 字节）。
2. 当 `raw_utf8_bytes + 78 > 10,240`（即原始载荷 **`> 10,162` 字节**）时，硬触发中间截断，仅保留首部约 `4.95 KB` + 尾部约 `4.95 KB`（合计恒定 `9,894 B`）。
3. 多元素 `content: [{"type":"text"}, ...]` 在 `agy acp` 内部会先合并再执行同一套 `10,240 B` 检查，无法通过分块绕过。
4. 一旦触发 `agy` 自身截断，跨机远端客户端（Remote 模式）及禁用宿主 `read_file` 的网关会话将永久丢失中间上下文。因此 **`turn_runner.rs` 的 `9,600 B` 主动截断 + `mcp_lg_read_context_spill` 仍是不可移除的核心防线**。

### 2.2 实验二：同轮多 MCP 工具串行锁死与 `~820 ms` 单次固定内耗

使用 `gemini-4-carbon-low` 在**单轮单步**中同时发出 3 个工具调用，同时记录 `agy acp` `stdout` 的 `session/update` 事件时间戳与本地 `127.0.0.1` MCP HTTP 服务器的请求到达时间：

1. **加延迟探针（服务端每次 `sleep(400ms)`，工具显式标注 `"annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}`）**：
   - `t = 9,958.8 ms`：收到唯一一次推理前 `usage_update`（证明模型在**单次推理步**内一次性吐出了全部 3 个工具调用）。
   - `t = 9,972.2 ms`：`session/update` 发出第 1 个 `tool_call (in_progress)` → HTTP 到达 `11,097.4 ms` → 返回 `11,498.6 ms` → `11,506.0 ms` 发出 `tool_call_update (completed)`。
   - `t = 11,515.4 ms`（第 1 个完成后 `+9.4 ms`，中间**无新 LLM 请求**）：才发出第 2 个 `tool_call (in_progress)` → HTTP 到达 `12,493.9 ms` → 返回 `12,895.1 ms`。
   - `t = 12,909.5 ms`（第 2 个完成后 `+7.1 ms`）：才发出第 3 个 `tool_call (in_progress)` → HTTP 到达 `14,118.1 ms` → 返回 `14,519.4 ms`。
2. **HTTP Keep-Alive + 零延迟瞬回探针（服务端 `< 0.2 ms` 立即返回，排除连接握手开销）**：
   - `wire_log` 证实会话运行期**不会**重复发送 `initialize` 或 `tools/list`；
   - 3 次 `tools/call (fast_ping)` 到达时间戳分别为 `13,803.9 ms` → `14,643.9 ms`（**`+840.0 ms`**）→ `15,456.2 ms`（**`+812.3 ms`**）。

**工程含义**：
- `agy acp 1.5.18` 的 MCP 调用执行器是严格的串行循环，完全忽略 `readOnlyHint: true`，且每次 MCP 调用在 `agy` 进程内部自带 **`~820 ms` 固定调度开销**。
- 在 `turn_runner.rs:4571` 中把 `yield_now()` 改成 `sleep(50ms)` **不可能**聚批同轮的其它 MCP 调用（因为 `agy` 在收到第 1 个 HTTP 响应前根本不会发出第 2 个请求）。
- **真正的破局点只有一个：减少单轮发起的顶层 `mcp_lg_*` 调用次数**——把 6 次散装读取合并为 1 次 `mcp_lg_exec` / `mcp_lg_Bash` / `mcp_lg_execute` / `mcp_lg_eval`，把 7 次单文件 `mcp_lg_apply_patch` 合并为 1 次多文件 `mcp_lg_apply_patch` 或 1 次 `exec` 批处理，可直接削减 `80%~90%` 的固定往返等待。

### 2.3 实验三：MCP `Eager` 槽位与字节预算新规律

| 注册工具数与形态 | 总定义字节量（约） | 实测 `Eager` 数量 | 实测 `Lazy` 数量 | 保留 / 丢弃特征 |
| --- | --- | --- | --- | --- |
| `75` 个混合工具（前 40 小 + 后 35 中） | `~30 KB` | **`75`** | `0` | 100% 进入 `Eager`，突破旧 `50` 数量限制 |
| `120` 个均匀工具（每个 `~400 B`） | `~48 KB` | **`120`** | `0` | 100% 进入 `Eager` |
| `220` 个均匀工具（每个 `~420 B`） | `~92 KB` | **`139`** | **`0`** | 保留 `t_082..t_220`（约 `58.4 KB`），前 `81` 个直接静默消失（`lazy: 0`） |
| `160` 个奇偶双峰工具（80 个偶数 `120 B` + 80 个奇数 `1,260 B`） | `~110 KB` | **`116`** | **`0`** | 80 个小工具全留（`9.6 KB`）+ 36 个大工具留存（`45.4 KB`，合计 **`55.0 KB`**）；其余 44 个大工具静默消失（`lazy: 0`） |

**工程含义**：
之前在 128 工具会话中观察到的“留存 49 个 `lg` 工具（`48.6 KB`）”并非硬性的 `50` 工具个数上限，而是 **`~55–58 KiB` 序列化字节预算上限**（按单工具字节数升序贪心装填）。更关键的是，在 `1.5.18` 中溢出预算的大工具不再进入 `Lazy` 列表，而是**彻底从模型视野中静默消失**。因此虚拟化器的**字节上限绝对不能撤除**，但**工具个数上限可以安全放宽**。

### 2.4 实验四与实验五：`systemPrompt.append` 与 `session/prompt` 边界

- **`_meta.antigravity.systemPrompt.append`**：实测 `39,796 B`、`96,185 B`、`160,265 B` 全部正常创建并完成问答，首尾金丝雀 100% 匹配，无 `429` 报错。
- **`session/prompt`**：
  - 单块 `217,139 B` → 尾部出现 `<truncated 25560 bytes>`，净留存 **`191,579 B`**，末尾提问丢失；
  - 双块 `122,417 B + 122,549 B = 244,966 B` → 尾部出现 `<truncated 53387 bytes>`，净留存同样精确为 **`191,579 B`**，末尾提问丢失。
  - 证明 `agy acp` 对 `session/prompt` 采用极其危险的**尾部截断（Tail Truncation）**，现有的 `MAX_ACP_PROMPT_TEXT_BYTES = 176_000` 中间水位线裁剪必须严格保留。

---

## 3. 架构与参数优化方案（第一阶段：协议与虚拟化预算校准）

### 3.1 放宽 `VirtualizerBudget` 数量约束，严守字节水位线

**修改目标**：`crates/liftgate-tool-virtualizer/src/budget.rs`（同步更新 `docs/mcp-tool-virtualization.md` 注释）

| 字段 | 现值（`DEFAULT_AGY_ACP`） | 建议新值 | 调整依据 |
| --- | --- | --- | --- |
| `max_eager_tools` | `24` | **`40`** | `120` 个工具实测全部 Eager，放宽至 `40` 仍给宿主插件留有充足余量，大幅减少大工具集下的 Hub 折叠率 |
| `max_eager_wire_bytes` | `28 * 1024` (`28 KiB`) | **`34 * 1024` (`34 KiB`)** | 距 `~55–58 KiB` 静默丢弃悬崖保留 `21+ KiB` 安全缓冲，确保 `exec`/`execute`/`eval`/`Bash` 100% 留存 |
| `direct_passthrough_max_tools` | `20` | **`36`** | 让挂载 1~2 个常用 MCP 插件（总工具数 `21~36` 个且体积 `<= 28 KiB`）的会话直接走 1:1 零虚拟化直通，消除 `mcp_lg_ns__*` 间接层 |
| `direct_passthrough_max_non_orchestrator_bytes` | `22 * 1024` (`22 KiB`) | **`28 * 1024` (`28 KiB`)** | 与 `34 KiB` 总字节上限匹配 |
| `max_direct_core_tools` | `10~13` | **`18`**（各 Profile 对齐 `16~20`） | 虚拟化激活时，让 `AskUserQuestion`、`Skill`、`LSP`、`view_image` 等高频辅助工具全部保持 1:1 顶层直调 |

### 3.2 校准 `system_append.rs` 静态容量上限，保持 `<developer_instructions>` 增量热复用

**修改目标**：`crates/liftgate-types/src/session_key/system_append.rs`

1. **`MAX_SYSTEM_APPEND_BYTES`**：从 `16_000` 上调至 **`48_000`**（实测 `160 KB` 安全，取 `48 KB` 兼顾首轮 TTFT 与指令完整度），同步调整 `prompt_plan.rs` 中的编译期静态断言比例。
2. **`compact_caller_system` 裁剪阈值**：从 `8_000` 上调至 **`24_000`**，不再对中等长度的自定义系统提示词过早剥离 `<example>` 块。
3. **保持 `<developer_instructions>` 位于 `messages` 不变**：Codex 的 `<collaboration_mode>`（Plan/Default 切换）、`<permissions instructions>` 与动态 Skill 注入会在会话中途变化；而 `_meta.antigravity.systemPrompt.append` 只能在 `session/new` 时写入一次。将其留在 `messages` 中才能保证中途切换模式时不破坏 `SessionPool` 热会话前缀命中。

### 3.3 保持不变的两条硬防线（更新源码注释以记录 `1.5.18` 实测值）

1. **`MAX_INLINE_MCP_TOOL_RESULT_WITH_SPILL_BYTES = 9_600`**（`crates/liftgate-server/src/turn_runner.rs:3699`）：注释明确记录实测公式 `raw + 78B > 10,240B -> retains 9,894B`。
2. **`MAX_ACP_PROMPT_TEXT_BYTES = 176_000`**（`crates/liftgate-types/src/session_key/prompt_plan.rs:19`）：注释明确记录 `agy acp 1.5.18` 在 `191,579 B` 处执行跨块拼接后的**尾部硬截断**。

---

## 4. 弹性内容读写方案（第二阶段：Bash-First / Orchestrator-First 改造）

### 4.1 痛点本质：为什么当前四端读写在 `agy acp` 下容易变慢？

对比 Anthropic Claude Code 在 Auto 模式下的内部 **Bash-First** 提示词策略（*“只要 Bash 能完成，优先用 `cat`/`head`/`sed -n`/`grep`/`find` 和脚本/heredoc 完成读写，仅在 Bash 无法胜任时回退到 `Read`/`Edit`/`Write`”*），审视 Liftgate 当前的桥接规则（`system_append.rs:150, 195` 与 `CODEX_AGY_BASE_INSTRUCTIONS`），存在三处明显的“逆向负优化”：

1. **误导模型同轮发多个“并行” `mcp_lg_*` 工具**：
   当前 `McpOrchestratorContract` 仅覆盖了 OpenCode（`OpenCodeExecute`）和 Oh My Pi（`OmpPolyglotEval`）；Codex（`cx`）和 Claude Code（`cc`）均落入 `None` 分支，被注入：
   > `Batch independent read-only searches, reads, or checks as parallel mcp_lg_* calls in a single turn.`
   在 `agy acp` 严格串行 MCP 调度下，Claude Code 同轮发 6 个 `Read`/`Grep` 或 Codex 同轮发 6 个 `exec_command`，会被拆成 **6 轮串行 HTTP 挂起—唤醒往返（白白消耗 `~8 秒` 框架空转）**。
2. **强制将多文件编辑拆成一文件一次 `mcp_lg_apply_patch`**：
   当前 `system_append.rs:195` 要求：
   > `scope each mcp_lg_apply_patch call to a single target file — issuing parallel per-file mcp_lg_apply_patch calls when editing multiple files`
   修改或新建 7 个文件会触发 **7 轮串行 HTTP 往返（`~10.5 秒` 空转）+ Codex 客户端 `RwLock::write()` 排他锁排队**；且新建 350 行长文档时每行加 `+` 并做 JSON 字符串转义，既慢又费输出 Token。
3. **缺少对 `9.6 KB` 单次返回窗口的“预算感知读取”引导**：
   模型在 `exec` 或 `Bash` 中整文件 `cat` 多个大文件，超过 `9,600 B` 立即触发 Spill，一方面诱使模型退回散装单次工具调用，另一方面产生本可避免的二次 `read_context_spill` 往返。

### 4.2 为什么不能无脑照搬“纯 `sed -i` / 纯 Shell 裸写一切”？

直接把所有客户端都改成“禁止用 `Edit`/`apply_patch`，全部用 `sed -i` / heredoc 改代码”在工程上会引入副作用，必须做**分级弹性（Elastic Tiering）**而非一刀切：

| 客户端机制 | 专用编辑工具（`apply_patch` / `Edit` / `MultiEdit`） | 纯 Shell 裸写（`sed -i` / `cat <<EOF` / `python3`） |
| --- | --- | --- |
| **UI 净 Diff 与回滚快照** | Codex `TurnDiffTracker` 实时计算本轮净 Diff；Claude Code 自动生成 `/rewind` 文件检查点 | 绕过会话级检查点与结构化补丁预览（仅依赖底层 `git diff`） |
| **权限与沙箱审批模型** | 在 `acceptEdits` / 受管工作区写模式下通常免弹窗静默放行；Codex Guardian 对诸如 `rm -f` 等危险 Shell 语法有额外拦截策略 | 在 `on-request` / Manual 审批模式下每条写文件 Shell 命令可能触发人工确认；在只读沙箱下会被拒绝 |
| **新建大文件 / 跨多文件机械重构** | 每行 `+` 前缀与上下文匹配脆弱、多文件拆单往返极慢 | **极快**：单次调用流式落盘或脚本批量精确改写，零上下文行号漂移报错 |

因此，最优解是建立**“感知编排器能力 + 感知权限模式 + 感知输出预算”的弹性读写契约（Elastic I/O Contract）**。

---

### 4.3 弹性读取策略（Budget-Aware Compound Read）

核心原则：**把 `N` 次细粒度只读调用折叠进 `1` 次可编程/复合调用，并在本地将单次输出主动裁剪在 `<= 8 KB` 以内（零 Spill、单往返）**。

1. **Codex（`cx`，启用 Code Mode `exec` 时）—— `CodexExec` 编排器优先**：
   - 在 `McpOrchestratorContract` 中新增 `CodexExec { mcp_name, has_apply_patch }` 变体，由 `CodexToolProfile::orchestrator_prompt_contract` 自动识别。
   - 多文件检索、符号定位、分段读取统一放进**单个 `mcp_lg_exec` 单元**，在 V8 内用 `await Promise.all([tools.exec_command(...), ...])` 真正并发拉起本地只读子进程（Codex 内部 `ExecCommandHandler` 持 `RwLock::read()` 共享读锁，本机真并发，而对 `agy acp` 只计 **1 次** MCP 调用）。
   - **8 KB 内联预算纪律**：在 V8 中将原始长输出存入 `store(key, raw)`，用 JS `.slice()` / 行过滤仅通过 `text(...)` 返回 `<= 8 KB` 的精简摘要与行号区间，彻底规避 `9,600 B` Spill 截断。
2. **Claude Code（`cc`）—— 网关级 Bash-First 弹性探查契约（`ClaudeBashFirst`）**：
   - 在 `McpOrchestratorContract` 中新增 `ClaudeBashFirst { bash_mcp_name }`（当工具集包含 `Bash` 且无 V8 Code Mode 时激活）。
   - 显式覆盖 Claude Code 默认提示词中“禁止用 Bash 跑 `rg`/`sed`/`cat`”的保守限制（因为该限制是为直连低延迟 API 设计的，在 `agy acp` 单次 `1.3s` 串行 MCP 开销下会造成严重劣化）：
     - **多步只读探查走单条复合 `mcp_lg_Bash`**：用一条命令（如 `rg -n 'pattern' src/; sed -n '80,160p' src/a.rs; sed -n '1,90p' src/b.rs`）一次性拿回多文件带行号切片（控制在 `~200` 行 / `< 8 KB` 内），将原本 5~8 轮 `Glob` + `Grep` + `Read` 压缩为 **1 轮**。
     - **保留专用工具的高价值场景**：读取图片/PDF/Jupyter Notebook、已知单文件精确区间且无需组合查询、或当前处于受限审批模式（`Bash` 需人工确认而 `Read` 免审）时，直接使用 `mcp_lg_Read`。
3. **OpenCode（`oc`）与 Oh My Pi（`omp`）—— 补齐 `< 8 KB` 预算切片规则**：
   - 在现有 `OpenCodeExecute` 与 `OmpPolyglotEval` 提示词中补充单次返回 `< 8 KB` 切片约束：在 Bun JS / Python 内核中先过滤出目标定义与上下文行再输出，避免整文件打印触发 Spill。

---

### 4.4 分级弹性写入策略（Tiered Elastic Write）

废除当前 `system_append.rs:195` 中“每次 `mcp_lg_apply_patch` 只能改一个文件”的硬性拆分令，改为按场景弹性路由的三级写入策略：

#### Tier W1：常规代码精准修改与少量文件联动（1~4 个文件）→ 单次多文件原子 Patch / 编排器内聚 Patch
- **Codex（`cx`）**：
  - 当已核对目标行上下文（或包含零匹配风险的 `*** Add File:` 新文件块）时，**允许并推荐在单次 `mcp_lg_apply_patch` 调用中合并多个文件的 `*** Add File:` / `*** Update File:` 块**，将 `N` 次串行往返降为 **1 次**，同时 100% 保留 `TurnDiffTracker` 净 Diff 与沙箱校验。
  - 当一次性修改 **4 个以上已存在文件**且希望获得逐文件容错隔离（防止单文件 hunk 失配回滚整批）时，在**单个 `mcp_lg_exec` 单元**内通过 `Array.join("\n")` 构造补丁并调用 `await Promise.allSettled(patches.map(p => tools.apply_patch(p)))`（实测 `apply_patch` 已原生挂载在 Codex `exec` 的 `ALL_TOOLS` 中）：这样既在 Codex 本地获得逐文件独立校验与完整 `TurnDiffTracker` 记录，又在 `agy acp` 侧只消耗 **1 次** MCP 往返！
  - 仅在单次多文件 `apply_patch` 实际返回 `apply_patch verification failed` 时，才对报错的那一个文件重读精确行号并单独重发。
- **Claude Code（`cc`）**：
  - 单文件小改动使用 `mcp_lg_Edit`（保留 `/rewind` 快照与清晰 UI Diff）；若存在 `MultiEdit` 优先单次调用完成同一文件的多处非相邻修改。

#### Tier W2：长文件新建与跨文件批量机械改写 → 脚本 / Heredoc 流式写入（Bash-First Write，受权限门控）
- **适用场景**：
  - 一次性新建数百行文档、配置、测试夹具或生成代码（避免在 JSON 字符串内为几百行文本逐行拼接 `+` 前缀与双层转义）；
  - 跨 5+ 个文件的确定性结构化替换、重命名或格式化脚本。
- **执行方式**：
  - **在免审 / 全权限 / 自动接受工作区写入模式下**（例如 Codex `danger-full-access` 或无交互拦截的 `workspace-write`、Claude Code `acceptEdits` / `auto` / `bypassPermissions`、OpenCode `full_code_mode`、OMP `eval`）：
    允许直接通过 `mcp_lg_Bash` / `mcp_lg_exec_command`（`python3 - <<'PY'`、`cat <<'EOF' > path`、单次脚本生成多文件）或 `mcp_lg_execute`（`Bun.write`）/ `mcp_lg_eval`（`pathlib.Path.write_text`）一次性完成创建或批量修改，并紧跟 `git status -s` / `git diff --stat` / 编译校验命令在同一调用内闭环！
- **安全与权限护栏（严禁越界）**：
  - 若 `<collaboration_mode>` 处于 `Plan Mode` 或 `<permissions instructions>` 为 `read-only`，严禁任何写盘操作；
  - 若客户端处于交互式命令审批模式（`approval_policy = on-request` / Manual Mode，Shell 写操作会弹窗而 `Edit`/`Write`/`apply_patch` 走专用文件审批流），自动回退至 Tier W1 原生补丁工具；
  - 遵循客户端命令安全策略（如在 Codex 下避免触发 Guardian 拦截的 `rm -f` 等危险模式，临时脚本优先使用 `python3 - <<'PY'` 标准输入管道零临时文件执行）。

#### Tier W3：网关预检与自愈提示同步刷新
- 更新 `crates/liftgate-server/src/turn_runner.rs:4074` 中 `apply_patch verification failed:` 的附加提示：去掉强制拆分所有文件的教条指令，改为引导“先核对报错文件的精确上下文行（若此前输出被截断则调用 `mcp_lg_read_context_spill`），仅针对失败文件重发修正后的补丁”。

---

## 5. 具体代码落地清单（Implementation Blueprint）

以下改动严格遵循工作区单向依赖图（`liftgate-server` / `liftgate-harness` → `liftgate-tool-virtualizer` → `liftgate-types`）及 `docs/2026-11-09-RUST.md` 工程规范：

### 5.1 `crates/liftgate-types`

1. **`src/session_key/codemode.rs`**：
   扩展 `McpOrchestratorContract<'a>` 枚举，覆盖全部四端：
   ```rust
   pub enum McpOrchestratorContract<'a> {
       /// OpenAI Codex Code Mode (`exec` V8 isolate wrapping `tools.exec_command`, `tools.apply_patch`, etc.).
       CodexExec {
           mcp_name: &'a str,
           has_apply_patch: bool,
       },
       /// Anthropic Claude Code Bash-first compound I/O (`Bash` alongside `Read`/`Edit`/`Write`).
       ClaudeBashFirst {
           bash_mcp_name: &'a str,
       },
       /// OpenCode `@opencode/codemode` (`execute`).
       OpenCodeExecute {
           mcp_name: &'a str,
           full_code_mode: bool,
       },
       /// Oh My Pi polyglot kernel (`eval`).
       OmpPolyglotEval {
           mcp_name: &'a str,
           elided_subordinate_tools: bool,
       },
   }
   ```
2. **`src/session_key/system_append.rs`**：
   - 上调 `MAX_SYSTEM_APPEND_BYTES`：`16_000` → `48_000`；`compact_caller_system` 免裁剪阈值：`8_000` → `24_000`。
   - 重写 `batch_clause` 与文件读写规则：
     - **彻底移除**宣称“同轮发多个并行 `mcp_lg_*` 调用”与“每次 `mcp_lg_apply_patch` 只准改单文件”的旧文案；
     - 针对 `CodexExec`：引导用单个 `mcp_lg_exec` 单元通过 `Promise.all` 聚合多路只读 `tools.exec_command`、用 `store()`/`load()` 缓存原文并将 `text(...)` 输出控制在 `<= 8 KB`；编辑 1~4 个文件直接合并进单次 `mcp_lg_apply_patch`，多文件独立容错可在 `mcp_lg_exec` 内调用 `tools.apply_patch`，大文件新建/批量脚本生成在权限允许时可走单次 `exec_command`（`python3 - <<'PY'` / heredoc）并同轮串接校验命令；
     - 针对 `ClaudeBashFirst`：注入弹性 **Bash-First** 规则——多文件搜索、行号区间切片读取（`rg -n`、`sed -n`、`git diff`）以及免审模式下的大文件生成/批量机械改写统一收敛到单条复合 `mcp_lg_Bash` 调用（单次输出控制在 `<= 8 KB`），小范围精准代码修改继续使用 `mcp_lg_Edit`（保留 `/rewind` 检查点与 Diff 视图），图片/PDF 读取使用 `mcp_lg_Read`；
     - 针对 `OpenCodeExecute` 与 `OmpPolyglotEval`：追加 `<= 8 KB` 结果过滤约束，减少 Spill 触发。
3. **`src/openai.rs`（`CODEX_AGY_BASE_INSTRUCTIONS`）**：
   - 更新工作原则：明确单次工具返回内联窗口（`< 8 KB` 精简摘要 + `store()` 状态复用），以及“小步精准修改用单次多文件 `apply_patch`、大段新建或批量机械生成在写权限允许时可结合 `exec` / `exec_command` 脚本一次完成并同命令完成静态检查”。

### 5.2 `crates/liftgate-tool-virtualizer`

1. **`src/budget.rs`**：
   - 按第 3.1 节表格更新 `DEFAULT_AGY_ACP` 及四个客户端子预设（`max_eager_tools = 40`、`max_eager_wire_bytes = 34 * 1024`、`direct_passthrough_max_tools = 36`、`direct_passthrough_max_non_orchestrator_bytes = 28 * 1024`、`max_direct_core_tools = 18`），并更新文档注释说明 `agy acp 1.5.18` 的 `~55–58 KiB` 字节预算与 `lazy: 0` 静默丢弃行为。
2. **`src/profiles/codex.rs` & `src/profiles/claude_code.rs`**：
   - 在 `CodexToolProfile::orchestrator_prompt_contract` 中检测 `exec`（及是否存在 `apply_patch`），返回 `Some(McpOrchestratorContract::CodexExec { ... })`；
   - 在 `ClaudeCodeToolProfile::orchestrator_prompt_contract` 中当无 `REPL` 但存在 `Bash` 时，返回 `Some(McpOrchestratorContract::ClaudeBashFirst { bash_mcp_name: "Bash" })`。

### 5.3 `crates/liftgate-server`

1. **`src/turn_runner.rs`**：
   - 更新 `MAX_INLINE_MCP_TOOL_RESULT_WITH_SPILL_BYTES` 与 `call_rx_guard.recv()` 处的源码注释，记录 `agy acp 1.5.18` 实测的 `10,240 B` 截断公式与串行 `~820 ms/call` 调度特征；
   - 更新第 4074 行的 `apply_patch verification failed` 自愈提示，仅要求对失配文件重读行号（或查 Spill）后重试，不再诱导模型将所有多文件编辑拆成多次串行调用。

---

## 6. 预期收益与验收指标

落地上述两阶段优化后，可通过 `lg top` 与 Web Dashboard 遥测面板直接观测以下量化改善：

1. **多文件探索轮次与端到端耗时下降 `60%~85%`**：
   原本 6 次散装 `Read` / `Grep` / `exec_command`（消耗 `6 × ~1.4s ≈ 8.4s` 串行挂起往返）收敛为 **1 次** `mcp_lg_exec` 或复合 `mcp_lg_Bash`（`~1.3s`），Dashboard 不再刷屏式出现一连串单命令子请求。
2. **多文件写入与长文档生成提速 `3x~6x`**：
   7 个文件的文档/代码更新从 7 轮串行 `mcp_lg_apply_patch`（`~10.5s` 往返开销）收敛为 **1 轮**多文件 `mcp_lg_apply_patch`、`mcp_lg_exec` 批处理或单次脚本流式落盘（`~1.5s`）。
3. **中等规模插件会话 100% 零虚拟化直通**：
   工具总数在 `21~36` 个且压缩后定义体积 `<= 28 KiB` 的会话不再触发 `mcp_lg_ns__*` 折叠，省去 `search_client_tools` 发现步骤，同时严格守住 `34 KiB < 55 KiB` 安全线，确保零工具静默丢失。
4. **被动 Spill 触发率显著下降**：
   通过提示词显式对齐 `8 KB` 内联预算与 `store()` 缓存策略，避免盲目整文件打印撞上 `9,600 B` 截断墙，进一步稳固 `SessionPool` `90%~96%` 的 KV 前缀缓存命中率。
