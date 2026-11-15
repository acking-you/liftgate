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
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use comfy_table::{Attribute, Cell, Color as TableColor};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tar::Archive;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::timeout;
use uuid::Uuid;
use zip::ZipArchive;

use crate::cli::{UpgradeArgs, invoked_bin_name};
use crate::daemon;
use crate::error::CliError;
use crate::state::StatePaths;
use crate::ui::{self, Spinner, Tone};

const CURRENT_VERSION_RAW: &str = env!("CARGO_PKG_VERSION");
const COMPILE_TARGET: &str = env!("LIFTGATE_BUILD_TARGET");

const MAX_METADATA_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_BINARY_EXTRACT_BYTES: u64 = 128 * 1024 * 1024;
const FETCH_METADATA_TIMEOUT: Duration = Duration::from_secs(25);
const FETCH_ARCHIVE_TIMEOUT: Duration = Duration::from_secs(300);
const BACKUP_MARKER: &str = ".old-upgrade";

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

pub async fn run_upgrade(state_paths: &StatePaths, args: UpgradeArgs) -> Result<(), CliError> {
    let current_exe = std::env::current_exe().map_err(|source| CliError::StateIo {
        path: PathBuf::from("current_exe"),
        source,
    })?;
    let outcome = execute_upgrade(state_paths, &args, &current_exe).await?;
    if args.json {
        let text = serde_json::to_string_pretty(&outcome).unwrap_or_else(|_| "{}".to_owned());
        println!("{text}");
    }
    Ok(())
}

async fn execute_upgrade(
    state_paths: &StatePaths,
    args: &UpgradeArgs,
    current_exe: &Path,
) -> Result<UpgradeOutcome, CliError> {
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

    let quiet_ui = args.json;
    let check_spinner =
        (!quiet_ui).then(|| Spinner::start(format!("Checking GitHub Releases ({})…", args.repo)));

    let plan_res = resolve_release_plan(
        &args.repo,
        requested_tag.as_deref(),
        desired_target,
        args.proxy.as_deref(),
        args.base_url.as_deref(),
    )
    .await;

    if let Some(sp) = check_spinner {
        sp.finish_and_clear().await;
    }
    let plan = plan_res?;

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
        if !quiet_ui {
            render_check_summary(&plan, &target_ver_str, cmp);
        }
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
        });
    }

    // Skip download when already up-to-date (or ahead on latest) unless `--force` or explicit `--version` downgrade.
    if !args.force {
        if cmp == Ordering::Equal {
            if !quiet_ui {
                ui::headline(
                    Tone::Ok,
                    &format!("Liftgate is already on the latest release (v{CURRENT_VERSION_RAW})"),
                );
                ui::kv("Target Binary", &current_exe.display().to_string());
                ui::kv_accent("Release Page", &plan.release_url);
                ui::cmd_hint(
                    "Force reinstall",
                    &format!("{} upgrade --force", invoked_bin_name()),
                );
            }
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
            });
        }
        if cmp == Ordering::Greater && requested_tag.is_none() {
            if !quiet_ui {
                ui::headline(
                    Tone::Ok,
                    &format!(
                        "Installed build (v{CURRENT_VERSION_RAW}) is ahead of latest release ({})",
                        plan.tag
                    ),
                );
                ui::cmd_hint(
                    "Install released build anyway",
                    &format!("{} upgrade --force", invoked_bin_name()),
                );
            }
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
            });
        }
    }

    let stage = StageDirGuard::create(state_paths)?;
    let sums_spinner = (!quiet_ui).then(|| {
        Spinner::start(format!(
            "Downloading & verifying {} ({})…",
            plan.archive_name, plan.tag
        ))
    });

    let download_and_stage_res =
        async {
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
                    detail: format!(
                        "`SHA256SUMS` for {} did not contain an entry for `{}`",
                        plan.tag, plan.archive_name
                    ),
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
            download_url_to_path(
                &plan.archive_url,
                &archive_path,
                MAX_ARCHIVE_BYTES,
                FETCH_ARCHIVE_TIMEOUT,
                args.proxy.as_deref(),
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

            let unpacked = unpack_release_binaries(&archive_path, &plan.archive_name, &stage.path)?;
            if plan.target_triple == COMPILE_TARGET {
                smoke_check_staged_binary(&unpacked.liftgate_bin).await?;
            }
            Ok::<(StagedBinaries, String), CliError>((unpacked, actual_sha256))
        }
        .await;

    if let Some(sp) = sums_spinner {
        sp.finish_and_clear().await;
    }
    let (staged, verified_sha256) = download_and_stage_res?;

    // Check if a managed background daemon is running so we unlock `liftgate.exe` on Windows
    // and bring the daemon back online running the newly installed version.
    let active_daemon_before = state_paths
        .read_daemon()?
        .filter(|d| daemon::is_pid_alive(d.pid));
    let saved_daemon_serve = active_daemon_before
        .as_ref()
        .filter(|_| !args.no_restart)
        .map(daemon::serve_args_from_daemon_state);

    if saved_daemon_serve.is_some() {
        let _ = daemon::stop_daemon(state_paths, true).await?;
    }

    let install_targets = plan_install_targets(current_exe, &staged, &plan.target_triple);
    let mut installed_paths = Vec::with_capacity(install_targets.len());
    for (staged_src, dest_path) in &install_targets {
        replace_executable_atomic(staged_src, dest_path)?;
        installed_paths.push(dest_path.display().to_string());
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
        if daemon::start_daemon_with_exe(state_paths, new_daemon_exe, serve_cfg, false, true)
            .await
            .is_ok()
        {
            daemon_restarted = true;
        }
    }

    if !quiet_ui {
        ui::headline(
            Tone::Ok,
            &format!(
                "Upgraded Liftgate CLI  v{CURRENT_VERSION_RAW} → v{target_ver_str} ({})",
                plan.tag
            ),
        );
        ui::kv("Platform", &plan.target_triple);
        ui::kv("Verified SHA256", &verified_sha256);
        for p in &installed_paths {
            ui::kv_accent("Updated Binary", p);
        }
        if daemon_restarted {
            ui::kv("Gateway Daemon", "restarted cleanly on upgraded binary");
        }
        ui::kv_accent("Release Notes", &plan.release_url);
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
    })
}

fn render_check_summary(plan: &ResolvedReleasePlan, target_ver: &str, cmp: Ordering) {
    let bin = invoked_bin_name();
    let (tone, badge_str, state_label) = match cmp {
        Ordering::Less => (Tone::Warn, "UPDATE AVAILABLE", "newer release ready"),
        Ordering::Equal => (Tone::Ok, "UP TO DATE", "running latest release"),
        Ordering::Greater => (Tone::Info, "AHEAD OF RELEASE", "local build is newer"),
    };

    ui::banner(
        "Liftgate Release Check",
        Some((tone, badge_str)),
        Some(&format!(
            "installed=v{CURRENT_VERSION_RAW} · latest=v{target_ver} · repo={}",
            plan.repo
        )),
    );

    let mut table = ui::new_table(&["Property", "Value"]);
    let ver_color = match cmp {
        Ordering::Less => TableColor::Yellow,
        Ordering::Equal => TableColor::Green,
        Ordering::Greater => TableColor::Cyan,
    };
    let state_cell = Cell::new(format!("{state_label} ({})", plan.release_title));
    table.add_row(vec![
        ui::dim_cell("Status"),
        if ui::use_color() {
            state_cell.fg(ver_color).add_attribute(Attribute::Bold)
        } else {
            state_cell
        },
    ]);
    table.add_row(vec![
        ui::dim_cell("Installed"),
        Cell::new(format!("v{CURRENT_VERSION_RAW}")),
    ]);
    let release_cell = Cell::new(format!(
        "{} ({})",
        plan.tag,
        if plan.prerelease {
            "pre-release"
        } else {
            "stable"
        }
    ));
    table.add_row(vec![
        ui::dim_cell("Target Release"),
        if ui::use_color() {
            release_cell.fg(ver_color).add_attribute(Attribute::Bold)
        } else {
            release_cell
        },
    ]);
    table.add_row(vec![
        ui::dim_cell("Platform Asset"),
        Cell::new(match plan.archive_size {
            Some(sz) if sz > 0 => format!("{} ({})", plan.archive_name, format_bytes(sz)),
            _ => plan.archive_name.clone(),
        }),
    ]);
    if let Some(pub_at) = &plan.published_at {
        table.add_row(vec![ui::dim_cell("Published At"), Cell::new(pub_at)]);
    }
    table.add_row(vec![
        ui::dim_cell("Release URL"),
        ui::accent_cell(&plan.release_url),
    ]);
    println!();
    println!("{table}");

    if let Some(excerpt) = &plan.notes_excerpt {
        println!();
        ui::note(Tone::Muted, excerpt);
    }
    if cmp == Ordering::Less {
        println!();
        ui::cmd_hint("Download & install now", &format!("{bin} upgrade"));
    }
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
                serde_json::from_slice(&raw_json).map_err(|err| CliError::UpgradeNetwork {
                    url: api_url.clone(),
                    detail: format!("invalid GitHub release JSON: {err}"),
                })?;
            build_plan_from_api_release(repo, &web_base, rel, desired_target)
        }
        Err(CliError::UpgradeNetwork { detail, .. })
            if base_url_override.is_none()
                && (detail.contains("HTTP 403") || detail.contains("HTTP 429")) =>
        {
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
            detail: "timed out resolving latest release tag".to_owned(),
        })?
        .map_err(|err| CliError::UpgradeNetwork {
            url: latest_url.clone(),
            detail: format!("failed to invoke curl: {err}"),
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
        detail: "could not resolve latest release tag from GitHub redirect".to_owned(),
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
    )
    .await;
    if let Err(err) = res {
        let _ = fs::remove_file(&temp_file);
        return Err(err);
    }
    let bytes = read_file_bounded(&temp_file, max_bytes).map_err(|source| CliError::StateIo {
        path: temp_file.clone(),
        source,
    });
    let _ = fs::remove_file(&temp_file);
    bytes
}

async fn download_url_to_path(
    url: &str,
    dest: &Path,
    max_bytes: u64,
    max_duration: Duration,
    proxy: Option<&str>,
) -> Result<(), CliError> {
    download_url_with_policy(url, dest, max_bytes, max_duration, proxy, false).await
}

pub(crate) async fn download_url_with_policy(
    url: &str,
    dest: &Path,
    max_bytes: u64,
    max_duration: Duration,
    proxy: Option<&str>,
    force_direct_when_no_proxy: bool,
) -> Result<(), CliError> {
    if let Some(loopback) = parse_loopback_http_url(url) {
        let bytes = fetch_loopback_http(&loopback, max_bytes, max_duration).await?;
        fs::write(dest, &bytes).map_err(|source| CliError::StateIo {
            path: dest.to_path_buf(),
            source,
        })?;
        return Ok(());
    }
    download_https_via_system_tool(
        url,
        dest,
        max_bytes,
        max_duration,
        proxy,
        force_direct_when_no_proxy,
        false,
    )
    .await
}

async fn download_https_via_system_tool(
    url: &str,
    dest: &Path,
    max_bytes: u64,
    max_duration: Duration,
    proxy: Option<&str>,
    force_direct_when_no_proxy: bool,
    accept_github_json: bool,
) -> Result<(), CliError> {
    if !url.starts_with("https://") {
        return Err(CliError::UpgradeInvalidInput {
            reason: format!("refusing non-HTTPS upgrade URL `{url}`"),
        });
    }

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
        "-o",
    ])
    .arg(dest)
    .args(["-w", "%{http_code}"])
    .arg(url)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

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
            let out = timeout(
                max_duration.saturating_add(Duration::from_secs(5)),
                child.wait_with_output(),
            )
            .await
            .map_err(|_| CliError::UpgradeNetwork {
                url: url.to_owned(),
                detail: "request timed out".to_owned(),
            })?
            .map_err(|err| CliError::UpgradeNetwork {
                url: url.to_owned(),
                detail: format!("I/O error waiting on curl: {err}"),
            })?;

            let status_code = String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<u16>()
                .unwrap_or(0);
            if status_code >= 400 {
                let _ = fs::remove_file(dest);
                return Err(CliError::UpgradeNetwork {
                    url: url.to_owned(),
                    detail: format!("HTTP {status_code}"),
                });
            }
            if !out.status.success() {
                let _ = fs::remove_file(dest);
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
                return Err(CliError::UpgradeNetwork {
                    url: url.to_owned(),
                    detail: if stderr.is_empty() {
                        format!("curl exited with {}", out.status)
                    } else {
                        stderr
                    },
                });
            }
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            download_https_fallback(url, dest, max_duration, proxy, force_direct_when_no_proxy)
                .await
        }
        Err(err) => Err(CliError::UpgradeNetwork {
            url: url.to_owned(),
            detail: format!("failed to spawn curl: {err}"),
        }),
    }
}

async fn download_https_fallback(
    url: &str,
    dest: &Path,
    max_duration: Duration,
    proxy: Option<&str>,
    force_direct_when_no_proxy: bool,
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
        .stderr(Stdio::piped());
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
        let out = timeout(max_duration, cmd.output())
            .await
            .map_err(|_| CliError::UpgradeNetwork {
                url: url.to_owned(),
                detail: "PowerShell download timed out".to_owned(),
            })?
            .map_err(|err| CliError::UpgradeNetwork {
                url: url.to_owned(),
                detail: format!("failed to invoke curl.exe or powershell.exe: {err}"),
            })?;
        if !out.status.success() {
            let _ = fs::remove_file(dest);
            return Err(CliError::UpgradeNetwork {
                url: url.to_owned(),
                detail: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
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
                .stderr(Stdio::piped());
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
                    let out = timeout(max_duration, child.wait_with_output())
                        .await
                        .map_err(|_| CliError::UpgradeNetwork {
                            url: url.to_owned(),
                            detail: format!("{tool} download timed out"),
                        })?
                        .map_err(|err| CliError::UpgradeNetwork {
                            url: url.to_owned(),
                            detail: format!("{tool} I/O error: {err}"),
                        })?;
                    if out.status.success() {
                        return Ok(());
                    }
                    let _ = fs::remove_file(dest);
                    return Err(CliError::UpgradeNetwork {
                        url: url.to_owned(),
                        detail: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
                    });
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(CliError::UpgradeNetwork {
                        url: url.to_owned(),
                        detail: format!("failed to spawn {tool}: {err}"),
                    });
                }
            }
        }
        Err(CliError::UpgradeNetwork {
            url: url.to_owned(),
            detail: "neither `curl`, `wget`, nor `fetch` was found on PATH".to_owned(),
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

async fn fetch_loopback_http(
    target: &LoopbackUrl,
    max_bytes: u64,
    max_duration: Duration,
) -> Result<Vec<u8>, CliError> {
    timeout(max_duration, async {
        let mut stream = TcpStream::connect(target.addr).await.map_err(|err| {
            CliError::UpgradeNetwork {
                url: target.raw_url.clone(),
                detail: format!("connect failed: {err}"),
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
                detail: format!("write failed: {err}"),
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
                    detail: format!("read failed: {err}"),
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
                    detail: "response exceeded maximum allowed byte size".to_owned(),
                });
            }
        }

        let Some(hdr_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
            return Err(CliError::UpgradeNetwork {
                url: target.raw_url.clone(),
                detail: "malformed HTTP response header".to_owned(),
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
                detail: format!("HTTP {status}"),
            });
        }
        if u64::try_from(body_slice.len()).unwrap_or(u64::MAX) > max_bytes {
            return Err(CliError::UpgradeNetwork {
                url: target.raw_url.clone(),
                detail: "body exceeded maximum allowed byte size".to_owned(),
            });
        }
        Ok(body_slice.to_vec())
    })
    .await
    .map_err(|_| CliError::UpgradeNetwork {
        url: target.raw_url.clone(),
        detail: "loopback request timed out".to_owned(),
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

fn read_file_bounded(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut out = Vec::new();
    let mut limited = file.take(max_bytes.saturating_add(1));
    limited.read_to_end(&mut out)?;
    if u64::try_from(out.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file exceeds maximum allowed download size",
        ));
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
            detail: format!("archive did not contain `{expected_liftgate}`"),
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
    let mut zip = ZipArchive::new(file).map_err(|err| CliError::UpgradeArchive {
        asset: archive_name.to_owned(),
        detail: format!("corrupt ZIP archive: {err}"),
    })?;

    for idx in 0..zip.len() {
        let entry = zip.by_index(idx).map_err(|err| CliError::UpgradeArchive {
            asset: archive_name.to_owned(),
            detail: format!("failed reading ZIP entry #{idx}: {err}"),
        })?;
        if entry.is_dir() || entry.is_symlink() {
            continue;
        }
        let base_name = entry
            .name()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(entry.name());
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
    let entries = archive.entries().map_err(|err| CliError::UpgradeArchive {
        asset: archive_name.to_owned(),
        detail: format!("corrupt tar.gz archive: {err}"),
    })?;

    for item in entries {
        let entry = item.map_err(|err| CliError::UpgradeArchive {
            asset: archive_name.to_owned(),
            detail: format!("corrupt tar entry: {err}"),
        })?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let entry_path = entry.path().map_err(|err| CliError::UpgradeArchive {
            asset: archive_name.to_owned(),
            detail: format!("invalid tar entry path: {err}"),
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
    let copied = io::copy(&mut limited, &mut out).map_err(|err| CliError::UpgradeArchive {
        asset: archive_name.to_owned(),
        detail: format!("decompression failed: {err}"),
    })?;
    if copied > MAX_BINARY_EXTRACT_BYTES {
        let _ = fs::remove_file(dest);
        return Err(CliError::UpgradeArchive {
            asset: archive_name.to_owned(),
            detail: "extracted binary exceeded 128 MiB safety limit".to_owned(),
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
            detail: "staged binary `--version` smoke check timed out".to_owned(),
        })?
        .map_err(|err| CliError::UpgradeArchive {
            asset: staged_exe.display().to_string(),
            detail: format!("staged binary failed to execute on this host: {err}"),
        })?;
    if !out.status.success() {
        return Err(CliError::UpgradeArchive {
            asset: staged_exe.display().to_string(),
            detail: format!(
                "staged binary `--version` exited with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
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
    let primary_src = if current_stem.eq_ignore_ascii_case("liftgate") {
        staged.liftgate_bin.clone()
    } else {
        staged.lg_bin.clone()
    };
    targets.push((primary_src, current_exe.to_path_buf()));

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

fn map_replace_io(target: &Path, source: io::Error) -> CliError {
    if source.kind() == io::ErrorKind::PermissionDenied {
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
            reason: format!("repository `{repo}` must be in `owner/name` format"),
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
            reason: format!("invalid GitHub repository slug `{repo}`"),
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
            reason: format!("invalid release version/tag `{raw}`"),
        });
    }
    if trimmed.starts_with('v') || trimmed.starts_with('V') {
        Ok(format!("v{}", &trimmed[1..]))
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
            reason: format!("invalid target triple `{target}`"),
        });
    }
    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const KIB: f64 = 1024.0;
    if bytes >= 1024 * 1024 {
        format!("{:.2} MiB", (bytes as f64) / MIB)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", (bytes as f64) / KIB)
    } else {
        format!("{bytes} B")
    }
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
}
