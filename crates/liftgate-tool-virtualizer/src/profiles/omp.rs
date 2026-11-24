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
        McpOrchestratorContract, is_code_mode_custom_exec, is_code_mode_orchestrator_tool,
        is_omp_eval_orchestrator_tool,
    },
};

use crate::{
    budget::VirtualizerBudget,
    profile::{
        HarnessToolProfile, ToolPlacement, V4aPatchContract, default_apply_patch_v4a_contract,
        json_schema_declares_property, normalize_namespace_slug, split_mcp_server_and_action,
    },
};

/// Typed catalog prepass built exclusively for [`OmpToolProfile`], sharing one family classifier
/// between inventory counting and final tier placement.
#[derive(Clone, Debug, Default)]
pub struct OmpCatalogPrepass {
    memory_builtins: usize,
    session_state_builtins: usize,
    mcp_first_seg: HashMap<String, usize>,
    mcp_two_seg: HashMap<String, usize>,
    custom_stems: HashMap<String, usize>,
}

/// Single source of truth for OMP tool-name family recognition across counting and placement.
enum OmpNameFamily<'a> {
    WaitBarrier,
    CoreDirect {
        rank: u8,
    },
    MemoryBuiltin {
        action: &'a str,
    },
    MemoryPrefixed {
        action: &'a str,
    },
    SessionStateBuiltin {
        action: &'a str,
    },
    Irc {
        action: &'a str,
    },
    McpDelimited {
        server: &'a str,
        action: &'a str,
    },
    McpSingleUnderscore {
        first_seg: &'a str,
        after_first: &'a str,
        two_seg_candidate: Option<(&'a str, &'a str)>,
    },
    Dotted {
        namespace: &'a str,
        action: &'a str,
    },
    BuiltinStandalone {
        backfill_rank: u8,
    },
    CustomUnderscore {
        stem: &'a str,
        action: &'a str,
    },
    OtherStandalone,
}

fn classify_omp_name_family(name: &str) -> OmpNameFamily<'_> {
    if name == "wait" {
        return OmpNameFamily::WaitBarrier;
    }
    match name {
        "bash" => return OmpNameFamily::CoreDirect { rank: 0 },
        "read" => return OmpNameFamily::CoreDirect { rank: 1 },
        "edit" | "apply_patch" | "ast_edit" => return OmpNameFamily::CoreDirect { rank: 2 },
        "write" => return OmpNameFamily::CoreDirect { rank: 3 },
        "grep" | "glob" | "find" | "ast_grep" => return OmpNameFamily::CoreDirect { rank: 4 },
        "task" | "yield" | "resolve" | "reject" | "propose" => {
            return OmpNameFamily::CoreDirect { rank: 5 };
        }
        "lsp" => return OmpNameFamily::CoreDirect { rank: 6 },
        _ => {}
    }

    if matches!(
        name,
        "retain" | "recall" | "reflect" | "memory_edit" | "learn"
    ) {
        return OmpNameFamily::MemoryBuiltin { action: name };
    }
    if let Some(action) = name
        .strip_prefix("hindsight_")
        .or_else(|| name.strip_prefix("mnemopi_"))
        .filter(|rest| !rest.is_empty())
    {
        return OmpNameFamily::MemoryPrefixed { action };
    }

    if matches!(
        name,
        "checkpoint" | "rewind" | "context_notes" | "new_context"
    ) {
        return OmpNameFamily::SessionStateBuiltin { action: name };
    }

    if let Some(action) = name.strip_prefix("irc_").filter(|rest| !rest.is_empty()) {
        return OmpNameFamily::Irc { action };
    }

    if let Some(after_mcp) = name.strip_prefix("mcp__") {
        if let Some((server, action)) = split_mcp_server_and_action(after_mcp) {
            return OmpNameFamily::McpDelimited { server, action };
        }
        if let Some((first_seg, after_first)) = after_mcp.split_once('_')
            && first_seg.len() >= 2
            && !after_first.is_empty()
        {
            let two_seg_candidate = if !is_known_single_token_mcp_server(first_seg)
                && let Some((second_seg, after_second)) = after_first.split_once('_')
                && second_seg.len() >= 2
                && !after_second.is_empty()
                && !is_common_mcp_action_or_resource(second_seg)
                && let Some(two_seg) = after_mcp.get(..first_seg.len() + 1 + second_seg.len())
            {
                Some((two_seg, after_second))
            } else {
                None
            };
            return OmpNameFamily::McpSingleUnderscore {
                first_seg,
                after_first,
                two_seg_candidate,
            };
        }
    }

    if let Some((namespace, action)) = name.rsplit_once('.')
        && !namespace.is_empty()
        && !action.is_empty()
    {
        return OmpNameFamily::Dotted { namespace, action };
    }

    match name {
        "todo" => return OmpNameFamily::BuiltinStandalone { backfill_rank: 10 },
        "ask" => return OmpNameFamily::BuiltinStandalone { backfill_rank: 12 },
        "web_search" => return OmpNameFamily::BuiltinStandalone { backfill_rank: 15 },
        "goal" | "think" => return OmpNameFamily::BuiltinStandalone { backfill_rank: 18 },
        "debug" => return OmpNameFamily::BuiltinStandalone { backfill_rank: 22 },
        "github" => return OmpNameFamily::BuiltinStandalone { backfill_rank: 25 },
        "manage_skill" | "security_scan" => {
            return OmpNameFamily::BuiltinStandalone { backfill_rank: 30 };
        }
        "generate_image" | "tts" | "browser" | "computer" => {
            return OmpNameFamily::BuiltinStandalone { backfill_rank: 40 };
        }
        _ => {}
    }

    if let Some((stem, action)) = name.split_once('_')
        && stem.len() >= 2
        && !action.is_empty()
    {
        return OmpNameFamily::CustomUnderscore { stem, action };
    }

    OmpNameFamily::OtherStandalone
}

impl OmpCatalogPrepass {
    #[must_use]
    pub fn from_tools(tools: &[CanonicalToolSpec]) -> Self {
        let mut prepass = Self::default();
        for spec in tools {
            if is_omp_eval_orchestrator_tool(spec) || is_code_mode_custom_exec(spec) {
                continue;
            }
            match classify_omp_name_family(&spec.name) {
                OmpNameFamily::MemoryBuiltin { .. } => {
                    prepass.memory_builtins = prepass.memory_builtins.saturating_add(1);
                }
                OmpNameFamily::SessionStateBuiltin { .. } => {
                    prepass.session_state_builtins =
                        prepass.session_state_builtins.saturating_add(1);
                }
                OmpNameFamily::McpSingleUnderscore {
                    first_seg,
                    two_seg_candidate,
                    ..
                } => {
                    *prepass
                        .mcp_first_seg
                        .entry(first_seg.to_ascii_lowercase())
                        .or_insert(0) += 1;
                    if let Some((two_seg, _)) = two_seg_candidate {
                        *prepass
                            .mcp_two_seg
                            .entry(two_seg.to_ascii_lowercase())
                            .or_insert(0) += 1;
                    }
                }
                OmpNameFamily::CustomUnderscore { stem, .. } => {
                    *prepass
                        .custom_stems
                        .entry(stem.to_ascii_lowercase())
                        .or_insert(0) += 1;
                }
                OmpNameFamily::WaitBarrier
                | OmpNameFamily::CoreDirect { .. }
                | OmpNameFamily::MemoryPrefixed { .. }
                | OmpNameFamily::Irc { .. }
                | OmpNameFamily::McpDelimited { .. }
                | OmpNameFamily::Dotted { .. }
                | OmpNameFamily::BuiltinStandalone { .. }
                | OmpNameFamily::OtherStandalone => {}
            }
        }
        prepass
    }
}

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
    type Prepass = OmpCatalogPrepass;

    fn build_prepass(self, tools: &[CanonicalToolSpec]) -> Self::Prepass {
        OmpCatalogPrepass::from_tools(tools)
    }

    fn classify_tool<'a>(
        self,
        spec: &'a CanonicalToolSpec,
        prepass: &Self::Prepass,
    ) -> ToolPlacement<'a> {
        // Tier 0: `eval` polyglot Code Mode kernel + `wait` job barrier.
        if is_omp_eval_orchestrator_tool(spec) || is_code_mode_custom_exec(spec) {
            return ToolPlacement::Orchestrator {
                custom_unwrap_key: Some("code"),
            };
        }
        if is_code_mode_orchestrator_tool(spec) {
            return ToolPlacement::Orchestrator {
                custom_unwrap_key: None,
            };
        }

        match classify_omp_name_family(&spec.name) {
            OmpNameFamily::WaitBarrier => ToolPlacement::Orchestrator {
                custom_unwrap_key: None,
            },
            OmpNameFamily::CoreDirect { rank } => ToolPlacement::CoreDirect { rank },
            OmpNameFamily::MemoryBuiltin { action } => {
                if prepass.memory_builtins >= 2 {
                    ToolPlacement::Namespaced {
                        namespace: Cow::Borrowed("memory"),
                        action: Cow::Borrowed(action),
                    }
                } else {
                    ToolPlacement::Standalone { backfill_rank: 35 }
                }
            }
            OmpNameFamily::MemoryPrefixed { action } => ToolPlacement::Namespaced {
                namespace: Cow::Borrowed("memory"),
                action: Cow::Borrowed(action),
            },
            OmpNameFamily::SessionStateBuiltin { action } => {
                if prepass.session_state_builtins >= 2 {
                    ToolPlacement::Namespaced {
                        namespace: Cow::Borrowed("session_state"),
                        action: Cow::Borrowed(action),
                    }
                } else {
                    ToolPlacement::Standalone { backfill_rank: 32 }
                }
            }
            OmpNameFamily::Irc { action } => ToolPlacement::Namespaced {
                namespace: Cow::Borrowed("irc"),
                action: Cow::Borrowed(action),
            },
            OmpNameFamily::McpDelimited { server, action } => ToolPlacement::Namespaced {
                namespace: normalize_namespace_slug(server),
                action: Cow::Borrowed(action),
            },
            OmpNameFamily::McpSingleUnderscore {
                first_seg,
                after_first,
                two_seg_candidate,
            } => {
                let first_count = prepass
                    .mcp_first_seg
                    .get(&first_seg.to_ascii_lowercase())
                    .copied()
                    .unwrap_or(0);
                if first_count < 2 {
                    return ToolPlacement::Standalone { backfill_rank: 100 };
                }
                if let Some((two_seg, after_second)) = two_seg_candidate {
                    let two_count = prepass
                        .mcp_two_seg
                        .get(&two_seg.to_ascii_lowercase())
                        .copied()
                        .unwrap_or(0);
                    if two_count == first_count {
                        return ToolPlacement::Namespaced {
                            namespace: normalize_namespace_slug(two_seg),
                            action: Cow::Borrowed(after_second),
                        };
                    }
                }
                ToolPlacement::Namespaced {
                    namespace: normalize_namespace_slug(first_seg),
                    action: Cow::Borrowed(after_first),
                }
            }
            OmpNameFamily::Dotted { namespace, action } => ToolPlacement::Namespaced {
                namespace: normalize_namespace_slug(namespace),
                action: Cow::Borrowed(action),
            },
            OmpNameFamily::BuiltinStandalone { backfill_rank } => {
                ToolPlacement::Standalone { backfill_rank }
            }
            OmpNameFamily::CustomUnderscore { stem, action } => {
                if prepass
                    .custom_stems
                    .get(&stem.to_ascii_lowercase())
                    .copied()
                    .unwrap_or(0)
                    >= 2
                {
                    ToolPlacement::Namespaced {
                        namespace: normalize_namespace_slug(stem),
                        action: Cow::Borrowed(action),
                    }
                } else {
                    ToolPlacement::Standalone { backfill_rank: 100 }
                }
            }
            OmpNameFamily::OtherStandalone => ToolPlacement::Standalone { backfill_rank: 100 },
        }
    }

    fn orchestrator_prompt_contract<'a>(
        self,
        spec: &CanonicalToolSpec,
        mcp_name: &'a str,
    ) -> Option<McpOrchestratorContract<'a>> {
        if is_omp_eval_orchestrator_tool(spec) {
            Some(McpOrchestratorContract::OmpPolyglotEval { mcp_name })
        } else {
            None
        }
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
