//! Zero-cost [`HarnessToolProfile`] trait and static harness detection.

use std::borrow::Cow;
use std::collections::HashMap;

use liftgate_types::{
    canonical::{CanonicalToolKind, CanonicalToolSpec},
    session_key::{is_code_mode_custom_exec, split_code_mode_ts_declaration},
};

use crate::budget::VirtualizerBudget;

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

    /// Classifies an eligible caller tool into Tier 0 (`Orchestrator`), Tier 1 (`CoreDirect`),
    /// Tier 2 (`Namespaced`), or `Standalone` backfill candidate.
    fn classify_tool<'a>(
        self,
        spec: &'a CanonicalToolSpec,
        fallback_stem_counts: &HashMap<String, usize>,
    ) -> ToolPlacement<'a>;

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

    /// Returns `(unwrap_key, input_schema)` exposing any freeform `CanonicalToolKind::Custom` tool
    /// natively over JSON MCP and unwrapping its single string property back to raw freeform text
    /// on dispatch, eliminating any need for `<agy_tool_call>` XML simulation in MCP sessions.
    #[inline]
    fn custom_tool_mcp_bridge(self, spec: &CanonicalToolSpec) -> (&'static str, serde_json::Value) {
        if is_code_mode_custom_exec(spec) || spec.name.eq_ignore_ascii_case("js_repl") {
            return ("code", crate::schema::code_mode_exec_input_schema());
        }
        if spec.name.eq_ignore_ascii_case("apply_patch") {
            return ("patch", crate::schema::apply_patch_input_schema());
        }
        ("input", crate::schema::freeform_custom_input_schema())
    }
}

/// Optional ingress wire-protocol hint supplied by the HTTP route layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProtocolHint {
    #[default]
    Unknown,
    OpenAiResponses,
    AnthropicMessages,
}

/// Closed set of statically dispatched built-in harness profiles.
///
/// Adding a new harness (e.g. `Omp` or `OpenCode`) requires only implementing
/// [`HarnessToolProfile`] on a new ZST and adding a variant here—exhaustive `match` blocks
/// without `_` wildcards ensure compile-time completeness across planning and routing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetectedHarness {
    Codex,
    ClaudeCode,
    Generic,
}

impl DetectedHarness {
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Codex => crate::profiles::CodexToolProfile::ID,
            Self::ClaudeCode => crate::profiles::ClaudeCodeToolProfile::ID,
            Self::Generic => crate::profiles::GenericToolProfile::ID,
        }
    }
}

/// Detects the active caller harness from tool signatures and optional wire-protocol hint.
#[must_use]
pub fn detect_harness(hint: ProtocolHint, client_tools: &[CanonicalToolSpec]) -> DetectedHarness {
    let mut codex_score: u32 = 0;
    let mut claude_score: u32 = 0;

    for spec in client_tools {
        let name = spec.name.as_str();
        if is_code_mode_custom_exec(spec)
            || matches!(
                name,
                "exec_command"
                    | "write_stdin"
                    | "__codex_local_shell"
                    | "__codex_tool_search"
                    | "view_image"
                    | "update_plan"
                    | "request_user_input"
                    | "request_user_input_async"
                    | "send_message_to_user_async"
                    | "js_repl"
                    | "js_repl_reset"
            )
            || name.starts_with("mcp__codex_apps__")
            || name.starts_with("mcp__codex_tui")
            || name.starts_with("mcp__node_repl")
            || name.starts_with("mcp__cua_repl")
            || name.starts_with("multi_agent_v1.")
            || (name == "apply_patch" && matches!(spec.kind, CanonicalToolKind::Custom { .. }))
            || spec
                .description
                .as_deref()
                .is_some_and(|d| d.contains("exec tool declaration:"))
        {
            codex_score = codex_score.saturating_add(4);
        }

        if matches!(
            name,
            "Bash"
                | "PowerShell"
                | "MultiEdit"
                | "NotebookEdit"
                | "TodoWrite"
                | "TaskCreate"
                | "TaskUpdate"
                | "TaskGet"
                | "TaskList"
                | "TaskOutput"
                | "TaskStop"
                | "BashOutput"
                | "KillShell"
                | "EnterPlanMode"
                | "ExitPlanMode"
                | "EnterWorktree"
                | "ExitWorktree"
                | "AskUserQuestion"
                | "Skill"
                | "SlashCommand"
                | "ToolSearch"
                | "REPL"
                | "Agent"
                | "TeamCreate"
                | "TeamDelete"
                | "SendMessage"
                | "CronCreate"
                | "ScheduleWakeup"
                | "ListMcpResourcesTool"
                | "ReadMcpResourceTool"
        ) || name.starts_with("mcp__claude_ai_")
            || name.starts_with("mcp__plugin_")
        {
            claude_score = claude_score.saturating_add(4);
        } else if matches!(
            name,
            "Read" | "Edit" | "Write" | "Glob" | "Grep" | "LS" | "LSP" | "WebFetch" | "WebSearch"
        ) {
            claude_score = claude_score.saturating_add(2);
        }
    }

    if codex_score > claude_score {
        return DetectedHarness::Codex;
    }
    if claude_score > codex_score {
        return DetectedHarness::ClaudeCode;
    }

    match hint {
        ProtocolHint::OpenAiResponses => DetectedHarness::Codex,
        ProtocolHint::AnthropicMessages => DetectedHarness::ClaudeCode,
        ProtocolHint::Unknown => DetectedHarness::Generic,
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

/// Counts fallback `<stem>_*` and single-underscore `mcp__<server>_*` prefixes across a catalog.
#[must_use]
pub fn count_fallback_prefix_stems(tools: &[CanonicalToolSpec]) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for spec in tools {
        let name = spec.name.as_str();
        if let Some(after_mcp) = name.strip_prefix("mcp__") {
            if split_mcp_server_and_action(after_mcp).is_none()
                && let Some((srv, rest)) = after_mcp.split_once('_')
                && srv.len() >= 2
                && !rest.is_empty()
            {
                let key = format!("mcp1:{}", srv.to_ascii_lowercase());
                *counts.entry(key).or_insert(0) += 1;
            }
            continue;
        }
        if name.contains('.') {
            continue;
        }
        if let Some((stem, rest)) = name.split_once('_')
            && stem.len() >= 3
            && !rest.is_empty()
        {
            let key = stem.to_ascii_lowercase();
            *counts.entry(key).or_insert(0) += 1;
        }
    }
    counts
}
