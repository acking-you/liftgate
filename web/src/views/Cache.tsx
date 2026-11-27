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
      onBanner({ tone: "err", text: "保留条数需在 0 到 50,000 之间；设为 0 表示不记录。" });
      return;
    }
    if (!Number.isFinite(pct) || pct < 0 || pct > 100) {
      onBanner({ tone: "err", text: "阈值需在 0% 到 100% 之间。" });
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
        text: `已保存：最多保留 ${formatNumber(next.max_low_cache_records)} 条，命中率低于 ${Math.round(next.low_cache_threshold_bps / 100)}% 的轮次会被记录。`,
      });
      await Promise.all([refresh(), reloadLowCache()]);
    } catch (err) {
      onBanner({ tone: "err", text: `保存失败：${errorMessage(err)}` });
    } finally {
      setSaving(false);
    }
  };

  const toggleCapture = async (next: boolean): Promise<void> => {
    try {
      await client.updateCacheConfig({ capture_request_details: next });
      onBanner({ tone: "ok", text: `低命中轮次的完整请求内容保存已${next ? "开启" : "关闭"}。` });
      await refresh();
    } catch (err) {
      onBanner({ tone: "err", text: `切换失败：${errorMessage(err)}` });
    }
  };

  const clear = async (resetTotals: boolean): Promise<void> => {
    try {
      await client.clearLowCacheHits(resetTotals);
      onBanner({
        tone: "ok",
        text: resetTotals ? "已清空低命中记录并重置累计统计。" : "已清空低命中记录，累计统计保留。",
      });
      await Promise.all([refresh(), reloadLowCache()]);
    } catch (err) {
      onBanner({ tone: "err", text: `清理失败：${errorMessage(err)}` });
    }
  };

  return (
    <Card title="记录策略">
      <form className="flex flex-col gap-4" onSubmit={(ev) => void save(ev)}>
        <div className="grid grid-cols-2 gap-3">
          <Field label="最多保留条数" htmlFor="cache-max-records">
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
          <Field label="低命中阈值 %" htmlFor="cache-threshold">
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
          title="保存完整请求内容"
          hint="低命中时落盘实际发往 ACP 的 Prompt、消息与工具 Schema。"
          checked={capture}
          onChange={(next) => void toggleCapture(next)}
        />
        <div className="flex flex-wrap items-center justify-between gap-2 border-t border-line pt-3">
          <Button type="submit" variant="primary" disabled={saving || !dirty}>
            {saving ? "保存中…" : "保存"}
          </Button>
          <span className="flex flex-wrap items-center gap-1.5">
            <ConfirmButton
              label="清空记录"
              confirmLabel="确认清空记录"
              icon={<Trash2 />}
              disabled={(summary?.stored_low_cache_count ?? 0) === 0}
              onConfirm={() => void clear(false)}
            />
            <ConfirmButton
              label="重置累计统计"
              confirmLabel="确认清空并重置"
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
          title="累计命中"
          sub="缓存读 ÷ 全部输入，跨重启累计。"
          actions={
            (summary?.auxiliary_turns ?? 0) > 0 ? (
              <Tag title="标题生成等一次性后台请求，不进入命中率分母">
                已隔离辅助请求 {formatNumber(summary?.auxiliary_turns ?? 0)} 轮
              </Tag>
            ) : undefined
          }
        >
          <div className="flex flex-wrap items-end gap-x-10 gap-y-4">
            <div>
              <div className="lg-hero">{formatBpsPct(stats.hitBps, 1)}</div>
              <div className="mt-2 text-[12px] text-fg-3">
                全部 {formatNumber(stats.evaluatedTurns)} 轮
              </div>
            </div>
            <div className="min-w-[180px] flex-1">
              <div className="flex items-baseline justify-between gap-3 text-[12px] text-fg-3">
                <span>热会话续接（排除首轮冷启动）</span>
                <span className="lg-num text-[15px] font-semibold text-fg">
                  {formatBpsPct(stats.warmHitBps, 1)}
                </span>
              </div>
              <div className="mt-1.5">
                <Meter ratio={(stats.warmHitBps ?? 0) / 10000} label="热会话续接命中率" />
              </div>
              <div className="mt-1.5 text-[12px] text-fg-3">
                {formatNumber(stats.warmTurns)} 轮复用 · 累计 {formatNumber(summary?.total_low_cache_turns ?? lowCacheRecords.length)} 轮低于阈值
              </div>
            </div>
          </div>
          <div className="mt-5">
            <h3 className="lg-section-title">输入 Token 去向 · 共 {formatNumber(stats.promptTokens)}</h3>
            <SegmentedBar
              ariaLabel="累计输入 Token 构成"
              format={formatNumber}
              parts={[
                { key: "read", label: "缓存读", value: stats.cachedRead, color: "var(--lg-s1)" },
                { key: "write", label: "缓存写", value: stats.cachedWrite, color: "var(--lg-s4)", ink: "dark" },
                { key: "input", label: "未命中输入", value: stats.uncachedInput, color: "var(--lg-s-neutral)" },
              ]}
            />
          </div>
        </Card>

        <ConfigCard summary={summary} onBanner={setBanner} />
      </div>

      <Card
        flush
        title="低命中记录"
        sub={
          <>
            保留 {formatNumber(summary?.stored_low_cache_count ?? lowCacheRecords.length)} /{" "}
            {formatNumber(maxRecords)} 条
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
            ariaLabel="按原因筛选"
            value={reason}
            onChange={(next) => {
              setReason(next);
              setPage(1);
            }}
            options={[
              { value: "all", label: "全部", count: lowCacheRecords.length },
              { value: "warm_cache_miss", label: "热会话掉缓存", count: counts.warm_cache_miss },
              { value: "warm_partial_hit", label: "部分命中", count: counts.warm_partial_hit },
              { value: "cold_session_start", label: "冷启动", count: counts.cold_session_start },
            ]}
          />
          <SearchInput
            value={query}
            onChange={(next) => {
              setQuery(next);
              setPage(1);
            }}
            placeholder="搜索模型、会话或 Prompt"
            className="w-full sm:w-64"
          />
        </div>
        {shown.length === 0 ? (
          <EmptyState
            icon={<Gauge />}
            title={lowCacheRecords.length === 0 ? "没有低命中记录" : "没有符合条件的记录"}
            hint={
              lowCacheRecords.length === 0
                ? `任何一轮的缓存命中率低于 ${Math.round(thresholdBps / 100)}% 时，会被记录在这里并附上归因。`
                : "换一个原因分类，或清空搜索词。"
            }
          />
        ) : (
          <div className="lg-table-wrap">
            <table className="lg-table">
              <thead>
                <tr>
                  <th>时间</th>
                  <th>归因</th>
                  <th>命中率</th>
                  <th className="hide-sm">模型 · 会话</th>
                  <th className="num hide-sm">缓存读 / 总输入</th>
                  <th className="num hide-sm">耗时</th>
                  <th className="hide-sm">Prompt</th>
                </tr>
              </thead>
              <tbody>
                {shown.map((rec) => {
                  const m = rec.request_metrics;
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
                              label="单轮缓存命中率"
                            />
                          </span>
                        </div>
                      </td>
                      <td className="hide-sm">
                        <div className="lg-num text-[12px] text-fg">{rec.resolved_model}</div>
                        <div className="lg-num whitespace-nowrap text-[11.5px] text-fg-4">
                          {rec.session_id
                            ? `${rec.session_reused ? "复用" : "新建"} ${shortId(rec.session_id)}`
                            : rec.task_id}
                          {m?.session_idle_ms != null && ` · 空闲 ${formatDurationMs(m.session_idle_ms)}`}
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
