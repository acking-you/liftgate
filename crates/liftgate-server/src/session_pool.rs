use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use liftgate_types::{
    canonical::{CanonicalMessage, CanonicalToolSpec},
    config::{GatewayLimits, ModelId, SessionId},
    openai::ResponseObject,
    session_key::ConversationHash,
    telemetry::{SessionPoolTelemetry, SessionSlotState, SessionSummary},
};
use tokio::sync::Semaphore;

use crate::{acp_client::AcpBackend, error::GatewayError, telemetry::unix_timestamp_ms};

const PERMIT_UNIT_BYTES: usize = 1024;
const MAX_ACTIVE_MEMORY_CAP_BYTES: usize = 256 * 1024 * 1024;
const MAX_IDLE_RETAINED_CAP_BYTES: usize = 192 * 1024 * 1024;
const MAX_STORED_RESPONSES_CAP_BYTES: usize = 64 * 1024 * 1024;

/// Baseline active memory reservation for receiving and JSON-decoding an HTTP body of `body_bytes`.
#[must_use]
pub const fn baseline_parse_reservation_bytes(body_bytes: usize) -> usize {
    if body_bytes == 0 {
        return 0;
    }
    let candidate_slack = body_bytes / 16;
    let slack = if candidate_slack > 4 * 1024 {
        candidate_slack
    } else {
        4 * 1024
    };
    body_bytes.saturating_mul(2).saturating_add(slack)
}

fn bytes_to_permits(bytes: usize) -> u32 {
    if bytes == 0 {
        return 0;
    }
    let units = bytes.div_ceil(PERMIT_UNIT_BYTES).max(1);
    u32::try_from(units).unwrap_or(u32::MAX)
}

/// Cancel-safe RAII reservation against `SessionPool`'s global active request/turn byte budget.
pub struct ActiveMemoryPermit {
    sem: Arc<Semaphore>,
    max_permits: u32,
    permits: u32,
}

impl ActiveMemoryPermit {
    /// Approximate bytes currently reserved by this permit.
    #[must_use]
    pub const fn reserved_bytes(&self) -> usize {
        (self.permits as usize).saturating_mul(PERMIT_UNIT_BYTES)
    }

    fn checked_permits(&self, target_bytes: usize) -> Result<u32, GatewayError> {
        let target_permits = bytes_to_permits(target_bytes);
        if target_permits > self.max_permits {
            return Err(GatewayError::RequestBudgetExhausted);
        }
        Ok(target_permits)
    }

    /// Acquires the initial reservation for a request before it holds any partial permits.
    ///
    /// Waiting on `Semaphore::acquire_many` is permitted *only* when `self.permits == 0` so
    /// concurrent requests can never form a hold-and-wait deadlock cycle. If already holding
    /// permits, delegates to non-blocking [`Self::try_grow_to_bytes`].
    pub async fn acquire_initial_bytes(
        &mut self,
        target_bytes: usize,
        timeout: Duration,
    ) -> Result<(), GatewayError> {
        if self.permits > 0 {
            return self.try_grow_to_bytes(target_bytes);
        }
        let target_permits = self.checked_permits(target_bytes)?;
        if target_permits == 0 {
            return Ok(());
        }
        let acquired = tokio::time::timeout(timeout, self.sem.acquire_many(target_permits))
            .await
            .map_err(|_| GatewayError::RequestBudgetExhausted)?
            .map_err(|_| GatewayError::RequestBudgetExhausted)?;
        acquired.forget();
        self.permits = target_permits;
        Ok(())
    }

    /// Non-blockingly grows an existing reservation to `target_bytes`, failing immediately if
    /// `target_bytes` exceeds pool capacity or insufficient permits are currently free.
    pub fn try_grow_to_bytes(&mut self, target_bytes: usize) -> Result<(), GatewayError> {
        let target_permits = self.checked_permits(target_bytes)?;
        if target_permits > self.permits {
            let delta = target_permits - self.permits;
            let acquired = self
                .sem
                .try_acquire_many(delta)
                .map_err(|_| GatewayError::RequestBudgetExhausted)?;
            acquired.forget();
            self.permits = target_permits;
        }
        Ok(())
    }

    /// Resizes this reservation to `target_bytes` without blocking: shrinks immediately via
    /// `add_permits`, or attempts non-blocking growth via [`Self::try_grow_to_bytes`].
    pub fn try_adjust_to_bytes(&mut self, target_bytes: usize) -> Result<(), GatewayError> {
        let target_permits = self.checked_permits(target_bytes)?;
        if target_permits <= self.permits {
            let release = (self.permits - target_permits) as usize;
            if release > 0 {
                self.sem.add_permits(release);
                self.permits = target_permits;
            }
            Ok(())
        } else {
            self.try_grow_to_bytes(target_bytes)
        }
    }
}

impl Drop for ActiveMemoryPermit {
    fn drop(&mut self) {
        if self.permits > 0 {
            self.sem.add_permits(self.permits as usize);
            self.permits = 0;
        }
    }
}

/// Cached metadata for an idle ACP session awaiting a multi-turn continuation.
#[derive(Clone, Debug)]
pub struct IdleSessionEntry {
    pub session_id: SessionId,
    pub tool_nonce: Arc<str>,
    pub model_id: ModelId,
    pub post_turn_hash: ConversationHash,
    pub post_turn_dialogue_hash: ConversationHash,
    pub declared_tools: Vec<CanonicalToolSpec>,
    pub last_context_used: u64,
    pub last_response_id: Option<String>,
    pub accumulated_messages: Vec<CanonicalMessage>,
    pub message_count: usize,
    pub turn_count: u64,
    pub created_at_unix_ms: u64,
    pub last_active_unix_ms: u64,
    retained_bytes: usize,
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

        std::mem::size_of::<Self>()
            .saturating_add(self.session_id.as_str().len())
            .saturating_add(self.tool_nonce.len())
            .saturating_add(self.model_id.as_str().len())
            .saturating_add(self.last_response_id.as_deref().map_or(0, str::len))
            .saturating_add(tool_bytes)
            .saturating_add(msg_bytes)
    }
}

#[derive(Clone, Debug)]
struct ActiveLeaseMeta {
    model_id: ModelId,
    turn_count: u64,
    message_count: usize,
    created_at_unix_ms: u64,
    last_active_unix_ms: u64,
    last_response_id: Option<String>,
}

struct StoredResponseEntry {
    id: String,
    retained_bytes: usize,
    response: ResponseObject,
}

/// Bounded LRU pool of idle ACP sessions, active leases, and recent OpenAI `ResponseObject` records.
pub struct SessionPool {
    limits: GatewayLimits,
    active_memory_sem: Arc<Semaphore>,
    max_active_permits: u32,
    max_active_bytes: usize,
    max_idle_retained_bytes: usize,
    max_stored_response_bytes: usize,
    inner: Mutex<PoolInner>,
}

struct PoolInner {
    active_leases: BTreeMap<String, ActiveLeaseMeta>,
    idle_sessions: VecDeque<IdleSessionEntry>,
    retained_idle_bytes: usize,
    stored_responses: VecDeque<StoredResponseEntry>,
    retained_response_bytes: usize,
    total_sessions_created: u64,
    total_sessions_reused: u64,
    total_sessions_closed_or_evicted: u64,
    last_activity_unix_ms: u64,
}

impl SessionPool {
    #[must_use]
    pub fn new(limits: GatewayLimits) -> Self {
        let max_body = limits.max_body_bytes().get();
        let min_single_turn_bytes = baseline_parse_reservation_bytes(max_body);
        let max_active_bytes = max_body
            .saturating_mul(8)
            .min(MAX_ACTIVE_MEMORY_CAP_BYTES)
            .max(min_single_turn_bytes);
        let max_idle_retained_bytes = max_body
            .saturating_mul(6)
            .min(MAX_IDLE_RETAINED_CAP_BYTES)
            .max(max_body);
        let max_stored_response_bytes = max_body
            .saturating_mul(2)
            .min(MAX_STORED_RESPONSES_CAP_BYTES)
            .max(max_body);
        let max_active_permits = bytes_to_permits(max_active_bytes).max(1);

        Self {
            limits,
            active_memory_sem: Arc::new(Semaphore::new(max_active_permits as usize)),
            max_active_permits,
            max_active_bytes,
            max_idle_retained_bytes,
            max_stored_response_bytes,
            inner: Mutex::new(PoolInner {
                active_leases: BTreeMap::new(),
                idle_sessions: VecDeque::with_capacity(limits.max_idle_sessions().get()),
                retained_idle_bytes: 0,
                stored_responses: VecDeque::with_capacity(limits.max_stored_responses().get()),
                retained_response_bytes: 0,
                total_sessions_created: 0,
                total_sessions_reused: 0,
                total_sessions_closed_or_evicted: 0,
                last_activity_unix_ms: unix_timestamp_ms(),
            }),
        }
    }

    /// Creates an empty RAII memory permit bound to this pool's active byte budget.
    #[must_use]
    pub fn new_active_memory_permit(&self) -> ActiveMemoryPermit {
        ActiveMemoryPermit {
            sem: Arc::clone(&self.active_memory_sem),
            max_permits: self.max_active_permits,
            permits: 0,
        }
    }

    /// Global byte ceiling across concurrent HTTP body reads and running turns.
    #[must_use]
    pub const fn max_active_memory_bytes(&self) -> usize {
        self.max_active_bytes
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

    /// Returns the Unix millisecond timestamp of the most recent turn lease checkout or checkin.
    #[must_use]
    pub fn last_activity_unix_ms(&self) -> u64 {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.last_activity_unix_ms
    }

    /// Atomically drains all idle cached sessions (used before an idle `agy` binary hot-swap).
    pub fn take_all_idle_sessions(&self) -> Vec<IdleSessionEntry> {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let count = guard.idle_sessions.len() as u64;
        guard.total_sessions_closed_or_evicted += count;
        guard.retained_idle_bytes = 0;
        guard.idle_sessions.drain(..).collect()
    }

    /// Manually closes and evicts an idle session from the pool (used by operator dashboard).
    pub fn close_idle_session<B: AcpBackend>(&self, session_id: &str, backend: &B) -> bool {
        let removed = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let Some(pos) = guard
                .idle_sessions
                .iter()
                .position(|e| e.session_id.as_str() == session_id)
            else {
                return false;
            };
            guard.total_sessions_closed_or_evicted += 1;
            let entry = guard.idle_sessions.remove(pos);
            if let Some(ref e) = entry {
                guard.retained_idle_bytes =
                    guard.retained_idle_bytes.saturating_sub(e.retained_bytes);
            }
            entry
        };

        if let Some(entry) = removed {
            backend.abort_session_nonblocking(&entry.session_id);
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
        let resp_bytes = response.estimated_retained_bytes();
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
            id: response.id.clone(),
            retained_bytes: resp_bytes,
            response,
        });
    }

    #[must_use]
    pub fn get_response(&self, response_id: &str) -> Option<ResponseObject> {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .stored_responses
            .iter()
            .rev()
            .find(|entry| entry.id == response_id)
            .map(|entry| entry.response.clone())
    }

    /// Captures a telemetry snapshot of all running and cached ACP sessions.
    #[must_use]
    pub fn snapshot(&self) -> SessionPoolTelemetry {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        let mut sessions =
            Vec::with_capacity(guard.active_leases.len() + guard.idle_sessions.len());
        for (sid, meta) in &guard.active_leases {
            sessions.push(SessionSummary {
                session_id: sid.clone(),
                model_id: meta.model_id.as_str().to_owned(),
                state: SessionSlotState::Running,
                turn_count: meta.turn_count,
                message_count: meta.message_count,
                created_at_unix_ms: meta.created_at_unix_ms,
                last_active_unix_ms: meta.last_active_unix_ms,
                last_response_id: meta.last_response_id.clone(),
            });
        }
        for idle in guard.idle_sessions.iter().rev() {
            sessions.push(SessionSummary {
                session_id: idle.session_id.as_str().to_owned(),
                model_id: idle.model_id.as_str().to_owned(),
                state: SessionSlotState::Idle,
                turn_count: idle.turn_count,
                message_count: idle.message_count,
                created_at_unix_ms: idle.created_at_unix_ms,
                last_active_unix_ms: idle.last_active_unix_ms,
                last_response_id: idle.last_response_id.clone(),
            });
        }

        SessionPoolTelemetry {
            active_leased_sessions: guard.active_leases.len(),
            idle_cached_sessions: guard.idle_sessions.len(),
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

    fn unregister_aborted_lease(&self, sid: &SessionId) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = guard.active_leases.remove(sid.as_str());
        guard.total_sessions_closed_or_evicted += 1;
        guard.last_activity_unix_ms = unix_timestamp_ms();
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
            return;
        }

        let evicted = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.last_activity_unix_ms = entry.last_active_unix_ms;
            let _ = guard.active_leases.remove(entry.session_id.as_str());
            let mut evicted = Vec::new();
            while guard.idle_sessions.len() >= self.limits.max_idle_sessions().get()
                || (!guard.idle_sessions.is_empty()
                    && guard
                        .retained_idle_bytes
                        .saturating_add(entry.retained_bytes)
                        > self.max_idle_retained_bytes)
            {
                if let Some(old) = guard.idle_sessions.pop_front() {
                    guard.retained_idle_bytes =
                        guard.retained_idle_bytes.saturating_sub(old.retained_bytes);
                    guard.total_sessions_closed_or_evicted += 1;
                    evicted.push(old);
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

        for old in evicted {
            backend.abort_session_nonblocking(&old.session_id);
        }
    }
}

/// Post-turn session state committed back to `SessionPool` when an interactive turn completes.
pub struct CommittedSessionTurn {
    pub post_turn_hash: ConversationHash,
    pub post_turn_dialogue_hash: ConversationHash,
    pub declared_tools: Vec<CanonicalToolSpec>,
    pub last_context_used: u64,
    pub message_count: usize,
    pub last_response_id: Option<String>,
    pub accumulated_messages: Vec<CanonicalMessage>,
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

/// RAII lease guard for an active ACP session turn.
///
/// Created *before* awaiting prompt execution. If dropped without calling `commit`
/// (e.g. HTTP client disconnect, timeout, or early `?`), synchronously triggers
/// `session/cancel` + `session/close` on the backend and removes the active lease.
pub struct SessionLeaseGuard<B: AcpBackend> {
    session_id: SessionId,
    tool_nonce: Arc<str>,
    model_id: ModelId,
    declared_tools: Vec<CanonicalToolSpec>,
    last_context_used: u64,
    turn_count: u64,
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
                turn_count: 1,
                message_count: initial_message_count,
                created_at_unix_ms: now_ms,
                last_active_unix_ms: now_ms,
                last_response_id: None,
            },
            true,
        );
        Self {
            session_id,
            tool_nonce,
            model_id,
            declared_tools,
            last_context_used: 0,
            turn_count: 1,
            created_at_unix_ms: now_ms,
            pool,
            backend,
            armed: true,
        }
    }

    #[must_use]
    pub fn from_cached(
        cached: &IdleSessionEntry,
        new_message_count: usize,
        pool: Arc<SessionPool>,
        backend: Arc<B>,
    ) -> Self {
        let now_ms = unix_timestamp_ms();
        let next_turn = cached.turn_count.saturating_add(1);
        pool.register_lease(
            &cached.session_id,
            ActiveLeaseMeta {
                model_id: cached.model_id.clone(),
                turn_count: next_turn,
                message_count: new_message_count,
                created_at_unix_ms: cached.created_at_unix_ms,
                last_active_unix_ms: now_ms,
                last_response_id: cached.last_response_id.clone(),
            },
            false,
        );
        Self {
            session_id: cached.session_id.clone(),
            tool_nonce: Arc::clone(&cached.tool_nonce),
            model_id: cached.model_id.clone(),
            declared_tools: cached.declared_tools.clone(),
            last_context_used: cached.last_context_used,
            turn_count: next_turn,
            created_at_unix_ms: cached.created_at_unix_ms,
            pool,
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
    pub fn declared_tools(&self) -> &[CanonicalToolSpec] {
        &self.declared_tools
    }

    #[must_use]
    pub const fn last_context_used(&self) -> u64 {
        self.last_context_used
    }

    pub fn set_model_id(&mut self, model_id: ModelId) {
        self.pool.update_lease_model(&self.session_id, &model_id);
        self.model_id = model_id;
    }

    /// Disarms abort-on-drop and returns the healthy session to `pool` for future turns.
    pub fn commit(mut self, committed: CommittedSessionTurn) {
        self.armed = false;
        let now_ms = unix_timestamp_ms();
        let entry = IdleSessionEntry {
            session_id: self.session_id.clone(),
            tool_nonce: Arc::clone(&self.tool_nonce),
            model_id: self.model_id.clone(),
            post_turn_hash: committed.post_turn_hash,
            post_turn_dialogue_hash: committed.post_turn_dialogue_hash,
            declared_tools: committed.declared_tools,
            last_context_used: committed.last_context_used,
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
