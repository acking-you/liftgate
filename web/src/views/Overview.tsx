import * as React from "react";
import { useMemo, useState } from "react";
import { Activity, ArrowRight, CircleDollarSign, Gauge, Layers, Sparkles, Wrench } from "lucide-react";
import { CacheTrendChart } from "@/components/charts";
import { Skyline } from "@/components/Skyline";
import {
  ModelRoute,
  PhasePill,
  TaskActivity,
  TaskSheet,
  ToolOutcome,
  taskTone,
} from "@/components/TaskSheet";
import {
  Card,
  EmptyState,
  Figure,
  HitRate,
  Latency,
  Meter,
  Pager,
  Pill,
  Segmented,
  Stat,
  StatGrid,
  Tag,
  Usd,
} from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import {
  formatBpsPct,
  formatBytes,
  formatClock,
  formatCompact,
  formatNumber,
  formatPct,
  formatUsd,
  protocolPath,
  shortId,
  taskHitBps,
  taskPromptTokens,
} from "@/lib/format";
import { useI18n } from "@/lib/i18n";
import {
  WIRE_BUDGET_BYTES,
  cacheStats,
  elasticStats,
  sessionReuseRatio,
} from "@/lib/stats";
import { toolOutcomesByTask } from "@/lib/toolcalls";
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
  const { m } = useI18n();
  const t = m.overview.elastic;
  const s = elasticStats(io);
  const peakWire = sessions.reduce((max, sess) => Math.max(max, sess.mcp?.wire_bytes ?? 0), 0);
  const wireRatio = peakWire / WIRE_BUDGET_BYTES;
  return (
    <Card title={t.title}>
      <div className="grid grid-cols-2 gap-x-8 gap-y-5 xl:grid-cols-4">
        <Figure
          label={t.orchestrated}
          value={formatPct(s.orchestrationRatio)}
          sub={t.callsOf(formatNumber(s.orchestrator), formatNumber(s.forwarded))}
        />
        <Figure label={t.inline} value={formatPct(s.inlineRatio)} sub={t.spills(formatNumber(s.resultsSpilled))} />
        <Figure
          label={t.zeroRtt}
          value={formatNumber(s.zeroRttTotal)}
          sub={t.zeroRttParts(s.zeroRttSearches, s.zeroRttSpillReads, s.zeroRttPreflight)}
        />
        <div className="min-w-0">
          <div className="text-[12px] text-fg-3">{t.wirePeak}</div>
          <div className="mt-0.5 text-[15px] font-semibold text-fg">
            {formatBytes(peakWire)}
            <span className="ml-1 text-[12px] font-normal text-fg-4">/ {formatBytes(WIRE_BUDGET_BYTES)}</span>
          </div>
          <div className="mt-2">
            <Meter ratio={wireRatio} tone={wireRatio > 0.9 ? "warn" : undefined} label={t.wireRatio} />
          </div>
        </div>
      </div>
    </Card>
  );
}

function InFlight({ task }: { task: TaskSnapshot }): React.JSX.Element {
  const { m } = useI18n();
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
        <span className="flex items-center gap-2 text-[12px] text-fg-3">
          <Latency ms={task.elapsed_ms} kind="elapsed" pending />
          {task.ttft_ms != null ? (
            <span className="flex items-center gap-1">
              TTFT <Latency ms={task.ttft_ms} kind="ttft" />
            </span>
          ) : (
            // Still waiting for the first token: judge the wait so far against the TTFT scale.
            <span className="flex items-center gap-1">
              TTFT <Latency ms={task.elapsed_ms} kind="ttft" />…
            </span>
          )}
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
            {task.session_reused ? m.overview.reused : m.overview.created} {shortId(task.session_id)}
          </span>
        )}
        <span className="lg-num">
          {m.overview.inFlightChars(formatNumber(task.thought_chars), formatNumber(task.output_chars))}
        </span>
        <TaskActivity task={task} />
      </div>
    </div>
  );
}

export function OverviewView({ navigate }: ViewProps): React.JSX.Element {
  const { snapshot, lowCacheRecords, toolCalls } = useGateway();
  const { m } = useI18n();
  const t = m.overview;
  const [protocol, setProtocol] = useState<"all" | TaskProtocol>("all");
  const [selectedTask, setSelectedTask] = useState<TaskSnapshot | null>(null);
  const [taskPage, setTaskPage] = useState(1);

  const metrics = snapshot?.metrics;
  const pool = snapshot?.session_pool;
  const cacheSummary = snapshot?.cache_telemetry;
  const toolSummary = snapshot?.tool_call_telemetry;
  const usage = snapshot?.usage;
  const activeTasks = snapshot?.active_tasks ?? [];
  const recentTasks = useMemo(() => snapshot?.recent_tasks ?? [], [snapshot?.recent_tasks]);
  const thresholdBps = cacheSummary?.low_cache_threshold_bps ?? 5000;
  const stats = useMemo(() => cacheStats(cacheSummary, metrics), [cacheSummary, metrics]);
  const reuse = sessionReuseRatio(pool);
  const warmMisses = lowCacheRecords.filter((r) => r.reason === "warm_cache_miss").length;
  const storedLow = cacheSummary?.stored_low_cache_count ?? lowCacheRecords.length;
  const parked = (pool?.idle_cached_sessions ?? 0) + (pool?.suspended_mcp_sessions ?? 0);
  const failed = metrics?.failed_requests ?? 0;
  const outcomes = useMemo(() => toolOutcomesByTask(toolCalls), [toolCalls]);

  const pulse = useMemo(
    () =>
      [...toolCalls]
        .sort((a, b) => a.started_at_unix_ms - b.started_at_unix_ms || a.seq - b.seq)
        .slice(-72),
    [toolCalls],
  );

  const filteredTasks = useMemo(
    () => (protocol === "all" ? recentTasks : recentTasks.filter((task) => task.protocol === protocol)),
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
          label={t.stats.hitRate}
          icon={<Gauge />}
          value={formatBpsPct(stats.hitBps)}
          onClick={() => navigate("cache")}
          foot={
            <span className={warmMisses > 0 ? "text-err" : undefined}>
              {storedLow > 0
                ? t.stats.lowTurns(formatNumber(storedLow), Math.round(thresholdBps / 100))
                : t.stats.noLowTurns}
            </span>
          }
        >
          <Meter
            ratio={(stats.hitBps ?? 0) / 10000}
            tone={stats.hitBps !== null && stats.hitBps < thresholdBps ? "warn" : undefined}
            label={t.stats.hitMeter}
          />
        </Stat>
        <Stat
          label={t.stats.requests}
          icon={<Activity />}
          value={formatNumber(metrics?.total_requests ?? 0)}
          foot={
            failed > 0 ? (
              <span className="text-err">{t.stats.failures(formatNumber(failed))}</span>
            ) : (
              <span>{activeTasks.length > 0 ? t.stats.running(activeTasks.length) : t.stats.allOk}</span>
            )
          }
        />
        <Stat
          label={t.stats.reuse}
          icon={<Layers />}
          value={formatPct(reuse)}
          onClick={() => navigate("sessions")}
          foot={<span>{t.stats.parked(parked, pool?.max_idle_sessions ?? 0)}</span>}
        />
        {usage ? (
          <Stat
            label={t.stats.cost}
            icon={<CircleDollarSign />}
            value={formatUsd(usage.cost_usd)}
            onClick={() => navigate("usage")}
            foot={
              usage.unpriced_calls > 0 ? (
                <span className="text-warn">{t.stats.unpricedFoot(formatNumber(usage.unpriced_calls))}</span>
              ) : (
                <span>
                  {t.stats.costFoot(
                    formatCompact(usage.tokens.input + usage.tokens.cached_input + usage.tokens.cache_write + usage.tokens.output),
                    formatNumber(usage.calls),
                  )}
                </span>
              )
            }
          />
        ) : (
          <Stat
            label={t.stats.tokens}
            icon={<Sparkles />}
            value={formatCompact(stats.promptTokens + (metrics?.total_output_tokens ?? 0))}
            foot={
              <span>
                {t.stats.tokensFoot(
                  formatCompact(stats.promptTokens),
                  formatCompact(metrics?.total_output_tokens ?? 0),
                )}
              </span>
            }
          />
        )}
      </StatGrid>

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
        <Card title={t.trendTitle}>
          <CacheTrendChart tasks={recentTasks} thresholdBps={thresholdBps} onSelect={setSelectedTask} />
        </Card>

        <Card
          title={t.pulseTitle}
          actions={
            pulse.length > 0 ? (
              <button type="button" className="lg-link" onClick={() => navigate("tools")}>
                {t.callStream}
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
            <EmptyState icon={<Wrench />} title={toolSummary ? t.noToolCalls : t.toolCallsUnsupported} />
          )}
        </Card>
      </div>

      {snapshot?.elastic_io && <ElasticCard io={snapshot.elastic_io} sessions={pool?.sessions ?? []} />}

      {activeTasks.length > 0 && (
        <Card title={t.inFlight(activeTasks.length)}>
          <div className="grid grid-cols-1 gap-3 lg:grid-cols-2">
            {activeTasks.map((task) => (
              <InFlight key={task.task_id} task={task} />
            ))}
          </div>
        </Card>
      )}

      <Card
        flush
        title={t.recent}
        actions={
          <Segmented<"all" | TaskProtocol>
            ariaLabel={t.protocolFilter}
            value={protocol}
            onChange={(next) => {
              setProtocol(next);
              setTaskPage(1);
            }}
            options={[
              { value: "all", label: t.all, count: recentTasks.length },
              { value: "openai_responses", label: "Responses" },
              { value: "anthropic_messages", label: "Messages" },
            ]}
          />
        }
      >
        {shownTasks.length === 0 ? (
          <EmptyState icon={<Activity />} title={t.noRequests} />
        ) : (
          <div className="lg-table-wrap">
            <table className="lg-table">
              <thead>
                <tr>
                  <th>{t.col.time}</th>
                  <th>{t.col.model}</th>
                  <th className="hide-sm">{t.col.session}</th>
                  <th className="hide-sm">{t.col.activity}</th>
                  <th className="num">{t.col.hitRate}</th>
                  <th className="num hide-sm">{t.col.io}</th>
                  <th className="num" title={t.ttftHeaderHint}>
                    {t.col.ttft}
                  </th>
                  <th className="num hide-sm">{t.col.elapsed}</th>
                  {usage && <th className="num">{t.col.cost}</th>}
                </tr>
              </thead>
              <tbody>
                {shownTasks.map((task) => {
                  const toolOutcome = outcomes.get(task.task_id);
                  return (
                    <tr
                      key={task.task_id}
                      data-clickable="true"
                      data-tone={taskTone(task, toolOutcome)}
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
                            {!task.session_reused && <Pill>{t.created}</Pill>}
                          </span>
                        ) : (
                          <span className="text-fg-4">—</span>
                        )}
                      </td>
                      <td className="hide-sm">
                        <span className="inline-flex flex-wrap items-center gap-1.5">
                          {task.phase !== "completed" && <PhasePill phase={task.phase} />}
                          <ToolOutcome counts={toolOutcome} />
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
                      <td className="num whitespace-nowrap">
                        <Latency ms={task.ttft_ms} kind="ttft" />
                      </td>
                      <td className="num hide-sm whitespace-nowrap">
                        <Latency ms={task.elapsed_ms} kind="elapsed" />
                      </td>
                      {usage && (
                        <td className="num whitespace-nowrap">
                          <Usd value={task.cost_usd} unpricedHint={task.usage !== null} />
                        </td>
                      )}
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
        <Pager page={clampedTaskPage} pages={taskPages} total={filteredTasks.length} onPage={setTaskPage} />
      </Card>

      <TaskSheet
        task={selectedTask}
        thresholdBps={thresholdBps}
        toolOutcome={selectedTask ? outcomes.get(selectedTask.task_id) : undefined}
        onClose={() => setSelectedTask(null)}
        onOpenToolCalls={toolSummary ? (taskId) => navigate("tools", taskId) : undefined}
      />
    </>
  );
}
