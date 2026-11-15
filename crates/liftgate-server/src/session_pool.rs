use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use liftgate_types::{
    canonical::CanonicalMessage,
    config::{GatewayLimits, ModelId, SessionId},
    openai::ResponseObject,
    session_key::ConversationHash,
    telemetry::{SessionPoolTelemetry, SessionSlotState, SessionSummary},
};

use crate::{acp_client::AcpBackend, telemetry::unix_timestamp_ms};

/// Cached metadata for an idle ACP session awaiting a multi-turn continuation.
#[derive(Clone, Debug)]
pub struct IdleSessionEntry {
    pub session_id: SessionId,
    pub tool_nonce: Arc<str>,
    pub model_id: ModelId,
    pub requested_model_id: ModelId,
    pub failover_pinned: bool,
    pub post_turn_hash: ConversationHash,
    pub last_response_id: Option<String>,
    pub accumulated_messages: Vec<CanonicalMessage>,
    pub turn_count: u64,
    pub created_at_unix_ms: u64,
    pub last_active_unix_ms: u64,
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

/// Bounded LRU pool of idle ACP sessions, active leases, and recent OpenAI `ResponseObject` records.
pub struct SessionPool {
    limits: GatewayLimits,
    inner: Mutex<PoolInner>,
}

struct PoolInner {
    active_leases: BTreeMap<String, ActiveLeaseMeta>,
    idle_sessions: VecDeque<IdleSessionEntry>,
    stored_responses: VecDeque<(String, ResponseObject)>,
    total_sessions_created: u64,
    total_sessions_reused: u64,
    total_sessions_closed_or_evicted: u64,
    last_activity_unix_ms: u64,
}

impl SessionPool {
    #[must_use]
    pub fn new(limits: GatewayLimits) -> Self {
        Self {
            limits,
            inner: Mutex::new(PoolInner {
                active_leases: BTreeMap::new(),
                idle_sessions: VecDeque::with_capacity(limits.max_idle_sessions().get()),
                stored_responses: VecDeque::with_capacity(limits.max_stored_responses().get()),
                total_sessions_created: 0,
                total_sessions_reused: 0,
                total_sessions_closed_or_evicted: 0,
                last_activity_unix_ms: unix_timestamp_ms(),
            }),
        }
    }

    /// Atomically removes and returns an idle session matching `previous_response_id`.
    #[must_use]
    pub fn checkout_by_response_id(&self, response_id: &str) -> Option<IdleSessionEntry> {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let pos = guard
            .idle_sessions
            .iter()
            .rposition(|e| e.last_response_id.as_deref() == Some(response_id))?;
        guard.total_sessions_reused += 1;
        guard.last_activity_unix_ms = unix_timestamp_ms();
        guard.idle_sessions.remove(pos)
    }

    /// Atomically removes and returns an idle session matching a canonical prefix hash.
    #[must_use]
    pub fn checkout_by_hash(&self, hash: &ConversationHash) -> Option<IdleSessionEntry> {
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
        guard.idle_sessions.remove(pos)
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
            guard.idle_sessions.remove(pos)
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
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.stored_responses.len() >= self.limits.max_stored_responses().get() {
            let _ = guard.stored_responses.pop_front();
        }
        guard
            .stored_responses
            .push_back((response.id.clone(), response));
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
            .find(|(id, _)| id == response_id)
            .map(|(_, obj)| obj.clone())
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
                message_count: idle.accumulated_messages.len(),
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

    fn checkin<B: AcpBackend>(&self, entry: IdleSessionEntry, backend: &B) {
        let evicted = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.last_activity_unix_ms = entry.last_active_unix_ms;
            let _ = guard.active_leases.remove(entry.session_id.as_str());
            let evicted = if guard.idle_sessions.len() >= self.limits.max_idle_sessions().get() {
                guard.total_sessions_closed_or_evicted += 1;
                guard.idle_sessions.pop_front()
            } else {
                None
            };
            guard.idle_sessions.push_back(entry);
            evicted
        };

        if let Some(old) = evicted {
            backend.abort_session_nonblocking(&old.session_id);
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
    requested_model_id: ModelId,
    failover_pinned: bool,
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
        let requested_model_id = model_id.clone();
        Self {
            session_id,
            tool_nonce,
            model_id,
            requested_model_id,
            failover_pinned: false,
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
            requested_model_id: cached.requested_model_id.clone(),
            failover_pinned: cached.failover_pinned,
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
    pub const fn requested_model_id(&self) -> &ModelId {
        &self.requested_model_id
    }

    #[must_use]
    pub const fn is_failover_pinned(&self) -> bool {
        self.failover_pinned
    }

    pub fn mark_failover_from(&mut self, requested_model_id: ModelId) {
        self.requested_model_id = requested_model_id;
        self.failover_pinned = true;
    }

    pub fn set_model_id(&mut self, model_id: ModelId) {
        self.pool.update_lease_model(&self.session_id, &model_id);
        self.requested_model_id = model_id.clone();
        self.failover_pinned = false;
        self.model_id = model_id;
    }

    /// Disarms abort-on-drop and returns the healthy session to `pool` for future turns.
    pub fn commit(
        mut self,
        post_turn_hash: ConversationHash,
        last_response_id: Option<String>,
        accumulated_messages: Vec<CanonicalMessage>,
    ) {
        self.armed = false;
        let now_ms = unix_timestamp_ms();
        let entry = IdleSessionEntry {
            session_id: self.session_id.clone(),
            tool_nonce: Arc::clone(&self.tool_nonce),
            model_id: self.model_id.clone(),
            requested_model_id: self.requested_model_id.clone(),
            failover_pinned: self.failover_pinned,
            post_turn_hash,
            last_response_id,
            accumulated_messages,
            turn_count: self.turn_count,
            created_at_unix_ms: self.created_at_unix_ms,
            last_active_unix_ms: now_ms,
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
