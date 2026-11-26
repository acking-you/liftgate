//! Monomorphized 3-tier MCP virtualization planner with slack-slot backfill.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use liftgate_types::{
    canonical::{CanonicalToolKind, CanonicalToolSpec},
    session_key::{McpOrchestratorContract, TruncationSpillSink, compact_mcp_tool_description},
};
use serde_json::{Value, json};

use crate::{
    CALL_CLIENT_TOOL_MCP_NAME, GATEWAY_WEB_FETCH_MCP_NAME, NAMESPACE_HUB_MCP_PREFIX,
    SEARCH_CLIENT_TOOLS_MCP_NAME,
    profile::{
        DetectedHarness, HarnessToolProfile, ProtocolHint, ToolPlacement, V4aPatchContract,
        detect_harness,
    },
    profiles::{
        ClaudeCodeToolProfile, CodexToolProfile, GenericToolProfile, OmpToolProfile,
        OpenCodeToolProfile,
    },
    router::McpRoutingTable,
    schema::{
        McpBridgedTool, McpWireTool, code_mode_exec_input_schema, estimate_wire_tool_bytes,
        gateway_web_fetch_input_schema, normalize_mcp_object_schema, sanitize_mcp_tool_name,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum OwnedOrchestratorContract {
    OmpPolyglotEval {
        mcp_name: String,
        elided_subordinate_tools: bool,
    },
}

impl OwnedOrchestratorContract {
    fn from_borrowed(contract: McpOrchestratorContract<'_>) -> Self {
        match contract {
            McpOrchestratorContract::OmpPolyglotEval {
                mcp_name,
                elided_subordinate_tools,
            } => Self::OmpPolyglotEval {
                mcp_name: mcp_name.to_owned(),
                elided_subordinate_tools,
            },
        }
    }

    fn as_borrowed(&self) -> McpOrchestratorContract<'_> {
        match self {
            Self::OmpPolyglotEval {
                mcp_name,
                elided_subordinate_tools,
            } => McpOrchestratorContract::OmpPolyglotEval {
                mcp_name,
                elided_subordinate_tools: *elided_subordinate_tools,
            },
        }
    }
}

/// Partitioned client tool catalog ready for `session/new` registration and `McpSessionHandle`.
#[derive(Clone, Debug, Default)]
pub struct McpSessionCatalog {
    pub profile_id: &'static str,
    pub wire_tools: Arc<[McpWireTool]>,
    pub by_mcp_name: HashMap<String, McpBridgedTool>,
    /// `(client_name, mcp_name)` pairs in caller declaration order for `<api_gateway_bridge_rules>`.
    pub bridged_pairs: Vec<(String, String)>,
    /// `(hub_mcp_name, namespace_label, action_count)` summaries when namespace virtualization is active.
    pub virtualized_hubs: Vec<(String, String, usize)>,
    /// Whether `search_client_tools` and `call_client_tool` are mounted on `lg`.
    pub has_virtualized_dispatch: bool,
    /// Whether server-managed `gateway_web_fetch` (`mcp_lg_gateway_web_fetch`) is mounted on `lg`.
    pub has_gateway_web_fetch: bool,
    /// Pure zero-IO routing and 0-RTT search table retained by `McpSessionHandle`.
    pub routing_table: Arc<McpRoutingTable>,
    orchestrator: Option<OwnedOrchestratorContract>,
}

impl McpSessionCatalog {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.wire_tools.is_empty()
    }

    #[must_use]
    pub fn lookup(&self, mcp_name: &str) -> Option<&McpBridgedTool> {
        self.by_mcp_name.get(mcp_name)
    }

    /// Active Code Mode orchestrator prompt contract for `<api_gateway_bridge_rules>`, if any.
    #[must_use]
    pub fn orchestrator_contract(&self) -> Option<McpOrchestratorContract<'_>> {
        self.orchestrator
            .as_ref()
            .map(OwnedOrchestratorContract::as_borrowed)
    }
}

/// Auto-detects the caller harness and partitions `client_tools` using static enum dispatch.
#[must_use]
pub fn partition_tools_for_mcp<S: TruncationSpillSink>(
    client_tools: &[CanonicalToolSpec],
    spill: &mut S,
) -> McpSessionCatalog {
    partition_tools_for_mcp_with_hint(ProtocolHint::Unknown, client_tools, spill)
}

/// Auto-detects the caller harness (guided by `hint`) and statically dispatches to its ZST profile.
#[must_use]
pub fn partition_tools_for_mcp_with_hint<S: TruncationSpillSink>(
    hint: ProtocolHint,
    client_tools: &[CanonicalToolSpec],
    spill: &mut S,
) -> McpSessionCatalog {
    partition_tools_for_mcp_session(hint, client_tools, false, spill)
}

/// Auto-detects the caller harness and partitions `client_tools` plus optional server-side
/// `gateway_web_fetch` onto the loopback MCP server.
#[must_use]
pub fn partition_tools_for_mcp_session<S: TruncationSpillSink>(
    hint: ProtocolHint,
    client_tools: &[CanonicalToolSpec],
    enable_gateway_web_fetch: bool,
    spill: &mut S,
) -> McpSessionCatalog {
    let harness = detect_harness(hint, client_tools);
    match harness {
        DetectedHarness::Codex => plan_virtualization_with_options(
            CodexToolProfile,
            client_tools,
            enable_gateway_web_fetch,
            spill,
        ),
        DetectedHarness::ClaudeCode => plan_virtualization_with_options(
            ClaudeCodeToolProfile,
            client_tools,
            enable_gateway_web_fetch,
            spill,
        ),
        DetectedHarness::OpenCode => plan_virtualization_with_options(
            OpenCodeToolProfile,
            client_tools,
            enable_gateway_web_fetch,
            spill,
        ),
        DetectedHarness::Omp => plan_virtualization_with_options(
            OmpToolProfile,
            client_tools,
            enable_gateway_web_fetch,
            spill,
        ),
        DetectedHarness::Generic => plan_virtualization_with_options(
            GenericToolProfile,
            client_tools,
            enable_gateway_web_fetch,
            spill,
        ),
    }
}

/// Partitions `client_tools` for an explicit [`DetectedHarness`] without `dyn` dispatch.
#[must_use]
pub fn partition_tools_for_harness<S: TruncationSpillSink>(
    harness: DetectedHarness,
    client_tools: &[CanonicalToolSpec],
    spill: &mut S,
) -> McpSessionCatalog {
    match harness {
        DetectedHarness::Codex => {
            plan_virtualization_with_options(CodexToolProfile, client_tools, false, spill)
        }
        DetectedHarness::ClaudeCode => {
            plan_virtualization_with_options(ClaudeCodeToolProfile, client_tools, false, spill)
        }
        DetectedHarness::OpenCode => {
            plan_virtualization_with_options(OpenCodeToolProfile, client_tools, false, spill)
        }
        DetectedHarness::Omp => {
            plan_virtualization_with_options(OmpToolProfile, client_tools, false, spill)
        }
        DetectedHarness::Generic => {
            plan_virtualization_with_options(GenericToolProfile, client_tools, false, spill)
        }
    }
}

/// Monomorphized zero-cost 3-tier virtualization planner parameterized over `P: HarnessToolProfile`.
#[must_use]
pub fn plan_virtualization<P: HarnessToolProfile, S: TruncationSpillSink>(
    profile: P,
    client_tools: &[CanonicalToolSpec],
    spill: &mut S,
) -> McpSessionCatalog {
    plan_virtualization_with_options(profile, client_tools, false, spill)
}

/// Monomorphized zero-cost 3-tier virtualization planner with optional `gateway_web_fetch` mounting.
#[must_use]
pub fn plan_virtualization_with_options<P: HarnessToolProfile, S: TruncationSpillSink>(
    profile: P,
    client_tools: &[CanonicalToolSpec],
    enable_gateway_web_fetch: bool,
    spill: &mut S,
) -> McpSessionCatalog {
    assert_eq!(
        std::mem::size_of::<P>(),
        0,
        "HarnessToolProfile implementations must be zero-sized unit structs"
    );
    let budget = P::BUDGET;
    budget.assert_valid();

    #[derive(Clone, Debug)]
    enum CandidateKind {
        Orchestrator,
        CoreDirect { rank: u8 },
        Namespaced { namespace: String, action: String },
        Standalone { backfill_rank: u8 },
        FoldedIntoOrchestrator,
    }

    struct EligibleCandidate {
        orig_idx: usize,
        kind: CandidateKind,
        is_orchestrator: bool,
        is_custom: bool,
        unwrap_custom_key: Option<&'static str>,
        v4a_patch: Option<V4aPatchContract>,
        description: Option<String>,
        input_schema: Value,
        parameters_json: Option<String>,
        est_wire_bytes: usize,
    }

    let prepass = profile.build_prepass(client_tools);
    let mut eligible: Vec<EligibleCandidate> = Vec::with_capacity(client_tools.len());

    for (orig_idx, spec) in client_tools.iter().enumerate() {
        let placement = profile.classify_tool(spec, &prepass);
        let v4a_patch = profile.v4a_patch_contract(spec);
        let (schema_val, is_custom, unwrap_custom_key, parameters_json) = match &spec.kind {
            CanonicalToolKind::Custom { .. } => {
                let (unwrap_key, schema_val) = match &placement {
                    ToolPlacement::Orchestrator {
                        custom_unwrap_key: Some(key),
                    } => (*key, code_mode_exec_input_schema()),
                    ToolPlacement::Orchestrator { .. }
                    | ToolPlacement::CoreDirect { .. }
                    | ToolPlacement::Namespaced { .. }
                    | ToolPlacement::Standalone { .. }
                    | ToolPlacement::FoldedIntoOrchestrator => profile.custom_tool_mcp_bridge(spec),
                };
                (schema_val, true, Some(unwrap_key), None)
            }
            CanonicalToolKind::Function { parameters_json } => {
                let schema_val = normalize_mcp_object_schema(parameters_json);
                let raw_opt = (!parameters_json.trim().is_empty()).then(|| parameters_json.clone());
                (schema_val, false, None, raw_opt)
            }
        };

        let (cand_kind, is_orchestrator) = match placement {
            ToolPlacement::Orchestrator { .. } => (CandidateKind::Orchestrator, true),
            ToolPlacement::CoreDirect { rank } => (CandidateKind::CoreDirect { rank }, false),
            ToolPlacement::Namespaced { namespace, action } => (
                CandidateKind::Namespaced {
                    namespace: namespace.into_owned(),
                    action: action.into_owned(),
                },
                false,
            ),
            ToolPlacement::Standalone { backfill_rank } => {
                (CandidateKind::Standalone { backfill_rank }, false)
            }
            ToolPlacement::FoldedIntoOrchestrator => (CandidateKind::FoldedIntoOrchestrator, false),
        };

        let max_desc = if is_orchestrator {
            budget.max_orchestrator_desc_bytes
        } else {
            budget.max_standard_desc_bytes
        };
        let dehydrated_spec = if is_orchestrator {
            None
        } else {
            spec.description.as_deref().and_then(|raw| {
                let (prose, ts) = profile.dehydrate_description(raw, false);
                ts.is_some().then(|| CanonicalToolSpec {
                    name: spec.name.clone(),
                    description: Some(prose.to_owned()),
                    kind: spec.kind.clone(),
                })
            })
        };
        let effective_spec = dehydrated_spec.as_ref().unwrap_or(spec);
        let description = compact_mcp_tool_description(effective_spec, max_desc, spill);
        let est_wire_bytes =
            estimate_wire_tool_bytes(&spec.name, description.as_deref(), &schema_val);
        eligible.push(EligibleCandidate {
            orig_idx,
            kind: cand_kind,
            is_orchestrator,
            is_custom,
            unwrap_custom_key,
            v4a_patch,
            description,
            input_schema: schema_val,
            parameters_json,
            est_wire_bytes,
        });
    }

    let folded_indices: Vec<usize> = eligible
        .iter()
        .filter(|c| matches!(c.kind, CandidateKind::FoldedIntoOrchestrator))
        .map(|c| c.orig_idx)
        .collect();
    let has_folded_tools = !folded_indices.is_empty();
    if has_folded_tools {
        let folded_block = build_folded_orchestrator_tools_block(client_tools, &folded_indices);
        if !folded_block.is_empty()
            && let Some(orch_cand) = eligible.iter_mut().find(|c| {
                c.is_orchestrator
                    && client_tools
                        .get(c.orig_idx)
                        .is_some_and(|spec| spec.name != "wait")
            })
        {
            let merged = match orch_cand.description.take() {
                Some(existing) if !existing.is_empty() => format!("{existing}\n\n{folded_block}"),
                _ => folded_block,
            };
            if merged.len() <= budget.max_orchestrator_desc_bytes {
                orch_cand.description = Some(merged);
            } else if let Some(spec) = client_tools.get(orch_cand.orig_idx) {
                let synthetic = CanonicalToolSpec {
                    name: spec.name.clone(),
                    description: Some(merged),
                    kind: spec.kind.clone(),
                };
                orch_cand.description = compact_mcp_tool_description(
                    &synthetic,
                    budget.max_orchestrator_desc_bytes,
                    spill,
                );
            }
            if let Some(spec) = client_tools.get(orch_cand.orig_idx) {
                orch_cand.est_wire_bytes = estimate_wire_tool_bytes(
                    &spec.name,
                    orch_cand.description.as_deref(),
                    &orch_cand.input_schema,
                );
            }
        }
    }

    let wire_eligible_count = eligible
        .iter()
        .filter(|c| !matches!(c.kind, CandidateKind::FoldedIntoOrchestrator))
        .count();
    let non_orch_wire_bytes: usize = eligible
        .iter()
        .filter(|c| !c.is_orchestrator && !matches!(c.kind, CandidateKind::FoldedIntoOrchestrator))
        .fold(0usize, |acc, c| acc.saturating_add(c.est_wire_bytes));
    let reserved_slots: usize = 2usize.saturating_add(usize::from(enable_gateway_web_fetch));
    let passthrough_max_tools = budget
        .direct_passthrough_max_tools
        .min(budget.max_eager_tools.saturating_sub(reserved_slots));
    let should_virtualize = wire_eligible_count > passthrough_max_tools
        || non_orch_wire_bytes > budget.direct_passthrough_max_non_orchestrator_bytes;

    let mut used_mcp_names: HashSet<String> =
        HashSet::with_capacity(budget.max_eager_tools.saturating_add(4));
    used_mcp_names.insert(SEARCH_CLIENT_TOOLS_MCP_NAME.to_owned());
    used_mcp_names.insert(CALL_CLIENT_TOOL_MCP_NAME.to_owned());
    if enable_gateway_web_fetch {
        used_mcp_names.insert(GATEWAY_WEB_FETCH_MCP_NAME.to_owned());
    }

    let mut wire_tools: Vec<McpWireTool> = Vec::with_capacity(budget.max_eager_tools);
    let mut by_mcp_name: HashMap<String, McpBridgedTool> = HashMap::new();
    let mut bridged_pairs: Vec<(String, String)> = Vec::new();
    let mut virtualized_hubs: Vec<(String, String, usize)> = Vec::new();
    let mut hub_names_by_namespace: HashMap<String, String> = HashMap::new();
    let mut orchestrator: Option<OwnedOrchestratorContract> = None;
    let call_tool_wire = build_call_client_tool_wire_tool();
    let search_tool_wire = build_search_client_tools_wire_tool(&budget);
    let web_fetch_wire = enable_gateway_web_fetch.then(build_gateway_web_fetch_wire_tool);

    if !should_virtualize {
        for c in eligible {
            if matches!(c.kind, CandidateKind::FoldedIntoOrchestrator) {
                continue;
            }
            let Some(spec) = client_tools.get(c.orig_idx) else {
                continue;
            };
            let mcp_name = sanitize_mcp_tool_name(&spec.name, &mut used_mcp_names);
            if orchestrator.is_none()
                && let Some(contract) =
                    profile.orchestrator_prompt_contract(spec, &mcp_name, has_folded_tools)
            {
                orchestrator = Some(OwnedOrchestratorContract::from_borrowed(contract));
            }
            wire_tools.push(McpWireTool {
                name: mcp_name.clone(),
                description: c.description,
                input_schema: c.input_schema,
            });
            by_mcp_name.insert(
                mcp_name.clone(),
                McpBridgedTool {
                    mcp_name: mcp_name.clone(),
                    client_name: spec.name.clone(),
                    is_custom: c.is_custom,
                    unwrap_custom_key: c.unwrap_custom_key,
                    v4a_patch: c.v4a_patch,
                    parameters_json: c.parameters_json,
                },
            );
            bridged_pairs.push((spec.name.clone(), mcp_name));
        }
        if let Some(wf_wire) = web_fetch_wire {
            wire_tools.push(wf_wire);
        }
        wire_tools.push(call_tool_wire);
        wire_tools.push(search_tool_wire);
    } else {
        let mut core_indices: Vec<(u8, u8, usize)> = eligible
            .iter()
            .enumerate()
            .filter_map(|(pos, c)| match c.kind {
                CandidateKind::Orchestrator => Some((0u8, 0u8, pos)),
                CandidateKind::CoreDirect { rank } => Some((1u8, rank, pos)),
                CandidateKind::Namespaced { .. }
                | CandidateKind::Standalone { .. }
                | CandidateKind::FoldedIntoOrchestrator => None,
            })
            .collect();
        core_indices.sort_by_key(|&(tier, rank, pos)| {
            (tier, rank, eligible.get(pos).map_or(0, |e| e.orig_idx))
        });
        let max_core = budget
            .max_direct_core_tools
            .min(budget.max_eager_tools.saturating_sub(reserved_slots));
        let selected_direct_set: HashSet<usize> = core_indices
            .into_iter()
            .take(max_core)
            .map(|(_, _, pos)| pos)
            .collect();

        struct PendingHub {
            namespace: String,
            actions: Vec<String>,
        }
        let mut hub_order: Vec<PendingHub> = Vec::new();
        let mut hub_idx_by_ns: HashMap<String, usize> = HashMap::new();
        let mut standalone_positions: Vec<(u8, usize, usize)> = Vec::new();

        for (pos, c) in eligible.iter().enumerate() {
            if selected_direct_set.contains(&pos) {
                continue;
            }
            match &c.kind {
                CandidateKind::Namespaced { namespace, action } => {
                    if let Some(&idx) = hub_idx_by_ns.get(namespace) {
                        if let Some(grp) = hub_order.get_mut(idx) {
                            grp.actions.push(action.clone());
                        }
                    } else {
                        let idx = hub_order.len();
                        hub_idx_by_ns.insert(namespace.clone(), idx);
                        hub_order.push(PendingHub {
                            namespace: namespace.clone(),
                            actions: vec![action.clone()],
                        });
                    }
                }
                CandidateKind::Standalone { backfill_rank } => {
                    standalone_positions.push((*backfill_rank, c.est_wire_bytes, pos));
                }
                CandidateKind::Orchestrator => {
                    standalone_positions.push((0, c.est_wire_bytes, pos));
                }
                CandidateKind::CoreDirect { rank } => {
                    standalone_positions.push((*rank, c.est_wire_bytes, pos));
                }
                CandidateKind::FoldedIntoOrchestrator => {}
            }
        }

        let core_slots = selected_direct_set.len();
        let max_hub_slots = budget
            .max_eager_tools
            .saturating_sub(core_slots.saturating_add(reserved_slots));
        if hub_order.len() > max_hub_slots {
            let mut indexed_hubs: Vec<(usize, PendingHub)> =
                hub_order.into_iter().enumerate().collect();
            indexed_hubs.sort_by(|a, b| {
                b.1.actions
                    .len()
                    .cmp(&a.1.actions.len())
                    .then(a.0.cmp(&b.0))
            });
            indexed_hubs.truncate(max_hub_slots);
            indexed_hubs.sort_by_key(|(orig_order, _)| *orig_order);
            hub_order = indexed_hubs.into_iter().map(|(_, grp)| grp).collect();
        }

        let mut reserved_dispatch_bytes = estimate_wire_tool_bytes(
            &call_tool_wire.name,
            call_tool_wire.description.as_deref(),
            &call_tool_wire.input_schema,
        )
        .saturating_add(estimate_wire_tool_bytes(
            &search_tool_wire.name,
            search_tool_wire.description.as_deref(),
            &search_tool_wire.input_schema,
        ));
        if let Some(ref wf_wire) = web_fetch_wire {
            reserved_dispatch_bytes =
                reserved_dispatch_bytes.saturating_add(estimate_wire_tool_bytes(
                    &wf_wire.name,
                    wf_wire.description.as_deref(),
                    &wf_wire.input_schema,
                ));
        }

        let mut built_hub_wires: Vec<(String, String, usize, McpWireTool)> =
            Vec::with_capacity(hub_order.len());
        let mut hubs_wire_bytes: usize = 0;
        for grp in hub_order {
            let raw_hub_name = format!("{NAMESPACE_HUB_MCP_PREFIX}{}", grp.namespace);
            let hub_mcp_name = sanitize_mcp_tool_name(&raw_hub_name, &mut used_mcp_names);
            let wire_hub = build_namespace_hub_wire_tool(
                &hub_mcp_name,
                &grp.namespace,
                &grp.actions,
                budget.small_hub_enum_max_actions,
            );
            hubs_wire_bytes = hubs_wire_bytes.saturating_add(estimate_wire_tool_bytes(
                &wire_hub.name,
                wire_hub.description.as_deref(),
                &wire_hub.input_schema,
            ));
            built_hub_wires.push((hub_mcp_name, grp.namespace, grp.actions.len(), wire_hub));
        }

        let mut direct_wire_bytes: usize = selected_direct_set
            .iter()
            .filter_map(|&pos| eligible.get(pos))
            .filter(|c| !c.is_orchestrator)
            .fold(0usize, |acc, c| acc.saturating_add(c.est_wire_bytes));

        standalone_positions.sort_by_key(|&(rank, bytes, pos)| {
            (rank, bytes, eligible.get(pos).map_or(0, |e| e.orig_idx))
        });
        let mut final_direct_set = selected_direct_set;
        for (_, cand_bytes, pos) in standalone_positions {
            let occupied_slots = final_direct_set
                .len()
                .saturating_add(built_hub_wires.len())
                .saturating_add(reserved_slots);
            if occupied_slots >= budget.max_eager_tools {
                break;
            }
            let projected_bytes = direct_wire_bytes
                .saturating_add(hubs_wire_bytes)
                .saturating_add(reserved_dispatch_bytes)
                .saturating_add(cand_bytes);
            if projected_bytes <= budget.max_eager_wire_bytes {
                final_direct_set.insert(pos);
                direct_wire_bytes = direct_wire_bytes.saturating_add(cand_bytes);
            }
        }

        let mut ordered_direct_positions: Vec<usize> = final_direct_set.into_iter().collect();
        ordered_direct_positions.sort_by_key(|&pos| eligible.get(pos).map_or(0, |e| e.orig_idx));
        for pos in ordered_direct_positions {
            let Some(c) = eligible.get(pos) else {
                continue;
            };
            let Some(spec) = client_tools.get(c.orig_idx) else {
                continue;
            };
            let mcp_name = sanitize_mcp_tool_name(&spec.name, &mut used_mcp_names);
            if orchestrator.is_none()
                && let Some(contract) =
                    profile.orchestrator_prompt_contract(spec, &mcp_name, has_folded_tools)
            {
                orchestrator = Some(OwnedOrchestratorContract::from_borrowed(contract));
            }
            wire_tools.push(McpWireTool {
                name: mcp_name.clone(),
                description: c.description.clone(),
                input_schema: c.input_schema.clone(),
            });
            by_mcp_name.insert(
                mcp_name.clone(),
                McpBridgedTool {
                    mcp_name: mcp_name.clone(),
                    client_name: spec.name.clone(),
                    is_custom: c.is_custom,
                    unwrap_custom_key: c.unwrap_custom_key,
                    v4a_patch: c.v4a_patch,
                    parameters_json: c.parameters_json.clone(),
                },
            );
            bridged_pairs.push((spec.name.clone(), mcp_name));
        }

        for (hub_mcp_name, namespace, count, wire_hub) in built_hub_wires {
            wire_tools.push(wire_hub);
            virtualized_hubs.push((hub_mcp_name.clone(), namespace.clone(), count));
            hub_names_by_namespace.insert(namespace, hub_mcp_name);
        }

        if let Some(wf_wire) = web_fetch_wire {
            wire_tools.push(wf_wire);
        }
        wire_tools.push(call_tool_wire);
        wire_tools.push(search_tool_wire);
    }

    let routing_table = Arc::new(McpRoutingTable::build_with_prepass_and_options(
        profile,
        &prepass,
        client_tools,
        by_mcp_name.clone(),
        hub_names_by_namespace,
        true,
        enable_gateway_web_fetch,
    ));

    McpSessionCatalog {
        profile_id: P::ID,
        wire_tools: Arc::from(wire_tools.into_boxed_slice()),
        by_mcp_name,
        bridged_pairs,
        virtualized_hubs,
        has_virtualized_dispatch: true,
        has_gateway_web_fetch: enable_gateway_web_fetch,
        routing_table,
        orchestrator,
    }
}

const MAX_FOLDED_TOOLS_SUMMARY_BYTES: usize = 6 * 1024;

fn build_folded_orchestrator_tools_block(
    client_tools: &[CanonicalToolSpec],
    folded_indices: &[usize],
) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(
        "<eval_session_tools>\n\
         Folded session tools (omitted from top-level `mcp_lg_*`; invoke inside `eval` via `await tool.<name>(args)` in Python/JS, or inspect full schemas with `mcp_lg_search_client_tools`):\n",
    );
    let mut omitted_count: usize = 0;
    for (pos, &orig_idx) in folded_indices.iter().enumerate() {
        let Some(spec) = client_tools.get(orig_idx) else {
            continue;
        };
        let sig = summarize_folded_tool_signature(spec);
        let summary = summarize_folded_tool_hint(spec);
        let line = if summary.is_empty() {
            format!("- `tool.{}`({sig})\n", spec.name)
        } else {
            format!("- `tool.{}`({sig}): {summary}\n", spec.name)
        };
        if out.len().saturating_add(line.len()) > MAX_FOLDED_TOOLS_SUMMARY_BYTES {
            omitted_count = folded_indices.len().saturating_sub(pos);
            break;
        }
        out.push_str(&line);
    }
    if omitted_count > 0 {
        out.push_str(&format!(
            "- ... (+{omitted_count} additional folded session tools; query `mcp_lg_search_client_tools`)\n"
        ));
    }
    out.push_str("</eval_session_tools>");
    out
}

fn summarize_folded_tool_signature(spec: &CanonicalToolSpec) -> String {
    let CanonicalToolKind::Function { parameters_json } = &spec.kind else {
        return "raw_input".to_owned();
    };
    let Ok(val) = serde_json::from_str::<Value>(parameters_json) else {
        return "{ ... }".to_owned();
    };
    let Some(props) = val.get("properties").and_then(Value::as_object) else {
        return "{}".to_owned();
    };
    if props.is_empty() {
        return "{}".to_owned();
    }
    let required: HashSet<&str> = val
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut req_names: Vec<&str> = Vec::new();
    let mut opt_names: Vec<&str> = Vec::new();
    for key in props.keys() {
        if required.contains(key.as_str()) {
            req_names.push(key.as_str());
        } else {
            opt_names.push(key.as_str());
        }
    }
    let mut parts: Vec<String> = Vec::with_capacity(req_names.len() + opt_names.len());
    for r in req_names {
        parts.push(r.to_owned());
    }
    for o in opt_names.iter().take(6) {
        parts.push(format!("{o}?"));
    }
    if opt_names.len() > 6 {
        parts.push("...".to_owned());
    }
    format!("{{ {} }}", parts.join(", "))
}

fn summarize_folded_tool_hint(spec: &CanonicalToolSpec) -> String {
    match spec.name.as_str() {
        "read" => {
            return "Read file/dir/URI (`\"file.rs:1-80\"` returns `[FILENAME#TAG]` hashline anchors for `mcp_lg_edit`; also resolves `skill://`, `help://`, `local://`)".to_owned();
        }
        "bash" => {
            return "Run shell command (with optional `pty`/`async`/`ready`) or mounted tool-commands (`ast_edit`, `debug`, `lsp --help`)".to_owned();
        }
        _ => {}
    }
    let Some(raw) = spec.description.as_deref() else {
        return String::new();
    };
    let first_line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('<'))
        .unwrap_or_default();
    if first_line.chars().count() <= 110 {
        first_line.to_owned()
    } else {
        let mut truncated: String = first_line.chars().take(107).collect();
        truncated.push_str("...");
        truncated
    }
}

fn build_namespace_hub_wire_tool(
    hub_mcp_name: &str,
    namespace: &str,
    actions: &[String],
    small_hub_enum_max_actions: usize,
) -> McpWireTool {
    let count = actions.len();
    let (desc, action_prop) = if count <= small_hub_enum_max_actions {
        let joined = actions.join(", ");
        (
            format!(
                "`{namespace}` tool hub ({count} actions: {joined}). Pass `action` and JSON `args`; call `{SEARCH_CLIENT_TOOLS_MCP_NAME}` with `namespace: \"{namespace}\"` to inspect exact parameter schemas."
            ),
            json!({
                "type": "string",
                "enum": actions,
                "description": format!("Action within `{namespace}` to invoke.")
            }),
        )
    } else {
        let sample: Vec<&str> = actions.iter().take(6).map(String::as_str).collect();
        (
            format!(
                "`{namespace}` tool hub ({count} actions, e.g. {}, ...). Call `{SEARCH_CLIENT_TOOLS_MCP_NAME}` with `namespace: \"{namespace}\"` and a `query` keyword FIRST to inspect exact `action` names and required `args` schemas.",
                sample.join(", ")
            ),
            json!({
                "type": "string",
                "description": format!("Exact action name within `{namespace}` (discover via `{SEARCH_CLIENT_TOOLS_MCP_NAME}`).")
            }),
        )
    };

    McpWireTool {
        name: hub_mcp_name.to_owned(),
        description: Some(desc),
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": action_prop,
                "args": {
                    "type": "object",
                    "description": "JSON arguments object matching the selected action's parameter schema."
                }
            },
            "required": ["action"]
        }),
    }
}

fn build_call_client_tool_wire_tool() -> McpWireTool {
    McpWireTool {
        name: CALL_CLIENT_TOOL_MCP_NAME.to_owned(),
        description: Some(format!(
            "Invoke any active client tool by exact `name` and JSON `arguments` object (for standalone, long-tail, or mid-session dynamically added tools). Use `{SEARCH_CLIENT_TOOLS_MCP_NAME}` first when parameter schemas are needed."
        )),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Exact client tool name."
                },
                "arguments": {
                    "type": "object",
                    "description": "JSON arguments object matching the target tool's schema."
                }
            },
            "required": ["name"]
        }),
    }
}

fn build_gateway_web_fetch_wire_tool() -> McpWireTool {
    McpWireTool {
        name: GATEWAY_WEB_FETCH_MCP_NAME.to_owned(),
        description: Some(
            "Fetch the live content of an http:// or https:// URL server-side via the gateway and return extracted readable text/markdown in the current step.".to_owned(),
        ),
        input_schema: gateway_web_fetch_input_schema(),
    }
}

fn build_search_client_tools_wire_tool(budget: &crate::budget::VirtualizerBudget) -> McpWireTool {
    McpWireTool {
        name: SEARCH_CLIENT_TOOLS_MCP_NAME.to_owned(),
        description: Some(
            "Zero-round-trip local lookup across all active client tools in this session. Query by `query` keywords and/or `namespace`, or pass exact `names` to retrieve full JSON `inputSchema` and Code Mode TypeScript signatures."
                .to_owned(),
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Space-separated keywords matching tool name, action, parameter names, or description."
                },
                "namespace": {
                    "type": "string",
                    "description": "Optional namespace filter (e.g. 'github', 'tasks', 'workflow', 'codex_tui', 'multi_agent_v1')."
                },
                "names": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional exact tool or action names to inspect."
                },
                "limit": {
                    "type": "integer",
                    "description": format!(
                        "Maximum tools to return (1..{}, default {}).",
                        budget.max_search_limit, budget.default_search_limit
                    )
                }
            }
        }),
    }
}
