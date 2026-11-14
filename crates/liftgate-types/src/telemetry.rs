use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::acp::{AcpUsage, AvailableModel};

/// Single bounded stderr diagnostic line captured from `agy acp`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StderrLogEntry {
    pub timestamp_unix_ms: u64,
    pub line: String,
}

/// Real-time health and transport statistics of the supervised `agy acp` master process.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AcpProcessTelemetry {
    pub connected: bool,
    pub pid: Option<u32>,
    pub binary_path: String,
    pub session_cwd: String,
    pub os: Cow<'static, str>,
    pub arch: Cow<'static, str>,
    pub supervision_strategy: Cow<'static, str>,
    pub protocol_version: u32,
    pub agent_name: Option<String>,
    pub agent_version: Option<String>,
    pub started_at_unix_ms: u64,
    pub uptime_secs: u64,
    pub default_model: String,
    pub acp_mode: String,
    pub rpc_requests_sent: u64,
    pub rpc_responses_received: u64,
    pub rpc_notifications_received: u64,
    pub rpc_pending_in_flight: usize,
    pub active_prompt_streams: usize,
    pub recent_stderr: Vec<StderrLogEntry>,
}

/// Current lifecycle state of a tracked ACP session worker (`--process-per-session`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSlotState {
    Running,
    Idle,
}

/// Metadata summary for a single active or cached ACP session worker.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub model_id: String,
    pub state: SessionSlotState,
    pub turn_count: u64,
    pub message_count: usize,
    pub created_at_unix_ms: u64,
    pub last_active_unix_ms: u64,
    pub last_response_id: Option<String>,
}

/// Real-time utilization and reuse statistics of the bounded LRU `SessionPool`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionPoolTelemetry {
    pub active_leased_sessions: usize,
    pub idle_cached_sessions: usize,
    pub max_idle_sessions: usize,
    pub stored_responses: usize,
    pub max_stored_responses: usize,
    pub total_sessions_created: u64,
    pub total_sessions_reused: u64,
    pub total_sessions_closed_or_evicted: u64,
    pub sessions: Vec<SessionSummary>,
}

/// API surface through which a gateway task arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskProtocol {
    OpenaiResponses,
    AnthropicMessages,
}

/// Fine-grained execution phase of an active or finished gateway turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPhase {
    InitializingSession,
    Thinking,
    SearchingWeb,
    FetchingWeb,
    StreamingText,
    CallingClientTool,
    Completed,
    Failed,
    Cancelled,
}

/// Live or historical snapshot of a single API request turn.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TaskSnapshot {
    pub task_id: String,
    pub protocol: TaskProtocol,
    pub stream: bool,
    pub requested_model: String,
    pub resolved_model: String,
    pub session_id: Option<String>,
    pub session_reused: bool,
    pub phase: TaskPhase,
    pub started_at_unix_ms: u64,
    pub finished_at_unix_ms: Option<u64>,
    pub elapsed_ms: u64,
    pub thought_chars: usize,
    pub output_chars: usize,
    pub client_tool_calls: Vec<String>,
    pub native_web_searches: usize,
    #[serde(default)]
    pub native_web_fetches: usize,
    pub prompt_preview: String,
    pub output_preview: String,
    pub stop_reason: Option<String>,
    pub error_message: Option<String>,
    pub usage: Option<AcpUsage>,
}

/// Cumulative gateway throughput and token accounting counters.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct GatewayMetricsSummary {
    pub total_requests: u64,
    pub active_requests: usize,
    pub completed_requests: u64,
    pub failed_requests: u64,
    pub cancelled_requests: u64,
    pub openai_requests: u64,
    pub anthropic_requests: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_thought_tokens: u64,
    pub total_cached_tokens: u64,
}

/// Top-level observability payload served over `GET /api/telemetry/snapshot` and SSE.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GatewayTelemetrySnapshot {
    pub generated_at_unix_ms: u64,
    pub gateway_version: Cow<'static, str>,
    pub acp_process: AcpProcessTelemetry,
    pub session_pool: SessionPoolTelemetry,
    pub metrics: GatewayMetricsSummary,
    pub active_tasks: Vec<TaskSnapshot>,
    pub recent_tasks: Vec<TaskSnapshot>,
    pub available_models: Vec<AvailableModel>,
}
