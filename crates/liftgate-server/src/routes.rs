use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::Infallible;
use std::path::{Component, Path as StdPath, PathBuf};
use std::sync::{Arc, atomic::Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use liftgate_types::{
    acp::AcpUsage,
    anthropic::{
        ANTHROPIC_THOUGHT_SIGNATURE, AnthropicCountTokensResponse, AnthropicMessagesRequest,
        AnthropicMessagesResponse, AnthropicOutputBlock, AnthropicServerToolUsage, AnthropicUsage,
        AnthropicWebSearchResultItem, estimate_anthropic_tokens, normalize_anthropic_request,
    },
    canonical::{extract_markdown_links, extract_search_query_hint},
    config::{CacheHitThresholdBps, LowCacheRecordLimit},
    openai::{
        CreateResponseRequest, LocalShellAction, ModelCard, ModelChatMetadata, ModelListResponse,
        ModelModalities, OutputMessageContentPart, ReasoningSummaryPart, ResponseInlineError,
        ResponseObject, ResponseOutputItem, ResponseTextFormatConfig, ResponsesUsage,
        WebSearchAction, WebSearchSource, build_codex_model_catalog_with_tool_mode,
        normalize_responses_request, split_namespaced_tool_name,
    },
    session_key::{INTERNAL_CODEX_LOCAL_SHELL_TOOL, INTERNAL_CODEX_TOOL_SEARCH_TOOL},
    telemetry::{GatewayTelemetrySnapshot, TaskProtocol},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    acp_client::AcpBackend,
    error::{AcpError, GatewayError, RequestBodyError},
    session_pool::SessionPool,
    telemetry::{TelemetryHub, unix_timestamp_ms},
    turn_runner::{
        ActiveTurnStream, GatewayRuntimeConfig, TurnEvent, execute_normalized_turn,
        generate_short_nonce,
    },
    web_fetch::WebFetchOutcome,
};

/// Header name required by `POST /api/admin/shutdown` for managed daemon lifecycle control.
pub const ADMIN_SHUTDOWN_HEADER: &str = "x-liftgate-shutdown-token";

const CONTROL_ROUTE_BODY_LIMIT_BYTES: usize = 256 * 1024;
const INITIAL_BODY_ALLOC_CAP_BYTES: usize = 4 * 1024 * 1024;
const MAX_BODY_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Compile-time `(relative_slash_path, bytes)` entry for embedded companion web assets.
pub type EmbeddedUiAsset = (&'static str, &'static [u8]);

/// Token-authenticated graceful shutdown trigger exposed to the local `liftgate` CLI.
#[derive(Clone, Debug)]
pub struct AdminShutdownHandle {
    token: Arc<str>,
    trigger: tokio::sync::watch::Sender<bool>,
}

impl AdminShutdownHandle {
    /// Creates a new shutdown handle paired with a `watch::Receiver<bool>` that resolves to `true`
    /// once an authenticated `POST /api/admin/shutdown` request arrives.
    #[must_use]
    pub fn new(token: Arc<str>) -> (Self, tokio::sync::watch::Receiver<bool>) {
        let (trigger, rx) = tokio::sync::watch::channel(false);
        (Self { token, trigger }, rx)
    }

    fn verify_and_trigger(&self, provided: &str) -> bool {
        if self.token.is_empty() || !constant_time_eq(self.token.as_bytes(), provided.as_bytes()) {
            return false;
        }
        let _ = self.trigger.send_replace(true);
        true
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (&lhs, &rhs) in a.iter().zip(b.iter()) {
        diff |= lhs ^ rhs;
    }
    diff == 0
}

/// Shared application state parameterized statically over `B: AcpBackend`.
pub struct AppState<B: AcpBackend> {
    pub backend: Arc<B>,
    pub pool: Arc<SessionPool>,
    pub telemetry: Arc<TelemetryHub>,
    pub config: GatewayRuntimeConfig,
    pub ui_dist_dir: Option<PathBuf>,
    pub embedded_ui: &'static [EmbeddedUiAsset],
    pub admin_shutdown: Option<AdminShutdownHandle>,
}

impl<B: AcpBackend> Clone for AppState<B> {
    fn clone(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
            pool: Arc::clone(&self.pool),
            telemetry: Arc::clone(&self.telemetry),
            config: self.config.clone(),
            ui_dist_dir: self.ui_dist_dir.clone(),
            embedded_ui: self.embedded_ui,
            admin_shutdown: self.admin_shutdown.clone(),
        }
    }
}

/// Builds the complete HTTP gateway `Router` exposing OpenAI `/v1/responses`,
/// Anthropic `/v1/messages`, `/v1/models`, `/healthz`, real-time `/api/telemetry/*`,
/// and optional companion web UI static hosting.
pub fn build_router<B: AcpBackend>(state: AppState<B>) -> Router {
    let turn_routes = Router::new()
        .route("/v1/responses", post(handle_create_response::<B>))
        .route("/responses", post(handle_create_response::<B>))
        .route("/v1/messages", post(handle_anthropic_messages::<B>))
        .route("/messages", post(handle_anthropic_messages::<B>))
        .route(
            "/v1/messages/count_tokens",
            post(handle_anthropic_count_tokens::<B>),
        )
        .route(
            "/messages/count_tokens",
            post(handle_anthropic_count_tokens::<B>),
        )
        .route(
            "/claude-desktop/v1/messages",
            post(handle_anthropic_messages::<B>),
        )
        .route(
            "/claude-desktop/messages",
            post(handle_anthropic_messages::<B>),
        )
        .route(
            "/claude-desktop/v1/messages/count_tokens",
            post(handle_anthropic_count_tokens::<B>),
        )
        .route(
            "/claude-desktop/messages/count_tokens",
            post(handle_anthropic_count_tokens::<B>),
        )
        .layer(DefaultBodyLimit::disable());

    let control_routes = Router::new()
        .route("/healthz", get(handle_healthz))
        .route("/v1/models", get(handle_list_models::<B>))
        .route("/models", get(handle_list_models::<B>))
        .route("/v1/codex-models", get(handle_codex_model_catalog::<B>))
        .route("/codex-models", get(handle_codex_model_catalog::<B>))
        .route("/v1/models/{model_id}", get(handle_get_model::<B>))
        .route("/models/{model_id}", get(handle_get_model::<B>))
        .route("/v1/responses/{response_id}", get(handle_get_response::<B>))
        .route("/responses/{response_id}", get(handle_get_response::<B>))
        .route(
            "/claude-desktop/v1/models",
            get(handle_claude_desktop_models::<B>),
        )
        .route(
            "/claude-desktop/models",
            get(handle_claude_desktop_models::<B>),
        )
        .route(
            "/claude-desktop/v1/models/{model_id}",
            get(handle_get_model::<B>),
        )
        .route(
            "/claude-desktop/models/{model_id}",
            get(handle_get_model::<B>),
        )
        .route(
            "/claude-desktop/api/event_logging/batch",
            post(handle_event_logging_noop),
        )
        .route(
            "/claude-desktop/user/bootstrap",
            get(handle_claude_desktop_bootstrap::<B>),
        )
        .route("/user/bootstrap", get(handle_claude_desktop_bootstrap::<B>))
        .route("/api/event_logging/batch", post(handle_event_logging_noop))
        .route(
            "/api/telemetry/snapshot",
            get(handle_telemetry_snapshot::<B>),
        )
        .route("/api/telemetry/events", get(handle_telemetry_events::<B>))
        .route(
            "/api/telemetry/sessions/{session_id}/close",
            post(handle_close_idle_session::<B>),
        )
        .route(
            "/api/telemetry/vision-offload",
            post(handle_set_vision_offload::<B>),
        )
        .route(
            "/api/telemetry/mcp-tool-bridge",
            post(handle_set_mcp_tool_bridge::<B>),
        )
        .route(
            "/api/telemetry/low-cache-hits",
            get(handle_get_low_cache_hits::<B>),
        )
        .route(
            "/api/telemetry/low-cache-hits/{task_id}",
            get(handle_get_low_cache_hit_detail::<B>),
        )
        .route(
            "/api/telemetry/low-cache-hits/clear",
            post(handle_clear_low_cache_hits::<B>),
        )
        .route(
            "/api/telemetry/cache-config",
            post(handle_set_cache_config::<B>),
        )
        .route("/api/admin/shutdown", post(handle_admin_shutdown::<B>))
        .fallback(handle_ui_static::<B>)
        .layer(DefaultBodyLimit::max(CONTROL_ROUTE_BODY_LIMIT_BYTES));

    control_routes
        .merge(turn_routes)
        .layer(middleware::from_fn(cors_middleware))
        .with_state(state)
}

/// Serves `router` over `listener` with graceful shutdown without exposing `axum`
/// directly to the thin binary crate.
pub async fn serve_router(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}

async fn handle_healthz() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

async fn handle_event_logging_noop() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

async fn handle_claude_desktop_bootstrap<B: AcpBackend>(
    State(state): State<AppState<B>>,
    headers: HeaderMap,
) -> Response {
    if let Err(err) = check_auth(&headers, state.config.api_key.as_deref()) {
        return err.into_anthropic_response();
    }
    Json(json!({
        "builtinBrowserEnabled": true,
    }))
    .into_response()
}

async fn handle_claude_desktop_models<B: AcpBackend>(
    State(state): State<AppState<B>>,
    headers: HeaderMap,
) -> Response {
    const MAX_DESKTOP_MODELS: usize = 200;
    const MAX_DESCRIPTION_CHARS: usize = 100;

    if let Err(err) = check_auth(&headers, state.config.api_key.as_deref()) {
        return err.into_anthropic_response();
    }
    let available = state.backend.refresh_models_if_empty().await;
    let default_id = state.config.default_model.as_str();

    let mut ordered: Vec<(String, String)> = Vec::with_capacity(available.len().max(1));
    if let Some(primary) = available
        .iter()
        .find(|m| m.model_id.eq_ignore_ascii_case(default_id))
    {
        let label = if primary.name.is_empty() {
            primary.model_id.clone()
        } else {
            primary.name.clone()
        };
        ordered.push((primary.model_id.clone(), label));
    } else if available.is_empty() {
        ordered.push((default_id.to_owned(), default_id.to_owned()));
    }

    for m in available {
        if ordered
            .iter()
            .any(|(existing_id, _)| existing_id.eq_ignore_ascii_case(&m.model_id))
        {
            continue;
        }
        let label = if m.name.is_empty() {
            m.model_id.clone()
        } else {
            m.name
        };
        ordered.push((m.model_id, label));
        if ordered.len() >= MAX_DESKTOP_MODELS {
            break;
        }
    }

    let data: Vec<Value> = ordered
        .into_iter()
        .map(|(model_id, display_name)| {
            let route_id = liftgate_types::model::encode_claude_desktop_route_id(&model_id);
            let (window, _) = liftgate_types::model::context_limits(&model_id);
            let supports_extended = window > liftgate_types::model::CLAUDE_DEFAULT_WINDOW;
            let raw_desc = format!(
                "{}k ctx \u{00b7} Antigravity ACP ({model_id})",
                window / 1_000
            );
            let description: String = raw_desc.chars().take(MAX_DESCRIPTION_CHARS).collect();
            let mut item = json!({
                "type": "model",
                "id": route_id,
                "display_name": display_name,
                "created_at": "2026-01-01T00:00:00Z",
                "description": description,
            });
            if supports_extended && let Some(obj) = item.as_object_mut() {
                obj.insert("supports1m".to_owned(), json!(true));
                obj.insert("prefer1m".to_owned(), json!(true));
            }
            item
        })
        .collect();

    let first_id = data
        .first()
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let last_id = data
        .last()
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    Json(json!({
        "data": data,
        "has_more": false,
        "first_id": first_id,
        "last_id": last_id,
    }))
    .into_response()
}

fn build_model_card(id: String, display_name: String, created: u64) -> ModelCard {
    let (context_window, auto_compact_token_limit) = liftgate_types::model::context_limits(&id);
    let description = format!(
        "{}k ctx \u{00b7} Antigravity ACP ({id})",
        context_window / 1_000
    );
    ModelCard {
        id,
        object: "model",
        item_type: "model",
        display_name,
        description,
        created,
        created_at: "2026-01-01T00:00:00Z",
        owned_by: "antigravity",
        context_window,
        auto_compact_token_limit,
        max_input_tokens: context_window,
        max_output_tokens: 65_536,
        chat: ModelChatMetadata {
            modalities: ModelModalities {
                input: vec!["text", "image"],
                output: vec!["text"],
            },
        },
    }
}

async fn handle_list_models<B: AcpBackend>(
    State(state): State<AppState<B>>,
    headers: HeaderMap,
) -> Response {
    if let Err(err) = check_auth(&headers, state.config.api_key.as_deref()) {
        return err.into_openai_response();
    }
    let now = unix_timestamp_secs();
    let models = state.backend.refresh_models_if_empty().await;
    let mut data: Vec<ModelCard> = models
        .into_iter()
        .map(|m| {
            let display = if m.name.is_empty() {
                m.model_id.clone()
            } else {
                m.name
            };
            build_model_card(m.model_id, display, now)
        })
        .collect();

    if data.is_empty() {
        let fallback_id = state.config.default_model.as_str().to_owned();
        data.push(build_model_card(fallback_id.clone(), fallback_id, now));
    }

    let first_id = data.first().map(|m| m.id.clone());
    let last_id = data.last().map(|m| m.id.clone());

    Json(ModelListResponse {
        object: "list",
        data,
        has_more: false,
        first_id,
        last_id,
    })
    .into_response()
}

fn query_param<'a>(uri: &'a Uri, key: &str) -> Option<&'a str> {
    uri.query()?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        let trimmed = v.trim();
        (k == key && !trimmed.is_empty()).then_some(trimmed)
    })
}

async fn handle_codex_model_catalog<B: AcpBackend>(
    State(state): State<AppState<B>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Err(err) = check_auth(&headers, state.config.api_key.as_deref()) {
        return err.into_openai_response();
    }
    let available = state.backend.refresh_models_if_empty().await;
    let preferred_default =
        query_param(&uri, "default_model").unwrap_or_else(|| state.config.default_model.as_str());
    let tool_mode = query_param(&uri, "tool_mode");
    let catalog =
        build_codex_model_catalog_with_tool_mode(&available, preferred_default, tool_mode);
    Json(catalog).into_response()
}

async fn handle_get_model<B: AcpBackend>(
    State(state): State<AppState<B>>,
    headers: HeaderMap,
    Path(model_id): Path<String>,
) -> Response {
    if let Err(err) = check_auth(&headers, state.config.api_key.as_deref()) {
        return err.into_openai_response();
    }
    let now = unix_timestamp_secs();
    let available = state.backend.refresh_models_if_empty().await;
    if let Some(exact) = available
        .iter()
        .find(|m| m.model_id.eq_ignore_ascii_case(&model_id))
    {
        let display = if exact.name.is_empty() {
            exact.model_id.clone()
        } else {
            exact.name.clone()
        };
        return Json(build_model_card(exact.model_id.clone(), display, now)).into_response();
    }

    let resolved = liftgate_types::model::resolve_model_id(
        Some(&model_id),
        None,
        &available,
        &state.config.default_model,
    );
    let display = available
        .iter()
        .find(|m| m.model_id == resolved.as_str())
        .filter(|m| !m.name.is_empty())
        .map_or_else(|| resolved.as_str().to_owned(), |m| m.name.clone());

    let mut card = build_model_card(resolved.as_str().to_owned(), display, now);
    card.id = model_id;
    Json(card).into_response()
}

async fn handle_get_response<B: AcpBackend>(
    State(state): State<AppState<B>>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Response {
    if let Err(err) = check_auth(&headers, state.config.api_key.as_deref()) {
        return err.into_openai_response();
    }
    match state.pool.get_response_json(&response_id) {
        Some(raw_json) => ([(header::CONTENT_TYPE, "application/json")], raw_json).into_response(),
        None => GatewayError::NotFound {
            resource: response_id,
        }
        .into_openai_response(),
    }
}

// ============================================================================
// OpenAI `/v1/responses` Handler (Non-Streaming + SSE Streaming)
// ============================================================================

#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "explicit wire-rendering flags for OpenAI Responses output items"
)]
struct OpenAiRenderOptions {
    emit_web_search_items: bool,
    force_web_search: bool,
    emit_web_fetch_items: bool,
    emit_tool_search_items: bool,
    include_encrypted_reasoning: bool,
    query_hint: Option<String>,
}

async fn handle_create_response<B: AcpBackend>(
    State(state): State<AppState<B>>,
    http_req: Request,
) -> Response {
    let req: CreateResponseRequest = match read_authenticated_json_body(&state, http_req).await {
        Ok(parsed) => parsed,
        Err(err) => return err.into_openai_response(),
    };

    let normalized = match normalize_responses_request(&req) {
        Ok(n) => n,
        Err(err) => return GatewayError::from(err).into_openai_response(),
    };
    let CreateResponseRequest {
        model,
        input,
        instructions,
        previous_response_id,
        tools,
        tool_choice,
        parallel_tool_calls,
        reasoning,
        include,
        stream: _,
        max_output_tokens,
        temperature,
        top_p,
        store,
        metadata,
    } = req;
    drop((model, input, reasoning, include));

    let is_stream = normalized.stream;
    let render_opts = OpenAiRenderOptions {
        emit_web_search_items: normalized.enable_native_web_search,
        force_web_search: normalized.force_native_web_search,
        emit_web_fetch_items: normalized.enable_native_web_fetch,
        emit_tool_search_items: tools.iter().any(|t| t.tool_type == "tool_search"),
        include_encrypted_reasoning: normalized.include_encrypted_reasoning,
        query_hint: extract_search_query_hint(&normalized.messages),
    };
    let resp_id = format!("resp_{}", generate_short_nonce());
    let created_at = unix_timestamp_secs();
    let mut base_resp = ResponseObject {
        id: resp_id.clone(),
        object: Cow::Borrowed("response"),
        created_at,
        status: Cow::Borrowed("in_progress"),
        error: None,
        incomplete_details: None,
        instructions,
        max_output_tokens,
        model: String::new(),
        output: Vec::new(),
        parallel_tool_calls: parallel_tool_calls.unwrap_or(true),
        previous_response_id,
        store: store.unwrap_or(true),
        temperature: temperature.unwrap_or(1.0),
        text: ResponseTextFormatConfig::default(),
        tool_choice: tool_choice.unwrap_or_else(|| json!("auto")),
        tools,
        top_p: top_p.unwrap_or(1.0),
        truncation: Cow::Borrowed("disabled"),
        usage: None,
        metadata: metadata.unwrap_or_default(),
    };

    let turn_stream = match execute_normalized_turn(
        Arc::clone(&state.backend),
        Arc::clone(&state.pool),
        &state.telemetry,
        &state.config,
        TaskProtocol::OpenaiResponses,
        normalized,
        resp_id,
    )
    .await
    {
        Ok(s) => s,
        Err(err) => return err.into_openai_response(),
    };
    base_resp.model = turn_stream.resolved_model.as_str().to_owned();

    if is_stream {
        stream_openai_responses(
            turn_stream,
            base_resp,
            render_opts,
            Arc::clone(&state.pool),
            state.config.limits.channel_capacity().get(),
        )
    } else {
        collect_openai_response(turn_stream, base_resp, render_opts, &state.pool).await
    }
}

async fn collect_openai_response(
    mut turn: ActiveTurnStream,
    mut resp: ResponseObject,
    opts: OpenAiRenderOptions,
    pool: &SessionPool,
) -> Response {
    let mut thought_text = String::new();
    let mut msg_text = String::new();
    let mut tool_items: Vec<ResponseOutputItem> = Vec::new();
    let mut search_items: Vec<(String, Option<String>)> = Vec::new();
    let mut fetch_urls: Vec<(String, String)> = Vec::new();
    let mut final_usage = AcpUsage::default();

    while let Some(ev) = turn.rx.recv().await {
        match ev {
            TurnEvent::ThoughtDelta(delta) => thought_text.push_str(&delta),
            TurnEvent::WebSearch {
                call_id,
                status,
                query,
            } => {
                if opts.emit_web_search_items && status == "completed" {
                    search_items.push((call_id, query));
                }
            }
            TurnEvent::WebFetchStart { .. } => {}
            TurnEvent::WebFetchComplete { call_id, outcome } => {
                if opts.emit_web_fetch_items || opts.emit_web_search_items {
                    let url = match outcome {
                        WebFetchOutcome::Success { url, .. }
                        | WebFetchOutcome::Error { url, .. } => url,
                    };
                    fetch_urls.push((call_id, url));
                }
            }
            TurnEvent::TextDelta(delta) => msg_text.push_str(&delta),
            TurnEvent::ClientToolCallStart { .. } | TurnEvent::ClientToolCallDelta { .. } => {}
            TurnEvent::ClientToolCall {
                call_id,
                name,
                arguments,
                is_custom,
            } => {
                tool_items.push(build_openai_client_tool_item(
                    call_id,
                    name,
                    arguments,
                    is_custom,
                    opts.emit_tool_search_items,
                ));
            }
            TurnEvent::Completed { usage, .. } => {
                final_usage = usage;
                break;
            }
            TurnEvent::Failed(err) => {
                return GatewayError::SharedAcp { source: err }.into_openai_response();
            }
        }
    }

    if opts.emit_web_search_items && opts.force_web_search && search_items.is_empty() {
        search_items.push((generate_short_nonce(), opts.query_hint.clone()));
    }

    if !thought_text.trim().is_empty() {
        resp.output.push(ResponseOutputItem::Reasoning {
            id: format!("rs_{}", generate_short_nonce()),
            summary: vec![ReasoningSummaryPart {
                part_type: "summary_text".to_owned(),
                text: thought_text,
            }],
            encrypted_content: opts
                .include_encrypted_reasoning
                .then(|| "agy_reasoning_v1".to_owned()),
        });
    }

    let extracted_links = if !search_items.is_empty() || !fetch_urls.is_empty() {
        extract_markdown_links(&msg_text)
    } else {
        Vec::new()
    };
    let ws_sources = build_openai_web_sources(&extracted_links);

    for (ws_id, query) in search_items {
        resp.output.push(ResponseOutputItem::WebSearchCall {
            id: format!("ws_{}", sanitize_call_suffix(&ws_id)),
            status: "completed".to_owned(),
            action: Some(WebSearchAction {
                action_type: "search".to_owned(),
                query: query.or_else(|| opts.query_hint.clone()),
                url: None,
                sources: (!ws_sources.is_empty()).then(|| ws_sources.clone()),
            }),
        });
    }

    for (fetch_id, url) in fetch_urls {
        resp.output.push(ResponseOutputItem::WebSearchCall {
            id: format!("ws_{}", sanitize_call_suffix(&fetch_id)),
            status: "completed".to_owned(),
            action: Some(WebSearchAction {
                action_type: "open_page".to_owned(),
                query: None,
                url: Some(url),
                sources: None,
            }),
        });
    }

    let trimmed_for_check = msg_text.trim();
    if !trimmed_for_check.is_empty() || tool_items.is_empty() {
        let final_text = if tool_items.is_empty() {
            msg_text
        } else {
            trimmed_for_check.to_owned()
        };
        let annotations = if !ws_sources.is_empty() || !extracted_links.is_empty() {
            build_openai_url_annotations(&final_text)
        } else {
            Vec::new()
        };
        resp.output.push(ResponseOutputItem::Message {
            id: format!("msg_{}", generate_short_nonce()),
            status: "completed".to_owned(),
            role: "assistant".to_owned(),
            content: vec![OutputMessageContentPart {
                part_type: "output_text".to_owned(),
                text: final_text,
                annotations,
            }],
        });
    }

    resp.output.extend(tool_items);
    resp.status = Cow::Borrowed("completed");
    resp.usage = Some(ResponsesUsage::from(final_usage));
    if resp.store {
        pool.store_response(resp.clone());
    }

    Json(resp).into_response()
}

struct OpenStreamingToolItem {
    idx: usize,
    item_id: String,
    is_custom: bool,
    emitted_any_delta: bool,
}

fn stream_openai_responses(
    mut turn: ActiveTurnStream,
    mut resp: ResponseObject,
    opts: OpenAiRenderOptions,
    pool: Arc<SessionPool>,
    channel_cap: usize,
) -> Response {
    let (sse_tx, sse_rx) = mpsc::channel::<Result<Bytes, Infallible>>(channel_cap);

    tokio::spawn(async move {
        let mut seq: u64 = 0;

        if !send_sse_frame(
            &sse_tx,
            "response.created",
            &json!({
                "type": "response.created",
                "sequence_number": next_seq(&mut seq),
                "response": &resp,
            }),
        )
        .await
        {
            return;
        }
        if !send_sse_frame(
            &sse_tx,
            "response.in_progress",
            &json!({
                "type": "response.in_progress",
                "sequence_number": next_seq(&mut seq),
                "response": &resp,
            }),
        )
        .await
        {
            return;
        }

        let mut output_index: usize = 0;
        let mut active_reasoning: Option<(usize, String, String)> = None;
        let mut active_message: Option<(usize, String, String)> = None;
        let mut open_searches: HashMap<String, (usize, String, Option<String>)> = HashMap::new();
        let mut open_fetches: HashMap<String, (usize, String, String)> = HashMap::new();
        let mut open_client_tools: HashMap<String, OpenStreamingToolItem> = HashMap::new();
        let mut used_web_grounding = false;
        let mut emitted_search_count: usize = 0;
        let mut leading_ws_buf = String::new();
        let mut finalized_items: Vec<ResponseOutputItem> = Vec::new();
        let mut final_usage = AcpUsage::default();
        let mut failed_err: Option<Arc<crate::error::AcpError>> = None;

        loop {
            let ev = tokio::select! {
                biased;
                () = sse_tx.closed() => return,
                maybe_ev = turn.rx.recv() => match maybe_ev {
                    Some(ev) => ev,
                    None => break,
                },
            };
            match ev {
                TurnEvent::ThoughtDelta(delta) => {
                    if active_message.is_some() {
                        continue;
                    }
                    let (idx, item_id, acc) = active_reasoning.get_or_insert_with(|| {
                        let idx = output_index;
                        output_index += 1;
                        (idx, format!("rs_{}", generate_short_nonce()), String::new())
                    });
                    if acc.is_empty() {
                        let mut item_json = json!({
                            "id": item_id,
                            "type": "reasoning",
                            "summary": []
                        });
                        if opts.include_encrypted_reasoning
                            && let Some(obj) = item_json.as_object_mut()
                        {
                            obj.insert("encrypted_content".to_owned(), json!("agy_reasoning_v1"));
                        }
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.output_item.added",
                            &json!({
                                "type": "response.output_item.added",
                                "sequence_number": next_seq(&mut seq),
                                "output_index": *idx,
                                "item": item_json,
                            }),
                        )
                        .await;
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.reasoning_summary_part.added",
                            &json!({
                                "type": "response.reasoning_summary_part.added",
                                "sequence_number": next_seq(&mut seq),
                                "item_id": item_id,
                                "output_index": *idx,
                                "summary_index": 0,
                                "part": { "type": "summary_text", "text": "" }
                            }),
                        )
                        .await;
                    }
                    acc.push_str(&delta);
                    if !send_sse_frame(
                        &sse_tx,
                        "response.reasoning_summary_text.delta",
                        &json!({
                            "type": "response.reasoning_summary_text.delta",
                            "sequence_number": next_seq(&mut seq),
                            "item_id": item_id,
                            "output_index": *idx,
                            "summary_index": 0,
                            "delta": delta,
                        }),
                    )
                    .await
                    {
                        return;
                    }
                }
                TurnEvent::WebSearch {
                    call_id,
                    status,
                    query,
                } => {
                    if !opts.emit_web_search_items {
                        continue;
                    }
                    used_web_grounding = true;
                    close_active_reasoning(
                        &sse_tx,
                        &mut seq,
                        &mut active_reasoning,
                        opts.include_encrypted_reasoning,
                        &mut finalized_items,
                    )
                    .await;
                    close_active_message(
                        &sse_tx,
                        &mut seq,
                        &mut active_message,
                        used_web_grounding,
                        &mut finalized_items,
                    )
                    .await;

                    if status != "completed" {
                        if let Some(entry) = open_searches.get_mut(&call_id) {
                            if query.is_some() {
                                entry.2 = query;
                            }
                        } else {
                            let idx = output_index;
                            output_index += 1;
                            let ws_id = format!("ws_{}", sanitize_call_suffix(&call_id));
                            let resolved_q = query.or_else(|| opts.query_hint.clone());
                            let added_item = ResponseOutputItem::WebSearchCall {
                                id: ws_id.clone(),
                                status: "in_progress".to_owned(),
                                action: Some(WebSearchAction {
                                    action_type: "search".to_owned(),
                                    query: resolved_q.clone(),
                                    url: None,
                                    sources: None,
                                }),
                            };
                            let _ = send_sse_frame(
                                &sse_tx,
                                "response.output_item.added",
                                &json!({
                                    "type": "response.output_item.added",
                                    "sequence_number": next_seq(&mut seq),
                                    "output_index": idx,
                                    "item": &added_item,
                                }),
                            )
                            .await;
                            let _ = send_sse_frame(
                                &sse_tx,
                                "response.web_search_call.in_progress",
                                &json!({
                                    "type": "response.web_search_call.in_progress",
                                    "sequence_number": next_seq(&mut seq),
                                    "output_index": idx,
                                    "item_id": ws_id,
                                }),
                            )
                            .await;
                            let _ = send_sse_frame(
                                &sse_tx,
                                "response.web_search_call.searching",
                                &json!({
                                    "type": "response.web_search_call.searching",
                                    "sequence_number": next_seq(&mut seq),
                                    "output_index": idx,
                                    "item_id": ws_id,
                                }),
                            )
                            .await;
                            open_searches.insert(call_id, (idx, ws_id, resolved_q));
                        }
                    } else {
                        let (idx, ws_id, prev_q) =
                            if let Some(opened) = open_searches.remove(&call_id) {
                                opened
                            } else {
                                let idx = output_index;
                                output_index += 1;
                                let ws_id = format!("ws_{}", sanitize_call_suffix(&call_id));
                                let init_q = query.clone().or_else(|| opts.query_hint.clone());
                                let added_item = ResponseOutputItem::WebSearchCall {
                                    id: ws_id.clone(),
                                    status: "in_progress".to_owned(),
                                    action: Some(WebSearchAction {
                                        action_type: "search".to_owned(),
                                        query: init_q.clone(),
                                        url: None,
                                        sources: None,
                                    }),
                                };
                                let _ = send_sse_frame(
                                    &sse_tx,
                                    "response.output_item.added",
                                    &json!({
                                        "type": "response.output_item.added",
                                        "sequence_number": next_seq(&mut seq),
                                        "output_index": idx,
                                        "item": &added_item,
                                    }),
                                )
                                .await;
                                let _ = send_sse_frame(
                                    &sse_tx,
                                    "response.web_search_call.in_progress",
                                    &json!({
                                        "type": "response.web_search_call.in_progress",
                                        "sequence_number": next_seq(&mut seq),
                                        "output_index": idx,
                                        "item_id": ws_id,
                                    }),
                                )
                                .await;
                                let _ = send_sse_frame(
                                    &sse_tx,
                                    "response.web_search_call.searching",
                                    &json!({
                                        "type": "response.web_search_call.searching",
                                        "sequence_number": next_seq(&mut seq),
                                        "output_index": idx,
                                        "item_id": ws_id,
                                    }),
                                )
                                .await;
                                (idx, ws_id, init_q)
                            };
                        emitted_search_count = emitted_search_count.saturating_add(1);
                        let done_item = ResponseOutputItem::WebSearchCall {
                            id: ws_id.clone(),
                            status: "completed".to_owned(),
                            action: Some(WebSearchAction {
                                action_type: "search".to_owned(),
                                query: query.or(prev_q).or_else(|| opts.query_hint.clone()),
                                url: None,
                                sources: None,
                            }),
                        };
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.web_search_call.completed",
                            &json!({
                                "type": "response.web_search_call.completed",
                                "sequence_number": next_seq(&mut seq),
                                "output_index": idx,
                                "item_id": ws_id,
                            }),
                        )
                        .await;
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.output_item.done",
                            &json!({
                                "type": "response.output_item.done",
                                "sequence_number": next_seq(&mut seq),
                                "output_index": idx,
                                "item": &done_item,
                            }),
                        )
                        .await;
                        finalized_items.push(done_item);
                    }
                }
                TurnEvent::WebFetchStart { call_id, url } => {
                    if !(opts.emit_web_fetch_items || opts.emit_web_search_items) {
                        continue;
                    }
                    used_web_grounding = true;
                    close_active_reasoning(
                        &sse_tx,
                        &mut seq,
                        &mut active_reasoning,
                        opts.include_encrypted_reasoning,
                        &mut finalized_items,
                    )
                    .await;
                    close_active_message(
                        &sse_tx,
                        &mut seq,
                        &mut active_message,
                        used_web_grounding,
                        &mut finalized_items,
                    )
                    .await;

                    let idx = output_index;
                    output_index += 1;
                    let ws_id = format!("ws_{}", sanitize_call_suffix(&call_id));
                    let added_item = ResponseOutputItem::WebSearchCall {
                        id: ws_id.clone(),
                        status: "in_progress".to_owned(),
                        action: Some(WebSearchAction {
                            action_type: "open_page".to_owned(),
                            query: None,
                            url: Some(url.clone()),
                            sources: None,
                        }),
                    };
                    let _ = send_sse_frame(
                        &sse_tx,
                        "response.output_item.added",
                        &json!({
                            "type": "response.output_item.added",
                            "sequence_number": next_seq(&mut seq),
                            "output_index": idx,
                            "item": &added_item,
                        }),
                    )
                    .await;
                    let _ = send_sse_frame(
                        &sse_tx,
                        "response.web_search_call.in_progress",
                        &json!({
                            "type": "response.web_search_call.in_progress",
                            "sequence_number": next_seq(&mut seq),
                            "output_index": idx,
                            "item_id": ws_id,
                        }),
                    )
                    .await;
                    open_fetches.insert(call_id, (idx, ws_id, url));
                }
                TurnEvent::WebFetchComplete { call_id, outcome } => {
                    if !(opts.emit_web_fetch_items || opts.emit_web_search_items) {
                        continue;
                    }
                    let outcome_url = match outcome {
                        WebFetchOutcome::Success { url, .. }
                        | WebFetchOutcome::Error { url, .. } => url,
                    };
                    let (idx, ws_id, url) = if let Some(opened) = open_fetches.remove(&call_id) {
                        opened
                    } else {
                        let idx = output_index;
                        output_index += 1;
                        let ws_id = format!("ws_{}", sanitize_call_suffix(&call_id));
                        (idx, ws_id, outcome_url)
                    };
                    let done_item = ResponseOutputItem::WebSearchCall {
                        id: ws_id.clone(),
                        status: "completed".to_owned(),
                        action: Some(WebSearchAction {
                            action_type: "open_page".to_owned(),
                            query: None,
                            url: Some(url),
                            sources: None,
                        }),
                    };
                    let _ = send_sse_frame(
                        &sse_tx,
                        "response.web_search_call.completed",
                        &json!({
                            "type": "response.web_search_call.completed",
                            "sequence_number": next_seq(&mut seq),
                            "output_index": idx,
                            "item_id": ws_id,
                        }),
                    )
                    .await;
                    let _ = send_sse_frame(
                        &sse_tx,
                        "response.output_item.done",
                        &json!({
                            "type": "response.output_item.done",
                            "sequence_number": next_seq(&mut seq),
                            "output_index": idx,
                            "item": &done_item,
                        }),
                    )
                    .await;
                    finalized_items.push(done_item);
                }
                TurnEvent::TextDelta(delta) => {
                    close_active_reasoning(
                        &sse_tx,
                        &mut seq,
                        &mut active_reasoning,
                        opts.include_encrypted_reasoning,
                        &mut finalized_items,
                    )
                    .await;
                    if opts.emit_web_search_items
                        && opts.force_web_search
                        && emitted_search_count == 0
                        && open_searches.is_empty()
                    {
                        used_web_grounding = true;
                        emitted_search_count = 1;
                        let ws_idx = output_index;
                        output_index += 1;
                        let ws_id = format!("ws_{}", generate_short_nonce());
                        let synth_item = ResponseOutputItem::WebSearchCall {
                            id: ws_id.clone(),
                            status: "completed".to_owned(),
                            action: Some(WebSearchAction {
                                action_type: "search".to_owned(),
                                query: opts.query_hint.clone(),
                                url: None,
                                sources: None,
                            }),
                        };
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.output_item.added",
                            &json!({
                                "type": "response.output_item.added",
                                "sequence_number": next_seq(&mut seq),
                                "output_index": ws_idx,
                                "item": &synth_item,
                            }),
                        )
                        .await;
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.web_search_call.completed",
                            &json!({
                                "type": "response.web_search_call.completed",
                                "sequence_number": next_seq(&mut seq),
                                "output_index": ws_idx,
                                "item_id": ws_id,
                            }),
                        )
                        .await;
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.output_item.done",
                            &json!({
                                "type": "response.output_item.done",
                                "sequence_number": next_seq(&mut seq),
                                "output_index": ws_idx,
                                "item": &synth_item,
                            }),
                        )
                        .await;
                        finalized_items.push(synth_item);
                    }

                    if active_message.is_none() && delta.trim().is_empty() {
                        leading_ws_buf.push_str(&delta);
                        continue;
                    }
                    let effective_delta = if !leading_ws_buf.is_empty() {
                        let mut combined = std::mem::take(&mut leading_ws_buf);
                        combined.push_str(&delta);
                        combined
                    } else {
                        delta
                    };

                    let is_new = active_message.is_none();
                    let (idx, msg_id, acc) = active_message.get_or_insert_with(|| {
                        let idx = output_index;
                        output_index += 1;
                        (
                            idx,
                            format!("msg_{}", generate_short_nonce()),
                            String::new(),
                        )
                    });
                    if is_new {
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.output_item.added",
                            &json!({
                                "type": "response.output_item.added",
                                "sequence_number": next_seq(&mut seq),
                                "output_index": *idx,
                                "item": {
                                    "id": msg_id,
                                    "type": "message",
                                    "status": "in_progress",
                                    "role": "assistant",
                                    "content": []
                                }
                            }),
                        )
                        .await;
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.content_part.added",
                            &json!({
                                "type": "response.content_part.added",
                                "sequence_number": next_seq(&mut seq),
                                "item_id": msg_id,
                                "output_index": *idx,
                                "content_index": 0,
                                "part": {
                                    "type": "output_text",
                                    "text": "",
                                    "annotations": []
                                }
                            }),
                        )
                        .await;
                    }
                    acc.push_str(&effective_delta);
                    if !send_sse_frame(
                        &sse_tx,
                        "response.output_text.delta",
                        &json!({
                            "type": "response.output_text.delta",
                            "sequence_number": next_seq(&mut seq),
                            "item_id": msg_id,
                            "output_index": *idx,
                            "content_index": 0,
                            "delta": effective_delta,
                            "logprobs": []
                        }),
                    )
                    .await
                    {
                        return;
                    }
                }
                TurnEvent::ClientToolCallStart {
                    call_id,
                    name,
                    is_custom,
                } => {
                    leading_ws_buf.clear();
                    close_active_reasoning(
                        &sse_tx,
                        &mut seq,
                        &mut active_reasoning,
                        opts.include_encrypted_reasoning,
                        &mut finalized_items,
                    )
                    .await;
                    close_active_message(
                        &sse_tx,
                        &mut seq,
                        &mut active_message,
                        used_web_grounding,
                        &mut finalized_items,
                    )
                    .await;

                    if name == INTERNAL_CODEX_LOCAL_SHELL_TOOL
                        || (opts.emit_tool_search_items && name == INTERNAL_CODEX_TOOL_SEARCH_TOOL)
                    {
                        continue;
                    }

                    let idx = output_index;
                    output_index += 1;
                    let (namespace, bare_name) = split_namespaced_tool_name(&name);
                    let (item_id, added_item) = if is_custom {
                        let item_id = format!("ctc_{}", generate_short_nonce());
                        (
                            item_id.clone(),
                            ResponseOutputItem::CustomToolCall {
                                id: item_id,
                                status: "in_progress".to_owned(),
                                call_id: call_id.clone(),
                                namespace,
                                name: bare_name,
                                input: String::new(),
                            },
                        )
                    } else {
                        let item_id = format!("fc_{}", generate_short_nonce());
                        (
                            item_id.clone(),
                            ResponseOutputItem::FunctionCall {
                                id: item_id,
                                status: "in_progress".to_owned(),
                                call_id: call_id.clone(),
                                namespace,
                                name: bare_name,
                                arguments: String::new(),
                            },
                        )
                    };
                    if !send_sse_frame(
                        &sse_tx,
                        "response.output_item.added",
                        &json!({
                            "type": "response.output_item.added",
                            "sequence_number": next_seq(&mut seq),
                            "output_index": idx,
                            "item": &added_item,
                        }),
                    )
                    .await
                    {
                        return;
                    }
                    open_client_tools.insert(
                        call_id,
                        OpenStreamingToolItem {
                            idx,
                            item_id,
                            is_custom,
                            emitted_any_delta: false,
                        },
                    );
                }
                TurnEvent::ClientToolCallDelta { call_id, delta } => {
                    if !delta.is_empty()
                        && let Some(open) = open_client_tools.get_mut(&call_id)
                        && open.is_custom
                    {
                        open.emitted_any_delta = true;
                        if !send_sse_frame(
                            &sse_tx,
                            "response.custom_tool_call_input.delta",
                            &json!({
                                "type": "response.custom_tool_call_input.delta",
                                "sequence_number": next_seq(&mut seq),
                                "item_id": &open.item_id,
                                "call_id": call_id,
                                "output_index": open.idx,
                                "delta": delta,
                            }),
                        )
                        .await
                        {
                            return;
                        }
                    }
                }
                TurnEvent::ClientToolCall {
                    call_id,
                    name,
                    arguments,
                    is_custom,
                } => {
                    leading_ws_buf.clear();
                    close_active_reasoning(
                        &sse_tx,
                        &mut seq,
                        &mut active_reasoning,
                        opts.include_encrypted_reasoning,
                        &mut finalized_items,
                    )
                    .await;
                    close_active_message(
                        &sse_tx,
                        &mut seq,
                        &mut active_message,
                        used_web_grounding,
                        &mut finalized_items,
                    )
                    .await;

                    if let Some(open) = open_client_tools.remove(&call_id) {
                        let (namespace, bare_name) = split_namespaced_tool_name(&name);
                        if open.is_custom {
                            if !open.emitted_any_delta {
                                let _ = send_sse_frame(
                                    &sse_tx,
                                    "response.custom_tool_call_input.delta",
                                    &json!({
                                        "type": "response.custom_tool_call_input.delta",
                                        "sequence_number": next_seq(&mut seq),
                                        "item_id": &open.item_id,
                                        "call_id": &call_id,
                                        "output_index": open.idx,
                                        "delta": &arguments,
                                    }),
                                )
                                .await;
                            }
                            let _ = send_sse_frame(
                                &sse_tx,
                                "response.custom_tool_call_input.done",
                                &json!({
                                    "type": "response.custom_tool_call_input.done",
                                    "sequence_number": next_seq(&mut seq),
                                    "item_id": &open.item_id,
                                    "call_id": &call_id,
                                    "output_index": open.idx,
                                    "input": &arguments,
                                }),
                            )
                            .await;
                            let done_item = ResponseOutputItem::CustomToolCall {
                                id: open.item_id,
                                status: "completed".to_owned(),
                                call_id,
                                namespace,
                                name: bare_name,
                                input: arguments,
                            };
                            let _ = send_sse_frame(
                                &sse_tx,
                                "response.output_item.done",
                                &json!({
                                    "type": "response.output_item.done",
                                    "sequence_number": next_seq(&mut seq),
                                    "output_index": open.idx,
                                    "item": &done_item,
                                }),
                            )
                            .await;
                            finalized_items.push(done_item);
                        } else {
                            let _ = send_sse_frame(
                                &sse_tx,
                                "response.function_call_arguments.delta",
                                &json!({
                                    "type": "response.function_call_arguments.delta",
                                    "sequence_number": next_seq(&mut seq),
                                    "item_id": &open.item_id,
                                    "output_index": open.idx,
                                    "delta": &arguments,
                                }),
                            )
                            .await;
                            let mut args_done = json!({
                                "type": "response.function_call_arguments.done",
                                "sequence_number": next_seq(&mut seq),
                                "item_id": &open.item_id,
                                "output_index": open.idx,
                                "name": &bare_name,
                                "arguments": &arguments,
                            });
                            if let Some(ns) = &namespace
                                && let Some(obj) = args_done.as_object_mut()
                            {
                                obj.insert("namespace".to_owned(), json!(ns));
                            }
                            let _ = send_sse_frame(
                                &sse_tx,
                                "response.function_call_arguments.done",
                                &args_done,
                            )
                            .await;
                            let done_item = ResponseOutputItem::FunctionCall {
                                id: open.item_id,
                                status: "completed".to_owned(),
                                call_id,
                                namespace,
                                name: bare_name,
                                arguments,
                            };
                            let _ = send_sse_frame(
                                &sse_tx,
                                "response.output_item.done",
                                &json!({
                                    "type": "response.output_item.done",
                                    "sequence_number": next_seq(&mut seq),
                                    "output_index": open.idx,
                                    "item": &done_item,
                                }),
                            )
                            .await;
                            finalized_items.push(done_item);
                        }
                        continue;
                    }

                    let idx = output_index;
                    output_index += 1;

                    if name == INTERNAL_CODEX_LOCAL_SHELL_TOOL {
                        let item_id = format!("lsh_{}", generate_short_nonce());
                        let action = parse_local_shell_action(&arguments);
                        let added_item = ResponseOutputItem::LocalShellCall {
                            id: item_id.clone(),
                            status: "in_progress".to_owned(),
                            call_id: call_id.clone(),
                            action: action.clone(),
                        };
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.output_item.added",
                            &json!({
                                "type": "response.output_item.added",
                                "sequence_number": next_seq(&mut seq),
                                "output_index": idx,
                                "item": &added_item,
                            }),
                        )
                        .await;
                        let done_item = ResponseOutputItem::LocalShellCall {
                            id: item_id,
                            status: "completed".to_owned(),
                            call_id,
                            action,
                        };
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.output_item.done",
                            &json!({
                                "type": "response.output_item.done",
                                "sequence_number": next_seq(&mut seq),
                                "output_index": idx,
                                "item": &done_item,
                            }),
                        )
                        .await;
                        finalized_items.push(done_item);
                    } else if opts.emit_tool_search_items && name == INTERNAL_CODEX_TOOL_SEARCH_TOOL
                    {
                        let item_id = format!("tsc_{}", generate_short_nonce());
                        let parsed_args =
                            serde_json::from_str::<Value>(&arguments).unwrap_or_else(|_| json!({}));
                        let added_item = ResponseOutputItem::ToolSearchCall {
                            id: item_id.clone(),
                            status: "in_progress".to_owned(),
                            call_id: call_id.clone(),
                            execution: "client".to_owned(),
                            arguments: parsed_args.clone(),
                        };
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.output_item.added",
                            &json!({
                                "type": "response.output_item.added",
                                "sequence_number": next_seq(&mut seq),
                                "output_index": idx,
                                "item": &added_item,
                            }),
                        )
                        .await;
                        let done_item = ResponseOutputItem::ToolSearchCall {
                            id: item_id,
                            status: "completed".to_owned(),
                            call_id,
                            execution: "client".to_owned(),
                            arguments: parsed_args,
                        };
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.output_item.done",
                            &json!({
                                "type": "response.output_item.done",
                                "sequence_number": next_seq(&mut seq),
                                "output_index": idx,
                                "item": &done_item,
                            }),
                        )
                        .await;
                        finalized_items.push(done_item);
                    } else if is_custom {
                        let (namespace, bare_name) = split_namespaced_tool_name(&name);
                        let item_id = format!("ctc_{}", generate_short_nonce());
                        let added_item = ResponseOutputItem::CustomToolCall {
                            id: item_id.clone(),
                            status: "in_progress".to_owned(),
                            call_id: call_id.clone(),
                            namespace: namespace.clone(),
                            name: bare_name.clone(),
                            input: String::new(),
                        };
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.output_item.added",
                            &json!({
                                "type": "response.output_item.added",
                                "sequence_number": next_seq(&mut seq),
                                "output_index": idx,
                                "item": &added_item,
                            }),
                        )
                        .await;
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.custom_tool_call_input.delta",
                            &json!({
                                "type": "response.custom_tool_call_input.delta",
                                "sequence_number": next_seq(&mut seq),
                                "item_id": item_id,
                                "call_id": call_id,
                                "output_index": idx,
                                "delta": arguments,
                            }),
                        )
                        .await;
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.custom_tool_call_input.done",
                            &json!({
                                "type": "response.custom_tool_call_input.done",
                                "sequence_number": next_seq(&mut seq),
                                "item_id": item_id,
                                "call_id": call_id,
                                "output_index": idx,
                                "input": arguments,
                            }),
                        )
                        .await;
                        let done_item = ResponseOutputItem::CustomToolCall {
                            id: item_id,
                            status: "completed".to_owned(),
                            call_id,
                            namespace,
                            name: bare_name,
                            input: arguments,
                        };
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.output_item.done",
                            &json!({
                                "type": "response.output_item.done",
                                "sequence_number": next_seq(&mut seq),
                                "output_index": idx,
                                "item": &done_item,
                            }),
                        )
                        .await;
                        finalized_items.push(done_item);
                    } else {
                        let (namespace, bare_name) = split_namespaced_tool_name(&name);
                        let item_id = format!("fc_{}", generate_short_nonce());
                        let added_item = ResponseOutputItem::FunctionCall {
                            id: item_id.clone(),
                            status: "in_progress".to_owned(),
                            call_id: call_id.clone(),
                            namespace: namespace.clone(),
                            name: bare_name.clone(),
                            arguments: String::new(),
                        };
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.output_item.added",
                            &json!({
                                "type": "response.output_item.added",
                                "sequence_number": next_seq(&mut seq),
                                "output_index": idx,
                                "item": &added_item,
                            }),
                        )
                        .await;
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.function_call_arguments.delta",
                            &json!({
                                "type": "response.function_call_arguments.delta",
                                "sequence_number": next_seq(&mut seq),
                                "item_id": item_id,
                                "output_index": idx,
                                "delta": arguments,
                            }),
                        )
                        .await;
                        let mut args_done = json!({
                            "type": "response.function_call_arguments.done",
                            "sequence_number": next_seq(&mut seq),
                            "item_id": item_id,
                            "output_index": idx,
                            "name": bare_name,
                            "arguments": arguments,
                        });
                        if let Some(ns) = &namespace
                            && let Some(obj) = args_done.as_object_mut()
                        {
                            obj.insert("namespace".to_owned(), json!(ns));
                        }
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.function_call_arguments.done",
                            &args_done,
                        )
                        .await;
                        let done_item = ResponseOutputItem::FunctionCall {
                            id: item_id,
                            status: "completed".to_owned(),
                            call_id,
                            namespace,
                            name: bare_name,
                            arguments,
                        };
                        let _ = send_sse_frame(
                            &sse_tx,
                            "response.output_item.done",
                            &json!({
                                "type": "response.output_item.done",
                                "sequence_number": next_seq(&mut seq),
                                "output_index": idx,
                                "item": &done_item,
                            }),
                        )
                        .await;
                        finalized_items.push(done_item);
                    }
                }
                TurnEvent::Completed { usage, .. } => {
                    final_usage = usage;
                    break;
                }
                TurnEvent::Failed(err) => {
                    failed_err = Some(err);
                    break;
                }
            }
        }

        // Close any still-open search items cleanly
        for (_, (idx, ws_id, q)) in open_searches.drain() {
            let done_item = ResponseOutputItem::WebSearchCall {
                id: ws_id.clone(),
                status: "completed".to_owned(),
                action: Some(WebSearchAction {
                    action_type: "search".to_owned(),
                    query: q,
                    url: None,
                    sources: None,
                }),
            };
            let _ = send_sse_frame(
                &sse_tx,
                "response.web_search_call.completed",
                &json!({
                    "type": "response.web_search_call.completed",
                    "sequence_number": next_seq(&mut seq),
                    "output_index": idx,
                    "item_id": ws_id,
                }),
            )
            .await;
            let _ = send_sse_frame(
                &sse_tx,
                "response.output_item.done",
                &json!({
                    "type": "response.output_item.done",
                    "sequence_number": next_seq(&mut seq),
                    "output_index": idx,
                    "item": &done_item,
                }),
            )
            .await;
            finalized_items.push(done_item);
        }

        close_active_reasoning(
            &sse_tx,
            &mut seq,
            &mut active_reasoning,
            opts.include_encrypted_reasoning,
            &mut finalized_items,
        )
        .await;
        close_active_message(
            &sse_tx,
            &mut seq,
            &mut active_message,
            used_web_grounding,
            &mut finalized_items,
        )
        .await;

        if failed_err.is_none() && !open_client_tools.is_empty() {
            failed_err = Some(Arc::new(AcpError::UnclosedToolCallStream {
                protocol: "OpenAI Responses",
            }));
        }

        if let Some(acp_err) = failed_err {
            let gw_err = GatewayError::SharedAcp { source: acp_err };
            tracing::error!(error = ?gw_err, "OpenAI Responses SSE stream failed");
            let (_, code) = gw_err.stable_kind();
            resp.status = Cow::Borrowed("failed");
            resp.error = Some(ResponseInlineError {
                code: code.to_owned(),
                message: gw_err.openai_client_message(),
            });
            resp.output = finalized_items;
            let _ = send_sse_frame(
                &sse_tx,
                "response.failed",
                &json!({
                    "type": "response.failed",
                    "sequence_number": next_seq(&mut seq),
                    "response": &resp,
                }),
            )
            .await;
            return;
        }

        // Ensure at least an empty assistant message if nothing at all was emitted
        if finalized_items.is_empty() {
            let idx = output_index;
            let msg_id = format!("msg_{}", generate_short_nonce());
            let empty_item = ResponseOutputItem::Message {
                id: msg_id,
                status: "completed".to_owned(),
                role: "assistant".to_owned(),
                content: vec![OutputMessageContentPart {
                    part_type: "output_text".to_owned(),
                    text: leading_ws_buf,
                    annotations: Vec::new(),
                }],
            };
            let _ = send_sse_frame(
                &sse_tx,
                "response.output_item.added",
                &json!({
                    "type": "response.output_item.added",
                    "sequence_number": next_seq(&mut seq),
                    "output_index": idx,
                    "item": &empty_item,
                }),
            )
            .await;
            let _ = send_sse_frame(
                &sse_tx,
                "response.output_item.done",
                &json!({
                    "type": "response.output_item.done",
                    "sequence_number": next_seq(&mut seq),
                    "output_index": idx,
                    "item": &empty_item,
                }),
            )
            .await;
            finalized_items.push(empty_item);
        }

        resp.status = Cow::Borrowed("completed");
        resp.output = finalized_items;
        resp.usage = Some(ResponsesUsage::from(final_usage));
        if resp.store {
            pool.store_response(resp.clone());
        }

        let _ = send_sse_frame(
            &sse_tx,
            "response.completed",
            &json!({
                "type": "response.completed",
                "sequence_number": next_seq(&mut seq),
                "response": &resp,
            }),
        )
        .await;
    });

    sse_body_response(sse_rx)
}

fn build_openai_client_tool_item(
    call_id: String,
    name: String,
    arguments: String,
    is_custom: bool,
    emit_tool_search_items: bool,
) -> ResponseOutputItem {
    if name == INTERNAL_CODEX_LOCAL_SHELL_TOOL {
        ResponseOutputItem::LocalShellCall {
            id: format!("lsh_{}", generate_short_nonce()),
            status: "completed".to_owned(),
            call_id,
            action: parse_local_shell_action(&arguments),
        }
    } else if emit_tool_search_items && name == INTERNAL_CODEX_TOOL_SEARCH_TOOL {
        ResponseOutputItem::ToolSearchCall {
            id: format!("tsc_{}", generate_short_nonce()),
            status: "completed".to_owned(),
            call_id,
            execution: "client".to_owned(),
            arguments: serde_json::from_str::<Value>(&arguments).unwrap_or_else(|_| json!({})),
        }
    } else if is_custom {
        let (namespace, bare_name) = split_namespaced_tool_name(&name);
        ResponseOutputItem::CustomToolCall {
            id: format!("ctc_{}", generate_short_nonce()),
            status: "completed".to_owned(),
            call_id,
            namespace,
            name: bare_name,
            input: arguments,
        }
    } else {
        let (namespace, bare_name) = split_namespaced_tool_name(&name);
        ResponseOutputItem::FunctionCall {
            id: format!("fc_{}", generate_short_nonce()),
            status: "completed".to_owned(),
            call_id,
            namespace,
            name: bare_name,
            arguments,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ShellCommandField {
    Argv(Vec<String>),
    Single(String),
}

#[derive(Deserialize)]
struct RawLocalShellArgs {
    command: ShellCommandField,
    #[serde(default)]
    working_directory: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

fn parse_local_shell_action(raw: &str) -> LocalShellAction {
    if let Ok(parsed) = serde_json::from_str::<RawLocalShellArgs>(raw) {
        let command = match parsed.command {
            ShellCommandField::Argv(v) if !v.is_empty() => v,
            ShellCommandField::Argv(_) => default_shell_wrap(""),
            ShellCommandField::Single(s) => default_shell_wrap(&s),
        };
        return LocalShellAction {
            action_type: "exec".to_owned(),
            command,
            working_directory: parsed.working_directory,
            timeout_ms: parsed.timeout_ms,
            env: parsed.env,
        };
    }
    LocalShellAction {
        action_type: "exec".to_owned(),
        command: default_shell_wrap(raw.trim()),
        working_directory: None,
        timeout_ms: None,
        env: BTreeMap::new(),
    }
}

fn default_shell_wrap(cmd: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![
            "powershell.exe".to_owned(),
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            cmd.to_owned(),
        ]
    } else {
        vec!["sh".to_owned(), "-lc".to_owned(), cmd.to_owned()]
    }
}

fn build_openai_web_sources(
    links: &[liftgate_types::canonical::MarkdownLinkSpan],
) -> Vec<WebSearchSource> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for link in links {
        if seen.insert(link.url.clone()) {
            out.push(WebSearchSource {
                source_type: "url".to_owned(),
                url: link.url.clone(),
            });
        }
    }
    out
}

fn build_openai_url_annotations(text: &str) -> Vec<Value> {
    extract_markdown_links(text)
        .into_iter()
        .map(|link| {
            json!({
                "type": "url_citation",
                "start_index": link.start_char,
                "end_index": link.end_char,
                "title": link.title,
                "url": link.url,
            })
        })
        .collect()
}

async fn close_active_reasoning(
    sse_tx: &mpsc::Sender<Result<Bytes, Infallible>>,
    seq: &mut u64,
    active_reasoning: &mut Option<(usize, String, String)>,
    include_encrypted_reasoning: bool,
    finalized_items: &mut Vec<ResponseOutputItem>,
) {
    if let Some((idx, item_id, full_text)) = active_reasoning.take() {
        let _ = send_sse_frame(
            sse_tx,
            "response.reasoning_summary_text.done",
            &json!({
                "type": "response.reasoning_summary_text.done",
                "sequence_number": next_seq(seq),
                "item_id": item_id,
                "output_index": idx,
                "summary_index": 0,
                "text": full_text,
            }),
        )
        .await;
        let _ = send_sse_frame(
            sse_tx,
            "response.reasoning_summary_part.done",
            &json!({
                "type": "response.reasoning_summary_part.done",
                "sequence_number": next_seq(seq),
                "item_id": item_id,
                "output_index": idx,
                "summary_index": 0,
                "part": { "type": "summary_text", "text": full_text }
            }),
        )
        .await;
        let done_item = ResponseOutputItem::Reasoning {
            id: item_id,
            summary: vec![ReasoningSummaryPart {
                part_type: "summary_text".to_owned(),
                text: full_text,
            }],
            encrypted_content: include_encrypted_reasoning.then(|| "agy_reasoning_v1".to_owned()),
        };
        let _ = send_sse_frame(
            sse_tx,
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "sequence_number": next_seq(seq),
                "output_index": idx,
                "item": &done_item,
            }),
        )
        .await;
        finalized_items.push(done_item);
    }
}

async fn close_active_message(
    sse_tx: &mpsc::Sender<Result<Bytes, Infallible>>,
    seq: &mut u64,
    active_message: &mut Option<(usize, String, String)>,
    emit_citations: bool,
    finalized_items: &mut Vec<ResponseOutputItem>,
) {
    if let Some((idx, msg_id, full_text)) = active_message.take() {
        let annotations = if emit_citations {
            build_openai_url_annotations(&full_text)
        } else {
            Vec::new()
        };
        for (ann_idx, ann) in annotations.iter().enumerate() {
            let _ = send_sse_frame(
                sse_tx,
                "response.output_text.annotation.added",
                &json!({
                    "type": "response.output_text.annotation.added",
                    "sequence_number": next_seq(seq),
                    "item_id": msg_id,
                    "output_index": idx,
                    "content_index": 0,
                    "annotation_index": ann_idx,
                    "annotation": ann,
                }),
            )
            .await;
        }
        let _ = send_sse_frame(
            sse_tx,
            "response.output_text.done",
            &json!({
                "type": "response.output_text.done",
                "sequence_number": next_seq(seq),
                "item_id": msg_id,
                "output_index": idx,
                "content_index": 0,
                "text": full_text,
                "logprobs": []
            }),
        )
        .await;
        let _ = send_sse_frame(
            sse_tx,
            "response.content_part.done",
            &json!({
                "type": "response.content_part.done",
                "sequence_number": next_seq(seq),
                "item_id": msg_id,
                "output_index": idx,
                "content_index": 0,
                "part": {
                    "type": "output_text",
                    "text": full_text,
                    "annotations": &annotations
                }
            }),
        )
        .await;
        let done_item = ResponseOutputItem::Message {
            id: msg_id,
            status: "completed".to_owned(),
            role: "assistant".to_owned(),
            content: vec![OutputMessageContentPart {
                part_type: "output_text".to_owned(),
                text: full_text,
                annotations,
            }],
        };
        let _ = send_sse_frame(
            sse_tx,
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "sequence_number": next_seq(seq),
                "output_index": idx,
                "item": &done_item,
            }),
        )
        .await;
        finalized_items.push(done_item);
    }
}

// ============================================================================
// Anthropic `/v1/messages` & `/v1/messages/count_tokens` Handlers
// ============================================================================

#[derive(Clone, Debug)]
struct AnthropicRenderOptions {
    emit_web_search: bool,
    force_web_search: bool,
    query_hint: Option<String>,
}

async fn handle_anthropic_count_tokens<B: AcpBackend>(
    State(state): State<AppState<B>>,
    http_req: Request,
) -> Response {
    let req: AnthropicMessagesRequest = match read_authenticated_json_body(&state, http_req).await {
        Ok(parsed) => parsed,
        Err(err) => return err.into_anthropic_response(),
    };
    let input_tokens = estimate_anthropic_tokens(&req);
    Json(AnthropicCountTokensResponse { input_tokens }).into_response()
}

async fn handle_anthropic_messages<B: AcpBackend>(
    State(state): State<AppState<B>>,
    http_req: Request,
) -> Response {
    let req: AnthropicMessagesRequest = match read_authenticated_json_body(&state, http_req).await {
        Ok(parsed) => parsed,
        Err(err) => return err.into_anthropic_response(),
    };
    let requested_model_echo = req.model.clone();

    let normalized = match normalize_anthropic_request(req) {
        Ok(n) => n,
        Err(err) => return GatewayError::from(err).into_anthropic_response(),
    };

    let is_stream = normalized.stream;
    let render_opts = AnthropicRenderOptions {
        emit_web_search: normalized.enable_native_web_search,
        force_web_search: normalized.force_native_web_search,
        query_hint: extract_search_query_hint(&normalized.messages),
    };
    let msg_id = format!("msg_{}", generate_short_nonce());

    let turn_stream = match execute_normalized_turn(
        Arc::clone(&state.backend),
        Arc::clone(&state.pool),
        &state.telemetry,
        &state.config,
        TaskProtocol::AnthropicMessages,
        normalized,
        msg_id.clone(),
    )
    .await
    {
        Ok(s) => s,
        Err(err) => return err.into_anthropic_response(),
    };

    let model_name = if requested_model_echo.trim().is_empty() {
        turn_stream.resolved_model.as_str().to_owned()
    } else {
        requested_model_echo
    };

    if is_stream {
        stream_anthropic_messages(
            turn_stream,
            msg_id,
            model_name,
            render_opts,
            state.config.limits.channel_capacity().get(),
        )
    } else {
        collect_anthropic_messages(turn_stream, msg_id, model_name, render_opts).await
    }
}

async fn collect_anthropic_messages(
    mut turn: ActiveTurnStream,
    msg_id: String,
    model_name: String,
    opts: AnthropicRenderOptions,
) -> Response {
    let mut thinking_text = String::new();
    let mut content: Vec<AnthropicOutputBlock> = Vec::new();
    let mut current_text = String::new();
    let mut tool_uses: Vec<AnthropicOutputBlock> = Vec::new();
    let mut seen_searches: HashMap<String, Option<String>> = HashMap::new();
    let mut search_order: Vec<String> = Vec::new();
    let mut web_search_requests: u64 = 0;
    let mut web_fetch_requests: u64 = 0;
    let mut final_usage = AcpUsage::default();

    while let Some(ev) = turn.rx.recv().await {
        match ev {
            TurnEvent::ThoughtDelta(delta) => thinking_text.push_str(&delta),
            TurnEvent::WebSearch {
                call_id,
                status,
                query,
            } => {
                if opts.emit_web_search {
                    match seen_searches.entry(call_id.clone()) {
                        std::collections::hash_map::Entry::Vacant(v) => {
                            search_order.push(call_id);
                            v.insert(query);
                            web_search_requests = web_search_requests.saturating_add(1);
                        }
                        std::collections::hash_map::Entry::Occupied(mut o) => {
                            if query.is_some() {
                                o.insert(query);
                            }
                        }
                    }
                } else if status == "completed" {
                    web_search_requests = web_search_requests.saturating_add(1);
                }
            }
            TurnEvent::WebFetchStart { call_id, url } => {
                if !current_text.trim().is_empty() {
                    content.push(AnthropicOutputBlock::Text {
                        text: current_text.trim().to_owned(),
                    });
                    current_text.clear();
                }
                web_fetch_requests = web_fetch_requests.saturating_add(1);
                content.push(AnthropicOutputBlock::ServerToolUse {
                    id: ensure_srvtoolu_id(&call_id),
                    name: "web_fetch".to_owned(),
                    input: json!({ "url": url }),
                });
            }
            TurnEvent::WebFetchComplete { call_id, outcome } => {
                content.push(AnthropicOutputBlock::WebFetchToolResult {
                    tool_use_id: ensure_srvtoolu_id(&call_id),
                    content: build_anthropic_web_fetch_result_content(&outcome),
                });
            }
            TurnEvent::TextDelta(delta) => current_text.push_str(&delta),
            TurnEvent::ClientToolCallStart { .. } | TurnEvent::ClientToolCallDelta { .. } => {}
            TurnEvent::ClientToolCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                let input_val: Value = serde_json::from_str(&arguments)
                    .unwrap_or_else(|_| json!({ "raw": arguments }));
                tool_uses.push(AnthropicOutputBlock::ToolUse {
                    id: format!(
                        "toolu_{}",
                        call_id.strip_prefix("call_").unwrap_or(&call_id)
                    ),
                    name,
                    input: input_val,
                });
            }
            TurnEvent::Completed { usage, .. } => {
                final_usage = usage;
                break;
            }
            TurnEvent::Failed(err) => {
                return GatewayError::SharedAcp { source: err }.into_anthropic_response();
            }
        }
    }

    if opts.emit_web_search && opts.force_web_search && search_order.is_empty() {
        let synth_id = generate_short_nonce();
        search_order.push(synth_id.clone());
        seen_searches.insert(synth_id, opts.query_hint.clone());
        web_search_requests = 1;
    }

    let mut ordered_blocks = Vec::new();
    if !thinking_text.is_empty() {
        ordered_blocks.push(AnthropicOutputBlock::Thinking {
            thinking: thinking_text,
            signature: ANTHROPIC_THOUGHT_SIGNATURE.to_owned(),
        });
    }

    if !search_order.is_empty() {
        let search_results = build_anthropic_web_search_results(&current_text);
        for cid in &search_order {
            let srv_id = ensure_srvtoolu_id(cid);
            let q = seen_searches
                .get(cid)
                .and_then(Clone::clone)
                .or_else(|| opts.query_hint.clone())
                .unwrap_or_default();
            ordered_blocks.push(AnthropicOutputBlock::ServerToolUse {
                id: srv_id.clone(),
                name: "web_search".to_owned(),
                input: json!({ "query": q }),
            });
            ordered_blocks.push(AnthropicOutputBlock::WebSearchToolResult {
                tool_use_id: srv_id,
                content: search_results.clone(),
            });
        }
    }

    ordered_blocks.extend(content);

    let has_tools = !tool_uses.is_empty();
    let trimmed = current_text.trim();
    if !trimmed.is_empty() || (!has_tools && ordered_blocks.is_empty()) {
        ordered_blocks.push(AnthropicOutputBlock::Text {
            text: if has_tools {
                trimmed.to_owned()
            } else {
                current_text
            },
        });
    }
    ordered_blocks.extend(tool_uses);

    let stop_reason = if has_tools {
        Cow::Borrowed("tool_use")
    } else {
        Cow::Borrowed("end_turn")
    };

    let mut usage = AnthropicUsage::from(final_usage);
    if web_search_requests > 0 || web_fetch_requests > 0 {
        usage.server_tool_use = Some(AnthropicServerToolUsage {
            web_search_requests,
            web_fetch_requests,
        });
    }

    Json(AnthropicMessagesResponse {
        id: msg_id,
        object_type: Cow::Borrowed("message"),
        role: Cow::Borrowed("assistant"),
        model: model_name,
        content: ordered_blocks,
        stop_reason: Some(stop_reason),
        stop_sequence: None,
        usage,
    })
    .into_response()
}

fn stream_anthropic_messages(
    mut turn: ActiveTurnStream,
    msg_id: String,
    model_name: String,
    opts: AnthropicRenderOptions,
    channel_cap: usize,
) -> Response {
    let (sse_tx, sse_rx) = mpsc::channel::<Result<Bytes, Infallible>>(channel_cap);

    tokio::spawn(async move {
        let mut message_started = false;
        let mut next_block_idx: usize = 0;
        let mut active_thinking_idx: Option<usize> = None;
        let mut active_text_idx: Option<usize> = None;
        let mut leading_ws = String::new();
        let mut emitted_any_block = false;
        let mut has_tool_use = false;
        let mut emitted_search_srv_ids: HashSet<String> = HashSet::new();
        let mut pending_search_result_srv_ids: Vec<String> = Vec::new();
        let mut open_tool_blocks: HashMap<String, usize> = HashMap::new();
        let mut post_search_text_buf = String::new();
        let mut web_search_requests: u64 = 0;
        let mut web_fetch_requests: u64 = 0;
        let mut final_usage = AcpUsage::default();

        loop {
            let ev = tokio::select! {
                biased;
                () = sse_tx.closed() => return,
                maybe_ev = turn.rx.recv() => match maybe_ev {
                    Some(ev) => ev,
                    None => break,
                },
            };
            match ev {
                TurnEvent::ThoughtDelta(delta) => {
                    if active_text_idx.is_some()
                        || !pending_search_result_srv_ids.is_empty()
                        || !post_search_text_buf.is_empty()
                    {
                        continue;
                    }
                    let idx = match active_thinking_idx {
                        Some(idx) => idx,
                        None => {
                            if !ensure_anthropic_message_started(
                                &sse_tx,
                                &mut message_started,
                                &msg_id,
                                &model_name,
                            )
                            .await
                            {
                                return;
                            }
                            let idx = next_block_idx;
                            next_block_idx += 1;
                            active_thinking_idx = Some(idx);
                            emitted_any_block = true;
                            let _ = send_sse_frame(
                                &sse_tx,
                                "content_block_start",
                                &json!({
                                    "type": "content_block_start",
                                    "index": idx,
                                    "content_block": { "type": "thinking", "thinking": "" }
                                }),
                            )
                            .await;
                            idx
                        }
                    };
                    if !send_sse_frame(
                        &sse_tx,
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": idx,
                            "delta": { "type": "thinking_delta", "thinking": delta }
                        }),
                    )
                    .await
                    {
                        return;
                    }
                }
                TurnEvent::WebSearch {
                    call_id,
                    status,
                    query,
                } => {
                    if opts.emit_web_search {
                        let srv_id = ensure_srvtoolu_id(&call_id);
                        if emitted_search_srv_ids.insert(srv_id.clone()) {
                            if !ensure_anthropic_message_started(
                                &sse_tx,
                                &mut message_started,
                                &msg_id,
                                &model_name,
                            )
                            .await
                            {
                                return;
                            }
                            close_anthropic_thinking(&sse_tx, &mut active_thinking_idx).await;
                            close_anthropic_text(&sse_tx, &mut active_text_idx).await;

                            let resolved_q = query
                                .or_else(|| opts.query_hint.clone())
                                .unwrap_or_default();
                            emit_anthropic_server_tool_use(
                                &sse_tx,
                                &mut next_block_idx,
                                &srv_id,
                                "web_search",
                                &json!({ "query": resolved_q }),
                            )
                            .await;
                            emitted_any_block = true;
                            web_search_requests = web_search_requests.saturating_add(1);
                            pending_search_result_srv_ids.push(srv_id);
                        }
                    } else if status == "completed" {
                        web_search_requests = web_search_requests.saturating_add(1);
                    }
                }
                TurnEvent::WebFetchStart { call_id, url } => {
                    if !ensure_anthropic_message_started(
                        &sse_tx,
                        &mut message_started,
                        &msg_id,
                        &model_name,
                    )
                    .await
                    {
                        return;
                    }
                    flush_pending_anthropic_searches(
                        &sse_tx,
                        &mut next_block_idx,
                        &mut pending_search_result_srv_ids,
                        &mut post_search_text_buf,
                        &mut emitted_any_block,
                    )
                    .await;
                    close_anthropic_thinking(&sse_tx, &mut active_thinking_idx).await;
                    close_anthropic_text(&sse_tx, &mut active_text_idx).await;

                    let srv_id = ensure_srvtoolu_id(&call_id);
                    emit_anthropic_server_tool_use(
                        &sse_tx,
                        &mut next_block_idx,
                        &srv_id,
                        "web_fetch",
                        &json!({ "url": url }),
                    )
                    .await;
                    emitted_any_block = true;
                    web_fetch_requests = web_fetch_requests.saturating_add(1);
                }
                TurnEvent::WebFetchComplete { call_id, outcome } => {
                    if !ensure_anthropic_message_started(
                        &sse_tx,
                        &mut message_started,
                        &msg_id,
                        &model_name,
                    )
                    .await
                    {
                        return;
                    }
                    let srv_id = ensure_srvtoolu_id(&call_id);
                    let result_content = build_anthropic_web_fetch_result_content(&outcome);
                    let idx = next_block_idx;
                    next_block_idx += 1;
                    emitted_any_block = true;
                    let _ = send_sse_frame(
                        &sse_tx,
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": idx,
                            "content_block": {
                                "type": "web_fetch_tool_result",
                                "tool_use_id": srv_id,
                                "content": result_content
                            }
                        }),
                    )
                    .await;
                    let _ = send_sse_frame(
                        &sse_tx,
                        "content_block_stop",
                        &json!({ "type": "content_block_stop", "index": idx }),
                    )
                    .await;
                }
                TurnEvent::TextDelta(delta) => {
                    close_anthropic_thinking(&sse_tx, &mut active_thinking_idx).await;

                    // If a forced web search turn produced grounded prose directly without a
                    // separate ACP `ToolCall` notification, synthesize the `server_tool_use`
                    // block so Claude Code always records `Did 1 search` + extracted sources.
                    if opts.emit_web_search
                        && opts.force_web_search
                        && web_search_requests == 0
                        && pending_search_result_srv_ids.is_empty()
                    {
                        if !ensure_anthropic_message_started(
                            &sse_tx,
                            &mut message_started,
                            &msg_id,
                            &model_name,
                        )
                        .await
                        {
                            return;
                        }
                        close_anthropic_text(&sse_tx, &mut active_text_idx).await;
                        let srv_id = format!("srvtoolu_{}", generate_short_nonce());
                        let q = opts.query_hint.clone().unwrap_or_default();
                        emit_anthropic_server_tool_use(
                            &sse_tx,
                            &mut next_block_idx,
                            &srv_id,
                            "web_search",
                            &json!({ "query": q }),
                        )
                        .await;
                        emitted_any_block = true;
                        web_search_requests = 1;
                        pending_search_result_srv_ids.push(srv_id);
                    }

                    // Buffer post-search prose until completion so we can populate
                    // `web_search_tool_result.content` with actual `[Title](https://...)` hits
                    // right before emitting the `text` content block.
                    if !pending_search_result_srv_ids.is_empty() {
                        post_search_text_buf.push_str(&delta);
                        continue;
                    }

                    if active_text_idx.is_none() && delta.trim().is_empty() {
                        leading_ws.push_str(&delta);
                        continue;
                    }
                    let effective = if !leading_ws.is_empty() {
                        let mut combined = std::mem::take(&mut leading_ws);
                        combined.push_str(&delta);
                        combined
                    } else {
                        delta
                    };
                    let idx = match active_text_idx {
                        Some(idx) => idx,
                        None => {
                            if !ensure_anthropic_message_started(
                                &sse_tx,
                                &mut message_started,
                                &msg_id,
                                &model_name,
                            )
                            .await
                            {
                                return;
                            }
                            let idx = next_block_idx;
                            next_block_idx += 1;
                            active_text_idx = Some(idx);
                            emitted_any_block = true;
                            let _ = send_sse_frame(
                                &sse_tx,
                                "content_block_start",
                                &json!({
                                    "type": "content_block_start",
                                    "index": idx,
                                    "content_block": { "type": "text", "text": "" }
                                }),
                            )
                            .await;
                            idx
                        }
                    };
                    if !send_sse_frame(
                        &sse_tx,
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": idx,
                            "delta": { "type": "text_delta", "text": effective }
                        }),
                    )
                    .await
                    {
                        return;
                    }
                }
                TurnEvent::ClientToolCallStart { call_id, name, .. } => {
                    if !ensure_anthropic_message_started(
                        &sse_tx,
                        &mut message_started,
                        &msg_id,
                        &model_name,
                    )
                    .await
                    {
                        return;
                    }
                    leading_ws.clear();
                    flush_pending_anthropic_searches(
                        &sse_tx,
                        &mut next_block_idx,
                        &mut pending_search_result_srv_ids,
                        &mut post_search_text_buf,
                        &mut emitted_any_block,
                    )
                    .await;
                    close_anthropic_thinking(&sse_tx, &mut active_thinking_idx).await;
                    close_anthropic_text(&sse_tx, &mut active_text_idx).await;

                    has_tool_use = true;
                    emitted_any_block = true;
                    let idx = next_block_idx;
                    next_block_idx += 1;
                    let toolu_id = format!(
                        "toolu_{}",
                        call_id.strip_prefix("call_").unwrap_or(&call_id)
                    );
                    if !send_sse_frame(
                        &sse_tx,
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": idx,
                            "content_block": {
                                "type": "tool_use",
                                "id": toolu_id,
                                "name": name,
                                "input": {}
                            }
                        }),
                    )
                    .await
                    {
                        return;
                    }
                    open_tool_blocks.insert(call_id, idx);
                }
                TurnEvent::ClientToolCallDelta { .. } => {}
                TurnEvent::ClientToolCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } => {
                    if !ensure_anthropic_message_started(
                        &sse_tx,
                        &mut message_started,
                        &msg_id,
                        &model_name,
                    )
                    .await
                    {
                        return;
                    }
                    leading_ws.clear();
                    flush_pending_anthropic_searches(
                        &sse_tx,
                        &mut next_block_idx,
                        &mut pending_search_result_srv_ids,
                        &mut post_search_text_buf,
                        &mut emitted_any_block,
                    )
                    .await;
                    close_anthropic_thinking(&sse_tx, &mut active_thinking_idx).await;
                    close_anthropic_text(&sse_tx, &mut active_text_idx).await;

                    has_tool_use = true;
                    emitted_any_block = true;
                    let idx = if let Some(opened_idx) = open_tool_blocks.remove(&call_id) {
                        opened_idx
                    } else {
                        let idx = next_block_idx;
                        next_block_idx += 1;
                        let toolu_id = format!(
                            "toolu_{}",
                            call_id.strip_prefix("call_").unwrap_or(&call_id)
                        );
                        let _ = send_sse_frame(
                            &sse_tx,
                            "content_block_start",
                            &json!({
                                "type": "content_block_start",
                                "index": idx,
                                "content_block": {
                                    "type": "tool_use",
                                    "id": toolu_id,
                                    "name": name,
                                    "input": {}
                                }
                            }),
                        )
                        .await;
                        idx
                    };
                    let partial_json = match serde_json::from_str::<Value>(&arguments) {
                        Ok(Value::Object(_)) => arguments,
                        _ => json!({ "raw": arguments }).to_string(),
                    };
                    let _ = send_sse_frame(
                        &sse_tx,
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": idx,
                            "delta": {
                                "type": "input_json_delta",
                                "partial_json": partial_json
                            }
                        }),
                    )
                    .await;
                    let _ = send_sse_frame(
                        &sse_tx,
                        "content_block_stop",
                        &json!({
                            "type": "content_block_stop",
                            "index": idx
                        }),
                    )
                    .await;
                }
                TurnEvent::Completed { usage, .. } => {
                    final_usage = usage;
                    break;
                }
                TurnEvent::Failed(acp_err) => {
                    let gw_err = GatewayError::SharedAcp { source: acp_err };
                    tracing::error!(
                        error = ?gw_err,
                        message_started,
                        "Anthropic Messages SSE stream failed"
                    );
                    let error_type = gw_err.anthropic_error_type();
                    let _ = send_sse_frame(
                        &sse_tx,
                        "error",
                        &json!({
                            "type": "error",
                            "error": {
                                "type": error_type,
                                "message": gw_err.anthropic_client_message()
                            }
                        }),
                    )
                    .await;
                    return;
                }
            }
        }

        if !open_tool_blocks.is_empty() {
            let gw_err = GatewayError::SharedAcp {
                source: Arc::new(AcpError::UnclosedToolCallStream {
                    protocol: "Anthropic Messages",
                }),
            };
            tracing::error!(
                error = ?gw_err,
                open_blocks = open_tool_blocks.len(),
                "Anthropic Messages SSE stream ended with unclosed tool_use block"
            );
            let error_type = gw_err.anthropic_error_type();
            let _ = send_sse_frame(
                &sse_tx,
                "error",
                &json!({
                    "type": "error",
                    "error": {
                        "type": error_type,
                        "message": gw_err.anthropic_client_message()
                    }
                }),
            )
            .await;
            return;
        }

        if !ensure_anthropic_message_started(&sse_tx, &mut message_started, &msg_id, &model_name)
            .await
        {
            return;
        }

        flush_pending_anthropic_searches(
            &sse_tx,
            &mut next_block_idx,
            &mut pending_search_result_srv_ids,
            &mut post_search_text_buf,
            &mut emitted_any_block,
        )
        .await;
        close_anthropic_thinking(&sse_tx, &mut active_thinking_idx).await;
        close_anthropic_text(&sse_tx, &mut active_text_idx).await;

        if !emitted_any_block {
            let idx = next_block_idx;
            let _ = send_sse_frame(
                &sse_tx,
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": { "type": "text", "text": "" }
                }),
            )
            .await;
            if !leading_ws.is_empty() {
                let _ = send_sse_frame(
                    &sse_tx,
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": { "type": "text_delta", "text": leading_ws }
                    }),
                )
                .await;
            }
            let _ = send_sse_frame(
                &sse_tx,
                "content_block_stop",
                &json!({ "type": "content_block_stop", "index": idx }),
            )
            .await;
        }

        let stop_reason = if has_tool_use { "tool_use" } else { "end_turn" };
        let mut usage = AnthropicUsage::from(final_usage);
        if web_search_requests > 0 || web_fetch_requests > 0 {
            usage.server_tool_use = Some(AnthropicServerToolUsage {
                web_search_requests,
                web_fetch_requests,
            });
        }

        let _ = send_sse_frame(
            &sse_tx,
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason,
                    "stop_sequence": null
                },
                "usage": usage
            }),
        )
        .await;
        let _ = send_sse_frame(&sse_tx, "message_stop", &json!({ "type": "message_stop" })).await;
    });

    sse_body_response(sse_rx)
}

async fn ensure_anthropic_message_started(
    sse_tx: &mpsc::Sender<Result<Bytes, Infallible>>,
    message_started: &mut bool,
    msg_id: &str,
    model_name: &str,
) -> bool {
    if *message_started {
        return true;
    }
    *message_started = true;
    send_sse_frame(
        sse_tx,
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "model": model_name,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "output_tokens": 1
                }
            }
        }),
    )
    .await
}

async fn emit_anthropic_server_tool_use(
    sse_tx: &mpsc::Sender<Result<Bytes, Infallible>>,
    next_block_idx: &mut usize,
    srv_id: &str,
    tool_name: &str,
    input_obj: &Value,
) {
    let idx = *next_block_idx;
    *next_block_idx += 1;
    let input_json = serde_json::to_string(input_obj).unwrap_or_else(|_| "{}".to_owned());
    let _ = send_sse_frame(
        sse_tx,
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": idx,
            "content_block": {
                "type": "server_tool_use",
                "id": srv_id,
                "name": tool_name,
                "input": {}
            }
        }),
    )
    .await;
    let _ = send_sse_frame(
        sse_tx,
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": idx,
            "delta": {
                "type": "input_json_delta",
                "partial_json": input_json
            }
        }),
    )
    .await;
    let _ = send_sse_frame(
        sse_tx,
        "content_block_stop",
        &json!({ "type": "content_block_stop", "index": idx }),
    )
    .await;
}

async fn flush_pending_anthropic_searches(
    sse_tx: &mpsc::Sender<Result<Bytes, Infallible>>,
    next_block_idx: &mut usize,
    pending_srv_ids: &mut Vec<String>,
    buffered_text: &mut String,
    emitted_any_block: &mut bool,
) {
    if pending_srv_ids.is_empty() {
        return;
    }
    let results = build_anthropic_web_search_results(buffered_text);
    for srv_id in pending_srv_ids.drain(..) {
        let idx = *next_block_idx;
        *next_block_idx += 1;
        *emitted_any_block = true;
        let _ = send_sse_frame(
            sse_tx,
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {
                    "type": "web_search_tool_result",
                    "tool_use_id": srv_id,
                    "content": &results
                }
            }),
        )
        .await;
        let _ = send_sse_frame(
            sse_tx,
            "content_block_stop",
            &json!({ "type": "content_block_stop", "index": idx }),
        )
        .await;
    }

    let trimmed = buffered_text.trim();
    if !trimmed.is_empty() {
        let text_to_emit = std::mem::take(buffered_text);
        let idx = *next_block_idx;
        *next_block_idx += 1;
        *emitted_any_block = true;
        let _ = send_sse_frame(
            sse_tx,
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": { "type": "text", "text": "" }
            }),
        )
        .await;
        let _ = send_sse_frame(
            sse_tx,
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": idx,
                "delta": { "type": "text_delta", "text": text_to_emit }
            }),
        )
        .await;
        let _ = send_sse_frame(
            sse_tx,
            "content_block_stop",
            &json!({ "type": "content_block_stop", "index": idx }),
        )
        .await;
    } else {
        buffered_text.clear();
    }
}

fn build_anthropic_web_search_results(text: &str) -> Vec<AnthropicWebSearchResultItem> {
    let mut seen_urls = HashSet::new();
    let mut items = Vec::new();
    for link in extract_markdown_links(text) {
        if seen_urls.insert(link.url.clone()) {
            items.push(AnthropicWebSearchResultItem {
                item_type: Cow::Borrowed("web_search_result"),
                title: link.title,
                url: link.url,
                encrypted_content: "agy_grounded_v1".to_owned(),
                page_age: None,
            });
            if items.len() >= 10 {
                break;
            }
        }
    }
    items
}

fn build_anthropic_web_fetch_result_content(outcome: &WebFetchOutcome) -> Value {
    match outcome {
        WebFetchOutcome::Success {
            url,
            title,
            media_type,
            content,
        } => json!({
            "type": "web_fetch_result",
            "url": url,
            "retrieved_at": iso8601_utc_now(),
            "content": {
                "type": "document",
                "title": title,
                "source": {
                    "type": "text",
                    "media_type": media_type,
                    "data": content
                }
            }
        }),
        WebFetchOutcome::Error { error_code, .. } => json!({
            "type": "web_fetch_tool_error",
            "error_code": error_code
        }),
    }
}

fn sanitize_call_suffix(raw: &str) -> String {
    let stripped = raw
        .strip_prefix("srvtoolu_")
        .or_else(|| raw.strip_prefix("call_"))
        .unwrap_or(raw);
    let cleaned: String = stripped
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if cleaned.is_empty() {
        generate_short_nonce()
    } else {
        cleaned
    }
}

fn ensure_srvtoolu_id(raw: &str) -> String {
    if raw.starts_with("srvtoolu_") {
        raw.to_owned()
    } else {
        format!("srvtoolu_{}", sanitize_call_suffix(raw))
    }
}

async fn close_anthropic_thinking(
    sse_tx: &mpsc::Sender<Result<Bytes, Infallible>>,
    active_thinking_idx: &mut Option<usize>,
) {
    if let Some(idx) = active_thinking_idx.take() {
        let _ = send_sse_frame(
            sse_tx,
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": idx,
                "delta": {
                    "type": "signature_delta",
                    "signature": ANTHROPIC_THOUGHT_SIGNATURE
                }
            }),
        )
        .await;
        let _ = send_sse_frame(
            sse_tx,
            "content_block_stop",
            &json!({ "type": "content_block_stop", "index": idx }),
        )
        .await;
    }
}

async fn close_anthropic_text(
    sse_tx: &mpsc::Sender<Result<Bytes, Infallible>>,
    active_text_idx: &mut Option<usize>,
) {
    if let Some(idx) = active_text_idx.take() {
        let _ = send_sse_frame(
            sse_tx,
            "content_block_stop",
            &json!({ "type": "content_block_stop", "index": idx }),
        )
        .await;
    }
}

// ============================================================================
// Shared Auth, Time & SSE Framing Helpers
// ============================================================================

fn check_auth(headers: &HeaderMap, expected_key: Option<&str>) -> Result<(), GatewayError> {
    let Some(expected) = expected_key.filter(|k| !k.is_empty()) else {
        return Ok(());
    };

    if let Some(x_key) = headers.get("x-api-key").and_then(|v| v.to_str().ok())
        && x_key.trim() == expected
    {
        return Ok(());
    }

    if let Some(auth) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        && auth
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
        && let Some(token) = auth.get(7..)
        && token.trim() == expected
    {
        return Ok(());
    }

    Err(GatewayError::Unauthorized)
}

fn parse_content_length_hint(
    headers: &HeaderMap,
    max_body: usize,
) -> Result<Option<usize>, GatewayError> {
    let Some(raw) = headers.get(header::CONTENT_LENGTH) else {
        return Ok(None);
    };
    let trimmed = raw
        .to_str()
        .map_err(|_| RequestBodyError::InvalidContentLength)?
        .trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return Err(RequestBodyError::InvalidContentLength.into());
    }
    let len = trimmed
        .parse::<usize>()
        .map_err(|_| GatewayError::PayloadTooLarge {
            max_bytes: max_body,
        })?;
    if len > max_body {
        return Err(GatewayError::PayloadTooLarge {
            max_bytes: max_body,
        });
    }
    Ok(Some(len))
}

async fn read_authenticated_json_body<T: serde::de::DeserializeOwned, B: AcpBackend>(
    state: &AppState<B>,
    req: Request,
) -> Result<T, GatewayError> {
    check_auth(req.headers(), state.config.api_key.as_deref())?;

    let max_body = state.config.limits.max_body_bytes().get();
    let header_length_hint = parse_content_length_hint(req.headers(), max_body)?;

    let timeout_secs = state.config.limits.turn_timeout_secs().get();
    let turn_timeout = Duration::from_secs(timeout_secs);
    let read_deadline = tokio::time::Instant::now() + turn_timeout.min(MAX_BODY_READ_TIMEOUT);

    let mut body = req.into_body();
    let body_exact_hint = match axum::body::HttpBody::size_hint(&body).exact() {
        Some(exact_u64) => {
            let exact = usize::try_from(exact_u64).map_err(|_| GatewayError::PayloadTooLarge {
                max_bytes: max_body,
            })?;
            if exact > max_body {
                return Err(GatewayError::PayloadTooLarge {
                    max_bytes: max_body,
                });
            }
            Some(exact)
        }
        None => None,
    };
    let exact_length_hint = header_length_hint.or(body_exact_hint);

    let mut buf = Vec::with_capacity(
        exact_length_hint
            .unwrap_or(0)
            .min(INITIAL_BODY_ALLOC_CAP_BYTES),
    );

    loop {
        let next_frame =
            tokio::time::timeout_at(read_deadline, http_body_util::BodyExt::frame(&mut body))
                .await
                .map_err(|_| RequestBodyError::ReadTimeout)?;
        let Some(frame_res) = next_frame else {
            break;
        };
        let frame = frame_res.map_err(|source| RequestBodyError::ReadFrame { source })?;
        let Ok(chunk) = frame.into_data() else {
            continue;
        };
        if chunk.is_empty() {
            continue;
        }
        let next_len = buf.len().saturating_add(chunk.len());
        if next_len > max_body {
            return Err(GatewayError::PayloadTooLarge {
                max_bytes: max_body,
            });
        }
        if let Some(exact) = exact_length_hint
            && next_len > exact
        {
            return Err(RequestBodyError::ExceededContentLength.into());
        }
        buf.extend_from_slice(&chunk);
    }

    if let Some(exact) = header_length_hint
        && buf.len() != exact
    {
        return Err(RequestBodyError::ContentLengthMismatch.into());
    }

    Ok(serde_json::from_slice(&buf).map_err(RequestBodyError::from)?)
}

const fn next_seq(seq: &mut u64) -> u64 {
    let cur = *seq;
    *seq += 1;
    cur
}

async fn send_sse_frame<T: serde::Serialize>(
    tx: &mpsc::Sender<Result<Bytes, Infallible>>,
    event_name: &str,
    payload: &T,
) -> bool {
    let Ok(json_str) = serde_json::to_string(payload) else {
        return false;
    };
    let mut frame = String::with_capacity(event_name.len() + json_str.len() + 16);
    frame.push_str("event: ");
    frame.push_str(event_name);
    frame.push_str("\ndata: ");
    frame.push_str(&json_str);
    frame.push_str("\n\n");
    tx.send(Ok(Bytes::from(frame))).await.is_ok()
}

fn sse_body_response(rx: mpsc::Receiver<Result<Bytes, Infallible>>) -> Response {
    let (hb_tx, hb_rx) = mpsc::channel::<Result<Bytes, Infallible>>(32);
    tokio::spawn(async move {
        let mut rx = rx;
        let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Consume the immediate initial tick so normal fast streams emit zero extra frames.
        let _ = heartbeat.tick().await;
        loop {
            tokio::select! {
                biased;
                () = hb_tx.closed() => break,
                maybe_frame = rx.recv() => {
                    match maybe_frame {
                        Some(frame) => {
                            if hb_tx.send(frame).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = heartbeat.tick() => {
                    if hb_tx.send(Ok(Bytes::from_static(b": keep-alive\n\n"))).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    let body = axum::body::Body::from_stream(ReceiverStream::new(hb_rx));
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/event-stream; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::CONNECTION, "keep-alive"),
        ],
        body,
    )
        .into_response()
}

fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn iso8601_utc_now() -> String {
    let total_secs = unix_timestamp_secs();
    let days = i64::try_from(total_secs / 86_400).unwrap_or(0);
    let rem = total_secs % 86_400;
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = u64::try_from(z - era * 146_097).unwrap_or(0);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = i64::try_from(yoe).unwrap_or(0) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

// ============================================================================
// Companion Frontend Telemetry Endpoints, Static SPA Hosting & CORS Middleware
// ============================================================================

fn extract_query_api_key(uri: &Uri) -> Option<&str> {
    let query = uri.query()?;
    for pair in query.split('&') {
        if let Some(val) = pair.strip_prefix("api_key=") {
            return Some(val);
        }
    }
    None
}

fn check_auth_with_query(
    headers: &HeaderMap,
    uri: &Uri,
    expected_key: Option<&str>,
) -> Result<(), GatewayError> {
    if check_auth(headers, expected_key).is_ok() {
        return Ok(());
    }
    if let (Some(q), Some(exp)) = (
        extract_query_api_key(uri),
        expected_key.filter(|k| !k.is_empty()),
    ) && q.trim() == exp
    {
        return Ok(());
    }
    Err(GatewayError::Unauthorized)
}

fn build_telemetry_snapshot<B: AcpBackend>(state: &AppState<B>) -> GatewayTelemetrySnapshot {
    let acp_process = state.backend.process_telemetry(
        state.config.default_model.as_str(),
        state.config.acp_mode.as_str(),
    );
    let session_pool = state.pool.snapshot();
    let (metrics, cache_telemetry, active_tasks, recent_tasks) = state.telemetry.snapshot_tasks();
    let available_models = state.backend.available_models();

    GatewayTelemetrySnapshot {
        generated_at_unix_ms: unix_timestamp_ms(),
        gateway_version: Cow::Borrowed(env!("CARGO_PKG_VERSION")),
        vision_offload_enabled: state.config.vision_offload.load(Ordering::Relaxed),
        mcp_tool_bridge_enabled: state.config.mcp_tool_bridge.load(Ordering::Relaxed),
        acp_process,
        session_pool,
        metrics,
        cache_telemetry,
        active_tasks,
        recent_tasks,
        available_models,
    }
}

async fn handle_telemetry_snapshot<B: AcpBackend>(
    State(state): State<AppState<B>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Err(err) = check_auth_with_query(&headers, &uri, state.config.api_key.as_deref()) {
        return err.into_openai_response();
    }
    Json(build_telemetry_snapshot(&state)).into_response()
}

async fn handle_telemetry_events<B: AcpBackend>(
    State(state): State<AppState<B>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Err(err) = check_auth_with_query(&headers, &uri, state.config.api_key.as_deref()) {
        return err.into_openai_response();
    }

    let cap = state.config.limits.channel_capacity().get();
    let (sse_tx, sse_rx) = mpsc::channel::<Result<Bytes, Infallible>>(cap);
    let mut notify_rx = state.telemetry.subscribe();
    let mut shutdown_rx = state.telemetry.subscribe_shutdown();
    if *shutdown_rx.borrow() {
        return sse_body_response(sse_rx);
    }

    tokio::spawn(async move {
        let initial = build_telemetry_snapshot(&state);
        if !send_sse_frame(&sse_tx, "snapshot", &initial).await {
            return;
        }

        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let _ = tick.tick().await;

        loop {
            tokio::select! {
                biased;
                () = sse_tx.closed() => return,
                () = wait_shutdown_signal(&mut shutdown_rx) => return,
                recv_res = notify_rx.recv() => {
                    if matches!(recv_res, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                        return;
                    }
                    let snap = build_telemetry_snapshot(&state);
                    if !send_sse_frame(&sse_tx, "snapshot", &snap).await {
                        return;
                    }
                }
                _ = tick.tick() => {
                    let snap = build_telemetry_snapshot(&state);
                    if !send_sse_frame(&sse_tx, "snapshot", &snap).await {
                        return;
                    }
                }
            }
        }
    });

    sse_body_response(sse_rx)
}

async fn wait_shutdown_signal(rx: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = rx.wait_for(|&stop| stop).await;
}

async fn handle_close_idle_session<B: AcpBackend>(
    State(state): State<AppState<B>>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    uri: Uri,
) -> Response {
    if let Err(err) = check_auth_with_query(&headers, &uri, state.config.api_key.as_deref()) {
        return err.into_openai_response();
    }

    let closed = state.pool.close_idle_session(&session_id, &*state.backend);
    if closed {
        state.telemetry.notify_change();
    }
    Json(json!({ "closed": closed, "session_id": session_id })).into_response()
}

#[derive(Deserialize)]
struct VisionOffloadTogglePayload {
    #[serde(default)]
    enabled: Option<bool>,
}

fn parse_query_bool_param(uri: &Uri, param_key: &str) -> Result<Option<bool>, &'static str> {
    let Some(query) = uri.query() else {
        return Ok(None);
    };
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        if k == param_key {
            return match v.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "on" | "yes" => Ok(Some(true)),
                "0" | "false" | "off" | "no" => Ok(Some(false)),
                _ => Err("invalid boolean query value (expected `true` or `false`)"),
            };
        }
    }
    Ok(None)
}

async fn handle_set_vision_offload<B: AcpBackend>(
    State(state): State<AppState<B>>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Err(err) = check_auth_with_query(&headers, &uri, state.config.api_key.as_deref()) {
        return err.into_openai_response();
    }

    let query_opt = match parse_query_bool_param(&uri, "enabled") {
        Ok(opt) => opt,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": msg,
                        "type": "invalid_request_error",
                    }
                })),
            )
                .into_response();
        }
    };
    let body_opt = if query_opt.is_none() && !body.iter().all(u8::is_ascii_whitespace) {
        match serde_json::from_slice::<VisionOffloadTogglePayload>(&body) {
            Ok(parsed) => parsed.enabled,
            Err(err) => {
                return GatewayError::from(RequestBodyError::from(err)).into_openai_response();
            }
        }
    } else {
        None
    };

    let next = match query_opt.or(body_opt) {
        Some(explicit) => {
            state
                .config
                .vision_offload
                .store(explicit, Ordering::Release);
            explicit
        }
        None => !state
            .config
            .vision_offload
            .fetch_xor(true, Ordering::AcqRel),
    };
    if let Some(ref settings_path) = state.config.settings_path {
        crate::telemetry::sync_settings_json_bool_field(settings_path, "vision_offload", next);
    }
    state.telemetry.notify_change();
    Json(json!({ "vision_offload_enabled": next })).into_response()
}

async fn handle_set_mcp_tool_bridge<B: AcpBackend>(
    State(state): State<AppState<B>>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Err(err) = check_auth_with_query(&headers, &uri, state.config.api_key.as_deref()) {
        return err.into_openai_response();
    }

    let query_opt = match parse_query_bool_param(&uri, "enabled") {
        Ok(opt) => opt,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": msg,
                        "type": "invalid_request_error",
                    }
                })),
            )
                .into_response();
        }
    };
    let body_opt = if query_opt.is_none() && !body.iter().all(u8::is_ascii_whitespace) {
        match serde_json::from_slice::<VisionOffloadTogglePayload>(&body) {
            Ok(parsed) => parsed.enabled,
            Err(err) => {
                return GatewayError::from(RequestBodyError::from(err)).into_openai_response();
            }
        }
    } else {
        None
    };

    let next = match query_opt.or(body_opt) {
        Some(explicit) => {
            state
                .config
                .mcp_tool_bridge
                .store(explicit, Ordering::Release);
            explicit
        }
        None => !state
            .config
            .mcp_tool_bridge
            .fetch_xor(true, Ordering::AcqRel),
    };
    if let Some(ref settings_path) = state.config.settings_path {
        crate::telemetry::sync_settings_json_bool_field(settings_path, "mcp_tool_bridge", next);
    }
    state.telemetry.notify_change();
    Json(json!({ "mcp_tool_bridge_enabled": next })).into_response()
}

async fn handle_get_low_cache_hits<B: AcpBackend>(
    State(state): State<AppState<B>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Err(err) = check_auth_with_query(&headers, &uri, state.config.api_key.as_deref()) {
        return err.into_openai_response();
    }
    Json(state.telemetry.low_cache_hits()).into_response()
}

async fn handle_get_low_cache_hit_detail<B: AcpBackend>(
    State(state): State<AppState<B>>,
    Path(task_id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Err(err) = check_auth_with_query(&headers, &uri, state.config.api_key.as_deref()) {
        return err.into_openai_response();
    }
    match state.telemetry.low_cache_hit_detail(task_id.trim()) {
        Some(detail) => Json(detail).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": {
                    "message": format!("low-cache diagnostic record `{}` not found", task_id.trim()),
                    "type": "not_found_error",
                }
            })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct ClearLowCachePayload {
    #[serde(default)]
    reset_totals: Option<bool>,
}

async fn handle_clear_low_cache_hits<B: AcpBackend>(
    State(state): State<AppState<B>>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Err(err) = check_auth_with_query(&headers, &uri, state.config.api_key.as_deref()) {
        return err.into_openai_response();
    }

    let query_opt = match parse_query_bool_param(&uri, "reset_totals") {
        Ok(opt) => opt,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": msg,
                        "type": "invalid_request_error",
                    }
                })),
            )
                .into_response();
        }
    };
    let body_opt = if query_opt.is_none() && !body.iter().all(u8::is_ascii_whitespace) {
        match serde_json::from_slice::<ClearLowCachePayload>(&body) {
            Ok(parsed) => parsed.reset_totals,
            Err(err) => {
                return GatewayError::from(RequestBodyError::from(err)).into_openai_response();
            }
        }
    } else {
        None
    };

    let summary = state
        .telemetry
        .clear_low_cache_hits(query_opt.or(body_opt).unwrap_or(false));
    Json(summary).into_response()
}

#[derive(Deserialize)]
struct CacheConfigUpdatePayload {
    #[serde(default)]
    max_low_cache_records: Option<usize>,
    #[serde(default)]
    low_cache_threshold_bps: Option<u16>,
    #[serde(default)]
    low_cache_threshold_pct: Option<u8>,
    #[serde(default)]
    capture_request_details: Option<bool>,
}

async fn handle_set_cache_config<B: AcpBackend>(
    State(state): State<AppState<B>>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Err(err) = check_auth_with_query(&headers, &uri, state.config.api_key.as_deref()) {
        return err.into_openai_response();
    }

    let parsed: CacheConfigUpdatePayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(err) => {
            return GatewayError::from(RequestBodyError::from(err)).into_openai_response();
        }
    };

    let next_limit = match parsed.max_low_cache_records {
        Some(raw) => match LowCacheRecordLimit::try_new(raw) {
            Ok(valid) => Some(valid),
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": {
                            "message": err.to_string(),
                            "type": "invalid_request_error",
                        }
                    })),
                )
                    .into_response();
            }
        },
        None => None,
    };

    let next_threshold = if let Some(bps) = parsed.low_cache_threshold_bps {
        match CacheHitThresholdBps::try_new(bps) {
            Ok(valid) => Some(valid),
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": {
                            "message": err.to_string(),
                            "type": "invalid_request_error",
                        }
                    })),
                )
                    .into_response();
            }
        }
    } else if let Some(pct) = parsed.low_cache_threshold_pct {
        match CacheHitThresholdBps::try_from_percent(pct) {
            Ok(valid) => Some(valid),
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": {
                            "message": err.to_string(),
                            "type": "invalid_request_error",
                        }
                    })),
                )
                    .into_response();
            }
        }
    } else {
        None
    };

    let summary = state.telemetry.update_cache_config(
        next_limit,
        next_threshold,
        parsed.capture_request_details,
    );
    Json(summary).into_response()
}

async fn handle_admin_shutdown<B: AcpBackend>(
    State(state): State<AppState<B>>,
    headers: HeaderMap,
) -> Response {
    let Some(handle) = &state.admin_shutdown else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let provided = headers
        .get(ADMIN_SHUTDOWN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or_default();
    if !handle.verify_and_trigger(provided) {
        return GatewayError::Unauthorized.into_openai_response();
    }
    state.telemetry.signal_shutdown();
    Json(json!({ "status": "shutting_down" })).into_response()
}

async fn handle_ui_static<B: AcpBackend>(
    State(state): State<AppState<B>>,
    method: Method,
    uri: Uri,
) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::NOT_FOUND.into_response();
    }

    let req_path = uri.path().trim_start_matches('/');
    let is_api_prefix = req_path.starts_with("v1/")
        || req_path == "v1"
        || req_path.starts_with("api/")
        || req_path == "api"
        || req_path.starts_with("claude-desktop/")
        || req_path == "claude-desktop";

    let (relative, relative_slash) = if req_path.is_empty() {
        (PathBuf::from("index.html"), "index.html".to_owned())
    } else {
        let candidate = StdPath::new(req_path);
        let mut clean = PathBuf::new();
        let mut slash_parts: Vec<&str> = Vec::new();
        for comp in candidate.components() {
            match comp {
                Component::Normal(part) => {
                    let Some(s) = part.to_str() else {
                        return StatusCode::BAD_REQUEST.into_response();
                    };
                    clean.push(part);
                    slash_parts.push(s);
                }
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return StatusCode::BAD_REQUEST.into_response();
                }
            }
        }
        if slash_parts.is_empty() {
            (PathBuf::from("index.html"), "index.html".to_owned())
        } else {
            (clean, slash_parts.join("/"))
        }
    };

    if let Some(base_dir) = &state.ui_dist_dir {
        let full_path = base_dir.join(&relative);
        let target_file = if tokio::fs::metadata(&full_path)
            .await
            .is_ok_and(|m| m.is_file())
        {
            full_path
        } else if !is_api_prefix {
            base_dir.join("index.html")
        } else {
            return StatusCode::NOT_FOUND.into_response();
        };

        let Ok(bytes) = tokio::fs::read(&target_file).await else {
            return StatusCode::NOT_FOUND.into_response();
        };

        let mime = guess_mime_type(&target_file);
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, mime)],
            Bytes::from(bytes),
        )
            .into_response();
    }

    if !state.embedded_ui.is_empty() {
        let found = state
            .embedded_ui
            .iter()
            .find(|(path, _)| *path == relative_slash)
            .or_else(|| {
                (!is_api_prefix).then(|| {
                    state
                        .embedded_ui
                        .iter()
                        .find(|(path, _)| *path == "index.html")
                })?
            });
        if let Some((matched_path, bytes)) = found {
            let mime = guess_mime_type(StdPath::new(matched_path));
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime)],
                Bytes::from_static(bytes),
            )
                .into_response();
        }
        return StatusCode::NOT_FOUND.into_response();
    }

    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": {
                "message": "UI static bundle not built yet. Run `bun run dev` inside `web/` for development or `bun run build` inside `web/` to serve the dashboard here.",
                "type": "not_found"
            }
        })),
    )
        .into_response()
}

fn guess_mime_type(path: &StdPath) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        _ => "application/octet-stream",
    }
}

async fn cors_middleware(req: Request, next: Next) -> Response {
    let requested_headers = req
        .headers()
        .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
        .cloned();
    if req.method() == Method::OPTIONS {
        let mut preflight = StatusCode::NO_CONTENT.into_response();
        append_cors_headers(preflight.headers_mut(), requested_headers);
        return preflight;
    }
    let mut resp = next.run(req).await;
    append_cors_headers(resp.headers_mut(), requested_headers);
    resp
}

fn append_cors_headers(headers: &mut HeaderMap, requested_headers: Option<HeaderValue>) {
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, DELETE, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        requested_headers.unwrap_or_else(|| {
            HeaderValue::from_static(
                "Content-Type, Authorization, X-Api-Key, Anthropic-Version, Anthropic-Beta, Anthropic-Dangerous-Direct-Browser-Access, Accept, Cache-Control",
            )
        }),
    );
}
