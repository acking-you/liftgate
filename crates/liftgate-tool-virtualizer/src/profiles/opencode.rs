//! Zero-cost [`HarnessToolProfile`] specialized for OpenCode (`opencode` v1 & v2).
//!
//! Handles OpenCode's specific tool ecosystem:
//! - Tier 0: `@opencode/codemode` `execute` orchestrator (both JSON `{ code, timeout?, background? }`
//!   over `/v1/messages` and freeform `Custom` text over `/v1/responses`);
//! - Tier 1: Lowercase core coding primitives (`shell` / `bash`, `read`, `edit` / `multiedit` /
//!   `patch` / `apply_patch`, `write`, `grep` / `glob` / `ls` / `list`, `subagent` / `task` /
//!   `batch`, `lsp` / `codesearch`);
//! - Tier 2: Built-in OpenCode namespace families (`opencode_session_*` / `session_*` -> `ns__session`,
//!   `opencode_*` / `artifact_*` -> `ns__opencode`, `automation_*` -> `ns__automation`,
//!   `machine_*` -> `ns__machine`, `todowrite`/`todoread` -> `ns__todo`) and MCP server tools
//!   registered under OpenCode's `<server>_<tool>` naming (`McpTool.name`);
//! - Slack-slot backfill priorities for standalone tools (`question`, `skill`, `websearch`,
//!   `webfetch`, `generate_image`, `generate_video`).

use std::borrow::Cow;
use std::collections::HashMap;

use liftgate_types::{
    canonical::{CanonicalToolKind, CanonicalToolSpec},
    session_key::{
        McpOrchestratorContract, is_code_mode_custom_exec, is_code_mode_orchestrator_tool,
        is_opencode_execute_orchestrator_tool, is_opencode_full_code_mode_exec,
    },
};

use crate::{
    budget::VirtualizerBudget,
    profile::{
        HarnessToolProfile, ToolPlacement, V4aPatchContract, count_fallback_prefix_stems,
        default_apply_patch_v4a_contract, is_opencode_v4a_patch_spec,
        json_schema_declares_property, normalize_namespace_slug, split_mcp_server_and_action,
    },
};

/// Zero-sized compile-time profile for OpenCode CLI / TUI / Web (`opencode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenCodeToolProfile;

const _: () = assert!(
    std::mem::size_of::<OpenCodeToolProfile>() == 0,
    "OpenCodeToolProfile must remain a zero-sized type"
);

impl HarnessToolProfile for OpenCodeToolProfile {
    const ID: &'static str = "opencode";
    const BUDGET: VirtualizerBudget = VirtualizerBudget::OPENCODE_AGY_ACP;
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

        // Tier 0: `@opencode/codemode` (`execute`) and any generic Code Mode orchestrator
        if name == "execute" || is_code_mode_custom_exec(spec) {
            return ToolPlacement::Orchestrator {
                custom_unwrap_key: Some("code"),
            };
        }
        if is_code_mode_orchestrator_tool(spec) {
            return ToolPlacement::Orchestrator {
                custom_unwrap_key: None,
            };
        }

        // Tier 1: Core coding loop tools across OpenCode v2 (`shell`, `patch`, `subagent`)
        // and OpenCode v1 (`bash`, `multiedit`, `apply_patch`, `task`, `list`, `lsp`).
        match name {
            "shell" | "bash" => return ToolPlacement::CoreDirect { rank: 0 },
            "read" => return ToolPlacement::CoreDirect { rank: 1 },
            "edit" | "multiedit" | "patch" | "apply_patch" => {
                return ToolPlacement::CoreDirect { rank: 2 };
            }
            "write" => return ToolPlacement::CoreDirect { rank: 3 },
            "grep" | "glob" | "ls" | "list" => return ToolPlacement::CoreDirect { rank: 4 },
            "subagent" | "task" | "batch" => return ToolPlacement::CoreDirect { rank: 5 },
            "lsp" | "codesearch" => return ToolPlacement::CoreDirect { rank: 6 },
            _ => {}
        }

        // Tier 2a: OpenCode built-in namespace families (exposed directly when `--no-code-mode`
        // or `execute: deny` is active).
        if let Some(action) = name
            .strip_prefix("opencode_session_")
            .or_else(|| name.strip_prefix("session_"))
            .filter(|rest| !rest.is_empty())
        {
            return ToolPlacement::Namespaced {
                namespace: Cow::Borrowed("session"),
                action: Cow::Borrowed(action),
            };
        }
        if let Some(action) = name
            .strip_prefix("opencode_")
            .or_else(|| name.strip_prefix("artifact_"))
            .filter(|rest| !rest.is_empty())
        {
            return ToolPlacement::Namespaced {
                namespace: Cow::Borrowed("opencode"),
                action: Cow::Borrowed(action),
            };
        }
        if let Some(action) = name
            .strip_prefix("automation_")
            .filter(|rest| !rest.is_empty())
        {
            return ToolPlacement::Namespaced {
                namespace: Cow::Borrowed("automation"),
                action: Cow::Borrowed(action),
            };
        }
        if let Some(action) = name
            .strip_prefix("machine_")
            .filter(|rest| !rest.is_empty())
        {
            return ToolPlacement::Namespaced {
                namespace: Cow::Borrowed("machine"),
                action: Cow::Borrowed(action),
            };
        }
        if matches!(name, "todowrite" | "todoread") {
            let has_both_todo = fallback_stem_counts.get("todo_pair").copied().unwrap_or(0) >= 2;
            if has_both_todo {
                return ToolPlacement::Namespaced {
                    namespace: Cow::Borrowed("todo"),
                    action: Cow::Borrowed(name),
                };
            }
            return ToolPlacement::Standalone { backfill_rank: 2 };
        }

        // Tier 2b: Explicit `mcp__<server>__<tool>` prefixes if emitted by bridges/plugins.
        if let Some(after_mcp) = name.strip_prefix("mcp__") {
            if let Some((ns_raw, action_raw)) = split_mcp_server_and_action(after_mcp) {
                return ToolPlacement::Namespaced {
                    namespace: normalize_namespace_slug(ns_raw),
                    action: Cow::Borrowed(action_raw),
                };
            }
            if let Some((srv, action_raw)) = after_mcp.split_once('_')
                && srv.len() >= 2
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

        // Tier 2c: Dotted `<namespace>.<action>` tools (`opencode.session.search`, etc.).
        if let Some((ns_raw, action_raw)) = name.rsplit_once('.')
            && !ns_raw.is_empty()
            && !action_raw.is_empty()
        {
            let ns = ns_raw.strip_prefix("opencode.").unwrap_or(ns_raw);
            return ToolPlacement::Namespaced {
                namespace: normalize_namespace_slug(ns),
                action: Cow::Borrowed(action_raw),
            };
        }

        // High-value OpenCode standalone tools prioritized for slack-slot backfill before
        // `<server>_<tool>` prefix clustering so `generate_image` / `generate_video` stay intact.
        match name {
            "question" => return ToolPlacement::Standalone { backfill_rank: 0 },
            "skill" => return ToolPlacement::Standalone { backfill_rank: 1 },
            "websearch" => return ToolPlacement::Standalone { backfill_rank: 3 },
            "webfetch" => return ToolPlacement::Standalone { backfill_rank: 4 },
            "generate_image"
            | "experimental_image_generate"
            | "generate_video"
            | "experimental_video_generate" => {
                return ToolPlacement::Standalone { backfill_rank: 5 };
            }
            _ => {}
        }

        // Tier 2d: OpenCode MCP server tools registered via `McpTool.name(server, tool)` as
        // `<server>_<tool>` when direct MCP mode is active (`>= 2` tools sharing `<server>_`).
        if let Some((stem, action_raw)) = name.split_once('_')
            && stem.len() >= 2
            && !action_raw.is_empty()
        {
            let key = stem.to_ascii_lowercase();
            if fallback_stem_counts.get(&key).copied().unwrap_or(0) >= 2 {
                return ToolPlacement::Namespaced {
                    namespace: normalize_namespace_slug(stem),
                    action: Cow::Borrowed(action_raw),
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
    ) -> Option<McpOrchestratorContract<'a>> {
        is_opencode_execute_orchestrator_tool(spec).then(|| {
            McpOrchestratorContract::OpenCodeExecute {
                mcp_name,
                full_code_mode: is_opencode_full_code_mode_exec(spec),
            }
        })
    }

    fn search_aliases(self, _spec: &CanonicalToolSpec, namespace: Option<&str>) -> &'static str {
        match namespace {
            Some("session") => "session history search transcript messages opencode",
            Some("opencode") => "artifact resource mcp events workspace opencode",
            Some("automation") => "automation schedule workflow trigger cron",
            Some("machine") => "machine remote ssh host target",
            Some("todo") => "todo checklist tasks progress plan",
            Some("playwright" | "browser" | "chrome") => {
                "browser page click navigate screenshot dom"
            }
            Some("github" | "gh") => "git repo pull request pr issue commit review",
            Some("linear") => "ticket issue sprint project roadmap",
            _ => "",
        }
    }

    fn v4a_patch_contract(self, spec: &CanonicalToolSpec) -> Option<V4aPatchContract> {
        if let Some(contract) = default_apply_patch_v4a_contract(spec) {
            return Some(contract);
        }
        if !is_opencode_v4a_patch_spec(spec) {
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
}
