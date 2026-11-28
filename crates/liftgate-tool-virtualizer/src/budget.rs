//! Validated budget constants for `agy acp` Eager MCP tool exposure.

/// Wire target mode for tool virtualization (`mcp_lg_*` loopback MCP vs unprefixed native HTTP API).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VirtualizationTargetMode {
    /// Loopback MCP server mounted on `agy acp` / ACP sessions (`mcp_lg_*` wire prefix).
    #[default]
    AcpLoopbackMcp,
    /// Native Platform HTTP API `tools` array (no `mcp_lg_` prefix; local 0-RTT SSE continuation).
    NativeApiTools,
}

impl VirtualizationTargetMode {
    /// Tool name prefix injected by the upstream host when invoking virtualized tools.
    #[must_use]
    pub const fn wire_prefix(self) -> &'static str {
        match self {
            Self::AcpLoopbackMcp => "mcp_lg_",
            Self::NativeApiTools => "",
        }
    }
}

/// Wire and search budget governing how a [`crate::HarnessToolProfile`] partitions tools
/// onto the `lg` loopback MCP server.
///
/// Empirical black-box probes on `agy acp 1.5.18` showed that the legacy `50`-tool count cap no
/// longer applies (`120` uniform tools at `~48 KB` stay `100% Eager`); instead, `agy acp` enforces
/// a **`~55–58 KiB` serialized byte budget** across attached MCP servers, packing tools ascending
/// by definition byte length and **silently dropping** (`lazy: 0`) oversized tools past the cliff.
/// Setting `max_eager_tools = 40`, `direct_passthrough_max_tools = 36`, and `max_eager_wire_bytes = 34 KiB`
/// lets medium-sized plugin sessions (`<= 36` tools, `<= 28 KiB`) run in 1:1 zero-virtualization
/// passthrough while keeping `21+ KiB` of headroom below the silent-drop cliff so large
/// orchestrators (`exec`, `execute`, `eval`, `Bash`) are never dropped.
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
    /// Default `agy acp 1.5.18`-calibrated budget shared across general harnesses.
    pub const DEFAULT_AGY_ACP: Self = Self {
        max_eager_tools: 40,
        max_eager_wire_bytes: 34 * 1024,
        direct_passthrough_max_tools: 36,
        direct_passthrough_max_non_orchestrator_bytes: 28 * 1024,
        max_direct_core_tools: 18,
        small_hub_enum_max_actions: 12,
        max_orchestrator_desc_bytes: 64 * 1024,
        max_standard_desc_bytes: 8 * 1024,
        default_search_limit: 8,
        max_search_limit: 20,
        max_search_reply_bytes: 32 * 1024,
        max_single_schema_in_search_bytes: 12 * 1024,
    };

    /// Budget tuned for OpenAI Codex CLI / Codex App (`exec` + PTY pair + `apply_patch` + `codex_apps` hubs).
    pub const CODEX_AGY_ACP: Self = Self {
        max_direct_core_tools: 16,
        ..Self::DEFAULT_AGY_ACP
    };

    /// Budget tuned for Anthropic Claude Code (`Bash`/`Read`/`Edit`/`Write`/`Grep`/`Glob`/`Agent`
    /// + `AskUserQuestion`/`Skill` + PascalCase built-in hubs + MCP plugins).
    pub const CLAUDE_CODE_AGY_ACP: Self = Self {
        max_direct_core_tools: 18,
        ..Self::DEFAULT_AGY_ACP
    };

    /// Budget tuned for OpenCode (`execute` `@opencode/codemode` orchestrator + lowercase core
    /// coding primitives + `opencode_*`/`automation_*`/`<server>_*` hubs).
    pub const OPENCODE_AGY_ACP: Self = Self {
        max_direct_core_tools: 18,
        ..Self::DEFAULT_AGY_ACP
    };

    /// Budget tuned for Oh My Pi (`omp` — `eval` polyglot Python/Bun Code Mode kernel + `wait`
    /// + built-in tools in 1:1 passthrough + `mcp__<server>_<tool>` hubs).
    pub const OMP_AGY_ACP: Self = Self {
        direct_passthrough_max_non_orchestrator_bytes: 30 * 1024,
        max_direct_core_tools: 18,
        ..Self::DEFAULT_AGY_ACP
    };

    /// Budget tuned for official stateless Platform HTTP APIs (OpenAI, Anthropic, Google Gemini).
    ///
    /// Keeps normal plugin sessions (`<= 64` tools and `<= 72 KiB` non-orchestrator schema bytes)
    /// in pure 1:1 native passthrough with zero synthetic tools, while folding massive MCP plugin
    /// suites (`100+` tools / `150+ KB`) into unprefixed `ns__*` hubs + `search_client_tools`.
    pub const NATIVE_PLATFORM_API: Self = Self {
        max_eager_tools: 80,
        max_eager_wire_bytes: 96 * 1024,
        direct_passthrough_max_tools: 64,
        direct_passthrough_max_non_orchestrator_bytes: 72 * 1024,
        max_direct_core_tools: 24,
        small_hub_enum_max_actions: 16,
        max_orchestrator_desc_bytes: 96 * 1024,
        max_standard_desc_bytes: 16 * 1024,
        default_search_limit: 8,
        max_search_limit: 20,
        max_search_reply_bytes: 48 * 1024,
        max_single_schema_in_search_bytes: 16 * 1024,
    };

    /// Compile-time invariant check relating all budget thresholds.
    pub const fn assert_valid(&self) {
        assert!(
            self.max_eager_tools >= 4 && self.max_eager_tools <= 48,
            "max_eager_tools must leave room for dispatchers and host MCP plugins"
        );
        assert!(
            self.max_eager_wire_bytes <= 40 * 1024,
            "max_eager_wire_bytes must stay well below agy acp 1.5.18's ~55 KiB silent-drop cliff"
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

    /// Compile-time / runtime invariant check for budgets targeting `mode`.
    pub const fn assert_valid_for_mode(&self, mode: VirtualizationTargetMode) {
        match mode {
            VirtualizationTargetMode::AcpLoopbackMcp => self.assert_valid(),
            VirtualizationTargetMode::NativeApiTools => {
                assert!(
                    self.max_eager_tools >= 4 && self.max_eager_tools <= 256,
                    "native API max_eager_tools must lie within 4..=256"
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
                    self.default_search_limit >= 1
                        && self.default_search_limit <= self.max_search_limit,
                    "default_search_limit must lie within 1..=max_search_limit"
                );
                assert!(
                    self.max_single_schema_in_search_bytes <= self.max_search_reply_bytes,
                    "single schema snippet cap must fit inside total search reply cap"
                );
            }
        }
    }
}

const _: () = VirtualizerBudget::DEFAULT_AGY_ACP.assert_valid();
const _: () = VirtualizerBudget::CODEX_AGY_ACP.assert_valid();
const _: () = VirtualizerBudget::CLAUDE_CODE_AGY_ACP.assert_valid();
const _: () = VirtualizerBudget::OPENCODE_AGY_ACP.assert_valid();
const _: () = VirtualizerBudget::OMP_AGY_ACP.assert_valid();
const _: () = VirtualizerBudget::NATIVE_PLATFORM_API
    .assert_valid_for_mode(VirtualizationTargetMode::NativeApiTools);
