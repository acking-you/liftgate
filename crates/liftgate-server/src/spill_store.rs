//! Bounded, atomic, content-addressed local file store for truncated prompt, tool, and web payloads.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use liftgate_types::session_key::{TruncationSpillSink, format_content_addressed_spill_filename};
use thiserror::Error;
use uuid::Uuid;

use crate::platform::canonicalize_clean;

/// Default cap on retained spill files inside `~/.liftgate/spills`.
pub const DEFAULT_MAX_SPILL_FILES: usize = 256;

/// Default cumulative disk cap (`256 MiB`) across retained spill files.
pub const DEFAULT_MAX_SPILL_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

const _: () = {
    assert!(
        DEFAULT_MAX_SPILL_FILES >= 16,
        "default spill file retention cap must accommodate multi-tool turns"
    );
    assert!(
        DEFAULT_MAX_SPILL_TOTAL_BYTES >= 16 * 1024 * 1024,
        "default spill byte cap must accommodate large command logs and transcripts"
    );
};

/// Typed filesystem error returned by [`SpillStore::try_spill`].
#[derive(Debug, Error)]
pub enum SpillStoreError {
    #[error("failed to create spill directory `{}`", path.display())]
    CreateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to create temporary spill file `{}`", path.display())]
    CreateTemp {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to write temporary spill file `{}`", path.display())]
    WriteTemp {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to persist spill file `{}`", path.display())]
    Persist {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

struct TempFileCleanupGuard {
    path: Option<PathBuf>,
}

impl TempFileCleanupGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempFileCleanupGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// Bounded local directory store that persists full lossless payloads before truncation.
#[derive(Clone, Debug)]
pub struct SpillStore {
    dir: PathBuf,
    max_files: usize,
    max_total_bytes: u64,
}

impl SpillStore {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            max_files: DEFAULT_MAX_SPILL_FILES,
            max_total_bytes: DEFAULT_MAX_SPILL_TOTAL_BYTES,
        }
    }

    #[must_use]
    pub fn with_limits(dir: impl Into<PathBuf>, max_files: usize, max_total_bytes: u64) -> Self {
        Self {
            dir: dir.into(),
            max_files: max_files.max(1),
            max_total_bytes: max_total_bytes.max(1024),
        }
    }

    /// Atomically writes `full_content` into a content-addressed file under `self.dir`,
    /// deduplicating existing identical files and pruning oldest entries past retention bounds.
    pub fn try_spill(
        &self,
        kind_label: &str,
        full_content: &str,
    ) -> Result<String, SpillStoreError> {
        ensure_spill_dir(&self.dir)?;
        let clean_dir = canonicalize_clean(&self.dir);
        let file_name = format_content_addressed_spill_filename(kind_label, full_content);
        let final_path = clean_dir.join(&file_name);
        let expected_len = full_content.len() as u64;

        if let Ok(meta) = fs::metadata(&final_path)
            && meta.is_file()
            && meta.len() == expected_len
        {
            return Ok(format_portable_path(&final_path));
        }

        let temp_path = clean_dir.join(format!(".tmp-{file_name}-{}", Uuid::new_v4().simple()));
        let mut temp_guard = TempFileCleanupGuard::new(temp_path.clone());

        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }

        let mut file = opts
            .open(&temp_path)
            .map_err(|source| SpillStoreError::CreateTemp {
                path: temp_path.clone(),
                source,
            })?;
        file.write_all(full_content.as_bytes())
            .map_err(|source| SpillStoreError::WriteTemp {
                path: temp_path.clone(),
                source,
            })?;
        file.sync_all()
            .map_err(|source| SpillStoreError::WriteTemp {
                path: temp_path.clone(),
                source,
            })?;
        drop(file);

        if let Err(rename_err) = fs::rename(&temp_path, &final_path) {
            // On Windows, a concurrent writer persisting the identical content-addressed file
            // can win the rename race; verify length before treating as an error.
            let raced_identical = fs::metadata(&final_path)
                .is_ok_and(|meta| meta.is_file() && meta.len() == expected_len);
            if !raced_identical {
                return Err(SpillStoreError::Persist {
                    path: final_path,
                    source: rename_err,
                });
            }
        } else {
            temp_guard.disarm();
        }
        drop(temp_guard);

        prune_excess_spill_files(
            &clean_dir,
            &file_name,
            expected_len,
            self.max_files,
            self.max_total_bytes,
        );

        Ok(format_portable_path(&final_path))
    }
}

impl TruncationSpillSink for SpillStore {
    fn spill_truncated_content(&mut self, kind_label: &str, full_content: &str) -> Option<String> {
        match self.try_spill(kind_label, full_content) {
            Ok(path) => Some(path),
            Err(err) => {
                tracing::warn!(
                    error = ?err,
                    kind = kind_label,
                    bytes = full_content.len(),
                    "failed to persist full truncated payload to local spill directory"
                );
                None
            }
        }
    }
}

fn ensure_spill_dir(dir: &Path) -> Result<(), SpillStoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(dir)
            .map_err(|source| SpillStoreError::CreateDir {
                path: dir.to_path_buf(),
                source,
            })
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir).map_err(|source| SpillStoreError::CreateDir {
            path: dir.to_path_buf(),
            source,
        })
    }
}

fn format_portable_path(path: &Path) -> String {
    canonicalize_clean(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn prune_excess_spill_files(
    clean_dir: &Path,
    protected_file_name: &str,
    protected_len: u64,
    max_files: usize,
    max_total_bytes: u64,
) {
    let Ok(read_dir) = fs::read_dir(clean_dir) else {
        return;
    };
    let mut candidates: Vec<(u128, u64, PathBuf)> = Vec::new();
    let mut total_bytes = protected_len;
    let mut total_files = 1usize;

    for entry in read_dir.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let os_name = entry.file_name();
        let Some(name) = os_name.to_str() else {
            continue;
        };
        if name.starts_with(".tmp-") || name == protected_file_name {
            continue;
        }
        if !(name.ends_with(".txt") || name.ends_with(".json")) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let len = meta.len();
        let modified_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        total_bytes = total_bytes.saturating_add(len);
        total_files = total_files.saturating_add(1);
        candidates.push((modified_ns, len, entry.path()));
    }

    if total_files <= max_files && total_bytes <= max_total_bytes {
        return;
    }

    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.cmp(&b.2)));
    for (_, len, path) in candidates {
        if total_files <= max_files && total_bytes <= max_total_bytes {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total_files = total_files.saturating_sub(1);
            total_bytes = total_bytes.saturating_sub(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deduplicates_content_addressed_spills_and_prunes_oldest_entries() {
        let dir = std::env::temp_dir().join(format!("liftgate-spill-test-{}", Uuid::new_v4()));
        let store = SpillStore::with_limits(&dir, 3, 20_000);

        let p1 = store.try_spill("tool output", "alpha payload").unwrap();
        let p1_dup = store.try_spill("tool output", "alpha payload").unwrap();
        assert_eq!(p1, p1_dup);
        assert!(p1.ends_with(".txt"));
        assert!(!p1.contains('\\'));
        assert_eq!(fs::read_to_string(&p1).unwrap(), "alpha payload");

        let json_path = store
            .try_spill("client tools catalog", r#"[{"name":"Bash"}]"#)
            .unwrap();
        assert!(json_path.ends_with(".json"));

        let _ = store.try_spill("input text", &"b".repeat(4_000)).unwrap();
        let newest = store.try_spill("input text", &"c".repeat(4_000)).unwrap();

        let remaining: Vec<_> = fs::read_dir(&dir).unwrap().flatten().collect();
        assert!(
            remaining.len() <= 3,
            "expected bounded retention <= 3 files, got {}",
            remaining.len()
        );
        assert_eq!(fs::read_to_string(&newest).unwrap(), "c".repeat(4_000));

        let _ = fs::remove_dir_all(&dir);
    }
}
