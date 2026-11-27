//! Zero-cost [`HarnessToolProfile`] trait and static harness detection.

use std::borrow::Cow;
use std::collections::HashMap;

pub use liftgate_types::harness_prompt::{
    DetectedHarness, ProtocolHint, detect_harness, detect_harness_by_tools, is_omp_input_edit_spec,
    is_opencode_v4a_patch_spec, json_schema_declares_property,
};
use liftgate_types::{
    canonical::{CanonicalToolKind, CanonicalToolSpec},
    session_key::{
        McpOrchestratorContract, is_code_mode_custom_exec, split_code_mode_ts_declaration,
    },
};

use crate::budget::VirtualizerBudget;

const fn const_str_eq(a: &str, b: &str) -> bool {
    let mut a = a.as_bytes();
    let mut b = b.as_bytes();
    while let (Some((&ha, ta)), Some((&hb, tb))) = (a.split_first(), b.split_first()) {
        if ha != hb {
            return false;
        }
        a = ta;
        b = tb;
    }
    a.is_empty() && b.is_empty()
}

const _: () = {
    assert!(
        const_str_eq(
            DetectedHarness::Codex.id(),
            crate::profiles::CodexToolProfile::ID
        ),
        "DetectedHarness::Codex.id() must match CodexToolProfile::ID"
    );
    assert!(
        const_str_eq(
            DetectedHarness::ClaudeCode.id(),
            crate::profiles::ClaudeCodeToolProfile::ID
        ),
        "DetectedHarness::ClaudeCode.id() must match ClaudeCodeToolProfile::ID"
    );
    assert!(
        const_str_eq(
            DetectedHarness::OpenCode.id(),
            crate::profiles::OpenCodeToolProfile::ID
        ),
        "DetectedHarness::OpenCode.id() must match OpenCodeToolProfile::ID"
    );
    assert!(
        const_str_eq(
            DetectedHarness::Omp.id(),
            crate::profiles::OmpToolProfile::ID
        ),
        "DetectedHarness::Omp.id() must match OmpToolProfile::ID"
    );
    assert!(
        const_str_eq(
            DetectedHarness::Generic.id(),
            crate::profiles::GenericToolProfile::ID
        ),
        "DetectedHarness::Generic.id() must match GenericToolProfile::ID"
    );
};

/// Tiered placement decision returned by [`HarnessToolProfile::classify_tool`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolPlacement<'a> {
    /// Tier 0: Code Mode orchestrator (`exec`, `wait`, `REPL`).
    ///
    /// Guaranteed a 1:1 native Eager MCP slot and full orchestrator description budget.
    Orchestrator {
        /// When `Some(key)` on a freeform `Custom` tool (such as `Some("code")` for Codex `exec`),
        /// wraps raw script text in a single-property JSON schema and unwraps it losslessly.
        custom_unwrap_key: Option<&'static str>,
    },
    /// Tier 1: High-frequency core coding loop tool (`exec_command`, `Bash`, `Read`, `Edit`, `Agent`).
    ///
    /// Guaranteed a 1:1 native Eager MCP slot ahead of namespace hubs, ordered by `rank` ascending.
    CoreDirect {
        /// Sub-priority within Tier 1 (`0` highest ..= `255` lowest).
        rank: u8,
    },
    /// Tier 2: Semantic family member folded into `mcp_lg_ns__<namespace>` when virtualization activates.
    Namespaced {
        namespace: Cow<'a, str>,
        action: Cow<'a, str>,
    },
    /// Tier 2 standalone tool without a multi-action family.
    ///
    /// Promoted to a 1:1 native Eager MCP slot via slack-slot backfill whenever Eager slot and
    /// wire-byte budgets permit; otherwise reachable via `mcp_lg_call_client_tool` and 0-RTT
    /// `mcp_lg_search_client_tools`.
    Standalone {
        /// Sub-priority for slack-slot backfill (`0` highest ..= `255` lowest).
        backfill_rank: u8,
    },
    /// Elided from top-level `tools/list` (`wire_tools`) when a Tier-0 Code Mode orchestrator is
    /// active (such as Oh My Pi's `eval` `tool.<name>(args)` bridge), matching OpenAI Codex's
    /// Code Mode tool elision while remaining indexed in `search_client_tools` and summarized
    /// onto the orchestrator's wire description.
    FoldedIntoOrchestrator,
}

/// V4A (`*** Begin Patch` ... `*** End Patch`) contract recognized for a client tool.
///
/// Ordinary client tools named `patch` (such as unified-diff or RFC-6902 JSON Patch tools) return
/// `None` from [`HarnessToolProfile::v4a_patch_contract`] so their payloads are forwarded verbatim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum V4aPatchContract {
    /// Freeform `CanonicalToolKind::Custom` V4A patch tool.
    CustomFreeform,
    /// JSON `CanonicalToolKind::Function` V4A patch tool storing V4A text in the named property.
    JsonField(&'static str),
}

/// Default V4A contract detector matching `apply_patch` across Codex and generic harnesses.
#[must_use]
pub fn default_apply_patch_v4a_contract(spec: &CanonicalToolSpec) -> Option<V4aPatchContract> {
    if !spec.name.eq_ignore_ascii_case("apply_patch") {
        return None;
    }
    match &spec.kind {
        CanonicalToolKind::Custom { .. } => Some(V4aPatchContract::CustomFreeform),
        CanonicalToolKind::Function { parameters_json } => {
            if json_schema_declares_property(parameters_json, "patchText") {
                Some(V4aPatchContract::JsonField("patchText"))
            } else if json_schema_declares_property(parameters_json, "patch") {
                Some(V4aPatchContract::JsonField("patch"))
            } else {
                None
            }
        }
    }
}

/// Zero-cost compile-time adapter tailoring 3-tier MCP tool virtualization to a coding harness.
///
/// Implementations are zero-sized unit structs (`Copy + 'static`) monomorphized directly into
/// [`crate::plan_virtualization`] with zero heap allocation or `dyn` vtable dispatch.
pub trait HarnessToolProfile: Copy + Default + Send + Sync + 'static {
    /// Stable lowercase identifier (`"codex"`, `"claude-code"`, `"generic"`) for diagnostics.
    const ID: &'static str;

    /// `agy acp` Eager tool & wire byte budget for this harness.
    const BUDGET: VirtualizerBudget = VirtualizerBudget::DEFAULT_AGY_ACP;

    /// Profile-specific catalog prepass state built once per tool inventory.
    type Prepass;

    /// Computes profile-specific family/stem counts once across `tools`.
    fn build_prepass(self, tools: &[CanonicalToolSpec]) -> Self::Prepass;

    /// Classifies an eligible caller tool into Tier 0 (`Orchestrator`), Tier 1 (`CoreDirect`),
    /// Tier 2 (`Namespaced`), or `Standalone` backfill candidate.
    fn classify_tool<'a>(
        self,
        spec: &'a CanonicalToolSpec,
        prepass: &Self::Prepass,
    ) -> ToolPlacement<'a>;

    /// Returns the Code Mode or compound-I/O orchestration prompt contract to inject into
    /// `<api_gateway_bridge_rules>` when `spec` is mounted directly on `lg` under `mcp_name`.
    #[inline]
    fn orchestrator_prompt_contract<'a>(
        self,
        _spec: &CanonicalToolSpec,
        _mcp_name: &'a str,
        _has_folded_tools: bool,
        _client_tools: &[CanonicalToolSpec],
    ) -> Option<McpOrchestratorContract<'a>> {
        None
    }

    /// Dehydrates embedded Code Mode TypeScript declarations out of non-orchestrator tool
    /// descriptions so wire `tools/list` schemas stay compact while 0-RTT search retains
    /// full TypeScript signatures.
    #[inline]
    fn dehydrate_description(self, raw_desc: &str, is_orchestrator: bool) -> (&str, Option<&str>) {
        if is_orchestrator {
            (raw_desc.trim(), None)
        } else {
            split_code_mode_ts_declaration(raw_desc)
        }
    }

    /// Optional zero-allocation domain aliases indexed alongside `spec` in `search_client_tools`.
    #[inline]
    fn search_aliases(self, _spec: &CanonicalToolSpec, _namespace: Option<&str>) -> &'static str {
        ""
    }

    /// Identifies whether `spec` uses the V4A `*** Begin Patch` grammar so the router can heal and
    /// preflight-validate V4A hunks without touching generic unified-diff `patch` tools.
    #[inline]
    fn v4a_patch_contract(self, spec: &CanonicalToolSpec) -> Option<V4aPatchContract> {
        default_apply_patch_v4a_contract(spec)
    }

    /// Returns `(unwrap_key, input_schema)` exposing any freeform `CanonicalToolKind::Custom` tool
    /// natively over JSON MCP and unwrapping its single string property back to raw freeform text
    /// on dispatch, eliminating any need for `<agy_tool_call>` XML simulation in MCP sessions.
    #[inline]
    fn custom_tool_mcp_bridge(self, spec: &CanonicalToolSpec) -> (&'static str, serde_json::Value) {
        if is_code_mode_custom_exec(spec) || spec.name.eq_ignore_ascii_case("js_repl") {
            return ("code", crate::schema::code_mode_exec_input_schema());
        }
        if matches!(
            self.v4a_patch_contract(spec),
            Some(V4aPatchContract::CustomFreeform)
        ) {
            return ("patch", crate::schema::apply_patch_input_schema());
        }
        ("input", crate::schema::freeform_custom_input_schema())
    }
}

/// Borrows `s` directly when already ASCII-lowercase, allocating only when uppercase bytes exist.
#[inline]
#[must_use]
pub fn ascii_lowercase_cow(s: &str) -> Cow<'_, str> {
    if s.bytes().all(|b| !b.is_ascii_uppercase()) {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(s.to_ascii_lowercase())
    }
}

/// Normalizes a server/connector identifier to lowercase `snake_case`, borrowing when clean.
#[must_use]
pub fn normalize_namespace_slug(raw: &str) -> Cow<'_, str> {
    let clean = raw
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if clean {
        return Cow::Borrowed(raw);
    }
    let mut out = String::with_capacity(raw.len());
    let mut prev_underscore = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_underscore = false;
        } else if ch == '-' {
            out.push('-');
            prev_underscore = false;
        } else if !prev_underscore && !out.is_empty() {
            out.push('_');
            prev_underscore = true;
        }
    }
    while out.ends_with('_') {
        let _ = out.pop();
    }
    if out.is_empty() {
        Cow::Borrowed("ext")
    } else {
        Cow::Owned(out)
    }
}

/// Splits `<server>.<action>` or `<server>__<action>` (whichever delimiter comes first).
#[must_use]
pub fn split_mcp_server_and_action(trimmed: &str) -> Option<(&str, &str)> {
    let dot_pos = trimmed.find('.');
    let dunder_pos = trimmed.find("__");
    let choice = match (dot_pos, dunder_pos) {
        (Some(d), Some(u)) => {
            if d < u {
                Some((d, 1usize))
            } else {
                Some((u, 2usize))
            }
        }
        (Some(d), None) => Some((d, 1usize)),
        (None, Some(u)) => Some((u, 2usize)),
        (None, None) => None,
    };
    let (idx, sep_len) = choice?;
    let (ns_raw, rest_with_sep) = trimmed.split_at_checked(idx)?;
    let (_, action_raw) = rest_with_sep.split_at_checked(sep_len)?;
    if ns_raw.is_empty() || action_raw.is_empty() {
        return None;
    }
    Some((ns_raw, action_raw))
}

/// Counts generic `<stem>_*` fallback prefixes across a catalog.
#[must_use]
pub fn count_fallback_prefix_stems(tools: &[CanonicalToolSpec]) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for spec in tools {
        let name = spec.name.as_str();
        if matches!(name, "todowrite" | "todoread") {
            *counts.entry("todo_pair".to_owned()).or_insert(0) += 1;
        }
        if name.starts_with("mcp__") {
            continue;
        }
        if name.contains('.') {
            continue;
        }
        if let Some((stem, rest)) = name.split_once('_')
            && stem.len() >= 2
            && !rest.is_empty()
        {
            let key = stem.to_ascii_lowercase();
            *counts.entry(key).or_insert(0) += 1;
        }
    }
    counts
}
