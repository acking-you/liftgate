//! Zero-cost [`HarnessToolProfile`] tuned for OpenAI Codex CLI & Codex App (`app-server`).

use std::borrow::Cow;
use std::collections::HashMap;

use liftgate_types::{
    canonical::CanonicalToolSpec,
    session_key::{
        McpOrchestratorContract, is_code_mode_custom_exec, is_code_mode_orchestrator_tool,
        is_codex_exec_orchestrator_tool,
    },
};

use crate::{
    budget::VirtualizerBudget,
    profile::{
        HarnessToolProfile, ToolPlacement, ascii_lowercase_cow, count_fallback_prefix_stems,
        normalize_namespace_slug, split_mcp_server_and_action,
    },
};

/// Zero-sized compile-time profile optimized for OpenAI Codex CLI & Codex App.
///
/// ### Codex-Specific Optimizations
/// 1. **Code Mode (`exec` + `wait`)**: Guaranteed Tier-0 Eager slots; raw JS `exec` unwrapped via
///    `"code"` and non-orchestrator `exec tool declaration:` TypeScript blocks dehydrated out of
///    wire `tools/list` while preserved verbatim in 0-RTT `search_client_tools`.
/// 2. **Core PTY & Workspace Pair**: `exec_command`, `write_stdin`, `local_shell` / `shell`
///    variants, `view_image`, `update_plan`, and `tool_search` stay 1:1 Direct Eager.
/// 3. **Codex Apps & Built-in Families**: Strips `mcp__codex_apps__` cleanly (`ns__github`,
///    `ns__linear`, `ns__slack`), groups `mcp__codex_tui` / `mcp__node_repl` / `mcp__cua_repl`,
///    and unifies both dotted (`multi_agent_v1.*`, `clock.*`) and top-level un-namespaced Codex
///    families (`spawn_agent`..`close_agent`, `request_user_input*`, `get_goal*`, `js_repl*`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodexToolProfile;

const _: () = assert!(
    std::mem::size_of::<CodexToolProfile>() == 0,
    "CodexToolProfile must remain a zero-sized type"
);

impl HarnessToolProfile for CodexToolProfile {
    const ID: &'static str = "codex";
    const BUDGET: VirtualizerBudget = VirtualizerBudget::CODEX_AGY_ACP;
    type Prepass = HashMap<String, usize>;

    fn build_prepass(self, tools: &[CanonicalToolSpec]) -> Self::Prepass {
        count_fallback_prefix_stems(tools)
    }

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
        if name == "wait" || is_code_mode_orchestrator_tool(spec) {
            return ToolPlacement::Orchestrator {
                custom_unwrap_key: None,
            };
        }

        match name {
            "exec_command" => return ToolPlacement::CoreDirect { rank: 0 },
            "write_stdin" => return ToolPlacement::CoreDirect { rank: 1 },
            "apply_patch" => return ToolPlacement::CoreDirect { rank: 2 },
            "__codex_local_shell" | "shell" | "shell_command" | "container.exec" => {
                return ToolPlacement::CoreDirect { rank: 3 };
            }
            "view_image" => return ToolPlacement::CoreDirect { rank: 4 },
            "update_plan" => return ToolPlacement::CoreDirect { rank: 5 },
            "tool_search" | "__codex_tool_search" => {
                return ToolPlacement::CoreDirect { rank: 6 };
            }
            _ => {}
        }

        if let Some(after_mcp) = name.strip_prefix("mcp__") {
            let trimmed = after_mcp
                .strip_prefix("codex_apps__")
                .or_else(|| after_mcp.strip_prefix("claude_ai_"))
                .unwrap_or(after_mcp);
            if let Some((ns_raw, action_raw)) = split_mcp_server_and_action(trimmed) {
                return ToolPlacement::Namespaced {
                    namespace: normalize_namespace_slug(ns_raw),
                    action: Cow::Borrowed(action_raw),
                };
            }
            if let Some((srv, action_raw)) = trimmed.split_once('_')
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

        if let Some((ns_raw, action_raw)) = name.split_once('.')
            && !ns_raw.is_empty()
            && !action_raw.is_empty()
        {
            return ToolPlacement::Namespaced {
                namespace: ascii_lowercase_cow(ns_raw),
                action: Cow::Borrowed(action_raw),
            };
        }

        match name {
            "spawn_agent" | "send_input" | "wait_agent" | "resume_agent" | "close_agent" => {
                return ToolPlacement::Namespaced {
                    namespace: Cow::Borrowed("multi_agent_v1"),
                    action: Cow::Borrowed(name),
                };
            }
            "request_user_input" | "request_user_input_async" | "send_message_to_user_async" => {
                return ToolPlacement::Namespaced {
                    namespace: Cow::Borrowed("user_input"),
                    action: Cow::Borrowed(name),
                };
            }
            "get_goal" | "create_goal" | "update_goal" => {
                return ToolPlacement::Namespaced {
                    namespace: Cow::Borrowed("goals"),
                    action: Cow::Borrowed(name),
                };
            }
            "list_mcp_resources" | "list_mcp_resource_templates" | "read_mcp_resource" => {
                return ToolPlacement::Namespaced {
                    namespace: Cow::Borrowed("mcp_resources"),
                    action: Cow::Borrowed(name),
                };
            }
            "js_repl" | "js_repl_reset" => {
                return ToolPlacement::Namespaced {
                    namespace: Cow::Borrowed("js_repl"),
                    action: Cow::Borrowed(name),
                };
            }
            _ => {}
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

    fn orchestrator_prompt_contract<'a>(
        self,
        spec: &CanonicalToolSpec,
        mcp_name: &'a str,
        _has_folded_tools: bool,
        client_tools: &[CanonicalToolSpec],
    ) -> Option<McpOrchestratorContract<'a>> {
        is_codex_exec_orchestrator_tool(spec).then(|| McpOrchestratorContract::CodexExec {
            mcp_name,
            has_apply_patch: client_tools
                .iter()
                .any(|t| t.name.eq_ignore_ascii_case("apply_patch")),
            has_update_plan: client_tools
                .iter()
                .any(|t| t.name.eq_ignore_ascii_case("update_plan"))
                || spec
                    .description
                    .as_deref()
                    .is_some_and(|d| d.contains("update_plan")),
        })
    }

    fn search_aliases(self, _spec: &CanonicalToolSpec, namespace: Option<&str>) -> &'static str {
        match namespace {
            Some("github") => "pr pull request issue commit branch workflow actions review",
            Some("codex_tui") => "thread worktree fork archive",
            Some("multi_agent_v1") => "subagent agent spawn parallel delegate",
            Some("user_input") => "ask question clarify notify user",
            Some("cua_repl") => "computer desktop browser ui automation",
            Some("node_repl" | "js_repl") => "javascript node repl eval",
            _ => "",
        }
    }
}
