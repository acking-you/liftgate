//! Self-upgrade command (`lg upgrade` / `liftgate upgrade`) backed by GitHub Releases.
//!
//! Security & reliability guarantees:
//! - Queries GitHub Releases REST API (`releases/latest` or `releases/tags/<tag>`) with
//!   automatic redirect fallback when unauthenticated REST API calls hit HTTP 403/429 rate limits.
//! - Strictly enforces `https://` for all non-loopback network traffic and passes any
//!   `GITHUB_TOKEN` / `GH_TOKEN` via `curl --config -` stdin so tokens never appear in `argv`.
//! - Verifies downloaded archives fail-closed against `SHA256SUMS` (and GitHub's native
//!   `sha256:` asset digest when present) before unpacking.
//! - Unpacks `.tar.gz` and `.zip` archives in pure safe Rust with strict per-file byte caps
//!   and filename-only matching (immune to `../` path traversal and symlink entries).
//! - Smoke-tests the staged binary (`--version`), gracefully stops & restarts any active
//!   managed background gateway daemon, and atomically replaces both `lg` and `liftgate`
//!   (with automatic rollback and Windows running-executable rename handling).

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tar::Archive;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, BufWriter};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::timeout;
use uuid::Uuid;
use zip::ZipArchive;

use crate::cli::UpgradeArgs;
use crate::daemon;
use crate::error::{ArchiveError, CliError, TransferError, UpgradeInputError};
use crate::state::StatePaths;

const CURRENT_VERSION_RAW: &str = env!("CARGO_PKG_VERSION");
const COMPILE_TARGET: &str = env!("LIFTGATE_BUILD_TARGET");

const MAX_METADATA_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_BINARY_EXTRACT_BYTES: u64 = 512 * 1024 * 1024;
const FETCH_METADATA_TIMEOUT: Duration = Duration::from_secs(25);
const FETCH_ARCHIVE_TIMEOUT: Duration = Duration::from_secs(600);
const BACKUP_MARKER: &str = ".old-upgrade";
const PROGRESS_EMIT_INTERVAL: Duration = Duration::from_millis(60);
const MAX_CURL_STDERR_BYTES: usize = 64 * 1024;
const MAX_HTTP_HEADER_FILE_BYTES: u64 = 256 * 1024;

/// High-level lifecycle stage of an active Liftgate self-upgrade or `agy` install/update.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferStage {
    Checking,
    Downloading,
    Verifying,
    Extracting,
    Testing,
    Installing,
}

impl TransferStage {
    /// Short human-readable stage title.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Checking => "Checking",
            Self::Downloading => "Downloading",
            Self::Verifying => "Verifying",
            Self::Extracting => "Extracting",
            Self::Testing => "Testing",
            Self::Installing => "Installing",
        }
    }

    /// 1-based step index across the 6-stage upgrade lifecycle.
    #[must_use]
    pub const fn step_index(self) -> u8 {
        match self {
            Self::Checking => 1,
            Self::Downloading => 2,
            Self::Verifying => 3,
            Self::Extracting => 4,
            Self::Testing => 5,
            Self::Installing => 6,
        }
    }
}

/// Real-time progress snapshot emitted during verified downloads and binary installations.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TransferProgress {
    pub stage: TransferStage,
    pub message: String,
    pub asset_name: Option<String>,
    pub target_version: Option<String>,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub bytes_per_sec: f64,
    pub eta_secs: Option<u64>,
}

impl TransferProgress {
    /// Creates a checking-stage initial progress snapshot.
    #[must_use]
    pub fn checking(message: impl Into<String>) -> Self {
        Self {
            stage: TransferStage::Checking,
            message: message.into(),
            asset_name: None,
            target_version: None,
            downloaded_bytes: 0,
            total_bytes: None,
            bytes_per_sec: 0.0,
            eta_secs: None,
        }
    }

    /// Creates a non-download lifecycle stage snapshot carrying optional asset/version metadata.
    #[must_use]
    pub fn stage(
        stage: TransferStage,
        message: impl Into<String>,
        asset_name: Option<String>,
        target_version: Option<String>,
    ) -> Self {
        Self {
            stage,
            message: message.into(),
            asset_name,
            target_version,
            downloaded_bytes: 0,
            total_bytes: None,
            bytes_per_sec: 0.0,
            eta_secs: None,
        }
    }

    /// Returns download completion ratio in `0.0..=1.0` when total byte length is known.
    #[must_use]
    pub fn progress_fraction(&self) -> Option<f64> {
        let total = self.total_bytes.filter(|&t| t > 0)?;
        Some((self.downloaded_bytes as f64 / total as f64).clamp(0.0, 1.0))
    }
}

pub(crate) struct DownloadProgressContext<'a> {
    pub tx: &'a watch::Sender<TransferProgress>,
    pub asset_name: String,
    pub target_version: Option<String>,
    pub known_total_bytes: Option<u64>,
}

pub(crate) fn emit_progress(tx: Option<&watch::Sender<TransferProgress>>, snap: TransferProgress) {
    if let Some(sender) = tx {
        let _ = sender.send_replace(snap);
    }
}

struct SpeedTracker {
    started_at: Instant,
    samples: VecDeque<(Instant, u64)>,
}

impl SpeedTracker {
    fn new(now: Instant) -> Self {
        let mut samples = VecDeque::with_capacity(32);
        samples.push_back((now, 0));
        Self {
            started_at: now,
            samples,
        }
    }

    fn record(&mut self, now: Instant, downloaded_bytes: u64) -> f64 {
        const WINDOW: Duration = Duration::from_secs(2);
        const MIN_SAMPLE_GAP: Duration = Duration::from_millis(80);

        let should_push = self
            .samples
            .back()
            .is_none_or(|(last_t, _)| now.duration_since(*last_t) >= MIN_SAMPLE_GAP);
        if should_push {
            self.samples.push_back((now, downloaded_bytes));
        }
        while self.samples.len() > 2
            && self
                .samples
                .front()
                .is_some_and(|(t, _)| now.duration_since(*t) > WINDOW)
        {
            let _ = self.samples.pop_front();
        }

        if let Some(&(oldest_t, oldest_b)) = self.samples.front() {
            let dt = now.duration_since(oldest_t).as_secs_f64();
            if dt >= 0.15 {
                return downloaded_bytes.saturating_sub(oldest_b) as f64 / dt;
            }
        }
        let elapsed = now.duration_since(self.started_at).as_secs_f64();
        if elapsed > 0.01 {
            downloaded_bytes as f64 / elapsed
        } else {
            0.0
        }
    }
}

fn estimate_eta_secs(downloaded: u64, total: Option<u64>, rate_bps: f64) -> Option<u64> {
    let total = total?;
    if downloaded >= total {
        return Some(0);
    }
    if !rate_bps.is_finite() || rate_bps < 1.0 {
        return None;
    }
    let remaining = total.saturating_sub(downloaded) as f64;
    let secs = (remaining / rate_bps).ceil();
    if secs.is_finite() && secs >= 0.0 {
        Some(secs.min(u64::MAX as f64) as u64)
    } else {
        None
    }
}

struct TempFileGuard {
    path: PathBuf,
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PreId {
    Numeric(u64),
    Alpha(String),
}

impl Ord for PreId {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Numeric(a), Self::Numeric(b)) => a.cmp(b),
            // Per SemVer 2.0.0 §11.4.3: numeric identifiers always have lower precedence than non-numeric.
            (Self::Numeric(_), Self::Alpha(_)) => Ordering::Less,
            (Self::Alpha(_), Self::Numeric(_)) => Ordering::Greater,
            (Self::Alpha(a), Self::Alpha(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for PreId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Parsed SemVer 2.0.0 release version (`vMAJOR.MINOR.PATCH[-PRERELEASE][+BUILD]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pre: Vec<PreId>,
}

impl SemVer {
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        let without_v = trimmed
            .strip_prefix('v')
            .or_else(|| trimmed.strip_prefix('V'))
            .unwrap_or(trimmed);
        let without_build = without_v.split('+').next().unwrap_or(without_v);
        let (core, pre_raw) = match without_build.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (without_build, None),
        };
        let mut nums = core.split('.');
        let major = nums.next()?.parse::<u64>().ok()?;
        let minor = nums.next()?.parse::<u64>().ok()?;
        let patch = nums.next()?.parse::<u64>().ok()?;
        if nums.next().is_some() {
            return None;
        }
        let mut pre = Vec::new();
        if let Some(p) = pre_raw {
            if p.is_empty() {
                return None;
            }
            for part in p.split('.') {
                if part.is_empty() {
                    return None;
                }
                if part.bytes().all(|b| b.is_ascii_digit()) {
                    pre.push(PreId::Numeric(part.parse::<u64>().ok()?));
                } else {
                    pre.push(PreId::Alpha(part.to_owned()));
                }
            }
        }
        Some(Self {
            major,
            minor,
            patch,
            pre,
        })
    }
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> Ordering {
        match (
            self.major.cmp(&other.major),
            self.minor.cmp(&other.minor),
            self.patch.cmp(&other.patch),
        ) {
            (Ordering::Equal, Ordering::Equal, Ordering::Equal) => {
                match (self.pre.is_empty(), other.pre.is_empty()) {
                    (true, true) => Ordering::Equal,
                    // Final release has higher precedence than pre-release.
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => self.pre.cmp(&other.pre),
                }
            }
            (Ordering::Equal, Ordering::Equal, p) => p,
            (Ordering::Equal, m, _) => m,
            (maj, _, _) => maj,
        }
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Deserialize)]
struct GithubReleaseResponse {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Clone, Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Clone, Debug)]
struct ResolvedReleasePlan {
    repo: String,
    tag: String,
    release_title: String,
    release_url: String,
    published_at: Option<String>,
    prerelease: bool,
    notes_excerpt: Option<String>,
    target_triple: String,
    archive_name: String,
    archive_url: String,
    archive_size: Option<u64>,
    api_sha256: Option<String>,
    checksums_url: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct UpgradeOutcome {
    pub status: &'static str,
    pub current_version: String,
    pub target_version: String,
    pub tag: String,
    pub update_available: bool,
    pub upgraded: bool,
    pub target_triple: String,
    pub archive_name: String,
    pub archive_url: String,
    pub archive_size_bytes: Option<u64>,
    pub sha256: Option<String>,
    pub release_url: String,
    pub published_at: Option<String>,
    pub installed_binaries: Vec<String>,
    pub daemon_restarted: bool,
    #[serde(skip_serializing)]
    pub repo: String,
    #[serde(skip_serializing)]
    pub release_title: String,
    #[serde(skip_serializing)]
    pub prerelease: bool,
    #[serde(skip_serializing)]
    pub notes_excerpt: Option<String>,
}

struct StageDirGuard {
    path: PathBuf,
}

impl StageDirGuard {
    fn create(state_paths: &StatePaths) -> Result<Self, CliError> {
        let base = state_paths.ensure_ephemeral_dir()?;
        let path = base.join(format!(
            "upgrade-stage-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&path).map_err(|source| CliError::StateIo {
            path: path.clone(),
            source,
        })?;
        Ok(Self { path })
    }
}

impl Drop for StageDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Best-effort startup cleanup for Windows `.old-upgrade` backup files left behind by
/// a previous in-place self-upgrade after the old process exited.
pub fn sweep_stale_backups() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    sweep_stale_backups_in_dir(dir);
}

fn sweep_stale_backups_in_dir(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.contains(BACKUP_MARKER) {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Returns the packaged bundle description when `current_exe` is running from inside a
/// read-only or code-signed desktop bundle (such as a Linux AppImage mount or macOS `.app`)
/// where raw-binary in-place replacement would fail or corrupt the bundle.
#[must_use]
pub fn detect_packaged_bundle_kind(current_exe: &Path) -> Option<&'static str> {
    detect_packaged_bundle_kind_with_env(
        current_exe,
        std::env::var_os("APPIMAGE").as_deref(),
        std::env::var_os("APPDIR").as_deref(),
    )
}

fn detect_packaged_bundle_kind_with_env(
    current_exe: &Path,
    appimage: Option<&OsStr>,
    appdir: Option<&OsStr>,
) -> Option<&'static str> {
    let normalized = current_exe.to_string_lossy().replace('\\', "/");
    for segment in normalized.split('/') {
        if segment.starts_with(".mount_") || segment.starts_with("appimage_extracted_") {
            return Some("an AppImage runtime mount");
        }
    }

    if let Some(raw_appdir) = appdir.filter(|v| !v.is_empty()) {
        let appdir_path = Path::new(raw_appdir);
        if current_exe.starts_with(appdir_path) {
            return Some("an AppImage runtime mount");
        }
        if let (Ok(canon_exe), Ok(canon_dir)) =
            (fs::canonicalize(current_exe), fs::canonicalize(appdir_path))
            && canon_exe.starts_with(&canon_dir)
        {
            return Some("an AppImage runtime mount");
        }
    }

    if let Some(raw_appimage) = appimage.filter(|v| !v.is_empty()) {
        let outer = Path::new(raw_appimage);
        if current_exe != outer && normalized.contains("/usr/bin/") {
            return Some("an AppImage runtime mount");
        }
    }

    if let Some(macos_dir) = current_exe.parent()
        && macos_dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("MacOS"))
        && let Some(contents_dir) = macos_dir.parent()
        && contents_dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("Contents"))
        && let Some(bundle_dir) = contents_dir.parent()
        && bundle_dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| {
                Path::new(n)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
            })
    {
        return Some("a signed macOS .app bundle");
    }

    None
}

pub async fn execute_upgrade(
    state_paths: &StatePaths,
    args: &UpgradeArgs,
    current_exe: &Path,
) -> Result<UpgradeOutcome, CliError> {
    execute_upgrade_with_progress(state_paths, args, current_exe, None).await
}

pub(crate) enum IdleSelfUpgradeOutcome {
    UpToDate {
        current_version: String,
        latest_version: String,
    },
    SkippedPackagedBundle {
        current_version: String,
        bundle_kind: &'static str,
    },
    DeferredBusy {
        current_version: String,
        target_version: String,
    },
    Installed {
        from_version: String,
        to_version: String,
        liftgate_exe: PathBuf,
    },
}

/// Checks GitHub Releases while idle, stages and verifies `SHA256SUMS` + `--version`, re-checks
/// `is_idle` right before modifying disk binaries, and atomically replaces `liftgate` and `lg`
/// without stopping the caller process (allowing a managed daemon to spawn a detached respawn helper).
pub(crate) async fn stage_and_install_upgrade_if_idle<F>(
    state_paths: &StatePaths,
    args: &UpgradeArgs,
    current_exe: &Path,
    mut is_idle: F,
) -> Result<IdleSelfUpgradeOutcome, CliError>
where
    F: FnMut() -> bool,
{
    validate_repo_slug(&args.repo)?;
    if let Some(bundle_kind) = detect_packaged_bundle_kind(current_exe) {
        return Ok(IdleSelfUpgradeOutcome::SkippedPackagedBundle {
            current_version: CURRENT_VERSION_RAW.to_owned(),
            bundle_kind,
        });
    }
    let desired_target = args
        .target
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(COMPILE_TARGET);
    validate_target_triple(desired_target)?;

    let plan = resolve_release_plan(
        &args.repo,
        None,
        desired_target,
        args.proxy.as_deref(),
        args.base_url.as_deref(),
    )
    .await?;

    let target_ver_str = plan
        .tag
        .strip_prefix('v')
        .or_else(|| plan.tag.strip_prefix('V'))
        .unwrap_or(&plan.tag)
        .to_owned();
    let current_sem = SemVer::parse(CURRENT_VERSION_RAW);
    let target_sem = SemVer::parse(&plan.tag);
    let cmp = match (&current_sem, &target_sem) {
        (Some(cur), Some(tgt)) => cur.cmp(tgt),
        _ if CURRENT_VERSION_RAW == target_ver_str => Ordering::Equal,
        _ => Ordering::Less,
    };
    if cmp != Ordering::Less {
        return Ok(IdleSelfUpgradeOutcome::UpToDate {
            current_version: CURRENT_VERSION_RAW.to_owned(),
            latest_version: target_ver_str,
        });
    }
    if !is_idle() {
        return Ok(IdleSelfUpgradeOutcome::DeferredBusy {
            current_version: CURRENT_VERSION_RAW.to_owned(),
            target_version: target_ver_str,
        });
    }

    let stage = StageDirGuard::create(state_paths)?;
    let sums_bytes = fetch_bytes_bounded(
        &plan.checksums_url,
        MAX_METADATA_BYTES,
        FETCH_METADATA_TIMEOUT,
        args.proxy.as_deref(),
        false,
    )
    .await?;
    let sums_text = String::from_utf8_lossy(&sums_bytes);
    let expected_sha256 =
        lookup_sha256_in_sums(&sums_text, &plan.archive_name).ok_or_else(|| {
            CliError::UpgradeArchive {
                asset: plan.archive_name.clone(),
                source: ArchiveError::MissingChecksumEntry {
                    tag: plan.tag.clone(),
                    asset: plan.archive_name.clone(),
                },
            }
        })?;
    if let Some(api_digest) = &plan.api_sha256
        && !api_digest.eq_ignore_ascii_case(&expected_sha256)
    {
        return Err(CliError::UpgradeChecksumMismatch {
            asset: plan.archive_name.clone(),
            expected: expected_sha256,
            actual: api_digest.clone(),
        });
    }

    let archive_path = stage.path.join(&plan.archive_name);
    download_url_to_path_with_progress(
        &plan.archive_url,
        &archive_path,
        MAX_ARCHIVE_BYTES,
        FETCH_ARCHIVE_TIMEOUT,
        args.proxy.as_deref(),
        None,
    )
    .await?;

    let actual_sha256 = compute_file_sha256(&archive_path)?;
    if !actual_sha256.eq_ignore_ascii_case(&expected_sha256) {
        return Err(CliError::UpgradeChecksumMismatch {
            asset: plan.archive_name.clone(),
            expected: expected_sha256,
            actual: actual_sha256,
        });
    }

    let staged = unpack_release_binaries(&archive_path, &plan.archive_name, &stage.path)?;
    if plan.target_triple == COMPILE_TARGET {
        smoke_check_staged_binary(&staged.liftgate_bin).await?;
    }
    let install_targets = plan_install_targets(current_exe, &staged, &plan.target_triple);
    verify_install_targets_writable(&install_targets)?;

    if !is_idle() {
        return Ok(IdleSelfUpgradeOutcome::DeferredBusy {
            current_version: CURRENT_VERSION_RAW.to_owned(),
            target_version: target_ver_str,
        });
    }

    let mut rollback_copies: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(install_targets.len());
    for (idx, (staged_src, dest_path)) in install_targets.iter().enumerate() {
        let prev_copy = stage.path.join(format!("rollback-{idx}.bin"));
        let had_prev =
            dest_path.is_file() && copy_executable_with_sync(dest_path, &prev_copy).is_ok();
        if let Err(err) = replace_executable_atomic(staged_src, dest_path) {
            for (saved_src, orig_dest) in rollback_copies.iter().rev() {
                let _ = replace_executable_atomic(saved_src, orig_dest);
            }
            return Err(err);
        }
        if had_prev {
            rollback_copies.push((prev_copy, dest_path.clone()));
        }
    }

    let liftgate_exe = install_targets
        .iter()
        .find(|(_, dest)| {
            dest.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("liftgate"))
        })
        .or_else(|| install_targets.first())
        .map_or_else(|| current_exe.to_path_buf(), |(_, dest)| dest.clone());

    Ok(IdleSelfUpgradeOutcome::Installed {
        from_version: CURRENT_VERSION_RAW.to_owned(),
        to_version: target_ver_str,
        liftgate_exe,
    })
}

pub async fn execute_upgrade_with_progress(
    state_paths: &StatePaths,
    args: &UpgradeArgs,
    current_exe: &Path,
    progress: Option<watch::Sender<TransferProgress>>,
) -> Result<UpgradeOutcome, CliError> {
    let progress_tx = progress.as_ref();
    validate_repo_slug(&args.repo)?;
    let requested_tag = match args
        .version
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw) => Some(normalize_tag_input(raw)?),
        None => None,
    };
    let desired_target = args
        .target
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(COMPILE_TARGET);
    validate_target_triple(desired_target)?;

    if !args.check
        && let Some(bundle_kind) = detect_packaged_bundle_kind(current_exe)
    {
        let release_url = match &requested_tag {
            Some(tag) => format!("https://github.com/{}/releases/tag/{tag}", args.repo.trim()),
            None => format!("https://github.com/{}/releases", args.repo.trim()),
        };
        return Err(CliError::UpgradePackagedBundle {
            bundle_kind,
            exe_path: current_exe.to_path_buf(),
            release_url,
        });
    }

    emit_progress(
        progress_tx,
        TransferProgress::checking(format!("Querying GitHub Releases ({})…", args.repo)),
    );
    let plan = resolve_release_plan(
        &args.repo,
        requested_tag.as_deref(),
        desired_target,
        args.proxy.as_deref(),
        args.base_url.as_deref(),
    )
    .await?;

    let target_ver_str = plan
        .tag
        .strip_prefix('v')
        .or_else(|| plan.tag.strip_prefix('V'))
        .unwrap_or(&plan.tag)
        .to_owned();
    let current_sem = SemVer::parse(CURRENT_VERSION_RAW);
    let target_sem = SemVer::parse(&plan.tag);

    let cmp = match (&current_sem, &target_sem) {
        (Some(cur), Some(tgt)) => cur.cmp(tgt),
        _ if CURRENT_VERSION_RAW == target_ver_str => Ordering::Equal,
        _ => Ordering::Less,
    };
    let update_available = cmp == Ordering::Less;

    if args.check {
        let status = match cmp {
            Ordering::Less => "update_available",
            Ordering::Equal => "up_to_date",
            Ordering::Greater => "ahead_of_release",
        };
        return Ok(UpgradeOutcome {
            status,
            current_version: CURRENT_VERSION_RAW.to_owned(),
            target_version: target_ver_str,
            tag: plan.tag,
            update_available,
            upgraded: false,
            target_triple: plan.target_triple,
            archive_name: plan.archive_name,
            archive_url: plan.archive_url,
            archive_size_bytes: plan.archive_size,
            sha256: plan.api_sha256,
            release_url: plan.release_url,
            published_at: plan.published_at,
            installed_binaries: Vec::new(),
            daemon_restarted: false,
            repo: plan.repo,
            release_title: plan.release_title,
            prerelease: plan.prerelease,
            notes_excerpt: plan.notes_excerpt,
        });
    }

    // Skip download when already up-to-date (or ahead on latest) unless `--force` or explicit `--version` downgrade.
    if !args.force {
        if cmp == Ordering::Equal {
            return Ok(UpgradeOutcome {
                status: "up_to_date",
                current_version: CURRENT_VERSION_RAW.to_owned(),
                target_version: target_ver_str,
                tag: plan.tag,
                update_available: false,
                upgraded: false,
                target_triple: plan.target_triple,
                archive_name: plan.archive_name,
                archive_url: plan.archive_url,
                archive_size_bytes: plan.archive_size,
                sha256: plan.api_sha256,
                release_url: plan.release_url,
                published_at: plan.published_at,
                installed_binaries: Vec::new(),
                daemon_restarted: false,
                repo: plan.repo,
                release_title: plan.release_title,
                prerelease: plan.prerelease,
                notes_excerpt: plan.notes_excerpt,
            });
        }
        if cmp == Ordering::Greater && requested_tag.is_none() {
            return Ok(UpgradeOutcome {
                status: "ahead_of_release",
                current_version: CURRENT_VERSION_RAW.to_owned(),
                target_version: target_ver_str,
                tag: plan.tag,
                update_available: false,
                upgraded: false,
                target_triple: plan.target_triple,
                archive_name: plan.archive_name,
                archive_url: plan.archive_url,
                archive_size_bytes: plan.archive_size,
                sha256: plan.api_sha256,
                release_url: plan.release_url,
                published_at: plan.published_at,
                installed_binaries: Vec::new(),
                daemon_restarted: false,
                repo: plan.repo,
                release_title: plan.release_title,
                prerelease: plan.prerelease,
                notes_excerpt: plan.notes_excerpt,
            });
        }
    }

    let stage = StageDirGuard::create(state_paths)?;
    let (staged, verified_sha256) =
        async {
            emit_progress(
                progress_tx,
                TransferProgress::stage(
                    TransferStage::Checking,
                    format!("Fetching SHA256SUMS for {}…", plan.tag),
                    Some(plan.archive_name.clone()),
                    Some(plan.tag.clone()),
                ),
            );
            let sums_bytes = fetch_bytes_bounded(
                &plan.checksums_url,
                MAX_METADATA_BYTES,
                FETCH_METADATA_TIMEOUT,
                args.proxy.as_deref(),
                false,
            )
            .await?;
            let sums_text = String::from_utf8_lossy(&sums_bytes);
            let expected_sha256 = lookup_sha256_in_sums(&sums_text, &plan.archive_name)
                .ok_or_else(|| CliError::UpgradeArchive {
                    asset: plan.archive_name.clone(),
                    source: ArchiveError::MissingChecksumEntry {
                        tag: plan.tag.clone(),
                        asset: plan.archive_name.clone(),
                    },
                })?;

            if let Some(api_digest) = &plan.api_sha256
                && !api_digest.eq_ignore_ascii_case(&expected_sha256)
            {
                return Err(CliError::UpgradeChecksumMismatch {
                    asset: plan.archive_name.clone(),
                    expected: expected_sha256,
                    actual: api_digest.clone(),
                });
            }

            let archive_path = stage.path.join(&plan.archive_name);
            let dl_ctx = progress_tx.map(|tx| DownloadProgressContext {
                tx,
                asset_name: plan.archive_name.clone(),
                target_version: Some(plan.tag.clone()),
                known_total_bytes: plan.archive_size,
            });
            download_url_to_path_with_progress(
                &plan.archive_url,
                &archive_path,
                MAX_ARCHIVE_BYTES,
                FETCH_ARCHIVE_TIMEOUT,
                args.proxy.as_deref(),
                dl_ctx.as_ref(),
            )
            .await?;

            emit_progress(
                progress_tx,
                TransferProgress::stage(
                    TransferStage::Verifying,
                    format!("Verifying SHA-256 checksum of {}…", plan.archive_name),
                    Some(plan.archive_name.clone()),
                    Some(plan.tag.clone()),
                ),
            );
            let actual_sha256 = compute_file_sha256(&archive_path)?;
            if !actual_sha256.eq_ignore_ascii_case(&expected_sha256) {
                return Err(CliError::UpgradeChecksumMismatch {
                    asset: plan.archive_name.clone(),
                    expected: expected_sha256,
                    actual: actual_sha256,
                });
            }

            emit_progress(
                progress_tx,
                TransferProgress::stage(
                    TransferStage::Extracting,
                    format!("Unpacking release binaries from {}…", plan.archive_name),
                    Some(plan.archive_name.clone()),
                    Some(plan.tag.clone()),
                ),
            );
            let unpacked = unpack_release_binaries(&archive_path, &plan.archive_name, &stage.path)?;
            if plan.target_triple == COMPILE_TARGET {
                emit_progress(
                    progress_tx,
                    TransferProgress::stage(
                        TransferStage::Testing,
                        "Running `--version` smoke check on staged binaries…",
                        Some(plan.archive_name.clone()),
                        Some(plan.tag.clone()),
                    ),
                );
                smoke_check_staged_binary(&unpacked.liftgate_bin).await?;
            }
            Ok::<(StagedBinaries, String), CliError>((unpacked, actual_sha256))
        }
        .await?;

    emit_progress(
        progress_tx,
        TransferProgress::stage(
            TransferStage::Installing,
            format!("Installing {} & updating background gateway…", plan.tag),
            Some(plan.archive_name.clone()),
            Some(plan.tag.clone()),
        ),
    );
    // Check if a managed background daemon is running so we unlock `liftgate.exe` on Windows
    // and bring the daemon back online running the newly installed version.
    let install_targets = plan_install_targets(current_exe, &staged, &plan.target_triple);
    verify_install_targets_writable(&install_targets)?;

    let active_daemon_before = state_paths
        .read_daemon()?
        .filter(|d| daemon::is_pid_alive(d.pid));
    let saved_daemon_serve = active_daemon_before
        .as_ref()
        .filter(|_| !args.no_restart)
        .map(daemon::serve_args_from_daemon_state);

    if saved_daemon_serve.is_some() {
        let _ = daemon::stop_daemon(state_paths).await?;
    }

    let mut installed_paths = Vec::with_capacity(install_targets.len());
    let mut install_err: Option<CliError> = None;
    for (staged_src, dest_path) in &install_targets {
        match replace_executable_atomic(staged_src, dest_path) {
            Ok(()) => installed_paths.push(dest_path.display().to_string()),
            Err(err) => {
                install_err = Some(err);
                break;
            }
        }
    }
    if let Some(err) = install_err {
        if let Some(serve_cfg) = saved_daemon_serve {
            let _ = daemon::start_daemon_with_exe(state_paths, Some(current_exe), serve_cfg, false)
                .await;
        }
        return Err(err);
    }

    let mut daemon_restarted = false;
    if let Some(serve_cfg) = saved_daemon_serve {
        let new_daemon_exe = install_targets
            .iter()
            .find(|(_, dest)| {
                dest.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.eq_ignore_ascii_case("liftgate"))
            })
            .or_else(|| install_targets.first())
            .map(|(_, dest)| dest.as_path());
        if daemon::start_daemon_with_exe(state_paths, new_daemon_exe, serve_cfg, false)
            .await
            .is_ok()
        {
            daemon_restarted = true;
        }
    }

    Ok(UpgradeOutcome {
        status: "upgraded",
        current_version: CURRENT_VERSION_RAW.to_owned(),
        target_version: target_ver_str,
        tag: plan.tag,
        update_available: true,
        upgraded: true,
        target_triple: plan.target_triple,
        archive_name: plan.archive_name,
        archive_url: plan.archive_url,
        archive_size_bytes: plan.archive_size,
        sha256: Some(verified_sha256),
        release_url: plan.release_url,
        published_at: plan.published_at,
        installed_binaries: installed_paths,
        daemon_restarted,
        repo: plan.repo,
        release_title: plan.release_title,
        prerelease: plan.prerelease,
        notes_excerpt: plan.notes_excerpt,
    })
}

async fn resolve_release_plan(
    repo: &str,
    requested_tag: Option<&str>,
    desired_target: &str,
    proxy: Option<&str>,
    base_url_override: Option<&str>,
) -> Result<ResolvedReleasePlan, CliError> {
    let (api_base, web_base) = match base_url_override.map(str::trim).filter(|s| !s.is_empty()) {
        Some(custom) => {
            let clean = custom.trim_end_matches('/').to_owned();
            (clean.clone(), clean)
        }
        None => (
            "https://api.github.com".to_owned(),
            "https://github.com".to_owned(),
        ),
    };

    let api_url = match requested_tag {
        Some(tag) => format!("{api_base}/repos/{repo}/releases/tags/{tag}"),
        None => format!("{api_base}/repos/{repo}/releases/latest"),
    };

    match fetch_bytes_bounded(
        &api_url,
        MAX_METADATA_BYTES,
        FETCH_METADATA_TIMEOUT,
        proxy,
        true,
    )
    .await
    {
        Ok(raw_json) => {
            let rel: GithubReleaseResponse =
                serde_json::from_slice(&raw_json).map_err(|source| CliError::UpgradeNetwork {
                    url: api_url.clone(),
                    source: TransferError::InvalidJson { source },
                })?;
            build_plan_from_api_release(repo, &web_base, rel, desired_target)
        }
        Err(CliError::UpgradeNetwork {
            source: TransferError::HttpStatus { status: 403 | 429 },
            ..
        }) if base_url_override.is_none() => {
            // Fallback when unauthenticated `api.github.com` is rate-limited on shared NAT/VPN IPs.
            let resolved_tag = match requested_tag {
                Some(t) => t.to_owned(),
                None => resolve_latest_tag_via_redirect(repo, &web_base, proxy).await?,
            };
            Ok(build_synthetic_plan(
                repo,
                &web_base,
                &resolved_tag,
                desired_target,
            ))
        }
        Err(other) => Err(other),
    }
}

fn build_plan_from_api_release(
    repo: &str,
    web_base: &str,
    rel: GithubReleaseResponse,
    desired_target: &str,
) -> Result<ResolvedReleasePlan, CliError> {
    let candidates = candidate_targets_for(desired_target);
    let mut matched: Option<(String, String, GithubAsset)> = None;
    for cand in &candidates {
        let asset_name = archive_filename_for_target(cand);
        if let Some(found) = rel.assets.iter().find(|a| a.name == asset_name) {
            matched = Some((cand.clone(), asset_name, found.clone()));
            break;
        }
    }
    let Some((chosen_target, archive_name, asset)) = matched else {
        return Err(CliError::UpgradeUnsupportedTarget {
            tag: rel.tag_name,
            target: desired_target.to_owned(),
        });
    };

    let checksums_url = rel
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS")
        .map_or_else(
            || {
                format!(
                    "{web_base}/{repo}/releases/download/{}/SHA256SUMS",
                    rel.tag_name
                )
            },
            |a| a.browser_download_url.clone(),
        );

    let api_sha256 = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .filter(|hex| is_valid_sha256_hex(hex))
        .map(str::to_ascii_lowercase);

    let release_title = rel
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&rel.tag_name)
        .to_owned();
    let release_url = rel
        .html_url
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("{web_base}/{repo}/releases/tag/{}", rel.tag_name));
    let notes_excerpt = rel.body.as_deref().and_then(extract_notes_headline);

    Ok(ResolvedReleasePlan {
        repo: repo.to_owned(),
        tag: rel.tag_name,
        release_title,
        release_url,
        published_at: rel.published_at,
        prerelease: rel.prerelease,
        notes_excerpt,
        target_triple: chosen_target,
        archive_name,
        archive_url: asset.browser_download_url,
        archive_size: (asset.size > 0).then_some(asset.size),
        api_sha256,
        checksums_url,
    })
}

fn build_synthetic_plan(
    repo: &str,
    web_base: &str,
    tag: &str,
    desired_target: &str,
) -> ResolvedReleasePlan {
    let archive_name = archive_filename_for_target(desired_target);
    ResolvedReleasePlan {
        repo: repo.to_owned(),
        tag: tag.to_owned(),
        release_title: format!("Liftgate {tag}"),
        release_url: format!("{web_base}/{repo}/releases/tag/{tag}"),
        published_at: None,
        prerelease: tag.contains('-'),
        notes_excerpt: None,
        target_triple: desired_target.to_owned(),
        archive_url: format!("{web_base}/{repo}/releases/download/{tag}/{archive_name}"),
        archive_name,
        archive_size: None,
        api_sha256: None,
        checksums_url: format!("{web_base}/{repo}/releases/download/{tag}/SHA256SUMS"),
    }
}

fn candidate_targets_for(desired: &str) -> Vec<String> {
    let mut out = vec![desired.to_owned()];
    // Portable static musl fallback when a Linux host binary was built on a custom gnu triple.
    if let Some(arch) = desired.strip_suffix("-unknown-linux-gnu") {
        out.push(format!("{arch}-unknown-linux-musl"));
    } else if desired == "armv7-unknown-linux-gnueabihf" {
        out.push("armv7-unknown-linux-musleabihf".to_owned());
    }
    out
}

#[must_use]
pub fn archive_filename_for_target(target: &str) -> String {
    if target.contains("windows") {
        format!("liftgate-{target}.zip")
    } else {
        format!("liftgate-{target}.tar.gz")
    }
}

fn extract_notes_headline(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with('|')
            || trimmed.starts_with("> ")
        {
            continue;
        }
        let clean: String = trimmed.chars().take(120).collect();
        return Some(clean);
    }
    None
}

async fn resolve_latest_tag_via_redirect(
    repo: &str,
    web_base: &str,
    proxy: Option<&str>,
) -> Result<String, CliError> {
    let latest_url = format!("{web_base}/{repo}/releases/latest");
    let curl_bin = if cfg!(windows) { "curl.exe" } else { "curl" };
    let mut cmd = Command::new(curl_bin);
    cmd.args([
        "-sSL",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--max-redirs",
        "6",
        "--connect-timeout",
        "15",
        "--max-time",
        "20",
        "-o",
        if cfg!(windows) { "NUL" } else { "/dev/null" },
        "-w",
        "%{http_code}\t%{url_effective}",
    ]);
    if let Some(p) = proxy.map(str::trim).filter(|s| !s.is_empty()) {
        cmd.arg("--proxy").arg(p);
    }
    cmd.arg(&latest_url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    suppress_windows_console(&mut cmd);

    let out = timeout(FETCH_METADATA_TIMEOUT, cmd.output())
        .await
        .map_err(|_| CliError::UpgradeNetwork {
            url: latest_url.clone(),
            source: TransferError::TimedOut,
        })?
        .map_err(|err| CliError::UpgradeNetwork {
            url: latest_url.clone(),
            source: TransferError::SpawnTool {
                tool: "curl",
                source: err,
            },
        })?;

    let meta = String::from_utf8_lossy(&out.stdout);
    let (_, effective) = meta.trim().split_once('\t').unwrap_or(("", meta.trim()));
    if let Some((_, tag)) = effective.rsplit_once("/releases/tag/") {
        let clean_tag = tag.split(['?', '#']).next().unwrap_or(tag).trim();
        if !clean_tag.is_empty() {
            return normalize_tag_input(clean_tag);
        }
    }
    Err(CliError::UpgradeNetwork {
        url: latest_url,
        source: TransferError::MissingReleaseRedirectTag,
    })
}

async fn fetch_bytes_bounded(
    url: &str,
    max_bytes: u64,
    max_duration: Duration,
    proxy: Option<&str>,
    accept_github_json: bool,
) -> Result<Vec<u8>, CliError> {
    fetch_bytes_with_policy(
        url,
        max_bytes,
        max_duration,
        proxy,
        false,
        accept_github_json,
    )
    .await
}

pub(crate) async fn fetch_bytes_with_policy(
    url: &str,
    max_bytes: u64,
    max_duration: Duration,
    proxy: Option<&str>,
    force_direct_when_no_proxy: bool,
    accept_github_json: bool,
) -> Result<Vec<u8>, CliError> {
    if let Some(loopback) = parse_loopback_http_url(url) {
        return fetch_loopback_http(&loopback, max_bytes, max_duration).await;
    }
    let temp_file = std::env::temp_dir().join(format!(
        "liftgate-fetch-{}-{}.part",
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    let res = download_https_via_system_tool(
        url,
        &temp_file,
        max_bytes,
        max_duration,
        proxy,
        force_direct_when_no_proxy,
        accept_github_json,
        None,
    )
    .await;
    if let Err(err) = res {
        let _ = fs::remove_file(&temp_file);
        return Err(err);
    }
    let bytes = read_file_bounded(&temp_file, url, max_bytes);
    let _ = fs::remove_file(&temp_file);
    bytes
}

async fn download_url_to_path_with_progress(
    url: &str,
    dest: &Path,
    max_bytes: u64,
    max_duration: Duration,
    proxy: Option<&str>,
    progress: Option<&DownloadProgressContext<'_>>,
) -> Result<(), CliError> {
    download_url_with_policy_and_progress(
        url,
        dest,
        max_bytes,
        max_duration,
        proxy,
        false,
        progress,
    )
    .await
}

pub(crate) async fn download_url_with_policy_and_progress(
    url: &str,
    dest: &Path,
    max_bytes: u64,
    max_duration: Duration,
    proxy: Option<&str>,
    force_direct_when_no_proxy: bool,
    progress: Option<&DownloadProgressContext<'_>>,
) -> Result<(), CliError> {
    if let Some(loopback) = parse_loopback_http_url(url) {
        return download_loopback_http_to_path(&loopback, dest, max_bytes, max_duration, progress)
            .await;
    }
    download_https_via_system_tool(
        url,
        dest,
        max_bytes,
        max_duration,
        proxy,
        force_direct_when_no_proxy,
        false,
        progress,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal transport helper carries explicit download bounds, proxy policy, and optional progress sink"
)]
async fn download_https_via_system_tool(
    url: &str,
    dest: &Path,
    max_bytes: u64,
    max_duration: Duration,
    proxy: Option<&str>,
    force_direct_when_no_proxy: bool,
    accept_github_json: bool,
    progress: Option<&DownloadProgressContext<'_>>,
) -> Result<(), CliError> {
    if !url.starts_with("https://") {
        return Err(CliError::UpgradeInvalidInput {
            source: UpgradeInputError::NonHttpsUrl {
                url: url.to_owned(),
            },
        });
    }

    let header_path = dest.with_file_name(format!(
        "liftgate-hdr-{}-{}.tmp",
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    let _header_guard = TempFileGuard {
        path: header_path.clone(),
    };

    let curl_bin = if cfg!(windows) { "curl.exe" } else { "curl" };
    let mut cmd = Command::new(curl_bin);
    let max_secs = max_duration.as_secs().max(10).to_string();
    let max_size_str = max_bytes.to_string();
    cmd.args([
        "-sSL",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--max-redirs",
        "8",
        "--connect-timeout",
        "15",
        "--max-time",
        &max_secs,
        "--max-filesize",
        &max_size_str,
        "--config",
        "-",
        "-D",
    ])
    .arg(&header_path)
    .arg(url)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);

    if let Some(p) = proxy.map(str::trim).filter(|s| !s.is_empty()) {
        cmd.arg("--proxy").arg(p);
    } else if force_direct_when_no_proxy {
        cmd.arg("--noproxy").arg("*");
    }
    suppress_windows_console(&mut cmd);

    let mut curl_cfg = String::from("user-agent = \"Liftgate-CLI-Updater\"\n");
    if accept_github_json {
        curl_cfg.push_str("header = \"Accept: application/vnd.github+json\"\n");
    }
    if is_official_github_host(url)
        && let Some(tok) = github_auth_token()
    {
        let escaped = tok.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = writeln!(curl_cfg, "header = \"Authorization: Bearer {escaped}\"");
    }

    match cmd.spawn() {
        Ok(mut child) => {
            if let Some(mut stdin_pipe) = child.stdin.take() {
                let _ = stdin_pipe.write_all(curl_cfg.as_bytes()).await;
                let _ = stdin_pipe.shutdown().await;
            }

            let Some(mut stdout_pipe) = child.stdout.take() else {
                let _ = child.kill().await;
                return Err(CliError::UpgradeNetwork {
                    url: url.to_owned(),
                    source: TransferError::MissingStdoutPipe { tool: "curl" },
                });
            };
            let stderr_pipe = child.stderr.take();
            let stderr_task = tokio::spawn(async move {
                let Some(pipe) = stderr_pipe else {
                    return Vec::new();
                };
                let mut out = Vec::new();
                let mut limited = pipe.take(MAX_CURL_STDERR_BYTES as u64);
                let _ = limited.read_to_end(&mut out).await;
                out
            });
            let stderr_abort = stderr_task.abort_handle();

            let stream_res = timeout(max_duration.saturating_add(Duration::from_secs(5)), async {
                let file =
                    tokio::fs::File::create(dest)
                        .await
                        .map_err(|source| CliError::StateIo {
                            path: dest.to_path_buf(),
                            source,
                        })?;
                let mut writer = BufWriter::with_capacity(64 * 1024, file);
                let mut buf = vec![0u8; 32 * 1024];
                let mut downloaded: u64 = 0;
                let mut total_bytes = progress.and_then(|p| p.known_total_bytes);
                let mut header_checked = false;
                let mut tracker = SpeedTracker::new(Instant::now());
                let mut last_emit = Instant::now();

                if let Some(p) = progress {
                    let _ = p.tx.send_replace(TransferProgress {
                        stage: TransferStage::Downloading,
                        message: format!("Downloading {}…", p.asset_name),
                        asset_name: Some(p.asset_name.clone()),
                        target_version: p.target_version.clone(),
                        downloaded_bytes: 0,
                        total_bytes,
                        bytes_per_sec: 0.0,
                        eta_secs: None,
                    });
                }

                loop {
                    let n = stdout_pipe.read(&mut buf).await.map_err(|err| {
                        CliError::UpgradeNetwork {
                            url: url.to_owned(),
                            source: TransferError::ToolIo {
                                tool: "curl",
                                source: err,
                            },
                        }
                    })?;
                    if n == 0 {
                        break;
                    }

                    if !header_checked || total_bytes.is_none() {
                        let meta = parse_http_headers_file(&header_path);
                        if let Some(code) = meta.status_code {
                            header_checked = true;
                            if code >= 400 {
                                let _ = child.kill().await;
                                let _ = child.wait().await;
                                return Err(CliError::UpgradeNetwork {
                                    url: url.to_owned(),
                                    source: TransferError::HttpStatus { status: code },
                                });
                            }
                        }
                        if total_bytes.is_none() && meta.content_length.is_some() {
                            total_bytes = meta.content_length;
                        }
                    }

                    downloaded = downloaded.saturating_add(n as u64);
                    if downloaded > max_bytes {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        return Err(CliError::UpgradeNetwork {
                            url: url.to_owned(),
                            source: TransferError::PayloadTooLarge {
                                limit_mib: (max_bytes / (1024 * 1024)).max(1),
                            },
                        });
                    }

                    if let Some(chunk) = buf.get(..n) {
                        writer
                            .write_all(chunk)
                            .await
                            .map_err(|source| CliError::StateIo {
                                path: dest.to_path_buf(),
                                source,
                            })?;
                    }

                    if let Some(p) = progress {
                        let now = Instant::now();
                        let rate = tracker.record(now, downloaded);
                        if now.duration_since(last_emit) >= PROGRESS_EMIT_INTERVAL
                            || downloaded == n as u64
                        {
                            last_emit = now;
                            let _ = p.tx.send_replace(TransferProgress {
                                stage: TransferStage::Downloading,
                                message: format!("Downloading {}…", p.asset_name),
                                asset_name: Some(p.asset_name.clone()),
                                target_version: p.target_version.clone(),
                                downloaded_bytes: downloaded,
                                total_bytes,
                                bytes_per_sec: rate,
                                eta_secs: estimate_eta_secs(downloaded, total_bytes, rate),
                            });
                        }
                    }
                }

                writer.flush().await.map_err(|source| CliError::StateIo {
                    path: dest.to_path_buf(),
                    source,
                })?;
                drop(writer);

                let exit_status = child.wait().await.map_err(|err| CliError::UpgradeNetwork {
                    url: url.to_owned(),
                    source: TransferError::ToolIo {
                        tool: "curl",
                        source: err,
                    },
                })?;
                let stderr_bytes = stderr_task.await.unwrap_or_default();

                let meta = parse_http_headers_file(&header_path);
                if let Some(code) = meta.status_code
                    && code >= 400
                {
                    return Err(CliError::UpgradeNetwork {
                        url: url.to_owned(),
                        source: TransferError::HttpStatus { status: code },
                    });
                }
                if !exit_status.success() {
                    let stderr = String::from_utf8_lossy(&stderr_bytes).trim().to_owned();
                    return Err(CliError::UpgradeNetwork {
                        url: url.to_owned(),
                        source: if stderr.is_empty() {
                            TransferError::ToolExitStatus {
                                tool: "curl",
                                status: exit_status,
                            }
                        } else {
                            TransferError::ToolStderr {
                                tool: "curl",
                                stderr,
                            }
                        },
                    });
                }

                if let Some(p) = progress {
                    let now = Instant::now();
                    let rate = tracker.record(now, downloaded);
                    let final_total = total_bytes.or(meta.content_length).or(Some(downloaded));
                    let _ = p.tx.send_replace(TransferProgress {
                        stage: TransferStage::Downloading,
                        message: format!("Downloaded {}", p.asset_name),
                        asset_name: Some(p.asset_name.clone()),
                        target_version: p.target_version.clone(),
                        downloaded_bytes: downloaded,
                        total_bytes: final_total,
                        bytes_per_sec: rate,
                        eta_secs: Some(0),
                    });
                }
                Ok(())
            })
            .await;

            match stream_res {
                Ok(Ok(())) => Ok(()),
                Ok(Err(err)) => {
                    stderr_abort.abort();
                    let _ = fs::remove_file(dest);
                    Err(err)
                }
                Err(_) => {
                    stderr_abort.abort();
                    let _ = fs::remove_file(dest);
                    Err(CliError::UpgradeNetwork {
                        url: url.to_owned(),
                        source: TransferError::TimedOut,
                    })
                }
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            download_https_fallback(
                url,
                dest,
                max_duration,
                proxy,
                force_direct_when_no_proxy,
                progress,
            )
            .await
        }
        Err(err) => Err(CliError::UpgradeNetwork {
            url: url.to_owned(),
            source: TransferError::SpawnTool {
                tool: "curl",
                source: err,
            },
        }),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HttpHeaderMeta {
    status_code: Option<u16>,
    content_length: Option<u64>,
}

/// Parses `curl -D` response headers and returns the final non-1xx, non-CONNECT HTTP block.
fn parse_http_headers_file(path: &Path) -> HttpHeaderMeta {
    let Ok(bytes) = read_file_bounded(path, "", MAX_HTTP_HEADER_FILE_BYTES) else {
        return HttpHeaderMeta::default();
    };
    parse_http_headers_text(&String::from_utf8_lossy(&bytes))
}

fn parse_http_headers_text(text: &str) -> HttpHeaderMeta {
    let mut result = HttpHeaderMeta::default();
    let mut in_ignored_block = false;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if line
            .get(..5)
            .is_some_and(|p| p.eq_ignore_ascii_case("HTTP/"))
        {
            let code = line
                .split_whitespace()
                .nth(1)
                .and_then(|tok| tok.parse::<u16>().ok());
            let is_proxy_connect = line.to_ascii_lowercase().contains("connection established");
            if is_proxy_connect || code.is_some_and(|c| (100..200).contains(&c)) {
                in_ignored_block = true;
            } else {
                in_ignored_block = false;
                result = HttpHeaderMeta {
                    status_code: code,
                    content_length: None,
                };
            }
            continue;
        }
        if in_ignored_block {
            continue;
        }
        if let Some((name, val)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
            && let Ok(len) = val.trim().parse::<u64>()
        {
            result.content_length = Some(len);
        }
    }
    result
}

async fn wait_child_with_file_progress(
    tool: &'static str,
    child: tokio::process::Child,
    dest: &Path,
    max_duration: Duration,
    progress: Option<&DownloadProgressContext<'_>>,
) -> Result<std::process::Output, TransferError> {
    let Some(p) = progress else {
        return match timeout(max_duration, child.wait_with_output()).await {
            Ok(res) => res.map_err(|source| TransferError::ToolIo { tool, source }),
            Err(_) => Err(TransferError::TimedOut),
        };
    };

    let mut tracker = SpeedTracker::new(Instant::now());
    let mut wait_fut = std::pin::pin!(child.wait_with_output());
    let mut tick = tokio::time::interval(Duration::from_millis(120));
    let deadline = tokio::time::Instant::now() + max_duration;
    loop {
        tokio::select! {
            res = &mut wait_fut => {
                return res.map_err(|source| TransferError::ToolIo { tool, source });
            }
            () = tokio::time::sleep_until(deadline) => {
                return Err(TransferError::TimedOut);
            }
            _ = tick.tick() => {
                let downloaded = fs::metadata(dest).map_or(0, |m| m.len());
                let rate = tracker.record(Instant::now(), downloaded);
                let _ = p.tx.send_replace(TransferProgress {
                    stage: TransferStage::Downloading,
                    message: format!("Downloading {}…", p.asset_name),
                    asset_name: Some(p.asset_name.clone()),
                    target_version: p.target_version.clone(),
                    downloaded_bytes: downloaded,
                    total_bytes: p.known_total_bytes,
                    bytes_per_sec: rate,
                    eta_secs: estimate_eta_secs(downloaded, p.known_total_bytes, rate),
                });
            }
        }
    }
}

async fn download_https_fallback(
    url: &str,
    dest: &Path,
    max_duration: Duration,
    proxy: Option<&str>,
    force_direct_when_no_proxy: bool,
    progress: Option<&DownloadProgressContext<'_>>,
) -> Result<(), CliError> {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("powershell.exe");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$ProgressPreference='SilentlyContinue'; if ($env:LG_UPG_PROXY) { Invoke-WebRequest -UseBasicParsing -Uri $env:LG_UPG_URL -OutFile $env:LG_UPG_OUT -Proxy $env:LG_UPG_PROXY } else { Invoke-WebRequest -UseBasicParsing -Uri $env:LG_UPG_URL -OutFile $env:LG_UPG_OUT }",
        ])
        .env("LG_UPG_URL", url)
        .env("LG_UPG_OUT", dest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        if let Some(p) = proxy.map(str::trim).filter(|s| !s.is_empty()) {
            cmd.env("LG_UPG_PROXY", p);
        } else {
            cmd.env_remove("LG_UPG_PROXY");
            if force_direct_when_no_proxy {
                for k in [
                    "HTTP_PROXY",
                    "http_proxy",
                    "HTTPS_PROXY",
                    "https_proxy",
                    "ALL_PROXY",
                    "all_proxy",
                ] {
                    cmd.env_remove(k);
                }
            }
        }
        suppress_windows_console(&mut cmd);
        let child = cmd.spawn().map_err(|err| CliError::UpgradeNetwork {
            url: url.to_owned(),
            source: TransferError::SpawnTool {
                tool: "powershell.exe",
                source: err,
            },
        })?;
        let out =
            wait_child_with_file_progress("powershell.exe", child, dest, max_duration, progress)
                .await
                .map_err(|source| {
                    let _ = fs::remove_file(dest);
                    CliError::UpgradeNetwork {
                        url: url.to_owned(),
                        source,
                    }
                })?;
        if !out.status.success() {
            let _ = fs::remove_file(dest);
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            return Err(CliError::UpgradeNetwork {
                url: url.to_owned(),
                source: if stderr.is_empty() {
                    TransferError::ToolExitStatus {
                        tool: "powershell.exe",
                        status: out.status,
                    }
                } else {
                    TransferError::ToolStderr {
                        tool: "powershell.exe",
                        stderr,
                    }
                },
            });
        }
        Ok(())
    }

    #[cfg(not(windows))]
    {
        for (tool, args) in [
            ("wget", vec!["--https-only", "-q", "-O"]),
            ("fetch", vec!["-q", "-o"]),
        ] {
            let mut cmd = Command::new(tool);
            if force_direct_when_no_proxy && proxy.is_none() && tool == "wget" {
                cmd.arg("--no-proxy");
            }
            cmd.args(&args)
                .arg(dest)
                .arg(url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            if let Some(p) = proxy.map(str::trim).filter(|s| !s.is_empty()) {
                cmd.env("https_proxy", p).env("HTTPS_PROXY", p);
            } else if force_direct_when_no_proxy {
                for k in [
                    "HTTP_PROXY",
                    "http_proxy",
                    "HTTPS_PROXY",
                    "https_proxy",
                    "ALL_PROXY",
                    "all_proxy",
                ] {
                    cmd.env_remove(k);
                }
            }
            match cmd.spawn() {
                Ok(child) => {
                    let out =
                        wait_child_with_file_progress(tool, child, dest, max_duration, progress)
                            .await
                            .map_err(|source| {
                                let _ = fs::remove_file(dest);
                                CliError::UpgradeNetwork {
                                    url: url.to_owned(),
                                    source,
                                }
                            })?;
                    if out.status.success() {
                        return Ok(());
                    }
                    let _ = fs::remove_file(dest);
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
                    return Err(CliError::UpgradeNetwork {
                        url: url.to_owned(),
                        source: if stderr.is_empty() {
                            TransferError::ToolExitStatus {
                                tool,
                                status: out.status,
                            }
                        } else {
                            TransferError::ToolStderr { tool, stderr }
                        },
                    });
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(CliError::UpgradeNetwork {
                        url: url.to_owned(),
                        source: TransferError::SpawnTool { tool, source: err },
                    });
                }
            }
        }
        Err(CliError::UpgradeNetwork {
            url: url.to_owned(),
            source: TransferError::NoHttpsDownloadTool,
        })
    }
}

struct LoopbackUrl {
    addr: SocketAddr,
    host_header: String,
    path_and_query: String,
    raw_url: String,
}

fn parse_loopback_http_url(url: &str) -> Option<LoopbackUrl> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path_part) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_owned()),
    };
    let addr: SocketAddr = if let Some(port_str) = authority.strip_prefix("localhost:") {
        let port = port_str.parse::<u16>().ok()?;
        SocketAddr::from(([127, 0, 0, 1], port))
    } else {
        authority.parse::<SocketAddr>().ok()?
    };
    if !addr.ip().is_loopback() {
        return None;
    }
    Some(LoopbackUrl {
        addr,
        host_header: authority.to_owned(),
        path_and_query: path_part,
        raw_url: url.to_owned(),
    })
}

async fn download_loopback_http_to_path(
    target: &LoopbackUrl,
    dest: &Path,
    max_bytes: u64,
    max_duration: Duration,
    progress: Option<&DownloadProgressContext<'_>>,
) -> Result<(), CliError> {
    let res = timeout(max_duration, async {
        let mut stream = TcpStream::connect(target.addr).await.map_err(|err| {
            CliError::UpgradeNetwork {
                url: target.raw_url.clone(),
                source: TransferError::LoopbackIo {
                    op: "connect",
                    source: err,
                },
            }
        })?;
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: Liftgate-CLI-Updater\r\nConnection: close\r\n\r\n",
            target.path_and_query, target.host_header
        );
        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|err| CliError::UpgradeNetwork {
                url: target.raw_url.clone(),
                source: TransferError::LoopbackIo {
                    op: "write",
                    source: err,
                },
            })?;

        let mut header_buf = Vec::new();
        let mut buf = vec![0u8; 32 * 1024];
        let hdr_end = loop {
            let n = stream
                .read(&mut buf)
                .await
                .map_err(|err| CliError::UpgradeNetwork {
                    url: target.raw_url.clone(),
                    source: TransferError::LoopbackIo {
                        op: "read",
                        source: err,
                    },
                })?;
            if n == 0 {
                return Err(CliError::UpgradeNetwork {
                    url: target.raw_url.clone(),
                    source: TransferError::MalformedHttpHeader,
                });
            }
            if let Some(chunk) = buf.get(..n) {
                header_buf.extend_from_slice(chunk);
            }
            if let Some(pos) = header_buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
            if header_buf.len() > 65_536 {
                return Err(CliError::UpgradeNetwork {
                    url: target.raw_url.clone(),
                    source: TransferError::HttpHeaderTooLarge,
                });
            }
        };

        let header_str = String::from_utf8_lossy(header_buf.get(..hdr_end).unwrap_or_default());
        let meta = parse_http_headers_text(&header_str);
        let status = meta.status_code.unwrap_or(0);
        if !(200..300).contains(&status) {
            return Err(CliError::UpgradeNetwork {
                url: target.raw_url.clone(),
                source: TransferError::HttpStatus { status },
            });
        }

        let total_bytes = progress
            .and_then(|p| p.known_total_bytes)
            .or(meta.content_length);
        let file = tokio::fs::File::create(dest)
            .await
            .map_err(|source| CliError::StateIo {
                path: dest.to_path_buf(),
                source,
            })?;
        let mut writer = BufWriter::with_capacity(64 * 1024, file);
        let mut downloaded: u64 = 0;
        let mut tracker = SpeedTracker::new(Instant::now());
        let mut last_emit = Instant::now();

        let initial_body = header_buf.get(hdr_end + 4..).unwrap_or_default();
        if !initial_body.is_empty() {
            downloaded = downloaded.saturating_add(initial_body.len() as u64);
            if downloaded > max_bytes {
                return Err(CliError::UpgradeNetwork {
                    url: target.raw_url.clone(),
                    source: TransferError::PayloadTooLarge {
                        limit_mib: (max_bytes / (1024 * 1024)).max(1),
                    },
                });
            }
            writer
                .write_all(initial_body)
                .await
                .map_err(|source| CliError::StateIo {
                    path: dest.to_path_buf(),
                    source,
                })?;
        }

        loop {
            let n = stream
                .read(&mut buf)
                .await
                .map_err(|err| CliError::UpgradeNetwork {
                    url: target.raw_url.clone(),
                    source: TransferError::LoopbackIo {
                        op: "read",
                        source: err,
                    },
                })?;
            if n == 0 {
                break;
            }
            downloaded = downloaded.saturating_add(n as u64);
            if downloaded > max_bytes {
                return Err(CliError::UpgradeNetwork {
                    url: target.raw_url.clone(),
                    source: TransferError::PayloadTooLarge {
                        limit_mib: (max_bytes / (1024 * 1024)).max(1),
                    },
                });
            }
            if let Some(chunk) = buf.get(..n) {
                writer
                    .write_all(chunk)
                    .await
                    .map_err(|source| CliError::StateIo {
                        path: dest.to_path_buf(),
                        source,
                    })?;
            }
            if let Some(p) = progress {
                let now = Instant::now();
                let rate = tracker.record(now, downloaded);
                if now.duration_since(last_emit) >= PROGRESS_EMIT_INTERVAL {
                    last_emit = now;
                    let _ = p.tx.send_replace(TransferProgress {
                        stage: TransferStage::Downloading,
                        message: format!("Downloading {}…", p.asset_name),
                        asset_name: Some(p.asset_name.clone()),
                        target_version: p.target_version.clone(),
                        downloaded_bytes: downloaded,
                        total_bytes,
                        bytes_per_sec: rate,
                        eta_secs: estimate_eta_secs(downloaded, total_bytes, rate),
                    });
                }
            }
        }

        writer.flush().await.map_err(|source| CliError::StateIo {
            path: dest.to_path_buf(),
            source,
        })?;
        if let Some(p) = progress {
            let rate = tracker.record(Instant::now(), downloaded);
            let _ = p.tx.send_replace(TransferProgress {
                stage: TransferStage::Downloading,
                message: format!("Downloaded {}", p.asset_name),
                asset_name: Some(p.asset_name.clone()),
                target_version: p.target_version.clone(),
                downloaded_bytes: downloaded,
                total_bytes: total_bytes.or(Some(downloaded)),
                bytes_per_sec: rate,
                eta_secs: Some(0),
            });
        }
        Ok(())
    })
    .await
    .map_err(|_| CliError::UpgradeNetwork {
        url: target.raw_url.clone(),
        source: TransferError::TimedOut,
    })?;
    if res.is_err() {
        let _ = fs::remove_file(dest);
    }
    res
}

async fn fetch_loopback_http(
    target: &LoopbackUrl,
    max_bytes: u64,
    max_duration: Duration,
) -> Result<Vec<u8>, CliError> {
    timeout(max_duration, async {
        let mut stream = TcpStream::connect(target.addr).await.map_err(|err| {
            CliError::UpgradeNetwork {
                url: target.raw_url.clone(),
                source: TransferError::LoopbackIo {
                    op: "connect",
                    source: err,
                },
            }
        })?;
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: Liftgate-CLI-Updater\r\nConnection: close\r\n\r\n",
            target.path_and_query, target.host_header
        );
        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|err| CliError::UpgradeNetwork {
                url: target.raw_url.clone(),
                source: TransferError::LoopbackIo {
                    op: "write",
                    source: err,
                },
            })?;

        let cap = usize::try_from(max_bytes.saturating_add(65_536)).unwrap_or(usize::MAX);
        let mut raw = Vec::new();
        let mut buf = [0u8; 16_384];
        loop {
            let n = stream
                .read(&mut buf)
                .await
                .map_err(|err| CliError::UpgradeNetwork {
                    url: target.raw_url.clone(),
                    source: TransferError::LoopbackIo {
                        op: "read",
                        source: err,
                    },
                })?;
            if n == 0 {
                break;
            }
            if let Some(chunk) = buf.get(..n) {
                raw.extend_from_slice(chunk);
            }
            if raw.len() > cap {
                return Err(CliError::UpgradeNetwork {
                    url: target.raw_url.clone(),
                    source: TransferError::PayloadTooLarge {
                        limit_mib: (max_bytes / (1024 * 1024)).max(1),
                    },
                });
            }
        }

        let Some(hdr_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
            return Err(CliError::UpgradeNetwork {
                url: target.raw_url.clone(),
                source: TransferError::MalformedHttpHeader,
            });
        };
        let header_slice = raw.get(..hdr_end).unwrap_or_default();
        let body_slice = raw.get(hdr_end + 4..).unwrap_or_default();
        let header_str = String::from_utf8_lossy(header_slice);
        let status = header_str
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or(0);
        if !(200..300).contains(&status) {
            return Err(CliError::UpgradeNetwork {
                url: target.raw_url.clone(),
                source: TransferError::HttpStatus { status },
            });
        }
        if u64::try_from(body_slice.len()).unwrap_or(u64::MAX) > max_bytes {
            return Err(CliError::UpgradeNetwork {
                url: target.raw_url.clone(),
                source: TransferError::PayloadTooLarge {
                    limit_mib: (max_bytes / (1024 * 1024)).max(1),
                },
            });
        }
        Ok(body_slice.to_vec())
    })
    .await
    .map_err(|_| CliError::UpgradeNetwork {
        url: target.raw_url.clone(),
        source: TransferError::TimedOut,
    })?
}

fn is_official_github_host(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or("");
    host.eq_ignore_ascii_case("api.github.com") || host.eq_ignore_ascii_case("github.com")
}

fn github_auth_token() -> Option<String> {
    for key in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(val) = std::env::var(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() && !trimmed.contains(['\r', '\n']) {
                return Some(trimmed.to_owned());
            }
        }
    }
    None
}

fn suppress_windows_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

fn read_file_bounded(path: &Path, url: &str, max_bytes: u64) -> Result<Vec<u8>, CliError> {
    let file = File::open(path).map_err(|source| CliError::StateIo {
        path: path.to_path_buf(),
        source,
    })?;
    let mut out = Vec::new();
    let mut limited = file.take(max_bytes.saturating_add(1));
    limited
        .read_to_end(&mut out)
        .map_err(|source| CliError::StateIo {
            path: path.to_path_buf(),
            source,
        })?;
    if u64::try_from(out.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(CliError::UpgradeNetwork {
            url: url.to_owned(),
            source: TransferError::PayloadTooLarge {
                limit_mib: (max_bytes / (1024 * 1024)).max(1),
            },
        });
    }
    Ok(out)
}

/// Extracts the expected 64-character lowercase SHA-256 hex digest for `asset_name` from `SHA256SUMS`.
#[must_use]
pub fn lookup_sha256_in_sums(sums_content: &str, asset_name: &str) -> Option<String> {
    for raw_line in sums_content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(hash_cand) = parts.next() else {
            continue;
        };
        let Some(file_cand) = parts.next() else {
            continue;
        };
        let clean_name = file_cand
            .trim_start_matches('*')
            .trim_start_matches("./")
            .rsplit('/')
            .next()
            .unwrap_or(file_cand);
        if clean_name == asset_name && is_valid_sha256_hex(hash_cand) {
            return Some(hash_cand.to_ascii_lowercase());
        }
    }
    None
}

fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn compute_file_sha256(path: &Path) -> Result<String, CliError> {
    let mut file = File::open(path).map_err(|source| CliError::StateIo {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65_536];
    loop {
        let n = file.read(&mut buf).map_err(|source| CliError::StateIo {
            path: path.to_path_buf(),
            source,
        })?;
        if n == 0 {
            break;
        }
        if let Some(slice) = buf.get(..n) {
            hasher.update(slice);
        }
    }
    let digest = hasher.finalize();
    Ok(hex_lower(&digest))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

struct StagedBinaries {
    liftgate_bin: PathBuf,
    lg_bin: PathBuf,
}

fn unpack_release_binaries(
    archive_path: &Path,
    archive_name: &str,
    stage_dir: &Path,
) -> Result<StagedBinaries, CliError> {
    let is_zip = archive_name
        .rsplit('.')
        .next()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"));
    let expected_liftgate = if is_zip { "liftgate.exe" } else { "liftgate" };
    let expected_lg = if is_zip { "lg.exe" } else { "lg" };

    let staged_liftgate = stage_dir.join(format!("staged-{expected_liftgate}"));
    let staged_lg = stage_dir.join(format!("staged-{expected_lg}"));

    if is_zip {
        extract_zip_binaries(
            archive_path,
            archive_name,
            expected_liftgate,
            expected_lg,
            &staged_liftgate,
            &staged_lg,
        )?;
    } else {
        extract_targz_binaries(
            archive_path,
            archive_name,
            expected_liftgate,
            expected_lg,
            &staged_liftgate,
            &staged_lg,
        )?;
    }

    if !staged_liftgate.is_file() {
        return Err(CliError::UpgradeArchive {
            asset: archive_name.to_owned(),
            source: ArchiveError::MissingExecutable {
                expected: expected_liftgate.to_owned(),
            },
        });
    }
    if !staged_lg.is_file() {
        fs::copy(&staged_liftgate, &staged_lg).map_err(|source| CliError::StateIo {
            path: staged_lg.clone(),
            source,
        })?;
        set_executable_permissions(&staged_lg)?;
    }

    Ok(StagedBinaries {
        liftgate_bin: staged_liftgate,
        lg_bin: staged_lg,
    })
}

fn extract_zip_binaries(
    archive_path: &Path,
    archive_name: &str,
    expected_liftgate: &str,
    expected_lg: &str,
    staged_liftgate: &Path,
    staged_lg: &Path,
) -> Result<(), CliError> {
    let file = File::open(archive_path).map_err(|source| CliError::StateIo {
        path: archive_path.to_path_buf(),
        source,
    })?;
    let mut zip = ZipArchive::new(file).map_err(|source| CliError::UpgradeArchive {
        asset: archive_name.to_owned(),
        source: ArchiveError::CorruptZip { source },
    })?;

    for idx in 0..zip.len() {
        let entry = zip
            .by_index(idx)
            .map_err(|source| CliError::UpgradeArchive {
                asset: archive_name.to_owned(),
                source: ArchiveError::ZipEntry { index: idx, source },
            })?;
        if entry.is_dir() || entry.is_symlink() {
            continue;
        }
        let dest = {
            let entry_name = entry.name().map_err(|source| CliError::UpgradeArchive {
                asset: archive_name.to_owned(),
                source: ArchiveError::ZipEntry { index: idx, source },
            })?;
            let base_name = entry_name
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(entry_name.as_ref());
            if base_name == expected_liftgate {
                Some(staged_liftgate)
            } else if base_name == expected_lg {
                Some(staged_lg)
            } else {
                None
            }
        };
        if let Some(out_path) = dest {
            write_bounded_stream_to_executable(entry, out_path, archive_name)?;
        }
    }
    Ok(())
}

fn extract_targz_binaries(
    archive_path: &Path,
    archive_name: &str,
    expected_liftgate: &str,
    expected_lg: &str,
    staged_liftgate: &Path,
    staged_lg: &Path,
) -> Result<(), CliError> {
    let file = File::open(archive_path).map_err(|source| CliError::StateIo {
        path: archive_path.to_path_buf(),
        source,
    })?;
    let gz = GzDecoder::new(file);
    let mut archive = Archive::new(gz);
    let entries = archive
        .entries()
        .map_err(|source| CliError::UpgradeArchive {
            asset: archive_name.to_owned(),
            source: ArchiveError::CorruptTarGz { source },
        })?;

    for item in entries {
        let entry = item.map_err(|source| CliError::UpgradeArchive {
            asset: archive_name.to_owned(),
            source: ArchiveError::CorruptTarEntry { source },
        })?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let entry_path = entry.path().map_err(|source| CliError::UpgradeArchive {
            asset: archive_name.to_owned(),
            source: ArchiveError::InvalidTarEntryPath { source },
        })?;
        let Some(base_name) = entry_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let dest = if base_name == expected_liftgate {
            Some(staged_liftgate)
        } else if base_name == expected_lg {
            Some(staged_lg)
        } else {
            None
        };
        if let Some(out_path) = dest {
            write_bounded_stream_to_executable(entry, out_path, archive_name)?;
        }
    }
    Ok(())
}

fn write_bounded_stream_to_executable<R: io::Read>(
    reader: R,
    dest: &Path,
    archive_name: &str,
) -> Result<(), CliError> {
    let mut out = File::create(dest).map_err(|source| CliError::StateIo {
        path: dest.to_path_buf(),
        source,
    })?;
    let mut limited = reader.take(MAX_BINARY_EXTRACT_BYTES.saturating_add(1));
    let copied = io::copy(&mut limited, &mut out).map_err(|source| CliError::UpgradeArchive {
        asset: archive_name.to_owned(),
        source: ArchiveError::Decompress { source },
    })?;
    if copied > MAX_BINARY_EXTRACT_BYTES {
        let _ = fs::remove_file(dest);
        return Err(CliError::UpgradeArchive {
            asset: archive_name.to_owned(),
            source: ArchiveError::ExtractedBinaryTooLarge {
                limit_mib: MAX_BINARY_EXTRACT_BYTES / (1024 * 1024),
            },
        });
    }
    out.sync_all().map_err(|source| CliError::StateIo {
        path: dest.to_path_buf(),
        source,
    })?;
    drop(out);
    set_executable_permissions(dest)
}

fn set_executable_permissions(path: &Path) -> Result<(), CliError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|source| {
            CliError::StateIo {
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

async fn smoke_check_staged_binary(staged_exe: &Path) -> Result<(), CliError> {
    let mut cmd = Command::new(staged_exe);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    suppress_windows_console(&mut cmd);

    let out = timeout(Duration::from_secs(5), cmd.output())
        .await
        .map_err(|_| CliError::UpgradeArchive {
            asset: staged_exe.display().to_string(),
            source: ArchiveError::SmokeCheckTimeout,
        })?
        .map_err(|source| CliError::UpgradeArchive {
            asset: staged_exe.display().to_string(),
            source: ArchiveError::SmokeCheckExec { source },
        })?;
    if !out.status.success() {
        return Err(CliError::UpgradeArchive {
            asset: staged_exe.display().to_string(),
            source: ArchiveError::SmokeCheckFailed {
                status: out.status,
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            },
        });
    }
    Ok(())
}

fn plan_install_targets(
    current_exe: &Path,
    staged: &StagedBinaries,
    target_triple: &str,
) -> Vec<(PathBuf, PathBuf)> {
    let ext = if target_triple.contains("windows") {
        ".exe"
    } else {
        ""
    };
    let Some(install_dir) = current_exe.parent() else {
        return vec![(staged.liftgate_bin.clone(), current_exe.to_path_buf())];
    };

    let canonical_liftgate = install_dir.join(format!("liftgate{ext}"));
    let canonical_lg = install_dir.join(format!("lg{ext}"));

    let mut targets: Vec<(PathBuf, PathBuf)> = Vec::new();
    let current_stem = current_exe
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("liftgate");
    if current_stem.eq_ignore_ascii_case("liftgate") {
        targets.push((staged.liftgate_bin.clone(), current_exe.to_path_buf()));
    } else {
        targets.push((staged.lg_bin.clone(), current_exe.to_path_buf()));
    }

    let is_cargo_build_dir = install_dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "debug" || n == "release")
        && install_dir
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "target" || n.contains('-'));

    for (src, candidate) in [
        (&staged.liftgate_bin, canonical_liftgate),
        (&staged.lg_bin, canonical_lg),
    ] {
        let already_listed = targets.iter().any(|(_, p)| same_file_path(p, &candidate));
        if !already_listed && (!is_cargo_build_dir || candidate.is_file()) {
            targets.push((src.clone(), candidate));
        }
    }

    targets
}

fn same_file_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}

/// Atomically installs `staged_src` onto `dest_path`, handling Windows locked running executables
/// via same-volume rename + rollback protection.
pub fn replace_executable_atomic(staged_src: &Path, dest_path: &Path) -> Result<(), CliError> {
    let parent = dest_path.parent().unwrap_or_else(|| Path::new("."));
    sweep_stale_backups_in_dir(parent);

    let base_name = dest_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("liftgate");
    let tmp_new = parent.join(format!(".{base_name}.new.{}", Uuid::new_v4().simple()));

    copy_executable_with_sync(staged_src, &tmp_new)
        .map_err(|source| map_replace_io(dest_path, source))?;

    #[cfg(unix)]
    {
        if let Err(source) = fs::rename(&tmp_new, dest_path) {
            let _ = fs::remove_file(&tmp_new);
            return Err(map_replace_io(dest_path, source));
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        if !dest_path.exists() {
            if let Err(source) = fs::rename(&tmp_new, dest_path) {
                let _ = fs::remove_file(&tmp_new);
                return Err(map_replace_io(dest_path, source));
            }
            return Ok(());
        }

        let primary_backup = parent.join(format!("{base_name}{BACKUP_MARKER}"));
        let _ = fs::remove_file(&primary_backup);
        let backup_path = if primary_backup.exists() {
            parent.join(format!(
                "{base_name}{BACKUP_MARKER}.{}.{}",
                std::process::id(),
                Uuid::new_v4().simple()
            ))
        } else {
            primary_backup
        };

        if let Err(source) = fs::rename(dest_path, &backup_path) {
            let _ = fs::remove_file(&tmp_new);
            return Err(map_replace_io(dest_path, source));
        }

        if let Err(source) = fs::rename(&tmp_new, dest_path) {
            // Roll back immediately so `dest_path` is never left missing.
            let _ = fs::rename(&backup_path, dest_path);
            let _ = fs::remove_file(&tmp_new);
            return Err(map_replace_io(dest_path, source));
        }

        // Succeeds immediately for non-running binaries; for the currently running `.exe` on
        // Windows, the `.old-upgrade` file is swept automatically on the next invocation.
        let _ = fs::remove_file(&backup_path);
        Ok(())
    }
}

fn copy_executable_with_sync(src: &Path, dst: &Path) -> io::Result<()> {
    let mut reader = File::open(src)?;
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o755);
    }
    let mut writer = opts.open(dst)?;
    io::copy(&mut reader, &mut writer)?;
    writer.sync_all()?;
    Ok(())
}

fn verify_install_targets_writable(targets: &[(PathBuf, PathBuf)]) -> Result<(), CliError> {
    let mut checked_dirs: Vec<PathBuf> = Vec::new();
    for (_, dest_path) in targets {
        let parent = dest_path.parent().unwrap_or_else(|| Path::new("."));
        if checked_dirs.iter().any(|d| same_file_path(d, parent)) {
            continue;
        }
        let probe_path = parent.join(format!(
            ".liftgate-upgrade-probe.{}.{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe_path)
        {
            Ok(file) => {
                drop(file);
                let _ = fs::remove_file(&probe_path);
            }
            Err(source) => return Err(map_replace_io(dest_path, source)),
        }
        checked_dirs.push(parent.to_path_buf());
    }
    Ok(())
}

fn map_replace_io(target: &Path, source: io::Error) -> CliError {
    if matches!(
        source.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
    ) {
        CliError::UpgradePermissionDenied {
            path: target.to_path_buf(),
            source,
        }
    } else {
        CliError::StateIo {
            path: target.to_path_buf(),
            source,
        }
    }
}

fn validate_repo_slug(repo: &str) -> Result<(), CliError> {
    let trimmed = repo.trim();
    let Some((owner, name)) = trimmed.split_once('/') else {
        return Err(CliError::UpgradeInvalidInput {
            source: UpgradeInputError::InvalidRepoFormat {
                repo: repo.to_owned(),
            },
        });
    };
    let valid_part = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && !s.contains('/')
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    if !valid_part(owner) || !valid_part(name) {
        return Err(CliError::UpgradeInvalidInput {
            source: UpgradeInputError::InvalidRepoSlug {
                repo: repo.to_owned(),
            },
        });
    }
    Ok(())
}

fn normalize_tag_input(raw: &str) -> Result<String, CliError> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.contains("..")
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+'))
    {
        return Err(CliError::UpgradeInvalidInput {
            source: UpgradeInputError::InvalidReleaseTag {
                tag: raw.to_owned(),
            },
        });
    }
    if trimmed.starts_with('v') || trimmed.starts_with('V') {
        Ok(format!("v{}", trimmed.get(1..).unwrap_or_default()))
    } else {
        Ok(format!("v{trimmed}"))
    }
}

fn validate_target_triple(target: &str) -> Result<(), CliError> {
    if target.is_empty()
        || !target
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(CliError::UpgradeInvalidInput {
            source: UpgradeInputError::InvalidTargetTriple {
                target: target.to_owned(),
            },
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::Arc;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use serde_json::json;
    use tokio::net::TcpListener;
    use zip::write::SimpleFileOptions;

    use super::*;

    #[test]
    fn semver_orders_releases_and_prereleases_accurately() {
        let v010 = SemVer::parse("v0.1.0").unwrap();
        let v010_plain = SemVer::parse("0.1.0").unwrap();
        let v020_rc1 = SemVer::parse("v0.2.0-rc.1").unwrap();
        let v020_rc2 = SemVer::parse("v0.2.0-rc.2").unwrap();
        let v020_rc10 = SemVer::parse("v0.2.0-rc.10").unwrap();
        let v020 = SemVer::parse("v0.2.0").unwrap();
        let v100 = SemVer::parse("v1.0.0+20261115").unwrap();

        assert_eq!(v010, v010_plain);
        assert!(v010 < v020_rc1);
        assert!(v020_rc1 < v020_rc2);
        assert!(v020_rc2 < v020_rc10);
        assert!(v020_rc10 < v020);
        assert!(v020 < v100);
    }

    #[test]
    fn sha256sums_parser_handles_gnu_and_bsd_star_prefixes() {
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789ABCDEF";
        let sums = format!(
            "# comment\n{digest}  ./liftgate-x86_64-unknown-linux-musl.tar.gz\n\
             fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210 *liftgate-x86_64-pc-windows-msvc.zip\n"
        );
        assert_eq!(
            lookup_sha256_in_sums(&sums, "liftgate-x86_64-unknown-linux-musl.tar.gz").as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            lookup_sha256_in_sums(&sums, "liftgate-x86_64-pc-windows-msvc.zip").as_deref(),
            Some("fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210")
        );
        assert!(lookup_sha256_in_sums(&sums, "missing.tar.gz").is_none());
    }

    fn build_mock_zip(liftgate_bytes: &[u8], lg_bytes: &[u8]) -> Vec<u8> {
        let mut cursor = io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut cursor);
            let opts =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            zip.start_file("liftgate-x86_64-pc-windows-msvc/liftgate.exe", opts)
                .unwrap();
            zip.write_all(liftgate_bytes).unwrap();
            zip.start_file("liftgate-x86_64-pc-windows-msvc/lg.exe", opts)
                .unwrap();
            zip.write_all(lg_bytes).unwrap();
            zip.finish().unwrap();
        }
        cursor.into_inner()
    }

    fn build_mock_targz(liftgate_bytes: &[u8], lg_bytes: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        {
            let mut tar = tar::Builder::new(&mut enc);
            for (path, data) in [
                (
                    "liftgate-x86_64-unknown-linux-musl/liftgate",
                    liftgate_bytes,
                ),
                ("liftgate-x86_64-unknown-linux-musl/lg", lg_bytes),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                tar.append_data(&mut header, path, data).unwrap();
            }
            tar.finish().unwrap();
        }
        enc.finish().unwrap()
    }

    async fn spawn_mock_release_server(
        tag: &str,
        asset_name: &str,
        archive_bytes: Vec<u8>,
        reported_sha256: &str,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");

        let release_json = json!({
            "tag_name": tag,
            "name": format!("Liftgate {tag}"),
            "html_url": format!("{base_url}/acking-you/liftgate/releases/tag/{tag}"),
            "published_at": "2026-11-15T00:39:51Z",
            "prerelease": false,
            "body": "### Added\n- Fast self-upgrade command",
            "assets": [
                {
                    "name": asset_name,
                    "browser_download_url": format!("{base_url}/download/{asset_name}"),
                    "size": archive_bytes.len(),
                    "digest": format!("sha256:{reported_sha256}")
                },
                {
                    "name": "SHA256SUMS",
                    "browser_download_url": format!("{base_url}/download/SHA256SUMS"),
                    "size": 96
                }
            ]
        })
        .to_string()
        .into_bytes();

        let sums_body = format!("{reported_sha256}  {asset_name}\n").into_bytes();
        let asset_route = format!("/download/{asset_name}");
        let archive_arc = Arc::new(archive_bytes);
        let release_arc = Arc::new(release_json);
        let sums_arc = Arc::new(sums_body);

        tokio::spawn(async move {
            for _ in 0..8 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let archive_arc = Arc::clone(&archive_arc);
                let release_arc = Arc::clone(&release_arc);
                let sums_arc = Arc::clone(&sums_arc);
                let asset_route = asset_route.clone();
                tokio::spawn(async move {
                    let mut req_buf = [0u8; 2048];
                    let n = sock.read(&mut req_buf).await.unwrap_or(0);
                    let req_line = String::from_utf8_lossy(&req_buf[..n]);
                    let path = req_line
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let (status, body): (&str, &[u8]) = if path.starts_with("/repos/") {
                        ("200 OK", release_arc.as_slice())
                    } else if path == "/download/SHA256SUMS" {
                        ("200 OK", sums_arc.as_slice())
                    } else if path == asset_route {
                        ("200 OK", archive_arc.as_slice())
                    } else {
                        ("404 Not Found", b"not found")
                    };
                    let hdr = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(hdr.as_bytes()).await;
                    let _ = sock.write_all(body).await;
                });
            }
        });

        base_url
    }

    #[tokio::test]
    async fn end_to_end_check_and_upgrade_replaces_both_binaries_and_verifies_sha256() {
        let temp_root = std::env::temp_dir().join(format!("lg-upgrade-e2e-{}", Uuid::new_v4()));
        let state_dir = temp_root.join("state");
        let bin_dir = temp_root.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let state_paths = StatePaths::resolve(Some(state_dir)).unwrap();

        // Cross-target override (`x86_64-unknown-linux-musl` or `x86_64-pc-windows-msvc`) that
        // differs from host so we exercise pure-Rust archive extraction + atomic dual replacement
        // deterministically without needing to compile an actual inner executable.
        let mock_target = if COMPILE_TARGET.contains("windows") {
            "x86_64-unknown-linux-musl"
        } else {
            "x86_64-pc-windows-msvc"
        };
        let archive_name = archive_filename_for_target(mock_target);
        let new_liftgate_payload = b"UPGRADED-LIFTGATE-BINARY-V99";
        let new_lg_payload = b"UPGRADED-LG-ALIAS-BINARY-V99";
        let archive_bytes = if mock_target.contains("windows") {
            build_mock_zip(new_liftgate_payload, new_lg_payload)
        } else {
            build_mock_targz(new_liftgate_payload, new_lg_payload)
        };
        let real_sha = hex_lower(&Sha256::digest(&archive_bytes));

        let base_url =
            spawn_mock_release_server("v99.0.0", &archive_name, archive_bytes.clone(), &real_sha)
                .await;

        let ext = if mock_target.contains("windows") {
            ".exe"
        } else {
            ""
        };
        let installed_lg = bin_dir.join(format!("lg{ext}"));
        let installed_liftgate = bin_dir.join(format!("liftgate{ext}"));
        fs::write(&installed_lg, b"OLD-LG-V0").unwrap();
        fs::write(&installed_liftgate, b"OLD-LIFTGATE-V0").unwrap();

        let check_args = UpgradeArgs {
            check: true,
            version: None,
            force: false,
            target: Some(mock_target.to_owned()),
            repo: "acking-you/liftgate".to_owned(),
            proxy: None,
            no_restart: true,
            json: true,
            base_url: Some(base_url.clone()),
        };
        let check_out = execute_upgrade(&state_paths, &check_args, &installed_lg)
            .await
            .unwrap();
        assert_eq!(check_out.status, "update_available");
        assert!(check_out.update_available);
        assert!(!check_out.upgraded);
        assert_eq!(fs::read(&installed_lg).unwrap(), b"OLD-LG-V0");

        let apply_args = UpgradeArgs {
            check: false,
            ..check_args
        };
        let upg_out = execute_upgrade(&state_paths, &apply_args, &installed_lg)
            .await
            .unwrap();
        assert!(upg_out.upgraded);
        assert_eq!(upg_out.sha256.as_deref(), Some(real_sha.as_str()));
        assert_eq!(fs::read(&installed_lg).unwrap(), new_lg_payload);
        assert_eq!(fs::read(&installed_liftgate).unwrap(), new_liftgate_payload);

        // Tampered checksum MUST fail closed without touching installed binaries.
        let bad_sha = "0000000000000000000000000000000000000000000000000000000000000000";
        let bad_url =
            spawn_mock_release_server("v99.1.0", &archive_name, archive_bytes, bad_sha).await;
        let bad_args = UpgradeArgs {
            base_url: Some(bad_url),
            ..apply_args
        };
        let err = execute_upgrade(&state_paths, &bad_args, &installed_lg)
            .await
            .unwrap_err();
        assert!(matches!(err, CliError::UpgradeChecksumMismatch { .. }));
        assert_eq!(fs::read(&installed_lg).unwrap(), new_lg_payload);

        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn unpack_and_plan_install_targets_stages_cli_binaries_and_alias() {
        let temp_root = std::env::temp_dir().join(format!("lg-upgrade-cli-{}", Uuid::new_v4()));
        let stage_dir = temp_root.join("stage");
        let bin_dir = temp_root.join("bin");
        fs::create_dir_all(&stage_dir).unwrap();
        fs::create_dir_all(&bin_dir).unwrap();

        let archive_path = stage_dir.join("liftgate-x86_64-pc-windows-msvc.zip");
        let mut cursor = io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut cursor);
            let opts =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            zip.start_file("liftgate-x86_64-pc-windows-msvc/liftgate.exe", opts)
                .unwrap();
            zip.write_all(b"LIFTGATE-EXE").unwrap();
            zip.start_file("liftgate-x86_64-pc-windows-msvc/lg.exe", opts)
                .unwrap();
            zip.write_all(b"LG-EXE").unwrap();
            zip.finish().unwrap();
        }
        fs::write(&archive_path, cursor.into_inner()).unwrap();

        let staged = unpack_release_binaries(
            &archive_path,
            "liftgate-x86_64-pc-windows-msvc.zip",
            &stage_dir,
        )
        .unwrap();

        let current_cli = bin_dir.join("liftgate.exe");
        fs::write(&current_cli, b"OLD-CLI").unwrap();
        let targets = plan_install_targets(&current_cli, &staged, "x86_64-pc-windows-msvc");
        assert_eq!(targets.len(), 2);
        assert!(same_file_path(&targets[0].1, &current_cli));

        let _ = fs::remove_dir_all(&temp_root);
    }

    #[tokio::test]
    async fn blocks_appimage_and_app_bundle_in_place_upgrade_while_allowing_check() {
        assert_eq!(
            detect_packaged_bundle_kind_with_env(
                Path::new("/tmp/.mount_liftgaAbCdEf/usr/bin/liftgate"),
                None,
                None,
            ),
            Some("an AppImage runtime mount")
        );
        assert_eq!(
            detect_packaged_bundle_kind_with_env(
                Path::new("/tmp/appimage_extracted_1234/usr/bin/liftgate"),
                None,
                None,
            ),
            Some("an AppImage runtime mount")
        );
        assert_eq!(
            detect_packaged_bundle_kind_with_env(
                Path::new("/opt/squash/usr/bin/liftgate"),
                Some(OsStr::new("/home/dev/Apps/Liftgate.AppImage")),
                Some(OsStr::new("/opt/squash")),
            ),
            Some("an AppImage runtime mount")
        );
        assert_eq!(
            detect_packaged_bundle_kind_with_env(
                Path::new("/home/dev/.cargo/bin/lg"),
                Some(OsStr::new("/home/dev/Apps/Liftgate.AppImage")),
                Some(OsStr::new("/tmp/.mount_liftgaAbCdEf")),
            ),
            None
        );
        assert_eq!(
            detect_packaged_bundle_kind_with_env(
                Path::new("/Applications/Liftgate.app/Contents/MacOS/liftgate"),
                None,
                None,
            ),
            Some("a signed macOS .app bundle")
        );
        assert_eq!(
            detect_packaged_bundle_kind_with_env(
                Path::new("/Users/dev/liftgate-aarch64-apple-darwin/liftgate"),
                None,
                None,
            ),
            None
        );

        let temp_root =
            std::env::temp_dir().join(format!("lg-upgrade-appimage-{}", Uuid::new_v4()));
        let state_paths = StatePaths::resolve(Some(temp_root.join("state"))).unwrap();
        let appimage_exe = Path::new("/tmp/.mount_liftgaTest/usr/bin/liftgate");
        let archive_name = if COMPILE_TARGET.contains("windows") {
            format!("liftgate-{COMPILE_TARGET}.zip")
        } else {
            format!("liftgate-{COMPILE_TARGET}.tar.gz")
        };
        let mock_url = spawn_mock_release_server(
            "v99.2.0",
            &archive_name,
            vec![1, 2, 3, 4],
            "0000000000000000000000000000000000000000000000000000000000000000",
        )
        .await;
        let check_outcome = execute_upgrade(
            &state_paths,
            &UpgradeArgs {
                check: true,
                base_url: Some(mock_url),
                ..UpgradeArgs::default()
            },
            appimage_exe,
        )
        .await
        .unwrap();
        assert_eq!(check_outcome.status, "update_available");
        assert!(!check_outcome.upgraded);

        let err = execute_upgrade(
            &state_paths,
            &UpgradeArgs {
                check: false,
                version: Some("v0.2.0".to_owned()),
                ..UpgradeArgs::default()
            },
            appimage_exe,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            CliError::UpgradePackagedBundle {
                ref release_url,
                ..
            } if release_url.ends_with("/releases/tag/v0.2.0")
        ));

        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn parses_http_headers_across_proxy_connect_and_redirects() {
        let headers = concat!(
            "HTTP/1.1 200 Connection established\r\n",
            "\r\n",
            "HTTP/1.1 302 Found\r\n",
            "Location: https://storage.googleapis.com/bucket/cli_windows_x64.exe\r\n",
            "Content-Length: 0\r\n",
            "\r\n",
            "HTTP/2 200\r\n",
            "content-type: application/octet-stream\r\n",
            "Content-Length: 234827416\r\n",
            "\r\n"
        );
        let meta = parse_http_headers_text(headers);
        assert_eq!(meta.status_code, Some(200));
        assert_eq!(meta.content_length, Some(234_827_416));

        let err_meta = parse_http_headers_text("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n");
        assert_eq!(err_meta.status_code, Some(404));
        assert_eq!(err_meta.content_length, Some(0));
    }

    #[tokio::test]
    async fn loopback_download_streams_progress_with_total_bytes_and_rate() {
        let payload = vec![0x5Au8; 256 * 1024];
        let mock_url = spawn_mock_release_server(
            "v99.9.9",
            "liftgate-demo.bin",
            payload.clone(),
            "0000000000000000000000000000000000000000000000000000000000000000",
        )
        .await;
        let dl_url = format!("{mock_url}/download/liftgate-demo.bin");
        let temp_file = std::env::temp_dir().join(format!("lg-dl-prog-{}.bin", Uuid::new_v4()));
        let (tx, rx) = watch::channel(TransferProgress::checking("init"));
        let ctx = DownloadProgressContext {
            tx: &tx,
            asset_name: "liftgate-demo.bin".to_owned(),
            target_version: Some("v99.9.9".to_owned()),
            known_total_bytes: None,
        };

        download_url_with_policy_and_progress(
            &dl_url,
            &temp_file,
            MAX_ARCHIVE_BYTES,
            Duration::from_secs(10),
            None,
            false,
            Some(&ctx),
        )
        .await
        .unwrap();

        let final_prog = rx.borrow().clone();
        assert_eq!(final_prog.stage, TransferStage::Downloading);
        assert_eq!(final_prog.downloaded_bytes, payload.len() as u64);
        assert_eq!(final_prog.total_bytes, Some(payload.len() as u64));
        assert_eq!(final_prog.progress_fraction(), Some(1.0));
        assert_eq!(final_prog.eta_secs, Some(0));
        assert_eq!(fs::read(&temp_file).unwrap(), payload);
        let _ = fs::remove_file(&temp_file);
    }
}
