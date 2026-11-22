use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc, OnceLock, Weak,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use liftgate_types::{
    acp::{AcpPromptBlock, AcpUsage, PromptTurnResult, SessionUpdatePayload},
    canonical::{
        AuxiliaryTaskKind, CanonicalMessage, CanonicalPart, CanonicalRole, CanonicalToolKind,
        NormalizedTurnRequest,
    },
    config::{AcpModeId, GatewayLimits, ModelId},
    error::format_error_chain,
    model::{resolve_model_id, select_fastest_gemini_model},
    parser::{IncrementalToolParser, ParsedStreamItem, ParsedToolCall},
    session_key::{
        ConversationHash, INTERNAL_GATEWAY_WEB_FETCH_TOOL, MAX_ACP_PROMPT_TEXT_BYTES,
        McpBridgePromptSpec, WebToolBridgeConfig, build_acp_prompt_plan_with_spill,
        build_client_tools_delta_xml_with_spill, build_compaction_directive_xml_with_spill,
        build_mcp_virtualized_tools_delta_xml_with_spill,
        build_session_system_append_for_mcp_with_spill_sink,
        build_session_system_append_with_spill_sink, compute_routing_plan,
        format_conversation_hash_hex, hash_conversation_state, hash_conversation_with_reply,
        hash_dialogue_state, hash_dialogue_with_reply, scrub_terminal_control_noise,
        strip_billing_noise, truncate_head_tail_lines_with_spill,
    },
    telemetry::{LowCacheRoutingStrategy, TaskProtocol},
};
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, timeout, timeout_at},
};
use tokio_util::sync::{CancellationToken, DropGuard};
use uuid::Uuid;

use crate::{
    acp_client::AcpBackend,
    error::{AcpError, GatewayError},
    mcp_bridge::{
        ACP_MCP_TOOL_PREFIX, IncomingMcpToolCall, LocalReplyKind, LoopbackMcpServer,
        McpContentItem, McpSessionHandle, McpToolCallReply, ProtocolHint, ResolvedMcpIncoming,
        partition_tools_for_mcp_with_hint, strip_call_prefix,
    },
    session_pool::{
        CommittedSessionTurn, IdleCheckoutGuard, IdleSessionEntry, McpDriverCommand,
        McpResumedStepInput, PendingMcpCallInfo, PromotedMcpSession, SessionLeaseGuard,
        SessionPool, SuspendedCheckoutGuard, SuspendedMcpTurn,
    },
    spill_store::SpillStore,
    telemetry::{ActiveTaskGuard, TelemetryHub, TurnCompletionDiagnostics, unix_timestamp_ms},
    web_fetch::{WebFetchError, WebFetchOutcome},
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
        // `_cancel_guard` drops immediately after this hook, signalling `CancellationToken`
        // so `run_turn_pipeline` can soft-cancel & recycle the live ACP session (`session/cancel`
        // without `session/close`) instead of destroying warm context on steering interrupts.
    }
}

type BoxedPromptFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = Result<Result<PromptTurnResult, AcpError>, tokio::time::error::Elapsed>,
            > + Send,
    >,
>;

/// Lightweight routing & payload metadata retained across a turn so low-cache-hit diagnostics
/// can inspect exact hashes, idle duration, and ACP payloads with zero cloning on cache hits.
pub(crate) struct TurnRoutingMetadata {
    routing_strategy: LowCacheRoutingStrategy,
    delta_start_index: usize,
    session_turn_index: u64,
    session_idle_ms: Option<u64>,
    previous_session_context_tokens: u64,
    model_switched: bool,
    previous_model: Option<String>,
    prefix_hash_hex: Option<String>,
    base_state_hash_hex: String,
    acp_system_append: Option<String>,
    tools_delta_xml: Option<String>,
}

/// Shared gateway runtime configuration.
#[derive(Clone, Debug)]
pub struct GatewayRuntimeConfig {
    pub title_model_file: Option<std::path::PathBuf>,
    pub spill_dir: Option<std::path::PathBuf>,
    pub settings_path: Option<std::path::PathBuf>,
    pub default_model: ModelId,
    pub acp_mode: AcpModeId,
    pub api_key: Option<Arc<str>>,
    pub limits: GatewayLimits,
    /// Live runtime toggle for `>=150k`-token ephemeral vision sub-session offload (`true` by default).
    pub vision_offload: Arc<AtomicBool>,
    /// Live runtime toggle for Hybrid Loopback MCP Tool Bridge (`true` by default).
    pub mcp_tool_bridge: Arc<AtomicBool>,
    /// Dedicated loopback-only (`127.0.0.1:0`) HTTP MCP server for native ACP tool suspension.
    pub mcp_server: Option<Arc<LoopbackMcpServer>>,
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
        req.auxiliary_task,
        &req.messages,
    );

    let timeout_secs = cfg.limits.turn_timeout_secs().get();
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);

    let available = backend.refresh_models_if_empty().await;
    let resolved_model = match req.auxiliary_task {
        Some(AuxiliaryTaskKind::SessionTitleOrTopic) => {
            crate::title_model::title_model(cfg.title_model_file.as_deref()).await
        }
        Some(_) => select_fastest_gemini_model(&available, &cfg.default_model),
        None => resolve_model_id(
            req.requested_model.as_deref(),
            req.reasoning_effort,
            &available,
            &cfg.default_model,
        ),
    };
    if let Some(kind) = req.auxiliary_task {
        tracing::debug!(auxiliary_task = ?kind, %resolved_model, "routing harness auxiliary request");
    }

    let web_cfg = WebToolBridgeConfig {
        enable_native_web_search: req.enable_native_web_search,
        force_native_web_search: req.force_native_web_search,
        enable_native_web_fetch: req.enable_native_web_fetch,
        force_native_web_fetch: req.force_native_web_fetch,
    };
    let is_ephemeral_aux = req.auxiliary_task.is_some();
    let mcp_enabled = cfg.mcp_tool_bridge.load(Ordering::Relaxed);
    let vision_offload_enabled = cfg.vision_offload.load(Ordering::Relaxed);
    let cap = cfg.limits.channel_capacity().get();
    let mut spill_store = cfg.spill_dir.as_ref().map(SpillStore::new);

    // 0. Fast-path check for a parked MCP-suspended session (`session/prompt` awaiting `tools/call`
    //    reply). Either resumes directly inside the live `session/prompt` or gracefully unwinds via
    //    `session/cancel` onto the warm session continuation path.
    let mut pre_unwound_resp_checkout: Option<IdleCheckoutGuard<B>> = None;
    let mut pre_unwound_prefix_checkout: Option<(IdleCheckoutGuard<B>, ConversationHash, usize)> =
        None;
    let mut pre_unwound_compaction_checkout: Option<(
        IdleCheckoutGuard<B>,
        ConversationHash,
        usize,
    )> = None;

    if !is_ephemeral_aux {
        if let Some(prev_resp_id) = req.previous_response_id.as_deref()
            && let Some(susp_checkout) =
                pool.checkout_suspended_by_response_id(prev_resp_id, Arc::clone(&backend))
        {
            if mcp_enabled
                && susp_checkout.entry().model_id == resolved_model
                && can_satisfy_pending_mcp_calls(
                    &susp_checkout.entry().pending_calls,
                    &req.messages,
                )
            {
                let delta_start_index = susp_checkout.entry().accumulated_messages.len();
                let combined_len = delta_start_index.saturating_add(req.messages.len());
                let (mut promoted, step_lease) =
                    susp_checkout.promote_for_mcp_continuation(combined_len);
                let delta_for_reply = req.messages.clone();
                promoted
                    .accumulated_messages
                    .extend(std::mem::take(&mut req.messages));
                return Ok(spawn_resumed_mcp_continuation(
                    backend,
                    promoted,
                    step_lease,
                    vision_offload_enabled,
                    task_guard,
                    delta_for_reply,
                    delta_start_index,
                    req,
                    web_cfg,
                    resolved_model,
                    cfg.acp_mode.clone(),
                    response_id,
                    deadline,
                    timeout_secs,
                    cap,
                    spill_store,
                ));
            }

            let saved_history = susp_checkout.entry().accumulated_messages.clone();
            let unwound_opt = if mcp_enabled {
                unwind_suspended_to_idle_entry(&backend, susp_checkout, deadline).await
            } else {
                abort_suspended_checkout(&backend, susp_checkout);
                None
            };
            if let Some(unwound_entry) = unwound_opt {
                pre_unwound_resp_checkout = Some(IdleCheckoutGuard::from_unwound(
                    unwound_entry,
                    Arc::clone(&pool),
                    Arc::clone(&backend),
                ));
            } else if !saved_history.is_empty() {
                let mut rebuilt = saved_history;
                rebuilt.extend(std::mem::take(&mut req.messages));
                req.messages = rebuilt;
            }
        } else if req.previous_response_id.is_none() {
            let susp_routing = compute_routing_plan(
                req.system_instructions.as_deref(),
                &req.client_tools,
                web_cfg,
                &resolved_model,
                &req.messages,
            );
            let susp_is_compaction = susp_routing.compaction_lookup.is_some();

            enum SuspHashMatch<B: AcpBackend> {
                PostTurn {
                    checkout: SuspendedCheckoutGuard<B>,
                    hash: ConversationHash,
                    delta_start: usize,
                },
                PreStep {
                    checkout: SuspendedCheckoutGuard<B>,
                    hash: ConversationHash,
                    delta_start: usize,
                },
                Compaction {
                    checkout: SuspendedCheckoutGuard<B>,
                    hash: ConversationHash,
                    delta_start: usize,
                },
            }

            let matched_susp = susp_routing
                .prefix_lookup
                .and_then(|(h, delta_start)| {
                    pool.checkout_suspended_by_post_hash(&h, Arc::clone(&backend))
                        .map(|checkout| SuspHashMatch::PostTurn {
                            checkout,
                            hash: h,
                            delta_start,
                        })
                })
                .or_else(|| {
                    susp_routing.compaction_lookup.and_then(|(h, delta_start)| {
                        pool.checkout_suspended_by_dialogue_hash(&h, Arc::clone(&backend))
                            .map(|checkout| SuspHashMatch::Compaction {
                                checkout,
                                hash: h,
                                delta_start,
                            })
                    })
                })
                .or_else(|| {
                    susp_routing
                        .cancelled_turn_lookup
                        .and_then(|(h, delta_start)| {
                            pool.checkout_suspended_by_pre_hash(&h, Arc::clone(&backend))
                                .map(|checkout| SuspHashMatch::PreStep {
                                    checkout,
                                    hash: h,
                                    delta_start,
                                })
                        })
                })
                .or_else(|| {
                    susp_routing.prefix_lookup.and_then(|(h, delta_start)| {
                        pool.checkout_suspended_by_pre_hash(&h, Arc::clone(&backend))
                            .or_else(|| {
                                pool.checkout_suspended_by_alt_pre_user_hash(
                                    &h,
                                    Arc::clone(&backend),
                                )
                            })
                            .map(|checkout| SuspHashMatch::PreStep {
                                checkout,
                                hash: h,
                                delta_start,
                            })
                    })
                });

            match matched_susp {
                Some(SuspHashMatch::PostTurn {
                    checkout,
                    hash,
                    delta_start,
                }) => {
                    if mcp_enabled
                        && !susp_is_compaction
                        && checkout.entry().model_id == resolved_model
                        && let Some(delta_slice) = req.messages.get(delta_start..)
                        && can_satisfy_pending_mcp_calls(
                            &checkout.entry().pending_calls,
                            delta_slice,
                        )
                    {
                        let delta_for_reply = delta_slice.to_vec();
                        let combined_len = req.messages.len();
                        let (mut promoted, step_lease) =
                            checkout.promote_for_mcp_continuation(combined_len);
                        promoted.accumulated_messages = std::mem::take(&mut req.messages);
                        return Ok(spawn_resumed_mcp_continuation(
                            backend,
                            promoted,
                            step_lease,
                            vision_offload_enabled,
                            task_guard,
                            delta_for_reply,
                            delta_start,
                            req,
                            web_cfg,
                            resolved_model,
                            cfg.acp_mode.clone(),
                            response_id,
                            deadline,
                            timeout_secs,
                            cap,
                            spill_store,
                        ));
                    }

                    if !mcp_enabled {
                        abort_suspended_checkout(&backend, checkout);
                    } else if let Some(mut unwound_entry) =
                        unwind_suspended_to_idle_entry(&backend, checkout, deadline).await
                    {
                        unwound_entry.post_turn_hash = hash;
                        pre_unwound_prefix_checkout = Some((
                            IdleCheckoutGuard::from_unwound(
                                unwound_entry,
                                Arc::clone(&pool),
                                Arc::clone(&backend),
                            ),
                            hash,
                            delta_start,
                        ));
                    }
                }
                Some(SuspHashMatch::PreStep {
                    checkout,
                    hash,
                    delta_start,
                }) => {
                    if !mcp_enabled {
                        abort_suspended_checkout(&backend, checkout);
                    } else if let Some(mut unwound_entry) =
                        unwind_suspended_to_idle_entry(&backend, checkout, deadline).await
                    {
                        unwound_entry.post_turn_hash = hash;
                        pre_unwound_prefix_checkout = Some((
                            IdleCheckoutGuard::from_unwound(
                                unwound_entry,
                                Arc::clone(&pool),
                                Arc::clone(&backend),
                            ),
                            hash,
                            delta_start,
                        ));
                    }
                }
                Some(SuspHashMatch::Compaction {
                    checkout,
                    hash,
                    delta_start,
                }) => {
                    if !mcp_enabled {
                        abort_suspended_checkout(&backend, checkout);
                    } else if let Some(mut unwound_entry) =
                        unwind_suspended_to_idle_entry(&backend, checkout, deadline).await
                    {
                        unwound_entry.post_turn_dialogue_hash = hash;
                        pre_unwound_compaction_checkout = Some((
                            IdleCheckoutGuard::from_unwound(
                                unwound_entry,
                                Arc::clone(&pool),
                                Arc::clone(&backend),
                            ),
                            hash,
                            delta_start,
                        ));
                    }
                }
                None => {}
            }
        }
    }

    let setup_res: Result<_, GatewayError> = async {
        let mut is_compaction_turn = false;
        // 1. Attempt stateful `previous_response_id` or canonical prefix-hash continuation
        //    ONLY for interactive turns (auxiliary tasks such as forked away-recaps must never
        //    check out or cancel the user's active conversation session).
        if !is_ephemeral_aux {
            let resp_checkout_opt = pre_unwound_resp_checkout.take().or_else(|| {
                req.previous_response_id
                    .as_deref()
                    .and_then(|prev_resp_id| {
                        pool.checkout_by_response_id(prev_resp_id, Arc::clone(&backend))
                    })
            });
            if let Some(checkout) = resp_checkout_opt {
                if !mcp_enabled && checkout.entry().mcp_handle.is_some() {
                    let (retired_lease, mut saved_history) = checkout.into_lease_guard(0);
                    retired_lease.finish_ephemeral();
                    if !saved_history.is_empty() {
                        saved_history.extend(std::mem::take(&mut req.messages));
                        req.messages = saved_history;
                    }
                } else {
                    let delta_start_index = checkout.entry().accumulated_messages.len();
                    let previous_session_context_tokens = checkout.entry().last_context_used;
                    let prefix_hash_hex = Some(format_conversation_hash_hex(
                        &checkout.entry().post_turn_hash,
                    ));
                    let combined_len = checkout
                        .entry()
                        .accumulated_messages
                        .len()
                        .saturating_add(req.messages.len());
                    let tools_delta = build_client_tools_delta_xml_with_spill(
                        &checkout.entry().declared_tools,
                        &req.client_tools,
                        &checkout.entry().tool_nonce,
                        &mut spill_store,
                    );
                    let prompt_blocks = build_acp_prompt_plan_with_spill(
                        &req.messages,
                        &checkout.entry().tool_nonce,
                        false,
                        tools_delta.as_deref(),
                        &mut spill_store,
                    )?;
                    let (mut guard, mut combined) = checkout.into_lease_guard(combined_len);
                    let session_idle_ms = guard.idle_before_checkout_ms();
                    let session_turn_index = guard.turn_count();
                    let mut model_switched = false;
                    let mut previous_model = None;
                    if guard.model_id() != &resolved_model {
                        model_switched = true;
                        previous_model = Some(guard.model_id().as_str().to_owned());
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
                    combined.extend(std::mem::take(&mut req.messages));
                    let base_state_hash_hex =
                        format_conversation_hash_hex(&hash_conversation_state(
                            req.system_instructions.as_deref(),
                            &req.client_tools,
                            web_cfg,
                            &resolved_model,
                            &combined,
                        ));
                    let route_meta = TurnRoutingMetadata {
                        routing_strategy: LowCacheRoutingStrategy::PreviousResponseId,
                        delta_start_index,
                        session_turn_index,
                        session_idle_ms,
                        previous_session_context_tokens,
                        model_switched,
                        previous_model,
                        prefix_hash_hex,
                        base_state_hash_hex,
                        acp_system_append: None,
                        tools_delta_xml: tools_delta,
                    };
                    return Ok((guard, true, false, prompt_blocks, combined, route_meta));
                }
            }

            let routing = compute_routing_plan(
                req.system_instructions.as_deref(),
                &req.client_tools,
                web_cfg,
                &resolved_model,
                &req.messages,
            );
            is_compaction_turn = routing.compaction_lookup.is_some();

            if pre_unwound_prefix_checkout.is_none() && pre_unwound_compaction_checkout.is_none() {
                let mut wait_candidates = Vec::with_capacity(4);
                if let Some((h, _)) = routing.prefix_lookup {
                    wait_candidates.push(h);
                }
                if let Some((h, _)) = routing.cancelled_turn_lookup {
                    wait_candidates.push(h);
                }
                if let Some((h, _)) = routing.compaction_lookup {
                    wait_candidates.push(h);
                }
                wait_candidates.push(routing.base_state_hash_before_reply);
                pool.wait_for_pending_checkin(&wait_candidates, Duration::from_secs(2))
                    .await;
            }

            let matched_prefix_checkout = pre_unwound_prefix_checkout.take().or_else(|| {
                routing
                    .prefix_lookup
                    .and_then(|(prefix_hash, delta_start)| {
                        pool.checkout_by_hash(&prefix_hash, Arc::clone(&backend))
                            .map(|checkout| (checkout, prefix_hash, delta_start))
                    })
                    .or_else(|| {
                        routing
                            .cancelled_turn_lookup
                            .and_then(|(cancel_hash, delta_start)| {
                                pool.checkout_by_hash(&cancel_hash, Arc::clone(&backend))
                                    .map(|checkout| (checkout, cancel_hash, delta_start))
                            })
                    })
                    .or_else(|| {
                        routing
                            .prefix_lookup
                            .and_then(|(prefix_hash, delta_start)| {
                                pool.checkout_by_alt_hash(&prefix_hash, Arc::clone(&backend))
                                    .map(|checkout| (checkout, prefix_hash, delta_start))
                            })
                    })
            });

            if let Some((checkout, prefix_hash, delta_start)) = matched_prefix_checkout {
                if !mcp_enabled && checkout.entry().mcp_handle.is_some() {
                    let (retired_lease, _) = checkout.into_lease_guard(0);
                    retired_lease.finish_ephemeral();
                } else if let Some(delta_slice) = req.messages.get(delta_start..) {
                    let previous_session_context_tokens = checkout.entry().last_context_used;
                    let use_virtualized_delta =
                        checkout.entry().mcp_handle.as_ref().is_some_and(|handle| {
                            handle.sync_client_tools(&req.client_tools);
                            handle.has_virtualized_dispatch()
                        });
                    let tools_delta = if use_virtualized_delta {
                        build_mcp_virtualized_tools_delta_xml_with_spill(
                            &checkout.entry().declared_tools,
                            &req.client_tools,
                            &checkout.entry().tool_nonce,
                            &mut spill_store,
                        )
                    } else {
                        build_client_tools_delta_xml_with_spill(
                            &checkout.entry().declared_tools,
                            &req.client_tools,
                            &checkout.entry().tool_nonce,
                            &mut spill_store,
                        )
                    };
                    let prompt_blocks = build_acp_prompt_plan_with_spill(
                        delta_slice,
                        &checkout.entry().tool_nonce,
                        false,
                        tools_delta.as_deref(),
                        &mut spill_store,
                    )?;
                    let (mut guard, _) = checkout.into_lease_guard(req.messages.len());
                    let session_idle_ms = guard.idle_before_checkout_ms();
                    let session_turn_index = guard.turn_count();
                    let mut model_switched = false;
                    let mut previous_model = None;
                    if guard.model_id() != &resolved_model {
                        model_switched = true;
                        previous_model = Some(guard.model_id().as_str().to_owned());
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
                    let route_meta = TurnRoutingMetadata {
                        routing_strategy: LowCacheRoutingStrategy::PrefixHash,
                        delta_start_index: delta_start,
                        session_turn_index,
                        session_idle_ms,
                        previous_session_context_tokens,
                        model_switched,
                        previous_model,
                        prefix_hash_hex: Some(format_conversation_hash_hex(&prefix_hash)),
                        base_state_hash_hex: format_conversation_hash_hex(
                            &routing.base_state_hash_before_reply,
                        ),
                        acp_system_append: None,
                        tools_delta_xml: tools_delta,
                    };
                    return Ok((
                        guard,
                        true,
                        is_compaction_turn,
                        prompt_blocks,
                        std::mem::take(&mut req.messages),
                        route_meta,
                    ));
                }
            }

            let matched_compaction_checkout =
                pre_unwound_compaction_checkout.take().or_else(|| {
                    routing
                        .compaction_lookup
                        .and_then(|(dialogue_hash, delta_start)| {
                            pool.checkout_by_dialogue_hash(&dialogue_hash, Arc::clone(&backend))
                                .map(|checkout| (checkout, dialogue_hash, delta_start))
                        })
                });

            if let Some((checkout, dialogue_hash, delta_start)) = matched_compaction_checkout
                && let Some(delta_slice) = req.messages.get(delta_start..)
            {
                let previous_session_context_tokens = checkout.entry().last_context_used;
                let use_virtualized_delta =
                    checkout.entry().mcp_handle.as_ref().is_some_and(|handle| {
                        handle.sync_client_tools(&req.client_tools);
                        handle.has_virtualized_dispatch()
                    });
                let tools_delta = if use_virtualized_delta {
                    build_mcp_virtualized_tools_delta_xml_with_spill(
                        &checkout.entry().declared_tools,
                        &req.client_tools,
                        &checkout.entry().tool_nonce,
                        &mut spill_store,
                    )
                } else {
                    build_client_tools_delta_xml_with_spill(
                        &checkout.entry().declared_tools,
                        &req.client_tools,
                        &checkout.entry().tool_nonce,
                        &mut spill_store,
                    )
                };
                let directive = build_compaction_directive_xml_with_spill(
                    req.system_instructions.as_deref(),
                    tools_delta.as_deref(),
                    &mut spill_store,
                );
                let prompt_blocks = build_acp_prompt_plan_with_spill(
                    delta_slice,
                    &checkout.entry().tool_nonce,
                    false,
                    directive.as_deref(),
                    &mut spill_store,
                )?;
                let (mut guard, _) = checkout.into_lease_guard(req.messages.len());
                let session_idle_ms = guard.idle_before_checkout_ms();
                let session_turn_index = guard.turn_count();
                let mut model_switched = false;
                let mut previous_model = None;
                if guard.model_id() != &resolved_model {
                    model_switched = true;
                    previous_model = Some(guard.model_id().as_str().to_owned());
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
                tracing::info!(
                    session_id = %guard.session_id(),
                    delta_messages = delta_slice.len(),
                    "reusing warm pre-compaction ACP session for harness context compaction"
                );
                let route_meta = TurnRoutingMetadata {
                    routing_strategy: LowCacheRoutingStrategy::CompactionDialogueHash,
                    delta_start_index: delta_start,
                    session_turn_index,
                    session_idle_ms,
                    previous_session_context_tokens,
                    model_switched,
                    previous_model,
                    prefix_hash_hex: Some(format_conversation_hash_hex(&dialogue_hash)),
                    base_state_hash_hex: format_conversation_hash_hex(
                        &routing.base_state_hash_before_reply,
                    ),
                    acp_system_append: None,
                    tools_delta_xml: directive,
                };
                return Ok((
                    guard,
                    true,
                    true,
                    prompt_blocks,
                    std::mem::take(&mut req.messages),
                    route_meta,
                ));
            }
        }

        let nonce = gateway_process_tool_nonce();
        let mut mcp_session_handle: Option<Arc<McpSessionHandle>> = None;
        let (system_append, initial_tools_spill) = if !is_ephemeral_aux
            && mcp_enabled
            && !req.client_tools.is_empty()
            && let Some(ref mcp_srv) = cfg.mcp_server
        {
            let proto_hint = match protocol {
                TaskProtocol::OpenaiResponses => ProtocolHint::OpenAiResponses,
                TaskProtocol::AnthropicMessages => ProtocolHint::AnthropicMessages,
            };
            let catalog =
                partition_tools_for_mcp_with_hint(proto_hint, &req.client_tools, &mut spill_store);
            if catalog.is_empty() {
                build_session_system_append_with_spill_sink(
                    req.system_instructions.as_deref(),
                    &req.client_tools,
                    web_cfg,
                    &nonce,
                    &mut spill_store,
                )
            } else {
                let out = build_session_system_append_for_mcp_with_spill_sink(
                    req.system_instructions.as_deref(),
                    McpBridgePromptSpec {
                        bridged_tools: &catalog.bridged_pairs,
                        xml_tools: &catalog.xml_only_tools,
                        virtualized_hubs: &catalog.virtualized_hubs,
                        has_virtualized_dispatch: catalog.has_virtualized_dispatch,
                    },
                    web_cfg,
                    &nonce,
                    &mut spill_store,
                );
                mcp_session_handle = Some(mcp_srv.register_session(catalog));
                out
            }
        } else {
            build_session_system_append_with_spill_sink(
                req.system_instructions.as_deref(),
                &req.client_tools,
                web_cfg,
                &nonce,
                &mut spill_store,
            )
        };

        let created = if let Some(ref handle) = mcp_session_handle {
            timeout_at(
                deadline,
                backend.create_session_with_mcp(
                    &system_append,
                    std::slice::from_ref(handle.server_config()),
                ),
            )
            .await
            .map_err(|_| AcpError::TurnTimeout {
                seconds: timeout_secs,
            })??
        } else {
            timeout_at(deadline, backend.create_session(&system_append))
                .await
                .map_err(|_| AcpError::TurnTimeout {
                    seconds: timeout_secs,
                })??
        };

        let guard = SessionLeaseGuard::new_fresh(
            created.session_id,
            nonce,
            resolved_model.clone(),
            req.client_tools.clone(),
            req.messages.len(),
            Arc::clone(&pool),
            Arc::clone(&backend),
        )
        .with_mcp_handle(mcp_session_handle);

        let prompt_blocks = build_acp_prompt_plan_with_spill(
            &req.messages,
            guard.tool_nonce(),
            true,
            initial_tools_spill.as_deref(),
            &mut spill_store,
        )?;

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

        let route_meta = if is_ephemeral_aux {
            TurnRoutingMetadata {
                routing_strategy: LowCacheRoutingStrategy::ColdNewSession,
                delta_start_index: 0,
                session_turn_index: 1,
                session_idle_ms: None,
                previous_session_context_tokens: 0,
                model_switched: false,
                previous_model: None,
                prefix_hash_hex: None,
                base_state_hash_hex: String::new(),
                acp_system_append: None,
                tools_delta_xml: None,
            }
        } else {
            let cold_routing = compute_routing_plan(
                req.system_instructions.as_deref(),
                &req.client_tools,
                web_cfg,
                &resolved_model,
                &req.messages,
            );
            let has_prior_assistant = req
                .messages
                .iter()
                .any(|m| m.role == CanonicalRole::Assistant);
            TurnRoutingMetadata {
                routing_strategy: if has_prior_assistant {
                    LowCacheRoutingStrategy::ColdMidConversationRebuild
                } else {
                    LowCacheRoutingStrategy::ColdNewSession
                },
                delta_start_index: 0,
                session_turn_index: 1,
                session_idle_ms: None,
                previous_session_context_tokens: 0,
                model_switched: false,
                previous_model: None,
                prefix_hash_hex: cold_routing
                    .prefix_lookup
                    .map(|(h, _)| format_conversation_hash_hex(&h)),
                base_state_hash_hex: format_conversation_hash_hex(
                    &cold_routing.base_state_hash_before_reply,
                ),
                acp_system_append: Some(system_append),
                tools_delta_xml: initial_tools_spill,
            }
        };

        Ok((
            guard,
            false,
            is_ephemeral_aux || is_compaction_turn,
            prompt_blocks,
            std::mem::take(&mut req.messages),
            route_meta,
        ))
    }
    .await;

    let (lease, session_reused, retire_after_turn, prompt_blocks, base_messages, route_meta) =
        match setup_res {
            Ok(t) => t,
            Err(err) => {
                let err = upgrade_gateway_error(err, &resolved_model);
                task_guard.fail(format_error_chain(&err));
                return Err(err);
            }
        };

    let active_model = lease.model_id().clone();
    if !retire_after_turn && req.auxiliary_task.is_none() {
        lease.set_in_flight_hashes(vec![
            hash_conversation_state(
                req.system_instructions.as_deref(),
                &req.client_tools,
                web_cfg,
                &active_model,
                &base_messages,
            ),
            hash_dialogue_state(&active_model, &base_messages),
        ]);
    }
    task_guard.bind_session(
        lease.session_id().as_str(),
        session_reused,
        active_model.as_str(),
    );

    let turn_cancel = CancellationToken::new();
    let cancel_guard = turn_cancel.clone().drop_guard();
    let (events_tx, events_rx) = mpsc::channel::<TurnEvent>(cap);

    let worker_resp_id = response_id.clone();
    let acp_mode = cfg.acp_mode.clone();
    let worker = if lease.mcp_handle().is_some() {
        tokio::spawn(run_mcp_session_driver(
            backend,
            Arc::downgrade(&pool),
            lease,
            session_reused,
            vision_offload_enabled,
            task_guard,
            prompt_blocks,
            base_messages,
            route_meta,
            req,
            web_cfg,
            active_model,
            acp_mode,
            retire_after_turn,
            worker_resp_id,
            timeout_secs,
            cap,
            spill_store,
            events_tx,
            turn_cancel,
        ))
    } else {
        tokio::spawn(run_turn_pipeline(
            backend,
            lease,
            session_reused,
            vision_offload_enabled,
            task_guard,
            prompt_blocks,
            base_messages,
            route_meta,
            req,
            web_cfg,
            active_model,
            acp_mode,
            retire_after_turn,
            worker_resp_id,
            deadline,
            timeout_secs,
            cap,
            spill_store,
            events_tx,
            turn_cancel,
        ))
    };

    Ok(ActiveTurnStream {
        resolved_model,
        response_id,
        rx: events_rx,
        _cancel_guard: cancel_guard,
        _worker: worker,
    })
}

/// Context occupancy threshold (`150,000` tokens) at and above which sending raw multimodal
/// `Image` blocks into a warm ACP session invalidates upstream prompt caches and triggers a
/// 3–7x TTFT cliff (`40–97s/turn`). Warm sessions at or above this threshold inspect incoming
/// images in a zero-history ephemeral ACP sub-session and forward a structured text-only report.
pub const LONG_CONTEXT_VISION_OFFLOAD_THRESHOLD_TOKENS: u64 = 150_000;

const VISION_SUBSESSION_SYSTEM_APPEND: &str = "<vision_subagent_rules>\n\
You are an isolated visual inspection sub-session assisting an active coding session.\n\
- NEVER invoke any tools (`read_file`, `write_file`, `edit_file`, `run_command`, `search_web`, `call_mcp_tool`, `mcp_*`) and NEVER emit XML tool blocks.\n\
- Inspect every attached image thoroughly against the caller context excerpt and respond in concise, exhaustive plain text covering: (1) exact visible text, numbers, labels, and error messages (verbatim OCR); (2) UI layout, component states, colors, alignment, or visual defects; (3) any file paths, stack traces, selectors, or structural relationships shown.\n\
</vision_subagent_rules>\n";

struct EphemeralVisionSessionGuard<B: AcpBackend> {
    session_id: liftgate_types::config::SessionId,
    backend: Arc<B>,
    armed: bool,
}

impl<B: AcpBackend> EphemeralVisionSessionGuard<B> {
    async fn close_gracefully(mut self) {
        self.armed = false;
        let _ = self.backend.close_session(&self.session_id).await;
    }
}

impl<B: AcpBackend> Drop for EphemeralVisionSessionGuard<B> {
    fn drop(&mut self) {
        if self.armed {
            self.backend.abort_session_nonblocking(&self.session_id);
        }
    }
}

enum VisionOffloadOutcome {
    Replaced {
        text_blocks: Vec<AcpPromptBlock>,
        usage: AcpUsage,
        image_count: usize,
    },
    FallbackOriginal,
    RateLimited(AcpError),
    Cancelled,
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal helper borrows existing turn state without extra heap wrapper"
)]
async fn offload_images_via_ephemeral_session<B: AcpBackend>(
    backend: &Arc<B>,
    prompt_blocks: &[AcpPromptBlock],
    active_model: &ModelId,
    acp_mode: &AcpModeId,
    deadline: Instant,
    channel_cap: usize,
    spill_store: &mut Option<SpillStore>,
    cancel: &CancellationToken,
) -> VisionOffloadOutcome {
    let mut image_blocks = Vec::new();
    let mut context_text = String::new();
    for block in prompt_blocks {
        match block {
            AcpPromptBlock::Image { data, mime_type } => {
                image_blocks.push((mime_type.clone(), data.clone()));
            }
            AcpPromptBlock::Text { text } => {
                if !text.trim().is_empty() {
                    if !context_text.is_empty() && !context_text.ends_with('\n') {
                        context_text.push('\n');
                    }
                    context_text.push_str(text.trim());
                }
            }
        }
    }
    let image_count = image_blocks.len();
    if image_count == 0 {
        return VisionOffloadOutcome::FallbackOriginal;
    }

    let created = tokio::select! {
        biased;
        () = cancel.cancelled() => return VisionOffloadOutcome::Cancelled,
        res = timeout_at(deadline, backend.create_session(VISION_SUBSESSION_SYSTEM_APPEND)) => {
            match res {
                Ok(Ok(c)) => c,
                Ok(Err(err)) => {
                    let upgraded = upgrade_acp_error(err, active_model);
                    if matches!(upgraded, AcpError::UpstreamRateLimited { .. } | AcpError::UpstreamQuotaExhausted { .. }) {
                        return VisionOffloadOutcome::RateLimited(upgraded);
                    }
                    tracing::warn!(error = ?upgraded, "ephemeral vision sub-session creation failed; falling back to inline image prompt");
                    return VisionOffloadOutcome::FallbackOriginal;
                }
                Err(_) => return VisionOffloadOutcome::FallbackOriginal,
            }
        }
    };

    let guard = EphemeralVisionSessionGuard {
        session_id: created.session_id.clone(),
        backend: Arc::clone(backend),
        armed: true,
    };

    let setup_ok = tokio::select! {
        biased;
        () = cancel.cancelled() => return VisionOffloadOutcome::Cancelled,
        res = async {
            timeout_at(deadline, backend.set_session_model(&created.session_id, active_model)).await.ok()?.ok()?;
            timeout_at(deadline, backend.set_session_mode(&created.session_id, acp_mode)).await.ok()?.ok()
        } => res.is_some(),
    };
    if !setup_ok {
        return VisionOffloadOutcome::FallbackOriginal;
    }

    let bounded_excerpt = liftgate_types::session_key::truncate_head_tail_lines(
        &context_text,
        4_096,
        "caller context excerpt",
    );
    let mut vision_prompt = Vec::with_capacity(image_count + 1);
    vision_prompt.push(AcpPromptBlock::Text {
        text: format!(
            "<caller_turn_context_excerpt>\n{bounded_excerpt}\n</caller_turn_context_excerpt>\n\
             Inspect the {image_count} attached image(s) in detail and provide a complete plain-text visual inspection report:"
        ),
    });
    for (mime_type, data) in image_blocks {
        vision_prompt.push(AcpPromptBlock::Image { data, mime_type });
    }

    let (v_tx, mut v_rx) = mpsc::channel::<SessionUpdatePayload>(channel_cap);
    let prompt_backend = Arc::clone(backend);
    let vision_sid = created.session_id.clone();
    let mut prompt_fut = std::pin::pin!(async move {
        timeout_at(
            deadline,
            prompt_backend.start_prompt(&vision_sid, &vision_prompt, v_tx),
        )
        .await
    });

    let mut banner_filter = AcpBannerFilter::new();
    let mut filter_chunk = String::new();
    let mut report_text = String::new();
    let mut prompt_done: Option<Result<PromptTurnResult, AcpError>> = None;
    let mut fatal_sig: Option<(UpstreamBannerSignal, String)> = None;

    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return VisionOffloadOutcome::Cancelled,
            maybe_upd = v_rx.recv() => {
                match maybe_upd {
                    Some(SessionUpdatePayload::AgentMessageChunk { content }) => {
                        filter_chunk.clear();
                        banner_filter.feed(&content.text, &mut filter_chunk);
                        report_text.push_str(&filter_chunk);
                        if let Some(sig_pair) = banner_filter.take_signal() {
                            fatal_sig = Some(sig_pair);
                            break;
                        }
                    }
                    Some(SessionUpdatePayload::Error { message, error }) => {
                        let detail = format_update_error_detail(&message, error.as_deref());
                        if !detail.is_empty() {
                            let sig = classify_upstream_error_text(&detail)
                                .unwrap_or(UpstreamBannerSignal::FatalAgentError);
                            fatal_sig = Some((sig, detail));
                            break;
                        }
                    }
                    Some(_) => {}
                    None => {
                        if prompt_done.is_some() {
                            break;
                        }
                    }
                }
            }
            res = &mut prompt_fut, if prompt_done.is_none() => {
                prompt_done = Some(match res {
                    Ok(inner) => inner,
                    Err(_) => Err(AcpError::Cancelled),
                });
            }
        }
    }

    if fatal_sig.is_none() {
        while let Ok(upd) = v_rx.try_recv() {
            if let SessionUpdatePayload::AgentMessageChunk { content } = upd {
                filter_chunk.clear();
                banner_filter.feed(&content.text, &mut filter_chunk);
                report_text.push_str(&filter_chunk);
            }
        }
        filter_chunk.clear();
        banner_filter.finish(&mut filter_chunk);
        report_text.push_str(&filter_chunk);
        if fatal_sig.is_none() {
            fatal_sig = banner_filter.take_signal();
        }
    }

    if let Some((
        sig @ (UpstreamBannerSignal::RateLimited | UpstreamBannerSignal::QuotaExhausted),
        detail,
    )) = fatal_sig
    {
        return VisionOffloadOutcome::RateLimited(signal_to_acp_error(sig, active_model, detail));
    }

    let turn_res = match prompt_done {
        Some(Ok(r)) => r,
        Some(Err(err)) => {
            let upgraded = upgrade_acp_error(err, active_model);
            if matches!(
                upgraded,
                AcpError::UpstreamRateLimited { .. } | AcpError::UpstreamQuotaExhausted { .. }
            ) {
                return VisionOffloadOutcome::RateLimited(upgraded);
            }
            tracing::warn!(error = ?upgraded, "ephemeral vision sub-session prompt failed; falling back to inline image prompt");
            return VisionOffloadOutcome::FallbackOriginal;
        }
        None => return VisionOffloadOutcome::FallbackOriginal,
    };

    let trimmed_report = report_text.trim();
    if trimmed_report.is_empty() {
        return VisionOffloadOutcome::FallbackOriginal;
    }

    guard.close_gracefully().await;

    let bounded_report = if trimmed_report.len() > 24_000 {
        truncate_head_tail_lines_with_spill(
            trimmed_report,
            24_000,
            "ephemeral vision inspection",
            spill_store,
        )
    } else {
        trimmed_report.to_owned()
    };

    let mut merged = String::with_capacity(context_text.len() + bounded_report.len() + 256);
    let mut img_idx = 0usize;
    for block in prompt_blocks {
        match block {
            AcpPromptBlock::Text { text } => {
                if !merged.is_empty() && !merged.ends_with('\n') {
                    merged.push('\n');
                }
                merged.push_str(text);
            }
            AcpPromptBlock::Image { mime_type, .. } => {
                img_idx += 1;
                if !merged.is_empty() && !merged.ends_with('\n') {
                    merged.push('\n');
                }
                merged.push_str(&format!(
                    "[Attached Image #{img_idx} ({mime_type}) — inspected in isolated sub-session below to preserve >150k session prompt cache]\n"
                ));
            }
        }
    }
    if !merged.ends_with('\n') {
        merged.push('\n');
    }
    merged.push_str(&format!(
        "<ephemeral_vision_inspection images=\"{image_count}\">\n{bounded_report}\n</ephemeral_vision_inspection>\n"
    ));
    if merged.len() > MAX_ACP_PROMPT_TEXT_BYTES {
        merged = truncate_head_tail_lines_with_spill(
            &merged,
            MAX_ACP_PROMPT_TEXT_BYTES,
            "vision-augmented prompt",
            spill_store,
        );
    }

    tracing::info!(
        images = image_count,
        report_bytes = bounded_report.len(),
        "offloaded multimodal image inspection to ephemeral zero-history ACP session to preserve >=150k warm prompt cache"
    );

    VisionOffloadOutcome::Replaced {
        text_blocks: vec![AcpPromptBlock::Text { text: merged }],
        usage: turn_res.usage,
        image_count,
    }
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
    step_thought_chars: usize,
    step_text_chars: usize,
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
            step_thought_chars: 0,
            step_text_chars: 0,
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
    session_reused: bool,
    vision_offload_enabled: bool,
    task_guard: ActiveTaskGuard,
    prompt_blocks: Vec<AcpPromptBlock>,
    mut full_messages: Vec<CanonicalMessage>,
    route_meta: TurnRoutingMetadata,
    req: NormalizedTurnRequest,
    web_cfg: WebToolBridgeConfig,
    active_model: ModelId,
    acp_mode: AcpModeId,
    retire_after_turn: bool,
    response_id: String,
    deadline: Instant,
    timeout_secs: u64,
    channel_cap: usize,
    mut spill_store: Option<SpillStore>,
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
    let mut current_prompt_blocks = prompt_blocks;

    if vision_offload_enabled
        && session_reused
        && initial_context_used >= LONG_CONTEXT_VISION_OFFLOAD_THRESHOLD_TOKENS
        && current_prompt_blocks
            .iter()
            .any(|b| matches!(b, AcpPromptBlock::Image { .. }))
    {
        match offload_images_via_ephemeral_session(
            &backend,
            &current_prompt_blocks,
            &active_model,
            &acp_mode,
            deadline,
            channel_cap,
            &mut spill_store,
            &cancel,
        )
        .await
        {
            VisionOffloadOutcome::Replaced {
                text_blocks,
                usage,
                image_count,
            } => {
                accumulate_usage(&mut cumulative_usage, usage);
                task_guard.record_vision_offload(image_count);
                current_prompt_blocks = text_blocks;
            }
            VisionOffloadOutcome::FallbackOriginal => {}
            VisionOffloadOutcome::RateLimited(err) => {
                if let Some(old_lease) = lease.take() {
                    old_lease.finish_ephemeral();
                }
                task_guard.fail(format_error_chain(&err));
                let _ = events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
                return;
            }
            VisionOffloadOutcome::Cancelled => return,
        }
    }

    let outbound_prompt_bytes: usize = current_prompt_blocks
        .iter()
        .map(|b| match b {
            AcpPromptBlock::Text { text } => text.len(),
            AcpPromptBlock::Image { .. } => 8_192,
        })
        .sum();
    let initial_acp_blocks: Arc<[AcpPromptBlock]> = Arc::from(current_prompt_blocks);
    let mut next_step_blocks: Option<Arc<[AcpPromptBlock]>> = Some(Arc::clone(&initial_acp_blocks));
    let mut state = TurnStreamState::new(initial_context_used);
    let mut deferred_soft_cancel_drain: Option<BoxedPromptFuture> = None;
    let mut last_step_started_ms: u64;
    let mut last_suppressed_banners: Vec<String>;
    let mut last_turn_error: Option<String>;

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
        let step_blocks = next_step_blocks
            .take()
            .unwrap_or_else(|| Arc::clone(&initial_acp_blocks));
        let mut prompt_fut: BoxedPromptFuture = Box::pin(async move {
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
                    let can_recycle_warm = req.auxiliary_task.is_none()
                        && !retire_after_turn
                        && !state.tainted_session
                        && completed_web_fetches == 0
                        && backend.cancel_prompt_for_reuse(&session_id);
                    if can_recycle_warm
                        && let Some(cancelled_lease) = lease.take()
                    {
                        drop(task_guard);
                        let post_cancel_hash = hash_conversation_state(
                            req.system_instructions.as_deref(),
                            &req.client_tools,
                            web_cfg,
                            cancelled_lease.model_id(),
                            &full_messages,
                        );
                        let alt_pre_cancel_hash = (route_meta.delta_start_index > 0)
                            .then(|| full_messages.get(..route_meta.delta_start_index))
                            .flatten()
                            .map(|prefix| {
                                hash_conversation_state(
                                    req.system_instructions.as_deref(),
                                    &req.client_tools,
                                    web_cfg,
                                    cancelled_lease.model_id(),
                                    prefix,
                                )
                            });
                        let dialogue_hash =
                            hash_dialogue_state(cancelled_lease.model_id(), &full_messages);
                        let mut pending_hashes = vec![post_cancel_hash, dialogue_hash];
                        if let Some(alt_h) = alt_pre_cancel_hash {
                            pending_hashes.push(alt_h);
                        }
                        let pending_guard =
                            cancelled_lease.register_pending_checkin(pending_hashes);
                        let message_count = full_messages.len();
                        let retained_messages = if req.retain_response_history {
                            full_messages
                        } else {
                            Vec::new()
                        };
                        let declared_tools = req.client_tools;
                        let approx_ctx = if state.turn_peak_context_used > 0 {
                            state.latest_context_used
                        } else {
                            initial_context_used.saturating_add(
                                u64::try_from(outbound_prompt_bytes / 4).unwrap_or(0),
                            )
                        };
                        tokio::spawn(async move {
                            let _pending = pending_guard;
                            match timeout(Duration::from_secs(5), prompt_fut).await {
                                Ok(Ok(Ok(res))) => {
                                    let prompt_tok = res.usage.total_prompt_tokens();
                                    let cached_tok = res.usage.cached_read_tokens;
                                    let final_ctx = if res.usage.total_tokens > 0 {
                                        res.usage.total_tokens
                                    } else {
                                        approx_ctx
                                    };
                                    tracing::info!(
                                        session_id = %cancelled_lease.session_id(),
                                        context_tokens = final_ctx,
                                        "drained cancelled turn via session/cancel and preserved warm ACP session in pool"
                                    );
                                    cancelled_lease.commit(CommittedSessionTurn {
                                        post_turn_hash: post_cancel_hash,
                                        alt_post_turn_hash: alt_pre_cancel_hash,
                                        post_turn_dialogue_hash: dialogue_hash,
                                        declared_tools,
                                        last_context_used: final_ctx,
                                        turn_prompt_tokens: prompt_tok,
                                        turn_cached_read_tokens: cached_tok,
                                        message_count,
                                        last_response_id: None,
                                        accumulated_messages: retained_messages,
                                    });
                                }
                                _ => {
                                    cancelled_lease.finish_ephemeral();
                                }
                            }
                        });
                    }
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
                                state.tainted_session = true;
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
                            state.tainted_session = true;
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
                            if !state.pending_web_fetches.is_empty() && !state.emitted_client_tool {
                                state.tainted_session = true;
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
                            } else if !state.tainted_session
                                && req.auxiliary_task.is_none()
                                && !retire_after_turn
                                && backend.cancel_prompt_for_reuse(&session_id)
                            {
                                let desired_stop = if state.emitted_client_tool {
                                    "tool_use".to_owned()
                                } else {
                                    "end_turn".to_owned()
                                };
                                match timeout(Duration::from_millis(250), &mut prompt_fut).await {
                                    Ok(Ok(Ok(mut res))) => {
                                        res.stop_reason = desired_stop;
                                        prompt_outcome = Some(Ok(res));
                                    }
                                    Ok(Ok(Err(_))) | Ok(Err(_)) => {
                                        state.tainted_session = true;
                                        prompt_outcome = Some(Ok(PromptTurnResult {
                                            stop_reason: desired_stop,
                                            usage: AcpUsage::default(),
                                            error: None,
                                        }));
                                    }
                                    Err(_) => {
                                        deferred_soft_cancel_drain = Some(prompt_fut);
                                        prompt_outcome = Some(Ok(PromptTurnResult {
                                            stop_reason: desired_stop,
                                            usage: AcpUsage::default(),
                                            error: None,
                                        }));
                                        break;
                                    }
                                }
                            } else {
                                state.tainted_session = true;
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
                        state.tainted_session = true;
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
                    state.tainted_session = true;
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
            task_guard.fail(format_error_chain(&err));
            let _ = events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
            return;
        }

        if let Some(aborted_tool) = state.truncated_tool_abort.take() {
            if let Some(old_lease) = lease.take() {
                old_lease.finish_ephemeral();
            }
            let err = AcpError::CorruptedToolCallStream { tool: aborted_tool };
            tracing::error!(error = ?err, "aborting turn immediately due to corrupted in-progress XML tool call");
            task_guard.fail(format_error_chain(&err));
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
                    let err = AcpError::CorruptedToolCallStream { tool: aborted_tool };
                    tracing::error!(error = ?err, "aborting turn due to truncated in-progress XML tool call");
                    task_guard.fail(format_error_chain(&err));
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
                            WebFetchOutcome::from_error(
                                raw_url.clone(),
                                &WebFetchError::MaxUsesExceeded {
                                    max_uses: max_fetch_uses,
                                },
                            )
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
                                        Err(_) => WebFetchOutcome::from_error(
                                            raw_url.clone(),
                                            &WebFetchError::TurnDeadlineExceeded,
                                        ),
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
                        continuation_xml = truncate_head_tail_lines_with_spill(
                            &continuation_xml,
                            MAX_ACP_PROMPT_TEXT_BYTES,
                            "web_fetch content",
                            &mut spill_store,
                        );
                    }
                    state.post_tool_spill_buf.clear();
                    state.halted_post_tool_runaway = false;
                    next_step_blocks = Some(Arc::from(vec![AcpPromptBlock::Text {
                        text: continuation_xml,
                    }]));
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
                task_guard.fail(format_error_chain(&err));
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
        task_guard.fail(format_error_chain(&err));
        let _ = events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
        return;
    }

    context_usage = reconcile_turn_context_usage(
        context_usage,
        &state,
        initial_context_used,
        outbound_prompt_bytes,
    );

    // Materialize low-cache diagnostics BEFORE `full_messages` or `req.client_tools` are moved
    // into `SessionPool`, doing zero work when `cumulative_usage` is a healthy cache hit.
    let complete_stop_reason = final_stop_reason.clone();
    let req_auxiliary = req.auxiliary_task;
    let req_prev_resp_id = req.previous_response_id.clone();
    let req_reasoning_effort = req.reasoning_effort;
    let req_sys_inst = req.system_instructions.clone();

    if let Some(final_lease) = lease.take() {
        if retire_after_turn || req.auxiliary_task.is_some() || state.tainted_session {
            final_lease.finish_ephemeral();
            let diag = TurnCompletionDiagnostics {
                routing_strategy: route_meta.routing_strategy,
                delta_start_index: route_meta.delta_start_index,
                session_turn_index: route_meta.session_turn_index,
                session_idle_ms: route_meta.session_idle_ms,
                previous_session_context_tokens: route_meta.previous_session_context_tokens,
                model_switched: route_meta.model_switched,
                previous_model: route_meta.previous_model.as_deref(),
                previous_response_id: req_prev_resp_id.as_deref(),
                prefix_hash_hex: route_meta.prefix_hash_hex.as_deref(),
                base_state_hash_hex: &route_meta.base_state_hash_hex,
                reasoning_effort: req_reasoning_effort,
                web_cfg,
                acp_system_append: route_meta.acp_system_append.as_deref(),
                tools_delta_xml: route_meta.tools_delta_xml.as_deref(),
                outbound_acp_blocks: &initial_acp_blocks,
                messages: &full_messages,
                client_tools: &req.client_tools,
            };
            task_guard.complete_with_diagnostics(
                complete_stop_reason,
                cumulative_usage,
                req_auxiliary.is_none().then_some(diag),
            );
        } else {
            let pre_reply_len = full_messages.len();
            let assistant_msg = CanonicalMessage {
                role: CanonicalRole::Assistant,
                parts: state.assistant_parts,
            };
            let post_hash = hash_conversation_with_reply(
                req_sys_inst.as_deref(),
                &req.client_tools,
                web_cfg,
                final_lease.model_id(),
                &full_messages,
                &assistant_msg,
            );
            let dialogue_hash =
                hash_dialogue_with_reply(final_lease.model_id(), &full_messages, &assistant_msg);
            {
                let pre_reply_slice = full_messages.get(..pre_reply_len).unwrap_or(&[]);
                let diag = TurnCompletionDiagnostics {
                    routing_strategy: route_meta.routing_strategy,
                    delta_start_index: route_meta.delta_start_index,
                    session_turn_index: route_meta.session_turn_index,
                    session_idle_ms: route_meta.session_idle_ms,
                    previous_session_context_tokens: route_meta.previous_session_context_tokens,
                    model_switched: route_meta.model_switched,
                    previous_model: route_meta.previous_model.as_deref(),
                    previous_response_id: req_prev_resp_id.as_deref(),
                    prefix_hash_hex: route_meta.prefix_hash_hex.as_deref(),
                    base_state_hash_hex: &route_meta.base_state_hash_hex,
                    reasoning_effort: req_reasoning_effort,
                    web_cfg,
                    acp_system_append: route_meta.acp_system_append.as_deref(),
                    tools_delta_xml: route_meta.tools_delta_xml.as_deref(),
                    outbound_acp_blocks: &initial_acp_blocks,
                    messages: pre_reply_slice,
                    client_tools: &req.client_tools,
                };
                task_guard.complete_with_diagnostics(
                    complete_stop_reason,
                    cumulative_usage,
                    Some(diag),
                );
            }
            full_messages.push(assistant_msg);
            let message_count = full_messages.len();
            let retained_messages = if req.retain_response_history {
                full_messages
            } else {
                Vec::new()
            };
            let committed_context_used = if state.turn_peak_context_used > 0 {
                state.latest_context_used
            } else {
                context_usage.total_tokens
            };
            let committed_turn = CommittedSessionTurn {
                post_turn_hash: post_hash,
                alt_post_turn_hash: None,
                post_turn_dialogue_hash: dialogue_hash,
                declared_tools: req.client_tools,
                last_context_used: committed_context_used,
                turn_prompt_tokens: cumulative_usage.total_prompt_tokens(),
                turn_cached_read_tokens: cumulative_usage.cached_read_tokens,
                message_count,
                last_response_id: Some(response_id),
                accumulated_messages: retained_messages,
            };
            if let Some(pending_fut) = deferred_soft_cancel_drain.take() {
                let pending_guard =
                    final_lease.register_pending_checkin(vec![post_hash, dialogue_hash]);
                tokio::spawn(async move {
                    let _pending = pending_guard;
                    match timeout(Duration::from_secs(5), pending_fut).await {
                        Ok(Ok(Ok(_))) => {
                            final_lease.commit(committed_turn);
                        }
                        _ => {
                            final_lease.finish_ephemeral();
                        }
                    }
                });
            } else {
                final_lease.commit(committed_turn);
            }
        }
    } else {
        task_guard.complete(complete_stop_reason, cumulative_usage);
    }

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
                let thought_chars = content.text.chars().count();
                state.step_thought_chars = state.step_thought_chars.saturating_add(thought_chars);
                task_guard.record_thought(thought_chars);
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
            if name.starts_with(ACP_MCP_TOOL_PREFIX) {
                task_guard.mark_calling_client_tool();
                return true;
            }
            if state.emitted_client_tool || !state.pending_web_fetches.is_empty() {
                tracing::info!(
                    acp_tool = %name,
                    acp_kind = %kind,
                    "short-circuiting post-tool native ACP tool call because XML tool call was already captured"
                );
                state.tainted_session = true;
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
            if name.starts_with(ACP_MCP_TOOL_PREFIX) {
                return true;
            }
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
            state.step_text_chars = state
                .step_text_chars
                .saturating_add(content.text.chars().count());
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
        | GatewayError::PayloadTooLarge { .. }
        | GatewayError::NotFound { .. }
        | GatewayError::InvalidBody { .. }
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
        | AcpError::CorruptedToolCallStream { .. }
        | AcpError::UnclosedToolCallStream { .. }
        | AcpError::EmptyUpstreamResponse { .. }
        | AcpError::Cancelled
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
                        if contains_runaway_marker(&state.post_tool_spill_buf)
                            || is_malformed_function_call_banner(&state.post_tool_spill_buf)
                            || classify_upstream_error_text(&state.post_tool_spill_buf).is_some()
                        {
                            state.tainted_session = true;
                        }
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
                    state.tainted_session = true;
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
        let is_apply_patch = matched_spec.is_some_and(|s| s.name == "apply_patch");
        if !is_raw_input
            && let Ok(serde_json::Value::Object(map)) = serde_json::from_str(&arguments)
        {
            for key in ["input", "patch", "diff"] {
                if let Some(serde_json::Value::String(inner)) = map.get(key) {
                    return if is_apply_patch {
                        liftgate_types::parser::normalize_apply_patch_text(inner)
                    } else {
                        inner.clone()
                    };
                }
            }
        }
        return if is_apply_patch {
            liftgate_types::parser::normalize_apply_patch_text(&arguments)
        } else {
            arguments
        };
    }

    if !is_raw_input {
        let trimmed = arguments.trim();
        if trimmed.is_empty() || trimmed == "null" {
            return "{}".to_owned();
        }
        if let Ok(serde_json::Value::Object(mut map)) =
            serde_json::from_str::<serde_json::Value>(trimmed)
        {
            if let Some(spec) = matched_spec
                && let CanonicalToolKind::Function { parameters_json } = &spec.kind
                && coerce_stringified_container_fields(&mut map, parameters_json)
                && let Ok(reserialized) = serde_json::to_string(&serde_json::Value::Object(map))
            {
                return reserialized;
            }
            return arguments;
        }
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

fn coerce_stringified_container_fields(
    map: &mut serde_json::Map<String, serde_json::Value>,
    parameters_json: &str,
) -> bool {
    liftgate_tool_virtualizer::coerce_stringified_container_fields(map, parameters_json)
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

const MAX_SINGLE_MCP_TOOL_RESULT_BYTES: usize = 96 * 1024;

type UnpinnedPromptFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<PromptTurnResult, AcpError>> + Send>>;

fn can_satisfy_pending_mcp_calls(
    pending_calls: &[PendingMcpCallInfo],
    delta_messages: &[CanonicalMessage],
) -> bool {
    if pending_calls.is_empty() {
        return false;
    }
    if delta_messages
        .iter()
        .any(|m| m.role == CanonicalRole::Assistant)
    {
        return false;
    }
    let mut provided_suffixes: HashSet<&str> = HashSet::with_capacity(pending_calls.len());
    for msg in delta_messages {
        for part in &msg.parts {
            if let CanonicalPart::ToolResult { call_id, .. } = part {
                let norm = strip_call_prefix(call_id);
                if !norm.is_empty() {
                    provided_suffixes.insert(norm);
                }
            }
        }
    }
    pending_calls
        .iter()
        .all(|p| provided_suffixes.contains(p.nonce_suffix.as_str()))
}

fn abort_suspended_checkout<B: AcpBackend>(backend: &Arc<B>, checkout: SuspendedCheckoutGuard<B>) {
    let entry = checkout.take_for_unwind();
    let sid = entry.session_id.clone();
    drop(entry);
    backend.abort_session_nonblocking(&sid);
}

async fn unwind_suspended_to_idle_entry<B: AcpBackend>(
    backend: &Arc<B>,
    checkout: SuspendedCheckoutGuard<B>,
    deadline: Instant,
) -> Option<IdleSessionEntry> {
    let mut entry = checkout.take_for_unwind();
    let sid = entry.session_id.clone();
    if !backend.cancel_prompt_for_reuse(&sid) {
        drop(entry);
        backend.abort_session_nonblocking(&sid);
        return None;
    }

    let (done_tx, done_rx) = oneshot::channel();
    if entry
        .cmd_tx
        .send(McpDriverCommand::CancelForUnwind { done_tx })
        .is_err()
    {
        backend.abort_session_nonblocking(&sid);
        return None;
    }

    let unwind_deadline = deadline.min(Instant::now() + Duration::from_secs(5));
    match timeout_at(unwind_deadline, done_rx).await {
        Ok(Ok(Ok((unwound_ctx, unwound_delta, mcp_handle)))) => {
            let now_ms = unix_timestamp_ms();
            let accumulated_messages = std::mem::take(&mut entry.accumulated_messages);
            Some(IdleSessionEntry {
                session_id: sid,
                tool_nonce: entry.tool_nonce,
                model_id: entry.model_id,
                mcp_handle: Some(mcp_handle),
                post_turn_hash: entry.post_turn_hash,
                alt_post_turn_hash: entry.alt_pre_user_hash,
                post_turn_dialogue_hash: entry.post_turn_dialogue_hash,
                declared_tools: entry.declared_tools,
                last_context_used: if unwound_ctx > 0 {
                    unwound_ctx
                } else {
                    entry.last_context_used
                },
                total_prompt_tokens: entry
                    .total_prompt_tokens
                    .saturating_add(unwound_delta.total_prompt_tokens()),
                total_cached_read_tokens: entry
                    .total_cached_read_tokens
                    .saturating_add(unwound_delta.cached_read_tokens),
                last_response_id: entry.last_response_id,
                accumulated_messages,
                message_count: entry.message_count,
                turn_count: entry.turn_count,
                created_at_unix_ms: entry.created_at_unix_ms,
                last_active_unix_ms: now_ms,
                retained_bytes: 0,
            })
        }
        _ => {
            backend.abort_session_nonblocking(&sid);
            None
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal helper wires promoted MCP continuation step without extra heap indirection"
)]
fn spawn_resumed_mcp_continuation<B: AcpBackend>(
    backend: Arc<B>,
    mut promoted: PromotedMcpSession,
    step_lease: SessionLeaseGuard<B>,
    vision_offload_enabled: bool,
    task_guard: ActiveTaskGuard,
    mut delta_for_reply: Vec<CanonicalMessage>,
    delta_start_index: usize,
    req: NormalizedTurnRequest,
    web_cfg: WebToolBridgeConfig,
    resolved_model: ModelId,
    acp_mode: AcpModeId,
    response_id: String,
    deadline: Instant,
    timeout_secs: u64,
    channel_cap: usize,
    mut spill_store: Option<SpillStore>,
) -> ActiveTurnStream {
    let use_virtualized_delta = step_lease.mcp_handle().is_some_and(|handle| {
        handle.sync_client_tools(&req.client_tools);
        handle.has_virtualized_dispatch()
    });
    let tools_delta = if use_virtualized_delta {
        build_mcp_virtualized_tools_delta_xml_with_spill(
            &promoted.declared_tools,
            &req.client_tools,
            step_lease.tool_nonce(),
            &mut spill_store,
        )
    } else {
        build_client_tools_delta_xml_with_spill(
            &promoted.declared_tools,
            &req.client_tools,
            step_lease.tool_nonce(),
            &mut spill_store,
        )
    };
    let full_messages = std::mem::take(&mut promoted.accumulated_messages);
    let base_state_hash = hash_conversation_state(
        req.system_instructions.as_deref(),
        &req.client_tools,
        web_cfg,
        &resolved_model,
        &full_messages,
    );
    let dialogue_hash = hash_dialogue_state(&resolved_model, &full_messages);
    if req.auxiliary_task.is_none() {
        step_lease.set_in_flight_hashes(vec![base_state_hash, dialogue_hash]);
    }
    task_guard.bind_session(
        step_lease.session_id().as_str(),
        true,
        resolved_model.as_str(),
    );

    let route_meta = TurnRoutingMetadata {
        routing_strategy: LowCacheRoutingStrategy::McpSuspendedContinuation,
        delta_start_index,
        session_turn_index: step_lease.turn_count(),
        session_idle_ms: step_lease.idle_before_checkout_ms(),
        previous_session_context_tokens: promoted.last_context_used,
        model_switched: false,
        previous_model: None,
        prefix_hash_hex: Some(format_conversation_hash_hex(&promoted.post_turn_hash)),
        base_state_hash_hex: format_conversation_hash_hex(&base_state_hash),
        acp_system_append: None,
        tools_delta_xml: tools_delta.clone(),
    };

    let turn_cancel = CancellationToken::new();
    let cancel_guard = turn_cancel.clone().drop_guard();
    let (events_tx, events_rx) = mpsc::channel::<TurnEvent>(channel_cap);
    let worker_resp_id = response_id.clone();
    let worker_model = resolved_model.clone();

    let worker = tokio::spawn(async move {
        let mut vision_offload_usage = AcpUsage::default();
        let has_delta_images = delta_for_reply.iter().any(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, CanonicalPart::ImageBase64 { .. }))
        });
        if vision_offload_enabled
            && promoted.last_context_used >= LONG_CONTEXT_VISION_OFFLOAD_THRESHOLD_TOKENS
            && has_delta_images
        {
            let vision_blocks = delta_messages_to_vision_blocks(&delta_for_reply);
            match offload_images_via_ephemeral_session(
                &backend,
                &vision_blocks,
                &worker_model,
                &acp_mode,
                deadline,
                channel_cap,
                &mut spill_store,
                &turn_cancel,
            )
            .await
            {
                VisionOffloadOutcome::Replaced {
                    text_blocks,
                    usage,
                    image_count,
                } => {
                    vision_offload_usage = usage;
                    task_guard.record_vision_offload(image_count);
                    replace_delta_images_with_vision_text(&mut delta_for_reply, &text_blocks);
                }
                VisionOffloadOutcome::FallbackOriginal => {}
                VisionOffloadOutcome::RateLimited(err) => {
                    step_lease.finish_ephemeral();
                    task_guard.fail(format_error_chain(&err));
                    let _ = events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
                    return;
                }
                VisionOffloadOutcome::Cancelled => return,
            }
        }

        let (tool_replies, diag_blocks) = build_mcp_tool_replies(
            &promoted.pending_calls,
            &delta_for_reply,
            tools_delta.as_deref(),
            &mut spill_store,
        );
        let sid = step_lease.session_id().clone();
        let raw_lease = step_lease.into_raw_parts();
        let resumed = McpResumedStepInput {
            raw_lease,
            task_guard,
            req,
            web_cfg,
            full_messages,
            route_meta,
            outbound_diag_blocks: diag_blocks,
            response_id: worker_resp_id,
            tool_replies,
            vision_offload_usage,
            events_tx,
            step_cancel: turn_cancel,
            spill_store,
            retire_after_turn: false,
            timeout_secs,
        };
        if let Err(McpDriverCommand::ResumeStep(failed_box)) = promoted
            .cmd_tx
            .send(McpDriverCommand::ResumeStep(Box::new(resumed)))
        {
            drop(failed_box.raw_lease);
            backend.abort_session_nonblocking(&sid);
            let err = AcpError::TransportClosed;
            failed_box.task_guard.fail(format_error_chain(&err));
            let _ = failed_box
                .events_tx
                .send(TurnEvent::Failed(Arc::new(err)))
                .await;
        }
    });

    ActiveTurnStream {
        resolved_model,
        response_id,
        rx: events_rx,
        _cancel_guard: cancel_guard,
        _worker: worker,
    }
}

fn delta_messages_to_vision_blocks(delta: &[CanonicalMessage]) -> Vec<AcpPromptBlock> {
    let mut blocks = Vec::new();
    for msg in delta {
        for part in &msg.parts {
            match part {
                CanonicalPart::Text(t) if !t.trim().is_empty() => {
                    blocks.push(AcpPromptBlock::Text { text: t.clone() });
                }
                CanonicalPart::ToolResult { output, .. } if !output.trim().is_empty() => {
                    blocks.push(AcpPromptBlock::Text {
                        text: output.clone(),
                    });
                }
                CanonicalPart::ImageBase64 { media_type, data } => {
                    blocks.push(AcpPromptBlock::Image {
                        data: data.clone(),
                        mime_type: media_type.clone(),
                    });
                }
                _ => {}
            }
        }
    }
    blocks
}

fn replace_delta_images_with_vision_text(
    delta: &mut Vec<CanonicalMessage>,
    replacement_blocks: &[AcpPromptBlock],
) {
    for msg in &mut *delta {
        msg.parts
            .retain(|part| !matches!(part, CanonicalPart::ImageBase64 { .. }));
    }
    let mut report_only = String::new();
    for block in replacement_blocks {
        if let AcpPromptBlock::Text { text } = block
            && let Some(pos) = text.find("<ephemeral_vision_inspection")
            && let Some(tail) = text.get(pos..)
        {
            if !report_only.is_empty() {
                report_only.push('\n');
            }
            report_only.push_str(tail.trim());
        }
    }
    if !report_only.is_empty() {
        delta.push(CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::Text(report_only)],
        });
    }
}

fn is_developer_instructions_block(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with("<developer_instructions>")
        && trimmed.ends_with("</developer_instructions>")
}

fn sanitize_mcp_tool_result_text(
    raw_output: &str,
    client_tool_name: &str,
    spill_store: &mut Option<SpillStore>,
) -> String {
    let billing_clean = strip_billing_noise(raw_output);
    let term_clean = scrub_terminal_control_noise(&billing_clean);
    if term_clean.len() > MAX_SINGLE_MCP_TOOL_RESULT_BYTES {
        let label = format!("tool_result:{client_tool_name}");
        truncate_head_tail_lines_with_spill(
            &term_clean,
            MAX_SINGLE_MCP_TOOL_RESULT_BYTES,
            &label,
            spill_store,
        )
    } else {
        term_clean.into_owned()
    }
}

fn build_mcp_tool_replies(
    pending_calls: &[PendingMcpCallInfo],
    delta_messages: &[CanonicalMessage],
    tools_delta_xml: Option<&str>,
    spill_store: &mut Option<SpillStore>,
) -> (Vec<(String, McpToolCallReply)>, Arc<[AcpPromptBlock]>) {
    let pending_set: HashSet<&str> = pending_calls
        .iter()
        .map(|p| p.nonce_suffix.as_str())
        .collect();

    let mut matched_results: HashMap<String, (String, bool)> =
        HashMap::with_capacity(pending_calls.len());
    let mut extra_xml_results: Vec<(String, String, bool)> = Vec::new();
    let mut followup_texts: Vec<String> = Vec::new();
    let mut followup_images: Vec<(String, String)> = Vec::new();

    for msg in delta_messages {
        for part in &msg.parts {
            match part {
                CanonicalPart::ToolResult {
                    call_id,
                    output,
                    is_error,
                } => {
                    let norm = strip_call_prefix(call_id);
                    if pending_set.contains(norm) {
                        let slot = matched_results
                            .entry(norm.to_owned())
                            .or_insert_with(|| (String::new(), false));
                        if !slot.0.is_empty() && !output.is_empty() {
                            slot.0.push('\n');
                        }
                        slot.0.push_str(output);
                        slot.1 |= *is_error;
                    } else {
                        extra_xml_results.push((call_id.clone(), output.clone(), *is_error));
                    }
                }
                CanonicalPart::Text(text) => {
                    if is_developer_instructions_block(text) {
                        continue;
                    }
                    let cleaned = strip_billing_noise(text);
                    let trimmed = cleaned.trim();
                    if !trimmed.is_empty() {
                        followup_texts.push(trimmed.to_owned());
                    }
                }
                CanonicalPart::ImageBase64 { media_type, data } => {
                    followup_images.push((media_type.clone(), data.clone()));
                }
                CanonicalPart::Reasoning(_) | CanonicalPart::ToolCall { .. } => {}
            }
        }
    }

    let total_pending = pending_calls.len();
    let mut replies = Vec::with_capacity(total_pending);
    let mut diag_summary = String::new();

    for (idx, pending) in pending_calls.iter().enumerate() {
        let (raw_out, is_error) = matched_results
            .remove(&pending.nonce_suffix)
            .unwrap_or_else(|| (String::new(), false));
        let sanitized = sanitize_mcp_tool_result_text(&raw_out, &pending.client_name, spill_store);
        if !diag_summary.is_empty() {
            diag_summary.push('\n');
        }
        diag_summary.push_str(&format!(
            "[mcp_tool_result name=\"{}\" id=\"{}\" is_error={}]\n{}",
            pending.client_name, pending.call_id, is_error, sanitized
        ));

        let mut content = vec![McpContentItem::Text { text: sanitized }];

        if idx + 1 == total_pending {
            for (xml_id, xml_out, xml_err) in &extra_xml_results {
                let clean_xml = sanitize_mcp_tool_result_text(xml_out, "xml_tool", spill_store);
                content.push(McpContentItem::Text {
                    text: format!(
                        "<client_tool_result tool_call_id=\"{xml_id}\" is_error=\"{xml_err}\">\n{clean_xml}\n</client_tool_result>"
                    ),
                });
            }
            if let Some(td_xml) = tools_delta_xml
                && !td_xml.trim().is_empty()
            {
                content.push(McpContentItem::Text {
                    text: td_xml.to_owned(),
                });
            }
            if !followup_texts.is_empty() {
                let joined = followup_texts.join("\n\n");
                content.push(McpContentItem::Text {
                    text: format!("<user_followup_message>\n{joined}\n</user_followup_message>"),
                });
            }
            for (mime_type, data) in &followup_images {
                content.push(McpContentItem::Image {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                });
            }
        }

        replies.push((
            pending.nonce_suffix.clone(),
            McpToolCallReply { content, is_error },
        ));
    }

    let diag_blocks: Arc<[AcpPromptBlock]> =
        Arc::from(vec![AcpPromptBlock::Text { text: diag_summary }]);
    (replies, diag_blocks)
}

const fn estimate_tokens_from_chars(chars: usize) -> u64 {
    (chars.saturating_add(3) / 4) as u64
}

fn estimate_step_mcp_usage(
    u_k: u64,
    is_cold_first_step: bool,
    prev_prompt_tokens: u64,
    step_thought_chars: usize,
    step_text_chars: usize,
    step_tool_arg_chars: usize,
) -> AcpUsage {
    let total_out_chars = step_thought_chars
        .saturating_add(step_text_chars)
        .saturating_add(step_tool_arg_chars);
    let est_out = estimate_tokens_from_chars(total_out_chars).max(1).min(u_k);
    let est_thought = estimate_tokens_from_chars(step_thought_chars).min(est_out);
    let p_k = u_k.saturating_sub(est_out);
    let cached_read = if is_cold_first_step {
        0
    } else if prev_prompt_tokens >= 4_096 {
        prev_prompt_tokens.saturating_sub(2_300).min(p_k)
    } else {
        0
    };
    let input = p_k.saturating_sub(cached_read);
    AcpUsage {
        input_tokens: input,
        output_tokens: est_out,
        cached_read_tokens: cached_read,
        cached_write_tokens: 0,
        thought_tokens: est_thought,
        total_tokens: u_k,
    }
}

fn compute_final_mcp_step_usage(
    authoritative_sum: AcpUsage,
    accounted_prior: AcpUsage,
    u_m: u64,
    fallback_context_used: u64,
) -> AcpUsage {
    let rem = authoritative_sum.saturating_sub(accounted_prior);
    if u_m > 0 {
        let o_m = rem.output_tokens.min(u_m);
        let th_m = rem.thought_tokens.min(o_m);
        let p_m = u_m.saturating_sub(o_m);
        let cr_m = rem.cached_read_tokens.min(p_m);
        let cw_m = rem.cached_write_tokens.min(p_m.saturating_sub(cr_m));
        let in_m = p_m.saturating_sub(cr_m).saturating_sub(cw_m);
        AcpUsage {
            input_tokens: in_m,
            output_tokens: o_m,
            cached_read_tokens: cr_m,
            cached_write_tokens: cw_m,
            thought_tokens: th_m,
            total_tokens: u_m,
        }
    } else {
        let prompt_sum = rem.total_prompt_tokens();
        AcpUsage {
            input_tokens: rem.input_tokens,
            output_tokens: rem.output_tokens,
            cached_read_tokens: rem.cached_read_tokens,
            cached_write_tokens: rem.cached_write_tokens,
            thought_tokens: rem.thought_tokens,
            total_tokens: prompt_sum
                .saturating_add(rem.output_tokens)
                .max(fallback_context_used),
        }
    }
}

const MAX_CONSECUTIVE_DISABLED_MCP_REJECTIONS: u8 = 3;
const MAX_CONSECUTIVE_PREFLIGHT_MCP_REJECTIONS: u8 = 4;

struct ForwardedMcpCall {
    client_name: String,
    arguments: String,
    is_custom: bool,
    reply_tx: oneshot::Sender<McpToolCallReply>,
}

fn record_forwarded_mcp_call(
    forwarded: ForwardedMcpCall,
    task_guard: &ActiveTaskGuard,
    state: &mut TurnStreamState,
) -> (
    PendingMcpCallInfo,
    oneshot::Sender<McpToolCallReply>,
    TurnEvent,
    usize,
) {
    let ForwardedMcpCall {
        client_name,
        arguments,
        is_custom,
        reply_tx,
    } = forwarded;
    let arg_chars = arguments.chars().count();
    let nonce_suffix = generate_short_nonce();
    let call_id = format!("call_{nonce_suffix}");

    task_guard.record_client_tool(&client_name);
    task_guard.record_tool_delta(arg_chars);
    state.emitted_client_tool = true;
    state.assistant_parts.push(CanonicalPart::ToolCall {
        call_id: call_id.clone(),
        name: client_name.clone(),
        arguments: arguments.clone(),
        is_custom,
    });

    let info = PendingMcpCallInfo {
        nonce_suffix,
        call_id: call_id.clone(),
        client_name: client_name.clone(),
    };
    let ev = TurnEvent::ClientToolCall {
        call_id,
        name: client_name,
        arguments,
        is_custom,
    };
    (info, reply_tx, ev, arg_chars)
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal MCP session driver owns explicit moved state across multi-step tool suspensions"
)]
async fn run_mcp_session_driver<B: AcpBackend>(
    backend: Arc<B>,
    pool: Weak<SessionPool>,
    initial_lease: SessionLeaseGuard<B>,
    initial_session_reused: bool,
    vision_offload_enabled: bool,
    initial_task_guard: ActiveTaskGuard,
    initial_prompt_blocks: Vec<AcpPromptBlock>,
    initial_full_messages: Vec<CanonicalMessage>,
    initial_route_meta: TurnRoutingMetadata,
    initial_req: NormalizedTurnRequest,
    initial_web_cfg: WebToolBridgeConfig,
    active_model: ModelId,
    acp_mode: AcpModeId,
    initial_retire_after_turn: bool,
    initial_response_id: String,
    initial_timeout_secs: u64,
    channel_cap: usize,
    mut cur_spill_store: Option<SpillStore>,
    initial_events_tx: mpsc::Sender<TurnEvent>,
    initial_cancel: CancellationToken,
) {
    let Some(mcp_handle) = initial_lease.mcp_handle().cloned() else {
        return;
    };
    let session_id = initial_lease.session_id().clone();
    let active_tool_nonce = initial_lease.tool_nonce().to_owned();
    let initial_context_used = initial_lease.last_context_used();

    let mut call_rx_guard = mcp_handle.call_rx().lock().await;
    while call_rx_guard.try_recv().is_ok() {}

    let mut cur_lease = initial_lease;
    let mut cur_task_guard = initial_task_guard;
    let mut cur_req = initial_req;
    let mut cur_web_cfg = initial_web_cfg;
    let mut cur_full_messages = initial_full_messages;
    let mut cur_route_meta = initial_route_meta;
    let mut cur_response_id = initial_response_id;
    let mut cur_events_tx = initial_events_tx;
    let mut cur_cancel = initial_cancel;
    let mut cur_retire_after_turn = initial_retire_after_turn;
    let mut cur_timeout_secs = initial_timeout_secs;
    let mut cur_vision_usage = AcpUsage::default();
    let mut current_prompt_blocks = initial_prompt_blocks;

    if vision_offload_enabled
        && initial_session_reused
        && initial_context_used >= LONG_CONTEXT_VISION_OFFLOAD_THRESHOLD_TOKENS
        && current_prompt_blocks
            .iter()
            .any(|b| matches!(b, AcpPromptBlock::Image { .. }))
    {
        let vision_deadline = Instant::now() + Duration::from_secs(cur_timeout_secs);
        match offload_images_via_ephemeral_session(
            &backend,
            &current_prompt_blocks,
            &active_model,
            &acp_mode,
            vision_deadline,
            channel_cap,
            &mut cur_spill_store,
            &cur_cancel,
        )
        .await
        {
            VisionOffloadOutcome::Replaced {
                text_blocks,
                usage,
                image_count,
            } => {
                cur_vision_usage = usage;
                cur_task_guard.record_vision_offload(image_count);
                current_prompt_blocks = text_blocks;
            }
            VisionOffloadOutcome::FallbackOriginal => {}
            VisionOffloadOutcome::RateLimited(err) => {
                cur_lease.finish_ephemeral();
                cur_task_guard.fail(format_error_chain(&err));
                let _ = cur_events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
                return;
            }
            VisionOffloadOutcome::Cancelled => return,
        }
    }

    let mut cur_outbound_blocks: Arc<[AcpPromptBlock]> = Arc::from(current_prompt_blocks);
    let mut next_prompt_blocks: Option<Arc<[AcpPromptBlock]>> =
        Some(Arc::clone(&cur_outbound_blocks));
    let mut completed_web_fetches: usize = 0;

    'prompt_loop: loop {
        let mut prev_prompt_tokens: u64 = cur_lease.last_context_used();
        let mut llm_step_idx: u32 = 0;
        let mut accounted_usage = AcpUsage::default();
        let mut peak_context_used: u64 = cur_lease.last_context_used();
        let mut latest_context_used: u64 = cur_lease.last_context_used();

        let (acp_tx, mut acp_rx) = mpsc::channel::<SessionUpdatePayload>(channel_cap);
        let mut acp_closed = false;

        let prompt_backend = Arc::clone(&backend);
        let prompt_sid = session_id.clone();
        let rpc_blocks = next_prompt_blocks
            .take()
            .unwrap_or_else(|| Arc::clone(&cur_outbound_blocks));
        let mut prompt_fut: UnpinnedPromptFuture = Box::pin(async move {
            prompt_backend
                .start_prompt(&prompt_sid, &rpc_blocks, acp_tx)
                .await
        });

        loop {
            let step_started_ms = unix_timestamp_ms();
            let step_deadline = Instant::now() + Duration::from_secs(cur_timeout_secs);
            let has_xml_tools = !cur_req.client_tools.is_empty() || cur_req.enable_native_web_fetch;
            let mut parser = IncrementalToolParser::new(&active_tool_nonce, has_xml_tools);
            let mut banner_filter = if has_xml_tools {
                AcpBannerFilter::with_tool_nonce(&active_tool_nonce)
            } else {
                AcpBannerFilter::new()
            };
            let mut filter_out = String::new();
            let mut state = TurnStreamState::new(latest_context_used);
            state.peak_context_used = peak_context_used;

            let mut suppressed_banners: Vec<String> = Vec::new();
            let mut disabled_mcp_rejections: u8 = 0;
            let mut preflight_mcp_rejections: u8 = 0;
            let mut pending_mcp_incoming: Vec<IncomingMcpToolCall> = Vec::new();
            let mut ready_forwarded_calls: Vec<ForwardedMcpCall> = Vec::new();
            let mut prompt_outcome: Option<Result<PromptTurnResult, AcpError>> = None;

            'substep_loop: loop {
                pending_mcp_incoming.clear();
                ready_forwarded_calls.clear();
                let mut upstream_abort: Option<(UpstreamBannerSignal, String)> = None;
                let mut step_timed_out = false;

                loop {
                    tokio::select! {
                        biased;
                        () = cur_cancel.cancelled() => {
                            while call_rx_guard.try_recv().is_ok() {}
                            let can_recycle_warm = cur_req.auxiliary_task.is_none()
                                && !cur_retire_after_turn
                                && !state.tainted_session
                                && completed_web_fetches == 0
                                && backend.cancel_prompt_for_reuse(&session_id);
                            if can_recycle_warm {
                                let telemetry_hub = Arc::clone(cur_task_guard.hub());
                                drop(cur_task_guard);
                                let post_cancel_hash = hash_conversation_state(
                                    cur_req.system_instructions.as_deref(),
                                    &cur_req.client_tools,
                                    cur_web_cfg,
                                    cur_lease.model_id(),
                                    &cur_full_messages,
                                );
                                let alt_pre_cancel_hash = (cur_route_meta.delta_start_index > 0)
                                    .then(|| cur_full_messages.get(..cur_route_meta.delta_start_index))
                                    .flatten()
                                    .map(|prefix| {
                                        hash_conversation_state(
                                            cur_req.system_instructions.as_deref(),
                                            &cur_req.client_tools,
                                            cur_web_cfg,
                                            cur_lease.model_id(),
                                            prefix,
                                        )
                                    });
                                let dialogue_hash =
                                    hash_dialogue_state(cur_lease.model_id(), &cur_full_messages);
                                let mut pending_hashes = vec![post_cancel_hash, dialogue_hash];
                                if let Some(alt_h) = alt_pre_cancel_hash {
                                    pending_hashes.push(alt_h);
                                }
                                let pending_guard =
                                    cur_lease.register_pending_checkin(pending_hashes);
                                let message_count = cur_full_messages.len();
                                let retained_messages = if cur_req.retain_response_history {
                                    cur_full_messages
                                } else {
                                    Vec::new()
                                };
                                let declared_tools = cur_req.client_tools;
                                let step_bytes: usize = cur_outbound_blocks
                                    .iter()
                                    .map(|b| match b {
                                        AcpPromptBlock::Text { text } => text.len(),
                                        AcpPromptBlock::Image { .. } => 8_192,
                                    })
                                    .sum();
                                let approx_ctx = if state.turn_peak_context_used > 0 {
                                    state.latest_context_used
                                } else {
                                    latest_context_used.saturating_add(
                                        u64::try_from(step_bytes / 4).unwrap_or(0),
                                    )
                                };
                                tokio::spawn(async move {
                                    let _pending = pending_guard;
                                    match timeout(Duration::from_secs(5), prompt_fut).await {
                                        Ok(Ok(res)) => {
                                            if llm_step_idx > 0 {
                                                telemetry_hub.reconcile_cascade_usage_drift(
                                                    accounted_usage,
                                                    res.usage,
                                                    initial_session_reused,
                                                );
                                            }
                                            cur_lease.reconcile_prior_cascade_tokens(
                                                accounted_usage.total_prompt_tokens(),
                                                accounted_usage.cached_read_tokens,
                                            );
                                            let final_ctx = if res.usage.total_tokens > 0 {
                                                res.usage.total_tokens
                                            } else {
                                                approx_ctx
                                            };
                                            cur_lease.commit(CommittedSessionTurn {
                                                post_turn_hash: post_cancel_hash,
                                                alt_post_turn_hash: alt_pre_cancel_hash,
                                                post_turn_dialogue_hash: dialogue_hash,
                                                declared_tools,
                                                last_context_used: final_ctx,
                                                turn_prompt_tokens: res.usage.total_prompt_tokens(),
                                                turn_cached_read_tokens: res.usage.cached_read_tokens,
                                                message_count,
                                                last_response_id: None,
                                                accumulated_messages: retained_messages,
                                            });
                                        }
                                        _ => {
                                            cur_lease.finish_ephemeral();
                                        }
                                    }
                                });
                            } else {
                                cur_lease.finish_ephemeral();
                            }
                            return;
                        }
                        () = tokio::time::sleep_until(step_deadline) => {
                            step_timed_out = true;
                            break;
                        }
                        maybe_call = call_rx_guard.recv() => {
                            if let Some(first_call) = maybe_call {
                                pending_mcp_incoming.push(first_call);
                                tokio::task::yield_now().await;
                                tokio::task::yield_now().await;
                                while let Ok(extra) = call_rx_guard.try_recv() {
                                    pending_mcp_incoming.push(extra);
                                }
                                break;
                            }
                        }
                        maybe_upd = acp_rx.recv(), if !acp_closed => {
                            match maybe_upd {
                                Some(upd) => {
                                    if !forward_acp_update(
                                        upd,
                                        &cur_req,
                                        &cur_task_guard,
                                        &mut banner_filter,
                                        &mut filter_out,
                                        &mut parser,
                                        &mut state,
                                        &cur_events_tx,
                                        &cur_cancel,
                                    )
                                    .await
                                    {
                                        cur_lease.finish_ephemeral();
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
                                        if state.emitted_client_tool
                                            || !state.pending_web_fetches.is_empty()
                                        {
                                            state.tainted_session = true;
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
                                        state.tainted_session = true;
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
                                        backend.cancel_prompt_nonblocking(&session_id);
                                        prompt_outcome = match timeout(
                                            Duration::from_millis(500),
                                            &mut prompt_fut,
                                        )
                                        .await
                                        {
                                            Ok(Ok(mut res)) => {
                                                res.stop_reason = if state.emitted_client_tool {
                                                    "tool_use".to_owned()
                                                } else {
                                                    "end_turn".to_owned()
                                                };
                                                Some(Ok(res))
                                            }
                                            _ => {
                                                state.tainted_session = true;
                                                Some(Ok(PromptTurnResult {
                                                    stop_reason: if state.emitted_client_tool {
                                                        "tool_use".to_owned()
                                                    } else {
                                                        "end_turn".to_owned()
                                                    },
                                                    usage: AcpUsage::default(),
                                                    error: None,
                                                }))
                                            }
                                        };
                                        break;
                                    }
                                }
                                None => {
                                    acp_closed = true;
                                    if prompt_outcome.is_some() {
                                        break;
                                    }
                                }
                            }
                        }
                        res = &mut prompt_fut, if prompt_outcome.is_none() => {
                            prompt_outcome = Some(res);
                            break;
                        }
                    }
                }

                if !step_timed_out && upstream_abort.is_none() && !state.halted_post_tool_runaway {
                    while let Ok(upd) = acp_rx.try_recv() {
                        if !forward_acp_update(
                            upd,
                            &cur_req,
                            &cur_task_guard,
                            &mut banner_filter,
                            &mut filter_out,
                            &mut parser,
                            &mut state,
                            &cur_events_tx,
                            &cur_cancel,
                        )
                        .await
                        {
                            cur_lease.finish_ephemeral();
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
                                state.tainted_session = true;
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
                            state.tainted_session = true;
                            state.halted_post_tool_runaway = true;
                        }
                        if state.halted_post_tool_runaway {
                            break;
                        }
                    }
                }

                if !step_timed_out && upstream_abort.is_none() && !state.halted_post_tool_runaway {
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
                suppressed_banners.extend(banner_filter.take_suppressed_banners());

                if step_timed_out {
                    cur_lease.finish_ephemeral();
                    let err = AcpError::TurnTimeout {
                        seconds: cur_timeout_secs,
                    };
                    cur_task_guard.fail(format_error_chain(&err));
                    let _ = cur_events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
                    return;
                }

                if let Some((sig, detail)) = upstream_abort {
                    cur_lease.finish_ephemeral();
                    let err = signal_to_acp_error(sig, &active_model, detail);
                    cur_task_guard.fail(format_error_chain(&err));
                    let _ = cur_events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
                    return;
                }

                if !state.halted_post_tool_runaway {
                    if !filter_out.is_empty() && state.truncated_tool_abort.is_none() {
                        state.parsed_buf.clear();
                        parser.feed(&filter_out, &mut state.parsed_buf);
                        if !emit_parsed_items(
                            &cur_req,
                            &cur_task_guard,
                            &mut state,
                            &cur_events_tx,
                            &cur_cancel,
                        )
                        .await
                        {
                            cur_lease.finish_ephemeral();
                            return;
                        }
                    }
                    if state.truncated_tool_abort.is_none() {
                        state.parsed_buf.clear();
                        parser.finish(&mut state.parsed_buf);
                        if !emit_parsed_items(
                            &cur_req,
                            &cur_task_guard,
                            &mut state,
                            &cur_events_tx,
                            &cur_cancel,
                        )
                        .await
                        {
                            cur_lease.finish_ephemeral();
                            return;
                        }
                    }
                }

                if let Some(aborted_tool) = state.truncated_tool_abort.take() {
                    cur_lease.finish_ephemeral();
                    let err = AcpError::CorruptedToolCallStream { tool: aborted_tool };
                    cur_task_guard.fail(format_error_chain(&err));
                    let _ = cur_events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
                    return;
                }

                peak_context_used = peak_context_used.max(state.peak_context_used);
                if state.turn_peak_context_used > 0 {
                    latest_context_used = state.latest_context_used;
                }

                if !pending_mcp_incoming.is_empty() && prompt_outcome.is_none() {
                    mcp_handle.sync_client_tools(&cur_req.client_tools);
                    let mut exceeded_rejection_reason: Option<&'static str> = None;
                    for incoming in pending_mcp_incoming.drain(..) {
                        match mcp_handle.resolve_incoming_call(incoming, &cur_req.client_tools) {
                            ResolvedMcpIncoming::ImmediateLocalReply {
                                reply_tx,
                                reply,
                                kind,
                            } => match kind {
                                LocalReplyKind::Search => {
                                    disabled_mcp_rejections = 0;
                                    preflight_mcp_rejections = 0;
                                    let _ = reply_tx.send(reply);
                                }
                                LocalReplyKind::PreflightError => {
                                    preflight_mcp_rejections =
                                        preflight_mcp_rejections.saturating_add(1);
                                    if preflight_mcp_rejections
                                        > MAX_CONSECUTIVE_PREFLIGHT_MCP_REJECTIONS
                                    {
                                        exceeded_rejection_reason = Some(
                                            "\"model repeatedly failed virtualized MCP tool preflight\"",
                                        );
                                    } else {
                                        let _ = reply_tx.send(reply);
                                    }
                                }
                                LocalReplyKind::DisabledTool => {
                                    disabled_mcp_rejections =
                                        disabled_mcp_rejections.saturating_add(1);
                                    if disabled_mcp_rejections
                                        > MAX_CONSECUTIVE_DISABLED_MCP_REJECTIONS
                                    {
                                        exceeded_rejection_reason =
                                            Some("\"model repeatedly invoked disabled MCP tool\"");
                                    } else {
                                        let _ = reply_tx.send(reply);
                                    }
                                }
                            },
                            ResolvedMcpIncoming::ForwardToClient {
                                client_name,
                                arguments,
                                is_custom,
                                reply_tx,
                            } => {
                                disabled_mcp_rejections = 0;
                                preflight_mcp_rejections = 0;
                                ready_forwarded_calls.push(ForwardedMcpCall {
                                    client_name,
                                    arguments,
                                    is_custom,
                                    reply_tx,
                                });
                            }
                        }
                    }

                    if let Some(abort_msg) = exceeded_rejection_reason {
                        ready_forwarded_calls.clear();
                        state.tainted_session = true;
                        state.halted_post_tool_runaway = true;
                        backend.cancel_prompt_nonblocking(&session_id);
                        prompt_outcome = match timeout(Duration::from_millis(500), &mut prompt_fut)
                            .await
                        {
                            Ok(Ok(mut res)) => {
                                res.stop_reason = if state.emitted_client_tool {
                                    "tool_use".to_owned()
                                } else {
                                    "end_turn".to_owned()
                                };
                                if res.error.is_none() {
                                    res.error = RawValue::from_string(abort_msg.to_owned()).ok();
                                }
                                Some(Ok(res))
                            }
                            _ => Some(Ok(PromptTurnResult {
                                stop_reason: if state.emitted_client_tool {
                                    "tool_use".to_owned()
                                } else {
                                    "end_turn".to_owned()
                                },
                                usage: AcpUsage::default(),
                                error: RawValue::from_string(abort_msg.to_owned()).ok(),
                            })),
                        };
                        break 'substep_loop;
                    }

                    if ready_forwarded_calls.is_empty() {
                        continue 'substep_loop;
                    }
                }
                break 'substep_loop;
            }

            // Case A: Native MCP tool suspension (`POST /mcp/{token} tools/call` parked in-flight).
            if !ready_forwarded_calls.is_empty() && prompt_outcome.is_none() {
                state.flush_text_part();
                let mut pending_infos = Vec::with_capacity(ready_forwarded_calls.len());
                let mut parked_replies: Vec<(String, oneshot::Sender<McpToolCallReply>)> =
                    Vec::with_capacity(ready_forwarded_calls.len());
                let mut step_tool_arg_chars: usize = 0;

                for forwarded in ready_forwarded_calls {
                    let (info, reply_tx, ev, arg_chars) =
                        record_forwarded_mcp_call(forwarded, &cur_task_guard, &mut state);
                    step_tool_arg_chars = step_tool_arg_chars.saturating_add(arg_chars);
                    parked_replies.push((info.nonce_suffix.clone(), reply_tx));
                    pending_infos.push(info);
                    if !send_cancel_safe(&cur_events_tx, ev, &cur_cancel).await {
                        drop(parked_replies);
                        cur_lease.finish_ephemeral();
                        return;
                    }
                }

                let u_k = state.turn_peak_context_used;
                let (telemetry_step_usage, wire_step_usage, step_primary_prompt, step_primary_cr) =
                    if u_k > 0 {
                        let is_cold_first = !initial_session_reused && llm_step_idx == 0;
                        let primary_u = estimate_step_mcp_usage(
                            u_k,
                            is_cold_first,
                            prev_prompt_tokens,
                            state.step_thought_chars,
                            state.step_text_chars,
                            step_tool_arg_chars,
                        );
                        accounted_usage = accounted_usage.saturating_add(primary_u);
                        prev_prompt_tokens = primary_u.total_prompt_tokens();
                        llm_step_idx = llm_step_idx.saturating_add(1);
                        let combined_tel =
                            primary_u.saturating_add(std::mem::take(&mut cur_vision_usage));
                        (
                            combined_tel,
                            primary_u,
                            primary_u.total_prompt_tokens(),
                            primary_u.cached_read_tokens,
                        )
                    } else {
                        let wire_u = AcpUsage {
                            cached_read_tokens: latest_context_used,
                            total_tokens: latest_context_used,
                            ..AcpUsage::default()
                        };
                        let tel_u = std::mem::take(&mut cur_vision_usage);
                        (tel_u, wire_u, 0, 0)
                    };

                let telemetry_hub_ref = Arc::clone(cur_task_guard.hub());
                let pre_step_len = cur_full_messages.len();
                let pre_step_hash = hash_conversation_state(
                    cur_req.system_instructions.as_deref(),
                    &cur_req.client_tools,
                    cur_web_cfg,
                    cur_lease.model_id(),
                    &cur_full_messages,
                );
                let pre_step_dialogue_hash =
                    hash_dialogue_state(cur_lease.model_id(), &cur_full_messages);
                let alt_pre_user_hash = (cur_route_meta.delta_start_index > 0)
                    .then(|| cur_full_messages.get(..cur_route_meta.delta_start_index))
                    .flatten()
                    .map(|prefix| {
                        hash_conversation_state(
                            cur_req.system_instructions.as_deref(),
                            &cur_req.client_tools,
                            cur_web_cfg,
                            cur_lease.model_id(),
                            prefix,
                        )
                    });

                let assistant_msg = CanonicalMessage {
                    role: CanonicalRole::Assistant,
                    parts: state.assistant_parts,
                };
                let post_turn_hash = hash_conversation_with_reply(
                    cur_req.system_instructions.as_deref(),
                    &cur_req.client_tools,
                    cur_web_cfg,
                    cur_lease.model_id(),
                    &cur_full_messages,
                    &assistant_msg,
                );
                let post_turn_dialogue_hash = hash_dialogue_with_reply(
                    cur_lease.model_id(),
                    &cur_full_messages,
                    &assistant_msg,
                );

                if telemetry_step_usage.total_prompt_tokens() > 0 {
                    let pre_reply_slice = cur_full_messages.get(..pre_step_len).unwrap_or(&[]);
                    let diag = TurnCompletionDiagnostics {
                        routing_strategy: cur_route_meta.routing_strategy,
                        delta_start_index: cur_route_meta.delta_start_index,
                        session_turn_index: cur_route_meta.session_turn_index,
                        session_idle_ms: cur_route_meta.session_idle_ms,
                        previous_session_context_tokens: cur_route_meta
                            .previous_session_context_tokens,
                        model_switched: cur_route_meta.model_switched,
                        previous_model: cur_route_meta.previous_model.as_deref(),
                        previous_response_id: cur_req.previous_response_id.as_deref(),
                        prefix_hash_hex: cur_route_meta.prefix_hash_hex.as_deref(),
                        base_state_hash_hex: &cur_route_meta.base_state_hash_hex,
                        reasoning_effort: cur_req.reasoning_effort,
                        web_cfg: cur_web_cfg,
                        acp_system_append: cur_route_meta.acp_system_append.as_deref(),
                        tools_delta_xml: cur_route_meta.tools_delta_xml.as_deref(),
                        outbound_acp_blocks: &cur_outbound_blocks,
                        messages: pre_reply_slice,
                        client_tools: &cur_req.client_tools,
                    };
                    cur_task_guard.complete_with_diagnostics(
                        "tool_use".to_owned(),
                        telemetry_step_usage,
                        cur_req.auxiliary_task.is_none().then_some(diag),
                    );
                } else {
                    cur_task_guard.complete("tool_use".to_owned(), telemetry_step_usage);
                }

                cur_full_messages.push(assistant_msg);
                let message_count = cur_full_messages.len();
                let retained_messages = if cur_req.retain_response_history {
                    std::mem::take(&mut cur_full_messages)
                } else {
                    Vec::new()
                };
                let (cmd_tx, mut cmd_rx) = oneshot::channel::<McpDriverCommand>();

                // Park into `SessionPool` BEFORE emitting `TurnEvent::Completed` so any 0ms
                // client follow-up request is guaranteed to find the suspended session in pool.
                cur_lease.suspend_on_mcp(SuspendedMcpTurn {
                    mcp_handle: Arc::clone(&mcp_handle),
                    pending_calls: pending_infos,
                    cmd_tx,
                    post_turn_hash,
                    post_turn_dialogue_hash,
                    pre_step_hash,
                    pre_step_dialogue_hash,
                    alt_pre_user_hash,
                    declared_tools: std::mem::take(&mut cur_req.client_tools),
                    last_context_used: latest_context_used,
                    step_prompt_tokens: step_primary_prompt,
                    step_cached_read_tokens: step_primary_cr,
                    message_count,
                    pre_step_message_count: pre_step_len,
                    last_response_id: Some(cur_response_id.clone()),
                    accumulated_messages: retained_messages,
                });

                let _ = cur_events_tx
                    .send(TurnEvent::Completed {
                        usage: wire_step_usage,
                        stop_reason: "tool_use".to_owned(),
                    })
                    .await;
                drop(cur_events_tx);

                // Parked state: wait for next HTTP tool-result continuation, unwind, or eviction.
                loop {
                    tokio::select! {
                        biased;
                        cmd_res = &mut cmd_rx => {
                            match cmd_res {
                                Ok(McpDriverCommand::ResumeStep(next_step)) => {
                                    let McpResumedStepInput {
                                        raw_lease,
                                        task_guard: next_tg,
                                        req: next_req,
                                        web_cfg: next_wcfg,
                                        full_messages: next_msgs,
                                        route_meta: next_rmeta,
                                        outbound_diag_blocks: next_diag_blocks,
                                        response_id: next_resp_id,
                                        tool_replies,
                                        vision_offload_usage: next_vis_usage,
                                        events_tx: next_ev_tx,
                                        step_cancel: next_cancel,
                                        spill_store: next_spill,
                                        retire_after_turn: next_retire,
                                        timeout_secs: next_tsecs,
                                    } = *next_step;

                                    cur_lease = SessionLeaseGuard::from_raw_parts(
                                        raw_lease,
                                        Arc::clone(&backend),
                                    );
                                    cur_task_guard = next_tg;
                                    cur_req = next_req;
                                    cur_web_cfg = next_wcfg;
                                    cur_full_messages = next_msgs;
                                    cur_route_meta = next_rmeta;
                                    cur_outbound_blocks = next_diag_blocks;
                                    cur_response_id = next_resp_id;
                                    cur_vision_usage = next_vis_usage;
                                    cur_events_tx = next_ev_tx;
                                    cur_cancel = next_cancel;
                                    cur_spill_store = next_spill;
                                    cur_retire_after_turn = next_retire;
                                    cur_timeout_secs = next_tsecs;

                                    let mut reply_map: HashMap<String, McpToolCallReply> =
                                        tool_replies.into_iter().collect();
                                    for (suffix, reply_tx) in parked_replies {
                                        let reply = reply_map.remove(&suffix).unwrap_or(
                                            McpToolCallReply {
                                                content: vec![McpContentItem::Text {
                                                    text: String::new(),
                                                }],
                                                is_error: false,
                                            },
                                        );
                                        let _ = reply_tx.send(reply);
                                    }
                                    break;
                                }
                                Ok(McpDriverCommand::CancelForUnwind { done_tx }) => {
                                    drop(parked_replies);
                                    while call_rx_guard.try_recv().is_ok() {}
                                    match timeout(Duration::from_secs(3), &mut prompt_fut).await {
                                        Ok(Ok(res)) => {
                                            if llm_step_idx > 0 {
                                                telemetry_hub_ref.reconcile_cascade_usage_drift(
                                                    accounted_usage,
                                                    res.usage,
                                                    initial_session_reused,
                                                );
                                            }
                                            let unwound_delta =
                                                res.usage.saturating_sub(accounted_usage);
                                            let final_ctx = if res.usage.total_tokens > 0 {
                                                res.usage.total_tokens
                                            } else {
                                                latest_context_used
                                            };
                                            let _ = done_tx.send(Ok((
                                                final_ctx,
                                                unwound_delta,
                                                Arc::clone(&mcp_handle),
                                            )));
                                        }
                                        _ => {
                                            backend.abort_session_nonblocking(&session_id);
                                            let _ = done_tx.send(Err(()));
                                        }
                                    }
                                    return;
                                }
                                Err(_) => {
                                    drop(parked_replies);
                                    backend.abort_session_nonblocking(&session_id);
                                    return;
                                }
                            }
                        }
                        maybe_upd = acp_rx.recv(), if !acp_closed => {
                            match maybe_upd {
                                Some(SessionUpdatePayload::UsageUpdate { used, .. }) => {
                                    if used > 0 {
                                        peak_context_used = peak_context_used.max(used);
                                        latest_context_used = used;
                                    }
                                }
                                Some(_) => {}
                                None => {
                                    acp_closed = true;
                                }
                            }
                        }
                        _ = &mut prompt_fut => {
                            drop(parked_replies);
                            if let Some(p) = pool.upgrade() {
                                p.remove_dead_suspended(&session_id);
                            }
                            return;
                        }
                    }
                }
                continue;
            }

            // Case B: `session/prompt` completed (final prose turn, XML-only tool turn, or error).
            match prompt_outcome {
                Some(Ok(turn_res)) => {
                    let last_turn_error = turn_res
                        .error
                        .as_ref()
                        .map(|raw| raw.get().trim().to_owned())
                        .filter(|s| !s.is_empty() && s != "null" && s != "{}");

                    // Server-side `__gateway_web_fetch` continuation loop.
                    if !state.pending_web_fetches.is_empty() && !state.emitted_client_tool {
                        let max_fetch_uses =
                            cur_req.web_fetch_max_uses.unwrap_or(5).clamp(1, 10) as usize;
                        let queued_fetches: Vec<(String, String)> =
                            std::mem::take(&mut state.pending_web_fetches);
                        let mut continuation_xml = String::new();
                        let fetch_deadline = Instant::now() + Duration::from_secs(cur_timeout_secs);

                        for (fetch_call_id, raw_url) in queued_fetches {
                            if !send_cancel_safe(
                                &cur_events_tx,
                                TurnEvent::WebFetchStart {
                                    call_id: fetch_call_id.clone(),
                                    url: raw_url.clone(),
                                },
                                &cur_cancel,
                            )
                            .await
                            {
                                cur_lease.finish_ephemeral();
                                return;
                            }

                            let outcome = if completed_web_fetches >= max_fetch_uses {
                                WebFetchOutcome::from_error(
                                    raw_url.clone(),
                                    &WebFetchError::MaxUsesExceeded {
                                        max_uses: max_fetch_uses,
                                    },
                                )
                            } else {
                                completed_web_fetches = completed_web_fetches.saturating_add(1);
                                cur_task_guard.record_web_fetch();
                                let fetch_fut = backend.fetch_web_url(
                                    &raw_url,
                                    &cur_req.web_fetch_allowed_domains,
                                    &cur_req.web_fetch_blocked_domains,
                                );
                                tokio::select! {
                                    biased;
                                    () = cur_cancel.cancelled() => {
                                        cur_lease.finish_ephemeral();
                                        return;
                                    }
                                    timed = timeout_at(fetch_deadline, fetch_fut) => {
                                        match timed {
                                            Ok(out) => out,
                                            Err(_) => WebFetchOutcome::from_error(
                                                raw_url.clone(),
                                                &WebFetchError::TurnDeadlineExceeded,
                                            ),
                                        }
                                    }
                                }
                            };

                            format_web_fetch_continuation(&mut continuation_xml, &outcome);
                            if !send_cancel_safe(
                                &cur_events_tx,
                                TurnEvent::WebFetchComplete {
                                    call_id: fetch_call_id,
                                    outcome,
                                },
                                &cur_cancel,
                            )
                            .await
                            {
                                cur_lease.finish_ephemeral();
                                return;
                            }
                        }

                        continuation_xml.push_str(
                            "\nNow synthesize a complete, accurate response grounded in the fetched page content above.",
                        );
                        if continuation_xml.len() > MAX_ACP_PROMPT_TEXT_BYTES {
                            continuation_xml = truncate_head_tail_lines_with_spill(
                                &continuation_xml,
                                MAX_ACP_PROMPT_TEXT_BYTES,
                                "web_fetch content",
                                &mut cur_spill_store,
                            );
                        }
                        let step_delta = turn_res.usage.saturating_sub(accounted_usage);
                        if llm_step_idx > 0 {
                            let accounted_total = accounted_usage.saturating_add(step_delta);
                            cur_task_guard.hub().reconcile_cascade_usage_drift(
                                accounted_total,
                                turn_res.usage,
                                initial_session_reused,
                            );
                            let excess_prompt = accounted_total
                                .total_prompt_tokens()
                                .saturating_sub(turn_res.usage.total_prompt_tokens());
                            let excess_cr = accounted_total
                                .cached_read_tokens
                                .saturating_sub(turn_res.usage.cached_read_tokens);
                            if excess_prompt > 0 || excess_cr > 0 {
                                cur_lease.reconcile_prior_cascade_tokens(excess_prompt, excess_cr);
                            }
                        }
                        cur_vision_usage = cur_vision_usage.saturating_add(step_delta);
                        next_prompt_blocks = Some(Arc::from(vec![AcpPromptBlock::Text {
                            text: continuation_xml,
                        }]));
                        continue 'prompt_loop;
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

                    let final_stop_reason = if state.emitted_client_tool {
                        "tool_use".to_owned()
                    } else if !turn_res.stop_reason.is_empty() {
                        turn_res.stop_reason.clone()
                    } else {
                        "end_turn".to_owned()
                    };

                    if !produced_any_output {
                        cur_lease.finish_ephemeral();
                        let recent_stderr = backend.recent_stderr_since(step_started_ms);
                        let err = build_zero_output_error(
                            &active_model,
                            &final_stop_reason,
                            last_turn_error.as_deref(),
                            &suppressed_banners,
                            &recent_stderr,
                        );
                        cur_task_guard.fail(format_error_chain(&err));
                        let _ = cur_events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
                        return;
                    }

                    let step_outbound_bytes: usize = cur_outbound_blocks
                        .iter()
                        .map(|b| match b {
                            AcpPromptBlock::Text { text } => text.len(),
                            AcpPromptBlock::Image { .. } => 8_192,
                        })
                        .sum();

                    let (
                        telemetry_step_usage,
                        wire_usage,
                        commit_prompt_tokens,
                        commit_cr_tokens,
                        reconcile_cascade_auth,
                    ) = if llm_step_idx > 0 {
                        let auth = turn_res.usage;
                        let step_m = compute_final_mcp_step_usage(
                            auth,
                            accounted_usage,
                            state.turn_peak_context_used,
                            latest_context_used,
                        );
                        let combined_tel = step_m.saturating_add(cur_vision_usage);
                        (
                            combined_tel,
                            step_m,
                            auth.total_prompt_tokens(),
                            auth.cached_read_tokens,
                            Some((accounted_usage.saturating_add(step_m), auth)),
                        )
                    } else {
                        let wire_u = reconcile_turn_context_usage(
                            turn_res.usage,
                            &state,
                            initial_context_used,
                            step_outbound_bytes,
                        );
                        let tel_u = turn_res.usage.saturating_add(cur_vision_usage);
                        (
                            tel_u,
                            wire_u,
                            tel_u.total_prompt_tokens(),
                            tel_u.cached_read_tokens,
                            None,
                        )
                    };

                    let telemetry_hub_ref = Arc::clone(cur_task_guard.hub());
                    let complete_stop = final_stop_reason.clone();
                    let req_auxiliary = cur_req.auxiliary_task;
                    let req_prev_resp_id = cur_req.previous_response_id.clone();
                    let req_reasoning = cur_req.reasoning_effort;
                    let req_sys_inst = cur_req.system_instructions.clone();

                    if cur_retire_after_turn
                        || cur_req.auxiliary_task.is_some()
                        || state.tainted_session
                    {
                        cur_lease.finish_ephemeral();
                        let diag = TurnCompletionDiagnostics {
                            routing_strategy: cur_route_meta.routing_strategy,
                            delta_start_index: cur_route_meta.delta_start_index,
                            session_turn_index: cur_route_meta.session_turn_index,
                            session_idle_ms: cur_route_meta.session_idle_ms,
                            previous_session_context_tokens: cur_route_meta
                                .previous_session_context_tokens,
                            model_switched: cur_route_meta.model_switched,
                            previous_model: cur_route_meta.previous_model.as_deref(),
                            previous_response_id: req_prev_resp_id.as_deref(),
                            prefix_hash_hex: cur_route_meta.prefix_hash_hex.as_deref(),
                            base_state_hash_hex: &cur_route_meta.base_state_hash_hex,
                            reasoning_effort: req_reasoning,
                            web_cfg: cur_web_cfg,
                            acp_system_append: cur_route_meta.acp_system_append.as_deref(),
                            tools_delta_xml: cur_route_meta.tools_delta_xml.as_deref(),
                            outbound_acp_blocks: &cur_outbound_blocks,
                            messages: &cur_full_messages,
                            client_tools: &cur_req.client_tools,
                        };
                        cur_task_guard.complete_with_diagnostics(
                            complete_stop,
                            telemetry_step_usage,
                            req_auxiliary.is_none().then_some(diag),
                        );
                    } else {
                        let pre_reply_len = cur_full_messages.len();
                        let assistant_msg = CanonicalMessage {
                            role: CanonicalRole::Assistant,
                            parts: state.assistant_parts,
                        };
                        let post_hash = hash_conversation_with_reply(
                            req_sys_inst.as_deref(),
                            &cur_req.client_tools,
                            cur_web_cfg,
                            cur_lease.model_id(),
                            &cur_full_messages,
                            &assistant_msg,
                        );
                        let dialogue_hash = hash_dialogue_with_reply(
                            cur_lease.model_id(),
                            &cur_full_messages,
                            &assistant_msg,
                        );
                        {
                            let pre_reply_slice =
                                cur_full_messages.get(..pre_reply_len).unwrap_or(&[]);
                            let diag = TurnCompletionDiagnostics {
                                routing_strategy: cur_route_meta.routing_strategy,
                                delta_start_index: cur_route_meta.delta_start_index,
                                session_turn_index: cur_route_meta.session_turn_index,
                                session_idle_ms: cur_route_meta.session_idle_ms,
                                previous_session_context_tokens: cur_route_meta
                                    .previous_session_context_tokens,
                                model_switched: cur_route_meta.model_switched,
                                previous_model: cur_route_meta.previous_model.as_deref(),
                                previous_response_id: req_prev_resp_id.as_deref(),
                                prefix_hash_hex: cur_route_meta.prefix_hash_hex.as_deref(),
                                base_state_hash_hex: &cur_route_meta.base_state_hash_hex,
                                reasoning_effort: req_reasoning,
                                web_cfg: cur_web_cfg,
                                acp_system_append: cur_route_meta.acp_system_append.as_deref(),
                                tools_delta_xml: cur_route_meta.tools_delta_xml.as_deref(),
                                outbound_acp_blocks: &cur_outbound_blocks,
                                messages: pre_reply_slice,
                                client_tools: &cur_req.client_tools,
                            };
                            cur_task_guard.complete_with_diagnostics(
                                complete_stop,
                                telemetry_step_usage,
                                Some(diag),
                            );
                        }
                        cur_full_messages.push(assistant_msg);
                        let message_count = cur_full_messages.len();
                        let retained_messages = if cur_req.retain_response_history {
                            cur_full_messages
                        } else {
                            Vec::new()
                        };
                        let committed_context_used = if state.turn_peak_context_used > 0 {
                            state.latest_context_used
                        } else {
                            wire_usage.total_tokens.max(latest_context_used)
                        };
                        if llm_step_idx > 0 {
                            cur_lease.reconcile_prior_cascade_tokens(
                                accounted_usage.total_prompt_tokens(),
                                accounted_usage.cached_read_tokens,
                            );
                        }
                        cur_lease.commit(CommittedSessionTurn {
                            post_turn_hash: post_hash,
                            alt_post_turn_hash: None,
                            post_turn_dialogue_hash: dialogue_hash,
                            declared_tools: cur_req.client_tools,
                            last_context_used: committed_context_used,
                            turn_prompt_tokens: commit_prompt_tokens,
                            turn_cached_read_tokens: commit_cr_tokens,
                            message_count,
                            last_response_id: Some(cur_response_id),
                            accumulated_messages: retained_messages,
                        });
                    }

                    if let Some((accounted_total, auth)) = reconcile_cascade_auth {
                        telemetry_hub_ref.reconcile_cascade_usage_drift(
                            accounted_total,
                            auth,
                            initial_session_reused,
                        );
                    }

                    let _ = cur_events_tx
                        .send(TurnEvent::Completed {
                            usage: wire_usage,
                            stop_reason: final_stop_reason,
                        })
                        .await;
                    return;
                }
                Some(Err(err)) => {
                    cur_lease.finish_ephemeral();
                    let err = upgrade_acp_error(err, &active_model);
                    cur_task_guard.fail(format_error_chain(&err));
                    let _ = cur_events_tx.send(TurnEvent::Failed(Arc::new(err))).await;
                    return;
                }
                None => {
                    cur_lease.finish_ephemeral();
                    return;
                }
            }
        }
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

    #[test]
    fn normalizes_null_args_and_coerces_stringified_array_and_object_fields_by_schema() {
        use liftgate_types::canonical::CanonicalToolSpec;

        let spec = CanonicalToolSpec {
            name: "apply_edits".to_owned(),
            description: Some("Batch edit files".to_owned()),
            kind: CanonicalToolKind::Function {
                parameters_json: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "edits": { "type": "array" },
                        "options": { "type": ["object", "null"] },
                        "literal_json_text": { "type": "string" }
                    },
                    "required": ["edits"]
                })
                .to_string(),
            },
        };

        assert_eq!(
            normalize_emitted_tool_arguments("null".to_owned(), false, false, Some(&spec)),
            "{}"
        );
        assert_eq!(
            normalize_emitted_tool_arguments(String::new(), false, false, Some(&spec)),
            "{}"
        );

        let stringified = serde_json::json!({
            "edits": "[{\"path\":\"src/lib.rs\",\"target\":\"old\",\"replacement\":\"new\"}]",
            "options": "{\"dryRun\":true}",
            "literal_json_text": "[1, 2, 3]"
        })
        .to_string();
        let normalized = normalize_emitted_tool_arguments(stringified, false, false, Some(&spec));
        let parsed: serde_json::Value =
            serde_json::from_str(&normalized).expect("must remain valid json object");
        assert!(parsed["edits"].is_array());
        assert_eq!(parsed["edits"][0]["path"], "src/lib.rs");
        assert_eq!(parsed["options"]["dryRun"], true);
        assert_eq!(
            parsed["literal_json_text"], "[1, 2, 3]",
            "string-typed schema properties must never be coerced into arrays"
        );
    }

    #[test]
    fn mcp_step_and_final_usage_accounting_has_zero_cumulative_drift() {
        let hub = Arc::new(TelemetryHub::new());
        let auth_total = AcpUsage {
            input_tokens: 14_200,
            output_tokens: 950,
            cached_read_tokens: 23_800,
            cached_write_tokens: 0,
            thought_tokens: 310,
            total_tokens: 38_950,
        };

        // Cold Step 1 (`u_1 = 12_500`): `cached_read == 0`
        let step1 = estimate_step_mcp_usage(12_500, true, 0, 400, 120, 280);
        assert_eq!(step1.cached_read_tokens, 0);
        assert_eq!(step1.total_tokens, 12_500);
        assert_eq!(step1.input_tokens + step1.output_tokens, 12_500);
        let g1 = hub.begin_task(
            "resp_step1".to_owned(),
            TaskProtocol::OpenaiResponses,
            true,
            "gemini-4-carbon-high".to_owned(),
            None,
            &[],
        );
        g1.bind_session("sess_mcp_1", false, "gemini-4-carbon-high");
        g1.complete("tool_use".to_owned(), step1);

        // Warm Step 2 (`u_2 = 13_400`, `prev_prompt_tokens = step1.total_prompt_tokens()`)
        let step2 =
            estimate_step_mcp_usage(13_400, false, step1.total_prompt_tokens(), 320, 80, 240);
        assert!(step2.cached_read_tokens > 9_000);
        assert_eq!(step2.total_tokens, 13_400);
        let g2 = hub.begin_task(
            "resp_step2".to_owned(),
            TaskProtocol::OpenaiResponses,
            true,
            "gemini-4-carbon-high".to_owned(),
            None,
            &[],
        );
        g2.bind_session("sess_mcp_1", true, "gemini-4-carbon-high");
        g2.complete("tool_use".to_owned(), step2);

        // Final Step 3 (`u_3 = 14_600`) reconciled against `auth_total`
        let accounted_prior = step1.saturating_add(step2);
        let step3 = compute_final_mcp_step_usage(auth_total, accounted_prior, 14_600, 13_400);
        assert_eq!(step3.total_tokens, 14_600);
        let g3 = hub.begin_task(
            "resp_step3".to_owned(),
            TaskProtocol::OpenaiResponses,
            true,
            "gemini-4-carbon-high".to_owned(),
            None,
            &[],
        );
        g3.bind_session("sess_mcp_1", true, "gemini-4-carbon-high");
        g3.complete("end_turn".to_owned(), step3);

        hub.reconcile_cascade_usage_drift(accounted_prior.saturating_add(step3), auth_total, false);

        let (metrics, cache_summary, _, _) = hub.snapshot_tasks();
        assert_eq!(metrics.total_input_tokens, auth_total.input_tokens);
        assert_eq!(metrics.total_output_tokens, auth_total.output_tokens);
        assert_eq!(metrics.total_thought_tokens, auth_total.thought_tokens);
        assert_eq!(metrics.total_cached_tokens, auth_total.cached_read_tokens);
        assert_eq!(
            cache_summary.total_prompt_tokens,
            auth_total.total_prompt_tokens()
        );
        assert_eq!(
            cache_summary.total_cached_read_tokens,
            auth_total.cached_read_tokens
        );
    }
}
