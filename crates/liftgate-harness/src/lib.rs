//! Synchronous harness adapters for Claude Code, OpenAI Codex, OpenCode, and Oh My Pi (`omp`).
//!
//! Owns configuration schemas, structure-preserving YAML/TOML/JSON surgical editors, displaced
//! provider/setting ledgers, and cross-platform executable & Windows shim discovery with zero
//! async runtime, HTTP, or server dependencies so it compiles in parallel with `liftgate-server`.

pub mod args_util;
pub mod claude;
pub mod codex;
pub mod error;
pub mod fs_util;
pub mod ledger;
pub mod omp;
pub mod opencode;
pub mod resolve;
pub mod yaml_edit;

use liftgate_types::{
    acp::AvailableModel,
    config::ModelId,
    model::{GEMINI_DEFAULT_WINDOW, select_fastest_gemini_model},
};
use serde::{Deserialize, Serialize};

pub use error::HarnessError;
pub use fs_util::{extract_url_host, merge_endpoint_no_proxy, merge_loopback_no_proxy};
pub use resolve::ResolvedCommand;

/// Model metadata discovered from `/v1/models` on a running Liftgate daemon.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub context_window: u64,
    #[serde(default)]
    pub auto_compact_token_limit: u64,
}

impl DiscoveredModel {
    /// Uses the live gateway's complete context policy, or a conservative fallback for old servers.
    #[must_use]
    pub fn context_limits(&self) -> (u64, u64) {
        if self.auto_compact_token_limit >= 100_000
            && self.auto_compact_token_limit <= self.context_window
            && self.context_window <= GEMINI_DEFAULT_WINDOW
        {
            (self.context_window, self.auto_compact_token_limit)
        } else {
            liftgate_types::model::context_limits(&self.id)
        }
    }

    #[must_use]
    pub fn to_available_model(&self) -> AvailableModel {
        AvailableModel {
            model_id: self.id.clone(),
            name: if self.display_name.is_empty() {
                self.id.clone()
            } else {
                self.display_name.clone()
            },
        }
    }
}

/// Code Mode policy applied when generating Oh My Pi (`omp`) configuration or ephemeral overlays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OmpCodeModePolicy {
    /// Keep persistent `eval` (`py` + `js`) and `tools.commands` enabled with top-level/virtualized MCP tools.
    #[default]
    Standard,
    /// Mount MCP servers directly into `eval` global namespaces (`mcp.evalNamespaces: true`).
    FullEvalNamespaces,
    /// Disable `eval` Python/JS kernels (`eval.py: false`, `eval.js: false`, `mcp.evalNamespaces: false`).
    Disabled,
}

/// Shared gateway and model parameters supplied to per-harness configuration builders.
#[derive(Clone, Debug)]
pub struct HarnessSyncParams {
    pub gateway_base_url: String,
    pub api_key: String,
    pub primary_model: String,
    pub fast_model: String,
    pub no_bypass: bool,
    pub no_code_mode: bool,
    pub discovered_models: Vec<DiscoveredModel>,
}

/// Strips an optional `agy/` provider prefix from a model selector (`agy/gemini-4-carbon-high` -> `gemini-4-carbon-high`).
#[must_use]
pub fn strip_agy_provider_prefix(model: &str) -> &str {
    model
        .trim()
        .strip_prefix("agy/")
        .unwrap_or_else(|| model.trim())
}

/// Selects the fast auxiliary model for harness background/subagent tasks, skipping Gemini 3.8 Flash.
#[must_use]
pub fn resolve_fast_model(
    explicit_fast: Option<&str>,
    primary_model: &str,
    discovered: &[DiscoveredModel],
) -> String {
    if let Some(exp) = explicit_fast.map(str::trim).filter(|s| !s.is_empty()) {
        return exp.to_owned();
    }
    let available: Vec<_> = discovered
        .iter()
        .map(DiscoveredModel::to_available_model)
        .collect();
    let fallback = ModelId::try_new(primary_model).unwrap_or_else(|_| ModelId::fallback_fast());
    select_fastest_gemini_model(&available, &fallback)
        .as_str()
        .to_owned()
}
