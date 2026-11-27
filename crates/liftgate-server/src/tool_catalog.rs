//! Session tool catalog served at `/api/telemetry/sessions/{session_id}/tool-catalog`.

use liftgate_types::{
    session_key::orchestrator_contract_rules,
    telemetry::{ContextSegment, ContextSegmentKey, SessionToolCatalog},
};

use crate::{mcp_bridge::McpSessionHandle, telemetry::SessionContextSample};

fn tokens(bytes: usize) -> u64 {
    u64::try_from(bytes.saturating_add(3) / 4).unwrap_or(u64::MAX)
}

/// Combines the live virtualization plan of `handle` with the session's latest context sample.
#[must_use]
pub fn build_session_tool_catalog(
    session_id: &str,
    handle: &McpSessionHandle,
    model_id: String,
    sample: Option<SessionContextSample>,
) -> SessionToolCatalog {
    let catalog = handle.catalog();
    let described = handle.describe_catalog();
    let contract = catalog.orchestrator_contract();
    let orchestrator_contract_prompt = contract.map(orchestrator_contract_rules);
    let bridge_rules_prompt = handle.bridge_rules().map(str::to_owned);

    let contract_bytes = orchestrator_contract_prompt.as_deref().map_or(0, str::len);
    let rules_bytes = bridge_rules_prompt
        .as_deref()
        .map_or(0, str::len)
        .saturating_sub(contract_bytes);
    let wire_bytes = described.wire_tools.iter().map(|w| w.bytes).sum::<usize>();
    let segment = |key, bytes| ContextSegment {
        key,
        bytes,
        tokens_est: tokens(bytes),
    };
    let mut segments = vec![
        segment(ContextSegmentKey::WireTools, wire_bytes),
        segment(ContextSegmentKey::BridgeRules, rules_bytes),
        segment(ContextSegmentKey::OrchestratorContract, contract_bytes),
    ];
    let captured_at_unix_ms = match sample {
        Some(sample) => {
            segments.extend(sample.segments);
            sample.captured_at_unix_ms
        }
        None => 0,
    };

    SessionToolCatalog {
        session_id: session_id.to_owned(),
        harness_profile: catalog.profile_id.to_owned(),
        orchestrator_mode: contract.map(|c| c.mode_id().to_owned()),
        is_passthrough: catalog.is_passthrough,
        model_id,
        captured_at_unix_ms,
        wire_budget_bytes: described.wire_budget_bytes,
        eager_tool_budget: described.eager_tool_budget,
        client_tools_bytes: described.client_tools_bytes,
        segments,
        wire_tools: described.wire_tools,
        tools: described.tools,
        bridge_rules_prompt,
        orchestrator_contract_prompt,
        search_index_entries: described.search_index_entries,
    }
}
