import * as React from "react";
import { useEffect, useMemo, useState } from "react";
import { CircleAlert, CircleCheck, Gauge, Trash2 } from "lucide-react";
import { SegmentedBar } from "@/components/charts";
import { LowCacheSheet, ReasonPills } from "@/components/LowCacheSheet";
import {
  Button,
  Card,
  ConfirmButton,
  EmptyState,
  Field,
  Meter,
  Note,
  Pager,
  SearchInput,
  Segmented,
  SwitchRow,
  Tag,
} from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { errorMessage } from "@/lib/api";
import {
  formatBpsPct,
  formatDurationMs,
  formatNumber,
  formatSmartTime,
  shortId,
} from "@/lib/format";
import { useI18n } from "@/lib/i18n";
import { cacheStats } from "@/lib/stats";
import type { CacheTelemetrySummary, LowCacheHitReason, LowCacheHitRecord } from "@/types/telemetry";
import type { ViewProps } from "@/views/types";

const PAGE_SIZE = 25;

interface Banner {
  tone: "ok" | "err";
  text: string;
}

function ConfigCard({
  summary,
  onBanner,
}: {
  summary: CacheTelemetrySummary | undefined;
  onBanner: (banner: Banner) => void;
}): React.JSX.Element {
  const { client, refresh, reloadLowCache } = useGateway();
  const { m } = useI18n();
  const t = m.cache.config;
  const [maxRecords, setMaxRecords] = useState("1000");
  const [thresholdPct, setThresholdPct] = useState("50");
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const capture = summary?.capture_request_details ?? true;
  const serverMax = summary?.max_low_cache_records;
  const serverThreshold = summary?.low_cache_threshold_bps;

  // Follow the server unless the reader is mid-edit.
  useEffect(() => {
    if (dirty || serverMax === undefined || serverThreshold === undefined) return;
    setMaxRecords(String(serverMax));
    setThresholdPct(String(Math.round(serverThreshold / 100)));
  }, [dirty, serverMax, serverThreshold]);

  const save = async (ev: React.FormEvent): Promise<void> => {
    ev.preventDefault();
    const max = Number.parseInt(maxRecords.trim(), 10);
    const pct = Number.parseInt(thresholdPct.trim(), 10);
    if (!Number.isFinite(max) || max < 0 || max > 50_000) {
      onBanner({ tone: "err", text: t.recordsRange });
      return;
    }
    if (!Number.isFinite(pct) || pct < 0 || pct > 100) {
      onBanner({ tone: "err", text: t.thresholdRange });
      return;
    }
    setSaving(true);
    try {
      const next = await client.updateCacheConfig({
        max_low_cache_records: max,
        low_cache_threshold_pct: pct,
      });
      setDirty(false);
      onBanner({
        tone: "ok",
        text: t.saved(formatNumber(next.max_low_cache_records), Math.round(next.low_cache_threshold_bps / 100)),
      });
      await Promise.all([refresh(), reloadLowCache()]);
    } catch (err) {
      onBanner({ tone: "err", text: t.saveFailed(errorMessage(err)) });
    } finally {
      setSaving(false);
    }
  };

  const toggleCapture = async (next: boolean): Promise<void> => {
    try {
      await client.updateCacheConfig({ capture_request_details: next });
      onBanner({ tone: "ok", text: t.captureToggled(next) });
      await refresh();
    } catch (err) {
      onBanner({ tone: "err", text: t.toggleFailed(errorMessage(err)) });
    }
  };

  const clear = async (resetTotals: boolean): Promise<void> => {
    try {
      await client.clearLowCacheHits(resetTotals);
      onBanner({
        tone: "ok",
        text: resetTotals ? t.clearedReset : t.cleared,
      });
      await Promise.all([refresh(), reloadLowCache()]);
    } catch (err) {
      onBanner({ tone: "err", text: t.clearFailed(errorMessage(err)) });
    }
  };

  return (
    <Card title={t.title}>
      <form className="flex flex-col gap-4" onSubmit={(ev) => void save(ev)}>
        <div className="grid grid-cols-2 gap-3">
          <Field label={t.maxRecords} htmlFor="cache-max-records">
            <input
              id="cache-max-records"
              className="lg-input lg-num"
              type="number"
              min={0}
              max={50000}
              value={maxRecords}
              onChange={(e) => {
                setMaxRecords(e.target.value);
                setDirty(true);
              }}
            />
          </Field>
          <Field label={t.threshold} htmlFor="cache-threshold">
            <input
              id="cache-threshold"
              className="lg-input lg-num"
              type="number"
              min={0}
              max={100}
              value={thresholdPct}
              onChange={(e) => {
                setThresholdPct(e.target.value);
                setDirty(true);
              }}
            />
          </Field>
        </div>
        <SwitchRow
          title={t.capture}
          hint={t.captureHint}
          checked={capture}
          onChange={(next) => void toggleCapture(next)}
        />
        <div className="flex flex-wrap items-center justify-between gap-2 border-t border-line pt-3">
          <Button type="submit" variant="primary" disabled={saving || !dirty}>
            {saving ? t.saving : t.save}
          </Button>
          <span className="flex flex-wrap items-center gap-1.5">
            <ConfirmButton
              label={t.clear}
              confirmLabel={t.clearConfirm}
              icon={<Trash2 />}
              disabled={(summary?.stored_low_cache_count ?? 0) === 0}
              onConfirm={() => void clear(false)}
            />
            <ConfirmButton
              label={t.reset}
              confirmLabel={t.resetConfirm}
              onConfirm={() => void clear(true)}
            />
          </span>
        </div>
      </form>
    </Card>
  );
}

export function CacheView({ arg, navigate }: ViewProps): React.JSX.Element {
  const { snapshot, lowCacheRecords } = useGateway();
  const { m } = useI18n();
  const t = m.cache.view;
  const summary = snapshot?.cache_telemetry;
  const stats = useMemo(() => cacheStats(summary, snapshot?.metrics), [summary, snapshot?.metrics]);
  const thresholdBps = summary?.low_cache_threshold_bps ?? 5000;
  const maxRecords = summary?.max_low_cache_records ?? 1000;

  const [reason, setReason] = useState<"all" | LowCacheHitReason>("all");
  const [query, setQuery] = useState("");
  const [page, setPage] = useState(1);
  const [selected, setSelected] = useState<LowCacheHitRecord | null>(null);
  const [banner, setBanner] = useState<Banner | null>(null);

  // `#/cache/<task_id>` opens that turn's saved tool round-trips directly.
  const linked = arg !== null ? (lowCacheRecords.find((r) => r.task_id === arg) ?? null) : null;
  const open = selected ?? linked;

  const counts = useMemo(() => {
    const out: Record<LowCacheHitReason, number> = {
      warm_cache_miss: 0,
      warm_partial_hit: 0,
      cold_session_start: 0,
      unknown: 0,
    };
    for (const rec of lowCacheRecords) out[rec.reason] += 1;
    return out;
  }, [lowCacheRecords]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return lowCacheRecords.filter((rec) => {
      if (reason !== "all" && rec.reason !== reason) return false;
      if (q === "") return true;
      return (
        rec.task_id.toLowerCase().includes(q) ||
        rec.resolved_model.toLowerCase().includes(q) ||
        rec.requested_model.toLowerCase().includes(q) ||
        (rec.session_id?.toLowerCase().includes(q) ?? false) ||
        rec.prompt_preview.toLowerCase().includes(q)
      );
    });
  }, [lowCacheRecords, reason, query]);
  const pages = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE));
  const clampedPage = Math.min(page, pages);
  const shown = filtered.slice((clampedPage - 1) * PAGE_SIZE, clampedPage * PAGE_SIZE);

  return (
    <>
      {banner && (
        <Note tone={banner.tone} icon={banner.tone === "ok" ? <CircleCheck /> : <CircleAlert />}>
          {banner.text}
        </Note>
      )}

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-[minmax(0,1.7fr)_minmax(0,1fr)]">
        <Card
          title={t.total}
          sub={t.totalSub}
          actions={
            (summary?.auxiliary_turns ?? 0) > 0 ? (
              <Tag title={t.auxiliaryHint}>{t.auxiliary(formatNumber(summary?.auxiliary_turns ?? 0))}</Tag>
            ) : undefined
          }
        >
          <div className="flex flex-wrap items-end gap-x-10 gap-y-4">
            <div>
              <div className="lg-hero">{formatBpsPct(stats.hitBps, 1)}</div>
              <div className="mt-2 text-[12px] text-fg-3">
                {t.allTurns(formatNumber(stats.evaluatedTurns))}
              </div>
            </div>
            <div className="min-w-[180px] flex-1">
              <div className="flex items-baseline justify-between gap-3 text-[12px] text-fg-3">
                <span>{t.warm}</span>
                <span className="lg-num text-[15px] font-semibold text-fg">
                  {formatBpsPct(stats.warmHitBps, 1)}
                </span>
              </div>
              <div className="mt-1.5">
                <Meter ratio={(stats.warmHitBps ?? 0) / 10000} label={t.warmMeter} />
              </div>
              <div className="mt-1.5 text-[12px] text-fg-3">
                {t.warmFoot(
                  formatNumber(stats.warmTurns),
                  formatNumber(summary?.total_low_cache_turns ?? lowCacheRecords.length),
                )}
              </div>
            </div>
          </div>
          <div className="mt-5">
            <h3 className="lg-section-title">{t.inputMix(formatNumber(stats.promptTokens))}</h3>
            <SegmentedBar
              ariaLabel={t.inputMixAria}
              format={formatNumber}
              parts={[
                { key: "read", label: t.cacheRead, value: stats.cachedRead, color: "var(--lg-s1)" },
                { key: "write", label: t.cacheWrite, value: stats.cachedWrite, color: "var(--lg-s4)", ink: "dark" },
                { key: "input", label: t.uncached, value: stats.uncachedInput, color: "var(--lg-s-neutral)" },
              ]}
            />
          </div>
        </Card>

        <ConfigCard summary={summary} onBanner={setBanner} />
      </div>

      <Card
        flush
        title={t.records}
        sub={
          <>
            {t.kept(formatNumber(summary?.stored_low_cache_count ?? lowCacheRecords.length), formatNumber(maxRecords))}
            {summary?.persisted_path && (
              <>
                <span className="lg-num max-md:hidden" title={summary.persisted_path}>
                  {" · "}
                  {summary.persisted_path}
                </span>
              </>
            )}
          </>
        }
      >
        <div className="lg-toolbar">
          <Segmented<"all" | LowCacheHitReason>
            ariaLabel={t.reasonFilter}
            value={reason}
            onChange={(next) => {
              setReason(next);
              setPage(1);
            }}
            options={[
              { value: "all", label: t.all, count: lowCacheRecords.length },
              { value: "warm_cache_miss", label: m.cache.reason.warm_cache_miss, count: counts.warm_cache_miss },
              { value: "warm_partial_hit", label: t.partial, count: counts.warm_partial_hit },
              { value: "cold_session_start", label: m.cache.reason.cold_session_start, count: counts.cold_session_start },
            ]}
          />
          <SearchInput
            value={query}
            onChange={(next) => {
              setQuery(next);
              setPage(1);
            }}
            placeholder={t.search}
            className="w-full sm:w-64"
          />
        </div>
        {shown.length === 0 ? (
          <EmptyState
            icon={<Gauge />}
            title={lowCacheRecords.length === 0 ? t.none : t.noMatch}
            hint={lowCacheRecords.length === 0 ? t.noneHint(Math.round(thresholdBps / 100)) : t.noMatchHint}
          />
        ) : (
          <div className="lg-table-wrap">
            <table className="lg-table">
              <thead>
                <tr>
                  <th>{t.col.time}</th>
                  <th>{t.col.reason}</th>
                  <th>{t.col.hitRate}</th>
                  <th className="hide-sm">{t.col.modelSession}</th>
                  <th className="num hide-sm">{t.col.readOfTotal}</th>
                  <th className="num hide-sm">{t.col.elapsed}</th>
                  <th className="hide-sm">Prompt</th>
                </tr>
              </thead>
              <tbody>
                {shown.map((rec) => {
                  const metrics = rec.request_metrics;
                  return (
                    <tr
                      key={`${rec.task_id}-${rec.recorded_at_unix_ms}`}
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
                      <td>
                        <ReasonPills record={rec} />
                      </td>
                      <td>
                        <div className="flex items-center gap-2 md:w-[104px]">
                          <span className="lg-num w-11 text-[12px] font-medium text-fg">
                            {formatBpsPct(rec.cache_hit_rate_bps, 1)}
                          </span>
                          <span className="flex-1 max-md:hidden">
                            <Meter
                              ratio={rec.cache_hit_rate_bps / 10000}
                              tone={rec.cache_hit_rate_bps === 0 ? "err" : "warn"}
                              label={t.turnHitRate}
                            />
                          </span>
                        </div>
                      </td>
                      <td className="hide-sm">
                        <div className="lg-num text-[12px] text-fg">{rec.resolved_model}</div>
                        <div className="lg-num whitespace-nowrap text-[11.5px] text-fg-4">
                          {rec.session_id
                            ? `${rec.session_reused ? t.reused : t.created} ${shortId(rec.session_id)}`
                            : rec.task_id}
                          {metrics?.session_idle_ms != null && t.idle(formatDurationMs(metrics.session_idle_ms))}
                        </div>
                      </td>
                      <td className="num hide-sm whitespace-nowrap">
                        {formatNumber(rec.cached_read_tokens)}
                        <span className="text-fg-4"> / </span>
                        {formatNumber(rec.total_prompt_tokens)}
                      </td>
                      <td className="num hide-sm whitespace-nowrap">
                        {formatDurationMs(rec.elapsed_ms)}
                      </td>
                      <td className="hide-sm max-w-[280px]">
                        <span className="lg-clip text-[12px] text-fg-3" title={rec.prompt_preview}>
                          {rec.prompt_preview || "—"}
                        </span>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
        <Pager page={clampedPage} pages={pages} total={filtered.length} onPage={setPage} />
      </Card>

      <LowCacheSheet
        record={open}
        replayTools={selected === null && linked !== null}
        onClose={() => {
          setSelected(null);
          if (arg !== null) navigate("cache");
        }}
      />
    </>
  );
}
