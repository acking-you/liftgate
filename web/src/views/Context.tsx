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
import { useI18n } from "@/lib/i18n";
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
  const { m } = useI18n();
  const t = m.context.sheet;
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
            {t.description(
              record.resolved_model,
              shortId(record.session_id),
              record.session_turn_index,
              formatDateTime(record.recorded_at_unix_ms),
            )}
          </span>
        )
      }
    >
      {record && (
        <>
          <div className="grid grid-cols-2 gap-4 sm:grid-cols-4">
            <Figure
              label={t.hitRate}
              value={<HitRate bps={record.cache_hit_rate_bps} thresholdBps={thresholdBps} digits={2} />}
            />
            <Figure
              label={t.io}
              value={`${formatNumber(record.total_prompt_tokens)} / ${formatNumber(record.output_tokens)}`}
              sub="tokens"
            />
            <Figure
              label={t.messages}
              value={t.messageCount(record.message_count)}
              sub={t.deltaCount(record.delta_message_count)}
            />
            <Figure label={t.payload} value={formatBytes(record.payload_bytes)} />
          </div>

          <div className="flex items-end justify-between gap-3">
            <div className="min-w-0 flex-1">
              <Tabs<TabId>
                ariaLabel={t.tabsAria}
                value={tab}
                onChange={setTab}
                tabs={[
                  { value: "timeline", label: t.tabTimeline, count: payload?.messages.length ?? record.message_count },
                  { value: "request", label: t.tabRequest },
                  { value: "output", label: t.tabOutput },
                  { value: "tools", label: t.tabTools, count: payload?.client_tools.length },
                ]}
              />
            </div>
            {payload && <CopyButton value={() => JSON.stringify(detail.data, null, 2)} label={t.copyJson} />}
          </div>

          {detail.status === "loading" && <Loading label={t.loading} />}
          {detail.status === "error" && (
            <Note tone="err" icon={<CircleAlert />}>
              {t.readFailed(detail.error ?? "")}
            </Note>
          )}
          {detail.status === "unsupported" && <Note icon={<CircleAlert />}>{t.reclaimed}</Note>}

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
                <ProseBlock
                  text={payload.assistant_reasoning}
                  title={payload.assistant_reasoning_truncated ? t.reasoningCut : t.reasoning}
                  maxHeight={360}
                />
              )}
              <ProseBlock
                text={payload.assistant_output ?? payload.assistant_output_preview}
                title={
                  payload.assistant_output == null
                    ? t.outputPreview
                    : payload.assistant_output_truncated
                      ? t.outputCut
                      : t.output
                }
                maxHeight={520}
              />
            </div>
          )}
          {payload && tab === "tools" && <ClientToolsView tools={payload.client_tools} />}
          {payload?.payload_truncated && <p className="lg-hint m-0">{t.truncated}</p>}
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
  const { m } = useI18n();
  const t = m.context.control;
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
      onBanner({ tone: "err", text: t.failed(errorMessage(err)) });
    } finally {
      setBusy(false);
    }
  };

  const saveQuota = (ev: React.FormEvent): void => {
    ev.preventDefault();
    const records = Number.parseInt(maxRecords.trim(), 10);
    const mb = Number.parseInt(maxMb.trim(), 10);
    if (!Number.isFinite(records) || records < 1 || records > 10_000) {
      onBanner({ tone: "err", text: t.recordsRange });
      return;
    }
    if (!Number.isFinite(mb) || mb < 16 || mb > 65_536) {
      onBanner({ tone: "err", text: t.diskRange });
      return;
    }
    void apply(
      () => client.updateContextCapture({ max_records: records, max_total_mib: mb }),
      t.quotaSaved(formatNumber(records), formatNumber(mb)),
    ).then(() => setDirty(false));
  };

  return (
    <Card title={t.title}>
      <div className="grid grid-cols-1 gap-x-10 gap-y-5 xl:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]">
        <div className="flex flex-col gap-4">
          <SwitchRow
            title={summary.enabled ? t.on : t.off}
            hint={t.hint}
            checked={summary.enabled}
            disabled={busy}
            onChange={(next) =>
              void apply(
                () => client.updateContextCapture({ enabled: next }),
                next ? t.enabled : t.disabled,
              )
            }
          />
          {summary.capture_dir && (
            <p className="lg-hint m-0">
              {t.location} <span className="lg-num text-fg-2">{summary.capture_dir}</span>
            </p>
          )}
          {(summary.skipped_records ?? 0) > 0 && (
            <p className="lg-hint m-0 text-warn">
              {t.skipped(<span className="lg-num">{summary.skipped_records}</span>)}
            </p>
          )}
        </div>

        <form className="flex flex-col gap-3" onSubmit={saveQuota}>
          <div className="grid grid-cols-2 gap-3">
            <div>
              <div className="mb-1.5 flex items-baseline justify-between text-[12px] text-fg-3">
                <span>{t.records}</span>
                <span className="lg-num">
                  {formatNumber(summary.stored_records)} / {formatNumber(summary.max_records)}
                </span>
              </div>
              <Meter
                ratio={summary.max_records > 0 ? summary.stored_records / summary.max_records : 0}
                label={t.recordsQuota}
              />
            </div>
            <div>
              <div className="mb-1.5 flex items-baseline justify-between text-[12px] text-fg-3">
                <span>{t.disk}</span>
                <span className="lg-num">
                  {formatBytes(summary.stored_bytes)} / {formatBytes(summary.max_total_bytes)}
                </span>
              </div>
              <Meter
                ratio={summary.max_total_bytes > 0 ? summary.stored_bytes / summary.max_total_bytes : 0}
                tone={summary.stored_bytes / Math.max(1, summary.max_total_bytes) > 0.9 ? "warn" : undefined}
                label={t.diskQuota}
              />
            </div>
            <Field label={t.maxRecords} htmlFor="ctx-max-records">
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
            <Field label={t.maxMb} htmlFor="ctx-max-mb">
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
              {t.saveQuota}
            </Button>
            <ConfirmButton
              label={t.clear}
              confirmLabel={t.clearConfirm(formatNumber(summary.stored_records))}
              icon={<Trash2 />}
              disabled={busy || summary.stored_records === 0}
              onConfirm={() => void apply(() => client.clearContextCaptures(), t.cleared)}
            />
          </div>
        </form>
      </div>
    </Card>
  );
}

export function ContextView(): React.JSX.Element {
  const { client, snapshot, baseUrl } = useGateway();
  const { m } = useI18n();
  const t = m.context.view;
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
        <Loading label={t.waiting} />
      </Card>
    );
  }
  if (advertised === false || (captures.status === "unsupported" && !live)) {
    return (
      <Card>
        <Unsupported feature={t.unsupported} endpoint="GET /api/telemetry/context-captures">
          {t.unsupportedHint}
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
          <Loading label={t.loadingState} />
        </Card>
      )}

      <Card
        flush
        title={t.saved(records.length)}
        sub={t.savedSub}
      >
        {captures.status === "loading" ? (
          <Loading />
        ) : captures.status === "error" && captures.data === null ? (
          <div className="px-4 pb-4">
            <Note tone="err" icon={<CircleAlert />}>
              {t.readFailed(captures.error ?? "")}
            </Note>
          </div>
        ) : records.length === 0 ? (
          <EmptyState
            icon={<ScrollText />}
            title={summary?.enabled ? t.noneYet : t.off}
            hint={summary?.enabled ? t.noneYetHint : t.offHint}
          />
        ) : (
          <div className="lg-table-wrap">
            <table className="lg-table">
              <thead>
                <tr>
                  <th>{t.col.time}</th>
                  <th>{t.col.harness}</th>
                  <th className="hide-sm">{t.col.model}</th>
                  <th className="num hide-sm">{t.col.turn}</th>
                  <th className="num">{t.col.hitRate}</th>
                  <th className="num hide-sm">{t.col.io}</th>
                  <th className="num hide-sm">{t.col.messages}</th>
                  <th className="num">{t.col.payload}</th>
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
                      <span className="text-fg-4">{t.delta(rec.delta_message_count)}</span>
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
