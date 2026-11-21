export interface StderrLogEntry {
  timestamp_unix_ms: number;
  line: string;
}

export interface AcpProcessTelemetry {
  connected: boolean;
  pid: number | null;
  binary_path: string;
  session_cwd: string;
  os: string;
  arch: string;
  supervision_strategy: string;
  protocol_version: number;
  agent_name: string | null;
  agent_version: string | null;
  started_at_unix_ms: number;
  uptime_secs: number;
  default_model: string;
  acp_mode: string;
  rpc_requests_sent: number;
  rpc_responses_received: number;
  rpc_notifications_received: number;
  rpc_pending_in_flight: number;
  active_prompt_streams: number;
  agy_proxy?: string | null;
  auto_update_enabled?: boolean;
  latest_known_version?: string | null;
  last_update_check_unix_ms?: number | null;
  last_auto_update_unix_ms?: number | null;
  last_auto_updated_from?: string | null;
  auto_update_note?: string | null;
  recent_stderr: StderrLogEntry[];
}

export type SessionSlotState = "running" | "idle";

export type PromptCacheWarmth = "active" | "hot" | "warm" | "cold" | "unknown";

export interface SessionSummary {
  session_id: string;
  model_id: string;
  state: SessionSlotState;
  cache_warmth?: PromptCacheWarmth;
  turn_count: number;
  message_count: number;
  last_context_tokens?: number;
  total_prompt_tokens?: number;
  total_cached_read_tokens?: number;
  cache_hit_rate_bps?: number | null;
  created_at_unix_ms: number;
  last_active_unix_ms: number;
  last_response_id: string | null;
}

export interface SessionPoolTelemetry {
  active_leased_sessions: number;
  idle_cached_sessions: number;
  max_idle_sessions: number;
  stored_responses: number;
  max_stored_responses: number;
  total_sessions_created: number;
  total_sessions_reused: number;
  total_sessions_closed_or_evicted: number;
  sessions: SessionSummary[];
}

export type TaskProtocol = "openai_responses" | "anthropic_messages";

export type TaskPhase =
  | "initializing_session"
  | "thinking"
  | "searching_web"
  | "fetching_web"
  | "streaming_text"
  | "calling_client_tool"
  | "completed"
  | "failed"
  | "cancelled";

export interface AcpUsage {
  inputTokens: number;
  outputTokens: number;
  cachedReadTokens: number;
  cachedWriteTokens: number;
  thoughtTokens: number;
  totalTokens: number;
}

export interface TaskSnapshot {
  task_id: string;
  protocol: TaskProtocol;
  stream: boolean;
  requested_model: string;
  resolved_model: string;
  session_id: string | null;
  session_reused: boolean;
  phase: TaskPhase;
  started_at_unix_ms: number;
  finished_at_unix_ms: number | null;
  elapsed_ms: number;
  ttft_ms?: number | null;
  thought_chars: number;
  output_chars: number;
  client_tool_calls: string[];
  native_web_searches: number;
  native_web_fetches: number;
  vision_offloaded_images?: number;
  prompt_preview: string;
  output_preview: string;
  stop_reason: string | null;
  error_message: string | null;
  usage: AcpUsage | null;
  cache_hit_rate_bps?: number | null;
}

export type LowCacheHitReason =
  | "warm_cache_miss"
  | "warm_partial_hit"
  | "cold_session_start"
  | "unknown";

export interface LowCacheHitRecord {
  task_id: string;
  recorded_at_unix_ms: number;
  protocol: TaskProtocol;
  requested_model: string;
  resolved_model: string;
  session_id: string | null;
  session_reused: boolean;
  reason: LowCacheHitReason;
  cache_hit_rate_bps: number;
  input_uncached_tokens: number;
  cached_read_tokens: number;
  cached_write_tokens: number;
  total_prompt_tokens: number;
  output_tokens: number;
  elapsed_ms: number;
  ttft_ms?: number | null;
  prompt_preview: string;
}

export interface CacheTelemetrySummary {
  revision: number;
  max_low_cache_records: number;
  low_cache_threshold_bps: number;
  persisted_path: string | null;
  total_prompt_tokens: number;
  total_uncached_input_tokens: number;
  total_cached_read_tokens: number;
  total_cached_write_tokens: number;
  total_cache_hit_rate_bps: number | null;
  warm_prompt_tokens: number;
  warm_cached_read_tokens: number;
  warm_cache_hit_rate_bps: number | null;
  evaluated_turns: number;
  warm_evaluated_turns: number;
  total_low_cache_turns: number;
  stored_low_cache_count: number;
}

export interface LowCacheHitsResponse {
  summary: CacheTelemetrySummary;
  records: LowCacheHitRecord[];
}

export interface GatewayMetricsSummary {
  total_requests: number;
  active_requests: number;
  completed_requests: number;
  failed_requests: number;
  cancelled_requests: number;
  openai_requests: number;
  anthropic_requests: number;
  total_input_tokens: number;
  total_output_tokens: number;
  total_thought_tokens: number;
  total_cached_tokens: number;
  total_cached_write_tokens?: number;
  cache_hit_rate_bps?: number | null;
}

export interface AvailableModel {
  modelId: string;
  name: string;
}

export interface GatewayTelemetrySnapshot {
  generated_at_unix_ms: number;
  gateway_version: string;
  vision_offload_enabled?: boolean;
  acp_process: AcpProcessTelemetry;
  session_pool: SessionPoolTelemetry;
  metrics: GatewayMetricsSummary;
  cache_telemetry?: CacheTelemetrySummary;
  active_tasks: TaskSnapshot[];
  recent_tasks: TaskSnapshot[];
  available_models: AvailableModel[];
}
