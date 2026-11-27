import { formatBytes, formatDurationMs, formatNumber } from "@/lib/format";
import type {
  LowCacheDiagnosticCause,
  LowCacheHitReason,
  LowCacheHitRecord,
  LowCacheRoutingStrategy,
} from "@/types/telemetry";

export const REASON_LABEL: Record<LowCacheHitReason, string> = {
  warm_cache_miss: "热会话掉缓存",
  warm_partial_hit: "热会话部分命中",
  cold_session_start: "冷启动",
  unknown: "未分类",
};

export const REASON_TONE: Record<LowCacheHitReason, "err" | "warn" | "neutral"> = {
  warm_cache_miss: "err",
  warm_partial_hit: "warn",
  cold_session_start: "neutral",
  unknown: "neutral",
};

export function causeLabel(cause: LowCacheDiagnosticCause | undefined): string | null {
  switch (cause) {
    case "warm_upstream_implicit_miss":
      return "上游隐式缓存未命中";
    case "warm_idle_ttl_expired":
      return "空闲超过 5 分钟";
    case "warm_model_switched":
      return "中途切换模型";
    case "warm_large_delta_dilution":
      return "大增量稀释";
    case "mid_conversation_cold_rebuild":
      return "中途冷重建";
    case "first_turn_cold_start":
      return "首轮冷启动";
    default:
      return null;
  }
}

export function routingLabel(strategy: LowCacheRoutingStrategy | undefined): string {
  switch (strategy) {
    case "previous_response_id":
      return "previous_response_id 续接";
    case "prefix_hash":
      return "前缀哈希命中";
    case "mcp_suspended_continuation":
      return "MCP 挂起续接";
    case "compaction_dialogue_hash":
      return "压缩对话哈希命中";
    case "cold_new_session":
      return "首轮新建会话";
    case "cold_mid_conversation_rebuild":
      return "中途冷重建重放";
    default:
      return "—";
  }
}

/** Plain-language account of why this turn missed the prompt cache. */
export function explainLowCache(record: LowCacheHitRecord): string {
  const m = record.request_metrics;
  const idle = m?.session_idle_ms != null ? `，检出前空闲 ${formatDurationMs(m.session_idle_ms)}` : "";
  switch (record.diagnostic_cause) {
    case "warm_upstream_implicit_miss": {
      const prevCtx = m?.previous_session_context_tokens ?? 0;
      const read =
        record.cached_read_tokens > 0 && prevCtx > 0
          ? `只命中了共享前缀（缓存读 ${formatNumber(record.cached_read_tokens)} / 上轮上下文 ${formatNumber(prevCtx)}）`
          : "缓存读为 0";
      const sub100k =
        prevCtx > 0 && prevCtx < 100_000
          ? `历史前缀约 ${formatNumber(prevCtx)} tokens，低于约 100k 的稳态水位，上游跨副本缓存粘性偏弱。`
          : "";
      return record.resolved_model.toLowerCase().includes("gemini")
        ? `本地已复用热会话${idle}，只发送了本轮增量（${m?.delta_message_count ?? 1} 条），但上游 Gemini 的隐式前缀缓存把请求调度到了没有上一轮快照的副本，${read}。${sub100k}`
        : `本地已复用热会话${idle}且未切换模型，但上游没有命中完整前缀缓存，${read}。`;
    }
    case "warm_idle_ttl_expired":
      return `复用了会话池里的会话${idle}，但空闲达到 5 分钟后上游缓存已自然过期。本地仍省去了冷会话重放。`;
    case "warm_model_switched":
      return `复用了现有会话，但模型从 ${m?.previous_model ?? "原模型"} 切到了 ${record.resolved_model}，不同模型之间无法共享上游缓存。`;
    case "warm_large_delta_dilution":
      return `命中了前缀缓存（读取 ${formatNumber(record.cached_read_tokens)} tokens），但本轮新增输入较大（发送 ${formatBytes(m?.outbound_acp_prompt_bytes ?? 0)}），拉低了单轮占比。`;
    case "mid_conversation_cold_rebuild":
      return `对话已有 ${m?.total_message_count ?? "多"} 条消息，却没有命中空闲会话（常见于上一轮工具短路销毁了会话、上下文压缩切换或会话池淘汰），因此新建会话并重放了历史。`;
    case "first_turn_cold_start":
      return "这是全新对话的第一轮，新建了独立的 ACP 子进程，还没有可读取的前缀缓存。";
    default:
      break;
  }
  switch (record.reason) {
    case "warm_cache_miss":
      return "复用了热会话，但上游返回的缓存读为 0。常见原因是跨副本调度未命中、空闲超过 5 分钟或中途切换了模型。";
    case "warm_partial_hit":
      return "复用了热会话并命中了部分前缀，但本轮新增的工具输出或上下文较大，整体命中率低于阈值。";
    case "cold_session_start":
      return "全新对话首轮，或前缀哈希没有匹配到空闲会话，新建了独立的 ACP 子进程。";
    case "unknown":
      return "这一轮的缓存命中率低于配置阈值，已写入本地审计队列。";
  }
}
