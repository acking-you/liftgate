//! Pure zero-IO in-memory routing table, 0-RTT `search_client_tools` engine, and preflight validator.

use std::borrow::Cow;
use std::collections::HashMap;

use liftgate_types::{
    canonical::{CanonicalToolKind, CanonicalToolSpec},
    parser::{ApplyPatchPreflightError, normalize_code_mode_exec_js},
    session_key::{is_code_mode_custom_exec, is_code_mode_orchestrator_tool},
    telemetry::{CatalogToolEntry, CatalogToolTier, CatalogWireRole, CatalogWireTool},
};
use serde_json::Value;

use crate::{
    CALL_CLIENT_TOOL_MCP_NAME, GATEWAY_WEB_FETCH_MCP_NAME, READ_CONTEXT_SPILL_MCP_NAME,
    SEARCH_CLIENT_TOOLS_MCP_NAME,
    budget::VirtualizerBudget,
    planner::McpSessionCatalog,
    profile::{HarnessToolProfile, ToolPlacement, V4aPatchContract},
    schema::{
        McpBridgedTool, code_mode_exec_input_schema, coerce_stringified_container_fields,
        estimate_wire_tool_bytes, extract_custom_tool_input, extract_schema_metadata,
        normalize_apply_patch_text, normalize_mcp_object_schema, truncate_utf8_prefix,
        validate_apply_patch_preflight,
    },
};

/// Classification of a gateway-local immediate MCP reply (never forwarded to the client).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalReplyKind {
    Search,
    PreflightError,
    DisabledTool,
    SpillRead,
}

/// Pure transport-independent resolution of an incoming `tools/call` against [`McpRoutingTable`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VirtualizedCallDecision {
    /// Forward this translated call to the external caller harness.
    ForwardToClient {
        client_name: String,
        arguments: String,
        is_custom: bool,
        /// V4A auto-repair rewrote the patch text the model sent before it was forwarded.
        patch_repaired: bool,
    },
    /// Resolve immediately inside the gateway in 0-RTT without contacting the caller.
    LocalReply {
        text: String,
        is_error: bool,
        kind: LocalReplyKind,
    },
    /// Execute server-managed URL fetch inside the gateway and reply in-flight.
    GatewayWebFetch {
        url: String,
        pattern: Option<String>,
    },
    /// Read a bounded line slice or search matches from a gateway-local context spill file in 0-RTT.
    ReadContextSpill {
        path: String,
        start_line: usize,
        byte_offset: usize,
        max_lines: usize,
        query: Option<String>,
    },
}

/// Profile placement of a client tool, retained so the catalog can explain the final tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlacementTier {
    Orchestrator,
    CoreDirect,
    Namespaced,
    Standalone,
    FoldedIntoOrchestrator,
}

impl PlacementTier {
    const fn of(placement: &ToolPlacement<'_>) -> Self {
        match placement {
            ToolPlacement::Orchestrator { .. } => Self::Orchestrator,
            ToolPlacement::CoreDirect { .. } => Self::CoreDirect,
            ToolPlacement::Namespaced { .. } => Self::Namespaced,
            ToolPlacement::Standalone { .. } => Self::Standalone,
            ToolPlacement::FoldedIntoOrchestrator => Self::FoldedIntoOrchestrator,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct IndexedClientTool {
    pub(crate) client_name: String,
    pub(crate) placement: PlacementTier,
    pub(crate) namespace: Option<String>,
    pub(crate) short_action: Option<String>,
    pub(crate) invoke_via_mcp_name: String,
    pub(crate) folded_into_orchestrator: bool,
    pub(crate) is_custom: bool,
    pub(crate) unwrap_custom_key: Option<&'static str>,
    pub(crate) v4a_patch: Option<V4aPatchContract>,
    pub(crate) prose_description: String,
    pub(crate) ts_declaration: Option<String>,
    pub(crate) parameters_json: Option<String>,
    pub(crate) required_fields: Vec<String>,
    pub(crate) non_nullable_required_fields: Vec<String>,
    pub(crate) present_required_fields: Vec<String>,
    pub(crate) declares_tool_summary: bool,
    pub(crate) requires_string_intent_i: bool,
    pub(crate) has_root_combinator: bool,
    pub(crate) searchable_lower: String,
}

impl IndexedClientTool {
    #[must_use]
    pub(crate) fn estimated_heap_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.client_name.len())
            .saturating_add(self.namespace.as_deref().map_or(0, str::len))
            .saturating_add(self.short_action.as_deref().map_or(0, str::len))
            .saturating_add(self.invoke_via_mcp_name.len())
            .saturating_add(self.prose_description.len())
            .saturating_add(self.ts_declaration.as_deref().map_or(0, str::len))
            .saturating_add(self.parameters_json.as_deref().map_or(0, str::len))
            .saturating_add(self.searchable_lower.len())
            .saturating_add(
                self.required_fields
                    .iter()
                    .fold(0usize, |acc, s| acc.saturating_add(s.len())),
            )
    }
}

#[derive(Clone, Debug, Default)]
struct HubNamespaceTable {
    namespace: String,
    hub_mcp_name: String,
    actions: Vec<IndexedClientTool>,
}

type RebuildRoutingFn = fn(
    &[CanonicalToolSpec],
    HashMap<String, McpBridgedTool>,
    HashMap<String, String>,
    bool,
    bool,
    bool,
) -> McpRoutingTable;

/// Live routing and 0-RTT search snapshot for an ACP session's active client tools.
///
/// Retains a monomorphized `rebuild_fn` pointer (`build_routing_table_for_profile::<P>`) so
/// dynamic warm-turn tool additions/removals re-index under the exact same zero-cost
/// [`HarnessToolProfile`] `P` without infecting session-pool types with generic parameters or `dyn`.
#[derive(Clone, Debug)]
pub struct McpRoutingTable {
    profile_id: &'static str,
    budget: VirtualizerBudget,
    source_tools: Vec<CanonicalToolSpec>,
    direct_by_mcp: HashMap<String, McpBridgedTool>,
    hub_names_by_namespace: HashMap<String, String>,
    hubs_by_mcp: HashMap<String, HubNamespaceTable>,
    all_indexed_tools: Vec<IndexedClientTool>,
    has_virtualized_dispatch: bool,
    has_gateway_web_fetch: bool,
    has_context_spill_reader: bool,
    rebuild_fn: RebuildRoutingFn,
}

impl Default for McpRoutingTable {
    fn default() -> Self {
        Self {
            profile_id: crate::profiles::GenericToolProfile::ID,
            budget: VirtualizerBudget::DEFAULT_AGY_ACP,
            source_tools: Vec::new(),
            direct_by_mcp: HashMap::new(),
            hub_names_by_namespace: HashMap::new(),
            hubs_by_mcp: HashMap::new(),
            all_indexed_tools: Vec::new(),
            has_virtualized_dispatch: false,
            has_gateway_web_fetch: false,
            has_context_spill_reader: false,
            rebuild_fn: build_routing_table_for_profile::<crate::profiles::GenericToolProfile>,
        }
    }
}

pub(crate) fn build_routing_table_for_profile<P: HarnessToolProfile>(
    client_tools: &[CanonicalToolSpec],
    direct_by_mcp: HashMap<String, McpBridgedTool>,
    hub_names_by_namespace: HashMap<String, String>,
    has_virtualized_dispatch: bool,
    has_gateway_web_fetch: bool,
    has_context_spill_reader: bool,
) -> McpRoutingTable {
    McpRoutingTable::build_with_profile_and_session_options(
        P::default(),
        client_tools,
        direct_by_mcp,
        hub_names_by_namespace,
        has_virtualized_dispatch,
        has_gateway_web_fetch,
        has_context_spill_reader,
    )
}

impl McpRoutingTable {
    /// Builds a live routing and search index monomorphized over `profile: P`.
    #[must_use]
    pub fn build_with_profile<P: HarnessToolProfile>(
        profile: P,
        client_tools: &[CanonicalToolSpec],
        direct_by_mcp: HashMap<String, McpBridgedTool>,
        hub_names_by_namespace: HashMap<String, String>,
        has_virtualized_dispatch: bool,
    ) -> Self {
        Self::build_with_profile_and_options(
            profile,
            client_tools,
            direct_by_mcp,
            hub_names_by_namespace,
            has_virtualized_dispatch,
            false,
        )
    }

    /// Builds a live routing and search index with optional gateway `gateway_web_fetch` support.
    #[must_use]
    pub fn build_with_profile_and_options<P: HarnessToolProfile>(
        profile: P,
        client_tools: &[CanonicalToolSpec],
        direct_by_mcp: HashMap<String, McpBridgedTool>,
        hub_names_by_namespace: HashMap<String, String>,
        has_virtualized_dispatch: bool,
        has_gateway_web_fetch: bool,
    ) -> Self {
        Self::build_with_profile_and_session_options(
            profile,
            client_tools,
            direct_by_mcp,
            hub_names_by_namespace,
            has_virtualized_dispatch,
            has_gateway_web_fetch,
            false,
        )
    }

    /// Builds a live routing and search index with gateway `gateway_web_fetch` and `read_context_spill` flags.
    #[must_use]
    pub fn build_with_profile_and_session_options<P: HarnessToolProfile>(
        profile: P,
        client_tools: &[CanonicalToolSpec],
        direct_by_mcp: HashMap<String, McpBridgedTool>,
        hub_names_by_namespace: HashMap<String, String>,
        has_virtualized_dispatch: bool,
        has_gateway_web_fetch: bool,
        has_context_spill_reader: bool,
    ) -> Self {
        let prepass = profile.build_prepass(client_tools);
        Self::build_with_prepass_and_session_options(
            profile,
            &prepass,
            client_tools,
            direct_by_mcp,
            hub_names_by_namespace,
            has_virtualized_dispatch,
            has_gateway_web_fetch,
            has_context_spill_reader,
        )
    }

    /// Builds a routing table reusing an already-computed profile prepass.
    #[must_use]
    pub fn build_with_prepass_and_options<P: HarnessToolProfile>(
        profile: P,
        prepass: &P::Prepass,
        client_tools: &[CanonicalToolSpec],
        direct_by_mcp: HashMap<String, McpBridgedTool>,
        hub_names_by_namespace: HashMap<String, String>,
        has_virtualized_dispatch: bool,
        has_gateway_web_fetch: bool,
    ) -> Self {
        Self::build_with_prepass_and_session_options(
            profile,
            prepass,
            client_tools,
            direct_by_mcp,
            hub_names_by_namespace,
            has_virtualized_dispatch,
            has_gateway_web_fetch,
            false,
        )
    }

    /// Builds a routing table reusing an already-computed profile prepass and full session options.
    #[must_use]
    #[expect(
        clippy::too_many_arguments,
        reason = "internal zero-cost monomorphized builder passes prepass and boolean capabilities directly"
    )]
    pub fn build_with_prepass_and_session_options<P: HarnessToolProfile>(
        profile: P,
        prepass: &P::Prepass,
        client_tools: &[CanonicalToolSpec],
        mut direct_by_mcp: HashMap<String, McpBridgedTool>,
        hub_names_by_namespace: HashMap<String, String>,
        has_virtualized_dispatch: bool,
        has_gateway_web_fetch: bool,
        has_context_spill_reader: bool,
    ) -> Self {
        assert_eq!(
            std::mem::size_of::<P>(),
            0,
            "HarnessToolProfile implementations must be zero-sized unit structs"
        );
        let mut client_to_mcp_key: HashMap<String, String> =
            HashMap::with_capacity(direct_by_mcp.len());
        for (mcp_key, b) in &direct_by_mcp {
            client_to_mcp_key.insert(b.client_name.clone(), mcp_key.clone());
        }

        let mut hubs_by_mcp: HashMap<String, HubNamespaceTable> =
            HashMap::with_capacity(hub_names_by_namespace.len());
        for (ns, hub_mcp) in &hub_names_by_namespace {
            hubs_by_mcp.insert(
                hub_mcp.clone(),
                HubNamespaceTable {
                    namespace: ns.clone(),
                    hub_mcp_name: hub_mcp.clone(),
                    actions: Vec::new(),
                },
            );
        }

        let mut all_indexed_tools = Vec::with_capacity(client_tools.len());
        for spec in client_tools {
            let placement = profile.classify_tool(spec, prepass);
            let placement_tier = PlacementTier::of(&placement);
            let profile_v4a_patch = profile.v4a_patch_contract(spec);
            let is_orchestrator = matches!(placement, ToolPlacement::Orchestrator { .. });
            let folded_into_orchestrator =
                matches!(placement, ToolPlacement::FoldedIntoOrchestrator);
            let custom_bridge = match &spec.kind {
                CanonicalToolKind::Custom { .. } => match &placement {
                    ToolPlacement::Orchestrator {
                        custom_unwrap_key: Some(key),
                    } => Some((*key, code_mode_exec_input_schema())),
                    ToolPlacement::Orchestrator { .. }
                    | ToolPlacement::CoreDirect { .. }
                    | ToolPlacement::Namespaced { .. }
                    | ToolPlacement::Standalone { .. }
                    | ToolPlacement::FoldedIntoOrchestrator => {
                        Some(profile.custom_tool_mcp_bridge(spec))
                    }
                },
                CanonicalToolKind::Function { .. } => None,
            };
            let custom_unwrap_key = custom_bridge.as_ref().map(|(k, _)| *k);
            let is_custom_eligible = custom_bridge.is_some();

            let params_json = match &spec.kind {
                CanonicalToolKind::Custom { .. } => {
                    custom_bridge.map(|(_, schema_val)| schema_val.to_string())
                }
                CanonicalToolKind::Function { parameters_json } => {
                    if parameters_json.trim().is_empty() {
                        Some(normalize_mcp_object_schema(parameters_json).to_string())
                    } else {
                        Some(parameters_json.clone())
                    }
                }
            };

            let raw_desc = spec.description.as_deref().unwrap_or("");
            let (prose, ts_decl) = profile.dehydrate_description(raw_desc, is_orchestrator);
            let schema_meta = extract_schema_metadata(params_json.as_deref());

            let (namespace, short_action, invoke_via_mcp_name, is_custom, unwrap_key, v4a_patch) =
                if let Some(mcp_key) = client_to_mcp_key.get(spec.name.as_str())
                    && let Some(direct_mut) = direct_by_mcp.get_mut(mcp_key)
                {
                    direct_mut.is_custom = is_custom_eligible;
                    direct_mut.unwrap_custom_key = custom_unwrap_key;
                    direct_mut.v4a_patch = profile_v4a_patch;
                    direct_mut.parameters_json.clone_from(&params_json);
                    direct_mut
                        .non_nullable_required_fields
                        .clone_from(&schema_meta.non_nullable_required_fields);
                    direct_mut
                        .present_required_fields
                        .clone_from(&schema_meta.present_required_fields);
                    direct_mut.declares_tool_summary = schema_meta.declares_tool_summary;
                    direct_mut.requires_string_intent_i = schema_meta.requires_string_intent_i;
                    direct_mut.has_root_combinator = schema_meta.has_root_combinator;

                    let opt_ns = match &placement {
                        ToolPlacement::Namespaced { namespace, .. } => {
                            Some(namespace.clone().into_owned())
                        }
                        ToolPlacement::Orchestrator { .. }
                        | ToolPlacement::CoreDirect { .. }
                        | ToolPlacement::Standalone { .. }
                        | ToolPlacement::FoldedIntoOrchestrator => None,
                    };
                    (
                        opt_ns,
                        None,
                        direct_mut.mcp_name.clone(),
                        direct_mut.is_custom,
                        direct_mut.unwrap_custom_key,
                        direct_mut.v4a_patch,
                    )
                } else if let ToolPlacement::Namespaced { namespace, action } = placement {
                    let ns_owned = namespace.into_owned();
                    let act_owned = action.into_owned();
                    let via = hub_names_by_namespace
                        .get(&ns_owned)
                        .cloned()
                        .unwrap_or_else(|| CALL_CLIENT_TOOL_MCP_NAME.to_owned());
                    (
                        Some(ns_owned),
                        Some(act_owned),
                        via,
                        is_custom_eligible,
                        custom_unwrap_key,
                        profile_v4a_patch,
                    )
                } else {
                    (
                        None,
                        None,
                        CALL_CLIENT_TOOL_MCP_NAME.to_owned(),
                        is_custom_eligible,
                        custom_unwrap_key,
                        profile_v4a_patch,
                    )
                };

            let aliases = profile.search_aliases(spec, namespace.as_deref());
            let searchable_lower = format!(
                "{} {} {} {} {} {}",
                spec.name.to_ascii_lowercase(),
                namespace.as_deref().unwrap_or(""),
                short_action.as_deref().unwrap_or("").to_ascii_lowercase(),
                schema_meta.prop_names_joined.to_ascii_lowercase(),
                prose.to_ascii_lowercase(),
                aliases
            );

            let indexed = IndexedClientTool {
                client_name: spec.name.clone(),
                placement: placement_tier,
                namespace,
                short_action,
                invoke_via_mcp_name: invoke_via_mcp_name.clone(),
                folded_into_orchestrator,
                is_custom,
                unwrap_custom_key: unwrap_key,
                v4a_patch,
                prose_description: prose.to_owned(),
                ts_declaration: ts_decl.map(str::to_owned),
                parameters_json: params_json,
                required_fields: schema_meta.required_fields,
                non_nullable_required_fields: schema_meta.non_nullable_required_fields,
                present_required_fields: schema_meta.present_required_fields,
                declares_tool_summary: schema_meta.declares_tool_summary,
                requires_string_intent_i: schema_meta.requires_string_intent_i,
                has_root_combinator: schema_meta.has_root_combinator,
                searchable_lower,
            };

            if let Some(hub_table) = hubs_by_mcp.get_mut(&invoke_via_mcp_name) {
                hub_table.actions.push(indexed.clone());
            }
            all_indexed_tools.push(indexed);
        }

        Self {
            profile_id: P::ID,
            budget: P::BUDGET,
            source_tools: client_tools.to_vec(),
            direct_by_mcp,
            hub_names_by_namespace,
            hubs_by_mcp,
            all_indexed_tools,
            has_virtualized_dispatch,
            has_gateway_web_fetch,
            has_context_spill_reader,
            rebuild_fn: build_routing_table_for_profile::<P>,
        }
    }

    /// Active [`HarnessToolProfile::ID`] (`"codex"`, `"claude-code"`, `"generic"`, etc.).
    #[must_use]
    pub const fn profile_id(&self) -> &'static str {
        self.profile_id
    }

    /// Whether `search_client_tools` and `call_client_tool` are active on this table.
    #[must_use]
    pub const fn has_virtualized_dispatch(&self) -> bool {
        self.has_virtualized_dispatch
    }

    /// Returns `Some(rebuilt)` when `client_tools` differs from the indexed tool snapshot,
    /// preserving the exact monomorphized [`HarnessToolProfile`] `P` via `rebuild_fn`.
    #[must_use]
    pub fn rebuild_if_changed(&self, client_tools: &[CanonicalToolSpec]) -> Option<Self> {
        if self.source_tools == client_tools {
            return None;
        }
        Some((self.rebuild_fn)(
            client_tools,
            self.direct_by_mcp.clone(),
            self.hub_names_by_namespace.clone(),
            self.has_virtualized_dispatch,
            self.has_gateway_web_fetch,
            self.has_context_spill_reader,
        ))
    }

    /// Estimates heap bytes retained by all indexed tools in this routing table.
    #[must_use]
    pub fn estimated_retained_bytes(&self) -> usize {
        self.all_indexed_tools.iter().fold(0usize, |acc, item| {
            acc.saturating_add(item.estimated_heap_bytes())
        })
    }

    /// Budget this table was planned against.
    #[must_use]
    pub const fn budget(&self) -> VirtualizerBudget {
        self.budget
    }

    /// Explains how every active client tool maps onto the `lg` wire tools of `catalog`:
    /// tier, serving wire tool, client vs. wire definitions, and per-tool byte attribution.
    #[must_use]
    pub fn describe(&self, catalog: &McpSessionCatalog) -> CatalogDescription {
        let specs: HashMap<&str, &CanonicalToolSpec> = self
            .source_tools
            .iter()
            .map(|spec| (spec.name.as_str(), spec))
            .collect();
        let indexed_by_client: HashMap<&str, &IndexedClientTool> = self
            .all_indexed_tools
            .iter()
            .map(|t| (t.client_name.as_str(), t))
            .collect();
        let mcp_by_client: HashMap<&str, &str> = self
            .direct_by_mcp
            .values()
            .map(|b| (b.client_name.as_str(), b.mcp_name.as_str()))
            .collect();
        let orchestrator_mcp: Option<&str> = self
            .direct_by_mcp
            .values()
            .filter(|b| {
                indexed_by_client
                    .get(b.client_name.as_str())
                    .is_some_and(|t| t.placement == PlacementTier::Orchestrator)
            })
            .map(|b| b.mcp_name.as_str())
            .find(|name| *name != "wait");
        let folded_clients: Vec<String> = self
            .all_indexed_tools
            .iter()
            .filter(|t| t.folded_into_orchestrator)
            .map(|t| t.client_name.clone())
            .collect();

        let mut wire_tools = Vec::with_capacity(catalog.wire_tools.len());
        let mut wire_bytes_by_name: HashMap<&str, usize> =
            HashMap::with_capacity(catalog.wire_tools.len());
        let mut wire_by_name: HashMap<&str, &crate::schema::McpWireTool> =
            HashMap::with_capacity(catalog.wire_tools.len());
        for wire in catalog.wire_tools.iter() {
            let bytes = estimate_wire_tool_bytes(
                &wire.name,
                wire.description.as_deref(),
                &wire.input_schema,
            );
            wire_bytes_by_name.insert(wire.name.as_str(), bytes);
            wire_by_name.insert(wire.name.as_str(), wire);
            let (role, folded_actions) = if is_gateway_wire_tool(&wire.name) {
                (CatalogWireRole::Gateway, Vec::new())
            } else if let Some(hub) = self.hubs_by_mcp.get(&wire.name) {
                (
                    CatalogWireRole::NamespaceHub,
                    hub.actions.iter().map(|a| a.client_name.clone()).collect(),
                )
            } else if let Some(bridged) = self.direct_by_mcp.get(&wire.name) {
                let role = direct_wire_role(
                    indexed_by_client
                        .get(bridged.client_name.as_str())
                        .map(|t| t.placement),
                    catalog.is_passthrough,
                );
                let folded = if role == CatalogWireRole::Orchestrator
                    && orchestrator_mcp == Some(wire.name.as_str())
                {
                    folded_clients.clone()
                } else {
                    Vec::new()
                };
                (role, folded)
            } else {
                (CatalogWireRole::Unknown, Vec::new())
            };
            wire_tools.push(CatalogWireTool {
                wire_name: wire.name.clone(),
                role,
                description: wire.description.clone(),
                input_schema: wire.input_schema.to_string(),
                bytes,
                folded_actions,
            });
        }

        let mut client_tools_bytes = 0usize;
        let mut tools = Vec::with_capacity(self.all_indexed_tools.len());
        for item in &self.all_indexed_tools {
            let spec = specs.get(item.client_name.as_str()).copied();
            let (client_kind, client_schema) = match spec.map(|s| &s.kind) {
                Some(CanonicalToolKind::Function { parameters_json }) => {
                    ("function", Some(parameters_json.clone()))
                }
                Some(CanonicalToolKind::Custom { format_json }) => ("custom", format_json.clone()),
                None => ("function", None),
            };
            let client_description = spec.and_then(|s| s.description.clone());
            let client_bytes = item
                .client_name
                .len()
                .saturating_add(client_description.as_deref().map_or(0, str::len))
                .saturating_add(client_schema.as_deref().map_or(0, str::len));
            client_tools_bytes = client_tools_bytes.saturating_add(client_bytes);

            let direct_mcp = mcp_by_client.get(item.client_name.as_str()).copied();
            let (tier, wire_name, wire_bytes, wire_def) = if item.folded_into_orchestrator {
                (
                    CatalogToolTier::FoldedIntoOrchestrator,
                    orchestrator_mcp.map(str::to_owned),
                    0,
                    None,
                )
            } else if let Some(mcp_name) = direct_mcp {
                let tier = match direct_wire_role(Some(item.placement), catalog.is_passthrough) {
                    CatalogWireRole::Orchestrator => CatalogToolTier::Orchestrator,
                    CatalogWireRole::SlackBackfill => CatalogToolTier::SlackBackfill,
                    CatalogWireRole::Direct
                    | CatalogWireRole::NamespaceHub
                    | CatalogWireRole::Gateway
                    | CatalogWireRole::Unknown => CatalogToolTier::Direct,
                };
                (
                    tier,
                    Some(mcp_name.to_owned()),
                    wire_bytes_by_name.get(mcp_name).copied().unwrap_or(0),
                    wire_by_name.get(mcp_name).copied(),
                )
            } else if let Some(hub) = self.hubs_by_mcp.get(&item.invoke_via_mcp_name) {
                let hub_bytes = wire_bytes_by_name
                    .get(item.invoke_via_mcp_name.as_str())
                    .copied()
                    .unwrap_or(0);
                (
                    CatalogToolTier::NamespaceHubAction,
                    Some(hub.hub_mcp_name.clone()),
                    hub_bytes / hub.actions.len().max(1),
                    None,
                )
            } else {
                (
                    CatalogToolTier::SearchIndexOnly,
                    self.has_virtualized_dispatch
                        .then(|| CALL_CLIENT_TOOL_MCP_NAME.to_owned()),
                    0,
                    None,
                )
            };

            tools.push(CatalogToolEntry {
                client_name: item.client_name.clone(),
                tier,
                wire_name,
                client_kind: client_kind.to_owned(),
                client_description,
                client_schema,
                client_bytes,
                wire_description: wire_def.and_then(|w| w.description.clone()),
                wire_schema: wire_def.map(|w| w.input_schema.to_string()),
                wire_bytes,
                ts_signature: item.ts_declaration.clone(),
            });
        }

        CatalogDescription {
            tools,
            wire_tools,
            search_index_entries: self.all_indexed_tools.len(),
            client_tools_bytes,
            wire_budget_bytes: self.budget.max_eager_wire_bytes,
            eager_tool_budget: self.budget.max_eager_tools,
        }
    }

    /// Pure synchronous resolution of an incoming MCP `tools/call` invocation.
    #[must_use]
    pub fn resolve_call(
        &self,
        mcp_name: &str,
        arguments: &Value,
        active_tools: &[CanonicalToolSpec],
    ) -> VirtualizedCallDecision {
        if self.has_gateway_web_fetch && mcp_name == GATEWAY_WEB_FETCH_MCP_NAME {
            let url = arguments
                .get("url")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if url.is_empty() {
                return preflight_error_decision(
                    "Missing non-empty `url` string for `mcp_lg_gateway_web_fetch`.".to_owned(),
                );
            }
            let pattern = arguments
                .get("pattern")
                .or_else(|| arguments.get("query"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            return VirtualizedCallDecision::GatewayWebFetch {
                url: url.to_owned(),
                pattern,
            };
        }

        if self.has_context_spill_reader && mcp_name == READ_CONTEXT_SPILL_MCP_NAME {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if path.is_empty() {
                return preflight_error_decision(
                    "Missing non-empty `path` string for `mcp_lg_read_context_spill`.".to_owned(),
                );
            }
            let start_line = arguments
                .get("start_line")
                .and_then(Value::as_u64)
                .and_then(|v| usize::try_from(v).ok())
                .unwrap_or(1)
                .max(1);
            let byte_offset = arguments
                .get("byte_offset")
                .and_then(Value::as_u64)
                .and_then(|v| usize::try_from(v).ok())
                .unwrap_or(0);
            let max_lines = arguments
                .get("max_lines")
                .and_then(Value::as_u64)
                .and_then(|v| usize::try_from(v).ok())
                .unwrap_or(120)
                .clamp(1, 200);
            let query = arguments
                .get("query")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            return VirtualizedCallDecision::ReadContextSpill {
                path: path.to_owned(),
                start_line,
                byte_offset,
                max_lines,
                query,
            };
        }

        if self.has_virtualized_dispatch && mcp_name == SEARCH_CLIENT_TOOLS_MCP_NAME {
            let text = self.execute_search(arguments, active_tools);
            return VirtualizedCallDecision::LocalReply {
                text,
                is_error: false,
                kind: LocalReplyKind::Search,
            };
        }

        if let Some(bridged) = self.direct_by_mcp.get(mcp_name) {
            let Some(active_spec) = active_tools.iter().find(|t| t.name == bridged.client_name)
            else {
                return disabled_tool_decision(&bridged.client_name);
            };
            let live_bridged = refresh_bridged_tool_if_changed(bridged, active_spec);
            if live_bridged.is_custom {
                return resolve_custom_tool_call(
                    &live_bridged.client_name,
                    arguments,
                    live_bridged.unwrap_custom_key,
                    live_bridged.v4a_patch,
                );
            }
            let (serialized, patch_repaired) = match format_direct_tool_arguments(
                arguments,
                &live_bridged,
                active_spec,
                self.budget.max_single_schema_in_search_bytes,
            ) {
                Ok(formatted) => formatted,
                Err(preflight_msg) => return preflight_error_decision(preflight_msg),
            };
            return VirtualizedCallDecision::ForwardToClient {
                client_name: live_bridged.client_name.clone(),
                arguments: serialized,
                is_custom: false,
                patch_repaired,
            };
        }

        if let Some(hub) = self.hubs_by_mcp.get(mcp_name) {
            return self.resolve_hub_call(hub, arguments, active_tools);
        }

        if self.has_virtualized_dispatch && mcp_name == CALL_CLIENT_TOOL_MCP_NAME {
            return self.resolve_universal_dispatch_call(arguments, active_tools);
        }

        if is_tool_active(mcp_name, active_tools)
            && let Some(indexed) = self
                .all_indexed_tools
                .iter()
                .find(|t| t.client_name == mcp_name)
        {
            return finalize_indexed_tool_call(
                indexed,
                arguments,
                None,
                true,
                self.budget.max_single_schema_in_search_bytes,
            );
        }

        disabled_tool_decision(mcp_name)
    }

    fn resolve_hub_call(
        &self,
        hub: &HubNamespaceTable,
        arguments: &Value,
        active_tools: &[CanonicalToolSpec],
    ) -> VirtualizedCallDecision {
        let requested_action = arguments
            .get("action")
            .or_else(|| arguments.get("name"))
            .or_else(|| arguments.get("tool"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");

        if requested_action.is_empty() {
            let sample: Vec<&str> = hub
                .actions
                .iter()
                .filter_map(|a| a.short_action.as_deref())
                .take(10)
                .collect();
            return preflight_error_decision(format!(
                "Missing `action` string for `mcp_lg_{}`. Available `{}` actions include: [{}]. Call `mcp_lg_{SEARCH_CLIENT_TOOLS_MCP_NAME}` with `namespace: \"{}\"` to inspect schemas.",
                hub.hub_mcp_name,
                hub.namespace,
                sample.join(", "),
                hub.namespace
            ));
        }

        let matched = find_action_in_hub(&hub.actions, requested_action).or_else(|| {
            self.all_indexed_tools
                .iter()
                .find(|t| t.client_name == requested_action)
        });

        let Some(indexed) = matched else {
            let suggestions = suggest_closest_actions(&hub.actions, requested_action, 8);
            return preflight_error_decision(format!(
                "Unknown action `{requested_action}` on `mcp_lg_{}`. Closest `{}` actions: [{}]. Call `mcp_lg_{SEARCH_CLIENT_TOOLS_MCP_NAME}` with `namespace: \"{}\"` and `query` to inspect exact action names and schemas.",
                hub.hub_mcp_name,
                hub.namespace,
                suggestions.join(", "),
                hub.namespace
            ));
        };

        if !is_tool_active(&indexed.client_name, active_tools) {
            return disabled_tool_decision(&indexed.client_name);
        }

        let outer_summary = arguments.get("toolSummary").and_then(Value::as_str);
        let inner_args = match extract_virtualized_inner_args(
            arguments,
            &["action", "toolSummary"],
            &indexed.required_fields,
        ) {
            Ok(v) => v,
            Err(err) => return preflight_error_decision(err.into_message()),
        };

        finalize_indexed_tool_call(
            indexed,
            &inner_args,
            outer_summary,
            true,
            self.budget.max_single_schema_in_search_bytes,
        )
    }

    fn resolve_universal_dispatch_call(
        &self,
        arguments: &Value,
        active_tools: &[CanonicalToolSpec],
    ) -> VirtualizedCallDecision {
        let requested_name = arguments
            .get("name")
            .or_else(|| arguments.get("tool"))
            .or_else(|| arguments.get("action"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");

        if requested_name.is_empty() {
            return preflight_error_decision(format!(
                "Missing `name` string for `mcp_lg_{CALL_CLIENT_TOOL_MCP_NAME}`. Call `mcp_lg_{SEARCH_CLIENT_TOOLS_MCP_NAME}` first to discover active tool names."
            ));
        }

        let matched = self
            .all_indexed_tools
            .iter()
            .find(|t| t.client_name == requested_name)
            .or_else(|| {
                self.all_indexed_tools
                    .iter()
                    .find(|t| t.client_name.eq_ignore_ascii_case(requested_name))
            })
            .or_else(|| {
                let mut suffix_matches = self.all_indexed_tools.iter().filter(|t| {
                    t.short_action
                        .as_deref()
                        .is_some_and(|sa| sa.eq_ignore_ascii_case(requested_name))
                });
                let first = suffix_matches.next()?;
                if suffix_matches.next().is_none() {
                    Some(first)
                } else {
                    None
                }
            });

        let Some(indexed) = matched else {
            let suggestions = suggest_closest_tools(&self.all_indexed_tools, requested_name, 8);
            return preflight_error_decision(format!(
                "Unknown client tool `{requested_name}`. Closest active tools: [{}]. Call `mcp_lg_{SEARCH_CLIENT_TOOLS_MCP_NAME}` with `query` to inspect exact tool names and parameter schemas.",
                suggestions.join(", ")
            ));
        };

        if !is_tool_active(&indexed.client_name, active_tools) {
            return disabled_tool_decision(&indexed.client_name);
        }

        let outer_summary = arguments.get("toolSummary").and_then(Value::as_str);
        let inner_args = match extract_virtualized_inner_args(
            arguments,
            &["name", "tool", "action", "toolSummary"],
            &indexed.required_fields,
        ) {
            Ok(v) => v,
            Err(err) => return preflight_error_decision(err.into_message()),
        };

        finalize_indexed_tool_call(
            indexed,
            &inner_args,
            outer_summary,
            true,
            self.budget.max_single_schema_in_search_bytes,
        )
    }

    fn execute_search(&self, args: &Value, active_tools: &[CanonicalToolSpec]) -> String {
        let query_raw = args
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let ns_filter = args
            .get("namespace")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.trim_start_matches("mcp_lg_ns__")
                    .trim_start_matches("ns__")
                    .to_ascii_lowercase()
            });
        let exact_names: Vec<String> = args
            .get("names")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.trim().to_ascii_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(self.budget.default_search_limit)
            .clamp(1, self.budget.max_search_limit);

        let tokens: Vec<String> = query_raw
            .split(|c: char| c.is_whitespace() || c == ',' || c == '/')
            .map(|t| t.trim().to_ascii_lowercase())
            .filter(|t| !t.is_empty())
            .collect();

        let mut scored: Vec<(u32, usize, &IndexedClientTool)> = Vec::new();
        for (idx, item) in self.all_indexed_tools.iter().enumerate() {
            if !is_tool_active(&item.client_name, active_tools) {
                continue;
            }
            if let Some(ref ns_req) = ns_filter {
                let item_ns = item.namespace.as_deref().unwrap_or("");
                if item_ns != ns_req && !item.client_name.to_ascii_lowercase().contains(ns_req) {
                    continue;
                }
            }
            let score = score_indexed_tool(item, &exact_names, &tokens);
            if (!exact_names.is_empty() || !tokens.is_empty()) && score == 0 {
                continue;
            }
            scored.push((score, idx, item));
        }

        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let total_matches = scored.len();
        let has_code_mode_exec = active_tools
            .iter()
            .any(|t| is_code_mode_custom_exec(t) || is_code_mode_orchestrator_tool(t));

        if scored.is_empty() {
            let mut ns_counts: HashMap<&str, usize> = HashMap::new();
            for item in &self.all_indexed_tools {
                if is_tool_active(&item.client_name, active_tools) {
                    let label = item.namespace.as_deref().unwrap_or("standalone");
                    *ns_counts.entry(label).or_insert(0) += 1;
                }
            }
            let mut ns_list: Vec<(&str, usize)> = ns_counts.into_iter().collect();
            ns_list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            let formatted_ns: Vec<String> = ns_list
                .into_iter()
                .map(|(ns, cnt)| format!("{ns} ({cnt})"))
                .collect();
            let client_tool_search_hint = if active_tools.iter().any(|t| {
                matches!(
                    t.name.as_str(),
                    "tool_search" | "ToolSearch" | "__codex_tool_search"
                )
            }) {
                "\nHint: if the connector has not been loaded by the client yet, invoke the client's own `tool_search` / `ToolSearch` tool."
            } else {
                ""
            };
            return format!(
                "No active client tools matched query={query_raw:?}, namespace={ns_filter:?}, names={exact_names:?}.\nAvailable namespaces: [{}].{client_tool_search_hint}",
                formatted_ns.join(", ")
            );
        }

        let mut out = String::with_capacity(2048);
        out.push_str(&format!(
            "Matched {total_matches} active client tool(s) (showing top {}):\n",
            total_matches.min(limit)
        ));

        for (_, _, item) in scored.into_iter().take(limit) {
            if out.len() >= self.budget.max_search_reply_bytes {
                out.push_str(
                    "\n...[search output truncated; refine `query` or pass exact `names`]",
                );
                break;
            }
            let invoke_hint = if item.folded_into_orchestrator {
                format!(
                    "`mcp_lg_eval` cell via `await tool.{}({{ ... }})`",
                    item.client_name
                )
            } else if item.invoke_via_mcp_name == CALL_CLIENT_TOOL_MCP_NAME {
                format!(
                    "`mcp_lg_{CALL_CLIENT_TOOL_MCP_NAME}` with `{{\"name\": \"{}\", \"arguments\": {{ ... }}}}`",
                    item.client_name
                )
            } else if item.invoke_via_mcp_name.starts_with("ns__") {
                let act = item
                    .short_action
                    .as_deref()
                    .unwrap_or(item.client_name.as_str());
                format!(
                    "`mcp_lg_{}` with `{{\"action\": \"{act}\", \"args\": {{ ... }}}}`",
                    item.invoke_via_mcp_name
                )
            } else {
                format!("`mcp_lg_{}` directly", item.invoke_via_mcp_name)
            };

            let desc_short = truncate_utf8_prefix(&item.prose_description, 520);
            let schema_short = item
                .parameters_json
                .as_deref()
                .map(|s| truncate_utf8_prefix(s, self.budget.max_single_schema_in_search_bytes))
                .unwrap_or(Cow::Borrowed("{}"));

            out.push_str(&format!(
                "\n- tool: `{}`\n  invoke_via: {invoke_hint}\n  description: {desc_short}\n  inputSchema: {schema_short}\n",
                item.client_name
            ));
            if has_code_mode_exec && let Some(ref ts) = item.ts_declaration {
                let ts_short = truncate_utf8_prefix(ts, 1200);
                out.push_str(&format!("  codeModeTs: {ts_short}\n"));
            }
        }

        out
    }
}

/// How a session's active client tools map onto its `lg` wire tools.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogDescription {
    /// Every active client tool, in declaration order.
    pub tools: Vec<CatalogToolEntry>,
    /// Every tool mounted on `lg`, in `tools/list` order.
    pub wire_tools: Vec<CatalogWireTool>,
    /// Entries reachable through 0-RTT `search_client_tools`.
    pub search_index_entries: usize,
    /// Bytes of the client-declared tool list (names, descriptions, schemas).
    pub client_tools_bytes: usize,
    pub wire_budget_bytes: usize,
    pub eager_tool_budget: usize,
}

fn is_gateway_wire_tool(name: &str) -> bool {
    matches!(
        name,
        SEARCH_CLIENT_TOOLS_MCP_NAME
            | CALL_CLIENT_TOOL_MCP_NAME
            | GATEWAY_WEB_FETCH_MCP_NAME
            | READ_CONTEXT_SPILL_MCP_NAME
    )
}

/// Role of a client tool mounted 1:1 on `lg`: core tools stay direct, while long-tail tools that
/// only fit because the virtualized plan had spare slots are slack-slot backfills.
const fn direct_wire_role(
    placement: Option<PlacementTier>,
    is_passthrough: bool,
) -> CatalogWireRole {
    match placement {
        Some(PlacementTier::Orchestrator) => CatalogWireRole::Orchestrator,
        _ if is_passthrough => CatalogWireRole::Direct,
        Some(PlacementTier::CoreDirect) | None => CatalogWireRole::Direct,
        Some(
            PlacementTier::Standalone
            | PlacementTier::Namespaced
            | PlacementTier::FoldedIntoOrchestrator,
        ) => CatalogWireRole::SlackBackfill,
    }
}

fn is_tool_active(client_name: &str, active_tools: &[CanonicalToolSpec]) -> bool {
    active_tools.iter().any(|t| t.name == client_name)
}

fn disabled_tool_decision(tool_name: &str) -> VirtualizedCallDecision {
    VirtualizedCallDecision::LocalReply {
        text: format!("Tool `{tool_name}` is disabled for this turn; do not call it."),
        is_error: true,
        kind: LocalReplyKind::DisabledTool,
    }
}

fn preflight_error_decision(message: String) -> VirtualizedCallDecision {
    VirtualizedCallDecision::LocalReply {
        text: message,
        is_error: true,
        kind: LocalReplyKind::PreflightError,
    }
}

fn resolve_custom_tool_call(
    client_name: &str,
    raw_args: &Value,
    unwrap_custom_key: Option<&'static str>,
    v4a_patch: Option<V4aPatchContract>,
) -> VirtualizedCallDecision {
    let extracted = extract_custom_tool_input(raw_args, unwrap_custom_key);
    if matches!(v4a_patch, Some(V4aPatchContract::CustomFreeform)) {
        let normalized = normalize_apply_patch_text(&extracted);
        if let Err(err) = validate_apply_patch_preflight(&normalized) {
            return preflight_error_decision(err.to_string());
        }
        return VirtualizedCallDecision::ForwardToClient {
            client_name: client_name.to_owned(),
            patch_repaired: normalized != extracted,
            arguments: normalized,
            is_custom: true,
        };
    }
    if unwrap_custom_key == Some("code") {
        let normalized = normalize_code_mode_exec_js(&extracted);
        let repaired = normalized.as_ref() != extracted && extracted.contains("*** Begin Patch");
        return VirtualizedCallDecision::ForwardToClient {
            client_name: client_name.to_owned(),
            patch_repaired: repaired,
            arguments: normalized.into_owned(),
            is_custom: true,
        };
    }
    VirtualizedCallDecision::ForwardToClient {
        client_name: client_name.to_owned(),
        arguments: extracted,
        is_custom: true,
        patch_repaired: false,
    }
}

/// Repairs and validates a JSON-carried V4A patch; `Ok(true)` when the repair changed its text.
fn heal_and_validate_json_patch_field(
    v4a_patch: Option<V4aPatchContract>,
    map: &mut serde_json::Map<String, Value>,
) -> Result<bool, ApplyPatchPreflightError> {
    let Some(V4aPatchContract::JsonField(field_key)) = v4a_patch else {
        return Ok(false);
    };
    let Some(Value::String(raw_patch)) = map.get_mut(field_key) else {
        return Ok(false);
    };
    let normalized = normalize_apply_patch_text(raw_patch);
    validate_apply_patch_preflight(&normalized)?;
    let repaired = normalized != *raw_patch;
    *raw_patch = normalized;
    Ok(repaired)
}

fn backfill_intent_i_from_summary(
    map: &mut serde_json::Map<String, Value>,
    requires_string_intent_i: bool,
    outer_summary_fallback: Option<&str>,
) {
    if !requires_string_intent_i {
        return;
    }
    let missing_i = map
        .get("i")
        .and_then(Value::as_str)
        .is_none_or(|s| s.trim().is_empty());
    if !missing_i {
        return;
    }
    let candidate = map
        .get("toolSummary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            outer_summary_fallback
                .map(str::trim)
                .filter(|s| !s.is_empty())
        });
    if let Some(summary) = candidate {
        map.insert("i".to_owned(), Value::String(summary.to_owned()));
    }
}

fn check_missing_non_nullable_required(
    map: &serde_json::Map<String, Value>,
    non_nullable_required_fields: &[String],
    present_required_fields: &[String],
    client_name: &str,
    parameters_json: Option<&str>,
    max_schema_bytes: usize,
) -> Result<(), String> {
    if non_nullable_required_fields.is_empty() && present_required_fields.is_empty() {
        return Ok(());
    }
    let mut missing: Vec<&str> = non_nullable_required_fields
        .iter()
        .filter(|req_key| map.get(req_key.as_str()).is_none_or(Value::is_null))
        .map(String::as_str)
        .collect();
    for req_key in present_required_fields {
        if !map.contains_key(req_key.as_str()) {
            missing.push(req_key.as_str());
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let provided: Vec<&str> = map.keys().map(String::as_str).collect();
    let schema_snippet = parameters_json
        .map(|s| truncate_utf8_prefix(s, max_schema_bytes))
        .unwrap_or(Cow::Borrowed("{}"));
    Err(format!(
        "Missing required parameter(s) [{}] for `{client_name}` (provided keys: [{}]).\nExact inputSchema:\n{schema_snippet}\nRetry the call with all required parameters.",
        missing.join(", "),
        provided.join(", ")
    ))
}

fn refresh_bridged_tool_if_changed<'a>(
    bridged: &'a McpBridgedTool,
    active_spec: &CanonicalToolSpec,
) -> Cow<'a, McpBridgedTool> {
    match &active_spec.kind {
        CanonicalToolKind::Custom { .. } => {
            if bridged.is_custom {
                Cow::Borrowed(bridged)
            } else {
                let unwrap_key = if active_spec.name == "apply_patch" {
                    Some("patch")
                } else if is_code_mode_custom_exec(active_spec) {
                    Some("code")
                } else {
                    Some("input")
                };
                let mut updated = bridged.clone();
                updated.is_custom = true;
                updated.unwrap_custom_key = unwrap_key;
                updated.parameters_json = None;
                updated.non_nullable_required_fields.clear();
                updated.present_required_fields.clear();
                Cow::Owned(updated)
            }
        }
        CanonicalToolKind::Function { parameters_json } => {
            let normalized_opt = if parameters_json.trim().is_empty() {
                Some(normalize_mcp_object_schema(parameters_json).to_string())
            } else {
                None
            };
            let effective_json = normalized_opt
                .as_deref()
                .unwrap_or(parameters_json.as_str());
            if !bridged.is_custom && bridged.parameters_json.as_deref() == Some(effective_json) {
                return Cow::Borrowed(bridged);
            }
            let meta = extract_schema_metadata(Some(effective_json));
            Cow::Owned(McpBridgedTool {
                mcp_name: bridged.mcp_name.clone(),
                client_name: bridged.client_name.clone(),
                is_custom: false,
                unwrap_custom_key: None,
                v4a_patch: bridged.v4a_patch,
                parameters_json: Some(effective_json.to_owned()),
                non_nullable_required_fields: meta.non_nullable_required_fields,
                present_required_fields: meta.present_required_fields,
                declares_tool_summary: meta.declares_tool_summary,
                requires_string_intent_i: meta.requires_string_intent_i,
                has_root_combinator: meta.has_root_combinator,
            })
        }
    }
}

/// Arguments forwarded for a direct (1:1) tool, plus whether V4A auto-repair changed its patch.
fn format_direct_tool_arguments(
    raw_args: &Value,
    bridged: &McpBridgedTool,
    active_spec: &CanonicalToolSpec,
    max_schema_bytes: usize,
) -> Result<(String, bool), String> {
    if bridged.is_custom {
        return Ok((
            extract_custom_tool_input(raw_args, bridged.unwrap_custom_key),
            false,
        ));
    }
    let mut patch_repaired = false;
    let mut args_val = raw_args.clone();
    if args_val.is_null() {
        args_val = Value::Object(serde_json::Map::new());
    }
    if let Value::Object(ref mut map) = args_val {
        backfill_intent_i_from_summary(map, bridged.requires_string_intent_i, None);
        if !bridged.declares_tool_summary {
            let _ = map.remove("toolSummary");
        }
        if let Some(ref params_json) = bridged.parameters_json {
            let _ = coerce_stringified_container_fields(map, params_json);
        }
        if (liftgate_types::session_key::is_codex_exec_orchestrator_tool(active_spec)
            || liftgate_types::session_key::is_opencode_execute_orchestrator_tool(active_spec))
            && let Some(Value::String(raw_code)) = map.get_mut("code")
        {
            let normalized = normalize_code_mode_exec_js(raw_code);
            if let std::borrow::Cow::Owned(healed_code) = normalized {
                if raw_code.contains("*** Begin Patch") {
                    patch_repaired = true;
                }
                *raw_code = healed_code;
            }
        }
        patch_repaired |= heal_and_validate_json_patch_field(bridged.v4a_patch, map)
            .map_err(|err| err.to_string())?;
        if !bridged.has_root_combinator {
            check_missing_non_nullable_required(
                map,
                &bridged.non_nullable_required_fields,
                &bridged.present_required_fields,
                &bridged.client_name,
                bridged.parameters_json.as_deref(),
                max_schema_bytes,
            )?;
        }
    }
    Ok((
        serde_json::to_string(&args_val).unwrap_or_else(|_| "{}".to_owned()),
        patch_repaired,
    ))
}

enum InnerArgsError {
    NonObjectString { key: &'static str },
    NonObjectValue { key: &'static str },
}

impl InnerArgsError {
    fn into_message(self) -> String {
        match self {
            Self::NonObjectString { key } => {
                format!("`{key}` must be a JSON object, received non-object string.")
            }
            Self::NonObjectValue { key } => format!("`{key}` must be a JSON object."),
        }
    }
}

fn extract_virtualized_inner_args(
    outer: &Value,
    wrapper_keys: &[&str],
    target_required: &[String],
) -> Result<Value, InnerArgsError> {
    let Value::Object(outer_map) = outer else {
        return Ok(Value::Object(serde_json::Map::new()));
    };
    for key in ["args", "arguments", "params"] {
        if let Some(inner) = outer_map.get(key) {
            return match inner {
                Value::Null => Ok(Value::Object(serde_json::Map::new())),
                Value::Object(_) => Ok(inner.clone()),
                Value::String(raw_str) => {
                    let trimmed = raw_str.trim();
                    if trimmed.is_empty() {
                        Ok(Value::Object(serde_json::Map::new()))
                    } else {
                        match serde_json::from_str::<Value>(trimmed) {
                            Ok(parsed @ Value::Object(_)) => Ok(parsed),
                            _ => Err(InnerArgsError::NonObjectString { key }),
                        }
                    }
                }
                _ => Err(InnerArgsError::NonObjectValue { key }),
            };
        }
    }

    let mut fallback = serde_json::Map::new();
    for (k, v) in outer_map {
        let is_wrapper = wrapper_keys.contains(&k.as_str());
        let required_by_target = target_required.iter().any(|r| r == k);
        if !is_wrapper || (required_by_target && k != "action" && k != "toolSummary") {
            fallback.insert(k.clone(), v.clone());
        }
    }
    Ok(Value::Object(fallback))
}

fn finalize_indexed_tool_call(
    indexed: &IndexedClientTool,
    inner_args: &Value,
    outer_summary_fallback: Option<&str>,
    enforce_required_preflight: bool,
    max_schema_bytes: usize,
) -> VirtualizedCallDecision {
    if indexed.is_custom {
        return resolve_custom_tool_call(
            &indexed.client_name,
            inner_args,
            indexed.unwrap_custom_key,
            indexed.v4a_patch,
        );
    }

    let mut map = match inner_args {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    backfill_intent_i_from_summary(
        &mut map,
        indexed.requires_string_intent_i,
        outer_summary_fallback,
    );
    if !indexed.declares_tool_summary {
        let _ = map.remove("toolSummary");
    }
    if let Some(ref params_json) = indexed.parameters_json {
        let _ = coerce_stringified_container_fields(&mut map, params_json);
    }
    let patch_repaired = match heal_and_validate_json_patch_field(indexed.v4a_patch, &mut map) {
        Ok(repaired) => repaired,
        Err(preflight_err) => return preflight_error_decision(preflight_err.to_string()),
    };

    if enforce_required_preflight
        && !indexed.has_root_combinator
        && let Err(msg) = check_missing_non_nullable_required(
            &map,
            &indexed.non_nullable_required_fields,
            &indexed.present_required_fields,
            &indexed.client_name,
            indexed.parameters_json.as_deref(),
            max_schema_bytes,
        )
    {
        return preflight_error_decision(msg);
    }

    let arguments = serde_json::to_string(&Value::Object(map)).unwrap_or_else(|_| "{}".to_owned());
    VirtualizedCallDecision::ForwardToClient {
        client_name: indexed.client_name.clone(),
        arguments,
        is_custom: false,
        patch_repaired,
    }
}

fn find_action_in_hub<'a>(
    actions: &'a [IndexedClientTool],
    requested: &str,
) -> Option<&'a IndexedClientTool> {
    if let Some(exact) = actions
        .iter()
        .find(|a| a.short_action.as_deref() == Some(requested) || a.client_name == requested)
    {
        return Some(exact);
    }
    let trimmed_req = requested.trim_start_matches('_');
    if !trimmed_req.is_empty()
        && let Some(stripped) = actions.iter().find(|a| {
            a.short_action
                .as_deref()
                .is_some_and(|sa| sa.trim_start_matches('_').eq_ignore_ascii_case(trimmed_req))
                || a.client_name.eq_ignore_ascii_case(requested)
        })
    {
        return Some(stripped);
    }
    None
}

fn suggest_closest_actions(
    actions: &[IndexedClientTool],
    requested: &str,
    max_items: usize,
) -> Vec<String> {
    let req_lower = requested.trim_start_matches('_').to_ascii_lowercase();
    let parts: Vec<&str> = req_lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| p.len() >= 2)
        .collect();
    let mut ranked: Vec<(usize, usize, &str)> = actions
        .iter()
        .enumerate()
        .map(|(idx, a)| {
            let label = a.short_action.as_deref().unwrap_or(a.client_name.as_str());
            let lower = label.to_ascii_lowercase();
            let mut score = usize::from(lower.contains(&req_lower)) * 10;
            for p in &parts {
                if lower.contains(p) {
                    score = score.saturating_add(3);
                }
            }
            (score, idx, label)
        })
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    ranked
        .into_iter()
        .take(max_items)
        .map(|(_, _, s)| s.to_owned())
        .collect()
}

fn suggest_closest_tools(
    tools: &[IndexedClientTool],
    requested: &str,
    max_items: usize,
) -> Vec<String> {
    let req_lower = requested.to_ascii_lowercase();
    let parts: Vec<&str> = req_lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| p.len() >= 2)
        .collect();
    let mut ranked: Vec<(usize, usize, &str)> = tools
        .iter()
        .enumerate()
        .map(|(idx, t)| {
            let lower = t.client_name.to_ascii_lowercase();
            let mut score = usize::from(lower.contains(&req_lower)) * 10;
            for p in &parts {
                if lower.contains(p) {
                    score = score.saturating_add(3);
                }
            }
            (score, idx, t.client_name.as_str())
        })
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    ranked
        .into_iter()
        .take(max_items)
        .map(|(_, _, s)| s.to_owned())
        .collect()
}

fn score_indexed_tool(item: &IndexedClientTool, exact_names: &[String], tokens: &[String]) -> u32 {
    let mut score: u32 = 0;
    let client_lower = item.client_name.to_ascii_lowercase();
    let action_lower = item
        .short_action
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    let action_stripped = action_lower.trim_start_matches('_');

    if !exact_names.is_empty() {
        for target in exact_names {
            let target_stripped = target.trim_start_matches('_');
            if client_lower == *target
                || action_lower == *target
                || action_stripped == target_stripped
            {
                score = score.saturating_add(1_000);
            } else if client_lower.ends_with(target) {
                score = score.saturating_add(400);
            }
        }
    }

    if !tokens.is_empty() {
        let mut matched_tokens: u32 = 0;
        for tok in tokens {
            let mut tok_score: u32 = 0;
            if action_lower == *tok || action_stripped == tok {
                tok_score = tok_score.saturating_add(120);
            } else if action_lower.contains(tok) || client_lower.contains(tok) {
                tok_score = tok_score.saturating_add(60);
            }
            if item
                .namespace
                .as_deref()
                .is_some_and(|ns| ns.contains(tok.as_str()))
            {
                tok_score = tok_score.saturating_add(30);
            }
            if item.searchable_lower.contains(tok.as_str()) {
                tok_score = tok_score.saturating_add(15);
            }
            if tok_score > 0 {
                matched_tokens = matched_tokens.saturating_add(1);
                score = score.saturating_add(tok_score);
            }
        }
        if matched_tokens == u32::try_from(tokens.len()).unwrap_or(u32::MAX) {
            score = score.saturating_add(80);
        }
    }

    if exact_names.is_empty() && tokens.is_empty() {
        score = 1;
    }
    score
}
