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
import { REASON_LABEL, REASON_TONE, causeLabel, explainLowCache, routingLabel } from "@/lib/lowcache";
import type { LowCacheHitRecord, LowCacheRequestMetrics } from "@/types/telemetry";

type TabId = "why" | "request" | "messages" | "tools";

export function ReasonPills({ record }: { record: LowCacheHitRecord }): React.JSX.Element {
  const cause = causeLabel(record.diagnostic_cause);
  return (
    <span className="inline-flex flex-wrap items-center gap-1.5">
      <Pill tone={REASON_TONE[record.reason]}>{REASON_LABEL[record.reason]}</Pill>
      {cause !== null && <Tag>{cause}</Tag>}
    </span>
  );
}

function Composition({ metrics }: { metrics: LowCacheRequestMetrics }): React.JSX.Element {
  const segments = segmentsFromMetrics(metrics);
  const ranked = rankSegments(segments);
  return (
    <section>
      <h3 className="lg-section-title">会话内容构成 · {formatBytes(metrics.total_conversation_bytes)}</h3>
      <SegmentedBar parts={groupSegments(segments)} format={formatBytes} ariaLabel="会话内容按类别的体积构成" />
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
        另有客户端工具声明 {metrics.client_tool_count} 个 · {formatBytes(metrics.client_tools_schema_bytes)}
        （不直接发往上游，经虚拟化后挂载）。
      </p>
    </section>
  );
}

function Why({ record }: { record: LowCacheHitRecord }): React.JSX.Element {
  const m = record.request_metrics;
  return (
    <>
      <div className="grid grid-cols-2 gap-4 sm:grid-cols-4">
        <Figure
          label="缓存读 / 总输入"
          value={`${formatNumber(record.cached_read_tokens)} / ${formatNumber(record.total_prompt_tokens)}`}
          sub="tokens"
        />
        <Figure label="未命中输入" value={formatNumber(record.input_uncached_tokens)} sub="tokens" />
        <Figure
          label="输出 / 思考"
          value={`${formatNumber(record.output_tokens)} / ${formatNumber(m?.thought_tokens ?? 0)}`}
          sub="tokens"
        />
        <Figure
          label="耗时"
          value={formatDurationMs(record.elapsed_ms)}
          sub={`TTFT ${formatDurationMs(record.ttft_ms)}`}
        />
      </div>

      {m ? (
        <>
          <section>
            <h3 className="lg-section-title">路由与连续性</h3>
            <KV
              items={[
                ["路由方式", routingLabel(m.routing_strategy)],
                ["会话轮次", `第 ${m.session_turn_index} 轮 · ${record.session_reused ? "热复用" : "冷启动"}`],
                [
                  "检出前空闲",
                  m.session_idle_ms != null ? formatDurationMs(m.session_idle_ms) : "—（新建会话）",
                ],
                ["上轮上下文", `${formatNumber(m.previous_session_context_tokens)} tokens`],
                [
                  "模型切换",
                  m.model_switched ? `是 · ${m.previous_model ?? "?"} → ${record.resolved_model}` : "否",
                ],
                [
                  "热轮工具变更",
                  m.tools_updated_on_warm_turn ? `是 · ${formatBytes(m.tools_delta_xml_bytes)}` : "否",
                ],
                [
                  "本轮增量",
                  `${m.delta_message_count} / ${m.total_message_count} 条 · ${formatBytes(m.delta_messages_bytes)}`,
                ],
                [
                  "实际发送",
                  `${formatBytes(m.outbound_acp_prompt_bytes)} · ${m.outbound_acp_text_blocks} 文本块${
                    m.outbound_acp_image_blocks > 0 ? ` · ${m.outbound_acp_image_blocks} 图像块` : ""
                  }`,
                ],
                ["会话", record.session_id ?? "—"],
                ...(m.reasoning_effort
                  ? ([["推理强度", m.reasoning_effort]] as Array<readonly [string, React.ReactNode]>)
                  : []),
              ]}
            />
          </section>
          <Composition metrics={m} />
          <Disclosure head={<span className="font-medium text-fg-2">会话指纹</span>}>
            <KV
              items={[
                ...(m.previous_response_id
                  ? ([["previous_response_id", m.previous_response_id]] as Array<
                      readonly [string, React.ReactNode]
                    >)
                  : []),
                ...(m.prefix_hash_hex
                  ? ([["前缀哈希", m.prefix_hash_hex]] as Array<readonly [string, React.ReactNode]>)
                  : []),
                ["本轮状态哈希", m.base_state_hash_hex],
              ]}
            />
          </Disclosure>
        </>
      ) : (
        <KV
          items={[
            ["模型", record.resolved_model],
            ["会话", `${record.session_id ?? "—"} · ${record.session_reused ? "复用" : "新建"}`],
          ]}
        />
      )}
      {record.prompt_preview && <ProseBlock text={record.prompt_preview} title="末条用户输入摘要" maxHeight={200} />}
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
                ariaLabel="低命中记录详情"
                value={tab}
                onChange={setTab}
                tabs={[
                  { value: "why", label: "归因与构成" },
                  { value: "request", label: "发往 ACP 的请求" },
                  { value: "messages", label: "消息", count: messageCount },
                  { value: "tools", label: "工具声明", count: toolCount },
                ]}
              />
            </div>
            <CopyButton value={() => JSON.stringify(detail.data ?? record, null, 2)} label="复制 JSON" />
          </div>

          {tab === "why" && <Why record={record} />}

          {tab !== "why" && detail.status === "loading" && <Loading label="读取保存的请求载荷…" />}

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
                  title="模型输出摘要"
                  maxHeight={240}
                />
              )}
              {payload?.payload_truncated && (
                <p className="lg-hint m-0">载荷超过保存上限，已被截断。</p>
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
                <p className="lg-hint m-0">完整消息未保存，以下是每条消息的摘录。</p>
                {capture.message_digests.map((msg) => (
                  <article className="lg-msg" key={msg.index}>
                    <header className="lg-msg-head">
                      <span className="lg-role">
                        #{msg.index} {msg.role}
                      </span>
                      <Pill tone={msg.in_delta ? "accent" : "neutral"}>
                        {msg.in_delta ? "本轮增量" : "驻留前缀"}
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
              <p className="lg-hint m-0">这条记录没有保存消息序列。</p>
            ))}

          {tab === "tools" && detail.status !== "loading" && (
            <>
              {capture && (
                <div className="flex flex-wrap gap-1.5">
                  <Tag>原生联网搜索 {capture.enable_native_web_search ? "开" : "关"}</Tag>
                  <Tag>原生网页抓取 {capture.enable_native_web_fetch ? "开" : "关"}</Tag>
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
