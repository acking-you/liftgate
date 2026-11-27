//! Opt-in full-context capture (`/api/telemetry/context-captures`).
//!
//! When enabled, every completed turn writes its complete context (message history split into
//! resident prefix and delta, reasoning, the `session/prompt` blocks sent to `agy acp`, and the
//! model output) to `~/.liftgate/context-captures/ctx-<task_id>.json`. An index file keeps the
//! list cheap to serve, and both a record-count and a disk quota evict the oldest captures.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io;
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

/// Startup configuration for [`ContextCaptureStore`].
#[derive(Clone, Debug, Default)]
pub struct ContextCaptureConfig {
    /// Capture every completed turn (`false` by default).
    pub enabled: bool,
    pub max_records: ContextCaptureRecordLimit,
    pub max_disk: ContextCaptureDiskMib,
    /// Directory holding payload files and the index (`None` disables capture entirely).
    pub dir: Option<PathBuf>,
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

    /// Reserves the position of a completed turn's capture before its payload is written.
    pub const fn reserve_seq(&mut self) -> u64 {
        self.last_capture_seq = self.last_capture_seq.saturating_add(1);
        self.last_capture_seq
    }

    /// Adds a freshly written capture at its reserved position (writers finish out of order),
    /// replacing an older capture of the same task, and returns records evicted to stay within
    /// quota.
    pub fn commit(&mut self, seq: u64, record: ContextCaptureRecord) -> Vec<ContextCaptureRecord> {
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
        self.trim_to_quota()
    }

    /// Drops every record and returns them so their files can be removed.
    pub fn clear(&mut self) -> Vec<ContextCaptureRecord> {
        self.stored_bytes = 0;
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
        }
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
            dir: None,
        });
        for i in 0..4 {
            let seq = store.reserve_seq();
            let evicted = store.commit(seq, record(&format!("t{i}"), 1_000));
            if i < 3 {
                assert!(evicted.is_empty());
            } else {
                assert_eq!(evicted.first().map(|r| r.task_id.as_str()), Some("t0"));
            }
        }
        let big = 9 * 1024 * 1024;
        let seq = store.reserve_seq();
        let evicted = store.commit(seq, record("t-big-a", big));
        assert!(!evicted.is_empty());
        let seq = store.reserve_seq();
        let evicted = store.commit(seq, record("t-big-b", big));
        assert!(evicted.iter().any(|r| r.task_id == "t-big-a"));
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
            let seq = store.reserve_seq();
            let _ = store.commit(seq, record(task, 2));
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
        let first = store.reserve_seq();
        let second = store.reserve_seq();
        let third = store.reserve_seq();
        let _ = store.commit(second, record("t2", 1));
        let _ = store.commit(third, record("t3", 1));
        let _ = store.commit(first, record("t1", 1));
        let ids: Vec<String> = store
            .list()
            .records
            .into_iter()
            .map(|r| r.task_id)
            .collect();
        assert_eq!(ids, ["t3", "t2", "t1"]);

        let mut full = ContextCaptureStore::load(ContextCaptureConfig {
            enabled: true,
            max_records: ContextCaptureRecordLimit::try_new(1).unwrap_or_default(),
            dir: None,
            ..ContextCaptureConfig::default()
        });
        let older = full.reserve_seq();
        let newer = full.reserve_seq();
        let _ = full.commit(newer, record("new", 1));
        let evicted = full.commit(older, record("old", 1));
        assert_eq!(evicted.first().map(|r| r.task_id.as_str()), Some("old"));
        assert_eq!(
            full.list().records.first().map(|r| r.task_id.as_str()),
            Some("new")
        );
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
