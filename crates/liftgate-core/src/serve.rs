use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

use liftgate_server::{
    acp_client::{LiveAcpClient, LiveAcpConfig},
    job::install_panic_cleanup_hook as install_server_panic_hook,
    platform::{canonicalize_clean, prepare_isolated_session_cwd, wait_for_shutdown_signal},
    routes::{AdminShutdownHandle, AppState, EmbeddedUiAsset, build_router, serve_router},
    session_pool::SessionPool,
    telemetry::{CacheTelemetryConfig, TelemetryHub},
    turn_runner::GatewayRuntimeConfig,
};
use liftgate_types::config::{AcpModeId, GatewayLimits, ModelId};
use tokio::net::TcpListener;
use tokio::task::JoinSet;

use crate::cli::{ServeArgs, normalize_optional_api_key};
use crate::error::CliError;
use crate::state::{
    StatePaths, format_connect_url, resolve_effective_agy_auto_update, resolve_effective_agy_bin,
    resolve_effective_agy_proxy, resolve_effective_low_cache_threshold_bps,
    resolve_effective_max_low_cache_records, resolve_effective_vision_offload,
};

pub const ENV_SHUTDOWN_TOKEN: &str = "LIFTGATE_SHUTDOWN_TOKEN";

/// Installs the cross-platform panic hook that tears down supervised `agy acp` child trees.
pub fn install_panic_cleanup_hook() {
    install_server_panic_hook();
}

/// Summary of an initialized foreground gateway server right after its listeners are ready.
#[derive(Clone, Debug)]
pub struct ServeReadyInfo {
    pub listen_url: String,
    pub dashboard_url: String,
    pub default_model: String,
    pub acp_mode: String,
    pub auth_enabled: bool,
    pub full_ui_embedded: bool,
    pub agy_proxy_display: Option<String>,
    pub agy_auto_update: bool,
    pub vision_offload: bool,
}

mod generated {
    include!(concat!(env!("OUT_DIR"), "/embedded_ui.rs"));
}

static FALLBACK_DASHBOARD_HTML: &[u8] = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Liftgate · Dashboard</title>
  <style>
    body { font-family: system-ui, -apple-system, sans-serif; background: #0b0f17; color: #e5e7eb; margin: 0; padding: 2rem; }
    .wrap { max-width: 960px; margin: 0 auto; }
    .card { background: #131a27; border: 1px solid #243044; border-radius: 10px; padding: 1.25rem; margin-bottom: 1rem; }
    h1 { margin: 0 0 0.5rem; font-size: 1.35rem; }
    pre { background: #090d14; padding: 0.85rem; border-radius: 8px; overflow-x: auto; font-size: 0.82rem; line-height: 1.45; }
    .muted { color: #9ca3af; font-size: 0.9rem; }
  </style>
</head>
<body>
  <div class="wrap">
    <div class="card">
      <h1>Liftgate Live Telemetry</h1>
      <div class="muted" id="status">Connecting to /api/telemetry/events ...</div>
    </div>
    <div class="card">
      <pre id="payload">Loading snapshot...</pre>
    </div>
  </div>
  <script>
    const hash = new URLSearchParams(location.hash.slice(1));
    let bootKey = hash.get('api_key') || '';
    let bootBase = (hash.get('base_url') || '').replace(/\/+$/, '');
    if (bootKey || bootBase) {
      try {
        if (bootKey) localStorage.setItem('agy_gateway_api_key', bootKey);
        if (bootBase) localStorage.setItem('agy_gateway_base_url', bootBase);
      } catch {}
      history.replaceState(null, '', location.pathname + location.search);
    }
    let key = bootKey, base = bootBase;
    try { key = key || localStorage.getItem('agy_gateway_api_key') || ''; base = base || localStorage.getItem('agy_gateway_base_url') || ''; } catch {}
    const sseUrl = base + '/api/telemetry/events' + (key ? '?api_key=' + encodeURIComponent(key) : '');
    const es = new EventSource(sseUrl);
    es.addEventListener('snapshot', (e) => {
      document.getElementById('status').textContent = 'Connected · Live SSE';
      document.getElementById('payload').textContent = JSON.stringify(JSON.parse(e.data), null, 2);
    });
    es.onerror = () => {
      document.getElementById('status').textContent = 'Reconnecting (set api_key if --api-key is active)...';
    };
  </script>
</body>
</html>
"#
.as_bytes();

static FALLBACK_UI_ASSETS: &[EmbeddedUiAsset] = &[("index.html", FALLBACK_DASHBOARD_HTML)];

#[must_use]
pub fn active_embedded_ui() -> (&'static [EmbeddedUiAsset], bool) {
    if generated::EMBEDDED_UI_ASSETS.is_empty() {
        (FALLBACK_UI_ASSETS, false)
    } else {
        (generated::EMBEDDED_UI_ASSETS, true)
    }
}

pub async fn run_serve(state_paths: &StatePaths, args: ServeArgs) -> Result<(), CliError> {
    run_serve_with_hook(state_paths, args, None).await
}

pub async fn run_serve_with_hook(
    state_paths: &StatePaths,
    args: ServeArgs,
    on_ready: Option<&(dyn Fn(&ServeReadyInfo) + Send + Sync)>,
) -> Result<(), CliError> {
    let default_model = ModelId::try_new(&args.default_model)?;
    let acp_mode = AcpModeId::try_new(&args.acp_mode)?;
    let limits = GatewayLimits::try_new(
        args.max_line_bytes,
        args.max_body_bytes,
        args.channel_capacity,
        args.max_idle_sessions,
        args.max_stored_responses,
        args.turn_timeout_secs,
    )?;

    let settings = state_paths.read_settings()?;
    let agy_proxy = resolve_effective_agy_proxy(args.agy_proxy.as_deref(), &settings)?;
    let auto_update_enabled =
        resolve_effective_agy_auto_update(args.agy_auto_update, args.no_agy_auto_update, &settings);
    let vision_offload_enabled =
        resolve_effective_vision_offload(args.vision_offload, args.no_vision_offload, &settings);
    let max_low_cache_records =
        resolve_effective_max_low_cache_records(args.max_low_cache_records, &settings)?;
    let low_cache_threshold_bps =
        resolve_effective_low_cache_threshold_bps(args.low_cache_threshold_pct, &settings)?;
    let binary_path =
        resolve_effective_agy_bin(args.agy_bin, &settings).ok_or(CliError::AgyBinaryNotFound)?;
    let (_, session_cwd) = prepare_isolated_session_cwd(args.acp_cwd)?;
    let spill_dir = state_paths.ensure_spill_dir()?;

    // Bind TCP listeners BEFORE spawning `agy acp` so port conflicts fail instantaneously
    // without leaving orphaned initialization work.
    let listener =
        TcpListener::bind(args.listen)
            .await
            .map_err(|source| CliError::BindListener {
                addr: args.listen,
                source,
            })?;
    let ui_listener = if let Some(ui_addr) = args.ui_listen.filter(|&a| a != args.listen) {
        Some(
            TcpListener::bind(ui_addr)
                .await
                .map_err(|source| CliError::BindListener {
                    addr: ui_addr,
                    source,
                })?,
        )
    } else {
        None
    };

    tracing::info!(
        listen = %args.listen,
        ui_listen = ?args.ui_listen,
        agy_bin = %binary_path.display(),
        session_cwd = %session_cwd.display(),
        default_model = %default_model,
        acp_mode = %acp_mode,
        agy_proxy = agy_proxy
            .as_ref()
            .map(liftgate_types::config::ProxyUrl::redacted_display)
            .as_deref()
            .unwrap_or("none"),
        agy_auto_update = auto_update_enabled,
        vision_offload = vision_offload_enabled,
        max_low_cache_records = max_low_cache_records.get(),
        low_cache_threshold_bps = low_cache_threshold_bps.get(),
        "starting liftgate"
    );

    let live_client = Arc::new(
        LiveAcpClient::spawn_and_init(LiveAcpConfig {
            binary_path,
            extra_args: Vec::new(),
            session_cwd,
            spill_dir: Some(spill_dir.clone()),
            limits,
            agy_proxy: agy_proxy.clone(),
            auto_update_enabled,
        })
        .await?,
    );

    let pool = Arc::new(SessionPool::new(limits));
    let telemetry = Arc::new(TelemetryHub::with_config(CacheTelemetryConfig {
        persist_path: Some(state_paths.cache_telemetry_file()),
        settings_path: Some(state_paths.settings_file()),
        max_low_cache_records,
        low_cache_threshold_bps,
    }));
    let ui_dist_dir = resolve_ui_dist_dir(args.ui_dir);
    let (embedded_ui, full_bundle_embedded) = active_embedded_ui();
    let api_key = normalize_optional_api_key(args.api_key.as_deref()).map(Arc::<str>::from);
    let auth_enabled = api_key.is_some();
    let default_model_str = default_model.as_str().to_owned();
    let acp_mode_str = acp_mode.as_str().to_owned();

    let shutdown_token = std::env::var(ENV_SHUTDOWN_TOKEN)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let (admin_shutdown, mut admin_rx) = match shutdown_token {
        Some(tok) => {
            let (handle, rx) = AdminShutdownHandle::new(Arc::from(tok));
            (Some(handle), Some(rx))
        }
        None => (None, None),
    };

    let app_state = AppState {
        backend: Arc::new(Arc::clone(&live_client)),
        pool: Arc::clone(&pool),
        telemetry: Arc::clone(&telemetry),
        config: GatewayRuntimeConfig {
            title_model_file: Some(state_paths.title_model_file()),
            spill_dir: Some(spill_dir),
            default_model,
            acp_mode,
            api_key,
            limits,
            vision_offload: Arc::new(AtomicBool::new(vision_offload_enabled)),
        },
        ui_dist_dir: ui_dist_dir.clone(),
        embedded_ui,
        admin_shutdown,
    };

    let router = build_router(app_state.clone());
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut bg_tasks = JoinSet::new();

    let shutdown_telemetry = Arc::clone(&telemetry);
    bg_tasks.spawn(async move {
        tokio::select! {
            biased;
            () = wait_admin_shutdown(&mut admin_rx), if admin_rx.is_some() => {
                tracing::info!("received authenticated admin shutdown request");
            }
            () = wait_for_shutdown_signal() => {}
        }
        shutdown_telemetry.signal_shutdown();
        let _ = stop_tx.send_replace(true);
    });

    {
        let reaper_pool = Arc::clone(&pool);
        let reaper_client = Arc::clone(&live_client);
        let reaper_telemetry = Arc::clone(&telemetry);
        let mut reaper_stop_rx = stop_rx.clone();
        bg_tasks.spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let _ = ticker.tick().await;
            loop {
                tokio::select! {
                    biased;
                    _ = reaper_stop_rx.wait_for(|&stop| stop) => break,
                    _ = ticker.tick() => {
                        let reaped = reaper_pool.reap_expired_idle_sessions(
                            liftgate_server::telemetry::unix_timestamp_ms(),
                            liftgate_server::session_pool::DEFAULT_MAX_IDLE_SESSION_AGE_MS,
                            &reaper_client,
                        );
                        if reaped > 0 {
                            tracing::info!(
                                reaped_sessions = reaped,
                                "reaped expired idle `agy acp` session worker(s)"
                            );
                            reaper_telemetry.notify_change();
                        }
                    }
                }
            }
        });
    }

    if auto_update_enabled {
        let updater_paths = state_paths.clone();
        let updater_client = Arc::clone(&live_client);
        let updater_pool = Arc::clone(&pool);
        let updater_telemetry = Arc::clone(&telemetry);
        let updater_stop_rx = stop_rx.clone();
        bg_tasks.spawn(async move {
            crate::agy_manager::run_idle_agy_auto_updater(
                updater_paths,
                updater_client,
                updater_pool,
                updater_telemetry,
                updater_stop_rx,
            )
            .await;
        });
    }

    tracing::info!(
        dashboard_url = %format!("http://{}/", args.listen),
        telemetry_url = %format!("http://{}/api/telemetry/snapshot", args.listen),
        openai_responses_url = %format!("http://{}/v1/responses", args.listen),
        anthropic_messages_url = %format!("http://{}/v1/messages", args.listen),
        models_url = %format!("http://{}/v1/models", args.listen),
        ui_disk_override = ui_dist_dir.is_some(),
        ui_embedded_bundle = full_bundle_embedded,
        "gateway listening and ready"
    );

    let listen_url = format_connect_url(args.listen);
    let dash_url = args
        .ui_listen
        .map_or_else(|| listen_url.clone(), format_connect_url);
    let proxy_banner = agy_proxy
        .as_ref()
        .map(|p| p.redacted_display().into_owned());
    let ready_info = ServeReadyInfo {
        listen_url,
        dashboard_url: dash_url,
        default_model: default_model_str,
        acp_mode: acp_mode_str,
        auth_enabled,
        full_ui_embedded: full_bundle_embedded,
        agy_proxy_display: proxy_banner,
        agy_auto_update: auto_update_enabled,
        vision_offload: vision_offload_enabled,
    };
    if let Some(cb) = on_ready {
        cb(&ready_info);
    }

    if let Some(extra_listener) = ui_listener {
        let ui_router = build_router(app_state);
        let mut ui_stop_rx = stop_rx.clone();
        bg_tasks.spawn(async move {
            let _ = serve_router(extra_listener, ui_router, async move {
                let _ = ui_stop_rx.wait_for(|&stop| stop).await;
            })
            .await;
        });
    }

    let mut main_stop_rx = stop_rx.clone();
    let serve_fut = serve_router(listener, router, async move {
        let _ = main_stop_rx.wait_for(|&stop| stop).await;
    });
    tokio::pin!(serve_fut);

    let mut drain_stop_rx = stop_rx;
    tokio::select! {
        biased;
        res = &mut serve_fut => {
            res.map_err(|source| CliError::Serve { source })?;
        }
        () = async {
            let _ = drain_stop_rx.wait_for(|&stop| stop).await;
            tokio::time::sleep(Duration::from_secs(3)).await;
        } => {
            tracing::info!("graceful HTTP drain budget elapsed; closing remaining connections");
        }
    }

    bg_tasks.abort_all();
    while bg_tasks.join_next().await.is_some() {}

    tracing::info!("shutting down ACP subprocess tree");
    if let Ok(owned) = Arc::try_unwrap(live_client) {
        let _ = owned.close().await;
    }

    Ok(())
}

async fn wait_admin_shutdown(rx: &mut Option<tokio::sync::watch::Receiver<bool>>) {
    if let Some(r) = rx {
        let _ = r.wait_for(|&stop| stop).await;
    }
}

fn resolve_ui_dist_dir(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(dir) = explicit {
        return dir
            .join("index.html")
            .is_file()
            .then(|| canonicalize_clean(&dir));
    }

    if !generated::EMBEDDED_UI_ASSETS.is_empty() {
        return None;
    }

    let cwd_candidate = PathBuf::from("web").join("dist");
    if cwd_candidate.join("index.html").is_file() {
        return Some(canonicalize_clean(&cwd_candidate));
    }

    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        for ancestor in [exe_dir.to_path_buf(), exe_dir.join("..").join("..")] {
            let candidate = ancestor.join("web").join("dist");
            if candidate.join("index.html").is_file() {
                return Some(canonicalize_clean(&candidate));
            }
        }
    }

    None
}
