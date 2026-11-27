import * as React from "react";
import { useState } from "react";
import { CircleAlert, Layers, Library, LoaderCircle, Trash2 } from "lucide-react";
import { CopyButton } from "@/components/CopyButton";
import {
  Button,
  Card,
  Dot,
  EmptyState,
  Figure,
  HarnessBadge,
  HitRate,
  KV,
  Meter,
  Note,
  Pill,
  SwitchRow,
  Tag,
} from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { errorMessage } from "@/lib/api";
import { formatBytes, formatNumber, formatSmartTime, formatUptime, shortId } from "@/lib/format";
import { orchestratorModeLabel } from "@/lib/harness";
import { WIRE_BUDGET_BYTES } from "@/lib/stats";
import type { PromptCacheWarmth, SessionSummary } from "@/types/telemetry";
import type { ViewProps } from "@/views/types";

const WARMTH: Record<PromptCacheWarmth, { label: string; tone: "ok" | "warn" | "neutral" }> = {
  active: { label: "活跃", tone: "ok" },
  hot: { label: "热 · 1 分钟内", tone: "ok" },
  warm: { label: "温 · 1–5 分钟", tone: "warn" },
  cold: { label: "冷 · 超过 5 分钟", tone: "neutral" },
  unknown: { label: "未知", tone: "neutral" },
};

function SessionState({ session }: { session: SessionSummary }): React.JSX.Element {
  const warmth = WARMTH[session.cache_warmth ?? "unknown"];
  if (session.state === "running") {
    return (
      <Pill tone="accent" icon={<LoaderCircle className="lg-spin" />}>
        执行中
      </Pill>
    );
  }
  return (
    <span className="inline-flex flex-wrap items-center gap-1.5">
      {session.state === "suspended" && (
        <Pill tone="accent" title="模型已发出工具调用，会话挂起等待客户端返回结果">
          挂起等工具
        </Pill>
      )}
      <span className="inline-flex items-center gap-1.5 whitespace-nowrap text-[12px] text-fg-2" title="上游 Prompt 缓存温度">
        <Dot tone={warmth.tone} />
        {warmth.label}
      </span>
    </span>
  );
}

export function SessionsView({ navigate }: ViewProps): React.JSX.Element {
  const { client, snapshot, refresh } = useGateway();
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const acp = snapshot?.acp_process;
  const pool = snapshot?.session_pool;
  const sessions = pool?.sessions ?? [];
  const mcpBridge = snapshot?.mcp_tool_bridge_enabled ?? true;
  const visionOffload = snapshot?.vision_offload_enabled ?? false;
  const thresholdBps = snapshot?.cache_telemetry?.low_cache_threshold_bps ?? 5000;
  const parked = (pool?.idle_cached_sessions ?? 0) + (pool?.suspended_mcp_sessions ?? 0);

  const run = async (key: string, action: () => Promise<unknown>): Promise<void> => {
    setBusy(key);
    setError(null);
    try {
      await action();
      await refresh();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(null);
    }
  };

  return (
    <>
      {error && (
        <Note tone="err" icon={<CircleAlert />}>
          操作失败：{error}
        </Note>
      )}

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-[minmax(0,1.6fr)_minmax(0,1fr)]">
        <Card
          title={acp?.agent_name ?? "Antigravity ACP Server"}
          sub={acp ? `进程隔离：${acp.supervision_strategy}` : undefined}
          actions={
            acp && (
              <>
                <Pill tone={acp.connected ? "ok" : "err"}>
                  {acp.connected ? `已连接${acp.pid !== null ? ` · PID ${acp.pid}` : ""}` : "已断开"}
                </Pill>
                <Tag>
                  {acp.os} / {acp.arch}
                </Tag>
                {acp.agent_version && <Tag>agy {acp.agent_version}</Tag>}
              </>
            )
          }
        >
          {acp ? (
            <div className="flex flex-col gap-5">
              <div className="grid grid-cols-2 gap-4 sm:grid-cols-4">
                <Figure label="运行时长" value={formatUptime(acp.uptime_secs)} sub={`协议 v${acp.protocol_version}`} />
                <Figure
                  label="JSON-RPC 请求 / 响应"
                  value={`${formatNumber(acp.rpc_requests_sent)} / ${formatNumber(acp.rpc_responses_received)}`}
                  sub={`${acp.rpc_pending_in_flight} 个在途`}
                />
                <Figure
                  label="流式通知帧"
                  value={formatNumber(acp.rpc_notifications_received)}
                  sub={`${acp.active_prompt_streams} 条活跃流`}
                />
                <Figure label="默认模型" value={<span className="lg-num text-[13px]">{acp.default_model}</span>} sub={`模式 ${acp.acp_mode}`} />
              </div>
              <KV
                items={[
                  ["agy 可执行文件", acp.binary_path],
                  ["隔离工作目录", acp.session_cwd],
                  ["出站代理", acp.agy_proxy ? acp.agy_proxy : "直连"],
                  [
                    "空闲自动更新",
                    `${(acp.auto_update_enabled ?? true) ? "开" : "关"}${
                      acp.latest_known_version ? ` · 上游 ${acp.latest_known_version}` : ""
                    }${acp.auto_update_note ? ` · ${acp.auto_update_note}` : ""}`,
                  ],
                ]}
              />
            </div>
          ) : (
            <p className="lg-hint m-0">等待网关上报 ACP 进程状态…</p>
          )}
        </Card>

        <Card title="运行开关">
          <div className="flex flex-col gap-4">
            <SwitchRow
              title="MCP 工具桥接"
              hint="经本地回环 MCP 把客户端工具原生挂给模型。关闭后退回 XML 工具协议。"
              checked={mcpBridge}
              disabled={busy !== null}
              onChange={(next) => void run("mcp", () => client.setMcpToolBridge(next))}
            />
            <div className="border-t border-line" />
            <SwitchRow
              title="长上下文视觉卸载"
              hint="上下文超过 300k tokens 后，图像交给独立的视觉子会话处理。"
              checked={visionOffload}
              disabled={busy !== null}
              onChange={(next) => void run("vision", () => client.setVisionOffload(next))}
            />
          </div>
        </Card>
      </div>

      <Card
        flush
        title={`会话池 · ${sessions.length}`}
        sub={
          <>
            累计新建 {formatNumber(pool?.total_sessions_created ?? 0)} · 复用{" "}
            {formatNumber(pool?.total_sessions_reused ?? 0)} · 关闭或淘汰{" "}
            {formatNumber(pool?.total_sessions_closed_or_evicted ?? 0)}
          </>
        }
        actions={
          <>
            <Tag title="空闲与挂起会话占驻留上限的数量">
              驻留 {parked} / {pool?.max_idle_sessions ?? 0}
            </Tag>
            <Tag title="因工具调用挂起后被原生续接的次数">
              MCP 续接 {formatNumber(pool?.mcp_continuations ?? 0)}
            </Tag>
            <Tag title="为 previous_response_id 续接保存的响应">
              响应 {pool?.stored_responses ?? 0} / {pool?.max_stored_responses ?? 0}
            </Tag>
          </>
        }
      >
        {sessions.length === 0 ? (
          <EmptyState
            icon={<Layers />}
            title="会话池是空的"
            hint="第一轮多轮对话到达后，网关会为它保留一个独立的 ACP 子进程，供后续轮次热复用。"
          />
        ) : (
          <div className="lg-table-wrap">
            <table className="lg-table">
              <thead>
                <tr>
                  <th>会话</th>
                  <th className="hide-sm">状态 · 缓存温度</th>
                  <th className="hide-sm">模型</th>
                  <th>工具虚拟化</th>
                  <th className="num">命中率</th>
                  <th className="num hide-sm">上下文</th>
                  <th className="num hide-sm">轮 / 消息</th>
                  <th className="num hide-sm">最近活跃</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {sessions.map((sess) => (
                  <tr key={sess.session_id}>
                    <td className="whitespace-nowrap">
                      <span className="inline-flex items-center gap-1">
                        <span className="lg-num text-[12px] text-fg" title={sess.session_id}>
                          {shortId(sess.session_id)}
                        </span>
                        <span className="max-md:hidden">
                          <CopyButton value={sess.session_id} title="复制会话 ID" />
                        </span>
                      </span>
                      {/* On phones the state rides under the id instead of taking a column. */}
                      <div className="mt-1 md:hidden">
                        <SessionState session={sess} />
                      </div>
                    </td>
                    <td className="hide-sm">
                      <SessionState session={sess} />
                    </td>
                    <td className="lg-num hide-sm whitespace-nowrap text-[12px]">{sess.model_id}</td>
                    <td>
                      {sess.mcp ? (
                        <div className="md:min-w-[190px]">
                          <div className="flex flex-wrap items-center gap-1.5 text-[12px]">
                            <HarnessBadge id={sess.mcp.harness_profile} className="font-medium text-fg" />
                            <span className="inline-flex gap-1.5 max-md:hidden">
                              <Tag>{orchestratorModeLabel(sess.mcp.orchestrator_mode)}</Tag>
                              <Tag>{sess.mcp.is_passthrough ? "1:1 直通" : `Hub × ${sess.mcp.virtualized_hubs}`}</Tag>
                            </span>
                          </div>
                          <div className="mt-1.5 flex items-center gap-2 max-md:hidden">
                            <Meter
                              ratio={sess.mcp.wire_bytes / WIRE_BUDGET_BYTES}
                              tone={sess.mcp.wire_bytes / WIRE_BUDGET_BYTES > 0.9 ? "warn" : undefined}
                              label="Wire 工具体积占预算比例"
                            />
                            <span className="lg-num flex-none text-[11.5px] text-fg-3">
                              {formatBytes(sess.mcp.wire_bytes)} · Eager {sess.mcp.eager_tools} / {sess.mcp.declared_tools}
                            </span>
                          </div>
                        </div>
                      ) : (
                        <span className="text-[12px] text-fg-4">无工具 / XML</span>
                      )}
                    </td>
                    <td className="num">
                      <HitRate bps={sess.cache_hit_rate_bps} thresholdBps={thresholdBps} />
                    </td>
                    <td className="num hide-sm">
                      {(sess.last_context_tokens ?? 0) > 0 ? formatNumber(sess.last_context_tokens ?? 0) : "—"}
                    </td>
                    <td className="num hide-sm">
                      {sess.turn_count} / {sess.message_count}
                    </td>
                    <td className="num hide-sm whitespace-nowrap">{formatSmartTime(sess.last_active_unix_ms)}</td>
                    <td className="whitespace-nowrap text-right">
                      <span className="inline-flex gap-1.5">
                        {sess.mcp && (
                          <Button
                            size="sm"
                            aria-label="工具目录"
                            title="工具目录"
                            onClick={() => navigate("catalog", sess.session_id)}
                          >
                            <Library />
                            <span className="max-md:hidden">工具目录</span>
                          </Button>
                        )}
                        {sess.state !== "running" && (
                          <Button
                            size="sm"
                            variant="danger"
                            aria-label="释放会话"
                            title="释放会话"
                            disabled={busy !== null}
                            onClick={() =>
                              void run(sess.session_id, () => client.closeSession(sess.session_id))
                            }
                          >
                            <Trash2 />
                            <span className="max-md:hidden">释放</span>
                          </Button>
                        )}
                      </span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>
    </>
  );
}
