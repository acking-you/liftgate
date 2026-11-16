use std::borrow::Cow;
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
pub const DEFAULT_TURN_TIMEOUT_SECS: u64 = 30 * 60;

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
    /// Default low-effort Hydrogen model for short background requests.
    #[must_use]
    pub fn fallback_fast() -> Self {
        Self(Arc::from("gemini-4-hydrogen-low"))
    }

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

/// Returns `true` when `raw` is a user-facing keyword disabling proxy injection.
#[must_use]
pub fn is_proxy_disable_keyword(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "off" | "none" | "direct" | "clear" | "unset" | "false" | "disable" | "disabled"
    )
}

/// Validated outbound proxy URL (`http://`, `https://`, `socks5://`, or `socks5h://`)
/// injected into `agy` subprocesses when explicitly configured.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProxyUrl(Arc<str>);

impl ProxyUrl {
    /// Parses and normalizes a proxy specification.
    ///
    /// Accepted forms:
    /// - Port shorthand (`"7890"`) -> `"http://127.0.0.1:7890"`
    /// - Host:port shorthand (`"127.0.0.1:7890"`, `"[::1]:1080"`) -> `"http://<host>:<port>"`
    /// - Explicit URL (`http://`, `https://`, `socks5://`, `socks5h://`)
    pub fn try_new(raw: impl AsRef<str>) -> Result<Self, ValidationError> {
        let raw_ref = raw.as_ref();
        let trimmed = raw_ref.trim();
        if trimmed.is_empty() {
            return Err(ValidationError::InvalidProxyUrl {
                value: raw_ref.to_owned(),
                reason: "proxy address must not be empty",
            });
        }
        if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(ValidationError::InvalidProxyUrl {
                value: trimmed.to_owned(),
                reason: "proxy address must not contain whitespace or control characters",
            });
        }

        // 1. Pure port shorthand: e.g. `7890` -> `http://127.0.0.1:7890`.
        if trimmed.bytes().all(|b| b.is_ascii_digit()) {
            let port = trimmed
                .parse::<u16>()
                .ok()
                .filter(|&p| p > 0)
                .ok_or_else(|| ValidationError::InvalidProxyUrl {
                    value: trimmed.to_owned(),
                    reason: "port number must be between 1 and 65535",
                })?;
            return Ok(Self(Arc::from(format!("http://127.0.0.1:{port}"))));
        }

        // 2. Host:port shorthand without scheme (`://`).
        let normalized = if let Some((scheme_raw, rest_raw)) = trimmed.split_once("://") {
            let scheme = scheme_raw.to_ascii_lowercase();
            if !matches!(scheme.as_str(), "http" | "https" | "socks5" | "socks5h") {
                return Err(ValidationError::InvalidProxyUrl {
                    value: trimmed.to_owned(),
                    reason: "scheme must be http://, https://, socks5://, or socks5h://",
                });
            }
            let rest = rest_raw.strip_suffix('/').unwrap_or(rest_raw);
            if rest.contains('/') {
                return Err(ValidationError::InvalidProxyUrl {
                    value: trimmed.to_owned(),
                    reason: "proxy URL must not contain a URL path",
                });
            }
            validate_proxy_authority(rest, trimmed)?;
            format!("{scheme}://{rest}")
        } else {
            let rest = trimmed.strip_suffix('/').unwrap_or(trimmed);
            if rest.contains('/') {
                return Err(ValidationError::InvalidProxyUrl {
                    value: trimmed.to_owned(),
                    reason: "missing URL scheme (expected e.g. http://127.0.0.1:7890)",
                });
            }
            validate_proxy_authority(rest, trimmed)?;
            format!("http://{rest}")
        };

        Ok(Self(Arc::from(normalized)))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Formats the proxy URL for logs, UI, and telemetry while masking any embedded `user:pass@` password.
    #[must_use]
    pub fn redacted_display(&self) -> Cow<'_, str> {
        let raw = self.as_str();
        let Some((scheme, after_scheme)) = raw.split_once("://") else {
            return Cow::Borrowed(raw);
        };
        let Some((userinfo, host_part)) = after_scheme.rsplit_once('@') else {
            return Cow::Borrowed(raw);
        };
        if let Some((user, _secret)) = userinfo.split_once(':') {
            Cow::Owned(format!("{scheme}://{user}:***@{host_part}"))
        } else {
            Cow::Owned(format!("{scheme}://***@{host_part}"))
        }
    }
}

fn validate_proxy_authority(authority: &str, original: &str) -> Result<(), ValidationError> {
    let host_port = match authority.rsplit_once('@') {
        Some((userinfo, hp)) => {
            if userinfo.is_empty() {
                return Err(ValidationError::InvalidProxyUrl {
                    value: original.to_owned(),
                    reason: "proxy credentials before '@' must not be empty",
                });
            }
            hp
        }
        None => authority,
    };

    if host_port.is_empty() {
        return Err(ValidationError::InvalidProxyUrl {
            value: original.to_owned(),
            reason: "proxy host must not be empty",
        });
    }

    let (host, port_opt) = if let Some(after_bracket) = host_port.strip_prefix('[') {
        let Some((ipv6_host, tail)) = after_bracket.split_once(']') else {
            return Err(ValidationError::InvalidProxyUrl {
                value: original.to_owned(),
                reason: "unclosed IPv6 bracket in proxy host",
            });
        };
        if ipv6_host.is_empty() {
            return Err(ValidationError::InvalidProxyUrl {
                value: original.to_owned(),
                reason: "IPv6 proxy host must not be empty",
            });
        }
        let port_str = if tail.is_empty() {
            None
        } else if let Some(p) = tail.strip_prefix(':') {
            Some(p)
        } else {
            return Err(ValidationError::InvalidProxyUrl {
                value: original.to_owned(),
                reason: "unexpected characters after IPv6 host bracket",
            });
        };
        (ipv6_host, port_str)
    } else if let Some((h, p)) = host_port.rsplit_once(':') {
        if h.contains(':') {
            return Err(ValidationError::InvalidProxyUrl {
                value: original.to_owned(),
                reason: "IPv6 proxy host must be enclosed in brackets (e.g. [::1]:7890)",
            });
        }
        (h, Some(p))
    } else {
        (host_port, None)
    };

    if host.is_empty() {
        return Err(ValidationError::InvalidProxyUrl {
            value: original.to_owned(),
            reason: "proxy host must not be empty",
        });
    }
    if let Some(p_str) = port_opt {
        let valid_port = p_str.parse::<u16>().ok().is_some_and(|p| p > 0);
        if !valid_port {
            return Err(ValidationError::InvalidProxyUrl {
                value: original.to_owned(),
                reason: "proxy port must be a valid integer between 1 and 65535",
            });
        }
    }
    Ok(())
}

impl fmt::Display for ProxyUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ProxyUrl {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<ProxyUrl> for String {
    fn from(value: ProxyUrl) -> Self {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_url_normalizes_ports_hosts_schemes_and_redacts_secrets() {
        assert_eq!(
            ProxyUrl::try_new("7890").unwrap().as_str(),
            "http://127.0.0.1:7890"
        );
        assert_eq!(
            ProxyUrl::try_new("127.0.0.1:7890").unwrap().as_str(),
            "http://127.0.0.1:7890"
        );
        assert_eq!(
            ProxyUrl::try_new("[::1]:1080").unwrap().as_str(),
            "http://[::1]:1080"
        );
        assert_eq!(
            ProxyUrl::try_new("SOCKS5H://127.0.0.1:1080/")
                .unwrap()
                .as_str(),
            "socks5h://127.0.0.1:1080"
        );
        let authed = ProxyUrl::try_new("socks5://alice:s3cr3t@proxy.internal:1080").unwrap();
        assert_eq!(
            authed.redacted_display(),
            "socks5://alice:***@proxy.internal:1080"
        );
        assert!(is_proxy_disable_keyword("off"));
        assert!(is_proxy_disable_keyword("DIRECT"));
        assert!(!is_proxy_disable_keyword("7890"));

        assert!(ProxyUrl::try_new("").is_err());
        assert!(ProxyUrl::try_new("0").is_err());
        assert!(ProxyUrl::try_new("70000").is_err());
        assert!(ProxyUrl::try_new("ftp://127.0.0.1:21").is_err());
        assert!(ProxyUrl::try_new("http://").is_err());
        assert!(ProxyUrl::try_new("http://127.0.0.1:7890/bad/path").is_err());
        assert!(ProxyUrl::try_new("http://127.0.0.1:7890 bad").is_err());
    }
}
