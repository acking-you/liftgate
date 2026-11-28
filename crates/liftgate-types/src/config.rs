use std::borrow::Cow;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::{NonZeroU16, NonZeroU64, NonZeroUsize};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{GatewayEndpointError, ProxyUrlError, ValidationError};

pub const DEFAULT_MAX_LINE_BYTES: usize = 48 * 1024 * 1024;
pub const DEFAULT_MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
pub const DEFAULT_CHANNEL_CAPACITY: usize = 256;
pub const DEFAULT_MAX_IDLE_SESSIONS: usize = 64;
pub const DEFAULT_MAX_STORED_RESPONSES: usize = 256;
pub const DEFAULT_TURN_TIMEOUT_SECS: u64 = 30 * 60;
pub const DEFAULT_MAX_LOW_CACHE_RECORDS: usize = 1_000;
pub const MAX_ALLOWED_LOW_CACHE_RECORDS: usize = 50_000;
pub const DEFAULT_LOW_CACHE_THRESHOLD_BPS: u16 = 5_000;
pub const DEFAULT_MAX_CONTEXT_CAPTURES: usize = 200;
pub const MAX_ALLOWED_CONTEXT_CAPTURES: usize = 10_000;
pub const DEFAULT_CONTEXT_CAPTURE_DISK_MIB: usize = 512;
pub const MIN_CONTEXT_CAPTURE_DISK_MIB: usize = 16;
pub const MAX_CONTEXT_CAPTURE_DISK_MIB: usize = 64 * 1024;
pub const DEFAULT_AUTO_UPDATE_INTERVAL_MINS: u64 = 30;
pub const MIN_AUTO_UPDATE_INTERVAL_MINS: u64 = 5;
pub const MAX_AUTO_UPDATE_INTERVAL_MINS: u64 = 24 * 60;
pub const DEFAULT_GATEWAY_PORT: u16 = 8787;

const _: () = assert!(
    DEFAULT_MAX_LINE_BYTES > DEFAULT_MAX_BODY_BYTES,
    "ACP stdio line buffer limit must exceed HTTP request body limit"
);
const _: () = assert!(
    DEFAULT_MAX_LOW_CACHE_RECORDS <= MAX_ALLOWED_LOW_CACHE_RECORDS,
    "default persisted low-cache record limit must not exceed hard upper bound"
);
const _: () = assert!(
    DEFAULT_LOW_CACHE_THRESHOLD_BPS <= 10_000,
    "default low-cache threshold must be valid basis points (0..=10,000)"
);
const _: () = assert!(
    DEFAULT_MAX_CONTEXT_CAPTURES >= 1
        && DEFAULT_MAX_CONTEXT_CAPTURES <= MAX_ALLOWED_CONTEXT_CAPTURES,
    "default full-context capture retention must lie inside its validated range"
);
const _: () = assert!(
    DEFAULT_CONTEXT_CAPTURE_DISK_MIB >= MIN_CONTEXT_CAPTURE_DISK_MIB
        && DEFAULT_CONTEXT_CAPTURE_DISK_MIB <= MAX_CONTEXT_CAPTURE_DISK_MIB,
    "default full-context capture disk quota must lie inside its validated range"
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

    /// Default high-capability model identifier used when no explicit model is supplied.
    #[must_use]
    pub fn fallback_primary() -> Self {
        Self(Arc::from("gemini-4-carbon-high"))
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
                source: ProxyUrlError::Empty,
            });
        }
        if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(ValidationError::InvalidProxyUrl {
                value: trimmed.to_owned(),
                source: ProxyUrlError::WhitespaceOrControl,
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
                    source: ProxyUrlError::InvalidPort,
                })?;
            return Ok(Self(Arc::from(format!("http://127.0.0.1:{port}"))));
        }

        // 2. Host:port shorthand without scheme (`://`).
        let normalized = if let Some((scheme_raw, rest_raw)) = trimmed.split_once("://") {
            let scheme = scheme_raw.to_ascii_lowercase();
            if !matches!(scheme.as_str(), "http" | "https" | "socks5" | "socks5h") {
                return Err(ValidationError::InvalidProxyUrl {
                    value: trimmed.to_owned(),
                    source: ProxyUrlError::UnsupportedScheme,
                });
            }
            let rest = rest_raw.strip_suffix('/').unwrap_or(rest_raw);
            if rest.contains('/') {
                return Err(ValidationError::InvalidProxyUrl {
                    value: trimmed.to_owned(),
                    source: ProxyUrlError::ContainsPath,
                });
            }
            validate_proxy_authority(rest, trimmed)?;
            format!("{scheme}://{rest}")
        } else {
            let rest = trimmed.strip_suffix('/').unwrap_or(trimmed);
            if rest.contains('/') {
                return Err(ValidationError::InvalidProxyUrl {
                    value: trimmed.to_owned(),
                    source: ProxyUrlError::MissingScheme,
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
                    source: ProxyUrlError::EmptyCredentials,
                });
            }
            hp
        }
        None => authority,
    };

    if host_port.is_empty() {
        return Err(ValidationError::InvalidProxyUrl {
            value: original.to_owned(),
            source: ProxyUrlError::EmptyHost,
        });
    }

    let (host, port_opt) = if let Some(after_bracket) = host_port.strip_prefix('[') {
        let Some((ipv6_host, tail)) = after_bracket.split_once(']') else {
            return Err(ValidationError::InvalidProxyUrl {
                value: original.to_owned(),
                source: ProxyUrlError::UnclosedIpv6Bracket,
            });
        };
        if ipv6_host.is_empty() {
            return Err(ValidationError::InvalidProxyUrl {
                value: original.to_owned(),
                source: ProxyUrlError::EmptyIpv6Host,
            });
        }
        let port_str = if tail.is_empty() {
            None
        } else if let Some(p) = tail.strip_prefix(':') {
            Some(p)
        } else {
            return Err(ValidationError::InvalidProxyUrl {
                value: original.to_owned(),
                source: ProxyUrlError::UnexpectedAfterIpv6Bracket,
            });
        };
        (ipv6_host, port_str)
    } else if let Some((h, p)) = host_port.rsplit_once(':') {
        if h.contains(':') {
            return Err(ValidationError::InvalidProxyUrl {
                value: original.to_owned(),
                source: ProxyUrlError::UnbracketedIpv6Host,
            });
        }
        (h, Some(p))
    } else {
        (host_port, None)
    };

    if host.is_empty() {
        return Err(ValidationError::InvalidProxyUrl {
            value: original.to_owned(),
            source: ProxyUrlError::EmptyHost,
        });
    }
    if let Some(p_str) = port_opt {
        let valid_port = p_str.parse::<u16>().ok().is_some_and(|p| p > 0);
        if !valid_port {
            return Err(ValidationError::InvalidProxyUrl {
                value: original.to_owned(),
                source: ProxyUrlError::InvalidPort,
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

/// Validated retention bound for persisted low-cache-hit records (`0..=50_000`; `1_000` by default; `0` disables recording).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "usize", into = "usize")]
pub struct LowCacheRecordLimit(usize);

impl LowCacheRecordLimit {
    pub fn try_new(raw: usize) -> Result<Self, ValidationError> {
        if raw > MAX_ALLOWED_LOW_CACHE_RECORDS {
            return Err(ValidationError::LimitOutOfRange {
                field: "max_low_cache_records",
                value: raw,
                min: 0,
                max: MAX_ALLOWED_LOW_CACHE_RECORDS,
            });
        }
        Ok(Self(raw))
    }

    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }
}

impl Default for LowCacheRecordLimit {
    fn default() -> Self {
        Self(DEFAULT_MAX_LOW_CACHE_RECORDS)
    }
}

impl TryFrom<usize> for LowCacheRecordLimit {
    type Error = ValidationError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<LowCacheRecordLimit> for usize {
    fn from(value: LowCacheRecordLimit) -> Self {
        value.0
    }
}

/// Validated basis-point threshold (`0..=10_000`, `5_000` = `50.00%` by default) below which
/// a completed turn's prompt cache hit rate is recorded as a low-cache-hit event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct CacheHitThresholdBps(u16);

impl CacheHitThresholdBps {
    pub fn try_new(raw: u16) -> Result<Self, ValidationError> {
        if raw > 10_000 {
            return Err(ValidationError::LimitOutOfRange {
                field: "low_cache_threshold_bps",
                value: usize::from(raw),
                min: 0,
                max: 10_000,
            });
        }
        Ok(Self(raw))
    }

    pub fn try_from_percent(pct: u8) -> Result<Self, ValidationError> {
        if pct > 100 {
            return Err(ValidationError::LimitOutOfRange {
                field: "low_cache_threshold_pct",
                value: usize::from(pct),
                min: 0,
                max: 100,
            });
        }
        Ok(Self(u16::from(pct) * 100))
    }

    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl Default for CacheHitThresholdBps {
    fn default() -> Self {
        Self(DEFAULT_LOW_CACHE_THRESHOLD_BPS)
    }
}

impl TryFrom<u16> for CacheHitThresholdBps {
    type Error = ValidationError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<CacheHitThresholdBps> for u16 {
    fn from(value: CacheHitThresholdBps) -> Self {
        value.0
    }
}

/// Validated retention bound for full-context captures (`1..=10_000`; `200` by default).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "usize", into = "usize")]
pub struct ContextCaptureRecordLimit(usize);

impl ContextCaptureRecordLimit {
    pub fn try_new(raw: usize) -> Result<Self, ValidationError> {
        if raw == 0 || raw > MAX_ALLOWED_CONTEXT_CAPTURES {
            return Err(ValidationError::LimitOutOfRange {
                field: "max_context_captures",
                value: raw,
                min: 1,
                max: MAX_ALLOWED_CONTEXT_CAPTURES,
            });
        }
        Ok(Self(raw))
    }

    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }
}

impl Default for ContextCaptureRecordLimit {
    fn default() -> Self {
        Self(DEFAULT_MAX_CONTEXT_CAPTURES)
    }
}

impl TryFrom<usize> for ContextCaptureRecordLimit {
    type Error = ValidationError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<ContextCaptureRecordLimit> for usize {
    fn from(value: ContextCaptureRecordLimit) -> Self {
        value.0
    }
}

/// Validated disk quota in MiB for full-context captures (`16..=65_536`; `512` by default).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "usize", into = "usize")]
pub struct ContextCaptureDiskMib(usize);

impl ContextCaptureDiskMib {
    pub fn try_new(raw: usize) -> Result<Self, ValidationError> {
        if !(MIN_CONTEXT_CAPTURE_DISK_MIB..=MAX_CONTEXT_CAPTURE_DISK_MIB).contains(&raw) {
            return Err(ValidationError::LimitOutOfRange {
                field: "max_context_capture_mib",
                value: raw,
                min: MIN_CONTEXT_CAPTURE_DISK_MIB,
                max: MAX_CONTEXT_CAPTURE_DISK_MIB,
            });
        }
        Ok(Self(raw))
    }

    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }

    /// Quota in bytes (`MiB * 1_048_576`).
    #[must_use]
    pub const fn bytes(self) -> u64 {
        (self.0 as u64).saturating_mul(1024 * 1024)
    }
}

impl Default for ContextCaptureDiskMib {
    fn default() -> Self {
        Self(DEFAULT_CONTEXT_CAPTURE_DISK_MIB)
    }
}

impl TryFrom<usize> for ContextCaptureDiskMib {
    type Error = ValidationError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<ContextCaptureDiskMib> for usize {
    fn from(value: ContextCaptureDiskMib) -> Self {
        value.0
    }
}

const _: () = assert!(
    DEFAULT_AUTO_UPDATE_INTERVAL_MINS >= MIN_AUTO_UPDATE_INTERVAL_MINS
        && DEFAULT_AUTO_UPDATE_INTERVAL_MINS <= MAX_AUTO_UPDATE_INTERVAL_MINS
        && MIN_AUTO_UPDATE_INTERVAL_MINS >= 1,
    "default auto-update check interval must lie inside its validated range"
);

/// Closed set of components managed by Liftgate's idle auto-update scheduler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoUpdateTarget {
    /// Liftgate CLI & managed background daemon (`lg` + `liftgate`); enabled by default.
    Liftgate,
    /// Official Antigravity ACP CLI (`agy`); enabled by default.
    Agy,
    /// OpenAI Codex CLI (`codex`); disabled by default.
    Codex,
    /// Anthropic Claude Code CLI (`claude`); disabled by default.
    Claude,
    /// OpenCode CLI (`opencode`); disabled by default.
    Opencode,
    /// Oh My Pi CLI (`omp`); disabled by default.
    Omp,
}

impl AutoUpdateTarget {
    /// Canonical display order shown in TUI tables, status reports, and JSON payloads.
    pub const ALL: [Self; 6] = [
        Self::Liftgate,
        Self::Agy,
        Self::Codex,
        Self::Claude,
        Self::Opencode,
        Self::Omp,
    ];

    /// Safe execution order for an idle update sweep (`Liftgate` runs last because a
    /// self-upgrade hands off to a freshly spawned daemon process).
    pub const SWEEP_ORDER: [Self; 6] = [
        Self::Agy,
        Self::Codex,
        Self::Claude,
        Self::Opencode,
        Self::Omp,
        Self::Liftgate,
    ];

    /// Canonical CLI/wire identifier (`lg`, `agy`, `codex`, `claude`, `opencode`, `omp`).
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Liftgate => "lg",
            Self::Agy => "agy",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Opencode => "opencode",
            Self::Omp => "omp",
        }
    }

    /// Human-readable title used in TUI and status tables.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Liftgate => "Liftgate (lg)",
            Self::Agy => "Antigravity CLI (agy)",
            Self::Codex => "Codex CLI (codex)",
            Self::Claude => "Claude Code (claude)",
            Self::Opencode => "OpenCode (opencode)",
            Self::Omp => "Oh My Pi (omp)",
        }
    }

    /// Concise update strategy summary shown in the Auto-Update Manager TUI inspector.
    #[must_use]
    pub const fn strategy_summary(self) -> &'static str {
        match self {
            Self::Liftgate => "GitHub Releases + SHA256SUMS + atomic daemon respawn",
            Self::Agy => "Official manifest + SHA-512 idle hot-swap",
            Self::Codex => "codex update (with brew / npm package-manager fallback)",
            Self::Claude => "claude update (native & package-manager aware)",
            Self::Opencode => "opencode upgrade (curl / brew / npm / bun / pnpm aware)",
            Self::Omp => "omp update (built-in release updater)",
        }
    }

    /// Whether idle auto-update is enabled out of the box (`true` for `lg` and `agy`,
    /// `false` for third-party coding harnesses).
    #[must_use]
    pub const fn default_enabled(self) -> bool {
        match self {
            Self::Liftgate | Self::Agy => true,
            Self::Codex | Self::Claude | Self::Opencode | Self::Omp => false,
        }
    }

    /// Parses a CLI target name or common shorthand alias (`lg`, `liftgate`, `cx`, `cc`, `oc`, `omp`).
    #[must_use]
    pub fn from_slug(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "lg" | "liftgate" | "self" => Some(Self::Liftgate),
            "agy" | "antigravity" => Some(Self::Agy),
            "codex" | "cx" | "codex-cli" => Some(Self::Codex),
            "claude" | "cc" | "claude-code" => Some(Self::Claude),
            "opencode" | "oc" | "open-code" => Some(Self::Opencode),
            "omp" | "oh-my-pi" => Some(Self::Omp),
            _ => None,
        }
    }
}

impl fmt::Display for AutoUpdateTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

/// Validated periodic idle check interval in minutes (`5..=1440`, `30` by default).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct AutoUpdateIntervalMins(NonZeroU64);

impl AutoUpdateIntervalMins {
    /// Interactive presets cycled by `-`/`+` or `[`/`]` inside the Auto-Update Manager TUI.
    pub const PRESETS: [u64; 8] = [5, 15, 30, 60, 120, 360, 720, 1_440];

    /// Validates that `mins` lies inside `MIN_AUTO_UPDATE_INTERVAL_MINS..=MAX_AUTO_UPDATE_INTERVAL_MINS`.
    pub fn try_new(mins: u64) -> Result<Self, ValidationError> {
        let Some(nz) = NonZeroU64::new(mins) else {
            return Err(ValidationError::ZeroLimit {
                field: "auto_update_interval_mins",
            });
        };
        if !(MIN_AUTO_UPDATE_INTERVAL_MINS..=MAX_AUTO_UPDATE_INTERVAL_MINS).contains(&mins) {
            return Err(ValidationError::LimitOutOfRange {
                field: "auto_update_interval_mins",
                value: usize::try_from(mins).unwrap_or(usize::MAX),
                min: MIN_AUTO_UPDATE_INTERVAL_MINS as usize,
                max: MAX_AUTO_UPDATE_INTERVAL_MINS as usize,
            });
        }
        Ok(Self(nz))
    }

    /// Interval in minutes (`>= 5`).
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// Interval converted to seconds (`mins * 60`).
    #[must_use]
    pub const fn as_secs(self) -> u64 {
        self.0.get().saturating_mul(60)
    }

    /// Steps down to the previous preset interval (clamped at minimum preset).
    #[must_use]
    pub fn step_prev(self) -> Self {
        let cur = self.get();
        let chosen = Self::PRESETS
            .iter()
            .copied()
            .rev()
            .find(|&p| p < cur)
            .unwrap_or(MIN_AUTO_UPDATE_INTERVAL_MINS);
        Self(NonZeroU64::new(chosen).expect("preset >= MIN_AUTO_UPDATE_INTERVAL_MINS"))
    }

    /// Steps up to the next preset interval (clamped at maximum preset).
    #[must_use]
    pub fn step_next(self) -> Self {
        let cur = self.get();
        let chosen = Self::PRESETS
            .iter()
            .copied()
            .find(|&p| p > cur)
            .unwrap_or(MAX_AUTO_UPDATE_INTERVAL_MINS);
        Self(NonZeroU64::new(chosen).expect("preset >= MIN_AUTO_UPDATE_INTERVAL_MINS"))
    }
}

impl Default for AutoUpdateIntervalMins {
    fn default() -> Self {
        Self(
            NonZeroU64::new(DEFAULT_AUTO_UPDATE_INTERVAL_MINS)
                .expect("DEFAULT_AUTO_UPDATE_INTERVAL_MINS is non-zero"),
        )
    }
}

impl TryFrom<u64> for AutoUpdateIntervalMins {
    type Error = ValidationError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<AutoUpdateIntervalMins> for u64 {
    fn from(value: AutoUpdateIntervalMins) -> Self {
        value.get()
    }
}

/// Returns `true` when `raw` is a keyword switching target mode back to the local managed daemon.
#[must_use]
pub fn is_endpoint_local_keyword(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "local" | "off" | "none" | "clear" | "reset" | "default"
    )
}

/// Extensible gateway connection mode stored in client settings (`~/.liftgate/settings.json`).
///
/// Unknown variants written by newer Liftgate releases deserialize cleanly to [`Self::Unknown`]
/// so older binaries remain forward-compatible without failing settings deserialization.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayTargetMode {
    /// Managed or external local loopback gateway (`127.0.0.1:8787` by default).
    #[default]
    Local,
    /// Local CLI/harnesses connect directly to a remote `liftgate` HTTP endpoint (`<ip:port>`).
    Remote,
    /// Forward-compatible catch-all for connection modes introduced in newer versions.
    #[serde(other)]
    Unknown,
}

impl GatewayTargetMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local | Self::Unknown => "local",
            Self::Remote => "remote",
        }
    }

    #[must_use]
    pub const fn is_remote(self) -> bool {
        matches!(self, Self::Remote)
    }
}

/// Validated HTTP gateway endpoint (`http://<host>:<port>`) used to connect local harnesses
/// and observability commands to a local or remote `liftgate` instance.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GatewayEndpoint {
    base_url: Arc<str>,
    host: Arc<str>,
    host_header: Arc<str>,
    port: NonZeroU16,
}

impl GatewayEndpoint {
    /// Parses and normalizes a gateway endpoint specification.
    ///
    /// Accepted forms:
    /// - `<ip>:<port>` / `<host>:<port>` (`"192.168.1.50:8787"`, `"[fd00::1]:8787"`, `"devbox.lan:9000"`)
    /// - Bare `<ip>` / `<host>` (`"192.168.1.50"`, `"[fd00::1]"`, `"devbox.lan"`) -> defaults port to `8787`
    /// - Port shorthand (`"9000"`) -> `"http://127.0.0.1:9000"`
    /// - Explicit HTTP URL (`"http://192.168.1.50:8787"`, `"http://192.168.1.50:8787/v1"`)
    pub fn try_new(raw: impl AsRef<str>) -> Result<Self, ValidationError> {
        let raw_ref = raw.as_ref();
        let trimmed = raw_ref.trim();
        if trimmed.is_empty() {
            return Err(ValidationError::InvalidGatewayEndpoint {
                value: raw_ref.to_owned(),
                source: GatewayEndpointError::Empty,
            });
        }
        if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(ValidationError::InvalidGatewayEndpoint {
                value: trimmed.to_owned(),
                source: GatewayEndpointError::WhitespaceOrControl,
            });
        }

        if trimmed.bytes().all(|b| b.is_ascii_digit()) {
            let port = trimmed
                .parse::<u16>()
                .ok()
                .and_then(NonZeroU16::new)
                .ok_or_else(|| ValidationError::InvalidGatewayEndpoint {
                    value: trimmed.to_owned(),
                    source: GatewayEndpointError::InvalidPort,
                })?;
            return Ok(Self::from_validated_parts("127.0.0.1", false, port));
        }

        let authority = if let Some((scheme_raw, rest_raw)) = trimmed.split_once("://") {
            if !scheme_raw.eq_ignore_ascii_case("http") {
                return Err(ValidationError::InvalidGatewayEndpoint {
                    value: trimmed.to_owned(),
                    source: GatewayEndpointError::UnsupportedScheme,
                });
            }
            strip_optional_v1_path(rest_raw, trimmed)?
        } else {
            strip_optional_v1_path(trimmed, trimmed)?
        };

        if authority.contains('@') {
            return Err(ValidationError::InvalidGatewayEndpoint {
                value: trimmed.to_owned(),
                source: GatewayEndpointError::ContainsCredentials,
            });
        }
        if authority.is_empty() {
            return Err(ValidationError::InvalidGatewayEndpoint {
                value: trimmed.to_owned(),
                source: GatewayEndpointError::EmptyHost,
            });
        }

        let (raw_host, is_ipv6, port_opt) = if let Some(after_bracket) = authority.strip_prefix('[')
        {
            let Some((ipv6_host, tail)) = after_bracket.split_once(']') else {
                return Err(ValidationError::InvalidGatewayEndpoint {
                    value: trimmed.to_owned(),
                    source: GatewayEndpointError::UnclosedIpv6Bracket,
                });
            };
            if ipv6_host.is_empty() {
                return Err(ValidationError::InvalidGatewayEndpoint {
                    value: trimmed.to_owned(),
                    source: GatewayEndpointError::EmptyIpv6Host,
                });
            }
            let port_str = if tail.is_empty() {
                None
            } else if let Some(p) = tail.strip_prefix(':') {
                Some(p)
            } else {
                return Err(ValidationError::InvalidGatewayEndpoint {
                    value: trimmed.to_owned(),
                    source: GatewayEndpointError::UnexpectedAfterIpv6Bracket,
                });
            };
            (ipv6_host, true, port_str)
        } else if let Some((h, p)) = authority.rsplit_once(':') {
            if h.contains(':') {
                return Err(ValidationError::InvalidGatewayEndpoint {
                    value: trimmed.to_owned(),
                    source: GatewayEndpointError::UnbracketedIpv6Host,
                });
            }
            (h, false, Some(p))
        } else {
            (authority, false, None)
        };

        if raw_host.is_empty() {
            return Err(ValidationError::InvalidGatewayEndpoint {
                value: trimmed.to_owned(),
                source: GatewayEndpointError::EmptyHost,
            });
        }

        let port = match port_opt {
            Some(p_str) => p_str
                .parse::<u16>()
                .ok()
                .and_then(NonZeroU16::new)
                .ok_or_else(|| ValidationError::InvalidGatewayEndpoint {
                    value: trimmed.to_owned(),
                    source: GatewayEndpointError::InvalidPort,
                })?,
            None => NonZeroU16::new(DEFAULT_GATEWAY_PORT)
                .expect("DEFAULT_GATEWAY_PORT is a non-zero compile-time constant"),
        };

        let (norm_host, norm_ipv6) = normalize_connectable_host(raw_host, is_ipv6);
        Ok(Self::from_validated_parts(&norm_host, norm_ipv6, port))
    }

    /// Builds a [`GatewayEndpoint`] from a local or remote [`SocketAddr`], mapping unspecified
    /// bind addresses (`0.0.0.0` / `::`) to loopback (`127.0.0.1` / `::1`).
    #[must_use]
    pub fn from_socket_addr(addr: SocketAddr) -> Self {
        let port = NonZeroU16::new(addr.port()).unwrap_or_else(|| {
            NonZeroU16::new(DEFAULT_GATEWAY_PORT)
                .expect("DEFAULT_GATEWAY_PORT is a non-zero compile-time constant")
        });
        match addr {
            SocketAddr::V4(v4) => {
                let ip = if v4.ip().is_unspecified() {
                    Ipv4Addr::LOCALHOST
                } else {
                    *v4.ip()
                };
                Self::from_validated_parts(&ip.to_string(), false, port)
            }
            SocketAddr::V6(v6) => {
                let ip = if v6.ip().is_unspecified() {
                    Ipv6Addr::LOCALHOST
                } else {
                    *v6.ip()
                };
                Self::from_validated_parts(&ip.to_string(), true, port)
            }
        }
    }

    fn from_validated_parts(host: &str, is_ipv6: bool, port: NonZeroU16) -> Self {
        let p = port.get();
        let host_header = if is_ipv6 {
            format!("[{host}]:{p}")
        } else {
            format!("{host}:{p}")
        };
        let base_url = format!("http://{host_header}");
        Self {
            base_url: Arc::from(base_url),
            host: Arc::from(host),
            host_header: Arc::from(host_header),
            port,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.base_url
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Unbracketed hostname or IP literal suitable for `TcpStream::connect((host, port))` and `NO_PROXY`.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Authority string (`host:port` or `[ipv6]:port`) sent in HTTP/1.1 `Host` headers.
    #[must_use]
    pub fn host_header(&self) -> &str {
        &self.host_header
    }

    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port.get()
    }

    /// Returns `Some(SocketAddr)` when `host` is a literal IPv4 or IPv6 address.
    #[must_use]
    pub fn socket_addr(&self) -> Option<SocketAddr> {
        let ip: IpAddr = self.host.parse().ok()?;
        Some(SocketAddr::new(ip, self.port.get()))
    }
}

fn strip_optional_v1_path<'a>(rest: &'a str, original: &str) -> Result<&'a str, ValidationError> {
    let without_trailing = rest.strip_suffix('/').unwrap_or(rest);
    if let Some((authority, path)) = without_trailing.split_once('/') {
        if path.eq_ignore_ascii_case("v1") {
            Ok(authority)
        } else {
            Err(ValidationError::InvalidGatewayEndpoint {
                value: original.to_owned(),
                source: GatewayEndpointError::ContainsPath,
            })
        }
    } else {
        Ok(without_trailing)
    }
}

fn normalize_connectable_host(raw_host: &str, is_ipv6: bool) -> (String, bool) {
    if is_ipv6 {
        if let Ok(v6) = raw_host.parse::<Ipv6Addr>() {
            let norm = if v6.is_unspecified() {
                Ipv6Addr::LOCALHOST
            } else {
                v6
            };
            return (norm.to_string(), true);
        }
        return (raw_host.to_owned(), true);
    }
    if let Ok(v4) = raw_host.parse::<Ipv4Addr>() {
        let norm = if v4.is_unspecified() {
            Ipv4Addr::LOCALHOST
        } else {
            v4
        };
        return (norm.to_string(), false);
    }
    (raw_host.to_owned(), false)
}

impl fmt::Display for GatewayEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.base_url)
    }
}

impl TryFrom<String> for GatewayEndpoint {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<GatewayEndpoint> for String {
    fn from(value: GatewayEndpoint) -> Self {
        value.base_url.to_string()
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

    #[test]
    fn auto_update_target_and_interval_invariants_hold() {
        assert!(AutoUpdateTarget::Liftgate.default_enabled());
        assert!(AutoUpdateTarget::Agy.default_enabled());
        assert!(!AutoUpdateTarget::Codex.default_enabled());
        assert!(!AutoUpdateTarget::Claude.default_enabled());
        assert!(!AutoUpdateTarget::Opencode.default_enabled());
        assert!(!AutoUpdateTarget::Omp.default_enabled());
        assert_eq!(
            AutoUpdateTarget::from_slug("LG"),
            Some(AutoUpdateTarget::Liftgate)
        );
        assert_eq!(
            AutoUpdateTarget::from_slug("cx"),
            Some(AutoUpdateTarget::Codex)
        );
        assert_eq!(
            AutoUpdateTarget::from_slug("cc"),
            Some(AutoUpdateTarget::Claude)
        );
        assert_eq!(
            AutoUpdateTarget::from_slug("oc"),
            Some(AutoUpdateTarget::Opencode)
        );
        assert_eq!(
            AutoUpdateTarget::from_slug("oh-my-pi"),
            Some(AutoUpdateTarget::Omp)
        );
        assert_eq!(AutoUpdateTarget::from_slug("unknown"), None);

        let def = AutoUpdateIntervalMins::default();
        assert_eq!(def.get(), 30);
        assert_eq!(def.step_prev().get(), 15);
        assert_eq!(def.step_next().get(), 60);
        assert!(AutoUpdateIntervalMins::try_new(0).is_err());
        assert!(AutoUpdateIntervalMins::try_new(4).is_err());
        assert!(AutoUpdateIntervalMins::try_new(1_441).is_err());
        assert_eq!(AutoUpdateIntervalMins::try_new(5).unwrap().get(), 5);
        assert_eq!(AutoUpdateIntervalMins::try_new(1_440).unwrap().get(), 1_440);
    }

    #[test]
    fn low_cache_limits_and_cache_hit_bps_validate_bounds_and_round_accurately() {
        use crate::acp::{AcpUsage, compute_cache_hit_bps};

        assert_eq!(LowCacheRecordLimit::default().get(), 1_000);
        assert_eq!(LowCacheRecordLimit::try_new(0).unwrap().get(), 0);
        assert_eq!(LowCacheRecordLimit::try_new(50_000).unwrap().get(), 50_000);
        assert!(LowCacheRecordLimit::try_new(50_001).is_err());

        assert_eq!(CacheHitThresholdBps::default().get(), 5_000);
        assert_eq!(CacheHitThresholdBps::try_from_percent(0).unwrap().get(), 0);
        assert_eq!(
            CacheHitThresholdBps::try_from_percent(85).unwrap().get(),
            8_500
        );
        assert_eq!(
            CacheHitThresholdBps::try_from_percent(100).unwrap().get(),
            10_000
        );
        assert!(CacheHitThresholdBps::try_from_percent(101).is_err());
        assert!(CacheHitThresholdBps::try_new(10_001).is_err());

        assert_eq!(ContextCaptureRecordLimit::default().get(), 200);
        assert!(ContextCaptureRecordLimit::try_new(0).is_err());
        assert_eq!(ContextCaptureRecordLimit::try_new(1).unwrap().get(), 1);
        assert!(ContextCaptureRecordLimit::try_new(10_001).is_err());
        assert_eq!(ContextCaptureDiskMib::default().bytes(), 512 * 1024 * 1024);
        assert!(ContextCaptureDiskMib::try_new(15).is_err());
        assert_eq!(ContextCaptureDiskMib::try_new(16).unwrap().get(), 16);
        assert!(ContextCaptureDiskMib::try_new(64 * 1024 + 1).is_err());
        let decoded: ContextCaptureDiskMib = serde_json::from_str("2048").unwrap();
        assert_eq!(decoded.bytes(), 2 * 1024 * 1024 * 1024);
        assert!(serde_json::from_str::<ContextCaptureRecordLimit>("0").is_err());

        assert_eq!(compute_cache_hit_bps(0, 0), None);
        assert_eq!(compute_cache_hit_bps(0, 2_000), Some(0));
        assert_eq!(compute_cache_hit_bps(1_800, 2_000), Some(9_000));
        assert_eq!(compute_cache_hit_bps(2_500, 2_000), Some(10_000));

        let usage = AcpUsage {
            input_tokens: 300,
            cached_read_tokens: 1_500,
            cached_write_tokens: 200,
            output_tokens: 120,
            thought_tokens: 40,
            total_tokens: 2_120,
        };
        assert_eq!(usage.total_prompt_tokens(), 2_000);
        assert_eq!(usage.cache_hit_bps(), Some(7_500));
    }

    #[test]
    fn gateway_endpoint_and_target_mode_parse_ipv4_ipv6_dns_and_forward_unknown_modes() {
        let ep_v4 = GatewayEndpoint::try_new("192.168.1.50:8787").unwrap();
        assert_eq!(ep_v4.base_url(), "http://192.168.1.50:8787");
        assert_eq!(ep_v4.host(), "192.168.1.50");
        assert_eq!(ep_v4.host_header(), "192.168.1.50:8787");
        assert_eq!(ep_v4.port(), 8787);
        assert_eq!(ep_v4.socket_addr(), "192.168.1.50:8787".parse().ok());

        let ep_default_port = GatewayEndpoint::try_new("http://192.168.1.50/v1/").unwrap();
        assert_eq!(ep_default_port.base_url(), "http://192.168.1.50:8787");

        let ep_port_only = GatewayEndpoint::try_new("9000").unwrap();
        assert_eq!(ep_port_only.base_url(), "http://127.0.0.1:9000");

        let ep_dns = GatewayEndpoint::try_new("devbox.tailnet.ts.net:8899").unwrap();
        assert_eq!(ep_dns.base_url(), "http://devbox.tailnet.ts.net:8899");
        assert_eq!(ep_dns.host(), "devbox.tailnet.ts.net");
        assert_eq!(ep_dns.socket_addr(), None);

        let ep_v6 = GatewayEndpoint::try_new("http://[fd00::42]:8787/v1").unwrap();
        assert_eq!(ep_v6.base_url(), "http://[fd00::42]:8787");
        assert_eq!(ep_v6.host(), "fd00::42");
        assert_eq!(ep_v6.host_header(), "[fd00::42]:8787");
        assert_eq!(ep_v6.socket_addr(), "[fd00::42]:8787".parse().ok());

        let wildcard_v4 = GatewayEndpoint::from_socket_addr("0.0.0.0:8787".parse().unwrap());
        assert_eq!(wildcard_v4.base_url(), "http://127.0.0.1:8787");
        let wildcard_v6 = GatewayEndpoint::from_socket_addr("[::]:8787".parse().unwrap());
        assert_eq!(wildcard_v6.base_url(), "http://[::1]:8787");

        assert!(GatewayEndpoint::try_new("").is_err());
        assert!(GatewayEndpoint::try_new("0").is_err());
        assert!(GatewayEndpoint::try_new("https://192.168.1.50:8787").is_err());
        assert!(GatewayEndpoint::try_new("http://user:pass@192.168.1.50:8787").is_err());
        assert!(GatewayEndpoint::try_new("http://192.168.1.50:8787/custom/path").is_err());
        assert!(GatewayEndpoint::try_new("fd00::42:8787").is_err());

        assert!(is_endpoint_local_keyword("local"));
        assert!(is_endpoint_local_keyword("OFF"));
        assert!(!is_endpoint_local_keyword("192.168.1.50"));

        let unknown_mode: GatewayTargetMode = serde_json::from_str("\"future_cluster\"").unwrap();
        assert_eq!(unknown_mode, GatewayTargetMode::Unknown);
        assert!(!unknown_mode.is_remote());
        assert_eq!(unknown_mode.as_str(), "local");
    }
}
