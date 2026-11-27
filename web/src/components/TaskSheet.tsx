import * as React from "react";
import { ArrowRight, CircleAlert, LoaderCircle } from "lucide-react";
import { ProseBlock } from "@/components/CodeBlock";
import { SegmentedBar } from "@/components/charts";
import { ToolGlyph } from "@/components/ToolExchange";
import { Sheet } from "@/components/ui/dialog";
import { Figure, HitRate, KV, Note, Pill, Tag, type Tone } from "@/components/ui";
import {
  formatDurationMs,
  formatNumber,
  formatSmartTime,
  protocolPath,
  taskHitBps,
  taskPromptTokens,
} from "@/lib/format";
import { classifyTool, countBy } from "@/lib/toolcalls";
import type { TaskPhase, TaskSnapshot } from "@/types/telemetry";

const PHASE: Record<TaskPhase, { label: string; tone: Tone; live: boolean }> = {
  initializing_session: { label: "初始化会话", tone: "accent", live: true },
  thinking: { label: "思考中", tone: "accent", live: true },
  searching_web: { label: "联网搜索", tone: "accent", live: true },
  fetching_web: { label: "抓取网页", tone: "accent", live: true },
  streaming_text: { label: "输出中", tone: "accent", live: true },
  calling_client_tool: { label: "调用工具", tone: "accent", live: true },
  completed: { label: "完成", tone: "ok", live: false },
  failed: { label: "失败", tone: "err", live: false },
  cancelled: { label: "已取消", tone: "neutral", live: false },
};

export function PhasePill({ phase }: { phase: TaskPhase }): React.JSX.Element {
  const meta = PHASE[phase];
  return (
    <Pill tone={meta.tone} icon={meta.live ? <LoaderCircle className="lg-spin" /> : undefined}>
      {meta.label}
    </Pill>
  );
}

function activityItems(task: TaskSnapshot): Array<[string, number, string]> {
  const items: Array<[string, number, string]> = [
    ["工具", task.client_tool_calls.length, "客户端工具调用次数"],
    ["搜索", task.native_web_searches, "原生联网搜索"],
    ["抓取", task.native_web_fetches, "原生网页抓取"],
    ["视觉卸载", task.vision_offloaded_images ?? 0, "由视觉子会话旁路处理的图像"],
    ["0-RTT", task.zero_rtt_mcp_ops ?? 0, "网关本地闭环、没有客户端往返的操作"],
    ["Spill", task.spilled_tool_results ?? 0, "超过内联上限而分片落盘的工具输出"],
  ];
  return items.filter(([, count]) => count > 0);
}

/** Side activity of a turn as quiet tags: only what actually happened shows up. */
export function TaskActivity({ task }: { task: TaskSnapshot }): React.JSX.Element | null {
  const shown = activityItems(task);
  if (shown.length === 0) return null;
  return (
    <span className="inline-flex flex-wrap gap-1">
      {shown.map(([label, count, title]) => (
        <Tag key={label} title={title}>
          {label} {count}
        </Tag>
      ))}
    </span>
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
  onClose,
  onOpenToolCalls,
}: {
  task: TaskSnapshot | null;
  thresholdBps: number;
  onClose: () => void;
  /** Jump to the tool-call stream filtered to this turn. */
  onOpenToolCalls?: (taskId: string) => void;
}): React.JSX.Element {
  const usage = task?.usage ?? null;
  const toolCounts = task ? countBy(task.client_tool_calls, (n) => n) : [];
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

          <div className="grid grid-cols-2 gap-4 sm:grid-cols-4">
            <Figure label="总耗时" value={formatDurationMs(task.elapsed_ms)} />
            <Figure label="首字延迟" value={formatDurationMs(task.ttft_ms)} />
            <Figure
              label="缓存命中率"
              value={<HitRate bps={taskHitBps(task)} thresholdBps={thresholdBps} digits={2} />}
            />
            <Figure label="输出" value={usage ? formatNumber(usage.outputTokens) : "—"} sub="tokens" />
          </div>

          {usage && taskPromptTokens(task) > 0 && (
            <section>
              <h3 className="lg-section-title">
                输入 Token 构成 · 共 {formatNumber(taskPromptTokens(task))}
              </h3>
              <SegmentedBar
                ariaLabel="本轮输入 Token 构成"
                format={formatNumber}
                parts={[
                  { key: "read", label: "缓存读", value: usage.cachedReadTokens, color: "var(--lg-s1)" },
                  {
                    key: "write",
                    label: "缓存写",
                    value: usage.cachedWriteTokens,
                    color: "var(--lg-s4)",
                    ink: "dark",
                  },
                  { key: "input", label: "未命中输入", value: usage.inputTokens, color: "var(--lg-s-neutral)" },
                ]}
              />
              {usage.thoughtTokens > 0 && (
                <p className="lg-hint m-0 mt-2">
                  另有思考 <span className="lg-num text-fg-2">{formatNumber(usage.thoughtTokens)}</span> tokens。
                </p>
              )}
            </section>
          )}

          <KV
            items={[
              ["模型", <ModelRoute key="m" task={task} />],
              [
                "会话",
                task.session_id ? (
                  <>
                    {task.session_id}{" "}
                    <span className="text-fg-3">· {task.session_reused ? "复用热会话" : "新建"}</span>
                  </>
                ) : (
                  "—"
                ),
              ],
              ["停止原因", task.stop_reason ?? "—"],
              ["思考 / 输出", `${formatNumber(task.thought_chars)} / ${formatNumber(task.output_chars)} 字符`],
              ...(task.auxiliary_task
                ? ([["辅助请求", task.auxiliary_task]] as Array<readonly [string, React.ReactNode]>)
                : []),
            ]}
          />

          {activityItems(task).length > 0 && (
            <section>
              <div className="mb-2 flex items-center justify-between gap-2">
                <h3 className="lg-section-title m-0">本轮活动</h3>
                {onOpenToolCalls && task.client_tool_calls.length > 0 && (
                  <button type="button" className="lg-link" onClick={() => onOpenToolCalls(task.task_id)}>
                    查看每次调用的输入输出
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
                <TaskActivity task={task} />
              </div>
            </section>
          )}

          {task.prompt_preview && <ProseBlock text={task.prompt_preview} title="输入摘要" maxHeight={220} />}
          {task.output_preview && <ProseBlock text={task.output_preview} title="输出摘要" maxHeight={220} />}
        </>
      )}
    </Sheet>
  );
}
