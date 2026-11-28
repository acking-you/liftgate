//! Opt-in full-context capture (`/api/telemetry/context-captures`).
//!
//! When enabled, every completed turn writes its complete context (message history split into
//! resident prefix and delta, reasoning, the `session/prompt` blocks sent to `agy acp`, and the
//! model output) to `~/.liftgate/context-captures/ctx-<task_id>.json`. An index file keeps the
//! list cheap to serve, and both a record-count and a disk quota evict the oldest captures.
//!
//! Payloads are written off the async workers. Each completed turn first takes a
//! [`CaptureTicket`]: its sequence keeps the index in turn order however the writers finish, and
//! its generation lets clearing (or switching capture off) reject writers that are still running.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use liftgate_types::{
    config::{ContextCaptureDiskMib, ContextCaptureRecordLimit},
    telemetry::{
        ContextCaptureDetailResponse, ContextCaptureRecord, ContextCaptureSummary,
        ContextCapturesResponse, FullContextPayload,
    },
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const INDEX_FILE_NAME: &str = "index.json";
const PAYLOAD_FILE_PREFIX: &str = "ctx-";
const INDEX_SCHEMA_VERSION: u32 = 1;

/// Default for [`ContextCaptureConfig::max_pending_writes`].
pub const DEFAULT_MAX_PENDING_CAPTURE_WRITES: NonZeroUsize = NonZeroUsize::MIN.saturating_add(1);

/// Startup configuration for [`ContextCaptureStore`].
#[derive(Clone, Debug)]
pub struct ContextCaptureConfig {
    /// Capture every completed turn (`false` by default).
    pub enabled: bool,
    pub max_records: ContextCaptureRecordLimit,
    pub max_disk: ContextCaptureDiskMib,
    /// Directory holding payload files and the index (`None` disables capture entirely).
    pub dir: Option<PathBuf>,
    /// Captures that may wait for, or be in the middle of, their disk write at once. A turn that
    /// completes while all of them are taken is skipped and counted instead of holding its
    /// payload in memory.
    pub max_pending_writes: NonZeroUsize,
}

impl Default for ContextCaptureConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_records: ContextCaptureRecordLimit::default(),
            max_disk: ContextCaptureDiskMib::default(),
            dir: None,
            max_pending_writes: DEFAULT_MAX_PENDING_CAPTURE_WRITES,
        }
    }
}

/// Reservation taken when a turn completes, before its payload is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureTicket {
    /// Turn-completion order.
    seq: u64,
    /// [`ContextCaptureStore`] generation the turn completed in.
    generation: u64,
}

/// Outcome of [`ContextCaptureStore::commit`].
#[derive(Debug, PartialEq, Eq)]
pub enum CaptureCommit {
    /// Indexed; carries the older captures evicted to stay within quota.
    Stored(Vec<ContextCaptureRecord>),
    /// Captures were cleared, or capture was switched off, after the turn completed.
    Stale,
}

/// Failure while writing or reading a capture file.
#[derive(Debug, Error)]
pub enum ContextCaptureIoError {
    #[error("failed to serialize full-context capture `{task_id}`")]
    Serialize {
        task_id: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("full-context capture task id `{task_id}` cannot be used as a file name")]
    InvalidTaskId { task_id: String },
    #[error("failed to read full-context capture `{}`", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to decode full-context capture `{}`", path.display())]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct PersistedIndex {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    records: Vec<ContextCaptureRecord>,
}

/// File name for a captured payload; only `[A-Za-z0-9_-]` survive from the task id.
#[must_use]
pub fn payload_file_name(task_id: &str) -> Option<String> {
    let safe: String = task_id
        .chars()
        .take(96)
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    (!safe.is_empty()).then(|| format!("{PAYLOAD_FILE_PREFIX}{safe}.json"))
}

/// A committed capture and the order its turn completed in.
#[derive(Clone, Debug)]
struct StoredCapture {
    seq: u64,
    record: ContextCaptureRecord,
}

/// In-memory index of captured turns plus their quota bookkeeping.
pub struct ContextCaptureStore {
    enabled: bool,
    max_records: ContextCaptureRecordLimit,
    max_disk: ContextCaptureDiskMib,
    dir: Option<PathBuf>,
    /// Oldest first, ordered by [`StoredCapture::seq`].
    records: VecDeque<StoredCapture>,
    stored_bytes: u64,
    revision: u64,
    persist_seq: u64,
    last_capture_seq: u64,
    /// Bumped by [`Self::clear`] and by switching capture off; older tickets become stale.
    generation: u64,
    skipped_records: u64,
}

impl ContextCaptureStore {
    /// Loads the persisted index, dropping entries whose payload file vanished, trimming to the
    /// configured quotas, and removing orphan payload files.
    #[must_use]
    pub fn load(config: ContextCaptureConfig) -> Self {
        let mut records: VecDeque<StoredCapture> = VecDeque::new();
        if let Some(dir) = config.dir.as_deref() {
            let index_path = dir.join(INDEX_FILE_NAME);
            match fs::read(&index_path) {
                Ok(bytes) => match serde_json::from_slice::<PersistedIndex>(&bytes) {
                    Ok(index) => {
                        records = index
                            .records
                            .into_iter()
                            .filter(|r| {
                                payload_file_name(&r.task_id)
                                    .is_some_and(|name| dir.join(name).is_file())
                            })
                            .zip(1_u64..)
                            .map(|(record, seq)| StoredCapture { seq, record })
                            .collect();
                    }
                    Err(err) => tracing::warn!(
                        path = %index_path.display(),
                        error = ?err,
                        "corrupt full-context capture index; starting empty"
                    ),
                },
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => tracing::warn!(
                    path = %index_path.display(),
                    error = ?err,
                    "failed to read full-context capture index; starting empty"
                ),
            }
        }
        let stored_bytes = records.iter().map(|c| c.record.payload_bytes).sum();
        let last_capture_seq = records.back().map_or(0, |c| c.seq);
        let mut store = Self {
            enabled: config.enabled,
            max_records: config.max_records,
            max_disk: config.max_disk,
            dir: config.dir,
            records,
            stored_bytes,
            revision: 1,
            persist_seq: 0,
            last_capture_seq,
            generation: 0,
            skipped_records: 0,
        };
        let evicted = store.trim_to_quota();
        store.remove_payload_files(&evicted);
        store.prune_orphans();
        store
    }

    /// `true` when new turns should be captured.
    #[must_use]
    pub const fn is_capturing(&self) -> bool {
        self.enabled && self.dir.is_some()
    }

    #[must_use]
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    #[must_use]
    pub fn summary(&self) -> ContextCaptureSummary {
        ContextCaptureSummary {
            revision: self.revision,
            enabled: self.enabled,
            max_records: self.max_records.get(),
            max_total_bytes: self.max_disk.bytes(),
            stored_records: self.records.len(),
            stored_bytes: self.stored_bytes,
            skipped_records: self.skipped_records,
            capture_dir: self.dir.as_ref().map(|d| d.to_string_lossy().into_owned()),
        }
    }

    /// Captured turns, newest first.
    #[must_use]
    pub fn list(&self) -> ContextCapturesResponse {
        ContextCapturesResponse {
            summary: self.summary(),
            records: self
                .records
                .iter()
                .rev()
                .map(|c| c.record.clone())
                .collect(),
        }
    }

    /// Index row and payload path of a captured turn.
    #[must_use]
    pub fn locate(&self, task_id: &str) -> Option<(ContextCaptureRecord, PathBuf)> {
        let stored = self
            .records
            .iter()
            .rev()
            .find(|c| c.record.task_id == task_id)?;
        let path = self.dir.as_deref()?.join(payload_file_name(task_id)?);
        Some((stored.record.clone(), path))
    }

    /// Applies new settings and returns records evicted by a smaller quota.
    pub fn update_config(
        &mut self,
        enabled: Option<bool>,
        max_records: Option<ContextCaptureRecordLimit>,
        max_disk: Option<ContextCaptureDiskMib>,
    ) -> Vec<ContextCaptureRecord> {
        if let Some(flag) = enabled {
            if self.enabled && !flag {
                // Writers still finishing turns from before the switch must not land after it.
                self.generation = self.generation.saturating_add(1);
            }
            self.enabled = flag;
        }
        if let Some(limit) = max_records {
            self.max_records = limit;
        }
        if let Some(quota) = max_disk {
            self.max_disk = quota;
        }
        self.revision = self.revision.saturating_add(1);
        self.trim_to_quota()
    }

    #[must_use]
    pub const fn settings(&self) -> (bool, ContextCaptureRecordLimit, ContextCaptureDiskMib) {
        (self.enabled, self.max_records, self.max_disk)
    }

    /// Reserves the index position of a completed turn's capture before its payload is written.
    pub const fn reserve(&mut self) -> CaptureTicket {
        self.last_capture_seq = self.last_capture_seq.saturating_add(1);
        CaptureTicket {
            seq: self.last_capture_seq,
            generation: self.generation,
        }
    }

    /// `false` once captures were cleared, or capture was switched off, after `ticket` was taken.
    #[must_use]
    pub const fn accepts(&self, ticket: CaptureTicket) -> bool {
        ticket.generation == self.generation
    }

    /// Counts a completed turn that was not captured because earlier captures were still being
    /// written.
    pub const fn note_skipped(&mut self) {
        self.skipped_records = self.skipped_records.saturating_add(1);
        self.revision = self.revision.saturating_add(1);
    }

    /// Adds a freshly written capture at its reserved position (writers finish out of order),
    /// replacing an older capture of the same task. Rejects tickets taken before the latest
    /// clear or switch-off.
    pub fn commit(&mut self, ticket: CaptureTicket, record: ContextCaptureRecord) -> CaptureCommit {
        if !self.accepts(ticket) {
            return CaptureCommit::Stale;
        }
        let seq = ticket.seq;
        if let Some(pos) = self
            .records
            .iter()
            .position(|c| c.record.task_id == record.task_id)
            && let Some(old) = self.records.remove(pos)
        {
            self.stored_bytes = self.stored_bytes.saturating_sub(old.record.payload_bytes);
        }
        self.stored_bytes = self.stored_bytes.saturating_add(record.payload_bytes);
        let at = self
            .records
            .iter()
            .rposition(|c| c.seq < seq)
            .map_or(0, |pos| pos.saturating_add(1));
        self.records.insert(at, StoredCapture { seq, record });
        self.revision = self.revision.saturating_add(1);
        CaptureCommit::Stored(self.trim_to_quota())
    }

    /// Drops every record and returns them so their files can be removed; captures still being
    /// written are rejected when they try to commit.
    pub fn clear(&mut self) -> Vec<ContextCaptureRecord> {
        self.stored_bytes = 0;
        self.skipped_records = 0;
        self.generation = self.generation.saturating_add(1);
        self.revision = self.revision.saturating_add(1);
        self.records.drain(..).map(|c| c.record).collect()
    }

    fn trim_to_quota(&mut self) -> Vec<ContextCaptureRecord> {
        let mut evicted = Vec::new();
        while self.records.len() > self.max_records.get()
            || (self.stored_bytes > self.max_disk.bytes() && !self.records.is_empty())
        {
            let Some(old) = self.records.pop_front() else {
                break;
            };
            self.stored_bytes = self.stored_bytes.saturating_sub(old.record.payload_bytes);
            evicted.push(old.record);
        }
        if !evicted.is_empty() {
            self.revision = self.revision.saturating_add(1);
        }
        evicted
    }

    /// Serializes the index for an atomic write; `seq` orders concurrent writers.
    #[must_use]
    pub fn prepare_index(&mut self) -> Option<(PathBuf, u64, Vec<u8>)> {
        let path = self.dir.as_deref()?.join(INDEX_FILE_NAME);
        self.persist_seq = self.persist_seq.saturating_add(1);
        let index = PersistedIndex {
            version: INDEX_SCHEMA_VERSION,
            records: self.records.iter().map(|c| c.record.clone()).collect(),
        };
        match serde_json::to_vec_pretty(&index) {
            Ok(mut bytes) => {
                bytes.push(b'\n');
                Some((path, self.persist_seq, bytes))
            }
            Err(err) => {
                tracing::warn!(error = ?err, "failed to serialize full-context capture index");
                None
            }
        }
    }

    /// Best-effort removal of payload files belonging to `records`.
    pub fn remove_payload_files(&self, records: &[ContextCaptureRecord]) {
        let Some(dir) = self.dir.as_deref() else {
            return;
        };
        for record in records {
            if let Some(name) = payload_file_name(&record.task_id) {
                let _ = fs::remove_file(dir.join(name));
            }
        }
    }

    fn prune_orphans(&self) {
        let Some(dir) = self.dir.as_deref() else {
            return;
        };
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let live: HashSet<String> = self
            .records
            .iter()
            .filter_map(|c| payload_file_name(&c.record.task_id))
            .collect();
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.starts_with(PAYLOAD_FILE_PREFIX)
                && name.ends_with(".json")
                && !live.contains(&name)
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// Serializes a capture payload.
pub fn encode_payload(
    task_id: &str,
    payload: &FullContextPayload,
) -> Result<Vec<u8>, ContextCaptureIoError> {
    serde_json::to_vec(payload).map_err(|source| ContextCaptureIoError::Serialize {
        task_id: task_id.to_owned(),
        source,
    })
}

/// Reads a captured payload from disk.
pub fn read_payload(
    record: ContextCaptureRecord,
    path: &Path,
) -> Result<ContextCaptureDetailResponse, ContextCaptureIoError> {
    let bytes = fs::read(path).map_err(|source| ContextCaptureIoError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let payload =
        serde_json::from_slice(&bytes).map_err(|source| ContextCaptureIoError::Decode {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(ContextCaptureDetailResponse { record, payload })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(task_id: &str, bytes: u64) -> ContextCaptureRecord {
        ContextCaptureRecord {
            task_id: task_id.to_owned(),
            recorded_at_unix_ms: 1,
            session_id: Some("s".to_owned()),
            harness_profile: Some("codex".to_owned()),
            resolved_model: "gemini-4-carbon-high".to_owned(),
            session_turn_index: 1,
            cache_hit_rate_bps: Some(9_000),
            total_prompt_tokens: 100,
            output_tokens: 10,
            message_count: 3,
            delta_message_count: 1,
            payload_bytes: bytes,
            prompt_preview: "hi".to_owned(),
            rewrite_count: 0,
        }
    }

    /// Commits `record` under a fresh ticket and returns the evicted records.
    fn store_now(store: &mut ContextCaptureStore, record: ContextCaptureRecord) -> Vec<String> {
        let ticket = store.reserve();
        evicted_ids(store.commit(ticket, record))
    }

    fn evicted_ids(outcome: CaptureCommit) -> Vec<String> {
        match outcome {
            CaptureCommit::Stored(evicted) => evicted.into_iter().map(|r| r.task_id).collect(),
            CaptureCommit::Stale => panic!("ticket unexpectedly stale"),
        }
    }

    fn listed_ids(store: &ContextCaptureStore) -> Vec<String> {
        store
            .list()
            .records
            .into_iter()
            .map(|r| r.task_id)
            .collect()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("liftgate-ctx-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap_or_else(|err| panic!("create temp dir: {err}"));
        dir
    }

    #[test]
    fn quota_evicts_oldest_by_count_and_bytes() {
        let mut store = ContextCaptureStore::load(ContextCaptureConfig {
            enabled: true,
            max_records: ContextCaptureRecordLimit::try_new(3).unwrap_or_default(),
            max_disk: ContextCaptureDiskMib::try_new(16).unwrap_or_default(),
            ..ContextCaptureConfig::default()
        });
        for i in 0..4 {
            let evicted = store_now(&mut store, record(&format!("t{i}"), 1_000));
            if i < 3 {
                assert!(evicted.is_empty());
            } else {
                assert_eq!(evicted, ["t0"]);
            }
        }
        let big = 9 * 1024 * 1024;
        let evicted = store_now(&mut store, record("t-big-a", big));
        assert!(!evicted.is_empty());
        let evicted = store_now(&mut store, record("t-big-b", big));
        assert!(evicted.iter().any(|id| id == "t-big-a"));
        assert!(store.summary().stored_bytes <= 16 * 1024 * 1024);
        assert_eq!(
            store.list().records.first().map(|r| r.task_id.as_str()),
            Some("t-big-b")
        );
        assert!(
            !store.is_capturing(),
            "no directory means nothing is written"
        );
    }

    #[test]
    fn reload_keeps_indexed_payloads_and_prunes_orphans() {
        let dir = temp_dir("reload");
        let mut store = ContextCaptureStore::load(ContextCaptureConfig {
            enabled: true,
            dir: Some(dir.clone()),
            ..ContextCaptureConfig::default()
        });
        assert!(store.is_capturing());
        for task in ["resp_a", "resp_b"] {
            let name = payload_file_name(task).unwrap_or_default();
            fs::write(dir.join(name), b"{}").unwrap_or_else(|err| panic!("write: {err}"));
            let _ = store_now(&mut store, record(task, 2));
        }
        fs::write(dir.join("ctx-orphan.json"), b"{}").unwrap_or_else(|err| panic!("write: {err}"));
        let (path, _, bytes) = store.prepare_index().unwrap_or_else(|| panic!("index"));
        fs::write(path, bytes).unwrap_or_else(|err| panic!("write index: {err}"));
        let _ = fs::remove_file(dir.join(payload_file_name("resp_a").unwrap_or_default()));

        let reloaded = ContextCaptureStore::load(ContextCaptureConfig {
            enabled: false,
            dir: Some(dir.clone()),
            ..ContextCaptureConfig::default()
        });
        let ids: Vec<String> = reloaded
            .list()
            .records
            .into_iter()
            .map(|r| r.task_id)
            .collect();
        assert_eq!(ids, vec!["resp_b".to_owned()]);
        assert!(!dir.join("ctx-orphan.json").exists());
        assert!(!reloaded.is_capturing());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn late_writers_keep_turn_completion_order() {
        let mut store = ContextCaptureStore::load(ContextCaptureConfig {
            enabled: true,
            dir: None,
            ..ContextCaptureConfig::default()
        });
        let first = store.reserve();
        let second = store.reserve();
        let third = store.reserve();
        let _ = evicted_ids(store.commit(second, record("t2", 1)));
        let _ = evicted_ids(store.commit(third, record("t3", 1)));
        let _ = evicted_ids(store.commit(first, record("t1", 1)));
        assert_eq!(listed_ids(&store), ["t3", "t2", "t1"]);

        let mut full = ContextCaptureStore::load(ContextCaptureConfig {
            enabled: true,
            max_records: ContextCaptureRecordLimit::try_new(1).unwrap_or_default(),
            dir: None,
            ..ContextCaptureConfig::default()
        });
        let older = full.reserve();
        let newer = full.reserve();
        let _ = evicted_ids(full.commit(newer, record("new", 1)));
        let evicted = evicted_ids(full.commit(older, record("old", 1)));
        assert_eq!(evicted, ["old"]);
        assert_eq!(listed_ids(&full), ["new"]);
    }

    #[test]
    fn writers_from_before_a_clear_or_switch_off_are_rejected() {
        let mut store = ContextCaptureStore::load(ContextCaptureConfig {
            enabled: true,
            ..ContextCaptureConfig::default()
        });
        let _ = store_now(&mut store, record("kept-until-clear", 1));
        let in_flight = store.reserve();
        store.note_skipped();
        assert_eq!(store.summary().skipped_records, 1);

        let cleared = store.clear();
        assert_eq!(cleared.len(), 1);
        assert_eq!(store.summary().skipped_records, 0);
        assert!(!store.accepts(in_flight));
        assert_eq!(
            store.commit(in_flight, record("from-before-clear", 1)),
            CaptureCommit::Stale
        );
        assert!(listed_ids(&store).is_empty());

        // Switching capture off invalidates in-flight writers too, even after switching back on.
        let before_off = store.reserve();
        let _ = store.update_config(Some(false), None, None);
        let _ = store.update_config(Some(true), None, None);
        assert_eq!(
            store.commit(before_off, record("from-before-off", 1)),
            CaptureCommit::Stale
        );
        // Re-applying the same setting or changing quotas keeps writers valid.
        let current = store.reserve();
        let _ = store.update_config(Some(true), None, None);
        assert_eq!(
            store.commit(current, record("fresh", 1)),
            CaptureCommit::Stored(Vec::new())
        );
        assert_eq!(listed_ids(&store), ["fresh"]);
    }

    #[test]
    fn payload_file_names_are_sanitized() {
        assert_eq!(
            payload_file_name("resp_1a-2").as_deref(),
            Some("ctx-resp_1a-2.json")
        );
        assert_eq!(
            payload_file_name("../../etc").as_deref(),
            Some("ctx-etc.json")
        );
        assert_eq!(payload_file_name("///"), None);
    }
}
