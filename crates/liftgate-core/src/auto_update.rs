//! Unified idle auto-update coordinator for `lg` (`liftgate`), `agy`, and all supported
//! coding harnesses (`codex`, `claude`, `opencode`, `omp`).
//!
//! Safety invariants:
//! - Background checks AND upgrades only run when **zero** sessions are active across HTTP turns,
//!   ACP streams, `SessionPool` active/suspended leases, open CLI harness wrappers (`lg cx`,
//!   `lg cc`, `lg oc`, `lg omp`), and open desktop GUI sessions (`lg cxa`, `lg cd`), plus quiet
//!   cooldown after the most recent turn.
//! - `lg` and `agy` idle auto-updates default to `true`; third-party harnesses default to `false`
//!   and are toggled individually via the dedicated Auto-Update Manager TUI (`lg auto-update`).
//! - All spawned updater subprocesses run non-interactively with `Stdio::null()` stdin, bounded
//!   stdout/stderr capture, per-attempt timeouts, and OS process-group / Win32 Job Object teardown.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use liftgate_harness::ResolvedCommand;
use liftgate_server::{
    acp_client::LiveAcpClient,
    job::{ProcessGroupGuard, configure_command_group},
    platform::{canonicalize_clean, split_expanded_path_var},
    session_pool::SessionPool,
    telemetry::TelemetryHub,
};
use liftgate_types::{
    config::{AutoUpdateIntervalMins, AutoUpdateTarget},
    telemetry::{AutoUpdateLastOutcome, AutoUpdateTargetTelemetry, AutoUpdateTelemetrySummary},
};
use serde::Serialize;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::timeout;

use crate::agy_manager::{
    apply_idle_agy_hot_update_if_idle, detect_agy_platform_slug, fetch_agy_manifest,
    probe_agy_version, sweep_agy_backups_in_dir,
};
use crate::cli::{
    AutoUpdateIntervalArgs, AutoUpdateRunArgs, AutoUpdateTargetsArgs, DaemonRespawnArgs,
    UpgradeArgs,
};
use crate::daemon;
use crate::desktop;
use crate::error::CliError;
use crate::harness::{
    resolve_claude_command, resolve_codex_command, resolve_omp_command, resolve_opencode_command,
};
use crate::state::{
    AutoUpdateLedger, LiftgateSettings, StatePaths, TargetAutoUpdateRecord, now_unix_ms,
    resolve_effective_agy_bin, resolve_effective_agy_proxy,
};
use crate::upgrade::{self, IdleSelfUpgradeOutcome, SemVer};

const DEFAULT_INITIAL_DELAY_SECS: u64 = 45;
const DEFAULT_DEFER_RETRY_SECS: u64 = 60;
const DEFAULT_EMPTY_POOL_COOLDOWN_MS: u64 = 15_000;
const DEFAULT_WARM_POOL_COOLDOWN_MS: u64 = 5 * 60 * 1_000;
const SETTINGS_WATCH_POLL_INTERVAL: Duration = Duration::from_secs(2);
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(12);
const HARNESS_UPDATE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_CHILD_CAPTURE_BYTES: u64 = 128 * 1024;
const DAEMON_RESPAWN_WAIT_TIMEOUT: Duration = Duration::from_secs(20);

/// Optional command-line overrides supplied to `liftgate serve` (`None` = track `settings.json` live).
#[derive(Clone, Copy, Debug, Default)]
pub struct AutoUpdateCliOverrides {
    pub lg: Option<bool>,
    pub agy: Option<bool>,
}

impl AutoUpdateCliOverrides {
    #[must_use]
    pub const fn effective_enabled(
        self,
        target: AutoUpdateTarget,
        settings: &LiftgateSettings,
    ) -> bool {
        match target {
            AutoUpdateTarget::Liftgate => match self.lg {
                Some(v) => v,
                None => settings.lg_auto_update,
            },
            AutoUpdateTarget::Agy => match self.agy {
                Some(v) => v,
                None => settings.agy_auto_update,
            },
            AutoUpdateTarget::Codex
            | AutoUpdateTarget::Claude
            | AutoUpdateTarget::Opencode
            | AutoUpdateTarget::Omp => settings.is_auto_update_enabled(target),
        }
    }
}

/// Timing and mirror overrides for [`run_idle_fleet_auto_updater_with_options`].
#[derive(Clone, Debug)]
pub struct FleetAutoUpdateOptions {
    pub initial_delay: Duration,
    pub interval_override: Option<Duration>,
    pub defer_retry: Duration,
    pub empty_pool_cooldown_ms: u64,
    pub warm_pool_cooldown_ms: u64,
    pub agy_manifest_base_url: Option<String>,
}

impl FleetAutoUpdateOptions {
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            initial_delay: env_duration_secs(
                "LIFTGATE_AUTO_UPDATE_INITIAL_DELAY_SECS",
                env_u64_opt("LIFTGATE_AGY_UPDATE_INITIAL_DELAY_SECS")
                    .unwrap_or(DEFAULT_INITIAL_DELAY_SECS),
            ),
            interval_override: env_u64_opt("LIFTGATE_AUTO_UPDATE_INTERVAL_SECS")
                .or_else(|| env_u64_opt("LIFTGATE_AGY_UPDATE_INTERVAL_SECS"))
                .map(|secs| Duration::from_secs(secs.max(1))),
            defer_retry: env_duration_secs(
                "LIFTGATE_AUTO_UPDATE_DEFER_SECS",
                env_u64_opt("LIFTGATE_AGY_UPDATE_DEFER_SECS").unwrap_or(DEFAULT_DEFER_RETRY_SECS),
            ),
            empty_pool_cooldown_ms: env_u64_ms(
                "LIFTGATE_AUTO_UPDATE_EMPTY_COOLDOWN_MS",
                env_u64_opt("LIFTGATE_AGY_IDLE_EMPTY_COOLDOWN_MS")
                    .unwrap_or(DEFAULT_EMPTY_POOL_COOLDOWN_MS),
            ),
            warm_pool_cooldown_ms: env_u64_ms(
                "LIFTGATE_AUTO_UPDATE_WARM_COOLDOWN_MS",
                env_u64_opt("LIFTGATE_AGY_IDLE_WARM_COOLDOWN_MS")
                    .unwrap_or(DEFAULT_WARM_POOL_COOLDOWN_MS),
            ),
            agy_manifest_base_url: std::env::var("LIFTGATE_AGY_MANIFEST_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty()),
        }
    }
}

/// Result of evaluating whether the gateway currently has zero active sessions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdleGateEvaluation {
    Idle,
    Busy(String),
}

impl IdleGateEvaluation {
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    #[must_use]
    pub fn busy_reason(&self) -> Option<&str> {
        match self {
            Self::Idle => None,
            Self::Busy(reason) => Some(reason.as_str()),
        }
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

/// Evaluates local client session activity when no live gateway telemetry handle is attached
/// (e.g. standalone `lg auto-update status` while the daemon is offline).
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

/// Structured status report returned by `lg auto-update status` and consumed by the TUI.
#[derive(Clone, Debug, Serialize)]
pub struct AutoUpdateStatusReport {
    pub settings_file: PathBuf,
    pub ledger_file: PathBuf,
    pub daemon_running: bool,
    pub daemon_pid: Option<u32>,
    pub interval_mins: u64,
    pub idle_ready: bool,
    pub busy_reason: Option<String>,
    pub sweep_in_progress: bool,
    pub active_target: Option<AutoUpdateTarget>,
    pub targets: Vec<AutoUpdateTargetTelemetry>,
}

/// Result of running an immediate check or update for a single target.
#[derive(Clone, Debug, Serialize)]
pub struct SingleTargetRunResult {
    pub target: AutoUpdateTarget,
    pub status: AutoUpdateLastOutcome,
    pub previous_version: Option<String>,
    pub current_version: Option<String>,
    pub latest_version: Option<String>,
    pub note: String,
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
    if all_flag
        || raw_targets
            .iter()
            .any(|t| t.trim().eq_ignore_ascii_case("all"))
    {
        return Ok(AutoUpdateTarget::ALL.to_vec());
    }
    let mut out = Vec::with_capacity(raw_targets.len());
    for raw in raw_targets {
        let Some(target) = AutoUpdateTarget::from_slug(raw) else {
            return Err(CliError::UnknownAutoUpdateTarget {
                value: raw.trim().to_owned(),
            });
        };
        if !out.contains(&target) {
            out.push(target);
        }
    }
    Ok(out)
}

/// Applies `lg auto-update enable|disable` to `settings.json`.
pub fn apply_targets_toggle(
    state_paths: &StatePaths,
    args: &AutoUpdateTargetsArgs,
    enabled: bool,
) -> Result<Vec<AutoUpdateTarget>, CliError> {
    let targets = parse_target_selection(&args.targets, args.all)?;
    if targets.is_empty() {
        return Err(CliError::UnknownAutoUpdateTarget {
            value: String::new(),
        });
    }
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

/// Inspects all 6 managed targets, combining live gateway telemetry (when online), `settings.json`,
/// `auto-update-state.json`, and local binary/version probes.
pub async fn inspect_auto_update_status(
    state_paths: &StatePaths,
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

    let probed = probe_all_targets_concurrently(state_paths, &settings).await;
    let targets = build_target_telemetry_rows(
        &settings,
        &ledger,
        &probed,
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
    settings: &LiftgateSettings,
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
            run_harness_target_pass(target, check_only, &stop_rx).await
        }
    };

    let _ = persist_target_result(state_paths, settings, &res, now);
    res
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
    let agy_args = crate::cli::AgyUpdateArgs {
        check: check_only,
        force: false,
        proxy: None,
        no_restart: false,
        manifest_base_url: None,
    };
    match crate::agy_manager::execute_agy_update(state_paths, None, &agy_args).await {
        Ok(out) => {
            use crate::agy_manager::AgyUpdateStatus;
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

// ============================================================================
// Background Idle Fleet Scheduler (runs inside `liftgate serve`)
// ============================================================================

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
    mut stop_rx: watch::Receiver<bool>,
    opts: FleetAutoUpdateOptions,
) {
    let mut cached_settings = state_paths.read_settings().unwrap_or_default();
    let mut cached_ledger = state_paths.read_auto_update_ledger().unwrap_or_default();
    let mut probed = probe_all_targets_concurrently(&state_paths, &cached_settings).await;
    let mut last_settings_mtime = file_mtime(&state_paths.settings_file());
    let mut last_ledger_mtime = file_mtime(&state_paths.auto_update_state_file());

    live_client.set_auto_update_enabled(
        cli_overrides.effective_enabled(AutoUpdateTarget::Agy, &cached_settings),
    );
    publish_live_summary(
        &state_paths,
        &live_client,
        &pool,
        &telemetry,
        &cached_settings,
        &cached_ledger,
        &probed,
        cli_overrides,
        &opts,
        false,
        None,
    );

    let mut next_sweep_at = Instant::now() + opts.initial_delay;
    let mut watch_tick = tokio::time::interval(SETTINGS_WATCH_POLL_INTERVAL);
    watch_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let _ = watch_tick.tick().await;

    loop {
        let now_inst = Instant::now();
        let sleep_dur = next_sweep_at.saturating_duration_since(now_inst);

        tokio::select! {
            biased;
            _ = stop_rx.wait_for(|&stop| stop) => return,
            () = tokio::time::sleep(sleep_dur) => {}
            _ = watch_tick.tick() => {
                let cur_s_mtime = file_mtime(&state_paths.settings_file());
                let cur_l_mtime = file_mtime(&state_paths.auto_update_state_file());
                if cur_s_mtime != last_settings_mtime || cur_l_mtime != last_ledger_mtime {
                    let prev_interval = cached_settings.auto_update_interval_mins;
                    cached_settings = state_paths.read_settings().unwrap_or_default();
                    cached_ledger = state_paths.read_auto_update_ledger().unwrap_or_default();
                    last_settings_mtime = cur_s_mtime;
                    last_ledger_mtime = cur_l_mtime;
                    live_client.set_auto_update_enabled(
                        cli_overrides.effective_enabled(AutoUpdateTarget::Agy, &cached_settings),
                    );
                    if opts.interval_override.is_none()
                        && cached_settings.auto_update_interval_mins != prev_interval
                    {
                        next_sweep_at = Instant::now()
                            + Duration::from_secs(cached_settings.auto_update_interval_mins.as_secs());
                    }
                    publish_live_summary(
                        &state_paths,
                        &live_client,
                        &pool,
                        &telemetry,
                        &cached_settings,
                        &cached_ledger,
                        &probed,
                        cli_overrides,
                        &opts,
                        false,
                        None,
                    );
                }
                continue;
            }
        }

        if *stop_rx.borrow() {
            return;
        }

        cached_settings = state_paths.read_settings().unwrap_or_default();
        cached_ledger = state_paths.read_auto_update_ledger().unwrap_or_default();
        live_client.set_auto_update_enabled(
            cli_overrides.effective_enabled(AutoUpdateTarget::Agy, &cached_settings),
        );

        let any_enabled = AutoUpdateTarget::ALL
            .into_iter()
            .any(|t| cli_overrides.effective_enabled(t, &cached_settings));
        let configured_interval = opts.interval_override.unwrap_or_else(|| {
            Duration::from_secs(cached_settings.auto_update_interval_mins.as_secs())
        });

        if !any_enabled {
            publish_live_summary(
                &state_paths,
                &live_client,
                &pool,
                &telemetry,
                &cached_settings,
                &cached_ledger,
                &probed,
                cli_overrides,
                &opts,
                false,
                None,
            );
            next_sweep_at = Instant::now() + configured_interval;
            continue;
        }

        // Gate ALL periodic detection and updates on zero active sessions.
        let gate = evaluate_idle_gate(
            &state_paths,
            &live_client,
            &pool,
            &telemetry,
            opts.empty_pool_cooldown_ms,
            opts.warm_pool_cooldown_ms,
        );
        if let IdleGateEvaluation::Busy(reason) = gate {
            tracing::debug!(
                reason = %reason,
                "deferring periodic auto-update check because a session is active"
            );
            publish_live_summary(
                &state_paths,
                &live_client,
                &pool,
                &telemetry,
                &cached_settings,
                &cached_ledger,
                &probed,
                cli_overrides,
                &opts,
                false,
                None,
            );
            next_sweep_at = Instant::now() + opts.defer_retry;
            continue;
        }

        let mut deferred_during_sweep = false;
        for target in AutoUpdateTarget::SWEEP_ORDER {
            if *stop_rx.borrow() {
                return;
            }
            cached_settings = state_paths.read_settings().unwrap_or_default();
            if !cli_overrides.effective_enabled(target, &cached_settings) {
                continue;
            }

            let pre_gate = evaluate_idle_gate(
                &state_paths,
                &live_client,
                &pool,
                &telemetry,
                opts.empty_pool_cooldown_ms,
                opts.warm_pool_cooldown_ms,
            );
            if !pre_gate.is_idle() {
                deferred_during_sweep = true;
                break;
            }

            publish_live_summary(
                &state_paths,
                &live_client,
                &pool,
                &telemetry,
                &cached_settings,
                &cached_ledger,
                &probed,
                cli_overrides,
                &opts,
                true,
                Some(target),
            );

            let (outcome, trigger_daemon_respawn) = execute_idle_target_pass(
                &state_paths,
                &cached_settings,
                &live_client,
                &pool,
                &telemetry,
                target,
                &opts,
                &stop_rx,
            )
            .await;

            if matches!(outcome.status, AutoUpdateLastOutcome::DeferredBusy) {
                deferred_during_sweep = true;
            }
            let now = now_unix_ms();
            if let Ok(updated_ledger) =
                persist_target_result(&state_paths, &cached_settings, &outcome, now)
            {
                cached_ledger = updated_ledger;
            }
            if let Some(ver) = &outcome.current_version {
                update_probed_version(&mut probed, target, ver.clone());
            }

            if let Some(new_liftgate_exe) = trigger_daemon_respawn
                && spawn_detached_daemon_respawn(
                    &state_paths,
                    &new_liftgate_exe,
                    std::process::id(),
                )
                .is_ok()
            {
                tracing::info!(
                    exe = %new_liftgate_exe.display(),
                    "handed off self-upgraded daemon respawn; initiating graceful drain"
                );
                let _ = stop_tx.send_replace(true);
                return;
            }
        }

        last_settings_mtime = file_mtime(&state_paths.settings_file());
        last_ledger_mtime = file_mtime(&state_paths.auto_update_state_file());
        publish_live_summary(
            &state_paths,
            &live_client,
            &pool,
            &telemetry,
            &cached_settings,
            &cached_ledger,
            &probed,
            cli_overrides,
            &opts,
            false,
            None,
        );

        next_sweep_at = Instant::now()
            + if deferred_during_sweep {
                opts.defer_retry
            } else {
                configured_interval
            };
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "single-target idle pass shares live ACP, pool, telemetry, and cancellation handles"
)]
async fn execute_idle_target_pass(
    state_paths: &StatePaths,
    settings: &LiftgateSettings,
    live_client: &Arc<LiveAcpClient>,
    pool: &Arc<SessionPool>,
    telemetry: &Arc<TelemetryHub>,
    target: AutoUpdateTarget,
    opts: &FleetAutoUpdateOptions,
    stop_rx: &watch::Receiver<bool>,
) -> (SingleTargetRunResult, Option<PathBuf>) {
    match target {
        AutoUpdateTarget::Liftgate => {
            execute_idle_lg_pass(state_paths, settings, live_client, pool, telemetry, opts).await
        }
        AutoUpdateTarget::Agy => {
            let res = execute_idle_agy_pass(state_paths, live_client, pool, telemetry, opts).await;
            (res, None)
        }
        AutoUpdateTarget::Codex
        | AutoUpdateTarget::Claude
        | AutoUpdateTarget::Opencode
        | AutoUpdateTarget::Omp => {
            let res = run_harness_target_pass(target, false, stop_rx).await;
            (res, None)
        }
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

// ============================================================================
// Harness Resolution, Version Probing & Supervised Non-Interactive Updates
// ============================================================================

fn resolve_harness_for_target(target: AutoUpdateTarget) -> Option<ResolvedCommand> {
    match target {
        AutoUpdateTarget::Codex => resolve_codex_command(None).ok(),
        AutoUpdateTarget::Claude => resolve_claude_command(None).ok(),
        AutoUpdateTarget::Opencode => resolve_opencode_command(None).ok(),
        AutoUpdateTarget::Omp => resolve_omp_command(None).ok(),
        AutoUpdateTarget::Liftgate | AutoUpdateTarget::Agy => None,
    }
}

async fn run_harness_target_pass(
    target: AutoUpdateTarget,
    check_only: bool,
    stop_rx: &watch::Receiver<bool>,
) -> SingleTargetRunResult {
    let Some(resolved) = resolve_harness_for_target(target) else {
        return SingleTargetRunResult {
            target,
            status: AutoUpdateLastOutcome::NotInstalled,
            previous_version: None,
            current_version: None,
            latest_version: None,
            note: "not installed in PATH".to_owned(),
        };
    };

    let before_version = probe_resolved_command_version(&resolved).await;
    if check_only && target != AutoUpdateTarget::Omp {
        let note = match &before_version {
            Some(v) => {
                format!("installed v{v} (run without `--check` to invoke `{target}` updater)")
            }
            None => format!("installed at {}", resolved.program.display()),
        };
        return SingleTargetRunResult {
            target,
            status: AutoUpdateLastOutcome::UpToDate,
            previous_version: before_version.clone(),
            current_version: before_version.clone(),
            latest_version: before_version,
            note,
        };
    }

    let primary_args: &[&str] = match target {
        AutoUpdateTarget::Codex => &["update"],
        AutoUpdateTarget::Claude => &["update"],
        AutoUpdateTarget::Opencode => &["upgrade"],
        AutoUpdateTarget::Omp => {
            if check_only {
                &["update", "--check"]
            } else {
                &["update"]
            }
        }
        AutoUpdateTarget::Liftgate | AutoUpdateTarget::Agy => &[],
    };

    let mut exec_res =
        run_supervised_resolved_command(&resolved, primary_args, HARNESS_UPDATE_TIMEOUT, stop_rx)
            .await;

    if exec_res.is_err()
        && !check_only
        && let Some((fallback_cmd, fallback_args)) =
            detect_package_manager_fallback(target, &resolved)
    {
        let fb_resolved = ResolvedCommand {
            program: fallback_cmd,
            prefix_args: Vec::new(),
        };
        let fb_refs: Vec<&str> = fallback_args.iter().map(String::as_str).collect();
        exec_res = run_supervised_resolved_command(
            &fb_resolved,
            &fb_refs,
            HARNESS_UPDATE_TIMEOUT,
            stop_rx,
        )
        .await;
    }

    match exec_res {
        Ok(output) => {
            let after_version = probe_resolved_command_version(&resolved)
                .await
                .or_else(|| before_version.clone());
            if check_only && target == AutoUpdateTarget::Omp {
                let lower = output.to_ascii_lowercase();
                let has_update = lower.contains("update available")
                    || (lower.contains("available") && !lower.contains("up to date"));
                return SingleTargetRunResult {
                    target,
                    status: if has_update {
                        AutoUpdateLastOutcome::UpdateAvailable
                    } else {
                        AutoUpdateLastOutcome::UpToDate
                    },
                    previous_version: before_version,
                    current_version: after_version.clone(),
                    latest_version: after_version,
                    note: summarize_child_output(&output, "checked"),
                };
            }

            let changed = match (&before_version, &after_version) {
                (Some(b), Some(a)) => b != a,
                _ => false,
            };
            if changed {
                let from_v = before_version
                    .clone()
                    .unwrap_or_else(|| "unknown".to_owned());
                let to_v = after_version
                    .clone()
                    .unwrap_or_else(|| "unknown".to_owned());
                tracing::info!(
                    harness = %target,
                    from = %from_v,
                    to = %to_v,
                    "automatically updated coding harness while gateway was idle"
                );
                SingleTargetRunResult {
                    target,
                    status: AutoUpdateLastOutcome::Updated,
                    previous_version: before_version,
                    current_version: after_version.clone(),
                    latest_version: after_version,
                    note: format!("auto-updated v{from_v} -> v{to_v}"),
                }
            } else {
                SingleTargetRunResult {
                    target,
                    status: AutoUpdateLastOutcome::UpToDate,
                    previous_version: before_version,
                    current_version: after_version.clone(),
                    latest_version: after_version,
                    note: "up to date".to_owned(),
                }
            }
        }
        Err(detail) => {
            tracing::warn!(harness = %target, error = %detail, "harness auto-update command failed");
            SingleTargetRunResult {
                target,
                status: AutoUpdateLastOutcome::Failed,
                previous_version: before_version.clone(),
                current_version: before_version,
                latest_version: None,
                note: detail,
            }
        }
    }
}

fn detect_package_manager_fallback(
    target: AutoUpdateTarget,
    resolved: &ResolvedCommand,
) -> Option<(PathBuf, Vec<String>)> {
    let canon = canonicalize_clean(&resolved.program);
    let path_str = canon.to_string_lossy();
    let is_homebrew = path_str.contains("/homebrew/")
        || path_str.contains("/Cellar/")
        || path_str.contains("/Caskroom/")
        || path_str.contains("/linuxbrew/");

    if is_homebrew && let Some(brew) = find_tool_in_path("brew") {
        let pkg = match target {
            AutoUpdateTarget::Codex => Some("codex"),
            AutoUpdateTarget::Claude => Some("claude-code"),
            AutoUpdateTarget::Opencode => Some("opencode"),
            AutoUpdateTarget::Omp | AutoUpdateTarget::Liftgate | AutoUpdateTarget::Agy => None,
        }?;
        return Some((brew, vec!["upgrade".to_owned(), pkg.to_owned()]));
    }

    if target == AutoUpdateTarget::Codex
        && let Some(npm) = find_tool_in_path("npm")
    {
        return Some((
            npm,
            vec![
                "install".to_owned(),
                "-g".to_owned(),
                "@openai/codex@latest".to_owned(),
            ],
        ));
    }
    None
}

fn find_tool_in_path(name: &str) -> Option<PathBuf> {
    for dir in split_expanded_path_var() {
        #[cfg(windows)]
        {
            for ext in ["cmd", "exe", "bat"] {
                let candidate = dir.join(format!("{name}.{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        #[cfg(not(windows))]
        {
            let candidate = dir.join(name);
            if liftgate_server::platform::is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

async fn run_supervised_resolved_command(
    resolved: &ResolvedCommand,
    extra_args: &[&str],
    max_duration: Duration,
    stop_rx: &watch::Receiver<bool>,
) -> Result<String, String> {
    let mut cmd = Command::new(&resolved.program);
    cmd.args(&resolved.prefix_args)
        .args(extra_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CI", "1")
        .env("NONINTERACTIVE", "1")
        .env("NPM_CONFIG_YES", "true")
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .env("HOMEBREW_NO_INSTALL_CLEANUP", "1")
        .env("NO_COLOR", "1");
    configure_command_group(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn `{}`: {e}", resolved.program.display()))?;
    let _group_guard = ProcessGroupGuard::attach(&child);

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let read_out = async move {
        let mut buf = Vec::new();
        if let Some(out) = stdout_pipe {
            let _ = out
                .take(MAX_CHILD_CAPTURE_BYTES)
                .read_to_end(&mut buf)
                .await;
        }
        buf
    };
    let read_err = async move {
        let mut buf = Vec::new();
        if let Some(err) = stderr_pipe {
            let _ = err
                .take(MAX_CHILD_CAPTURE_BYTES)
                .read_to_end(&mut buf)
                .await;
        }
        buf
    };

    let mut stop_clone = stop_rx.clone();
    let wait_fut = async {
        let (out_bytes, err_bytes, wait_res) = tokio::join!(read_out, read_err, child.wait());
        (out_bytes, err_bytes, wait_res)
    };

    let outcome = tokio::select! {
        biased;
        Ok(_) = stop_clone.wait_for(|&stop| stop) => {
            return Err("aborted due to gateway shutdown".to_owned());
        }
        timed = timeout(max_duration, wait_fut) => timed,
    };

    let Ok((out_bytes, err_bytes, wait_res)) = outcome else {
        return Err(format!("timed out after {}s", max_duration.as_secs()));
    };
    let status = wait_res.map_err(|e| format!("wait failed: {e}"))?;
    let combined = merge_captured_output(&out_bytes, &err_bytes);
    if status.success() {
        Ok(combined)
    } else {
        let detail = summarize_child_output(
            &combined,
            &format!("exited with status {}", status.code().unwrap_or(-1)),
        );
        Err(detail)
    }
}

async fn probe_resolved_command_version(resolved: &ResolvedCommand) -> Option<String> {
    let (_stop_tx, stop_rx) = watch::channel(false);
    let output =
        run_supervised_resolved_command(resolved, &["--version"], VERSION_PROBE_TIMEOUT, &stop_rx)
            .await
            .ok()?;
    extract_clean_version_token(&output)
}

/// Extracts a clean SemVer-like version token (e.g. `0.171.0`, `2.1.326`, `19.2.1`) from `--version` output.
#[must_use]
pub fn extract_clean_version_token(raw: &str) -> Option<String> {
    let first_line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    for token in first_line.split(|c: char| c.is_whitespace() || c == '/' || c == '(' || c == ')') {
        let clean_punct = token.trim_matches(|c: char| c == ',' || c == ';');
        let trimmed = clean_punct
            .strip_prefix('v')
            .or_else(|| clean_punct.strip_prefix('V'))
            .unwrap_or(clean_punct);
        if trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) && trimmed.contains('.') {
            return Some(trimmed.to_owned());
        }
    }
    Some(first_line.chars().take(48).collect())
}

fn merge_captured_output(stdout: &[u8], stderr: &[u8]) -> String {
    let out_s = String::from_utf8_lossy(stdout);
    let err_s = String::from_utf8_lossy(stderr);
    match (out_s.trim().is_empty(), err_s.trim().is_empty()) {
        (false, true) => out_s.trim().to_owned(),
        (true, false) => err_s.trim().to_owned(),
        (false, false) => format!("{}\n{}", out_s.trim(), err_s.trim()),
        (true, true) => String::new(),
    }
}

fn summarize_child_output(raw: &str, fallback: &str) -> String {
    let line = raw
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(fallback);
    line.chars().take(140).collect()
}

// ============================================================================
// Detached Managed Daemon Respawn (`__daemon-respawn`)
// ============================================================================

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

// ============================================================================
// Probing, Persistence & Telemetry Summary Builders
// ============================================================================

#[derive(Clone, Debug, Default)]
struct ProbedTargetInfo {
    binary_path: Option<String>,
    installed_version: Option<String>,
}

async fn probe_all_targets_concurrently(
    state_paths: &StatePaths,
    settings: &LiftgateSettings,
) -> [ProbedTargetInfo; 6] {
    let lg_info = ProbedTargetInfo {
        binary_path: std::env::current_exe()
            .ok()
            .map(|p| canonicalize_clean(&p).display().to_string()),
        installed_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
    };
    let proxy = resolve_effective_agy_proxy(None, settings).ok().flatten();
    let agy_bin = resolve_effective_agy_bin(None, settings);
    let agy_fut = async {
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
    let codex_fut = probe_harness_info(AutoUpdateTarget::Codex);
    let claude_fut = probe_harness_info(AutoUpdateTarget::Claude);
    let opencode_fut = probe_harness_info(AutoUpdateTarget::Opencode);
    let omp_fut = probe_harness_info(AutoUpdateTarget::Omp);
    let _ = state_paths;

    let (agy_i, codex_i, claude_i, opencode_i, omp_i) =
        tokio::join!(agy_fut, codex_fut, claude_fut, opencode_fut, omp_fut);
    [lg_info, agy_i, codex_i, claude_i, opencode_i, omp_i]
}

async fn probe_harness_info(target: AutoUpdateTarget) -> ProbedTargetInfo {
    let Some(resolved) = resolve_harness_for_target(target) else {
        return ProbedTargetInfo::default();
    };
    let ver = probe_resolved_command_version(&resolved).await;
    ProbedTargetInfo {
        binary_path: Some(resolved.program.display().to_string()),
        installed_version: ver,
    }
}

fn target_slot_index(target: AutoUpdateTarget) -> usize {
    match target {
        AutoUpdateTarget::Liftgate => 0,
        AutoUpdateTarget::Agy => 1,
        AutoUpdateTarget::Codex => 2,
        AutoUpdateTarget::Claude => 3,
        AutoUpdateTarget::Opencode => 4,
        AutoUpdateTarget::Omp => 5,
    }
}

fn update_probed_version(
    probed: &mut [ProbedTargetInfo; 6],
    target: AutoUpdateTarget,
    new_version: String,
) {
    if let Some(slot) = probed.get_mut(target_slot_index(target)) {
        slot.installed_version = Some(new_version);
    }
}

fn persist_target_result(
    state_paths: &StatePaths,
    _settings: &LiftgateSettings,
    res: &SingleTargetRunResult,
    now_ms: u64,
) -> Result<AutoUpdateLedger, CliError> {
    let mut ledger = state_paths.read_auto_update_ledger().unwrap_or_default();
    let prev_rec = ledger.targets.get(&res.target).cloned().unwrap_or_default();

    let (last_update_unix_ms, last_updated_from, last_updated_to) =
        if res.status == AutoUpdateLastOutcome::Updated {
            (
                Some(now_ms),
                res.previous_version.clone().or(prev_rec.last_updated_from),
                res.current_version.clone().or(res.latest_version.clone()),
            )
        } else {
            (
                prev_rec.last_update_unix_ms,
                prev_rec.last_updated_from,
                prev_rec.last_updated_to,
            )
        };

    let record = TargetAutoUpdateRecord {
        last_check_unix_ms: Some(now_ms),
        latest_seen_version: res.latest_version.clone().or(prev_rec.latest_seen_version),
        last_update_unix_ms,
        last_updated_from,
        last_updated_to,
        last_status: res.status,
        note: Some(res.note.clone()),
    };
    ledger.targets.insert(res.target, record);
    state_paths.write_auto_update_ledger(&ledger)?;
    Ok(ledger)
}

fn build_target_telemetry_rows(
    settings: &LiftgateSettings,
    ledger: &AutoUpdateLedger,
    probed: &[ProbedTargetInfo; 6],
    cli_overrides: AutoUpdateCliOverrides,
) -> Vec<AutoUpdateTargetTelemetry> {
    AutoUpdateTarget::ALL
        .into_iter()
        .map(|target| {
            let info = probed
                .get(target_slot_index(target))
                .cloned()
                .unwrap_or_default();
            let rec = ledger.targets.get(&target).cloned().unwrap_or_default();
            let status = if info.binary_path.is_none()
                && rec.last_status == AutoUpdateLastOutcome::NeverChecked
            {
                AutoUpdateLastOutcome::NotInstalled
            } else {
                rec.last_status
            };
            AutoUpdateTargetTelemetry {
                target,
                enabled: cli_overrides.effective_enabled(target, settings),
                installed_version: info.installed_version,
                binary_path: info.binary_path,
                latest_seen_version: rec.latest_seen_version,
                last_check_unix_ms: rec.last_check_unix_ms,
                last_update_unix_ms: rec.last_update_unix_ms,
                last_updated_from: rec.last_updated_from,
                last_updated_to: rec.last_updated_to,
                last_status: status,
                note: rec.note,
            }
        })
        .collect()
}

#[expect(
    clippy::too_many_arguments,
    reason = "summary publisher snapshots live idle state alongside settings, ledger, and probed versions"
)]
fn publish_live_summary(
    state_paths: &StatePaths,
    live_client: &LiveAcpClient,
    pool: &SessionPool,
    telemetry: &TelemetryHub,
    settings: &LiftgateSettings,
    ledger: &AutoUpdateLedger,
    probed: &[ProbedTargetInfo; 6],
    cli_overrides: AutoUpdateCliOverrides,
    opts: &FleetAutoUpdateOptions,
    sweep_in_progress: bool,
    active_target: Option<AutoUpdateTarget>,
) {
    let gate = evaluate_idle_gate(
        state_paths,
        live_client,
        pool,
        telemetry,
        opts.empty_pool_cooldown_ms,
        opts.warm_pool_cooldown_ms,
    );
    let summary = AutoUpdateTelemetrySummary {
        interval_mins: settings.auto_update_interval_mins.get(),
        idle_ready: gate.is_idle(),
        busy_reason: gate.busy_reason().map(str::to_owned),
        sweep_in_progress,
        active_target,
        targets: build_target_telemetry_rows(settings, ledger, probed, cli_overrides),
    };
    telemetry.set_auto_update_summary(summary);
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

fn env_u64_opt(key: &str) -> Option<u64> {
    std::env::var_os(OsStr::new(key))
        .and_then(|v| v.to_str().and_then(|s| s.trim().parse::<u64>().ok()))
}

fn env_duration_secs(key: &str, default_secs: u64) -> Duration {
    Duration::from_secs(env_u64_opt(key).unwrap_or(default_secs).max(1))
}

fn env_u64_ms(key: &str, default_ms: u64) -> u64 {
    env_u64_opt(key).unwrap_or(default_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Result<Self, std::io::Error> {
            let path = std::env::temp_dir().join(format!("liftgate-au-test-{}", Uuid::new_v4()));
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
    fn default_settings_enable_lg_and_agy_while_harnesses_remain_disabled() {
        let settings = LiftgateSettings::default();
        assert!(settings.is_auto_update_enabled(AutoUpdateTarget::Liftgate));
        assert!(settings.is_auto_update_enabled(AutoUpdateTarget::Agy));
        assert!(!settings.is_auto_update_enabled(AutoUpdateTarget::Codex));
        assert!(!settings.is_auto_update_enabled(AutoUpdateTarget::Claude));
        assert!(!settings.is_auto_update_enabled(AutoUpdateTarget::Opencode));
        assert!(!settings.is_auto_update_enabled(AutoUpdateTarget::Omp));
        assert_eq!(settings.auto_update_interval_mins.get(), 30);
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
    fn extract_clean_version_token_handles_all_supported_cli_formats() {
        assert_eq!(
            extract_clean_version_token("2.1.326 (Claude Code)").as_deref(),
            Some("2.1.326")
        );
        assert_eq!(
            extract_clean_version_token("codex-cli 0.171.0").as_deref(),
            Some("0.171.0")
        );
        assert_eq!(
            extract_clean_version_token("omp/19.2.1").as_deref(),
            Some("19.2.1")
        );
        assert_eq!(
            extract_clean_version_token("opencode v2.3.1\n").as_deref(),
            Some("2.3.1")
        );
    }

    #[test]
    fn parse_target_selection_resolves_aliases_and_rejects_unknown_names() {
        let parsed =
            parse_target_selection(&["lg".to_owned(), "cx".to_owned(), "omp".to_owned()], false)
                .unwrap();
        assert_eq!(
            parsed,
            vec![
                AutoUpdateTarget::Liftgate,
                AutoUpdateTarget::Codex,
                AutoUpdateTarget::Omp
            ]
        );
        let all = parse_target_selection(&[], true).unwrap();
        assert_eq!(all.len(), 6);
        assert!(parse_target_selection(&["nonexistent".to_owned()], false).is_err());
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

    #[cfg(unix)]
    #[tokio::test]
    async fn supervised_harness_command_captures_version_upgrade_and_up_to_date() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = TempDir::new().unwrap();
        let ver_file = tmp.path().join("ver.txt");
        fs::write(&ver_file, "0.9.1\n").unwrap();
        let mock_bin = tmp.path().join("mock-codex");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  printf 'codex-cli %s\\n' \"$(cat '{}')\"\n  exit 0\nfi\nif [ \"$1\" = \"update\" ]; then\n  printf '0.9.2\\n' > '{}'\n  printf 'Updated mock-codex to 0.9.2\\n'\n  exit 0\nfi\nexit 1\n",
            ver_file.display(),
            ver_file.display()
        );
        fs::write(&mock_bin, script).unwrap();
        let mut perms = fs::metadata(&mock_bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&mock_bin, perms).unwrap();

        let resolved = ResolvedCommand {
            program: mock_bin,
            prefix_args: Vec::new(),
        };
        let (_stop_tx, stop_rx) = watch::channel(false);
        let v1 = probe_resolved_command_version(&resolved).await;
        assert_eq!(v1.as_deref(), Some("0.9.1"));

        let out = run_supervised_resolved_command(
            &resolved,
            &["update"],
            Duration::from_secs(10),
            &stop_rx,
        )
        .await
        .unwrap();
        assert!(out.contains("0.9.2"));

        let v2 = probe_resolved_command_version(&resolved).await;
        assert_eq!(v2.as_deref(), Some("0.9.2"));
    }
}
