import * as React from "react";
import { useEffect, useMemo, useState } from "react";
import { CircleAlert, CircleCheck, FilterX, GitCompare, ScrollText, Trash2, TrendingDown } from "lucide-react";
import { ProseBlock } from "@/components/CodeBlock";
import { ContextRewritesView, ThreeStagePipelineBanner } from "@/components/ContextRewrites";
import { CopyButton } from "@/components/CopyButton";
import { MessageTimeline } from "@/components/MessageTimeline";
import { AcpRequestView, ClientToolsView } from "@/components/PayloadViews";
import { Sheet } from "@/components/ui/dialog";
import {
  Button,
  Card,
  ConfirmButton,
  EmptyState,
  Field,
  Figure,
  HarnessBadge,
  HitRate,
  Loading,
  Meter,
  Note,
  Pill,
  ProviderBadge,
  SearchInput,
  Select,
  SwitchRow,
  Tabs,
  Unsupported,
} from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { useResource, useRevisionResource } from "@/hooks/useResource";
import { errorMessage } from "@/lib/api";
import { formatBytes, formatDateTime, formatNumber, formatSmartTime, shortId } from "@/lib/format";
import { harnessMeta, sortHarnessIds } from "@/lib/harness";
import { useI18n } from "@/lib/i18n";
import { providerMeta, sortProviderIds } from "@/lib/provider";
import { UNKNOWN_HARNESS } from "@/lib/taskfilter";
import type { ContextCaptureRecord, ContextCaptureSummary } from "@/types/telemetry";
import type { ViewProps } from "@/views/types";

const MB = 1024 * 1024;

interface Banner {
  tone: "ok" | "err";
  text: string;
}

type TabId = "timeline" | "rewrites" | "request" | "output" | "tools";

interface CaptureFilter {
  /** Harness profile id, {@link UNKNOWN_HARNESS}, or `"all"`. */
  harness: string;
  provider: string;
  session: string;
  model: string;
  rewrittenOnly: boolean;
  lowCacheOnly: boolean;
  /** Case-insensitive substring over ids, model, and prompt preview. */
  query: string;
}

const EMPTY_CAPTURE_FILTER: CaptureFilter = {
  harness: "all",
  provider: "all",
  session: "all",
  model: "all",
  rewrittenOnly: false,
  lowCacheOnly: false,
  query: "",
};

function captureHarness(rec: ContextCaptureRecord): string {
  return rec.harness_profile || UNKNOWN_HARNESS;
}

function captureProvider(rec: ContextCaptureRecord): string {
  return (rec.provider_id ?? "agy").trim().toLowerCase() || "agy";
}

function matchesCapture(rec: ContextCaptureRecord, filter: CaptureFilter, thresholdBps: number): boolean {
  if (filter.harness !== "all" && captureHarness(rec) !== filter.harness) return false;
  if (filter.provider !== "all" && captureProvider(rec) !== filter.provider) return false;
  if (filter.session !== "all" && rec.session_id !== filter.session) return false;
  if (filter.model !== "all" && rec.resolved_model !== filter.model) return false;
  if (filter.rewrittenOnly && (rec.rewrite_count ?? 0) === 0) return false;
  if (filter.lowCacheOnly && !(typeof rec.cache_hit_rate_bps === "number" && rec.cache_hit_rate_bps < thresholdBps)) {
    return false;
  }
  const query = filter.query.trim().toLowerCase();
  return (
    query === "" ||
    [rec.task_id, rec.session_id, rec.provider_id, rec.resolved_model, rec.prompt_preview].some(
      (field) => typeof field === "string" && field.toLowerCase().includes(query),
    )
  );
}

/** Counts per key in first-seen order (records arrive newest first). */
function tally(
  records: readonly ContextCaptureRecord[],
  key: (rec: ContextCaptureRecord) => string | null | undefined,
): Map<string, number> {
  const out = new Map<string, number>();
  for (const rec of records) {
    const k = key(rec);
    if (k) out.set(k, (out.get(k) ?? 0) + 1);
  }
  return out;
}

function ContextSheet({
  record,
  thresholdBps,
  onClose,
}: {
  record: ContextCaptureRecord | null;
  thresholdBps: number;
  onClose: () => void;
}): React.JSX.Element {
  const { client } = useGateway();
  const { m } = useI18n();
  const t = m.context.sheet;
  const [tab, setTab] = useState<TabId>("timeline");
  // Cross-links between the timeline and the rewrite comparisons.
  const [focusMessage, setFocusMessage] = useState<number | null>(null);
  const [focusRewrite, setFocusRewrite] = useState<number | null>(null);
  const taskId = record?.task_id ?? null;
  const detail = useResource(
    taskId !== null ? () => client.contextCaptureDetail(taskId) : null,
    `context|${taskId ?? ""}`,
  );
  useEffect(() => {
    setTab("timeline");
    setFocusMessage(null);
    setFocusRewrite(null);
  }, [taskId]);
  const payload = detail.data?.payload ?? null;
  const rewrites = payload?.rewrites ?? [];
  const tracksRewrites = payload?.rewrites !== undefined || record?.rewrite_count !== undefined;

  return (
    <Sheet
      size="wide"
      open={record !== null}
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      title={
        record && (
          <>
            <span className="lg-num">{record.task_id}</span>
            <HarnessBadge id={record.harness_profile} className="text-[12.5px] font-normal text-fg-2" />
            <ProviderBadge
              id={record.provider_id}
              mode={record.optimization_mode}
              compact
              className="text-[12.5px] font-normal text-fg-2"
            />
          </>
        )
      }
      description={
        record && (
          <span className="lg-num">
            {t.description(
              record.resolved_model,
              shortId(record.session_id),
              record.session_turn_index,
              formatDateTime(record.recorded_at_unix_ms),
            )}
          </span>
        )
      }
    >
      {record && (
        <>
          <div className="grid grid-cols-2 gap-4 sm:grid-cols-4">
            <Figure
              label={t.hitRate}
              value={<HitRate bps={record.cache_hit_rate_bps} thresholdBps={thresholdBps} digits={2} />}
            />
            <Figure
              label={t.io}
              value={`${formatNumber(record.total_prompt_tokens)} / ${formatNumber(record.output_tokens)}`}
              sub="tokens"
            />
            <Figure
              label={t.messages}
              value={t.messageCount(record.message_count)}
              sub={t.deltaCount(record.delta_message_count)}
            />
            <Figure label={t.payload} value={formatBytes(record.payload_bytes)} />
          </div>

          {payload?.upstream_wire && <ThreeStagePipelineBanner wire={payload.upstream_wire} />}

          <div className="flex items-end justify-between gap-3">
            <div className="min-w-0 flex-1">
              <Tabs<TabId>
                ariaLabel={t.tabsAria}
                value={tab}
                onChange={(next) => {
                  setTab(next);
                  setFocusMessage(null);
                  setFocusRewrite(null);
                }}
                tabs={[
                  { value: "timeline", label: t.tabTimeline, count: payload?.messages.length ?? record.message_count },
                  ...(tracksRewrites
                    ? [
                        {
                          value: "rewrites" as const,
                          label: t.tabRewrites,
                          count: payload ? rewrites.length : record.rewrite_count,
                        },
                      ]
                    : []),
                  { value: "request", label: t.tabRequest },
                  { value: "output", label: t.tabOutput },
                  { value: "tools", label: t.tabTools, count: payload?.client_tools.length },
                ]}
              />
            </div>
            {payload && <CopyButton value={() => JSON.stringify(detail.data, null, 2)} label={t.copyJson} />}
          </div>

          {detail.status === "loading" && <Loading label={t.loading} />}
          {detail.status === "error" && (
            <Note tone="err" icon={<CircleAlert />}>
              {t.readFailed(detail.error ?? "")}
            </Note>
          )}
          {detail.status === "unsupported" && <Note icon={<CircleAlert />}>{t.reclaimed}</Note>}

          {payload && tab === "timeline" && (
            <MessageTimeline
              key={record.task_id}
              messages={payload.messages}
              rewrites={rewrites}
              focus={focusMessage}
              onShowRewrite={(idx) => {
                setFocusRewrite(idx);
                setTab("rewrites");
              }}
            />
          )}
          {payload && tab === "rewrites" && (
            <ContextRewritesView
              rewrites={rewrites}
              focus={focusRewrite}
              onLocate={(messageIndex) => {
                setFocusMessage(messageIndex);
                setTab("timeline");
              }}
            />
          )}
          {payload && tab === "request" && (
            <AcpRequestView
              systemAppend={payload.acp_system_append}
              toolsDelta={payload.tools_delta_xml}
              promptBlocks={payload.outbound_acp_prompt_blocks}
            />
          )}
          {payload && tab === "output" && (
            <div className="flex flex-col gap-3">
              {payload.assistant_reasoning && (
                <ProseBlock
                  text={payload.assistant_reasoning}
                  title={payload.assistant_reasoning_truncated ? t.reasoningCut : t.reasoning}
                  maxHeight={360}
                />
              )}
              <ProseBlock
                text={payload.assistant_output ?? payload.assistant_output_preview}
                title={
                  payload.assistant_output == null
                    ? t.outputPreview
                    : payload.assistant_output_truncated
                      ? t.outputCut
                      : t.output
                }
                maxHeight={520}
              />
            </div>
          )}
          {payload && tab === "tools" && <ClientToolsView tools={payload.client_tools} />}
          {payload?.payload_truncated && <p className="lg-hint m-0">{t.truncated}</p>}
        </>
      )}
    </Sheet>
  );
}

function ControlCard({
  summary,
  onBanner,
  onChanged,
}: {
  summary: ContextCaptureSummary;
  onBanner: (banner: Banner) => void;
  onChanged: () => void;
}): React.JSX.Element {
  const { client, refresh } = useGateway();
  const { m } = useI18n();
  const t = m.context.control;
  const [maxRecords, setMaxRecords] = useState(String(summary.max_records));
  const [maxMb, setMaxMb] = useState(String(Math.round(summary.max_total_bytes / MB)));
  const [dirty, setDirty] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (dirty) return;
    setMaxRecords(String(summary.max_records));
    setMaxMb(String(Math.round(summary.max_total_bytes / MB)));
  }, [dirty, summary.max_records, summary.max_total_bytes]);

  const apply = async (action: () => Promise<unknown>, done: string): Promise<void> => {
    setBusy(true);
    try {
      await action();
      onBanner({ tone: "ok", text: done });
      await refresh();
      onChanged();
    } catch (err) {
      onBanner({ tone: "err", text: t.failed(errorMessage(err)) });
    } finally {
      setBusy(false);
    }
  };

  const saveQuota = (ev: React.FormEvent): void => {
    ev.preventDefault();
    const records = Number.parseInt(maxRecords.trim(), 10);
    const mb = Number.parseInt(maxMb.trim(), 10);
    if (!Number.isFinite(records) || records < 1 || records > 10_000) {
      onBanner({ tone: "err", text: t.recordsRange });
      return;
    }
    if (!Number.isFinite(mb) || mb < 16 || mb > 65_536) {
      onBanner({ tone: "err", text: t.diskRange });
      return;
    }
    void apply(
      () => client.updateContextCapture({ max_records: records, max_total_mib: mb }),
      t.quotaSaved(formatNumber(records), formatNumber(mb)),
    ).then(() => setDirty(false));
  };

  return (
    <Card title={t.title}>
      <div className="grid grid-cols-1 gap-x-10 gap-y-5 xl:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]">
        <div className="flex flex-col gap-4">
          <SwitchRow
            title={summary.enabled ? t.on : t.off}
            hint={t.hint}
            checked={summary.enabled}
            disabled={busy}
            onChange={(next) =>
              void apply(
                () => client.updateContextCapture({ enabled: next }),
                next ? t.enabled : t.disabled,
              )
            }
          />
          {summary.capture_dir && (
            <p className="lg-hint m-0">
              {t.location} <span className="lg-num text-fg-2">{summary.capture_dir}</span>
            </p>
          )}
          {(summary.skipped_records ?? 0) > 0 && (
            <p className="lg-hint m-0 text-warn">
              {t.skipped(<span className="lg-num">{summary.skipped_records}</span>)}
            </p>
          )}
        </div>

        <form className="flex flex-col gap-3" onSubmit={saveQuota}>
          <div className="grid grid-cols-2 gap-3">
            <div>
              <div className="mb-1.5 flex items-baseline justify-between text-[12px] text-fg-3">
                <span>{t.records}</span>
                <span className="lg-num">
                  {formatNumber(summary.stored_records)} / {formatNumber(summary.max_records)}
                </span>
              </div>
              <Meter
                ratio={summary.max_records > 0 ? summary.stored_records / summary.max_records : 0}
                label={t.recordsQuota}
              />
            </div>
            <div>
              <div className="mb-1.5 flex items-baseline justify-between text-[12px] text-fg-3">
                <span>{t.disk}</span>
                <span className="lg-num">
                  {formatBytes(summary.stored_bytes)} / {formatBytes(summary.max_total_bytes)}
                </span>
              </div>
              <Meter
                ratio={summary.max_total_bytes > 0 ? summary.stored_bytes / summary.max_total_bytes : 0}
                tone={summary.stored_bytes / Math.max(1, summary.max_total_bytes) > 0.9 ? "warn" : undefined}
                label={t.diskQuota}
              />
            </div>
            <Field label={t.maxRecords} htmlFor="ctx-max-records">
              <input
                id="ctx-max-records"
                className="lg-input lg-num"
                type="number"
                min={1}
                value={maxRecords}
                onChange={(e) => {
                  setMaxRecords(e.target.value);
                  setDirty(true);
                }}
              />
            </Field>
            <Field label={t.maxMb} htmlFor="ctx-max-mb">
              <input
                id="ctx-max-mb"
                className="lg-input lg-num"
                type="number"
                min={1}
                value={maxMb}
                onChange={(e) => {
                  setMaxMb(e.target.value);
                  setDirty(true);
                }}
              />
            </Field>
          </div>
          <div className="flex flex-wrap items-center justify-between gap-2">
            <Button type="submit" variant="primary" disabled={busy || !dirty}>
              {t.saveQuota}
            </Button>
            <ConfirmButton
              label={t.clear}
              confirmLabel={t.clearConfirm(formatNumber(summary.stored_records))}
              icon={<Trash2 />}
              disabled={busy || summary.stored_records === 0}
              onConfirm={() => void apply(() => client.clearContextCaptures(), t.cleared)}
            />
          </div>
        </form>
      </div>
    </Card>
  );
}

export function ContextView({ arg, navigate }: ViewProps): React.JSX.Element {
  const { client, snapshot, baseUrl } = useGateway();
  const { m } = useI18n();
  const t = m.context.view;
  const live = snapshot?.context_capture;
  const advertised = snapshot === null ? null : live !== undefined;
  const captures = useRevisionResource(
    advertised === true ? () => client.contextCaptures() : null,
    `${baseUrl}|${client.demo}|context-captures`,
    live?.revision ?? -1,
  );
  const [selected, setSelected] = useState<ContextCaptureRecord | null>(null);
  const [banner, setBanner] = useState<Banner | null>(null);
  const thresholdBps = snapshot?.cache_telemetry?.low_cache_threshold_bps ?? 5000;
  const summary = live ?? captures.data?.summary;
  const records = useMemo(() => captures.data?.records ?? [], [captures.data]);
  // `#/context/<task_id>` opens that turn's saved full context directly.
  const linked = arg !== null ? (records.find((r) => r.task_id === arg) ?? null) : null;
  const open = selected ?? linked;
  const linkMissing = arg !== null && captures.status === "ready" && linked === null;
  const fo = m.overview.filters;
  const fv = t.filters;
  const [filter, setFilter] = useState<CaptureFilter>(EMPTY_CAPTURE_FILTER);
  const patchFilter = (patch: Partial<CaptureFilter>): void => setFilter((prev) => ({ ...prev, ...patch }));
  const filterActive =
    filter.harness !== "all" ||
    filter.provider !== "all" ||
    filter.session !== "all" ||
    filter.model !== "all" ||
    filter.rewrittenOnly ||
    filter.lowCacheOnly ||
    filter.query.trim() !== "";
  const filtered = useMemo(
    () => records.filter((rec) => matchesCapture(rec, filter, thresholdBps)),
    [records, filter, thresholdBps],
  );
  // Facet counts ignore their own facet, so every option shows what picking it would yield.
  const facets = useMemo(() => {
    const without = (patch: Partial<CaptureFilter>): ContextCaptureRecord[] =>
      records.filter((rec) => matchesCapture(rec, { ...filter, ...patch }, thresholdBps));
    return {
      byHarness: tally(without({ harness: "all" }), captureHarness),
      byProvider: tally(without({ provider: "all" }), captureProvider),
      bySession: tally(without({ session: "all" }), (rec) => rec.session_id),
      byModel: tally(without({ model: "all" }), (rec) => rec.resolved_model),
      rewritten: without({ rewrittenOnly: false }).filter((rec) => (rec.rewrite_count ?? 0) > 0).length,
      lowCache: without({ lowCacheOnly: false }).filter(
        (rec) => typeof rec.cache_hit_rate_bps === "number" && rec.cache_hit_rate_bps < thresholdBps,
      ).length,
    };
  }, [records, filter, thresholdBps]);
  const harnessIds = useMemo(() => {
    const ids = new Set(records.map(captureHarness));
    if (filter.harness !== "all") ids.add(filter.harness);
    const known = sortHarnessIds([...ids].filter((id) => id !== UNKNOWN_HARNESS));
    return ids.has(UNKNOWN_HARNESS) ? [...known, UNKNOWN_HARNESS] : known;
  }, [records, filter.harness]);
  const sessionOptions = useMemo(() => {
    const harnessOf = new Map<string, string | null | undefined>();
    for (const rec of records) {
      if (rec.session_id && !harnessOf.has(rec.session_id)) harnessOf.set(rec.session_id, rec.harness_profile);
    }
    const counts = new Map(facets.bySession);
    if (filter.session !== "all" && !counts.has(filter.session)) counts.set(filter.session, 0);
    return [
      { value: "all", label: fo.allSessions },
      ...[...counts].map(([id, count]) => ({
        value: id,
        label: `${harnessMeta(harnessOf.get(id)).label} · ${shortId(id)} (${count})`,
      })),
    ];
  }, [records, facets.bySession, filter.session, fo.allSessions]);
  const modelOptions = useMemo(() => {
    const counts = new Map(facets.byModel);
    if (filter.model !== "all" && !counts.has(filter.model)) counts.set(filter.model, 0);
    return [
      { value: "all", label: fo.allModels },
      ...[...counts].map(([id, count]) => ({ value: id, label: `${id} (${count})` })),
    ];
  }, [facets.byModel, filter.model, fo.allModels]);
  const providerOptions = useMemo(() => {
    const counts = new Map(facets.byProvider);
    if (filter.provider !== "all" && !counts.has(filter.provider)) counts.set(filter.provider, 0);
    const ids = sortProviderIds([...counts.keys()]);
    return [
      { value: "all", label: fo.allProviders },
      ...ids.map((id) => ({ value: id, label: `${providerMeta(id).label} (${counts.get(id) ?? 0})` })),
    ];
  }, [facets.byProvider, filter.provider, fo.allProviders]);

  if (advertised === null) {
    return (
      <Card>
        <Loading label={t.waiting} />
      </Card>
    );
  }
  if (advertised === false || (captures.status === "unsupported" && !live)) {
    return (
      <Card>
        <Unsupported feature={t.unsupported} endpoint="GET /api/telemetry/context-captures">
          {t.unsupportedHint}
        </Unsupported>
      </Card>
    );
  }

  return (
    <>
      {banner && (
        <Note tone={banner.tone} icon={banner.tone === "ok" ? <CircleCheck /> : <CircleAlert />}>
          {banner.text}
        </Note>
      )}

      {linkMissing && (
        <Note icon={<CircleAlert />}>
          {t.linkMissing(arg)}
        </Note>
      )}

      {summary ? (
        <ControlCard summary={summary} onBanner={setBanner} onChanged={captures.reload} />
      ) : (
        <Card>
          <Loading label={t.loadingState} />
        </Card>
      )}

      <Card
        flush
        title={t.saved(records.length)}
        sub={filterActive ? fv.matched(formatNumber(filtered.length), formatNumber(records.length)) : t.savedSub}
      >
        {records.length > 0 && (
          <>
            <div className="lg-toolbar lg-chiprow" role="group" aria-label={fo.harness}>
              {harnessIds.map((id) => {
                const meta = harnessMeta(id === UNKNOWN_HARNESS ? null : id);
                return (
                  <button
                    key={id}
                    type="button"
                    className="lg-chip lg-chip-plain"
                    aria-pressed={filter.harness === id}
                    title={fo.harnessHint(meta.label)}
                    onClick={() => patchFilter({ harness: filter.harness === id ? "all" : id })}
                  >
                    <span className="lg-swatch" style={{ background: meta.color }} aria-hidden="true" />
                    {meta.label}
                    <span className="lg-seg-count">{facets.byHarness.get(id) ?? 0}</span>
                  </button>
                );
              })}
              {(facets.rewritten > 0 || filter.rewrittenOnly) && (
                <button
                  type="button"
                  className="lg-chip"
                  aria-pressed={filter.rewrittenOnly}
                  title={fv.rewrittenHint}
                  onClick={() => patchFilter({ rewrittenOnly: !filter.rewrittenOnly })}
                >
                  <GitCompare />
                  {fv.rewritten}
                  <span className="lg-seg-count">{facets.rewritten}</span>
                </button>
              )}
              <button
                type="button"
                className="lg-chip"
                aria-pressed={filter.lowCacheOnly}
                title={fo.lowCacheHint}
                onClick={() => patchFilter({ lowCacheOnly: !filter.lowCacheOnly })}
              >
                <TrendingDown />
                {fo.lowCache(Math.round(thresholdBps / 100))}
                <span className="lg-seg-count">{facets.lowCache}</span>
              </button>
            </div>
            <div className="lg-toolbar">
              <SearchInput
                className="min-w-[200px] flex-[1.4]"
                value={filter.query}
                onChange={(query) => patchFilter({ query })}
                placeholder={fv.search}
              />
              <Select
                ariaLabel={fo.provider}
                className="min-w-[165px] flex-1"
                value={filter.provider}
                onChange={(provider) => patchFilter({ provider })}
                options={providerOptions}
              />
              <Select
                ariaLabel={fo.session}
                className="min-w-[180px] flex-1"
                value={filter.session}
                onChange={(session) => patchFilter({ session })}
                options={sessionOptions}
              />
              <Select
                ariaLabel={fo.model}
                className="min-w-[180px] flex-1"
                value={filter.model}
                onChange={(model) => patchFilter({ model })}
                options={modelOptions}
              />
              {filterActive && (
                <Button size="sm" variant="ghost" onClick={() => setFilter(EMPTY_CAPTURE_FILTER)}>
                  <FilterX />
                  {fo.clear}
                </Button>
              )}
            </div>
          </>
        )}
        {captures.status === "loading" ? (
          <Loading />
        ) : captures.status === "error" && captures.data === null ? (
          <div className="px-4 pb-4">
            <Note tone="err" icon={<CircleAlert />}>
              {t.readFailed(captures.error ?? "")}
            </Note>
          </div>
        ) : records.length === 0 ? (
          <EmptyState
            icon={<ScrollText />}
            title={summary?.enabled ? t.noneYet : t.off}
            hint={summary?.enabled ? t.noneYetHint : t.offHint}
          />
        ) : filtered.length === 0 ? (
          <EmptyState icon={<ScrollText />} title={fv.noMatch} />
        ) : (
          <div className="lg-table-wrap">
            <table className="lg-table">
              <thead>
                <tr>
                  <th>{t.col.time}</th>
                  <th>{t.col.harness}</th>
                  <th className="hide-sm">{t.col.provider}</th>
                  <th className="hide-sm">{t.col.model}</th>
                  <th className="num hide-sm">{t.col.turn}</th>
                  <th className="num">{t.col.hitRate}</th>
                  <th className="num hide-sm">{t.col.io}</th>
                  <th className="num hide-sm">{t.col.messages}</th>
                  <th className="num hide-sm">{t.col.rewrites}</th>
                  <th className="num">{t.col.payload}</th>
                  <th className="hide-sm">Prompt</th>
                </tr>
              </thead>
              <tbody>
                {filtered.map((rec) => (
                  <tr
                    key={rec.task_id}
                    data-clickable="true"
                    tabIndex={0}
                    onClick={() => setSelected(rec)}
                    onKeyDown={(ev) => {
                      if (ev.key === "Enter") setSelected(rec);
                    }}
                  >
                    <td className="lg-num whitespace-nowrap text-[12px] text-fg-3">
                      {formatSmartTime(rec.recorded_at_unix_ms)}
                    </td>
                    <td className="whitespace-nowrap">
                      <button
                        type="button"
                        className="lg-cellfilter"
                        aria-pressed={filter.harness === captureHarness(rec)}
                        title={fo.harnessHint(harnessMeta(rec.harness_profile).label)}
                        onClick={(ev) => {
                          ev.stopPropagation();
                          const id = captureHarness(rec);
                          patchFilter({ harness: filter.harness === id ? "all" : id });
                        }}
                        onKeyDown={(ev) => ev.stopPropagation()}
                      >
                        <HarnessBadge id={rec.harness_profile} className="text-fg" />
                      </button>
                      {rec.session_id && (
                        <button
                          type="button"
                          className="lg-cellfilter lg-num ml-3 text-[11.5px] text-fg-4 max-md:hidden"
                          aria-pressed={filter.session === rec.session_id}
                          title={fo.sessionHint}
                          onClick={(ev) => {
                            ev.stopPropagation();
                            const id = rec.session_id ?? "all";
                            patchFilter({ session: filter.session === id ? "all" : id });
                          }}
                          onKeyDown={(ev) => ev.stopPropagation()}
                        >
                          {shortId(rec.session_id)}
                        </button>
                      )}
                    </td>
                    <td className="hide-sm whitespace-nowrap">
                      <button
                        type="button"
                        className="lg-cellfilter"
                        aria-pressed={filter.provider === captureProvider(rec)}
                        title={fo.provider}
                        onClick={(ev) => {
                          ev.stopPropagation();
                          const id = captureProvider(rec);
                          patchFilter({ provider: filter.provider === id ? "all" : id });
                        }}
                        onKeyDown={(ev) => ev.stopPropagation()}
                      >
                        <ProviderBadge id={rec.provider_id} mode={rec.optimization_mode} compact className="text-fg" />
                      </button>
                    </td>
                    <td className="lg-num hide-sm whitespace-nowrap text-[12px]">{rec.resolved_model}</td>
                    <td className="num hide-sm">{rec.session_turn_index}</td>
                    <td className="num">
                      <HitRate bps={rec.cache_hit_rate_bps} thresholdBps={thresholdBps} />
                    </td>
                    <td className="num hide-sm whitespace-nowrap">
                      {formatNumber(rec.total_prompt_tokens)}
                      <span className="text-fg-4"> / </span>
                      {formatNumber(rec.output_tokens)}
                    </td>
                    <td className="num hide-sm whitespace-nowrap">
                      {rec.message_count}
                      <span className="text-fg-4">{t.delta(rec.delta_message_count)}</span>
                    </td>
                    <td className="num hide-sm">
                      {rec.rewrite_count ? (
                        <Pill tone="warn">{rec.rewrite_count}</Pill>
                      ) : (
                        <span className="text-fg-4">—</span>
                      )}
                    </td>
                    <td className="num whitespace-nowrap">{formatBytes(rec.payload_bytes)}</td>
                    <td className="hide-sm max-w-[260px]">
                      <span className="lg-clip text-[12px] text-fg-3" title={rec.prompt_preview}>
                        {rec.prompt_preview || "—"}
                      </span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>

      <ContextSheet
        record={open}
        thresholdBps={thresholdBps}
        onClose={() => {
          setSelected(null);
          if (arg !== null) navigate("context");
        }}
      />
    </>
  );
}
