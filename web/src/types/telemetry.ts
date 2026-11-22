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

export type SessionSlotState = "running" | "suspended" | "idle";

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
  suspended_mcp_sessions?: number;
  idle_cached_sessions: number;
  max_idle_sessions: number;
  stored_responses: number;
  max_stored_responses: number;
  total_sessions_created: number;
  total_sessions_reused: number;
  mcp_continuations?: number;
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
  auxiliary_task?: string | null;
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

export type LowCacheDiagnosticCause =
  | "first_turn_cold_start"
  | "mid_conversation_cold_rebuild"
  | "warm_upstream_implicit_miss"
  | "warm_idle_ttl_expired"
  | "warm_model_switched"
  | "warm_large_delta_dilution"
  | "unknown";

export type LowCacheRoutingStrategy =
  | "previous_response_id"
  | "prefix_hash"
  | "mcp_suspended_continuation"
  | "compaction_dialogue_hash"
  | "cold_new_session"
  | "cold_mid_conversation_rebuild"
  | "unknown";

export interface LowCacheRequestMetrics {
  routing_strategy: LowCacheRoutingStrategy;
  session_turn_index: number;
  session_idle_ms?: number | null;
  previous_session_context_tokens: number;
  model_switched: boolean;
  previous_model?: string | null;
  previous_response_id?: string | null;
  prefix_hash_hex?: string | null;
  base_state_hash_hex: string;
  tools_updated_on_warm_turn: boolean;
  stream: boolean;
  reasoning_effort?: string | null;
  total_message_count: number;
  conversational_message_count: number;
  user_message_count: number;
  assistant_message_count: number;
  delta_message_count: number;
  total_conversation_bytes: number;
  delta_messages_bytes: number;
  developer_instructions_blocks: number;
  developer_instructions_bytes: number;
  user_text_bytes: number;
  assistant_text_bytes: number;
  reasoning_part_count: number;
  reasoning_bytes: number;
  tool_call_part_count: number;
  tool_call_bytes: number;
  tool_result_part_count: number;
  tool_result_bytes: number;
  image_part_count: number;
  image_bytes: number;
  client_tool_count: number;
  client_tools_schema_bytes: number;
  acp_system_append_bytes: number;
  tools_delta_xml_bytes: number;
  outbound_acp_prompt_bytes: number;
  outbound_acp_text_blocks: number;
  outbound_acp_image_blocks: number;
  thought_tokens: number;
  thought_chars: number;
  output_chars: number;
  native_web_searches: number;
  native_web_fetches: number;
  vision_offloaded_images: number;
  emitted_client_tools: string[];
  stop_reason?: string | null;
}

export interface CapturedMessageSummary {
  index: number;
  role: string;
  in_delta: boolean;
  total_bytes: number;
  part_kinds: string[];
  excerpt: string;
}

export interface LowCacheRequestCapture {
  client_tool_names: string[];
  enable_native_web_search: boolean;
  force_native_web_search: boolean;
  enable_native_web_fetch: boolean;
  force_native_web_fetch: boolean;
  acp_system_append_excerpt?: string | null;
  tools_delta_xml_excerpt?: string | null;
  outbound_acp_prompt_excerpt: string;
  output_preview: string;
  message_digests: CapturedMessageSummary[];
  full_payload_saved: boolean;
  payload_file_path?: string | null;
}

export type CapturedAcpPromptBlock =
  | { type: "text"; bytes: number; text: string }
  | { type: "image"; mime_type: string; base64_bytes: number };

export type CapturedFullPart =
  | {
      kind: "text";
      is_developer_instructions: boolean;
      bytes: number;
      text: string;
    }
  | { kind: "reasoning"; bytes: number; text: string }
  | { kind: "image"; media_type: string; base64_bytes: number }
  | {
      kind: "tool_call";
      call_id: string;
      name: string;
      is_custom: boolean;
      bytes: number;
      arguments: string;
    }
  | {
      kind: "tool_result";
      call_id: string;
      is_error: boolean;
      bytes: number;
      output: string;
    };

export interface CapturedFullMessage {
  index: number;
  role: string;
  in_delta: boolean;
  total_bytes: number;
  parts: CapturedFullPart[];
}

export interface CapturedToolSpec {
  name: string;
  kind: string;
  schema_bytes: number;
  description?: string | null;
  schema_or_format?: string | null;
}

export interface LowCacheFullRequestPayload {
  task_id: string;
  recorded_at_unix_ms: number;
  acp_system_append?: string | null;
  tools_delta_xml?: string | null;
  outbound_acp_prompt_blocks: CapturedAcpPromptBlock[];
  messages: CapturedFullMessage[];
  client_tools: CapturedToolSpec[];
  assistant_output_preview: string;
  payload_truncated: boolean;
}

export interface LowCachePayloadDetailResponse {
  record: LowCacheHitRecord;
  full_payload?: LowCacheFullRequestPayload | null;
}

export interface LowCacheHitRecord {
  task_id: string;
  recorded_at_unix_ms: number;
  protocol: TaskProtocol;
  requested_model: string;
  resolved_model: string;
  session_id: string | null;
  session_reused: boolean;
  reason: LowCacheHitReason;
  diagnostic_cause?: LowCacheDiagnosticCause;
  cache_hit_rate_bps: number;
  input_uncached_tokens: number;
  cached_read_tokens: number;
  cached_write_tokens: number;
  total_prompt_tokens: number;
  output_tokens: number;
  elapsed_ms: number;
  ttft_ms?: number | null;
  prompt_preview: string;
  request_metrics?: LowCacheRequestMetrics | null;
  request_capture?: LowCacheRequestCapture | null;
}

export interface CacheTelemetrySummary {
  revision: number;
  max_low_cache_records: number;
  low_cache_threshold_bps: number;
  capture_request_details?: boolean;
  persisted_path: string | null;
  payload_dir?: string | null;
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
  auxiliary_turns?: number;
  auxiliary_prompt_tokens?: number;
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
  mcp_tool_bridge_enabled?: boolean;
  acp_process: AcpProcessTelemetry;
  session_pool: SessionPoolTelemetry;
  metrics: GatewayMetricsSummary;
  cache_telemetry?: CacheTelemetrySummary;
  active_tasks: TaskSnapshot[];
  recent_tasks: TaskSnapshot[];
  available_models: AvailableModel[];
}
