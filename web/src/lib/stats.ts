import type {
  CacheTelemetrySummary,
  ElasticIoTelemetry,
  GatewayMetricsSummary,
  SessionPoolTelemetry,
} from "@/types/telemetry";

export interface CacheStats {
  promptTokens: number;
  cachedRead: number;
  cachedWrite: number;
  uncachedInput: number;
  hitBps: number | null;
  warmHitBps: number | null;
  evaluatedTurns: number;
  warmTurns: number;
}

/** Persisted totals when the gateway has them, process-lifetime counters otherwise. */
export function cacheStats(
  summary: CacheTelemetrySummary | undefined,
  metrics: GatewayMetricsSummary | undefined,
): CacheStats {
  if (summary && summary.total_prompt_tokens > 0) {
    return {
      promptTokens: summary.total_prompt_tokens,
      cachedRead: summary.total_cached_read_tokens,
      cachedWrite: summary.total_cached_write_tokens,
      uncachedInput: summary.total_uncached_input_tokens,
      hitBps: summary.total_cache_hit_rate_bps,
      warmHitBps: summary.warm_cache_hit_rate_bps,
      evaluatedTurns: summary.evaluated_turns,
      warmTurns: summary.warm_evaluated_turns,
    };
  }
  const uncachedInput = metrics?.total_input_tokens ?? 0;
  const cachedRead = metrics?.total_cached_tokens ?? 0;
  const cachedWrite = metrics?.total_cached_write_tokens ?? 0;
  const promptTokens = uncachedInput + cachedRead + cachedWrite;
  return {
    promptTokens,
    cachedRead,
    cachedWrite,
    uncachedInput,
    hitBps:
      metrics?.cache_hit_rate_bps ??
      (promptTokens > 0
        ? Math.round((Math.min(cachedRead, promptTokens) * 10000) / promptTokens)
        : null),
    warmHitBps: null,
    evaluatedTurns: metrics?.completed_requests ?? 0,
    warmTurns: 0,
  };
}

/** Share of turns served by a session that already existed. */
export function sessionReuseRatio(pool: SessionPoolTelemetry | undefined): number | null {
  if (!pool) return null;
  const total = pool.total_sessions_created + pool.total_sessions_reused;
  return total > 0 ? pool.total_sessions_reused / total : null;
}

/** Wire budget the virtualizer plans against; mirrored from `liftgate-tool-virtualizer`. */
export const WIRE_BUDGET_BYTES = 34 * 1024;

export interface ElasticStats {
  forwarded: number;
  orchestrator: number;
  orchestrationRatio: number | null;
  callsPerToolTurn: number | null;
  patchCalls: number;
  multiFilePatches: number;
  patchFiles: number;
  zeroRttSearches: number;
  zeroRttSpillReads: number;
  zeroRttPreflight: number;
  zeroRttTotal: number;
  resultsTotal: number;
  resultsSpilled: number;
  inlineRatio: number | null;
  spillsCreated: number;
  spillsBytes: number;
  passthroughSessions: number;
  virtualizedSessions: number;
}

export function elasticStats(io: ElasticIoTelemetry): ElasticStats {
  const zeroRttTotal =
    io.mcp_zero_rtt_searches + io.mcp_zero_rtt_spill_reads + io.mcp_zero_rtt_preflight_heals;
  return {
    forwarded: io.mcp_forwarded_tool_calls,
    orchestrator: io.mcp_orchestrator_calls,
    orchestrationRatio:
      io.mcp_forwarded_tool_calls > 0 ? io.mcp_orchestrator_calls / io.mcp_forwarded_tool_calls : null,
    callsPerToolTurn:
      io.mcp_tool_using_turns > 0 ? io.mcp_forwarded_tool_calls / io.mcp_tool_using_turns : null,
    patchCalls: io.mcp_patch_calls,
    multiFilePatches: io.mcp_multi_file_patch_calls,
    patchFiles: io.mcp_patch_files_total,
    zeroRttSearches: io.mcp_zero_rtt_searches,
    zeroRttSpillReads: io.mcp_zero_rtt_spill_reads,
    zeroRttPreflight: io.mcp_zero_rtt_preflight_heals,
    zeroRttTotal,
    resultsTotal: io.mcp_tool_results_total,
    resultsSpilled: io.mcp_tool_results_spilled,
    inlineRatio:
      io.mcp_tool_results_total > 0
        ? Math.max(0, io.mcp_tool_results_total - io.mcp_tool_results_spilled) /
          io.mcp_tool_results_total
        : null,
    spillsCreated: io.context_spills_created,
    spillsBytes: io.context_spills_bytes,
    passthroughSessions: io.passthrough_sessions,
    virtualizedSessions: io.virtualized_sessions,
  };
}

export function modelFamily(modelId: string): "gemini" | "claude" | "other" {
  const lower = modelId.toLowerCase();
  if (lower.includes("gemini")) return "gemini";
  if (lower.includes("claude")) return "claude";
  return "other";
}
