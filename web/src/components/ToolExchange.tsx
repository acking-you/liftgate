import * as React from "react";
import { useMemo, useState } from "react";
import {
  Braces,
  FileDiff,
  FileText,
  Globe,
  Image as ImageIcon,
  PackageSearch,
  SquareTerminal,
  Wrench,
} from "lucide-react";
import { CodeBlock, PatchBlock, ProseBlock } from "@/components/CodeBlock";
import { Pill } from "@/components/ui";
import { formatBytes, formatNumber } from "@/lib/format";
import { harnessMeta } from "@/lib/harness";
import {
  type ToolKind,
  classifyTool,
  nestedToolUses,
  outputSections,
  parseOutputEnvelope,
  parseToolInput,
  prettyJson,
} from "@/lib/toolcalls";
import type { ToolCallRecord, ToolOutputBlock } from "@/types/telemetry";

const KIND_ICON: Record<ToolKind, React.ComponentType<{ className?: string }>> = {
  orchestrator: Braces,
  patch: FileDiff,
  shell: SquareTerminal,
  file: FileText,
  search: PackageSearch,
  web: Globe,
  other: Wrench,
};

/**
 * The mark every tool call wears. Ink weight carries the one distinction worth
 * seeing at a glance (CodeMode orchestrator vs. plain call); red is failure.
 */
export function ToolGlyph({
  kind,
  failed = false,
  size = "md",
}: {
  kind: ToolKind;
  failed?: boolean;
  size?: "sm" | "md" | "lg";
}): React.JSX.Element {
  const Icon = KIND_ICON[kind];
  return (
    <span className="lg-glyph" data-kind={kind} data-failed={failed} data-size={size} aria-hidden="true">
      <Icon />
    </span>
  );
}

/* ---------- Route flow ---------- */

interface FlowNode {
  caption: string;
  name: string;
  tone?: "ok" | "err" | "warn" | "accent";
  /** Drawn dashed: the hop that never happened. */
  skipped?: boolean;
}

function flowNodes(rec: ToolCallRecord): FlowNode[] {
  const failed = rec.status === "error";
  const endTone =
    rec.status === "pending"
      ? "accent"
      : failed
        ? "err"
        : rec.status === "abandoned"
          ? undefined
          : "ok";
  const harness = harnessMeta(rec.harness_profile).label;
  const model: FlowNode = { caption: "模型发出", name: rec.model_id };
  const client = rec.client_tool_name ?? rec.wire_tool_name;
  switch (rec.route) {
    case "client_roundtrip":
    case "unknown":
      return [
        model,
        { caption: rec.is_orchestrator ? "CodeMode 编排器" : "Wire 工具", name: rec.wire_tool_name },
        {
          caption: `${harness} 执行`,
          name: rec.status === "abandoned" ? "结果未返回" : "客户端往返",
          tone: endTone,
          skipped: rec.status === "abandoned",
        },
      ];
    case "namespace_hub":
    case "generic_dispatcher":
      return [
        model,
        {
          caption: rec.route === "namespace_hub" ? "命名空间 Hub" : "通用分发器",
          name: rec.wire_tool_name,
        },
        { caption: "网关解包为", name: client },
        { caption: `${harness} 执行`, name: "客户端往返", tone: endTone },
      ];
    case "zero_rtt_preflight":
      return [
        model,
        { caption: "Wire 工具", name: rec.wire_tool_name },
        {
          caption: "网关预检",
          name: rec.status === "preflight_rejected" ? "拦截并本地报错" : "本地自愈",
          tone: rec.status === "preflight_rejected" ? "warn" : "ok",
        },
        { caption: `${harness}`, name: "未到达客户端", skipped: true },
      ];
    case "zero_rtt_search":
    case "zero_rtt_spill_read":
      return [
        model,
        { caption: "网关内置工具", name: rec.wire_tool_name },
        {
          caption: "0-RTT 本地应答",
          name: rec.route === "zero_rtt_search" ? "检索工具索引" : "读取分片",
          tone: endTone,
        },
      ];
    case "gateway_managed":
      return [
        model,
        { caption: "网关托管工具", name: rec.wire_tool_name },
        { caption: "网关执行", name: "无客户端往返", tone: endTone },
      ];
  }
}

/** Where a call travelled: model → wire tool → (unwrap) → client or gateway. */
export function RouteFlow({ record }: { record: ToolCallRecord }): React.JSX.Element {
  const nodes = flowNodes(record);
  const live = record.status === "pending";
  return (
    <ol className="lg-flow" aria-label="调用路由">
      {nodes.map((node, idx) => (
        <React.Fragment key={`${node.caption}-${node.name}`}>
          {idx > 0 && (
            <li
              className="lg-flow-link"
              data-live={live}
              data-skipped={node.skipped ? "true" : undefined}
              aria-hidden="true"
            />
          )}
          <li className="lg-flow-node" data-tone={node.tone} data-skipped={node.skipped ? "true" : undefined}>
            <span className="lg-flow-cap">{node.caption}</span>
            <span className="lg-flow-name" title={node.name}>
              {node.name}
            </span>
          </li>
        </React.Fragment>
      ))}
    </ol>
  );
}

/* ---------- Input ---------- */

function ParamChips({ params }: { params: Array<[string, string]> }): React.JSX.Element | null {
  if (params.length === 0) return null;
  return (
    <ul className="lg-params">
      {params.map(([key, value]) => (
        <li key={key} className="lg-param" title={value}>
          <span>{key}</span>
          <code>{value.length > 96 ? `${value.slice(0, 96)}…` : value}</code>
        </li>
      ))}
    </ul>
  );
}

/** Flat argument objects read better as a short list than as a JSON block. */
function flatArguments(pretty: string): Array<[string, string]> | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(pretty);
  } catch {
    return null;
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) return null;
  const entries = Object.entries(parsed as Record<string, unknown>);
  if (entries.length === 0 || entries.length > 8) return null;
  const out: Array<[string, string]> = [];
  for (const [key, value] of entries) {
    if (value !== null && typeof value === "object") return null;
    const text = typeof value === "string" ? value : JSON.stringify(value);
    if (text.length > 240 || text.includes("\n")) return null;
    out.push([key, text]);
  }
  return out;
}

const SCRIPT_TITLE: Record<string, string> = { js: "JavaScript", py: "Python", sh: "Shell" };

/**
 * A tool call's arguments in the form a person would want to read them:
 * CodeMode scripts and shell commands out of their JSON escaping, patches as
 * diffs, flat argument objects as a list. The raw string is one click away.
 */
export function ToolInputView({
  toolName,
  raw,
  isOrchestrator,
  language,
  maxHeight = 440,
  label = "输入",
  aside,
}: {
  toolName: string;
  raw: string;
  isOrchestrator?: boolean;
  language?: string | null;
  maxHeight?: number | null;
  label?: string;
  /** Extra controls rendered beside the heading. */
  aside?: React.ReactNode;
}): React.JSX.Element {
  const [showRaw, setShowRaw] = useState(false);
  const [focusTool, setFocusTool] = useState<string | null>(null);
  const parsed = useMemo(
    () => parseToolInput(toolName, raw, { isOrchestrator, language }),
    [toolName, raw, isOrchestrator, language],
  );
  const kind = classifyTool(toolName, isOrchestrator);
  const nested = useMemo(
    () => (parsed.view === "code" && kind === "orchestrator" ? nestedToolUses(parsed.code) : []),
    [parsed, kind],
  );
  const highlight = useMemo(() => {
    const lines = nested.find((n) => n.name === focusTool)?.lines;
    return lines ? new Set(lines) : undefined;
  }, [nested, focusTool]);

  let body: React.ReactNode;
  if (raw.trim().length === 0) {
    body = <p className="lg-hint m-0">这次调用没有携带参数。</p>;
  } else if (showRaw) {
    body = <CodeBlock code={raw} language="text" title="模型原始输出的参数字符串" wrap maxHeight={maxHeight} />;
  } else if (parsed.view === "code") {
    body = (
      <>
        <ParamChips params={parsed.params} />
        {nested.length > 0 && (
          <div className="flex flex-wrap items-center gap-1.5">
            <span className="text-[12px] text-fg-3">脚本内调用</span>
            {nested.map((use) => (
              <button
                key={use.name}
                type="button"
                className="lg-chip"
                aria-pressed={focusTool === use.name}
                title={`定位到 tools.${use.name} 所在的 ${use.lines.length} 行`}
                onClick={() => setFocusTool((cur) => (cur === use.name ? null : use.name))}
              >
                <ToolGlyph kind={classifyTool(use.name)} size="sm" />
                <span className="lg-num">{use.name}</span>
                <span className="lg-seg-count">×{use.lines.length}</span>
              </button>
            ))}
          </div>
        )}
        <CodeBlock
          code={parsed.code}
          language={parsed.language}
          title={
            kind === "shell"
              ? "$ 命令"
              : `脚本 · ${SCRIPT_TITLE[parsed.language] ?? parsed.language}`
          }
          maxHeight={maxHeight}
          highlightLines={highlight}
        />
      </>
    );
  } else if (parsed.view === "patch") {
    body = (
      <>
        <ParamChips params={parsed.params} />
        <PatchBlock patch={parsed.patch} maxHeight={maxHeight} />
      </>
    );
  } else if (parsed.view === "json") {
    const flat = flatArguments(parsed.pretty);
    body = flat ? (
      <dl className="lg-kv lg-well">
        {flat.map(([key, value]) => (
          <React.Fragment key={key}>
            <dt className="lg-num text-[12px]">{key}</dt>
            <dd className="lg-num text-[12px]">{value}</dd>
          </React.Fragment>
        ))}
      </dl>
    ) : (
      <CodeBlock code={parsed.pretty} language="json" title="参数 · JSON" maxHeight={maxHeight} />
    );
  } else {
    body = <CodeBlock code={parsed.text} language="text" title="参数" wrap maxHeight={maxHeight} />;
  }

  return (
    <section className="flex min-w-0 flex-col gap-2">
      <div className="flex items-center justify-between gap-2">
        <h3 className="lg-section-title m-0">{label}</h3>
        <div className="flex items-center gap-1.5">
          {aside}
          {raw.trim().length > 0 && (
            <button
              type="button"
              className="lg-btn"
              data-size="sm"
              data-variant="ghost"
              aria-pressed={showRaw}
              title="查看未经解析的原始参数字符串"
              onClick={() => setShowRaw((v) => !v)}
            >
              原始
            </button>
          )}
        </div>
      </div>
      {body}
    </section>
  );
}

/* ---------- Output ---------- */


function hasLongLines(text: string): boolean {
  return text.split("\n", 400).some((line) => line.length > 220);
}

/** Terminal-style body with jump chips for the `=== section ===` rulers scripts print. */
function TerminalOutput({
  body,
  title,
  tone,
  maxHeight,
}: {
  body: string;
  title: string;
  tone?: "err";
  maxHeight: number | null;
}): React.JSX.Element {
  const [focus, setFocus] = useState<number | null>(null);
  const text = body.replace(/\n$/, "");
  const sections = useMemo(() => outputSections(text), [text]);
  const highlight = useMemo(() => (focus !== null ? new Set([focus]) : undefined), [focus]);
  return (
    <>
      {sections.length > 1 && (
        <div className="flex flex-wrap items-center gap-1.5">
          <span className="text-[12px] text-fg-3">分段</span>
          {sections.map((section) => (
            <button
              key={`${section.line}-${section.title}`}
              type="button"
              className="lg-chip lg-chip-plain"
              aria-pressed={focus === section.line}
              onClick={() => setFocus((cur) => (cur === section.line ? null : section.line))}
            >
              <span className="lg-num">{section.title}</span>
            </button>
          ))}
        </div>
      )}
      <CodeBlock
        code={text}
        language="text"
        title={title}
        tone={tone}
        wrap={hasLongLines(text)}
        maxHeight={maxHeight}
        highlightLines={highlight}
      />
    </>
  );
}

function TextOutput({
  block,
  failed,
  maxHeight,
}: {
  block: Extract<ToolOutputBlock, { type: "text" }>;
  failed: boolean;
  maxHeight: number | null;
}): React.JSX.Element {
  const tone = failed ? "err" : undefined;
  const title = "输出";
  const envelope = useMemo(() => parseOutputEnvelope(block.text), [block.text]);
  const pretty = useMemo(() => (envelope ? null : prettyJson(block.text)), [envelope, block.text]);

  if (envelope) {
    const bad = envelope.script === "failed" || (envelope.exitCode !== null && envelope.exitCode !== 0);
    return (
      <div className="flex min-w-0 flex-col gap-2">
        <div className="flex flex-wrap items-center gap-1.5">
          {envelope.script !== null && (
            <Pill tone={envelope.script === "completed" ? "ok" : "err"}>
              {envelope.script === "completed" ? "脚本完成" : "脚本失败"}
            </Pill>
          )}
          {envelope.exitCode !== null && (
            <Pill tone={envelope.exitCode === 0 ? "ok" : "err"}>退出码 {envelope.exitCode}</Pill>
          )}
          {envelope.wallSeconds !== null && <Pill>墙钟 {envelope.wallSeconds.toFixed(2)} s</Pill>}
          {envelope.truncated && (
            <Pill tone="warn" title="Harness 在返回前已截断这段输出">
              已被截断{envelope.totalLines !== null ? ` · 原 ${formatNumber(envelope.totalLines)} 行` : ""}
            </Pill>
          )}
          {envelope.originalTokens !== null && (
            <Pill>原始 {formatNumber(envelope.originalTokens)} tokens</Pill>
          )}
          {envelope.chunkId !== null && <Pill>分块 {envelope.chunkId}</Pill>}
        </div>
        {envelope.body.trim().length > 0 ? (
          <TerminalOutput
            body={envelope.body}
            title="终端输出"
            tone={bad ? "err" : tone}
            maxHeight={maxHeight}
          />
        ) : (
          <p className="lg-hint m-0">没有产生输出。</p>
        )}
      </div>
    );
  }
  if (pretty !== null) {
    return <CodeBlock code={pretty} language="json" title={`${title} · JSON`} tone={tone} maxHeight={maxHeight} />;
  }
  if (block.text.includes("\n")) {
    return <TerminalOutput body={block.text} title={title} tone={tone} maxHeight={maxHeight} />;
  }
  return <ProseBlock text={block.text} title={title} tone={tone} maxHeight={maxHeight} />;
}

/** Image results carry no pixels here, so draw the frame at its true aspect ratio. */
function ImageOutput({
  block,
}: {
  block: Extract<ToolOutputBlock, { type: "image" }>;
}): React.JSX.Element {
  const w = block.width ?? null;
  const h = block.height ?? null;
  const ratio = w && h ? w / h : 4 / 3;
  const boxH = 96;
  const boxW = Math.max(56, Math.min(220, Math.round(boxH * ratio)));
  return (
    <div className="flex items-center gap-3 rounded-lg border border-line p-3">
      <div className="lg-imgframe" style={{ width: boxW, height: boxH }}>
        <ImageIcon />
      </div>
      <div className="min-w-0 text-[12.5px]">
        <div className="font-medium text-fg">图像块</div>
        <div className="lg-num mt-0.5 text-[12px] text-fg-3">
          {block.mime_type}
          {w && h ? ` · ${w} × ${h}` : " · 尺寸未知"}
        </div>
        <div className="lg-num text-[12px] text-fg-3">base64 {formatBytes(block.base64_bytes)}</div>
      </div>
    </div>
  );
}

export function ToolOutputView({
  blocks,
  failed = false,
  maxHeight = 440,
}: {
  blocks: ToolOutputBlock[];
  failed?: boolean;
  maxHeight?: number | null;
}): React.JSX.Element {
  if (blocks.length === 0) {
    return <p className="lg-hint m-0">这次调用没有返回内容。</p>;
  }
  return (
    <div className="flex min-w-0 flex-col gap-2">
      {blocks.map((block, idx) => {
        // Output blocks are an ordered, immutable list.
        if (block.type === "text") {
          return <TextOutput key={idx} block={block} failed={failed} maxHeight={maxHeight} />;
        }
        return <ImageOutput key={idx} block={block} />;
      })}
    </div>
  );
}
