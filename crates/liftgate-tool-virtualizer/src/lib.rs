//! Zero-cost, trait-driven 3-tier MCP tool virtualization for `agy acp`.
//!
//! This crate contains zero async runtime, HTTP, or OS I/O dependencies. Each coding harness
//! implements [`HarnessToolProfile`] as a zero-sized unit struct (`Copy + Default + 'static`),
//! monomorphized into [`plan_virtualization`] to keep `lg` loopback MCP tool exposure strictly
//! within `agy acp 1.5.18`'s `~55–58 KiB` Eager wire byte budget while enabling 1:1 passthrough
//! up to `36` tools, 0-RTT in-memory schema search, and required-parameter preflight self-healing.

pub mod budget;
pub mod planner;
pub mod profile;
pub mod profiles;
pub mod router;
pub mod schema;

pub use budget::VirtualizerBudget;
pub use planner::{
    McpSessionCatalog, McpSessionOptions, partition_tools_for_harness, partition_tools_for_mcp,
    partition_tools_for_mcp_session, partition_tools_for_mcp_session_with_options,
    partition_tools_for_mcp_with_hint, plan_virtualization, plan_virtualization_with_options,
    plan_virtualization_with_session_options,
};
pub use profile::{
    DetectedHarness, HarnessToolProfile, ProtocolHint, ToolPlacement, V4aPatchContract,
    default_apply_patch_v4a_contract, detect_harness, is_omp_input_edit_spec,
    is_opencode_v4a_patch_spec,
};
pub use profiles::{
    ClaudeCodeToolProfile, CodexToolProfile, GenericToolProfile, OmpToolProfile,
    OpenCodeToolProfile,
};
pub use router::{CatalogDescription, LocalReplyKind, McpRoutingTable, VirtualizedCallDecision};
pub use schema::{
    McpBridgedTool, McpWireTool, apply_patch_input_schema, code_mode_exec_input_schema,
    coerce_stringified_container_fields, estimate_wire_tool_bytes, extract_custom_tool_input,
    freeform_custom_input_schema, gateway_web_fetch_input_schema, normalize_apply_patch_text,
    normalize_mcp_object_schema, read_context_spill_input_schema, sanitize_mcp_tool_name,
    strip_call_prefix, validate_apply_patch_preflight,
};

/// Default hard ceiling on MCP tools exposed per session on `lg`.
pub const MAX_NATIVE_MCP_TOOLS: usize = VirtualizerBudget::DEFAULT_AGY_ACP.max_eager_tools;

/// Gateway-local 0-RTT search tool mounted on `lg` (`mcp_lg_search_client_tools`).
pub const SEARCH_CLIENT_TOOLS_MCP_NAME: &str = "search_client_tools";

/// Universal dispatcher MCP tool mounted on `lg` (`mcp_lg_call_client_tool`).
pub const CALL_CLIENT_TOOL_MCP_NAME: &str = "call_client_tool";

/// Server-managed URL fetch MCP tool mounted on `lg` (`mcp_lg_gateway_web_fetch`).
pub const GATEWAY_WEB_FETCH_MCP_NAME: &str = "gateway_web_fetch";

/// Gateway-local 0-RTT context spill reader MCP tool mounted on `lg` (`mcp_lg_read_context_spill`).
pub const READ_CONTEXT_SPILL_MCP_NAME: &str = "read_context_spill";

/// Prefix for namespace action hub MCP tools (`mcp_lg_ns__<namespace>`).
pub const NAMESPACE_HUB_MCP_PREFIX: &str = "ns__";

#[cfg(test)]
mod tests;
