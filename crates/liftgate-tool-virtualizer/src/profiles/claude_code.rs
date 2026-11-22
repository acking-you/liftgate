//! Zero-cost [`HarnessToolProfile`] tuned for Anthropic Claude Code (`v2.1.x+`).

use std::borrow::Cow;
use std::collections::HashMap;

use liftgate_types::{
    canonical::CanonicalToolSpec,
    session_key::{is_code_mode_custom_exec, is_code_mode_orchestrator_tool},
};

use crate::{
    budget::VirtualizerBudget,
    profile::{
        HarnessToolProfile, ToolPlacement, ascii_lowercase_cow, normalize_namespace_slug,
        split_mcp_server_and_action,
    },
};

/// Zero-sized compile-time profile optimized for Anthropic Claude Code.
///
/// ### Claude Code-Specific Optimizations
/// 1. **Core Editing, Search & Subagent Loop**: `REPL` (Tier 0) plus `Bash`, `Read`, `Edit`,
///    `Write`, `MultiEdit`, `Grep`, `Glob`, `LS`, `Agent` / `Task`, `LSP`, `NotebookEdit`, and
///    `ToolSearch` (Tier 1) are guaranteed 1:1 Direct Eager MCP slots with zero Hub indirection.
/// 2. **PascalCase Built-in Families**: Groups Claude Code's underscore-free PascalCase tool
///    clusters into compact inline-`enum` hubs:
///    - `ns__tasks`: `TodoWrite`, `TaskCreate`, `TaskUpdate`, `TaskGet`, `TaskList`, `TaskOutput`, `TaskStop`
///    - `ns__workflow`: `EnterPlanMode`, `ExitPlanMode`, `EnterWorktree`, `ExitWorktree`
///    - `ns__teams`: `TeamCreate`, `TeamDelete`, `SendMessage` (`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS`)
///    - `ns__schedule`: `CronCreate`, `CronDelete`, `CronList`, `ScheduleWakeup`, `Monitor`, `PushNotification`, `RemoteTrigger`
///    - `ns__mcp_resources`: `ListMcpResourcesTool`, `ReadMcpResourceTool`
/// 3. **Plugin & Cloud Connector Deduplication**:
///    - Collapses redundant plugin server names (`mcp__plugin_playwright_playwright__browser_click`
///      -> `ns__playwright` / `browser_click`).
///    - Strips `mcp__claude_ai_` cloud connector prefixes cleanly (`mcp__claude_ai_Linear__list_issues`
///      -> `ns__linear` / `list_issues`).
/// 4. **Standalone Slack-Slot Backfill**: Prioritizes `AskUserQuestion`, `Skill`, `SlashCommand`,
///    and `Config` for automatic promotion to 1:1 Direct Eager MCP slots whenever spare Eager
///    budget remains.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClaudeCodeToolProfile;

const _: () = assert!(
    std::mem::size_of::<ClaudeCodeToolProfile>() == 0,
    "ClaudeCodeToolProfile must remain a zero-sized type"
);

impl HarnessToolProfile for ClaudeCodeToolProfile {
    const ID: &'static str = "claude-code";
    const BUDGET: VirtualizerBudget = VirtualizerBudget::CLAUDE_CODE_AGY_ACP;

    fn classify_tool<'a>(
        self,
        spec: &'a CanonicalToolSpec,
        fallback_stem_counts: &HashMap<String, usize>,
    ) -> ToolPlacement<'a> {
        let name = spec.name.as_str();

        if is_code_mode_custom_exec(spec) {
            return ToolPlacement::Orchestrator {
                custom_unwrap_key: Some("code"),
            };
        }
        if name == "REPL" || is_code_mode_orchestrator_tool(spec) {
            return ToolPlacement::Orchestrator {
                custom_unwrap_key: None,
            };
        }

        match name {
            "Bash" | "PowerShell" => return ToolPlacement::CoreDirect { rank: 0 },
            "Read" => return ToolPlacement::CoreDirect { rank: 1 },
            "Edit" => return ToolPlacement::CoreDirect { rank: 2 },
            "Write" => return ToolPlacement::CoreDirect { rank: 3 },
            "MultiEdit" => return ToolPlacement::CoreDirect { rank: 4 },
            "Grep" => return ToolPlacement::CoreDirect { rank: 5 },
            "Glob" => return ToolPlacement::CoreDirect { rank: 6 },
            "LS" => return ToolPlacement::CoreDirect { rank: 7 },
            "Agent" | "Task" => return ToolPlacement::CoreDirect { rank: 8 },
            "LSP" => return ToolPlacement::CoreDirect { rank: 9 },
            "NotebookEdit" => return ToolPlacement::CoreDirect { rank: 10 },
            "ToolSearch" => return ToolPlacement::CoreDirect { rank: 11 },
            "apply_patch" => return ToolPlacement::CoreDirect { rank: 12 },
            _ => {}
        }

        // Claude Code plugin MCP tools: `mcp__plugin_<plugin>_<server>__<action>`
        if let Some(after_plugin) = name.strip_prefix("mcp__plugin_")
            && let Some((plugin_srv, action_raw)) = split_mcp_server_and_action(after_plugin)
        {
            let deduped = collapse_repeated_plugin_slug(plugin_srv);
            return ToolPlacement::Namespaced {
                namespace: normalize_namespace_slug(deduped),
                action: Cow::Borrowed(action_raw),
            };
        }

        // Claude.ai cloud MCP connectors: `mcp__claude_ai_<Service>__<action>`
        if let Some(after_cloud) = name.strip_prefix("mcp__claude_ai_")
            && let Some((service_raw, action_raw)) = split_mcp_server_and_action(after_cloud)
        {
            return ToolPlacement::Namespaced {
                namespace: normalize_namespace_slug(service_raw),
                action: Cow::Borrowed(action_raw),
            };
        }

        // Standard MCP servers: `mcp__<server>__<action>` or `mcp__<server>.<action>`
        if let Some(after_mcp) = name.strip_prefix("mcp__") {
            if let Some((ns_raw, action_raw)) = split_mcp_server_and_action(after_mcp) {
                return ToolPlacement::Namespaced {
                    namespace: normalize_namespace_slug(ns_raw),
                    action: Cow::Borrowed(action_raw),
                };
            }
            if let Some((srv, action_raw)) = after_mcp.split_once('_')
                && !srv.is_empty()
                && !action_raw.is_empty()
                && fallback_stem_counts
                    .get(&format!("mcp1:{}", srv.to_ascii_lowercase()))
                    .copied()
                    .unwrap_or(0)
                    >= 2
            {
                return ToolPlacement::Namespaced {
                    namespace: normalize_namespace_slug(srv),
                    action: Cow::Borrowed(action_raw),
                };
            }
        }

        // Claude Code PascalCase built-in families
        match name {
            "TodoWrite" | "TaskCreate" | "TaskUpdate" | "TaskGet" | "TaskList" | "TaskOutput"
            | "TaskStop" | "BashOutput" | "KillShell" => {
                return ToolPlacement::Namespaced {
                    namespace: Cow::Borrowed("tasks"),
                    action: Cow::Borrowed(name),
                };
            }
            "EnterPlanMode" | "ExitPlanMode" | "EnterWorktree" | "ExitWorktree" => {
                return ToolPlacement::Namespaced {
                    namespace: Cow::Borrowed("workflow"),
                    action: Cow::Borrowed(name),
                };
            }
            "TeamCreate" | "TeamDelete" | "SendMessage" => {
                return ToolPlacement::Namespaced {
                    namespace: Cow::Borrowed("teams"),
                    action: Cow::Borrowed(name),
                };
            }
            "CronCreate" | "CronDelete" | "CronList" | "ScheduleWakeup" | "Monitor"
            | "PushNotification" | "RemoteTrigger" => {
                return ToolPlacement::Namespaced {
                    namespace: Cow::Borrowed("schedule"),
                    action: Cow::Borrowed(name),
                };
            }
            "ListMcpResourcesTool" | "ReadMcpResourceTool" => {
                return ToolPlacement::Namespaced {
                    namespace: Cow::Borrowed("mcp_resources"),
                    action: Cow::Borrowed(name),
                };
            }
            "AskUserQuestion" => return ToolPlacement::Standalone { backfill_rank: 0 },
            "Skill" => return ToolPlacement::Standalone { backfill_rank: 1 },
            "SlashCommand" => return ToolPlacement::Standalone { backfill_rank: 2 },
            "Config" => return ToolPlacement::Standalone { backfill_rank: 3 },
            "WebSearch" => return ToolPlacement::Standalone { backfill_rank: 4 },
            "WebFetch" => return ToolPlacement::Standalone { backfill_rank: 5 },
            _ => {}
        }

        if let Some((ns_raw, action_raw)) = name.split_once('.')
            && !ns_raw.is_empty()
            && !action_raw.is_empty()
        {
            return ToolPlacement::Namespaced {
                namespace: ascii_lowercase_cow(ns_raw),
                action: Cow::Borrowed(action_raw),
            };
        }

        if let Some((stem, rest)) = name.split_once('_')
            && stem.len() >= 3
            && !rest.is_empty()
        {
            let key = stem.to_ascii_lowercase();
            if fallback_stem_counts.get(&key).copied().unwrap_or(0) >= 3 {
                return ToolPlacement::Namespaced {
                    namespace: Cow::Owned(key),
                    action: Cow::Borrowed(name),
                };
            }
        }

        ToolPlacement::Standalone { backfill_rank: 16 }
    }

    fn search_aliases(self, _spec: &CanonicalToolSpec, namespace: Option<&str>) -> &'static str {
        match namespace {
            Some("tasks") => "todo task checklist background output stop progress",
            Some("workflow") => "plan mode worktree branch isolation",
            Some("teams") => "agent team swarm message teammate collaborate",
            Some("schedule") => "cron timer wakeup loop monitor trigger",
            Some("mcp_resources") => "mcp resource uri template read list",
            _ => "",
        }
    }
}

/// Collapses `<slug>_<slug>` plugin-server pairs (such as `playwright_playwright` or
/// `github_github`) into `<slug>` while leaving distinct `<plugin>_<server>` names intact.
fn collapse_repeated_plugin_slug(raw: &str) -> &str {
    if raw.len().is_multiple_of(2) {
        return raw;
    }
    let mid = raw.len() / 2;
    if let Some((left, right_with_sep)) = raw.split_at_checked(mid)
        && let Some(right) = right_with_sep.strip_prefix('_')
        && left.eq_ignore_ascii_case(right)
        && !left.is_empty()
    {
        return left;
    }
    raw
}
