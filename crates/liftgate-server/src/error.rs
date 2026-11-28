use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use liftgate_types::{
    anthropic::{AnthropicErrorBody, AnthropicErrorEnvelope},
    error::{ProtocolError, format_error_chain},
    openai::{OpenAiErrorBody, OpenAiErrorEnvelope},
};
use thiserror::Error;
use tokio_util::codec::LinesCodecError;

use crate::job::ProcessGroupError;

/// Errors originating from ACP subprocess supervision or JSON-RPC transport.
#[derive(Debug, Error)]
pub enum AcpError {
    #[error("upstream ACP model `{model}` context window exceeded: {detail}")]
    ContextLengthExceeded { model: String, detail: String },
    #[error("failed to spawn ACP subprocess `{}`", path.display())]
    SpawnProcess {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to attach ACP subprocess to OS process group / Job Object")]
    ProcessGroup {
        #[from]
        source: ProcessGroupError,
    },

    #[error("ACP subprocess missing `{stream}` pipe")]
    MissingStdioPipe { stream: &'static str },

    #[error("ACP stdio transport closed unexpectedly")]
    TransportClosed,

    #[error("ACP stdout line framing failed")]
    LineCodec {
        #[from]
        source: LinesCodecError,
    },

    #[error("failed to serialize outbound ACP JSON-RPC payload")]
    EncodeJson {
        #[source]
        source: serde_json::Error,
    },

    #[error("failed to decode ACP `{method}` JSON-RPC response")]
    DecodeJson {
        method: &'static str,
        #[source]
        source: serde_json::Error,
    },

    #[error("ACP `{method}` returned JSON-RPC error ({code}): {message}")]
    Rpc {
        method: &'static str,
        code: i64,
        message: String,
    },

    #[error("ACP prompt turn timed out after {seconds}s")]
    TurnTimeout { seconds: u64 },

    #[error("upstream ACP model `{model}` rate-limited or capacity exhausted: {detail}")]
    UpstreamRateLimited { model: String, detail: String },

    #[error("upstream Antigravity quota exhausted on `{model}`: {detail}")]
    UpstreamQuotaExhausted { model: String, detail: String },

    #[error("upstream ACP agent terminated with fatal error: {detail}")]
    UpstreamAgentTerminated { detail: String },

    #[error(
        "upstream stream truncated or corrupted in-progress `{tool}` tool call before closing tag"
    )]
    CorruptedToolCallStream { tool: String },

    #[error("upstream stream ended before closing active `{protocol}` tool call")]
    UnclosedToolCallStream { protocol: &'static str },

    #[error(
        "upstream ACP model `{model}` returned an empty response (stop_reason={stop_reason}){}",
        format_optional_detail(detail.as_deref())
    )]
    EmptyUpstreamResponse {
        model: String,
        stop_reason: String,
        detail: Option<String>,
    },

    #[error("ACP prompt turn was cancelled")]
    Cancelled,
}

fn format_optional_detail(detail: Option<&str>) -> String {
    match detail.map(str::trim).filter(|s| !s.is_empty()) {
        Some(text) => format!(": {text}"),
        None => String::new(),
    }
}

/// Structured validation and transport failures when reading an HTTP JSON request body.
#[derive(Debug, Error)]
pub enum RequestBodyError {
    #[error("invalid Content-Length header")]
    InvalidContentLength,

    #[error("timed out reading HTTP request body")]
    ReadTimeout,

    #[error("failed to read HTTP request body")]
    ReadFrame {
        #[source]
        source: axum::Error,
    },

    #[error("HTTP request body exceeded declared Content-Length")]
    ExceededContentLength,

    #[error("HTTP request body length did not match declared Content-Length")]
    ContentLengthMismatch,

    #[error("malformed JSON request body")]
    MalformedJson {
        #[from]
        source: serde_json::Error,
    },
}

/// Top-level gateway request error mapped cleanly at HTTP boundaries.
#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("invalid or missing gateway API key")]
    Unauthorized,

    #[error("request payload exceeds maximum allowed size ({max_bytes} bytes)")]
    PayloadTooLarge { max_bytes: usize },

    #[error("requested resource `{resource}` was not found")]
    NotFound { resource: String },

    #[error("provider `{provider_id}` is disabled")]
    ProviderDisabled { provider_id: String },

    #[error("invalid HTTP request body")]
    InvalidBody {
        #[from]
        source: RequestBodyError,
    },

    #[error("protocol normalization error")]
    Protocol {
        #[from]
        source: ProtocolError,
    },

    #[error("upstream ACP execution error")]
    Acp {
        #[from]
        source: AcpError,
    },

    #[error("upstream ACP stream execution error")]
    SharedAcp {
        #[source]
        source: Arc<AcpError>,
    },
}

impl GatewayError {
    #[must_use]
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::PayloadTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::NotFound { .. } => StatusCode::NOT_FOUND,
            Self::ProviderDisabled { .. } | Self::InvalidBody { .. } | Self::Protocol { .. } => {
                StatusCode::BAD_REQUEST
            }
            Self::Acp { source } => Self::acp_status(source),
            Self::SharedAcp { source } => Self::acp_status(source),
        }
    }

    /// HTTP status code for Anthropic `/v1/messages` responses.
    ///
    /// Exhausted account quota and empty upstream responses map to `400 Bad Request`
    /// (`invalid_request_error`) so `@anthropic-ai/sdk` and Claude Code halt immediately
    /// and display the diagnostic message rather than entering multi-attempt retry loops.
    #[must_use]
    pub fn anthropic_status_code(&self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::PayloadTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::NotFound { .. } => StatusCode::NOT_FOUND,
            Self::ProviderDisabled { .. } | Self::InvalidBody { .. } | Self::Protocol { .. } => {
                StatusCode::BAD_REQUEST
            }
            Self::Acp { source } => Self::acp_anthropic_status(source),
            Self::SharedAcp { source } => Self::acp_anthropic_status(source),
        }
    }

    const fn acp_status(source: &AcpError) -> StatusCode {
        match source {
            AcpError::ContextLengthExceeded { .. } => StatusCode::BAD_REQUEST,
            AcpError::TurnTimeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            AcpError::UpstreamRateLimited { .. } | AcpError::UpstreamQuotaExhausted { .. } => {
                StatusCode::TOO_MANY_REQUESTS
            }
            AcpError::UpstreamAgentTerminated { .. }
            | AcpError::CorruptedToolCallStream { .. }
            | AcpError::UnclosedToolCallStream { .. }
            | AcpError::EmptyUpstreamResponse { .. }
            | AcpError::Cancelled
            | AcpError::SpawnProcess { .. }
            | AcpError::ProcessGroup { .. }
            | AcpError::MissingStdioPipe { .. }
            | AcpError::TransportClosed
            | AcpError::LineCodec { .. }
            | AcpError::EncodeJson { .. }
            | AcpError::DecodeJson { .. }
            | AcpError::Rpc { .. } => StatusCode::BAD_GATEWAY,
        }
    }

    const fn acp_anthropic_status(source: &AcpError) -> StatusCode {
        match source {
            AcpError::ContextLengthExceeded { .. } => StatusCode::BAD_REQUEST,
            AcpError::TurnTimeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            AcpError::UpstreamRateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            AcpError::UpstreamQuotaExhausted { .. } | AcpError::EmptyUpstreamResponse { .. } => {
                StatusCode::BAD_REQUEST
            }
            AcpError::UpstreamAgentTerminated { .. }
            | AcpError::CorruptedToolCallStream { .. }
            | AcpError::UnclosedToolCallStream { .. }
            | AcpError::Cancelled
            | AcpError::SpawnProcess { .. }
            | AcpError::ProcessGroup { .. }
            | AcpError::MissingStdioPipe { .. }
            | AcpError::TransportClosed
            | AcpError::LineCodec { .. }
            | AcpError::EncodeJson { .. }
            | AcpError::DecodeJson { .. }
            | AcpError::Rpc { .. } => StatusCode::BAD_GATEWAY,
        }
    }

    #[must_use]
    pub fn stable_kind(&self) -> (&'static str, &'static str) {
        match self {
            Self::Unauthorized => ("authentication_error", "invalid_api_key"),
            Self::PayloadTooLarge { .. } => ("invalid_request_error", "request_too_large"),
            Self::NotFound { .. } => ("not_found_error", "not_found"),
            Self::ProviderDisabled { .. } => ("invalid_request_error", "provider_disabled"),
            Self::InvalidBody { .. } => ("invalid_request_error", "invalid_request"),
            Self::Protocol { source } => {
                if source.context_overflow_bytes().is_some() {
                    ("invalid_request_error", "context_length_exceeded")
                } else {
                    ("invalid_request_error", "invalid_request")
                }
            }
            Self::Acp { source } => Self::acp_kind(source),
            Self::SharedAcp { source } => Self::acp_kind(source),
        }
    }

    /// Anthropic wire `error.type` string for HTTP JSON envelopes and SSE `event: error` frames.
    #[must_use]
    pub fn anthropic_error_type(&self) -> &'static str {
        match self {
            Self::Unauthorized => "authentication_error",
            Self::PayloadTooLarge { .. } => "request_too_large",
            Self::NotFound { .. } => "not_found_error",
            Self::ProviderDisabled { .. } | Self::InvalidBody { .. } | Self::Protocol { .. } => {
                "invalid_request_error"
            }
            Self::Acp { source } => Self::acp_anthropic_error_type(source),
            Self::SharedAcp { source } => Self::acp_anthropic_error_type(source),
        }
    }

    const fn acp_kind(source: &AcpError) -> (&'static str, &'static str) {
        match source {
            AcpError::ContextLengthExceeded { .. } => {
                ("invalid_request_error", "context_length_exceeded")
            }
            AcpError::TurnTimeout { .. } => ("api_error", "upstream_timeout"),
            AcpError::UpstreamRateLimited { .. } => ("rate_limit_error", "rate_limit_exceeded"),
            AcpError::UpstreamQuotaExhausted { .. } => ("insufficient_quota", "insufficient_quota"),
            AcpError::UpstreamAgentTerminated { .. }
            | AcpError::CorruptedToolCallStream { .. }
            | AcpError::UnclosedToolCallStream { .. } => ("api_error", "upstream_agent_error"),
            AcpError::EmptyUpstreamResponse { .. } => ("api_error", "empty_upstream_response"),
            AcpError::Rpc { .. } => ("api_error", "acp_rpc_error"),
            AcpError::SpawnProcess { .. }
            | AcpError::ProcessGroup { .. }
            | AcpError::MissingStdioPipe { .. }
            | AcpError::TransportClosed
            | AcpError::LineCodec { .. }
            | AcpError::EncodeJson { .. }
            | AcpError::DecodeJson { .. }
            | AcpError::Cancelled => ("api_error", "acp_transport_error"),
        }
    }

    const fn acp_anthropic_error_type(source: &AcpError) -> &'static str {
        match source {
            AcpError::ContextLengthExceeded { .. } => "invalid_request_error",
            AcpError::UpstreamRateLimited { .. } => "rate_limit_error",
            AcpError::UpstreamQuotaExhausted { .. } | AcpError::EmptyUpstreamResponse { .. } => {
                "invalid_request_error"
            }
            AcpError::TurnTimeout { .. }
            | AcpError::UpstreamAgentTerminated { .. }
            | AcpError::CorruptedToolCallStream { .. }
            | AcpError::UnclosedToolCallStream { .. }
            | AcpError::Rpc { .. }
            | AcpError::SpawnProcess { .. }
            | AcpError::ProcessGroup { .. }
            | AcpError::MissingStdioPipe { .. }
            | AcpError::TransportClosed
            | AcpError::LineCodec { .. }
            | AcpError::EncodeJson { .. }
            | AcpError::DecodeJson { .. }
            | AcpError::Cancelled => "api_error",
        }
    }

    /// Returns `Some((used_tokens, max_tokens))` when this error represents a context-window
    /// overflow so OpenAI Codex (`context_length_exceeded`) and Anthropic Claude Code
    /// (`prompt is too long: N tokens > M maximum`) can trigger automatic reactive compaction.
    #[must_use]
    pub fn context_overflow_tokens(&self) -> Option<(u64, u64)> {
        match self {
            Self::Protocol { source } => {
                let bytes = source.context_overflow_bytes()?;
                let max_tokens = liftgate_types::model::GEMINI_DEFAULT_WINDOW;
                let used_tokens = ((bytes as u64) / 4).max(max_tokens + 1);
                Some((used_tokens, max_tokens))
            }
            Self::Acp { source } => Self::acp_context_overflow_tokens(source),
            Self::SharedAcp { source } => Self::acp_context_overflow_tokens(source),
            Self::Unauthorized
            | Self::PayloadTooLarge { .. }
            | Self::NotFound { .. }
            | Self::ProviderDisabled { .. }
            | Self::InvalidBody { .. } => None,
        }
    }

    fn acp_context_overflow_tokens(source: &AcpError) -> Option<(u64, u64)> {
        match source {
            AcpError::ContextLengthExceeded { model, .. } => {
                let (max_tokens, _) = liftgate_types::model::context_limits(model);
                Some((max_tokens + 1, max_tokens))
            }
            AcpError::SpawnProcess { .. }
            | AcpError::ProcessGroup { .. }
            | AcpError::MissingStdioPipe { .. }
            | AcpError::TransportClosed
            | AcpError::LineCodec { .. }
            | AcpError::EncodeJson { .. }
            | AcpError::DecodeJson { .. }
            | AcpError::Rpc { .. }
            | AcpError::TurnTimeout { .. }
            | AcpError::UpstreamRateLimited { .. }
            | AcpError::UpstreamQuotaExhausted { .. }
            | AcpError::UpstreamAgentTerminated { .. }
            | AcpError::CorruptedToolCallStream { .. }
            | AcpError::UnclosedToolCallStream { .. }
            | AcpError::EmptyUpstreamResponse { .. }
            | AcpError::Cancelled => None,
        }
    }

    #[must_use]
    pub fn openai_param(&self) -> Option<&'static str> {
        if self.context_overflow_tokens().is_some() {
            Some("input")
        } else {
            None
        }
    }

    #[must_use]
    pub fn client_message(&self) -> String {
        match self {
            Self::Unauthorized => "Invalid or missing API key.".to_owned(),
            Self::PayloadTooLarge { max_bytes } => {
                format!("Request body exceeds maximum allowed size ({max_bytes} bytes).")
            }
            Self::NotFound { resource } => format!("Resource `{resource}` was not found."),
            Self::ProviderDisabled { provider_id } => format!(
                "Provider `{provider_id}` is disabled. Enable it via `lg providers` or `providers.json`."
            ),
            Self::InvalidBody { source } => match source {
                RequestBodyError::MalformedJson { source: json_err } => {
                    format!("Malformed JSON request body: {json_err}")
                }
                RequestBodyError::InvalidContentLength
                | RequestBodyError::ReadTimeout
                | RequestBodyError::ReadFrame { .. }
                | RequestBodyError::ExceededContentLength
                | RequestBodyError::ContentLengthMismatch => format_error_chain(source),
            },
            Self::Protocol { source } => format_error_chain(source),
            Self::Acp { source } => format_error_chain(source),
            Self::SharedAcp { source } => format_error_chain(source.as_ref()),
        }
    }

    /// Standard OpenAI wire message (`Your input exceeds the context window of this model...`)
    /// recognized by OpenAI Codex (`codex-rs`) for automatic reactive context compaction.
    #[must_use]
    pub fn openai_client_message(&self) -> String {
        if self.context_overflow_tokens().is_some() {
            "Your input exceeds the context window of this model. Please adjust your input and try again."
                .to_owned()
        } else {
            self.client_message()
        }
    }

    /// Standard Anthropic wire message (`prompt is too long: N tokens > M maximum`) recognized
    /// by Claude Code (`cc`) for automatic reactive context compaction.
    #[must_use]
    pub fn anthropic_client_message(&self) -> String {
        if let Some((used_tokens, max_tokens)) = self.context_overflow_tokens() {
            format!("prompt is too long: {used_tokens} tokens > {max_tokens} maximum")
        } else {
            self.client_message()
        }
    }

    #[must_use]
    pub fn into_openai_response(self) -> Response {
        tracing::error!(error = ?self, "OpenAI endpoint request failed");
        let status = self.status_code();
        let (error_type, code) = self.stable_kind();
        let param = self.openai_param().map(str::to_owned);
        let body = OpenAiErrorEnvelope {
            error: OpenAiErrorBody {
                message: self.openai_client_message(),
                error_type,
                code,
                param,
            },
        };
        (status, Json(body)).into_response()
    }

    #[must_use]
    pub fn into_anthropic_response(self) -> Response {
        tracing::error!(error = ?self, "Anthropic endpoint request failed");
        let status = self.anthropic_status_code();
        let error_type = self.anthropic_error_type();
        let body = AnthropicErrorEnvelope {
            envelope_type: "error",
            error: AnthropicErrorBody {
                error_type,
                message: self.anthropic_client_message(),
            },
        };
        (status, Json(body)).into_response()
    }
}
