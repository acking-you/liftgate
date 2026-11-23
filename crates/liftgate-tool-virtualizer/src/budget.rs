//! Validated budget constants for `agy acp` Eager MCP tool exposure.

/// Wire and search budget governing how a [`crate::HarnessToolProfile`] partitions tools
/// onto the `lg` loopback MCP server.
///
/// Upstream `agy acp` enforces a hard `50`-tool Eager ceiling across *all* attached MCP servers
/// and sorts candidates ascending by definition byte length when trimming excess tools into `Lazy`.
/// Keeping `max_eager_tools <= 24` and `max_eager_wire_bytes <= 28 KiB` leaves `26+` Eager slots
/// for host-side plugins while guaranteeing large orchestrators (`exec`, `exec_command`, `Bash`,
/// `REPL`, `Agent`) are never evicted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtualizerBudget {
    /// Hard ceiling on total MCP tools exposed in `lg` `tools/list` (including hubs + dispatchers).
    pub max_eager_tools: usize,
    /// Soft wire byte cap (names + descriptions + JSON schemas) used during slack-slot backfill.
    pub max_eager_wire_bytes: usize,
    /// Maximum eligible tools for 1:1 zero-virtualization passthrough mode.
    pub direct_passthrough_max_tools: usize,
    /// Maximum cumulative non-orchestrator wire bytes for 1:1 passthrough mode.
    pub direct_passthrough_max_non_orchestrator_bytes: usize,
    /// Maximum Tier-0 + Tier-1 core coding tools kept 1:1 before allocating namespace hubs.
    pub max_direct_core_tools: usize,
    /// Maximum actions in a namespace hub before switching from inline `"enum"` to 0-RTT search.
    pub small_hub_enum_max_actions: usize,
    /// Maximum description byte budget preserved for Tier-0 Code Mode orchestrators (`exec`, `REPL`).
    pub max_orchestrator_desc_bytes: usize,
    /// Maximum description byte budget for standard non-orchestrator tools on the wire.
    pub max_standard_desc_bytes: usize,
    /// Default number of matches returned by `search_client_tools`.
    pub default_search_limit: usize,
    /// Hard ceiling on matches returned by a single `search_client_tools` call.
    pub max_search_limit: usize,
    /// Maximum total UTF-8 byte budget of a formatted `search_client_tools` reply.
    pub max_search_reply_bytes: usize,
    /// Maximum UTF-8 bytes emitted for a single tool's `inputSchema` inside search/preflight text.
    pub max_single_schema_in_search_bytes: usize,
}

impl VirtualizerBudget {
    /// Default `agy acp`-safe budget shared across general harnesses.
    pub const DEFAULT_AGY_ACP: Self = Self {
        max_eager_tools: 24,
        max_eager_wire_bytes: 28 * 1024,
        direct_passthrough_max_tools: 20,
        direct_passthrough_max_non_orchestrator_bytes: 22 * 1024,
        max_direct_core_tools: 12,
        small_hub_enum_max_actions: 12,
        max_orchestrator_desc_bytes: 64 * 1024,
        max_standard_desc_bytes: 8 * 1024,
        default_search_limit: 8,
        max_search_limit: 20,
        max_search_reply_bytes: 32 * 1024,
        max_single_schema_in_search_bytes: 12 * 1024,
    };

    /// Budget tuned for OpenAI Codex CLI / Codex App (`exec` + PTY pair + many `codex_apps` hubs).
    pub const CODEX_AGY_ACP: Self = Self {
        max_direct_core_tools: 10,
        ..Self::DEFAULT_AGY_ACP
    };

    /// Budget tuned for Anthropic Claude Code (`Bash`/`Read`/`Edit`/`Write`/`Grep`/`Glob`/`Agent`
    /// + PascalCase built-in hubs + MCP plugins).
    pub const CLAUDE_CODE_AGY_ACP: Self = Self {
        max_direct_core_tools: 13,
        ..Self::DEFAULT_AGY_ACP
    };

    /// Budget tuned for OpenCode (`execute` `@opencode/codemode` orchestrator + lowercase core
    /// coding primitives + `opencode_*`/`automation_*`/`<server>_*` hubs).
    pub const OPENCODE_AGY_ACP: Self = Self {
        max_direct_core_tools: 12,
        ..Self::DEFAULT_AGY_ACP
    };

    /// Budget tuned for Oh My Pi (`omp` — `eval` polyglot Python/Bun Code Mode kernel + `wait`
    /// + 14-18 default built-in tools in 1:1 passthrough + `mcp__<server>_<tool>` hubs).
    pub const OMP_AGY_ACP: Self = Self {
        max_eager_wire_bytes: 34 * 1024,
        direct_passthrough_max_tools: 21,
        direct_passthrough_max_non_orchestrator_bytes: 30 * 1024,
        max_direct_core_tools: 13,
        ..Self::DEFAULT_AGY_ACP
    };

    /// Compile-time invariant check relating all budget thresholds.
    pub const fn assert_valid(&self) {
        assert!(
            self.max_eager_tools >= 4 && self.max_eager_tools <= 40,
            "max_eager_tools must leave room for dispatchers while staying below agy acp's 50-tool ceiling"
        );
        assert!(
            self.direct_passthrough_max_tools <= self.max_eager_tools,
            "direct passthrough tool threshold must fit within max_eager_tools"
        );
        assert!(
            self.max_direct_core_tools + 2 <= self.max_eager_tools,
            "core direct tools plus search and universal dispatcher must fit within max_eager_tools"
        );
        assert!(
            self.direct_passthrough_max_non_orchestrator_bytes <= self.max_eager_wire_bytes,
            "passthrough byte threshold must not exceed eager wire byte cap"
        );
        assert!(
            self.default_search_limit >= 1 && self.default_search_limit <= self.max_search_limit,
            "default_search_limit must lie within 1..=max_search_limit"
        );
        assert!(
            self.max_single_schema_in_search_bytes <= self.max_search_reply_bytes,
            "single schema snippet cap must fit inside total search reply cap"
        );
    }
}

const _: () = VirtualizerBudget::DEFAULT_AGY_ACP.assert_valid();
const _: () = VirtualizerBudget::CODEX_AGY_ACP.assert_valid();
const _: () = VirtualizerBudget::CLAUDE_CODE_AGY_ACP.assert_valid();
const _: () = VirtualizerBudget::OPENCODE_AGY_ACP.assert_valid();
const _: () = VirtualizerBudget::OMP_AGY_ACP.assert_valid();
