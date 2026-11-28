import * as React from "react";
import { useEffect, useMemo, useRef, useState } from "react";
import { LocateFixed, WrapText } from "lucide-react";
import { DiffStat, ProseBlock } from "@/components/CodeBlock";
import { CopyButton } from "@/components/CopyButton";
import { Button, Disclosure, Pill, Segmented, type Tone } from "@/components/ui";
import { byteLength, formatBytes, formatPct, shortId } from "@/lib/format";
import { useI18n } from "@/lib/i18n";
import { diffLines, foldContext, revealControls } from "@/lib/textdiff";
import type { ContextRewrite } from "@/types/telemetry";

/** Diff rows mounted before the reader asks for the rest. */
const ROW_PAGE = 400;

type View = "diff" | "split";

function kindTone(kind: string): Tone {
  if (kind === "prompt_distilled") return "accent";
  if (kind === "prompt_dropped") return "warn";
  return "neutral";
}

function isPrompt(kind: string): boolean {
  return kind.startsWith("prompt_");
}

/** Size change in whole percent, e.g. "92%" for a distilled prompt; `null` below 0.5% either way. */
function sizeChange(before: number, after: number): { pct: string; shrank: boolean } | null {
  const ratio = before === 0 ? 0 : Math.abs(before - after) / before;
  if (ratio < 0.005) return null;
  return { pct: formatPct(ratio), shrank: after < before };
}

function DiffBlock({ original, rewritten }: { original: string; rewritten: string }): React.JSX.Element {
  const { m } = useI18n();
  const t = m.context.rewrites;
  const diff = useMemo(
    () => diffLines(revealControls(original), revealControls(rewritten)),
    [original, rewritten],
  );
  const [wrap, setWrap] = useState(true);
  const [unfolded, setUnfolded] = useState(false);
  const [limit, setLimit] = useState(ROW_PAGE);
  const rows = useMemo(
    () =>
      unfolded ? diff.lines.map((line) => ({ kind: "line" as const, line })) : foldContext(diff.lines),
    [diff, unfolded],
  );
  const shown = rows.length > limit ? rows.slice(0, limit) : rows;

  return (
    <div className="flex min-w-0 flex-col gap-2">
      {diff.coarse && <p className="lg-hint m-0">{t.coarse}</p>}
      <figure className="lg-codeblock m-0">
        <figcaption className="lg-codeblock-head">
          <span className="flex min-w-0 items-center gap-2 truncate">
            <DiffStat added={diff.added} removed={diff.removed} />
          </span>
          <span className="flex flex-none items-center gap-1">
            {unfolded && (
              <Button size="sm" variant="ghost" onClick={() => setUnfolded(false)}>
                {t.collapse}
              </Button>
            )}
            <button
              type="button"
              className="lg-btn"
              data-size="sm"
              data-variant="ghost"
              data-icon="true"
              aria-pressed={wrap}
              title={m.common.wrap}
              aria-label={m.common.wrap}
              onClick={() => setWrap((w) => !w)}
            >
              <WrapText />
            </button>
            <CopyButton
              value={() =>
                diff.lines
                  .map((l) => `${l.op === "add" ? "+" : l.op === "del" ? "-" : " "} ${l.text}`)
                  .join("\n")
              }
            />
          </span>
        </figcaption>
        <pre className="lg-code" data-wrap={wrap} style={{ maxHeight: 560 }} tabIndex={0}>
          <code className="lg-diff-lines">
            {shown.map((row, idx) =>
              row.kind === "gap" ? (
                // Rows are derived from immutable texts; position is a stable key.
                <button key={idx} type="button" className="lg-diff-gap" onClick={() => setUnfolded(true)}>
                  ⋯ {t.unchanged(row.hidden)}
                </button>
              ) : (
                <span
                  key={idx}
                  className="lg-code-row"
                  data-kind={row.line.op === "ctx" ? undefined : row.line.op}
                >
                  <span className="lg-code-ln">{row.line.oldNo ?? ""}</span>
                  <span className="lg-code-ln">{row.line.newNo ?? ""}</span>
                  <span className="lg-diff-sign">
                    {row.line.op === "add" ? "+" : row.line.op === "del" ? "−" : " "}
                  </span>
                  <span className="lg-code-text">{row.line.text}</span>
                </span>
              ),
            )}
          </code>
        </pre>
        {rows.length > limit && (
          <button type="button" className="lg-codeblock-more" onClick={() => setLimit((n) => n + ROW_PAGE * 4)}>
            {t.moreRows(rows.length - limit)}
          </button>
        )}
      </figure>
    </div>
  );
}

function RewriteItem({
  rewrite,
  focused,
  defaultOpen,
  onLocate,
}: {
  rewrite: ContextRewrite;
  focused: boolean;
  defaultOpen: boolean;
  onLocate?: (messageIndex: number) => void;
}): React.JSX.Element {
  const { m } = useI18n();
  const t = m.context.rewrites;
  const ref = useRef<HTMLDivElement>(null);
  const dropped = rewrite.rewritten == null;
  const [view, setView] = useState<View>(isPrompt(rewrite.kind) ? "split" : "diff");
  const change = sizeChange(rewrite.original_bytes, rewrite.rewritten_bytes);
  const msgIdx = rewrite.message_index;
  const located = msgIdx != null && rewrite.part_index != null;
  const where = [
    rewrite.slot ? (t.slot[rewrite.slot] ?? rewrite.slot) : null,
    rewrite.tool_name ?? null,
    rewrite.call_id ? shortId(rewrite.call_id) : null,
  ].filter((s): s is string => Boolean(s));
  // Texts past the per-part capture cap were head/tail trimmed on the gateway.
  const cut =
    byteLength(rewrite.original) < rewrite.original_bytes ||
    (rewrite.rewritten != null && byteLength(rewrite.rewritten) < rewrite.rewritten_bytes);

  useEffect(() => {
    if (focused) ref.current?.scrollIntoView({ block: "start", behavior: "smooth" });
  }, [focused]);

  const original = isPrompt(rewrite.kind) ? rewrite.original : revealControls(rewrite.original);
  const forwarded = rewrite.rewritten == null ? "" : isPrompt(rewrite.kind) ? rewrite.rewritten : revealControls(rewrite.rewritten);

  return (
    <div ref={ref} className="scroll-mt-4">
      <Disclosure
        defaultOpen={defaultOpen || focused}
        head={
          <>
            <Pill tone={kindTone(rewrite.kind)}>{t.kind[rewrite.kind] ?? rewrite.kind}</Pill>
            <span className="lg-num truncate text-[12px] text-fg-2">
              {located ? t.location(msgIdx, rewrite.part_index ?? 0) : t.unlocated}
              {where.length > 0 && <span className="text-fg-4"> · {where.join(" · ")}</span>}
            </span>
          </>
        }
        meta={
          <>
            {formatBytes(rewrite.original_bytes)} → {dropped ? "0 B" : formatBytes(rewrite.rewritten_bytes)}
            {change && (
              <span className={change.shrank ? "ml-1.5 text-ok-fg" : "ml-1.5 text-warn-fg"}>
                {change.shrank ? "−" : "+"}
                {change.pct}
              </span>
            )}
          </>
        }
      >
        <div className="flex min-w-0 flex-col gap-3">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <p className="lg-hint m-0 min-w-0 flex-1">{t.kindHint[rewrite.kind] ?? ""}</p>
            <span className="flex flex-none items-center gap-2">
              {located && onLocate && (
                <Button size="sm" variant="ghost" onClick={() => onLocate(msgIdx)}>
                  <LocateFixed />
                  {t.locate}
                </Button>
              )}
              {!dropped && (
                <Segmented<View>
                  ariaLabel={t.viewAria}
                  value={view}
                  onChange={setView}
                  options={[
                    { value: "diff", label: t.diff },
                    { value: "split", label: t.split },
                  ]}
                />
              )}
            </span>
          </div>
          {cut && <p className="lg-hint m-0">{t.cut}</p>}
          {dropped ? (
            <>
              <p className="lg-hint m-0">{t.droppedNote}</p>
              <ProseBlock text={original} title={t.original} maxHeight={480} />
            </>
          ) : view === "diff" ? (
            <DiffBlock original={rewrite.original} rewritten={rewrite.rewritten ?? ""} />
          ) : (
            <div className="grid min-w-0 gap-3 lg:grid-cols-2">
              <ProseBlock text={original} title={t.original} maxHeight={560} />
              <ProseBlock text={forwarded} title={t.forwarded} maxHeight={560} />
            </div>
          )}
        </div>
      </Disclosure>
    </div>
  );
}

/** Every place lg changed the harness context, each comparing what was sent with what went upstream. */
export function ContextRewritesView({
  rewrites,
  focus,
  onLocate,
}: {
  rewrites: readonly ContextRewrite[];
  /** Index into `rewrites` to open and scroll to. */
  focus?: number | null;
  onLocate?: (messageIndex: number) => void;
}): React.JSX.Element {
  const { m } = useI18n();
  const t = m.context.rewrites;
  const [kind, setKind] = useState<string>("all");
  const kinds = useMemo(() => {
    const counts = new Map<string, number>();
    for (const rw of rewrites) counts.set(rw.kind, (counts.get(rw.kind) ?? 0) + 1);
    return [...counts.entries()];
  }, [rewrites]);
  const before = rewrites.reduce((sum, rw) => sum + rw.original_bytes, 0);
  const after = rewrites.reduce((sum, rw) => sum + rw.rewritten_bytes, 0);
  const change = sizeChange(before, after);
  // A focus request always shows the focused rewrite, whatever the filter.
  const activeKind = focus != null ? "all" : kind;
  const listed = rewrites
    .map((rewrite, idx) => ({ rewrite, idx }))
    .filter(({ rewrite }) => activeKind === "all" || rewrite.kind === activeKind);

  if (rewrites.length === 0) {
    return <p className="lg-hint m-0">{t.none}</p>;
  }

  return (
    <div className="flex min-w-0 flex-col gap-3">
      <p className="lg-hint m-0">{t.intro}</p>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <span className="lg-num text-[12.5px] text-fg-2">
          <span className="font-semibold text-fg">{t.count(rewrites.length)}</span>
          <span className="text-fg-4"> · </span>
          {t.sizes(formatBytes(before), formatBytes(after))}
          {change && (
            <span className={change.shrank ? "ml-1.5 text-ok-fg" : "ml-1.5 text-warn-fg"}>
              {change.shrank ? t.saved(change.pct) : t.grew(change.pct)}
            </span>
          )}
        </span>
        {kinds.length > 1 && (
          <Segmented<string>
            ariaLabel={t.kindFilter}
            value={activeKind}
            onChange={setKind}
            options={[
              { value: "all", label: t.all, count: rewrites.length },
              ...kinds.map(([value, count]) => ({ value, label: t.kind[value] ?? value, count })),
            ]}
          />
        )}
      </div>
      {listed.map(({ rewrite, idx }) => (
        <RewriteItem
          // A focus request remounts the item so it opens even if the reader closed it.
          key={`${idx}:${focus === idx ? "focus" : ""}`}
          rewrite={rewrite}
          focused={focus === idx}
          defaultOpen={rewrites.length <= 3}
          onLocate={onLocate}
        />
      ))}
    </div>
  );
}
