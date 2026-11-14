use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use agy_gateway_server::{
    acp_client::{LiveAcpClient, LiveAcpConfig},
    error::AcpError,
    job::install_panic_cleanup_hook,
    platform::{
        canonicalize_clean, prepare_isolated_session_cwd, resolve_agy_binary,
        wait_for_shutdown_signal,
    },
    routes::{AppState, build_router, serve_router},
    session_pool::SessionPool,
    telemetry::TelemetryHub,
    turn_runner::GatewayRuntimeConfig,
};
use agy_gateway_types::{
    config::{
        AcpModeId, DEFAULT_CHANNEL_CAPACITY, DEFAULT_MAX_BODY_BYTES, DEFAULT_MAX_IDLE_SESSIONS,
        DEFAULT_MAX_LINE_BYTES, DEFAULT_MAX_STORED_RESPONSES, DEFAULT_TURN_TIMEOUT_SECS,
        GatewayLimits, ModelId,
    },
    error::ValidationError,
};
use clap::Parser;
use thiserror::Error;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "agy-acp-gateway",
    version,
    about = "Local HTTP API gateway exposing OpenAI `/v1/responses` and Anthropic `/v1/messages` over `agy acp`"
)]
struct CliArgs {
    /// Local socket address to bind the HTTP gateway on.
    #[arg(long, env = "AGY_GATEWAY_LISTEN", default_value = "127.0.0.1:8787")]
    listen: SocketAddr,

    /// Path to `agy` (`agy.exe` on Windows). Auto-detected from `PATH` and standard Windows/macOS/Linux install paths if omitted.
    #[arg(long, env = "AGY_BIN")]
    agy_bin: Option<PathBuf>,

    /// Default ACP model identifier when caller requests an unknown/generic model name.
    #[arg(
        long,
        env = "AGY_DEFAULT_MODEL",
        default_value = "gemini-4-carbon-high"
    )]
    default_model: String,

    /// Default ACP session mode (`auto`, `accept-edits`, `default`, `plan`).
    #[arg(long, env = "AGY_ACP_MODE", default_value = "auto")]
    acp_mode: String,

    /// Working directory assigned to ACP sessions (defaults to an isolated empty directory under system temp).
    #[arg(long, env = "AGY_ACP_CWD")]
    acp_cwd: Option<PathBuf>,

    /// Optional local API key required via `Authorization: Bearer <key>` or `x-api-key: <key>`.
    #[arg(long, env = "AGY_GATEWAY_API_KEY")]
    api_key: Option<String>,

    /// Optional directory containing built companion frontend static assets (defaults to `./web/dist` if present).
    #[arg(long, env = "AGY_UI_DIR")]
    ui_dir: Option<PathBuf>,

    /// Maximum ACP stdout JSON-RPC line size in bytes.
    #[arg(long, default_value_t = DEFAULT_MAX_LINE_BYTES)]
    max_line_bytes: usize,

    /// Maximum HTTP request body size in bytes.
    #[arg(long, default_value_t = DEFAULT_MAX_BODY_BYTES)]
    max_body_bytes: usize,

    /// Internal bounded channel capacity per stream.
    #[arg(long, default_value_t = DEFAULT_CHANNEL_CAPACITY)]
    channel_capacity: usize,

    /// Maximum idle multi-turn ACP sessions retained in LRU cache.
    #[arg(long, default_value_t = DEFAULT_MAX_IDLE_SESSIONS)]
    max_idle_sessions: usize,

    /// Maximum completed OpenAI `ResponseObject` records retained in memory.
    #[arg(long, default_value_t = DEFAULT_MAX_STORED_RESPONSES)]
    max_stored_responses: usize,

    /// Per-turn overall timeout in seconds.
    #[arg(long, default_value_t = DEFAULT_TURN_TIMEOUT_SECS)]
    turn_timeout_secs: u64,
}

#[derive(Debug, Error)]
enum StartupError {
    #[error("invalid gateway configuration")]
    Validation {
        #[from]
        source: ValidationError,
    },

    #[error("could not locate `agy` executable; pass `--agy-bin <path>` or set `AGY_BIN`")]
    AgyBinaryNotFound,

    #[error("failed to prepare isolated ACP session directory `{}`", path.display())]
    PrepareCwd {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to initialize `agy acp` subprocess")]
    AcpInit {
        #[from]
        source: AcpError,
    },

    #[error("failed to bind HTTP listener on `{addr}`")]
    BindListener {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    #[error("HTTP server runtime error")]
    Serve {
        #[source]
        source: std::io::Error,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    install_panic_cleanup_hook();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let args = CliArgs::parse();
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = ?err, "{err}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: CliArgs) -> Result<(), StartupError> {
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

    let binary_path = resolve_agy_binary(args.agy_bin).ok_or(StartupError::AgyBinaryNotFound)?;
    let (_, session_cwd) = prepare_isolated_session_cwd(args.acp_cwd)
        .map_err(|(path, source)| StartupError::PrepareCwd { path, source })?;

    tracing::info!(
        listen = %args.listen,
        agy_bin = %binary_path.display(),
        session_cwd = %session_cwd.display(),
        default_model = %default_model,
        acp_mode = %acp_mode,
        "starting agy-acp-gateway"
    );

    let live_client = Arc::new(
        LiveAcpClient::spawn_and_init(LiveAcpConfig {
            binary_path,
            extra_args: Vec::new(),
            session_cwd,
            limits,
        })
        .await?,
    );

    let pool = Arc::new(SessionPool::new(limits));
    let telemetry = Arc::new(TelemetryHub::new());
    let ui_dist_dir = resolve_ui_dist_dir(args.ui_dir);
    let api_key = args
        .api_key
        .map(|k| k.trim().to_owned())
        .filter(|k| !k.is_empty())
        .map(Arc::<str>::from);

    let app_state = AppState {
        backend: Arc::new(Arc::clone(&live_client)),
        pool,
        telemetry,
        config: GatewayRuntimeConfig {
            default_model,
            acp_mode,
            api_key,
            limits,
        },
        ui_dist_dir: ui_dist_dir.clone(),
    };

    let router = build_router(app_state);
    let listener =
        TcpListener::bind(args.listen)
            .await
            .map_err(|source| StartupError::BindListener {
                addr: args.listen,
                source,
            })?;

    tracing::info!(
        dashboard_url = %format!("http://{}/", args.listen),
        telemetry_url = %format!("http://{}/api/telemetry/snapshot", args.listen),
        openai_responses_url = %format!("http://{}/v1/responses", args.listen),
        anthropic_messages_url = %format!("http://{}/v1/messages", args.listen),
        models_url = %format!("http://{}/v1/models", args.listen),
        ui_static_hosted = ui_dist_dir.is_some(),
        "gateway listening and ready"
    );

    serve_router(listener, router, wait_for_shutdown_signal())
        .await
        .map_err(|source| StartupError::Serve { source })?;

    tracing::info!("shutting down ACP subprocess tree");
    if let Ok(owned) = Arc::try_unwrap(live_client) {
        let _ = owned.close().await;
    }

    Ok(())
}

fn resolve_ui_dist_dir(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(dir) = explicit {
        return dir
            .join("index.html")
            .is_file()
            .then(|| canonicalize_clean(&dir));
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
