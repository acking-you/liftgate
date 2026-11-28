//! Typed upstream Platform HTTP/SSE errors with strict rate-limit classification.

use thiserror::Error;

/// Typed error returned by stateless Platform API drivers (OpenAI, Anthropic, Google Gemini).
#[derive(Debug, Error)]
pub enum PlatformApiError {
    #[error("missing credentials for provider `{provider_id}`: {hint}")]
    MissingCredentials { provider_id: String, hint: String },

    #[error("invalid upstream URL `{url}`: {reason}")]
    InvalidUrl { url: String, reason: String },

    #[error("upstream authentication failed (HTTP {status}): {message}")]
    AuthenticationFailed { status: u16, message: String },

    #[error("upstream rate limit reached (HTTP {status}): {message}")]
    RateLimited {
        status: u16,
        retry_after_ms: Option<u64>,
        message: String,
    },

    #[error("upstream quota exhausted (HTTP {status}): {message}")]
    QuotaExhausted { status: u16, message: String },

    #[error("upstream rejected request (HTTP {status}): {message}")]
    InvalidRequest { status: u16, message: String },

    #[error("upstream model or endpoint not found (HTTP {status}): {message}")]
    NotFound { status: u16, message: String },

    #[error("upstream server error (HTTP {status}): {message}")]
    UpstreamServer { status: u16, message: String },

    #[error("HTTP transport failed: {message}")]
    Transport {
        message: String,
        #[source]
        source: Option<std::io::Error>,
    },

    #[error("upstream request timed out after {elapsed_ms} ms")]
    Timeout { elapsed_ms: u64 },

    #[error("upstream SSE protocol error: {message}")]
    Protocol { message: String },
}

impl PlatformApiError {
    /// Exhaustive retry classifier (no `_` wildcard).
    ///
    /// Per repository invariants, `429` / capacity errors are NEVER retried or failed over across models.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. }
            | Self::QuotaExhausted { .. }
            | Self::MissingCredentials { .. }
            | Self::InvalidUrl { .. }
            | Self::AuthenticationFailed { .. }
            | Self::InvalidRequest { .. }
            | Self::NotFound { .. }
            | Self::Timeout { .. }
            | Self::Protocol { .. } => false,
            Self::UpstreamServer { status, .. } => matches!(*status, 502..=504),
            Self::Transport { .. } => true,
        }
    }

    #[must_use]
    pub fn from_http_status(status: u16, retry_after_ms: Option<u64>, raw_body: &str) -> Self {
        let extracted = extract_api_error_message(raw_body);
        match status {
            401 | 403 => Self::AuthenticationFailed {
                status,
                message: extracted,
            },
            402 => Self::QuotaExhausted {
                status,
                message: extracted,
            },
            404 => Self::NotFound {
                status,
                message: extracted,
            },
            429 | 529 => Self::RateLimited {
                status: 429,
                retry_after_ms,
                message: extracted,
            },
            400..=499 => {
                let lower_msg = extracted.to_ascii_lowercase();
                let lower_raw = raw_body.to_ascii_lowercase();
                if lower_msg.contains("quota exceeded") || lower_raw.contains("insufficient_quota")
                {
                    Self::QuotaExhausted {
                        status,
                        message: extracted,
                    }
                } else {
                    Self::InvalidRequest {
                        status,
                        message: extracted,
                    }
                }
            }
            _ => Self::UpstreamServer {
                status,
                message: extracted,
            },
        }
    }
}

/// Extracts a concise human-readable error message from OpenAI, Anthropic, or Gemini JSON error bodies.
#[must_use]
pub fn extract_api_error_message(raw_body: &str) -> String {
    let trimmed = raw_body.trim();
    if trimmed.is_empty() {
        return "empty upstream error body".to_owned();
    }
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(msg) = val
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            return msg.trim().to_owned();
        }
        if let Some(arr) = val.as_array()
            && let Some(first) = arr.first()
            && let Some(msg) = first
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.trim().is_empty())
        {
            return msg.trim().to_owned();
        }
        if let Some(msg) = val.get("message").and_then(serde_json::Value::as_str)
            && !msg.trim().is_empty()
        {
            return msg.trim().to_owned();
        }
    }
    let mut bounded: String = trimmed.chars().take(600).collect();
    if trimmed.chars().count() > 600 {
        bounded.push_str("...");
    }
    bounded
}
