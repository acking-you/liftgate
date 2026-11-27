import * as React from "react";
import { useEffect, useMemo, useState } from "react";
import {
  CircleAlert,
  CircleCheck,
  CircleDollarSign,
  Coins,
  ExternalLink,
  Pencil,
  RotateCcw,
  Undo2,
} from "lucide-react";
import { SegmentedBar } from "@/components/charts";
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
  Stat,
  StatGrid,
  Tag,
  Unsupported,
  Usd,
} from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { useRevisionResource } from "@/hooks/useResource";
import { errorMessage } from "@/lib/api";
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

/** `YYYY-MM-DD` in local time. */
function formatDay(ms: number): string {
  const d = new Date(ms);
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
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
                        formatDay(next.effective_from_unix_ms),
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

export function UsageView(_props: ViewProps): React.JSX.Element {
  const { client, snapshot, baseUrl, refresh } = useGateway();
  const { m } = useI18n();
  const t = m.usage;
  const summary = snapshot?.usage;
  const revision = summary?.revision ?? -1;
  const supported = snapshot === null ? null : summary !== undefined;
  const key = `${baseUrl}|${client.demo}`;
  const usage = useRevisionResource(
    supported ? () => client.usage() : null,
    `${key}|usage`,
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

  const report = usage.data;
  const totals = report?.summary ?? summary;
  // Priced per price period on the gateway, like cost, so scheduled changes never re-price it.
  const saved = totals?.cache_savings_usd ?? null;

  if (supported === false) {
    return (
      <Card>
        <Unsupported feature={t.unsupportedFeature} endpoint="GET /api/telemetry/usage" />
      </Card>
    );
  }
  if (!totals) return <Loading />;

  const prompt = promptTokens(totals.tokens);
  const cacheShare = prompt > 0 ? totals.tokens.cached_input / prompt : null;
  const auxiliary = report?.models.reduce((acc, row) => acc + row.auxiliary_calls, 0) ?? 0;

  return (
    <>
      {banner && (
        <Note tone={banner.tone} icon={banner.tone === "ok" ? <CircleCheck /> : <CircleAlert />}>
          {banner.text}
        </Note>
      )}

      <StatGrid>
        <Stat
          label={t.totalCost}
          icon={<CircleDollarSign />}
          value={formatUsd(totals.cost_usd)}
          foot={
            totals.unpriced_calls > 0 ? (
              <span className="text-warn">{t.unpricedCalls(formatNumber(totals.unpriced_calls))}</span>
            ) : (
              <span>{t.since(formatDateTime(totals.since_unix_ms))}</span>
            )
          }
        />
        <Stat
          label={t.calls}
          icon={<Coins />}
          value={formatNumber(totals.calls)}
          foot={
            <span>{auxiliary > 0 ? t.auxiliaryCalls(formatNumber(auxiliary)) : t.noAuxiliary}</span>
          }
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

      <Card title={t.mixTitle} sub={t.mixSub}>
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

      <Card
        flush
        title={t.byModel}
        sub={t.byModelSub}
        actions={
          totals.calls > 0 ? (
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
        ) : usage.status === "loading" && !report ? (
          <Loading />
        ) : !report || report.models.length === 0 ? (
          <EmptyState icon={<Coins />} title={t.empty} hint={t.emptyHint} />
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
          void refresh();
        }}
      />
    </>
  );
}
