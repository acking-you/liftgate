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
    #[serde(default)]
    pub agy_proxy: Option<String>,
    #[serde(default)]
    pub auto_update_enabled: bool,
    #[serde(default)]
    pub latest_known_version: Option<String>,
    #[serde(default)]
    pub last_update_check_unix_ms: Option<u64>,
    #[serde(default)]
    pub last_auto_update_unix_ms: Option<u64>,
    #[serde(default)]
    pub last_auto_updated_from: Option<String>,
    #[serde(default)]
    pub auto_update_note: Option<String>,
}

/// Current lifecycle state of a tracked ACP session worker (`--process-per-session`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSlotState {
    Running,
    Idle,
}

/// Empirical upstream prompt-cache warmth tier derived from elapsed idle time since a session's
/// last completed turn (`<60s`: ~98.6% hit, `60s..300s`: cooling ~65-84% hit, `>=300s`: cold).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheWarmth {
    /// Session currently has an in-flight turn leased.
    Active,
    /// Idle `< 60s` (~98.6% upstream KV cache retention).
    Hot,
    /// Idle `60s .. 300s` (cooling; ~65%–84% upstream KV cache retention).
    Warm,
    /// Idle `>= 300s` (upstream KV cache likely expired; stdio session still avoids cold replay).
    #[default]
    Cold,
    #[serde(other)]
    Unknown,
}

impl PromptCacheWarmth {
    pub const HOT_IDLE_THRESHOLD_MS: u64 = 60_000;
    pub const WARM_IDLE_THRESHOLD_MS: u64 = 300_000;

    /// Classifies upstream KV-cache warmth from session slot state and timestamps.
    #[must_use]
    pub const fn classify(
        state: SessionSlotState,
        last_active_unix_ms: u64,
        now_unix_ms: u64,
    ) -> Self {
        match state {
            SessionSlotState::Running => Self::Active,
            SessionSlotState::Idle => {
                let idle_ms = now_unix_ms.saturating_sub(last_active_unix_ms);
                if idle_ms < Self::HOT_IDLE_THRESHOLD_MS {
                    Self::Hot
                } else if idle_ms < Self::WARM_IDLE_THRESHOLD_MS {
                    Self::Warm
                } else {
                    Self::Cold
                }
            }
        }
    }
}

/// Metadata summary for a single active or cached ACP session worker.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub model_id: String,
    pub state: SessionSlotState,
    #[serde(default)]
    pub cache_warmth: PromptCacheWarmth,
    pub turn_count: u64,
    pub message_count: usize,
    #[serde(default)]
    pub last_context_tokens: u64,
    #[serde(default)]
    pub total_prompt_tokens: u64,
    #[serde(default)]
    pub total_cached_read_tokens: u64,
    #[serde(default)]
    pub cache_hit_rate_bps: Option<u16>,
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
    #[serde(default)]
    pub ttft_ms: Option<u64>,
    pub thought_chars: usize,
    pub output_chars: usize,
    pub client_tool_calls: Vec<String>,
    pub native_web_searches: usize,
    #[serde(default)]
    pub native_web_fetches: usize,
    #[serde(default)]
    pub vision_offloaded_images: usize,
    pub prompt_preview: String,
    pub output_preview: String,
    pub stop_reason: Option<String>,
    pub error_message: Option<String>,
    pub usage: Option<AcpUsage>,
    #[serde(default)]
    pub cache_hit_rate_bps: Option<u16>,
}

/// Diagnostic classification for why a completed turn experienced a low prompt cache hit rate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LowCacheHitReason {
    /// Warm session was reused, yet upstream reported `0` cached read tokens (KV cache expired or busted).
    WarmCacheMiss,
    /// Warm session was reused, with partial prefix cache hit still below the configured threshold.
    WarmPartialHit,
    /// Turn created a brand-new ACP session (`!session_reused`, cold start or prefix hash miss).
    #[default]
    ColdSessionStart,
    #[serde(other)]
    Unknown,
}

impl LowCacheHitReason {
    #[must_use]
    pub const fn classify(session_reused: bool, cached_read_tokens: u64) -> Self {
        if !session_reused {
            Self::ColdSessionStart
        } else if cached_read_tokens == 0 {
            Self::WarmCacheMiss
        } else {
            Self::WarmPartialHit
        }
    }
}

/// Persisted diagnostic record for a turn whose prompt cache hit rate fell below threshold.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct LowCacheHitRecord {
    pub task_id: String,
    pub recorded_at_unix_ms: u64,
    pub protocol: TaskProtocol,
    pub requested_model: String,
    pub resolved_model: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub session_reused: bool,
    #[serde(default)]
    pub reason: LowCacheHitReason,
    pub cache_hit_rate_bps: u16,
    pub input_uncached_tokens: u64,
    pub cached_read_tokens: u64,
    #[serde(default)]
    pub cached_write_tokens: u64,
    pub total_prompt_tokens: u64,
    pub output_tokens: u64,
    pub elapsed_ms: u64,
    #[serde(default)]
    pub ttft_ms: Option<u64>,
    #[serde(default)]
    pub prompt_preview: String,
}

const fn default_max_low_cache_records_usize() -> usize {
    crate::config::DEFAULT_MAX_LOW_CACHE_RECORDS
}

const fn default_low_cache_threshold_bps_u16() -> u16 {
    crate::config::DEFAULT_LOW_CACHE_THRESHOLD_BPS
}

/// Cumulative prompt-cache hit accounting and persisted low-cache-hit store summary.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CacheTelemetrySummary {
    #[serde(default)]
    pub revision: u64,
    #[serde(default = "default_max_low_cache_records_usize")]
    pub max_low_cache_records: usize,
    #[serde(default = "default_low_cache_threshold_bps_u16")]
    pub low_cache_threshold_bps: u16,
    #[serde(default)]
    pub persisted_path: Option<String>,
    #[serde(default)]
    pub total_prompt_tokens: u64,
    #[serde(default)]
    pub total_uncached_input_tokens: u64,
    #[serde(default)]
    pub total_cached_read_tokens: u64,
    #[serde(default)]
    pub total_cached_write_tokens: u64,
    #[serde(default)]
    pub total_cache_hit_rate_bps: Option<u16>,
    #[serde(default)]
    pub warm_prompt_tokens: u64,
    #[serde(default)]
    pub warm_cached_read_tokens: u64,
    #[serde(default)]
    pub warm_cache_hit_rate_bps: Option<u16>,
    #[serde(default)]
    pub evaluated_turns: u64,
    #[serde(default)]
    pub warm_evaluated_turns: u64,
    #[serde(default)]
    pub total_low_cache_turns: u64,
    #[serde(default)]
    pub stored_low_cache_count: usize,
}

impl Default for CacheTelemetrySummary {
    fn default() -> Self {
        Self {
            revision: 0,
            max_low_cache_records: crate::config::DEFAULT_MAX_LOW_CACHE_RECORDS,
            low_cache_threshold_bps: crate::config::DEFAULT_LOW_CACHE_THRESHOLD_BPS,
            persisted_path: None,
            total_prompt_tokens: 0,
            total_uncached_input_tokens: 0,
            total_cached_read_tokens: 0,
            total_cached_write_tokens: 0,
            total_cache_hit_rate_bps: None,
            warm_prompt_tokens: 0,
            warm_cached_read_tokens: 0,
            warm_cache_hit_rate_bps: None,
            evaluated_turns: 0,
            warm_evaluated_turns: 0,
            total_low_cache_turns: 0,
            stored_low_cache_count: 0,
        }
    }
}

/// Response payload served by `GET /api/telemetry/low-cache-hits`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct LowCacheHitsResponse {
    pub summary: CacheTelemetrySummary,
    pub records: Vec<LowCacheHitRecord>,
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
    #[serde(default)]
    pub total_cached_write_tokens: u64,
    #[serde(default)]
    pub cache_hit_rate_bps: Option<u16>,
}

const fn default_true() -> bool {
    true
}

/// Top-level observability payload served over `GET /api/telemetry/snapshot` and SSE.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GatewayTelemetrySnapshot {
    pub generated_at_unix_ms: u64,
    pub gateway_version: Cow<'static, str>,
    #[serde(default = "default_true")]
    pub vision_offload_enabled: bool,
    pub acp_process: AcpProcessTelemetry,
    pub session_pool: SessionPoolTelemetry,
    pub metrics: GatewayMetricsSummary,
    #[serde(default)]
    pub cache_telemetry: CacheTelemetrySummary,
    pub active_tasks: Vec<TaskSnapshot>,
    pub recent_tasks: Vec<TaskSnapshot>,
    pub available_models: Vec<AvailableModel>,
}
