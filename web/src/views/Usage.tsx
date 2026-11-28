import * as React from "react";
import { useEffect, useMemo, useState } from "react";
import {
  CalendarDays,
  ChevronLeft,
  ChevronRight,
  CircleAlert,
  CircleCheck,
  CircleDollarSign,
  Coins,
  ExternalLink,
  Pencil,
  RotateCcw,
  Undo2,
  X,
} from "lucide-react";
import { type DayBar, DayBars, SegmentedBar } from "@/components/charts";
import { Modal } from "@/components/ui/dialog";
import {
  Button,
  Card,
  ConfirmButton,
  EmptyState,
  Field,
  Loading,
  Meter,
  Note,
  Pill,
  ProviderBadge,
  Segmented,
  Stat,
  StatGrid,
  Tag,
  Unsupported,
  Usd,
} from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { useMediaQuery } from "@/hooks/useMediaQuery";
import { type Resource, useRevisionResource } from "@/hooks/useResource";
import { useToday } from "@/hooks/useToday";
import { errorMessage, type UsageRange } from "@/lib/api";
import {
  addDays,
  dayBounds,
  dayKey,
  daysBetween,
  formatDayLong,
  formatDayMonth,
  formatDayShort,
  parseDayKey,
  startOfDay,
} from "@/lib/days";
import {
  formatCompact,
  formatDateTime,
  formatNumber,
  formatPct,
  formatPrice,
  formatUsd,
} from "@/lib/format";
import { useI18n } from "@/lib/i18n";
import type {
  ModelPrice,
  ModelPricingEntry,
  ModelUsage,
  PricingResponse,
  ProviderUsage,
  UsageInterval,
  UsageSeriesResponse,
  UsageTokens,
} from "@/types/telemetry";
import type { ViewProps } from "@/views/types";

type Banner = { tone: "ok" | "err"; text: string } | null;

/** A failed fetch, said plainly, with a way to try again. */
function FetchError({ error, onRetry }: { error: string | null; onRetry: () => void }): React.JSX.Element {
  const { m } = useI18n();
  return (
    <div className="px-4 pb-4">
      <Note tone="err" icon={<CircleAlert />}>
        {m.usage.loadFailed(error ?? "")}
        <Button size="sm" className="ml-2" onClick={onRetry}>
          {m.usage.retry}
        </Button>
      </Note>
    </div>
  );
}

const PRICE_FIELDS = ["input", "cached_input", "cache_write", "output"] as const;
type PriceField = (typeof PRICE_FIELDS)[number];

function totalTokens(tokens: UsageTokens): number {
  return tokens.input + tokens.cached_input + tokens.cache_write + tokens.output;
}

function promptTokens(tokens: UsageTokens): number {
  return tokens.input + tokens.cached_input + tokens.cache_write;
}

const EFFORT_ORDER = ["minimal", "low", "medium", "high", "xhigh", "max"];

/** `gemini-4-neon-high`, `gemini-4-neon-low` → efforts `low`, `high`; aliases stay whole. */
function compactModelIds(key: string, ids: readonly string[]): { efforts: string[]; others: string[] } {
  const prefix = `${key}-`;
  const efforts: string[] = [];
  const others: string[] = [];
  for (const id of ids) {
    if (id.startsWith(prefix)) efforts.push(id.slice(prefix.length));
    else others.push(id);
  }
  const rank = (effort: string): number => {
    const idx = EFFORT_ORDER.indexOf(effort);
    return idx === -1 ? EFFORT_ORDER.length : idx;
  };
  efforts.sort((a, b) => rank(a) - rank(b) || a.localeCompare(b));
  return { efforts, others };
}

function ModelIds({ entry }: { entry: ModelPricingEntry }): React.JSX.Element {
  const { m } = useI18n();
  if (entry.model_ids.length === 0) return <span className="text-fg-4">{m.usage.noModels}</span>;
  const { efforts, others } = compactModelIds(entry.key, entry.model_ids);
  return (
    <span className="inline-flex flex-wrap items-center gap-1" title={entry.model_ids.join("\n")}>
      {efforts.map((effort) => (
        <Tag key={effort}>{effort}</Tag>
      ))}
      {others.map((id) => (
        <span key={id} className="lg-num text-[11.5px] text-fg-3">
          {id}
        </span>
      ))}
    </span>
  );
}

function effectivePrice(entry: ModelPricingEntry | undefined): ModelPrice | null {
  return entry?.custom_price ?? entry?.default_price ?? null;
}

function PriceEditor({
  entry,
  onClose,
  onSaved,
}: {
  entry: ModelPricingEntry | null;
  onClose: () => void;
  onSaved: (response: PricingResponse, text: string) => void;
}): React.JSX.Element {
  const { client } = useGateway();
  const { m } = useI18n();
  const t = m.usage.editor;
  const [draft, setDraft] = useState<Record<PriceField, string>>({
    input: "",
    cached_input: "",
    cache_write: "",
    output: "",
  });
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!entry) return;
    const base = effectivePrice(entry);
    setDraft({
      input: base ? String(base.input) : "",
      cached_input: base ? String(base.cached_input) : "",
      cache_write: base ? String(base.cache_write) : "",
      output: base ? String(base.output) : "",
    });
    setError(null);
  }, [entry]);

  const labels: Record<PriceField, string> = {
    input: t.input,
    cached_input: t.cached,
    cache_write: t.write,
    output: t.output,
  };

  const submit = async (price: ModelPrice | null): Promise<void> => {
    if (!entry) return;
    setBusy(true);
    setError(null);
    try {
      const response = await client.setModelPrice(entry.key, price);
      onSaved(response, price ? t.saved : entry.default_price ? t.restored : t.removed);
      onClose();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const save = (ev: React.FormEvent): void => {
    ev.preventDefault();
    const parsed: Partial<ModelPrice> = {};
    for (const field of PRICE_FIELDS) {
      const value = Number(draft[field].trim());
      if (draft[field].trim() === "" || !Number.isFinite(value) || value < 0 || value > 10_000) {
        setError(t.invalid);
        return;
      }
      parsed[field] = value;
    }
    void submit(parsed as ModelPrice);
  };

  return (
    <Modal
      open={entry !== null}
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      title={entry ? t.title(entry.label ?? entry.key) : ""}
      description={t.description}
      className="max-w-[480px]"
    >
      {entry && (
        <form className="flex flex-col gap-3.5" onSubmit={save}>
          <div className="grid grid-cols-2 gap-3">
            {PRICE_FIELDS.map((field) => (
              <Field
                key={field}
                label={labels[field]}
                htmlFor={`price-${field}`}
                hint={entry.default_price ? t.defaultHint(formatPrice(entry.default_price[field])) : undefined}
              >
                <input
                  id={`price-${field}`}
                  className="lg-input lg-num"
                  inputMode="decimal"
                  value={draft[field]}
                  onChange={(ev) => setDraft((prev) => ({ ...prev, [field]: ev.target.value }))}
                />
              </Field>
            ))}
          </div>
          {error && (
            <Note tone="err" icon={<CircleAlert />}>
              {error}
            </Note>
          )}
          <div className="flex flex-wrap items-center justify-between gap-2 pt-1">
            {entry.custom_price ? (
              // Without a default this is the only way back to "unpriced" (and to free a slot).
              <Button variant="ghost" disabled={busy} onClick={() => void submit(null)}>
                <Undo2 />
                {entry.default_price ? t.restore : t.remove}
              </Button>
            ) : (
              <span />
            )}
            <span className="flex gap-2">
              <Button onClick={onClose}>{m.common.cancel}</Button>
              <Button type="submit" variant="primary" disabled={busy}>
                {m.common.save}
              </Button>
            </span>
          </div>
        </form>
      )}
    </Modal>
  );
}

function ModelTable({
  models,
  pricing,
  totalCost,
  onSetPrice,
}: {
  models: ModelUsage[];
  pricing: Map<string, ModelPricingEntry>;
  totalCost: number;
  onSetPrice: (entry: ModelPricingEntry) => void;
}): React.JSX.Element {
  const { m } = useI18n();
  const t = m.usage;
  const showWrite = models.some((row) => row.tokens.cache_write > 0);
  const sum = models.reduce<UsageTokens>(
    (acc, row) => ({
      input: acc.input + row.tokens.input,
      cached_input: acc.cached_input + row.tokens.cached_input,
      cache_write: acc.cache_write + row.tokens.cache_write,
      output: acc.output + row.tokens.output,
      thought: acc.thought + row.tokens.thought,
    }),
    { input: 0, cached_input: 0, cache_write: 0, output: 0, thought: 0 },
  );
  const calls = models.reduce((acc, row) => acc + row.calls, 0);

  return (
    <div className="lg-table-wrap">
      <table className="lg-table">
        <thead>
          <tr>
            <th>{t.col.model}</th>
            <th className="num">{t.col.calls}</th>
            <th className="num hide-sm">{t.col.input}</th>
            <th className="num hide-sm">{t.col.cached}</th>
            {showWrite && <th className="num hide-sm">{t.col.write}</th>}
            <th className="num hide-sm">{t.col.output}</th>
            <th className="num">{t.col.total}</th>
            <th className="num">{t.col.cost}</th>
            <th className="hide-sm">{t.col.share}</th>
          </tr>
        </thead>
        <tbody>
          {models.map((row) => {
            const entry = pricing.get(row.pricing_key);
            const share = row.cost_usd !== null && totalCost > 0 ? row.cost_usd / totalCost : 0;
            return (
              <tr key={row.model_id} data-tone={row.cost_usd === null ? "warn" : undefined}>
                <td className="text-[12px]">
                  <div className="lg-num text-fg">{row.model_id}</div>
                  {entry?.label && <div className="text-[11.5px] text-fg-4">{entry.label}</div>}
                </td>
                <td className="num">
                  {formatNumber(row.calls)}
                  {row.auxiliary_calls > 0 && (
                    <span className="text-fg-4" title={t.auxiliaryCalls(formatNumber(row.auxiliary_calls))}>
                      {" "}
                      · {formatNumber(row.auxiliary_calls)}
                    </span>
                  )}
                </td>
                <td className="num hide-sm">{formatCompact(row.tokens.input)}</td>
                <td className="num hide-sm">{formatCompact(row.tokens.cached_input)}</td>
                {showWrite && <td className="num hide-sm">{formatCompact(row.tokens.cache_write)}</td>}
                <td className="num hide-sm">{formatCompact(row.tokens.output)}</td>
                <td className="num">{formatCompact(totalTokens(row.tokens))}</td>
                <td className="num whitespace-nowrap">
                  {row.cost_usd === null && entry ? (
                    <button type="button" className="lg-link" onClick={() => onSetPrice(entry)}>
                      {t.setPrice}
                    </button>
                  ) : (
                    <Usd value={row.cost_usd} unpricedHint />
                  )}
                </td>
                <td className="hide-sm min-w-[120px]">
                  <div className="flex items-center gap-2">
                    <div className="flex-1">
                      <Meter ratio={share} label={t.col.share} />
                    </div>
                    <span className="lg-num w-10 text-right text-[11.5px] text-fg-3">{formatPct(share)}</span>
                  </div>
                </td>
              </tr>
            );
          })}
        </tbody>
        <tfoot>
          <tr>
            <td className="font-medium text-fg">{t.totalRow}</td>
            <td className="num font-medium">{formatNumber(calls)}</td>
            <td className="num hide-sm">{formatCompact(sum.input)}</td>
            <td className="num hide-sm">{formatCompact(sum.cached_input)}</td>
            {showWrite && <td className="num hide-sm">{formatCompact(sum.cache_write)}</td>}
            <td className="num hide-sm">{formatCompact(sum.output)}</td>
            <td className="num font-medium">{formatCompact(totalTokens(sum))}</td>
            <td className="num font-semibold text-fg">{formatUsd(totalCost)}</td>
            <td className="hide-sm" />
          </tr>
        </tfoot>
      </table>
    </div>
  );
}

function PricingTable({
  pricing,
  onEdit,
}: {
  pricing: PricingResponse;
  onEdit: (entry: ModelPricingEntry) => void;
}): React.JSX.Element {
  const { m } = useI18n();
  const t = m.usage;
  return (
    <div className="lg-table-wrap">
      <table className="lg-table">
        <thead>
          <tr>
            <th>{t.pcol.family}</th>
            <th className="hide-sm">{t.pcol.models}</th>
            <th className="num">{t.pcol.input}</th>
            <th className="num">{t.pcol.cached}</th>
            <th className="num hide-sm">{t.pcol.write}</th>
            <th className="num">{t.pcol.output}</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {pricing.entries.map((entry) => {
            const price = effectivePrice(entry);
            const next = entry.custom_price ? undefined : entry.upcoming_defaults[0];
            return (
              <tr key={entry.key} data-tone={price ? undefined : "warn"}>
                <td className="text-[12px]">
                  <div className="flex flex-wrap items-center gap-1.5">
                    <span className="font-medium text-fg">{entry.label ?? entry.key}</span>
                    {entry.custom_price ? (
                      <Pill tone="accent">{t.custom}</Pill>
                    ) : price ? null : (
                      <Pill tone="warn">{t.noPrice}</Pill>
                    )}
                  </div>
                  {entry.label && <div className="lg-num text-[11.5px] text-fg-4">{entry.key}</div>}
                  {next && (
                    <div className="mt-0.5 text-[11.5px] text-warn">
                      {t.upcoming(
                        dayKey(next.effective_from_unix_ms),
                        formatPrice(next.price.input),
                        formatPrice(next.price.output),
                      )}
                    </div>
                  )}
                </td>
                <td className="hide-sm max-w-[320px]">
                  <ModelIds entry={entry} />
                </td>
                <td className="num">{price ? formatPrice(price.input) : "—"}</td>
                <td className="num">{price ? formatPrice(price.cached_input) : "—"}</td>
                <td className="num hide-sm">{price ? formatPrice(price.cache_write) : "—"}</td>
                <td className="num">{price ? formatPrice(price.output) : "—"}</td>
                <td className="num whitespace-nowrap">
                  {entry.source_url && (
                    <a
                      className="lg-btn mr-1 max-sm:hidden"
                      data-size="sm"
                      data-variant="ghost"
                      data-icon="true"
                      href={entry.source_url}
                      target="_blank"
                      rel="noreferrer"
                      title={t.source}
                      aria-label={t.source}
                    >
                      <ExternalLink />
                    </a>
                  )}
                  <Button size="sm" variant="ghost" onClick={() => onEdit(entry)} aria-label={t.edit}>
                    <Pencil />
                    <span className="max-sm:hidden">{price ? t.edit : t.setPrice}</span>
                  </Button>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

type PeriodId = "all" | "today" | "yesterday" | "7d" | "30d";

interface Period {
  /** `day` is a single day picked in the chart or named in the URL. */
  id: PeriodId | "day";
  range: UsageRange | null;
  /** Start of the period when it is a single day. */
  day: number | null;
}

/** URL argument selecting the single day starting at `day`. */
function argForDay(day: number, today: number): string {
  if (day === today) return "today";
  if (day === addDays(today, -1)) return "yesterday";
  return dayKey(day);
}

/** The period `#/usage/<arg>` names; anything else means everything recorded. */
function periodOf(arg: string | null, today: number): Period {
  const single = (id: Period["id"], day: number): Period => ({
    id,
    range: { from: day, to: addDays(day, 1) },
    day,
  });
  switch (arg) {
    case "today":
      return single("today", today);
    case "yesterday":
      return single("yesterday", addDays(today, -1));
    case "7d":
      return { id: "7d", range: { from: addDays(today, -6), to: addDays(today, 1) }, day: null };
    case "30d":
      return { id: "30d", range: { from: addDays(today, -29), to: addDays(today, 1) }, day: null };
    default: {
      const day = arg ? parseDayKey(arg) : null;
      if (day === null) return { id: "all", range: null, day: null };
      const id = argForDay(day, today);
      return single(id === "today" || id === "yesterday" ? id : "day", day);
    }
  }
}

/** Chart page (`size` days each, newest first) holding `day`; today's page without one. */
function chartPageOf(day: number | null, today: number, size: number): number {
  return day === null ? 0 : Math.max(0, Math.floor(daysBetween(day, today) / size));
}

/** The latest non-null value, so a period switch dims the old figures instead of blanking them. */
function useLastPresent<T>(value: T | null): T | null {
  const [last, setLast] = useState<T | null>(value);
  if (value !== null && value !== last) setLast(value);
  return value ?? last;
}

type Metric = "cost" | "tokens" | "calls" | "cache";

function cacheShareOf(tokens: UsageTokens): number | null {
  const prompt = promptTokens(tokens);
  return prompt > 0 ? tokens.cached_input / prompt : null;
}

function metricValue(metric: Metric, day: UsageInterval): number {
  switch (metric) {
    case "cost":
      return day.cost_usd;
    case "tokens":
      return totalTokens(day.tokens);
    case "calls":
      return day.calls;
    case "cache":
      return cacheShareOf(day.tokens) ?? 0;
  }
}

function formatMetric(metric: Metric, value: number): string {
  switch (metric) {
    case "cost":
      return formatUsd(value);
    case "tokens":
      return formatCompact(value);
    case "calls":
      return formatNumber(value);
    case "cache":
      return formatPct(value, 0);
  }
}

/** The smallest 1-1.5-2-2.5-3-4-5-6-8 × 10ⁿ at or above `value`, for a tidy top gridline. */
function niceCeil(value: number): number {
  if (!(value > 0)) return 0;
  const magnitude = 10 ** Math.floor(Math.log10(value));
  const step = [1, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10].find((s) => s * magnitude >= value * (1 - 1e-9)) ?? 10;
  return step * magnitude;
}

/** Tick every 7th day back from the newest, plus the oldest when it is not crowded. */
function dayTicks(count: number): number[] {
  const ticks: number[] = [];
  for (let i = count - 1; i >= 0; i -= 7) ticks.unshift(i);
  if (ticks[0] !== undefined && ticks[0] >= 3) ticks.unshift(0);
  return ticks;
}

function DailyUsage({
  series,
  bounds,
  today,
  period,
  historyFrom,
  pageSize,
  page,
  onPage,
  onPick,
}: {
  series: Resource<UsageSeriesResponse>;
  bounds: number[];
  today: number;
  period: Period;
  historyFrom: number;
  pageSize: number;
  page: number;
  onPage: (page: number) => void;
  onPick: (day: number) => void;
}): React.JSX.Element | null {
  const { m, locale } = useI18n();
  const t = m.usage;
  const [metric, setMetric] = useState<Metric>("cost");
  const intervals = series.data?.intervals ?? [];
  const from = series.data?.history_from_unix_ms ?? historyFrom;
  const first = bounds[0] ?? today;
  const last = addDays(bounds[bounds.length - 1] ?? addDays(today, 1), -1);
  const known = intervals.filter((day) => day.end_unix_ms > from);
  const total = known.reduce(
    (acc, day) => ({
      cost: acc.cost + day.cost_usd,
      calls: acc.calls + day.calls,
      tokens: {
        input: acc.tokens.input + day.tokens.input,
        cached_input: acc.tokens.cached_input + day.tokens.cached_input,
        cache_write: acc.tokens.cache_write + day.tokens.cache_write,
        output: acc.tokens.output + day.tokens.output,
        thought: acc.tokens.thought + day.tokens.thought,
      },
    }),
    { cost: 0, calls: 0, tokens: { input: 0, cached_input: 0, cache_write: 0, output: 0, thought: 0 } },
  );
  const totalValue =
    metric === "cost"
      ? total.cost
      : metric === "calls"
        ? total.calls
        : metric === "tokens"
          ? totalTokens(total.tokens)
          : (cacheShareOf(total.tokens) ?? 0);
  const max = metric === "cache" ? 1 : niceCeil(Math.max(0, ...known.map((day) => metricValue(metric, day))));
  const span = t.period.span(formatDayMonth(first, locale), formatDayMonth(last, locale));
  const canGoBack = first > startOfDay(from);

  const bars: DayBar[] = intervals.map((day) => {
    const start = day.start_unix_ms;
    const unknown = day.end_unix_ms <= from;
    const value = metricValue(metric, day);
    const inPeriod = period.range === null || (start >= period.range.from && start < period.range.to);
    return {
      key: dayKey(start),
      value: unknown ? 0 : value,
      unknown,
      muted: !inPeriod,
      pressed: period.day === start,
      label: t.daily.bar(formatDayLong(start, locale), unknown ? t.daily.noDetail : formatMetric(metric, value)),
    };
  });

  // A gateway without the series endpoint simply has no daily chart.
  if (series.status === "unsupported") return null;

  const tip = (i: number): React.ReactNode => {
    const day = intervals[i];
    if (!day) return null;
    const head = <div className="font-medium text-fg">{formatDayLong(day.start_unix_ms, locale)}</div>;
    if (day.end_unix_ms <= from) {
      return (
        <>
          {head}
          <div className="mt-1 text-fg-3">{t.daily.noDetail}</div>
        </>
      );
    }
    return (
      <>
        {head}
        <div className="lg-num mt-1 text-[15px] font-semibold text-fg">{formatUsd(day.cost_usd)}</div>
        <div className="lg-num mt-0.5 text-fg-3">
          {t.daily.tokens(formatCompact(totalTokens(day.tokens)), formatPct(cacheShareOf(day.tokens), 0))}
        </div>
        <div className="lg-num mt-0.5 text-fg-3">
          {t.daily.calls(formatNumber(day.calls))}
          {day.unpriced_calls > 0 && (
            <span className="text-warn"> · {t.daily.unpriced(formatNumber(day.unpriced_calls))}</span>
          )}
        </div>
      </>
    );
  };

  return (
    <Card
      title={t.daily.title}
      sub={series.data ? t.daily.sub(span, formatMetric(metric, totalValue)) : span}
      actions={
        <>
          <Segmented<Metric>
            value={metric}
            onChange={setMetric}
            ariaLabel={t.daily.metricAria}
            options={[
              { value: "cost", label: t.daily.metric.cost },
              { value: "tokens", label: t.daily.metric.tokens },
              { value: "calls", label: t.daily.metric.calls },
              { value: "cache", label: t.daily.metric.cache },
            ]}
          />
          <span className="flex">
            <Button
              size="sm"
              variant="ghost"
              icon
              disabled={!canGoBack}
              aria-label={t.daily.earlier(pageSize)}
              title={t.daily.earlier(pageSize)}
              onClick={() => onPage(page + 1)}
            >
              <ChevronLeft />
            </Button>
            <Button
              size="sm"
              variant="ghost"
              icon
              disabled={page === 0}
              aria-label={t.daily.later(pageSize)}
              title={t.daily.later(pageSize)}
              onClick={() => onPage(Math.max(0, page - 1))}
            >
              <ChevronRight />
            </Button>
          </span>
        </>
      }
    >
      {series.status === "error" && !series.data ? (
        <FetchError error={series.error} onRetry={series.reload} />
      ) : !series.data ? (
        <div style={{ height: 161 }}>
          <Loading />
        </div>
      ) : (
        <DayBars
          bars={bars}
          max={max}
          maxLabel={formatMetric(metric, max)}
          ticks={dayTicks(bars.length).map((index) => {
            const start = intervals[index]?.start_unix_ms ?? 0;
            return { index, label: start === today ? t.daily.today : formatDayShort(start) };
          })}
          ariaLabel={t.daily.chartAria}
          onSelect={(i) => {
            const day = intervals[i];
            if (day) onPick(day.start_unix_ms);
          }}
          renderTip={tip}
        />
      )}
    </Card>
  );
}

export function UsageView({ arg, navigate }: ViewProps): React.JSX.Element {
  const { client, snapshot, baseUrl, refresh } = useGateway();
  const { m, locale } = useI18n();
  const t = m.usage;
  const today = useToday();
  const period = useMemo(() => periodOf(arg, today), [arg, today]);
  const range = period.range;
  const pageSize = useMediaQuery("(max-width: 640px)") ? 14 : 30;
  const [page, setPage] = useState(() => chartPageOf(period.day, today, pageSize));
  // Show the page holding a picked day; any other period starts from today.
  useEffect(() => {
    setPage(chartPageOf(period.day, today, pageSize));
  }, [period.day, today, pageSize]);

  const summary = snapshot?.usage;
  const revision = summary?.revision ?? -1;
  const supported = snapshot === null ? null : summary !== undefined;
  const key = `${baseUrl}|${client.demo}`;
  const usage = useRevisionResource(
    supported ? () => client.usage(range ?? undefined) : null,
    `${key}|usage|${range ? `${range.from}-${range.to}` : "all"}`,
    revision,
  );
  const windowStart = addDays(today, 1 - (page + 1) * pageSize);
  const bounds = useMemo(() => dayBounds(windowStart, pageSize), [windowStart, pageSize]);
  const series = useRevisionResource(
    supported ? () => client.usageSeries(bounds) : null,
    `${key}|series|${windowStart}|${pageSize}`,
    revision,
  );
  const pricingResource = useRevisionResource(
    supported ? () => client.pricing() : null,
    `${key}|pricing`,
    revision,
  );
  // The saved price list, shown until the refetch it triggers lands, and only for its gateway.
  const [override, setOverride] = useState<{
    key: string;
    base: PricingResponse | null;
    data: PricingResponse;
  } | null>(null);
  const [editing, setEditing] = useState<ModelPricingEntry | null>(null);
  const [banner, setBanner] = useState<Banner>(null);

  const pricing =
    override && override.key === key && pricingResource.data === override.base
      ? override.data
      : pricingResource.data;
  const byKey = useMemo(
    () => new Map((pricing?.entries ?? []).map((entry) => [entry.key, entry])),
    [pricing],
  );

  const lastReport = useLastPresent(usage.data);
  const report = usage.status === "error" ? usage.data : lastReport;
  // Figures of the previous period stay on screen, dimmed, until the new ones land.
  const stale = usage.data === null && report !== null;
  const totals = range ? report?.summary : (usage.data?.summary ?? summary);
  const since = summary?.since_unix_ms ?? 0;
  const historyFrom = summary?.history_from_unix_ms ?? since;

  if (supported === false) {
    return (
      <Card>
        <Unsupported feature={t.unsupportedFeature} endpoint="GET /api/telemetry/usage" />
      </Card>
    );
  }
  if (!summary) return <Loading />;

  const tomorrow = addDays(today, 1);
  const pick = (day: number): void =>
    navigate("usage", period.day === day ? null : argForDay(day, today));
  const dim = stale ? "opacity-60 transition-opacity" : undefined;

  let costFoot: React.ReactNode = <span>{t.since(formatDateTime(since))}</span>;
  if (totals && totals.unpriced_calls > 0) {
    costFoot = <span className="text-warn">{t.unpricedCalls(formatNumber(totals.unpriced_calls))}</span>;
  } else if (totals && period.day !== null) {
    const before = addDays(period.day, -1);
    const prev = series.data?.intervals.find((day) => day.start_unix_ms === before);
    // The date itself already sits next to the period picker.
    costFoot = <span>{prev && prev.end_unix_ms > historyFrom ? t.previousDay(formatUsd(prev.cost_usd)) : "—"}</span>;
  } else if (totals && range) {
    const days = Math.max(1, daysBetween(Math.max(range.from, startOfDay(since)), Math.min(range.to, tomorrow)));
    costFoot = <span>{t.dailyAverage(formatUsd(totals.cost_usd / days))}</span>;
  }

  const rangeLabel =
    period.id === "today" || period.id === "yesterday"
      ? formatDayLong(period.day ?? today, locale)
      : period.id === "7d" || period.id === "30d"
        ? t.period.span(formatDayMonth(range?.from ?? today, locale), formatDayMonth(today, locale))
        : null;

  const prompt = totals ? promptTokens(totals.tokens) : 0;
  const cacheShare = totals && prompt > 0 ? totals.tokens.cached_input / prompt : null;
  const saved = totals?.cache_savings_usd ?? null;
  const auxiliary = report?.models.reduce((acc, row) => acc + row.auxiliary_calls, 0) ?? 0;

  return (
    <>
      {banner && (
        <Note tone={banner.tone} icon={banner.tone === "ok" ? <CircleCheck /> : <CircleAlert />}>
          {banner.text}
        </Note>
      )}

      <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
        <Segmented<PeriodId | "day">
          value={period.id}
          onChange={(id) => {
            setPage(0);
            navigate("usage", id === "all" ? null : id);
          }}
          ariaLabel={t.period.aria}
          options={[
            { value: "today", label: t.period.today },
            { value: "yesterday", label: t.period.yesterday },
            { value: "7d", label: t.period.last7 },
            { value: "30d", label: t.period.last30 },
            { value: "all", label: t.period.all },
          ]}
        />
        {period.id === "day" && period.day !== null && (
          <Pill tone="accent">
            <CalendarDays className="h-3.5 w-3.5" />
            {formatDayLong(period.day, locale)}
            <button
              type="button"
              className="-mr-1 ml-0.5 inline-flex cursor-pointer rounded border-0 bg-transparent p-0.5 text-current hover:bg-hover"
              aria-label={t.period.clear}
              title={t.period.clear}
              onClick={() => navigate("usage", null)}
            >
              <X className="h-3 w-3" />
            </button>
          </Pill>
        )}
        {rangeLabel && <span className="text-[12px] text-fg-3">{rangeLabel}</span>}
      </div>
      {range && historyFrom > since && range.from < historyFrom && (
        <p className="lg-hint m-0">{t.partialRange(formatDayMonth(historyFrom, locale))}</p>
      )}

      {!totals ? (
        usage.status === "error" ? (
          <Card>
            <FetchError error={usage.error} onRetry={usage.reload} />
          </Card>
        ) : (
          <Loading />
        )
      ) : (
        <div className={dim} aria-busy={stale}>
          <StatGrid>
            <Stat
              label={range ? t.cost : t.totalCost}
              icon={<CircleDollarSign />}
              value={formatUsd(totals.cost_usd)}
              foot={costFoot}
            />
            <Stat
              label={t.calls}
              icon={<Coins />}
              value={formatNumber(totals.calls)}
              foot={<span>{auxiliary > 0 ? t.auxiliaryCalls(formatNumber(auxiliary)) : t.noAuxiliary}</span>}
            />
            <Stat
              label={t.tokens}
              value={formatCompact(totalTokens(totals.tokens))}
              foot={
                <span>{totals.calls > 0 ? t.perCall(formatUsd(totals.cost_usd / totals.calls)) : "—"}</span>
              }
            />
            <Stat
              label={t.cacheShare}
              value={formatPct(cacheShare, 1)}
              foot={<span>{saved !== null && saved > 0 ? t.saved(formatUsd(saved)) : "—"}</span>}
            >
              <Meter ratio={cacheShare ?? 0} label={t.cacheShare} />
            </Stat>
          </StatGrid>
        </div>
      )}

      <DailyUsage
        series={series}
        bounds={bounds}
        today={today}
        period={period}
        historyFrom={historyFrom}
        pageSize={pageSize}
        page={page}
        onPage={setPage}
        onPick={pick}
      />

      {totals && (
        <Card title={t.mixTitle} sub={t.mixSub} className={dim}>
          <SegmentedBar
            ariaLabel={t.mixAria}
            format={formatCompact}
            parts={[
              { key: "input", label: t.part.input, value: totals.tokens.input, color: "var(--lg-s-neutral)" },
              { key: "cached", label: t.part.cached, value: totals.tokens.cached_input, color: "var(--lg-s1)" },
              {
                key: "write",
                label: t.part.write,
                value: totals.tokens.cache_write,
                color: "var(--lg-s4)",
                ink: "dark",
              },
              { key: "output", label: t.part.output, value: totals.tokens.output, color: "var(--lg-s2)" },
            ]}
          />
          {totals.tokens.thought > 0 && (
            <p className="lg-hint m-0 mt-2">
              {t.thinkingShare(<span className="lg-num text-fg-2">{formatNumber(totals.tokens.thought)}</span>)}
            </p>
          )}
        </Card>
      )}

      {report && (report.providers ?? report.summary.by_provider ?? []).length > 0 && (
        <Card flush title={t.byProvider} sub={t.byProviderSub} className={dim}>
          <div className="lg-table-wrap">
            <table className="lg-table">
              <thead>
                <tr>
                  <th>{t.col.provider}</th>
                  <th className="num">{t.col.calls}</th>
                  <th className="num hide-sm">{t.col.input}</th>
                  <th className="num hide-sm">{t.col.cached}</th>
                  <th className="num hide-sm">{t.col.output}</th>
                  <th className="num">{t.col.total}</th>
                  <th className="num hide-sm">{t.col.savings}</th>
                  <th className="num">{t.col.cost}</th>
                  <th className="hide-sm w-[140px]">{t.col.share}</th>
                </tr>
              </thead>
              <tbody>
                {(report.providers ?? report.summary.by_provider ?? []).map((p: ProviderUsage) => {
                  const share = report.summary.cost_usd > 0 ? p.cost_usd / report.summary.cost_usd : 0;
                  return (
                    <tr key={p.provider_id}>
                      <td>
                        <ProviderBadge id={p.provider_id} />
                      </td>
                      <td className="num">{formatNumber(p.calls)}</td>
                      <td className="num hide-sm">{formatCompact(p.tokens.input)}</td>
                      <td className="num hide-sm">{formatCompact(p.tokens.cached_input)}</td>
                      <td className="num hide-sm">{formatCompact(p.tokens.output)}</td>
                      <td className="num">{formatCompact(totalTokens(p.tokens))}</td>
                      <td className="num hide-sm text-ok-fg">{formatUsd(p.cache_savings_usd)}</td>
                      <td className="num font-medium text-fg">{formatUsd(p.cost_usd)}</td>
                      <td className="hide-sm">
                        <Meter ratio={share} label={`${p.provider_id} share`} />
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </Card>
      )}

      <Card
        flush
        title={t.byModel}
        sub={t.byModelSub}
        className={dim}
        actions={
          summary.calls > 0 ? (
            <ConfirmButton
              label={t.reset}
              confirmLabel={t.resetConfirm}
              icon={<RotateCcw />}
              onConfirm={() => {
                void client.resetUsage().then(
                  async () => {
                    setBanner({ tone: "ok", text: t.resetDone });
                    await refresh();
                    usage.reload();
                    series.reload();
                  },
                  (err: unknown) => setBanner({ tone: "err", text: errorMessage(err) }),
                );
              }}
            />
          ) : undefined
        }
      >
        {usage.status === "error" && !report ? (
          <FetchError error={usage.error} onRetry={usage.reload} />
        ) : !report ? (
          <Loading />
        ) : report.models.length === 0 ? (
          range ? (
            <EmptyState icon={<CalendarDays />} title={t.rangeEmpty} hint={t.rangeEmptyHint} />
          ) : (
            <EmptyState icon={<Coins />} title={t.empty} hint={t.emptyHint} />
          )
        ) : (
          <ModelTable
            models={report.models}
            pricing={byKey}
            totalCost={report.summary.cost_usd}
            onSetPrice={setEditing}
          />
        )}
      </Card>

      <Card flush title={t.pricing} sub={pricing ? t.pricingSub(pricing.defaults_as_of) : undefined}>
        {pricing ? (
          <PricingTable pricing={pricing} onEdit={setEditing} />
        ) : pricingResource.status === "error" ? (
          <FetchError error={pricingResource.error} onRetry={pricingResource.reload} />
        ) : (
          <Loading />
        )}
      </Card>

      <PriceEditor
        entry={editing}
        onClose={() => setEditing(null)}
        onSaved={(response, text) => {
          setOverride({ key, base: pricingResource.data, data: response });
          setBanner({ tone: "ok", text });
          pricingResource.reload();
          usage.reload();
          series.reload();
          void refresh();
        }}
      />
    </>
  );
}
