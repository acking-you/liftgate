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

use crate::canonical::{CanonicalMessage, CanonicalPart, CanonicalToolKind, CanonicalToolSpec};
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

/// Compact Codex CLI & Codex App system contract (`< 600B`), delegating tool-routing rules to
/// `<api_gateway_bridge_rules>` while overriding host `file://` and `<appDataDir>/brain` habits.
pub const CODEX_DISTILLED_PROMPT: &str = "\
You are a coding agent running in the Codex CLI. Be precise, safe, and concise.
- Resolve the user's request end-to-end; inspect the workspace directly (`rg`, `rg --files`) rather than guessing, and validate changes with targeted checks.
- Obey scoped `AGENTS.md` files (deeper overrides parent; user/developer instructions take precedence). Never commit or create git branches unless asked.
- Send a brief 1-sentence preamble before grouped tool calls.
- Reply concisely in GitHub-flavored Markdown, wrap identifiers/paths/commands in backticks, and format file references as inline `path/to/file:line` (never emit `file://` URIs, `【F:...】` citations, or host `<appDataDir>/brain` artifacts).";

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

    if trimmed.len() >= BUILTIN_BASE_PROMPT_MIN_BYTES || is_known_builtin_base_preamble(trimmed) {
        let mut sections = vec![base_prompt.to_owned()];
        append_extracted_structured_sections(&mut sections, trimmed);
        return sections.join("\n\n");
    }

    let compacted_custom = compact_auxiliary_developer_tags(trimmed);
    let custom_trimmed = compacted_custom.trim();
    if custom_trimmed.is_empty() || is_short_identity_banner(custom_trimmed) {
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
        .filter(|s| !s.is_empty() && !is_short_identity_banner(s))
        .collect();

    let Some((&first, tail)) = meaningful.split_first() else {
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

/// Distills `<developer_instructions>` blocks inside normalized OpenAI `/v1/responses` messages
/// for a positively identified `harness`.
///
/// Pure and stateless per message part so stateful `previous_response_id` continuations and full
/// stateless replays always produce byte-for-byte identical `CanonicalMessage` histories.
pub fn distill_responses_developer_messages(
    harness: DetectedHarness,
    has_base_instructions_slot: bool,
    is_cold_turn: bool,
    messages: &mut Vec<CanonicalMessage>,
) {
    if distilled_harness_system_prompt(harness).is_none() {
        return;
    }

    let mut consumed_base_slot = false;
    for (msg_idx, msg) in messages.iter_mut().enumerate() {
        let mut next_parts = Vec::with_capacity(msg.parts.len());
        for (part_idx, part) in std::mem::take(&mut msg.parts).into_iter().enumerate() {
            let CanonicalPart::Text(text) = part else {
                next_parts.push(part);
                continue;
            };
            let Some(inner) = unwrap_developer_instructions(&text) else {
                next_parts.push(CanonicalPart::Text(text));
                continue;
            };

            let treat_as_base = !consumed_base_slot
                && msg_idx == 0
                && part_idx == 0
                && (has_base_instructions_slot
                    || (is_cold_turn && looks_like_standalone_base_prompt(inner)));

            if treat_as_base {
                consumed_base_slot = true;
                let distilled = distill_harness_base_prompt(harness, inner);
                if let Some(wrapped) = wrap_developer_instructions_block(&distilled) {
                    next_parts.push(CanonicalPart::Text(wrapped));
                }
            } else if let Some(distilled_item) = distill_harness_developer_item(harness, inner)
                && let Some(wrapped) = wrap_developer_instructions_block(&distilled_item)
            {
                next_parts.push(CanonicalPart::Text(wrapped));
            }
        }
        msg.parts = next_parts;
    }
    messages.retain(|m| !m.parts.is_empty());
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

fn is_short_identity_banner(text: &str) -> bool {
    if text.len() > 240 {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    lower.starts_with("you are claude code")
        || lower.starts_with("you are a claude agent")
        || lower.starts_with("you are a coding agent assisting the user via their cli harness")
        || lower.starts_with("you are opencode")
        || lower.starts_with("you are omp's trusted coding assistant")
}

fn is_known_builtin_base_preamble(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.starts_with("you are a coding agent running in the codex cli")
        || lower.starts_with("you are codex, a coding agent")
        || lower.starts_with("you are claude code")
        || lower.starts_with("you are a claude agent")
        || lower.starts_with("you are opencode")
        || lower.starts_with("you are omp's trusted coding assistant")
}

fn looks_like_standalone_base_prompt(inner: &str) -> bool {
    if is_known_builtin_base_preamble(inner) {
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
    ] {
        for block in extract_all_tag_blocks(raw, open_tag, close_tag) {
            sections.push(block.trim().to_owned());
        }
        outside_containers =
            transform_tag_blocks(&outside_containers, open_tag, close_tag, |_| None);
    }

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

    if let Some(skills_inner) =
        extract_tag_inner(raw, "<skills_instructions>", "</skills_instructions>")
        && let Some(compacted) = compact_codex_skills_instructions(skills_inner)
    {
        sections.push(compacted);
    }
    outside_containers = transform_tag_blocks(
        &outside_containers,
        "<skills_instructions>",
        "</skills_instructions>",
        |_| None,
    );
    if let Some(xml_skills_inner) = extract_tag_inner(raw, "<skills>", "</skills>")
        && let Some(compacted) = compact_xml_skills_block(xml_skills_inner)
    {
        sections.push(compacted);
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

fn compact_codex_skills_instructions(inner: &str) -> Option<String> {
    let mut roots = Vec::new();
    let mut skills = Vec::new();

    for raw_line in inner.lines() {
        let line = raw_line.trim();
        let Some(bullet) = line.strip_prefix("- ") else {
            continue;
        };
        if bullet.starts_with('`') && bullet.contains(" = ") {
            roots.push(line.to_owned());
            continue;
        }
        if let Some((raw_name, _)) = bullet.split_once(':') {
            let name = raw_name.trim();
            if !name.is_empty() {
                if let Some(file_idx) = bullet.rfind("(file:")
                    && let Some(file_ref) = bullet.get(file_idx..)
                {
                    skills.push(format!("- {name} {file_ref}"));
                    continue;
                }
                if let Some(paren_idx) = bullet.rfind('(')
                    && let Some(paren_ref) = bullet.get(paren_idx..)
                    && paren_ref.contains("SKILL.md")
                {
                    skills.push(format!("- {name} {paren_ref}"));
                    continue;
                }
            }
        }
    }

    if !skills.is_empty() {
        let mut body =
            String::from("<skills_instructions>\nRead a skill's `SKILL.md` only when relevant:\n");
        for root in &roots {
            body.push_str(root);
            body.push('\n');
        }
        for skill in &skills {
            body.push_str(skill);
            body.push('\n');
        }
        body.push_str("</skills_instructions>");
        return Some(body);
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
            "<collaboration_mode>Collaboration Mode: Default. Execute directly; ask one concise plain-text question only if blocked (never emit textual multiple-choice lists).</collaboration_mode>",
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

fn extract_all_tag_blocks<'a>(text: &'a str, open_tag: &str, close_tag: &str) -> Vec<&'a str> {
    if !text.contains(open_tag) {
        return Vec::new();
    }
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(open_tag) {
        let Some(after_open_idx) = start.checked_add(open_tag.len()) else {
            break;
        };
        let Some(tail) = rest.get(after_open_idx..) else {
            break;
        };
        let Some(rel_end) = tail.find(close_tag) else {
            break;
        };
        let Some(end) = after_open_idx
            .checked_add(rel_end)
            .and_then(|i| i.checked_add(close_tag.len()))
        else {
            break;
        };
        if let Some(block) = rest.get(start..end) {
            blocks.push(block);
        }
        rest = rest.get(end..).unwrap_or("");
    }
    blocks
}

fn extract_tag_inner<'a>(text: &'a str, open_tag: &str, close_tag: &str) -> Option<&'a str> {
    let start = text.find(open_tag)?;
    let after_open = start.checked_add(open_tag.len())?;
    let tail = text.get(after_open..)?;
    let rel_end = tail.find(close_tag)?;
    tail.get(..rel_end)
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
    while let Some(start) = rest.find(open_tag) {
        if let Some(before) = rest.get(..start) {
            out.push_str(before);
        }
        let Some(after_open_idx) = start.checked_add(open_tag.len()) else {
            break;
        };
        let Some(after_open) = rest.get(after_open_idx..) else {
            break;
        };
        let Some(rel_close) = after_open.find(close_tag) else {
            if let Some(rem) = rest.get(start..) {
                out.push_str(rem);
            }
            return out;
        };
        let Some(inner) = after_open.get(..rel_close) else {
            break;
        };
        if let Some(replacement) = f(inner) {
            out.push_str(&replacement);
        }
        let Some(next_idx) = rel_close.checked_add(close_tag.len()) else {
            break;
        };
        rest = after_open.get(next_idx..).unwrap_or("");
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
        // and embeds live runtime context tags (`<current_time_reminder>`, `<budget:token_budget>`, `<timestamp>`).
        let raw_v99_block = format!(
            "Welcome to NextGen CLI v99.\n{}\n<current_time_reminder>2026-11-28T02:05:00+08:00</current_time_reminder>\n<budget:token_budget>200000</budget:token_budget>\n<context_window>500000</context_window>\n<rollout_budget>100</rollout_budget>\n<timestamp>1795802700</timestamp>\n<env>\nWorking directory: /workspace/demo\nPlatform: darwin\n</env>\ngitStatus: On branch main\n<claude_background_info>Model marketing</claude_background_info>",
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
        assert!(distilled.len() < 1_500);

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
}
