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
}

/// Protocol normalization and payload decoding errors.
#[derive(Debug, Error)]
pub enum ProtocolError {
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
