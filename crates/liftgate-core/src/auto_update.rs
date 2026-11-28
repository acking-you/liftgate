//! Host integration glue connecting [`liftgate_auto_update`] to Liftgate's live server handles,
//! OS process-group supervision, `agy` hot-updater, and SHA-256 verified self-upgrader.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use liftgate_auto_update::{
    AutoUpdateCliOverrides, AutoUpdateProbeCache, AutoUpdateStatusReport, FleetAutoUpdateOptions,
    IdleGateEvaluation, SingleTargetRunResult, extract_clean_version_token,
};
use liftgate_auto_update::{
    ChildGroupSupervisor, FleetSwitchSnapshot, FleetUpdateBackend, ProbedTargetInfo,
    TargetSelectionError, build_target_telemetry_rows, persist_target_result_file,
    probe_all_targets_concurrently, run_harness_target_pass, run_idle_fleet_scheduler,
};
use liftgate_server::{
    acp_client::LiveAcpClient,
    job::{ProcessGroupGuard, configure_command_group},
    platform::canonicalize_clean,
    session_pool::SessionPool,
    telemetry::TelemetryHub,
};
use liftgate_types::{
    config::{AutoUpdateIntervalMins, AutoUpdateTarget},
    telemetry::{AutoUpdateLastOutcome, AutoUpdateTelemetrySummary},
};
use tokio::process::{Child, Command};
use tokio::sync::watch;

use crate::agy_manager::{
    AgyUpdateStatus, apply_idle_agy_hot_update_if_idle, detect_agy_platform_slug,
    execute_agy_update, fetch_agy_manifest, probe_agy_version, sweep_agy_backups_in_dir,
};
use crate::cli::{
    AgyUpdateArgs, AutoUpdateIntervalArgs, AutoUpdateRunArgs, AutoUpdateTargetsArgs,
    DaemonRespawnArgs, UpgradeArgs,
};
use crate::daemon;
use crate::desktop;
use crate::error::CliError;
use crate::state::{
    LiftgateSettings, StatePaths, now_unix_ms, resolve_effective_agy_bin,
    resolve_effective_agy_proxy,
};
use crate::upgrade::{self, IdleSelfUpgradeOutcome, SemVer};

const DAEMON_RESPAWN_WAIT_TIMEOUT: Duration = Duration::from_secs(20);

/// Bridges `liftgate-server`'s OS process-group / Win32 Job Object supervision into [`liftgate_auto_update`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ServerProcessGroupSupervisor;

impl ChildGroupSupervisor for ServerProcessGroupSupervisor {
    fn configure_command(&self, cmd: &mut Command) {
        configure_command_group(cmd);
    }

    fn attach_guard(&self, child: &Child) -> Option<Box<dyn Send + Sync>> {
        ProcessGroupGuard::attach(child)
            .ok()
            .map(|guard| Box::new(guard) as Box<dyn Send + Sync>)
    }
}

/// Evaluates whether the gateway and local client launchers currently have zero active sessions.
#[must_use]
pub fn evaluate_idle_gate(
    state_paths: &StatePaths,
    live_client: &LiveAcpClient,
    pool: &SessionPool,
    telemetry: &TelemetryHub,
    empty_pool_cooldown_ms: u64,
    warm_pool_cooldown_ms: u64,
) -> IdleGateEvaluation {
    let (metrics, _, _, _) = telemetry.snapshot_tasks();
    if metrics.active_requests > 0 {
        return IdleGateEvaluation::Busy(format!(
            "{} active HTTP turn(s) in flight",
            metrics.active_requests
        ));
    }
    let (rpc_in_flight, prompt_streams) = live_client.active_activity_counts();
    if rpc_in_flight > 0 || prompt_streams > 0 {
        return IdleGateEvaluation::Busy("active ACP session stream".to_owned());
    }
    let (active_leases, idle_sessions) = pool.idle_and_active_counts();
    let suspended = pool.suspended_count();
    if active_leases > 0 || suspended > 0 {
        return IdleGateEvaluation::Busy(format!(
            "{active_leases} active / {suspended} suspended session(s)"
        ));
    }
    if let IdleGateEvaluation::Busy(local_reason) = evaluate_local_client_idle(state_paths) {
        return IdleGateEvaluation::Busy(local_reason);
    }
    let quiet_required_ms = if idle_sessions == 0 {
        empty_pool_cooldown_ms
    } else {
        warm_pool_cooldown_ms
    };
    let quiet_elapsed_ms = now_unix_ms().saturating_sub(pool.last_activity_unix_ms());
    if quiet_elapsed_ms < quiet_required_ms {
        let rem_secs = (quiet_required_ms - quiet_elapsed_ms).div_ceil(1_000);
        return IdleGateEvaluation::Busy(format!("session quiet cooldown ({rem_secs}s left)"));
    }
    IdleGateEvaluation::Idle
}

/// Evaluates local client session activity when no live gateway telemetry handle is attached.
#[must_use]
pub fn evaluate_local_client_idle(state_paths: &StatePaths) -> IdleGateEvaluation {
    let active_cli_tools = state_paths.active_client_session_tools();
    if !active_cli_tools.is_empty() {
        return IdleGateEvaluation::Busy(format!(
            "open CLI session ({})",
            active_cli_tools.join(", ")
        ));
    }
    if desktop::has_active_desktop_sessions(state_paths) {
        return IdleGateEvaluation::Busy("open desktop GUI session".to_owned());
    }
    IdleGateEvaluation::Idle
}

/// Syncs `daemon.json`'s cached `lg_auto_update` and `agy_auto_update` fields whenever
/// `settings.json` is mutated so `lg status` and `start_daemon` stay consistent without a restart.
pub fn sync_daemon_state_toggles(state_paths: &StatePaths, settings: &LiftgateSettings) {
    if let Ok(Some(mut d)) = state_paths.read_daemon()
        && daemon::is_pid_alive(d.pid)
        && (d.lg_auto_update != settings.lg_auto_update
            || d.agy_auto_update != settings.agy_auto_update)
    {
        d.lg_auto_update = settings.lg_auto_update;
        d.agy_auto_update = settings.agy_auto_update;
        let _ = state_paths.write_daemon(&d);
    }
}

/// Parses CLI target arguments (`lg`, `agy`, `codex`, `claude`, `opencode`, `omp`, or `all`).
pub fn parse_target_selection(
    raw_targets: &[String],
    all_flag: bool,
) -> Result<Vec<AutoUpdateTarget>, CliError> {
    liftgate_auto_update::parse_target_selection(raw_targets, all_flag).map_err(|err| match err {
        TargetSelectionError::EmptySelection => CliError::UnknownAutoUpdateTarget {
            value: String::new(),
        },
        TargetSelectionError::UnknownTarget { name } => {
            CliError::UnknownAutoUpdateTarget { value: name }
        }
    })
}

/// Applies `lg auto-update enable|disable` to `settings.json`.
pub fn apply_targets_toggle(
    state_paths: &StatePaths,
    args: &AutoUpdateTargetsArgs,
    enabled: bool,
) -> Result<Vec<AutoUpdateTarget>, CliError> {
    let targets = parse_target_selection(&args.targets, args.all)?;
    let mut settings = state_paths.read_settings()?;
    for &target in &targets {
        settings.set_auto_update_enabled(target, enabled);
    }
    state_paths.write_settings(&settings)?;
    sync_daemon_state_toggles(state_paths, &settings);
    Ok(targets)
}

/// Applies `lg auto-update interval <minutes>` to `settings.json`.
pub fn apply_interval_change(
    state_paths: &StatePaths,
    args: &AutoUpdateIntervalArgs,
) -> Result<AutoUpdateIntervalMins, CliError> {
    let interval = AutoUpdateIntervalMins::try_new(args.minutes)?;
    let mut settings = state_paths.read_settings()?;
    settings.auto_update_interval_mins = interval;
    state_paths.write_settings(&settings)?;
    Ok(interval)
}

/// Resets auto-update switches in `settings.json` to defaults (`lg` & `agy` ON; harnesses OFF).
pub fn reset_to_defaults(state_paths: &StatePaths) -> Result<(), CliError> {
    let mut settings = state_paths.read_settings()?;
    settings.reset_auto_update_defaults();
    state_paths.write_settings(&settings)?;
    sync_daemon_state_toggles(state_paths, &settings);
    Ok(())
}

/// Inspects all 6 managed targets, combining live gateway telemetry, `settings.json`,
/// `auto-update-state.json`, and local binary/version probes.
pub async fn inspect_auto_update_status(
    state_paths: &StatePaths,
) -> Result<AutoUpdateStatusReport, CliError> {
    let mut cache = AutoUpdateProbeCache::default();
    inspect_auto_update_status_cached(state_paths, &mut cache, true).await
}

/// Inspects fleet auto-update status, only spawning `--version` subprocesses when `cache` is
/// empty or `force_reprobe` is `true`.
pub async fn inspect_auto_update_status_cached(
    state_paths: &StatePaths,
    cache: &mut AutoUpdateProbeCache,
    force_reprobe: bool,
) -> Result<AutoUpdateStatusReport, CliError> {
    let settings = state_paths.read_settings()?;
    let ledger = state_paths.read_auto_update_ledger().unwrap_or_default();
    let active_daemon = state_paths
        .read_daemon()?
        .filter(|d| daemon::is_pid_alive(d.pid));

    let mut live_summary: Option<AutoUpdateTelemetrySummary> = None;
    if let Some(d) = &active_daemon {
        let client = crate::client::GatewayClient::new(d.listen, d.api_key.clone());
        if let Ok(snap) = client.fetch_telemetry().await {
            live_summary = snap.auto_update;
        }
    }

    let local_idle = evaluate_local_client_idle(state_paths);
    let (idle_ready, busy_reason, sweep_in_progress, active_target) = match &live_summary {
        Some(s) => (
            s.idle_ready && local_idle.is_idle(),
            s.busy_reason
                .clone()
                .or_else(|| local_idle.busy_reason().map(str::to_owned)),
            s.sweep_in_progress,
            s.active_target,
        ),
        None => (
            local_idle.is_idle(),
            local_idle.busy_reason().map(str::to_owned),
            false,
            None,
        ),
    };

    let probed = cache
        .get_or_probe_with(force_reprobe, || probe_host_targets(&settings))
        .await;
    let targets = build_target_telemetry_rows(
        settings.auto_update_switches(),
        &ledger,
        probed,
        AutoUpdateCliOverrides::default(),
    );

    Ok(AutoUpdateStatusReport {
        settings_file: state_paths.settings_file(),
        ledger_file: state_paths.auto_update_state_file(),
        daemon_running: active_daemon.is_some(),
        daemon_pid: active_daemon.map(|d| d.pid),
        interval_mins: settings.auto_update_interval_mins.get(),
        idle_ready,
        busy_reason,
        sweep_in_progress,
        active_target,
        targets,
    })
}

/// Runs an on-demand check or update pass from the CLI or Auto-Update Manager TUI.
pub async fn execute_manual_auto_update_run(
    state_paths: &StatePaths,
    args: &AutoUpdateRunArgs,
) -> Result<Vec<SingleTargetRunResult>, CliError> {
    let settings = state_paths.read_settings()?;
    let selected = if args.all {
        AutoUpdateTarget::ALL.to_vec()
    } else if !args.targets.is_empty() {
        parse_target_selection(&args.targets, false)?
    } else {
        AutoUpdateTarget::ALL
            .into_iter()
            .filter(|&t| settings.is_auto_update_enabled(t))
            .collect()
    };

    let mut results = Vec::with_capacity(selected.len());
    for target in selected {
        let res = run_manual_single_target(state_paths, &settings, target, args.check).await;
        results.push(res);
    }
    Ok(results)
}

/// Runs a manual check or update for a single `target` and persists its outcome in `auto-update-state.json`.
pub async fn run_manual_single_target(
    state_paths: &StatePaths,
    _settings: &LiftgateSettings,
    target: AutoUpdateTarget,
    check_only: bool,
) -> SingleTargetRunResult {
    let now = now_unix_ms();
    let res = match target {
        AutoUpdateTarget::Liftgate => run_manual_lg_target(state_paths, check_only).await,
        AutoUpdateTarget::Agy => run_manual_agy_target(state_paths, check_only).await,
        AutoUpdateTarget::Codex
        | AutoUpdateTarget::Claude
        | AutoUpdateTarget::Opencode
        | AutoUpdateTarget::Omp => {
            let (_stop_tx, stop_rx) = watch::channel(false);
            run_harness_target_pass(target, check_only, &ServerProcessGroupSupervisor, &stop_rx)
                .await
        }
    };

    let _ = state_paths.ensure_root_dir();
    let _ = persist_target_result_file(&state_paths.auto_update_state_file(), &res, now);
    res
}

/// Runs the unified background idle auto-update coordinator inside `liftgate serve`.
pub async fn run_idle_fleet_auto_updater(
    state_paths: StatePaths,
    live_client: Arc<LiveAcpClient>,
    pool: Arc<SessionPool>,
    telemetry: Arc<TelemetryHub>,
    cli_overrides: AutoUpdateCliOverrides,
    stop_tx: watch::Sender<bool>,
    stop_rx: watch::Receiver<bool>,
) {
    run_idle_fleet_auto_updater_with_options(
        state_paths,
        live_client,
        pool,
        telemetry,
        cli_overrides,
        stop_tx,
        stop_rx,
        FleetAutoUpdateOptions::from_env(),
    )
    .await;
}

/// Runs the unified background idle auto-update coordinator with explicit timing options.
#[expect(
    clippy::too_many_arguments,
    reason = "daemon background coordinator wires shared gateway handles and explicit test options"
)]
pub async fn run_idle_fleet_auto_updater_with_options(
    state_paths: StatePaths,
    live_client: Arc<LiveAcpClient>,
    pool: Arc<SessionPool>,
    telemetry: Arc<TelemetryHub>,
    cli_overrides: AutoUpdateCliOverrides,
    stop_tx: watch::Sender<bool>,
    stop_rx: watch::Receiver<bool>,
    opts: FleetAutoUpdateOptions,
) {
    let backend = CoreFleetUpdateBackend {
        settings_file: state_paths.settings_file(),
        ledger_file: state_paths.auto_update_state_file(),
        state_paths,
        live_client,
        pool,
        telemetry,
        supervisor: ServerProcessGroupSupervisor,
    };
    run_idle_fleet_scheduler(&backend, cli_overrides, stop_tx, stop_rx, opts).await;
}

/// Waits for the retiring managed daemon (`args.old_pid`) to close its sockets, cleans up stale
/// `.old-upgrade` backups, and relaunches the managed daemon on the newly installed executable.
pub async fn execute_daemon_respawn(
    state_paths: &StatePaths,
    args: DaemonRespawnArgs,
) -> Result<(), CliError> {
    let deadline = Instant::now() + DAEMON_RESPAWN_WAIT_TIMEOUT;
    while daemon::is_pid_alive(args.old_pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(75)).await;
    }
    upgrade::sweep_stale_backups();

    let Some(prev) = state_paths.read_daemon()? else {
        return Ok(());
    };
    if prev.pid != args.old_pid && daemon::is_pid_alive(prev.pid) {
        return Ok(());
    }
    let serve_cfg = daemon::serve_args_from_daemon_state(&prev);
    let current_exe = std::env::current_exe().ok();
    let _ =
        daemon::start_daemon_with_exe(state_paths, current_exe.as_deref(), serve_cfg, true).await?;
    Ok(())
}

struct CoreFleetUpdateBackend {
    state_paths: StatePaths,
    settings_file: PathBuf,
    ledger_file: PathBuf,
    live_client: Arc<LiveAcpClient>,
    pool: Arc<SessionPool>,
    telemetry: Arc<TelemetryHub>,
    supervisor: ServerProcessGroupSupervisor,
}

impl FleetUpdateBackend for CoreFleetUpdateBackend {
    fn settings_file(&self) -> &Path {
        &self.settings_file
    }

    fn ledger_file(&self) -> &Path {
        &self.ledger_file
    }

    fn read_switches(&self) -> FleetSwitchSnapshot {
        self.state_paths
            .read_settings()
            .unwrap_or_default()
            .auto_update_switches()
    }

    fn sync_agy_auto_update_flag(&self, enabled: bool) {
        self.live_client.set_auto_update_enabled(enabled);
    }

    fn evaluate_idle_gate(&self, opts: &FleetAutoUpdateOptions) -> IdleGateEvaluation {
        evaluate_idle_gate(
            &self.state_paths,
            &self.live_client,
            &self.pool,
            &self.telemetry,
            opts.empty_pool_cooldown_ms,
            opts.warm_pool_cooldown_ms,
        )
    }

    fn publish_summary(&self, summary: AutoUpdateTelemetrySummary) {
        self.telemetry.set_auto_update_summary(summary);
    }

    fn child_supervisor(&self) -> &dyn ChildGroupSupervisor {
        &self.supervisor
    }

    async fn probe_lg_and_agy(
        &self,
        _switches: FleetSwitchSnapshot,
    ) -> (ProbedTargetInfo, ProbedTargetInfo) {
        let settings = self.state_paths.read_settings().unwrap_or_default();
        probe_lg_and_agy_infos(&settings).await
    }

    async fn run_idle_lg_pass(
        &self,
        opts: &FleetAutoUpdateOptions,
        _stop_rx: &watch::Receiver<bool>,
    ) -> (SingleTargetRunResult, Option<PathBuf>) {
        let settings = self.state_paths.read_settings().unwrap_or_default();
        execute_idle_lg_pass(
            &self.state_paths,
            &settings,
            &self.live_client,
            &self.pool,
            &self.telemetry,
            opts,
        )
        .await
    }

    async fn run_idle_agy_pass(&self, opts: &FleetAutoUpdateOptions) -> SingleTargetRunResult {
        execute_idle_agy_pass(
            &self.state_paths,
            &self.live_client,
            &self.pool,
            &self.telemetry,
            opts,
        )
        .await
    }

    fn handoff_self_upgrade(
        &self,
        new_liftgate_exe: &Path,
        stop_tx: &watch::Sender<bool>,
    ) -> Result<(), String> {
        spawn_detached_daemon_respawn(&self.state_paths, new_liftgate_exe, std::process::id())
            .map_err(|e| e.display_chain())?;
        tracing::info!(
            exe = %new_liftgate_exe.display(),
            "handed off self-upgraded daemon respawn; initiating graceful drain"
        );
        let _ = stop_tx.send_replace(true);
        Ok(())
    }
}

async fn probe_lg_and_agy_infos(
    settings: &LiftgateSettings,
) -> (ProbedTargetInfo, ProbedTargetInfo) {
    let lg_info = ProbedTargetInfo {
        binary_path: std::env::current_exe()
            .ok()
            .map(|p| canonicalize_clean(&p).display().to_string()),
        installed_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
    };
    let proxy = resolve_effective_agy_proxy(None, settings).ok().flatten();
    let agy_info = match resolve_effective_agy_bin(None, settings) {
        Some(bin) => {
            let ver = probe_agy_version(&bin, proxy.as_ref()).await;
            ProbedTargetInfo {
                binary_path: Some(bin.display().to_string()),
                installed_version: ver,
            }
        }
        None => ProbedTargetInfo::default(),
    };
    (lg_info, agy_info)
}

async fn probe_host_targets(settings: &LiftgateSettings) -> [ProbedTargetInfo; 6] {
    let lg_info = ProbedTargetInfo {
        binary_path: std::env::current_exe()
            .ok()
            .map(|p| canonicalize_clean(&p).display().to_string()),
        installed_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
    };
    let proxy = resolve_effective_agy_proxy(None, settings).ok().flatten();
    let agy_bin = resolve_effective_agy_bin(None, settings);
    let agy_fut = async move {
        match agy_bin {
            Some(bin) => {
                let ver = probe_agy_version(&bin, proxy.as_ref()).await;
                ProbedTargetInfo {
                    binary_path: Some(bin.display().to_string()),
                    installed_version: ver,
                }
            }
            None => ProbedTargetInfo::default(),
        }
    };
    probe_all_targets_concurrently(lg_info, agy_fut, &ServerProcessGroupSupervisor).await
}

async fn run_manual_lg_target(state_paths: &StatePaths, check_only: bool) -> SingleTargetRunResult {
    let Ok(current_exe) = std::env::current_exe() else {
        return SingleTargetRunResult {
            target: AutoUpdateTarget::Liftgate,
            status: AutoUpdateLastOutcome::Failed,
            previous_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            current_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            latest_version: None,
            note: "failed to resolve current executable path".to_owned(),
        };
    };
    let upg_args = UpgradeArgs {
        check: check_only,
        ..UpgradeArgs::default()
    };
    match upgrade::execute_upgrade_with_progress(state_paths, &upg_args, &current_exe, None).await {
        Ok(out) => {
            let status = match out.status {
                "upgraded" => AutoUpdateLastOutcome::Updated,
                "update_available" => AutoUpdateLastOutcome::UpdateAvailable,
                _ => AutoUpdateLastOutcome::UpToDate,
            };
            let note = match status {
                AutoUpdateLastOutcome::Updated => format!(
                    "updated v{} -> v{}{}",
                    out.current_version,
                    out.target_version,
                    if out.daemon_restarted {
                        " (daemon restarted)"
                    } else {
                        ""
                    }
                ),
                AutoUpdateLastOutcome::UpdateAvailable => {
                    format!("v{} available", out.target_version)
                }
                _ => "up to date".to_owned(),
            };
            let cur_after = if out.upgraded {
                out.target_version.clone()
            } else {
                out.current_version.clone()
            };
            SingleTargetRunResult {
                target: AutoUpdateTarget::Liftgate,
                status,
                previous_version: Some(out.current_version),
                current_version: Some(cur_after),
                latest_version: Some(out.target_version),
                note,
            }
        }
        Err(CliError::UpgradePackagedBundle { bundle_kind, .. }) => SingleTargetRunResult {
            target: AutoUpdateTarget::Liftgate,
            status: AutoUpdateLastOutcome::Skipped,
            previous_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            current_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            latest_version: None,
            note: format!("packaged bundle ({bundle_kind}); update via installer"),
        },
        Err(err) => SingleTargetRunResult {
            target: AutoUpdateTarget::Liftgate,
            status: AutoUpdateLastOutcome::Failed,
            previous_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            current_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            latest_version: None,
            note: err.display_chain(),
        },
    }
}

async fn run_manual_agy_target(
    state_paths: &StatePaths,
    check_only: bool,
) -> SingleTargetRunResult {
    let agy_args = AgyUpdateArgs {
        check: check_only,
        force: false,
        proxy: None,
        no_restart: false,
        manifest_base_url: None,
    };
    match execute_agy_update(state_paths, None, &agy_args).await {
        Ok(out) => {
            let (status, note) = match out.status {
                AgyUpdateStatus::Updated | AgyUpdateStatus::InstalledFresh => (
                    AutoUpdateLastOutcome::Updated,
                    format!(
                        "updated {} -> v{}",
                        out.previous_version
                            .as_deref()
                            .map_or_else(|| "fresh".to_owned(), |v| format!("v{v}")),
                        out.latest_version
                    ),
                ),
                AgyUpdateStatus::UpdateAvailable => (
                    AutoUpdateLastOutcome::UpdateAvailable,
                    format!("v{} available", out.latest_version),
                ),
                AgyUpdateStatus::UpToDate => {
                    (AutoUpdateLastOutcome::UpToDate, "up to date".to_owned())
                }
            };
            let cur_after = if matches!(
                out.status,
                AgyUpdateStatus::Updated | AgyUpdateStatus::InstalledFresh
            ) {
                Some(out.latest_version.clone())
            } else {
                out.previous_version.clone()
            };
            SingleTargetRunResult {
                target: AutoUpdateTarget::Agy,
                status,
                previous_version: out.previous_version,
                current_version: cur_after,
                latest_version: Some(out.latest_version),
                note,
            }
        }
        Err(err) => SingleTargetRunResult {
            target: AutoUpdateTarget::Agy,
            status: AutoUpdateLastOutcome::Failed,
            previous_version: None,
            current_version: None,
            latest_version: None,
            note: err.display_chain(),
        },
    }
}

async fn execute_idle_lg_pass(
    state_paths: &StatePaths,
    settings: &LiftgateSettings,
    live_client: &Arc<LiveAcpClient>,
    pool: &Arc<SessionPool>,
    telemetry: &Arc<TelemetryHub>,
    opts: &FleetAutoUpdateOptions,
) -> (SingleTargetRunResult, Option<PathBuf>) {
    let cur_ver = env!("CARGO_PKG_VERSION").to_owned();
    let Ok(current_exe) = std::env::current_exe() else {
        return (
            SingleTargetRunResult {
                target: AutoUpdateTarget::Liftgate,
                status: AutoUpdateLastOutcome::Failed,
                previous_version: Some(cur_ver.clone()),
                current_version: Some(cur_ver),
                latest_version: None,
                note: "failed to resolve current executable path".to_owned(),
            },
            None,
        );
    };

    let mut upg_args = UpgradeArgs::default();
    if upg_args.proxy.is_none()
        && let Some(px) = &settings.agy_proxy
    {
        upg_args.proxy = Some(px.as_str().to_owned());
    }

    let paths_ref = state_paths.clone();
    let client_ref = Arc::clone(live_client);
    let pool_ref = Arc::clone(pool);
    let tel_ref = Arc::clone(telemetry);
    let empty_cd = opts.empty_pool_cooldown_ms;
    let warm_cd = opts.warm_pool_cooldown_ms;

    match upgrade::stage_and_install_upgrade_if_idle(&paths_ref, &upg_args, &current_exe, || {
        evaluate_idle_gate(
            &paths_ref,
            &client_ref,
            &pool_ref,
            &tel_ref,
            empty_cd,
            warm_cd,
        )
        .is_idle()
    })
    .await
    {
        Ok(IdleSelfUpgradeOutcome::UpToDate {
            current_version,
            latest_version,
        }) => (
            SingleTargetRunResult {
                target: AutoUpdateTarget::Liftgate,
                status: AutoUpdateLastOutcome::UpToDate,
                previous_version: Some(current_version.clone()),
                current_version: Some(current_version),
                latest_version: Some(latest_version),
                note: "up to date".to_owned(),
            },
            None,
        ),
        Ok(IdleSelfUpgradeOutcome::SkippedPackagedBundle {
            current_version,
            bundle_kind,
        }) => (
            SingleTargetRunResult {
                target: AutoUpdateTarget::Liftgate,
                status: AutoUpdateLastOutcome::Skipped,
                previous_version: Some(current_version.clone()),
                current_version: Some(current_version),
                latest_version: None,
                note: format!("packaged {bundle_kind} bundle; update via installer"),
            },
            None,
        ),
        Ok(IdleSelfUpgradeOutcome::DeferredBusy {
            current_version,
            target_version,
        }) => (
            SingleTargetRunResult {
                target: AutoUpdateTarget::Liftgate,
                status: AutoUpdateLastOutcome::DeferredBusy,
                previous_version: Some(current_version.clone()),
                current_version: Some(current_version),
                latest_version: Some(target_version.clone()),
                note: format!("v{target_version} staged; deferred because session became active"),
            },
            None,
        ),
        Ok(IdleSelfUpgradeOutcome::Installed {
            from_version,
            to_version,
            liftgate_exe,
        }) => {
            let is_managed_self = state_paths
                .read_daemon()
                .ok()
                .flatten()
                .is_some_and(|d| d.pid == std::process::id());
            let note = if is_managed_self {
                format!("auto-updated v{from_version} -> v{to_version} (daemon respawned)")
            } else {
                format!(
                    "auto-updated binaries v{from_version} -> v{to_version}; restart server to apply"
                )
            };
            tracing::info!(
                from = %from_version,
                to = %to_version,
                managed_respawn = is_managed_self,
                "automatically upgraded Liftgate (`lg` & `liftgate`) while idle"
            );
            (
                SingleTargetRunResult {
                    target: AutoUpdateTarget::Liftgate,
                    status: AutoUpdateLastOutcome::Updated,
                    previous_version: Some(from_version),
                    current_version: Some(to_version.clone()),
                    latest_version: Some(to_version),
                    note,
                },
                is_managed_self.then_some(liftgate_exe),
            )
        }
        Err(CliError::UpgradePermissionDenied { path, .. }) => (
            SingleTargetRunResult {
                target: AutoUpdateTarget::Liftgate,
                status: AutoUpdateLastOutcome::Failed,
                previous_version: Some(cur_ver.clone()),
                current_version: Some(cur_ver),
                latest_version: None,
                note: format!(
                    "permission denied writing {}; run `lg upgrade`",
                    path.display()
                ),
            },
            None,
        ),
        Err(err) => {
            let detail = err.display_chain();
            tracing::warn!(error = %detail, "idle `lg` self-update attempt failed");
            (
                SingleTargetRunResult {
                    target: AutoUpdateTarget::Liftgate,
                    status: AutoUpdateLastOutcome::Failed,
                    previous_version: Some(cur_ver.clone()),
                    current_version: Some(cur_ver),
                    latest_version: None,
                    note: detail,
                },
                None,
            )
        }
    }
}

async fn execute_idle_agy_pass(
    state_paths: &StatePaths,
    live_client: &Arc<LiveAcpClient>,
    pool: &Arc<SessionPool>,
    telemetry: &Arc<TelemetryHub>,
    opts: &FleetAutoUpdateOptions,
) -> SingleTargetRunResult {
    let Ok(platform_slug) = detect_agy_platform_slug() else {
        let msg = "unsupported OS/arch".to_owned();
        live_client.record_auto_update_check(None, Some(msg.clone()));
        return SingleTargetRunResult {
            target: AutoUpdateTarget::Agy,
            status: AutoUpdateLastOutcome::Skipped,
            previous_version: live_client.current_agent_version(),
            current_version: live_client.current_agent_version(),
            latest_version: None,
            note: msg,
        };
    };

    let proxy = live_client.agy_proxy().cloned();
    let bin_path = live_client.binary_path().to_path_buf();
    let manifest = match fetch_agy_manifest(
        opts.agy_manifest_base_url.as_deref(),
        platform_slug,
        proxy.as_ref(),
    )
    .await
    {
        Ok(m) => m,
        Err(err) => {
            let note = format!("check error: {err}");
            live_client.record_auto_update_check(None, Some(note.clone()));
            return SingleTargetRunResult {
                target: AutoUpdateTarget::Agy,
                status: AutoUpdateLastOutcome::Failed,
                previous_version: live_client.current_agent_version(),
                current_version: live_client.current_agent_version(),
                latest_version: None,
                note,
            };
        }
    };

    let current_ver = match live_client.current_agent_version() {
        Some(v) if !v.trim().is_empty() => Some(v),
        _ => probe_agy_version(&bin_path, proxy.as_ref()).await,
    };
    let newer = match &current_ver {
        Some(cur) => match (SemVer::parse(cur), SemVer::parse(&manifest.version)) {
            (Some(c), Some(l)) => l > c,
            _ => *cur != manifest.version,
        },
        None => false,
    };

    if !newer {
        live_client.record_auto_update_check(
            Some(manifest.version.clone()),
            Some("up to date".to_owned()),
        );
        return SingleTargetRunResult {
            target: AutoUpdateTarget::Agy,
            status: AutoUpdateLastOutcome::UpToDate,
            previous_version: current_ver.clone(),
            current_version: current_ver,
            latest_version: Some(manifest.version),
            note: "up to date".to_owned(),
        };
    }

    let prev_str = current_ver.clone().unwrap_or_else(|| "unknown".to_owned());
    let paths_ref = state_paths.clone();
    let client_ref = Arc::clone(live_client);
    let pool_ref = Arc::clone(pool);
    let tel_ref = Arc::clone(telemetry);
    let empty_cd = opts.empty_pool_cooldown_ms;
    let warm_cd = opts.warm_pool_cooldown_ms;

    match apply_idle_agy_hot_update_if_idle(
        state_paths,
        live_client,
        pool,
        &prev_str,
        &manifest,
        move || {
            evaluate_idle_gate(
                &paths_ref,
                &client_ref,
                &pool_ref,
                &tel_ref,
                empty_cd,
                warm_cd,
            )
            .is_idle()
        },
    )
    .await
    {
        Ok(true) => {
            if let Some(parent) = bin_path.parent() {
                sweep_agy_backups_in_dir(parent);
            }
            let note = format!("auto-updated v{prev_str} -> v{}", manifest.version);
            SingleTargetRunResult {
                target: AutoUpdateTarget::Agy,
                status: AutoUpdateLastOutcome::Updated,
                previous_version: Some(prev_str),
                current_version: Some(manifest.version.clone()),
                latest_version: Some(manifest.version),
                note,
            }
        }
        Ok(false) => {
            let note = format!("v{} ready; waiting for idle window", manifest.version);
            live_client
                .record_auto_update_check(Some(manifest.version.clone()), Some(note.clone()));
            SingleTargetRunResult {
                target: AutoUpdateTarget::Agy,
                status: AutoUpdateLastOutcome::DeferredBusy,
                previous_version: current_ver.clone(),
                current_version: current_ver,
                latest_version: Some(manifest.version),
                note,
            }
        }
        Err(err) => {
            let detail = err.display_chain();
            live_client
                .record_auto_update_check(Some(manifest.version.clone()), Some(detail.clone()));
            SingleTargetRunResult {
                target: AutoUpdateTarget::Agy,
                status: AutoUpdateLastOutcome::Failed,
                previous_version: current_ver.clone(),
                current_version: current_ver,
                latest_version: Some(manifest.version),
                note: detail,
            }
        }
    }
}

fn spawn_detached_daemon_respawn(
    state_paths: &StatePaths,
    new_liftgate_exe: &Path,
    old_pid: u32,
) -> Result<(), CliError> {
    let mut cmd = std::process::Command::new(new_liftgate_exe);
    cmd.arg("--state-dir")
        .arg(state_paths.root())
        .arg("__daemon-respawn")
        .arg("--old-pid")
        .arg(old_pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    daemon::configure_detached_command(&mut cmd);
    let _child = cmd.spawn().map_err(|source| CliError::StateIo {
        path: new_liftgate_exe.to_path_buf(),
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use uuid::Uuid;

    use super::*;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Result<Self, std::io::Error> {
            let path = std::env::temp_dir().join(format!("liftgate-au-host-{}", Uuid::new_v4()));
            fs::create_dir_all(&path)?;
            Ok(Self { path })
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn active_client_session_guard_blocks_idle_gate_and_cleans_up_on_drop() {
        let tmp = TempDir::new().unwrap();
        let paths = StatePaths::resolve(Some(tmp.path().to_path_buf())).unwrap();
        assert_eq!(evaluate_local_client_idle(&paths), IdleGateEvaluation::Idle);

        let guard = paths.register_active_client_session("codex").unwrap();
        assert!(paths.has_active_client_sessions());
        assert_eq!(
            evaluate_local_client_idle(&paths),
            IdleGateEvaluation::Busy("open CLI session (codex)".to_owned())
        );

        drop(guard);
        assert!(!paths.has_active_client_sessions());
        assert_eq!(evaluate_local_client_idle(&paths), IdleGateEvaluation::Idle);
    }

    #[test]
    fn settings_toggle_interval_and_reset_persist_cleanly() {
        let tmp = TempDir::new().unwrap();
        let paths = StatePaths::resolve(Some(tmp.path().to_path_buf())).unwrap();

        let enabled = apply_targets_toggle(
            &paths,
            &AutoUpdateTargetsArgs {
                targets: vec!["codex".to_owned(), "opencode".to_owned()],
                all: false,
                json: false,
            },
            true,
        )
        .unwrap();
        assert_eq!(
            enabled,
            vec![AutoUpdateTarget::Codex, AutoUpdateTarget::Opencode]
        );

        let _ = apply_targets_toggle(
            &paths,
            &AutoUpdateTargetsArgs {
                targets: vec!["lg".to_owned()],
                all: false,
                json: false,
            },
            false,
        )
        .unwrap();

        let iv = apply_interval_change(
            &paths,
            &AutoUpdateIntervalArgs {
                minutes: 120,
                json: false,
            },
        )
        .unwrap();
        assert_eq!(iv.get(), 120);

        let s = paths.read_settings().unwrap();
        assert!(!s.lg_auto_update);
        assert!(s.agy_auto_update);
        assert!(s.harness_auto_update.codex);
        assert!(s.harness_auto_update.opencode);
        assert!(!s.harness_auto_update.claude);
        assert!(!s.harness_auto_update.omp);
        assert_eq!(s.auto_update_interval_mins.get(), 120);

        reset_to_defaults(&paths).unwrap();
        let reset = paths.read_settings().unwrap();
        assert!(reset.lg_auto_update);
        assert!(reset.agy_auto_update);
        assert!(!reset.harness_auto_update.codex);
        assert!(!reset.harness_auto_update.opencode);
        assert_eq!(reset.auto_update_interval_mins.get(), 120);
    }
}
