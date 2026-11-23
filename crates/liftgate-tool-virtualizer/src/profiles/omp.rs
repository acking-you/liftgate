//! Zero-cost [`HarnessToolProfile`] specialized for Oh My Pi (`omp` / `@oh-my-pi/pi-coding-agent`).
//!
//! Models `omp`'s complete tool and Code Mode architecture:
//! - Tier 0: Polyglot `eval` (`py` Python + `js` Bun persistent kernels with `tool.<name>(args)`,
//!   `mcp.evalNamespaces` globals, `agent()` / `workpool()`, and `<browser>` / `<computer>` /
//!   `<dsm>` preludes) plus `wait` (background `eval` / `bash` / `task` job barrier);
//! - Tier 1: Essential coding loop tools (`bash` — also the `tools.commands` shell transport,
//!   `read` — also resolves `omp://`, `help://`, `local://`, and `skill://` topics, `edit` /
//!   `apply_patch` / `ast_edit`, `write`, `grep` / `glob` / `find` / `ast_grep`, `task` / `yield`
//!   / `resolve` / `reject` / `propose`, and `lsp`);
//! - Tier 2: Semantic `omp` tool families (`retain` / `recall` / `reflect` / `memory_edit` /
//!   `learn` -> `ns__memory`, `checkpoint` / `rewind` / `context_notes` / `new_context` ->
//!   `ns__session_state`, `irc_*` -> `ns__irc`) and `omp`'s `mcp__<server>_<tool>` single-underscore
//!   MCP bridge tools (`ns__<server>`);
//! - Hashline-safe patch routing: `omp`'s default `edit` tool (`hashline` / `sloppy` / `magic` /
//!   `patch` / `replace`) uses `[path#TAG]` + `PUT` envelopes inside `*** Begin Patch` rather than
//!   OpenAI V4A `*** Update File:` hunks, so `v4a_patch_contract` leaves `edit` untouched while
//!   bridging custom Lark-grammar `edit` over JSON `{ input: string }` losslessly.

use std::borrow::Cow;
use std::collections::HashMap;

use liftgate_types::{
    canonical::{CanonicalToolKind, CanonicalToolSpec},
    session_key::{
        is_code_mode_custom_exec, is_code_mode_orchestrator_tool, is_omp_eval_orchestrator_tool,
    },
};

use crate::{
    budget::VirtualizerBudget,
    profile::{
        HarnessToolProfile, ToolPlacement, V4aPatchContract, default_apply_patch_v4a_contract,
        json_schema_declares_property, normalize_namespace_slug, split_mcp_server_and_action,
    },
};

/// Zero-sized compile-time profile for Oh My Pi (`omp`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OmpToolProfile;

const _: () = assert!(
    std::mem::size_of::<OmpToolProfile>() == 0,
    "OmpToolProfile must remain a zero-sized type"
);

impl HarnessToolProfile for OmpToolProfile {
    const ID: &'static str = "omp";
    const BUDGET: VirtualizerBudget = VirtualizerBudget::OMP_AGY_ACP;

    fn classify_tool<'a>(
        self,
        spec: &'a CanonicalToolSpec,
        fallback_stem_counts: &HashMap<String, usize>,
    ) -> ToolPlacement<'a> {
        let name = spec.name.as_str();

        // Tier 0: `eval` polyglot Code Mode kernel + `wait` job barrier.
        if name == "eval" || is_omp_eval_orchestrator_tool(spec) || is_code_mode_custom_exec(spec) {
            return ToolPlacement::Orchestrator {
                custom_unwrap_key: Some("code"),
            };
        }
        if name == "wait" || is_code_mode_orchestrator_tool(spec) {
            return ToolPlacement::Orchestrator {
                custom_unwrap_key: None,
            };
        }

        // Tier 1: Core coding loop & turn-control tools (`ESSENTIAL_BUILTIN_TOOL_NAMES` +
        // `resolve`/`reject`/`propose`/`yield` soft/forced tool-choice targets).
        match name {
            "bash" => return ToolPlacement::CoreDirect { rank: 0 },
            "read" => return ToolPlacement::CoreDirect { rank: 1 },
            "edit" | "apply_patch" | "ast_edit" => return ToolPlacement::CoreDirect { rank: 2 },
            "write" => return ToolPlacement::CoreDirect { rank: 3 },
            "grep" | "glob" | "find" | "ast_grep" => return ToolPlacement::CoreDirect { rank: 4 },
            "task" | "yield" | "resolve" | "reject" | "propose" => {
                return ToolPlacement::CoreDirect { rank: 5 };
            }
            "lsp" => return ToolPlacement::CoreDirect { rank: 6 },
            _ => {}
        }

        // Tier 2a: Long-term memory family (`mnemopi` / `hindsight` / `autolearn`).
        if matches!(
            name,
            "retain" | "recall" | "reflect" | "memory_edit" | "learn"
        ) {
            if fallback_stem_counts.get("omp_memory").copied().unwrap_or(0) >= 2 {
                return ToolPlacement::Namespaced {
                    namespace: Cow::Borrowed("memory"),
                    action: Cow::Borrowed(name),
                };
            }
            return ToolPlacement::Standalone { backfill_rank: 35 };
        }
        if let Some(action) = name
            .strip_prefix("hindsight_")
            .or_else(|| name.strip_prefix("mnemopi_"))
            .filter(|rest| !rest.is_empty())
        {
            return ToolPlacement::Namespaced {
                namespace: Cow::Borrowed("memory"),
                action: Cow::Borrowed(action),
            };
        }

        // Tier 2b: Session & context-window checkpointing family.
        if matches!(
            name,
            "checkpoint" | "rewind" | "context_notes" | "new_context"
        ) {
            if fallback_stem_counts
                .get("omp_session_state")
                .copied()
                .unwrap_or(0)
                >= 2
            {
                return ToolPlacement::Namespaced {
                    namespace: Cow::Borrowed("session_state"),
                    action: Cow::Borrowed(name),
                };
            }
            return ToolPlacement::Standalone { backfill_rank: 32 };
        }

        // Tier 2c: IRC collaboration tools (`irc_*`).
        if let Some(action) = name.strip_prefix("irc_").filter(|rest| !rest.is_empty()) {
            return ToolPlacement::Namespaced {
                namespace: Cow::Borrowed("irc"),
                action: Cow::Borrowed(action),
            };
        }

        // Tier 2d: OMP `MCPTool` bridge (`mcp__<server>_<tool>` single-underscore convention,
        // plus standard `mcp__<server>__<tool>` / `mcp__<server>.<tool>`).
        if let Some(after_mcp) = name.strip_prefix("mcp__") {
            if let Some((ns_raw, action_raw)) = split_mcp_server_and_action(after_mcp) {
                return ToolPlacement::Namespaced {
                    namespace: normalize_namespace_slug(ns_raw),
                    action: Cow::Borrowed(action_raw),
                };
            }
            if let Some((ns_raw, action_raw)) =
                split_omp_single_underscore_mcp(after_mcp, fallback_stem_counts)
            {
                return ToolPlacement::Namespaced {
                    namespace: normalize_namespace_slug(ns_raw),
                    action: Cow::Borrowed(action_raw),
                };
            }
        }

        // Tier 2e: Dotted custom/extension tools (`<ns>.<action>`).
        if let Some((ns_raw, action_raw)) = name.rsplit_once('.')
            && !ns_raw.is_empty()
            && !action_raw.is_empty()
        {
            return ToolPlacement::Namespaced {
                namespace: normalize_namespace_slug(ns_raw),
                action: Cow::Borrowed(action_raw),
            };
        }

        // Tier 2f: Top-level built-in standalone tools (`TOOL_COMMAND_KEEP_TOP_LEVEL` prioritized
        // first so `todo`, `ask`, and `web_search` backfill ahead of rarely-used tools).
        match name {
            "todo" => return ToolPlacement::Standalone { backfill_rank: 10 },
            "ask" => return ToolPlacement::Standalone { backfill_rank: 12 },
            "web_search" => return ToolPlacement::Standalone { backfill_rank: 15 },
            "goal" | "think" => return ToolPlacement::Standalone { backfill_rank: 18 },
            "debug" => return ToolPlacement::Standalone { backfill_rank: 22 },
            "github" => return ToolPlacement::Standalone { backfill_rank: 25 },
            "manage_skill" | "security_scan" => {
                return ToolPlacement::Standalone { backfill_rank: 30 };
            }
            "generate_image" | "tts" | "browser" | "computer" => {
                return ToolPlacement::Standalone { backfill_rank: 40 };
            }
            _ => {}
        }

        // Tier 2g: Custom `.omp/tools/` `<stem>_<action>` families with >= 2 siblings.
        if let Some((stem, action_raw)) = name.split_once('_')
            && stem.len() >= 2
            && !action_raw.is_empty()
        {
            let stem_lower = stem.to_ascii_lowercase();
            if fallback_stem_counts.get(&stem_lower).copied().unwrap_or(0) >= 2 {
                return ToolPlacement::Namespaced {
                    namespace: normalize_namespace_slug(stem),
                    action: Cow::Borrowed(action_raw),
                };
            }
        }

        ToolPlacement::Standalone { backfill_rank: 100 }
    }

    fn v4a_patch_contract(self, spec: &CanonicalToolSpec) -> Option<V4aPatchContract> {
        // `omp`'s `edit` tool uses `hashline` (`[path#TAG]` + `PUT N.=M:`), `sloppy`, `magic`,
        // `patch`, or `replace` — never validate `"edit"` against OpenAI V4A `*** Update File:`.
        if spec.name == "edit" {
            return None;
        }
        if spec.name == "apply_patch" {
            if let CanonicalToolKind::Function { parameters_json } = &spec.kind
                && json_schema_declares_property(parameters_json, "input")
            {
                return Some(V4aPatchContract::JsonField("input"));
            }
            return default_apply_patch_v4a_contract(spec);
        }
        None
    }

    fn search_aliases(self, spec: &CanonicalToolSpec, namespace: Option<&str>) -> &'static str {
        match namespace {
            Some("memory") => "memory recall retain reflect learn hindsight mnemopi",
            Some("session_state") => "checkpoint rewind context_notes new_context snapshot branch",
            Some("irc") => "irc chat message channel collab peer",
            _ => match spec.name.as_str() {
                "eval" => "codemode code_mode python js bun kernel repl prelude workpool",
                "lsp" => "definition references hover symbols rename diagnostics code intelligence",
                "ast_edit" | "ast_grep" => "ast structural rewrite pattern tree-sitter",
                "debug" => "dap debugger breakpoint stack variables step",
                "ask" => "question user input prompt interactive choice",
                "task" => "subagent delegate parallel worker agent",
                _ => "",
            },
        }
    }
}

/// Splits `omp`'s `mcp__<server>_<tool>` suffix (`<server>_<tool>`) into `(server, action)`,
/// supporting two-token server names (`google_drive`, `brave_search`) when every tool under the
/// first segment shares the second segment and the second segment is not an action verb.
fn split_omp_single_underscore_mcp<'a>(
    after_mcp: &'a str,
    stem_counts: &HashMap<String, usize>,
) -> Option<(&'a str, &'a str)> {
    let (first_seg, after_first) = after_mcp.split_once('_')?;
    if first_seg.len() < 2 || after_first.is_empty() {
        return None;
    }
    let first_count = stem_counts
        .get(&format!("mcp1:{}", first_seg.to_ascii_lowercase()))
        .copied()
        .unwrap_or(0);
    if first_count < 2 {
        return None;
    }
    if !is_known_single_token_mcp_server(first_seg)
        && let Some((second_seg, after_second)) = after_first.split_once('_')
        && second_seg.len() >= 2
        && !after_second.is_empty()
        && !is_common_mcp_action_or_resource(second_seg)
    {
        let two_seg_len = first_seg.len() + 1 + second_seg.len();
        if let Some(two_seg) = after_mcp.get(..two_seg_len) {
            let two_count = stem_counts
                .get(&format!("mcp2:{}", two_seg.to_ascii_lowercase()))
                .copied()
                .unwrap_or(0);
            if two_count == first_count {
                return Some((two_seg, after_second));
            }
        }
    }
    Some((first_seg, after_first))
}

fn is_known_single_token_mcp_server(segment: &str) -> bool {
    matches!(
        segment.to_ascii_lowercase().as_str(),
        "github"
            | "gitlab"
            | "linear"
            | "slack"
            | "notion"
            | "sentry"
            | "jira"
            | "figma"
            | "stripe"
            | "supabase"
            | "postgres"
            | "sqlite"
            | "redis"
            | "docker"
            | "kubernetes"
            | "playwright"
            | "puppeteer"
            | "filesystem"
            | "git"
            | "exa"
            | "firecrawl"
            | "context7"
    )
}

fn is_common_mcp_action_or_resource(segment: &str) -> bool {
    matches!(
        segment.to_ascii_lowercase().as_str(),
        "get"
            | "list"
            | "read"
            | "write"
            | "create"
            | "update"
            | "delete"
            | "remove"
            | "search"
            | "query"
            | "find"
            | "fetch"
            | "run"
            | "exec"
            | "execute"
            | "add"
            | "set"
            | "send"
            | "post"
            | "put"
            | "patch"
            | "open"
            | "close"
            | "start"
            | "stop"
            | "check"
            | "browser"
            | "issue"
            | "issues"
            | "pr"
            | "pull"
            | "repo"
            | "page"
            | "pages"
            | "file"
            | "files"
            | "doc"
            | "docs"
            | "message"
            | "messages"
            | "channel"
            | "channels"
    )
}
