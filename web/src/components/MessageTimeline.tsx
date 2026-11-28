import * as React from "react";
import { useEffect, useMemo, useRef, useState } from "react";
import { CircleAlert, FilterX, GitCompare, Image as ImageIcon } from "lucide-react";
import { ProseBlock } from "@/components/CodeBlock";
import { DiffBlock, sizeChange } from "@/components/ContextRewrites";
import { SegmentedBar } from "@/components/charts";
import { ToolGlyph, ToolInputView, ToolOutputView } from "@/components/ToolExchange";
import { Button, Disclosure, Pill, SearchInput, Segmented, Select } from "@/components/ui";
import {
  PART_KIND_ORDER,
  type PartKindId,
  partKindLabel,
  messagePartComposition,
  partBytes,
  partKindColor,
  partKindOf,
} from "@/lib/context";
import { formatBytes } from "@/lib/format";
import { useI18n } from "@/lib/i18n";
import { classifyTool, outputReportsFailure } from "@/lib/toolcalls";
import type { CapturedFullMessage, CapturedFullPart, ContextRewrite } from "@/types/telemetry";

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

/** Harnesses often report a failed script inside the text while leaving the flag unset. */
function partFailed(part: CapturedFullPart): boolean {
  return part.kind === "tool_result" && (part.is_error || outputReportsFailure(part.output));
}

function stripCallPrefix(id: string): string {
  return id.trim().replace(/^(?:call_|toolu_)/, "");
}

interface IndexedRewrite {
  index: number;
  rewrite: ContextRewrite;
}

/** Tool a call invokes or a result answers; `null` for every other part. */
function partToolName(
  part: CapturedFullPart,
  toolNames: ReadonlyMap<string, string>,
  partRewrite?: IndexedRewrite,
): string | null {
  if (part.kind === "tool_call") return part.name;
  if (part.kind === "tool_result") {
    return (
      toolNames.get(part.call_id) ??
      toolNames.get(stripCallPrefix(part.call_id)) ??
      partRewrite?.rewrite.tool_name ??
      null
    );
  }
  return null;
}

/** Lower-cased searchable text, computed once per captured part. */
const searchCache = new WeakMap<CapturedFullPart, string>();

function searchableText(part: CapturedFullPart, toolNames: ReadonlyMap<string, string>): string {
  const cached = searchCache.get(part);
  if (cached !== undefined) return cached;
  let text: string;
  switch (part.kind) {
    case "text":
    case "reasoning":
      text = part.text;
      break;
    case "tool_call":
      text = `${part.name}\n${part.call_id}\n${part.arguments}`;
      break;
    case "tool_result":
      text = `${toolNames.get(part.call_id) ?? toolNames.get(stripCallPrefix(part.call_id)) ?? ""}\n${part.call_id}\n${part.output}`;
      break;
    case "image":
      text = part.media_type;
      break;
  }
  const lowered = text.toLowerCase();
  searchCache.set(part, lowered);
  return lowered;
}

function PartView({
  part,
  open,
  toolNames,
  partRewrite,
  onShowRewrite,
}: {
  part: CapturedFullPart;
  open: boolean;
  /** call_id → tool name, so a result can say which call it answers. */
  toolNames: ReadonlyMap<string, string>;
  partRewrite?: IndexedRewrite;
  onShowRewrite?: (rewriteIndex: number) => void;
}): React.JSX.Element {
  const { m } = useI18n();
  const t = m.context.timeline;
  const kind = partKindOf(part);
  const toolName = partToolName(part, toolNames, partRewrite);
  const failed = partFailed(part);
  const rw = partRewrite?.rewrite;
  const canCompare = rw != null && rw.rewritten != null;
  const [partMode, setPartMode] = useState<"original" | "diff" | "forwarded">("original");
  const rwChange = rw ? sizeChange(rw.original_bytes, rw.rewritten_bytes) : null;

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
      {rw && (
        <Pill tone="warn">
          {rw.kind === "tool_result_spilled" ? t.partSpilled : t.partRewritten}
        </Pill>
      )}
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
      {canCompare || (partRewrite && onShowRewrite) ? (
        <div className="flex min-w-0 flex-col gap-2">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <span className="lg-num text-[11.5px] text-fg-3">
              {rw && (
                <>
                  {formatBytes(rw.original_bytes)} → {rw.rewritten == null ? "0 B" : formatBytes(rw.rewritten_bytes)}
                  {rwChange && (
                    <span className={rwChange.shrank ? "ml-1.5 text-ok-fg" : "ml-1.5 text-warn-fg"}>
                      {rwChange.shrank ? "−" : "+"}
                      {rwChange.pct}
                    </span>
                  )}
                </>
              )}
            </span>
            <span className="flex flex-wrap items-center gap-1.5">
              {canCompare && (
                <Segmented<"original" | "diff" | "forwarded">
                  ariaLabel={t.partViewAria}
                  value={partMode}
                  onChange={setPartMode}
                  options={[
                    { value: "original", label: t.partOriginal },
                    { value: "diff", label: t.partDiff },
                    { value: "forwarded", label: t.partForwarded },
                  ]}
                />
              )}
              {partRewrite && onShowRewrite && (
                <Button size="sm" variant="ghost" onClick={() => onShowRewrite(partRewrite.index)}>
                  <GitCompare />
                  {t.openRewrite}
                </Button>
              )}
            </span>
          </div>
          {canCompare && partMode === "diff" ? (
            <DiffBlock original={rw.original} rewritten={rw.rewritten ?? ""} maxHeight={360} />
          ) : canCompare && partMode === "forwarded" ? (
            part.kind === "tool_result" ? (
              <ToolOutputView
                blocks={[{ type: "text", bytes: rw.rewritten_bytes, text: rw.rewritten ?? "" }]}
                failed={failed}
                maxHeight={360}
              />
            ) : (
              <ProseBlock text={rw.rewritten ?? ""} title={t.partForwarded} maxHeight={360} />
            )
          ) : (
            body
          )}
        </div>
      ) : (
        body
      )}
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

interface TimelineFilter {
  /** Case-insensitive substring over part text, tool names, and call ids. */
  query: string;
  role: string;
  kind: "all" | PartKindId;
  tool: string;
  errorsOnly: boolean;
  rewrittenOnly: boolean;
}

const EMPTY_FILTER: TimelineFilter = {
  query: "",
  role: "all",
  kind: "all",
  tool: "all",
  errorsOnly: false,
  rewrittenOnly: false,
};

/**
 * The conversation exactly as the gateway held it for one turn: resident
 * prefix first, then a rule, then what this turn actually sent upstream.
 */
export function MessageTimeline({
  messages,
  initialScope,
  rewrites = [],
  focus = null,
  onShowRewrite,
}: {
  messages: CapturedFullMessage[];
  /** Scope to open on; defaults to this turn's delta for long histories. */
  initialScope?: TimelineScope;
  /** Gateway rewrites of this turn, marked on the messages they touched. */
  rewrites?: readonly ContextRewrite[];
  /** Message index to reveal and highlight on mount. */
  focus?: number | null;
  /** Opens the comparison for a rewrite (index into `rewrites`). */
  onShowRewrite?: (rewriteIndex: number) => void;
}): React.JSX.Element {
  const { m } = useI18n();
  const t = m.context.timeline;
  const f = t.filters;
  const rootRef = useRef<HTMLDivElement>(null);
  const deltaCount = useMemo(() => messages.filter((msg) => msg.in_delta).length, [messages]);
  const focusPos = useMemo(
    () => (focus !== null ? messages.findIndex((msg) => msg.index === focus) : -1),
    [focus, messages],
  );
  // A focus request opens on everything, deep enough to include the focused message.
  const [scope, setScope] = useState<Scope>(
    focus !== null ? "all" : (initialScope ?? (deltaCount > 0 && messages.length > PAGE ? "delta" : "all")),
  );
  const [limit, setLimit] = useState(focusPos >= 0 ? Math.max(PAGE, messages.length - focusPos) : PAGE);
  const [filter, setFilter] = useState<TimelineFilter>(EMPTY_FILTER);
  const patchFilter = (patch: Partial<TimelineFilter>): void => {
    setFilter((prev) => ({ ...prev, ...patch }));
    setLimit(PAGE);
  };

  useEffect(() => {
    if (focus === null) return;
    setScope("all");
    setFilter(EMPTY_FILTER);
    if (focusPos >= 0) {
      setLimit((prev) => Math.max(prev, messages.length - focusPos));
    }
    rootRef.current?.querySelector(`[data-msg="${focus}"]`)?.scrollIntoView({ block: "center", behavior: "smooth" });
  }, [focus, focusPos, messages.length]);

  const toolNames = useMemo(() => {
    const names = new Map<string, string>();
    for (const msg of messages) {
      for (const part of msg.parts) {
        if (part.kind === "tool_call") {
          names.set(part.call_id, part.name);
          names.set(stripCallPrefix(part.call_id), part.name);
        }
      }
    }
    for (const rw of rewrites) {
      if (rw.call_id && rw.tool_name) {
        names.set(rw.call_id, rw.tool_name);
        names.set(stripCallPrefix(rw.call_id), rw.tool_name);
      }
    }
    return names;
  }, [messages, rewrites]);

  const rewritesByMessage = useMemo(() => {
    const out = new Map<number, { first: number; count: number }>();
    rewrites.forEach((rw, idx) => {
      if (rw.message_index == null) return;
      const entry = out.get(rw.message_index);
      if (entry) entry.count += 1;
      else out.set(rw.message_index, { first: idx, count: 1 });
    });
    return out;
  }, [rewrites]);

  const rewritesByPart = useMemo(() => {
    const out = new Map<string, IndexedRewrite>();
    rewrites.forEach((rewrite, index) => {
      if (rewrite.message_index != null && rewrite.part_index != null) {
        const key = `${rewrite.message_index}:${rewrite.part_index}`;
        if (!out.has(key)) out.set(key, { index, rewrite });
      }
    });
    return out;
  }, [rewrites]);

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

  // Facets count within the scope, so every chip says what it would leave.
  const facets = useMemo(() => {
    const roles = new Map<string, number>();
    const kinds = new Map<PartKindId, number>();
    const tools = new Map<string, number>();
    let errors = 0;
    let rewritten = 0;
    for (const msg of scoped) {
      roles.set(msg.role, (roles.get(msg.role) ?? 0) + 1);
      if (rewritesByMessage.has(msg.index)) rewritten += 1;
      msg.parts.forEach((part, partIdx) => {
        const kind = partKindOf(part);
        kinds.set(kind, (kinds.get(kind) ?? 0) + 1);
        const tool = partToolName(part, toolNames, rewritesByPart.get(`${msg.index}:${partIdx}`));
        if (tool !== null) tools.set(tool, (tools.get(tool) ?? 0) + 1);
        if (partFailed(part)) errors += 1;
      });
    }
    return { roles, kinds, tools, errors, rewritten };
  }, [scoped, rewritesByMessage, rewritesByPart, toolNames]);

  const query = filter.query.trim().toLowerCase();
  const partFilterActive = filter.kind !== "all" || filter.tool !== "all" || filter.errorsOnly || query !== "";
  const filterActive = partFilterActive || filter.role !== "all" || filter.rewrittenOnly;

  const filtered = useMemo(() => {
    const out: { msg: CapturedFullMessage; parts: { part: CapturedFullPart; idx: number }[] }[] = [];
    for (const msg of scoped) {
      if (filter.role !== "all" && msg.role !== filter.role) continue;
      if (filter.rewrittenOnly && !rewritesByMessage.has(msg.index)) continue;
      const parts = msg.parts
        .map((part, idx) => ({ part, idx }))
        .filter(({ part, idx }) => {
          if (!partFilterActive) return true;
          if (filter.kind !== "all" && partKindOf(part) !== filter.kind) return false;
          if (
            filter.tool !== "all" &&
            partToolName(part, toolNames, rewritesByPart.get(`${msg.index}:${idx}`)) !== filter.tool
          ) {
            return false;
          }
          if (filter.errorsOnly && !partFailed(part)) return false;
          return query === "" || searchableText(part, toolNames).includes(query);
        });
      if (parts.length > 0 || !partFilterActive) out.push({ msg, parts });
    }
    return out;
  }, [scoped, filter, query, partFilterActive, rewritesByMessage, rewritesByPart, toolNames]);

  // The newest messages matter most, so paging reveals older ones upward.
  const hidden = Math.max(0, filtered.length - limit);
  const shown = hidden > 0 ? filtered.slice(hidden) : filtered;
  const firstDeltaIndex = shown.find((entry) => entry.msg.in_delta)?.msg.index;
  const toolMessages = messages.filter((entry) =>
    entry.parts.some((p) => p.kind === "tool_call" || p.kind === "tool_result"),
  ).length;
  const toolOptions = [
    { value: "all", label: f.allTools },
    ...[...facets.tools].sort((a, b) => b[1] - a[1]).map(([name, count]) => ({ value: name, label: `${name} (${count})` })),
  ];
  if (filter.tool !== "all" && !facets.tools.has(filter.tool)) {
    toolOptions.push({ value: filter.tool, label: `${filter.tool} (0)` });
  }

  if (messages.length === 0) {
    return <p className="lg-hint m-0">{t.empty}</p>;
  }

  return (
    <div ref={rootRef} className="flex min-w-0 flex-col gap-3">
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

      <div className="flex flex-wrap items-center gap-2">
        <SearchInput
          className="min-w-[200px] flex-1"
          value={filter.query}
          onChange={(next) => patchFilter({ query: next })}
          placeholder={f.search}
        />
        {toolOptions.length > 1 && (
          <Select
            ariaLabel={f.tool}
            className="min-w-[160px] max-w-[280px] flex-1"
            value={filter.tool}
            onChange={(tool) => patchFilter({ tool })}
            options={toolOptions}
          />
        )}
        {filterActive && (
          <Button size="sm" variant="ghost" onClick={() => patchFilter(EMPTY_FILTER)}>
            <FilterX />
            {f.clear}
          </Button>
        )}
      </div>
      <div className="flex flex-wrap items-center gap-1.5">
        <span className="contents" role="group" aria-label={f.role}>
          {[...facets.roles].map(([role, count]) => (
            <button
              key={role}
              type="button"
              className="lg-chip"
              aria-pressed={filter.role === role}
              onClick={() => patchFilter({ role: filter.role === role ? "all" : role })}
            >
              {ROLE_LABEL[role] ?? role}
              <span className="lg-seg-count">{count}</span>
            </button>
          ))}
        </span>
        <span className="contents" role="group" aria-label={f.kind}>
          {PART_KIND_ORDER.filter((kind) => facets.kinds.has(kind) || filter.kind === kind).map((kind) => (
            <button
              key={kind}
              type="button"
              className="lg-chip lg-chip-plain"
              aria-pressed={filter.kind === kind}
              onClick={() => patchFilter({ kind: filter.kind === kind ? "all" : kind })}
            >
              <span className="lg-swatch" style={{ background: partKindColor(kind) }} aria-hidden="true" />
              {partKindLabel(kind)}
              <span className="lg-seg-count">{facets.kinds.get(kind) ?? 0}</span>
            </button>
          ))}
        </span>
        {(facets.errors > 0 || filter.errorsOnly) && (
          <button
            type="button"
            className="lg-chip"
            data-tone="err"
            aria-pressed={filter.errorsOnly}
            title={f.errorsHint}
            onClick={() => patchFilter({ errorsOnly: !filter.errorsOnly })}
          >
            <CircleAlert />
            {f.errors}
            <span className="lg-seg-count">{facets.errors}</span>
          </button>
        )}
        {(facets.rewritten > 0 || filter.rewrittenOnly) && (
          <button
            type="button"
            className="lg-chip"
            aria-pressed={filter.rewrittenOnly}
            title={f.rewrittenHint}
            onClick={() => patchFilter({ rewrittenOnly: !filter.rewrittenOnly })}
          >
            <GitCompare />
            {f.rewritten}
            <span className="lg-seg-count">{facets.rewritten}</span>
          </button>
        )}
      </div>
      {filterActive && (
        <p className="lg-hint m-0" aria-live="polite">
          {f.matched(filtered.length, scoped.length)}
        </p>
      )}

      {shown.length === 0 && <p className="lg-hint m-0">{t.noneInScope}</p>}

      {shown.map(({ msg, parts }) => {
        const rewrite = rewritesByMessage.get(msg.index);
        return (
          <React.Fragment key={msg.index}>
            {scope === "all" && !filterActive && msg.index === firstDeltaIndex && (
              <div className="lg-rule" role="separator">
                {t.deltaRule(deltaCount)}
              </div>
            )}
            <article
              className="lg-msg scroll-mt-4"
              data-delta={msg.in_delta}
              data-msg={msg.index}
              data-focus={focus === msg.index ? "true" : undefined}
            >
              <header className="lg-msg-head">
                <span className="lg-role">
                  #{msg.index} {ROLE_LABEL[msg.role] ?? msg.role}
                </span>
                <Pill tone={msg.in_delta ? "accent" : "neutral"}>
                  {msg.in_delta ? t.inDelta : t.resident}
                </Pill>
                {rewrite &&
                  (onShowRewrite ? (
                    <button
                      type="button"
                      className="lg-pill-btn"
                      title={t.rewrittenTitle}
                      onClick={() => onShowRewrite(rewrite.first)}
                    >
                      <Pill tone="warn">{t.rewritten(rewrite.count)}</Pill>
                    </button>
                  ) : (
                    <Pill tone="warn" title={t.rewrittenTitle}>
                      {t.rewritten(rewrite.count)}
                    </Pill>
                  ))}
                <span className="lg-num text-[11.5px]">
                  {t.parts(msg.parts.length, formatBytes(msg.total_bytes))}
                </span>
              </header>
              {parts.map(({ part, idx }) => (
                <PartView
                  // Parts are an ordered, immutable list; a filter remounts them to reopen matches.
                  key={`${idx}:${partFilterActive ? "f" : ""}`}
                  part={part}
                  open={(msg.in_delta && msg.parts.length <= 6) || (partFilterActive && parts.length <= 3)}
                  toolNames={toolNames}
                  partRewrite={rewritesByPart.get(`${msg.index}:${idx}`)}
                  onShowRewrite={onShowRewrite}
                />
              ))}
              {parts.length < msg.parts.length && (
                <p className="lg-hint m-0 text-[11.5px]">{f.hiddenParts(msg.parts.length - parts.length)}</p>
              )}
            </article>
          </React.Fragment>
        );
      })}
    </div>
  );
}
