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
  ProviderBadge,
  SwitchRow,
  Tag,
} from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { errorMessage } from "@/lib/api";
import { formatBytes, formatNumber, formatSmartTime, formatUptime, shortId } from "@/lib/format";
import { orchestratorModeLabel } from "@/lib/harness";
import { useI18n } from "@/lib/i18n";
import { WIRE_BUDGET_BYTES } from "@/lib/stats";
import type { PromptCacheWarmth, SessionSummary } from "@/types/telemetry";
import type { ViewProps } from "@/views/types";

const WARMTH_TONE: Record<PromptCacheWarmth, "ok" | "warn" | "neutral"> = {
  active: "ok",
  hot: "ok",
  warm: "warn",
  cold: "neutral",
  unknown: "neutral",
};

function SessionState({ session }: { session: SessionSummary }): React.JSX.Element {
  const { m } = useI18n();
  const t = m.infra.sessions;
  const warmthKey = session.cache_warmth ?? "unknown";
  const warmth = { label: t.warmth[warmthKey], tone: WARMTH_TONE[warmthKey] };
  if (session.state === "running") {
    return (
      <Pill tone="accent" icon={<LoaderCircle className="lg-spin" />}>
        {t.running}
      </Pill>
    );
  }
  return (
    <span className="inline-flex flex-wrap items-center gap-1.5">
      {session.state === "suspended" && (
        <Pill tone="accent" title={t.suspendedHint}>
          {t.suspended}
        </Pill>
      )}
      <span className="inline-flex items-center gap-1.5 whitespace-nowrap text-[12px] text-fg-2" title={t.warmthHint}>
        <Dot tone={warmth.tone} />
        {warmth.label}
      </span>
    </span>
  );
}

export function SessionsView({ navigate }: ViewProps): React.JSX.Element {
  const { client, snapshot, refresh } = useGateway();
  const { m } = useI18n();
  const t = m.infra.sessions;
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const acp = snapshot?.acp_process;
  const pool = snapshot?.session_pool;
  const fleetAutoUpdate = snapshot?.auto_update;
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
          {t.failed(error)}
        </Note>
      )}

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-[minmax(0,1.6fr)_minmax(0,1fr)]">
        <Card
          title={acp?.agent_name ?? "Antigravity ACP Server"}
          sub={acp ? t.isolation(acp.supervision_strategy) : undefined}
          actions={
            acp && (
              <>
                <Pill tone={acp.connected ? "ok" : "err"}>
                  {acp.connected ? t.connected(acp.pid) : t.disconnected}
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
                <Figure label={t.uptime} value={formatUptime(acp.uptime_secs)} sub={t.protocol(acp.protocol_version)} />
                <Figure
                  label={t.rpc}
                  value={`${formatNumber(acp.rpc_requests_sent)} / ${formatNumber(acp.rpc_responses_received)}`}
                  sub={t.inFlight(acp.rpc_pending_in_flight)}
                />
                <Figure
                  label={t.frames}
                  value={formatNumber(acp.rpc_notifications_received)}
                  sub={t.streams(acp.active_prompt_streams)}
                />
                <Figure
                  label={t.defaultModel}
                  value={<span className="lg-num text-[13px]">{acp.default_model}</span>}
                  sub={t.mode(acp.acp_mode)}
                />
              </div>
              <KV
                items={[
                  [t.binary, acp.binary_path],
                  [t.cwd, acp.session_cwd],
                  [t.proxy, acp.agy_proxy ? acp.agy_proxy : t.direct],
                  [
                    t.autoUpdate,
                    `${(acp.auto_update_enabled ?? true) ? t.on : t.off}${
                      acp.latest_known_version ? t.upstream(acp.latest_known_version) : ""
                    }${acp.auto_update_note ? ` · ${acp.auto_update_note}` : ""}`,
                  ],
                ]}
              />
            </div>
          ) : (
            <p className="lg-hint m-0">{t.waiting}</p>
          )}
        </Card>

        <Card title={t.switches}>
          <div className="flex flex-col gap-4">
            <SwitchRow
              title={t.mcp}
              hint={t.mcpHint}
              checked={mcpBridge}
              disabled={busy !== null}
              onChange={(next) => void run("mcp", () => client.setMcpToolBridge(next))}
            />
            <div className="border-t border-line" />
            <SwitchRow
              title={t.vision}
              hint={t.visionHint}
              checked={visionOffload}
              disabled={busy !== null}
              onChange={(next) => void run("vision", () => client.setVisionOffload(next))}
            />
          </div>
        </Card>
      </div>

      {fleetAutoUpdate && fleetAutoUpdate.targets.length > 0 && (
        <Card
          title={t.fleetAutoUpdate}
          sub={t.fleetAutoUpdateSub(fleetAutoUpdate.interval_mins, fleetAutoUpdate.idle_ready)}
        >
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-6">
            {fleetAutoUpdate.targets.map((item) => (
              <div
                key={item.target}
                className="flex flex-col gap-1 rounded-lg border border-line px-3 py-2"
              >
                <div className="flex items-center justify-between gap-2">
                  <span className="text-[13px] font-medium capitalize">
                    {item.target === "liftgate" ? "lg" : item.target}
                  </span>
                  <Pill tone={item.enabled ? "ok" : "neutral"}>
                    {item.enabled ? t.on : t.off}
                  </Pill>
                </div>
                <span className="lg-num text-[12px] text-ink-muted">
                  {item.installed_version ? `v${item.installed_version}` : "—"}
                </span>
              </div>
            ))}
          </div>
        </Card>
      )}

      <Card
        flush
        title={t.pool(sessions.length)}
        sub={t.poolSub(
          formatNumber(pool?.total_sessions_created ?? 0),
          formatNumber(pool?.total_sessions_reused ?? 0),
          formatNumber(pool?.total_sessions_closed_or_evicted ?? 0),
        )}
        actions={
          <>
            <Tag title={t.parkedHint}>{t.parked(parked, pool?.max_idle_sessions ?? 0)}</Tag>
            <Tag title={t.continuationsHint}>{t.continuations(formatNumber(pool?.mcp_continuations ?? 0))}</Tag>
            <Tag title={t.responsesHint}>
              {t.responses(pool?.stored_responses ?? 0, pool?.max_stored_responses ?? 0)}
            </Tag>
          </>
        }
      >
        {sessions.length === 0 ? (
          <EmptyState
            icon={<Layers />}
            title={t.empty}
            hint={t.emptyHint}
          />
        ) : (
          <div className="lg-table-wrap">
            <table className="lg-table">
              <thead>
                <tr>
                  <th>{t.col.session}</th>
                  <th className="hide-sm">{t.col.state}</th>
                  <th className="hide-sm">{t.col.model}</th>
                  <th>{t.col.virtualization}</th>
                  <th className="num">{t.col.hitRate}</th>
                  <th className="num hide-sm">{t.col.context}</th>
                  <th className="num hide-sm">{t.col.turns}</th>
                  <th className="num hide-sm">{t.col.lastActive}</th>
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
                          <CopyButton value={sess.session_id} title={t.copyId} />
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
                    <td className="lg-num hide-sm whitespace-nowrap text-[12px]">
                      <span className="inline-flex items-center gap-2">
                        <ProviderBadge id={sess.provider_id} compact className="text-fg-2" />
                        <span>{sess.model_id}</span>
                      </span>
                    </td>
                    <td>
                      {sess.mcp ? (
                        <div className="md:min-w-[190px]">
                          <div className="flex flex-wrap items-center gap-1.5 text-[12px]">
                            <HarnessBadge id={sess.mcp.harness_profile} className="font-medium text-fg" />
                            <span className="inline-flex gap-1.5 max-md:hidden">
                              <Tag>{orchestratorModeLabel(sess.mcp.orchestrator_mode)}</Tag>
                              <Tag>{sess.mcp.is_passthrough ? t.passthrough : `Hub × ${sess.mcp.virtualized_hubs}`}</Tag>
                            </span>
                          </div>
                          <div className="mt-1.5 flex items-center gap-2 max-md:hidden">
                            <Meter
                              ratio={sess.mcp.wire_bytes / WIRE_BUDGET_BYTES}
                              tone={sess.mcp.wire_bytes / WIRE_BUDGET_BYTES > 0.9 ? "warn" : undefined}
                              label={t.wireRatio}
                            />
                            <span className="lg-num flex-none text-[11.5px] text-fg-3">
                              {formatBytes(sess.mcp.wire_bytes)} · Eager {sess.mcp.eager_tools} / {sess.mcp.declared_tools}
                            </span>
                          </div>
                        </div>
                      ) : (
                        <span className="text-[12px] text-fg-4">{t.noTools}</span>
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
                            aria-label={t.catalog}
                            title={t.catalog}
                            onClick={() => navigate("catalog", sess.session_id)}
                          >
                            <Library />
                            <span className="max-md:hidden">{t.catalogShort}</span>
                          </Button>
                        )}
                        {sess.state !== "running" && (
                          <Button
                            size="sm"
                            variant="danger"
                            aria-label={t.release}
                            title={t.release}
                            disabled={busy !== null}
                            onClick={() =>
                              void run(sess.session_id, () => client.closeSession(sess.session_id))
                            }
                          >
                            <Trash2 />
                            <span className="max-md:hidden">{t.releaseShort}</span>
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
