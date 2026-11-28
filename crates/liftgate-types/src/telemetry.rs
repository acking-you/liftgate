use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::acp::{AcpUsage, AvailableModel};
use crate::canonical::{AuxiliaryTaskKind, ReasoningEffort};
use crate::config::AutoUpdateTarget;
use crate::pricing::UsageSummary;
use crate::provider::{
    OptimizationMode, OptimizationPolicy, ProviderDriverKind, ProviderFamily, ProviderId,
    ToolWireMode, UpstreamWireProtocol,
};

fn default_provider_id_string() -> String {
    ProviderId::AGY.to_owned()
}

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
    Suspended,
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
            SessionSlotState::Suspended | SessionSlotState::Idle => {
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

/// Per-session loopback MCP virtualization & Elastic I/O orchestration summary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionMcpSummary {
    /// Detected harness tool profile (`"codex"`, `"claude-code"`, `"opencode"`, `"omp"`, `"generic"`).
    #[serde(default)]
    pub harness_profile: String,
    /// Active orchestration mode (`"codex_exec"`, `"claude_bash_first"`, `"opencode_execute_full"`, `"omp_eval"`, `"direct"`).
    #[serde(default)]
    pub orchestrator_mode: String,
    /// `true` when all eligible caller tools fit in 1:1 Direct Eager passthrough (`0` namespace hubs).
    #[serde(default)]
    pub is_passthrough: bool,
    /// Total tools declared by the caller on the session's latest turn.
    #[serde(default)]
    pub declared_tools: usize,
    /// Total Eager MCP tools exposed on `lg` `tools/list` (`<= 40` budget).
    #[serde(default)]
    pub eager_tools: usize,
    /// Number of `mcp_lg_ns__*` namespace hubs mounted (`0` when `is_passthrough`).
    #[serde(default)]
    pub virtualized_hubs: usize,
    /// Estimated serialized wire schema bytes on `lg` `tools/list` (`<= 34 KiB` budget, `< 55 KiB` upstream cliff).
    #[serde(default)]
    pub wire_bytes: usize,
}

/// Metadata summary for a single active or cached ACP session worker.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    #[serde(default = "default_provider_id_string")]
    pub provider_id: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<SessionMcpSummary>,
    pub created_at_unix_ms: u64,
    pub last_active_unix_ms: u64,
    pub last_response_id: Option<String>,
}

/// Real-time utilization and reuse statistics of the bounded LRU `SessionPool`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionPoolTelemetry {
    pub active_leased_sessions: usize,
    #[serde(default)]
    pub suspended_mcp_sessions: usize,
    pub idle_cached_sessions: usize,
    pub max_idle_sessions: usize,
    pub stored_responses: usize,
    pub max_stored_responses: usize,
    pub total_sessions_created: u64,
    pub total_sessions_reused: u64,
    #[serde(default)]
    pub mcp_continuations: u64,
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
    #[serde(default = "default_provider_id_string")]
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optimization_mode: Option<OptimizationMode>,
    pub protocol: TaskProtocol,
    pub stream: bool,
    pub requested_model: String,
    pub resolved_model: String,
    pub session_id: Option<String>,
    pub session_reused: bool,
    /// Harness profile (`codex`, `claude-code`, `opencode`, `omp`, `generic`) attributed once the
    /// turn binds a session; `None` while initializing or on gateways that predate attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auxiliary_task: Option<AuxiliaryTaskKind>,
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
    #[serde(default)]
    pub zero_rtt_mcp_ops: usize,
    #[serde(default)]
    pub spilled_tool_results: usize,
    pub prompt_preview: String,
    pub output_preview: String,
    pub stop_reason: Option<String>,
    pub error_message: Option<String>,
    pub usage: Option<AcpUsage>,
    #[serde(default)]
    pub cache_hit_rate_bps: Option<u16>,
    /// Cost of `usage` in USD at the model's price, `None` while unknown or unpriced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
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

/// Fine-grained root-cause diagnosis refining [`LowCacheHitReason`] with session idle age,
/// model-switch tracking, and multi-turn conversation rebuild detection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LowCacheDiagnosticCause {
    /// First turn of a brand-new conversation (`!session_reused`, single conversational turn).
    FirstTurnColdStart,
    /// Multi-turn conversation (`conversational_message_count > 1`) that missed the warm session
    /// pool (e.g. prior session retired after post-tool short-circuit, compaction, or LRU eviction).
    MidConversationColdRebuild,
    /// Reused warm session within `< 5m` idle window on the same model, yet upstream returned
    /// `cached_read_tokens == 0` (typical of opportunistic implicit KV replica routing on Gemini).
    WarmUpstreamImplicitMiss,
    /// Reused warm session after `>= 5m` idle time; upstream KV cache TTL naturally expired.
    WarmIdleTtlExpired,
    /// Reused warm session after switching models via `session/set_model`.
    WarmModelSwitched,
    /// Reused warm session with partial prefix cache hit, diluted below threshold by large new delta.
    WarmLargeDeltaDilution,
    #[default]
    #[serde(other)]
    Unknown,
}

impl LowCacheDiagnosticCause {
    #[must_use]
    pub const fn classify(
        session_reused: bool,
        cached_read_tokens: u64,
        previous_session_context_tokens: u64,
        conversational_message_count: usize,
        session_idle_ms: Option<u64>,
        model_switched: bool,
    ) -> Self {
        if !session_reused {
            if conversational_message_count > 1 {
                Self::MidConversationColdRebuild
            } else {
                Self::FirstTurnColdStart
            }
        } else if cached_read_tokens == 0
            || (previous_session_context_tokens > 0
                && cached_read_tokens.saturating_mul(2) < previous_session_context_tokens)
        {
            if model_switched {
                Self::WarmModelSwitched
            } else if let Some(idle_ms) = session_idle_ms
                && idle_ms >= PromptCacheWarmth::WARM_IDLE_THRESHOLD_MS
            {
                Self::WarmIdleTtlExpired
            } else {
                Self::WarmUpstreamImplicitMiss
            }
        } else {
            Self::WarmLargeDeltaDilution
        }
    }
}

/// How `turn_runner` matched (or created) the ACP session for a turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LowCacheRoutingStrategy {
    /// Resumed a live parked `session/prompt` cascade directly over loopback MCP `tools/call`.
    McpSuspendedContinuation,
    /// Matched warm session via OpenAI `previous_response_id`.
    PreviousResponseId,
    /// Matched warm session via deterministic conversation prefix SHA-256 hash.
    PrefixHash,
    /// Matched warm pre-compaction session via pure dialogue SHA-256 hash.
    CompactionDialogueHash,
    /// Created a new ACP session for a first-turn conversation.
    #[default]
    ColdNewSession,
    /// Created a new ACP session replaying multi-turn history after missing warm pool.
    ColdMidConversationRebuild,
    #[serde(other)]
    Unknown,
}

/// Exhaustive structural, byte-level, and routing metrics captured for low-cache-hit analysis.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct LowCacheRequestMetrics {
    #[serde(default)]
    pub routing_strategy: LowCacheRoutingStrategy,
    #[serde(default)]
    pub session_turn_index: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_idle_ms: Option<u64>,
    #[serde(default)]
    pub previous_session_context_tokens: u64,
    #[serde(default)]
    pub model_switched: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix_hash_hex: Option<String>,
    #[serde(default)]
    pub base_state_hash_hex: String,
    #[serde(default)]
    pub tools_updated_on_warm_turn: bool,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub total_message_count: usize,
    #[serde(default)]
    pub conversational_message_count: usize,
    #[serde(default)]
    pub user_message_count: usize,
    #[serde(default)]
    pub assistant_message_count: usize,
    #[serde(default)]
    pub delta_message_count: usize,
    #[serde(default)]
    pub total_conversation_bytes: usize,
    #[serde(default)]
    pub delta_messages_bytes: usize,
    #[serde(default)]
    pub developer_instructions_blocks: usize,
    #[serde(default)]
    pub developer_instructions_bytes: usize,
    #[serde(default)]
    pub user_text_bytes: usize,
    #[serde(default)]
    pub assistant_text_bytes: usize,
    #[serde(default)]
    pub reasoning_part_count: usize,
    #[serde(default)]
    pub reasoning_bytes: usize,
    #[serde(default)]
    pub tool_call_part_count: usize,
    #[serde(default)]
    pub tool_call_bytes: usize,
    #[serde(default)]
    pub tool_result_part_count: usize,
    #[serde(default)]
    pub tool_result_bytes: usize,
    #[serde(default)]
    pub image_part_count: usize,
    #[serde(default)]
    pub image_bytes: usize,
    #[serde(default)]
    pub client_tool_count: usize,
    #[serde(default)]
    pub client_tools_schema_bytes: usize,
    #[serde(default)]
    pub acp_system_append_bytes: usize,
    #[serde(default)]
    pub tools_delta_xml_bytes: usize,
    #[serde(default)]
    pub outbound_acp_prompt_bytes: usize,
    #[serde(default)]
    pub outbound_acp_text_blocks: usize,
    #[serde(default)]
    pub outbound_acp_image_blocks: usize,
    #[serde(default)]
    pub thought_tokens: u64,
    #[serde(default)]
    pub thought_chars: usize,
    #[serde(default)]
    pub output_chars: usize,
    #[serde(default)]
    pub native_web_searches: usize,
    #[serde(default)]
    pub native_web_fetches: usize,
    #[serde(default)]
    pub vision_offloaded_images: usize,
    #[serde(default)]
    pub emitted_client_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

/// Per-message structural digest stored inline inside [`LowCacheRequestCapture`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct CapturedMessageSummary {
    pub index: usize,
    pub role: String,
    pub in_delta: bool,
    pub total_bytes: usize,
    pub part_kinds: Vec<String>,
    pub excerpt: String,
}

/// Bounded inline request payload snapshot attached to [`LowCacheHitRecord`] when
/// `capture_request_details` is enabled (`true` by default).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors the four orthogonal web bridge flags plus full-payload persistence status"
)]
pub struct LowCacheRequestCapture {
    #[serde(default)]
    pub client_tool_names: Vec<String>,
    #[serde(default)]
    pub enable_native_web_search: bool,
    #[serde(default)]
    pub force_native_web_search: bool,
    #[serde(default)]
    pub enable_native_web_fetch: bool,
    #[serde(default)]
    pub force_native_web_fetch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_system_append_excerpt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_delta_xml_excerpt: Option<String>,
    #[serde(default)]
    pub outbound_acp_prompt_excerpt: String,
    #[serde(default)]
    pub output_preview: String,
    #[serde(default)]
    pub message_digests: Vec<CapturedMessageSummary>,
    #[serde(default)]
    pub full_payload_saved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_file_path: Option<String>,
}

/// Serialized ACP prompt block stored inside [`LowCacheFullRequestPayload`].
///
/// Image blocks keep their base64 payload only when it is a small raster image that fits the
/// capture's image budget, so dashboards can render a preview without bloating captures.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CapturedAcpPromptBlock {
    Text {
        bytes: usize,
        text: String,
    },
    Image {
        mime_type: String,
        base64_bytes: usize,
        /// Bounded base64 preview; absent on older captures or images over the preview cap.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
    },
}

/// Serialized message part stored inside [`LowCacheFullRequestPayload`].
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapturedFullPart {
    Text {
        is_developer_instructions: bool,
        bytes: usize,
        text: String,
    },
    Reasoning {
        bytes: usize,
        text: String,
    },
    Image {
        media_type: String,
        base64_bytes: usize,
        /// Bounded base64 preview; absent on older captures or images over the preview cap.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
    },
    ToolCall {
        call_id: String,
        name: String,
        is_custom: bool,
        bytes: usize,
        arguments: String,
    },
    ToolResult {
        call_id: String,
        is_error: bool,
        bytes: usize,
        output: String,
    },
}

/// Complete normalized conversation message stored in [`LowCacheFullRequestPayload`].
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct CapturedFullMessage {
    pub index: usize,
    pub role: String,
    pub in_delta: bool,
    pub total_bytes: usize,
    pub parts: Vec<CapturedFullPart>,
}

/// Client tool specification summary stored in [`LowCacheFullRequestPayload`].
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct CapturedToolSpec {
    pub name: String,
    pub kind: String,
    pub schema_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_or_format: Option<String>,
}

/// Full on-disk diagnostic payload persisted at `~/.liftgate/low-cache-payloads/<task_id>.json`
/// when `capture_request_details` is enabled (`true` by default).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct LowCacheFullRequestPayload {
    pub task_id: String,
    pub recorded_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_system_append: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_delta_xml: Option<String>,
    #[serde(default)]
    pub outbound_acp_prompt_blocks: Vec<CapturedAcpPromptBlock>,
    #[serde(default)]
    pub messages: Vec<CapturedFullMessage>,
    #[serde(default)]
    pub client_tools: Vec<CapturedToolSpec>,
    #[serde(default)]
    pub assistant_output_preview: String,
    #[serde(default)]
    pub payload_truncated: bool,
}

/// Response served by `GET /api/telemetry/low-cache-hits/{task_id}`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LowCachePayloadDetailResponse {
    pub record: LowCacheHitRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_payload: Option<LowCacheFullRequestPayload>,
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
    #[serde(default)]
    pub diagnostic_cause: LowCacheDiagnosticCause,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_metrics: Option<LowCacheRequestMetrics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_capture: Option<LowCacheRequestCapture>,
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
    #[serde(default = "default_true")]
    pub capture_request_details: bool,
    #[serde(default)]
    pub persisted_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_dir: Option<String>,
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
    #[serde(default)]
    pub auxiliary_turns: u64,
    #[serde(default)]
    pub auxiliary_prompt_tokens: u64,
}

impl Default for CacheTelemetrySummary {
    fn default() -> Self {
        Self {
            revision: 0,
            max_low_cache_records: crate::config::DEFAULT_MAX_LOW_CACHE_RECORDS,
            low_cache_threshold_bps: crate::config::DEFAULT_LOW_CACHE_THRESHOLD_BPS,
            capture_request_details: true,
            persisted_path: None,
            payload_dir: None,
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
            auxiliary_turns: 0,
            auxiliary_prompt_tokens: 0,
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

/// Cumulative Elastic I/O, 0-RTT MCP resolution, `< 9.6 KB` inline budget, and tool virtualization telemetry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ElasticIoTelemetry {
    /// MCP sessions registered in 1:1 Direct Eager passthrough (`<= 36` tools, `<= 28 KiB`, `0` hubs).
    #[serde(default)]
    pub passthrough_sessions: u64,
    /// MCP sessions registered with 3-tier `mcp_lg_ns__*` namespace hub virtualization.
    #[serde(default)]
    pub virtualized_sessions: u64,
    /// Total top-level `mcp_lg_*` client tool calls forwarded over HTTP/SSE.
    #[serde(default)]
    pub mcp_forwarded_tool_calls: u64,
    /// Forwarded calls targeting compound orchestrator / Bash-first tools (`exec`, `execute`, `eval`, `REPL`, `Bash`, `exec_command`, `shell`, `bash`).
    #[serde(default)]
    pub mcp_orchestrator_calls: u64,
    /// Completed or suspended turns that emitted at least one forwarded client tool call.
    #[serde(default)]
    pub mcp_tool_using_turns: u64,
    /// Forwarded V4A `apply_patch` / `patch` calls containing at least one file operation header.
    #[serde(default)]
    pub mcp_patch_calls: u64,
    /// Forwarded V4A patch calls coalescing `>= 2` target files in a single atomic call.
    #[serde(default)]
    pub mcp_multi_file_patch_calls: u64,
    /// Total file operation headers (`*** Add/Update/Delete File:`) across V4A patch calls.
    #[serde(default)]
    pub mcp_patch_files_total: u64,
    /// Gateway-local 0-RTT `mcp_lg_search_client_tools` lookups resolved in-memory (`< 0.1 ms`).
    #[serde(default)]
    pub mcp_zero_rtt_searches: u64,
    /// Gateway-local 0-RTT `mcp_lg_read_context_spill` slices/queries resolved in-memory.
    #[serde(default)]
    pub mcp_zero_rtt_spill_reads: u64,
    /// Gateway-local 0-RTT schema & V4A preflight self-healing rejections answered in-memory.
    #[serde(default)]
    pub mcp_zero_rtt_preflight_heals: u64,
    /// Total MCP client tool results sanitized by the gateway.
    #[serde(default)]
    pub mcp_tool_results_total: u64,
    /// MCP client tool results that exceeded `9,600 B` (`MAX_INLINE_MCP_TOOL_RESULT_WITH_SPILL_BYTES`) and spilled.
    #[serde(default)]
    pub mcp_tool_results_spilled: u64,
    /// Cumulative lossless spill files persisted to `~/.liftgate/spills` (prompts, tool results, schemas).
    #[serde(default)]
    pub context_spills_created: u64,
    /// Cumulative UTF-8 bytes persisted across lossless spill files.
    #[serde(default)]
    pub context_spills_bytes: u64,
}

const fn default_true() -> bool {
    true
}

/// Outcome recorded by the most recent idle auto-update check or upgrade attempt for a target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoUpdateLastOutcome {
    #[default]
    NeverChecked,
    UpToDate,
    UpdateAvailable,
    Updated,
    DeferredBusy,
    NotInstalled,
    Skipped,
    Failed,
    #[serde(other)]
    Unknown,
}

/// Per-component status entry inside [`AutoUpdateTelemetrySummary`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AutoUpdateTargetTelemetry {
    pub target: AutoUpdateTarget,
    pub enabled: bool,
    #[serde(default)]
    pub installed_version: Option<String>,
    #[serde(default)]
    pub binary_path: Option<String>,
    #[serde(default)]
    pub latest_seen_version: Option<String>,
    #[serde(default)]
    pub last_check_unix_ms: Option<u64>,
    #[serde(default)]
    pub last_update_unix_ms: Option<u64>,
    #[serde(default)]
    pub last_updated_from: Option<String>,
    #[serde(default)]
    pub last_updated_to: Option<String>,
    #[serde(default)]
    pub last_status: AutoUpdateLastOutcome,
    #[serde(default)]
    pub note: Option<String>,
}

/// Fleet-wide idle auto-update configuration and status published by the gateway daemon.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AutoUpdateTelemetrySummary {
    pub interval_mins: u64,
    pub idle_ready: bool,
    #[serde(default)]
    pub busy_reason: Option<String>,
    #[serde(default)]
    pub sweep_in_progress: bool,
    #[serde(default)]
    pub active_target: Option<AutoUpdateTarget>,
    #[serde(default)]
    pub targets: Vec<AutoUpdateTargetTelemetry>,
}

/// Live status of one configured upstream provider surfaced on telemetry snapshots.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProviderTelemetrySummary {
    pub id: String,
    pub display_name: String,
    pub driver: ProviderDriverKind,
    pub family: ProviderFamily,
    pub wire_protocol: UpstreamWireProtocol,
    pub tool_wire_mode: ToolWireMode,
    pub enabled: bool,
    pub active: bool,
    pub auth_configured: bool,
    pub auth_source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub default_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_model: Option<String>,
    pub spill_enabled: bool,
}

/// Top-level observability payload served over `GET /api/telemetry/snapshot` and SSE.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GatewayTelemetrySnapshot {
    pub generated_at_unix_ms: u64,
    pub gateway_version: Cow<'static, str>,
    #[serde(default = "default_provider_id_string")]
    pub active_provider: String,
    #[serde(default)]
    pub optimization_mode: OptimizationMode,
    #[serde(default)]
    pub optimization_policy: OptimizationPolicy,
    #[serde(default)]
    pub providers: Vec<ProviderTelemetrySummary>,
    #[serde(default)]
    pub vision_offload_enabled: bool,
    #[serde(default = "default_true")]
    pub mcp_tool_bridge_enabled: bool,
    pub acp_process: AcpProcessTelemetry,
    pub session_pool: SessionPoolTelemetry,
    pub metrics: GatewayMetricsSummary,
    #[serde(default)]
    pub elastic_io: ElasticIoTelemetry,
    #[serde(default)]
    pub cache_telemetry: CacheTelemetrySummary,
    #[serde(default)]
    pub tool_call_telemetry: ToolCallTelemetrySummary,
    #[serde(default)]
    pub context_capture: ContextCaptureSummary,
    /// Recorded token usage and its cost.
    #[serde(default)]
    pub usage: UsageSummary,
    /// Fleet idle auto-update configuration and latest check/update status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_update: Option<AutoUpdateTelemetrySummary>,
    pub active_tasks: Vec<TaskSnapshot>,
    pub recent_tasks: Vec<TaskSnapshot>,
    pub available_models: Vec<AvailableModel>,
}

// ============================================================================
// Per-call tool I/O telemetry (`/api/telemetry/tool-calls`)
// ============================================================================

/// How a tool call travelled through the gateway.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallRoute {
    /// Suspended or XML round-trip executed by the caller harness (`exec`, `Bash`, `apply_patch`, ...).
    ClientRoundtrip,
    /// `mcp_lg_ns__*` namespace hub unwrapped to the original client tool.
    NamespaceHub,
    /// `mcp_lg_call_client_tool` generic dispatcher unwrapped to the original client tool.
    GenericDispatcher,
    /// Gateway-local `mcp_lg_search_client_tools` answered without a round-trip.
    ZeroRttSearch,
    /// Gateway-local `mcp_lg_read_context_spill` paging.
    ZeroRttSpillRead,
    /// Gateway-local preflight rejection (missing fields, V4A syntax, unknown or disabled tool).
    ZeroRttPreflight,
    /// Gateway-hosted tool such as `mcp_lg_gateway_web_fetch`.
    GatewayManaged,
    #[serde(other)]
    Unknown,
}

/// Lifecycle state of a recorded tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Emitted to the caller; the result has not come back yet.
    Pending,
    Ok,
    Error,
    /// Answered locally by gateway preflight without reaching the caller.
    PreflightRejected,
    /// The result never came back (session evicted, turn cancelled, or result never sent).
    Abandoned,
    #[serde(other)]
    Unknown,
}

/// Coarse classification of a failed tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallErrorKind {
    /// The caller marked the result as an error.
    ToolError,
    /// A CodeMode script (`exec` / `execute` / `eval` / `REPL`) failed at runtime.
    ScriptException,
    /// Gateway preflight rejected the arguments (schema, V4A syntax, unknown action).
    PreflightRejected,
    /// The model invoked a tool that is not active for the turn.
    DisabledTool,
    #[serde(other)]
    Unknown,
}

/// Lightweight row served by `GET /api/telemetry/tool-calls`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent per-call flags (orchestrator, V4A repair, sanitization, spill) shown as separate badges"
)]
pub struct ToolCallRecord {
    /// Call id as emitted to the caller (`call_*`), or a gateway-local `lg_*` id.
    pub call_id: String,
    /// Insertion order inside the ring buffer; records with `seq < summary.oldest_seq` were evicted.
    pub seq: u64,
    /// Buffer revision at which this record last changed (used for incremental `?since=` polling).
    pub revision: u64,
    pub task_id: String,
    #[serde(default = "default_provider_id_string")]
    pub provider_id: String,
    #[serde(default)]
    pub session_id: Option<String>,
    /// Session turn (one per HTTP request against the session) that emitted the call.
    #[serde(default)]
    pub turn_index: u64,
    /// 0-based order of the call inside its turn.
    #[serde(default)]
    pub step_index: u32,
    /// Detected harness profile (`codex`, `claude-code`, `opencode`, `omp`, `generic`).
    pub harness_profile: String,
    #[serde(default)]
    pub orchestrator_mode: Option<String>,
    pub model_id: String,
    pub route: ToolCallRoute,
    /// Tool name the model invoked (`mcp_lg_*` for MCP calls, the client name for XML calls).
    pub wire_tool_name: String,
    /// Client tool that executed the call (`None` for gateway-local calls).
    #[serde(default)]
    pub client_tool_name: Option<String>,
    /// `true` for CodeMode orchestrators (`exec`, `execute`, `eval`, `REPL`).
    #[serde(default)]
    pub is_orchestrator: bool,
    /// Script language when it is known (`js`, `py`).
    #[serde(default)]
    pub code_language: Option<String>,
    pub status: ToolCallStatus,
    #[serde(default)]
    pub error_kind: Option<ToolCallErrorKind>,
    /// The gateway rewrote a malformed V4A patch before forwarding it.
    #[serde(default)]
    pub v4a_auto_repaired: bool,
    /// Terminal control sequences or billing noise were scrubbed from the output.
    #[serde(default)]
    pub output_sanitized: bool,
    /// The output exceeded the inline budget; the model saw a head/tail excerpt plus a spill file.
    #[serde(default)]
    pub output_spilled: bool,
    pub started_at_unix_ms: u64,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub input_tokens_est: u64,
    pub output_tokens_est: u64,
    pub input_preview: String,
    pub output_preview: String,
}

/// Output block of a recorded tool call.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolOutputBlock {
    Text {
        bytes: usize,
        text: String,
    },
    Image {
        mime_type: String,
        base64_bytes: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<u32>,
        /// Bounded base64 preview; absent on older records or images over the preview cap.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
    },
}

/// Predicate for `#[serde(skip_serializing_if = "is_false")]` on boolean flags.
const fn is_false(value: &bool) -> bool {
    !*value
}

/// Full record served by `GET /api/telemetry/tool-calls/{call_id}`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ToolCallDetail {
    #[serde(flatten)]
    pub record: ToolCallRecord,
    /// Arguments exactly as the model emitted them on the wire tool.
    pub wire_arguments: String,
    /// Arguments forwarded to the client tool after unwrapping or repair (`None` when unchanged).
    #[serde(default)]
    pub client_arguments: Option<String>,
    #[serde(default)]
    pub output_blocks: Vec<ToolOutputBlock>,
    /// Output text actually handed back to the model when the gateway spilled or annotated the
    /// client tool result (`None` when forwarded unchanged after basic control-noise scrubbing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarded_output: Option<String>,
    /// UTF-8 byte length of the forwarded text before any per-field storage cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarded_output_bytes: Option<usize>,
    /// The per-field byte cap clipped the stored input.
    #[serde(default)]
    pub input_truncated: bool,
    /// The per-field byte cap clipped the stored output.
    #[serde(default)]
    pub output_truncated: bool,
    /// The per-field byte cap clipped [`Self::forwarded_output`].
    #[serde(default, skip_serializing_if = "is_false")]
    pub forwarded_output_truncated: bool,
}

/// Ring-buffer status carried on the snapshot as `tool_call_telemetry`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ToolCallTelemetrySummary {
    /// Identifies the buffer (one per gateway process): revisions and `seq` start over when it
    /// changes, so clients holding older records must refetch everything.
    #[serde(default)]
    pub store_id: String,
    /// Bumped on every insert, completion, or eviction.
    #[serde(default)]
    pub revision: u64,
    /// `seq` of the oldest retained record (equal to the next `seq` when empty).
    #[serde(default)]
    pub oldest_seq: u64,
    #[serde(default)]
    pub max_records: usize,
    /// Per-field cap applied to stored arguments and output text.
    #[serde(default)]
    pub max_field_bytes: usize,
    #[serde(default)]
    pub max_total_bytes: usize,
    #[serde(default)]
    pub stored_records: usize,
    #[serde(default)]
    pub stored_bytes: usize,
    #[serde(default)]
    pub total_recorded: u64,
    #[serde(default)]
    pub total_evicted: u64,
}

/// Response served by `GET /api/telemetry/tool-calls[?since=<revision>]`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ToolCallsResponse {
    pub summary: ToolCallTelemetrySummary,
    /// Records changed after `since` (all retained records when `since` is absent), oldest first.
    pub records: Vec<ToolCallRecord>,
}

// ============================================================================
// Session tool catalog (`/api/telemetry/sessions/{session_id}/tool-catalog`)
// ============================================================================

/// Where a declared client tool ended up after 3-tier virtualization.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogToolTier {
    /// Tier-0 CodeMode orchestrator mounted on `lg`.
    Orchestrator,
    /// Tier-1 one-to-one direct tool.
    Direct,
    /// Folded into the orchestrator contract (e.g. Oh My Pi `<eval_session_tools>`).
    FoldedIntoOrchestrator,
    /// Tier-2 action folded behind an `mcp_lg_ns__*` hub.
    NamespaceHubAction,
    /// Promoted back to a direct tool by slack-slot backfill.
    SlackBackfill,
    /// Reachable only through the 0-RTT search index and `mcp_lg_call_client_tool`.
    SearchIndexOnly,
    #[serde(other)]
    Unknown,
}

/// Role of a tool mounted on the `lg` loopback MCP server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogWireRole {
    Orchestrator,
    Direct,
    NamespaceHub,
    SlackBackfill,
    /// Gateway-provided tool (`search_client_tools`, `call_client_tool`, `gateway_web_fetch`, `read_context_spill`).
    Gateway,
    #[serde(other)]
    Unknown,
}

/// One client tool as declared by the caller and as mounted for the model.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct CatalogToolEntry {
    pub client_name: String,
    pub tier: CatalogToolTier,
    /// Wire tool (`mcp_lg_*` without prefix) that serves this client tool.
    #[serde(default)]
    pub wire_name: Option<String>,
    /// `function` or `custom`.
    pub client_kind: String,
    #[serde(default)]
    pub client_description: Option<String>,
    /// JSON Schema or custom grammar exactly as the client declared it.
    #[serde(default)]
    pub client_schema: Option<String>,
    pub client_bytes: usize,
    /// Description mounted on `lg` (`None` when the tool is not mounted directly).
    #[serde(default)]
    pub wire_description: Option<String>,
    #[serde(default)]
    pub wire_schema: Option<String>,
    /// Wire bytes attributable to this tool (hub bytes are split across its actions).
    pub wire_bytes: usize,
    /// TypeScript declaration indexed for 0-RTT search (Codex `code_mode_host` declarations).
    #[serde(default)]
    pub ts_signature: Option<String>,
}

/// One tool mounted on the `lg` loopback MCP server.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct CatalogWireTool {
    /// MCP-local name (the model sees `mcp_lg_<wire_name>`).
    pub wire_name: String,
    pub role: CatalogWireRole,
    #[serde(default)]
    pub description: Option<String>,
    pub input_schema: String,
    pub bytes: usize,
    /// Client tools folded behind this wire tool (hub actions or orchestrator-folded tools).
    #[serde(default)]
    pub folded_actions: Vec<String>,
}

/// Category of context bytes shown in the session composition bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSegmentKey {
    WireTools,
    BridgeRules,
    OrchestratorContract,
    ClientToolsUpdate,
    DeveloperInstructions,
    UserText,
    AssistantText,
    Reasoning,
    ToolCalls,
    ToolResults,
    Images,
    #[serde(other)]
    Unknown,
}

/// Bytes and estimated tokens of one context category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ContextSegment {
    pub key: ContextSegmentKey,
    pub bytes: usize,
    pub tokens_est: u64,
}

/// Response served by `GET /api/telemetry/sessions/{session_id}/tool-catalog`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionToolCatalog {
    pub session_id: String,
    pub harness_profile: String,
    #[serde(default)]
    pub orchestrator_mode: Option<String>,
    pub is_passthrough: bool,
    pub model_id: String,
    /// When the context composition was sampled (`0` before the first completed turn).
    #[serde(default)]
    pub captured_at_unix_ms: u64,
    pub wire_budget_bytes: usize,
    pub eager_tool_budget: usize,
    /// Bytes of the tool list as the client declared it (never sent upstream verbatim).
    pub client_tools_bytes: usize,
    /// Context composition of the session's latest completed turn.
    pub segments: Vec<ContextSegment>,
    pub wire_tools: Vec<CatalogWireTool>,
    pub tools: Vec<CatalogToolEntry>,
    /// `<api_gateway_bridge_rules>` sent in `_meta.antigravity.systemPrompt.append` at `session/new`.
    #[serde(default)]
    pub bridge_rules_prompt: Option<String>,
    /// The CodeMode orchestrator contract embedded in the bridge rules.
    #[serde(default)]
    pub orchestrator_contract_prompt: Option<String>,
    pub search_index_entries: usize,
}

// ============================================================================
// Opt-in full-context capture (`/api/telemetry/context-captures`)
// ============================================================================

/// Full-context capture status carried on the snapshot as `context_capture`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ContextCaptureSummary {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub max_records: usize,
    #[serde(default)]
    pub max_total_bytes: u64,
    #[serde(default)]
    pub stored_records: usize,
    #[serde(default)]
    pub stored_bytes: u64,
    /// Completed turns left uncaptured because earlier captures were still being written
    /// (reset when captures are cleared).
    #[serde(default)]
    pub skipped_records: u64,
    #[serde(default)]
    pub capture_dir: Option<String>,
}

/// Index row describing one captured turn.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ContextCaptureRecord {
    pub task_id: String,
    pub recorded_at_unix_ms: u64,
    #[serde(default = "default_provider_id_string")]
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optimization_mode: Option<OptimizationMode>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub harness_profile: Option<String>,
    pub resolved_model: String,
    #[serde(default)]
    pub session_turn_index: u64,
    #[serde(default)]
    pub cache_hit_rate_bps: Option<u16>,
    #[serde(default)]
    pub total_prompt_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub message_count: usize,
    #[serde(default)]
    pub delta_message_count: usize,
    /// Serialized payload size on disk.
    #[serde(default)]
    pub payload_bytes: u64,
    #[serde(default)]
    pub prompt_preview: String,
    /// Places where the gateway changed the harness's context before forwarding it
    /// ([`FullContextPayload::rewrites`]); `None` on captures written before rewrite tracking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewrite_count: Option<usize>,
}

/// Why the gateway changed a piece of harness context before forwarding it upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextRewriteKind {
    /// A harness system / developer prompt replaced by its distilled ACP-aligned contract.
    PromptDistilled,
    /// A harness developer item dropped as redundant harness-internal boilerplate.
    PromptDropped,
    /// A client tool result cleaned (billing noise, terminal control sequences) or annotated with
    /// a gateway recovery hint before being handed back to the model.
    ToolResultSanitized,
    /// A client tool result exceeded the inline byte limit; the model received a head/tail excerpt
    /// plus a local spill file reference.
    ToolResultSpilled,
    /// Unrecognized rewrite kind written by a newer gateway version.
    #[serde(other)]
    Unknown,
}

/// One gateway change to the harness context: what the harness sent and what went upstream.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ContextRewrite {
    pub kind: ContextRewriteKind,
    /// Index into [`LowCacheFullRequestPayload::messages`] of the rewritten message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_index: Option<usize>,
    /// Index of the rewritten part inside that message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_index: Option<usize>,
    /// Wire slot of a rewritten prompt: `primary_base`, `anthropic_system`, or `developer_item`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    /// Call answered by a rewritten tool result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Size of the harness's original text before any capture cap.
    pub original_bytes: usize,
    /// Size of the forwarded text before any capture cap (`0` when dropped).
    pub rewritten_bytes: usize,
    pub original: String,
    /// Forwarded text; `None` when the gateway dropped the part entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewritten: Option<String>,
    /// `true` when `original` or `rewritten` was clipped to the capture safety budget.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

/// Response served by `GET /api/telemetry/context-captures` (newest first).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ContextCapturesResponse {
    pub summary: ContextCaptureSummary,
    pub records: Vec<ContextCaptureRecord>,
}

/// Stage-3 wire delivery summary enabling 3-stage comparison:
/// `Raw Harness Input -> Gateway Provider-Aware Optimization -> Final Upstream Provider Wire`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct UpstreamWireSummary {
    pub provider_id: String,
    pub driver: ProviderDriverKind,
    pub wire_protocol: UpstreamWireProtocol,
    pub optimization_mode: OptimizationMode,
    /// `true` when ingress and upstream share the same native wire format (e.g. Codex -> OpenAI Responses).
    pub isomorphic_wire: bool,
    pub raw_system_bytes: usize,
    pub forwarded_system_bytes: usize,
    pub raw_tool_count: usize,
    pub raw_tool_bytes: usize,
    pub forwarded_tool_count: usize,
    pub forwarded_tool_bytes: usize,
    pub spill_enabled: bool,
    pub cache_breakpoints_aligned: usize,
    pub zero_rtt_continuations: usize,
}

/// On-disk payload of one captured turn.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct FullContextPayload {
    #[serde(flatten)]
    pub request: LowCacheFullRequestPayload,
    /// Three-stage pipeline delivery summary (`None` on captures written before multi-provider support).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_wire: Option<UpstreamWireSummary>,
    /// Reasoning the model produced during this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_reasoning: Option<String>,
    /// Complete text output of this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_output: Option<String>,
    /// Reasoning beyond the per-field capture cap was dropped.
    #[serde(default)]
    pub assistant_reasoning_truncated: bool,
    /// Output beyond the per-field capture cap was dropped.
    #[serde(default)]
    pub assistant_output_truncated: bool,
    /// Every place the gateway changed the harness context before forwarding it, in message order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rewrites: Vec<ContextRewrite>,
}

/// Response served by `GET /api/telemetry/context-captures/{task_id}`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ContextCaptureDetailResponse {
    pub record: ContextCaptureRecord,
    pub payload: FullContextPayload,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_detail_flattens_the_record_and_tolerates_unknown_enum_values() {
        let detail = ToolCallDetail {
            record: ToolCallRecord {
                call_id: "call_abc".to_owned(),
                seq: 3,
                revision: 9,
                task_id: "resp_1".to_owned(),
                provider_id: "agy".to_owned(),
                session_id: Some("sess".to_owned()),
                turn_index: 2,
                step_index: 0,
                harness_profile: "codex".to_owned(),
                orchestrator_mode: Some("codex_exec".to_owned()),
                model_id: "gemini-4-carbon-high".to_owned(),
                route: ToolCallRoute::ClientRoundtrip,
                wire_tool_name: "mcp_lg_exec".to_owned(),
                client_tool_name: Some("exec".to_owned()),
                is_orchestrator: true,
                code_language: Some("js".to_owned()),
                status: ToolCallStatus::Ok,
                error_kind: None,
                v4a_auto_repaired: false,
                output_sanitized: false,
                output_spilled: false,
                started_at_unix_ms: 1,
                duration_ms: Some(5),
                input_bytes: 10,
                output_bytes: 20,
                input_tokens_est: 3,
                output_tokens_est: 5,
                input_preview: "text(1)".to_owned(),
                output_preview: "1".to_owned(),
            },
            wire_arguments: "text(1)".to_owned(),
            client_arguments: None,
            output_blocks: vec![ToolOutputBlock::Image {
                mime_type: "image/png".to_owned(),
                base64_bytes: 128,
                width: Some(2),
                height: None,
                data: None,
            }],
            forwarded_output: None,
            forwarded_output_bytes: None,
            input_truncated: false,
            output_truncated: false,
            forwarded_output_truncated: false,
        };
        let json = serde_json::to_value(&detail).unwrap();
        assert_eq!(json["call_id"], "call_abc");
        assert_eq!(json["route"], "client_roundtrip");
        assert_eq!(json["output_blocks"][0]["type"], "image");
        assert!(json["output_blocks"][0].get("height").is_none());
        let round: ToolCallDetail = serde_json::from_value(json).unwrap();
        assert_eq!(round, detail);

        let route: ToolCallRoute = serde_json::from_str("\"teleport\"").unwrap();
        assert_eq!(route, ToolCallRoute::Unknown);
        let status: ToolCallStatus = serde_json::from_str("\"exploded\"").unwrap();
        assert_eq!(status, ToolCallStatus::Unknown);
        let kind: ContextRewriteKind = serde_json::from_str("\"future_rewrite_v2\"").unwrap();
        assert_eq!(kind, ContextRewriteKind::Unknown);
    }

    #[test]
    fn snapshot_without_new_sections_still_decodes() {
        let summary: ToolCallTelemetrySummary = serde_json::from_str("{}").unwrap();
        assert_eq!(summary.stored_records, 0);
        let capture: ContextCaptureSummary = serde_json::from_str("{}").unwrap();
        assert!(!capture.enabled);
    }
}
