//! Persistent fleet auto-update state ledger (`auto-update-state.json`) and harness switches.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use liftgate_types::{
    config::{AutoUpdateIntervalMins, AutoUpdateTarget},
    telemetry::{AutoUpdateLastOutcome, AutoUpdateTargetTelemetry},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{AutoUpdateCliOverrides, ProbedTargetInfo, SingleTargetRunResult, target_slot_index};

/// Typed I/O and serialization failure when reading or writing `auto-update-state.json`.
#[derive(Debug, Error)]
pub enum LedgerError {
    #[error("I/O error accessing `{}`", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid JSON in `{}`", path.display())]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Per-harness idle auto-update switches stored under `settings.json` (all `false` by default).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessAutoUpdateSettings {
    #[serde(default)]
    pub codex: bool,
    #[serde(default)]
    pub claude: bool,
    #[serde(default)]
    pub opencode: bool,
    #[serde(default)]
    pub omp: bool,
}

/// Snapshot of all 6 auto-update switches plus the check interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FleetSwitchSnapshot {
    pub lg_auto_update: bool,
    pub agy_auto_update: bool,
    pub harness_auto_update: HarnessAutoUpdateSettings,
    pub interval_mins: AutoUpdateIntervalMins,
}

impl Default for FleetSwitchSnapshot {
    fn default() -> Self {
        Self {
            lg_auto_update: true,
            agy_auto_update: true,
            harness_auto_update: HarnessAutoUpdateSettings::default(),
            interval_mins: AutoUpdateIntervalMins::default(),
        }
    }
}

impl FleetSwitchSnapshot {
    #[must_use]
    pub const fn is_enabled(self, target: AutoUpdateTarget) -> bool {
        match target {
            AutoUpdateTarget::Liftgate => self.lg_auto_update,
            AutoUpdateTarget::Agy => self.agy_auto_update,
            AutoUpdateTarget::Codex => self.harness_auto_update.codex,
            AutoUpdateTarget::Claude => self.harness_auto_update.claude,
            AutoUpdateTarget::Opencode => self.harness_auto_update.opencode,
            AutoUpdateTarget::Omp => self.harness_auto_update.omp,
        }
    }
}

/// Persisted per-target auto-update check and upgrade state stored in `auto-update-state.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetAutoUpdateRecord {
    #[serde(default)]
    pub last_check_unix_ms: Option<u64>,
    #[serde(default)]
    pub latest_seen_version: Option<String>,
    #[serde(default)]
    pub last_update_unix_ms: Option<u64>,
    #[serde(default)]
    pub last_updated_from: Option<String>,
    #[serde(default)]
    pub last_updated_to: Option<String>,
    #[serde(default)]
    pub last_status: AutoUpdateLastOutcome,
    #[serde(default)]
    pub note: Option<String>,
}

/// Persisted fleet auto-update ledger stored at `~/.liftgate/auto-update-state.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AutoUpdateLedger {
    #[serde(default)]
    pub targets: BTreeMap<AutoUpdateTarget, TargetAutoUpdateRecord>,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Reads `auto-update-state.json` from `path`, returning an empty ledger when missing.
pub fn read_ledger_file(path: &Path) -> Result<AutoUpdateLedger, LedgerError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AutoUpdateLedger::default());
        }
        Err(source) => {
            return Err(LedgerError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    serde_json::from_slice(&bytes).map_err(|source| LedgerError::Json {
        path: path.to_path_buf(),
        source,
    })
}

/// Atomically persists `ledger` to `path` via a sibling temp file, `sync_all`, and `rename`.
pub fn write_ledger_file_atomic(path: &Path, ledger: &AutoUpdateLedger) -> Result<(), LedgerError> {
    let mut json = serde_json::to_vec_pretty(ledger).map_err(|source| LedgerError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    json.push(b'\n');
    write_private_bytes_atomic(path, &json)
}

/// Updates `target`'s check/apply record inside `ledger_path` and returns the updated ledger.
pub fn persist_target_result_file(
    ledger_path: &Path,
    res: &SingleTargetRunResult,
    now_ms: u64,
) -> Result<AutoUpdateLedger, LedgerError> {
    let mut ledger = read_ledger_file(ledger_path).unwrap_or_default();
    let prev_rec = ledger.targets.get(&res.target).cloned().unwrap_or_default();

    let (last_update_unix_ms, last_updated_from, last_updated_to) =
        if res.status == AutoUpdateLastOutcome::Updated {
            (
                Some(now_ms),
                res.previous_version.clone().or(prev_rec.last_updated_from),
                res.current_version.clone().or(res.latest_version.clone()),
            )
        } else {
            (
                prev_rec.last_update_unix_ms,
                prev_rec.last_updated_from,
                prev_rec.last_updated_to,
            )
        };

    let record = TargetAutoUpdateRecord {
        last_check_unix_ms: Some(now_ms),
        latest_seen_version: res.latest_version.clone().or(prev_rec.latest_seen_version),
        last_update_unix_ms,
        last_updated_from,
        last_updated_to,
        last_status: res.status,
        note: Some(res.note.clone()),
    };
    ledger.targets.insert(res.target, record);
    write_ledger_file_atomic(ledger_path, &ledger)?;
    Ok(ledger)
}

/// Combines switch settings, persisted ledger records, and probed executable info into wire rows.
#[must_use]
pub fn build_target_telemetry_rows(
    switches: FleetSwitchSnapshot,
    ledger: &AutoUpdateLedger,
    probed: &[ProbedTargetInfo; 6],
    cli_overrides: AutoUpdateCliOverrides,
) -> Vec<AutoUpdateTargetTelemetry> {
    AutoUpdateTarget::ALL
        .into_iter()
        .map(|target| {
            let info = probed
                .get(target_slot_index(target))
                .cloned()
                .unwrap_or_default();
            let rec = ledger.targets.get(&target).cloned().unwrap_or_default();
            let status = if info.binary_path.is_none()
                && rec.last_status == AutoUpdateLastOutcome::NeverChecked
            {
                AutoUpdateLastOutcome::NotInstalled
            } else {
                rec.last_status
            };
            AutoUpdateTargetTelemetry {
                target,
                enabled: cli_overrides.effective_enabled(target, switches),
                installed_version: info.installed_version,
                binary_path: info.binary_path,
                latest_seen_version: rec.latest_seen_version,
                last_check_unix_ms: rec.last_check_unix_ms,
                last_update_unix_ms: rec.last_update_unix_ms,
                last_updated_from: rec.last_updated_from,
                last_updated_to: rec.last_updated_to,
                last_status: status,
                note: rec.note,
            }
        })
        .collect()
}

struct TempFileCleanup<'a> {
    path: &'a Path,
    armed: bool,
}

impl Drop for TempFileCleanup<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(self.path);
        }
    }
}

fn write_private_bytes_atomic(target: &Path, bytes: &[u8]) -> Result<(), LedgerError> {
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| LedgerError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("auto-update-state.json");
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}", Uuid::new_v4().simple()));

    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }

    let mut guard = TempFileCleanup {
        path: &tmp_path,
        armed: true,
    };
    let mut file = opts.open(&tmp_path).map_err(|source| LedgerError::Io {
        path: tmp_path.clone(),
        source,
    })?;
    file.write_all(bytes).map_err(|source| LedgerError::Io {
        path: tmp_path.clone(),
        source,
    })?;
    file.sync_all().map_err(|source| LedgerError::Io {
        path: tmp_path.clone(),
        source,
    })?;
    drop(file);

    fs::rename(&tmp_path, target).map_err(|source| LedgerError::Io {
        path: target.to_path_buf(),
        source,
    })?;
    guard.armed = false;
    Ok(())
}
