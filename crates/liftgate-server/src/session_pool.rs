use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use liftgate_types::{
    acp::{AcpUsage, compute_cache_hit_bps},
    canonical::{CanonicalMessage, CanonicalToolSpec},
    config::{GatewayLimits, ModelId, SessionId},
    openai::ResponseObject,
    session_key::ConversationHash,
    telemetry::{
        PromptCacheWarmth, SessionMcpSummary, SessionPoolTelemetry, SessionSlotState,
        SessionSummary,
    },
};
use tokio::sync::{oneshot, watch};

use crate::{
    acp_client::AcpBackend,
    mcp_bridge::{McpSessionHandle, McpToolCallReply},
    telemetry::unix_timestamp_ms,
};

const MAX_IDLE_RETAINED_CAP_BYTES: usize = 192 * 1024 * 1024;
const MAX_STORED_RESPONSES_CAP_BYTES: usize = 64 * 1024 * 1024;

/// Default maximum idle age (`45 minutes`) before an untouched `--process-per-session` ACP
/// worker is closed by the background reaper to reclaim resident child memory once upstream
/// prompt caches have long expired.
pub const DEFAULT_MAX_IDLE_SESSION_AGE_MS: u64 = 45 * 60 * 1_000;

/// Cached metadata for an idle ACP session awaiting a multi-turn continuation.
#[derive(Clone, Debug)]
pub struct IdleSessionEntry {
    pub session_id: SessionId,
    pub tool_nonce: Arc<str>,
    pub model_id: ModelId,
    pub mcp_handle: Option<Arc<McpSessionHandle>>,
    pub post_turn_hash: ConversationHash,
    pub alt_post_turn_hash: Option<ConversationHash>,
    pub post_turn_dialogue_hash: ConversationHash,
    pub declared_tools: Vec<CanonicalToolSpec>,
    pub last_context_used: u64,
    pub total_prompt_tokens: u64,
    pub total_cached_read_tokens: u64,
    pub last_response_id: Option<String>,
    pub accumulated_messages: Vec<CanonicalMessage>,
    pub message_count: usize,
    pub turn_count: u64,
    pub created_at_unix_ms: u64,
    pub last_active_unix_ms: u64,
    pub(crate) retained_bytes: usize,
}

impl IdleSessionEntry {
    /// Estimates total heap + structural bytes retained while cached in `SessionPool`.
    #[must_use]
    pub fn estimated_retained_bytes(&self) -> usize {
        let tool_bytes = self.declared_tools.iter().fold(0usize, |acc, t| {
            acc.saturating_add(t.estimated_heap_bytes())
        });
        let msg_bytes = self.accumulated_messages.iter().fold(0usize, |acc, m| {
            acc.saturating_add(m.estimated_heap_bytes())
        });
        let mcp_bytes = self
            .mcp_handle
            .as_deref()
            .map_or(0, McpSessionHandle::estimated_retained_bytes);

        std::mem::size_of::<Self>()
            .saturating_add(self.session_id.as_str().len())
            .saturating_add(self.tool_nonce.len())
            .saturating_add(self.model_id.as_str().len())
            .saturating_add(self.last_response_id.as_deref().map_or(0, str::len))
            .saturating_add(tool_bytes)
            .saturating_add(msg_bytes)
            .saturating_add(mcp_bytes)
    }
}

/// Metadata for a single pending client tool call parked inside a suspended MCP `session/prompt`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingMcpCallInfo {
    /// Unprefixed call suffix matched against `call_<suffix>` or `toolu_<suffix>`.
    pub nonce_suffix: String,
    /// Canonical `call_<suffix>` identifier recorded in `CanonicalPart::ToolCall`.
    pub call_id: String,
    /// Caller-declared tool name (e.g. `"exec"`, `"wait"`, `"Bash"`).
    pub client_name: String,
}

/// Resumed HTTP step input handed from `execute_normalized_turn` into the parked MCP session driver.
pub(crate) struct McpResumedStepInput {
    pub raw_lease: RawSessionLease,
    pub task_guard: crate::telemetry::ActiveTaskGuard,
    pub req: liftgate_types::canonical::NormalizedTurnRequest,
    pub web_cfg: liftgate_types::session_key::WebToolBridgeConfig,
    pub full_messages: Vec<CanonicalMessage>,
    pub route_meta: crate::turn_runner::TurnRoutingMetadata,
    pub outbound_diag_blocks: Arc<[liftgate_types::acp::AcpPromptBlock]>,
    pub response_id: String,
    pub tool_replies: Vec<(String, McpToolCallReply)>,
    pub vision_offload_usage: AcpUsage,
    pub events_tx: tokio::sync::mpsc::Sender<crate::turn_runner::TurnEvent>,
    pub step_cancel: tokio_util::sync::CancellationToken,
    pub spill_store: Option<crate::spill_store::SpillStore>,
    pub retire_after_turn: bool,
    pub timeout_secs: u64,
}

/// Outcome delivered when a parked `McpSessionDriver` finishes unwinding via `session/cancel`.
pub(crate) type McpUnwindOutcome = Result<(u64, AcpUsage, Arc<McpSessionHandle>), ()>;

/// Control message sent to a parked `McpSessionDriver` waiting for client tool execution.
pub(crate) enum McpDriverCommand {
    ResumeStep(Box<McpResumedStepInput>),
    CancelForUnwind {
        done_tx: oneshot::Sender<McpUnwindOutcome>,
    },
}

/// Parked ACP session whose `session/prompt` is actively suspended inside a loopback MCP `tools/call`.
pub struct SuspendedMcpEntry {
    pub session_id: SessionId,
    pub tool_nonce: Arc<str>,
    pub model_id: ModelId,
    pub mcp_handle: Arc<McpSessionHandle>,
    pub pending_calls: Vec<PendingMcpCallInfo>,
    pub(crate) cmd_tx: oneshot::Sender<McpDriverCommand>,
    pub post_turn_hash: ConversationHash,
    pub post_turn_dialogue_hash: ConversationHash,
    pub pre_step_hash: ConversationHash,
    pub pre_step_dialogue_hash: ConversationHash,
    pub alt_pre_user_hash: Option<ConversationHash>,
    pub declared_tools: Vec<CanonicalToolSpec>,
    pub last_context_used: u64,
    pub total_prompt_tokens: u64,
    pub total_cached_read_tokens: u64,
    pub last_response_id: Option<String>,
    pub accumulated_messages: Vec<CanonicalMessage>,
    pub message_count: usize,
    pub pre_step_message_count: usize,
    pub turn_count: u64,
    pub created_at_unix_ms: u64,
    pub last_active_unix_ms: u64,
    retained_bytes: usize,
}

impl std::fmt::Debug for SuspendedMcpEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SuspendedMcpEntry")
            .field("session_id", &self.session_id)
            .field("model_id", &self.model_id)
            .field("pending_calls", &self.pending_calls)
            .field("turn_count", &self.turn_count)
            .finish_non_exhaustive()
    }
}

impl SuspendedMcpEntry {
    #[must_use]
    pub fn estimated_retained_bytes(&self) -> usize {
        let tool_bytes = self.declared_tools.iter().fold(0usize, |acc, t| {
            acc.saturating_add(t.estimated_heap_bytes())
        });
        let msg_bytes = self.accumulated_messages.iter().fold(0usize, |acc, m| {
            acc.saturating_add(m.estimated_heap_bytes())
        });
        let pending_bytes = self.pending_calls.iter().fold(0usize, |acc, p| {
            acc.saturating_add(p.nonce_suffix.len() + p.call_id.len() + p.client_name.len())
        });

        std::mem::size_of::<Self>()
            .saturating_add(self.session_id.as_str().len())
            .saturating_add(self.tool_nonce.len())
            .saturating_add(self.model_id.as_str().len())
            .saturating_add(self.last_response_id.as_deref().map_or(0, str::len))
            .saturating_add(tool_bytes)
            .saturating_add(msg_bytes)
            .saturating_add(pending_bytes)
            .saturating_add(self.mcp_handle.estimated_retained_bytes())
    }
}

/// State extracted when promoting a [`SuspendedCheckoutGuard`] into a live continuation step.
pub(crate) struct PromotedMcpSession {
    pub cmd_tx: oneshot::Sender<McpDriverCommand>,
    pub pending_calls: Vec<PendingMcpCallInfo>,
    pub declared_tools: Vec<CanonicalToolSpec>,
    pub accumulated_messages: Vec<CanonicalMessage>,
    pub last_context_used: u64,
    pub post_turn_hash: ConversationHash,
}

/// Payload passed to [`SessionLeaseGuard::suspend_on_mcp`] when parking an active step on MCP.
pub(crate) struct SuspendedMcpTurn {
    pub mcp_handle: Arc<McpSessionHandle>,
    pub pending_calls: Vec<PendingMcpCallInfo>,
    pub cmd_tx: oneshot::Sender<McpDriverCommand>,
    pub post_turn_hash: ConversationHash,
    pub post_turn_dialogue_hash: ConversationHash,
    pub pre_step_hash: ConversationHash,
    pub pre_step_dialogue_hash: ConversationHash,
    pub alt_pre_user_hash: Option<ConversationHash>,
    pub declared_tools: Vec<CanonicalToolSpec>,
    pub last_context_used: u64,
    pub step_prompt_tokens: u64,
    pub step_cached_read_tokens: u64,
    pub message_count: usize,
    pub pre_step_message_count: usize,
    pub last_response_id: Option<String>,
    pub accumulated_messages: Vec<CanonicalMessage>,
}

#[derive(Clone, Debug)]
struct ActiveLeaseMeta {
    model_id: ModelId,
    in_flight_hashes: Vec<ConversationHash>,
    turn_count: u64,
    message_count: usize,
    last_context_used: u64,
    total_prompt_tokens: u64,
    total_cached_read_tokens: u64,
    created_at_unix_ms: u64,
    last_active_unix_ms: u64,
    last_response_id: Option<String>,
    mcp_summary: Option<SessionMcpSummary>,
}

struct StoredResponseEntry {
    id: String,
    retained_bytes: usize,
    raw_json: Bytes,
}

/// Bounded LRU pool of idle ACP sessions, active leases, and recent OpenAI `ResponseObject` records.
pub struct SessionPool {
    limits: GatewayLimits,
    max_idle_retained_bytes: usize,
    max_stored_response_bytes: usize,
    lifecycle_seq: watch::Sender<u64>,
    inner: Mutex<PoolInner>,
}

struct PoolInner {
    active_leases: BTreeMap<String, ActiveLeaseMeta>,
    suspended_sessions: VecDeque<SuspendedMcpEntry>,
    idle_sessions: VecDeque<IdleSessionEntry>,
    pending_checkins: Vec<(u64, Vec<ConversationHash>)>,
    next_pending_id: u64,
    retained_idle_bytes: usize,
    stored_responses: VecDeque<StoredResponseEntry>,
    retained_response_bytes: usize,
    mcp_continuations: u64,
    total_sessions_created: u64,
    total_sessions_reused: u64,
    total_sessions_closed_or_evicted: u64,
    last_activity_unix_ms: u64,
}

impl PoolInner {
    fn purge_closed_suspended(&mut self) {
        let mut kept = VecDeque::with_capacity(self.suspended_sessions.len());
        while let Some(entry) = self.suspended_sessions.pop_front() {
            if entry.cmd_tx.is_closed() {
                self.retained_idle_bytes = self
                    .retained_idle_bytes
                    .saturating_sub(entry.retained_bytes);
                self.total_sessions_closed_or_evicted += 1;
            } else {
                kept.push_back(entry);
            }
        }
        self.suspended_sessions = kept;
    }

    fn cached_slot_count(&self) -> usize {
        self.idle_sessions
            .len()
            .saturating_add(self.suspended_sessions.len())
    }

    fn evict_oldest_cached_slot(&mut self) -> Option<SessionId> {
        let evict_idle = match (self.idle_sessions.front(), self.suspended_sessions.front()) {
            (Some(idle), Some(susp)) => idle.last_active_unix_ms <= susp.last_active_unix_ms,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => return None,
        };
        if evict_idle {
            let old = self.idle_sessions.pop_front()?;
            self.retained_idle_bytes = self.retained_idle_bytes.saturating_sub(old.retained_bytes);
            self.total_sessions_closed_or_evicted += 1;
            Some(old.session_id)
        } else {
            let old = self.suspended_sessions.pop_front()?;
            self.retained_idle_bytes = self.retained_idle_bytes.saturating_sub(old.retained_bytes);
            self.total_sessions_closed_or_evicted += 1;
            Some(old.session_id)
        }
    }
}

impl SessionPool {
    #[must_use]
    pub fn new(limits: GatewayLimits) -> Self {
        let max_body = limits.max_body_bytes().get();
        let max_idle_retained_bytes = max_body
            .saturating_mul(6)
            .min(MAX_IDLE_RETAINED_CAP_BYTES)
            .max(max_body);
        let max_stored_response_bytes = max_body
            .saturating_mul(2)
            .min(MAX_STORED_RESPONSES_CAP_BYTES)
            .max(max_body);
        let (lifecycle_seq, _) = watch::channel(0u64);

        Self {
            limits,
            max_idle_retained_bytes,
            max_stored_response_bytes,
            lifecycle_seq,
            inner: Mutex::new(PoolInner {
                active_leases: BTreeMap::new(),
                suspended_sessions: VecDeque::with_capacity(limits.max_idle_sessions().get()),
                idle_sessions: VecDeque::with_capacity(limits.max_idle_sessions().get()),
                pending_checkins: Vec::new(),
                next_pending_id: 1,
                retained_idle_bytes: 0,
                stored_responses: VecDeque::with_capacity(limits.max_stored_responses().get()),
                retained_response_bytes: 0,
                mcp_continuations: 0,
                total_sessions_created: 0,
                total_sessions_reused: 0,
                total_sessions_closed_or_evicted: 0,
                last_activity_unix_ms: unix_timestamp_ms(),
            }),
        }
    }

    /// Global byte ceiling across cached idle ACP sessions.
    #[must_use]
    pub const fn max_idle_retained_bytes(&self) -> usize {
        self.max_idle_retained_bytes
    }

    /// Global byte ceiling across cached `GET /v1/responses/{id}` objects.
    #[must_use]
    pub const fn max_stored_response_bytes(&self) -> usize {
        self.max_stored_response_bytes
    }

    /// Returns `(retained_idle_session_bytes, retained_stored_response_bytes)` currently held.
    #[must_use]
    pub fn retained_memory_bytes(&self) -> (usize, usize) {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        (guard.retained_idle_bytes, guard.retained_response_bytes)
    }

    /// Atomically checks out a suspended MCP session matching `previous_response_id`.
    #[must_use]
    pub fn checkout_suspended_by_response_id<B: AcpBackend>(
        self: &Arc<Self>,
        response_id: &str,
        backend: Arc<B>,
    ) -> Option<SuspendedCheckoutGuard<B>> {
        self.checkout_suspended_where(backend, |e| {
            !e.accumulated_messages.is_empty() && e.last_response_id.as_deref() == Some(response_id)
        })
    }

    /// Atomically checks out a suspended MCP session whose `post_turn_hash` matches `hash`.
    #[must_use]
    pub fn checkout_suspended_by_post_hash<B: AcpBackend>(
        self: &Arc<Self>,
        hash: &ConversationHash,
        backend: Arc<B>,
    ) -> Option<SuspendedCheckoutGuard<B>> {
        self.checkout_suspended_where(backend, |e| &e.post_turn_hash == hash)
    }

    /// Atomically checks out a suspended MCP session whose `pre_step_hash` matches `hash`
    /// (e.g. client cancelled/discarded the pending assistant tool turn and sent a new prompt).
    #[must_use]
    pub fn checkout_suspended_by_pre_hash<B: AcpBackend>(
        self: &Arc<Self>,
        hash: &ConversationHash,
        backend: Arc<B>,
    ) -> Option<SuspendedCheckoutGuard<B>> {
        self.checkout_suspended_where(backend, |e| &e.pre_step_hash == hash)
    }

    /// Atomically checks out a suspended MCP session whose `alt_pre_user_hash` matches `hash`
    /// (e.g. client rolled back both the assistant tool turn and its triggering user message).
    #[must_use]
    pub fn checkout_suspended_by_alt_pre_user_hash<B: AcpBackend>(
        self: &Arc<Self>,
        hash: &ConversationHash,
        backend: Arc<B>,
    ) -> Option<SuspendedCheckoutGuard<B>> {
        self.checkout_suspended_where(backend, |e| e.alt_pre_user_hash.as_ref() == Some(hash))
    }

    /// Atomically checks out a suspended MCP session matching a pure dialogue hash (`/compact`).
    #[must_use]
    pub fn checkout_suspended_by_dialogue_hash<B: AcpBackend>(
        self: &Arc<Self>,
        hash: &ConversationHash,
        backend: Arc<B>,
    ) -> Option<SuspendedCheckoutGuard<B>> {
        self.checkout_suspended_where(backend, |e| {
            &e.post_turn_dialogue_hash == hash || &e.pre_step_dialogue_hash == hash
        })
    }

    fn checkout_suspended_where<B, F>(
        self: &Arc<Self>,
        backend: Arc<B>,
        predicate: F,
    ) -> Option<SuspendedCheckoutGuard<B>>
    where
        B: AcpBackend,
        F: Fn(&SuspendedMcpEntry) -> bool,
    {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.purge_closed_suspended();
        let pos = guard
            .suspended_sessions
            .iter()
            .rposition(|e| !e.cmd_tx.is_closed() && predicate(e))?;
        guard.last_activity_unix_ms = unix_timestamp_ms();
        let entry = guard.suspended_sessions.remove(pos)?;
        guard.retained_idle_bytes = guard
            .retained_idle_bytes
            .saturating_sub(entry.retained_bytes);
        drop(guard);
        Some(SuspendedCheckoutGuard::new(
            entry,
            Arc::clone(self),
            backend,
        ))
    }

    /// Purges a suspended MCP session whose background `session/prompt` exited unexpectedly while parked.
    pub fn remove_dead_suspended(&self, session_id: &SessionId) {
        let removed = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let Some(pos) = guard
                .suspended_sessions
                .iter()
                .position(|e| &e.session_id == session_id)
            else {
                return;
            };
            guard.total_sessions_closed_or_evicted += 1;
            let entry = guard.suspended_sessions.remove(pos);
            if let Some(ref e) = entry {
                guard.retained_idle_bytes =
                    guard.retained_idle_bytes.saturating_sub(e.retained_bytes);
            }
            entry.is_some()
        };
        if removed {
            self.bump_lifecycle_seq();
        }
    }

    /// Atomically removes an idle session matching `previous_response_id` wrapped in a
    /// cancel-safe [`IdleCheckoutGuard`].
    #[must_use]
    pub fn checkout_by_response_id<B: AcpBackend>(
        self: &Arc<Self>,
        response_id: &str,
        backend: Arc<B>,
    ) -> Option<IdleCheckoutGuard<B>> {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let pos = guard.idle_sessions.iter().rposition(|e| {
            !e.accumulated_messages.is_empty() && e.last_response_id.as_deref() == Some(response_id)
        })?;
        guard.total_sessions_reused += 1;
        guard.last_activity_unix_ms = unix_timestamp_ms();
        let entry = guard.idle_sessions.remove(pos)?;
        guard.retained_idle_bytes = guard
            .retained_idle_bytes
            .saturating_sub(entry.retained_bytes);
        drop(guard);
        Some(IdleCheckoutGuard::new(entry, Arc::clone(self), backend))
    }

    /// Atomically removes an idle session matching a canonical prefix hash wrapped in a
    /// cancel-safe [`IdleCheckoutGuard`].
    #[must_use]
    pub fn checkout_by_hash<B: AcpBackend>(
        self: &Arc<Self>,
        hash: &ConversationHash,
        backend: Arc<B>,
    ) -> Option<IdleCheckoutGuard<B>> {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let pos = guard
            .idle_sessions
            .iter()
            .rposition(|e| &e.post_turn_hash == hash)?;
        guard.total_sessions_reused += 1;
        guard.last_activity_unix_ms = unix_timestamp_ms();
        let entry = guard.idle_sessions.remove(pos)?;
        guard.retained_idle_bytes = guard
            .retained_idle_bytes
            .saturating_sub(entry.retained_bytes);
        drop(guard);
        Some(IdleCheckoutGuard::new(entry, Arc::clone(self), backend))
    }

    /// Atomically removes an idle session whose secondary `alt_post_turn_hash` matches `hash`
    /// (for example when a prior turn was cancelled mid-flight and the client harness rolled back
    /// the cancelled turn's user message before submitting a replacement prompt).
    #[must_use]
    pub fn checkout_by_alt_hash<B: AcpBackend>(
        self: &Arc<Self>,
        hash: &ConversationHash,
        backend: Arc<B>,
    ) -> Option<IdleCheckoutGuard<B>> {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let pos = guard
            .idle_sessions
            .iter()
            .rposition(|e| e.alt_post_turn_hash.as_ref() == Some(hash))?;
        guard.total_sessions_reused += 1;
        guard.last_activity_unix_ms = unix_timestamp_ms();
        let entry = guard.idle_sessions.remove(pos)?;
        guard.retained_idle_bytes = guard
            .retained_idle_bytes
            .saturating_sub(entry.retained_bytes);
        drop(guard);
        Some(IdleCheckoutGuard::new(entry, Arc::clone(self), backend))
    }

    /// Registers an in-flight soft-cancelled session drain so an immediate follow-up turn
    /// arriving within milliseconds waits briefly for `session/cancel` to finish instead of
    /// spawning a duplicate cold session.
    #[must_use]
    pub fn register_pending_checkin(
        self: &Arc<Self>,
        hashes: Vec<ConversationHash>,
    ) -> PendingCheckinGuard {
        let id = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let id = guard.next_pending_id;
            guard.next_pending_id = guard.next_pending_id.wrapping_add(1);
            guard.pending_checkins.push((id, hashes));
            id
        };
        PendingCheckinGuard {
            id,
            pool: Arc::clone(self),
        }
    }

    /// Waits up to `max_wait` if a warm session matching any hash in `candidates` is currently
    /// active or completing a non-destructive `session/cancel` drain.
    pub async fn wait_for_pending_checkin(
        &self,
        candidates: &[ConversationHash],
        max_wait: Duration,
    ) {
        if candidates.is_empty() {
            return;
        }
        let deadline = tokio::time::Instant::now() + max_wait;
        let mut rx = self.lifecycle_seq.subscribe();
        loop {
            {
                let guard = match self.inner.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let _ = *rx.borrow_and_update();
                let already_idle = guard.idle_sessions.iter().any(|e| {
                    candidates.contains(&e.post_turn_hash)
                        || e.alt_post_turn_hash
                            .as_ref()
                            .is_some_and(|alt| candidates.contains(alt))
                        || candidates.contains(&e.post_turn_dialogue_hash)
                }) || guard.suspended_sessions.iter().any(|e| {
                    !e.cmd_tx.is_closed()
                        && (candidates.contains(&e.post_turn_hash)
                            || candidates.contains(&e.pre_step_hash)
                            || e.alt_pre_user_hash
                                .as_ref()
                                .is_some_and(|alt| candidates.contains(alt))
                            || candidates.contains(&e.post_turn_dialogue_hash)
                            || candidates.contains(&e.pre_step_dialogue_hash))
                });
                if already_idle {
                    return;
                }
                let in_flight_or_draining = guard
                    .pending_checkins
                    .iter()
                    .any(|(_, hashes)| hashes.iter().any(|h| candidates.contains(h)))
                    || guard
                        .active_leases
                        .values()
                        .any(|meta| meta.in_flight_hashes.iter().any(|h| candidates.contains(h)));
                if !in_flight_or_draining {
                    return;
                }
            }

            if !matches!(
                tokio::time::timeout_at(deadline, rx.changed()).await,
                Ok(Ok(()))
            ) {
                return;
            }
        }
    }

    /// Atomically removes an idle session matching a pure dialogue hash wrapped in a
    /// cancel-safe [`IdleCheckoutGuard`]
    /// (used to run `/compact` summarization against the warm pre-compaction ACP session).
    #[must_use]
    pub fn checkout_by_dialogue_hash<B: AcpBackend>(
        self: &Arc<Self>,
        hash: &ConversationHash,
        backend: Arc<B>,
    ) -> Option<IdleCheckoutGuard<B>> {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let pos = guard
            .idle_sessions
            .iter()
            .rposition(|e| &e.post_turn_dialogue_hash == hash)?;
        guard.total_sessions_reused += 1;
        guard.last_activity_unix_ms = unix_timestamp_ms();
        let entry = guard.idle_sessions.remove(pos)?;
        guard.retained_idle_bytes = guard
            .retained_idle_bytes
            .saturating_sub(entry.retained_bytes);
        drop(guard);
        Some(IdleCheckoutGuard::new(entry, Arc::clone(self), backend))
    }

    /// Returns `(active_leased_sessions, idle_cached_sessions)` without cloning session entries.
    #[must_use]
    pub fn idle_and_active_counts(&self) -> (usize, usize) {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        (guard.active_leases.len(), guard.idle_sessions.len())
    }

    /// Returns the number of currently parked MCP-suspended sessions.
    #[must_use]
    pub fn suspended_count(&self) -> usize {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.purge_closed_suspended();
        guard.suspended_sessions.len()
    }

    /// Returns the Unix millisecond timestamp of the most recent turn lease checkout or checkin.
    #[must_use]
    pub fn last_activity_unix_ms(&self) -> u64 {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.last_activity_unix_ms
    }

    /// Drains all cached idle and MCP-suspended sessions during gateway shutdown so parked
    /// session driver tasks exit cleanly and release their capability handles.
    pub fn clear_for_shutdown(&self) {
        let (dropped_suspended, dropped_idle) = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let removed = guard
                .suspended_sessions
                .len()
                .saturating_add(guard.idle_sessions.len());
            guard.total_sessions_closed_or_evicted = guard
                .total_sessions_closed_or_evicted
                .saturating_add(removed as u64);
            guard.retained_idle_bytes = 0;
            (
                std::mem::take(&mut guard.suspended_sessions),
                std::mem::take(&mut guard.idle_sessions),
            )
        };
        let had_entries = !dropped_suspended.is_empty() || !dropped_idle.is_empty();
        drop(dropped_suspended);
        drop(dropped_idle);
        if had_entries {
            self.bump_lifecycle_seq();
        }
    }

    /// Atomically drains all idle cached sessions (used before an idle `agy` binary hot-swap).
    pub fn take_all_idle_sessions(&self) -> Vec<IdleSessionEntry> {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut idle_bytes = 0usize;
        for e in &guard.idle_sessions {
            idle_bytes = idle_bytes.saturating_add(e.retained_bytes);
        }
        let count = guard.idle_sessions.len() as u64;
        guard.total_sessions_closed_or_evicted += count;
        guard.retained_idle_bytes = guard.retained_idle_bytes.saturating_sub(idle_bytes);
        guard.idle_sessions.drain(..).collect()
    }

    /// Manually closes and evicts an idle or suspended session from the pool (used by operator dashboard).
    pub fn close_idle_session<B: AcpBackend>(&self, session_id: &str, backend: &B) -> bool {
        let removed_sid = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Some(pos) = guard
                .idle_sessions
                .iter()
                .position(|e| e.session_id.as_str() == session_id)
            {
                guard.total_sessions_closed_or_evicted += 1;
                let entry = guard.idle_sessions.remove(pos);
                if let Some(ref e) = entry {
                    guard.retained_idle_bytes =
                        guard.retained_idle_bytes.saturating_sub(e.retained_bytes);
                }
                entry.map(|e| e.session_id)
            } else if let Some(pos) = guard
                .suspended_sessions
                .iter()
                .position(|e| e.session_id.as_str() == session_id)
            {
                guard.total_sessions_closed_or_evicted += 1;
                let entry = guard.suspended_sessions.remove(pos);
                if let Some(ref e) = entry {
                    guard.retained_idle_bytes =
                        guard.retained_idle_bytes.saturating_sub(e.retained_bytes);
                }
                entry.map(|e| e.session_id)
            } else {
                return false;
            }
        };

        if let Some(sid) = removed_sid {
            backend.abort_session_nonblocking(&sid);
            self.bump_lifecycle_seq();
            true
        } else {
            false
        }
    }

    /// Stores a completed OpenAI `ResponseObject` for `GET /v1/responses/{id}` inspection.
    pub fn store_response(&self, response: ResponseObject) {
        if !response.store {
            return;
        }
        let Ok(serialized) = serde_json::to_vec(&response) else {
            return;
        };
        let raw_json = Bytes::from(serialized);
        let resp_bytes = std::mem::size_of::<StoredResponseEntry>()
            .saturating_add(response.id.len())
            .saturating_add(raw_json.len());
        if resp_bytes > self.max_stored_response_bytes {
            return;
        }
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        while guard.stored_responses.len() >= self.limits.max_stored_responses().get()
            || (!guard.stored_responses.is_empty()
                && guard.retained_response_bytes.saturating_add(resp_bytes)
                    > self.max_stored_response_bytes)
        {
            if let Some(old) = guard.stored_responses.pop_front() {
                guard.retained_response_bytes = guard
                    .retained_response_bytes
                    .saturating_sub(old.retained_bytes);
            } else {
                break;
            }
        }
        guard.retained_response_bytes = guard.retained_response_bytes.saturating_add(resp_bytes);
        guard.stored_responses.push_back(StoredResponseEntry {
            id: response.id,
            retained_bytes: resp_bytes,
            raw_json,
        });
    }

    #[must_use]
    pub fn get_response_json(&self, response_id: &str) -> Option<Bytes> {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .stored_responses
            .iter()
            .rev()
            .find(|entry| entry.id == response_id)
            .map(|entry| entry.raw_json.clone())
    }

    /// Closes and evicts idle or suspended sessions whose `last_active_unix_ms` is at least
    /// `max_idle_age_ms` behind `now_unix_ms`, returning the number of reaped sessions.
    pub fn reap_expired_idle_sessions<B: AcpBackend>(
        &self,
        now_unix_ms: u64,
        max_idle_age_ms: u64,
        backend: &B,
    ) -> usize {
        if max_idle_age_ms == 0 {
            return 0;
        }
        let expired_sids = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.purge_closed_suspended();
            let mut reaped = Vec::new();
            let mut kept_idle = VecDeque::with_capacity(guard.idle_sessions.len());
            while let Some(entry) = guard.idle_sessions.pop_front() {
                if now_unix_ms.saturating_sub(entry.last_active_unix_ms) >= max_idle_age_ms {
                    guard.retained_idle_bytes = guard
                        .retained_idle_bytes
                        .saturating_sub(entry.retained_bytes);
                    guard.total_sessions_closed_or_evicted += 1;
                    reaped.push(entry.session_id);
                } else {
                    kept_idle.push_back(entry);
                }
            }
            guard.idle_sessions = kept_idle;

            let mut kept_susp = VecDeque::with_capacity(guard.suspended_sessions.len());
            while let Some(entry) = guard.suspended_sessions.pop_front() {
                if now_unix_ms.saturating_sub(entry.last_active_unix_ms) >= max_idle_age_ms {
                    guard.retained_idle_bytes = guard
                        .retained_idle_bytes
                        .saturating_sub(entry.retained_bytes);
                    guard.total_sessions_closed_or_evicted += 1;
                    reaped.push(entry.session_id);
                } else {
                    kept_susp.push_back(entry);
                }
            }
            guard.suspended_sessions = kept_susp;
            reaped
        };

        let count = expired_sids.len();
        if count > 0 {
            self.bump_lifecycle_seq();
        }
        for sid in expired_sids {
            backend.abort_session_nonblocking(&sid);
        }
        count
    }

    /// Captures a telemetry snapshot of all running and cached ACP sessions.
    #[must_use]
    pub fn snapshot(&self) -> SessionPoolTelemetry {
        self.snapshot_at(unix_timestamp_ms())
    }

    /// Captures a telemetry snapshot using an explicit `now_unix_ms` clock for deterministic
    /// KV-cache warmth classification in tests and live telemetry.
    #[must_use]
    pub fn snapshot_at(&self, now_unix_ms: u64) -> SessionPoolTelemetry {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.purge_closed_suspended();

        let mut sessions = Vec::with_capacity(
            guard.active_leases.len() + guard.suspended_sessions.len() + guard.idle_sessions.len(),
        );
        for (sid, meta) in &guard.active_leases {
            sessions.push(SessionSummary {
                session_id: sid.clone(),
                model_id: meta.model_id.as_str().to_owned(),
                state: SessionSlotState::Running,
                cache_warmth: PromptCacheWarmth::classify(
                    SessionSlotState::Running,
                    meta.last_active_unix_ms,
                    now_unix_ms,
                ),
                turn_count: meta.turn_count,
                message_count: meta.message_count,
                last_context_tokens: meta.last_context_used,
                total_prompt_tokens: meta.total_prompt_tokens,
                total_cached_read_tokens: meta.total_cached_read_tokens,
                cache_hit_rate_bps: compute_cache_hit_bps(
                    meta.total_cached_read_tokens,
                    meta.total_prompt_tokens,
                ),
                created_at_unix_ms: meta.created_at_unix_ms,
                last_active_unix_ms: meta.last_active_unix_ms,
                last_response_id: meta.last_response_id.clone(),
                mcp: meta.mcp_summary.clone(),
            });
        }
        for susp in guard.suspended_sessions.iter().rev() {
            sessions.push(SessionSummary {
                session_id: susp.session_id.as_str().to_owned(),
                model_id: susp.model_id.as_str().to_owned(),
                state: SessionSlotState::Suspended,
                cache_warmth: PromptCacheWarmth::classify(
                    SessionSlotState::Suspended,
                    susp.last_active_unix_ms,
                    now_unix_ms,
                ),
                turn_count: susp.turn_count,
                message_count: susp.message_count,
                last_context_tokens: susp.last_context_used,
                total_prompt_tokens: susp.total_prompt_tokens,
                total_cached_read_tokens: susp.total_cached_read_tokens,
                cache_hit_rate_bps: compute_cache_hit_bps(
                    susp.total_cached_read_tokens,
                    susp.total_prompt_tokens,
                ),
                created_at_unix_ms: susp.created_at_unix_ms,
                last_active_unix_ms: susp.last_active_unix_ms,
                last_response_id: susp.last_response_id.clone(),
                mcp: Some(susp.mcp_handle.telemetry_summary(susp.declared_tools.len())),
            });
        }
        for idle in guard.idle_sessions.iter().rev() {
            sessions.push(SessionSummary {
                session_id: idle.session_id.as_str().to_owned(),
                model_id: idle.model_id.as_str().to_owned(),
                state: SessionSlotState::Idle,
                cache_warmth: PromptCacheWarmth::classify(
                    SessionSlotState::Idle,
                    idle.last_active_unix_ms,
                    now_unix_ms,
                ),
                turn_count: idle.turn_count,
                message_count: idle.message_count,
                last_context_tokens: idle.last_context_used,
                total_prompt_tokens: idle.total_prompt_tokens,
                total_cached_read_tokens: idle.total_cached_read_tokens,
                cache_hit_rate_bps: compute_cache_hit_bps(
                    idle.total_cached_read_tokens,
                    idle.total_prompt_tokens,
                ),
                created_at_unix_ms: idle.created_at_unix_ms,
                last_active_unix_ms: idle.last_active_unix_ms,
                last_response_id: idle.last_response_id.clone(),
                mcp: idle
                    .mcp_handle
                    .as_ref()
                    .map(|h| h.telemetry_summary(idle.declared_tools.len())),
            });
        }

        SessionPoolTelemetry {
            active_leased_sessions: guard.active_leases.len(),
            suspended_mcp_sessions: guard.suspended_sessions.len(),
            idle_cached_sessions: guard.idle_sessions.len(),
            mcp_continuations: guard.mcp_continuations,
            max_idle_sessions: self.limits.max_idle_sessions().get(),
            stored_responses: guard.stored_responses.len(),
            max_stored_responses: self.limits.max_stored_responses().get(),
            total_sessions_created: guard.total_sessions_created,
            total_sessions_reused: guard.total_sessions_reused,
            total_sessions_closed_or_evicted: guard.total_sessions_closed_or_evicted,
            sessions,
        }
    }

    fn register_lease(&self, sid: &SessionId, meta: ActiveLeaseMeta, is_new: bool) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if is_new {
            guard.total_sessions_created += 1;
        }
        guard.last_activity_unix_ms = meta.last_active_unix_ms;
        guard.active_leases.insert(sid.as_str().to_owned(), meta);
    }

    fn update_lease_model(&self, sid: &SessionId, model_id: &ModelId) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(meta) = guard.active_leases.get_mut(sid.as_str()) {
            meta.model_id = model_id.clone();
        }
    }

    fn update_lease_mcp_summary(&self, sid: &SessionId, summary: Option<SessionMcpSummary>) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(meta) = guard.active_leases.get_mut(sid.as_str()) {
            meta.mcp_summary = summary;
        }
    }

    fn set_lease_in_flight_hashes(&self, sid: &SessionId, hashes: Vec<ConversationHash>) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(meta) = guard.active_leases.get_mut(sid.as_str()) {
            meta.in_flight_hashes = hashes;
        }
    }

    fn unregister_aborted_lease(&self, sid: &SessionId) {
        {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let _ = guard.active_leases.remove(sid.as_str());
            guard.total_sessions_closed_or_evicted += 1;
            guard.last_activity_unix_ms = unix_timestamp_ms();
        }
        self.bump_lifecycle_seq();
    }

    /// Returns a checked-out idle session back to the LRU pool when turn admission is rejected
    /// before any RPC mutates the underlying ACP session.
    pub(crate) fn restore_idle_on_reject<B: AcpBackend>(
        &self,
        entry: IdleSessionEntry,
        backend: &B,
    ) {
        {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.total_sessions_reused = guard.total_sessions_reused.saturating_sub(1);
        }
        self.checkin(entry, backend);
    }

    fn checkin<B: AcpBackend>(&self, mut entry: IdleSessionEntry, backend: &B) {
        let mut entry_bytes = entry.estimated_retained_bytes();
        if entry_bytes > self.max_idle_retained_bytes && !entry.accumulated_messages.is_empty() {
            entry.accumulated_messages.clear();
            entry.accumulated_messages.shrink_to_fit();
            entry_bytes = entry.estimated_retained_bytes();
        }
        entry.retained_bytes = entry_bytes;

        if entry_bytes > self.max_idle_retained_bytes {
            {
                let mut guard = match self.inner.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.last_activity_unix_ms = entry.last_active_unix_ms;
                let _ = guard.active_leases.remove(entry.session_id.as_str());
                guard.total_sessions_closed_or_evicted += 1;
            }
            backend.abort_session_nonblocking(&entry.session_id);
            self.bump_lifecycle_seq();
            return;
        }

        let evicted_sids = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.purge_closed_suspended();
            guard.last_activity_unix_ms = entry.last_active_unix_ms;
            let _ = guard.active_leases.remove(entry.session_id.as_str());
            let mut evicted = Vec::new();
            while guard.cached_slot_count() >= self.limits.max_idle_sessions().get()
                || (guard.cached_slot_count() > 0
                    && guard
                        .retained_idle_bytes
                        .saturating_add(entry.retained_bytes)
                        > self.max_idle_retained_bytes)
            {
                if let Some(old_sid) = guard.evict_oldest_cached_slot() {
                    evicted.push(old_sid);
                } else {
                    break;
                }
            }
            guard.retained_idle_bytes = guard
                .retained_idle_bytes
                .saturating_add(entry.retained_bytes);
            guard.idle_sessions.push_back(entry);
            evicted
        };

        self.bump_lifecycle_seq();
        for old_sid in evicted_sids {
            backend.abort_session_nonblocking(&old_sid);
        }
    }

    fn park_suspended<B: AcpBackend>(&self, mut entry: SuspendedMcpEntry, backend: &B) {
        if entry.cmd_tx.is_closed() {
            {
                let mut guard = match self.inner.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let _ = guard.active_leases.remove(entry.session_id.as_str());
                guard.total_sessions_closed_or_evicted += 1;
            }
            backend.abort_session_nonblocking(&entry.session_id);
            self.bump_lifecycle_seq();
            return;
        }

        let mut entry_bytes = entry.estimated_retained_bytes();
        if entry_bytes > self.max_idle_retained_bytes && !entry.accumulated_messages.is_empty() {
            entry.accumulated_messages.clear();
            entry.accumulated_messages.shrink_to_fit();
            entry_bytes = entry.estimated_retained_bytes();
        }
        entry.retained_bytes = entry_bytes;

        if entry_bytes > self.max_idle_retained_bytes {
            let abort_sid = entry.session_id.clone();
            {
                let mut guard = match self.inner.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.last_activity_unix_ms = entry.last_active_unix_ms;
                let _ = guard.active_leases.remove(abort_sid.as_str());
                guard.total_sessions_closed_or_evicted += 1;
            }
            drop(entry);
            backend.abort_session_nonblocking(&abort_sid);
            self.bump_lifecycle_seq();
            return;
        }

        let evicted_sids = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.purge_closed_suspended();
            guard.last_activity_unix_ms = entry.last_active_unix_ms;
            let _ = guard.active_leases.remove(entry.session_id.as_str());
            let mut evicted = Vec::new();
            while guard.cached_slot_count() >= self.limits.max_idle_sessions().get()
                || (guard.cached_slot_count() > 0
                    && guard
                        .retained_idle_bytes
                        .saturating_add(entry.retained_bytes)
                        > self.max_idle_retained_bytes)
            {
                if let Some(old_sid) = guard.evict_oldest_cached_slot() {
                    evicted.push(old_sid);
                } else {
                    break;
                }
            }
            guard.retained_idle_bytes = guard
                .retained_idle_bytes
                .saturating_add(entry.retained_bytes);
            guard.suspended_sessions.push_back(entry);
            evicted
        };

        self.bump_lifecycle_seq();
        for old_sid in evicted_sids {
            backend.abort_session_nonblocking(&old_sid);
        }
    }

    fn record_mcp_continuation(&self) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.mcp_continuations += 1;
        guard.total_sessions_reused += 1;
    }

    fn record_unwound_session_reused(&self) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.total_sessions_reused += 1;
    }

    fn bump_lifecycle_seq(&self) {
        self.lifecycle_seq
            .send_modify(|seq| *seq = seq.wrapping_add(1));
    }
}

/// Post-turn session state committed back to `SessionPool` when an interactive turn completes.
pub struct CommittedSessionTurn {
    pub post_turn_hash: ConversationHash,
    pub alt_post_turn_hash: Option<ConversationHash>,
    pub post_turn_dialogue_hash: ConversationHash,
    pub declared_tools: Vec<CanonicalToolSpec>,
    pub last_context_used: u64,
    pub turn_prompt_tokens: u64,
    pub turn_cached_read_tokens: u64,
    pub message_count: usize,
    pub last_response_id: Option<String>,
    pub accumulated_messages: Vec<CanonicalMessage>,
}

/// RAII guard that notifies any waiting follow-up request as soon as a soft-cancelled warm
/// session finishes draining and checks back into `SessionPool`.
pub struct PendingCheckinGuard {
    id: u64,
    pool: Arc<SessionPool>,
}

impl Drop for PendingCheckinGuard {
    fn drop(&mut self) {
        {
            let mut guard = match self.pool.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard
                .pending_checkins
                .retain(|(entry_id, _)| *entry_id != self.id);
        }
        self.pool.bump_lifecycle_seq();
    }
}

/// RAII guard for a warm session checked out of `idle_sessions` before it transitions into an
/// active turn lease ([`SessionLeaseGuard`]).
///
/// If dropped before [`Self::into_lease_guard`] (for example on admission rejection, prompt
/// validation failure, early `?`, or caller future cancellation), restores the untouched warm
/// session back to `idle_sessions` (or aborts it if the idle pool has since exceeded capacity).
pub struct IdleCheckoutGuard<B: AcpBackend> {
    entry: Option<IdleSessionEntry>,
    pool: Arc<SessionPool>,
    backend: Arc<B>,
}

impl<B: AcpBackend> IdleCheckoutGuard<B> {
    #[must_use]
    fn new(entry: IdleSessionEntry, pool: Arc<SessionPool>, backend: Arc<B>) -> Self {
        Self {
            entry: Some(entry),
            pool,
            backend,
        }
    }

    /// Wraps a gracefully unwound suspended MCP session as an [`IdleCheckoutGuard`] so standard
    /// warm continuation / model-switch / `/compact` paths reuse the live ACP worker seamlessly.
    #[must_use]
    pub fn from_unwound(entry: IdleSessionEntry, pool: Arc<SessionPool>, backend: Arc<B>) -> Self {
        pool.record_unwound_session_reused();
        Self::new(entry, pool, backend)
    }

    #[must_use]
    pub fn entry(&self) -> &IdleSessionEntry {
        self.entry
            .as_ref()
            .expect("IdleCheckoutGuard entry invariant: populated until into_lease_guard or drop")
    }

    /// Disarms idle-restore-on-drop and promotes this checked-out session into an active
    /// [`SessionLeaseGuard`], returning any previously accumulated messages by value.
    #[must_use]
    pub fn into_lease_guard(
        mut self,
        new_message_count: usize,
    ) -> (SessionLeaseGuard<B>, Vec<CanonicalMessage>) {
        let mut entry = self
            .entry
            .take()
            .expect("IdleCheckoutGuard entry invariant: populated until into_lease_guard or drop");
        let accumulated = std::mem::take(&mut entry.accumulated_messages);
        let lease = SessionLeaseGuard::from_cached(
            &entry,
            new_message_count,
            Arc::clone(&self.pool),
            Arc::clone(&self.backend),
        );
        (lease, accumulated)
    }
}

impl<B: AcpBackend> Drop for IdleCheckoutGuard<B> {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            self.pool.restore_idle_on_reject(entry, &*self.backend);
        }
    }
}

/// Cancel-safe RAII checkout guard for a parked [`SuspendedMcpEntry`].
///
/// Dropping without calling `promote_for_mcp_continuation` or `take_for_unwind`
/// restores the untouched suspended entry back into `SessionPool`.
pub struct SuspendedCheckoutGuard<B: AcpBackend> {
    entry: Option<SuspendedMcpEntry>,
    pool: Arc<SessionPool>,
    backend: Arc<B>,
}

impl<B: AcpBackend> SuspendedCheckoutGuard<B> {
    #[must_use]
    fn new(entry: SuspendedMcpEntry, pool: Arc<SessionPool>, backend: Arc<B>) -> Self {
        Self {
            entry: Some(entry),
            pool,
            backend,
        }
    }

    #[must_use]
    pub fn entry(&self) -> &SuspendedMcpEntry {
        self.entry.as_ref().expect(
            "SuspendedCheckoutGuard entry invariant: populated until promote, unwind, or drop",
        )
    }

    /// Promotes this parked session into an active [`SessionLeaseGuard`] for native MCP
    /// tool-result continuation inside the same running `session/prompt`.
    #[must_use]
    pub(crate) fn promote_for_mcp_continuation(
        mut self,
        new_message_count: usize,
    ) -> (PromotedMcpSession, SessionLeaseGuard<B>) {
        let mut entry = self.entry.take().expect(
            "SuspendedCheckoutGuard entry invariant: populated until promote, unwind, or drop",
        );
        self.pool.record_mcp_continuation();
        let now_ms = unix_timestamp_ms();
        let idle_ms = now_ms.saturating_sub(entry.last_active_unix_ms);
        let next_turn = entry.turn_count.saturating_add(1);
        self.pool.register_lease(
            &entry.session_id,
            ActiveLeaseMeta {
                model_id: entry.model_id.clone(),
                in_flight_hashes: Vec::new(),
                turn_count: next_turn,
                message_count: new_message_count,
                last_context_used: entry.last_context_used,
                total_prompt_tokens: entry.total_prompt_tokens,
                total_cached_read_tokens: entry.total_cached_read_tokens,
                created_at_unix_ms: entry.created_at_unix_ms,
                last_active_unix_ms: now_ms,
                last_response_id: entry.last_response_id.clone(),
                mcp_summary: Some(
                    entry
                        .mcp_handle
                        .telemetry_summary(entry.declared_tools.len()),
                ),
            },
            false,
        );
        let lease = SessionLeaseGuard {
            session_id: entry.session_id,
            tool_nonce: entry.tool_nonce,
            model_id: entry.model_id,
            mcp_handle: Some(entry.mcp_handle),
            declared_tools: entry.declared_tools.clone(),
            last_context_used: entry.last_context_used,
            total_prompt_tokens: entry.total_prompt_tokens,
            total_cached_read_tokens: entry.total_cached_read_tokens,
            turn_count: next_turn,
            idle_before_checkout_ms: Some(idle_ms),
            created_at_unix_ms: entry.created_at_unix_ms,
            pool: Arc::clone(&self.pool),
            backend: Arc::clone(&self.backend),
            armed: true,
        };
        let promoted = PromotedMcpSession {
            cmd_tx: entry.cmd_tx,
            pending_calls: entry.pending_calls,
            declared_tools: entry.declared_tools,
            accumulated_messages: std::mem::take(&mut entry.accumulated_messages),
            last_context_used: entry.last_context_used,
            post_turn_hash: entry.post_turn_hash,
        };
        (promoted, lease)
    }

    /// Takes ownership of the [`SuspendedMcpEntry`] without restoring on drop so the caller can
    /// unwind its `session/prompt` via `session/cancel` and transition to warm continuation.
    #[must_use]
    pub fn take_for_unwind(mut self) -> SuspendedMcpEntry {
        self.entry.take().expect(
            "SuspendedCheckoutGuard entry invariant: populated until promote, unwind, or drop",
        )
    }
}

impl<B: AcpBackend> Drop for SuspendedCheckoutGuard<B> {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            self.pool.park_suspended(entry, &*self.backend);
        }
    }
}

/// Non-generic RAII transfer token used to pass an active lease across `McpDriverCommand`
/// without making `SessionPool` generic over `B: AcpBackend`.
pub(crate) struct RawSessionLease {
    session_id: SessionId,
    tool_nonce: Arc<str>,
    model_id: ModelId,
    mcp_handle: Option<Arc<McpSessionHandle>>,
    declared_tools: Vec<CanonicalToolSpec>,
    last_context_used: u64,
    total_prompt_tokens: u64,
    total_cached_read_tokens: u64,
    turn_count: u64,
    idle_before_checkout_ms: Option<u64>,
    created_at_unix_ms: u64,
    pool: Arc<SessionPool>,
    armed: bool,
}

impl Drop for RawSessionLease {
    fn drop(&mut self) {
        if self.armed {
            self.pool.unregister_aborted_lease(&self.session_id);
        }
    }
}

/// RAII lease guard for an active ACP session turn.
///
/// Created *before* awaiting prompt execution. If dropped without calling `commit`
/// (e.g. HTTP client disconnect, timeout, or early `?`), synchronously triggers
/// `session/cancel` + `session/close` on the backend and removes the active lease.
pub struct SessionLeaseGuard<B: AcpBackend> {
    session_id: SessionId,
    tool_nonce: Arc<str>,
    model_id: ModelId,
    mcp_handle: Option<Arc<McpSessionHandle>>,
    declared_tools: Vec<CanonicalToolSpec>,
    last_context_used: u64,
    total_prompt_tokens: u64,
    total_cached_read_tokens: u64,
    turn_count: u64,
    idle_before_checkout_ms: Option<u64>,
    created_at_unix_ms: u64,
    pool: Arc<SessionPool>,
    backend: Arc<B>,
    armed: bool,
}

impl<B: AcpBackend> SessionLeaseGuard<B> {
    #[must_use]
    pub fn new_fresh(
        session_id: SessionId,
        tool_nonce: Arc<str>,
        model_id: ModelId,
        declared_tools: Vec<CanonicalToolSpec>,
        initial_message_count: usize,
        pool: Arc<SessionPool>,
        backend: Arc<B>,
    ) -> Self {
        let now_ms = unix_timestamp_ms();
        pool.register_lease(
            &session_id,
            ActiveLeaseMeta {
                model_id: model_id.clone(),
                in_flight_hashes: Vec::new(),
                turn_count: 1,
                message_count: initial_message_count,
                last_context_used: 0,
                total_prompt_tokens: 0,
                total_cached_read_tokens: 0,
                created_at_unix_ms: now_ms,
                last_active_unix_ms: now_ms,
                last_response_id: None,
                mcp_summary: None,
            },
            true,
        );
        Self {
            session_id,
            tool_nonce,
            model_id,
            mcp_handle: None,
            declared_tools,
            last_context_used: 0,
            total_prompt_tokens: 0,
            total_cached_read_tokens: 0,
            turn_count: 1,
            idle_before_checkout_ms: None,
            created_at_unix_ms: now_ms,
            pool,
            backend,
            armed: true,
        }
    }

    #[must_use]
    pub fn with_mcp_handle(mut self, mcp_handle: Option<Arc<McpSessionHandle>>) -> Self {
        let summary = mcp_handle
            .as_ref()
            .map(|h| h.telemetry_summary(self.declared_tools.len()));
        self.pool
            .update_lease_mcp_summary(&self.session_id, summary);
        self.mcp_handle = mcp_handle;
        self
    }

    #[must_use]
    pub fn from_cached(
        cached: &IdleSessionEntry,
        new_message_count: usize,
        pool: Arc<SessionPool>,
        backend: Arc<B>,
    ) -> Self {
        let now_ms = unix_timestamp_ms();
        let idle_ms = now_ms.saturating_sub(cached.last_active_unix_ms);
        let next_turn = cached.turn_count.saturating_add(1);
        pool.register_lease(
            &cached.session_id,
            ActiveLeaseMeta {
                model_id: cached.model_id.clone(),
                in_flight_hashes: Vec::new(),
                turn_count: next_turn,
                message_count: new_message_count,
                last_context_used: cached.last_context_used,
                total_prompt_tokens: cached.total_prompt_tokens,
                total_cached_read_tokens: cached.total_cached_read_tokens,
                created_at_unix_ms: cached.created_at_unix_ms,
                last_active_unix_ms: now_ms,
                last_response_id: cached.last_response_id.clone(),
                mcp_summary: cached
                    .mcp_handle
                    .as_ref()
                    .map(|h| h.telemetry_summary(cached.declared_tools.len())),
            },
            false,
        );
        Self {
            session_id: cached.session_id.clone(),
            tool_nonce: Arc::clone(&cached.tool_nonce),
            model_id: cached.model_id.clone(),
            mcp_handle: cached.mcp_handle.clone(),
            declared_tools: cached.declared_tools.clone(),
            last_context_used: cached.last_context_used,
            total_prompt_tokens: cached.total_prompt_tokens,
            total_cached_read_tokens: cached.total_cached_read_tokens,
            turn_count: next_turn,
            idle_before_checkout_ms: Some(idle_ms),
            created_at_unix_ms: cached.created_at_unix_ms,
            pool,
            backend,
            armed: true,
        }
    }

    #[must_use]
    pub(crate) fn into_raw_parts(mut self) -> RawSessionLease {
        self.armed = false;
        RawSessionLease {
            session_id: self.session_id.clone(),
            tool_nonce: Arc::clone(&self.tool_nonce),
            model_id: self.model_id.clone(),
            mcp_handle: self.mcp_handle.take(),
            declared_tools: std::mem::take(&mut self.declared_tools),
            last_context_used: self.last_context_used,
            total_prompt_tokens: self.total_prompt_tokens,
            total_cached_read_tokens: self.total_cached_read_tokens,
            turn_count: self.turn_count,
            idle_before_checkout_ms: self.idle_before_checkout_ms,
            created_at_unix_ms: self.created_at_unix_ms,
            pool: Arc::clone(&self.pool),
            armed: true,
        }
    }

    #[must_use]
    pub(crate) fn from_raw_parts(mut raw: RawSessionLease, backend: Arc<B>) -> Self {
        raw.armed = false;
        Self {
            session_id: raw.session_id.clone(),
            tool_nonce: Arc::clone(&raw.tool_nonce),
            model_id: raw.model_id.clone(),
            mcp_handle: raw.mcp_handle.take(),
            declared_tools: std::mem::take(&mut raw.declared_tools),
            last_context_used: raw.last_context_used,
            total_prompt_tokens: raw.total_prompt_tokens,
            total_cached_read_tokens: raw.total_cached_read_tokens,
            turn_count: raw.turn_count,
            idle_before_checkout_ms: raw.idle_before_checkout_ms,
            created_at_unix_ms: raw.created_at_unix_ms,
            pool: Arc::clone(&raw.pool),
            backend,
            armed: true,
        }
    }

    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    #[must_use]
    pub fn tool_nonce(&self) -> &str {
        &self.tool_nonce
    }

    #[must_use]
    pub const fn model_id(&self) -> &ModelId {
        &self.model_id
    }

    #[must_use]
    pub const fn mcp_handle(&self) -> Option<&Arc<McpSessionHandle>> {
        self.mcp_handle.as_ref()
    }

    #[must_use]
    pub fn declared_tools(&self) -> &[CanonicalToolSpec] {
        &self.declared_tools
    }

    #[must_use]
    pub const fn last_context_used(&self) -> u64 {
        self.last_context_used
    }

    #[must_use]
    pub const fn turn_count(&self) -> u64 {
        self.turn_count
    }

    #[must_use]
    pub const fn idle_before_checkout_ms(&self) -> Option<u64> {
        self.idle_before_checkout_ms
    }

    pub fn set_model_id(&mut self, model_id: ModelId) {
        self.pool.update_lease_model(&self.session_id, &model_id);
        self.model_id = model_id;
    }

    pub fn refresh_mcp_summary(&self, declared_tools_len: usize) {
        if let Some(ref handle) = self.mcp_handle {
            self.pool.update_lease_mcp_summary(
                &self.session_id,
                Some(handle.telemetry_summary(declared_tools_len)),
            );
        }
    }

    /// Reconciles prior step estimates in a multi-step MCP cascade with authoritative cumulative tokens.
    pub fn reconcile_prior_cascade_tokens(
        &mut self,
        accounted_prior_prompt: u64,
        accounted_prior_cached_read: u64,
    ) {
        self.total_prompt_tokens = self
            .total_prompt_tokens
            .saturating_sub(accounted_prior_prompt);
        self.total_cached_read_tokens = self
            .total_cached_read_tokens
            .saturating_sub(accounted_prior_cached_read);
    }

    #[must_use]
    pub fn register_pending_checkin(&self, hashes: Vec<ConversationHash>) -> PendingCheckinGuard {
        self.pool.register_pending_checkin(hashes)
    }

    /// Disarms abort-on-drop and parks this active session into `SessionPool` while its
    /// `session/prompt` remains suspended inside a loopback MCP `tools/call`.
    pub(crate) fn suspend_on_mcp(mut self, suspended: SuspendedMcpTurn) {
        self.armed = false;
        let now_ms = unix_timestamp_ms();
        let entry = SuspendedMcpEntry {
            session_id: self.session_id.clone(),
            tool_nonce: Arc::clone(&self.tool_nonce),
            model_id: self.model_id.clone(),
            mcp_handle: suspended.mcp_handle,
            pending_calls: suspended.pending_calls,
            cmd_tx: suspended.cmd_tx,
            post_turn_hash: suspended.post_turn_hash,
            post_turn_dialogue_hash: suspended.post_turn_dialogue_hash,
            pre_step_hash: suspended.pre_step_hash,
            pre_step_dialogue_hash: suspended.pre_step_dialogue_hash,
            alt_pre_user_hash: suspended.alt_pre_user_hash,
            declared_tools: suspended.declared_tools,
            last_context_used: suspended.last_context_used,
            total_prompt_tokens: self
                .total_prompt_tokens
                .saturating_add(suspended.step_prompt_tokens),
            total_cached_read_tokens: self
                .total_cached_read_tokens
                .saturating_add(suspended.step_cached_read_tokens),
            last_response_id: suspended.last_response_id,
            accumulated_messages: suspended.accumulated_messages,
            message_count: suspended.message_count,
            pre_step_message_count: suspended.pre_step_message_count,
            turn_count: self.turn_count,
            created_at_unix_ms: self.created_at_unix_ms,
            last_active_unix_ms: now_ms,
            retained_bytes: 0,
        };
        self.pool.park_suspended(entry, &*self.backend);
    }

    /// Disarms abort-on-drop and returns the healthy session to `pool` for future turns.
    pub fn commit(mut self, committed: CommittedSessionTurn) {
        self.armed = false;
        let now_ms = unix_timestamp_ms();
        let entry = IdleSessionEntry {
            session_id: self.session_id.clone(),
            tool_nonce: Arc::clone(&self.tool_nonce),
            model_id: self.model_id.clone(),
            mcp_handle: self.mcp_handle.take(),
            post_turn_hash: committed.post_turn_hash,
            alt_post_turn_hash: committed.alt_post_turn_hash,
            post_turn_dialogue_hash: committed.post_turn_dialogue_hash,
            declared_tools: committed.declared_tools,
            last_context_used: committed.last_context_used,
            total_prompt_tokens: self
                .total_prompt_tokens
                .saturating_add(committed.turn_prompt_tokens),
            total_cached_read_tokens: self
                .total_cached_read_tokens
                .saturating_add(committed.turn_cached_read_tokens),
            last_response_id: committed.last_response_id,
            accumulated_messages: committed.accumulated_messages,
            message_count: committed.message_count,
            turn_count: self.turn_count,
            created_at_unix_ms: self.created_at_unix_ms,
            last_active_unix_ms: now_ms,
            retained_bytes: 0,
        };
        self.pool.checkin(entry, &*self.backend);
    }

    pub fn set_in_flight_hashes(&self, hashes: Vec<ConversationHash>) {
        self.pool
            .set_lease_in_flight_hashes(&self.session_id, hashes);
    }

    /// Disarms abort-on-drop and closes a one-shot auxiliary/classifier session immediately so
    /// ephemeral background calls do not occupy idle session slots or worker processes.
    pub fn finish_ephemeral(mut self) {
        self.armed = false;
        self.pool.unregister_aborted_lease(&self.session_id);
        self.backend.abort_session_nonblocking(&self.session_id);
    }
}

impl<B: AcpBackend> Drop for SessionLeaseGuard<B> {
    fn drop(&mut self) {
        if self.armed {
            self.pool.unregister_aborted_lease(&self.session_id);
            self.backend.abort_session_nonblocking(&self.session_id);
        }
    }
}
