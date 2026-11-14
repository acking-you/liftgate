use std::fmt;
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::ValidationError;

pub const DEFAULT_MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_BODY_BYTES: usize = 12 * 1024 * 1024;
pub const DEFAULT_CHANNEL_CAPACITY: usize = 256;
pub const DEFAULT_MAX_IDLE_SESSIONS: usize = 64;
pub const DEFAULT_MAX_STORED_RESPONSES: usize = 256;
pub const DEFAULT_TURN_TIMEOUT_SECS: u64 = 600;

const _: () = assert!(
    DEFAULT_MAX_LINE_BYTES > DEFAULT_MAX_BODY_BYTES,
    "ACP stdio line buffer limit must exceed HTTP request body limit"
);

/// Validated non-empty ACP session identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId(Arc<str>);

impl SessionId {
    pub fn try_new(raw: impl AsRef<str>) -> Result<Self, ValidationError> {
        let trimmed = raw.as_ref().trim();
        if trimmed.is_empty() {
            return Err(ValidationError::EmptySessionId);
        }
        Ok(Self(Arc::from(trimmed)))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for SessionId {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<SessionId> for String {
    fn from(value: SessionId) -> Self {
        value.0.to_string()
    }
}

/// Validated non-empty model identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModelId(Arc<str>);

impl ModelId {
    pub fn try_new(raw: impl AsRef<str>) -> Result<Self, ValidationError> {
        let trimmed = raw.as_ref().trim();
        if trimmed.is_empty() {
            return Err(ValidationError::EmptyModelId);
        }
        Ok(Self(Arc::from(trimmed)))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ModelId {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<ModelId> for String {
    fn from(value: ModelId) -> Self {
        value.0.to_string()
    }
}

/// Validated non-empty ACP execution mode identifier (e.g. `auto`, `accept-edits`, `default`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AcpModeId(Arc<str>);

impl AcpModeId {
    pub fn try_new(raw: impl AsRef<str>) -> Result<Self, ValidationError> {
        let trimmed = raw.as_ref().trim();
        if trimmed.is_empty() {
            return Err(ValidationError::EmptyModeId);
        }
        Ok(Self(Arc::from(trimmed)))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AcpModeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for AcpModeId {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<AcpModeId> for String {
    fn from(value: AcpModeId) -> Self {
        value.0.to_string()
    }
}

/// Bounded resource limits for external HTTP bodies, ACP stdio lines, and queues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GatewayLimits {
    max_line_bytes: NonZeroUsize,
    max_body_bytes: NonZeroUsize,
    channel_capacity: NonZeroUsize,
    max_idle_sessions: NonZeroUsize,
    max_stored_responses: NonZeroUsize,
    turn_timeout_secs: NonZeroU64,
}

impl GatewayLimits {
    pub fn try_new(
        max_line_bytes: usize,
        max_body_bytes: usize,
        channel_capacity: usize,
        max_idle_sessions: usize,
        max_stored_responses: usize,
        turn_timeout_secs: u64,
    ) -> Result<Self, ValidationError> {
        let max_line_bytes =
            NonZeroUsize::new(max_line_bytes).ok_or(ValidationError::ZeroLimit {
                field: "max_line_bytes",
            })?;
        let max_body_bytes =
            NonZeroUsize::new(max_body_bytes).ok_or(ValidationError::ZeroLimit {
                field: "max_body_bytes",
            })?;
        if max_line_bytes.get() <= max_body_bytes.get() {
            return Err(ValidationError::LineLimitBelowBodyLimit {
                max_line_bytes: max_line_bytes.get(),
                max_body_bytes: max_body_bytes.get(),
            });
        }
        let channel_capacity =
            NonZeroUsize::new(channel_capacity).ok_or(ValidationError::ZeroLimit {
                field: "channel_capacity",
            })?;
        let max_idle_sessions =
            NonZeroUsize::new(max_idle_sessions).ok_or(ValidationError::ZeroLimit {
                field: "max_idle_sessions",
            })?;
        let max_stored_responses =
            NonZeroUsize::new(max_stored_responses).ok_or(ValidationError::ZeroLimit {
                field: "max_stored_responses",
            })?;
        let turn_timeout_secs =
            NonZeroU64::new(turn_timeout_secs).ok_or(ValidationError::ZeroLimit {
                field: "turn_timeout_secs",
            })?;

        Ok(Self {
            max_line_bytes,
            max_body_bytes,
            channel_capacity,
            max_idle_sessions,
            max_stored_responses,
            turn_timeout_secs,
        })
    }

    #[must_use]
    pub const fn max_line_bytes(self) -> NonZeroUsize {
        self.max_line_bytes
    }

    #[must_use]
    pub const fn max_body_bytes(self) -> NonZeroUsize {
        self.max_body_bytes
    }

    #[must_use]
    pub const fn channel_capacity(self) -> NonZeroUsize {
        self.channel_capacity
    }

    #[must_use]
    pub const fn max_idle_sessions(self) -> NonZeroUsize {
        self.max_idle_sessions
    }

    #[must_use]
    pub const fn max_stored_responses(self) -> NonZeroUsize {
        self.max_stored_responses
    }

    #[must_use]
    pub const fn turn_timeout_secs(self) -> NonZeroU64 {
        self.turn_timeout_secs
    }
}

impl Default for GatewayLimits {
    fn default() -> Self {
        Self {
            max_line_bytes: NonZeroUsize::new(DEFAULT_MAX_LINE_BYTES)
                .expect("DEFAULT_MAX_LINE_BYTES is a non-zero compile-time constant"),
            max_body_bytes: NonZeroUsize::new(DEFAULT_MAX_BODY_BYTES)
                .expect("DEFAULT_MAX_BODY_BYTES is a non-zero compile-time constant"),
            channel_capacity: NonZeroUsize::new(DEFAULT_CHANNEL_CAPACITY)
                .expect("DEFAULT_CHANNEL_CAPACITY is a non-zero compile-time constant"),
            max_idle_sessions: NonZeroUsize::new(DEFAULT_MAX_IDLE_SESSIONS)
                .expect("DEFAULT_MAX_IDLE_SESSIONS is a non-zero compile-time constant"),
            max_stored_responses: NonZeroUsize::new(DEFAULT_MAX_STORED_RESPONSES)
                .expect("DEFAULT_MAX_STORED_RESPONSES is a non-zero compile-time constant"),
            turn_timeout_secs: NonZeroU64::new(DEFAULT_TURN_TIMEOUT_SECS)
                .expect("DEFAULT_TURN_TIMEOUT_SECS is a non-zero compile-time constant"),
        }
    }
}
