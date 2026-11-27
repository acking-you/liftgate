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

export interface SessionMcpSummary {
  harness_profile: string;
  orchestrator_mode?: string | null;
  is_passthrough: boolean;
  declared_tools: number;
  eager_tools: number;
  virtualized_hubs: number;
  wire_bytes: number;
}

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
  mcp?: SessionMcpSummary | null;
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
  /** Harness profile (`codex`, `claude-code`, …); absent until attributed or on older gateways. */
  harness_profile?: string | null;
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
  zero_rtt_mcp_ops?: number;
  spilled_tool_results?: number;
  prompt_preview: string;
  output_preview: string;
  stop_reason: string | null;
  error_message: string | null;
  usage: AcpUsage | null;
  cache_hit_rate_bps?: number | null;
  /** Cost in USD at the model's current price; absent while unknown or unpriced. */
  cost_usd?: number | null;
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

export interface ElasticIoTelemetry {
  passthrough_sessions: number;
  virtualized_sessions: number;
  mcp_forwarded_tool_calls: number;
  mcp_orchestrator_calls: number;
  mcp_tool_using_turns: number;
  mcp_patch_calls: number;
  mcp_multi_file_patch_calls: number;
  mcp_patch_files_total: number;
  mcp_zero_rtt_searches: number;
  mcp_zero_rtt_spill_reads: number;
  mcp_zero_rtt_preflight_heals: number;
  mcp_tool_results_total: number;
  mcp_tool_results_spilled: number;
  context_spills_created: number;
  context_spills_bytes: number;
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
  elastic_io?: ElasticIoTelemetry;
  /** Present once the gateway records per-call tool I/O (issue #21). */
  tool_call_telemetry?: ToolCallTelemetrySummary;
  /** Present once the gateway supports opt-in full-context capture (issue #21). */
  context_capture?: ContextCaptureSummary;
  /** Recorded usage and cost (absent on gateways that predate pricing). */
  usage?: UsageSummary;
  active_tasks: TaskSnapshot[];
  recent_tasks: TaskSnapshot[];
  available_models: AvailableModel[];
}

/* ===========================================================================
   Per-call tool I/O, session tool catalog & opt-in full-context capture.
   Mirrors `liftgate_types::telemetry`. A gateway that predates these features
   omits `tool_call_telemetry` / `context_capture` on the snapshot; views check
   for those before calling the endpoints below.
   =========================================================================== */

/** How a tool call travelled through the gateway. */
export type ToolCallRoute =
  /** Suspended or XML round-trip executed by the harness (`exec`, `Bash`, `apply_patch`, …). */
  | "client_roundtrip"
  /** `mcp_lg_ns__*` namespace hub unwrapped to the original client tool. */
  | "namespace_hub"
  /** `mcp_lg_call_client_tool` generic dispatcher unwrapped to a client tool. */
  | "generic_dispatcher"
  /** Gateway-local `mcp_lg_search_client_tools` answered without a round-trip. */
  | "zero_rtt_search"
  /** Gateway-local `mcp_lg_read_context_spill` paging. */
  | "zero_rtt_spill_read"
  /** Gateway-local preflight rejection (missing fields, V4A syntax, unknown or disabled tool). */
  | "zero_rtt_preflight"
  /** Gateway-hosted tool such as `mcp_lg_gateway_web_fetch`. */
  | "gateway_managed"
  | "unknown";

export type ToolCallStatus =
  | "pending"
  | "ok"
  | "error"
  | "preflight_rejected"
  /** The result never came back (session evicted, turn cancelled, result never sent). */
  | "abandoned"
  | "unknown";

export type ToolCallErrorKind =
  | "tool_error"
  | "script_exception"
  | "preflight_rejected"
  | "disabled_tool"
  | "unknown";

/** Row returned by `GET /api/telemetry/tool-calls`. */
export interface ToolCallRecord {
  call_id: string;
  /** Insertion order; records with `seq < summary.oldest_seq` were evicted. */
  seq: number;
  /** Buffer revision at which this record last changed (for `?since=` polling). */
  revision: number;
  task_id: string;
  session_id: string | null;
  /** Session turn (one per HTTP request against the session). */
  turn_index: number;
  /** 0-based order of the call inside its turn. */
  step_index: number;
  harness_profile: string;
  orchestrator_mode?: string | null;
  model_id: string;
  route: ToolCallRoute;
  /** Tool name the model invoked (`mcp_lg_*` for MCP calls, the client name for XML calls). */
  wire_tool_name: string;
  /** Client tool that executed the call (null for gateway-local calls). */
  client_tool_name?: string | null;
  /** True for CodeMode orchestrators (`exec` / `execute` / `eval` / `REPL`). */
  is_orchestrator: boolean;
  code_language?: string | null;
  status: ToolCallStatus;
  error_kind?: ToolCallErrorKind | null;
  v4a_auto_repaired?: boolean;
  output_sanitized?: boolean;
  output_spilled?: boolean;
  started_at_unix_ms: number;
  duration_ms: number | null;
  input_bytes: number;
  output_bytes: number;
  input_tokens_est: number;
  output_tokens_est: number;
  input_preview: string;
  output_preview: string;
}

export type ToolOutputBlock =
  | { type: "text"; bytes: number; text: string }
  | {
      type: "image";
      mime_type: string;
      base64_bytes: number;
      width?: number | null;
      height?: number | null;
    };

/** Full record returned by `GET /api/telemetry/tool-calls/{call_id}`. */
export interface ToolCallDetail extends ToolCallRecord {
  /** Arguments exactly as the model emitted them on the wire tool. */
  wire_arguments: string;
  /** Arguments forwarded to the client after unwrapping or repair (null when unchanged). */
  client_arguments?: string | null;
  output_blocks: ToolOutputBlock[];
  /** True when the per-field byte cap clipped the stored input or output. */
  input_truncated?: boolean;
  output_truncated?: boolean;
}

/** Ring-buffer status carried on the snapshot as `tool_call_telemetry`. */
export interface ToolCallTelemetrySummary {
  /** Identifies the gateway's buffer; revisions and `seq` start over when it changes. */
  store_id?: string;
  revision: number;
  oldest_seq: number;
  max_records: number;
  max_field_bytes: number;
  max_total_bytes: number;
  stored_records: number;
  stored_bytes: number;
  total_recorded: number;
  total_evicted: number;
}

export interface ToolCallsResponse {
  summary: ToolCallTelemetrySummary;
  /** Records changed after `since` (all retained records without it), oldest first. */
  records: ToolCallRecord[];
}

/** Where a declared client tool ended up after 3-tier virtualization. */
export type CatalogToolTier =
  | "orchestrator"
  | "direct"
  | "folded_into_orchestrator"
  | "namespace_hub_action"
  | "slack_backfill"
  | "search_index_only"
  | "unknown";

export interface CatalogToolEntry {
  client_name: string;
  tier: CatalogToolTier;
  /** Wire tool that serves this client tool (hub name for folded actions). */
  wire_name?: string | null;
  client_kind: string;
  client_description?: string | null;
  /** JSON Schema or custom grammar exactly as the client declared it. */
  client_schema?: string | null;
  client_bytes: number;
  /** Description / schema mounted on `lg` for this tool (null when it is not mounted directly). */
  wire_description?: string | null;
  wire_schema?: string | null;
  /** Wire bytes attributable to this tool (hub bytes are split across its actions). */
  wire_bytes: number;
  /** TypeScript declaration indexed for 0-RTT search (Codex `code_mode_host`). */
  ts_signature?: string | null;
}

export interface CatalogWireTool {
  wire_name: string;
  role: "orchestrator" | "direct" | "namespace_hub" | "slack_backfill" | "gateway" | "unknown";
  description?: string | null;
  input_schema: string;
  bytes: number;
  /** Client tools folded behind this wire tool. */
  folded_actions: string[];
}

export type ContextSegmentKey =
  | "wire_tools"
  | "bridge_rules"
  | "orchestrator_contract"
  | "client_tools_update"
  | "developer_instructions"
  | "user_text"
  | "assistant_text"
  | "reasoning"
  | "tool_calls"
  | "tool_results"
  | "images";

export interface ContextSegment {
  key: ContextSegmentKey;
  bytes: number;
  tokens_est: number;
}

/** `GET /api/telemetry/sessions/{session_id}/tool-catalog`. */
export interface SessionToolCatalog {
  session_id: string;
  harness_profile: string;
  orchestrator_mode?: string | null;
  is_passthrough: boolean;
  model_id: string;
  /** When the context composition was sampled (0 before the first completed turn). */
  captured_at_unix_ms: number;
  wire_budget_bytes: number;
  eager_tool_budget: number;
  /** Bytes of the tool list as the client declared it (never sent upstream verbatim). */
  client_tools_bytes: number;
  /** Context composition of the session's latest completed turn. */
  segments: ContextSegment[];
  wire_tools: CatalogWireTool[];
  tools: CatalogToolEntry[];
  /** `<api_gateway_bridge_rules>` sent in `_meta.antigravity.systemPrompt.append`. */
  bridge_rules_prompt?: string | null;
  /** The CodeMode orchestrator contract embedded in the bridge rules. */
  orchestrator_contract_prompt?: string | null;
  search_index_entries: number;
}

/** Full-context capture status carried on the snapshot as `context_capture`. */
export interface ContextCaptureSummary {
  revision: number;
  enabled: boolean;
  max_records: number;
  max_total_bytes: number;
  stored_records: number;
  stored_bytes: number;
  /** Turns skipped because earlier captures were still being written; reset by clearing. */
  skipped_records?: number;
  capture_dir?: string | null;
}

export interface ContextCaptureRecord {
  task_id: string;
  recorded_at_unix_ms: number;
  session_id: string | null;
  harness_profile?: string | null;
  resolved_model: string;
  session_turn_index: number;
  cache_hit_rate_bps?: number | null;
  total_prompt_tokens: number;
  output_tokens: number;
  message_count: number;
  delta_message_count: number;
  payload_bytes: number;
  prompt_preview: string;
}

export interface ContextCapturesResponse {
  summary: ContextCaptureSummary;
  records: ContextCaptureRecord[];
}

export interface FullContextPayload extends LowCacheFullRequestPayload {
  /** Reasoning the model produced during this turn. */
  assistant_reasoning?: string | null;
  /** Complete text output of this turn. */
  assistant_output?: string | null;
  /** Reasoning beyond the per-field capture cap was dropped. */
  assistant_reasoning_truncated?: boolean;
  /** Output beyond the per-field capture cap was dropped. */
  assistant_output_truncated?: boolean;
}

/** `GET /api/telemetry/context-captures/{task_id}`. */
export interface ContextCaptureDetailResponse {
  record: ContextCaptureRecord;
  payload: FullContextPayload;
}

/** Body of `POST /api/telemetry/context-capture`. */
export interface ContextCaptureConfigUpdate {
  enabled?: boolean;
  max_records?: number;
  /** Disk quota in MiB (16..=65536). */
  max_total_mib?: number;
}

/* ---------- Usage & pricing (`/api/telemetry/usage`, `/api/telemetry/pricing`) ---------- */

/** Tokens split the way providers bill them; `thought` is a share of `output`. */
export interface UsageTokens {
  input: number;
  cached_input: number;
  cache_write: number;
  output: number;
  thought: number;
}

export interface UsageSummary {
  revision: number;
  /** Start of the covered range: when recording started (or was reset), or the requested `from`. */
  since_unix_ms: number;
  /** End of the requested range; absent for the all-time totals. */
  until_unix_ms?: number;
  /** Where bucketed history starts; earlier usage only counts toward the all-time totals. */
  history_from_unix_ms?: number;
  calls: number;
  tokens: UsageTokens;
  /** Cost of priced usage. */
  cost_usd: number;
  /** What cache hits saved versus uncached input, priced per price period. */
  cache_savings_usd?: number;
  /** Calls on models without a price, left out of `cost_usd`. */
  unpriced_calls: number;
}

/** Usage within one interval of `GET /api/telemetry/usage/series`. */
export interface UsageInterval {
  start_unix_ms: number;
  end_unix_ms: number;
  calls: number;
  auxiliary_calls: number;
  tokens: UsageTokens;
  /** Cost of priced usage in the interval. */
  cost_usd: number;
  cache_savings_usd: number;
  /** Calls on models without a price, left out of `cost_usd`. */
  unpriced_calls: number;
}

/** One interval per consecutive pair of requested bounds, e.g. the viewer's local midnights. */
export interface UsageSeriesResponse {
  revision: number;
  /** Width of the usage buckets the intervals are cut from. */
  bucket_ms: number;
  /** Where bucketed history starts; intervals before it read as zero. */
  history_from_unix_ms: number;
  intervals: UsageInterval[];
}

export interface ModelUsage {
  model_id: string;
  pricing_key: string;
  calls: number;
  auxiliary_calls: number;
  tokens: UsageTokens;
  /** Null when the model has neither a default nor a custom price. */
  cost_usd: number | null;
  first_used_at_unix_ms: number;
  last_used_at_unix_ms: number;
}

export interface UsageResponse {
  summary: UsageSummary;
  /** Costliest first. */
  models: ModelUsage[];
}

/** USD per 1M tokens. */
export interface ModelPrice {
  input: number;
  cached_input: number;
  cache_write: number;
  output: number;
}

export interface ScheduledPrice {
  effective_from_unix_ms: number;
  price: ModelPrice;
}

export interface ModelPricingEntry {
  key: string;
  label: string | null;
  model_ids: string[];
  default_price: ModelPrice | null;
  upcoming_defaults: ScheduledPrice[];
  custom_price: ModelPrice | null;
  source_url: string | null;
}

export interface PricingResponse {
  revision: number;
  currency: string;
  defaults_as_of: string;
  entries: ModelPricingEntry[];
}
