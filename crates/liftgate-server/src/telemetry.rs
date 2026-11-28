use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, RwLock,
    atomic::{AtomicU32, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

use liftgate_tool_virtualizer::strip_call_prefix;
use liftgate_types::{
    acp::{AcpPromptBlock, AcpUsage, AvailableModel, compute_cache_hit_bps},
    canonical::{
        AuxiliaryTaskKind, CanonicalMessage, CanonicalPart, CanonicalRole, CanonicalToolKind,
        CanonicalToolSpec, ProtocolDeveloperSlot, ReasoningEffort,
    },
    config::{
        CacheHitThresholdBps, ContextCaptureDiskMib, ContextCaptureRecordLimit, LowCacheRecordLimit,
    },
    harness_prompt::{DetectedHarness, explain_developer_slot_rewrites},
    pricing::{
        ModelPrice, PricingResponse, UsageResponse, UsageSeriesResponse, UsageSummary, UsageTokens,
    },
    provider::OptimizationMode,
    session_key::{WebToolBridgeConfig, truncate_head_tail_lines},
    telemetry::{
        AutoUpdateTelemetrySummary, CacheTelemetrySummary, CapturedAcpPromptBlock,
        CapturedFullMessage, CapturedFullPart, CapturedMessageSummary, CapturedToolSpec,
        ContextCaptureDetailResponse, ContextCaptureRecord, ContextCaptureSummary,
        ContextCapturesResponse, ContextRewrite, ContextRewriteKind, ContextSegment,
        ContextSegmentKey, ElasticIoTelemetry, FullContextPayload, GatewayMetricsSummary,
        LowCacheDiagnosticCause, LowCacheFullRequestPayload, LowCacheHitReason, LowCacheHitRecord,
        LowCacheHitsResponse, LowCachePayloadDetailResponse, LowCacheRequestCapture,
        LowCacheRequestMetrics, LowCacheRoutingStrategy, TaskPhase, TaskProtocol, TaskSnapshot,
        ToolCallDetail, ToolCallTelemetrySummary, ToolCallsResponse, UpstreamWireSummary,
    },
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, watch};
use uuid::Uuid;

use crate::context_capture::{
    CaptureCommit, CaptureTicket, ContextCaptureConfig, ContextCaptureIoError, ContextCaptureStore,
    encode_payload, payload_file_name, read_payload,
};
use crate::provider_runtime::ProviderRuntimeRegistry;
use crate::tool_calls::{
    ToolCallLimits, ToolCallOutcome, ToolCallStart, ToolCallStore, ToolCallTaskContext,
};
use crate::usage_ledger::{PreparedLedger, PriceBook, PriceUpdate, UsageLedger};

const MAX_RECENT_TASKS: usize = 100;
const MAX_PROMPT_PREVIEW_CHARS: usize = 180;
const MAX_OUTPUT_PREVIEW_CHARS: usize = 240;
const PERSISTED_CACHE_SCHEMA_VERSION: u32 = 1;
const MAX_INLINE_SYSTEM_APPEND_BYTES: usize = 16 * 1024;
const MAX_INLINE_TOOLS_DELTA_BYTES: usize = 8 * 1024;
const MAX_INLINE_OUTBOUND_PROMPT_BYTES: usize = 24 * 1024;
const MAX_INLINE_MESSAGE_DIGESTS: usize = 24;
const MAX_INLINE_MESSAGE_EXCERPT_CHARS: usize = 600;
/// Bounds applied while materializing a request payload for disk.
#[derive(Clone, Copy, Debug)]
struct PayloadBudget {
    per_part: usize,
    total: usize,
    /// Largest single base64 image kept as a preview.
    per_image: usize,
    /// Base64 image bytes kept per payload, separate from the text budget.
    images_total: usize,
}

const LOW_CACHE_PAYLOAD_BUDGET: PayloadBudget = PayloadBudget {
    per_part: 256 * 1024,
    total: 2 * 1024 * 1024,
    per_image: 256 * 1024,
    images_total: 1024 * 1024,
};
const CONTEXT_CAPTURE_PAYLOAD_BUDGET: PayloadBudget = PayloadBudget {
    per_part: 1024 * 1024,
    total: 16 * 1024 * 1024,
    per_image: 2 * 1024 * 1024,
    images_total: 8 * 1024 * 1024,
};
/// Bounds for the before / after texts kept per gateway context rewrite.
const CONTEXT_REWRITE_BUDGET: PayloadBudget = PayloadBudget {
    per_part: 256 * 1024,
    total: 4 * 1024 * 1024,
    per_image: 0,
    images_total: 0,
};
/// Reasoning / output text retained per turn for full-context capture.
const MAX_CAPTURED_TURN_TEXT_BYTES: usize = 1024 * 1024;
/// Sessions whose latest context composition is kept for the tool catalog view.
const MAX_SESSION_CONTEXT_SAMPLES: usize = 256;
/// Rough token cost charged per image when estimating context composition.
const IMAGE_TOKENS_ESTIMATE: u64 = 1_000;

const _: () = assert!(
    LOW_CACHE_PAYLOAD_BUDGET.per_part <= LOW_CACHE_PAYLOAD_BUDGET.total
        && CONTEXT_CAPTURE_PAYLOAD_BUDGET.per_part <= CONTEXT_CAPTURE_PAYLOAD_BUDGET.total
        && CONTEXT_REWRITE_BUDGET.per_part <= CONTEXT_REWRITE_BUDGET.total
        && LOW_CACHE_PAYLOAD_BUDGET.per_image <= LOW_CACHE_PAYLOAD_BUDGET.images_total
        && CONTEXT_CAPTURE_PAYLOAD_BUDGET.per_image <= CONTEXT_CAPTURE_PAYLOAD_BUDGET.images_total,
    "a single payload part must fit inside its payload budget"
);

const fn default_persisted_schema_version() -> u32 {
    PERSISTED_CACHE_SCHEMA_VERSION
}

const fn default_true() -> bool {
    true
}

/// Configuration for cumulative prompt-cache accounting and persisted low-cache-hit diagnostics.
#[derive(Clone, Debug)]
pub struct CacheTelemetryConfig {
    /// Optional path to `~/.liftgate/cache-telemetry.json` for durable persistence across restarts.
    pub persist_path: Option<PathBuf>,
    /// Optional path to `~/.liftgate/settings.json` so live Web UI config edits survive restarts.
    pub settings_path: Option<PathBuf>,
    /// Maximum number of low-cache-hit records retained (`1_000` by default; `0` disables recording).
    pub max_low_cache_records: LowCacheRecordLimit,
    /// Basis-point threshold (`5_000` = `50.00%` by default) below which a turn is recorded.
    pub low_cache_threshold_bps: CacheHitThresholdBps,
    /// Persist full request payload details & message breakdown for low-cache turns (`true` by default).
    pub capture_request_details: bool,
}

impl Default for CacheTelemetryConfig {
    fn default() -> Self {
        Self {
            persist_path: None,
            settings_path: None,
            max_low_cache_records: LowCacheRecordLimit::default(),
            low_cache_threshold_bps: CacheHitThresholdBps::default(),
            capture_request_details: true,
        }
    }
}

/// Complete [`TelemetryHub`] configuration.
#[derive(Clone, Debug, Default)]
pub struct TelemetryConfig {
    pub cache: CacheTelemetryConfig,
    /// Bounds of the per-call tool I/O ring buffer (enabled by default).
    pub tool_calls: ToolCallLimits,
    /// Opt-in full-context capture (disabled by default).
    pub context_capture: ContextCaptureConfig,
    /// Where custom model prices persist (`~/.liftgate/model-pricing.json`); `None` keeps them in
    /// memory only.
    pub pricing_path: Option<PathBuf>,
    /// Where per-model usage and its quarter-hour history persist
    /// (`~/.liftgate/usage-ledger.json`); `None` keeps them in memory only.
    pub usage_path: Option<PathBuf>,
}

/// Latest context composition observed for a session, used by the tool catalog view.
#[derive(Clone, Debug)]
pub struct SessionContextSample {
    pub model_id: String,
    pub captured_at_unix_ms: u64,
    /// Message-derived segments plus `<client_tools_update>`; wire-tool and bridge-prompt
    /// segments are added from the live MCP handle when the catalog is built.
    pub segments: Vec<ContextSegment>,
}

/// Borrowed zero-copy turn context passed to [`ActiveTaskGuard::complete_with_diagnostics`]
/// so full request metrics and payloads are materialized ONLY when a turn actually falls
/// below the low-cache-hit threshold.
pub struct TurnCompletionDiagnostics<'a> {
    pub routing_strategy: LowCacheRoutingStrategy,
    pub delta_start_index: usize,
    pub session_turn_index: u64,
    pub session_idle_ms: Option<u64>,
    pub previous_session_context_tokens: u64,
    pub model_switched: bool,
    pub previous_model: Option<&'a str>,
    pub previous_response_id: Option<&'a str>,
    pub prefix_hash_hex: Option<&'a str>,
    pub base_state_hash_hex: &'a str,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub web_cfg: WebToolBridgeConfig,
    pub acp_system_append: Option<&'a str>,
    pub tools_delta_xml: Option<&'a str>,
    pub outbound_acp_blocks: &'a [AcpPromptBlock],
    pub messages: &'a [CanonicalMessage],
    pub client_tools: &'a [CanonicalToolSpec],
    /// Harness whose developer slots were distilled on the way out; `None` forwards them verbatim.
    pub distill_harness: Option<DetectedHarness>,
    /// Protocol developer slots indexing into `messages`.
    pub developer_slots: &'a [ProtocolDeveloperSlot],
}

impl TurnCompletionDiagnostics<'_> {
    fn clamped_delta_start(&self) -> usize {
        self.delta_start_index.min(self.messages.len())
    }
}

#[derive(Debug, Error)]
enum CachePersistError {
    #[error("failed to create cache telemetry directory `{}`", path.display())]
    CreateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to serialize cache telemetry state `{}`", path.display())]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("failed to write temporary cache telemetry file `{}`", path.display())]
    WriteTemp {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to atomically persist cache telemetry file `{}`", path.display())]
    Rename {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

struct TempUnlinkGuard {
    path: Option<PathBuf>,
}

impl TempUnlinkGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempUnlinkGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct PersistedCacheTelemetryFile {
    #[serde(default = "default_persisted_schema_version")]
    version: u32,
    #[serde(default)]
    updated_at_unix_ms: u64,
    #[serde(default)]
    max_low_cache_records: usize,
    #[serde(default)]
    low_cache_threshold_bps: u16,
    #[serde(default = "default_true")]
    capture_request_details: bool,
    #[serde(default)]
    total_prompt_tokens: u64,
    #[serde(default)]
    total_uncached_input_tokens: u64,
    #[serde(default)]
    total_cached_read_tokens: u64,
    #[serde(default)]
    total_cached_write_tokens: u64,
    #[serde(default)]
    warm_prompt_tokens: u64,
    #[serde(default)]
    warm_cached_read_tokens: u64,
    #[serde(default)]
    evaluated_turns: u64,
    #[serde(default)]
    warm_evaluated_turns: u64,
    #[serde(default)]
    total_low_cache_turns: u64,
    #[serde(default)]
    auxiliary_turns: u64,
    #[serde(default)]
    auxiliary_prompt_tokens: u64,
    #[serde(default)]
    low_cache_records: VecDeque<LowCacheHitRecord>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

struct CacheTelemetryStore {
    persist_path: Option<PathBuf>,
    payload_dir: Option<PathBuf>,
    settings_path: Option<PathBuf>,
    max_low_cache_records: LowCacheRecordLimit,
    low_cache_threshold_bps: CacheHitThresholdBps,
    capture_request_details: bool,
    revision: u64,
    persist_seq: u64,
    file_state: PersistedCacheTelemetryFile,
}

impl CacheTelemetryStore {
    fn from_config(config: CacheTelemetryConfig) -> Self {
        let payload_dir = config
            .persist_path
            .as_deref()
            .and_then(Path::parent)
            .map(|parent| parent.join("low-cache-payloads"));
        let mut file_state = config
            .persist_path
            .as_deref()
            .and_then(load_persisted_cache_file)
            .unwrap_or_default();

        file_state.version = PERSISTED_CACHE_SCHEMA_VERSION;
        file_state.max_low_cache_records = config.max_low_cache_records.get();
        file_state.low_cache_threshold_bps = config.low_cache_threshold_bps.get();
        file_state.capture_request_details = config.capture_request_details;
        let evicted = trim_records_to_cap(
            &mut file_state.low_cache_records,
            config.max_low_cache_records.get(),
        );
        remove_payload_files(payload_dir.as_deref(), &evicted);
        prune_orphan_payload_files(payload_dir.as_deref(), &file_state.low_cache_records);

        Self {
            persist_path: config.persist_path,
            payload_dir,
            settings_path: config.settings_path,
            max_low_cache_records: config.max_low_cache_records,
            low_cache_threshold_bps: config.low_cache_threshold_bps,
            capture_request_details: config.capture_request_details,
            revision: 1,
            persist_seq: 0,
            file_state,
        }
    }

    fn summary(&self) -> CacheTelemetrySummary {
        let f = &self.file_state;
        CacheTelemetrySummary {
            revision: self.revision,
            max_low_cache_records: self.max_low_cache_records.get(),
            low_cache_threshold_bps: self.low_cache_threshold_bps.get(),
            capture_request_details: self.capture_request_details,
            persisted_path: self
                .persist_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            payload_dir: self
                .payload_dir
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            total_prompt_tokens: f.total_prompt_tokens,
            total_uncached_input_tokens: f.total_uncached_input_tokens,
            total_cached_read_tokens: f.total_cached_read_tokens,
            total_cached_write_tokens: f.total_cached_write_tokens,
            total_cache_hit_rate_bps: compute_cache_hit_bps(
                f.total_cached_read_tokens,
                f.total_prompt_tokens,
            ),
            warm_prompt_tokens: f.warm_prompt_tokens,
            warm_cached_read_tokens: f.warm_cached_read_tokens,
            warm_cache_hit_rate_bps: compute_cache_hit_bps(
                f.warm_cached_read_tokens,
                f.warm_prompt_tokens,
            ),
            evaluated_turns: f.evaluated_turns,
            warm_evaluated_turns: f.warm_evaluated_turns,
            total_low_cache_turns: f.total_low_cache_turns,
            stored_low_cache_count: f.low_cache_records.len(),
            auxiliary_turns: f.auxiliary_turns,
            auxiliary_prompt_tokens: f.auxiliary_prompt_tokens,
        }
    }

    fn prepare_persist_payload(
        &mut self,
        now_ms: u64,
    ) -> Option<(PathBuf, u64, PersistedCacheTelemetryFile)> {
        let path = self.persist_path.clone()?;
        self.persist_seq = self.persist_seq.saturating_add(1);
        self.file_state.version = PERSISTED_CACHE_SCHEMA_VERSION;
        self.file_state.updated_at_unix_ms = now_ms;
        self.file_state.max_low_cache_records = self.max_low_cache_records.get();
        self.file_state.low_cache_threshold_bps = self.low_cache_threshold_bps.get();
        self.file_state.capture_request_details = self.capture_request_details;
        Some((path, self.persist_seq, self.file_state.clone()))
    }
}

fn trim_records_to_cap(
    records: &mut VecDeque<LowCacheHitRecord>,
    max_records: usize,
) -> Vec<LowCacheHitRecord> {
    let mut evicted = Vec::new();
    while records.len() > max_records {
        if let Some(old) = records.pop_front() {
            evicted.push(old);
        } else {
            break;
        }
    }
    evicted
}

fn load_persisted_cache_file(path: &Path) -> Option<PersistedCacheTelemetryFile> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = ?err,
                "failed to read persisted cache telemetry file; starting with empty store"
            );
            return None;
        }
    };
    match serde_json::from_slice::<PersistedCacheTelemetryFile>(&bytes) {
        Ok(parsed) => Some(parsed),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = ?err,
                "corrupt persisted cache telemetry JSON; starting with empty store"
            );
            None
        }
    }
}

fn write_bytes_atomic_private(target: &Path, bytes: &[u8]) -> Result<(), CachePersistError> {
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| CachePersistError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }

    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("cache-telemetry.json");
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}", Uuid::new_v4().simple()));
    let mut temp_guard = TempUnlinkGuard::new(tmp_path.clone());

    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }

    let mut file = opts
        .open(&tmp_path)
        .map_err(|source| CachePersistError::WriteTemp {
            path: tmp_path.clone(),
            source,
        })?;
    file.write_all(bytes)
        .map_err(|source| CachePersistError::WriteTemp {
            path: tmp_path.clone(),
            source,
        })?;
    file.sync_all()
        .map_err(|source| CachePersistError::WriteTemp {
            path: tmp_path.clone(),
            source,
        })?;
    drop(file);

    fs::rename(&tmp_path, target).map_err(|source| CachePersistError::Rename {
        path: target.to_path_buf(),
        source,
    })?;
    temp_guard.disarm();
    Ok(())
}

fn persist_cache_file_atomic(
    target: &Path,
    payload: &PersistedCacheTelemetryFile,
) -> Result<(), CachePersistError> {
    let mut json =
        serde_json::to_vec_pretty(payload).map_err(|source| CachePersistError::Serialize {
            path: target.to_path_buf(),
            source,
        })?;
    json.push(b'\n');
    write_bytes_atomic_private(target, &json)
}

/// Atomically updates a single boolean field inside `settings.json` (creating the file if absent)
/// while preserving all existing settings keys, and synchronizes any active sibling `daemon.json`
/// owned by the current process (`pid == std::process::id()`).
pub fn sync_settings_json_bool_field(settings_path: &Path, key: &str, value: bool) {
    sync_settings_json_fields(settings_path, &[(key, serde_json::Value::from(value))]);
}

/// Atomically sets `fields` inside `settings.json` (creating the file if absent) while preserving
/// all other keys, and mirrors them into a sibling `daemon.json` owned by this process.
pub fn sync_settings_json_fields(settings_path: &Path, fields: &[(&str, serde_json::Value)]) {
    let mut map: serde_json::Map<String, serde_json::Value> = match fs::read(settings_path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(err) => {
            tracing::warn!(
                path = %settings_path.display(),
                error = ?err,
                "failed to read settings.json while syncing runtime settings"
            );
            return;
        }
    };
    for (key, value) in fields {
        map.insert((*key).to_owned(), value.clone());
    }
    match serde_json::to_vec_pretty(&map) {
        Ok(mut json) => {
            json.push(b'\n');
            if let Err(err) = write_bytes_atomic_private(settings_path, &json) {
                tracing::warn!(
                    path = %settings_path.display(),
                    error = ?err,
                    "failed to persist updated runtime settings to settings.json"
                );
            }
        }
        Err(err) => {
            tracing::warn!(
                path = %settings_path.display(),
                error = ?err,
                "failed to serialize updated settings.json"
            );
        }
    }

    if let Some(parent) = settings_path.parent() {
        let daemon_path = parent.join("daemon.json");
        if let Ok(daemon_bytes) = fs::read(&daemon_path)
            && let Ok(mut daemon_map) =
                serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&daemon_bytes)
            && daemon_map.get("pid").and_then(serde_json::Value::as_u64)
                == Some(u64::from(std::process::id()))
        {
            for (key, value) in fields {
                daemon_map.insert((*key).to_owned(), value.clone());
            }
            if let Ok(mut daemon_json) = serde_json::to_vec_pretty(&daemon_map) {
                daemon_json.push(b'\n');
                if let Err(err) = write_bytes_atomic_private(&daemon_path, &daemon_json) {
                    tracing::warn!(
                        path = %daemon_path.display(),
                        error = ?err,
                        "failed to persist updated runtime settings to daemon.json"
                    );
                }
            }
        }
    }
}

fn remove_capture_files(dir: Option<&Path>, records: &[ContextCaptureRecord]) {
    let Some(dir) = dir else {
        return;
    };
    for record in records {
        if let Some(name) = payload_file_name(&record.task_id) {
            let _ = fs::remove_file(dir.join(name));
        }
    }
}

fn sync_settings_json_cache_fields(
    settings_path: &Path,
    max_low_cache_records: LowCacheRecordLimit,
    low_cache_threshold_bps: CacheHitThresholdBps,
    capture_request_details: bool,
) {
    let mut map: serde_json::Map<String, serde_json::Value> = match fs::read(settings_path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(err) => {
            tracing::warn!(
                path = %settings_path.display(),
                error = ?err,
                "failed to read settings.json while syncing cache telemetry config"
            );
            return;
        }
    };
    map.insert(
        "max_low_cache_records".to_owned(),
        serde_json::Value::from(max_low_cache_records.get()),
    );
    map.insert(
        "low_cache_threshold_bps".to_owned(),
        serde_json::Value::from(low_cache_threshold_bps.get()),
    );
    map.insert(
        "capture_low_cache_request_details".to_owned(),
        serde_json::Value::from(capture_request_details),
    );
    match serde_json::to_vec_pretty(&map) {
        Ok(mut json) => {
            json.push(b'\n');
            if let Err(err) = write_bytes_atomic_private(settings_path, &json) {
                tracing::warn!(
                    path = %settings_path.display(),
                    error = ?err,
                    "failed to persist updated cache config to settings.json"
                );
            }
        }
        Err(err) => {
            tracing::warn!(
                path = %settings_path.display(),
                error = ?err,
                "failed to serialize updated settings.json"
            );
        }
    }
}

/// Writes usage ledger snapshots off the async runtime, one at a time. Only the newest snapshot
/// waits behind a running write, so a slow disk never piles up copies of the ledger, and an older
/// snapshot never replaces a newer file.
#[derive(Debug, Default)]
struct UsageWriter {
    state: Mutex<UsageWriterState>,
}

#[derive(Debug, Default)]
struct UsageWriterState {
    waiting: Option<PreparedLedger>,
    running: bool,
    written_seq: u64,
}

impl UsageWriter {
    fn lock_state(&self) -> std::sync::MutexGuard<'_, UsageWriterState> {
        match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    fn submit(self: &Arc<Self>, prepared: PreparedLedger) {
        {
            let mut state = self.lock_state();
            if prepared.seq <= state.written_seq
                || state
                    .waiting
                    .as_ref()
                    .is_some_and(|waiting| waiting.seq > prepared.seq)
            {
                return;
            }
            state.waiting = Some(prepared);
            if state.running {
                return;
            }
            state.running = true;
        }
        let writer = Arc::clone(self);
        let job = move || writer.drain();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => drop(handle.spawn_blocking(job)),
            Err(_) => job(),
        }
    }

    fn drain(&self) {
        loop {
            let next = {
                let mut state = self.lock_state();
                match state.waiting.take() {
                    Some(next) if next.seq > state.written_seq => next,
                    Some(_) => continue,
                    None => {
                        state.running = false;
                        return;
                    }
                }
            };
            let written = match next.to_bytes() {
                Ok(bytes) => match write_bytes_atomic_private(&next.path, &bytes) {
                    Ok(()) => true,
                    Err(err) => {
                        tracing::warn!(
                            path = %next.path.display(),
                            error = ?err,
                            "failed to persist usage ledger"
                        );
                        false
                    }
                },
                Err(err) => {
                    tracing::warn!(error = ?err, "failed to serialize usage ledger");
                    false
                }
            };
            if written {
                let mut state = self.lock_state();
                state.written_seq = state.written_seq.max(next.seq);
            }
        }
    }
}

/// Central bounded telemetry store and real-time event notifier for the companion web UI.
pub struct TelemetryHub {
    inner: Mutex<TelemetryInner>,
    providers: RwLock<Arc<ProviderRuntimeRegistry>>,
    persist_lock: Mutex<u64>,
    context_persist_lock: Mutex<u64>,
    pricing_persist_lock: Mutex<u64>,
    usage_writer: Arc<UsageWriter>,
    /// One permit per full-context capture allowed to wait for, or be in, its disk write.
    context_write_slots: Arc<Semaphore>,
    settings_path: Option<PathBuf>,
    notify_tx: broadcast::Sender<()>,
    shutdown_tx: watch::Sender<bool>,
}

impl fmt::Debug for TelemetryHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelemetryHub").finish_non_exhaustive()
    }
}

struct TelemetryInner {
    metrics: GatewayMetricsSummary,
    elastic_io: ElasticIoTelemetry,
    cache_store: CacheTelemetryStore,
    tool_calls: ToolCallStore,
    context_store: ContextCaptureStore,
    pricing: PriceBook,
    usage: UsageLedger,
    auto_update: Option<AutoUpdateTelemetrySummary>,
    session_contexts: HashMap<String, SessionContextSample>,
    active_tasks: BTreeMap<String, TaskSnapshot>,
    recent_tasks: VecDeque<TaskSnapshot>,
}

impl TelemetryInner {
    fn store_session_sample(&mut self, session_id: &str, sample: SessionContextSample) {
        if !self.session_contexts.contains_key(session_id)
            && self.session_contexts.len() >= MAX_SESSION_CONTEXT_SAMPLES
            && let Some(oldest) = self
                .session_contexts
                .iter()
                .min_by_key(|(_, s)| s.captured_at_unix_ms)
                .map(|(k, _)| k.clone())
        {
            let _ = self.session_contexts.remove(&oldest);
        }
        self.session_contexts.insert(session_id.to_owned(), sample);
    }
}

impl Default for TelemetryHub {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetryHub {
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(CacheTelemetryConfig::default())
    }

    #[must_use]
    pub fn with_config(config: CacheTelemetryConfig) -> Self {
        Self::with_telemetry_config(TelemetryConfig {
            cache: config,
            ..TelemetryConfig::default()
        })
    }

    #[must_use]
    pub fn with_telemetry_config(config: TelemetryConfig) -> Self {
        let (notify_tx, _) = broadcast::channel(64);
        let (shutdown_tx, _) = watch::channel(false);
        let settings_path = config.cache.settings_path.clone();
        let context_write_slots = Arc::new(Semaphore::new(
            config.context_capture.max_pending_writes.get(),
        ));
        let cache_store = CacheTelemetryStore::from_config(config.cache);
        Self {
            inner: Mutex::new(TelemetryInner {
                metrics: GatewayMetricsSummary::default(),
                elastic_io: ElasticIoTelemetry::default(),
                cache_store,
                tool_calls: ToolCallStore::new(config.tool_calls),
                context_store: ContextCaptureStore::load(config.context_capture),
                pricing: PriceBook::load(config.pricing_path),
                usage: UsageLedger::load(config.usage_path, unix_timestamp_ms()),
                auto_update: None,
                session_contexts: HashMap::new(),
                active_tasks: BTreeMap::new(),
                recent_tasks: VecDeque::with_capacity(MAX_RECENT_TASKS),
            }),
            providers: RwLock::new(Arc::new(ProviderRuntimeRegistry::default())),
            persist_lock: Mutex::new(0),
            context_persist_lock: Mutex::new(0),
            pricing_persist_lock: Mutex::new(0),
            usage_writer: Arc::new(UsageWriter::default()),
            context_write_slots,
            settings_path,
            notify_tx,
            shutdown_tx,
        }
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, TelemetryInner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Publishes the latest idle auto-update fleet summary and notifies SSE subscribers.
    pub fn set_auto_update_summary(&self, summary: AutoUpdateTelemetrySummary) {
        self.lock_inner().auto_update = Some(summary);
        self.notify_change();
    }

    /// Returns the most recently published idle auto-update fleet summary, if available.
    #[must_use]
    pub fn auto_update_summary(&self) -> Option<AutoUpdateTelemetrySummary> {
        self.lock_inner().auto_update.clone()
    }

    fn flush_prepared_persist(
        &self,
        prepared: Option<(PathBuf, u64, PersistedCacheTelemetryFile)>,
    ) {
        let Some((path, seq, payload)) = prepared else {
            return;
        };
        let mut written_seq = match self.persist_lock.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if seq <= *written_seq {
            return;
        }
        match persist_cache_file_atomic(&path, &payload) {
            Ok(()) => {
                *written_seq = seq;
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = ?err,
                    "failed to persist cache telemetry store"
                );
            }
        }
    }

    /// Hands a usage ledger snapshot to the background writer.
    fn persist_usage(&self, prepared: Option<PreparedLedger>) {
        if let Some(prepared) = prepared {
            self.usage_writer.submit(prepared);
        }
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.notify_tx.subscribe()
    }

    pub fn notify_change(&self) {
        let _ = self.notify_tx.send(());
    }

    /// Subscribes to gateway shutdown so long-lived telemetry SSE streams close immediately
    /// instead of blocking `axum`'s graceful connection drain.
    /// Returns the active multi-provider runtime registry attached to this hub.
    #[must_use]
    pub fn provider_registry(&self) -> Arc<ProviderRuntimeRegistry> {
        match self.providers.read() {
            Ok(g) => Arc::clone(&g),
            Err(p) => Arc::clone(&p.into_inner()),
        }
    }

    /// Attaches a disk-backed or pre-configured [`ProviderRuntimeRegistry`] to this hub.
    pub fn set_provider_registry(&self, registry: Arc<ProviderRuntimeRegistry>) {
        match self.providers.write() {
            Ok(mut g) => *g = registry,
            Err(p) => *p.into_inner() = registry,
        }
        self.notify_change();
    }

    #[must_use]
    pub fn subscribe_shutdown(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    /// Signals all open telemetry SSE streams to close cleanly.
    pub fn signal_shutdown(&self) {
        let _ = self.shutdown_tx.send_replace(true);
    }

    /// Registers a newly arrived API turn and returns an RAII `ActiveTaskGuard` that guarantees
    /// cleanup into `recent_tasks` even on SSE client disconnect, timeout, or early `?`.
    #[must_use]
    pub fn begin_task(
        self: &Arc<Self>,
        task_id: String,
        protocol: TaskProtocol,
        stream: bool,
        requested_model: String,
        auxiliary_task: Option<AuxiliaryTaskKind>,
        messages: &[CanonicalMessage],
    ) -> ActiveTaskGuard {
        let now_ms = unix_timestamp_ms();
        let prompt_preview = extract_last_user_preview(messages);
        let requested_model_for_ctx = requested_model.clone();

        let snapshot = TaskSnapshot {
            task_id: task_id.clone(),
            provider_id: "agy".to_owned(),
            optimization_mode: None,
            protocol,
            stream,
            requested_model: requested_model.clone(),
            resolved_model: requested_model,
            session_id: None,
            session_reused: false,
            harness_profile: None,
            auxiliary_task,
            phase: TaskPhase::InitializingSession,
            started_at_unix_ms: now_ms,
            finished_at_unix_ms: None,
            elapsed_ms: 0,
            ttft_ms: None,
            thought_chars: 0,
            output_chars: 0,
            client_tool_calls: Vec::new(),
            native_web_searches: 0,
            native_web_fetches: 0,
            vision_offloaded_images: 0,
            zero_rtt_mcp_ops: 0,
            spilled_tool_results: 0,
            prompt_preview,
            output_preview: String::new(),
            stop_reason: None,
            error_message: None,
            usage: None,
            cache_hit_rate_bps: None,
            cost_usd: None,
        };

        let capturing = {
            let mut guard = self.lock_inner();
            guard.metrics.total_requests += 1;
            match protocol {
                TaskProtocol::OpenaiResponses => guard.metrics.openai_requests += 1,
                TaskProtocol::AnthropicMessages => guard.metrics.anthropic_requests += 1,
            }
            if guard.tool_calls.pending_count() > 0 {
                guard.tool_calls.sweep_stale(now_ms);
                let _ = guard.tool_calls.complete_from_messages(messages, now_ms);
            }
            guard.active_tasks.insert(task_id.clone(), snapshot);
            guard.metrics.active_requests = guard.active_tasks.len();
            auxiliary_task.is_none() && guard.context_store.is_capturing()
        };
        self.notify_change();

        ActiveTaskGuard {
            hub: Arc::clone(self),
            tool_ctx: Mutex::new(ToolCallTaskContext {
                task_id: task_id.clone(),
                provider_id: "agy".to_owned(),
                session_id: None,
                model_id: requested_model_for_ctx,
                harness_profile: GENERIC_HARNESS_PROFILE,
                orchestrator_mode: None,
                turn_index: 0,
            }),
            next_tool_step: AtomicU32::new(0),
            text_capture: capturing.then(|| Mutex::new(TurnTextCapture::default())),
            upstream_wire: Mutex::new(None),
            task_id,
            armed: true,
        }
    }

    /// Returns a copy of cumulative Elastic I/O & `agy acp 1.5.18` MCP virtualization counters.
    #[must_use]
    pub fn elastic_io_snapshot(&self) -> ElasticIoTelemetry {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.elastic_io
    }

    /// Records registration of a new MCP-bridged session (`1:1` Direct Passthrough vs `ns__*` Virtualized).
    pub fn record_mcp_session_registered(&self, is_passthrough: bool) {
        {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if is_passthrough {
                guard.elastic_io.passthrough_sessions =
                    guard.elastic_io.passthrough_sessions.saturating_add(1);
            } else {
                guard.elastic_io.virtualized_sessions =
                    guard.elastic_io.virtualized_sessions.saturating_add(1);
            }
        }
        self.notify_change();
    }

    /// Records creation of a new lossless context spill file in `~/.liftgate/spills`.
    pub fn record_context_spill(&self, bytes: u64) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.elastic_io.context_spills_created =
            guard.elastic_io.context_spills_created.saturating_add(1);
        guard.elastic_io.context_spills_bytes =
            guard.elastic_io.context_spills_bytes.saturating_add(bytes);
    }

    // ---- per-call tool I/O -------------------------------------------------------------

    /// Ring-buffer status; also resolves calls that stayed pending past their TTL.
    #[must_use]
    pub fn tool_call_summary(&self) -> ToolCallTelemetrySummary {
        let mut guard = self.lock_inner();
        guard.tool_calls.sweep_stale(unix_timestamp_ms());
        guard.tool_calls.summary()
    }

    /// Tool calls changed after `since` (all retained calls when `None`), oldest first.
    #[must_use]
    pub fn tool_calls(&self, since: Option<u64>) -> ToolCallsResponse {
        self.lock_inner().tool_calls.list(since)
    }

    #[must_use]
    pub fn tool_call_detail(&self, call_id: &str) -> Option<ToolCallDetail> {
        self.lock_inner().tool_calls.detail(call_id)
    }

    /// Completes a client round-trip with the result the caller returned. When the request scan
    /// already completed it, attaches the gateway-forwarded output and spill state.
    pub fn finish_client_tool_call(
        &self,
        call_id: &str,
        raw_output: &str,
        forwarded_output: Option<&str>,
        is_error: bool,
        spilled: bool,
        images: Vec<crate::tool_calls::ToolOutputImage>,
    ) {
        let state = self.lock_inner().tool_calls.is_pending(call_id);
        let changed = match state {
            None => false,
            Some(true) => {
                let mut outcome = ToolCallOutcome::from_client(raw_output, is_error);
                outcome.spilled = spilled;
                outcome.forwarded_output = forwarded_output.map(str::to_owned);
                outcome.images = images;
                self.lock_inner()
                    .tool_calls
                    .complete(call_id, outcome, unix_timestamp_ms())
            }
            Some(false) => self.lock_inner().tool_calls.record_forwarded_output(
                call_id,
                forwarded_output,
                spilled,
            ),
        };
        if changed {
            self.notify_change();
        }
    }

    // ---- session context composition ---------------------------------------------------

    /// Latest context composition recorded for `session_id`.
    #[must_use]
    pub fn session_context_sample(&self, session_id: &str) -> Option<SessionContextSample> {
        self.lock_inner().session_contexts.get(session_id).cloned()
    }

    // ---- usage & pricing ---------------------------------------------------------------

    /// Reset epoch of the usage ledger, captured when an MCP cascade starts so its final drift
    /// correction is dropped if usage was reset in between.
    #[must_use]
    pub fn usage_epoch(&self) -> u64 {
        self.lock_inner().usage.epoch()
    }

    /// Usage totals and their cost, carried on every snapshot.
    #[must_use]
    pub fn usage_summary(&self) -> UsageSummary {
        let guard = self.lock_inner();
        guard.usage.report(&guard.pricing, None).summary
    }

    /// Per-model usage and cost: everything recorded, or the usage within `[from, to)`.
    #[must_use]
    pub fn usage_report(&self, range: Option<(u64, u64)>) -> UsageResponse {
        let guard = self.lock_inner();
        guard.usage.report(&guard.pricing, range)
    }

    /// Usage per interval between consecutive `bounds`, e.g. the caller's local days.
    #[must_use]
    pub fn usage_series(&self, bounds: &[u64]) -> UsageSeriesResponse {
        let guard = self.lock_inner();
        guard.usage.series(&guard.pricing, bounds)
    }

    /// Prices of every shipped family plus every catalog, recorded, or custom-priced model.
    #[must_use]
    pub fn pricing_report(&self, catalog: &[AvailableModel]) -> PricingResponse {
        let guard = self.lock_inner();
        guard
            .pricing
            .report(catalog, &guard.usage, unix_timestamp_ms())
    }

    /// Sets (`Some`) or clears (`None`) the custom price of a pricing key and persists the price
    /// book.
    pub fn set_custom_price(&self, key: &str, price: Option<ModelPrice>) -> PriceUpdate {
        let (outcome, prepared) = {
            let mut guard = self.lock_inner();
            let outcome = guard.pricing.set(key, price);
            let prepared = if outcome == PriceUpdate::Changed {
                guard.pricing.prepare_persist()
            } else {
                None
            };
            (outcome, prepared)
        };
        if let Some((path, seq, bytes)) = prepared {
            let mut written_seq = match self.pricing_persist_lock.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if seq > *written_seq {
                match write_bytes_atomic_private(&path, &bytes) {
                    Ok(()) => *written_seq = seq,
                    Err(err) => tracing::warn!(
                        path = %path.display(),
                        error = ?err,
                        "failed to persist model pricing"
                    ),
                }
            }
        }
        if outcome == PriceUpdate::Changed {
            self.notify_change();
        }
        outcome
    }

    /// Drops all recorded usage and starts counting again now.
    pub fn reset_usage(&self) -> UsageSummary {
        let now_ms = unix_timestamp_ms();
        let (summary, prepared) = {
            let mut guard = self.lock_inner();
            guard.usage.reset(now_ms);
            let summary = guard.usage.report(&guard.pricing, None).summary;
            (summary, guard.usage.prepare_persist())
        };
        self.persist_usage(prepared);
        self.notify_change();
        summary
    }

    // ---- full-context capture ----------------------------------------------------------

    #[must_use]
    pub fn context_capture_summary(&self) -> ContextCaptureSummary {
        self.lock_inner().context_store.summary()
    }

    /// Captured turns, newest first.
    #[must_use]
    pub fn context_captures(&self) -> ContextCapturesResponse {
        self.lock_inner().context_store.list()
    }

    /// Loads the payload of a captured turn; `Ok(None)` when the turn is not captured.
    pub fn context_capture_detail(
        &self,
        task_id: &str,
    ) -> Result<Option<ContextCaptureDetailResponse>, ContextCaptureIoError> {
        let located = self.lock_inner().context_store.locate(task_id);
        match located {
            Some((record, path)) => read_payload(record, &path).map(Some),
            None => Ok(None),
        }
    }

    /// Updates capture settings at runtime, enforcing a smaller quota immediately and syncing
    /// `settings.json` (and this process' `daemon.json`).
    pub fn update_context_capture(
        &self,
        enabled: Option<bool>,
        max_records: Option<ContextCaptureRecordLimit>,
        max_disk: Option<ContextCaptureDiskMib>,
    ) -> ContextCaptureSummary {
        let (summary, evicted, prepared, dir, settings) = {
            let mut guard = self.lock_inner();
            let evicted = guard
                .context_store
                .update_config(enabled, max_records, max_disk);
            let prepared = if evicted.is_empty() {
                None
            } else {
                guard.context_store.prepare_index()
            };
            (
                guard.context_store.summary(),
                evicted,
                prepared,
                guard.context_store.dir().map(Path::to_path_buf),
                guard.context_store.settings(),
            )
        };
        remove_capture_files(dir.as_deref(), &evicted);
        self.flush_context_index(prepared);
        if let Some(path) = self.settings_path.as_deref() {
            let (flag, limit, quota) = settings;
            sync_settings_json_fields(
                path,
                &[
                    ("capture_full_context", serde_json::Value::from(flag)),
                    ("max_context_captures", serde_json::Value::from(limit.get())),
                    (
                        "max_context_capture_mib",
                        serde_json::Value::from(quota.get()),
                    ),
                ],
            );
        }
        self.notify_change();
        summary
    }

    /// Deletes every captured payload.
    pub fn clear_context_captures(&self) -> ContextCaptureSummary {
        let (summary, cleared, prepared, dir) = {
            let mut guard = self.lock_inner();
            let cleared = guard.context_store.clear();
            let prepared = guard.context_store.prepare_index();
            (
                guard.context_store.summary(),
                cleared,
                prepared,
                guard.context_store.dir().map(Path::to_path_buf),
            )
        };
        remove_capture_files(dir.as_deref(), &cleared);
        self.flush_context_index(prepared);
        self.notify_change();
        summary
    }

    fn flush_context_index(&self, prepared: Option<(PathBuf, u64, Vec<u8>)>) {
        let Some((path, seq, bytes)) = prepared else {
            return;
        };
        let mut written_seq = match self.context_persist_lock.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if seq <= *written_seq {
            return;
        }
        match write_bytes_atomic_private(&path, &bytes) {
            Ok(()) => *written_seq = seq,
            Err(err) => tracing::warn!(
                path = %path.display(),
                error = ?err,
                "failed to persist full-context capture index"
            ),
        }
    }

    /// Writes a capture off the async workers when a runtime is available.
    fn persist_context_capture(self: &Arc<Self>, capture: PendingContextCapture) {
        let hub = Arc::clone(self);
        let job = move || hub.write_context_capture(capture);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => drop(handle.spawn_blocking(job)),
            Err(_) => job(),
        }
    }

    fn write_context_capture(&self, capture: PendingContextCapture) {
        // `_slot` is held until this write finishes, bounding payloads waiting on the disk.
        let PendingContextCapture {
            slot: _slot,
            ticket,
            mut record,
            payload,
        } = capture;
        let dir = {
            let guard = self.lock_inner();
            if !guard.context_store.accepts(ticket) {
                // Cleared or switched off since this turn completed.
                return;
            }
            let Some(dir) = guard.context_store.dir().map(Path::to_path_buf) else {
                return;
            };
            dir
        };
        let Some(file_name) = payload_file_name(&record.task_id) else {
            tracing::warn!(task_id = %record.task_id, "skipping full-context capture with unusable task id");
            return;
        };
        let bytes = match encode_payload(&record.task_id, &payload) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(error = ?err, "failed to encode full-context capture");
                return;
            }
        };
        drop(payload);
        let payload_path = dir.join(file_name);
        if let Err(err) = write_bytes_atomic_private(&payload_path, &bytes) {
            tracing::warn!(error = ?err, "failed to write full-context capture");
            return;
        }
        record.payload_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let committed = {
            let mut guard = self.lock_inner();
            match guard.context_store.commit(ticket, record) {
                CaptureCommit::Stored(evicted) => {
                    Some((evicted, guard.context_store.prepare_index()))
                }
                CaptureCommit::Stale => None,
            }
        };
        let Some((evicted, prepared)) = committed else {
            // Cleared or switched off while the payload was being written: it must not survive.
            let _ = fs::remove_file(&payload_path);
            return;
        };
        remove_capture_files(Some(&dir), &evicted);
        self.flush_context_index(prepared);
        self.notify_change();
    }

    /// Returns `(metrics, cache_telemetry, active_tasks, recent_tasks)` with live `elapsed_ms`.
    #[must_use]
    pub fn snapshot_tasks(
        &self,
    ) -> (
        GatewayMetricsSummary,
        CacheTelemetrySummary,
        Vec<TaskSnapshot>,
        Vec<TaskSnapshot>,
    ) {
        let now_ms = unix_timestamp_ms();
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        let mut active: Vec<TaskSnapshot> = guard
            .active_tasks
            .values()
            .cloned()
            .map(|mut t| {
                t.elapsed_ms = now_ms.saturating_sub(t.started_at_unix_ms);
                t
            })
            .collect();
        active.sort_by_key(|b| std::cmp::Reverse(b.started_at_unix_ms));

        let recent: Vec<TaskSnapshot> = guard
            .recent_tasks
            .iter()
            .rev()
            .cloned()
            .map(|mut t| {
                // Priced on read, so a price edit shows up on every listed request at once.
                t.cost_usd = t.usage.and_then(|usage| {
                    guard.pricing.cost_of(
                        &t.resolved_model,
                        &UsageTokens::from_acp(&usage),
                        t.finished_at_unix_ms.unwrap_or(now_ms),
                    )
                });
                t
            })
            .collect();
        let mut metrics = guard.metrics;
        metrics.active_requests = active.len();
        let cache_summary = guard.cache_store.summary();

        (metrics, cache_summary, active, recent)
    }

    /// Returns the persisted low-cache-hit records (newest first) alongside the current summary.
    #[must_use]
    pub fn low_cache_hits(&self) -> LowCacheHitsResponse {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let summary = guard.cache_store.summary();
        let records = guard
            .cache_store
            .file_state
            .low_cache_records
            .iter()
            .rev()
            .cloned()
            .collect();
        LowCacheHitsResponse { summary, records }
    }

    /// Looks up a persisted low-cache-hit record by `task_id` and loads its lossless on-disk
    /// request payload if available.
    #[must_use]
    pub fn low_cache_hit_detail(&self, task_id: &str) -> Option<LowCachePayloadDetailResponse> {
        let (record, payload_dir) = {
            let guard = match self.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let found = guard
                .cache_store
                .file_state
                .low_cache_records
                .iter()
                .rev()
                .find(|r| r.task_id == task_id)
                .cloned()?;
            (found, guard.cache_store.payload_dir.clone())
        };

        let full_payload = load_full_payload_for_record(&record, payload_dir.as_deref());
        Some(LowCachePayloadDetailResponse {
            record,
            full_payload,
        })
    }

    /// Dynamically updates the low-cache retention limit and/or threshold, trimming excess
    /// records immediately and atomically persisting to disk.
    pub fn update_cache_config(
        &self,
        max_low_cache_records: Option<LowCacheRecordLimit>,
        low_cache_threshold_bps: Option<CacheHitThresholdBps>,
        capture_request_details: Option<bool>,
    ) -> CacheTelemetrySummary {
        let now_ms = unix_timestamp_ms();
        let (summary, prepared, settings_sync, evicted, payload_dir) = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let mut evicted = Vec::new();
            if let Some(limit) = max_low_cache_records {
                guard.cache_store.max_low_cache_records = limit;
                evicted = trim_records_to_cap(
                    &mut guard.cache_store.file_state.low_cache_records,
                    limit.get(),
                );
            }
            if let Some(threshold) = low_cache_threshold_bps {
                guard.cache_store.low_cache_threshold_bps = threshold;
            }
            if let Some(capture) = capture_request_details {
                guard.cache_store.capture_request_details = capture;
            }
            guard.cache_store.revision = guard.cache_store.revision.saturating_add(1);
            let prepared = guard.cache_store.prepare_persist_payload(now_ms);
            let settings_sync = guard.cache_store.settings_path.clone().map(|p| {
                (
                    p,
                    guard.cache_store.max_low_cache_records,
                    guard.cache_store.low_cache_threshold_bps,
                    guard.cache_store.capture_request_details,
                )
            });
            let payload_dir = guard.cache_store.payload_dir.clone();
            (
                guard.cache_store.summary(),
                prepared,
                settings_sync,
                evicted,
                payload_dir,
            )
        };

        remove_payload_files(payload_dir.as_deref(), &evicted);
        self.flush_prepared_persist(prepared);
        if let Some((settings_path, limit, threshold, capture)) = settings_sync {
            sync_settings_json_cache_fields(&settings_path, limit, threshold, capture);
        }
        self.notify_change();
        summary
    }

    /// Clears persisted low-cache-hit records (and optionally resets cumulative cache counters).
    pub fn clear_low_cache_hits(&self, reset_totals: bool) -> CacheTelemetrySummary {
        let now_ms = unix_timestamp_ms();
        let (summary, prepared, cleared, payload_dir) = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let cleared: Vec<LowCacheHitRecord> = guard
                .cache_store
                .file_state
                .low_cache_records
                .drain(..)
                .collect();
            if reset_totals {
                guard.cache_store.file_state.total_prompt_tokens = 0;
                guard.cache_store.file_state.total_uncached_input_tokens = 0;
                guard.cache_store.file_state.total_cached_read_tokens = 0;
                guard.cache_store.file_state.total_cached_write_tokens = 0;
                guard.cache_store.file_state.warm_prompt_tokens = 0;
                guard.cache_store.file_state.warm_cached_read_tokens = 0;
                guard.cache_store.file_state.evaluated_turns = 0;
                guard.cache_store.file_state.warm_evaluated_turns = 0;
                guard.cache_store.file_state.total_low_cache_turns = 0;
                guard.cache_store.file_state.auxiliary_turns = 0;
                guard.cache_store.file_state.auxiliary_prompt_tokens = 0;
            }
            guard.cache_store.revision = guard.cache_store.revision.saturating_add(1);
            let prepared = guard.cache_store.prepare_persist_payload(now_ms);
            let payload_dir = guard.cache_store.payload_dir.clone();
            (guard.cache_store.summary(), prepared, cleared, payload_dir)
        };

        remove_payload_files(payload_dir.as_deref(), &cleared);
        clear_payload_directory(payload_dir.as_deref());
        self.flush_prepared_persist(prepared);
        self.notify_change();
        summary
    }
    /// Reconciles any rounding delta between intermediate `UsageUpdate` step estimates and the
    /// final authoritative `PromptTurnResult.usage` sum when a multi-step suspended MCP
    /// `session/prompt` completes, guaranteeing 0-token cumulative drift (including in the
    /// per-model usage ledger of `model_id`, unless usage was reset after the cascade began at
    /// `usage_epoch`; see [`Self::usage_epoch`]).
    pub fn reconcile_cascade_usage_drift(
        &self,
        model_id: &str,
        usage_epoch: u64,
        accounted_sum: AcpUsage,
        authoritative: AcpUsage,
        all_steps_warm: bool,
    ) {
        if accounted_sum == authoritative
            || (authoritative.total_prompt_tokens() == 0 && authoritative.output_tokens == 0)
        {
            return;
        }

        let adjust_u64 = |cur: u64, acc: u64, auth: u64| -> u64 {
            if auth >= acc {
                cur.saturating_add(auth - acc)
            } else {
                cur.saturating_sub(acc - auth)
            }
        };

        let acc_prompt = accounted_sum.total_prompt_tokens();
        let auth_prompt = authoritative.total_prompt_tokens();
        let now_ms = unix_timestamp_ms();
        let (cache_prepared, usage_prepared) = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.metrics.total_input_tokens = adjust_u64(
                guard.metrics.total_input_tokens,
                accounted_sum.input_tokens,
                authoritative.input_tokens,
            );
            guard.metrics.total_output_tokens = adjust_u64(
                guard.metrics.total_output_tokens,
                accounted_sum.output_tokens,
                authoritative.output_tokens,
            );
            guard.metrics.total_thought_tokens = adjust_u64(
                guard.metrics.total_thought_tokens,
                accounted_sum.thought_tokens,
                authoritative.thought_tokens,
            );
            guard.metrics.total_cached_tokens = adjust_u64(
                guard.metrics.total_cached_tokens,
                accounted_sum.cached_read_tokens,
                authoritative.cached_read_tokens,
            );
            guard.metrics.total_cached_write_tokens = adjust_u64(
                guard.metrics.total_cached_write_tokens,
                accounted_sum.cached_write_tokens,
                authoritative.cached_write_tokens,
            );

            let cs = &mut guard.cache_store;
            cs.file_state.total_prompt_tokens =
                adjust_u64(cs.file_state.total_prompt_tokens, acc_prompt, auth_prompt);
            cs.file_state.total_uncached_input_tokens = adjust_u64(
                cs.file_state.total_uncached_input_tokens,
                accounted_sum.input_tokens,
                authoritative.input_tokens,
            );
            cs.file_state.total_cached_read_tokens = adjust_u64(
                cs.file_state.total_cached_read_tokens,
                accounted_sum.cached_read_tokens,
                authoritative.cached_read_tokens,
            );
            cs.file_state.total_cached_write_tokens = adjust_u64(
                cs.file_state.total_cached_write_tokens,
                accounted_sum.cached_write_tokens,
                authoritative.cached_write_tokens,
            );
            if all_steps_warm {
                cs.file_state.warm_prompt_tokens =
                    adjust_u64(cs.file_state.warm_prompt_tokens, acc_prompt, auth_prompt)
                        .min(cs.file_state.total_prompt_tokens);
                cs.file_state.warm_cached_read_tokens = adjust_u64(
                    cs.file_state.warm_cached_read_tokens,
                    accounted_sum.cached_read_tokens,
                    authoritative.cached_read_tokens,
                )
                .min(cs.file_state.warm_prompt_tokens);
            }
            cs.revision = cs.revision.saturating_add(1);
            guard.usage.reconcile_with_provider(
                "agy",
                model_id,
                usage_epoch,
                UsageTokens::from_acp(&accounted_sum),
                UsageTokens::from_acp(&authoritative),
                now_ms,
            );

            let cumulative_prompt = guard
                .metrics
                .total_input_tokens
                .saturating_add(guard.metrics.total_cached_tokens)
                .saturating_add(guard.metrics.total_cached_write_tokens);
            guard.metrics.cache_hit_rate_bps =
                if guard.cache_store.file_state.total_prompt_tokens > 0 {
                    compute_cache_hit_bps(
                        guard.cache_store.file_state.total_cached_read_tokens,
                        guard.cache_store.file_state.total_prompt_tokens,
                    )
                } else {
                    compute_cache_hit_bps(guard.metrics.total_cached_tokens, cumulative_prompt)
                };

            (
                guard.cache_store.prepare_persist_payload(now_ms),
                guard.usage.prepare_persist(),
            )
        };

        self.flush_prepared_persist(cache_prepared);
        self.persist_usage(usage_prepared);
        self.notify_change();
    }
}

fn stamp_ttft_if_first(task: &mut TaskSnapshot, now_ms: u64) -> bool {
    if task.ttft_ms.is_none() {
        task.ttft_ms = Some(now_ms.saturating_sub(task.started_at_unix_ms));
        true
    } else {
        false
    }
}

fn is_compound_orchestrator_or_shell_tool(tool_name: &str) -> bool {
    let trimmed = tool_name.trim();
    trimmed.eq_ignore_ascii_case("exec")
        || trimmed.eq_ignore_ascii_case("execute")
        || trimmed.eq_ignore_ascii_case("eval")
        || trimmed.eq_ignore_ascii_case("repl")
        || trimmed.eq_ignore_ascii_case("bash")
        || trimmed.eq_ignore_ascii_case("powershell")
        || trimmed.eq_ignore_ascii_case("exec_command")
        || trimmed.eq_ignore_ascii_case("shell")
        || trimmed == "__codex_local_shell"
}

fn extract_v4a_patch_file_count(tool_name: &str, arguments: &str) -> Option<usize> {
    let trimmed_name = tool_name.trim();
    let is_apply_patch = trimmed_name.eq_ignore_ascii_case("apply_patch");
    let is_patch = trimmed_name.eq_ignore_ascii_case("patch");
    if !is_apply_patch && !is_patch {
        return None;
    }
    let extracted_json_patch = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|val| {
            val.get("patch")
                .or_else(|| val.get("patchText"))
                .or_else(|| val.get("input"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    let patch_text = extracted_json_patch.as_deref().unwrap_or(arguments);
    let headers = patch_text
        .lines()
        .filter(|line| {
            let l = line.trim_start();
            l.starts_with("*** Add File:")
                || l.starts_with("*** Update File:")
                || l.starts_with("*** Delete File:")
        })
        .count();
    if is_apply_patch {
        Some(headers.max(usize::from(!patch_text.trim().is_empty())))
    } else {
        (headers > 0).then_some(headers)
    }
}

/// Harness profile reported until a turn identifies its harness.
pub const GENERIC_HARNESS_PROFILE: &str = "generic";

/// Reasoning and output text accumulated for full-context capture.
#[derive(Debug, Default)]
struct TurnTextCapture {
    reasoning: String,
    output: String,
    reasoning_truncated: bool,
    output_truncated: bool,
    tool_result_rewrites: Vec<PendingToolResultRewrite>,
    tool_result_rewrite_bytes: usize,
}

/// A client tool result the gateway changed before handing it back to the model.
#[derive(Debug)]
struct PendingToolResultRewrite {
    call_id: String,
    nonce_suffix: String,
    tool_name: String,
    original_bytes: usize,
    rewritten_bytes: usize,
    /// Head/tail bounded to [`CONTEXT_REWRITE_BUDGET`]`.per_part`.
    original: String,
    rewritten: String,
    spilled: bool,
    truncated: bool,
}

/// Head/tail bounds `raw` within both the per-part and remaining turn rewrite budgets.
fn bound_rewrite_text(raw: &str, used_bytes: &mut usize, truncated: &mut bool) -> String {
    let remaining = CONTEXT_REWRITE_BUDGET.total.saturating_sub(*used_bytes);
    let cap = remaining.min(CONTEXT_REWRITE_BUDGET.per_part);
    if cap == 0 {
        if !raw.is_empty() {
            *truncated = true;
        }
        return "[...omitted: diagnostic payload exceeded its safety cap...]".to_owned();
    }
    if raw.len() <= cap {
        *used_bytes = used_bytes.saturating_add(raw.len());
        return raw.to_owned();
    }
    *truncated = true;
    let bounded = truncate_head_tail_lines(raw, cap, "context rewrite");
    *used_bytes = used_bytes.saturating_add(bounded.len());
    bounded
}

/// Finds the `(message, part, wire_call_id)` holding the tool result that answers `call_id`,
/// searching this turn's delta slice (`delta_start..`) before falling back to earlier messages.
fn locate_tool_result(
    messages: &[CanonicalMessage],
    delta_start: usize,
    call_id: &str,
    nonce_suffix: &str,
) -> Option<(usize, usize, String)> {
    let clamped = delta_start.min(messages.len());
    let search_range = |base: usize, slice: &[CanonicalMessage]| {
        slice.iter().enumerate().find_map(|(rel_idx, msg)| {
            msg.parts.iter().enumerate().find_map(|(part_idx, part)| {
                let CanonicalPart::ToolResult { call_id: id, .. } = part else {
                    return None;
                };
                let matched = id == call_id
                    || (!nonce_suffix.is_empty() && strip_call_prefix(id) == nonce_suffix);
                matched.then(|| (base.saturating_add(rel_idx), part_idx, id.clone()))
            })
        })
    };
    messages
        .get(clamped..)
        .and_then(|tail| search_range(clamped, tail))
        .or_else(|| {
            messages
                .get(..clamped)
                .and_then(|head| search_range(0, head))
        })
}

/// Lists every place the gateway changed the harness context of this turn, in message order:
/// distilled or dropped developer slots plus sanitized or spilled client tool results.
fn collect_context_rewrites(
    diag: &TurnCompletionDiagnostics<'_>,
    tool_results: Vec<PendingToolResultRewrite>,
) -> (Vec<ContextRewrite>, bool) {
    let per_part = CONTEXT_REWRITE_BUDGET.per_part;
    let mut budget = CONTEXT_REWRITE_BUDGET.total;
    let mut truncated = false;
    let mut out = Vec::new();
    let delta_start = diag.clamped_delta_start();
    if let Some(harness) = diag.distill_harness {
        for rw in explain_developer_slot_rewrites(harness, diag.developer_slots, diag.messages) {
            let mut item_truncated = false;
            let original =
                bound_part_string(rw.original, &mut budget, per_part, &mut item_truncated);
            let rewritten = rw
                .rewritten
                .as_deref()
                .map(|text| bound_part_string(text, &mut budget, per_part, &mut item_truncated));
            truncated |= item_truncated;
            out.push(ContextRewrite {
                kind: if rewritten.is_some() {
                    ContextRewriteKind::PromptDistilled
                } else {
                    ContextRewriteKind::PromptDropped
                },
                message_index: Some(rw.msg_index),
                part_index: Some(rw.part_index),
                slot: Some(rw.slot.to_owned()),
                call_id: None,
                tool_name: None,
                original_bytes: rw.original.len(),
                rewritten_bytes: rw.rewritten.as_deref().map_or(0, str::len),
                original,
                rewritten,
                truncated: item_truncated,
            });
        }
    }
    for rw in tool_results {
        let mut item_truncated = rw.truncated;
        let coords = locate_tool_result(diag.messages, delta_start, &rw.call_id, &rw.nonce_suffix);
        let original = bound_part_string(&rw.original, &mut budget, per_part, &mut item_truncated);
        let rewritten =
            bound_part_string(&rw.rewritten, &mut budget, per_part, &mut item_truncated);
        truncated |= item_truncated;
        let (message_index, part_index, matched_call_id) = match coords {
            Some((msg_idx, part_idx, wire_id)) => (Some(msg_idx), Some(part_idx), wire_id),
            None => (None, None, rw.call_id),
        };
        out.push(ContextRewrite {
            kind: if rw.spilled {
                ContextRewriteKind::ToolResultSpilled
            } else {
                ContextRewriteKind::ToolResultSanitized
            },
            message_index,
            part_index,
            slot: None,
            call_id: Some(matched_call_id),
            tool_name: Some(rw.tool_name),
            original_bytes: rw.original_bytes,
            rewritten_bytes: rw.rewritten_bytes,
            original,
            rewritten: Some(rewritten),
            truncated: item_truncated,
        });
    }
    out.sort_by_key(|rw| {
        (
            rw.message_index.unwrap_or(usize::MAX),
            rw.part_index.unwrap_or(usize::MAX),
        )
    });
    (out, truncated)
}

/// A completed turn's capture, waiting for its payload to be written off the async workers.
struct PendingContextCapture {
    /// Write slot taken before the payload was materialized; released when the write finishes.
    slot: OwnedSemaphorePermit,
    /// Taken when the turn completed ([`ContextCaptureStore::reserve`]).
    ticket: CaptureTicket,
    record: ContextCaptureRecord,
    payload: FullContextPayload,
}

/// Appends as much of `delta` as fits in `cap` bytes; `true` when anything was dropped.
fn push_bounded(buf: &mut String, delta: &str, cap: usize) -> bool {
    let room = cap.saturating_sub(buf.len());
    if delta.len() <= room {
        buf.push_str(delta);
        return false;
    }
    if let Some(prefix) = delta.get(..delta.floor_char_boundary(room)) {
        buf.push_str(prefix);
    }
    true
}

/// RAII tracker for an in-flight gateway turn.
pub struct ActiveTaskGuard {
    hub: Arc<TelemetryHub>,
    task_id: String,
    armed: bool,
    tool_ctx: Mutex<ToolCallTaskContext>,
    next_tool_step: AtomicU32,
    text_capture: Option<Mutex<TurnTextCapture>>,
    upstream_wire: Mutex<Option<UpstreamWireSummary>>,
}

impl ActiveTaskGuard {
    #[must_use]
    pub const fn hub(&self) -> &Arc<TelemetryHub> {
        &self.hub
    }

    #[must_use]
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    fn lock_tool_ctx(&self) -> std::sync::MutexGuard<'_, ToolCallTaskContext> {
        match self.tool_ctx.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Tags this turn with its resolved upstream provider ID and optimization mode.
    pub fn set_provider_and_optimization(&self, provider_id: &str, opt_mode: OptimizationMode) {
        {
            let mut ctx = self.lock_tool_ctx();
            provider_id.clone_into(&mut ctx.provider_id);
        }
        {
            let mut guard = self.hub.lock_inner();
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                provider_id.clone_into(&mut task.provider_id);
                task.optimization_mode = Some(opt_mode);
            }
        }
        self.hub.notify_change();
    }

    /// Attaches Stage-3 upstream wire delivery metrics for full-context 3-stage comparison.
    pub fn set_upstream_wire_summary(&self, summary: UpstreamWireSummary) {
        match self.upstream_wire.lock() {
            Ok(mut g) => *g = Some(summary),
            Err(p) => *p.into_inner() = Some(summary),
        }
    }

    pub fn bind_session(&self, session_id: &str, reused: bool, resolved_model: &str) {
        {
            let mut ctx = self.lock_tool_ctx();
            ctx.session_id = Some(session_id.to_owned());
            resolved_model.clone_into(&mut ctx.model_id);
        }
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                task.session_id = Some(session_id.to_owned());
                task.session_reused = reused;
                task.resolved_model = resolved_model.to_owned();
            }
        }
        self.hub.notify_change();
    }

    /// Attributes this turn's tool calls to a harness profile, orchestration mode, and the
    /// session turn index.
    pub fn set_tool_context(
        &self,
        harness_profile: &'static str,
        orchestrator_mode: Option<&'static str>,
        turn_index: u64,
    ) {
        {
            let mut ctx = self.lock_tool_ctx();
            ctx.harness_profile = harness_profile;
            ctx.orchestrator_mode = orchestrator_mode;
            ctx.turn_index = turn_index;
        }
        let changed = {
            let mut guard = self.hub.lock_inner();
            match guard.active_tasks.get_mut(&self.task_id) {
                Some(task) if task.harness_profile.as_deref() != Some(harness_profile) => {
                    task.harness_profile = Some(harness_profile.to_owned());
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.hub.notify_change();
        }
    }

    /// Harness profile attributed to this turn.
    #[must_use]
    pub fn harness_profile(&self) -> &'static str {
        self.lock_tool_ctx().harness_profile
    }

    fn next_tool_context(&self) -> (ToolCallTaskContext, u32) {
        let step = self.next_tool_step.fetch_add(1, Ordering::Relaxed);
        (self.lock_tool_ctx().clone(), step)
    }

    /// Records a call emitted to the caller; it stays pending until its result arrives.
    pub fn begin_tool_call(&self, start: ToolCallStart) {
        let (ctx, step) = self.next_tool_context();
        let now_ms = unix_timestamp_ms();
        self.hub
            .lock_inner()
            .tool_calls
            .begin(&ctx, step, start, now_ms);
        self.hub.notify_change();
    }

    /// Records a call answered inside the gateway (0-RTT replies, preflight, web fetch).
    pub fn record_local_tool_call(
        &self,
        start: ToolCallStart,
        outcome: ToolCallOutcome,
        started_ms: u64,
    ) {
        let (ctx, step) = self.next_tool_context();
        let now_ms = unix_timestamp_ms();
        self.hub
            .lock_inner()
            .tool_calls
            .record_local(&ctx, step, start, outcome, started_ms, now_ms);
        self.hub.notify_change();
    }

    /// Keeps a client tool result the gateway changed before forwarding, for full-context capture.
    pub fn record_tool_result_rewrite(
        &self,
        call_id: &str,
        nonce_suffix: &str,
        tool_name: &str,
        original: &str,
        rewritten: &str,
        spilled: bool,
    ) {
        let Some(capture) = self.text_capture.as_ref() else {
            return;
        };
        let mut buf = match capture.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let mut truncated = false;
        let bounded_original =
            bound_rewrite_text(original, &mut buf.tool_result_rewrite_bytes, &mut truncated);
        let bounded_rewritten = bound_rewrite_text(
            rewritten,
            &mut buf.tool_result_rewrite_bytes,
            &mut truncated,
        );
        let entry = PendingToolResultRewrite {
            call_id: call_id.to_owned(),
            nonce_suffix: nonce_suffix.to_owned(),
            tool_name: tool_name.to_owned(),
            original_bytes: original.len(),
            rewritten_bytes: rewritten.len(),
            original: bounded_original,
            rewritten: bounded_rewritten,
            spilled,
            truncated,
        };
        buf.tool_result_rewrites.push(entry);
    }

    fn capture_text(&self, delta: &str, reasoning: bool) {
        let Some(capture) = self.text_capture.as_ref() else {
            return;
        };
        let mut buf = match capture.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let TurnTextCapture {
            reasoning: reasoning_text,
            output,
            reasoning_truncated,
            output_truncated,
            ..
        } = &mut *buf;
        let (target, truncated) = if reasoning {
            (reasoning_text, reasoning_truncated)
        } else {
            (output, output_truncated)
        };
        if push_bounded(target, delta, MAX_CAPTURED_TURN_TEXT_BYTES) {
            *truncated = true;
        }
    }

    pub fn record_vision_offload(&self, image_count: usize) {
        if image_count == 0 {
            return;
        }
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                task.vision_offloaded_images =
                    task.vision_offloaded_images.saturating_add(image_count);
            }
        }
        self.hub.notify_change();
    }

    pub fn record_thought(&self, delta: &str) {
        let delta_chars = delta.chars().count();
        self.capture_text(delta, true);
        let now_ms = unix_timestamp_ms();
        let should_notify = {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                let first_ttft = stamp_ttft_if_first(task, now_ms);
                let phase_changed = task.phase != TaskPhase::Thinking;
                task.phase = TaskPhase::Thinking;
                task.thought_chars = task.thought_chars.saturating_add(delta_chars);
                phase_changed || first_ttft
            } else {
                false
            }
        };
        if should_notify {
            self.hub.notify_change();
        }
    }

    pub fn record_web_search(&self) {
        let now_ms = unix_timestamp_ms();
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                let _ = stamp_ttft_if_first(task, now_ms);
                task.phase = TaskPhase::SearchingWeb;
                task.native_web_searches = task.native_web_searches.saturating_add(1);
            }
        }
        self.hub.notify_change();
    }

    pub fn record_web_fetch(&self) {
        let now_ms = unix_timestamp_ms();
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                let _ = stamp_ttft_if_first(task, now_ms);
                task.phase = TaskPhase::FetchingWeb;
                task.native_web_fetches = task.native_web_fetches.saturating_add(1);
            }
        }
        self.hub.notify_change();
    }

    pub fn record_text(&self, delta: &str) {
        self.capture_text(delta, false);
        let now_ms = unix_timestamp_ms();
        let should_notify = {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                let first_ttft = stamp_ttft_if_first(task, now_ms);
                let phase_changed = task.phase != TaskPhase::StreamingText;
                task.phase = TaskPhase::StreamingText;
                task.output_chars = task.output_chars.saturating_add(delta.chars().count());
                append_preview_bounded(&mut task.output_preview, delta, MAX_OUTPUT_PREVIEW_CHARS);
                phase_changed || first_ttft
            } else {
                false
            }
        };
        if should_notify {
            self.hub.notify_change();
        }
    }

    pub fn mark_calling_client_tool(&self) {
        let now_ms = unix_timestamp_ms();
        let should_notify = {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                let first_ttft = stamp_ttft_if_first(task, now_ms);
                let phase_changed = task.phase != TaskPhase::CallingClientTool;
                task.phase = TaskPhase::CallingClientTool;
                phase_changed || first_ttft
            } else {
                false
            }
        };
        if should_notify {
            self.hub.notify_change();
        }
    }

    pub fn record_client_tool(&self, tool_name: &str) {
        let now_ms = unix_timestamp_ms();
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                let _ = stamp_ttft_if_first(task, now_ms);
                task.phase = TaskPhase::CallingClientTool;
                task.client_tool_calls.push(tool_name.to_owned());
            }
        }
        self.hub.notify_change();
    }

    /// Records a validated native MCP client tool call forwarded over SSE/JSON to the caller,
    /// classifying compound orchestrators (`exec`/`execute`/`eval`/`Bash`/shell) and V4A multi-file patches.
    pub fn record_mcp_forwarded_tool(
        &self,
        tool_name: &str,
        arguments: &str,
        is_first_in_prompt_turn: bool,
    ) {
        let now_ms = unix_timestamp_ms();
        let is_compound = is_compound_orchestrator_or_shell_tool(tool_name);
        let patch_files = extract_v4a_patch_file_count(tool_name, arguments);
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                let _ = stamp_ttft_if_first(task, now_ms);
                task.phase = TaskPhase::CallingClientTool;
                task.client_tool_calls.push(tool_name.to_owned());
            }
            guard.elastic_io.mcp_forwarded_tool_calls =
                guard.elastic_io.mcp_forwarded_tool_calls.saturating_add(1);
            if is_first_in_prompt_turn {
                guard.elastic_io.mcp_tool_using_turns =
                    guard.elastic_io.mcp_tool_using_turns.saturating_add(1);
            }
            if is_compound {
                guard.elastic_io.mcp_orchestrator_calls =
                    guard.elastic_io.mcp_orchestrator_calls.saturating_add(1);
            }
            if let Some(files) = patch_files {
                guard.elastic_io.mcp_patch_calls =
                    guard.elastic_io.mcp_patch_calls.saturating_add(1);
                guard.elastic_io.mcp_patch_files_total = guard
                    .elastic_io
                    .mcp_patch_files_total
                    .saturating_add(files as u64);
                if files >= 2 {
                    guard.elastic_io.mcp_multi_file_patch_calls = guard
                        .elastic_io
                        .mcp_multi_file_patch_calls
                        .saturating_add(1);
                }
            }
        }
        self.hub.notify_change();
    }

    pub fn record_zero_rtt_search(&self) {
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.elastic_io.mcp_zero_rtt_searches =
                guard.elastic_io.mcp_zero_rtt_searches.saturating_add(1);
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                task.zero_rtt_mcp_ops = task.zero_rtt_mcp_ops.saturating_add(1);
            }
        }
        self.hub.notify_change();
    }

    pub fn record_zero_rtt_spill_read(&self) {
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.elastic_io.mcp_zero_rtt_spill_reads =
                guard.elastic_io.mcp_zero_rtt_spill_reads.saturating_add(1);
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                task.zero_rtt_mcp_ops = task.zero_rtt_mcp_ops.saturating_add(1);
            }
        }
        self.hub.notify_change();
    }

    pub fn record_zero_rtt_preflight_heal(&self) {
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.elastic_io.mcp_zero_rtt_preflight_heals = guard
                .elastic_io
                .mcp_zero_rtt_preflight_heals
                .saturating_add(1);
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                task.zero_rtt_mcp_ops = task.zero_rtt_mcp_ops.saturating_add(1);
            }
        }
        self.hub.notify_change();
    }

    pub fn record_mcp_tool_results(&self, total: usize, spilled: usize) {
        if total == 0 {
            return;
        }
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.elastic_io.mcp_tool_results_total = guard
                .elastic_io
                .mcp_tool_results_total
                .saturating_add(total as u64);
            guard.elastic_io.mcp_tool_results_spilled = guard
                .elastic_io
                .mcp_tool_results_spilled
                .saturating_add(spilled as u64);
            if spilled > 0
                && let Some(task) = guard.active_tasks.get_mut(&self.task_id)
            {
                task.spilled_tool_results = task.spilled_tool_results.saturating_add(spilled);
            }
        }
        if spilled > 0 {
            self.hub.notify_change();
        }
    }

    pub fn record_tool_delta(&self, delta_chars: usize) {
        if delta_chars == 0 {
            return;
        }
        let mut guard = match self.hub.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
            task.output_chars = task.output_chars.saturating_add(delta_chars);
        }
    }

    pub fn complete(self, stop_reason: String, usage: AcpUsage) {
        self.complete_with_diagnostics(stop_reason, usage, None);
    }

    pub fn complete_with_diagnostics(
        mut self,
        stop_reason: String,
        usage: AcpUsage,
        diagnostics: Option<TurnCompletionDiagnostics<'_>>,
    ) {
        self.armed = false;
        let now_ms = unix_timestamp_ms();
        let turn_prompt_tokens = usage.total_prompt_tokens();
        let turn_hit_bps = usage.cache_hit_bps();
        let measure = diagnostics
            .as_ref()
            .map(|d| measure_messages(d.messages, d.delta_start_index));
        let context_sample_segments = diagnostics
            .as_ref()
            .zip(measure.as_ref())
            .map(|(d, m)| message_context_segments(m, d.tools_delta_xml.map_or(0, str::len)));
        let full_context_capture =
            self.build_full_context_capture(diagnostics.as_ref(), measure.as_ref(), usage, now_ms);

        let (capture_settings, task_snapshot_preview) = {
            let guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let task_meta = guard.active_tasks.get(&self.task_id).map(|t| {
                (
                    t.auxiliary_task,
                    t.session_reused,
                    t.stream,
                    t.thought_chars,
                    t.output_chars,
                    t.native_web_searches,
                    t.native_web_fetches,
                    t.vision_offloaded_images,
                    t.client_tool_calls.clone(),
                    t.output_preview.clone(),
                )
            });
            let cs = &guard.cache_store;
            (
                (
                    cs.max_low_cache_records.get(),
                    cs.low_cache_threshold_bps.get(),
                    cs.capture_request_details,
                    cs.payload_dir.clone(),
                ),
                task_meta,
            )
        };

        let prebuilt_low_cache_extras = if let (
            Some(hit_bps),
            Some(diag),
            Some(measure),
            Some((
                None,
                session_reused,
                stream,
                thought_chars,
                output_chars,
                native_web_searches,
                native_web_fetches,
                vision_offloaded_images,
                emitted_client_tools,
                output_preview,
            )),
        ) = (
            turn_hit_bps,
            diagnostics.as_ref(),
            measure.as_ref(),
            task_snapshot_preview,
        ) && capture_settings.0 > 0
            && hit_bps < capture_settings.1
        {
            let (metrics, cause) = build_low_cache_request_metrics(
                diag,
                measure,
                session_reused,
                stream,
                usage,
                thought_chars,
                output_chars,
                native_web_searches,
                native_web_fetches,
                vision_offloaded_images,
                emitted_client_tools,
                &stop_reason,
            );
            let capture = if capture_settings.2 {
                Some(build_and_persist_low_cache_capture(
                    &self.task_id,
                    now_ms,
                    diag,
                    &output_preview,
                    capture_settings.3.as_deref(),
                ))
            } else {
                None
            };
            Some((cause, metrics, capture))
        } else {
            None
        };

        let (prepared, usage_prepared, evicted_records, payload_dir) = {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let mut persist_needed = false;
            let mut usage_prepared = None;
            let mut evicted_records = Vec::new();
            let payload_dir = guard.cache_store.payload_dir.clone();

            if let Some(mut task) = guard.active_tasks.remove(&self.task_id) {
                if let (Some(segments), Some(session_id)) =
                    (context_sample_segments, task.session_id.as_deref())
                {
                    let sample = SessionContextSample {
                        model_id: task.resolved_model.clone(),
                        captured_at_unix_ms: now_ms,
                        segments,
                    };
                    guard.store_session_sample(session_id, sample);
                }
                let elapsed_ms = now_ms.saturating_sub(task.started_at_unix_ms);
                task.phase = TaskPhase::Completed;
                task.finished_at_unix_ms = Some(now_ms);
                task.elapsed_ms = elapsed_ms;
                task.stop_reason = Some(stop_reason);
                task.usage = Some(usage);
                task.cache_hit_rate_bps = turn_hit_bps;
                guard.usage.record_with_provider(
                    &task.provider_id,
                    &task.resolved_model,
                    task.auxiliary_task.is_some(),
                    UsageTokens::from_acp(&usage),
                    now_ms,
                );
                usage_prepared = guard.usage.prepare_persist();

                if let Some(hit_bps) = turn_hit_bps {
                    persist_needed = true;
                    let cs = &mut guard.cache_store;
                    if task.auxiliary_task.is_some() {
                        cs.file_state.auxiliary_turns =
                            cs.file_state.auxiliary_turns.saturating_add(1);
                        cs.file_state.auxiliary_prompt_tokens = cs
                            .file_state
                            .auxiliary_prompt_tokens
                            .saturating_add(turn_prompt_tokens);
                        cs.revision = cs.revision.saturating_add(1);
                    } else {
                        cs.file_state.total_prompt_tokens = cs
                            .file_state
                            .total_prompt_tokens
                            .saturating_add(turn_prompt_tokens);
                        cs.file_state.total_uncached_input_tokens = cs
                            .file_state
                            .total_uncached_input_tokens
                            .saturating_add(usage.input_tokens);
                        cs.file_state.total_cached_read_tokens = cs
                            .file_state
                            .total_cached_read_tokens
                            .saturating_add(usage.cached_read_tokens);
                        cs.file_state.total_cached_write_tokens = cs
                            .file_state
                            .total_cached_write_tokens
                            .saturating_add(usage.cached_write_tokens);
                        cs.file_state.evaluated_turns =
                            cs.file_state.evaluated_turns.saturating_add(1);

                        if task.session_reused {
                            cs.file_state.warm_prompt_tokens = cs
                                .file_state
                                .warm_prompt_tokens
                                .saturating_add(turn_prompt_tokens);
                            cs.file_state.warm_cached_read_tokens = cs
                                .file_state
                                .warm_cached_read_tokens
                                .saturating_add(usage.cached_read_tokens);
                            cs.file_state.warm_evaluated_turns =
                                cs.file_state.warm_evaluated_turns.saturating_add(1);
                        }

                        if hit_bps < cs.low_cache_threshold_bps.get() {
                            cs.file_state.total_low_cache_turns =
                                cs.file_state.total_low_cache_turns.saturating_add(1);
                            let cap = cs.max_low_cache_records.get();
                            if cap > 0 {
                                while cs.file_state.low_cache_records.len() >= cap {
                                    if let Some(old) = cs.file_state.low_cache_records.pop_front() {
                                        evicted_records.push(old);
                                    } else {
                                        break;
                                    }
                                }
                                let (diagnostic_cause, request_metrics, request_capture) =
                                    match prebuilt_low_cache_extras {
                                        Some((cause, metrics, capture)) => {
                                            (cause, Some(metrics), capture)
                                        }
                                        None => (
                                            LowCacheDiagnosticCause::classify(
                                                task.session_reused,
                                                usage.cached_read_tokens,
                                                0,
                                                1,
                                                None,
                                                false,
                                            ),
                                            None,
                                            None,
                                        ),
                                    };
                                cs.file_state
                                    .low_cache_records
                                    .push_back(LowCacheHitRecord {
                                        task_id: task.task_id.clone(),
                                        recorded_at_unix_ms: now_ms,
                                        protocol: task.protocol,
                                        requested_model: task.requested_model.clone(),
                                        resolved_model: task.resolved_model.clone(),
                                        session_id: task.session_id.clone(),
                                        session_reused: task.session_reused,
                                        reason: LowCacheHitReason::classify(
                                            task.session_reused,
                                            usage.cached_read_tokens,
                                        ),
                                        diagnostic_cause,
                                        cache_hit_rate_bps: hit_bps,
                                        input_uncached_tokens: usage.input_tokens,
                                        cached_read_tokens: usage.cached_read_tokens,
                                        cached_write_tokens: usage.cached_write_tokens,
                                        total_prompt_tokens: turn_prompt_tokens,
                                        output_tokens: usage.output_tokens,
                                        elapsed_ms,
                                        ttft_ms: task.ttft_ms,
                                        prompt_preview: task.prompt_preview.clone(),
                                        request_metrics,
                                        request_capture,
                                    });
                                cs.revision = cs.revision.saturating_add(1);
                            }
                        }
                    }
                }

                push_recent(&mut guard.recent_tasks, task);
            }

            guard.metrics.active_requests = guard.active_tasks.len();
            guard.metrics.completed_requests += 1;
            guard.metrics.total_input_tokens = guard
                .metrics
                .total_input_tokens
                .saturating_add(usage.input_tokens);
            guard.metrics.total_output_tokens = guard
                .metrics
                .total_output_tokens
                .saturating_add(usage.output_tokens);
            guard.metrics.total_thought_tokens = guard
                .metrics
                .total_thought_tokens
                .saturating_add(usage.thought_tokens);
            guard.metrics.total_cached_tokens = guard
                .metrics
                .total_cached_tokens
                .saturating_add(usage.cached_read_tokens);
            guard.metrics.total_cached_write_tokens = guard
                .metrics
                .total_cached_write_tokens
                .saturating_add(usage.cached_write_tokens);
            let cumulative_prompt = guard
                .metrics
                .total_input_tokens
                .saturating_add(guard.metrics.total_cached_tokens)
                .saturating_add(guard.metrics.total_cached_write_tokens);
            guard.metrics.cache_hit_rate_bps =
                if guard.cache_store.file_state.total_prompt_tokens > 0 {
                    compute_cache_hit_bps(
                        guard.cache_store.file_state.total_cached_read_tokens,
                        guard.cache_store.file_state.total_prompt_tokens,
                    )
                } else {
                    compute_cache_hit_bps(guard.metrics.total_cached_tokens, cumulative_prompt)
                };

            let prep = if persist_needed {
                guard.cache_store.prepare_persist_payload(now_ms)
            } else {
                None
            };
            (prep, usage_prepared, evicted_records, payload_dir)
        };

        remove_payload_files(payload_dir.as_deref(), &evicted_records);
        self.hub.flush_prepared_persist(prepared);
        self.hub.persist_usage(usage_prepared);
        if let Some(capture) = full_context_capture {
            self.hub.persist_context_capture(capture);
        }
        self.hub.notify_change();
    }

    /// Materializes the full-context capture of a completed turn while its borrowed state is
    /// still alive; `None` unless capture was enabled when the turn began.
    fn build_full_context_capture(
        &self,
        diagnostics: Option<&TurnCompletionDiagnostics<'_>>,
        measure: Option<&MessageMeasure>,
        usage: AcpUsage,
        now_ms: u64,
    ) -> Option<PendingContextCapture> {
        let capture = self.text_capture.as_ref()?;
        let diag = diagnostics?;
        let (
            slot,
            ticket,
            (provider_id, opt_mode, session_id, resolved_model, prompt_preview, output_preview),
        ) = {
            let mut guard = self.hub.lock_inner();
            if !guard.context_store.is_capturing() {
                return None;
            }
            let task = guard.active_tasks.get(&self.task_id)?;
            let fields = (
                task.provider_id.clone(),
                task.optimization_mode,
                task.session_id.clone(),
                task.resolved_model.clone(),
                task.prompt_preview.clone(),
                task.output_preview.clone(),
            );
            // Admit before materializing: when the disk falls behind, skip this turn rather than
            // queue another multi-megabyte payload in memory.
            let Ok(slot) = Arc::clone(&self.hub.context_write_slots).try_acquire_owned() else {
                guard.context_store.note_skipped();
                tracing::warn!(
                    task_id = %self.task_id,
                    "skipping full-context capture: earlier captures are still being written"
                );
                return None;
            };
            // Reserved now, in completion order; the blocking writer may finish later.
            (slot, guard.context_store.reserve(), fields)
        };
        let TurnTextCapture {
            reasoning,
            output,
            reasoning_truncated,
            output_truncated,
            tool_result_rewrites,
            tool_result_rewrite_bytes: _,
        } = {
            let mut buf = match capture.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            std::mem::take(&mut *buf)
        };
        let request = build_full_request_payload(
            &self.task_id,
            now_ms,
            diag,
            diag.clamped_delta_start(),
            &output_preview,
            CONTEXT_CAPTURE_PAYLOAD_BUDGET,
        );
        let mut record = ContextCaptureRecord {
            task_id: self.task_id.clone(),
            recorded_at_unix_ms: now_ms,
            provider_id,
            optimization_mode: opt_mode,
            session_id,
            harness_profile: Some(self.harness_profile().to_owned()),
            resolved_model,
            session_turn_index: diag.session_turn_index,
            cache_hit_rate_bps: usage.cache_hit_bps(),
            total_prompt_tokens: usage.total_prompt_tokens(),
            output_tokens: usage.output_tokens,
            message_count: diag.messages.len(),
            delta_message_count: measure.map_or(0, |m| m.delta_message_count),
            payload_bytes: 0,
            prompt_preview,
            rewrite_count: None,
        };
        let (rewrites, rewrites_truncated) = collect_context_rewrites(diag, tool_result_rewrites);
        record.rewrite_count = Some(rewrites.len());
        let upstream_wire = match self.upstream_wire.lock() {
            Ok(mut g) => g.take(),
            Err(p) => p.into_inner().take(),
        };
        let mut payload = FullContextPayload {
            request,
            upstream_wire,
            assistant_reasoning: (!reasoning.is_empty()).then_some(reasoning),
            assistant_output: (!output.is_empty()).then_some(output),
            assistant_reasoning_truncated: reasoning_truncated,
            assistant_output_truncated: output_truncated,
            rewrites,
        };
        // One indicator for "this capture is incomplete", wherever the cut happened.
        payload.request.payload_truncated |=
            reasoning_truncated || output_truncated || rewrites_truncated;
        Some(PendingContextCapture {
            slot,
            ticket,
            record,
            payload,
        })
    }

    pub fn fail(mut self, error_message: String) {
        self.armed = false;
        let now_ms = unix_timestamp_ms();
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(mut task) = guard.active_tasks.remove(&self.task_id) {
                task.phase = TaskPhase::Failed;
                task.finished_at_unix_ms = Some(now_ms);
                task.elapsed_ms = now_ms.saturating_sub(task.started_at_unix_ms);
                task.error_message = Some(error_message);
                push_recent(&mut guard.recent_tasks, task);
            }
            guard.metrics.active_requests = guard.active_tasks.len();
            guard.metrics.failed_requests += 1;
        }
        self.hub.notify_change();
    }
}

impl Drop for ActiveTaskGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let now_ms = unix_timestamp_ms();
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(mut task) = guard.active_tasks.remove(&self.task_id) {
                task.phase = TaskPhase::Cancelled;
                task.finished_at_unix_ms = Some(now_ms);
                task.elapsed_ms = now_ms.saturating_sub(task.started_at_unix_ms);
                task.error_message = Some("client disconnected or turn cancelled".to_owned());
                push_recent(&mut guard.recent_tasks, task);
            }
            guard.metrics.active_requests = guard.active_tasks.len();
            guard.metrics.cancelled_requests += 1;
        }
        self.hub.notify_change();
    }
}

fn push_recent(deque: &mut VecDeque<TaskSnapshot>, task: TaskSnapshot) {
    if deque.len() >= MAX_RECENT_TASKS {
        let _ = deque.pop_front();
    }
    deque.push_back(task);
}

fn extract_last_user_preview(messages: &[CanonicalMessage]) -> String {
    for msg in messages.iter().rev() {
        if msg.role == CanonicalRole::User {
            for part in msg.parts.iter().rev() {
                match part {
                    CanonicalPart::Text(t) if !t.trim().is_empty() => {
                        return truncate_chars(t.trim(), MAX_PROMPT_PREVIEW_CHARS);
                    }
                    CanonicalPart::ToolResult {
                        call_id, output, ..
                    } => {
                        let summary = format!("[tool_result:{call_id}] {}", output.trim());
                        return truncate_chars(&summary, MAX_PROMPT_PREVIEW_CHARS);
                    }
                    _ => {}
                }
            }
        }
    }
    String::new()
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in input.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn append_preview_bounded(buf: &mut String, delta: &str, max_chars: usize) {
    let current_chars = buf.chars().count();
    if current_chars >= max_chars {
        return;
    }
    let remaining = max_chars - current_chars;
    for (idx, ch) in delta.chars().enumerate() {
        if idx >= remaining {
            buf.push('…');
            break;
        }
        buf.push(ch);
    }
}

#[must_use]
pub fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn is_developer_instructions_text(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.starts_with("<developer_instructions>")
        && trimmed.ends_with("</developer_instructions>")
}

fn message_content_bytes(msg: &CanonicalMessage) -> usize {
    msg.parts.iter().fold(0usize, |acc, part| {
        acc.saturating_add(part.estimated_heap_bytes())
    })
}

/// Byte and part counts of a conversation, split by content kind.
#[derive(Clone, Copy, Debug, Default)]
struct MessageMeasure {
    delta_message_count: usize,
    conversational_message_count: usize,
    user_message_count: usize,
    assistant_message_count: usize,
    total_conversation_bytes: usize,
    delta_messages_bytes: usize,
    developer_instructions_blocks: usize,
    developer_instructions_bytes: usize,
    user_text_bytes: usize,
    assistant_text_bytes: usize,
    reasoning_part_count: usize,
    reasoning_bytes: usize,
    tool_call_part_count: usize,
    tool_call_bytes: usize,
    tool_result_part_count: usize,
    tool_result_bytes: usize,
    image_part_count: usize,
    image_bytes: usize,
}

fn measure_messages(messages: &[CanonicalMessage], delta_start_index: usize) -> MessageMeasure {
    let mut m = MessageMeasure::default();
    let clamped_delta_start = delta_start_index.min(messages.len());
    m.delta_message_count = messages.len().saturating_sub(clamped_delta_start);

    for (idx, msg) in messages.iter().enumerate() {
        let msg_bytes = message_content_bytes(msg);
        m.total_conversation_bytes = m.total_conversation_bytes.saturating_add(msg_bytes);
        if idx >= clamped_delta_start {
            m.delta_messages_bytes = m.delta_messages_bytes.saturating_add(msg_bytes);
        }

        match msg.role {
            CanonicalRole::User => m.user_message_count = m.user_message_count.saturating_add(1),
            CanonicalRole::Assistant => {
                m.assistant_message_count = m.assistant_message_count.saturating_add(1);
            }
        }

        let mut has_conversational_part = false;
        for part in &msg.parts {
            match part {
                CanonicalPart::Text(t) => {
                    if is_developer_instructions_text(t) {
                        m.developer_instructions_blocks =
                            m.developer_instructions_blocks.saturating_add(1);
                        m.developer_instructions_bytes =
                            m.developer_instructions_bytes.saturating_add(t.len());
                    } else {
                        has_conversational_part = true;
                        match msg.role {
                            CanonicalRole::User => {
                                m.user_text_bytes = m.user_text_bytes.saturating_add(t.len());
                            }
                            CanonicalRole::Assistant => {
                                m.assistant_text_bytes =
                                    m.assistant_text_bytes.saturating_add(t.len());
                            }
                        }
                    }
                }
                CanonicalPart::Reasoning(r) => {
                    has_conversational_part = true;
                    m.reasoning_part_count = m.reasoning_part_count.saturating_add(1);
                    m.reasoning_bytes = m.reasoning_bytes.saturating_add(r.len());
                }
                CanonicalPart::ImageBase64 { media_type, data } => {
                    has_conversational_part = true;
                    m.image_part_count = m.image_part_count.saturating_add(1);
                    m.image_bytes = m
                        .image_bytes
                        .saturating_add(media_type.len())
                        .saturating_add(data.len());
                }
                CanonicalPart::ToolCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } => {
                    has_conversational_part = true;
                    m.tool_call_part_count = m.tool_call_part_count.saturating_add(1);
                    m.tool_call_bytes = m
                        .tool_call_bytes
                        .saturating_add(call_id.len())
                        .saturating_add(name.len())
                        .saturating_add(arguments.len());
                }
                CanonicalPart::ToolResult {
                    call_id, output, ..
                } => {
                    has_conversational_part = true;
                    m.tool_result_part_count = m.tool_result_part_count.saturating_add(1);
                    m.tool_result_bytes = m
                        .tool_result_bytes
                        .saturating_add(call_id.len())
                        .saturating_add(output.len());
                }
            }
        }
        if has_conversational_part {
            m.conversational_message_count = m.conversational_message_count.saturating_add(1);
        }
    }
    m
}

fn estimate_text_tokens(bytes: usize) -> u64 {
    u64::try_from(bytes.saturating_add(3) / 4).unwrap_or(u64::MAX)
}

/// Message-derived context segments of one turn (wire tools and bridge prompts are added by
/// the catalog builder from the live MCP handle).
fn message_context_segments(m: &MessageMeasure, tools_delta_bytes: usize) -> Vec<ContextSegment> {
    let text = |key, bytes| ContextSegment {
        key,
        bytes,
        tokens_est: estimate_text_tokens(bytes),
    };
    vec![
        text(ContextSegmentKey::ClientToolsUpdate, tools_delta_bytes),
        text(
            ContextSegmentKey::DeveloperInstructions,
            m.developer_instructions_bytes,
        ),
        text(ContextSegmentKey::UserText, m.user_text_bytes),
        text(ContextSegmentKey::AssistantText, m.assistant_text_bytes),
        text(ContextSegmentKey::Reasoning, m.reasoning_bytes),
        text(ContextSegmentKey::ToolCalls, m.tool_call_bytes),
        text(ContextSegmentKey::ToolResults, m.tool_result_bytes),
        ContextSegment {
            key: ContextSegmentKey::Images,
            bytes: m.image_bytes,
            tokens_est: u64::try_from(m.image_part_count)
                .unwrap_or(u64::MAX)
                .saturating_mul(IMAGE_TOKENS_ESTIMATE),
        },
    ]
}

#[expect(
    clippy::too_many_arguments,
    reason = "aggregates turn routing, token, and task-level counters into a single flat metrics struct"
)]
fn build_low_cache_request_metrics(
    diag: &TurnCompletionDiagnostics<'_>,
    measure: &MessageMeasure,
    session_reused: bool,
    stream: bool,
    usage: AcpUsage,
    thought_chars: usize,
    output_chars: usize,
    native_web_searches: usize,
    native_web_fetches: usize,
    vision_offloaded_images: usize,
    emitted_client_tools: Vec<String>,
    stop_reason: &str,
) -> (LowCacheRequestMetrics, LowCacheDiagnosticCause) {
    let MessageMeasure {
        delta_message_count,
        conversational_message_count,
        user_message_count,
        assistant_message_count,
        total_conversation_bytes,
        delta_messages_bytes,
        developer_instructions_blocks,
        developer_instructions_bytes,
        user_text_bytes,
        assistant_text_bytes,
        reasoning_part_count,
        reasoning_bytes,
        tool_call_part_count,
        tool_call_bytes,
        tool_result_part_count,
        tool_result_bytes,
        image_part_count,
        image_bytes,
    } = *measure;

    let client_tools_schema_bytes = diag.client_tools.iter().fold(0usize, |acc, t| {
        acc.saturating_add(t.estimated_heap_bytes())
    });

    let mut outbound_acp_prompt_bytes: usize = 0;
    let mut outbound_acp_text_blocks: usize = 0;
    let mut outbound_acp_image_blocks: usize = 0;
    for block in diag.outbound_acp_blocks {
        match block {
            AcpPromptBlock::Text { text } => {
                outbound_acp_text_blocks = outbound_acp_text_blocks.saturating_add(1);
                outbound_acp_prompt_bytes = outbound_acp_prompt_bytes.saturating_add(text.len());
            }
            AcpPromptBlock::Image { data, mime_type } => {
                outbound_acp_image_blocks = outbound_acp_image_blocks.saturating_add(1);
                outbound_acp_prompt_bytes = outbound_acp_prompt_bytes
                    .saturating_add(mime_type.len())
                    .saturating_add(data.len());
            }
        }
    }

    let cause = LowCacheDiagnosticCause::classify(
        session_reused,
        usage.cached_read_tokens,
        diag.previous_session_context_tokens,
        conversational_message_count,
        diag.session_idle_ms,
        diag.model_switched,
    );

    let metrics = LowCacheRequestMetrics {
        routing_strategy: diag.routing_strategy,
        session_turn_index: diag.session_turn_index,
        session_idle_ms: diag.session_idle_ms,
        previous_session_context_tokens: diag.previous_session_context_tokens,
        model_switched: diag.model_switched,
        previous_model: diag.previous_model.map(str::to_owned),
        previous_response_id: diag.previous_response_id.map(str::to_owned),
        prefix_hash_hex: diag.prefix_hash_hex.map(str::to_owned),
        base_state_hash_hex: diag.base_state_hash_hex.to_owned(),
        tools_updated_on_warm_turn: session_reused && diag.tools_delta_xml.is_some(),
        stream,
        reasoning_effort: diag.reasoning_effort,
        total_message_count: diag.messages.len(),
        conversational_message_count,
        user_message_count,
        assistant_message_count,
        delta_message_count,
        total_conversation_bytes,
        delta_messages_bytes,
        developer_instructions_blocks,
        developer_instructions_bytes,
        user_text_bytes,
        assistant_text_bytes,
        reasoning_part_count,
        reasoning_bytes,
        tool_call_part_count,
        tool_call_bytes,
        tool_result_part_count,
        tool_result_bytes,
        image_part_count,
        image_bytes,
        client_tool_count: diag.client_tools.len(),
        client_tools_schema_bytes,
        acp_system_append_bytes: diag.acp_system_append.map_or(0, str::len),
        tools_delta_xml_bytes: diag.tools_delta_xml.map_or(0, str::len),
        outbound_acp_prompt_bytes,
        outbound_acp_text_blocks,
        outbound_acp_image_blocks,
        thought_tokens: usage.thought_tokens,
        thought_chars,
        output_chars,
        native_web_searches,
        native_web_fetches,
        vision_offloaded_images,
        emitted_client_tools,
        stop_reason: (!stop_reason.is_empty()).then(|| stop_reason.to_owned()),
    };

    (metrics, cause)
}

fn build_and_persist_low_cache_capture(
    task_id: &str,
    recorded_at_unix_ms: u64,
    diag: &TurnCompletionDiagnostics<'_>,
    output_preview: &str,
    payload_dir: Option<&Path>,
) -> LowCacheRequestCapture {
    let clamped_delta_start = diag.delta_start_index.min(diag.messages.len());

    let mut combined_outbound = String::new();
    for (idx, block) in diag.outbound_acp_blocks.iter().enumerate() {
        if idx > 0 && !combined_outbound.ends_with('\n') {
            combined_outbound.push('\n');
        }
        match block {
            AcpPromptBlock::Text { text } => combined_outbound.push_str(text),
            AcpPromptBlock::Image { data, mime_type } => {
                use std::fmt::Write as _;
                let _ = write!(
                    &mut combined_outbound,
                    "[Image block: mime={mime_type}, base64_bytes={}]",
                    data.len()
                );
            }
        }
    }
    let outbound_acp_prompt_excerpt = truncate_head_tail_lines(
        &combined_outbound,
        MAX_INLINE_OUTBOUND_PROMPT_BYTES,
        "outbound ACP prompt",
    );

    let acp_system_append_excerpt = diag.acp_system_append.map(|sys| {
        truncate_head_tail_lines(
            sys,
            MAX_INLINE_SYSTEM_APPEND_BYTES,
            "ACP session systemPrompt.append",
        )
    });
    let tools_delta_xml_excerpt = diag.tools_delta_xml.map(|xml| {
        truncate_head_tail_lines(xml, MAX_INLINE_TOOLS_DELTA_BYTES, "client_tools_update XML")
    });

    let digest_start = diag
        .messages
        .len()
        .saturating_sub(MAX_INLINE_MESSAGE_DIGESTS);
    let mut message_digests = Vec::with_capacity(diag.messages.len().saturating_sub(digest_start));
    for (offset, msg) in diag
        .messages
        .get(digest_start..)
        .unwrap_or(&[])
        .iter()
        .enumerate()
    {
        let abs_idx = digest_start.saturating_add(offset);
        message_digests.push(summarize_message_inline(
            abs_idx,
            abs_idx >= clamped_delta_start,
            msg,
        ));
    }

    let mut payload_file_path: Option<String> = None;
    let mut full_payload_saved = false;
    if let Some(dir) = payload_dir {
        let full_payload = build_full_request_payload(
            task_id,
            recorded_at_unix_ms,
            diag,
            clamped_delta_start,
            output_preview,
            LOW_CACHE_PAYLOAD_BUDGET,
        );
        if let Some(saved_path) = persist_full_payload_atomic(dir, task_id, &full_payload) {
            full_payload_saved = true;
            payload_file_path = Some(saved_path.to_string_lossy().into_owned());
        }
    }

    LowCacheRequestCapture {
        client_tool_names: diag.client_tools.iter().map(|t| t.name.clone()).collect(),
        enable_native_web_search: diag.web_cfg.enable_native_web_search,
        force_native_web_search: diag.web_cfg.force_native_web_search,
        enable_native_web_fetch: diag.web_cfg.enable_native_web_fetch,
        force_native_web_fetch: diag.web_cfg.force_native_web_fetch,
        acp_system_append_excerpt,
        tools_delta_xml_excerpt,
        outbound_acp_prompt_excerpt,
        output_preview: output_preview.to_owned(),
        message_digests,
        full_payload_saved,
        payload_file_path,
    }
}

fn summarize_message_inline(
    index: usize,
    in_delta: bool,
    msg: &CanonicalMessage,
) -> CapturedMessageSummary {
    let role = match msg.role {
        CanonicalRole::User => "user".to_owned(),
        CanonicalRole::Assistant => "assistant".to_owned(),
    };
    let mut part_kinds = Vec::with_capacity(msg.parts.len());
    let mut excerpt = String::new();
    for part in &msg.parts {
        match part {
            CanonicalPart::Text(t) => {
                if is_developer_instructions_text(t) {
                    part_kinds.push(format!("developer_instructions({}B)", t.len()));
                } else {
                    part_kinds.push(format!("text({}B)", t.len()));
                }
                append_preview_bounded(&mut excerpt, t.trim(), MAX_INLINE_MESSAGE_EXCERPT_CHARS);
            }
            CanonicalPart::Reasoning(r) => {
                part_kinds.push(format!("reasoning({}B)", r.len()));
            }
            CanonicalPart::ImageBase64 { media_type, data } => {
                part_kinds.push(format!("image:{media_type}({}B)", data.len()));
            }
            CanonicalPart::ToolCall {
                name, arguments, ..
            } => {
                part_kinds.push(format!("tool_call:{name}({}B)", arguments.len()));
                append_preview_bounded(
                    &mut excerpt,
                    &format!("[tool_call:{name}] {arguments}"),
                    MAX_INLINE_MESSAGE_EXCERPT_CHARS,
                );
            }
            CanonicalPart::ToolResult {
                call_id,
                output,
                is_error,
            } => {
                let tag = if *is_error {
                    "tool_error"
                } else {
                    "tool_result"
                };
                part_kinds.push(format!("{tag}:{call_id}({}B)", output.len()));
                append_preview_bounded(
                    &mut excerpt,
                    &format!("[{tag}:{call_id}] {output}"),
                    MAX_INLINE_MESSAGE_EXCERPT_CHARS,
                );
            }
        }
    }
    CapturedMessageSummary {
        index,
        role,
        in_delta,
        total_bytes: message_content_bytes(msg),
        part_kinds,
        excerpt,
    }
}

fn bound_part_string(
    raw: &str,
    budget_remaining: &mut usize,
    per_part: usize,
    truncated: &mut bool,
) -> String {
    let cap = (*budget_remaining).min(per_part);
    if cap == 0 {
        if !raw.is_empty() {
            *truncated = true;
        }
        return "[...omitted: diagnostic payload exceeded its safety cap...]".to_owned();
    }
    if raw.len() <= cap {
        *budget_remaining = budget_remaining.saturating_sub(raw.len());
        raw.to_owned()
    } else {
        *truncated = true;
        let bounded = truncate_head_tail_lines(raw, cap, "diagnostic payload part");
        *budget_remaining = budget_remaining.saturating_sub(bounded.len());
        bounded
    }
}

/// Keeps a raster image's base64 payload while it fits the per-image and remaining image budgets.
fn bound_image_data(
    media_type: &str,
    data: &str,
    budget_remaining: &mut usize,
    per_image: usize,
) -> Option<String> {
    let kept = crate::tool_calls::previewable_image_data(
        media_type,
        data,
        per_image.min(*budget_remaining),
    )?;
    *budget_remaining = budget_remaining.saturating_sub(kept.len());
    Some(kept)
}

fn build_full_request_payload(
    task_id: &str,
    recorded_at_unix_ms: u64,
    diag: &TurnCompletionDiagnostics<'_>,
    clamped_delta_start: usize,
    output_preview: &str,
    limits: PayloadBudget,
) -> LowCacheFullRequestPayload {
    let mut budget = limits.total;
    let per_part = limits.per_part;
    let mut image_budget = limits.images_total;
    let mut truncated = false;

    let acp_system_append = diag
        .acp_system_append
        .map(|s| bound_part_string(s, &mut budget, per_part, &mut truncated));
    let tools_delta_xml = diag
        .tools_delta_xml
        .map(|s| bound_part_string(s, &mut budget, per_part, &mut truncated));

    let mut outbound_acp_prompt_blocks = Vec::with_capacity(diag.outbound_acp_blocks.len());
    for block in diag.outbound_acp_blocks {
        match block {
            AcpPromptBlock::Text { text } => {
                let bytes = text.len();
                let saved = bound_part_string(text, &mut budget, per_part, &mut truncated);
                outbound_acp_prompt_blocks
                    .push(CapturedAcpPromptBlock::Text { bytes, text: saved });
            }
            AcpPromptBlock::Image { data, mime_type } => {
                outbound_acp_prompt_blocks.push(CapturedAcpPromptBlock::Image {
                    mime_type: mime_type.clone(),
                    base64_bytes: data.len(),
                    data: bound_image_data(mime_type, data, &mut image_budget, limits.per_image),
                });
            }
        }
    }

    // Prioritize leading developer/repo instruction blocks (`0..2`) AND the newest delta/tail
    // messages (`len - 1` down to `2`) before older archival turns so >2 MiB sessions always
    // preserve the exact active turn context instead of exhausting the cap on old history.
    let total_msgs = diag.messages.len();
    let mut priority_indices = Vec::with_capacity(total_msgs);
    let pinned_head = total_msgs.min(2);
    for idx in 0..pinned_head {
        priority_indices.push(idx);
    }
    for idx in (pinned_head..total_msgs).rev() {
        priority_indices.push(idx);
    }

    let mut messages = Vec::with_capacity(total_msgs);
    for index in priority_indices {
        let Some(msg) = diag.messages.get(index) else {
            continue;
        };
        let role = match msg.role {
            CanonicalRole::User => "user".to_owned(),
            CanonicalRole::Assistant => "assistant".to_owned(),
        };
        let mut parts = Vec::with_capacity(msg.parts.len());
        for part in &msg.parts {
            match part {
                CanonicalPart::Text(t) => {
                    let is_dev = is_developer_instructions_text(t);
                    let bytes = t.len();
                    let text = bound_part_string(t, &mut budget, per_part, &mut truncated);
                    parts.push(CapturedFullPart::Text {
                        is_developer_instructions: is_dev,
                        bytes,
                        text,
                    });
                }
                CanonicalPart::Reasoning(r) => {
                    let bytes = r.len();
                    let text = bound_part_string(r, &mut budget, per_part, &mut truncated);
                    parts.push(CapturedFullPart::Reasoning { bytes, text });
                }
                CanonicalPart::ImageBase64 { media_type, data } => {
                    parts.push(CapturedFullPart::Image {
                        media_type: media_type.clone(),
                        base64_bytes: data.len(),
                        data: bound_image_data(
                            media_type,
                            data,
                            &mut image_budget,
                            limits.per_image,
                        ),
                    });
                }
                CanonicalPart::ToolCall {
                    call_id,
                    name,
                    arguments,
                    is_custom,
                } => {
                    let bytes = arguments.len();
                    let saved_args =
                        bound_part_string(arguments, &mut budget, per_part, &mut truncated);
                    parts.push(CapturedFullPart::ToolCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        is_custom: *is_custom,
                        bytes,
                        arguments: saved_args,
                    });
                }
                CanonicalPart::ToolResult {
                    call_id,
                    output,
                    is_error,
                } => {
                    let bytes = output.len();
                    let saved_out =
                        bound_part_string(output, &mut budget, per_part, &mut truncated);
                    parts.push(CapturedFullPart::ToolResult {
                        call_id: call_id.clone(),
                        is_error: *is_error,
                        bytes,
                        output: saved_out,
                    });
                }
            }
        }
        messages.push(CapturedFullMessage {
            index,
            role,
            in_delta: index >= clamped_delta_start,
            total_bytes: message_content_bytes(msg),
            parts,
        });
    }
    messages.sort_by_key(|m| m.index);

    let mut client_tools = Vec::with_capacity(diag.client_tools.len());
    for tool in diag.client_tools {
        let (kind, raw_schema) = match &tool.kind {
            CanonicalToolKind::Function { parameters_json } => {
                ("function".to_owned(), Some(parameters_json.as_str()))
            }
            CanonicalToolKind::Custom { format_json } => {
                ("custom".to_owned(), format_json.as_deref())
            }
        };
        let description = tool
            .description
            .as_deref()
            .map(|d| bound_part_string(d, &mut budget, per_part, &mut truncated));
        let schema_or_format =
            raw_schema.map(|s| bound_part_string(s, &mut budget, per_part, &mut truncated));
        client_tools.push(CapturedToolSpec {
            name: tool.name.clone(),
            kind,
            schema_bytes: tool.estimated_heap_bytes(),
            description,
            schema_or_format,
        });
    }

    LowCacheFullRequestPayload {
        task_id: task_id.to_owned(),
        recorded_at_unix_ms,
        acp_system_append,
        tools_delta_xml,
        outbound_acp_prompt_blocks,
        messages,
        client_tools,
        assistant_output_preview: output_preview.to_owned(),
        payload_truncated: truncated,
    }
}

fn sanitize_task_id_filename(task_id: &str) -> Option<String> {
    let mut safe = String::with_capacity(task_id.len().min(96) + 5);
    for ch in task_id.chars().take(96) {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            safe.push(ch);
        }
    }
    if safe.is_empty() {
        return None;
    }
    safe.push_str(".json");
    Some(safe)
}

fn persist_full_payload_atomic(
    payload_dir: &Path,
    task_id: &str,
    payload: &LowCacheFullRequestPayload,
) -> Option<PathBuf> {
    let file_name = sanitize_task_id_filename(task_id)?;
    let target_path = payload_dir.join(file_name);
    let mut json_bytes = serde_json::to_vec_pretty(payload).ok()?;
    json_bytes.push(b'\n');
    if let Err(err) = write_bytes_atomic_private(&target_path, &json_bytes) {
        tracing::warn!(
            path = %target_path.display(),
            error = ?err,
            "failed to persist full low-cache request payload"
        );
        return None;
    }
    Some(target_path)
}

fn load_full_payload_for_record(
    record: &LowCacheHitRecord,
    payload_dir: Option<&Path>,
) -> Option<LowCacheFullRequestPayload> {
    let dir = payload_dir?;
    let file_name = sanitize_task_id_filename(&record.task_id)?;
    let target = dir.join(file_name);
    let bytes = fs::read(&target).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn remove_payload_files(payload_dir: Option<&Path>, records: &[LowCacheHitRecord]) {
    let Some(dir) = payload_dir else {
        return;
    };
    for rec in records {
        if let Some(file_name) = sanitize_task_id_filename(&rec.task_id) {
            let _ = fs::remove_file(dir.join(file_name));
        }
    }
}

fn clear_payload_directory(payload_dir: Option<&Path>) {
    let Some(dir) = payload_dir else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let _ = fs::remove_file(path);
        }
    }
}

fn prune_orphan_payload_files(
    payload_dir: Option<&Path>,
    retained_records: &VecDeque<LowCacheHitRecord>,
) {
    let Some(dir) = payload_dir else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let live_files: std::collections::HashSet<String> = retained_records
        .iter()
        .filter_map(|r| sanitize_task_id_filename(&r.task_id))
        .collect();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let keep = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| live_files.contains(name));
            if !keep {
                let _ = fs::remove_file(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use liftgate_types::telemetry::TaskProtocol;

    #[test]
    fn set_tool_context_attributes_harness_to_active_and_recent_task() {
        let hub = Arc::new(TelemetryHub::new());
        let guard = hub.begin_task(
            "task_harness".to_owned(),
            TaskProtocol::OpenaiResponses,
            true,
            "gemini-4-carbon-high".to_owned(),
            None,
            &[],
        );
        let (_, _, active, _) = hub.snapshot_tasks();
        assert_eq!(
            active.first().and_then(|t| t.harness_profile.as_deref()),
            None
        );

        guard.bind_session("sess_harness", false, "gemini-4-carbon-high");
        guard.set_tool_context("codex", Some("codex_exec"), 1);
        let (_, _, active, _) = hub.snapshot_tasks();
        assert_eq!(
            active.first().and_then(|t| t.harness_profile.as_deref()),
            Some("codex")
        );

        guard.complete("end_turn".to_owned(), AcpUsage::default());
        let (_, _, _, recent) = hub.snapshot_tasks();
        assert_eq!(
            recent.first().and_then(|t| t.harness_profile.as_deref()),
            Some("codex")
        );
    }

    #[test]
    fn reconcile_cascade_usage_drift_keeps_cold_step_cached_reads_out_of_warm_totals() {
        let hub = Arc::new(TelemetryHub::new());
        let guard = hub.begin_task(
            "task_cold_cascade".to_owned(),
            TaskProtocol::OpenaiResponses,
            false,
            "gemini-4-carbon-high".to_owned(),
            None,
            &[],
        );
        guard.bind_session("sess_cold_1", false, "gemini-4-carbon-high");
        let step1_est = AcpUsage {
            input_tokens: 1_000,
            cached_read_tokens: 0,
            output_tokens: 50,
            total_tokens: 1_050,
            ..AcpUsage::default()
        };
        guard.complete("tool_use".to_owned(), step1_est);

        let guard2 = hub.begin_task(
            "task_warm_step2".to_owned(),
            TaskProtocol::OpenaiResponses,
            false,
            "gemini-4-carbon-high".to_owned(),
            None,
            &[],
        );
        guard2.bind_session("sess_cold_1", true, "gemini-4-carbon-high");
        let step2_est = AcpUsage {
            input_tokens: 200,
            cached_read_tokens: 1_050,
            output_tokens: 80,
            total_tokens: 1_330,
            ..AcpUsage::default()
        };
        guard2.complete("end_turn".to_owned(), step2_est);

        // Cold step 1 actually got a 400-token implicit upstream cache hit (`1450` vs `1050` cr).
        let accounted = step1_est.saturating_add(step2_est);
        let authoritative = AcpUsage {
            input_tokens: 800,
            cached_read_tokens: 1_450,
            output_tokens: 130,
            total_tokens: 2_380,
            ..AcpUsage::default()
        };
        hub.reconcile_cascade_usage_drift(
            "gemini-4-carbon-high",
            hub.usage_epoch(),
            accounted,
            authoritative,
            false,
        );

        let summary = hub.low_cache_hits().summary;
        assert_eq!(summary.total_prompt_tokens, 2_250);
        assert_eq!(summary.total_cached_read_tokens, 1_450);
        assert_eq!(summary.warm_prompt_tokens, 1_250);
        assert_eq!(summary.warm_cached_read_tokens, 1_050);
    }

    fn capture_test_diagnostics(messages: &[CanonicalMessage]) -> TurnCompletionDiagnostics<'_> {
        TurnCompletionDiagnostics {
            routing_strategy: LowCacheRoutingStrategy::PrefixHash,
            delta_start_index: 0,
            session_turn_index: 1,
            session_idle_ms: None,
            previous_session_context_tokens: 0,
            model_switched: false,
            previous_model: None,
            previous_response_id: None,
            prefix_hash_hex: None,
            base_state_hash_hex: "00",
            reasoning_effort: None,
            web_cfg: WebToolBridgeConfig::default(),
            acp_system_append: None,
            tools_delta_xml: None,
            outbound_acp_blocks: &[],
            messages,
            client_tools: &[],
            distill_harness: None,
            developer_slots: &[],
        }
    }

    #[test]
    fn full_context_capture_skips_turns_instead_of_queueing_behind_slow_writes() {
        let dir = std::env::temp_dir().join(format!("liftgate-ctx-backlog-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let hub = Arc::new(TelemetryHub::with_telemetry_config(TelemetryConfig {
            context_capture: ContextCaptureConfig {
                enabled: true,
                dir: Some(dir.clone()),
                max_pending_writes: std::num::NonZeroUsize::MIN,
                ..ContextCaptureConfig::default()
            },
            ..TelemetryConfig::default()
        }));
        let messages = vec![CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::Text("hi".to_owned())],
        }];
        let run_turn = |task_id: &str| {
            let guard = hub.begin_task(
                task_id.to_owned(),
                TaskProtocol::OpenaiResponses,
                false,
                "gemini-4-carbon-high".to_owned(),
                None,
                &messages,
            );
            guard.bind_session("sess_capture", false, "gemini-4-carbon-high");
            guard.complete_with_diagnostics(
                "end_turn".to_owned(),
                AcpUsage::default(),
                Some(capture_test_diagnostics(&messages)),
            );
        };

        // An earlier write still holds the only slot: the turn is counted as skipped, not queued.
        let held = Arc::clone(&hub.context_write_slots)
            .try_acquire_owned()
            .expect("free write slot");
        run_turn("task_backlogged");
        let summary = hub.context_capture_summary();
        assert_eq!(summary.skipped_records, 1);
        assert_eq!(summary.stored_records, 0);

        // With the slot free again the next turn is captured (inline: no runtime in this test)
        // and its slot is returned once the write finishes.
        drop(held);
        run_turn("task_captured");
        let listed = hub.context_captures();
        assert_eq!(listed.summary.skipped_records, 1);
        let ids: Vec<&str> = listed.records.iter().map(|r| r.task_id.as_str()).collect();
        assert_eq!(ids, ["task_captured"]);
        assert_eq!(hub.context_write_slots.available_permits(), 1);
        assert!(
            hub.context_capture_detail("task_captured")
                .expect("read capture")
                .is_some()
        );

        // Clearing resets the skip counter along with the captures.
        let cleared = hub.clear_context_captures();
        assert_eq!((cleared.stored_records, cleared.skipped_records), (0, 0));

        // Output beyond the per-field cap is dropped, and the capture says so.
        let guard = hub.begin_task(
            "task_long_output".to_owned(),
            TaskProtocol::OpenaiResponses,
            false,
            "gemini-4-carbon-high".to_owned(),
            None,
            &messages,
        );
        guard.bind_session("sess_capture", false, "gemini-4-carbon-high");
        guard.record_thought("short reasoning");
        let chunk = "é".repeat(64 * 1024);
        for _ in 0..12 {
            guard.record_text(&chunk);
        }
        guard.complete_with_diagnostics(
            "end_turn".to_owned(),
            AcpUsage::default(),
            Some(capture_test_diagnostics(&messages)),
        );
        let detail = hub
            .context_capture_detail("task_long_output")
            .expect("read capture")
            .expect("captured");
        let output = detail.payload.assistant_output.unwrap_or_default();
        assert!(output.len() <= MAX_CAPTURED_TURN_TEXT_BYTES);
        assert!(output.len() > MAX_CAPTURED_TURN_TEXT_BYTES - 4);
        assert!(detail.payload.assistant_output_truncated);
        assert!(!detail.payload.assistant_reasoning_truncated);
        assert!(detail.payload.request.payload_truncated);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_context_capture_lists_gateway_context_rewrites() {
        use liftgate_types::canonical::ProtocolDeveloperSlotKind;

        let dir = std::env::temp_dir().join(format!("liftgate-ctx-rewrites-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let hub = Arc::new(TelemetryHub::with_telemetry_config(TelemetryConfig {
            context_capture: ContextCaptureConfig {
                enabled: true,
                dir: Some(dir.clone()),
                ..ContextCaptureConfig::default()
            },
            ..TelemetryConfig::default()
        }));
        let harness_prompt = format!(
            "<developer_instructions>\nYou are Codex, a coding agent.\n{}\n</developer_instructions>",
            "Follow the harness tutorial. ".repeat(200)
        );
        let messages = vec![
            CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text(harness_prompt.clone())],
            },
            CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::ToolResult {
                    call_id: "call_abc123".to_owned(),
                    output: "\u{1b}[31mred\u{1b}[0m".to_owned(),
                    is_error: false,
                }],
            },
        ];
        let slots = [ProtocolDeveloperSlot {
            msg_index: 0,
            part_index: 0,
            kind: ProtocolDeveloperSlotKind::PrimaryBase,
        }];
        let guard = hub.begin_task(
            "task_rewrites".to_owned(),
            TaskProtocol::OpenaiResponses,
            false,
            "gemini-4-carbon-high".to_owned(),
            None,
            &messages,
        );
        guard.bind_session("sess_rewrites", false, "gemini-4-carbon-high");
        guard.record_tool_result_rewrite(
            "call_abc123",
            "abc123",
            "exec",
            "\u{1b}[31mred\u{1b}[0m",
            "red",
            false,
        );
        let mut diag = capture_test_diagnostics(&messages);
        diag.distill_harness = Some(DetectedHarness::Codex);
        diag.developer_slots = &slots;
        guard.complete_with_diagnostics("end_turn".to_owned(), AcpUsage::default(), Some(diag));

        let detail = hub
            .context_capture_detail("task_rewrites")
            .expect("read capture")
            .expect("captured");
        assert_eq!(detail.record.rewrite_count, Some(2));
        let rewrites = &detail.payload.rewrites;
        assert_eq!(rewrites.len(), 2);
        let prompt = rewrites.first().expect("prompt rewrite");
        assert_eq!(prompt.kind, ContextRewriteKind::PromptDistilled);
        assert_eq!(
            (prompt.message_index, prompt.part_index),
            (Some(0), Some(0))
        );
        assert_eq!(prompt.slot.as_deref(), Some("primary_base"));
        assert_eq!(prompt.original, harness_prompt);
        assert_eq!(prompt.original_bytes, harness_prompt.len());
        let distilled = prompt.rewritten.as_deref().expect("distilled text");
        assert_ne!(distilled, harness_prompt);
        assert_eq!(prompt.rewritten_bytes, distilled.len());
        let tool = rewrites.get(1).expect("tool rewrite");
        assert_eq!(tool.kind, ContextRewriteKind::ToolResultSanitized);
        assert_eq!((tool.message_index, tool.part_index), (Some(1), Some(0)));
        assert_eq!(tool.tool_name.as_deref(), Some("exec"));
        assert_eq!(tool.rewritten.as_deref(), Some("red"));
        assert!(!detail.payload.request.payload_truncated);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn context_rewrites_distinguish_spills_and_match_delta_or_unprefixed_ids() {
        let dir = std::env::temp_dir().join(format!("liftgate-ctx-spill-rw-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let hub = Arc::new(TelemetryHub::with_telemetry_config(TelemetryConfig {
            context_capture: ContextCaptureConfig {
                enabled: true,
                dir: Some(dir.clone()),
                ..ContextCaptureConfig::default()
            },
            ..TelemetryConfig::default()
        }));
        // Message 0 has an unrelated `step_1` id that used to collide with `call_1` suffix matching;
        // Message 1 is in delta and carries both `toolu_1` and unprefixed `99`.
        let messages = vec![
            CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::ToolResult {
                    call_id: "step_1".to_owned(),
                    output: "old".to_owned(),
                    is_error: false,
                }],
            },
            CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![
                    CanonicalPart::ToolResult {
                        call_id: "toolu_1".to_owned(),
                        output: "x".repeat(12_000),
                        is_error: false,
                    },
                    CanonicalPart::ToolResult {
                        call_id: "99".to_owned(),
                        output: "\u{1b}[32mok\u{1b}[0m".to_owned(),
                        is_error: false,
                    },
                ],
            },
        ];
        let guard = hub.begin_task(
            "task_spill_rw".to_owned(),
            TaskProtocol::AnthropicMessages,
            false,
            "gemini-4-carbon-high".to_owned(),
            None,
            &messages,
        );
        guard.bind_session("sess_spill_rw", true, "gemini-4-carbon-high");
        guard.record_tool_result_rewrite(
            "call_1",
            "1",
            "Read",
            &"x".repeat(12_000),
            "x...spilled",
            true,
        );
        guard.record_tool_result_rewrite(
            "call_99",
            "99",
            "Bash",
            "\u{1b}[32mok\u{1b}[0m",
            "ok",
            false,
        );
        let mut diag = capture_test_diagnostics(&messages);
        diag.delta_start_index = 1;
        guard.complete_with_diagnostics("end_turn".to_owned(), AcpUsage::default(), Some(diag));

        let detail = hub
            .context_capture_detail("task_spill_rw")
            .expect("read capture")
            .expect("captured");
        assert_eq!(detail.record.rewrite_count, Some(2));
        let rws = &detail.payload.rewrites;
        assert_eq!(rws.len(), 2);
        assert_eq!(rws[0].kind, ContextRewriteKind::ToolResultSpilled);
        assert_eq!(
            (rws[0].message_index, rws[0].part_index),
            (Some(1), Some(0))
        );
        assert_eq!(rws[0].call_id.as_deref(), Some("toolu_1"));
        assert_eq!(rws[1].kind, ContextRewriteKind::ToolResultSanitized);
        assert_eq!(
            (rws[1].message_index, rws[1].part_index),
            (Some(1), Some(1))
        );
        assert_eq!(rws[1].call_id.as_deref(), Some("99"));

        // Legacy capture records without `rewrite_count` decode as `None`.
        let legacy: ContextCaptureRecord = serde_json::from_str(
            r#"{"task_id":"old","recorded_at_unix_ms":1,"resolved_model":"m"}"#,
        )
        .expect("decode legacy record");
        assert_eq!(legacy.rewrite_count, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_ledger_prices_completed_turns_and_survives_restart() {
        let dir = std::env::temp_dir().join(format!("liftgate-usage-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let config = || TelemetryConfig {
            cache: CacheTelemetryConfig {
                persist_path: Some(dir.join("cache-telemetry.json")),
                ..CacheTelemetryConfig::default()
            },
            pricing_path: Some(dir.join("model-pricing.json")),
            usage_path: Some(dir.join("usage-ledger.json")),
            ..TelemetryConfig::default()
        };
        let hub = Arc::new(TelemetryHub::with_telemetry_config(config()));
        let guard = hub.begin_task(
            "task_priced".to_owned(),
            TaskProtocol::OpenaiResponses,
            false,
            "gpt-5.4".to_owned(),
            None,
            &[],
        );
        guard.bind_session("sess_priced", false, "gemini-4-hydrogen-high");
        guard.complete(
            "end_turn".to_owned(),
            AcpUsage {
                input_tokens: 1_000_000,
                output_tokens: 100_000,
                cached_read_tokens: 2_000_000,
                cached_write_tokens: 0,
                thought_tokens: 40_000,
                total_tokens: 3_100_000,
            },
        );

        // Hydrogen: $1.50 input + 2 x $0.075 cached + 0.1 x $7.50 output.
        let report = hub.usage_report(None);
        assert_eq!(report.summary.calls, 1);
        assert!((report.summary.cost_usd - 2.40).abs() < 1e-9, "{report:?}");
        let model = report.models.first().expect("model row");
        assert_eq!(model.model_id, "gemini-4-hydrogen-high");
        assert_eq!(model.pricing_key, "gemini-4-hydrogen");
        assert_eq!(model.tokens.thought, 40_000);
        let (_, _, _, recent) = hub.snapshot_tasks();
        let task_cost = recent
            .first()
            .and_then(|t| t.cost_usd)
            .expect("priced task");
        assert!((task_cost - 2.40).abs() < 1e-9);

        // A custom price re-prices recorded usage and listed requests alike.
        let custom = ModelPrice {
            input: 1.0,
            cached_input: 0.0,
            cache_write: 1.0,
            output: 0.0,
        };
        assert_eq!(
            hub.set_custom_price("gemini-4-hydrogen", Some(custom)),
            PriceUpdate::Changed
        );
        assert!((hub.usage_summary().cost_usd - 1.0).abs() < 1e-9);
        let (_, _, _, recent) = hub.snapshot_tasks();
        assert_eq!(recent.first().and_then(|t| t.cost_usd), Some(1.0));
        let pricing = hub.pricing_report(&[]);
        let hydrogen = pricing
            .entries
            .iter()
            .find(|e| e.key == "gemini-4-hydrogen")
            .expect("hydrogen entry");
        assert_eq!(hydrogen.custom_price, Some(custom));
        assert_eq!(hydrogen.model_ids, ["gemini-4-hydrogen-high"]);
        drop(hub);

        // Usage, its history and custom prices survive a restart.
        let hub = TelemetryHub::with_telemetry_config(config());
        let restored = hub.usage_report(None);
        assert_eq!(restored.summary.calls, 1);
        assert!((restored.summary.cost_usd - 1.0).abs() < 1e-9);
        assert!(restored.summary.since_unix_ms > 0);
        let now = unix_timestamp_ms();
        let today = hub.usage_report(Some((now - 86_400_000, now + 1))).summary;
        assert_eq!(today.calls, 1);
        assert!((today.cost_usd - 1.0).abs() < 1e-9);
        let days = hub.usage_series(&[now - 2 * 86_400_000, now - 86_400_000, now + 1]);
        let calls: Vec<u64> = days.intervals.iter().map(|d| d.calls).collect();
        assert_eq!(calls, [0, 1]);
        let reset = hub.reset_usage();
        assert_eq!((reset.calls, reset.tokens.total()), (0, 0));
        drop(hub);
        let hub = TelemetryHub::with_telemetry_config(config());
        assert_eq!(hub.usage_summary().calls, 0, "reset is persisted");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn push_bounded_reports_dropped_text_on_char_boundaries() {
        let mut buf = String::new();
        assert!(!push_bounded(&mut buf, "abc", 5));
        assert!(
            push_bounded(&mut buf, "dé", 5),
            "`é` does not fit in the last byte"
        );
        assert_eq!(buf, "abcd");
        assert!(push_bounded(&mut buf, "x", 4));
        assert!(!push_bounded(&mut buf, "", 4));
        assert_eq!(buf, "abcd");
    }

    #[test]
    fn sync_settings_json_bool_field_updates_matching_pid_daemon_json_only() {
        let dir = std::env::temp_dir().join(format!("liftgate-tel-sync-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let settings_path = dir.join("settings.json");
        let daemon_path = dir.join("daemon.json");

        let matching = serde_json::json!({
            "pid": std::process::id(),
            "mcp_tool_bridge": true,
            "vision_offload": true
        });
        fs::write(
            &daemon_path,
            serde_json::to_vec_pretty(&matching).expect("serialize"),
        )
        .expect("write daemon.json");

        sync_settings_json_bool_field(&settings_path, "mcp_tool_bridge", false);
        let synced_daemon: serde_json::Value =
            serde_json::from_slice(&fs::read(&daemon_path).expect("read")).expect("parse");
        assert_eq!(synced_daemon["mcp_tool_bridge"], false);
        assert_eq!(synced_daemon["vision_offload"], true);

        let foreign = serde_json::json!({
            "pid": std::process::id().wrapping_add(99_999),
            "mcp_tool_bridge": false
        });
        fs::write(
            &daemon_path,
            serde_json::to_vec_pretty(&foreign).expect("serialize"),
        )
        .expect("write foreign daemon.json");

        sync_settings_json_bool_field(&settings_path, "mcp_tool_bridge", true);
        let foreign_after: serde_json::Value =
            serde_json::from_slice(&fs::read(&daemon_path).expect("read")).expect("parse");
        assert_eq!(foreign_after["mcp_tool_bridge"], false);

        let _ = fs::remove_dir_all(&dir);
    }
}
