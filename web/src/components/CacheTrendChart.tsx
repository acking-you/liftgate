import * as React from "react";
import { useMemo, useState } from "react";
import type { TaskSnapshot } from "@/types/telemetry";

interface TrendPoint {
  taskId: string;
  model: string;
  startedAtMs: number;
  hitBps: number;
  promptTokens: number;
  cachedTokens: number;
  ttftMs: number | null;
  reused: boolean;
}

function computeTaskHitBps(task: TaskSnapshot): number | null {
  if (typeof task.cache_hit_rate_bps === "number") {
    return task.cache_hit_rate_bps;
  }
  const u = task.usage;
  if (!u) return null;
  const totalPrompt = u.inputTokens + u.cachedReadTokens + u.cachedWriteTokens;
  if (totalPrompt <= 0) return null;
  return Math.round((Math.min(u.cachedReadTokens, totalPrompt) * 10000) / totalPrompt);
}

function formatClock(ms: number): string {
  if (ms <= 0) return "-";
  return new Date(ms).toLocaleTimeString("zh-CN", { hour12: false });
}

export function CacheTrendChart({
  recentTasks,
  thresholdBps,
  onSelectTask,
}: {
  recentTasks: TaskSnapshot[];
  thresholdBps: number;
  onSelectTask?: (task: TaskSnapshot) => void;
}): React.JSX.Element {
  const [hoveredIdx, setHoveredIdx] = useState<number | null>(null);

  const { points, taskByIndex } = useMemo(() => {
    const valid = recentTasks
      .filter((t) => t.phase === "completed" && t.usage !== null)
      .slice(0, 36)
      .reverse();

    const mapped: TrendPoint[] = [];
    const tasks: TaskSnapshot[] = [];
    for (const t of valid) {
      const hitBps = computeTaskHitBps(t);
      if (hitBps === null || !t.usage) continue;
      const promptTokens =
        t.usage.inputTokens + t.usage.cachedReadTokens + t.usage.cachedWriteTokens;
      mapped.push({
        taskId: t.task_id,
        model: t.resolved_model,
        startedAtMs: t.started_at_unix_ms,
        hitBps,
        promptTokens,
        cachedTokens: t.usage.cachedReadTokens,
        ttftMs: t.ttft_ms ?? null,
        reused: t.session_reused,
      });
      tasks.push(t);
    }
    return { points: mapped, taskByIndex: tasks };
  }, [recentTasks]);

  if (points.length === 0) {
    return (
      <div className="sf-empty py-8">
        <p className="sf-empty__title">尚无已完成轮次的缓存曲线样本</p>
        <p className="sf-empty__hint">
          发起请求或点击右上角「在线拨测」后，将在此实时绘制每轮 Prompt 缓存命中率曲线与告警阈值线。
        </p>
      </div>
    );
  }

  const width = 760;
  const height = 216;
  const padLeft = 46;
  const padRight = 18;
  const padTop = 16;
  const padBottom = 30;
  const plotW = width - padLeft - padRight;
  const plotH = height - padTop - padBottom;

  const thresholdRatio = Math.min(1, Math.max(0, thresholdBps / 10000));
  const thresholdY = padTop + (1 - thresholdRatio) * plotH;

  const coords = points.map((pt, idx) => {
    const x =
      points.length > 1
        ? padLeft + (idx / (points.length - 1)) * plotW
        : padLeft + plotW / 2;
    const ratio = Math.min(1, Math.max(0, pt.hitBps / 10000));
    const y = padTop + (1 - ratio) * plotH;
    return { x, y, pt };
  });

  const polyline = coords.map((c) => `${c.x.toFixed(1)},${c.y.toFixed(1)}`).join(" ");
  const firstCoord = coords[0];
  const lastCoord = coords[coords.length - 1];
  const areaPath =
    coords.length > 1 && firstCoord && lastCoord
      ? `M ${firstCoord.x.toFixed(1)} ${(padTop + plotH).toFixed(1)} L ` +
        coords.map((c) => `${c.x.toFixed(1)} ${c.y.toFixed(1)}`).join(" L ") +
        ` L ${lastCoord.x.toFixed(1)} ${(padTop + plotH).toFixed(1)} Z`
      : null;

  const active = hoveredIdx !== null ? coords[hoveredIdx] ?? null : lastCoord ?? null;

  return (
    <div className="space-y-2.5">
      <div className="flex flex-wrap items-center justify-between gap-2 text-xs text-[var(--muted)]">
        <div className="flex flex-wrap items-center gap-3">
          <span className="inline-flex items-center gap-1.5">
            <span className="inline-block h-2 w-2 rounded-full bg-[var(--ok)]" />
            达标轮次 (&ge;{(thresholdBps / 100).toFixed(0)}%)
          </span>
          <span className="inline-flex items-center gap-1.5">
            <span className="inline-block h-2 w-2 rounded-full bg-[var(--err)]" />
            低命中记录 (&lt;{(thresholdBps / 100).toFixed(0)}%)
          </span>
          <span className="inline-flex items-center gap-1.5">
            <span className="inline-block h-0 w-3.5 border-t border-dashed border-[var(--warn)]" />
            持久化阈值线 ({(thresholdBps / 100).toFixed(0)}%)
          </span>
        </div>

        {active && (
          <div className="font-mono text-[11.5px] text-[var(--text-2)] tabular-nums">
            <span className="text-[var(--faint)]">{formatClock(active.pt.startedAtMs)} · </span>
            <span>{active.pt.model}</span>
            <span className="mx-1.5 text-[var(--border-strong)]">|</span>
            <strong
              className={
                active.pt.hitBps < thresholdBps
                  ? "text-[var(--err)]"
                  : "text-[var(--ok)]"
              }
            >
              {(active.pt.hitBps / 100).toFixed(1)}%
            </strong>
            <span className="text-[var(--faint)]">
              {" "}
              (缓存↓ {active.pt.cachedTokens.toLocaleString("en-US")} /{" "}
              {active.pt.promptTokens.toLocaleString("en-US")})
            </span>
            {active.pt.ttftMs !== null && (
              <span className="text-[var(--faint)]"> · TTFT {active.pt.ttftMs}ms</span>
            )}
          </div>
        )}
      </div>

      <div className="overflow-x-auto rounded-[8px] border border-[var(--border)] bg-[var(--surface-alt)] p-2.5">
        <svg
          viewBox={`0 0 ${width} ${height}`}
          className="block h-auto w-full min-w-[520px]"
          role="img"
          aria-label="近期请求 Prompt 缓存命中率趋势图"
          onMouseLeave={() => setHoveredIdx(null)}
        >
          <defs>
            <linearGradient id="sf-cache-area" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0%" stopColor="var(--primary)" stopOpacity="0.18" />
              <stop offset="100%" stopColor="var(--primary)" stopOpacity="0.0" />
            </linearGradient>
          </defs>

          {[0, 0.5, 1].map((ratio) => {
            const y = padTop + (1 - ratio) * plotH;
            const pct = Math.round(ratio * 100);
            return (
              <g key={pct}>
                <line
                  x1={padLeft}
                  y1={y}
                  x2={width - padRight}
                  y2={y}
                  stroke="var(--border)"
                  strokeWidth="1"
                />
                <text
                  x={padLeft - 8}
                  y={y + 3.5}
                  textAnchor="end"
                  fill="var(--faint)"
                  fontSize="10.5"
                  fontFamily="var(--font-mono)"
                >
                  {pct}%
                </text>
              </g>
            );
          })}

          {/* Threshold reference line */}
          <line
            x1={padLeft}
            y1={thresholdY}
            x2={width - padRight}
            y2={thresholdY}
            stroke="var(--warn)"
            strokeWidth="1.2"
            strokeDasharray="4 4"
          />

          {areaPath && <path d={areaPath} fill="url(#sf-cache-area)" />}

          {coords.length > 1 && (
            <polyline
              points={polyline}
              fill="none"
              stroke="var(--primary)"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            />
          )}

          {coords.map((c, idx) => {
            const isLow = c.pt.hitBps < thresholdBps;
            const isHovered = hoveredIdx === idx;
            return (
              <g
                key={`${c.pt.taskId}-${idx}`}
                className="cursor-pointer"
                onMouseEnter={() => setHoveredIdx(idx)}
                onClick={() => {
                  const target = taskByIndex[idx];
                  if (target && onSelectTask) onSelectTask(target);
                }}
              >
                <circle cx={c.x} cy={c.y} r="11" fill="transparent" />
                <circle
                  cx={c.x}
                  cy={c.y}
                  r={isHovered ? 5 : 3.4}
                  fill={isLow ? "var(--err)" : "var(--ok)"}
                  stroke="var(--surface)"
                  strokeWidth="1.5"
                />
              </g>
            );
          })}

          {firstCoord && (
            <>
              <text
                x={padLeft}
                y={height - 8}
                textAnchor="start"
                fill="var(--faint)"
                fontSize="10.5"
                fontFamily="var(--font-mono)"
              >
                {formatClock(firstCoord.pt.startedAtMs)}
              </text>
              {coords.length > 1 && lastCoord && (
                <text
                  x={width - padRight}
                  y={height - 8}
                  textAnchor="end"
                  fill="var(--faint)"
                  fontSize="10.5"
                  fontFamily="var(--font-mono)"
                >
                  {formatClock(lastCoord.pt.startedAtMs)} (最新 · 共{" "}
                  {coords.length} 轮)
                </text>
              )}
            </>
          )}
        </svg>
      </div>
    </div>
  );
}
