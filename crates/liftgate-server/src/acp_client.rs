use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use bytes::Bytes;
use liftgate_types::{
    acp::{
        ACP_PROTOCOL_VERSION, AcpPromptBlock, AntigravityNewMeta, AvailableModel,
        ClientCapabilities, ClientInfo, FsCapabilities, IncomingRpcFrame, InitializeParams,
        InitializeResult, JsonRpcErrorPayload, JsonRpcNotification, JsonRpcRequest,
        JsonRpcResponseOut, McpServerConfig, PermissionOutcome, PromptTurnResult,
        RequestPermissionParams, RequestPermissionResult, SessionCancelParams, SessionCloseParams,
        SessionNewMeta, SessionNewParams, SessionNewResult, SessionPromptParams,
        SessionSetModeParams, SessionSetModelParams, SessionUpdateNotification,
        SessionUpdatePayload, SystemPromptAppendMeta,
    },
    config::{AcpModeId, GatewayLimits, ModelId, ProxyUrl, SessionId},
    telemetry::{AcpProcessTelemetry, StderrLogEntry},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::value::RawValue;
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
};
use tokio_stream::StreamExt;
use tokio_util::{
    codec::{FramedRead, LinesCodec},
    sync::{CancellationToken, DropGuard},
};

use crate::{
    error::AcpError,
    job::{ProcessGroupGuard, configure_command_group},
    platform::configure_acp_child_env,
    session_pool::SessionPool,
    telemetry::unix_timestamp_ms,
};

const MAX_STDERR_LINES: usize = 100;

/// Static-dispatch trait abstracting ACP agent operations so HTTP handlers & session pooling
/// run with zero `dyn` overhead and deterministic mockability in tests.
pub trait AcpBackend: Send + Sync + 'static {
    fn available_models(&self) -> Vec<AvailableModel>;

    /// Returns cached models or triggers a one-shot warm-up probe when empty (e.g. right after sign-in).
    fn refresh_models_if_empty(&self) -> impl Future<Output = Vec<AvailableModel>> + Send {
        let current = self.available_models();
        async move { current }
    }

    fn create_session(
        &self,
        system_append: &str,
    ) -> impl Future<Output = Result<SessionNewResult, AcpError>> + Send;

    /// Creates a new isolated ACP session wired to optional loopback MCP servers (`mcpServers`).
    fn create_session_with_mcp(
        &self,
        system_append: &str,
        mcp_servers: &[McpServerConfig],
    ) -> impl Future<Output = Result<SessionNewResult, AcpError>> + Send {
        let _ = mcp_servers;
        self.create_session(system_append)
    }

    fn set_session_model(
        &self,
        session_id: &SessionId,
        model_id: &ModelId,
    ) -> impl Future<Output = Result<(), AcpError>> + Send;

    fn set_session_mode(
        &self,
        session_id: &SessionId,
        mode_id: &AcpModeId,
    ) -> impl Future<Output = Result<(), AcpError>> + Send;

    fn start_prompt(
        &self,
        session_id: &SessionId,
        prompt: &[AcpPromptBlock],
        updates_tx: mpsc::Sender<SessionUpdatePayload>,
    ) -> impl Future<Output = Result<PromptTurnResult, AcpError>> + Send;

    /// Cancels only the currently running `session/prompt` turn on `session_id` without closing
    /// the session, used to immediately halt runaway post-tool hallucinations.
    fn cancel_prompt_nonblocking(&self, _session_id: &SessionId) {}

    /// Sends a non-destructive `session/cancel` (`CancelOnly`, without `session/close`) and
    /// returns `true` when the backend supports draining the cancelled `session/prompt` turn
    /// and returning the live warm session back to `SessionPool`.
    fn cancel_prompt_for_reuse(&self, _session_id: &SessionId) -> bool {
        false
    }

    /// Non-blocking synchronous cancellation & session teardown safe to call from `Drop`.
    fn abort_session_nonblocking(&self, session_id: &SessionId);

    fn close_session(
        &self,
        session_id: &SessionId,
    ) -> impl Future<Output = Result<(), AcpError>> + Send;

    /// Returns recent `agy acp` stderr lines emitted at or after `since_unix_ms`.
    fn recent_stderr_since(&self, _since_unix_ms: u64) -> Vec<String> {
        Vec::new()
    }

    /// Performs a server-managed URL fetch for `web_fetch_20250910` / `web_fetch` tool turns.
    fn fetch_web_url(
        &self,
        url: &str,
        allowed_domains: &[String],
        blocked_domains: &[String],
    ) -> impl Future<Output = crate::web_fetch::WebFetchOutcome> + Send {
        let url_owned = url.to_owned();
        let allowed = allowed_domains.to_vec();
        let blocked = blocked_domains.to_vec();
        async move { crate::web_fetch::fetch_url_sanitized(&url_owned, &allowed, &blocked).await }
    }

    /// Performs a server-managed URL fetch with optional full-page `pattern` filtering (`find_in_page`).
    fn fetch_web_url_with_pattern(
        &self,
        url: &str,
        pattern: Option<&str>,
        allowed_domains: &[String],
        blocked_domains: &[String],
    ) -> impl Future<Output = crate::web_fetch::WebFetchOutcome> + Send {
        let pat_owned = pattern.map(str::to_owned);
        let base_fut = self.fetch_web_url(url, allowed_domains, blocked_domains);
        async move {
            let out = base_fut.await;
            crate::web_fetch::apply_pattern_to_outcome(out, pat_owned.as_deref())
        }
    }

    /// Snapshot of ACP master process health and JSON-RPC statistics for the companion UI.
    fn process_telemetry(&self, default_model: &str, acp_mode: &str) -> AcpProcessTelemetry {
        AcpProcessTelemetry {
            connected: true,
            pid: None,
            binary_path: "mock-acp".to_owned(),
            session_cwd: String::new(),
            os: Cow::Borrowed(std::env::consts::OS),
            arch: Cow::Borrowed(std::env::consts::ARCH),
            supervision_strategy: Cow::Borrowed(supervision_strategy_name()),
            protocol_version: ACP_PROTOCOL_VERSION,
            agent_name: Some("Mock ACP Agent".to_owned()),
            agent_version: Some("0.1.0".to_owned()),
            started_at_unix_ms: 0,
            uptime_secs: 0,
            default_model: default_model.to_owned(),
            acp_mode: acp_mode.to_owned(),
            rpc_requests_sent: 0,
            rpc_responses_received: 0,
            rpc_notifications_received: 0,
            rpc_pending_in_flight: 0,
            active_prompt_streams: 0,
            recent_stderr: Vec::new(),
            agy_proxy: None,
            auto_update_enabled: false,
            latest_known_version: None,
            last_update_check_unix_ms: None,
            last_auto_update_unix_ms: None,
            last_auto_updated_from: None,
            auto_update_note: None,
        }
    }
}

#[derive(Default)]
struct RpcCounters {
    requests_sent: AtomicU64,
    responses_received: AtomicU64,
    notifications_received: AtomicU64,
}

type PendingResponseMap =
    Mutex<HashMap<u64, oneshot::Sender<Result<Box<RawValue>, JsonRpcErrorPayload>>>>;
type SessionStreamMap = Mutex<HashMap<SessionId, mpsc::Sender<SessionUpdatePayload>>>;

/// Configuration for launching the `agy acp` subprocess.
#[derive(Clone, Debug)]
pub struct LiveAcpConfig {
    pub binary_path: PathBuf,
    pub extra_args: Vec<String>,
    pub session_cwd: PathBuf,
    pub spill_dir: Option<PathBuf>,
    pub limits: GatewayLimits,
    pub agy_proxy: Option<ProxyUrl>,
    pub auto_update_enabled: bool,
}

#[derive(Clone, Debug, Default)]
struct AutoUpdateTelemetryState {
    latest_known_version: Option<String>,
    last_update_check_unix_ms: Option<u64>,
    last_auto_update_unix_ms: Option<u64>,
    last_auto_updated_from: Option<String>,
    auto_update_note: Option<String>,
}

/// Synchronous rollback guard for an in-place `agy` executable swap.
///
/// If dropped before [`Self::commit`], synchronously restores `backup_path` back to `target_path`.
pub struct BinarySwapGuard {
    target_path: PathBuf,
    backup_path: Option<PathBuf>,
    committed: bool,
}

impl BinarySwapGuard {
    #[must_use]
    pub fn new(target_path: PathBuf, backup_path: Option<PathBuf>) -> Self {
        Self {
            target_path,
            backup_path,
            committed: false,
        }
    }

    /// Disarms rollback and cleans up the temporary backup file on a best-effort basis.
    pub fn commit(mut self) {
        self.committed = true;
        if let Some(bak) = &self.backup_path {
            let _ = std::fs::remove_file(bak);
        }
    }
}

impl Drop for BinarySwapGuard {
    fn drop(&mut self) {
        if !self.committed
            && let Some(bak) = &self.backup_path
            && bak.exists()
        {
            for attempt in 0..15u8 {
                if self.target_path.exists() {
                    let _ = std::fs::remove_file(&self.target_path);
                }
                if std::fs::rename(bak, &self.target_path).is_ok() || !bak.exists() {
                    break;
                }
                if attempt < 14 {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
    }
}

/// Self-healing supervised `agy acp --process-per-session --dangerously-skip-permissions` client over stdio.
pub struct LiveAcpClient {
    cfg: LiveAcpConfig,
    auto_update_enabled: std::sync::atomic::AtomicBool,
    next_gen_id: AtomicU64,
    generation: std::sync::RwLock<Arc<LiveAcpGeneration>>,
    respawn_lock: tokio::sync::Mutex<()>,
    models_tx: watch::Sender<Arc<[AvailableModel]>>,
    models_rx: watch::Receiver<Arc<[AvailableModel]>>,
    auto_update_state: Mutex<AutoUpdateTelemetryState>,
}

struct LiveAcpGeneration {
    id: u64,
    inner: Arc<LiveAcpInner>,
    tasks: Mutex<JoinSet<()>>,
    cancel: CancellationToken,
    _cancel_guard: DropGuard,
}

struct LiveAcpInner {
    next_id: AtomicU64,
    stdin_tx: mpsc::Sender<Bytes>,
    abort_tx: mpsc::Sender<AbortCommand>,
    pending: Arc<PendingResponseMap>,
    session_streams: Arc<SessionStreamMap>,
    child_pid: Option<u32>,
    binary_path: Arc<str>,
    session_cwd: Arc<str>,
    started_at_unix_ms: u64,
    agent_meta: Mutex<(u32, Option<String>, Option<String>)>,
    rpc_stats: Arc<RpcCounters>,
    stderr_log: Arc<Mutex<VecDeque<StderrLogEntry>>>,
    _job_guard: ProcessGroupGuard,
}

enum AbortCommand {
    CancelOnly(SessionId),
    CancelAndClose(SessionId),
}

struct StreamSlotGuard {
    map: Arc<SessionStreamMap>,
    session_id: SessionId,
}

impl Drop for StreamSlotGuard {
    fn drop(&mut self) {
        match self.map.lock() {
            Ok(mut guard) => {
                let _ = guard.remove(&self.session_id);
            }
            Err(poisoned) => {
                let _ = poisoned.into_inner().remove(&self.session_id);
            }
        }
    }
}

struct PendingSlotGuard {
    map: Arc<PendingResponseMap>,
    id: u64,
}

impl Drop for PendingSlotGuard {
    fn drop(&mut self) {
        match self.map.lock() {
            Ok(mut guard) => {
                let _ = guard.remove(&self.id);
            }
            Err(poisoned) => {
                let _ = poisoned.into_inner().remove(&self.id);
            }
        }
    }
}

impl LiveAcpGeneration {
    async fn spawn_and_handshake(id: u64, cfg: &LiveAcpConfig) -> Result<Self, AcpError> {
        let canonical_cwd = canonicalize_or_keep(&cfg.session_cwd);
        let cwd_str: Arc<str> = Arc::from(canonical_cwd.to_string_lossy().as_ref());

        let mut cmd = Command::new(&cfg.binary_path);
        cmd.arg("acp")
            .arg("--process-per-session")
            .arg("--dangerously-skip-permissions");
        let overrides_customizations = cfg
            .extra_args
            .iter()
            .any(|arg| arg.starts_with("--include-default-customizations"));
        if !overrides_customizations {
            // Strip Antigravity IDE default skills/subagents/slash-commands (~4.5k tokens per
            // session) while keeping built-in `search_web` active.
            cmd.arg("--include-default-customizations=false");
        }
        cmd.args(&cfg.extra_args)
            .current_dir(&canonical_cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_acp_child_env(&mut cmd, cfg.agy_proxy.as_ref());
        configure_command_group(&mut cmd);

        let mut child = cmd.spawn().map_err(|source| AcpError::SpawnProcess {
            path: cfg.binary_path.clone(),
            source,
        })?;
        let child_pid = child.id();
        let binary_str: Arc<str> = Arc::from(cfg.binary_path.to_string_lossy().as_ref());
        let started_at_unix_ms = unix_timestamp_ms();

        let job_guard = ProcessGroupGuard::attach(&child)?;

        let stdin = child
            .stdin
            .take()
            .ok_or(AcpError::MissingStdioPipe { stream: "stdin" })?;
        let stdout = child
            .stdout
            .take()
            .ok_or(AcpError::MissingStdioPipe { stream: "stdout" })?;
        let stderr = child
            .stderr
            .take()
            .ok_or(AcpError::MissingStdioPipe { stream: "stderr" })?;

        let cancel = CancellationToken::new();
        let cancel_guard = cancel.clone().drop_guard();

        let cap = cfg.limits.channel_capacity().get();
        let (stdin_tx, stdin_rx) = mpsc::channel::<Bytes>(cap);
        let (abort_tx, abort_rx) = mpsc::channel::<AbortCommand>(cap);
        let pending: Arc<PendingResponseMap> = Arc::new(Mutex::new(HashMap::new()));
        let session_streams: Arc<SessionStreamMap> = Arc::new(Mutex::new(HashMap::new()));
        let rpc_stats = Arc::new(RpcCounters::default());
        let stderr_log: Arc<Mutex<VecDeque<StderrLogEntry>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(MAX_STDERR_LINES)));

        let mut tasks = JoinSet::new();

        // 1. Dedicated cancel-safe stdin writer task
        {
            let writer_cancel = cancel.clone();
            tasks.spawn(run_stdin_writer(stdin, stdin_rx, writer_cancel));
        }

        // 2. Cancel-safe bounded stdout JSON-RPC reader task
        {
            let reader_cancel = cancel.clone();
            let reader_pending = Arc::clone(&pending);
            let reader_streams = Arc::clone(&session_streams);
            let reader_stats = Arc::clone(&rpc_stats);
            let reply_stdin_tx = stdin_tx.clone();
            let max_line = cfg.limits.max_line_bytes().get();
            tasks.spawn(run_stdout_reader(
                stdout,
                max_line,
                reader_pending,
                reader_streams,
                reader_stats,
                reply_stdin_tx,
                reader_cancel,
            ));
        }

        // 3. Cancel-safe bounded stderr drain task + child reaper
        {
            let stderr_cancel = cancel.clone();
            let stderr_ring = Arc::clone(&stderr_log);
            tasks.spawn(async move {
                let mut reader = FramedRead::new(stderr, LinesCodec::new_with_max_length(64 * 1024));
                loop {
                    tokio::select! {
                        biased;
                        () = stderr_cancel.cancelled() => break,
                        maybe_line = reader.next() => {
                            match maybe_line {
                                Some(Ok(line)) => {
                                    tracing::debug!(target: "agy_acp_stderr", "{line}");
                                    let trimmed = line.trim();
                                    if !trimmed.is_empty() {
                                        let entry = StderrLogEntry {
                                            timestamp_unix_ms: unix_timestamp_ms(),
                                            line: trimmed.to_owned(),
                                        };
                                        let mut guard = match stderr_ring.lock() {
                                            Ok(g) => g,
                                            Err(p) => p.into_inner(),
                                        };
                                        if guard.len() >= MAX_STDERR_LINES {
                                            let _ = guard.pop_front();
                                        }
                                        guard.push_back(entry);
                                    }
                                }
                                Some(Err(err)) => {
                                    tracing::warn!(error = ?err, "agy acp stderr line exceeded bound");
                                }
                                None => break,
                            }
                        }
                    }
                }
                let _ = child.wait().await;
            });
        }

        let inner = Arc::new(LiveAcpInner {
            next_id: AtomicU64::new(1),
            stdin_tx,
            abort_tx,
            pending,
            session_streams,
            child_pid,
            binary_path: binary_str,
            session_cwd: cwd_str,
            started_at_unix_ms,
            agent_meta: Mutex::new((ACP_PROTOCOL_VERSION, None, None)),
            rpc_stats,
            stderr_log,
            _job_guard: job_guard,
        });

        // 4. Non-blocking abort processor (handles synchronous `Drop` cancellations)
        {
            let abort_inner = Arc::clone(&inner);
            let abort_cancel = cancel.clone();
            tasks.spawn(run_abort_worker(abort_inner, abort_rx, abort_cancel));
        }

        let generation = Self {
            id,
            inner,
            tasks: Mutex::new(tasks),
            cancel,
            _cancel_guard: cancel_guard,
        };

        // Perform ACP `initialize` handshake; on failure, explicitly await `shutdown()` so worker
        // tasks and the OS Job Object release executable image locks before returning (critical
        // for synchronous `BinarySwapGuard` rollback on Windows).
        let init_res: InitializeResult = match generation
            .inner
            .call_rpc(
                "initialize",
                InitializeParams {
                    protocol_version: ACP_PROTOCOL_VERSION,
                    client_capabilities: ClientCapabilities {
                        fs: FsCapabilities {
                            read_text_file: false,
                            write_text_file: false,
                        },
                        terminal: false,
                    },
                    client_info: ClientInfo {
                        name: "liftgate",
                        version: env!("CARGO_PKG_VERSION"),
                    },
                },
            )
            .await
        {
            Ok(res) => res,
            Err(err) => {
                generation.shutdown().await;
                return Err(err);
            }
        };

        tracing::info!(
            generation_id = id,
            pid = ?child_pid,
            protocol_version = init_res.protocol_version,
            agent = ?init_res.agent_info,
            "connected to Antigravity ACP server"
        );
        {
            let (agent_name, agent_version) = match &init_res.agent_info {
                Some(info) => (Some(info.name.clone()), Some(info.version.clone())),
                None => (None, None),
            };
            let mut guard = match generation.inner.agent_meta.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            *guard = (init_res.protocol_version, agent_name, agent_version);
        }

        Ok(generation)
    }

    async fn create_session_once(
        &self,
        system_append: &str,
        mcp_servers: &[McpServerConfig],
        models_tx: &watch::Sender<Arc<[AvailableModel]>>,
    ) -> Result<SessionNewResult, AcpError> {
        let meta = if system_append.trim().is_empty() {
            None
        } else {
            Some(SessionNewMeta {
                antigravity: AntigravityNewMeta {
                    system_prompt: SystemPromptAppendMeta {
                        append: system_append,
                    },
                },
            })
        };
        let res: SessionNewResult = self
            .inner
            .call_rpc(
                "session/new",
                SessionNewParams {
                    cwd: &self.inner.session_cwd,
                    mcp_servers,
                    meta,
                },
            )
            .await?;

        if !res.models.available_models.is_empty() {
            let new_slice: Arc<[AvailableModel]> =
                Arc::from(res.models.available_models.clone().into_boxed_slice());
            let _ = models_tx.send(new_slice);
        }

        Ok(res)
    }

    async fn shutdown(&self) {
        self.cancel.cancel();
        let mut join_set = match self.tasks.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        while join_set.join_next().await.is_some() {}
        self.inner._job_guard.terminate_now();
    }
}

impl LiveAcpClient {
    /// Spawns `agy acp`, attaches a Job Object / process group, completes `initialize`,
    /// and probes the initial `availableModels` catalog.
    pub async fn spawn_and_init(cfg: LiveAcpConfig) -> Result<Self, AcpError> {
        let initial_gen = Arc::new(LiveAcpGeneration::spawn_and_handshake(1, &cfg).await?);
        let (models_tx, models_rx) = watch::channel::<Arc<[AvailableModel]>>(Arc::from([]));
        let initial_auto_update = cfg.auto_update_enabled;

        let client = Self {
            cfg,
            auto_update_enabled: std::sync::atomic::AtomicBool::new(initial_auto_update),
            next_gen_id: AtomicU64::new(2),
            generation: std::sync::RwLock::new(initial_gen),
            respawn_lock: tokio::sync::Mutex::new(()),
            models_tx,
            models_rx,
            auto_update_state: Mutex::new(AutoUpdateTelemetryState::default()),
        };

        // Warm up model catalog so `GET /v1/models` immediately sees all account models; if `agy`
        // requires interactive login first, log a clear hint instead of failing gateway startup.
        match client
            .active_gen()
            .create_session_once("", &[], &client.models_tx)
            .await
        {
            Ok(probe_session) => {
                let _ = client.close_session_inner(&probe_session.session_id).await;
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "initial model catalog probe deferred (run `lg agy login` if sign-in is required); gateway will auto-reconnect on next request"
                );
            }
        }

        Ok(client)
    }

    #[must_use]
    pub fn binary_path(&self) -> &Path {
        &self.cfg.binary_path
    }

    #[must_use]
    pub fn agy_proxy(&self) -> Option<&ProxyUrl> {
        self.cfg.agy_proxy.as_ref()
    }

    /// Dynamically updates the advertised `agy` idle auto-update toggle without restarting `agy acp`.
    pub fn set_auto_update_enabled(&self, enabled: bool) {
        self.auto_update_enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    #[must_use]
    pub fn is_auto_update_enabled(&self) -> bool {
        self.auto_update_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Returns the `agentInfo.version` reported by the active `agy acp` handshake, if available.
    #[must_use]
    pub fn current_agent_version(&self) -> Option<String> {
        let active = self.active_gen();
        let guard = match active.inner.agent_meta.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.2.clone()
    }

    /// Returns `(rpc_pending_in_flight, active_prompt_streams)` on the active ACP generation.
    #[must_use]
    pub fn active_activity_counts(&self) -> (usize, usize) {
        let active = self.active_gen();
        let rpc_pending = match active.inner.pending.lock() {
            Ok(g) => g.len(),
            Err(p) => p.into_inner().len(),
        };
        let streams = match active.inner.session_streams.lock() {
            Ok(g) => g.len(),
            Err(p) => p.into_inner().len(),
        };
        (rpc_pending, streams)
    }

    pub fn record_auto_update_check(&self, latest_version: Option<String>, note: Option<String>) {
        let mut guard = match self.auto_update_state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.last_update_check_unix_ms = Some(unix_timestamp_ms());
        if latest_version.is_some() {
            guard.latest_known_version = latest_version;
        }
        guard.auto_update_note = note;
    }

    pub fn record_auto_update_applied(
        &self,
        previous_version: String,
        new_version: String,
        note: Option<String>,
    ) {
        let now = unix_timestamp_ms();
        let mut guard = match self.auto_update_state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.last_update_check_unix_ms = Some(now);
        guard.last_auto_update_unix_ms = Some(now);
        guard.last_auto_updated_from = Some(previous_version);
        guard.latest_known_version = Some(new_version);
        guard.auto_update_note = note;
    }

    /// Safely shuts down the active `agy acp` process tree while the gateway is completely idle,
    /// runs `swap_binary` to replace the executable on disk (releasing Windows `.exe` locks),
    /// and immediately spawns & handshakes a fresh `agy acp` generation with automatic rollback.
    pub async fn hot_swap_binary_when_idle<CheckIdle, SwapFn>(
        &self,
        pool: &SessionPool,
        auto_update_record: Option<(String, String, Option<String>)>,
        is_still_idle: CheckIdle,
        swap_binary: SwapFn,
    ) -> Result<Option<String>, AcpError>
    where
        CheckIdle: Fn() -> bool,
        SwapFn: FnOnce(&Path) -> std::io::Result<BinarySwapGuard>,
    {
        let _guard = self.respawn_lock.lock().await;
        if !is_still_idle()
            || self.active_activity_counts() != (0, 0)
            || pool.idle_and_active_counts().0 > 0
            || pool.suspended_count() > 0
        {
            return Ok(None);
        }

        let idle_entries = pool.take_all_idle_sessions();
        for entry in &idle_entries {
            let _ = self.close_session_inner(&entry.session_id).await;
        }

        if !is_still_idle()
            || self.active_activity_counts() != (0, 0)
            || pool.idle_and_active_counts().0 > 0
            || pool.suspended_count() > 0
        {
            return Ok(None);
        }

        let previous = self.active_gen();
        previous.shutdown().await;

        let swap_guard = match swap_binary(&self.cfg.binary_path) {
            Ok(g) => g,
            Err(source) => {
                let recovery_id = self.next_gen_id.fetch_add(1, Ordering::Relaxed);
                if let Ok(recovered) =
                    LiveAcpGeneration::spawn_and_handshake(recovery_id, &self.cfg).await
                {
                    let mut write_guard = match self.generation.write() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    *write_guard = Arc::new(recovered);
                }
                return Err(AcpError::SpawnProcess {
                    path: self.cfg.binary_path.clone(),
                    source,
                });
            }
        };

        let new_id = self.next_gen_id.fetch_add(1, Ordering::Relaxed);
        match LiveAcpGeneration::spawn_and_handshake(new_id, &self.cfg).await {
            Ok(fresh_raw) => {
                swap_guard.commit();
                let fresh = Arc::new(fresh_raw);
                if let Ok(probe) = fresh.create_session_once("", &[], &self.models_tx).await {
                    let _: Result<serde::de::IgnoredAny, AcpError> = fresh
                        .inner
                        .call_rpc(
                            "session/close",
                            SessionCloseParams {
                                session_id: probe.session_id.as_str(),
                            },
                        )
                        .await;
                }
                if let Some((previous_version, new_version, note)) = auto_update_record {
                    self.record_auto_update_applied(previous_version, new_version, note);
                }
                {
                    let mut write_guard = match self.generation.write() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    *write_guard = Arc::clone(&fresh);
                }
                Ok(Some(
                    self.current_agent_version()
                        .unwrap_or_else(|| "updated".to_owned()),
                ))
            }
            Err(spawn_err) => {
                drop(swap_guard);
                let recovery_id = self.next_gen_id.fetch_add(1, Ordering::Relaxed);
                if let Ok(recovered) =
                    LiveAcpGeneration::spawn_and_handshake(recovery_id, &self.cfg).await
                {
                    let mut write_guard = match self.generation.write() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    *write_guard = Arc::new(recovered);
                }
                Err(spawn_err)
            }
        }
    }

    fn active_gen(&self) -> Arc<LiveAcpGeneration> {
        match self.generation.read() {
            Ok(g) => Arc::clone(&*g),
            Err(poisoned) => Arc::clone(&*poisoned.into_inner()),
        }
    }

    async fn respawn_if_still_on(
        &self,
        failed_gen_id: u64,
    ) -> Result<Arc<LiveAcpGeneration>, AcpError> {
        let _guard = self.respawn_lock.lock().await;
        let current = self.active_gen();
        if current.id != failed_gen_id {
            return Ok(current);
        }

        let new_id = self.next_gen_id.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            previous_generation = failed_gen_id,
            new_generation = new_id,
            "respawning `agy acp` subprocess to recover transport/authentication state"
        );
        let fresh = Arc::new(LiveAcpGeneration::spawn_and_handshake(new_id, &self.cfg).await?);
        let old = {
            let mut write_guard = match self.generation.write() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            std::mem::replace(&mut *write_guard, Arc::clone(&fresh))
        };
        old.shutdown().await;
        Ok(fresh)
    }

    /// Gracefully shuts down background tasks and terminates the active ACP process group.
    pub async fn shutdown(&self) {
        let active = self.active_gen();
        active.shutdown().await;
    }

    /// Gracefully shuts down background tasks and terminates the ACP process group.
    pub async fn close(self) -> Result<(), AcpError> {
        self.shutdown().await;
        Ok(())
    }

    async fn create_session_resilient(
        &self,
        system_append: &str,
        mcp_servers: &[McpServerConfig],
    ) -> Result<SessionNewResult, AcpError> {
        let current = self.active_gen();
        match current
            .create_session_once(system_append, mcp_servers, &self.models_tx)
            .await
        {
            Ok(res) => Ok(res),
            Err(err) if should_respawn_on_session_new_error(&err) => {
                let fresh = self.respawn_if_still_on(current.id).await?;
                fresh
                    .create_session_once(system_append, mcp_servers, &self.models_tx)
                    .await
            }
            Err(err) => Err(err),
        }
    }

    async fn close_session_inner(&self, session_id: &SessionId) -> Result<(), AcpError> {
        let active = self.active_gen();
        let _: serde::de::IgnoredAny = active
            .inner
            .call_rpc(
                "session/close",
                SessionCloseParams {
                    session_id: session_id.as_str(),
                },
            )
            .await?;
        Ok(())
    }
}

const fn should_respawn_on_session_new_error(err: &AcpError) -> bool {
    matches!(
        err,
        AcpError::TransportClosed | AcpError::Rpc { code: -32000, .. }
    )
}

impl AcpBackend for Arc<LiveAcpClient> {
    fn available_models(&self) -> Vec<AvailableModel> {
        self.models_rx.borrow().to_vec()
    }

    async fn refresh_models_if_empty(&self) -> Vec<AvailableModel> {
        let existing = self.available_models();
        if !existing.is_empty() {
            return existing;
        }
        if let Ok(probe) = self.create_session_resilient("", &[]).await {
            let _ = self.close_session_inner(&probe.session_id).await;
        }
        self.available_models()
    }

    async fn create_session(&self, system_append: &str) -> Result<SessionNewResult, AcpError> {
        self.create_session_resilient(system_append, &[]).await
    }

    async fn create_session_with_mcp(
        &self,
        system_append: &str,
        mcp_servers: &[McpServerConfig],
    ) -> Result<SessionNewResult, AcpError> {
        self.create_session_resilient(system_append, mcp_servers)
            .await
    }

    async fn set_session_model(
        &self,
        session_id: &SessionId,
        model_id: &ModelId,
    ) -> Result<(), AcpError> {
        let active = self.active_gen();
        let _: serde::de::IgnoredAny = active
            .inner
            .call_rpc(
                "session/set_model",
                SessionSetModelParams {
                    session_id: session_id.as_str(),
                    model_id: model_id.as_str(),
                },
            )
            .await?;
        Ok(())
    }

    async fn set_session_mode(
        &self,
        session_id: &SessionId,
        mode_id: &AcpModeId,
    ) -> Result<(), AcpError> {
        let active = self.active_gen();
        let _: serde::de::IgnoredAny = active
            .inner
            .call_rpc(
                "session/set_mode",
                SessionSetModeParams {
                    session_id: session_id.as_str(),
                    mode_id: mode_id.as_str(),
                },
            )
            .await?;
        Ok(())
    }

    async fn start_prompt(
        &self,
        session_id: &SessionId,
        prompt: &[AcpPromptBlock],
        updates_tx: mpsc::Sender<SessionUpdatePayload>,
    ) -> Result<PromptTurnResult, AcpError> {
        let active = self.active_gen();
        let _slot_guard = {
            match active.inner.session_streams.lock() {
                Ok(mut guard) => {
                    guard.insert(session_id.clone(), updates_tx);
                }
                Err(poisoned) => {
                    poisoned.into_inner().insert(session_id.clone(), updates_tx);
                }
            }
            StreamSlotGuard {
                map: Arc::clone(&active.inner.session_streams),
                session_id: session_id.clone(),
            }
        };

        active
            .inner
            .call_rpc(
                "session/prompt",
                SessionPromptParams {
                    session_id: session_id.as_str(),
                    prompt,
                },
            )
            .await
    }

    fn cancel_prompt_nonblocking(&self, session_id: &SessionId) {
        let active = self.active_gen();
        let _ = active
            .inner
            .abort_tx
            .try_send(AbortCommand::CancelOnly(session_id.clone()));
    }

    fn cancel_prompt_for_reuse(&self, session_id: &SessionId) -> bool {
        let active = self.active_gen();
        active
            .inner
            .abort_tx
            .try_send(AbortCommand::CancelOnly(session_id.clone()))
            .is_ok()
    }

    fn abort_session_nonblocking(&self, session_id: &SessionId) {
        let active = self.active_gen();
        let _ = active
            .inner
            .abort_tx
            .try_send(AbortCommand::CancelAndClose(session_id.clone()));
    }

    async fn close_session(&self, session_id: &SessionId) -> Result<(), AcpError> {
        self.close_session_inner(session_id).await
    }

    fn recent_stderr_since(&self, since_unix_ms: u64) -> Vec<String> {
        let active = self.active_gen();
        let guard = match active.inner.stderr_log.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard
            .iter()
            .filter(|entry| entry.timestamp_unix_ms >= since_unix_ms)
            .map(|entry| entry.line.clone())
            .collect()
    }

    async fn fetch_web_url(
        &self,
        url: &str,
        allowed_domains: &[String],
        blocked_domains: &[String],
    ) -> crate::web_fetch::WebFetchOutcome {
        crate::web_fetch::fetch_url_sanitized_with_options(
            url,
            allowed_domains,
            blocked_domains,
            self.cfg.agy_proxy.as_ref(),
            self.cfg.spill_dir.as_deref(),
        )
        .await
    }

    async fn fetch_web_url_with_pattern(
        &self,
        url: &str,
        pattern: Option<&str>,
        allowed_domains: &[String],
        blocked_domains: &[String],
    ) -> crate::web_fetch::WebFetchOutcome {
        crate::web_fetch::fetch_url_sanitized_with_options_and_pattern(
            url,
            allowed_domains,
            blocked_domains,
            self.cfg.agy_proxy.as_ref(),
            self.cfg.spill_dir.as_deref(),
            pattern,
        )
        .await
    }

    fn process_telemetry(&self, default_model: &str, acp_mode: &str) -> AcpProcessTelemetry {
        (**self).process_telemetry(default_model, acp_mode)
    }
}

impl LiveAcpClient {
    #[must_use]
    pub fn process_telemetry(&self, default_model: &str, acp_mode: &str) -> AcpProcessTelemetry {
        let active = self.active_gen();
        let inner = &active.inner;
        let now_ms = unix_timestamp_ms();
        let uptime_secs = now_ms.saturating_sub(inner.started_at_unix_ms) / 1000;
        let (protocol_version, agent_name, agent_version) = match inner.agent_meta.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        };
        let rpc_pending_in_flight = match inner.pending.lock() {
            Ok(g) => g.len(),
            Err(p) => p.into_inner().len(),
        };
        let active_prompt_streams = match inner.session_streams.lock() {
            Ok(g) => g.len(),
            Err(p) => p.into_inner().len(),
        };
        let recent_stderr = match inner.stderr_log.lock() {
            Ok(g) => g.iter().cloned().collect(),
            Err(p) => p.into_inner().iter().cloned().collect(),
        };
        let auto_meta = match self.auto_update_state.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        };

        AcpProcessTelemetry {
            connected: !inner.stdin_tx.is_closed() && !active.cancel.is_cancelled(),
            pid: inner.child_pid,
            binary_path: inner.binary_path.as_ref().to_owned(),
            session_cwd: inner.session_cwd.as_ref().to_owned(),
            os: Cow::Borrowed(std::env::consts::OS),
            arch: Cow::Borrowed(std::env::consts::ARCH),
            supervision_strategy: Cow::Borrowed(supervision_strategy_name()),
            protocol_version,
            agent_name,
            agent_version,
            started_at_unix_ms: inner.started_at_unix_ms,
            uptime_secs,
            default_model: default_model.to_owned(),
            acp_mode: acp_mode.to_owned(),
            rpc_requests_sent: inner.rpc_stats.requests_sent.load(Ordering::Relaxed),
            rpc_responses_received: inner.rpc_stats.responses_received.load(Ordering::Relaxed),
            rpc_notifications_received: inner
                .rpc_stats
                .notifications_received
                .load(Ordering::Relaxed),
            rpc_pending_in_flight,
            active_prompt_streams,
            recent_stderr,
            agy_proxy: self
                .cfg
                .agy_proxy
                .as_ref()
                .map(|p| p.redacted_display().into_owned()),
            auto_update_enabled: self.is_auto_update_enabled(),
            latest_known_version: auto_meta.latest_known_version,
            last_update_check_unix_ms: auto_meta.last_update_check_unix_ms,
            last_auto_update_unix_ms: auto_meta.last_auto_update_unix_ms,
            last_auto_updated_from: auto_meta.last_auto_updated_from,
            auto_update_note: auto_meta.auto_update_note,
        }
    }
}

impl LiveAcpInner {
    async fn call_rpc<P, R>(&self, method: &'static str, params: P) -> Result<R, AcpError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = JsonRpcRequest::new(id, method, params);
        let mut encoded =
            serde_json::to_vec(&req).map_err(|source| AcpError::EncodeJson { source })?;
        encoded.push(b'\n');

        let (tx, rx) = oneshot::channel();
        {
            match self.pending.lock() {
                Ok(mut guard) => {
                    guard.insert(id, tx);
                }
                Err(poisoned) => {
                    poisoned.into_inner().insert(id, tx);
                }
            }
        }
        let _pending_guard = PendingSlotGuard {
            map: Arc::clone(&self.pending),
            id,
        };

        self.rpc_stats.requests_sent.fetch_add(1, Ordering::Relaxed);
        self.stdin_tx
            .send(Bytes::from(encoded))
            .await
            .map_err(|_| AcpError::TransportClosed)?;

        let raw_res = rx.await.map_err(|_| AcpError::TransportClosed)?;
        match raw_res {
            Ok(raw_val) => serde_json::from_str(raw_val.get())
                .map_err(|source| AcpError::DecodeJson { method, source }),
            Err(rpc_err) => Err(AcpError::Rpc {
                method,
                code: rpc_err.code,
                message: format_rpc_error_message(rpc_err),
            }),
        }
    }

    async fn send_notification<P: Serialize>(&self, method: &'static str, params: P) {
        let notif = JsonRpcNotification::new(method, params);
        if let Ok(mut encoded) = serde_json::to_vec(&notif) {
            encoded.push(b'\n');
            let _ = self.stdin_tx.send(Bytes::from(encoded)).await;
        }
    }
}

async fn run_stdin_writer(
    mut stdin: tokio::process::ChildStdin,
    mut rx: mpsc::Receiver<Bytes>,
    cancel: CancellationToken,
) {
    loop {
        let next_frame = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            frame = rx.recv() => frame,
        };
        let Some(frame) = next_frame else {
            break;
        };
        if stdin.write_all(&frame).await.is_err() {
            break;
        }
        if stdin.flush().await.is_err() {
            break;
        }
    }
}

async fn run_stdout_reader(
    stdout: tokio::process::ChildStdout,
    max_line_bytes: usize,
    pending: Arc<PendingResponseMap>,
    session_streams: Arc<SessionStreamMap>,
    rpc_stats: Arc<RpcCounters>,
    reply_stdin_tx: mpsc::Sender<Bytes>,
    cancel: CancellationToken,
) {
    let mut reader = FramedRead::new(stdout, LinesCodec::new_with_max_length(max_line_bytes));
    loop {
        let next_item = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            item = reader.next() => item,
        };

        let line = match next_item {
            Some(Ok(line)) => line,
            Some(Err(err)) => {
                tracing::error!(error = ?err, "ACP stdout line codec error");
                break;
            }
            None => break,
        };

        if line.trim().is_empty() {
            continue;
        }

        let frame: IncomingRpcFrame<'_> = match serde_json::from_str(&line) {
            Ok(f) => f,
            Err(err) => {
                tracing::warn!(error = ?err, "ignoring malformed JSON-RPC line from ACP stdout");
                continue;
            }
        };

        match (frame.id, frame.method.as_deref()) {
            // Notification from agent -> client
            (None, Some("session/update")) => {
                rpc_stats
                    .notifications_received
                    .fetch_add(1, Ordering::Relaxed);
                if let Some(params_raw) = frame.params {
                    match serde_json::from_str::<SessionUpdateNotification>(params_raw.get()) {
                        Ok(notif) => {
                            let sender = match session_streams.lock() {
                                Ok(guard) => guard.get(&notif.session_id).cloned(),
                                Err(poisoned) => {
                                    poisoned.into_inner().get(&notif.session_id).cloned()
                                }
                            };
                            if let Some(tx) = sender {
                                tokio::select! {
                                    biased;
                                    () = cancel.cancelled() => break,
                                    _ = tx.send(notif.update) => {}
                                }
                            }
                        }
                        Err(err) => {
                            tracing::warn!(
                                error = ?err,
                                raw_snippet = %truncate_snippet(params_raw.get(), 256),
                                "failed to decode session/update payload"
                            );
                        }
                    }
                }
            }
            // Request from agent -> client (auto-approve permission requests if ever triggered)
            (Some(req_id_raw), Some("session/request_permission")) => {
                let selected_option = frame
                    .params
                    .and_then(|p| serde_json::from_str::<RequestPermissionParams>(p.get()).ok())
                    .and_then(|p| {
                        p.options
                            .iter()
                            .find(|o| o.kind.contains("allow"))
                            .or_else(|| p.options.first())
                            .map(|o| o.option_id.clone())
                    })
                    .unwrap_or_else(|| "allow_once".to_owned());

                let reply = JsonRpcResponseOut {
                    jsonrpc: "2.0",
                    id: req_id_raw,
                    result: RequestPermissionResult {
                        outcome: PermissionOutcome::Selected {
                            option_id: selected_option,
                        },
                    },
                };
                if let Ok(mut bytes) = serde_json::to_vec(&reply) {
                    bytes.push(b'\n');
                    let _ = reply_stdin_tx.try_send(Bytes::from(bytes));
                }
            }
            // Response to our outbound request
            (Some(id_raw), None) => {
                rpc_stats.responses_received.fetch_add(1, Ordering::Relaxed);
                if let Ok(numeric_id) = serde_json::from_str::<u64>(id_raw.get()) {
                    let waiter = match pending.lock() {
                        Ok(mut guard) => guard.remove(&numeric_id),
                        Err(poisoned) => poisoned.into_inner().remove(&numeric_id),
                    };
                    if let Some(tx) = waiter {
                        if let Some(err_obj) = frame.error {
                            let _ = tx.send(Err(err_obj));
                        } else if let Some(res_raw) = frame.result {
                            let _ = tx.send(Ok(res_raw.to_owned()));
                        } else if let Ok(null_raw) = RawValue::from_string("null".to_owned()) {
                            let _ = tx.send(Ok(null_raw));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Fail any remaining pending requests immediately on EOF/shutdown
    let drained: Vec<_> = match pending.lock() {
        Ok(mut guard) => guard.drain().map(|(_, tx)| tx).collect(),
        Err(poisoned) => poisoned.into_inner().drain().map(|(_, tx)| tx).collect(),
    };
    drop(drained);
}

async fn run_abort_worker(
    inner: Arc<LiveAcpInner>,
    mut abort_rx: mpsc::Receiver<AbortCommand>,
    cancel: CancellationToken,
) {
    loop {
        let cmd = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            cmd = abort_rx.recv() => cmd,
        };
        let Some(cmd) = cmd else {
            break;
        };
        match cmd {
            AbortCommand::CancelOnly(sid) => {
                inner
                    .send_notification(
                        "session/cancel",
                        SessionCancelParams {
                            session_id: sid.as_str(),
                        },
                    )
                    .await;
            }
            AbortCommand::CancelAndClose(sid) => {
                inner
                    .send_notification(
                        "session/cancel",
                        SessionCancelParams {
                            session_id: sid.as_str(),
                        },
                    )
                    .await;
                let _: Result<serde::de::IgnoredAny, _> = inner
                    .call_rpc(
                        "session/close",
                        SessionCloseParams {
                            session_id: sid.as_str(),
                        },
                    )
                    .await;
            }
        }
    }
}

fn canonicalize_or_keep(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn truncate_snippet(input: &str, max_chars: usize) -> String {
    let Some((byte_idx, _)) = input.char_indices().nth(max_chars) else {
        return input.to_owned();
    };
    let Some((head, _)) = input.split_at_checked(byte_idx) else {
        return input.to_owned();
    };
    let mut out = head.trim_end().to_owned();
    out.push_str("...");
    out
}

fn format_rpc_error_message(rpc_err: JsonRpcErrorPayload) -> String {
    let base = rpc_err.message.trim();
    let Some(data) = rpc_err.data else {
        return base.to_owned();
    };
    let raw = data.get().trim();
    if raw.is_empty() || raw == "null" || raw == "{}" {
        return base.to_owned();
    }
    let snippet = truncate_snippet(raw, 512);
    if base.is_empty() {
        snippet
    } else if base.contains(&snippet) {
        base.to_owned()
    } else {
        format!("{base} — {snippet}")
    }
}

const fn supervision_strategy_name() -> &'static str {
    #[cfg(windows)]
    {
        "Win32 Job Object (KILL_ON_JOB_CLOSE + CREATE_NEW_PROCESS_GROUP)"
    }
    #[cfg(target_os = "linux")]
    {
        "Linux Process Group (killpg) + PR_SET_PDEATHSIG"
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        "POSIX Process Group (killpg + SIGTERM/SIGINT/SIGHUP)"
    }
    #[cfg(not(any(windows, unix)))]
    {
        "Standard Child Supervision"
    }
}
