import * as React from "react";
import { ArrowRight, CircleAlert, LoaderCircle } from "lucide-react";
import { ProseBlock } from "@/components/CodeBlock";
import { SegmentedBar } from "@/components/charts";
import { ToolGlyph } from "@/components/ToolExchange";
import { Sheet } from "@/components/ui/dialog";
import { Figure, HitRate, KV, Latency, Note, Pill, Tag, type Tone, Usd } from "@/components/ui";
import {
  formatNumber,
  formatSmartTime,
  protocolPath,
  taskHitBps,
  taskPromptTokens,
} from "@/lib/format";
import { type Messages, useI18n } from "@/lib/i18n";
import { type ToolOutcomeCounts, classifyTool, countBy } from "@/lib/toolcalls";
import type { TaskPhase, TaskSnapshot } from "@/types/telemetry";

const PHASE_TONE: Record<TaskPhase, { tone: Tone; live: boolean }> = {
  initializing_session: { tone: "accent", live: true },
  thinking: { tone: "accent", live: true },
  searching_web: { tone: "accent", live: true },
  fetching_web: { tone: "accent", live: true },
  streaming_text: { tone: "accent", live: true },
  calling_client_tool: { tone: "accent", live: true },
  completed: { tone: "ok", live: false },
  failed: { tone: "err", live: false },
  cancelled: { tone: "warn", live: false },
};

export function PhasePill({ phase }: { phase: TaskPhase }): React.JSX.Element {
  const { m } = useI18n();
  const meta = PHASE_TONE[phase];
  return (
    <Pill tone={meta.tone} icon={meta.live ? <LoaderCircle className="lg-spin" /> : undefined}>
      {m.overview.phase[phase]}
    </Pill>
  );
}

/** Row tone for a request: red when it failed, amber when it was cancelled or a tool failed. */
export function taskTone(task: TaskSnapshot, tools?: ToolOutcomeCounts): "err" | "warn" | undefined {
  if (task.phase === "failed") return "err";
  if (task.phase === "cancelled") return "warn";
  if (tools && tools.failed + tools.rejected > 0) return "warn";
  return undefined;
}

function activityItems(
  task: TaskSnapshot,
  m: Messages,
): Array<{ key: string; label: string; hint: string; count: number }> {
  const a = m.overview.activity;
  const items = [
    { key: "tools", ...a.tools, count: task.client_tool_calls.length },
    { key: "searches", ...a.searches, count: task.native_web_searches },
    { key: "fetches", ...a.fetches, count: task.native_web_fetches },
    { key: "vision", ...a.vision, count: task.vision_offloaded_images ?? 0 },
    { key: "zeroRtt", ...a.zeroRtt, count: task.zero_rtt_mcp_ops ?? 0 },
    { key: "spills", ...a.spills, count: task.spilled_tool_results ?? 0 },
  ];
  return items.filter((item) => item.count > 0);
}

/** Side activity of a turn as quiet tags: only what actually happened shows up. */
export function TaskActivity({ task }: { task: TaskSnapshot }): React.JSX.Element | null {
  const { m } = useI18n();
  const shown = activityItems(task, m);
  if (shown.length === 0) return null;
  return (
    <span className="inline-flex flex-wrap gap-1">
      {shown.map((item) => (
        <Tag key={item.key} title={item.hint}>
          {item.label} {item.count}
        </Tag>
      ))}
    </span>
  );
}

/** Loud pills for tool calls that failed, were rejected, or are still out with the client. */
export function ToolOutcome({ counts }: { counts: ToolOutcomeCounts | undefined }): React.JSX.Element | null {
  const { m } = useI18n();
  if (!counts) return null;
  const o = m.overview.outcome;
  return (
    <>
      {counts.failed > 0 && (
        <Pill tone="err" title={o.failedHint}>
          {o.failed(counts.failed)}
        </Pill>
      )}
      {counts.rejected > 0 && (
        <Pill tone="warn" title={o.rejectedHint}>
          {o.rejected(counts.rejected)}
        </Pill>
      )}
      {counts.waiting > 0 && (
        <Pill tone="accent" title={o.waitingHint}>
          {o.waiting(counts.waiting)}
        </Pill>
      )}
    </>
  );
}

export function ModelRoute({ task }: { task: TaskSnapshot }): React.JSX.Element {
  if (task.requested_model === task.resolved_model) {
    return <span className="lg-num">{task.resolved_model}</span>;
  }
  return (
    <span className="lg-num inline-flex flex-wrap items-center gap-1">
      <span className="text-fg-3">{task.requested_model}</span>
      <ArrowRight className="h-3 w-3 text-fg-4" />
      <span>{task.resolved_model}</span>
    </span>
  );
}

export function TaskSheet({
  task,
  thresholdBps,
  toolOutcome,
  onClose,
  onOpenToolCalls,
}: {
  task: TaskSnapshot | null;
  thresholdBps: number;
  /** How this turn's recorded tool calls ended, when the gateway records them. */
  toolOutcome?: ToolOutcomeCounts;
  onClose: () => void;
  /** Jump to the tool-call stream filtered to this turn. */
  onOpenToolCalls?: (taskId: string) => void;
}): React.JSX.Element {
  const { m } = useI18n();
  const t = m.overview.sheet;
  const usage = task?.usage ?? null;
  const toolCounts = task ? countBy(task.client_tool_calls, (n) => n) : [];
  const live = task !== null && task.finished_at_unix_ms === null;
  return (
    <Sheet
      open={task !== null}
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      title={
        task && (
          <>
            <span className="lg-num">{task.task_id}</span>
            <PhasePill phase={task.phase} />
          </>
        )
      }
      description={
        task && (
          <span className="lg-num">
            {protocolPath(task.protocol)} · {task.stream ? "SSE" : "JSON"} ·{" "}
            {formatSmartTime(task.started_at_unix_ms)}
          </span>
        )
      }
    >
      {task && (
        <>
          {task.error_message && (
            <Note tone="err" icon={<CircleAlert />}>
              {task.error_message}
            </Note>
          )}

          <div className="grid grid-cols-2 gap-4 sm:grid-cols-5">
            <Figure label={t.elapsed} value={<Latency ms={task.elapsed_ms} kind="elapsed" pending={live} />} />
            <Figure label={t.ttft} value={<Latency ms={task.ttft_ms} kind="ttft" />} />
            <Figure
              label={t.hitRate}
              value={<HitRate bps={taskHitBps(task)} thresholdBps={thresholdBps} digits={2} />}
            />
            <Figure label={t.output} value={usage ? formatNumber(usage.outputTokens) : "—"} sub="tokens" />
            <Figure label={t.cost} value={<Usd value={task.cost_usd} unpricedHint={usage !== null} />} />
          </div>

          {usage && taskPromptTokens(task) > 0 && (
            <section>
              <h3 className="lg-section-title">{t.inputMix(formatNumber(taskPromptTokens(task)))}</h3>
              <SegmentedBar
                ariaLabel={t.inputMixAria}
                format={formatNumber}
                parts={[
                  { key: "read", label: t.cacheRead, value: usage.cachedReadTokens, color: "var(--lg-s1)" },
                  {
                    key: "write",
                    label: t.cacheWrite,
                    value: usage.cachedWriteTokens,
                    color: "var(--lg-s4)",
                    ink: "dark",
                  },
                  { key: "input", label: t.uncached, value: usage.inputTokens, color: "var(--lg-s-neutral)" },
                ]}
              />
              {usage.thoughtTokens > 0 && (
                <p className="lg-hint m-0 mt-2">
                  {t.thinking(<span className="lg-num text-fg-2">{formatNumber(usage.thoughtTokens)}</span>)}
                </p>
              )}
            </section>
          )}

          <KV
            items={[
              [t.model, <ModelRoute key="m" task={task} />],
              [
                t.session,
                task.session_id ? (
                  <>
                    {task.session_id}{" "}
                    <span className="text-fg-3">· {task.session_reused ? t.warm : t.created}</span>
                  </>
                ) : (
                  "—"
                ),
              ],
              [t.stopReason, task.stop_reason ?? "—"],
              [t.thoughtOutput, t.chars(formatNumber(task.thought_chars), formatNumber(task.output_chars))],
              ...(task.auxiliary_task
                ? ([[t.auxiliary, task.auxiliary_task]] as Array<readonly [string, React.ReactNode]>)
                : []),
            ]}
          />

          {activityItems(task, m).length > 0 && (
            <section>
              <div className="mb-2 flex items-center justify-between gap-2">
                <h3 className="lg-section-title m-0">{t.activity}</h3>
                {onOpenToolCalls && task.client_tool_calls.length > 0 && (
                  <button type="button" className="lg-link" onClick={() => onOpenToolCalls(task.task_id)}>
                    {t.openCalls}
                    <ArrowRight />
                  </button>
                )}
              </div>
              <div className="flex flex-col gap-2">
                {toolCounts.length > 0 && (
                  <div className="flex flex-wrap gap-1.5">
                    {toolCounts.map(([name, count]) => (
                      <span key={name} className="lg-chip" style={{ cursor: "default" }}>
                        <ToolGlyph kind={classifyTool(name)} size="sm" />
                        <span className="lg-num">{name}</span>
                        <span className="lg-seg-count">×{count}</span>
                      </span>
                    ))}
                  </div>
                )}
                <span className="inline-flex flex-wrap items-center gap-1.5">
                  <ToolOutcome counts={toolOutcome} />
                  <TaskActivity task={task} />
                </span>
              </div>
            </section>
          )}

          {task.prompt_preview && <ProseBlock text={task.prompt_preview} title={t.prompt} maxHeight={220} />}
          {task.output_preview && (
            <ProseBlock text={task.output_preview} title={t.outputPreview} maxHeight={220} />
          )}
        </>
      )}
    </Sheet>
  );
}
