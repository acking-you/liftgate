//! Headless execution engine for `lg provider` (`list`, `add`, `remove`, `use`, `test`, `models`, `opt`).

use std::io::Read as _;
use std::time::Duration;

use liftgate_server::provider_runtime::ProviderRuntimeRegistry;
use liftgate_types::config::{ModelId, ProxyUrl};
use liftgate_types::provider::{
    ApiKeySource, OptimizationMode, OptimizationPolicy, ProviderDriverKind, ProviderId,
    ProviderModelEntry, ProviderProfileConfig, context_limits_for_driver,
};
use liftgate_types::telemetry::ProviderTelemetrySummary;
use serde::Serialize;
use serde_json::{Value, json};

use crate::cli::{
    ProviderAddArgs, ProviderArgs, ProviderCommand, ProviderModelsArgs, ProviderOptArgs,
    ProviderRemoveArgs, ProviderTestArgs, ProviderUseArgs,
};
use crate::client::GatewayClient;
use crate::daemon::{is_pid_alive, resolve_client_api_key};
use crate::error::CliError;
use crate::state::{
    EffectiveTargetMode, StatePaths, resolve_effective_agy_proxy, resolve_effective_target_mode,
};

const MAX_STDIN_SECRET_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct ProviderListReport {
    pub active_provider: String,
    pub optimization_mode: OptimizationMode,
    pub optimization_policy: OptimizationPolicy,
    pub providers: Vec<ProviderTelemetrySummary>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderMutationReport {
    pub headline: String,
    pub provider_id: String,
    pub active_provider: String,
    pub daemon_synced: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderModelsReport {
    pub provider_id: String,
    pub driver: ProviderDriverKind,
    pub default_model: String,
    pub fast_model: Option<String>,
    pub title_model: Option<String>,
    pub models: Vec<ProviderModelRow>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderModelRow {
    pub id: String,
    pub display_name: String,
    pub context_window_tokens: u64,
    pub auto_compact_token_limit: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderOptReport {
    pub updated: bool,
    pub mode: OptimizationMode,
    pub policy: OptimizationPolicy,
    pub daemon_synced: bool,
}

#[derive(Clone, Debug)]
pub enum ProviderCommandOutcome {
    List {
        report: ProviderListReport,
        json: bool,
    },
    Mutated {
        report: ProviderMutationReport,
        json: bool,
    },
    Test {
        result: Value,
        json: bool,
    },
    Models {
        report: ProviderModelsReport,
        json: bool,
    },
    Opt {
        report: ProviderOptReport,
        json: bool,
    },
}

pub async fn execute_provider_command(
    state_paths: &StatePaths,
    args: ProviderArgs,
) -> Result<ProviderCommandOutcome, CliError> {
    match args.action {
        None => Ok(ProviderCommandOutcome::List {
            report: inspect_providers(state_paths).await?,
            json: args.json,
        }),
        Some(ProviderCommand::List(sub)) => Ok(ProviderCommandOutcome::List {
            report: inspect_providers(state_paths).await?,
            json: args.json || sub.json,
        }),
        Some(ProviderCommand::Add(sub)) => {
            let json_out = args.json || sub.json;
            let report = execute_provider_add(state_paths, *sub).await?;
            Ok(ProviderCommandOutcome::Mutated {
                report,
                json: json_out,
            })
        }
        Some(ProviderCommand::Remove(sub)) => {
            let json_out = args.json || sub.json;
            let report = execute_provider_remove(state_paths, sub).await?;
            Ok(ProviderCommandOutcome::Mutated {
                report,
                json: json_out,
            })
        }
        Some(ProviderCommand::Use(sub)) => {
            let json_out = args.json || sub.json;
            let report = execute_provider_use(state_paths, sub).await?;
            Ok(ProviderCommandOutcome::Mutated {
                report,
                json: json_out,
            })
        }
        Some(ProviderCommand::Test(sub)) => {
            let json_out = args.json || sub.json;
            let result = execute_provider_test(state_paths, sub).await?;
            Ok(ProviderCommandOutcome::Test {
                result,
                json: json_out,
            })
        }
        Some(ProviderCommand::Models(sub)) => {
            let json_out = args.json || sub.json;
            let report = execute_provider_models(state_paths, sub).await?;
            Ok(ProviderCommandOutcome::Models {
                report,
                json: json_out,
            })
        }
        Some(ProviderCommand::Opt(sub)) => {
            let json_out = args.json || sub.json;
            let report = execute_provider_opt(state_paths, sub).await?;
            Ok(ProviderCommandOutcome::Opt {
                report,
                json: json_out,
            })
        }
    }
}

fn load_local_registry(state_paths: &StatePaths) -> Result<ProviderRuntimeRegistry, CliError> {
    let settings = state_paths.read_settings()?;
    let default_proxy = resolve_effective_agy_proxy(None, &settings)?;
    Ok(ProviderRuntimeRegistry::load_from_dir(
        state_paths.root(),
        &ModelId::fallback_primary(),
        default_proxy,
    ))
}

async fn try_live_gateway_client(state_paths: &StatePaths) -> Option<GatewayClient> {
    let settings = state_paths.read_settings().ok()?;
    match resolve_effective_target_mode(None, None, None, &settings).ok()? {
        EffectiveTargetMode::Remote { endpoint, api_key } => {
            let client = GatewayClient::from_endpoint(endpoint, api_key);
            client
                .check_health(Duration::from_millis(800))
                .await
                .then_some(client)
        }
        EffectiveTargetMode::Local { explicit_listen } => {
            let daemon = state_paths.read_daemon().ok().flatten();
            if let Some(addr) = explicit_listen {
                let key = resolve_client_api_key(None, daemon.as_ref(), addr);
                let client = GatewayClient::new(addr, key);
                return client
                    .check_health(Duration::from_millis(600))
                    .await
                    .then_some(client);
            }
            if let Some(d) = daemon
                && is_pid_alive(d.pid)
            {
                let key = resolve_client_api_key(None, Some(&d), d.listen);
                let client = GatewayClient::new(d.listen, key);
                if client.check_health(Duration::from_millis(600)).await {
                    return Some(client);
                }
            }
            None
        }
    }
}

async fn inspect_providers(state_paths: &StatePaths) -> Result<ProviderListReport, CliError> {
    if let Some(client) = try_live_gateway_client(state_paths).await
        && let Ok(snap) = client.fetch_telemetry().await
        && !snap.providers.is_empty()
    {
        return Ok(ProviderListReport {
            active_provider: snap.active_provider,
            optimization_mode: snap.optimization_mode,
            optimization_policy: snap.optimization_policy,
            providers: snap.providers,
        });
    }
    let reg = load_local_registry(state_paths)?;
    let (active_provider, optimization_mode, optimization_policy, providers) =
        reg.snapshot_fields();
    Ok(ProviderListReport {
        active_provider,
        optimization_mode,
        optimization_policy,
        providers,
    })
}

fn infer_driver_for_id(
    id: &ProviderId,
    explicit_driver: Option<&str>,
) -> Result<ProviderDriverKind, CliError> {
    if let Some(raw_drv) = explicit_driver.map(str::trim).filter(|s| !s.is_empty()) {
        return ProviderDriverKind::from_slug(raw_drv).ok_or_else(|| CliError::ProviderConfig {
            message: format!(
                "unknown provider driver `{raw_drv}`; expected `agy-acp`, `openai`, `anthropic`, `gemini`, `codex-app-server`, or `grok-acp`"
            ),
        });
    }
    match id.as_str() {
        ProviderId::AGY => Ok(ProviderDriverKind::AgyAcp),
        ProviderId::OPENAI => Ok(ProviderDriverKind::OpenAiResponses),
        ProviderId::ANTHROPIC => Ok(ProviderDriverKind::AnthropicMessages),
        ProviderId::GEMINI => Ok(ProviderDriverKind::GoogleGemini),
        ProviderId::CODEX_APP_SERVER => Ok(ProviderDriverKind::CodexAppServer),
        ProviderId::GROK_ACP => Ok(ProviderDriverKind::GrokAcp),
        other => Err(CliError::ProviderConfig {
            message: format!(
                "custom provider `{other}` requires `--driver <openai|anthropic|gemini|codex-app-server|grok-acp|agy-acp>`"
            ),
        }),
    }
}

fn read_secret_from_stdin() -> Result<String, CliError> {
    let mut buf = String::new();
    std::io::stdin()
        .take(MAX_STDIN_SECRET_BYTES)
        .read_to_string(&mut buf)
        .map_err(|e| CliError::ProviderConfig {
            message: format!("failed reading API key from stdin: {e}"),
        })?;
    let secret = buf.trim().to_owned();
    if secret.is_empty() {
        return Err(CliError::ProviderConfig {
            message: "`--api-key-stdin` received an empty API key on standard input".to_owned(),
        });
    }
    Ok(secret)
}

async fn execute_provider_add(
    state_paths: &StatePaths,
    args: ProviderAddArgs,
) -> Result<ProviderMutationReport, CliError> {
    let id = ProviderId::try_new(&args.id).map_err(|e| CliError::ProviderConfig {
        message: format!("invalid provider id `{}`: {e}", args.id),
    })?;
    let reg = load_local_registry(state_paths)?;
    let cfg = reg.config_snapshot();

    let mut profile = match cfg.get(&id).cloned() {
        Some(mut existing) => {
            if let Some(ref drv_raw) = args.driver {
                existing.driver = infer_driver_for_id(&id, Some(drv_raw))?;
            }
            existing
        }
        None => {
            let drv = infer_driver_for_id(&id, args.driver.as_deref())?;
            ProviderProfileConfig::builtin(id.clone(), drv)
        }
    };

    if let Some(name) = args
        .name
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
    {
        profile.display_name = name;
    }
    if let Some(ep) = args.endpoint.map(|s| s.trim().to_owned()) {
        profile.endpoint = (!ep.is_empty()).then_some(ep);
    }
    if let Some(dm) = args
        .default_model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        profile.default_model = ModelId::try_new(dm)?;
    }
    if let Some(fm) = args.fast_model.as_deref().map(str::trim) {
        profile.fast_model = if fm.is_empty() {
            None
        } else {
            Some(ModelId::try_new(fm)?)
        };
    }
    if let Some(tm) = args.title_model.as_deref().map(str::trim) {
        profile.title_model = if tm.is_empty() {
            None
        } else {
            Some(ModelId::try_new(tm)?)
        };
    }
    if let Some(bin) = args.bin {
        profile.command = Some(bin);
    }
    if let Some(raw_proxy) = args
        .proxy
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if matches!(
            raw_proxy.to_ascii_lowercase().as_str(),
            "off" | "none" | "direct"
        ) {
            profile.proxy = None;
            profile.force_direct_proxy = true;
        } else if raw_proxy.eq_ignore_ascii_case("inherit") {
            profile.proxy = None;
            profile.force_direct_proxy = false;
        } else {
            let p = ProxyUrl::try_new(raw_proxy)?;
            profile.proxy = Some(p);
            profile.force_direct_proxy = false;
        }
    }
    if let Some(raw_opt) = args.opt.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if matches!(
            raw_opt.to_ascii_lowercase().as_str(),
            "inherit" | "default" | "clear"
        ) {
            profile.optimization = None;
        } else {
            let pol = OptimizationPolicy::from_slug(raw_opt).ok_or_else(|| {
                CliError::ProviderConfig {
                    message: format!("invalid `--opt {raw_opt}`; expected `optimized`, `passthrough`, or `inherit`"),
                }
            })?;
            profile.optimization = Some(pol);
        }
    }

    let secret_to_store = if args.api_key_stdin {
        let secret = read_secret_from_stdin()?;
        profile.auth = ApiKeySource::Stored;
        Some(secret)
    } else if let Some(env_var) = args
        .api_key_env
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
    {
        profile.auth = ApiKeySource::Env { var: env_var };
        None
    } else if args.clear_auth {
        profile.auth = ApiKeySource::None;
        Some(String::new())
    } else {
        None
    };

    reg.upsert_provider(profile, secret_to_store)
        .map_err(|message| CliError::ProviderConfig { message })?;
    if args.activate {
        reg.set_active_provider(id.clone())
            .map_err(|message| CliError::ProviderConfig { message })?;
    }

    let daemon_synced = if let Some(client) = try_live_gateway_client(state_paths).await {
        client.reload_providers().await.is_ok()
    } else {
        false
    };
    let active_provider = reg.config_snapshot().active_provider.as_str().to_owned();
    Ok(ProviderMutationReport {
        headline: format!("Configured provider `{id}`"),
        provider_id: id.as_str().to_owned(),
        active_provider,
        daemon_synced,
    })
}

async fn execute_provider_remove(
    state_paths: &StatePaths,
    args: ProviderRemoveArgs,
) -> Result<ProviderMutationReport, CliError> {
    let id = ProviderId::try_new(&args.id).map_err(|e| CliError::ProviderConfig {
        message: format!("invalid provider id `{}`: {e}", args.id),
    })?;
    let reg = load_local_registry(state_paths)?;
    let is_builtin = ProviderId::is_builtin_id_or_alias(id.as_str());
    let _ = reg
        .remove_provider(&id)
        .map_err(|message| CliError::ProviderConfig { message })?;

    let daemon_synced = if let Some(client) = try_live_gateway_client(state_paths).await {
        client.reload_providers().await.is_ok()
    } else {
        false
    };
    let active_provider = reg.config_snapshot().active_provider.as_str().to_owned();
    let headline = if is_builtin {
        format!("Reset built-in provider `{id}` to defaults and cleared stored secret")
    } else {
        format!("Removed custom provider `{id}`")
    };
    Ok(ProviderMutationReport {
        headline,
        provider_id: id.as_str().to_owned(),
        active_provider,
        daemon_synced,
    })
}

async fn execute_provider_use(
    state_paths: &StatePaths,
    args: ProviderUseArgs,
) -> Result<ProviderMutationReport, CliError> {
    let id = ProviderId::try_new(&args.id).map_err(|e| CliError::ProviderConfig {
        message: format!("invalid provider id `{}`: {e}", args.id),
    })?;
    let reg = load_local_registry(state_paths)?;
    reg.set_active_provider(id.clone())
        .map_err(|message| CliError::ProviderConfig { message })?;

    let daemon_synced = if let Some(client) = try_live_gateway_client(state_paths).await {
        client.set_active_provider(id.as_str()).await.is_ok()
            || client.reload_providers().await.is_ok()
    } else {
        false
    };
    Ok(ProviderMutationReport {
        headline: format!("Switched active default provider to `{id}`"),
        provider_id: id.as_str().to_owned(),
        active_provider: id.as_str().to_owned(),
        daemon_synced,
    })
}

async fn execute_provider_test(
    state_paths: &StatePaths,
    args: ProviderTestArgs,
) -> Result<Value, CliError> {
    let reg = load_local_registry(state_paths)?;
    let target_id = match args.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => ProviderId::try_new(raw).map_err(|e| CliError::ProviderConfig {
            message: format!("invalid provider id `{raw}`: {e}"),
        })?,
        None => reg.config_snapshot().active_provider,
    };

    if let Some(client) = try_live_gateway_client(state_paths).await
        && let Ok(res) = client.test_provider(target_id.as_str()).await
    {
        return Ok(res);
    }
    if target_id.as_str() == ProviderId::AGY {
        let settings = state_paths.read_settings()?;
        let agy_bin = crate::state::resolve_effective_agy_bin(None, &settings);
        return Ok(json!({
            "ok": agy_bin.is_some(),
            "provider_id": "agy",
            "driver": "agy-acp",
            "binary": agy_bin.map(|p| p.display().to_string()),
            "note": "daemon offline; verified local `agy` binary discovery",
        }));
    }
    let none_backend: Option<std::sync::Arc<liftgate_server::acp_client::LiveAcpClient>> = None;
    reg.probe_provider(&target_id, &none_backend)
        .await
        .map_err(|message| CliError::ProviderConfig { message })
}

async fn execute_provider_models(
    state_paths: &StatePaths,
    args: ProviderModelsArgs,
) -> Result<ProviderModelsReport, CliError> {
    let reg = load_local_registry(state_paths)?;
    let cfg = reg.config_snapshot();
    let target_id = match args.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => ProviderId::try_new(raw).map_err(|e| CliError::ProviderConfig {
            message: format!("invalid provider id `{raw}`: {e}"),
        })?,
        None => cfg.active_provider.clone(),
    };
    let profile = cfg
        .get(&target_id)
        .cloned()
        .ok_or_else(|| CliError::ProviderConfig {
            message: format!("unknown provider `{target_id}`"),
        })?;

    let mut rows: Vec<ProviderModelRow> = Vec::new();
    if let Some(client) = try_live_gateway_client(state_paths).await
        && let Ok(live_models) = client
            .fetch_models_for_route(Some(target_id.as_str()))
            .await
        && !live_models.is_empty()
    {
        rows = live_models
            .into_iter()
            .map(|m| {
                let (ctx, compact) = context_limits_for_driver(&m.id, profile.driver);
                ProviderModelRow {
                    id: m.id,
                    display_name: m.display_name,
                    context_window_tokens: ctx,
                    auto_compact_token_limit: compact,
                }
            })
            .collect();
    }
    if rows.is_empty() {
        let catalog: Vec<ProviderModelEntry> = profile.effective_models();
        rows = catalog
            .into_iter()
            .map(|m| ProviderModelRow {
                id: m.id,
                display_name: m.display_name,
                context_window_tokens: m.context_window_tokens,
                auto_compact_token_limit: m.auto_compact_token_limit,
            })
            .collect();
    }

    Ok(ProviderModelsReport {
        provider_id: target_id.as_str().to_owned(),
        driver: profile.driver,
        default_model: profile.default_model.as_str().to_owned(),
        fast_model: profile.fast_model.as_ref().map(|m| m.as_str().to_owned()),
        title_model: profile.title_model.as_ref().map(|m| m.as_str().to_owned()),
        models: rows,
    })
}

async fn execute_provider_opt(
    state_paths: &StatePaths,
    args: ProviderOptArgs,
) -> Result<ProviderOptReport, CliError> {
    let reg = load_local_registry(state_paths)?;
    let mut policy = reg.config_snapshot().optimization;
    let has_granular = args.prompt_distillation.is_some()
        || args.tool_virtualization.is_some()
        || args.code_mode.is_some()
        || args.strip_billing.is_some()
        || args.spill.is_some();

    if args.mode.is_none() && !has_granular {
        if let Some(client) = try_live_gateway_client(state_paths).await
            && let Ok(snap) = client.fetch_telemetry().await
        {
            policy = snap.optimization_policy;
        }
        return Ok(ProviderOptReport {
            updated: false,
            mode: policy.mode(),
            policy,
            daemon_synced: false,
        });
    }

    if let Some(ref raw_mode) = args.mode {
        policy =
            OptimizationPolicy::from_slug(raw_mode).ok_or_else(|| CliError::ProviderConfig {
                message: format!(
                    "invalid optimization mode `{raw_mode}`; expected `optimized` or `passthrough`"
                ),
            })?;
    }
    if let Some(v) = args.prompt_distillation {
        policy.prompt_distillation = v;
    }
    if let Some(v) = args.tool_virtualization {
        policy.tool_virtualization = v;
    }
    if let Some(v) = args.code_mode {
        policy.code_mode_enhancements = v;
    }
    if let Some(v) = args.strip_billing {
        policy.strip_billing_noise = v;
    }
    if let Some(v) = args.spill {
        policy.spill_truncation = Some(v);
    }

    reg.set_optimization_policy(policy.clone())
        .map_err(|message| CliError::ProviderConfig { message })?;

    let daemon_synced = if let Some(client) = try_live_gateway_client(state_paths).await {
        let payload = json!({
            "prompt_distillation": policy.prompt_distillation,
            "tool_virtualization": policy.tool_virtualization,
            "code_mode_enhancements": policy.code_mode_enhancements,
            "strip_billing_noise": policy.strip_billing_noise,
            "spill_truncation": policy.spill_truncation,
        });
        client.set_optimization_policy(&payload).await.is_ok()
            || client.reload_providers().await.is_ok()
    } else {
        false
    };

    Ok(ProviderOptReport {
        updated: true,
        mode: policy.mode(),
        policy,
        daemon_synced,
    })
}
