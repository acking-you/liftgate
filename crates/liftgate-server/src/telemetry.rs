use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use liftgate_types::{
    acp::{AcpUsage, compute_cache_hit_bps},
    canonical::{CanonicalMessage, CanonicalPart, CanonicalRole},
    config::{CacheHitThresholdBps, LowCacheRecordLimit},
    telemetry::{
        CacheTelemetrySummary, GatewayMetricsSummary, LowCacheHitReason, LowCacheHitRecord,
        LowCacheHitsResponse, TaskPhase, TaskProtocol, TaskSnapshot,
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

const fn default_persisted_schema_version() -> u32 {
    PERSISTED_CACHE_SCHEMA_VERSION
}

/// Configuration for cumulative prompt-cache accounting and persisted low-cache-hit diagnostics.
#[derive(Clone, Debug, Default)]
pub struct CacheTelemetryConfig {
    /// Optional path to `~/.liftgate/cache-telemetry.json` for durable persistence across restarts.
    pub persist_path: Option<PathBuf>,
    /// Optional path to `~/.liftgate/settings.json` so live Web UI config edits survive restarts.
    pub settings_path: Option<PathBuf>,
    /// Maximum number of low-cache-hit records retained (`1_000` by default; `0` disables recording).
    pub max_low_cache_records: LowCacheRecordLimit,
    /// Basis-point threshold (`5_000` = `50.00%` by default) below which a turn is recorded.
    pub low_cache_threshold_bps: CacheHitThresholdBps,
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
    low_cache_records: VecDeque<LowCacheHitRecord>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

struct CacheTelemetryStore {
    persist_path: Option<PathBuf>,
    settings_path: Option<PathBuf>,
    max_low_cache_records: LowCacheRecordLimit,
    low_cache_threshold_bps: CacheHitThresholdBps,
    revision: u64,
    persist_seq: u64,
    file_state: PersistedCacheTelemetryFile,
}

impl CacheTelemetryStore {
    fn from_config(config: CacheTelemetryConfig) -> Self {
        let mut file_state = config
            .persist_path
            .as_deref()
            .and_then(load_persisted_cache_file)
            .unwrap_or_default();

        file_state.version = PERSISTED_CACHE_SCHEMA_VERSION;
        file_state.max_low_cache_records = config.max_low_cache_records.get();
        file_state.low_cache_threshold_bps = config.low_cache_threshold_bps.get();
        trim_records_to_cap(
            &mut file_state.low_cache_records,
            config.max_low_cache_records.get(),
        );

        Self {
            persist_path: config.persist_path,
            settings_path: config.settings_path,
            max_low_cache_records: config.max_low_cache_records,
            low_cache_threshold_bps: config.low_cache_threshold_bps,
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
            persisted_path: self
                .persist_path
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
        Some((path, self.persist_seq, self.file_state.clone()))
    }
}

fn trim_records_to_cap(records: &mut VecDeque<LowCacheHitRecord>, max_records: usize) {
    while records.len() > max_records {
        let _ = records.pop_front();
    }
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

fn sync_settings_json_cache_fields(
    settings_path: &Path,
    max_low_cache_records: LowCacheRecordLimit,
    low_cache_threshold_bps: CacheHitThresholdBps,
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

    /// Dynamically updates the low-cache retention limit and/or threshold, trimming excess
    /// records immediately and atomically persisting to disk.
    pub fn update_cache_config(
        &self,
        max_low_cache_records: Option<LowCacheRecordLimit>,
        low_cache_threshold_bps: Option<CacheHitThresholdBps>,
    ) -> CacheTelemetrySummary {
        let now_ms = unix_timestamp_ms();
        let (summary, prepared, settings_sync) = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(limit) = max_low_cache_records {
                guard.cache_store.max_low_cache_records = limit;
                trim_records_to_cap(
                    &mut guard.cache_store.file_state.low_cache_records,
                    limit.get(),
                );
            }
            if let Some(threshold) = low_cache_threshold_bps {
                guard.cache_store.low_cache_threshold_bps = threshold;
            }
            guard.cache_store.revision = guard.cache_store.revision.saturating_add(1);
            let prepared = guard.cache_store.prepare_persist_payload(now_ms);
            let settings_sync = guard.cache_store.settings_path.clone().map(|p| {
                (
                    p,
                    guard.cache_store.max_low_cache_records,
                    guard.cache_store.low_cache_threshold_bps,
                )
            });
            (guard.cache_store.summary(), prepared, settings_sync)
        };

        self.flush_prepared_persist(prepared);
        if let Some((settings_path, limit, threshold)) = settings_sync {
            sync_settings_json_cache_fields(&settings_path, limit, threshold);
        }
        self.notify_change();
        summary
    }

    /// Clears persisted low-cache-hit records (and optionally resets cumulative cache counters).
    pub fn clear_low_cache_hits(&self, reset_totals: bool) -> CacheTelemetrySummary {
        let now_ms = unix_timestamp_ms();
        let (summary, prepared) = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.cache_store.file_state.low_cache_records.clear();
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
            }
            guard.cache_store.revision = guard.cache_store.revision.saturating_add(1);
            let prepared = guard.cache_store.prepare_persist_payload(now_ms);
            (guard.cache_store.summary(), prepared)
        };

        self.flush_prepared_persist(prepared);
        self.notify_change();
        summary
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

    pub fn complete(mut self, stop_reason: String, usage: AcpUsage) {
        self.armed = false;
        let now_ms = unix_timestamp_ms();
        let turn_prompt_tokens = usage.total_prompt_tokens();
        let turn_hit_bps = usage.cache_hit_bps();

        let prepared = {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let mut persist_needed = false;

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
                    cs.file_state.evaluated_turns = cs.file_state.evaluated_turns.saturating_add(1);

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
                                let _ = cs.file_state.low_cache_records.pop_front();
                            }
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
                                    cache_hit_rate_bps: hit_bps,
                                    input_uncached_tokens: usage.input_tokens,
                                    cached_read_tokens: usage.cached_read_tokens,
                                    cached_write_tokens: usage.cached_write_tokens,
                                    total_prompt_tokens: turn_prompt_tokens,
                                    output_tokens: usage.output_tokens,
                                    elapsed_ms,
                                    ttft_ms: task.ttft_ms,
                                    prompt_preview: task.prompt_preview.clone(),
                                });
                            cs.revision = cs.revision.saturating_add(1);
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
                compute_cache_hit_bps(guard.metrics.total_cached_tokens, cumulative_prompt);

            if persist_needed {
                guard.cache_store.prepare_persist_payload(now_ms)
            } else {
                None
            }
        };

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
