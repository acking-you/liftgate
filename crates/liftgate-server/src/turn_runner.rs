use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use liftgate_types::{
    acp::{AcpPromptBlock, AcpUsage, PromptTurnResult, SessionUpdatePayload},
    canonical::{
        AuxiliaryTaskKind, CanonicalMessage, CanonicalPart, CanonicalRole, CanonicalToolKind,
        NormalizedTurnRequest,
    },
    config::{AcpModeId, GatewayLimits, ModelId},
    model::{resolve_model_id, select_fastest_gemini_model},
    parser::{IncrementalToolParser, ParsedStreamItem, ParsedToolCall},
    session_key::{
        AcpPromptPlan, INTERNAL_GATEWAY_WEB_FETCH_TOOL, MAX_ACP_PROMPT_TEXT_BYTES,
        WebToolBridgeConfig, build_acp_prompt_plan, build_client_tools_delta_xml,
        build_compaction_directive_xml, build_session_system_append_with_spill,
        compute_routing_plan, hash_conversation_with_reply, hash_dialogue_with_reply,
        truncate_head_tail_lines,
    },
    telemetry::TaskProtocol,
};
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, timeout, timeout_at},
};
use tokio_util::sync::{CancellationToken, DropGuard};
use uuid::Uuid;

use crate::{
    acp_client::AcpBackend,
    error::{AcpError, GatewayError},
    session_pool::{SessionLeaseGuard, SessionPool},
    telemetry::{ActiveTaskGuard, TelemetryHub, unix_timestamp_ms},
    web_fetch::WebFetchOutcome,
};

static PROCESS_TOOL_NONCE: OnceLock<Arc<str>> = OnceLock::new();

/// Process-wide stable XML tool-call nonce so `<api_gateway_bridge_rules>` remains 100%
/// byte-identical across sessions (preserving Gemini KV prefix cache hits) while remaining
/// unpredictable across gateway restarts.
fn gateway_process_tool_nonce() -> Arc<str> {
    Arc::clone(PROCESS_TOOL_NONCE.get_or_init(|| {
        let s = generate_short_nonce();
        Arc::from(s.as_str())
    }))
}

/// Normalized streaming event emitted during an ACP prompt turn.
#[derive(Clone, Debug)]
pub enum TurnEvent {
    ThoughtDelta(String),
    WebSearch {
        call_id: String,
        status: String,
        query: Option<String>,
    },
    WebFetchStart {
        call_id: String,
        url: String,
    },
    WebFetchComplete {
        call_id: String,
        outcome: WebFetchOutcome,
    },
    TextDelta(String),
    ClientToolCallStart {
        call_id: String,
        name: String,
        is_custom: bool,
    },
    ClientToolCallDelta {
        call_id: String,
        delta: String,
    },
    ClientToolCall {
        call_id: String,
        name: String,
        arguments: String,
        is_custom: bool,
    },
    Completed {
        usage: AcpUsage,
        stop_reason: String,
    },
    Failed(Arc<AcpError>),
}

/// Active turn stream handle; dropping it before completion cancels the turn immediately.
pub struct ActiveTurnStream {
    pub resolved_model: ModelId,
    pub response_id: String,
    pub rx: mpsc::Receiver<TurnEvent>,
    _cancel_guard: DropGuard,
    _worker: JoinHandle<()>,
}

impl Drop for ActiveTurnStream {
    fn drop(&mut self) {
        self._worker.abort();
    }
}

/// Shared gateway runtime configuration.
#[derive(Clone, Debug)]
pub struct GatewayRuntimeConfig {
    pub long_context: bool,
    pub default_model: ModelId,
    pub acp_mode: AcpModeId,
    pub api_key: Option<Arc<str>>,
    pub limits: GatewayLimits,
}

/// Prepares an ACP session (reusing a matching multi-turn session when available, or creating
/// a new isolated one) and starts streaming `TurnEvent`s.
pub async fn execute_normalized_turn<B: AcpBackend>(
    backend: Arc<B>,
    pool: Arc<SessionPool>,
    telemetry: &Arc<TelemetryHub>,
    cfg: &GatewayRuntimeConfig,
    protocol: TaskProtocol,
    mut req: NormalizedTurnRequest,
    response_id: String,
) -> Result<ActiveTurnStream, GatewayError> {
    if let Some(aux_kind) = req.auxiliary_task {
        req.client_tools.clear();
        if !matches!(aux_kind, AuxiliaryTaskKind::WebSearchSubrequest) {
            req.enable_native_web_search = false;
            req.enable_native_web_fetch = false;
        }
    }
    req.materialize_system_instructions();

    let requested_label = req
        .requested_model
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| cfg.default_model.as_str().to_owned());
    let task_guard = telemetry.begin_task(
        response_id.clone(),
        protocol,
        req.stream,
        requested_label,
        &req.messages,
    );

    let timeout_secs = cfg.limits.turn_timeout_secs().get();
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);

    let available = backend.refresh_models_if_empty().await;
    let resolved_model = if let Some(aux_kind) = req.auxiliary_task {
        let fast_model = select_fastest_gemini_model(&available, &cfg.default_model);
        tracing::debug!(
            auxiliary_task = ?aux_kind,
            resolved_model = %fast_model,
            "routing harness auxiliary/classifier sub-request to fastest low-cost Gemini model"
        );
        fast_model
    } else {
        resolve_model_id(
            req.requested_model.as_deref(),
            req.reasoning_effort,
            &available,
            &cfg.default_model,
        )
    };
    let hydration_seed_model = select_fastest_gemini_model(&available, &resolved_model);

    let web_cfg = WebToolBridgeConfig {
        enable_native_web_search: req.enable_native_web_search,
        force_native_web_search: req.force_native_web_search,
        enable_native_web_fetch: req.enable_native_web_fetch,
        force_native_web_fetch: req.force_native_web_fetch,
    };
    let is_ephemeral_aux = req.auxiliary_task.is_some();

    let setup_res: Result<_, GatewayError> = async {
        let mut is_compaction_turn = false;
        // 1. Attempt stateful `previous_response_id` or canonical prefix-hash continuation
        //    ONLY for interactive turns (auxiliary tasks such as forked away-recaps must never
        //    check out or cancel the user's active conversation session).
        if !is_ephemeral_aux {
            if let Some(prev_resp_id) = req.previous_response_id.as_deref()
                && let Some(cached) = pool.checkout_by_response_id(prev_resp_id)
            {
                let mut combined = cached.accumulated_messages.clone();
                combined.extend(req.messages.iter().cloned());
                let mut guard = SessionLeaseGuard::from_cached(
                    &cached,
                    combined.len(),
                    Arc::clone(&pool),
                    Arc::clone(&backend),
                );
                let tools_delta = build_client_tools_delta_xml(
                    guard.declared_tools(),
                    &req.client_tools,
                    guard.tool_nonce(),
                );
                let plan = build_acp_prompt_plan(
                    &req.messages,
                    guard.tool_nonce(),
                    false,
                    tools_delta.as_deref(),
                    cfg.long_context,
                )?;
                let initial_rpc_model = if plan.seed_chunks.is_empty() {
                    &resolved_model
                } else {
                    &hydration_seed_model
                };
                if guard.model_id() != initial_rpc_model {
                    timeout_at(
                        deadline,
                        backend.set_session_model(guard.session_id(), initial_rpc_model),
                    )
                    .await
                    .map_err(|_| AcpError::TurnTimeout {
                        seconds: timeout_secs,
                    })??;
                }
                if guard.model_id() != &resolved_model {
                    guard.set_model_id(resolved_model.clone());
                }
                return Ok((
                    guard,
                    true,
                    false,
                    initial_rpc_model.clone(),
                    plan,
                    combined,
                ));
            }

            let routing = compute_routing_plan(
                req.system_instructions.as_deref(),
                &req.client_tools,
                web_cfg,
                &resolved_model,
                &req.messages,
            );
            is_compaction_turn = routing.compaction_lookup.is_some();

            if let Some((prefix_hash, delta_start)) = routing.prefix_lookup
                && let Some(cached) = pool.checkout_by_hash(&prefix_hash)
                && let Some(delta_slice) = req.messages.get(delta_start..)
            {
                let mut guard = SessionLeaseGuard::from_cached(
                    &cached,
                    req.messages.len(),
                    Arc::clone(&pool),
                    Arc::clone(&backend),
                );
                let tools_delta = build_client_tools_delta_xml(
                    guard.declared_tools(),
                    &req.client_tools,
                    guard.tool_nonce(),
                );
                let plan = build_acp_prompt_plan(
                    delta_slice,
                    guard.tool_nonce(),
                    false,
                    tools_delta.as_deref(),
                    cfg.long_context,
                )?;
                let initial_rpc_model = if plan.seed_chunks.is_empty() {
                    &resolved_model
                } else {
                    &hydration_seed_model
                };
                if guard.model_id() != initial_rpc_model {
                    timeout_at(
                        deadline,
                        backend.set_session_model(guard.session_id(), initial_rpc_model),
                    )
                    .await
                    .map_err(|_| AcpError::TurnTimeout {
                        seconds: timeout_secs,
                    })??;
                }
                if guard.model_id() != &resolved_model {
                    guard.set_model_id(resolved_model.clone());
                }
                return Ok((
                    guard,
                    true,
                    is_compaction_turn,
                    initial_rpc_model.clone(),
                    plan,
                    req.messages.clone(),
                ));
            }

            if let Some((dialogue_hash, delta_start)) = routing.compaction_lookup
                && let Some(cached) = pool.checkout_by_dialogue_hash(&dialogue_hash)
                && let Some(delta_slice) = req.messages.get(delta_start..)
            {
                let mut guard = SessionLeaseGuard::from_cached(
                    &cached,
                    req.messages.len(),
                    Arc::clone(&pool),
                    Arc::clone(&backend),
                );
                let tools_delta = build_client_tools_delta_xml(
                    guard.declared_tools(),
                    &req.client_tools,
                    guard.tool_nonce(),
                );
                let directive = build_compaction_directive_xml(
                    req.system_instructions.as_deref(),
                    tools_delta.as_deref(),
                );
                let plan = build_acp_prompt_plan(
                    delta_slice,
                    guard.tool_nonce(),
                    false,
                    directive.as_deref(),
                    cfg.long_context,
                )?;
                let initial_rpc_model = if plan.seed_chunks.is_empty() {
                    &resolved_model
                } else {
                    &hydration_seed_model
                };
                if guard.model_id() != initial_rpc_model {
                    timeout_at(
                        deadline,
                        backend.set_session_model(guard.session_id(), initial_rpc_model),
                    )
                    .await
                    .map_err(|_| AcpError::TurnTimeout {
                        seconds: timeout_secs,
                    })??;
                }
                if guard.model_id() != &resolved_model {
                    guard.set_model_id(resolved_model.clone());
                }
                tracing::info!(
                    session_id = %guard.session_id(),
                    delta_messages = delta_slice.len(),
                    "reusing warm pre-compaction ACP session for harness context compaction"
                );
                return Ok((
                    guard,
                    true,
                    true,
                    initial_rpc_model.clone(),
                    plan,
                    req.messages.clone(),
                ));
            }
        }

        let nonce = gateway_process_tool_nonce();
        let (system_append, initial_tools_spill) = build_session_system_append_with_spill(
            req.system_instructions.as_deref(),
            &req.client_tools,
            web_cfg,
            &nonce,
        );

        let created = timeout_at(deadline, backend.create_session(&system_append))
            .await
            .map_err(|_| AcpError::TurnTimeout {
                seconds: timeout_secs,
            })??;

        let guard = SessionLeaseGuard::new_fresh(
            created.session_id,
            nonce,
            resolved_model.clone(),
            req.client_tools.clone(),
            req.messages.len(),
            Arc::clone(&pool),
            Arc::clone(&backend),
        );

        let plan = build_acp_prompt_plan(
            &req.messages,
            guard.tool_nonce(),
            true,
            initial_tools_spill.as_deref(),
            cfg.long_context,
        )?;
        let initial_rpc_model = if plan.seed_chunks.is_empty() {
            &resolved_model
        } else {
            &hydration_seed_model
        };

        timeout_at(
            deadline,
            backend.set_session_model(guard.session_id(), initial_rpc_model),
        )
        .await
        .map_err(|_| AcpError::TurnTimeout {
            seconds: timeout_secs,
        })??;

        timeout_at(
            deadline,
            backend.set_session_mode(guard.session_id(), &cfg.acp_mode),
        )
        .await
        .map_err(|_| AcpError::TurnTimeout {
            seconds: timeout_secs,
        })??;

        Ok((
            guard,
            false,
            is_ephemeral_aux || is_compaction_turn,
            initial_rpc_model.clone(),
            plan,
            req.messages.clone(),
        ))
    }
    .await;

    let (lease, session_reused, retire_after_turn, initial_rpc_model, prompt_plan, base_messages) =
        match setup_res {
            Ok(t) => t,
            Err(err) => {
                let err = upgrade_gateway_error(err, &resolved_model);
                task_guard.fail(err.to_string());
                return Err(err);
            }
        };

    let active_model = lease.model_id().clone();
    task_guard.bind_session(
        lease.session_id().as_str(),
        session_reused,
        active_model.as_str(),
    );

    let turn_cancel = CancellationToken::new();
    let cancel_guard = turn_cancel.clone().drop_guard();
    let cap = cfg.limits.channel_capacity().get();
    let (events_tx, events_rx) = mpsc::channel::<TurnEvent>(cap);

    let worker_resp_id = response_id.clone();
    let worker = tokio::spawn(run_turn_pipeline(
        backend,
        lease,
        task_guard,
        prompt_plan,
        base_messages,
        req,
        web_cfg,
        active_model,
        initial_rpc_model,
        retire_after_turn,
        worker_resp_id,
        deadline,
        timeout_secs,
        cap,
        events_tx,
        turn_cancel,
    ));

    Ok(ActiveTurnStream {
        resolved_model,
        response_id,
        rx: events_rx,
        _cancel_guard: cancel_guard,
        _worker: worker,
    })
}

const TRANSCRIPT_RUNAWAY_MARKERS: [&str; 8] = [
    "</assistant>",
    "<assistant>",
    "</user>",
    "<client_tool_result",
    "</client_tool_result>",
    "[Client Tool Result id=",
    "--- Prior Conversation Context",
    "### Prior User Turn",
];

const MAX_POST_TOOL_BRIDGE_NON_WS_CHARS: usize = 120;

struct StreamingClientToolState {
    name: String,
    call_id: String,
    is_custom: bool,
    emitted_start: bool,
}

/// Mutable per-turn stream accumulator shared across `forward_acp_update` and multi-step `web_fetch`.
struct TurnStreamState {
    parsed_buf: Vec<ParsedStreamItem>,
    assistant_parts: Vec<CanonicalPart>,
    text_accumulator: String,
    post_tool_spill_buf: String,
    active_searches: HashMap<String, Option<String>>,
    seen_search_ids: HashSet<String>,
    seen_client_tools: HashSet<(String, String)>,
    pending_web_fetches: Vec<(String, String)>,
    active_streaming_tool: Option<StreamingClientToolState>,
    truncated_tool_abort: Option<String>,
    emitted_client_tool: bool,
    halted_post_tool_runaway: bool,
    tainted_session: bool,
    peak_context_used: u64,
    turn_peak_context_used: u64,
    latest_context_used: u64,
}

impl TurnStreamState {
    fn new(initial_context_used: u64) -> Self {
        Self {
            parsed_buf: Vec::new(),
            assistant_parts: Vec::new(),
            text_accumulator: String::new(),
            post_tool_spill_buf: String::new(),
            active_searches: HashMap::new(),
            seen_search_ids: HashSet::new(),
            seen_client_tools: HashSet::new(),
            pending_web_fetches: Vec::new(),
            active_streaming_tool: None,
            truncated_tool_abort: None,
            emitted_client_tool: false,
            halted_post_tool_runaway: false,
            tainted_session: false,
            peak_context_used: initial_context_used,
            turn_peak_context_used: 0,
            latest_context_used: initial_context_used,
        }
    }

    fn has_open_streamed_tool(&self) -> bool {
        self.active_streaming_tool
            .as_ref()
            .is_some_and(|active| active.emitted_start)
    }

    fn take_open_streamed_tool_name(&mut self) -> Option<String> {
        if self.has_open_streamed_tool() {
            self.active_streaming_tool.take().map(|active| active.name)
        } else {
            None
        }
    }

    fn observe_usage_update(&mut self, used: u64, size: u64) {
        if used == 0 {
            return;
        }
        if self.peak_context_used >= 64_000 && used.saturating_mul(2) < self.peak_context_used {
            tracing::info!(
                previous_peak_tokens = self.peak_context_used,
                compacted_tokens = used,
                window_size = size,
                "detected internal `agy acp` post-turn context compaction; retiring session after delivering current reply"
            );
            self.tainted_session = true;
        }
        self.peak_context_used = self.peak_context_used.max(used);
        self.turn_peak_context_used = self.turn_peak_context_used.max(used);
        self.latest_context_used = used;
    }

    fn flush_text_part(&mut self) {
        let trimmed = self.text_accumulator.trim();
        if !trimmed.is_empty() {
            self.assistant_parts
                .push(CanonicalPart::Text(trimmed.to_owned()));
        }
        self.text_accumulator.clear();
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal turn pipeline task owns explicit moved state without an extra heap struct"
)]
async fn run_turn_pipeline<B: AcpBackend>(
    backend: Arc<B>,
    initial_lease: SessionLeaseGuard<B>,
    task_guard: ActiveTaskGuard,
    prompt_plan: AcpPromptPlan,
    mut full_messages: Vec<CanonicalMessage>,
    req: NormalizedTurnRequest,
    web_cfg: WebToolBridgeConfig,
    active_model: ModelId,
    mut current_rpc_model: ModelId,
    retire_after_turn: bool,
    response_id: String,
    deadline: Instant,
    timeout_secs: u64,
    channel_cap: usize,
    events_tx: mpsc::Sender<TurnEvent>,
    cancel: CancellationToken,
) {
    let session_id = initial_lease.session_id().clone();
    let active_tool_nonce = initial_lease.tool_nonce().to_owned();
    let initial_context_used = initial_lease.last_context_used();
    let mut lease = Some(initial_lease);
    let has_xml_tools = !req.client_tools.is_empty() || req.enable_native_web_fetch;
    let max_fetch_uses = req.web_fetch_max_uses.unwrap_or(5).clamp(1, 10) as usize;
    let mut completed_web_fetches: usize = 0;
    let mut cumulative_usage = AcpUsage::default();
    let mut context_usage: AcpUsage;
    let mut final_stop_reason = "end_turn".to_owned();
    let AcpPromptPlan {
        seed_chunks,
        final_blocks,
    } = prompt_plan;
    let outbound_prompt_bytes: usize = seed_chunks
        .iter()
        .flat_map(|chunk| chunk.iter())
        .chain(final_blocks.iter())
        .map(|b| match b {
            AcpPromptBlock::Text { text } => text.len(),
            AcpPromptBlock::Image { .. } => 8_192,
        })
        .sum();
    let mut current_prompt_blocks = final_blocks;
    let mut state = TurnStreamState::new(initial_context_used);
    let mut last_step_started_ms: u64;
    let mut last_suppressed_banners: Vec<String>;
    let mut last_turn_error: Option<String>;

    if !seed_chunks.is_empty() {
        tracing::info!(
            session_id = %session_id,
            seed_chunks = seed_chunks.len(),
            seed_model = %current_rpc_model,
            target_model = %active_model,
            "hydrating multi-chunk conversation context on fast Flash model before target turn"
        );
        for chunk_blocks in seed_chunks {
            if let Err(err) = run_hydration_seed_step(
                &backend,
                &session_id,
                &chunk_blocks,
                &current_rpc_model,
                &mut state,
                deadline,
                timeout_secs,
                channel_cap,
                &cancel,
            )
            .await
            .map(|usage| accumulate_usage(&mut cumulative_usage, usage))
            {
                if matches!(err, AcpError::Cancelled) {
                    return;
                }
                if let Some(old_lease) = lease.take() {
                    old_lease.finish_ephemeral();
                }
                let err = upgrade_acp_error(err, &active_model);
                tracing::error!(error = ?err, "ACP context hydration seed step failed");
                task_guard.fail(err.to_string());
                let _ = events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
                return;
            }
        }

        if current_rpc_model != active_model {
            let switch_res = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                timed = timeout_at(deadline, backend.set_session_model(&session_id, &active_model)) => {
                    match timed {
                        Ok(inner) => inner,
                        Err(_) => Err(AcpError::TurnTimeout { seconds: timeout_secs }),
                    }
                }
            };
            if let Err(err) = switch_res {
                if let Some(old_lease) = lease.take() {
                    old_lease.finish_ephemeral();
                }
                let err = upgrade_acp_error(err, &active_model);
                tracing::error!(error = ?err, "failed to switch hydrated ACP session to target model");
                task_guard.fail(err.to_string());
                let _ = events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
                return;
            }
            current_rpc_model = active_model.clone();
        }
    }
    let _ = &current_rpc_model;

    loop {
        last_step_started_ms = unix_timestamp_ms();
        state.halted_post_tool_runaway = false;
        state.post_tool_spill_buf.clear();
        state.active_streaming_tool = None;
        state.truncated_tool_abort = None;

        let (acp_tx, mut acp_rx) = mpsc::channel::<SessionUpdatePayload>(channel_cap);
        let mut parser = IncrementalToolParser::new(&active_tool_nonce, has_xml_tools);
        let mut banner_filter = if has_xml_tools {
            AcpBannerFilter::with_tool_nonce(&active_tool_nonce)
        } else {
            AcpBannerFilter::new()
        };
        let mut filter_out = String::new();

        let prompt_backend = Arc::clone(&backend);
        let prompt_session_id = session_id.clone();
        let step_blocks = current_prompt_blocks;
        let mut prompt_fut = std::pin::pin!(async move {
            timeout_at(
                deadline,
                prompt_backend.start_prompt(&prompt_session_id, &step_blocks, acp_tx),
            )
            .await
        });

        let mut prompt_outcome: Option<Result<PromptTurnResult, AcpError>> = None;
        let mut upstream_abort: Option<(UpstreamBannerSignal, String)> = None;

        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return;
                }
                maybe_upd = acp_rx.recv() => {
                    if let Some(upd) = maybe_upd {
                        if !forward_acp_update(
                            upd,
                            &req,
                            &task_guard,
                            &mut banner_filter,
                            &mut filter_out,
                            &mut parser,
                            &mut state,
                            &events_tx,
                            &cancel,
                        ).await {
                            return;
                        }
                        if state.truncated_tool_abort.is_some() {
                            break;
                        }
                        if banner_filter.take_malformed_retry()
                            || banner_filter.take_transient_retry()
                        {
                            if let Some(open_name) = state.take_open_streamed_tool_name() {
                                state.truncated_tool_abort = Some(open_name);
                                break;
                            }
                            if state.emitted_client_tool || !state.pending_web_fetches.is_empty() {
                                tracing::info!(
                                    "short-circuiting internal ACP post-tool retry banner because XML tool call was already captured"
                                );
                                state.halted_post_tool_runaway = true;
                            }
                        }
                        if let Some((sig, detail)) = banner_filter.take_signal() {
                            let no_output_yet = state.text_accumulator.is_empty()
                                && state.assistant_parts.is_empty()
                                && !state.emitted_client_tool
                                && state.seen_search_ids.is_empty()
                                && completed_web_fetches == 0;
                            if state.has_open_streamed_tool()
                                || no_output_yet
                                || (sig == UpstreamBannerSignal::QuotaExhausted
                                    && !state.emitted_client_tool)
                            {
                                state.active_streaming_tool = None;
                                upstream_abort = Some((sig, detail));
                                break;
                            }
                            state.halted_post_tool_runaway = true;
                        }
                        if state.halted_post_tool_runaway
                            && let Some(open_name) = state.take_open_streamed_tool_name()
                        {
                            state.halted_post_tool_runaway = false;
                            state.truncated_tool_abort = Some(open_name);
                            break;
                        }
                        if state.halted_post_tool_runaway {
                            state.tainted_session = true;
                            if !state.pending_web_fetches.is_empty() && !state.emitted_client_tool {
                                // Keep session alive for server-side `web_fetch` continuation; ask
                                // ACP to stop only the current step's generation.
                                backend.cancel_prompt_nonblocking(&session_id);
                                if prompt_outcome.is_none()
                                    && let Ok(Ok(res)) =
                                        timeout(Duration::from_millis(1500), &mut prompt_fut).await
                                {
                                    prompt_outcome = Some(res);
                                }
                                if prompt_outcome.is_none() {
                                    prompt_outcome = Some(Ok(PromptTurnResult {
                                        stop_reason: "end_turn".to_owned(),
                                        usage: AcpUsage::default(),
                                        error: None,
                                    }));
                                }
                            } else {
                                // Client tool or runaway transcript detected: complete turn with
                                // ZERO wait so Claude Code / Codex never hangs waiting on trailing
                                // hallucinated tokens; `lease.finish_ephemeral()` below terminates
                                // the runaway child session in the background.
                                prompt_outcome = Some(Ok(PromptTurnResult {
                                    stop_reason: if state.emitted_client_tool {
                                        "tool_use".to_owned()
                                    } else {
                                        "end_turn".to_owned()
                                    },
                                    usage: AcpUsage::default(),
                                    error: None,
                                }));
                            }
                            break;
                        }
                    } else if prompt_outcome.is_some() {
                        break;
                    }
                }
                res = &mut prompt_fut, if prompt_outcome.is_none() => {
                    let mapped = match res {
                        Ok(inner_res) => inner_res,
                        Err(_) => Err(AcpError::TurnTimeout { seconds: timeout_secs }),
                    };
                    prompt_outcome = Some(mapped);
                }
            }
        }

        if upstream_abort.is_none() && !state.halted_post_tool_runaway {
            while let Ok(upd) = acp_rx.try_recv() {
                if !forward_acp_update(
                    upd,
                    &req,
                    &task_guard,
                    &mut banner_filter,
                    &mut filter_out,
                    &mut parser,
                    &mut state,
                    &events_tx,
                    &cancel,
                )
                .await
                {
                    return;
                }
                if state.truncated_tool_abort.is_some() {
                    break;
                }
                if banner_filter.take_malformed_retry() || banner_filter.take_transient_retry() {
                    if let Some(open_name) = state.take_open_streamed_tool_name() {
                        state.truncated_tool_abort = Some(open_name);
                        break;
                    }
                    if state.emitted_client_tool || !state.pending_web_fetches.is_empty() {
                        tracing::info!(
                            "short-circuiting internal ACP post-tool retry banner because XML tool call was already captured"
                        );
                        state.halted_post_tool_runaway = true;
                    }
                }
                if let Some((sig, detail)) = banner_filter.take_signal() {
                    let no_output_yet = state.text_accumulator.is_empty()
                        && state.assistant_parts.is_empty()
                        && !state.emitted_client_tool
                        && state.seen_search_ids.is_empty()
                        && completed_web_fetches == 0;
                    if state.has_open_streamed_tool()
                        || no_output_yet
                        || (sig == UpstreamBannerSignal::QuotaExhausted
                            && !state.emitted_client_tool)
                    {
                        state.active_streaming_tool = None;
                        upstream_abort = Some((sig, detail));
                        break;
                    }
                    state.halted_post_tool_runaway = true;
                }
                if state.halted_post_tool_runaway
                    && let Some(open_name) = state.take_open_streamed_tool_name()
                {
                    state.halted_post_tool_runaway = false;
                    state.truncated_tool_abort = Some(open_name);
                    break;
                }
                if state.halted_post_tool_runaway {
                    state.tainted_session = true;
                    break;
                }
            }
        }

        if upstream_abort.is_none() && !state.halted_post_tool_runaway {
            filter_out.clear();
            banner_filter.finish(&mut filter_out);
            if let Some((sig, detail)) = banner_filter.take_signal() {
                let no_output_yet = state.text_accumulator.is_empty()
                    && state.assistant_parts.is_empty()
                    && !state.emitted_client_tool
                    && state.seen_search_ids.is_empty()
                    && completed_web_fetches == 0
                    && filter_out.trim().is_empty();
                if state.has_open_streamed_tool()
                    || no_output_yet
                    || (sig == UpstreamBannerSignal::QuotaExhausted
                        && !state.emitted_client_tool
                        && filter_out.trim().is_empty())
                {
                    state.active_streaming_tool = None;
                    upstream_abort = Some((sig, detail));
                }
            }
        }
        last_suppressed_banners = banner_filter.take_suppressed_banners();

        if let Some((sig, detail)) = upstream_abort {
            if let Some(old_lease) = lease.take() {
                old_lease.finish_ephemeral();
            }
            let err = signal_to_acp_error(sig, &active_model, detail);
            tracing::error!(error = ?err, "aborting ACP prompt immediately due to upstream quota/context/rate-limit/fatal banner");
            task_guard.fail(err.to_string());
            let _ = events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
            return;
        }

        if let Some(aborted_tool) = state.truncated_tool_abort.take() {
            if let Some(old_lease) = lease.take() {
                old_lease.finish_ephemeral();
            }
            let err = AcpError::UpstreamAgentTerminated {
                detail: format!(
                    "upstream stream truncated or corrupted in-progress `{aborted_tool}` tool call before closing tag"
                ),
            };
            tracing::error!(error = ?err, "aborting turn immediately due to corrupted in-progress XML tool call");
            task_guard.fail(err.to_string());
            let _ = events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
            return;
        }

        match prompt_outcome {
            Some(Ok(turn_res)) => {
                last_turn_error = turn_res
                    .error
                    .as_ref()
                    .map(|raw| raw.get().trim().to_owned())
                    .filter(|s| !s.is_empty() && s != "null" && s != "{}");

                if !state.halted_post_tool_runaway {
                    if !filter_out.is_empty() && state.truncated_tool_abort.is_none() {
                        state.parsed_buf.clear();
                        parser.feed(&filter_out, &mut state.parsed_buf);
                        if !emit_parsed_items(&req, &task_guard, &mut state, &events_tx, &cancel)
                            .await
                        {
                            return;
                        }
                    }

                    if state.truncated_tool_abort.is_none() {
                        state.parsed_buf.clear();
                        parser.finish(&mut state.parsed_buf);
                        if !emit_parsed_items(&req, &task_guard, &mut state, &events_tx, &cancel)
                            .await
                        {
                            return;
                        }
                    }
                }

                if let Some(aborted_tool) = state.truncated_tool_abort.take() {
                    if let Some(old_lease) = lease.take() {
                        old_lease.finish_ephemeral();
                    }
                    let err = AcpError::UpstreamAgentTerminated {
                        detail: format!(
                            "upstream stream truncated or corrupted in-progress `{aborted_tool}` tool call before closing tag"
                        ),
                    };
                    tracing::error!(error = ?err, "aborting turn due to truncated in-progress XML tool call");
                    task_guard.fail(err.to_string());
                    let _ = events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
                    return;
                }

                accumulate_usage(&mut cumulative_usage, turn_res.usage);
                context_usage = turn_res.usage;
                if !turn_res.stop_reason.is_empty() {
                    final_stop_reason = turn_res.stop_reason;
                }

                if !state.pending_web_fetches.is_empty() && !state.emitted_client_tool {
                    let queued_fetches: Vec<(String, String)> =
                        std::mem::take(&mut state.pending_web_fetches);
                    let mut continuation_xml = String::new();

                    for (fetch_call_id, raw_url) in queued_fetches {
                        if !send_cancel_safe(
                            &events_tx,
                            TurnEvent::WebFetchStart {
                                call_id: fetch_call_id.clone(),
                                url: raw_url.clone(),
                            },
                            &cancel,
                        )
                        .await
                        {
                            return;
                        }

                        let outcome = if completed_web_fetches >= max_fetch_uses {
                            WebFetchOutcome::Error {
                                url: raw_url.clone(),
                                error_code: "max_uses_exceeded",
                                message: format!(
                                    "exceeded web_fetch max_uses limit ({max_fetch_uses})"
                                ),
                            }
                        } else {
                            completed_web_fetches = completed_web_fetches.saturating_add(1);
                            task_guard.record_web_fetch();
                            let fetch_fut = backend.fetch_web_url(
                                &raw_url,
                                &req.web_fetch_allowed_domains,
                                &req.web_fetch_blocked_domains,
                            );
                            tokio::select! {
                                biased;
                                () = cancel.cancelled() => return,
                                timed = timeout_at(deadline, fetch_fut) => {
                                    match timed {
                                        Ok(out) => out,
                                        Err(_) => WebFetchOutcome::Error {
                                            url: raw_url.clone(),
                                            error_code: "url_not_accessible",
                                            message: "turn deadline exceeded while fetching URL".to_owned(),
                                        },
                                    }
                                }
                            }
                        };

                        format_web_fetch_continuation(&mut continuation_xml, &outcome);

                        if !send_cancel_safe(
                            &events_tx,
                            TurnEvent::WebFetchComplete {
                                call_id: fetch_call_id,
                                outcome,
                            },
                            &cancel,
                        )
                        .await
                        {
                            return;
                        }
                    }

                    continuation_xml.push_str(
                        "\nNow synthesize a complete, accurate response grounded in the fetched page content above.",
                    );
                    if continuation_xml.len() > MAX_ACP_PROMPT_TEXT_BYTES {
                        continuation_xml = truncate_head_tail_lines(
                            &continuation_xml,
                            MAX_ACP_PROMPT_TEXT_BYTES,
                            "web_fetch content",
                        );
                    }
                    state.post_tool_spill_buf.clear();
                    state.halted_post_tool_runaway = false;
                    current_prompt_blocks = vec![AcpPromptBlock::Text {
                        text: continuation_xml,
                    }];
                    continue;
                }

                break;
            }
            Some(Err(err)) => {
                if let Some(old_lease) = lease.take() {
                    old_lease.finish_ephemeral();
                }
                let err = upgrade_acp_error(err, &active_model);
                tracing::error!(error = ?err, "ACP prompt turn failed");
                task_guard.fail(err.to_string());
                let _ = events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
                return;
            }
            None => return,
        }
    }

    if !state.emitted_client_tool && !state.text_accumulator.is_empty() {
        let trailing = std::mem::take(&mut state.text_accumulator);
        if !trailing.trim().is_empty() {
            state.assistant_parts.push(CanonicalPart::Text(trailing));
        }
    }

    let has_non_ws_part = state.assistant_parts.iter().any(|part| match part {
        CanonicalPart::Text(t) => !t.trim().is_empty(),
        CanonicalPart::ToolCall { .. }
        | CanonicalPart::ImageBase64 { .. }
        | CanonicalPart::Reasoning(_)
        | CanonicalPart::ToolResult { .. } => true,
    });
    let produced_any_output = has_non_ws_part
        || state.emitted_client_tool
        || !state.seen_search_ids.is_empty()
        || completed_web_fetches > 0;

    if !produced_any_output {
        if let Some(empty_lease) = lease.take() {
            empty_lease.finish_ephemeral();
        }
        let recent_stderr = backend.recent_stderr_since(last_step_started_ms);
        let err = build_zero_output_error(
            &active_model,
            &final_stop_reason,
            last_turn_error.as_deref(),
            &last_suppressed_banners,
            &recent_stderr,
        );
        tracing::error!(
            error = ?err,
            "upstream ACP prompt completed with zero assistant output; failing turn instead of returning empty reply"
        );
        task_guard.fail(err.to_string());
        let _ = events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
        return;
    }

    context_usage = reconcile_turn_context_usage(
        context_usage,
        &state,
        initial_context_used,
        outbound_prompt_bytes,
    );

    if let Some(final_lease) = lease.take() {
        if retire_after_turn || req.auxiliary_task.is_some() || state.tainted_session {
            final_lease.finish_ephemeral();
        } else {
            let assistant_msg = CanonicalMessage {
                role: CanonicalRole::Assistant,
                parts: state.assistant_parts,
            };
            let post_hash = hash_conversation_with_reply(
                req.system_instructions.as_deref(),
                &req.client_tools,
                web_cfg,
                final_lease.model_id(),
                &full_messages,
                &assistant_msg,
            );
            let dialogue_hash =
                hash_dialogue_with_reply(final_lease.model_id(), &full_messages, &assistant_msg);
            full_messages.push(assistant_msg);
            let committed_context_used = if state.turn_peak_context_used > 0 {
                state.latest_context_used
            } else {
                context_usage.total_tokens
            };
            final_lease.commit(
                post_hash,
                dialogue_hash,
                req.client_tools,
                committed_context_used,
                Some(response_id),
                full_messages,
            );
        }
    }

    task_guard.complete(final_stop_reason.clone(), cumulative_usage);
    let _ = events_tx
        .send(TurnEvent::Completed {
            usage: context_usage,
            stop_reason: final_stop_reason,
        })
        .await;
}

/// Reconciles `PromptTurnResult.usage` (which `agy acp` sums across all internal `search_web` /
/// retry sub-calls inside a single `session/prompt` RPC) with true single-window context occupancy
/// so Codex and Claude Code neither miss compaction on warm deltas nor prematurely auto-compact
/// when `cached_read_tokens` is multiplied across 5–7 internal sub-steps.
fn reconcile_turn_context_usage(
    mut usage: AcpUsage,
    state: &TurnStreamState,
    initial_context_used: u64,
    outbound_prompt_bytes: usize,
) -> AcpUsage {
    if state.turn_peak_context_used > 0 {
        let authoritative_total = if state.tainted_session {
            state.peak_context_used
        } else {
            state.turn_peak_context_used
        };
        usage.output_tokens = usage.output_tokens.min(authoritative_total);
        usage.thought_tokens = usage.thought_tokens.min(usage.output_tokens);
        let target_input = authoritative_total.saturating_sub(usage.output_tokens);
        let reported_input = usage
            .input_tokens
            .saturating_add(usage.cached_read_tokens)
            .saturating_add(usage.cached_write_tokens);
        if reported_input < target_input {
            usage.input_tokens = usage
                .input_tokens
                .saturating_add(target_input - reported_input);
        } else if reported_input > target_input {
            let rep_u128 = u128::from(reported_input);
            let tgt_u128 = u128::from(target_input);
            let new_cached_read =
                u64::try_from((u128::from(usage.cached_read_tokens) * tgt_u128) / rep_u128)
                    .unwrap_or(0);
            let new_cached_write =
                u64::try_from((u128::from(usage.cached_write_tokens) * tgt_u128) / rep_u128)
                    .unwrap_or(0)
                    .min(target_input.saturating_sub(new_cached_read));
            usage.cached_read_tokens = new_cached_read;
            usage.cached_write_tokens = new_cached_write;
            usage.input_tokens = target_input
                .saturating_sub(new_cached_read)
                .saturating_sub(new_cached_write);
        }
        usage.total_tokens = target_input.saturating_add(usage.output_tokens);
        return usage;
    }

    let single_call_ceiling = initial_context_used
        .saturating_add(((outbound_prompt_bytes / 3) as u64).max(24_576))
        .saturating_add(24_576);
    let mut reported_input = usage
        .input_tokens
        .saturating_add(usage.cached_read_tokens)
        .saturating_add(usage.cached_write_tokens);
    if usage.cached_read_tokens > single_call_ceiling
        && reported_input > single_call_ceiling.saturating_mul(2)
        && let Some(denom) = std::num::NonZeroU64::new(single_call_ceiling)
    {
        let est_subcalls = (reported_input / denom).max(2);
        if let Some(subcalls) = std::num::NonZeroU64::new(est_subcalls) {
            usage.cached_read_tokens /= subcalls;
            usage.cached_write_tokens /= subcalls;
            usage.input_tokens /= subcalls;
            reported_input = usage
                .input_tokens
                .saturating_add(usage.cached_read_tokens)
                .saturating_add(usage.cached_write_tokens);
        }
    }

    let required_input = state.peak_context_used.saturating_sub(usage.output_tokens);
    usage.input_tokens = usage
        .input_tokens
        .saturating_add(required_input.saturating_sub(reported_input));
    usage.total_tokens = usage
        .input_tokens
        .saturating_add(usage.cached_read_tokens)
        .saturating_add(usage.cached_write_tokens)
        .saturating_add(usage.output_tokens);
    usage
}

fn accumulate_usage(acc: &mut AcpUsage, step: AcpUsage) {
    acc.input_tokens = acc.input_tokens.saturating_add(step.input_tokens);
    acc.output_tokens = acc.output_tokens.saturating_add(step.output_tokens);
    acc.cached_read_tokens = acc
        .cached_read_tokens
        .saturating_add(step.cached_read_tokens);
    acc.cached_write_tokens = acc
        .cached_write_tokens
        .saturating_add(step.cached_write_tokens);
    acc.thought_tokens = acc.thought_tokens.saturating_add(step.thought_tokens);
    acc.total_tokens = acc.total_tokens.saturating_add(step.total_tokens);
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal seed runner borrows turn context without extra wrapper allocation"
)]
async fn run_hydration_seed_step<B: AcpBackend>(
    backend: &Arc<B>,
    session_id: &liftgate_types::config::SessionId,
    chunk_blocks: &[AcpPromptBlock],
    seed_model: &ModelId,
    state: &mut TurnStreamState,
    deadline: Instant,
    timeout_secs: u64,
    channel_cap: usize,
    cancel: &CancellationToken,
) -> Result<AcpUsage, AcpError> {
    let (seed_tx, mut seed_rx) = mpsc::channel::<SessionUpdatePayload>(channel_cap);
    let mut banner_filter = AcpBannerFilter::new();
    let mut sink = String::new();
    let prompt_backend = Arc::clone(backend);
    let prompt_sid = session_id.clone();
    let step_blocks = chunk_blocks.to_vec();
    let mut prompt_fut = std::pin::pin!(async move {
        timeout_at(
            deadline,
            prompt_backend.start_prompt(&prompt_sid, &step_blocks, seed_tx),
        )
        .await
    });

    let mut outcome: Option<Result<PromptTurnResult, AcpError>> = None;
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(AcpError::Cancelled),
            maybe_upd = seed_rx.recv() => {
                match maybe_upd {
                    Some(SessionUpdatePayload::AgentMessageChunk { content }) => {
                        sink.clear();
                        banner_filter.feed(&content.text, &mut sink);
                        if let Some((sig, detail)) = banner_filter.take_signal() {
                            return Err(signal_to_acp_error(sig, seed_model, detail));
                        }
                    }
                    Some(SessionUpdatePayload::Error { message, error }) => {
                        let detail = format_update_error_detail(&message, error.as_deref());
                        if !detail.is_empty() {
                            let sig = classify_upstream_error_text(&detail)
                                .unwrap_or(UpstreamBannerSignal::FatalAgentError);
                            return Err(signal_to_acp_error(sig, seed_model, detail));
                        }
                    }
                    Some(SessionUpdatePayload::UsageUpdate { used, size }) => {
                        state.observe_usage_update(used, size);
                    }
                    Some(
                        SessionUpdatePayload::AgentThoughtChunk { .. }
                        | SessionUpdatePayload::ToolCall { .. }
                        | SessionUpdatePayload::ToolCallUpdate { .. }
                        | SessionUpdatePayload::Unknown,
                    ) => {}
                    None => {
                        if outcome.is_some() {
                            break;
                        }
                    }
                }
            }
            res = &mut prompt_fut, if outcome.is_none() => {
                outcome = Some(match res {
                    Ok(inner) => inner,
                    Err(_) => Err(AcpError::TurnTimeout { seconds: timeout_secs }),
                });
            }
        }
    }

    sink.clear();
    banner_filter.finish(&mut sink);
    if let Some((sig, detail)) = banner_filter.take_signal() {
        return Err(signal_to_acp_error(sig, seed_model, detail));
    }

    match outcome {
        Some(Ok(_)) if state.tainted_session => Err(AcpError::HydrationCompacted),
        Some(Ok(result)) => Ok(result.usage),
        Some(Err(err)) => Err(err),
        None => Err(AcpError::TransportClosed),
    }
}

fn format_web_fetch_continuation(buf: &mut String, outcome: &WebFetchOutcome) {
    if !buf.is_empty() {
        buf.push('\n');
    }
    match outcome {
        WebFetchOutcome::Success {
            url,
            title,
            media_type,
            content,
        } => {
            buf.push_str("<gateway_web_fetch_result url=\"");
            buf.push_str(url);
            buf.push_str("\" media_type=\"");
            buf.push_str(media_type);
            if let Some(t) = title {
                buf.push_str("\" title=\"");
                buf.push_str(&t.replace('"', "'"));
            }
            buf.push_str("\">\n");
            buf.push_str(content);
            buf.push_str("\n</gateway_web_fetch_result>");
        }
        WebFetchOutcome::Error {
            url,
            error_code,
            message,
        } => {
            buf.push_str("<gateway_web_fetch_result url=\"");
            buf.push_str(url);
            buf.push_str("\" is_error=\"true\" error_code=\"");
            buf.push_str(error_code);
            buf.push_str("\">\n");
            buf.push_str(message);
            buf.push_str("\n</gateway_web_fetch_result>");
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "helper borrows existing buffers from `run_turn_pipeline` to avoid reallocations"
)]
async fn forward_acp_update(
    upd: SessionUpdatePayload,
    req: &NormalizedTurnRequest,
    task_guard: &ActiveTaskGuard,
    banner_filter: &mut AcpBannerFilter,
    filter_out: &mut String,
    parser: &mut IncrementalToolParser,
    state: &mut TurnStreamState,
    events_tx: &mpsc::Sender<TurnEvent>,
    cancel: &CancellationToken,
) -> bool {
    if state.halted_post_tool_runaway {
        return true;
    }
    match upd {
        SessionUpdatePayload::AgentThoughtChunk { content } => {
            if !content.text.is_empty() {
                if let Some(open_name) = state.take_open_streamed_tool_name() {
                    tracing::warn!(
                        tool = %open_name,
                        "aborting active streamed XML tool call interrupted by ACP reasoning sub-step before closing tag"
                    );
                    state.truncated_tool_abort = Some(open_name);
                    return true;
                }
                if state.emitted_client_tool || !state.pending_web_fetches.is_empty() {
                    tracing::info!(
                        "short-circuiting post-tool internal ACP reasoning sub-step because XML tool call was already captured"
                    );
                    state.halted_post_tool_runaway = true;
                    return true;
                }
                task_guard.record_thought(content.text.chars().count());
                if req.include_thinking && !req.omit_thinking_text {
                    return send_cancel_safe(
                        events_tx,
                        TurnEvent::ThoughtDelta(content.text),
                        cancel,
                    )
                    .await;
                }
            }
            true
        }
        SessionUpdatePayload::ToolCall {
            tool_call_id,
            name,
            status,
            kind,
            title,
            args,
            raw_input,
        } => {
            if let Some(open_name) = state.take_open_streamed_tool_name() {
                tracing::warn!(
                    tool = %open_name,
                    acp_tool = %name,
                    acp_kind = %kind,
                    "aborting active streamed XML tool call interrupted by native ACP tool call before closing tag"
                );
                state.truncated_tool_abort = Some(open_name);
                return true;
            }
            if state.emitted_client_tool || !state.pending_web_fetches.is_empty() {
                tracing::info!(
                    acp_tool = %name,
                    acp_kind = %kind,
                    "short-circuiting post-tool native ACP tool call because XML tool call was already captured"
                );
                state.halted_post_tool_runaway = true;
                return true;
            }
            if is_acp_web_search(&kind, &name, &title) {
                if state.seen_search_ids.insert(tool_call_id.clone()) {
                    task_guard.record_web_search();
                }
                let query = extract_acp_search_query(raw_input.as_deref(), args.as_deref(), &title);
                if status != "completed" && !tool_call_id.is_empty() {
                    state
                        .active_searches
                        .insert(tool_call_id.clone(), query.clone());
                }
                return send_cancel_safe(
                    events_tx,
                    TurnEvent::WebSearch {
                        call_id: tool_call_id,
                        status,
                        query,
                    },
                    cancel,
                )
                .await;
            }
            true
        }
        SessionUpdatePayload::ToolCallUpdate {
            tool_call_id,
            name,
            status,
            kind,
            title,
            args,
            raw_input,
        } => {
            let new_query = extract_acp_search_query(raw_input.as_deref(), args.as_deref(), &title);
            let is_known_search = state.active_searches.contains_key(&tool_call_id)
                || state.seen_search_ids.contains(&tool_call_id)
                || is_acp_web_search(&kind, &name, &title);
            if is_known_search {
                if !tool_call_id.is_empty() && state.seen_search_ids.insert(tool_call_id.clone()) {
                    task_guard.record_web_search();
                }
                let merged_query = if status == "completed" {
                    let prev = state.active_searches.remove(&tool_call_id).flatten();
                    new_query.or(prev)
                } else {
                    let entry = state
                        .active_searches
                        .entry(tool_call_id.clone())
                        .or_default();
                    if new_query.is_some() {
                        entry.clone_from(&new_query);
                    }
                    entry.clone()
                };
                return send_cancel_safe(
                    events_tx,
                    TurnEvent::WebSearch {
                        call_id: tool_call_id,
                        status,
                        query: merged_query,
                    },
                    cancel,
                )
                .await;
            }
            true
        }
        SessionUpdatePayload::AgentMessageChunk { content } => {
            filter_out.clear();
            banner_filter.feed(&content.text, filter_out);
            if filter_out.is_empty() {
                return true;
            }
            state.parsed_buf.clear();
            parser.feed(filter_out, &mut state.parsed_buf);
            emit_parsed_items(req, task_guard, state, events_tx, cancel).await
        }
        SessionUpdatePayload::Error { message, error } => {
            let detail = format_update_error_detail(&message, error.as_deref());
            if !detail.is_empty() {
                let sig = classify_upstream_error_text(&detail)
                    .unwrap_or(UpstreamBannerSignal::FatalAgentError);
                banner_filter.record_external_signal(sig, detail);
            }
            true
        }
        SessionUpdatePayload::UsageUpdate { used, size } => {
            state.observe_usage_update(used, size);
            true
        }
        SessionUpdatePayload::Unknown => true,
    }
}

fn format_update_error_detail(message: &str, error: Option<&RawValue>) -> String {
    let trimmed_msg = message.trim();
    let raw_err = error
        .map(|r| r.get().trim())
        .filter(|s| !s.is_empty() && *s != "null" && *s != "{}");
    match (trimmed_msg.is_empty(), raw_err) {
        (false, Some(raw)) if !trimmed_msg.contains(raw) => format!("{trimmed_msg} — {raw}"),
        (false, _) => trimmed_msg.to_owned(),
        (true, Some(raw)) => raw.to_owned(),
        (true, None) => String::new(),
    }
}

fn upgrade_gateway_error(err: GatewayError, model: &ModelId) -> GatewayError {
    match err {
        GatewayError::Acp { source } => GatewayError::Acp {
            source: upgrade_acp_error(source, model),
        },
        GatewayError::SharedAcp { source } => {
            if let AcpError::Rpc { message, .. } = source.as_ref()
                && let Some(sig) = classify_upstream_error_text(message)
            {
                let upgraded = signal_to_acp_error(sig, model, message.clone());
                return GatewayError::SharedAcp {
                    source: Arc::new(upgraded),
                };
            }
            GatewayError::SharedAcp { source }
        }
        GatewayError::Unauthorized
        | GatewayError::NotFound { .. }
        | GatewayError::BadJson { .. }
        | GatewayError::Protocol { .. } => err,
    }
}

fn upgrade_acp_error(err: AcpError, model: &ModelId) -> AcpError {
    match err {
        AcpError::Rpc {
            method,
            code,
            message,
        } => {
            if let Some(sig) = classify_upstream_error_text(&message) {
                signal_to_acp_error(sig, model, message)
            } else {
                AcpError::Rpc {
                    method,
                    code,
                    message,
                }
            }
        }
        AcpError::SpawnProcess { .. }
        | AcpError::ProcessGroup { .. }
        | AcpError::MissingStdioPipe { .. }
        | AcpError::TransportClosed
        | AcpError::LineCodec { .. }
        | AcpError::EncodeJson { .. }
        | AcpError::DecodeJson { .. }
        | AcpError::TurnTimeout { .. }
        | AcpError::UpstreamRateLimited { .. }
        | AcpError::UpstreamQuotaExhausted { .. }
        | AcpError::UpstreamAgentTerminated { .. }
        | AcpError::EmptyUpstreamResponse { .. }
        | AcpError::Cancelled
        | AcpError::HydrationCompacted
        | AcpError::ContextLengthExceeded { .. } => err,
    }
}

fn signal_to_acp_error(sig: UpstreamBannerSignal, model: &ModelId, detail: String) -> AcpError {
    match sig {
        UpstreamBannerSignal::QuotaExhausted => AcpError::UpstreamQuotaExhausted {
            model: model.as_str().to_owned(),
            detail,
        },
        UpstreamBannerSignal::ContextLengthExceeded => AcpError::ContextLengthExceeded {
            model: model.as_str().to_owned(),
            detail,
        },
        UpstreamBannerSignal::RateLimited => AcpError::UpstreamRateLimited {
            model: model.as_str().to_owned(),
            detail,
        },
        UpstreamBannerSignal::FatalAgentError => AcpError::UpstreamAgentTerminated { detail },
    }
}

fn build_zero_output_error(
    model: &ModelId,
    stop_reason: &str,
    turn_error: Option<&str>,
    suppressed_banners: &[String],
    recent_stderr: &[String],
) -> AcpError {
    let mut first_hint: Option<String> = None;
    let candidates = turn_error
        .into_iter()
        .chain(suppressed_banners.iter().map(String::as_str))
        .chain(recent_stderr.iter().rev().map(String::as_str));

    for raw in candidates {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if first_hint.is_none() {
            first_hint = Some(trimmed.to_owned());
        }
        if let Some(sig) = classify_upstream_error_text(trimmed) {
            return signal_to_acp_error(sig, model, trimmed.to_owned());
        }
    }

    AcpError::EmptyUpstreamResponse {
        model: model.as_str().to_owned(),
        stop_reason: if stop_reason.trim().is_empty() {
            "end_turn".to_owned()
        } else {
            stop_reason.to_owned()
        },
        detail: first_hint,
    }
}

async fn emit_parsed_items(
    req: &NormalizedTurnRequest,
    task_guard: &ActiveTaskGuard,
    state: &mut TurnStreamState,
    events_tx: &mpsc::Sender<TurnEvent>,
    cancel: &CancellationToken,
) -> bool {
    let mut items = std::mem::take(&mut state.parsed_buf);
    let ok = emit_parsed_items_inner(&mut items, req, task_guard, state, events_tx, cancel).await;
    state.parsed_buf = items;
    ok
}

async fn emit_parsed_items_inner(
    items: &mut Vec<ParsedStreamItem>,
    req: &NormalizedTurnRequest,
    task_guard: &ActiveTaskGuard,
    state: &mut TurnStreamState,
    events_tx: &mpsc::Sender<TurnEvent>,
    cancel: &CancellationToken,
) -> bool {
    for item in items.drain(..) {
        if state.halted_post_tool_runaway {
            break;
        }
        match item {
            ParsedStreamItem::TextDelta(delta) => {
                // 1. After a client tool call or server-side web_fetch in the current step,
                //    suppress all trailing prose AND immediately halt the turn if the model
                //    starts simulating transcript/tool-result boundaries or writing prose
                //    beyond a short parallel-tool bridge.
                if state.emitted_client_tool || !state.pending_web_fetches.is_empty() {
                    state.post_tool_spill_buf.push_str(&delta);
                    let non_ws_chars = state
                        .post_tool_spill_buf
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .count();
                    if contains_runaway_marker(&state.post_tool_spill_buf)
                        || is_malformed_function_call_banner(&state.post_tool_spill_buf)
                        || classify_upstream_error_text(&state.post_tool_spill_buf).is_some()
                        || non_ws_chars > MAX_POST_TOOL_BRIDGE_NON_WS_CHARS
                    {
                        tracing::warn!(
                            non_ws_chars,
                            "detected post-tool hallucination / simulated transcript boundary; halting turn immediately"
                        );
                        state.halted_post_tool_runaway = true;
                        break;
                    }
                    continue;
                }

                // 2. Even in a non-tool text turn, guard against mid-response simulated role
                //    switches (`</assistant>\n<user>` or `<client_tool_result`).
                if let Some(cut_idx) = find_inline_runaway_cut(&state.text_accumulator, &delta) {
                    if let Some((clean_prefix, _)) = delta.split_at_checked(cut_idx)
                        && !clean_prefix.is_empty()
                    {
                        task_guard.record_text(clean_prefix);
                        state.text_accumulator.push_str(clean_prefix);
                        if !send_cancel_safe(
                            events_tx,
                            TurnEvent::TextDelta(clean_prefix.to_owned()),
                            cancel,
                        )
                        .await
                        {
                            return false;
                        }
                    }
                    tracing::warn!(
                        "detected simulated role/tool-result boundary in assistant text; halting turn immediately"
                    );
                    state.halted_post_tool_runaway = true;
                    break;
                }

                task_guard.record_text(&delta);
                state.text_accumulator.push_str(&delta);
                if !send_cancel_safe(events_tx, TurnEvent::TextDelta(delta), cancel).await {
                    return false;
                }
            }
            ParsedStreamItem::ToolCallStart { name, is_raw_input } => {
                state.post_tool_spill_buf.clear();

                if name == INTERNAL_GATEWAY_WEB_FETCH_TOOL && req.enable_native_web_fetch {
                    state.active_streaming_tool = Some(StreamingClientToolState {
                        name,
                        call_id: String::new(),
                        is_custom: false,
                        emitted_start: false,
                    });
                    continue;
                }

                let already_seen_name = state
                    .seen_client_tools
                    .iter()
                    .any(|(seen_name, _)| seen_name == &name);
                if already_seen_name {
                    state.active_streaming_tool = Some(StreamingClientToolState {
                        name,
                        call_id: String::new(),
                        is_custom: false,
                        emitted_start: false,
                    });
                    continue;
                }

                state.flush_text_part();
                let matched_spec = req.client_tools.iter().find(|t| t.name == name);
                let is_custom = match matched_spec {
                    Some(spec) => matches!(spec.kind, CanonicalToolKind::Custom { .. }),
                    None => is_raw_input,
                };
                task_guard.record_client_tool(&name);
                let call_id = format!("call_{}", generate_short_nonce());
                state.active_streaming_tool = Some(StreamingClientToolState {
                    name: name.clone(),
                    call_id: call_id.clone(),
                    is_custom,
                    emitted_start: true,
                });
                if !send_cancel_safe(
                    events_tx,
                    TurnEvent::ClientToolCallStart {
                        call_id,
                        name,
                        is_custom,
                    },
                    cancel,
                )
                .await
                {
                    return false;
                }
            }
            ParsedStreamItem::ToolCallDelta(delta) => {
                if let Some(active) = state.active_streaming_tool.as_ref()
                    && active.emitted_start
                    && active.is_custom
                    && !delta.is_empty()
                {
                    task_guard.record_tool_delta(delta.chars().count());
                    if !send_cancel_safe(
                        events_tx,
                        TurnEvent::ClientToolCallDelta {
                            call_id: active.call_id.clone(),
                            delta,
                        },
                        cancel,
                    )
                    .await
                    {
                        return false;
                    }
                }
            }
            ParsedStreamItem::ToolCall(ParsedToolCall {
                name,
                arguments,
                is_raw_input,
            }) => {
                let active_stream = state.active_streaming_tool.take();
                // Valid parallel tool call arrived before any runaway marker -> clear bridge spill
                state.post_tool_spill_buf.clear();

                if name == INTERNAL_GATEWAY_WEB_FETCH_TOOL && req.enable_native_web_fetch {
                    if let Some(url) = extract_web_fetch_url(&arguments) {
                        state.flush_text_part();
                        let call_id = format!("srvtoolu_{}", generate_short_nonce());
                        state.pending_web_fetches.push((call_id, url));
                    }
                    continue;
                }

                let canonical_args = canonicalize_tool_args(&arguments);
                if !state
                    .seen_client_tools
                    .insert((name.clone(), canonical_args))
                {
                    tracing::warn!(
                        tool = %name,
                        "suppressed duplicate client tool call emitted during internal ACP retry"
                    );
                    continue;
                }

                state.flush_text_part();
                state.emitted_client_tool = true;

                let matched_spec = req.client_tools.iter().find(|t| t.name == name);
                let (call_id, is_custom) = if let Some(active) =
                    active_stream.filter(|a| a.emitted_start && a.name == name)
                {
                    (active.call_id, active.is_custom)
                } else {
                    task_guard.record_client_tool(&name);
                    let is_custom = match matched_spec {
                        Some(spec) => matches!(spec.kind, CanonicalToolKind::Custom { .. }),
                        None => is_raw_input,
                    };
                    (format!("call_{}", generate_short_nonce()), is_custom)
                };
                let arguments = normalize_emitted_tool_arguments(
                    arguments,
                    is_custom,
                    is_raw_input,
                    matched_spec,
                );
                state.assistant_parts.push(CanonicalPart::ToolCall {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                    is_custom,
                });
                if !send_cancel_safe(
                    events_tx,
                    TurnEvent::ClientToolCall {
                        call_id,
                        name,
                        arguments,
                        is_custom,
                    },
                    cancel,
                )
                .await
                {
                    return false;
                }
            }
            ParsedStreamItem::ToolCallAborted { name } => {
                state.active_streaming_tool = None;
                state.truncated_tool_abort = Some(name);
                break;
            }
        }
    }
    true
}

fn normalize_emitted_tool_arguments(
    arguments: String,
    is_custom: bool,
    is_raw_input: bool,
    matched_spec: Option<&liftgate_types::canonical::CanonicalToolSpec>,
) -> String {
    if is_custom {
        if !is_raw_input
            && let Ok(serde_json::Value::Object(map)) = serde_json::from_str(&arguments)
        {
            for key in ["input", "patch", "diff"] {
                if let Some(serde_json::Value::String(inner)) = map.get(key) {
                    return inner.clone();
                }
            }
        }
        return arguments;
    }

    if !is_raw_input
        && matches!(
            serde_json::from_str::<serde_json::Value>(&arguments),
            Ok(serde_json::Value::Object(_))
        )
    {
        return arguments;
    }

    let fallback_key = matched_spec
        .and_then(|spec| match &spec.kind {
            CanonicalToolKind::Function { parameters_json } => {
                single_required_string_param(parameters_json)
            }
            CanonicalToolKind::Custom { .. } => None,
        })
        .unwrap_or_else(|| "input".to_owned());

    let mut map = serde_json::Map::with_capacity(1);
    map.insert(fallback_key, serde_json::Value::String(arguments));
    serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or_else(|_| "{}".to_owned())
}

fn single_required_string_param(parameters_json: &str) -> Option<String> {
    let schema: serde_json::Value = serde_json::from_str(parameters_json).ok()?;
    let req_arr = schema.get("required")?.as_array()?;
    if req_arr.len() != 1 {
        return None;
    }
    let key = req_arr.first()?.as_str()?;
    let prop_type = schema
        .get("properties")
        .and_then(|p| p.get(key))
        .and_then(|prop| prop.get("type"))
        .and_then(serde_json::Value::as_str);
    if matches!(prop_type, Some("string") | None) {
        Some(key.to_owned())
    } else {
        None
    }
}

fn contains_runaway_marker(buf: &str) -> bool {
    TRANSCRIPT_RUNAWAY_MARKERS.iter().any(|m| buf.contains(m))
}

/// Checks if appending `delta` to `prev` introduces a simulated multi-turn role boundary
/// (`\n</assistant>`, `\n<client_tool_result`, `\n[Client Tool Result id=`), returning the byte
/// index inside `delta` where the cut should occur (`0` if the boundary started across `prev`+`delta`).
fn find_inline_runaway_cut(prev: &str, delta: &str) -> Option<usize> {
    const INLINE_MARKERS: [&str; 3] = [
        "\n</assistant>",
        "\n<client_tool_result",
        "\n[Client Tool Result id=",
    ];
    for marker in INLINE_MARKERS {
        if let Some(pos) = delta.find(marker) {
            return Some(pos);
        }
    }
    let tail_start = prev.len().saturating_sub(32);
    let prev_tail = prev
        .char_indices()
        .find(|(idx, _)| *idx >= tail_start)
        .and_then(|(idx, _)| prev.get(idx..))
        .unwrap_or(prev);
    let mut seam = String::with_capacity(prev_tail.len() + delta.len().min(64));
    seam.push_str(prev_tail);
    let delta_head: String = delta.chars().take(48).collect();
    seam.push_str(&delta_head);
    if INLINE_MARKERS.iter().any(|m| seam.contains(m)) {
        return Some(0);
    }
    None
}

fn canonicalize_tool_args(raw: &str) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw)
        && let Ok(compact) = serde_json::to_string(&val)
    {
        return compact;
    }
    raw.trim().to_owned()
}

#[derive(Deserialize)]
struct WebFetchArgsExtract {
    #[serde(default)]
    url: Option<String>,
}

fn extract_web_fetch_url(arguments: &str) -> Option<String> {
    if let Ok(parsed) = serde_json::from_str::<WebFetchArgsExtract>(arguments)
        && let Some(u) = parsed.url.map(|s| s.trim().to_owned())
        && !u.is_empty()
    {
        return Some(u);
    }
    let trimmed = arguments.trim();
    if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        return Some(trimmed.to_owned());
    }
    None
}

fn is_acp_web_search(kind: &str, name: &str, title: &str) -> bool {
    kind == "search" || name == "search_web" || title.ends_with(" on Web")
}

#[derive(Deserialize)]
struct SearchQueryExtract {
    #[serde(default)]
    query: Option<String>,
}

fn extract_acp_search_query(
    raw_input: Option<&RawValue>,
    args: Option<&RawValue>,
    title: &str,
) -> Option<String> {
    for candidate in [raw_input, args].into_iter().flatten() {
        if let Ok(parsed) = serde_json::from_str::<SearchQueryExtract>(candidate.get())
            && let Some(q) = parsed.query.map(|s| s.trim().to_owned())
            && !q.is_empty()
        {
            return Some(q);
        }
    }
    if let Some(prefix) = title.strip_suffix(" on Web") {
        let cleaned = prefix.trim().trim_matches('"').trim();
        if !cleaned.is_empty() {
            return Some(cleaned.to_owned());
        }
    }
    None
}

async fn send_cancel_safe(
    tx: &mpsc::Sender<TurnEvent>,
    ev: TurnEvent,
    cancel: &CancellationToken,
) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        res = tx.send(ev) => res.is_ok(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpstreamBannerSignal {
    QuotaExhausted,
    ContextLengthExceeded,
    RateLimited,
    FatalAgentError,
}

const MAX_SUPPRESSED_BANNERS: usize = 8;

/// Classifies an upstream `agy acp` banner, JSON-RPC error message, or stderr diagnostic line.
#[must_use]
pub fn classify_upstream_error_text(text: &str) -> Option<UpstreamBannerSignal> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();

    // 1. Account / subscription / model-tier quota exhaustion (must stop immediately without
    //    silent failover or retry loops).
    if lower.contains("individual quota")
        || lower.contains("quota reached")
        || lower.contains("quota_exhausted")
        || lower.contains("insufficient_quota")
        || lower.contains("quota exhausted")
        || lower.contains("quota exceeded")
        || lower.contains("quota limit")
        || lower.contains("quota will reset")
        || lower.contains("exhausted your quota")
        || lower.contains("exhausted your capacity")
        || lower.contains("upgrade your subscription")
        || lower.contains("increase your limits")
        || lower.contains("usage limit")
        || lower.contains("daily limit")
        || lower.contains("monthly limit")
        || lower.contains("credit balance")
        || lower.contains("billing hard limit")
        || (lower.contains("quota") && lower.contains("reset"))
        || trimmed.contains("额度")
        || trimmed.contains("配额")
    {
        return Some(UpstreamBannerSignal::QuotaExhausted);
    }

    // 2. Upstream model context-window overflow (must surface as a standard auto-compactable
    //    `context_length_exceeded` / `prompt is too long` error so Codex & Claude Code compact).
    if lower.contains("context_length_exceeded")
        || lower.contains("prompt is too long")
        || lower.contains("maximum context length")
        || lower.contains("exceeds the context window")
        || lower.contains("context window exceeded")
        || lower.contains("too many input tokens")
        || lower.contains("token count exceeds")
        || (lower.contains("input token") && lower.contains("exceed"))
    {
        return Some(UpstreamBannerSignal::ContextLengthExceeded);
    }

    // 2. Server-side capacity / rate-limit congestion (eligible for immediate model failover).
    //    Note: transient internal `agy acp` retry notices like `transient 503 — retrying in 1s...`
    //    are intentionally NOT matched here so recoverable 1s retries can complete normally.
    if trimmed.contains("RESOURCE_EXHAUSTED")
        || trimmed.contains("(code 429)")
        || trimmed.contains("MODEL_CAPACITY_EXHAUSTED")
        || lower.contains("no capacity available")
        || trimmed.contains("(code 503)")
        || trimmed.contains("Service Unavailable")
    {
        return Some(UpstreamBannerSignal::RateLimited);
    }

    // 3. Fatal agent / authentication errors.
    if trimmed.contains("Agent execution terminated due to error")
        || trimmed.contains("UNAUTHENTICATED")
        || trimmed.contains("PERMISSION_DENIED")
        || lower.contains("authentication failed")
        || lower.contains("not logged in")
        || lower.contains("please sign in")
    {
        return Some(UpstreamBannerSignal::FatalAgentError);
    }

    None
}

fn is_malformed_function_call_banner(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains("Your previous response contained an improperly formatted function call")
        || trimmed.contains("Malformed function call:")
        || trimmed.starts_with("Please retry with a properly formatted function call")
        || trimmed.starts_with("Retries remaining:")
}

fn is_secondary_retry_continuation_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("Please retry with a properly formatted function call")
        || trimmed.starts_with("Retries remaining:")
}

/// Stateful streaming filter that strips:
/// 1) transient `agy acp` retry / `MALFORMED_FUNCTION_CALL` banners across chunk splits,
/// 2) stray echoed transcript wrapper lines (`</client_tool_result>`, `</user>`, `<assistant>`)
///    at the start of an assistant reply before real content begins, AND
/// 3) surfaces upstream quota exhaustion, `429 RESOURCE_EXHAUSTED`, and fatal agent termination
///    banners immediately (0ms wait) so `run_turn_pipeline` can halt or fail over cleanly.
#[derive(Debug, Default)]
pub struct AcpBannerFilter {
    at_line_start: bool,
    suppressing_line: bool,
    has_emitted_non_ws: bool,
    saw_malformed_function_call_retry: bool,
    saw_transient_retry_banner: bool,
    in_tool_payload: bool,
    at_post_tool_boundary: bool,
    tool_open_tag: Option<String>,
    tool_close_tag: Option<String>,
    tag_ring_buf: String,
    hold_buf: String,
    midline_probe_buf: String,
    dropped_line_buf: String,
    signal: Option<(UpstreamBannerSignal, String)>,
    suppressed_banners: Vec<String>,
}

const ACP_BANNER_LINE_PREFIXES: [&str; 4] = [
    "⚠",
    "Your previous response contained an improperly formatted function call",
    "Please retry with a properly formatted function call",
    "Retries remaining:",
];

const POST_TOOL_MIDLINE_BANNER_PREFIXES: [&str; 5] = [
    "⚠ Your previous response",
    "⚠ API error (",
    "⚠ Individual quota",
    "⚠ Agent execution terminated",
    "⚠ Quota ",
];

const LEADING_STRAY_WRAPPER_LINES: [&str; 9] = [
    "</client_tool_result>",
    "[End Client Tool Result]",
    "</gateway_web_fetch_result>",
    "</user>",
    "<assistant>",
    "</conversation_history>",
    "--- End Prior Conversation Context ---",
    "### Current Caller Input (Respond ONLY for the current assistant step)",
    "### Prior Assistant Turn",
];

impl AcpBannerFilter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            at_line_start: true,
            suppressing_line: false,
            has_emitted_non_ws: false,
            saw_malformed_function_call_retry: false,
            saw_transient_retry_banner: false,
            in_tool_payload: false,
            at_post_tool_boundary: false,
            tool_open_tag: None,
            tool_close_tag: None,
            tag_ring_buf: String::new(),
            hold_buf: String::new(),
            midline_probe_buf: String::new(),
            dropped_line_buf: String::new(),
            signal: None,
            suppressed_banners: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_tool_nonce(nonce: &str) -> Self {
        let mut filter = Self::new();
        let trimmed = nonce.trim();
        if !trimmed.is_empty() {
            filter.tool_open_tag = Some(format!("<agy_tool_call_{trimmed}>"));
            filter.tool_close_tag = Some(format!("</agy_tool_call_{trimmed}>"));
        }
        filter
    }

    pub fn take_signal(&mut self) -> Option<(UpstreamBannerSignal, String)> {
        self.signal.take()
    }

    pub fn take_malformed_retry(&mut self) -> bool {
        std::mem::replace(&mut self.saw_malformed_function_call_retry, false)
    }

    pub fn take_transient_retry(&mut self) -> bool {
        std::mem::replace(&mut self.saw_transient_retry_banner, false)
    }

    pub fn take_suppressed_banners(&mut self) -> Vec<String> {
        std::mem::take(&mut self.suppressed_banners)
    }

    pub fn record_external_signal(&mut self, sig: UpstreamBannerSignal, detail: String) {
        let should_replace = match self.signal.as_ref() {
            None => true,
            Some((existing, _)) => {
                *existing != UpstreamBannerSignal::QuotaExhausted
                    && sig == UpstreamBannerSignal::QuotaExhausted
            }
        };
        if should_replace {
            self.signal = Some((sig, detail));
        }
    }

    fn inspect_banner_line(banner: &str) -> Option<(UpstreamBannerSignal, String)> {
        let trimmed = banner.trim();
        classify_upstream_error_text(trimmed).map(|sig| (sig, trimmed.to_owned()))
    }

    fn record_dropped_banner(&mut self, raw_banner: &str) {
        let dropped = raw_banner.trim();
        if dropped.is_empty() {
            return;
        }
        if !is_secondary_retry_continuation_line(dropped)
            && self.suppressed_banners.len() < MAX_SUPPRESSED_BANNERS
        {
            self.suppressed_banners.push(dropped.to_owned());
        }

        if let Some((sig, detail)) = Self::inspect_banner_line(dropped) {
            self.record_external_signal(sig, detail);
            match sig {
                UpstreamBannerSignal::QuotaExhausted => {
                    tracing::warn!(
                        banner = %dropped,
                        "detected upstream Antigravity quota exhaustion banner"
                    );
                }
                UpstreamBannerSignal::ContextLengthExceeded => {
                    tracing::warn!(
                        banner = %dropped,
                        "detected upstream ACP context-length exceeded banner"
                    );
                }
                UpstreamBannerSignal::RateLimited | UpstreamBannerSignal::FatalAgentError => {
                    tracing::warn!(
                        banner = %dropped,
                        "detected upstream ACP rate-limit or fatal error banner"
                    );
                }
            }
            return;
        }

        if is_malformed_function_call_banner(dropped) {
            self.saw_malformed_function_call_retry = true;
            if is_secondary_retry_continuation_line(dropped) {
                tracing::debug!(
                    banner = %dropped,
                    "filtered continuation line of internal `agy acp` malformed-call retry banner"
                );
            } else {
                tracing::warn!(
                    banner = %dropped,
                    "filtered internal `agy acp` malformed-function-call retry banner"
                );
            }
            return;
        }

        self.saw_transient_retry_banner = true;
        tracing::warn!(
            banner = %dropped,
            "filtered internal `agy acp` retry/diagnostic banner from assistant stream"
        );
    }

    fn emit_char(&mut self, ch: char, out: &mut String) {
        if !ch.is_whitespace() {
            self.has_emitted_non_ws = true;
        }
        out.push(ch);
        self.at_line_start = ch == '\n';
        if ch == '\n' {
            self.at_post_tool_boundary = false;
        }

        let max_tag_len = self
            .tool_close_tag
            .as_ref()
            .map_or(0, String::len)
            .max(self.tool_open_tag.as_ref().map_or(0, String::len));
        if max_tag_len == 0 {
            return;
        }

        self.tag_ring_buf.push(ch);
        if self.tag_ring_buf.len() > max_tag_len {
            let min_drop = self.tag_ring_buf.len() - max_tag_len;
            let mut drop_idx = min_drop;
            while drop_idx < self.tag_ring_buf.len()
                && !self.tag_ring_buf.is_char_boundary(drop_idx)
            {
                drop_idx += 1;
            }
            self.tag_ring_buf.drain(..drop_idx);
        }

        if self
            .tool_close_tag
            .as_deref()
            .is_some_and(|close_tag| self.tag_ring_buf.ends_with(close_tag))
        {
            self.in_tool_payload = false;
            self.at_post_tool_boundary = true;
            self.tag_ring_buf.clear();
        } else if !self.in_tool_payload
            && self
                .tool_open_tag
                .as_deref()
                .is_some_and(|open_tag| self.tag_ring_buf.ends_with(open_tag))
        {
            self.in_tool_payload = true;
            self.at_post_tool_boundary = false;
            self.tag_ring_buf.clear();
        }
    }

    fn emit_str(&mut self, text: &str, out: &mut String) {
        for ch in text.chars() {
            self.emit_char(ch, out);
        }
    }

    pub fn feed(&mut self, chunk: &str, out: &mut String) {
        for ch in chunk.chars() {
            if self.in_tool_payload {
                self.emit_char(ch, out);
                continue;
            }

            if self.suppressing_line {
                if ch == '\n' {
                    let line = std::mem::take(&mut self.dropped_line_buf);
                    self.record_dropped_banner(&line);
                    self.suppressing_line = false;
                    self.at_line_start = true;
                    self.at_post_tool_boundary = false;
                } else if self.dropped_line_buf.len() < 512 {
                    self.dropped_line_buf.push(ch);
                }
                continue;
            }

            if self.at_line_start || !self.hold_buf.is_empty() {
                if ch == '\n' && self.hold_buf.is_empty() {
                    if self.has_emitted_non_ws {
                        self.emit_char('\n', out);
                    } else {
                        self.at_line_start = true;
                        self.at_post_tool_boundary = false;
                    }
                    continue;
                }
                self.at_line_start = false;
                self.hold_buf.push(ch);
                let trimmed_start = self.hold_buf.trim_start_matches([' ', '\r']);
                if trimmed_start.is_empty() {
                    if ch == '\n' {
                        let flushed = std::mem::take(&mut self.hold_buf);
                        if self.has_emitted_non_ws {
                            self.emit_str(&flushed, out);
                        } else {
                            self.at_line_start = true;
                            self.at_post_tool_boundary = false;
                        }
                    }
                    continue;
                }

                // Strip leading stray echoed wrapper tags (e.g. `</client_tool_result>\n\n</user>\n<assistant>\n`)
                if !self.has_emitted_non_ws {
                    let trimmed_line = trimmed_start.trim_end_matches(['\r', '\n']);
                    if ch == '\n' && LEADING_STRAY_WRAPPER_LINES.contains(&trimmed_line) {
                        tracing::warn!(
                            wrapper = %trimmed_line,
                            "stripped stray echoed wrapper tag at start of assistant reply"
                        );
                        self.hold_buf.clear();
                        self.at_line_start = true;
                        continue;
                    }
                    let could_match_wrapper = ch != '\n'
                        && LEADING_STRAY_WRAPPER_LINES
                            .iter()
                            .any(|w| w.starts_with(trimmed_start));
                    if could_match_wrapper {
                        continue;
                    }
                }

                if ACP_BANNER_LINE_PREFIXES
                    .iter()
                    .any(|pat| trimmed_start.starts_with(pat))
                {
                    if ch == '\n' {
                        let line = std::mem::take(&mut self.hold_buf);
                        self.record_dropped_banner(&line);
                        self.at_line_start = true;
                    } else {
                        self.dropped_line_buf.clear();
                        self.dropped_line_buf.push_str(trimmed_start);
                        self.hold_buf.clear();
                        self.suppressing_line = true;
                    }
                    continue;
                }

                let still_could_match = ch != '\n'
                    && ACP_BANNER_LINE_PREFIXES
                        .iter()
                        .any(|pat| pat.starts_with(trimmed_start));
                if !still_could_match {
                    let flushed = std::mem::take(&mut self.hold_buf);
                    self.emit_str(&flushed, out);
                }
                continue;
            }

            if self.midline_probe_buf.is_empty()
                && self.at_post_tool_boundary
                && (ch == ' ' || ch == '\t')
            {
                self.emit_char(ch, out);
                continue;
            }

            if !self.midline_probe_buf.is_empty() || (self.at_post_tool_boundary && ch == '⚠') {
                self.midline_probe_buf.push(ch);
                if POST_TOOL_MIDLINE_BANNER_PREFIXES
                    .iter()
                    .any(|pat| self.midline_probe_buf.starts_with(pat))
                {
                    self.at_post_tool_boundary = false;
                    if ch == '\n' {
                        let line = std::mem::take(&mut self.midline_probe_buf);
                        self.record_dropped_banner(&line);
                        self.at_line_start = true;
                    } else {
                        self.dropped_line_buf.clear();
                        self.dropped_line_buf.push_str(&self.midline_probe_buf);
                        self.midline_probe_buf.clear();
                        self.suppressing_line = true;
                    }
                    continue;
                }
                let still_could_match_midline = ch != '\n'
                    && POST_TOOL_MIDLINE_BANNER_PREFIXES
                        .iter()
                        .any(|pat| pat.starts_with(&self.midline_probe_buf));
                if still_could_match_midline {
                    continue;
                }
                self.at_post_tool_boundary = false;
                let flushed = std::mem::take(&mut self.midline_probe_buf);
                self.emit_str(&flushed, out);
                continue;
            }

            self.at_post_tool_boundary = false;
            self.emit_char(ch, out);
        }

        // Also inspect in-progress suppressed banner lines immediately (even if `agy acp`
        // emitted a quota / `429` / malformed-call chunk without a trailing newline right
        // before sleeping!).
        if self.suppressing_line {
            if self.signal.is_none()
                && let Some((sig, detail)) = Self::inspect_banner_line(&self.dropped_line_buf)
            {
                self.signal = Some((sig, detail));
            }
            if !self.saw_malformed_function_call_retry
                && is_malformed_function_call_banner(&self.dropped_line_buf)
            {
                self.saw_malformed_function_call_retry = true;
            }
            if !self.saw_transient_retry_banner
                && (self.dropped_line_buf.contains("API error")
                    || self.dropped_line_buf.contains("retrying in"))
            {
                self.saw_transient_retry_banner = true;
            }
        }
    }

    pub fn finish(&mut self, out: &mut String) {
        if self.suppressing_line {
            let line = std::mem::take(&mut self.dropped_line_buf);
            self.record_dropped_banner(&line);
            self.suppressing_line = false;
        }
        if !self.midline_probe_buf.is_empty() {
            let is_midline_banner = POST_TOOL_MIDLINE_BANNER_PREFIXES
                .iter()
                .any(|pat| self.midline_probe_buf.starts_with(pat));
            if is_midline_banner {
                let line = std::mem::take(&mut self.midline_probe_buf);
                self.record_dropped_banner(&line);
            } else {
                let flushed = std::mem::take(&mut self.midline_probe_buf);
                self.emit_str(&flushed, out);
            }
        }
        if !self.hold_buf.is_empty() {
            let trimmed = self.hold_buf.trim();
            let is_banner = ACP_BANNER_LINE_PREFIXES
                .iter()
                .any(|pat| trimmed.starts_with(pat));
            let is_wrapper =
                !self.has_emitted_non_ws && LEADING_STRAY_WRAPPER_LINES.contains(&trimmed);
            if is_banner {
                let line = std::mem::take(&mut self.hold_buf);
                self.record_dropped_banner(&line);
            } else if is_wrapper {
                self.hold_buf.clear();
            } else {
                let flushed = std::mem::take(&mut self.hold_buf);
                self.emit_str(&flushed, out);
            }
        }
        self.at_post_tool_boundary = false;
        self.at_line_start = true;
    }
}

#[must_use]
pub fn generate_short_nonce() -> String {
    let simple = Uuid::new_v4().simple().to_string();
    simple.get(..12).unwrap_or(&simple).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_filter_strips_multiline_malformed_call_and_leading_wrapper_tags() {
        let mut filter = AcpBannerFilter::new();
        let mut out = String::new();
        let chunks = [
            "</client_tool_res",
            "ult>\n\n</user>\n<assistant>\nChecking files.\n⚠ Your previous response contained an impro",
            "perly formatted function call\nPlease retry with a properly ",
            "formatted function call\nRetries remaining: 3\nReal answer line!\nYes, it works.",
        ];
        for c in chunks {
            filter.feed(c, &mut out);
        }
        filter.finish(&mut out);
        assert_eq!(out, "Checking files.\nReal answer line!\nYes, it works.");
        assert!(filter.take_malformed_retry());
        assert_eq!(filter.take_signal(), None);
    }

    #[test]
    fn classifies_individual_quota_banner_distinct_from_transient_capacity_and_retry() {
        assert_eq!(
            classify_upstream_error_text(
                "⚠ Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 48m36s."
            ),
            Some(UpstreamBannerSignal::QuotaExhausted)
        );
        assert_eq!(
            classify_upstream_error_text(
                "⚠ API error (attempt 1/8): RESOURCE_EXHAUSTED (code 429): No capacity available"
            ),
            Some(UpstreamBannerSignal::RateLimited)
        );
        assert_eq!(
            classify_upstream_error_text(
                "⚠ API error (attempt 1): transient 503 — retrying in 1s..."
            ),
            None
        );
        assert_eq!(
            classify_upstream_error_text(
                "⚠ Your previous response contained an improperly formatted function call: Malformed function call: Failed to parse function call: Function call is empty - no input to parse."
            ),
            None
        );
    }

    #[test]
    fn reconciles_multi_subcall_cached_token_inflation_to_single_window_occupancy() {
        let summed_six_subcalls = AcpUsage {
            input_tokens: 58_432,
            output_tokens: 7_413,
            cached_read_tokens: 656_495,
            cached_write_tokens: 0,
            thought_tokens: 5_789,
            total_tokens: 722_340,
        };

        // 1. With `UsageUpdate` (`115_000` actual context occupancy): exact reconciliation.
        let mut state_with_upd = TurnStreamState::new(97_971);
        state_with_upd.observe_usage_update(115_000, 1_048_576);
        let rec =
            reconcile_turn_context_usage(summed_six_subcalls, &state_with_upd, 97_971, 30_000);
        assert_eq!(rec.total_tokens, 115_000);
        assert_eq!(rec.output_tokens, 7_413);
        assert_eq!(
            rec.input_tokens + rec.cached_read_tokens + rec.cached_write_tokens,
            107_587
        );
        assert!(rec.cached_read_tokens > 90_000 && rec.cached_read_tokens < 105_000);

        // 2. Without `UsageUpdate` (`turn_peak_context_used == 0`): heuristic subcall divisor
        //    still prevents the 722k billable sum from crossing Codex's 245k auto-compact limit.
        let state_no_upd = TurnStreamState::new(97_971);
        let rec_fallback =
            reconcile_turn_context_usage(summed_six_subcalls, &state_no_upd, 97_971, 30_000);
        assert!(rec_fallback.total_tokens < 200_000);
        assert!(rec_fallback.total_tokens >= 97_971);
    }

    #[test]
    fn banner_filter_detects_post_tool_midline_banners_and_preserves_tool_and_prose_warnings() {
        // 1. Mid-line `⚠ Your previous response...` immediately after `</agy_tool_call_abcd>` (no `\n`)
        let mut f1 = AcpBannerFilter::with_tool_nonce("abcd");
        let mut out1 = String::new();
        f1.feed(
            "<agy_tool_call_abcd>{\"name\":\"Read\"}</agy_tool_call_abcd>⚠ Your previous",
            &mut out1,
        );
        f1.feed(
            " response contained an improperly formatted function call",
            &mut out1,
        );
        assert_eq!(
            out1,
            "<agy_tool_call_abcd>{\"name\":\"Read\"}</agy_tool_call_abcd>"
        );
        assert!(f1.take_malformed_retry());

        // 2. Mid-line transient 503 retry banner without trailing `\n` (including optional spaces)
        let mut f2 = AcpBannerFilter::with_tool_nonce("abcd");
        let mut out2 = String::new();
        f2.feed(
            "</agy_tool_call_abcd>  ⚠ API error (attempt 1): transient 503 — retrying in 1s...",
            &mut out2,
        );
        assert_eq!(out2, "</agy_tool_call_abcd>  ");
        assert!(f2.take_transient_retry());
        assert_eq!(f2.take_signal(), None);

        // 3. Mid-line 429 rate-limit banner surfaces `RateLimited` signal immediately
        let mut f3 = AcpBannerFilter::with_tool_nonce("abcd");
        let mut out3 = String::new();
        f3.feed(
            "</agy_tool_call_abcd>⚠ API error (attempt 1/8): RESOURCE_EXHAUSTED (code 429)",
            &mut out3,
        );
        assert_eq!(out3, "</agy_tool_call_abcd>");
        assert!(matches!(
            f3.take_signal(),
            Some((UpstreamBannerSignal::RateLimited, _))
        ));

        // 4. Ordinary assistant prose quoting `⚠ API error (...)` mid-line is preserved intact
        let mut f4 = AcpBannerFilter::with_tool_nonce("abcd");
        let mut out4 = String::new();
        f4.feed("Docs note: `⚠ API error (attempt 1)` retries", &mut out4);
        f4.feed(" automatically.\n", &mut out4);
        f4.finish(&mut out4);
        assert_eq!(
            out4,
            "Docs note: `⚠ API error (attempt 1)` retries automatically.\n"
        );
        assert!(!f4.take_malformed_retry());
        assert!(!f4.take_transient_retry());
        assert_eq!(f4.take_signal(), None);

        // 5. Tool payloads (`apply_patch` / `exec`) containing both mid-line and line-start
        //    `⚠ API error (...)` / `⚠ Individual quota` / `Retries remaining:` are preserved 100%
        //    across tiny 2-byte chunk splits without eating closing `</agy_tool_call_abcd>` tags!
        let mut f5 = AcpBannerFilter::with_tool_nonce("abcd");
        let mut out5 = String::new();
        let tool_stream = "<agy_tool_call_abcd>{\"name\":\"apply_patch\",\"input\":\"*** Begin Patch\\n+ \\\"⚠ API error (attempt 1): transient 503\\\"\\n⚠ Individual quota reached.\\nRetries remaining: 3\\n*** End Patch\"}</agy_tool_call_abcd>⚠ API error (attempt 1): transient 503 — retrying in 1s...";
        let chars: Vec<char> = tool_stream.chars().collect();
        for pair in chars.chunks(2) {
            let chunk: String = pair.iter().collect();
            f5.feed(&chunk, &mut out5);
        }
        f5.finish(&mut out5);
        assert_eq!(
            out5,
            "<agy_tool_call_abcd>{\"name\":\"apply_patch\",\"input\":\"*** Begin Patch\\n+ \\\"⚠ API error (attempt 1): transient 503\\\"\\n⚠ Individual quota reached.\\nRetries remaining: 3\\n*** End Patch\"}</agy_tool_call_abcd>"
        );
        assert!(f5.take_transient_retry());
        assert_eq!(f5.take_signal(), None);
    }
}
