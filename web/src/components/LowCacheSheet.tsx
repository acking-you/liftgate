import * as React from "react";
import { useEffect, useState } from "react";
import { Info } from "lucide-react";
import { ProseBlock } from "@/components/CodeBlock";
import { CopyButton } from "@/components/CopyButton";
import { RankedBars, SegmentedBar } from "@/components/charts";
import { MessageTimeline } from "@/components/MessageTimeline";
import { AcpRequestView, ClientToolsView } from "@/components/PayloadViews";
import { Sheet } from "@/components/ui/dialog";
import { Disclosure, Figure, KV, Loading, Note, Pill, Tabs, Tag } from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { useResource } from "@/hooks/useResource";
import { groupSegments, rankSegments, segmentsFromMetrics } from "@/lib/context";
import { formatBpsPct, formatBytes, formatDateTime, formatDurationMs, formatNumber } from "@/lib/format";
import { useI18n } from "@/lib/i18n";
import { REASON_TONE, causeLabel, explainLowCache, reasonLabel, routingLabel } from "@/lib/lowcache";
import type { LowCacheHitRecord, LowCacheRequestMetrics } from "@/types/telemetry";

type TabId = "why" | "request" | "messages" | "tools";

export function ReasonPills({ record }: { record: LowCacheHitRecord }): React.JSX.Element {
  useI18n();
  const cause = causeLabel(record.diagnostic_cause);
  return (
    <span className="inline-flex flex-wrap items-center gap-1.5">
      <Pill tone={REASON_TONE[record.reason]}>{reasonLabel(record.reason)}</Pill>
      {cause !== null && <Tag>{cause}</Tag>}
    </span>
  );
}

function Composition({ metrics }: { metrics: LowCacheRequestMetrics }): React.JSX.Element {
  const { m } = useI18n();
  const t = m.cache.sheet;
  const segments = segmentsFromMetrics(metrics);
  const ranked = rankSegments(segments);
  return (
    <section>
      <h3 className="lg-section-title">{t.composition(formatBytes(metrics.total_conversation_bytes))}</h3>
      <SegmentedBar parts={groupSegments(segments)} format={formatBytes} ariaLabel={t.compositionAria} />
      {ranked.length > 0 && (
        <div className="mt-3">
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
      <p className="lg-hint m-0 mt-2">
        {t.clientTools(metrics.client_tool_count, formatBytes(metrics.client_tools_schema_bytes))}
      </p>
    </section>
  );
}

function Why({ record }: { record: LowCacheHitRecord }): React.JSX.Element {
  const { m: messages } = useI18n();
  const t = messages.cache.sheet;
  const m = record.request_metrics;
  return (
    <>
      <div className="grid grid-cols-2 gap-4 sm:grid-cols-4">
        <Figure
          label={t.readOfTotal}
          value={`${formatNumber(record.cached_read_tokens)} / ${formatNumber(record.total_prompt_tokens)}`}
          sub="tokens"
        />
        <Figure label={t.uncached} value={formatNumber(record.input_uncached_tokens)} sub="tokens" />
        <Figure
          label={t.outputThought}
          value={`${formatNumber(record.output_tokens)} / ${formatNumber(m?.thought_tokens ?? 0)}`}
          sub="tokens"
        />
        <Figure
          label={t.elapsed}
          value={formatDurationMs(record.elapsed_ms)}
          sub={`TTFT ${formatDurationMs(record.ttft_ms)}`}
        />
      </div>

      {m ? (
        <>
          <section>
            <h3 className="lg-section-title">{t.routing}</h3>
            <KV
              items={[
                [t.routingMode, routingLabel(m.routing_strategy)],
                [t.sessionTurn, t.turnOf(m.session_turn_index, record.session_reused)],
                [t.idleBefore, m.session_idle_ms != null ? formatDurationMs(m.session_idle_ms) : t.newSession],
                [t.previousContext, `${formatNumber(m.previous_session_context_tokens)} tokens`],
                [
                  t.modelSwitch,
                  m.model_switched ? t.yes(`${m.previous_model ?? "?"} → ${record.resolved_model}`) : t.no,
                ],
                [
                  t.warmToolChange,
                  m.tools_updated_on_warm_turn ? t.yes(formatBytes(m.tools_delta_xml_bytes)) : t.no,
                ],
                [
                  t.delta,
                  t.deltaValue(m.delta_message_count, m.total_message_count, formatBytes(m.delta_messages_bytes)),
                ],
                [
                  t.sent,
                  t.sentValue(
                    formatBytes(m.outbound_acp_prompt_bytes),
                    m.outbound_acp_text_blocks,
                    m.outbound_acp_image_blocks,
                  ),
                ],
                [t.session, record.session_id ?? "—"],
                ...(m.reasoning_effort
                  ? ([[t.effort, m.reasoning_effort]] as Array<readonly [string, React.ReactNode]>)
                  : []),
              ]}
            />
          </section>
          <Composition metrics={m} />
          <Disclosure head={<span className="font-medium text-fg-2">{t.fingerprint}</span>}>
            <KV
              items={[
                ...(m.previous_response_id
                  ? ([["previous_response_id", m.previous_response_id]] as Array<
                      readonly [string, React.ReactNode]
                    >)
                  : []),
                ...(m.prefix_hash_hex
                  ? ([[t.prefixHash, m.prefix_hash_hex]] as Array<readonly [string, React.ReactNode]>)
                  : []),
                [t.stateHash, m.base_state_hash_hex],
              ]}
            />
          </Disclosure>
        </>
      ) : (
        <KV
          items={[
            [t.model, record.resolved_model],
            [t.session, t.sessionValue(record.session_id ?? "—", record.session_reused)],
          ]}
        />
      )}
      {record.prompt_preview && <ProseBlock text={record.prompt_preview} title={t.lastPrompt} maxHeight={200} />}
    </>
  );
}

export function LowCacheSheet({
  record,
  replayTools = false,
  onClose,
}: {
  record: LowCacheHitRecord | null;
  /** Open straight on the turn's tool round-trips instead of the attribution tab. */
  replayTools?: boolean;
  onClose: () => void;
}): React.JSX.Element {
  const { client } = useGateway();
  const { m } = useI18n();
  const t = m.cache.sheet;
  const [tab, setTab] = useState<TabId>(replayTools ? "messages" : "why");
  const taskId = record?.task_id ?? null;
  const detail = useResource(
    taskId !== null ? () => client.lowCacheHitDetail(taskId) : null,
    `low-cache|${taskId ?? ""}|${record?.recorded_at_unix_ms ?? 0}`,
  );

  useEffect(() => {
    setTab(replayTools ? "messages" : "why");
  }, [taskId, replayTools]);

  const payload = detail.data?.full_payload ?? null;
  const capture = record?.request_capture ?? null;
  const messageCount =
    payload?.messages.length ??
    record?.request_metrics?.total_message_count ??
    capture?.message_digests.length ??
    0;
  const toolCount =
    record?.request_metrics?.client_tool_count ?? capture?.client_tool_names.length ?? 0;

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
            <span className="lg-num">{formatBpsPct(record.cache_hit_rate_bps, 2)}</span>
            <ReasonPills record={record} />
          </>
        )
      }
      description={
        record && (
          <span className="lg-num">
            {record.task_id} · {record.resolved_model} · {formatDateTime(record.recorded_at_unix_ms)}
          </span>
        )
      }
    >
      {record && (
        <>
          <Note icon={<Info />}>{explainLowCache(record)}</Note>

          <div className="flex items-end justify-between gap-3">
            <div className="min-w-0 flex-1">
              <Tabs<TabId>
                ariaLabel={t.tabsAria}
                value={tab}
                onChange={setTab}
                tabs={[
                  { value: "why", label: t.tabWhy },
                  { value: "request", label: t.tabRequest },
                  { value: "messages", label: t.tabMessages, count: messageCount },
                  { value: "tools", label: t.tabTools, count: toolCount },
                ]}
              />
            </div>
            <CopyButton value={() => JSON.stringify(detail.data ?? record, null, 2)} label={t.copyJson} />
          </div>

          {tab === "why" && <Why record={record} />}

          {tab !== "why" && detail.status === "loading" && <Loading label={t.loading} />}

          {tab === "request" && detail.status !== "loading" && (
            <>
              <AcpRequestView
                systemAppend={payload?.acp_system_append ?? capture?.acp_system_append_excerpt}
                toolsDelta={payload?.tools_delta_xml ?? capture?.tools_delta_xml_excerpt}
                promptBlocks={payload?.outbound_acp_prompt_blocks}
                promptExcerpt={capture?.outbound_acp_prompt_excerpt}
                excerptOnly={payload === null && capture !== null}
              />
              {(payload?.assistant_output_preview || capture?.output_preview) && (
                <ProseBlock
                  text={payload?.assistant_output_preview || capture?.output_preview || ""}
                  title={t.outputPreview}
                  maxHeight={240}
                />
              )}
              {payload?.payload_truncated && (
                <p className="lg-hint m-0">{t.truncated}</p>
              )}
            </>
          )}

          {tab === "messages" &&
            detail.status !== "loading" &&
            (payload && payload.messages.length > 0 ? (
              <MessageTimeline
                key={record.task_id}
                messages={payload.messages}
                initialScope={replayTools ? "tools" : undefined}
              />
            ) : capture && capture.message_digests.length > 0 ? (
              <div className="flex flex-col gap-3">
                <p className="lg-hint m-0">{t.digestsOnly}</p>
                {capture.message_digests.map((msg) => (
                  <article className="lg-msg" key={msg.index}>
                    <header className="lg-msg-head">
                      <span className="lg-role">
                        #{msg.index} {msg.role}
                      </span>
                      <Pill tone={msg.in_delta ? "accent" : "neutral"}>
                        {msg.in_delta ? t.inDelta : t.resident}
                      </Pill>
                      <span className="lg-num text-[11.5px]">
                        {msg.part_kinds.join(" · ")} · {formatBytes(msg.total_bytes)}
                      </span>
                    </header>
                    <pre className="lg-code lg-code-plain">{msg.excerpt || "—"}</pre>
                  </article>
                ))}
              </div>
            ) : (
              <p className="lg-hint m-0">{t.noMessages}</p>
            ))}

          {tab === "tools" && detail.status !== "loading" && (
            <>
              {capture && (
                <div className="flex flex-wrap gap-1.5">
                  <Tag>{t.webSearch(capture.enable_native_web_search)}</Tag>
                  <Tag>{t.webFetch(capture.enable_native_web_fetch)}</Tag>
                </div>
              )}
              <ClientToolsView
                tools={payload?.client_tools ?? []}
                fallbackNames={capture?.client_tool_names}
              />
            </>
          )}
        </>
      )}
    </Sheet>
  );
}
