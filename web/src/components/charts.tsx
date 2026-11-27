import * as React from "react";
import { useEffect, useMemo, useRef, useState } from "react";
import {
  formatBpsPct,
  formatClock,
  formatCompact,
  formatDurationMs,
  taskHitBps,
  taskPromptTokens,
} from "@/lib/format";
import { useI18n } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import type { TaskSnapshot } from "@/types/telemetry";

function useElementWidth<T extends HTMLElement>(): [React.RefObject<T | null>, number] {
  const ref = useRef<T | null>(null);
  const [width, setWidth] = useState(0);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    setWidth(el.clientWidth);
    const observer = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (entry) setWidth(Math.round(entry.contentRect.width));
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  return [ref, width];
}

/* ---------- Segmented bar: one whole split into parts ---------- */

export interface SegmentPart {
  key: string;
  label: string;
  value: number;
  /** CSS colour for the block. */
  color: string;
  /** Ink for a label printed inside the block. */
  ink?: "light" | "dark";
}

export function SegmentedBar({
  parts,
  size = "tall",
  format,
  ariaLabel,
  legend = true,
}: {
  parts: SegmentPart[];
  size?: "tall" | "thin";
  format: (value: number) => string;
  ariaLabel: string;
  legend?: boolean;
}): React.JSX.Element {
  const { m } = useI18n();
  const [ref, width] = useElementWidth<HTMLDivElement>();
  const [active, setActive] = useState<string | null>(null);
  const total = parts.reduce((sum, p) => sum + Math.max(0, p.value), 0);
  const shown = parts.filter((p) => p.value > 0);
  const share = (p: SegmentPart): string =>
    total > 0 ? `${((p.value / total) * 100).toFixed(p.value / total < 0.1 ? 1 : 0)}%` : "0%";
  const activePart = shown.find((p) => p.key === active) ?? null;

  return (
    <div className="min-w-0">
      <div className="relative">
        <div
          ref={ref}
          className="lg-segbar"
          data-size={size}
          role="img"
          aria-label={m.common.segmentAria(
            ariaLabel,
            shown.map((p) => m.common.segmentPart(p.label, format(p.value), share(p))),
          )}
        >
          {total === 0 && <div className="flex-1 bg-sunken" />}
          {shown.map((p) => {
            const text = share(p);
            const px = total > 0 ? (p.value / total) * width : 0;
            const fits = size === "tall" && px >= text.length * 7.2 + 16;
            return (
              <div
                key={p.key}
                style={{ flexGrow: p.value, flexBasis: 0, background: p.color }}
                onMouseEnter={() => setActive(p.key)}
                onMouseLeave={() => setActive(null)}
              >
                {fits && (
                  <span className="lg-segbar-label" data-ink={p.ink ?? "light"}>
                    {text}
                  </span>
                )}
              </div>
            );
          })}
        </div>
        {activePart && (
          <div className="lg-chart-tip left-0 top-full mt-1.5">
            <div className="flex items-center gap-1.5 font-medium text-fg">
              <span className="lg-swatch" style={{ background: activePart.color }} />
              {activePart.label}
            </div>
            <div className="lg-num mt-0.5">
              {format(activePart.value)} · {share(activePart)}
            </div>
          </div>
        )}
      </div>
      {legend && (
        <ul className="m-0 mt-2.5 flex list-none flex-wrap gap-x-4 gap-y-1 p-0 text-[12px] text-fg-3">
          {shown.map((p) => (
            <li key={p.key} className="flex items-center gap-1.5">
              <span className="lg-swatch" style={{ background: p.color }} aria-hidden="true" />
              <span>{p.label}</span>
              <span className="lg-num text-fg-2">{format(p.value)}</span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/* ---------- Ranked bars: compare parts on one baseline ---------- */

export interface RankedRow {
  key: string;
  label: React.ReactNode;
  value: number;
  /** Text printed at the bar tip. */
  display: string;
  hint?: string;
}

export function RankedBars({
  rows,
  selectedKey,
  onSelect,
  labelWidth = 168,
  color = "var(--lg-s1)",
}: {
  rows: RankedRow[];
  selectedKey?: string | null;
  onSelect?: (key: string) => void;
  labelWidth?: number;
  color?: string;
}): React.JSX.Element {
  const max = rows.reduce((m, r) => Math.max(m, r.value), 0);
  return (
    <ul className="m-0 flex list-none flex-col p-0">
      {rows.map((row) => {
        const pct = max > 0 ? (row.value / max) * 100 : 0;
        const content = (
          <>
            <span
              className="min-w-0 flex-none truncate text-[12.5px] text-fg-2"
              style={{ width: labelWidth }}
              title={row.hint}
            >
              {row.label}
            </span>
            <span className="flex min-w-0 flex-1 items-center">
              <span
                className="block h-2.5 rounded-r-[4px]"
                style={{ width: `${pct}%`, minWidth: row.value > 0 ? 2 : 0, background: color }}
              />
            </span>
            <span className="lg-num w-[84px] flex-none text-right text-[12px] text-fg-2">
              {row.display}
            </span>
          </>
        );
        const cls = cn(
          "flex w-full items-center gap-3 rounded-md px-2 py-[5px] text-left",
          onSelect && "cursor-pointer hover:bg-hover",
          selectedKey === row.key && "bg-accent-soft",
        );
        return (
          <li key={row.key}>
            {onSelect ? (
              <button
                type="button"
                className={cn(cls, "border-0 bg-transparent")}
                aria-pressed={selectedKey === row.key}
                onClick={() => onSelect(row.key)}
              >
                {content}
              </button>
            ) : (
              <div className={cls}>{content}</div>
            )}
          </li>
        );
      })}
    </ul>
  );
}

/* ---------- Sparkline ---------- */

export function Sparkline({
  values,
  width = 96,
  height = 28,
}: {
  values: number[];
  width?: number;
  height?: number;
}): React.JSX.Element | null {
  if (values.length < 2) return null;
  const min = Math.min(...values);
  const max = Math.max(...values);
  const span = max - min || 1;
  const pad = 3;
  const points = values.map((v, i) => {
    const x = pad + (i / (values.length - 1)) * (width - pad * 2);
    const y = pad + (1 - (v - min) / span) * (height - pad * 2);
    return [x, y] as const;
  });
  const last = points[points.length - 1];
  return (
    <svg width={width} height={height} aria-hidden="true" className="flex-none">
      <polyline
        points={points.map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join(" ")}
        fill="none"
        stroke="var(--lg-s-neutral)"
        strokeWidth="1.5"
        strokeLinejoin="round"
        strokeLinecap="round"
      />
      {last && <circle cx={last[0]} cy={last[1]} r="2.5" fill="var(--lg-s1)" />}
    </svg>
  );
}

/* ---------- Per-turn cache hit trend ---------- */

interface TrendPoint {
  task: TaskSnapshot;
  hitBps: number;
}

const TREND_HEIGHT = 196;
const TREND_PAD = { top: 14, right: 46, bottom: 24, left: 42 };
/** Room right of the plot for the threshold label, which is wider in English. */
const TREND_PAD_RIGHT = { zh: 46, en: 66 } as const;

export const CacheTrendChart = React.memo(function CacheTrendChart({
  tasks,
  thresholdBps,
  onSelect,
}: {
  tasks: TaskSnapshot[];
  thresholdBps: number;
  onSelect?: (task: TaskSnapshot) => void;
}): React.JSX.Element {
  const { m, locale } = useI18n();
  const padRight = TREND_PAD_RIGHT[locale];
  const [ref, width] = useElementWidth<HTMLDivElement>();
  const [hover, setHover] = useState<number | null>(null);

  const points = useMemo<TrendPoint[]>(() => {
    const out: TrendPoint[] = [];
    for (const task of tasks) {
      if (task.phase !== "completed" || !task.usage) continue;
      const hitBps = taskHitBps(task);
      if (hitBps === null) continue;
      out.push({ task, hitBps });
      if (out.length >= 60) break;
    }
    return out.reverse();
  }, [tasks]);

  const plotW = Math.max(0, width - TREND_PAD.left - padRight);
  const plotH = TREND_HEIGHT - TREND_PAD.top - TREND_PAD.bottom;
  const xAt = (i: number): number =>
    TREND_PAD.left + (points.length <= 1 ? plotW / 2 : (i / (points.length - 1)) * plotW);
  const yAt = (bps: number): number => TREND_PAD.top + (1 - bps / 10000) * plotH;

  const hovered = hover !== null ? points[hover] : undefined;
  const last = points[points.length - 1];
  const lowCount = points.filter((p) => p.hitBps < thresholdBps).length;

  const handleMove = (ev: React.MouseEvent<SVGSVGElement>): void => {
    if (points.length === 0 || plotW <= 0) return;
    const rect = ev.currentTarget.getBoundingClientRect();
    const ratio = (ev.clientX - rect.left - TREND_PAD.left) / plotW;
    const idx = Math.round(ratio * (points.length - 1));
    setHover(Math.max(0, Math.min(points.length - 1, idx)));
  };

  if (points.length === 0) {
    return (
      <div ref={ref} className="grid place-items-center text-[12.5px] text-fg-3" style={{ height: TREND_HEIGHT }}>
        {m.common.trend.empty}
      </div>
    );
  }

  const line = points.map((p, i) => `${xAt(i).toFixed(1)},${yAt(p.hitBps).toFixed(1)}`).join(" ");
  const baseY = yAt(0);
  const area = `M${xAt(0).toFixed(1)},${baseY} L${line.replace(/ /g, " L")} L${xAt(points.length - 1).toFixed(1)},${baseY} Z`;
  const thresholdY = yAt(thresholdBps);
  const tipLeft = hover !== null ? Math.min(Math.max(xAt(hover) - 90, 0), Math.max(0, width - 200)) : 0;

  return (
    <div ref={ref} className="relative">
      {width > 0 && (
        <svg
          width={width}
          height={TREND_HEIGHT}
          role="img"
          aria-label={m.common.trend.aria(points.length, formatBpsPct(last?.hitBps), lowCount)}
          className={onSelect ? "cursor-pointer" : undefined}
          onMouseMove={handleMove}
          onMouseLeave={() => setHover(null)}
          onClick={() => {
            if (hovered && onSelect) onSelect(hovered.task);
          }}
        >
          {[0, 5000, 10000].map((bps) => (
            <g key={bps}>
              <line
                x1={TREND_PAD.left}
                x2={width - padRight}
                y1={yAt(bps)}
                y2={yAt(bps)}
                stroke={bps === 0 ? "var(--lg-axis)" : "var(--lg-grid)"}
                strokeWidth="1"
              />
              <text
                x={TREND_PAD.left - 8}
                y={yAt(bps) + 4}
                textAnchor="end"
                fontSize="11"
                fill="var(--lg-fg-4)"
                style={{ fontVariantNumeric: "tabular-nums" }}
              >
                {bps / 100}%
              </text>
            </g>
          ))}

          {thresholdBps !== 5000 && thresholdBps > 0 && thresholdBps < 10000 && (
            <line
              x1={TREND_PAD.left}
              x2={width - padRight}
              y1={thresholdY}
              y2={thresholdY}
              stroke="var(--lg-fg-4)"
              strokeWidth="1"
              strokeDasharray="3 4"
            />
          )}
          <text x={width - padRight + 6} y={thresholdY + 4} fontSize="11" fill="var(--lg-fg-3)">
            {m.common.trend.threshold}
          </text>

          <path d={area} fill="var(--lg-s1)" opacity="0.1" />
          <polyline
            points={line}
            fill="none"
            stroke="var(--lg-s1)"
            strokeWidth="2"
            strokeLinejoin="round"
            strokeLinecap="round"
          />

          {points.map((p, i) =>
            p.hitBps < thresholdBps ? (
              <circle
                key={p.task.task_id}
                cx={xAt(i)}
                cy={yAt(p.hitBps)}
                r="4"
                fill="var(--lg-err)"
                stroke="var(--lg-surface)"
                strokeWidth="2"
              />
            ) : null,
          )}

          {last && hover === null && (
            <>
              <circle
                cx={xAt(points.length - 1)}
                cy={yAt(last.hitBps)}
                r="4"
                fill="var(--lg-s1)"
                stroke="var(--lg-surface)"
                strokeWidth="2"
              />
              <text
                x={xAt(points.length - 1) + 8}
                y={yAt(last.hitBps) - 6}
                fontSize="11"
                fontWeight="600"
                fill="var(--lg-fg)"
              >
                {formatBpsPct(last.hitBps, 0)}
              </text>
            </>
          )}

          {hovered && hover !== null && (
            <>
              <line
                x1={xAt(hover)}
                x2={xAt(hover)}
                y1={TREND_PAD.top}
                y2={baseY}
                stroke="var(--lg-axis)"
                strokeWidth="1"
              />
              <circle
                cx={xAt(hover)}
                cy={yAt(hovered.hitBps)}
                r="5"
                fill={hovered.hitBps < thresholdBps ? "var(--lg-err)" : "var(--lg-s1)"}
                stroke="var(--lg-surface)"
                strokeWidth="2"
              />
            </>
          )}

          <text x={TREND_PAD.left} y={TREND_HEIGHT - 6} fontSize="11" fill="var(--lg-fg-4)">
            {formatClock(points[0]?.task.started_at_unix_ms ?? 0)}
          </text>
          <text
            x={width - padRight}
            y={TREND_HEIGHT - 6}
            textAnchor="end"
            fontSize="11"
            fill="var(--lg-fg-4)"
          >
            {formatClock(last?.task.started_at_unix_ms ?? 0)}
          </text>
        </svg>
      )}

      {hovered && (
        <div className="lg-chart-tip" style={{ left: tipLeft, top: 0 }}>
          <div className="flex items-center justify-between gap-3">
            <span className="text-[15px] font-semibold text-fg">{formatBpsPct(hovered.hitBps)}</span>
            <span className="lg-num text-fg-3">{formatClock(hovered.task.started_at_unix_ms)}</span>
          </div>
          <div className="lg-num mt-1 truncate text-fg-2">{hovered.task.resolved_model}</div>
          <div className="lg-num mt-0.5 text-fg-3">
            {m.common.trend.cacheRead} {formatCompact(hovered.task.usage?.cachedReadTokens ?? 0)} /{" "}
            {formatCompact(taskPromptTokens(hovered.task))} · TTFT {formatDurationMs(hovered.task.ttft_ms)}
          </div>
        </div>
      )}

      {lowCount > 0 && (
        <div className="mt-1 flex items-center gap-1.5 text-[12px] text-fg-3">
          <span className="lg-dot" data-tone="err" />
          {m.common.trend.lowTurns(lowCount)}
        </div>
      )}
    </div>
  );
});
