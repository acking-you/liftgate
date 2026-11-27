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

const TIER: Record<CatalogToolTier, { label: string; tone: Tone; hint: string }> = {
  orchestrator: { label: "Tier-0 编排器", tone: "accent", hint: "CodeMode 编排器，脚本里再调其他工具" },
  direct: { label: "Tier-1 直通", tone: "ok", hint: "一对一原样挂载" },
  folded_into_orchestrator: {
    label: "折叠进编排器",
    tone: "neutral",
    hint: "不单独挂载，说明内联在编排器契约里",
  },
  namespace_hub_action: { label: "Tier-2 Hub 动作", tone: "warn", hint: "折叠在 mcp_lg_ns__* 命名空间 Hub 后面" },
  slack_backfill: { label: "Slack 回填", tone: "ok", hint: "预算有余量时回填成直通工具" },
  search_index_only: { label: "仅检索索引", tone: "neutral", hint: "只能通过 0-RTT 检索发现" },
  unknown: { label: "未知", tone: "neutral", hint: "当前版本无法识别的分层" },
};

const WIRE_ROLE: Record<CatalogWireTool["role"], string> = {
  orchestrator: "编排器",
  direct: "直通",
  namespace_hub: "Hub",
  slack_backfill: "回填",
  gateway: "网关内置",
  unknown: "未知",
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
  let title: React.ReactNode = null;
  let body: React.ReactNode = null;

  if (target?.kind === "client") {
    const entry = target.entry;
    const tier = TIER[entry.tier];
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
          <Figure label="客户端声明" value={formatBytes(entry.client_bytes)} sub={entry.client_kind} />
          <Figure
            label="挂到 lg 之后"
            value={formatBytes(entry.wire_bytes)}
            sub={entry.wire_name ? `由 ${entry.wire_name} 提供` : "不占 Wire 体积"}
          />
          <Figure
            label="体积变化"
            value={formatSizeChange(entry.client_bytes, entry.wire_bytes)}
            sub={tier.hint}
          />
        </div>
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
          <section className="flex min-w-0 flex-col gap-2">
            <h3 className="lg-section-title m-0">客户端原始声明</h3>
            {entry.client_description ? (
              <ProseBlock text={entry.client_description} title="Description" maxHeight={300} />
            ) : (
              <p className="lg-hint m-0">没有 Description。</p>
            )}
            {entry.client_schema &&
              schemaBlock(entry.client_schema, entry.client_kind === "custom" ? "Custom Grammar" : "JSON Schema")}
          </section>
          <section className="flex min-w-0 flex-col gap-2">
            <h3 className="lg-section-title m-0">模型实际看到的</h3>
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
                  ? "这个工具没有挂载到 lg。模型需要先用 mcp_lg_search_client_tools 检索到它，再经分发器调用。"
                  : entry.tier === "folded_into_orchestrator"
                    ? `这个工具没有单独挂载，它的说明内联在编排器契约里${entry.wire_name ? `，由 ${entry.wire_name} 在脚本中调用` : ""}。`
                    : `这个工具折叠在 ${entry.wire_name ?? "命名空间 Hub"} 后面，模型通过 Hub 的 action 参数选择它。`}
              </Note>
            )}
            {entry.ts_signature && (
              <CodeBlock
                code={entry.ts_signature}
                language="js"
                title="检索索引里的 TypeScript 签名"
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
        <Tag>{WIRE_ROLE[tool.role]}</Tag>
      </>
    );
    body = (
      <>
        <div className="grid grid-cols-3 gap-4">
          <Figure label="Wire 体积" value={formatBytes(tool.bytes)} sub={`预算的 ${formatPct(tool.bytes / catalog.wire_budget_bytes)}`} />
          <Figure label="Description" value={formatBytes(byteLength(tool.description ?? ""))} />
          <Figure label="折叠的客户端工具" value={tool.folded_actions.length} />
        </div>
        {tool.folded_actions.length > 0 && (
          <section>
            <h3 className="lg-section-title">折叠在它后面的工具</h3>
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
      description={target?.kind === "client" ? "同一个工具：客户端怎么声明，Liftgate 怎么交给模型。" : "挂载在 lg MCP 服务器上的工具定义。"}
    >
      {body}
    </Sheet>
  );
}

function CatalogDetail({ catalog }: { catalog: SessionToolCatalog }): React.JSX.Element {
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
          title="上下文构成"
          sub={`最近一轮共 ${formatBytes(contextTotal)}，约 ${formatNumber(contextTokens)} tokens`}
        >
          <SegmentedBar parts={groupSegments(catalog.segments)} format={formatBytes} ariaLabel="上下文按类别的体积构成" />
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

        <Card title="虚拟化收益" sub={`检索索引 ${catalog.search_index_entries} 项`}>
          <div className="flex flex-col gap-4">
            <div className="flex items-center gap-3">
              <Figure label="客户端声明" value={formatBytes(catalog.client_tools_bytes)} sub={`${catalog.tools.length} 个工具`} />
              <ArrowRight className="h-4 w-4 flex-none text-fg-4" />
              <Figure
                label="挂载后"
                value={formatBytes(clientWireBytes)}
                sub={`${catalog.wire_tools.length - gatewayWire.length} 个工具`}
              />
              <div className="ml-auto text-right">
                <div className="text-[12px] text-fg-3">体积变化</div>
                <div className="lg-num text-[22px] font-semibold leading-tight text-fg">
                  {formatSizeChange(catalog.client_tools_bytes, clientWireBytes)}
                </div>
              </div>
            </div>
            {gatewayWire.length > 0 && (
              <p className="lg-hint m-0">
                另挂载 {gatewayWire.length} 个网关内置工具 · {formatBytes(gatewayBytes)}
              </p>
            )}
            <div>
              <div className="mb-1.5 flex items-baseline justify-between text-[12px] text-fg-3">
                <span>Wire 预算占用</span>
                <span className="lg-num">
                  {formatBytes(wireTotal)} / {formatBytes(catalog.wire_budget_bytes)}
                </span>
              </div>
              <Meter
                ratio={wireTotal / catalog.wire_budget_bytes}
                tone={wireTotal / catalog.wire_budget_bytes > 0.9 ? "warn" : undefined}
                label="Wire 工具体积占预算比例"
              />
            </div>
          </div>
        </Card>
      </div>

      <Card
        flush
        title="三层虚拟化映射"
        sub="左：模型看到的 Wire 工具，点击筛出它背后的工具。右：客户端声明的工具，点击对照两份定义。"
      >
        <div className="lg-toolbar lg-chiprow" role="group" aria-label="按分层筛选">
          <button type="button" className="lg-chip lg-chip-plain" aria-pressed={tier === "all"} onClick={() => setTier("all")}>
            全部
            <span className="lg-seg-count">{catalog.tools.length}</span>
          </button>
          {TIER_ORDER.filter((id) => (tierCounts.get(id) ?? 0) > 0).map((id) => (
            <button
              key={id}
              type="button"
              className="lg-chip lg-chip-plain"
              aria-pressed={tier === id}
              title={TIER[id].hint}
              onClick={() => setTier((cur) => (cur === id ? "all" : id))}
            >
              {TIER[id].label}
              <span className="lg-seg-count">{tierCounts.get(id)}</span>
            </button>
          ))}
          <SearchInput value={query} onChange={setQuery} placeholder="按工具名筛选" className="ml-auto w-full sm:w-56" />
        </div>

        <div className="lg-map">
          <section className="lg-map-col" aria-label="Wire 工具">
            <h3 className="lg-map-title">
              Wire 工具 · {catalog.wire_tools.length}
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
                            {WIRE_ROLE[tool.role]}
                            {tool.folded_actions.length > 0 ? ` · 折叠 ${tool.folded_actions.length}` : ""}
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
                      aria-label={`查看 ${tool.wire_name} 的定义`}
                      title="查看定义"
                      onClick={() => setTarget({ kind: "wire", tool })}
                    >
                      <FileSearch />
                    </Button>
                  </div>
                </li>
              ))}
            </ul>
          </section>

          <section className="lg-map-col" aria-label="客户端工具">
            <h3 className="lg-map-title">
              客户端工具 · {shownTools.length}
              {wireFilter !== null && (
                <button type="button" className="lg-link text-[11.5px]" onClick={() => setWireFilter(null)}>
                  取消按 {wireFilter} 筛选
                </button>
              )}
            </h3>
            {shownTools.length === 0 ? (
              <p className="lg-hint m-0 px-4 py-6 text-center">没有符合条件的工具。</p>
            ) : (
              <ul className="m-0 list-none p-0">
                {shownTools.map((tool) => (
                  <li key={tool.client_name}>
                    <div className="lg-map-row">
                      <button type="button" className="lg-map-main" onClick={() => setTarget({ kind: "client", entry: tool })}>
                        <ToolGlyph kind={classifyTool(tool.client_name, tool.tier === "orchestrator")} size="sm" />
                        <span className="lg-num lg-clip min-w-0 flex-1 text-[12.5px] text-fg">{tool.client_name}</span>
                        <Pill tone={TIER[tool.tier].tone}>{TIER[tool.tier].label}</Pill>
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
        <Card title="桥接提示词" sub="新建会话时追加到系统提示词。">
          <div className="flex flex-col gap-2">
            {catalog.orchestrator_contract_prompt && (
              <Disclosure
                head={
                  <>
                    <span className="flex-none font-medium text-fg-2">编排器契约</span>
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
                    <span className="flex-none font-medium text-fg-2">桥接规则</span>
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
  return (
    <div className="lg-table-wrap">
      <table className="lg-table">
        <thead>
          <tr>
            <th>Harness</th>
            <th className="hide-sm">会话</th>
            <th className="hide-sm">模型</th>
            <th className="hide-sm">编排方式</th>
            <th className="num hide-sm">声明 → Eager</th>
            <th className="num hide-sm">Hub</th>
            <th>Wire 体积</th>
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
                    {mcp.is_passthrough && <Tag>1:1 直通</Tag>}
                  </span>
                </td>
                <td className="num hide-sm">
                  {mcp.declared_tools} → {mcp.eager_tools}
                </td>
                <td className="num hide-sm">{mcp.virtualized_hubs}</td>
                <td>
                  <div className="flex min-w-[120px] items-center gap-2 sm:w-[190px]">
                    <Meter ratio={ratio} tone={ratio > 0.9 ? "warn" : undefined} label="Wire 工具体积占预算比例" />
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
  const sessions = useMemo(
    () => (snapshot?.session_pool.sessions ?? []).filter((s) => s.mcp != null),
    [snapshot?.session_pool.sessions],
  );
  const selectedId = arg ?? sessions[0]?.session_id ?? null;
  const selected = sessions.find((s) => s.session_id === selectedId);
  // Gateways that record per-call tool I/O also serve the catalog.
  const supported = snapshot === null ? null : snapshot.tool_call_telemetry !== undefined;
  const catalog = useRevisionResource(
    supported === true && selectedId !== null ? () => client.toolCatalog(selectedId) : null,
    `${baseUrl}|${client.demo}|catalog|${selectedId ?? ""}`,
    selected?.turn_count ?? -1,
  );

  return (
    <>
      <Card flush title="选择会话" sub="并排比较各 Harness 的工具虚拟化结果。">
        {sessions.length === 0 ? (
          <EmptyState
            icon={<Library />}
            title="会话池里没有带工具的会话"
            hint="发起一次声明了工具的对话后，这里会列出它的虚拟化规划。"
          />
        ) : (
          <SessionPicker sessions={sessions} selectedId={selectedId} onSelect={(id) => navigate("catalog", id)} />
        )}
      </Card>

      {supported === false ? (
        <Card>
          <Unsupported feature="会话工具目录" endpoint="GET /api/telemetry/sessions/{id}/tool-catalog" />
        </Card>
      ) : selectedId === null || supported === null ? null : catalog.status === "loading" ? (
        <Card>
          <Loading label="读取工具目录…" />
        </Card>
      ) : catalog.status === "unsupported" ? (
        <Card>
          <EmptyState
            icon={<Library />}
            title="这个会话已不在会话池里"
            hint={
              <>
                会话 <span className="lg-num">{shortId(selectedId, 13)}</span>{" "}
                被回收后，它的工具目录也一起释放了。
              </>
            }
            action={
              sessions[0] ? (
                <Button size="sm" onClick={() => navigate("catalog", sessions[0]?.session_id)}>
                  查看当前会话
                </Button>
              ) : undefined
            }
          />
        </Card>
      ) : catalog.status === "error" || catalog.data === null ? (
        <Note tone="err" icon={<CircleAlert />}>
          读取工具目录失败：{catalog.error ?? "未知错误"}
          <Button size="sm" className="ml-2" onClick={catalog.reload}>
            重试
          </Button>
        </Note>
      ) : (
        <CatalogDetail key={catalog.data.session_id} catalog={catalog.data} />
      )}
    </>
  );
}
