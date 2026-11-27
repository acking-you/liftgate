import * as React from "react";
import { useMemo, useState } from "react";
import { ArrowRight, Play, Sparkles } from "lucide-react";
import { CopyButton } from "@/components/CopyButton";
import { Button, Card, EmptyState, Pill, SearchInput, Segmented } from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { useRevisionResource } from "@/hooks/useResource";
import { formatPrice } from "@/lib/format";
import { useI18n } from "@/lib/i18n";
import { modelFamily } from "@/lib/stats";
import type { ModelPrice, ModelPricingEntry } from "@/types/telemetry";

type Family = "all" | "gemini" | "claude" | "other";

/** Price entry and effective price for every model id the pricing endpoint lists. */
function priceIndex(entries: ModelPricingEntry[]): Map<string, ModelPrice | null> {
  const out = new Map<string, ModelPrice | null>();
  for (const entry of entries) {
    const price = entry.custom_price ?? entry.default_price;
    for (const id of entry.model_ids) out.set(id, price);
  }
  return out;
}

export function ModelsView({
  onProbe,
  onEditPrices,
}: {
  onProbe: (modelId: string) => void;
  onEditPrices: () => void;
}): React.JSX.Element {
  const { client, snapshot, baseUrl } = useGateway();
  const { m } = useI18n();
  const t = m.infra.models;
  const models = useMemo(() => snapshot?.available_models ?? [], [snapshot?.available_models]);
  const defaultModel = snapshot?.acp_process.default_model;
  const [family, setFamily] = useState<Family>("all");
  const [query, setQuery] = useState("");
  const pricingSupported = snapshot?.usage !== undefined;
  const pricing = useRevisionResource(
    pricingSupported ? () => client.pricing() : null,
    `${baseUrl}|${client.demo}|pricing`,
    snapshot?.usage?.revision ?? -1,
  );
  const prices = useMemo(() => priceIndex(pricing.data?.entries ?? []), [pricing.data]);

  const counts = useMemo(() => {
    const out = { gemini: 0, claude: 0, other: 0 };
    for (const model of models) out[modelFamily(model.modelId)] += 1;
    return out;
  }, [models]);

  const shown = useMemo(() => {
    const q = query.trim().toLowerCase();
    return models.filter((model) => {
      if (family !== "all" && modelFamily(model.modelId) !== family) return false;
      return q === "" || model.modelId.toLowerCase().includes(q) || model.name.toLowerCase().includes(q);
    });
  }, [models, family, query]);

  return (
    <Card
      flush
      title={t.title(models.length)}
      actions={
        pricingSupported ? (
          pricing.status === "error" ? (
            <button type="button" className="lg-link text-err" onClick={pricing.reload}>
              {t.reloadPrices}
            </button>
          ) : (
            <button type="button" className="lg-link" onClick={onEditPrices}>
              {t.editPrices}
              <ArrowRight />
            </button>
          )
        ) : undefined
      }
    >
      <div className="lg-toolbar">
        <Segmented<Family>
          ariaLabel={t.familyFilter}
          value={family}
          onChange={setFamily}
          options={[
            { value: "all", label: t.all, count: models.length },
            { value: "gemini", label: "Gemini", count: counts.gemini },
            { value: "claude", label: "Claude", count: counts.claude },
            { value: "other", label: t.other, count: counts.other },
          ]}
        />
        <SearchInput value={query} onChange={setQuery} placeholder={t.search} className="w-full sm:w-64" />
      </div>
      {shown.length === 0 ? (
        <EmptyState
          icon={<Sparkles />}
          title={models.length === 0 ? t.none : t.noMatch}
          hint={models.length === 0 ? t.noneHint : t.noMatchHint}
        />
      ) : (
        <div className="lg-table-wrap">
          <table className="lg-table">
            <thead>
              <tr>
                <th>{t.id}</th>
                <th className="hide-sm">{t.name}</th>
                {pricingSupported && <th className="num hide-sm">{t.price}</th>}
                <th />
              </tr>
            </thead>
            <tbody>
              {shown.map((model) => {
                const price = prices.get(model.modelId);
                return (
                  <tr key={model.modelId}>
                    <td>
                      <span className="inline-flex items-center gap-1.5">
                        <span className="lg-num text-[12.5px] font-medium text-fg">{model.modelId}</span>
                        <CopyButton value={model.modelId} title={t.copyId} />
                        {model.modelId === defaultModel && <Pill tone="accent">{t.default}</Pill>}
                      </span>
                    </td>
                    <td className="hide-sm text-fg-3">{model.name || "—"}</td>
                    {pricingSupported && (
                      <td className="num hide-sm whitespace-nowrap">
                        {price ? (
                          <span
                            className="lg-num text-[12px] text-fg-2"
                            title={t.priceHint(
                              formatPrice(price.input),
                              formatPrice(price.cached_input),
                              formatPrice(price.output),
                            )}
                          >
                            {formatPrice(price.input)}
                            <span className="text-fg-4"> / </span>
                            {formatPrice(price.output)}
                          </span>
                        ) : pricing.data ? (
                          <span className="text-[11.5px] text-warn">{t.unpriced}</span>
                        ) : (
                          <span className="text-fg-4">—</span>
                        )}
                      </td>
                    )}
                    <td className="text-right">
                      <Button size="sm" onClick={() => onProbe(model.modelId)}>
                        <Play />
                        {t.probe}
                      </Button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </Card>
  );
}
