import * as React from "react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  CircleAlert,
  FilterX,
  LoaderCircle,
  MousePointerClick,
  Radio,
  Scissors,
  Sparkles,
  Wrench,
} from "lucide-react";
import { RankedBars } from "@/components/charts";
import { Skyline, type SkylineMetric } from "@/components/Skyline";
import { InspectorTitle, ToolCallInspector } from "@/components/ToolCallInspector";
import { ToolGlyph } from "@/components/ToolExchange";
import { Sheet } from "@/components/ui/dialog";
import {
  Button,
  Card,
  EmptyState,
  HarnessBadge,
  Latency,
  Loading,
  Note,
  Pager,
  SearchInput,
  Segmented,
  Select,
  Unsupported,
} from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { useMediaQuery } from "@/hooks/useMediaQuery";
import {
  formatBytes,
  formatDurationMs,
  formatNumber,
  formatPct,
  formatSmartTime,
  shortId,
} from "@/lib/format";
import { harnessMeta } from "@/lib/harness";
import { useI18n } from "@/lib/i18n";
import { TOOL_SCALE, latencyTone } from "@/lib/latency";
import {
  EMPTY_FILTER,
  type HarnessStats,
  type ToolCallFilter,
  type ToolKind,
  aggregateByHarness,
  classifyTool,
  countBy,
  displayToolName,
  filterToolCalls,
  isFailure,
  isFilterActive,
  recordKind,
  routeLabel,
  toolKindLabel,
} from "@/lib/toolcalls";
import type { ToolCallRecord, ToolCallTelemetrySummary } from "@/types/telemetry";
import type { ViewProps } from "@/views/types";

const PAGE_SIZE = 60;
const KIND_ORDER: readonly ToolKind[] = ["orchestrator", "patch", "shell", "file", "search", "web", "other"];

/* ---------- Harness scoreboard ---------- */

function MiniBar({
  label,
  ratio,
  display,
  color,
}: {
  label: string;
  /** 0..1 against the scale shared by every card. */
  ratio: number;
  display: string;
  color: string;
}): React.JSX.Element {
  return (
    <div className="lg-minibar">
      <span className="text-fg-3">{label}</span>
      <span className="lg-minibar-track">
        <span style={{ width: `${Math.max(0, Math.min(1, ratio)) * 100}%`, background: color }} />
      </span>
      <span className="lg-num text-right text-fg-2">{display}</span>
    </div>
  );
}

function HarnessBoard({
  stats,
  active,
  onToggle,
}: {
  stats: HarnessStats[];
  active: string;
  onToggle: (harness: string) => void;
}): React.JSX.Element {
  const { m } = useI18n();
  const t = m.tools.board;
  // Every card measures against the same maxima, so bar lengths compare across cards.
  const maxOut = Math.max(1, ...stats.map((s) => s.avgOutputBytes));
  const maxP50 = Math.max(1, ...stats.map((s) => s.p50DurationMs ?? 0));
  return (
    <div className="lg-board" role="group" aria-label={t.aria}>
      {stats.map((s) => {
        const meta = harnessMeta(s.harness);
        return (
          <button
            key={s.harness}
            type="button"
            className="lg-hcard"
            aria-pressed={active === s.harness}
            onClick={() => onToggle(s.harness)}
          >
            <span className="flex items-center justify-between gap-2">
              <HarnessBadge id={s.harness} className="text-[13px] font-semibold text-fg" />
              <span className="lg-num text-[11.5px] text-fg-3">{t.calls(formatNumber(s.calls))}</span>
            </span>
            <span className="flex items-baseline gap-2">
              <span className={`lg-hcard-hero ${s.failures > 0 ? "text-err" : ""}`}>
                {formatPct(s.failureRate, 1)}
              </span>
              <span className="text-[12px] text-fg-3">{t.failureRate}</span>
            </span>
            <MiniBar
              label="CodeMode"
              ratio={s.orchestratorShare}
              display={formatPct(s.orchestratorShare)}
              color={meta.color}
            />
            <MiniBar
              label={t.avgOutput}
              ratio={s.avgOutputBytes / maxOut}
              display={formatBytes(s.avgOutputBytes)}
              color={meta.color}
            />
            <MiniBar
              label={t.p50}
              ratio={(s.p50DurationMs ?? 0) / maxP50}
              display={formatDurationMs(s.p50DurationMs)}
              color={meta.color}
            />
          </button>
        );
      })}
    </div>
  );
}

/* ---------- Call stream ---------- */

function CallRow({
  rec,
  selected,
  hot,
  fresh,
  maxDuration,
  onSelect,
  onHover,
}: {
  rec: ToolCallRecord;
  selected: boolean;
  hot: boolean;
  fresh: boolean;
  maxDuration: number;
  onSelect: (rec: ToolCallRecord) => void;
  onHover: (id: string | null) => void;
}): React.JSX.Element {
  const { m } = useI18n();
  const t = m.tools.row;
  const failed = isFailure(rec);
  const pending = rec.status === "pending";
  const name = displayToolName(rec);
  const durTone = rec.duration_ms != null ? latencyTone(rec.duration_ms, TOOL_SCALE) : undefined;
  const unwrapped = rec.route === "namespace_hub" || rec.route === "generic_dispatcher";
  const preview = failed && rec.output_preview ? rec.output_preview : rec.input_preview;
  // Square-root keeps short calls visible beside a multi-second outlier.
  const durRatio =
    maxDuration > 0 && rec.duration_ms != null ? Math.sqrt(rec.duration_ms / maxDuration) : 0;
  return (
    <li
      id={`call-${rec.call_id}`}
      role="option"
      aria-selected={selected}
      className="lg-call"
      data-failed={failed}
      data-status={rec.status}
      data-hot={hot}
      data-fresh={fresh ? "true" : undefined}
      onClick={() => onSelect(rec)}
      onMouseEnter={() => onHover(rec.call_id)}
      onMouseLeave={() => onHover(null)}
    >
      <ToolGlyph kind={recordKind(rec)} failed={failed} />
      <div className="min-w-0">
        <div className="flex min-w-0 items-center gap-1.5">
          <span className="lg-call-name">{name}</span>
          {unwrapped && (
            <span className="lg-num lg-clip min-w-0 text-[11px] text-fg-4" title={t.unwrappedFrom(rec.wire_tool_name)}>
              ← {rec.wire_tool_name}
            </span>
          )}
          {rec.route !== "client_roundtrip" && !unwrapped && (
            <span className="lg-call-route">{routeLabel(rec.route)}</span>
          )}
          {rec.v4a_auto_repaired && (
            <span className="lg-call-flag" title={t.v4a}>
              <Sparkles />
            </span>
          )}
          {rec.output_spilled && (
            <span className="lg-call-flag" title={t.spilled}>
              <Scissors />
            </span>
          )}
          {rec.output_sanitized && (
            <span className="lg-call-flag" title={t.sanitized}>
              <Wrench />
            </span>
          )}
        </div>
        <div className="lg-call-preview" data-tone={failed ? "err" : undefined}>
          {preview || "—"}
        </div>
      </div>
      <div className="lg-call-io">
        <span>{formatBytes(rec.input_bytes)}</span>
        <span className="text-fg-4"> → </span>
        <span className={rec.output_bytes > 9600 ? "font-medium text-fg" : undefined}>
          {formatBytes(rec.output_bytes)}
        </span>
      </div>
      <div className="lg-call-dur" data-tone={failed ? "err" : durTone}>
        {pending ? (
          <span className="inline-flex items-center gap-1 text-accent">
            <LoaderCircle className="lg-spin h-3 w-3" />
            {t.running}
          </span>
        ) : (
          <Latency ms={rec.duration_ms} kind="tool" />
        )}
        <span className="lg-durbar" aria-hidden="true">
          <span style={{ width: `${durRatio * 100}%` }} />
        </span>
      </div>
    </li>
  );
}

interface CallGroup {
  taskId: string;
  head: ToolCallRecord;
  records: ToolCallRecord[];
}

function groupByTurn(records: ToolCallRecord[]): CallGroup[] {
  const groups: CallGroup[] = [];
  for (const rec of records) {
    const last = groups[groups.length - 1];
    if (last && last.taskId === rec.task_id) last.records.push(rec);
    else groups.push({ taskId: rec.task_id, head: rec, records: [rec] });
  }
  return groups;
}

/* ---------- Degraded view for gateways without per-call telemetry ---------- */

function LegacyToolNames(): React.JSX.Element | null {
  const { snapshot } = useGateway();
  const { m } = useI18n();
  const rows = useMemo(() => {
    const names: string[] = [];
    for (const task of snapshot?.recent_tasks ?? []) names.push(...task.client_tool_calls);
    return countBy(names, (n) => n).slice(0, 12);
  }, [snapshot?.recent_tasks]);
  if (rows.length === 0) return null;
  return (
    <Card
      title={m.tools.legacy.title}
      sub={m.tools.legacy.sub}
    >
      <RankedBars
        rows={rows.map(([name, count]) => ({
          key: name,
          label: (
            <span className="flex items-center gap-2">
              <ToolGlyph kind={classifyTool(name)} size="sm" />
              <span className="lg-num lg-clip">{name}</span>
            </span>
          ),
          value: count,
          display: m.tools.legacy.times(count),
          hint: name,
        }))}
        labelWidth={220}
        color="var(--lg-s-neutral)"
      />
    </Card>
  );
}

/** Turns whose saved low-cache payload already holds real tool round-trips. */
function ReplayableTurns({ onOpen }: { onOpen: (taskId: string) => void }): React.JSX.Element | null {
  const { lowCacheRecords } = useGateway();
  const { m } = useI18n();
  const t = m.tools.replay;
  const turns = useMemo(
    () =>
      lowCacheRecords
        .filter(
          (r) =>
            (r.request_metrics?.tool_call_part_count ?? 0) > 0 &&
            r.request_capture?.full_payload_saved === true,
        )
        .sort((a, b) => b.recorded_at_unix_ms - a.recorded_at_unix_ms)
        .slice(0, 8),
    [lowCacheRecords],
  );
  if (turns.length === 0) return null;
  return (
    <Card
      flush
      title={t.title}
      sub={t.sub}
    >
      <div className="lg-table-wrap">
        <table className="lg-table">
          <thead>
            <tr>
              <th>{t.time}</th>
              <th className="hide-sm">{t.modelSession}</th>
              <th className="num">{t.calls}</th>
              <th className="num hide-sm">{t.sizes}</th>
              <th>Prompt</th>
            </tr>
          </thead>
          <tbody>
            {turns.map((rec) => (
              <tr
                key={`${rec.task_id}-${rec.recorded_at_unix_ms}`}
                data-clickable="true"
                tabIndex={0}
                onClick={() => onOpen(rec.task_id)}
                onKeyDown={(ev) => {
                  if (ev.key === "Enter") onOpen(rec.task_id);
                }}
              >
                <td className="lg-num whitespace-nowrap text-[12px] text-fg-3">
                  {formatSmartTime(rec.recorded_at_unix_ms)}
                </td>
                <td className="hide-sm whitespace-nowrap">
                  <span className="lg-num text-[12px] text-fg">{rec.resolved_model}</span>
                  <span className="lg-num ml-2 text-[11.5px] text-fg-4">{shortId(rec.session_id)}</span>
                </td>
                <td className="num">{t.times(rec.request_metrics?.tool_call_part_count ?? 0)}</td>
                <td className="num hide-sm whitespace-nowrap">
                  {formatBytes(rec.request_metrics?.tool_call_bytes ?? 0)}
                  <span className="text-fg-4"> / </span>
                  {formatBytes(rec.request_metrics?.tool_result_bytes ?? 0)}
                </td>
                <td className="max-w-[360px]">
                  <span className="lg-clip text-[12px] text-fg-3" title={rec.prompt_preview}>
                    {rec.prompt_preview || "—"}
                  </span>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

function BufferStatus({ summary }: { summary: ToolCallTelemetrySummary }): React.JSX.Element {
  const { m } = useI18n();
  const t = m.tools.buffer;
  return (
    <span
      title={t.hint(
        formatBytes(summary.stored_bytes),
        formatBytes(summary.max_total_bytes),
        formatNumber(summary.total_evicted),
      )}
    >
      {t.cap(formatNumber(summary.max_records))}
    </span>
  );
}

/* ---------- View ---------- */

export function ToolCallsView({ arg, navigate }: ViewProps): React.JSX.Element {
  const { snapshot, toolCalls, toolCallsStatus, toolCallsError, reloadToolCalls } = useGateway();
  const { m } = useI18n();
  const t = m.tools.view;
  const summary = snapshot?.tool_call_telemetry;
  const wide = useMediaQuery("(min-width: 1180px)");

  const [filter, setFilter] = useState<ToolCallFilter>(EMPTY_FILTER);
  const [page, setPage] = useState(1);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [hoverId, setHoverId] = useState<string | null>(null);
  const [metric, setMetric] = useState<SkylineMetric>("bytes");
  const [follow, setFollow] = useState(false);
  const [sheetOpen, setSheetOpen] = useState(false);
  const [freshIds, setFreshIds] = useState<ReadonlySet<string>>(new Set());


  const patchFilter = useCallback((patch: Partial<ToolCallFilter>): void => {
    setFilter((f) => ({ ...f, ...patch }));
    setPage(1);
  }, []);

  const records = useMemo(
    () =>
      [...toolCalls].sort((a, b) => b.started_at_unix_ms - a.started_at_unix_ms || b.seq - a.seq),
    [toolCalls],
  );
  const filtered = useMemo(() => filterToolCalls(records, filter), [records, filter]);
  const pages = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE));
  const clampedPage = Math.min(page, pages);
  const pageRecords = useMemo(
    () => filtered.slice((clampedPage - 1) * PAGE_SIZE, clampedPage * PAGE_SIZE),
    [filtered, clampedPage],
  );
  const chronological = useMemo(() => [...pageRecords].reverse(), [pageRecords]);
  const groups = useMemo(() => groupByTurn(pageRecords), [pageRecords]);
  const maxDuration = useMemo(
    () => pageRecords.reduce((m, r) => Math.max(m, r.duration_ms ?? 0), 0),
    [pageRecords],
  );

  // The board stays a comparison even while one harness is picked.
  const harnessStats = useMemo(
    () => aggregateByHarness(filterToolCalls(records, { ...filter, harness: "all" })),
    [records, filter],
  );
  const kindCounts = useMemo(() => {
    const counts = new Map<ToolKind, number>();
    for (const rec of filterToolCalls(records, { ...filter, kind: "all" })) {
      const kind = recordKind(rec);
      counts.set(kind, (counts.get(kind) ?? 0) + 1);
    }
    return counts;
  }, [records, filter]);
  const failureCount = useMemo(
    () => filterToolCalls(records, { ...filter, errorsOnly: false }).filter(isFailure).length,
    [records, filter],
  );
  const spilledCount = useMemo(
    () => filterToolCalls(records, { ...filter, spilledOnly: false }).filter((r) => r.output_spilled).length,
    [records, filter],
  );
  const sessionOptions = useMemo(() => {
    const harnessOf = new Map<string, string>();
    for (const rec of records) {
      if (rec.session_id) harnessOf.set(rec.session_id, rec.harness_profile);
    }
    return [
      { value: "all", label: t.allSessions },
      ...countBy(records, (r) => r.session_id).map(([id, count]) => ({
        value: id,
        label: `${harnessMeta(harnessOf.get(id)).label} · ${shortId(id)} (${count})`,
      })),
    ];
  }, [records, t.allSessions]);
  // The URL argument is either a call to open or a search to run.
  const appliedArg = useRef<string | null>(null);
  useEffect(() => {
    if (arg === null || toolCallsStatus !== "ready" || appliedArg.current === arg) return;
    appliedArg.current = arg;
    const index = records.findIndex((r) => r.call_id === arg);
    if (index >= 0) {
      setFilter(EMPTY_FILTER);
      setPage(Math.floor(index / PAGE_SIZE) + 1);
      setSelectedId(arg);
      setSheetOpen(true);
    } else {
      setFilter({ ...EMPTY_FILTER, query: arg });
      setPage(1);
    }
  }, [arg, toolCallsStatus, records]);

  // Selection is derived: on a wide screen the pane always shows something.
  const selected =
    filtered.find((r) => r.call_id === selectedId) ?? (wide ? (pageRecords[0] ?? null) : null);
  const selectedIndex = selected ? pageRecords.findIndex((r) => r.call_id === selected.call_id) : -1;
  const autoPicked = wide && selected !== null && selected.call_id !== selectedId ? selected.call_id : null;
  useEffect(() => {
    // Pin the automatic pick: calls that arrive later must not steal the pane.
    if (autoPicked !== null) setSelectedId(autoPicked);
  }, [autoPicked]);

  const select = useCallback((rec: ToolCallRecord, reveal = false): void => {
    setSelectedId(rec.call_id);
    setSheetOpen(true);
    if (reveal) {
      window.requestAnimationFrame(() => {
        document.getElementById(`call-${rec.call_id}`)?.scrollIntoView({ block: "nearest" });
      });
    }
  }, []);

  const move = useCallback(
    (delta: number): void => {
      if (pageRecords.length === 0) return;
      const from = selectedIndex < 0 ? (delta > 0 ? -1 : 0) : selectedIndex;
      const next = pageRecords[Math.max(0, Math.min(pageRecords.length - 1, from + delta))];
      if (next) select(next, true);
    },
    [pageRecords, selectedIndex, select],
  );

  // J / K walk the stream from anywhere on the page, like a mail client.
  useEffect(() => {
    const onKey = (ev: KeyboardEvent): void => {
      if (ev.defaultPrevented || ev.metaKey || ev.ctrlKey || ev.altKey) return;
      // The target is the document itself when nothing is focused.
      const target = ev.target instanceof Element ? ev.target : null;
      if (target?.closest("input, textarea, select, [contenteditable='true']")) return;
      if (ev.key === "j") move(1);
      else if (ev.key === "k") move(-1);
      else return;
      ev.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [move]);

  // Calls that arrived after the first load flash once as they land.
  const seen = useRef<Set<string>>(new Set());
  const primed = useRef(false);
  const followRef = useRef(follow);
  followRef.current = follow;
  useEffect(() => {
    if (toolCallsStatus !== "ready") return;
    const arrived = new Set<string>();
    for (const rec of records) {
      if (seen.current.has(rec.call_id)) continue;
      seen.current.add(rec.call_id);
      if (primed.current) arrived.add(rec.call_id);
    }
    primed.current = true;
    if (arrived.size === 0) return;
    setFreshIds(arrived);
    if (followRef.current) {
      const newest = records[0];
      if (newest) {
        setPage(1);
        setSelectedId(newest.call_id);
      }
    }
    const timer = window.setTimeout(() => setFreshIds(new Set()), 2200);
    return () => window.clearTimeout(timer);
  }, [records, toolCallsStatus]);

  if (toolCallsStatus === "unsupported") {
    return (
      <>
        <Card>
          <Unsupported feature={t.unsupported} endpoint="GET /api/telemetry/tool-calls" />
        </Card>
        <ReplayableTurns onOpen={(taskId) => navigate("cache", taskId)} />
        <LegacyToolNames />
      </>
    );
  }
  if (toolCallsStatus === "loading") {
    return (
      <Card>
        <Loading label={t.loading} />
      </Card>
    );
  }
  if (toolCallsStatus === "error" && records.length === 0) {
    return (
      <Note tone="err" icon={<CircleAlert />}>
        {t.loadFailed(toolCallsError ?? "")}
        <Button size="sm" className="ml-2" onClick={reloadToolCalls}>
          {t.retry}
        </Button>
      </Note>
    );
  }
  if (records.length === 0) {
    return (
      <Card>
        <EmptyState
          icon={<Wrench />}
          title={t.empty}
          hint={t.emptyHint}
        />
      </Card>
    );
  }

  const active = isFilterActive(filter);
  const position =
    selectedIndex >= 0 ? `${selectedIndex + 1} / ${pageRecords.length}` : undefined;
  const inspector = selected ? (
    <ToolCallInspector
      key={selected.call_id}
      record={selected}
      position={position}
      onPrev={selectedIndex > 0 ? () => move(-1) : undefined}
      onNext={selectedIndex >= 0 && selectedIndex < pageRecords.length - 1 ? () => move(1) : undefined}
      onOpenCatalog={(sessionId) => navigate("catalog", sessionId)}
      onFilterSession={(sessionId) => patchFilter({ session: sessionId })}
    />
  ) : null;

  return (
    <>
      <HarnessBoard
        stats={harnessStats}
        active={filter.harness}
        onToggle={(harness) => patchFilter({ harness: filter.harness === harness ? "all" : harness })}
      />

      <Card
        title={t.skyline}
        sub={metric === "bytes" ? t.skylineBytes : t.skylineDuration}
        actions={
          <Segmented<SkylineMetric>
            ariaLabel={t.skylineMetric}
            value={metric}
            onChange={setMetric}
            options={[
              { value: "bytes", label: t.bytes },
              { value: "duration", label: t.duration },
            ]}
          />
        }
      >
        {chronological.length > 0 ? (
          <Skyline
            records={chronological}
            metric={metric}
            selectedId={selected?.call_id ?? null}
            hoverId={hoverId}
            onSelect={(rec) => select(rec, true)}
            onHover={setHoverId}
          />
        ) : (
          <p className="lg-hint m-0">{t.noMatch}</p>
        )}
      </Card>

      <div className="lg-split" data-wide={wide}>
        <Card
          flush
          title={t.stream}
          sub={
            <>
              {active ? `${formatNumber(filtered.length)} / ` : ""}
              {t.count(formatNumber(records.length))}
              {summary && <BufferStatus summary={summary} />}
            </>
          }
          actions={
            <>
              <Button
                size="sm"
                aria-pressed={follow}
                title={t.followHint}
                onClick={() => setFollow((v) => !v)}
              >
                <Radio />
                {t.follow}
              </Button>
            </>
          }
        >
          <div className="lg-toolbar">
            <SearchInput
              value={filter.query}
              onChange={(query) => patchFilter({ query })}
              placeholder={t.search}
              className="min-w-[220px] flex-[2]"
            />
            <Select
              ariaLabel={t.sessionFilter}
              className="min-w-[150px] flex-1"
              value={filter.session}
              onChange={(session) => patchFilter({ session })}
              options={sessionOptions}
            />
            {active && (
              <Button size="sm" variant="ghost" onClick={() => patchFilter(EMPTY_FILTER)}>
                <FilterX />
                {t.clear}
              </Button>
            )}
          </div>
          <div className="lg-toolbar lg-chiprow" role="group" aria-label={t.kindFilter}>
            {KIND_ORDER.filter((kind) => (kindCounts.get(kind) ?? 0) > 0 || filter.kind === kind).map(
              (kind) => (
                <button
                  key={kind}
                  type="button"
                  className="lg-chip"
                  aria-pressed={filter.kind === kind}
                  onClick={() => patchFilter({ kind: filter.kind === kind ? "all" : kind })}
                >
                  <ToolGlyph kind={kind} size="sm" />
                  {toolKindLabel(kind)}
                  <span className="lg-seg-count">{kindCounts.get(kind) ?? 0}</span>
                </button>
              ),
            )}
            <button
              type="button"
              className="lg-chip"
              data-tone="err"
              aria-pressed={filter.errorsOnly}
              onClick={() => patchFilter({ errorsOnly: !filter.errorsOnly })}
            >
              <CircleAlert />
              {t.errorsOnly}
              <span className="lg-seg-count">{failureCount}</span>
            </button>
            {(spilledCount > 0 || filter.spilledOnly) && (
              <button
                type="button"
                className="lg-chip"
                aria-pressed={filter.spilledOnly}
                onClick={() => patchFilter({ spilledOnly: !filter.spilledOnly })}
              >
                <Scissors />
                {t.spilledOnly}
                <span className="lg-seg-count">{spilledCount}</span>
              </button>
            )}
          </div>

          {pageRecords.length === 0 ? (
            <EmptyState
              icon={<FilterX />}
              title={t.noMatchTitle}
              action={
                <Button size="sm" onClick={() => patchFilter(EMPTY_FILTER)}>
                  {t.clear}
                </Button>
              }
            />
          ) : (
            <ul
              className="lg-stream"
              role="listbox"
              tabIndex={0}
              aria-label={t.streamAria}
              aria-activedescendant={selected ? `call-${selected.call_id}` : undefined}
              onKeyDown={(ev) => {
                if (ev.key === "ArrowDown") move(1);
                else if (ev.key === "ArrowUp") move(-1);
                else if (ev.key === "Home" && pageRecords[0]) select(pageRecords[0], true);
                else if (ev.key === "End") {
                  const last = pageRecords[pageRecords.length - 1];
                  if (last) select(last, true);
                } else return;
                ev.preventDefault();
              }}
            >
              {groups.map((group) => (
                <React.Fragment key={`${group.taskId}-${group.head.call_id}`}>
                  <li className="lg-callgroup" role="presentation">
                    <HarnessBadge id={group.head.harness_profile} className="font-medium text-fg-2" />
                    <span className="lg-num" title={group.head.session_id ?? undefined}>
                      {shortId(group.head.session_id)}
                    </span>
                    <span className="lg-num flex-1">{t.turn(group.head.turn_index)}</span>
                    <span className="lg-num flex-none">{formatSmartTime(group.head.started_at_unix_ms)}</span>
                  </li>
                  {group.records.map((rec) => (
                    <CallRow
                      key={rec.call_id}
                      rec={rec}
                      selected={selected?.call_id === rec.call_id}
                      hot={hoverId === rec.call_id}
                      fresh={freshIds.has(rec.call_id)}
                      maxDuration={maxDuration}
                      onSelect={select}
                      onHover={setHoverId}
                    />
                  ))}
                </React.Fragment>
              ))}
            </ul>
          )}
          <Pager page={clampedPage} pages={pages} total={filtered.length} onPage={setPage} />
        </Card>

        {wide && (
          <aside className="lg-dock" aria-label={t.dockAria}>
            {selected && inspector ? (
              <>
                <div className="lg-dock-head">
                  <InspectorTitle record={selected} />
                </div>
                <div className="lg-dock-body">{inspector}</div>
              </>
            ) : (
              <EmptyState
                icon={<MousePointerClick />}
                title={t.pick}
                hint={t.pickHint}
              />
            )}
          </aside>
        )}
      </div>

      {!wide && (
        <Sheet
          open={sheetOpen && selected !== null}
          onOpenChange={setSheetOpen}
          title={selected ? <InspectorTitle record={selected} /> : t.sheetTitle}
        >
          {inspector}
        </Sheet>
      )}
    </>
  );
}
