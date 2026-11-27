use std::fmt::Write as _;

use thiserror::Error;

/// Specific syntax or range failure when parsing a [`crate::config::ProxyUrl`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum ProxyUrlError {
    #[error("proxy address must not be empty")]
    Empty,

    #[error("proxy address must not contain whitespace or control characters")]
    WhitespaceOrControl,

    #[error("port number must be an integer between 1 and 65535")]
    InvalidPort,

    #[error("scheme must be http://, https://, socks5://, or socks5h://")]
    UnsupportedScheme,

    #[error("proxy URL must not contain a URL path")]
    ContainsPath,

    #[error("missing URL scheme (expected e.g. http://127.0.0.1:7890)")]
    MissingScheme,

    #[error("proxy credentials before `@` must not be empty")]
    EmptyCredentials,

    #[error("proxy host must not be empty")]
    EmptyHost,

    #[error("unclosed IPv6 bracket in proxy host")]
    UnclosedIpv6Bracket,

    #[error("IPv6 proxy host must not be empty")]
    EmptyIpv6Host,

    #[error("unexpected characters after IPv6 host bracket")]
    UnexpectedAfterIpv6Bracket,

    #[error("IPv6 proxy host must be enclosed in brackets (e.g. [::1]:7890)")]
    UnbracketedIpv6Host,
}

/// Specific syntax or range failure when parsing a [`crate::config::GatewayEndpoint`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum GatewayEndpointError {
    #[error("gateway endpoint must not be empty")]
    Empty,

    #[error("gateway endpoint must not contain whitespace or control characters")]
    WhitespaceOrControl,

    #[error("port number must be an integer between 1 and 65535")]
    InvalidPort,

    #[error("endpoint scheme must be http:// (or omit scheme for <ip:port>)")]
    UnsupportedScheme,

    #[error(
        "gateway endpoint must not embed URL userinfo credentials (`@`); pass `--api-key` instead"
    )]
    ContainsCredentials,

    #[error("gateway endpoint must not contain a URL path beyond optional `/v1`")]
    ContainsPath,

    #[error("gateway endpoint host must not be empty")]
    EmptyHost,

    #[error("unclosed IPv6 bracket in gateway endpoint host")]
    UnclosedIpv6Bracket,

    #[error("IPv6 gateway endpoint host must not be empty")]
    EmptyIpv6Host,

    #[error("unexpected characters after IPv6 host bracket")]
    UnexpectedAfterIpv6Bracket,

    #[error("IPv6 endpoint host must be enclosed in brackets (e.g. [fd00::1]:8787)")]
    UnbracketedIpv6Host,
}

/// Validation failures for wire, configuration, and identifier invariants.
#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("session identifier must not be empty")]
    EmptySessionId,

    #[error("model identifier must not be empty")]
    EmptyModelId,

    #[error("ACP mode identifier must not be empty")]
    EmptyModeId,

    #[error("configured limit `{field}` must be non-zero")]
    ZeroLimit { field: &'static str },

    #[error("configured limit `{field}` ({value}) must be between {min} and {max}")]
    LimitOutOfRange {
        field: &'static str,
        value: usize,
        min: usize,
        max: usize,
    },

    #[error("`max_line_bytes` ({max_line_bytes}) must exceed `max_body_bytes` ({max_body_bytes})")]
    LineLimitBelowBodyLimit {
        max_line_bytes: usize,
        max_body_bytes: usize,
    },

    #[error("invalid data URL for base64 image input")]
    InvalidImageDataUrl,

    #[error("invalid proxy URL `{value}`")]
    InvalidProxyUrl {
        value: String,
        #[source]
        source: ProxyUrlError,
    },

    #[error("invalid gateway endpoint `{value}`")]
    InvalidGatewayEndpoint {
        value: String,
        #[source]
        source: GatewayEndpointError,
    },
}

/// Protocol normalization and payload decoding errors.
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error(
        "Your input exceeds the context window of this model ({bytes} bytes). Please adjust your input and try again."
    )]
    PromptTooLarge { bytes: usize },
    #[error(
        "Your input exceeds the context window of this model ({bytes} directive bytes). Please disable unused client tools or compact the conversation and try again."
    )]
    PromptDirectivesTooLarge { bytes: usize },

    #[error("request contains no prompt input messages")]
    EmptyInput,

    #[error("unsupported image URL scheme (only `data:<mime>;base64,...` is supported inline)")]
    UnsupportedImageUrl,

    #[error("invalid JSON payload")]
    Json {
        #[from]
        source: serde_json::Error,
    },

    #[error("invariant validation failed")]
    Validation {
        #[from]
        source: ValidationError,
    },
}

impl ProtocolError {
    /// Returns the offending byte size when this error represents a prompt/context size overflow.
    #[must_use]
    pub const fn context_overflow_bytes(&self) -> Option<usize> {
        match self {
            Self::PromptTooLarge { bytes } | Self::PromptDirectivesTooLarge { bytes } => {
                Some(*bytes)
            }
            Self::EmptyInput
            | Self::UnsupportedImageUrl
            | Self::Json { .. }
            | Self::Validation { .. } => None,
        }
    }
}

/// Formats `err` together with its full [`std::error::Error::source`] chain separated by `": "`.
///
/// Because `thiserror` `#[error("...")]` templates do not repeat `#[source]` / `#[from]`
/// messages, single-line human-facing surfaces (CLI diagnostics, TUI toasts, desktop alerts,
/// and local telemetry entries) use this helper to render complete context.
#[must_use]
pub fn format_error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut current = err.source();
    while let Some(cause) = current {
        let _ = write!(out, ": {cause}");
        current = cause.source();
    }
    out
}
