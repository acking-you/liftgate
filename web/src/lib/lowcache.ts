import { formatBytes, formatDurationMs, formatNumber } from "@/lib/format";
import { msg } from "@/lib/i18n";
import type {
  LowCacheDiagnosticCause,
  LowCacheHitReason,
  LowCacheHitRecord,
  LowCacheRoutingStrategy,
} from "@/types/telemetry";

export function reasonLabel(reason: LowCacheHitReason): string {
  return msg().cache.reason[reason];
}

export const REASON_TONE: Record<LowCacheHitReason, "err" | "warn" | "neutral"> = {
  warm_cache_miss: "err",
  warm_partial_hit: "warn",
  cold_session_start: "neutral",
  unknown: "neutral",
};

export function causeLabel(cause: LowCacheDiagnosticCause | undefined): string | null {
  const labels = msg().cache.cause;
  switch (cause) {
    case "warm_upstream_implicit_miss":
    case "warm_idle_ttl_expired":
    case "warm_model_switched":
    case "warm_large_delta_dilution":
    case "mid_conversation_cold_rebuild":
    case "first_turn_cold_start":
      return labels[cause];
    default:
      return null;
  }
}

export function routingLabel(strategy: LowCacheRoutingStrategy | undefined): string {
  const labels = msg().cache.routing;
  switch (strategy) {
    case "previous_response_id":
    case "prefix_hash":
    case "mcp_suspended_continuation":
    case "compaction_dialogue_hash":
    case "cold_new_session":
    case "cold_mid_conversation_rebuild":
      return labels[strategy];
    default:
      return "—";
  }
}

/** Plain-language account of why this turn missed the prompt cache. */
export function explainLowCache(record: LowCacheHitRecord): string {
  const e = msg().cache.explain;
  const m = record.request_metrics;
  const idle = m?.session_idle_ms != null ? e.idle(formatDurationMs(m.session_idle_ms)) : "";
  switch (record.diagnostic_cause) {
    case "warm_upstream_implicit_miss": {
      const prevCtx = m?.previous_session_context_tokens ?? 0;
      const read =
        record.cached_read_tokens > 0 && prevCtx > 0
          ? e.sharedPrefix(formatNumber(record.cached_read_tokens), formatNumber(prevCtx))
          : e.zeroRead;
      const sub100k = prevCtx > 0 && prevCtx < 100_000 ? e.sub100k(formatNumber(prevCtx)) : "";
      return record.resolved_model.toLowerCase().includes("gemini")
        ? e.implicitGemini(idle, m?.delta_message_count ?? 1, read, sub100k)
        : e.implicitOther(idle, read);
    }
    case "warm_idle_ttl_expired":
      return e.idleExpired(idle);
    case "warm_model_switched":
      return e.modelSwitched(m?.previous_model ?? e.previousModel, record.resolved_model);
    case "warm_large_delta_dilution":
      return e.dilution(
        formatNumber(record.cached_read_tokens),
        formatBytes(m?.outbound_acp_prompt_bytes ?? 0),
      );
    case "mid_conversation_cold_rebuild":
      return e.coldRebuild(m?.total_message_count != null ? String(m.total_message_count) : e.many);
    case "first_turn_cold_start":
      return e.firstTurn;
    default:
      break;
  }
  switch (record.reason) {
    case "warm_cache_miss":
      return e.warmMiss;
    case "warm_partial_hit":
      return e.warmPartial;
    case "cold_session_start":
      return e.cold;
    case "unknown":
      return e.unknown;
  }
}
