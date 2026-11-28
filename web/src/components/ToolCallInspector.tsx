import * as React from "react";
import { useEffect, useRef, useState } from "react";
import {
  ArrowDown,
  ArrowUp,
  CircleAlert,
  Filter,
  Library,
  LoaderCircle,
  Scissors,
  ShieldAlert,
  Sparkles,
  Wrench,
} from "lucide-react";
import { CopyButton } from "@/components/CopyButton";
import { DiffBlock, sizeChange } from "@/components/ContextRewrites";
import { RouteFlow, ToolGlyph, ToolInputView, ToolOutputView } from "@/components/ToolExchange";
import { Button, Figure, HarnessBadge, Latency, Note, Pill, Segmented } from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { useRevisionResource } from "@/hooks/useResource";
import { byteLength, formatBytes, formatNumber, formatSmartTime, shortId } from "@/lib/format";
import { useI18n } from "@/lib/i18n";
import {
  displayToolName,
  errorKindLabel,
  isFailure,
  recordKind,
} from "@/lib/toolcalls";
import type { ToolCallDetail, ToolCallRecord } from "@/types/telemetry";

export function StatusPill({ record }: { record: ToolCallRecord }): React.JSX.Element {
  const { m } = useI18n();
  const s = m.tools.status;
  switch (record.status) {
    case "ok":
      return <Pill tone="ok">{s.ok}</Pill>;
    case "error":
      return (
        <Pill tone="err" icon={<CircleAlert />}>
          {errorKindLabel(record.error_kind)}
        </Pill>
      );
    case "preflight_rejected":
      return (
        <Pill tone="warn" icon={<ShieldAlert />}>
          {s.preflight}
        </Pill>
      );
    case "pending":
      return (
        <Pill tone="accent" icon={<LoaderCircle className="lg-spin" />}>
          {s.pending}
        </Pill>
      );
    case "abandoned":
      return (
        <Pill tone="warn" title={s.abandonedHint}>
          {s.abandoned}
        </Pill>
      );
    case "unknown":
      return <Pill>{s.unknown}</Pill>;
  }
}

/** Title block shared by the docked pane and the mobile sheet. */
export function InspectorTitle({ record }: { record: ToolCallRecord }): React.JSX.Element {
  const { m } = useI18n();
  const kind = recordKind(record);
  return (
    <div className="flex min-w-0 items-center gap-2.5">
      <ToolGlyph kind={kind} failed={isFailure(record)} size="lg" />
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <span className="lg-num truncate text-[15px] font-semibold text-fg">
            {displayToolName(record)}
          </span>
          <StatusPill record={record} />
        </div>
        <div className="mt-0.5 flex flex-wrap items-center gap-x-2 gap-y-0.5 text-[12px] text-fg-3">
          <HarnessBadge id={record.harness_profile} />
          <span className="lg-num" title={record.session_id ?? undefined}>
            {shortId(record.session_id)}
          </span>
          <span className="lg-num">{m.tools.inspector.turnStep(record.turn_index, record.step_index)}</span>
          <span className="lg-num">{formatSmartTime(record.started_at_unix_ms)}</span>
        </div>
      </div>
    </div>
  );
}

function previewDetail(record: ToolCallRecord): ToolCallDetail {
  return {
    ...record,
    wire_arguments: record.input_preview,
    client_arguments: null,
    output_blocks:
      record.output_preview.length > 0
        ? [{ type: "text", bytes: record.output_bytes, text: record.output_preview }]
        : [],
  };
}

/**
 * Everything about one tool call: the route it took, what went in, what came
 * back. Renders without chrome so it can sit in a docked pane or a sheet.
 */
export function ToolCallInspector({
  record,
  position,
  onPrev,
  onNext,
  onOpenCatalog,
  onFilterSession,
}: {
  record: ToolCallRecord;
  /** "12 / 60" style position inside the visible list. */
  position?: string;
  onPrev?: () => void;
  onNext?: () => void;
  onOpenCatalog?: (sessionId: string) => void;
  onFilterSession?: (sessionId: string) => void;
}): React.JSX.Element {
  const { client } = useGateway();
  const { m } = useI18n();
  const t = m.tools.inspector;
  // Refetch in place whenever status or revision advances (for example when a
  // pending call completes or post-completion spill shaping attaches forwarded_output).
  const detail = useRevisionResource(
    () => client.toolCallDetail(record.call_id),
    record.call_id,
    `${record.status}|${record.revision}`,
  );
  const [argView, setArgView] = useState<"client" | "wire">("client");
  const [outView, setOutView] = useState<"raw" | "diff" | "forwarded">(
    record.output_spilled ? "diff" : "raw",
  );
  const outputSectionRef = useRef<HTMLElement>(null);
  useEffect(() => {
    if (record.output_spilled) setOutView("diff");
  }, [record.output_spilled]);

  const full = detail.status === "ready" ? detail.data : null;
  const shown = full ?? previewDetail(record);
  const loading = full === null && (detail.status === "loading" || detail.status === "idle");
  const previewOnly = full === null && !loading;
  const unwrapped =
    full !== null &&
    typeof full.client_arguments === "string" &&
    full.client_arguments !== full.wire_arguments;
  const showWire = unwrapped && argView === "wire";
  const inputRaw = showWire ? shown.wire_arguments : (shown.client_arguments ?? shown.wire_arguments);
  const inputTool = showWire ? record.wire_tool_name : displayToolName(record);
  const failed = isFailure(record);
  const primaryOutputText = shown.output_blocks
    .filter((b): b is Extract<typeof b, { type: "text" }> => b.type === "text")
    .map((b) => b.text)
    .join("\n");
  const forwardedOutput = full?.forwarded_output ?? null;
  const hasForwardedDiff =
    forwardedOutput !== null && forwardedOutput !== primaryOutputText;
  const effectiveOutView = hasForwardedDiff ? outView : "raw";
  const forwardedBytes =
    full?.forwarded_output_bytes ?? (forwardedOutput !== null ? byteLength(forwardedOutput) : null);
  const spillChange =
    hasForwardedDiff && forwardedBytes !== null
      ? sizeChange(record.output_bytes, forwardedBytes)
      : null;
  const imageBlocks = shown.output_blocks.filter((b) => b.type === "image");

  return (
    <div className="flex min-w-0 flex-col gap-4">
      <RouteFlow record={record} />

      <div className="grid grid-cols-3 gap-3">
        <Figure
          label={t.duration}
          value={record.status === "pending" ? t.running : <Latency ms={record.duration_ms} kind="tool" />}
        />
        <Figure
          label={t.input}
          value={formatBytes(record.input_bytes)}
          sub={`≈ ${formatNumber(record.input_tokens_est)} tokens`}
        />
        <Figure
          label={t.output}
          value={formatBytes(record.output_bytes)}
          sub={`≈ ${formatNumber(record.output_tokens_est)} tokens`}
        />
      </div>
      {(record.v4a_auto_repaired || record.output_sanitized || record.output_spilled) && (
        <div className="flex flex-wrap gap-1.5">
          {record.v4a_auto_repaired && (
            <Pill tone="accent" icon={<Sparkles />} title={t.v4aHint}>
              {t.v4a}
            </Pill>
          )}
          {record.output_sanitized && (
            <Pill icon={<Wrench />} title={t.sanitizedHint}>
              {t.sanitized}
            </Pill>
          )}
          {record.output_spilled && (
            hasForwardedDiff ? (
              <button
                type="button"
                className="lg-pill-btn"
                title={t.spilledJumpHint}
                onClick={() => {
                  setOutView("diff");
                  outputSectionRef.current?.scrollIntoView({ block: "nearest", behavior: "smooth" });
                }}
              >
                <Pill tone="warn" icon={<Scissors />}>
                  {t.spilled}
                </Pill>
              </button>
            ) : (
              <Pill tone="warn" icon={<Scissors />} title={t.spilledHint}>
                {t.spilled}
              </Pill>
            )
          )}
        </div>
      )}

      {previewOnly && (
        <Note icon={<CircleAlert />}>
          {detail.status === "error"
            ? t.readFailed(detail.error ?? t.unknownError)
            : t.evicted}
        </Note>
      )}

      {loading ? (
        <div className="lg-skeleton" role="status" aria-label={t.loadingAria}>
          <span />
          <span />
          <span />
        </div>
      ) : (
        <>
        <ToolInputView
          key={`${record.call_id}-${showWire ? "wire" : "client"}`}
          toolName={inputTool}
          raw={inputRaw}
          isOrchestrator={record.is_orchestrator}
          language={record.code_language}
          aside={
            unwrapped ? (
              <Segmented<"client" | "wire">
                ariaLabel={t.argView}
                value={argView}
                onChange={setArgView}
                options={[
                  { value: "client", label: t.unwrapped },
                  { value: "wire", label: t.wire },
                ]}
              />
            ) : undefined
          }
        />
        {full?.input_truncated && <p className="lg-hint m-0">{t.inputTruncated}</p>}

        <section ref={outputSectionRef} className="flex min-w-0 flex-col gap-2">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <div className="flex min-w-0 items-center gap-2">
              <h3 className="lg-section-title m-0">{t.output}</h3>
              {hasForwardedDiff && forwardedBytes !== null && (
                <span className="lg-num text-[12px] text-fg-3">
                  {formatBytes(record.output_bytes)} → {formatBytes(forwardedBytes)}
                  {spillChange && (
                    <span className={spillChange.shrank ? "ml-1.5 text-ok-fg" : "ml-1.5 text-warn-fg"}>
                      {spillChange.shrank ? "−" : "+"}
                      {spillChange.pct}
                    </span>
                  )}
                </span>
              )}
            </div>
            {hasForwardedDiff && record.status !== "pending" && (
              <Segmented<"raw" | "diff" | "forwarded">
                ariaLabel={t.outputView}
                value={effectiveOutView}
                onChange={setOutView}
                options={[
                  { value: "raw", label: t.outOriginal },
                  { value: "diff", label: t.outDiff },
                  { value: "forwarded", label: t.outForwarded },
                ]}
              />
            )}
          </div>
          {record.status === "pending" ? (
            <p className="lg-hint m-0 flex items-center gap-1.5">
              <LoaderCircle className="lg-spin h-3.5 w-3.5" />
              {t.waiting}
            </p>
          ) : effectiveOutView === "diff" && forwardedOutput !== null ? (
            <DiffBlock original={primaryOutputText} rewritten={forwardedOutput} maxHeight={460} />
          ) : effectiveOutView === "forwarded" && forwardedOutput !== null ? (
            <ToolOutputView
              blocks={[
                { type: "text", bytes: forwardedBytes ?? byteLength(forwardedOutput), text: forwardedOutput },
                ...imageBlocks,
              ]}
              failed={failed}
            />
          ) : (
            <ToolOutputView blocks={shown.output_blocks} failed={failed} />
          )}
          {full?.output_truncated && <p className="lg-hint m-0">{t.outputTruncated}</p>}
          {full?.forwarded_output_truncated && <p className="lg-hint m-0">{t.forwardedTruncated}</p>}
        </section>
        </>
      )}

      <footer className="flex flex-wrap items-center justify-between gap-2 border-t border-line pt-3">
        <div className="flex flex-wrap items-center gap-1.5">
          {record.session_id && onFilterSession && (
            <Button size="sm" onClick={() => onFilterSession(record.session_id ?? "")}>
              <Filter />
              {t.onlySession}
            </Button>
          )}
          {record.session_id && onOpenCatalog && (
            <Button size="sm" onClick={() => onOpenCatalog(record.session_id ?? "")}>
              <Library />
              {t.catalog}
            </Button>
          )}
          <CopyButton value={() => JSON.stringify(full ?? record, null, 2)} label={t.copyJson} />
        </div>
        {(onPrev || onNext) && (
          <div className="flex items-center gap-1.5">
            {position && <span className="lg-num text-[12px] text-fg-3">{position}</span>}
            <Button size="sm" icon aria-label={t.prev} title={t.prev} disabled={!onPrev} onClick={onPrev}>
              <ArrowUp />
            </Button>
            <Button size="sm" icon aria-label={t.next} title={t.next} disabled={!onNext} onClick={onNext}>
              <ArrowDown />
            </Button>
          </div>
        )}
      </footer>
    </div>
  );
}
