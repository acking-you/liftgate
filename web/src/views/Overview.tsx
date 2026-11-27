import * as React from "react";
import { useEffect, useMemo, useState } from "react";
import {
  Activity,
  ArrowRight,
  CircleAlert,
  CircleDollarSign,
  FilterX,
  Gauge,
  Layers,
  Sparkles,
  TrendingDown,
  Wrench,
} from "lucide-react";
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
  Button,
  Card,
  EmptyState,
  Figure,
  HarnessBadge,
  HitRate,
  Latency,
  Meter,
  Pager,
  Pill,
  SearchInput,
  Segmented,
  Select,
  Stat,
  StatGrid,
  Tag,
  Usd,
} from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { useRevisionResource } from "@/hooks/useResource";
import { useToday } from "@/hooks/useToday";
import { addDays } from "@/lib/days";
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
import { harnessMeta, sortHarnessIds } from "@/lib/harness";
import { useI18n } from "@/lib/i18n";
import {
  WIRE_BUDGET_BYTES,
  cacheStats,
  elasticStats,
  sessionReuseRatio,
} from "@/lib/stats";
import {
  EMPTY_TASK_FILTER,
  type TaskFilter,
  UNKNOWN_HARNESS,
  countInOrder,
  filterTasks,
  isTaskFilterActive,
  isTaskLowCache,
  isTaskProblem,
  loadTaskFilter,
  saveTaskFilter,
  summarizeTasks,
  taskHarness,
} from "@/lib/taskfilter";
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
          {task.harness_profile && <HarnessBadge id={task.harness_profile} className="text-[12px] text-fg-2" />}
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
  const { client, baseUrl, snapshot, lowCacheRecords, toolCalls } = useGateway();
  const { m } = useI18n();
  const t = m.overview;
  const [filter, setFilter] = useState<TaskFilter>(loadTaskFilter);
  const [selectedTask, setSelectedTask] = useState<TaskSnapshot | null>(null);
  const [taskPage, setTaskPage] = useState(1);

  const metrics = snapshot?.metrics;
  const pool = snapshot?.session_pool;
  const cacheSummary = snapshot?.cache_telemetry;
  const toolSummary = snapshot?.tool_call_telemetry;
  const usage = snapshot?.usage;
  const today = useToday();
  const todayUsage = useRevisionResource(
    usage ? () => client.usage({ from: today, to: addDays(today, 1) }) : null,
    `${baseUrl}|${client.demo}|usage-today|${today}`,
    usage?.revision ?? -1,
  );
  const todaySummary = todayUsage.data?.summary;
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

  const filterCtx = useMemo(() => ({ thresholdBps, outcomes }), [thresholdBps, outcomes]);
  const patchFilter = (patch: Partial<TaskFilter>): void => {
    setFilter((f) => ({ ...f, ...patch }));
    setTaskPage(1);
  };
  const toggleHarness = (harness: string): void =>
    patchFilter({ harness: filter.harness === harness ? "all" : harness });
  const toggleSession = (session: string): void =>
    patchFilter({ session: filter.session === session ? "all" : session });
  const filterActive = isTaskFilterActive(filter);
  useEffect(() => saveTaskFilter(filter), [filter]);
  const filteredTasks = useMemo(
    () => filterTasks(recentTasks, filter, filterCtx),
    [recentTasks, filter, filterCtx],
  );
  const scope = useMemo(() => summarizeTasks(filteredTasks, outcomes), [filteredTasks, outcomes]);
  // Facet counts ignore their own facet, so every option shows what picking it would yield.
  const facets = useMemo(() => {
    const without = (patch: Partial<TaskFilter>): TaskSnapshot[] =>
      filterTasks(recentTasks, { ...filter, ...patch }, filterCtx);
    const byHarness = countInOrder(without({ harness: "all" }), taskHarness);
    const bySession = countInOrder(without({ session: "all" }), (task) => task.session_id);
    const byModel = countInOrder(without({ model: "all" }), (task) => task.resolved_model);
    const byProtocol = countInOrder(without({ protocol: "all" }), (task) => task.protocol);
    const problems = without({ problemsOnly: false }).filter((task) =>
      isTaskProblem(task, outcomes.get(task.task_id)),
    ).length;
    const lowCache = without({ lowCacheOnly: false }).filter((task) => isTaskLowCache(task, thresholdBps)).length;
    return { byHarness, bySession, byModel, byProtocol, problems, lowCache };
  }, [recentTasks, filter, filterCtx, outcomes, thresholdBps]);
  const harnessIds = useMemo(() => {
    const ids = new Set(recentTasks.map(taskHarness));
    if (filter.harness !== "all") ids.add(filter.harness);
    const known = sortHarnessIds([...ids].filter((id) => id !== UNKNOWN_HARNESS));
    return ids.has(UNKNOWN_HARNESS) ? [...known, UNKNOWN_HARNESS] : known;
  }, [recentTasks, filter.harness]);
  const sessionOptions = useMemo(() => {
    const harnessOf = new Map<string, string | null | undefined>();
    for (const task of recentTasks) {
      if (task.session_id && !harnessOf.has(task.session_id)) harnessOf.set(task.session_id, task.harness_profile);
    }
    const counts = new Map(facets.bySession);
    if (filter.session !== "all" && !counts.has(filter.session)) counts.set(filter.session, 0);
    return [
      { value: "all", label: t.filters.allSessions },
      ...[...counts].map(([id, count]) => ({
        value: id,
        label: `${harnessMeta(harnessOf.get(id)).label} · ${shortId(id)} (${count})`,
      })),
    ];
  }, [recentTasks, facets.bySession, filter.session, t.filters.allSessions]);
  const modelOptions = useMemo(() => {
    const counts = new Map(facets.byModel);
    if (filter.model !== "all" && !counts.has(filter.model)) counts.set(filter.model, 0);
    return [
      { value: "all", label: t.filters.allModels },
      ...[...counts].map(([id, count]) => ({ value: id, label: `${id} (${count})` })),
    ];
  }, [facets.byModel, filter.model, t.filters.allModels]);
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
            label={t.stats.costToday}
            icon={<CircleDollarSign />}
            value={todaySummary ? formatUsd(todaySummary.cost_usd) : "—"}
            onClick={() => navigate("usage", "today")}
            foot={
              todaySummary && todaySummary.unpriced_calls > 0 ? (
                <span className="text-warn">{t.stats.unpricedFoot(formatNumber(todaySummary.unpriced_calls))}</span>
              ) : (
                <span>{t.stats.costTotalFoot(formatUsd(usage.cost_usd), formatNumber(usage.calls))}</span>
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
        <Card title={t.trendTitle} sub={filterActive ? t.filters.trendFiltered : undefined}>
          <CacheTrendChart tasks={filteredTasks} thresholdBps={thresholdBps} onSelect={setSelectedTask} />
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
        sub={
          filterActive
            ? t.filters.matched(formatNumber(filteredTasks.length), formatNumber(recentTasks.length))
            : undefined
        }
        actions={
          <Segmented<"all" | TaskProtocol>
            ariaLabel={t.protocolFilter}
            value={filter.protocol}
            onChange={(protocol) => patchFilter({ protocol })}
            options={[
              { value: "all", label: t.all, count: filterTasks(recentTasks, { ...filter, protocol: "all" }, filterCtx).length },
              {
                value: "openai_responses",
                label: "Responses",
                count: facets.byProtocol.get("openai_responses") ?? 0,
              },
              {
                value: "anthropic_messages",
                label: "Messages",
                count: facets.byProtocol.get("anthropic_messages") ?? 0,
              },
            ]}
          />
        }
      >
        {recentTasks.length > 0 && (
          <>
            <div className="lg-toolbar lg-chiprow" role="group" aria-label={t.filters.harness}>
              {harnessIds.map((id) => {
                const meta = harnessMeta(id === UNKNOWN_HARNESS ? null : id);
                return (
                  <button
                    key={id}
                    type="button"
                    className="lg-chip lg-chip-plain"
                    aria-pressed={filter.harness === id}
                    title={t.filters.harnessHint(meta.label)}
                    onClick={() => toggleHarness(id)}
                  >
                    <span className="lg-swatch" style={{ background: meta.color }} aria-hidden="true" />
                    {meta.label}
                    <span className="lg-seg-count">{facets.byHarness.get(id) ?? 0}</span>
                  </button>
                );
              })}
              <button
                type="button"
                className="lg-chip"
                data-tone="err"
                aria-pressed={filter.problemsOnly}
                title={t.filters.problemsHint}
                onClick={() => patchFilter({ problemsOnly: !filter.problemsOnly })}
              >
                <CircleAlert />
                {t.filters.problems}
                <span className="lg-seg-count">{facets.problems}</span>
              </button>
              <button
                type="button"
                className="lg-chip"
                aria-pressed={filter.lowCacheOnly}
                title={t.filters.lowCacheHint}
                onClick={() => patchFilter({ lowCacheOnly: !filter.lowCacheOnly })}
              >
                <TrendingDown />
                {t.filters.lowCache(Math.round(thresholdBps / 100))}
                <span className="lg-seg-count">{facets.lowCache}</span>
              </button>
            </div>
            <div className="lg-toolbar">
              <SearchInput
                className="min-w-[200px] flex-[1.4]"
                value={filter.query}
                onChange={(query) => patchFilter({ query })}
                placeholder={t.filters.search}
              />
              <Select
                ariaLabel={t.filters.session}
                className="min-w-[180px] flex-1"
                value={filter.session}
                onChange={(session) => patchFilter({ session })}
                options={sessionOptions}
              />
              <Select
                ariaLabel={t.filters.model}
                className="min-w-[180px] flex-1"
                value={filter.model}
                onChange={(model) => patchFilter({ model })}
                options={modelOptions}
              />
              {filterActive && (
                <Button size="sm" variant="ghost" onClick={() => patchFilter(EMPTY_TASK_FILTER)}>
                  <FilterX />
                  {t.filters.clear}
                </Button>
              )}
            </div>
            {filterActive && filteredTasks.length > 0 && (
              <div
                className="flex flex-wrap items-center gap-x-5 gap-y-1 border-t border-line px-4 py-2 text-[12px] text-fg-3"
                aria-live="polite"
              >
                <span className="lg-num text-fg-2">
                  {t.filters.scope.requests(formatNumber(scope.count), formatNumber(scope.sessions))}
                </span>
                <span className="inline-flex items-center gap-1.5">
                  {t.filters.scope.problems}
                  <span className={scope.problems > 0 ? "lg-num font-medium text-err" : "lg-num text-fg"}>
                    {formatNumber(scope.problems)}
                  </span>
                </span>
                <span className="inline-flex items-center gap-1.5">
                  {t.filters.scope.hitRate}
                  <HitRate bps={scope.hitBps} thresholdBps={thresholdBps} />
                </span>
                <span className="inline-flex items-center gap-1.5">
                  {t.filters.scope.ttft}
                  <Latency ms={scope.ttftP50Ms} kind="ttft" />
                  <span className="text-fg-4">/</span>
                  <Latency ms={scope.ttftP90Ms} kind="ttft" />
                </span>
                {usage && (
                  <span className="inline-flex items-center gap-1.5">
                    {t.filters.scope.cost}
                    <Usd value={scope.costUsd} />
                  </span>
                )}
              </div>
            )}
          </>
        )}
        {shownTasks.length === 0 ? (
          <EmptyState icon={<Activity />} title={recentTasks.length > 0 ? t.filters.noMatch : t.noRequests} />
        ) : (
          <div className="lg-table-wrap">
            <table className="lg-table">
              <thead>
                <tr>
                  <th>{t.col.time}</th>
                  <th>{t.col.harness}</th>
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
                      <td className="whitespace-nowrap text-[12px] text-fg-2">
                        <button
                          type="button"
                          className="lg-cellfilter"
                          aria-pressed={filter.harness === taskHarness(task)}
                          title={t.filters.harnessHint(harnessMeta(task.harness_profile).label)}
                          onClick={(ev) => {
                            ev.stopPropagation();
                            toggleHarness(taskHarness(task));
                          }}
                          onKeyDown={(ev) => ev.stopPropagation()}
                        >
                          <HarnessBadge id={task.harness_profile} />
                        </button>
                      </td>
                      <td className="text-[12px] text-fg">
                        <ModelRoute task={task} />
                      </td>
                      <td className="hide-sm whitespace-nowrap">
                        {task.session_id ? (
                          <span className="inline-flex items-center gap-1.5">
                            <button
                              type="button"
                              className="lg-cellfilter lg-num text-[12px]"
                              aria-pressed={filter.session === task.session_id}
                              title={t.filters.sessionHint}
                              onClick={(ev) => {
                                ev.stopPropagation();
                                if (task.session_id) toggleSession(task.session_id);
                              }}
                              onKeyDown={(ev) => ev.stopPropagation()}
                            >
                              {shortId(task.session_id)}
                            </button>
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
