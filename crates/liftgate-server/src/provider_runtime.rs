//! Multi-provider runtime registry, credential vault, stateless native 0-RTT turn runner,
//! and stateful stdio dialect dispatcher.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, atomic::Ordering};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, StatusCode};
use liftgate_provider_http::{
    DefaultHttpTransport, NativeDriverInput, NativeStreamEvent, NativeWireTool, PlatformApiError,
    open_driver_sse_step, probe_http_provider, pump_opened_driver_sse_step,
};
use liftgate_provider_stateful::{
    adapters::{
        DynamicToolDisposition, StatefulClientToolSpec, StatefulDialect, StatefulInputImage,
        StatefulStreamEvent, StatefulTurnInput, run_turn_on_spawned_peer_with_handler,
    },
    stdio_rpc::{StatefulRpcError, StdioSpawnConfig},
};
use liftgate_tool_virtualizer::{
    LocalReplyKind, McpSessionCatalog, McpSessionOptions, ProtocolHint, VirtualizationTargetMode,
    VirtualizedCallDecision, VirtualizerBudget, VirtualizerPlanConfig, normalize_mcp_object_schema,
    partition_tools_with_config,
};
use liftgate_types::{
    acp::{AcpUsage, AvailableModel},
    canonical::{
        AuxiliaryTaskKind, CanonicalMessage, CanonicalPart, CanonicalRole, CanonicalToolKind,
        CanonicalToolSpec, NormalizedTurnRequest, ProtocolDeveloperSlot,
    },
    config::{ModelId, ProxyUrl},
    harness_prompt::{DetectedHarness, detect_harness_by_tools},
    model::resolve_model_id,
    provider::{
        ApiKeySource, OptimizationMode, OptimizationPolicy, PromptCacheSemantics,
        ProviderCapabilities, ProviderDriverKind, ProviderId, ProviderProfileConfig,
        ProviderRegistryConfig, build_native_api_bridge_rules, parse_route_provider_spec,
    },
    session_key::{
        McpBridgePromptSpec, NoopSpillSink, WebToolBridgeConfig,
        build_session_system_append_for_mcp_with_spill_sink, format_conversation_hash_hex,
        hash_conversation_state, is_code_mode_orchestrator_tool, scrub_terminal_control_noise,
        strip_billing_noise,
    },
    telemetry::{
        LowCacheRoutingStrategy, ProviderTelemetrySummary, TaskProtocol, ToolCallErrorKind,
        ToolCallRoute, ToolCallStatus, UpstreamWireSummary,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    acp_client::AcpBackend,
    error::{AcpError, GatewayError},
    mcp_bridge::{McpContentItem, McpSessionHandle, McpToolCallReply, ResolvedMcpIncoming},
    session_pool::SessionPool,
    telemetry::{TelemetryHub, TurnCompletionDiagnostics, unix_timestamp_ms},
    tool_calls::{ToolCallOutcome, ToolCallStart},
    turn_runner::{
        ActiveTurnStream, GatewayRuntimeConfig, TurnEvent, execute_normalized_turn_with_provider,
    },
    web_fetch::{WebFetchOutcome, fetch_url_sanitized_with_options_and_pattern},
};

pub const HEADER_PROVIDER_ID: &str = "x-liftgate-provider";
pub const HEADER_OPTIMIZATION_MODE: &str = "x-liftgate-optimization";
const MAX_LOCAL_CONTINUATION_STEPS: usize = 8;
const MAX_CACHED_CONTINUATIONS: usize = 256;
const MAX_CACHED_THOUGHT_SIGNATURES: usize = 2_048;

#[derive(Clone, Debug)]
struct StoredResponseContinuation {
    messages: Vec<CanonicalMessage>,
    developer_slots: Vec<ProtocolDeveloperSlot>,
    client_tools: Vec<CanonicalToolSpec>,
    detected_harness: Option<DetectedHarness>,
}

#[derive(Debug, Default)]
struct ResponseContinuationCache {
    order: VecDeque<String>,
    by_id: HashMap<String, StoredResponseContinuation>,
}

impl ResponseContinuationCache {
    fn insert(&mut self, response_id: String, entry: StoredResponseContinuation) {
        if !self.by_id.contains_key(&response_id) {
            if self.order.len() >= MAX_CACHED_CONTINUATIONS
                && let Some(evicted) = self.order.pop_front()
            {
                self.by_id.remove(&evicted);
            }
            self.order.push_back(response_id.clone());
        }
        self.by_id.insert(response_id, entry);
    }

    fn get(&self, response_id: &str) -> Option<StoredResponseContinuation> {
        self.by_id.get(response_id).cloned()
    }
}

#[derive(Debug, Default)]
struct ThoughtSignatureCache {
    order: VecDeque<String>,
    by_call_id: HashMap<String, String>,
}

fn normalize_tool_call_stem(call_id: &str) -> &str {
    call_id
        .strip_prefix("toolu_")
        .or_else(|| call_id.strip_prefix("call_"))
        .unwrap_or(call_id)
}

fn lookup_by_call_id_or_stem<'a, V>(map: &'a HashMap<String, V>, call_id: &str) -> Option<&'a V> {
    if let Some(v) = map.get(call_id) {
        return Some(v);
    }
    let stem = normalize_tool_call_stem(call_id);
    if let Some(v) = map.get(stem) {
        return Some(v);
    }
    map.get(&format!("call_{stem}"))
        .or_else(|| map.get(&format!("toolu_{stem}")))
}

impl ThoughtSignatureCache {
    fn insert(&mut self, call_id: String, signature: String) {
        if call_id.is_empty() || signature.is_empty() {
            return;
        }
        if !self.by_call_id.contains_key(&call_id) {
            if self.order.len() >= MAX_CACHED_THOUGHT_SIGNATURES
                && let Some(evicted) = self.order.pop_front()
            {
                self.by_call_id.remove(&evicted);
            }
            self.order.push_back(call_id.clone());
        }
        self.by_call_id.insert(call_id, signature);
    }

    fn collect_for_messages(&self, messages: &[CanonicalMessage]) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for msg in messages {
            for part in &msg.parts {
                if let CanonicalPart::ToolCall { call_id, .. } = part
                    && let Some(sig) = lookup_by_call_id_or_stem(&self.by_call_id, call_id)
                {
                    out.insert(call_id.clone(), sig.clone());
                }
            }
        }
        out
    }
}

#[derive(Debug, Default)]
struct AnthropicThinkingCache {
    order: VecDeque<String>,
    by_call_id: HashMap<String, Vec<CanonicalPart>>,
}

impl AnthropicThinkingCache {
    fn insert(&mut self, call_id: String, blocks: Vec<CanonicalPart>) {
        if call_id.is_empty() || blocks.is_empty() {
            return;
        }
        if !self.by_call_id.contains_key(&call_id) {
            if self.order.len() >= MAX_CACHED_THOUGHT_SIGNATURES
                && let Some(evicted) = self.order.pop_front()
            {
                self.by_call_id.remove(&evicted);
            }
            self.order.push_back(call_id.clone());
        }
        self.by_call_id.insert(call_id, blocks);
    }

    fn restore_into_messages(&self, messages: &mut [CanonicalMessage]) {
        for msg in messages.iter_mut() {
            if msg.role != CanonicalRole::Assistant {
                continue;
            }
            let already_has_signed = msg.parts.iter().any(|p| {
                matches!(
                    p,
                    CanonicalPart::SignedThinking { .. } | CanonicalPart::RedactedThinking { .. }
                )
            });
            if already_has_signed {
                continue;
            }
            let found = msg.parts.iter().find_map(|p| match p {
                CanonicalPart::ToolCall { call_id, .. } => {
                    lookup_by_call_id_or_stem(&self.by_call_id, call_id).cloned()
                }
                _ => None,
            });
            if let Some(mut blocks) = found {
                blocks.append(&mut msg.parts);
                msg.parts = blocks;
            }
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct StoredCredentialsFile {
    #[serde(default)]
    keys: BTreeMap<String, String>,
}

#[derive(Debug)]
struct RegistryInner {
    config: ProviderRegistryConfig,
    credentials: BTreeMap<String, String>,
    providers_path: Option<PathBuf>,
    credentials_path: Option<PathBuf>,
    default_proxy: Option<ProxyUrl>,
}

/// Resolved per-request provider profile, capabilities, optimization policy, and runtime secret.
#[derive(Clone, Debug)]
pub struct ResolvedRequestProvider {
    pub profile: ProviderProfileConfig,
    pub capabilities: ProviderCapabilities,
    pub optimization: OptimizationPolicy,
    pub api_key: Option<String>,
    pub effective_proxy: Option<ProxyUrl>,
}

/// Thread-safe multi-provider configuration and mode-0600 credential registry.
pub struct ProviderRuntimeRegistry {
    inner: RwLock<RegistryInner>,
    continuations: Mutex<ResponseContinuationCache>,
    thought_signatures: Mutex<ThoughtSignatureCache>,
    anthropic_thinking: Mutex<AnthropicThinkingCache>,
}

impl std::fmt::Debug for ProviderRuntimeRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRuntimeRegistry")
            .finish_non_exhaustive()
    }
}

impl Default for ProviderRuntimeRegistry {
    fn default() -> Self {
        Self::in_memory(ProviderRegistryConfig::default(), None)
    }
}

impl ProviderRuntimeRegistry {
    #[must_use]
    pub fn in_memory(config: ProviderRegistryConfig, default_proxy: Option<ProxyUrl>) -> Self {
        Self {
            inner: RwLock::new(RegistryInner {
                config: config.normalized(),
                credentials: BTreeMap::new(),
                providers_path: None,
                credentials_path: None,
                default_proxy,
            }),
            continuations: Mutex::new(ResponseContinuationCache::default()),
            thought_signatures: Mutex::new(ThoughtSignatureCache::default()),
            anthropic_thinking: Mutex::new(AnthropicThinkingCache::default()),
        }
    }

    /// Loads `providers.json` and `credentials.json` from `state_dir` (creating defaults in memory if absent).
    #[must_use]
    pub fn load_from_dir(
        state_dir: &Path,
        fallback_agy_model: &ModelId,
        default_proxy: Option<ProxyUrl>,
    ) -> Self {
        let providers_path = state_dir.join("providers.json");
        let credentials_path = state_dir.join("credentials.json");
        let mut cfg = read_json_file::<ProviderRegistryConfig>(&providers_path)
            .unwrap_or_default()
            .normalized();
        if !providers_path.exists()
            && let Some(agy) = cfg.get_mut(&ProviderId::agy())
        {
            agy.default_model = fallback_agy_model.clone();
        }
        let creds = read_json_file::<StoredCredentialsFile>(&credentials_path)
            .unwrap_or_default()
            .keys;

        Self {
            inner: RwLock::new(RegistryInner {
                config: cfg,
                credentials: creds,
                providers_path: Some(providers_path),
                credentials_path: Some(credentials_path),
                default_proxy,
            }),
            continuations: Mutex::new(ResponseContinuationCache::default()),
            thought_signatures: Mutex::new(ThoughtSignatureCache::default()),
            anthropic_thinking: Mutex::new(AnthropicThinkingCache::default()),
        }
    }

    fn restore_continuation_into_request(&self, req: &mut NormalizedTurnRequest) {
        if req.auxiliary_task.is_some() {
            return;
        }
        let Some(prev_id) = req.previous_response_id.as_deref() else {
            return;
        };
        let prior_opt = match self.continuations.lock() {
            Ok(guard) => guard.get(prev_id),
            Err(poisoned) => poisoned.into_inner().get(prev_id),
        };
        let Some(prior) = prior_opt else {
            return;
        };
        let has_prior_base_dev = !prior.developer_slots.is_empty()
            || prior.messages.iter().any(|m| {
                m.parts.iter().any(|p| {
                    matches!(p, CanonicalPart::Text(t) if t.contains("<developer_instructions>"))
                })
            });
        if has_prior_base_dev {
            req.system_instructions = None;
        }
        req.prepend_history_with_slots(prior.messages, prior.developer_slots);
        if req.client_tools.is_empty() && !prior.client_tools.is_empty() {
            req.client_tools = prior.client_tools;
        }
        if req.detected_harness.is_none() {
            req.detected_harness = prior.detected_harness;
        }
    }

    fn save_continuation(&self, response_id: String, entry: StoredResponseContinuation) {
        match self.continuations.lock() {
            Ok(mut guard) => guard.insert(response_id, entry),
            Err(poisoned) => poisoned.into_inner().insert(response_id, entry),
        }
    }

    fn save_thought_signature(&self, call_id: String, signature: String) {
        match self.thought_signatures.lock() {
            Ok(mut guard) => guard.insert(call_id, signature),
            Err(poisoned) => poisoned.into_inner().insert(call_id, signature),
        }
    }

    fn collect_thought_signatures(&self, messages: &[CanonicalMessage]) -> HashMap<String, String> {
        match self.thought_signatures.lock() {
            Ok(guard) => guard.collect_for_messages(messages),
            Err(poisoned) => poisoned.into_inner().collect_for_messages(messages),
        }
    }

    fn save_anthropic_thinking(&self, call_id: String, blocks: Vec<CanonicalPart>) {
        match self.anthropic_thinking.lock() {
            Ok(mut guard) => guard.insert(call_id, blocks),
            Err(poisoned) => poisoned.into_inner().insert(call_id, blocks),
        }
    }

    fn restore_anthropic_thinking(&self, messages: &mut [CanonicalMessage]) {
        match self.anthropic_thinking.lock() {
            Ok(guard) => guard.restore_into_messages(messages),
            Err(poisoned) => poisoned.into_inner().restore_into_messages(messages),
        }
    }

    fn read_guard(&self) -> std::sync::RwLockReadGuard<'_, RegistryInner> {
        match self.inner.read() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    fn write_guard(&self) -> std::sync::RwLockWriteGuard<'_, RegistryInner> {
        match self.inner.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Reloads `providers.json` and `credentials.json` from disk after CLI changes.
    pub fn reload_from_disk(&self) -> bool {
        let mut guard = self.write_guard();
        let Some(ref prov_path) = guard.providers_path else {
            return false;
        };
        if let Some(cfg) = read_json_file::<ProviderRegistryConfig>(prov_path) {
            guard.config = cfg.normalized();
        }
        if let Some(ref cred_path) = guard.credentials_path
            && let Some(creds) = read_json_file::<StoredCredentialsFile>(cred_path)
        {
            guard.credentials = creds.keys;
        }
        true
    }

    #[must_use]
    pub fn config_snapshot(&self) -> ProviderRegistryConfig {
        self.read_guard().config.clone()
    }

    /// Updates the global active default provider and persists `providers.json` if backed by disk.
    pub fn set_active_provider(&self, id: ProviderId) -> Result<(), String> {
        let to_write = {
            let mut guard = self.write_guard();
            if guard.config.get(&id).is_none() {
                return Err(format!("unknown provider `{id}`"));
            }
            guard.config.active_provider = id;
            guard
                .providers_path
                .clone()
                .map(|p| (p, guard.config.clone()))
        };
        if let Some((path, cfg)) = to_write {
            write_json_private(&path, &cfg)?;
        }
        Ok(())
    }

    /// Updates the global optimization policy (`Optimized` / `Passthrough` / `Custom`) and persists.
    pub fn set_optimization_policy(&self, policy: OptimizationPolicy) -> Result<(), String> {
        let to_write = {
            let mut guard = self.write_guard();
            guard.config.optimization = policy;
            guard
                .providers_path
                .clone()
                .map(|p| (p, guard.config.clone()))
        };
        if let Some((path, cfg)) = to_write {
            write_json_private(&path, &cfg)?;
        }
        Ok(())
    }

    /// Inserts or updates a provider profile and optional stored API secret.
    pub fn upsert_provider(
        &self,
        profile: ProviderProfileConfig,
        secret_key: Option<String>,
    ) -> Result<(), String> {
        let (prov_write, cred_write) = {
            let mut guard = self.write_guard();
            let id_str = profile.id.as_str().to_owned();
            guard.config.upsert(profile);
            let cw = if let Some(sec) = secret_key {
                let trimmed = sec.trim().to_owned();
                if trimmed.is_empty() {
                    guard.credentials.remove(&id_str);
                } else {
                    guard.credentials.insert(id_str, trimmed);
                }
                guard.credentials_path.clone().map(|p| {
                    (
                        p,
                        StoredCredentialsFile {
                            keys: guard.credentials.clone(),
                        },
                    )
                })
            } else {
                None
            };
            let pw = guard
                .providers_path
                .clone()
                .map(|p| (p, guard.config.clone()));
            (pw, cw)
        };
        if let Some((path, creds)) = cred_write {
            write_json_private(&path, &creds)?;
        }
        if let Some((path, cfg)) = prov_write {
            write_json_private(&path, &cfg)?;
        }
        Ok(())
    }

    /// Removes a custom provider or resets a built-in provider to its factory defaults,
    /// clearing any stored API secret from `credentials.json`.
    pub fn remove_provider(&self, id: &ProviderId) -> Result<bool, String> {
        let (existed, prov_write, cred_write) = {
            let mut guard = self.write_guard();
            let id_str = id.as_str().to_owned();
            let had_cred = guard.credentials.remove(&id_str).is_some();
            let before_len = guard.config.providers.len();
            guard.config.providers.retain(|p| &p.id != id);
            let removed_profile = guard.config.providers.len() != before_len;
            guard.config = guard.config.clone().normalized();
            if &guard.config.active_provider == id
                && !ProviderId::is_builtin_id_or_alias(id.as_str())
            {
                guard.config.active_provider = ProviderId::agy();
            }
            if !removed_profile && !had_cred {
                return Ok(false);
            }
            let cw = guard.credentials_path.clone().map(|p| {
                (
                    p,
                    StoredCredentialsFile {
                        keys: guard.credentials.clone(),
                    },
                )
            });
            let pw = guard
                .providers_path
                .clone()
                .map(|p| (p, guard.config.clone()));
            (true, pw, cw)
        };
        if let Some((path, creds)) = cred_write {
            write_json_private(&path, &creds)?;
        }
        if let Some((path, cfg)) = prov_write {
            write_json_private(&path, &cfg)?;
        }
        Ok(existed)
    }

    /// Resolves the target provider and optimization policy for an incoming HTTP request.
    pub fn resolve_for_request(
        &self,
        uri_path: &str,
        headers: &HeaderMap,
    ) -> Result<ResolvedRequestProvider, GatewayError> {
        let url_spec = extract_url_provider_spec(uri_path);
        let header_prov = headers
            .get(HEADER_PROVIDER_ID)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let header_opt = headers
            .get(HEADER_OPTIMIZATION_MODE)
            .and_then(|v| v.to_str().ok())
            .and_then(OptimizationPolicy::from_slug);

        let guard = self.read_guard();
        let (target_id, route_opt) = if let Some(spec) = url_spec {
            parse_route_provider_spec(spec).ok_or_else(|| GatewayError::NotFound {
                resource: format!("provider:{spec}"),
            })?
        } else if let Some(hp) = header_prov {
            let id = ProviderId::try_new(hp).map_err(|_| GatewayError::NotFound {
                resource: format!("provider:{hp}"),
            })?;
            (id, None)
        } else {
            (guard.config.active_provider.clone(), None)
        };

        let profile =
            guard
                .config
                .get(&target_id)
                .cloned()
                .ok_or_else(|| GatewayError::NotFound {
                    resource: format!("provider:{target_id}"),
                })?;
        if !profile.enabled {
            return Err(GatewayError::ProviderDisabled {
                provider_id: profile.id.as_str().to_owned(),
            });
        }

        let optimization = route_opt
            .or(header_opt)
            .or_else(|| profile.optimization.clone())
            .unwrap_or_else(|| guard.config.optimization.clone());
        let capabilities = profile.capabilities();
        let api_key = resolve_api_key(&profile.id, &profile.auth, &guard.credentials);
        let effective_proxy = if profile.force_direct_proxy {
            None
        } else {
            profile
                .proxy
                .clone()
                .or_else(|| guard.default_proxy.clone())
        };

        Ok(ResolvedRequestProvider {
            profile,
            capabilities,
            optimization,
            api_key,
            effective_proxy,
        })
    }

    /// Returns `(default_model, available_models)` for `target_id`.
    #[must_use]
    pub fn models_for_provider(
        &self,
        target_id: &ProviderId,
        agy_models: &[AvailableModel],
        fallback_agy_default: &ModelId,
    ) -> (ModelId, Vec<AvailableModel>) {
        let guard = self.read_guard();
        let Some(profile) = guard.config.get(target_id) else {
            return (fallback_agy_default.clone(), agy_models.to_vec());
        };
        match profile.driver {
            ProviderDriverKind::AgyAcp => {
                let default_m = if target_id.as_str() == ProviderId::AGY {
                    fallback_agy_default.clone()
                } else {
                    profile.default_model.clone()
                };
                if !agy_models.is_empty() {
                    (default_m, agy_models.to_vec())
                } else {
                    let synthesized = profile
                        .effective_models()
                        .into_iter()
                        .map(|m| AvailableModel {
                            model_id: m.id,
                            name: m.display_name,
                        })
                        .collect();
                    (default_m, synthesized)
                }
            }
            ProviderDriverKind::OpenAiResponses
            | ProviderDriverKind::AnthropicMessages
            | ProviderDriverKind::GoogleGemini
            | ProviderDriverKind::CodexAppServer
            | ProviderDriverKind::GrokAcp => {
                let models = profile
                    .effective_models()
                    .into_iter()
                    .map(|m| AvailableModel {
                        model_id: m.id,
                        name: m.display_name,
                    })
                    .collect();
                (profile.default_model.clone(), models)
            }
        }
    }

    /// Builds redacted telemetry summaries for `GET /api/telemetry/snapshot`.
    #[must_use]
    pub fn snapshot_fields(
        &self,
    ) -> (
        String,
        OptimizationMode,
        OptimizationPolicy,
        Vec<ProviderTelemetrySummary>,
    ) {
        let guard = self.read_guard();
        let active_id = guard.config.active_provider.as_str().to_owned();
        let opt_policy = guard.config.optimization.clone();
        let opt_mode = opt_policy.mode();

        let summaries = guard
            .config
            .providers
            .iter()
            .map(|p| {
                let caps = p.capabilities();
                let eff_opt = p.optimization.as_ref().unwrap_or(&opt_policy);
                let auth_configured = match &p.auth {
                    ApiKeySource::None => true,
                    ApiKeySource::Env { .. } | ApiKeySource::Stored => {
                        resolve_api_key(&p.id, &p.auth, &guard.credentials).is_some()
                    }
                };
                ProviderTelemetrySummary {
                    id: p.id.as_str().to_owned(),
                    display_name: if p.display_name.is_empty() {
                        p.driver.display_name().to_owned()
                    } else {
                        p.display_name.clone()
                    },
                    driver: p.driver,
                    family: caps.family,
                    wire_protocol: caps.wire_protocol,
                    tool_wire_mode: caps.tool_wire_mode,
                    enabled: p.enabled,
                    active: p.id == guard.config.active_provider,
                    auth_configured,
                    auth_source: p.auth.summary_label(),
                    endpoint: p.effective_endpoint().map(str::to_owned),
                    default_model: p.default_model.as_str().to_owned(),
                    fast_model: p.fast_model.as_ref().map(|m| m.as_str().to_owned()),
                    title_model: p.title_model.as_ref().map(|m| m.as_str().to_owned()),
                    spill_enabled: caps.effective_spill_limit(eff_opt).is_some(),
                }
            })
            .collect();

        (active_id, opt_mode, opt_policy, summaries)
    }

    /// Live connectivity check invoked by `POST /api/telemetry/providers/test` and `lg provider test`.
    pub async fn probe_provider<B: AcpBackend>(
        &self,
        provider_id: &ProviderId,
        agy_backend: &B,
    ) -> Result<Value, String> {
        let resolved = {
            let guard = self.read_guard();
            let profile = guard
                .config
                .get(provider_id)
                .cloned()
                .ok_or_else(|| format!("unknown provider `{provider_id}`"))?;
            let api_key = resolve_api_key(&profile.id, &profile.auth, &guard.credentials);
            let proxy = if profile.force_direct_proxy {
                None
            } else {
                profile
                    .proxy
                    .clone()
                    .or_else(|| guard.default_proxy.clone())
            };
            (profile, api_key, proxy)
        };
        let (profile, api_key, proxy) = resolved;
        let started = Instant::now();
        match profile.driver {
            ProviderDriverKind::AgyAcp => {
                let models = agy_backend.refresh_models_if_empty().await;
                Ok(json!({
                    "ok": true,
                    "provider_id": profile.id.as_str(),
                    "driver": profile.driver.as_str(),
                    "latency_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(0),
                    "models_discovered": models.len(),
                }))
            }
            ProviderDriverKind::CodexAppServer | ProviderDriverKind::GrokAcp => {
                let bin_name = profile
                    .command
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .or_else(|| profile.driver.default_binary_name().map(str::to_owned))
                    .unwrap_or_default();
                let found = which_in_path(&bin_name);
                if let Some(resolved_bin) = found {
                    Ok(json!({
                        "ok": true,
                        "provider_id": profile.id.as_str(),
                        "driver": profile.driver.as_str(),
                        "binary": resolved_bin.display().to_string(),
                        "latency_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(0),
                    }))
                } else {
                    Err(format!(
                        "executable `{bin_name}` not found in PATH for stateful provider `{}`",
                        profile.id
                    ))
                }
            }
            ProviderDriverKind::OpenAiResponses
            | ProviderDriverKind::AnthropicMessages
            | ProviderDriverKind::GoogleGemini => {
                let endpoint = profile
                    .effective_endpoint()
                    .ok_or_else(|| "missing HTTP endpoint".to_owned())?;
                let count = probe_http_provider(
                    &DefaultHttpTransport,
                    profile.driver,
                    endpoint,
                    api_key.as_deref(),
                    proxy,
                    profile.force_direct_proxy,
                )
                .await
                .map_err(|e| e.to_string())?;
                Ok(json!({
                    "ok": true,
                    "provider_id": profile.id.as_str(),
                    "driver": profile.driver.as_str(),
                    "endpoint": endpoint,
                    "latency_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(0),
                    "models_discovered": count,
                }))
            }
        }
    }
}

/// Extracts `<provider_spec>` from `/p/<provider_spec>/...` paths.
#[must_use]
pub fn extract_url_provider_spec(uri_path: &str) -> Option<&str> {
    let rest = uri_path.strip_prefix("/p/")?;
    let (spec, _) = rest.split_once('/')?;
    let trimmed = spec.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn resolve_api_key(
    id: &ProviderId,
    auth: &ApiKeySource,
    stored: &BTreeMap<String, String>,
) -> Option<String> {
    match auth {
        ApiKeySource::None => None,
        ApiKeySource::Env { var } => std::env::var(var)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                if var == "GEMINI_API_KEY" {
                    std::env::var("GOOGLE_API_KEY")
                        .ok()
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                } else {
                    None
                }
            }),
        ApiKeySource::Stored => stored
            .get(id.as_str())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty()),
    }
}

fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Atomically writes JSON with `0600` owner-only permissions on Unix.
pub fn write_json_private<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed creating {}: {e}", parent.display()))?;
    }
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    let tmp = path.with_extension("json.tmp");
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("failed opening {}: {e}", tmp.display()))?;
        f.write_all(&bytes)
            .map_err(|e| format!("failed writing {}: {e}", tmp.display()))?;
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    {
        fs::write(&tmp, &bytes).map_err(|e| format!("failed writing {}: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, path).map_err(|e| format!("failed renaming {}: {e}", path.display()))
}

fn which_in_path(bin: &str) -> Option<PathBuf> {
    let trimmed = bin.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = Path::new(trimmed);
    if candidate.components().count() > 1 && candidate.is_file() {
        return Some(candidate.to_path_buf());
    }
    let path_os = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_os) {
        let full = dir.join(trimmed);
        if full.is_file() {
            return Some(full);
        }
        #[cfg(windows)]
        {
            for ext in ["exe", "cmd", "bat"] {
                let with_ext = dir.join(format!("{trimmed}.{ext}"));
                if with_ext.is_file() {
                    return Some(with_ext);
                }
            }
        }
    }
    None
}

/// Unified multi-provider turn dispatcher supporting both Camp A stateful sessions and Camp B stateless HTTP APIs.
#[expect(
    clippy::too_many_arguments,
    reason = "turn entrypoint threads backend, pool, telemetry, runtime config, resolved provider, protocol, and normalized request"
)]
pub async fn execute_provider_aware_turn<B: AcpBackend>(
    backend: Arc<B>,
    pool: Arc<SessionPool>,
    telemetry: &Arc<TelemetryHub>,
    cfg: &GatewayRuntimeConfig,
    resolved: ResolvedRequestProvider,
    protocol: TaskProtocol,
    mut req: NormalizedTurnRequest,
    response_id: String,
) -> Result<ActiveTurnStream, GatewayError> {
    if !resolved.optimization.prompt_distillation {
        req.allow_prompt_distillation = false;
    }
    match resolved.profile.driver {
        ProviderDriverKind::AgyAcp => {
            execute_normalized_turn_with_provider(
                backend,
                pool,
                telemetry,
                cfg,
                resolved.profile.id.as_str().to_owned(),
                resolved.optimization,
                protocol,
                req,
                response_id,
            )
            .await
        }
        ProviderDriverKind::OpenAiResponses
        | ProviderDriverKind::AnthropicMessages
        | ProviderDriverKind::GoogleGemini => {
            execute_stateless_http_turn(telemetry, cfg, resolved, protocol, req, response_id).await
        }
        ProviderDriverKind::CodexAppServer | ProviderDriverKind::GrokAcp => {
            execute_stateful_dialect_turn(telemetry, cfg, resolved, protocol, req, response_id)
        }
    }
}

fn resolve_non_agy_model(profile: &ProviderProfileConfig, req: &NormalizedTurnRequest) -> ModelId {
    if let Some(aux) = req.auxiliary_task {
        if matches!(aux, AuxiliaryTaskKind::SessionTitleOrTopic)
            && let Some(ref tm) = profile.title_model
        {
            return tm.clone();
        }
        if let Some(ref fm) = profile.fast_model {
            return fm.clone();
        }
    }
    if let Some(raw_req) = req
        .requested_model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let catalog: Vec<AvailableModel> = profile
            .effective_models()
            .into_iter()
            .map(|m| AvailableModel {
                model_id: m.id,
                name: m.display_name,
            })
            .collect();
        if catalog
            .iter()
            .any(|m| m.model_id.eq_ignore_ascii_case(raw_req))
        {
            return resolve_model_id(
                Some(raw_req),
                req.reasoning_effort,
                &catalog,
                &profile.default_model,
            );
        }
        // When a harness built on another vendor sends a foreign default slug (e.g. `claude-sonnet-*`
        // sent to OpenAI, or `gpt-*` sent to Anthropic), map cleanly to this provider's default model;
        // otherwise honor custom same-family model IDs verbatim!
        let lower = raw_req.to_ascii_lowercase();
        let foreign_slug = match profile.driver {
            ProviderDriverKind::OpenAiResponses | ProviderDriverKind::CodexAppServer => {
                lower.starts_with("claude-")
                    || lower.starts_with("gemini-")
                    || lower.starts_with("grok-")
            }
            ProviderDriverKind::AnthropicMessages => {
                lower.starts_with("gpt-")
                    || lower.starts_with("o3")
                    || lower.starts_with("o4")
                    || lower.starts_with("gemini-")
                    || lower.starts_with("grok-")
            }
            ProviderDriverKind::GoogleGemini => {
                lower.starts_with("claude-")
                    || lower.starts_with("gpt-")
                    || lower.starts_with("o3")
                    || lower.starts_with("o4")
                    || lower.starts_with("grok-")
            }
            ProviderDriverKind::GrokAcp => {
                lower.starts_with("claude-")
                    || lower.starts_with("gemini-")
                    || lower.starts_with("gpt-")
                    || lower.starts_with("o3")
                    || lower.starts_with("o4")
            }
            ProviderDriverKind::AgyAcp => false,
        };
        if foreign_slug {
            return profile.default_model.clone();
        }
        if let Ok(custom_id) = ModelId::try_new(raw_req) {
            return custom_id;
        }
    }
    profile.default_model.clone()
}

fn estimate_raw_tool_bytes(tools: &[CanonicalToolSpec]) -> usize {
    tools.iter().fold(0usize, |acc, t| {
        let schema_len = match &t.kind {
            CanonicalToolKind::Function { parameters_json } => parameters_json.len(),
            CanonicalToolKind::Custom { format_json } => format_json.as_deref().map_or(0, str::len),
        };
        acc.saturating_add(t.name.len())
            .saturating_add(t.description.as_deref().map_or(0, str::len))
            .saturating_add(schema_len)
    })
}

fn measure_system_slot_bytes(messages: &[CanonicalMessage]) -> usize {
    messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            CanonicalPart::Text(t) if t.contains("<developer_instructions>") => Some(t.len()),
            _ => None,
        })
        .sum()
}

fn split_system_and_conversation_messages(
    rendered: &[CanonicalMessage],
    strip_billing: bool,
) -> (Vec<String>, Vec<CanonicalMessage>) {
    let mut sys_blocks: Vec<String> = Vec::new();
    let mut out_msgs: Vec<CanonicalMessage> = Vec::with_capacity(rendered.len());

    for (msg_idx, msg) in rendered.iter().enumerate() {
        let mut kept_parts: Vec<CanonicalPart> = Vec::with_capacity(msg.parts.len());
        for part in &msg.parts {
            match part {
                CanonicalPart::Text(raw_text) => {
                    let cleaned = if strip_billing {
                        strip_billing_noise(raw_text)
                    } else {
                        raw_text.clone()
                    };
                    if msg_idx == 0
                        && msg.role == CanonicalRole::User
                        && let Some(inner) = extract_pure_developer_block(&cleaned)
                    {
                        if !inner.is_empty() {
                            sys_blocks.push(inner);
                        }
                    } else if !cleaned.trim().is_empty() {
                        kept_parts.push(CanonicalPart::Text(cleaned));
                    }
                }
                CanonicalPart::ToolResult {
                    call_id,
                    output,
                    is_error,
                } => {
                    let cleaned = if strip_billing {
                        let s = strip_billing_noise(output);
                        scrub_terminal_control_noise(&s).into_owned()
                    } else {
                        output.clone()
                    };
                    kept_parts.push(CanonicalPart::ToolResult {
                        call_id: call_id.clone(),
                        output: cleaned,
                        is_error: *is_error,
                    });
                }
                other => kept_parts.push(other.clone()),
            }
        }
        if !kept_parts.is_empty() {
            out_msgs.push(CanonicalMessage {
                role: msg.role,
                parts: kept_parts,
            });
        }
    }

    if out_msgs.is_empty() {
        out_msgs.push(CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::Text(".".to_owned())],
        });
    }
    (sys_blocks, out_msgs)
}

fn extract_pure_developer_block(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix("<developer_instructions>")?;
    let inner = rest.strip_suffix("</developer_instructions>")?;
    if inner.contains("<developer_instructions>") {
        return None;
    }
    Some(inner.trim().to_owned())
}

fn map_platform_error_to_acp(err: PlatformApiError, model: &str) -> AcpError {
    match err {
        PlatformApiError::RateLimited { message, .. } => AcpError::UpstreamRateLimited {
            model: model.to_owned(),
            detail: message,
        },
        PlatformApiError::QuotaExhausted { message, .. } => AcpError::UpstreamQuotaExhausted {
            model: model.to_owned(),
            detail: message,
        },
        PlatformApiError::Timeout { elapsed_ms } => AcpError::TurnTimeout {
            seconds: (elapsed_ms / 1_000).max(1),
        },
        PlatformApiError::InvalidRequest { message, .. }
            if message.to_ascii_lowercase().contains("context")
                || message.to_ascii_lowercase().contains("too long")
                || message.to_ascii_lowercase().contains("maximum") =>
        {
            AcpError::ContextLengthExceeded {
                model: model.to_owned(),
                detail: message,
            }
        }
        other => AcpError::Rpc {
            method: "http_sse",
            code: -32000,
            message: other.to_string(),
        },
    }
}

fn map_platform_error_to_gateway(err: PlatformApiError, model: &str) -> GatewayError {
    match err {
        PlatformApiError::RateLimited { message, .. } => GatewayError::Acp {
            source: AcpError::UpstreamRateLimited {
                model: model.to_owned(),
                detail: message,
            },
        },
        PlatformApiError::QuotaExhausted { message, .. } => GatewayError::Acp {
            source: AcpError::UpstreamQuotaExhausted {
                model: model.to_owned(),
                detail: message,
            },
        },
        PlatformApiError::Timeout { elapsed_ms } => GatewayError::Acp {
            source: AcpError::TurnTimeout {
                seconds: (elapsed_ms / 1_000).max(1),
            },
        },
        PlatformApiError::AuthenticationFailed { status, message } => {
            let (http_status, oai_type, oai_code, anth_type) = if status == 403 {
                (
                    StatusCode::FORBIDDEN,
                    "permission_error",
                    "permission_denied",
                    "permission_error",
                )
            } else {
                (
                    StatusCode::UNAUTHORIZED,
                    "authentication_error",
                    "invalid_api_key",
                    "authentication_error",
                )
            };
            GatewayError::UpstreamProviderHttp {
                status: http_status,
                openai_type: oai_type,
                openai_code: oai_code,
                anthropic_type: anth_type,
                message,
            }
        }
        PlatformApiError::MissingCredentials { provider_id, hint } => {
            GatewayError::UpstreamProviderHttp {
                status: StatusCode::UNAUTHORIZED,
                openai_type: "authentication_error",
                openai_code: "missing_api_key",
                anthropic_type: "authentication_error",
                message: format!("missing credentials for provider `{provider_id}`: {hint}"),
            }
        }
        PlatformApiError::NotFound { message, .. } => GatewayError::UpstreamProviderHttp {
            status: StatusCode::NOT_FOUND,
            openai_type: "not_found_error",
            openai_code: "model_not_found",
            anthropic_type: "not_found_error",
            message,
        },
        PlatformApiError::InvalidUrl { url, reason } => GatewayError::UpstreamProviderHttp {
            status: StatusCode::BAD_REQUEST,
            openai_type: "invalid_request_error",
            openai_code: "invalid_url",
            anthropic_type: "invalid_request_error",
            message: format!("invalid upstream URL `{url}`: {reason}"),
        },
        PlatformApiError::InvalidRequest { message, .. } => {
            let lower = message.to_ascii_lowercase();
            if lower.contains("context") || lower.contains("too long") || lower.contains("maximum")
            {
                GatewayError::Acp {
                    source: AcpError::ContextLengthExceeded {
                        model: model.to_owned(),
                        detail: message,
                    },
                }
            } else {
                GatewayError::UpstreamProviderHttp {
                    status: StatusCode::BAD_REQUEST,
                    openai_type: "invalid_request_error",
                    openai_code: "invalid_request",
                    anthropic_type: "invalid_request_error",
                    message,
                }
            }
        }
        PlatformApiError::UpstreamServer { status, message } => {
            GatewayError::UpstreamProviderHttp {
                status: if status == 503 {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::BAD_GATEWAY
                },
                openai_type: "api_error",
                openai_code: "upstream_error",
                anthropic_type: "api_error",
                message,
            }
        }
        PlatformApiError::Transport { message, .. } | PlatformApiError::Protocol { message } => {
            GatewayError::UpstreamProviderHttp {
                status: StatusCode::BAD_GATEWAY,
                openai_type: "api_error",
                openai_code: "upstream_transport_error",
                anthropic_type: "api_error",
                message,
            }
        }
    }
}

async fn execute_stateless_http_turn(
    telemetry: &Arc<TelemetryHub>,
    cfg: &GatewayRuntimeConfig,
    resolved: ResolvedRequestProvider,
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
    telemetry
        .provider_registry()
        .restore_continuation_into_request(&mut req);
    req.materialize_system_instructions();

    let endpoint = resolved
        .profile
        .effective_endpoint()
        .ok_or_else(|| GatewayError::NotFound {
            resource: format!("endpoint:{}", resolved.profile.id),
        })?
        .to_owned();

    // Fail fast before opening SSE if official HTTPS provider lacks an API key.
    let is_official_https = endpoint.starts_with("https://api.openai.com")
        || endpoint.starts_with("https://api.anthropic.com")
        || endpoint.starts_with("https://generativelanguage.googleapis.com");
    if is_official_https && resolved.api_key.is_none() {
        let hint = resolved.profile.auth.summary_label();
        return Err(GatewayError::UpstreamProviderHttp {
            status: StatusCode::UNAUTHORIZED,
            openai_type: "authentication_error",
            openai_code: "missing_api_key",
            anthropic_type: "authentication_error",
            message: format!(
                "missing API key for provider `{}` ({hint}); set environment variable or run `lg provider add {}`",
                resolved.profile.id, resolved.profile.id
            ),
        });
    }

    let resolved_model = resolve_non_agy_model(&resolved.profile, &req);
    let requested_label = req
        .requested_model
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| resolved_model.as_str().to_owned());

    let opt_mode = resolved.optimization.mode();
    let provider_id_str = resolved.profile.id.as_str().to_owned();

    let task_guard = telemetry.begin_task(
        response_id.clone(),
        protocol,
        req.stream,
        requested_label,
        req.auxiliary_task,
        &req.messages,
    );
    task_guard.set_provider_and_optimization(&provider_id_str, opt_mode);
    let synthetic_session_id = format!("native:{}:{}", provider_id_str, resolved_model.as_str());
    task_guard.bind_session(&synthetic_session_id, false, resolved_model.as_str());

    let detected_harness = req
        .detected_harness
        .or_else(|| detect_harness_by_tools(&req.client_tools));
    let raw_system_bytes = measure_system_slot_bytes(&req.messages);
    let raw_tool_count = req.client_tools.len();
    let raw_tool_bytes = estimate_raw_tool_bytes(&req.client_tools);

    let proto_hint = match protocol {
        TaskProtocol::OpenaiResponses => ProtocolHint::OpenAiResponses,
        TaskProtocol::AnthropicMessages => ProtocolHint::AnthropicMessages,
    };

    let mut noop_spill = NoopSpillSink;
    let native_catalog: Option<McpSessionCatalog> = if (resolved.optimization.tool_virtualization
        || resolved.optimization.code_mode_enhancements)
        && !req.client_tools.is_empty()
    {
        Some(partition_tools_with_config(
            proto_hint,
            &req.client_tools,
            VirtualizerPlanConfig {
                session: McpSessionOptions {
                    enable_gateway_web_fetch: req.enable_native_web_fetch,
                    enable_context_spill_reader: false,
                },
                target_mode: VirtualizationTargetMode::NativeApiTools,
                budget_override: Some(VirtualizerBudget::NATIVE_PLATFORM_API),
                disable_virtualization: !resolved.optimization.tool_virtualization,
                disable_code_mode_enhancements: !resolved.optimization.code_mode_enhancements,
            },
            &mut noop_spill,
        ))
    } else {
        None
    };

    let harness_id = native_catalog
        .as_ref()
        .map(|c| c.profile_id)
        .or_else(|| detected_harness.map(|h| h.id()))
        .unwrap_or("generic");
    let orch_mode = native_catalog
        .as_ref()
        .and_then(|c| c.orchestrator_contract().map(|o| o.mode_id()));
    task_guard.set_tool_context(harness_id, orch_mode, 1);

    let custom_tool_names: std::collections::HashSet<String> = req
        .client_tools
        .iter()
        .filter(|t| matches!(t.kind, CanonicalToolKind::Custom { .. }))
        .map(|t| t.name.clone())
        .collect();

    let wire_tools: Vec<NativeWireTool> = if let Some(ref cat) = native_catalog {
        cat.wire_tools
            .iter()
            .map(|wt| {
                let orig_spec = cat
                    .lookup(&wt.name)
                    .and_then(|b| req.client_tools.iter().find(|s| s.name == b.client_name));
                let is_custom_freeform = resolved.capabilities.supports_custom_freeform_tools
                    && orig_spec
                        .is_some_and(|s| matches!(s.kind, CanonicalToolKind::Custom { .. }));
                let custom_format = orig_spec.and_then(|s| match &s.kind {
                    CanonicalToolKind::Custom {
                        format_json: Some(fmt),
                    } => serde_json::from_str::<Value>(fmt).ok(),
                    _ => None,
                });
                NativeWireTool {
                    name: wt.name.clone(),
                    description: wt.description.clone(),
                    input_schema: wt.input_schema.clone(),
                    is_custom_freeform,
                    custom_format,
                }
            })
            .collect()
    } else {
        req.client_tools
            .iter()
            .map(|spec| match &spec.kind {
                CanonicalToolKind::Function { parameters_json } => NativeWireTool {
                    name: spec.name.clone(),
                    description: spec.description.clone(),
                    input_schema: normalize_mcp_object_schema(parameters_json),
                    is_custom_freeform: false,
                    custom_format: None,
                },
                CanonicalToolKind::Custom { format_json } => {
                    let is_freeform = resolved.capabilities.supports_custom_freeform_tools;
                    let fmt = format_json
                        .as_deref()
                        .and_then(|f| serde_json::from_str::<Value>(f).ok());
                    NativeWireTool {
                        name: spec.name.clone(),
                        description: spec.description.clone(),
                        input_schema: json!({
                            "type": "object",
                            "properties": {
                                "input": { "type": "string", "description": "Raw tool input text." }
                            },
                            "required": ["input"]
                        }),
                        is_custom_freeform: is_freeform,
                        custom_format: fmt,
                    }
                }
            })
            .collect()
    };

    let forwarded_tool_count = wire_tools.len();
    let forwarded_tool_bytes: usize = wire_tools.iter().fold(0usize, |acc, t| {
        acc.saturating_add(t.name.len())
            .saturating_add(t.description.as_deref().map_or(0, str::len))
            .saturating_add(t.input_schema.to_string().len())
    });

    let rendered_cow = req.render_messages_for_outbound(0, None);
    let forwarded_system_bytes = measure_system_slot_bytes(&rendered_cow);
    let (mut sys_blocks, mut working_messages) = split_system_and_conversation_messages(
        &rendered_cow,
        resolved.optimization.strip_billing_noise,
    );

    let bridge_rules = if resolved.optimization.code_mode_enhancements
        || native_catalog
            .as_ref()
            .is_some_and(|c| c.has_virtualized_dispatch)
    {
        native_catalog.as_ref().and_then(|cat| {
            build_native_api_bridge_rules(
                &resolved.capabilities,
                cat.orchestrator_contract(),
                &cat.virtualized_hubs,
                cat.has_virtualized_dispatch,
                cat.has_gateway_web_fetch,
            )
        })
    } else {
        None
    };
    if let Some(ref rules) = bridge_rules {
        sys_blocks.push(rules.clone());
    }

    let cap = cfg.limits.channel_capacity().get();
    let (events_tx, events_rx) = mpsc::channel::<TurnEvent>(cap);
    let turn_cancel = CancellationToken::new();
    let cancel_child = turn_cancel.clone();
    let cancel_guard = turn_cancel.drop_guard();
    let timeout_secs = cfg.limits.turn_timeout_secs().get();
    let spill_dir = cfg.spill_dir.clone();
    let resolved_model_for_worker = resolved_model.clone();
    let response_id_for_store = response_id.clone();
    let telemetry_for_worker = Arc::clone(telemetry);
    let (step0_ready_tx, step0_ready_rx) =
        tokio::sync::oneshot::channel::<Result<(), GatewayError>>();

    let worker = tokio::spawn(async move {
        let mut step0_ready_tx = Some(step0_ready_tx);
        let turn_deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let mut total_usage = AcpUsage::default();
        let mut continuations: usize = 0;
        let mut last_wire_bytes: usize = 0;
        let mut last_breakpoints: usize = 0;
        let mut final_stop_reason = "end_turn".to_owned();
        let mut accumulated_messages = req.messages.clone();
        let mut final_assistant_parts: Vec<CanonicalPart> = Vec::new();
        let mut local_thought_signatures: HashMap<String, String> = HashMap::new();

        let prompt_cache_key = Some(format!("lg:{provider_id_str}:{harness_id}"));

        for _step in 0..=MAX_LOCAL_CONTINUATION_STEPS {
            if cancel_child.is_cancelled() {
                return;
            }
            let remaining_timeout = turn_deadline.saturating_duration_since(Instant::now());
            if remaining_timeout.is_zero() {
                if let Some(tx) = step0_ready_tx.take() {
                    let acp_err = AcpError::TurnTimeout {
                        seconds: timeout_secs,
                    };
                    task_guard.fail(acp_err.to_string());
                    let _ = tx.send(Err(GatewayError::Acp { source: acp_err }));
                    return;
                }
                let acp_err = AcpError::TurnTimeout {
                    seconds: timeout_secs,
                };
                task_guard.fail(acp_err.to_string());
                let _ = events_tx.send(TurnEvent::Failed(Arc::new(acp_err))).await;
                return;
            }
            if matches!(
                resolved.profile.driver,
                ProviderDriverKind::AnthropicMessages
            ) {
                telemetry_for_worker
                    .provider_registry()
                    .restore_anthropic_thinking(&mut working_messages);
            }
            let mut step_thought_signatures = telemetry_for_worker
                .provider_registry()
                .collect_thought_signatures(&working_messages);
            for msg in &working_messages {
                for part in &msg.parts {
                    if let CanonicalPart::ToolCall { call_id, .. } = part
                        && let Some(sig) =
                            lookup_by_call_id_or_stem(&local_thought_signatures, call_id)
                    {
                        step_thought_signatures.insert(call_id.clone(), sig.clone());
                    }
                }
            }
            let driver_input = NativeDriverInput {
                provider_id: provider_id_str.clone(),
                driver: resolved.profile.driver,
                endpoint: endpoint.clone(),
                api_key: resolved.api_key.clone(),
                proxy: resolved.effective_proxy.clone(),
                force_direct_proxy: resolved.profile.force_direct_proxy,
                model: resolved_model_for_worker.as_str().to_owned(),
                effort: req.reasoning_effort,
                max_output_tokens: None,
                system_blocks: sys_blocks.clone(),
                messages: working_messages.clone(),
                tools: wire_tools.clone(),
                enable_native_web_search: req.enable_native_web_search
                    && resolved.capabilities.supports_native_web_search,
                thought_signatures: step_thought_signatures,
                prompt_cache_key: prompt_cache_key.clone(),
                align_cache_breakpoints: matches!(
                    resolved.capabilities.cache_semantics,
                    PromptCacheSemantics::ExplicitBreakpoints
                ),
                connect_timeout: Duration::from_secs(25).min(remaining_timeout),
                total_timeout: remaining_timeout,
            };

            let opened = tokio::select! {
                biased;
                () = cancel_child.cancelled() => return,
                res = open_driver_sse_step(&DefaultHttpTransport, &driver_input) => match res {
                    Ok(opened) => {
                        if let Some(tx) = step0_ready_tx.take() {
                            let _ = tx.send(Ok(()));
                        }
                        opened
                    }
                    Err(err) => {
                        if let Some(tx) = step0_ready_tx.take() {
                            let gw_err = map_platform_error_to_gateway(
                                err,
                                resolved_model_for_worker.as_str(),
                            );
                            task_guard.fail(gw_err.client_message());
                            let _ = tx.send(Err(gw_err));
                            return;
                        }
                        let acp_err =
                            map_platform_error_to_acp(err, resolved_model_for_worker.as_str());
                        task_guard.fail(acp_err.to_string());
                        let _ = events_tx.send(TurnEvent::Failed(Arc::new(acp_err))).await;
                        return;
                    }
                }
            };

            let (step_tx, mut step_rx) =
                mpsc::channel::<Result<NativeStreamEvent, PlatformApiError>>(128);
            let step_fut = async {
                let step_tx = step_tx;
                pump_opened_driver_sse_step(opened, &step_tx).await
            };
            tokio::pin!(step_fut);

            let mut step_done = false;
            let mut step_reasoning = String::new();
            let mut step_signed_thinking_parts: Vec<CanonicalPart> = Vec::new();
            let mut step_text = String::new();
            let mut emitted_raw_calls: Vec<(String, String, String, bool)> = Vec::new();
            let mut stream_err: Option<PlatformApiError> = None;

            loop {
                tokio::select! {
                    biased;
                    () = cancel_child.cancelled() => return,
                    maybe_ev = step_rx.recv() => {
                        match maybe_ev {
                            Some(Ok(ev)) => match ev {
                                NativeStreamEvent::ReasoningDelta(d) => {
                                    step_reasoning.push_str(&d);
                                    task_guard.record_thought(&d);
                                    let _ = events_tx.send(TurnEvent::ThoughtDelta(d)).await;
                                }
                                NativeStreamEvent::ThinkingSignatureDelta(sig) => {
                                    let _ = sig;
                                }
                                NativeStreamEvent::SignedThinkingBlock {
                                    thinking,
                                    signature,
                                } => {
                                    let _ = events_tx
                                        .send(TurnEvent::ThoughtSignature(signature.clone()))
                                        .await;
                                    step_signed_thinking_parts.push(CanonicalPart::SignedThinking {
                                        thinking,
                                        signature,
                                    });
                                }
                                NativeStreamEvent::RedactedThinkingBlock { data } => {
                                    step_signed_thinking_parts.push(
                                        CanonicalPart::RedactedThinking { data: data.clone() },
                                    );
                                    let _ =
                                        events_tx.send(TurnEvent::RedactedThinking(data)).await;
                                }
                                NativeStreamEvent::TextDelta(d) => {
                                    step_text.push_str(&d);
                                    task_guard.record_text(&d);
                                    let _ = events_tx.send(TurnEvent::TextDelta(d)).await;
                                }
                                NativeStreamEvent::WebSearch { query } => {
                                    task_guard.record_web_search();
                                    let _ = events_tx.send(TurnEvent::WebSearch {
                                        call_id: format!("ws_{}", unix_timestamp_ms()),
                                        status: "completed".to_owned(),
                                        query: (!query.is_empty()).then_some(query),
                                    }).await;
                                }
                                NativeStreamEvent::ToolCall {
                                    id,
                                    name,
                                    arguments_json,
                                    is_custom,
                                    thought_signature,
                                } => {
                                    if let Some(sig) = thought_signature
                                        && !sig.is_empty()
                                    {
                                        local_thought_signatures.insert(id.clone(), sig.clone());
                                        telemetry_for_worker
                                            .provider_registry()
                                            .save_thought_signature(id.clone(), sig);
                                    }
                                    emitted_raw_calls.push((id, name, arguments_json, is_custom));
                                }
                                NativeStreamEvent::Usage(u) => {
                                    total_usage.input_tokens = total_usage.input_tokens.saturating_add(u.input_tokens);
                                    total_usage.output_tokens = total_usage.output_tokens.saturating_add(u.output_tokens);
                                    total_usage.thought_tokens = total_usage.thought_tokens.saturating_add(u.thought_tokens);
                                    total_usage.cached_read_tokens = total_usage.cached_read_tokens.saturating_add(u.cached_read_tokens);
                                    total_usage.cached_write_tokens = total_usage.cached_write_tokens.saturating_add(u.cached_write_tokens);
                                    total_usage.total_tokens = total_usage.total_tokens.saturating_add(u.total_tokens);
                                }
                                NativeStreamEvent::Completed { stop_reason } => {
                                    final_stop_reason = stop_reason;
                                }
                            },
                            Some(Err(err)) => {
                                stream_err = Some(err);
                                break;
                            }
                            None => {
                                break;
                            }
                        }
                    }
                    res = &mut step_fut, if !step_done => {
                        step_done = true;
                        match res {
                            Ok(metrics) => {
                                last_wire_bytes = metrics.wire_body_bytes;
                                last_breakpoints = metrics.cache_breakpoints_aligned;
                            }
                            Err(err) => {
                                stream_err = Some(err);
                                break;
                            }
                        }
                    }
                }
            }

            if let Some(err) = stream_err {
                let acp_err = map_platform_error_to_acp(err, resolved_model_for_worker.as_str());
                task_guard.fail(acp_err.to_string());
                let _ = events_tx.send(TurnEvent::Failed(Arc::new(acp_err))).await;
                return;
            }

            if emitted_raw_calls.is_empty() {
                if !step_signed_thinking_parts.is_empty() {
                    final_assistant_parts.extend(step_signed_thinking_parts);
                } else if !step_reasoning.is_empty() {
                    final_assistant_parts.push(CanonicalPart::Reasoning(step_reasoning));
                }
                if !step_text.is_empty() {
                    final_assistant_parts.push(CanonicalPart::Text(step_text));
                }
                break;
            }

            if !step_signed_thinking_parts.is_empty()
                && let Some((first_call_id, ..)) = emitted_raw_calls.first()
            {
                telemetry_for_worker
                    .provider_registry()
                    .save_anthropic_thinking(
                        first_call_id.clone(),
                        step_signed_thinking_parts.clone(),
                    );
            }

            let mut local_continuation_parts: Vec<CanonicalPart> = Vec::new();
            let mut local_result_parts: Vec<CanonicalPart> = Vec::new();
            let mut client_forwarded_parts: Vec<CanonicalPart> = Vec::new();
            let mut forwarded_any_to_client = false;

            if !step_signed_thinking_parts.is_empty() {
                local_continuation_parts.extend(step_signed_thinking_parts.clone());
                client_forwarded_parts.extend(step_signed_thinking_parts);
            } else if !step_reasoning.is_empty() {
                local_continuation_parts.push(CanonicalPart::Reasoning(step_reasoning.clone()));
                client_forwarded_parts.push(CanonicalPart::Reasoning(step_reasoning));
            }
            if !step_text.is_empty() {
                client_forwarded_parts.push(CanonicalPart::Text(step_text.clone()));
                local_continuation_parts.push(CanonicalPart::Text(step_text));
            }

            for (call_id, wire_name, raw_args, raw_is_custom) in emitted_raw_calls {
                let started_ms = unix_timestamp_ms();
                if let Some(ref cat) = native_catalog {
                    let parsed_args = if raw_is_custom {
                        json!({ "code": raw_args, "patch": raw_args, "input": raw_args })
                    } else {
                        serde_json::from_str::<Value>(&raw_args).unwrap_or_else(|_| json!({}))
                    };
                    let decision =
                        cat.routing_table
                            .resolve_call(&wire_name, &parsed_args, &req.client_tools);
                    match decision {
                        VirtualizedCallDecision::LocalReply {
                            text,
                            is_error,
                            kind,
                        } => {
                            let (route, status, error_kind) = match kind {
                                LocalReplyKind::Search => {
                                    task_guard.record_zero_rtt_search();
                                    (
                                        ToolCallRoute::ZeroRttSearch,
                                        if is_error {
                                            ToolCallStatus::Error
                                        } else {
                                            ToolCallStatus::Ok
                                        },
                                        is_error.then_some(ToolCallErrorKind::ToolError),
                                    )
                                }
                                LocalReplyKind::PreflightError => {
                                    task_guard.record_zero_rtt_preflight_heal();
                                    (
                                        ToolCallRoute::ZeroRttPreflight,
                                        ToolCallStatus::PreflightRejected,
                                        Some(ToolCallErrorKind::PreflightRejected),
                                    )
                                }
                                LocalReplyKind::DisabledTool => {
                                    task_guard.record_zero_rtt_preflight_heal();
                                    (
                                        ToolCallRoute::ZeroRttPreflight,
                                        ToolCallStatus::PreflightRejected,
                                        Some(ToolCallErrorKind::DisabledTool),
                                    )
                                }
                                LocalReplyKind::SpillRead => (
                                    ToolCallRoute::ZeroRttSpillRead,
                                    if is_error {
                                        ToolCallStatus::Error
                                    } else {
                                        ToolCallStatus::Ok
                                    },
                                    is_error.then_some(ToolCallErrorKind::ToolError),
                                ),
                            };
                            task_guard.record_local_tool_call(
                                ToolCallStart {
                                    call_id: call_id.clone(),
                                    route,
                                    wire_tool_name: wire_name.clone(),
                                    client_tool_name: None,
                                    is_orchestrator: false,
                                    code_language: None,
                                    wire_arguments: raw_args.clone(),
                                    client_arguments: None,
                                    v4a_auto_repaired: false,
                                },
                                ToolCallOutcome {
                                    status,
                                    error_kind,
                                    raw_bytes: text.len(),
                                    text: text.clone(),
                                    images: Vec::new(),
                                    forwarded_output: None,
                                    sanitized: false,
                                    spilled: false,
                                },
                                started_ms,
                            );
                            local_continuation_parts.push(CanonicalPart::ToolCall {
                                call_id: call_id.clone(),
                                name: wire_name,
                                arguments: raw_args,
                                is_custom: raw_is_custom,
                            });
                            local_result_parts.push(CanonicalPart::ToolResult {
                                call_id,
                                output: text,
                                is_error,
                            });
                        }
                        VirtualizedCallDecision::GatewayWebFetch { url, pattern } => {
                            task_guard.record_web_fetch();
                            let outcome = fetch_url_sanitized_with_options_and_pattern(
                                &url,
                                &req.web_fetch_allowed_domains,
                                &req.web_fetch_blocked_domains,
                                resolved.effective_proxy.as_ref(),
                                spill_dir.as_deref(),
                                pattern.as_deref(),
                            )
                            .await;
                            let (out_text, is_err) = match outcome {
                                WebFetchOutcome::Success { content, .. } => (content, false),
                                WebFetchOutcome::Error {
                                    error_code,
                                    message,
                                    ..
                                } => (format!("[{error_code}] {message}"), true),
                            };
                            task_guard.record_local_tool_call(
                                ToolCallStart {
                                    call_id: call_id.clone(),
                                    route: ToolCallRoute::GatewayManaged,
                                    wire_tool_name: wire_name.clone(),
                                    client_tool_name: None,
                                    is_orchestrator: false,
                                    code_language: None,
                                    wire_arguments: raw_args.clone(),
                                    client_arguments: None,
                                    v4a_auto_repaired: false,
                                },
                                ToolCallOutcome {
                                    status: if is_err {
                                        ToolCallStatus::Error
                                    } else {
                                        ToolCallStatus::Ok
                                    },
                                    error_kind: is_err.then_some(ToolCallErrorKind::ToolError),
                                    raw_bytes: out_text.len(),
                                    text: out_text.clone(),
                                    images: Vec::new(),
                                    forwarded_output: None,
                                    sanitized: false,
                                    spilled: false,
                                },
                                started_ms,
                            );
                            local_continuation_parts.push(CanonicalPart::ToolCall {
                                call_id: call_id.clone(),
                                name: wire_name,
                                arguments: raw_args,
                                is_custom: raw_is_custom,
                            });
                            local_result_parts.push(CanonicalPart::ToolResult {
                                call_id,
                                output: out_text,
                                is_error: is_err,
                            });
                        }
                        VirtualizedCallDecision::ReadContextSpill { path, .. } => {
                            local_continuation_parts.push(CanonicalPart::ToolCall {
                                call_id: call_id.clone(),
                                name: wire_name,
                                arguments: raw_args,
                                is_custom: raw_is_custom,
                            });
                            local_result_parts.push(CanonicalPart::ToolResult {
                                call_id,
                                output: format!(
                                    "Spill file `{path}` not active on native API turn."
                                ),
                                is_error: true,
                            });
                        }
                        VirtualizedCallDecision::ForwardToClient {
                            client_name,
                            arguments,
                            is_custom,
                            patch_repaired,
                        } => {
                            let route = if wire_name.starts_with("ns__") {
                                ToolCallRoute::NamespaceHub
                            } else if wire_name == "call_client_tool" {
                                ToolCallRoute::GenericDispatcher
                            } else {
                                ToolCallRoute::ClientRoundtrip
                            };
                            let is_orch = req
                                .client_tools
                                .iter()
                                .find(|t| t.name == client_name)
                                .is_some_and(is_code_mode_orchestrator_tool);
                            task_guard.record_client_tool(&client_name);
                            task_guard.begin_tool_call(ToolCallStart {
                                call_id: call_id.clone(),
                                route,
                                wire_tool_name: wire_name,
                                client_tool_name: Some(client_name.clone()),
                                is_orchestrator: is_orch,
                                code_language: is_orch.then(|| "js".to_owned()),
                                wire_arguments: raw_args,
                                client_arguments: Some(arguments.clone()),
                                v4a_auto_repaired: patch_repaired,
                            });
                            forwarded_any_to_client = true;
                            final_stop_reason = "tool_use".to_owned();
                            client_forwarded_parts.push(CanonicalPart::ToolCall {
                                call_id: call_id.clone(),
                                name: client_name.clone(),
                                arguments: arguments.clone(),
                                is_custom,
                            });
                            let _ = events_tx
                                .send(TurnEvent::ClientToolCall {
                                    call_id,
                                    name: client_name,
                                    arguments,
                                    is_custom,
                                })
                                .await;
                        }
                    }
                } else {
                    let is_custom = raw_is_custom || custom_tool_names.contains(&wire_name);
                    let unwrapped_args = if is_custom && !raw_is_custom {
                        serde_json::from_str::<Value>(&raw_args)
                            .ok()
                            .and_then(|v| {
                                v.get("input")
                                    .or_else(|| v.get("code"))
                                    .or_else(|| v.get("patch"))
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                            })
                            .unwrap_or_else(|| raw_args.clone())
                    } else {
                        raw_args.clone()
                    };
                    let is_orch = req
                        .client_tools
                        .iter()
                        .find(|t| t.name == wire_name)
                        .is_some_and(is_code_mode_orchestrator_tool);
                    task_guard.record_client_tool(&wire_name);
                    task_guard.begin_tool_call(ToolCallStart {
                        call_id: call_id.clone(),
                        route: ToolCallRoute::ClientRoundtrip,
                        wire_tool_name: wire_name.clone(),
                        client_tool_name: Some(wire_name.clone()),
                        is_orchestrator: is_orch,
                        code_language: is_orch.then(|| "js".to_owned()),
                        wire_arguments: raw_args,
                        client_arguments: None,
                        v4a_auto_repaired: false,
                    });
                    forwarded_any_to_client = true;
                    final_stop_reason = "tool_use".to_owned();
                    client_forwarded_parts.push(CanonicalPart::ToolCall {
                        call_id: call_id.clone(),
                        name: wire_name.clone(),
                        arguments: unwrapped_args.clone(),
                        is_custom,
                    });
                    let _ = events_tx
                        .send(TurnEvent::ClientToolCall {
                            call_id,
                            name: wire_name,
                            arguments: unwrapped_args,
                            is_custom,
                        })
                        .await;
                }
            }

            if forwarded_any_to_client || local_result_parts.is_empty() {
                final_assistant_parts = client_forwarded_parts;
                break;
            }

            // Gateway answered 100% of this step's tool calls locally in 0-RTT; continue seamlessly!
            continuations = continuations.saturating_add(1);
            accumulated_messages.push(CanonicalMessage {
                role: CanonicalRole::Assistant,
                parts: local_continuation_parts.clone(),
            });
            accumulated_messages.push(CanonicalMessage {
                role: CanonicalRole::User,
                parts: local_result_parts.clone(),
            });
            working_messages.push(CanonicalMessage {
                role: CanonicalRole::Assistant,
                parts: local_continuation_parts,
            });
            working_messages.push(CanonicalMessage {
                role: CanonicalRole::User,
                parts: local_result_parts,
            });
        }

        if req.auxiliary_task.is_none() {
            if !final_assistant_parts.is_empty() {
                accumulated_messages.push(CanonicalMessage {
                    role: CanonicalRole::Assistant,
                    parts: final_assistant_parts,
                });
            }
            telemetry_for_worker.provider_registry().save_continuation(
                response_id_for_store,
                StoredResponseContinuation {
                    messages: accumulated_messages,
                    developer_slots: req.developer_slots.clone(),
                    client_tools: req.client_tools.clone(),
                    detected_harness,
                },
            );
        }

        let upstream_wire = UpstreamWireSummary {
            provider_id: provider_id_str,
            driver: resolved.profile.driver,
            wire_protocol: resolved.capabilities.wire_protocol,
            optimization_mode: opt_mode,
            isomorphic_wire: resolved.capabilities.is_isomorphic_ingress(protocol),
            raw_system_bytes,
            forwarded_system_bytes: forwarded_system_bytes
                .max(last_wire_bytes.min(forwarded_system_bytes)),
            raw_tool_count,
            raw_tool_bytes,
            forwarded_tool_count,
            forwarded_tool_bytes,
            spill_enabled: resolved
                .capabilities
                .effective_spill_limit(&resolved.optimization)
                .is_some(),
            cache_breakpoints_aligned: last_breakpoints,
            zero_rtt_continuations: continuations,
        };
        task_guard.set_upstream_wire_summary(upstream_wire);

        let web_cfg = WebToolBridgeConfig {
            enable_native_web_search: req.enable_native_web_search,
            force_native_web_search: req.force_native_web_search,
            enable_native_web_fetch: req.enable_native_web_fetch,
            force_native_web_fetch: req.force_native_web_fetch,
        };
        let base_hash = format_conversation_hash_hex(&hash_conversation_state(
            None,
            &req.client_tools,
            web_cfg,
            &resolved_model_for_worker,
            &req.messages,
        ));
        let distill_harness = req.distillation_harness();
        let diag = TurnCompletionDiagnostics {
            routing_strategy: LowCacheRoutingStrategy::ColdNewSession,
            delta_start_index: 0,
            session_turn_index: 1,
            session_idle_ms: None,
            previous_session_context_tokens: 0,
            model_switched: false,
            previous_model: None,
            previous_response_id: req.previous_response_id.as_deref(),
            prefix_hash_hex: None,
            base_state_hash_hex: &base_hash,
            reasoning_effort: req.reasoning_effort,
            web_cfg,
            acp_system_append: bridge_rules.as_deref(),
            tools_delta_xml: None,
            outbound_acp_blocks: &[],
            messages: &req.messages,
            client_tools: &req.client_tools,
            distill_harness,
            developer_slots: &req.developer_slots,
        };
        task_guard.complete_with_diagnostics(final_stop_reason.clone(), total_usage, Some(diag));
        let _ = events_tx
            .send(TurnEvent::Completed {
                usage: total_usage,
                stop_reason: final_stop_reason,
            })
            .await;
    });

    match step0_ready_rx.await {
        Ok(Ok(())) => Ok(ActiveTurnStream::from_parts(
            resolved_model,
            response_id,
            events_rx,
            cancel_guard,
            worker,
        )),
        Ok(Err(gw_err)) => Err(gw_err),
        Err(_) => Err(GatewayError::UpstreamProviderHttp {
            status: StatusCode::BAD_GATEWAY,
            openai_type: "api_error",
            openai_code: "upstream_cancelled",
            anthropic_type: "api_error",
            message: "upstream HTTP turn terminated before response headers".to_owned(),
        }),
    }
}

fn execute_stateful_dialect_turn(
    telemetry: &Arc<TelemetryHub>,
    cfg: &GatewayRuntimeConfig,
    resolved: ResolvedRequestProvider,
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
    telemetry
        .provider_registry()
        .restore_continuation_into_request(&mut req);
    req.materialize_system_instructions();

    let dialect = match resolved.profile.driver {
        ProviderDriverKind::CodexAppServer => StatefulDialect::CodexAppServer,
        ProviderDriverKind::GrokAcp => StatefulDialect::GrokAcp,
        _ => {
            return Err(GatewayError::NotFound {
                resource: resolved.profile.id.as_str().to_owned(),
            });
        }
    };

    let program = resolved
        .profile
        .command
        .clone()
        .or_else(|| {
            resolved
                .profile
                .driver
                .default_binary_name()
                .and_then(which_in_path)
        })
        .unwrap_or_else(|| {
            PathBuf::from(
                resolved
                    .profile
                    .driver
                    .default_binary_name()
                    .unwrap_or("codex"),
            )
        });
    let args = if resolved.profile.command_args.is_empty() {
        dialect.default_args()
    } else {
        resolved.profile.command_args.clone()
    };

    let resolved_model = resolve_non_agy_model(&resolved.profile, &req);
    let requested_label = req
        .requested_model
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| resolved_model.as_str().to_owned());
    let opt_mode = resolved.optimization.mode();
    let provider_id_str = resolved.profile.id.as_str().to_owned();

    let task_guard = telemetry.begin_task(
        response_id.clone(),
        protocol,
        req.stream,
        requested_label,
        req.auxiliary_task,
        &req.messages,
    );
    task_guard.set_provider_and_optimization(&provider_id_str, opt_mode);
    let sess_label = format!("stateful:{}:{}", provider_id_str, resolved_model.as_str());
    task_guard.bind_session(&sess_label, false, resolved_model.as_str());

    let raw_system_bytes = measure_system_slot_bytes(&req.messages);
    let raw_tool_count = req.client_tools.len();
    let raw_tool_bytes = estimate_raw_tool_bytes(&req.client_tools);
    let detected_harness = req
        .detected_harness
        .or_else(|| detect_harness_by_tools(&req.client_tools));

    let rendered = req.render_messages_for_outbound(0, None);
    let forwarded_system_bytes = measure_system_slot_bytes(&rendered);
    let (mut sys_blocks, conv_msgs) = split_system_and_conversation_messages(
        &rendered,
        resolved.optimization.strip_billing_noise,
    );

    let mut prompt_buf = String::with_capacity(1024);
    let mut turn_images: Vec<StatefulInputImage> = Vec::new();
    for m in &conv_msgs {
        let tag = match m.role {
            CanonicalRole::User => "user",
            CanonicalRole::Assistant => "assistant",
        };
        for p in &m.parts {
            match p {
                CanonicalPart::Text(t) => {
                    prompt_buf.push_str(&format!("<{tag}>\n{t}\n</{tag}>\n"));
                }
                CanonicalPart::Reasoning(r) | CanonicalPart::SignedThinking { thinking: r, .. } => {
                    prompt_buf.push_str(&format!(
                        "<{tag}>\n<thinking>\n{r}\n</thinking>\n</{tag}>\n"
                    ));
                }
                CanonicalPart::RedactedThinking { .. } => {}
                CanonicalPart::ToolCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } => {
                    prompt_buf.push_str(&format!(
                        "<assistant>\n<tool_call id=\"{call_id}\" name=\"{name}\">\n{arguments}\n</tool_call>\n</assistant>\n"
                    ));
                }
                CanonicalPart::ToolResult {
                    call_id,
                    output,
                    is_error,
                } => {
                    prompt_buf.push_str(&format!(
                        "<user>\n<tool_result call_id=\"{call_id}\" is_error=\"{is_error}\">\n{output}\n</tool_result>\n</user>\n"
                    ));
                }
                CanonicalPart::ImageBase64 { media_type, data } => {
                    let img_idx = turn_images.len() + 1;
                    turn_images.push(StatefulInputImage {
                        media_type: media_type.clone(),
                        data: data.clone(),
                    });
                    prompt_buf.push_str(&format!(
                        "<{tag}>[image #{img_idx}: {media_type}]</{tag}>\n"
                    ));
                }
            }
        }
    }

    let mut env = Vec::new();
    if let Some(ref k) = resolved.api_key
        && let Some(env_name) = resolved.profile.driver.default_api_key_env()
    {
        env.push((env_name.to_owned(), k.clone()));
    }
    if let Some(ref p) = resolved.effective_proxy {
        env.push(("HTTPS_PROXY".to_owned(), p.as_str().to_owned()));
        env.push(("HTTP_PROXY".to_owned(), p.as_str().to_owned()));
    }

    let spawn_cfg = StdioSpawnConfig {
        program,
        args,
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        env,
        max_line_bytes: cfg.limits.max_line_bytes().get(),
    };

    let proto_hint = match protocol {
        TaskProtocol::OpenaiResponses => ProtocolHint::OpenAiResponses,
        TaskProtocol::AnthropicMessages => ProtocolHint::AnthropicMessages,
    };
    let mcp_enabled = cfg.mcp_tool_bridge.load(Ordering::Relaxed);
    let use_loopback_mcp = matches!(dialect, StatefulDialect::GrokAcp)
        && !req.client_tools.is_empty()
        && mcp_enabled
        && cfg.mcp_server.is_some();

    let mut noop_spill = NoopSpillSink;
    let mut mcp_session_handle: Option<Arc<McpSessionHandle>> = None;
    let mut dynamic_catalog: Option<McpSessionCatalog> = None;
    let mut mcp_servers = Vec::new();
    let mut dynamic_tools = Vec::new();

    if use_loopback_mcp && let Some(ref mcp_srv) = cfg.mcp_server {
        let catalog = partition_tools_with_config(
            proto_hint,
            &req.client_tools,
            VirtualizerPlanConfig {
                session: McpSessionOptions {
                    enable_gateway_web_fetch: req.enable_native_web_fetch,
                    enable_context_spill_reader: false,
                },
                target_mode: VirtualizationTargetMode::AcpLoopbackMcp,
                budget_override: None,
                disable_virtualization: !resolved.optimization.tool_virtualization,
                disable_code_mode_enhancements: !resolved.optimization.code_mode_enhancements,
            },
            &mut noop_spill,
        );
        if !matches!(resolved.optimization.mode(), OptimizationMode::Passthrough) {
            let (append_str, _) = build_session_system_append_for_mcp_with_spill_sink(
                None,
                McpBridgePromptSpec {
                    bridged_tools: &catalog.bridged_pairs,
                    virtualized_hubs: &catalog.virtualized_hubs,
                    has_virtualized_dispatch: catalog.has_virtualized_dispatch,
                    has_gateway_web_fetch: catalog.has_gateway_web_fetch,
                    has_context_spill_reader: catalog.has_context_spill_reader,
                    orchestrator: catalog.orchestrator_contract(),
                },
                WebToolBridgeConfig::default(),
                "stateful",
                &mut noop_spill,
            );
            if !append_str.trim().is_empty() {
                sys_blocks.push(append_str);
            }
        }
        let handle = mcp_srv.register_session(catalog);
        mcp_servers.push(handle.server_config().clone());
        mcp_session_handle = Some(handle);
    } else if !req.client_tools.is_empty() {
        if resolved.optimization.tool_virtualization || resolved.optimization.code_mode_enhancements
        {
            let cat = partition_tools_with_config(
                proto_hint,
                &req.client_tools,
                VirtualizerPlanConfig {
                    session: McpSessionOptions {
                        enable_gateway_web_fetch: req.enable_native_web_fetch,
                        enable_context_spill_reader: false,
                    },
                    target_mode: VirtualizationTargetMode::NativeApiTools,
                    budget_override: Some(VirtualizerBudget::NATIVE_PLATFORM_API),
                    disable_virtualization: !resolved.optimization.tool_virtualization,
                    disable_code_mode_enhancements: !resolved.optimization.code_mode_enhancements,
                },
                &mut noop_spill,
            );
            if let Some(rules) = build_native_api_bridge_rules(
                &resolved.capabilities,
                cat.orchestrator_contract(),
                &cat.virtualized_hubs,
                cat.has_virtualized_dispatch,
                cat.has_gateway_web_fetch,
            ) {
                sys_blocks.push(rules);
            }
            dynamic_tools = cat
                .wire_tools
                .iter()
                .map(|wt| {
                    let orig_spec = cat
                        .lookup(&wt.name)
                        .and_then(|b| req.client_tools.iter().find(|s| s.name == b.client_name));
                    StatefulClientToolSpec {
                        name: wt.name.clone(),
                        description: wt.description.clone().unwrap_or_default(),
                        input_schema: wt.input_schema.clone(),
                        is_custom_freeform: orig_spec
                            .is_some_and(|s| matches!(s.kind, CanonicalToolKind::Custom { .. })),
                    }
                })
                .collect();
            dynamic_catalog = Some(cat);
        } else {
            dynamic_tools = req
                .client_tools
                .iter()
                .map(|t| match &t.kind {
                    CanonicalToolKind::Function { parameters_json } => StatefulClientToolSpec {
                        name: t.name.clone(),
                        description: t.description.clone().unwrap_or_default(),
                        input_schema: normalize_mcp_object_schema(parameters_json),
                        is_custom_freeform: false,
                    },
                    CanonicalToolKind::Custom { .. } => StatefulClientToolSpec {
                        name: t.name.clone(),
                        description: t.description.clone().unwrap_or_default(),
                        input_schema: json!({
                            "type": "object",
                            "properties": {
                                "input": { "type": "string", "description": "Raw tool input text." }
                            },
                            "required": ["input"]
                        }),
                        is_custom_freeform: true,
                    },
                })
                .collect();
        }
    }

    let forwarded_tool_count = if let Some(ref h) = mcp_session_handle {
        h.catalog().wire_tools.len()
    } else {
        dynamic_tools.len()
    };
    let forwarded_tool_bytes = if let Some(ref h) = mcp_session_handle {
        h.catalog().wire_tool_bytes
    } else {
        dynamic_tools.iter().fold(0usize, |acc, t| {
            acc.saturating_add(t.name.len())
                .saturating_add(t.description.len())
                .saturating_add(t.input_schema.to_string().len())
        })
    };
    let harness_id = mcp_session_handle
        .as_ref()
        .map(|h| h.catalog().profile_id)
        .or_else(|| dynamic_catalog.as_ref().map(|c| c.profile_id))
        .or_else(|| detected_harness.map(|h| h.id()))
        .unwrap_or("generic");
    let orch_mode = mcp_session_handle
        .as_ref()
        .and_then(|h| h.catalog().orchestrator_contract().map(|o| o.mode_id()))
        .or_else(|| {
            dynamic_catalog
                .as_ref()
                .and_then(|c| c.orchestrator_contract().map(|o| o.mode_id()))
        });
    task_guard.set_tool_context(harness_id, orch_mode, 1);

    let turn_input = StatefulTurnInput {
        dialect,
        model: resolved_model.as_str().to_owned(),
        effort: req.reasoning_effort,
        system_instructions: (!sys_blocks.is_empty()).then(|| sys_blocks.join("\n\n")),
        prompt_text: prompt_buf,
        images: turn_images,
        cwd: spawn_cfg.cwd.clone(),
        mcp_servers,
        dynamic_tools,
    };

    let cap = cfg.limits.channel_capacity().get();
    let (events_tx, events_rx) = mpsc::channel::<TurnEvent>(cap);
    let turn_cancel = CancellationToken::new();
    let cancel_child = turn_cancel.clone();
    let cancel_guard = turn_cancel.drop_guard();
    let resolved_model_worker = resolved_model.clone();
    let response_id_for_store = response_id.clone();
    let telemetry_for_worker = Arc::clone(telemetry);
    let spill_dir = cfg.spill_dir.clone();
    let timeout_secs = cfg.limits.turn_timeout_secs().get();
    let custom_tool_names: std::collections::HashSet<String> = req
        .client_tools
        .iter()
        .filter(|t| matches!(t.kind, CanonicalToolKind::Custom { .. }))
        .map(|t| t.name.clone())
        .collect();

    let worker = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        let (st_tx, mut st_rx) =
            mpsc::channel::<Result<StatefulStreamEvent, StatefulRpcError>>(128);
        let dyn_cat_for_peer = dynamic_catalog.clone();
        let tools_for_peer = req.client_tools.clone();
        let allowed_for_peer = req.web_fetch_allowed_domains.clone();
        let blocked_for_peer = req.web_fetch_blocked_domains.clone();
        let proxy_for_peer = resolved.effective_proxy.clone();
        let spill_for_peer = spill_dir.clone();
        let dyn_searches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dyn_heals = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dyn_fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dyn_searches_c = Arc::clone(&dyn_searches);
        let dyn_heals_c = Arc::clone(&dyn_heals);
        let dyn_fetches_c = Arc::clone(&dyn_fetches);
        let run_fut = async {
            let st_tx = st_tx;
            tokio::time::timeout_at(
                deadline,
                run_turn_on_spawned_peer_with_handler(
                    &spawn_cfg,
                    &turn_input,
                    &st_tx,
                    |wire_name, args_val| {
                        let dyn_cat = dyn_cat_for_peer.clone();
                        let tools = tools_for_peer.clone();
                        let allowed = allowed_for_peer.clone();
                        let blocked = blocked_for_peer.clone();
                        let proxy = proxy_for_peer.clone();
                        let spill = spill_for_peer.clone();
                        let searches = Arc::clone(&dyn_searches_c);
                        let heals = Arc::clone(&dyn_heals_c);
                        let fetches = Arc::clone(&dyn_fetches_c);
                        async move {
                            if let Some(ref cat) = dyn_cat {
                                match cat
                                    .routing_table
                                    .resolve_call(&wire_name, &args_val, &tools)
                                {
                                    VirtualizedCallDecision::LocalReply {
                                        text,
                                        is_error,
                                        kind,
                                    } => {
                                        match kind {
                                            LocalReplyKind::Search => {
                                                searches.fetch_add(1, Ordering::Relaxed);
                                            }
                                            LocalReplyKind::PreflightError
                                            | LocalReplyKind::DisabledTool => {
                                                heals.fetch_add(1, Ordering::Relaxed);
                                            }
                                            LocalReplyKind::SpillRead => {}
                                        }
                                        return DynamicToolDisposition::LocalReply {
                                            text,
                                            is_error,
                                        };
                                    }
                                    VirtualizedCallDecision::GatewayWebFetch { url, pattern } => {
                                        fetches.fetch_add(1, Ordering::Relaxed);
                                        let outcome = fetch_url_sanitized_with_options_and_pattern(
                                            &url,
                                            &allowed,
                                            &blocked,
                                            proxy.as_ref(),
                                            spill.as_deref(),
                                            pattern.as_deref(),
                                        )
                                        .await;
                                        let (text, is_error) = match outcome {
                                            WebFetchOutcome::Success { content, .. } => {
                                                (content, false)
                                            }
                                            WebFetchOutcome::Error {
                                                error_code,
                                                message,
                                                ..
                                            } => (format!("[{error_code}] {message}"), true),
                                        };
                                        return DynamicToolDisposition::LocalReply {
                                            text,
                                            is_error,
                                        };
                                    }
                                    VirtualizedCallDecision::ReadContextSpill { path, .. } => {
                                        return DynamicToolDisposition::LocalReply {
                                            text: format!("Spill file `{path}` unavailable."),
                                            is_error: true,
                                        };
                                    }
                                    VirtualizedCallDecision::ForwardToClient { .. } => {}
                                }
                            }
                            DynamicToolDisposition::SuspendToClient
                        }
                    },
                ),
            )
            .await
            .map_err(|_| StatefulRpcError::Timeout {
                elapsed_ms: timeout_secs.saturating_mul(1_000),
            })?
        };
        tokio::pin!(run_fut);

        let mut run_done = false;
        let mut usage = AcpUsage::default();
        let mut stop_reason = "end_turn".to_owned();
        let mut rpc_err: Option<StatefulRpcError> = None;
        let mut assistant_reasoning = String::new();
        let mut assistant_text = String::new();
        let mut assistant_tool_calls: Vec<CanonicalPart> = Vec::new();
        let mut zero_rtt_count = 0usize;

        loop {
            let has_mcp = mcp_session_handle.is_some();
            let mcp_recv_fut = async {
                match mcp_session_handle.as_ref() {
                    Some(handle) => {
                        let mut rx_guard = handle.call_rx().lock().await;
                        rx_guard.recv().await
                    }
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                () = cancel_child.cancelled() => return,
                Some(incoming) = mcp_recv_fut, if has_mcp => {
                    let Some(ref handle) = mcp_session_handle else { continue };
                    match handle.resolve_incoming_call(incoming, &req.client_tools) {
                        ResolvedMcpIncoming::ImmediateLocalReply { reply_tx, reply, .. } => {
                            zero_rtt_count = zero_rtt_count.saturating_add(1);
                            let _ = reply_tx.send(reply);
                        }
                        ResolvedMcpIncoming::GatewayWebFetch { url, pattern, reply_tx } => {
                            zero_rtt_count = zero_rtt_count.saturating_add(1);
                            task_guard.record_web_fetch();
                            let outcome = fetch_url_sanitized_with_options_and_pattern(
                                &url,
                                &req.web_fetch_allowed_domains,
                                &req.web_fetch_blocked_domains,
                                resolved.effective_proxy.as_ref(),
                                spill_dir.as_deref(),
                                pattern.as_deref(),
                            )
                            .await;
                            let (text, is_error) = match outcome {
                                WebFetchOutcome::Success { content, .. } => (content, false),
                                WebFetchOutcome::Error { error_code, message, .. } => {
                                    (format!("[{error_code}] {message}"), true)
                                }
                            };
                            let _ = reply_tx.send(McpToolCallReply {
                                content: vec![McpContentItem::Text { text }],
                                is_error,
                            });
                        }
                        ResolvedMcpIncoming::ReadContextSpill { path, reply_tx, .. } => {
                            let _ = reply_tx.send(McpToolCallReply {
                                content: vec![McpContentItem::Text {
                                    text: format!("Spill file `{path}` unavailable."),
                                }],
                                is_error: true,
                            });
                        }
                        ResolvedMcpIncoming::ForwardToClient {
                            client_name,
                            arguments,
                            is_custom,
                            patch_repaired,
                            reply_tx: _,
                        } => {
                            let call_id = format!("call_{}", unix_timestamp_ms());
                            task_guard.record_client_tool(&client_name);
                            task_guard.begin_tool_call(ToolCallStart {
                                call_id: call_id.clone(),
                                route: ToolCallRoute::ClientRoundtrip,
                                wire_tool_name: client_name.clone(),
                                client_tool_name: Some(client_name.clone()),
                                is_orchestrator: false,
                                code_language: None,
                                wire_arguments: arguments.clone(),
                                client_arguments: Some(arguments.clone()),
                                v4a_auto_repaired: patch_repaired,
                            });
                            assistant_tool_calls.push(CanonicalPart::ToolCall {
                                call_id: call_id.clone(),
                                name: client_name.clone(),
                                arguments: arguments.clone(),
                                is_custom,
                            });
                            stop_reason = "tool_use".to_owned();
                            let _ = events_tx
                                .send(TurnEvent::ClientToolCall {
                                    call_id,
                                    name: client_name,
                                    arguments,
                                    is_custom,
                                })
                                .await;
                            break;
                        }
                    }
                }
                maybe_ev = st_rx.recv() => {
                    match maybe_ev {
                        Some(Ok(ev)) => match ev {
                            StatefulStreamEvent::ReasoningDelta(d) => {
                                assistant_reasoning.push_str(&d);
                                task_guard.record_thought(&d);
                                let _ = events_tx.send(TurnEvent::ThoughtDelta(d)).await;
                            }
                            StatefulStreamEvent::TextDelta(d) => {
                                assistant_text.push_str(&d);
                                task_guard.record_text(&d);
                                let _ = events_tx.send(TurnEvent::TextDelta(d)).await;
                            }
                            StatefulStreamEvent::ToolCall {
                                id,
                                name: wire_name,
                                arguments_json: raw_args,
                                is_custom: raw_is_custom,
                            } => {
                                let (client_name, client_args, is_custom, patch_repaired) =
                                    if let Some(ref cat) = dynamic_catalog {
                                        let parsed = if raw_is_custom {
                                            json!({ "code": raw_args, "patch": raw_args, "input": raw_args })
                                        } else {
                                            serde_json::from_str::<Value>(&raw_args)
                                                .unwrap_or_else(|_| json!({}))
                                        };
                                        match cat.routing_table.resolve_call(
                                            &wire_name,
                                            &parsed,
                                            &req.client_tools,
                                        ) {
                                            VirtualizedCallDecision::ForwardToClient {
                                                client_name,
                                                arguments,
                                                is_custom,
                                                patch_repaired,
                                            } => (client_name, arguments, is_custom, patch_repaired),
                                            _ => (
                                                wire_name.clone(),
                                                raw_args.clone(),
                                                raw_is_custom
                                                    || custom_tool_names.contains(&wire_name),
                                                false,
                                            ),
                                        }
                                    } else {
                                        (
                                            wire_name.clone(),
                                            raw_args.clone(),
                                            raw_is_custom || custom_tool_names.contains(&wire_name),
                                            false,
                                        )
                                    };
                                task_guard.record_client_tool(&client_name);
                                task_guard.begin_tool_call(ToolCallStart {
                                    call_id: id.clone(),
                                    route: ToolCallRoute::ClientRoundtrip,
                                    wire_tool_name: wire_name,
                                    client_tool_name: Some(client_name.clone()),
                                    is_orchestrator: false,
                                    code_language: None,
                                    wire_arguments: raw_args,
                                    client_arguments: Some(client_args.clone()),
                                    v4a_auto_repaired: patch_repaired,
                                });
                                assistant_tool_calls.push(CanonicalPart::ToolCall {
                                    call_id: id.clone(),
                                    name: client_name.clone(),
                                    arguments: client_args.clone(),
                                    is_custom,
                                });
                                stop_reason = "tool_use".to_owned();
                                let _ = events_tx
                                    .send(TurnEvent::ClientToolCall {
                                        call_id: id,
                                        name: client_name,
                                        arguments: client_args,
                                        is_custom,
                                    })
                                    .await;
                            }
                            StatefulStreamEvent::Usage(u) => {
                                usage = u;
                            }
                            StatefulStreamEvent::Completed { stop_reason: sr } => {
                                stop_reason = sr;
                            }
                        },
                        Some(Err(e)) => {
                            rpc_err = Some(e);
                            break;
                        }
                        None => break,
                    }
                }
                res = &mut run_fut, if !run_done => {
                    run_done = true;
                    if let Err(e) = res {
                        rpc_err = Some(e);
                        break;
                    }
                }
            }
        }

        if let Some(err) = rpc_err {
            for _ in 0..dyn_searches.load(Ordering::Relaxed) {
                task_guard.record_zero_rtt_search();
            }
            for _ in 0..dyn_heals.load(Ordering::Relaxed) {
                task_guard.record_zero_rtt_preflight_heal();
            }
            for _ in 0..dyn_fetches.load(Ordering::Relaxed) {
                task_guard.record_web_fetch();
            }
            let acp_err = match err {
                StatefulRpcError::RateLimited { message, .. } => AcpError::UpstreamRateLimited {
                    model: resolved_model_worker.as_str().to_owned(),
                    detail: message,
                },
                StatefulRpcError::Timeout { elapsed_ms } => AcpError::TurnTimeout {
                    seconds: (elapsed_ms / 1_000).max(1),
                },
                other => AcpError::Rpc {
                    method: "stateful_stdio",
                    code: -32000,
                    message: other.to_string(),
                },
            };
            task_guard.fail(acp_err.to_string());
            let _ = events_tx.send(TurnEvent::Failed(Arc::new(acp_err))).await;
            return;
        }

        if req.auxiliary_task.is_none() {
            let mut accumulated = req.messages.clone();
            let mut final_parts = Vec::new();
            if !assistant_reasoning.is_empty() {
                final_parts.push(CanonicalPart::Reasoning(assistant_reasoning));
            }
            if !assistant_text.is_empty() {
                final_parts.push(CanonicalPart::Text(assistant_text));
            }
            final_parts.extend(assistant_tool_calls);
            if !final_parts.is_empty() {
                accumulated.push(CanonicalMessage {
                    role: CanonicalRole::Assistant,
                    parts: final_parts,
                });
            }
            telemetry_for_worker.provider_registry().save_continuation(
                response_id_for_store,
                StoredResponseContinuation {
                    messages: accumulated,
                    developer_slots: req.developer_slots.clone(),
                    client_tools: req.client_tools.clone(),
                    detected_harness,
                },
            );
        }

        let ds = dyn_searches.load(Ordering::Relaxed);
        let dh = dyn_heals.load(Ordering::Relaxed);
        let df = dyn_fetches.load(Ordering::Relaxed);
        for _ in 0..ds {
            task_guard.record_zero_rtt_search();
        }
        for _ in 0..dh {
            task_guard.record_zero_rtt_preflight_heal();
        }
        for _ in 0..df {
            task_guard.record_web_fetch();
        }
        zero_rtt_count = zero_rtt_count.saturating_add(ds + dh + df);

        task_guard.set_upstream_wire_summary(UpstreamWireSummary {
            provider_id: provider_id_str,
            driver: resolved.profile.driver,
            wire_protocol: resolved.capabilities.wire_protocol,
            optimization_mode: opt_mode,
            isomorphic_wire: false,
            raw_system_bytes,
            forwarded_system_bytes,
            raw_tool_count,
            raw_tool_bytes,
            forwarded_tool_count,
            forwarded_tool_bytes,
            spill_enabled: false,
            cache_breakpoints_aligned: 0,
            zero_rtt_continuations: zero_rtt_count,
        });

        let web_cfg = WebToolBridgeConfig::default();
        let base_hash = format_conversation_hash_hex(&hash_conversation_state(
            None,
            &req.client_tools,
            web_cfg,
            &resolved_model_worker,
            &req.messages,
        ));
        let diag = TurnCompletionDiagnostics {
            routing_strategy: LowCacheRoutingStrategy::ColdNewSession,
            delta_start_index: 0,
            session_turn_index: 1,
            session_idle_ms: None,
            previous_session_context_tokens: 0,
            model_switched: false,
            previous_model: None,
            previous_response_id: req.previous_response_id.as_deref(),
            prefix_hash_hex: None,
            base_state_hash_hex: &base_hash,
            reasoning_effort: req.reasoning_effort,
            web_cfg,
            acp_system_append: None,
            tools_delta_xml: None,
            outbound_acp_blocks: &[],
            messages: &req.messages,
            client_tools: &req.client_tools,
            distill_harness: req.distillation_harness(),
            developer_slots: &req.developer_slots,
        };
        task_guard.complete_with_diagnostics(stop_reason.clone(), usage, Some(diag));
        let _ = events_tx
            .send(TurnEvent::Completed { usage, stop_reason })
            .await;
    });

    Ok(ActiveTurnStream::from_parts(
        resolved_model,
        response_id,
        events_rx,
        cancel_guard,
        worker,
    ))
}
