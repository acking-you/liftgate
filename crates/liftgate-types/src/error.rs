use thiserror::Error;

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

    #[error("`max_line_bytes` ({max_line_bytes}) must exceed `max_body_bytes` ({max_body_bytes})")]
    LineLimitBelowBodyLimit {
        max_line_bytes: usize,
        max_body_bytes: usize,
    },

    #[error("invalid data URL for base64 image input")]
    InvalidImageDataUrl,

    #[error("invalid proxy URL `{value}`: {reason}")]
    InvalidProxyUrl { value: String, reason: &'static str },
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
