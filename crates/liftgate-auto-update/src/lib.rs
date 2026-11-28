//! Idle fleet auto-update scheduler, persistent state ledger, cached version probes,
//! and supervised coding-harness updaters for Liftgate (`lg`, `agy`, `codex`, `claude`,
//! `opencode`, and `omp`).
//!
//! Safety invariants:
//! - Periodic background checks **and** binary updates run strictly when zero sessions are active
//!   as reported by [`FleetUpdateBackend::evaluate_idle_gate`].
//! - `lg` and `agy` auto-updates default to `true`; third-party coding harnesses default to `false`.
//! - All child subprocesses run non-interactively with `Stdio::null()` stdin, bounded capture
//!   buffers, per-command timeouts, and pluggable OS process-group / Win32 Job Object supervision.

#![forbid(unsafe_code)]

pub mod harness_runner;
pub mod ledger;

use std::ffi::OsStr;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use harness_runner::{
    ChildGroupSupervisor, DefaultChildSupervisor, extract_clean_version_token, probe_harness_info,
    probe_resolved_command_version, resolve_harness_for_target, run_harness_target_pass,
    run_supervised_resolved_command,
};
pub use ledger::{
    AutoUpdateLedger, FleetSwitchSnapshot, HarnessAutoUpdateSettings, LedgerError,
    TargetAutoUpdateRecord, build_target_telemetry_rows, persist_target_result_file,
    read_ledger_file, write_ledger_file_atomic,
};
use liftgate_types::{
    config::AutoUpdateTarget,
    telemetry::{AutoUpdateLastOutcome, AutoUpdateTargetTelemetry, AutoUpdateTelemetrySummary},
};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::watch;
use tokio::time::Instant;

const DEFAULT_INITIAL_DELAY_SECS: u64 = 45;
const DEFAULT_DEFER_RETRY_SECS: u64 = 60;
const DEFAULT_EMPTY_POOL_COOLDOWN_MS: u64 = 15_000;
const DEFAULT_WARM_POOL_COOLDOWN_MS: u64 = 5 * 60 * 1_000;
const SETTINGS_WATCH_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Optional command-line overrides supplied to `liftgate serve` (`None` = track `settings.json` live).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutoUpdateCliOverrides {
    pub lg: Option<bool>,
    pub agy: Option<bool>,
}

impl AutoUpdateCliOverrides {
    /// Resolves whether `target` is enabled after applying explicit CLI overrides over `switches`.
    #[must_use]
    pub const fn effective_enabled(
        self,
        target: AutoUpdateTarget,
        switches: FleetSwitchSnapshot,
    ) -> bool {
        match target {
            AutoUpdateTarget::Liftgate => match self.lg {
                Some(v) => v,
                None => switches.lg_auto_update,
            },
            AutoUpdateTarget::Agy => match self.agy {
                Some(v) => v,
                None => switches.agy_auto_update,
            },
            AutoUpdateTarget::Codex
            | AutoUpdateTarget::Claude
            | AutoUpdateTarget::Opencode
            | AutoUpdateTarget::Omp => switches.is_enabled(target),
        }
    }
}

/// Timing and mirror overrides for [`run_idle_fleet_scheduler`].
#[derive(Clone, Debug)]
pub struct FleetAutoUpdateOptions {
    pub initial_delay: Duration,
    pub interval_override: Option<Duration>,
    pub defer_retry: Duration,
    pub empty_pool_cooldown_ms: u64,
    pub warm_pool_cooldown_ms: u64,
    pub agy_manifest_base_url: Option<String>,
}

impl Default for FleetAutoUpdateOptions {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_secs(DEFAULT_INITIAL_DELAY_SECS),
            interval_override: None,
            defer_retry: Duration::from_secs(DEFAULT_DEFER_RETRY_SECS),
            empty_pool_cooldown_ms: DEFAULT_EMPTY_POOL_COOLDOWN_MS,
            warm_pool_cooldown_ms: DEFAULT_WARM_POOL_COOLDOWN_MS,
            agy_manifest_base_url: None,
        }
    }
}

impl FleetAutoUpdateOptions {
    /// Reads optional timing and manifest overrides from environment variables.
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

/// Result of evaluating whether the gateway and client sessions currently have zero active turns.
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

/// Result of running an immediate or scheduled check/update for a single target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SingleTargetRunResult {
    pub target: AutoUpdateTarget,
    pub status: AutoUpdateLastOutcome,
    pub previous_version: Option<String>,
    pub current_version: Option<String>,
    pub latest_version: Option<String>,
    pub note: String,
}

/// Probed executable path and version string for one [`AutoUpdateTarget`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProbedTargetInfo {
    pub binary_path: Option<String>,
    pub installed_version: Option<String>,
}

/// Cached binary version probes reused across TUI refresh ticks to avoid spawning `--version`
/// subprocesses on every UI poll.
#[derive(Clone, Debug, Default)]
pub struct AutoUpdateProbeCache(Option<[ProbedTargetInfo; 6]>);

impl AutoUpdateProbeCache {
    /// Returns the cached probe array, populating it with `probe_fn` when empty or `force_reprobe` is set.
    pub async fn get_or_probe_with<F, Fut>(
        &mut self,
        force_reprobe: bool,
        probe_fn: F,
    ) -> &[ProbedTargetInfo; 6]
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = [ProbedTargetInfo; 6]>,
    {
        if force_reprobe || self.0.is_none() {
            self.0 = Some(probe_fn().await);
        }
        // `self.0` was populated on the branch above whenever `None`.
        match &self.0 {
            Some(arr) => arr,
            None => unreachable_empty_probe_ref(),
        }
    }

    /// Clears cached binary probes so the next inspection re-probes installed versions.
    pub fn invalidate(&mut self) {
        self.0 = None;
    }
}

fn unreachable_empty_probe_ref() -> &'static [ProbedTargetInfo; 6] {
    static EMPTY: [ProbedTargetInfo; 6] = [
        ProbedTargetInfo {
            binary_path: None,
            installed_version: None,
        },
        ProbedTargetInfo {
            binary_path: None,
            installed_version: None,
        },
        ProbedTargetInfo {
            binary_path: None,
            installed_version: None,
        },
        ProbedTargetInfo {
            binary_path: None,
            installed_version: None,
        },
        ProbedTargetInfo {
            binary_path: None,
            installed_version: None,
        },
        ProbedTargetInfo {
            binary_path: None,
            installed_version: None,
        },
    ];
    &EMPTY
}

/// Error returned when parsing user-supplied target names in `lg auto-update`.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum TargetSelectionError {
    #[error("specify at least one target (lg, agy, codex, claude, opencode, omp) or pass `--all`")]
    EmptySelection,

    #[error(
        "unknown auto-update target `{name}` (expected: lg, agy, codex, claude, opencode, omp, or --all)"
    )]
    UnknownTarget { name: String },
}

/// Parses target names and `--all` into a deduplicated ordered target list.
pub fn parse_target_selection(
    raw_targets: &[String],
    all: bool,
) -> Result<Vec<AutoUpdateTarget>, TargetSelectionError> {
    if all {
        return Ok(AutoUpdateTarget::ALL.to_vec());
    }
    if raw_targets.is_empty() {
        return Err(TargetSelectionError::EmptySelection);
    }
    let mut out = Vec::with_capacity(raw_targets.len());
    for token in raw_targets {
        for part in token.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if part.eq_ignore_ascii_case("all") {
                return Ok(AutoUpdateTarget::ALL.to_vec());
            }
            let parsed = AutoUpdateTarget::from_slug(part).ok_or_else(|| {
                TargetSelectionError::UnknownTarget {
                    name: part.to_owned(),
                }
            })?;
            if !out.contains(&parsed) {
                out.push(parsed);
            }
        }
    }
    if out.is_empty() {
        return Err(TargetSelectionError::EmptySelection);
    }
    Ok(out)
}

/// Fixed index in the 6-element probe cache corresponding to `target`.
#[must_use]
pub const fn target_slot_index(target: AutoUpdateTarget) -> usize {
    match target {
        AutoUpdateTarget::Liftgate => 0,
        AutoUpdateTarget::Agy => 1,
        AutoUpdateTarget::Codex => 2,
        AutoUpdateTarget::Claude => 3,
        AutoUpdateTarget::Opencode => 4,
        AutoUpdateTarget::Omp => 5,
    }
}

/// Updates the cached installed version for `target` after a successful upgrade.
pub fn update_probed_version(
    probed: &mut [ProbedTargetInfo; 6],
    target: AutoUpdateTarget,
    new_version: String,
) {
    if let Some(slot) = probed.get_mut(target_slot_index(target)) {
        slot.installed_version = Some(new_version);
    }
}

/// Probes `lg`, `agy`, and all 4 coding harnesses concurrently.
pub async fn probe_all_targets_concurrently<F>(
    lg_info: ProbedTargetInfo,
    agy_fut: F,
    supervisor: &dyn ChildGroupSupervisor,
) -> [ProbedTargetInfo; 6]
where
    F: Future<Output = ProbedTargetInfo>,
{
    let codex_fut = probe_harness_info(AutoUpdateTarget::Codex, supervisor);
    let claude_fut = probe_harness_info(AutoUpdateTarget::Claude, supervisor);
    let opencode_fut = probe_harness_info(AutoUpdateTarget::Opencode, supervisor);
    let omp_fut = probe_harness_info(AutoUpdateTarget::Omp, supervisor);

    let (agy_i, codex_i, claude_i, opencode_i, omp_i) =
        tokio::join!(agy_fut, codex_fut, claude_fut, opencode_fut, omp_fut);
    [lg_info, agy_i, codex_i, claude_i, opencode_i, omp_i]
}

/// Host integration boundary implemented by `liftgate-core` so [`run_idle_fleet_scheduler`] stays
/// decoupled from HTTP server, ACP session pool, and self-upgrade archive internals.
pub trait FleetUpdateBackend: Send + Sync {
    /// Path to `settings.json` watched for live toggle and interval edits.
    fn settings_file(&self) -> &Path;

    /// Path to `auto-update-state.json` watched and updated after each target check/apply.
    fn ledger_file(&self) -> &Path;

    /// Reads the latest auto-update switches and interval from disk.
    fn read_switches(&self) -> FleetSwitchSnapshot;

    /// Synchronizes the `agy` auto-update telemetry flag on the live ACP process handle.
    fn sync_agy_auto_update_flag(&self, enabled: bool);

    /// Evaluates whether **zero** sessions are active across HTTP turns, ACP streams,
    /// `SessionPool` leases, CLI harness wrappers, desktop GUI sessions, and quiet cooldown.
    fn evaluate_idle_gate(&self, opts: &FleetAutoUpdateOptions) -> IdleGateEvaluation;

    /// Publishes the latest [`AutoUpdateTelemetrySummary`] to the live SSE/HTTP telemetry hub.
    fn publish_summary(&self, summary: AutoUpdateTelemetrySummary);

    /// OS process-group / Win32 Job Object supervisor for third-party harness subprocesses.
    fn child_supervisor(&self) -> &dyn ChildGroupSupervisor;

    /// Probes `lg` and `agy` binary paths and installed versions.
    fn probe_lg_and_agy(
        &self,
        switches: FleetSwitchSnapshot,
    ) -> impl Future<Output = (ProbedTargetInfo, ProbedTargetInfo)> + Send;

    /// Runs an idle check-and-install pass for `lg` (`liftgate`), returning an optional path to
    /// the newly installed `liftgate` executable when a managed daemon respawn is required.
    fn run_idle_lg_pass(
        &self,
        opts: &FleetAutoUpdateOptions,
        stop_rx: &watch::Receiver<bool>,
    ) -> impl Future<Output = (SingleTargetRunResult, Option<PathBuf>)> + Send;

    /// Runs an idle check-and-hot-swap pass for `agy`.
    fn run_idle_agy_pass(
        &self,
        opts: &FleetAutoUpdateOptions,
    ) -> impl Future<Output = SingleTargetRunResult> + Send;

    /// Spawns `__daemon-respawn` on `new_liftgate_exe` and signals `stop_tx` so the retiring
    /// daemon shuts down cleanly.
    fn handoff_self_upgrade(
        &self,
        new_liftgate_exe: &Path,
        stop_tx: &watch::Sender<bool>,
    ) -> Result<(), String>;
}

/// Runs the periodic idle auto-update coordinator loop until `stop_rx` signals shutdown or a
/// managed `lg` self-upgrade hands off to `__daemon-respawn`.
pub async fn run_idle_fleet_scheduler<B: FleetUpdateBackend>(
    backend: &B,
    cli_overrides: AutoUpdateCliOverrides,
    stop_tx: watch::Sender<bool>,
    mut stop_rx: watch::Receiver<bool>,
    opts: FleetAutoUpdateOptions,
) {
    let mut switches = backend.read_switches();
    let mut cached_ledger = read_ledger_file(backend.ledger_file()).unwrap_or_default();
    let (lg_info, agy_info) = backend.probe_lg_and_agy(switches).await;
    let mut probed =
        probe_all_targets_concurrently(lg_info, async { agy_info }, backend.child_supervisor())
            .await;
    let mut last_settings_mtime = file_mtime(backend.settings_file());
    let mut last_ledger_mtime = file_mtime(backend.ledger_file());

    backend.sync_agy_auto_update_flag(
        cli_overrides.effective_enabled(AutoUpdateTarget::Agy, switches),
    );
    publish_backend_summary(
        backend,
        switches,
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
        tokio::select! {
            biased;
            Ok(_) = stop_rx.wait_for(|&stop| stop) => {
                return;
            }
            () = tokio::time::sleep_until(next_sweep_at) => {}
            _ = watch_tick.tick() => {
                let cur_settings_mtime = file_mtime(backend.settings_file());
                let cur_ledger_mtime = file_mtime(backend.ledger_file());
                if cur_settings_mtime != last_settings_mtime || cur_ledger_mtime != last_ledger_mtime {
                    last_settings_mtime = cur_settings_mtime;
                    last_ledger_mtime = cur_ledger_mtime;
                    let prev_interval = switches.interval_mins;
                    switches = backend.read_switches();
                    cached_ledger = read_ledger_file(backend.ledger_file()).unwrap_or_default();
                    backend.sync_agy_auto_update_flag(
                        cli_overrides.effective_enabled(AutoUpdateTarget::Agy, switches),
                    );
                    if opts.interval_override.is_none() && switches.interval_mins != prev_interval {
                        next_sweep_at =
                            Instant::now() + Duration::from_secs(switches.interval_mins.as_secs());
                    }
                    publish_backend_summary(
                        backend,
                        switches,
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

        switches = backend.read_switches();
        cached_ledger = read_ledger_file(backend.ledger_file()).unwrap_or_default();
        backend.sync_agy_auto_update_flag(
            cli_overrides.effective_enabled(AutoUpdateTarget::Agy, switches),
        );

        let any_enabled = AutoUpdateTarget::ALL
            .into_iter()
            .any(|t| cli_overrides.effective_enabled(t, switches));
        let configured_interval = opts
            .interval_override
            .unwrap_or_else(|| Duration::from_secs(switches.interval_mins.as_secs()));

        if !any_enabled {
            publish_backend_summary(
                backend,
                switches,
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
        let gate = backend.evaluate_idle_gate(&opts);
        if let IdleGateEvaluation::Busy(reason) = gate {
            tracing::debug!(
                reason = %reason,
                "deferring periodic auto-update check because a session is active"
            );
            publish_backend_summary(
                backend,
                switches,
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
            switches = backend.read_switches();
            if !cli_overrides.effective_enabled(target, switches) {
                continue;
            }

            let pre_gate = backend.evaluate_idle_gate(&opts);
            if !pre_gate.is_idle() {
                deferred_during_sweep = true;
                break;
            }

            publish_backend_summary(
                backend,
                switches,
                &cached_ledger,
                &probed,
                cli_overrides,
                &opts,
                true,
                Some(target),
            );

            let now_ms = current_unix_ms();
            let (result, respawn_exe) = match target {
                AutoUpdateTarget::Liftgate => backend.run_idle_lg_pass(&opts, &stop_rx).await,
                AutoUpdateTarget::Agy => (backend.run_idle_agy_pass(&opts).await, None),
                AutoUpdateTarget::Codex
                | AutoUpdateTarget::Claude
                | AutoUpdateTarget::Opencode
                | AutoUpdateTarget::Omp => (
                    run_harness_target_pass(target, false, backend.child_supervisor(), &stop_rx)
                        .await,
                    None,
                ),
            };

            if result.status == AutoUpdateLastOutcome::Updated
                && let Some(new_ver) = &result.current_version
            {
                update_probed_version(&mut probed, target, new_ver.clone());
            }
            if let Ok(updated_ledger) =
                persist_target_result_file(backend.ledger_file(), &result, now_ms)
            {
                cached_ledger = updated_ledger;
                last_ledger_mtime = file_mtime(backend.ledger_file());
            }
            if result.status == AutoUpdateLastOutcome::DeferredBusy {
                deferred_during_sweep = true;
            }

            if let Some(new_liftgate_exe) = respawn_exe {
                publish_backend_summary(
                    backend,
                    switches,
                    &cached_ledger,
                    &probed,
                    cli_overrides,
                    &opts,
                    false,
                    None,
                );
                match backend.handoff_self_upgrade(&new_liftgate_exe, &stop_tx) {
                    Ok(()) => return,
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            "failed to spawn detached daemon respawn helper after self-upgrade"
                        );
                    }
                }
            }
        }

        publish_backend_summary(
            backend,
            switches,
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
    reason = "summary publisher combines live idle gate state with settings, ledger, and probed versions"
)]
fn publish_backend_summary<B: FleetUpdateBackend>(
    backend: &B,
    switches: FleetSwitchSnapshot,
    ledger: &AutoUpdateLedger,
    probed: &[ProbedTargetInfo; 6],
    cli_overrides: AutoUpdateCliOverrides,
    opts: &FleetAutoUpdateOptions,
    sweep_in_progress: bool,
    active_target: Option<AutoUpdateTarget>,
) {
    let gate = backend.evaluate_idle_gate(opts);
    let summary = AutoUpdateTelemetrySummary {
        interval_mins: switches.interval_mins.get(),
        idle_ready: gate.is_idle(),
        busy_reason: gate.busy_reason().map(str::to_owned),
        sweep_in_progress,
        active_target,
        targets: build_target_telemetry_rows(switches, ledger, probed, cli_overrides),
    };
    backend.publish_summary(summary);
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
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
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use uuid::Uuid;

    use super::*;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Result<Self, std::io::Error> {
            let path = std::env::temp_dir().join(format!("liftgate-au-crate-{}", Uuid::new_v4()));
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
    fn default_switches_enable_lg_and_agy_while_harnesses_remain_disabled() {
        let switches = FleetSwitchSnapshot::default();
        assert!(switches.is_enabled(AutoUpdateTarget::Liftgate));
        assert!(switches.is_enabled(AutoUpdateTarget::Agy));
        assert!(!switches.is_enabled(AutoUpdateTarget::Codex));
        assert!(!switches.is_enabled(AutoUpdateTarget::Claude));
        assert!(!switches.is_enabled(AutoUpdateTarget::Opencode));
        assert!(!switches.is_enabled(AutoUpdateTarget::Omp));
        assert_eq!(switches.interval_mins.get(), 30);
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
        assert_eq!(
            parse_target_selection(&[], false),
            Err(TargetSelectionError::EmptySelection)
        );
    }

    #[test]
    fn ledger_roundtrip_persists_updated_versions_and_notes() {
        let tmp = TempDir::new().unwrap();
        let ledger_path = tmp.path().join("auto-update-state.json");
        let res = SingleTargetRunResult {
            target: AutoUpdateTarget::Liftgate,
            status: AutoUpdateLastOutcome::Updated,
            previous_version: Some("0.2.22".to_owned()),
            current_version: Some("0.2.23".to_owned()),
            latest_version: Some("0.2.23".to_owned()),
            note: "auto-updated v0.2.22 -> v0.2.23".to_owned(),
        };
        let written = persist_target_result_file(&ledger_path, &res, 1_700_000_000_000).unwrap();
        assert_eq!(
            written
                .targets
                .get(&AutoUpdateTarget::Liftgate)
                .and_then(|r| r.last_updated_to.as_deref()),
            Some("0.2.23")
        );
        let reloaded = read_ledger_file(&ledger_path).unwrap();
        assert_eq!(reloaded.targets, written.targets);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supervised_harness_command_captures_version_upgrade() {
        use liftgate_harness::ResolvedCommand;
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
        let sup = DefaultChildSupervisor;
        let (_stop_tx, stop_rx) = watch::channel(false);
        let v1 = probe_resolved_command_version(&resolved, &sup).await;
        assert_eq!(v1.as_deref(), Some("0.9.1"));

        let out = run_supervised_resolved_command(
            &resolved,
            &["update"],
            Duration::from_secs(10),
            &sup,
            &stop_rx,
        )
        .await
        .unwrap();
        assert!(out.contains("0.9.2"));

        let v2 = probe_resolved_command_version(&resolved, &sup).await;
        assert_eq!(v2.as_deref(), Some("0.9.2"));
    }

    struct MockBackend {
        settings_path: PathBuf,
        ledger_path: PathBuf,
        busy: AtomicBool,
        lg_checks: AtomicUsize,
        agy_checks: AtomicUsize,
        handoffs: AtomicUsize,
        summaries: Mutex<Vec<AutoUpdateTelemetrySummary>>,
    }

    impl FleetUpdateBackend for MockBackend {
        fn settings_file(&self) -> &Path {
            &self.settings_path
        }

        fn ledger_file(&self) -> &Path {
            &self.ledger_path
        }

        fn read_switches(&self) -> FleetSwitchSnapshot {
            FleetSwitchSnapshot::default()
        }

        fn sync_agy_auto_update_flag(&self, _enabled: bool) {}

        fn evaluate_idle_gate(&self, _opts: &FleetAutoUpdateOptions) -> IdleGateEvaluation {
            if self.busy.load(Ordering::SeqCst) {
                IdleGateEvaluation::Busy("1 active HTTP turn(s) in flight".to_owned())
            } else {
                IdleGateEvaluation::Idle
            }
        }

        fn publish_summary(&self, summary: AutoUpdateTelemetrySummary) {
            if let Ok(mut guard) = self.summaries.lock() {
                guard.push(summary);
            }
        }

        fn child_supervisor(&self) -> &dyn ChildGroupSupervisor {
            &DefaultChildSupervisor
        }

        async fn probe_lg_and_agy(
            &self,
            _switches: FleetSwitchSnapshot,
        ) -> (ProbedTargetInfo, ProbedTargetInfo) {
            (
                ProbedTargetInfo {
                    binary_path: Some("/usr/local/bin/lg".to_owned()),
                    installed_version: Some("0.2.22".to_owned()),
                },
                ProbedTargetInfo {
                    binary_path: Some("/usr/local/bin/agy".to_owned()),
                    installed_version: Some("1.0.0".to_owned()),
                },
            )
        }

        async fn run_idle_lg_pass(
            &self,
            _opts: &FleetAutoUpdateOptions,
            _stop_rx: &watch::Receiver<bool>,
        ) -> (SingleTargetRunResult, Option<PathBuf>) {
            self.lg_checks.fetch_add(1, Ordering::SeqCst);
            (
                SingleTargetRunResult {
                    target: AutoUpdateTarget::Liftgate,
                    status: AutoUpdateLastOutcome::Updated,
                    previous_version: Some("0.2.22".to_owned()),
                    current_version: Some("0.2.23".to_owned()),
                    latest_version: Some("0.2.23".to_owned()),
                    note: "auto-updated v0.2.22 -> v0.2.23 (daemon respawned)".to_owned(),
                },
                Some(PathBuf::from("/usr/local/bin/liftgate")),
            )
        }

        async fn run_idle_agy_pass(&self, _opts: &FleetAutoUpdateOptions) -> SingleTargetRunResult {
            self.agy_checks.fetch_add(1, Ordering::SeqCst);
            SingleTargetRunResult {
                target: AutoUpdateTarget::Agy,
                status: AutoUpdateLastOutcome::UpToDate,
                previous_version: Some("1.0.0".to_owned()),
                current_version: Some("1.0.0".to_owned()),
                latest_version: Some("1.0.0".to_owned()),
                note: "up to date".to_owned(),
            }
        }

        fn handoff_self_upgrade(
            &self,
            _new_liftgate_exe: &Path,
            stop_tx: &watch::Sender<bool>,
        ) -> Result<(), String> {
            self.handoffs.fetch_add(1, Ordering::SeqCst);
            let _ = stop_tx.send(true);
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn scheduler_strictly_skips_checks_while_busy_then_sweeps_and_respawns_when_idle() {
        let tmp = TempDir::new().unwrap();
        let backend = MockBackend {
            settings_path: tmp.path().join("settings.json"),
            ledger_path: tmp.path().join("auto-update-state.json"),
            busy: AtomicBool::new(true),
            lg_checks: AtomicUsize::new(0),
            agy_checks: AtomicUsize::new(0),
            handoffs: AtomicUsize::new(0),
            summaries: Mutex::new(Vec::new()),
        };
        let opts = FleetAutoUpdateOptions {
            initial_delay: Duration::from_secs(10),
            interval_override: Some(Duration::from_secs(30)),
            defer_retry: Duration::from_secs(15),
            empty_pool_cooldown_ms: 0,
            warm_pool_cooldown_ms: 0,
            agy_manifest_base_url: None,
        };
        let (stop_tx, stop_rx) = watch::channel(false);

        let sched = run_idle_fleet_scheduler(
            &backend,
            AutoUpdateCliOverrides::default(),
            stop_tx,
            stop_rx,
            opts,
        );
        tokio::pin!(sched);

        // Advance past initial_delay while session is active: zero checks may run!
        let _ = tokio::time::timeout(Duration::from_secs(12), &mut sched).await;
        assert_eq!(backend.agy_checks.load(Ordering::SeqCst), 0);
        assert_eq!(backend.lg_checks.load(Ordering::SeqCst), 0);
        assert_eq!(backend.handoffs.load(Ordering::SeqCst), 0);

        // Clear active session and advance past defer_retry: agy checks, then lg updates & hands off.
        backend.busy.store(false, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(20), &mut sched)
            .await
            .expect("scheduler exits cleanly after self-upgrade handoff");

        assert_eq!(backend.agy_checks.load(Ordering::SeqCst), 1);
        assert_eq!(backend.lg_checks.load(Ordering::SeqCst), 1);
        assert_eq!(backend.handoffs.load(Ordering::SeqCst), 1);

        let ledger = read_ledger_file(&backend.ledger_path).unwrap();
        assert_eq!(
            ledger
                .targets
                .get(&AutoUpdateTarget::Liftgate)
                .map(|r| r.last_status),
            Some(AutoUpdateLastOutcome::Updated)
        );
    }
}
