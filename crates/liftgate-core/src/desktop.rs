//! Cross-platform zero-pollution launchers for **Codex App** and **Claude Desktop**.
//!
//! # Architecture
//!
//! - **Codex App** (`Codex.app` / `OpenAI.Codex` MSIX / `codex-desktop`):
//!   - Synthesizes an isolated physical `CODEX_HOME` under `~/.liftgate/desktop/sessions/`
//!     containing `config.toml` (non-destructively merged from `~/.codex/config.toml` read-only),
//!     `agy-models.json`, and `auth.json` (`0o600`), never mutating `~/.codex`.
//!   - Launches with `--user-data-dir` scoped under `~/.liftgate/desktop/codex-app` (or a
//!     disposable session directory with `--ephemeral`) so native `%APPDATA%\Codex`,
//!     `~/Library/Application Support/Codex`, and `~/.config/Codex` remain untouched.
//!   - Never alters `HOME`, `USERPROFILE`, `APPDATA`, or `LOCALAPPDATA`, keeping child developer
//!     shells (`git`, `cargo`, `ssh`, `npm`, `gh`) working identically to native terminals.
//!
//! - **Claude Desktop** (`Claude.app` / `Claude` MSIX & Squirrel / `claude-desktop`):
//!   - Uses Claude Desktop's official Third-Party Inference (`deploymentMode: "3p"`) profile
//!     library (`Claude-3p/configLibrary/<LIFTGATE_PROFILE_ID>.json`) pointed at Liftgate's
//!     `/claude-desktop` endpoint, encoding live Antigravity ACP models into reversible
//!     Claude-safe route IDs (`claude-{opus|sonnet|haiku}-5-5-lg--<acp_model_id>`).
//!   - Records an atomic rollback journal under `~/.liftgate/desktop-journals/` *before*
//!     touching any file, then monitors the GUI process lifetime (via a detached
//!     `__desktop-watch` reaper by default or synchronously under `--wait`) and surgically
//!     restores original `deploymentMode`, `_meta.json`, and newly created files on exit,
//!     preserving any `mcpServers` or preferences edited during the session.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use liftgate_server::platform::{
    canonicalize_clean, is_executable_file, merge_loopback_no_proxy, split_expanded_path_var,
};
use liftgate_types::acp::AvailableModel;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::cli::{
    ClaudeDesktopArgs, CodexAppArgs, DesktopWatchArgs, PLACEHOLDER_API_KEY,
    normalize_optional_api_key,
};
use crate::client::{DiscoveredModel, infer_live_long_context};
use crate::config_sync::{
    build_offline_codex_catalog_bytes_with_mode, merge_codex_config_toml_with_mode,
};
use crate::daemon::{configure_detached_command, ensure_gateway_ready, is_pid_alive};
use crate::error::CliError;
use crate::harness::resolve_codex_command;
use crate::state::{StatePaths, now_unix_ms, user_home_dir, write_file_atomic};

/// Structured launch or session-restore outcome returned by desktop app launchers.
#[derive(Clone, Debug, Serialize)]
pub struct DesktopLaunchOutcome {
    pub kind: DesktopKind,
    pub headline: String,
    pub rows: Vec<(&'static str, String)>,
    pub restored_sessions: Option<usize>,
    pub waited_foreground: bool,
    pub watcher_fallback_warn: bool,
}

/// Discovery and running-process snapshot for a single desktop application.
#[derive(Clone, Debug, Serialize)]
pub struct DesktopAppInfo {
    pub installed: bool,
    pub display_label: Option<String>,
    pub running_pids: Vec<u32>,
}

/// Aggregated status of Codex App, Claude Desktop, and active Liftgate rollback journals.
#[derive(Clone, Debug, Serialize)]
pub struct DesktopAppsStatus {
    pub codex_app: DesktopAppInfo,
    pub claude_desktop: DesktopAppInfo,
    pub active_journals: usize,
}

/// Inspects installed paths, running PIDs, and active Liftgate session journals for desktop apps.
#[must_use]
pub fn inspect_desktop_apps(state_paths: &StatePaths) -> DesktopAppsStatus {
    let codex_bin = resolve_codex_app(None).ok();
    let claude_bin = resolve_claude_desktop(None).ok();
    let active_journals = fs::read_dir(state_paths.desktop_journals_dir())
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
                .count()
        })
        .unwrap_or(0);
    DesktopAppsStatus {
        codex_app: DesktopAppInfo {
            installed: codex_bin.is_some(),
            display_label: codex_bin.map(|b| b.display_label().to_owned()),
            running_pids: find_running_desktop_pids(DesktopKind::CodexApp, None),
        },
        claude_desktop: DesktopAppInfo {
            installed: claude_bin.is_some(),
            display_label: claude_bin.map(|b| b.display_label().to_owned()),
            running_pids: find_running_desktop_pids(DesktopKind::ClaudeDesktop, None),
        },
        active_journals,
    }
}

/// Deterministic UUID v4 reserved for Liftgate's temporary Claude Desktop 3P profile.
pub const LIFTGATE_CLAUDE_3P_PROFILE_ID: &str = "00000000-0000-4000-8000-000000878700";
/// Human-readable profile name shown in Claude Desktop while the Liftgate session is active.
pub const LIFTGATE_CLAUDE_3P_PROFILE_NAME: &str = "Liftgate (Antigravity ACP)";

const CLAUDE_DESKTOP_CONFIG_FILENAME: &str = "claude_desktop_config.json";
const CLAUDE_CONFIG_LIBRARY_DIR: &str = "configLibrary";
const CLAUDE_META_FILENAME: &str = "_meta.json";
const MAX_DESKTOP_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_USER_DOC_COPY_BYTES: u64 = 1024 * 1024;
const MAX_INFERENCE_MODELS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DesktopKind {
    CodexApp,
    ClaudeDesktop,
}

impl DesktopKind {
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::CodexApp => "Codex App",
            Self::ClaudeDesktop => "Claude Desktop",
        }
    }
}

/// Platform-specific launch descriptor for a discovered desktop GUI application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DesktopAppBinary {
    Direct {
        executable: PathBuf,
        display_label: String,
    },
    #[cfg(windows)]
    WindowsMsix {
        package_family_name: String,
        app_id: String,
        executable_rel: String,
        executable_full: PathBuf,
        display_label: String,
    },
}

impl DesktopAppBinary {
    #[must_use]
    pub fn display_label(&self) -> &str {
        match self {
            Self::Direct { display_label, .. } => display_label,
            #[cfg(windows)]
            Self::WindowsMsix { display_label, .. } => display_label,
        }
    }
}

/// Resolved configuration paths for one Claude Desktop root (`Claude` + `Claude-3p`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeDesktopPathSet {
    pub normal_dir: PathBuf,
    pub threep_dir: PathBuf,
    pub normal_config_path: PathBuf,
    pub threep_config_path: PathBuf,
    pub config_library_dir: PathBuf,
    pub meta_path: PathBuf,
    pub profile_path: PathBuf,
}

impl ClaudeDesktopPathSet {
    #[must_use]
    pub fn from_dirs(normal_dir: PathBuf, threep_dir: PathBuf) -> Self {
        let config_library_dir = threep_dir.join(CLAUDE_CONFIG_LIBRARY_DIR);
        let meta_path = config_library_dir.join(CLAUDE_META_FILENAME);
        let profile_path = config_library_dir.join(format!("{LIFTGATE_CLAUDE_3P_PROFILE_ID}.json"));
        Self {
            normal_config_path: normal_dir.join(CLAUDE_DESKTOP_CONFIG_FILENAME),
            threep_config_path: threep_dir.join(CLAUDE_DESKTOP_CONFIG_FILENAME),
            normal_dir,
            threep_dir,
            config_library_dir,
            meta_path,
            profile_path,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ClaudePathSetBackup {
    paths: ClaudeDesktopPathSet,
    normal_dir_existed: bool,
    threep_dir_existed: bool,
    config_library_existed: bool,
    normal_config_existed: bool,
    prior_normal_deployment_mode: Option<Value>,
    threep_config_existed: bool,
    prior_threep_deployment_mode: Option<Value>,
    meta_existed: bool,
    prior_applied_id: Option<Value>,
    had_prior_liftgate_entry: bool,
    profile_existed: bool,
    prior_profile_bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "session_type", rename_all = "kebab-case")]
enum DesktopSessionPayload {
    CodexApp {
        session_dir: PathBuf,
        user_data_dir: PathBuf,
        ephemeral_user_data: bool,
    },
    ClaudeDesktop {
        backups: Vec<ClaudePathSetBackup>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DesktopSessionJournal {
    session_id: String,
    kind: DesktopKind,
    created_at_unix_ms: u64,
    launcher_pid: u32,
    #[serde(default)]
    watcher_pid: Option<u32>,
    #[serde(default)]
    tracked_pids: Vec<u32>,
    #[serde(default)]
    explicit_bin: Option<PathBuf>,
    payload: DesktopSessionPayload,
}

struct ForegroundJournalGuard {
    journal_path: PathBuf,
    journal: Option<DesktopSessionJournal>,
    armed: bool,
}

impl ForegroundJournalGuard {
    fn new(journal_path: PathBuf, journal: DesktopSessionJournal) -> Self {
        Self {
            journal_path,
            journal: Some(journal),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn finish(&mut self) {
        if self.armed {
            self.armed = false;
            if let Some(journal) = self.journal.take() {
                cleanup_session_journal(&self.journal_path, &journal);
            }
        }
    }
}

impl Drop for ForegroundJournalGuard {
    fn drop(&mut self) {
        if self.armed
            && let Some(journal) = self.journal.take()
        {
            cleanup_session_journal(&self.journal_path, &journal);
        }
    }
}

/// Replays and cleans up any orphaned desktop session journals left behind by crashed processes
/// or power loss once both the watcher and the target GUI application are no longer running.
pub fn sweep_stale_desktop_sessions(state_paths: &StatePaths) {
    let dir = state_paths.desktop_journals_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(journal) = read_journal_file(&path) else {
            continue;
        };
        let watcher_alive = journal.watcher_pid.is_some_and(is_pid_alive);
        let launcher_alive = is_pid_alive(journal.launcher_pid);
        if watcher_alive || launcher_alive {
            continue;
        }
        if is_journal_session_still_running(&journal) {
            continue;
        }
        cleanup_session_journal(&path, &journal);
    }
}

/// Launches **Codex App** routed through Liftgate with an isolated ephemeral `CODEX_HOME`.
pub async fn launch_codex_app(
    state_paths: &StatePaths,
    args: CodexAppArgs,
) -> Result<DesktopLaunchOutcome, CliError> {
    sweep_stale_desktop_sessions(state_paths);

    let app_bin = resolve_codex_app(args.codex_app_bin.as_deref())?;
    let explicit_codex_cli = match args.codex_bin.as_deref() {
        Some(explicit) => Some(resolve_codex_command(Some(explicit))?.program),
        None => None,
    };

    if args.restart_app {
        stop_running_desktop_app(DesktopKind::CodexApp, args.codex_app_bin.as_deref()).await;
    }

    let target = ensure_gateway_ready(
        state_paths,
        args.listen,
        args.api_key.clone(),
        args.model.as_deref(),
        true,
    )
    .await?;

    let primary_model = args
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&target.default_model)
        .to_owned();
    let normalized_key = normalize_optional_api_key(target.api_key.as_deref());
    let require_env_key = normalized_key.is_some();
    let auth_token = normalized_key.unwrap_or_else(|| PLACEHOLDER_API_KEY.to_owned());

    let tool_mode = if args.no_code_mode {
        "direct"
    } else if args.code_mode_only {
        "code_mode_only"
    } else {
        "code_mode"
    };

    let catalog_bytes = match target
        .client
        .fetch_codex_catalog_json_with_mode(&primary_model, Some(tool_mode))
        .await
    {
        Ok(bytes) => bytes,
        Err(_) => {
            let discovered = target.client.fetch_models().await.unwrap_or_default();
            let long_context = infer_live_long_context(&discovered, target.managed_long_context);
            build_offline_codex_catalog_bytes_with_mode(
                &discovered,
                &primary_model,
                long_context,
                Some(tool_mode),
            )?
        }
    };

    let session_id = Uuid::new_v4().simple().to_string();
    let sessions_root = state_paths.ensure_desktop_sessions_dir()?;
    let session_dir = sessions_root.join(format!("codex-app-{session_id}"));
    let codex_home = session_dir.join("codex-home");
    fs::create_dir_all(&codex_home).map_err(|source| CliError::StateIo {
        path: codex_home.clone(),
        source,
    })?;
    let codex_home = canonicalize_clean(&codex_home);

    let source_codex_dir = user_home_dir().ok().map(|h| h.join(".codex"));
    seed_ephemeral_codex_home(
        &codex_home,
        source_codex_dir.as_deref(),
        target.client.base_url(),
        &primary_model,
        &auth_token,
        require_env_key,
        &catalog_bytes,
        args.no_bypass,
        args.no_code_mode,
        args.code_mode_only,
    )?;

    let persistent_user_data = state_paths.desktop_dir().join("codex-app");
    let use_ephemeral_profile = args.ephemeral || is_electron_profile_locked(&persistent_user_data);
    let user_data_dir = if use_ephemeral_profile {
        session_dir.join("user-data")
    } else {
        persistent_user_data
    };
    fs::create_dir_all(&user_data_dir).map_err(|source| CliError::StateIo {
        path: user_data_dir.clone(),
        source,
    })?;
    let user_data_dir = canonicalize_clean(&user_data_dir);

    let journals_dir = state_paths.ensure_desktop_journals_dir()?;
    let journal_path = journals_dir.join(format!("codex-app-{session_id}.json"));
    let initial_journal = DesktopSessionJournal {
        session_id: session_id.clone(),
        kind: DesktopKind::CodexApp,
        created_at_unix_ms: now_unix_ms(),
        launcher_pid: std::process::id(),
        watcher_pid: None,
        tracked_pids: Vec::new(),
        explicit_bin: args.codex_app_bin.clone(),
        payload: DesktopSessionPayload::CodexApp {
            session_dir: session_dir.clone(),
            user_data_dir: user_data_dir.clone(),
            ephemeral_user_data: use_ephemeral_profile,
        },
    };
    write_journal_file(&journal_path, &initial_journal)?;
    let mut guard = ForegroundJournalGuard::new(journal_path.clone(), initial_journal);

    let mut launch_args = Vec::with_capacity(1 + args.passthrough_args.len());
    if !args
        .passthrough_args
        .iter()
        .any(|a| a == "--user-data-dir" || a.starts_with("--user-data-dir="))
    {
        launch_args.push(format!("--user-data-dir={}", user_data_dir.display()));
    }
    launch_args.extend(args.passthrough_args.iter().cloned());

    let merged_no_proxy = merge_loopback_no_proxy(
        std::env::var("NO_PROXY")
            .ok()
            .or_else(|| std::env::var("no_proxy").ok())
            .as_deref(),
    );
    let mut extra_env: Vec<(String, String)> = vec![
        ("CODEX_HOME".to_owned(), codex_home.display().to_string()),
        ("AGY_GATEWAY_API_KEY".to_owned(), auth_token),
        ("NO_PROXY".to_owned(), merged_no_proxy.clone()),
        ("no_proxy".to_owned(), merged_no_proxy),
    ];
    if let Some(cli_bin) = &explicit_codex_cli {
        extra_env.push(("CODEX_CLI_PATH".to_owned(), cli_bin.display().to_string()));
    }

    let tracked_pids = spawn_desktop_application(
        DesktopKind::CodexApp,
        &app_bin,
        args.codex_app_bin.as_deref(),
        &launch_args,
        &extra_env,
    )
    .await?;

    finalize_or_watch_session(
        DesktopKind::CodexApp,
        &journal_path,
        &mut guard,
        tracked_pids,
        args.wait,
        "Codex App launched via Liftgate (zero-pollution)",
        vec![
            ("App", app_bin.display_label().to_owned()),
            ("Gateway", format!("{}/v1", target.client.base_url())),
            ("Default Model", primary_model),
            ("Code Mode", tool_mode.to_owned()),
            ("Ephemeral CODEX_HOME", codex_home.display().to_string()),
            ("Isolated Profile", user_data_dir.display().to_string()),
        ],
    )
    .await
}

/// Launches **Claude Desktop** routed through Liftgate's `/claude-desktop` 3P endpoint,
/// restoring original user configuration automatically once Claude Desktop closes.
pub async fn launch_claude_desktop(
    state_paths: &StatePaths,
    args: ClaudeDesktopArgs,
) -> Result<DesktopLaunchOutcome, CliError> {
    if args.restore {
        let restored = force_restore_claude_desktop_sessions(state_paths)?;
        let headline = if restored > 0 {
            format!("Restored {restored} Claude Desktop Liftgate session(s) to original state")
        } else {
            "No active or leftover Liftgate Claude Desktop session state found".to_owned()
        };
        return Ok(DesktopLaunchOutcome {
            kind: DesktopKind::ClaudeDesktop,
            headline,
            rows: Vec::new(),
            restored_sessions: Some(restored),
            waited_foreground: false,
            watcher_fallback_warn: false,
        });
    }

    sweep_stale_desktop_sessions(state_paths);

    let app_bin = resolve_claude_desktop(args.claude_desktop_bin.as_deref())?;
    let running = find_running_desktop_pids(
        DesktopKind::ClaudeDesktop,
        args.claude_desktop_bin.as_deref(),
    );
    if !running.is_empty() {
        if !args.restart_app {
            let pids = running
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(CliError::DesktopAppAlreadyRunning {
                app: "Claude Desktop",
                pids,
            });
        }
        stop_running_desktop_app(
            DesktopKind::ClaudeDesktop,
            args.claude_desktop_bin.as_deref(),
        )
        .await;
    }
    // If a previous session had just exited milliseconds ago, ensure any stale journals are swept
    // before snapshotting clean baseline files.
    let _ = force_restore_claude_desktop_sessions(state_paths);

    let target = ensure_gateway_ready(
        state_paths,
        args.listen,
        args.api_key.clone(),
        args.model.as_deref(),
        true,
    )
    .await?;

    let primary_model = args
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&target.default_model)
        .to_owned();
    let auth_token = normalize_optional_api_key(target.api_key.as_deref())
        .unwrap_or_else(|| PLACEHOLDER_API_KEY.to_owned());

    let discovered = target.client.fetch_models().await.unwrap_or_default();
    let long_context = infer_live_long_context(&discovered, target.managed_long_context);
    let available_models: Vec<AvailableModel> = discovered
        .iter()
        .map(DiscoveredModel::to_available_model)
        .collect();

    let path_sets = resolve_claude_desktop_path_sets()?;
    let backups = snapshot_claude_desktop_path_sets(&path_sets)?;

    let session_id = Uuid::new_v4().simple().to_string();
    let journals_dir = state_paths.ensure_desktop_journals_dir()?;
    let journal_path = journals_dir.join(format!("claude-desktop-{session_id}.json"));
    let initial_journal = DesktopSessionJournal {
        session_id,
        kind: DesktopKind::ClaudeDesktop,
        created_at_unix_ms: now_unix_ms(),
        launcher_pid: std::process::id(),
        watcher_pid: None,
        tracked_pids: Vec::new(),
        explicit_bin: args.claude_desktop_bin.clone(),
        payload: DesktopSessionPayload::ClaudeDesktop { backups },
    };
    // Persist the rollback journal BEFORE modifying any Claude Desktop configuration file.
    write_journal_file(&journal_path, &initial_journal)?;
    let mut guard = ForegroundJournalGuard::new(journal_path.clone(), initial_journal);

    let gateway_3p_url = format!(
        "{}/claude-desktop",
        target.client.base_url().trim_end_matches('/')
    );
    let profile_doc = build_claude_desktop_3p_profile(
        &gateway_3p_url,
        &auth_token,
        &primary_model,
        &available_models,
        long_context,
    );
    apply_claude_desktop_3p_profile(&path_sets, &profile_doc)?;

    let merged_no_proxy = merge_loopback_no_proxy(
        std::env::var("NO_PROXY")
            .ok()
            .or_else(|| std::env::var("no_proxy").ok())
            .as_deref(),
    );
    let extra_env = vec![
        ("NO_PROXY".to_owned(), merged_no_proxy.clone()),
        ("no_proxy".to_owned(), merged_no_proxy),
    ];

    let tracked_pids = spawn_desktop_application(
        DesktopKind::ClaudeDesktop,
        &app_bin,
        args.claude_desktop_bin.as_deref(),
        &args.passthrough_args,
        &extra_env,
    )
    .await?;

    finalize_or_watch_session(
        DesktopKind::ClaudeDesktop,
        &journal_path,
        &mut guard,
        tracked_pids,
        args.wait,
        "Claude Desktop launched via Liftgate (auto-restore armed)",
        vec![
            ("App", app_bin.display_label().to_owned()),
            ("3P Gateway", gateway_3p_url),
            ("Default Model", primary_model),
            (
                "Auto-Restore",
                "restores original 1P/3P state automatically when Claude Desktop exits".to_owned(),
            ),
        ],
    )
    .await
}

/// Internal detached watcher (`lg __desktop-watch --journal <path>`) that waits for the
/// launched GUI session to exit and immediately restores/cleans up temporary session state.
pub async fn run_desktop_watch(args: DesktopWatchArgs) -> Result<(), CliError> {
    let journal = match read_journal_file(&args.journal) {
        Ok(j) => j,
        Err(CliError::StateIo { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(err) => return Err(err),
    };

    wait_for_desktop_session_exit(&journal, false).await;
    cleanup_session_journal(&args.journal, &journal);
    Ok(())
}

async fn finalize_or_watch_session(
    kind: DesktopKind,
    journal_path: &Path,
    guard: &mut ForegroundJournalGuard,
    tracked_pids: Vec<u32>,
    wait_foreground: bool,
    headline_text: &str,
    kv_rows: Vec<(&'static str, String)>,
) -> Result<DesktopLaunchOutcome, CliError> {
    if let Some(journal) = guard.journal.as_mut() {
        journal.tracked_pids = tracked_pids;
        write_journal_file(journal_path, journal)?;
    }

    if wait_foreground {
        if let Some(journal) = guard.journal.as_ref() {
            wait_for_desktop_session_exit(journal, true).await;
        }
        guard.finish();
        return Ok(DesktopLaunchOutcome {
            kind,
            headline: headline_text.to_owned(),
            rows: kv_rows,
            restored_sessions: None,
            waited_foreground: true,
            watcher_fallback_warn: false,
        });
    }

    match spawn_detached_watcher(journal_path) {
        Ok(watcher_pid) => {
            if let Some(journal) = guard.journal.as_mut() {
                journal.watcher_pid = Some(watcher_pid);
                let _ = write_journal_file(journal_path, journal);
            }
            guard.disarm();
            Ok(DesktopLaunchOutcome {
                kind,
                headline: headline_text.to_owned(),
                rows: kv_rows,
                restored_sessions: None,
                waited_foreground: false,
                watcher_fallback_warn: false,
            })
        }
        Err(_) => {
            if let Some(journal) = guard.journal.as_ref() {
                wait_for_desktop_session_exit(journal, true).await;
            }
            guard.finish();
            Ok(DesktopLaunchOutcome {
                kind,
                headline: headline_text.to_owned(),
                rows: kv_rows,
                restored_sessions: None,
                waited_foreground: true,
                watcher_fallback_warn: true,
            })
        }
    }
}

fn spawn_detached_watcher(journal_path: &Path) -> Result<u32, CliError> {
    let current_exe =
        std::env::current_exe().map_err(|source| CliError::DesktopAppLaunchFailed {
            app: "liftgate watcher",
            detail: source.to_string(),
        })?;
    let mut cmd = std::process::Command::new(&current_exe);
    cmd.arg("__desktop-watch")
        .arg("--journal")
        .arg(journal_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_detached_command(&mut cmd);

    #[cfg(windows)]
    let _inherit_guard = crate::daemon::StdioInheritGuard::suspend_current();

    let child = cmd
        .spawn()
        .map_err(|source| CliError::DesktopAppLaunchFailed {
            app: "liftgate watcher",
            detail: format!("{}: {source}", current_exe.display()),
        })?;
    #[cfg(windows)]
    drop(_inherit_guard);

    Ok(child.id())
}

async fn wait_for_desktop_session_exit(journal: &DesktopSessionJournal, honor_ctrl_c: bool) {
    let mut consecutive_dead_checks: u8 = 0;
    loop {
        if !is_journal_session_still_running(journal) {
            consecutive_dead_checks = consecutive_dead_checks.saturating_add(1);
            if consecutive_dead_checks >= 2 {
                break;
            }
        } else {
            consecutive_dead_checks = 0;
        }

        if honor_ctrl_c {
            tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => break,
                () = tokio::time::sleep(Duration::from_millis(600)) => {}
            }
        } else {
            tokio::time::sleep(Duration::from_millis(900)).await;
        }
    }
}

fn is_journal_session_still_running(journal: &DesktopSessionJournal) -> bool {
    if journal.tracked_pids.iter().copied().any(is_pid_alive) {
        return true;
    }
    match journal.kind {
        DesktopKind::ClaudeDesktop => {
            !find_running_desktop_pids(DesktopKind::ClaudeDesktop, journal.explicit_bin.as_deref())
                .is_empty()
        }
        DesktopKind::CodexApp => {
            if journal.tracked_pids.is_empty() {
                !find_running_desktop_pids(DesktopKind::CodexApp, journal.explicit_bin.as_deref())
                    .is_empty()
            } else {
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Codex App Zero-Pollution Seeding
// ---------------------------------------------------------------------------

#[expect(
    clippy::too_many_arguments,
    reason = "isolated CODEX_HOME builder receives validated session parameters directly"
)]
pub fn seed_ephemeral_codex_home(
    ephemeral_codex_home: &Path,
    source_codex_dir: Option<&Path>,
    gateway_base_url: &str,
    primary_model: &str,
    auth_token: &str,
    require_env_key: bool,
    catalog_bytes: &[u8],
    no_bypass: bool,
    no_code_mode: bool,
    code_mode_only: bool,
) -> Result<(), CliError> {
    fs::create_dir_all(ephemeral_codex_home).map_err(|source| CliError::StateIo {
        path: ephemeral_codex_home.to_path_buf(),
        source,
    })?;

    let catalog_path = ephemeral_codex_home.join("agy-models.json");
    write_file_atomic(&catalog_path, catalog_bytes, false)?;

    let existing_toml = source_codex_dir
        .map(|d| d.join("config.toml"))
        .and_then(|p| read_bounded_utf8(&p, MAX_DESKTOP_CONFIG_BYTES).ok())
        .unwrap_or_default();
    let merged_toml = merge_codex_config_toml_with_mode(
        &existing_toml,
        gateway_base_url,
        primary_model,
        &catalog_path,
        require_env_key,
        no_bypass,
        no_code_mode,
        code_mode_only,
    );
    write_file_atomic(
        &ephemeral_codex_home.join("config.toml"),
        merged_toml.as_bytes(),
        true,
    )?;

    // Codex App requires `auth.json` to unlock custom model selection in its GUI picker.
    // If the user already signed in to Codex CLI (`~/.codex/auth.json`), copy it read-only
    // (`0o600`) without modifying `~/.codex`; `[model_providers.agy]` still routes all
    // requests to Liftgate using `AGY_GATEWAY_API_KEY`. Otherwise synthesize an API-key entry.
    let auth_target = ephemeral_codex_home.join("auth.json");
    let copied_existing_auth = source_codex_dir
        .map(|d| d.join("auth.json"))
        .and_then(|src| read_bounded_bytes(&src, MAX_DESKTOP_CONFIG_BYTES).ok())
        .filter(|bytes| !bytes.is_empty());
    if let Some(raw_auth) = copied_existing_auth {
        write_file_atomic(&auth_target, &raw_auth, true)?;
    } else {
        let synthetic_auth = serde_json::to_vec_pretty(&json!({
            "OPENAI_API_KEY": auth_token,
        }))
        .map_err(|source| CliError::StateJson {
            path: auth_target.clone(),
            source,
        })?;
        write_file_atomic(&auth_target, &synthetic_auth, true)?;
    }

    if let Some(src_dir) = source_codex_dir {
        for doc_name in ["AGENTS.md", "instructions.md"] {
            let src_doc = src_dir.join(doc_name);
            if let Ok(bytes) = read_bounded_bytes(&src_doc, MAX_USER_DOC_COPY_BYTES) {
                let _ = write_file_atomic(&ephemeral_codex_home.join(doc_name), &bytes, false);
            }
        }
    }

    Ok(())
}

fn is_electron_profile_locked(profile_dir: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;

        let lockfile = profile_dir.join("lockfile");
        if !lockfile.is_file() {
            return false;
        }
        match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&lockfile)
        {
            Ok(_) => false,
            Err(err) => matches!(err.raw_os_error(), Some(32 | 33)),
        }
    }

    #[cfg(unix)]
    {
        let singleton = profile_dir.join("SingletonLock");
        let Ok(target) = fs::read_link(&singleton) else {
            return false;
        };
        let target_str = target.to_string_lossy();
        if let Some(pid_part) = target_str.rsplit('-').next()
            && let Ok(pid) = pid_part.parse::<u32>()
            && pid > 0
        {
            return is_pid_alive(pid);
        }
        !find_running_desktop_pids(DesktopKind::CodexApp, None).is_empty()
    }
}

// ---------------------------------------------------------------------------
// Claude Desktop 3P Profile Synthesis, Snapshot & Surgical Restore
// ---------------------------------------------------------------------------

#[must_use]
pub fn build_claude_desktop_3p_profile(
    gateway_3p_url: &str,
    api_key: &str,
    primary_model: &str,
    discovered_models: &[AvailableModel],
    long_context: bool,
) -> Value {
    let mut ordered: Vec<(String, String)> =
        Vec::with_capacity(discovered_models.len().saturating_add(1));
    let primary_label = discovered_models
        .iter()
        .find(|m| m.model_id.eq_ignore_ascii_case(primary_model))
        .map(|m| {
            if m.name.trim().is_empty() {
                m.model_id.clone()
            } else {
                m.name.clone()
            }
        })
        .unwrap_or_else(|| primary_model.to_owned());
    ordered.push((primary_model.to_owned(), primary_label));

    for model in discovered_models {
        if ordered
            .iter()
            .any(|(existing_id, _)| existing_id.eq_ignore_ascii_case(&model.model_id))
        {
            continue;
        }
        let label = if model.name.trim().is_empty() {
            model.model_id.clone()
        } else {
            model.name.clone()
        };
        ordered.push((model.model_id.clone(), label));
        if ordered.len() >= MAX_INFERENCE_MODELS {
            break;
        }
    }

    let inference_models: Vec<Value> = ordered
        .into_iter()
        .map(|(model_id, label)| {
            let route_id = liftgate_types::model::encode_claude_desktop_route_id(&model_id);
            let (window, _) = liftgate_types::model::context_limits(&model_id, long_context);
            let supports_1m = window >= 1_000_000;
            let mut item = json!({
                "name": route_id,
                "labelOverride": label,
            });
            if supports_1m && let Some(obj) = item.as_object_mut() {
                obj.insert("supports1m".to_owned(), json!(true));
                obj.insert("prefer1m".to_owned(), json!(true));
            }
            item
        })
        .collect();

    json!({
        "apiKey": api_key,
        "baseUrl": gateway_3p_url,
        "coworkEgressAllowedHosts": ["*"],
        "coworkMode": "enabled",
        "disableDeploymentModeChooser": false,
        "inferenceModels": inference_models,
        "isCcdEnabled": true,
        "isClaudeCodeForDesktopEnabled": true,
        "isCoworkPluginsEnabled": true,
        "isCustomAgentSkillsEnabled": true,
        "isCustomConnectorsEnabled": true,
        "isDirectoryConnectorsEnabled": true,
        "isLocalDevMcpEnabled": true,
        "isMcpAppsEnabled": true,
        "modelDiscoveryEnabled": true,
    })
}

fn snapshot_claude_desktop_path_sets(
    path_sets: &[ClaudeDesktopPathSet],
) -> Result<Vec<ClaudePathSetBackup>, CliError> {
    let mut backups = Vec::with_capacity(path_sets.len());
    for paths in path_sets {
        let normal_dir_existed = paths.normal_dir.is_dir();
        let threep_dir_existed = paths.threep_dir.is_dir();
        let config_library_existed = paths.config_library_dir.is_dir();

        let (normal_config_existed, prior_normal_deployment_mode) =
            read_optional_json_key(&paths.normal_config_path, "deploymentMode")?;
        let (threep_config_existed, prior_threep_deployment_mode) =
            read_optional_json_key(&paths.threep_config_path, "deploymentMode")?;

        let meta_existed = paths.meta_path.is_file();
        let (prior_applied_id, had_prior_liftgate_entry) = if meta_existed {
            let doc = read_json_object_or_empty(&paths.meta_path)?;
            let applied = doc.get("appliedId").cloned();
            let has_entry = doc
                .get("entries")
                .and_then(Value::as_array)
                .is_some_and(|arr| {
                    arr.iter().any(|item| {
                        item.get("id").and_then(Value::as_str)
                            == Some(LIFTGATE_CLAUDE_3P_PROFILE_ID)
                    })
                });
            (applied, has_entry)
        } else {
            (None, false)
        };

        let profile_existed = paths.profile_path.is_file();
        let prior_profile_bytes = if profile_existed {
            Some(read_bounded_bytes(
                &paths.profile_path,
                MAX_DESKTOP_CONFIG_BYTES,
            )?)
        } else {
            None
        };

        backups.push(ClaudePathSetBackup {
            paths: paths.clone(),
            normal_dir_existed,
            threep_dir_existed,
            config_library_existed,
            normal_config_existed,
            prior_normal_deployment_mode,
            threep_config_existed,
            prior_threep_deployment_mode,
            meta_existed,
            prior_applied_id,
            had_prior_liftgate_entry,
            profile_existed,
            prior_profile_bytes,
        });
    }
    Ok(backups)
}

fn apply_claude_desktop_3p_profile(
    path_sets: &[ClaudeDesktopPathSet],
    profile_doc: &Value,
) -> Result<(), CliError> {
    let mut profile_bytes =
        serde_json::to_vec_pretty(profile_doc).map_err(|source| CliError::StateJson {
            path: PathBuf::from("liftgate-3p-profile.json"),
            source,
        })?;
    profile_bytes.push(b'\n');

    for paths in path_sets {
        patch_json_key_atomic(
            &paths.normal_config_path,
            "deploymentMode",
            Some(json!("3p")),
            false,
        )?;
        patch_json_key_atomic(
            &paths.threep_config_path,
            "deploymentMode",
            Some(json!("3p")),
            false,
        )?;

        let mut meta_obj = read_json_object_or_empty(&paths.meta_path)?;
        let mut entries = meta_obj
            .remove("entries")
            .and_then(|v| match v {
                Value::Array(arr) => Some(arr),
                _ => None,
            })
            .unwrap_or_default();
        if let Some(existing) = entries
            .iter_mut()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(LIFTGATE_CLAUDE_3P_PROFILE_ID))
            .and_then(Value::as_object_mut)
        {
            existing.insert("name".to_owned(), json!(LIFTGATE_CLAUDE_3P_PROFILE_NAME));
        } else {
            entries.push(json!({
                "id": LIFTGATE_CLAUDE_3P_PROFILE_ID,
                "name": LIFTGATE_CLAUDE_3P_PROFILE_NAME,
            }));
        }
        meta_obj.insert("entries".to_owned(), Value::Array(entries));
        meta_obj.insert("appliedId".to_owned(), json!(LIFTGATE_CLAUDE_3P_PROFILE_ID));
        write_json_object_atomic(&paths.meta_path, &meta_obj, false)?;

        write_file_atomic(&paths.profile_path, &profile_bytes, true)?;
    }
    Ok(())
}

fn restore_claude_desktop_backups(backups: &[ClaudePathSetBackup]) {
    for backup in backups {
        // Step 1: Restore or remove Liftgate's profile file.
        if backup.profile_existed {
            if let Some(bytes) = &backup.prior_profile_bytes {
                let _ = write_file_atomic(&backup.paths.profile_path, bytes, true);
            }
        } else {
            let _ = fs::remove_file(&backup.paths.profile_path);
        }

        // Step 2: Restore `_meta.json` while preserving any third-party entries added concurrently.
        if !backup.meta_existed {
            let _ = fs::remove_file(&backup.paths.meta_path);
        } else if let Ok(mut meta_obj) = read_json_object_or_empty(&backup.paths.meta_path) {
            if !backup.had_prior_liftgate_entry
                && let Some(Value::Array(entries)) = meta_obj.get_mut("entries")
            {
                entries.retain(|e| {
                    e.get("id").and_then(Value::as_str) != Some(LIFTGATE_CLAUDE_3P_PROFILE_ID)
                });
            }
            match &backup.prior_applied_id {
                Some(prev) => {
                    meta_obj.insert("appliedId".to_owned(), prev.clone());
                }
                None => {
                    meta_obj.remove("appliedId");
                }
            }
            let _ = write_json_object_atomic(&backup.paths.meta_path, &meta_obj, false);
        }

        if !backup.config_library_existed {
            let _ = remove_dir_if_empty(&backup.paths.config_library_dir);
        }

        // Step 3: Restore `Claude-3p` config or remove newly created `Claude-3p` tree.
        if !backup.threep_dir_existed {
            // Set safe `1p` fallback first in case a background OS indexer briefly holds a lock.
            let _ = patch_json_key_atomic(
                &backup.paths.threep_config_path,
                "deploymentMode",
                Some(json!("1p")),
                false,
            );
            remove_dir_all_best_effort(&backup.paths.threep_dir);
        } else if !backup.threep_config_existed {
            restore_or_unlink_new_config_file(&backup.paths.threep_config_path, "deploymentMode");
        } else {
            let _ = patch_json_key_atomic(
                &backup.paths.threep_config_path,
                "deploymentMode",
                backup.prior_threep_deployment_mode.clone(),
                false,
            );
        }

        // Step 4: Surgically restore `Claude/claude_desktop_config.json`, preserving any
        // `mcpServers` or `preferences` changes the user made during the session.
        let threep_still_present = backup.paths.threep_dir.is_dir();
        if !backup.normal_config_existed && !threep_still_present {
            restore_or_unlink_new_config_file(&backup.paths.normal_config_path, "deploymentMode");
            if !backup.normal_dir_existed {
                let _ = remove_dir_if_empty(&backup.paths.normal_dir);
            }
        } else {
            let target_mode = match &backup.prior_normal_deployment_mode {
                Some(prior) => Some(prior.clone()),
                None if threep_still_present => Some(json!("1p")),
                None => None,
            };
            let _ = patch_json_key_atomic(
                &backup.paths.normal_config_path,
                "deploymentMode",
                target_mode,
                false,
            );
        }
    }
}

pub fn force_restore_claude_desktop_sessions(state_paths: &StatePaths) -> Result<usize, CliError> {
    let dir = state_paths.desktop_journals_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(it) => it,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(CliError::StateIo { path: dir, source }),
    };
    let mut count = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if let Ok(journal) = read_journal_file(&path)
            && journal.kind == DesktopKind::ClaudeDesktop
        {
            cleanup_session_journal(&path, &journal);
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

fn cleanup_session_journal(journal_path: &Path, journal: &DesktopSessionJournal) {
    match &journal.payload {
        DesktopSessionPayload::CodexApp {
            session_dir,
            user_data_dir,
            ephemeral_user_data,
        } => {
            if *ephemeral_user_data {
                remove_dir_all_best_effort(user_data_dir);
            }
            remove_dir_all_best_effort(session_dir);
        }
        DesktopSessionPayload::ClaudeDesktop { backups } => {
            restore_claude_desktop_backups(backups);
        }
    }
    let _ = fs::remove_file(journal_path);
}

fn restore_or_unlink_new_config_file(path: &Path, key_to_strip: &str) {
    let Ok(mut obj) = read_json_object_or_empty(path) else {
        let _ = fs::remove_file(path);
        return;
    };
    obj.remove(key_to_strip);
    if obj.is_empty() {
        let _ = fs::remove_file(path);
    } else {
        let _ = write_json_object_atomic(path, &obj, false);
    }
}

fn remove_dir_if_empty(dir: &Path) -> std::io::Result<()> {
    let mut iter = fs::read_dir(dir)?;
    if iter.next().is_none() {
        fs::remove_dir(dir)?;
    }
    Ok(())
}

fn remove_dir_all_best_effort(dir: &Path) {
    for attempt in 0..3u8 {
        if !dir.exists() || fs::remove_dir_all(dir).is_ok() {
            return;
        }
        if attempt < 2 {
            std::thread::sleep(Duration::from_millis(120));
        }
    }
}

fn read_optional_json_key(path: &Path, key: &str) -> Result<(bool, Option<Value>), CliError> {
    if !path.is_file() {
        return Ok((false, None));
    }
    let obj = read_json_object_or_empty(path)?;
    Ok((true, obj.get(key).cloned()))
}

fn read_json_object_or_empty(path: &Path) -> Result<Map<String, Value>, CliError> {
    let bytes = match read_bounded_bytes(path, MAX_DESKTOP_CONFIG_BYTES) {
        Ok(b) => b,
        Err(CliError::StateIo { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Map::new());
        }
        Err(err) => return Err(err),
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|source| CliError::StateJson {
        path: path.to_path_buf(),
        source,
    })?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Ok(Map::new()),
    }
}

fn patch_json_key_atomic(
    path: &Path,
    key: &str,
    new_value: Option<Value>,
    secret_mode: bool,
) -> Result<(), CliError> {
    let mut obj = read_json_object_or_empty(path)?;
    match new_value {
        Some(v) => {
            obj.insert(key.to_owned(), v);
        }
        None => {
            obj.remove(key);
        }
    }
    write_json_object_atomic(path, &obj, secret_mode)
}

fn write_json_object_atomic(
    path: &Path,
    obj: &Map<String, Value>,
    secret_mode: bool,
) -> Result<(), CliError> {
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(obj.clone())).map_err(|source| {
        CliError::StateJson {
            path: path.to_path_buf(),
            source,
        }
    })?;
    bytes.push(b'\n');
    write_file_atomic(path, &bytes, secret_mode)
}

fn read_journal_file(path: &Path) -> Result<DesktopSessionJournal, CliError> {
    let bytes = read_bounded_bytes(path, MAX_DESKTOP_CONFIG_BYTES)?;
    serde_json::from_slice(&bytes).map_err(|source| CliError::StateJson {
        path: path.to_path_buf(),
        source,
    })
}

fn write_journal_file(path: &Path, journal: &DesktopSessionJournal) -> Result<(), CliError> {
    let mut bytes = serde_json::to_vec_pretty(journal).map_err(|source| CliError::StateJson {
        path: path.to_path_buf(),
        source,
    })?;
    bytes.push(b'\n');
    write_file_atomic(path, &bytes, true)
}

fn read_bounded_bytes(path: &Path, limit: u64) -> Result<Vec<u8>, CliError> {
    let file = File::open(path).map_err(|source| CliError::StateIo {
        path: path.to_path_buf(),
        source,
    })?;
    let mut buf = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|source| CliError::StateIo {
            path: path.to_path_buf(),
            source,
        })?;
    if u64::try_from(buf.len()).unwrap_or(u64::MAX) > limit {
        return Err(CliError::StateIo {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "desktop configuration file exceeds safety size limit",
            ),
        });
    }
    Ok(buf)
}

fn read_bounded_utf8(path: &Path, limit: u64) -> Result<String, CliError> {
    let bytes = read_bounded_bytes(path, limit)?;
    String::from_utf8(bytes).map_err(|err| CliError::StateIo {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, err),
    })
}

// ---------------------------------------------------------------------------
// Cross-Platform Claude Desktop Config Path Resolution
// ---------------------------------------------------------------------------

pub fn resolve_claude_desktop_path_sets() -> Result<Vec<ClaudeDesktopPathSet>, CliError> {
    let home = user_home_dir()?;

    #[cfg(target_os = "macos")]
    {
        Ok(vec![macos_claude_desktop_paths(&home)])
    }

    #[cfg(windows)]
    {
        let roaming = std::env::var_os("APPDATA")
            .filter(|s| !s.is_empty())
            .map_or_else(|| home.join("AppData").join("Roaming"), PathBuf::from);
        let local = std::env::var_os("LOCALAPPDATA")
            .filter(|s| !s.is_empty())
            .map_or_else(|| home.join("AppData").join("Local"), PathBuf::from);
        Ok(windows_claude_desktop_path_sets(&roaming, &local))
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let in_flatpak = Path::new("/.flatpak-info").exists();
        let mut sets = vec![linux_claude_desktop_paths(
            &home,
            xdg.as_deref(),
            in_flatpak,
        )];
        if !in_flatpak {
            sets.extend(linux_flatpak_claude_desktop_path_sets(&home));
        }
        Ok(sets)
    }
}

#[cfg(any(target_os = "macos", test))]
#[must_use]
pub fn macos_claude_desktop_paths(home: &Path) -> ClaudeDesktopPathSet {
    let app_support = home.join("Library").join("Application Support");
    ClaudeDesktopPathSet::from_dirs(app_support.join("Claude"), app_support.join("Claude-3p"))
}

#[cfg(any(not(any(target_os = "macos", windows)), test))]
#[must_use]
pub fn linux_claude_desktop_paths(
    home: &Path,
    xdg_config_home: Option<&Path>,
    in_flatpak: bool,
) -> ClaudeDesktopPathSet {
    let base_config = match (in_flatpak, xdg_config_home) {
        (false, Some(xdg))
            if xdg.is_absolute() || xdg.to_str().is_some_and(|s| s.starts_with('/')) =>
        {
            xdg.to_path_buf()
        }
        _ => home.join(".config"),
    };
    ClaudeDesktopPathSet::from_dirs(base_config.join("Claude"), base_config.join("Claude-3p"))
}

#[cfg(any(not(any(target_os = "macos", windows)), test))]
#[must_use]
pub fn linux_flatpak_claude_desktop_path_sets(home: &Path) -> Vec<ClaudeDesktopPathSet> {
    let var_app = home.join(".var").join("app");
    let Ok(entries) = fs::read_dir(&var_app) else {
        return Vec::new();
    };
    let mut app_dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| name.to_ascii_lowercase().contains("claude"))
        })
        .collect();
    app_dirs.sort();
    app_dirs
        .into_iter()
        .map(|app_dir| {
            let cfg = app_dir.join("config");
            ClaudeDesktopPathSet::from_dirs(cfg.join("Claude"), cfg.join("Claude-3p"))
        })
        .collect()
}

#[cfg(any(windows, test))]
#[must_use]
pub fn windows_claude_desktop_path_sets(
    roaming_app_data: &Path,
    local_app_data: &Path,
) -> Vec<ClaudeDesktopPathSet> {
    let mut sets = Vec::new();

    // 1. Standard Electron `userData` (`%APPDATA%\Claude` & `%APPDATA%\Claude-3p`).
    sets.push(ClaudeDesktopPathSet::from_dirs(
        roaming_app_data.join("Claude"),
        roaming_app_data.join("Claude-3p"),
    ));

    // 2. LocalAppData (`%LOCALAPPDATA%\Claude` & `%LOCALAPPDATA%\Claude-3p`) used by newer
    //    enterprise/3P desktop builds.
    if local_app_data != roaming_app_data {
        sets.push(ClaudeDesktopPathSet::from_dirs(
            local_app_data.join("Claude"),
            local_app_data.join("Claude-3p"),
        ));
    }

    // 3. Windows Store / MSIX virtualized `LocalCache` roots (`Packages\Claude_*\LocalCache\...`).
    let packages_dir = local_app_data.join("Packages");
    if let Ok(entries) = fs::read_dir(&packages_dir) {
        let mut pkg_dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|name| name.starts_with("Claude_"))
            })
            .collect();
        pkg_dirs.sort();
        for pkg in pkg_dirs {
            let local_cache = pkg.join("LocalCache");
            if !local_cache.is_dir() {
                continue;
            }
            let msix_roaming = local_cache.join("Roaming");
            sets.push(ClaudeDesktopPathSet::from_dirs(
                msix_roaming.join("Claude"),
                msix_roaming.join("Claude-3p"),
            ));
            let msix_local = local_cache.join("Local");
            sets.push(ClaudeDesktopPathSet::from_dirs(
                msix_local.join("Claude"),
                msix_local.join("Claude-3p"),
            ));
        }
    }

    sets
}

// ---------------------------------------------------------------------------
// Cross-Platform Binary & Package Discovery
// ---------------------------------------------------------------------------

pub fn resolve_codex_app(explicit: Option<&Path>) -> Result<DesktopAppBinary, CliError> {
    if let Some(path) = explicit {
        return resolve_explicit_desktop_binary(DesktopKind::CodexApp, path);
    }

    #[cfg(target_os = "macos")]
    {
        for bundle in macos_bundle_candidates("Codex.app") {
            if let Some(exe) = resolve_macos_app_bundle(&bundle, &["Codex"]) {
                return Ok(DesktopAppBinary::Direct {
                    display_label: format!("{} ({})", bundle.display(), exe.display()),
                    executable: exe,
                });
            }
        }
    }

    #[cfg(windows)]
    {
        for candidate in windows_codex_standalone_candidates() {
            if is_executable_file(&candidate) {
                let clean = canonicalize_clean(&candidate);
                return Ok(DesktopAppBinary::Direct {
                    display_label: clean.display().to_string(),
                    executable: clean,
                });
            }
        }
        if let Some(msix) = resolve_windows_msix_app(
            "OpenAI.Codex",
            &["App"],
            &[
                "app\\Codex.exe",
                "app\\ChatGPT.exe",
                "Codex.exe",
                "ChatGPT.exe",
            ],
        ) {
            return Ok(msix);
        }
    }

    for candidate in path_and_linux_candidates(&["codex-desktop", "codex-app"], "Codex") {
        if is_usable_direct_desktop_binary(&candidate) {
            let clean = canonicalize_clean(&candidate);
            return Ok(DesktopAppBinary::Direct {
                display_label: clean.display().to_string(),
                executable: clean,
            });
        }
    }

    Err(CliError::DesktopAppNotFound {
        app: "Codex App",
        hint: "install Codex desktop app or pass `--codex-app-bin <PATH>` (or set `CODEX_APP_BIN`)",
    })
}

pub fn resolve_claude_desktop(explicit: Option<&Path>) -> Result<DesktopAppBinary, CliError> {
    if let Some(path) = explicit {
        return resolve_explicit_desktop_binary(DesktopKind::ClaudeDesktop, path);
    }

    #[cfg(target_os = "macos")]
    {
        for bundle in macos_bundle_candidates("Claude.app") {
            if let Some(exe) = resolve_macos_app_bundle(&bundle, &["Claude"]) {
                return Ok(DesktopAppBinary::Direct {
                    display_label: format!("{} ({})", bundle.display(), exe.display()),
                    executable: exe,
                });
            }
        }
    }

    #[cfg(windows)]
    {
        for candidate in windows_claude_standalone_candidates() {
            if is_executable_file(&candidate) {
                let clean = canonicalize_clean(&candidate);
                return Ok(DesktopAppBinary::Direct {
                    display_label: clean.display().to_string(),
                    executable: clean,
                });
            }
        }
        if let Some(msix) = resolve_windows_msix_app(
            "Claude",
            &["Claude", "App"],
            &["app\\Claude.exe", "Claude.exe"],
        ) {
            return Ok(msix);
        }
    }

    for candidate in path_and_linux_candidates(&["claude-desktop"], "Claude") {
        if is_usable_direct_desktop_binary(&candidate) {
            let clean = canonicalize_clean(&candidate);
            return Ok(DesktopAppBinary::Direct {
                display_label: clean.display().to_string(),
                executable: clean,
            });
        }
    }

    Err(CliError::DesktopAppNotFound {
        app: "Claude Desktop",
        hint: "install Claude Desktop or pass `--claude-desktop-bin <PATH>` (or set `CLAUDE_DESKTOP_BIN`)",
    })
}

fn resolve_explicit_desktop_binary(
    kind: DesktopKind,
    explicit: &Path,
) -> Result<DesktopAppBinary, CliError> {
    let preferred_names: &[&str] = match kind {
        DesktopKind::CodexApp => &["Codex", "ChatGPT", "codex-desktop", "codex-app"],
        DesktopKind::ClaudeDesktop => &["Claude", "claude-desktop"],
    };
    if explicit.is_dir()
        && explicit
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("app"))
        && let Some(exe) = resolve_macos_app_bundle(explicit, preferred_names)
    {
        return Ok(DesktopAppBinary::Direct {
            display_label: format!("{} ({})", explicit.display(), exe.display()),
            executable: exe,
        });
    }

    #[cfg(windows)]
    {
        let path_lower = explicit.to_string_lossy().to_ascii_lowercase();
        if path_lower.contains("\\windowsapps\\") {
            let (pkg_name, ids, rels): (&str, &[&str], &[&str]) = match kind {
                DesktopKind::CodexApp => (
                    "OpenAI.Codex",
                    &["App"],
                    &[
                        "app\\Codex.exe",
                        "app\\ChatGPT.exe",
                        "Codex.exe",
                        "ChatGPT.exe",
                    ],
                ),
                DesktopKind::ClaudeDesktop => (
                    "Claude",
                    &["Claude", "App"],
                    &["app\\Claude.exe", "Claude.exe"],
                ),
            };
            if let Some(msix) = resolve_windows_msix_app(pkg_name, ids, rels) {
                return Ok(msix);
            }
        }
    }

    if is_executable_file(explicit) {
        let clean = canonicalize_clean(explicit);
        return Ok(DesktopAppBinary::Direct {
            display_label: clean.display().to_string(),
            executable: clean,
        });
    }

    let (app, hint) = match kind {
        DesktopKind::CodexApp => (
            "Codex App",
            "explicit `--codex-app-bin` path does not exist or is not an executable / `.app` bundle",
        ),
        DesktopKind::ClaudeDesktop => (
            "Claude Desktop",
            "explicit `--claude-desktop-bin` path does not exist or is not an executable / `.app` bundle",
        ),
    };
    Err(CliError::DesktopAppNotFound { app, hint })
}

fn resolve_macos_app_bundle(bundle_dir: &Path, preferred_names: &[&str]) -> Option<PathBuf> {
    let macos_dir = bundle_dir.join("Contents").join("MacOS");
    if !macos_dir.is_dir() {
        return None;
    }
    for name in preferred_names {
        let candidate = macos_dir.join(name);
        if is_executable_file(&candidate) {
            return Some(canonicalize_clean(&candidate));
        }
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(&macos_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_executable_file(p))
        .collect();
    entries.sort();
    entries.into_iter().next().map(|p| canonicalize_clean(&p))
}

#[cfg(target_os = "macos")]
fn macos_bundle_candidates(bundle_name: &str) -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from("/Applications").join(bundle_name)];
    if let Ok(home) = user_home_dir() {
        out.push(home.join("Applications").join(bundle_name));
    }
    out
}

fn path_and_linux_candidates(bin_names: &[&str], app_prefix: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in split_expanded_path_var() {
        for name in bin_names {
            #[cfg(windows)]
            {
                out.push(dir.join(format!("{name}.exe")));
            }
            out.push(dir.join(name));
        }
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    {
        for name in bin_names {
            out.push(PathBuf::from("/usr/bin").join(name));
            out.push(PathBuf::from("/usr/local/bin").join(name));
            out.push(PathBuf::from("/usr/lib").join(name).join(name));
            out.push(PathBuf::from("/opt").join(app_prefix).join(name));
            out.push(PathBuf::from("/opt").join(name).join(name));
            out.push(PathBuf::from("/snap/bin").join(name));
        }
        let flatpak_ids: &[&str] = if app_prefix.eq_ignore_ascii_case("Claude") {
            &[
                "com.anthropic.Claude",
                "io.github.aaddrick.claude-desktop-debian",
            ]
        } else {
            &["com.openai.Codex", "com.openai.codex"]
        };
        if let Ok(home) = user_home_dir() {
            for name in bin_names {
                out.push(home.join(".local").join("bin").join(name));
            }
            for fp_id in flatpak_ids {
                out.push(
                    home.join(".local")
                        .join("share")
                        .join("flatpak")
                        .join("exports")
                        .join("bin")
                        .join(fp_id),
                );
            }
            for appimage_dir in [home.join("Applications"), home.join(".local").join("bin")] {
                if let Ok(entries) = fs::read_dir(&appimage_dir) {
                    let prefix_lower = app_prefix.to_ascii_lowercase();
                    let mut appimages: Vec<PathBuf> = entries
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| {
                            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                                let lower = n.to_ascii_lowercase();
                                lower.starts_with(&prefix_lower) && lower.ends_with(".appimage")
                            })
                        })
                        .collect();
                    appimages.sort();
                    appimages.reverse();
                    out.extend(appimages);
                }
            }
        }
        for fp_id in flatpak_ids {
            out.push(PathBuf::from("/var/lib/flatpak/exports/bin").join(fp_id));
        }
    }
    #[cfg(any(target_os = "macos", windows))]
    let _ = app_prefix;

    out
}

fn is_usable_direct_desktop_binary(path: &Path) -> bool {
    #[cfg(windows)]
    {
        if let Ok(meta) = fs::symlink_metadata(path)
            && meta.len() == 0
        {
            return false;
        }
    }
    is_executable_file(path)
}

#[cfg(windows)]
fn windows_codex_standalone_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        out.push(local.join("Programs").join("Codex").join("Codex.exe"));
        out.push(local.join("Codex").join("Codex.exe"));
    }
    if let Some(pf) = std::env::var_os("ProgramFiles").map(PathBuf::from) {
        out.push(pf.join("Codex").join("Codex.exe"));
    }
    out
}

#[cfg(windows)]
fn windows_claude_standalone_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        // Prefer the versioned `AnthropicClaude\app-*\claude.exe` binary directly over the
        // Squirrel launcher stub (`AnthropicClaude\claude.exe`) so PID tracking follows the GUI.
        let squirrel_root = local.join("AnthropicClaude");
        if let Ok(entries) = fs::read_dir(&squirrel_root) {
            let mut app_dirs: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_dir()
                        && p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("app-"))
                })
                .collect();
            app_dirs.sort();
            app_dirs.reverse();
            for dir in app_dirs {
                out.push(dir.join("claude.exe"));
                out.push(dir.join("Claude.exe"));
            }
        }
        out.push(squirrel_root.join("claude.exe"));
        out.push(local.join("Programs").join("Claude").join("Claude.exe"));
        out.push(
            local
                .join("Programs")
                .join("claude-desktop")
                .join("Claude.exe"),
        );
    }
    if let Some(pf) = std::env::var_os("ProgramFiles").map(PathBuf::from) {
        out.push(pf.join("Claude").join("Claude.exe"));
    }
    out
}

#[cfg(windows)]
#[derive(Clone, Debug, Deserialize)]
struct AppxPackageInfo {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "PackageFamilyName")]
    package_family_name: String,
    #[serde(rename = "InstallLocation")]
    install_location: String,
}

#[cfg(windows)]
fn query_installed_msix_packages() -> &'static [AppxPackageInfo] {
    static CACHE: std::sync::OnceLock<Vec<AppxPackageInfo>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

        let ps_exe = windows_powershell_exe();
        let script = "@(foreach ($n in @('OpenAI.Codex', 'Claude')) { \
            Get-AppxPackage -Name $n -ErrorAction SilentlyContinue | \
            Select-Object -First 1 Name, PackageFamilyName, InstallLocation \
        }) | ConvertTo-Json -Compress";
        let output = std::process::Command::new(ps_exe)
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .stdin(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        let Ok(out) = output else {
            return Vec::new();
        };
        let raw = String::from_utf8_lossy(&out.stdout);
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "null" {
            return Vec::new();
        }
        if let Ok(list) = serde_json::from_str::<Vec<AppxPackageInfo>>(trimmed) {
            list
        } else if let Ok(single) = serde_json::from_str::<AppxPackageInfo>(trimmed) {
            vec![single]
        } else {
            Vec::new()
        }
    })
}

#[cfg(windows)]
fn resolve_windows_msix_app(
    package_name: &str,
    preferred_app_ids: &[&str],
    fallback_rel_exes: &[&str],
) -> Option<DesktopAppBinary> {
    let pkg = query_installed_msix_packages()
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(package_name))?;
    let install_dir = PathBuf::from(&pkg.install_location);
    if !install_dir.is_dir() {
        return None;
    }

    let manifest_xml = read_bounded_utf8(
        &install_dir.join("AppxManifest.xml"),
        MAX_DESKTOP_CONFIG_BYTES,
    )
    .unwrap_or_default();
    if let Some((app_id, exe_rel)) =
        parse_appx_manifest_primary_app(&manifest_xml, preferred_app_ids)
    {
        let exe_rel = exe_rel.replace('/', "\\");
        let exe_full = install_dir.join(&exe_rel);
        return Some(DesktopAppBinary::WindowsMsix {
            display_label: format!("{} [MSIX {}]", exe_full.display(), pkg.package_family_name),
            package_family_name: pkg.package_family_name.clone(),
            app_id,
            executable_rel: exe_rel,
            executable_full: exe_full,
        });
    }

    let app_id = preferred_app_ids
        .first()
        .copied()
        .unwrap_or("App")
        .to_owned();
    for rel in fallback_rel_exes {
        let full = install_dir.join(rel);
        if full.is_file() {
            return Some(DesktopAppBinary::WindowsMsix {
                display_label: format!("{} [MSIX {}]", full.display(), pkg.package_family_name),
                package_family_name: pkg.package_family_name.clone(),
                app_id,
                executable_rel: (*rel).to_owned(),
                executable_full: full,
            });
        }
    }
    None
}

#[cfg(any(windows, test))]
#[must_use]
pub fn parse_appx_manifest_primary_app(
    manifest_xml: &str,
    preferred_app_ids: &[&str],
) -> Option<(String, String)> {
    let mut discovered: Vec<(String, String)> = Vec::new();
    let mut rest = manifest_xml;
    while let Some(pos) = rest.find("<Application") {
        let after_tag = rest.get(pos + "<Application".len()..)?;
        let valid_boundary = after_tag
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_whitespace() || c == '>');
        if !valid_boundary {
            rest = after_tag;
            continue;
        }
        let Some(end_pos) = after_tag.find('>') else {
            break;
        };
        let attrs = after_tag.get(..end_pos).unwrap_or("");
        if let (Some(id), Some(exe)) = (
            extract_xml_attr(attrs, "Id"),
            extract_xml_attr(attrs, "Executable"),
        ) {
            discovered.push((id, exe));
        }
        rest = after_tag.get(end_pos + 1..).unwrap_or("");
    }

    for preferred in preferred_app_ids {
        if let Some(found) = discovered
            .iter()
            .find(|(id, _)| id.eq_ignore_ascii_case(preferred))
        {
            return Some(found.clone());
        }
    }
    discovered
        .iter()
        .find(|(_, exe)| !exe.to_ascii_lowercase().contains("resources"))
        .or_else(|| discovered.first())
        .cloned()
}

#[cfg(any(windows, test))]
fn extract_xml_attr(tag_attrs: &str, attr_name: &str) -> Option<String> {
    let pattern = format!("{attr_name}=\"");
    let mut search = tag_attrs;
    while let Some(idx) = search.find(&pattern) {
        let valid_prefix = idx == 0
            || search
                .get(..idx)
                .and_then(|before| before.chars().next_back())
                .is_some_and(|ch| ch.is_ascii_whitespace());
        let after = search.get(idx + pattern.len()..)?;
        if valid_prefix {
            let end_quote = after.find('"')?;
            return after.get(..end_quote).map(str::to_owned);
        }
        search = after;
    }
    None
}

#[cfg(windows)]
fn windows_powershell_exe() -> PathBuf {
    if let Some(sys_root) = std::env::var_os("SystemRoot").filter(|s| !s.is_empty()) {
        let candidate = PathBuf::from(sys_root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("powershell.exe")
}

// ---------------------------------------------------------------------------
// Process Spawning, Enumeration & Graceful Restart
// ---------------------------------------------------------------------------

async fn spawn_desktop_application(
    kind: DesktopKind,
    app_bin: &DesktopAppBinary,
    explicit_bin: Option<&Path>,
    launch_args: &[String],
    extra_env: &[(String, String)],
) -> Result<Vec<u32>, CliError> {
    let pre_pids: BTreeSet<u32> = find_running_desktop_pids(kind, explicit_bin)
        .into_iter()
        .collect();

    let direct_pid = match app_bin {
        DesktopAppBinary::Direct { executable, .. } => {
            let mut cmd = std::process::Command::new(executable);
            cmd.args(launch_args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            for (k, v) in extra_env {
                cmd.env(k, v);
            }
            configure_detached_command(&mut cmd);

            #[cfg(windows)]
            let _inherit_guard = crate::daemon::StdioInheritGuard::suspend_current();

            let child = cmd
                .spawn()
                .map_err(|source| CliError::DesktopAppLaunchFailed {
                    app: kind.display_name(),
                    detail: format!("{}: {source}", executable.display()),
                })?;
            #[cfg(windows)]
            drop(_inherit_guard);
            Some(child.id())
        }
        #[cfg(windows)]
        DesktopAppBinary::WindowsMsix {
            package_family_name,
            app_id,
            executable_rel,
            executable_full,
            ..
        } => {
            spawn_windows_msix_package(
                kind.display_name(),
                package_family_name,
                app_id,
                executable_rel,
                executable_full,
                launch_args,
                extra_env,
            )
            .await?;
            None
        }
    };

    // Poll briefly so packaged activators (`Invoke-CommandInDesktopPackage`, wrapper scripts) have
    // materialized the target Electron main process PID.
    let mut tracked = BTreeSet::new();
    if let Some(pid) = direct_pid.filter(|&p| p > 0 && is_pid_alive(p)) {
        tracked.insert(pid);
    }

    for _ in 0..35u8 {
        let now_pids = find_running_desktop_pids(kind, explicit_bin);
        for pid in &now_pids {
            if !pre_pids.contains(pid) {
                tracked.insert(*pid);
            }
        }
        if !tracked.is_empty() {
            break;
        }
        if pre_pids.is_empty() && !now_pids.is_empty() {
            tracked.extend(now_pids);
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    Ok(tracked.into_iter().collect())
}

#[cfg(windows)]
async fn spawn_windows_msix_package(
    app_display_name: &'static str,
    package_family_name: &str,
    app_id: &str,
    executable_rel: &str,
    executable_full: &Path,
    launch_args: &[String],
    extra_env: &[(String, String)],
) -> Result<(), CliError> {
    use tokio::process::Command;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    let ps_exe = windows_powershell_exe();
    let joined_args = join_windows_command_line_args(launch_args);
    let exe_arg = if executable_full.is_file() {
        executable_full.display().to_string()
    } else {
        executable_rel.to_owned()
    };

    let ps_script = "$ErrorActionPreference = 'Stop'; \
        $pfn = $env:__LG_MSIX_PFN; \
        $app = $env:__LG_MSIX_APP_ID; \
        $exe = $env:__LG_MSIX_EXE; \
        $argStr = $env:__LG_MSIX_ARGS; \
        Remove-Item Env:__LG_MSIX_PFN, Env:__LG_MSIX_APP_ID, Env:__LG_MSIX_EXE, Env:__LG_MSIX_ARGS -ErrorAction SilentlyContinue; \
        if ([string]::IsNullOrEmpty($argStr)) { \
            Invoke-CommandInDesktopPackage -PackageFamilyName $pfn -AppId $app -Command $exe | Out-Null \
        } else { \
            Invoke-CommandInDesktopPackage -PackageFamilyName $pfn -AppId $app -Command $exe -Args $argStr | Out-Null \
        }";

    let mut cmd = Command::new(&ps_exe);
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", ps_script])
        .env("__LG_MSIX_PFN", package_family_name)
        .env("__LG_MSIX_APP_ID", app_id)
        .env("__LG_MSIX_EXE", &exe_arg)
        .env("__LG_MSIX_ARGS", &joined_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }

    let output = cmd
        .output()
        .await
        .map_err(|source| CliError::DesktopAppLaunchFailed {
            app: app_display_name,
            detail: format!("{} ({package_family_name}): {source}", ps_exe.display()),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(CliError::DesktopAppLaunchFailed {
            app: app_display_name,
            detail: if detail.is_empty() {
                format!(
                    "Invoke-CommandInDesktopPackage ({package_family_name}!{app_id}) exited with {}",
                    output.status
                )
            } else {
                detail.to_owned()
            },
        });
    }
    Ok(())
}

#[cfg(any(windows, test))]
#[must_use]
fn join_windows_command_line_args(args: &[String]) -> String {
    args.iter()
        .map(|s| quote_windows_arg(s))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(any(windows, test))]
#[must_use]
fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| c == ' ' || c == '\t' || c == '\n' || c == '\x0b' || c == '"')
    {
        return arg.to_owned();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        if ch == '\\' {
            backslashes += 1;
        } else if ch == '"' {
            for _ in 0..(backslashes * 2 + 1) {
                out.push('\\');
            }
            out.push('"');
            backslashes = 0;
        } else {
            for _ in 0..backslashes {
                out.push('\\');
            }
            backslashes = 0;
            out.push(ch);
        }
    }
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
    out
}

async fn stop_running_desktop_app(kind: DesktopKind, explicit_bin: Option<&Path>) {
    let initial_pids = find_running_desktop_pids(kind, explicit_bin);
    if initial_pids.is_empty() {
        return;
    }

    #[cfg(target_os = "macos")]
    {
        let app_name = match kind {
            DesktopKind::CodexApp => "Codex",
            DesktopKind::ClaudeDesktop => "Claude",
        };
        let _ = tokio::process::Command::new("/usr/bin/osascript")
            .args(["-e", &format!("tell application \"{app_name}\" to quit")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }

    #[cfg(unix)]
    {
        for &pid in &initial_pids {
            if let Ok(raw) = i32::try_from(pid)
                && raw > 0
            {
                // SAFETY: Sends polite `SIGTERM` to verified desktop application PID.
                unsafe {
                    let _ = libc::kill(raw, libc::SIGTERM);
                }
            }
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

        for &pid in &initial_pids {
            let _ = std::process::Command::new("taskkill.exe")
                .args(["/PID", &pid.to_string(), "/T"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .status();
        }
    }

    for _ in 0..25u8 {
        if find_running_desktop_pids(kind, explicit_bin).is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(160)).await;
    }

    let stubborn = find_running_desktop_pids(kind, explicit_bin);
    #[cfg(unix)]
    {
        for pid in stubborn {
            if let Ok(raw) = i32::try_from(pid)
                && raw > 0
            {
                // SAFETY: Force-terminates unresponsive desktop app PID requested via `--restart-app`.
                unsafe {
                    let _ = libc::kill(raw, libc::SIGKILL);
                }
            }
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

        for pid in stubborn {
            let _ = std::process::Command::new("taskkill.exe")
                .args(["/F", "/PID", &pid.to_string(), "/T"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .status();
        }
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
}

/// Enumerates running desktop GUI process IDs for `kind`, carefully excluding CLI binaries
/// (`claude` Code CLI, `codex` CLI, bundled `resources/codex.exe` app-server) so terminal
/// agent sessions are never misidentified as desktop GUI windows.
#[must_use]
pub fn find_running_desktop_pids(kind: DesktopKind, explicit_bin: Option<&Path>) -> Vec<u32> {
    let self_pid = std::process::id();
    let explicit_norm = explicit_bin.map(|p| normalize_match_path(&canonicalize_clean(p)));

    #[cfg(windows)]
    {
        find_running_desktop_pids_windows(kind, self_pid, explicit_norm.as_deref())
    }

    #[cfg(target_os = "linux")]
    {
        find_running_desktop_pids_linux(kind, self_pid, explicit_norm.as_deref())
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    {
        find_running_desktop_pids_ps(kind, self_pid, explicit_norm.as_deref())
    }
}

fn normalize_match_path(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s
    }
}

fn is_matching_desktop_exe_path(
    kind: DesktopKind,
    raw_exe_path: &str,
    explicit_norm: Option<&str>,
) -> bool {
    let norm = raw_exe_path.replace('\\', "/");
    let lower = norm.to_ascii_lowercase();

    if let Some(exp) = explicit_norm {
        let cmp_target = if cfg!(windows) { &lower } else { &norm };
        if cmp_target == exp || cmp_target.starts_with(&format!("{exp}/")) {
            return true;
        }
    }

    // Never match Electron helper bundles on macOS (`... Helper.app/Contents/MacOS/...`)
    // or bundled CLI sidecars (`.../resources/codex`, `claude-ssh-proxy`, etc.).
    if lower.contains("helper.app/") || lower.contains("helper (") || lower.contains("/resources/")
    {
        return false;
    }

    let file_name = lower.rsplit('/').next().unwrap_or("");
    match kind {
        DesktopKind::CodexApp => {
            if lower.ends_with("/codex.app/contents/macos/codex") {
                return true;
            }
            if lower.contains("/windowsapps/openai.codex_")
                && (file_name == "chatgpt.exe" || file_name == "codex.exe")
            {
                return true;
            }
            if (lower.contains("/programs/codex/") || lower.contains("/appdata/local/codex/"))
                && file_name == "codex.exe"
            {
                return true;
            }
            file_name == "codex-desktop"
                || file_name == "codex-desktop.exe"
                || file_name == "codex-app"
                || file_name == "codex-app.exe"
                || (file_name.starts_with("codex") && file_name.ends_with(".appimage"))
                || lower.ends_with("/opt/codex/codex")
        }
        DesktopKind::ClaudeDesktop => {
            if lower.ends_with("/claude.app/contents/macos/claude") {
                return true;
            }
            if (lower.contains("/windowsapps/claude_")
                || lower.contains("/anthropicclaude/")
                || lower.contains("/programs/claude/")
                || lower.contains("/programs/claude-desktop/")
                || lower.contains("/program files/claude/"))
                && file_name == "claude.exe"
            {
                return true;
            }
            file_name == "claude-desktop"
                || file_name == "claude-desktop.exe"
                || (file_name.starts_with("claude") && file_name.ends_with(".appimage"))
                || lower.ends_with("/opt/claude/claude")
        }
    }
}

#[cfg(windows)]
fn find_running_desktop_pids_windows(
    kind: DesktopKind,
    self_pid: u32,
    explicit_norm: Option<&str>,
) -> Vec<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    let mut matches = Vec::new();
    // SAFETY: `CreateToolhelp32Snapshot` returns a kernel snapshot handle closed before return;
    // `PROCESSENTRY32W` has `dwSize` initialized to `size_of::<PROCESSENTRY32W>()`.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
            return matches;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0);

        let mut has_item = Process32FirstW(snapshot, &raw mut entry) != 0;
        while has_item {
            let pid = entry.th32ProcessID;
            if pid != 0 && pid != self_pid {
                let proc_handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                if !proc_handle.is_null() {
                    let mut buf = [0u16; 1024];
                    let mut len = u32::try_from(buf.len()).unwrap_or(1024);
                    let ok =
                        QueryFullProcessImageNameW(proc_handle, 0, buf.as_mut_ptr(), &raw mut len);
                    let _ = CloseHandle(proc_handle);
                    if ok != 0 {
                        let full_path =
                            String::from_utf16_lossy(buf.get(..len as usize).unwrap_or(&[]));
                        if is_matching_desktop_exe_path(kind, &full_path, explicit_norm) {
                            matches.push(pid);
                        }
                    }
                }
            }
            has_item = Process32NextW(snapshot, &raw mut entry) != 0;
        }
        let _ = CloseHandle(snapshot);
    }
    matches
}

#[cfg(target_os = "linux")]
fn find_running_desktop_pids_linux(
    kind: DesktopKind,
    self_pid: u32,
    explicit_norm: Option<&str>,
) -> Vec<u32> {
    let mut matches = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return matches;
    };
    for entry in entries.flatten() {
        let Some(pid_str) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if pid == 0 || pid == self_pid {
            continue;
        }
        let proc_dir = entry.path();
        let cmdline = read_bounded_bytes(&proc_dir.join("cmdline"), 8192).unwrap_or_default();
        let cmdline_str = String::from_utf8_lossy(&cmdline);
        // Ignore Electron renderer/GPU/utility sub-processes.
        if cmdline_str.contains("--type=") {
            continue;
        }
        if let Ok(exe_target) = fs::read_link(proc_dir.join("exe")) {
            let exe_str = exe_target.to_string_lossy();
            let cleaned = exe_str.strip_suffix(" (deleted)").unwrap_or(&exe_str);
            if is_matching_desktop_exe_path(kind, cleaned, explicit_norm) {
                matches.push(pid);
                continue;
            }
            if is_linux_electron_or_appimage_main(kind, cleaned, &cmdline_str) {
                matches.push(pid);
                continue;
            }
        }
        if let Some(arg0) = cmdline_str.split('\0').next()
            && !arg0.is_empty()
            && is_matching_desktop_exe_path(kind, arg0, explicit_norm)
        {
            matches.push(pid);
        }
    }
    matches
}

#[cfg(any(target_os = "linux", test))]
#[must_use]
fn is_linux_electron_or_appimage_main(kind: DesktopKind, exe_path: &str, cmdline: &str) -> bool {
    let exe_lower = exe_path.replace('\\', "/").to_ascii_lowercase();
    if exe_lower.contains("/tmp/.mount_") {
        return match kind {
            DesktopKind::CodexApp => exe_lower.contains("/.mount_codex"),
            DesktopKind::ClaudeDesktop => exe_lower.contains("/.mount_claude"),
        };
    }
    let exe_name = exe_lower.rsplit('/').next().unwrap_or("");
    let is_electron_host = exe_name == "electron"
        || exe_name
            .strip_prefix("electron")
            .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()));
    if !is_electron_host {
        return false;
    }
    let cmd_lower = cmdline.replace('\\', "/").to_ascii_lowercase();
    match kind {
        DesktopKind::CodexApp => {
            cmd_lower.contains("/codex-desktop/") || cmd_lower.contains("/codex-app/")
        }
        DesktopKind::ClaudeDesktop => cmd_lower.contains("/claude-desktop/"),
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn find_running_desktop_pids_ps(
    kind: DesktopKind,
    self_pid: u32,
    explicit_norm: Option<&str>,
) -> Vec<u32> {
    let Ok(out) = std::process::Command::new("/bin/ps")
        .args(["-axww", "-o", "pid=,comm="])
        .stdin(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut matches = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some((pid_part, comm_part)) = trimmed.split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pid) = pid_part.trim().parse::<u32>() else {
            continue;
        };
        if pid == 0 || pid == self_pid {
            continue;
        }
        let comm = comm_part.trim();
        if is_matching_desktop_exe_path(kind, comm, explicit_norm) {
            matches.push(pid);
        }
    }
    matches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appx_manifest_parser_selects_primary_gui_application() {
        let claude_manifest = r#"
            <Package>
              <Applications>
                <Application Id="SshProxy" Executable="app\resources\claude-ssh-proxy.exe" />
                <Application Id="Claude" Executable="app\Claude.exe" EntryPoint="Windows.FullTrustApplication" />
              </Applications>
            </Package>
        "#;
        assert_eq!(
            parse_appx_manifest_primary_app(claude_manifest, &["Claude", "App"]),
            Some(("Claude".to_owned(), "app\\Claude.exe".to_owned()))
        );

        let codex_manifest = r#"
            <Package>
              <Applications>
                <Application Id="App" Executable="app/ChatGPT.exe" EntryPoint="Windows.FullTrustApplication" />
              </Applications>
            </Package>
        "#;
        assert_eq!(
            parse_appx_manifest_primary_app(codex_manifest, &["App"]),
            Some(("App".to_owned(), "app/ChatGPT.exe".to_owned()))
        );
    }

    #[test]
    fn windows_command_line_quoting_roundtrips_spaces_and_quotes() {
        assert_eq!(quote_windows_arg("--flag=simple"), "--flag=simple");
        assert_eq!(
            join_windows_command_line_args(&[
                "--flag=simple".to_owned(),
                r"--user-data-dir=C:\Users\Alice Smith\.liftgate\codex-app".to_owned(),
            ]),
            r#"--flag=simple "--user-data-dir=C:\Users\Alice Smith\.liftgate\codex-app""#
        );
        assert_eq!(quote_windows_arg(r#"a\"b"#), r#""a\\\"b""#);
    }

    #[test]
    fn desktop_exe_matcher_distinguishes_gui_apps_from_cli_binaries() {
        // CLI binaries must NEVER be mistaken for desktop GUI apps.
        assert!(!is_matching_desktop_exe_path(
            DesktopKind::ClaudeDesktop,
            "/Users/alice/.local/bin/claude",
            None,
        ));
        assert!(!is_matching_desktop_exe_path(
            DesktopKind::ClaudeDesktop,
            r"C:\Users\alice\.local\bin\claude.exe",
            None,
        ));
        assert!(!is_matching_desktop_exe_path(
            DesktopKind::CodexApp,
            r"C:\Program Files\WindowsApps\OpenAI.Codex_1.0\app\resources\codex.exe",
            None,
        ));
        assert!(!is_matching_desktop_exe_path(
            DesktopKind::ClaudeDesktop,
            "/Applications/Claude.app/Contents/Frameworks/Claude Helper (Renderer).app/Contents/MacOS/Claude Helper (Renderer)",
            None,
        ));

        // Genuine GUI binaries across macOS, Windows (MSIX & Squirrel), and Linux MUST match.
        assert!(is_matching_desktop_exe_path(
            DesktopKind::ClaudeDesktop,
            "/Applications/Claude.app/Contents/MacOS/Claude",
            None,
        ));
        assert!(is_matching_desktop_exe_path(
            DesktopKind::ClaudeDesktop,
            r"C:\Program Files\WindowsApps\Claude_4.29.0.0_x64__pzs8sxrjxfjjc\app\Claude.exe",
            None,
        ));
        assert!(is_matching_desktop_exe_path(
            DesktopKind::ClaudeDesktop,
            r"C:\Users\alice\AppData\Local\AnthropicClaude\app-1.2.3\claude.exe",
            None,
        ));
        assert!(is_matching_desktop_exe_path(
            DesktopKind::ClaudeDesktop,
            "/usr/bin/claude-desktop",
            None,
        ));
        assert!(is_matching_desktop_exe_path(
            DesktopKind::CodexApp,
            "/Applications/Codex.app/Contents/MacOS/Codex",
            None,
        ));
        assert!(is_matching_desktop_exe_path(
            DesktopKind::CodexApp,
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.1_x64__2p2nqsd0c76g0\app\ChatGPT.exe",
            None,
        ));
        assert!(is_matching_desktop_exe_path(
            DesktopKind::CodexApp,
            "/usr/bin/codex-desktop",
            None,
        ));
        assert!(is_linux_electron_or_appimage_main(
            DesktopKind::ClaudeDesktop,
            "/usr/lib/electron34/electron",
            "/usr/lib/electron34/electron\0/usr/lib/claude-desktop/app.asar\0",
        ));
        assert!(is_linux_electron_or_appimage_main(
            DesktopKind::CodexApp,
            "/tmp/.mount_CodexAbCdEf/usr/bin/app",
            "/tmp/.mount_CodexAbCdEf/usr/bin/app\0",
        ));
        assert!(!is_linux_electron_or_appimage_main(
            DesktopKind::ClaudeDesktop,
            "/usr/bin/node",
            "/usr/bin/node\0/usr/local/bin/claude\0",
        ));
    }

    #[test]
    fn cross_platform_claude_desktop_paths_cover_macos_linux_and_windows_msix() {
        let mac = macos_claude_desktop_paths(Path::new("/Users/alice"));
        assert_eq!(
            mac.normal_config_path,
            PathBuf::from(
                "/Users/alice/Library/Application Support/Claude/claude_desktop_config.json"
            )
        );
        assert_eq!(
            mac.threep_dir,
            PathBuf::from("/Users/alice/Library/Application Support/Claude-3p")
        );

        let linux_xdg = linux_claude_desktop_paths(
            Path::new("/home/alice"),
            Some(Path::new("/custom/xdg")),
            false,
        );
        assert_eq!(
            linux_xdg.normal_config_path,
            PathBuf::from("/custom/xdg/Claude/claude_desktop_config.json")
        );
        let linux_flatpak = linux_claude_desktop_paths(
            Path::new("/home/alice"),
            Some(Path::new("/home/alice/.var/app/x/config")),
            true,
        );
        assert_eq!(
            linux_flatpak.normal_config_path,
            PathBuf::from("/home/alice/.config/Claude/claude_desktop_config.json")
        );

        let tmp = std::env::temp_dir().join(format!("lg-win-paths-{}", Uuid::new_v4()));
        let roaming = tmp.join("Roaming");
        let local = tmp.join("Local");
        let msix_cache = local
            .join("Packages")
            .join("Claude_pzs8sxrjxfjjc")
            .join("LocalCache");
        fs::create_dir_all(msix_cache.join("Roaming")).unwrap();
        fs::create_dir_all(msix_cache.join("Local")).unwrap();

        let sets = windows_claude_desktop_path_sets(&roaming, &local);
        assert_eq!(sets.len(), 4);

        let fp_app = tmp
            .join(".var")
            .join("app")
            .join("io.github.aaddrick.claude-desktop-debian")
            .join("config");
        fs::create_dir_all(&fp_app).unwrap();
        let fp_sets = linux_flatpak_claude_desktop_path_sets(&tmp);
        assert_eq!(fp_sets.len(), 1);
        assert_eq!(fp_sets[0].threep_dir, fp_app.join("Claude-3p"));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn seed_ephemeral_codex_home_preserves_source_dir_untouched() {
        let tmp = std::env::temp_dir().join(format!("lg-codex-app-seed-{}", Uuid::new_v4()));
        let fake_user_codex = tmp.join("dot-codex");
        let ephemeral_home = tmp.join("ephemeral-home");
        fs::create_dir_all(&fake_user_codex).unwrap();

        let original_config = "model = \"gpt-5\"\n\n[mcp_servers.demo]\ncommand = \"npx\"\n";
        fs::write(fake_user_codex.join("config.toml"), original_config).unwrap();
        fs::write(fake_user_codex.join("AGENTS.md"), "# User rules\n").unwrap();

        seed_ephemeral_codex_home(
            &ephemeral_home,
            Some(&fake_user_codex),
            "http://127.0.0.1:8787",
            "gemini-4-carbon-high",
            "agy-local",
            false,
            b"{\"models\":[]}\n",
            false,
            false,
            false,
        )
        .unwrap();

        // Original `~/.codex` must remain 100% untouched (zero pollution).
        assert_eq!(
            fs::read_to_string(fake_user_codex.join("config.toml")).unwrap(),
            original_config
        );
        assert!(!fake_user_codex.join("agy-models.json").exists());
        assert!(!fake_user_codex.join("auth.json").exists());

        // Ephemeral CODEX_HOME gets merged config, catalog, synthetic auth.json, and AGENTS.md.
        let merged = fs::read_to_string(ephemeral_home.join("config.toml")).unwrap();
        assert!(merged.contains("model_provider = \"agy\""));
        assert!(merged.contains("model = \"gemini-4-carbon-high\""));
        assert!(merged.contains("[mcp_servers.demo]"));
        assert!(ephemeral_home.join("agy-models.json").is_file());
        assert!(ephemeral_home.join("auth.json").is_file());
        assert!(ephemeral_home.join("AGENTS.md").is_file());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn claude_desktop_3p_session_surgically_restores_and_keeps_concurrent_mcp_edits() {
        let tmp = std::env::temp_dir().join(format!("lg-claude-3p-{}", Uuid::new_v4()));
        let paths = ClaudeDesktopPathSet::from_dirs(tmp.join("Claude"), tmp.join("Claude-3p"));
        fs::create_dir_all(&paths.normal_dir).unwrap();
        fs::write(
            &paths.normal_config_path,
            r#"{"mcpServers":{"fs":{"command":"node"}}}"#,
        )
        .unwrap();

        let path_sets = vec![paths.clone()];
        let backups = snapshot_claude_desktop_path_sets(&path_sets).unwrap();
        let profile = build_claude_desktop_3p_profile(
            "http://127.0.0.1:8787/claude-desktop",
            "agy-local",
            "gemini-4-carbon-high",
            &[AvailableModel {
                model_id: "gemini-4-carbon-high".to_owned(),
                name: "Gemini 4 Carbon (High)".to_owned(),
            }],
            false,
        );
        apply_claude_desktop_3p_profile(&path_sets, &profile).unwrap();

        // Verify 3P config is active and route IDs satisfy Claude Desktop's strict whitelist.
        let active_cfg = read_json_object_or_empty(&paths.normal_config_path).unwrap();
        assert_eq!(active_cfg.get("deploymentMode"), Some(&json!("3p")));
        assert!(paths.profile_path.is_file());
        let written_profile = read_json_object_or_empty(&paths.profile_path).unwrap();
        let first_model_name = written_profile
            .get("inferenceModels")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|m| m.get("name"))
            .and_then(Value::as_str)
            .unwrap();
        assert!(liftgate_types::model::is_claude_desktop_safe_model_id(
            first_model_name
        ));

        // Simulate the user adding a new MCP server inside Claude Desktop while running!
        patch_json_key_atomic(
            &paths.normal_config_path,
            "preferences",
            Some(json!({"coworkScheduledTasksEnabled": true})),
            false,
        )
        .unwrap();

        // Restore session and assert zero Liftgate residue while keeping user's concurrent edit.
        restore_claude_desktop_backups(&backups);

        assert!(!paths.threep_dir.exists());
        let restored_cfg = read_json_object_or_empty(&paths.normal_config_path).unwrap();
        assert!(!restored_cfg.contains_key("deploymentMode"));
        assert!(restored_cfg.contains_key("mcpServers"));
        assert_eq!(
            restored_cfg.get("preferences"),
            Some(&json!({"coworkScheduledTasksEnabled": true}))
        );

        let _ = fs::remove_dir_all(&tmp);
    }
}
