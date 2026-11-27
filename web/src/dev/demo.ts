/**
 * Synthetic gateway for `vite dev` (`?demo`). It implements the full
 * `GatewayClient` contract, including the issue #21 endpoints, so every view
 * can be exercised without a running gateway. Never imported in production.
 */
import type {
  CacheConfigUpdate,
  GatewayClient,
  SnapshotHandlers,
} from "@/lib/api";
import { ApiError } from "@/lib/api";
import type {
  AvailableModel,
  CacheTelemetrySummary,
  CapturedFullMessage,
  CapturedFullPart,
  CatalogToolEntry,
  CatalogToolTier,
  CatalogWireTool,
  ContextCaptureConfigUpdate,
  ContextCaptureDetailResponse,
  ContextCaptureRecord,
  ContextCaptureSummary,
  ContextCapturesResponse,
  ContextSegment,
  ContextSegmentKey,
  FullContextPayload,
  GatewayTelemetrySnapshot,
  LowCacheHitRecord,
  LowCacheHitsResponse,
  LowCachePayloadDetailResponse,
  SessionSummary,
  SessionToolCatalog,
  TaskProtocol,
  TaskSnapshot,
  ToolCallDetail,
  ToolCallErrorKind,
  ToolCallRecord,
  ToolCallRoute,
  ToolCallStatus,
  ToolCallsResponse,
  ToolCallTelemetrySummary,
  ToolOutputBlock,
} from "@/types/telemetry";

/* ---------- deterministic randomness ---------- */

function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const rand = mulberry32(21);
const between = (lo: number, hi: number): number => Math.round(lo + rand() * (hi - lo));
const chance = (p: number): boolean => rand() < p;
function pick<T>(items: readonly T[]): T {
  const item = items[Math.floor(rand() * items.length)];
  if (item === undefined) throw new Error("pick() from an empty list");
  return item;
}
const hex = (len: number): string =>
  Array.from({ length: len }, () => Math.floor(rand() * 16).toString(16)).join("");
const uuid = (): string => `${hex(8)}-${hex(4)}-${hex(4)}-${hex(4)}-${hex(12)}`;
const bytes = (text: string): number => new TextEncoder().encode(text).length;

/* ---------- content pools ---------- */

const FILES = [
  "crates/liftgate-server/src/routes.rs",
  "crates/liftgate-server/src/mcp_bridge.rs",
  "crates/liftgate-server/src/turn_runner.rs",
  "crates/liftgate-server/src/telemetry.rs",
  "crates/liftgate-tool-virtualizer/src/planner.rs",
  "crates/liftgate-tool-virtualizer/src/profiles/omp.rs",
  "crates/liftgate-types/src/telemetry.rs",
  "web/src/App.tsx",
] as const;

const RUST_LINES = [
  "pub async fn handle_telemetry_snapshot<B: AcpBackend>(",
  "    State(state): State<AppState<B>>,",
  ") -> Result<Json<GatewayTelemetrySnapshot>, GatewayError> {",
  "    let snapshot = state.telemetry.snapshot().await;",
  "    Ok(Json(snapshot))",
  "}",
  "",
  "impl ToolCallRing {",
  "    /// Evicts the oldest record once either cap is exceeded.",
  "    pub fn push(&mut self, record: ToolCallRecord) {",
  "        self.stored_bytes += record.approx_bytes();",
  "        self.records.push_back(record);",
  "        while self.records.len() > self.max_records || self.stored_bytes > self.max_total_bytes {",
  "            let Some(evicted) = self.records.pop_front() else { break };",
  "            self.stored_bytes = self.stored_bytes.saturating_sub(evicted.approx_bytes());",
  "            self.total_evicted += 1;",
  "        }",
  "    }",
  "}",
] as const;

function fileBody(lines: number, numbered = false): string {
  const out: string[] = [];
  for (let i = 0; i < lines; i += 1) {
    const line = RUST_LINES[i % RUST_LINES.length] ?? "";
    out.push(numbered ? `${String(i + 1).padStart(5, " ")}\t${line}` : line);
  }
  return out.join("\n");
}

const JS_SCRIPTS = [
  `// Locate the telemetry routes and the ring buffer that backs them.
const files = await tools.exec_command({ cmd: "rg --files crates/liftgate-server/src | sort" });
const routes = await tools.exec_command({
  cmd: "rg -n '/api/telemetry' crates/liftgate-server/src/routes.rs",
});
const ring = await tools.exec_command({ cmd: "rg -n 'struct .*Ring' crates/ -g '*.rs'" });
text([files.output, routes.output, ring.output].join("\\n---\\n"));`,
  `const targets = ["routes.rs", "telemetry.rs", "mcp_bridge.rs"];
const results = await Promise.all(
  targets.map((name) =>
    tools.exec_command({ cmd: \`wc -l crates/liftgate-server/src/\${name}\` }),
  ),
);
for (const r of results) text(r.output.trim());

await tools.apply_patch(\`*** Begin Patch
*** Update File: crates/liftgate-server/src/telemetry.rs
@@
-    pub max_records: usize,
+    pub max_records: usize,
+    pub max_record_bytes: usize,
*** End Patch\`);
const check = await tools.exec_command({ cmd: "cargo check -p liftgate-server --locked 2>&1 | tail -n 12" });
text(check.output);`,
  `const plan = await tools.update_plan({
  plan: [
    { step: "Record tool-call I/O in turn_runner", status: "in_progress" },
    { step: "Expose /api/telemetry/tool-calls", status: "pending" },
  ],
});
const diff = await tools.exec_command({ cmd: "git diff --stat" });
text(diff.output);`,
] as const;

const PY_SCRIPTS = [
  `import json

listing = tools.bash(command="ls crates/liftgate-tool-virtualizer/src/profiles")
profile = tools.read(path="crates/liftgate-tool-virtualizer/src/profiles/omp.rs", limit=80)
print(listing["stdout"])
print(profile["content"][:1200])`,
  `hits = tools.grep(pattern="FoldedIntoOrchestrator", path="crates")
for hit in hits["matches"][:20]:
    print(f"{hit['file']}:{hit['line']}: {hit['text']}")
result = tools.edit(
    path="crates/liftgate-tool-virtualizer/src/profiles/omp.rs",
    old_text="const INLINE_LIMIT: usize = 36 * 1024;",
    new_text="const INLINE_LIMIT: usize = 48 * 1024;",
)
print(json.dumps(result))`,
] as const;

const V4A_PATCH = `*** Begin Patch
*** Update File: crates/liftgate-server/src/telemetry.rs
@@ impl ToolCallRing {
     pub fn push(&mut self, record: ToolCallRecord) {
-        self.records.push_back(record);
+        self.stored_bytes += record.approx_bytes();
+        self.records.push_back(record);
+        self.evict_over_budget();
     }
*** Add File: crates/liftgate-server/src/tool_calls.rs
+//! Bounded ring buffer for per-call tool I/O.
+pub const DEFAULT_MAX_RECORDS: usize = 2_000;
+pub const DEFAULT_MAX_RECORD_BYTES: usize = 256 * 1024;
*** Update File: crates/liftgate-server/src/lib.rs
@@
 mod telemetry;
+mod tool_calls;
 mod turn_runner;
*** End Patch`;

const SHELL_COMMANDS = [
  "cargo check --workspace --all-targets --locked 2>&1 | tail -n 20",
  "rg -n 'client_tool_calls' crates/ -g '*.rs' | head -40",
  "git status --short && git log --oneline -5",
  "cargo test -p liftgate-server telemetry:: --locked 2>&1 | tail -n 30",
  "ls -la web/src/components",
] as const;

const SHELL_OUTPUTS = [
  "    Checking liftgate-types v0.2.15\n    Checking liftgate-tool-virtualizer v0.2.15\n    Checking liftgate-server v0.2.15\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 6.41s",
  " M crates/liftgate-server/src/telemetry.rs\n M crates/liftgate-types/src/telemetry.rs\n?? crates/liftgate-server/src/tool_calls.rs\n7e82ddb Implement agy acp 1.5.18 Elastic I/O optimization & telemetry (v0.2.15) (#23)\n86f38a5 Add remote gateway guides and agy acp 1.5.18 elastic I/O plan",
  "running 14 tests\ntest telemetry::tests::ring_evicts_oldest ... ok\ntest telemetry::tests::summary_tracks_bytes ... ok\ntest telemetry::tests::revision_bumps_on_push ... ok\n\ntest result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.08s",
] as const;

const RUST_ERROR = `error[E0308]: mismatched types
   --> crates/liftgate-server/src/telemetry.rs:412:37
    |
412 |         self.stored_bytes -= record.approx_bytes;
    |                              ^^^^^^^^^^^^^^^^^^^ expected \`usize\`, found fn item
    |
help: use parentheses to call this method
    |
412 |         self.stored_bytes -= record.approx_bytes();
    |                                                 ++

error: could not compile \`liftgate-server\` (lib) due to 1 previous error`;

/* ---------- tool-call drafts ---------- */

interface Draft {
  wire: string;
  client?: string;
  route: ToolCallRoute;
  orchestrator?: boolean;
  language?: string;
  wireArgs: string;
  clientArgs?: string;
  blocks: ToolOutputBlock[];
  status?: ToolCallStatus;
  errorKind?: ToolCallErrorKind;
  repaired?: boolean;
  sanitized?: boolean;
  spilled?: boolean;
  /** Milliseconds, inclusive range. */
  duration: [number, number];
}

const textBlock = (text: string): ToolOutputBlock => ({ type: "text", bytes: bytes(text), text });
const jsonBlock = (value: unknown): ToolOutputBlock => textBlock(JSON.stringify(value, null, 2));

function envelope(body: string, code = 0): string {
  return `Chunk ID: ${hex(6)}\nWall time: ${(rand() * 4).toFixed(4)} seconds\nProcess exited with code ${code}\nOriginal token count: ${Math.round(body.length / 4)}\nOutput:\n${body}\n`;
}

const drafts = {
  codexExec(): Draft {
    const failed = chance(0.04);
    return {
      wire: "exec",
      route: "client_roundtrip",
      orchestrator: true,
      language: "js",
      wireArgs: pick(JS_SCRIPTS),
      blocks: failed
        ? [textBlock("ReferenceError: tools.exec_comand is not a function\n    at <exec>:3:27")]
        : [textBlock(`${pick(SHELL_OUTPUTS)}\n---\n${fileBody(between(6, 60))}`)],
      status: failed ? "error" : "ok",
      errorKind: failed ? "script_exception" : undefined,
      duration: [900, 5200],
    };
  },
  codexShell(): Draft {
    const failed = chance(0.06);
    return {
      wire: "exec_command",
      route: "client_roundtrip",
      wireArgs: JSON.stringify({ cmd: pick(SHELL_COMMANDS), workdir: "/Users/dev/liftgate", yield_time_ms: 10000 }),
      blocks: [textBlock(envelope(failed ? RUST_ERROR : pick(SHELL_OUTPUTS), failed ? 101 : 0))],
      status: failed ? "error" : "ok",
      errorKind: failed ? "tool_error" : undefined,
      sanitized: chance(0.3),
      duration: [120, 7800],
    };
  },
  applyPatch(): Draft {
    return {
      wire: "apply_patch",
      route: "client_roundtrip",
      wireArgs: V4A_PATCH,
      blocks: [
        textBlock(
          "Success. Updated the following files:\nM crates/liftgate-server/src/telemetry.rs\nA crates/liftgate-server/src/tool_calls.rs\nM crates/liftgate-server/src/lib.rs",
        ),
      ],
      repaired: chance(0.25),
      duration: [40, 260],
    };
  },
  preflightReject(): Draft {
    return {
      wire: "apply_patch",
      route: "zero_rtt_preflight",
      wireArgs: V4A_PATCH.replace("\n*** End Patch", ""),
      blocks: [
        textBlock(
          "Preflight rejected: V4A patch is missing the closing '*** End Patch' line. Re-send the complete patch envelope.",
        ),
      ],
      status: "preflight_rejected",
      errorKind: "preflight_rejected",
      duration: [0, 2],
    };
  },
  search(): Draft {
    const query = pick(["github pull request", "linear issue", "browser screenshot", "sentry events"]);
    const hits = Array.from({ length: between(3, 6) }, (_, i) => ({
      name: `mcp__${query.split(" ")[0]}__${pick(["create", "get", "list", "update"])}_${pick(["issue", "pull_request", "comment", "page"])}`,
      score: Number((0.94 - i * 0.07).toFixed(2)),
      signature: "(args: { owner: string; repo: string; title: string; body?: string }) => Promise<unknown>",
    }));
    return {
      wire: "mcp_lg_search_client_tools",
      route: "zero_rtt_search",
      wireArgs: JSON.stringify({ query, limit: hits.length }),
      blocks: [jsonBlock({ query, hits })],
      duration: [0, 3],
    };
  },
  spillRead(): Draft {
    const body = fileBody(between(120, 220));
    return {
      wire: "mcp_lg_read_context_spill",
      route: "zero_rtt_spill_read",
      wireArgs: JSON.stringify({ spill_id: `spill_${hex(8)}`, offset: 9600, limit: 9600 }),
      blocks: [textBlock(body)],
      duration: [0, 4],
    };
  },
  hub(): Draft {
    const clientArgs = {
      owner: "acking-you",
      repo: "liftgate",
      title: "Record per-call tool I/O in a bounded ring buffer",
      head: "feat/tool-call-telemetry",
      base: "main",
      draft: true,
    };
    return {
      wire: "mcp_lg_ns__github",
      client: "mcp__github__create_pull_request",
      route: "namespace_hub",
      wireArgs: JSON.stringify({ action: "create_pull_request", arguments: clientArgs }),
      clientArgs: JSON.stringify(clientArgs),
      blocks: [
        jsonBlock({ number: 24, state: "open", draft: true, html_url: "https://github.com/acking-you/liftgate/pull/24" }),
      ],
      duration: [600, 2400],
    };
  },
  dispatcher(): Draft {
    const failed = chance(0.3);
    const clientArgs = { id: `LG-${between(100, 400)}`, include_comments: true };
    return {
      wire: "mcp_lg_call_client_tool",
      client: "mcp__linear__get_issue",
      route: "generic_dispatcher",
      wireArgs: JSON.stringify({ name: "mcp__linear__get_issue", arguments: clientArgs }),
      clientArgs: JSON.stringify(clientArgs),
      blocks: failed
        ? [textBlock("MCP error -32602: Invalid params: 'id' must match /^[A-Z]+-\\d+$/")]
        : [
            jsonBlock({ id: clientArgs.id, title: "Dashboard: tool-call I/O view", state: "In Progress", priority: 2 }),
          ],
      status: failed ? "error" : "ok",
      errorKind: failed ? "tool_error" : undefined,
      duration: [300, 1500],
    };
  },
  webFetch(): Draft {
    return {
      wire: "mcp_lg_gateway_web_fetch",
      route: "gateway_managed",
      wireArgs: JSON.stringify({ url: "https://github.com/acking-you/liftgate/issues/21", max_bytes: 65536 }),
      blocks: [
        textBlock(
          "# Web 监控面板支持工具调用完整 I/O、工具分布与提示词可视化\n\n## 背景与动机\n\n目前 Liftgate 已经支持多家 Harness 的 CodeMode 与 3 层 MCP 工具虚拟化…\n\n- Codex 的 CodeMode 表现显著优于其他家\n- 其他家 Harness 的 CodeMode 要么 Token 消耗依然偏高，要么频繁出现工具调用报错",
        ),
      ],
      duration: [400, 2100],
    };
  },
  claudeBash(): Draft {
    const failed = chance(0.1);
    return {
      wire: "Bash",
      route: "client_roundtrip",
      wireArgs: JSON.stringify({ command: pick(SHELL_COMMANDS), description: "Run workspace checks", timeout: 120000 }),
      blocks: failed
        ? [textBlock(RUST_ERROR), textBlock("Exit code 101")]
        : [textBlock(pick(SHELL_OUTPUTS))],
      status: failed ? "error" : "ok",
      errorKind: failed ? "tool_error" : undefined,
      duration: [200, 9000],
    };
  },
  claudeRead(): Draft {
    const big = chance(0.25);
    return {
      wire: "Read",
      route: "client_roundtrip",
      wireArgs: JSON.stringify({ file_path: `/Users/dev/liftgate/${pick(FILES)}`, offset: 1, limit: big ? 800 : 120 }),
      blocks: [textBlock(fileBody(big ? between(420, 700) : between(40, 120), true))],
      spilled: big,
      duration: [8, 60],
    };
  },
  claudeEdit(): Draft {
    const failed = chance(0.12);
    return {
      wire: "Edit",
      route: "client_roundtrip",
      wireArgs: JSON.stringify({
        file_path: `/Users/dev/liftgate/${pick(FILES)}`,
        old_string: "        self.records.push_back(record);",
        new_string: "        self.stored_bytes += record.approx_bytes();\n        self.records.push_back(record);\n        self.evict_over_budget();",
      }),
      blocks: [
        textBlock(
          failed
            ? "String to replace not found in file.\nString:         self.records.push_back(record);"
            : "The file has been updated successfully.",
        ),
      ],
      status: failed ? "error" : "ok",
      errorKind: failed ? "tool_error" : undefined,
      duration: [10, 90],
    };
  },
  claudeRepl(): Draft {
    return {
      wire: "REPL",
      route: "client_roundtrip",
      orchestrator: true,
      language: "js",
      wireArgs: JSON.stringify({ code: pick(JS_SCRIPTS), description: "Batch the lookups in one round-trip" }),
      blocks: [textBlock(pick(SHELL_OUTPUTS))],
      duration: [700, 3800],
    };
  },
  screenshot(): Draft {
    return {
      wire: "mcp_lg_ns__playwright",
      client: "mcp__playwright__browser_take_screenshot",
      route: "namespace_hub",
      wireArgs: JSON.stringify({ action: "browser_take_screenshot", arguments: { fullPage: false, type: "png" } }),
      clientArgs: JSON.stringify({ fullPage: false, type: "png" }),
      blocks: [
        textBlock("Took the viewport screenshot and saved it as page-2026-11-27.png"),
        { type: "image", mime_type: "image/png", base64_bytes: 184_320, width: 1440, height: 900 },
      ],
      duration: [500, 1600],
    };
  },
  opencodeExecute(): Draft {
    const failed = chance(0.2);
    return {
      wire: "execute",
      route: "client_roundtrip",
      orchestrator: true,
      language: "js",
      wireArgs: JSON.stringify({ code: pick(JS_SCRIPTS).replaceAll("exec_command", "bash").replaceAll("cmd:", "command:") }),
      blocks: failed
        ? [textBlock("TypeError: Cannot read properties of undefined (reading 'output')\n    at execute (codemode:5:18)")]
        : [textBlock(`${pick(SHELL_OUTPUTS)}\n${fileBody(between(20, 140))}`)],
      status: failed ? "error" : "ok",
      errorKind: failed ? "script_exception" : undefined,
      spilled: !failed && chance(0.2),
      duration: [1100, 6400],
    };
  },
  opencodePatch(): Draft {
    return {
      wire: "patch",
      route: "client_roundtrip",
      wireArgs: JSON.stringify({ patchText: V4A_PATCH }),
      blocks: [textBlock("Patch applied: 3 files changed")],
      repaired: chance(0.4),
      duration: [50, 300],
    };
  },
  ompEval(): Draft {
    const roll = rand();
    const py = chance(0.7);
    const failed = roll < 0.24;
    return {
      wire: "eval",
      route: "client_roundtrip",
      orchestrator: true,
      language: py ? "py" : "js",
      wireArgs: JSON.stringify({ language: py ? "py" : "js", code: py ? pick(PY_SCRIPTS) : pick(JS_SCRIPTS) }),
      blocks: failed
        ? [
            textBlock(
              roll < 0.12
                ? 'Traceback (most recent call last):\n  File "<eval>", line 5, in <module>\nKeyError: \'stdout\''
                : "Tool 'edit' argument validation failed: missing required property 'old_text'",
            ),
          ]
        : [textBlock(fileBody(between(60, 520)))],
      status: failed ? "error" : "ok",
      errorKind: failed ? (roll < 0.12 ? "script_exception" : "tool_error") : undefined,
      spilled: !failed && chance(0.35),
      duration: [1400, 8800],
    };
  },
  ompRead(): Draft {
    return {
      wire: "read",
      route: "client_roundtrip",
      wireArgs: JSON.stringify({ path: pick(FILES), limit: 200 }),
      blocks: [textBlock(fileBody(between(60, 200), true))],
      duration: [6, 40],
    };
  },
} as const;

type DraftFn = () => Draft;

const MIX: Record<string, ReadonlyArray<readonly [DraftFn, number]>> = {
  codex: [
    [drafts.codexExec, 58],
    [drafts.codexShell, 12],
    [drafts.applyPatch, 9],
    [drafts.search, 6],
    [drafts.spillRead, 4],
    [drafts.hub, 5],
    [drafts.webFetch, 3],
    [drafts.preflightReject, 3],
  ],
  "claude-code": [
    [drafts.claudeBash, 40],
    [drafts.claudeRead, 24],
    [drafts.claudeEdit, 16],
    [drafts.claudeRepl, 6],
    [drafts.screenshot, 4],
    [drafts.dispatcher, 5],
    [drafts.webFetch, 5],
  ],
  opencode: [
    [drafts.opencodeExecute, 44],
    [drafts.opencodePatch, 12],
    [drafts.ompRead, 20],
    [drafts.search, 8],
    [drafts.preflightReject, 8],
    [drafts.hub, 8],
  ],
  omp: [
    [drafts.ompEval, 56],
    [drafts.ompRead, 22],
    [drafts.spillRead, 10],
    [drafts.dispatcher, 12],
  ],
  generic: [
    [drafts.claudeBash, 60],
    [drafts.claudeRead, 40],
  ],
};

function drawDraft(harness: string): Draft {
  const mix = MIX[harness] ?? MIX.generic ?? [];
  const total = mix.reduce((sum, [, weight]) => sum + weight, 0);
  let roll = rand() * total;
  for (const [fn, weight] of mix) {
    roll -= weight;
    if (roll <= 0) return fn();
  }
  return drafts.claudeBash();
}

/* ---------- sessions ---------- */

interface DemoSession {
  id: string;
  harness: string;
  mode: string | null;
  model: string;
  passthrough: boolean;
  declared: number;
  eager: number;
  hubs: number;
  wire: number;
  turns: number;
  prompt: string;
}

const SESSIONS: DemoSession[] = [
  { id: uuid(), harness: "codex", mode: "codex_exec", model: "gemini-4-carbon-high", passthrough: false, declared: 128, eager: 14, hubs: 4, wire: 21_300, turns: 13, prompt: "给网关加上逐次工具调用的 I/O 记录，并在 Web 面板里可视化" },
  { id: uuid(), harness: "claude-code", mode: "claude_bash_first", model: "claude-opus-5", passthrough: false, declared: 46, eager: 31, hubs: 2, wire: 28_900, turns: 9, prompt: "排查会话池在 mcp-cont 续接后的缓存命中率抖动" },
  { id: uuid(), harness: "opencode", mode: "opencode_execute_full", model: "gemini-4-carbon-high", passthrough: false, declared: 38, eager: 12, hubs: 1, wire: 26_100, turns: 8, prompt: "把 OpenCode 的 execute 契约与 Codex 对齐，减少脚本运行时报错" },
  { id: uuid(), harness: "omp", mode: "omp_eval", model: "gemini-4-neon-high", passthrough: false, declared: 52, eager: 10, hubs: 0, wire: 32_400, turns: 9, prompt: "修复 omp eval 里 edit 工具的参数校验失败" },
  { id: uuid(), harness: "generic", mode: "direct", model: "gemini-4-hydrogen-low", passthrough: true, declared: 6, eager: 6, hubs: 0, wire: 4_100, turns: 3, prompt: "总结一下这个仓库的目录结构" },
];

/* ---------- world state ---------- */

const NOW = Date.now();
const details = new Map<string, ToolCallDetail>();
const callOrder: string[] = [];
const turnTask = new Map<string, string>();

function taskIdFor(session: DemoSession, turn: number): string {
  const key = `${session.id}:${turn}`;
  let id = turnTask.get(key);
  if (!id) {
    id = `${session.harness === "claude-code" ? "msg" : "resp"}_${hex(12)}`;
    turnTask.set(key, id);
  }
  return id;
}

function outputFields(draft: Draft): Pick<
  ToolCallDetail,
  "status" | "error_kind" | "output_bytes" | "output_tokens_est" | "output_preview" | "output_blocks"
> {
  const outputBytes = draft.blocks.reduce(
    (sum, b) => sum + (b.type === "image" ? b.base64_bytes : b.bytes),
    0,
  );
  const firstText = draft.blocks.find((b) => b.type === "text");
  const preview = firstText?.type === "text" ? firstText.text.slice(0, 240) : "";
  return {
    status: draft.status ?? "ok",
    error_kind: draft.errorKind ?? null,
    output_bytes: outputBytes,
    output_tokens_est: Math.round(outputBytes / 4),
    output_preview: preview.replace(/\s+/g, " "),
    output_blocks: draft.blocks,
  };
}

let nextSeq = 1;
let storeRevision = 0;

function materialize(
  session: DemoSession,
  turn: number,
  step: number,
  startedAt: number,
  draft: Draft,
  pending = false,
): ToolCallDetail {
  const inputBytes = bytes(draft.wireArgs);
  const output: ReturnType<typeof outputFields> = pending
    ? { status: "pending", error_kind: null, output_bytes: 0, output_tokens_est: 0, output_preview: "", output_blocks: [] }
    : outputFields(draft);
  const detail: ToolCallDetail = {
    call_id: `call_${hex(12)}`,
    task_id: taskIdFor(session, turn),
    session_id: session.id,
    turn_index: turn,
    step_index: step,
    harness_profile: session.harness,
    orchestrator_mode: session.mode,
    model_id: session.model,
    route: draft.route,
    wire_tool_name: draft.wire,
    client_tool_name: draft.client ?? null,
    is_orchestrator: draft.orchestrator ?? false,
    code_language: draft.language ?? null,
    v4a_auto_repaired: draft.repaired ?? false,
    output_sanitized: draft.sanitized ?? false,
    output_spilled: !pending && (draft.spilled ?? false),
    started_at_unix_ms: startedAt,
    duration_ms: pending ? null : between(draft.duration[0], draft.duration[1]),
    input_bytes: inputBytes,
    input_tokens_est: Math.round(inputBytes / 4),
    input_preview: (draft.clientArgs ?? draft.wireArgs).slice(0, 240).replace(/\s+/g, " "),
    wire_arguments: draft.wireArgs,
    client_arguments: draft.clientArgs ?? null,
    ...output,
    seq: nextSeq++,
    revision: ++storeRevision,
  };
  details.set(detail.call_id, detail);
  return detail;
}

// Lay each session's turns out over the last ~50 minutes.
for (const session of SESSIONS) {
  let cursor = NOW - between(38, 52) * 60_000;
  for (let turn = 1; turn <= session.turns; turn += 1) {
    const steps = session.harness === "codex" ? between(1, 4) : between(2, 8);
    for (let step = 0; step < steps; step += 1) {
      const detail = materialize(session, turn, step, cursor, drawDraft(session.harness));
      callOrder.push(detail.call_id);
      cursor += (detail.duration_ms ?? 0) + between(1500, 9000);
    }
    cursor += between(20_000, 110_000);
  }
}

const toRecord = (d: ToolCallDetail): ToolCallRecord => {
  const { wire_arguments: _w, client_arguments: _c, output_blocks: _o, input_truncated: _i, output_truncated: _t, ...record } = d;
  return record;
};

function allRecords(): ToolCallRecord[] {
  const out: ToolCallRecord[] = [];
  for (const id of callOrder) {
    const d = details.get(id);
    if (d) out.push(toRecord(d));
  }
  return out;
}

const toolSummary: ToolCallTelemetrySummary = {
  store_id: "demo",
  revision: 1,
  oldest_seq: 1,
  max_records: 2000,
  max_field_bytes: 128 * 1024,
  max_total_bytes: 64 * 1024 * 1024,
  stored_records: 0,
  stored_bytes: 0,
  total_recorded: 0,
  total_evicted: 0,
};

function refreshToolSummary(): void {
  let stored = 0;
  for (const id of callOrder) {
    const d = details.get(id);
    if (d) stored += d.input_bytes + d.output_bytes;
  }
  toolSummary.stored_records = callOrder.length;
  toolSummary.stored_bytes = stored;
  toolSummary.total_recorded = callOrder.length + toolSummary.total_evicted;
  toolSummary.revision = storeRevision;
  const oldest = callOrder[0] !== undefined ? details.get(callOrder[0]) : undefined;
  toolSummary.oldest_seq = oldest?.seq ?? nextSeq;
}
refreshToolSummary();

/* ---------- tasks ---------- */

const settledTasks = new Map<string, TaskSnapshot>();

function buildTasks(): TaskSnapshot[] {
  const tasks: TaskSnapshot[] = [];
  for (const session of SESSIONS) {
    for (let turn = 1; turn <= session.turns; turn += 1) {
      const taskId = taskIdFor(session, turn);
      const calls = [...details.values()].filter((d) => d.task_id === taskId);
      if (calls.length === 0) continue;
      const settled = settledTasks.get(taskId);
      if (settled && settled.client_tool_calls.length === calls.filter((c) => c.route === "client_roundtrip" || c.client_tool_name).length) {
        tasks.push(settled);
        continue;
      }
      const first = calls[0];
      const last = calls[calls.length - 1];
      if (!first || !last) continue;
      const elapsed = last.started_at_unix_ms + (last.duration_ms ?? 0) - first.started_at_unix_ms + 4000;
      const prompt = 26_000 + turn * 7_400 + between(0, 3000);
      // First turns are cold; a few warm turns miss upstream to feed the audit list.
      const miss = turn === 1 || (turn > 2 && chance(0.09));
      const cachedRead = miss ? (turn === 1 ? 0 : between(2000, 9000)) : Math.round(prompt * (0.9 + rand() * 0.08));
      const pending = calls.some((c) => c.status === "pending");
      const task: TaskSnapshot = {
        task_id: taskId,
        protocol: session.harness === "claude-code" ? "anthropic_messages" : "openai_responses",
        stream: true,
        requested_model: session.harness === "claude-code" ? "claude-opus-5" : session.model,
        resolved_model: session.model,
        session_id: session.id,
        session_reused: turn > 1,
        phase: pending ? "calling_client_tool" : "completed",
        started_at_unix_ms: first.started_at_unix_ms - 3000,
        finished_at_unix_ms: pending ? null : first.started_at_unix_ms + elapsed,
        elapsed_ms: elapsed,
        ttft_ms: between(900, 4200),
        thought_chars: between(200, 4200),
        output_chars: between(80, 1800),
        client_tool_calls: calls.filter((c) => c.route === "client_roundtrip" || c.client_tool_name).map((c) => c.client_tool_name ?? c.wire_tool_name),
        native_web_searches: 0,
        native_web_fetches: calls.filter((c) => c.route === "gateway_managed").length,
        vision_offloaded_images: 0,
        zero_rtt_mcp_ops: calls.filter((c) => c.route.startsWith("zero_rtt")).length,
        spilled_tool_results: calls.filter((c) => c.output_spilled).length,
        prompt_preview: turn === 1 ? session.prompt : `[tool_result:${last.call_id}] ${last.output_preview.slice(0, 160)}`,
        output_preview: "已完成这一步，接下来继续校验编译并补充测试。",
        stop_reason: pending ? null : "end_turn",
        error_message: null,
        usage: pending
          ? null
          : {
              inputTokens: prompt - cachedRead,
              outputTokens: between(120, 2400),
              cachedReadTokens: cachedRead,
              cachedWriteTokens: 0,
              thoughtTokens: between(0, 1800),
              totalTokens: prompt + 1200,
            },
        cache_hit_rate_bps: pending ? null : Math.round((cachedRead * 10000) / prompt),
      };
      if (!pending) settledTasks.set(taskId, task);
      tasks.push(task);
    }
  }
  return tasks.sort((a, b) => b.started_at_unix_ms - a.started_at_unix_ms);
}

/* ---------- catalogs ---------- */

const MCP_GROUPS: Record<string, readonly string[]> = {
  github: ["create_pull_request", "get_pull_request", "list_pull_requests", "merge_pull_request", "create_issue", "get_issue", "list_issues", "add_issue_comment", "search_code", "get_file_contents", "create_branch", "list_commits", "get_commit", "create_review", "list_workflow_runs"],
  linear: ["get_issue", "list_issues", "create_issue", "update_issue", "list_projects", "create_comment", "list_teams", "search_issues"],
  playwright: ["browser_navigate", "browser_click", "browser_type", "browser_take_screenshot", "browser_snapshot", "browser_evaluate", "browser_wait_for", "browser_press_key", "browser_tabs", "browser_close"],
  sentry: ["list_issues", "get_issue", "list_events", "get_event", "resolve_issue", "list_projects"],
};

const CORE_TOOLS: Record<string, readonly string[]> = {
  codex: ["exec", "exec_command", "write_stdin", "apply_patch", "update_plan", "view_image", "web_search"],
  "claude-code": ["Bash", "Read", "Edit", "Write", "Grep", "Glob", "REPL", "WebFetch", "TodoWrite", "Task", "NotebookEdit"],
  opencode: ["execute", "bash", "read", "edit", "patch", "grep", "glob", "list", "webfetch", "todowrite"],
  omp: ["eval", "bash", "read", "edit", "write", "grep", "find", "ls"],
  generic: ["Bash", "Read", "Write", "Edit", "Grep", "Glob"],
};

const ORCHESTRATORS = new Set(["exec", "REPL", "execute", "eval"]);

function schemaFor(name: string, props: number): string {
  const properties: Record<string, unknown> = {};
  for (let i = 0; i < props; i += 1) {
    properties[pick(["path", "query", "owner", "repo", "title", "body", "limit", "offset", "command", "timeout", "id", "state", "labels", "ref"]) + (i > 5 ? `_${i}` : "")] = {
      type: pick(["string", "string", "integer", "boolean"]),
      description: `Parameter ${i + 1} of ${name}. Provide the exact value; relative values are resolved against the workspace root.`,
    };
  }
  return JSON.stringify({ type: "object", properties, required: Object.keys(properties).slice(0, 2), additionalProperties: false });
}

function describe(name: string, verbosity: number): string {
  const base = `${name}: performs the operation against the user's workspace and returns a structured result.`;
  const extra = [
    "Use this tool whenever the task requires it; prefer batching related calls.",
    "IMPORTANT: always pass absolute paths. Never guess identifiers; look them up first.",
    "Returns an error object with `code` and `message` when the operation fails, and the caller must surface it.",
    "Usage notes:\n- Results above the inline limit are spilled and can be paged.\n- Parallel calls are allowed when the operations are independent.\n- Do not call this tool to re-read content you already have in context.",
  ];
  return [base, ...extra.slice(0, verbosity)].join("\n\n");
}

const catalogs = new Map<string, SessionToolCatalog>();

function buildCatalog(session: DemoSession): SessionToolCatalog {
  const cached = catalogs.get(session.id);
  if (cached) return cached;

  const core = CORE_TOOLS[session.harness] ?? [];
  const verbosity = session.harness === "codex" ? 1 : session.harness === "omp" ? 4 : 3;
  const tools: CatalogToolEntry[] = [];
  const wire: CatalogWireTool[] = [];
  const orchestrator = core.find((n) => ORCHESTRATORS.has(n)) ?? null;
  const foldCore = session.harness === "omp" || session.harness === "codex";

  for (const name of core) {
    const description = describe(name, verbosity);
    const schema = schemaFor(name, name === orchestrator ? 1 : between(2, 6));
    const isOrch = name === orchestrator;
    const tier: CatalogToolTier = isOrch
      ? "orchestrator"
      : foldCore && orchestrator !== null && !session.passthrough && chance(0.55)
        ? "folded_into_orchestrator"
        : "direct";
    const wireDescription = isOrch
      ? `${description}\n\n<tool_catalog>\n${core.filter((n) => n !== name).map((n) => `declare function ${n}(args: object): Promise<unknown>;`).join("\n")}\n</tool_catalog>`
      : description.split("\n\n").slice(0, session.harness === "codex" ? 1 : 2).join("\n\n");
    const mounted = tier !== "folded_into_orchestrator";
    const wireBytes = mounted ? bytes(wireDescription) + bytes(schema) : 0;
    tools.push({
      client_name: name,
      tier,
      wire_name: mounted ? name : orchestrator,
      client_kind: name === "apply_patch" ? "custom" : "function",
      client_description: description,
      client_schema: name === "apply_patch" ? 'start: begin_patch hunk+ end_patch\nbegin_patch: "*** Begin Patch" LF\nend_patch: "*** End Patch" LF?' : schema,
      client_bytes: bytes(description) + bytes(schema),
      wire_description: mounted ? wireDescription : null,
      wire_schema: mounted ? schema : null,
      wire_bytes: wireBytes,
      ts_signature: session.harness === "codex" ? `declare const tools: { ${name}(args: { [key: string]: unknown }): Promise<{ output: string }> };` : null,
    });
    if (mounted) {
      wire.push({
        wire_name: name,
        role: isOrch ? "orchestrator" : "direct",
        description: wireDescription,
        input_schema: schema,
        bytes: wireBytes,
        folded_actions: isOrch ? tools.filter((t) => t.tier === "folded_into_orchestrator").map((t) => t.client_name) : [],
      });
    }
  }

  const groups = Object.entries(MCP_GROUPS).slice(0, Math.max(session.hubs, session.passthrough ? 0 : 1));
  let remaining = session.declared - core.length;
  groups.forEach(([server, actions], groupIdx) => {
    const hubName = `mcp_lg_ns__${server}`;
    const hubbed = groupIdx < session.hubs;
    const folded: string[] = [];
    let hubBytes = 0;
    for (const action of actions) {
      if (remaining <= 0) break;
      remaining -= 1;
      const name = `mcp__${server}__${action}`;
      const description = describe(name, verbosity);
      const schema = schemaFor(name, between(3, 9));
      const backfilled = hubbed && chance(0.12);
      const tier: CatalogToolTier = backfilled ? "slack_backfill" : hubbed ? "namespace_hub_action" : "search_index_only";
      const wireBytes = backfilled ? bytes(description) + bytes(schema) : hubbed ? 64 : 0;
      if (hubbed && !backfilled) {
        folded.push(name);
        hubBytes += wireBytes;
      }
      tools.push({
        client_name: name,
        tier,
        wire_name: backfilled ? name : hubbed ? hubName : null,
        client_kind: "function",
        client_description: description,
        client_schema: schema,
        client_bytes: bytes(description) + bytes(schema),
        wire_description: backfilled ? description : null,
        wire_schema: backfilled ? schema : null,
        wire_bytes: wireBytes,
        ts_signature: `declare function ${action}(args: object): Promise<unknown>; // ${server}`,
      });
      if (backfilled) {
        wire.push({ wire_name: name, role: "slack_backfill", description, input_schema: schema, bytes: wireBytes, folded_actions: [] });
      }
    }
    if (hubbed && folded.length > 0) {
      const description = `Namespace hub for the ${server} MCP server. Pick one action:\n${folded.map((n) => `- ${n.split("__")[2] ?? n}`).join("\n")}`;
      const schema = JSON.stringify({ type: "object", properties: { action: { type: "string", enum: folded.map((n) => n.split("__")[2]) }, arguments: { type: "object" } }, required: ["action", "arguments"] });
      wire.push({ wire_name: hubName, role: "namespace_hub", description, input_schema: schema, bytes: bytes(description) + bytes(schema) + hubBytes, folded_actions: folded });
    }
  });

  if (!session.passthrough) {
    for (const [name, description] of [
      ["mcp_lg_search_client_tools", "Search the full client tool catalog by keyword. Answers locally (0-RTT)."],
      ["mcp_lg_call_client_tool", "Call any client tool by its exact name with a JSON arguments object."],
      ["mcp_lg_read_context_spill", "Page through a tool output that was spilled because it exceeded the inline limit."],
      ["mcp_lg_gateway_web_fetch", "Fetch a public URL through the gateway's SSRF-safe fetcher."],
    ] as const) {
      const schema = JSON.stringify({ type: "object", properties: { query: { type: "string" } }, required: ["query"] });
      wire.push({ wire_name: name, role: "gateway", description, input_schema: schema, bytes: bytes(description) + bytes(schema), folded_actions: [] });
    }
  }

  const wireBytes = wire.reduce((sum, t) => sum + t.bytes, 0);
  const scale = session.turns;
  const seg = (key: ContextSegmentKey, b: number): ContextSegment => ({ key, bytes: Math.round(b), tokens_est: Math.round(b / 4) });
  const bridgeRules = session.passthrough
    ? null
    : `<api_gateway_bridge_rules>\n  <rule>Client tools are mounted on the \`lg\` MCP server. Call them natively; never emit XML tool calls.</rule>\n  <rule>Tools not listed are still reachable: search with mcp_lg_search_client_tools, then call through mcp_lg_call_client_tool.</rule>\n  <rule>Outputs above 9.6 KB are spilled. Page them with mcp_lg_read_context_spill instead of re-running the tool.</rule>\n  <rule>Prefer one orchestrated script over several sequential calls: every round-trip costs about 1.3 s.</rule>\n</api_gateway_bridge_rules>`;
  const contract =
    orchestrator === null || session.passthrough
      ? null
      : `<mcp_orchestrator_contract tool="${orchestrator}">\n  Write ${session.harness === "omp" ? "Python or JavaScript" : "JavaScript"} that calls the bindings below and prints only what you need.\n  <eval_session_tools>\n${tools.filter((t) => t.tier === "folded_into_orchestrator" || t.tier === "direct").map((t) => `    tools.${t.client_name}(args)  // ${t.client_description?.split("\n")[0]?.slice(0, 90) ?? ""}`).join("\n")}\n  </eval_session_tools>\n  ${session.harness === "codex" ? "Keep scripts short; return text() once." : "IMPORTANT: every binding returns a dict; check for an `error` key before reading fields.\n  IMPORTANT: do not import os/subprocess; use tools.bash.\n  NOTE: string arguments must be valid UTF-8 and paths must be absolute."}\n</mcp_orchestrator_contract>`;

  const catalog: SessionToolCatalog = {
    session_id: session.id,
    harness_profile: session.harness,
    orchestrator_mode: session.mode,
    is_passthrough: session.passthrough,
    model_id: session.model,
    captured_at_unix_ms: NOW - 60_000,
    wire_budget_bytes: 34 * 1024,
    eager_tool_budget: 40,
    client_tools_bytes: tools.reduce((sum, t) => sum + t.client_bytes, 0),
    segments: [
      seg("wire_tools", wireBytes),
      seg("bridge_rules", bridgeRules ? bytes(bridgeRules) : 0),
      seg("orchestrator_contract", contract ? bytes(contract) : 0),
      seg("client_tools_update", chance(0.4) ? 1800 : 0),
      seg("developer_instructions", 18_000 + rand() * 9000),
      seg("user_text", 2400 * scale * 0.4),
      seg("assistant_text", 3100 * scale * 0.5),
      seg("reasoning", 2200 * scale),
      seg("tool_calls", (session.harness === "codex" ? 900 : 1700) * scale * 3),
      seg("tool_results", (session.harness === "omp" ? 9800 : session.harness === "codex" ? 3600 : 6400) * scale * 3),
      seg("images", session.harness === "claude-code" ? 184_320 : 0),
    ],
    wire_tools: wire,
    tools,
    bridge_rules_prompt: bridgeRules,
    orchestrator_contract_prompt: contract,
    search_index_entries: session.passthrough ? 0 : tools.length,
  };
  catalogs.set(session.id, catalog);
  return catalog;
}

/* ---------- full-context payloads ---------- */

const DEV_INSTRUCTIONS = `<developer_instructions>
You are a coding agent working in the user's repository. Follow AGENTS.md.
- Never introduce unwrap()/expect() on fallible paths.
- Keep commits short and imperative.
- Run cargo fmt, clippy and the test suite before declaring the work done.
</developer_instructions>`;

function buildPayload(session: DemoSession, turn: number, taskId: string): FullContextPayload {
  const messages: CapturedFullMessage[] = [];
  const push = (role: string, inDelta: boolean, parts: CapturedFullPart[]): void => {
    const total = parts.reduce((sum, p) => sum + (p.kind === "image" ? p.base64_bytes : p.bytes), 0);
    messages.push({ index: messages.length, role, in_delta: inDelta, total_bytes: total, parts });
  };
  push("user", turn === 1, [
    { kind: "text", is_developer_instructions: true, bytes: bytes(DEV_INSTRUCTIONS), text: DEV_INSTRUCTIONS },
    { kind: "text", is_developer_instructions: false, bytes: bytes(session.prompt), text: session.prompt },
  ]);
  for (let t = 1; t <= turn; t += 1) {
    const calls = [...details.values()].filter((d) => d.session_id === session.id && d.turn_index === t && d.status !== "pending");
    const inDelta = t === turn;
    for (const call of calls) {
      const reasoning = `先确认 ${call.wire_tool_name} 的输入是否完整，再根据返回结果决定下一步。`;
      const args = call.client_arguments ?? call.wire_arguments;
      push("assistant", inDelta, [
        { kind: "reasoning", bytes: bytes(reasoning), text: reasoning },
        { kind: "tool_call", call_id: call.call_id, name: call.client_tool_name ?? call.wire_tool_name, is_custom: call.wire_tool_name === "apply_patch", bytes: bytes(args), arguments: args },
      ]);
      const parts: CapturedFullPart[] = call.output_blocks.map((block) =>
        block.type === "image"
          ? { kind: "image", media_type: block.mime_type, base64_bytes: block.base64_bytes }
          : {
              kind: "tool_result",
              call_id: call.call_id,
              is_error: call.status !== "ok",
              bytes: block.bytes,
              output: block.text,
            },
      );
      push("user", inDelta, parts);
    }
    if (!inDelta) {
      const reply = "这一步已经完成，我继续处理下一项。";
      push("assistant", false, [{ kind: "text", is_developer_instructions: false, bytes: bytes(reply), text: reply }]);
    }
  }
  const catalog = buildCatalog(session);
  const delta = messages.filter((m) => m.in_delta);
  const promptText = delta
    .flatMap((m) => m.parts)
    .map((p) => (p.kind === "tool_result" ? `<tool_result call_id="${p.call_id}">\n${p.output.slice(0, 1200)}\n</tool_result>` : p.kind === "text" ? p.text : ""))
    .filter((s) => s.length > 0)
    .join("\n\n");
  return {
    task_id: taskId,
    recorded_at_unix_ms: NOW,
    acp_system_append: turn === 1 ? [catalog.bridge_rules_prompt, catalog.orchestrator_contract_prompt].filter(Boolean).join("\n\n") : null,
    tools_delta_xml: null,
    outbound_acp_prompt_blocks: [{ type: "text", bytes: bytes(promptText), text: promptText || session.prompt }],
    messages,
    client_tools: catalog.tools.map((t) => ({
      name: t.client_name,
      kind: t.client_kind,
      schema_bytes: t.client_bytes,
      description: t.client_description,
      schema_or_format: t.client_schema,
    })),
    assistant_output_preview: "已完成这一步，接下来继续校验编译并补充测试。",
    payload_truncated: false,
    assistant_reasoning: "需要先看遥测存储的结构，再决定工具调用记录放在哪一层。环形缓冲要同时限制条数和总字节数。",
    assistant_output: "已完成这一步，接下来继续校验编译并补充测试。\n\n- 新增 `tool_calls.rs`，实现有界环形缓冲\n- `telemetry.rs` 接入淘汰逻辑\n- 下一步：补上 `/api/telemetry/tool-calls` 路由",
  };
}

/* ---------- client ---------- */

const MODELS: AvailableModel[] = [
  ["gemini-4-carbon-high", "Gemini 4 Carbon (High)"],
  ["gemini-4-carbon-medium", "Gemini 4 Carbon (Medium)"],
  ["gemini-4-neon-xhigh", "Gemini 4 Neon (XHigh)"],
  ["gemini-4-neon-high", "Gemini 4 Neon (High)"],
  ["gemini-4-hydrogen-low", "Gemini 4 Hydrogen (Low)"],
  ["gemini-3.8-flash", "Gemini 3.8 Flash"],
  ["claude-opus-5", "Claude Opus 5"],
  ["claude-sonnet-5", "Claude Sonnet 5"],
  ["gpt-oss-140b", "GPT-OSS 140B"],
].map(([modelId, name]) => ({ modelId: modelId ?? "", name: name ?? "" }));

export function createDemoClient(): GatewayClient {
  let tasks = buildTasks();
  let visionOffload = false;
  let mcpBridge = true;
  let closed = new Set<string>();
  let pendingId: string | null = null;
  let pendingDraft: Draft | null = null;

  const cacheSummary: CacheTelemetrySummary = {
    revision: 1,
    max_low_cache_records: 1000,
    low_cache_threshold_bps: 5000,
    capture_request_details: true,
    persisted_path: "~/.liftgate/cache-telemetry.json",
    payload_dir: "~/.liftgate/low-cache-payloads",
    total_prompt_tokens: 0,
    total_uncached_input_tokens: 0,
    total_cached_read_tokens: 0,
    total_cached_write_tokens: 0,
    total_cache_hit_rate_bps: null,
    warm_prompt_tokens: 0,
    warm_cached_read_tokens: 0,
    warm_cache_hit_rate_bps: null,
    evaluated_turns: 0,
    warm_evaluated_turns: 0,
    total_low_cache_turns: 0,
    stored_low_cache_count: 0,
    auxiliary_turns: 3,
    auxiliary_prompt_tokens: 41_200,
  };

  let lowRecords: LowCacheHitRecord[] = [];
  const sessionOf = (id: string | null): DemoSession | undefined => SESSIONS.find((s) => s.id === id);

  const recomputeCache = (): void => {
    let prompt = 0;
    let read = 0;
    let warmPrompt = 0;
    let warmRead = 0;
    let turns = 0;
    let warmTurns = 0;
    const low: LowCacheHitRecord[] = [];
    for (const task of tasks) {
      const u = task.usage;
      if (!u) continue;
      const total = u.inputTokens + u.cachedReadTokens + u.cachedWriteTokens;
      prompt += total;
      read += u.cachedReadTokens;
      turns += 1;
      if (task.session_reused) {
        warmPrompt += total;
        warmRead += u.cachedReadTokens;
        warmTurns += 1;
      }
      const bps = task.cache_hit_rate_bps ?? 0;
      if (bps >= cacheSummary.low_cache_threshold_bps) continue;
      const session = sessionOf(task.session_id);
      const turn = [...turnTask.entries()].find(([, id]) => id === task.task_id)?.[0].split(":")[1];
      const turnIndex = Number.parseInt(turn ?? "1", 10);
      const cold = !task.session_reused;
      low.push({
        task_id: task.task_id,
        recorded_at_unix_ms: task.finished_at_unix_ms ?? task.started_at_unix_ms,
        protocol: task.protocol,
        requested_model: task.requested_model,
        resolved_model: task.resolved_model,
        session_id: task.session_id,
        session_reused: task.session_reused,
        reason: cold ? "cold_session_start" : u.cachedReadTokens === 0 ? "warm_cache_miss" : "warm_partial_hit",
        diagnostic_cause: cold ? "first_turn_cold_start" : "warm_upstream_implicit_miss",
        cache_hit_rate_bps: bps,
        input_uncached_tokens: u.inputTokens,
        cached_read_tokens: u.cachedReadTokens,
        cached_write_tokens: 0,
        total_prompt_tokens: total,
        output_tokens: u.outputTokens,
        elapsed_ms: task.elapsed_ms,
        ttft_ms: task.ttft_ms,
        prompt_preview: task.prompt_preview,
        request_metrics: {
          routing_strategy: cold ? "cold_new_session" : "mcp_suspended_continuation",
          session_turn_index: turnIndex,
          session_idle_ms: cold ? null : 42_000,
          previous_session_context_tokens: cold ? 0 : total - 4200,
          model_switched: false,
          base_state_hash_hex: hex(64),
          prefix_hash_hex: cold ? null : hex(64),
          tools_updated_on_warm_turn: false,
          stream: true,
          reasoning_effort: "high",
          total_message_count: turnIndex * 6 + 1,
          conversational_message_count: turnIndex * 6,
          user_message_count: turnIndex * 3 + 1,
          assistant_message_count: turnIndex * 3,
          delta_message_count: cold ? 1 : 4,
          total_conversation_bytes: total * 4,
          delta_messages_bytes: cold ? total * 4 : 18_400,
          developer_instructions_blocks: 2,
          developer_instructions_bytes: 20_800,
          user_text_bytes: 6_700,
          assistant_text_bytes: turnIndex * 1400,
          reasoning_part_count: turnIndex * 2,
          reasoning_bytes: turnIndex * 2100,
          tool_call_part_count: turnIndex * 3,
          tool_call_bytes: turnIndex * 3400,
          tool_result_part_count: turnIndex * 3,
          tool_result_bytes: turnIndex * 16_200,
          image_part_count: 0,
          image_bytes: 0,
          client_tool_count: session?.declared ?? 0,
          client_tools_schema_bytes: (session?.declared ?? 0) * 1900,
          acp_system_append_bytes: cold ? 5200 : 0,
          tools_delta_xml_bytes: 0,
          outbound_acp_prompt_bytes: cold ? total * 4 : 18_400,
          outbound_acp_text_blocks: 1,
          outbound_acp_image_blocks: 0,
          thought_tokens: u.thoughtTokens,
          thought_chars: task.thought_chars,
          output_chars: task.output_chars,
          native_web_searches: 0,
          native_web_fetches: 0,
          vision_offloaded_images: 0,
          emitted_client_tools: task.client_tool_calls,
          stop_reason: task.stop_reason,
        },
        request_capture: {
          client_tool_names: [],
          enable_native_web_search: true,
          force_native_web_search: false,
          enable_native_web_fetch: true,
          force_native_web_fetch: false,
          outbound_acp_prompt_excerpt: task.prompt_preview,
          output_preview: task.output_preview,
          message_digests: [],
          full_payload_saved: true,
          payload_file_path: `~/.liftgate/low-cache-payloads/${task.task_id}.json`,
        },
      });
    }
    lowRecords = low;
    Object.assign(cacheSummary, {
      total_prompt_tokens: prompt,
      total_cached_read_tokens: read,
      total_uncached_input_tokens: prompt - read,
      total_cache_hit_rate_bps: prompt > 0 ? Math.round((read * 10000) / prompt) : null,
      warm_prompt_tokens: warmPrompt,
      warm_cached_read_tokens: warmRead,
      warm_cache_hit_rate_bps: warmPrompt > 0 ? Math.round((warmRead * 10000) / warmPrompt) : null,
      evaluated_turns: turns,
      warm_evaluated_turns: warmTurns,
      total_low_cache_turns: low.length,
      stored_low_cache_count: low.length,
    } satisfies Partial<CacheTelemetrySummary>);
  };
  recomputeCache();

  const contextSummary: ContextCaptureSummary = {
    revision: 1,
    enabled: true,
    max_records: 200,
    max_total_bytes: 512 * 1024 * 1024,
    stored_records: 0,
    stored_bytes: 0,
    skipped_records: 0,
    capture_dir: "~/.liftgate/context-captures",
  };
  let contextRecords: ContextCaptureRecord[] = tasks
    .filter((t) => t.usage !== null)
    .slice(0, 14)
    .map((task) => {
      const turn = Number.parseInt([...turnTask.entries()].find(([, id]) => id === task.task_id)?.[0].split(":")[1] ?? "1", 10);
      const prompt = (task.usage?.inputTokens ?? 0) + (task.usage?.cachedReadTokens ?? 0);
      return {
        task_id: task.task_id,
        recorded_at_unix_ms: task.finished_at_unix_ms ?? task.started_at_unix_ms,
        session_id: task.session_id,
        harness_profile: sessionOf(task.session_id)?.harness ?? null,
        resolved_model: task.resolved_model,
        session_turn_index: turn,
        cache_hit_rate_bps: task.cache_hit_rate_bps,
        total_prompt_tokens: prompt,
        output_tokens: task.usage?.outputTokens ?? 0,
        message_count: turn * 6 + 1,
        delta_message_count: 4,
        payload_bytes: prompt * 4 + 20_000,
        prompt_preview: task.prompt_preview,
      };
    });
  const refreshContextSummary = (): void => {
    contextSummary.stored_records = contextRecords.length;
    contextSummary.stored_bytes = contextRecords.reduce((sum, r) => sum + r.payload_bytes, 0);
    contextSummary.revision += 1;
  };
  refreshContextSummary();

  const sessionSummaries = (): SessionSummary[] =>
    SESSIONS.filter((s) => !closed.has(s.id)).map((s, idx) => {
      const mine = tasks.filter((t) => t.session_id === s.id && t.usage);
      const prompt = mine.reduce((sum, t) => sum + (t.usage?.inputTokens ?? 0) + (t.usage?.cachedReadTokens ?? 0), 0);
      const read = mine.reduce((sum, t) => sum + (t.usage?.cachedReadTokens ?? 0), 0);
      const hasPending = pendingId !== null && details.get(pendingId)?.session_id === s.id;
      return {
        session_id: s.id,
        model_id: s.model,
        state: hasPending ? "suspended" : "idle",
        cache_warmth: (["hot", "warm", "hot", "cold", "warm"] as const)[idx] ?? "unknown",
        turn_count: s.turns,
        message_count: s.turns * 6 + 1,
        last_context_tokens: 26_000 + s.turns * 7400,
        total_prompt_tokens: prompt,
        total_cached_read_tokens: read,
        cache_hit_rate_bps: prompt > 0 ? Math.round((read * 10000) / prompt) : null,
        created_at_unix_ms: NOW - 50 * 60_000,
        last_active_unix_ms: mine[0]?.started_at_unix_ms ?? NOW,
        last_response_id: mine[0]?.task_id ?? null,
        mcp: {
          harness_profile: s.harness,
          orchestrator_mode: s.mode,
          is_passthrough: s.passthrough,
          declared_tools: s.declared,
          eager_tools: s.eager,
          virtualized_hubs: s.hubs,
          wire_bytes: s.wire,
        },
      };
    });

  const snapshot = (): GatewayTelemetrySnapshot => {
    const all = [...details.values()];
    const active = tasks.filter((t) => t.phase !== "completed");
    const done = tasks.filter((t) => t.phase === "completed");
    return {
      generated_at_unix_ms: Date.now(),
      gateway_version: "0.3.0-demo",
      vision_offload_enabled: visionOffload,
      mcp_tool_bridge_enabled: mcpBridge,
      acp_process: {
        connected: true,
        pid: 57569,
        binary_path: "~/.local/bin/agy",
        session_cwd: "/tmp/liftgate-cwd-501",
        os: "macos",
        arch: "aarch64",
        supervision_strategy: "POSIX Process Group (killpg + SIGTERM)",
        protocol_version: 1,
        agent_name: "Antigravity ACP Server",
        agent_version: "1.5.18",
        started_at_unix_ms: NOW - 6 * 3600_000,
        uptime_secs: 6 * 3600 + Math.round((Date.now() - NOW) / 1000),
        default_model: "gemini-4-carbon-high",
        acp_mode: "auto",
        rpc_requests_sent: 412,
        rpc_responses_received: 412,
        rpc_notifications_received: 18_240,
        rpc_pending_in_flight: active.length,
        active_prompt_streams: active.length,
        agy_proxy: null,
        auto_update_enabled: true,
        latest_known_version: "1.5.18",
        auto_update_note: "up to date",
        recent_stderr: [],
      },
      session_pool: {
        active_leased_sessions: 0,
        suspended_mcp_sessions: pendingId !== null ? 1 : 0,
        idle_cached_sessions: SESSIONS.length - closed.size - (pendingId !== null ? 1 : 0),
        max_idle_sessions: 64,
        stored_responses: done.length,
        max_stored_responses: 256,
        total_sessions_created: SESSIONS.length,
        total_sessions_reused: done.filter((t) => t.session_reused).length,
        mcp_continuations: all.filter((d) => d.route === "client_roundtrip").length,
        total_sessions_closed_or_evicted: closed.size,
        sessions: sessionSummaries(),
      },
      metrics: {
        total_requests: tasks.length,
        active_requests: active.length,
        completed_requests: done.length,
        failed_requests: 0,
        cancelled_requests: 0,
        openai_requests: tasks.filter((t) => t.protocol === "openai_responses").length,
        anthropic_requests: tasks.filter((t) => t.protocol === "anthropic_messages").length,
        total_input_tokens: cacheSummary.total_uncached_input_tokens,
        total_output_tokens: done.reduce((sum, t) => sum + (t.usage?.outputTokens ?? 0), 0),
        total_thought_tokens: done.reduce((sum, t) => sum + (t.usage?.thoughtTokens ?? 0), 0),
        total_cached_tokens: cacheSummary.total_cached_read_tokens,
        total_cached_write_tokens: 0,
        cache_hit_rate_bps: cacheSummary.total_cache_hit_rate_bps,
      },
      cache_telemetry: { ...cacheSummary },
      elastic_io: {
        passthrough_sessions: SESSIONS.filter((s) => s.passthrough).length,
        virtualized_sessions: SESSIONS.filter((s) => !s.passthrough).length,
        mcp_forwarded_tool_calls: all.filter((d) => !d.route.startsWith("zero_rtt")).length,
        mcp_orchestrator_calls: all.filter((d) => d.is_orchestrator).length,
        mcp_tool_using_turns: tasks.length,
        mcp_patch_calls: all.filter((d) => d.wire_tool_name.includes("patch")).length,
        mcp_multi_file_patch_calls: all.filter((d) => d.wire_tool_name.includes("patch")).length,
        mcp_patch_files_total: all.filter((d) => d.wire_tool_name.includes("patch")).length * 3,
        mcp_zero_rtt_searches: all.filter((d) => d.route === "zero_rtt_search").length,
        mcp_zero_rtt_spill_reads: all.filter((d) => d.route === "zero_rtt_spill_read").length,
        mcp_zero_rtt_preflight_heals: all.filter((d) => d.route === "zero_rtt_preflight").length,
        mcp_tool_results_total: all.length,
        mcp_tool_results_spilled: all.filter((d) => d.output_spilled).length,
        context_spills_created: all.filter((d) => d.output_spilled).length,
        context_spills_bytes: all.filter((d) => d.output_spilled).reduce((sum, d) => sum + d.output_bytes, 0),
      },
      tool_call_telemetry: { ...toolSummary },
      context_capture: { ...contextSummary },
      active_tasks: active,
      recent_tasks: done.slice(0, 100),
      available_models: MODELS,
    };
  };

  /** Every tick either lands the in-flight call or starts a new one, so the stream stays alive. */
  const tick = (): void => {
    const live = SESSIONS.filter((s) => !closed.has(s.id) && s.harness !== "generic");
    if (pendingId !== null) {
      const pending = details.get(pendingId);
      if (pending && pendingDraft) {
        details.set(pending.call_id, {
          ...pending,
          ...outputFields(pendingDraft),
          output_spilled: pendingDraft.spilled ?? false,
          duration_ms: Date.now() - pending.started_at_unix_ms,
          revision: ++storeRevision,
        });
      }
      pendingId = null;
      pendingDraft = null;
    } else if (live.length > 0) {
      const session = pick(live);
      const last = [...details.values()].filter((d) => d.session_id === session.id).pop();
      const continueTurn = last !== undefined && last.step_index < 5 && chance(0.7);
      const turn = continueTurn && last ? last.turn_index : (last?.turn_index ?? 0) + 1;
      const step = continueTurn && last ? last.step_index + 1 : 0;
      session.turns = Math.max(session.turns, turn);
      const startPending = chance(0.6);
      const draft = drawDraft(session.harness);
      const detail = materialize(session, turn, step, Date.now(), draft, startPending);
      callOrder.push(detail.call_id);
      if (startPending) {
        pendingId = detail.call_id;
        pendingDraft = draft;
      }
    }
    if (callOrder.length > toolSummary.max_records) {
      const evicted = callOrder.splice(0, callOrder.length - toolSummary.max_records);
      for (const id of evicted) details.delete(id);
      toolSummary.total_evicted += evicted.length;
      storeRevision += 1;
    }
    refreshToolSummary();
    tasks = buildTasks();
  };

  const wait = <T,>(value: T, ms = 120): Promise<T> =>
    new Promise((resolve) => window.setTimeout(() => resolve(value), ms));

  return {
    demo: true,
    origin: "http://127.0.0.1:8787",

    subscribe(handlers: SnapshotHandlers): () => void {
      handlers.onState("live");
      handlers.onError(null);
      handlers.onSnapshot(snapshot());
      const timer = window.setInterval(() => {
        if (document.visibilityState === "hidden") return;
        tick();
        handlers.onSnapshot(snapshot());
      }, 2600);
      return () => window.clearInterval(timer);
    },
    snapshot: () => wait(snapshot()),

    lowCacheHits: (): Promise<LowCacheHitsResponse> => wait({ summary: { ...cacheSummary }, records: lowRecords }),
    lowCacheHitDetail(taskId: string): Promise<LowCachePayloadDetailResponse> {
      const record = lowRecords.find((r) => r.task_id === taskId);
      if (!record) return Promise.reject(new ApiError(404, "record not found"));
      const session = sessionOf(record.session_id);
      return wait({
        record,
        full_payload: session ? buildPayload(session, record.request_metrics?.session_turn_index ?? 1, taskId) : null,
      });
    },
    clearLowCacheHits(resetTotals: boolean): Promise<CacheTelemetrySummary> {
      lowRecords = [];
      cacheSummary.stored_low_cache_count = 0;
      if (resetTotals) {
        Object.assign(cacheSummary, { total_prompt_tokens: 0, total_cached_read_tokens: 0, total_uncached_input_tokens: 0, total_cache_hit_rate_bps: null, warm_cache_hit_rate_bps: null, evaluated_turns: 0, warm_evaluated_turns: 0, total_low_cache_turns: 0 });
      }
      cacheSummary.revision += 1;
      return wait({ ...cacheSummary });
    },
    updateCacheConfig(update: CacheConfigUpdate): Promise<CacheTelemetrySummary> {
      if (update.max_low_cache_records !== undefined) cacheSummary.max_low_cache_records = update.max_low_cache_records;
      if (update.low_cache_threshold_pct !== undefined) {
        cacheSummary.low_cache_threshold_bps = update.low_cache_threshold_pct * 100;
        recomputeCache();
      }
      if (update.capture_request_details !== undefined) cacheSummary.capture_request_details = update.capture_request_details;
      cacheSummary.revision += 1;
      return wait({ ...cacheSummary });
    },

    closeSession(sessionId: string): Promise<boolean> {
      closed = new Set([...closed, sessionId]);
      return wait(true);
    },
    setVisionOffload(enabled: boolean): Promise<void> {
      visionOffload = enabled;
      return wait(undefined);
    },
    setMcpToolBridge(enabled: boolean): Promise<void> {
      mcpBridge = enabled;
      return wait(undefined);
    },

    toolCalls: (since?: number): Promise<ToolCallsResponse> =>
      wait({
        summary: { ...toolSummary },
        records: allRecords().filter((r) => since === undefined || r.revision > since),
      }),
    toolCallDetail(callId: string): Promise<ToolCallDetail> {
      const detail = details.get(callId);
      return detail ? wait(detail, 90) : Promise.reject(new ApiError(404, "call evicted"));
    },
    toolCatalog(sessionId: string): Promise<SessionToolCatalog> {
      const session = sessionOf(sessionId);
      return session ? wait(buildCatalog(session), 160) : Promise.reject(new ApiError(404, "unknown session"));
    },

    contextCaptures: (): Promise<ContextCapturesResponse> => wait({ summary: { ...contextSummary }, records: contextRecords }),
    contextCaptureDetail(taskId: string): Promise<ContextCaptureDetailResponse> {
      const record = contextRecords.find((r) => r.task_id === taskId);
      const session = sessionOf(record?.session_id ?? null);
      if (!record || !session) return Promise.reject(new ApiError(404, "capture not found"));
      return wait({ record, payload: buildPayload(session, record.session_turn_index, taskId) }, 200);
    },
    updateContextCapture(update: ContextCaptureConfigUpdate): Promise<ContextCaptureSummary> {
      if (update.enabled !== undefined) contextSummary.enabled = update.enabled;
      if (update.max_records !== undefined) contextSummary.max_records = update.max_records;
      if (update.max_total_mib !== undefined) contextSummary.max_total_bytes = update.max_total_mib * 1024 * 1024;
      contextRecords = contextRecords.slice(0, contextSummary.max_records);
      refreshContextSummary();
      return wait({ ...contextSummary });
    },
    clearContextCaptures(): Promise<ContextCaptureSummary> {
      contextRecords = [];
      refreshContextSummary();
      return wait({ ...contextSummary });
    },

    probe(protocol: TaskProtocol, model: string, prompt: string): Promise<string> {
      return wait(`[${protocol === "openai_responses" ? "resp" : "msg"}_demo] （演示）${model} 收到了：${prompt}`, 700);
    },
  };
}
