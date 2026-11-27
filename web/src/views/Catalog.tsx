import * as React from "react";
import { useMemo, useState } from "react";
import { ArrowRight, CircleAlert, FileSearch, Library } from "lucide-react";
import { CodeBlock, ProseBlock } from "@/components/CodeBlock";
import { RankedBars, SegmentedBar } from "@/components/charts";
import { ToolGlyph } from "@/components/ToolExchange";
import { Sheet } from "@/components/ui/dialog";
import {
  Button,
  Card,
  Disclosure,
  EmptyState,
  Figure,
  HarnessBadge,
  Loading,
  Meter,
  Note,
  Pill,
  SearchInput,
  Tag,
  type Tone,
  Unsupported,
} from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { useRevisionResource } from "@/hooks/useResource";
import { groupSegments, rankSegments } from "@/lib/context";
import { byteLength, formatBytes, formatNumber, formatPct, formatSizeChange, shortId } from "@/lib/format";
import { orchestratorModeLabel } from "@/lib/harness";
import { useI18n } from "@/lib/i18n";
import { WIRE_BUDGET_BYTES } from "@/lib/stats";
import { classifyTool, prettyJson } from "@/lib/toolcalls";
import type {
  CatalogToolEntry,
  CatalogToolTier,
  CatalogWireTool,
  SessionSummary,
  SessionToolCatalog,
} from "@/types/telemetry";
import type { ViewProps } from "@/views/types";

const TIER_ORDER: readonly CatalogToolTier[] = [
  "orchestrator",
  "direct",
  "folded_into_orchestrator",
  "namespace_hub_action",
  "slack_backfill",
  "search_index_only",
];

const TIER_TONE: Record<CatalogToolTier, Tone> = {
  orchestrator: "accent",
  direct: "ok",
  folded_into_orchestrator: "neutral",
  namespace_hub_action: "warn",
  slack_backfill: "ok",
  search_index_only: "neutral",
  unknown: "neutral",
};

type SheetTarget = { kind: "client"; entry: CatalogToolEntry } | { kind: "wire"; tool: CatalogWireTool };

function schemaBlock(raw: string, title: string): React.JSX.Element {
  const pretty = prettyJson(raw);
  return (
    <CodeBlock code={pretty ?? raw} language={pretty !== null ? "json" : "text"} title={title} maxHeight={360} />
  );
}

function CompareSheet({
  target,
  catalog,
  onClose,
}: {
  target: SheetTarget | null;
  catalog: SessionToolCatalog;
  onClose: () => void;
}): React.JSX.Element {
  const { m } = useI18n();
  const c = m.catalog.compare;
  let title: React.ReactNode = null;
  let body: React.ReactNode = null;

  if (target?.kind === "client") {
    const entry = target.entry;
    const tier = { ...m.catalog.tier[entry.tier], tone: TIER_TONE[entry.tier] };
    const mounted = entry.wire_description != null || entry.wire_schema != null;
    title = (
      <>
        <ToolGlyph kind={classifyTool(entry.client_name, entry.tier === "orchestrator")} />
        <span className="lg-num">{entry.client_name}</span>
        <Pill tone={tier.tone}>{tier.label}</Pill>
      </>
    );
    body = (
      <>
        <div className="grid grid-cols-3 gap-4">
          <Figure label={c.clientDeclared} value={formatBytes(entry.client_bytes)} sub={entry.client_kind} />
          <Figure
            label={c.onLg}
            value={formatBytes(entry.wire_bytes)}
            sub={entry.wire_name ? c.providedBy(entry.wire_name) : c.noWire}
          />
          <Figure
            label={c.sizeChange}
            value={formatSizeChange(entry.client_bytes, entry.wire_bytes)}
            sub={tier.hint}
          />
        </div>
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
          <section className="flex min-w-0 flex-col gap-2">
            <h3 className="lg-section-title m-0">{c.clientOriginal}</h3>
            {entry.client_description ? (
              <ProseBlock text={entry.client_description} title="Description" maxHeight={300} />
            ) : (
              <p className="lg-hint m-0">{c.noDescription}</p>
            )}
            {entry.client_schema &&
              schemaBlock(entry.client_schema, entry.client_kind === "custom" ? "Custom Grammar" : "JSON Schema")}
          </section>
          <section className="flex min-w-0 flex-col gap-2">
            <h3 className="lg-section-title m-0">{c.modelSees}</h3>
            {mounted ? (
              <>
                {entry.wire_description && (
                  <ProseBlock text={entry.wire_description} title="Wire Description" maxHeight={300} />
                )}
                {entry.wire_schema && schemaBlock(entry.wire_schema, "Wire Input Schema")}
              </>
            ) : (
              <Note>
                {entry.tier === "search_index_only"
                  ? c.searchOnly
                  : entry.tier === "folded_into_orchestrator"
                    ? c.folded(entry.wire_name ?? null)
                    : c.behindHub(entry.wire_name ?? null)}
              </Note>
            )}
            {entry.ts_signature && (
              <CodeBlock
                code={entry.ts_signature}
                language="js"
                title={c.tsSignature}
                maxHeight={220}
              />
            )}
          </section>
        </div>
      </>
    );
  } else if (target?.kind === "wire") {
    const tool = target.tool;
    title = (
      <>
        <ToolGlyph kind={classifyTool(tool.wire_name, tool.role === "orchestrator")} />
        <span className="lg-num">{tool.wire_name}</span>
        <Tag>{m.catalog.role[tool.role]}</Tag>
      </>
    );
    body = (
      <>
        <div className="grid grid-cols-3 gap-4">
          <Figure
            label={c.wireSize}
            value={formatBytes(tool.bytes)}
            sub={c.ofBudget(formatPct(tool.bytes / catalog.wire_budget_bytes))}
          />
          <Figure label="Description" value={formatBytes(byteLength(tool.description ?? ""))} />
          <Figure label={c.foldedTools} value={tool.folded_actions.length} />
        </div>
        {tool.folded_actions.length > 0 && (
          <section>
            <h3 className="lg-section-title">{c.foldedBehind}</h3>
            <div className="flex flex-wrap gap-1.5">
              {tool.folded_actions.map((name) => (
                <Tag key={name}>{name}</Tag>
              ))}
            </div>
          </section>
        )}
        {tool.description && <ProseBlock text={tool.description} title="Wire Description" maxHeight={420} />}
        {schemaBlock(tool.input_schema, "Wire Input Schema")}
      </>
    );
  }

  return (
    <Sheet
      size="wide"
      open={target !== null}
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      title={title}
      description={target?.kind === "client" ? c.clientDescription : c.wireDescription}
    >
      {body}
    </Sheet>
  );
}

function CatalogDetail({ catalog }: { catalog: SessionToolCatalog }): React.JSX.Element {
  const { m } = useI18n();
  const d = m.catalog.detail;
  const [tier, setTier] = useState<CatalogToolTier | "all">("all");
  const [wireFilter, setWireFilter] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [target, setTarget] = useState<SheetTarget | null>(null);

  const wireTotal = catalog.wire_tools.reduce((sum, t) => sum + t.bytes, 0);
  const gatewayWire = catalog.wire_tools.filter((t) => t.role === "gateway");
  const gatewayBytes = gatewayWire.reduce((sum, t) => sum + t.bytes, 0);
  // Compare like with like: what the client's own tools cost on the wire.
  const clientWireBytes = wireTotal - gatewayBytes;
  const wireMax = catalog.wire_tools.reduce((m, t) => Math.max(m, t.bytes), 0);
  const contextTotal = catalog.segments.reduce((sum, s) => sum + s.bytes, 0);
  const contextTokens = catalog.segments.reduce((sum, s) => sum + s.tokens_est, 0);
  const ranked = rankSegments(catalog.segments);
  const wireSorted = useMemo(
    () => [...catalog.wire_tools].sort((a, b) => b.bytes - a.bytes),
    [catalog.wire_tools],
  );

  const tierCounts = useMemo(() => {
    const counts = new Map<CatalogToolTier, number>();
    for (const tool of catalog.tools) counts.set(tool.tier, (counts.get(tool.tier) ?? 0) + 1);
    return counts;
  }, [catalog.tools]);

  const shownTools = useMemo(() => {
    const q = query.trim().toLowerCase();
    return catalog.tools
      .filter((tool) => {
        if (tier !== "all" && tool.tier !== tier) return false;
        if (wireFilter !== null && tool.wire_name !== wireFilter) return false;
        return q === "" || tool.client_name.toLowerCase().includes(q);
      })
      .sort((a, b) => b.client_bytes - a.client_bytes);
  }, [catalog.tools, tier, wireFilter, query]);

  return (
    <>
      <div className="grid grid-cols-1 gap-4 xl:grid-cols-[minmax(0,1.5fr)_minmax(0,1fr)]">
        <Card
          title={d.context}
          sub={d.contextSub(formatBytes(contextTotal), formatNumber(contextTokens))}
        >
          <SegmentedBar parts={groupSegments(catalog.segments)} format={formatBytes} ariaLabel={d.contextAria} />
          {ranked.length > 0 && (
            <div className="mt-4">
              <RankedBars
                rows={ranked.map((row) => ({
                  key: row.key,
                  label: row.label,
                  value: row.bytes,
                  display: formatBytes(row.bytes),
                }))}
                labelWidth={132}
                color="var(--lg-s-neutral)"
              />
            </div>
          )}
        </Card>

        <Card title={d.savings} sub={d.searchEntries(catalog.search_index_entries)}>
          <div className="flex flex-col gap-4">
            <div className="flex items-center gap-3">
              <Figure
                label={d.clientDeclared}
                value={formatBytes(catalog.client_tools_bytes)}
                sub={d.tools(catalog.tools.length)}
              />
              <ArrowRight className="h-4 w-4 flex-none text-fg-4" />
              <Figure
                label={d.mounted}
                value={formatBytes(clientWireBytes)}
                sub={d.tools(catalog.wire_tools.length - gatewayWire.length)}
              />
              <div className="ml-auto text-right">
                <div className="text-[12px] text-fg-3">{d.sizeChange}</div>
                <div className="lg-num text-[22px] font-semibold leading-tight text-fg">
                  {formatSizeChange(catalog.client_tools_bytes, clientWireBytes)}
                </div>
              </div>
            </div>
            {gatewayWire.length > 0 && (
              <p className="lg-hint m-0">
                {d.gatewayExtra(gatewayWire.length, formatBytes(gatewayBytes))}
              </p>
            )}
            <div>
              <div className="mb-1.5 flex items-baseline justify-between text-[12px] text-fg-3">
                <span>{d.wireBudget}</span>
                <span className="lg-num">
                  {formatBytes(wireTotal)} / {formatBytes(catalog.wire_budget_bytes)}
                </span>
              </div>
              <Meter
                ratio={wireTotal / catalog.wire_budget_bytes}
                tone={wireTotal / catalog.wire_budget_bytes > 0.9 ? "warn" : undefined}
                label={d.wireRatio}
              />
            </div>
          </div>
        </Card>
      </div>

      <Card
        flush
        title={d.map}
        sub={d.mapSub}
      >
        <div className="lg-toolbar lg-chiprow" role="group" aria-label={d.tierFilter}>
          <button type="button" className="lg-chip lg-chip-plain" aria-pressed={tier === "all"} onClick={() => setTier("all")}>
            {d.all}
            <span className="lg-seg-count">{catalog.tools.length}</span>
          </button>
          {TIER_ORDER.filter((id) => (tierCounts.get(id) ?? 0) > 0).map((id) => (
            <button
              key={id}
              type="button"
              className="lg-chip lg-chip-plain"
              aria-pressed={tier === id}
              title={m.catalog.tier[id].hint}
              onClick={() => setTier((cur) => (cur === id ? "all" : id))}
            >
              {m.catalog.tier[id].label}
              <span className="lg-seg-count">{tierCounts.get(id)}</span>
            </button>
          ))}
          <SearchInput value={query} onChange={setQuery} placeholder={d.filterByName} className="ml-auto w-full sm:w-56" />
        </div>

        <div className="lg-map">
          <section className="lg-map-col" aria-label={d.wireTools}>
            <h3 className="lg-map-title">
              {d.wireTools} · {catalog.wire_tools.length}
              <span className="lg-num font-normal text-fg-4">{formatBytes(wireTotal)}</span>
            </h3>
            <ul className="m-0 list-none p-0">
              {wireSorted.map((tool) => (
                <li key={tool.wire_name}>
                  <div className="lg-map-row" data-active={wireFilter === tool.wire_name}>
                    <button
                      type="button"
                      className="lg-map-main"
                      aria-pressed={wireFilter === tool.wire_name}
                      onClick={() => setWireFilter((cur) => (cur === tool.wire_name ? null : tool.wire_name))}
                    >
                      <ToolGlyph kind={classifyTool(tool.wire_name, tool.role === "orchestrator")} size="sm" />
                      <span className="min-w-0 flex-1">
                        <span className="flex items-center gap-1.5">
                          <span className="lg-num lg-clip text-[12.5px] font-medium text-fg">{tool.wire_name}</span>
                          <span className="flex-none text-[11px] text-fg-4">
                            {m.catalog.role[tool.role]}
                            {tool.folded_actions.length > 0 ? d.folded(tool.folded_actions.length) : ""}
                          </span>
                        </span>
                        <span className="lg-minibar-track mt-1.5">
                          <span
                            style={{
                              width: `${wireMax > 0 ? (tool.bytes / wireMax) * 100 : 0}%`,
                              background: "var(--lg-s1)",
                            }}
                          />
                        </span>
                      </span>
                      <span className="lg-num w-[62px] flex-none text-right text-[11.5px] text-fg-2">
                        {formatBytes(tool.bytes)}
                      </span>
                    </button>
                    <Button
                      size="sm"
                      variant="ghost"
                      icon
                      aria-label={d.viewDefinitionOf(tool.wire_name)}
                      title={d.viewDefinition}
                      onClick={() => setTarget({ kind: "wire", tool })}
                    >
                      <FileSearch />
                    </Button>
                  </div>
                </li>
              ))}
            </ul>
          </section>

          <section className="lg-map-col" aria-label={d.clientTools}>
            <h3 className="lg-map-title">
              {d.clientTools} · {shownTools.length}
              {wireFilter !== null && (
                <button type="button" className="lg-link text-[11.5px]" onClick={() => setWireFilter(null)}>
                  {d.clearWireFilter(wireFilter)}
                </button>
              )}
            </h3>
            {shownTools.length === 0 ? (
              <p className="lg-hint m-0 px-4 py-6 text-center">{d.noTools}</p>
            ) : (
              <ul className="m-0 list-none p-0">
                {shownTools.map((tool) => (
                  <li key={tool.client_name}>
                    <div className="lg-map-row">
                      <button type="button" className="lg-map-main" onClick={() => setTarget({ kind: "client", entry: tool })}>
                        <ToolGlyph kind={classifyTool(tool.client_name, tool.tier === "orchestrator")} size="sm" />
                        <span className="lg-num lg-clip min-w-0 flex-1 text-[12.5px] text-fg">{tool.client_name}</span>
                        <Pill tone={TIER_TONE[tool.tier]}>{m.catalog.tier[tool.tier].label}</Pill>
                        <span className="lg-num w-[132px] flex-none text-right text-[11.5px] text-fg-3 max-sm:hidden">
                          {formatBytes(tool.client_bytes)}
                          <span className="text-fg-4"> → </span>
                          <span className="text-fg-2">{formatBytes(tool.wire_bytes)}</span>
                        </span>
                      </button>
                    </div>
                  </li>
                ))}
              </ul>
            )}
          </section>
        </div>
      </Card>

      {(catalog.bridge_rules_prompt || catalog.orchestrator_contract_prompt) && (
        <Card title={d.bridge} sub={d.bridgeSub}>
          <div className="flex flex-col gap-2">
            {catalog.orchestrator_contract_prompt && (
              <Disclosure
                head={
                  <>
                    <span className="flex-none font-medium text-fg-2">{d.contract}</span>
                    <span className="lg-num lg-clip min-w-0 text-[11.5px] text-fg-4">McpOrchestratorContract</span>
                  </>
                }
                meta={formatBytes(byteLength(catalog.orchestrator_contract_prompt))}
              >
                <CodeBlock code={catalog.orchestrator_contract_prompt} language="xml" title="McpOrchestratorContract" wrap maxHeight={520} />
              </Disclosure>
            )}
            {catalog.bridge_rules_prompt && (
              <Disclosure
                head={
                  <>
                    <span className="flex-none font-medium text-fg-2">{d.rules}</span>
                    <span className="lg-num lg-clip min-w-0 text-[11.5px] text-fg-4">&lt;api_gateway_bridge_rules&gt;</span>
                  </>
                }
                meta={formatBytes(byteLength(catalog.bridge_rules_prompt))}
              >
                <CodeBlock code={catalog.bridge_rules_prompt} language="xml" title="<api_gateway_bridge_rules>" wrap maxHeight={520} />
              </Disclosure>
            )}
          </div>
        </Card>
      )}

      <CompareSheet target={target} catalog={catalog} onClose={() => setTarget(null)} />
    </>
  );
}

function SessionPicker({
  sessions,
  selectedId,
  onSelect,
}: {
  sessions: SessionSummary[];
  selectedId: string | null;
  onSelect: (id: string) => void;
}): React.JSX.Element {
  const { m } = useI18n();
  const p = m.catalog.picker;
  return (
    <div className="lg-table-wrap">
      <table className="lg-table">
        <thead>
          <tr>
            <th>Harness</th>
            <th className="hide-sm">{p.session}</th>
            <th className="hide-sm">{p.model}</th>
            <th className="hide-sm">{p.mode}</th>
            <th className="num hide-sm">{p.eager}</th>
            <th className="num hide-sm">Hub</th>
            <th>{p.wireSize}</th>
          </tr>
        </thead>
        <tbody>
          {sessions.map((sess) => {
            const mcp = sess.mcp;
            if (!mcp) return null;
            const ratio = mcp.wire_bytes / WIRE_BUDGET_BYTES;
            return (
              <tr
                key={sess.session_id}
                data-clickable="true"
                aria-selected={selectedId === sess.session_id}
                tabIndex={0}
                onClick={() => onSelect(sess.session_id)}
                onKeyDown={(ev) => {
                  if (ev.key === "Enter") onSelect(sess.session_id);
                }}
              >
                <td>
                  <HarnessBadge id={mcp.harness_profile} className="font-medium text-fg" />
                </td>
                <td className="lg-num hide-sm text-[12px]" title={sess.session_id}>
                  {shortId(sess.session_id)}
                </td>
                <td className="lg-num hide-sm whitespace-nowrap text-[12px]">{sess.model_id}</td>
                <td className="hide-sm">
                  <span className="inline-flex gap-1.5">
                    <Tag>{orchestratorModeLabel(mcp.orchestrator_mode)}</Tag>
                    {mcp.is_passthrough && <Tag>{p.passthrough}</Tag>}
                  </span>
                </td>
                <td className="num hide-sm">
                  {mcp.declared_tools} → {mcp.eager_tools}
                </td>
                <td className="num hide-sm">{mcp.virtualized_hubs}</td>
                <td>
                  <div className="flex min-w-[120px] items-center gap-2 sm:w-[190px]">
                    <Meter ratio={ratio} tone={ratio > 0.9 ? "warn" : undefined} label={m.catalog.detail.wireRatio} />
                    <span className="lg-num w-[64px] flex-none text-right text-[11.5px] text-fg-2">
                      {formatBytes(mcp.wire_bytes)}
                    </span>
                  </div>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

export function CatalogView({ arg, navigate }: ViewProps): React.JSX.Element {
  const { client, snapshot, baseUrl } = useGateway();
  const { m } = useI18n();
  const v = m.catalog.view;
  const sessions = useMemo(
    () => (snapshot?.session_pool.sessions ?? []).filter((s) => s.mcp != null),
    [snapshot?.session_pool.sessions],
  );
  const selectedId = arg ?? sessions[0]?.session_id ?? null;
  const selected = sessions.find((s) => s.session_id === selectedId);
  // Gateways that record per-call tool I/O also serve the catalog.
  const supported = snapshot === null ? null : snapshot.tool_call_telemetry !== undefined;
  // Client tools sync when a turn starts (`turn_count`), but the context sample only lands when
  // it finishes, so refetch on both.
  const lastFinished = useMemo(() => {
    let latest = 0;
    for (const task of snapshot?.recent_tasks ?? []) {
      if (task.session_id === selectedId) latest = Math.max(latest, task.finished_at_unix_ms ?? 0);
    }
    return latest;
  }, [snapshot?.recent_tasks, selectedId]);
  const catalog = useRevisionResource(
    supported === true && selectedId !== null ? () => client.toolCatalog(selectedId) : null,
    `${baseUrl}|${client.demo}|catalog|${selectedId ?? ""}`,
    `${selected?.turn_count ?? -1}|${lastFinished}`,
  );

  return (
    <>
      <Card flush title={v.pick} sub={v.pickSub}>
        {sessions.length === 0 ? (
          <EmptyState
            icon={<Library />}
            title={v.noSessions}
            hint={v.noSessionsHint}
          />
        ) : (
          <SessionPicker sessions={sessions} selectedId={selectedId} onSelect={(id) => navigate("catalog", id)} />
        )}
      </Card>

      {supported === false ? (
        <Card>
          <Unsupported feature={v.unsupported} endpoint="GET /api/telemetry/sessions/{id}/tool-catalog" />
        </Card>
      ) : selectedId === null || supported === null ? null : catalog.status === "loading" ? (
        <Card>
          <Loading label={v.loading} />
        </Card>
      ) : catalog.status === "unsupported" ? (
        <Card>
          <EmptyState
            icon={<Library />}
            title={v.gone}
            hint={v.goneHint(<span className="lg-num">{shortId(selectedId, 13)}</span>)}
            action={
              sessions[0] ? (
                <Button size="sm" onClick={() => navigate("catalog", sessions[0]?.session_id)}>
                  {v.current}
                </Button>
              ) : undefined
            }
          />
        </Card>
      ) : catalog.status === "error" || catalog.data === null ? (
        <Note tone="err" icon={<CircleAlert />}>
          {v.failed(catalog.error ?? v.unknownError)}
          <Button size="sm" className="ml-2" onClick={catalog.reload}>
            {v.retry}
          </Button>
        </Note>
      ) : (
        <CatalogDetail key={catalog.data.session_id} catalog={catalog.data} />
      )}
    </>
  );
}
