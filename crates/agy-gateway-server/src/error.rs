use std::path::PathBuf;
use std::sync::Arc;

use agy_gateway_types::{
    anthropic::{AnthropicErrorBody, AnthropicErrorEnvelope},
    error::ProtocolError,
    openai::{OpenAiErrorBody, OpenAiErrorEnvelope},
};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use thiserror::Error;
use tokio_util::codec::LinesCodecError;

/// Errors originating from ACP subprocess supervision or JSON-RPC transport.
#[derive(Debug, Error)]
pub enum AcpError {
    #[error("failed to spawn ACP subprocess `{}`", path.display())]
    SpawnProcess {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to attach ACP subprocess to OS process group / Job Object")]
    ProcessGroup {
        #[source]
        source: std::io::Error,
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

    #[error("upstream ACP agent terminated with fatal error: {detail}")]
    UpstreamAgentTerminated { detail: String },

    #[error("ACP prompt turn was cancelled")]
    Cancelled,
}

/// Top-level gateway request error mapped cleanly at HTTP boundaries.
#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("invalid or missing gateway API key")]
    Unauthorized,

    #[error("requested resource `{resource}` was not found")]
    NotFound { resource: String },

    #[error("invalid request payload")]
    BadJson { message: String },

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
            Self::NotFound { .. } => StatusCode::NOT_FOUND,
            Self::BadJson { .. } | Self::Protocol { .. } => StatusCode::BAD_REQUEST,
            Self::Acp { source } => Self::acp_status(source),
            Self::SharedAcp { source } => Self::acp_status(source),
        }
    }

    const fn acp_status(source: &AcpError) -> StatusCode {
        match source {
            AcpError::TurnTimeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            AcpError::UpstreamRateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            AcpError::UpstreamAgentTerminated { .. }
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
            Self::NotFound { .. } => ("not_found_error", "not_found"),
            Self::BadJson { .. } | Self::Protocol { .. } => {
                ("invalid_request_error", "invalid_request")
            }
            Self::Acp { source } => Self::acp_kind(source),
            Self::SharedAcp { source } => Self::acp_kind(source),
        }
    }

    const fn acp_kind(source: &AcpError) -> (&'static str, &'static str) {
        match source {
            AcpError::TurnTimeout { .. } => ("api_error", "upstream_timeout"),
            AcpError::UpstreamRateLimited { .. } => ("rate_limit_error", "rate_limit_exceeded"),
            AcpError::UpstreamAgentTerminated { .. } => ("api_error", "upstream_agent_error"),
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

    #[must_use]
    pub fn client_message(&self) -> String {
        match self {
            Self::Unauthorized => "Invalid or missing API key.".to_owned(),
            Self::NotFound { resource } => format!("Resource `{resource}` was not found."),
            Self::BadJson { message } => format!("Malformed JSON request body: {message}"),
            Self::Protocol { source } => source.to_string(),
            Self::Acp { source } => source.to_string(),
            Self::SharedAcp { source } => source.to_string(),
        }
    }

    #[must_use]
    pub fn into_openai_response(self) -> Response {
        tracing::error!(error = ?self, "OpenAI endpoint request failed");
        let status = self.status_code();
        let (error_type, code) = self.stable_kind();
        let body = OpenAiErrorEnvelope {
            error: OpenAiErrorBody {
                message: self.client_message(),
                error_type,
                code,
                param: None,
            },
        };
        (status, Json(body)).into_response()
    }

    #[must_use]
    pub fn into_anthropic_response(self) -> Response {
        tracing::error!(error = ?self, "Anthropic endpoint request failed");
        let status = self.status_code();
        let (error_type, _) = self.stable_kind();
        let body = AnthropicErrorEnvelope {
            envelope_type: "error",
            error: AnthropicErrorBody {
                error_type,
                message: self.client_message(),
            },
        };
        (status, Json(body)).into_response()
    }
}
