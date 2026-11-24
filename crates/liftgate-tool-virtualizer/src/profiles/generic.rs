//! Zero-cost fallback [`HarnessToolProfile`] for arbitrary OpenAI / Anthropic / custom harnesses.

use std::borrow::Cow;
use std::collections::HashMap;

use liftgate_types::{
    canonical::CanonicalToolSpec,
    session_key::{is_code_mode_custom_exec, is_code_mode_orchestrator_tool},
};

use crate::{
    budget::VirtualizerBudget,
    profile::{
        HarnessToolProfile, ToolPlacement, ascii_lowercase_cow, count_fallback_prefix_stems,
        normalize_namespace_slug, split_mcp_server_and_action,
    },
};

/// Zero-sized fallback profile supporting standard MCP prefixes (`mcp__<server>__<tool>`,
/// `mcp__<server>.<tool>`, repeated `mcp__<server>_<tool>`), dotted namespaces, and
/// case-insensitive core coding primitives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GenericToolProfile;

const _: () = assert!(
    std::mem::size_of::<GenericToolProfile>() == 0,
    "GenericToolProfile must remain a zero-sized type"
);

impl HarnessToolProfile for GenericToolProfile {
    const ID: &'static str = "generic";
    const BUDGET: VirtualizerBudget = VirtualizerBudget::DEFAULT_AGY_ACP;
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
        if matches!(name, "wait" | "REPL") || is_code_mode_orchestrator_tool(spec) {
            return ToolPlacement::Orchestrator {
                custom_unwrap_key: None,
            };
        }

        if name.eq_ignore_ascii_case("bash")
            || name.eq_ignore_ascii_case("exec_command")
            || name.eq_ignore_ascii_case("shell")
        {
            return ToolPlacement::CoreDirect { rank: 0 };
        }
        if name.eq_ignore_ascii_case("write_stdin") {
            return ToolPlacement::CoreDirect { rank: 1 };
        }
        if name.eq_ignore_ascii_case("read")
            || name.eq_ignore_ascii_case("edit")
            || name.eq_ignore_ascii_case("write")
            || name.eq_ignore_ascii_case("multiedit")
            || name.eq_ignore_ascii_case("apply_patch")
        {
            return ToolPlacement::CoreDirect { rank: 2 };
        }
        if name.eq_ignore_ascii_case("grep")
            || name.eq_ignore_ascii_case("glob")
            || name.eq_ignore_ascii_case("find")
            || name.eq_ignore_ascii_case("ls")
            || name.eq_ignore_ascii_case("lsp")
            || name.eq_ignore_ascii_case("python")
        {
            return ToolPlacement::CoreDirect { rank: 3 };
        }
        if name.eq_ignore_ascii_case("agent")
            || name.eq_ignore_ascii_case("task")
            || name.eq_ignore_ascii_case("view_image")
            || name.eq_ignore_ascii_case("tool_search")
            || name.eq_ignore_ascii_case("toolsearch")
        {
            return ToolPlacement::CoreDirect { rank: 4 };
        }

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
}
