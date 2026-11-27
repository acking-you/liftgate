import * as React from "react";
import { useEffect, useState } from "react";
import { CircleAlert, CircleCheck, ScrollText, Trash2 } from "lucide-react";
import { ProseBlock } from "@/components/CodeBlock";
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
  SwitchRow,
  Tabs,
  Unsupported,
} from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { useResource, useRevisionResource } from "@/hooks/useResource";
import { errorMessage } from "@/lib/api";
import { formatBytes, formatDateTime, formatNumber, formatSmartTime, shortId } from "@/lib/format";
import type { ContextCaptureRecord, ContextCaptureSummary } from "@/types/telemetry";

const MB = 1024 * 1024;

interface Banner {
  tone: "ok" | "err";
  text: string;
}

type TabId = "timeline" | "request" | "output" | "tools";

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
  const [tab, setTab] = useState<TabId>("timeline");
  const taskId = record?.task_id ?? null;
  const detail = useResource(
    taskId !== null ? () => client.contextCaptureDetail(taskId) : null,
    `context|${taskId ?? ""}`,
  );
  useEffect(() => {
    setTab("timeline");
  }, [taskId]);
  const payload = detail.data?.payload ?? null;

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
          </>
        )
      }
      description={
        record && (
          <span className="lg-num">
            {record.resolved_model} · 会话 {shortId(record.session_id)} 第 {record.session_turn_index} 轮 ·{" "}
            {formatDateTime(record.recorded_at_unix_ms)}
          </span>
        )
      }
    >
      {record && (
        <>
          <div className="grid grid-cols-2 gap-4 sm:grid-cols-4">
            <Figure
              label="缓存命中率"
              value={<HitRate bps={record.cache_hit_rate_bps} thresholdBps={thresholdBps} digits={2} />}
            />
            <Figure
              label="输入 / 输出"
              value={`${formatNumber(record.total_prompt_tokens)} / ${formatNumber(record.output_tokens)}`}
              sub="tokens"
            />
            <Figure
              label="消息"
              value={`${record.message_count} 条`}
              sub={`其中本轮增量 ${record.delta_message_count} 条`}
            />
            <Figure label="载荷" value={formatBytes(record.payload_bytes)} />
          </div>

          <div className="flex items-end justify-between gap-3">
            <div className="min-w-0 flex-1">
              <Tabs<TabId>
                ariaLabel="完整上下文"
                value={tab}
                onChange={setTab}
                tabs={[
                  { value: "timeline", label: "时间线", count: payload?.messages.length ?? record.message_count },
                  { value: "request", label: "发往 ACP 的请求" },
                  { value: "output", label: "模型输出" },
                  { value: "tools", label: "工具声明", count: payload?.client_tools.length },
                ]}
              />
            </div>
            {payload && <CopyButton value={() => JSON.stringify(detail.data, null, 2)} label="复制 JSON" />}
          </div>

          {detail.status === "loading" && <Loading label="读取完整上下文…" />}
          {detail.status === "error" && (
            <Note tone="err" icon={<CircleAlert />}>
              读取失败：{detail.error}
            </Note>
          )}
          {detail.status === "unsupported" && (
            <Note icon={<CircleAlert />}>这份载荷已被配额回收，只剩下列表里的摘要。</Note>
          )}

          {payload && tab === "timeline" && <MessageTimeline key={record.task_id} messages={payload.messages} />}
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
                <ProseBlock text={payload.assistant_reasoning} title="思考" maxHeight={360} />
              )}
              <ProseBlock
                text={payload.assistant_output ?? payload.assistant_output_preview}
                title={payload.assistant_output != null ? "输出" : "输出摘要"}
                maxHeight={520}
              />
            </div>
          )}
          {payload && tab === "tools" && <ClientToolsView tools={payload.client_tools} />}
          {payload?.payload_truncated && <p className="lg-hint m-0">载荷超过保存上限，已被截断。</p>}
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
      onBanner({ tone: "err", text: `操作失败：${errorMessage(err)}` });
    } finally {
      setBusy(false);
    }
  };

  const saveQuota = (ev: React.FormEvent): void => {
    ev.preventDefault();
    const records = Number.parseInt(maxRecords.trim(), 10);
    const mb = Number.parseInt(maxMb.trim(), 10);
    if (!Number.isFinite(records) || records < 1 || records > 10_000) {
      onBanner({ tone: "err", text: "保留条数需在 1 到 10,000 之间。" });
      return;
    }
    if (!Number.isFinite(mb) || mb < 16 || mb > 65_536) {
      onBanner({ tone: "err", text: "磁盘上限需在 16 到 65,536 MB 之间。" });
      return;
    }
    void apply(
      () => client.updateContextCapture({ max_records: records, max_total_mib: mb }),
      `配额已更新：最多 ${formatNumber(records)} 条、${formatNumber(mb)} MB，超出后回收最旧的载荷。`,
    ).then(() => setDirty(false));
  };

  return (
    <Card title="完整上下文捕获">
      <div className="grid grid-cols-1 gap-x-10 gap-y-5 xl:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]">
        <div className="flex flex-col gap-4">
          <SwitchRow
            title={summary.enabled ? "正在捕获每一轮" : "未在捕获"}
            hint="开启后每一轮都落盘完整消息历史、思考、发往 ACP 的 Prompt 与输出。长上下文一轮可达数 MB，排查问题时再打开。"
            checked={summary.enabled}
            disabled={busy}
            onChange={(next) =>
              void apply(
                () => client.updateContextCapture({ enabled: next }),
                next ? "已开启，之后的每一轮都会保存完整上下文。" : "已关闭，不再保存新的上下文；已有的载荷保留。",
              )
            }
          />
          {summary.capture_dir && (
            <p className="lg-hint m-0">
              保存位置 <span className="lg-num text-fg-2">{summary.capture_dir}</span>
            </p>
          )}
          {(summary.skipped_records ?? 0) > 0 && (
            <p className="lg-hint m-0 text-warn">
              磁盘写入跟不上，已跳过 <span className="lg-num">{summary.skipped_records}</span> 轮。
            </p>
          )}
        </div>

        <form className="flex flex-col gap-3" onSubmit={saveQuota}>
          <div className="grid grid-cols-2 gap-3">
            <div>
              <div className="mb-1.5 flex items-baseline justify-between text-[12px] text-fg-3">
                <span>条数</span>
                <span className="lg-num">
                  {formatNumber(summary.stored_records)} / {formatNumber(summary.max_records)}
                </span>
              </div>
              <Meter ratio={summary.max_records > 0 ? summary.stored_records / summary.max_records : 0} label="已用条数配额" />
            </div>
            <div>
              <div className="mb-1.5 flex items-baseline justify-between text-[12px] text-fg-3">
                <span>磁盘</span>
                <span className="lg-num">
                  {formatBytes(summary.stored_bytes)} / {formatBytes(summary.max_total_bytes)}
                </span>
              </div>
              <Meter
                ratio={summary.max_total_bytes > 0 ? summary.stored_bytes / summary.max_total_bytes : 0}
                tone={summary.stored_bytes / Math.max(1, summary.max_total_bytes) > 0.9 ? "warn" : undefined}
                label="已用磁盘配额"
              />
            </div>
            <Field label="最多保留条数" htmlFor="ctx-max-records">
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
            <Field label="磁盘上限 MB" htmlFor="ctx-max-mb">
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
              保存配额
            </Button>
            <ConfirmButton
              label="清空已保存的上下文"
              confirmLabel={`确认删除 ${formatNumber(summary.stored_records)} 份载荷`}
              icon={<Trash2 />}
              disabled={busy || summary.stored_records === 0}
              onConfirm={() => void apply(() => client.clearContextCaptures(), "已清空全部保存的上下文载荷。")}
            />
          </div>
        </form>
      </div>
    </Card>
  );
}

export function ContextView(): React.JSX.Element {
  const { client, snapshot, baseUrl } = useGateway();
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
  const records = captures.data?.records ?? [];

  if (advertised === null) {
    return (
      <Card>
        <Loading label="等待网关上报状态…" />
      </Card>
    );
  }
  if (advertised === false || (captures.status === "unsupported" && !live)) {
    return (
      <Card>
        <Unsupported feature="完整上下文捕获" endpoint="GET /api/telemetry/context-captures">
          {" "}
          低命中轮次的完整载荷可在「缓存」页查看。
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

      {summary ? (
        <ControlCard summary={summary} onBanner={setBanner} onChanged={captures.reload} />
      ) : (
        <Card>
          <Loading label="读取捕获状态…" />
        </Card>
      )}

      <Card
        flush
        title={`已保存的上下文 · ${records.length}`}
        sub="一行一轮，点开查看时间线。"
      >
        {captures.status === "loading" ? (
          <Loading />
        ) : captures.status === "error" && captures.data === null ? (
          <div className="px-4 pb-4">
            <Note tone="err" icon={<CircleAlert />}>
              读取失败：{captures.error}
            </Note>
          </div>
        ) : records.length === 0 ? (
          <EmptyState
            icon={<ScrollText />}
            title={summary?.enabled ? "还没有捕获到任何一轮" : "捕获处于关闭状态"}
            hint={
              summary?.enabled
                ? "下一轮请求完成后，它的完整上下文会出现在这里。"
                : "打开上面的开关后，之后每一轮请求的完整上下文都会保存下来。"
            }
          />
        ) : (
          <div className="lg-table-wrap">
            <table className="lg-table">
              <thead>
                <tr>
                  <th>时间</th>
                  <th>Harness · 会话</th>
                  <th className="hide-sm">模型</th>
                  <th className="num hide-sm">轮次</th>
                  <th className="num">命中率</th>
                  <th className="num hide-sm">输入 / 输出</th>
                  <th className="num hide-sm">消息（增量）</th>
                  <th className="num">载荷</th>
                  <th className="hide-sm">Prompt</th>
                </tr>
              </thead>
              <tbody>
                {records.map((rec) => (
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
                      <HarnessBadge id={rec.harness_profile} className="text-fg" />
                      <span className="lg-num ml-2 text-[11.5px] text-fg-4 max-md:hidden">
                        {shortId(rec.session_id)}
                      </span>
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
                      <span className="text-fg-4">（{rec.delta_message_count}）</span>
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

      <ContextSheet record={selected} thresholdBps={thresholdBps} onClose={() => setSelected(null)} />
    </>
  );
}
