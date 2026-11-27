import * as React from "react";
import { useState } from "react";
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
import { RouteFlow, ToolGlyph, ToolInputView, ToolOutputView } from "@/components/ToolExchange";
import { Button, Figure, HarnessBadge, Note, Pill, Segmented } from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { useResource } from "@/hooks/useResource";
import { formatBytes, formatDurationMs, formatNumber, formatSmartTime, shortId } from "@/lib/format";
import {
  displayToolName,
  errorKindLabel,
  isFailure,
  recordKind,
} from "@/lib/toolcalls";
import type { ToolCallDetail, ToolCallRecord } from "@/types/telemetry";

export function StatusPill({ record }: { record: ToolCallRecord }): React.JSX.Element {
  switch (record.status) {
    case "ok":
      return <Pill tone="ok">成功</Pill>;
    case "error":
      return (
        <Pill tone="err" icon={<CircleAlert />}>
          {errorKindLabel(record.error_kind)}
        </Pill>
      );
    case "preflight_rejected":
      return (
        <Pill tone="warn" icon={<ShieldAlert />}>
          预检拒绝
        </Pill>
      );
    case "pending":
      return (
        <Pill tone="accent" icon={<LoaderCircle className="lg-spin" />}>
          进行中
        </Pill>
      );
    case "abandoned":
      return <Pill title="会话被回收或回合中止，结果没有返回">未返回</Pill>;
    case "unknown":
      return <Pill>未知</Pill>;
  }
}

/** Title block shared by the docked pane and the mobile sheet. */
export function InspectorTitle({ record }: { record: ToolCallRecord }): React.JSX.Element {
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
          <span className="lg-num">
            轮 {record.turn_index} · 步 {record.step_index}
          </span>
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
  // Keyed on status too: a pending call is refetched the moment it lands.
  const detail = useResource(
    () => client.toolCallDetail(record.call_id),
    `${record.call_id}|${record.status}`,
  );
  const [argView, setArgView] = useState<"client" | "wire">("client");

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

  return (
    <div className="flex min-w-0 flex-col gap-4">
      <RouteFlow record={record} />

      <div className="grid grid-cols-3 gap-3">
        <Figure
          label="耗时"
          value={record.status === "pending" ? "进行中" : formatDurationMs(record.duration_ms)}
        />
        <Figure
          label="输入"
          value={formatBytes(record.input_bytes)}
          sub={`≈ ${formatNumber(record.input_tokens_est)} tokens`}
        />
        <Figure
          label="输出"
          value={formatBytes(record.output_bytes)}
          sub={`≈ ${formatNumber(record.output_tokens_est)} tokens`}
        />
      </div>
      {(record.v4a_auto_repaired || record.output_sanitized || record.output_spilled) && (
        <div className="flex flex-wrap gap-1.5">
          {record.v4a_auto_repaired && (
            <Pill tone="accent" icon={<Sparkles />} title="网关在转发前修正了 V4A 补丁语法">
              V4A 已自动修复
            </Pill>
          )}
          {record.output_sanitized && (
            <Pill icon={<Wrench />} title="输出中的终端控制符已被清洗">
              控制符已清洗
            </Pill>
          )}
          {record.output_spilled && (
            <Pill tone="warn" icon={<Scissors />} title="输出超过内联上限，已分片落盘">
              输出已 Spill
            </Pill>
          )}
        </div>
      )}

      {previewOnly && (
        <Note icon={<CircleAlert />}>
          {detail.status === "error"
            ? `完整记录读取失败（${detail.error ?? "未知错误"}），以下是列表里的截断摘要。`
            : "这条记录的完整内容已被环形缓冲淘汰，以下是列表里的截断摘要。"}
        </Note>
      )}

      {loading ? (
        <div className="lg-skeleton" role="status" aria-label="读取完整输入输出">
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
                ariaLabel="参数视角"
                value={argView}
                onChange={setArgView}
                options={[
                  { value: "client", label: "解包后" },
                  { value: "wire", label: "Wire 原始" },
                ]}
              />
            ) : undefined
          }
        />
        {full?.input_truncated && <p className="lg-hint m-0">输入超过单条记录上限，已被截断保存。</p>}

        <section className="flex min-w-0 flex-col gap-2">
          <h3 className="lg-section-title m-0">输出</h3>
          {record.status === "pending" ? (
            <p className="lg-hint m-0 flex items-center gap-1.5">
              <LoaderCircle className="lg-spin h-3.5 w-3.5" />
              等待工具返回…
            </p>
          ) : (
            <ToolOutputView blocks={shown.output_blocks} failed={failed} />
          )}
          {full?.output_truncated && <p className="lg-hint m-0">输出超过单条记录上限，已被截断保存。</p>}
        </section>
        </>
      )}

      <footer className="flex flex-wrap items-center justify-between gap-2 border-t border-line pt-3">
        <div className="flex flex-wrap items-center gap-1.5">
          {record.session_id && onFilterSession && (
            <Button size="sm" onClick={() => onFilterSession(record.session_id ?? "")}>
              <Filter />
              只看这个会话
            </Button>
          )}
          {record.session_id && onOpenCatalog && (
            <Button size="sm" onClick={() => onOpenCatalog(record.session_id ?? "")}>
              <Library />
              会话的工具目录
            </Button>
          )}
          <CopyButton value={() => JSON.stringify(full ?? record, null, 2)} label="复制 JSON" />
        </div>
        {(onPrev || onNext) && (
          <div className="flex items-center gap-1.5">
            {position && <span className="lg-num text-[12px] text-fg-3">{position}</span>}
            <Button size="sm" icon aria-label="上一条（K）" title="上一条（K）" disabled={!onPrev} onClick={onPrev}>
              <ArrowUp />
            </Button>
            <Button size="sm" icon aria-label="下一条（J）" title="下一条（J）" disabled={!onNext} onClick={onNext}>
              <ArrowDown />
            </Button>
          </div>
        )}
      </footer>
    </div>
  );
}
