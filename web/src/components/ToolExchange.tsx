import * as React from "react";
import { useMemo, useState } from "react";
import {
  Braces,
  FileDiff,
  FileText,
  Globe,
  PackageSearch,
  SquareTerminal,
  Wrench,
} from "lucide-react";
import { CodeBlock, PatchBlock, ProseBlock } from "@/components/CodeBlock";
import { ImagePreview, WebInputCard, WebOutputView } from "@/components/RichContent";
import { Pill } from "@/components/ui";
import { formatNumber } from "@/lib/format";
import { harnessMeta } from "@/lib/harness";
import { parseWebInput, webToolMode } from "@/lib/webtools";
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
import { msg, useI18n } from "@/lib/i18n";

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
  const f = msg().tools.flow;
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
  const model: FlowNode = { caption: f.modelEmits, name: rec.model_id };
  const client = rec.client_tool_name ?? rec.wire_tool_name;
  switch (rec.route) {
    case "client_roundtrip":
    case "unknown":
      return [
        model,
        { caption: rec.is_orchestrator ? f.orchestrator : f.wireTool, name: rec.wire_tool_name },
        {
          caption: f.runs(harness),
          name: rec.status === "abandoned" ? f.noResult : f.roundtrip,
          tone: endTone,
          skipped: rec.status === "abandoned",
        },
      ];
    case "namespace_hub":
    case "generic_dispatcher":
      return [
        model,
        {
          caption: rec.route === "namespace_hub" ? f.hub : f.dispatcher,
          name: rec.wire_tool_name,
        },
        { caption: f.unwrapsTo, name: client },
        { caption: f.runs(harness), name: f.roundtrip, tone: endTone },
      ];
    case "zero_rtt_preflight":
      return [
        model,
        { caption: f.wireTool, name: rec.wire_tool_name },
        {
          caption: f.preflight,
          name: rec.status === "preflight_rejected" ? f.rejectedLocally : f.healed,
          tone: rec.status === "preflight_rejected" ? "warn" : "ok",
        },
        { caption: `${harness}`, name: f.neverReached, skipped: true },
      ];
    case "zero_rtt_search":
    case "zero_rtt_spill_read":
      return [
        model,
        { caption: f.builtin, name: rec.wire_tool_name },
        {
          caption: f.localReply,
          name: rec.route === "zero_rtt_search" ? f.searchIndex : f.readSpill,
          tone: endTone,
        },
      ];
    case "gateway_managed":
      return [
        model,
        { caption: f.managed, name: rec.wire_tool_name },
        { caption: f.gatewayRuns, name: f.noRoundtrip, tone: endTone },
      ];
  }
}

/** Where a call travelled: model → wire tool → (unwrap) → client or gateway. */
export function RouteFlow({ record }: { record: ToolCallRecord }): React.JSX.Element {
  const nodes = flowNodes(record);
  const live = record.status === "pending";
  return (
    <ol className="lg-flow" aria-label={msg().tools.flow.aria}>
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
  label,
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
  const { m } = useI18n();
  const ti = m.tools.input;
  const [showRaw, setShowRaw] = useState(false);
  const [focusTool, setFocusTool] = useState<{ name: string; idx: number; tick: number } | null>(null);
  const parsed = useMemo(
    () => parseToolInput(toolName, raw, { isOrchestrator, language }),
    [toolName, raw, isOrchestrator, language],
  );
  const kind = classifyTool(toolName, isOrchestrator);
  const webMode = webToolMode(toolName);
  const webInput = useMemo(() => (webMode ? parseWebInput(raw) : null), [webMode, raw]);
  const nested = useMemo(
    () => (parsed.view === "code" && kind === "orchestrator" ? nestedToolUses(parsed.code) : []),
    [parsed, kind],
  );
  const activeNestedLines = useMemo(
    () => (focusTool ? nested.find((n) => n.name === focusTool.name)?.lines : undefined),
    [nested, focusTool],
  );
  const highlight = useMemo(
    () => (activeNestedLines ? new Set(activeNestedLines) : undefined),
    [activeNestedLines],
  );
  const scrollLine =
    activeNestedLines && focusTool ? (activeNestedLines[focusTool.idx % activeNestedLines.length] ?? null) : null;

  let body: React.ReactNode;
  if (raw.trim().length === 0) {
    body = <p className="lg-hint m-0">{ti.noArgs}</p>;
  } else if (showRaw) {
    body = <CodeBlock code={raw} language="text" title={ti.rawTitle} wrap maxHeight={maxHeight} />;
  } else if (webMode && webInput) {
    body = (
      <>
        <WebInputCard mode={webMode} input={webInput} />
        <ParamChips params={webInput.params} />
      </>
    );
  } else if (parsed.view === "code") {
    body = (
      <>
        <ParamChips params={parsed.params} />
        {nested.length > 0 && (
          <div className="flex flex-wrap items-center gap-1.5">
            <span className="text-[12px] text-fg-3">{ti.inScript}</span>
            {nested.map((use) => (
              <button
                key={use.name}
                type="button"
                className="lg-chip"
                aria-pressed={focusTool?.name === use.name}
                title={ti.locate(use.name, use.lines.length)}
                onClick={() =>
                  setFocusTool((cur) => {
                    if (cur?.name !== use.name) return { name: use.name, idx: 0, tick: (cur?.tick ?? 0) + 1 };
                    if (cur.idx + 1 < use.lines.length) return { name: use.name, idx: cur.idx + 1, tick: cur.tick + 1 };
                    return null;
                  })
                }
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
            kind === "shell" ? ti.command : ti.script(SCRIPT_TITLE[parsed.language] ?? parsed.language)
          }
          maxHeight={maxHeight}
          highlightLines={highlight}
          scrollLine={scrollLine}
          scrollNonce={focusTool?.tick}
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
      <CodeBlock code={parsed.pretty} language="json" title={ti.argsJson} maxHeight={maxHeight} />
    );
  } else {
    body = <CodeBlock code={parsed.text} language="text" title={ti.args} wrap maxHeight={maxHeight} />;
  }

  return (
    <section className="flex min-w-0 flex-col gap-2">
      <div className="flex items-center justify-between gap-2">
        <h3 className="lg-section-title m-0">{label ?? ti.label}</h3>
        <div className="flex items-center gap-1.5">
          {aside}
          {raw.trim().length > 0 && (
            <button
              type="button"
              className="lg-btn"
              data-size="sm"
              data-variant="ghost"
              aria-pressed={showRaw}
              title={ti.rawHint}
              onClick={() => setShowRaw((v) => !v)}
            >
              {ti.raw}
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
  const [focus, setFocus] = useState<{ line: number; tick: number } | null>(null);
  const focusLine = focus?.line ?? null;
  const text = body.replace(/\n$/, "");
  const sections = useMemo(() => outputSections(text), [text]);
  const highlight = useMemo(() => (focusLine !== null ? new Set([focusLine]) : undefined), [focusLine]);
  return (
    <>
      {sections.length > 1 && (
        <div className="flex flex-wrap items-center gap-1.5">
          <span className="text-[12px] text-fg-3">{msg().tools.output.sections}</span>
          {sections.map((section) => (
            <button
              key={`${section.line}-${section.title}`}
              type="button"
              className="lg-chip lg-chip-plain"
              aria-pressed={focusLine === section.line}
              title={msg().tools.output.jumpSection(section.line + 1)}
              onClick={() => setFocus((cur) => ({ line: section.line, tick: (cur?.tick ?? 0) + 1 }))}
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
        scrollLine={focusLine}
        scrollNonce={focus?.tick}
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
  const { m } = useI18n();
  const to = m.tools.output;
  const tone = failed ? "err" : undefined;
  const title = to.label;
  const envelope = useMemo(() => parseOutputEnvelope(block.text), [block.text]);
  const pretty = useMemo(() => (envelope ? null : prettyJson(block.text)), [envelope, block.text]);

  if (envelope) {
    const bad = envelope.script === "failed" || (envelope.exitCode !== null && envelope.exitCode !== 0);
    return (
      <div className="flex min-w-0 flex-col gap-2">
        <div className="flex flex-wrap items-center gap-1.5">
          {envelope.script !== null && (
            <Pill tone={envelope.script === "completed" ? "ok" : "err"}>
              {envelope.script === "completed" ? to.scriptDone : to.scriptFailed}
            </Pill>
          )}
          {envelope.exitCode !== null && (
            <Pill tone={envelope.exitCode === 0 ? "ok" : "err"}>{to.exitCode(envelope.exitCode)}</Pill>
          )}
          {envelope.wallSeconds !== null && <Pill>{to.wall(envelope.wallSeconds.toFixed(2))}</Pill>}
          {envelope.truncated && (
            <Pill tone="warn" title={to.truncatedHint}>
              {to.truncated}
              {envelope.totalLines !== null ? to.originalLines(formatNumber(envelope.totalLines)) : ""}
            </Pill>
          )}
          {envelope.originalTokens !== null && (
            <Pill>{to.originalTokens(formatNumber(envelope.originalTokens))}</Pill>
          )}
          {envelope.chunkId !== null && <Pill>{to.chunk(String(envelope.chunkId))}</Pill>}
        </div>
        {envelope.body.trim().length > 0 ? (
          <TerminalOutput
            body={envelope.body}
            title={to.terminal}
            tone={bad ? "err" : tone}
            maxHeight={maxHeight}
          />
        ) : (
          <p className="lg-hint m-0">{to.noOutput}</p>
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

export function ToolOutputView({
  blocks,
  toolName,
  failed = false,
  maxHeight = 440,
}: {
  blocks: ToolOutputBlock[];
  /** Optional tool name so web search / web fetch outputs render with structured cards. */
  toolName?: string;
  failed?: boolean;
  maxHeight?: number | null;
}): React.JSX.Element {
  const webMode = toolName ? webToolMode(toolName) : null;
  if (blocks.length === 0) {
    return <p className="lg-hint m-0">{msg().tools.output.nothing}</p>;
  }
  return (
    <div className="flex min-w-0 flex-col gap-2">
      {blocks.map((block, idx) => {
        // Output blocks are an ordered, immutable list.
        if (block.type === "text") {
          const plain = <TextOutput block={block} failed={failed} maxHeight={maxHeight} />;
          if (webMode !== null) {
            return (
              <WebOutputView
                key={idx}
                mode={webMode}
                text={block.text}
                failed={failed}
                maxHeight={maxHeight}
                fallback={plain}
              />
            );
          }
          return <React.Fragment key={idx}>{plain}</React.Fragment>;
        }
        return (
          <ImagePreview
            key={idx}
            mediaType={block.mime_type}
            data={block.data}
            base64Bytes={block.base64_bytes}
            width={block.width}
            height={block.height}
          />
        );
      })}
    </div>
  );
}
