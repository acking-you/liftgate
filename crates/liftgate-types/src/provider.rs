//! Provider identity, capability matrix, optimization policy, and multi-provider configuration models.
//!
//! This module contains zero async runtime, HTTP, or filesystem dependencies. It models both:
//! - **Camp A (`ProviderFamily::StatefulSession`)**: Stateful JSON-RPC `stdio` session protocols
//!   such as `agy acp`, `codex app-server`, and `grok acp`.
//! - **Camp B (`ProviderFamily::StatelessHttp`)**: Official stateless Platform HTTP/SSE APIs
//!   such as OpenAI Responses (`/v1/responses`), Anthropic Messages (`/v1/messages`), and
//!   Google Gemini (`streamGenerateContent`), as well as compatible custom endpoints.

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{ModelId, ProxyUrl};
use crate::session_key::{MAX_ACP_PROMPT_TEXT_BYTES, McpOrchestratorContract};
use crate::telemetry::TaskProtocol;

/// Maximum length in ASCII bytes of a validated [`ProviderId`].
pub const MAX_PROVIDER_ID_BYTES: usize = 64;

/// Default single tool-result spill threshold (`9.6 KB`) calibrated for `agy acp` `~10 KiB` truncation wall.
pub const AGY_ACP_TOOL_RESULT_SPILL_BYTES: usize = 9_600;

/// Default eager wire tool budget (`34 KiB`) calibrated for `agy acp 1.5.18`.
pub const AGY_ACP_EAGER_TOOL_BUDGET_BYTES: usize = 34 * 1024;

/// Default eager wire tool budget (`96 KiB`) for native Platform HTTP APIs before engaging
/// unprefixed 3-tier tool virtualization on massive MCP plugin sessions.
pub const NATIVE_API_EAGER_TOOL_BUDGET_BYTES: usize = 96 * 1024;

const _: () = assert!(
    NATIVE_API_EAGER_TOOL_BUDGET_BYTES > AGY_ACP_EAGER_TOOL_BUDGET_BYTES,
    "native Platform API tool schema budget must exceed agy acp silent-drop budget"
);

/// Syntax failure when parsing or constructing a [`ProviderId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum ProviderIdError {
    #[error("provider identifier must not be empty")]
    Empty,

    #[error("provider identifier exceeds maximum length of {MAX_PROVIDER_ID_BYTES} characters")]
    TooLong,

    #[error(
        "provider identifier must start with [a-z0-9] and contain only lowercase ASCII letters, digits, '-' or '_'"
    )]
    InvalidCharacters,
}

/// Validated lowercase ASCII provider identifier (`^[a-z0-9][a-z0-9_-]{0,63}$`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProviderId(Arc<str>);

impl ProviderId {
    pub const AGY: &'static str = "agy";
    pub const OPENAI: &'static str = "openai";
    pub const ANTHROPIC: &'static str = "anthropic";
    pub const GEMINI: &'static str = "gemini";
    pub const CODEX_APP_SERVER: &'static str = "codex-app-server";
    pub const GROK_ACP: &'static str = "grok-acp";

    /// Normalizes common human aliases and validates slug syntax.
    pub fn try_new(raw: impl AsRef<str>) -> Result<Self, ProviderIdError> {
        let trimmed = raw.as_ref().trim();
        if trimmed.is_empty() {
            return Err(ProviderIdError::Empty);
        }
        let normalized = Self::normalize_builtin_alias(trimmed);
        if normalized.len() > MAX_PROVIDER_ID_BYTES {
            return Err(ProviderIdError::TooLong);
        }
        let mut bytes = normalized.bytes();
        let Some(first) = bytes.next() else {
            return Err(ProviderIdError::Empty);
        };
        if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
            return Err(ProviderIdError::InvalidCharacters);
        }
        if !bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
            return Err(ProviderIdError::InvalidCharacters);
        }
        Ok(Self(Arc::from(normalized.as_str())))
    }

    #[must_use]
    pub fn normalize_builtin_alias(raw: &str) -> String {
        let lower = raw.trim().to_ascii_lowercase();
        match lower.as_str() {
            "antigravity" | "agy-acp" | "agy_acp" => Self::AGY.to_owned(),
            "openai-platform" | "openai_platform" => Self::OPENAI.to_owned(),
            "claude" | "claude-api" | "anthropic-platform" => Self::ANTHROPIC.to_owned(),
            "google" | "google-gemini" | "gemini-api" => Self::GEMINI.to_owned(),
            "codex" | "codex-acp" | "codex_app_server" => Self::CODEX_APP_SERVER.to_owned(),
            "grok" | "grok_acp" => Self::GROK_ACP.to_owned(),
            _ => lower,
        }
    }

    /// Returns `true` when `raw` matches any built-in provider ID or canonical alias.
    #[must_use]
    pub fn is_builtin_id_or_alias(raw: &str) -> bool {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "agy"
                | "antigravity"
                | "agy-acp"
                | "agy_acp"
                | "openai"
                | "openai-platform"
                | "openai_platform"
                | "anthropic"
                | "claude-api"
                | "anthropic-platform"
                | "gemini"
                | "google"
                | "google-gemini"
                | "gemini-api"
                | "codex-app-server"
                | "codex-acp"
                | "codex_app_server"
                | "grok-acp"
                | "grok"
                | "grok_acp"
        )
    }

    #[must_use]
    pub fn agy() -> Self {
        Self(Arc::from(Self::AGY))
    }

    #[must_use]
    pub fn openai() -> Self {
        Self(Arc::from(Self::OPENAI))
    }

    #[must_use]
    pub fn anthropic() -> Self {
        Self(Arc::from(Self::ANTHROPIC))
    }

    #[must_use]
    pub fn gemini() -> Self {
        Self(Arc::from(Self::GEMINI))
    }

    #[must_use]
    pub fn codex_app_server() -> Self {
        Self(Arc::from(Self::CODEX_APP_SERVER))
    }

    #[must_use]
    pub fn grok_acp() -> Self {
        Self(Arc::from(Self::GROK_ACP))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ProviderId {
    fn default() -> Self {
        Self::agy()
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ProviderId {
    type Error = ProviderIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<ProviderId> for String {
    fn from(value: ProviderId) -> Self {
        value.0.to_string()
    }
}

/// High-level execution camp of an upstream provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFamily {
    /// Camp A: Stateful JSON-RPC `stdio` subprocess session (`SessionPool` + `job.rs`).
    StatefulSession,
    /// Camp B: Stateless native Platform HTTP/SSE API.
    StatelessHttp,
}

impl ProviderFamily {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::StatefulSession => "stateful_session",
            Self::StatelessHttp => "stateless_http",
        }
    }
}

/// Native upstream wire protocol spoken by a provider driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamWireProtocol {
    AcpSession,
    AppServerSession,
    OpenAiResponses,
    AnthropicMessages,
    GoogleGenerateContent,
}

impl UpstreamWireProtocol {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AcpSession => "acp_session",
            Self::AppServerSession => "app_server_session",
            Self::OpenAiResponses => "openai_responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::GoogleGenerateContent => "google_generate_content",
        }
    }
}

/// How client and virtualized tools are exposed to the upstream model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolWireMode {
    /// Mounted over Liftgate's loopback HTTP MCP server as `mcp_lg_*`.
    AcpLoopbackMcp,
    /// Passed natively in the HTTP API `tools` field without `mcp_lg_` prefix.
    NativeApiTools,
}

/// Tool call execution concurrency profile of the upstream provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDispatchStyle {
    /// Tool round-trips execute sequentially with high per-hop latency; encourage compound batching.
    SerialHighCost,
    /// Upstream model and wire protocol natively emit parallel tool calls in a single turn.
    NativeParallel,
}

/// Upstream prompt cache accounting and breakpoint mechanism.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheSemantics {
    /// Automatic prefix caching (`cached_tokens`, `cachedContentTokenCount`, or stateful session KV).
    ImplicitPrefix,
    /// Explicit cache breakpoint annotations (`cache_control: {"type": "ephemeral"}` on Anthropic).
    ExplicitBreakpoints,
}

/// Closed set of statically dispatched upstream provider protocol drivers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderDriverKind {
    #[default]
    AgyAcp,
    CodexAppServer,
    GrokAcp,
    OpenAiResponses,
    AnthropicMessages,
    GoogleGemini,
}

impl ProviderDriverKind {
    pub const ALL: [Self; 6] = [
        Self::AgyAcp,
        Self::OpenAiResponses,
        Self::AnthropicMessages,
        Self::GoogleGemini,
        Self::CodexAppServer,
        Self::GrokAcp,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgyAcp => "agy_acp",
            Self::CodexAppServer => "codex_app_server",
            Self::GrokAcp => "grok_acp",
            Self::OpenAiResponses => "openai_responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::GoogleGemini => "google_gemini",
        }
    }

    #[must_use]
    pub fn from_slug(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "agy" | "agy_acp" | "agy-acp" | "antigravity" => Some(Self::AgyAcp),
            "codex" | "codex_app_server" | "codex-app-server" | "app-server" => {
                Some(Self::CodexAppServer)
            }
            "grok" | "grok_acp" | "grok-acp" => Some(Self::GrokAcp),
            "openai" | "openai_responses" | "openai-responses" => Some(Self::OpenAiResponses),
            "anthropic" | "anthropic_messages" | "anthropic-messages" | "claude" => {
                Some(Self::AnthropicMessages)
            }
            "gemini" | "google" | "google_gemini" | "google-gemini" => Some(Self::GoogleGemini),
            _ => None,
        }
    }

    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::AgyAcp => "Google Antigravity (agy acp)",
            Self::CodexAppServer => "OpenAI Codex App-Server (stdio)",
            Self::GrokAcp => "Grok ACP (stdio)",
            Self::OpenAiResponses => "OpenAI Platform (/v1/responses)",
            Self::AnthropicMessages => "Anthropic Platform (/v1/messages)",
            Self::GoogleGemini => "Google Gemini API (streamGenerateContent)",
        }
    }

    #[must_use]
    pub const fn family(self) -> ProviderFamily {
        match self {
            Self::AgyAcp | Self::CodexAppServer | Self::GrokAcp => ProviderFamily::StatefulSession,
            Self::OpenAiResponses | Self::AnthropicMessages | Self::GoogleGemini => {
                ProviderFamily::StatelessHttp
            }
        }
    }

    #[must_use]
    pub const fn wire_protocol(self) -> UpstreamWireProtocol {
        match self {
            Self::AgyAcp | Self::GrokAcp => UpstreamWireProtocol::AcpSession,
            Self::CodexAppServer => UpstreamWireProtocol::AppServerSession,
            Self::OpenAiResponses => UpstreamWireProtocol::OpenAiResponses,
            Self::AnthropicMessages => UpstreamWireProtocol::AnthropicMessages,
            Self::GoogleGemini => UpstreamWireProtocol::GoogleGenerateContent,
        }
    }

    #[must_use]
    pub const fn default_endpoint(self) -> Option<&'static str> {
        match self {
            Self::AgyAcp | Self::CodexAppServer | Self::GrokAcp => None,
            Self::OpenAiResponses => Some("https://api.openai.com/v1"),
            Self::AnthropicMessages => Some("https://api.anthropic.com"),
            Self::GoogleGemini => Some("https://generativelanguage.googleapis.com"),
        }
    }

    #[must_use]
    pub const fn default_api_key_env(self) -> Option<&'static str> {
        match self {
            Self::AgyAcp | Self::CodexAppServer => None,
            Self::GrokAcp => Some("XAI_API_KEY"),
            Self::OpenAiResponses => Some("OPENAI_API_KEY"),
            Self::AnthropicMessages => Some("ANTHROPIC_API_KEY"),
            Self::GoogleGemini => Some("GEMINI_API_KEY"),
        }
    }

    #[must_use]
    pub const fn default_binary_name(self) -> Option<&'static str> {
        match self {
            Self::AgyAcp => Some("agy"),
            Self::CodexAppServer => Some("codex"),
            Self::GrokAcp => Some("grok"),
            Self::OpenAiResponses | Self::AnthropicMessages | Self::GoogleGemini => None,
        }
    }

    #[must_use]
    pub const fn default_primary_model(self) -> &'static str {
        match self {
            Self::AgyAcp => "gemini-4-carbon-high",
            Self::CodexAppServer => "gpt-5.3-codex",
            Self::GrokAcp => "grok-4",
            Self::OpenAiResponses => "gpt-5.4",
            Self::AnthropicMessages => "claude-sonnet-4-6",
            Self::GoogleGemini => "gemini-4-carbon",
        }
    }

    #[must_use]
    pub const fn default_fast_model(self) -> &'static str {
        match self {
            Self::AgyAcp => "gemini-4-hydrogen-low",
            Self::CodexAppServer => "gpt-5-mini",
            Self::GrokAcp => "grok-code-fast-1",
            Self::OpenAiResponses => "gpt-5-mini",
            Self::AnthropicMessages => "claude-haiku-4-5",
            Self::GoogleGemini => "gemini-3.8-flash",
        }
    }

    #[must_use]
    pub const fn default_title_model(self) -> &'static str {
        self.default_fast_model()
    }
}

/// Declarative capability profile governing how Liftgate optimizes and forwards turns to a provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub family: ProviderFamily,
    pub wire_protocol: UpstreamWireProtocol,
    pub tool_wire_mode: ToolWireMode,
    pub tool_dispatch: ToolDispatchStyle,
    pub cache_semantics: PromptCacheSemantics,
    /// Hard single-turn prompt text ceiling (`Some(180_224)` on `agy acp`, `None` on native HTTP APIs).
    pub single_turn_input_limit_bytes: Option<NonZeroUsize>,
    /// Single tool-output truncation watermark (`Some(9_600)` on `agy acp`, `None` on native HTTP APIs).
    pub tool_result_spill_limit_bytes: Option<NonZeroUsize>,
    /// Serialized tool-definition byte budget before engaging 3-tier virtualization.
    pub eager_tool_wire_budget_bytes: Option<NonZeroUsize>,
    /// Whether upstream natively accepts freeform Custom Tools (`exec`, `apply_patch`).
    pub supports_custom_freeform_tools: bool,
    /// Whether upstream natively provides server-side web search grounding.
    pub supports_native_web_search: bool,
    /// Whether upstream natively accepts inline multimodal image inputs without vision offload.
    pub supports_vision_inline: bool,
    /// Default uncompacted context window size in tokens when not overridden per model.
    pub default_context_window_tokens: u64,
}

impl ProviderCapabilities {
    #[must_use]
    pub const fn for_driver(driver: ProviderDriverKind) -> Self {
        match driver {
            ProviderDriverKind::AgyAcp => Self {
                family: ProviderFamily::StatefulSession,
                wire_protocol: UpstreamWireProtocol::AcpSession,
                tool_wire_mode: ToolWireMode::AcpLoopbackMcp,
                tool_dispatch: ToolDispatchStyle::SerialHighCost,
                cache_semantics: PromptCacheSemantics::ImplicitPrefix,
                single_turn_input_limit_bytes: NonZeroUsize::new(MAX_ACP_PROMPT_TEXT_BYTES),
                tool_result_spill_limit_bytes: NonZeroUsize::new(AGY_ACP_TOOL_RESULT_SPILL_BYTES),
                eager_tool_wire_budget_bytes: NonZeroUsize::new(AGY_ACP_EAGER_TOOL_BUDGET_BYTES),
                supports_custom_freeform_tools: false,
                supports_native_web_search: true,
                supports_vision_inline: true,
                default_context_window_tokens: 500_000,
            },
            ProviderDriverKind::CodexAppServer => Self {
                family: ProviderFamily::StatefulSession,
                wire_protocol: UpstreamWireProtocol::AppServerSession,
                tool_wire_mode: ToolWireMode::AcpLoopbackMcp,
                tool_dispatch: ToolDispatchStyle::NativeParallel,
                cache_semantics: PromptCacheSemantics::ImplicitPrefix,
                single_turn_input_limit_bytes: None,
                tool_result_spill_limit_bytes: None,
                eager_tool_wire_budget_bytes: NonZeroUsize::new(NATIVE_API_EAGER_TOOL_BUDGET_BYTES),
                supports_custom_freeform_tools: true,
                supports_native_web_search: true,
                supports_vision_inline: true,
                default_context_window_tokens: 400_000,
            },
            ProviderDriverKind::GrokAcp => Self {
                family: ProviderFamily::StatefulSession,
                wire_protocol: UpstreamWireProtocol::AcpSession,
                tool_wire_mode: ToolWireMode::AcpLoopbackMcp,
                tool_dispatch: ToolDispatchStyle::NativeParallel,
                cache_semantics: PromptCacheSemantics::ImplicitPrefix,
                single_turn_input_limit_bytes: None,
                tool_result_spill_limit_bytes: None,
                eager_tool_wire_budget_bytes: NonZeroUsize::new(NATIVE_API_EAGER_TOOL_BUDGET_BYTES),
                supports_custom_freeform_tools: false,
                supports_native_web_search: true,
                supports_vision_inline: true,
                default_context_window_tokens: 256_000,
            },
            ProviderDriverKind::OpenAiResponses => Self {
                family: ProviderFamily::StatelessHttp,
                wire_protocol: UpstreamWireProtocol::OpenAiResponses,
                tool_wire_mode: ToolWireMode::NativeApiTools,
                tool_dispatch: ToolDispatchStyle::NativeParallel,
                cache_semantics: PromptCacheSemantics::ImplicitPrefix,
                single_turn_input_limit_bytes: None,
                tool_result_spill_limit_bytes: None,
                eager_tool_wire_budget_bytes: NonZeroUsize::new(NATIVE_API_EAGER_TOOL_BUDGET_BYTES),
                supports_custom_freeform_tools: true,
                supports_native_web_search: true,
                supports_vision_inline: true,
                default_context_window_tokens: 400_000,
            },
            ProviderDriverKind::AnthropicMessages => Self {
                family: ProviderFamily::StatelessHttp,
                wire_protocol: UpstreamWireProtocol::AnthropicMessages,
                tool_wire_mode: ToolWireMode::NativeApiTools,
                tool_dispatch: ToolDispatchStyle::NativeParallel,
                cache_semantics: PromptCacheSemantics::ExplicitBreakpoints,
                single_turn_input_limit_bytes: None,
                tool_result_spill_limit_bytes: None,
                eager_tool_wire_budget_bytes: NonZeroUsize::new(NATIVE_API_EAGER_TOOL_BUDGET_BYTES),
                supports_custom_freeform_tools: false,
                supports_native_web_search: true,
                supports_vision_inline: true,
                default_context_window_tokens: 200_000,
            },
            ProviderDriverKind::GoogleGemini => Self {
                family: ProviderFamily::StatelessHttp,
                wire_protocol: UpstreamWireProtocol::GoogleGenerateContent,
                tool_wire_mode: ToolWireMode::NativeApiTools,
                tool_dispatch: ToolDispatchStyle::NativeParallel,
                cache_semantics: PromptCacheSemantics::ImplicitPrefix,
                single_turn_input_limit_bytes: None,
                tool_result_spill_limit_bytes: None,
                eager_tool_wire_budget_bytes: NonZeroUsize::new(NATIVE_API_EAGER_TOOL_BUDGET_BYTES),
                supports_custom_freeform_tools: false,
                supports_native_web_search: true,
                supports_vision_inline: true,
                default_context_window_tokens: 1_048_576,
            },
        }
    }

    /// Returns `true` when the ingress wire protocol matches the upstream native protocol.
    #[must_use]
    pub const fn is_isomorphic_ingress(&self, ingress: TaskProtocol) -> bool {
        matches!(
            (ingress, self.wire_protocol),
            (
                TaskProtocol::OpenaiResponses,
                UpstreamWireProtocol::OpenAiResponses
            ) | (
                TaskProtocol::AnthropicMessages,
                UpstreamWireProtocol::AnthropicMessages
            )
        )
    }

    /// Effective tool-result spill byte threshold after applying `policy`.
    #[must_use]
    pub const fn effective_spill_limit(&self, policy: &OptimizationPolicy) -> Option<NonZeroUsize> {
        match policy.spill_truncation {
            Some(false) => None,
            Some(true) => match self.tool_result_spill_limit_bytes {
                Some(limit) => Some(limit),
                None => NonZeroUsize::new(AGY_ACP_TOOL_RESULT_SPILL_BYTES),
            },
            None => {
                if matches!(policy.mode(), OptimizationMode::Passthrough) {
                    None
                } else {
                    self.tool_result_spill_limit_bytes
                }
            }
        }
    }
}

/// High-level optimization preset for Harness context & tool transformations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptimizationMode {
    /// Full provider-aware optimization (prompt distillation + adaptive tool virtualization + Code Mode self-heal).
    #[default]
    Optimized,
    /// Pure observe-only passthrough: zero prompt distillation and zero tool virtualization while capturing full telemetry.
    Passthrough,
    /// Granular custom combination of individual optimization stages.
    Custom,
}

impl OptimizationMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Optimized => "optimized",
            Self::Passthrough => "passthrough",
            Self::Custom => "custom",
        }
    }

    #[must_use]
    pub fn from_slug(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "optimized" | "opt" | "full" | "default" => Some(Self::Optimized),
            "passthrough" | "observe" | "observe-only" | "observe_only" | "raw" | "off" => {
                Some(Self::Passthrough)
            }
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }
}

const fn default_true() -> bool {
    true
}

/// Fine-grained optimization switches governing prompt distillation, tool virtualization, and Code Mode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptimizationPolicy {
    #[serde(default = "default_true")]
    pub prompt_distillation: bool,
    #[serde(default = "default_true")]
    pub tool_virtualization: bool,
    #[serde(default = "default_true")]
    pub code_mode_enhancements: bool,
    #[serde(default = "default_true")]
    pub strip_billing_noise: bool,
    /// `None` = follow `ProviderCapabilities` (`Some` on `agy acp`, `None` on native Platform APIs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spill_truncation: Option<bool>,
}

impl Default for OptimizationPolicy {
    fn default() -> Self {
        Self::optimized()
    }
}

impl OptimizationPolicy {
    #[must_use]
    pub const fn optimized() -> Self {
        Self {
            prompt_distillation: true,
            tool_virtualization: true,
            code_mode_enhancements: true,
            strip_billing_noise: true,
            spill_truncation: None,
        }
    }

    #[must_use]
    pub const fn passthrough() -> Self {
        Self {
            prompt_distillation: false,
            tool_virtualization: false,
            code_mode_enhancements: false,
            strip_billing_noise: false,
            spill_truncation: Some(false),
        }
    }

    #[must_use]
    pub fn from_mode(mode: OptimizationMode) -> Self {
        match mode {
            OptimizationMode::Optimized | OptimizationMode::Custom => Self::optimized(),
            OptimizationMode::Passthrough => Self::passthrough(),
        }
    }

    #[must_use]
    pub fn from_slug(raw: &str) -> Option<Self> {
        match OptimizationMode::from_slug(raw)? {
            OptimizationMode::Optimized => Some(Self::optimized()),
            OptimizationMode::Passthrough => Some(Self::passthrough()),
            OptimizationMode::Custom => None,
        }
    }

    #[must_use]
    pub const fn mode(&self) -> OptimizationMode {
        if self.prompt_distillation
            && self.tool_virtualization
            && self.code_mode_enhancements
            && self.strip_billing_noise
            && self.spill_truncation.is_none()
        {
            OptimizationMode::Optimized
        } else if !self.prompt_distillation
            && !self.tool_virtualization
            && !self.code_mode_enhancements
            && !self.strip_billing_noise
            && matches!(self.spill_truncation, Some(false) | None)
        {
            OptimizationMode::Passthrough
        } else {
            OptimizationMode::Custom
        }
    }
}

/// Credential binding policy for a provider profile (never embeds plaintext keys in `providers.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApiKeySource {
    /// Uses CLI session auth (e.g. `agy login` or `codex login`) or unauthenticated local endpoint.
    #[default]
    None,
    /// Reads API key at request time from the named environment variable.
    Env { var: String },
    /// Reads API key from Liftgate's mode-0600 `credentials.json` vault keyed by `ProviderId`.
    Stored,
}

impl ApiKeySource {
    #[must_use]
    pub fn summary_label(&self) -> String {
        match self {
            Self::None => "cli/none".to_owned(),
            Self::Env { var } => format!("env:{var}"),
            Self::Stored => "stored:vault".to_owned(),
        }
    }
}

/// Model descriptor carried by a provider profile with authentic context window limits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModelEntry {
    pub id: String,
    pub display_name: String,
    pub context_window_tokens: u64,
    pub auto_compact_token_limit: u64,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u64,
    #[serde(default = "default_true")]
    pub supports_reasoning: bool,
    #[serde(default = "default_true")]
    pub supports_vision: bool,
}

const fn default_max_output_tokens() -> u64 {
    32_768
}

/// Returns the built-in curated model catalog for `driver`.
#[must_use]
pub fn builtin_models_for_driver(driver: ProviderDriverKind) -> Vec<ProviderModelEntry> {
    match driver {
        ProviderDriverKind::AgyAcp => vec![
            ProviderModelEntry {
                id: "gemini-4-carbon-high".to_owned(),
                display_name: "Gemini 4 Carbon (High)".to_owned(),
                context_window_tokens: 500_000,
                auto_compact_token_limit: 425_000,
                max_output_tokens: 65_536,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "gemini-4-neon-high".to_owned(),
                display_name: "Gemini 4 Neon (High)".to_owned(),
                context_window_tokens: 500_000,
                auto_compact_token_limit: 425_000,
                max_output_tokens: 65_536,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "gemini-4-hydrogen-low".to_owned(),
                display_name: "Gemini 4 Hydrogen (Low)".to_owned(),
                context_window_tokens: 500_000,
                auto_compact_token_limit: 425_000,
                max_output_tokens: 32_768,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "claude-opus-5-5-high".to_owned(),
                display_name: "Claude Opus 5.5 (High)".to_owned(),
                context_window_tokens: 200_000,
                auto_compact_token_limit: 170_000,
                max_output_tokens: 32_768,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "claude-sonnet-5-5-high".to_owned(),
                display_name: "Claude Sonnet 5.5 (High)".to_owned(),
                context_window_tokens: 200_000,
                auto_compact_token_limit: 170_000,
                max_output_tokens: 32_768,
                supports_reasoning: true,
                supports_vision: true,
            },
        ],
        ProviderDriverKind::OpenAiResponses => vec![
            ProviderModelEntry {
                id: "gpt-5.4".to_owned(),
                display_name: "GPT-5.4".to_owned(),
                context_window_tokens: 400_000,
                auto_compact_token_limit: 272_000,
                max_output_tokens: 128_000,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "gpt-5.3-codex".to_owned(),
                display_name: "GPT-5.3 Codex".to_owned(),
                context_window_tokens: 400_000,
                auto_compact_token_limit: 272_000,
                max_output_tokens: 128_000,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "gpt-5-mini".to_owned(),
                display_name: "GPT-5 Mini".to_owned(),
                context_window_tokens: 400_000,
                auto_compact_token_limit: 272_000,
                max_output_tokens: 65_536,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "o4-mini".to_owned(),
                display_name: "o4-mini".to_owned(),
                context_window_tokens: 200_000,
                auto_compact_token_limit: 170_000,
                max_output_tokens: 100_000,
                supports_reasoning: true,
                supports_vision: true,
            },
        ],
        ProviderDriverKind::AnthropicMessages => vec![
            ProviderModelEntry {
                id: "claude-opus-4-6".to_owned(),
                display_name: "Claude Opus 4.6".to_owned(),
                context_window_tokens: 200_000,
                auto_compact_token_limit: 170_000,
                max_output_tokens: 64_000,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "claude-sonnet-4-6".to_owned(),
                display_name: "Claude Sonnet 4.6".to_owned(),
                context_window_tokens: 200_000,
                auto_compact_token_limit: 170_000,
                max_output_tokens: 64_000,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "claude-haiku-4-5".to_owned(),
                display_name: "Claude Haiku 4.5".to_owned(),
                context_window_tokens: 200_000,
                auto_compact_token_limit: 170_000,
                max_output_tokens: 32_768,
                supports_reasoning: true,
                supports_vision: true,
            },
        ],
        ProviderDriverKind::GoogleGemini => vec![
            ProviderModelEntry {
                id: "gemini-4-carbon".to_owned(),
                display_name: "Gemini 4 Carbon".to_owned(),
                context_window_tokens: 1_048_576,
                auto_compact_token_limit: 890_000,
                max_output_tokens: 65_536,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "gemini-4-argon".to_owned(),
                display_name: "Gemini 4 Argon".to_owned(),
                context_window_tokens: 1_048_576,
                auto_compact_token_limit: 890_000,
                max_output_tokens: 65_536,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "gemini-3.8-flash".to_owned(),
                display_name: "Gemini 3.8 Flash".to_owned(),
                context_window_tokens: 1_048_576,
                auto_compact_token_limit: 890_000,
                max_output_tokens: 65_536,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "gemini-3.5-flash-lite".to_owned(),
                display_name: "Gemini 3.5 Flash-Lite".to_owned(),
                context_window_tokens: 1_048_576,
                auto_compact_token_limit: 890_000,
                max_output_tokens: 65_536,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "gemini-3.1-pro-preview".to_owned(),
                display_name: "Gemini 3.1 Pro Preview".to_owned(),
                context_window_tokens: 1_048_576,
                auto_compact_token_limit: 890_000,
                max_output_tokens: 65_536,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "gemini-3-flash-preview".to_owned(),
                display_name: "Gemini 3 Flash Preview".to_owned(),
                context_window_tokens: 1_048_576,
                auto_compact_token_limit: 890_000,
                max_output_tokens: 65_536,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "gemini-2.5-pro".to_owned(),
                display_name: "Gemini 2.5 Pro".to_owned(),
                context_window_tokens: 1_048_576,
                auto_compact_token_limit: 890_000,
                max_output_tokens: 65_536,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "gemini-2.5-flash".to_owned(),
                display_name: "Gemini 2.5 Flash".to_owned(),
                context_window_tokens: 1_048_576,
                auto_compact_token_limit: 890_000,
                max_output_tokens: 65_536,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "gemini-2.5-flash-lite".to_owned(),
                display_name: "Gemini 2.5 Flash-Lite".to_owned(),
                context_window_tokens: 1_048_576,
                auto_compact_token_limit: 890_000,
                max_output_tokens: 32_768,
                supports_reasoning: true,
                supports_vision: true,
            },
        ],
        ProviderDriverKind::CodexAppServer => vec![
            ProviderModelEntry {
                id: "gpt-5.3-codex".to_owned(),
                display_name: "GPT-5.3 Codex (App-Server)".to_owned(),
                context_window_tokens: 400_000,
                auto_compact_token_limit: 272_000,
                max_output_tokens: 128_000,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "gpt-5.4".to_owned(),
                display_name: "GPT-5.4 (App-Server)".to_owned(),
                context_window_tokens: 400_000,
                auto_compact_token_limit: 272_000,
                max_output_tokens: 128_000,
                supports_reasoning: true,
                supports_vision: true,
            },
        ],
        ProviderDriverKind::GrokAcp => vec![
            ProviderModelEntry {
                id: "grok-4".to_owned(),
                display_name: "Grok 4 (ACP)".to_owned(),
                context_window_tokens: 256_000,
                auto_compact_token_limit: 215_000,
                max_output_tokens: 65_536,
                supports_reasoning: true,
                supports_vision: true,
            },
            ProviderModelEntry {
                id: "grok-code-fast-1".to_owned(),
                display_name: "Grok Code Fast 1 (ACP)".to_owned(),
                context_window_tokens: 256_000,
                auto_compact_token_limit: 215_000,
                max_output_tokens: 65_536,
                supports_reasoning: true,
                supports_vision: false,
            },
        ],
    }
}

/// Persisted configuration for a single upstream provider instance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProfileConfig {
    pub id: ProviderId,
    pub driver: ProviderDriverKind,
    #[serde(default)]
    pub display_name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command_args: Vec<String>,
    #[serde(default)]
    pub auth: ApiKeySource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyUrl>,
    #[serde(default)]
    pub force_direct_proxy: bool,
    pub default_model: ModelId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_model: Option<ModelId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_model: Option<ModelId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optimization: Option<OptimizationPolicy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_models: Vec<ProviderModelEntry>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl ProviderProfileConfig {
    /// Constructs the default profile for a built-in driver and provider ID.
    #[must_use]
    pub fn builtin(id: ProviderId, driver: ProviderDriverKind) -> Self {
        let default_model = ModelId::try_new(driver.default_primary_model())
            .unwrap_or_else(|_| ModelId::fallback_primary());
        let fast_model = ModelId::try_new(driver.default_fast_model()).ok();
        let title_model = ModelId::try_new(driver.default_title_model()).ok();
        let auth = match driver.default_api_key_env() {
            Some(var) => ApiKeySource::Env {
                var: var.to_owned(),
            },
            None => ApiKeySource::None,
        };
        Self {
            id,
            driver,
            display_name: driver.display_name().to_owned(),
            enabled: true,
            endpoint: driver.default_endpoint().map(str::to_owned),
            command: None,
            command_args: Vec::new(),
            auth,
            proxy: None,
            force_direct_proxy: false,
            default_model,
            fast_model,
            title_model,
            optimization: None,
            custom_models: Vec::new(),
            extra: BTreeMap::new(),
        }
    }

    #[must_use]
    pub const fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::for_driver(self.driver)
    }

    /// Returns the merged model catalog (custom entries override built-in entries of the same ID).
    #[must_use]
    pub fn effective_models(&self) -> Vec<ProviderModelEntry> {
        let mut out = self.custom_models.clone();
        for builtin in builtin_models_for_driver(self.driver) {
            if !out.iter().any(|m| m.id.eq_ignore_ascii_case(&builtin.id)) {
                out.push(builtin);
            }
        }
        if let Some(pos) = out
            .iter()
            .position(|m| m.id.eq_ignore_ascii_case(self.default_model.as_str()))
        {
            if pos > 0 {
                let default_entry = out.remove(pos);
                out.insert(0, default_entry);
            }
        } else {
            let (win, compact) =
                context_limits_for_driver(self.default_model.as_str(), self.driver);
            out.insert(
                0,
                ProviderModelEntry {
                    id: self.default_model.as_str().to_owned(),
                    display_name: self.default_model.as_str().to_owned(),
                    context_window_tokens: win,
                    auto_compact_token_limit: compact,
                    max_output_tokens: 65_536,
                    supports_reasoning: true,
                    supports_vision: true,
                },
            );
        }
        out
    }

    #[must_use]
    pub fn effective_endpoint(&self) -> Option<&str> {
        self.endpoint
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| self.driver.default_endpoint())
    }
}

/// Returns built-in provider profiles shipped out-of-the-box.
#[must_use]
pub fn builtin_provider_profiles() -> Vec<ProviderProfileConfig> {
    vec![
        ProviderProfileConfig::builtin(ProviderId::agy(), ProviderDriverKind::AgyAcp),
        ProviderProfileConfig::builtin(ProviderId::openai(), ProviderDriverKind::OpenAiResponses),
        ProviderProfileConfig::builtin(
            ProviderId::anthropic(),
            ProviderDriverKind::AnthropicMessages,
        ),
        ProviderProfileConfig::builtin(ProviderId::gemini(), ProviderDriverKind::GoogleGemini),
        ProviderProfileConfig::builtin(
            ProviderId::codex_app_server(),
            ProviderDriverKind::CodexAppServer,
        ),
        ProviderProfileConfig::builtin(ProviderId::grok_acp(), ProviderDriverKind::GrokAcp),
    ]
}

/// Top-level multi-provider configuration persisted in `~/.liftgate/providers.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRegistryConfig {
    #[serde(default)]
    pub active_provider: ProviderId,
    #[serde(default)]
    pub optimization: OptimizationPolicy,
    #[serde(default)]
    pub providers: Vec<ProviderProfileConfig>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Default for ProviderRegistryConfig {
    fn default() -> Self {
        Self {
            active_provider: ProviderId::agy(),
            optimization: OptimizationPolicy::optimized(),
            providers: builtin_provider_profiles(),
            extra: BTreeMap::new(),
        }
    }
}

impl ProviderRegistryConfig {
    /// Ensures all built-in providers are present while preserving user customizations and order.
    pub fn normalized(mut self) -> Self {
        for builtin in builtin_provider_profiles() {
            if !self.providers.iter().any(|p| p.id == builtin.id) {
                self.providers.push(builtin);
            }
        }
        if !self.providers.iter().any(|p| p.id == self.active_provider) {
            self.active_provider = ProviderId::agy();
        }
        self
    }

    #[must_use]
    pub fn get(&self, id: &ProviderId) -> Option<&ProviderProfileConfig> {
        self.providers.iter().find(|p| &p.id == id)
    }

    pub fn get_mut(&mut self, id: &ProviderId) -> Option<&mut ProviderProfileConfig> {
        self.providers.iter_mut().find(|p| &p.id == id)
    }

    #[must_use]
    pub fn active(&self) -> Option<&ProviderProfileConfig> {
        self.get(&self.active_provider)
    }

    pub fn upsert(&mut self, profile: ProviderProfileConfig) {
        if let Some(existing) = self.get_mut(&profile.id) {
            *existing = profile;
        } else {
            self.providers.push(profile);
        }
    }
}

/// Resolves `(context_window_tokens, auto_compact_token_limit)` tailored to `driver` and `model_id`.
#[must_use]
pub fn context_limits_for_driver(model_id: &str, driver: ProviderDriverKind) -> (u64, u64) {
    let lower = model_id.trim().to_ascii_lowercase();
    if lower.contains("[1m]") || lower.ends_with("-1m") {
        return (1_000_000, 850_000);
    }
    match driver {
        ProviderDriverKind::AgyAcp => crate::model::context_limits(model_id),
        ProviderDriverKind::OpenAiResponses | ProviderDriverKind::CodexAppServer => {
            if lower.starts_with("o3") || lower.starts_with("o4") || lower.starts_with("gpt-4.1") {
                (200_000, 170_000)
            } else {
                (400_000, 272_000)
            }
        }
        ProviderDriverKind::AnthropicMessages => (200_000, 170_000),
        ProviderDriverKind::GoogleGemini => (1_048_576, 890_000),
        ProviderDriverKind::GrokAcp => (256_000, 215_000),
    }
}

/// Parses a URL route provider specifier (`<provider_id>` or `<provider_id>@<opt_mode>`).
#[must_use]
pub fn parse_route_provider_spec(
    raw_spec: &str,
) -> Option<(ProviderId, Option<OptimizationPolicy>)> {
    let trimmed = raw_spec.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((prov_part, opt_part)) = trimmed.split_once('@') {
        let id = ProviderId::try_new(prov_part).ok()?;
        let opt = OptimizationPolicy::from_slug(opt_part)?;
        Some((id, Some(opt)))
    } else {
        let id = ProviderId::try_new(trimmed).ok()?;
        Some((id, None))
    }
}

/// Builds clean, unprefixed `<api_gateway_bridge_rules>` for native Platform HTTP APIs when
/// native tool virtualization or Code Mode enhancements are active.
#[must_use]
pub fn build_native_api_bridge_rules(
    capabilities: &ProviderCapabilities,
    orchestrator: Option<McpOrchestratorContract<'_>>,
    virtualized_hubs: &[(String, String, usize)],
    has_virtualized_dispatch: bool,
    has_gateway_web_fetch: bool,
) -> Option<String> {
    if !has_virtualized_dispatch && orchestrator.is_none() && !has_gateway_web_fetch {
        return None;
    }
    let mut out = String::with_capacity(768);
    out.push_str("<api_gateway_bridge_rules>\n");
    if matches!(
        capabilities.tool_dispatch,
        ToolDispatchStyle::NativeParallel
    ) {
        out.push_str(
            "- Native parallel tool calls are supported: emit independent read, search, or check tool calls in parallel in the same turn whenever safe.\n",
        );
    }
    if let Some(contract) = orchestrator {
        match contract {
            McpOrchestratorContract::CodexExec {
                mcp_name,
                has_apply_patch,
                has_update_plan,
            } => {
                out.push_str(&format!(
                    "- Codex Code Mode (`{mcp_name}`): pass raw JavaScript in `{{\"code\": \"...\"}}` (no markdown fences; bare V8 isolate with `btoa`/`atob`, no Node `Buffer`/`require`/`fs`/`process`/`console`), fan out reads via `await Promise.all([tools.exec_command(...), ...])`, and emit concise output via `text(...)`.\n"
                ));
                if has_apply_patch {
                    out.push_str(
                        "- File edits (`apply_patch`): call top-level `apply_patch` directly for 1-4 file surgical V4A patches (`*** Begin Patch ... *** End Patch`) after reading target lines.\n",
                    );
                }
                if has_update_plan {
                    out.push_str(
                        "- Multi-step tracking: maintain live checklist progress via `update_plan` (at most one `in_progress` step) when executing 3+ step tasks outside Plan Mode.\n",
                    );
                }
            }
            McpOrchestratorContract::ClaudeBashFirst { bash_mcp_name } => {
                out.push_str(&format!(
                    "- Compound shell work (`{bash_mcp_name}`): chain dependent shell checks with `&&` while issuing independent file `Read`/`Grep`/`Glob` calls in parallel.\n"
                ));
            }
            McpOrchestratorContract::OpenCodeExecute { mcp_name, .. } => {
                out.push_str(&format!(
                    "- OpenCode Code Mode (`{mcp_name}`): pass raw JavaScript without markdown fences to orchestrate multi-step workspace queries cleanly.\n"
                ));
            }
            McpOrchestratorContract::OmpPolyglotEval { mcp_name, .. } => {
                out.push_str(&format!(
                    "- OMP Polyglot Code Mode (`{mcp_name}`): invoke folded session tools inside `{mcp_name}` via `await tool.<name>(args)`.\n"
                ));
            }
        }
    }
    if has_virtualized_dispatch {
        if !virtualized_hubs.is_empty() {
            let hubs: Vec<String> = virtualized_hubs
                .iter()
                .map(|(hub_name, ns, count)| format!("`{hub_name}` ({ns}: {count} actions)"))
                .collect();
            out.push_str(&format!(
                "- Virtualized client tool hubs: {}.\n",
                hubs.join(", ")
            ));
        }
        out.push_str(
            "- Virtualized tool discovery & dispatch: call `search_client_tools` first (0-RTT gateway-resolved) to inspect exact action names and JSON `inputSchema`s, then invoke grouped actions via `ns__<namespace>` (`{\"action\": \"<action>\", \"args\": {{ ... }}}}`) or long-tail tools via `call_client_tool` (`{\"name\": \"<exact_tool_name>\", \"arguments\": {{ ... }}}}`).\n",
        );
    }
    if has_gateway_web_fetch {
        out.push_str(
            "- Server-managed URL fetch: invoke `gateway_web_fetch` (`{\"url\": \"https://...\", \"pattern\"?: \"...\"}}`) to retrieve readable page content.\n",
        );
    }
    out.push_str("</api_gateway_bridge_rules>");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_id_validates_and_normalizes_aliases() {
        assert_eq!(
            ProviderId::try_new("antigravity").unwrap(),
            ProviderId::agy()
        );
        assert_eq!(
            ProviderId::try_new("OpenAI-Platform").unwrap(),
            ProviderId::openai()
        );
        assert_eq!(
            ProviderId::try_new("claude-api").unwrap(),
            ProviderId::anthropic()
        );
        assert_eq!(ProviderId::try_new("google").unwrap(), ProviderId::gemini());
        assert_eq!(
            ProviderId::try_new("my-openrouter_1").unwrap().as_str(),
            "my-openrouter_1"
        );
        assert!(ProviderId::try_new("").is_err());
        assert!(ProviderId::try_new("-bad").is_err());
        assert!(ProviderId::try_new("summarize repo").is_err());
    }

    #[test]
    fn capabilities_differentiate_stateful_agy_from_stateless_native_apis() {
        let agy = ProviderCapabilities::for_driver(ProviderDriverKind::AgyAcp);
        assert_eq!(agy.family, ProviderFamily::StatefulSession);
        assert!(agy.single_turn_input_limit_bytes.is_some());
        assert!(agy.tool_result_spill_limit_bytes.is_some());
        assert_eq!(agy.tool_wire_mode, ToolWireMode::AcpLoopbackMcp);

        let oai = ProviderCapabilities::for_driver(ProviderDriverKind::OpenAiResponses);
        assert_eq!(oai.family, ProviderFamily::StatelessHttp);
        assert!(oai.single_turn_input_limit_bytes.is_none());
        assert!(oai.tool_result_spill_limit_bytes.is_none());
        assert_eq!(oai.tool_wire_mode, ToolWireMode::NativeApiTools);
        assert!(oai.supports_custom_freeform_tools);
        assert!(oai.is_isomorphic_ingress(TaskProtocol::OpenaiResponses));
        assert!(!oai.is_isomorphic_ingress(TaskProtocol::AnthropicMessages));

        let anth = ProviderCapabilities::for_driver(ProviderDriverKind::AnthropicMessages);
        assert_eq!(
            anth.cache_semantics,
            PromptCacheSemantics::ExplicitBreakpoints
        );
        assert!(anth.is_isomorphic_ingress(TaskProtocol::AnthropicMessages));
    }

    #[test]
    fn route_spec_parses_provider_and_optional_optimization_mode() {
        let (id, opt) = parse_route_provider_spec("openai@passthrough").unwrap();
        assert_eq!(id, ProviderId::openai());
        assert_eq!(opt.unwrap().mode(), OptimizationMode::Passthrough);

        let (id2, opt2) = parse_route_provider_spec("anthropic").unwrap();
        assert_eq!(id2, ProviderId::anthropic());
        assert!(opt2.is_none());
    }

    #[test]
    fn effective_models_places_configured_default_model_first() {
        let anth = ProviderProfileConfig::builtin(
            ProviderId::anthropic(),
            ProviderDriverKind::AnthropicMessages,
        );
        assert_eq!(anth.default_model.as_str(), "claude-sonnet-4-6");
        let anth_models = anth.effective_models();
        assert_eq!(anth_models[0].id, "claude-sonnet-4-6");
        assert!(anth_models.iter().any(|m| m.id == "claude-opus-4-6"));

        let mut oai = ProviderProfileConfig::builtin(
            ProviderId::openai(),
            ProviderDriverKind::OpenAiResponses,
        );
        oai.default_model = ModelId::try_new("gpt-5-mini").unwrap();
        let oai_models = oai.effective_models();
        assert_eq!(oai_models[0].id, "gpt-5-mini");
        assert!(oai_models.iter().any(|m| m.id == "gpt-5.4"));
    }
}
