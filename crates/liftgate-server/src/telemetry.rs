use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use liftgate_types::{
    acp::AcpUsage,
    canonical::{CanonicalMessage, CanonicalPart, CanonicalRole},
    telemetry::{GatewayMetricsSummary, TaskPhase, TaskProtocol, TaskSnapshot},
};
use tokio::sync::{broadcast, watch};

const MAX_RECENT_TASKS: usize = 100;
const MAX_PROMPT_PREVIEW_CHARS: usize = 180;
const MAX_OUTPUT_PREVIEW_CHARS: usize = 240;

/// Central bounded telemetry store and real-time event notifier for the companion web UI.
pub struct TelemetryHub {
    inner: Mutex<TelemetryInner>,
    notify_tx: broadcast::Sender<()>,
    shutdown_tx: watch::Sender<bool>,
}

struct TelemetryInner {
    metrics: GatewayMetricsSummary,
    active_tasks: BTreeMap<String, TaskSnapshot>,
    recent_tasks: VecDeque<TaskSnapshot>,
}

impl Default for TelemetryHub {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetryHub {
    #[must_use]
    pub fn new() -> Self {
        let (notify_tx, _) = broadcast::channel(64);
        let (shutdown_tx, _) = watch::channel(false);
        Self {
            inner: Mutex::new(TelemetryInner {
                metrics: GatewayMetricsSummary::default(),
                active_tasks: BTreeMap::new(),
                recent_tasks: VecDeque::with_capacity(MAX_RECENT_TASKS),
            }),
            notify_tx,
            shutdown_tx,
        }
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.notify_tx.subscribe()
    }

    pub fn notify_change(&self) {
        let _ = self.notify_tx.send(());
    }

    /// Subscribes to gateway shutdown so long-lived telemetry SSE streams close immediately
    /// instead of blocking `axum`'s graceful connection drain.
    #[must_use]
    pub fn subscribe_shutdown(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    /// Signals all open telemetry SSE streams to close cleanly.
    pub fn signal_shutdown(&self) {
        let _ = self.shutdown_tx.send_replace(true);
    }

    /// Registers a newly arrived API turn and returns an RAII `ActiveTaskGuard` that guarantees
    /// cleanup into `recent_tasks` even on SSE client disconnect, timeout, or early `?`.
    #[must_use]
    pub fn begin_task(
        self: &Arc<Self>,
        task_id: String,
        protocol: TaskProtocol,
        stream: bool,
        requested_model: String,
        messages: &[CanonicalMessage],
    ) -> ActiveTaskGuard {
        let now_ms = unix_timestamp_ms();
        let prompt_preview = extract_last_user_preview(messages);

        let snapshot = TaskSnapshot {
            task_id: task_id.clone(),
            protocol,
            stream,
            requested_model: requested_model.clone(),
            resolved_model: requested_model,
            session_id: None,
            session_reused: false,
            phase: TaskPhase::InitializingSession,
            started_at_unix_ms: now_ms,
            finished_at_unix_ms: None,
            elapsed_ms: 0,
            thought_chars: 0,
            output_chars: 0,
            client_tool_calls: Vec::new(),
            native_web_searches: 0,
            native_web_fetches: 0,
            prompt_preview,
            output_preview: String::new(),
            stop_reason: None,
            error_message: None,
            usage: None,
        };

        {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.metrics.total_requests += 1;
            match protocol {
                TaskProtocol::OpenaiResponses => guard.metrics.openai_requests += 1,
                TaskProtocol::AnthropicMessages => guard.metrics.anthropic_requests += 1,
            }
            guard.active_tasks.insert(task_id.clone(), snapshot);
            guard.metrics.active_requests = guard.active_tasks.len();
        }
        self.notify_change();

        ActiveTaskGuard {
            hub: Arc::clone(self),
            task_id,
            armed: true,
        }
    }

    /// Returns `(metrics, active_tasks, recent_tasks)` with live `elapsed_ms` populated.
    #[must_use]
    pub fn snapshot_tasks(&self) -> (GatewayMetricsSummary, Vec<TaskSnapshot>, Vec<TaskSnapshot>) {
        let now_ms = unix_timestamp_ms();
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        let mut active: Vec<TaskSnapshot> = guard
            .active_tasks
            .values()
            .cloned()
            .map(|mut t| {
                t.elapsed_ms = now_ms.saturating_sub(t.started_at_unix_ms);
                t
            })
            .collect();
        active.sort_by_key(|b| std::cmp::Reverse(b.started_at_unix_ms));

        let recent: Vec<TaskSnapshot> = guard.recent_tasks.iter().rev().cloned().collect();
        let mut metrics = guard.metrics;
        metrics.active_requests = active.len();

        (metrics, active, recent)
    }
}

/// RAII tracker for an in-flight gateway turn.
pub struct ActiveTaskGuard {
    hub: Arc<TelemetryHub>,
    task_id: String,
    armed: bool,
}

impl ActiveTaskGuard {
    pub fn bind_session(&self, session_id: &str, reused: bool, resolved_model: &str) {
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                task.session_id = Some(session_id.to_owned());
                task.session_reused = reused;
                task.resolved_model = resolved_model.to_owned();
            }
        }
        self.hub.notify_change();
    }

    pub fn record_thought(&self, delta_chars: usize) {
        let should_notify = {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                let phase_changed = task.phase != TaskPhase::Thinking;
                task.phase = TaskPhase::Thinking;
                task.thought_chars = task.thought_chars.saturating_add(delta_chars);
                phase_changed
            } else {
                false
            }
        };
        if should_notify {
            self.hub.notify_change();
        }
    }

    pub fn record_web_search(&self) {
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                task.phase = TaskPhase::SearchingWeb;
                task.native_web_searches = task.native_web_searches.saturating_add(1);
            }
        }
        self.hub.notify_change();
    }

    pub fn record_web_fetch(&self) {
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                task.phase = TaskPhase::FetchingWeb;
                task.native_web_fetches = task.native_web_fetches.saturating_add(1);
            }
        }
        self.hub.notify_change();
    }

    pub fn record_text(&self, delta: &str) {
        let should_notify = {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                let phase_changed = task.phase != TaskPhase::StreamingText;
                task.phase = TaskPhase::StreamingText;
                task.output_chars = task.output_chars.saturating_add(delta.chars().count());
                append_preview_bounded(&mut task.output_preview, delta, MAX_OUTPUT_PREVIEW_CHARS);
                phase_changed
            } else {
                false
            }
        };
        if should_notify {
            self.hub.notify_change();
        }
    }

    pub fn record_client_tool(&self, tool_name: &str) {
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
                task.phase = TaskPhase::CallingClientTool;
                task.client_tool_calls.push(tool_name.to_owned());
            }
        }
        self.hub.notify_change();
    }

    pub fn record_tool_delta(&self, delta_chars: usize) {
        if delta_chars == 0 {
            return;
        }
        let mut guard = match self.hub.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(task) = guard.active_tasks.get_mut(&self.task_id) {
            task.output_chars = task.output_chars.saturating_add(delta_chars);
        }
    }

    pub fn complete(mut self, stop_reason: String, usage: AcpUsage) {
        self.armed = false;
        let now_ms = unix_timestamp_ms();
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(mut task) = guard.active_tasks.remove(&self.task_id) {
                task.phase = TaskPhase::Completed;
                task.finished_at_unix_ms = Some(now_ms);
                task.elapsed_ms = now_ms.saturating_sub(task.started_at_unix_ms);
                task.stop_reason = Some(stop_reason);
                task.usage = Some(usage);
                push_recent(&mut guard.recent_tasks, task);
            }
            guard.metrics.active_requests = guard.active_tasks.len();
            guard.metrics.completed_requests += 1;
            guard.metrics.total_input_tokens = guard
                .metrics
                .total_input_tokens
                .saturating_add(usage.input_tokens);
            guard.metrics.total_output_tokens = guard
                .metrics
                .total_output_tokens
                .saturating_add(usage.output_tokens);
            guard.metrics.total_thought_tokens = guard
                .metrics
                .total_thought_tokens
                .saturating_add(usage.thought_tokens);
            guard.metrics.total_cached_tokens = guard
                .metrics
                .total_cached_tokens
                .saturating_add(usage.cached_read_tokens);
        }
        self.hub.notify_change();
    }

    pub fn fail(mut self, error_message: String) {
        self.armed = false;
        let now_ms = unix_timestamp_ms();
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(mut task) = guard.active_tasks.remove(&self.task_id) {
                task.phase = TaskPhase::Failed;
                task.finished_at_unix_ms = Some(now_ms);
                task.elapsed_ms = now_ms.saturating_sub(task.started_at_unix_ms);
                task.error_message = Some(error_message);
                push_recent(&mut guard.recent_tasks, task);
            }
            guard.metrics.active_requests = guard.active_tasks.len();
            guard.metrics.failed_requests += 1;
        }
        self.hub.notify_change();
    }
}

impl Drop for ActiveTaskGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let now_ms = unix_timestamp_ms();
        {
            let mut guard = match self.hub.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(mut task) = guard.active_tasks.remove(&self.task_id) {
                task.phase = TaskPhase::Cancelled;
                task.finished_at_unix_ms = Some(now_ms);
                task.elapsed_ms = now_ms.saturating_sub(task.started_at_unix_ms);
                task.error_message = Some("client disconnected or turn cancelled".to_owned());
                push_recent(&mut guard.recent_tasks, task);
            }
            guard.metrics.active_requests = guard.active_tasks.len();
            guard.metrics.cancelled_requests += 1;
        }
        self.hub.notify_change();
    }
}

fn push_recent(deque: &mut VecDeque<TaskSnapshot>, task: TaskSnapshot) {
    if deque.len() >= MAX_RECENT_TASKS {
        let _ = deque.pop_front();
    }
    deque.push_back(task);
}

fn extract_last_user_preview(messages: &[CanonicalMessage]) -> String {
    for msg in messages.iter().rev() {
        if msg.role == CanonicalRole::User {
            for part in msg.parts.iter().rev() {
                match part {
                    CanonicalPart::Text(t) if !t.trim().is_empty() => {
                        return truncate_chars(t.trim(), MAX_PROMPT_PREVIEW_CHARS);
                    }
                    CanonicalPart::ToolResult {
                        call_id, output, ..
                    } => {
                        let summary = format!("[tool_result:{call_id}] {}", output.trim());
                        return truncate_chars(&summary, MAX_PROMPT_PREVIEW_CHARS);
                    }
                    _ => {}
                }
            }
        }
    }
    String::new()
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in input.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn append_preview_bounded(buf: &mut String, delta: &str, max_chars: usize) {
    let current_chars = buf.chars().count();
    if current_chars >= max_chars {
        return;
    }
    let remaining = max_chars - current_chars;
    for (idx, ch) in delta.chars().enumerate() {
        if idx >= remaining {
            buf.push('…');
            break;
        }
        buf.push(ch);
    }
}

#[must_use]
pub fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
