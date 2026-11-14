use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agy_gateway_types::{
    acp::{AcpPromptBlock, AcpUsage, PromptTurnResult, SessionUpdatePayload},
    canonical::{
        AuxiliaryTaskKind, CanonicalMessage, CanonicalPart, CanonicalRole, CanonicalToolKind,
        NormalizedTurnRequest,
    },
    config::{AcpModeId, GatewayLimits, ModelId},
    model::{resolve_model_id, select_failover_models, select_fastest_gemini_model},
    parser::{IncrementalToolParser, ParsedStreamItem, ParsedToolCall},
    session_key::{
        INTERNAL_GATEWAY_WEB_FETCH_TOOL, WebToolBridgeConfig, build_acp_prompt_blocks,
        build_session_system_append, compute_routing_plan, hash_conversation_with_reply,
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
    telemetry::{ActiveTaskGuard, TelemetryHub},
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
    let failover_models = VecDeque::from(select_failover_models(&resolved_model, &available));

    let web_cfg = WebToolBridgeConfig {
        enable_native_web_search: req.enable_native_web_search,
        force_native_web_search: req.force_native_web_search,
        enable_native_web_fetch: req.enable_native_web_fetch,
        force_native_web_fetch: req.force_native_web_fetch,
    };
    let is_ephemeral_aux = req.auxiliary_task.is_some();

    let setup_res: Result<_, GatewayError> = async {
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
                let keep_failover =
                    guard.is_failover_pinned() && guard.requested_model_id() == &resolved_model;
                if !keep_failover && guard.model_id() != &resolved_model {
                    timeout_at(
                        deadline,
                        backend.set_session_model(guard.session_id(), &resolved_model),
                    )
                    .await
                    .map_err(|_| AcpError::TurnTimeout {
                        seconds: timeout_secs,
                    })??;
                    guard.set_model_id(resolved_model.clone());
                }
                let blocks = build_acp_prompt_blocks(&req.messages, guard.tool_nonce(), false);
                return Ok((guard, true, blocks, combined));
            }

            let routing = compute_routing_plan(
                req.system_instructions.as_deref(),
                &req.client_tools,
                web_cfg,
                &resolved_model,
                &req.messages,
            );

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
                let keep_failover =
                    guard.is_failover_pinned() && guard.requested_model_id() == &resolved_model;
                if !keep_failover && guard.model_id() != &resolved_model {
                    timeout_at(
                        deadline,
                        backend.set_session_model(guard.session_id(), &resolved_model),
                    )
                    .await
                    .map_err(|_| AcpError::TurnTimeout {
                        seconds: timeout_secs,
                    })??;
                    guard.set_model_id(resolved_model.clone());
                }
                let blocks = build_acp_prompt_blocks(delta_slice, guard.tool_nonce(), false);
                return Ok((guard, true, blocks, req.messages.clone()));
            }
        }

        let nonce = gateway_process_tool_nonce();
        let system_append = build_session_system_append(
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
            req.messages.len(),
            Arc::clone(&pool),
            Arc::clone(&backend),
        );

        timeout_at(
            deadline,
            backend.set_session_model(guard.session_id(), &resolved_model),
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

        let blocks = build_acp_prompt_blocks(&req.messages, guard.tool_nonce(), true);
        Ok((guard, false, blocks, req.messages.clone()))
    }
    .await;

    let (lease, session_reused, prompt_blocks, base_messages) = match setup_res {
        Ok(t) => t,
        Err(err) => {
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
    let worker_req_model = resolved_model.clone();
    let worker_acp_mode = cfg.acp_mode.clone();
    let worker = tokio::spawn(run_turn_pipeline(
        backend,
        pool,
        lease,
        task_guard,
        prompt_blocks,
        base_messages,
        req,
        web_cfg,
        worker_req_model,
        active_model,
        failover_models,
        worker_acp_mode,
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
    emitted_client_tool: bool,
    halted_post_tool_runaway: bool,
    tainted_session: bool,
}

impl TurnStreamState {
    fn new() -> Self {
        Self {
            parsed_buf: Vec::new(),
            assistant_parts: Vec::new(),
            text_accumulator: String::new(),
            post_tool_spill_buf: String::new(),
            active_searches: HashMap::new(),
            seen_search_ids: HashSet::new(),
            seen_client_tools: HashSet::new(),
            pending_web_fetches: Vec::new(),
            emitted_client_tool: false,
            halted_post_tool_runaway: false,
            tainted_session: false,
        }
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
    pool: Arc<SessionPool>,
    initial_lease: SessionLeaseGuard<B>,
    task_guard: ActiveTaskGuard,
    initial_prompt_blocks: Vec<AcpPromptBlock>,
    mut full_messages: Vec<CanonicalMessage>,
    req: NormalizedTurnRequest,
    web_cfg: WebToolBridgeConfig,
    resolved_model: ModelId,
    mut active_model: ModelId,
    mut failover_models: VecDeque<ModelId>,
    acp_mode: AcpModeId,
    response_id: String,
    deadline: Instant,
    timeout_secs: u64,
    channel_cap: usize,
    events_tx: mpsc::Sender<TurnEvent>,
    cancel: CancellationToken,
) {
    let mut session_id = initial_lease.session_id().clone();
    let mut active_tool_nonce = initial_lease.tool_nonce().to_owned();
    let mut lease = Some(initial_lease);
    let has_xml_tools = !req.client_tools.is_empty() || req.enable_native_web_fetch;
    let max_fetch_uses = req.web_fetch_max_uses.unwrap_or(5).clamp(1, 10) as usize;
    let mut completed_web_fetches: usize = 0;
    let mut cumulative_usage = AcpUsage::default();
    let mut final_stop_reason = "end_turn".to_owned();
    let mut current_prompt_blocks = initial_prompt_blocks;
    let mut state = TurnStreamState::new();

    loop {
        state.halted_post_tool_runaway = false;
        state.post_tool_spill_buf.clear();

        let (acp_tx, mut acp_rx) = mpsc::channel::<SessionUpdatePayload>(channel_cap);
        let mut parser = IncrementalToolParser::new(&active_tool_nonce, has_xml_tools);
        let mut banner_filter = AcpBannerFilter::new();
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
                        if let Some((sig, detail)) = banner_filter.take_signal() {
                            let no_output_yet = state.text_accumulator.is_empty()
                                && state.assistant_parts.is_empty()
                                && !state.emitted_client_tool
                                && state.seen_search_ids.is_empty()
                                && completed_web_fetches == 0;
                            if no_output_yet {
                                upstream_abort = Some((sig, detail));
                                break;
                            }
                            state.halted_post_tool_runaway = true;
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
                if let Some((sig, detail)) = banner_filter.take_signal() {
                    let no_output_yet = state.text_accumulator.is_empty()
                        && state.assistant_parts.is_empty()
                        && !state.emitted_client_tool
                        && state.seen_search_ids.is_empty()
                        && completed_web_fetches == 0;
                    if no_output_yet {
                        upstream_abort = Some((sig, detail));
                        break;
                    }
                    state.halted_post_tool_runaway = true;
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
                if no_output_yet {
                    upstream_abort = Some((sig, detail));
                }
            }
        }

        if let Some((sig, detail)) = upstream_abort {
            if let Some(old_lease) = lease.take() {
                old_lease.finish_ephemeral();
            }

            if sig == UpstreamBannerSignal::RateLimited
                && let Some(backup_model) = failover_models.pop_front()
            {
                tracing::warn!(
                    from_model = %active_model,
                    to_model = %backup_model,
                    detail = %detail,
                    "upstream ACP model rate-limited before stream output; failing over immediately"
                );
                let failover_setup: Result<SessionLeaseGuard<B>, AcpError> = async {
                    let nonce = gateway_process_tool_nonce();
                    let system_append = build_session_system_append(
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
                    let mut new_lease = SessionLeaseGuard::new_fresh(
                        created.session_id,
                        nonce,
                        backup_model.clone(),
                        full_messages.len(),
                        Arc::clone(&pool),
                        Arc::clone(&backend),
                    );
                    new_lease.mark_failover_from(resolved_model.clone());
                    timeout_at(
                        deadline,
                        backend.set_session_model(new_lease.session_id(), &backup_model),
                    )
                    .await
                    .map_err(|_| AcpError::TurnTimeout {
                        seconds: timeout_secs,
                    })??;
                    timeout_at(
                        deadline,
                        backend.set_session_mode(new_lease.session_id(), &acp_mode),
                    )
                    .await
                    .map_err(|_| AcpError::TurnTimeout {
                        seconds: timeout_secs,
                    })??;
                    Ok(new_lease)
                }
                .await;

                match failover_setup {
                    Ok(new_lease) => {
                        session_id = new_lease.session_id().clone();
                        active_tool_nonce = new_lease.tool_nonce().to_owned();
                        active_model = backup_model;
                        current_prompt_blocks =
                            build_acp_prompt_blocks(&full_messages, &active_tool_nonce, true);
                        task_guard.bind_session(session_id.as_str(), false, active_model.as_str());
                        state = TurnStreamState::new();
                        lease = Some(new_lease);
                        continue;
                    }
                    Err(err) => {
                        task_guard.fail(err.to_string());
                        let _ = events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
                        return;
                    }
                }
            }

            let err = match sig {
                UpstreamBannerSignal::RateLimited => AcpError::UpstreamRateLimited {
                    model: active_model.as_str().to_owned(),
                    detail,
                },
                UpstreamBannerSignal::FatalAgentError => {
                    AcpError::UpstreamAgentTerminated { detail }
                }
            };
            tracing::error!(error = ?err, "aborting ACP prompt immediately due to upstream fatal/rate-limit banner");
            task_guard.fail(err.to_string());
            let _ = events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
            return;
        }

        match prompt_outcome {
            Some(Ok(turn_res)) => {
                if !state.halted_post_tool_runaway {
                    if !filter_out.is_empty() {
                        state.parsed_buf.clear();
                        parser.feed(&filter_out, &mut state.parsed_buf);
                        if !emit_parsed_items(&req, &task_guard, &mut state, &events_tx, &cancel)
                            .await
                        {
                            return;
                        }
                    }

                    state.parsed_buf.clear();
                    parser.finish(&mut state.parsed_buf);
                    if !emit_parsed_items(&req, &task_guard, &mut state, &events_tx, &cancel).await
                    {
                        return;
                    }
                }

                accumulate_usage(&mut cumulative_usage, turn_res.usage);
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
        state.assistant_parts.push(CanonicalPart::Text(trailing));
    }

    if let Some(final_lease) = lease.take() {
        if req.auxiliary_task.is_some() || state.tainted_session {
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
                final_lease.requested_model_id(),
                &full_messages,
                &assistant_msg,
            );
            full_messages.push(assistant_msg);
            final_lease.commit(post_hash, Some(response_id), full_messages);
        }
    }

    task_guard.complete(final_stop_reason.clone(), cumulative_usage);
    let _ = events_tx
        .send(TurnEvent::Completed {
            usage: cumulative_usage,
            stop_reason: final_stop_reason,
        })
        .await;
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
                task_guard.record_thought(content.text.chars().count());
                if req.include_thinking && !req.omit_thinking_text && !state.emitted_client_tool {
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
        SessionUpdatePayload::UsageUpdate { .. } | SessionUpdatePayload::Unknown => true,
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
            ParsedStreamItem::ToolCall(ParsedToolCall {
                name,
                arguments,
                is_raw_input,
            }) => {
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

                task_guard.record_client_tool(&name);
                let is_declared_custom = req
                    .client_tools
                    .iter()
                    .any(|t| t.name == name && matches!(t.kind, CanonicalToolKind::Custom { .. }));
                let is_custom = is_declared_custom || is_raw_input;
                let call_id = format!("call_{}", generate_short_nonce());
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
        }
    }
    true
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
    RateLimited,
    FatalAgentError,
}

/// Stateful streaming filter that strips:
/// 1) transient `agy acp` retry / `MALFORMED_FUNCTION_CALL` banners across chunk splits,
/// 2) stray echoed transcript wrapper lines (`</client_tool_result>`, `</user>`, `<assistant>`)
///    at the start of an assistant reply before real content begins, AND
/// 3) surfaces upstream `429 RESOURCE_EXHAUSTED` / fatal agent termination banners immediately
///    (0ms wait) so `run_turn_pipeline` can fail over to a healthy model instead of hanging 158s.
#[derive(Debug, Default)]
pub struct AcpBannerFilter {
    at_line_start: bool,
    suppressing_line: bool,
    has_emitted_non_ws: bool,
    hold_buf: String,
    dropped_line_buf: String,
    signal: Option<(UpstreamBannerSignal, String)>,
}

const ACP_BANNER_LINE_PREFIXES: [&str; 4] = [
    "⚠",
    "Your previous response contained an improperly formatted function call",
    "Please retry with a properly formatted function call",
    "Retries remaining:",
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
            hold_buf: String::new(),
            dropped_line_buf: String::new(),
            signal: None,
        }
    }

    pub fn take_signal(&mut self) -> Option<(UpstreamBannerSignal, String)> {
        self.signal.take()
    }

    fn inspect_banner_line(banner: &str) -> Option<(UpstreamBannerSignal, String)> {
        let trimmed = banner.trim();
        if trimmed.contains("RESOURCE_EXHAUSTED")
            || trimmed.contains("(code 429)")
            || trimmed.contains("QUOTA_EXHAUSTED")
            || trimmed.contains("MODEL_CAPACITY_EXHAUSTED")
            || trimmed.contains("(code 503)")
            || trimmed.contains("Service Unavailable")
        {
            Some((UpstreamBannerSignal::RateLimited, trimmed.to_owned()))
        } else if trimmed.contains("Agent execution terminated due to error")
            || trimmed.contains("UNAUTHENTICATED")
            || trimmed.contains("PERMISSION_DENIED")
        {
            Some((UpstreamBannerSignal::FatalAgentError, trimmed.to_owned()))
        } else {
            None
        }
    }

    pub fn feed(&mut self, chunk: &str, out: &mut String) {
        for ch in chunk.chars() {
            if self.suppressing_line {
                if ch == '\n' {
                    let dropped = self.dropped_line_buf.trim();
                    if !dropped.is_empty() {
                        if self.signal.is_none() {
                            self.signal = Self::inspect_banner_line(dropped);
                        }
                        tracing::warn!(
                            banner = %dropped,
                            "filtered internal `agy acp` retry/diagnostic banner from assistant stream"
                        );
                    }
                    self.dropped_line_buf.clear();
                    self.suppressing_line = false;
                    self.at_line_start = true;
                } else if self.dropped_line_buf.len() < 512 {
                    self.dropped_line_buf.push(ch);
                }
                continue;
            }

            if self.at_line_start || !self.hold_buf.is_empty() {
                if ch == '\n' && self.hold_buf.is_empty() {
                    if self.has_emitted_non_ws {
                        out.push('\n');
                    }
                    self.at_line_start = true;
                    continue;
                }
                self.at_line_start = false;
                self.hold_buf.push(ch);
                let trimmed_start = self.hold_buf.trim_start_matches([' ', '\r']);
                if trimmed_start.is_empty() {
                    if ch == '\n' {
                        if self.has_emitted_non_ws {
                            out.push_str(&self.hold_buf);
                        }
                        self.hold_buf.clear();
                        self.at_line_start = true;
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
                        let dropped = trimmed_start.trim();
                        if self.signal.is_none() {
                            self.signal = Self::inspect_banner_line(dropped);
                        }
                        tracing::warn!(
                            banner = %dropped,
                            "filtered internal `agy acp` retry/diagnostic banner from assistant stream"
                        );
                        self.hold_buf.clear();
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
                    self.has_emitted_non_ws = true;
                    out.push_str(&self.hold_buf);
                    self.hold_buf.clear();
                    if ch == '\n' {
                        self.at_line_start = true;
                    }
                }
                continue;
            }

            if !ch.is_whitespace() {
                self.has_emitted_non_ws = true;
            }
            out.push(ch);
            if ch == '\n' {
                self.at_line_start = true;
            }
        }

        // Also inspect in-progress suppressed banner lines immediately (even if `agy acp`
        // emitted the `⚠ API error (attempt 1/8): RESOURCE_EXHAUSTED (code 429)` chunk
        // without a trailing newline right before sleeping!).
        if self.suppressing_line && self.signal.is_none() {
            self.signal = Self::inspect_banner_line(&self.dropped_line_buf);
        }
    }

    pub fn finish(&mut self, out: &mut String) {
        if self.suppressing_line {
            let dropped = self.dropped_line_buf.trim();
            if !dropped.is_empty() {
                if self.signal.is_none() {
                    self.signal = Self::inspect_banner_line(dropped);
                }
                tracing::warn!(
                    banner = %dropped,
                    "filtered trailing `agy acp` diagnostic banner at end of turn"
                );
            }
            self.dropped_line_buf.clear();
            self.suppressing_line = false;
        }
        if !self.hold_buf.is_empty() {
            let trimmed = self.hold_buf.trim();
            let is_complete_banner = ACP_BANNER_LINE_PREFIXES
                .iter()
                .any(|pat| trimmed.starts_with(pat))
                || (!self.has_emitted_non_ws && LEADING_STRAY_WRAPPER_LINES.contains(&trimmed));
            if is_complete_banner {
                if self.signal.is_none() {
                    self.signal = Self::inspect_banner_line(trimmed);
                }
            } else {
                self.has_emitted_non_ws = true;
                out.push_str(&self.hold_buf);
            }
            self.hold_buf.clear();
        }
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
    }
}
