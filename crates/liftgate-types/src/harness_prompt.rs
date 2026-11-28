//! Tool-signature-driven harness identification and version-resilient system prompt distillation.
//!
//! ### Core Design Invariants
//! 1. **Zero System-Prompt Sniffing for Identification**: [`detect_harness_by_tools`] inspects
//!    **only** normalized caller tool definitions ([`CanonicalToolSpec`] names, JSON schema
//!    properties, and tool-level Code Mode / URI metadata) — never `system` or `instructions`
//!    text, which changes across harness releases and could misclassify generic API callers.
//! 2. **Safe Generic Passthrough**: Callers without positive characteristic harness tools (such
//!    as raw OpenAI/Anthropic SDK scripts, zero-tool chats, `/compact` summarization turns, or
//!    unfamiliar third-party agents) yield `None` from [`detect_harness_by_tools`] and keep their
//!    original prompts untouched.
//! 3. **Protocol-Slot & Structural Distillation**: Once a known CLI harness is positively
//!    identified by its tools, multi-kilobyte built-in CLI prompts are replaced at the wire
//!    protocol slot boundary with a compact ACP-aligned harness contract (`~500–600B`) while
//!    deterministically preserving workspace environment blocks (`<env>`, `<environment_context>`,
//!    `gitStatus:`), project/user rules (`<project-context>`, `<repo-rules>`, `<INSTRUCTIONS>`,
//!    `Instructions from:`, `--append-system-prompt`, short custom subagent prompts), sandbox
//!    `<permissions instructions>`, active Plan Mode contracts, and compact skill path indexes.

use std::collections::BTreeSet;

use crate::canonical::{
    CanonicalMessage, CanonicalPart, CanonicalToolKind, CanonicalToolSpec, ProtocolDeveloperSlot,
    ProtocolDeveloperSlotKind,
};
use crate::session_key::{
    is_code_mode_custom_exec, is_codex_exec_orchestrator_tool, is_omp_eval_orchestrator_tool,
    is_opencode_execute_orchestrator_tool, strip_billing_noise, wrap_developer_instructions_block,
};

/// Minimum UTF-8 byte length above which a harness's primary `system` / `instructions` slot is
/// treated as built-in CLI boilerplate and replaced unconditionally (preserving structured
/// workspace/rule blocks embedded within it), independent of version-specific prompt prose.
///
/// Shorter primary prompts (`< 1,500B`) that do not start with a known built-in CLI preamble
/// (for example, specialized `.claude/agents/*.md` subagent roles or explicit `--system-prompt`
/// overrides) are retained alongside the compact ACP formatting contract.
const BUILTIN_BASE_PROMPT_MIN_BYTES: usize = 1_500;

/// Maximum UTF-8 bytes of trailing `gitStatus:` snapshot retained from a built-in system block.
const MAX_PRESERVED_GIT_STATUS_BYTES: usize = 2_400;

/// Compact Codex CLI & Codex App system contract (`< 900B`), delegating tool-routing rules to
/// `<api_gateway_bridge_rules>` while aligning live commentary + `update_plan` progress and
/// overriding host `file://` and `<appDataDir>/brain` habits.
pub const CODEX_DISTILLED_PROMPT: &str = "\
You are a coding agent running in the Codex CLI. Be precise, safe, and concise.
- Resolve the user's request end-to-end; inspect the workspace directly (`rg`, `rg --files`) rather than guessing, and validate changes with targeted checks.
- Obey scoped `AGENTS.md` files (deeper overrides parent; user/developer instructions take precedence). Never commit or create git branches unless asked.
- Share concise 1-sentence progress commentary before starting tool work and when shifting phases or uncovering key findings (skip pleasantries/filler and skip updates when nothing changed); for multi-step work when `update_plan` is available (never in `Plan Mode`), maintain a short 3-7 step plan with at most one `in_progress` step without repeating the checklist in prose.
- Keep your final answer self-contained in GitHub-flavored Markdown (earlier commentary collapses once the final answer renders), wrap identifiers/paths/commands in backticks, and format file references as inline `path/to/file:line` (never emit `file://` URIs, `【F:...】` citations, or host `<appDataDir>/brain` artifacts).";

/// Compact Claude Code (`claude`) system contract (`< 550B`).
pub const CLAUDE_CODE_DISTILLED_PROMPT: &str = "\
You are Claude Code, a terminal coding agent. Be direct, accurate, and concise.
- Inspect files and code directly before editing; make minimal, targeted changes within scope and verify with relevant builds/tests.
- Honor `CLAUDE.md`, `AGENTS.md`, and active `<system-reminder>` / Plan Mode constraints. Never commit, push, or open PRs unless explicitly asked.
- Keep responses brief and scannable in Markdown; reference code as `path/to/file:line` (never emit `file://` URIs or write host `<appDataDir>/brain` artifacts).";

/// Compact OpenCode (`opencode`) system contract (`< 550B`).
pub const OPENCODE_DISTILLED_PROMPT: &str = "\
You are OpenCode, an interactive CLI coding agent. Be concise, precise, and direct.
- Inspect the workspace before making assumptions, keep edits minimal and idiomatic, and verify changes when appropriate.
- Follow any `AGENTS.md`, `CLAUDE.md`, project rules, and active mode constraints (`plan` / read-only vs build). Never commit or push unless asked.
- Keep replies terse in Markdown and cite locations as `path/to/file:line` (never emit `file://` URIs or host `<appDataDir>/brain` artifacts).";

/// Compact Oh My Pi (`omp`) system contract (`< 600B`).
pub const OMP_DISTILLED_PROMPT: &str = "\
You are Oh My Pi (`omp`), an evidence-first CLI coding agent. Be terse, accurate, and direct.
- Inspect code before editing, prefer simple zero-overhead designs, verify changes with targeted checks, and never commit or push unless asked.
- Honor `<project-context>`, `AGENTS.md`, and active mode constraints.
- When using `omp`'s `edit` tool with hashline anchors, always copy exact `LINE#TAG` anchors returned by `read` (never invent tags).
- Reference code as `path/to/file:line` (never emit `file://` URIs or host `<appDataDir>/brain` artifacts).";

/// Optional ingress wire-protocol hint supplied by the HTTP route or virtualizer layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProtocolHint {
    #[default]
    Unknown,
    OpenAiResponses,
    AnthropicMessages,
}

/// Closed set of statically dispatched built-in harness profiles.
///
/// Exhaustive `match` blocks without `_` wildcards ensure compile-time completeness across
/// tool virtualization and system prompt distillation whenever a new harness is added.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetectedHarness {
    Codex,
    ClaudeCode,
    OpenCode,
    Omp,
    Generic,
}

impl DetectedHarness {
    /// Stable lowercase identifier matching `HarnessToolProfile::ID`.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::OpenCode => "opencode",
            Self::Omp => "omp",
            Self::Generic => "generic",
        }
    }
}

/// Returns the compact distilled base system prompt for a known CLI harness, or `None` for
/// [`DetectedHarness::Generic`].
#[must_use]
pub const fn distilled_harness_system_prompt(harness: DetectedHarness) -> Option<&'static str> {
    match harness {
        DetectedHarness::Codex => Some(CODEX_DISTILLED_PROMPT),
        DetectedHarness::ClaudeCode => Some(CLAUDE_CODE_DISTILLED_PROMPT),
        DetectedHarness::OpenCode => Some(OPENCODE_DISTILLED_PROMPT),
        DetectedHarness::Omp => Some(OMP_DISTILLED_PROMPT),
        DetectedHarness::Generic => None,
    }
}

/// Returns `true` when `parameters_json` is a JSON Schema object declaring `prop_name` under `"properties"`.
#[must_use]
pub fn json_schema_declares_property(parameters_json: &str, prop_name: &str) -> bool {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(parameters_json) else {
        return false;
    };
    val.get("properties")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|props| props.contains_key(prop_name))
}

/// Returns `true` when a tool named `patch` carries OpenCode's V4A `patchText` / `*** Begin Patch`
/// contract rather than a generic unified-diff or JSON-Patch schema.
#[must_use]
pub fn is_opencode_v4a_patch_spec(spec: &CanonicalToolSpec) -> bool {
    if spec.name != "patch" {
        return false;
    }
    let desc_has_v4a = spec
        .description
        .as_deref()
        .is_some_and(|d| d.contains("*** Begin Patch"));
    match &spec.kind {
        CanonicalToolKind::Function { parameters_json } => {
            json_schema_declares_property(parameters_json, "patchText")
                || (desc_has_v4a && json_schema_declares_property(parameters_json, "patch"))
        }
        CanonicalToolKind::Custom { format_json } => {
            let grammar_has_v4a = format_json
                .as_deref()
                .is_some_and(|g| g.contains("*** Begin Patch"));
            desc_has_v4a || grammar_has_v4a
        }
    }
}

/// Returns `true` when an `edit` tool carries Oh My Pi (`omp`)'s `{ input: string }` or custom
/// Lark grammar shape (`hashline` / `sloppy` / `magic` / `apply_patch`).
#[must_use]
pub fn is_omp_input_edit_spec(spec: &CanonicalToolSpec) -> bool {
    if spec.name != "edit" {
        return false;
    }
    match &spec.kind {
        CanonicalToolKind::Custom { .. } => true,
        CanonicalToolKind::Function { parameters_json } => {
            json_schema_declares_property(parameters_json, "input")
                && !json_schema_declares_property(parameters_json, "oldString")
                && !json_schema_declares_property(parameters_json, "old_string")
        }
    }
}

#[must_use]
fn is_omp_distinctive_bash_or_todo_spec(spec: &CanonicalToolSpec) -> bool {
    let CanonicalToolKind::Function { parameters_json } = &spec.kind else {
        return false;
    };
    match spec.name.as_str() {
        "bash" => {
            json_schema_declares_property(parameters_json, "pty")
                && json_schema_declares_property(parameters_json, "ready")
        }
        "todo" => {
            json_schema_declares_property(parameters_json, "op")
                && json_schema_declares_property(parameters_json, "items")
        }
        _ => false,
    }
}

/// Runtime context signal tags preserved verbatim when distilling a harness's primary base
/// prompt (`AGENTS.md`: forwarded to the model intact and normalized only inside
/// `session_key::strip_unstable_harness_noise` for deterministic prefix hashing).
const RUNTIME_CONTEXT_SIGNAL_TAGS: &[(&str, &str)] = &[
    ("<system-reminder>", "</system-reminder>"),
    ("<total_tokens>", "</total_tokens>"),
    ("<budget:token_budget>", "</budget:token_budget>"),
    ("<context_window>", "</context_window>"),
    ("<context_window_reminder>", "</context_window_reminder>"),
    ("<rollout_budget>", "</rollout_budget>"),
    ("<current_time>", "</current_time>"),
    ("<current_time_reminder>", "</current_time_reminder>"),
    ("<current_time_unavailable>", "</current_time_unavailable>"),
    ("<current_date>", "</current_date>"),
    ("<timestamp>", "</timestamp>"),
];

/// Distinguishes OpenAI Codex's array-command `shell` tool (`command: string[]` + `workdir`)
/// from OpenCode v2's string-command `shell` tool (`command: string`).
#[must_use]
fn is_codex_array_shell_spec(spec: &CanonicalToolSpec) -> bool {
    if spec.name != "shell" {
        return false;
    }
    let CanonicalToolKind::Function { parameters_json } = &spec.kind else {
        return false;
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(parameters_json) else {
        return false;
    };
    let Some(props) = val.get("properties").and_then(serde_json::Value::as_object) else {
        return false;
    };
    props
        .get("command")
        .and_then(|cmd| cmd.get("type"))
        .and_then(serde_json::Value::as_str)
        == Some("array")
}

#[must_use]
fn is_opencode_string_shell_spec(spec: &CanonicalToolSpec) -> bool {
    if spec.name != "shell" {
        return false;
    }
    let CanonicalToolKind::Function { parameters_json } = &spec.kind else {
        return false;
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(parameters_json) else {
        return false;
    };
    let Some(props) = val.get("properties").and_then(serde_json::Value::as_object) else {
        return false;
    };
    props
        .get("command")
        .and_then(|cmd| cmd.get("type"))
        .and_then(serde_json::Value::as_str)
        == Some("string")
}

/// Strictly identifies a known CLI coding harness from characteristic tool signatures alone.
///
/// Returns `None` when `client_tools` is empty or lacks distinctive harness-owned tools, ensuring
/// generic SDK callers and unknown third-party agents never have their system prompts rewritten.
/// Generic single-word tool names (`shell`, `execute`, `question`, `skill`, `Bash`, `Agent`)
/// never establish harness identity on their own without harness-specific metadata or companion
/// harness tools.
#[must_use]
pub fn detect_harness_by_tools(client_tools: &[CanonicalToolSpec]) -> Option<DetectedHarness> {
    let mut has_codex_strong_identity = false;
    let mut codex_companion_count: u32 = 0;
    let mut codex_score: u32 = 0;

    let mut has_claude_strong_identity = false;
    let mut claude_companion_count: u32 = 0;
    let mut claude_score: u32 = 0;

    let mut has_opencode_strong_identity = false;
    let mut has_opencode_v2_marker = false;
    let mut opencode_companion_count: u32 = 0;
    let mut opencode_score: u32 = 0;

    let mut has_omp_identity = false;
    let mut omp_primary_score: u32 = 0;
    let mut omp_secondary_score: u32 = 0;

    for spec in client_tools {
        let name = spec.name.as_str();
        if (is_code_mode_custom_exec(spec) && name != "execute")
            || is_codex_exec_orchestrator_tool(spec)
            || matches!(
                name,
                "exec_command"
                    | "write_stdin"
                    | "__codex_local_shell"
                    | "__codex_tool_search"
                    | "request_user_input_async"
                    | "send_message_to_user_async"
                    | "send_user_message_async"
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
            has_codex_strong_identity = true;
            codex_score = codex_score.saturating_add(4);
        } else if is_codex_array_shell_spec(spec)
            || matches!(
                name,
                "shell_command"
                    | "apply_patch"
                    | "view_image"
                    | "update_plan"
                    | "request_user_input"
                    | "spawn_agent"
                    | "wait_agent"
                    | "close_agent"
                    | "resume_agent"
            )
            || name.starts_with("clock.")
        {
            codex_companion_count = codex_companion_count.saturating_add(1);
            codex_score = codex_score.saturating_add(2);
        }

        if matches!(
            name,
            "MultiEdit"
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
                | "SlashCommand"
                | "ToolSearch"
                | "TeamCreate"
                | "TeamDelete"
                | "CronCreate"
                | "ScheduleWakeup"
                | "ListMcpResourcesTool"
                | "ReadMcpResourceTool"
        ) || name.starts_with("mcp__claude_ai_")
            || name.starts_with("mcp__plugin_")
        {
            has_claude_strong_identity = true;
            claude_score = claude_score.saturating_add(4);
        } else if matches!(
            name,
            "Bash"
                | "PowerShell"
                | "REPL"
                | "Agent"
                | "Skill"
                | "SendMessage"
                | "Monitor"
                | "Read"
                | "Edit"
                | "Write"
                | "Glob"
                | "Grep"
                | "LS"
                | "LSP"
                | "WebFetch"
                | "WebSearch"
        ) {
            claude_companion_count = claude_companion_count.saturating_add(1);
            let weight = if matches!(name, "Bash" | "PowerShell" | "REPL" | "Agent" | "Skill") {
                4
            } else {
                2
            };
            claude_score = claude_score.saturating_add(weight);
        }

        if matches!(
            name,
            "subagent"
                | "webfetch"
                | "websearch"
                | "todowrite"
                | "todoread"
                | "codesearch"
                | "multiedit"
        ) || is_opencode_v4a_patch_spec(spec)
            || is_opencode_execute_orchestrator_tool(spec)
            || name.starts_with("opencode_")
            || name.starts_with("opencode.")
            || spec
                .description
                .as_deref()
                .is_some_and(|d| d.contains("Code Mode runtime") || d.contains("Code Mode catalog"))
        {
            has_opencode_strong_identity = true;
            opencode_score = opencode_score.saturating_add(4);
        } else if is_opencode_string_shell_spec(spec)
            || matches!(name, "execute" | "question" | "skill" | "batch")
            || name.starts_with("automation_")
            || name.starts_with("machine_")
        {
            has_opencode_v2_marker = true;
            opencode_companion_count = opencode_companion_count.saturating_add(1);
            opencode_score = opencode_score.saturating_add(4);
        } else if matches!(
            name,
            "bash" | "read" | "edit" | "write" | "glob" | "grep" | "lsp"
        ) {
            opencode_companion_count = opencode_companion_count.saturating_add(1);
            opencode_score = opencode_score.saturating_add(2);
        }

        if is_omp_eval_orchestrator_tool(spec)
            || is_omp_input_edit_spec(spec)
            || is_omp_distinctive_bash_or_todo_spec(spec)
            || matches!(
                name,
                "ast_edit"
                    | "ast_grep"
                    | "context_notes"
                    | "new_context"
                    | "manage_skill"
                    | "memory_edit"
            )
            || name.starts_with("hindsight_")
            || name.starts_with("mnemopi_")
            || spec.description.as_deref().is_some_and(|d| {
                d.contains("help://")
                    || d.contains("omp://")
                    || d.contains("Copy `[FILENAME#TAG]`")
                    || d.contains("Copy [FILENAME#TAG]")
                    || d.contains("Put line ranges in `path`")
            })
        {
            has_omp_identity = true;
            omp_primary_score = omp_primary_score.saturating_add(5);
        } else if matches!(
            name,
            "checkpoint"
                | "rewind"
                | "security_scan"
                | "ask"
                | "web_search"
                | "retain"
                | "recall"
                | "reflect"
                | "learn"
                | "find"
                | "debug"
                | "yield"
                | "bash"
                | "read"
                | "edit"
                | "write"
                | "glob"
                | "grep"
                | "lsp"
                | "task"
                | "todo"
        ) || name.starts_with("irc_")
        {
            omp_secondary_score = omp_secondary_score.saturating_add(2);
        }
    }

    let omp_effective = if has_omp_identity {
        omp_primary_score.saturating_add(omp_secondary_score)
    } else {
        0
    };
    let opencode_effective = if has_opencode_strong_identity
        || (!has_omp_identity
            && (opencode_companion_count >= 4
                || (has_opencode_v2_marker && opencode_companion_count >= 3)))
    {
        opencode_score
    } else {
        0
    };
    let codex_effective = if has_codex_strong_identity || codex_companion_count >= 2 {
        codex_score
    } else {
        0
    };
    let claude_effective = if has_claude_strong_identity || claude_companion_count >= 2 {
        claude_score
    } else {
        0
    };

    if omp_effective > codex_effective
        && omp_effective > claude_effective
        && omp_effective > opencode_effective
    {
        return Some(DetectedHarness::Omp);
    }
    if opencode_effective > codex_effective && opencode_effective > claude_effective {
        return Some(DetectedHarness::OpenCode);
    }
    if codex_effective > claude_effective
        && codex_effective >= opencode_effective
        && codex_effective > 0
    {
        return Some(DetectedHarness::Codex);
    }
    if claude_effective > codex_effective
        && claude_effective >= opencode_effective
        && claude_effective > 0
    {
        return Some(DetectedHarness::ClaudeCode);
    }

    None
}

/// Detects the active caller harness from tool signatures, falling back to `hint` for MCP tool
/// virtualization when a caller declares only generic/custom tools.
#[must_use]
pub fn detect_harness(hint: ProtocolHint, client_tools: &[CanonicalToolSpec]) -> DetectedHarness {
    detect_harness_by_tools(client_tools).unwrap_or(match hint {
        ProtocolHint::OpenAiResponses => DetectedHarness::Codex,
        ProtocolHint::AnthropicMessages => DetectedHarness::ClaudeCode,
        ProtocolHint::Unknown => DetectedHarness::Generic,
    })
}

/// Distills a single raw `system` / `instructions` string for a positively identified `harness`,
/// preserving embedded workspace metadata (`<env>`, `<project-context>`, `Instructions from:`,
/// `gitStatus:`, compacted `<skills>`) and short custom role overrides.
#[must_use]
pub fn distill_harness_base_prompt(harness: DetectedHarness, raw_system: &str) -> String {
    let cleaned = strip_billing_noise(raw_system);
    let trimmed = cleaned.trim();
    let Some(base_prompt) = distilled_harness_system_prompt(harness) else {
        return trimmed.to_owned();
    };
    if trimmed.is_empty() {
        return base_prompt.to_owned();
    }
    // Idempotence: already distilled on an earlier pass.
    if trimmed.starts_with(base_prompt) {
        return trimmed.to_owned();
    }

    if trimmed.len() >= BUILTIN_BASE_PROMPT_MIN_BYTES {
        let mut sections = vec![base_prompt.to_owned()];
        append_extracted_structured_sections(&mut sections, trimmed);
        return sections.join("\n\n");
    }

    let compacted_custom = compact_auxiliary_developer_tags(trimmed);
    let custom_trimmed = compacted_custom.trim();
    if custom_trimmed.is_empty() || is_exact_identity_banner(custom_trimmed) {
        base_prompt.to_owned()
    } else {
        format!("{base_prompt}\n\n{custom_trimmed}")
    }
}

/// Distills an ordered list of Anthropic `system` blocks for a positively identified `harness`,
/// replacing the built-in CLI system prompt while retaining `<env>`, `Instructions from:`,
/// `gitStatus:`, and user `--append-system-prompt` segments.
#[must_use]
pub fn distill_anthropic_system_prompt(
    harness: DetectedHarness,
    raw_segments: &[String],
) -> Option<String> {
    if raw_segments.is_empty() {
        return None;
    }
    let Some(base_prompt) = distilled_harness_system_prompt(harness) else {
        let joined = raw_segments
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        return (!joined.is_empty()).then_some(joined);
    };

    let meaningful: Vec<&str> = raw_segments
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    let mut remaining = meaningful.as_slice();
    while let Some((&head, rest)) = remaining.split_first() {
        if is_exact_identity_banner(head) {
            remaining = rest;
        } else {
            break;
        }
    }

    let Some((&first, tail)) = remaining.split_first() else {
        return Some(base_prompt.to_owned());
    };

    let mut out_parts = vec![distill_harness_base_prompt(harness, first)];
    let primary_has_env = out_parts
        .first()
        .is_some_and(|p| p.contains("<env>") && p.contains("</env>"));

    for &seg in tail {
        let cleaned_seg = clean_trailing_system_segment(seg, primary_has_env);
        let seg_trimmed = cleaned_seg.trim();
        if !seg_trimmed.is_empty() {
            out_parts.push(seg_trimmed.to_owned());
        }
    }

    Some(out_parts.join("\n\n"))
}

/// Result of distilling one protocol developer slot's text.
enum SlotDistillation {
    /// The slot does not apply to this text (not a wrapped developer block); keep it verbatim.
    Keep,
    /// Forward this text instead.
    Replace(String),
    /// Forward nothing for this part.
    Drop,
}

/// Marks the base prompt as consumed by a slot lying before the rendered range.
fn note_slot_outside_range(kind: &ProtocolDeveloperSlotKind, consumed_base_slot: &mut bool) {
    match kind {
        ProtocolDeveloperSlotKind::PrimaryBase
        | ProtocolDeveloperSlotKind::AnthropicSystem { .. } => {
            *consumed_base_slot = true;
        }
        ProtocolDeveloperSlotKind::DeveloperItem {
            allow_standalone_base,
        } => {
            if *allow_standalone_base {
                *consumed_base_slot = true;
            }
        }
    }
}

/// Distills the text of one protocol developer slot; the single source of truth shared by
/// [`apply_developer_slots_to_slice`] and [`explain_developer_slot_rewrites`].
fn distill_slot_text(
    harness: DetectedHarness,
    kind: &ProtocolDeveloperSlotKind,
    text: &str,
    consumed_base_slot: &mut bool,
) -> SlotDistillation {
    let wrapped_or_drop = |body: &str| {
        wrap_developer_instructions_block(body)
            .map_or(SlotDistillation::Drop, SlotDistillation::Replace)
    };
    match kind {
        ProtocolDeveloperSlotKind::PrimaryBase => {
            *consumed_base_slot = true;
            let Some(inner) = unwrap_developer_instructions(text) else {
                return SlotDistillation::Keep;
            };
            wrapped_or_drop(&distill_harness_base_prompt(harness, inner))
        }
        ProtocolDeveloperSlotKind::AnthropicSystem {
            segments,
            json_schema_directive,
        } => {
            *consumed_base_slot = true;
            let mut distilled_parts = Vec::new();
            if let Some(distilled) = distill_anthropic_system_prompt(harness, segments) {
                distilled_parts.push(distilled);
            }
            if let Some(directive) = json_schema_directive {
                distilled_parts.push(directive.clone());
            }
            wrapped_or_drop(&distilled_parts.join("\n\n"))
        }
        ProtocolDeveloperSlotKind::DeveloperItem {
            allow_standalone_base,
        } => {
            let Some(inner) = unwrap_developer_instructions(text) else {
                return SlotDistillation::Keep;
            };
            if !*consumed_base_slot
                && *allow_standalone_base
                && looks_like_standalone_base_prompt(inner)
            {
                *consumed_base_slot = true;
                wrapped_or_drop(&distill_harness_base_prompt(harness, inner))
            } else {
                distill_harness_developer_item(harness, inner)
                    .map_or(SlotDistillation::Drop, |item| wrapped_or_drop(&item))
            }
        }
    }
}

/// Applies outbound prompt distillation strictly to protocol-originated developer/system slots in
/// `slice` (representing `messages[range_start..]`).
///
/// Conversational `user` and `assistant` parts are never listed in `slots`, so literal
/// `<developer_instructions>` text emitted by the assistant or user is preserved verbatim.
pub fn apply_developer_slots_to_slice(
    harness: DetectedHarness,
    slots: &[ProtocolDeveloperSlot],
    range_start: usize,
    slice: &mut Vec<CanonicalMessage>,
) {
    if distilled_harness_system_prompt(harness).is_none() || slots.is_empty() || slice.is_empty() {
        return;
    }

    let mut consumed_base_slot = false;
    let mut dropped_coords: BTreeSet<(usize, usize)> = BTreeSet::new();
    let mut touched_msgs: BTreeSet<usize> = BTreeSet::new();

    for slot in slots {
        let Some(rel_msg_idx) = slot.msg_index.checked_sub(range_start) else {
            note_slot_outside_range(&slot.kind, &mut consumed_base_slot);
            continue;
        };
        let Some(msg) = slice.get_mut(rel_msg_idx) else {
            continue;
        };
        let Some(CanonicalPart::Text(text)) = msg.parts.get_mut(slot.part_index) else {
            continue;
        };
        match distill_slot_text(harness, &slot.kind, text, &mut consumed_base_slot) {
            SlotDistillation::Keep => {}
            SlotDistillation::Replace(next) => *text = next,
            SlotDistillation::Drop => {
                dropped_coords.insert((rel_msg_idx, slot.part_index));
                touched_msgs.insert(rel_msg_idx);
            }
        }
    }

    if !dropped_coords.is_empty() {
        for &rel_msg_idx in &touched_msgs {
            if let Some(msg) = slice.get_mut(rel_msg_idx) {
                let mut part_idx = 0usize;
                msg.parts.retain(|_| {
                    let keep = !dropped_coords.contains(&(rel_msg_idx, part_idx));
                    part_idx = part_idx.saturating_add(1);
                    keep
                });
            }
        }
        let mut msg_idx = 0usize;
        slice.retain(|msg| {
            let drop_empty = touched_msgs.contains(&msg_idx) && msg.parts.is_empty();
            msg_idx = msg_idx.saturating_add(1);
            !drop_empty
        });
    }
}

/// One protocol developer slot whose forwarded text differs from what the harness sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeveloperSlotRewrite<'a> {
    pub msg_index: usize,
    pub part_index: usize,
    pub slot: &'static str,
    /// The harness's text as held in `messages`.
    pub original: &'a str,
    /// The forwarded text; `None` when the part was dropped.
    pub rewritten: Option<String>,
}

/// Stable wire label of a developer slot kind.
#[must_use]
pub const fn developer_slot_label(kind: &ProtocolDeveloperSlotKind) -> &'static str {
    match kind {
        ProtocolDeveloperSlotKind::PrimaryBase => "primary_base",
        ProtocolDeveloperSlotKind::AnthropicSystem { .. } => "anthropic_system",
        ProtocolDeveloperSlotKind::DeveloperItem { .. } => "developer_item",
    }
}

/// Lists, for the whole of `messages`, every developer slot that outbound distillation rewrites
/// or drops, using the exact per-slot logic of [`apply_developer_slots_to_slice`] (rendering from
/// any `range_start` distills each slot identically, so this also describes earlier turns).
#[must_use]
pub fn explain_developer_slot_rewrites<'a>(
    harness: DetectedHarness,
    slots: &[ProtocolDeveloperSlot],
    messages: &'a [CanonicalMessage],
) -> Vec<DeveloperSlotRewrite<'a>> {
    if distilled_harness_system_prompt(harness).is_none() {
        return Vec::new();
    }
    let mut consumed_base_slot = false;
    let mut out = Vec::new();
    for slot in slots {
        let Some(CanonicalPart::Text(text)) = messages
            .get(slot.msg_index)
            .and_then(|msg| msg.parts.get(slot.part_index))
        else {
            continue;
        };
        let rewritten = match distill_slot_text(harness, &slot.kind, text, &mut consumed_base_slot)
        {
            SlotDistillation::Keep => continue,
            SlotDistillation::Replace(next) if next == *text => continue,
            SlotDistillation::Replace(next) => Some(next),
            SlotDistillation::Drop => None,
        };
        out.push(DeveloperSlotRewrite {
            msg_index: slot.msg_index,
            part_index: slot.part_index,
            slot: developer_slot_label(&slot.kind),
            original: text,
            rewritten,
        });
    }
    out
}

/// Distills a single `role: "developer" | "system"` message item (outside the primary
/// `instructions` / `system` base slot) for `harness`, returning `None` when the item contains
/// only redundant harness-internal tool boilerplate.
#[must_use]
pub fn distill_harness_developer_item(harness: DetectedHarness, raw_inner: &str) -> Option<String> {
    let trimmed = raw_inner.trim();
    if trimmed.is_empty() {
        return None;
    }

    if harness == DetectedHarness::Omp && is_omp_internal_prelude_item(trimmed) {
        return None;
    }

    let mut processed = compact_auxiliary_developer_tags(trimmed);
    if harness == DetectedHarness::Omp && processed.contains("</project-context>") {
        processed = transform_tag_blocks(&processed, "<critical>", "</critical>", |_| None);
    }

    let final_trimmed = processed.trim();
    if final_trimmed.is_empty() {
        None
    } else {
        Some(final_trimmed.to_owned())
    }
}

fn unwrap_developer_instructions(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let inner = trimmed
        .strip_prefix("<developer_instructions>")?
        .strip_suffix("</developer_instructions>")?;
    Some(inner.trim())
}

fn is_exact_identity_banner(text: &str) -> bool {
    let normalized = text.trim().trim_end_matches('.');
    normalized.eq_ignore_ascii_case("You are Claude Code")
        || normalized.eq_ignore_ascii_case("You are Claude Code, Anthropic's official CLI for Claude")
        || normalized.eq_ignore_ascii_case("You are a Claude agent, built on Anthropic's Claude Agent SDK")
        || normalized.eq_ignore_ascii_case("You are a coding agent assisting the user via their CLI harness")
        || normalized.eq_ignore_ascii_case(
            "You are OpenCode, an interactive CLI tool that helps users with software engineering tasks",
        )
        || normalized.eq_ignore_ascii_case("You are omp's trusted coding assistant")
        || normalized.eq_ignore_ascii_case(
            "You are a coding agent running in the Codex CLI, a terminal-based coding assistant. Be precise, safe, and helpful",
        )
}

fn looks_like_standalone_base_prompt(inner: &str) -> bool {
    if is_exact_identity_banner(inner) {
        return true;
    }
    if inner.len() < BUILTIN_BASE_PROMPT_MIN_BYTES {
        return false;
    }
    // Do not mistake Codex's structured `<skills_instructions>` / `<permissions instructions>` /
    // `<project-context>` developer item for a primary `instructions` slot when `instructions`
    // was omitted.
    !inner.starts_with("<skills_instructions>")
        && !inner.starts_with("<permissions instructions>")
        && !inner.starts_with("<collaboration_mode>")
        && !inner.starts_with("<project-context>")
        && !inner.starts_with("<INSTRUCTIONS>")
        && !inner.starts_with("<env>")
        && !inner.starts_with("<environment_context>")
}

fn is_omp_internal_prelude_item(trimmed: &str) -> bool {
    if trimmed.contains("<project-context>")
        || trimmed.contains("<repo-rules>")
        || trimmed.contains("<user-rules>")
        || trimmed.contains("<INSTRUCTIONS>")
        || trimmed.contains("<env>")
        || trimmed.contains("<environment_context>")
    {
        return false;
    }
    trimmed.contains("help://eval/")
        || trimmed.contains("eval `commit` namespace")
        || (trimmed.starts_with("# Browser user sessions") && trimmed.contains("Tern block"))
}

fn append_extracted_structured_sections(sections: &mut Vec<String>, raw: &str) {
    let mut outside_containers = transform_tag_blocks(
        raw,
        "<claude_background_info>",
        "</claude_background_info>",
        |_| None,
    );

    for (open_tag, close_tag) in [
        ("<env>", "</env>"),
        ("<environment_context>", "</environment_context>"),
        ("<project-context>", "</project-context>"),
        ("<INSTRUCTIONS>", "</INSTRUCTIONS>"),
        ("<permissions instructions>", "</permissions instructions>"),
    ] {
        for block in extract_all_tag_blocks(raw, open_tag, close_tag) {
            sections.push(block.trim().to_owned());
        }
        outside_containers =
            transform_tag_blocks(&outside_containers, open_tag, close_tag, |_| None);
    }

    for collab_inner in extract_all_tag_inners(raw, "<collaboration_mode>", "</collaboration_mode>")
    {
        sections.push(compact_collaboration_mode(collab_inner));
    }
    outside_containers = transform_tag_blocks(
        &outside_containers,
        "<collaboration_mode>",
        "</collaboration_mode>",
        |_| None,
    );

    if !raw.contains("<project-context>") {
        for (open_tag, close_tag) in [
            ("<repo-rules>", "</repo-rules>"),
            ("<user-rules>", "</user-rules>"),
        ] {
            for block in extract_all_tag_blocks(raw, open_tag, close_tag) {
                sections.push(block.trim().to_owned());
            }
            outside_containers =
                transform_tag_blocks(&outside_containers, open_tag, close_tag, |_| None);
        }
    }

    for skills_inner in
        extract_all_tag_inners(raw, "<skills_instructions>", "</skills_instructions>")
    {
        if let Some(compacted) = compact_codex_skills_instructions(skills_inner) {
            sections.push(compacted);
        }
    }
    outside_containers = transform_tag_blocks(
        &outside_containers,
        "<skills_instructions>",
        "</skills_instructions>",
        |_| None,
    );
    for xml_skills_inner in extract_all_tag_inners(raw, "<skills>", "</skills>") {
        if let Some(compacted) = compact_xml_skills_block(xml_skills_inner) {
            sections.push(compacted);
        }
    }
    outside_containers =
        transform_tag_blocks(&outside_containers, "<skills>", "</skills>", |_| None);

    for &(open_tag, close_tag) in RUNTIME_CONTEXT_SIGNAL_TAGS {
        for block in extract_all_tag_blocks(&outside_containers, open_tag, close_tag) {
            let trimmed_block = block.trim();
            if !trimmed_block.is_empty() {
                sections.push(trimmed_block.to_owned());
            }
        }
        outside_containers =
            transform_tag_blocks(&outside_containers, open_tag, close_tag, |_| None);
    }

    if let Some(inst_from) = extract_instructions_from_tail(raw) {
        sections.push(inst_from);
    }
    if let Some(git_status) = extract_bounded_git_status(raw) {
        sections.push(git_status);
    }
}

fn clean_trailing_system_segment(seg: &str, drop_duplicate_env: bool) -> String {
    let mut text = transform_tag_blocks(
        seg,
        "<claude_background_info>",
        "</claude_background_info>",
        |_| None,
    );
    if drop_duplicate_env {
        text = transform_tag_blocks(&text, "<env>", "</env>", |_| None);
    }
    text = compact_auxiliary_developer_tags(&text);

    let mut kept_lines = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if matches!(
            trimmed,
            "Here is useful information about the environment you are running in:"
                | "Here is some useful information about the environment you are running in:"
        ) {
            continue;
        }
        kept_lines.push(line);
    }
    kept_lines.join("\n")
}

fn compact_auxiliary_developer_tags(text: &str) -> String {
    let mut out = transform_tag_blocks(text, "<apps_instructions>", "</apps_instructions>", |_| {
        None
    });
    out = transform_tag_blocks(
        &out,
        "<claude_background_info>",
        "</claude_background_info>",
        |_| None,
    );
    out = transform_tag_blocks(
        &out,
        "<skills_instructions>",
        "</skills_instructions>",
        compact_codex_skills_instructions,
    );
    out = transform_tag_blocks(&out, "<skills>", "</skills>", compact_xml_skills_block);
    out = transform_tag_blocks(
        &out,
        "<collaboration_mode>",
        "</collaboration_mode>",
        |inner| Some(compact_collaboration_mode(inner)),
    );
    out
}

fn is_excluded_codex_plugin_skill_path(raw_path: &str) -> bool {
    let norm = raw_path
        .trim()
        .trim_matches('`')
        .replace('\\', "/")
        .to_ascii_lowercase();
    norm.contains("/plugins/cache/openai-")
        || norm.contains("/plugins/openai-primary-runtime")
        || norm.contains("/bundled-marketplaces/openai-bundled")
}

fn parse_codex_skill_root_bullet(bullet: &str) -> Option<(&str, &str)> {
    let after_tick = bullet.strip_prefix('`')?;
    let end_tick = after_tick.find('`')?;
    let alias = after_tick.get(..end_tick)?.trim();
    let rest = after_tick.get(end_tick.checked_add(1)?..)?.trim_start();
    let path = rest.strip_prefix('=')?.trim();
    (!alias.is_empty() && !path.is_empty()).then_some((alias, path))
}

fn extract_skill_target_path(paren_or_file_ref: &str) -> &str {
    let inner = paren_or_file_ref
        .trim()
        .strip_prefix('(')
        .unwrap_or(paren_or_file_ref)
        .strip_suffix(')')
        .unwrap_or(paren_or_file_ref)
        .trim();
    inner.strip_prefix("file:").map_or(inner, str::trim)
}

fn compact_codex_skills_instructions(inner: &str) -> Option<String> {
    let mut candidate_roots: Vec<(Option<String>, String)> = Vec::new();
    let mut excluded_aliases: BTreeSet<String> = BTreeSet::new();
    let mut used_aliases: BTreeSet<String> = BTreeSet::new();
    let mut skills = Vec::new();

    for raw_line in inner.lines() {
        let line = raw_line.trim();
        let Some(bullet) = line.strip_prefix("- ") else {
            continue;
        };
        if bullet.starts_with('`') && bullet.contains(" = ") {
            if let Some((alias, path)) = parse_codex_skill_root_bullet(bullet) {
                if is_excluded_codex_plugin_skill_path(path) {
                    excluded_aliases.insert(alias.to_owned());
                } else {
                    candidate_roots.push((Some(alias.to_owned()), line.to_owned()));
                }
            } else if !is_excluded_codex_plugin_skill_path(bullet) {
                candidate_roots.push((None, line.to_owned()));
            }
            continue;
        }
        if let Some((raw_name, _)) = bullet.split_once(':') {
            let name = raw_name.trim();
            if !name.is_empty() {
                let extracted_ref = if let Some(file_idx) = bullet.rfind("(file:")
                    && let Some(file_ref) = bullet.get(file_idx..)
                {
                    Some(file_ref)
                } else if let Some(paren_idx) = bullet.rfind('(')
                    && let Some(paren_ref) = bullet.get(paren_idx..)
                    && paren_ref.contains("SKILL.md")
                {
                    Some(paren_ref)
                } else {
                    None
                };
                if let Some(target_ref) = extracted_ref {
                    let target_path = extract_skill_target_path(target_ref);
                    let root_alias = target_path.split(['/', '\\']).next().unwrap_or("");
                    if excluded_aliases.contains(root_alias)
                        || is_excluded_codex_plugin_skill_path(target_path)
                    {
                        continue;
                    }
                    if !root_alias.is_empty() {
                        used_aliases.insert(root_alias.to_owned());
                    }
                    skills.push(format!("- {name} {target_ref}"));
                }
            }
        }
    }

    if !skills.is_empty() {
        let mut body =
            String::from("<skills_instructions>\nRead a skill's `SKILL.md` only when relevant:\n");
        for (alias_opt, root_line) in &candidate_roots {
            if alias_opt
                .as_ref()
                .is_none_or(|alias| used_aliases.contains(alias))
            {
                body.push_str(root_line);
                body.push('\n');
            }
        }
        for skill in &skills {
            body.push_str(skill);
            body.push('\n');
        }
        body.push_str("</skills_instructions>");
        return Some(body);
    }

    if !excluded_aliases.is_empty() {
        return None;
    }

    if inner.len() < 1_200 {
        let trimmed = inner.trim();
        (!trimmed.is_empty())
            .then(|| format!("<skills_instructions>\n{trimmed}\n</skills_instructions>"))
    } else {
        None
    }
}

fn compact_xml_skills_block(inner: &str) -> Option<String> {
    let mut entries = Vec::new();
    let mut rest = inner;
    while let Some(start) = rest.find("<skill") {
        let Some(after_start) = rest.get(start..) else {
            break;
        };
        let Some(open_gt) = after_start.find('>') else {
            break;
        };
        let Some(body_start_idx) = open_gt.checked_add(1) else {
            break;
        };
        let Some(after_open) = after_start.get(body_start_idx..) else {
            break;
        };
        let Some(close_rel) = after_open.find("</skill>") else {
            break;
        };
        if let Some(skill_xml) = after_open.get(..close_rel) {
            let name = extract_tag_inner(skill_xml, "<name>", "</name>").map(str::trim);
            let path = extract_tag_inner(skill_xml, "<path>", "</path>")
                .or_else(|| extract_tag_inner(skill_xml, "<location>", "</location>"))
                .map(str::trim);
            if let Some(n) = name.filter(|s| !s.is_empty()) {
                match path.filter(|p| !p.is_empty()) {
                    Some(p) => entries.push(format!("- {n}: {p}")),
                    None => entries.push(format!("- {n}")),
                }
            }
        }
        let Some(next_idx) = close_rel.checked_add("</skill>".len()) else {
            break;
        };
        rest = after_open.get(next_idx..).unwrap_or("");
    }

    if !entries.is_empty() {
        let mut out = String::from("<skills>\nRead a skill's `SKILL.md` only when relevant:\n");
        for entry in &entries {
            out.push_str(entry);
            out.push('\n');
        }
        out.push_str("</skills>");
        return Some(out);
    }

    if !inner.contains("<skill") && inner.len() < 1_200 {
        let trimmed = inner.trim();
        (!trimmed.is_empty()).then(|| format!("<skills>\n{trimmed}\n</skills>"))
    } else {
        None
    }
}

fn compact_collaboration_mode(inner: &str) -> String {
    let trimmed = inner.trim();
    let first_line = trimmed.lines().next().unwrap_or("");
    let lower_first = first_line.to_ascii_lowercase();
    if lower_first.contains("default") && !lower_first.contains("plan") {
        String::from(
            "<collaboration_mode>Collaboration Mode: Default. Execute end-to-end with brief milestone commentary; ask one concise plain-text question only if blocked (never emit textual multiple-choice lists).</collaboration_mode>",
        )
    } else {
        format!("<collaboration_mode>\n{trimmed}\n</collaboration_mode>")
    }
}

fn extract_instructions_from_tail(raw: &str) -> Option<String> {
    let search_scope = if let Some(env_end) = raw.find("</env>") {
        let idx = env_end.checked_add("</env>".len())?;
        raw.get(idx..)?
    } else {
        raw
    };
    let pos = if search_scope.starts_with("Instructions from:") {
        Some(0)
    } else {
        search_scope
            .find("\nInstructions from:")
            .and_then(|i| i.checked_add(1))
    }?;
    let slice = search_scope.get(pos..)?.trim();
    (!slice.is_empty()).then(|| slice.to_owned())
}

fn extract_bounded_git_status(raw: &str) -> Option<String> {
    let pos = if raw.starts_with("gitStatus:") {
        Some(0)
    } else {
        raw.find("\ngitStatus:").and_then(|i| i.checked_add(1))
    }?;
    let tail = raw.get(pos..)?;
    let cleaned = transform_tag_blocks(
        tail,
        "<claude_background_info>",
        "</claude_background_info>",
        |_| None,
    );
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() <= MAX_PRESERVED_GIT_STATUS_BYTES {
        Some(trimmed.to_owned())
    } else {
        let end = trimmed.floor_char_boundary(MAX_PRESERVED_GIT_STATUS_BYTES);
        trimmed.get(..end).map(|s| s.trim_end().to_owned())
    }
}

fn count_run_of_byte(bytes: &[u8], start: usize, needle: u8) -> usize {
    let mut count = 0usize;
    while let Some(&b) = bytes.get(start.saturating_add(count)) {
        if b != needle {
            break;
        }
        count = count.saturating_add(1);
    }
    count
}

fn skip_fenced_code_block_at_line_start(text: &str, idx: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let at_line_start = idx == 0 || bytes.get(idx.wrapping_sub(1)) == Some(&b'\n');
    if !at_line_start {
        return None;
    }
    let mut pos = idx;
    while let Some(&b) = bytes.get(pos) {
        if b == b' ' || b == b'\t' {
            pos = pos.saturating_add(1);
        } else {
            break;
        }
    }
    let fence_len = count_run_of_byte(bytes, pos, b'`');
    if fence_len < 3 {
        return None;
    }
    let after_open_fence = pos.checked_add(fence_len)?;
    let rest = text.get(after_open_fence..)?;
    let first_nl = rest.find('\n')?;
    let mut scan = after_open_fence.checked_add(first_nl)?.checked_add(1)?;
    while scan < text.len() {
        let mut line_pos = scan;
        while let Some(&b) = bytes.get(line_pos) {
            if b == b' ' || b == b'\t' {
                line_pos = line_pos.saturating_add(1);
            } else {
                break;
            }
        }
        let close_ticks = count_run_of_byte(bytes, line_pos, b'`');
        let after_ticks = line_pos.saturating_add(close_ticks);
        let line_tail = text.get(after_ticks..).unwrap_or("");
        let line_end_rel = line_tail.find('\n');
        let trailing_slice = match line_end_rel {
            Some(rel) => line_tail.get(..rel).unwrap_or(""),
            None => line_tail,
        };
        if close_ticks >= fence_len
            && trailing_slice
                .bytes()
                .all(|b| b == b' ' || b == b'\t' || b == b'\r')
        {
            return match line_end_rel {
                Some(rel) => after_ticks.checked_add(rel)?.checked_add(1),
                None => Some(text.len()),
            };
        }
        let rel_nl = text.get(scan..).and_then(|s| s.find('\n'))?;
        scan = scan.checked_add(rel_nl)?.checked_add(1)?;
    }
    None
}

fn skip_inline_backtick_span_on_line(text: &str, idx: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let ticks = count_run_of_byte(bytes, idx, b'`');
    if ticks == 0 {
        return None;
    }
    let after_open = idx.checked_add(ticks)?;
    let tail = text.get(after_open..)?;
    let line_len = tail.find('\n').unwrap_or(tail.len());
    let line_bytes = tail.as_bytes().get(..line_len)?;
    let mut rel = 0usize;
    while rel < line_bytes.len() {
        if line_bytes.get(rel) == Some(&b'`') {
            let run = count_run_of_byte(line_bytes, rel, b'`');
            if run == ticks {
                return after_open.checked_add(rel)?.checked_add(run);
            }
            rel = rel.saturating_add(run);
        } else {
            rel = rel.saturating_add(1);
        }
    }
    after_open.checked_add(0)
}

/// Finds the next top-level `(open_start, inner_start, inner_end, close_end)` span for
/// `open_tag` .. `close_tag`, ignoring tag literals inside Markdown inline backticks or fenced
/// code blocks and balancing nested occurrences of `open_tag`.
fn find_unquoted_tag_span(
    text: &str,
    open_tag: &str,
    close_tag: &str,
) -> Option<(usize, usize, usize, usize)> {
    if open_tag.is_empty() || close_tag.is_empty() || !text.contains(open_tag) {
        return None;
    }
    let mut idx = 0usize;
    let mut open_coords: Option<(usize, usize)> = None;
    let mut depth = 0usize;

    while let Some(sub) = text.get(idx..) {
        if sub.is_empty() {
            break;
        }
        if let Some(next_idx) = skip_fenced_code_block_at_line_start(text, idx) {
            idx = next_idx;
            continue;
        }
        if sub.as_bytes().first() == Some(&b'`')
            && let Some(next_idx) = skip_inline_backtick_span_on_line(text, idx)
        {
            idx = next_idx;
            continue;
        }
        if let Some((open_start, inner_start)) = open_coords {
            if sub.starts_with(close_tag) {
                depth = depth.saturating_sub(1);
                let close_end = idx.checked_add(close_tag.len())?;
                if depth == 0 {
                    return Some((open_start, inner_start, idx, close_end));
                }
                idx = close_end;
                continue;
            }
            if sub.starts_with(open_tag) {
                depth = depth.saturating_add(1);
                idx = idx.checked_add(open_tag.len())?;
                continue;
            }
        } else if sub.starts_with(open_tag) {
            let inner_start = idx.checked_add(open_tag.len())?;
            open_coords = Some((idx, inner_start));
            depth = 1;
            idx = inner_start;
            continue;
        }
        let ch = sub.chars().next()?;
        idx = idx.checked_add(ch.len_utf8())?;
    }
    None
}

fn extract_all_tag_blocks<'a>(text: &'a str, open_tag: &str, close_tag: &str) -> Vec<&'a str> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some((open_start, _, _, close_end)) =
        find_unquoted_tag_span(rest, open_tag, close_tag)
    {
        if let Some(block) = rest.get(open_start..close_end) {
            blocks.push(block);
        }
        rest = rest.get(close_end..).unwrap_or("");
    }
    blocks
}

fn extract_tag_inner<'a>(text: &'a str, open_tag: &str, close_tag: &str) -> Option<&'a str> {
    let (_, inner_start, inner_end, _) = find_unquoted_tag_span(text, open_tag, close_tag)?;
    text.get(inner_start..inner_end)
}

fn extract_all_tag_inners<'a>(text: &'a str, open_tag: &str, close_tag: &str) -> Vec<&'a str> {
    let mut inners = Vec::new();
    let mut rest = text;
    while let Some((_, inner_start, inner_end, close_end)) =
        find_unquoted_tag_span(rest, open_tag, close_tag)
    {
        if let Some(inner) = rest.get(inner_start..inner_end) {
            inners.push(inner);
        }
        rest = rest.get(close_end..).unwrap_or("");
    }
    inners
}

fn transform_tag_blocks<F>(text: &str, open_tag: &str, close_tag: &str, mut f: F) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    if !text.contains(open_tag) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((open_start, inner_start, inner_end, close_end)) =
        find_unquoted_tag_span(rest, open_tag, close_tag)
    {
        if let Some(before) = rest.get(..open_start) {
            out.push_str(before);
        }
        if let Some(inner) = rest.get(inner_start..inner_end)
            && let Some(replacement) = f(inner)
        {
            out.push_str(&replacement);
        }
        rest = rest.get(close_end..).unwrap_or("");
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn fn_spec(name: &str, desc: &str, schema: serde_json::Value) -> CanonicalToolSpec {
        CanonicalToolSpec {
            name: name.to_owned(),
            description: (!desc.is_empty()).then(|| desc.to_owned()),
            kind: CanonicalToolKind::Function {
                parameters_json: schema.to_string(),
            },
        }
    }

    #[test]
    fn explained_slot_rewrites_match_the_forwarded_render() {
        use crate::canonical::CanonicalRole;

        let base = format!(
            "<developer_instructions>\nYou are Codex, a coding agent.\n{}\n</developer_instructions>",
            "Follow the harness tutorial. ".repeat(200)
        );
        let messages = vec![
            CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text(base.clone())],
            },
            CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text("hello".to_owned())],
            },
        ];
        let slots = vec![ProtocolDeveloperSlot {
            msg_index: 0,
            part_index: 0,
            kind: ProtocolDeveloperSlotKind::PrimaryBase,
        }];
        let explained = explain_developer_slot_rewrites(DetectedHarness::Codex, &slots, &messages);
        assert_eq!(explained.len(), 1);
        let rewrite = explained.first().expect("one rewrite");
        assert_eq!(
            (rewrite.msg_index, rewrite.part_index, rewrite.slot),
            (0, 0, "primary_base")
        );
        assert_eq!(rewrite.original, base);

        let mut rendered = messages.clone();
        apply_developer_slots_to_slice(DetectedHarness::Codex, &slots, 0, &mut rendered);
        let forwarded = rendered
            .first()
            .and_then(|msg| msg.parts.first())
            .and_then(|part| match part {
                CanonicalPart::Text(text) => Some(text.as_str()),
                _ => None,
            });
        assert_eq!(rewrite.rewritten.as_deref(), forwarded);
        assert!(
            explain_developer_slot_rewrites(DetectedHarness::Generic, &slots, &messages).is_empty()
        );
    }

    #[test]
    fn strict_tool_detection_never_relies_on_prompt_and_ignores_generic_callers() {
        assert_eq!(detect_harness_by_tools(&[]), None);

        let generic = vec![
            fn_spec("get_weather", "Lookup forecast", json!({"type": "object"})),
            fn_spec("query_db", "Run SQL", json!({"type": "object"})),
        ];
        assert_eq!(detect_harness_by_tools(&generic), None);
        assert_eq!(
            detect_harness(ProtocolHint::AnthropicMessages, &generic),
            DetectedHarness::ClaudeCode
        );

        // Single generic `Read` or `bash` tool without companion harness signatures stays `None`.
        let single_read = vec![fn_spec("Read", "Read file", json!({"type": "object"}))];
        assert_eq!(detect_harness_by_tools(&single_read), None);
        let single_bash = vec![fn_spec("bash", "Run shell", json!({"type": "object"}))];
        assert_eq!(detect_harness_by_tools(&single_bash), None);

        // Read-only Claude Code Explore subagent (`Read` + `Glob` + `Grep`) is positively recognized.
        let claude_explore = vec![
            fn_spec("Read", "Read file", json!({"type": "object"})),
            fn_spec("Glob", "Find files", json!({"type": "object"})),
            fn_spec("Grep", "Search text", json!({"type": "object"})),
        ];
        assert_eq!(
            detect_harness_by_tools(&claude_explore),
            Some(DetectedHarness::ClaudeCode)
        );

        // Generic single-word tool names (`shell`, `execute`, `Bash`, `Agent`) NEVER match alone.
        for lone in [
            fn_spec("shell", "Run command", json!({"type": "object"})),
            fn_spec(
                "shell",
                "Run command",
                json!({"type": "object", "properties": {"command": {"type": "string"}}}),
            ),
            fn_spec(
                "shell",
                "Run command",
                json!({"type": "object", "properties": {"command": {"type": "array", "items": {"type": "string"}}}}),
            ),
            fn_spec("execute", "Execute query", json!({"type": "object"})),
            fn_spec("Bash", "Run bash", json!({"type": "object"})),
            fn_spec("Agent", "Delegate", json!({"type": "object"})),
        ] {
            assert_eq!(
                detect_harness_by_tools(std::slice::from_ref(&lone)),
                None,
                "lone generic tool `{}` must not trigger harness prompt rewriting",
                lone.name
            );
        }

        // Companion tuples disambiguate Codex array `shell` + `apply_patch` vs OpenCode string `shell` + `read` + `grep`.
        let codex_shell_pair = vec![
            fn_spec(
                "shell",
                "Run command",
                json!({"type": "object", "properties": {"command": {"type": "array", "items": {"type": "string"}}}}),
            ),
            fn_spec(
                "apply_patch",
                "Patch file",
                json!({"type": "object", "properties": {"patch": {"type": "string"}}}),
            ),
        ];
        assert_eq!(
            detect_harness_by_tools(&codex_shell_pair),
            Some(DetectedHarness::Codex)
        );
        let opencode_shell_tuple = vec![
            fn_spec(
                "shell",
                "Run command",
                json!({"type": "object", "properties": {"command": {"type": "string"}}}),
            ),
            fn_spec("read", "Read file", json!({"type": "object"})),
            fn_spec("grep", "Search text", json!({"type": "object"})),
        ];
        assert_eq!(
            detect_harness_by_tools(&opencode_shell_tuple),
            Some(DetectedHarness::OpenCode)
        );
    }

    #[test]
    fn distills_version_changed_builtin_prompts_while_preserving_env_and_custom_rules() {
        // Simulate a hypothetical future Claude Code v99 whose prose changed completely
        // and embeds live runtime context tags plus `<permissions instructions>` and Plan `<collaboration_mode>`.
        let raw_v99_block = format!(
            "Welcome to NextGen CLI v99.\n{}\n<permissions instructions>\nsandbox_mode is read-only.\n</permissions instructions>\n<collaboration_mode>\n# Collaboration Mode: Plan\nEmit `<proposed_plan>` block.\n</collaboration_mode>\n<current_time_reminder>2026-11-28T02:05:00+08:00</current_time_reminder>\n<budget:token_budget>200000</budget:token_budget>\n<context_window>500000</context_window>\n<rollout_budget>100</rollout_budget>\n<timestamp>1795802700</timestamp>\n<env>\nWorking directory: /workspace/demo\nPlatform: darwin\n</env>\ngitStatus: On branch main\n<claude_background_info>Model marketing</claude_background_info>",
            "Verbose built-in tool manual paragraph.\n".repeat(120)
        );
        let segments = vec![
            "You are Claude Code, Anthropic's official CLI for Claude.".to_owned(),
            raw_v99_block,
            "Always respond in Simplified Chinese.".to_owned(),
        ];

        let distilled = distill_anthropic_system_prompt(DetectedHarness::ClaudeCode, &segments)
            .expect("distilled Claude Code prompt");
        assert!(!distilled.contains("Verbose built-in tool manual paragraph"));
        assert!(!distilled.contains("Model marketing"));
        assert!(distilled.contains(CLAUDE_CODE_DISTILLED_PROMPT));
        assert!(distilled.contains("sandbox_mode is read-only."));
        assert!(distilled.contains("<proposed_plan>"));
        assert!(
            distilled.contains(
                "<current_time_reminder>2026-11-28T02:05:00+08:00</current_time_reminder>"
            )
        );
        assert!(distilled.contains("<budget:token_budget>200000</budget:token_budget>"));
        assert!(distilled.contains("<context_window>500000</context_window>"));
        assert!(distilled.contains("<rollout_budget>100</rollout_budget>"));
        assert!(distilled.contains("<timestamp>1795802700</timestamp>"));
        assert!(distilled.contains("<env>\nWorking directory: /workspace/demo"));
        assert!(distilled.contains("gitStatus: On branch main"));
        assert!(distilled.contains("Always respond in Simplified Chinese."));
        assert!(distilled.len() < 1_800);

        // Idempotence check.
        assert_eq!(
            distill_harness_base_prompt(DetectedHarness::ClaudeCode, &distilled),
            distilled
        );
    }

    #[test]
    fn preserves_short_specialized_subagent_role_prompts() {
        let subagent_role = "You are a Rust security auditor focusing on unsafe FFI boundaries and integer overflows.";
        let distilled = distill_harness_base_prompt(DetectedHarness::ClaudeCode, subagent_role);
        assert!(distilled.contains(CLAUDE_CODE_DISTILLED_PROMPT));
        assert!(distilled.contains(subagent_role));

        // Trailing `--append-system-prompt` or short primary prompts starting with `"You are Claude Code..."`
        // that carry custom rules must NEVER be discarded as boilerplate banners.
        let custom_trailing = vec![
            "You are Claude Code, Anthropic's official CLI for Claude.".to_owned(),
            format!("Built-in block.\n{}", "Boilerplate.\n".repeat(150)),
            "You are Claude Code. For this repository, respond only in French.".to_owned(),
        ];
        let distilled_trailing =
            distill_anthropic_system_prompt(DetectedHarness::ClaudeCode, &custom_trailing)
                .expect("distilled with identity-prefixed append");
        assert!(distilled_trailing.contains("For this repository, respond only in French."));
    }

    #[test]
    fn compacts_codex_skills_collaboration_and_omp_xml_skills() {
        let codex_dev = format!(
            "<skills_instructions>\n## Skills\nLong intro\n### Skill roots\n- `r0` = `/Users/dev/.agents/skills`\n### Available skills\n- pdf: {} (file: r0/pdf/SKILL.md)\n</skills_instructions>\n<permissions instructions>\nsandbox_mode is danger-full-access. Do not pass sandbox_permissions.\n</permissions instructions>\n<collaboration_mode>\n# Collaboration Mode: Default\n{}\n</collaboration_mode>\n<apps_instructions>\nConnector prose\n</apps_instructions>",
            "Very verbose skill description ".repeat(40),
            "Verbose collaboration prose ".repeat(25)
        );
        let compacted = distill_harness_developer_item(DetectedHarness::Codex, &codex_dev)
            .expect("compacted developer item");
        assert!(compacted.contains("- `r0` = `/Users/dev/.agents/skills`"));
        assert!(compacted.contains("- pdf (file: r0/pdf/SKILL.md)"));
        assert!(!compacted.contains("Very verbose skill description"));
        assert!(compacted.contains("Do not pass sandbox_permissions."));
        assert!(compacted.contains("Collaboration Mode: Default."));
        assert!(!compacted.contains("Verbose collaboration prose"));
        assert!(!compacted.contains("<apps_instructions>"));

        // Plan mode `<collaboration_mode>` is preserved verbatim.
        let plan_dev = "<collaboration_mode>\n# Collaboration Mode: Plan\nEmit `<proposed_plan>` when ready.\n</collaboration_mode>";
        let kept_plan = distill_harness_developer_item(DetectedHarness::Codex, plan_dev)
            .expect("plan mode preserved");
        assert!(kept_plan.contains("<proposed_plan>"));

        // OMP `<skills>` compaction + internal `eval` prelude drop + `<project-context>` preservation.
        let omp_base = format!(
            "You are omp's trusted coding assistant.\n{}\n<skills>\n<skill><name>rust-audit</name><description>{}</description><path>/skills/rust-audit/SKILL.md</path></skill>\n</skills>",
            "OMP built-in engineering & tool manual.\n".repeat(150),
            "Long skill prose ".repeat(30)
        );
        let omp_distilled = distill_harness_base_prompt(DetectedHarness::Omp, &omp_base);
        assert!(omp_distilled.contains(OMP_DISTILLED_PROMPT));
        assert!(omp_distilled.contains("- rust-audit: /skills/rust-audit/SKILL.md"));
        assert!(!omp_distilled.contains("OMP built-in engineering & tool manual"));

        assert_eq!(
            distill_harness_developer_item(
                DetectedHarness::Omp,
                "Binary analysis lives in eval's `dsm` namespace; read `help://eval/dsm` before use."
            ),
            None
        );
        let omp_proj = "<project-context>\n<repo-rules>\nFollow AGENTS.md\n</repo-rules>\n</project-context>\n<critical>Repeat rules</critical>";
        let kept_proj = distill_harness_developer_item(DetectedHarness::Omp, omp_proj)
            .expect("project context");
        assert!(kept_proj.contains("Follow AGENTS.md"));
        assert!(!kept_proj.contains("Repeat rules"));
    }

    #[test]
    fn codex_tui_nested_backtick_collaboration_mode_and_cloud_plugin_skills_compact_cleanly() {
        let codex_tui_dev = "\
<skills_instructions>
## Skills
### Skill roots
- `r0` = `/Users/dev/.agents/skills`
- `r1` = `/Users/dev/.codex/skills/.system`
- `r2` = `/Users/dev/.codex/plugins/cache/openai-curated-remote/work-pets/0.1.6/skills`
- `r3` = `/Users/dev/.codex/plugins/cache/openai-bundled`
- `r4` = `/Users/dev/.codex/plugins/cache/openai-primary-runtime/spreadsheets/26.1123/skills`
### Available skills
- find-skills: Discover agent skills. (file: r0/find-skills/SKILL.md)
- skill-creator: Create or update a skill. (file: r1/skill-creator/SKILL.md)
- work-pets:pets: Manage animated pets in ChatGPT Work mode. (file: r2/pets/SKILL.md)
- visualize: Render interactive widgets. (file: r3/visualize/1.0.0/skills/visualize/SKILL.md)
- spreadsheets: Edit workbooks in ChatGPT runtime. (file: r4/spreadsheets/SKILL.md)
</skills_instructions>
<permissions instructions>
Approval policy is currently never.
</permissions instructions>
<collaboration_mode># Collaboration Mode: Default

Your active mode changes only when new developer instructions with a different `<collaboration_mode>...</collaboration_mode>` change it; user requests or tool descriptions do not change mode by themselves.

## request_user_input availability

Use the `request_user_input` tool only when it is listed in the available tools for this turn.
</collaboration_mode>
<apps_instructions>
Apps prose.
</apps_instructions>";

        let distilled = distill_harness_developer_item(DetectedHarness::Codex, codex_tui_dev)
            .expect("compacted codex developer item");
        assert!(distilled.contains("- `r0` = `/Users/dev/.agents/skills`"));
        assert!(distilled.contains("- `r1` = `/Users/dev/.codex/skills/.system`"));
        assert!(distilled.contains("- find-skills (file: r0/find-skills/SKILL.md)"));
        assert!(distilled.contains("- skill-creator (file: r1/skill-creator/SKILL.md)"));
        assert!(!distilled.contains("r2"));
        assert!(!distilled.contains("r3"));
        assert!(!distilled.contains("r4"));
        assert!(!distilled.contains("work-pets"));
        assert!(!distilled.contains("visualize"));
        assert!(!distilled.contains("spreadsheets"));
        assert!(!distilled.contains("request_user_input availability"));
        assert!(!distilled.contains("`<collaboration_mode>...</collaboration_mode>`"));
        assert_eq!(distilled.matches("</collaboration_mode>").count(), 1);
        assert!(!distilled.contains("<apps_instructions>"));
    }

    #[test]
    fn unclosed_code_fence_inside_instructions_preserves_agents_md() {
        let user_env = "\
# AGENTS.md instructions for /workspace/demo

<INSTRUCTIONS>
# Project Rules
Example snippet with an unclosed fence:
```rust
fn demo() {}
</INSTRUCTIONS>
<environment_context>
  <cwd>/workspace/demo</cwd>
</environment_context>";

        let extracted = extract_all_tag_blocks(user_env, "<INSTRUCTIONS>", "</INSTRUCTIONS>");
        assert_eq!(extracted.len(), 1);
        assert!(extracted[0].contains("# Project Rules"));
        assert!(CODEX_DISTILLED_PROMPT.contains("concise 1-sentence progress commentary"));
        assert!(CODEX_DISTILLED_PROMPT.contains("update_plan"));
    }
}
