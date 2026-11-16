//! Interactive terminal experiences for `liftgate` (`lg`):
//! - **Cockpit** (`lg` with no arguments on a TTY): quick status & single-key launcher.
//! - **Live Dashboard** (`lg top` / `lg tui`): real-time telemetry, turn inspector,
//!   session-pool eviction, live model switching, and log viewer.
//! - **Model Picker** (`lg models --pick` / `lg m -i`): live-filtered model selector.

use std::io::{IsTerminal as _, Stdout, stdin, stdout};
use std::net::SocketAddr;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use liftgate_types::telemetry::{
    GatewayTelemetrySnapshot, SessionSlotState, TaskPhase, TaskProtocol, TaskSnapshot,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, Gauge, List, ListItem, ListState, Paragraph, Row,
        Table, TableState, Tabs, Wrap,
    },
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::ui::{
    format_duration_secs, format_latency_ms, format_relative_ms, format_tokens, format_utc_hms,
};
use liftgate_core::cli::{RestartArgs, ServeArgs, TopArgs};
use liftgate_core::client::{DiscoveredModel, GatewayClient, infer_live_long_context};
use liftgate_core::config_sync::resolve_fast_model;
use liftgate_core::daemon::{
    build_authenticated_dashboard_url, ensure_gateway_ready, is_pid_alive, open_browser,
    read_log_tail, resolve_client_api_key, restart_daemon, start_daemon, stop_daemon,
};
use liftgate_core::error::CliError;
use liftgate_core::state::{StatePaths, format_connect_url, now_unix_ms, write_file_atomic};
use liftgate_server::title_model::title_model;
use liftgate_types::config::ModelId;

static TUI_RAW_ACTIVE: AtomicBool = AtomicBool::new(false);
static TUI_PANIC_HOOK_ONCE: Once = Once::new();

/// Returns `true` while an interactive `ratatui` alternate-screen session owns the terminal.
#[must_use]
pub fn is_tui_raw_active() -> bool {
    TUI_RAW_ACTIVE.load(Ordering::Relaxed)
}

const BRAILLE_SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn install_tui_panic_hook() {
    TUI_PANIC_HOOK_ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if TUI_RAW_ACTIVE.swap(false, Ordering::SeqCst) {
                let _ = disable_raw_mode();
                let mut out = stdout();
                let _ = execute!(out, LeaveAlternateScreen, cursor::Show);
            }
            prev(info);
        }));
    });
}

/// Synchronous `Drop` guard restoring terminal raw mode and alternate screen on exit.
struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter(feature: &'static str) -> Result<Self, CliError> {
        if !stdin().is_terminal() || !stdout().is_terminal() {
            return Err(CliError::NotInteractiveTerminal { feature });
        }
        install_tui_panic_hook();
        enable_raw_mode().map_err(|source| CliError::TuiIo { source })?;
        TUI_RAW_ACTIVE.store(true, Ordering::SeqCst);

        let mut out = stdout();
        if let Err(source) = execute!(out, EnterAlternateScreen, cursor::Hide) {
            TUI_RAW_ACTIVE.store(false, Ordering::SeqCst);
            let _ = disable_raw_mode();
            return Err(CliError::TuiIo { source });
        }

        let backend = CrosstermBackend::new(out);
        match Terminal::new(backend) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(source) => {
                TUI_RAW_ACTIVE.store(false, Ordering::SeqCst);
                let _ = disable_raw_mode();
                let _ = execute!(stdout(), LeaveAlternateScreen, cursor::Show);
                Err(CliError::TuiIo { source })
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if TUI_RAW_ACTIVE.swap(false, Ordering::SeqCst) {
            let _ = disable_raw_mode();
            let _ = execute!(
                self.terminal.backend_mut(),
                LeaveAlternateScreen,
                cursor::Show
            );
        }
    }
}

/// Action chosen from the bare `lg` interactive cockpit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CockpitAction {
    LaunchCodex,
    LaunchClaude,
    LaunchCodexApp,
    LaunchClaudeDesktop,
    OpenTop,
    PickModel,
    OpenWebUi,
    StartOrRestart,
    AgyManage,
    AgyLogin,
    SyncConfig,
    Doctor,
    Upgrade,
    StopGateway,
}

struct CockpitItem {
    hotkey: char,
    label: &'static str,
    desc: &'static str,
    action: CockpitAction,
}

const COCKPIT_ITEMS: &[CockpitItem] = &[
    CockpitItem {
        hotkey: 'c',
        label: "Launch Codex CLI",
        desc: "Start Codex in current dir (auto-starts gateway, 0 file side effects)",
        action: CockpitAction::LaunchCodex,
    },
    CockpitItem {
        hotkey: 'a',
        label: "Launch Claude Code",
        desc: "Start Claude Code in current dir (auto-starts gateway, 0 file side effects)",
        action: CockpitAction::LaunchClaude,
    },
    CockpitItem {
        hotkey: 'w',
        label: "Launch Codex App",
        desc: "Open Codex desktop GUI with isolated ephemeral CODEX_HOME (0 pollution)",
        action: CockpitAction::LaunchCodexApp,
    },
    CockpitItem {
        hotkey: 'e',
        label: "Launch Claude Desktop",
        desc: "Open Claude Desktop GUI in auto-restoring 3P gateway mode (0 pollution)",
        action: CockpitAction::LaunchClaudeDesktop,
    },
    CockpitItem {
        hotkey: 't',
        label: "Live Terminal Dashboard",
        desc: "Real-time turns, inspector, session eviction, models & logs (TUI)",
        action: CockpitAction::OpenTop,
    },
    CockpitItem {
        hotkey: 'm',
        label: "Switch Default Model",
        desc: "Search and switch active Antigravity model interactively",
        action: CockpitAction::PickModel,
    },
    CockpitItem {
        hotkey: 'u',
        label: "Open Web Dashboard",
        desc: "Open embedded React observability dashboard in default browser",
        action: CockpitAction::OpenWebUi,
    },
    CockpitItem {
        hotkey: 's',
        label: "Start / Restart Gateway",
        desc: "Start background gateway (if offline) or restart active daemon",
        action: CockpitAction::StartOrRestart,
    },
    CockpitItem {
        hotkey: 'i',
        label: "Antigravity CLI (agy)",
        desc: "Inspect agy status, proxy & auto-update, or install agy if missing",
        action: CockpitAction::AgyManage,
    },
    CockpitItem {
        hotkey: 'l',
        label: "Antigravity Sign-In",
        desc: "Interactive Google sign-in via official `agy acp --login`",
        action: CockpitAction::AgyLogin,
    },
    CockpitItem {
        hotkey: 'g',
        label: "Configure Client Files",
        desc: "Non-destructively sync .claude & .codex project configuration",
        action: CockpitAction::SyncConfig,
    },
    CockpitItem {
        hotkey: 'd',
        label: "Environment Doctor",
        desc: "Check agy, claude, codex, network proxy & login readiness",
        action: CockpitAction::Doctor,
    },
    CockpitItem {
        hotkey: 'p',
        label: "Check & Upgrade CLI",
        desc: "Verify SHA256SUMS and self-upgrade lg & liftgate from GitHub Releases",
        action: CockpitAction::Upgrade,
    },
    CockpitItem {
        hotkey: 'x',
        label: "Stop Gateway Service",
        desc: "Gracefully drain & stop managed background gateway",
        action: CockpitAction::StopGateway,
    },
];

struct CockpitSummary {
    healthy: bool,
    managed: bool,
    pid: Option<u32>,
    endpoint: String,
    default_model: String,
    uptime_secs: Option<u64>,
    active_sessions: usize,
    idle_sessions: usize,
    total_requests: u64,
    total_tokens: u64,
    agy_installed: bool,
    agy_version: Option<String>,
    agy_proxy: Option<String>,
    agy_auto_update: bool,
    cwd_display: String,
}

async fn probe_cockpit_summary(state_paths: &StatePaths) -> CockpitSummary {
    let local_agy = liftgate_core::agy_manager::quick_local_agy_summary(state_paths, None).await;
    let daemon = state_paths.read_daemon().ok().flatten();
    let default_serve = ServeArgs::default();
    let listen = daemon.as_ref().map_or(default_serve.listen, |d| d.listen);
    let api_key = resolve_client_api_key(None, daemon.as_ref(), listen);
    let client = GatewayClient::new(listen, api_key);
    let managed_alive = daemon.as_ref().is_some_and(|d| is_pid_alive(d.pid));
    let healthy = client.check_health(Duration::from_millis(350)).await;
    let snap = if healthy {
        client.fetch_telemetry().await.ok()
    } else {
        None
    };

    let default_model = snap
        .as_ref()
        .map(|s| s.acp_process.default_model.clone())
        .or_else(|| {
            daemon
                .as_ref()
                .map(|d| d.default_model.clone())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or(default_serve.default_model);

    let cwd_display = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_owned());

    let agy_version = snap
        .as_ref()
        .and_then(|s| s.acp_process.agent_version.clone())
        .or(local_agy.version);
    let agy_proxy = snap
        .as_ref()
        .and_then(|s| s.acp_process.agy_proxy.clone())
        .or_else(|| {
            local_agy
                .proxy
                .as_ref()
                .map(|p| p.redacted_display().into_owned())
        });
    let agy_auto_update = snap.as_ref().map_or(local_agy.auto_update_enabled, |s| {
        s.acp_process.auto_update_enabled
    });

    CockpitSummary {
        healthy,
        managed: healthy && managed_alive,
        pid: daemon
            .as_ref()
            .filter(|_| managed_alive)
            .map(|d| d.pid)
            .or_else(|| snap.as_ref().and_then(|s| s.acp_process.pid)),
        endpoint: format!("{}/", client.base_url()),
        default_model,
        uptime_secs: snap.as_ref().map(|s| s.acp_process.uptime_secs),
        active_sessions: snap
            .as_ref()
            .map_or(0, |s| s.session_pool.active_leased_sessions),
        idle_sessions: snap
            .as_ref()
            .map_or(0, |s| s.session_pool.idle_cached_sessions),
        total_requests: snap.as_ref().map_or(0, |s| s.metrics.total_requests),
        total_tokens: snap.as_ref().map_or(0, |s| {
            s.metrics
                .total_input_tokens
                .saturating_add(s.metrics.total_output_tokens)
        }),
        agy_installed: local_agy.binary_path.is_some(),
        agy_version,
        agy_proxy,
        agy_auto_update,
        cwd_display,
    }
}

/// Opens the bare `lg` interactive command center and returns the chosen action.
pub async fn run_cockpit(state_paths: &StatePaths) -> Result<Option<CockpitAction>, CliError> {
    let summary = probe_cockpit_summary(state_paths).await;
    let mut session = TerminalSession::enter("interactive cockpit")?;
    let mut selected = 0usize;
    let mut list_state = ListState::default();
    list_state.select(Some(0));

    let chosen = loop {
        session
            .terminal
            .draw(|frame| draw_cockpit(frame, &summary, &mut list_state))
            .map_err(|source| CliError::TuiIo { source })?;

        if let Some(decision) = poll_cockpit_input(&mut selected)? {
            break decision;
        }
        list_state.select(Some(selected));
        tokio::time::sleep(Duration::from_millis(35)).await;
    };

    drop(session);
    Ok(chosen)
}

fn poll_cockpit_input(selected: &mut usize) -> Result<Option<Option<CockpitAction>>, CliError> {
    while event::poll(Duration::ZERO).map_err(|source| CliError::TuiIo { source })? {
        let Event::Key(key) = event::read().map_err(|source| CliError::TuiIo { source })? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(Some(None));
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(Some(None)),
            KeyCode::Up | KeyCode::Char('k') => {
                *selected = if *selected == 0 {
                    COCKPIT_ITEMS.len().saturating_sub(1)
                } else {
                    selected.saturating_sub(1)
                };
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *selected = (*selected + 1) % COCKPIT_ITEMS.len();
            }
            KeyCode::Enter => {
                if let Some(item) = COCKPIT_ITEMS.get(*selected) {
                    return Ok(Some(Some(item.action)));
                }
            }
            KeyCode::Char(ch) => {
                let lower = ch.to_ascii_lowercase();
                if let Some(digit) = lower.to_digit(10)
                    && digit >= 1
                    && let Some(item) = COCKPIT_ITEMS.get((digit - 1) as usize)
                {
                    return Ok(Some(Some(item.action)));
                }
                if let Some(item) = COCKPIT_ITEMS.iter().find(|i| i.hotkey == lower) {
                    return Ok(Some(Some(item.action)));
                }
            }
            _ => {}
        }
    }
    Ok(None)
}

fn draw_cockpit(frame: &mut Frame<'_>, summary: &CockpitSummary, list_state: &mut ListState) {
    let area = centered_rect(88, 28, frame.area());
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Min(15),
            Constraint::Length(2),
        ])
        .split(area);

    let status_spans = if summary.healthy {
        let mode_tag = if summary.managed {
            "managed"
        } else {
            "external"
        };
        let pid_str = summary
            .pid
            .map_or_else(|| "—".to_owned(), |p| p.to_string());
        let up_str = summary
            .uptime_secs
            .map_or_else(|| "—".to_owned(), format_duration_secs);
        vec![
            Span::styled(
                " ● ONLINE ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                &summary.endpoint,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ({mode_tag} · pid {pid_str} · up {up_str})"),
                Style::default().fg(Color::DarkGray),
            ),
        ]
    } else {
        vec![
            Span::styled(
                " ○ OFFLINE ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(&summary.endpoint, Style::default().fg(Color::DarkGray)),
            Span::styled(
                "  (auto-starts when launching Codex, Claude, or Web UI)",
                Style::default().fg(Color::DarkGray),
            ),
        ]
    };

    let meta_line = Line::from(vec![
        Span::styled("Model: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            &summary.default_model,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("   Sessions: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(
                "{} active / {} warm",
                summary.active_sessions, summary.idle_sessions
            ),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled("   Traffic: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(
                "{} reqs · {} tok",
                summary.total_requests,
                format_tokens(summary.total_tokens)
            ),
            Style::default().fg(Color::Magenta),
        ),
    ]);

    let cwd_line = Line::from(vec![
        Span::styled("Workspace: ", Style::default().fg(Color::DarkGray)),
        Span::styled(&summary.cwd_display, Style::default().fg(Color::Gray)),
    ]);

    let agy_badge = if summary.agy_installed {
        let v = summary.agy_version.as_deref().unwrap_or("installed");
        Span::styled(
            format!("v{v}"),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "MISSING (press [i] to install)",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    };
    let agy_line = Line::from(vec![
        Span::styled("AGY CLI: ", Style::default().fg(Color::DarkGray)),
        agy_badge,
        Span::styled("   Proxy: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            summary.agy_proxy.as_deref().unwrap_or("none"),
            Style::default().fg(if summary.agy_proxy.is_some() {
                Color::Cyan
            } else {
                Color::Gray
            }),
        ),
        Span::styled(
            "   Idle Auto-Update: ",
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            if summary.agy_auto_update { "on" } else { "off" },
            Style::default().fg(if summary.agy_auto_update {
                Color::Green
            } else {
                Color::DarkGray
            }),
        ),
    ]);

    let header_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Line::from(vec![
            Span::styled(
                " LIFTGATE ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " v{} · Antigravity Agent Hub (`lg`) ",
                    env!("CARGO_PKG_VERSION")
                ),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

    if let Some(&header_rect) = chunks.first() {
        let header_para = Paragraph::new(vec![
            Line::from(status_spans),
            meta_line,
            agy_line,
            cwd_line,
        ])
        .block(header_block);
        frame.render_widget(header_para, header_rect);
    }

    let items: Vec<ListItem<'_>> = COCKPIT_ITEMS
        .iter()
        .map(|item| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" [{}] ", item.hotkey),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<25}", item.label),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(item.desc, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let menu = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    " Quick Actions ",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(24, 52, 74))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" ❯ ");

    if let Some(&menu_rect) = chunks.get(1) {
        frame.render_stateful_widget(menu, menu_rect, list_state);
    }

    if let Some(&footer_rect) = chunks.get(2) {
        let footer = Paragraph::new(Line::from(vec![
            Span::styled(
                " ↑/↓ ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("Move   ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "Enter ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("Run   ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "c/a/t/m/u/s/g/d/x ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("Instant Hotkey   ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "q / Esc ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("Exit", Style::default().fg(Color::DarkGray)),
        ]))
        .alignment(Alignment::Center);
        frame.render_widget(footer, footer_rect);
    }
}

/// Interactive live-filtered model picker (`lg models --pick` / `lg m -i`).
pub async fn run_model_picker(
    models: &[DiscoveredModel],
    current_default: &str,
    fast_model: &str,
) -> Result<Option<String>, CliError> {
    if models.is_empty() {
        return Ok(None);
    }
    let mut session = TerminalSession::enter("interactive model picker")?;
    let mut query = String::new();
    let mut selected = models
        .iter()
        .position(|m| m.id == current_default)
        .unwrap_or(0);
    let mut table_state = TableState::default();

    let long_context = infer_live_long_context(models, None);
    let chosen = loop {
        let filtered: Vec<&DiscoveredModel> = models
            .iter()
            .filter(|m| {
                if query.trim().is_empty() {
                    return true;
                }
                let q = query.trim().to_ascii_lowercase();
                m.id.to_ascii_lowercase().contains(&q)
                    || m.display_name.to_ascii_lowercase().contains(&q)
            })
            .collect();

        if filtered.is_empty() {
            selected = 0;
            table_state.select(None);
        } else {
            selected = selected.min(filtered.len().saturating_sub(1));
            table_state.select(Some(selected));
        }

        session
            .terminal
            .draw(|frame| {
                draw_model_picker(
                    frame,
                    &query,
                    &filtered,
                    current_default,
                    fast_model,
                    long_context,
                    &mut table_state,
                );
            })
            .map_err(|source| CliError::TuiIo { source })?;

        let mut stop_loop: Option<Option<String>> = None;
        while event::poll(Duration::ZERO).map_err(|source| CliError::TuiIo { source })? {
            let Event::Key(key) = event::read().map_err(|source| CliError::TuiIo { source })?
            else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('c') => {
                        stop_loop = Some(None);
                        break;
                    }
                    KeyCode::Char('u') => {
                        query.clear();
                        selected = 0;
                    }
                    KeyCode::Char('n') if !filtered.is_empty() => {
                        selected = (selected + 1) % filtered.len();
                    }
                    KeyCode::Char('p') if !filtered.is_empty() => {
                        selected = if selected == 0 {
                            filtered.len().saturating_sub(1)
                        } else {
                            selected.saturating_sub(1)
                        };
                    }
                    _ => {}
                }
                continue;
            }
            match key.code {
                KeyCode::Esc => {
                    stop_loop = Some(None);
                    break;
                }
                KeyCode::Up if !filtered.is_empty() => {
                    selected = if selected == 0 {
                        filtered.len().saturating_sub(1)
                    } else {
                        selected.saturating_sub(1)
                    };
                }
                KeyCode::Down if !filtered.is_empty() => {
                    selected = (selected + 1) % filtered.len();
                }
                KeyCode::Backspace => {
                    query.pop();
                    selected = 0;
                }
                KeyCode::Enter => {
                    if let Some(m) = filtered.get(selected) {
                        stop_loop = Some(Some(m.id.clone()));
                        break;
                    }
                }
                KeyCode::Char(c) => {
                    query.push(c);
                    selected = 0;
                }
                _ => {}
            }
        }

        if let Some(res) = stop_loop {
            break res;
        }
        tokio::time::sleep(Duration::from_millis(35)).await;
    };

    drop(session);
    Ok(chosen)
}

fn draw_model_picker(
    frame: &mut Frame<'_>,
    query: &str,
    filtered: &[&DiscoveredModel],
    current_default: &str,
    fast_model: &str,
    long_context: bool,
    table_state: &mut TableState,
) {
    let area = centered_rect(92, 24, frame.area());
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(2),
        ])
        .split(area);

    if let Some(&search_rect) = chunks.first() {
        let search = Paragraph::new(Line::from(vec![
            Span::styled(
                " 🔍 Filter: ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                query,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("█", Style::default().fg(Color::Cyan)),
            Span::styled(
                format!("   ({} matched)", filtered.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Cyan))
                .title(Span::styled(
                    " Select Default Antigravity Model ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
        );
        frame.render_widget(search, search_rect);
    }

    let rows: Vec<Row<'_>> = filtered
        .iter()
        .map(|m| {
            let ctx_k = format!("{}k", m.context_limits(long_context).0 / 1_000);
            let role = match (m.id == current_default, m.id == fast_model) {
                (true, true) => "★ default · ⚡ fast",
                (true, false) => "★ default",
                (false, true) => "⚡ fast",
                (false, false) => "",
            };
            let role_color = if m.id == current_default {
                Color::Green
            } else {
                Color::Yellow
            };
            Row::new(vec![
                Cell::from(m.id.clone()).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Cell::from(if m.display_name.is_empty() {
                    m.id.clone()
                } else {
                    m.display_name.clone()
                }),
                Cell::from(ctx_k).style(Style::default().fg(Color::Gray)),
                Cell::from(role)
                    .style(Style::default().fg(role_color).add_modifier(Modifier::BOLD)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(36),
            Constraint::Percentage(34),
            Constraint::Length(9),
            Constraint::Percentage(22),
        ],
    )
    .header(
        Row::new(vec!["MODEL ID", "DISPLAY NAME", "CONTEXT", "ROLE"]).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray)),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::Rgb(24, 52, 74))
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol(" ❯ ");

    if let Some(&table_rect) = chunks.get(1) {
        frame.render_stateful_widget(table, table_rect, table_state);
    }

    if let Some(&footer_rect) = chunks.get(2) {
        let footer = Paragraph::new(Line::from(vec![
            Span::styled(
                " Type ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("Filter   ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "↑/↓ ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("Select   ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "Enter ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Apply Default Model   ",
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                "Esc ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("Cancel", Style::default().fg(Color::DarkGray)),
        ]))
        .alignment(Alignment::Center);
        frame.render_widget(footer, footer_rect);
    }
}

// ============================================================================
// Full-screen Real-Time Terminal Observability Dashboard (`lg top` / `lg tui`)
// ============================================================================

#[derive(Clone, Debug)]
struct PolledData {
    healthy: bool,
    managed: bool,
    daemon_pid: Option<u32>,
    gateway_url: String,
    dashboard_url: String,
    default_model: String,
    fast_model: String,
    title_model: Option<ModelId>,
    telemetry: Option<GatewayTelemetrySnapshot>,
    models: Vec<DiscoveredModel>,
    service_log_lines: Vec<String>,
    polled_at_ms: u64,
}

enum TuiCommand {
    CloseSession(String),
    SwitchDefaultModel(String),
    SetTitleModel(String),
    StartOrRestartDaemon,
    StopDaemon,
    OpenBrowser,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LogSourceTab {
    AcpStderr,
    ServiceLog,
}

struct TopApp {
    tab_idx: usize,
    turn_idx: usize,
    session_idx: usize,
    model_idx: usize,
    log_scroll: u16,
    log_source: LogSourceTab,
    show_help: bool,
    anim_tick: usize,
    toast: Option<(String, bool, u64)>,
    data: PolledData,
}

impl TopApp {
    fn new(initial: PolledData) -> Self {
        Self {
            tab_idx: 0,
            turn_idx: 0,
            session_idx: 0,
            model_idx: 0,
            log_scroll: 0,
            log_source: LogSourceTab::AcpStderr,
            show_help: false,
            anim_tick: 0,
            toast: None,
            data: initial,
        }
    }

    fn set_toast(&mut self, msg: impl Into<String>, ok: bool) {
        self.toast = Some((msg.into(), ok, now_unix_ms()));
    }

    fn combined_turns(&self) -> Vec<&TaskSnapshot> {
        let Some(snap) = &self.data.telemetry else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(snap.active_tasks.len() + snap.recent_tasks.len());
        for t in &snap.active_tasks {
            out.push(t);
        }
        for t in &snap.recent_tasks {
            out.push(t);
        }
        out
    }

    fn clamp_selections(&mut self) {
        let turn_len = self.combined_turns().len();
        if turn_len == 0 {
            self.turn_idx = 0;
        } else {
            self.turn_idx = self.turn_idx.min(turn_len.saturating_sub(1));
        }

        let sess_len = self
            .data
            .telemetry
            .as_ref()
            .map_or(0, |s| s.session_pool.sessions.len());
        if sess_len == 0 {
            self.session_idx = 0;
        } else {
            self.session_idx = self.session_idx.min(sess_len.saturating_sub(1));
        }

        let model_len = self.data.models.len();
        if model_len == 0 {
            self.model_idx = 0;
        } else {
            self.model_idx = self.model_idx.min(model_len.saturating_sub(1));
        }
    }
}

async fn poll_gateway_snapshot(
    state_paths: &StatePaths,
    explicit_listen: Option<SocketAddr>,
    explicit_key: Option<&str>,
) -> PolledData {
    let daemon = state_paths.read_daemon().ok().flatten();
    let default_serve = ServeArgs::default();
    let listen = explicit_listen
        .or_else(|| daemon.as_ref().map(|d| d.listen))
        .unwrap_or(default_serve.listen);
    let api_key = resolve_client_api_key(explicit_key, daemon.as_ref(), listen);
    let client = GatewayClient::new(listen, api_key.clone());
    let managed_alive = daemon
        .as_ref()
        .is_some_and(|d| d.listen == listen && is_pid_alive(d.pid));
    let healthy = client.check_health(Duration::from_millis(600)).await;

    let telemetry = if healthy {
        client.fetch_telemetry().await.ok()
    } else {
        None
    };
    let models = if healthy {
        client.fetch_models().await.unwrap_or_default()
    } else {
        Vec::new()
    };

    let default_model = telemetry
        .as_ref()
        .map(|s| s.acp_process.default_model.clone())
        .or_else(|| {
            daemon
                .as_ref()
                .map(|d| d.default_model.clone())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or(default_serve.default_model);
    let fast_model = resolve_fast_model(None, &default_model, &models);
    let title_model = if healthy && managed_alive {
        Some(title_model(Some(&state_paths.title_model_file())).await)
    } else {
        None
    };

    let dashboard_base = daemon
        .as_ref()
        .map_or_else(|| client.base_url().to_owned(), |d| d.dashboard_base_url());
    let dashboard_url =
        build_authenticated_dashboard_url(&dashboard_base, client.base_url(), api_key.as_deref());

    let log_file = state_paths.log_file();
    let service_log_lines = if log_file.is_file() {
        read_log_tail(&log_file, 120)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    } else {
        Vec::new()
    };

    PolledData {
        healthy,
        managed: healthy && managed_alive,
        daemon_pid: daemon.as_ref().filter(|_| managed_alive).map(|d| d.pid),
        gateway_url: format!("{}/", client.base_url()),
        dashboard_url,
        default_model,
        fast_model,
        title_model,
        telemetry,
        models,
        service_log_lines,
        polled_at_ms: now_unix_ms(),
    }
}

/// Runs the full-screen real-time observability dashboard (`lg top` / `lg tui`).
pub async fn run_top(state_paths: &StatePaths, args: TopArgs) -> Result<(), CliError> {
    if args.auto_start {
        let _ = ensure_gateway_ready(state_paths, args.listen, args.api_key.clone(), None, true)
            .await?;
    }

    let initial = poll_gateway_snapshot(state_paths, args.listen, args.api_key.as_deref()).await;
    let mut session = TerminalSession::enter("live terminal dashboard (`lg top`)")?;
    let mut app = TopApp::new(initial);

    let (refresh_tx, mut refresh_rx) = watch::channel(0u64);
    let (data_tx, mut data_rx) = mpsc::channel::<PolledData>(4);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<TuiCommand>(8);
    let (toast_tx, mut toast_rx) = mpsc::channel::<(String, bool)>(8);

    let mut tasks = JoinSet::new();
    let poll_paths = state_paths.clone();
    let poll_listen = args.listen;
    let poll_key = args.api_key.clone();
    let poll_interval = Duration::from_millis(args.interval_ms.max(250));
    tasks.spawn(async move {
        loop {
            tokio::select! {
                biased;
                res = refresh_rx.changed() => {
                    if res.is_err() {
                        break;
                    }
                }
                () = tokio::time::sleep(poll_interval) => {}
            }
            let snap = poll_gateway_snapshot(&poll_paths, poll_listen, poll_key.as_deref()).await;
            if data_tx.send(snap).await.is_err() {
                break;
            }
        }
    });

    let worker_paths = state_paths.clone();
    let worker_listen = args.listen;
    let worker_key = args.api_key.clone();
    let worker_refresh = refresh_tx.clone();
    tasks.spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            let outcome: Result<String, String> = match cmd {
                TuiCommand::CloseSession(sess_id) => {
                    let daemon = worker_paths.read_daemon().ok().flatten();
                    let addr = worker_listen
                        .or_else(|| daemon.as_ref().map(|d| d.listen))
                        .unwrap_or_else(|| ServeArgs::default().listen);
                    let key = resolve_client_api_key(worker_key.as_deref(), daemon.as_ref(), addr);
                    let client = GatewayClient::new(addr, key);
                    match client.close_idle_session(&sess_id).await {
                        Ok(true) => Ok(format!("Closed idle session {sess_id}")),
                        Ok(false) => Err(format!("Session {sess_id} is active or already closed")),
                        Err(e) => Err(e.to_string()),
                    }
                }
                TuiCommand::SwitchDefaultModel(model_id) => {
                    let restart_args = RestartArgs {
                        listen: worker_listen,
                        default_model: Some(model_id.clone()),
                        api_key: worker_key.clone(),
                        ..RestartArgs::default()
                    };
                    match restart_daemon(&worker_paths, restart_args).await {
                        Ok(_) => Ok(format!("Switched default model to {model_id}")),
                        Err(e) => Err(e.to_string()),
                    }
                }
                TuiCommand::SetTitleModel(model_id) => {
                    write_file_atomic(&worker_paths.title_model_file(), model_id.as_bytes(), false)
                        .map(|()| format!("Title model set to {model_id}"))
                        .map_err(|e| e.to_string())
                }
                TuiCommand::StartOrRestartDaemon => {
                    let daemon = worker_paths.read_daemon().ok().flatten();
                    let running = daemon.as_ref().is_some_and(|d| is_pid_alive(d.pid));
                    let res = if running {
                        restart_daemon(
                            &worker_paths,
                            RestartArgs {
                                listen: worker_listen,
                                api_key: worker_key.clone(),
                                ..RestartArgs::default()
                            },
                        )
                        .await
                        .map(|_| "Restarted background gateway".to_owned())
                    } else {
                        let mut serve = ServeArgs::default();
                        if let Some(addr) = worker_listen {
                            serve.listen = addr;
                        }
                        if let Some(k) = worker_key.clone() {
                            serve.api_key = Some(k);
                        }
                        start_daemon(&worker_paths, serve, false).await.map(|out| {
                            format!(
                                "Started gateway (PID {}) on {}",
                                out.daemon.pid,
                                format_connect_url(out.daemon.listen)
                            )
                        })
                    };
                    res.map_err(|e| e.to_string())
                }
                TuiCommand::StopDaemon => match stop_daemon(&worker_paths).await {
                    Ok(out) if out.stopped_pid.is_some() => {
                        Ok("Stopped background gateway".to_owned())
                    }
                    Ok(_) => Err("No managed background gateway is running".to_owned()),
                    Err(e) => Err(e.to_string()),
                },
                TuiCommand::OpenBrowser => {
                    let snap =
                        poll_gateway_snapshot(&worker_paths, worker_listen, worker_key.as_deref())
                            .await;
                    open_browser(&snap.dashboard_url)
                        .map(|()| format!("Opened Web UI ({})", snap.gateway_url))
                        .map_err(|e| e.to_string())
                }
            };

            match outcome {
                Ok(msg) => {
                    let _ = toast_tx.send((msg, true)).await;
                }
                Err(err) => {
                    let _ = toast_tx.send((err, false)).await;
                }
            }
            let next = worker_refresh.borrow().wrapping_add(1);
            let _ = worker_refresh.send_replace(next);
        }
    });

    loop {
        while let Ok(fresh) = data_rx.try_recv() {
            app.data = fresh;
            app.clamp_selections();
        }
        while let Ok((msg, ok)) = toast_rx.try_recv() {
            app.set_toast(msg, ok);
        }
        if let Some((_, _, ts)) = &app.toast
            && now_unix_ms().saturating_sub(*ts) > 5_000
        {
            app.toast = None;
        }

        app.anim_tick = app.anim_tick.wrapping_add(1);
        session
            .terminal
            .draw(|frame| draw_top_dashboard(frame, &app))
            .map_err(|source| CliError::TuiIo { source })?;

        if handle_top_events(&mut app, &refresh_tx, &cmd_tx)? {
            break;
        }

        tokio::time::sleep(Duration::from_millis(45)).await;
    }

    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    drop(session);
    Ok(())
}

fn handle_top_events(
    app: &mut TopApp,
    refresh_tx: &watch::Sender<u64>,
    cmd_tx: &mpsc::Sender<TuiCommand>,
) -> Result<bool, CliError> {
    while event::poll(Duration::ZERO).map_err(|source| CliError::TuiIo { source })? {
        let Event::Key(key) = event::read().map_err(|source| CliError::TuiIo { source })? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(true);
        }
        if app.show_help {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter
            ) {
                app.show_help = false;
            }
            continue;
        }

        if handle_top_key(app, key, refresh_tx, cmd_tx) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn handle_top_key(
    app: &mut TopApp,
    key: KeyEvent,
    refresh_tx: &watch::Sender<u64>,
    cmd_tx: &mpsc::Sender<TuiCommand>,
) -> bool {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Char('?') => app.show_help = true,
        KeyCode::Tab | KeyCode::Right | KeyCode::Char(']') => {
            app.tab_idx = (app.tab_idx + 1) % 4;
        }
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('[') => {
            app.tab_idx = if app.tab_idx == 0 { 3 } else { app.tab_idx - 1 };
        }
        KeyCode::Char('1') => app.tab_idx = 0,
        KeyCode::Char('2') => app.tab_idx = 1,
        KeyCode::Char('3') => app.tab_idx = 2,
        KeyCode::Char('4') => app.tab_idx = 3,
        KeyCode::Char('r') => {
            let next = refresh_tx.borrow().wrapping_add(1);
            let _ = refresh_tx.send_replace(next);
            app.set_toast("Refreshing telemetry...", true);
        }
        KeyCode::Char('s') => {
            app.set_toast("Starting / restarting background gateway...", true);
            let _ = cmd_tx.try_send(TuiCommand::StartOrRestartDaemon);
        }
        KeyCode::Char('S') => {
            app.set_toast("Stopping background gateway...", true);
            let _ = cmd_tx.try_send(TuiCommand::StopDaemon);
        }
        KeyCode::Char('o') => {
            let _ = cmd_tx.try_send(TuiCommand::OpenBrowser);
        }
        KeyCode::Char('l') if app.tab_idx == 3 => {
            app.log_source = match app.log_source {
                LogSourceTab::AcpStderr => LogSourceTab::ServiceLog,
                LogSourceTab::ServiceLog => LogSourceTab::AcpStderr,
            };
            app.log_scroll = 0;
        }
        KeyCode::Up | KeyCode::Char('k') => match app.tab_idx {
            0 => app.turn_idx = app.turn_idx.saturating_sub(1),
            1 => app.session_idx = app.session_idx.saturating_sub(1),
            2 => app.model_idx = app.model_idx.saturating_sub(1),
            _ => app.log_scroll = app.log_scroll.saturating_sub(1),
        },
        KeyCode::Down | KeyCode::Char('j') => match app.tab_idx {
            0 => {
                let max = app.combined_turns().len().saturating_sub(1);
                app.turn_idx = (app.turn_idx + 1).min(max);
            }
            1 => {
                let max = app
                    .data
                    .telemetry
                    .as_ref()
                    .map_or(0, |s| s.session_pool.sessions.len().saturating_sub(1));
                app.session_idx = (app.session_idx + 1).min(max);
            }
            2 => {
                let max = app.data.models.len().saturating_sub(1);
                app.model_idx = (app.model_idx + 1).min(max);
            }
            _ => app.log_scroll = app.log_scroll.saturating_add(1),
        },
        KeyCode::PageUp => {
            if app.tab_idx == 3 {
                app.log_scroll = app.log_scroll.saturating_sub(12);
            }
        }
        KeyCode::PageDown => {
            if app.tab_idx == 3 {
                app.log_scroll = app.log_scroll.saturating_add(12);
            }
        }
        KeyCode::Char('x') | KeyCode::Delete if app.tab_idx == 1 => {
            if let Some(snap) = &app.data.telemetry
                && let Some(sess) = snap.session_pool.sessions.get(app.session_idx)
            {
                if sess.state == SessionSlotState::Idle {
                    let _ = cmd_tx.try_send(TuiCommand::CloseSession(sess.session_id.clone()));
                } else {
                    app.set_toast("Cannot evict an actively leased session", false);
                }
            }
        }
        KeyCode::Char('t') if app.tab_idx == 2 => {
            if !app.data.managed {
                app.set_toast(
                    "Select the local managed gateway to change its title model",
                    false,
                );
            } else if let Some(model) = app.data.models.get(app.model_idx)
                && cmd_tx
                    .try_send(TuiCommand::SetTitleModel(model.id.clone()))
                    .is_err()
            {
                app.set_toast("Gateway controls are busy; try again", false);
            }
        }
        KeyCode::Enter if app.tab_idx == 2 => {
            if let Some(model_id) = app.data.models.get(app.model_idx).map(|m| m.id.clone()) {
                app.set_toast(format!("Switching default model to {model_id} ..."), true);
                let _ = cmd_tx.try_send(TuiCommand::SwitchDefaultModel(model_id));
            }
        }
        _ => {}
    }
    false
}

fn draw_top_dashboard(frame: &mut Frame<'_>, app: &TopApp) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(frame.area());

    if let Some(&r0) = root.first() {
        draw_top_header(frame, r0, app);
    }
    if let Some(&r1) = root.get(1) {
        draw_kpi_row(frame, r1, app);
    }
    if let Some(&r2) = root.get(2) {
        draw_tabs_bar(frame, r2, app);
    }
    if let Some(&r3) = root.get(3) {
        match app.tab_idx {
            0 => draw_turns_tab(frame, r3, app),
            1 => draw_sessions_tab(frame, r3, app),
            2 => draw_models_tab(frame, r3, app),
            _ => draw_logs_tab(frame, r3, app),
        }
    }
    if let Some(&r4) = root.get(4) {
        draw_footer_bar(frame, r4, app);
    }

    if app.show_help {
        draw_help_modal(frame);
    }
}

fn draw_top_header(frame: &mut Frame<'_>, area: Rect, app: &TopApp) {
    let pulse = BRAILLE_SPINNER
        .get((app.anim_tick / 2) % BRAILLE_SPINNER.len())
        .copied()
        .unwrap_or("⠋");

    let mut spans = vec![
        Span::styled(
            " LIFTGATE ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];

    if app.data.healthy {
        let mode = match (app.data.managed, app.data.daemon_pid) {
            (true, Some(pid)) => format!("managed pid {pid}"),
            (true, None) => "managed".to_owned(),
            (false, _) => "external".to_owned(),
        };
        let up = app.data.telemetry.as_ref().map_or_else(
            || "—".to_owned(),
            |t| format_duration_secs(t.acp_process.uptime_secs),
        );
        let agy_pid = app
            .data
            .telemetry
            .as_ref()
            .and_then(|t| t.acp_process.pid)
            .map_or_else(|| "—".to_owned(), |p| p.to_string());
        spans.extend([
            Span::styled(
                " ● ONLINE ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                &app.data.gateway_url,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {mode} · up {up} · agy_pid {agy_pid}"),
                Style::default().fg(Color::Gray),
            ),
            Span::styled("   model ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                &app.data.default_model,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("   {pulse}"), Style::default().fg(Color::Cyan)),
        ]);
    } else {
        spans.extend([
            Span::styled(
                " ○ OFFLINE ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(&app.data.gateway_url, Style::default().fg(Color::DarkGray)),
            Span::styled(
                "  Press [s] to start the background gateway",
                Style::default().fg(Color::Yellow),
            ),
        ]);
    }

    let bar = Paragraph::new(Line::from(spans)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(bar, area);
}

fn draw_kpi_row(frame: &mut Frame<'_>, area: Rect, app: &TopApp) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);

    let snap = app.data.telemetry.as_ref();

    // 1. Traffic KPI
    if let Some(&c0) = cols.first() {
        let (tot, act, ok, err, oai, ant) = snap.map_or((0, 0, 0, 0, 0, 0), |s| {
            (
                s.metrics.total_requests,
                s.metrics.active_requests,
                s.metrics.completed_requests,
                s.metrics.failed_requests,
                s.metrics.openai_requests,
                s.metrics.anthropic_requests,
            )
        });
        let card = Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    format!("{tot}"),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" total · ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{act} active"),
                    Style::default()
                        .fg(if act > 0 { Color::Cyan } else { Color::Gray })
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled(format!("✓ {ok}"), Style::default().fg(Color::Green)),
                Span::styled("  ", Style::default()),
                Span::styled(
                    format!("✗ {err}"),
                    Style::default().fg(if err > 0 { Color::Red } else { Color::DarkGray }),
                ),
            ]),
            Line::from(vec![
                Span::styled(format!("OAI {oai}"), Style::default().fg(Color::Cyan)),
                Span::styled(" · ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("ANT {ant}"), Style::default().fg(Color::Magenta)),
            ]),
        ])
        .block(kpi_block(" Requests "));
        frame.render_widget(card, c0);
    }

    // 2. Token Accounting KPI
    if let Some(&c1) = cols.get(1) {
        let (inp, outp, thk, cached) = snap.map_or((0, 0, 0, 0), |s| {
            (
                s.metrics.total_input_tokens,
                s.metrics.total_output_tokens,
                s.metrics.total_thought_tokens,
                s.metrics.total_cached_tokens,
            )
        });
        let card = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("In ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format_tokens(inp),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" · Out ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format_tokens(outp),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("Think ", Style::default().fg(Color::DarkGray)),
                Span::styled(format_tokens(thk), Style::default().fg(Color::Yellow)),
                Span::styled(" · Cache ", Style::default().fg(Color::DarkGray)),
                Span::styled(format_tokens(cached), Style::default().fg(Color::Green)),
            ]),
            Line::from(Span::styled(
                format!(
                    "Total processed: {}",
                    format_tokens(inp.saturating_add(outp))
                ),
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .block(kpi_block(" Tokens "));
        frame.render_widget(card, c1);
    }

    // 3. Session Pool & Reuse Hit Rate Gauge
    if let Some(&c2) = cols.get(2) {
        let (act, idle, max_idle, created, reused) = snap.map_or((0, 0, 64, 0, 0), |s| {
            (
                s.session_pool.active_leased_sessions,
                s.session_pool.idle_cached_sessions,
                s.session_pool.max_idle_sessions,
                s.session_pool.total_sessions_created,
                s.session_pool.total_sessions_reused,
            )
        });
        let total_acq = created.saturating_add(reused);
        let hit_pct = reused
            .saturating_mul(100)
            .checked_div(total_acq)
            .map_or(0u16, |v| u16::try_from(v.min(100)).unwrap_or(100));
        let inner_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Length(1)])
            .margin(1)
            .split(c2);
        frame.render_widget(kpi_block(" Warm Session Pool "), c2);
        if let Some(&top_r) = inner_chunks.first() {
            let p = Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(
                        format!("{act}"),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" leased · ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{idle}/{max_idle}"),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" warm", Style::default().fg(Color::DarkGray)),
                ]),
                Line::from(Span::styled(
                    format!("Reused {reused} · Created {created}"),
                    Style::default().fg(Color::DarkGray),
                )),
            ]);
            frame.render_widget(p, top_r);
        }
        if let Some(&bar_r) = inner_chunks.get(1) {
            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(Color::Green).bg(Color::Rgb(30, 35, 42)))
                .percent(hit_pct)
                .label(format!("Reuse {hit_pct}%"));
            frame.render_widget(gauge, bar_r);
        }
    }

    // 4. ACP JSON-RPC Transport
    if let Some(&c3) = cols.get(3) {
        let (sent, recv, notifs, streams, agy_meta) = snap.map_or_else(
            || (0, 0, 0, 0, "proxy=— · upd=—".to_owned()),
            |s| {
                let px = s.acp_process.agy_proxy.as_deref().unwrap_or("none");
                let au = if s.acp_process.auto_update_enabled {
                    "on"
                } else {
                    "off"
                };
                let ver = s.acp_process.agent_version.as_deref().unwrap_or("—");
                (
                    s.acp_process.rpc_requests_sent,
                    s.acp_process.rpc_responses_received,
                    s.acp_process.rpc_notifications_received,
                    s.acp_process.active_prompt_streams,
                    format!("v{ver} · px={px} · upd={au}"),
                )
            },
        );
        let card = Paragraph::new(vec![
            Line::from(vec![Span::styled(
                format!("RPC {sent}↑ / {recv}↓"),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(vec![
                Span::styled(format!("{notifs} events"), Style::default().fg(Color::Cyan)),
                Span::styled(" · ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{streams} streams"),
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            Line::from(Span::styled(agy_meta, Style::default().fg(Color::DarkGray))),
        ])
        .block(kpi_block(" ACP Subprocess "));
        frame.render_widget(card, c3);
    }
}

fn kpi_block(title: &'static str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
}

fn draw_tabs_bar(frame: &mut Frame<'_>, area: Rect, app: &TopApp) {
    let sess_cnt = app
        .data
        .telemetry
        .as_ref()
        .map_or(0, |s| s.session_pool.sessions.len());
    let model_cnt = app.data.models.len();
    let turn_cnt = app.combined_turns().len();

    let titles = vec![
        Line::from(format!(" [1] Turns & Inspector ({turn_cnt}) ")),
        Line::from(format!(" [2] Session Pool ({sess_cnt}) ")),
        Line::from(format!(" [3] Models ({model_cnt}) ")),
        Line::from(" [4] Live Logs "),
    ];

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(area);

    if let Some(&left) = cols.first() {
        let tabs = Tabs::new(titles)
            .select(app.tab_idx)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .style(Style::default().fg(Color::Gray))
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(tabs, left);
    }

    if let Some(&right) = cols.get(1) {
        let toast_line = if let Some((msg, ok, _)) = &app.toast {
            let (glyph, color) = if *ok {
                ("✓ ", Color::Green)
            } else {
                ("⚠ ", Color::Yellow)
            };
            Line::from(vec![
                Span::styled(
                    glyph,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(msg.as_str(), Style::default().fg(color)),
            ])
        } else {
            Line::from(Span::styled(
                format!("Updated {}", format_utc_hms(app.data.polled_at_ms)),
                Style::default().fg(Color::DarkGray),
            ))
        };
        let status_box = Paragraph::new(toast_line)
            .alignment(Alignment::Right)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::DarkGray)),
            );
        frame.render_widget(status_box, right);
    }
}

fn draw_turns_tab(frame: &mut Frame<'_>, area: Rect, app: &TopApp) {
    let turns = app.combined_turns();
    let panes = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(area);

    let spin = BRAILLE_SPINNER
        .get((app.anim_tick / 2) % BRAILLE_SPINNER.len())
        .copied()
        .unwrap_or("⠋");

    let rows: Vec<Row<'_>> = turns
        .iter()
        .map(|t| {
            let (phase_txt, phase_color) = format_phase(t.phase, spin);
            let api_txt = match t.protocol {
                TaskProtocol::OpenaiResponses => "OAI",
                TaskProtocol::AnthropicMessages => "ANT",
            };
            let warm_txt = if t.session_reused {
                "⚡ warm"
            } else {
                "· cold"
            };
            let tools_cnt =
                t.client_tool_calls.len() + t.native_web_searches + t.native_web_fetches;
            let tok_str = t.usage.as_ref().map_or_else(
                || format!("{}c", t.output_chars),
                |u| {
                    format!(
                        "{} / {}",
                        format_tokens(u.input_tokens),
                        format_tokens(u.output_tokens)
                    )
                },
            );
            Row::new(vec![
                Cell::from(phase_txt).style(
                    Style::default()
                        .fg(phase_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Cell::from(api_txt).style(Style::default().fg(Color::Cyan)),
                Cell::from(t.resolved_model.clone()),
                Cell::from(warm_txt).style(Style::default().fg(if t.session_reused {
                    Color::Green
                } else {
                    Color::DarkGray
                })),
                Cell::from(tools_cnt.to_string()),
                Cell::from(tok_str),
                Cell::from(format_latency_ms(t.elapsed_ms)),
            ])
        })
        .collect();

    let mut state = TableState::default();
    if !turns.is_empty() {
        state.select(Some(app.turn_idx));
    }

    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(5),
            Constraint::Percentage(28),
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Length(16),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(vec![
            "STATE", "API", "MODEL", "SESSION", "TOOLS", "IN / OUT", "ELAPSED",
        ])
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " Active & Recent Turns ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::Rgb(24, 52, 74))
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol(" ❯ ");

    if let Some(&top_pane) = panes.first() {
        frame.render_stateful_widget(table, top_pane, &mut state);
    }

    if let Some(&bottom_pane) = panes.get(1) {
        let detail_lines = if let Some(selected) = turns.get(app.turn_idx) {
            let sess_id = selected.session_id.as_deref().unwrap_or("ephemeral");
            let tools_summary = if selected.client_tool_calls.is_empty() {
                format!(
                    "web_search={} · web_fetch={}",
                    selected.native_web_searches, selected.native_web_fetches
                )
            } else {
                format!(
                    "{} (web_search={} · web_fetch={})",
                    selected.client_tool_calls.join(", "),
                    selected.native_web_searches,
                    selected.native_web_fetches
                )
            };
            let preview = if let Some(err) = &selected.error_message {
                Line::from(vec![
                    Span::styled(
                        "Error  : ",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(single_line_clean(err, 220), Style::default().fg(Color::Red)),
                ])
            } else {
                Line::from(vec![
                    Span::styled("Output : ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        single_line_clean(&selected.output_preview, 220),
                        Style::default().fg(Color::White),
                    ),
                ])
            };
            vec![
                Line::from(vec![
                    Span::styled("Task   : ", Style::default().fg(Color::DarkGray)),
                    Span::styled(&selected.task_id, Style::default().fg(Color::Cyan)),
                    Span::styled("   Session: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(sess_id, Style::default().fg(Color::Green)),
                    Span::styled("   Stop: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        selected.stop_reason.as_deref().unwrap_or("in-progress"),
                        Style::default().fg(Color::Yellow),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("Tools  : ", Style::default().fg(Color::DarkGray)),
                    Span::styled(tools_summary, Style::default().fg(Color::Magenta)),
                ]),
                Line::from(vec![
                    Span::styled("Prompt : ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        single_line_clean(&selected.prompt_preview, 220),
                        Style::default().fg(Color::Gray),
                    ),
                ]),
                preview,
            ]
        } else {
            vec![Line::from(Span::styled(
                "No turns recorded yet. Send a request via `lg cx` or `lg cc` to watch live streaming telemetry.",
                Style::default().fg(Color::DarkGray),
            ))]
        };

        let inspector = Paragraph::new(detail_lines)
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(Span::styled(
                        " Turn Inspector ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )),
            );
        frame.render_widget(inspector, bottom_pane);
    }
}

fn draw_sessions_tab(frame: &mut Frame<'_>, area: Rect, app: &TopApp) {
    let sessions = app
        .data
        .telemetry
        .as_ref()
        .map(|s| s.session_pool.sessions.as_slice())
        .unwrap_or(&[]);
    let now_ms = app.data.polled_at_ms;

    let rows: Vec<Row<'_>> = sessions
        .iter()
        .map(|s| {
            let (st_label, st_color) = match s.state {
                SessionSlotState::Running => ("● LEASED", Color::Cyan),
                SessionSlotState::Idle => ("✓ IDLE", Color::Green),
            };
            Row::new(vec![
                Cell::from(s.session_id.clone()).style(Style::default().fg(Color::Cyan)),
                Cell::from(st_label)
                    .style(Style::default().fg(st_color).add_modifier(Modifier::BOLD)),
                Cell::from(s.model_id.clone()),
                Cell::from(s.turn_count.to_string()),
                Cell::from(s.message_count.to_string()),
                Cell::from(format_relative_ms(s.created_at_unix_ms, now_ms)),
                Cell::from(format_relative_ms(s.last_active_unix_ms, now_ms)),
            ])
        })
        .collect();

    let mut state = TableState::default();
    if !sessions.is_empty() {
        state.select(Some(app.session_idx));
    }

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(26),
            Constraint::Length(11),
            Constraint::Percentage(26),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Length(12),
        ],
    )
    .header(
        Row::new(vec![
            "SESSION ID",
            "STATE",
            "MODEL",
            "TURNS",
            "MSGS",
            "CREATED",
            "LAST ACTIVE",
        ])
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " Pooled `agy acp` Sessions  (press [x] to close selected idle session) ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::Rgb(24, 52, 74))
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol(" ❯ ");

    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_models_tab(frame: &mut Frame<'_>, area: Rect, app: &TopApp) {
    let long_context = infer_live_long_context(&app.data.models, None);
    let rows: Vec<Row<'_>> = app
        .data
        .models
        .iter()
        .map(|m| {
            let ctx_k = format!("{}k", m.context_limits(long_context).0 / 1_000);
            let is_def = m.id == app.data.default_model;
            let is_fast = m.id == app.data.fast_model;
            let is_title = app
                .data
                .title_model
                .as_ref()
                .is_some_and(|id| id.as_str() == m.id);
            let role = [
                (is_title, "title"),
                (is_def, "★ default"),
                (is_fast, "⚡ fast"),
            ]
            .into_iter()
            .filter_map(|(active, label)| active.then_some(label))
            .collect::<Vec<_>>()
            .join(" · ");
            Row::new(vec![
                Cell::from(m.id.clone()).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Cell::from(if m.display_name.is_empty() {
                    m.id.clone()
                } else {
                    m.display_name.clone()
                }),
                Cell::from(ctx_k),
                Cell::from(role).style(
                    Style::default()
                        .fg(if is_def { Color::Green } else { Color::Yellow })
                        .add_modifier(Modifier::BOLD),
                ),
            ])
        })
        .collect();

    let mut state = TableState::default();
    if !app.data.models.is_empty() {
        state.select(Some(app.model_idx));
    }

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(36),
            Constraint::Percentage(34),
            Constraint::Length(10),
            Constraint::Percentage(20),
        ],
    )
    .header(
        Row::new(vec!["MODEL ID", "DISPLAY NAME", "CONTEXT", "ROLE"]).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " Live Antigravity Models  [Enter] set primary  [t] set title model ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::Rgb(24, 52, 74))
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol(" ❯ ");

    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_logs_tab(frame: &mut Frame<'_>, area: Rect, app: &TopApp) {
    let (title, lines): (&str, Vec<Line<'_>>) = match app.log_source {
        LogSourceTab::AcpStderr => {
            let entries = app
                .data
                .telemetry
                .as_ref()
                .map(|s| s.acp_process.recent_stderr.as_slice())
                .unwrap_or(&[]);
            let rendered = if entries.is_empty() {
                vec![Line::from(Span::styled(
                    "No `agy acp` stderr diagnostics recorded. Press [l] to view `gateway.log`.",
                    Style::default().fg(Color::DarkGray),
                ))]
            } else {
                entries
                    .iter()
                    .map(|e| {
                        Line::from(vec![
                            Span::styled(
                                format!("[{}] ", format_utc_hms(e.timestamp_unix_ms)),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::raw(e.line.clone()),
                        ])
                    })
                    .collect()
            };
            (
                " `agy acp` Stderr Stream  (press [l] to switch to gateway.log) ",
                rendered,
            )
        }
        LogSourceTab::ServiceLog => {
            let rendered = if app.data.service_log_lines.is_empty() {
                vec![Line::from(Span::styled(
                    "No lines in `~/.liftgate/gateway.log` yet. Press [l] to switch to `agy acp` stderr.",
                    Style::default().fg(Color::DarkGray),
                ))]
            } else {
                app.data
                    .service_log_lines
                    .iter()
                    .map(|l| Line::from(l.as_str()))
                    .collect()
            };
            (
                " Gateway Service Log (`gateway.log`)  (press [l] to switch to `agy` stderr) ",
                rendered,
            )
        }
    };

    let para = Paragraph::new(lines)
        .scroll((app.log_scroll, 0))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    title,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )),
        );
    frame.render_widget(para, area);
}

fn draw_footer_bar(frame: &mut Frame<'_>, area: Rect, app: &TopApp) {
    let mut chips = vec![
        Span::styled(
            " 1-4/Tab ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("Views  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "↑↓/jk ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("Select  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "s/S ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("Start/Stop  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "o ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("Web UI  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "r ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("Refresh  ", Style::default().fg(Color::DarkGray)),
    ];

    match app.tab_idx {
        1 => {
            chips.push(Span::styled(
                "x ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            chips.push(Span::styled(
                "Evict Idle  ",
                Style::default().fg(Color::DarkGray),
            ));
        }
        2 => {
            chips.push(Span::styled(
                "Enter / t ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            chips.push(Span::styled(
                "Set Primary / Title  ",
                Style::default().fg(Color::DarkGray),
            ));
        }
        3 => {
            chips.push(Span::styled(
                "l ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            chips.push(Span::styled(
                "Toggle Log  ",
                Style::default().fg(Color::DarkGray),
            ));
        }
        _ => {}
    }

    chips.push(Span::styled(
        "? ",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    chips.push(Span::styled("Help  ", Style::default().fg(Color::DarkGray)));
    chips.push(Span::styled(
        "q ",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    chips.push(Span::styled("Quit", Style::default().fg(Color::DarkGray)));

    let bar = Paragraph::new(Line::from(chips));
    frame.render_widget(bar, area);
}

fn draw_help_modal(frame: &mut Frame<'_>) {
    let area = centered_rect(68, 16, frame.area());
    frame.render_widget(Clear, area);
    let lines = vec![
        Line::from(vec![
            Span::styled(
                "  Tab / Shift+Tab / 1-4  ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Switch dashboard views"),
        ]),
        Line::from(vec![
            Span::styled(
                "  ↑ / ↓ or j / k         ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Navigate rows or scroll logs"),
        ]),
        Line::from(vec![
            Span::styled(
                "  s                      ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Start (if offline) or restart background gateway"),
        ]),
        Line::from(vec![
            Span::styled(
                "  S (Shift+s)            ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Gracefully stop background gateway"),
        ]),
        Line::from(vec![
            Span::styled(
                "  x / Delete             ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Close selected idle session (Session Pool tab)"),
        ]),
        Line::from(vec![
            Span::styled(
                "  Enter                  ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Switch primary model (Models tab); t sets the title model"),
        ]),
        Line::from(vec![
            Span::styled(
                "  l                      ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Toggle `agy acp` stderr vs `gateway.log` (Logs tab)"),
        ]),
        Line::from(vec![
            Span::styled(
                "  o                      ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Open Web Dashboard in browser"),
        ]),
        Line::from(vec![
            Span::styled(
                "  r                      ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Immediate telemetry refresh"),
        ]),
        Line::from(vec![
            Span::styled(
                "  q / Esc / ?            ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Close help / Quit dashboard"),
        ]),
    ];

    let modal = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                " Keyboard Shortcuts ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
    );
    frame.render_widget(modal, area);
}

fn format_phase(phase: TaskPhase, spin: &str) -> (String, Color) {
    match phase {
        TaskPhase::InitializingSession => (format!("{spin} INIT"), Color::Yellow),
        TaskPhase::Thinking => (format!("{spin} THINKING"), Color::Yellow),
        TaskPhase::SearchingWeb => (format!("{spin} WEB SEARCH"), Color::Cyan),
        TaskPhase::FetchingWeb => (format!("{spin} WEB FETCH"), Color::Cyan),
        TaskPhase::StreamingText => (format!("{spin} STREAMING"), Color::Green),
        TaskPhase::CallingClientTool => (format!("{spin} TOOL CALL"), Color::Magenta),
        TaskPhase::Completed => ("✓ COMPLETED".to_owned(), Color::Green),
        TaskPhase::Failed => ("✗ FAILED".to_owned(), Color::Red),
        TaskPhase::Cancelled => ("· CANCELLED".to_owned(), Color::DarkGray),
    }
}

fn single_line_clean(input: &str, max_chars: usize) -> String {
    let collapsed: String = input
        .chars()
        .map(|c| {
            if matches!(c, '\n' | '\r' | '\t') {
                ' '
            } else {
                c
            }
        })
        .take(max_chars)
        .collect();
    let trimmed = collapsed.trim();
    if trimmed.is_empty() {
        "—".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn centered_rect(max_width: u16, max_height: u16, area: Rect) -> Rect {
    let width = max_width.min(area.width);
    let height = max_height.min(area.height);
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height) / 2);
    Rect::new(x, y, width, height)
}
