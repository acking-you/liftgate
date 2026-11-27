import * as React from "react";
import { useEffect, useMemo, useRef, useState } from "react";
import { formatBytes, formatClock, formatDurationMs } from "@/lib/format";
import { harnessMeta } from "@/lib/harness";
import { displayToolName, isFailure, recordKind } from "@/lib/toolcalls";
import type { ToolCallRecord } from "@/types/telemetry";

export type SkylineMetric = "bytes" | "duration";

const UP_H = 84;
const DOWN_H = 40;
const PAD_TOP = 8;
const PAD_BOTTOM = 6;
const HEIGHT = PAD_TOP + UP_H + DOWN_H + PAD_BOTTOM;
const BASE_Y = PAD_TOP + UP_H;
/** Extra air between two turns so a turn reads as one cluster of columns. */
const TURN_GAP = 5;

function fillOf(rec: ToolCallRecord): string {
  if (isFailure(rec)) return "var(--lg-err)";
  if (rec.status === "pending") return "var(--lg-accent)";
  return recordKind(rec) === "orchestrator" ? "var(--lg-fg)" : "var(--lg-s-neutral)";
}

/**
 * One column per tool call, oldest on the left. In byte mode the column rises
 * by what the tool returned and hangs by what the model sent, on one shared
 * scale, so output bloat is visible as a skyline. Columns are the same calls
 * as the list beside it: hover and selection travel both ways.
 */
export function Skyline({
  records,
  metric,
  selectedId,
  hoverId,
  onSelect,
  onHover,
}: {
  /** Chronological, oldest first. */
  records: ToolCallRecord[];
  metric: SkylineMetric;
  selectedId: string | null;
  hoverId: string | null;
  onSelect: (rec: ToolCallRecord) => void;
  onHover: (id: string | null) => void;
}): React.JSX.Element {
  const wrapRef = useRef<HTMLDivElement | null>(null);
  const [width, setWidth] = useState(0);
  const [localHover, setLocalHover] = useState<number | null>(null);

  useEffect(() => {
    const el = wrapRef.current;
    if (!el) return;
    setWidth(el.clientWidth);
    const observer = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (entry) setWidth(Math.round(entry.contentRect.width));
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  const layout = useMemo(() => {
    const n = records.length;
    let turnBreaks = 0;
    for (let i = 1; i < n; i += 1) {
      if (records[i]?.task_id !== records[i - 1]?.task_id) turnBreaks += 1;
    }
    const slot = n > 0 ? Math.max(3, Math.min(18, (width - turnBreaks * TURN_GAP) / n)) : 0;
    const xs: number[] = [];
    let x = 0;
    for (let i = 0; i < n; i += 1) {
      if (i > 0 && records[i]?.task_id !== records[i - 1]?.task_id) x += TURN_GAP;
      xs.push(x);
      x += slot;
    }
    // Right-align: the newest call sits against the right edge.
    const shift = Math.max(0, width - x);
    return { slot, xs: xs.map((v) => v + shift), barW: Math.max(2, slot - 2) };
  }, [records, width]);

  const scale = useMemo(() => {
    let maxOut = 0;
    let maxIn = 0;
    let maxDur = 0;
    for (const rec of records) {
      maxOut = Math.max(maxOut, rec.output_bytes);
      maxIn = Math.max(maxIn, rec.input_bytes);
      maxDur = Math.max(maxDur, rec.duration_ms ?? 0);
    }
    // One scale for both directions keeps input and output comparable.
    const perByte = Math.min(
      maxOut > 0 ? UP_H / maxOut : Number.POSITIVE_INFINITY,
      maxIn > 0 ? DOWN_H / maxIn : Number.POSITIVE_INFINITY,
    );
    return {
      maxOut,
      maxIn,
      maxDur,
      perByte: Number.isFinite(perByte) ? perByte : 0,
      perMs: maxDur > 0 ? (UP_H + DOWN_H) / maxDur : 0,
    };
  }, [records]);

  const hoverIndex =
    localHover ?? (hoverId !== null ? records.findIndex((r) => r.call_id === hoverId) : -1);
  const hovered = hoverIndex >= 0 ? records[hoverIndex] : undefined;
  const hoveredX = hoverIndex >= 0 ? (layout.xs[hoverIndex] ?? 0) : 0;
  const tipLeft = Math.min(Math.max(hoveredX - 100, 0), Math.max(0, width - 232));

  const failures = records.filter(isFailure).length;
  const summary =
    metric === "bytes"
      ? `最近 ${records.length} 次工具调用的输入与输出体积，最大输出 ${formatBytes(scale.maxOut)}，${failures} 次失败`
      : `最近 ${records.length} 次工具调用的耗时，最长 ${formatDurationMs(scale.maxDur)}，${failures} 次失败`;

  return (
    <div className="relative min-w-0">
      <div className="lg-skyline-axis" aria-hidden="true">
        {metric === "bytes" ? (
          <>
            <span>↑ 输出 · 峰值 {formatBytes(scale.maxOut)}</span>
            <span>↓ 输入 · 峰值 {formatBytes(scale.maxIn)}</span>
          </>
        ) : (
          <span>↑ 耗时 · 峰值 {formatDurationMs(scale.maxDur)}</span>
        )}
      </div>
      <div ref={wrapRef} className="min-w-0">
        {width > 0 && (
          <svg
            width={width}
            height={HEIGHT}
            role="img"
            aria-label={summary}
            className="block"
            onMouseLeave={() => {
              setLocalHover(null);
              onHover(null);
            }}
          >
            <line
              x1={0}
              x2={width}
              y1={metric === "bytes" ? BASE_Y : PAD_TOP + UP_H + DOWN_H}
              y2={metric === "bytes" ? BASE_Y : PAD_TOP + UP_H + DOWN_H}
              stroke="var(--lg-axis)"
              strokeWidth="1"
            />
            {records.map((rec, i) => {
              const x = layout.xs[i] ?? 0;
              const fill = fillOf(rec);
              const active = rec.call_id === selectedId;
              const hot = i === hoverIndex;
              const pending = rec.status === "pending";
              let marks: React.ReactNode;
              if (metric === "bytes") {
                const up = rec.output_bytes > 0 ? Math.max(2, rec.output_bytes * scale.perByte) : 0;
                const down = rec.input_bytes > 0 ? Math.max(2, rec.input_bytes * scale.perByte) : 0;
                marks = (
                  <>
                    <rect x={x + 1} y={BASE_Y - up - 1} width={layout.barW} height={up} rx={Math.min(2, layout.barW / 2)} fill={fill} />
                    <rect x={x + 1} y={BASE_Y + 1} width={layout.barW} height={down} rx={Math.min(2, layout.barW / 2)} fill={fill} opacity="0.5" />
                  </>
                );
              } else {
                const floor = PAD_TOP + UP_H + DOWN_H;
                const h = pending
                  ? UP_H + DOWN_H
                  : Math.max(2, (rec.duration_ms ?? 0) * scale.perMs);
                marks = (
                  <rect
                    x={x + 1}
                    y={floor - h - 1}
                    width={layout.barW}
                    height={h}
                    rx={Math.min(2, layout.barW / 2)}
                    fill={fill}
                    opacity={pending ? 0.35 : 1}
                  />
                );
              }
              return (
                <g
                  key={rec.call_id}
                  className={pending ? "lg-skyline-pending" : undefined}
                  style={{ cursor: "pointer" }}
                  onMouseEnter={() => {
                    setLocalHover(i);
                    onHover(rec.call_id);
                  }}
                  onClick={() => onSelect(rec)}
                >
                  {(active || hot) && (
                    <rect
                      x={x - 0.5}
                      y={0}
                      width={layout.slot + 1}
                      height={HEIGHT}
                      rx="3"
                      fill={active ? "var(--lg-accent-soft)" : "var(--lg-hover)"}
                    />
                  )}
                  {marks}
                  {active && (
                    <rect x={x + 1} y={HEIGHT - 3} width={layout.barW} height={3} rx="1.5" fill="var(--lg-accent)" />
                  )}
                  {/* Full-height hit target: the mark itself can be two pixels tall. */}
                  <rect x={x} y={0} width={layout.slot} height={HEIGHT} fill="transparent" />
                </g>
              );
            })}
          </svg>
        )}
      </div>

      {hovered && (
        <div className="lg-chart-tip" style={{ left: tipLeft, top: HEIGHT + 4, width: 232 }}>
          <div className="flex items-center justify-between gap-2">
            <span className="lg-num lg-clip font-medium text-fg">{displayToolName(hovered)}</span>
            <span className="lg-num flex-none text-fg-3">{formatClock(hovered.started_at_unix_ms)}</span>
          </div>
          <div className="mt-1 flex items-center gap-1.5 text-fg-3">
            <span
              className="lg-swatch"
              style={{ background: harnessMeta(hovered.harness_profile).color }}
            />
            {harnessMeta(hovered.harness_profile).label} · 轮 {hovered.turn_index} · 步 {hovered.step_index}
          </div>
          <div className="lg-num mt-1 text-fg-2">
            入 {formatBytes(hovered.input_bytes)} → 出 {formatBytes(hovered.output_bytes)} ·{" "}
            {hovered.status === "pending" ? "进行中" : formatDurationMs(hovered.duration_ms)}
          </div>
          {isFailure(hovered) && (
            <div className="mt-1 text-err">
              {hovered.status === "preflight_rejected" ? "网关预检拒绝" : "调用失败"}
              {hovered.error_kind ? ` · ${hovered.error_kind}` : ""}
            </div>
          )}
        </div>
      )}

      <ul className="m-0 mt-2 flex list-none flex-wrap gap-x-4 gap-y-1 p-0 text-[12px] text-fg-3">
        <li className="flex items-center gap-1.5">
          <span className="lg-swatch" style={{ background: "var(--lg-fg)" }} aria-hidden="true" />
          CodeMode 编排器
        </li>
        <li className="flex items-center gap-1.5">
          <span className="lg-swatch" style={{ background: "var(--lg-s-neutral)" }} aria-hidden="true" />
          其他调用
        </li>
        <li className="flex items-center gap-1.5">
          <span className="lg-swatch" style={{ background: "var(--lg-err)" }} aria-hidden="true" />
          失败 {failures > 0 ? `(${failures})` : ""}
        </li>
      </ul>
    </div>
  );
}
