//! Persistent installation ledger tracking displaced harness keys/providers for lossless `--uninstall`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::error::HarnessError;
use crate::fs_util::{canonicalize_clean, write_file_atomic};

/// Persistent installation ledger tracking exact changes made by `lg config <harness>` per target
/// file so `--uninstall` restores pre-existing settings without touching unrelated user edits.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ConfigSyncLedger {
    #[serde(default)]
    pub opencode: BTreeMap<String, OpenCodeManagedRecord>,
    #[serde(default)]
    pub omp: BTreeMap<String, OmpManagedRecord>,
}

/// Per-target installation record for Oh My Pi (`lg config omp`).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct OmpManagedRecord {
    #[serde(default)]
    pub global: bool,
    #[serde(default)]
    pub config_existed_before_sync: bool,
    #[serde(default)]
    pub aux_existed_before_sync: bool,
    #[serde(default)]
    pub previous_providers_agy_yaml: Option<String>,
    #[serde(default)]
    pub wrote_providers_agy_yaml: Option<String>,
    #[serde(default)]
    pub previous_providers_header_rhs: Option<String>,
    #[serde(default)]
    pub previous_model_roles: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub wrote_model_roles: BTreeMap<String, String>,
    #[serde(default)]
    pub previous_model_roles_header_rhs: Option<String>,
    #[serde(default)]
    pub model_roles_was_absent: bool,
    #[serde(default)]
    pub added_extension_entry: Option<String>,
    #[serde(default)]
    pub previous_extensions_raw_lines: Option<Vec<String>>,
    #[serde(default)]
    pub extensions_was_absent: bool,
    #[serde(default)]
    pub previous_tools_entries: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub wrote_tools_entries: BTreeMap<String, String>,
    #[serde(default)]
    pub previous_tools_header_rhs: Option<String>,
    #[serde(default)]
    pub tools_was_absent: bool,
    #[serde(default)]
    pub previous_eval_entries: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub wrote_eval_entries: BTreeMap<String, String>,
    #[serde(default)]
    pub previous_eval_header_rhs: Option<String>,
    #[serde(default)]
    pub eval_was_absent: bool,
    #[serde(default)]
    pub previous_mcp_entries: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub wrote_mcp_entries: BTreeMap<String, String>,
    #[serde(default)]
    pub previous_mcp_header_rhs: Option<String>,
    #[serde(default)]
    pub mcp_was_absent: bool,
    #[serde(default)]
    pub previous_top_entries: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub wrote_top_entries: BTreeMap<String, String>,
}

/// Per-file installation record for OpenCode (`lg config opencode`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OpenCodeManagedRecord {
    #[serde(default)]
    pub existed_before_sync: bool,
    #[serde(default)]
    pub wrote_schema: bool,
    #[serde(default)]
    pub previous_model: Option<Value>,
    #[serde(default)]
    pub wrote_model: String,
    #[serde(default)]
    pub previous_small_model: Option<Value>,
    #[serde(default)]
    pub wrote_small_model: String,
    #[serde(default)]
    pub wrote_provider_agy: Value,
    #[serde(
        default,
        deserialize_with = "deserialize_present_json_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub previous_provider_agy: Option<Value>,
    #[serde(default)]
    pub removed_v2_providers_agy: Option<Value>,
    #[serde(default)]
    pub permission_was_absent: bool,
    #[serde(default)]
    pub permission_was_shorthand: Option<String>,
    #[serde(default)]
    pub previous_permission_wildcard: Option<Value>,
    #[serde(default)]
    pub wrote_permission_wildcard: Option<Value>,
    #[serde(default)]
    pub previous_permission_execute: Option<Value>,
    #[serde(default)]
    pub wrote_permission_execute: Option<Value>,
    #[serde(default)]
    pub experimental_was_absent: bool,
    #[serde(default)]
    pub previous_experimental_full_codemode: Option<Value>,
    #[serde(default)]
    pub wrote_experimental_full_codemode: Option<Value>,
}

fn deserialize_present_json_value<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

/// Canonical string key for indexing a config file inside [`ConfigSyncLedger`].
#[must_use]
pub fn ledger_key_for_path(target_path: &Path) -> String {
    let parent_canon = target_path
        .parent()
        .map_or_else(|| target_path.to_path_buf(), canonicalize_clean);
    match target_path.file_name() {
        Some(name) => parent_canon.join(name).to_string_lossy().into_owned(),
        None => canonicalize_clean(target_path)
            .to_string_lossy()
            .into_owned(),
    }
}

/// Reads the JSON ledger at `ledger_path`, returning an empty ledger when absent.
pub fn read_config_sync_ledger(ledger_path: &Path) -> Result<ConfigSyncLedger, HarnessError> {
    match fs::read(ledger_path) {
        Ok(raw) if !raw.iter().all(u8::is_ascii_whitespace) => serde_json::from_slice(&raw)
            .map_err(|source| HarnessError::Json {
                path: ledger_path.to_path_buf(),
                source,
            }),
        Ok(_) => Ok(ConfigSyncLedger::default()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ConfigSyncLedger::default()),
        Err(source) => Err(HarnessError::Io {
            path: ledger_path.to_path_buf(),
            source,
        }),
    }
}

/// Writes or removes the JSON ledger at `ledger_path` atomically.
pub fn write_config_sync_ledger(
    ledger_path: &Path,
    ledger: &ConfigSyncLedger,
) -> Result<(), HarnessError> {
    if ledger.opencode.is_empty() && ledger.omp.is_empty() {
        match fs::remove_file(ledger_path) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(HarnessError::Io {
                    path: ledger_path.to_path_buf(),
                    source,
                });
            }
        }
    }
    let mut bytes = serde_json::to_vec_pretty(ledger).map_err(|source| HarnessError::Json {
        path: ledger_path.to_path_buf(),
        source,
    })?;
    bytes.push(b'\n');
    write_file_atomic(ledger_path, &bytes, true)
}
