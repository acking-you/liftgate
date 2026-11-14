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
  recent_stderr: StderrLogEntry[];
}

export type SessionSlotState = "running" | "idle";

export interface SessionSummary {
  session_id: string;
  model_id: string;
  state: SessionSlotState;
  turn_count: number;
  message_count: number;
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
  thought_chars: number;
  output_chars: number;
  client_tool_calls: string[];
  native_web_searches: number;
  native_web_fetches: number;
  prompt_preview: string;
  output_preview: string;
  stop_reason: string | null;
  error_message: string | null;
  usage: AcpUsage | null;
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
}

export interface AvailableModel {
  modelId: string;
  name: string;
}

export interface GatewayTelemetrySnapshot {
  generated_at_unix_ms: number;
  gateway_version: string;
  acp_process: AcpProcessTelemetry;
  session_pool: SessionPoolTelemetry;
  metrics: GatewayMetricsSummary;
  active_tasks: TaskSnapshot[];
  recent_tasks: TaskSnapshot[];
  available_models: AvailableModel[];
}
