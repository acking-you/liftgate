import * as React from "react";
import { useMemo, useState } from "react";
import { Image as ImageIcon } from "lucide-react";
import { ProseBlock } from "@/components/CodeBlock";
import { SegmentedBar } from "@/components/charts";
import { ToolGlyph, ToolInputView, ToolOutputView } from "@/components/ToolExchange";
import { Button, Disclosure, Pill, Segmented } from "@/components/ui";
import {
  partKindLabel,
  messagePartComposition,
  partBytes,
  partKindColor,
  partKindOf,
} from "@/lib/context";
import { formatBytes } from "@/lib/format";
import { useI18n } from "@/lib/i18n";
import { classifyTool, outputReportsFailure } from "@/lib/toolcalls";
import type { CapturedFullMessage, CapturedFullPart } from "@/types/telemetry";

/** Messages mounted per step; long histories grow on demand. */
const PAGE = 40;

export type TimelineScope = "all" | "delta" | "tools";
type Scope = TimelineScope;

function excerptOf(part: CapturedFullPart): string {
  switch (part.kind) {
    case "text":
    case "reasoning":
      return part.text.slice(0, 120).replace(/\s+/g, " ");
    case "tool_call":
      return part.arguments.slice(0, 120).replace(/\s+/g, " ");
    case "tool_result":
      return part.output.slice(0, 120).replace(/\s+/g, " ");
    case "image":
      return part.media_type;
  }
}

function PartView({
  part,
  open,
  toolNames,
}: {
  part: CapturedFullPart;
  open: boolean;
  /** call_id → tool name, so a result can say which call it answers. */
  toolNames: ReadonlyMap<string, string>;
}): React.JSX.Element {
  const { m } = useI18n();
  const t = m.context.timeline;
  const kind = partKindOf(part);
  const toolName =
    part.kind === "tool_call"
      ? part.name
      : part.kind === "tool_result"
        ? (toolNames.get(part.call_id) ?? null)
        : null;
  // Harnesses often report a failed script inside the text while leaving the flag unset.
  const failed = part.kind === "tool_result" && (part.is_error || outputReportsFailure(part.output));

  const head = (
    <>
      {toolName !== null ? (
        <ToolGlyph kind={classifyTool(toolName)} failed={failed} size="sm" />
      ) : (
        <span className="lg-swatch" style={{ background: partKindColor(kind) }} aria-hidden="true" />
      )}
      <span className="flex-none font-medium text-fg-2">{partKindLabel(kind)}</span>
      {toolName !== null && <span className="lg-num flex-none text-fg">{toolName}</span>}
      {failed && <Pill tone="err">{t.error}</Pill>}
      <span className="lg-clip lg-num min-w-0 flex-1 text-[11.5px] text-fg-4">{excerptOf(part)}</span>
    </>
  );

  let body: React.ReactNode;
  switch (part.kind) {
    case "text":
      body = (
        <ProseBlock
          text={part.text}
          title={part.is_developer_instructions ? "<developer_instructions>" : t.text}
        />
      );
      break;
    case "reasoning":
      body = <ProseBlock text={part.text} title={t.reasoning} />;
      break;
    case "tool_call":
      body = (
        <ToolInputView
          toolName={part.name}
          raw={part.arguments}
          label={part.is_custom ? t.customArgs : t.args}
          maxHeight={360}
        />
      );
      break;
    case "tool_result":
      body = (
        <ToolOutputView
          blocks={[{ type: "text", bytes: part.bytes, text: part.output }]}
          failed={failed}
          maxHeight={360}
        />
      );
      break;
    case "image":
      body = (
        <div className="flex items-center gap-2 text-[12.5px] text-fg-3">
          <ImageIcon className="h-4 w-4" />
          <span className="lg-num">
            {part.media_type} · base64 {formatBytes(part.base64_bytes)}
          </span>
        </div>
      );
      break;
  }

  return (
    <Disclosure
      head={head}
      meta={formatBytes(partBytes(part))}
      tone={failed ? "err" : undefined}
      defaultOpen={open}
    >
      {body}
    </Disclosure>
  );
}

const ROLE_LABEL: Record<string, string> = {
  user: "user",
  assistant: "assistant",
  system: "system",
  developer: "developer",
  tool: "tool",
};

/**
 * The conversation exactly as the gateway held it for one turn: resident
 * prefix first, then a rule, then what this turn actually sent upstream.
 */
export function MessageTimeline({
  messages,
  initialScope,
}: {
  messages: CapturedFullMessage[];
  /** Scope to open on; defaults to this turn's delta for long histories. */
  initialScope?: TimelineScope;
}): React.JSX.Element {
  const { m } = useI18n();
  const t = m.context.timeline;
  const deltaCount = useMemo(() => messages.filter((msg) => msg.in_delta).length, [messages]);
  const [scope, setScope] = useState<Scope>(
    initialScope ?? (deltaCount > 0 && messages.length > PAGE ? "delta" : "all"),
  );
  const [limit, setLimit] = useState(PAGE);

  const toolNames = useMemo(() => {
    const names = new Map<string, string>();
    for (const msg of messages) {
      for (const part of msg.parts) {
        if (part.kind === "tool_call") names.set(part.call_id, part.name);
      }
    }
    return names;
  }, [messages]);

  // Labels come from the catalog, so a language switch recomputes the composition too.
  const composition = useMemo(() => messagePartComposition(messages), [messages, m]);

  const scoped = useMemo(() => {
    if (scope === "delta") return messages.filter((entry) => entry.in_delta);
    if (scope === "tools") {
      return messages.filter((entry) =>
        entry.parts.some((p) => p.kind === "tool_call" || p.kind === "tool_result"),
      );
    }
    return messages;
  }, [messages, scope]);

  // The newest messages matter most, so paging reveals older ones upward.
  const hidden = Math.max(0, scoped.length - limit);
  const shown = hidden > 0 ? scoped.slice(hidden) : scoped;
  const firstDeltaIndex = shown.find((entry) => entry.in_delta)?.index;
  const toolMessages = messages.filter((entry) =>
    entry.parts.some((p) => p.kind === "tool_call" || p.kind === "tool_result"),
  ).length;

  if (messages.length === 0) {
    return <p className="lg-hint m-0">{t.empty}</p>;
  }

  return (
    <div className="flex min-w-0 flex-col gap-3">
      <SegmentedBar
        parts={composition}
        format={formatBytes}
        ariaLabel={t.compositionAria}
      />
      <div className="flex flex-wrap items-center justify-between gap-2">
        <Segmented<Scope>
          ariaLabel={t.scope}
          value={scope}
          onChange={(next) => {
            setScope(next);
            setLimit(PAGE);
          }}
          options={[
            { value: "all", label: t.all, count: messages.length },
            { value: "delta", label: t.delta, count: deltaCount },
            { value: "tools", label: t.tools, count: toolMessages },
          ]}
        />
        {hidden > 0 && (
          <Button size="sm" onClick={() => setLimit((n) => n + PAGE * 2)}>
            {t.showEarlier(Math.min(hidden, PAGE * 2), hidden)}
          </Button>
        )}
      </div>

      {shown.length === 0 && <p className="lg-hint m-0">{t.noneInScope}</p>}

      {shown.map((msg) => (
        <React.Fragment key={msg.index}>
          {scope === "all" && msg.index === firstDeltaIndex && (
            <div className="lg-rule" role="separator">
              {t.deltaRule(deltaCount)}
            </div>
          )}
          <article className="lg-msg" data-delta={msg.in_delta}>
            <header className="lg-msg-head">
              <span className="lg-role">
                #{msg.index} {ROLE_LABEL[msg.role] ?? msg.role}
              </span>
              <Pill tone={msg.in_delta ? "accent" : "neutral"}>
                {msg.in_delta ? t.inDelta : t.resident}
              </Pill>
              <span className="lg-num text-[11.5px]">
                {t.parts(msg.parts.length, formatBytes(msg.total_bytes))}
              </span>
            </header>
            {msg.parts.map((part, idx) => (
              // Parts are an ordered, immutable list inside a captured message.
              <PartView key={idx} part={part} open={msg.in_delta && msg.parts.length <= 6} toolNames={toolNames} />
            ))}
          </article>
        </React.Fragment>
      ))}
    </div>
  );
}
