import * as React from "react";
import { useMemo, useState } from "react";
import { Activity, ArrowRight, Gauge, Layers, Sparkles, Wrench } from "lucide-react";
import { CacheTrendChart } from "@/components/charts";
import { Skyline } from "@/components/Skyline";
import { ModelRoute, PhasePill, TaskActivity, TaskSheet } from "@/components/TaskSheet";
import {
  Card,
  EmptyState,
  Figure,
  HitRate,
  Meter,
  Pager,
  Pill,
  Segmented,
  Stat,
  StatGrid,
  Tag,
} from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import {
  formatBpsPct,
  formatBytes,
  formatClock,
  formatCompact,
  formatDurationMs,
  formatNumber,
  formatPct,
  protocolPath,
  shortId,
  taskHitBps,
  taskPromptTokens,
} from "@/lib/format";
import {
  WIRE_BUDGET_BYTES,
  cacheStats,
  elasticStats,
  sessionReuseRatio,
} from "@/lib/stats";
import type { ElasticIoTelemetry, SessionSummary, TaskProtocol, TaskSnapshot } from "@/types/telemetry";
import type { ViewProps } from "@/views/types";

const TASK_PAGE_SIZE = 15;

function ElasticCard({
  io,
  sessions,
}: {
  io: ElasticIoTelemetry;
  sessions: SessionSummary[];
}): React.JSX.Element {
  const s = elasticStats(io);
  const peakWire = sessions.reduce((m, sess) => Math.max(m, sess.mcp?.wire_bytes ?? 0), 0);
  const wireRatio = peakWire / WIRE_BUDGET_BYTES;
  return (
    <Card title="弹性 I/O">
      <div className="grid grid-cols-2 gap-x-8 gap-y-5 xl:grid-cols-4">
        <Figure
          label="编排器 / Shell 聚合调用"
          value={formatPct(s.orchestrationRatio)}
          sub={`${formatNumber(s.orchestrator)} / ${formatNumber(s.forwarded)} 次调用`}
        />
        <Figure
          label="输出未超内联上限"
          value={formatPct(s.inlineRatio)}
          sub={`${formatNumber(s.resultsSpilled)} 次 Spill`}
        />
        <Figure
          label="0-RTT 本地闭环"
          value={formatNumber(s.zeroRttTotal)}
          sub={`检索 ${s.zeroRttSearches} · 分片读 ${s.zeroRttSpillReads} · 预检 ${s.zeroRttPreflight}`}
        />
        <div className="min-w-0">
          <div className="text-[12px] text-fg-3">Wire 工具体积峰值</div>
          <div className="mt-0.5 text-[15px] font-semibold text-fg">
            {formatBytes(peakWire)}
            <span className="ml-1 text-[12px] font-normal text-fg-4">/ {formatBytes(WIRE_BUDGET_BYTES)}</span>
          </div>
          <div className="mt-2">
            <Meter
              ratio={wireRatio}
              tone={wireRatio > 0.9 ? "warn" : undefined}
              label="Wire 工具体积占预算比例"
            />
          </div>
        </div>
      </div>
    </Card>
  );
}

function InFlight({ task }: { task: TaskSnapshot }): React.JSX.Element {
  return (
    <div className="lg-activity">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <span className="flex flex-wrap items-center gap-1.5">
          <PhasePill phase={task.phase} />
          <Tag>{protocolPath(task.protocol)}</Tag>
          <span className="text-[12.5px] text-fg-2">
            <ModelRoute task={task} />
          </span>
        </span>
        <span className="lg-num text-[12px] text-fg-3">
          {formatDurationMs(task.elapsed_ms)}
          {task.ttft_ms != null ? ` · TTFT ${formatDurationMs(task.ttft_ms)}` : ""}
        </span>
      </div>
      {task.prompt_preview && (
        <p className="m-0 line-clamp-2 text-[12.5px] text-fg-3">{task.prompt_preview}</p>
      )}
      {task.output_preview && (
        <p className="m-0 line-clamp-2 border-l-2 border-accent-solid pl-2.5 text-[12.5px] text-fg">
          {task.output_preview}
        </p>
      )}
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[11.5px] text-fg-3">
        <span className="lg-num">{task.task_id}</span>
        {task.session_id && (
          <span className="lg-num">
            {task.session_reused ? "复用" : "新建"} {shortId(task.session_id)}
          </span>
        )}
        <span className="lg-num">
          思考 {formatNumber(task.thought_chars)} · 输出 {formatNumber(task.output_chars)} 字符
        </span>
        <TaskActivity task={task} />
      </div>
    </div>
  );
}

export function OverviewView({ navigate }: ViewProps): React.JSX.Element {
  const { snapshot, lowCacheRecords, toolCalls } = useGateway();
  const [protocol, setProtocol] = useState<"all" | TaskProtocol>("all");
  const [selectedTask, setSelectedTask] = useState<TaskSnapshot | null>(null);
  const [taskPage, setTaskPage] = useState(1);

  const metrics = snapshot?.metrics;
  const pool = snapshot?.session_pool;
  const cacheSummary = snapshot?.cache_telemetry;
  const toolSummary = snapshot?.tool_call_telemetry;
  const activeTasks = snapshot?.active_tasks ?? [];
  const recentTasks = useMemo(() => snapshot?.recent_tasks ?? [], [snapshot?.recent_tasks]);
  const thresholdBps = cacheSummary?.low_cache_threshold_bps ?? 5000;
  const stats = useMemo(() => cacheStats(cacheSummary, metrics), [cacheSummary, metrics]);
  const reuse = sessionReuseRatio(pool);
  const warmMisses = lowCacheRecords.filter((r) => r.reason === "warm_cache_miss").length;
  const storedLow = cacheSummary?.stored_low_cache_count ?? lowCacheRecords.length;
  const parked = (pool?.idle_cached_sessions ?? 0) + (pool?.suspended_mcp_sessions ?? 0);
  const failed = metrics?.failed_requests ?? 0;

  const pulse = useMemo(
    () =>
      [...toolCalls]
        .sort((a, b) => a.started_at_unix_ms - b.started_at_unix_ms || a.seq - b.seq)
        .slice(-72),
    [toolCalls],
  );

  const filteredTasks = useMemo(
    () => (protocol === "all" ? recentTasks : recentTasks.filter((t) => t.protocol === protocol)),
    [recentTasks, protocol],
  );
  const taskPages = Math.max(1, Math.ceil(filteredTasks.length / TASK_PAGE_SIZE));
  const clampedTaskPage = Math.min(taskPage, taskPages);
  const shownTasks = filteredTasks.slice(
    (clampedTaskPage - 1) * TASK_PAGE_SIZE,
    clampedTaskPage * TASK_PAGE_SIZE,
  );

  return (
    <>
      <StatGrid>
        <Stat
          label="缓存命中率"
          icon={<Gauge />}
          value={formatBpsPct(stats.hitBps)}
          onClick={() => navigate("cache")}
          foot={
            <span className={warmMisses > 0 ? "text-err" : undefined}>
              {storedLow > 0 ? `${formatNumber(storedLow)} 轮低于 ${Math.round(thresholdBps / 100)}%` : "没有低命中轮次"}
            </span>
          }
        >
          <Meter
            ratio={(stats.hitBps ?? 0) / 10000}
            tone={stats.hitBps !== null && stats.hitBps < thresholdBps ? "warn" : undefined}
            label="Prompt 缓存命中率"
          />
        </Stat>
        <Stat
          label="请求"
          icon={<Activity />}
          value={formatNumber(metrics?.total_requests ?? 0)}
          foot={
            failed > 0 ? (
              <span className="text-err">{formatNumber(failed)} 次失败</span>
            ) : (
              <span>{activeTasks.length > 0 ? `${activeTasks.length} 个执行中` : "全部成功"}</span>
            )
          }
        />
        <Stat
          label="会话复用率"
          icon={<Layers />}
          value={formatPct(reuse)}
          onClick={() => navigate("sessions")}
          foot={
            <span>
              驻留 {parked} / {pool?.max_idle_sessions ?? 0}
            </span>
          }
        />
        <Stat
          label="Token"
          icon={<Sparkles />}
          value={formatCompact(stats.promptTokens + (metrics?.total_output_tokens ?? 0))}
          foot={
            <span>
              输入 {formatCompact(stats.promptTokens)} · 输出 {formatCompact(metrics?.total_output_tokens ?? 0)}
            </span>
          }
        />
      </StatGrid>

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
        <Card
          title="逐轮缓存命中率">
          <CacheTrendChart tasks={recentTasks} thresholdBps={thresholdBps} onSelect={setSelectedTask} />
        </Card>

        <Card
          title="工具脉搏"
          actions={
            pulse.length > 0 ? (
              <button type="button" className="lg-link" onClick={() => navigate("tools")}>
                调用流
                <ArrowRight />
              </button>
            ) : undefined
          }
        >
          {pulse.length > 0 ? (
            <Skyline
              records={pulse}
              metric="bytes"
              selectedId={null}
              hoverId={null}
              onSelect={(rec) => navigate("tools", rec.call_id)}
              onHover={() => undefined}
            />
          ) : (
            <EmptyState
              icon={<Wrench />}
              title={toolSummary ? "还没有工具调用" : "当前网关不记录逐次工具调用"}
            />
          )}
        </Card>
      </div>

      {snapshot?.elastic_io && <ElasticCard io={snapshot.elastic_io} sessions={pool?.sessions ?? []} />}

      {activeTasks.length > 0 && (
        <Card title={`执行中 · ${activeTasks.length}`}>
          <div className="grid grid-cols-1 gap-3 lg:grid-cols-2">
            {activeTasks.map((task) => (
              <InFlight key={task.task_id} task={task} />
            ))}
          </div>
        </Card>
      )}

      <Card
        flush
        title="最近的请求"
        actions={
          <Segmented<"all" | TaskProtocol>
            ariaLabel="按协议筛选"
            value={protocol}
            onChange={(next) => {
              setProtocol(next);
              setTaskPage(1);
            }}
            options={[
              { value: "all", label: "全部", count: recentTasks.length },
              { value: "openai_responses", label: "Responses" },
              { value: "anthropic_messages", label: "Messages" },
            ]}
          />
        }
      >
        {shownTasks.length === 0 ? (
          <EmptyState
            icon={<Activity />}
            title="还没有请求"
          />
        ) : (
          <div className="lg-table-wrap">
            <table className="lg-table">
              <thead>
                <tr>
                  <th>时间</th>
                  <th>模型</th>
                  <th className="hide-sm">会话</th>
                  <th className="hide-sm">活动</th>
                  <th className="num">命中率</th>
                  <th className="num hide-sm">输入 / 输出</th>
                  <th className="num">耗时</th>
                </tr>
              </thead>
              <tbody>
                {shownTasks.map((task) => (
                  <tr
                    key={task.task_id}
                    data-clickable="true"
                    tabIndex={0}
                    onClick={() => setSelectedTask(task)}
                    onKeyDown={(ev) => {
                      if (ev.key === "Enter") setSelectedTask(task);
                    }}
                  >
                    <td className="lg-num whitespace-nowrap text-[12px] text-fg-3">
                      {formatClock(task.started_at_unix_ms)}
                    </td>
                    <td className="text-[12px] text-fg">
                      <ModelRoute task={task} />
                    </td>
                    <td className="hide-sm whitespace-nowrap">
                      {task.session_id ? (
                        <span className="inline-flex items-center gap-1.5">
                          <span className="lg-num text-[12px]">{shortId(task.session_id)}</span>
                          {!task.session_reused && <Pill>新建</Pill>}
                        </span>
                      ) : (
                        <span className="text-fg-4">—</span>
                      )}
                    </td>
                    <td className="hide-sm">
                      <span className="inline-flex flex-wrap items-center gap-1.5">
                        {task.phase !== "completed" && <PhasePill phase={task.phase} />}
                        <TaskActivity task={task} />
                      </span>
                    </td>
                    <td className="num whitespace-nowrap">
                      <HitRate bps={taskHitBps(task)} thresholdBps={thresholdBps} />
                    </td>
                    <td className="num hide-sm whitespace-nowrap">
                      {task.usage ? (
                        <>
                          {formatNumber(taskPromptTokens(task))}
                          <span className="text-fg-4"> / </span>
                          {formatNumber(task.usage.outputTokens)}
                        </>
                      ) : (
                        <span className="text-fg-4">—</span>
                      )}
                    </td>
                    <td className="num whitespace-nowrap">{formatDurationMs(task.elapsed_ms)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        <Pager page={clampedTaskPage} pages={taskPages} total={filteredTasks.length} onPage={setTaskPage} />
      </Card>

      <TaskSheet
        task={selectedTask}
        thresholdBps={thresholdBps}
        onClose={() => setSelectedTask(null)}
        onOpenToolCalls={toolSummary ? (taskId) => navigate("tools", taskId) : undefined}
      />
    </>
  );
}
