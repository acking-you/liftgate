use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use liftgate_types::{
    acp::{AcpPromptBlock, AcpUsage, compute_cache_hit_bps},
    canonical::{
        AuxiliaryTaskKind, CanonicalMessage, CanonicalPart, CanonicalRole, CanonicalToolKind,
        CanonicalToolSpec, ReasoningEffort,
    },
    config::{CacheHitThresholdBps, LowCacheRecordLimit},
    session_key::{WebToolBridgeConfig, truncate_head_tail_lines},
    telemetry::{
        CacheTelemetrySummary, CapturedAcpPromptBlock, CapturedFullMessage, CapturedFullPart,
        CapturedMessageSummary, CapturedToolSpec, GatewayMetricsSummary, LowCacheDiagnosticCause,
        LowCacheFullRequestPayload, LowCacheHitReason, LowCacheHitRecord, LowCacheHitsResponse,
        LowCachePayloadDetailResponse, LowCacheRequestCapture, LowCacheRequestMetrics,
        LowCacheRoutingStrategy, TaskPhase, TaskProtocol, TaskSnapshot,
    },
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{broadcast, watch};
use uuid::Uuid;

const MAX_RECENT_TASKS: usize = 100;
const MAX_PROMPT_PREVIEW_CHARS: usize = 180;
const MAX_OUTPUT_PREVIEW_CHARS: usize = 240;
const PERSISTED_CACHE_SCHEMA_VERSION: u32 = 1;
const MAX_INLINE_SYSTEM_APPEND_BYTES: usize = 16 * 1024;
const MAX_INLINE_TOOLS_DELTA_BYTES: usize = 8 * 1024;
const MAX_INLINE_OUTBOUND_PROMPT_BYTES: usize = 24 * 1024;
const MAX_INLINE_MESSAGE_DIGESTS: usize = 24;
const MAX_INLINE_MESSAGE_EXCERPT_CHARS: usize = 600;
const MAX_FULL_PART_BYTES: usize = 256 * 1024;
const MAX_FULL_PAYLOAD_BUDGET_BYTES: usize = 2 * 1024 * 1024;

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
    let mut map: serde_json::Map<String, serde_json::Value> = match fs::read(settings_path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(err) => {
            tracing::warn!(
                path = %settings_path.display(),
                key,
                error = ?err,
                "failed to read settings.json while syncing runtime boolean toggle"
            );
            return;
        }
    };
    map.insert(key.to_owned(), serde_json::Value::from(value));
    match serde_json::to_vec_pretty(&map) {
        Ok(mut json) => {
            json.push(b'\n');
            if let Err(err) = write_bytes_atomic_private(settings_path, &json) {
                tracing::warn!(
                    path = %settings_path.display(),
                    key,
                    error = ?err,
                    "failed to persist updated runtime boolean toggle to settings.json"
                );
            }
        }
        Err(err) => {
            tracing::warn!(
                path = %settings_path.display(),
                key,
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
            daemon_map.insert(key.to_owned(), serde_json::Value::from(value));
            if let Ok(mut daemon_json) = serde_json::to_vec_pretty(&daemon_map) {
                daemon_json.push(b'\n');
                if let Err(err) = write_bytes_atomic_private(&daemon_path, &daemon_json) {
                    tracing::warn!(
                        path = %daemon_path.display(),
                        key,
                        error = ?err,
                        "failed to persist updated runtime boolean toggle to daemon.json"
                    );
                }
            }
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

/// Central bounded telemetry store and real-time event notifier for the companion web UI.
pub struct TelemetryHub {
    inner: Mutex<TelemetryInner>,
    persist_lock: Mutex<u64>,
    notify_tx: broadcast::Sender<()>,
    shutdown_tx: watch::Sender<bool>,
}

struct TelemetryInner {
    metrics: GatewayMetricsSummary,
    cache_store: CacheTelemetryStore,
    active_tasks: BTreeMap<String, TaskSnapshot>,
    recent_tasks: VecDeque<TaskSnapshot>,
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
        let (notify_tx, _) = broadcast::channel(64);
        let (shutdown_tx, _) = watch::channel(false);
        let cache_store = CacheTelemetryStore::from_config(config);
        Self {
            inner: Mutex::new(TelemetryInner {
                metrics: GatewayMetricsSummary::default(),
                cache_store,
                active_tasks: BTreeMap::new(),
                recent_tasks: VecDeque::with_capacity(MAX_RECENT_TASKS),
            }),
            persist_lock: Mutex::new(0),
            notify_tx,
            shutdown_tx,
        }
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

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.notify_tx.subscribe()
    }

    pub fn notify_change(&self) {
        let _ = self.notify_tx.send(());
    }

    /// Subscribes to gateway shutdown so long-lived telemetry SSE streams close immediately
    /// instead of blocking `axum`'s graceful connection drain.
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

        let snapshot = TaskSnapshot {
            task_id: task_id.clone(),
            protocol,
            stream,
            requested_model: requested_model.clone(),
            resolved_model: requested_model,
            session_id: None,
            session_reused: false,
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
            prompt_preview,
            output_preview: String::new(),
            stop_reason: None,
            error_message: None,
            usage: None,
            cache_hit_rate_bps: None,
        };

        {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.metrics.total_requests += 1;
            match protocol {
                TaskProtocol::OpenaiResponses => guard.metrics.openai_requests += 1,
                TaskProtocol::AnthropicMessages => guard.metrics.anthropic_requests += 1,
            }
            guard.active_tasks.insert(task_id.clone(), snapshot);
            guard.metrics.active_requests = guard.active_tasks.len();
        }
        self.notify_change();

        ActiveTaskGuard {
            hub: Arc::clone(self),
            task_id,
            armed: true,
        }
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

        let recent: Vec<TaskSnapshot> = guard.recent_tasks.iter().rev().cloned().collect();
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
    /// `session/prompt` completes, guaranteeing 0-token cumulative drift.
    pub fn reconcile_cascade_usage_drift(
        &self,
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
        let prepared = {
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

            guard.cache_store.prepare_persist_payload(now_ms)
        };

        self.flush_prepared_persist(prepared);
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

/// RAII tracker for an in-flight gateway turn.
pub struct ActiveTaskGuard {
    hub: Arc<TelemetryHub>,
    task_id: String,
    armed: bool,
}

impl ActiveTaskGuard {
    #[must_use]
    pub const fn hub(&self) -> &Arc<TelemetryHub> {
        &self.hub
    }

    pub fn bind_session(&self, session_id: &str, reused: bool, resolved_model: &str) {
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

    pub fn record_thought(&self, delta_chars: usize) {
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
        ) =
            (turn_hit_bps, diagnostics.as_ref(), task_snapshot_preview)
            && capture_settings.0 > 0
            && hit_bps < capture_settings.1
        {
            let (metrics, cause) = build_low_cache_request_metrics(
                diag,
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

        let (prepared, evicted_records, payload_dir) = {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let mut persist_needed = false;
            let mut evicted_records = Vec::new();
            let payload_dir = guard.cache_store.payload_dir.clone();

            if let Some(mut task) = guard.active_tasks.remove(&self.task_id) {
                let elapsed_ms = now_ms.saturating_sub(task.started_at_unix_ms);
                task.phase = TaskPhase::Completed;
                task.finished_at_unix_ms = Some(now_ms);
                task.elapsed_ms = elapsed_ms;
                task.stop_reason = Some(stop_reason);
                task.usage = Some(usage);
                task.cache_hit_rate_bps = turn_hit_bps;

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
            (prep, evicted_records, payload_dir)
        };

        remove_payload_files(payload_dir.as_deref(), &evicted_records);
        self.hub.flush_prepared_persist(prepared);
        self.hub.notify_change();
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

#[expect(
    clippy::too_many_arguments,
    reason = "aggregates turn routing, token, and task-level counters into a single flat metrics struct"
)]
fn build_low_cache_request_metrics(
    diag: &TurnCompletionDiagnostics<'_>,
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
    let mut conversational_message_count: usize = 0;
    let mut user_message_count: usize = 0;
    let mut assistant_message_count: usize = 0;
    let mut total_conversation_bytes: usize = 0;
    let mut delta_messages_bytes: usize = 0;
    let mut developer_instructions_blocks: usize = 0;
    let mut developer_instructions_bytes: usize = 0;
    let mut user_text_bytes: usize = 0;
    let mut assistant_text_bytes: usize = 0;
    let mut reasoning_part_count: usize = 0;
    let mut reasoning_bytes: usize = 0;
    let mut tool_call_part_count: usize = 0;
    let mut tool_call_bytes: usize = 0;
    let mut tool_result_part_count: usize = 0;
    let mut tool_result_bytes: usize = 0;
    let mut image_part_count: usize = 0;
    let mut image_bytes: usize = 0;

    let clamped_delta_start = diag.delta_start_index.min(diag.messages.len());
    let delta_message_count = diag.messages.len().saturating_sub(clamped_delta_start);

    for (idx, msg) in diag.messages.iter().enumerate() {
        let msg_bytes = message_content_bytes(msg);
        total_conversation_bytes = total_conversation_bytes.saturating_add(msg_bytes);
        if idx >= clamped_delta_start {
            delta_messages_bytes = delta_messages_bytes.saturating_add(msg_bytes);
        }

        match msg.role {
            CanonicalRole::User => user_message_count = user_message_count.saturating_add(1),
            CanonicalRole::Assistant => {
                assistant_message_count = assistant_message_count.saturating_add(1);
            }
        }

        let mut has_conversational_part = false;
        for part in &msg.parts {
            match part {
                CanonicalPart::Text(t) => {
                    if is_developer_instructions_text(t) {
                        developer_instructions_blocks =
                            developer_instructions_blocks.saturating_add(1);
                        developer_instructions_bytes =
                            developer_instructions_bytes.saturating_add(t.len());
                    } else {
                        has_conversational_part = true;
                        match msg.role {
                            CanonicalRole::User => {
                                user_text_bytes = user_text_bytes.saturating_add(t.len());
                            }
                            CanonicalRole::Assistant => {
                                assistant_text_bytes = assistant_text_bytes.saturating_add(t.len());
                            }
                        }
                    }
                }
                CanonicalPart::Reasoning(r) => {
                    has_conversational_part = true;
                    reasoning_part_count = reasoning_part_count.saturating_add(1);
                    reasoning_bytes = reasoning_bytes.saturating_add(r.len());
                }
                CanonicalPart::ImageBase64 { media_type, data } => {
                    has_conversational_part = true;
                    image_part_count = image_part_count.saturating_add(1);
                    image_bytes = image_bytes
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
                    tool_call_part_count = tool_call_part_count.saturating_add(1);
                    tool_call_bytes = tool_call_bytes
                        .saturating_add(call_id.len())
                        .saturating_add(name.len())
                        .saturating_add(arguments.len());
                }
                CanonicalPart::ToolResult {
                    call_id, output, ..
                } => {
                    has_conversational_part = true;
                    tool_result_part_count = tool_result_part_count.saturating_add(1);
                    tool_result_bytes = tool_result_bytes
                        .saturating_add(call_id.len())
                        .saturating_add(output.len());
                }
            }
        }
        if has_conversational_part {
            conversational_message_count = conversational_message_count.saturating_add(1);
        }
    }

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

fn bound_part_string(raw: &str, budget_remaining: &mut usize, truncated: &mut bool) -> String {
    let cap = (*budget_remaining).min(MAX_FULL_PART_BYTES);
    if cap == 0 {
        if !raw.is_empty() {
            *truncated = true;
        }
        return "[...omitted: full diagnostic payload exceeded 2 MiB safety cap...]".to_owned();
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

fn build_full_request_payload(
    task_id: &str,
    recorded_at_unix_ms: u64,
    diag: &TurnCompletionDiagnostics<'_>,
    clamped_delta_start: usize,
    output_preview: &str,
) -> LowCacheFullRequestPayload {
    let mut budget = MAX_FULL_PAYLOAD_BUDGET_BYTES;
    let mut truncated = false;

    let acp_system_append = diag
        .acp_system_append
        .map(|s| bound_part_string(s, &mut budget, &mut truncated));
    let tools_delta_xml = diag
        .tools_delta_xml
        .map(|s| bound_part_string(s, &mut budget, &mut truncated));

    let mut outbound_acp_prompt_blocks = Vec::with_capacity(diag.outbound_acp_blocks.len());
    for block in diag.outbound_acp_blocks {
        match block {
            AcpPromptBlock::Text { text } => {
                let bytes = text.len();
                let saved = bound_part_string(text, &mut budget, &mut truncated);
                outbound_acp_prompt_blocks
                    .push(CapturedAcpPromptBlock::Text { bytes, text: saved });
            }
            AcpPromptBlock::Image { data, mime_type } => {
                outbound_acp_prompt_blocks.push(CapturedAcpPromptBlock::Image {
                    mime_type: mime_type.clone(),
                    base64_bytes: data.len(),
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
                    let text = bound_part_string(t, &mut budget, &mut truncated);
                    parts.push(CapturedFullPart::Text {
                        is_developer_instructions: is_dev,
                        bytes,
                        text,
                    });
                }
                CanonicalPart::Reasoning(r) => {
                    let bytes = r.len();
                    let text = bound_part_string(r, &mut budget, &mut truncated);
                    parts.push(CapturedFullPart::Reasoning { bytes, text });
                }
                CanonicalPart::ImageBase64 { media_type, data } => {
                    parts.push(CapturedFullPart::Image {
                        media_type: media_type.clone(),
                        base64_bytes: data.len(),
                    });
                }
                CanonicalPart::ToolCall {
                    call_id,
                    name,
                    arguments,
                    is_custom,
                } => {
                    let bytes = arguments.len();
                    let saved_args = bound_part_string(arguments, &mut budget, &mut truncated);
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
                    let saved_out = bound_part_string(output, &mut budget, &mut truncated);
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
            .map(|d| bound_part_string(d, &mut budget, &mut truncated));
        let schema_or_format =
            raw_schema.map(|s| bound_part_string(s, &mut budget, &mut truncated));
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
        hub.reconcile_cascade_usage_drift(accounted, authoritative, false);

        let summary = hub.low_cache_hits().summary;
        assert_eq!(summary.total_prompt_tokens, 2_250);
        assert_eq!(summary.total_cached_read_tokens, 1_450);
        assert_eq!(summary.warm_prompt_tokens, 1_250);
        assert_eq!(summary.warm_cached_read_tokens, 1_050);
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
