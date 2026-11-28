//! Unified terminal visual system for `liftgate` (`lg`).
//!
//! Provides semantic status headlines, aligned key-value blocks, rounded UTF-8
//! tables (`comfy-table`), animated non-blocking progress spinners, structured
//! log highlighting, and human-friendly metric formatters. Every helper honors
//! `NO_COLOR`, `TERM=dumb`, `CLICOLOR_FORCE`, and `IsTerminal` independently on
//! stdout and stderr so piped output stays 100% ANSI-free.

use std::env;
use std::fmt::Write as _;
use std::io::{IsTerminal as _, Write as _, stderr, stdout};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use comfy_table::{Attribute, Cell, CellAlignment, Color, ContentArrangement, Table, presets};
use liftgate_core::ReportTone;
use owo_colors::{OwoColorize as _, Style};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use liftgate_core::error::{CliError, recovery_hint_for_error};
pub use liftgate_core::format::{
    format_bytes, format_duration_secs, format_eta, format_latency_ms, format_relative_ms,
    format_tokens, format_transfer_rate, format_utc_hms, progress_bar,
};
use liftgate_core::upgrade::{TransferProgress, TransferStage};

const FALLBACK_TABLE_WIDTH: u16 = 120;
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Returns `true` when ANSI colors should be emitted on `stdout`.
#[must_use]
pub fn use_color() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| color_allowed_for_stream(stdout().is_terminal()))
}

/// Returns `true` when ANSI colors should be emitted on `stderr`.
#[must_use]
pub fn use_color_stderr() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| color_allowed_for_stream(stderr().is_terminal()))
}

fn color_allowed_for_stream(is_tty: bool) -> bool {
    if env::var_os("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    if env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if env::var("TERM").is_ok_and(|t| t.eq_ignore_ascii_case("dumb")) {
        return false;
    }
    is_tty
}

/// Semantic tone of a CLI status line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Settled healthy outcome (`✓` green).
    Ok,
    /// Active/informational state (`●` cyan).
    Info,
    /// Recommended next step (`→` cyan).
    Action,
    /// State transition or non-fatal caution (`⚠` yellow).
    Warn,
    /// Error or offline failure (`✗` red).
    Err,
    /// Low-emphasis no-op (`·` dim).
    Muted,
}

impl From<ReportTone> for Tone {
    fn from(value: ReportTone) -> Self {
        match value {
            ReportTone::Ok => Self::Ok,
            ReportTone::Info => Self::Info,
            ReportTone::Action => Self::Action,
            ReportTone::Warn => Self::Warn,
            ReportTone::Err => Self::Err,
            ReportTone::Muted => Self::Muted,
        }
    }
}

impl Tone {
    #[must_use]
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Ok => "✓",
            Self::Info => "●",
            Self::Action => "→",
            Self::Warn => "⚠",
            Self::Err => "✗",
            Self::Muted => "·",
        }
    }

    #[must_use]
    fn style(self) -> Style {
        match self {
            Self::Ok => Style::new().green(),
            Self::Info | Self::Action => Style::new().cyan(),
            Self::Warn => Style::new().yellow(),
            Self::Err => Style::new().red(),
            Self::Muted => Style::new().dimmed(),
        }
    }
}

#[must_use]
pub fn paint(text: &str, style: Style) -> String {
    if use_color() {
        text.style(style).to_string()
    } else {
        text.to_owned()
    }
}

#[must_use]
pub fn paint_stderr(text: &str, style: Style) -> String {
    if use_color_stderr() {
        text.style(style).to_string()
    } else {
        text.to_owned()
    }
}

/// Prints a prominent banner header with an optional right-side subtitle/pill.
pub fn banner(title: &str, pill: Option<(Tone, &str)>, subtitle: Option<&str>) {
    let brand = paint(title, Style::new().bold());
    let mut line = brand;
    if let Some((tone, badge)) = pill {
        let glyph = paint(tone.glyph(), tone.style().bold());
        let text = paint(badge, tone.style().bold());
        let _ = write!(line, "  {glyph} {text}");
    }
    if let Some(sub) = subtitle.filter(|s| !s.is_empty()) {
        let dim_sub = paint(&format!("· {sub}"), Style::new().dimmed());
        let _ = write!(line, "  {dim_sub}");
    }
    println!("{line}");
}

/// Prints a semantic status headline (`✓ Title`).
pub fn headline(tone: Tone, title: &str) {
    let glyph = paint(tone.glyph(), tone.style().bold());
    let text = match tone {
        Tone::Muted => paint(title, Style::new().dimmed()),
        _ => paint(title, Style::new().bold()),
    };
    println!("{glyph} {text}");
}

/// Prints a sub-section divider title.
pub fn section(title: &str) {
    println!();
    let label = paint(title, Style::new().cyan().bold());
    println!("  {label}");
}

/// Prints an aligned `label` + `value` row beneath a headline or banner.
pub fn kv(label: &str, value: &str) {
    let padded = format!("{label:<15}");
    let dim_label = paint(&padded, Style::new().dimmed());
    println!("    {dim_label} {value}");
}

/// Prints an aligned `label` + accented URL/identifier row.
pub fn kv_accent(label: &str, value: &str) {
    let styled_val = paint(value, Style::new().cyan());
    kv(label, &styled_val);
}

/// Prints an indented note or tip line.
pub fn note(tone: Tone, msg: &str) {
    let glyph = paint(tone.glyph(), tone.style());
    let body = match tone {
        Tone::Muted => paint(msg, Style::new().dimmed()),
        Tone::Warn => paint(msg, Style::new().yellow()),
        _ => msg.to_owned(),
    };
    println!("    {glyph} {body}");
}

/// Prints an actionable next-step command suggestion.
pub fn cmd_hint(description: &str, command: &str) {
    let arrow = paint("→", Style::new().cyan().bold());
    let desc = paint(description, Style::new().dimmed());
    let cmd = paint(command, Style::new().cyan().bold());
    println!("    {arrow} {desc}: {cmd}");
}

/// Prints a structured error tree and contextual recovery hint to `stderr`.
pub fn print_cli_error(err: &CliError, bin_name: &str) {
    let glyph = paint_stderr("✗", Style::new().red().bold());
    let title = paint_stderr(&err.to_string(), Style::new().red().bold());
    eprintln!("{glyph} {title}");

    let mut causes = Vec::new();
    let mut current = std::error::Error::source(err);
    while let Some(cause) = current {
        causes.push(cause.to_string());
        current = cause.source();
    }
    for (idx, cause) in causes.iter().enumerate() {
        let branch = if idx + 1 == causes.len() {
            "└─"
        } else {
            "├─"
        };
        let dim_branch = paint_stderr(branch, Style::new().dimmed());
        let dim_label = paint_stderr("caused by:", Style::new().dimmed());
        eprintln!("  {dim_branch} {dim_label} {cause}");
    }

    if let Some(hint) = recovery_hint_for_error(err, bin_name) {
        let arrow = paint_stderr("→", Style::new().cyan().bold());
        let text = paint_stderr(&hint, Style::new().cyan());
        eprintln!("  {arrow} {text}");
    }
}

/// Prints a concise foreground startup banner to `stderr` when attached to a terminal.
#[expect(
    clippy::too_many_arguments,
    reason = "startup banner summarizes all active server settings in one call"
)]
pub fn print_serve_ready_banner(
    listen_url: &str,
    dashboard_url: &str,
    default_model: &str,
    acp_mode: &str,
    auth_enabled: bool,
    full_ui_embedded: bool,
    agy_proxy_display: Option<&str>,
    lg_auto_update: bool,
    agy_auto_update: bool,
    vision_offload: bool,
    mcp_tool_bridge: bool,
) {
    if !stderr().is_terminal() {
        return;
    }
    let ok = paint_stderr("●", Style::new().green().bold());
    let title = paint_stderr("Liftgate foreground server listening", Style::new().bold());
    let ver = paint_stderr(
        &format!("v{}", env!("CARGO_PKG_VERSION")),
        Style::new().dimmed(),
    );
    eprintln!();
    eprintln!("  {ok} {title} {ver}");
    let rows = [
        ("Gateway API", format!("{listen_url}/")),
        ("Web Dashboard", format!("{dashboard_url}/")),
        ("OpenAI API", format!("{listen_url}/v1/responses")),
        ("Anthropic API", format!("{listen_url}/v1/messages")),
        (
            "Default Model",
            format!("{default_model} (mode: {acp_mode})"),
        ),
        (
            "AGY Policy",
            format!(
                "proxy={} · lg-auto-update={} · agy-auto-update={} · vision-offload={} · mcp-bridge={}",
                agy_proxy_display.unwrap_or("none"),
                if lg_auto_update { "on" } else { "off" },
                if agy_auto_update { "on" } else { "off" },
                if vision_offload { "on" } else { "off" },
                if mcp_tool_bridge { "on" } else { "off" }
            ),
        ),
        (
            "Security & UI",
            format!(
                "auth={} · ui={}",
                if auth_enabled {
                    "api-key"
                } else {
                    "open-loopback"
                },
                if full_ui_embedded {
                    "react-bundle"
                } else {
                    "sse-lite"
                }
            ),
        ),
    ];
    for (k, v) in rows {
        let key = paint_stderr(&format!("{k:<15}"), Style::new().dimmed());
        let val = if v.starts_with("http://") {
            paint_stderr(&v, Style::new().cyan())
        } else {
            v
        };
        eprintln!("    {key} {val}");
    }
    let stop_hint = paint_stderr(
        "Press Ctrl+C to drain & stop gracefully.",
        Style::new().dimmed(),
    );
    eprintln!("    {stop_hint}");
    eprintln!();
}

/// Builds a `comfy_table::Table` styled as a rounded UTF-8 box on color TTYs,
/// degrading cleanly to frameless single-line output when piped or `NO_COLOR`.
#[must_use]
pub fn new_table(headers: &[&str]) -> Table {
    let colored = use_color();
    let framed = stdout().is_terminal() || colored;
    let mut table = Table::new();
    if framed {
        table.load_style(presets::UTF8_FULL.with_rounded_corners());
        if colored {
            table.enforce_styling();
        }
        table.set_content_arrangement(ContentArrangement::Dynamic);
        if terminal_width().is_none() {
            table.set_width(FALLBACK_TABLE_WIDTH);
        }
    } else {
        table.load_style(presets::NOTHING);
    }

    let header_cells: Vec<Cell> = headers
        .iter()
        .map(|&h| {
            let cell = Cell::new(h);
            if colored {
                cell.fg(Color::Cyan).add_attribute(Attribute::Bold)
            } else {
                cell
            }
        })
        .collect();
    table.set_header(header_cells);
    table
}

#[must_use]
pub fn status_cell(label: &str, tone: Tone) -> Cell {
    let text = format!("{} {label}", tone.glyph());
    let cell = Cell::new(text);
    if !use_color() {
        return cell;
    }
    match tone {
        Tone::Ok => cell.fg(Color::Green).add_attribute(Attribute::Bold),
        Tone::Info | Tone::Action => cell.fg(Color::Cyan),
        Tone::Warn => cell.fg(Color::Yellow),
        Tone::Err => cell.fg(Color::Red).add_attribute(Attribute::Bold),
        Tone::Muted => cell.fg(Color::DarkGrey),
    }
}

#[must_use]
pub fn accent_cell(text: impl Into<String>) -> Cell {
    let cell = Cell::new(text.into());
    if use_color() {
        cell.fg(Color::Cyan)
    } else {
        cell
    }
}

#[must_use]
pub fn dim_cell(text: impl Into<String>) -> Cell {
    let cell = Cell::new(text.into());
    if use_color() {
        cell.fg(Color::DarkGrey)
    } else {
        cell
    }
}

#[must_use]
pub fn right_cell(text: impl Into<String>) -> Cell {
    Cell::new(text.into()).set_alignment(CellAlignment::Right)
}

fn terminal_width() -> Option<u16> {
    match crossterm::terminal::size() {
        Ok((cols, _)) if cols > 0 => Some(cols),
        _ => None,
    }
}

/// Non-blocking animated stderr spinner that cleans up its terminal line on `Drop`.
pub struct Spinner {
    stop_tx: Option<watch::Sender<bool>>,
    join_handle: Option<JoinHandle<()>>,
    animated: bool,
}

impl Spinner {
    #[must_use]
    pub fn start(message: impl Into<String>) -> Self {
        if crate::tui::is_tui_raw_active() {
            return Self {
                stop_tx: None,
                join_handle: None,
                animated: false,
            };
        }
        let msg = message.into();
        let animated = stderr().is_terminal() && use_color_stderr();
        if !animated {
            if stderr().is_terminal() {
                eprintln!("· {msg}");
            }
            return Self {
                stop_tx: None,
                join_handle: None,
                animated: false,
            };
        }

        let (stop_tx, mut stop_rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            let mut tick_idx = 0usize;
            loop {
                let frame = SPINNER_FRAMES
                    .get(tick_idx % SPINNER_FRAMES.len())
                    .copied()
                    .unwrap_or("⠋");
                let glyph = frame.style(Style::new().cyan().bold()).to_string();
                let _ = write!(stderr(), "\r\x1b[2K{glyph} {msg}");
                let _ = stderr().flush();
                tick_idx = tick_idx.wrapping_add(1);

                tokio::select! {
                    biased;
                    _ = stop_rx.wait_for(|&stopped| stopped) => break,
                    () = tokio::time::sleep(Duration::from_millis(80)) => {}
                }
            }
        });

        Self {
            stop_tx: Some(stop_tx),
            join_handle: Some(handle),
            animated: true,
        }
    }

    pub async fn finish_and_clear(mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send_replace(true);
        }
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.await;
        }
        if self.animated {
            let _ = write!(stderr(), "\r\x1b[2K");
            let _ = stderr().flush();
            self.animated = false;
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send_replace(true);
        }
        if let Some(handle) = self.join_handle.take() {
            handle.abort();
        }
        if self.animated {
            let _ = write!(stderr(), "\r\x1b[2K");
            let _ = stderr().flush();
        }
    }
}

/// Final summary stats captured while [`TransferProgressDisplay`] observed a transfer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransferDisplayStats {
    pub downloaded_bytes: u64,
}

/// Non-blocking animated stderr progress display for verified downloads & binary upgrades.
///
/// Renders a live stage header, smooth Unicode progress bar, downloaded/total bytes,
/// transfer speed, and ETA on color TTYs; degrades cleanly to single-line stage notices on
/// plain terminals and remains silent under `--json`, non-TTY pipes, or active raw TUI mode.
pub struct TransferProgressDisplay {
    stop_tx: Option<watch::Sender<bool>>,
    join_handle: Option<JoinHandle<(usize, u64)>>,
    animated: bool,
}

impl TransferProgressDisplay {
    #[must_use]
    pub fn start(mut progress_rx: watch::Receiver<TransferProgress>) -> Self {
        if crate::tui::is_tui_raw_active() {
            return Self {
                stop_tx: None,
                join_handle: None,
                animated: false,
            };
        }

        let is_tty = stderr().is_terminal();
        let is_dumb_term = env::var("TERM").is_ok_and(|t| t.eq_ignore_ascii_case("dumb"));
        let animated = is_tty && !is_dumb_term;
        if !is_tty {
            return Self {
                stop_tx: None,
                join_handle: None,
                animated: false,
            };
        }

        let (stop_tx, mut stop_rx) = watch::channel(false);
        if !animated {
            let handle = tokio::spawn(async move {
                let mut last_key: Option<(TransferStage, String)> = None;
                let mut last_dl_log = Instant::now();
                let mut peak_downloaded = 0u64;
                let print_plain = |snap: &TransferProgress,
                                   last: &mut Option<(TransferStage, String)>,
                                   last_dl: &mut Instant,
                                   peak: &mut u64| {
                    *peak = (*peak).max(snap.downloaded_bytes);
                    let key = (snap.stage, snap.message.clone());
                    if last.as_ref() != Some(&key) {
                        eprintln!("· [{}/6] {}", snap.stage.step_index(), snap.message);
                        *last = Some(key);
                        *last_dl = Instant::now();
                    } else if snap.stage == TransferStage::Downloading
                        && snap.downloaded_bytes > 0
                        && last_dl.elapsed() >= Duration::from_secs(5)
                    {
                        *last_dl = Instant::now();
                        let rate = format_transfer_rate(snap.bytes_per_sec);
                        match snap.total_bytes.filter(|&t| t > 0) {
                            Some(total) => {
                                let pct = snap.progress_fraction().unwrap_or(0.0) * 100.0;
                                let eta = snap
                                    .eta_secs
                                    .map(|s| format!(" · ETA {}", format_eta(s)))
                                    .unwrap_or_default();
                                eprintln!(
                                    "· [{}/6] {:.1}% ({} / {} · {rate}{eta})",
                                    snap.stage.step_index(),
                                    pct,
                                    format_bytes(snap.downloaded_bytes),
                                    format_bytes(total)
                                );
                            }
                            None => {
                                eprintln!(
                                    "· [{}/6] {} · {rate}",
                                    snap.stage.step_index(),
                                    format_bytes(snap.downloaded_bytes)
                                );
                            }
                        }
                    }
                };
                print_plain(
                    &progress_rx.borrow().clone(),
                    &mut last_key,
                    &mut last_dl_log,
                    &mut peak_downloaded,
                );
                loop {
                    tokio::select! {
                        biased;
                        _ = stop_rx.wait_for(|&stopped| stopped) => break,
                        changed = progress_rx.changed() => {
                            if changed.is_err() {
                                break;
                            }
                            print_plain(
                                &progress_rx.borrow().clone(),
                                &mut last_key,
                                &mut last_dl_log,
                                &mut peak_downloaded,
                            );
                        }
                    }
                }
                (0, peak_downloaded)
            });
            return Self {
                stop_tx: Some(stop_tx),
                join_handle: Some(handle),
                animated: false,
            };
        }

        let handle = tokio::spawn(async move {
            let mut tick_idx = 0usize;
            let mut rendered_lines = 0usize;
            let mut peak_downloaded = 0u64;
            loop {
                let snap = progress_rx.borrow().clone();
                peak_downloaded = peak_downloaded.max(snap.downloaded_bytes);
                let width = usize::from(terminal_width().unwrap_or(100));
                let colored = use_color_stderr();
                let lines = format_transfer_lines(&snap, tick_idx, width, colored);
                redraw_stderr_lines(&lines, rendered_lines);
                rendered_lines = lines.len();
                tick_idx = tick_idx.wrapping_add(1);

                tokio::select! {
                    biased;
                    _ = stop_rx.wait_for(|&stopped| stopped) => break,
                    changed = progress_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                    () = tokio::time::sleep(Duration::from_millis(80)) => {}
                }
            }
            (rendered_lines, peak_downloaded)
        });

        Self {
            stop_tx: Some(stop_tx),
            join_handle: Some(handle),
            animated: true,
        }
    }

    pub async fn finish_and_clear(mut self) -> TransferDisplayStats {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send_replace(true);
        }
        let (rendered_lines, downloaded_bytes) = if let Some(handle) = self.join_handle.take() {
            handle.await.unwrap_or((1, 0))
        } else {
            (0, 0)
        };
        if self.animated && rendered_lines > 0 {
            clear_stderr_lines(rendered_lines);
            self.animated = false;
        }
        TransferDisplayStats { downloaded_bytes }
    }
}

impl Drop for TransferProgressDisplay {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send_replace(true);
        }
        if let Some(handle) = self.join_handle.take() {
            handle.abort();
        }
        if self.animated {
            clear_stderr_lines(2);
        }
    }
}

fn redraw_stderr_lines(lines: &[String], prev_lines: usize) {
    let mut out = String::with_capacity(256);
    if prev_lines > 1 {
        let _ = write!(out, "\x1b[{}A", prev_lines - 1);
    }
    out.push_str("\r\x1b[J");
    for (idx, line) in lines.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }
    let _ = write!(stderr(), "{out}");
    let _ = stderr().flush();
}

fn clear_stderr_lines(lines: usize) {
    let mut out = String::with_capacity(32);
    if lines > 1 {
        let _ = write!(out, "\x1b[{}A", lines - 1);
    }
    out.push_str("\r\x1b[J");
    let _ = write!(stderr(), "{out}");
    let _ = stderr().flush();
}

fn style_if(text: &str, style: Style, colored: bool) -> String {
    if colored {
        text.style(style).to_string()
    } else {
        text.to_owned()
    }
}

fn format_transfer_lines(
    snap: &TransferProgress,
    tick_idx: usize,
    cols: usize,
    colored: bool,
) -> Vec<String> {
    let frame = SPINNER_FRAMES
        .get(tick_idx % SPINNER_FRAMES.len())
        .copied()
        .unwrap_or("⠋");
    let glyph = style_if(frame, Style::new().cyan().bold(), colored);
    let step_tag = style_if(
        &format!("[{}/6]", snap.stage.step_index()),
        Style::new().dimmed(),
        colored,
    );
    let stage_lbl = style_if(snap.stage.label(), Style::new().cyan().bold(), colored);
    let max_body = cols.saturating_sub(24).max(20);

    if snap.stage != TransferStage::Downloading {
        let msg = truncate_display_text(&snap.message, max_body);
        return vec![format!("{glyph} {step_tag} {stage_lbl}  {msg}")];
    }

    let mut title = snap
        .asset_name
        .as_deref()
        .unwrap_or(&snap.message)
        .to_owned();
    if let Some(ver) = &snap.target_version
        && !title.contains(ver)
    {
        let _ = write!(title, " ({ver})");
    }
    let title_clean = truncate_display_text(&title, max_body);
    let line1 = format!("{glyph} {step_tag} {stage_lbl}  {title_clean}");

    let rate_str = format_transfer_rate(snap.bytes_per_sec);
    let metrics_plain = match snap.total_bytes.filter(|&t| t > 0) {
        Some(total) => {
            let pct = snap.progress_fraction().unwrap_or(0.0) * 100.0;
            let bytes_part = format!(
                "{} / {}",
                format_bytes(snap.downloaded_bytes),
                format_bytes(total)
            );
            let eta_part = snap
                .eta_secs
                .map(|s| format!("  ·  ETA {}", format_eta(s)))
                .unwrap_or_default();
            format!(" {pct:5.1}%  {bytes_part}  ·  {rate_str}{eta_part}")
        }
        None => {
            format!("  {}  ·  {rate_str}", format_bytes(snap.downloaded_bytes))
        }
    };

    let bar_width = cols
        .saturating_sub(metrics_plain.chars().count().saturating_add(6))
        .clamp(12, 34);
    let bar_rendered = render_download_bar(snap.progress_fraction(), tick_idx, bar_width, colored);
    let line2 = match snap.total_bytes.filter(|&t| t > 0) {
        Some(total) => {
            let pct = style_if(
                &format!("{:5.1}%", snap.progress_fraction().unwrap_or(0.0) * 100.0),
                Style::new().bold(),
                colored,
            );
            let bytes_part = format!(
                "{} / {}",
                format_bytes(snap.downloaded_bytes),
                format_bytes(total)
            );
            let sep = style_if("·", Style::new().dimmed(), colored);
            let rate_colored = style_if(&rate_str, Style::new().green().bold(), colored);
            let eta_part = snap
                .eta_secs
                .map(|s| {
                    let eta_txt = style_if(
                        &format!("ETA {}", format_eta(s)),
                        Style::new().dimmed(),
                        colored,
                    );
                    format!("  {sep}  {eta_txt}")
                })
                .unwrap_or_default();
            format!("  {bar_rendered}  {pct}  {bytes_part}  {sep}  {rate_colored}{eta_part}")
        }
        None => {
            let bytes_part = format_bytes(snap.downloaded_bytes);
            let sep = style_if("·", Style::new().dimmed(), colored);
            let rate_colored = style_if(&rate_str, Style::new().green().bold(), colored);
            format!("  {bar_rendered}  {bytes_part}  {sep}  {rate_colored}")
        }
    };

    vec![line1, line2]
}

fn render_download_bar(
    fraction: Option<f64>,
    tick_idx: usize,
    width: usize,
    colored: bool,
) -> String {
    if width == 0 {
        return String::new();
    }
    if let Some(frac) = fraction {
        let clamped = frac.clamp(0.0, 1.0);
        let filled = ((clamped * width as f64).round() as usize).min(width);
        if filled >= width {
            return style_if(&"━".repeat(width), Style::new().green().bold(), colored);
        }
        let mut out = String::new();
        if filled > 0 {
            out.push_str(&style_if(
                &"━".repeat(filled),
                Style::new().cyan().bold(),
                colored,
            ));
        }
        let rem = width.saturating_sub(filled);
        if rem > 0 {
            if filled > 0 {
                out.push_str(&style_if("╸", Style::new().cyan(), colored));
                if rem > 1 {
                    out.push_str(&style_if(
                        &"━".repeat(rem - 1),
                        Style::new().dimmed(),
                        colored,
                    ));
                }
            } else {
                out.push_str(&style_if(&"━".repeat(rem), Style::new().dimmed(), colored));
            }
        }
        out
    } else {
        let pulse_len = (width / 4).clamp(3, 6).min(width);
        let span = width.saturating_sub(pulse_len).max(1);
        let cycle = span * 2;
        let pos_in_cycle = tick_idx % cycle;
        let start = if pos_in_cycle <= span {
            pos_in_cycle
        } else {
            cycle - pos_in_cycle
        };
        let end = (start + pulse_len).min(width);
        let mut out = String::new();
        if start > 0 {
            out.push_str(&style_if(
                &"━".repeat(start),
                Style::new().dimmed(),
                colored,
            ));
        }
        out.push_str(&style_if(
            &"━".repeat(end - start),
            Style::new().cyan().bold(),
            colored,
        ));
        if end < width {
            out.push_str(&style_if(
                &"━".repeat(width - end),
                Style::new().dimmed(),
                colored,
            ));
        }
        out
    }
}

fn truncate_display_text(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    if max_chars <= 1 {
        return "…".to_owned();
    }
    let mut out: String = text.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}
/// Prints a single log line with structured level and timestamp highlighting on TTYs.
pub fn print_log_line(line: &str) {
    if !use_color() {
        println!("{line}");
        return;
    }

    let trimmed = line.trim_end_matches('\r');
    for (tag, style) in [
        (" ERROR ", Style::new().red().bold()),
        (" WARN ", Style::new().yellow().bold()),
        (" INFO ", Style::new().green().bold()),
        (" DEBUG ", Style::new().blue()),
        (" TRACE ", Style::new().dimmed()),
    ] {
        if let Some((prefix, rest)) = trimmed.split_once(tag) {
            let ts = prefix.dimmed();
            let padded_lvl = format!("{:<5}", tag.trim());
            let lvl = padded_lvl.style(style);
            let body = style_log_fields(rest);
            println!("{ts} {lvl} {body}");
            return;
        }
    }
    println!("{trimmed}");
}

fn style_log_fields(rest: &str) -> String {
    let mut out = String::with_capacity(rest.len() + 16);
    for (idx, token) in rest.split_whitespace().enumerate() {
        if idx > 0 {
            out.push(' ');
        }
        if let Some((k, v)) = token.split_once('=')
            && !k.is_empty()
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            let _ = write!(out, "{}={}", k.dimmed(), v.cyan());
        } else {
            out.push_str(token);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations_tokens_and_relative_timestamps_cleanly() {
        assert_eq!(format_duration_secs(9), "9s");
        assert_eq!(format_duration_secs(125), "2m 05s");
        assert_eq!(format_duration_secs(7_380), "2h 03m");

        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(12_500), "12.5k");
        assert_eq!(format_tokens(1_500_000), "1.50M");

        assert_eq!(format_relative_ms(10_000, 12_000), "just now");
        assert_eq!(format_relative_ms(10_000, 40_000), "30s ago");
        assert_eq!(format_relative_ms(10_000, 190_000), "3m ago");
        assert_eq!(format_utc_hms(0), "00:00:00Z");
    }

    #[test]
    fn renders_transfer_progress_lines_with_bar_rate_and_eta() {
        let checking = TransferProgress::checking("Querying GitHub Releases…");
        let lines_chk = format_transfer_lines(&checking, 0, 100, false);
        assert_eq!(lines_chk.len(), 1);
        assert!(lines_chk[0].contains("[1/6]"));
        assert!(!lines_chk[0].contains("\x1b["));

        let downloading = TransferProgress {
            stage: TransferStage::Downloading,
            message: "Downloading cli_windows_x64.exe…".to_owned(),
            asset_name: Some("cli_windows_x64.exe".to_owned()),
            target_version: Some("v1.5.11".to_owned()),
            downloaded_bytes: 120 * 1024 * 1024,
            total_bytes: Some(240 * 1024 * 1024),
            bytes_per_sec: 16.0 * 1024.0 * 1024.0,
            eta_secs: Some(8),
        };
        let lines_dl = format_transfer_lines(&downloading, 2, 100, true);
        assert_eq!(lines_dl.len(), 2);
        assert!(lines_dl[0].contains("cli_windows_x64.exe (v1.5.11)"));
        assert!(lines_dl[1].contains("50.0%"));
        assert!(lines_dl[1].contains("16.00 MiB/s"));
        assert!(lines_dl[1].contains("ETA 8s"));

        let unknown = TransferProgress {
            total_bytes: None,
            eta_secs: None,
            ..downloading
        };
        let lines_narrow = format_transfer_lines(&unknown, 5, 48, false);
        assert_eq!(lines_narrow.len(), 2);
        assert!(lines_narrow[1].contains("16.00 MiB/s"));
    }
}
